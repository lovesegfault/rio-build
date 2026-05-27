//! Establishment sweep for open pull-mode attempts: the red-first
//! battery (window, store-probe adopt, charge+requeue, the generation
//! fence, and the stream-mode exclusion).

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

// r[verify sched.attempt.establishment-window]
/// (a) An open pull-mode attempt past deadline + slack with no terminal
/// row is established exactly once as executor_crash/unreported,
/// charged to failed_builders + failure_count, and the drv requeues.
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
        info.retry.failed_builders.contains("est-a"),
        "the attempt's executor identity joins the exclusion set, got {:?}",
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

// r[verify sched.attempt.establishment-window]
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

// r[verify sched.attempt.establishment-window]
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

// r[verify sched.attempt.establishment-window]
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

// r[verify sched.attempt.establishment-window]
/// (e) Mixed fleet: an active assignment+execution pair written exactly
/// as the as-built stream dispatch writes them, past any window, is
/// NEVER visited or established by the new sweep.
#[tokio::test]
async fn establishment_never_visits_stream_attempts() -> TestResult {
    let (db, handle, _task, mut rx) = setup_with_worker("w-est", "x86_64-linux").await?;
    let _ev = merge_single_node(&handle, Uuid::new_v4(), "est-e", PriorityClass::Scheduled).await?;
    let stream_assignment = recv_assignment(&mut rx).await;
    let stream_exec: uuid::Uuid = stream_assignment.exec_id.parse()?;
    backdate_assignment(&db.pool, stream_exec).await?;

    tick(&handle).await?;
    tick(&handle).await?;

    assert!(
        attempt_rows_for(&db.pool, "est-e").await.is_empty(),
        "the pull-mode sweep never establishes stream attempts"
    );
    let info = expect_drv(&handle, "est-e").await;
    assert_eq!(
        info.status,
        crate::state::DerivationStatus::Assigned,
        "the stream attempt is untouched (its own machinery owns it)"
    );
    assert_eq!(
        assignment_statuses(&db.pool, "est-e").await,
        vec!["pending"],
        "the stream assignment row stays open"
    );
    Ok(())
}
