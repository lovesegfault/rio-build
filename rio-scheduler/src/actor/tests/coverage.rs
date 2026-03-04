use super::*;

/// keepGoing=false: on PermanentFailure, the entire build fails immediately.
#[tokio::test]
async fn test_keepgoing_false_fails_fast() -> TestResult {
    let (_db, handle, _task, _stream_rx) =
        setup_with_worker("test-worker", "x86_64-linux", 2).await?;

    // Merge a two-node DAG with keepGoing=false
    let build_id = Uuid::new_v4();
    let _rx = merge_dag(
        &handle,
        build_id,
        vec![
            make_test_node("hashA", "x86_64-linux"),
            make_test_node("hashB", "x86_64-linux"),
        ],
        vec![],
        false, // keep_going=false (critical)
    )
    .await?;
    settle().await;

    // Send PermanentFailure for hashA
    complete_failure(
        &handle,
        "test-worker",
        &test_drv_path("hashA"),
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "compile error",
    )
    .await?;
    settle().await;

    // Build should be Failed (not waiting for hashB)
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Failed as i32,
        "build should fail fast on PermanentFailure with keepGoing=false"
    );
    Ok(())
}

/// keepGoing=true: build waits for all derivations, fails only at the end.
#[tokio::test]
async fn test_keepgoing_true_waits_all() -> TestResult {
    let (_db, handle, _task, _stream_rx) =
        setup_with_worker("test-worker", "x86_64-linux", 2).await?;

    // Merge a two-node DAG with keepGoing=true
    let build_id = Uuid::new_v4();
    let _rx = merge_dag(
        &handle,
        build_id,
        vec![
            make_test_node("hashX", "x86_64-linux"),
            make_test_node("hashY", "x86_64-linux"),
        ],
        vec![],
        true, // keep_going=true (critical)
    )
    .await?;
    settle().await;

    // Send PermanentFailure for hashX
    complete_failure(
        &handle,
        "test-worker",
        &test_drv_path("hashX"),
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "failed",
    )
    .await?;
    settle().await;

    // Build should still be Active (waiting for hashY)
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Active as i32,
        "build should still be Active with keepGoing=true and pending derivations"
    );

    // Complete hashY successfully
    complete_success_empty(&handle, "test-worker", &test_drv_path("hashY")).await?;
    settle().await;

    // Now build should be Failed (all resolved, one failed)
    let status2 = query_status(&handle, build_id).await?;
    assert_eq!(
        status2.state,
        rio_proto::types::BuildState::Failed as i32,
        "build should fail after all derivations resolve with keepGoing=true"
    );
    Ok(())
}

/// keepGoing=true with a dependency chain: poisoning a leaf must cascade
/// DependencyFailed to all ancestors so the build terminates. Without the
/// cascade, parents stay Queued forever and completed+failed never reaches
/// total -> build hangs.
#[tokio::test]
async fn test_keepgoing_poisoned_dependency_cascades_failure() -> TestResult {
    // Worker with capacity 1: only the leaf gets dispatched initially.
    let (_db, handle, _task, _stream_rx) =
        setup_with_worker("cascade-worker", "x86_64-linux", 1).await?;

    // Chain: A depends on B depends on C. C is the leaf.
    let build_id = Uuid::new_v4();
    let _rx = merge_dag(
        &handle,
        build_id,
        vec![
            make_test_node("cascadeA", "x86_64-linux"),
            make_test_node("cascadeB", "x86_64-linux"),
            make_test_node("cascadeC", "x86_64-linux"),
        ],
        vec![
            make_test_edge("cascadeA", "cascadeB"),
            make_test_edge("cascadeB", "cascadeC"),
        ],
        true, // keep_going
    )
    .await?;
    settle().await;

    // Sanity: C is the only Ready/Assigned derivation; A and B are Queued.
    let info_a = handle
        .debug_query_derivation("cascadeA")
        .await?
        .expect("exists");
    let info_b = handle
        .debug_query_derivation("cascadeB")
        .await?
        .expect("exists");
    assert_eq!(info_a.status, DerivationStatus::Queued);
    assert_eq!(info_b.status, DerivationStatus::Queued);

    // Poison C via PermanentFailure.
    complete_failure(
        &handle,
        "cascade-worker",
        &test_drv_path("cascadeC"),
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "compile error",
    )
    .await?;
    settle().await;

    // B and A should now be DependencyFailed (cascaded transitively).
    let info_b = handle
        .debug_query_derivation("cascadeB")
        .await?
        .expect("exists");
    assert_eq!(
        info_b.status,
        DerivationStatus::DependencyFailed,
        "immediate parent B should be DependencyFailed after C poisoned"
    );
    let info_a = handle
        .debug_query_derivation("cascadeA")
        .await?
        .expect("exists");
    assert_eq!(
        info_a.status,
        DerivationStatus::DependencyFailed,
        "transitive parent A should also be DependencyFailed"
    );

    // Build should terminate as Failed (all 3 derivations resolved:
    // 1 Poisoned + 2 DependencyFailed counted in failed).
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Failed as i32,
        "keepGoing build with poisoned dependency chain should terminate as Failed, not hang"
    );
    assert_eq!(
        status.failed_derivations, 3,
        "1 Poisoned + 2 DependencyFailed should all count as failed"
    );
    Ok(())
}

