//! Two-phase garbage collection: mark reachable, sweep unreachable.
//!
//! # Phases
//!
//! 1. **Mark** ([`mark::compute_unreachable`]): recursive CTE over
//!    `narinfo."references"` from root seeds. Returns `store_path_hash`
//!    for paths NOT reachable from any root.
//!
//! 2. **Sweep** ([`sweep::sweep`]): per unreachable path, in batched
//!    transactions: DELETE narinfo (CASCADE), decrement chunk refcounts,
//!    mark refcount=0 chunks deleted, enqueue S3 keys to
//!    `pending_s3_deletes`. `SELECT FOR UPDATE` on chunk_list guards
//!    the TOCTOU with concurrent PutPath refcount increment.
//!
//! 3. **Drain** ([`drain::spawn_drain_task`]): background task that
//!    reads `pending_s3_deletes`, calls `ChunkBackend::delete_by_key`,
//!    deletes row on success / increments attempts on failure. Max
//!    attempts = 10 (alert-worthy after that).
//!
//! # Root seeds
//!
//! - `gc_roots` table (explicit pins — `PinPath` RPC)
//! - `manifests WHERE status='uploading'` (in-flight PutPath —
//!   don't delete what's being written)
//! - `narinfo WHERE created_at > now() - grace_hours` (recent
//!   paths — don't GC something that JUST arrived before a build
//!   can reference it)
//! - `extra_roots` param (scheduler's live-build output paths —
//!   passed from `ActorCommand::GcRoots`, may not be in narinfo
//!   yet so can't use FK'd gc_roots)
//!
//! # Two-phase S3 commit
//!
//! The sweep tx can DELETE narinfo atomically, but S3 DeleteObject
//! isn't transactional. Enqueue S3 keys in the SAME tx; drain
//! later. If drain fails, object leaks (storage cost) but PG state
//! is correct. Better than the reverse (S3 deleted, tx rolled back,
//! dangling chunk ref → GetPath fails).

pub mod drain;
pub mod mark;
pub mod orphan;
pub mod sweep;
pub mod tenant;

/// PG advisory lock ID for TriggerGC. Arbitrary constant — just
/// needs to not collide with other advisory locks in the schema
/// (only GC_MARK_LOCK_ID below). "rOGC" ASCII + 1.
///
/// Serializes GC-vs-GC: two concurrent TriggerGC calls would
/// waste work and produce misleading stats. `pg_try_advisory_lock`
/// (non-blocking) — second caller gets "already running".
pub const GC_LOCK_ID: i64 = 0x724F_4743_0001;

/// PG advisory lock ID for the mark-vs-PutPath race.
///
/// PutPath takes this SHARED (`pg_advisory_lock_shared`) around
/// the placeholder insert → complete_manifest window. Mark takes
/// this EXCLUSIVE (`pg_advisory_lock`) around `compute_unreachable`.
/// Sweep does NOT hold it — instead it re-checks references
/// per-path inside the FOR UPDATE tx (GIN-indexed).
///
/// This gives: mark blocks PutPath for ~1s (CTE duration),
/// sweep doesn't block PutPath at all. If a PutPath completes
/// BETWEEN mark and sweep with a reference to a marked-unreachable
/// path, sweep's re-check catches it and skips the delete
/// (`rio_store_gc_path_resurrected_total` metric).
pub const GC_MARK_LOCK_ID: i64 = 0x724F_4743_0002;

use std::sync::Arc;

use sqlx::{Postgres, Transaction};
use tracing::warn;

use crate::backend::chunk::ChunkBackend;
use crate::manifest::Manifest;

/// Summary stats from a GC run.
#[derive(Debug, Default, Clone)]
pub struct GcStats {
    /// Paths deleted from narinfo (and cascaded tables).
    pub paths_deleted: u64,
    /// Chunks marked deleted (refcount → 0 this sweep).
    pub chunks_deleted: u64,
    /// S3 keys enqueued to pending_s3_deletes.
    pub s3_keys_enqueued: u64,
    /// Total bytes of chunks marked deleted (for storage savings estimate).
    pub bytes_freed: u64,
    /// Paths skipped because a new narinfo referenced them after
    /// mark (mark-vs-sweep race window — a PutPath completed BETWEEN
    /// mark and sweep with this path in its references). Sweep's
    /// per-path re-check catches these and skips the delete.
    /// Metric for alerting if this is frequent.
    pub paths_resurrected: u64,
}

