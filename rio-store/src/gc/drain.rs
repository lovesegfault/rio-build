//! Drain task: consume `pending_s3_deletes`, call `ChunkBackend::delete_by_key`.
// r[impl store.gc.pending-deletes]

use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use tracing::{debug, warn};

use crate::backend::chunk::ChunkBackend;

/// Batch size: how many pending rows to process per drain iteration.
/// Small enough that a slow S3 DeleteObject for one doesn't block
/// 1000 others; large enough to amortize the SELECT.
const DRAIN_BATCH_SIZE: i64 = 100;

/// Max attempts before we stop retrying a row. After this, the row
/// stays (attempts >= MAX → excluded by the partial index) and the
/// `rio_store_s3_deletes_pending` gauge stays non-zero — operator
/// investigates. Usually: S3 permission change, key format mismatch
/// after a config change, or the object is Glacier-archived.
const MAX_ATTEMPTS: i32 = 10;

/// Interval between drain iterations. 30s: fast enough to keep
/// the pending table small under steady-state GC, slow enough to
/// not hammer S3 when there's nothing to do.
const DRAIN_INTERVAL: Duration = Duration::from_secs(30);

/// Run one drain iteration. Returns (deleted_count, failed_count).
///
/// For each pending row (up to DRAIN_BATCH_SIZE):
/// - `ChunkBackend::delete_by_key` (S3 DeleteObject or fs rm)
/// - On success: DELETE the pending row
/// - On failure: UPDATE attempts = attempts + 1, last_error
///
/// NOT transactional across the batch: each row is committed
/// individually. A mid-batch S3 outage means we process some,
/// fail some, and retry the failed ones next iteration. The PG
/// DELETE/UPDATE per-row means a crash mid-batch doesn't re-
/// process already-deleted rows.
pub async fn drain_once(
    pool: &PgPool,
    backend: &Arc<dyn ChunkBackend>,
) -> Result<(u64, u64), sqlx::Error> {
    // SELECT eligible rows. The partial index (WHERE attempts < 10)
    // makes this efficient even if permanently-failed rows
    // accumulate. ORDER BY enqueued_at: oldest first (roughly
    // FIFO, though retries can reorder).
    let rows: Vec<(i64, String)> = sqlx::query_as(
        r#"
        SELECT id, s3_key FROM pending_s3_deletes
         WHERE attempts < $1
         ORDER BY enqueued_at
         LIMIT $2
        "#,
    )
    .bind(MAX_ATTEMPTS)
    .bind(DRAIN_BATCH_SIZE)
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok((0, 0));
    }

    let mut deleted = 0u64;
    let mut failed = 0u64;

    for (id, key) in rows {
        match backend.delete_by_key(&key).await {
            Ok(()) => {
                // DELETE the pending row. Fire-and-forget: if
                // PG is down here (S3 succeeded, PG failed), the
                // row stays and we'll retry the S3 delete next
                // iteration — idempotent (S3 delete of non-
                // existent key is a no-op).
                if let Err(e) = sqlx::query("DELETE FROM pending_s3_deletes WHERE id = $1")
                    .bind(id)
                    .execute(pool)
                    .await
                {
                    warn!(id, error = %e, "drain: S3 delete ok but PG row delete failed (will retry S3, harmless)");
                }
                deleted += 1;
            }
            Err(e) => {
                // Increment attempts + record error. Next iteration
                // retries (if attempts < MAX). After MAX: row stays,
                // excluded by partial index, operator investigates.
                if let Err(pg_err) = sqlx::query(
                    "UPDATE pending_s3_deletes \
                     SET attempts = attempts + 1, last_error = $2 \
                     WHERE id = $1",
                )
                .bind(id)
                .bind(e.to_string())
                .execute(pool)
                .await
                {
                    // Both S3 and PG failing — double whammy.
                    // Log and move on; next iteration will pick
                    // up the row again (attempts unchanged).
                    warn!(id, s3_error = %e, pg_error = %pg_err, "drain: S3 delete failed AND PG update failed");
                }
                failed += 1;
                debug!(id, key = %key, error = %e, "drain: S3 delete failed (will retry)");
            }
        }
    }

    if deleted > 0 || failed > 0 {
        debug!(deleted, failed, "drain iteration complete");
    }

    // Update the pending gauge. COUNT(*) of eligible rows —
    // operators track this; non-zero after several intervals
    // with failed=0 means permanently-failed rows need attention.
    let pending: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM pending_s3_deletes WHERE attempts < $1")
            .bind(MAX_ATTEMPTS)
            .fetch_one(pool)
            .await?;
    metrics::gauge!("rio_store_s3_deletes_pending").set(pending.0 as f64);

    Ok((deleted, failed))
}

