//! Dispatch: size-class routing, skip-ineligible, options propagation, interactive priority.
// r[verify sched.classify.smallest-covering]
// r[verify sched.overflow.up-only]

use super::*;

/// Size-class routing: a derivation classified as "large" goes only to
/// the large worker, even when a small worker has free capacity. The
/// overflow chain walks small→large but a large build never tries small.
#[tokio::test]
async fn test_size_class_routing_respects_classification() -> TestResult {
    use crate::assignment::SizeClassConfig;

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

    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |a| {
        a.with_size_classes(vec![
            SizeClassConfig {
                name: "small".into(),
                cutoff_secs: 30.0,
                mem_limit_bytes: u64::MAX,
                cpu_limit_cores: None,
            },
            SizeClassConfig {
                name: "large".into(),
                cutoff_secs: 3600.0,
                mem_limit_bytes: u64::MAX,
                cpu_limit_cores: None,
            },
        ])
    });

    // Connect TWO workers: one small (will NOT get the big build),
    // one large (will). Both idle with capacity.
    let (small_tx, mut small_rx) = mpsc::channel(256);
    handle
        .send_unchecked(ActorCommand::WorkerConnected {
            worker_id: "w-small".into(),
            stream_tx: small_tx,
        })
        .await?;
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            store_degraded: false,
            resources: None,
            bloom: None,
            size_class: Some("small".into()),
            worker_id: "w-small".into(),
            systems: vec!["x86_64-linux".into()],
            supported_features: vec![],
            max_builds: 4,
            running_builds: vec![],
        })
        .await?;

    let (large_tx, mut large_rx) = mpsc::channel(256);
    handle
        .send_unchecked(ActorCommand::WorkerConnected {
            worker_id: "w-large".into(),
            stream_tx: large_tx,
        })
        .await?;
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            store_degraded: false,
            resources: None,
            bloom: None,
            size_class: Some("large".into()),
            worker_id: "w-large".into(),
            systems: vec!["x86_64-linux".into()],
            supported_features: vec![],
            max_builds: 4,
            running_builds: vec![],
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
    let mut node = make_test_node("bigthing-hash", "x86_64-linux");
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
    let state = handle
        .debug_query_derivation("bigthing-hash")
        .await?
        .expect("exists");
    assert_eq!(
        state.assigned_size_class.as_deref(),
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
        setup_with_worker("x86-only-worker", "x86_64-linux", 2).await?;

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
    let arm_info = handle
        .debug_query_derivation("arm-hash")
        .await?
        .expect("exists");
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
        setup_with_worker("options-worker", "x86_64-linux", 1).await?;

    // Submit with build_timeout=300, max_silent_time=60.
    let build_id = Uuid::new_v4();
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            req: MergeDagRequest {
                build_id,
                tenant_id: None,
                priority_class: PriorityClass::Scheduled,
                nodes: vec![make_test_node("opts-hash", "x86_64-linux")],
                edges: vec![],
                options: BuildOptions {
                    max_silent_time: 60,
                    build_timeout: 300,
                    build_cores: 4,
                },
                keep_going: false,
                traceparent: String::new(),
            },
            reply: reply_tx,
        })
        .await?;
    let _rx = reply_rx.await??;

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
        setup_with_worker("trace-worker", "x86_64-linux", 1).await?;

    // Known traceparent (W3C format: version-trace_id-span_id-flags).
    // The exact bytes don't matter for this test — we just verify it
    // flows through unchanged.
    let known_tp = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

    let build_id = Uuid::new_v4();
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            req: MergeDagRequest {
                build_id,
                tenant_id: None,
                priority_class: PriorityClass::Scheduled,
                nodes: vec![make_test_node("trace-hash", "x86_64-linux")],
                edges: vec![],
                options: BuildOptions::default(),
                keep_going: false,
                traceparent: known_tp.to_string(),
            },
            reply: reply_tx,
        })
        .await?;
    let _rx = reply_rx.await??;

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
    // Worker with 0 slots so nothing dispatches until we send a heartbeat
    // with capacity (lets us merge TWICE before dispatch).
    let (db, handle, _task) = setup().await;
    let (stream_tx, mut stream_rx) = mpsc::channel(8);
    handle
        .send_unchecked(ActorCommand::WorkerConnected {
            worker_id: "dedup-worker".into(),
            stream_tx,
        })
        .await?;
    // Heartbeat with max_builds=0: registered but no capacity yet.
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            store_degraded: false,
            worker_id: "dedup-worker".into(),
            systems: vec!["x86_64-linux".into()],
            supported_features: vec![],
            max_builds: 0,
            running_builds: vec![],
            bloom: None,
            size_class: None,
            resources: None,
        })
        .await?;

    let tp_first = "00-11111111111111111111111111111111-1111111111111111-01";
    let tp_second = "00-22222222222222222222222222222222-2222222222222222-01";

    // First submit with tp_first.
    let (r1, rr1) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            req: MergeDagRequest {
                build_id: Uuid::new_v4(),
                tenant_id: None,
                priority_class: PriorityClass::Scheduled,
                nodes: vec![make_test_node("dedup-hash", "x86_64-linux")],
                edges: vec![],
                options: BuildOptions::default(),
                keep_going: false,
                traceparent: tp_first.to_string(),
            },
            reply: r1,
        })
        .await?;
    let _ = rr1.await??;

    // Second submit: SAME derivation, DIFFERENT traceparent (dedup hit).
    let (r2, rr2) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            req: MergeDagRequest {
                build_id: Uuid::new_v4(),
                tenant_id: None,
                priority_class: PriorityClass::Scheduled,
                nodes: vec![make_test_node("dedup-hash", "x86_64-linux")],
                edges: vec![],
                options: BuildOptions::default(),
                keep_going: false,
                traceparent: tp_second.to_string(),
            },
            reply: r2,
        })
        .await?;
    let _ = rr2.await??;

    // Now give capacity: heartbeat with max_builds=1 triggers dispatch.
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            store_degraded: false,
            worker_id: "dedup-worker".into(),
            systems: vec!["x86_64-linux".into()],
            supported_features: vec![],
            max_builds: 1,
            running_builds: vec![],
            bloom: None,
            size_class: None,
            resources: None,
        })
        .await?;

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
    let (stream_tx, mut stream_rx) = mpsc::channel(8);
    handle
        .send_unchecked(ActorCommand::WorkerConnected {
            worker_id: "upgrade-worker".into(),
            stream_tx,
        })
        .await?;
    // Zero capacity so we can merge twice before dispatch.
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            store_degraded: false,
            worker_id: "upgrade-worker".into(),
            systems: vec!["x86_64-linux".into()],
            supported_features: vec![],
            max_builds: 0,
            running_builds: vec![],
            bloom: None,
            size_class: None,
            resources: None,
        })
        .await?;

    // First merge with EMPTY traceparent (simulates recovery:
    // from_recovery_row/from_poisoned_row set traceparent="").
    let (r1, rr1) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            req: MergeDagRequest {
                build_id: Uuid::new_v4(),
                tenant_id: None,
                priority_class: PriorityClass::Scheduled,
                nodes: vec![make_test_node("upgrade-hash", "x86_64-linux")],
                edges: vec![],
                options: BuildOptions::default(),
                keep_going: false,
                traceparent: String::new(),
            },
            reply: r1,
        })
        .await?;
    let _ = rr1.await??;

    // Second merge with a REAL traceparent — dedup hit, should upgrade.
    let live_tp = "00-33333333333333333333333333333333-3333333333333333-01";
    let (r2, rr2) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            req: MergeDagRequest {
                build_id: Uuid::new_v4(),
                tenant_id: None,
                priority_class: PriorityClass::Scheduled,
                nodes: vec![make_test_node("upgrade-hash", "x86_64-linux")],
                edges: vec![],
                options: BuildOptions::default(),
                keep_going: false,
                traceparent: live_tp.to_string(),
            },
            reply: r2,
        })
        .await?;
    let _ = rr2.await??;

    // Give capacity → dispatch.
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            store_degraded: false,
            worker_id: "upgrade-worker".into(),
            systems: vec!["x86_64-linux".into()],
            supported_features: vec![],
            max_builds: 1,
            running_builds: vec![],
            bloom: None,
            size_class: None,
            resources: None,
        })
        .await?;

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
        setup_with_worker("prio-worker", "x86_64-linux", 1).await?;

    // Build 1: Scheduled, 2 independent leaves (Q, R). Both queue immediately.
    // Only 1 dispatches (worker has 1 slot); the other stays queued.
    let build1 = Uuid::new_v4();
    let _rx1 = merge_dag(
        &handle,
        build1,
        vec![
            make_test_node("prioQ", "x86_64-linux"),
            make_test_node("prioR", "x86_64-linux"),
        ],
        vec![],
        false,
    )
    .await?;

    // Build 2: Interactive, 2-node chain A → B. B is a leaf, A blocked.
    let p_prio_a = test_drv_path("prioA");
    let p_prio_b = test_drv_path("prioB");
    let build2 = Uuid::new_v4();
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            req: MergeDagRequest {
                build_id: build2,
                tenant_id: None,
                priority_class: PriorityClass::Interactive,
                nodes: vec![
                    make_test_node("prioA", "x86_64-linux"),
                    make_test_node("prioB", "x86_64-linux"),
                ],
                edges: vec![make_test_edge("prioA", "prioB")],
                options: BuildOptions::default(),
                keep_going: false,
                traceparent: String::new(),
            },
            reply: reply_tx,
        })
        .await?;
    let _rx2 = reply_rx.await??;

    // Drain the first assignment (one of Q/R/B — whichever dispatched first).
    // We don't care which; we only care what happens AFTER we complete it
    // in a way that makes A newly-ready.
    //
    // Strategy: complete EVERYTHING currently assigned with success until
    // prioB is completed. Then A becomes newly-ready with INTERACTIVE_BOOST,
    // and the NEXT dispatch should be A (not a leftover Q/R).
    let mut seen_paths = Vec::new();
    for _ in 0..4 {
        // Receive one assignment.
        let Some(msg) = worker_rx.recv().await else {
            break;
        };
        let Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) = msg.msg else {
            continue;
        };
        let path = a.drv_path.clone();
        seen_paths.push(path.clone());
        // Complete it.
        complete_success(&handle, "prio-worker", &path, &test_store_path("out")).await?;
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
    let (_db, handle, _task, mut rx) = setup_with_worker("w1", "x86_64-linux", 4).await?;

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
    // Trailing suffix check — the token is "{worker_id}-{drv_hash}-{gen}",
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
    let (_db, handle, _task, mut rx) = setup_with_worker("w1", "x86_64-linux", 4).await?;

    // Two-node chain: child (leaf) → parent (depends on child).
    // merge_dag edges point parent→child (parent's input is child's output).
    //
    // make_test_node leaves expected_output_paths EMPTY by default —
    // most tests don't care about it. approx_input_closure DOES care
    // (that's what it iterates). Populate explicitly.
    let child_out = rio_test_support::fixtures::test_store_path("child-out");
    let mut child = make_test_node("child", "x86_64-linux");
    child.expected_output_paths = vec![child_out.clone()];
    let parent = make_test_node("parent", "x86_64-linux");
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

    // Complete the child so parent becomes ready.
    complete_success_empty(&handle, "w1", "child").await?;

    // Parent has one child in the DAG (the completed "child"). Its
    // approx_input_closure = child's expected_output_paths. Worker
    // has no bloom → pessimistic → send all. Hint arrives FIRST.
    let second = rx.recv().await.expect("prefetch hint");
    let hint = match second.msg {
        Some(rio_proto::types::scheduler_message::Msg::Prefetch(h)) => h,
        other => panic!("expected PrefetchHint before parent's Assignment, got {other:?}"),
    };
    assert_eq!(
        hint.store_paths,
        vec![child_out],
        "hint = child's output path (parent's direct input via DAG children)"
    );

    // THEN the assignment.
    let third = rx.recv().await.expect("parent assignment");
    match third.msg {
        Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) => {
            assert!(a.drv_path.contains("parent"));
        }
        other => panic!("expected Assignment after hint, got {other:?}"),
    }

    Ok(())
}

