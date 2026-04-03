//! Miscellaneous actor feature tests that don't fit the other modules:
//! GcRoots collection, orphan-build cancellation, backpressure hysteresis,
//! leader/recovery dispatch gating.

use super::*;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tracing_test::traced_test;

// ---------------------------------------------------------------------------
// Leader/recovery dispatch gate
// ---------------------------------------------------------------------------

/// Helper: build an actor with custom leader/recovery flags (no mock store).
fn spawn_actor_with_flags(
    pool: sqlx::PgPool,
    is_leader: bool,
    recovery_complete: bool,
) -> (ActorHandle, tokio::task::JoinHandle<()>) {
    let leader_flag = Arc::new(AtomicBool::new(is_leader));
    let recovery_flag = Arc::new(AtomicBool::new(recovery_complete));
    setup_actor_configured(pool, None, |a| {
        a.with_leader_flag(leader_flag)
            .with_recovery_flag(recovery_flag)
    })
}

// r[verify sched.recovery.gate-dispatch]
/// When is_leader=false, dispatch_ready early-returns. Worker
/// connected, DAG merged, heartbeat sent → NO assignment received.
#[tokio::test]
async fn test_not_leader_does_not_dispatch() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = spawn_actor_with_flags(db.pool.clone(), false, true);

    let mut rx = connect_executor(&handle, "nl-worker", "x86_64-linux", 1).await?;
    merge_single_node(&handle, Uuid::new_v4(), "nl-drv", PriorityClass::Scheduled).await?;

    // Extra heartbeat to trigger dispatch_ready.
    send_heartbeat(&handle, "nl-worker", "x86_64-linux", 1).await?;
    barrier(&handle).await;

    // No assignment — dispatch gated by !is_leader.
    assert!(
        rx.try_recv().is_err(),
        "not-leader → no assignment dispatched"
    );
    Ok(())
}

// r[verify obs.metric.scheduler-leader-gate]
/// When is_leader=false, handle_tick must NOT set state gauges.
/// Standby actor is warm (DAGs merge for takeover) but workers don't
/// connect to it (leader-guarded gRPC) — its counts are stale/zero.
/// Publishing them creates a second Prometheus series that stat-panel
/// reducers pick nondeterministically.
///
/// Mechanism mirrors test_force_drain_increments_cancel_signals_total
/// (tests/worker.rs:444): `set_default_local_recorder` installs a
/// thread-local recorder; `#[tokio::test]`'s current-thread runtime
/// means the actor task sees it at `.await` points. The recorder's
/// `register_gauge` tracks names touched — absence of all four gauge
/// names after Tick proves the gate held.
///
/// No connect_executor: the inc/dec at worker.rs:52/76/384 would touch
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

