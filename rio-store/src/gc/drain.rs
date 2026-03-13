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
/// stays (attempts >= MAX → excluded by the partial index) and shows
/// in `rio_store_s3_deletes_stuck` (not `_pending`; pending counts
/// only retriable rows). Usually: S3 permission change, key format
/// mismatch after a config change, or the object is Glacier-archived.
const MAX_ATTEMPTS: i32 = 10;

/// Interval between drain iterations. 30s: fast enough to keep
/// the pending table small under steady-state GC, slow enough to
/// not hammer S3 when there's nothing to do.
const DRAIN_INTERVAL: Duration = Duration::from_secs(30);

/// Run one drain iteration. Returns (deleted_count, failed_count).
///
/// For each pending row (up to DRAIN_BATCH_SIZE):
/// - Re-check `chunks.(deleted AND refcount=0)` — if the chunk was
///   resurrected by a PutPath since sweep enqueued it, skip S3
///   delete (just remove the pending row). Guards the sweep-vs-
///   PutPath TOCTOU for chunks shared across different paths.
/// - `ChunkBackend::delete_by_key` (S3 DeleteObject or fs rm)
/// - On success: DELETE the pending row
/// - On failure: UPDATE attempts = attempts + 1, last_error
///
/// Transactional: SELECT ... FOR UPDATE SKIP LOCKED grabs a batch
/// of rows and holds row-level locks until commit. Multiple store
/// replicas running drain concurrently each grab DISJOINT batches
/// (SKIP LOCKED). Without this, all replicas select the same 100
/// rows → duplicate S3 DeleteObject calls (idempotent but wasteful,
/// noisy logs).
///
/// S3 delete happens inside the tx → long-held row locks (~seconds
/// per S3 call). This is acceptable: locks are row-level, other
/// replicas skip locked rows and drain different ones concurrently.
/// If the tx rolls back (PG error mid-batch), S3 deletes that
/// already happened are lost from PG's view → next iteration re-
/// processes those rows → S3 delete of non-existent key is a no-op.
pub async fn drain_once(
    pool: &PgPool,
    backend: &Arc<dyn ChunkBackend>,
) -> Result<(u64, u64), sqlx::Error> {
    let mut tx = pool.begin().await?;

    // SELECT eligible rows. FOR UPDATE SKIP LOCKED: multi-replica
    // coordination. The partial index (WHERE attempts < 10) makes
    // this efficient even if permanently-failed rows accumulate.
    // ORDER BY enqueued_at: oldest first (roughly FIFO, though
    // retries can reorder).
    //
    // blake3_hash is nullable: pre-migration-006 rows have NULL
    // → drain proceeds unconditionally (old behavior). New rows
    // have the hash → re-check `chunks` before S3 delete.
    let rows: Vec<(i64, String, Option<Vec<u8>>)> = sqlx::query_as(
        r#"
        SELECT id, s3_key, blake3_hash FROM pending_s3_deletes
         WHERE attempts < $1
         ORDER BY enqueued_at
         LIMIT $2
           FOR UPDATE SKIP LOCKED
        "#,
    )
    .bind(MAX_ATTEMPTS)
    .bind(DRAIN_BATCH_SIZE)
    .fetch_all(&mut *tx)
    .await?;

    if rows.is_empty() {
        // Nothing to do — commit (no-op) to release.
        tx.commit().await?;
        return Ok((0, 0));
    }

    let mut deleted = 0u64;
    let mut failed = 0u64;

    for (id, key, blake3_hash) in rows {
        // Re-check: was this chunk resurrected since sweep enqueued
        // it? PutPath's ON CONFLICT sets deleted=false + refcount+1.
        // If so, the chunk is live again — skip S3, drop pending row.
        // NULL blake3_hash (pre-006 row) → skip re-check, proceed.
        //
        // Re-check runs inside the SKIP LOCKED tx: the pending row
        // is locked (no other replica touches it), but the CHUNKS
        // row is NOT locked by this tx — PutPath can still flip it
        // between this check and the S3 delete. That window is
        // small (one PG roundtrip + one S3 call).
        //
        // Known narrow race: if this re-check misses AND two
        // concurrent PutPaths BOTH see refcount≥2 (each saw the
        // other's increment) → both skip upload (cas.rs:243) →
        // permanent S3 hole. Requires: drain delete + 2 PutPaths
        // for SAME chunk in ~milliseconds. GetPath's BLAKE3
        // verify catches the hole (NotFound, not silent
        // corruption). TODO(phase4b): xmax-based inserted-check
        // in cas.rs upsert to close this (X18 drain race).
        if let Some(hash) = &blake3_hash {
            let still_dead: bool = sqlx::query_scalar(
                "SELECT (deleted AND refcount = 0) FROM chunks WHERE blake3_hash = $1",
            )
            .bind(hash)
            .fetch_optional(&mut *tx)
            .await?
            // Row gone entirely = still dead (nothing references it,
            // and the chunks row itself was deleted somehow — S3
            // delete is still safe).
            .unwrap_or(true);
            if !still_dead {
                sqlx::query("DELETE FROM pending_s3_deletes WHERE id = $1")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                metrics::counter!("rio_store_gc_chunk_resurrected_total").increment(1);
                debug!(id, key = %key, "drain: chunk resurrected, skipping S3 delete");
                continue;
            }
        }

        match backend.delete_by_key(&key).await {
            Ok(()) => {
                // DELETE the pending row (same tx). If tx commit
                // later fails, the S3 delete already happened —
                // next iteration re-processes this row, S3 delete
                // of non-existent key is a no-op.
                sqlx::query("DELETE FROM pending_s3_deletes WHERE id = $1")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                deleted += 1;
            }
            Err(e) => {
                // Increment attempts + record error (same tx).
                // Next iteration retries (if attempts < MAX).
                sqlx::query(
                    "UPDATE pending_s3_deletes \
                     SET attempts = attempts + 1, last_error = $2 \
                     WHERE id = $1",
                )
                .bind(id)
                .bind(e.to_string())
                .execute(&mut *tx)
                .await?;
                failed += 1;
                debug!(id, key = %key, error = %e, "drain: S3 delete failed (will retry)");
            }
        }
    }

    tx.commit().await?;

    if deleted > 0 || failed > 0 {
        debug!(deleted, failed, "drain iteration complete");
    }

    // Two gauges: _pending = retriable backlog (attempts < MAX),
    // _stuck = permanently-failed (attempts >= MAX, excluded from
    // drain). Operators alert on _stuck > 0 — those need manual
    // intervention (S3 perms, key format, Glacier). _pending > 0
    // with steady drain activity is normal.
    let (pending, stuck): (i64, i64) = sqlx::query_as(
        "SELECT \
           COUNT(*) FILTER (WHERE attempts < $1), \
           COUNT(*) FILTER (WHERE attempts >= $1) \
         FROM pending_s3_deletes",
    )
    .bind(MAX_ATTEMPTS)
    .fetch_one(pool)
    .await?;
    metrics::gauge!("rio_store_s3_deletes_pending").set(pending as f64);
    metrics::gauge!("rio_store_s3_deletes_stuck").set(stuck as f64);

    Ok((deleted, failed))
}

