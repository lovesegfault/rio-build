//! Open attempt view tests (the OA5 / busy-bridge read).

use rio_test_support::TestDb;
use uuid::Uuid;

use super::insert_test_derivation;
use crate::db::{AssignmentStatus, SchedulerDb};
use crate::state::ExecutorId;

/// Insert a `drv_executions` row the way the *pull transaction* writes
/// it: the source-node binding when known. Direct SQL on purpose — the
/// production writer lands with the `PullAssignment` handler; this
/// fixture pins the row shape the view must read.
async fn insert_pull_execution(
    pool: &sqlx::PgPool,
    exec_id: Uuid,
    drv_hash32: &str,
    executor_id: &str,
    source_node: Option<&str>,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO drv_executions \
             (exec_id, drv_hash, executor_id, started_at, source_node) \
         VALUES ($1, $2, $3, now(), $4)",
    )
    .bind(exec_id)
    .bind(drv_hash32)
    .bind(executor_id)
    .bind(source_node)
    .execute(pool)
    .await?;
    Ok(())
}

/// 32-char drv_hash for the `drv_executions.drv_hash` CHAR(32) column
/// (the log-hash form, not the DAG key).
fn log_hash(tag: &str) -> String {
    format!("{tag:0>32}")
}

// r[verify sched.admin.list-open-attempts+2]
/// The open-attempt view returns exactly the open attempt — not the
/// terminal-filled one, and not an assignment that never got its
/// execution row (the join requires the pull-minted pair).
#[tokio::test]
async fn open_attempt_view_filters_terminal_and_unminted_rows() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // (1) Open attempt: active assignment + execution row.
    let open_drv = insert_test_derivation(&db, "oa-open").await?;
    let open_exec = Uuid::now_v7();
    db.insert_assignment(open_drv, &ExecutorId::from("oa-open"), 7, open_exec)
        .await?;
    insert_pull_execution(
        &test_db.pool,
        open_exec,
        &log_hash("oaopen"),
        "oa-open",
        Some("node-1"),
    )
    .await?;

    // (2) Terminal-filled attempt: assignment closed AND the attempt
    // row carries a terminal fill — excluded on both counts.
    let term_drv = insert_test_derivation(&db, "oa-term").await?;
    let term_exec = Uuid::now_v7();
    db.insert_assignment(term_drv, &ExecutorId::from("oa-term"), 7, term_exec)
        .await?;
    insert_pull_execution(
        &test_db.pool,
        term_exec,
        &log_hash("oaterm"),
        "oa-term",
        None,
    )
    .await?;
    db.update_assignment_status(term_drv, AssignmentStatus::Completed)
        .await?;
    sqlx::query(
        "INSERT INTO drv_attempts \
             (attempt_id, derivation_id, exec_id, executor_id, event_kind, outcome_class, \
              termination_reason, reporting_party, occurred_at) \
         VALUES ($1, $2, $3, 'oa-term', 'attempt', 'executor_crash', 'unreported', \
                 'scheduler', now())",
    )
    .bind(Uuid::now_v7())
    .bind(term_drv)
    .bind(term_exec)
    .execute(&test_db.pool)
    .await?;

    // (3) Assignment with NO execution row: the pull mint writes the
    // pair in one transaction, so a lone assignment row (a test
    // shortcut, or a partial write that never committed) is not an
    // attempt and must not appear in the view.
    let bare_drv = insert_test_derivation(&db, "oa-bare").await?;
    db.insert_assignment(bare_drv, &ExecutorId::from("oa-bare"), 7, Uuid::now_v7())
        .await?;

    let rows = db.list_open_pull_attempts().await?;
    assert_eq!(
        rows.len(),
        1,
        "exactly the open attempt is listed, got {rows:?}"
    );
    let row = &rows[0];
    assert_eq!(row.derivation_id, open_drv);
    assert_eq!(row.drv_hash, "oa-open");
    assert_eq!(
        row.drv_path,
        rio_test_support::fixtures::test_drv_path("oa-open")
    );
    assert_eq!(row.exec_id, open_exec);
    assert_eq!(row.executor_id, "oa-open");
    assert_eq!(row.system, "x86_64-linux");
    assert!(!row.is_fixed_output, "test derivation defaults to non-FOD");
    assert_eq!(row.source_node.as_deref(), Some("node-1"));
    assert_eq!(row.generation, 7);
    assert!(
        row.assigned_at_epoch_secs > 0.0,
        "assigned_at must be populated"
    );
    assert!(
        row.age_secs >= 0.0 && row.age_secs < 600.0,
        "age must be small for a row written moments ago, got {}",
        row.age_secs
    );
    Ok(())
}

/// An attempt whose drv_attempts row got a terminal fill while its
/// assignment row is still active is NOT open (terminality is decided
/// by the fill, not only by the assignment status).
#[tokio::test]
async fn open_attempt_view_respects_terminal_fill_alone() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let drv = insert_test_derivation(&db, "oa-filled").await?;
    let exec = Uuid::now_v7();
    db.insert_assignment(drv, &ExecutorId::from("oa-filled"), 3, exec)
        .await?;
    insert_pull_execution(
        &test_db.pool,
        exec,
        &log_hash("oafilled"),
        "oa-filled",
        None,
    )
    .await?;
    // Terminal fill lands (e.g. establishment) but the assignment row
    // close is still in flight.
    sqlx::query(
        "INSERT INTO drv_attempts \
             (attempt_id, derivation_id, exec_id, executor_id, event_kind, outcome_class, \
              termination_reason, reporting_party, occurred_at) \
         VALUES ($1, $2, $3, 'oa-filled', 'attempt', 'executor_crash', 'unreported', \
                 'scheduler', now())",
    )
    .bind(Uuid::now_v7())
    .bind(drv)
    .bind(exec)
    .execute(&test_db.pool)
    .await?;

    let rows = db.list_open_pull_attempts().await?;
    assert!(
        rows.is_empty(),
        "terminal-filled attempt must not be listed as open, got {rows:?}"
    );
    Ok(())
}

// r[verify sched.attempt.establishment-window+3]
/// The dispatched deadline persisted by the fenced mint round-trips
/// through the open-attempt view (the establishment window's anchor).
#[tokio::test]
async fn mint_persists_dispatched_deadline_and_view_returns_it() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let drv = insert_test_derivation(&db, "oa-deadline").await?;
    let exec = Uuid::now_v7();
    let committed = db
        .mint_pull_attempt_fenced(
            drv,
            &ExecutorId::from("oa-deadline"),
            1,
            exec,
            &log_hash("oadeadline"),
            Some("node-1"),
            Some(1234.0),
        )
        .await?;
    assert!(committed, "the fenced mint commits on a fresh cluster");

    let rows = db.list_open_pull_attempts().await?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].exec_id, exec);
    assert_eq!(
        rows[0].deadline_secs,
        Some(1234.0),
        "the dispatched deadline round-trips through the view"
    );
    assert_eq!(rows[0].source_node.as_deref(), Some("node-1"));
    Ok(())
}
