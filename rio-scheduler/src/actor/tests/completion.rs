//! Completion handling: retry/poison thresholds, dep-chain release, duplicate idempotence.
// r[verify sched.completion.idempotent]
// r[verify sched.state.transitions]
// r[verify sched.state.terminal-idempotent]

use super::*;
use tracing_test::traced_test;

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

    // Should be retried: retry_count=1. backoff_until is set so
    // the derivation stays Ready (not immediately re-dispatched).
    // failed_workers contains first_worker so when backoff elapses,
    // retry goes to the OTHER worker.
    let info2 = handle
        .debug_query_derivation("retry-hash")
        .await?
        .expect("exists");
    assert_eq!(
        info2.retry_count, 1,
        "transient failure should increment retry_count"
    );
    // Status: Ready with backoff_until set. NOT Assigned — backoff
    // blocks immediate re-dispatch. If it IS Assigned, dispatch
    // raced us between complete_failure and query — still fine for
    // the test, the worker must be different (failed_workers
    // exclusion).
    match info2.status {
        DerivationStatus::Ready => {
            // Backoff active: assigned_worker cleared.
            assert!(
                info2.assigned_worker.is_none(),
                "Ready after failure → assigned_worker cleared"
            );
        }
        DerivationStatus::Assigned => {
            // Raced: dispatch happened. Worker MUST be different
            // (failed_workers exclusion).
            let retry_worker = info2.assigned_worker.expect("Assigned → worker set");
            assert_ne!(
                retry_worker, first_worker,
                "failed_workers exclusion: retry must go to a DIFFERENT worker"
            );
        }
        other => panic!("expected Ready or Assigned, got {other:?}"),
    }
    Ok(())
}

