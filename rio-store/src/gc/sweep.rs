//! Sweep phase: delete unreachable paths + decrement chunks + enqueue S3.
// r[impl store.gc.two-phase]

use std::sync::Arc;

use sqlx::PgPool;
use tracing::info;

use crate::backend::chunk::ChunkBackend;

use super::{GcStats, decrement_and_enqueue};

/// Batch size for sweep transactions. Each batch is a single tx:
/// N narinfo DELETEs + chunk refcount decrements + pending_s3_deletes
/// INSERTs. Small enough that a batch-rollback on conflict doesn't
/// waste much; large enough to amortize tx overhead.
const SWEEP_BATCH_SIZE: usize = 100;

/// Grace period before a standalone chunk (refcount=0, written by
/// `PutChunk`) becomes GC-eligible.
///
/// # Why a grace window exists
///
/// Client-side chunking sends PutChunk (per chunk) then PutPath
/// (manifest). In between, the chunk exists at refcount=0 — no
/// manifest references it yet. Without a grace window, a GC sweep
/// running in that gap would see `refcount=0 AND deleted=FALSE`,
/// reap the chunk, and the subsequent PutPath would bump refcount
/// to 1 on a chunk whose S3 object is already gone (or queued for
/// deletion — drain.rs's re-check would save it, but that's the
/// LAST line of defense, not the first).
///
/// # Why 300s
///
/// Long enough to cover the worst-case PutChunk → PutPath gap: a
/// worker streaming a 1000-chunk manifest at one-PutChunk-per-RTT
/// over a 200ms-latency WAN link takes ~200s. 300s gives headroom.
/// Short enough that a genuinely abandoned chunk (worker crashed
/// between PutChunk and PutPath) leaks storage for only 5 minutes
/// before the next sweep reaps it.
///
/// Compare `orphan::STALE_THRESHOLD` (2h) — that's for stale
/// `uploading` manifests, which are rarer (only on crash) and
/// whose false-positive reaping is costlier (a whole NAR
/// re-upload). Orphan chunks are cheap to re-PutChunk.
///
/// `i64` to match the bind pattern in `orphan.rs` (`make_interval(
/// secs => $1)` accepts a bigint bind — PG casts it to double
/// internally for the `secs` named argument).
pub const CHUNK_GRACE_SECS: i64 = 300;