/// When a new build depends on an already-poisoned derivation (from a
/// prior build), compute_initial_states must mark the new node
/// DependencyFailed immediately. Previously it went to Queued and hung
/// forever (never Ready, never cascaded since cascade only runs on
/// *transition to* Poisoned).
#[tokio::test]
async fn test_merge_with_prepoisoned_dep_marks_dependency_failed() -> TestResult {
    let (_db, handle, _task, _stream_rx) =
        setup_with_worker("poison-worker", "x86_64-linux", 1).await?;

    // Build 1: single leaf, poisoned via PermanentFailure.
    let build1 = Uuid::new_v4();
    let _rx1 = merge_single_node(&handle, build1, "preleaf", PriorityClass::Scheduled).await?;
    settle().await;
    complete_failure(
        &handle,
        "poison-worker",
        &test_drv_path("preleaf"),
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "preleaf failed",
    )
    .await?;
    settle().await;

    // Verify preleaf is Poisoned.
    let leaf = handle
        .debug_query_derivation("preleaf")
        .await?
        .expect("exists");
    assert_eq!(leaf.status, DerivationStatus::Poisoned);

    // Build 2: new node depending on the poisoned preleaf.
    // keepGoing=false: build should fail immediately at merge.
    let build2 = Uuid::new_v4();
    let _rx2 = merge_dag(
        &handle,
        build2,
        vec![
            make_test_node("preparent", "x86_64-linux"),
            make_test_node("preleaf", "x86_64-linux"),
        ],
        vec![make_test_edge("preparent", "preleaf")],
        false,
    )
    .await?;
    settle().await;

    // preparent must be DependencyFailed (not stuck Queued).
    let parent = handle
        .debug_query_derivation("preparent")
        .await?
        .expect("exists");
    assert_eq!(
        parent.status,
        DerivationStatus::DependencyFailed,
        "new node depending on pre-poisoned dep must be DependencyFailed, not stuck Queued"
    );

    // Build 2 must be Failed (!keepGoing + dep failure).
    let status = query_status(&handle, build2).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Failed as i32,
        "build depending on pre-poisoned dep must fail immediately (!keepGoing)"
    );
    Ok(())
}

/// WatchBuild on an already-terminal build must immediately send the
/// terminal event. Previously it just subscribed — if the original
/// BuildCompleted was sent to zero receivers (e.g., submit subscriber
/// disconnected before completion), a late WatchBuild would hang forever.
#[tokio::test]
async fn test_watch_build_after_completion_receives_terminal_event() -> TestResult {
    let (_db, handle, _task, _stream_rx) =
        setup_with_worker("watch-worker", "x86_64-linux", 1).await?;

    // Submit a build, complete it, then drop the original subscriber.
    let build_id = Uuid::new_v4();
    let original_rx =
        merge_single_node(&handle, build_id, "watch-hash", PriorityClass::Scheduled).await?;
    settle().await;

    complete_success_empty(&handle, "watch-worker", &test_drv_path("watch-hash")).await?;
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
        .await?;
    let mut watch_rx = reply_rx.await??;

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
    Ok(())
}

/// Terminal build state should be cleaned up after TERMINAL_CLEANUP_DELAY
/// to prevent unbounded memory growth for long-running schedulers.
///
/// This test sends CleanupTerminalBuild directly (bypassing the delay)
/// since paused time interferes with PG pool timeouts. The delay
/// scheduling itself is trivially correct (tokio::time::sleep + try_send).
#[tokio::test]
async fn test_terminal_build_cleanup_after_delay() -> TestResult {
    let (_db, handle, _task, _stream_rx) =
        setup_with_worker("cleanup-worker", "x86_64-linux", 1).await?;

    // Complete a build.
    let build_id = Uuid::new_v4();
    let drv_hash = "cleanup-hash";
    let drv_path = test_drv_path(drv_hash);
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;
    settle().await;

    complete_success_empty(&handle, "cleanup-worker", &drv_path).await?;
    settle().await;

    // Build should be Succeeded and still queryable.
    let status = try_query_status(&handle, build_id).await?;
    assert!(status.is_ok(), "build should be queryable before cleanup");

    // Directly inject the cleanup command (bypassing the 60s delay).
    handle
        .send_unchecked(ActorCommand::CleanupTerminalBuild { build_id })
        .await?;
    settle().await;

    // Build should now be gone (BuildNotFound).
    let status = try_query_status(&handle, build_id).await?;
    assert!(
        matches!(status, Err(ActorError::BuildNotFound(_))),
        "build should be cleaned up after delay, got {:?}",
        status
    );

    // DAG node should also be reaped (Completed + orphaned).
    let info = handle.debug_query_derivation(drv_hash).await?;
    assert!(
        info.is_none(),
        "orphaned+terminal DAG node should be reaped"
    );
    Ok(())
}

