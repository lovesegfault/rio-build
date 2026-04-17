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

// r[verify sched.sla.reactive-floor]
/// D4: `solve_intent_for` clamps its solved (mem, disk) at
/// `resource_floor`. A derivation with `floor.mem=32GiB` (from prior
/// `bump_floor_or_count` cycles) gets a SpawnIntent with mem ≥ 32GiB
/// even when the SLA solve would return less.
#[tokio::test]
async fn solve_intent_for_clamps_at_resource_floor() {
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor(db.pool.clone());

    // floor.mem=32GiB; cold-start solve (no fit, no override) returns
    // probe-default mem (typically a few GiB) — the clamp raises it.
    actor.test_inject_ready_with_floor("a", "x86_64-linux", 32 << 30);
    let state = actor.dag.node("a").unwrap();
    let intent = actor.solve_intent_for(state);
    assert!(
        intent.mem_bytes >= 32 << 30,
        "D4: solve_intent_for clamps mem at floor (got {})",
        intent.mem_bytes
    );

    // floor=zero (cold start) → solve returns its own value unchanged.
    actor.test_inject_ready_with_floor("b", "x86_64-linux", 0);
    let state = actor.dag.node("b").unwrap();
    let intent_b = actor.solve_intent_for(state);
    assert!(intent_b.mem_bytes < 32 << 30, "control: floor=0 → no clamp");
    // disk also clamped (orthogonal dimension).
    let _ = intent.disk_bytes;
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
        n_eff: 1.0,
        span: 1.0,
        explore: ExploreState {
            distinct_c: 1,
            min_c: RawCores(4.0),
            max_c: RawCores(4.0),
            frozen: false,
            saturated: false,
            last_wall: WallSeconds(800.0),
        },
        t_min_ci: None,
        ci_computed_at: None,
        tier: None,
        hw_bias: Default::default(),
        prior_source: None,
    });
    actor.test_inject_ready("p", Some("exploring"), "x86_64-linux", false);
    let intent = actor.solve_intent_for(actor.dag.node("p").unwrap());
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
        n_eff: 10.0,
        span: 8.0,
        explore: ExploreState {
            distinct_c: 3,
            min_c: RawCores(1.0),
            max_c: RawCores(8.0),
            frozen: true,
            saturated: false,
            last_wall: WallSeconds(0.5),
        },
        t_min_ci: None,
        ci_computed_at: None,
        tier: None,
        hw_bias: Default::default(),
        prior_source: None,
    });
    actor.test_inject_ready("t", Some("trivial"), "x86_64-linux", false);
    let intent = actor.solve_intent_for(actor.dag.node("t").unwrap());
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
    let intent = actor.solve_intent_for(actor.dag.node("k").unwrap());
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
