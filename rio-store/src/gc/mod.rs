//! Two-phase garbage collection: mark reachable, sweep unreachable.
//!
//! # Phases
//!
//! 1. **Mark** (`mark::compute_unreachable`): recursive CTE over
//!    `narinfo."references"` from root seeds. Returns `store_path_hash`
//!    for paths NOT reachable from any root.
//!
//! 2. **Sweep** ([`sweep::sweep`]): per unreachable path, in batched
//!    transactions: lock the path's manifest row (`FOR UPDATE`, so a
//!    concurrent PutPath for the same path waits), re-check
//!    references, DELETE narinfo (CASCADE) plus realisations and
//!    path_tenants. The sweep never touches `chunks` — chunk GC is
//!    decoupled and owned by the collect cycle (phase 3).
//!
//! 3. **Collect** (`collect::collect_cycle`): the lazy chunk
//!    collector — phase 3 of `run_gc` plus a daily backstop timer.
//!    Derives the live-chunk set from every existing manifest's
//!    `chunk_list` (fail-closed on any unparseable blob), then
//!    soft-deletes + enqueues unreferenced chunks past grace, capped
//!    per cycle with a keyset cursor carrying any backlog to the next
//!    cycle. A dry-run GC keeps this phase observation-only (shadow
//!    mode: would-collect / drift gauges, no modification).
//!
//! 4. **Drain** ([`drain::spawn_drain_task`]): background task that
//!    reads `pending_s3_deletes`, calls `ChunkBackend::delete_by_key`,
//!    deletes row on success / increments attempts on failure. Max
//!    attempts = 10 (alert-worthy after that).
//!
//! # Root seeds
//!
//! - `manifests WHERE status='uploading'` (in-flight PutPath —
//!   don't delete what's being written)
//! - `narinfo WHERE created_at > now() - grace_hours` (recent
//!   paths — don't GC something that JUST arrived before a build
//!   can reference it)
//! - `extra_roots` param (scheduler's live-build output paths —
//!   passed from `ActorCommand::GcRoots`, may not be in narinfo yet)
//! - `scheduler_live_pins` (scheduler auto-pinned live-build inputs)
//! - per-tenant retention windows (path_tenants × tenants.retention)
//!
//! # Two-phase S3 commit
//!
//! PostgreSQL deletes are transactional, S3 DeleteObject is not. The
//! collect batch enqueues S3 keys in the SAME transaction as its
//! soft-deletes; the drain issues the actual DeleteObject later. If
//! drain fails, the object leaks (storage cost) but PG state is
//! correct. Better than the reverse (S3 deleted, tx rolled back,
//! dangling chunk ref → GetPath fails).

pub mod collect;
pub mod drain;
mod mark;
#[cfg(test)]
mod mark_scan_bench;
pub mod orphan;
pub mod sweep;
pub mod tenant;

/// PG advisory lock ID for TriggerGC. Arbitrary constant — just
/// needs to not collide with other advisory locks in the schema
/// (currently the only one). "rOGC" ASCII + 1.
///
/// Serializes GC-vs-GC: two concurrent TriggerGC calls would
/// waste work and produce misleading stats. `pg_try_advisory_lock`
/// (non-blocking) — second caller gets "already running".
///
/// I-192: there is no longer a mark-vs-PutPath lock. PutPath's
/// `insert_manifest_uploading` writes `references` into the
/// placeholder narinfo at INSERT time; sweep's per-path re-check
/// (`narinfo."references" @> ARRAY[Q]`, fresh READ-COMMITTED snapshot
/// over ALL narinfo including `'uploading'`) is the sole load-bearing
/// guard. The mark lock was released before sweep anyway, so it never
/// participated in sweep-time safety — it only made PutPath wait for
/// the mark CTE, which doesn't change whether Q ends up in
/// `unreachable` or whether the re-check saves it. See
/// `r[store.gc.sweep-recheck+2]`.
pub const GC_LOCK_ID: i64 = 0x724F_4743_0001;

use std::sync::Arc;

use sqlx::{PgPool, Postgres, Transaction};
use tokio::sync::mpsc;
use tonic::Status;
use tracing::{info, warn};

use rio_proto::types::GcProgress;

use crate::backend::ChunkBackend;
use crate::manifest::{Manifest, ManifestError};

/// Summary stats from a GC run.
#[derive(Debug, Default, Clone)]
pub struct GcStats {
    /// Paths deleted from narinfo (and cascaded tables).
    pub paths_deleted: u64,
    /// Chunks soft-deleted by the collect cycle (run_gc phase 3); a
    /// dry run reports the cycle's would-collect estimate instead.
    pub chunks_deleted: u64,
    /// S3 keys enqueued to pending_s3_deletes by the collect cycle.
    pub s3_keys_enqueued: u64,
    /// Total bytes of chunks soft-deleted by the collect cycle (for
    /// storage savings estimate).
    pub bytes_freed: u64,
    /// Paths skipped because a new narinfo referenced them after
    /// mark (mark-vs-sweep race window — a PutPath completed BETWEEN
    /// mark and sweep with this path in its references). Sweep's
    /// per-path re-check catches these and skips the delete.
    /// Metric for alerting if this is frequent.
    pub paths_resurrected: u64,
}

/// Parameters for `run_gc`. Struct (not positional) so the
/// cron caller can express defaults clearly and the gRPC wrapper
/// can pass everything through without argument-order drift.
///
/// Audit C #27: was positional with `grace_hours` + `extra_roots`
/// missing — would have broken the gRPC API that accepts both.
pub struct GcParams {
    /// Compute stats, ROLLBACK sweep tx. Operator sees "would
    /// delete N paths" without committing.
    pub dry_run: bool,
    /// Paths younger than this are root seeds (don't GC what
    /// just arrived before a build can reference it). Already
    /// clamped at the gRPC boundary; clamped again in mark.rs
    /// (defense in depth).
    pub grace_hours: u32,
    /// Scheduler-populated live-build output paths. May not be
    /// in narinfo yet (worker hasn't uploaded); mark's CTE
    /// handles absent paths gracefully.
    pub extra_roots: Vec<String>,
}

