//! Miscellaneous actor feature tests that don't fit the other modules:
//! GcRoots collection, orphan-build cancellation, backpressure hysteresis,
//! leader/recovery dispatch gating.

use super::*;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tracing_test::traced_test;

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
        Arc::new(AtomicBool::new(recovery_complete)),
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

// r[verify sched.recovery.gate-dispatch]
/// `dispatch_ready` early-returns when `is_leader=false` OR
/// `recovery_complete=false`. Worker connected, DAG merged, heartbeat
/// sent → NO assignment received.
#[rstest::rstest]
#[case::not_leader(false, true)]
#[case::recovery_incomplete(true, false)]
#[tokio::test]
async fn test_dispatch_gated_on_leader_and_recovery(
    #[case] is_leader: bool,
    #[case] recovery_complete: bool,
) -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = spawn_actor_with_flags(db.pool.clone(), is_leader, recovery_complete);

    let mut rx = connect_executor(&handle, "gate-w", "x86_64-linux").await?;
    merge_single_node(
        &handle,
        Uuid::new_v4(),
        "gate-drv",
        PriorityClass::Scheduled,
    )
    .await?;
    send_heartbeat(&handle, "gate-w", "x86_64-linux").await?;
    barrier(&handle).await;

    assert!(
        rx.try_recv().is_err(),
        "is_leader={is_leader} recovery_complete={recovery_complete} → no dispatch"
    );
    Ok(())
}

// r[verify obs.metric.scheduler-leader-gate+2]
/// When is_leader=false, handle_tick must NOT set state gauges.
/// Standby actor is warm (DAGs merge for takeover) but workers don't
/// connect to it (leader-guarded gRPC) — its counts are stale/zero.
/// Publishing them creates a second Prometheus series that stat-panel
/// reducers pick nondeterministically.
///
/// Mechanism mirrors test_force_drain_increments_cancel_signals_total
/// (tests/`state/executor.rs`): `set_default_local_recorder` installs a
/// thread-local recorder; `#[tokio::test]`'s current-thread runtime
/// means the actor task sees it at `.await` points. The recorder's
/// `register_gauge` tracks names touched — absence of all four gauge
/// names after Tick proves the gate held.
///
/// No connect_executor: the inc/dec at `state/executor.rs`/76/384 would touch
/// `workers_active` outside the gated block. MergeDag is safe —
/// dispatch_ready (the only gauge path reachable from merge) early-
/// returns at dispatch.rs:18 on a standby before touching
/// class_queue_depth.
#[tokio::test]
async fn test_not_leader_does_not_set_gauges() -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = spawn_actor_with_flags(db.pool.clone(), false, true);

    // Merge a DAG so there's something to count. Standby DOES merge
    // (r[sched.lease.k8s-lease]: "DAGs are still merged so state is
    // warm for takeover"). If the gate is broken, derivations_queued
    // would be set to 1 (this node enters ready_queue — no deps).
    merge_single_node(&handle, Uuid::new_v4(), "sg-drv", PriorityClass::Scheduled).await?;

    // Tick on a fresh actor: tick_count 0→1, maybe_refresh_estimator
    // early-returns (1%6≠0), event_persist_tx is None → sweep gated
    // out. No workers, nothing running → heartbeat/backstop/poison
    // scans no-op. Gauge block is the only gauge path reachable.
    handle.send_unchecked(ActorCommand::Tick).await?;
    barrier(&handle).await;

    // The four handle_tick gauges must NOT appear.
    for name in [
        "rio_scheduler_derivations_queued",
        "rio_scheduler_workers_active",
        "rio_scheduler_builds_active",
        "rio_scheduler_derivations_running",
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

// r[verify sched.lease.standby-tick-noop]
// r[verify obs.metric.scheduler-leader-gate+2]
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
    // LeaderLost zeroed the leader-state gauges (one-shot, not
    // per-Tick). workers_active is NOT in this list — it's
    // connection-state (executors map is retained on lose), maintained
    // by inc/dec on standby.
    for g in [
        "rio_scheduler_derivations_queued",
        "rio_scheduler_builds_active",
        "rio_scheduler_derivations_running",
    ] {
        assert_eq!(
            recorder.gauge_value(&format!("{g}{{}}")),
            Some(0.0),
            "LeaderLost should zero {g} so ex-leader's series collapses"
        );
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

// r[verify obs.metric.scheduler-leader-gate+2]
/// `handle_leader_lost` must NOT zero `rio_scheduler_workers_active`:
/// `executors` is retained (live connections, not persisted) and
/// `ExecutorDisconnected` is not leader-gated. Zeroing it desyncs from
/// N retained entries; each worker rebalancing away then decrements
/// from the zeroed baseline → −1…−N. The inc/dec path maintains it
/// correctly on standby; workers leaving drain it naturally to 0.
#[tokio::test]
async fn test_leader_lost_workers_active_stays_nonneg() -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task, leader) = spawn_actor_with_leader(db.pool.clone(), true, true);

    // Connect+register 2 workers as leader. inc/dec at executor.rs
    // sets workers_active=2.
    let _rx1 = connect_executor(&handle, "wa-w1", "x86_64-linux").await?;
    let _rx2 = connect_executor(&handle, "wa-w2", "x86_64-linux").await?;
    barrier(&handle).await;
    assert_eq!(
        recorder.gauge_value("rio_scheduler_workers_active{}"),
        Some(2.0),
        "precondition: 2 workers registered"
    );

    // Lose the lease. handle_leader_lost must NOT zero workers_active
    // (executors map still has 2 entries).
    leader.on_lose();
    handle.send_unchecked(ActorCommand::LeaderLost).await?;
    barrier(&handle).await;
    assert_eq!(
        recorder.gauge_value("rio_scheduler_workers_active{}"),
        Some(2.0),
        "LeaderLost must NOT zero workers_active (executors retained)"
    );

    // Workers rebalance to the new leader → streams to this pod drop
    // → ExecutorDisconnected (NOT leader-gated) → decrement.
    for w in ["wa-w1", "wa-w2"] {
        handle
            .send_unchecked(ActorCommand::ExecutorDisconnected {
                executor_id: w.into(),
                stream_epoch: stream_epoch_for(w),
                seen_drvs: vec![],
            })
            .await?;
    }
    barrier(&handle).await;

    // Gauge must be 0, NOT −2. Before the fix: set(0.0) on LeaderLost
    // followed by 2× decrement → −2.0.
    assert_eq!(
        recorder.gauge_value("rio_scheduler_workers_active{}"),
        Some(0.0),
        "workers_active must drain to 0 (not go negative) after \
         LeaderLost + disconnects"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// HMAC assignment token signing
// ---------------------------------------------------------------------------

// r[verify sec.boundary.grpc-hmac]
/// When `with_hmac_signer` is set, dispatched assignments carry a
/// signed token that the store can verify. Token must contain the
/// derivation's expected_output_paths so the store can enforce
/// "worker can only upload assigned outputs".
#[tokio::test]
async fn test_hmac_signer_produces_verifiable_token() -> TestResult {
    use rio_auth::hmac::{HmacSigner, HmacVerifier};

    let db = TestDb::new(&MIGRATOR).await;
    let test_key = b"test-scheduler-hmac-key-32bytes!".to_vec();

    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |_, p| {
        p.hmac_signer = Some(Arc::new(HmacSigner::from_key(test_key.clone())));
    });

    let mut worker_rx = connect_executor(&handle, "hmac-w", "x86_64-linux").await?;

    // Merge a node WITH expected_output_paths set — the token's
    // claims must include them.
    let expected_out = test_store_path("hmac-expected-out");
    let mut node = make_node("hmac-drv");
    node.drv_content = b"Derive-hmac-test".to_vec(); // ingress-byte-bound (see claims-derived suite)
    node.expected_output_paths = vec![expected_out.clone()];
    merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;

    let assignment = recv_assignment(&mut worker_rx).await;

    // Token is NOT the legacy "{worker}-{hash}-{gen}" format.
    assert!(
        !assignment.assignment_token.starts_with("hmac-w-hmac-drv-"),
        "should be HMAC-signed, not legacy format: {}",
        assignment.assignment_token
    );

    // Verify with the same key.
    let verifier = HmacVerifier::from_key(test_key);
    let claims = verifier
        .verify::<rio_auth::hmac::AssignmentClaims>(&assignment.assignment_token)
        .expect("token should verify with same key");

    assert_eq!(claims.executor_id, "hmac-w");
    assert_eq!(claims.drv_hash, "hmac-drv");
    assert!(
        claims.expected_outputs.contains(&expected_out),
        "claims should include expected_output_paths: {:?}",
        claims.expected_outputs
    );
    assert!(
        !claims.is_fixed_output,
        "a non-FOD assignment must sign is_fixed_output = false"
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

    let mut worker_rx = connect_executor(&handle, "phase2-w", "x86_64-linux").await?;

    let _ = merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id: Uuid::new_v4(),
            ingress_stripped: Default::default(),
            tenant_id: Some(tenant),
            priority_class: PriorityClass::Scheduled,
            nodes: vec![{
                // Ingress-byte-bound: token-mechanics pin (see the
                // sched.dispatch.claims-derived suite for store-backed).
                let mut n = make_node("phase2-drv");
                n.drv_content = b"Derive-hmac-test".to_vec();
                n
            }],
            edges: vec![],
            options: BuildOptions::default(),
            keep_going: false,
            traceparent: String::new(),
            jti: None,
            jwt_token: None,
        },
    )
    .await?;

    let assignment = recv_assignment(&mut worker_rx).await;
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

/// bug_001 (round 6): the scheduler always signs `is_fixed_output`
/// from the persisted node flag so the store can refuse
/// descriptor-less uploads for content-bound (fixed-output)
/// assignments. Writer-side pin — the serde shape lives in `rio-auth`
/// (`assignment_claims_fixed_output_round_trip`), the store-side
/// enforcement in `rio-store` (`hmac_fod_descriptorless_rejected`).
#[tokio::test]
async fn test_hmac_assignment_marks_fixed_output() -> TestResult {
    use rio_auth::hmac::{HmacSigner, HmacVerifier};

    let db = TestDb::new(&MIGRATOR).await;
    let test_key = b"test-fod-claims-key-32-bytes!!!!".to_vec();

    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |_, p| {
        p.hmac_signer = Some(Arc::new(HmacSigner::from_key(test_key.clone())));
    });

    // FOD nodes dispatch to Fetcher-kind executors.
    let mut worker_rx = connect_executor_kind(
        &handle,
        "fod-w",
        "x86_64-linux",
        rio_proto::types::ExecutorKind::Fetcher,
    )
    .await?;

    let expected_out = test_store_path("fod-expected-out");
    let mut node = make_node("fod-claims-drv");
    // Ingress-byte-bound: token-mechanics pin (see the
    // sched.dispatch.claims-derived suite for store-backed).
    node.drv_content = b"Derive-hmac-test".to_vec();
    node.is_fixed_output = true;
    node.expected_output_paths = vec![expected_out.clone()];
    merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;

    let assignment = recv_assignment(&mut worker_rx).await;
    let claims = HmacVerifier::from_key(test_key)
        .verify::<rio_auth::hmac::AssignmentClaims>(&assignment.assignment_token)
        .expect("token verifies");

    assert!(
        claims.is_fixed_output,
        "FOD assignment must sign is_fixed_output = true"
    );
    assert!(
        !claims.is_ca,
        "FOD has a known output path — is_ca must stay false"
    );
    assert_eq!(
        claims.expected_outputs,
        vec![expected_out],
        "the declared FOD path is membership-bound as usual"
    );

    Ok(())
}

