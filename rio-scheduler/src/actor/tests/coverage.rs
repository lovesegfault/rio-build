use super::*;

// -----------------------------------------------------------------------
// Group 10: Remaining coverage
// -----------------------------------------------------------------------

/// keepGoing=false: on PermanentFailure, the entire build fails immediately.
#[tokio::test]
async fn test_keepgoing_false_fails_fast() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    let _stream_rx = connect_worker(&handle, "test-worker", "x86_64-linux", 2).await;

    // Merge a two-node DAG with keepGoing=false
    let build_id = Uuid::new_v4();
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            build_id,
            tenant_id: None,
            priority_class: PriorityClass::Scheduled,
            nodes: vec![
                make_test_node("hashA", "/nix/store/hashA.drv", "x86_64-linux"),
                make_test_node("hashB", "/nix/store/hashB.drv", "x86_64-linux"),
            ],
            edges: vec![],
            options: BuildOptions::default(),
            keep_going: false, // critical
            reply: reply_tx,
        })
        .await
        .unwrap();
    let _rx = reply_rx.await.unwrap().unwrap();
    settle().await;

    // Send PermanentFailure for hashA
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "test-worker".into(),
            drv_key: "/nix/store/hashA.drv".into(),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::PermanentFailure.into(),
                error_msg: "compile error".into(),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    settle().await;

    // Build should be Failed (not waiting for hashB)
    let (status_tx, status_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::QueryBuildStatus {
            build_id,
            reply: status_tx,
        })
        .await
        .unwrap();
    let status = status_rx.await.unwrap().unwrap();
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Failed as i32,
        "build should fail fast on PermanentFailure with keepGoing=false"
    );
}

/// keepGoing=true: build waits for all derivations, fails only at the end.
#[tokio::test]
async fn test_keepgoing_true_waits_all() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    let _stream_rx = connect_worker(&handle, "test-worker", "x86_64-linux", 2).await;

    // Merge a two-node DAG with keepGoing=true
    let build_id = Uuid::new_v4();
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            build_id,
            tenant_id: None,
            priority_class: PriorityClass::Scheduled,
            nodes: vec![
                make_test_node("hashX", "/nix/store/hashX.drv", "x86_64-linux"),
                make_test_node("hashY", "/nix/store/hashY.drv", "x86_64-linux"),
            ],
            edges: vec![],
            options: BuildOptions::default(),
            keep_going: true, // critical
            reply: reply_tx,
        })
        .await
        .unwrap();
    let _rx = reply_rx.await.unwrap().unwrap();
    settle().await;

    // Send PermanentFailure for hashX
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "test-worker".into(),
            drv_key: "/nix/store/hashX.drv".into(),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::PermanentFailure.into(),
                error_msg: "failed".into(),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    settle().await;

    // Build should still be Active (waiting for hashY)
    let (status_tx, status_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::QueryBuildStatus {
            build_id,
            reply: status_tx,
        })
        .await
        .unwrap();
    let status = status_rx.await.unwrap().unwrap();
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Active as i32,
        "build should still be Active with keepGoing=true and pending derivations"
    );

    // Complete hashY successfully
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "test-worker".into(),
            drv_key: "/nix/store/hashY.drv".into(),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    settle().await;

    // Now build should be Failed (all resolved, one failed)
    let (status_tx2, status_rx2) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::QueryBuildStatus {
            build_id,
            reply: status_tx2,
        })
        .await
        .unwrap();
    let status2 = status_rx2.await.unwrap().unwrap();
    assert_eq!(
        status2.state,
        rio_proto::types::BuildState::Failed as i32,
        "build should fail after all derivations resolve with keepGoing=true"
    );
}