/// Ceiling on `grace_hours` before the `as i32` bind. u32 > i32::MAX
/// wraps negative → `make_interval(hours => negative)` → grace covers
/// nothing → everything sweepable. One year is the practical max;
/// "infinite grace" is a `scheduler_live_pins` entry or `extra_roots`
/// pass, not a huge grace window.
pub(crate) const GRACE_HOURS_CAP: u32 = 24 * 365;

/// Mark → sweep with advisory locks. Extracted from `grpc/admin.rs::
/// trigger_gc` so it's callable outside the stream context (cron
/// reconciler in rio-controller).
///
/// Progress messages go to `progress_tx`. Send failures are ignored
/// (`let _ =`) — GC continues even if the consumer dropped. Callers
/// that don't want progress pass a channel and drop the rx.
///
/// # Advisory lock choreography
///
/// One session-scoped lock, one pool connection:
///
/// **[`GC_LOCK_ID`]** (`pg_try_advisory_lock`): serializes GC-vs-GC.
/// Held for the full run. Non-blocking — second caller gets a `false`
/// back → "already running" terminal progress msg.
///
/// Uses `scopeguard::guard(conn, |c| c.detach())` so ANY exit (error,
/// task cancellation, panic) detaches the pool connection → PG
/// auto-releases on connection close. The happy path DEFUSES the
/// guard (`ScopeGuard::into_inner`) and explicitly unlocks (cheaper
/// than detach — returns conn to pool).
///
/// There is no mark-vs-PutPath lock (I-192) — sweep's per-path
/// reference re-check is the sole concurrency guard. PutPath runs
/// freely throughout mark and sweep.
///
/// # Errors
///
/// Returns `Err(Status)` on pool-acquire/lock-query/mark/sweep failure.
/// Callers forward this into the progress stream as a terminal Err.
///
/// Returns `Ok(None)` when another GC holds [`GC_LOCK_ID`] — the
/// "already running" terminal progress message is sent, but this
/// isn't an error.
pub async fn run_gc(
    pool: &PgPool,
    chunk_backend: Option<Arc<dyn ChunkBackend>>,
    params: GcParams,
    progress_tx: mpsc::Sender<Result<GcProgress, Status>>,
    shutdown: &rio_common::signal::Token,
) -> Result<Option<GcStats>, Status> {
    // --- Concurrency guard: pg_try_advisory_lock ---
    // Two TriggerGC calls → two concurrent mark+sweep.
    // Correctness is OK (FOR UPDATE + rows_affected checks
    // in sweep) but it wastes work, produces misleading
    // stats (GC2 finds everything already swept), and
    // creates lock contention. One-at-a-time via advisory
    // lock; second caller gets an immediate "already
    // running" response.
    //
    // Session-level advisory locks are CONNECTION-scoped;
    // pool.acquire() holds one connection for lock/unlock.
    // If we let the connection return to the pool between
    // lock and unlock, the unlock would go to a DIFFERENT
    // connection → no-op, lock held until connection
    // recycles (leak). Acquiring explicitly prevents that.
    let mut lock_conn = pool.acquire().await.map_err(|e| {
        warn!(error = %e, "GC: pool acquire for advisory lock failed");
        Status::internal(format!("pool acquire: {e}"))
    })?;
    // r[impl store.gc.serialize-lock]
    let lock_acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(GC_LOCK_ID)
        .fetch_one(&mut *lock_conn)
        .await
        .map_err(|e| {
            warn!(error = %e, "GC: advisory lock query failed");
            Status::internal(format!("advisory lock: {e}"))
        })?;
    if !lock_acquired {
        info!("GC: another GC is already running, returning early");
        let _ = progress_tx
            .send(Ok(GcProgress {
                paths_scanned: 0,
                paths_collected: 0,
                bytes_freed: 0,
                is_complete: true,
                current_path: "already running (concurrent GC in progress)".into(),
            }))
            .await;
        return Ok(None);
    }
    // lock_conn held for the whole GC; explicit unlock at the end
    // via gc_unlock.
    //
    // scopeguard detaches on ANY exit not going through gc_unlock —
    // including task cancellation (client drops the stream → tonic
    // may abort a spawning task) and panics. detach() removes the
    // connection from the pool; dropping the detached connection
    // closes it → PG releases the session-scoped lock.
    //
    // Without this, cancel/panic would leave the connection in the
    // pool with the lock held → next run_gc gets "already running"
    // until sqlx recycles that pooled connection (possibly hours).
    //
    // gc_unlock DEFUSES the scopeguard (ScopeGuard::into_inner)
    // and explicitly unlocks + returns conn to pool (cheaper
    // than detach on the happy path).
    let lock_conn = scopeguard::guard(lock_conn, |c| {
        let _ = c.detach();
    });

    // --- Mark phase ---
    // No mark-vs-PutPath lock (I-192). Mark's CTE takes a point-in-time
    // MVCC snapshot; a PutPath placeholder that commits after the
    // snapshot is invisible to mark but visible to sweep's per-path
    // re-check (fresh READ-COMMITTED snapshot over ALL narinfo). The
    // re-check is the load-bearing guard; the lock added nothing on
    // top — it was released before sweep anyway.
    let unreachable =
        match mark::compute_unreachable(pool, params.grace_hours, &params.extra_roots).await {
            Ok(u) => u,
            Err(e) => {
                warn!(error = %e, "GC: mark phase failed");
                gc_unlock(lock_conn).await;
                return Err(Status::internal(format!("mark phase: {e}")));
            }
        };

    // Progress after mark: scanned count. We don't have
    // a "total paths" count cheaply (would need COUNT(*)
    // on narinfo), so paths_scanned = unreachable count
    // (what mark found). Captured here so the FINAL message
    // can report the same number — `unreachable` is moved into
    // sweep, and `stats.paths_deleted` regresses below this
    // mid-progress value when paths_resurrected > 0.
    let found_unreachable = unreachable.len() as u64;
    let _ = progress_tx
        .send(Ok(GcProgress {
            paths_scanned: found_unreachable,
            paths_collected: 0,
            bytes_freed: 0,
            is_complete: false,
            current_path: "mark complete, starting sweep".into(),
        }))
        .await;

    info!(
        unreachable = unreachable.len(),
        "GC: mark complete, starting sweep"
    );

    // --- Sweep phase ---
    // Shutdown token threaded through: sweep checks it between
    // batches (not mid-transaction — a partial batch ROLLBACKs
    // cleanly via tx drop). Returns SweepAbort::Shutdown if fired.
    let mut stats = match sweep::sweep(
        pool,
        chunk_backend.as_ref(),
        unreachable,
        params.dry_run,
        shutdown,
    )
    .await
    {
        Ok(s) => s,
        // r[impl store.gc.shutdown-abort]
        Err(sweep::SweepAbort::Shutdown) => {
            info!("GC: sweep aborted by shutdown signal");
            gc_unlock(lock_conn).await;
            return Err(Status::aborted("GC aborted: process shutting down"));
        }
        Err(sweep::SweepAbort::Db(e)) => {
            warn!(error = %e, "GC: sweep phase failed");
            gc_unlock(lock_conn).await;
            return Err(Status::internal(format!("sweep phase: {e}")));
        }
    };

    // --- Phase 3: chunk-collect cycle (the live collect arm) ---
    // Runs while GC_LOCK_ID is still held: the cycle uses its own
    // pooled connection for the session temp table; the advisory lock
    // stays on lock_conn (same split the sweep's temp table uses).
    // A dry-run GC keeps phase 3 observation-only (Shadow mode) so a
    // dry run never deletes anything; a real run collects (capped per
    // cycle, cursor-resumable). A phase-3 failure never affects the
    // path GC that just committed — log and continue; the daily
    // backstop (and the next GC run) retries. A parse-failure abort is
    // reported inside the cycle (counter + error log), not as an Err.
    let collect_mode = if params.dry_run {
        collect::CollectMode::Shadow
    } else {
        collect::CollectMode::Live
    };
    match collect::collect_cycle(
        pool,
        chunk_backend.as_ref(),
        sweep::CHUNK_GRACE_SECS,
        collect_mode,
    )
    .await
    {
        Ok(report) => {
            // P11: from the cutover release on, the chunk-level GC
            // stats (chunks deleted / bytes freed / S3 keys enqueued)
            // are sourced from the collect cycle, not the path sweep;
            // a dry run reports the would-collect estimate instead.
            match collect_mode {
                collect::CollectMode::Live => {
                    stats.chunks_deleted = report.victims_collected;
                    stats.bytes_freed = report.victim_bytes;
                    stats.s3_keys_enqueued = report.s3_keys_enqueued;
                }
                collect::CollectMode::Shadow => {
                    stats.chunks_deleted = report.would_collect;
                    stats.bytes_freed = report.would_collect_bytes;
                    stats.s3_keys_enqueued = if chunk_backend.is_some() {
                        report.would_collect
                    } else {
                        0
                    };
                }
            }
            info!(
                outcome = ?report.outcome,
                mode = ?collect_mode,
                mark_set_size = report.mark_set_size,
                would_collect = report.would_collect,
                drift_leaked = report.drift_leaked,
                drift_undercount = report.drift_undercount,
                victims_collected = report.victims_collected,
                victim_bytes = report.victim_bytes,
                s3_keys_enqueued = report.s3_keys_enqueued,
                batches_run = report.batches_run,
                cap_reached = report.cap_reached,
                cursor_at_stop = ?report.cursor_at_stop.as_deref().map(hex::encode),
                cycle_seconds = report.cycle_seconds,
                "GC: collect phase 3 complete"
            );
        }
        Err(e) => {
            // Same error-outcome accounting as the backstop caller: a
            // cycle that fails against PostgreSQL is visible immediately
            // instead of only via the 25h stalled alert.
            metrics::counter!("rio_store_gc_collect_cycles_total", "outcome" => "error")
                .increment(1);
            warn!(error = %e, "GC: collect phase 3 failed");
        }
    }

    // Final progress: complete with stats. paths_scanned echoes the
    // mid-progress `found_unreachable` so it never goes backward;
    // resurrections surface in the `current_path` summary string
    // (proto has no `paths_resurrected` field — adding one is a
    // cross-crate change deferred to keep this fix store-local).
    let _ = progress_tx
        .send(Ok(GcProgress {
            paths_scanned: found_unreachable,
            paths_collected: stats.paths_deleted,
            bytes_freed: stats.bytes_freed,
            is_complete: true,
            current_path: if params.dry_run {
                format!(
                    "dry run: would delete {} paths, {} chunks, free {} bytes, {} resurrected",
                    stats.paths_deleted,
                    stats.chunks_deleted,
                    stats.bytes_freed,
                    stats.paths_resurrected
                )
            } else {
                format!(
                    "complete: {} paths deleted, {} chunks, {} S3 keys enqueued, {} bytes freed, {} resurrected",
                    stats.paths_deleted,
                    stats.chunks_deleted,
                    stats.s3_keys_enqueued,
                    stats.bytes_freed,
                    stats.paths_resurrected
                )
            },
        }))
        .await;

    gc_unlock(lock_conn).await;
    Ok(Some(stats))
}

