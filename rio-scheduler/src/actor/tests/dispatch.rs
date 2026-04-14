//! Dispatch: size-class routing, skip-ineligible, options propagation, interactive priority.
// r[verify sched.classify.smallest-covering]
// r[verify sched.overflow.up-only]

use super::*;

/// Size-class routing: a derivation classified as "large" goes only to
/// the large worker, even when a small worker has free capacity. The
/// overflow chain walks small→large but a large build never tries small.
#[tokio::test]
async fn test_size_class_routing_respects_classification() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;

    // Pre-seed build_history: "bigthing" has a 120s EMA. With a 30s
    // small cutoff, classify() will pick "large". Without this seed,
    // est_duration defaults to 30s (DEFAULT_DURATION_SECS) and hits
    // the small class exactly — not what we want to test.
    sqlx::query(
        "INSERT INTO build_history (pname, system, ema_duration_secs, sample_count, last_updated) \
         VALUES ('bigthing', 'x86_64-linux', 120.0, 1, now())",
    )
    .execute(&db.pool)
    .await?;

    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |c, _| {
        c.size_classes = size_classes(&[("small", 30.0), ("large", 3600.0)]);
    });

    // Connect TWO workers: one small (will NOT get the big build),
    // one large (will). Both idle with capacity.
    let (small_tx, mut small_rx) = mpsc::channel(256);
    handle
        .send_unchecked(ActorCommand::ExecutorConnected {
            executor_id: "w-small".into(),
            stream_tx: small_tx,
        })
        .await?;
    send_heartbeat_with(&handle, "w-small", "x86_64-linux", |hb| {
        hb.size_class = Some("small".into());
    })
    .await?;

    let (large_tx, mut large_rx) = mpsc::channel(256);
    handle
        .send_unchecked(ActorCommand::ExecutorConnected {
            executor_id: "w-large".into(),
            stream_tx: large_tx,
        })
        .await?;
    send_heartbeat_with(&handle, "w-large", "x86_64-linux", |hb| {
        hb.size_class = Some("large".into());
    })
    .await?;

    // Prime the estimator. Normally it refreshes on Tick every 60s;
    // for the test we trigger it via 6 Ticks (the refresh cadence).
    // Without this, estimator is empty → est_duration=30s default →
    // goes to "small" → test passes for the wrong reason.
    for _ in 0..6 {
        handle.send_unchecked(ActorCommand::Tick).await?;
    }

    // Merge a build with pname="bigthing" so estimator matches.
    let build_id = Uuid::new_v4();
    let mut node = make_node("bigthing-hash");
    node.pname = "bigthing".into();
    let _event_rx = merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    // Large worker should get the assignment. Small worker should NOT.
    // recv_assignment panics if not an Assignment within 2s.
    let _ = recv_assignment(&mut large_rx).await;

    // Small worker got nothing (try_recv = empty). If classify() was
    // broken and sent to small, this would fail.
    assert!(
        small_rx.try_recv().is_err(),
        "small worker should NOT get the big build (classify routed to large)"
    );

    // The derivation should record that it was assigned to "large"
    // (for misclassification detection at completion).
    let state = expect_drv(&handle, "bigthing-hash").await;
    assert_eq!(
        state.sched.assigned_size_class.as_deref(),
        Some("large"),
        "assigned_size_class recorded for misclassification detection"
    );

    Ok(())
}

/// Dispatch should skip over derivations with no eligible worker (wrong
/// system or missing feature) instead of blocking the entire queue.
#[tokio::test]
async fn test_dispatch_skips_ineligible_derivation() -> TestResult {
    // Only x86_64 worker registered.
    let (_db, handle, _task, mut stream_rx) =
        setup_with_worker("x86-only-worker", "x86_64-linux").await?;

    // Merge aarch64 derivation FIRST (goes to queue head), then x86_64.
    // With the old `None => break`, the aarch64 drv at head would block
    // the x86_64 drv from being dispatched.
    let build_arm = Uuid::new_v4();
    let _rx = merge_dag(
        &handle,
        build_arm,
        vec![make_test_node("arm-hash", "aarch64-linux")],
        vec![],
        false,
    )
    .await?;

    let build_x86 = Uuid::new_v4();
    let p_x86 = test_drv_path("x86-hash");
    let _rx = merge_single_node(&handle, build_x86, "x86-hash", PriorityClass::Scheduled).await?;

    // x86_64 derivation should be dispatched despite aarch64 ahead of it.
    let dispatched_path = recv_assignment(&mut stream_rx).await.drv_path;
    assert_eq!(
        dispatched_path, p_x86,
        "x86_64 derivation should be dispatched even with ineligible aarch64 ahead in queue"
    );

    // aarch64 derivation should still be Ready (not stuck, not dispatched).
    let arm_info = expect_drv(&handle, "arm-hash").await;
    assert_eq!(
        arm_info.status,
        DerivationStatus::Ready,
        "aarch64 derivation should remain Ready (no eligible worker)"
    );
    Ok(())
}

/// Per-build BuildOptions (max_silent_time, build_timeout) must propagate
/// to the worker via WorkAssignment. Regression guard: without
/// propagation, all-zeros defaults would be sent.
#[tokio::test]
async fn test_build_options_propagated_to_worker() -> TestResult {
    let (_db, handle, _task, mut stream_rx) =
        setup_with_worker("options-worker", "x86_64-linux").await?;

    // Submit with build_timeout=300, max_silent_time=60.
    let build_id = Uuid::new_v4();
    let _rx = merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id,
            tenant_id: None,
            priority_class: PriorityClass::Scheduled,
            nodes: vec![make_node("opts-hash")],
            edges: vec![],
            options: BuildOptions {
                max_silent_time: 60,
                build_timeout: 300,
                build_cores: 4,
            },
            keep_going: false,
            traceparent: String::new(),
            jti: None,
            jwt_token: None,
        },
    )
    .await?;

    // Worker should receive assignment with the build's options.
    let assignment = recv_assignment(&mut stream_rx).await;
    let opts = assignment.build_options.expect("options should be set");
    assert_eq!(
        opts.build_timeout, 300,
        "build_timeout should propagate from build to worker"
    );
    assert_eq!(
        opts.max_silent_time, 60,
        "max_silent_time should propagate from build to worker"
    );
    assert_eq!(opts.build_cores, 4);
    Ok(())
}

/// The submitter's W3C traceparent is carried through `MergeDagRequest` →
/// stored on `DerivationState` → embedded in `WorkAssignment.traceparent`
/// at dispatch. This gives gateway→scheduler→worker trace continuity
/// despite the span context NOT crossing the mpsc channel to the actor.
///
/// Regression: before this fix, `dispatch.rs` called
/// `current_traceparent()` which read the actor's ORPHAN span (a fresh
/// root), so the worker's span belonged to a disjoint trace.
// r[verify obs.trace.w3c-traceparent]
// r[verify sched.trace.assignment-traceparent]
#[tokio::test]
async fn test_dispatch_carries_submitter_traceparent() -> TestResult {
    let (_db, handle, _task, mut stream_rx) =
        setup_with_worker("trace-worker", "x86_64-linux").await?;

    // Known traceparent (W3C format: version-trace_id-span_id-flags).
    // The exact bytes don't matter for this test — we just verify it
    // flows through unchanged.
    let known_tp = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

    let build_id = Uuid::new_v4();
    let _rx = merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id,
            tenant_id: None,
            priority_class: PriorityClass::Scheduled,
            nodes: vec![make_node("trace-hash")],
            edges: vec![],
            options: BuildOptions::default(),
            keep_going: false,
            traceparent: known_tp.to_string(),
            jti: None,
            jwt_token: None,
        },
    )
    .await?;

    let assignment = recv_assignment(&mut stream_rx).await;
    assert_eq!(
        assignment.traceparent, known_tp,
        "WorkAssignment.traceparent must match the submitter's traceparent, \
         not the actor's orphan span"
    );
    Ok(())
}