/// Bloom filter skips paths the worker claims to have. Scoring and
/// hinting use the SAME approx_input_closure — so if scoring picks
/// a warm worker (most paths cached), the hint should be SMALL
/// (only what's actually missing). That's the optimization working
/// together.
///
/// We construct a bloom that claims to have ONE of two input paths.
/// The hint should contain only the OTHER.
#[tokio::test]
async fn test_prefetch_hint_bloom_filters() -> TestResult {
    use rio_common::bloom::BloomFilter;

    let (_db, handle, _task, mut rx) = setup_with_worker("w1", "x86_64-linux", 4).await?;

    // Three-node: parent depends on child_a AND child_b. After
    // both complete, parent dispatches with 2 input paths.
    let out_a = rio_test_support::fixtures::test_store_path("ca-out");
    let out_b = rio_test_support::fixtures::test_store_path("cb-out");
    let mut child_a = make_test_node("ca", "x86_64-linux");
    child_a.expected_output_paths = vec![out_a.clone()];
    let mut child_b = make_test_node("cb", "x86_64-linux");
    child_b.expected_output_paths = vec![out_b.clone()];
    let parent = make_test_node("parent", "x86_64-linux");
    let edges = vec![
        make_test_edge("parent", "ca"),
        make_test_edge("parent", "cb"),
    ];

    let _ev = merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![child_a, child_b, parent],
        edges,
        false,
    )
    .await?;

    // Drain leaf assignments (2 children, no hints — leaves).
    // Order nondeterministic (same priority, HashMap iteration).
    for _ in 0..2 {
        let msg = rx.recv().await.expect("leaf assignment");
        assert!(matches!(
            msg.msg,
            Some(rio_proto::types::scheduler_message::Msg::Assignment(_))
        ));
    }

    // ORDER MATTERS: bloom heartbeat BEFORE the last completion.
    // complete_success fires dispatch_ready internally; if parent
    // becomes ready then, it dispatches with whatever bloom the
    // worker had AT THAT MOMENT. Send the bloom first, so when
    // the second completion makes parent ready, the filter is live.
    //
    // Complete ONE child first (parent not yet ready — still
    // waiting on cb).
    complete_success_empty(&handle, "w1", "ca").await?;

    // Now the bloom. Size for 10 items at 1% FPR — way bigger than
    // needed for 1 insert, so false positives on out_b are
    // astronomically unlikely.
    let mut bloom = BloomFilter::new(10, 0.01);
    bloom.insert(&out_a);
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            store_degraded: false,
            resources: None,
            worker_id: "w1".into(),
            systems: vec!["x86_64-linux".into()],
            supported_features: vec![],
            max_builds: 4,
            running_builds: vec![],
            bloom: Some(bloom),
            size_class: None,
        })
        .await?;

    // NOW the second completion. Parent becomes ready → dispatch
    // fires → send_prefetch_hint reads the bloom we just sent.
    complete_success_empty(&handle, "w1", "cb").await?;

    // Parent dispatches.
    // Hint should skip out_a (bloom says worker has it), include
    // out_b (bloom says missing).
    let hint_msg = rx.recv().await.expect("filtered hint");
    let hint = match hint_msg.msg {
        Some(rio_proto::types::scheduler_message::Msg::Prefetch(h)) => h,
        other => panic!("expected filtered PrefetchHint, got {other:?}"),
    };
    assert_eq!(
        hint.store_paths,
        vec![out_b],
        "bloom filtered out_a (worker has it); hint only out_b. \
         If both present: filter not applied. If only out_a: filter inverted."
    );

    Ok(())
}

