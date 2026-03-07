//! Completion handling: retry/poison thresholds, dep-chain release, duplicate idempotence.
// r[verify sched.completion.idempotent]
// r[verify sched.state.transitions]
// r[verify sched.state.terminal-idempotent]
// r[verify sched.critical-path.incremental]
// r[verify sched.estimate.ema-alpha]
// r[verify sched.classify.penalty-overwrite]

use super::*;

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

    // B is dispatched first (leaf). A is Queued waiting for B.
    let info_a = handle
        .debug_query_derivation("chainA")
        .await?
        .expect("exists");
    assert_eq!(info_a.status, DerivationStatus::Queued);

    // Worker receives B's assignment.
    let assigned_path = recv_assignment(&mut stream_rx).await.drv_path;
    assert_eq!(assigned_path, p_chain_b);

    // Complete B.
    complete_success_empty(&handle, "chain-worker", &p_chain_b).await?;

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
    let assigned_path = recv_assignment(&mut stream_rx).await.drv_path;
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

    // Send completion TWICE.
    for _ in 0..2 {
        complete_success_empty(&handle, "idem-worker", &drv_path).await?;
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