/// Defuse the scopeguard, explicitly release [`GC_LOCK_ID`], return
/// connection to pool. Cheaper than letting the guard fire (detach
/// closes the conn). Called on every exit path from `run_gc` that
/// reaches a `return` AFTER the lock was acquired.
async fn gc_unlock(
    conn: scopeguard::ScopeGuard<
        sqlx::pool::PoolConnection<Postgres>,
        impl FnOnce(sqlx::pool::PoolConnection<Postgres>),
    >,
) {
    let mut conn = scopeguard::ScopeGuard::into_inner(conn);
    if let Err(e) = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(GC_LOCK_ID)
        .execute(&mut *conn)
        .await
    {
        warn!(error = %e, "GC: advisory unlock failed");
    }
}

/// Enqueue S3 keys for soft-deleted chunks to `pending_s3_deletes` in
/// the given transaction. Batched via unnest — one RTT per call
/// instead of per-chunk (a 1000-chunk collect batch would otherwise
/// need 1000 INSERTs at ~1ms RTT = ~1s; batched it's ~1ms).
///
/// `blake3_hash` is written alongside `s3_key` so the drain task can
/// re-check `chunks.deleted` before issuing the S3 DELETE — catches
/// the TOCTOU where PutPath resurrected the chunk after we enqueued
/// it. `ON CONFLICT DO NOTHING`: duplicate enqueues are idempotent
/// (drain deletes the row after S3 success).
///
/// Skips hashes that fail `try_from` to `[u8; 32]` (can't-happen — the
/// `chunks` PK is BYTEA but every writer inserts exactly 32 bytes;
/// `warn!` + skip rather than panic so one corrupt row doesn't kill
/// the collect batch). Returns the number of keys attempted
/// (duplicates already enqueued are no-ops via `ON CONFLICT DO
/// NOTHING`; actual insert count may be lower).
///
/// No-op if `backend` is None (inline-only store has no S3 keys).
// r[impl store.gc.pending-deletes+2]
pub(super) async fn enqueue_chunk_deletes(
    tx: &mut Transaction<'_, Postgres>,
    soft_deleted: &[(Vec<u8>, i64)],
    backend: Option<&Arc<dyn ChunkBackend>>,
) -> Result<u64, sqlx::Error> {
    let Some(backend) = backend else {
        return Ok(0);
    };
    if soft_deleted.is_empty() {
        return Ok(0);
    }
    // r[impl store.chunk.lock-order]
    // Sort by hash before building the parallel keys/hashes vecs: the
    // input is a RETURNING set (PG internal order, NOT input-array
    // order). The pending_s3_deletes INSERT below binds UNNEST() —
    // unsorted → circular-wait against a concurrent
    // enqueue_chunk_deletes or rollback. One sort here covers all
    // callers (the collect batches). The .to_vec() clone is cheap
    // (~KB) relative to the PG roundtrip.
    let mut soft_deleted: Vec<_> = soft_deleted.to_vec();
    soft_deleted.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let mut keys: Vec<String> = Vec::with_capacity(soft_deleted.len());
    let mut hashes: Vec<Vec<u8>> = Vec::with_capacity(soft_deleted.len());
    for (hash, _size) in &soft_deleted {
        let Ok(arr) = <[u8; 32]>::try_from(hash.as_slice()) else {
            warn!(
                len = hash.len(),
                "GC: chunk hash wrong length, skipping S3 enqueue"
            );
            continue;
        };
        keys.push(backend.key_for(&arr));
        hashes.push(hash.clone());
    }
    if keys.is_empty() {
        return Ok(0);
    }
    sqlx::query(
        "INSERT INTO pending_s3_deletes (s3_key, blake3_hash) \
         SELECT * FROM unnest($1::text[], $2::bytea[]) \
         ON CONFLICT DO NOTHING",
    )
    .bind(&keys)
    .bind(&hashes)
    .execute(&mut **tx)
    .await?;
    Ok(keys.len() as u64)
}

