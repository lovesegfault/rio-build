//! Build insert/delete/status transition tests.

use rio_test_support::TestDb;
use uuid::Uuid;

use crate::db::SchedulerDb;
use crate::state::BuildState;

/// BuildState::Pending has now_col="" → no timestamp column touched.
#[tokio::test]
async fn test_update_build_status_pending_no_timestamps() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let build_id = Uuid::new_v4();
    db.insert_build(
        build_id,
        None,
        crate::state::PriorityClass::Scheduled,
        true,
        &Default::default(),
        None,
    )
    .await?;

    // Insert starts at 'pending'. Transition to Active then back to Pending
    // (unusual but valid at the DB layer — the state machine rejects it,
    // but this is testing the raw SQL branch).
    db.update_build_status(build_id, BuildState::Active, None)
        .await?;
    db.update_build_status(build_id, BuildState::Pending, None)
        .await?;

    // Query timestamp as Option<String> via text cast to avoid adding a
    // chrono dep just for test assertions.
    let (status, finished_at): (String, Option<String>) =
        sqlx::query_as("SELECT status, finished_at::text FROM builds WHERE build_id = $1")
            .bind(build_id)
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(status, "pending");
    // Pending transition does NOT set finished_at (the now_col="" branch).
    assert!(finished_at.is_none());
    Ok(())
}