/// Spawn the periodic drain task. Runs `drain_once` every
/// DRAIN_INTERVAL. Exits cleanly when `shutdown` is cancelled.
pub fn spawn_drain_task(
    pool: PgPool,
    backend: Arc<dyn ChunkBackend>,
    shutdown: rio_common::signal::Token,
) -> tokio::task::JoinHandle<()> {
    rio_common::task::spawn_monitored("gc-drain-task", async move {
        let mut interval = tokio::time::interval(DRAIN_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    tracing::debug!("gc-drain-task shutting down");
                    break;
                }
                _ = interval.tick() => {
                    if let Err(e) = drain_once(&pool, &backend).await {
                        warn!(error = %e, "drain iteration failed (will retry)");
                    }
                }
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
    async fn drain_skips_resurrected_chunk() {
        // Sweep-vs-PutPath TOCTOU regression test: sweep marks chunk
        // X deleted + enqueues S3 delete; PutPath for a DIFFERENT
        // path (sharing X) resurrects it (refcount→1, deleted→false).
        // Drain must re-check and SKIP the S3 delete.
        let db = TestDb::new(&crate::MIGRATOR).await;
        let backend: Arc<dyn ChunkBackend> = Arc::new(MemoryChunkBackend::new());

        // Seed chunk X in backend + chunks table (resurrected state:
        // refcount=1, deleted=false — as PutPath's upsert would leave it).
        let hash_x = [0x11u8; 32];
        backend
            .put(&hash_x, bytes::Bytes::from_static(b"chunk-X-live-data"))
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO chunks (blake3_hash, refcount, size, deleted) \
             VALUES ($1, 1, 17, false)",
        )
        .bind(hash_x.as_slice())
        .execute(&db.pool)
        .await
        .unwrap();
        // Pending row (sweep enqueued this BEFORE PutPath resurrected).
        let key_x = backend.key_for(&hash_x);
        sqlx::query("INSERT INTO pending_s3_deletes (s3_key, blake3_hash) VALUES ($1, $2)")
            .bind(&key_x)
            .bind(hash_x.as_slice())
            .execute(&db.pool)
            .await
            .unwrap();

        // Also seed chunk Y — genuinely dead (refcount=0, deleted=true).
        // Proves the re-check doesn't accidentally skip REAL deletes.
        let hash_y = [0x22u8; 32];
        backend
            .put(&hash_y, bytes::Bytes::from_static(b"chunk-Y-dead-data"))
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO chunks (blake3_hash, refcount, size, deleted) \
             VALUES ($1, 0, 17, true)",
        )
        .bind(hash_y.as_slice())
        .execute(&db.pool)
        .await
        .unwrap();
        let key_y = backend.key_for(&hash_y);
        sqlx::query("INSERT INTO pending_s3_deletes (s3_key, blake3_hash) VALUES ($1, $2)")
            .bind(&key_y)
            .bind(hash_y.as_slice())
            .execute(&db.pool)
            .await
            .unwrap();

        // Drain.
        let (deleted, failed) = drain_once(&db.pool, &backend).await.unwrap();
        assert_eq!(deleted, 1, "only Y S3-deleted; X resurrected → skipped");
        assert_eq!(failed, 0);

        // X still in backend (NOT deleted — resurrection detected).
        assert!(
            backend.get(&hash_x).await.unwrap().is_some(),
            "resurrected chunk X preserved in backend"
        );
        // Y gone from backend (genuine delete went through).
        assert!(
            backend.get(&hash_y).await.unwrap().is_none(),
            "dead chunk Y deleted from backend"
        );
        // Both pending rows gone (X: removed by re-check; Y: normal).
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM pending_s3_deletes")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(count.0, 0, "both pending rows removed");
    }

    #[tokio::test]
    async fn drain_proceeds_on_null_blake3_hash() {
        // Pre-migration-006 rows have NULL blake3_hash → no re-check,
        // proceed unconditionally (old behavior).
        let db = TestDb::new(&crate::MIGRATOR).await;
        let backend: Arc<dyn ChunkBackend> = Arc::new(MemoryChunkBackend::new());

        let hash = [0x33u8; 32];
        backend
            .put(&hash, bytes::Bytes::from_static(b"legacy-chunk"))
            .await
            .unwrap();
        let key = backend.key_for(&hash);
        // blake3_hash NOT set (NULL) — simulates pre-006 row.
        sqlx::query("INSERT INTO pending_s3_deletes (s3_key) VALUES ($1)")
            .bind(&key)
            .execute(&db.pool)
            .await
            .unwrap();

        let (deleted, failed) = drain_once(&db.pool, &backend).await.unwrap();
        assert_eq!(deleted, 1, "NULL blake3_hash → drain proceeds");
        assert_eq!(failed, 0);
        assert!(
            backend.get(&hash).await.unwrap().is_none(),
            "S3 delete went through"
        );
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

    /// SKIP LOCKED: two concurrent drain_once calls against the same
    /// pool must grab DISJOINT batches. With 5 pending rows, total
    /// S3 deletes should be exactly 5 (not 10).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drain_skip_locked_disjoint_batches() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let backend: Arc<dyn ChunkBackend> = Arc::new(MemoryChunkBackend::new());

        // Seed 5 chunks + pending rows.
        for i in 0..5u8 {
            let hash = [i; 32];
            backend
                .put(&hash, bytes::Bytes::from(vec![i; 8]))
                .await
                .unwrap();
            let key = backend.key_for(&hash);
            sqlx::query("INSERT INTO pending_s3_deletes (s3_key) VALUES ($1)")
                .bind(&key)
                .execute(&db.pool)
                .await
                .unwrap();
        }

        // Run two drains concurrently. Each holds a FOR UPDATE
        // SKIP LOCKED tx — one grabs some rows, the other skips
        // those and grabs the rest. Total deleted = 5.
        //
        // Note: with MemoryChunkBackend, S3 delete is instant, so
        // the lock window is tiny. Both might serialize (one gets
        // all 5, other gets 0). Either way, total == 5 proves no
        // DUPLICATE processing — that's the invariant.
        let pool_a = db.pool.clone();
        let pool_b = db.pool.clone();
        let backend_a = Arc::clone(&backend);
        let backend_b = Arc::clone(&backend);

        let (a, b) = tokio::join!(
            drain_once(&pool_a, &backend_a),
            drain_once(&pool_b, &backend_b),
        );
        let (del_a, fail_a) = a.unwrap();
        let (del_b, fail_b) = b.unwrap();

        assert_eq!(fail_a + fail_b, 0, "no failures");
        assert_eq!(
            del_a + del_b,
            5,
            "total deletes = 5 (no duplicates); split was {del_a}/{del_b}"
        );

        // All pending rows gone.
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM pending_s3_deletes")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(count.0, 0, "all pending rows removed exactly once");
    }
}
