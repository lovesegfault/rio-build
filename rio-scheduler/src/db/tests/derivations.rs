//! Poison/retry/status transition tests.

use rio_test_support::TestDb;
use uuid::Uuid;

use super::{TERMINAL_STATUSES, insert_test_derivation};
use crate::db::SchedulerDb;
use crate::state::DrvHash;

// r[verify sched.poison.ttl-persist]
/// Roundtrip: persist_poisoned → load_poisoned_derivations → clear_poison.
/// Catches the `.as_bytes()` vs `.as_str()` binding regression — PG rejects
/// BYTEA against a TEXT column, but call sites swallow the error as best-effort.
///
/// Also verifies atomicity: a single `persist_poisoned` call sets BOTH
/// status AND poisoned_at (no crash window between two UPDATEs).
#[tokio::test]
async fn test_poison_persistence_roundtrip() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());
    let drv_hash: DrvHash = "poison-rt-hash".into();
    let _ = insert_test_derivation(&db, drv_hash.as_str()).await?;

    // Single atomic call: sets status='poisoned' AND poisoned_at=now()
    // AND assigned_builder_id=NULL. No separate status update needed.
    db.persist_poisoned(&drv_hash).await?;

    // Verify all three columns updated in one statement.
    let (status, has_ts, worker): (String, bool, Option<String>) = sqlx::query_as(
        "SELECT status, poisoned_at IS NOT NULL, assigned_builder_id \
         FROM derivations WHERE drv_hash=$1",
    )
    .bind(drv_hash.as_str())
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(status, "poisoned");
    assert!(has_ts, "poisoned_at must be set in the same statement");
    assert!(worker.is_none(), "assigned_builder_id must be NULLed");

    let rows = db.load_poisoned_derivations().await?;
    assert_eq!(rows.len(), 1, "persist_poisoned should make row loadable");
    assert_eq!(rows[0].drv_hash, drv_hash.as_str());
    assert_ne!(rows[0].derivation_id, Uuid::nil());
    assert!(
        rows[0].elapsed_secs >= 0.0 && rows[0].elapsed_secs < 5.0,
        "elapsed should be ~0s, got {}",
        rows[0].elapsed_secs
    );

    // clear_poison → no longer loadable; status reset to 'created'.
    db.clear_poison(&drv_hash).await?;
    let rows = db.load_poisoned_derivations().await?;
    assert!(
        rows.is_empty(),
        "clear_poison should remove from poisoned set"
    );

    let (status, poisoned_at): (String, Option<f64>) = sqlx::query_as(
        "SELECT status, EXTRACT(EPOCH FROM poisoned_at)::float8 \
         FROM derivations WHERE drv_hash=$1",
    )
    .bind(drv_hash.as_str())
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(status, "created");
    assert!(poisoned_at.is_none());
    Ok(())
}

// r[verify sched.db.clear-poison-batch]
/// `clear_poison_batch` clears N rows in one round-trip. Asserts both
/// the column-set parity with scalar `clear_poison` and the
/// rows_affected count (N hashes in → N rows touched, single statement).
#[tokio::test]
async fn test_clear_poison_batch() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // 100 poisoned rows.
    let hashes: Vec<DrvHash> = (0..100)
        .map(|i| format!("batch-poison-{i}").into())
        .collect();
    for h in &hashes {
        insert_test_derivation(&db, h.as_str()).await?;
        db.persist_poisoned(h).await?;
    }
    assert_eq!(db.load_poisoned_derivations().await?.len(), 100);

    // One call → 100 rows affected. The single-round-trip property is
    // structural (one `execute()`), so the assertion is on
    // `rows_affected`, not a query-count mock.
    let affected = db.clear_poison_batch(&hashes).await?;
    assert_eq!(affected, 100, "one ANY($1) UPDATE should touch all 100");

    // Same column set as scalar clear_poison: status='created',
    // poisoned_at NULL, failed_builders empty, retry_count 0.
    assert!(db.load_poisoned_derivations().await?.is_empty());
    let (n_created, n_clean): (i64, i64) = sqlx::query_as(
        "SELECT
             COUNT(*) FILTER (WHERE status = 'created'),
             COUNT(*) FILTER (WHERE poisoned_at IS NULL
                              AND failed_builders = '{}'
                              AND retry_count = 0)
         FROM derivations WHERE drv_hash = ANY($1)",
    )
    .bind(hashes.iter().map(DrvHash::as_str).collect::<Vec<_>>())
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(n_created, 100);
    assert_eq!(n_clean, 100);

    // Empty input: no-op, no PG round-trip.
    assert_eq!(db.clear_poison_batch(&[]).await?, 0);
    Ok(())
}