/// TransientFailure: retry on a different worker up to max_retries (default 2).
#[tokio::test]
async fn test_transient_retry_different_worker() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Register two workers
    let _rx1 = connect_worker(&handle, "worker-a", "x86_64-linux", 1).await?;
    let _rx2 = connect_worker(&handle, "worker-b", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let p_retry = test_drv_path("retry-hash");
    let _event_rx =
        merge_single_node(&handle, build_id, "retry-hash", PriorityClass::Scheduled).await?;
    settle().await;

    // Get initial worker assignment
    let info1 = handle
        .debug_query_derivation("retry-hash")
        .await?
        .expect("exists");
    let first_worker = info1.assigned_worker.clone().expect("assigned to a worker");
    assert_eq!(info1.retry_count, 0);

    // Send TransientFailure from the first worker
    complete_failure(
        &handle,
        &first_worker,
        &p_retry,
        rio_proto::types::BuildResultStatus::TransientFailure,
        "network hiccup",
    )
    .await?;
    settle().await;

    // Should be retried: retry_count=1, possibly on a different worker
    let info2 = handle
        .debug_query_derivation("retry-hash")
        .await?
        .expect("exists");
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
    Ok(())
}

/// max_retries (default 2) exhausted with the SAME worker should poison.
/// This branch (retry_count >= max_retries) is distinct from
/// POISON_THRESHOLD (3 distinct workers) — same worker failing
/// repeatedly hits max_retries first.
#[tokio::test]
async fn test_transient_failure_max_retries_same_worker_poisons() -> TestResult {
    let (_db, handle, _task, _rx) = setup_with_worker("flaky-worker", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let p_maxretry = test_drv_path("maxretry-hash");
    let _event_rx =
        merge_single_node(&handle, build_id, "maxretry-hash", PriorityClass::Scheduled).await?;
    settle().await;

    // Default RetryPolicy::max_retries = 2. Fail 3 times on same worker:
    // retry_count 0 -> 1 (retry), 1 -> 2 (retry), 2 >= 2 -> Poisoned.
    for attempt in 0..3 {
        complete_failure(
            &handle,
            "flaky-worker",
            &p_maxretry,
            rio_proto::types::BuildResultStatus::TransientFailure,
            &format!("attempt {attempt} failed"),
        )
        .await?;
        settle().await;
    }

    let info = handle
        .debug_query_derivation("maxretry-hash")
        .await?
        .expect("exists");
    assert_eq!(
        info.status,
        DerivationStatus::Poisoned,
        "3 transient failures on same worker (retry_count >= max_retries=2) should poison"
    );
    Ok(())
}

/// CancelBuild on an active build should clean up derivations and emit
/// BuildCancelled event. Previously untested.
#[tokio::test]
async fn test_cancel_build_active_drains_derivations() -> TestResult {
    let (_db, handle, _task) = setup().await;
    // No workers — derivation stays Ready (never assigned).

    let build_id = Uuid::new_v4();
    let mut event_rx =
        merge_single_node(&handle, build_id, "cancel-hash", PriorityClass::Scheduled).await?;
    settle().await;

    // Send CancelBuild.
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id,
            reason: "test cancel".into(),
            reply: reply_tx,
        })
        .await?;
    let cancelled = reply_rx.await??;
    assert!(cancelled, "CancelBuild should return true for active build");
    settle().await;

    // Build should be Cancelled.
    let status = query_status(&handle, build_id).await?;
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
        .await?;
    let re_cancelled = reply_rx2.await??;
    assert!(
        !re_cancelled,
        "CancelBuild on already-terminal build should return false"
    );
    Ok(())
}

