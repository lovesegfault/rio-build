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
/// state, but write_build_sample logs an error. Also exercises the
/// derivation-status and assignment-status DB-error branches.
#[tokio::test]
#[tracing_test::traced_test]
async fn test_completion_db_fault_build_sample_logged() -> TestResult {
    let (db, handle, _task, _rx) = setup_with_worker("fault-worker", "x86_64-linux").await?;

    // Use a node with pname so write_build_sample is called.
    let build_id = Uuid::new_v4();
    let mut node = make_node("fault-hash");
    node.pname = "fault-pkg".into();
    let _evt_rx = merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    // Close pool AFTER merge/dispatch so only completion DB writes fail.
    db.pool.close().await;

    // Success with start/stop times so the EMA branch is reached.
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            executor_id: "fault-worker".into(),
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
            peak_cpu_cores: 0.0,
            node_name: None,
            hw_class: None,
            final_line_count: 0,
            final_resources: None,
        })
        .await?;

    // In-memory state should have transitioned despite all DB write failures.
    let post = expect_drv(&handle, "fault-hash").await;
    assert_eq!(post.status, DerivationStatus::Completed);

    // The three DB-error branches should all have logged.
    assert!(
        logs_contain("failed to persist derivation status"),
        "derivation status DB failure should be logged"
    );
    assert!(
        logs_contain("write_build_sample failed"),
        "build_samples DB failure should be logged"
    );
    Ok(())
}

/// Transient failure with pool closed: the appending transaction fails,
/// the derivation stays in its pre-report state (Phase-1b posture), and
/// the failure is logged.
#[tokio::test]
#[tracing_test::traced_test]
async fn test_transient_failure_db_fault_keeps_pre_report_state() -> TestResult {
    let (db, handle, _task, _rx) = setup_with_worker("tfault-worker", "x86_64-linux").await?;
    // Pad worker (statically-eligible — same system) so the fleet-
    // exhaustion clamp doesn't poison after a single failure; we need
    // the retry-persist branch. store_degraded keeps it ineligible for
    // dispatch (rejection_reason → "store-degraded") while still
    // counting toward `statically_eligible` fleet size. NOT
    // `running_build=Some("busy")`: heartbeat reconcile resolves the
    // path against the DAG and "busy" → None → pad becomes idle →
    // dispatch may pick it (HashMap-order-dependent flake).
    let _pad = connect_executor_with(&handle, "tfault-pad", "x86_64-linux", true, |hb| {
        hb.store_degraded = true;
    })
    .await?;

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

    // Phase 1b posture: the appending transaction is the decision
    // point — when it cannot commit, the failure is not applied and the
    // derivation stays in its pre-report state (the legacy mirror write
    // failure is also logged, before the transaction runs).
    assert!(
        logs_contain("appending transaction failed"),
        "the failed appending transaction should be logged"
    );
    assert!(
        logs_contain("failed to persist failed_worker"),
        "the legacy failed_builders mirror failure should be logged"
    );
    let post = expect_drv(&handle, "tfault-hash").await;
    assert_eq!(
        post.status,
        DerivationStatus::Assigned,
        "pre-report state preserved when the appending transaction fails"
    );
    assert_eq!(
        post.retry.count, 0,
        "no retry charged without a committed record"
    );
    Ok(())
}

