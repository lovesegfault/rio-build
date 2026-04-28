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
    // Expiry is in the future (timeout_secs × 2 from now).
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(claims.expiry_unix > now, "expiry should be in the future");

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
            tenant_id: None,
            priority_class: PriorityClass::Scheduled,
            nodes: vec![make_node("clamp-drv")],
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
    assert!(s.required_features.is_empty(), "stripped");
    assert_eq!(
        s.sched.resource_floor,
        Default::default(),
        "D6: strip-only — floor stays zero"
    );

    // I-204: survives leader transition.
    actor.clear_persisted_state();
    actor.test_inject_ready_with_features("ff2", None, "x86_64-linux", &["big-parallel"]);
    assert!(
        actor.dag.node("ff2").unwrap().required_features.is_empty(),
        "stripping survives clear_persisted_state"
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
/// cores.
fn bare_actor_forecast(pool: sqlx::PgPool, max_lead: f64, max_forecast_cores: u32) -> DagActor {
    use crate::sla::config::CapacityType;
    let mut sla = test_sla_config();
    sla.lead_time_seed
        .insert(("intel-7".into(), CapacityType::Spot), max_lead);
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

/// `clear_persisted_state` clears `recently_disconnected`
/// (per-generation map). Same-process lose→reacquire would otherwise
/// carry stale executor IDs into the new generation.
#[tokio::test]
async fn clear_persisted_state_clears_per_generation_maps() {
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor(db.pool.clone());

    actor.recently_disconnected.insert(
        "stale-exec".into(),
        ("stale".into(), std::time::Instant::now()),
    );

    actor.clear_persisted_state();

    assert!(
        actor.recently_disconnected.is_empty(),
        "recently_disconnected must be cleared on leader transition"
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
    bus.try_log_flush(crate::logs::FlushRequest {
        drv_path: "x".into(),
        interested_builds: vec![],
    });

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