/// Dedup: if a second submitter merges an already-present derivation,
/// the FIRST submitter's traceparent is preserved on the state. The
/// worker's span should chain back to whichever build first introduced
/// the derivation (operationally: the trace that will have waited longest).
#[tokio::test]
async fn test_dispatch_traceparent_first_submitter_wins_on_dedup() -> TestResult {
    // P0537: delay capacity by not connecting the worker until after
    // both merges (zero-capacity workers are no longer expressible).
    let (db, handle, _task) = setup().await;

    let tp_first = "00-11111111111111111111111111111111-1111111111111111-01";
    let tp_second = "00-22222222222222222222222222222222-2222222222222222-01";

    // Helper: merge dedup-hash with a given traceparent (defaults otherwise).
    let merge_with_tp = |tp: &str| MergeDagRequest {
        build_id: Uuid::new_v4(),
        tenant_id: None,
        priority_class: PriorityClass::Scheduled,
        nodes: vec![make_node("dedup-hash")],
        edges: vec![],
        options: BuildOptions::default(),
        keep_going: false,
        traceparent: tp.to_string(),
        jti: None,
        jwt_token: None,
    };

    // First submit with tp_first.
    let _ = merge_dag_req(&handle, merge_with_tp(tp_first)).await?;
    // Second submit: SAME derivation, DIFFERENT traceparent (dedup hit).
    let _ = merge_dag_req(&handle, merge_with_tp(tp_second)).await?;

    // Now give capacity: connect worker → dispatch fires.
    let mut stream_rx = connect_executor(&handle, "dedup-worker", "x86_64-linux").await?;

    let assignment = recv_assignment(&mut stream_rx).await;
    assert_eq!(
        assignment.traceparent, tp_first,
        "first submitter's traceparent should win on dedup (existing state not overwritten)"
    );
    drop(db);
    Ok(())
}

/// Dedup upgrade: if the existing node has traceparent="" (from
/// recovery or poison-reset), a live submitter's traceparent REPLACES
/// it. Recovery isn't a "submitter" — without this, a user's
/// STDERR_NEXT trace_id after failover never finds the worker span.
#[tokio::test]
async fn test_dedup_upgrades_empty_traceparent_from_recovery() -> TestResult {
    let (db, handle, _task) = setup().await;
    // P0537: delay capacity by not connecting until after both merges.

    let merge_with_tp = |tp: &str| MergeDagRequest {
        build_id: Uuid::new_v4(),
        tenant_id: None,
        priority_class: PriorityClass::Scheduled,
        nodes: vec![make_node("upgrade-hash")],
        edges: vec![],
        options: BuildOptions::default(),
        keep_going: false,
        traceparent: tp.to_string(),
        jti: None,
        jwt_token: None,
    };

    // First merge with EMPTY traceparent (simulates recovery:
    // from_recovery_row/from_poisoned_row set traceparent="").
    let _ = merge_dag_req(&handle, merge_with_tp("")).await?;

    // Second merge with a REAL traceparent — dedup hit, should upgrade.
    let live_tp = "00-33333333333333333333333333333333-3333333333333333-01";
    let _ = merge_dag_req(&handle, merge_with_tp(live_tp)).await?;

    // Give capacity: connect worker → dispatch.
    let mut stream_rx = connect_executor(&handle, "upgrade-worker", "x86_64-linux").await?;

    let assignment = recv_assignment(&mut stream_rx).await;
    assert_eq!(
        assignment.traceparent, live_tp,
        "empty traceparent (recovery) should be upgraded by first live submitter"
    );
    drop(db);
    Ok(())
}

/// Interactive builds get a priority boost (+1e9 instead of the old
/// push_front). After a dependency completes, an Interactive build's
/// newly-ready derivation dispatches BEFORE already-queued Scheduled work.
///
/// Same observable behavior as the old push_front test; different
/// mechanism underneath (priority number vs queue position).
#[tokio::test]
async fn test_interactive_priority_boost() -> TestResult {
    // Worker with 1 slot so dispatch order is observable.
    let (_db, handle, _task, mut worker_rx) =
        setup_with_worker("prio-builder", "x86_64-linux").await?;

    // Build 1: Scheduled, 2 independent leaves (Q, R). Both queue immediately.
    // Only 1 dispatches (worker has 1 slot); the other stays queued.
    let build1 = Uuid::new_v4();
    let _rx1 = merge_dag(
        &handle,
        build1,
        vec![make_node("prioQ"), make_node("prioR")],
        vec![],
        false,
    )
    .await?;

    // Build 2: Interactive, 2-node chain A → B. B is a leaf, A blocked.
    let p_prio_a = test_drv_path("prioA");
    let p_prio_b = test_drv_path("prioB");
    let build2 = Uuid::new_v4();
    let _rx2 = merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id: build2,
            tenant_id: None,
            priority_class: PriorityClass::Interactive,
            nodes: vec![make_node("prioA"), make_node("prioB")],
            edges: vec![make_test_edge("prioA", "prioB")],
            options: BuildOptions::default(),
            keep_going: false,
            traceparent: String::new(),
            jti: None,
            jwt_token: None,
        },
    )
    .await?;

    // Drain the first assignment (one of Q/R/B — whichever dispatched first).
    // We don't care which; we only care what happens AFTER we complete it
    // in a way that makes A newly-ready.
    //
    // Strategy: complete EVERYTHING currently assigned with success until
    // prioB is completed. Then A becomes newly-ready with INTERACTIVE_BOOST,
    // and the NEXT dispatch should be A (not a leftover Q/R).
    //
    // One-shot workers: each completion drains the worker; connect a
    // fresh one per iteration.
    let mut seen_paths = Vec::new();
    let mut wid = "prio-builder".to_string();
    for i in 0..4 {
        let Some(msg) = worker_rx.recv().await else {
            break;
        };
        let Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) = msg.msg else {
            continue;
        };
        let path = a.drv_path.clone();
        seen_paths.push(path.clone());
        complete_success(&handle, &wid, &path, &test_store_path("out")).await?;
        // Fresh worker for the next dispatch.
        wid = format!("prio-builder-{}", i + 1);
        worker_rx = connect_executor(&handle, &wid, "x86_64-linux").await?;
        // If we just completed B, the NEXT dispatch should be A (priority boost).
        if path == p_prio_b {
            let next_a = recv_assignment(&mut worker_rx).await;
            assert_eq!(
                next_a.drv_path, p_prio_a,
                "Interactive newly-ready A should dispatch before queued Scheduled work. \
                 Dispatch history: {seen_paths:?}"
            );
            return Ok(());
        }
    }
    panic!("never dispatched prioB within 4 completions. Dispatch history: {seen_paths:?}");
}

// -----------------------------------------------------------------------------
// C1: Leader generation — Arc<AtomicU64>, single-load consistency
// -----------------------------------------------------------------------------

/// The generation in a WorkAssignment must equal the generation a
/// heartbeat would return at the same moment. Both read from the same
/// `Arc<AtomicU64>`. With no lease task running (no writer), both see
/// the init value.
///
/// This catches the previous design's hardcoded-1 bug: if
/// `HeartbeatResponse` and `WorkAssignment` used DIFFERENT generation
/// sources (one hardcoded, one from actor state), this test fails.
#[tokio::test]
async fn test_generation_consistent_between_heartbeat_and_assignment() -> TestResult {
    let (_db, handle, _task, mut rx) = setup_with_worker("w1", "x86_64-linux").await?;

    let _ev = merge_single_node(&handle, Uuid::new_v4(), "a", PriorityClass::Scheduled).await?;
    let assignment = recv_assignment(&mut rx).await;

    // Both sourced from the same Arc<AtomicU64>. No writer → both = 1.
    assert_eq!(assignment.generation, 1, "init value, no lease task");
    assert_eq!(handle.leader_generation(), 1);
    assert_eq!(
        assignment.generation,
        handle.leader_generation(),
        "WorkAssignment and HeartbeatResponse read the same atomic"
    );

    // The assignment_token embeds the generation too (format string).
    // Trailing suffix check — the token is "{executor_id}-{drv_hash}-{gen}",
    // drv_hash is variable-length so we can't split cleanly, but the
    // suffix is reliable.
    assert!(
        assignment.assignment_token.ends_with("-1"),
        "token embeds generation as suffix: {}",
        assignment.assignment_token
    );
    Ok(())
}

/// The generation starts at 1, not 0. Proto-default is 0; a worker
/// receiving `generation=0` should interpret it as "field unset (old
/// scheduler version)" not "first generation."
///
/// Catches the off-by-one if someone changes `AtomicU64::new(1)` → `new(0)`
/// during a refactor. Without this, the bug would only surface as workers
/// treating EVERY first-leadership assignment as unset/stale.
#[tokio::test]
async fn test_generation_starts_at_one_not_zero() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Not dispatching anything — just reading the reader directly.
    // This IS the value HeartbeatResponse.generation would carry.
    assert_eq!(
        handle.leader_generation(),
        1,
        "gen=0 is proto-default (unset); gen=1 is the real first generation"
    );
    Ok(())
}