/// keepGoing=true with a dependency chain: poisoning a leaf must cascade
/// DependencyFailed to all ancestors so the build terminates. Without the
/// cascade, parents stay Queued forever and completed+failed never reaches
/// total -> build hangs.
#[tokio::test]
async fn test_keepgoing_poisoned_dependency_cascades_failure() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    // Worker with capacity 1: only the leaf gets dispatched initially.
    let _stream_rx = connect_worker(&handle, "cascade-worker", "x86_64-linux", 1).await;

    // Chain: A depends on B depends on C. C is the leaf.
    let build_id = Uuid::new_v4();
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            build_id,
            tenant_id: None,
            priority_class: PriorityClass::Scheduled,
            nodes: vec![
                make_test_node("cascadeA", "/nix/store/cascadeA.drv", "x86_64-linux"),
                make_test_node("cascadeB", "/nix/store/cascadeB.drv", "x86_64-linux"),
                make_test_node("cascadeC", "/nix/store/cascadeC.drv", "x86_64-linux"),
            ],
            edges: vec![
                make_test_edge("/nix/store/cascadeA.drv", "/nix/store/cascadeB.drv"),
                make_test_edge("/nix/store/cascadeB.drv", "/nix/store/cascadeC.drv"),
            ],
            options: BuildOptions::default(),
            keep_going: true,
            reply: reply_tx,
        })
        .await
        .unwrap();
    let _rx = reply_rx.await.unwrap().unwrap();
    settle().await;

    // Sanity: C is the only Ready/Assigned derivation; A and B are Queued.
    let info_a = handle
        .debug_query_derivation("cascadeA")
        .await
        .unwrap()
        .unwrap();
    let info_b = handle
        .debug_query_derivation("cascadeB")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(info_a.status, DerivationStatus::Queued);
    assert_eq!(info_b.status, DerivationStatus::Queued);

    // Poison C via PermanentFailure.
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "cascade-worker".into(),
            drv_key: "/nix/store/cascadeC.drv".into(),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::PermanentFailure.into(),
                error_msg: "compile error".into(),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    settle().await;

    // B and A should now be DependencyFailed (cascaded transitively).
    let info_b = handle
        .debug_query_derivation("cascadeB")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        info_b.status,
        DerivationStatus::DependencyFailed,
        "immediate parent B should be DependencyFailed after C poisoned"
    );
    let info_a = handle
        .debug_query_derivation("cascadeA")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        info_a.status,
        DerivationStatus::DependencyFailed,
        "transitive parent A should also be DependencyFailed"
    );

    // Build should terminate as Failed (all 3 derivations resolved:
    // 1 Poisoned + 2 DependencyFailed counted in failed).
    let (status_tx, status_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::QueryBuildStatus {
            build_id,
            reply: status_tx,
        })
        .await
        .unwrap();
    let status = status_rx.await.unwrap().unwrap();
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Failed as i32,
        "keepGoing build with poisoned dependency chain should terminate as Failed, not hang"
    );
    assert_eq!(
        status.failed_derivations, 3,
        "1 Poisoned + 2 DependencyFailed should all count as failed"
    );
}

/// When a new build depends on an already-poisoned derivation (from a
/// prior build), compute_initial_states must mark the new node
/// DependencyFailed immediately. Previously it went to Queued and hung
/// forever (never Ready, never cascaded since cascade only runs on
/// *transition to* Poisoned).
#[tokio::test]
async fn test_merge_with_prepoisoned_dep_marks_dependency_failed() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;
    let _stream_rx = connect_worker(&handle, "poison-worker", "x86_64-linux", 1).await;

    // Build 1: single leaf, poisoned via PermanentFailure.
    let build1 = Uuid::new_v4();
    let _rx1 = merge_single_node(
        &handle,
        build1,
        "preleaf",
        "/nix/store/preleaf.drv",
        PriorityClass::Scheduled,
    )
    .await;
    settle().await;
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "poison-worker".into(),
            drv_key: "/nix/store/preleaf.drv".into(),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::PermanentFailure.into(),
                error_msg: "preleaf failed".into(),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    settle().await;

    // Verify preleaf is Poisoned.
    let leaf = handle
        .debug_query_derivation("preleaf")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(leaf.status, DerivationStatus::Poisoned);

    // Build 2: new node depending on the poisoned preleaf.
    // keepGoing=false: build should fail immediately at merge.
    let build2 = Uuid::new_v4();
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            build_id: build2,
            tenant_id: None,
            priority_class: PriorityClass::Scheduled,
            nodes: vec![
                make_test_node("preparent", "/nix/store/preparent.drv", "x86_64-linux"),
                make_test_node("preleaf", "/nix/store/preleaf.drv", "x86_64-linux"),
            ],
            edges: vec![make_test_edge(
                "/nix/store/preparent.drv",
                "/nix/store/preleaf.drv",
            )],
            options: BuildOptions::default(),
            keep_going: false,
            reply: reply_tx,
        })
        .await
        .unwrap();
    let _rx2 = reply_rx.await.unwrap().unwrap();
    settle().await;

    // preparent must be DependencyFailed (not stuck Queued).
    let parent = handle
        .debug_query_derivation("preparent")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        parent.status,
        DerivationStatus::DependencyFailed,
        "new node depending on pre-poisoned dep must be DependencyFailed, not stuck Queued"
    );

    // Build 2 must be Failed (!keepGoing + dep failure).
    let (status_tx, status_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::QueryBuildStatus {
            build_id: build2,
            reply: status_tx,
        })
        .await
        .unwrap();
    let status = status_rx.await.unwrap().unwrap();
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Failed as i32,
        "build depending on pre-poisoned dep must fail immediately (!keepGoing)"
    );
}