/// max_retries (default 2) exhausted → poison (the `retry_count >=
/// max_retries` branch, distinct from POISON_THRESHOLD 3-distinct-
/// workers).
///
/// Uses `debug_force_assign` between failures to bypass backoff_until
/// and failed_workers exclusion (which correctly prevent immediate
/// same-worker retry). The test drives the state machine directly to
/// test the completion handler's max_retries logic, not dispatch.
#[tokio::test]
async fn test_transient_failure_max_retries_poisons() -> TestResult {
    let (_db, handle, _task, _rx) = setup_with_worker("flaky-worker", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let p_maxretry = test_drv_path("maxretry-hash");
    let _event_rx =
        merge_single_node(&handle, build_id, "maxretry-hash", PriorityClass::Scheduled).await?;

    // Default RetryPolicy::max_retries = 2. Fail 3 times:
    // retry_count 0 -> 1 (retry), 1 -> 2 (retry), 2 >= 2 -> Poisoned.
    //
    // debug_force_assign before each failure (except the first —
    // initial dispatch happened) bypasses backoff so the completion
    // handler sees Assigned state and processes the failure.
    for attempt in 0..3 {
        if attempt > 0 {
            // Force back to Assigned: backoff would block real
            // dispatch, and failed_workers now excludes flaky-worker.
            let ok = handle
                .debug_force_assign("maxretry-hash", "flaky-worker")
                .await?;
            assert!(ok, "force-assign should succeed for Ready derivation");
        }
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
        "3 transient failures (retry_count >= max_retries=2) should poison"
    );
    Ok(())
}

/// Derivation poisoned after POISON_THRESHOLD (3) distinct worker failures.
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
    //
    // debug_force_assign between failures: backoff_until prevents
    // immediate re-dispatch after each failure. The test drives
    // the assignment directly — we're testing the poison-threshold
    // logic (3 distinct failed_workers → poisoned), not dispatch
    // timing.
    for (i, worker) in ["poison-w1", "poison-w2", "poison-w3"].iter().enumerate() {
        if i > 0 {
            // After the first failure, dispatch won't re-assign
            // until backoff elapses. Force it. The worker is
            // distinct each time so failed_workers exclusion
            // wouldn't block (if backoff weren't in play).
            let ok = handle.debug_force_assign(drv_hash, worker).await?;
            assert!(ok, "force-assign should succeed");
        }
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
        PoisonConfig::default().threshold
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

// r[verify sched.retry.per-worker-budget]
/// InfrastructureFailure is a worker-local problem (FUSE EIO, cgroup
/// setup fail, OOM-kill of the build process) — NOT the build's fault.
/// 3× InfrastructureFailure on distinct workers → failed_workers stays
/// EMPTY, derivation NOT poisoned. Contrast with the TransientFailure
/// test above, where 3 distinct failures → poison.
#[tokio::test]
async fn test_infrastructure_failure_does_not_count_toward_poison() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // 4 workers so re-dispatch always has a candidate.
    let _rx1 = connect_worker(&handle, "infra-w1", "x86_64-linux", 1).await?;
    let _rx2 = connect_worker(&handle, "infra-w2", "x86_64-linux", 1).await?;
    let _rx3 = connect_worker(&handle, "infra-w3", "x86_64-linux", 1).await?;
    let _rx4 = connect_worker(&handle, "infra-w4", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let drv_hash = "infra-drv";
    let drv_path = test_drv_path(drv_hash);
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // 3× InfrastructureFailure from distinct workers. In the
    // TransientFailure case this would poison; here it must not.
    // handle_infrastructure_failure does reset_to_ready WITHOUT
    // failed_workers insert and WITHOUT backoff — so immediate
    // re-dispatch to whatever worker wins. We drive via
    // debug_force_assign to make the per-worker assertion
    // deterministic (avoid racing dispatch).
    for (i, worker) in ["infra-w1", "infra-w2", "infra-w3"].iter().enumerate() {
        if i > 0 {
            let ok = handle.debug_force_assign(drv_hash, worker).await?;
            assert!(ok, "force-assign should succeed (no backoff, no exclusion)");
        }
        complete_failure(
            &handle,
            worker,
            &drv_path,
            rio_proto::types::BuildResultStatus::InfrastructureFailure,
            &format!("infra failure {i}"),
        )
        .await?;
    }

    let info = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("derivation should exist");
    // Exit criterion: 3× InfrastructureFailure → failed_workers.is_empty()
    assert!(
        info.failed_workers.is_empty(),
        "InfrastructureFailure must NOT insert into failed_workers, got {:?}",
        info.failed_workers
    );
    assert_eq!(
        info.failure_count, 0,
        "InfrastructureFailure must NOT increment failure_count"
    );
    assert_eq!(
        info.retry_count, 0,
        "InfrastructureFailure must NOT increment retry_count (no retry penalty)"
    );
    // Exit criterion: NOT poisoned. Ready-or-Assigned (no backoff →
    // immediate re-dispatch may have won the race).
    assert!(
        matches!(
            info.status,
            DerivationStatus::Ready | DerivationStatus::Assigned
        ),
        "3× InfrastructureFailure → NOT poisoned, got {:?}",
        info.status
    );

    // 4th attempt: now send TransientFailure. This DOES count — proving
    // the derivation is still live and the counting path still works.
    let ok = handle.debug_force_assign(drv_hash, "infra-w4").await?;
    assert!(ok);
    complete_failure(
        &handle,
        "infra-w4",
        &drv_path,
        rio_proto::types::BuildResultStatus::TransientFailure,
        "now this one counts",
    )
    .await?;

    let info = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("exists");
    assert_eq!(
        info.failed_workers.len(),
        1,
        "1× TransientFailure after 3× InfrastructureFailure → exactly 1 failed worker"
    );
    assert_eq!(info.failure_count, 1);
    assert_ne!(
        info.status,
        DerivationStatus::Poisoned,
        "1 failed worker < threshold(3), still not poisoned"
    );
    Ok(())
}

/// With `require_distinct_workers=false`, 3× TransientFailure on the
/// SAME worker → poisoned. Contrast with default distinct mode where
/// same-worker repeats don't count (HashSet insert is idempotent).
/// Primary use case: single-worker dev deployments, where 3 distinct
/// workers will never exist.
#[tokio::test]
async fn test_non_distinct_mode_counts_same_worker() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    // Configure: require_distinct_workers = false, threshold = 3.
    // Also max_retries = 5 so we hit the poison-threshold branch,
    // not the max_retries branch (default max_retries=2 would
    // poison at retry_count>=2, masking what we're testing).
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |a| {
        a.with_poison_config(PoisonConfig {
            threshold: 3,
            require_distinct_workers: false,
        })
    });
    let _db = db;

    let _rx = connect_worker(&handle, "solo-worker", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let drv_hash = "nondistinct-drv";
    let drv_path = test_drv_path(drv_hash);
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // 3× TransientFailure on the SAME worker. In default distinct
    // mode: failed_workers={solo-worker} stays at len()=1 < 3 → not
    // poisoned (blocked by max_retries instead). In non-distinct mode:
    // failure_count goes 1→2→3, and 3 >= threshold → poisoned.
    for i in 0..3 {
        if i > 0 {
            // Backoff + failed_workers exclusion block real dispatch
            // back to solo-worker. Force it.
            let ok = handle.debug_force_assign(drv_hash, "solo-worker").await?;
            assert!(ok, "force-assign should succeed");
        }
        complete_failure(
            &handle,
            "solo-worker",
            &drv_path,
            rio_proto::types::BuildResultStatus::TransientFailure,
            &format!("same-worker failure {i}"),
        )
        .await?;

        let info = handle
            .debug_query_derivation(drv_hash)
            .await?
            .expect("exists");
        // failed_workers (HashSet) stays at 1 — same worker every time.
        assert_eq!(
            info.failed_workers.len(),
            1,
            "HashSet: same worker inserted once, stays len()=1"
        );
        // failure_count increments regardless.
        assert_eq!(
            info.failure_count,
            i + 1,
            "non-distinct: failure_count increments unconditionally"
        );
    }

    // Exit criterion: 3× same-worker TransientFailure under
    // require_distinct_workers=false → POISONED.
    let info = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("exists");
    assert_eq!(
        info.status,
        DerivationStatus::Poisoned,
        "non-distinct mode: 3× same-worker failures → poisoned (failure_count={} >= threshold=3)",
        info.failure_count
    );
    Ok(())
}

/// Negative control for non-distinct mode: with DEFAULT config
/// (require_distinct_workers=true), 3× same-worker TransientFailure
/// does NOT poison via the threshold path — failed_workers.len()
/// stays at 1. (max_retries=2 default still poisons, but via a
/// different branch — this test isolates the distinct-workers logic
/// by raising max_retries.)
#[tokio::test]
async fn test_distinct_mode_same_worker_does_not_poison_via_threshold() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    // Default PoisonConfig (distinct=true, threshold=3) — but raise
    // max_retries so we can observe 3 same-worker failures WITHOUT
    // hitting max_retries. This is the control for the non-distinct
    // test above: same inputs, opposite config, opposite outcome.
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |mut a| {
        a.retry_policy.max_retries = 10;
        a
    });
    let _db = db;

    let _rx = connect_worker(&handle, "ctrl-worker", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let drv_hash = "ctrl-drv";
    let drv_path = test_drv_path(drv_hash);
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    for i in 0..3 {
        if i > 0 {
            let ok = handle.debug_force_assign(drv_hash, "ctrl-worker").await?;
            assert!(ok);
        }
        complete_failure(
            &handle,
            "ctrl-worker",
            &drv_path,
            rio_proto::types::BuildResultStatus::TransientFailure,
            &format!("ctrl failure {i}"),
        )
        .await?;
    }

    let info = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("exists");
    assert_eq!(info.failed_workers.len(), 1, "HashSet: same worker = 1");
    assert_eq!(info.failure_count, 3, "flat count still increments");
    // NOT poisoned: distinct mode uses failed_workers.len()=1 < 3.
    assert_ne!(
        info.status,
        DerivationStatus::Poisoned,
        "distinct mode (default): 3× same-worker → NOT poisoned via threshold \
         (failed_workers.len()=1 < 3); would need 3 DISTINCT workers"
    );
    Ok(())
}

