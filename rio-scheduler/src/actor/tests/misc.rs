//! Miscellaneous actor feature tests that don't fit the other modules:
//! GcRoots collection, orphan-build cancellation, backpressure hysteresis,
//! leader/recovery dispatch gating.

use super::*;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tracing_test::traced_test;

/// sh-018b structural red-first: `maybe_refresh_estimator` (phase-00)
/// MUST NOT call `SlaEstimator::refresh()` on the actor turn — the
/// refresh body lives in `estimator_poller`. Count-based, not
/// wall-clock (`ci-failure-patterns.md` "Wall-clock gate under load →
/// prefer (c)"). RED at base a008959a2: the on-actor `refresh()` call
/// at `housekeeping.rs:154-163` bumps `refresh_calls` on the 6th tick.
///
/// Secondary: `full_sweep` STILL runs on cadence — a sentinel priority
/// is overwritten by the sweep's recompute (the leaf falls back to
/// `DEFAULT_DURATION_SECS` with no fit; any value ≠ 999.0 proves the
/// sweep fired).
#[tokio::test]
async fn phase00_never_calls_refresh_on_actor() {
    use std::sync::atomic::Ordering;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor(db.pool.clone());

    actor.test_inject_ready("h", None, "x86_64-linux", false);
    actor.dag.node_mut("h").unwrap().sched.priority = 999.0;

    let before = actor.sla_estimator.refresh_calls.load(Ordering::Relaxed);
    for _ in 0..6 {
        actor.maybe_refresh_estimator().await;
    }
    assert_eq!(
        actor.sla_estimator.refresh_calls.load(Ordering::Relaxed),
        before,
        "refresh() ran on the actor turn — phase-00 still blocks the mailbox",
    );
    assert_ne!(
        actor.dag.node("h").unwrap().sched.priority,
        999.0,
        "full_sweep no longer runs on the 60s cadence",
    );
}

/// Regression for bug_032: `DerivationState.interested_builds` is a
/// `HashSet<Uuid>` (RandomState); the flusher uses `.first()` on the vec
/// returned here to pick the S3-key build_id. Before the fix this was
/// HashSet-iteration-ordered → re-flush across a restart could pick a
/// different bid → new S3 key → ON CONFLICT repoints PG rows and orphans
/// the previous blob. `get_interested_builds()` now sorts, so `.first()`
/// is always min(UUID). Reverting the sort fails this test (P(false-pass)
/// = 1/8! ≈ 2.5e-5 — HashSet would have to iterate in sorted order by
/// chance).
#[tokio::test]
async fn get_interested_builds_is_sorted() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor(db.pool.clone());
    actor.test_inject_ready("h", None, "x86_64-linux", false);

    let bids: Vec<Uuid> = (0..8).map(|_| Uuid::new_v4()).collect();
    actor
        .dag
        .node_mut("h")
        .unwrap()
        .interested_builds
        .extend(bids.iter().copied());

    let result = actor.get_interested_builds(&DrvHash::from("h"));
    let mut sorted = bids.clone();
    sorted.sort_unstable();
    assert_eq!(
        result, sorted,
        "get_interested_builds() must sort (S3-key determinism); \
         flusher's .first() relies on result[0] == min(UUID)"
    );
    assert_eq!(result.first(), sorted.first());
}

// ---------------------------------------------------------------------------
// Leader/recovery dispatch gate
// ---------------------------------------------------------------------------

/// Helper: build an actor with custom leader/recovery flags (no mock
/// store). Returns the `LeaderState` handle so tests can drive
/// `on_lose()` / `on_acquire()` from outside the lease loop.
fn spawn_actor_with_leader(
    pool: sqlx::PgPool,
    is_leader: bool,
    recovery_complete: bool,
) -> (
    ActorHandle,
    tokio::task::JoinHandle<()>,
    crate::lease::LeaderState,
) {
    let leader = crate::lease::LeaderState::from_parts(
        Arc::new(std::sync::atomic::AtomicU64::new(1)),
        Arc::new(AtomicBool::new(is_leader)),
        recovery_complete,
    );
    let leader_clone = leader.clone();
    let (handle, task) = setup_actor_configured(pool, None, move |_, p| {
        p.leader = leader_clone;
    });
    (handle, task, leader)
}

/// Backward-compat wrapper for tests that don't need the LeaderState handle.
fn spawn_actor_with_flags(
    pool: sqlx::PgPool,
    is_leader: bool,
    recovery_complete: bool,
) -> (ActorHandle, tokio::task::JoinHandle<()>) {
    let (h, t, _l) = spawn_actor_with_leader(pool, is_leader, recovery_complete);
    (h, t)
}

/// merged_bug_005 red (ack-on-enqueue axis): the `AckSpawnedIntents`
/// reply is sent AFTER the leader-gated apply — a deposed drain
/// answers `NotLeader`, so the gRPC layer errs the Ack and the
/// controller's commit-on-Ack buffer survives to redeliver at the
/// next leader. `left:` pre-fix the handler answered OK on
/// `send_unchecked` enqueue while the standby dropped the payload
/// whole (r[sched.lease.standby-drops-writes+4] defense-in-depth) —
/// the controller then cleared consume-once evidence that was never
/// applied. `right:` leader applies → `Ok`; deposed → typed refusal.
// r[verify ctrl.nodeclaim.evidence-ack-latch+3]
#[tokio::test]
async fn deposed_ack_spawned_intents_answers_not_leader() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (handle, _task, leader) = spawn_actor_with_leader(db.pool.clone(), true, true);

    let empty_ack = |reply| ActorCommand::AckSpawnedIntents {
        spawned: vec![],
        unfulfillable_cells: vec![],
        registered_cells: vec![],
        observed_instance_types: vec![],
        bound_intents: vec![],
        binding_snapshot: None,
        rejected: vec![],
        reply,
    };

    // Leader: the apply runs and the reply proves it.
    let applied = handle.query_unchecked(empty_ack).await?;
    assert_eq!(applied, Ok(()), "leader drain applies and acks");

    // Depose (mirror the lease loop: atomics flip, then the command).
    leader.on_lose();
    handle.send_unchecked(ActorCommand::LeaderLost).await?;
    barrier(&handle).await;

    let applied = handle.query_unchecked(empty_ack).await?;
    assert_eq!(
        applied,
        Err(crate::actor::AckApplyError::NotLeader),
        "deposed drain must refuse — an OK here would make the \
         controller wipe evidence the standby dropped"
    );
    Ok(())
}

// r[verify obs.metric.scheduler-leader-gate+5]
// r[verify obs.metric.scheduler-substituting+2]
/// When is_leader=false, handle_tick must NOT set state gauges.
/// Standby actor is warm (DAGs merge for takeover) but its counts are
/// stale/zero. Publishing them creates a second Prometheus series that
/// stat-panel reducers pick nondeterministically.
///
/// Mechanism: `set_default_local_recorder` installs a thread-local
/// recorder; `#[tokio::test]`'s current-thread runtime means the actor
/// task sees it at `.await` points. The recorder's `register_gauge`
/// tracks names touched — absence of all four gauge names after Tick
/// proves the gate held (workers_active, though deprecated/pinned to
/// zero, is published from the same gated block, so it is asserted
/// with the rest).
#[tokio::test]
async fn test_not_leader_does_not_set_gauges() -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (handle, _task) = spawn_actor_with_flags(db.pool.clone(), false, true);

    // Merge a DAG so there's something to count. Standby DOES merge
    // (r[sched.lease.k8s-lease+2]: "DAGs are still merged so state is
    // warm for takeover"). If the gate is broken, derivations_queued
    // would be set to 1 (this node is Ready — no deps).
    merge_single_node(&handle, Uuid::new_v4(), "sg-drv", PriorityClass::Scheduled).await?;

    // Tick on a fresh actor: tick_count 0→1, maybe_refresh_estimator
    // early-returns (1%6≠0). No workers, nothing running →
    // heartbeat/backstop/poison scans no-op. Gauge block is the only
    // gauge path reachable.
    handle.send_unchecked(ActorCommand::Tick).await?;
    barrier(&handle).await;

    // The handle_tick gauges must NOT appear (substituting_derivations
    // is published from the same gated tick — the snapshot site — so a
    // standby publishing it would feed KEDA a duplicate stale series).
    for name in [
        "rio_scheduler_derivations_queued",
        "rio_scheduler_workers_active",
        "rio_scheduler_builds_active",
        "rio_scheduler_derivations_running",
        "rio_scheduler_substituting_derivations",
    ] {
        assert!(
            !recorder.gauge_touched(name),
            "standby set gauge {name} — leader-gate broken.\n\
             Gauges touched: {:?}",
            recorder.gauge_names()
        );
    }
    Ok(())
}

// r[verify sched.lease.standby-tick-noop+2]
// r[verify obs.metric.scheduler-leader-gate+5]
// r[verify obs.metric.scheduler-substituting+2]
/// Was-leader → standby: `LeaderLost` clears in-memory state and zeros
/// gauges; subsequent `Tick` early-returns so the orphan-watcher does
/// NOT write `Cancelled` to PG for builds the new leader is running.
///
/// Pre-fix (b62291b8): no `LeaderLost` command, no `handle_tick`
/// leader gate. After `on_lose()`, dropping `event_rx` and ticking ×3
/// (cfg(test) `ORPHAN_BUILD_GRACE=ZERO` → cancels on tick 2) wrote
/// `status='cancelled'` to PG.
#[tokio::test]
async fn test_ex_leader_housekeeping_is_noop_after_lose() -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (handle, _task, leader) = spawn_actor_with_leader(db.pool.clone(), true, true);

    // Merge a build while we ARE leader. Hold event_rx so the
    // orphan-watcher precondition (receiver_count==0) is set up by an
    // explicit drop below, not by the temporary going out of scope.
    let build_id = Uuid::new_v4();
    let event_rx =
        merge_single_node(&handle, build_id, "ex-leader-drv", PriorityClass::Scheduled).await?;
    // One Tick as leader: gauges set non-zero (builds_active=1).
    handle.send_unchecked(ActorCommand::Tick).await?;
    barrier(&handle).await;
    assert_eq!(
        recorder.gauge_value("rio_scheduler_builds_active{}"),
        Some(1.0),
        "leader's first Tick should set builds_active=1"
    );

    // Precondition: PG has the build as Active.
    let row: (String,) = sqlx::query_as("SELECT status::text FROM builds WHERE build_id = $1")
        .bind(build_id)
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(row.0, "active");

    // ── Lose transition ──────────────────────────────────────────────
    // Mirror the lease loop: on_lose() flips atomics; LeaderLost tells
    // the actor. Order matters: handle_tick checks is_leader, so flip
    // BEFORE any Tick can run.
    leader.on_lose();
    handle.send_unchecked(ActorCommand::LeaderLost).await?;
    barrier(&handle).await;

    // LeaderLost cleared persisted state: drv gone from in-memory DAG.
    assert!(
        handle
            .debug_query_derivation("ex-leader-drv")
            .await?
            .is_none(),
        "LeaderLost should clear_persisted_state (DAG empty)"
    );
    // LeaderLost swept the whole leader-gauge FAMILY to its declared
    // resets (one-shot, not per-Tick) — family-driven, so a gauge
    // added to the declaration is covered here automatically (the
    // hand list this replaced omitted materialization_stalled and
    // never knew about sla_prior_divergence's 1.0 neutral).
    for g in crate::observability::LeaderGauge::ALL {
        match g.label_axis() {
            None => assert_eq!(
                recorder.gauge_value(&format!("{}{{}}", g.name())),
                Some(g.reset_value()),
                "LeaderLost should sweep {} to its declared reset so the \
                 ex-leader's series collapses",
                g.name()
            ),
            Some((axis, values)) => {
                for v in values {
                    assert_eq!(
                        recorder.gauge_value(&format!("{}{{{axis}={v}}}", g.name())),
                        Some(g.reset_value()),
                        "LeaderLost should sweep {}{{{axis}={v}}} to its declared reset",
                        g.name()
                    );
                }
            }
        }
    }

    // Drop the watcher (orphan condition) and Tick ×3. cfg(test)
    // ORPHAN_BUILD_GRACE=ZERO means a leader's Tick 2 would have
    // cancelled the build. Ex-leader's Tick must early-return.
    drop(event_rx);
    for _ in 0..3 {
        handle.send_unchecked(ActorCommand::Tick).await?;
    }
    barrier(&handle).await;

    // PG row UNTOUCHED: still 'active', NOT 'cancelled'. This is the
    // core split-brain assertion — db.update_build_status has no fence
    // in its WHERE clause, so the only thing stopping the ex-leader's
    // write is the handle_tick gate + cleared state.
    let row: (String,) = sqlx::query_as("SELECT status::text FROM builds WHERE build_id = $1")
        .bind(build_id)
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(
        row.0, "active",
        "ex-leader Tick must NOT write Cancelled to PG (orphan-watcher \
         on stale state would; gate + LeaderLost prevent it)"
    );

    Ok(())
}

// r[verify obs.metric.scheduler-leader-gate+5]
/// `LeaderLost` must zero `rio_scheduler_open_attempts` like the rest
/// of the leader-state gauge family. Unlike the DAG gauges it is set
/// from a durable view (the establishment sweep), so the new leader
/// republishes the same number — but the DEPOSED leader's frozen
/// series would otherwise sit in Prometheus until that pod restarts,
/// and `sum(rio_scheduler_open_attempts)` consumers (the store
/// ScaledObject's builders-per-replica trigger) would double-count
/// the fleet after every failover.
#[tokio::test]
async fn leader_lost_zeroes_open_attempts_gauge() -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (handle, _task, leader) = spawn_actor_with_leader(db.pool.clone(), true, true);

    // Merge + pull-mint one attempt: the durable open-attempt view is
    // non-empty, so the leader's sweep publishes a NON-zero gauge —
    // without this the zero-on-lose assertion below would be vacuous
    // (the gauge already reads 0.0 from the first Tick).
    merge_single_node(
        &handle,
        Uuid::new_v4(),
        "oa-gauge-drv",
        PriorityClass::Scheduled,
    )
    .await?;
    let _assignment = pull_attempt(&handle, "oa-gauge-drv").await;
    handle.send_unchecked(ActorCommand::Tick).await?;
    barrier(&handle).await;
    assert_eq!(
        recorder.gauge_value("rio_scheduler_open_attempts{}"),
        Some(1.0),
        "leader's sweep must publish the minted open attempt"
    );

    leader.on_lose();
    handle.send_unchecked(ActorCommand::LeaderLost).await?;
    barrier(&handle).await;
    assert_eq!(
        recorder.gauge_value("rio_scheduler_open_attempts{}"),
        Some(crate::observability::LeaderGauge::OpenAttempts.reset_value()),
        "LeaderLost must sweep open_attempts to its declared family reset — \
         a deposed leader's frozen series double-counts the fleet for sum() \
         consumers (the KEDA builders trigger)"
    );
    Ok(())
}

// r[verify obs.metric.scheduler-leader-gate+5]
/// Family-driven loss sweep: EVERY declared member — including the
/// labeled divergence gauge and members no tick has published — reads
/// its declared reset after LeaderLost. Pre-family red (recorded in
/// the introducing commit): the hand list missed
/// `materialization_stalled` (a deposed leader's frozen parked-count
/// kept feeding the MD-D1 stalled alert) and `sla_prior_divergence`
/// (whose 0.0 would FIRE the clamp alert; its declared reset is the
/// 1.0 neutral).
#[tokio::test]
async fn leader_lost_resets_every_leader_gauge() -> TestResult {
    use crate::observability::LeaderGauge;

    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (handle, _task, leader) = spawn_actor_with_leader(db.pool.clone(), true, true);

    // Sentinel-set every member through its typed accessor (7.0 is
    // outside every declared reset) so the sweep is provably a write,
    // not a leftover.
    for g in LeaderGauge::ALL {
        match g.label_axis() {
            None => g.set(7.0),
            Some((_, values)) => {
                for v in values {
                    g.set_with(v, 7.0);
                }
            }
        }
    }

    leader.on_lose();
    handle.send_unchecked(ActorCommand::LeaderLost).await?;
    barrier(&handle).await;

    for g in LeaderGauge::ALL {
        match g.label_axis() {
            None => assert_eq!(
                recorder.gauge_value(&format!("{}{{}}", g.name())),
                Some(g.reset_value()),
                "{} must read its declared reset after LeaderLost",
                g.name()
            ),
            Some((axis, values)) => {
                for v in values {
                    assert_eq!(
                        recorder.gauge_value(&format!("{}{{{axis}={v}}}", g.name())),
                        Some(g.reset_value()),
                        "{}{{{axis}={v}}} must read its declared reset after LeaderLost",
                        g.name()
                    );
                }
            }
        }
    }
    Ok(())
}

// r[verify sec.boundary.grpc-hmac]
/// When `with_hmac_signer` is set, dispatched assignments carry a
/// signed token that the store can verify. Token must contain the
/// derivation's expected_output_paths so the store can enforce
/// "worker can only upload assigned outputs".
#[tokio::test]
async fn test_hmac_signer_produces_verifiable_token() -> TestResult {
    use rio_auth::hmac::{HmacSigner, HmacVerifier};

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let test_key = b"test-scheduler-hmac-key-32bytes!".to_vec();

    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |_, p| {
        p.hmac_signer = Some(Arc::new(HmacSigner::from_key(test_key.clone())));
    });

    // Merge a node WITH expected_output_paths set — the token's
    // claims must include them.
    let expected_out = test_store_path("hmac-expected-out");
    let mut node = make_node("hmac-drv");
    node.expected_output_paths = vec![expected_out.clone()];
    merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;

    let assignment = pull_attempt(&handle, "hmac-drv").await;

    // Token is NOT the legacy "{executor}-{hash}-{gen}" format (the
    // pull identity is the intent id).
    assert!(
        !assignment
            .assignment_token
            .starts_with("hmac-drv-hmac-drv-"),
        "should be HMAC-signed, not legacy format: {}",
        assignment.assignment_token
    );

    // Verify with the same key.
    let verifier = HmacVerifier::from_key(test_key);
    let claims = verifier
        .verify::<rio_auth::hmac::AssignmentClaims>(&assignment.assignment_token)
        .expect("token should verify with same key");

    assert_eq!(claims.executor_id, "hmac-drv");
    assert_eq!(claims.drv_hash, "hmac-drv");
    assert!(
        claims.expected_outputs.contains(&expected_out),
        "claims should include expected_output_paths: {:?}",
        claims.expected_outputs
    );
    // Expiry is in the future (timeout_secs × 2 from now).
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(claims.expiry_unix > now, "expiry should be in the future");

    Ok(())
}

/// bug_011 Phase 2 invariant: `dispatch.rs` stamps the attributed
/// tenant UUID into the SIGNED `AssignmentClaims` so the store derives
/// `hw_perf_samples.submitting_tenant` from a verified token, never
/// from the worker's request body
/// (`r[sched.sla.threat.hw-median-of-medians]`).
///
/// This is the writer-side pin: it exercises `build_assignment_proto`
/// end-to-end (merge → dispatch → token → verify) and asserts the
/// claims body carries the seeded tenant. The serde-shape pin
/// (`assignment_claims_tenant_forward_skew` in `rio-auth`) is the
/// complement — it proves `Some(_)` emits the `"tenant"` key and `None`
/// omits it, but constructs its own claims and never touches
/// `dispatch.rs`. Without THIS test, a regression to `tenant: None` (or
/// to `state.tenant_id` / the request-body tenant) would pass the whole
/// suite while silently NULLing every `submitting_tenant` row.
#[tokio::test]
async fn test_hmac_assignment_carries_tenant() -> TestResult {
    use rio_auth::hmac::{HmacSigner, HmacVerifier};

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let test_key = b"test-phase2-tenant-key-32-bytes!".to_vec();

    // Tenant must exist (builds.tenant_id FK, migration 009). The
    // build below is attributed to it, so `attributed_tenant(...)`
    // returns `Some(tenant)` and the signed claims must carry it. A
    // tenant-less build can't pin the write: `attributed_tenant` is
    // `None` either way, so a regression to `tenant: None` would still
    // pass.
    let tenant = rio_store::test_helpers::seed_tenant(&db.pool, "phase2-tenant").await;

    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |_, p| {
        p.hmac_signer = Some(Arc::new(HmacSigner::from_key(test_key.clone())));
    });

    let _ = merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id: Uuid::new_v4(),
            tenant_id: Some(tenant),
            nodes: vec![make_node("phase2-drv")],
            edges: vec![],
            ..Default::default()
        },
    )
    .await?;

    let assignment = pull_attempt(&handle, "phase2-drv").await;
    let claims = HmacVerifier::from_key(test_key)
        .verify::<rio_auth::hmac::AssignmentClaims>(&assignment.assignment_token)
        .expect("token verifies");

    assert_eq!(
        claims.tenant,
        Some(tenant.to_string()),
        "Phase 2 (bug_011): dispatch.rs must sign the attributed tenant \
         (hyphenated UUID) into AssignmentClaims so the store derives \
         hw_perf_samples.submitting_tenant from a verified token, not \
         the request body."
    );

    Ok(())
}

// r[verify common.hmac.claims+3]
/// E3, the token-LIFETIME law: the 7-day bound is on the EXPIRY (the
/// security-relevant quantity — a leaked token's replay window), not
/// on the timeout input. Pre-fix the clamp bounded the timeout and
/// THEN doubled it (expiry = now + min(t, 7d) × 2 → 14d effective),
/// so the "7 days max" comment was false by a factor of two. The law:
/// for ANY requested build_timeout — saturating u64::MAX or the
/// boundary 7d request alike — expiry − now ≤ 7 days.
#[tokio::test]
async fn test_hmac_expiry_bounded_by_seven_day_lifetime() -> TestResult {
    use rio_auth::hmac::{HmacSigner, HmacVerifier};

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let test_key = b"test-clamp-key-at-least-32-bytes!!".to_vec();

    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |_, p| {
        p.hmac_signer = Some(Arc::new(HmacSigner::from_key(test_key.clone())));
    });

    // Two populations: the saturation face (u64::MAX) and the
    // boundary face (a lawful request of exactly 7d — the case the
    // pre-fix law doubled to 14d).
    for (drv, timeout) in [("clamp-drv", u64::MAX), ("boundary-drv", 7 * 86400)] {
        let _ = merge_dag_req(
            &handle,
            MergeDagRequest {
                build_id: Uuid::new_v4(),
                nodes: vec![make_node(drv)],
                edges: vec![],
                options: BuildOptions {
                    build_timeout: timeout.into(),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await?;

        let assignment = pull_attempt(&handle, drv).await;

        let claims = HmacVerifier::from_key(test_key.clone())
            .verify::<rio_auth::hmac::AssignmentClaims>(&assignment.assignment_token)
            .expect("token verifies");

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // The law's own quantity: lifetime = expiry − now ≤ 7d.
        // 60s slack covers test elapsed time between mint and check.
        let max_expected = now + 7 * 86400 + 60;
        assert!(
            claims.expiry_unix <= max_expected,
            "requested timeout {timeout}: token lifetime must be bounded by the \
             7-day law on the EXPIRY axis — got expiry {} (> {max_expected}; \
             a 14d window means the bound was applied to the timeout, not \
             the lifetime)",
            claims.expiry_unix,
        );

        Ok::<(), anyhow::Error>(())?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// GcRoots: collect live-build output paths
// ---------------------------------------------------------------------------

/// GcRoots collects expected_output_paths from non-terminal
/// derivations. Terminal drvs (Completed/Poisoned/Cancelled) are
/// excluded — their outputs are in the store proper, not live roots.
#[tokio::test]
async fn test_gc_roots_collects_expected_outputs() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Merge a node with expected outputs. Node starts in Ready —
    // non-terminal, so it should appear in roots.
    let out1 = test_store_path("gcroot-out1");
    let out2 = test_store_path("gcroot-out2");
    let mut node = make_node("gcroot-drv");
    node.expected_output_paths = vec![out1.clone(), out2.clone()];
    merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;

    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::Admin(AdminQuery::GcRoots { reply: reply_tx }))
        .await?;
    let roots = reply_rx.await?;

    assert!(roots.contains(&out1), "roots should include {out1}");
    assert!(roots.contains(&out2), "roots should include {out2}");

    Ok(())
}

/// GcRoots dedups: two nodes with the same expected output path →
/// single entry. Saves CTE work on the store side.
#[tokio::test]
async fn test_gc_roots_dedupes() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let shared_out = test_store_path("gcroot-shared");
    let mut n1 = make_node("gc-dup1");
    n1.expected_output_paths = vec![shared_out.clone()];
    let mut n2 = make_node("gc-dup2");
    n2.expected_output_paths = vec![shared_out.clone()];

    merge_dag(&handle, Uuid::new_v4(), vec![n1, n2], vec![], false).await?;

    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::Admin(AdminQuery::GcRoots { reply: reply_tx }))
        .await?;
    let roots = reply_rx.await?;

    let count = roots.iter().filter(|p| *p == &shared_out).count();
    assert_eq!(count, 1, "shared output deduped, not 2");

    Ok(())
}

