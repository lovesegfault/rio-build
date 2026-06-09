//! `executor_confirm_fences` (migration 097) integration tests: the
//! confirm-exit fence rows — write-ahead insert idempotency, the
//! DeliverNew screen's read, and the housekeeping TTL rider's sweep.

use rio_test_support::TestDb;

use crate::db::SchedulerDb;

/// Fresh ephemeral PG + a SchedulerDb handle.
async fn setup() -> (TestDb, SchedulerDb) {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());
    (test_db, db)
}

/// Insert is idempotent (re-confirms upsert nothing), the read sees
/// the fence, an unrelated hash stays unfenced.
#[tokio::test]
async fn fence_insert_idempotent_and_read() -> anyhow::Result<()> {
    let (_pg, db) = setup().await;
    let hash = "a".repeat(64);

    assert!(!db.confirm_fence_exists(&hash).await?);
    db.insert_confirm_fence(&hash, "intent-1").await?;
    // The builder's confirm regime retries: the second insert is the
    // same license, not an error.
    db.insert_confirm_fence(&hash, "intent-1").await?;
    assert!(db.confirm_fence_exists(&hash).await?);
    assert!(!db.confirm_fence_exists(&"b".repeat(64)).await?);
    Ok(())
}

/// The TTL rider deletes only rows past the horizon: a zero horizon
/// sweeps the fence, a 24h horizon keeps a fresh one.
#[tokio::test]
async fn fence_gc_respects_horizon() -> anyhow::Result<()> {
    let (_pg, db) = setup().await;
    let hash = "c".repeat(64);
    db.insert_confirm_fence(&hash, "intent-gc").await?;

    let kept = db
        .gc_confirm_fences(crate::db::confirm_fences::CONFIRM_FENCE_GC_SECS, 100)
        .await?;
    assert_eq!(kept, 0, "a fresh fence survives the 24h horizon");
    assert!(db.confirm_fence_exists(&hash).await?);

    let swept = db.gc_confirm_fences(0.0, 100).await?;
    assert_eq!(swept, 1, "a zero horizon sweeps the fence");
    assert!(!db.confirm_fence_exists(&hash).await?);
    Ok(())
}