/// Sweep unreachable paths. For each:
/// 1. `SELECT chunk_list FOR UPDATE` (TOCTOU guard vs PutPath
///    incrementing a refcount we're about to decrement)
/// 2. `DELETE realisations` for this path (NO FK to narinfo —
///    explicit delete prevents dangling wopQueryRealisation rows)
/// 3. `DELETE narinfo` (CASCADE → manifests/manifest_data/
///    content_index)
/// 4. `UPDATE chunks SET refcount = refcount - 1`
/// 5. `UPDATE chunks SET deleted = true WHERE refcount = 0 RETURNING`
/// 6. `INSERT INTO pending_s3_deletes` for each returned chunk
///
/// Batched: steps 1-5 run in ONE transaction for SWEEP_BATCH_SIZE
/// paths at a time. If `dry_run`: do the work, compute stats, then
/// `ROLLBACK` instead of `COMMIT` — operators can see what WOULD
/// be deleted without committing.
///
/// `chunk_backend` is only used for `key_for()` (no I/O). If None
/// (inline-only store), chunks are never populated so steps 3-5
/// are no-ops — just the narinfo CASCADE delete happens.
pub async fn sweep(
    pool: &PgPool,
    chunk_backend: Option<&Arc<dyn ChunkBackend>>,
    unreachable: Vec<Vec<u8>>,
    dry_run: bool,
) -> Result<GcStats, sqlx::Error> {
    let mut stats = GcStats::default();

    for batch in unreachable.chunks(SWEEP_BATCH_SIZE) {
        let mut tx = pool.begin().await?;

        for store_path_hash in batch {
            // Step 1: SELECT chunk_list FOR UPDATE. NULL for
            // inline storage. The FOR UPDATE locks the MANIFEST
            // row — a concurrent PutPath for the SAME path blocks
            // until we COMMIT (prevents re-upload mid-sweep).
            //
            // This does NOT guard chunk-level races: a DIFFERENT
            // path sharing chunk X can PutPath after we've already
            // set X to deleted=true+refcount=0 and enqueued it.
            // That race is handled by drain.rs's blake3_hash
            // re-check against chunks.(deleted AND refcount=0)
            // before calling S3 DeleteObject — PutPath's upsert
            // clears deleted=false, drain sees "not dead", skips.
            //
            // LEFT JOIN manifest_data: inline paths have no row
            // there. `chunk_list` is NULL for inline; we skip
            // the chunk decrement loop.
            let chunk_list: Option<Vec<u8>> = sqlx::query_scalar(
                r#"
                SELECT md.chunk_list
                  FROM manifests m
                  LEFT JOIN manifest_data md USING (store_path_hash)
                 WHERE m.store_path_hash = $1
                   FOR UPDATE OF m
                "#,
            )
            .bind(store_path_hash)
            .fetch_optional(&mut *tx)
            .await?
            .flatten();

            // Step 1b: reference re-check. Mark held GC_MARK_LOCK_ID
            // exclusive, but we RELEASED it before sweep (to avoid
            // blocking PutPath during the longer sweep phase). A PutPath
            // that completed BETWEEN mark and now may have written
            // references=[this_path]. Re-check via GIN index before
            // deleting. If found: skip, increment resurrected metric.
            //
            // The subquery resolves hash→path because narinfo."references"
            // is TEXT[] (store_path strings, not hashes). The GIN index
            // (migration 008) makes `= ANY("references")` index-scannable.
            let has_referrer: bool = sqlx::query_scalar(
                r#"
                SELECT EXISTS (
                  SELECT 1 FROM narinfo
                   WHERE (SELECT store_path FROM narinfo WHERE store_path_hash = $1)
                         = ANY("references")
                   LIMIT 1
                )
                "#,
            )
            .bind(store_path_hash)
            .fetch_one(&mut *tx)
            .await?;
            if has_referrer {
                tracing::debug!(
                    store_path_hash = %hex::encode(store_path_hash),
                    "GC sweep: path resurrected (new referrer after mark), skipping"
                );
                metrics::counter!("rio_store_gc_path_resurrected_total").increment(1);
                stats.paths_resurrected += 1;
                continue;
            }

            // Step 2a: DELETE realisations for this path. NOT via
            // CASCADE — realisations has NO FK to narinfo (002_
            // store.sql:134). Without this explicit DELETE, dangling
            // realisations rows point to swept paths →
            // wopQueryRealisation returns a path that 404s on fetch.
            // The realisations_output_idx index makes this fast.
            sqlx::query(
                r#"
                DELETE FROM realisations
                 WHERE output_path = (
                   SELECT store_path FROM narinfo WHERE store_path_hash = $1
                 )
                "#,
            )
            .bind(store_path_hash)
            .execute(&mut *tx)
            .await?;

            // Step 2b: DELETE narinfo. CASCADE takes manifests,
            // manifest_data, content_index (but NOT realisations —
            // see step 2a above).
            let deleted = sqlx::query("DELETE FROM narinfo WHERE store_path_hash = $1")
                .bind(store_path_hash)
                .execute(&mut *tx)
                .await?;
            if deleted.rows_affected() == 0 {
                // Already gone (concurrent sweep? shouldn't happen
                // with FOR UPDATE but be defensive). Skip.
                continue;
            }
            stats.paths_deleted += 1;

            // Steps 3-5: chunk refcount + pending_s3_deletes.
            // Only if chunked storage (chunk_list non-None).
            if let Some(bytes) = chunk_list {
                let dec = decrement_and_enqueue(&mut tx, &bytes, chunk_backend).await?;
                stats.chunks_deleted += dec.chunks_zeroed;
                stats.s3_keys_enqueued += dec.s3_keys_enqueued;
                stats.bytes_freed += dec.bytes_freed;
            }
        }

        // Commit or rollback the batch.
        if dry_run {
            // Rollback: all changes in this tx are discarded.
            // Stats were accumulated from what WOULD have
            // happened — operator sees the impact without
            // committing.
            tx.rollback().await?;
        } else {
            tx.commit().await?;
        }
    }

    info!(
        paths_deleted = stats.paths_deleted,
        paths_resurrected = stats.paths_resurrected,
        chunks_deleted = stats.chunks_deleted,
        s3_keys_enqueued = stats.s3_keys_enqueued,
        bytes_freed = stats.bytes_freed,
        dry_run,
        "GC sweep complete"
    );

    // Sweep counters. Singular naming matches
    // rio_store_gc_path_resurrected_total (observability.md:138).
    // `s3_key` not `chunk`: GcStats has s3_keys_enqueued (mod.rs:90);
    // there is no chunks_enqueued field — chunks are marked deleted
    // in PG, keys are what get queued for S3 DeleteObject.
    //
    // Gated on !dry_run: a dry-run ROLLBACKs the sweep tx. The stats
    // show what WOULD have been swept, but nothing WAS swept. A
    // counter is a promise of monotonic fact, not a what-if.
    if !dry_run {
        metrics::counter!("rio_store_gc_path_swept_total").increment(stats.paths_deleted);
        metrics::counter!("rio_store_gc_s3_key_enqueued_total").increment(stats.s3_keys_enqueued);
    }

    Ok(stats)
}