/// Result of [`decrement_and_enqueue`]: stats for the chunks touched by
/// ONE manifest's chunk_list.
#[derive(Debug, Default)]
pub(super) struct DecrementStats {
    /// Chunks that hit refcount=0 and were marked `deleted=true`.
    pub chunks_zeroed: u64,
    /// S3 keys inserted into `pending_s3_deletes`.
    pub s3_keys_enqueued: u64,
    /// Sum of `chunks.size` for zeroed chunks (bytes).
    pub bytes_freed: u64,
}

/// Shared helper for [`sweep::sweep`] and [`orphan::scan_once`]:
/// given a serialized manifest `chunk_list`, decrement refcounts
/// for its unique chunks, mark any that hit 0 as deleted, and
/// enqueue their S3 keys to `pending_s3_deletes`.
///
/// Runs inside an EXISTING transaction — caller is responsible for
/// begin/commit/rollback. Returns per-manifest stats for the caller
/// to aggregate.
///
/// A corrupt `chunk_list` (fails `Manifest::deserialize`) is logged
/// and yields zero stats — the narinfo DELETE (caller's step 2) has
/// already CASCADEd the manifest away, so the worst case is leaked
/// refcounts (chunks survive until a future GC sees them at actual 0).
pub(super) async fn decrement_and_enqueue(
    tx: &mut Transaction<'_, Postgres>,
    chunk_list: &[u8],
    backend: Option<&Arc<dyn ChunkBackend>>,
) -> Result<DecrementStats, sqlx::Error> {
    let mut stats = DecrementStats::default();

    let manifest = match Manifest::deserialize(chunk_list) {
        Ok(m) => m,
        Err(e) => {
            warn!(error = %e, "GC: corrupt chunk_list, skipping decrement");
            return Ok(stats);
        }
    };

    // Dedup chunk hashes: a manifest CAN repeat chunks if the NAR has
    // duplicate content blocks; decrement once per unique hash.
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
        return Ok(stats);
    }

    // Decrement refcounts. ANY($1) for batch.
    sqlx::query("UPDATE chunks SET refcount = refcount - 1 WHERE blake3_hash = ANY($1)")
        .bind(&unique_hashes)
        .execute(&mut **tx)
        .await?;

    // Mark refcount=0 as deleted, return hashes + sizes for stats.
    // Only rows we JUST touched (ANY) AND now at 0.
    let zeroed: Vec<(Vec<u8>, i64)> = sqlx::query_as(
        r#"
        UPDATE chunks SET deleted = true
         WHERE blake3_hash = ANY($1) AND refcount = 0
           AND deleted = false
        RETURNING blake3_hash, size
        "#,
    )
    .bind(&unique_hashes)
    .fetch_all(&mut **tx)
    .await?;

    stats.chunks_zeroed = zeroed.len() as u64;
    stats.bytes_freed = zeroed.iter().map(|(_, s)| *s as u64).sum();

    // Enqueue S3 keys. Only if we have a chunk backend (inline store
    // has no S3 keys to delete).
    //
    // Batched via unnest — one RTT per manifest instead of per-chunk.
    // A manifest with 1000 chunks would otherwise need 1000 INSERTs
    // (~1s at 1ms RTT); batched it's ~1ms. Significant for large sweeps.
    if let Some(backend) = backend {
        let mut keys: Vec<String> = Vec::with_capacity(zeroed.len());
        let mut hashes: Vec<Vec<u8>> = Vec::with_capacity(zeroed.len());
        for (hash, _) in &zeroed {
            let Ok(arr) = <[u8; 32]>::try_from(hash.as_slice()) else {
                warn!("GC: chunk hash wrong length, skipping S3 enqueue");
                continue;
            };
            keys.push(backend.key_for(&arr));
            hashes.push(hash.clone());
        }
        if !keys.is_empty() {
            // blake3_hash lets drain re-check chunks.(deleted AND
            // refcount=0) before S3 delete — catches the TOCTOU where
            // PutPath resurrected the chunk after sweep enqueued it.
            // ON CONFLICT DO NOTHING: duplicate enqueues are fine
            // (idempotent — drain deletes the row after S3 success).
            sqlx::query(
                "INSERT INTO pending_s3_deletes (s3_key, blake3_hash) \
                 SELECT * FROM unnest($1::text[], $2::bytea[]) \
                 ON CONFLICT DO NOTHING",
            )
            .bind(&keys)
            .bind(&hashes)
            .execute(&mut **tx)
            .await?;
            stats.s3_keys_enqueued = keys.len() as u64;
        }
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::chunk::MemoryChunkBackend;
    use crate::manifest::{Manifest, ManifestEntry};
    use rio_test_support::TestDb;
    use sqlx::PgPool;

    /// Seed a chunk row with the given refcount. Returns the blake3 hash.
    async fn seed_chunk(pool: &PgPool, tag: u8, refcount: i32, size: i64) -> [u8; 32] {
        let mut hash = [0u8; 32];
        hash[0] = tag; // distinct per tag
        sqlx::query("INSERT INTO chunks (blake3_hash, refcount, size) VALUES ($1, $2, $3)")
            .bind(&hash[..])
            .bind(refcount)
            .bind(size)
            .execute(pool)
            .await
            .unwrap();
        hash
    }

    /// Build a serialized manifest referencing the given chunk hashes.
    fn make_manifest(hashes: &[[u8; 32]]) -> Vec<u8> {
        Manifest {
            entries: hashes
                .iter()
                .map(|h| ManifestEntry {
                    hash: *h,
                    size: 100,
                })
                .collect(),
        }
        .serialize()
    }

    // r[verify store.chunk.refcount-txn]
    /// Core: manifest references chunks with refcount > 1 → decrement,
    /// nobody hits zero, no deleted=true, no S3 enqueue.
    #[tokio::test]
    async fn decrement_refcounts_no_zero() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let h1 = seed_chunk(&db.pool, 1, 2, 1000).await;
        let h2 = seed_chunk(&db.pool, 2, 3, 2000).await;
        let manifest = make_manifest(&[h1, h2]);
        let backend: Arc<dyn ChunkBackend> = Arc::new(MemoryChunkBackend::new());

        let mut tx = db.pool.begin().await.unwrap();
        let stats = decrement_and_enqueue(&mut tx, &manifest, Some(&backend))
            .await
            .unwrap();
        tx.commit().await.unwrap();

        assert_eq!(stats.chunks_zeroed, 0, "nobody hit zero");
        assert_eq!(stats.s3_keys_enqueued, 0);
        assert_eq!(stats.bytes_freed, 0);

        // Refcounts decremented (2→1, 3→2), not deleted.
        let rows: Vec<(Vec<u8>, i32, bool)> = sqlx::query_as(
            "SELECT blake3_hash, refcount, deleted FROM chunks ORDER BY blake3_hash",
        )
        .fetch_all(&db.pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].1, 1, "h1 refcount 2→1");
        assert!(!rows[0].2);
        assert_eq!(rows[1].1, 2, "h2 refcount 3→2");
        assert!(!rows[1].2);
    }

    /// Chunk at refcount=1 → decrement → 0 → deleted=true + enqueued
    /// to pending_s3_deletes. Stats reflect bytes freed.
    #[tokio::test]
    async fn zeroes_and_enqueues_s3() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let h = seed_chunk(&db.pool, 1, 1, 5000).await;
        let manifest = make_manifest(&[h]);
        let backend: Arc<dyn ChunkBackend> = Arc::new(MemoryChunkBackend::new());

        let mut tx = db.pool.begin().await.unwrap();
        let stats = decrement_and_enqueue(&mut tx, &manifest, Some(&backend))
            .await
            .unwrap();
        tx.commit().await.unwrap();

        assert_eq!(stats.chunks_zeroed, 1);
        assert_eq!(stats.s3_keys_enqueued, 1);
        assert_eq!(stats.bytes_freed, 5000);

        // Chunk row: refcount=0, deleted=true.
        let (refcount, deleted): (i32, bool) =
            sqlx::query_as("SELECT refcount, deleted FROM chunks WHERE blake3_hash = $1")
                .bind(&h[..])
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(refcount, 0);
        assert!(deleted, "zeroed chunk marked deleted");

        // pending_s3_deletes has a row with the backend's key + hash.
        let (s3_key, blake3): (String, Vec<u8>) =
            sqlx::query_as("SELECT s3_key, blake3_hash FROM pending_s3_deletes")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(s3_key, backend.key_for(&h));
        assert_eq!(blake3, h.to_vec());
    }

    /// Manifest can repeat chunk hashes (duplicate content blocks in
    /// the NAR). decrement_and_enqueue MUST dedup — decrement once
    /// per unique hash, not once per entry. Prevents refcount
    /// underflow and double-enqueue.
    #[tokio::test]
    async fn dedupes_duplicate_manifest_entries() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let h = seed_chunk(&db.pool, 1, 2, 1000).await;
        // Manifest references h THREE times.
        let manifest = make_manifest(&[h, h, h]);

        let mut tx = db.pool.begin().await.unwrap();
        let stats = decrement_and_enqueue(&mut tx, &manifest, None)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        assert_eq!(stats.chunks_zeroed, 0);

        // Refcount decremented ONCE (2→1), not three times (2→-1).
        let (refcount,): (i32,) =
            sqlx::query_as("SELECT refcount FROM chunks WHERE blake3_hash = $1")
                .bind(&h[..])
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(refcount, 1, "dedup: 3 manifest refs → 1 decrement");
    }

    /// Corrupt chunk_list bytes → warn + zero stats, no panic.
    /// The narinfo DELETE (caller's responsibility) has already
    /// CASCADEd the manifest away, so worst case = leaked refcounts.
    #[tokio::test]
    async fn corrupt_manifest_returns_zero_stats() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let _h = seed_chunk(&db.pool, 1, 2, 1000).await;

        let mut tx = db.pool.begin().await.unwrap();
        let stats = decrement_and_enqueue(&mut tx, b"garbage bytes", None)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        assert_eq!(stats.chunks_zeroed, 0);
        assert_eq!(stats.bytes_freed, 0);
        assert_eq!(stats.s3_keys_enqueued, 0);

        // Chunk untouched (corrupt manifest → skipped).
        let (refcount,): (i32,) = sqlx::query_as("SELECT refcount FROM chunks")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(refcount, 2, "corrupt manifest → no decrement");
    }

    /// `backend: None` (inline store — no S3). Refcounts decremented
    /// + chunks marked deleted, but NO pending_s3_deletes rows
    /// (nothing to delete from S3).
    #[tokio::test]
    async fn no_backend_skips_s3_enqueue() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let h = seed_chunk(&db.pool, 1, 1, 1000).await;
        let manifest = make_manifest(&[h]);

        let mut tx = db.pool.begin().await.unwrap();
        let stats = decrement_and_enqueue(&mut tx, &manifest, None)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        assert_eq!(stats.chunks_zeroed, 1, "chunk still zeroed");
        assert_eq!(stats.s3_keys_enqueued, 0, "no backend → no enqueue");

        let enqueued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_s3_deletes")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(enqueued, 0);
    }

    /// Empty manifest (no entries) → early return with zero stats.
    #[tokio::test]
    async fn empty_manifest_noop() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let manifest = make_manifest(&[]);

        let mut tx = db.pool.begin().await.unwrap();
        let stats = decrement_and_enqueue(&mut tx, &manifest, None)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        assert_eq!(stats.chunks_zeroed, 0);
        assert_eq!(stats.s3_keys_enqueued, 0);
        assert_eq!(stats.bytes_freed, 0);
    }

    /// ON CONFLICT DO NOTHING: running twice with the same chunk
    /// already zeroed + enqueued → second call doesn't error, doesn't
    /// duplicate the pending_s3_deletes row. (Drain deletes rows
    /// after S3 success; idempotent re-enqueue before drain is fine.)
    #[tokio::test]
    async fn idempotent_enqueue_on_conflict() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        // Two chunks: h1 at 1 (will zero), h2 at 2 (won't).
        let h1 = seed_chunk(&db.pool, 1, 1, 1000).await;
        let h2 = seed_chunk(&db.pool, 2, 2, 2000).await;
        let manifest = make_manifest(&[h1, h2]);
        let backend: Arc<dyn ChunkBackend> = Arc::new(MemoryChunkBackend::new());

        // First pass: h1 zeroed + enqueued.
        let mut tx = db.pool.begin().await.unwrap();
        decrement_and_enqueue(&mut tx, &manifest, Some(&backend))
            .await
            .unwrap();
        tx.commit().await.unwrap();

        // Second pass (same manifest — e.g., orphan scan after GC
        // already swept). h1 stays deleted (deleted=true filter in
        // the UPDATE...RETURNING), h2 goes 1→0 + enqueued.
        let mut tx = db.pool.begin().await.unwrap();
        let stats2 = decrement_and_enqueue(&mut tx, &manifest, Some(&backend))
            .await
            .unwrap();
        tx.commit().await.unwrap();

        // Second pass zeroed h2 (went 1→0) but NOT h1 (deleted=true
        // filter in RETURNING skips already-deleted).
        assert_eq!(stats2.chunks_zeroed, 1, "only h2 zeroed second pass");
        assert_eq!(stats2.s3_keys_enqueued, 1, "only h2 enqueued");

        // pending_s3_deletes has exactly 2 rows (h1 from first pass,
        // h2 from second). No duplicates despite ON CONFLICT exercised
        // for h1's re-enqueue attempt (it was already deleted=true so
        // wasn't in the zeroed list — but if it HAD been, the INSERT
        // would conflict-do-nothing).
        let enqueued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_s3_deletes")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(enqueued, 2);
    }
}