// -----------------------------------------------------------------------------
// PrefetchHint before WorkAssignment
// -----------------------------------------------------------------------------

/// PrefetchHint arrives BEFORE the WorkAssignment on the stream.
/// Worker starts warming while still parsing the .drv — a few
/// seconds of head start on multi-minute fetches.
///
/// Setup: two-node chain (child → parent). Dispatch the parent;
/// the hint should contain the child's output path (parent's input).
/// The child itself is a leaf (no children → no hint → the first
/// message for it IS the assignment).
#[tokio::test]
async fn test_prefetch_hint_before_assignment() -> TestResult {
    let (_db, handle, _task, mut rx) = setup_with_worker("w1", "x86_64-linux").await?;

    // Two-node chain: child (leaf) → parent (depends on child).
    // merge_dag edges point parent→child (parent's input is child's output).
    //
    // make_test_node leaves expected_output_paths EMPTY by default —
    // most tests don't care about it. approx_input_closure DOES care
    // (that's what it iterates). Populate explicitly.
    let child_out = rio_test_support::fixtures::test_store_path("child-out");
    let mut child = make_node("child");
    child.expected_output_paths = vec![child_out.clone()];
    let parent = make_node("parent");
    let edge = make_test_edge("parent", "child");

    let _ev = merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![child, parent],
        vec![edge],
        false,
    )
    .await?;

    // First message: child is a leaf (no DAG children → no inputs
    // to prefetch → send_prefetch_hint early-returns). So the FIRST
    // thing we get is the child's Assignment.
    let first = rx.recv().await.expect("first message");
    match first.msg {
        Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) => {
            assert!(
                a.drv_path.contains("child"),
                "leaf dispatches first (no deps), no hint precedes it: {a:?}"
            );
        }
        other => panic!("expected Assignment for leaf child, got {other:?}"),
    }

    // Complete the child so parent becomes ready. w1 drains; connect w2.
    complete_success_empty(&handle, "w1", "child").await?;
    let mut rx = connect_executor(&handle, "w2", "x86_64-linux").await?;

    // Parent has one child in the DAG (the completed "child"). Its
    // approx_input_closure = child's expected_output_paths.
    //
    // A fresh worker connecting with a non-empty ready queue gets the
    // on_worker_registered initial PrefetchHint AND the dispatch-time
    // hint (became-idle inline dispatch races ahead of PrefetchComplete
    // and assigns via cold-fallback). Drain ≥1 hint, then the assignment.
    let mut got_hint = None;
    let asgn = loop {
        match rx.recv().await.expect("msg").msg {
            Some(rio_proto::types::scheduler_message::Msg::Prefetch(h)) => got_hint = Some(h),
            Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) => break a,
            other => panic!("expected Prefetch or Assignment, got {other:?}"),
        }
    };
    let hint = got_hint.expect("at least one PrefetchHint before Assignment");
    assert_eq!(
        hint.store_paths,
        vec![child_out],
        "hint = child's output path (parent's direct input via DAG children)"
    );
    assert!(asgn.drv_path.contains("parent"));

    Ok(())
}

/// Two assignments within the same dispatch pass carry the same
/// generation. This is the single-load-per-assignment guarantee —
/// without a lease writer, it's trivially true (nothing changes
/// between loads), but it exercises the dispatch.rs load-once path.
///
/// The REAL torn-read test (concurrent lease fetch_add racing with
/// dispatch) lives with the lease task. This is the structural
/// precursor: proves the single load is what gets used throughout
/// send_assignment.
#[tokio::test]
async fn test_generation_single_load_within_assignment() -> TestResult {
    // P0537: two workers so the same dispatch_ready pass produces two
    // assignments (was one 4-slot worker).
    let (_db, handle, _task, mut rx) = setup_with_worker("w1", "x86_64-linux").await?;
    let mut rx2 = connect_executor(&handle, "w2", "x86_64-linux").await?;

    // Two independent derivations in the same dispatch pass.
    let _ev1 = merge_single_node(&handle, Uuid::new_v4(), "a", PriorityClass::Scheduled).await?;
    let _ev2 = merge_single_node(&handle, Uuid::new_v4(), "b", PriorityClass::Scheduled).await?;

    let a1 = recv_assignment(&mut rx).await;
    let a2 = recv_assignment(&mut rx2).await;

    // Same generation across both, and token suffix agrees with field.
    // If the load happened twice per assignment (e.g., once for token,
    // once for the field), a concurrent writer could split them —
    // token says "-1", field says 2. No writer here, so this asserts
    // STRUCTURAL consistency (same local used for both), not concurrent
    // safety (that's the lease task's job).
    assert_eq!(a1.generation, a2.generation);
    let expected_suffix = format!("-{}", a1.generation);
    assert!(
        a1.assignment_token.ends_with(&expected_suffix),
        "token suffix matches generation field: {} ends with {}",
        a1.assignment_token,
        expected_suffix
    );
    assert!(
        a2.assignment_token.ends_with(&expected_suffix),
        "same suffix on both assignments (single load per dispatch)"
    );
    Ok(())
}

/// Dispatch pins input-closure paths; terminal unpins.
/// Verifies the end-to-end pin → unpin lifecycle via scheduler_
/// live_pins row count.
// r[verify sched.gc.live-pins]
#[tokio::test]
async fn test_pin_unpin_live_inputs_lifecycle() -> TestResult {
    let (db, handle, _task, mut stream_rx) = setup_with_worker("w-x9", "x86_64-linux").await?;

    // Two-node chain: child (leaf, no inputs) + parent (depends
    // on child). Parent's approx_input_closure = child's
    // expected_output_paths. Dispatch of PARENT should pin those.
    //
    // make_test_node defaults expected_output_paths=vec![]; set
    // explicitly so approx_input_closure has something to collect.
    let build_id = Uuid::new_v4();
    let child_out = test_store_path("x9-child-out");
    let mut child = make_node("x9-child");
    child.expected_output_paths = vec![child_out.clone()];
    let parent = make_node("x9-parent");
    let _rx = merge_dag(
        &handle,
        build_id,
        vec![child, parent],
        vec![make_test_edge("x9-parent", "x9-child")],
        false,
    )
    .await?;

    // Child dispatches first (leaf → Ready immediately).
    let assignment_child = recv_assignment(&mut stream_rx).await;
    assert!(assignment_child.drv_path.contains("x9-child"));

    // Child is leaf → approx_input_closure empty → no pin.
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM scheduler_live_pins WHERE drv_hash = 'x9-child'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(count, 0, "leaf drv (no inputs) should not pin anything");

    // Complete child → parent becomes Ready → dispatched → pinned.
    // One-shot: w-x9 drains; connect a fresh worker for parent.
    complete_success_empty(&handle, "w-x9", "x9-child").await?;
    let mut stream_rx = connect_executor(&handle, "w-x9-2", "x86_64-linux").await?;
    // Parent dispatch sends PrefetchHint FIRST (child has expected_
    // output_paths set above), then Assignment. Drain both.
    let assignment_parent = loop {
        let msg = tokio::time::timeout(Duration::from_secs(2), stream_rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        if let Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) = msg.msg {
            break a;
        }
        // Else: PrefetchHint, skip.
    };
    assert!(assignment_parent.drv_path.contains("x9-parent"));
    barrier(&handle).await;

    // Parent's input-closure = child's expected_output_paths
    // (1 path via make_test_node). Pin should be present.
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM scheduler_live_pins WHERE drv_hash = 'x9-parent'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(count, 1, "parent dispatch should pin its 1 input path");

    // Complete parent → unpin.
    complete_success_empty(&handle, "w-x9-2", "x9-parent").await?;
    barrier(&handle).await;

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM scheduler_live_pins WHERE drv_hash = 'x9-parent'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(count, 0, "completion should unpin");

    Ok(())
}

// -----------------------------------------------------------------------------
// CA recovery-resolve: fetch ATerm from store when drv_content empty
// -----------------------------------------------------------------------------

