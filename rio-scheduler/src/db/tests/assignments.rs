//! Assignment status transition tests.

use rio_test_support::TestDb;

use super::insert_test_derivation;
use crate::db::{AssignmentStatus, SchedulerDb};
use crate::state::ExecutorId;

/// Non-terminal status (Pending re-set) → completed_at stays NULL.
#[tokio::test]
async fn test_update_assignment_status_pending_no_completed_at() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let drv_id = insert_test_derivation(&db, "bbb").await?;
    db.insert_assignment(
        drv_id,
        &ExecutorId::from("worker-1"),
        1,
        uuid::Uuid::now_v7(),
        None,
    )
    .await?;

    db.update_assignment_status(drv_id, AssignmentStatus::Pending)
        .await?;

    let (status, completed_at): (String, Option<String>) = sqlx::query_as(
        "SELECT status, completed_at::text FROM assignments WHERE derivation_id = $1",
    )
    .bind(drv_id)
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(status, "pending");
    assert!(completed_at.is_none(), "non-terminal → completed_at NULL");
    Ok(())
}

/// Terminal status (Completed) → completed_at = now().
#[tokio::test]
async fn test_update_assignment_status_completed_sets_completed_at() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let drv_id = insert_test_derivation(&db, "ccc").await?;
    db.insert_assignment(
        drv_id,
        &ExecutorId::from("worker-1"),
        1,
        uuid::Uuid::now_v7(),
        // §P0590 audit column: recorded when dispatch resolved a node
        // for the Mount-admission token.
        Some("ip-10-0-1-5.ec2.internal"),
    )
    .await?;

    db.update_assignment_status(drv_id, AssignmentStatus::Completed)
        .await?;

    let (status, completed_at, node_name): (String, Option<String>, Option<String>) =
        sqlx::query_as(
            "SELECT status, completed_at::text, node_name FROM assignments \
             WHERE derivation_id = $1",
        )
        .bind(drv_id)
        .fetch_one(&test_db.pool)
        .await?;
    assert_eq!(status, "completed");
    assert!(completed_at.is_some(), "terminal → completed_at set");
    assert_eq!(
        node_name.as_deref(),
        Some("ip-10-0-1-5.ec2.internal"),
        "M_069 audit column records the node the mountd token was scoped to"
    );
    Ok(())
}
