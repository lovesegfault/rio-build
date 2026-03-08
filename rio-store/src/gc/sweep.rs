//! Sweep phase: delete unreachable paths + decrement chunks + enqueue S3.
// r[impl store.gc.two-phase]

use std::sync::Arc;

use sqlx::PgPool;
use tracing::{info, warn};

use crate::backend::chunk::ChunkBackend;
use crate::manifest::Manifest;

use super::GcStats;

/// Batch size for sweep transactions. Each batch is a single tx:
/// N narinfo DELETEs + chunk refcount decrements + pending_s3_deletes
/// INSERTs. Small enough that a batch-rollback on conflict doesn't
/// waste much; large enough to amortize tx overhead.
const SWEEP_BATCH_SIZE: usize = 100;

/// Sweep unreachable paths. For each:
/// 1. `SELECT chunk_list FOR UPDATE` (TOCTOU guard vs PutPath
///    incrementing a refcount we're about to decrement)
/// 2. `DELETE narinfo` (CASCADE → manifests/manifest_data/
///    content_index/realisations)
/// 3. `UPDATE chunks SET refcount = refcount - 1`
/// 4. `UPDATE chunks SET deleted = true WHERE refcount = 0 RETURNING`
/// 5. `INSERT INTO pending_s3_deletes` for each returned chunk
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
            // inline storage. The FOR UPDATE locks the manifest
            // row — a concurrent PutPath with the same hash would
            // block until we COMMIT. This is the TOCTOU guard:
            // without it, PutPath could increment a refcount
            // between our "SELECT" and "UPDATE refcount - 1",
            // and we'd decrement a count that was JUST incremented
            // → chunk prematurely at refcount=0.
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

            // Step 2: DELETE narinfo. CASCADE takes manifests,
            // manifest_data, content_index, realisations.
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
                // Parse the manifest format (versioned binary).
                // Manifest::deserialize returns entries with
                // {hash: [u8;32], size: u32}. We only need hashes.
                let manifest = match Manifest::deserialize(&bytes) {
                    Ok(m) => m,
                    Err(e) => {
                        // Corrupt manifest — already CASCADEd away
                        // by DELETE narinfo. Log and continue; the
                        // chunks this manifest referenced will have
                        // their refcounts off by one (slightly over-
                        // counted), which just means they survive
                        // until a future GC sees them at actual 0.
                        warn!(error = %e, "sweep: corrupt chunk_list, skipping decrement");
                        continue;
                    }
                };

                // Collect unique chunk hashes (a manifest CAN
                // repeat chunks if the NAR has duplicate content
                // blocks; we decrement once per unique hash).
                // HashSet<[u8;32]> for dedup; Vec<Vec<u8>> for the
                // PG ANY() bind (sqlx needs owned Vec, not [u8;32]).
                let unique_hashes: Vec<Vec<u8>> = {
                    let mut seen = std::collections::HashSet::<[u8; 32]>::new();
                    manifest
                        .entries
                        .into_iter()
                        .filter(|e| seen.insert(e.hash))
                        .map(|e| e.hash.to_vec())
                        .collect()
                };
                if unique_hashes.is_empty() {
                    continue;
                }

                // Step 3: decrement refcounts. ANY($1) for batch.
                sqlx::query(
                    "UPDATE chunks SET refcount = refcount - 1 WHERE blake3_hash = ANY($1)",
                )
                .bind(&unique_hashes)
                .execute(&mut *tx)
                .await?;

                // Step 4: mark refcount=0 as deleted, return hashes
                // + sizes for stats. Only rows we JUST touched (ANY)
                // AND that are now at 0 (other paths may reference
                // them too).
                let zeroed: Vec<(Vec<u8>, i64)> = sqlx::query_as(
                    r#"
                    UPDATE chunks SET deleted = true
                     WHERE blake3_hash = ANY($1) AND refcount = 0
                       AND deleted = false
                    RETURNING blake3_hash, size
                    "#,
                )
                .bind(&unique_hashes)
                .fetch_all(&mut *tx)
                .await?;

                stats.chunks_deleted += zeroed.len() as u64;
                stats.bytes_freed += zeroed.iter().map(|(_, s)| *s as u64).sum::<u64>();

                // Step 5: enqueue S3 keys. Only if we have a
                // chunk backend (inline store has no S3 keys to
                // delete).
                if let Some(backend) = chunk_backend {
                    for (hash, _) in &zeroed {
                        // hash is Vec<u8>; key_for wants [u8;32].
                        // Malformed (wrong length) → skip with
                        // warn; chunk_list came from our own
                        // serializer so this shouldn't happen.
                        let Ok(arr) = <[u8; 32]>::try_from(hash.as_slice()) else {
                            warn!("sweep: chunk hash wrong length, skipping S3 enqueue");
                            continue;
                        };
                        let key = backend.key_for(&arr);
                        sqlx::query("INSERT INTO pending_s3_deletes (s3_key) VALUES ($1)")
                            .bind(&key)
                            .execute(&mut *tx)
                            .await?;
                        stats.s3_keys_enqueued += 1;
                    }
                }
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
        chunks_deleted = stats.chunks_deleted,
        s3_keys_enqueued = stats.s3_keys_enqueued,
        bytes_freed = stats.bytes_freed,
        dry_run,
        "GC sweep complete"
    );

    Ok(stats)
}
