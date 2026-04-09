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
    setup_actor_configured(pool, None, |_, p| {
        p.leader = crate::lease::LeaderState::from_parts(
            Arc::new(std::sync::atomic::AtomicU64::new(1)),
            leader_flag,
            recovery_flag,
        );
    })
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

// r[verify obs.metric.scheduler-leader-gate]
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
    let actor = bare_actor_classed(db.pool.clone(), &[("small", 60.0), ("large", 1800.0)]);

    // Simulate a rebalancer pass: mutate effective cutoffs in-place.
    {
        let mut g = actor.sizing.size_classes.write();
        g[0].cutoff_secs = 75.3;
        g[1].cutoff_secs = 2100.0;
    }

    let snap = actor.compute_size_class_snapshot(None);
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
    let actor = bare_actor(db.pool.clone());
    let snap = actor.compute_size_class_snapshot(None);
    assert!(
        snap.is_empty(),
        "unconfigured size-classes → empty snapshot"
    );
}

/// I-176: `pool_features` filters Ready derivations by
/// `required_features ⊆ pool_features` — the same subset check
/// `rejection_reason()` applies. A kvm derivation classified as `tiny`
/// (trivial runCommand wrapper) MUST count toward the kvm pool's view
/// and MUST NOT count toward a featureless pool's view, regardless of
/// which class `classify()` puts it in. Without this, the featureless
/// pool spawns a builder that hard_filter rejects (`feature-missing`),
/// and the kvm pool reads `queued{its_class}=0` and never spawns —
/// deadlock.
///
/// I-181: feature-gated pools (`pf ≠ ∅`) additionally exclude
/// ∅-feature derivations. ∅ ⊆ anything is vacuously true, so the
/// subset check alone would have the kvm pool spawn for `hello` —
/// dispatch routes it to the cheap featureless pool, the kvm builder
/// idles until activeDeadlineSeconds.
// r[verify sched.sizeclass.feature-filter+2]
#[tokio::test]
async fn size_class_snapshot_feature_filter() {
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_classed(db.pool.clone(), &[("tiny", 60.0), ("xlarge", 3600.0)]);

    // 3 Ready derivations, all classify as `tiny` (no estimator
    // samples → est_duration=None → smallest class):
    //   a: required_features=[]             — featureless work
    //   b: required_features=["kvm"]        — needs kvm
    //   c: required_features=["kvm","nixos-test"] — the I-176 trigger
    actor.test_inject_ready("a", None, "x86_64-linux");
    actor.test_inject_ready_with_features("b", None, "x86_64-linux", &["kvm"]);
    actor.test_inject_ready_with_features("c", None, "x86_64-linux", &["kvm", "nixos-test"]);

    // --- Unfiltered (None): backward compat — counts all 3. ---
    let snap = actor.compute_size_class_snapshot(None);
    let tiny = snap.iter().find(|s| s.name == "tiny").unwrap();
    assert_eq!(tiny.queued, 3, "unfiltered: all 3 Ready in tiny");
    assert_eq!(
        tiny.queued_by_system.get("x86_64-linux").copied(),
        Some(3),
        "per-system breakdown matches scalar"
    );

    // --- Featureless pool (Some([])): only `a` passes. ---
    // `[] ⊆ []` ✓; `["kvm"] ⊆ []` ✗; `["kvm","nixos-test"] ⊆ []` ✗.
    let snap = actor.compute_size_class_snapshot(Some(&[]));
    let tiny = snap.iter().find(|s| s.name == "tiny").unwrap();
    assert_eq!(
        tiny.queued, 1,
        "featureless pool: kvm derivations excluded → no wasted spawn"
    );
    assert_eq!(tiny.queued_by_system.get("x86_64-linux").copied(), Some(1));

    // --- kvm pool (Some(["kvm","nixos-test","big-parallel"])): b+c. ---
    // I-181: `a` (∅-feature) is EXCLUDED — featureless pool owns it.
    // `["kvm"] ⊆ pf` ✓; `["kvm","nixos-test"] ⊆ pf` ✓.
    // The load-bearing assertion: `b`+`c` are visible — without this
    // the kvm pool never spawns (I-176). `a` invisible — without THAT
    // the kvm pool spawns a phantom .metal builder for `hello` (I-181).
    let kvm_pf: Vec<String> = ["kvm", "nixos-test", "big-parallel"]
        .into_iter()
        .map(String::from)
        .collect();
    let snap = actor.compute_size_class_snapshot(Some(&kvm_pf));
    let tiny = snap.iter().find(|s| s.name == "tiny").unwrap();
    assert_eq!(
        tiny.queued, 2,
        "I-181: kvm pool counts feature-required work only (b+c, NOT a)"
    );

    // --- kvm-only pool (Some(["kvm"])): `b` only. ---
    // I-181: `a` excluded (∅-feature). I-176: `c` excluded
    // (`["kvm","nixos-test"] ⊆ ["kvm"]` is false — `nixos-test`
    // missing). Mirrors hard_filter exactly: a kvm-only worker can't
    // build a derivation that also needs nixos-test.
    let kvm_only: Vec<String> = vec!["kvm".into()];
    let snap = actor.compute_size_class_snapshot(Some(&kvm_only));
    let tiny = snap.iter().find(|s| s.name == "tiny").unwrap();
    assert_eq!(
        tiny.queued, 1,
        "kvm-only pool: ∅-feature (I-181) and nixos-test (I-176) both excluded"
    );

    // xlarge stays 0 in all views — feature filtering doesn't move
    // derivations between classes (classify() is feature-blind by
    // design; the controller's cross-class sum handles overflow).
    let xlarge = snap.iter().find(|s| s.name == "xlarge").unwrap();
    assert_eq!(xlarge.queued, 0);
}

