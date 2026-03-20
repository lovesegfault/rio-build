//! Poison/retry/status transition tests.

use rio_test_support::TestDb;
use uuid::Uuid;

use super::insert_test_derivation;
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
    // AND assigned_worker_id=NULL. No separate status update needed.
    db.persist_poisoned(&drv_hash).await?;

    // Verify all three columns updated in one statement.
    let (status, has_ts, worker): (String, bool, Option<String>) = sqlx::query_as(
        "SELECT status, poisoned_at IS NOT NULL, assigned_worker_id \
         FROM derivations WHERE drv_hash=$1",
    )
    .bind(drv_hash.as_str())
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(status, "poisoned");
    assert!(has_ts, "poisoned_at must be set in the same statement");
    assert!(worker.is_none(), "assigned_worker_id must be NULLed");

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