/// Completing a child releases its parent to Ready in a dependency chain.
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

/// Duplicate ProcessCompletion is an idempotent no-op.
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

// ---------------------------------------------------------------------------
// Completion edge cases: unknown drv, wrong state, unknown status
// ---------------------------------------------------------------------------

/// ProcessCompletion for a drv_key the actor has never seen → warn
/// + ignore. Could happen after stale worker reconnect with a build
/// from a previous scheduler generation.
#[tokio::test]
#[traced_test]
async fn test_completion_unknown_drv_key_ignored() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Completion for a drv that was never merged.
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "ghost-worker".into(),
            drv_key: "never-existed-drv-hash".into(),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                ..Default::default()
            },
            peak_memory_bytes: 0,
            output_size_bytes: 0,
            peak_cpu_cores: 0.0,
        })
        .await?;
    barrier(&handle).await;

    assert!(
        logs_contain("unknown derivation") || logs_contain("not in DAG"),
        "expected warn for unknown drv_key"
    );
    Ok(())
}

/// Completion for a drv in Ready (never dispatched) → warn + ignore.
/// A worker can't complete something it wasn't assigned.
#[tokio::test]
#[traced_test]
async fn test_completion_for_non_running_state_ignored() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Merge but DON'T connect a worker — drv stays Ready.
    let build_id = Uuid::new_v4();
    merge_single_node(&handle, build_id, "ready-drv", PriorityClass::Scheduled).await?;

    // Send completion. Drv is Ready, not Assigned/Running.
    complete_success_empty(&handle, "phantom-w", &test_drv_path("ready-drv")).await?;
    barrier(&handle).await;

    assert!(
        logs_contain("not in assigned") || logs_contain("unexpected state"),
        "expected warn for wrong-state completion"
    );

    // Drv still Ready (completion ignored).
    let info = handle
        .debug_query_derivation("ready-drv")
        .await?
        .expect("drv exists");
    assert!(
        !matches!(info.status, DerivationStatus::Completed),
        "completion from wrong state should be ignored, status={:?}",
        info.status
    );
    Ok(())
}

