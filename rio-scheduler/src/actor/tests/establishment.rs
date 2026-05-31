//! Establishment sweep for open pull-mode attempts: the red-first
//! battery (window, store-probe adopt, charge+requeue, and the
//! generation fence).

use super::*;
use crate::actor::pull::PullOutcome;
use crate::state::OutcomeClass;

/// Pull helper (same as the pull battery's, local to this module).
async fn pull_deliver(handle: &ActorHandle, intent: &str) -> rio_proto::types::WorkAssignment {
    let outcome = handle
        .query_unchecked(|reply| ActorCommand::PullAssignment {
            intent_id: intent.into(),
            auth_intent: Some(intent.into()),
            reply,
        })
        .await
        .expect("actor alive");
    match outcome {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("expected Deliver, got {other:?}"),
    }
}

/// Backdate the attempt's assignment row so its age exceeds any
/// deadline + slack the sweep can compute.
async fn backdate_assignment(pool: &sqlx::PgPool, exec_id: uuid::Uuid) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE assignments SET assigned_at = now() - interval '100 days' WHERE exec_id = $1",
    )
    .bind(exec_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn attempt_rows_for(pool: &sqlx::PgPool, drv_hash: &str) -> Vec<LedgerRow> {
    ledger_rows(pool, drv_hash).await
}

async fn assignment_statuses(pool: &sqlx::PgPool, drv_hash: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT a.status FROM assignments a \
         JOIN derivations d ON d.derivation_id = a.derivation_id \
         WHERE d.drv_hash = $1 ORDER BY a.assigned_at",
    )
    .bind(drv_hash)
    .fetch_all(pool)
    .await
    .expect("assignment statuses")
}

// r[verify sched.attempt.establishment-window+3]
/// (a) An open pull-mode attempt past deadline + slack with no terminal
/// row is established exactly once as executor_crash/unreported,
/// charged to failure_count, and the drv requeues.
/// No node attribution ever arrives here (no binding ack, no
/// controller report), so the established charge carries NO exclusion
/// key (decision P12: the budget key is the controller-authoritative
/// source node only — an unattributed crash cannot occupy a
/// distinct-source slot); the node-keyed cases live in the AD2
/// battery (`establishment_charge_carries_node_*`).
#[tokio::test]
async fn establishment_charges_and_requeues_after_window() -> TestResult {
    let (db, handle, _task) = setup().await;
    let _ev = merge_single_node(&handle, Uuid::new_v4(), "est-a", PriorityClass::Scheduled).await?;
    let assignment = pull_deliver(&handle, "est-a").await;
    let exec_id: uuid::Uuid = assignment.exec_id.parse()?;
    backdate_assignment(&db.pool, exec_id).await?;

    tick(&handle).await?;

    let rows = attempt_rows_for(&db.pool, "est-a").await;
    assert_eq!(rows.len(), 1, "established exactly once");
    assert_eq!(rows[0].outcome_class, OutcomeClass::ExecutorCrash.as_str());
    assert_eq!(rows[0].termination_reason.as_deref(), Some("unreported"));
    assert_eq!(rows[0].exec_id, Some(exec_id));

    let info = expect_drv(&handle, "est-a").await;
    assert_eq!(
        info.status,
        crate::state::DerivationStatus::Ready,
        "the drv requeues after establishment"
    );
    assert_eq!(info.retry.failure_count, 1, "charged once (C2)");
    assert!(
        info.retry.failed_builders.is_empty(),
        "an unattributed establishment contributes no exclusion key \
         (P12: source-node keys only), got {:?}",
        info.retry.failed_builders
    );
    assert_eq!(
        assignment_statuses(&db.pool, "est-a").await,
        vec!["failed"],
        "the assignment row minted by the pull is closed"
    );

    // A second sweep pass establishes nothing further.
    tick(&handle).await?;
    let rows = attempt_rows_for(&db.pool, "est-a").await;
    assert_eq!(rows.len(), 1, "the establishment is idempotent");
    assert_eq!(expect_drv(&handle, "est-a").await.retry.failure_count, 1);
    Ok(())
}

