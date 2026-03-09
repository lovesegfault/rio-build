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
    if let Some(backend) = backend {
        for (hash, _) in &zeroed {
            let Ok(arr) = <[u8; 32]>::try_from(hash.as_slice()) else {
                warn!("GC: chunk hash wrong length, skipping S3 enqueue");
                continue;
            };
            let key = backend.key_for(&arr);
            sqlx::query("INSERT INTO pending_s3_deletes (s3_key) VALUES ($1)")
                .bind(&key)
                .execute(&mut **tx)
                .await?;
            stats.s3_keys_enqueued += 1;
        }
    }

    Ok(stats)
}
