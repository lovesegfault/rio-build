//! Dispatch: size-class routing, skip-ineligible, options propagation, interactive priority.

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

    // Actor with size_classes configured. setup_actor_with_store
    // builds DagActor::new() directly, so we can't inject the config
    // that way — build the actor inline with .with_size_classes().
    let (tx, rx) = mpsc::channel(ACTOR_CHANNEL_CAPACITY);
    let actor = DagActor::new(SchedulerDb::new(db.pool.clone()), None).with_size_classes(vec![
        SizeClassConfig {
            name: "small".into(),
            cutoff_secs: 30.0,
            mem_limit_bytes: u64::MAX,
        },
        SizeClassConfig {
            name: "large".into(),
            cutoff_secs: 3600.0,
            mem_limit_bytes: u64::MAX,
        },
    ]);
    let backpressure = actor.backpressure_flag();
    let self_tx = tx.downgrade();
    let _task = tokio::spawn(actor.run_with_self_tx(rx, self_tx));
    let handle = ActorHandle { tx, backpressure };

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
            bloom: None,
            size_class: Some("small".into()),
            worker_id: "w-small".into(),
            system: "x86_64-linux".into(),
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
            bloom: None,
            size_class: Some("large".into()),
            worker_id: "w-large".into(),
            system: "x86_64-linux".into(),
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
/// to the worker via WorkAssignment. Previously sent all-zeros defaults.
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

/// Interactive builds get a priority boost (D5: +1e9 instead of the old
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