// r[verify sched.attempt.establishment-window+3]
/// (b) The same attempt with its outputs present in the store is
/// adopted as completed (store-probe arm) and never charged.
#[tokio::test]
async fn establishment_store_probe_adopts_completed_attempt() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    let db_pool = {
        // setup_with_mock_store returns the TestDb first; rebind for clarity.
        _db.pool.clone()
    };
    let out_path = test_store_path("est-b-out");

    let mut node = make_node("est-b");
    node.expected_output_paths = vec![out_path.clone()];
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;
    let assignment = pull_deliver(&handle, "est-b").await;
    let exec_id: uuid::Uuid = assignment.exec_id.parse()?;
    backdate_assignment(&db_pool, exec_id).await?;
    // The pod uploaded its outputs but died before its report landed:
    // the outputs appear in the store only after the pull.
    store.seed_with_content(&out_path, b"est-b output");

    tick(&handle).await?;

    let info = expect_drv(&handle, "est-b").await;
    assert_eq!(
        info.status,
        crate::state::DerivationStatus::Completed,
        "outputs present in the store → adopted as completed"
    );
    assert!(
        attempt_rows_for(&db_pool, "est-b").await.is_empty(),
        "the adopt arm never charges"
    );
    assert_eq!(info.retry.failure_count, 0);
    assert_eq!(
        assignment_statuses(&db_pool, "est-b").await,
        vec!["completed"],
        "the assignment row is closed as completed"
    );
    Ok(())
}

// r[verify sched.attempt.establishment-window+3]
/// (c) An attempt inside the window is never established.
#[tokio::test]
async fn establishment_never_fires_inside_window() -> TestResult {
    let (db, handle, _task) = setup().await;
    let _ev = merge_single_node(&handle, Uuid::new_v4(), "est-c", PriorityClass::Scheduled).await?;
    let _assignment = pull_deliver(&handle, "est-c").await;

    tick(&handle).await?;
    tick(&handle).await?;

    assert!(
        attempt_rows_for(&db.pool, "est-c").await.is_empty(),
        "no establishment inside the window"
    );
    let info = expect_drv(&handle, "est-c").await;
    assert_eq!(info.status, crate::state::DerivationStatus::Running);
    assert_eq!(info.retry.failure_count, 0);
    assert_eq!(
        assignment_statuses(&db.pool, "est-c").await,
        vec!["pending"]
    );
    Ok(())
}

// r[verify sched.attempt.establishment-window+3]
/// (d) The establishment transaction at a below-floor serving
/// generation writes nothing (the fence applies to establishment too).
#[tokio::test]
async fn establishment_below_floor_writes_nothing() -> TestResult {
    let (db, handle, _task) = setup().await;
    let _ev = merge_single_node(&handle, Uuid::new_v4(), "est-d", PriorityClass::Scheduled).await?;
    let assignment = pull_deliver(&handle, "est-d").await;
    let exec_id: uuid::Uuid = assignment.exec_id.parse()?;
    backdate_assignment(&db.pool, exec_id).await?;
    // A successor's claim raises the durable floor above this
    // replica's serving generation (1).
    sqlx::query("INSERT INTO leader_generation_claims (generation, holder_id) VALUES (2, 'next')")
        .execute(&db.pool)
        .await?;

    tick(&handle).await?;

    assert!(
        attempt_rows_for(&db.pool, "est-d").await.is_empty(),
        "below-floor establishment writes nothing"
    );
    let info = expect_drv(&handle, "est-d").await;
    assert_eq!(
        info.status,
        crate::state::DerivationStatus::Running,
        "the attempt is left untouched for the real leader to resolve"
    );
    assert_eq!(info.retry.failure_count, 0, "no charge");
    assert_eq!(
        assignment_statuses(&db.pool, "est-d").await,
        vec!["pending"],
        "the assignment row stays open"
    );
    Ok(())
}