// r[verify sched.gc.live-pins]
/// Floating-CA derivations carry `expected_output_paths == [""]`
/// pre-completion (translate.rs convention). GcRoots must filter these
/// — a `""` in the roots list makes the store's `validate_store_path`
/// reject the whole batch with `InvalidArgument`, breaking GC whenever
/// any CA build is in flight.
#[tokio::test]
async fn test_gc_roots_filters_empty_ca_paths() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let real_out = test_store_path("gc-ca-real");
    let mut ia = make_node("gc-ca-ia");
    ia.expected_output_paths = vec![real_out.clone()];
    // Floating-CA: path-less placeholder until completion.
    let mut ca = make_node("gc-ca-float");
    ca.expected_output_paths = vec![String::new()];
    merge_dag(&handle, Uuid::new_v4(), vec![ia, ca], vec![], false).await?;

    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::Admin(AdminQuery::GcRoots { reply: reply_tx }))
        .await?;
    let roots = reply_rx.await?;

    assert!(
        !roots.iter().any(String::is_empty),
        "GcRoots must filter empty CA placeholder paths; got {roots:?}"
    );
    assert!(roots.contains(&real_out), "real IA output still rooted");
    Ok(())
}

// ---------------------------------------------------------------------------
// MergeDag reply dropped → orphan build cancelled (Round 4 Z1)
// ---------------------------------------------------------------------------

/// If the MergeDag reply receiver is dropped before the actor
/// replies (client timed out / disconnected), the actor should
/// cancel the orphaned build — nobody is watching it.
#[tokio::test]
#[traced_test]
async fn test_merge_dag_reply_dropped_cancels_orphan() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    let (reply_tx, reply_rx) = oneshot::channel();
    // Drop the receiver BEFORE sending — actor's reply.send() will fail.
    drop(reply_rx);

    handle
        .send_unchecked(ActorCommand::MergeDag {
            req: MergeDagRequest {
                build_id,
                nodes: vec![make_node("orphan-drv")],
                edges: vec![],
                ..Default::default()
            },
            reply: reply_tx,
        })
        .await?;
    // P2: intake only queues; the reply.send (and orphan-cancel) fires
    // from `flush_pending_merges`. The first barrier's post-dispatch
    // trigger-(v) check (inline `armed_at.elapsed() ≥ DEADLINE`)
    // enters the flush; the second barrier serializes behind the
    // flush's PG awaits so logs_contain observes the orphan-cancel
    // line. (Tick-head trigger ii is deleted; the deadline arm (iv) is
    // racy against the test's sleep on current_thread.)
    tokio::time::sleep(crate::actor::merge::MERGE_PERSIST_FLUSH_DEADLINE).await;
    barrier(&handle).await;
    barrier(&handle).await;

    // Actor should log the orphan cancellation.
    assert!(
        logs_contain("cancelling orphaned build") || logs_contain("orphaned"),
        "expected orphan-cancel log"
    );

    // Build state is Cancelled (or not found — either is acceptable
    // since nobody's watching).
    let result = try_query_status(&handle, build_id).await?;
    match result {
        Ok(status) => {
            assert_eq!(
                status.state,
                rio_proto::types::BuildState::Cancelled as i32,
                "orphan build should be Cancelled"
            );
        }
        Err(ActorError::BuildNotFound(_)) => {
            // Also acceptable — actor may have cleaned it up already.
        }
        Err(e) => panic!("unexpected error: {e:?}"),
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Backpressure hysteresis (direct unit test)
// ---------------------------------------------------------------------------

// r[verify sched.backpressure.hysteresis+3]
/// Hysteresis: active fires at ≥80% (HIGH), clears at ≤60% (LOW).
/// Between 60-80% the current state is sticky — prevents flapping.
///
/// Tested on a bare non-spawned actor; update_backpressure only
/// touches self.backpressure_active (no DB access).
#[tokio::test]
async fn test_backpressure_hysteresis() -> TestResult {
    // Need a real TestDb because PgPool::connect_lazy requires a
    // tokio runtime. The method doesn't query — SchedulerDb::new
    // just stores the pool.
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor(db.pool.clone());
    let reader = actor.backpressure_flag();

    // Start: inactive.
    assert!(!reader.is_active(), "initial: inactive");

    // 79% → below HIGH (0.80) → stays inactive.
    actor.update_backpressure(7900, 10_000);
    assert!(!reader.is_active(), "79% < HIGH → stays inactive");

    // 80% → hits HIGH → activates.
    actor.update_backpressure(8000, 10_000);
    assert!(reader.is_active(), "80% ≥ HIGH → activates");

    // 70% → between LOW and HIGH → STAYS active (sticky).
    actor.update_backpressure(7000, 10_000);
    assert!(reader.is_active(), "70% between LOW/HIGH → sticky active");

    // 61% → still above LOW → STAYS active.
    actor.update_backpressure(6100, 10_000);
    assert!(reader.is_active(), "61% > LOW → still active");

    // 60% → hits LOW → deactivates.
    actor.update_backpressure(6000, 10_000);
    assert!(!reader.is_active(), "60% ≤ LOW → deactivates");

    // 70% again → below HIGH → STAYS inactive (sticky).
    actor.update_backpressure(7000, 10_000);
    assert!(!reader.is_active(), "70% < HIGH → sticky inactive");

    Ok(())
}

/// W9-AH (round-9 B6): backpressure engages on projected WORK-COST,
/// not queue depth alone. The live_053 inversion: one command worth
/// 140s of actor time, mailbox depth 1.0–12.8% — the depth watermarks
/// stayed silent through total time-starvation while queued callers'
/// deadlines all lapsed. Post-fix the same shape engages: projected
/// drain (depth × per-turn cost EWMA) over the high drain budget
/// trips the SAME hysteresis flag the gateway already sheds on, and
/// release requires BOTH axes low (depth AND projected drain).
///
/// Structural drive (no wall clock): the cost observations are fed
/// directly to `note_turn_cost` — exactly what `run_inner` records
/// after each `prices_into_drain()` command — and the law is asserted
/// on the flag.
// r[verify sched.admission.work-per-turn]
// r[verify sched.backpressure.hysteresis+3]
#[tokio::test]
async fn backpressure_engages_on_work_cost_while_depth_low() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor(db.pool.clone());
    let reader = actor.backpressure_flag();

    // The incident shape: ONE turn worth 140s, then the loop observes
    // a 1% depth (100 of 10,000 queued during the stall).
    actor.note_turn_cost(std::time::Duration::from_secs(140));
    actor.update_backpressure(100, 10_000);
    assert!(
        reader.is_active(),
        "depth 1% but projected drain = 100 × EWMA(140s-turn) ≫ the drain \
         budget: cost-blind watermarks stayed silent through total \
         time-starvation (W9-AH)"
    );

    // Recovery: subsequent idle Ticks (the only feeds — µs/ms-class
    // commands no longer fold) decay the EWMA; release only when BOTH
    // the depth axis (already low) AND the projected drain axis clear
    // LOW.
    for _ in 0..200 {
        actor.note_turn_cost(std::time::Duration::from_millis(1));
    }
    actor.update_backpressure(100, 10_000);
    assert!(
        !reader.is_active(),
        "after 200 idle Ticks the projected drain is far under the LOW \
         release bound at depth 1% — the cost axis must release (sticky \
         forever = a one-off stall sheds work indefinitely)"
    );

    // The depth law is UNCHANGED (joint hysteresis, not replacement):
    // 80% engages even with a cold cost EWMA…
    actor.update_backpressure(8000, 10_000);
    assert!(
        reader.is_active(),
        "80% depth must still engage with a near-zero cost EWMA"
    );
    // …and release needs depth back under LOW too.
    actor.update_backpressure(6000, 10_000);
    assert!(
        !reader.is_active(),
        "60% depth + near-zero projected drain must release (both axes low)"
    );

    Ok(())
}

/// Rising-edge counter for the backpressure flag — shared between the
/// cost-axis flap tests below so the next backpressure-flap regression
/// test doesn't copy a third hand-rolled `was_active`/`observe` pair.
struct RisingEdges {
    count: u32,
    was_active: bool,
}
impl RisingEdges {
    fn new(initial: bool) -> Self {
        Self {
            count: 0,
            was_active: initial,
        }
    }
    fn observe(&mut self, r: &crate::actor::command::BackpressureReader) {
        let now = r.is_active();
        if now && !self.was_active {
            self.count += 1;
        }
        self.was_active = now;
    }
}

// r[verify sched.admission.work-per-turn]
/// **sh-024 §S2 — RED-FIRST.** Repeated 1.3 s Ticks at 1 % depth must
/// not FLAP the cost-axis backpressure. At the prior α = 0.3 with the
/// fold-everything topology, each 1.3 s spike (ewma = 0.39, drain =
/// 44.5 s ≥ 30) activated and the inter-spike µs/ms-class commands
/// decayed it below LOW 80 µs later — sh-024 saw `queue_backpressure`
/// +24 in 120 s. With the `prices_into_drain` gate the inter-spike
/// work never touches the EWMA, so flap is structurally impossible:
/// the gate either never engages (a few spikes from cold) or engages
/// once and HOLDS while the slow-Tick stream persists — both correct.
/// The assertion is ≤ 1 rising edge.
///
/// Review wf_22a3fa70: the test body MUST evaluate
/// `update_backpressure` TWICE per outer iteration — post-spike THEN
/// post-inter-spike-work — matching the production topology; with one
/// post-decay evaluation only the test was vacuously green at both α
/// values.
#[tokio::test]
async fn backpressure_cost_axis_single_spike_at_low_depth_does_not_flap() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor(db.pool.clone());
    let reader = actor.backpressure_flag();

    let mut edges = RisingEdges::new(reader.is_active());
    for _ in 0..24 {
        // The sh-024 trace shape: ONE 1.3 s slow Tick lands, the very
        // next main-loop `update_backpressure` observes it…
        actor.note_turn_cost(std::time::Duration::from_millis(1300));
        actor.update_backpressure(114, 10_000);
        edges.observe(&reader);
        // …then the inter-spike work (fast-lane admin / ReportPull-
        // Outcome / SubstituteProgress) processes. None of it folds
        // into the EWMA (`prices_into_drain` = false), so the only
        // production effect is another `update_backpressure` at the
        // NEXT main-cmd dequeue.
        actor.update_backpressure(114, 10_000);
        edges.observe(&reader);
    }
    assert!(
        edges.count <= 1,
        "cost-axis backpressure flapped {}× over 24 single-1.3s-spike \
         cycles (the sh-024 §S2 +24-in-120s flap). With the \
         prices_into_drain gate inter-spike work never decays the \
         EWMA: ≤ 1 rising edge regardless of whether the slow-Tick \
         stream accumulates past HIGH.",
        edges.count
    );
    Ok(())
}

// r[verify sched.admission.work-per-turn]
/// **sh-024 §S2 — the live_053 preservation half.** A genuine 140 s
/// pathological Tick at 1 % depth MUST engage and SURVIVE the
/// inter-evaluation work: at α = 0.05 one observation lands ewma =
/// 7 s (drain = 700 s ≫ HIGH). The inter-spike fast-lane / µs-class
/// commands no longer fold (`prices_into_drain` gate), so the
/// evidence is preserved BY CONSTRUCTION until the next Tick/
/// MergeDag — and one subsequent idle Tick at α=0.05 gives ewma =
/// 0.05×0.001 + 0.95×7.0 = 6.65 → drain = 665 s, STILL active. At
/// α = 0.3 the prior fold-everything-then-decay topology released on
/// the 17th cheap feed (42 × 0.7¹⁷ ≈ 0.097 s, drain = 9.7 s ≤ LOW) —
/// the gap `_does_not_flap` exposes from the other side.
#[tokio::test]
async fn backpressure_cost_axis_survives_fast_lane_decay() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor(db.pool.clone());
    let reader = actor.backpressure_flag();

    actor.note_turn_cost(std::time::Duration::from_secs(140));
    actor.update_backpressure(100, 10_000);
    assert!(
        reader.is_active(),
        "one 140 s Tick at q=100 must engage (drain = 100 × α × 140 s)"
    );
    // Inter-spike fast-lane / µs/ms-class commands process; none fold
    // (`prices_into_drain` = false), so production sees only another
    // `update_backpressure` at the NEXT main-cmd dequeue with ewma
    // unchanged.
    actor.update_backpressure(100, 10_000);
    assert!(
        reader.is_active(),
        "non-folding inter-spike work must leave the live_053 evidence \
         intact (ewma unchanged at 7.0, drain = 700 s)"
    );
    // ONE subsequent idle Tick. At α=0.05 ewma = 0.05×0.001 +
    // 0.95×7.0 = 6.65 → drain = 665 s — stays.
    actor.note_turn_cost(std::time::Duration::from_millis(1));
    actor.update_backpressure(100, 10_000);
    assert!(
        reader.is_active(),
        "left: one subsequent idle Tick released the live_053 evidence \
         (at α=0.3 the prior fold-everything topology decayed it under \
         LOW within ~17 cheap feeds) / right: at α=0.05 a single \
         normal-cost Tick keeps a genuine pathological turn engaged"
    );
    Ok(())
}

// r[verify sched.admission.work-per-turn]
/// **Bimodal-mix flap regression.** The mailbox during a nix-fast-build
/// burst is bimodal across 5 OOM: SubstituteProgress 4µs × 65k vs
/// MergeDag 303ms × 256. Pre-`prices_into_drain`-gate, one slow Tick
/// (3.3s) lifted ewma to 0.166 → drain = 247s @ q=1484 → ACTIVATE;
/// ~60 × 4µs SubstituteProgress feeds decayed it to 0.007 → drain =
/// 9.8s → DEACTIVATE 112µs later — 11 flaps in 5min. With the gate,
/// only MergeDag/Tick fold; the µs/ms-class inter-spike work is a
/// no-op on the EWMA and the gate engages once on the spike and
/// holds until subsequent MergeDag/Tick observations genuinely bring
/// projected drain under LOW.
#[tokio::test]
async fn backpressure_cost_axis_bimodal_mix_does_not_flap() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor(db.pool.clone());
    let reader = actor.backpressure_flag();

    // Warm the EWMA with the MergeDag baseline (303ms × a few).
    for _ in 0..8 {
        actor.note_turn_cost(std::time::Duration::from_millis(303));
    }
    let mut edges = RisingEdges::new(false);
    // The diagnosed trace, 5 cycles: one 3.3s Tick spike @ q≈1484,
    // then ~60 SubstituteProgress 4µs commands. Those commands do
    // NOT fold (`prices_into_drain` = false); production only
    // re-evaluates `update_backpressure` per dequeue.
    for _ in 0..5 {
        actor.note_turn_cost(std::time::Duration::from_millis(3300));
        actor.update_backpressure(1484, 10_000);
        edges.observe(&reader);
        for _ in 0..60 {
            actor.update_backpressure(1422, 10_000);
            edges.observe(&reader);
        }
    }
    assert!(
        edges.count <= 1,
        "bimodal mix flapped {}× (pre-gate: every 3.3s spike activated \
         and 60×4µs feeds at α=0.05 decayed it below LOW within the \
         same evaluation window — the live 11-flaps-in-5min trace). \
         With the prices_into_drain gate the µs/ms-class work never \
         touches the EWMA; the gate engages once and holds.",
        edges.count
    );
    Ok(())
}

/// `prices_into_drain` is exhaustive: only MergeDag and Tick fold
/// into the cost-axis EWMA. A new variant added without considering
/// this is the structural seam — the post-P1 re-diagnosis 89s-hold
/// was 8.8k mid-cost commands inflating an estimator meant to track
/// MergeDag drain time. This test pins the list so the next variant
/// add hits a deliberate `false` arm or an explicit `true` here.
#[test]
fn prices_into_drain_is_exhaustive() {
    use crate::actor::command::ActorCommand;
    // Same exhaustiveness device as `name()`: every arm enumerated;
    // a new variant is a compile error here. The `false` arms are the
    // assertion — they are NOT an `_ => false` wildcard.
    fn check(c: &ActorCommand) -> bool {
        match c {
            ActorCommand::MergeDag { .. } => true,
            ActorCommand::Tick => true,
            ActorCommand::ProcessCompletion { .. } => false,
            ActorCommand::SubstituteProgress { .. } => false,
            ActorCommand::CancelBuild { .. } => false,
            ActorCommand::PullAssignment { .. } => false,
            ActorCommand::ListMaterializationJobs { .. } => false,
            ActorCommand::ReportPullOutcome { .. } => false,
            ActorCommand::ReportRunningTelemetry { .. } => false,
            ActorCommand::ReportAttemptOutcome { .. } => false,
            ActorCommand::AckSpawnedIntents { .. } => false,
            ActorCommand::QueryBuildStatus { .. } => false,
            ActorCommand::WatchBuild { .. } => false,
            ActorCommand::CleanupTerminalBuild { .. } => false,
            ActorCommand::Admin(_) => false,
            ActorCommand::ClearPoison { .. } => false,
            ActorCommand::LeaderAcquired => false,
            ActorCommand::LeaderLost => false,
            ActorCommand::LeaderRebound => false,
            ActorCommand::Debug(_) => false,
        }
    }
    // Spot-check the production fn agrees with the exhaustive match
    // above (the match is the structural pin; these assert the
    // production impl matches it).
    assert!(ActorCommand::Tick.prices_into_drain());
    assert!(check(&ActorCommand::Tick));
    assert!(!ActorCommand::LeaderLost.prices_into_drain());
    assert!(!check(&ActorCommand::LeaderLost));
}

// ---------------------------------------------------------------------------
// Token-aware shutdown
// ---------------------------------------------------------------------------

fn req_features(f: Option<&[&str]>) -> SpawnIntentsRequest {
    SpawnIntentsRequest {
        features: f.map(|v| v.iter().map(|s| s.to_string()).collect()),
        ..Default::default()
    }
}

/// I-176: `features` filters Ready derivations by
/// `required_features ⊆ features` — the same subset check
/// `rejection_reason()` applies. A kvm derivation MUST appear in the
/// kvm pool's view and MUST NOT appear in a featureless pool's view.
/// Without this, the featureless pool spawns a builder that
/// hard_filter rejects (`feature-missing`), and the kvm pool sees
/// nothing and never spawns — deadlock.
///
/// I-181: feature-gated pools (`pf ≠ ∅`) additionally exclude
/// ∅-feature derivations. ∅ ⊆ anything is vacuously true, so the
/// subset check alone would have the kvm pool spawn for `hello` —
/// dispatch routes it to the cheap featureless pool, the kvm builder
/// idles until activeDeadlineSeconds.
// r[verify sched.admin.spawn-intents.feature-filter]
#[tokio::test]
async fn spawn_intents_feature_filter() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_sla(db.pool.clone());

    // 3 Ready derivations:
    //   a: required_features=[]             — featureless work
    //   b: required_features=["kvm"]        — needs kvm
    //   c: required_features=["kvm","nixos-test"] — the I-176 trigger
    actor.test_inject_ready("a", None, "x86_64-linux", false);
    actor.test_inject_ready_with_features("b", None, "x86_64-linux", &["kvm"]);
    actor.test_inject_ready_with_features("c", None, "x86_64-linux", &["kvm", "nixos-test"]);

    // --- Unfiltered (None): backward compat — emits all 3. ---
    let snap = actor.compute_spawn_intents(&req_features(None));
    assert_eq!(snap.intents.len(), 3, "unfiltered: all 3 Ready emitted");
    assert_eq!(
        snap.queued_by_system.get("x86_64-linux"),
        Some(&3),
        "queued_by_system unfiltered"
    );

    // --- Featureless pool (Some([])): only `a` passes. ---
    // `[] ⊆ []` ✓; `["kvm"] ⊆ []` ✗; `["kvm","nixos-test"] ⊆ []` ✗.
    let snap = actor.compute_spawn_intents(&req_features(Some(&[])));
    assert_eq!(
        snap.intents.len(),
        1,
        "featureless pool: kvm derivations excluded → no wasted spawn"
    );
    assert_eq!(snap.intents[0].intent_id, "a");
    assert!(snap.intents[0].required_features.is_empty());
    // queued_by_system is filter-independent (ComponentScaler reads it).
    assert_eq!(snap.queued_by_system.get("x86_64-linux"), Some(&3));

    // --- kvm pool (Some(["kvm","nixos-test","big-parallel"])): b+c. ---
    // I-181: `a` (∅-feature) is EXCLUDED — featureless pool owns it.
    // `["kvm"] ⊆ pf` ✓; `["kvm","nixos-test"] ⊆ pf` ✓.
    // The load-bearing assertion: `b`+`c` are visible — without this
    // the kvm pool never spawns (I-176). `a` invisible — without THAT
    // the kvm pool spawns a phantom .metal builder for `hello` (I-181).
    let snap =
        actor.compute_spawn_intents(&req_features(Some(&["kvm", "nixos-test", "big-parallel"])));
    let ids: std::collections::HashSet<_> =
        snap.intents.iter().map(|i| i.intent_id.as_str()).collect();
    assert_eq!(
        ids,
        ["b", "c"].into(),
        "I-181: kvm pool counts feature-required work only (b+c, NOT a)"
    );

    // --- kvm-only pool (Some(["kvm"])): `b` only. ---
    // I-181: `a` excluded (∅-feature). I-176: `c` excluded
    // (`["kvm","nixos-test"] ⊆ ["kvm"]` is false — `nixos-test`
    // missing). Mirrors hard_filter exactly: a kvm-only worker can't
    // build a derivation that also needs nixos-test.
    let snap = actor.compute_spawn_intents(&req_features(Some(&["kvm"])));
    assert_eq!(
        snap.intents.len(),
        1,
        "kvm-only pool: ∅-feature (I-181) and nixos-test (I-176) both excluded"
    );
    assert_eq!(snap.intents[0].intent_id, "b");
}

/// I-181 isolation: ONE ∅-feature derivation Ready. kvm pool's view
/// MUST be empty (featureless pool owns it); featureless pool's view
/// MUST contain it. Regression: `rsb hello-shallow` (no
/// required_features) spawned both `x86-64-medium` AND
/// `x86-64-kvm-xlarge` — the subset check `∅ ⊆ ["kvm",...]` is
/// vacuously true → kvm pool counted it → controller spawned a .metal
/// instance that idled until deadline.
// r[verify sched.admin.spawn-intents.feature-filter]
#[tokio::test]
async fn spawn_intents_kvm_pool_excludes_featureless_work() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_sla(db.pool.clone());

    // Single Ready derivation, required_features = ∅ (e.g., hello).
    actor.test_inject_ready("hello", None, "x86_64-linux", false);

    // kvm pool query → empty. The bug: pre-I-181 this was 1.
    let snap = actor.compute_spawn_intents(&req_features(Some(&["kvm"])));
    assert!(
        snap.intents.is_empty(),
        "I-181: feature-gated pool MUST NOT count ∅-feature work"
    );

    // Featureless pool query → 1. The featureless pool owns it.
    let snap = actor.compute_spawn_intents(&req_features(Some(&[])));
    assert_eq!(
        snap.intents.len(),
        1,
        "featureless pool owns ∅-feature work"
    );

    // Unfiltered (None) → 1. CLI/status display still sees everything.
    let snap = actor.compute_spawn_intents(&req_features(None));
    assert_eq!(snap.intents.len(), 1, "None = no filter (CLI back-compat)");
}