/// Same gate but for recovery_complete=false.
#[tokio::test]
async fn test_recovery_not_complete_does_not_dispatch() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = spawn_actor_with_flags(db.pool.clone(), true, false);

    let mut rx = connect_executor(&handle, "rc-worker", "x86_64-linux", 1).await?;
    merge_single_node(&handle, Uuid::new_v4(), "rc-drv", PriorityClass::Scheduled).await?;

    send_heartbeat(&handle, "rc-worker", "x86_64-linux", 1).await?;
    barrier(&handle).await;

    assert!(
        rx.try_recv().is_err(),
        "recovery not complete → no dispatch"
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
    use rio_common::hmac::{HmacSigner, HmacVerifier};

    let db = TestDb::new(&MIGRATOR).await;
    let test_key = b"test-scheduler-hmac-key-32bytes!".to_vec();

    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |a| {
        a.with_hmac_signer(HmacSigner::from_key(test_key.clone()))
    });

    let mut worker_rx = connect_executor(&handle, "hmac-w", "x86_64-linux", 1).await?;

    // Merge a node WITH expected_output_paths set — the token's
    // claims must include them.
    let expected_out = test_store_path("hmac-expected-out");
    let mut node = make_test_node("hmac-drv", "x86_64-linux");
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
        .verify(&assignment.assignment_token)
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
    use rio_common::hmac::{HmacSigner, HmacVerifier};

    let db = TestDb::new(&MIGRATOR).await;
    let test_key = b"test-clamp-key-at-least-32-bytes!!".to_vec();

    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |a| {
        a.with_hmac_signer(HmacSigner::from_key(test_key.clone()))
    });

    let mut worker_rx = connect_executor(&handle, "clamp-w", "x86_64-linux", 1).await?;

    // Merge with build_timeout = u64::MAX.
    let _ = merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id: Uuid::new_v4(),
            tenant_id: None,
            priority_class: PriorityClass::Scheduled,
            nodes: vec![make_test_node("clamp-drv", "x86_64-linux")],
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
        .verify(&assignment.assignment_token)
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
    let mut node = make_test_node("gcroot-drv", "x86_64-linux");
    node.expected_output_paths = vec![out1.clone(), out2.clone()];
    merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;

    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::GcRoots { reply: reply_tx })
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
    let mut n1 = make_test_node("gc-dup1", "x86_64-linux");
    n1.expected_output_paths = vec![shared_out.clone()];
    let mut n2 = make_test_node("gc-dup2", "x86_64-linux");
    n2.expected_output_paths = vec![shared_out.clone()];

    merge_dag(&handle, Uuid::new_v4(), vec![n1, n2], vec![], false).await?;

    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::GcRoots { reply: reply_tx })
        .await?;
    let roots = reply_rx.await?;

    let count = roots.iter().filter(|p| *p == &shared_out).count();
    assert_eq!(count, 1, "shared output deduped, not 2");

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
                nodes: vec![make_test_node("orphan-drv", "x86_64-linux")],
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
    let actor = DagActor::new(SchedulerDb::new(db.pool.clone()), None);
    let reader = actor.backpressure_flag();
    let mut actor = actor;

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
        |a| a.with_shutdown_token(token)
    });

    // Connect a worker — gives the actor a stream_tx to drop. Then
    // query workers: the reply arrives AFTER ExecutorConnected is
    // processed (same mpsc queue, FIFO), so the stream_tx is in
    // self.executors when we cancel — the test exercises workers.clear()
    // specifically, not just "rx drops when the loop breaks".
    let mut stream_rx = connect_executor(&handle, "sd-worker", "x86_64-linux", 1).await?;
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
// Size-class snapshot
// ---------------------------------------------------------------------------

/// `compute_size_class_snapshot` returns configured cutoffs from the
/// initial `with_size_classes()` call, even after a rebalancer-style
/// mutation to `size_classes.cutoff_secs`. Drift visibility is the
/// whole point of `configured_cutoff_secs` — without it, operators
/// can't tell if the rebalancer converged or drifted.
#[tokio::test]
async fn size_class_snapshot_preserves_configured_after_rebalance() {
    let db = TestDb::new(&MIGRATOR).await;

    // Build actor directly (no spawn) so we can call
    // compute_size_class_snapshot and mutate size_classes.
    let actor = DagActor::new(SchedulerDb::new(db.pool.clone()), None).with_size_classes(vec![
        crate::assignment::SizeClassConfig {
            name: "small".into(),
            cutoff_secs: 60.0,
            mem_limit_bytes: u64::MAX,
            cpu_limit_cores: None,
        },
        crate::assignment::SizeClassConfig {
            name: "large".into(),
            cutoff_secs: 1800.0,
            mem_limit_bytes: u64::MAX,
            cpu_limit_cores: None,
        },
    ]);

    // Simulate a rebalancer pass: mutate effective cutoffs in-place.
    {
        let mut g = actor.size_classes.write();
        g[0].cutoff_secs = 75.3;
        g[1].cutoff_secs = 2100.0;
    }

    let snap = actor.compute_size_class_snapshot();
    assert_eq!(snap.len(), 2);

    // Sorted by effective cutoff ascending.
    assert_eq!(snap[0].name, "small");
    assert_eq!(snap[0].effective_cutoff_secs, 75.3);
    assert_eq!(
        snap[0].configured_cutoff_secs, 60.0,
        "configured must survive rebalancer mutation"
    );
    assert_eq!(snap[1].name, "large");
    assert_eq!(snap[1].effective_cutoff_secs, 2100.0);
    assert_eq!(snap[1].configured_cutoff_secs, 1800.0);

    // Empty DAG → all counts zero.
    assert_eq!(snap[0].queued, 0);
    assert_eq!(snap[0].running, 0);
    assert_eq!(snap[1].queued, 0);
    assert_eq!(snap[1].running, 0);
}