// r[verify sched.attempt.establishment-window+3]
/// (g) The window is anchored to the deadline the attempt was
/// dispatched with: a sweep-time re-solve that is smaller than the
/// persisted deadline must NOT shrink the window (no establishment
/// while the attempt is inside the dispatched deadline + slack), and
/// the attempt still establishes once the persisted anchor has passed.
#[tokio::test]
async fn establishment_window_anchored_to_dispatched_deadline() -> TestResult {
    let (db, handle, _task) = setup().await;
    let _ev = merge_single_node(&handle, Uuid::new_v4(), "est-g", PriorityClass::Scheduled).await?;
    let assignment = pull_deliver(&handle, "est-g").await;
    let exec_id: uuid::Uuid = assignment.exec_id.parse()?;

    // The mint persisted the deadline this attempt was dispatched
    // under (the same solve that sizes activeDeadlineSeconds).
    let minted: Option<f64> =
        sqlx::query_scalar("SELECT deadline_secs FROM drv_executions WHERE exec_id = $1")
            .bind(exec_id)
            .fetch_one(&db.pool)
            .await?;
    assert!(
        minted.is_some_and(|d| d > 0.0),
        "the pull mint persists the dispatched deadline, got {minted:?}"
    );

    // Simulate the dispatched deadline being far LARGER than anything
    // the sweep can re-solve now (the inverse of an estimator/hw-table
    // shrink): the attempt is backdated past the re-solved window but
    // not past the dispatched one — it must stay open and uncharged.
    sqlx::query("UPDATE drv_executions SET deadline_secs = $2 WHERE exec_id = $1")
        .bind(exec_id)
        .bind(20_000_000.0_f64)
        .execute(&db.pool)
        .await?;
    backdate_assignment(&db.pool, exec_id).await?;
    tick(&handle).await?;
    assert!(
        attempt_rows_for(&db.pool, "est-g").await.is_empty(),
        "no establishment while the attempt is inside its dispatched deadline + slack"
    );
    let info = expect_drv(&handle, "est-g").await;
    assert_eq!(info.status, crate::state::DerivationStatus::Running);
    assert_eq!(info.retry.failure_count, 0, "no charge inside the window");
    assert_eq!(
        assignment_statuses(&db.pool, "est-g").await,
        vec!["pending"],
        "the assignment row stays open"
    );

    // Once the dispatched anchor is genuinely in the past the sweep
    // establishes exactly as before.
    sqlx::query("UPDATE drv_executions SET deadline_secs = 1.0 WHERE exec_id = $1")
        .bind(exec_id)
        .execute(&db.pool)
        .await?;
    tick(&handle).await?;
    let rows = attempt_rows_for(&db.pool, "est-g").await;
    assert_eq!(rows.len(), 1, "established once the window truly closed");
    assert_eq!(rows[0].outcome_class, OutcomeClass::ExecutorCrash.as_str());
    Ok(())
}

// r[verify sched.attempt.synthesized-verdict]
/// (f) An attempt closed charge-free by a controller-synthesized verdict
/// is never re-established: the sweep adds no executor_crash row and no
/// charge for that exec, even far past the window.
#[tokio::test]
async fn establishment_skips_synthesized_closed_attempt() -> TestResult {
    let (db, handle, _task) = setup().await;
    let _ev = merge_single_node(&handle, Uuid::new_v4(), "est-f", PriorityClass::Scheduled).await?;
    let assignment = pull_deliver(&handle, "est-f").await;
    let exec_id: uuid::Uuid = assignment.exec_id.parse()?;

    // The controller synthesizes Preempted for the open attempt (the
    // AD5/C6 successor): closed charge-free, requeued at that fold.
    handle
        .query_unchecked(|reply| ActorCommand::ReportAttemptOutcome {
            identity: crate::actor::pull::AttemptIdentity {
                intent_id: Some("est-f".into()),
                job_name: None,
                exec_id: None,
            },
            reason: rio_proto::types::AttemptTerminalReason::Preempted,
            node_name: Some("node-est-f".into()),
            reply,
        })
        .await
        .expect("actor alive")
        .expect("synthesized report acked");
    let rows = attempt_rows_for(&db.pool, "est-f").await;
    assert_eq!(rows.len(), 1, "the synthesized close wrote one row");
    assert_eq!(rows[0].outcome_class, OutcomeClass::Disconnected.as_str());

    // Even far past any window the sweep establishes nothing further.
    backdate_assignment(&db.pool, exec_id).await?;
    tick(&handle).await?;
    let rows = attempt_rows_for(&db.pool, "est-f").await;
    assert_eq!(
        rows.len(),
        1,
        "no executor_crash establishment lands on a synthesized-closed attempt"
    );
    assert_eq!(rows[0].outcome_class, OutcomeClass::Disconnected.as_str());
    let info = expect_drv(&handle, "est-f").await;
    assert_eq!(
        info.retry.failure_count, 0,
        "still uncharged after the sweep"
    );
    Ok(())
}