/// I-204: `soft_features` strips capability-hint features at DAG
/// insertion. A `{big-parallel}` derivation MUST count toward the
/// featureless pool (any builder can run it) and MUST NOT count toward
/// the kvm pool (it doesn't need kvm). Regression: `rsb large-shallow`
/// (firefox/chromium carry `big-parallel`) spawned `x86-64-kvm-xlarge`
/// because the I-181 ∅-guard only fires on truly-empty
/// `required_features`; `{big-parallel} ⊆ {kvm,nixos-test,big-parallel}`
/// passed the subset check. With `soft_features=[big-parallel]` the
/// derivation enters the DAG as ∅-feature and I-181 fires.
// r[verify sched.dispatch.soft-features+3]
#[tokio::test]
async fn spawn_intents_soft_features_strip() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_cfg(
        db.pool.clone(),
        DagActorConfig {
            sla: test_sla_config(),
            soft_features: vec!["big-parallel".into()],
            ..Default::default()
        },
    );

    actor.test_inject_ready_with_features("ff", None, "x86_64-linux", &["big-parallel"]);
    actor.test_inject_ready_with_features("vm", None, "x86_64-linux", &["kvm", "big-parallel"]);

    // Featureless pool: emits ff (stripped → ∅), NOT vm (stripped →
    // {kvm}, fails subset check vs []).
    let snap = actor.compute_spawn_intents(&req_features(Some(&[])));
    assert_eq!(
        snap.intents
            .iter()
            .map(|i| &i.intent_id)
            .collect::<Vec<_>>(),
        vec!["ff"],
        "I-204: featureless pool owns big-parallel-only work after strip"
    );

    // kvm pool: emits vm (stripped → {kvm} ⊆ pf), NOT ff (stripped → ∅,
    // I-181 ∅-guard fires).
    let kvm = req_features(Some(&["kvm", "nixos-test", "big-parallel"]));
    let snap = actor.compute_spawn_intents(&kvm);
    assert_eq!(
        snap.intents
            .iter()
            .map(|i| &i.intent_id)
            .collect::<Vec<_>>(),
        vec!["vm"],
        "I-204: kvm pool excludes big-parallel-only work, keeps kvm work"
    );

    // Regression: leader-acquire calls clear_persisted_state which
    // replaces self.dag. soft_features MUST survive — first prod deploy
    // of I-204 was a no-op because recovery's `self.dag =
    // DerivationDag::new()` reset it to empty before the first merge.
    actor.clear_persisted_state();
    actor.test_inject_ready_with_features("ff2", None, "x86_64-linux", &["big-parallel"]);
    let snap = actor.compute_spawn_intents(&kvm);
    assert!(
        snap.intents.is_empty(),
        "I-204: soft_features survives clear_persisted_state (leader transition)"
    );
}

/// D6: soft features are strip-only. SLA `solve_intent_for` +
/// `resource_floor` doubling own initial sizing. I-204 regression
/// preserved: stripping survives leader transition.
#[tokio::test]
async fn soft_feature_strip_only() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_cfg(
        db.pool.clone(),
        DagActorConfig {
            soft_features: vec!["big-parallel".into(), "benchmark".into()],
            ..Default::default()
        },
    );

    // big-parallel → stripped; D6: floor stays zero.
    actor.test_inject_ready_with_features("ff", None, "x86_64-linux", &["big-parallel"]);
    let s = actor.dag.node("ff").unwrap();
    assert!(s.required_features().is_empty(), "stripped");
    // r35: effective_features must agree — `apply_soft_features` routes
    // through the `set_required_features` write-gate so both fields
    // are pinned. Pre-r35 a constructor-only chokepoint left
    // `effective_features = ["big-parallel"]` here.
    assert!(
        s.effective_features().as_slice().is_empty(),
        "stripped (effective)"
    );
    assert_eq!(
        s.sched.resource_floor,
        Default::default(),
        "D6: strip-only — floor stays zero"
    );

    // I-204: survives leader transition.
    actor.clear_persisted_state();
    actor.test_inject_ready_with_features("ff2", None, "x86_64-linux", &["big-parallel"]);
    let s2 = actor.dag.node("ff2").unwrap();
    assert!(
        s2.required_features().is_empty(),
        "stripping survives clear_persisted_state"
    );
    assert!(
        s2.effective_features().as_slice().is_empty(),
        "stripping survives clear_persisted_state (effective)"
    );
}

/// **r35 B0 (4/4 validator-converged blocker)** — `apply_soft_features`
/// mutates `required_features` AFTER construction (both the merge and
/// recovery paths call it). A constructor-only `effective_features`
/// derivation captures the PRE-strip declared set, then desyncs when
/// `apply_soft_features` strips `required_features` but not
/// `effective_features` — silently regressing I-204 (soft features
/// stripped at insertion so `rejection_reason`'s `feature-missing`
/// clause sees hardware-gate features only).
///
/// The fix: `set_required_features` write-gate that re-derives
/// `effective_features` atomically. `apply_soft_features` becomes a
/// fourth derivation site routing through it.
///
/// **Write the assertion against `effective_features`, not
/// `required_features` — a naive test against `required_features`
/// passes for the wrong reason** (`apply_soft_features` always
/// mutated that field).
///
/// Constructed via `insert_recovered_node` (NOT direct field
/// assignment) per §Spike-assertion-must-execute — proves production
/// code passes through the chokepoint.
///
/// **Pre-fix: RED** — `effective_features = ["big-parallel", "kvm"]`
/// (constructor-only derivation never re-derives after the strip).
// r[verify sched.sla.fod-feature-derivation+3]
// r[verify sched.dispatch.soft-features+3]
#[tokio::test]
async fn apply_soft_features_re_derives_effective_features() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_cfg(
        db.pool.clone(),
        DagActorConfig {
            soft_features: vec!["big-parallel".into()],
            ..Default::default()
        },
    );

    // Non-FOD with `["big-parallel", "kvm"]`. `apply_soft_features`
    // (called by `insert_recovered_node`) strips `big-parallel`.
    actor.test_inject_ready_with_features(
        "ff-soft",
        None,
        "x86_64-linux",
        &["big-parallel", "kvm"],
    );
    let s = actor.dag.node("ff-soft").unwrap();
    // Both fields must agree post-strip — pin both so the next
    // `required_features` mutation site can't desync them silently.
    assert_eq!(
        s.required_features(),
        &["kvm".to_string()],
        "apply_soft_features strips `big-parallel` from required_features",
    );
    assert_eq!(
        s.effective_features().as_slice(),
        &["kvm".to_string()],
        "apply_soft_features MUST re-derive effective_features — a \
         constructor-only chokepoint leaves it desynced at \
         [\"big-parallel\", \"kvm\"]",
    );
}

/// **sh-008** — `apply_soft_features` partitions (records the stripped
/// soft set on `DerivationState.soft_features`) instead of discarding,
/// AND the recorded set reaches `explore::next`'s `feature_probes`
/// lookup via `DrvHints.soft_features`. Pre-fix: `feature_probes.
/// big-parallel` is dead config — `hints.required_features` is the
/// post-strip `effective_features` set, so a soft-only `big-parallel`
/// never matches and the cold-start probe falls through to the default
/// `[sla].probe.cpu`.
///
/// RED at `0c4c55d5b`: `state.soft_features()` does not exist; with the
/// accessor stubbed to `[]` the second assert fails: `intent.cores`
/// equals `test_sla_config().probe.cpu` (4), not the `feature_probes.
/// big-parallel.cpu` override (48).
// r[verify sched.dispatch.soft-features+3]
#[tokio::test]
async fn apply_soft_features_records_for_probe_lookup() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_cfg(
        db.pool.clone(),
        DagActorConfig {
            sla: test_sla_config(),
            soft_features: vec!["big-parallel".into()],
            ..Default::default()
        },
    );
    actor.sla_config.feature_probes.insert(
        "big-parallel".into(),
        crate::sla::config::ProbeShape {
            cpu: 48.0,
            mem_per_core: 1 << 30,
            mem_base: 4 << 30,
            deadline_secs: 3600,
        },
    );

    actor.test_inject_ready_with_features("ff-bp", None, "x86_64-linux", &["big-parallel"]);
    let state = actor.dag.node("ff-bp").unwrap();
    assert_eq!(
        state.soft_features(),
        ["big-parallel"],
        "apply_soft_features records the stripped soft set"
    );
    assert!(
        state.effective_features().as_slice().is_empty(),
        "§13e: routing chokepoint stays soft-free"
    );

    let (hw, cost, ig) = actor.solve_inputs();
    let intent = actor.solve_intent_for(state, &hw, &cost, ig);
    assert_eq!(
        intent.cores, 48,
        "feature_probes[big-parallel].cpu reaches the cold-start intent \
         via DrvHints.soft_features (pre-fix: default probe.cpu={})",
        actor.sla_config.probe.cpu
    );
}

// r[verify sched.sla.reactive-floor+8]
/// D4: `solve_intent_for` clamps its solved (mem, disk) at
/// `resource_floor`. A derivation with `floor.mem=32GiB` (from prior
/// `observe_peaks` cycles) gets a SpawnIntent with mem ≥ 32GiB
/// even when the SLA solve would return less.
#[tokio::test]
async fn solve_intent_for_clamps_at_resource_floor() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    // `bare_actor_sla`: realistic ceilings (256 GiB > 32 GiB floor). The
    // chokepoint applies `.max(floor).min(ceil)`; `observe_peaks`
    // caps floor at ceil so the order is sound, but `test_default()`'s
    // tiny 2 GiB max_mem would otherwise make this assertion test the
    // ceiling, not the floor.
    let mut actor = bare_actor_sla(db.pool.clone());

    // floor.{mem,disk}=32/50 GiB; cold-start solve (no fit, no override)
    // returns probe-default (typically a few GiB) — the clamp raises both.
    actor.test_inject_ready_with_floor("a", "x86_64-linux", 32 << 30, 50 << 30);
    let state = actor.dag.node("a").unwrap();
    let intent = solve_intent(&actor, state);
    assert!(
        intent.mem_bytes >= 32 << 30,
        "D4: solve_intent_for clamps mem at floor (got {})",
        intent.mem_bytes
    );
    assert!(
        intent.disk_bytes >= 50 << 30,
        "D4: solve_intent_for clamps disk at floor (got {})",
        intent.disk_bytes
    );

    // floor=zero (cold start) → solve returns its own value unchanged.
    actor.test_inject_ready_with_floor("b", "x86_64-linux", 0, 0);
    let state = actor.dag.node("b").unwrap();
    let intent_b = solve_intent(&actor, state);
    assert!(intent_b.mem_bytes < 32 << 30, "control: floor=0 → no clamp");
}

// r[verify sched.sla.intent-from-solve]
/// D4: `solve_intent_for` clamps mem AND disk at `sla_ceilings` regardless
/// of which `intent_for` branch (or the post-solve `forced_mem` overlay)
/// produced the value. The serial-build early-return and the `--mem`
/// override used to pass `disk_p90` / `forced_mem` through unclamped →
/// permanently-Pending pod after an operator tightens `max_*`.
#[tokio::test]
async fn solve_intent_for_clamps_at_ceil() {
    use crate::sla::types::*;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_sla(db.pool.clone());
    let max_mem = actor.sla_ceilings.max_mem;
    let max_disk = actor.sla_ceilings.max_disk;

    // disk: serial drv with disk_p90 above max_disk.
    actor.sla_estimator.seed(FittedParams {
        key: ModelKey {
            pname: "big".into(),
            system: "x86_64-linux".into(),
            tenant: String::new(),
        },
        fit: DurationFit::Amdahl {
            s: RefSeconds(30.0),
            p: RefSeconds(2000.0),
        },
        mem: MemFit::Independent {
            p90: MemBytes(2 << 30),
        },
        disk_p90: Some(DiskBytes(max_disk + (50 << 30))),
        disk_p90_raw: Some(RawDiskP90(DiskBytes(max_disk + (50 << 30)))),
        sigma_resid: 0.1,
        log_residuals: Vec::new(),
        n_eff_ring: RingNEff(10.0),
        fit_df: FitDf(10.0),
        n_distinct_c: 5,
        sum_w: 10.0,
        span: 8.0,
        explore: ExploreState {
            distinct_c: 3,
            min_c: RawCores(4.0),
            max_c: RawCores(32.0),
            saturated: false,
            last_wall: WallSeconds(0.0),
        },
        t_min_ci: None,
        ci_computed_at: None,
        tier: None,
        hw_bias: Default::default(),
        alpha: crate::sla::alpha::UNIFORM,
        prior_source: None,
        is_fod: false,
    });
    actor.test_inject_ready("d", Some("big"), "x86_64-linux", false);
    actor.dag.node_mut("d").unwrap().enable_parallel_building = Some(false);
    let intent = solve_intent(&actor, actor.dag.node("d").unwrap());
    assert!(
        intent.disk_bytes <= max_disk,
        "serial branch: disk {} clamped to max_disk {}",
        intent.disk_bytes,
        max_disk
    );

    // mem: forced_mem override above max_mem (overlay applied AFTER
    // solve, before the chokepoint).
    actor
        .sla_estimator
        .seed_overrides(vec![crate::db::SlaOverrideRow {
            pname: "big".into(),
            mem_bytes: Some((max_mem + (64 << 30)) as i64),
            ..Default::default()
        }]);
    let intent = solve_intent(&actor, actor.dag.node("d").unwrap());
    assert!(
        intent.mem_bytes <= max_mem,
        "forced_mem overlay: mem {} clamped to max_mem {}",
        intent.mem_bytes,
        max_mem
    );

    // r[verify sched.sla.reactive-floor+8]
    // cores (a): forced_cores override above max_cores. The override path
    // emits raw `c.ceil()` with no `.min(max_cores)`; the chokepoint is
    // the only clamp.
    let max_cores = actor.sla_ceilings.max_cores as u32;
    actor
        .sla_estimator
        .seed_overrides(vec![crate::db::SlaOverrideRow {
            pname: "big".into(),
            cores: Some(f64::from(max_cores) + 50.0),
            ..Default::default()
        }]);
    let intent = solve_intent(&actor, actor.dag.node("d").unwrap());
    assert!(
        intent.cores <= max_cores,
        "forced_cores override: cores {} clamped to max_cores {}",
        intent.cores,
        max_cores
    );
    actor.sla_estimator.seed_overrides(vec![]);

    // cores (b): explore-frozen path returns raw `st.max_c` which can
    // exceed a since-tightened `max_cores`. Seed n_eff<3 so the gate
    // routes to `explore::next` → frozen → `max_c`.
    actor.sla_estimator.seed(FittedParams {
        key: ModelKey {
            pname: "wide".into(),
            system: "x86_64-linux".into(),
            tenant: String::new(),
        },
        fit: DurationFit::Probe,
        mem: MemFit::Independent {
            p90: MemBytes(2 << 30),
        },
        disk_p90: None,
        disk_p90_raw: None,
        sigma_resid: 0.2,
        log_residuals: Vec::new(),
        n_eff_ring: RingNEff(2.0),
        fit_df: FitDf(2.0),
        n_distinct_c: 5,
        sum_w: 10.0,
        span: 2.0,
        explore: ExploreState {
            distinct_c: 2,
            min_c: RawCores(1.0),
            max_c: RawCores(f64::from(max_cores) + 20.0),
            saturated: false,
            last_wall: WallSeconds(0.0),
        },
        t_min_ci: None,
        ci_computed_at: None,
        tier: None,
        hw_bias: Default::default(),
        alpha: crate::sla::alpha::UNIFORM,
        prior_source: None,
        is_fod: false,
    });
    actor.test_inject_ready("w", Some("wide"), "x86_64-linux", false);
    let intent = solve_intent(&actor, actor.dag.node("w").unwrap());
    assert!(
        intent.cores <= max_cores,
        "explore-frozen st.max_c: cores {} clamped to max_cores {}",
        intent.cores,
        max_cores
    );
}

/// D7: `solve_intent_for` deadline_secs falls back to
/// `probe.deadline_secs` for `DurationFit::Probe` (the n_eff<3 ∨
/// span<4 explore phase). The bug: `Some(Probe)` entered the fitted
/// `.map()` branch, `t_at()=∞ → q99×5 as u32` saturated → clamped to
/// the 24h cap instead of the configured 1h probe deadline. Same
/// regression poisoned `predicted.wall_secs` with ∞.
#[tokio::test]
async fn solve_intent_for_probe_fit_uses_probe_deadline() {
    use crate::sla::types::*;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_sla(db.pool.clone());
    actor.sla_estimator.seed(FittedParams {
        key: ModelKey {
            pname: "exploring".into(),
            system: "x86_64-linux".into(),
            tenant: String::new(),
        },
        fit: DurationFit::Probe,
        mem: MemFit::Independent {
            p90: MemBytes(4 << 30),
        },
        disk_p90: None,
        disk_p90_raw: None,
        sigma_resid: 0.2,
        log_residuals: Vec::new(),
        n_eff_ring: RingNEff(1.0),
        fit_df: FitDf(1.0),
        n_distinct_c: 5,
        sum_w: 10.0,
        span: 1.0,
        explore: ExploreState {
            distinct_c: 1,
            min_c: RawCores(4.0),
            max_c: RawCores(4.0),
            saturated: false,
            last_wall: WallSeconds(800.0),
        },
        t_min_ci: None,
        ci_computed_at: None,
        tier: None,
        hw_bias: Default::default(),
        alpha: crate::sla::alpha::UNIFORM,
        prior_source: None,
        is_fod: false,
    });
    actor.test_inject_ready("p", Some("exploring"), "x86_64-linux", false);
    let intent = solve_intent(&actor, actor.dag.node("p").unwrap());
    assert_eq!(
        intent.deadline_secs, 3600,
        "Probe fit → probe.deadline_secs, not 86400 (∞→u32::MAX→cap)"
    );
    assert!(
        intent.predicted.is_none(),
        "Probe fit → no prediction snapshot (wall_secs would be ∞)"
    );
}

/// D7: a sub-second fitted curve (trivial-builders) must not produce a
/// tiny `activeDeadlineSeconds` that kills the Job before pod startup
/// completes. `q99×5` for a 0.5s build is ~3; the fix floors the
/// fitted-path computation at `probe.deadline_secs` so the spawn-kill
/// loop can't form (the pod dies before any worker report, so nothing
/// would ever promote the deadline floor out of the loop).
#[tokio::test]
async fn solve_intent_for_subsecond_fit_floored_at_probe_deadline() {
    use crate::sla::types::*;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_sla(db.pool.clone());
    actor.sla_estimator.seed(FittedParams {
        key: ModelKey {
            pname: "trivial".into(),
            system: "x86_64-linux".into(),
            tenant: String::new(),
        },
        // s+p/c ≈ 0.5s at any c.
        fit: DurationFit::Amdahl {
            s: RefSeconds(0.4),
            p: RefSeconds(0.1),
        },
        mem: MemFit::Independent {
            p90: MemBytes(1 << 30),
        },
        disk_p90: None,
        disk_p90_raw: None,
        sigma_resid: 0.1,
        log_residuals: Vec::new(),
        n_eff_ring: RingNEff(10.0),
        fit_df: FitDf(10.0),
        n_distinct_c: 5,
        sum_w: 10.0,
        span: 8.0,
        explore: ExploreState {
            distinct_c: 3,
            min_c: RawCores(1.0),
            max_c: RawCores(8.0),
            saturated: false,
            last_wall: WallSeconds(0.5),
        },
        t_min_ci: None,
        ci_computed_at: None,
        tier: None,
        hw_bias: Default::default(),
        alpha: crate::sla::alpha::UNIFORM,
        prior_source: None,
        is_fod: false,
    });
    actor.test_inject_ready("t", Some("trivial"), "x86_64-linux", false);
    let intent = solve_intent(&actor, actor.dag.node("t").unwrap());
    assert_eq!(
        intent.deadline_secs, 3600,
        "sub-second fit floored at probe.deadline_secs (got {})",
        intent.deadline_secs
    );
    // Prediction snapshot IS recorded for a real fit (finite wall_secs).
    let p = intent.predicted.expect("fitted → prediction recorded");
    assert!(p.wall_secs.is_some_and(|w| w.is_finite() && w < 10.0));
}

/// D7: `feature_probes.{feat}.deadline_secs` is honoured for unfitted
/// builds with that feature — same lookup `explore::next` uses.
#[tokio::test]
async fn solve_intent_for_feature_probe_deadline() {
    use crate::sla::config;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut cfg = test_sla_config();
    cfg.feature_probes.insert(
        "kvm".into(),
        config::ProbeShape {
            cpu: 8.0,
            mem_per_core: 2 << 30,
            mem_base: 8 << 30,
            deadline_secs: 7200,
        },
    );
    let mut actor = bare_actor_cfg(
        db.pool.clone(),
        DagActorConfig {
            sla: cfg,
            ..Default::default()
        },
    );
    actor.test_inject_ready_with_features("k", Some("vm-test"), "x86_64-linux", &["kvm"]);
    let intent = solve_intent(&actor, actor.dag.node("k").unwrap());
    assert_eq!(
        intent.deadline_secs, 7200,
        "feature_probes.kvm.deadline_secs (not the default probe's 3600)"
    );
}

// ---------------------------------------------------------------------------

/// I-107: `queued_by_system` is a per-system breakdown of
/// `queued_derivations` — Ready-only, sum across keys equals the
/// scalar. Non-Ready (Queued/Assigned/Running) drvs do NOT count.
#[tokio::test]
async fn cluster_snapshot_queued_by_system_sums_to_scalar() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor(db.pool.clone());

    // 3 Ready x86_64, 1 Ready aarch64. test_inject_ready puts the
    // node in the DAG; the scalar and the per-system breakdown both
    // derive from DAG state.
    for (h, sys) in [
        ("x1", "x86_64-linux"),
        ("x2", "x86_64-linux"),
        ("x3", "x86_64-linux"),
        ("a1", "aarch64-linux"),
    ] {
        actor.test_inject_ready(h, None, sys, false);
    }

    let snap = actor.compute_cluster_snapshot();

    assert_eq!(snap.queued_by_system.get("x86_64-linux"), Some(&3));
    assert_eq!(snap.queued_by_system.get("aarch64-linux"), Some(&1));
    assert_eq!(
        snap.queued_by_system.values().sum::<u32>(),
        snap.queued_derivations,
        "sum across systems == scalar (both Ready-only)"
    );
}