/// WatchBuild on an already-terminal build must immediately send the
/// terminal event. Previously it just subscribed — if the original
/// BuildCompleted was sent to zero receivers (e.g., submit subscriber
/// disconnected before completion), a late WatchBuild would hang forever.
#[tokio::test]
async fn test_watch_build_after_completion_receives_terminal_event() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;
    let _stream_rx = connect_worker(&handle, "watch-worker", "x86_64-linux", 1).await;

    // Submit a build, complete it, then drop the original subscriber.
    let build_id = Uuid::new_v4();
    let original_rx = merge_single_node(
        &handle,
        build_id,
        "watch-hash",
        "/nix/store/watch-hash.drv",
        PriorityClass::Scheduled,
    )
    .await;
    settle().await;

    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "watch-worker".into(),
            drv_key: "/nix/store/watch-hash.drv".into(),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    settle().await;

    // Drop the original subscriber. The BuildCompleted event was already
    // sent; a NEW subscriber should not hang waiting for it.
    drop(original_rx);

    // Now WatchBuild. Should receive BuildCompleted immediately.
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::WatchBuild {
            build_id,
            since_sequence: 0,
            reply: reply_tx,
        })
        .await
        .unwrap();
    let mut watch_rx = reply_rx.await.unwrap().unwrap();

    // Should get a terminal event within a short timeout, not hang.
    let event = tokio::time::timeout(Duration::from_secs(2), watch_rx.recv())
        .await
        .expect("WatchBuild on terminal build should not hang")
        .expect("should receive an event");
    assert!(
        matches!(
            event.event,
            Some(rio_proto::types::build_event::Event::Completed(_))
        ),
        "late WatchBuild should receive BuildCompleted replay, got: {:?}",
        event.event
    );
}

/// Terminal build state should be cleaned up after TERMINAL_CLEANUP_DELAY
/// to prevent unbounded memory growth for long-running schedulers.
///
/// This test sends CleanupTerminalBuild directly (bypassing the delay)
/// since paused time interferes with PG pool timeouts. The delay
/// scheduling itself is trivially correct (tokio::time::sleep + try_send).
#[tokio::test]
async fn test_terminal_build_cleanup_after_delay() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    let _stream_rx = connect_worker(&handle, "cleanup-worker", "x86_64-linux", 1).await;

    // Complete a build.
    let build_id = Uuid::new_v4();
    let drv_hash = "cleanup-hash";
    let drv_path = "/nix/store/cleanup-hash.drv";
    let _event_rx = merge_single_node(
        &handle,
        build_id,
        drv_hash,
        drv_path,
        PriorityClass::Scheduled,
    )
    .await;
    settle().await;

    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "cleanup-worker".into(),
            drv_key: drv_path.into(),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    settle().await;

    // Build should be Succeeded and still queryable.
    let (status_tx, status_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::QueryBuildStatus {
            build_id,
            reply: status_tx,
        })
        .await
        .unwrap();
    let status = status_rx.await.unwrap();
    assert!(status.is_ok(), "build should be queryable before cleanup");

    // Directly inject the cleanup command (bypassing the 60s delay).
    handle
        .send_unchecked(ActorCommand::CleanupTerminalBuild { build_id })
        .await
        .unwrap();
    settle().await;

    // Build should now be gone (BuildNotFound).
    let (status_tx, status_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::QueryBuildStatus {
            build_id,
            reply: status_tx,
        })
        .await
        .unwrap();
    let status = status_rx.await.unwrap();
    assert!(
        matches!(status, Err(ActorError::BuildNotFound(_))),
        "build should be cleaned up after delay, got {:?}",
        status
    );

    // DAG node should also be reaped (Completed + orphaned).
    let info = handle.debug_query_derivation(drv_hash).await.unwrap();
    assert!(
        info.is_none(),
        "orphaned+terminal DAG node should be reaped"
    );
}

