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

/// Activating a build whose row doesn't exist must error (RowNotFound)
/// so the surrounding merge transaction aborts instead of committing
/// with the build never flipped to Active. Unreachable through the
/// single-threaded actor path today (the same command inserted the
/// row), but cheap defense against a silent half-committed merge.
#[tokio::test]
async fn test_activate_build_tx_missing_build_errors() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;

    let mut tx = test_db.pool.begin().await?;
    let res = SchedulerDb::activate_build_tx(&mut tx, Uuid::new_v4()).await;
    assert!(
        matches!(res, Err(sqlx::Error::RowNotFound)),
        "activating a nonexistent build must fail with RowNotFound, got {res:?}"
    );
    Ok(())
}

/// I-103: list_builds reads denormalized count columns directly — no
/// build_derivations/derivations join. persist_build_counts writes
/// them; the migration backfill seeds existing rows.
#[tokio::test]
async fn test_list_builds_denorm_counts_roundtrip() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let build_id = Uuid::new_v4();
    db.insert_build(
        build_id,
        None,
        crate::state::PriorityClass::Ci,
        true,
        &Default::default(),
        None,
    )
    .await?;

    // Initial: defaults are 0.
    let (_, rows) = db.list_builds(None, None, 10, 0).await?;
    let row = rows.iter().find(|r| r.build_id == build_id).unwrap();
    assert_eq!(row.total_derivations, 0);
    assert_eq!(row.completed_derivations, 0);
    assert_eq!(row.cached_derivations, 0);

    // Persist + re-read. No build_derivations rows exist — proves the
    // SELECT no longer joins (the old query would've returned 0 from
    // the COUNT regardless of these column values).
    db.persist_build_counts(build_id, 100, 50, 12).await?;
    let (_, rows) = db.list_builds(None, None, 10, 0).await?;
    let row = rows.iter().find(|r| r.build_id == build_id).unwrap();
    assert_eq!(row.total_derivations, 100);
    assert_eq!(row.completed_derivations, 50);
    assert_eq!(row.cached_derivations, 12);

    // Keyset variant reads the same columns.
    let rows = db
        .list_builds_keyset(None, None, 10, i64::MAX, Uuid::max())
        .await?;
    let row = rows.iter().find(|r| r.build_id == build_id).unwrap();
    assert_eq!(row.total_derivations, 100);
    assert_eq!(row.completed_derivations, 50);
    Ok(())
}

// r[verify sched.merge.displaced-failure-evidence]
/// `persist_build_error_summary_tx` records the first failure and never
/// overwrites an already-persisted summary (COALESCE semantics).
#[tokio::test]
async fn test_persist_build_error_summary_tx_first_failure_wins() -> anyhow::Result<()> {
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

    let read = || async {
        let (s,): (Option<String>,) =
            sqlx::query_as("SELECT error_summary FROM builds WHERE build_id = $1")
                .bind(build_id)
                .fetch_one(&test_db.pool)
                .await?;
        anyhow::Ok(s)
    };
    assert_eq!(read().await?, None);

    let mut conn = db.pool().acquire().await?;
    SchedulerDb::persist_build_error_summary_tx(&mut conn, build_id, "derivation a failed").await?;
    assert_eq!(read().await?.as_deref(), Some("derivation a failed"));

    // A later write does not displace the first failure.
    SchedulerDb::persist_build_error_summary_tx(&mut conn, build_id, "derivation b failed").await?;
    assert_eq!(
        read().await?.as_deref(),
        Some("derivation a failed"),
        "first persisted failure wins"
    );
    Ok(())
}