/// Feature off: `with_size_classes(vec![])` → snapshot is empty Vec.
/// The admin handler maps this to an empty response (not an error).
#[tokio::test]
async fn size_class_snapshot_empty_when_unconfigured() {
    let db = TestDb::new(&MIGRATOR).await;
    let actor = DagActor::new(SchedulerDb::new(db.pool.clone()), None);
    let snap = actor.compute_size_class_snapshot();
    assert!(
        snap.is_empty(),
        "unconfigured size-classes → empty snapshot"
    );
}

// ---------------------------------------------------------------------------

/// `compute_estimator_stats` walks the in-memory snapshot and
/// classifies under effective cutoffs (I-124). One short, one long
/// entry → "small" / "large". With size_classes unconfigured →
/// `size_class` is None for all entries.
#[tokio::test]
async fn estimator_stats_classifies_under_effective_cutoffs() {
    use crate::assignment::SizeClassConfig;

    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = DagActor::new(SchedulerDb::new(db.pool.clone()), None).with_size_classes(vec![
        SizeClassConfig {
            name: "small".into(),
            cutoff_secs: 60.0,
            mem_limit_bytes: u64::MAX,
            cpu_limit_cores: None,
        },
        SizeClassConfig {
            name: "large".into(),
            cutoff_secs: 3600.0,
            mem_limit_bytes: u64::MAX,
            cpu_limit_cores: None,
        },
    ]);

    actor.test_refresh_estimator(vec![
        ("hello".into(), "x86_64-linux".into(), 5.0, None, None, 12),
        (
            "chromium".into(),
            "x86_64-linux".into(),
            1800.0,
            Some(8e9),
            None,
            3,
        ),
    ]);

    let stats = actor.compute_estimator_stats();
    assert_eq!(stats.len(), 2);

    let hello = stats.iter().find(|e| e.pname == "hello").unwrap();
    assert_eq!(hello.size_class.as_deref(), Some("small"));
    assert_eq!(hello.sample_count, 12);
    assert_eq!(hello.ema_peak_memory_bytes, None);

    let chromium = stats.iter().find(|e| e.pname == "chromium").unwrap();
    assert_eq!(chromium.size_class.as_deref(), Some("large"));
    assert_eq!(chromium.sample_count, 3);

    // Feature off → size_class None for every entry.
    let mut actor_off = DagActor::new(SchedulerDb::new(db.pool.clone()), None);
    actor_off.test_refresh_estimator(vec![(
        "hello".into(),
        "x86_64-linux".into(),
        5.0,
        None,
        None,
        1,
    )]);
    let stats_off = actor_off.compute_estimator_stats();
    assert_eq!(stats_off.len(), 1);
    assert_eq!(stats_off[0].size_class, None);
}

/// `compute_capacity_manifest` omits cold-start derivations.
///
/// 3 Ready nodes, distinct pnames: 2 have Estimator history with a
/// memory sample, 1 is cold (no history). Manifest has 2 estimates.
/// The controller's deficit calculation uses its operator floor for
/// the missing one — it cannot guess from a non-estimate.
///
/// Plan P0501 T4 exit criterion. Tests the DAG-Ready × Estimator
/// join that `bucketed_estimate`'s pure-function tests cannot reach.
// r[verify sched.admin.capacity-manifest]
#[tokio::test]
async fn capacity_manifest_omits_cold_start() {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = DagActor::new(SchedulerDb::new(db.pool.clone()), None);

    actor.test_inject_ready("warm-a", Some("pkg-a"), "x86_64-linux");
    actor.test_inject_ready("warm-b", Some("pkg-b"), "x86_64-linux");
    actor.test_inject_ready("cold-c", Some("pkg-c"), "x86_64-linux");

    // History for 2 of 3. pkg-c has no row → lookup_entry None → omitted.
    actor.test_refresh_estimator(vec![
        (
            "pkg-a".into(),
            "x86_64-linux".into(),
            60.0,
            Some(6.0 * GIB),
            Some(2.0),
            1,
        ),
        (
            "pkg-b".into(),
            "x86_64-linux".into(),
            120.0,
            Some(10.0 * GIB),
            Some(4.0),
            1,
        ),
    ]);

    let manifest = actor.compute_capacity_manifest();

    assert_eq!(
        manifest.len(),
        2,
        "cold pkg-c omitted — controller uses its floor, not a guess"
    );
    // Bucketing sanity: all survivors have nonzero buckets.
    assert!(
        manifest
            .iter()
            .all(|b| b.memory_bytes >= crate::estimator::MEMORY_BUCKET_BYTES)
    );
    assert!(
        manifest
            .iter()
            .all(|b| b.cpu_millicores >= crate::estimator::CPU_BUCKET_MILLICORES)
    );
}