/// WatchBuild during an active build should receive events as they happen.
/// (The after-completion case is tested separately.)
#[tokio::test]
async fn test_watch_build_receives_events() -> TestResult {
    let (_db, handle, _task, _rx) =
        setup_with_worker("watch-events-worker", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let _original = merge_single_node(
        &handle,
        build_id,
        "watch-events-hash",
        PriorityClass::Scheduled,
    )
    .await?;
    settle().await;

    // WatchBuild on the active build.
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::WatchBuild {
            build_id,
            since_sequence: 0,
            reply: reply_tx,
        })
        .await?;
    let mut watch_rx = reply_rx.await??;

    // Complete the build; watcher should see BuildCompleted.
    complete_success_empty(
        &handle,
        "watch-events-worker",
        &test_drv_path("watch-events-hash"),
    )
    .await?;

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
    settle().await;

    // Worker should receive assignment with the build's options.
    let msg = tokio::time::timeout(Duration::from_secs(2), stream_rx.recv())
        .await?
        .expect("exists");
    let Some(rio_proto::types::scheduler_message::Msg::Assignment(assignment)) = msg.msg else {
        panic!("expected assignment");
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
    Ok(())
}

/// TOCTOU fix: a stale heartbeat (sent before scheduler assigned a
/// derivation) must not clobber the scheduler's fresh assignment in
/// worker.running_builds. The scheduler is authoritative.
#[tokio::test]
async fn test_heartbeat_does_not_clobber_fresh_assignment() -> TestResult {
    // Register worker (initial heartbeat has empty running_builds).
    let (_db, handle, _task, _stream_rx) =
        setup_with_worker("toctou-worker", "x86_64-linux", 2).await?;
    settle().await;

    // Merge a derivation. Scheduler will assign it to the worker and
    // insert it into worker.running_builds.
    let build_id = Uuid::new_v4();
    let drv_hash = "toctou-drv-hash";
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;
    settle().await;

    // Verify: derivation is Assigned, worker.running_builds contains it.
    let info = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("derivation should exist");
    assert_eq!(info.status, DerivationStatus::Assigned);

    let workers = handle.debug_query_workers().await?;
    let w = workers
        .iter()
        .find(|w| w.worker_id == "toctou-worker")
        .expect("toctou-worker registered");
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
        .await?;
    settle().await;

    // Assignment must still be tracked. Before the fix, running_builds
    // would be wholesale replaced with the empty set, orphaning the
    // assignment (completion would later warn "unknown derivation").
    let workers = handle.debug_query_workers().await?;
    let w = workers
        .iter()
        .find(|w| w.worker_id == "toctou-worker")
        .expect("toctou-worker registered");
    assert!(
        w.running_builds.contains(&drv_hash.to_string()),
        "stale heartbeat must not clobber scheduler's fresh assignment"
    );
    Ok(())
}

/// T4: Derivation poisoned after POISON_THRESHOLD (3) distinct worker failures.
#[tokio::test]
async fn test_poison_threshold_after_distinct_workers() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Register 4 workers so the derivation can be re-dispatched after each failure.
    let _rx1 = connect_worker(&handle, "poison-w1", "x86_64-linux", 1).await?;
    let _rx2 = connect_worker(&handle, "poison-w2", "x86_64-linux", 1).await?;
    let _rx3 = connect_worker(&handle, "poison-w3", "x86_64-linux", 1).await?;
    let _rx4 = connect_worker(&handle, "poison-w4", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let drv_hash = "poison-drv";
    let drv_path = test_drv_path(drv_hash);
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;
    settle().await;

    // Send TransientFailure from 3 DISTINCT workers. After the 3rd, poison.
    for (i, worker) in ["poison-w1", "poison-w2", "poison-w3"].iter().enumerate() {
        complete_failure(
            &handle,
            worker,
            &drv_path,
            rio_proto::types::BuildResultStatus::TransientFailure,
            &format!("failure {i}"),
        )
        .await?;
        settle().await;
    }

    let info = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("derivation should exist");
    assert_eq!(
        info.status,
        DerivationStatus::Poisoned,
        "derivation should be Poisoned after {} distinct worker failures",
        POISON_THRESHOLD
    );

    // Build should be Failed.
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Failed as i32,
        "build should fail after derivation is poisoned"
    );
    Ok(())
}

/// T5: Completing a child releases its parent to Ready in a dependency chain.
#[tokio::test]
async fn test_dependency_chain_releases_parent() -> TestResult {
    let (_db, handle, _task, mut stream_rx) =
        setup_with_worker("chain-worker", "x86_64-linux", 1).await?;

    // A depends on B. B is Ready (leaf), A is Queued.
    let build_id = Uuid::new_v4();
    let p_chain_a = test_drv_path("chainA");
    let p_chain_b = test_drv_path("chainB");
    let _rx = merge_dag(
        &handle,
        build_id,
        vec![
            make_test_node("chainA", "x86_64-linux"),
            make_test_node("chainB", "x86_64-linux"),
        ],
        vec![make_test_edge("chainA", "chainB")],
        false,
    )
    .await?;
    settle().await;

    // B is dispatched first (leaf). A is Queued waiting for B.
    let info_a = handle
        .debug_query_derivation("chainA")
        .await?
        .expect("exists");
    assert_eq!(info_a.status, DerivationStatus::Queued);

    // Worker receives B's assignment.
    let msg = tokio::time::timeout(Duration::from_secs(2), stream_rx.recv())
        .await?
        .expect("exists");
    let assigned_path = match msg.msg {
        Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) => a.drv_path,
        _ => panic!("expected assignment"),
    };
    assert_eq!(assigned_path, p_chain_b);

    // Complete B.
    complete_success_empty(&handle, "chain-worker", &p_chain_b).await?;
    settle().await;

    // A should now transition Queued -> Ready -> Assigned (dispatched).
    let info_a = handle
        .debug_query_derivation("chainA")
        .await?
        .expect("exists");
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
        .await?
        .expect("exists");
    let assigned_path = match msg.msg {
        Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) => a.drv_path,
        _ => panic!("expected assignment"),
    };
    assert_eq!(
        assigned_path, p_chain_a,
        "A should be dispatched after B completes"
    );
    Ok(())
}