// D3-retargeted (T-D3.3): the bucket is job-derived — the walk-era
// status arm died with the Substituting status.
/// `substituting_derivations` counts nodes carrying a pending
/// UNCLAIMED materialization job and is disjoint from queued/running:
/// a Ready node with a pending job is substitution backlog (the
/// ComponentScaler's store signal), not builder-queue backlog.
// r[verify sched.admin.snapshot-substituting+4]
#[tokio::test]
async fn snapshot_counts_substituting() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor(db.pool.clone());

    // 3 Ready nodes with pending unclaimed jobs, 1 plain Ready,
    // 1 Running — disjoint counts.
    for h in ["s1", "s2", "s3"] {
        actor.test_inject_ready(h, None, "x86_64-linux", false);
        actor.materialization_jobs.insert(
            crate::state::DrvHash::from(h),
            crate::actor::materialize::JobViewEntry::test_unclaimed(Uuid::new_v4()),
        );
    }
    actor.test_inject_ready("q1", None, "x86_64-linux", false);
    actor.test_inject_ready("r1", None, "x86_64-linux", false);
    actor
        .dag
        .node_mut("r1")
        .unwrap()
        .set_status_for_test(DerivationStatus::Running);

    let snap = actor.compute_cluster_snapshot();

    assert_eq!(
        snap.substituting_derivations, 3,
        "pending unclaimed jobs ARE the substituting bucket"
    );
    assert_eq!(snap.queued_derivations, 1, "job-backed Ready is NOT queued");
    assert_eq!(
        snap.running_derivations, 1,
        "job-backed Ready is NOT running"
    );
    assert_eq!(
        snap.queued_by_system.values().sum::<u32>(),
        1,
        "job-backed Ready does NOT enter queued_by_system"
    );
}

/// D2/D5: FODs and non-FODs go through the SAME `compute_spawn_intents`
/// path; `intent.kind` carries the ADR-019 boundary. `kind=Builder`
/// excludes FODs; `kind=Fetcher` excludes non-FODs; unfiltered emits
/// both. I-143: `systems` filter excludes other-arch derivations.
// r[verify sched.admin.spawn-intents.feature-filter]
// r[verify ctrl.pool.fetcher-spawn-builtin]
#[tokio::test]
async fn spawn_intents_kind_and_system_filter() {
    use rio_proto::types::ExecutorKind;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_sla(db.pool.clone());

    actor.test_inject_ready("build-x86", None, "x86_64-linux", false);
    actor.test_inject_ready("build-arm", None, "aarch64-linux", false);
    actor.test_inject_ready("fod-x86", None, "x86_64-linux", true);
    actor.test_inject_ready("fod-builtin", None, "builtin", true);

    let ids = |s: &crate::actor::SpawnIntentsSnapshot| -> std::collections::HashSet<String> {
        s.intents.iter().map(|i| i.intent_id.clone()).collect()
    };

    // Unfiltered: all four. Kinds tagged.
    let all = actor.compute_spawn_intents(&SpawnIntentsRequest::default());
    assert_eq!(
        all.intents.len(),
        4,
        "D2: FOD and non-FOD both emit intents"
    );
    assert_eq!(
        all.intents
            .iter()
            .find(|i| i.intent_id == "fod-x86")
            .map(|i| i.kind),
        Some(ExecutorKind::Fetcher.into())
    );
    assert_eq!(
        all.intents
            .iter()
            .find(|i| i.intent_id == "build-x86")
            .map(|i| i.kind),
        Some(ExecutorKind::Builder.into())
    );

    // kind=Builder → builds only.
    let b = actor.compute_spawn_intents(&SpawnIntentsRequest {
        kind: Some(ExecutorKind::Builder),
        ..Default::default()
    });
    assert_eq!(ids(&b), ["build-x86".into(), "build-arm".into()].into());

    // kind=Fetcher + systems=[x86_64-linux, builtin] → fod-x86 +
    // fod-builtin (the controller always appends `builtin`).
    let f = actor.compute_spawn_intents(&SpawnIntentsRequest {
        kind: Some(ExecutorKind::Fetcher),
        systems: vec!["x86_64-linux".into(), "builtin".into()],
        ..Default::default()
    });
    assert_eq!(ids(&f), ["fod-x86".into(), "fod-builtin".into()].into());

    // I-143: systems=[aarch64-linux] → build-arm only (kind unfiltered;
    // FODs are x86/builtin so excluded).
    let arm = actor.compute_spawn_intents(&SpawnIntentsRequest {
        systems: vec!["aarch64-linux".into()],
        ..Default::default()
    });
    assert_eq!(
        ids(&arm),
        ["build-arm".into()].into(),
        "I-143: x86 pool doesn't see aarch64 backlog and vice-versa"
    );
    // queued_by_system is filter-independent.
    assert_eq!(arm.queued_by_system.get("x86_64-linux"), Some(&2));
    assert_eq!(arm.queued_by_system.get("aarch64-linux"), Some(&1));
    assert_eq!(arm.queued_by_system.get("builtin"), Some(&1));
}

/// `compute_spawn_intents` returns priority-sorted (critical-path
/// first), not `dag.iter_nodes()` HashMap order. The controller
/// truncates to `[..headroom]` under `maxConcurrent` — unsorted, a
/// high-priority drv past the prefix would get no pod and fail
/// resource-fit on the small ones spawned for low-priority work.
#[tokio::test]
async fn compute_spawn_intents_priority_sorted() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor(db.pool.clone());
    actor.test_inject_ready("lo", Some("p"), "x86_64-linux", false);
    actor.test_inject_ready("hi", Some("p"), "x86_64-linux", false);
    actor.test_inject_ready("mid", Some("p"), "x86_64-linux", false);
    actor.test_set_priority("lo", 1.0);
    actor.test_set_priority("hi", 100.0);
    actor.test_set_priority("mid", 50.0);

    let snap = actor.compute_spawn_intents(&SpawnIntentsRequest::default());
    let order: Vec<_> = snap.intents.iter().map(|i| i.intent_id.as_str()).collect();
    assert_eq!(
        order,
        vec!["hi", "mid", "lo"],
        "priority desc, not HashMap order"
    );
}

// ---------------------------------------------------------------------------
// §13b forecast frontier (A10)
// ---------------------------------------------------------------------------

/// Bare actor with `[sla].lead_time_seed` populated so the forecast
/// pass is reachable (`max_lead > 0`). Ceilings/probe from
/// [`test_sla_config`] so unfitted drvs solve to `probe.cpu = 4`
/// cores. The seed is keyed on `test-hw` — the one configured hwClass
/// — so the per-intent `max_lead_for` (r33 bug_007) sees a routable
/// class. Pre-r33 the fixture used a key (`intel-7`) NOT in
/// `hw_classes`; the global `max(values())` didn't care, but
/// `validate_both` rejects that shape and `class_routes` (correctly)
/// returns `false` for an unknown class.
fn bare_actor_forecast(pool: sqlx::PgPool, max_lead: f64, max_forecast_cores: u32) -> DagActor {
    use crate::sla::config::CapacityType;
    let mut sla = test_sla_config();
    sla.lead_time_seed
        .insert(("test-hw".into(), CapacityType::Spot), max_lead);
    sla.max_forecast_cores_per_tenant = max_forecast_cores;
    bare_actor_cfg(
        pool,
        DagActorConfig {
            sla,
            ..Default::default()
        },
    )
}

/// ADR-023 §Forecast: forecast frontier is exactly one DAG layer. A
/// Queued drv whose every incomplete dep is Running with `ETA <
/// max_h lead_time` emits a forecast intent; a Queued drv with a
/// Queued dep does NOT (no progress-grounded ETA — propagating
/// `ETA(B)=ETA(A)+T(B)` would compound σ_resid per hop and admit
/// trivial-drv fanout chains).
// r[verify sched.sla.forecast.one-layer+2]
#[tokio::test]
async fn forecast_frontier_one_layer_only() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_forecast(db.pool.clone(), 45.0, 2_000);

    // DAG: a(Running, T=100, elapsed=70 → eta=30) → b(Queued) → c(Queued)
    // lead_time=45. b is forecast (30 < 45). c is NOT (b is Queued,
    // not Running). Also d(Queued) ← e(Ready): e is not Running →
    // d not forecast.
    actor.test_inject_at("a", "x86_64-linux", DerivationStatus::Running);
    actor.test_inject_at("b", "x86_64-linux", DerivationStatus::Queued);
    actor.test_inject_at("c", "x86_64-linux", DerivationStatus::Queued);
    actor.test_inject_at("d", "x86_64-linux", DerivationStatus::Queued);
    actor.test_inject_at("e", "x86_64-linux", DerivationStatus::Ready);
    actor.test_inject_edge("b", "a");
    actor.test_inject_edge("c", "b");
    actor.test_inject_edge("d", "e");
    actor.test_set_running_eta("a", 100.0, 70, 8);

    let snap = actor.compute_spawn_intents(&SpawnIntentsRequest::default());
    let by_id: std::collections::HashMap<_, _> = snap
        .intents
        .iter()
        .map(|i| (i.intent_id.as_str(), i))
        .collect();

    let b = by_id
        .get("b")
        .expect("b is forecast (dep Running, eta<lead)");
    assert!(
        (b.eta_seconds - 30.0).abs() < 2.0,
        "b.eta = T(c)-elapsed = 100-70 = 30, got {}",
        b.eta_seconds
    );
    assert!(!by_id.contains_key("c"), "c NOT forecast: dep b is Queued");
    assert!(
        !by_id.contains_key("d"),
        "d NOT forecast: dep e is Ready (not Running)"
    );
    assert_eq!(b.ready, Some(false), "forecast ⇒ ready=false");
    // e is Ready → emitted at eta=0 (the Ready loop, not forecast).
    assert_eq!(by_id["e"].eta_seconds, 0.0, "Ready ⇒ eta=0");
    assert_eq!(by_id["e"].ready, Some(true), "Ready loop ⇒ ready=true");
    // Ready-before-forecast in the sort: e (ready) precedes b (forecast)
    // regardless of priority (both default 0 here).
    let pos_e = snap
        .intents
        .iter()
        .position(|i| i.intent_id == "e")
        .unwrap();
    let pos_b = snap
        .intents
        .iter()
        .position(|i| i.intent_id == "b")
        .unwrap();
    assert!(pos_e < pos_b, "Ready sorts before forecast");
}

/// Panel-13 S1 fix: ETA is REMAINING, not total. `T(c) − elapsed`,
/// clamped at 0. Regression: an early draft used `predicted.wall_secs`
/// directly → forecast pods spawned `T(c)` early, idling for `elapsed`
/// before the dep completed.
// r[verify sched.sla.forecast.one-layer+2]
#[tokio::test]
async fn eta_is_remaining_not_total() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_forecast(db.pool.clone(), 200.0, 2_000);

    // a: T=100, elapsed=40 → eta=60. b: max-across-deps with c
    // (T=50, elapsed=80 → eta=0, clamped). d: dep elapsed > T → 0.
    actor.test_inject_at("a", "x86_64-linux", DerivationStatus::Running);
    actor.test_inject_at("c", "x86_64-linux", DerivationStatus::Running);
    actor.test_inject_at("b", "x86_64-linux", DerivationStatus::Queued);
    actor.test_inject_at("d", "x86_64-linux", DerivationStatus::Queued);
    actor.test_inject_edge("b", "a");
    actor.test_inject_edge("b", "c");
    actor.test_inject_edge("d", "c");
    actor.test_set_running_eta("a", 100.0, 40, 8);
    actor.test_set_running_eta("c", 50.0, 80, 4);

    let snap = actor.compute_spawn_intents(&SpawnIntentsRequest::default());
    let by_id: std::collections::HashMap<_, _> = snap
        .intents
        .iter()
        .map(|i| (i.intent_id.as_str(), i))
        .collect();

    let b = by_id["b"];
    assert!(
        (b.eta_seconds - 60.0).abs() < 2.0,
        "b.eta = max(100-40, max(0, 50-80)) = max(60, 0) = 60, got {}",
        b.eta_seconds
    );
    assert!(b.eta_seconds < 100.0, "remaining, NOT total T(c)");
    let d = by_id["d"];
    assert!(
        d.eta_seconds >= 0.0 && d.eta_seconds < 2.0,
        "d.eta clamped at 0 (elapsed > T), got {}",
        d.eta_seconds
    );
}

// ---------------------------------------------------------------------------
// §13b substitution face (F1 / live_049 lever 3 / WO-S7-R)
// ---------------------------------------------------------------------------

/// **W9-BU (the blind-minute inverse).** A Queued drv whose dep is
/// Ready with a store-ACTIVE (claimable-now) materialization job emits
/// a FORECAST intent carrying the typed substitution prior — the exact
/// population the pre-F1 exclusion silently dropped (dep status Ready
/// → the dep walk killed the parent; the parent's first intent waited
/// for readiness and paid the full node-provisioning lead cold).
///
/// Pre-fix: RED — `warm` emits nothing.
// r[verify sched.sla.forecast.substituting-dep-eta]
#[tokio::test]
async fn forecast_substituting_dep_contributes_typed_eta() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_forecast(db.pool.clone(), 200.0, 2_000);

    // dep(Ready, unclaimed claimable job) ← warm(Queued).
    actor.test_inject_at("dep", "x86_64-linux", DerivationStatus::Ready);
    actor.test_inject_at("warm", "x86_64-linux", DerivationStatus::Queued);
    actor.test_inject_edge("warm", "dep");
    actor.materialization_jobs.insert(
        "dep".into(),
        crate::actor::materialize::JobViewEntry::test_unclaimed(Uuid::new_v4()),
    );

    let snap = actor.compute_spawn_intents(&SpawnIntentsRequest::default());
    let warm = snap
        .intents
        .iter()
        .find(|i| i.intent_id == "warm")
        .expect("dep has an ACTIVE materialization job → warm is forecastable");
    assert_eq!(
        warm.eta_seconds,
        crate::actor::snapshot::SUBSTITUTING_DEP_ETA_PRIOR_SECS,
        "the contribution is the typed prior, exact (no elapsed decay \
         exists — the scheduler retains no claim timestamps)"
    );
    assert_eq!(warm.ready, Some(false), "forecast ⇒ ready=false");
    // The dep itself stays out of BOTH passes (PD-7: substituting
    // work is never builder demand).
    assert!(
        !snap.intents.iter().any(|i| i.intent_id == "dep"),
        "the substituting dep itself is never an intent"
    );
}

/// **W9-BU, claimed face.** A dep under a HELD materialization claim
/// is Assigned/Running with NO fitted curve (cache hits are never
/// builder-dispatched — pull.rs `DispatchShape::Unsized` stamps no
/// `last_intent`), so pre-F1 `running_dep_eta` returned `None` and the
/// parent was killed. Post-F1 the held claim contributes the prior.
/// And when a STALE curve exists (a pre-substitution dispatch), the
/// prior DISPLACES it: the claim is the executing resolution path.
///
/// Pre-fix: RED — `p1` emits nothing; `p2` is lead_horizon-dropped on
/// the stale 500 s curve.
// r[verify sched.sla.forecast.substituting-dep-eta]
#[tokio::test]
async fn forecast_substituting_claimed_dep_uses_prior_not_stale_curve() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_forecast(db.pool.clone(), 200.0, 2_000);

    // d1(Assigned, held claim, no curve) ← p1(Queued).
    actor.test_inject_at("d1", "x86_64-linux", DerivationStatus::Assigned);
    actor.test_inject_at("p1", "x86_64-linux", DerivationStatus::Queued);
    actor.test_inject_edge("p1", "d1");
    actor.materialization_jobs.insert(
        "d1".into(),
        crate::actor::materialize::JobViewEntry::test_unclaimed(Uuid::new_v4()),
    );
    actor
        .materialization_jobs
        .get_mut("d1")
        .unwrap()
        .mint_claim(crate::state::ExecutorId::from("store-0-w0"));

    // d2(Running, held claim, STALE curve eta=500 ≥ lead=200) ← p2.
    actor.test_inject_at("d2", "x86_64-linux", DerivationStatus::Running);
    actor.test_inject_at("p2", "x86_64-linux", DerivationStatus::Queued);
    actor.test_inject_edge("p2", "d2");
    actor.test_set_running_eta("d2", 600.0, 100, 4); // curve eta = 500
    actor.materialization_jobs.insert(
        "d2".into(),
        crate::actor::materialize::JobViewEntry::test_unclaimed(Uuid::new_v4()),
    );
    actor
        .materialization_jobs
        .get_mut("d2")
        .unwrap()
        .mint_claim(crate::state::ExecutorId::from("store-0-w1"));

    let snap = actor.compute_spawn_intents(&SpawnIntentsRequest::default());
    let by_id: std::collections::HashMap<_, _> = snap
        .intents
        .iter()
        .map(|i| (i.intent_id.as_str(), i))
        .collect();
    let p1 = by_id
        .get("p1")
        .expect("held claim, no curve → prior contributes (pre-F1: killed)");
    assert_eq!(
        p1.eta_seconds,
        crate::actor::snapshot::SUBSTITUTING_DEP_ETA_PRIOR_SECS
    );
    let p2 = by_id.get("p2").expect(
        "held claim DISPLACES the stale build curve (500 s ≥ lead would \
         have lead_horizon-dropped p2); the substitution prior governs",
    );
    assert_eq!(
        p2.eta_seconds,
        crate::actor::snapshot::SUBSTITUTING_DEP_ETA_PRIOR_SECS
    );
}

/// An UNCLAIMED job never displaces a live build attempt: a Running
/// dep with a fitted curve AND a claimable (unclaimed) job keeps the
/// progress-grounded curve — the job is the opportunistic sibling,
/// the build is the executing path (PD-20 family).
// r[verify sched.sla.forecast.substituting-dep-eta]
#[tokio::test]
async fn forecast_substituting_unclaimed_job_never_displaces_live_build() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_forecast(db.pool.clone(), 200.0, 2_000);

    // dep(Running, curve eta=30, unclaimed claimable job) ← par.
    actor.test_inject_at("dep", "x86_64-linux", DerivationStatus::Running);
    actor.test_inject_at("par", "x86_64-linux", DerivationStatus::Queued);
    actor.test_inject_edge("par", "dep");
    actor.test_set_running_eta("dep", 100.0, 70, 4); // curve eta = 30
    actor.materialization_jobs.insert(
        "dep".into(),
        crate::actor::materialize::JobViewEntry::test_unclaimed(Uuid::new_v4()),
    );

    let snap = actor.compute_spawn_intents(&SpawnIntentsRequest::default());
    let par = snap
        .intents
        .iter()
        .find(|i| i.intent_id == "par")
        .expect("live build attempt forecasts as before");
    assert!(
        (par.eta_seconds - 30.0).abs() < 2.0,
        "the curve (30 s) governs, not the prior ({} s); got {}",
        crate::actor::snapshot::SUBSTITUTING_DEP_ETA_PRIOR_SECS,
        par.eta_seconds
    );
}

/// **Pacing is a typed, counted exclusion.** A parked or deferred job
/// is not active store work within any lead horizon — the parent is
/// not forecastable, and the drop joins the censused
/// `forecast_dropped_total{reason}` alphabet (debounced once per
/// `(drv, reason)`), instead of vanishing into the silent status kill.
///
/// Pre-fix: RED — no `substituting_pacing` letter exists.
// r[verify sched.sla.forecast.substituting-dep-eta]
#[tokio::test]
async fn forecast_substituting_pacing_dep_drops_typed() {
    use metrics_util::debugging::DebuggingRecorder;

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_forecast(db.pool.clone(), 200.0, 2_000);

    let future = std::time::Instant::now() + std::time::Duration::from_secs(600);
    // d-park(Ready, parked job) ← p-park(Queued).
    actor.test_inject_at("d-park", "x86_64-linux", DerivationStatus::Ready);
    actor.test_inject_at("p-park", "x86_64-linux", DerivationStatus::Queued);
    actor.test_inject_edge("p-park", "d-park");
    actor.materialization_jobs.insert(
        "d-park".into(),
        crate::actor::materialize::JobViewEntry::test_unclaimed(Uuid::new_v4()),
    );
    actor
        .materialization_jobs
        .get_mut("d-park")
        .unwrap()
        .test_set_parked_until(Some(future));
    // d-defer(Ready, deferred job) ← p-defer(Queued).
    actor.test_inject_at("d-defer", "x86_64-linux", DerivationStatus::Ready);
    actor.test_inject_at("p-defer", "x86_64-linux", DerivationStatus::Queued);
    actor.test_inject_edge("p-defer", "d-defer");
    actor.materialization_jobs.insert(
        "d-defer".into(),
        crate::actor::materialize::JobViewEntry::test_unclaimed(Uuid::new_v4()),
    );
    actor
        .materialization_jobs
        .get_mut("d-defer")
        .unwrap()
        .test_set_defer_until(Some(future));

    let rec = DebuggingRecorder::new();
    let snapr = rec.snapshotter();
    let (s1, s2) = {
        let _g = metrics::set_default_local_recorder(&rec);
        let s1 = actor.compute_spawn_intents(&SpawnIntentsRequest::default());
        // Second poll: the per-(drv, reason) debounce holds — no
        // double count (hazard ppppp: snapshot taken ONCE below).
        let s2 = actor.compute_spawn_intents(&SpawnIntentsRequest::default());
        (s1, s2)
    };
    for snap in [&s1, &s2] {
        assert!(
            !snap
                .intents
                .iter()
                .any(|i| i.intent_id == "p-park" || i.intent_id == "p-defer"),
            "pacing (parked/deferred) deps do not contribute — the \
             parent is not forecastable"
        );
    }
    let dropped = crate::sla::metrics::counter_map_by(
        &snapr,
        "rio_scheduler_sla_forecast_dropped_total",
        Some("reason"),
    );
    assert_eq!(
        dropped.get("substituting_pacing"),
        Some(&2),
        "one debounced drop per parent (p-park + p-defer), NOT per \
         poll; got {dropped:?}"
    );
}

/// **W9-BW (the gate inequality, structural).** The substituting-dep
/// prior flows through the SAME pre/post-solve lead-horizon gates as
/// running-dep etas: emitted forecast intents satisfy
/// `eta < cell lead`; a cell whose lead is below the prior keeps
/// dropping (typed, `lead_horizon`) — the prior never bypasses the
/// gate law.
// r[verify sched.sla.forecast.substituting-dep-eta]
#[tokio::test]
async fn forecast_substituting_prior_respects_lead_horizon() {
    use metrics_util::debugging::DebuggingRecorder;

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    // lead 45 < prior 60 → the gate must drop.
    let mut actor = bare_actor_forecast(db.pool.clone(), 45.0, 2_000);

    actor.test_inject_at("dep", "x86_64-linux", DerivationStatus::Ready);
    actor.test_inject_at("warm", "x86_64-linux", DerivationStatus::Queued);
    actor.test_inject_edge("warm", "dep");
    actor.materialization_jobs.insert(
        "dep".into(),
        crate::actor::materialize::JobViewEntry::test_unclaimed(Uuid::new_v4()),
    );

    let rec = DebuggingRecorder::new();
    let snapr = rec.snapshotter();
    let snap = {
        let _g = metrics::set_default_local_recorder(&rec);
        actor.compute_spawn_intents(&SpawnIntentsRequest::default())
    };
    assert!(
        !snap.intents.iter().any(|i| i.intent_id == "warm"),
        "prior (60) ≥ lead (45) → the lead-horizon gate drops the \
         intent — the substitution face never bypasses the gate"
    );
    let dropped = crate::sla::metrics::counter_map_by(
        &snapr,
        "rio_scheduler_sla_forecast_dropped_total",
        Some("reason"),
    );
    assert_eq!(
        dropped.get("lead_horizon"),
        Some(&1),
        "the drop is the gate's own typed letter; got {dropped:?}"
    );

    // Inverse: lead 200 > prior 60 → emitted, eta < lead.
    let mut actor2 = bare_actor_forecast(db.pool.clone(), 200.0, 2_000);
    actor2.test_inject_at("dep", "x86_64-linux", DerivationStatus::Ready);
    actor2.test_inject_at("warm", "x86_64-linux", DerivationStatus::Queued);
    actor2.test_inject_edge("warm", "dep");
    actor2.materialization_jobs.insert(
        "dep".into(),
        crate::actor::materialize::JobViewEntry::test_unclaimed(Uuid::new_v4()),
    );
    let snap2 = actor2.compute_spawn_intents(&SpawnIntentsRequest::default());
    let warm = snap2
        .intents
        .iter()
        .find(|i| i.intent_id == "warm")
        .expect("prior within the horizon → emitted");
    assert!(
        warm.eta_seconds < 200.0,
        "every emitted forecast intent satisfies eta < cell lead"
    );
}