/// Unknown BuildResultStatus value (e.g. from a newer worker) → warn,
/// treat as transient failure. Don't panic, don't get stuck.
#[tokio::test]
#[traced_test]
async fn test_unknown_build_status_treated_as_transient() -> TestResult {
    let (_db, handle, _task, mut _rx) = setup_with_worker("unk-w", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let drv_path = test_drv_path("unk-status");
    merge_single_node(&handle, build_id, "unk-status", PriorityClass::Scheduled).await?;

    // Wait for dispatch (drv → Assigned).
    barrier(&handle).await;

    // Send completion with an invalid status int (9999).
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "unk-w".into(),
            drv_key: drv_path.clone(),
            result: rio_proto::types::BuildResult {
                status: 9999, // not a valid enum
                error_msg: "mystery".into(),
                ..Default::default()
            },
            peak_memory_bytes: 0,
            output_size_bytes: 0,
            peak_cpu_cores: 0.0,
        })
        .await?;
    barrier(&handle).await;

    assert!(
        logs_contain("unknown BuildResultStatus")
            || logs_contain("Unspecified")
            || logs_contain("unknown status"),
        "expected warn for unknown status enum"
    );
    Ok(())
}

/// After CancelBuild transitions a drv to Cancelled, a later
/// Cancelled completion from the worker is expected → no-op, debug
/// log. This is the "worker acknowledges the cancel signal" path.
#[tokio::test]
#[traced_test]
async fn test_cancelled_completion_after_cancel_is_noop() -> TestResult {
    let (_db, handle, _task, mut _rx) = setup_with_worker("cancel-w", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let drv_path = test_drv_path("cancel-drv");
    merge_single_node(&handle, build_id, "cancel-drv", PriorityClass::Scheduled).await?;
    barrier(&handle).await; // dispatch

    // Cancel the build (transitions drv → Cancelled, sends CancelSignal).
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id,
            reason: "user request".into(),
            reply: reply_tx,
        })
        .await?;
    let _cancelled = reply_rx.await??;

    // Worker reports Cancelled (acknowledging the signal).
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "cancel-w".into(),
            drv_key: drv_path,
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Cancelled.into(),
                ..Default::default()
            },
            peak_memory_bytes: 0,
            output_size_bytes: 0,
            peak_cpu_cores: 0.0,
        })
        .await?;
    barrier(&handle).await;

    // Drv still Cancelled (no spurious state change).
    let info = handle
        .debug_query_derivation("cancel-drv")
        .await?
        .expect("drv exists");
    assert_eq!(info.status, DerivationStatus::Cancelled);

    // The state check at handle_completion's top catches this —
    // drv is already Cancelled (not Assigned/Running), so it hits
    // the "not in assigned/running state, ignoring" early-return.
    // This IS the no-op path for acknowledging a cancel.
    assert!(
        logs_contain("not in assigned/running state"),
        "expected early-return for already-Cancelled state"
    );
    Ok(())
}