/// Deserialize a manifest's `chunk_list` and return its dedup'd chunk
/// hashes, sorted ascending. A manifest CAN repeat chunks (duplicate
/// content blocks in the NAR) — each unique hash appears exactly once.
///
/// Corrupt input (anything `Manifest::deserialize` rejects) is an
/// `Err`, never an empty `Ok`, so a caller that must fail closed (a
/// collector mark phase that derives liveness from the manifest fold)
/// can distinguish "this manifest references no chunks" from "this
/// manifest is unreadable". An empty manifest (zero entries) is NOT
/// corrupt: it parses to `Ok` of an empty vec.
///
/// The ascending sort gives a deterministic order independent of the
/// manifest's entry order; the consumers are order-insensitive
/// ([`decrement_hashes`] re-sorts its input).
pub(crate) fn try_parse_unique_chunk_hashes(
    chunk_list: &[u8],
) -> Result<Vec<[u8; 32]>, ManifestError> {
    let manifest = Manifest::deserialize(chunk_list)?;
    let mut hashes: Vec<[u8; 32]> = manifest.entries.into_iter().map(|e| e.hash).collect();
    hashes.sort_unstable();
    hashes.dedup();
    Ok(hashes)
}

/// Infallible wrapper around `try_parse_unique_chunk_hashes` for the
/// legacy decrement paths (today: the placeholder reapers' write-only
/// decrement): a corrupt `chunk_list` is logged and yields empty (the
/// narinfo DELETE has already CASCADEd the manifest away; worst case
/// is a stale counter — collection is unaffected because the collector
/// derives liveness from the manifest fold, not the counter). This
/// warn-and-empty behavior is the as-built C12 skip polarity,
/// deliberately preserved at the legacy callsites until they are
/// deleted; a collector that derives liveness from the manifest fold
/// must NOT use this wrapper — treating corrupt input as "references
/// nothing" would turn a storage leak into collected live data, so its
/// mark phase aborts the cycle on `Err` instead.
pub(super) fn parse_unique_chunk_hashes(chunk_list: &[u8]) -> Vec<[u8; 32]> {
    try_parse_unique_chunk_hashes(chunk_list).unwrap_or_else(|e| {
        warn!(error = %e, "GC: corrupt chunk_list, skipping decrement");
        Vec::new()
    })
}