/// **The job-grounded carve-out of the one-layer law.** A Queued dep
/// with an ACTIVE job contributes the prior — substitution resolves
/// the dep directly, independent of its own subtree, so this is direct
/// evidence, not the σ_resid-compounding layer propagation the
/// one-layer cutoff forbids. A Queued dep WITHOUT a job still kills
/// (the pre-F1 law, unchanged).
// r[verify sched.sla.forecast.substituting-dep-eta]
// r[verify sched.sla.forecast.one-layer+2]
#[tokio::test]
async fn forecast_substituting_queued_dep_job_grounded() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_forecast(db.pool.clone(), 200.0, 2_000);

    // dq(Queued, active job) ← pq(Queued): carve-out applies.
    actor.test_inject_at("dq", "x86_64-linux", DerivationStatus::Queued);
    actor.test_inject_at("pq", "x86_64-linux", DerivationStatus::Queued);
    actor.test_inject_edge("pq", "dq");
    actor.materialization_jobs.insert(
        "dq".into(),
        crate::actor::materialize::JobViewEntry::test_unclaimed(Uuid::new_v4()),
    );
    // dn(Queued, NO job) ← pn(Queued): the one-layer kill stands.
    actor.test_inject_at("dn", "x86_64-linux", DerivationStatus::Queued);
    actor.test_inject_at("pn", "x86_64-linux", DerivationStatus::Queued);
    actor.test_inject_edge("pn", "dn");

    let snap = actor.compute_spawn_intents(&SpawnIntentsRequest::default());
    let by_id: std::collections::HashMap<_, _> = snap
        .intents
        .iter()
        .map(|i| (i.intent_id.as_str(), i))
        .collect();
    assert!(
        by_id.contains_key("pq"),
        "Queued dep + active job → job-grounded contribution"
    );
    assert!(
        !by_id.contains_key("pn"),
        "Queued dep without a job → not forecastable (one-layer law)"
    );
    // dq itself is Queued with a pending job → its own intent is
    // excluded by the parent-level PD-7 check, on both passes.
    assert!(!by_id.contains_key("dq"));
}

/// Terminal dep statuses never contribute, job or no job: a
/// Failed/Poisoned/Cancelled dep is a dead end, not progressing work —
/// the parent is not forecastable regardless of the job's armament.
// r[verify sched.sla.forecast.substituting-dep-eta]
#[tokio::test]
async fn forecast_substituting_terminal_dep_never_contributes() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_forecast(db.pool.clone(), 200.0, 2_000);

    actor.test_inject_at("df", "x86_64-linux", DerivationStatus::Failed);
    actor.test_inject_at("pf", "x86_64-linux", DerivationStatus::Queued);
    actor.test_inject_edge("pf", "df");
    actor.materialization_jobs.insert(
        "df".into(),
        crate::actor::materialize::JobViewEntry::test_unclaimed(Uuid::new_v4()),
    );

    let snap = actor.compute_spawn_intents(&SpawnIntentsRequest::default());
    assert!(
        !snap.intents.iter().any(|i| i.intent_id == "pf"),
        "a Failed dep kills forecastability even with an active job"
    );
}

/// **NoView fails closed, uncounted.** With the job view unhydrated
/// (post-failover recovery window) job knowledge is unavailable: the
/// dep walk falls back to the pre-F1 status disposition — a Ready dep
/// kills the parent — and emits NO `substituting_pacing` count
/// (counting would assert job knowledge the arm exists to deny
/// having). Heals at the next successful recovery rebuild.
// r[verify sched.sla.forecast.substituting-dep-eta]
#[tokio::test]
async fn forecast_substituting_no_view_fails_closed() {
    use metrics_util::debugging::DebuggingRecorder;

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    use crate::actor::{DagActor, DagActorConfig, DagActorPlumbing};
    use crate::sla::config::CapacityType;
    let mut sla = test_sla_config();
    sla.lead_time_seed
        .insert(("test-hw".into(), CapacityType::Spot), 200.0);
    sla.max_forecast_cores_per_tenant = 2_000;
    let mut actor = DagActor::new(
        crate::db::SchedulerDb::new(db.pool.clone()),
        DagActorConfig {
            sla,
            ..Default::default()
        },
        DagActorPlumbing {
            start_hydrated_job_view: false,
            ..Default::default()
        },
    );

    actor.test_inject_at("dep", "x86_64-linux", DerivationStatus::Ready);
    actor.test_inject_at("warm", "x86_64-linux", DerivationStatus::Queued);
    actor.test_inject_edge("warm", "dep");

    let rec = DebuggingRecorder::new();
    let snapr = rec.snapshotter();
    let snap = {
        let _g = metrics::set_default_local_recorder(&rec);
        actor.compute_spawn_intents(&SpawnIntentsRequest::default())
    };
    assert!(
        !snap.intents.iter().any(|i| i.intent_id == "warm"),
        "unhydrated view → fail closed to the pre-F1 kill"
    );
    let dropped = crate::sla::metrics::counter_map_by(
        &snapr,
        "rio_scheduler_sla_forecast_dropped_total",
        Some("reason"),
    );
    assert_eq!(
        dropped.get("substituting_pacing"),
        None,
        "NoView is uncounted — no job knowledge is asserted; got {dropped:?}"
    );
}

/// **W9-BV (disposition census).** The `SubstDepEta` mapping is total
/// over the `Claimability` alphabet — the member list comes FROM the
/// alphabet: `expected()` is a wildcard-free match, so a new
/// `Claimability` variant breaks THIS function at compile time and
/// forces the census row (R15: rustc exhaustiveness is the generator).
/// Plus the two non-entry axes: NoJob (hydrated, no entry) and NoView
/// (unhydrated).
// r[verify sched.sla.forecast.substituting-dep-eta]
#[tokio::test]
async fn subst_dep_eta_disposition_census() {
    use crate::actor::materialize::{Claimability, JobViewEntry};
    use crate::actor::snapshot::{SubstActiveFace, SubstDepEta, SubstPacingFace};

    // The alphabet-derived mapping (compile-breaks on new variants).
    fn expected(c: Claimability) -> SubstDepEta {
        match c {
            Claimability::Claimed => SubstDepEta::Active(SubstActiveFace::Claimed),
            Claimability::ClaimableNow => SubstDepEta::Active(SubstActiveFace::ClaimableNow),
            Claimability::Parked => SubstDepEta::Pacing(SubstPacingFace::Parked),
            Claimability::Deferred => SubstDepEta::Pacing(SubstPacingFace::Deferred),
        }
    }

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_forecast(db.pool.clone(), 200.0, 2_000);
    let now = std::time::Instant::now();
    let future = now + std::time::Duration::from_secs(600);

    // One entry per armament state, constructed through the SAME
    // production-shape mutators the armament tests use.
    actor.test_inject_at("j-claimable", "x86_64-linux", DerivationStatus::Ready);
    actor.materialization_jobs.insert(
        "j-claimable".into(),
        JobViewEntry::test_unclaimed(Uuid::new_v4()),
    );
    actor.test_inject_at("j-claimed", "x86_64-linux", DerivationStatus::Ready);
    actor.materialization_jobs.insert(
        "j-claimed".into(),
        JobViewEntry::test_unclaimed(Uuid::new_v4()),
    );
    actor
        .materialization_jobs
        .get_mut("j-claimed")
        .unwrap()
        .mint_claim(crate::state::ExecutorId::from("store-0-w0"));
    actor.test_inject_at("j-parked", "x86_64-linux", DerivationStatus::Ready);
    actor.materialization_jobs.insert(
        "j-parked".into(),
        JobViewEntry::test_unclaimed(Uuid::new_v4()),
    );
    actor
        .materialization_jobs
        .get_mut("j-parked")
        .unwrap()
        .test_set_parked_until(Some(future));
    actor.test_inject_at("j-deferred", "x86_64-linux", DerivationStatus::Ready);
    actor.materialization_jobs.insert(
        "j-deferred".into(),
        JobViewEntry::test_unclaimed(Uuid::new_v4()),
    );
    actor
        .materialization_jobs
        .get_mut("j-deferred")
        .unwrap()
        .test_set_defer_until(Some(future));

    for drv in ["j-claimable", "j-claimed", "j-parked", "j-deferred"] {
        let armament = actor
            .materialization_jobs
            .get(drv)
            .unwrap()
            .claimability(now);
        assert_eq!(
            actor.subst_dep_eta(drv, now),
            expected(armament),
            "{drv}: the disposition derives from the one armament \
             source (bug_170) through the alphabet mapping"
        );
    }
    // The two non-entry axes.
    assert_eq!(
        actor.subst_dep_eta("no-such-job", now),
        SubstDepEta::NoJob,
        "hydrated view, no entry → NoJob"
    );
}

/// §Threat-model gap (d): `max_forecast_cores_per_tenant` debited by
/// Ready cores BEFORE forecast intents are admitted. A tenant whose
/// Ready frontier already consumes ≥ the cap emits zero forecast
/// intents — its layer-2 fanout cannot capture shared `maxFleetCores`
/// ahead of other tenants' Ready work.
// r[verify sched.sla.forecast.tenant-ceiling]
#[tokio::test]
async fn forecast_tenant_ceiling_subtracts_ready_first() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    // probe.cpu=4 → every unfitted intent is 4 cores. cap=10 cores.
    let mut actor = bare_actor_forecast(db.pool.clone(), 200.0, 10);

    // 3 Ready (Σ 12 cores) + 5 Queued forecast-candidates (each dep
    // Running, eta<lead). budget = 10 − 12 = −2 → no forecast.
    for r in ["r0", "r1", "r2"] {
        actor.test_inject_ready(r, None, "x86_64-linux", false);
    }
    for q in ["q0", "q1", "q2", "q3", "q4"] {
        let dep = format!("{q}dep");
        actor.test_inject_at(&dep, "x86_64-linux", DerivationStatus::Running);
        actor.test_inject_at(q, "x86_64-linux", DerivationStatus::Queued);
        actor.test_inject_edge(q, &dep);
        actor.test_set_running_eta(&dep, 50.0, 10, 4);
    }

    let snap = actor.compute_spawn_intents(&SpawnIntentsRequest::default());
    let n_forecast = snap
        .intents
        .iter()
        .filter(|i| i.ready == Some(false))
        .count();
    let n_ready = snap
        .intents
        .iter()
        .filter(|i| i.ready == Some(true))
        .count();
    assert_eq!(n_ready, 3, "Ready unaffected by ceiling");
    assert_eq!(
        n_forecast, 0,
        "Ready Σ12 > cap 10 → forecast budget exhausted before pass runs"
    );

    // Same shape with cap=20: budget = 20 − 12 = 8 → 2 forecast
    // intents (2×4=8) admitted, 3rd (4 > 0) rejected.
    let mut actor = bare_actor_forecast(db.pool.clone(), 200.0, 20);
    for r in ["r0", "r1", "r2"] {
        actor.test_inject_ready(r, None, "x86_64-linux", false);
    }
    for q in ["q0", "q1", "q2", "q3", "q4"] {
        let dep = format!("{q}dep");
        actor.test_inject_at(&dep, "x86_64-linux", DerivationStatus::Running);
        actor.test_inject_at(q, "x86_64-linux", DerivationStatus::Queued);
        actor.test_inject_edge(q, &dep);
        actor.test_set_running_eta(&dep, 50.0, 10, 4);
    }
    let snap = actor.compute_spawn_intents(&SpawnIntentsRequest::default());
    let n_forecast = snap
        .intents
        .iter()
        .filter(|i| i.ready == Some(false))
        .count();
    assert_eq!(
        n_forecast, 2,
        "budget 20−12=8 admits exactly 2×4-core forecast intents"
    );
}

/// bug_025: forecast budget gate is collect → sort → gate, NOT greedy
/// first-fit in `HashMap::iter()` order. Same DAG state must produce
/// the same admitted subset regardless of insertion order; the sort key
/// `(priority, c*) desc, drv_hash asc` means the high-priority drv is
/// always admitted and ties resolve by `drv_hash`.
///
/// All drvs solve to `probe.cpu = 4` cores (unfitted), so the `c*`
/// term is degenerate here — priority + `drv_hash` tiebreak are what's
/// exercised. cap=8 admits exactly two of three.
// r[verify sched.sla.forecast.tenant-ceiling]
#[tokio::test]
async fn forecast_budget_deterministic() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;

    // Build the SAME 3-drv forecast frontier twice with different
    // insertion orders. `iter_nodes()` is HashMap-backed → order is
    // undefined; the assertion is that the admitted subset is
    // identical regardless.
    let build = |order: &[&str]| {
        let mut actor = bare_actor_forecast(db.pool.clone(), 200.0, 8);
        actor.test_inject_at("dep", "x86_64-linux", DerivationStatus::Running);
        actor.test_set_running_eta("dep", 100.0, 70, 8);
        for &h in order {
            actor.test_inject_at(h, "x86_64-linux", DerivationStatus::Queued);
            actor.test_inject_edge(h, "dep");
        }
        actor.test_set_priority("fa", 1000.0);
        actor.test_set_priority("fb", 10.0);
        actor.test_set_priority("fc", 10.0);
        actor
    };

    let admitted = |actor: &DagActor| -> Vec<String> {
        let snap = actor.compute_spawn_intents(&SpawnIntentsRequest::default());
        let mut v: Vec<_> = snap
            .intents
            .into_iter()
            .filter(|i| i.ready == Some(false))
            .map(|i| i.intent_id)
            .collect();
        v.sort();
        v
    };

    let a1 = admitted(&build(&["fa", "fb", "fc"]));
    let a2 = admitted(&build(&["fc", "fb", "fa"]));

    // High-priority `fa` always admitted; between `fb`/`fc` (tied
    // priority + cores), drv_hash asc → `fb`. cap=8, 2×4 cores → `fc`
    // dropped.
    assert_eq!(
        a1,
        vec!["fa", "fb"],
        "prio sort admits fa; drv_hash tiebreak admits fb"
    );
    assert_eq!(
        a1, a2,
        "admitted subset deterministic across DAG insertion order"
    );
}

/// Round-10 merged_bug_006 (the producer law): the demand aggregate is
/// PER POPULATION CLASS. `queued_by_system` counts the Ready class
/// only (its increment sits in the Ready loop); the forecast pass
/// increments `forecast_by_system` at the EMIT site — post tenant-
/// budget admission — so the class covers exactly the forecast
/// intents a controller can hold Pending Jobs for. A budget-dropped
/// candidate spawns no Job and is counted by NEITHER class.
///
/// Population walked: 1 Ready + 2 admitted forecast + 1 budget-dropped
/// forecast (cap=12: Ready debits 4, leaving 8 = 2×4-core slots).
/// Asserts BOTH classes' counts AND the cross-check that the forecast
/// class equals the emitted `ready=Some(false)` population — the
/// boundary the merged_bug_006 reap bound undercounted.
// r[verify ctrl.pool.demand-completeness]
#[tokio::test]
async fn forecast_aggregate_counts_emitted_class_per_system() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_forecast(db.pool.clone(), 200.0, 12);

    actor.test_inject_ready("r0", None, "x86_64-linux", false);
    actor.test_inject_at("dep", "x86_64-linux", DerivationStatus::Running);
    actor.test_set_running_eta("dep", 100.0, 70, 8);
    for q in ["fa", "fb", "fc"] {
        actor.test_inject_at(q, "x86_64-linux", DerivationStatus::Queued);
        actor.test_inject_edge(q, "dep");
    }

    let snap = actor.compute_spawn_intents(&SpawnIntentsRequest::default());
    let emitted_forecast = snap
        .intents
        .iter()
        .filter(|i| i.ready == Some(false))
        .count() as u64;
    assert_eq!(
        emitted_forecast, 2,
        "budget 12−4=8 admits exactly 2×4-core forecast intents"
    );
    assert_eq!(
        snap.queued_by_system.get("x86_64-linux").copied(),
        Some(1),
        "Ready class counts the Ready population only — forecast \
         intents never inflate it"
    );
    assert_eq!(
        snap.forecast_by_system.get("x86_64-linux").copied(),
        Some(emitted_forecast),
        "forecast class == the emitted ready=false population (the \
         emit-site law; the budget-dropped candidate is uncounted)"
    );
    assert_eq!(
        snap.forecast_by_system.len(),
        1,
        "no phantom systems in the forecast class"
    );
}

/// `lead_time_seed` empty → `max_lead = 0` → forecast pass disabled.
/// Deploys without `xtask k8s probe-boot` seeding stay on the v1.0
/// Ready-only path.
// r[verify sched.sla.forecast.one-layer+2]
#[tokio::test]
async fn forecast_disabled_on_empty_lead_time_seed() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_sla(db.pool.clone()); // lead_time_seed = {}

    actor.test_inject_at("a", "x86_64-linux", DerivationStatus::Running);
    actor.test_inject_at("b", "x86_64-linux", DerivationStatus::Queued);
    actor.test_inject_edge("b", "a");
    actor.test_set_running_eta("a", 100.0, 70, 8);

    let snap = actor.compute_spawn_intents(&SpawnIntentsRequest::default());
    assert!(
        snap.intents.iter().all(|i| i.ready == Some(true)),
        "no forecast when lead_time_seed empty"
    );
    assert!(!snap.intents.iter().any(|i| i.intent_id == "b"));
}

/// bug_030 regression: a forecast intent whose dep's
/// `T(c) − elapsed` clamps to 0.0 (overdue) MUST still carry
/// `ready=false` and sort AFTER Ready intents. The previous
/// `eta_seconds == 0.0` discriminator collided with this case — the
/// controller's §13a filter would spawn a Job for a Queued drv whose
/// dep hasn't actually finished, and the scheduler's sort would
/// interleave it with genuine Ready work.
#[tokio::test]
async fn forecast_overdue_dep_is_not_ready() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_forecast(db.pool.clone(), 200.0, 2_000);

    // a: T=50, elapsed=80 → eta clamped to 0.0 (overdue). b depends
    // on a → forecast with eta=0.0 but NOT Ready (a is still Running).
    // r: genuinely Ready.
    actor.test_inject_at("a", "x86_64-linux", DerivationStatus::Running);
    actor.test_inject_at("b", "x86_64-linux", DerivationStatus::Queued);
    actor.test_inject_edge("b", "a");
    actor.test_set_running_eta("a", 50.0, 80, 4);
    actor.test_inject_ready("r", None, "x86_64-linux", false);

    let snap = actor.compute_spawn_intents(&SpawnIntentsRequest::default());
    let by_id: std::collections::HashMap<_, _> = snap
        .intents
        .iter()
        .map(|i| (i.intent_id.as_str(), i))
        .collect();

    let b = by_id.get("b").expect("b emitted as forecast");
    assert!(
        b.eta_seconds >= 0.0 && b.eta_seconds < 2.0,
        "b.eta clamped to ~0.0 (overdue dep), got {}",
        b.eta_seconds
    );
    assert_eq!(
        b.ready,
        Some(false),
        "b.ready=false despite eta=0.0 — forecast loop, dep still Running"
    );
    assert_eq!(by_id["r"].ready, Some(true), "r genuinely Ready");

    // Sort order: r (ready=true) precedes b (ready=false), even though
    // both have eta_seconds==0.0. Under the old `eta == 0.0` comparator
    // their relative order was priority-only (tie → HashMap-random).
    let pos_r = snap
        .intents
        .iter()
        .position(|i| i.intent_id == "r")
        .unwrap();
    let pos_b = snap
        .intents
        .iter()
        .position(|i| i.intent_id == "b")
        .unwrap();
    assert!(
        pos_r < pos_b,
        "Ready sorts before forecast even when both eta=0.0"
    );
}