/// MAX_HMAC_TIMEOUT_SECS clamp: even if build_timeout is u64::MAX,
/// the token's expiry stays bounded (≤ ~14 days from now: 7d × 2).
#[tokio::test]
async fn test_hmac_timeout_clamps_to_seven_days() -> TestResult {
    use rio_auth::hmac::{HmacSigner, HmacVerifier};

    let db = TestDb::new(&MIGRATOR).await;
    let test_key = b"test-clamp-key-at-least-32-bytes!!".to_vec();

    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |_, p| {
        p.hmac_signer = Some(Arc::new(HmacSigner::from_key(test_key.clone())));
    });

    let mut worker_rx = connect_executor(&handle, "clamp-w", "x86_64-linux").await?;

    // Merge with build_timeout = u64::MAX.
    let _ = merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id: Uuid::new_v4(),
            ingress_stripped: Default::default(),
            tenant_id: None,
            priority_class: PriorityClass::Scheduled,
            nodes: vec![{
                // Ingress-byte-bound: token-mechanics pin (see the
                // sched.dispatch.claims-derived suite for store-backed).
                let mut n = make_node("clamp-drv");
                n.drv_content = b"Derive-hmac-test".to_vec();
                n
            }],
            edges: vec![],
            options: BuildOptions {
                build_timeout: u64::MAX,
                ..Default::default()
            },
            keep_going: false,
            traceparent: String::new(),
            jti: None,
            jwt_token: None,
        },
    )
    .await?;

    let assignment = recv_assignment(&mut worker_rx).await;

    let claims = HmacVerifier::from_key(test_key)
        .verify::<rio_auth::hmac::AssignmentClaims>(&assignment.assignment_token)
        .expect("token verifies");

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // 7 days × 2 = 14 days max. Allow 15 for clock skew tolerance.
    let max_expected = now + 15 * 86400;
    assert!(
        claims.expiry_unix < max_expected,
        "expiry {} should be clamped (< {}), not year 584942417355",
        claims.expiry_unix,
        max_expected
    );

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
                ingress_stripped: Default::default(),
                tenant_id: None,
                priority_class: PriorityClass::Scheduled,
                nodes: vec![make_node("orphan-drv")],
                edges: vec![],
                options: BuildOptions::default(),
                keep_going: false,
                traceparent: String::new(),
                jti: None,
                jwt_token: None,
            },
            reply: reply_tx,
        })
        .await?;
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

// r[verify sched.backpressure.hysteresis]
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

// ---------------------------------------------------------------------------
// Token-aware shutdown
// ---------------------------------------------------------------------------

/// Cancelling the shutdown token drains `workers` and exits the actor
/// loop. The worker's `stream_rx` closes (receives None), proving the
/// `stream_tx` senders were dropped. This is the cascade that unblocks
/// tonic's `serve_with_shutdown` — without it, open bidi streams keep
/// the server waiting past `systemctl stop`'s timeout → SIGKILL →
/// no atexit → no LLVM profraw.
#[tokio::test]
async fn test_shutdown_token_drains_workers() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let token = rio_common::signal::Token::new();
    let (handle, task) = setup_actor_configured(db.pool.clone(), None, {
        let token = token.clone();
        |_, p| p.shutdown = token
    });

    // Connect a worker — gives the actor a stream_tx to drop. Then
    // query workers: the reply arrives AFTER ExecutorConnected is
    // processed (same mpsc queue, FIFO), so the stream_tx is in
    // self.executors when we cancel — the test exercises workers.clear()
    // specifically, not just "rx drops when the loop breaks".
    let mut stream_rx = connect_executor(&handle, "sd-worker", "x86_64-linux").await?;
    let workers = handle.debug_query_workers().await?;
    assert_eq!(workers.len(), 1, "worker should be registered");

    // Cancel. biased select! sees this first; workers.clear() drops
    // stream_tx.
    token.cancel();

    // stream_rx.recv() returns None once all senders (just the actor's
    // stream_tx) drop. Timeout: if the actor didn't drain, this hangs.
    let closed = tokio::time::timeout(Duration::from_secs(5), stream_rx.recv())
        .await
        .expect("stream should close within 5s of token cancel");
    assert!(
        closed.is_none(),
        "stream_rx should close (None) after drain"
    );

    // Actor loop broke → task joinable. Drop the handle so the
    // mpsc::Sender drops → rx.recv() also returns None if the select!
    // happens to poll the rx arm first (race, but biased mitigates).
    drop(handle);
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("actor task should join within 5s")
        .expect("actor task should not panic");

    Ok(())
}

// ---------------------------------------------------------------------------
// Spawn-intent snapshot (D5)
// ---------------------------------------------------------------------------