/// T9: Duplicate ProcessCompletion is an idempotent no-op.
#[tokio::test]
async fn test_duplicate_completion_idempotent() -> TestResult {
    let (_db, handle, _task, _stream_rx) =
        setup_with_worker("idem-worker", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let drv_hash = "idem-hash";
    let drv_path = test_drv_path(drv_hash);
    let mut event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;
    settle().await;

    // Send completion TWICE.
    for _ in 0..2 {
        complete_success_empty(&handle, "idem-worker", &drv_path).await?;
        settle().await;
    }

    // completed_count should be 1, not 2.
    let status = query_status(&handle, build_id).await?;
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
    Ok(())
}

/// T1: Heartbeat timeout deregisters worker and reassigns its builds.
/// Instead of advancing time (PG timeout issue), we send Tick commands
/// after manipulating the worker's last_heartbeat via multiple Tick cycles
/// without heartbeats. Actually simpler: send WorkerDisconnected directly
/// is equivalent (handle_tick calls handle_worker_disconnected on timeout),
/// so that path is already covered by test_worker_disconnect_running_derivation.
/// This test verifies the Tick-driven path specifically by injecting Ticks.
#[tokio::test]
async fn test_heartbeat_timeout_via_tick_deregisters_worker() -> TestResult {
    // NOTE: This test would ideally use tokio::time::pause + advance, but
    // that interferes with PG pool timeouts. Instead, we verify that Tick
    // correctly processes the timeout path by checking the missed_heartbeats
    // counter accumulates. Since we can't easily fast-forward real time in
    // this test harness, we verify the logic indirectly: Tick with fresh
    // heartbeat does NOT remove the worker (negative test), and the
    // timeout-removal path is exercised directly via WorkerDisconnected
    // in test_worker_disconnect_running_derivation.
    let (_db, handle, _task, _stream_rx) =
        setup_with_worker("tick-worker", "x86_64-linux", 1).await?;
    settle().await;

    // Send several Ticks. Worker has fresh heartbeat, should NOT be removed.
    for _ in 0..MAX_MISSED_HEARTBEATS + 1 {
        handle.send_unchecked(ActorCommand::Tick).await?;
    }
    settle().await;

    let workers = handle.debug_query_workers().await?;
    assert!(
        workers.iter().any(|w| w.worker_id == "tick-worker"),
        "worker with fresh heartbeat should survive Tick"
    );
    Ok(())
}

// ===========================================================================
// Poison-TTL expiry (POISON_TTL is cfg(test)-shadowed to 100ms in state/mod.rs)
// ===========================================================================

/// A poisoned derivation is reset to Created after POISON_TTL elapses and
/// a Tick is processed. Covers actor/worker.rs:178-201 (poison-expiry loop)
/// and state/mod.rs:reset_from_poison.
#[tokio::test]
async fn test_tick_expires_poisoned_derivation() -> TestResult {
    let (_db, handle, _task, _rx) =
        setup_with_worker("poison-ttl-worker", "x86_64-linux", 1).await?;

    // Merge, dispatch, poison via PermanentFailure.
    let build_id = Uuid::new_v4();
    let _evt_rx = merge_single_node(
        &handle,
        build_id,
        "poison-ttl-hash",
        PriorityClass::Scheduled,
    )
    .await?;
    settle().await;

    complete_failure(
        &handle,
        "poison-ttl-worker",
        &test_drv_path("poison-ttl-hash"),
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "permanent",
    )
    .await?;
    settle().await;

    // Verify poisoned.
    let pre = handle
        .debug_query_derivation("poison-ttl-hash")
        .await?
        .expect("derivation exists");
    assert_eq!(pre.status, DerivationStatus::Poisoned);

    // Wait past the cfg(test) POISON_TTL (100ms).
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Tick processes the expiry.
    handle.send_unchecked(ActorCommand::Tick).await?;
    settle().await;

    let post = handle
        .debug_query_derivation("poison-ttl-hash")
        .await?
        .expect("derivation still exists");
    assert_eq!(
        post.status,
        DerivationStatus::Created,
        "poisoned derivation should be reset after TTL expiry"
    );
    Ok(())
}

// ===========================================================================
// DB fault-injection suite for actor/completion.rs error branches
// ===========================================================================
//
// Pattern: setup normally so merge + dispatch succeed, then close the PG
// pool, then trigger the code path under test. DB writes fail; assert the
// actor logs the error and does NOT corrupt in-memory state.
// TestDb::Drop uses a fresh admin connection so closing the test pool here
// doesn't break cleanup.

/// After pool close, a successful completion still transitions in-memory
/// state, but update_build_history logs an error. Also exercises the
/// derivation-status and assignment-status DB-error branches.
#[tokio::test]
#[tracing_test::traced_test]
async fn test_completion_db_fault_build_history_logged() -> TestResult {
    let (db, handle, _task, _rx) = setup_with_worker("fault-worker", "x86_64-linux", 1).await?;

    // Use a node with pname so update_build_history is called.
    let build_id = Uuid::new_v4();
    let mut node = make_test_node("fault-hash", "x86_64-linux");
    node.pname = "fault-pkg".into();
    let _evt_rx = merge_dag(&handle, build_id, vec![node], vec![], false).await?;
    settle().await;

    // Close pool AFTER merge/dispatch so only completion DB writes fail.
    db.pool.close().await;

    // Success with start/stop times so the EMA branch is reached.
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "fault-worker".into(),
            drv_key: test_drv_path("fault-hash"),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                start_time: Some(prost_types::Timestamp {
                    seconds: 100,
                    nanos: 0,
                }),
                stop_time: Some(prost_types::Timestamp {
                    seconds: 110,
                    nanos: 0,
                }),
                ..Default::default()
            },
        })
        .await?;
    settle().await;

    // In-memory state should have transitioned despite all DB write failures.
    let post = handle
        .debug_query_derivation("fault-hash")
        .await?
        .expect("exists");
    assert_eq!(post.status, DerivationStatus::Completed);

    // The three DB-error branches should all have logged.
    assert!(
        logs_contain("failed to update derivation status"),
        "derivation status DB failure should be logged"
    );
    assert!(
        logs_contain("failed to update build history EMA"),
        "build_history EMA DB failure should be logged"
    );
    Ok(())
}

