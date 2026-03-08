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