/// No-pname derivations are also omitted — there's no `build_history`
/// key to look up. FODs, raw derivations without the stdenv `pname`
/// attr fall here.
#[tokio::test]
async fn capacity_manifest_omits_no_pname() {
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = DagActor::new(SchedulerDb::new(db.pool.clone()), None);

    actor.test_inject_ready("has-name", Some("pkg-named"), "x86_64-linux");
    actor.test_inject_ready("no-name", None, "x86_64-linux");

    // History exists for pkg-named only (no-name has nothing to key on).
    actor.test_refresh_estimator(vec![(
        "pkg-named".into(),
        "x86_64-linux".into(),
        30.0,
        Some(4.0 * 1024.0 * 1024.0 * 1024.0),
        Some(1.0),
        1,
    )]);

    let manifest = actor.compute_capacity_manifest();

    assert_eq!(manifest.len(), 1, "pname=None → no lookup key → omitted");
}

/// I-107: `queued_by_system` is a per-system breakdown of
/// `queued_derivations` — Ready-only, sum across keys equals the
/// scalar. Non-Ready (Queued/Assigned/Running) drvs do NOT count.
#[tokio::test]
async fn cluster_snapshot_queued_by_system_sums_to_scalar() {
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = DagActor::new(SchedulerDb::new(db.pool.clone()), None);

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
        actor.test_inject_ready(h, None, sys);
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

/// Non-Ready derivations are excluded even with history present.
/// Only the ready-queue set (same as `queued_derivations`) contributes.
#[tokio::test]
async fn capacity_manifest_ready_only() {
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = DagActor::new(SchedulerDb::new(db.pool.clone()), None);

    // Inject one Ready, then one more and force it to a non-Ready
    // status. Both have the same pname → same history entry applies
    // to both; only Ready contributes.
    actor.test_inject_ready("ready-one", Some("pkg-same"), "x86_64-linux");
    actor.test_inject_ready("not-ready", Some("pkg-same"), "x86_64-linux");
    actor
        .dag
        .node_mut("not-ready")
        .unwrap()
        .set_status_for_test(crate::state::DerivationStatus::Queued);

    actor.test_refresh_estimator(vec![(
        "pkg-same".into(),
        "x86_64-linux".into(),
        45.0,
        Some(5.0 * 1024.0 * 1024.0 * 1024.0),
        Some(2.0),
        1,
    )]);

    let manifest = actor.compute_capacity_manifest();

    assert_eq!(
        manifest.len(),
        1,
        "status != Ready excluded even with matching history"
    );
}

/// P0510 regression guard: manifest and dispatch read the same
/// `self.headroom_mult`.
///
/// Before P0510, headroom was plumbed via ActorCommand param
/// (manifest) AND DagActor field (dispatch) — two copies of one
/// config-static value. A future per-request knob could bypass one
/// path. Now both call `Estimator::bucketed_estimate(&e,
/// self.headroom_mult)`. This test picks a non-default headroom (1.5
/// vs default 1.25) where the bucket boundary differs: 6GiB×1.25 →
/// 8GiB bucket; 6GiB×1.5 → 12GiB bucket.
#[tokio::test]
async fn dispatch_and_manifest_use_same_headroom() {
    use crate::estimator::{Estimator, MEMORY_BUCKET_BYTES};
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = DagActor::new(SchedulerDb::new(db.pool.clone()), None).with_headroom_mult(1.5);

    actor.test_inject_ready("drv", Some("pkg"), "x86_64-linux");
    actor.test_refresh_estimator(vec![(
        "pkg".into(),
        "x86_64-linux".into(),
        60.0,
        Some(6.0 * GIB),
        Some(2.0),
        1,
    )]);

    // Manifest path: compute_capacity_manifest → self.headroom_mult.
    let manifest = actor.compute_capacity_manifest();
    assert_eq!(manifest.len(), 1);
    let manifest_mem = manifest[0].memory_bytes;

    // Dispatch path: dispatch.rs:115 does lookup_entry +
    // bucketed_estimate(&e, self.headroom_mult). Replicate with the
    // actor's field directly — proves the manifest read the same
    // field, not a stale copy.
    let entry = actor.estimator.lookup_entry("pkg", "x86_64-linux").unwrap();
    let dispatch_mem = Estimator::bucketed_estimate(&entry, actor.headroom_mult)
        .unwrap()
        .memory_bytes;

    assert_eq!(
        manifest_mem, dispatch_mem,
        "manifest and dispatch see the same headroom"
    );
    // 6GiB × 1.5 = 9GiB → ceil to 12GiB. At default 1.25 it would be
    // 7.5GiB → 8GiB; distinct buckets prove the injected 1.5 was read.
    assert_eq!(
        manifest_mem,
        3 * MEMORY_BUCKET_BYTES,
        "6GiB×1.5=9GiB → 12GiB bucket (not the 8GiB default-1.25 would give)"
    );
}

/// `queued` counts Ready-status derivations that classify() into each
/// class; `running` counts Assigned/Running by `assigned_size_class`.
/// Verifies the full end-to-end actor path (merge → Ready → queued
/// shows up; dispatch → Assigned → running shows up, queued drops).
#[tokio::test]
async fn size_class_snapshot_queued_and_running_counts() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |a| {
        a.with_size_classes(vec![
            crate::assignment::SizeClassConfig {
                name: "small".into(),
                // est_duration defaults to 0.0 (no build_history
                // entries for test nodes) → classify() routes to the
                // smallest class whose cutoff ≥ 0.0 = small.
                cutoff_secs: 30.0,
                mem_limit_bytes: u64::MAX,
                cpu_limit_cores: None,
            },
            crate::assignment::SizeClassConfig {
                name: "large".into(),
                cutoff_secs: 3600.0,
                mem_limit_bytes: u64::MAX,
                cpu_limit_cores: None,
            },
        ])
    });

    // Merge 3 single-node DAGs. All three → Ready immediately (no
    // deps). No workers connected yet → they stay queued.
    for tag in ["a", "b", "c"] {
        let _rx = merge_single_node(&handle, Uuid::new_v4(), tag, PriorityClass::Scheduled).await?;
    }

    // Snapshot before dispatch: 3 queued in small (est_dur=0 →
    // smallest covering class), 0 running.
    let snap = handle
        .query_unchecked(|reply| ActorCommand::GetSizeClassSnapshot { reply })
        .await?;
    assert_eq!(snap.len(), 2);
    let small = snap.iter().find(|s| s.name == "small").unwrap();
    assert_eq!(small.queued, 3, "three merged-and-ready derivations");
    assert_eq!(small.running, 0);
    let large = snap.iter().find(|s| s.name == "large").unwrap();
    assert_eq!(large.queued, 0);
    assert_eq!(large.running, 0);

    // Connect a small worker. Heartbeat triggers dispatch_ready →
    // one derivation moves to Assigned (one build per pod).
    let (tx, mut rx) = mpsc::channel(16);
    handle
        .send_unchecked(ActorCommand::ExecutorConnected {
            executor_id: "w-small".into(),
            stream_tx: tx,
        })
        .await?;
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            store_degraded: false,
            draining: false,
            kind: rio_proto::types::ExecutorKind::Builder,
            resources: None,
            bloom: None,
            size_class: Some("small".into()),
            executor_id: "w-small".into(),
            systems: vec!["x86_64-linux".into()],
            supported_features: vec![],
            running_builds: vec![],
        })
        .await?;

    // Drain the one assignment the worker receives (serializes with
    // the actor loop so the snapshot below sees the post-dispatch
    // state).
    let _assignment = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("assignment within 5s")
        .expect("assignment not dropped");

    let snap = handle
        .query_unchecked(|reply| ActorCommand::GetSizeClassSnapshot { reply })
        .await?;
    let small = snap.iter().find(|s| s.name == "small").unwrap();
    assert_eq!(
        small.queued, 2,
        "one dispatched → two still Ready (one build per pod)"
    );
    assert_eq!(small.running, 1, "one Assigned to w-small");

    Ok(())
}