/// Worker with a bloom claiming EVERYTHING → empty filtered set →
/// no hint message at all. This is the best case: best_worker picked
/// a fully-warm worker, nothing to prefetch. Saves one try_send.
#[tokio::test]
async fn test_prefetch_hint_skipped_when_bloom_covers_all() -> TestResult {
    use rio_common::bloom::BloomFilter;

    let (_db, handle, _task, mut rx) = setup_with_worker("w1", "x86_64-linux", 4).await?;

    let child_out = rio_test_support::fixtures::test_store_path("child-out");
    let mut child = make_test_node("child", "x86_64-linux");
    child.expected_output_paths = vec![child_out.clone()];
    let parent = make_test_node("parent", "x86_64-linux");
    let edge = make_test_edge("parent", "child");

    let _ev = merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![child, parent],
        vec![edge],
        false,
    )
    .await?;

    // Drain child's assignment (leaf).
    let _ = rx.recv().await.expect("child assignment");

    // Bloom BEFORE completion (same ordering as the filter test —
    // completion fires dispatch, so bloom must be in place first).
    let mut bloom = BloomFilter::new(10, 0.01);
    bloom.insert(&child_out);
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            store_degraded: false,
            resources: None,
            worker_id: "w1".into(),
            systems: vec!["x86_64-linux".into()],
            supported_features: vec![],
            max_builds: 4,
            running_builds: vec![],
            bloom: Some(bloom),
            size_class: None,
        })
        .await?;

    // NOW complete. Parent ready → dispatch → hint filtered → empty
    // → not sent.
    complete_success_empty(&handle, "w1", "child").await?;

    // Parent dispatches. Hint filtered to empty → NOT sent. First
    // message is the Assignment directly.
    let msg = rx.recv().await.expect("parent message");
    match msg.msg {
        Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) => {
            assert!(
                a.drv_path.contains("parent"),
                "no hint when bloom covers all inputs — straight to Assignment"
            );
        }
        Some(rio_proto::types::scheduler_message::Msg::Prefetch(h)) => {
            panic!(
                "hint sent despite bloom covering all inputs: {h:?}. \
                 Empty-hint early-return in send_prefetch_hint not working."
            );
        }
        other => panic!("unexpected: {other:?}"),
    }

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
    let (_db, handle, _task, mut rx) = setup_with_worker("w1", "x86_64-linux", 4).await?;

    // Two independent derivations in the same dispatch pass (worker has
    // capacity 4, both merge before capacity exhausts).
    let _ev1 = merge_single_node(&handle, Uuid::new_v4(), "a", PriorityClass::Scheduled).await?;
    let _ev2 = merge_single_node(&handle, Uuid::new_v4(), "b", PriorityClass::Scheduled).await?;

    let a1 = recv_assignment(&mut rx).await;
    let a2 = recv_assignment(&mut rx).await;

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
#[tokio::test]
async fn test_pin_unpin_live_inputs_lifecycle() -> TestResult {
    let (db, handle, _task, mut stream_rx) = setup_with_worker("w-x9", "x86_64-linux", 2).await?;

    // Two-node chain: child (leaf, no inputs) + parent (depends
    // on child). Parent's approx_input_closure = child's
    // expected_output_paths. Dispatch of PARENT should pin those.
    //
    // make_test_node defaults expected_output_paths=vec![]; set
    // explicitly so approx_input_closure has something to collect.
    let build_id = Uuid::new_v4();
    let child_out = test_store_path("x9-child-out");
    let mut child = make_test_node("x9-child", "x86_64-linux");
    child.expected_output_paths = vec![child_out.clone()];
    let parent = make_test_node("x9-parent", "x86_64-linux");
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
    complete_success_empty(&handle, "w-x9", "x9-child").await?;
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
    complete_success_empty(&handle, "w-x9", "x9-parent").await?;
    barrier(&handle).await;

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM scheduler_live_pins WHERE drv_hash = 'x9-parent'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(count, 0, "completion should unpin");

    Ok(())
}