// `recv_assignment` (helpers.rs) already skips Prefetch — alias kept so
// the CA-on-CA test bodies below stay readable at the old name.
use super::recv_assignment as recv_assignment_skip_prefetch;

/// Build a CA-on-CA fixture: (child_node, parent_node, parent_aterm,
/// placeholder, child_modular_hash, realized_path).
///
/// Parent is floating-CA with one inputDrv = child. Child is
/// floating-CA with `ca_modular_hash` set (so `collect_ca_inputs`
/// picks it up). The placeholder is what `resolve_ca_inputs` will
/// replace with `realized_path` once the child's realisation is in PG.
fn ca_on_ca_fixture() -> (
    rio_proto::types::DerivationNode,
    rio_proto::types::DerivationNode,
    String,
    String,
    [u8; 32],
    String,
) {
    use crate::ca::resolve::downstream_placeholder;
    use rio_nix::store_path::StorePath;

    let child_path = test_drv_path("ca-child");
    let child_modular: [u8; 32] = [0xCA; 32];
    let realized_path = test_store_path("ca-child-realized-out");

    let placeholder = downstream_placeholder(&StorePath::parse(&child_path).unwrap(), "out");

    // Parent's ATerm: floating-CA output ("sha256" algo, empty hash,
    // empty path), one inputDrv = child, placeholder in env.DEP.
    let parent_aterm = format!(
        r#"Derive([("out","","sha256","")],[("{child_path}",["out"])],[],"x86_64-linux","/bin/sh",["-c","build"],[("DEP","{placeholder}"),("out",""),("system","x86_64-linux")])"#
    );

    let mut child = make_node("ca-child");
    child.is_content_addressed = true;
    child.needs_resolve = true;
    child.ca_modular_hash = child_modular.to_vec();
    // expected_output_paths can stay empty — parent's PrefetchHint
    // will be empty and skipped (leaf child → no hint anyway).

    let mut parent = make_node("ca-parent");
    parent.is_content_addressed = true;
    parent.needs_resolve = true;
    parent.drv_content = parent_aterm.clone().into_bytes();

    (
        child,
        parent,
        parent_aterm,
        placeholder,
        child_modular,
        realized_path,
    )
}

// r[verify sched.ca.resolve+2]
/// Recovered CA-on-CA dispatch: scheduler restart cleared
/// `drv_content`, but the store has the `.drv` — `maybe_resolve_ca`
/// fetches it via `GetPath`, NAR-unwraps, and resolves placeholders.
///
/// Flow:
///   1. Seed MockStore with parent's ATerm bytes at its `.drv` path
///      (as a single-file NAR — same as `nix-store --dump` of a `.drv`).
///   2. Seed PG `realisations` with child's `(modular_hash, "out")` →
///      `realized_path`.
///   3. Merge CA-on-CA DAG (parent depends on child, both CA).
///   4. Child dispatches. Clear parent's `drv_content` (simulate
///      recovery).
///   5. Complete child → parent becomes Ready → `maybe_resolve_ca`
///      sees empty `drv_content` → fetches from MockStore → unwraps
///      NAR → `resolve_ca_inputs` rewrites placeholder.
///   6. Parent's `WorkAssignment.drv_content` contains the realized
///      path, not the placeholder.
#[tokio::test]
async fn recovered_ca_on_ca_dispatch_fetches_from_store() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let (child, parent, parent_aterm, placeholder, child_modular, realized_path) =
        ca_on_ca_fixture();

    // Seed MockStore: parent's ATerm wrapped in a single-file NAR
    // at its .drv store path. `seed_with_content` does the NAR wrap.
    store.seed_with_content(&parent.drv_path, parent_aterm.as_bytes());

    let mut rx = connect_executor(&handle, "ca-w", "x86_64-linux").await?;

    // Merge: child + parent, edge parent → child.
    let _ev = merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![child, parent],
        vec![make_test_edge("ca-parent", "ca-child")],
        false,
    )
    .await?;

    // Child dispatches first (leaf → Ready immediately).
    let a1 = recv_assignment_skip_prefetch(&mut rx).await;
    assert!(a1.drv_path.contains("ca-child"), "child dispatches first");

    // Seed PG realisations: child's (modular_hash, "out") → realized.
    // This is what `resolve_ca_inputs` queries to map placeholder →
    // realized path. Seeded AFTER merge so the child doesn't cache-hit
    // via check_cached_outputs' CA realisation lookup (GAP-3 fix) —
    // this test needs the child to actually dispatch and complete.
    sqlx::query(
        "INSERT INTO realisations (drv_hash, output_name, output_path, output_hash)
         VALUES ($1, 'out', $2, $3)",
    )
    .bind(child_modular.as_slice())
    .bind(&realized_path)
    .bind([0u8; 32].as_slice())
    .execute(&_db.pool)
    .await?;

    // Clear parent's drv_content BEFORE completing child — actor
    // processes serially, so the clear lands before the completion
    // fires dispatch_ready for the parent.
    let cleared = handle.debug_clear_drv_content("ca-parent").await?;
    assert!(cleared, "parent should be in DAG");

    // Complete child → parent becomes Ready → dispatch fires →
    // maybe_resolve_ca sees empty drv_content → fetches from store.
    complete_success(
        &handle,
        "ca-w",
        "ca-child",
        &test_store_path("ca-child-out"),
    )
    .await?;
    let mut rx = connect_executor(&handle, "ca-w-2", "x86_64-linux").await?;

    let a2 = recv_assignment_skip_prefetch(&mut rx).await;
    assert!(a2.drv_path.contains("ca-parent"));

    // The load-bearing assertions: drv_content was fetched + resolved.
    assert!(
        !a2.drv_content.is_empty(),
        "drv_content must be fetched from store, not left empty"
    );
    let text = std::str::from_utf8(&a2.drv_content).expect("ATerm is ASCII");
    assert!(
        !text.contains(&placeholder),
        "placeholder {placeholder:?} must be replaced post-fetch-and-resolve"
    );
    assert!(
        text.contains(&realized_path),
        "realized path {realized_path:?} must be present in resolved ATerm"
    );

    Ok(())
}

/// Fail-safe preserved: store unreachable → dispatch still proceeds
/// with empty `drv_content`. Same degrade as before the fetch
/// existed — worker fails on placeholder, self-heals via retry after
/// a fresh `SubmitBuild` re-merges with inline `drv_content`.
///
/// Also covers `store_client = None` via the early `?` in
/// `fetch_drv_content_from_store` — this test uses the explicit
/// `fail_get_path` knob instead (closer to a real store outage).
#[tokio::test]
async fn recovered_ca_on_ca_dispatch_degrades_on_store_failure() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let (child, parent, _parent_aterm, _placeholder, child_modular, _realized_path) =
        ca_on_ca_fixture();

    let mut rx = connect_executor(&handle, "ca-w", "x86_64-linux").await?;

    let _ev = merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![child, parent],
        vec![make_test_edge("ca-parent", "ca-child")],
        false,
    )
    .await?;

    let a1 = recv_assignment_skip_prefetch(&mut rx).await;
    assert!(a1.drv_path.contains("ca-child"));

    // Seed the realisation AFTER merge (so the ONLY failure is the
    // store fetch, not a missing-realisation — we're testing the
    // fetch fallback specifically). Seeded post-merge so the child
    // doesn't cache-hit via check_cached_outputs' CA realisation
    // lookup (GAP-3 fix) — this test needs child to dispatch.
    sqlx::query(
        "INSERT INTO realisations (drv_hash, output_name, output_path, output_hash)
         VALUES ($1, 'out', $2, $3)",
    )
    .bind(child_modular.as_slice())
    .bind(test_store_path("irrelevant"))
    .bind([0u8; 32].as_slice())
    .execute(&_db.pool)
    .await?;

    // Clear parent's drv_content AND make GetPath fail. Order matters:
    // actor serializes, so both land before the completion's dispatch.
    let cleared = handle.debug_clear_drv_content("ca-parent").await?;
    assert!(cleared);
    store
        .faults
        .fail_get_path
        .store(true, std::sync::atomic::Ordering::SeqCst);

    complete_success(
        &handle,
        "ca-w",
        "ca-child",
        &test_store_path("ca-child-out"),
    )
    .await?;
    let mut rx = connect_executor(&handle, "ca-w-2", "x86_64-linux").await?;

    let a2 = recv_assignment_skip_prefetch(&mut rx).await;
    assert!(a2.drv_path.contains("ca-parent"));

    // Fail-safe: store fetch failed → drv_content stays empty →
    // worker will fetch + fail on placeholder + retry. Same degrade
    // as before the store-fetch shortcut existed.
    assert!(
        a2.drv_content.is_empty(),
        "store fetch failed → drv_content must stay empty (degrade preserved)"
    );

    Ok(())
}

