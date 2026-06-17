//! Drain task: consume `pending_s3_deletes`, call `ChunkBackend::delete_by_keys`.
// r[impl store.gc.pending-deletes]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use tracing::{debug, warn};

use crate::backend::ChunkBackend;

/// Rows per drain batch: one transaction, one backend
/// `delete_by_keys` call. Matches the S3 `DeleteObjects` per-request
/// key limit, so a full batch is exactly one S3 round trip.
const DRAIN_BATCH_SIZE: i64 = 1000;

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

/// Max batches per tick. The per-tick budget scales with the backlog:
/// the loop keeps pulling batches until one comes back short (backlog
/// drained) or this cap is hit. 10 × 1000 keys per 30s tick ≈ 333
/// deletes/s per replica — comfortably above any measured GC enqueue
/// rate (the sequential per-object DeleteObject drain topped out at
/// 3.3/s and grew an unbounded backlog past ~12k enqueues/h; one GC
/// run left a 2.6h backlog) — while still bounding a single tick's S3
/// call volume and transaction count.
const MAX_BATCHES_PER_TICK: usize = 10;

/// Run one drain iteration. Returns (deleted_count, failed_count).
///
/// Pulls up to `MAX_BATCHES_PER_TICK` batches of
/// `DRAIN_BATCH_SIZE` rows. For each batch (one transaction):
/// - Re-check `chunks.(deleted AND refcount=0)` — if a chunk was
///   resurrected by a PutPath since sweep enqueued it, skip its S3
///   delete (just remove the pending row). Guards the sweep-vs-
///   PutPath TOCTOU for chunks shared across different paths.
/// - ONE `ChunkBackend::delete_by_keys` call (S3 `DeleteObjects`,
///   up to 1000 keys per round trip; fs/memory loop per key)
/// - Keys the batch response reports as failed: UPDATE attempts =
///   attempts + 1, last_error — the row stays tombstoned for retry,
///   never silently dropped. Succeeded keys: DELETE the pending row.
///
/// Transactional: SELECT ... FOR UPDATE SKIP LOCKED grabs a batch
/// of rows and holds row-level locks until commit. Multiple store
/// replicas running drain concurrently each grab DISJOINT batches
/// (SKIP LOCKED). Without this, all replicas select the same rows
/// → duplicate S3 delete calls (idempotent but wasteful, noisy logs).
///
/// Chunk-lock scope (bug_189): a `chunks` FOR UPDATE row lock spans
/// ONE backend round trip — the batched `delete_by_keys` call — and
/// releases at batch commit. The per-row predecessor held each lock
/// for one `DeleteObject` RTT; the pre-bug_189 batch held the first
/// chunk's lock across up to 100 sequential RTTs (~10s steady-state,
/// minutes under S3 degradation), blocking any concurrent PutPath
/// `INSERT INTO chunks ON CONFLICT DO UPDATE` that shared a chunk
/// (PutPath cannot SKIP LOCKED). One batched RTT keeps the lock
/// window per-call-sized while restoring batch throughput.
pub async fn drain_once(
    pool: &PgPool,
    backend: &Arc<dyn ChunkBackend>,
) -> Result<(u64, u64), sqlx::Error> {
    let mut deleted = 0u64;
    let mut failed = 0u64;
    // Exclude rows that already failed this call: their attempts++
    // committed, but they still match `attempts < MAX` → the next
    // batch's SELECT would re-grab and re-attempt them up to MAX
    // times in one tick.
    let mut seen: Vec<i64> = Vec::new();

    for _ in 0..MAX_BATCHES_PER_TICK {
        let Some(batch) = drain_one_batch(pool, backend, &seen).await? else {
            break; // no eligible rows
        };
        deleted += batch.deleted;
        failed += batch.failed;
        seen.extend(batch.failed_ids);
        if batch.resurrected > 0 {
            // Post-commit (drain_one_batch already committed). A
            // counter is a promise of monotonic fact — never fire
            // before the resurrection-skip is durable.
            metrics::counter!("rio_store_gc_chunk_resurrected_total").increment(batch.resurrected);
        }
        if batch.auth_error {
            break;
        }
        if batch.rows_selected < DRAIN_BATCH_SIZE as usize {
            break; // short batch: backlog drained, don't re-SELECT
        }
    }

    if deleted > 0 || failed > 0 {
        debug!(deleted, failed, "drain iteration complete");
    }

    // Two gauges: _pending = retriable backlog (attempts < MAX),
    // _stuck = permanently-failed (attempts >= MAX, excluded from
    // drain). Operators alert on _stuck > 0 — those need manual
    // intervention (S3 perms, key format, Glacier). _pending > 0
    // with steady drain activity is normal.
    //
    // Refreshed on every successful tick (including empty-SELECT
    // ticks — the for-loop above no-ops on empty rows). PG-error
    // ticks (`?`-return above) skip; next 30s tick recovers.
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

/// Outcome of one [`drain_one_batch`] transaction.
struct BatchOutcome {
    /// Rows the SELECT grabbed (before any skip/failure split). A
    /// short batch (< `DRAIN_BATCH_SIZE`) tells the caller the
    /// backlog is drained.
    rows_selected: usize,
    /// Backend deletes confirmed; pending rows removed.
    deleted: u64,
    /// Backend deletes failed; `attempts += 1`, rows retained.
    failed: u64,
    /// Chunks resurrected (PutPath bumped refcount since sweep
    /// enqueued); S3 skipped; pending rows removed.
    resurrected: u64,
    /// Ids of the failed rows — the caller excludes them from later
    /// batches this tick.
    failed_ids: Vec<i64>,
    /// `BackendAuthError` — permanent (IRSA misconfigured, IAM
    /// missing s3:DeleteObject). Attempts NOT incremented; caller
    /// breaks the iteration so we don't burn through retry budget at
    /// debug! level with no operator signal.
    auth_error: bool,
}

/// One drain-batch transaction: SELECT up to `DRAIN_BATCH_SIZE`
/// pending rows FOR UPDATE SKIP LOCKED, re-check
/// `chunks.(deleted AND refcount=0)` FOR UPDATE (sorted batch), ONE
/// `delete_by_keys` backend call, commit. Returns `None` if no
/// eligible rows.
async fn drain_one_batch(
    pool: &PgPool,
    backend: &Arc<dyn ChunkBackend>,
    skip_ids: &[i64],
) -> Result<Option<BatchOutcome>, sqlx::Error> {
    let mut tx = pool.begin().await?;

    // SELECT eligible rows. FOR UPDATE SKIP LOCKED: multi-replica
    // coordination. The partial index (WHERE attempts < 10) makes
    // this efficient even if permanently-failed rows accumulate.
    // ORDER BY enqueued_at: oldest first (roughly FIFO, though
    // retries can reorder). `id <> ALL($2)` skips rows that already
    // failed earlier in this drain_once call.
    //
    // blake3_hash is nullable: pre-migration-006 rows have NULL
    // → drain proceeds unconditionally (old behavior). New rows
    // have the hash → re-check `chunks` before S3 delete.
    let rows: Vec<(i64, String, Option<Vec<u8>>)> = sqlx::query_as(
        r#"
        SELECT id, s3_key, blake3_hash FROM pending_s3_deletes
         WHERE attempts < $1
           AND id <> ALL($2::bigint[])
         ORDER BY enqueued_at
         LIMIT $3
           FOR UPDATE SKIP LOCKED
        "#,
    )
    .bind(MAX_ATTEMPTS)
    .bind(skip_ids)
    .bind(DRAIN_BATCH_SIZE)
    .fetch_all(&mut *tx)
    .await?;
    if rows.is_empty() {
        tx.rollback().await?;
        return Ok(None);
    }

    // Re-check: were any of these chunks resurrected since sweep
    // enqueued them? PutPath's ON CONFLICT sets deleted=false +
    // refcount+1. If so, the chunk is live again — skip S3, drop the
    // pending row. NULL blake3_hash (pre-006 row) → no re-check.
    //
    // FOR UPDATE serializes this re-check with concurrent PutPath
    // upserts — the upsert's ON CONFLICT row lock blocks until this
    // tx commits or rolls back, so a resurrection-between-check-and-
    // S3-delete is impossible. Without FOR UPDATE, PutPath could
    // flip a chunk live between this SELECT and the batch delete
    // below: PG would say refcount≥1, S3 would no longer have the
    // object. Post-M033 a resurrecting PutPath re-uploads
    // (uploaded_at was cleared by sweep), so the data-loss exposure
    // is gone, but the FOR UPDATE still saves a wasted upload
    // round-trip and keeps the resurrection-between-sweep-and-drain
    // guard exact.
    //
    // Sorted + ORDER BY FOR UPDATE: the r[store.chunk.lock-order]
    // discipline every multi-row `chunks` locker follows — without
    // it this batch lock would ABBA against concurrent sorted
    // writers (PutPath upserts, sweep decrements).
    // r[impl store.gc.pending-deletes]
    let mut hashes: Vec<Vec<u8>> = rows.iter().filter_map(|(_, _, h)| h.clone()).collect();
    hashes.sort_unstable();
    hashes.dedup();
    let still_dead: HashMap<Vec<u8>, bool> = if hashes.is_empty() {
        HashMap::new()
    } else {
        sqlx::query_as(
            "SELECT blake3_hash, (deleted AND refcount = 0) FROM chunks \
              WHERE blake3_hash = ANY($1) \
              ORDER BY blake3_hash \
                FOR UPDATE",
        )
        .bind(&hashes)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .collect()
    };

    let mut resurrected_ids: Vec<i64> = Vec::new();
    let mut to_delete: Vec<(i64, String)> = Vec::new();
    for (id, key, hash) in rows.iter() {
        // Chunks row gone entirely = still dead (nothing references
        // it, and the chunks row itself was deleted somehow — S3
        // delete is still safe). Same for NULL blake3_hash.
        let live = hash
            .as_ref()
            .is_some_and(|h| !still_dead.get(h).copied().unwrap_or(true));
        if live {
            debug!(id, key = %key, "drain: chunk resurrected, skipping S3 delete");
            resurrected_ids.push(*id);
        } else {
            to_delete.push((*id, key.clone()));
        }
    }
    if !resurrected_ids.is_empty() {
        sqlx::query("DELETE FROM pending_s3_deletes WHERE id = ANY($1::bigint[])")
            .bind(&resurrected_ids)
            .execute(&mut *tx)
            .await?;
    }

    let mut outcome = BatchOutcome {
        rows_selected: rows.len(),
        deleted: 0,
        failed: 0,
        resurrected: resurrected_ids.len() as u64,
        failed_ids: Vec::new(),
        auth_error: false,
    };
    if to_delete.is_empty() {
        tx.commit().await?;
        return Ok(Some(outcome));
    }

    let keys: Vec<String> = to_delete.iter().map(|(_, k)| k.clone()).collect();
    match backend.delete_by_keys(&keys).await {
        Ok(failures) => {
            // Per-key failures: the row stays tombstoned (attempts+1,
            // last_error) and retries next tick — a failed key is
            // never silently dropped. Everything else: DELETE the
            // pending rows (same tx). If the commit later fails, the
            // backend deletes already happened — the next iteration
            // re-processes the rows; delete of a non-existent key is
            // a no-op.
            let failed_by_key: HashMap<&str, &str> = failures
                .iter()
                .map(|f| (f.key.as_str(), f.error.as_str()))
                .collect();
            let mut ok_ids: Vec<i64> = Vec::new();
            for (id, key) in &to_delete {
                if let Some(err) = failed_by_key.get(key.as_str()) {
                    sqlx::query(
                        "UPDATE pending_s3_deletes \
                         SET attempts = attempts + 1, last_error = $2 \
                         WHERE id = $1",
                    )
                    .bind(id)
                    .bind(err)
                    .execute(&mut *tx)
                    .await?;
                    debug!(id, key = %key, error = %err, "drain: S3 delete failed (will retry)");
                    outcome.failed += 1;
                    outcome.failed_ids.push(*id);
                } else {
                    ok_ids.push(*id);
                }
            }
            if !ok_ids.is_empty() {
                sqlx::query("DELETE FROM pending_s3_deletes WHERE id = ANY($1::bigint[])")
                    .bind(&ok_ids)
                    .execute(&mut *tx)
                    .await?;
                outcome.deleted = ok_ids.len() as u64;
            }
        }
        // Permanent auth (IRSA misconfigured, IAM missing
        // s3:DeleteObject): bumping attempts on every row chews
        // through the retry budget at debug! level with no operator
        // signal. Emit error! (alert-worthy), DON'T burn attempts;
        // caller breaks the iteration. The commit below still
        // persists the resurrection-skip row deletes.
        Err(e)
            if e.downcast_ref::<crate::backend::BackendAuthError>()
                .is_some() =>
        {
            tracing::error!(
                error = %e,
                "drain: storage backend authentication failed; \
                 check S3 credentials/IAM permissions"
            );
            outcome.auth_error = true;
        }
        // Whole-call transport failure: nothing is known per key, so
        // every attempted row gets attempts+1 — the _stuck alert can
        // eventually fire if this never recovers.
        Err(e) => {
            let ids: Vec<i64> = to_delete.iter().map(|(id, _)| *id).collect();
            sqlx::query(
                "UPDATE pending_s3_deletes \
                 SET attempts = attempts + 1, last_error = $2 \
                 WHERE id = ANY($1::bigint[])",
            )
            .bind(&ids)
            .bind(e.to_string())
            .execute(&mut *tx)
            .await?;
            debug!(error = %e, count = ids.len(), "drain: batch delete failed (will retry)");
            outcome.failed = ids.len() as u64;
            outcome.failed_ids = ids;
        }
    }

    tx.commit().await?;
    Ok(Some(outcome))
}

/// Spawn the periodic drain task. Runs `drain_once` every
/// DRAIN_INTERVAL. Exits cleanly when `shutdown` is cancelled.
pub fn spawn_drain_task(
    pool: PgPool,
    backend: Arc<dyn ChunkBackend>,
    shutdown: rio_common::signal::Token,
) -> tokio::task::JoinHandle<()> {
    rio_common::task::spawn_periodic("gc-drain-task", DRAIN_INTERVAL, shutdown, move || {
        let pool = pool.clone();
        let backend = Arc::clone(&backend);
        async move {
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
    use crate::test_helpers::mem_backend;
    use rio_test_support::TestDb;

    #[tokio::test]
    async fn drain_deletes_and_removes_row() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let backend: Arc<dyn ChunkBackend> = mem_backend();

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
        let backend: Arc<dyn ChunkBackend> = mem_backend();

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
        let backend: Arc<dyn ChunkBackend> = mem_backend();

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
        let backend: Arc<dyn ChunkBackend> = mem_backend();

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
        use rio_test_support::metrics::CountingRecorder;

        let db = TestDb::new(&crate::MIGRATOR).await;
        let backend: Arc<dyn ChunkBackend> = mem_backend();

        // Seed a row at max attempts — drain should SKIP it.
        sqlx::query("INSERT INTO pending_s3_deletes (s3_key, attempts) VALUES ('stuck-key', $1)")
            .bind(MAX_ATTEMPTS)
            .execute(&db.pool)
            .await
            .unwrap();

        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);

        let (deleted, failed) = drain_once(&db.pool, &backend).await.unwrap();
        // Both 0: the row is excluded by WHERE attempts < MAX.
        // Operator investigates; this row is effectively "parked."
        assert_eq!(deleted, 0);
        assert_eq!(failed, 0, "max-attempts row excluded from drain");

        // bug_029: SELECT returned empty (only stuck rows), but the
        // gauge block must STILL run so the alert fires. Before the
        // fix, the empty-rows early-return skipped it → None.
        assert_eq!(
            rec.gauge_value("rio_store_s3_deletes_stuck{}"),
            Some(1.0),
            "stuck gauge must refresh on empty-SELECT tick; saw {:?}",
            rec.gauge_names()
        );
        assert_eq!(rec.gauge_value("rio_store_s3_deletes_pending{}"), Some(0.0));
    }

    /// bug_029: empty `pending_s3_deletes` table — gauges must still
    /// be set to 0.0 (not left untouched). This is the "alert never
    /// clears after manual cleanup" direction: operator deletes stuck
    /// rows, next tick must drive `_stuck` back to 0.
    #[tokio::test]
    async fn drain_gauge_refreshed_on_empty_table() {
        use rio_test_support::metrics::CountingRecorder;

        let db = TestDb::new(&crate::MIGRATOR).await;
        let backend: Arc<dyn ChunkBackend> = mem_backend();

        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);

        let (deleted, failed) = drain_once(&db.pool, &backend).await.unwrap();
        assert_eq!((deleted, failed), (0, 0));

        assert_eq!(
            rec.gauge_value("rio_store_s3_deletes_pending{}"),
            Some(0.0),
            "pending gauge must be set on empty-table tick"
        );
        assert_eq!(
            rec.gauge_value("rio_store_s3_deletes_stuck{}"),
            Some(0.0),
            "stuck gauge must be set on empty-table tick"
        );
    }

    /// TOCTOU regression: drain's re-check SELECT ... FOR UPDATE must
    /// serialize with a concurrent PutPath upsert. Without FOR UPDATE,
    /// PutPath could resurrect the chunk between the SELECT and the S3
    /// delete → PG says refcount≥1, S3 no longer has the object →
    /// permanent data loss.
    ///
    /// We can't interleave inside drain_once's loop from a unit test,
    /// so we assert the INVARIANT the FOR UPDATE enforces: once drain
    /// holds the row lock, a concurrent upsert BLOCKS until drain
    /// commits. We simulate this by opening a second tx that issues
    /// the upsert while drain's tx holds FOR UPDATE on the chunk row.
    /// The upsert must either see still_dead=true (blocked until
    /// drain committed → chunk deleted → upsert sees refcount=0 and
    /// re-uploads) or drain sees resurrection and skips. Neither path
    /// results in loss.
    // r[verify store.gc.pending-deletes]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drain_for_update_serializes_with_upsert() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let backend: Arc<dyn ChunkBackend> = mem_backend();

        // Seed chunk X: dead state (refcount=0, deleted=true) — sweep
        // already marked it. Pending S3 delete enqueued.
        let hash_x = [0x77u8; 32];
        backend
            .put(&hash_x, bytes::Bytes::from_static(b"chunk-X-toctou"))
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO chunks (blake3_hash, refcount, size, deleted) \
             VALUES ($1, 0, 14, true)",
        )
        .bind(hash_x.as_slice())
        .execute(&db.pool)
        .await
        .unwrap();
        let key_x = backend.key_for(&hash_x);
        sqlx::query("INSERT INTO pending_s3_deletes (s3_key, blake3_hash) VALUES ($1, $2)")
            .bind(&key_x)
            .bind(hash_x.as_slice())
            .execute(&db.pool)
            .await
            .unwrap();

        // Race: drain_once vs the PutPath upsert. With FOR UPDATE, PG
        // serializes these — one completes fully before the other
        // touches the chunks row. Both orderings are correct; neither
        // loses data.
        let pool_a = db.pool.clone();
        let pool_b = db.pool.clone();
        let backend_a = Arc::clone(&backend);

        let (drain_res, upsert_res) = tokio::join!(
            drain_once(&pool_a, &backend_a),
            // PutPath's chunk upsert: ON CONFLICT bumps refcount,
            // clears deleted. RETURNING (uploaded_at IS NULL) tells
            // caller whether to upload (true = no prior upload
            // committed). Mirrors metadata::upgrade_manifest_to_chunked.
            sqlx::query_scalar::<_, bool>(
                "INSERT INTO chunks (blake3_hash, refcount, size, deleted) \
                 VALUES ($1, 1, 14, false) \
                 ON CONFLICT (blake3_hash) DO UPDATE SET \
                   refcount = chunks.refcount + 1, deleted = false \
                 RETURNING (uploaded_at IS NULL)"
            )
            .bind(hash_x.as_slice())
            .fetch_one(&pool_b),
        );
        let (deleted, failed) = drain_res.unwrap();
        let must_upload = upsert_res.unwrap();
        assert_eq!(failed, 0);

        // Two valid serializations:
        //
        // A) Drain wins: re-check sees (deleted AND refcount=0)=true,
        //    S3-deletes, commits. THEN upsert runs against the
        //    deleted row → uploaded_at is NULL → must_upload=true →
        //    caller re-uploads. deleted=1, must_upload=true.
        //
        // B) Upsert wins: refcount→1, deleted→false, commits. THEN
        //    drain's re-check sees (deleted AND refcount=0)=false,
        //    skips S3 delete. deleted=0, must_upload=true (uploaded_at
        //    still NULL — drain seed never set it).
        //
        // What must NEVER happen (the bug FOR UPDATE fixes):
        // deleted=1 AND must_upload=false → S3 deleted but caller
        // skipped re-upload → permanent loss.
        // nonminimal_bool: the negated-conjunction form directly
        // encodes "NOT the bad state"; De Morgan obscures the
        // invariant being asserted.
        #[allow(clippy::nonminimal_bool)]
        let no_loss = !(deleted == 1 && !must_upload);
        assert!(
            no_loss,
            "permanent data loss: S3 deleted but upsert saw uploaded_at set \
             (skipped re-upload). deleted={deleted} must_upload={must_upload}"
        );
        // In practice, with this timing, upsert always sees
        // refcount=1 (chunk was at 0 before). Both serializations
        // yield must_upload=true. Sanity-check that invariant.
        assert!(
            must_upload,
            "upsert should see refcount=1 (was 0) → must re-upload"
        );
    }

    /// bug_189 (batch era): chunk row locks from batch 1 are RELEASED
    /// (batch-1 tx committed) before batch 2's backend call runs.
    /// Seed DRAIN_BATCH_SIZE+1 pending rows so drain_once issues two
    /// batches; the backend blocks on the SECOND `delete_by_keys`
    /// call; from main, `SELECT chunks WHERE blake3_hash=X FOR UPDATE
    /// NOWAIT` for a batch-1 chunk must succeed. The lock window per
    /// chunk is one batched backend round trip — never longer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drain_chunk_lock_released_between_batches() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::sync::Notify;

        let db = TestDb::new(&crate::MIGRATOR).await;

        // DRAIN_BATCH_SIZE rows backdated 1h (batch 1, oldest-first
        // ORDER BY enqueued_at) + 1 fresh row (batch 2). Distinct
        // hashes via the index in the first two bytes.
        let n = DRAIN_BATCH_SIZE as usize;
        let mut hashes: Vec<Vec<u8>> = Vec::with_capacity(n + 1);
        for i in 0..=n {
            let mut h = [0u8; 32];
            h[..2].copy_from_slice(&(i as u16).to_be_bytes());
            hashes.push(h.to_vec());
        }
        let keys: Vec<String> = hashes.iter().map(hex::encode).collect();
        sqlx::query(
            "INSERT INTO chunks (blake3_hash, refcount, size, deleted) \
             SELECT h, 0, 8, true FROM unnest($1::bytea[]) AS h",
        )
        .bind(&hashes)
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO pending_s3_deletes (s3_key, blake3_hash, enqueued_at) \
             SELECT k, h, now() - interval '1 hour' \
               FROM unnest($1::text[], $2::bytea[]) AS t(k, h)",
        )
        .bind(&keys[..n])
        .bind(&hashes[..n])
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO pending_s3_deletes (s3_key, blake3_hash) VALUES ($1, $2)")
            .bind(&keys[n])
            .bind(&hashes[n])
            .execute(&db.pool)
            .await
            .unwrap();

        // Backend: first delete_by_keys OK; second waits on `release`
        // and signals `entered_second` when reached.
        struct BarrierBackend {
            inner: Arc<crate::backend::MemoryChunkBackend>,
            calls: AtomicUsize,
            entered_second: Arc<Notify>,
            release: Arc<Notify>,
        }
        #[async_trait::async_trait]
        impl ChunkBackend for BarrierBackend {
            async fn put(&self, h: &[u8; 32], d: bytes::Bytes) -> anyhow::Result<()> {
                self.inner.put(h, d).await
            }
            async fn get(&self, h: &[u8; 32]) -> anyhow::Result<Option<bytes::Bytes>> {
                self.inner.get(h).await
            }
            async fn exists_batch(&self, h: &[[u8; 32]]) -> anyhow::Result<Vec<bool>> {
                self.inner.exists_batch(h).await
            }
            fn key_for(&self, h: &[u8; 32]) -> String {
                self.inner.key_for(h)
            }
            async fn delete_by_key(&self, k: &str) -> anyhow::Result<()> {
                self.inner.delete_by_key(k).await
            }
            async fn delete_by_keys(
                &self,
                keys: &[String],
            ) -> anyhow::Result<Vec<crate::backend::BatchDeleteFailure>> {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                if call == 1 {
                    self.entered_second.notify_one();
                    self.release.notified().await;
                }
                self.inner.delete_by_keys(keys).await
            }
            async fn put_blob(&self, k: &str, d: bytes::Bytes) -> anyhow::Result<()> {
                self.inner.put_blob(k, d).await
            }
            async fn get_blob(&self, k: &str) -> anyhow::Result<Option<bytes::Bytes>> {
                self.inner.get_blob(k).await
            }
            async fn delete_blob(&self, k: &str) -> anyhow::Result<()> {
                self.inner.delete_blob(k).await
            }
        }

        let entered_second = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let backend: Arc<dyn ChunkBackend> = Arc::new(BarrierBackend {
            inner: mem_backend(),
            calls: AtomicUsize::new(0),
            entered_second: Arc::clone(&entered_second),
            release: Arc::clone(&release),
        });

        let pool = db.pool.clone();
        let drain = tokio::spawn(async move { drain_once(&pool, &backend).await });

        // Wait until batch 1 committed and batch 2's backend call is
        // mid-flight.
        entered_second.notified().await;

        // A batch-1 chunk's row lock MUST be released (batch-1 tx
        // committed). FOR UPDATE NOWAIT → 55P03 if still held.
        let r = sqlx::query("SELECT 1 FROM chunks WHERE blake3_hash = $1 FOR UPDATE NOWAIT")
            .bind(&hashes[0])
            .execute(&db.pool)
            .await;
        assert!(
            r.is_ok(),
            "batch-1 chunk lock released before batch-2 backend call; got {r:?}"
        );

        release.notify_one();
        let (deleted, failed) = drain.await.unwrap().unwrap();
        assert_eq!((deleted, failed), (n as u64 + 1, 0));
    }

    /// Per-tick budget scales with backlog: 2.5 × DRAIN_BATCH_SIZE
    /// pending rows drain in ONE drain_once call (3 batches), well
    /// under the MAX_BATCHES_PER_TICK cap. The fixed
    /// one-small-batch-per-tick predecessor capped throughput at
    /// ~3.3 deletes/s and let the backlog grow without bound.
    #[tokio::test]
    async fn drain_scales_batches_with_backlog() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let backend: Arc<dyn ChunkBackend> = mem_backend();

        let n = (DRAIN_BATCH_SIZE as usize) * 5 / 2;
        let keys: Vec<String> = (0..n)
            .map(|i| {
                let mut h = [0u8; 32];
                h[..4].copy_from_slice(&(i as u32).to_be_bytes());
                hex::encode(h)
            })
            .collect();
        // NULL blake3_hash → no chunks re-check; memory backend
        // treats delete of a never-PUT key as idempotent Ok.
        sqlx::query(
            "INSERT INTO pending_s3_deletes (s3_key) SELECT k FROM unnest($1::text[]) AS k",
        )
        .bind(&keys)
        .execute(&db.pool)
        .await
        .unwrap();

        let (deleted, failed) = drain_once(&db.pool, &backend).await.unwrap();
        assert_eq!(failed, 0);
        assert_eq!(
            deleted, n as u64,
            "one tick drains the whole multi-batch backlog"
        );
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM pending_s3_deletes")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(count.0, 0);
    }

    /// Per-key failures inside a batch: failed keys keep their
    /// tombstone rows (attempts+1, last_error) for retry; the rest of
    /// the batch is deleted normally. No silent loss of a failed key.
    #[tokio::test]
    async fn drain_partial_batch_failure_retains_failed_keys() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let backend: Arc<dyn ChunkBackend> = mem_backend();

        // Two deletable chunks + one key the memory backend can't
        // parse (not hex → per-key failure from the default
        // delete_by_keys loop).
        for i in [0x51u8, 0x52] {
            let hash = [i; 32];
            backend
                .put(&hash, bytes::Bytes::from(vec![i; 8]))
                .await
                .unwrap();
            sqlx::query("INSERT INTO pending_s3_deletes (s3_key) VALUES ($1)")
                .bind(backend.key_for(&hash))
                .execute(&db.pool)
                .await
                .unwrap();
        }
        sqlx::query("INSERT INTO pending_s3_deletes (s3_key) VALUES ('not-valid-hex!')")
            .execute(&db.pool)
            .await
            .unwrap();

        let (deleted, failed) = drain_once(&db.pool, &backend).await.unwrap();
        assert_eq!(deleted, 2, "both valid keys deleted in the batch");
        assert_eq!(failed, 1, "invalid key reported failed");

        // The failed row survives with attempts=1 + last_error; the
        // succeeded rows are gone.
        let rows: Vec<(String, i32, Option<String>)> =
            sqlx::query_as("SELECT s3_key, attempts, last_error FROM pending_s3_deletes")
                .fetch_all(&db.pool)
                .await
                .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "not-valid-hex!");
        assert_eq!(rows[0].1, 1, "attempts incremented for the failed key");
        assert!(rows[0].2.is_some(), "last_error recorded");
        // Both backend objects actually gone.
        assert!(backend.get(&[0x51u8; 32]).await.unwrap().is_none());
        assert!(backend.get(&[0x52u8; 32]).await.unwrap().is_none());
    }

    /// SKIP LOCKED: two concurrent drain_once calls against the same
    /// pool must grab DISJOINT batches. With 5 pending rows, total
    /// S3 deletes should be exactly 5 (not 10).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drain_skip_locked_disjoint_batches() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let backend: Arc<dyn ChunkBackend> = mem_backend();

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