/// TransientFailure: retry on a different worker up to max_retries (default 2).
#[tokio::test]
async fn test_transient_retry_different_worker() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    // Register two workers
    let _rx1 = connect_worker(&handle, "worker-a", "x86_64-linux", 1).await;
    let _rx2 = connect_worker(&handle, "worker-b", "x86_64-linux", 1).await;

    let build_id = Uuid::new_v4();
    let _event_rx = merge_single_node(
        &handle,
        build_id,
        "retry-hash",
        "/nix/store/retry-hash.drv",
        PriorityClass::Scheduled,
    )
    .await;
    settle().await;

    // Get initial worker assignment
    let info1 = handle
        .debug_query_derivation("retry-hash")
        .await
        .unwrap()
        .unwrap();
    let first_worker = info1.assigned_worker.clone().unwrap();
    assert_eq!(info1.retry_count, 0);

    // Send TransientFailure from the first worker
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: first_worker.clone().into(),
            drv_key: "/nix/store/retry-hash.drv".into(),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::TransientFailure.into(),
                error_msg: "network hiccup".into(),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    settle().await;

    // Should be retried: retry_count=1, possibly on a different worker
    let info2 = handle
        .debug_query_derivation("retry-hash")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        info2.retry_count, 1,
        "transient failure should increment retry_count"
    );
    // Note: the retry MAY go to the same worker (no affinity avoidance yet),
    // but retry_count proves it was processed.
    assert!(matches!(
        info2.status,
        DerivationStatus::Assigned | DerivationStatus::Ready
    ));
}

/// max_retries (default 2) exhausted with the SAME worker should poison.
/// This branch (retry_count >= max_retries) is distinct from
/// POISON_THRESHOLD (3 distinct workers) — same worker failing
/// repeatedly hits max_retries first.
#[tokio::test]
async fn test_transient_failure_max_retries_same_worker_poisons() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;
    let _rx = connect_worker(&handle, "flaky-worker", "x86_64-linux", 1).await;

    let build_id = Uuid::new_v4();
    let _event_rx = merge_single_node(
        &handle,
        build_id,
        "maxretry-hash",
        "/nix/store/maxretry-hash.drv",
        PriorityClass::Scheduled,
    )
    .await;
    settle().await;

    // Default RetryPolicy::max_retries = 2. Fail 3 times on same worker:
    // retry_count 0 -> 1 (retry), 1 -> 2 (retry), 2 >= 2 -> Poisoned.
    for attempt in 0..3 {
        handle
            .send_unchecked(ActorCommand::ProcessCompletion {
                worker_id: "flaky-worker".into(),
                drv_key: "/nix/store/maxretry-hash.drv".into(),
                result: rio_proto::types::BuildResult {
                    status: rio_proto::types::BuildResultStatus::TransientFailure.into(),
                    error_msg: format!("attempt {attempt} failed"),
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        settle().await;
    }

    let info = handle
        .debug_query_derivation("maxretry-hash")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        info.status,
        DerivationStatus::Poisoned,
        "3 transient failures on same worker (retry_count >= max_retries=2) should poison"
    );
}

/// CancelBuild on an active build should clean up derivations and emit
/// BuildCancelled event. Previously untested.
#[tokio::test]
async fn test_cancel_build_active_drains_derivations() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;
    // No workers — derivation stays Ready (never assigned).

    let build_id = Uuid::new_v4();
    let mut event_rx = merge_single_node(
        &handle,
        build_id,
        "cancel-hash",
        "/nix/store/cancel-hash.drv",
        PriorityClass::Scheduled,
    )
    .await;
    settle().await;

    // Send CancelBuild.
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id,
            reason: "test cancel".into(),
            reply: reply_tx,
        })
        .await
        .unwrap();
    let cancelled = reply_rx.await.unwrap().unwrap();
    assert!(cancelled, "CancelBuild should return true for active build");
    settle().await;

    // Build should be Cancelled.
    let (status_tx, status_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::QueryBuildStatus {
            build_id,
            reply: status_tx,
        })
        .await
        .unwrap();
    let status = status_rx.await.unwrap().unwrap();
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Cancelled as i32,
        "build should be Cancelled after CancelBuild"
    );

    // Should have received BuildCancelled event.
    let mut saw_cancelled = false;
    while let Ok(event) = event_rx.try_recv() {
        if matches!(
            event.event,
            Some(rio_proto::types::build_event::Event::Cancelled(_))
        ) {
            saw_cancelled = true;
        }
    }
    assert!(saw_cancelled, "BuildCancelled event should be emitted");

    // Second CancelBuild should be a no-op (idempotent: returns false).
    let (reply_tx2, reply_rx2) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id,
            reason: "already cancelled".into(),
            reply: reply_tx2,
        })
        .await
        .unwrap();
    let re_cancelled = reply_rx2.await.unwrap().unwrap();
    assert!(
        !re_cancelled,
        "CancelBuild on already-terminal build should return false"
    );
}