// r[verify sched.classify.penalty-overwrite]
/// Misclassification detection: a build routed to "small" (30s cutoff)
/// that completes in 100s (> 2× cutoff) triggers the penalty-overwrite
/// branch (completion.rs:303-324). The detection RECORDS the misclass
/// (metric + EMA overwrite) — next classify() for this pname sees 100s
/// and picks "large".
///
/// dispatch.rs:test_size_class_routing_respects_classification proves
/// `assigned_size_class` is recorded; this proves the detection USES it.
#[tokio::test]
#[traced_test]
async fn test_misclass_detection_on_slow_completion() -> TestResult {
    use crate::assignment::SizeClassConfig;

    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |a| {
        a.with_size_classes(vec![
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
        ])
    });

    // Small worker. No EMA pre-seed → classify() defaults to 30s →
    // routes to "small". Exactly the scenario: something LOOKED small,
    // but wasn't.
    let (tx, mut rx) = mpsc::channel(256);
    handle
        .send_unchecked(ActorCommand::WorkerConnected {
            worker_id: "w-small".into(),
            stream_tx: tx,
        })
        .await?;
    handle
        .send_unchecked(ActorCommand::Heartbeat {
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

    // Merge with pname — the EMA-update block (completion.rs:247-249)
    // gates on pname.is_some(). Without it, misclass detection never fires.
    let build_id = Uuid::new_v4();
    let mut node = make_test_node("misc-drv", "x86_64-linux");
    node.pname = "slowthing".into();
    let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    // Dispatch → assigned_size_class="small" set on the state.
    let _ = recv_assignment(&mut rx).await;

    // Complete with duration=100s (start=1000, stop=1100 unix seconds).
    // 100 > 2×30 = 60 → misclass branch fires.
    let drv_path = test_drv_path("misc-drv");
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "w-small".into(),
            drv_key: drv_path,
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                built_outputs: vec![rio_proto::types::BuiltOutput {
                    output_name: "out".into(),
                    output_path: "/nix/store/zzz-slowthing-out".into(),
                    output_hash: vec![0u8; 32],
                }],
                start_time: Some(prost_types::Timestamp {
                    seconds: 1000,
                    nanos: 0,
                }),
                stop_time: Some(prost_types::Timestamp {
                    seconds: 1100,
                    nanos: 0,
                }),
                ..Default::default()
            },
            peak_memory_bytes: 0,
            output_size_bytes: 0,
            peak_cpu_cores: 0.0,
        })
        .await?;
    barrier(&handle).await;

    // THE ASSERTION: the warn! in completion.rs:308-315 fired. If the
    // detection branch was dead code (missing assigned_size_class, missing
    // pname, threshold wrong), this would fail.
    assert!(
        logs_contain("misclassification"),
        "expected 'misclassification: actual > 2× cutoff' warn log; \
         detection branch at completion.rs:303-324 should have fired \
         (duration=100s > 2×30s=60s small-class cutoff)"
    );

    // build_history now has the penalty-overwritten EMA. The overwrite
    // uses actual duration directly (no blend), so ema_duration_secs ≈100.
    // ≥ 60 is enough to prove the write happened (update_build_history's
    // normal blend would give 0.3*100+0.7*30 = 51 for the FIRST sample…
    // but actually the first sample has no prior to blend with — let me
    // just check > 2×cutoff, which distinguishes penalty from no-penalty).
    let ema: f64 =
        sqlx::query_scalar("SELECT ema_duration_secs FROM build_history WHERE pname = 'slowthing'")
            .fetch_one(&db.pool)
            .await?;
    assert!(
        ema > 60.0,
        "penalty-overwrite should have written actual duration (≈100s), \
         not the blended EMA; got {ema}"
    );

    Ok(())
}