// -----------------------------------------------------------------------------
// maybe_resolve_ca gate-path passthrough coverage
// -----------------------------------------------------------------------------

// r[verify sched.ca.resolve+2]
/// IA passthrough: `state.ca.needs_resolve = false` → gate at
/// dispatch.rs:681 fails → `drv_content` returned unchanged. No
/// resolve fires, no ContentLookup, no PG query. The cheapest path —
/// every IA-with-IA-inputs dispatch takes it.
#[tokio::test]
async fn maybe_resolve_ca_ia_derivation_passthrough() -> TestResult {
    let (_db, handle, _task, mut rx) = setup_with_worker("ia-w", "x86_64-linux").await?;

    let original_content = b"dummy-ia-aterm-content".to_vec();
    let mut node = make_node("ia-drv");
    node.is_content_addressed = false; // explicit: IA
    node.needs_resolve = false; // explicit: no CA inputs either
    node.drv_content = original_content.clone();

    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;
    let asgn = recv_assignment(&mut rx).await;

    assert_eq!(
        asgn.drv_content, original_content,
        "IA derivation → maybe_resolve_ca passthrough; drv_content unchanged"
    );
    Ok(())
}

// r[verify sched.ca.resolve+2]
/// FOD passthrough: `is_ca = true` BUT `needs_resolve = false` (FOD
/// output path is eval-time known; gateway doesn't set needs_resolve
/// unless an inputDrv is floating-CA). ADR-018 `shouldResolve` table:
/// FOD → only if ca-derivations feature enabled (optional optimization;
/// rio doesn't fire it unless inputs are actually CA).
#[tokio::test]
async fn maybe_resolve_ca_fixed_output_passthrough() -> TestResult {
    // FOD routing (ADR-019): FODs only dispatch to fetchers. The
    // default Builder-kind worker from setup_with_worker would never
    // receive the assignment (hard_filter rejects FOD→builder).
    let (_db, handle, _task) = setup().await;
    let mut rx = connect_executor_no_ack_kind(
        &handle,
        "fod-w",
        "x86_64-linux",
        rio_proto::types::ExecutorKind::Fetcher,
    )
    .await?;
    handle
        .send_unchecked(ActorCommand::PrefetchComplete {
            executor_id: "fod-w".into(),
            paths_fetched: 0,
        })
        .await?;

    let original_content = b"dummy-fod-aterm-content".to_vec();
    let mut node = make_node("fod-drv");
    node.is_content_addressed = true;
    node.is_fixed_output = true;
    node.needs_resolve = false; // gateway: FOD with no CA inputs → no resolve
    node.drv_content = original_content.clone();

    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;
    let asgn = recv_assignment(&mut rx).await;

    assert_eq!(
        asgn.drv_content, original_content,
        "FOD (is_ca && is_fixed_output) → passthrough; output path known at eval"
    );
    Ok(())
}

// r[verify sched.ca.resolve+2]
/// No-CA-inputs passthrough: floating-CA derivation whose children
/// are all IA → `collect_ca_inputs` returns `[]` → gate at
/// dispatch.rs:694 fails → passthrough. The common case: a CA
/// `mkDerivation` on IA stdenv. No resolve needed — no placeholder
/// in the ATerm because all input paths were known at eval time.
#[tokio::test]
async fn maybe_resolve_ca_no_ca_inputs_passthrough() -> TestResult {
    let (_db, handle, _task, mut rx) = setup_with_worker("noca-w", "x86_64-linux").await?;

    let original_content = b"floating-ca-with-ia-deps".to_vec();
    let mut parent = make_node("noca-parent");
    parent.is_content_addressed = true;
    parent.is_fixed_output = false;
    parent.needs_resolve = true; // floating-CA self — gate passes
    parent.drv_content = original_content.clone();

    // IA child — collect_ca_inputs skips it (is_ca=false).
    let child = make_node("noca-child");

    let _ev = merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![parent, child],
        vec![make_test_edge("noca-parent", "noca-child")],
        false,
    )
    .await?;

    // Child dispatches first (leaf).
    let a1 = recv_assignment(&mut rx).await;
    assert!(a1.drv_path.contains("noca-child"));
    complete_success_empty(&handle, "noca-w", &test_drv_path("noca-child")).await?;
    let mut rx = connect_executor(&handle, "noca-w-2", "x86_64-linux").await?;

    // Parent dispatches. collect_ca_inputs(parent) = [] (child is IA)
    // → ca_inputs.is_empty() gate → passthrough.
    let a2 = recv_assignment_skip_prefetch(&mut rx).await;
    assert!(a2.drv_path.contains("noca-parent"));
    assert_eq!(
        a2.drv_content, original_content,
        "floating-CA with only IA children → collect_ca_inputs=[] → \
         passthrough (no resolve, drv_content unchanged)"
    );
    Ok(())
}

/// I-163 Fix 1 + became-idle carve-out: steady-state Heartbeats set
/// `dispatch_dirty` (drained by `Tick`); a Heartbeat that flips
/// capacity 0→1 dispatches inline. At 290 workers × 169ms/dispatch
/// the unconditional inline path was ~5× actor capacity — the 0→1
/// edge is bounded by spawn rate, not heartbeat rate.
///
/// Shape: merge with no worker (Ready, deferred) → connect_no_ack
/// (registration heartbeat = 0→1 → inline dispatch, cold-fallback
/// places the node) → assert Assignment lands WITHOUT a Tick. Then a
/// SECOND heartbeat for the now-busy worker is steady-state (0→0) →
/// only dirty, no inline dispatch.
// r[verify sched.actor.dispatch-decoupled]
// r[verify sched.dispatch.became-idle-immediate]
#[tokio::test]
async fn heartbeat_sets_dirty_tick_dispatches() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Merge first: dispatch_ready runs at end-of-merge with zero
    // workers → deferred. ready_queue holds the node.
    let _ev =
        merge_single_node(&handle, Uuid::new_v4(), "i163-hb", PriorityClass::Scheduled).await?;

    // Connected + Heartbeat. Registration heartbeat is a 0→1 capacity
    // edge → became_idle=true → inline dispatch_ready. Cold-fallback
    // places the node on the freshly-registered worker (warm-gate
    // sent a PrefetchHint but cold-fallback ignores warm).
    let mut rx = connect_executor_no_ack(&handle, "i163-w", "x86_64-linux").await?;
    let a = recv_assignment(&mut rx).await;
    assert!(
        a.drv_path.contains("i163-hb"),
        "registration heartbeat (0→1) must dispatch inline — \
         r[sched.dispatch.became-idle-immediate]"
    );

    // Second heartbeat: worker is now busy (running_build=Some) →
    // capacity 0→0 → became_idle=false → only dispatch_dirty set.
    // Queue another node; it must NOT dispatch on this heartbeat
    // (worker has no capacity AND no 0→1 edge), only on Tick after
    // capacity frees. Here we just assert no spurious second
    // assignment from the steady-state heartbeat.
    send_heartbeat_with(&handle, "i163-w", "x86_64-linux", |hb| {
        hb.running_build = Some(a.drv_path.clone());
    })
    .await?;
    barrier(&handle).await;
    while let Ok(m) = rx.try_recv() {
        use rio_proto::types::scheduler_message::Msg;
        assert!(
            !matches!(m.msg, Some(Msg::Assignment(_))),
            "steady-state heartbeat (0→0, busy) must not dispatch inline"
        );
    }
    Ok(())
}