/// I-181 isolation: ONE ∅-feature derivation Ready. kvm pool's view
/// MUST be 0 (featureless pool owns it); featureless pool's view MUST
/// be 1. Regression: `rsb hello-shallow` (no required_features) spawned
/// both `x86-64-medium` AND `x86-64-kvm-xlarge` — the subset check
/// `∅ ⊆ ["kvm",...]` is vacuously true → kvm pool counted it →
/// controller spawned a .metal instance that idled until deadline.
// r[verify sched.sizeclass.feature-filter+2]
#[tokio::test]
async fn size_class_snapshot_kvm_pool_excludes_featureless_work() {
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_classed(db.pool.clone(), &[("medium", 600.0)]);

    // Single Ready derivation, required_features = ∅ (e.g., hello).
    actor.test_inject_ready("hello", None, "x86_64-linux");

    // kvm pool query → 0. The bug: pre-I-181 this was 1.
    let kvm: Vec<String> = vec!["kvm".into()];
    let snap = actor.compute_size_class_snapshot(Some(&kvm));
    assert_eq!(
        snap.iter().find(|s| s.name == "medium").unwrap().queued,
        0,
        "I-181: feature-gated pool MUST NOT count ∅-feature work"
    );

    // Featureless pool query → 1. The featureless pool owns it.
    let snap = actor.compute_size_class_snapshot(Some(&[]));
    assert_eq!(
        snap.iter().find(|s| s.name == "medium").unwrap().queued,
        1,
        "featureless pool owns ∅-feature work"
    );

    // Unfiltered (None) → 1. CLI/status display still sees everything.
    let snap = actor.compute_size_class_snapshot(None);
    assert_eq!(
        snap.iter().find(|s| s.name == "medium").unwrap().queued,
        1,
        "None = no filter (CLI back-compat)"
    );
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
async fn size_class_snapshot_soft_features_strip() {
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_cfg(
        db.pool.clone(),
        DagActorConfig {
            size_classes: size_classes(&[("medium", 600.0)]),
            soft_features: vec![crate::assignment::SoftFeature {
                name: "big-parallel".into(),
                floor_hint: None,
            }],
            ..Default::default()
        },
    );

    actor.test_inject_ready_with_features("ff", None, "x86_64-linux", &["big-parallel"]);
    actor.test_inject_ready_with_features("vm", None, "x86_64-linux", &["kvm", "big-parallel"]);

    // Featureless pool: counts ff (stripped → ∅), NOT vm (stripped →
    // {kvm}, fails subset check vs []).
    let snap = actor.compute_size_class_snapshot(Some(&[]));
    assert_eq!(
        snap.iter().find(|s| s.name == "medium").unwrap().queued,
        1,
        "I-204: featureless pool owns big-parallel-only work after strip"
    );

    // kvm pool: counts vm (stripped → {kvm} ⊆ pf), NOT ff (stripped → ∅,
    // I-181 ∅-guard fires).
    let kvm: Vec<String> = vec!["kvm".into(), "nixos-test".into(), "big-parallel".into()];
    let snap = actor.compute_size_class_snapshot(Some(&kvm));
    assert_eq!(
        snap.iter().find(|s| s.name == "medium").unwrap().queued,
        1,
        "I-204: kvm pool excludes big-parallel-only work, keeps kvm work"
    );

    // Regression: leader-acquire calls clear_persisted_state which
    // replaces self.dag. soft_features MUST survive — first prod deploy
    // of I-204 was a no-op because recovery's `self.dag =
    // DerivationDag::new()` reset it to empty before the first merge.
    actor.clear_persisted_state();
    actor.test_inject_ready_with_features("ff2", None, "x86_64-linux", &["big-parallel"]);
    let snap = actor.compute_size_class_snapshot(Some(&kvm));
    assert_eq!(
        snap.iter().find(|s| s.name == "medium").unwrap().queued,
        0,
        "I-204: soft_features survives clear_persisted_state (leader transition)"
    );
}

/// I-213: a soft feature with `floor_hint` raises `size_class_floor`
/// at DAG-insert. firefox-unwrapped (`{big-parallel}`, ~50GB build dir)
/// previously landed on `tiny` after the strip, climbed
/// tiny→small→medium, and poisoned at `max_retries=2` before reaching
/// a tier with enough disk.
// r[verify sched.sizing.soft-feature-floor]
#[tokio::test]
async fn soft_feature_floor_hint_seeds_size_class_floor() {
    use crate::assignment::SoftFeature;
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_cfg(
        db.pool.clone(),
        DagActorConfig {
            size_classes: size_classes(&[
                ("tiny", 30.0),
                ("small", 120.0),
                ("medium", 600.0),
                ("large", 3600.0),
                ("xlarge", 36000.0),
            ]),
            soft_features: vec![
                SoftFeature {
                    name: "big-parallel".into(),
                    floor_hint: Some("xlarge".into()),
                },
                SoftFeature {
                    name: "benchmark".into(),
                    floor_hint: None,
                },
            ],
            ..Default::default()
        },
    );

    // big-parallel → stripped + floor=xlarge.
    actor.test_inject_ready_with_features("ff", None, "x86_64-linux", &["big-parallel"]);
    let s = actor.dag.node("ff").unwrap();
    assert!(s.required_features.is_empty(), "stripped");
    assert_eq!(
        s.sched.size_class_floor.as_deref(),
        Some("xlarge"),
        "floor_hint applied on strip"
    );

    // benchmark only → stripped, no hint → floor stays None.
    actor.test_inject_ready_with_features("bench", None, "x86_64-linux", &["benchmark"]);
    assert_eq!(
        actor
            .dag
            .node("bench")
            .unwrap()
            .sched
            .size_class_floor
            .as_deref(),
        None,
        "no-hint soft feature is strip-only"
    );

    // Hint must only RAISE: a recovered node with floor=xlarge keeps it
    // even if a lower-hint feature is stripped. Exercise via a node
    // whose persisted floor (medium) is BELOW the hint (xlarge) →
    // raised; and one whose persisted floor would be ABOVE a lower hint.
    let mut actor2 = bare_actor_cfg(
        db.pool.clone(),
        DagActorConfig {
            size_classes: size_classes(&[("tiny", 30.0), ("xlarge", 36000.0)]),
            soft_features: vec![SoftFeature {
                name: "big-parallel".into(),
                floor_hint: Some("tiny".into()),
            }],
            ..Default::default()
        },
    );
    let row = crate::db::RecoveryDerivationRow {
        derivation_id: uuid::Uuid::new_v4(),
        drv_hash: "persisted".into(),
        drv_path: rio_test_support::fixtures::test_drv_path("persisted"),
        pname: None,
        system: "x86_64-linux".into(),
        status: "ready".into(),
        required_features: vec!["big-parallel".into()],
        assigned_builder_id: None,
        retry_count: 0,
        expected_output_paths: vec![],
        output_names: vec!["out".into()],
        is_fixed_output: false,
        is_ca: false,
        failed_builders: vec![],
        size_class_floor: Some("xlarge".into()),
    };
    let state = crate::state::DerivationState::from_recovery_row(row, DerivationStatus::Ready)
        .expect("valid path");
    actor2.dag.insert_recovered_node(state);
    assert_eq!(
        actor2
            .dag
            .node("persisted")
            .unwrap()
            .sched
            .size_class_floor
            .as_deref(),
        Some("xlarge"),
        "floor_hint never demotes a higher persisted floor"
    );

    // Survives leader transition.
    actor.clear_persisted_state();
    actor.test_inject_ready_with_features("ff2", None, "x86_64-linux", &["big-parallel"]);
    assert_eq!(
        actor
            .dag
            .node("ff2")
            .unwrap()
            .sched
            .size_class_floor
            .as_deref(),
        Some("xlarge"),
        "floor_hint survives clear_persisted_state"
    );
}

/// I-187: snapshot honors `size_class_floor`. A derivation that
/// `classify()` puts in `tiny` (no estimator sample → smallest class)
/// but has `size_class_floor=small` (I-177 reactive promotion after a
/// disconnect) MUST count toward `small.queued`, NOT `tiny.queued` —
/// the same `max(target_cutoff, floor_cutoff)` clamp dispatch's
/// `find_executor_with_overflow` applies. Regression: pre-I-187 the
/// snapshot ignored the floor → controller spawned tiny → dispatch
/// rejected (floor>tiny) → tiny idled 120s → disconnected → I-173
/// bumped floor again → spawn loop.
// r[verify sched.sizeclass.snapshot-honors-floor]
#[tokio::test]
async fn size_class_snapshot_honors_floor() {
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_classed(db.pool.clone(), &[("tiny", 60.0), ("small", 600.0)]);

    // 1 Ready non-FOD: classify()=tiny (no estimator sample → smallest),
    // floor=small (set by I-177 promote_size_class_floor on a prior
    // disconnect).
    actor.test_inject_ready_with_floor("a", "x86_64-linux", Some("small"));

    let snap = actor.compute_size_class_snapshot(None);
    let tiny = snap.iter().find(|s| s.name == "tiny").unwrap();
    let small = snap.iter().find(|s| s.name == "small").unwrap();
    assert_eq!(
        small.queued, 1,
        "I-187: floor=small clamps the snapshot bucket — controller spawns small"
    );
    assert_eq!(
        tiny.queued, 0,
        "I-187: classify()=tiny is overridden by floor — pre-fix this was 1 (spawn loop)"
    );
    assert_eq!(
        small.queued_by_system.get("x86_64-linux").copied(),
        Some(1),
        "per-system breakdown follows the clamped bucket"
    );

    // Floor below classify (or equal) → classify wins. Inject a second
    // derivation with floor=tiny: max(tiny,tiny)=tiny.
    actor.test_inject_ready_with_floor("b", "x86_64-linux", Some("tiny"));
    let snap = actor.compute_size_class_snapshot(None);
    let tiny = snap.iter().find(|s| s.name == "tiny").unwrap();
    let small = snap.iter().find(|s| s.name == "small").unwrap();
    assert_eq!(tiny.queued, 1, "floor==classify → no change");
    assert_eq!(small.queued, 1);

    // Stale floor (not in config) → no-clamp, classify wins. Same
    // graceful fallback as dispatch's `cutoff_for()=None`.
    actor.test_inject_ready_with_floor("c", "x86_64-linux", Some("nonexistent"));
    let snap = actor.compute_size_class_snapshot(None);
    let tiny = snap.iter().find(|s| s.name == "tiny").unwrap();
    assert_eq!(
        tiny.queued, 2,
        "stale floor degrades to no-clamp (classify=tiny)"
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

/// `compute_estimator_stats` walks the in-memory snapshot and
/// classifies under effective cutoffs (I-124). One short, one long
/// entry → "small" / "large". With size_classes unconfigured →
/// `size_class` is None for all entries.
// r[verify sched.admin.estimator-stats]
#[tokio::test]
async fn estimator_stats_classifies_under_effective_cutoffs() {
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_classed(db.pool.clone(), &[("small", 60.0), ("large", 3600.0)]);

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
    let mut actor_off = bare_actor(db.pool.clone());
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

// r[verify sched.fod.size-class-reactive]
/// P0556: `compute_fod_size_class_snapshot` buckets in-flight FODs by
/// `size_class_floor`. floor=None → smallest class; unknown floor →
/// also smallest (config-drift fallback). Σ queued ==
/// `queued_fod_derivations` so the per-class signal and the flat
/// signal agree.
#[tokio::test]
async fn fod_size_class_snapshot_buckets_by_floor() {
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_cfg(
        db.pool.clone(),
        DagActorConfig {
            fetcher_size_classes: vec!["tiny".into(), "small".into()],
            ..Default::default()
        },
    );

    // 5 FODs: 3 floor=None → tiny; 2 floor=small → small.
    for h in ["f1", "f2", "f3"] {
        actor.test_inject_ready_fod(h, "x86_64-linux", None);
    }
    for h in ["f4", "f5"] {
        actor.test_inject_ready_fod(h, "x86_64-linux", Some("small"));
    }
    // Unknown floor (config dropped a class) → smallest.
    actor.test_inject_ready_fod("f6", "x86_64-linux", Some("huge"));
    // Non-FOD: ignored.
    actor.test_inject_ready("nonfod", None, "x86_64-linux");

    let fod = actor.compute_fod_size_class_snapshot();
    assert_eq!(fod.len(), 2);
    assert_eq!(fod[0].name, "tiny", "config order preserved");
    assert_eq!(
        fod[0].queued, 4,
        "3×floor=None + 1×unknown-floor → smallest"
    );
    assert_eq!(fod[0].queued_by_system.get("x86_64-linux"), Some(&4));
    assert_eq!(fod[1].name, "small");
    assert_eq!(fod[1].queued, 2, "2×floor=small");
    assert_eq!(fod[0].running, 0);

    // Σ across classes == flat queued_fod_derivations (the invariant
    // the controller's flat-fallback relies on).
    let cluster = actor.compute_cluster_snapshot();
    assert_eq!(
        fod.iter().map(|s| s.queued).sum::<u64>(),
        cluster.queued_fod_derivations as u64
    );

    // Feature off → empty.
    let actor = bare_actor(db.pool.clone());
    assert!(actor.compute_fod_size_class_snapshot().is_empty());
}

/// `queued` counts Ready-status derivations that classify() into each
/// class; `running` counts Assigned/Running by `assigned_size_class`.
/// Verifies the full end-to-end actor path (merge → Ready → queued
/// shows up; dispatch → Assigned → running shows up, queued drops).
#[tokio::test]
async fn size_class_snapshot_queued_and_running_counts() -> TestResult {
    // est_duration defaults to 0.0 (no build_history entries for test
    // nodes) → classify() routes to the smallest class = small.
    let (_db, handle, _task) = setup_with_classes(&[("small", 30.0), ("large", 3600.0)]).await;

    // Merge 3 single-node DAGs. All three → Ready immediately (no
    // deps). No workers connected yet → they stay queued.
    for tag in ["a", "b", "c"] {
        let _rx = merge_single_node(&handle, Uuid::new_v4(), tag, PriorityClass::Scheduled).await?;
    }

    // Snapshot before dispatch: 3 queued in small (est_dur=0 →
    // smallest covering class), 0 running.
    let (snap, _fod) = handle
        .query_unchecked(|reply| {
            ActorCommand::Admin(AdminQuery::GetSizeClassSnapshot {
                pool_features: None,
                reply,
            })
        })
        .await?;
    assert_eq!(snap.len(), 2);
    let small = snap.iter().find(|s| s.name == "small").unwrap();
    assert_eq!(small.queued, 3, "three merged-and-ready derivations");
    assert_eq!(small.running, 0);
    // I-143: per-system breakdown sums to scalar (all 3 are x86_64
    // via merge_single_node).
    assert_eq!(small.queued_by_system.get("x86_64-linux"), Some(&3));
    assert_eq!(small.queued_by_system.values().sum::<u64>(), small.queued);
    assert!(small.running_by_system.is_empty());
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
    send_heartbeat_with(&handle, "w-small", "x86_64-linux", |hb| {
        hb.size_class = Some("small".into());
    })
    .await?;
    // I-163: Heartbeat sets dispatch_dirty; Tick drains it.
    handle.send_unchecked(ActorCommand::Tick).await?;

    // Drain the one assignment the worker receives (serializes with
    // the actor loop so the snapshot below sees the post-dispatch
    // state).
    let _assignment = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("assignment within 5s")
        .expect("assignment not dropped");

    let (snap, _fod) = handle
        .query_unchecked(|reply| {
            ActorCommand::Admin(AdminQuery::GetSizeClassSnapshot {
                pool_features: None,
                reply,
            })
        })
        .await?;
    let small = snap.iter().find(|s| s.name == "small").unwrap();
    assert_eq!(
        small.queued, 2,
        "one dispatched → two still Ready (one build per pod)"
    );
    assert_eq!(small.running, 1, "one Assigned to w-small");
    assert_eq!(small.running_by_system.get("x86_64-linux"), Some(&1));
    assert_eq!(small.running_by_system.values().sum::<u64>(), small.running);

    Ok(())
}

/// I-146: a Ready derivation with NO estimator data (cold-start —
/// pname not in build_history, peak_mem/cpu=None) MUST count in the
/// smallest size-class's `queued`, never vanish. If it vanishes the
/// controller sees queued=0 across all pools → scales every pool to 0
/// → 1870 cold-start drvs sit in ready_queue with no workers and
/// dispatch deadlocks. Also: FODs are NOT counted in any size-class
/// (they dispatch to fetchers, not size-class builders — ADR-019).
#[tokio::test]
async fn size_class_snapshot_cold_start_counts_in_smallest_and_skips_fod() -> TestResult {
    // Classes deliberately UNSORTED so the smallest-class fallback
    // can't accidentally rely on index 0 being smallest.
    let (_db, handle, _task) = setup_with_classes(&[("large", 3600.0), ("tiny", 30.0)]).await;

    // Two non-FOD nodes with pnames the (empty) estimator has never
    // seen → est_duration falls back to DEFAULT_DURATION_SECS (30s)
    // → classify() picks smallest covering class = tiny. One FOD
    // node → must NOT appear in any size-class queued.
    let mut fod = make_node("cold-fod");
    fod.is_fixed_output = true;
    let _rx = merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![make_node("cold-a"), make_node("cold-b"), fod],
        vec![],
        false,
    )
    .await?;

    let (snap, _fod) = handle
        .query_unchecked(|reply| {
            ActorCommand::Admin(AdminQuery::GetSizeClassSnapshot {
                pool_features: None,
                reply,
            })
        })
        .await?;
    assert_eq!(snap.len(), 2);

    let tiny = snap.iter().find(|s| s.name == "tiny").unwrap();
    let large = snap.iter().find(|s| s.name == "large").unwrap();

    // Cold-start non-FODs land in the smallest class. FOD excluded.
    assert_eq!(
        tiny.queued, 2,
        "cold-start non-FOD drvs must count in the smallest class \
         (controller needs non-zero queued to spawn workers)"
    );
    assert_eq!(tiny.queued_by_system.get("x86_64-linux"), Some(&2));
    assert_eq!(large.queued, 0);

    // Invariant: every Ready non-FOD is counted in exactly one class
    // — sum across classes == Ready non-FOD count (2). The FOD (1) is
    // NOT in any class's queued.
    let total_queued: u64 = snap.iter().map(|s| s.queued).sum();
    assert_eq!(
        total_queued, 2,
        "FOD must not be counted in size-class queued (goes to fetchers); \
         every Ready non-FOD must be counted in exactly one class"
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