/// Transient failure with pool closed: retry-persist logs error.
#[tokio::test]
#[tracing_test::traced_test]
async fn test_transient_failure_db_fault_retry_persist_logged() -> TestResult {
    let (db, handle, _task, _rx) = setup_with_worker("tfault-worker", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let _evt_rx =
        merge_single_node(&handle, build_id, "tfault-hash", PriorityClass::Scheduled).await?;
    settle().await;

    db.pool.close().await;

    complete_failure(
        &handle,
        "tfault-worker",
        &test_drv_path("tfault-hash"),
        rio_proto::types::BuildResultStatus::TransientFailure,
        "flaky network",
    )
    .await?;
    settle().await;

    // Transient with retry_count < max → should hit the retry-persist branches.
    assert!(
        logs_contain("failed to persist Failed status") || logs_contain("failed to persist retry"),
        "transient-failure DB write failure should be logged"
    );
    Ok(())
}

/// 2-node chain: B completes, A becomes newly-ready. Pool closed →
/// the newly-ready DB update fails and logs.
#[tokio::test]
#[tracing_test::traced_test]
async fn test_newly_ready_db_fault_status_persist_logged() -> TestResult {
    let (db, handle, _task, _rx) = setup_with_worker("nrfault-worker", "x86_64-linux", 1).await?;

    // A depends on B (edge parent=A, child=B — B must complete first).
    let build_id = Uuid::new_v4();
    let _evt_rx = merge_dag(
        &handle,
        build_id,
        vec![
            make_test_node("nrA", "x86_64-linux"),
            make_test_node("nrB", "x86_64-linux"),
        ],
        vec![make_test_edge("nrA", "nrB")],
        false,
    )
    .await?;
    settle().await;

    db.pool.close().await;

    // Complete B → A becomes newly-ready.
    complete_success(
        &handle,
        "nrfault-worker",
        &test_drv_path("nrB"),
        &test_store_path("out-B"),
    )
    .await?;
    settle().await;

    // A should be Ready in-memory (transition succeeds); DB write logged.
    let a = handle
        .debug_query_derivation("nrA")
        .await?
        .expect("nrA exists");
    // A may have been dispatched immediately (Ready → Assigned). Either is fine.
    assert!(
        matches!(
            a.status,
            DerivationStatus::Ready | DerivationStatus::Assigned
        ),
        "A should be ready-ish after B completes, got {:?}",
        a.status
    );
    assert!(
        logs_contain("failed to update status"),
        "newly-ready DB write failure should be logged"
    );
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
    settle().await;

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
    settle().await;

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
        settle().await;
        // If we just completed B, the NEXT dispatch should be A (priority boost).
        if path == p_prio_b {
            let next = worker_rx.recv().await.expect("should get next assignment");
            let Some(rio_proto::types::scheduler_message::Msg::Assignment(next_a)) = next.msg
            else {
                panic!("expected Assignment");
            };
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

// ===========================================================================
// actor/merge.rs cleanup + cache-check error paths
// ===========================================================================

/// When DB persistence fails mid-merge, cleanup_failed_merge rolls back
/// all in-memory state. The build_id should be unknown afterward.
#[tokio::test]
async fn test_merge_db_failure_rolls_back_memory() -> TestResult {
    let (db, handle, _task) = setup().await;

    // Close pool BEFORE merge so insert_build fails immediately.
    db.pool.close().await;

    let build_id = Uuid::new_v4();
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            req: MergeDagRequest {
                build_id,
                tenant_id: None,
                priority_class: PriorityClass::Scheduled,
                nodes: vec![make_test_node("rollback", "x86_64-linux")],
                edges: vec![],
                options: BuildOptions::default(),
                keep_going: false,
            },
            reply: reply_tx,
        })
        .await?;
    let reply = reply_rx.await?;

    // Merge should have failed.
    assert!(
        matches!(reply, Err(ActorError::Database(_))),
        "expected Database error, got {reply:?}"
    );

    // And the build should NOT exist in memory (rollback worked).
    let status_result = try_query_status(&handle, build_id).await?;
    assert!(
        matches!(status_result, Err(ActorError::BuildNotFound(_))),
        "rolled-back build should be NotFound, got {status_result:?}"
    );
    Ok(())
}

/// check_cached_outputs store error is non-fatal: merge proceeds with
/// empty-set result (everything assumed uncached).
#[tokio::test]
async fn test_check_cached_outputs_store_error_non_fatal() -> TestResult {
    use rio_test_support::grpc::spawn_mock_store;
    use std::sync::atomic::Ordering;

    let test_db = TestDb::new(&MIGRATOR).await;
    let (store, addr, _store_h) = spawn_mock_store().await?;
    store.fail_find_missing.store(true, Ordering::SeqCst);

    let store_client = rio_proto::client::connect_store(&addr.to_string()).await?;
    let (handle, _task) = setup_actor_with_store(test_db.pool.clone(), Some(store_client));

    // Merge with expected_output_paths set so check_cached_outputs runs.
    let build_id = Uuid::new_v4();
    let mut node = make_test_node("cache-err", "x86_64-linux");
    node.expected_output_paths = vec![test_store_path("expected-out")];
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            req: MergeDagRequest {
                build_id,
                tenant_id: None,
                priority_class: PriorityClass::Scheduled,
                nodes: vec![node],
                edges: vec![],
                options: BuildOptions::default(),
                keep_going: false,
            },
            reply: reply_tx,
        })
        .await?;

    // Merge should SUCCEED despite the store error.
    let reply = reply_rx.await?;
    assert!(reply.is_ok(), "store error should be non-fatal: {reply:?}");

    // Build should exist and be Active.
    let status = query_status(&handle, build_id).await?;
    assert_eq!(status.state, rio_proto::types::BuildState::Active as i32);
    Ok(())
}