/// I-163 Fix 2: deferred FODs are checked by the batch pre-pass (one
/// `FindMissingPaths`), and the drain loop SKIPS the per-FOD
/// `fod_outputs_in_store` for hashes the batch already covered. The
/// doc-comment on `fod_outputs_in_store` claimed this; the code
/// didn't honor it (211 deferred FODs × ~0.7ms RTT ≈ 150ms of the
/// 169ms/Heartbeat I-163 cost).
///
/// Shape: merge 5 Ready FODs with no fetcher (all defer); count
/// `FindMissingPaths` across one dispatch_ready. Want exactly 1 (the
/// batch). Pre-fix would be 1 + 5 = 6.
#[tokio::test]
async fn batch_checked_fods_skip_per_fod_rpc() -> TestResult {
    use std::sync::atomic::Ordering;

    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    // Builder, not fetcher: FODs route to fetchers (ADR-019), so all
    // 5 defer in the drain loop — that's the I-163 hot path.
    let _rx = connect_executor(&handle, "i163-builder", "x86_64-linux").await?;

    let nodes: Vec<_> = (0..5)
        .map(|i| {
            let mut n = make_node(&format!("i163-fod-{i}"));
            n.is_fixed_output = true;
            // batch pre-pass filters on !expected_output_paths.is_empty()
            n.expected_output_paths = vec![test_store_path(&format!("i163-fod-{i}-out"))];
            n
        })
        .collect();
    let _ev = merge_dag(&handle, Uuid::new_v4(), nodes, vec![], false).await?;

    // Merge ran check_cached_outputs (1 RPC) + dispatch_ready (1 batch
    // RPC). Reset the baseline; the assertion is on the NEXT
    // dispatch_ready in isolation.
    barrier(&handle).await;
    store.calls.find_missing_calls.store(0, Ordering::SeqCst);

    // Drive one dispatch_ready: Heartbeat (dirty) + Tick (drain).
    // send_heartbeat already chains the Tick.
    send_heartbeat(&handle, "i163-builder", "x86_64-linux").await?;
    barrier(&handle).await;

    let calls = store.calls.find_missing_calls.load(Ordering::SeqCst);
    assert_eq!(
        calls, 1,
        "one dispatch_ready over 5 deferred FODs must issue exactly the batch \
         FindMissingPaths (got {calls}); >1 means the per-FOD fallback fired \
         for batch-checked hashes"
    );
    Ok(())
}

/// I-163 Fix 2, fail-open edge: when the batch RPC fails, the per-drv
/// fallback in the drain loop STILL fires for nodes the batch didn't
/// stamp (cascade-promoted). Nodes the batch DID stamp this gen are
/// `probed_generation`-gated in the per-drv path too, so a batch
/// failure for them defers retry to the next Tick (1/s) instead of
/// firing N sequential per-drv FMPs in the same pass.
#[tokio::test]
async fn batch_fod_fail_open_preserves_per_fod_fallback() -> TestResult {
    use std::sync::atomic::Ordering;

    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    let _rx = connect_executor(&handle, "i163-fo-b", "x86_64-linux").await?;

    let mut n = make_node("i163-fo-fod");
    n.is_fixed_output = true;
    n.expected_output_paths = vec![test_store_path("i163-fo-fod-out")];
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![n], vec![], false).await?;
    barrier(&handle).await;

    store.faults.fail_find_missing.store(true, Ordering::SeqCst);
    store.calls.find_missing_calls.store(0, Ordering::SeqCst);

    send_heartbeat(&handle, "i163-fo-b", "x86_64-linux").await?;
    barrier(&handle).await;

    // send_heartbeat → dispatch_dirty → Tick → gen++ → dispatch_ready:
    // batch stamps the FOD (1 FMP, fails). Per-drv fallback gated by
    // probed_generation → 0 extra. Exactly 1 proves the batch retries
    // at Tick cadence and the per-drv path doesn't re-fire for the
    // same node within a generation.
    assert_eq!(store.calls.find_missing_calls.load(Ordering::SeqCst), 1);
    Ok(())
}

// r[verify sched.dispatch.fod-substitute]
/// Dispatch-time substitution: a Ready IA derivation (FOD or non-FOD)
/// whose output becomes substitutable AFTER merge (so merge-time
/// `check_cached_outputs` missed it) is completed by
/// `batch_probe_cached_ready` without dispatching to a worker.
///
/// Pre-fix: the batch was FOD-only AND read only `missing_paths` (no
/// service-token, ignored `substitutable_paths`) → non-FODs relied on
/// merge-time `check_available` which truncates at 4096 → an 18k-drv
/// build's IA cache-hits dispatched to builders.
#[rstest::rstest]
#[case::fod(true)]
#[case::non_fod(false)]
#[tokio::test]
async fn dispatch_time_substitutable_completes(#[case] is_fod: bool) -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    // x86_64 builder; node is aarch64 → defers regardless of FOD-ness
    // (no system match), so merge-time dispatch can't assign it.
    let _rx = connect_executor(&handle, "sub-b", "x86_64-linux").await?;

    let out = test_store_path("dispatch-sub-out");
    let mut n = make_node("dispatch-sub-drv");
    n.is_fixed_output = is_fod;
    n.system = "aarch64-linux".into();
    n.expected_output_paths = vec![out.clone()];
    let drv_path = n.drv_path.clone();
    let build_id = Uuid::new_v4();
    let mut ev_rx = merge_dag(&handle, build_id, vec![n], vec![], false).await?;
    barrier(&handle).await;
    // Merge-time saw nothing (substitutable not yet seeded) → node
    // Ready/deferred, stamped probed_generation=1. Seed; the next
    // send_heartbeat is NOT became_idle (worker already idle) → sets
    // dispatch_dirty → chained Tick advances probe_generation → batch
    // re-probes → spawns substitute fetch.
    store.state.substitutable.write().unwrap().push(out.clone());
    send_heartbeat(&handle, "sub-b", "x86_64-linux").await?;
    settle_substituting(&handle, &[&make_node("dispatch-sub-drv").drv_hash]).await;
    tick(&handle).await?;

    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "is_fod={is_fod}: should complete via dispatch-time substitution probe"
    );
    let qpi = store.calls.qpi_calls.read().unwrap();
    assert!(
        qpi.contains(&out),
        "dispatch-time eager-fetch must call QueryPathInfo for the substitutable path; \
         qpi_calls={qpi:?}"
    );

    // Gateway visibility: both Substituting and Cached events must
    // reach interested builds (order is structurally guaranteed by
    // the actor — emit-after-transition + mailbox-ordered
    // SubstituteComplete — so this asserts presence only).
    use rio_proto::types::{DerivationEventKind, build_event::Event};
    let (mut got_substituting, mut got_cached) = (None, None);
    while let Ok(be) = ev_rx.try_recv() {
        if let Some(Event::Derivation(d)) = be.event
            && d.derivation_path == drv_path
        {
            match d.kind() {
                DerivationEventKind::Substituting => got_substituting = Some(d.clone()),
                DerivationEventKind::Cached => got_cached = Some(d.clone()),
                _ => {}
            }
        }
    }
    let s = got_substituting.expect("Substituting event must be emitted to interested builds");
    assert_eq!(s.output_paths, vec![out.clone()], "carries output paths");
    assert!(
        got_cached.is_some(),
        "Cached event (existing) must still arrive after substitution completes"
    );
    Ok(())
}

/// I-163 Fix 3: `cluster_snapshot_cached()` reads the watch-channel
/// value the actor publishes on `Tick` — no mailbox round-trip. The
/// `fn` (not `async fn`) signature is the structural proof; this test
/// verifies the value is wired (Tick → publish → handle reads it) and
/// is stale-until-Tick (merge alone doesn't update it).
// r[verify sched.admin.snapshot-cached]
#[tokio::test]
async fn cluster_snapshot_cached_reflects_tick() -> TestResult {
    let (_db, handle, _task) = setup().await;
    let _rx = connect_executor(&handle, "i163-snap-w", "x86_64-linux").await?;
    let _ev = merge_single_node(
        &handle,
        Uuid::new_v4(),
        "i163-snap",
        PriorityClass::Scheduled,
    )
    .await?;
    barrier(&handle).await;

    // No Tick yet → watch holds the Default snapshot (all zeros).
    // connect_executor's PrefetchComplete dispatched the node, but
    // the cached snapshot doesn't see that until Tick publishes.
    let pre = handle.cluster_snapshot_cached();
    assert_eq!(
        pre.total_executors, 0,
        "cached snapshot is Tick-published, not live; pre-Tick must be Default"
    );

    tick(&handle).await?;

    let post = handle.cluster_snapshot_cached();
    assert_eq!(post.total_executors, 1);
    assert_eq!(post.active_executors, 1);
    // Node was assigned by PrefetchComplete's inline dispatch (still
    // runs inline — only Heartbeat moved to dirty-flag).
    assert_eq!(post.running_derivations, 1);
    Ok(())
}

