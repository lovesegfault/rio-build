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

/// PG advisory lock ID for TriggerGC. Arbitrary constant — just
/// needs to not collide with other advisory locks in the schema
/// (only GC_MARK_LOCK_ID below). "rOGC" ASCII + 1.
///
/// Serializes GC-vs-GC: two concurrent TriggerGC calls would
/// waste work and produce misleading stats. `pg_try_advisory_lock`
/// (non-blocking) — second caller gets "already running".
pub const GC_LOCK_ID: i64 = 0x724F_4743_0001;

/// PG advisory lock ID for the mark-vs-PutPath race. Round 4 Z2.
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
    /// mark (Z2 hybrid race window — a PutPath completed BETWEEN
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
    // Round 4 Z18: batched via unnest — one RTT per manifest instead
    // of per-chunk. A manifest with 1000 chunks went from 1000 INSERTs
    // (~1s at 1ms RTT) to 1 (~1ms). Significant for large sweeps.
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