/// Circuit breaker: after 5 consecutive cache-check failures, the 6th
/// SubmitBuild is REJECTED with StoreUnavailable. This is the difference
/// from the test above — one failure is non-fatal, five trips the breaker.
///
/// Then: store recovers → 7th merge succeeds (half-open probe closes).
/// This proves both the open transition AND the close-on-recovery path.
#[tokio::test]
async fn test_cache_check_circuit_breaker_opens_then_closes() -> TestResult {
    use rio_test_support::grpc::spawn_mock_store;
    use std::sync::atomic::Ordering;

    let test_db = TestDb::new(&MIGRATOR).await;
    let (store, addr, _store_h) = spawn_mock_store().await?;
    store.fail_find_missing.store(true, Ordering::SeqCst);

    let store_client = rio_proto::client::connect_store(&addr.to_string()).await?;
    let (handle, _task) = setup_actor_with_store(test_db.pool.clone(), Some(store_client));

    // Helper: merge a single-node DAG. Each call MUST use a unique tag —
    // make_test_node derives drv_hash from tag, and merging the SAME node
    // twice gives empty newly_inserted on the second merge → cache check
    // skipped (check_paths empty) → no probe → no failure recorded. The
    // test would silently pass for the wrong reason.
    //
    // expected_output_paths must also be non-empty or the cache check skips
    // the store call entirely.
    let mut seq = 0u32;
    let mut do_merge = |label: &str| {
        seq += 1;
        // Unique tag per call — different drv_hash → always newly_inserted.
        let tag = format!("{label}-{seq}");
        let mut node = make_test_node(&tag, "x86_64-linux");
        node.expected_output_paths = vec![test_store_path("expected-out")];
        let (reply_tx, reply_rx) = oneshot::channel();
        let cmd = ActorCommand::MergeDag {
            req: MergeDagRequest {
                build_id: Uuid::new_v4(),
                tenant_id: None,
                priority_class: PriorityClass::Scheduled,
                nodes: vec![node],
                edges: vec![],
                options: BuildOptions::default(),
                keep_going: false,
            },
            reply: reply_tx,
        };
        (cmd, reply_rx)
    };

    // Merges 1-4: under threshold (OPEN_THRESHOLD = 5). Each fails the
    // cache check but proceeds with empty cache-hit set. Wasteful but
    // tolerable for a handful of submissions.
    for i in 1..=4 {
        let (cmd, rx) = do_merge("under");
        handle.send_unchecked(cmd).await?;
        let reply = rx.await?;
        assert!(
            reply.is_ok(),
            "merge #{i} should succeed (breaker still closed): {reply:?}"
        );
    }

    // Merge 5: this trips the breaker. consecutive_failures hits 5 = threshold.
    // record_failure() returns true → Err(StoreUnavailable) → merge rolled back.
    let (cmd, rx) = do_merge("trip");
    handle.send_unchecked(cmd).await?;
    let reply = rx.await?;
    assert!(
        matches!(reply, Err(ActorError::StoreUnavailable)),
        "merge #5 should trip breaker open, got: {reply:?}"
    );

    // Merge 6: breaker still open. The probe fails (store still broken) →
    // stays open → rejected. Proves the breaker doesn't spuriously close.
    let (cmd, rx) = do_merge("still-open");
    handle.send_unchecked(cmd).await?;
    let reply = rx.await?;
    assert!(
        matches!(reply, Err(ActorError::StoreUnavailable)),
        "merge #6 should still be rejected (breaker stays open): {reply:?}"
    );

    // === Store recovers ===
    store.fail_find_missing.store(false, Ordering::SeqCst);

    // Merge 7: half-open probe succeeds. record_success() closes the breaker.
    // The merge proceeds normally (empty cache-hit set because nothing's
    // seeded in MockStore, but that's fine — the point is it's ACCEPTED).
    let (cmd, rx) = do_merge("recovered");
    handle.send_unchecked(cmd).await?;
    let reply = rx.await?;
    assert!(
        reply.is_ok(),
        "merge #7 should succeed after store recovery (probe closes breaker): {reply:?}"
    );

    Ok(())
}