/// Sweep standalone chunks: `refcount=0` rows whose grace window
/// has expired.
///
/// These are chunks written by `PutChunk` that no subsequent
/// `PutPath` ever claimed. The main [`sweep`] above only touches
/// chunks as a SIDE EFFECT of path deletion (it decrements
/// refcounts for the swept path's chunk_list, then reaps whatever
/// hit zero). A chunk that STARTED at zero — never referenced by
/// any manifest — is invisible to that flow. This sweep finds them.
///
/// # Race with PutPath
///
/// The `FOR UPDATE SKIP LOCKED` + in-transaction `WHERE refcount=0`
/// guard mirrors `decrement_and_enqueue`'s logic, but for the
/// opposite direction: here we need to check refcount is STILL
/// zero at commit time, because a concurrent PutPath's chunk
/// UPSERT (`metadata/chunked.rs:117`) bumps refcount and clears
/// `deleted`. SKIP LOCKED means two concurrent sweeps (shouldn't
/// happen — GC_LOCK_ID serializes GC — but defense in depth
/// against a future direct caller) don't contend.
///
/// If PutPath's UPSERT lands between our outer SELECT and our
/// inner UPDATE: the UPDATE's `WHERE refcount = 0` won't match
/// (refcount is now 1), rows_affected is 0, chunk survives. If it
/// lands AFTER our commit (chunk already `deleted=true`, S3 key
/// enqueued): PutPath's UPSERT sets `deleted=false`, drain.rs's
/// re-check (`deleted AND refcount=0`) sees refcount>0, skips the
/// S3 delete. Same resurrection path as the main sweep.
///
/// # Returns
///
/// `(chunks_deleted, bytes_freed)`. Callers fold these into
/// [`GcStats`] (or log them directly — this is callable outside
/// the main GC run for a lightweight "just clean up orphan chunks"
/// cron).
// r[impl store.chunk.grace-ttl]
pub async fn sweep_orphan_chunks(
    pool: &PgPool,
    chunk_backend: Option<&Arc<dyn ChunkBackend>>,
    grace_secs: i64,
) -> Result<(u64, u64), sqlx::Error> {
    // Outer SELECT: candidates. refcount=0 + not-yet-deleted + old
    // enough. This is EXACTLY the `idx_chunks_gc` partial index
    // predicate (`refcount = 0 AND deleted = FALSE`) with an extra
    // `created_at` filter — PG uses the index for the predicate
    // match, then filters by created_at on the heap rows. Cheap
    // even with millions of live chunks (the index only covers the
    // handful at refcount=0).
    //
    // Snapshot semantics: rows returned here may be stale by the
    // time we reach the inner loop (PutPath bumped refcount). The
    // inner UPDATE re-checks.
    let candidates: Vec<(Vec<u8>, i64)> = sqlx::query_as(
        r#"
        SELECT blake3_hash, size FROM chunks
         WHERE refcount = 0 AND deleted = FALSE
           AND created_at < now() - make_interval(secs => $1)
        "#,
    )
    .bind(grace_secs)
    .fetch_all(pool)
    .await?;

    if candidates.is_empty() {
        return Ok((0, 0));
    }

    let mut chunks_deleted = 0u64;
    let mut bytes_freed = 0u64;

    // Batched transactions. Same SWEEP_BATCH_SIZE rationale as the
    // main sweep: small enough to roll back cheaply, large enough
    // to amortize. A future pathological case (a worker firing
    // thousands of PutChunk calls then crashing) produces a large
    // candidate set; batching keeps each tx bounded.
    for batch in candidates.chunks(SWEEP_BATCH_SIZE) {
        let mut tx = pool.begin().await?;

        let hashes: Vec<Vec<u8>> = batch.iter().map(|(h, _)| h.clone()).collect();

        // Inner UPDATE: re-check refcount=0 + deleted=FALSE at
        // execution time. `RETURNING` gives us the rows that
        // actually flipped — the difference between `hashes.len()`
        // and `zeroed.len()` is the count of chunks that were
        // resurrected (PutPath claimed them) between outer SELECT
        // and now. No metric for that yet (would be
        // `rio_store_gc_chunk_resurrected_total` if we see it
        // happen in practice).
        //
        // No FOR UPDATE needed on the outer SELECT: the UPDATE's
        // WHERE clause IS the guard. PG's row-level locking for
        // UPDATE serializes against the PutPath UPSERT on the
        // same blake3_hash.
        let zeroed: Vec<(Vec<u8>, i64)> = sqlx::query_as(
            r#"
            UPDATE chunks SET deleted = TRUE
             WHERE blake3_hash = ANY($1)
               AND refcount = 0 AND deleted = FALSE
            RETURNING blake3_hash, size
            "#,
        )
        .bind(&hashes)
        .fetch_all(&mut *tx)
        .await?;

        chunks_deleted += zeroed.len() as u64;
        bytes_freed += zeroed.iter().map(|(_, s)| *s as u64).sum::<u64>();

        // Enqueue S3 keys. Identical to the enqueue block in
        // `decrement_and_enqueue` (gc/mod.rs:558) — same unnest
        // batch insert, same `ON CONFLICT DO NOTHING` idempotence,
        // same `blake3_hash` column for drain's re-check. If
        // `chunk_backend` is None (inline-only store), there are
        // no S3 keys to delete — but an inline-only store also
        // has no PutChunk clients (require_cache() returns
        // FAILED_PRECONDITION), so `candidates` is empty and we
        // never reach here. The `if let` is belt-and-suspenders.
        if let Some(backend) = chunk_backend
            && !zeroed.is_empty()
        {
            let mut keys: Vec<String> = Vec::with_capacity(zeroed.len());
            let mut enqueue_hashes: Vec<Vec<u8>> = Vec::with_capacity(zeroed.len());
            for (hash, _) in &zeroed {
                // 32-byte invariant: the `chunks` table PK is
                // BYTEA but every writer inserts exactly 32 bytes
                // (BLAKE3 output). try_into() is a can't-happen
                // guard — `warn!` + skip rather than panic so one
                // corrupt row doesn't kill the sweep.
                let Ok(arr) = <[u8; 32]>::try_from(hash.as_slice()) else {
                    tracing::warn!(
                        len = hash.len(),
                        "sweep_orphan_chunks: chunk hash wrong length, skipping S3 enqueue"
                    );
                    continue;
                };
                keys.push(backend.key_for(&arr));
                enqueue_hashes.push(hash.clone());
            }
            if !keys.is_empty() {
                sqlx::query(
                    "INSERT INTO pending_s3_deletes (s3_key, blake3_hash) \
                     SELECT * FROM unnest($1::text[], $2::bytea[]) \
                     ON CONFLICT DO NOTHING",
                )
                .bind(&keys)
                .bind(&enqueue_hashes)
                .execute(&mut *tx)
                .await?;
            }
        }

        tx.commit().await?;
    }

    if chunks_deleted > 0 {
        info!(
            chunks_deleted,
            bytes_freed, grace_secs, "orphan chunk sweep: reaped standalone chunks past grace TTL"
        );
        // Matches the naming convention of rio_store_gc_path_swept_total
        // (observability.md). Counter (not gauge) — monotonic "chunks
        // ever reaped by orphan sweep".
        metrics::counter!("rio_store_gc_chunk_orphan_swept_total").increment(chunks_deleted);
    }

    Ok((chunks_deleted, bytes_freed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::TestDb;
    use sha2::Digest;

    async fn seed_complete_path(pool: &PgPool, path: &str) -> Vec<u8> {
        let hash: Vec<u8> = sha2::Sha256::digest(path.as_bytes()).to_vec();
        sqlx::query(
            "INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size) \
             VALUES ($1, $2, $1, 0)",
        )
        .bind(&hash)
        .bind(path)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO manifests (store_path_hash, status) VALUES ($1, 'complete')")
            .bind(&hash)
            .execute(pool)
            .await
            .unwrap();
        hash
    }

    /// Sweep must DELETE realisations rows pointing to swept paths.
    /// realisations has NO FK to narinfo (002_store.sql:134); without
    /// the explicit DELETE, dangling rows → wopQueryRealisation returns
    /// a path that 404s on fetch.
    #[tokio::test]
    async fn sweep_deletes_realisations() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Seed a complete path + a realisation pointing to it.
        let path = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-sweep-target";
        let hash = seed_complete_path(&db.pool, path).await;
        sqlx::query(
            "INSERT INTO realisations (drv_hash, output_name, output_path, output_hash) \
             VALUES ($1, 'out', $2, $3)",
        )
        .bind(vec![0x11u8; 32])
        .bind(path)
        .bind(vec![0x22u8; 32])
        .execute(&db.pool)
        .await
        .unwrap();

        // Sweep the path.
        let stats = sweep(&db.pool, None, vec![hash.clone()], false)
            .await
            .unwrap();
        assert_eq!(stats.paths_deleted, 1);

        // narinfo gone (CASCADE took manifests too).
        let narinfo_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM narinfo")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(narinfo_count, 0);

        // Realisation ALSO gone (explicit DELETE, not CASCADE).
        let realisations_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM realisations")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(
            realisations_count, 0,
            "sweep should delete realisations pointing to swept path (no FK CASCADE)"
        );
    }

    /// Dry-run: compute stats but ROLLBACK. Nothing actually deleted.
    #[tokio::test]
    async fn sweep_dry_run_rolls_back() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let path = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-dryrun";
        let hash = seed_complete_path(&db.pool, path).await;

        let stats = sweep(&db.pool, None, vec![hash.clone()], true)
            .await
            .unwrap();
        // Stats SHOW the path would be deleted.
        assert_eq!(stats.paths_deleted, 1);

        // But narinfo still there (rolled back).
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM narinfo")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "dry-run should roll back");
    }

    /// Sweep's per-path reference re-check catches paths that gained
    /// a new referrer AFTER mark. This simulates the race: mark
    /// declared P unreachable, a PutPath for Q completes with
    /// references=[P], sweep runs and must skip P.
    #[tokio::test]
    async fn sweep_resurrected_path_skipped() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // P: marked unreachable (would be swept).
        let p = "/nix/store/pppppppppppppppppppppppppppppppp-resurrected";
        let p_hash = seed_complete_path(&db.pool, p).await;

        // Q: references P. Seeded AFTER mark (simulating the race:
        // mark returned [p_hash], THEN PutPath for Q completed).
        let q = "/nix/store/qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq-referrer";
        let q_hash: Vec<u8> = sha2::Sha256::digest(q.as_bytes()).to_vec();
        sqlx::query(
            r#"INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size, "references")
               VALUES ($1, $2, $1, 0, ARRAY[$3::text])"#,
        )
        .bind(&q_hash)
        .bind(q)
        .bind(p)
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO manifests (store_path_hash, status) VALUES ($1, 'complete')")
            .bind(&q_hash)
            .execute(&db.pool)
            .await
            .unwrap();

        // Sweep with P in the unreachable list. The reference re-check should
        // find Q.references=[P] → skip P → paths_resurrected=1.
        let stats = sweep(&db.pool, None, vec![p_hash.clone()], false)
            .await
            .unwrap();
        assert_eq!(
            stats.paths_deleted, 0,
            "P should NOT be deleted — Q references it"
        );
        assert_eq!(
            stats.paths_resurrected, 1,
            "P should be counted as resurrected (reference re-check)"
        );

        // P still exists in narinfo.
        let p_exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM narinfo WHERE store_path_hash = $1)")
                .bind(&p_hash)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert!(p_exists, "P should still exist (resurrected, not swept)");
    }

    /// If nobody references the path, sweep proceeds normally (no false-positive resurrection).
    #[tokio::test]
    async fn sweep_unreferenced_path_deleted() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let p = "/nix/store/cccccccccccccccccccccccccccccccc-unreferenced";
        let hash = seed_complete_path(&db.pool, p).await;

        let stats = sweep(&db.pool, None, vec![hash], false).await.unwrap();
        assert_eq!(stats.paths_deleted, 1);
        assert_eq!(stats.paths_resurrected, 0);
    }

    // r[verify store.chunk.refcount-txn]
    /// Sweep a path WITH chunk_list (chunked storage): verify the
    /// `if let Some(bytes) = chunk_list` branch fires, decrements
    /// refcounts, marks zeroed chunks deleted, and enqueues S3 keys.
    /// All existing sweep tests use `seed_complete_path` which sets
    /// NO chunk_list — this is the chunked-storage path.
    #[tokio::test]
    async fn sweep_chunked_path_decrements_and_enqueues() {
        use crate::backend::chunk::{ChunkBackend, MemoryChunkBackend};
        use crate::manifest::{Manifest, ManifestEntry};
        use std::sync::Arc;

        let db = TestDb::new(&crate::MIGRATOR).await;
        let path = "/nix/store/dddddddddddddddddddddddddddddddd-chunked";
        let path_hash: Vec<u8> = sha2::Sha256::digest(path.as_bytes()).to_vec();

        // Seed two chunks at refcount=1 (will zero + enqueue).
        let mut chunk_h1 = [0u8; 32];
        chunk_h1[0] = 0xAA;
        let mut chunk_h2 = [0u8; 32];
        chunk_h2[0] = 0xBB;
        for (h, size) in [(&chunk_h1, 1000i64), (&chunk_h2, 2000i64)] {
            sqlx::query("INSERT INTO chunks (blake3_hash, refcount, size) VALUES ($1, 1, $2)")
                .bind(&h[..])
                .bind(size)
                .execute(&db.pool)
                .await
                .unwrap();
        }

        // Seed narinfo + manifest + manifest_data (chunked: inline_blob
        // NULL, chunk_list set).
        sqlx::query(
            "INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size) \
             VALUES ($1, $2, $1, 0)",
        )
        .bind(&path_hash)
        .bind(path)
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO manifests (store_path_hash, status, inline_blob) \
             VALUES ($1, 'complete', NULL)",
        )
        .bind(&path_hash)
        .execute(&db.pool)
        .await
        .unwrap();
        let chunk_list = Manifest {
            entries: vec![
                ManifestEntry {
                    hash: chunk_h1,
                    size: 1000,
                },
                ManifestEntry {
                    hash: chunk_h2,
                    size: 2000,
                },
            ],
        }
        .serialize();
        sqlx::query("INSERT INTO manifest_data (store_path_hash, chunk_list) VALUES ($1, $2)")
            .bind(&path_hash)
            .bind(&chunk_list)
            .execute(&db.pool)
            .await
            .unwrap();

        // Sweep with a backend → decrement + enqueue.
        let backend: Arc<dyn ChunkBackend> = Arc::new(MemoryChunkBackend::new());
        let stats = sweep(&db.pool, Some(&backend), vec![path_hash.clone()], false)
            .await
            .unwrap();

        assert_eq!(stats.paths_deleted, 1);
        assert_eq!(stats.chunks_deleted, 2, "both chunks zeroed");
        assert_eq!(stats.s3_keys_enqueued, 2);
        assert_eq!(stats.bytes_freed, 3000, "1000 + 2000");

        // Both chunks: refcount=0, deleted=true.
        let deleted_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM chunks WHERE refcount = 0 AND deleted = true")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(deleted_count, 2);

        // pending_s3_deletes has 2 rows.
        let enqueued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_s3_deletes")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(enqueued, 2);

        // narinfo + manifest gone (CASCADE).
        let narinfo_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM narinfo")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(narinfo_count, 0);
    }
}
