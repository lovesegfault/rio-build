//! DB fault-injection: pool.close() to exercise error-branch logging in completion paths.
// r[verify sched.actor.single-owner]
// r[verify sched.state.machine]

use super::*;

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
            peak_memory_bytes: 0,
            output_size_bytes: 0,
            peak_cpu_cores: 0.0,
        })
        .await?;

    // In-memory state should have transitioned despite all DB write failures.
    let post = handle
        .debug_query_derivation("fault-hash")
        .await?
        .expect("exists");
    assert_eq!(post.status, DerivationStatus::Completed);

    // The three DB-error branches should all have logged.
    assert!(
        logs_contain("failed to persist derivation status"),
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

    db.pool.close().await;

    complete_failure(
        &handle,
        "tfault-worker",
        &test_drv_path("tfault-hash"),
        rio_proto::types::BuildResultStatus::TransientFailure,
        "flaky network",
    )
    .await?;
    // logs_contain() checks captured tracing output, not actor state —
    // needs an explicit barrier since no request-reply follows.
    barrier(&handle).await;

    // Transient with retry_count < max → should hit the retry-persist branches.
    assert!(
        logs_contain("failed to persist derivation status")
            || logs_contain("failed to persist retry"),
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

    db.pool.close().await;

    // Complete B → A becomes newly-ready.
    complete_success(
        &handle,
        "nrfault-worker",
        &test_drv_path("nrB"),
        &test_store_path("out-B"),
    )
    .await?;

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
        logs_contain("failed to persist derivation status"),
        "newly-ready DB write failure should be logged"
    );
    Ok(())
}

/// Estimator refresh with PG closed → keeps OLD snapshot, logs the
/// failure, actor stays alive. The refresh runs every 6th Tick
/// (ESTIMATOR_REFRESH_EVERY). Stale estimates are better than no
/// estimates — critical-path priorities degrade gracefully, the next
/// successful refresh catches up.
#[tokio::test]
#[tracing_test::traced_test]
async fn test_estimator_refresh_pg_failure_keeps_old_snapshot() -> TestResult {
    let (db, handle, _task) = setup().await;

    // Close pool BEFORE any Ticks. All estimator refreshes will fail.
    db.pool.close().await;

    // 6 Ticks: tick_count increments each call; the 6th (a multiple
    // of ESTIMATOR_REFRESH_EVERY=6) triggers the refresh. Send 7 to
    // be robust against off-by-one in the modulo check.
    for _ in 0..7 {
        handle.send_unchecked(ActorCommand::Tick).await?;
    }
    barrier(&handle).await;

    // The refresh-failure branch should have logged.
    assert!(
        logs_contain("estimator refresh failed"),
        "estimator PG failure should be logged"
    );

    // Actor should still be alive (didn't panic or exit on the PG error).
    assert!(
        handle.is_alive(),
        "actor should survive estimator refresh failure"
    );

    Ok(())
}