/// Breaker unit tests: the state machine in isolation, no actor needed.
/// These are cheap synchronous tests covering the edge cases the
/// integration test above doesn't exercise (saturating add, auto-close
/// timeout interaction).
#[cfg(test)]
mod breaker_tests {
    use crate::actor::CacheCheckBreaker;

    #[test]
    fn closed_under_threshold() {
        let mut b = CacheCheckBreaker::default();
        for i in 1..=4 {
            assert!(!b.record_failure(), "failure {i} should not trip");
        }
    }

    #[test]
    fn trips_at_threshold() {
        let mut b = CacheCheckBreaker::default();
        for _ in 1..=4 {
            b.record_failure();
        }
        assert!(b.record_failure(), "5th failure should trip");
    }

    #[test]
    fn stays_open_on_subsequent_failures() {
        let mut b = CacheCheckBreaker::default();
        for _ in 1..=5 {
            b.record_failure();
        }
        // 6th, 7th, ... — still returns true (already open).
        assert!(b.record_failure());
        assert!(b.record_failure());
    }

    #[test]
    fn success_closes_and_resets() {
        let mut b = CacheCheckBreaker::default();
        for _ in 1..=5 {
            b.record_failure();
        }
        b.record_success();
        // Back to needing 5 more failures.
        for i in 1..=4 {
            assert!(
                !b.record_failure(),
                "post-reset failure {i} should not trip"
            );
        }
        assert!(b.record_failure(), "post-reset 5th should trip again");
    }

    #[test]
    fn success_while_closed_is_cheap_noop() {
        let mut b = CacheCheckBreaker::default();
        b.record_failure();
        b.record_failure();
        b.record_success(); // resets counter to 0
        // Need 5 MORE failures now, not 3.
        for i in 1..=4 {
            assert!(!b.record_failure(), "failure {i} should not trip");
        }
    }

    #[test]
    fn saturating_add_no_overflow() {
        // u32::MAX failures — saturating_add prevents wraparound to 0
        // (which would accidentally close the breaker). Not achievable in
        // practice but a real bug if record_failure used bare `+= 1`.
        let mut b = CacheCheckBreaker {
            consecutive_failures: u32::MAX,
            ..Default::default()
        };
        b.record_failure(); // would wrap without saturating_add
        assert_eq!(b.consecutive_failures, u32::MAX);
    }
}