use crate::actor::SpawnIntentsRequest;

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
// r[verify sched.dispatch.soft-features]
#[tokio::test]
async fn spawn_intents_soft_features_strip() {
    let db = TestDb::new(&MIGRATOR).await;
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
// r[verify sched.dispatch.soft-features]
#[tokio::test]
async fn apply_soft_features_re_derives_effective_features() {
    let db = TestDb::new(&MIGRATOR).await;
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

// r[verify sched.sla.reactive-floor+2]
/// D4: `solve_intent_for` clamps its solved (mem, disk) at
/// `resource_floor`. A derivation with `floor.mem=32GiB` (from prior
/// `bump_floor_or_count` cycles) gets a SpawnIntent with mem ≥ 32GiB
/// even when the SLA solve would return less.
#[tokio::test]
async fn solve_intent_for_clamps_at_resource_floor() {
    let db = TestDb::new(&MIGRATOR).await;
    // `bare_actor_sla`: realistic ceilings (256 GiB > 32 GiB floor). The
    // chokepoint applies `.max(floor).min(ceil)`; `bump_floor_or_count`
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

    // r[verify sched.sla.reactive-floor+2]
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
/// loop can't form (no heartbeat → no `recently_disconnected` → no
/// `bump_floor_or_count`).
#[tokio::test]
async fn solve_intent_for_subsecond_fit_floored_at_probe_deadline() {
    use crate::sla::types::*;
    let db = TestDb::new(&MIGRATOR).await;
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

/// `handle_inspect_build_dag` cross-references derivation state
/// against the live executor stream pool. The I-025 signal:
/// `executor_has_stream` is true iff the assigned executor's gRPC
/// bidi stream is present in `self.executors`.
// r[verify sched.admin.inspect-dag]
#[tokio::test]
async fn inspect_build_dag_cross_references_stream_pool() -> TestResult {
    let (_db, handle, _task, mut stream_rx) = setup_with_worker("w-idiag", "x86_64-linux").await?;

    let build_id = Uuid::new_v4();
    let node = make_node("idiag-drv");
    let _events = merge_dag(&handle, build_id, vec![node], vec![], false).await?;
    // dispatch_ready ran inside merge → drain the assignment so the
    // worker stream stays unblocked.
    let _ = stream_rx.try_recv();

    let (diags, live) = handle
        .query_unchecked(|reply| {
            ActorCommand::Admin(AdminQuery::InspectBuildDag { build_id, reply })
        })
        .await?;
    assert_eq!(diags.len(), 1, "one derivation in build");
    let d = &diags[0];
    assert_eq!(d.assigned_executor, "w-idiag");
    assert!(
        d.executor_has_stream,
        "live worker → executor_has_stream=true"
    );
    assert!(live.contains(&"w-idiag".to_string()));

    // Drop the executor entry (simulates dead bidi-stream;
    // ExecutorDisconnected also resets the drv but here we only care
    // that the cross-ref turns false for the snapshot taken BEFORE
    // any reconciliation reassigns it).
    handle
        .send_unchecked(ActorCommand::ExecutorDisconnected {
            executor_id: "w-idiag".into(),
            stream_epoch: stream_epoch_for("w-idiag"),
            seen_drvs: vec![],
        })
        .await?;
    let (_, live_after) = handle
        .query_unchecked(|reply| {
            ActorCommand::Admin(AdminQuery::InspectBuildDag { build_id, reply })
        })
        .await?;
    assert!(
        !live_after.contains(&"w-idiag".to_string()),
        "executor map dropped after disconnect"
    );
    Ok(())
}

/// I-107: `queued_by_system` is a per-system breakdown of
/// `queued_derivations` — Ready-only, sum across keys equals the
/// scalar. Non-Ready (Queued/Assigned/Running) drvs do NOT count.
#[tokio::test]
async fn cluster_snapshot_queued_by_system_sums_to_scalar() {
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor(db.pool.clone());

    // 3 Ready x86_64, 1 Ready aarch64. test_inject_ready only puts the
    // node in the DAG; push_ready() also adds it to ready_queue so the
    // scalar (= ready_queue.len()) and the DAG-derived breakdown agree
    // — same as the production merge/transition path does.
    for (h, sys) in [
        ("x1", "x86_64-linux"),
        ("x2", "x86_64-linux"),
        ("x3", "x86_64-linux"),
        ("a1", "aarch64-linux"),
    ] {
        actor.test_inject_ready(h, None, sys, false);
        actor.push_ready(h.to_string().into());
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

/// `substituting_derivations` counts DAG nodes in `Substituting` and is
/// disjoint from queued/running. Regression: the previous `_ => {}`
/// match arm dropped Substituting on the floor → ComponentScaler saw
/// `builders=0` during a substitution cascade and scaled the store
/// DOWN exactly when it was the bottleneck. The match is now
/// exhaustive over `DerivationStatus` so a future variant addition is
/// a compile-time break, not a silently-zero autoscaler input.
// r[verify sched.admin.snapshot-substituting]
#[tokio::test]
async fn snapshot_counts_substituting() {
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor(db.pool.clone());

    // 3 Substituting, 1 Ready, 1 Running — disjoint counts.
    for h in ["s1", "s2", "s3"] {
        actor.test_inject_ready(h, None, "x86_64-linux", false);
        actor
            .dag
            .node_mut(h)
            .unwrap()
            .set_status_for_test(DerivationStatus::Substituting);
    }
    actor.test_inject_ready("q1", None, "x86_64-linux", false);
    actor.push_ready("q1".to_string().into());
    actor.test_inject_ready("r1", None, "x86_64-linux", false);
    actor
        .dag
        .node_mut("r1")
        .unwrap()
        .set_status_for_test(DerivationStatus::Running);

    let snap = actor.compute_cluster_snapshot();

    assert_eq!(snap.substituting_derivations, 3);
    assert_eq!(snap.queued_derivations, 1, "Substituting is NOT queued");
    assert_eq!(snap.running_derivations, 1, "Substituting is NOT running");
    assert_eq!(
        snap.queued_by_system.values().sum::<u32>(),
        1,
        "Substituting does NOT enter queued_by_system"
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
// r[verify sched.sla.forecast.one-layer]
#[tokio::test]
async fn forecast_frontier_one_layer_only() {
    let db = TestDb::new(&MIGRATOR).await;
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
// r[verify sched.sla.forecast.one-layer]
#[tokio::test]
async fn eta_is_remaining_not_total() {
    let db = TestDb::new(&MIGRATOR).await;
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

/// §Threat-model gap (d): `max_forecast_cores_per_tenant` debited by
/// Ready cores BEFORE forecast intents are admitted. A tenant whose
/// Ready frontier already consumes ≥ the cap emits zero forecast
/// intents — its layer-2 fanout cannot capture shared `maxFleetCores`
/// ahead of other tenants' Ready work.
// r[verify sched.sla.forecast.tenant-ceiling]
#[tokio::test]
async fn forecast_tenant_ceiling_subtracts_ready_first() {
    let db = TestDb::new(&MIGRATOR).await;
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

/// `lead_time_seed` empty → `max_lead = 0` → forecast pass disabled.
/// Deploys without `xtask k8s probe-boot` seeding stay on the v1.0
/// Ready-only path.
// r[verify sched.sla.forecast.one-layer]
#[tokio::test]
async fn forecast_disabled_on_empty_lead_time_seed() {
    let db = TestDb::new(&MIGRATOR).await;
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
// r[verify sched.admin.spawn-intents]
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

    // Connect a worker. Heartbeat triggers dispatch_ready → one
    // derivation moves to Assigned (one build per pod) → drops out of
    // the intent stream.
    let (tx, mut rx) = mpsc::channel(16);
    handle
        .send_unchecked(ActorCommand::ExecutorConnected {
            executor_id: "w0".into(),
            stream_tx: tx,
            stream_epoch: next_stream_epoch_for("w0"),
            auth_intent: None,
            reply: noop_connect_reply(),
        })
        .await?;
    send_heartbeat_with(&handle, "w0", "x86_64-linux", |_| {}).await?;
    handle.send_unchecked(ActorCommand::Tick).await?;
    let _assignment = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("assignment within 5s")
        .expect("assignment not dropped");

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

// r[verify obs.metric.scheduler]
/// Mailbox-depth gauge is set on every dequeued command. Send a Tick,
/// barrier (request-reply, also dequeued), and assert the gauge was
/// touched. Value is non-deterministic (depends on how many commands
/// were queued at sample time) — touch-set assertion only.
#[tokio::test]
async fn test_mailbox_depth_gauge_set_per_command() -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let db = TestDb::new(&MIGRATOR).await;
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

// r[verify obs.metric.scheduler]
/// dispatch_wait_seconds is recorded on Ready→Assigned. Connect a
/// worker, merge a single-node DAG (enters Ready immediately — no
/// deps), wait for the assignment to land, then assert the histogram
/// was touched. Elapsed value is non-deterministic; touch-set only.
#[tokio::test]
async fn test_dispatch_wait_recorded_on_assignment() -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone());

    let mut rx = connect_executor(&handle, "dw-worker", "x86_64-linux").await?;
    merge_single_node(&handle, Uuid::new_v4(), "dw-drv", PriorityClass::Scheduled).await?;

    // MergeDag's reply is sent AFTER dispatch_ready runs inline
    // (helpers.rs:624), so the assignment is already in flight. Drain
    // it to confirm assign_to_worker actually ran.
    let _assignment = recv_assignment(&mut rx).await;

    assert!(
        recorder.histogram_touched("rio_scheduler_dispatch_wait_seconds"),
        "dispatch_wait_seconds not recorded on Ready→Assigned"
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
    let mut actor = bare_actor(db.pool.clone());

    actor.recently_disconnected.insert(
        "stale-exec".into(),
        ("stale".into(), std::time::Instant::now()),
    );
    actor
        .hung_nodes
        .insert("nA".into(), std::time::Instant::now());
    actor.authoritative_binding.insert(
        "stale-drv".into(),
        crate::actor::AuthBinding {
            node: "nA".into(),
            tenant: None,
        },
    );

    actor.clear_persisted_state();

    assert!(
        actor.recently_disconnected.is_empty(),
        "recently_disconnected must be cleared on leader transition"
    );
    assert!(
        actor.hung_nodes.is_empty(),
        "hung_nodes (tick-derived) must be cleared on leader transition"
    );
    assert!(
        actor.authoritative_binding.is_empty(),
        "authoritative_binding (controller-reported per-generation) must be cleared"
    );
    // Regression: soft_features survives (existing :649 invariant).
    actor.test_inject_ready_with_features("ff", None, "x86_64-linux", &["big-parallel"]);
}

// ---------------------------------------------------------------------------
// BuildEventBus: Log seq + try_log_flush Closed-vs-Full
// ---------------------------------------------------------------------------

/// `Event::Log` is not persisted, so it MUST NOT consume a sequence
/// number — otherwise the in-memory counter diverges from PG
/// `MAX(sequence)` and the `since_sequence < last_seq` replay guard
/// misfires after failover.
#[tokio::test]
async fn log_events_do_not_consume_sequence() {
    use crate::actor::event::BuildEventBus;
    use rio_proto::types::build_event::Event;
    let mut bus = BuildEventBus::new(None, None);
    let build_id = Uuid::new_v4();
    let _rx = bus.register(build_id);

    bus.emit(
        build_id,
        Event::Started(rio_proto::types::BuildStarted::default()),
    );
    assert_eq!(bus.last_seq(build_id), 1);
    for _ in 0..50 {
        bus.emit(
            build_id,
            Event::Log(rio_proto::types::BuildLogBatch::default()),
        );
    }
    assert_eq!(bus.last_seq(build_id), 1, "Log must not consume seq");
    bus.emit(
        build_id,
        Event::Derivation(rio_proto::types::DerivationEvent::default()),
    );
    assert_eq!(
        bus.last_seq(build_id),
        2,
        "next persisted event gets seq=2, not 52"
    );
}

/// `try_log_flush` MUST NOT warn/count when the receiver is dropped
/// (Closed) — only when Full. A dead flusher task is signalled by
/// `spawn_monitored`; spamming "channel full ... periodic tick will
/// snapshot" is doubly misleading (it's Closed, and the tick lives in
/// the dead task).
#[tokio::test]
#[traced_test]
async fn try_log_flush_silent_on_closed() {
    use crate::actor::event::BuildEventBus;
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let (tx, rx) = tokio::sync::mpsc::channel(1);
    drop(rx); // flusher died
    let bus = BuildEventBus::new(None, Some(tx));
    assert!(
        !bus.try_log_flush(crate::logs::FlushRequest {
            drv_path: "x".into(),
            exec_id: uuid::Uuid::now_v7(),
            status: None,
            lease_generation: 1,
        }),
        "Closed must report not-enqueued"
    );

    assert_eq!(
        recorder.get("rio_scheduler_log_flush_dropped_total{}"),
        0,
        "Closed must not increment dropped_total"
    );
    assert!(
        !logs_contain("log flush channel full"),
        "Closed must not warn 'channel full'"
    );
}

/// The terminal epilogue's empty-buffer reap (bug_008) keys on this
/// return value: `true` iff the request was handed to a live flusher.
#[tokio::test]
async fn try_log_flush_reports_enqueue_result() {
    use crate::actor::event::BuildEventBus;
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);
    let mk_req = || crate::logs::FlushRequest {
        drv_path: "x".into(),
        exec_id: uuid::Uuid::now_v7(),
        status: None,
        lease_generation: 1,
    };

    // No flusher configured → false.
    let no_flusher = BuildEventBus::new(None, None);
    assert!(
        !no_flusher.try_log_flush(mk_req()),
        "no flusher must report not-enqueued"
    );

    // Live channel with capacity → true, and the request is actually there.
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let bus = BuildEventBus::new(None, Some(tx));
    assert!(
        bus.try_log_flush(mk_req()),
        "successful enqueue must report true"
    );
    assert!(
        rx.try_recv().is_ok(),
        "the enqueued request reached the channel"
    );

    // Full → false, and the existing dropped_total contract still holds.
    assert!(bus.try_log_flush(mk_req()), "refill the single slot");
    assert!(
        !bus.try_log_flush(mk_req()),
        "Full must report not-enqueued"
    );
    assert_eq!(
        recorder.get("rio_scheduler_log_flush_dropped_total{}"),
        1,
        "Full still increments dropped_total exactly once"
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
// r[verify sched.tenant.authz+2]
#[tokio::test]
async fn watch_build_missing_returns_not_found_for_tenant() {
    let db = TestDb::new(&MIGRATOR).await;
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

// ===========================================================================
// Dispatch claims derivation (sched.dispatch.claims-derived)
// ===========================================================================

/// Signer + mock store for the claims-derivation suite. Backoff base is
/// zeroed so the unavailable→retry path can be driven with Ticks.
async fn setup_claims_fixture(
    test_key: &[u8],
) -> anyhow::Result<(
    TestDb,
    rio_test_support::grpc::MockStore,
    ActorHandle,
    (tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>),
)> {
    use rio_auth::hmac::HmacSigner;
    let db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let key = test_key.to_vec();
    let (handle, actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |c, p| {
            c.retry_policy.backoff_base_secs = 0.0;
            p.hmac_signer = Some(Arc::new(HmacSigner::from_key(key)));
        });
    Ok((db, store, handle, (store_task, actor_task)))
}

/// Drain the worker stream until an Assignment arrives (skipping
/// Prefetch hints), or `None` after `wait_ms` of silence.
async fn try_recv_assignment(
    rx: &mut tokio::sync::mpsc::Receiver<rio_proto::types::SchedulerMessage>,
    wait_ms: u64,
) -> Option<rio_proto::types::WorkAssignment> {
    loop {
        match tokio::time::timeout(Duration::from_millis(wait_ms), rx.recv()).await {
            Err(_) => return None,
            Ok(None) => return None,
            Ok(Some(msg)) => match msg.msg {
                Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) => return Some(a),
                Some(rio_proto::types::scheduler_message::Msg::Prefetch(_)) => continue,
                _ => continue,
            },
        }
    }
}

// r[verify sched.dispatch.claims-derived+3]
/// Happy path: a bare store-backed node whose `.drv` is in the store
/// gets its claims PROVEN against the store bytes — the token verifies
/// with the derived (== recorded, now byte-bound) values, the verified
/// bytes are forwarded to the worker, and the node's standing is
/// persisted as `path_bound_bytes` so re-dispatch skips the re-fetch.
#[tokio::test]
async fn test_dispatch_claims_derived_from_store_bytes() -> TestResult {
    use rio_auth::hmac::HmacVerifier;
    let test_key = b"test-scheduler-hmac-key-32bytes!".to_vec();
    let (db, store, handle, _tasks) = setup_claims_fixture(&test_key).await?;

    let (node, aterm, out_path) = mint_text_ca_leaf("claims-happy");
    let drv_path = node.drv_path.clone();
    store.seed_with_content(&drv_path, aterm.as_bytes());

    let mut worker_rx = connect_executor(&handle, "claims-w", "x86_64-linux").await?;
    merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;

    let assignment = recv_assignment(&mut worker_rx).await;
    assert_eq!(assignment.drv_path, drv_path);
    assert_eq!(
        assignment.drv_content,
        aterm.as_bytes(),
        "the store-verified bytes are forwarded as the build instructions"
    );
    let claims = HmacVerifier::from_key(test_key)
        .verify::<rio_auth::hmac::AssignmentClaims>(&assignment.assignment_token)
        .expect("token verifies");
    assert_eq!(
        claims.expected_outputs,
        vec![out_path],
        "signed expected_outputs are the byte-derived paths"
    );
    assert!(!claims.is_ca && !claims.is_fixed_output);

    let (rank,): (String,) =
        sqlx::query_as("SELECT evidence_rank FROM derivations WHERE drv_hash = $1")
            .bind(&drv_path)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(rank, "path_bound_bytes", "verification persisted");
    Ok(())
}

/// Round-17 bug_030 population cell: a (1,16] MiB `.drv` — too big to
/// arrive inline (`MAX_DRV_CONTENT_BYTES` = 1 MiB caps that path), so
/// necessarily a BARE store-backed node — must pass claims
/// verification through the store fetch. Pre-fix, the dispatch-side
/// fetch carried a private 1 MiB cap while store admission, gateway
/// BFS, and the worker fetch all admit 16 MiB
/// (`rio_common::limits::MAX_DRV_NAR_BYTES`): this exact node was
/// admitted everywhere else, then deterministically failed the fetch
/// → `StoreSilence` → transient backoff → poison blaming store
/// health. The cell pins the shared-cap behavior end to end: bytes
/// verified, forwarded, and the byte-bound rank persisted.
// r[verify sched.dispatch.claims-derived+3]
#[tokio::test]
async fn test_dispatch_claims_verifies_multi_mib_drv() -> TestResult {
    use rio_auth::hmac::HmacVerifier;
    let test_key = b"test-scheduler-hmac-key-32bytes!".to_vec();
    let (db, store, handle, _tasks) = setup_claims_fixture(&test_key).await?;

    // 2 MiB of env padding: comfortably inside the 16 MiB class cap,
    // comfortably outside the old private 1 MiB one.
    let (node, aterm, out_path) = mint_text_ca_leaf_padded("claims-2mib", 2 * 1024 * 1024);
    assert!(
        aterm.len() > rio_common::limits::MAX_DRV_CONTENT_BYTES,
        "fixture must exceed the inline cap (else the cell tests nothing)"
    );
    assert!(
        (aterm.len() as u64) < rio_common::limits::MAX_DRV_NAR_BYTES,
        "fixture must remain inside the derivation-text class cap"
    );
    let drv_path = node.drv_path.clone();
    store.seed_with_content(&drv_path, aterm.as_bytes());

    let mut worker_rx = connect_executor(&handle, "claims-2mib-w", "x86_64-linux").await?;
    merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;

    let assignment = recv_assignment(&mut worker_rx).await;
    assert_eq!(assignment.drv_path, drv_path);
    assert_eq!(
        assignment.drv_content,
        aterm.as_bytes(),
        "the multi-MiB store-verified bytes are forwarded as the build instructions"
    );
    let claims = HmacVerifier::from_key(test_key)
        .verify::<rio_auth::hmac::AssignmentClaims>(&assignment.assignment_token)
        .expect("token verifies");
    assert_eq!(claims.expected_outputs, vec![out_path]);

    let (rank,): (String,) =
        sqlx::query_as("SELECT evidence_rank FROM derivations WHERE drv_hash = $1")
            .bind(&drv_path)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        rank, "path_bound_bytes",
        "multi-MiB .drv reaches byte-bound rank — not StoreSilence backoff"
    );
    Ok(())
}

/// Round-16 bug_094: a deferred-IA node's dispatch-time resolution
/// must record its computed REAL paths as the claim
/// (`set_claim_output_paths`) — the HMAC `expected_outputs` site reads
/// `claim_output_paths()` — while `expected_output_paths` keeps its
/// INGRESS shape (the empty slot). Pre-fix, dispatch overwrote the
/// expected slots, which destroyed the emptiness signal the
/// byte-derived resolve probe (`child_unknown`, merge.rs) reads off
/// resident children: a bare store-backed FOD parent dispatching after
/// this child built recorded a sticky `needs_resolve=false` at its
/// PathBoundBytes raise and then failed deterministically on the
/// un-rewritten placeholder until poison. (Unsigned fixture: the claim
/// values are asserted via the debug surface, which exposes exactly
/// what the HMAC site signs — `claim_output_paths()`; the signed-token
/// mechanics are pinned by the claims suite above.)
#[tokio::test]
async fn test_deferred_ia_resolve_claims_real_path_and_preserves_ingress_shape() -> TestResult {
    use crate::ca::resolve::downstream_placeholder;
    use rio_nix::store_path::StorePath;
    let (db, _store, handle, _tasks) = setup_with_mock_store().await?;

    // Child: floating-CA leaf with a known modular hash.
    let child_modular: [u8; 32] = [0x11; 32];
    let mut child = make_node("dia-child");
    child.is_content_addressed = true;
    child.needs_resolve = true;
    child.ca_modular_hash = child_modular.to_vec();
    let child_drv_path = child.drv_path.clone();

    // Parent: deferred-IA — IA drv whose own output path is unknown
    // until its floating input resolves. Ingress shape: ONE empty
    // expected slot. Minted store-shaped: canonical ATerm with the
    // deferred output ("out","","","") and the child placeholder in
    // env, drv_path = the bytes' text content-address (the dispatch
    // claims gate re-derives and compares it), bare submission (the
    // gate fetches the seeded store bytes).
    let placeholder = downstream_placeholder(&StorePath::parse(&child_drv_path).unwrap(), "out");
    let parent_aterm = format!(
        r#"Derive([("out","","","")],[("{child_drv_path}",["out"])],[],"x86_64-linux","/bin/sh",["-c","build"],[("DEP","{placeholder}"),("name","dia-parent"),("out",""),("system","x86_64-linux")])"#
    );
    {
        use rio_nix::derivation::Derivation;
        let drv = Derivation::parse(&parent_aterm).expect("fixture parses");
        assert_eq!(drv.to_aterm(), parent_aterm, "fixture must be canonical");
    }
    let parent_path = {
        use rio_nix::hash::{HashAlgo, NixHash};
        use sha2::{Digest, Sha256};
        let h = NixHash::new(
            HashAlgo::SHA256,
            Sha256::digest(parent_aterm.as_bytes()).to_vec(),
        )
        .unwrap();
        StorePath::make_text(
            "dia-parent.drv",
            &h,
            &[StorePath::parse(&child_drv_path).unwrap()],
        )
        .unwrap()
        .as_str()
        .to_owned()
    };
    let parent = rio_proto::types::DerivationNode {
        drv_path: parent_path.clone(),
        drv_hash: parent_path.clone(),
        pname: "dia-parent".into(),
        system: "x86_64-linux".into(),
        output_names: vec!["out".into()],
        is_fixed_output: false,
        is_content_addressed: false,
        needs_resolve: true,
        expected_output_paths: vec![String::new()],
        ..Default::default()
    };
    _store.seed_with_content(&parent_path, parent_aterm.as_bytes());

    let mut worker_rx = connect_executor(&handle, "dia-w", "x86_64-linux").await?;
    merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![child, parent],
        vec![rio_proto::types::DerivationEdge {
            parent_drv_path: parent_path.clone(),
            child_drv_path: child_drv_path.clone(),
        }],
        false,
    )
    .await?;

    // Child dispatches first (leaf). Seed its realisation AFTER the
    // merge (so it does not cache-hit), then complete it — the parent
    // becomes Ready and resolves at dispatch.
    let a1 = recv_assignment(&mut worker_rx).await;
    assert!(a1.drv_path.contains("dia-child"), "child dispatches first");
    let realized = test_store_path("dia-child-realized-out");
    sqlx::query(
        "INSERT INTO realisations (drv_hash, output_name, output_path, output_hash)
         VALUES ($1, 'out', $2, $3)",
    )
    .bind(child_modular.as_slice())
    .bind(&realized)
    .bind([0x33u8; 32].as_slice())
    .execute(&db.pool)
    .await?;
    complete_success(&handle, "dia-w", "dia-child", &realized).await?;

    // The first executor drains after its completion (one-shot worker
    // semantics) — connect a fresh one for the parent, like the
    // recovered CA-on-CA dispatch test.
    let mut worker_rx2 = connect_executor(&handle, "dia-w2", "x86_64-linux").await?;
    let assignment = recv_assignment(&mut worker_rx2).await;
    assert_eq!(assignment.drv_path, parent_path);
    // Resolution actually happened: the forwarded bytes carry the
    // realized input path, not the placeholder.
    let sent = String::from_utf8(assignment.drv_content.clone())?;
    assert!(
        sent.contains(&realized) && !sent.contains(&placeholder),
        "parent must dispatch with the placeholder rewritten"
    );

    let info = expect_drv(&handle, &parent_path).await;
    // The claim carries the RESOLVED real path, not the ingress "".
    assert_eq!(info.claim_output_paths.len(), 1);
    let claimed = &info.claim_output_paths[0];
    assert!(
        claimed.starts_with("/nix/store/") && claimed.ends_with("-dia-parent"),
        "the claim field must carry the resolved deferred-IA path; got {claimed:?}"
    );
    // The node's expected paths keep the INGRESS shape — the resolve
    // probe contract (pre-fix this read [claimed] and the emptiness
    // signal was gone).
    assert_eq!(
        info.expected_output_paths,
        vec![String::new()],
        "expected_output_paths must keep its ingress shape across dispatch"
    );
    Ok(())
}

// r[verify sched.ca.absent-hash-surfaced]
/// THE ingress-stripped node, followed end-to-end PAST ingress for the
/// first time (round-15 C3c6, bug_048 part 2). Shape provenance: a
/// warm gateway submission whose floating input is already realized
/// and store-backed — the consumer's declared modular hash is
/// unverifiable at ingress and STRIPPED
/// (`sched.merge.ingress-inline-drv-binding+1`); this test submits the
/// post-strip shape directly (inline floating node, empty
/// ca_modular_hash). It dispatches (inline = ingress-byte-bound, no
/// claims fetch), completes — and the completion-time CA bookkeeping
/// SKIPS, surfaced by the counter: NO realisation row is written.
///
/// WHEN STAGED FOLLOW-UP F2 LANDS (the verifying re-establisher /
/// ModularHashState): this test's final assertions FLIP — the
/// realisation row EXISTS and the skip counter stays zero. Do not
/// weaken either assertion before then; they are the visible record
/// of the accepted gap.
#[tokio::test]
async fn test_stripped_node_completes_without_realisation_and_surfaces() -> TestResult {
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};

    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();
    let _g = metrics::set_default_local_recorder(&rec);

    let test_key = b"test-scheduler-hmac-key-32bytes!".to_vec();
    let (db, _store, handle, _tasks) = setup_claims_fixture(&test_key).await?;

    let (mut node, aterm, _hash) = mint_floating_ca_leaf("strip-e2e");
    let drv_path = node.drv_path.clone();
    // The post-strip shape: inline bytes, floating, NO declared hash.
    node.drv_content = aterm.into_bytes();
    node.ca_modular_hash = Vec::new();

    let mut worker_rx = connect_executor(&handle, "strip-w", "x86_64-linux").await?;
    let _events = merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;

    let assignment = recv_assignment(&mut worker_rx).await;
    assert_eq!(
        assignment.drv_path, drv_path,
        "stripped inline node dispatches"
    );

    let out_path = "/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-strip-e2e-out";
    complete_success(&handle, "strip-w", &drv_path, out_path).await?;
    barrier(&handle).await;

    let probe = handle.debug_query_derivation(&drv_path).await?.unwrap();
    assert_eq!(probe.status, DerivationStatus::Completed);
    assert!(
        probe.ca.modular_hash.is_none(),
        "still stripped at completion"
    );

    // F2 FLIP POINT 1: becomes count == 1 when the re-establisher lands.
    let (n_rows,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM realisations WHERE output_path = $1")
            .bind(out_path)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(n_rows, 0, "no realisation row for the stripped node");

    // F2 FLIP POINT 2: becomes 0 when the re-establisher lands.
    let skipped: u64 = snap
        .snapshot()
        .into_vec()
        .into_iter()
        .filter(|(ck, ..)| {
            ck.key().name() == "rio_scheduler_ca_bookkeeping_skipped_total"
                && ck
                    .key()
                    .labels()
                    .any(|l| l.key() == "consumer" && l.value() == "realisation_insert")
        })
        .map(|(.., v)| match v {
            DebugValue::Counter(c) => c,
            _ => 0,
        })
        .sum();
    assert_eq!(skipped, 1, "the realisation-insert skip is surfaced");
    Ok(())
}

// r[verify sched.dispatch.claims-derived+3]
/// Forged `needs_resolve` echo cannot steer post-verification
/// dispatch: a bare store-backed deferred-IA node submitted with
/// `needs_resolve = false` (the forged echo — its bytes derive TRUE:
/// deferred type with an input) ends up with the BYTE-DERIVED flag
/// recorded at the rank raise; the resolve gate reads only that
/// recorded state.
#[tokio::test]
async fn test_dispatch_records_byte_derived_resolve_not_echo() -> TestResult {
    let test_key = b"test-scheduler-hmac-key-32bytes!".to_vec();
    let (_db, store, handle, _tasks) = setup_claims_fixture(&test_key).await?;

    let (leaf, leaf_aterm, _out) = mint_text_ca_leaf("rne-leaf");
    let leaf_path = leaf.drv_path.clone();
    let (mut mid, mid_aterm) =
        mint_deferred_ia_node("rne-mid", &leaf_path, &[(&leaf_path, &leaf_aterm)]);
    let mid_path = mid.drv_path.clone();
    // FORGE the echo: bytes say resolve (deferred + input), the
    // submitter says don't.
    mid.needs_resolve = false;
    store.seed_with_content(&mid_path, mid_aterm.as_bytes());

    let mut worker_rx = connect_executor(&handle, "rne-w", "x86_64-linux").await?;
    // Submit the mid ALONE (no DAG edges): it is a root, dispatches
    // immediately, and the claims derivation verifies its bytes.
    let _events = merge_dag(&handle, Uuid::new_v4(), vec![mid], vec![], false).await?;

    let assignment = recv_assignment(&mut worker_rx).await;
    assert_eq!(assignment.drv_path, mid_path);

    let probe = handle.debug_query_derivation(&mid_path).await?.unwrap();
    assert!(
        probe.ca.needs_resolve,
        "recorded resolve flag is the byte-derived TRUE, not the \
         forged echo FALSE: {probe:?}"
    );
    Ok(())
}

// r[verify sched.dispatch.claims-derived+3]
/// THE forged-claims kill test (merged_bug_053 variants 2/3 + the
/// needs_resolve bypass): a submitter echoes forged expected outputs
/// and a forged resolve flag for a store-backed node. The store bytes
/// disprove the claim — NO token is signed, no WorkAssignment reaches
/// the worker, the node is poisoned, and the interested build fails
/// with its evidence persisted at source.
#[tokio::test]
async fn test_dispatch_claims_forgery_poisons_without_signing() -> TestResult {
    let test_key = b"test-scheduler-hmac-key-32bytes!".to_vec();
    let (db, store, handle, _tasks) = setup_claims_fixture(&test_key).await?;

    let (mut node, aterm, _out) = mint_text_ca_leaf("claims-forged");
    let drv_path = node.drv_path.clone();
    store.seed_with_content(&drv_path, aterm.as_bytes());
    // The forgery: claim a different output path, and a resolve flag
    // that would (pre-derivation) have steered signing onto the echo.
    node.expected_output_paths = vec![test_store_path("forged-target")];
    node.needs_resolve = true;

    let mut worker_rx = connect_executor(&handle, "forge-w", "x86_64-linux").await?;
    let build_id = Uuid::new_v4();
    let _events = merge_dag(&handle, build_id, vec![node], vec![], false).await?;
    barrier(&handle).await;

    assert!(
        try_recv_assignment(&mut worker_rx, 300).await.is_none(),
        "no WorkAssignment may be sent for forged claims"
    );
    wait_for_status(&handle, &drv_path, DerivationStatus::Poisoned).await;
    // Failure evidence at source: the interested build's error_summary
    // is durable while the failure propagates.
    let (summary,): (Option<String>,) =
        sqlx::query_as("SELECT error_summary FROM builds WHERE build_id = $1")
            .bind(build_id)
            .fetch_one(&db.pool)
            .await?;
    assert!(
        summary.is_some(),
        "failure evidence persisted at source for the forged dispatch"
    );
    Ok(())
}

// r[verify sched.dispatch.claims-derived+3]
/// Store unavailability is transient: the assignment rolls back with
/// backoff (no token, node NOT poisoned), and once the `.drv` appears
/// the next dispatch derives the claims and assigns normally.
#[tokio::test]
async fn test_dispatch_claims_unavailable_backs_off_then_succeeds() -> TestResult {
    let test_key = b"test-scheduler-hmac-key-32bytes!".to_vec();
    let (_db, store, handle, _tasks) = setup_claims_fixture(&test_key).await?;

    let (node, aterm, _out) = mint_text_ca_leaf("claims-unavail");
    let drv_path = node.drv_path.clone();
    // NOT seeded yet.

    let mut worker_rx = connect_executor(&handle, "unavail-w", "x86_64-linux").await?;
    merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;
    barrier(&handle).await;

    assert!(
        try_recv_assignment(&mut worker_rx, 300).await.is_none(),
        "no assignment while the store cannot vouch"
    );
    let info = handle
        .debug_query_derivation(&drv_path)
        .await?
        .expect("node resident");
    assert_eq!(
        info.status,
        DerivationStatus::Ready,
        "unavailable is transient: rolled back to Ready, not poisoned"
    );

    // The store recovers. A fresh worker registration triggers the
    // next dispatch pass (the harness's established trigger — Tick
    // alone only dispatches when the dirty flag is set); the deferred
    // node's claims now derive and either worker may receive it.
    store.seed_with_content(&drv_path, aterm.as_bytes());
    let mut worker_rx2 = connect_executor(&handle, "unavail-w2", "x86_64-linux").await?;
    for _ in 0..50 {
        handle.send_unchecked(ActorCommand::Tick).await?;
        barrier(&handle).await;
        let got = match try_recv_assignment(&mut worker_rx, 50).await {
            Some(a) => Some(a),
            None => try_recv_assignment(&mut worker_rx2, 50).await,
        };
        if let Some(a) = got {
            assert_eq!(a.drv_path, drv_path);
            // merged_bug_010 pin: silence deferrals charge their OWN
            // budget, never the transient build budget — and the
            // Verified edge resets the silence counter.
            let info = handle
                .debug_query_derivation(&drv_path)
                .await?
                .expect("resident");
            assert_eq!(
                info.retry.count, 0,
                "transient build budget must not be consumed by store silence"
            );
            assert_eq!(
                info.retry.claims_unavailable_count, 0,
                "Verified edge resets the consecutive-silence budget"
            );
            return Ok(());
        }
    }
    panic!("assignment never arrived after the store recovered");
}

// r[verify sched.dispatch.claims-derived+3]
/// Bytes that do not re-derive the declared text content-address are
/// transport-grade noise, not evidence in either direction: the
/// assignment is held (rolled back, NOT poisoned) — never signed.
#[tokio::test]
async fn test_dispatch_claims_text_ca_mismatch_never_signs() -> TestResult {
    let test_key = b"test-scheduler-hmac-key-32bytes!".to_vec();
    let (_db, store, handle, _tasks) = setup_claims_fixture(&test_key).await?;

    let (node, _aterm, _out) = mint_text_ca_leaf("claims-mismatch");
    let drv_path = node.drv_path.clone();
    // A DIFFERENT (parseable, canonical) derivation's bytes at the
    // path: text-CA re-derivation cannot match.
    let (_other, other_aterm, _o) = mint_text_ca_leaf("claims-other");
    store.seed_with_content(&drv_path, other_aterm.as_bytes());

    let mut worker_rx = connect_executor(&handle, "mismatch-w", "x86_64-linux").await?;
    merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;
    barrier(&handle).await;

    assert!(
        try_recv_assignment(&mut worker_rx, 300).await.is_none(),
        "unverifiable bytes must never be signed against"
    );
    let info = handle
        .debug_query_derivation(&drv_path)
        .await?
        .expect("node resident");
    assert_eq!(
        info.status,
        DerivationStatus::Ready,
        "transient, not poisoned"
    );
    Ok(())
}

// r[verify sched.dispatch.claims-derived+3]
/// Text-CA-VERIFIED garbage is permanent: the bytes are content-bound
/// to the declared path (zero-reference text-CA matches) and can never
/// parse — refetching reproduces them, so the node is poisoned instead
/// of hot-looping through backoff.
#[tokio::test]
async fn test_dispatch_unparseable_verified_bytes_poison() -> TestResult {
    use rio_nix::hash::{HashAlgo, NixHash};
    use rio_nix::store_path::StorePath;
    use sha2::{Digest, Sha256};

    let test_key = b"test-scheduler-hmac-key-32bytes!".to_vec();
    let (_db, store, handle, _tasks) = setup_claims_fixture(&test_key).await?;

    let garbage = b"this is not a derivation";
    let h = NixHash::new(HashAlgo::SHA256, Sha256::digest(garbage).to_vec()).unwrap();
    let garbage_path = StorePath::make_text("garbage.drv", &h, &[])
        .unwrap()
        .as_str()
        .to_owned();
    store.seed_with_content(&garbage_path, garbage);

    let (mut node, _aterm, _out) = mint_text_ca_leaf("claims-garbage");
    node.drv_path = garbage_path.clone();
    node.drv_hash = garbage_path.clone();

    let mut worker_rx = connect_executor(&handle, "garbage-w", "x86_64-linux").await?;
    merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;
    barrier(&handle).await;

    assert!(
        try_recv_assignment(&mut worker_rx, 300).await.is_none(),
        "no assignment for content-bound garbage"
    );
    wait_for_status(&handle, &garbage_path, DerivationStatus::Poisoned).await;
    Ok(())
}

// r[verify sched.merge.input-form-seed]
/// The seed constructors own the not-floating predicate: a floating-CA
/// node's declared (published, masked) hash never enters the seed map;
/// IA and FOD nodes' hashes do.
#[test]
fn input_form_seed_constructor_excludes_floating_published_hashes() {
    let mk = |path: &str, is_ca: bool, fod: bool| crate::domain::DerivationNode {
        drv_hash: path.to_owned(),
        drv_path: path.to_owned(),
        pname: String::new(),
        system: "x86_64-linux".into(),
        output_names: vec!["out".into()],
        expected_output_paths: vec![String::new()],
        is_fixed_output: fod,
        is_content_addressed: is_ca,
        ca_modular_hash: Some([7u8; 32]),
        ca_modular_hash_stripped: None,
        drv_content: Vec::new(),
        drv_content_authoritative: false,
        required_features: Vec::new(),
        wanted_output_names: Vec::new(),
        explicitly_requested: false,
        needs_resolve: false,
        version: None,
        enable_parallel_building: None,
        enable_parallel_checking: None,
        prefer_local_build: None,
    };
    let nodes = vec![
        mk("/nix/store/ia.drv", false, false),
        mk("/nix/store/fod.drv", true, true),
        mk("/nix/store/floating.drv", true, false),
    ];
    let seed = crate::actor::merge::InputFormSeed::from_submission_nodes(&nodes);
    assert!(
        seed.get("/nix/store/ia.drv").is_some(),
        "IA hash is an input-form digest"
    );
    assert!(
        seed.get("/nix/store/fod.drv").is_some(),
        "FOD hash is an input-form digest (mask never applies)"
    );
    assert!(
        seed.get("/nix/store/floating.drv").is_none(),
        "a floating node's published hash is the masked form and must \
         never seed input position"
    );
}

// r[verify sched.merge.input-form-seed]
/// merged_bug_003 e2e (the dispatch-time 4th site, fix-child of
/// e2c2dbfc2 / pattern R2): a bare IA parent whose Completed
/// floating-CA child carries its published (masked) hash must NOT have
/// that hash seeded into the dispatch-time claims verification. Pre-fix
/// the raw children loop seeded it, the validator derived the parent's
/// IA paths from a masked digest, the honest declared paths "differed",
/// and the node was wrongfully poisoned as FORGED (a hostile-submitter
/// verdict against an honest victim). Post-fix the child is excluded
/// and the input is UNSEEDED — under the claims-derived+3 permanence
/// contract that defers on the bounded unseeded budget (the floating
/// child's row is not seedable either: the not-floating predicate is
/// uniform across seed sources) and converges to the budget-exhausted
/// poison carrying the post-read-through remediation, NOT a forgery:
/// the failure class is the discriminator.
#[tokio::test]
async fn test_dispatch_floating_child_masked_hash_not_treated_as_forged() -> TestResult {
    use rio_nix::derivation::{Derivation, input_addressed_output_paths};
    use rio_nix::hash::{HashAlgo, NixHash};
    use rio_nix::store_path::StorePath;
    use sha2::{Digest, Sha256};
    use std::collections::HashMap;

    let test_key = b"test-scheduler-hmac-key-32bytes!".to_vec();
    let (_db, store, handle, _tasks) = setup_claims_fixture(&test_key).await?;

    // Floating child, resident + Completed, carrying its PUBLISHED
    // (masked) modular hash — the gateway warm shape.
    let (child_node, child_aterm, _published) = mint_floating_ca_leaf("ifs-child");
    let child_path = child_node.drv_path.clone();
    let child_drv = Derivation::parse(&child_aterm).expect("child parses");
    merge_dag(&handle, Uuid::new_v4(), vec![child_node], vec![], false).await?;
    assert!(
        handle
            .debug_force_status(&child_path, DerivationStatus::Completed)
            .await?,
        "child forced Completed"
    );

    // Honest IA parent over the floating child: declared output paths
    // derived through the real input-form recursion (the resolver
    // computes the child's UNMASKED input digest internally).
    let build_parent = |out: &str| {
        format!(
            r#"Derive([("out","{out}","","")],[("{child_path}",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo hi"],[("name","ifs-parent"),("out","{out}")])"#
        )
    };
    let masked_parent = Derivation::parse(&build_parent("")).expect("masked parent parses");
    let name_only = format!("/nix/store/{}-ifs-parent.drv", "a".repeat(32));
    let resolver = |p: &str| -> Option<&Derivation> { (p == child_path).then_some(&child_drv) };
    let paths =
        input_addressed_output_paths(&masked_parent, &name_only, &resolver, &mut HashMap::new())
            .expect("derive honest parent paths");
    let parent_out = paths["out"].as_str().to_owned();
    let parent_aterm = build_parent(&parent_out);
    let parent_drv = Derivation::parse(&parent_aterm).expect("parent parses");
    assert_eq!(parent_drv.to_aterm(), parent_aterm, "parent canonical");
    let content_hash = NixHash::new(
        HashAlgo::SHA256,
        Sha256::digest(parent_aterm.as_bytes()).to_vec(),
    )
    .unwrap();
    let parent_drv_path = StorePath::make_text(
        "ifs-parent.drv",
        &content_hash,
        &[StorePath::parse(&child_path).unwrap()],
    )
    .unwrap()
    .as_str()
    .to_owned();
    store.seed_with_content(&parent_drv_path, parent_aterm.as_bytes());

    let parent_node = rio_proto::types::DerivationNode {
        drv_path: parent_drv_path.clone(),
        drv_hash: parent_drv_path.clone(),
        pname: "ifs-parent".into(),
        system: "x86_64-linux".into(),
        output_names: vec!["out".into()],
        is_fixed_output: false,
        is_content_addressed: false,
        expected_output_paths: vec![parent_out],
        ..Default::default()
    };
    let edge = rio_proto::types::DerivationEdge {
        parent_drv_path: parent_drv_path.clone(),
        child_drv_path: child_path.clone(),
    };

    let mut worker_rx = connect_executor(&handle, "ifs-w", "x86_64-linux").await?;
    let mut events = merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![parent_node],
        vec![edge],
        false,
    )
    .await?;
    barrier(&handle).await;

    assert!(
        try_recv_assignment(&mut worker_rx, 300).await.is_none(),
        "the masked child hash must not be signed against"
    );
    // Post-claims-derived+3 the unseeded parent DEFERS on its bounded
    // budget (cap = max_infra_retries) before the visible poison;
    // each heartbeat chains a Tick that drives another dispatch pass.
    let mut poisoned = false;
    for _ in 0..300 {
        send_heartbeat(&handle, "ifs-w", "x86_64-linux").await?;
        barrier(&handle).await;
        if let Ok(Some(d)) = handle.debug_query_derivation(&parent_drv_path).await
            && d.status == DerivationStatus::Poisoned
        {
            poisoned = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(poisoned, "unseeded budget must converge to visible poison");
    // The DISCRIMINATOR: the failure must be the unseeded-input class
    // naming the excluded child — NEVER the forgery class a seeded
    // masked digest produced pre-fix.
    let mut failed_msg = None;
    while let Ok(ev) = events.try_recv() {
        if let Some(rio_proto::types::build_event::Event::Derivation(d)) = ev.event
            && d.kind == rio_proto::types::DerivationEventKind::Failed as i32
        {
            failed_msg = Some(d.error_message);
        }
    }
    let failed_msg = failed_msg.expect("failure event visible");
    assert!(
        failed_msg.contains("covered by neither the submission, the resident DAG"),
        "unseeded-input class (post-read-through), not forgery; got: {failed_msg}"
    );
    assert!(
        failed_msg.contains(&child_path),
        "remediation names the EXCLUDED floating child (proves the seed \
         filter held — across the resident AND row sources); got: {failed_msg}"
    );
    assert!(
        !failed_msg.contains("failed permanently"),
        "must not be the forgery/contradiction class"
    );
    Ok(())
}

// r[verify sched.dispatch.claims-derived+3]
/// THE merged_bug_019 strip kill (deploy-blocker; fix-child of
/// e2c2dbfc2 × 31d281c4d, pattern R1): a bare floating-CA node whose
/// declared modular hash cannot be recomputed (floating store-backed
/// input missing from every seed) used to bounce Unavailable→backoff
/// FOREVER — deterministic re-verification, identical verdict, no exit.
/// Post-fix the declaration is STRIPPED (ingress-strip parity: an
/// unverifiable claim is no claim), the node proceeds on the verified
/// bytes, and both the cleared hash and the raised rank are persisted.
#[tokio::test]
async fn test_dispatch_strips_unverifiable_declared_hash_and_assigns() -> TestResult {
    use rio_nix::hash::{HashAlgo, NixHash};
    use rio_nix::store_path::StorePath;
    use sha2::{Digest, Sha256};

    let test_key = b"test-scheduler-hmac-key-32bytes!".to_vec();
    let (db, store, handle, _tasks) = setup_claims_fixture(&test_key).await?;

    // Floating parent over a floating input that exists NOWHERE (not
    // submitted, not resident, not in the store): the declared hash is
    // structurally unrecomputable, but the parent's own bytes verify.
    let (child, _child_aterm, _h) = mint_floating_ca_leaf("strip-child");
    let child_path = child.drv_path.clone();
    let fparent_aterm = format!(
        r#"Derive([("out","","r:sha256","")],[("{child_path}",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo hi"],[("name","strip-parent"),("out","")])"#
    );
    let fhash = NixHash::new(
        HashAlgo::SHA256,
        Sha256::digest(fparent_aterm.as_bytes()).to_vec(),
    )
    .unwrap();
    let fparent_path = StorePath::make_text(
        "strip-parent.drv",
        &fhash,
        &[StorePath::parse(&child_path).unwrap()],
    )
    .unwrap()
    .as_str()
    .to_owned();
    store.seed_with_content(&fparent_path, fparent_aterm.as_bytes());

    let node = rio_proto::types::DerivationNode {
        drv_path: fparent_path.clone(),
        drv_hash: fparent_path.clone(),
        pname: "strip-parent".into(),
        system: "x86_64-linux".into(),
        output_names: vec!["out".into()],
        is_fixed_output: false,
        is_content_addressed: true,
        expected_output_paths: vec![String::new()],
        ca_modular_hash: vec![0xCC; 32], // junk claim, unrecomputable
        ..Default::default()
    };

    let mut worker_rx = connect_executor(&handle, "strip-w", "x86_64-linux").await?;
    let _events = merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;

    // Pre-fix: no assignment, ever (infinite backoff). Post-fix: the
    // verified bytes are forwarded.
    let assignment = recv_assignment(&mut worker_rx).await;
    assert_eq!(assignment.drv_path, fparent_path);
    assert_eq!(
        assignment.drv_content,
        fparent_aterm.as_bytes(),
        "the store-verified bytes are forwarded"
    );

    // The strip is durable AND in-memory: claim cleared, rank raised.
    let info = handle
        .debug_query_derivation(&fparent_path)
        .await?
        .expect("node resident");
    assert!(
        info.ca.modular_hash.is_none(),
        "in-memory declared hash cleared (an unverifiable claim is no claim)"
    );
    // M_070 (merged_bug_038): the strip MOVES the claim — preserved in
    // the segregated field, never destroyed — so the settled row this
    // node becomes can still match a byte-equal resubmission.
    assert_eq!(
        info.ca.modular_hash_stripped,
        Some([0xCC; 32]),
        "in-memory stripped claim preserved out-of-band"
    );
    let (rank, hash, stripped): (String, Option<Vec<u8>>, Option<Vec<u8>>) = sqlx::query_as(
        "SELECT evidence_rank, ca_modular_hash, ca_modular_hash_stripped \
         FROM derivations WHERE drv_hash = $1",
    )
    .bind(&fparent_path)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        rank, "path_bound_bytes",
        "rank raised on the verified bytes"
    );
    assert!(hash.is_none(), "persisted declared hash cleared");
    assert_eq!(
        stripped.as_deref(),
        Some([0xCC; 32].as_slice()),
        "persisted stripped claim moved to the preservation column"
    );
    Ok(())
}

// r[verify sched.dispatch.claims-derived+3]
/// THE bug_029 kill, failover trigger (depth 2, e47c330a0 <-
/// e2c2dbfc2 <- 1c8cc6877, pattern R5 population-axis): a bare
/// static-IA parent whose completed input child is erased from
/// residency by a LEADER FAILOVER (recovery rehydrates non-terminal
/// rows only). Pre-fix the dispatch claims gate typed the missing
/// input as STRUCTURAL permanence and instant-poisoned the parent —
/// every in-flight build with completed inputs died on the first
/// post-failover dispatch. Post-fix: the chokepoint's persisted-row
/// read-through re-seeds the verification from the child's row (the
/// content-derived state both residency erasers leave intact) and
/// the parent dispatches with proven claims.
#[tokio::test]
async fn test_failover_unseeded_input_reseeds_from_rows_and_dispatches() -> TestResult {
    use rio_auth::hmac::HmacSigner;
    use rio_nix::derivation::{Derivation, hash_derivation_modulo, input_addressed_output_paths};
    use rio_nix::hash::{HashAlgo, NixHash};
    use rio_nix::store_path::StorePath;
    use sha2::{Digest, Sha256};

    let test_key = b"test-scheduler-hmac-key-32bytes!".to_vec();
    let db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;

    // Child: seedable form (is_ca=false), gateway-shaped declared
    // modulo hash (populate_ca_modular_hashes stamps every hash it
    // can compute) — the value the creation upsert persists and the
    // read-through later recovers.
    let (mut child, child_aterm, child_out) = mint_text_ca_leaf("rt-child");
    let child_path = child.drv_path.clone();
    let child_drv = Derivation::parse(&child_aterm).unwrap();
    let child_hash = hash_derivation_modulo(
        &child_drv,
        &child_path,
        &|_| None,
        &mut std::collections::HashMap::new(),
    )
    .unwrap();
    child.ca_modular_hash = child_hash.to_vec();

    // Parent: bare static-IA over the child (concrete declared output
    // path derived through the child's modulo hash — the shape whose
    // verification NEEDS the input seed).
    let build_parent = |out: &str| {
        format!(
            r#"Derive([("out","{out}","","")],[("{child_path}",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo hi"],[("name","rt-parent"),("out","{out}")])"#
        )
    };
    let masked = Derivation::parse(&build_parent("")).unwrap();
    let name_only = format!("/nix/store/{}-rt-parent.drv", "a".repeat(32));
    let resolver = |p: &str| -> Option<&Derivation> { (p == child_path).then_some(&child_drv) };
    let paths = input_addressed_output_paths(
        &masked,
        &name_only,
        &resolver,
        &mut std::collections::HashMap::new(),
    )
    .unwrap();
    let parent_aterm = build_parent(paths["out"].as_str());
    let phash = NixHash::new(
        HashAlgo::SHA256,
        Sha256::digest(parent_aterm.as_bytes()).to_vec(),
    )
    .unwrap();
    let parent_path = StorePath::make_text(
        "rt-parent.drv",
        &phash,
        &[StorePath::parse(&child_path).unwrap()],
    )
    .unwrap()
    .as_str()
    .to_owned();
    let parent = rio_proto::types::DerivationNode {
        drv_path: parent_path.clone(),
        drv_hash: parent_path.clone(),
        pname: "rt-parent".into(),
        system: "x86_64-linux".into(),
        output_names: vec!["out".into()],
        expected_output_paths: vec![paths["out"].as_str().to_owned()],
        ..Default::default()
    };
    store.seed_with_content(&child_path, child_aterm.as_bytes());
    store.seed_with_content(&parent_path, parent_aterm.as_bytes());

    let key = test_key.clone();
    let edge = rio_proto::types::DerivationEdge {
        parent_drv_path: parent_path.clone(),
        child_drv_path: child_path.clone(),
    };

    // Phase 1: child completes under the first leader, then the
    // leader dies (handle dropped, task joined).
    {
        let (handle, task) =
            setup_actor_configured(db.pool.clone(), Some(store_client.clone()), |c, p| {
                c.retry_policy.backoff_base_secs = 0.0;
                p.hmac_signer = Some(Arc::new(HmacSigner::from_key(key.clone())));
            });
        let mut rx = connect_executor(&handle, "rt-w1", "x86_64-linux").await?;
        merge_dag(
            &handle,
            Uuid::new_v4(),
            vec![child.clone(), parent.clone()],
            vec![edge.clone()],
            false,
        )
        .await?;
        let assn = recv_assignment(&mut rx).await;
        assert_eq!(assn.drv_path, child_path, "child dispatches first");
        complete_success(&handle, "rt-w1", &child_path, &child_out).await?;
        wait_for_status(&handle, &child_path, DerivationStatus::Completed).await;
        drop(rx);
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    // Phase 2: fresh leader. The completed child is ROW-ONLY
    // (recovery loads non-terminal rows); the parent is resident and
    // becomes Ready. Pre-fix its first dispatch instant-poisoned.
    let (handle, _task) =
        setup_actor_configured(db.pool.clone(), Some(store_client.clone()), |c, p| {
            c.retry_policy.backoff_base_secs = 0.0;
            p.hmac_signer = Some(Arc::new(HmacSigner::from_key(test_key.clone())));
        });
    handle
        .send_unchecked(crate::actor::ActorCommand::LeaderAcquired)
        .await?;
    barrier(&handle).await;
    assert!(
        handle.debug_query_derivation(&child_path).await?.is_none(),
        "precondition: completed child is NOT resident after failover"
    );

    let mut rx2 = connect_executor(&handle, "rt-w2", "x86_64-linux").await?;
    let assn = recv_assignment(&mut rx2).await;
    assert_eq!(
        assn.drv_path, parent_path,
        "post-failover parent DISPATCHES (read-through re-seeded the \
         verification from the child's persisted row)"
    );
    assert_eq!(
        assn.drv_content,
        parent_aterm.as_bytes(),
        "claims proven against the store bytes, which are forwarded"
    );
    let d = handle
        .debug_query_derivation(&parent_path)
        .await?
        .expect("parent resident");
    assert_ne!(d.status, DerivationStatus::Poisoned, "no instant poison");
    let (rank,): (String,) =
        sqlx::query_as("SELECT evidence_rank FROM derivations WHERE drv_hash = $1")
            .bind(&parent_path)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(rank, "path_bound_bytes", "verified standing persisted");
    Ok(())
}

// r[verify sched.dispatch.claims-derived+3]
/// Read-through MISS arm: the missing input has NO seedable row (the
/// child never declared a hash, so its row's ca_modular_hash is
/// NULL). The verdict stands post-read-through — but the consequence
/// is BOUNDED BACKOFF on the dedicated budget, never the pre-fix
/// instant poison: the identity can still arrive (deeper submission,
/// upload, mid-merge row). Backoff base is set high so exactly one
/// charge lands in the test window.
#[tokio::test]
async fn test_unseeded_input_without_row_backs_off_not_poisons() -> TestResult {
    use rio_auth::hmac::HmacSigner;
    use rio_nix::derivation::{Derivation, input_addressed_output_paths};
    use rio_nix::hash::{HashAlgo, NixHash};
    use rio_nix::store_path::StorePath;
    use sha2::{Digest, Sha256};

    let test_key = b"test-scheduler-hmac-key-32bytes!".to_vec();
    let db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;

    // Hash-LESS child: its row will carry ca_modular_hash NULL — the
    // read-through finds nothing seedable.
    let (child, child_aterm, child_out) = mint_text_ca_leaf("nort-child");
    let child_path = child.drv_path.clone();
    let child_drv = Derivation::parse(&child_aterm).unwrap();
    let build_parent = |out: &str| {
        format!(
            r#"Derive([("out","{out}","","")],[("{child_path}",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo hi"],[("name","nort-parent"),("out","{out}")])"#
        )
    };
    let masked = Derivation::parse(&build_parent("")).unwrap();
    let name_only = format!("/nix/store/{}-nort-parent.drv", "a".repeat(32));
    let resolver = |p: &str| -> Option<&Derivation> { (p == child_path).then_some(&child_drv) };
    let paths = input_addressed_output_paths(
        &masked,
        &name_only,
        &resolver,
        &mut std::collections::HashMap::new(),
    )
    .unwrap();
    let parent_aterm = build_parent(paths["out"].as_str());
    let phash = NixHash::new(
        HashAlgo::SHA256,
        Sha256::digest(parent_aterm.as_bytes()).to_vec(),
    )
    .unwrap();
    let parent_path = StorePath::make_text(
        "nort-parent.drv",
        &phash,
        &[StorePath::parse(&child_path).unwrap()],
    )
    .unwrap()
    .as_str()
    .to_owned();
    let parent = rio_proto::types::DerivationNode {
        drv_path: parent_path.clone(),
        drv_hash: parent_path.clone(),
        pname: "nort-parent".into(),
        system: "x86_64-linux".into(),
        output_names: vec!["out".into()],
        expected_output_paths: vec![paths["out"].as_str().to_owned()],
        ..Default::default()
    };
    store.seed_with_content(&child_path, child_aterm.as_bytes());
    store.seed_with_content(&parent_path, parent_aterm.as_bytes());
    let edge = rio_proto::types::DerivationEdge {
        parent_drv_path: parent_path.clone(),
        child_drv_path: child_path.clone(),
    };

    // Phase 1: child completes; leader dies.
    {
        let key = test_key.clone();
        let (handle, task) =
            setup_actor_configured(db.pool.clone(), Some(store_client.clone()), |c, p| {
                c.retry_policy.backoff_base_secs = 0.0;
                p.hmac_signer = Some(Arc::new(HmacSigner::from_key(key)));
            });
        let mut rx = connect_executor(&handle, "nort-w1", "x86_64-linux").await?;
        merge_dag(
            &handle,
            Uuid::new_v4(),
            vec![child.clone(), parent.clone()],
            vec![edge.clone()],
            false,
        )
        .await?;
        let assn = recv_assignment(&mut rx).await;
        assert_eq!(assn.drv_path, child_path);
        complete_success(&handle, "nort-w1", &child_path, &child_out).await?;
        wait_for_status(&handle, &child_path, DerivationStatus::Completed).await;
        drop(rx);
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    // Phase 2: long backoff base — exactly one deferral lands.
    let (handle, _task) =
        setup_actor_configured(db.pool.clone(), Some(store_client.clone()), |c, p| {
            c.retry_policy.backoff_base_secs = 600.0;
            p.hmac_signer = Some(Arc::new(HmacSigner::from_key(test_key.clone())));
        });
    handle
        .send_unchecked(crate::actor::ActorCommand::LeaderAcquired)
        .await?;
    barrier(&handle).await;

    let mut rx2 = connect_executor(&handle, "nort-w2", "x86_64-linux").await?;
    // No assignment arrives (the deferral rolled it back)...
    let got = try_recv_assignment(&mut rx2, 1500).await;
    assert!(
        got.is_none(),
        "unseedable parent must not be assigned: {got:?}"
    );
    // ...and the node is DEFERRED on the dedicated budget — not
    // poisoned (pre-fix: instant Poisoned with structural
    // remediation).
    let d = handle
        .debug_query_derivation(&parent_path)
        .await?
        .expect("parent resident");
    assert_ne!(
        d.status,
        DerivationStatus::Poisoned,
        "post-read-through unseeded inputs defer; they never instant-poison"
    );
    assert_eq!(
        d.retry.unseeded_inputs_count, 1,
        "exactly one charge against the dedicated unseeded budget"
    );
    Ok(())
}

// r[verify sched.dispatch.claims-derived+3]
/// The verified 100%-livelock population end-to-end: a depth-3
/// deferred-IA chain (floating leaf ← deferred mid ← deferred root),
/// all bare store-backed with gateway-shaped declared hashes, under
/// signing. Pre-fix: the leaf dispatches (its hash recomputes — no
/// inputs) but mid and root livelock forever. Post-fix: the whole
/// chain dispatches and completes.
#[tokio::test]
async fn test_deferred_ia_chain_depth3_dispatches_under_signing() -> TestResult {
    let test_key = b"test-scheduler-hmac-key-32bytes!".to_vec();
    let (db, store, handle, _tasks) = setup_claims_fixture(&test_key).await?;

    // IA leaf: completes without CA-realisation machinery; the
    // deferred shape ABOVE it is what livelocked (the strip arm).
    let (leaf, leaf_aterm, leaf_out) = mint_text_ca_leaf("dia-leaf");
    let leaf_path = leaf.drv_path.clone();
    let (mid, mid_aterm) =
        mint_deferred_ia_node("dia-mid", &leaf_path, &[(&leaf_path, &leaf_aterm)]);
    let mid_path = mid.drv_path.clone();
    let (root, root_aterm) = mint_deferred_ia_node(
        "dia-root",
        &mid_path,
        &[(&mid_path, &mid_aterm), (&leaf_path, &leaf_aterm)],
    );
    let root_path = root.drv_path.clone();
    store.seed_with_content(&leaf_path, leaf_aterm.as_bytes());
    store.seed_with_content(&mid_path, mid_aterm.as_bytes());
    store.seed_with_content(&root_path, root_aterm.as_bytes());

    let edges = vec![
        rio_proto::types::DerivationEdge {
            parent_drv_path: root_path.clone(),
            child_drv_path: mid_path.clone(),
        },
        rio_proto::types::DerivationEdge {
            parent_drv_path: mid_path.clone(),
            child_drv_path: leaf_path.clone(),
        },
    ];
    let mut worker_rx = connect_executor(&handle, "dia-w", "x86_64-linux").await?;
    // Keep the event receiver alive: the orphan-watcher cancels
    // watcher-less builds at the first housekeeping tick (grace 0 in
    // tests), and this test pumps Ticks.
    let _events = merge_dag(&handle, Uuid::new_v4(), vec![leaf, mid, root], edges, false).await?;

    // Leaf: dispatches even pre-fix (no inputs, hash recomputes).
    let a1 = recv_assignment(&mut worker_rx).await;
    assert_eq!(a1.drv_path, leaf_path);
    barrier(&handle).await;
    let probe = handle.debug_query_derivation(&mid_path).await?.unwrap();
    assert_eq!(
        probe.status,
        DerivationStatus::Queued,
        "mid must be Queued while leaf builds; got {:?}",
        probe.status
    );
    complete_success(&handle, "dia-w", &leaf_path, &leaf_out).await?;

    // Mid: THE pre-fix livelock. Post-fix: stripped + assigned. A
    // fresh worker registration is the harness's established
    // post-completion dispatch trigger (Tick only dispatches when the
    // dirty flag is set).
    let mut worker_rx2 = connect_executor(&handle, "dia-w2", "x86_64-linux").await?;
    let mut a2 = None;
    for _ in 0..50 {
        handle.send_unchecked(ActorCommand::Tick).await?;
        barrier(&handle).await;
        if let Some(a) = try_recv_assignment(&mut worker_rx, 50).await {
            a2 = Some(a);
            break;
        }
        if let Some(a) = try_recv_assignment(&mut worker_rx2, 50).await {
            a2 = Some(a);
            break;
        }
    }
    if a2.is_none() {
        let leaf_info = handle.debug_query_derivation(&leaf_path).await?;
        let info = handle.debug_query_derivation(&mid_path).await?;
        panic!("mid never dispatched; leaf: {leaf_info:?}\nmid: {info:?}");
    }
    let a2 = a2.expect("mid must dispatch after the strip (pre-fix: livelocked forever)");
    assert_eq!(a2.drv_path, mid_path);
    let a2_executor = handle
        .debug_query_derivation(&mid_path)
        .await?
        .unwrap()
        .assigned_executor
        .expect("mid assigned");
    let (mid_rank, mid_hash): (String, Option<Vec<u8>>) = sqlx::query_as(
        "SELECT evidence_rank, ca_modular_hash FROM derivations WHERE drv_hash = $1",
    )
    .bind(&mid_path)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(mid_rank, "path_bound_bytes");
    assert!(mid_hash.is_none(), "mid's unverifiable claim stripped");
    let mid_out = format!("/nix/store/{}-dia-mid-out", "c".repeat(32));
    let mid_executor = a2_executor;
    complete_success(&handle, &mid_executor, &mid_path, &mid_out).await?;

    // Root: same strip path one level up.
    let mut worker_rx3 = connect_executor(&handle, "dia-w3", "x86_64-linux").await?;
    let mut a3 = None;
    for _ in 0..50 {
        handle.send_unchecked(ActorCommand::Tick).await?;
        barrier(&handle).await;
        for rx in [&mut worker_rx, &mut worker_rx2, &mut worker_rx3] {
            if let Some(a) = try_recv_assignment(rx, 40).await {
                a3 = Some(a);
                break;
            }
        }
        if a3.is_some() {
            break;
        }
    }
    let a3 = a3.expect("root must dispatch (pre-fix: livelocked forever)");
    assert_eq!(a3.drv_path, root_path);
    Ok(())
}

// r[verify sched.dispatch.claims-derived+3]
/// Unseeded-input EXHAUSTION converges to a visible poison: an IA
/// node whose direct input is neither submitted, nor resident, nor
/// seedable from any persisted row (the floating child here was
/// never even created — no row exists) defers on the bounded
/// unseeded-inputs budget and, once the cap is spent, poisons with
/// the POST-READ-THROUGH remediation. Three eras of this test: the
/// original arm livelocked through backoff forever; the +2 fix
/// instant-poisoned (which bug_029 showed also poisoned honest
/// post-failover builds); +3 defers on a budget and still ends in
/// the same visible poison for the genuinely-hopeless population —
/// no silent livelock returns. (Unsigned mode mints no claims and
/// still dispatches from source.)
#[tokio::test]
async fn test_dispatch_unseeded_exhaustion_poisons_with_remediation() -> TestResult {
    use rio_nix::derivation::{Derivation, input_addressed_output_paths};
    use rio_nix::hash::{HashAlgo, NixHash};
    use rio_nix::store_path::StorePath;
    use sha2::{Digest, Sha256};
    use std::collections::HashMap;

    use rio_auth::hmac::HmacSigner;
    let test_key = b"test-scheduler-hmac-key-32bytes!".to_vec();
    let db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    // cap=1 so exhaustion lands within two dispatch passes (the
    // merge-inline pass charges attempt 0; the next tick-driven pass
    // exhausts) — production cap (max_infra_retries=10) would need
    // ~10 one-second ticks, which is wall-clock the unit suite
    // shouldn't spend. The budget ARITHMETIC is pinned by the charge
    // unit test; this pins the routing (deferral -> exhaustion ->
    // visible poison with the post-read-through remediation).
    let key = test_key.clone();
    let (handle, _actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |c, p| {
            c.retry_policy.backoff_base_secs = 0.0;
            c.retry_policy.max_infra_retries = 1;
            p.hmac_signer = Some(Arc::new(HmacSigner::from_key(key)));
        });

    let (child, child_aterm, _h) = mint_floating_ca_leaf("unseed-child");
    let child_path = child.drv_path.clone();
    let child_drv = Derivation::parse(&child_aterm).unwrap();
    let build_parent = |out: &str| {
        format!(
            r#"Derive([("out","{out}","","")],[("{child_path}",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo hi"],[("name","unseed-parent"),("out","{out}")])"#
        )
    };
    let masked = Derivation::parse(&build_parent("")).unwrap();
    let name_only = format!("/nix/store/{}-unseed-parent.drv", "a".repeat(32));
    let resolver = |p: &str| -> Option<&Derivation> { (p == child_path).then_some(&child_drv) };
    let paths =
        input_addressed_output_paths(&masked, &name_only, &resolver, &mut HashMap::new()).unwrap();
    let parent_aterm = build_parent(paths["out"].as_str());
    let phash = NixHash::new(
        HashAlgo::SHA256,
        Sha256::digest(parent_aterm.as_bytes()).to_vec(),
    )
    .unwrap();
    let parent_path = StorePath::make_text(
        "unseed-parent.drv",
        &phash,
        &[StorePath::parse(&child_path).unwrap()],
    )
    .unwrap()
    .as_str()
    .to_owned();
    store.seed_with_content(&parent_path, parent_aterm.as_bytes());

    let node = rio_proto::types::DerivationNode {
        drv_path: parent_path.clone(),
        drv_hash: parent_path.clone(),
        pname: "unseed-parent".into(),
        system: "x86_64-linux".into(),
        output_names: vec!["out".into()],
        expected_output_paths: vec![paths["out"].as_str().to_owned()],
        ..Default::default()
    };

    let mut worker_rx = connect_executor(&handle, "unseed-w", "x86_64-linux").await?;
    let build_id = Uuid::new_v4();
    let mut events = merge_dag(&handle, build_id, vec![node], vec![], false).await?;
    barrier(&handle).await;

    assert!(
        try_recv_assignment(&mut worker_rx, 300).await.is_none(),
        "no token may be signed for an unverifiable node"
    );
    // backoff 0 + cap 1: the merge-inline dispatch pass charged
    // attempt 0; each heartbeat (which chains a Tick) drives another
    // dispatch pass — the second pass exhausts and poisons.
    let mut poisoned = false;
    for _ in 0..50 {
        send_heartbeat(&handle, "unseed-w", "x86_64-linux").await?;
        barrier(&handle).await;
        if let Ok(Some(d)) = handle.debug_query_derivation(&parent_path).await
            && d.status == DerivationStatus::Poisoned
        {
            poisoned = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        poisoned,
        "exhausted unseeded budget must converge to a visible poison"
    );

    // The remediation is CLIENT-VISIBLE: the DerivationFailed event's
    // error_message carries the generated POST-read-through text
    // (visible poison, not a silent backoff).
    let mut failed_msg = None;
    while let Ok(ev) = events.try_recv() {
        if let Some(rio_proto::types::build_event::Event::Derivation(d)) = ev.event
            && d.kind == rio_proto::types::DerivationEventKind::Failed as i32
        {
            failed_msg = Some(d.error_message);
        }
    }
    let failed_msg = failed_msg.expect("a DerivationFailed event reaches the watcher");
    assert!(
        failed_msg.contains("covered by neither the submission, the resident DAG"),
        "remediation is the post-read-through text (the rows WERE \
         consulted before permanence); got: {failed_msg}"
    );
    assert!(
        failed_msg.contains(&child_path),
        "remediation names the unseeded input"
    );
    assert!(
        failed_msg.contains("could not seed"),
        "poison message names the exhausted budget; got: {failed_msg}"
    );

    // Failure evidence is durably recorded at source.
    let (summary,): (Option<String>,) =
        sqlx::query_as("SELECT error_summary FROM builds WHERE build_id = $1")
            .bind(build_id)
            .fetch_one(&db.pool)
            .await?;
    assert!(
        summary.is_some(),
        "failure evidence recorded at source for the interested build"
    );
    Ok(())
}

// r[verify sched.dispatch.claims-derived+3]
/// Computed-bound scale pin (counts OPS, not wall-clock): dispatching
/// 128 independent bare store-backed nodes performs EXACTLY one store
/// GetPath per node — no closure walks, no refetches after the rank
/// raise. The claims gate's cost is O(nodes), in contrast to the
/// store-side deriver-proof read-through (O(closure), own budget).
/// One pre-connected worker per node so the merge-time dispatch pass
/// assigns the whole set in one wave (the harness has no idle-worker
/// heartbeat loop).
#[tokio::test]
async fn test_claims_gate_scale_one_getpath_per_node() -> TestResult {
    let test_key = b"test-scheduler-hmac-key-32bytes!".to_vec();
    let (_db, store, handle, _tasks) = setup_claims_fixture(&test_key).await?;

    const N: usize = 128;
    let mut nodes = Vec::with_capacity(N);
    for i in 0..N {
        let (node, aterm, _out) = mint_text_ca_leaf(&format!("scale{i:03}"));
        store.seed_with_content(&node.drv_path, aterm.as_bytes());
        nodes.push(node);
    }

    let mut receivers = Vec::with_capacity(N);
    for i in 0..N {
        receivers.push(connect_executor(&handle, &format!("scale-w{i:03}"), "x86_64-linux").await?);
    }
    let _events = merge_dag(&handle, Uuid::new_v4(), nodes, vec![], false).await?;
    barrier(&handle).await;

    // Every node dispatches exactly once across the worker fleet.
    let mut assigned: std::collections::HashSet<String> = std::collections::HashSet::new();
    for _ in 0..20 {
        for rx in receivers.iter_mut() {
            while let Some(a) = try_recv_assignment(rx, 20).await {
                assigned.insert(a.drv_path);
            }
        }
        if assigned.len() == N {
            break;
        }
        handle.send_unchecked(ActorCommand::Tick).await?;
        barrier(&handle).await;
    }
    assert_eq!(assigned.len(), N, "all {N} nodes dispatch");

    let fetches = store
        .calls
        .get_path_calls
        .load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        fetches as usize, N,
        "exactly ONE GetPath per node first-dispatch — no closure walks, \
         no refetch after the path_bound_bytes raise"
    );
    Ok(())
}

/// charge() unit semantics: per-class cap, attempt indices, reset.
#[test]
fn retry_charge_claims_budget_boundaries() {
    use crate::state::{ChargeDecision, FailureClass, RetryState};
    let mut r = RetryState::default();
    assert_eq!(
        r.charge(FailureClass::ClaimsUnavailable, 2),
        ChargeDecision::Backoff(0)
    );
    assert_eq!(
        r.charge(FailureClass::ClaimsUnavailable, 2),
        ChargeDecision::Backoff(1)
    );
    assert_eq!(
        r.charge(FailureClass::ClaimsUnavailable, 2),
        ChargeDecision::Exhausted,
        "cap reached -> terminal, never another retry"
    );
    assert_eq!(
        r.charge(FailureClass::ClaimsUnavailable, 2),
        ChargeDecision::Exhausted,
        "exhaustion is sticky until a success edge"
    );
    // The other budgets are untouched by the claims charge.
    assert_eq!(r.count, 0);
    assert_eq!(r.infra_count, 0);
    r.reset_claims_unavailable();
    assert_eq!(
        r.charge(FailureClass::ClaimsUnavailable, 2),
        ChargeDecision::Backoff(0),
        "success edge resets to consecutive-failure semantics"
    );
}

// r[verify sched.dispatch.claims-derived+3]
/// merged_bug_010 + merged_bug_019 residual: persistent store silence
/// on a deterministic input converges to a VISIBLE poison at its own
/// cap — and consumes neither the transient build budget nor the
/// completion-side infra budget.
#[tokio::test]
async fn test_dispatch_claims_silence_poisons_at_cap() -> TestResult {
    use rio_auth::hmac::HmacSigner;
    let db = TestDb::new(&MIGRATOR).await;
    let (_store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let key = b"test-scheduler-hmac-key-32bytes!".to_vec();
    let (handle, _actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |c, p| {
            c.retry_policy.backoff_base_secs = 0.0;
            c.retry_policy.max_infra_retries = 3;
            p.hmac_signer = Some(Arc::new(HmacSigner::from_key(key)));
        });

    let (node, _aterm, _out) = mint_text_ca_leaf("silence-cap");
    let drv_path = node.drv_path.clone();
    // NEVER seeded: the store stays silent forever.

    let mut worker_rx = connect_executor(&handle, "cap-w0", "x86_64-linux").await?;
    let mut events = merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;
    barrier(&handle).await;
    assert!(try_recv_assignment(&mut worker_rx, 200).await.is_none());

    // Pump dispatch attempts (fresh registration = the harness
    // trigger) until the budget converges to poison.
    for i in 1..=10 {
        let st = handle
            .debug_query_derivation(&drv_path)
            .await?
            .expect("resident")
            .status;
        if st == DerivationStatus::Poisoned {
            break;
        }
        let _rx = connect_executor(&handle, &format!("cap-w{i}"), "x86_64-linux").await?;
        barrier(&handle).await;
    }
    wait_for_status(&handle, &drv_path, DerivationStatus::Poisoned).await;

    let info = handle
        .debug_query_derivation(&drv_path)
        .await?
        .expect("resident");
    assert_eq!(
        info.retry.claims_unavailable_count, 3,
        "poisoned exactly at the cap"
    );
    assert_eq!(info.retry.count, 0, "transient build budget untouched");
    assert_eq!(
        info.retry.infra_count, 0,
        "completion-side infra budget untouched"
    );

    let mut failed_msg = None;
    while let Ok(ev) = events.try_recv() {
        if let Some(rio_proto::types::build_event::Event::Derivation(d)) = ev.event
            && d.kind == rio_proto::types::DerivationEventKind::Failed as i32
        {
            failed_msg = Some(d.error_message);
        }
    }
    let failed_msg = failed_msg.expect("visible failure event");
    assert!(
        failed_msg.contains("could not vouch") && failed_msg.contains("3 dispatch attempts"),
        "remediation names the cap; got: {failed_msg}"
    );
    Ok(())
}