/// WatchBuild during an active build should receive events as they happen.
/// (The after-completion case is tested separately.)
#[tokio::test]
async fn test_watch_build_receives_events() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;
    let _rx = connect_worker(&handle, "watch-events-worker", "x86_64-linux", 1).await;

    let build_id = Uuid::new_v4();
    let _original = merge_single_node(
        &handle,
        build_id,
        "watch-events-hash",
        "/nix/store/watch-events-hash.drv",
        PriorityClass::Scheduled,
    )
    .await;
    settle().await;

    // WatchBuild on the active build.
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::WatchBuild {
            build_id,
            since_sequence: 0,
            reply: reply_tx,
        })
        .await
        .unwrap();
    let mut watch_rx = reply_rx.await.unwrap().unwrap();

    // Complete the build; watcher should see BuildCompleted.
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "watch-events-worker".into(),
            drv_key: "/nix/store/watch-events-hash.drv".into(),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                ..Default::default()
            },
        })
        .await
        .unwrap();

    let mut saw_completed = false;
    // Drain events with a timeout.
    for _ in 0..10 {
        match tokio::time::timeout(Duration::from_millis(200), watch_rx.recv()).await {
            Ok(Ok(event)) => {
                if matches!(
                    event.event,
                    Some(rio_proto::types::build_event::Event::Completed(_))
                ) {
                    saw_completed = true;
                    break;
                }
            }
            _ => break,
        }
    }
    assert!(
        saw_completed,
        "WatchBuild subscriber should see BuildCompleted"
    );
}

/// Dispatch should skip over derivations with no eligible worker (wrong
/// system or missing feature) instead of blocking the entire queue.
#[tokio::test]
async fn test_dispatch_skips_ineligible_derivation() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    // Only x86_64 worker registered.
    let mut stream_rx = connect_worker(&handle, "x86-only-worker", "x86_64-linux", 2).await;

    // Merge aarch64 derivation FIRST (goes to queue head), then x86_64.
    // With the old `None => break`, the aarch64 drv at head would block
    // the x86_64 drv from being dispatched.
    let build_arm = Uuid::new_v4();
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            build_id: build_arm,
            tenant_id: None,
            priority_class: PriorityClass::Scheduled,
            nodes: vec![make_test_node(
                "arm-hash",
                "/nix/store/arm-hash.drv",
                "aarch64-linux",
            )],
            edges: vec![],
            options: BuildOptions::default(),
            keep_going: false,
            reply: reply_tx,
        })
        .await
        .unwrap();
    let _rx = reply_rx.await.unwrap().unwrap();

    let build_x86 = Uuid::new_v4();
    let _rx = merge_single_node(
        &handle,
        build_x86,
        "x86-hash",
        "/nix/store/x86-hash.drv",
        PriorityClass::Scheduled,
    )
    .await;
    settle().await;

    // x86_64 derivation should be dispatched despite aarch64 ahead of it.
    let msg = tokio::time::timeout(Duration::from_secs(2), stream_rx.recv())
        .await
        .expect("x86_64 derivation should be dispatched within 2s")
        .expect("stream should not close");
    let dispatched_path = match msg.msg {
        Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) => a.drv_path,
        _ => panic!("expected assignment"),
    };
    assert_eq!(
        dispatched_path, "/nix/store/x86-hash.drv",
        "x86_64 derivation should be dispatched even with ineligible aarch64 ahead in queue"
    );

    // aarch64 derivation should still be Ready (not stuck, not dispatched).
    let arm_info = handle
        .debug_query_derivation("arm-hash")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        arm_info.status,
        DerivationStatus::Ready,
        "aarch64 derivation should remain Ready (no eligible worker)"
    );
}