/// End-to-end actor path: merge → Ready → intent shows up; dispatch →
/// Assigned → intent drops (only Ready emits intents). Also covers
/// `solve_intent_for`'s `deadline_secs` clamp:
/// `min(max(computed, floor.deadline), DEADLINE_CAP_SECS)`.
// r[verify sched.admin.spawn-intents+2]
#[tokio::test]
async fn spawn_intents_end_to_end_and_deadline_clamp() -> TestResult {
    let (_db, handle, _task) = setup_with_big_ceilings().await;

    // Merge 3 single-node DAGs. All three → Ready immediately (no
    // deps). No workers connected yet → all 3 emit intents.
    for tag in ["a", "b", "c"] {
        let _rx = merge_single_node(&handle, Uuid::new_v4(), tag, PriorityClass::Scheduled).await?;
    }

    let snap = handle
        .query_unchecked(|reply| {
            ActorCommand::Admin(AdminQuery::GetSpawnIntents {
                req: SpawnIntentsRequest::default(),
                reply,
            })
        })
        .await?;
    assert_eq!(snap.intents.len(), 3, "three merged-and-ready derivations");
    assert_eq!(snap.queued_by_system.get("x86_64-linux"), Some(&3));
    // O3: deadline_secs = min(max(computed, floor.deadline_secs),
    // DEADLINE_CAP_SECS). Unfitted (no SlaEstimator entry) ⇒
    // computed = `[sla].probe.deadline_secs`; floor=0 (never bumped);
    // cap = 86400. Result == probe default.
    let probe = crate::sla::config::default_probe_deadline_secs();
    for i in &snap.intents {
        assert_eq!(
            i.deadline_secs, probe,
            "unfitted ⇒ deadline_secs == probe default; cap not engaged"
        );
        assert!(i.deadline_secs <= crate::actor::floor::DEADLINE_CAP_SECS);
    }

    // Open a pull attempt for one derivation — it moves to Running
    // and drops out of the intent stream.
    let _assignment = pull_attempt(&handle, "a").await;

    let snap = handle
        .query_unchecked(|reply| {
            ActorCommand::Admin(AdminQuery::GetSpawnIntents {
                req: SpawnIntentsRequest::default(),
                reply,
            })
        })
        .await?;
    assert_eq!(
        snap.intents.len(),
        2,
        "one dispatched → two still Ready (only Ready emits intents)"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// P0539c metrics: mailbox_depth, dispatch_wait_seconds
// ---------------------------------------------------------------------------

// r[verify obs.metric.scheduler+2]
/// Mailbox-depth gauge is set on every dequeued command. Send a Tick,
/// barrier (request-reply, also dequeued), and assert the gauge was
/// touched. Value is non-deterministic (depends on how many commands
/// were queued at sample time) — touch-set assertion only.
#[tokio::test]
async fn test_mailbox_depth_gauge_set_per_command() -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (handle, _task) = setup_actor(db.pool.clone());

    handle.send_unchecked(ActorCommand::Tick).await?;
    barrier(&handle).await;

    assert!(
        recorder.gauge_touched("rio_scheduler_actor_mailbox_depth"),
        "mailbox_depth gauge not set after dequeuing commands.\n\
         Gauges touched: {:?}",
        recorder.gauge_names()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// clear_persisted_state: per-generation maps
// ---------------------------------------------------------------------------

/// `clear_persisted_state` clears every per-generation map. The
/// destructure in the body makes a missed field a compile error; this
/// test makes a new cleared-binding that forgets its `.clear()` a
/// runtime error. Same-process lose→reacquire would otherwise carry
/// stale state into the new generation.
#[tokio::test]
async fn clear_persisted_state_clears_per_generation_maps() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor(db.pool.clone());

    actor.authoritative_binding.insert(
        "stale-drv".into(),
        crate::actor::AuthBinding {
            node: "nA".into(),
            tenant: None,
            deadline_secs: None,
        },
    );

    actor.clear_persisted_state();

    assert!(
        actor.authoritative_binding.is_empty(),
        "authoritative_binding (controller-reported per-generation) must be cleared"
    );
    // Regression: soft_features survives (existing :649 invariant).
    actor.test_inject_ready_with_features("ff", None, "x86_64-linux", &["big-parallel"]);
}

// ---------------------------------------------------------------------------
// BuildEventBus: display-only ring routing
// ---------------------------------------------------------------------------

/// `Event::SubstituteProgress` is display-only: it MUST be routed to the
/// log ring, never the state ring, so display volume cannot evict
/// state-transition events (`r[gw.activity.stop-parity]`).
#[tokio::test]
async fn display_only_events_route_to_log_ring() {
    use crate::actor::event::BuildEventBus;
    use rio_proto::types::build_event::Event;
    let mut bus = BuildEventBus::new();
    let build_id = Uuid::new_v4();
    let mut rx = bus.register(build_id);

    bus.emit(
        build_id,
        Event::SubstituteProgress(rio_proto::types::SubstituteProgress::default()),
    );
    bus.emit(
        build_id,
        Event::Derivation(rio_proto::types::DerivationEvent::default()),
    );

    // State ring sees ONLY the Derivation event.
    let state_ev = rx.state.try_recv().expect("state event");
    assert!(
        matches!(state_ev.event, Some(Event::Derivation(_))),
        "state ring must carry the Derivation event, got {:?}",
        state_ev.event
    );
    assert!(
        rx.state.try_recv().is_err(),
        "SubstituteProgress must NOT be on the state ring"
    );
    // Log ring sees ONLY the SubstituteProgress.
    let log_ev = rx.log.try_recv().expect("log event");
    assert!(
        matches!(log_ev.event, Some(Event::SubstituteProgress(_))),
        "log ring must carry the SubstituteProgress event, got {:?}",
        log_ev.event
    );
}

// ---------------------------------------------------------------------------
// handle_watch_build / build_options_for_derivation regressions
// ---------------------------------------------------------------------------

/// `handle_watch_build` on a missing build with a tenant caller MUST
/// return `BuildNotFound`, matching `handle_cancel_build` /
/// `handle_query_build_status`. Pre-fix it collapsed lookup+tenant-
/// check into one expression: `builds.get(b).map(|b| b.tenant_id) !=
/// Some(caller)` is `true` when the lookup is `None`, so a tenant
/// caller got `PermissionDenied` while an admin caller (`None`) got
/// `BuildNotFound` for the SAME missing build.
// r[verify sched.tenant.authz+3]
#[tokio::test]
async fn watch_build_missing_returns_not_found_for_tenant() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let actor = bare_actor(db.pool.clone());
    let missing = Uuid::new_v4();
    let tenant = Some(Uuid::new_v4());

    let watch = actor.handle_watch_build(missing, tenant);
    assert!(
        matches!(watch, Err(ActorError::BuildNotFound(b)) if b == missing),
        "tenant caller on missing build → BuildNotFound, got {:?}",
        watch.as_ref().err()
    );
    // Sibling agreement: same input → same error variant.
    let query = actor.handle_query_build_status(missing, tenant);
    assert!(
        matches!(query, Err(ActorError::BuildNotFound(_))),
        "siblings already return BuildNotFound"
    );
    // Admin caller on missing build → BuildNotFound (unchanged).
    assert!(matches!(
        actor.handle_watch_build(missing, None),
        Err(ActorError::BuildNotFound(_))
    ));
}

/// `build_options_for_derivation` build_cores merge: per
/// build_types.proto:307, `build_cores=0` means "all" — the MOST
/// permissive value. Pre-fix `.max()` made `max(0,4)=4`, so a client
/// requesting 0=all lost to any positive value, contradicting the
/// "more permissive wins" comment.
#[tokio::test]
async fn build_options_merge_zero_cores_is_all() {
    use crate::state::BuildInfo;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor(db.pool.clone());
    actor.test_inject_ready("h", None, "x86_64-linux", false);

    let mut mk = |cores: u64| {
        let bid = Uuid::new_v4();
        let info = BuildInfo::new_pending(
            bid,
            None,
            PriorityClass::Scheduled,
            false,
            BuildOptions {
                build_cores: cores,
                ..Default::default()
            },
            std::iter::once(DrvHash::from("h")).collect(),
        );
        actor.builds.insert(bid, info);
        actor
            .dag
            .node_mut("h")
            .unwrap()
            .interested_builds
            .insert(bid);
    };
    mk(4);
    mk(0);

    let opts = actor.build_options_for_derivation(&DrvHash::from("h"));
    assert_eq!(
        opts.build_cores, 0,
        "0 = all = most permissive, sticky once any interested build sets it"
    );

    // Positive-only merge still picks max.
    let mut actor = bare_actor(db.pool.clone());
    actor.test_inject_ready("h2", None, "x86_64-linux", false);
    let mut mk2 = |cores: u64| {
        let bid = Uuid::new_v4();
        actor.builds.insert(
            bid,
            BuildInfo::new_pending(
                bid,
                None,
                PriorityClass::Scheduled,
                false,
                BuildOptions {
                    build_cores: cores,
                    ..Default::default()
                },
                std::iter::once(DrvHash::from("h2")).collect(),
            ),
        );
        actor
            .dag
            .node_mut("h2")
            .unwrap()
            .interested_builds
            .insert(bid);
    };
    mk2(4);
    mk2(8);
    let opts = actor.build_options_for_derivation(&DrvHash::from("h2"));
    assert_eq!(opts.build_cores, 8, "all-positive → max");
}

// r[verify sched.timeout.per-build+2]
/// merged_bug_034 red: ONE interested build carrying `u64::MAX` wire
/// timeouts must not launder them onto the assignment wire — pre-fix
/// `min_nonzero` over a single element preserved `u64::MAX` verbatim
/// (min over one element is the element), and the folded value rode
/// the assignment proto into the builder's `Instant + Duration`
/// deadline math. Proposition certified (R16): the scheduler fold's
/// output is ceiling-bounded for ANY tenant input, because the seam
/// mint saturates and the fold preserves the bound (the typed
/// `WireSecs` field makes an unclamped operand unrepresentable).
#[tokio::test]
async fn sole_build_max_options_fold_bounded() {
    use crate::state::BuildInfo;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor(db.pool.clone());
    actor.test_inject_ready("h3", None, "x86_64-linux", false);

    let bid = Uuid::new_v4();
    let info = BuildInfo::new_pending(
        bid,
        None,
        PriorityClass::Scheduled,
        false,
        BuildOptions {
            // The tenant-seam mint (scheduler_service.rs) on a
            // u64::MAX SubmitBuildRequest — the saturating
            // constructor is the production ingestion path.
            max_silent_time: rio_common::clamped::WireSecs::from_wire(u64::MAX),
            build_timeout: rio_common::clamped::WireSecs::from_wire(u64::MAX),
            build_cores: 1,
        },
        std::iter::once(DrvHash::from("h3")).collect(),
    );
    actor.builds.insert(bid, info);
    actor
        .dag
        .node_mut("h3")
        .unwrap()
        .interested_builds
        .insert(bid);

    let opts = actor.build_options_for_derivation(&DrvHash::from("h3"));
    let ceiling = rio_common::clamped::ClampedSecs::MAX_SECS as u64;
    assert!(
        opts.build_timeout <= ceiling && opts.build_timeout > 0,
        "sole-build fold must emit the saturated ceiling, not u64::MAX; \
         got {}",
        opts.build_timeout
    );
    assert!(
        opts.max_silent_time <= ceiling && opts.max_silent_time > 0,
        "sole-build fold must emit the saturated ceiling, not u64::MAX; \
         got {}",
        opts.max_silent_time
    );
}

// ---------------------------------------------------------------------------
// Attempt-ledger GC tick (sched.db.attempts-gc)
// ---------------------------------------------------------------------------

// r[verify sched.db.attempts-gc]
/// The attempt-ledger sweep tick: driven directly on the GC multiple
/// (the `maybe_refresh_estimator` direct-drive precedent) it deletes
/// exactly the eligible rows; a standby's `handle_tick` never reaches
/// it — the standby-tick-noop early-return precedes every sweep, so a
/// deposed leader cannot delete ledger rows.
#[tokio::test]
async fn test_attempt_ledger_gc_tick_leader_sweeps_standby_noops() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;

    // Seed one derivation with [old pre-reset attempt, old reset,
    // fresh attempt].
    let drv_id = {
        let mut tx = db.pool.begin().await?;
        let row = crate::db::DerivationRow {
            drv_hash: "ledger-gc-smoke".into(),
            drv_path: rio_test_support::fixtures::test_drv_path("ledger-gc-smoke"),
            pname: Some("test-pkg".into()),
            system: "x86_64-linux".into(),
            status: crate::state::DerivationStatus::Created,
            required_features: vec![],
            expected_output_paths: vec![],
            output_names: vec!["out".into()],
            is_fixed_output: false,
            is_ca: false,
        };
        let ids = crate::db::SchedulerDb::batch_upsert_derivations(&mut tx, &[row]).await?;
        tx.commit().await?;
        ids.get("ledger-gc-smoke").expect("just inserted").0
    };
    {
        use crate::db::attempts::AttemptRow;
        use crate::state::{OutcomeClass, ReportingParty};
        let a1 = AttemptRow::new(
            drv_id,
            OutcomeClass::Transient,
            ReportingParty::Worker,
            crate::state::AttemptKind::Build,
        );
        let r1 = AttemptRow::new_reset(
            drv_id,
            OutcomeClass::ResubmitReset,
            ReportingParty::Scheduler,
            1,
            crate::state::AttemptKind::Build,
        );
        let a2 = AttemptRow::new(
            drv_id,
            OutcomeClass::Transient,
            ReportingParty::Worker,
            crate::state::AttemptKind::Build,
        );
        let old = [a1.attempt_id, r1.attempt_id];
        let mut tx = db.pool.begin().await?;
        crate::db::SchedulerDb::append_attempts_batch(&mut tx, &[a1, r1, a2]).await?;
        tx.commit().await?;
        sqlx::query(
            "UPDATE drv_attempts SET recorded_at = recorded_at - interval '3 days' \
             WHERE attempt_id = ANY($1)",
        )
        .bind(&old[..])
        .execute(&db.pool)
        .await?;
    }
    let count = || async {
        let (n,): (i64,) =
            sqlx::query_as("SELECT count(*) FROM drv_attempts WHERE derivation_id = $1")
                .bind(drv_id)
                .fetch_one(&db.pool)
                .await
                .expect("count query");
        n
    };
    assert_eq!(count().await, 3);

    // Standby half: handle_tick early-returns before any sweep, even
    // primed one tick short of the GC multiple. The default test
    // plumbing is a leader, so build the standby LeaderState explicitly
    // (the spawn_actor_with_leader pattern above).
    let standby_leader = crate::lease::LeaderState::from_parts(
        Arc::new(std::sync::atomic::AtomicU64::new(1)),
        Arc::new(AtomicBool::new(false)),
        true,
    );
    let mut standby = DagActor::new(
        crate::db::SchedulerDb::new(db.pool.clone()),
        DagActorConfig::default(),
        DagActorPlumbing {
            leader: standby_leader,
            ..Default::default()
        },
    );
    assert!(
        !standby.leader.is_leader(),
        "constructed standby must not lead"
    );
    standby.tick_count = 29;
    standby.handle_tick().await;
    assert_eq!(count().await, 3, "standby tick must not delete ledger rows");

    // Leader-path half: the sweep tick driven directly on the GC
    // multiple deletes exactly the pre-reset old attempt row.
    let leader_actor = {
        let mut a = bare_actor(db.pool.clone());
        a.tick_count = 30;
        a
    };
    leader_actor
        .tick_gc_attempt_ledger(
            &leader_actor
                .dag_authority()
                .expect("direct-setup actor is authoritative"),
        )
        .await;
    assert_eq!(
        count().await,
        2,
        "exactly the pre-reset old attempt row swept; reset + fresh rows survive"
    );
    Ok(())
}

// r[verify sched.materialize.claimability-projection+1]
// r[verify sched.materialize.claim-coherence]
/// bug_170: the four-way [`Claimability`] precedence grid — the ONE
/// law admission, the KEDA gauge, and the leader listing read. A held
/// claim dominates everything; the durable park dominates the
/// view-only deferral; an expired axis does not count. The raw fields
/// are private, so no consumer can recombine them differently — this
/// grid is the law's full truth table.
#[test]
fn claimability_precedence_grid() {
    use crate::actor::materialize::{Claimability, JobViewEntry};
    let now = std::time::Instant::now();
    let future = now + std::time::Duration::from_secs(60);
    let past = now - std::time::Duration::from_secs(60);

    let mut e = JobViewEntry::test_unclaimed(Uuid::new_v4());
    assert_eq!(e.claimability(now), Claimability::ClaimableNow);

    // Deferral alone.
    e.test_set_defer_until(Some(future));
    assert_eq!(e.claimability(now), Claimability::Deferred);
    // Park dominates deferral.
    e.test_set_parked_until(Some(future));
    assert_eq!(e.claimability(now), Claimability::Parked);
    // Claim dominates both.
    e.mint_claim(crate::state::ExecutorId::from("store-0-w0"));
    assert_eq!(e.claimability(now), Claimability::Claimed);

    // Release through the compare-and-clear law: a stale holder
    // clears nothing; the true holder releases.
    use crate::actor::materialize::ClaimRelease;
    assert_eq!(
        e.release_claim_if_held(&crate::state::ExecutorId::from("store-1-w0")),
        ClaimRelease::StaleHolder
    );
    assert_eq!(e.claimability(now), Claimability::Claimed);
    assert_eq!(
        e.release_claim_if_held(&crate::state::ExecutorId::from("store-0-w0")),
        ClaimRelease::Released
    );
    assert_eq!(e.claimability(now), Claimability::Parked);
    assert_eq!(
        e.release_claim_if_held(&crate::state::ExecutorId::from("store-0-w0")),
        ClaimRelease::Unclaimed,
        "idempotent re-release"
    );

    // Expired axes do not count.
    e.test_set_parked_until(Some(past));
    assert_eq!(e.claimability(now), Claimability::Deferred);
    e.test_set_defer_until(Some(past));
    assert_eq!(e.claimability(now), Claimability::ClaimableNow);
}

// r[verify obs.metric.scheduler+2]
/// bug_282: the width-observability chokepoint routes each event class
/// to ITS OWN counter (and its own warn latch). Pre-fix RED: the
/// zero-width materialization re-arm called the saturation noter with
/// a nil build id — the WRONG counter moved
/// (`rio_scheduler_wanted_width_saturated_total: 1` for a
/// no-verifiable-set event, captured below via the strawman) and the
/// single shared latch let that call suppress a genuine DQ-2 warn (and
/// its real build id) in the same 10s window. The typed `WidthEvent`
/// makes the wrong-class increment unrepresentable.
#[test]
fn width_events_route_to_their_own_counters() {
    use crate::sla::metrics::counter_map;
    use metrics_util::debugging::DebuggingRecorder;

    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();
    {
        let _g = metrics::set_default_local_recorder(&rec);
        let settled = crate::actor::materialize::SettledClose::test_witness();
        crate::state::note_width_event(crate::state::WidthEvent::NoVerifiableSet {
            exec_id: Uuid::new_v4(),
            settled: &settled,
        });
        drop(settled);
    }
    let counters = counter_map(&snap);
    assert_eq!(
        counters
            .get("rio_scheduler_materialization_no_verifiable_wanted_total")
            .copied(),
        Some(1),
        "the zero-width event moves ITS counter"
    );
    assert_eq!(
        counters
            .get("rio_scheduler_wanted_width_saturated_total")
            .copied(),
        None,
        "the zero-width event must NOT move the DQ-2 saturation counter"
    );

    {
        let _g = metrics::set_default_local_recorder(&rec);
        crate::state::note_width_event(crate::state::WidthEvent::SaturatedToDeclared {
            build_id: Uuid::new_v4(),
        });
    }
    let counters = counter_map(&snap);
    assert_eq!(
        counters
            .get("rio_scheduler_wanted_width_saturated_total")
            .copied(),
        Some(1),
        "the saturation event moves the DQ-2 counter"
    );
    assert_eq!(
        counters
            .get("rio_scheduler_materialization_no_verifiable_wanted_total")
            .copied()
            .unwrap_or(0),
        0,
        "…and leaves the zero-width counter untouched (snapshots drain — \
         this is the delta since the first read)"
    );
}

// ---------------------------------------------------------------------------
// Status-outbox replay precedence (merged_bug_025)
// ---------------------------------------------------------------------------

/// merged_bug_025 red A: the status-outbox flush latches NON-terminal
/// batches too (Ready from promote/dispatch, Queued from merge). The
/// absent-node arm KEEPs every DAG-absent drv on the "the latched
/// terminal status IS the node's last truth" justification — which
/// only holds for terminal latches. A latched Ready batch that
/// outlives the node's direct successful Completed persist and
/// terminal-cleanup reap permanently regresses the durable row
/// (completed -> ready): wrong admin surfaces, a retention/GC row
/// leak, and a spurious ready node on the next recovery.
#[tokio::test]
async fn absent_node_nonterminal_latch_must_not_regress_durable_status() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    // The durable truth: the node completed and was reaped from the
    // DAG by terminal cleanup.
    sqlx::query(
        "INSERT INTO derivations (drv_hash, drv_path, pname, system, status) \
         VALUES ($1, $2, 'pkg', 'x86_64-linux', 'completed')",
    )
    .bind("outbox-regress")
    .bind(test_drv_path("outbox-regress"))
    .execute(&db.pool)
    .await?;

    let mut actor = bare_actor(db.pool.clone());
    // A stale NON-terminal latch for the (now DAG-absent) node.
    actor.status_outbox.push_back(crate::actor::StatusBatch {
        drv_hashes: vec!["outbox-regress".into()],
        status: DerivationStatus::Ready,
        exec_ids: vec![],
        enqueued_at: std::time::Instant::now(),
        latched_at_epoch: crate::db::attempts::epoch_now(),
    });
    let authority = actor.dag_authority().expect("always-leader test actor");
    actor.tick_flush_status_outbox(&authority).await;

    let (status,): (String,) =
        sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = 'outbox-regress'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        status, "completed",
        "left: {status} / right: completed (an absent-node NON-terminal \
         latch must be dropped, never replayed over newer durable truth)"
    );
    Ok(())
}

/// merged_bug_017 red R2, actor-side rider (the db half records the
/// named-set red verbatim in db/tests/derivations.rs): a replay the
/// precedence conjunct refuses row-locally must surface LOUDLY at the
/// flusher — the named refusal warn — while the durable row stays
/// unregressed and the batch still POPS (the refusal is the precedence
/// law's FINAL verdict: the durable row is newer; re-pushing would
/// retry forever against it). Pre-fix neither the counter nor the
/// warn existed — the only observable was the info "batch flushed".
#[tokio::test]
async fn refused_replay_pops_batch_and_ticks_counter() -> TestResult {
    use crate::sla::metrics::counter_map;
    use metrics_util::debugging::DebuggingRecorder;

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    // Durable truth: the node advanced (Running, fresh PG stamp)
    // AFTER the latch below; it is DAG-absent so the terminal-KEEP
    // arm keeps it in the batch and only the PG-domain conjunct can
    // refuse it.
    sqlx::query(
        "INSERT INTO derivations (drv_hash, drv_path, pname, system, status) \
         VALUES ($1, $2, 'pkg', 'x86_64-linux', 'running')",
    )
    .bind("outbox-refused")
    .bind(test_drv_path("outbox-refused"))
    .execute(&db.pool)
    .await?;

    let mut actor = bare_actor(db.pool.clone());
    // A terminal latch 60s OLDER than the row's PG stamp (the
    // monotonic enqueue anchor carries the age; the epoch field is
    // diagnostic only).
    actor.status_outbox.push_back(crate::actor::StatusBatch {
        drv_hashes: vec!["outbox-refused".into()],
        status: DerivationStatus::Cancelled,
        exec_ids: vec![],
        enqueued_at: std::time::Instant::now() - std::time::Duration::from_secs(60),
        latched_at_epoch: crate::db::attempts::epoch_now() - 60.0,
    });
    let authority = actor.dag_authority().expect("always-leader test actor");
    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();
    {
        let _g = metrics::set_default_local_recorder(&rec);
        actor.tick_flush_status_outbox(&authority).await;
    }

    // The durable row stands.
    let (status,): (String,) =
        sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = 'outbox-refused'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        status, "running",
        "left: {status} / right: running (the refused replay must not \
         regress the newer durable row)"
    );
    // The batch popped — refusal is final, not a retry lane.
    assert_eq!(
        actor.status_outbox.len(),
        0,
        "the refused batch must pop (re-pushing would retry forever \
         against the newer durable row)"
    );
    // The refusal is counted.
    let counters = counter_map(&snap);
    assert_eq!(
        counters
            .get("rio_scheduler_status_outbox_replay_refused_total")
            .copied(),
        Some(1),
        "left: None (pre-fix: no refusal surface — the only observable \
         was the info \"batch flushed\") / right: Some(1)"
    );
    Ok(())
}

/// merged_bug_285: the split-release wedge — node Assigned/Running,
/// NO open assignment of either kind, job Pending-unclaimed (the
/// crash/flip window between the fenced close commit and the dropped
/// requeue companion). Recovery excludes Assigned/Running and the
/// establishment sweep sees only open attempts, so pre-fix this state
/// answered NotYetReady to every identity FOREVER (the arm counted a
/// skew metric and debug-asserted, repairing nothing). The repair is
/// two-strike (one-sweep insurance for the mint-mid-window race) and
/// uncharged: sweep 1 arms, sweep 2 requeues the node to dep-derived
/// status and the job returns to claimable.
#[tokio::test]
async fn split_release_wedge_repairs_on_second_sweep() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor(db.pool.clone());
    actor.test_inject_ready_row(crate::db::RecoveryDerivationRow::test_default(
        "wedge-drv",
        "x86_64-linux",
    ));
    actor
        .dag
        .node_mut("wedge-drv")
        .expect("just injected")
        .set_status_for_test(DerivationStatus::Assigned);
    actor.materialization_jobs.insert(
        DrvHash::from("wedge-drv"),
        crate::actor::materialize::JobViewEntry::test_unclaimed(Uuid::new_v4()),
    );
    let authority = actor.dag_authority().expect("always-leader test actor");

    // Sweep 1: arms the strike only (one-sweep insurance — an attempt
    // minted between the rows snapshot and the iteration must get one
    // full sweep to appear).
    actor.tick_sweep_open_pull_attempts(&authority).await;
    assert_eq!(
        actor.dag.node("wedge-drv").unwrap().status(),
        DerivationStatus::Assigned,
        "first wedged observation must not repair (one-sweep insurance)"
    );

    // Sweep 2: repairs — the node resets to its dep-derived status and
    // the pending job is claimable again.
    actor.tick_sweep_open_pull_attempts(&authority).await;
    let status = actor.dag.node("wedge-drv").unwrap().status();
    assert_eq!(
        status,
        DerivationStatus::Ready,
        "left: {status:?} / right: Ready (second wedged sweep must \
         requeue the stranded node uncharged)"
    );
    Ok(())
}