/// 2-node chain: B completes, A becomes newly-ready. Pool closed →
/// the newly-ready DB update fails and logs.
#[tokio::test]
#[tracing_test::traced_test]
async fn test_newly_ready_db_fault_status_persist_logged() -> TestResult {
    let (db, handle, _task, _rx) = setup_with_worker("nrfault-worker", "x86_64-linux").await?;

    // A depends on B (edge parent=A, child=B — B must complete first).
    let build_id = Uuid::new_v4();
    let _evt_rx = merge_dag(
        &handle,
        build_id,
        vec![make_node("nrA"), make_node("nrB")],
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
    let a = expect_drv(&handle, "nrA").await;
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

/// `transition_build` DB error must NOT commit the in-memory transition.
///
/// Previous order was in-mem mutate → DB write → `?`. A transient PG
/// error left in-mem terminal, DB Active; every caller swallowed with
/// `error!()`. `check_build_completion` then early-returned forever
/// (`is_terminal()` true), `BuildCompleted` was never emitted, and
/// `schedule_terminal_cleanup` never ran. Retry self-defeated: re-calling
/// on already-terminal returns `Rejected`.
///
/// Post-fix (validate → DB → in-mem): DB error leaves in-mem `Active`,
/// so the next completion-driven `check_build_completion` retries cleanly.
#[tokio::test]
#[tracing_test::traced_test]
async fn test_transition_build_db_fault_leaves_state_retryable() -> TestResult {
    let (db, handle, _task, mut rx) = setup_with_worker("tbfault-w", "x86_64-linux").await?;

    let build_id = Uuid::new_v4();
    let _ev = merge_single_node(&handle, build_id, "tbfault-h", PriorityClass::Scheduled).await?;
    let _ = recv_assignment(&mut rx).await;

    // Close pool AFTER dispatch so only transition_build's
    // update_build_status fails (plus the best-effort writes around it).
    db.pool.close().await;

    complete_success_empty(&handle, "tbfault-w", &test_drv_path("tbfault-h")).await?;
    barrier(&handle).await;

    // Load-bearing: in-mem state is STILL Active. Pre-fix this was
    // Succeeded — stuck terminal with no event emitted, no cleanup.
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Active as i32,
        "DB error in transition_build must leave in-mem Active so retry remains possible"
    );
    assert!(
        logs_contain("failed to persist build completion"),
        "transition_build DB failure should be logged by check_build_completion"
    );
    Ok(())
}

/// Retry DRIVER for the case above: after the last derivation
/// completes, no event-driven path calls `check_build_completion`
/// again. `tick_recheck_stuck_completions` (called from `handle_tick`)
/// re-checks Active builds with `completed+failed >= total` so a
/// transient PG blip on the final `update_build_status` recovers on
/// the next Tick instead of hanging `WatchBuild` until the user
/// disconnects (→ orphan-watcher wrongly Cancels) or the scheduler
/// restarts.
#[tokio::test]
#[tracing_test::traced_test]
async fn test_transition_build_db_fault_retried_by_tick() -> TestResult {
    let (db, handle, _task, mut rx) = setup_with_worker("tickretry-w", "x86_64-linux").await?;

    let build_id = Uuid::new_v4();
    let _ev = merge_single_node(&handle, build_id, "tickretry-h", PriorityClass::Scheduled).await?;
    let _ = recv_assignment(&mut rx).await;

    db.pool.close().await;
    complete_success_empty(&handle, "tickretry-w", &test_drv_path("tickretry-h")).await?;
    barrier(&handle).await;

    // Stuck Active (load-bearing precondition; covered by
    // test_transition_build_db_fault_leaves_state_retryable above).
    let status = query_status(&handle, build_id).await?;
    assert_eq!(status.state, rio_proto::types::BuildState::Active as i32);

    // Reopen the DB (fresh pool to the same database) and install it
    // on the actor; then drive one Tick.
    let fresh = db.reopen().await;
    let (tx, ack) = tokio::sync::oneshot::channel();
    handle
        .send_unchecked(ActorCommand::Debug(DebugCmd::SwapDb {
            pool: fresh,
            reply: tx,
        }))
        .await?;
    ack.await?;
    tick(&handle).await?;

    // tick_recheck_stuck_completions → check_build_completion →
    // transition_build (now succeeds) → Succeeded. Pre-fix: no tick
    // path called check_build_completion → stayed Active.
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "Tick must drive the transition_build retry after a transient DB error"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Phase 1b (T-1b.2): the appending transaction is the decision point.
// When it cannot commit, the failure is NOT applied — the derivation
// stays in its pre-report state and the completion event is re-delivered
// with bounded retry.
// ---------------------------------------------------------------------------

/// Posture flip: with PG down, a permanent failure leaves the derivation
/// in its pre-report state (still Assigned, not Poisoned, no
/// poisoned_at) instead of poisoning in memory while silently losing the
/// accounting. The bounded re-delivery warn is logged.
#[tokio::test]
#[tracing_test::traced_test]
async fn phase1b_e3_record_tx_failure_keeps_pre_report_state() -> TestResult {
    let (db, handle, _task, _rx) = setup_with_worker("e3fault-w", "x86_64-linux").await?;
    let _ev =
        merge_single_node(&handle, Uuid::new_v4(), "e3fault", PriorityClass::Scheduled).await?;
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, "e3fault").await.status,
        DerivationStatus::Assigned,
        "merged + dispatched before the fault"
    );

    db.pool.close().await;
    complete_failure(
        &handle,
        "e3fault-w",
        &test_drv_path("e3fault"),
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "boom",
    )
    .await?;
    barrier(&handle).await;

    let post = expect_drv(&handle, "e3fault").await;
    assert_eq!(
        post.status,
        DerivationStatus::Assigned,
        "pre-report state preserved when the appending transaction fails"
    );
    assert!(
        post.retry.poisoned_at.is_none(),
        "no in-memory poison without a committed record"
    );
    assert!(
        logs_contain("appending transaction failed"),
        "the bounded re-delivery path logged"
    );
    Ok(())
}

/// Bounded re-delivery convergence: a transient fault (the ledger table
/// briefly missing) fails the first appending transaction; once the
/// fault clears, the re-delivered completion converges to the same
/// Poisoned + cascade outcome with exactly one ledger row.
#[tokio::test]
async fn phase1b_e3_record_tx_retry_converges() -> TestResult {
    let (db, handle, _task, mut rx) = setup_with_worker("e3conv-w", "x86_64-linux").await?;
    let child = "e3conv-c";
    let parent = "e3conv-p";
    let _ev = merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![make_node(child), make_node(parent)],
        vec![make_test_edge(parent, child)],
        false,
    )
    .await?;
    let _ = recv_assignment(&mut rx).await;

    // Break only the appending transaction: hide the ledger table.
    sqlx::query("ALTER TABLE drv_attempts RENAME TO drv_attempts_hidden")
        .execute(&db.pool)
        .await?;
    complete_failure(
        &handle,
        "e3conv-w",
        &test_drv_path(child),
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "boom",
    )
    .await?;
    barrier(&handle).await;
    assert_ne!(
        expect_drv(&handle, child).await.status,
        DerivationStatus::Poisoned,
        "stays pre-report while the fault holds"
    );

    // Clear the fault; the delayed re-delivery (1 s) then lands.
    sqlx::query("ALTER TABLE drv_attempts_hidden RENAME TO drv_attempts")
        .execute(&db.pool)
        .await?;
    let mut converged = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if expect_drv(&handle, child).await.status == DerivationStatus::Poisoned {
            converged = true;
            break;
        }
    }
    assert!(
        converged,
        "re-delivered completion must converge to Poisoned"
    );
    assert_eq!(
        expect_drv(&handle, parent).await.status,
        DerivationStatus::DependencyFailed,
        "cascade still runs on the re-delivered event"
    );
    assert_eq!(
        ledger_classes(&db.pool, child).await,
        vec!["permanent"],
        "exactly one ledger row after convergence"
    );
    Ok(())
}