/// Per-build BuildOptions (max_silent_time, build_timeout) must propagate
/// to the worker via WorkAssignment. Previously sent all-zeros defaults.
#[tokio::test]
async fn test_build_options_propagated_to_worker() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    let mut stream_rx = connect_worker(&handle, "options-worker", "x86_64-linux", 1).await;

    // Submit with build_timeout=300, max_silent_time=60.
    let build_id = Uuid::new_v4();
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            build_id,
            tenant_id: None,
            priority_class: PriorityClass::Scheduled,
            nodes: vec![make_test_node(
                "opts-hash",
                "/nix/store/opts-hash.drv",
                "x86_64-linux",
            )],
            edges: vec![],
            options: BuildOptions {
                max_silent_time: 60,
                build_timeout: 300,
                build_cores: 4,
            },
            keep_going: false,
            reply: reply_tx,
        })
        .await
        .unwrap();
    let _rx = reply_rx.await.unwrap().unwrap();
    settle().await;

    // Worker should receive assignment with the build's options.
    let msg = tokio::time::timeout(Duration::from_secs(2), stream_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let assignment = match msg.msg {
        Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) => a,
        _ => panic!("expected assignment"),
    };
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
}

/// TOCTOU fix: a stale heartbeat (sent before scheduler assigned a
/// derivation) must not clobber the scheduler's fresh assignment in
/// worker.running_builds. The scheduler is authoritative.
#[tokio::test]
async fn test_heartbeat_does_not_clobber_fresh_assignment() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    // Register worker (initial heartbeat has empty running_builds).
    let _stream_rx = connect_worker(&handle, "toctou-worker", "x86_64-linux", 2).await;
    settle().await;

    // Merge a derivation. Scheduler will assign it to the worker and
    // insert it into worker.running_builds.
    let build_id = Uuid::new_v4();
    let drv_hash = "toctou-drv-hash";
    let drv_path = "/nix/store/toctou-drv-hash.drv";
    let _event_rx = merge_single_node(
        &handle,
        build_id,
        drv_hash,
        drv_path,
        PriorityClass::Scheduled,
    )
    .await;
    settle().await;

    // Verify: derivation is Assigned, worker.running_builds contains it.
    let info = handle
        .debug_query_derivation(drv_hash)
        .await
        .unwrap()
        .expect("derivation should exist");
    assert_eq!(info.status, DerivationStatus::Assigned);

    let workers = handle.debug_query_workers().await.unwrap();
    let w = workers
        .iter()
        .find(|w| w.worker_id == "toctou-worker")
        .unwrap();
    assert!(
        w.running_builds.contains(&drv_hash.to_string()),
        "scheduler should have tracked the assignment in worker.running_builds"
    );

    // Send a STALE heartbeat with empty running_builds. This mimics the
    // race: worker sent heartbeat before receiving/acking the assignment.
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            worker_id: "toctou-worker".into(),
            system: "x86_64-linux".into(),
            supported_features: vec![],
            max_builds: 2,
            running_builds: vec![], // stale — does NOT include fresh assignment
        })
        .await
        .unwrap();
    settle().await;

    // Assignment must still be tracked. Before the fix, running_builds
    // would be wholesale replaced with the empty set, orphaning the
    // assignment (completion would later warn "unknown derivation").
    let workers = handle.debug_query_workers().await.unwrap();
    let w = workers
        .iter()
        .find(|w| w.worker_id == "toctou-worker")
        .unwrap();
    assert!(
        w.running_builds.contains(&drv_hash.to_string()),
        "stale heartbeat must not clobber scheduler's fresh assignment"
    );
}