/// merged_bug_014: the wedge strike must NOT survive a claim
/// interlude. The two-strike repair is insurance against the
/// documented ONE-PASS snapshot race (a benign mint between the rows
/// snapshot and the view iteration) — two FRESH CONSECUTIVE wedged
/// observations, not two observations separated by a whole claim
/// episode. Pre-fix, only the sweep wrote the wedge strike (the claim
/// mutators reset just the sibling ghost strike), so a strike armed
/// before a claim froze across mint+release and the FIRST
/// post-interlude wedged sweep fired the uncharged requeue against a
/// node whose claim episode just legitimately ended — with the false
/// "two sweeps" warn and a spurious skew tick. Strikes here are armed
/// only by RUNNING the real sweep; the interlude transitions through
/// the production claim mutators (never strike setters).
#[tokio::test]
async fn wedge_strike_does_not_survive_claim_interlude() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor(db.pool.clone());
    actor.test_inject_ready_row(crate::db::RecoveryDerivationRow::test_default(
        "interlude-drv",
        "x86_64-linux",
    ));
    actor
        .dag
        .node_mut("interlude-drv")
        .expect("just injected")
        .set_status_for_test(DerivationStatus::Assigned);
    actor.materialization_jobs.insert(
        DrvHash::from("interlude-drv"),
        crate::actor::materialize::JobViewEntry::test_unclaimed(Uuid::new_v4()),
    );
    let authority = actor.dag_authority().expect("always-leader test actor");

    // Sweep 1: wedged observation arms the strike (no repair).
    actor.tick_sweep_open_pull_attempts(&authority).await;
    assert_eq!(
        actor.dag.node("interlude-drv").unwrap().status(),
        DerivationStatus::Assigned,
        "first wedged observation must not repair"
    );

    // The interlude: a claim episode opens and closes through the
    // production transitions.
    let holder = crate::state::ExecutorId::from("store-itl-w0");
    {
        let entry = actor
            .materialization_jobs
            .get_mut(&DrvHash::from("interlude-drv"))
            .expect("entry inserted above");
        entry.mint_claim(holder.clone());
        assert_eq!(
            entry.release_claim_if_held(&holder),
            crate::actor::materialize::ClaimRelease::Released,
        );
    }

    // Sweep 2: the FIRST wedged observation of the FRESH post-claim
    // episode. A repair here acts on stale cross-episode evidence.
    actor.tick_sweep_open_pull_attempts(&authority).await;
    let status = actor.dag.node("interlude-drv").unwrap().status();
    assert_eq!(
        status,
        DerivationStatus::Assigned,
        "left: repair fired (requeue + \"two sweeps\" warn + skew tick) \
         after ONE post-interlude wedged sweep / right: no repair — a \
         fresh episode requires two FRESH consecutive wedged observations \
         (got {status:?})"
    );

    // Sweep 3: the second FRESH consecutive wedged observation — the
    // repair lane itself is intact and fires now.
    actor.tick_sweep_open_pull_attempts(&authority).await;
    assert_eq!(
        actor.dag.node("interlude-drv").unwrap().status(),
        DerivationStatus::Ready,
        "two fresh consecutive wedged sweeps must still repair"
    );
    Ok(())
}

// r[verify sched.attempt.cancel-close-driven+3]
/// merged_bug_004 hole 1 red: two terminal batches for ONE reaped drv
/// (DependencyFailed latched first, Cancelled latched later — the
/// newer truth; Failed is non-terminal in this status alphabet, so
/// the DAG-absent terminal pair is dep-failed/cancelled).
/// FIFO replays the older first; its own stamp then refuses the newer
/// batch, INVERTING the durable row while the warn claims "the newer
/// rows stand". PROPOSITION CERTIFIED: per-drv supersession at the
/// latch chokepoint keeps at most ONE pending status per drv
/// queue-wide — the newest latched transition is the only truth worth
/// replaying — so the durable row lands on the newer truth and the
/// older batch still closes its latched exec rows (the close is
/// exec-scoped and unconditional, bug_158).
#[tokio::test]
async fn outbox_same_drv_newer_terminal_latch_supersedes_older() -> TestResult {
    use crate::sla::metrics::counter_map;
    use metrics_util::debugging::DebuggingRecorder;

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    // The durable row's last status event predates both latches.
    sqlx::query(
        "INSERT INTO derivations (drv_hash, drv_path, pname, system, status) \
         VALUES ($1, $2, 'pkg', 'x86_64-linux', 'running')",
    )
    .bind("outbox-superseded")
    .bind(test_drv_path("outbox-superseded"))
    .execute(&db.pool)
    .await?;
    sqlx::query(
        "UPDATE derivations SET \
           updated_at = now() - interval '120 seconds', \
           status_changed_at = now() - interval '120 seconds' \
         WHERE drv_hash = 'outbox-superseded'",
    )
    .execute(&db.pool)
    .await?;
    // The older batch's latched exec row (its attempt ended at latch
    // time and must close whatever happens to the drv UPDATE).
    let (derivation_id,): (Uuid,) =
        sqlx::query_as("SELECT derivation_id FROM derivations WHERE drv_hash = $1")
            .bind("outbox-superseded")
            .fetch_one(&db.pool)
            .await?;
    let older_exec = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO assignments \
             (derivation_id, builder_id, generation, status, exec_id) \
         VALUES ($1, 'builder-0', 1, 'acknowledged', $2)",
    )
    .bind(derivation_id)
    .bind(older_exec)
    .execute(&db.pool)
    .await?;

    let mut actor = bare_actor(db.pool.clone());
    // Failed latched at T1 (60s ago), Cancelled at T2 (30s ago) — the
    // node is DAG-absent (reaped), so the terminal-KEEP arm keeps
    // both. Latched through the production chokepoint.
    actor.latch_status_batch(crate::actor::StatusBatch {
        drv_hashes: vec!["outbox-superseded".into()],
        status: DerivationStatus::DependencyFailed,
        exec_ids: vec![older_exec],
        enqueued_at: std::time::Instant::now() - std::time::Duration::from_secs(60),
        latched_at_epoch: crate::db::attempts::epoch_now() - 60.0,
    });
    actor.latch_status_batch(crate::actor::StatusBatch {
        drv_hashes: vec!["outbox-superseded".into()],
        status: DerivationStatus::Cancelled,
        exec_ids: vec![],
        enqueued_at: std::time::Instant::now() - std::time::Duration::from_secs(30),
        latched_at_epoch: crate::db::attempts::epoch_now() - 30.0,
    });
    // Supersession invariant: at most one pending status per drv
    // queue-wide — the older batch no longer carries the drv.
    let carriers = actor
        .status_outbox
        .iter()
        .filter(|b| b.drv_hashes.iter().any(|h| h == "outbox-superseded"))
        .count();
    assert_eq!(
        carriers, 1,
        "left: {carriers} / right: 1 (per-drv supersession at the \
         latch chokepoint: the newest latch is the only carrier)"
    );

    let authority = actor.dag_authority().expect("always-leader test actor");
    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();
    {
        let _g = metrics::set_default_local_recorder(&rec);
        actor.tick_flush_status_outbox(&authority).await;
    }

    // The durable row lands on the NEWER truth.
    let (status,): (String,) =
        sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = 'outbox-superseded'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        status, "cancelled",
        "left: {status} / right: cancelled (the older same-drv replay's \
         own stamp must not refuse the newer terminal truth)"
    );
    // The older batch's latched exec row still closed (close-only
    // flush of the emptied batch).
    let (exec_status,): (String,) =
        sqlx::query_as("SELECT status FROM assignments WHERE exec_id = $1")
            .bind(older_exec)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        exec_status, "failed",
        "left: {exec_status} / right: failed (the emptied older batch \
         still flushes close-only for its latched exec_ids, with the \
         OLDER batch's terminal close mapping)"
    );
    // No false refusal fired: the only counted refusals would be the
    // inversion's lie.
    let counters = counter_map(&snap);
    assert_eq!(
        counters
            .get("rio_scheduler_status_outbox_replay_refused_total")
            .copied()
            .unwrap_or(0),
        0,
        "left: 1 (pre-fix: the older batch's own stamp refused the \
         newer truth and counted it as foreign precedence) / right: 0"
    );
    assert_eq!(actor.status_outbox.len(), 0, "both batches drain");
    Ok(())
}

// r[verify sched.attempt.cancel-close-driven+3]
/// merged_bug_108 case 2 red: an applied-but-ack-lost replay. The
/// first flush COMMITS; the Err arm re-pushes the batch (pop-return);
/// the next tick's kept set re-derives identically (memory never
/// changed) and the replay's own previous stamp zero-rows the batch.
/// PROPOSITION CERTIFIED: a zero-row residual whose durable status
/// EQUALS the latched truth is classified already-applied at the
/// durability point — never warned/counted as foreign precedence —
/// exactly during the PG brownout the outbox exists for.
#[tokio::test]
async fn outbox_replay_lost_ack_residual_classified_already_applied() -> TestResult {
    use crate::sla::metrics::counter_map;
    use metrics_util::debugging::DebuggingRecorder;

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    sqlx::query(
        "INSERT INTO derivations (drv_hash, drv_path, pname, system, status) \
         VALUES ($1, $2, 'pkg', 'x86_64-linux', 'running')",
    )
    .bind("outbox-lost-ack")
    .bind(test_drv_path("outbox-lost-ack"))
    .execute(&db.pool)
    .await?;
    sqlx::query(
        "UPDATE derivations SET \
           updated_at = now() - interval '120 seconds', \
           status_changed_at = now() - interval '120 seconds' \
         WHERE drv_hash = 'outbox-lost-ack'",
    )
    .execute(&db.pool)
    .await?;

    let mut actor = bare_actor(db.pool.clone());
    let enqueued_at = std::time::Instant::now() - std::time::Duration::from_secs(60);
    let latched_at_epoch = crate::db::attempts::epoch_now() - 60.0;
    actor.latch_status_batch(crate::actor::StatusBatch {
        drv_hashes: vec!["outbox-lost-ack".into()],
        status: DerivationStatus::Cancelled,
        exec_ids: vec![],
        enqueued_at,
        latched_at_epoch,
    });
    let authority = actor.dag_authority().expect("always-leader test actor");

    // Flush 1: applies and commits (the ack of THIS flush is the one
    // we model as lost).
    actor.tick_flush_status_outbox(&authority).await;
    let (status,): (String,) =
        sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = 'outbox-lost-ack'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(status, "cancelled", "first flush must land the latch");

    // The Err arm's pop-return: the SAME batch (same enqueue instant,
    // same memory) is back at the head — the queue was empty, so the
    // chokepoint reproduces the push_front state exactly.
    actor.latch_status_batch(crate::actor::StatusBatch {
        drv_hashes: vec!["outbox-lost-ack".into()],
        status: DerivationStatus::Cancelled,
        exec_ids: vec![],
        enqueued_at,
        latched_at_epoch,
    });
    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();
    {
        let _g = metrics::set_default_local_recorder(&rec);
        // Flush 2 IS the retry the Err arm produces.
        actor.tick_flush_status_outbox(&authority).await;
    }

    let counters = counter_map(&snap);
    assert_eq!(
        counters
            .get("rio_scheduler_status_outbox_replay_refused_total")
            .copied()
            .unwrap_or(0),
        0,
        "left: 1 (pre-fix: the retry's zero-row match was attributed \
         to foreign precedence, with the false \"newer rows stand\" \
         warn) / right: 0"
    );
    assert_eq!(
        counters
            .get("rio_scheduler_status_outbox_replay_already_applied_total")
            .copied(),
        Some(1),
        "left: None (pre-fix: the lane does not exist) / right: \
         Some(1) (lost-ack replay reconciled as already durable)"
    );
    assert_eq!(actor.status_outbox.len(), 0, "the reconciled batch pops");
    Ok(())
}

// r[verify sched.attempt.cancel-close-driven+3]
/// merged_bug_108 case 3 red: tick_gc_orphan_derivations runs BEFORE
/// the flush in the same housekeeping tick and deletes terminal
/// unlinked rows — the latched batch then zero-rows with NO newer row
/// standing. PROPOSITION CERTIFIED: an absent-row residual is
/// classified vanished at the durability point — never warned as
/// "the newer rows stand" naming a row that does not exist.
#[tokio::test]
async fn outbox_replay_vanished_row_classified_vanished() -> TestResult {
    use crate::sla::metrics::counter_map;
    use metrics_util::debugging::DebuggingRecorder;

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    // A terminal, unlinked, assignment-free row: GC-eligible.
    sqlx::query(
        "INSERT INTO derivations (drv_hash, drv_path, pname, system, status) \
         VALUES ($1, $2, 'pkg', 'x86_64-linux', 'cancelled')",
    )
    .bind("outbox-vanished")
    .bind(test_drv_path("outbox-vanished"))
    .execute(&db.pool)
    .await?;
    sqlx::query(
        "UPDATE derivations SET \
           updated_at = now() - interval '120 seconds', \
           status_changed_at = now() - interval '120 seconds' \
         WHERE drv_hash = 'outbox-vanished'",
    )
    .execute(&db.pool)
    .await?;

    let mut actor = bare_actor(db.pool.clone());
    // A DependencyFailed latch for the (DAG-absent) node — terminal,
    // so the KEEP arm retains it; distinct from the durable status so
    // the absent-row cell is unambiguous.
    actor.latch_status_batch(crate::actor::StatusBatch {
        drv_hashes: vec!["outbox-vanished".into()],
        status: DerivationStatus::DependencyFailed,
        exec_ids: vec![],
        enqueued_at: std::time::Instant::now() - std::time::Duration::from_secs(60),
        latched_at_epoch: crate::db::attempts::epoch_now() - 60.0,
    });

    // The same tick's earlier GC pass deletes the orphan row (the
    // production deleter, driven directly).
    let deleted = actor.db.gc_orphan_terminal_derivations(100).await?;
    assert_eq!(deleted, 1, "world setup: the orphan row must be GC'd");

    let authority = actor.dag_authority().expect("always-leader test actor");
    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();
    {
        let _g = metrics::set_default_local_recorder(&rec);
        actor.tick_flush_status_outbox(&authority).await;
    }

    let counters = counter_map(&snap);
    assert_eq!(
        counters
            .get("rio_scheduler_status_outbox_replay_refused_total")
            .copied()
            .unwrap_or(0),
        0,
        "left: 1 (pre-fix: the vanished row was counted as a refusal, \
         with the \"newer rows stand\" warn naming a row that does \
         not exist) / right: 0"
    );
    assert_eq!(
        counters
            .get("rio_scheduler_status_outbox_replay_vanished_total")
            .copied(),
        Some(1),
        "left: None (pre-fix: the lane does not exist) / right: \
         Some(1) (latched row GC'd before the replay; nothing stands)"
    );
    assert_eq!(actor.status_outbox.len(), 0, "the vanished batch pops");
    Ok(())
}

// r[verify sched.attempt.cancel-close-driven+3]
/// merged_bug_108 surviving-lane polarity pin — pre-fix this lane
/// ALSO ticks refused, but by collapse (every zero-row cell ticked
/// it), not by proof; DISCLOSED as such. PROPOSITION CERTIFIED
/// (post-fix): the refused warn/counter fire ONLY on evidenced
/// foreign precedence — a row standing with a DIFFERENT durable
/// status — and the sibling lanes stay silent.
#[tokio::test]
async fn outbox_replay_refused_requires_newer_foreign_truth() -> TestResult {
    use crate::sla::metrics::counter_map;
    use metrics_util::debugging::DebuggingRecorder;

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    sqlx::query(
        "INSERT INTO derivations (drv_hash, drv_path, pname, system, status) \
         VALUES ($1, $2, 'pkg', 'x86_64-linux', 'created')",
    )
    .bind("outbox-foreign-truth")
    .bind(test_drv_path("outbox-foreign-truth"))
    .execute(&db.pool)
    .await?;
    sqlx::query(
        "UPDATE derivations SET \
           updated_at = now() - interval '120 seconds', \
           status_changed_at = now() - interval '120 seconds' \
         WHERE drv_hash = 'outbox-foreign-truth'",
    )
    .execute(&db.pool)
    .await?;

    let mut actor = bare_actor(db.pool.clone());
    actor.latch_status_batch(crate::actor::StatusBatch {
        drv_hashes: vec!["outbox-foreign-truth".into()],
        status: DerivationStatus::Cancelled,
        exec_ids: vec![],
        enqueued_at: std::time::Instant::now() - std::time::Duration::from_secs(60),
        latched_at_epoch: crate::db::attempts::epoch_now() - 60.0,
    });
    // AFTER the latch: a production status writer advances the row
    // (the resubmit-race shape — genuine foreign precedence).
    let mut tx = db.pool.begin().await?;
    crate::db::SchedulerDb::update_derivation_status_in_tx(
        &mut tx,
        &DrvHash::from("outbox-foreign-truth"),
        DerivationStatus::Running,
        None,
    )
    .await?;
    tx.commit().await?;

    let authority = actor.dag_authority().expect("always-leader test actor");
    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();
    {
        let _g = metrics::set_default_local_recorder(&rec);
        actor.tick_flush_status_outbox(&authority).await;
    }

    let (status,): (String,) =
        sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = 'outbox-foreign-truth'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(status, "running", "the newer foreign truth stands");
    let counters = counter_map(&snap);
    assert_eq!(
        counters
            .get("rio_scheduler_status_outbox_replay_refused_total")
            .copied(),
        Some(1),
        "evidenced foreign precedence is the ONLY lane that counts a \
         refusal"
    );
    assert_eq!(
        counters
            .get("rio_scheduler_status_outbox_replay_already_applied_total")
            .copied()
            .unwrap_or(0),
        0,
        "sibling lane silent (no lost-ack shape here)"
    );
    assert_eq!(
        counters
            .get("rio_scheduler_status_outbox_replay_vanished_total")
            .copied()
            .unwrap_or(0),
        0,
        "sibling lane silent (the row stands)"
    );
    assert_eq!(actor.status_outbox.len(), 0, "refusal is final; pops");
    Ok(())
}

// r[verify sched.attempt.cancel-close-driven+3]
/// bug_078 R1: a tick whose persists all succeed emits ZERO staleness
/// disclosures regardless of backlog age. PROPOSITION CERTIFIED at
/// the VALUE (`stale_disclosure() == None` on the returned `Drained`
/// outcome) over a production-latched aged backlog, plus the tracing
/// capture pinning the emitted warn count at 0. Pre-fix TRUE red =
/// the tracing-capture count (one "PG persists keep failing" per
/// popped batch on the healed drain tick, depth counting down); the
/// typed half is green-side by necessity — the type IS the fix — and
/// is disclosed as such.
#[tokio::test]
#[tracing_test::traced_test]
async fn healed_drain_claims_no_persist_failures() -> TestResult {
    use crate::actor::housekeeping::FlushTickOutcome;

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor(db.pool.clone());
    // Three DAG-absent terminal latches (the terminal-KEEP arm), aged
    // far past the disclosure floor — the >5min-PG-outage-heals world.
    // Distinct drvs (no supersession); durable rows backdated so the
    // replay's age cut admits them.
    for i in 0..3 {
        let h = format!("heal-drain-{i}");
        sqlx::query(
            "INSERT INTO derivations (drv_hash, drv_path, pname, system, status) \
             VALUES ($1, $2, 'pkg', 'x86_64-linux', 'running')",
        )
        .bind(&h)
        .bind(test_drv_path(&h))
        .execute(&db.pool)
        .await?;
        sqlx::query(
            "UPDATE derivations SET \
               updated_at = now() - interval '900 seconds', \
               status_changed_at = now() - interval '900 seconds' \
             WHERE drv_hash = $1",
        )
        .bind(&h)
        .execute(&db.pool)
        .await?;
        actor.latch_status_batch(crate::actor::StatusBatch {
            drv_hashes: vec![h],
            status: DerivationStatus::Cancelled,
            exec_ids: vec![],
            enqueued_at: std::time::Instant::now() - std::time::Duration::from_secs(400),
            latched_at_epoch: crate::db::attempts::epoch_now() - 400.0,
        });
    }

    let authority = actor.dag_authority().expect("always-leader test actor");
    let outcome = actor.tick_flush_status_outbox(&authority).await;

    // The typed half (green-side by necessity, disclosed): all three
    // drained; the disclosure is unconstructible from this outcome.
    assert_eq!(outcome, FlushTickOutcome::Drained { batches: 3 });
    assert_eq!(outcome.stale_disclosure(), None);
    assert_eq!(actor.status_outbox.len(), 0, "the healed tick drains all");
    // The pre-fix red: the lying-warn count on the healed drain tick.
    logs_assert(|lines: &[&str]| {
        let lying = lines
            .iter()
            .filter(|l| l.contains("status outbox head is old"))
            .count();
        if lying == 0 {
            Ok(())
        } else {
            Err(format!(
                "left: {lying} warns on a tick with zero failed persists \
                 (one per popped batch, depth counting down) / right: 0"
            ))
        }
    });
    Ok(())
}