/// Spawn the periodic drain task. Runs `drain_once` every
/// DRAIN_INTERVAL.
pub fn spawn_drain_task(
    pool: PgPool,
    backend: Arc<dyn ChunkBackend>,
) -> tokio::task::JoinHandle<()> {
    rio_common::task::spawn_monitored("gc-drain-task", async move {
        let mut interval = tokio::time::interval(DRAIN_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if let Err(e) = drain_once(&pool, &backend).await {
                warn!(error = %e, "drain iteration failed (will retry)");
            }
        }
    })
}

// r[verify store.gc.pending-deletes]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::chunk::MemoryChunkBackend;
    use rio_test_support::TestDb;

    #[tokio::test]
    async fn drain_deletes_and_removes_row() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let backend: Arc<dyn ChunkBackend> = Arc::new(MemoryChunkBackend::new());

        // Seed a chunk + pending row. MemoryBackend's key_for is
        // hex; insert the row with that key.
        let hash = [0x42u8; 32];
        backend
            .put(&hash, bytes::Bytes::from_static(b"test-chunk-data"))
            .await
            .unwrap();
        let key = backend.key_for(&hash);
        sqlx::query("INSERT INTO pending_s3_deletes (s3_key) VALUES ($1)")
            .bind(&key)
            .execute(&db.pool)
            .await
            .unwrap();

        // Drain: should delete the chunk from backend + remove
        // the pending row.
        let (deleted, failed) = drain_once(&db.pool, &backend).await.unwrap();
        assert_eq!(deleted, 1, "one row drained");
        assert_eq!(failed, 0);

        // Backend: chunk gone.
        assert!(
            backend.get(&hash).await.unwrap().is_none(),
            "backend delete_by_key removed the chunk"
        );
        // PG: row gone.
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM pending_s3_deletes")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(count.0, 0, "pending row deleted");
    }

    #[tokio::test]
    async fn drain_increments_attempts_on_failure() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let backend: Arc<dyn ChunkBackend> = Arc::new(MemoryChunkBackend::new());

        // Seed a row with a key that CAN'T be deleted (not valid
        // hex → delete_by_key Errs).
        sqlx::query("INSERT INTO pending_s3_deletes (s3_key) VALUES ('not-valid-hex!')")
            .execute(&db.pool)
            .await
            .unwrap();

        let (deleted, failed) = drain_once(&db.pool, &backend).await.unwrap();
        assert_eq!(deleted, 0);
        assert_eq!(failed, 1, "invalid key → delete fails");

        // Row still there, attempts=1, last_error set.
        let (attempts, last_error): (i32, Option<String>) =
            sqlx::query_as("SELECT attempts, last_error FROM pending_s3_deletes LIMIT 1")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(attempts, 1, "attempts incremented");
        assert!(last_error.is_some(), "last_error recorded");
    }

    #[tokio::test]
    async fn drain_respects_max_attempts() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let backend: Arc<dyn ChunkBackend> = Arc::new(MemoryChunkBackend::new());

        // Seed a row at max attempts — drain should SKIP it.
        sqlx::query("INSERT INTO pending_s3_deletes (s3_key, attempts) VALUES ('stuck-key', $1)")
            .bind(MAX_ATTEMPTS)
            .execute(&db.pool)
            .await
            .unwrap();

        let (deleted, failed) = drain_once(&db.pool, &backend).await.unwrap();
        // Both 0: the row is excluded by WHERE attempts < MAX.
        // Operator investigates; this row is effectively "parked."
        assert_eq!(deleted, 0);
        assert_eq!(failed, 0, "max-attempts row excluded from drain");
    }
}