// r[verify sched.db.derivations-gc]
/// I-169.2: orphan-terminal rows are deleted; rows with a live
/// `build_derivations` link, an `assignments` row, or non-terminal
/// status are kept. LIMIT respected.
#[tokio::test]
async fn test_gc_orphan_terminal_derivations() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // (a) Orphan-terminal: one row per terminal status, no links → deleted.
    let mut orphan_ids = Vec::new();
    for status in TERMINAL_STATUSES {
        let h = format!("gc-orphan-{status}");
        let id = insert_test_derivation(&db, &h).await?;
        sqlx::query("UPDATE derivations SET status = $1 WHERE derivation_id = $2")
            .bind(*status)
            .bind(id)
            .execute(&test_db.pool)
            .await?;
        orphan_ids.push(id);
    }

    // (b) Terminal but linked via build_derivations → KEPT.
    let linked_id = insert_test_derivation(&db, "gc-linked").await?;
    sqlx::query("UPDATE derivations SET status = 'dependency_failed' WHERE derivation_id = $1")
        .bind(linked_id)
        .execute(&test_db.pool)
        .await?;
    let build_id = Uuid::new_v4();
    db.insert_build(
        build_id,
        None,
        crate::state::PriorityClass::Scheduled,
        false,
        &crate::state::BuildOptions::default(),
        None,
    )
    .await?;
    db.insert_build_derivation(build_id, linked_id).await?;

    // (c) Terminal but referenced by assignments → KEPT (FK is RESTRICT).
    let assigned_id = insert_test_derivation(&db, "gc-assigned").await?;
    sqlx::query("UPDATE derivations SET status = 'completed' WHERE derivation_id = $1")
        .bind(assigned_id)
        .execute(&test_db.pool)
        .await?;
    sqlx::query(
        "INSERT INTO assignments (derivation_id, builder_id, generation, status)
         VALUES ($1, 'w-test', 1, 'completed')",
    )
    .bind(assigned_id)
    .execute(&test_db.pool)
    .await?;

    // (d) Non-terminal, no links → KEPT.
    let live_id = insert_test_derivation(&db, "gc-live").await?;

    // Sweep with generous limit.
    let deleted = db.gc_orphan_terminal_derivations(1000).await?;
    assert_eq!(
        deleted,
        orphan_ids.len() as u64,
        "exactly the orphan-terminal set should be deleted"
    );

    let remaining: Vec<Uuid> = sqlx::query_scalar("SELECT derivation_id FROM derivations")
        .fetch_all(&test_db.pool)
        .await?;
    for id in &orphan_ids {
        assert!(!remaining.contains(id), "orphan {id} should be GC'd");
    }
    assert!(remaining.contains(&linked_id), "build-linked row kept");
    assert!(
        remaining.contains(&assigned_id),
        "assignment-linked row kept"
    );
    assert!(remaining.contains(&live_id), "non-terminal row kept");

    // Second sweep: nothing left to delete.
    assert_eq!(db.gc_orphan_terminal_derivations(1000).await?, 0);
    Ok(())
}

// r[verify sched.db.derivations-gc]
/// LIMIT batches the sweep: 5 orphans, limit=2 → 2, 2, 1, 0.
#[tokio::test]
async fn test_gc_orphan_terminal_derivations_limit() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    for i in 0..5 {
        let id = insert_test_derivation(&db, &format!("gc-lim-{i}")).await?;
        sqlx::query("UPDATE derivations SET status = 'dependency_failed' WHERE derivation_id = $1")
            .bind(id)
            .execute(&test_db.pool)
            .await?;
    }

    assert_eq!(db.gc_orphan_terminal_derivations(2).await?, 2);
    assert_eq!(db.gc_orphan_terminal_derivations(2).await?, 2);
    assert_eq!(db.gc_orphan_terminal_derivations(2).await?, 1);
    assert_eq!(db.gc_orphan_terminal_derivations(2).await?, 0);
    Ok(())
}