// r[verify sched.attempt.cancel-close-driven+3]
/// bug_078 R2: a parked-on-Err tick with an aged head yields exactly
/// one disclosure carrying that head's age and the live depth — the
/// failure-evidence coupling. PROPOSITION CERTIFIED: the disclosure
/// derives from the typed outcome's parked-on-Err arm (pool closed —
/// the production-shaped PG failure where every query errors) and
/// from nowhere else; totality over the third arm (Fenced → None)
/// rides the same closed match. The typed assertions are
/// pre-fix-inexpressible (the outcome type IS the fix — disclosed
/// strawman per R16); the pre-fix behavior is pinned by R1's capture.
#[tokio::test]
#[tracing_test::traced_test]
async fn parked_tick_disclosure_carries_the_failure_evidence() -> TestResult {
    use crate::actor::housekeeping::FlushTickOutcome;

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor(db.pool.clone());
    actor.latch_status_batch(crate::actor::StatusBatch {
        drv_hashes: vec!["parked-aged".into()],
        status: DerivationStatus::Cancelled,
        exec_ids: vec![],
        enqueued_at: std::time::Instant::now() - std::time::Duration::from_secs(400),
        latched_at_epoch: crate::db::attempts::epoch_now() - 400.0,
    });
    let authority = actor.dag_authority().expect("always-leader test actor");

    // Production-shaped PG failure: the pool is closed; every query
    // errors at acquire.
    db.pool.close().await;
    let outcome = actor.tick_flush_status_outbox(&authority).await;

    match &outcome {
        FlushTickOutcome::ParkedOnErr { head_age, depth } => {
            assert!(
                *head_age > std::time::Duration::from_secs(300),
                "the park captured the aged head's enqueue age (got {head_age:?})"
            );
            assert_eq!(*depth, 1, "the re-pushed head is the live depth");
        }
        other => panic!("left: {other:?} / right: ParkedOnErr with the failure evidence"),
    }
    let d = outcome
        .stale_disclosure()
        .expect("an aged parked-on-Err head discloses");
    assert!(d.head_age > std::time::Duration::from_secs(300));
    assert_eq!(d.depth, 1);
    assert_eq!(
        actor.status_outbox.len(),
        1,
        "the batch is retained for the next tick"
    );
    // Exactly one disclosure (single constructor x single emission
    // site — the cardinality envelope).
    logs_assert(|lines: &[&str]| {
        let n = lines
            .iter()
            .filter(|l| l.contains("status outbox head is old"))
            .count();
        if n == 1 {
            Ok(())
        } else {
            Err(format!("left: {n} disclosures / right: exactly 1"))
        }
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// Round-9 WO-S2-1 witness-gap delta (W9-O): driven-Tick phase cells
// ---------------------------------------------------------------------------

/// W9-O (round-9 B2 admissibility): a driven leader Tick populates
/// EVERY cell of `rio_scheduler_tick_phase_seconds` -- all 19 phases
/// `00-priority-sweep`..`18-snapshot-publish`, exactly. The landed
/// instrument (00fbb0717) is the measurement substrate for every
/// Banner-A bounding decision (which Tick term gets a work quota
/// first), so an unreachable phase cell is a silent forensics hole:
/// the live_053 134.65s Tick was log-silent for ~118s precisely
/// because nothing named the phase. The describe-side (lllll)
/// reachability is pinned by the metrics_registered census; this
/// drives the RECORD side through the production `handle_tick` on an
/// authoritative leader (the destructive block runs -- an
/// unauthoritative tick records only the two observe phases, which
/// this test would catch as 17 missing cells).
///
/// Thread-local recorder + current-thread runtime (the
/// test_not_leader_does_not_set_gauges mechanism); snapshot taken
/// EXACTLY ONCE (DebuggingRecorder snapshots drain).
#[tokio::test]
async fn driven_leader_tick_records_every_phase_cell() -> TestResult {
    use metrics_util::debugging::DebuggingRecorder;

    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor(db.pool.clone());
    assert!(
        actor.dag_authority().is_some(),
        "direct-setup actor must be authoritative (the destructive \
         phase block is the population under test)"
    );
    {
        let _g = metrics::set_default_local_recorder(&rec);
        actor.handle_tick().await;
    }

    // ONE snapshot (drains); collect the phase label set.
    let recorded: std::collections::BTreeSet<String> = snap
        .snapshot()
        .into_vec()
        .into_iter()
        .filter(|(key, _, _, _)| key.key().name() == "rio_scheduler_tick_phase_seconds")
        .flat_map(|(key, _, _, _)| {
            key.key()
                .labels()
                .filter(|l| l.key() == "phase")
                .map(|l| l.value().to_string())
                .collect::<Vec<_>>()
        })
        .collect();

    const PHASES: [&str; 19] = [
        "00-priority-sweep",
        "01-scan-dag",
        "02-build-timeouts",
        "03-stuck-completions",
        "04-orphaned-builds",
        "05-expired-poisons",
        "06-gc-orphan-derivations",
        "07-gc-attempt-ledger",
        "08-gc-materialization-jobs",
        "09-gc-wanted-outputs",
        "10-sweep-dispatched-cells",
        "11-flush-status-outbox",
        "12-establishment-sweep",
        "13-materialization-backstop",
        "14-zero-interest-cancel",
        "15-parked-reevaluation",
        "16-pending-carriers",
        "17-ready-cache-sweep",
        "18-snapshot-publish",
    ];
    for phase in PHASES {
        assert!(
            recorded.contains(phase),
            "driven leader Tick must record phase cell {phase:?}; \
             recorded set: {recorded:?}"
        );
    }
    assert_eq!(
        recorded.len(),
        PHASES.len(),
        "exactly the 19 documented phases (a stray label is a drifted \
         phase! call): {recorded:?}"
    );
    Ok(())
}

/// W9-AG (round-9 B8): admin mint latency is bounded independently of
/// Tick cost. A Fast-lane admin command (`AdminQuery::lane()` —
/// MintExecutorTokens is the spawn-path exemplar) completes within
/// `ADMIN_FAST_DELIVERY_SLO` (5s, mirroring the controller's
/// ADMIN_RPC_TIMEOUT) even while a Tick whose TOTAL cost exceeds the
/// SLO is mid-flight, because the fast lane is drained at every phase
/// boundary — delivery is bounded by the largest indivisible work
/// slice, not the whole mailbox FIFO.
///
/// Wall-clock witness BY DESIGN (the RC-2 carve-out, recorded in the
/// round-9 book): the law itself is a latency SLO. The synthetic Tick
/// is paused-clock-FREE (real sleeps via the StallTickPhases hook);
/// slack: post-fix delivery is expected ≤ one stalled phase (2.2s) +
/// scheduling jitter — asserting < 5s carries ≥ 2× headroom, so
/// builder CPU contention cannot flake it without ALSO breaking the
/// premise assert (which would name the real cause).
///
/// Pre-fix red (FIFO delivery behind the whole tick): the first mint
/// waited out the remaining ~6.3s of the 6.6s tick > 5s SLO.
// r[verify sched.admission.work-per-turn]
#[tokio::test]
async fn admin_mint_latency_bounded_under_long_tick() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Premise: 3 stalled phases × 2.2s = 6.6s total tick — over the
    // 5s SLO while each individual phase stays well under it (the
    // decomposed-tick shape; B1 made the worst real phase bounded,
    // B2's plane measures the rest).
    let each = std::time::Duration::from_millis(2200);
    handle.debug_stall_tick_phases(3, each).await?;

    let t_tick = std::time::Instant::now();
    handle.send_unchecked(ActorCommand::Tick).await?;
    // Let the tick enter its first stalled phase before minting, so
    // every delivery below is measured INSIDE the long tick.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Sequential mints during the long tick: enqueue→reply wall time
    // is the controller-visible delivery axis (the 18s live_053
    // measurement was exactly this).
    let mut deliveries: Vec<std::time::Duration> = Vec::new();
    for _ in 0..4 {
        let t = std::time::Instant::now();
        let _map = handle
            .query_unchecked(|reply| {
                ActorCommand::Admin(AdminQuery::MintExecutorTokens {
                    intent_ids: vec!["w9ag-absent-intent".into()],
                    reply,
                })
            })
            .await?;
        deliveries.push(t.elapsed());
    }

    // Premise reachability (RC-2): the tick really was longer than
    // the SLO — without this the deliveries above could have been
    // measured against an already-finished tick and the test would
    // be vacuous. barrier() rides the Bulk lane (GcRoots), so it
    // flushes the mailbox FIFO including the Tick.
    barrier(&handle).await;
    let tick_wall = t_tick.elapsed();
    assert!(
        tick_wall > crate::actor::ADMIN_FAST_DELIVERY_SLO,
        "premise: the synthetic tick must exceed the SLO (got {tick_wall:?}); \
         the stall hook did not fire — W9-AG is vacuous"
    );

    for (i, d) in deliveries.iter().enumerate() {
        assert!(
            *d < crate::actor::ADMIN_FAST_DELIVERY_SLO,
            "mint {i} delivered in {d:?} ≥ the {:?} SLO while the tick ran \
             {tick_wall:?}: Fast-lane admin starved behind Tick cost (W9-AG)",
            crate::actor::ADMIN_FAST_DELIVERY_SLO
        );
    }
    Ok(())
}

// r[verify sched.sla.forecast.tenant-ceiling]
/// W10-S (merged_bug_099) — the tenant ceiling at the TENANT
/// quantifier, not per-(tenant × filter view). Pre-fix
/// `tenant_forecast_budget` was seeded fresh inside every
/// `compute_spawn_intents` call and BOTH the Ready debit and the
/// forecast gate sat downstream of `passes_intent_filter`, so the
/// spec MUST (Ready cores subtracted before forecast admission,
/// per-tenant) was enforced per filter view: the controller's
/// per-pool polls each saw a fresh cap.
///
/// Two cells, one tenant, cap C = 8 (each unfitted intent solves to
/// probe.cpu = 4):
///
/// (a) double-spend: forecast candidates split across two system
///     views; pre-fix each view admits C worth → combined 2C (the
///     red); post-fix the admission set is computed against the
///     UNFILTERED population and views only PROJECT it — combined
///     ≤ C.
/// (b) cross-view Ready debit: Ready Σ8 on x86 exhausts the cap;
///     the aarch64 view's forecast admission must see that debit
///     (pre-fix it saw none — the Ready nodes fail the view filter
///     before the debit line).
#[tokio::test]
async fn forecast_tenant_ceiling_holds_across_filter_views() {
    use crate::sla::config::CapacityType;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;

    let view = |sys: &str| SpawnIntentsRequest {
        systems: vec![sys.to_string()],
        ..Default::default()
    };
    let forecast_cores = |snap: &crate::actor::SpawnIntentsSnapshot| -> u32 {
        snap.intents
            .iter()
            .filter(|i| i.ready == Some(false))
            .map(|i| i.cores)
            .sum()
    };

    // ── (a) the double-spend cell ───────────────────────────────────
    let mut actor = {
        let mut sla = test_sla_config();
        for s in ["x86_64-linux", "aarch64-linux"] {
            let _ = s;
        }
        sla.lead_time_seed
            .insert(("test-hw".into(), CapacityType::Spot), 200.0);
        sla.max_forecast_cores_per_tenant = 8;
        bare_actor_cfg(
            db.pool.clone(),
            DagActorConfig {
                sla,
                ..Default::default()
            },
        )
    };
    // 3 forecast candidates per system (3×4 = 12 > 8 per side).
    for (sys, tags) in [
        ("x86_64-linux", ["xa", "xb", "xc"]),
        ("aarch64-linux", ["ya", "yb", "yc"]),
    ] {
        for q in tags {
            let dep = format!("{q}dep");
            actor.test_inject_at(&dep, sys, DerivationStatus::Running);
            actor.test_inject_at(q, sys, DerivationStatus::Queued);
            actor.test_inject_edge(q, &dep);
            actor.test_set_running_eta(&dep, 50.0, 10, 4);
        }
    }
    let spent_a = forecast_cores(&actor.compute_spawn_intents(&view("x86_64-linux")));
    let spent_b = forecast_cores(&actor.compute_spawn_intents(&view("aarch64-linux")));
    assert!(
        spent_a + spent_b <= 8,
        "left (pre-fix): one tenant, cap 8, TWO filter views — combined \
         forecast spend {} + {} = {} ≈ 2×cap (each per-pool poll seeded a \
         fresh budget; the ceiling was per-(tenant × view)) / right: the \
         admission set is computed once against the unfiltered population \
         and views only project it — combined ≤ cap at the tenant \
         quantifier",
        spent_a,
        spent_b,
        spent_a + spent_b
    );

    // ── (b) the cross-view Ready-debit cell ────────────────────────
    let mut actor = {
        let mut sla = test_sla_config();
        sla.lead_time_seed
            .insert(("test-hw".into(), CapacityType::Spot), 200.0);
        sla.max_forecast_cores_per_tenant = 8;
        bare_actor_cfg(
            db.pool.clone(),
            DagActorConfig {
                sla,
                ..Default::default()
            },
        )
    };
    // Ready Σ8 on x86 (2 × 4 cores) exhausts the cap; forecast
    // candidates live on aarch64 only.
    for r in ["r0", "r1"] {
        actor.test_inject_ready(r, None, "x86_64-linux", false);
    }
    for q in ["za", "zb"] {
        let dep = format!("{q}dep");
        actor.test_inject_at(&dep, "aarch64-linux", DerivationStatus::Running);
        actor.test_inject_at(q, "aarch64-linux", DerivationStatus::Queued);
        actor.test_inject_edge(q, &dep);
        actor.test_set_running_eta(&dep, 50.0, 10, 4);
    }
    let spent = forecast_cores(&actor.compute_spawn_intents(&view("aarch64-linux")));
    assert_eq!(
        spent, 0,
        "left (pre-fix): the aarch64 view admitted {spent} forecast cores — \
         the tenant's x86 Ready Σ8 never debited this view's budget (the \
         debit sat below the view filter) / right: Ready cores debit the \
         tenant's budget against the UNFILTERED population; the exhausted \
         cap admits nothing in any view"
    );
}

// r[verify sched.sla.forecast.tenant-ceiling]
/// **W11-AC (bug_143)** — *proposition: Σ provisioned cores per
/// tenant ≤ cap under every gate/backoff/forecast interleaving — the
/// debit chokepoint sits ABOVE every emission gate, so suppressing
/// EMISSION never un-accounts demand (the bug_129 lesson generalized
/// from `queued_by_system` to the debit); population: the adversarial
/// schedule pinned — Ready cores at cap inside their backoff window
/// while forecast candidates sit inside the provisioning lead
/// horizon, then the backoff lapses. The probe gate is
/// position-equivalent (both gates sit below the single debit
/// chokepoint post-fix), so this cell quantifies the gate
/// composition.*
///
/// Pre-fix RED (the cap+N shape): the backoff gate `continue`d
/// BEFORE the per-tenant debit, so a tenant's in-backoff Ready cores
/// left the ledger at full cap and the forecast pass admitted up to
/// cap on top — when the backoff lapsed inside the provisioning lead
/// horizon, the never-debited Ready emission landed on the already-
/// forecast-provisioned capacity: cap+N provisioned cores with N
/// tenant-influenceable (a tenant manufactures backoff via failing
/// builds). Wave-10's merged_bug_099 made the debit view-independent
/// yet re-certified the gates as "upstream of the debit" — sound for
/// instant spawnability, false for a future-directed budget whose
/// lead horizon overlaps the backoff window.
#[tokio::test]
async fn forecast_debit_charges_backoffed_ready_cores() {
    use crate::sla::config::CapacityType;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;

    let forecast_cores = |snap: &crate::actor::SpawnIntentsSnapshot| -> u32 {
        snap.intents
            .iter()
            .filter(|i| i.ready == Some(false))
            .map(|i| i.cores)
            .sum()
    };
    let ready_cores = |snap: &crate::actor::SpawnIntentsSnapshot| -> u32 {
        snap.intents
            .iter()
            .filter(|i| i.ready == Some(true))
            .map(|i| i.cores)
            .sum()
    };

    let mut actor = {
        let mut sla = test_sla_config();
        sla.lead_time_seed
            .insert(("test-hw".into(), CapacityType::Spot), 200.0);
        sla.max_forecast_cores_per_tenant = 8;
        bare_actor_cfg(
            db.pool.clone(),
            DagActorConfig {
                sla,
                ..Default::default()
            },
        )
    };
    // Ready Σ8 (2 × 4 probe cores) — IN BACKOFF (the gate that
    // pre-fix sat above the debit).
    for r in ["bk0", "bk1"] {
        actor.test_inject_ready(r, None, "x86_64-linux", false);
        actor.dag.node_mut(r).expect("injected").retry.backoff_until =
            Some(std::time::Instant::now() + std::time::Duration::from_secs(60));
    }
    // Forecast candidates Σ8 inside the lead horizon (seed 200s,
    // deps ~40s remaining).
    for q in ["fa", "fb"] {
        let dep = format!("{q}dep");
        actor.test_inject_at(&dep, "x86_64-linux", DerivationStatus::Running);
        actor.test_inject_at(q, "x86_64-linux", DerivationStatus::Queued);
        actor.test_inject_edge(q, &dep);
        actor.test_set_running_eta(&dep, 50.0, 10, 4);
    }

    // Poll 1 — during the backoff window: the provisioning plane acts
    // on this snapshot's forecast admission.
    let during = actor.compute_spawn_intents(&Default::default());
    let f = forecast_cores(&during);
    assert_eq!(
        ready_cores(&during),
        0,
        "premise: the backoff gate suppresses Ready EMISSION (bug_282)"
    );

    // The window lapses inside the lead horizon: the Ready emission
    // lands on top of whatever the forecast already provisioned.
    for r in ["bk0", "bk1"] {
        actor.dag.node_mut(r).expect("injected").retry.backoff_until =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(1));
    }
    let after = actor.compute_spawn_intents(&Default::default());
    let r = ready_cores(&after);
    assert_eq!(r, 8, "premise: the lapsed window restores Ready emission");

    assert!(
        f + r <= 8,
        "left (pre-fix): forecast admitted {f} cores while the tenant's \
         Σ8 backoff'd Ready cores were never debited (the gate sat above \
         the debit), then the lapsed backoff emitted {r} Ready cores on \
         top — {} provisioned ≈ cap+N, N tenant-influenceable / right: \
         the debit chokepoint sits immediately after demand counting, \
         above every emission gate — in-backoff Ready cores exhaust the \
         cap and the forecast admits nothing: Σ ≤ cap under the \
         gate/backoff/forecast interleaving",
        f + r
    );
}

// r[verify sec.executor.identity-token+3]
/// W10-T (bug_046, triage-corrected) — the Omitted-loop death,
/// count-based at the mint. Pre-fix `mint_executor_tokens` re-solved
/// a full unfiltered page and DIFFED the requested ids against it:
/// any divergence between the page the controller polled and the
/// page the mint re-solved (the budget-granularity bug pre-W10-S;
/// population movement between the two RPCs after it) silently
/// OMITTED held intents — the controller skips the token-less Job
/// and re-polls, the same ids re-present, and the live
/// Fetcher/static-node arms looped "drv left Ready" Omitted skips
/// per tick (deterministic, not a transient race — the page diff
/// punished ids for OTHER nodes' movement).
///
/// Post-fix the mint resolves each REQUESTED id directly against the
/// DAG (solve-per-id, memoized): an id is minted iff ITS OWN node is
/// still in the mintable population (Ready|Queued — the same status
/// set the two emission loops serve); only the id's own state
/// movement omits.
///
/// Cells:
/// (a) displacement: ids lawfully obtained from a view poll, then
///     the population SHIFTS (new, higher-rank forecast candidates
///     merge before the mint). Pre-fix: the re-solved page admits
///     the newcomers, the budget displaces the held ids — ZERO
///     tokens for valid held intents (the red, count-based).
///     Post-fix: the held ids' own nodes are unchanged — full
///     coverage.
/// (b) stationary churn: with no population movement, repeated
///     polls and mints stay at full coverage (zero per-tick churn —
///     count-based, not wall-clock).
#[tokio::test]
async fn mint_resolves_requested_ids_not_a_reemitted_page() {
    use crate::sla::config::CapacityType;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;

    let mut sla = test_sla_config();
    sla.lead_time_seed
        .insert(("test-hw".into(), CapacityType::Spot), 200.0);
    sla.max_forecast_cores_per_tenant = 8; // admits 2 × 4-core intents
    let plumbing = DagActorPlumbing {
        hmac_signer: Some(std::sync::Arc::new(rio_auth::hmac::HmacSigner::from_key(
            b"test-key-at-least-32-bytes-long!".to_vec(),
        ))),
        ..Default::default()
    };
    let mut actor = DagActor::new(
        SchedulerDb::new(db.pool.clone()),
        DagActorConfig {
            sla,
            ..Default::default()
        },
        plumbing,
    );

    // Two aarch64 forecast candidates (hash-late: "zz*").
    for q in ["zz0", "zz1"] {
        let dep = format!("{q}dep");
        actor.test_inject_at(&dep, "aarch64-linux", DerivationStatus::Running);
        actor.test_inject_at(q, "aarch64-linux", DerivationStatus::Queued);
        actor.test_inject_edge(q, &dep);
        actor.test_set_running_eta(&dep, 50.0, 10, 4);
    }
    // The controller's per-pool poll: both zz intents emitted.
    let view = SpawnIntentsRequest {
        systems: vec!["aarch64-linux".into()],
        ..Default::default()
    };
    let polled: Vec<String> = actor
        .compute_spawn_intents(&view)
        .intents
        .iter()
        .filter(|i| i.ready == Some(false))
        .map(|i| i.intent_id.clone())
        .collect();
    assert_eq!(polled.len(), 2, "both zz intents polled");

    // ── (a) displacement between poll and mint ─────────────────────
    // Two new x86 candidates merge with near-zero ETA (deps almost
    // done): the canonical admission sort (priority, cores, ETA asc,
    // hash) ranks them FIRST, and they exhaust the cap in any page
    // re-solve.
    for q in ["aa0", "aa1"] {
        let dep = format!("{q}dep");
        actor.test_inject_at(&dep, "x86_64-linux", DerivationStatus::Running);
        actor.test_inject_at(q, "x86_64-linux", DerivationStatus::Queued);
        actor.test_inject_edge(q, &dep);
        actor.test_set_running_eta(&dep, 50.0, 49, 4);
    }
    let (tokens, keyless) = actor.mint_executor_tokens(&polled);
    assert!(!keyless, "signer configured");
    assert!(
        tokens.is_empty(),
        "displaced ids must REFUSE at the credential gate — the newly \
         admitted aa* work owns the cap now; minting the held zz ids \
         anyway would put the tenant's credentialed forecast set over \
         the ceiling (the R26 authorization gate, not churn)"
    );

    // ── (b) stationary zero-churn, count-based over 3 mint beats ───
    // The CURRENT admitted set mints at full coverage every beat —
    // the deterministic-page property W10-S delivered (pre-W10-S
    // this looped Omitted on the live arms).
    let admitted: Vec<String> = actor
        .compute_spawn_intents(&SpawnIntentsRequest::default())
        .intents
        .iter()
        .map(|i| i.intent_id.clone())
        .collect();
    assert_eq!(admitted.len(), 2, "the cap admits exactly the aa pair");
    for beat in 0..3 {
        let (tokens, _) = actor.mint_executor_tokens(&admitted);
        assert_eq!(
            tokens.len(),
            admitted.len(),
            "beat {beat}: a stationary population must produce ZERO \
             per-tick Omitted churn (count-based)"
        );
    }
}

// r[verify sec.executor.identity-token+3]
/// The mint AUTHORIZATION gate (security repair of the per-id form;
/// R26): `compute_spawn_intents` is the SOLE authority on which ids
/// are mintable — an executor credential exists ONLY for intents the
/// admission layer (classification, backoff, probe gate, 1-layer
/// law, tenant budget) actually emitted. The per-id DAG resolve this
/// repairs tested membership against the RAW DAG (status ∈
/// {Ready, Queued}) — an incomplete, unauthorized view: it signed
/// credentials for never-admissible work (a Queued drv behind a
/// Queued dep, which NO page ever emits) and for budget-DISPLACED
/// forecast intents (re-opening the tenant-ceiling bypass at the
/// credential surface — a worse form of the bug_046 divergence).
///
/// Pre-repair red (verbatim in the owning commit): both unadmitted
/// ids MINT. Post-repair: both REFUSE (omitted from the map — the
/// controller's existing skip-and-re-poll arm consumes the absence).
#[tokio::test]
async fn mint_refuses_ids_outside_the_admitted_emission() {
    use crate::sla::config::CapacityType;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;

    let mut sla = test_sla_config();
    sla.lead_time_seed
        .insert(("test-hw".into(), CapacityType::Spot), 200.0);
    sla.max_forecast_cores_per_tenant = 8; // admits 2 × 4-core intents
    let plumbing = DagActorPlumbing {
        hmac_signer: Some(std::sync::Arc::new(rio_auth::hmac::HmacSigner::from_key(
            b"test-key-at-least-32-bytes-long!".to_vec(),
        ))),
        ..Default::default()
    };
    let mut actor = DagActor::new(
        SchedulerDb::new(db.pool.clone()),
        DagActorConfig {
            sla,
            ..Default::default()
        },
        plumbing,
    );

    // (1) NEVER-ADMISSIBLE: Queued behind a Queued dep — the 1-layer
    // law excludes it from every page, forever.
    actor.test_inject_at("deep-dep", "x86_64-linux", DerivationStatus::Queued);
    actor.test_inject_at("deep", "x86_64-linux", DerivationStatus::Queued);
    actor.test_inject_edge("deep", "deep-dep");

    // (2) BUDGET-DISPLACED: zz forecast intents admitted, then aa
    // newcomers (near-zero eta) displace them from the cap.
    for q in ["zz0", "zz1"] {
        let dep = format!("{q}dep");
        actor.test_inject_at(&dep, "aarch64-linux", DerivationStatus::Running);
        actor.test_inject_at(q, "aarch64-linux", DerivationStatus::Queued);
        actor.test_inject_edge(q, &dep);
        actor.test_set_running_eta(&dep, 50.0, 10, 4);
    }
    for q in ["aa0", "aa1"] {
        let dep = format!("{q}dep");
        actor.test_inject_at(&dep, "x86_64-linux", DerivationStatus::Running);
        actor.test_inject_at(q, "x86_64-linux", DerivationStatus::Queued);
        actor.test_inject_edge(q, &dep);
        actor.test_set_running_eta(&dep, 50.0, 49, 4);
    }
    // Authority check: the admitted page is exactly {aa0, aa1}.
    let page: Vec<String> = actor
        .compute_spawn_intents(&SpawnIntentsRequest::default())
        .intents
        .iter()
        .map(|i| i.intent_id.clone())
        .collect();
    assert_eq!(page, vec!["aa0", "aa1"], "precondition: aa displaced zz");

    let requested: Vec<String> = vec!["deep".into(), "zz0".into(), "zz1".into()];
    let (tokens, keyless) = actor.mint_executor_tokens(&requested);
    assert!(!keyless, "signer configured");
    assert!(
        tokens.is_empty(),
        "left (pre-repair): the per-id mint signed executor credentials for \
         {} of 3 ids OUTSIDE the admitted emission — a never-admissible \
         1-layer-violating drv and two budget-displaced forecast drvs (the \
         tenant ceiling bypassed at the credential surface) / right: \
         membership in the computed page is REQUIRED to sign (R26: the \
         authorized view, not the raw DAG); all three refuse",
        tokens.len()
    );

    // The admitted ids still mint — the gate refuses, never starves.
    let (tokens, _) = actor.mint_executor_tokens(&page);
    assert_eq!(tokens.len(), 2, "admitted ids mint");
}