/// Write-only refcount decrement for the placeholder reap paths: takes
/// pre-deduped hashes paired with per-hash decrement counts and issues
/// the single by-count UPDATE — no zero-detect, no soft-delete, no
/// outbox enqueue. The counter stays maintained (write-only) for
/// mixed-fleet safety while pre-cutover pods may still read it; chunks
/// a reaped manifest leaves unreferenced are collected by the next
/// collect cycle, which never consults the counter. The decrement (and
/// the upsert increment it mirrors) is deleted wholesale in the
/// writer-removal release.
///
/// Runs inside an EXISTING transaction — caller is responsible for
/// begin/commit/rollback, and a caller-provided transaction is also
/// why this CANNOT use [`crate::metadata::with_sorted_retry`] (retry
/// would need to replay the whole outer txn). The sort below keeps a
/// deterministic lock order; PG drives the `unnest` join through a
/// btree scan regardless, so it is defense-in-depth (see
/// `reap_decrement_no_deadlock`).
// r[impl store.chunk.lock-order]
pub(super) async fn decrement_hashes(
    tx: &mut Transaction<'_, Postgres>,
    unique_hashes: &[Vec<u8>],
    counts: &[i64],
) -> Result<(), sqlx::Error> {
    debug_assert_eq!(unique_hashes.len(), counts.len());
    if unique_hashes.is_empty() {
        return Ok(());
    }

    let mut pairs: Vec<(Vec<u8>, i64)> = unique_hashes
        .iter()
        .cloned()
        .zip(counts.iter().copied())
        .collect();
    pairs.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let (unique_hashes, counts): (Vec<Vec<u8>>, Vec<i64>) = pairs.into_iter().unzip();

    // Decrement by count: a chunk referenced N times by the deleted
    // manifests is decremented by N. unnest preserves single-statement
    // semantics (one btree-scan-order lock acquisition). The M_023
    // CHECK still rejects a decrement below zero until Release B drops
    // it — a violation here is a caller bug (wrong hashes or a double
    // decrement), surfaced loudly rather than silently corrupting the
    // write-only counter.
    sqlx::query(
        r#"
        UPDATE chunks c SET refcount = c.refcount - d.n
          FROM unnest($1::bytea[], $2::bigint[]) AS d(h, n)
         WHERE c.blake3_hash = d.h
        "#,
    )
    .bind(&unique_hashes)
    .bind(&counts)
    .execute(&mut **tx)
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Manifest, ManifestEntry};
    use crate::test_helpers::{ChunkSeed, mem_backend};
    use rio_test_support::TestDb;
    use sqlx::PgPool;

    /// Seed a chunk row with the given refcount. Returns the blake3 hash.
    async fn seed_chunk(pool: &PgPool, tag: u8, refcount: i32, size: i64) -> [u8; 32] {
        ChunkSeed::new(tag)
            .with_refcount(refcount)
            .with_size(size)
            .seed(pool)
            .await
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

    /// Every corrupt class `Manifest::deserialize` rejects surfaces as
    /// `Err` from the fallible parse — never as an empty `Ok`.
    #[test]
    fn try_parse_rejects_corrupt_chunk_list() {
        use crate::manifest::{MAX_CHUNKS, ManifestError};

        // Empty input (no version byte).
        assert!(matches!(
            try_parse_unique_chunk_hashes(b""),
            Err(ManifestError::Empty)
        ));

        // Unknown version byte.
        assert!(matches!(
            try_parse_unique_chunk_hashes(&[0xFF]),
            Err(ManifestError::UnknownVersion(0xFF))
        ));

        // Body length not a multiple of the entry stride (truncated).
        let mut truncated = make_manifest(&[[0x11u8; 32]]);
        truncated.pop();
        assert!(matches!(
            try_parse_unique_chunk_hashes(&truncated),
            Err(ManifestError::BadLength(_))
        ));

        // Entry count above MAX_CHUNKS.
        let mut oversized = vec![0u8; 1 + (MAX_CHUNKS + 1) * 36];
        oversized[0] = 1;
        assert!(matches!(
            try_parse_unique_chunk_hashes(&oversized),
            Err(ManifestError::TooManyChunks(_))
        ));
    }

    /// Duplicate hashes collapse to one occurrence each and the result
    /// is sorted ascending (the deterministic order the callers and the
    /// future mark batches rely on), regardless of entry order.
    #[test]
    fn try_parse_dedups_and_sorts_hashes() {
        let a = [0x01u8; 32];
        let b = [0x02u8; 32];
        let c = [0x03u8; 32];
        let manifest = make_manifest(&[c, a, c, b, a, c]);

        let hashes = try_parse_unique_chunk_hashes(&manifest).unwrap();
        assert_eq!(hashes, vec![a, b, c], "deduped, ascending");
    }

    /// An empty manifest (zero entries) is well-formed, not corrupt:
    /// it parses to Ok of an empty set.
    #[test]
    fn try_parse_empty_manifest_is_ok_and_empty() {
        let empty = make_manifest(&[]);
        assert!(try_parse_unique_chunk_hashes(&empty).unwrap().is_empty());
    }

    /// The legacy infallible wrapper keeps the as-built C12 skip
    /// polarity: corrupt input yields an empty set (warn + skip the
    /// decrement), it does not error or panic. Pinned so the polarity
    /// is not "fixed" out from under the callsites that rely on it.
    #[test]
    fn parse_unique_chunk_hashes_keeps_corrupt_skip_polarity() {
        assert!(parse_unique_chunk_hashes(&[0xFF]).is_empty());
        assert!(parse_unique_chunk_hashes(b"").is_empty());

        // And the happy path still parses through the wrapper.
        let h = [0x42u8; 32];
        assert_eq!(parse_unique_chunk_hashes(&make_manifest(&[h, h])), vec![h]);
    }

    /// The reap-path decrement is write-only: refcounts go down by the
    /// requested counts, but nothing is soft-deleted and nothing is
    /// enqueued — even when a chunk reaches zero. (Zero-references is
    /// the collect cycle's business, not the reaper's.) Empty input is
    /// a no-op.
    #[tokio::test]
    async fn reap_decrement_is_write_only() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let h1 = seed_chunk(&db.pool, 1, 2, 1000).await;
        let h2 = seed_chunk(&db.pool, 2, 1, 2000).await;

        // Empty input: no-op, no error.
        let mut tx = db.pool.begin().await.unwrap();
        decrement_hashes(&mut tx, &[], &[]).await.unwrap();
        tx.commit().await.unwrap();

        let mut tx = db.pool.begin().await.unwrap();
        decrement_hashes(&mut tx, &[h1.to_vec(), h2.to_vec()], &[1, 1])
            .await
            .unwrap();
        tx.commit().await.unwrap();

        // h1: 2→1. h2: 1→0 — and even at zero it is NOT soft-deleted
        // and NOT enqueued (write-only).
        let rows: Vec<(Vec<u8>, i32, bool)> = sqlx::query_as(
            "SELECT blake3_hash, refcount, deleted FROM chunks ORDER BY blake3_hash",
        )
        .fetch_all(&db.pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].1, 1, "h1 refcount 2→1");
        assert!(!rows[0].2);
        assert_eq!(rows[1].1, 0, "h2 refcount 1→0");
        assert!(
            !rows[1].2,
            "zero-refcount chunk is NOT soft-deleted by the reaper"
        );
        let enqueued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_s3_deletes")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(enqueued, 0, "the reaper never writes the outbox");
    }

    /// enqueue_chunk_deletes: a hash that isn't 32 bytes is skipped
    /// with a warn, not a panic. The well-formed siblings in the same
    /// batch still enqueue. (Can't-happen in practice — chunks PK writers
    /// all insert 32 bytes — but warn+skip beats killing the collect
    /// batch.)
    #[tokio::test]
    // r[verify store.gc.pending-deletes+2]
    async fn enqueue_skips_corrupt_hash() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let backend: Arc<dyn ChunkBackend> = mem_backend();
        let mut tx = db.pool.begin().await.unwrap();

        // One well-formed (32 bytes), one corrupt (7 bytes).
        let good = vec![0xAAu8; 32];
        let bad = vec![0xBBu8; 7];
        let zeroed = vec![(good.clone(), 100i64), (bad, 50i64)];

        let enqueued = enqueue_chunk_deletes(&mut tx, &zeroed, Some(&backend))
            .await
            .unwrap();
        tx.commit().await.unwrap();

        // Only the well-formed one enqueued.
        assert_eq!(enqueued, 1);
        let rows: Vec<(Vec<u8>,)> = sqlx::query_as("SELECT blake3_hash FROM pending_s3_deletes")
            .fetch_all(&db.pool)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, good);
    }

    /// Migration 023's CHECK catches double-decrement at the source:
    /// an `UPDATE SET refcount = refcount - 1` that would take a
    /// chunk negative raises a constraint violation instead of
    /// silently leaking (a negative refcount never matches
    /// `WHERE refcount = 0` → chunk never GC'd).
    ///
    /// Replaces the old `idempotent_enqueue_on_conflict` test, which
    /// reached its assertion by decrementing the same manifest's
    /// chunks twice — silently driving refcount 0→-1.
    /// That test claimed to exercise the INSERT's `ON CONFLICT DO
    /// NOTHING`, but `pending_s3_deletes` has no unique constraint
    /// on `s3_key` or `blake3_hash` (only `id BIGSERIAL PK`,
    /// migrations/005) so the ON CONFLICT never actually fired. The
    /// "no duplicate" it observed came from the `deleted = false`
    /// filter in RETURNING, not the ON CONFLICT. The scenario it
    /// modeled ("orphan scan after GC already swept") can't happen
    /// in practice either: orphan scanner targets status='uploading'
    /// manifests, GC sweep targets completed narinfo paths —
    /// mutually exclusive.
    #[tokio::test]
    async fn double_decrement_rejected_by_check() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let h1 = seed_chunk(&db.pool, 1, 1, 1000).await;

        // First decrement: 1→0, fine.
        let mut tx = db.pool.begin().await.unwrap();
        decrement_hashes(&mut tx, &[h1.to_vec()], &[1])
            .await
            .unwrap();
        tx.commit().await.unwrap();

        // Second decrement: 0→-1, CHECK fires (until Release B drops it).
        let mut tx = db.pool.begin().await.unwrap();
        let err = decrement_hashes(&mut tx, &[h1.to_vec()], &[1])
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("chunks_refcount_nonneg"),
            "expected CHECK constraint violation, got: {err}"
        );
    }

    /// bug_304: final `GcProgress.paths_scanned` MUST equal the
    /// mid-progress value (`found_unreachable`), never regress to
    /// `stats.paths_deleted`. Resurrection (which makes the two
    /// diverge) requires a write landing between mark's snapshot and
    /// sweep's re-check — not deterministically reachable through
    /// `run_gc` in a unit test — so this pins the contract on a
    /// non-resurrecting run and asserts the summary string carries
    /// the resurrected count (the second half of the fix).
    #[tokio::test]
    async fn run_gc_final_paths_scanned_monotone() {
        use crate::test_helpers::StoreSeed;

        let db = TestDb::new(&crate::MIGRATOR).await;
        StoreSeed::path("monotone-a")
            .created_hours_ago(48)
            .seed(&db.pool)
            .await;
        StoreSeed::path("monotone-b")
            .created_hours_ago(48)
            .seed(&db.pool)
            .await;

        let (tx, mut rx) = mpsc::channel(8);
        let stats = run_gc(
            &db.pool,
            None,
            GcParams {
                dry_run: false,
                grace_hours: 2,
                extra_roots: vec![],
            },
            tx,
            &rio_common::signal::Token::new(),
        )
        .await
        .unwrap()
        .unwrap();

        let mut msgs = Vec::new();
        while let Some(m) = rx.recv().await {
            msgs.push(m.unwrap());
        }
        assert!(msgs.len() >= 2, "mid + final");
        let mid = &msgs[0];
        let fin = msgs.last().unwrap();
        assert!(fin.is_complete);
        assert_eq!(mid.paths_scanned, 2, "mark found both");
        assert_eq!(
            fin.paths_scanned, mid.paths_scanned,
            "final paths_scanned echoes found_unreachable, not paths_deleted"
        );
        assert!(fin.paths_scanned >= fin.paths_collected);
        assert!(
            fin.current_path.contains("0 resurrected"),
            "resurrections surfaced in summary: {}",
            fin.current_path
        );
        assert_eq!(stats.paths_deleted, 2);
    }

    /// I-192 liveness: `run_gc` (full mark+sweep orchestration)
    /// concurrent with a burst of `insert_manifest_uploading` calls.
    /// Every insert MUST succeed — GC never blocks PutPath. This IS
    /// the I-168/I-192 user-facing symptom: before this change, the
    /// inserts would block on `GC_MARK_LOCK_ID` shared and return
    /// `GcMarkBusy` → gRPC `Aborted` after the retry budget.
    ///
    /// `multi_thread`: GC and the insert burst must actually
    /// interleave on separate executor threads.
    ///
    /// Safety (re-check correctness) is proven deterministically by
    /// [`gc_mark_then_insert_then_sweep_preserves_referenced`] below
    /// and `sweep::tests::sweep_recheck_sees_uploading_placeholder`;
    /// a free-running race here can't distinguish "P_i committed
    /// before re-check" from "P_i committed after Q_i's DELETE"
    /// (the latter is a legitimate post-GC dangling ref, not a bug).
    #[tokio::test(flavor = "multi_thread")]
    async fn run_gc_concurrent_with_placeholder_inserts_liveness() {
        use crate::test_helpers::{StoreSeed, path_hash};
        use rio_test_support::fixtures::test_store_path;

        const N: usize = 100;
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Seed N old, unrooted targets Q_i (48h, past grace=2h) so GC
        // has real mark+sweep work to do while inserts run.
        let mut targets = Vec::with_capacity(N);
        for i in 0..N {
            let q = test_store_path(&format!("i192-live-target-{i:03}"));
            StoreSeed::raw_path(&q)
                .created_hours_ago(48)
                .seed(&db.pool)
                .await;
            targets.push(q);
        }

        // GC task: full run_gc (GC_LOCK_ID + mark + sweep).
        let pool_gc = db.pool.clone();
        let (tx, mut rx) = mpsc::channel(64);
        let gc = tokio::spawn(async move {
            let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
            let stats = run_gc(
                &pool_gc,
                None,
                GcParams {
                    dry_run: false,
                    grace_hours: 2,
                    extra_roots: vec![],
                },
                tx,
                &rio_common::signal::Token::new(),
            )
            .await
            .expect("run_gc");
            drain.await.ok();
            stats
        });

        // Insert burst: N placeholders P_i with refs=[Q_i], concurrent
        // with GC. Each insert MUST succeed — no lock to contend on.
        let mut insert_tasks = Vec::with_capacity(N);
        for (i, q) in targets.iter().cloned().enumerate() {
            let pool = db.pool.clone();
            insert_tasks.push(tokio::spawn(async move {
                let p = test_store_path(&format!("i192-live-uploader-{i:03}"));
                crate::metadata::insert_manifest_uploading(&pool, &path_hash(&p), &p, &[q])
                    .await
                    .expect("insert_manifest_uploading must not fail under concurrent GC")
            }));
        }
        for t in insert_tasks {
            assert!(
                t.await.unwrap().is_some(),
                "fresh path → placeholder inserted"
            );
        }

        let stats = gc.await.unwrap().expect("GC_LOCK_ID free → Some(stats)");
        // Accounting sanity: sweep saw at most N candidates.
        assert!(
            stats.paths_deleted + stats.paths_resurrected <= N as u64,
            "stats out of bounds: {stats:?}"
        );
    }

    /// I-192 safety, deterministic: glue `compute_unreachable` →
    /// concurrent placeholder inserts → `sweep` so the inserts land
    /// PRECISELY in the mark-snapshot/sweep-recheck window the removed
    /// lock used to close. Asserts every target survives via the
    /// re-check alone. This is the end-to-end form of
    /// `mark::tests::placeholder_refs_protect_closure` (mark side) +
    /// `sweep::tests::sweep_recheck_sees_uploading_placeholder` (sweep
    /// side) at N=100 with real concurrency on the insert burst.
    // r[verify store.gc.sweep-recheck+2]
    // r[verify store.put.placeholder-refs]
    #[tokio::test(flavor = "multi_thread")]
    async fn gc_mark_then_insert_then_sweep_preserves_referenced() {
        use crate::test_helpers::{StoreSeed, path_hash};
        use rio_test_support::fixtures::test_store_path;

        const N: usize = 100;
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Seed N old, unrooted targets Q_i.
        let mut targets = Vec::with_capacity(N);
        for i in 0..N {
            let q = test_store_path(&format!("i192-safe-target-{i:03}"));
            let h = StoreSeed::raw_path(&q)
                .created_hours_ago(48)
                .seed(&db.pool)
                .await;
            targets.push((q, h));
        }

        // T0: mark snapshot. All Q_i unreachable (no P_i exists yet).
        let unreachable = mark::compute_unreachable(&db.pool, 2, &[]).await.unwrap();
        assert_eq!(unreachable.len(), N, "all targets unreachable pre-insert");

        // T1: 100 concurrent placeholder inserts P_i refs=[Q_i]. All
        // commit AFTER mark's snapshot, BEFORE sweep — the exact window.
        let mut insert_tasks = Vec::with_capacity(N);
        for (i, (q, _)) in targets.iter().cloned().enumerate() {
            let pool = db.pool.clone();
            insert_tasks.push(tokio::spawn(async move {
                let p = test_store_path(&format!("i192-safe-uploader-{i:03}"));
                crate::metadata::insert_manifest_uploading(&pool, &path_hash(&p), &p, &[q])
                    .await
                    .expect("insert must succeed (no GC lock to contend on)")
            }));
        }
        for t in insert_tasks {
            assert!(t.await.unwrap().is_some());
        }

        // T2: sweep with mark's stale unreachable list. Re-check must
        // resurrect EVERY Q_i.
        let stats = sweep::sweep(
            &db.pool,
            None,
            unreachable,
            false,
            &rio_common::signal::Token::new(),
        )
        .await
        .unwrap();
        assert_eq!(stats.paths_deleted, 0, "no referenced path may be swept");
        assert_eq!(
            stats.paths_resurrected, N as u64,
            "every target resurrected by re-check"
        );

        // No dangling references anywhere.
        let dangling: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*) FROM narinfo n
             CROSS JOIN LATERAL unnest(n."references") r
             WHERE NOT EXISTS (SELECT 1 FROM narinfo n2 WHERE n2.store_path = r)
            "#,
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(dangling, 0, "no placeholder may reference a swept path");

        // Every Q_i's narinfo still exists.
        for (_, h) in &targets {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM narinfo WHERE store_path_hash = $1)",
            )
            .bind(h)
            .fetch_one(&db.pool)
            .await
            .unwrap();
            assert!(exists, "Q's narinfo must survive sweep");
        }
    }

    /// Regression: the concurrent reap-path decrement
    /// (`decrement_hashes`) vs another chunk writer on overlapping
    /// hashes MUST NOT deadlock.
    ///
    /// `decrement_hashes` runs inside a CALLER-provided txn so
    /// it cannot use `with_sorted_retry`; its inline sort is the
    /// lock-order discipline.
    /// The per-row contender stands in for any other chunk writer
    /// obeying r\[store.chunk.lock-order\] — it row-locks in iteration
    /// order, so its `with_sorted_retry` wrapper is what makes its
    /// order canonical. With both sides ascending, no circular wait.
    ///
    /// Mutation-tested: removing `with_sorted_retry`'s sort (the
    /// contender's discipline) makes the contender lock descending
    /// while `decrement_hashes` locks PK-ascending → 40P01 →
    /// attempts==3 → fails here. Removing the inline sort at this
    /// function's annotated site is NOT independently observable:
    /// both its UPDATEs are batch `= ANY($1)` which PG evaluates in
    /// scan order regardless of array order (btree presort) — same
    /// root insight as `rollback_overlapping_no_deadlock`. The inline
    /// sort is discipline (defends a future per-row refactor), not
    /// load-bearing under the current batch-ANY shape.
    // r[verify store.chunk.lock-order]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reap_decrement_no_deadlock() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;
        use tokio::time::timeout;

        let db = TestDb::new(&crate::MIGRATOR).await;

        // Seed 100 chunks at refcount=2 — both sides decrement once
        // without tripping CHECK(refcount>=0). 100 hashes so the
        // per-row contender's 100 sequential roundtrips overlap the
        // batch UPDATE. Raw INSERT (ChunkSeed's hash is `[tag,0..]`;
        // we need `[i;32]` to match `make_manifest`).
        let hashes: Vec<[u8; 32]> = (0u8..100).map(|i| [i; 32]).collect();
        for h in &hashes {
            sqlx::query("INSERT INTO chunks (blake3_hash, refcount, size) VALUES ($1, 2, 1024)")
                .bind(&h[..])
                .execute(&db.pool)
                .await
                .unwrap();
        }
        let hashes: Vec<Vec<u8>> = hashes.into_iter().map(|h| h.to_vec()).collect();

        // Per-row contender: locks in `sorted` ARRAY order. No-op
        // write — row-locks unconditionally.
        async fn contend_per_row(pool: &PgPool, sorted: &[Vec<u8>]) -> crate::metadata::Result<()> {
            let mut tx = pool.begin().await?;
            for h in sorted {
                sqlx::query("UPDATE chunks SET size = size WHERE blake3_hash = $1")
                    .bind(h)
                    .execute(&mut *tx)
                    .await?;
            }
            tx.commit().await?;
            Ok(())
        }

        // Side A: the production reap-path decrement inside a fresh
        // txn, wrapped in retry-once-on-40P01. The hashes vec passed
        // to with_sorted_retry is the SAME set the reaper derives from
        // the manifest — the helper's sort is a no-op for side A's
        // lock order (batch ANY locks scan-order); its role here is
        // the retry + attempt counter.
        // Side B: per-row contender fed REVERSED.
        let hashes_fwd = hashes.clone();
        let mut hashes_rev = hashes.clone();
        hashes_rev.reverse();

        let pool_a = db.pool.clone();
        let pool_b = db.pool.clone();
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_a = Arc::clone(&attempts);
        let attempts_b = Arc::clone(&attempts);

        let task_a = tokio::spawn(async move {
            crate::metadata::with_sorted_retry(hashes_fwd, move |sorted| {
                attempts_a.fetch_add(1, Ordering::Relaxed);
                let pool_a = pool_a.clone();
                async move {
                    let mut tx = pool_a.begin().await?;
                    let counts = vec![1i64; sorted.len()];
                    decrement_hashes(&mut tx, &sorted, &counts).await?;
                    tx.commit().await?;
                    Ok(())
                }
            })
            .await
        });
        let task_b = tokio::spawn(async move {
            crate::metadata::with_sorted_retry(hashes_rev, move |sorted| {
                attempts_b.fetch_add(1, Ordering::Relaxed);
                let pool_b = pool_b.clone();
                async move { contend_per_row(&pool_b, &sorted).await }
            })
            .await
        });

        let (ra, rb) = timeout(Duration::from_secs(5), async {
            tokio::try_join!(task_a, task_b).expect("tasks should not panic")
        })
        .await
        .expect("concurrent decrement+contender must complete within 5s — deadlock detected");

        ra.expect("the reap decrement should succeed");
        rb.expect("contender should succeed");

        // Mutation sentinel: with both sides obeying lock-order
        // discipline → no 40P01 → no retry → exactly 2 body
        // invocations total.
        assert_eq!(
            attempts.load(Ordering::Relaxed),
            2,
            "lock-order discipline should prevent 40P01 (no retry needed)"
        );

        // Vacuity sentinel: the decrement UPDATE must have matched all
        // 100 (refcount 2→1). If a future seed regression makes the
        // UPDATE match zero rows, this fails loudly instead of going
        // vacuous.
        let sum: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(refcount),0) FROM chunks WHERE blake3_hash = ANY($1)",
        )
        .bind(&hashes)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(sum, 100, "decrement matched all 100 chunks (2→1)");
    }
}
