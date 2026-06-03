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
// r[verify sched.poison.clear-failure-evidence]
// r[verify sched.build.failure-evidence-at-source+1]
/// `persist_build_error_summary_tx` records the first-failure PAIR
/// (M_072) and never overwrites either persisted half (COALESCE on
/// both columns); a NULL hash bind is a no-op so a backstop that only
/// knows a reconstructed summary cannot blank an earlier at-source
/// pair. The pool-level wrapper goes through the same statement.
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
        let (s, f): (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT error_summary, failed_derivation FROM builds WHERE build_id = $1",
        )
        .bind(build_id)
        .fetch_one(&test_db.pool)
        .await?;
        anyhow::Ok((s, f))
    };
    assert_eq!(read().await?, (None, None));

    let mut conn = db.pool().acquire().await?;
    // Backstop-shaped write first: summary only, no hash — must not
    // plant an empty hash that would block the later at-source pair.
    SchedulerDb::persist_build_error_summary_tx(&mut conn, build_id, "derivation a failed", None)
        .await?;
    let (s, f) = read().await?;
    assert_eq!(s.as_deref(), Some("derivation a failed"));
    assert_eq!(f, None, "NULL hash bind is a no-op, not an empty write");

    // The at-source pair lands the hash; the summary half keeps the
    // first write.
    SchedulerDb::persist_build_error_summary_tx(
        &mut conn,
        build_id,
        "derivation b failed",
        Some("drv-a"),
    )
    .await?;
    let (s, f) = read().await?;
    assert_eq!(
        s.as_deref(),
        Some("derivation a failed"),
        "first persisted summary wins"
    );
    assert_eq!(
        f.as_deref(),
        Some("drv-a"),
        "hash half fills in via COALESCE"
    );

    // Neither half is displaced by later writes.
    SchedulerDb::persist_build_error_summary_tx(
        &mut conn,
        build_id,
        "derivation c failed",
        Some("drv-c"),
    )
    .await?;
    let (s, f) = read().await?;
    assert_eq!(s.as_deref(), Some("derivation a failed"));
    assert_eq!(f.as_deref(), Some("drv-a"), "first persisted hash wins");

    // The pool-level wrapper (poison-clear paths) hits the same COALESCE.
    drop(conn);
    db.persist_build_error_summary(build_id, "derivation d failed", Some("drv-d"))
        .await?;
    let (s, f) = read().await?;
    assert_eq!(
        s.as_deref(),
        Some("derivation a failed"),
        "pool-level wrapper preserves first-write-wins"
    );
    assert_eq!(f.as_deref(), Some("drv-a"));
    Ok(())
}

/// Round-17 bug_043: the terminal arm of `update_build_status_tx` is the
/// FOURTH writer tier of the M_072 pair and must converge with the
/// chokepoint on every ordering — a terminal transition (Failed after a
/// recorded failure, Cancelled mid-failure, or a timeout's reconstructed
/// reason) can neither displace nor blank the sticky at-source pair, and
/// a terminal write on an evidence-free build still lands its summary
/// (the backstop role the plain bind used to serve).
#[tokio::test]
async fn test_terminal_status_write_never_displaces_failure_evidence() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let read = |build_id: Uuid| {
        let pool = test_db.pool.clone();
        async move {
            let (s, f): (Option<String>, Option<String>) = sqlx::query_as(
                "SELECT error_summary, failed_derivation FROM builds WHERE build_id = $1",
            )
            .bind(build_id)
            .fetch_one(&pool)
            .await?;
            anyhow::Ok((s, f))
        }
    };

    // Case 1: at-source pair persisted, then a timeout-shaped terminal
    // Failed write with a LATER summary — pair untouched.
    let b1 = Uuid::new_v4();
    db.insert_build(
        b1,
        None,
        crate::state::PriorityClass::Scheduled,
        true,
        &Default::default(),
        None,
    )
    .await?;
    db.persist_build_error_summary(b1, "derivation a failed", Some("drv-a"))
        .await?;
    db.update_build_status(b1, BuildState::Failed, Some("build_timeout 60s exceeded"))
        .await?;
    assert_eq!(
        read(b1).await?,
        (Some("derivation a failed".into()), Some("drv-a".into())),
        "terminal Failed write must not displace the at-source pair"
    );

    // Case 2: at-source pair persisted, then Cancelled with NO summary —
    // the old plain bind blanked the evidence here; COALESCE keeps it.
    let b2 = Uuid::new_v4();
    db.insert_build(
        b2,
        None,
        crate::state::PriorityClass::Scheduled,
        true,
        &Default::default(),
        None,
    )
    .await?;
    db.persist_build_error_summary(b2, "derivation b failed", Some("drv-b"))
        .await?;
    db.update_build_status(b2, BuildState::Cancelled, None)
        .await?;
    assert_eq!(
        read(b2).await?,
        (Some("derivation b failed".into()), Some("drv-b".into())),
        "cancel-after-failure must not blank the evidence pair"
    );

    // Case 3: evidence-free build, terminal Failed with a summary — the
    // backstop role still lands it (pair half stays NULL, never "").
    let b3 = Uuid::new_v4();
    db.insert_build(
        b3,
        None,
        crate::state::PriorityClass::Scheduled,
        true,
        &Default::default(),
        None,
    )
    .await?;
    db.update_build_status(b3, BuildState::Failed, Some("build_timeout 60s exceeded"))
        .await?;
    assert_eq!(
        read(b3).await?,
        (Some("build_timeout 60s exceeded".into()), None),
        "terminal write on an evidence-free build still provides the summary backstop"
    );

    Ok(())
}