// r[verify sched.fod.size-class-reactive]
// r[verify sched.builder.size-class-reactive]
// r[verify sched.dispatch.no-fod-fallback]
/// `size_class_floor=small` skips tiny-class executors even when free.
/// The overflow chain starts at `max(classify(), floor)`.
///
/// - **fod** (I-170): FOD branch walks `fetcher_classes` (= size_classes names).
/// - **builder** (I-177): non-FOD branch ignored floor before fix → a
///   build that OOM'd on tiny was re-routed to tiny by the
///   (success-only) EMA classifier → poison-loop.
///
/// Shape: 2 tiny + 1 small executor. Merge → first dispatch goes to
/// tiny (floor=None) → InfrastructureFailure(CgroupOom) → floor
/// promoted → second dispatch goes to small, tiny skipped.
/// (TransientFailure does NOT promote — that's a build-determinism
/// signal. CgroupOom is the worker-reported sizing signal.)
#[rstest::rstest]
#[case::fod(rio_proto::types::ExecutorKind::Fetcher, true, "oom-fod")]
#[case::builder(rio_proto::types::ExecutorKind::Builder, false, "glibc-177")]
#[tokio::test]
async fn size_class_floor_skips_smaller(
    #[case] kind: rio_proto::types::ExecutorKind,
    #[case] is_fod: bool,
    #[case] tag: &str,
) -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |c, _| {
        c.size_classes = size_classes(&[("tiny", 30.0), ("small", 3600.0)]);
        // Zero backoff so the retry redispatches on the next Tick.
        c.retry_policy = crate::RetryPolicy {
            backoff_base_secs: 0.0,
            ..Default::default()
        };
    });

    let mut tiny1 =
        connect_executor_classed(&handle, "tiny-1", "x86_64-linux", "tiny", kind).await?;
    let mut tiny2 =
        connect_executor_classed(&handle, "tiny-2", "x86_64-linux", "tiny", kind).await?;
    let mut small =
        connect_executor_classed(&handle, "small", "x86_64-linux", "small", kind).await?;

    let mut node = make_node(tag);
    node.is_fixed_output = is_fod;
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;

    // First dispatch: floor=None → tiny.
    barrier(&handle).await;
    let (first_exec, first_asgn) = tokio::select! {
        a = recv_assignment(&mut tiny1) => ("tiny-1", a),
        a = recv_assignment(&mut tiny2) => ("tiny-2", a),
    };
    assert!(first_asgn.drv_path.contains(tag));
    assert!(small.try_recv().is_err(), "floor=None: small not used");

    // Worker-reported CgroupOom → floor promoted tiny→small.
    complete_failure(
        &handle,
        first_exec,
        tag,
        rio_proto::types::BuildResultStatus::InfrastructureFailure,
        "cgroup OOM during build; promoting size class",
    )
    .await?;
    tick(&handle).await?;

    assert_eq!(
        expect_drv(&handle, tag)
            .await
            .sched
            .size_class_floor
            .as_deref(),
        Some("small"),
        "CgroupOom on tiny → floor promoted to small"
    );

    // Second dispatch: floor=small. Other tiny is free but MUST be skipped.
    let asgn = recv_assignment(&mut small).await;
    assert!(asgn.drv_path.contains(tag));
    assert!(
        tiny1.try_recv().is_err() && tiny2.try_recv().is_err(),
        "floor=small: tiny executors skipped even when free"
    );
    Ok(())
}

// r[verify sched.fod.size-class-reactive]
/// I-170: floor clamps at the largest class. A FOD that fails on
/// the largest configured fetcher class keeps `floor = largest`;
/// `next_fetcher_class(largest)` returns None.
#[tokio::test]
async fn fod_size_class_floor_clamps_at_largest() -> TestResult {
    use crate::assignment::next_fetcher_class;

    let classes = vec!["tiny".into(), "small".into()];
    assert_eq!(
        next_fetcher_class("tiny", &classes).as_deref(),
        Some("small")
    );
    assert_eq!(
        next_fetcher_class("small", &classes),
        None,
        "largest → None (clamp)"
    );
    assert_eq!(
        next_fetcher_class("unknown", &classes),
        None,
        "unknown class (config changed mid-run) → None, no promotion"
    );
    assert_eq!(
        next_fetcher_class("tiny", &[]),
        None,
        "feature off → no promotion"
    );
    Ok(())
}

// r[verify sched.fod.size-class-reactive]
/// I-170 back-compat: with `size_classes` empty (default), FOD dispatch
/// is unchanged — no class filter, any free fetcher. `size_class_floor`
/// stays None across failures (nothing to promote to). Fetcher classes
/// are derived from `size_classes`, so empty → both ladders off.
#[tokio::test]
async fn fod_dispatch_unclassed_when_feature_off() -> TestResult {
    let (_db, handle, _task) = setup().await;
    // Fetcher with NO size_class declared (pre-I-170 pool).
    let mut rx = connect_executor_no_ack_kind(
        &handle,
        "f-unclassed",
        "x86_64-linux",
        rio_proto::types::ExecutorKind::Fetcher,
    )
    .await?;
    handle
        .send_unchecked(ActorCommand::PrefetchComplete {
            executor_id: "f-unclassed".into(),
            paths_fetched: 0,
        })
        .await?;

    let mut node = make_node("plain-fod");
    node.is_fixed_output = true;
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;

    let asgn = recv_assignment(&mut rx).await;
    assert!(asgn.drv_path.contains("plain-fod"));
    let state = expect_drv(&handle, "plain-fod").await;
    assert_eq!(
        state.sched.size_class_floor, None,
        "feature off: floor stays None"
    );
    Ok(())
}

// r[verify sched.builder.size-class-reactive]
/// I-177: `next_builder_class` orders by `cutoff_secs`, not config
/// order. Clamps at largest; unknown/empty → None.
#[test]
fn next_builder_class_cutoff_ordered() {
    use crate::assignment::next_builder_class;
    // Deliberately unsorted config order — next_builder_class must
    // sort by cutoff, not by Vec position.
    let classes = size_classes(&[("large", 3600.0), ("tiny", 30.0), ("small", 300.0)]);
    assert_eq!(
        next_builder_class("tiny", &classes).as_deref(),
        Some("small")
    );
    assert_eq!(
        next_builder_class("small", &classes).as_deref(),
        Some("large")
    );
    assert_eq!(
        next_builder_class("large", &classes),
        None,
        "largest clamps"
    );
    assert_eq!(next_builder_class("unknown", &classes), None);
    assert_eq!(next_builder_class("tiny", &[]), None, "feature off");
}

// r[verify sched.dispatch.unroutable-system]
/// Ready drv whose `system` is advertised by zero registered executors:
/// stays Ready (deferred), WARN fires once (edge-triggered, not per
/// tick). Connecting a matching executor dispatches it.
#[tokio::test]
#[tracing_test::traced_test]
async fn test_unroutable_system_warn_then_dispatch() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Only an x86_64 builder registered.
    let _w = connect_executor(&handle, "x86-b1", "x86_64-linux").await?;

    // riscv drv has no advertising pool.
    let riscv = make_test_node("rv-unroutable", "riscv64-linux");
    let rv_hash = riscv.drv_hash.clone();
    // Hold the event receiver so the orphan-watcher doesn't auto-cancel.
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![riscv], vec![], false).await?;
    tick(&handle).await?;

    let d = expect_drv(&handle, &rv_hash).await;
    assert_eq!(
        d.status,
        DerivationStatus::Ready,
        "unroutable drv defers (not poisoned/failed)"
    );
    assert!(
        logs_contain("no registered executor advertises this system")
            && logs_contain("riscv64-linux"),
        "WARN fires when system first becomes unroutable"
    );

    // Edge-triggered: a second tick must NOT re-WARN.
    tick(&handle).await?;
    logs_assert(|lines| {
        let n = lines
            .iter()
            .filter(|l| l.contains("no registered executor advertises this system"))
            .count();
        if n == 1 {
            Ok(())
        } else {
            Err(format!("WARN must fire once across both ticks; got {n}"))
        }
    });

    // Connect a riscv builder → next tick dispatches. That same tick
    // sees riscv as routable, so `unroutable_warned.retain()` drops it.
    let mut rv_rx = connect_executor(&handle, "rv-b1", "riscv64-linux").await?;
    tick(&handle).await?;
    let assn = recv_assignment(&mut rv_rx).await;
    assert!(assn.drv_path.contains("rv-unroutable"));

    // Re-arm: riscv becomes unroutable again (only executor gone) → a
    // fresh riscv drv must trip a SECOND WARN.
    disconnect(&handle, "rv-b1").await?;
    let riscv2 = make_test_node("rv-unroutable-2", "riscv64-linux");
    let _ev2 = merge_dag(&handle, Uuid::new_v4(), vec![riscv2], vec![], false).await?;
    tick(&handle).await?;
    logs_assert(|lines| {
        let n = lines
            .iter()
            .filter(|l| l.contains("no registered executor advertises this system"))
            .count();
        if n == 2 {
            Ok(())
        } else {
            Err(format!(
                "WARN must re-arm after routable→unroutable; got {n}"
            ))
        }
    });

    Ok(())
}