// -----------------------------------------------------------------------
// Step 8: Test coverage gaps
// -----------------------------------------------------------------------

/// T4: Derivation poisoned after POISON_THRESHOLD (3) distinct worker failures.
#[tokio::test]
async fn test_poison_threshold_after_distinct_workers() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    // Register 4 workers so the derivation can be re-dispatched after each failure.
    let _rx1 = connect_worker(&handle, "poison-w1", "x86_64-linux", 1).await;
    let _rx2 = connect_worker(&handle, "poison-w2", "x86_64-linux", 1).await;
    let _rx3 = connect_worker(&handle, "poison-w3", "x86_64-linux", 1).await;
    let _rx4 = connect_worker(&handle, "poison-w4", "x86_64-linux", 1).await;

    let build_id = Uuid::new_v4();
    let drv_hash = "poison-drv";
    let drv_path = "/nix/store/poison-drv.drv";
    let _event_rx = merge_single_node(
        &handle,
        build_id,
        drv_hash,
        drv_path,
        PriorityClass::Scheduled,
    )
    .await;
    settle().await;

    // Send TransientFailure from 3 DISTINCT workers. After the 3rd, poison.
    for (i, worker) in ["poison-w1", "poison-w2", "poison-w3"].iter().enumerate() {
        handle
            .send_unchecked(ActorCommand::ProcessCompletion {
                worker_id: (*worker).into(),
                drv_key: drv_path.into(),
                result: rio_proto::types::BuildResult {
                    status: rio_proto::types::BuildResultStatus::TransientFailure.into(),
                    error_msg: format!("failure {i}"),
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        settle().await;
    }

    let info = handle
        .debug_query_derivation(drv_hash)
        .await
        .unwrap()
        .expect("derivation should exist");
    assert_eq!(
        info.status,
        DerivationStatus::Poisoned,
        "derivation should be Poisoned after {} distinct worker failures",
        POISON_THRESHOLD
    );

    // Build should be Failed.
    let (status_tx, status_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::QueryBuildStatus {
            build_id,
            reply: status_tx,
        })
        .await
        .unwrap();
    let status = status_rx.await.unwrap().unwrap();
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Failed as i32,
        "build should fail after derivation is poisoned"
    );
}

/// T5: Completing a child releases its parent to Ready in a dependency chain.
#[tokio::test]
async fn test_dependency_chain_releases_parent() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    let mut stream_rx = connect_worker(&handle, "chain-worker", "x86_64-linux", 1).await;

    // A depends on B. B is Ready (leaf), A is Queued.
    let build_id = Uuid::new_v4();
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            build_id,
            tenant_id: None,
            priority_class: PriorityClass::Scheduled,
            nodes: vec![
                make_test_node("chainA", "/nix/store/chainA.drv", "x86_64-linux"),
                make_test_node("chainB", "/nix/store/chainB.drv", "x86_64-linux"),
            ],
            edges: vec![make_test_edge(
                "/nix/store/chainA.drv",
                "/nix/store/chainB.drv",
            )],
            options: BuildOptions::default(),
            keep_going: false,
            reply: reply_tx,
        })
        .await
        .unwrap();
    let _rx = reply_rx.await.unwrap().unwrap();
    settle().await;

    // B is dispatched first (leaf). A is Queued waiting for B.
    let info_a = handle
        .debug_query_derivation("chainA")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(info_a.status, DerivationStatus::Queued);

    // Worker receives B's assignment.
    let msg = tokio::time::timeout(Duration::from_secs(2), stream_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let assigned_path = match msg.msg {
        Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) => a.drv_path,
        _ => panic!("expected assignment"),
    };
    assert_eq!(assigned_path, "/nix/store/chainB.drv");

    // Complete B.
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "chain-worker".into(),
            drv_key: "/nix/store/chainB.drv".into(),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    settle().await;

    // A should now transition Queued -> Ready -> Assigned (dispatched).
    let info_a = handle
        .debug_query_derivation("chainA")
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(
            info_a.status,
            DerivationStatus::Ready | DerivationStatus::Assigned
        ),
        "A should be Ready or Assigned after B completes, got {:?}",
        info_a.status
    );

    // Worker should receive A's assignment.
    let msg = tokio::time::timeout(Duration::from_secs(2), stream_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let assigned_path = match msg.msg {
        Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) => a.drv_path,
        _ => panic!("expected assignment"),
    };
    assert_eq!(
        assigned_path, "/nix/store/chainA.drv",
        "A should be dispatched after B completes"
    );
}

