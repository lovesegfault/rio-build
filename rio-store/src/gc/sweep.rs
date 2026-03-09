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

    Ok(stats)
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
}