// ─── ADR-023 SlaEstimator → SpawnIntent ────────────────────────────────

/// `compute_size_class_snapshot` emits one SpawnIntent per Ready non-FOD,
/// with `intent_id == drv_hash` and `cores ≈ solve_mvp(c_star)` for a
/// fitted key. Unfitted keys (no SlaEstimator entry) get probe defaults.
// r[verify sched.sla.intent-from-solve]
#[tokio::test]
async fn spawn_intent_from_sla_estimator() {
    use crate::sla::{config, solve, types::*};
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_classed(db.pool.clone(), &[("small", 60.0), ("large", 3600.0)]);
    // `sla_config.is_some()` gates spawn_intent emission (Static-mode
    // deployments must NOT emit intents — controller branches on
    // `intents.is_empty()`). The actor derives `sla_tiers`/`sla_ceilings`
    // from this in `new()`; here we set the actor fields directly so the
    // probe values below stay independent of the config struct's defaults.
    actor.sla_config = Some(config::SlaConfig {
        tiers: vec![config::Tier {
            name: "normal".into(),
            p50: None,
            p90: Some(1200.0),
            p99: None,
        }],
        default_tier: "normal".into(),
        probe: config::ProbeShape {
            cpu: 4.0,
            mem_per_core: 1 << 30,
            mem_base: 4 << 30,
        },
        feature_probes: Default::default(),
        max_cores: solve::DEFAULT_CEILINGS.max_cores,
        max_mem: solve::DEFAULT_CEILINGS.max_mem,
        max_disk: solve::DEFAULT_CEILINGS.max_disk,
        default_disk: solve::DEFAULT_CEILINGS.default_disk,
        fuse_cache_budget: 0,
        log_budget: 0,
        ring_buffer: 32,
        halflife_secs: 7.0 * 86400.0,
    });
    // Seed a single tier so the test exercises the Feasible path the way
    // a configured deploy would (empty ladder → solve_mvp BestEffort at
    // p̄ capped at max_cores).
    actor.sla_tiers = vec![solve::Tier {
        name: "normal".into(),
        p50: None,
        p90: Some(1200.0),
        p99: None,
    }];

    // Seed a fit for ("test-pkg", x86_64-linux, ""): Amdahl s=30 p=2000.
    // Against p90=1200, β=30-e^{-0.128}·1200≈-1026 → c*=2000/1026≈1.95
    // → ceil → 2 cores.
    actor.sla_estimator.seed(FittedParams {
        key: ModelKey {
            pname: "test-pkg".into(),
            system: "x86_64-linux".into(),
            tenant: String::new(),
        },
        fit: DurationFit::Amdahl {
            s: RefSeconds(30.0),
            p: RefSeconds(2000.0),
        },
        mem: MemFit::Independent {
            p90: MemBytes(6 << 30),
        },
        disk_p90: Some(DiskBytes(10 << 30)),
        sigma_resid: 0.1,
        log_residuals: Vec::new(),
        n_eff: 10.0,
        span: 8.0,
        explore: ExploreState {
            distinct_c: 3,
            min_c: RawCores(1.0),
            max_c: RawCores(32.0),
            frozen: false,
            saturated: false,
            last_wall: WallSeconds(0.0),
        },
        t_min_ci: None,
        ci_computed_at: None,
        tier: None,
    });

    // "fitted" matches the seeded key; "cold" has no fit (different
    // pname). Both Ready, both non-FOD. test_inject_ready uses the
    // first arg verbatim as drv_hash.
    actor.test_inject_ready("fitted", Some("test-pkg"), "x86_64-linux");
    actor.test_inject_ready("cold", Some("never-seen"), "x86_64-linux");

    let snap = actor.compute_size_class_snapshot(None);
    // Both classify into "small" (est_dur=0 → smallest covering class).
    let small = snap.iter().find(|s| s.name == "small").unwrap();
    assert_eq!(
        small.spawn_intents.len(),
        2,
        "one SpawnIntent per Ready non-FOD; queued={}",
        small.queued
    );
    assert_eq!(small.spawn_intents.len() as u64, small.queued);

    let fitted = small
        .spawn_intents
        .iter()
        .find(|i| i.intent_id == "fitted")
        .expect("intent_id == drv_hash");
    assert_eq!(
        fitted.cores, 2,
        "solve_mvp c_star ≈ 1.95 → ceil 2 (got {})",
        fitted.cores
    );
    assert_eq!(fitted.disk_bytes, 10 << 30, "disk_p90 from fit");
    assert!(fitted.mem_bytes >= 6 << 30, "mem ≥ p90 (× headroom)");

    let cold = small
        .spawn_intents
        .iter()
        .find(|i| i.intent_id == "cold")
        .unwrap();
    // No SlaEstimator entry + no [sla] config → fallback probe (4c, 8Gi).
    assert_eq!(cold.cores, 4, "no fit → fallback probe cores");
    assert_eq!(cold.mem_bytes, 8 << 30);
}

/// A worker heartbeating `intent_id == drv_hash` gets THAT drv even when
/// it isn't FIFO-first. Proves the `find_executor_with_overflow` intent
/// match preempts pick-from-queue. On miss (stale intent), the worker
/// falls through to FIFO.
// r[verify sched.sla.intent-match]
#[tokio::test]
async fn heartbeat_intent_id_prefers_precomputed_drv() -> TestResult {
    let (_db, handle, _task) = setup_with_classes(&[("small", 3600.0)]).await;

    // Two Ready drvs: "a" merged first (FIFO head), "b" second.
    let _rx = merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![make_node("a"), make_node("b")],
        vec![],
        false,
    )
    .await?;

    // Worker spawned for "b" (intent_id = drv_hash). Without the
    // intent-match it would get "a" (FIFO).
    let mut rx = connect_executor_with_intent(&handle, "w-b", "b").await?;
    handle.send_unchecked(ActorCommand::Tick).await?;
    let asg = recv_assignment(&mut rx).await;
    assert_eq!(
        asg.drv_path,
        test_drv_path("b"),
        "intent_id=b → worker gets b, not FIFO-head a"
    );

    // Stale-intent fallback: worker for "gone" (no such drv) → FIFO
    // pick-from-queue → gets "a".
    let mut rx2 = connect_executor_with_intent(&handle, "w-stale", "gone").await?;
    handle.send_unchecked(ActorCommand::Tick).await?;
    let asg2 = recv_assignment(&mut rx2).await;
    assert_eq!(
        asg2.drv_path,
        test_drv_path("a"),
        "no intent match → falls through to pick-from-queue"
    );
    Ok(())
}

/// Connect a size-classed builder with `intent_id` set. Local helper
/// for the ADR-023 intent-match tests above.
async fn connect_executor_with_intent(
    handle: &ActorHandle,
    executor_id: &str,
    intent_id: &str,
) -> anyhow::Result<mpsc::Receiver<rio_proto::types::SchedulerMessage>> {
    let (tx, rx) = mpsc::channel(16);
    handle
        .send_unchecked(ActorCommand::ExecutorConnected {
            executor_id: executor_id.into(),
            stream_tx: tx,
        })
        .await?;
    send_heartbeat_with(handle, executor_id, "x86_64-linux", |hb| {
        hb.size_class = Some("small".into());
        hb.intent_id = Some(intent_id.into());
    })
    .await?;
    handle
        .send_unchecked(ActorCommand::PrefetchComplete {
            executor_id: executor_id.into(),
            paths_fetched: 0,
        })
        .await?;
    Ok(rx)
}