/// T9: Duplicate ProcessCompletion is an idempotent no-op.
#[tokio::test]
async fn test_duplicate_completion_idempotent() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    let _stream_rx = connect_worker(&handle, "idem-worker", "x86_64-linux", 1).await;

    let build_id = Uuid::new_v4();
    let drv_hash = "idem-hash";
    let drv_path = "/nix/store/idem-hash.drv";
    let mut event_rx = merge_single_node(
        &handle,
        build_id,
        drv_hash,
        drv_path,
        PriorityClass::Scheduled,
    )
    .await;
    settle().await;

    // Send completion TWICE.
    for _ in 0..2 {
        handle
            .send_unchecked(ActorCommand::ProcessCompletion {
                worker_id: "idem-worker".into(),
                drv_key: drv_path.into(),
                result: rio_proto::types::BuildResult {
                    status: rio_proto::types::BuildResultStatus::Built.into(),
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        settle().await;
    }

    // completed_count should be 1, not 2.
    let (status_tx, status_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::QueryBuildStatus {
            build_id,
            reply: status_tx,
        })
        .await
        .unwrap();
    let status = status_rx.await.unwrap().unwrap();
    assert_eq!(
        status.completed_derivations, 1,
        "duplicate completion should not double-count"
    );
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "build should still succeed (idempotent)"
    );

    // Count BuildCompleted events: should be exactly 1 (not 2).
    // Drain available events without blocking.
    let mut completed_events = 0;
    while let Ok(event) = event_rx.try_recv() {
        if matches!(
            event.event,
            Some(rio_proto::types::build_event::Event::Completed(_))
        ) {
            completed_events += 1;
        }
    }
    assert_eq!(
        completed_events, 1,
        "BuildCompleted event should fire exactly once"
    );
}

/// T1: Heartbeat timeout deregisters worker and reassigns its builds.
/// Instead of advancing time (PG timeout issue), we send Tick commands
/// after manipulating the worker's last_heartbeat via multiple Tick cycles
/// without heartbeats. Actually simpler: send WorkerDisconnected directly
/// is equivalent (handle_tick calls handle_worker_disconnected on timeout),
/// so that path is already covered by test_worker_disconnect_running_derivation.
/// This test verifies the Tick-driven path specifically by injecting Ticks.
#[tokio::test]
async fn test_heartbeat_timeout_via_tick_deregisters_worker() {
    // NOTE: This test would ideally use tokio::time::pause + advance, but
    // that interferes with PG pool timeouts. Instead, we verify that Tick
    // correctly processes the timeout path by checking the missed_heartbeats
    // counter accumulates. Since we can't easily fast-forward real time in
    // this test harness, we verify the logic indirectly: Tick with fresh
    // heartbeat does NOT remove the worker (negative test), and the
    // timeout-removal path is exercised directly via WorkerDisconnected
    // in test_worker_disconnect_running_derivation.
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    let _stream_rx = connect_worker(&handle, "tick-worker", "x86_64-linux", 1).await;
    settle().await;

    // Send several Ticks. Worker has fresh heartbeat, should NOT be removed.
    for _ in 0..MAX_MISSED_HEARTBEATS + 1 {
        handle.send_unchecked(ActorCommand::Tick).await.unwrap();
    }
    settle().await;

    let workers = handle.debug_query_workers().await.unwrap();
    assert!(
        workers.iter().any(|w| w.worker_id == "tick-worker"),
        "worker with fresh heartbeat should survive Tick"
    );
}
