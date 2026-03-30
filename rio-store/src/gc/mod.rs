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

use sqlx::{PgPool, Postgres, Transaction};
use tokio::sync::mpsc;
use tonic::Status;
use tracing::{info, warn};

use rio_proto::types::GcProgress;

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

/// Parameters for [`run_gc`]. Struct (not positional) so the
/// cron caller can express defaults clearly and the gRPC wrapper
/// can pass everything through without argument-order drift.
///
/// Audit C #27: was positional with `grace_hours` + `extra_roots`
/// missing — would have broken the gRPC API that accepts both.
pub struct GcParams {
    /// Compute stats, ROLLBACK sweep tx. Operator sees "would
    /// delete N paths" without committing.
    pub dry_run: bool,
    /// Skip the empty-refs safety gate. Logged at `warn!` when
    /// true so the override is visible.
    pub force: bool,
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

/// Default threshold for the empty-refs safety gate. 10% is
/// intentionally low: in a healthy post-fix store, empty-ref non-CA
/// paths should be ~0% (only genuinely ref-free outputs like static
/// binaries). 10% gives headroom for legitimate cases without allowing
/// a pre-fix store (where it'd be ~100%) through.
const GC_EMPTY_REFS_THRESHOLD_PCT: f64 = 10.0;

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
/// Two session-scoped locks, two pool connections:
///
/// 1. **[`GC_LOCK_ID`]** (outer, `pg_try_advisory_lock`): serializes
///    GC-vs-GC. Held for the full run. Non-blocking — second caller
///    gets a `false` back → "already running" terminal progress msg.
///
/// 2. **[`GC_MARK_LOCK_ID`]** (inner, `pg_advisory_lock`): exclusive
///    against PutPath's shared lock around the placeholder insert.
///    Held ONLY for `compute_unreachable` (~1s CTE), released BEFORE
///    sweep so PutPath isn't blocked during the longer sweep phase.
///    Sweep's per-path re-check catches any race that slips through
///    the window between mark-release and sweep-start.
///
/// Both use `scopeguard::guard(conn, |c| c.detach())` so ANY exit
/// (error, task cancellation, panic) detaches the pool connection →
/// PG auto-releases on connection close. The happy path DEFUSES the
/// guard (`ScopeGuard::into_inner`) and explicitly unlocks (cheaper
/// than detach — returns conn to pool).
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

    // r[impl store.gc.empty-refs-gate]
    // Safety gate: refuse if >threshold% of sweep-eligible narinfo
    // have empty refs (pre-refscan data). Runs AFTER advisory lock
    // so two gated requests don't race on the check. Skip if
    // force=true, but still log so the override is visible.
    if !params.force {
        if let Err(e) =
            check_empty_refs_gate(pool, params.grace_hours, GC_EMPTY_REFS_THRESHOLD_PCT).await
        {
            gc_unlock(lock_conn).await;
            return Err(e);
        }
    } else {
        warn!("GC: force=true — bypassing empty-refs safety gate");
    }

    // --- Mark phase ---
    // Mark-vs-PutPath lock: take GC_MARK_LOCK_ID EXCLUSIVE for the mark
    // CTE only (~1s for typical store). PutPath takes this SHARED,
    // transaction-scoped — only ~ms around the placeholder insert
    // (NOT held for the full upload; the placeholder narinfo carries
    // its references from commit, so the upload itself is unprotected
    // by design). Mark blocks until no placeholder-insert tx is in
    // flight. This guarantees the reference graph seen by mark is
    // consistent: no PutPath can add a new reference to a path mark
    // is about to declare dead.
    //
    // Uses a SEPARATE connection (mark_lock_conn) from
    // lock_conn (GC_LOCK_ID) — both are session-scoped, but
    // keeping them on separate connections means we can drop
    // mark_lock_conn immediately after mark returns (releasing
    // the PutPath-blocking lock early) while GC_LOCK_ID stays
    // held through sweep.
    let mark_lock_conn = pool.acquire().await.map_err(|e| {
        warn!(error = %e, "GC: pool acquire for mark lock failed");
        Status::internal(format!("mark lock acquire: {e}"))
    });
    let mark_lock_conn = match mark_lock_conn {
        Ok(c) => c,
        Err(e) => {
            gc_unlock(lock_conn).await;
            return Err(e);
        }
    };
    // scopeguard: if mark fails or task is cancelled, the
    // connection is DETACHED (not returned to pool) → PG
    // auto-releases the session-scoped lock on connection
    // close. On success we explicitly unlock + defuse below.
    let mut mark_lock_guard = scopeguard::guard(mark_lock_conn, |c| {
        // detach() removes from pool; dropping the detached
        // Connection closes it → PG releases session lock.
        let _ = c.detach();
    });

    // Acquire exclusive. Blocks until no PutPath holds shared.
    if let Err(e) = sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(GC_MARK_LOCK_ID)
        .execute(&mut **mark_lock_guard)
        .await
    {
        warn!(error = %e, "GC: mark advisory lock query failed");
        gc_unlock(lock_conn).await;
        return Err(Status::internal(format!("mark lock: {e}")));
        // mark_lock_guard detached by scopeguard
    }

    let unreachable =
        match mark::compute_unreachable(pool, params.grace_hours, &params.extra_roots).await {
            Ok(u) => u,
            Err(e) => {
                warn!(error = %e, "GC: mark phase failed");
                gc_unlock(lock_conn).await;
                return Err(Status::internal(format!("mark phase: {e}")));
                // mark_lock_guard detached by scopeguard
            }
        };

    // Mark done — release the mark lock EXPLICITLY (early,
    // before sweep) and defuse the scopeguard. PutPath can now
    // proceed; sweep's per-path re-check handles any race.
    let mut mark_lock_conn = scopeguard::ScopeGuard::into_inner(mark_lock_guard);
    if let Err(e) = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(GC_MARK_LOCK_ID)
        .execute(&mut *mark_lock_conn)
        .await
    {
        warn!(error = %e, "GC: mark advisory unlock failed (continuing — conn drop will release)");
    }
    drop(mark_lock_conn); // returns to pool (lock already released)

    // Progress after mark: scanned count. We don't have
    // a "total paths" count cheaply (would need COUNT(*)
    // on narinfo), so paths_scanned = unreachable count
    // (what mark found). Not ideal but informative.
    let _ = progress_tx
        .send(Ok(GcProgress {
            paths_scanned: unreachable.len() as u64,
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
    let stats = match sweep::sweep(
        pool,
        chunk_backend.as_ref(),
        unreachable,
        params.dry_run,
        shutdown,
    )
    .await
    {
        Ok(s) => s,
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

    // Final progress: complete with stats.
    let _ = progress_tx
        .send(Ok(GcProgress {
            paths_scanned: stats.paths_deleted, // reuse for "found unreachable"
            paths_collected: stats.paths_deleted,
            bytes_freed: stats.bytes_freed,
            is_complete: true,
            current_path: if params.dry_run {
                format!(
                    "dry run: would delete {} paths, {} chunks, free {} bytes",
                    stats.paths_deleted, stats.chunks_deleted, stats.bytes_freed
                )
            } else {
                format!(
                    "complete: {} paths deleted, {} chunks, {} S3 keys enqueued, {} bytes freed",
                    stats.paths_deleted,
                    stats.chunks_deleted,
                    stats.s3_keys_enqueued,
                    stats.bytes_freed
                )
            },
        }))
        .await;

    gc_unlock(lock_conn).await;
    Ok(Some(stats))
}

/// Defuse the scopeguard, explicitly release [`GC_LOCK_ID`], return
/// connection to pool. Cheaper than letting the guard fire (detach
/// closes the conn). Called on every exit path from [`run_gc`] that
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

/// Safety gate: if more than `threshold_pct`% of COMPLETE narinfo rows
/// older than `grace_hours` have empty references AND no content
/// address, refuse GC. Protects against running GC on pre-refscan data
/// (worker upload.rs bug where references were never populated).
///
/// CA paths are excluded from the numerator (legitimately ref-free).
/// Paths inside the grace window are excluded entirely (they're
/// protected anyway; their ref-state doesn't matter for this sweep).
///
/// Schema note: narinfo column is `ca` (not `content_address`), and
/// `"references"` is a TEXT[] — empty array is `'{}'` in PG.
// r[impl store.gc.empty-refs-gate]
async fn check_empty_refs_gate(
    pool: &PgPool,
    grace_hours: u32,
    threshold_pct: f64,
) -> Result<(), Status> {
    let row: (i64, i64) = sqlx::query_as(
        r#"
        SELECT
            count(*) FILTER (
                WHERE n."references" = '{}'
                  AND (n.ca IS NULL OR n.ca = '')
            ) AS empty_ref_non_ca,
            count(*) AS total
        FROM narinfo n
        JOIN manifests m USING (store_path_hash)
        WHERE m.status = 'complete'
          AND n.created_at < now() - make_interval(hours => $1::int)
        "#,
    )
    .bind(grace_hours as i32)
    .fetch_one(pool)
    .await
    .map_err(|e| {
        // Don't leak sqlx chain to client; log full detail server-side.
        warn!(error = %e, "empty-refs gate query failed");
        Status::internal("empty-refs gate query failed")
    })?;

    let (empty, total) = row;
    if total == 0 {
        return Ok(()); // nothing sweep-eligible anyway
    }
    let pct = (empty as f64 / total as f64) * 100.0;
    metrics::gauge!("rio_store_gc_empty_refs_pct").set(pct);

    if pct > threshold_pct {
        tracing::error!(
            empty_ref_non_ca = empty,
            total,
            pct,
            threshold_pct,
            "GC REFUSED: high empty-refs ratio — store likely contains pre-refscan data"
        );
        return Err(Status::failed_precondition(format!(
            "GC refused: {empty}/{total} ({pct:.1}%) of sweep-eligible paths have empty \
             references (threshold {threshold_pct}%) — worker upload.rs bug? \
             Run backfill first or use force=true to override."
        )));
    }

    if empty > 0 {
        warn!(
            empty_ref_non_ca = empty,
            total, pct, "GC proceeding with some empty-ref paths (below threshold)"
        );
    }
    Ok(())
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

/// Enqueue S3 keys for zeroed chunks to `pending_s3_deletes` in the
/// given transaction. Batched via unnest — one RTT per call instead
/// of per-chunk (a 1000-chunk manifest would otherwise need 1000
/// INSERTs at ~1ms RTT = ~1s; batched it's ~1ms).
///
/// `blake3_hash` is written alongside `s3_key` so the drain task can
/// re-check `chunks.(deleted AND refcount=0)` before issuing the S3
/// DELETE — catches the TOCTOU where PutPath resurrected the chunk
/// after we enqueued it. `ON CONFLICT DO NOTHING`: duplicate enqueues
/// are idempotent (drain deletes the row after S3 success).
///
/// Skips hashes that fail `try_from` to `[u8; 32]` (can't-happen — the
/// `chunks` PK is BYTEA but every writer inserts exactly 32 bytes;
/// `warn!` + skip rather than panic so one corrupt row doesn't kill
/// the sweep). Returns the number of keys attempted (duplicates already
/// enqueued are no-ops via `ON CONFLICT DO NOTHING`; actual insert
/// count may be lower).
///
/// No-op if `backend` is None (inline-only store has no S3 keys).
// r[impl store.gc.pending-deletes]
pub(super) async fn enqueue_chunk_deletes(
    tx: &mut Transaction<'_, Postgres>,
    zeroed: &[(Vec<u8>, i64)],
    backend: Option<&Arc<dyn ChunkBackend>>,
) -> Result<u64, sqlx::Error> {
    let Some(backend) = backend else {
        return Ok(0);
    };
    if zeroed.is_empty() {
        return Ok(0);
    }
    let mut keys: Vec<String> = Vec::with_capacity(zeroed.len());
    let mut hashes: Vec<Vec<u8>> = Vec::with_capacity(zeroed.len());
    for (hash, _size) in zeroed {
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
    // Drop chunk_tenants junction rows for the zeroed chunks. The FK's
    // ON DELETE CASCADE (migrations/018) never fires because we
    // soft-delete chunks (UPDATE SET deleted=TRUE, never DELETE FROM).
    // Without this, junction rows accumulate forever and
    // find_missing_chunks_for_tenant (which queries ONLY chunk_tenants,
    // no JOIN chunks) reports a tombstoned chunk as present → worker
    // skips PutChunk → manifest references S3-deleted object → GetChunk
    // NotFound.
    //
    // TOCTOU w/ drain: a chunk resurrected by PutPath between sweep
    // and drain keeps its S3 object (drain re-checks deleted=TRUE
    // before issuing S3 DELETE) but LOSES its junction rows here.
    // Tenant sees "missing" on next FindMissingChunks → re-uploads →
    // record_chunk_tenant re-inserts. Self-healing — same recovery
    // path as "chunk with zero junction rows" (chunked.rs:310).
    //
    // Same tx as the S3-enqueue + soft-delete (callers hold the tx).
    // r[impl store.chunk.tenant-scoped]
    sqlx::query("DELETE FROM chunk_tenants WHERE blake3_hash = ANY($1)")
        .bind(&hashes)
        .execute(&mut **tx)
        .await?;
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
///
/// # Deadlock safety
///
/// Runs inside a caller-provided `&mut Transaction`, so this CANNOT
/// use [`crate::metadata::with_sorted_retry`] — retry would need to
/// replay the whole outer txn. The `unique_hashes.sort_unstable()`
/// below is the primary defense (deterministic lock-acquisition
/// order across all `ANY($1)` writers). If the caller owns the txn
/// and wants defensive retry, wrap the outer txn in the helper.
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
    let mut unique_hashes: Vec<Vec<u8>> = {
        let mut seen = std::collections::HashSet::<[u8; 32]>::new();
        manifest
            .entries
            .into_iter()
            .filter(|e| seen.insert(e.hash))
            .map(|e| e.hash.to_vec())
            .collect()
    };
    // r[impl store.chunk.lock-order]
    // HashSet iteration order is nondeterministic across runs. Sort
    // before binding to ANY($1) so both UPDATEs below acquire row
    // locks in the same canonical order as every other chunk-hash
    // writer (rollback path, sweep path). Without this, a concurrent
    // decrement_refcounts_for_manifest on an overlapping manifest
    // could lock h1→h3 while we lock h3→h1 → circular wait → 40P01.
    unique_hashes.sort_unstable();
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
    stats.s3_keys_enqueued = enqueue_chunk_deletes(tx, &zeroed, backend).await?;

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::chunk::MemoryChunkBackend;
    use crate::manifest::{Manifest, ManifestEntry};
    use crate::test_helpers::ChunkSeed;
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

    /// enqueue_chunk_deletes: a hash that isn't 32 bytes is skipped
    /// with a warn, not a panic. The well-formed siblings in the same
    /// batch still enqueue. (Can't-happen in practice — chunks PK writers
    /// all insert 32 bytes — but warn+skip beats killing the sweep.)
    #[tokio::test]
    // r[verify store.gc.pending-deletes]
    async fn enqueue_skips_corrupt_hash() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let backend: Arc<dyn ChunkBackend> = Arc::new(MemoryChunkBackend::new());
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

    /// Regression: chunk_tenants junction rows must drop when a chunk is
    /// soft-deleted. Without the DELETE in enqueue_chunk_deletes, the
    /// junction accumulates forever AND find_missing_chunks_for_tenant
    /// (which queries ONLY chunk_tenants, no JOIN chunks) reports the
    /// tombstoned chunk as present → worker skips PutChunk → manifest
    /// references S3-deleted object.
    ///
    /// Shape: seed chunk + junction row → enqueue_chunk_deletes →
    /// assert junction row gone → assert find_missing says MISSING.
    // r[verify store.chunk.tenant-scoped]
    #[tokio::test]
    async fn enqueue_drops_junction_rows() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let backend: Arc<dyn ChunkBackend> = Arc::new(MemoryChunkBackend::new());

        // Seed a chunk at refcount=0 (post-decrement state).
        let hash = seed_chunk(&db.pool, 0xCA, 0, 100).await;

        // Seed tenant + junction row. FK requires tenant exists.
        let tid = rio_test_support::seed_tenant(&db.pool, "junction-test").await;
        sqlx::query("INSERT INTO chunk_tenants (blake3_hash, tenant_id) VALUES ($1, $2)")
            .bind(&hash[..])
            .bind(tid)
            .execute(&db.pool)
            .await
            .unwrap();

        // Precondition self-check: junction row exists → find_missing
        // says PRESENT. Proves the test isn't vacuous (junction seed worked).
        let pre = crate::metadata::find_missing_chunks_for_tenant(&db.pool, &[hash.to_vec()], tid)
            .await
            .unwrap();
        assert_eq!(
            pre,
            vec![false],
            "precondition: chunk should be present pre-sweep"
        );

        // Act: enqueue_chunk_deletes on the "zeroed" chunk.
        let mut tx = db.pool.begin().await.unwrap();
        let zeroed = vec![(hash.to_vec(), 100i64)];
        enqueue_chunk_deletes(&mut tx, &zeroed, Some(&backend))
            .await
            .unwrap();
        tx.commit().await.unwrap();

        // Junction row gone.
        let rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM chunk_tenants WHERE blake3_hash = $1")
                .bind(&hash[..])
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(
            rows, 0,
            "junction row should be deleted in same tx as S3 enqueue"
        );

        // End-to-end: find_missing now says MISSING for this tenant.
        let post = crate::metadata::find_missing_chunks_for_tenant(&db.pool, &[hash.to_vec()], tid)
            .await
            .unwrap();
        assert_eq!(post, vec![true], "tombstoned chunk must report missing");
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

    /// Migration 023's CHECK catches double-decrement at the source:
    /// an `UPDATE SET refcount = refcount - 1` that would take a
    /// chunk negative raises a constraint violation instead of
    /// silently leaking (a negative refcount never matches
    /// `WHERE refcount = 0` → chunk never GC'd).
    ///
    /// Replaces the old `idempotent_enqueue_on_conflict` test, which
    /// reached its assertion by calling `decrement_and_enqueue`
    /// twice on the same manifest — silently driving refcount 0→-1.
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
        let manifest = make_manifest(&[h1]);

        // First decrement: 1→0, fine.
        let mut tx = db.pool.begin().await.unwrap();
        decrement_and_enqueue(&mut tx, &manifest, None)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        // Second decrement: 0→-1, CHECK fires.
        let mut tx = db.pool.begin().await.unwrap();
        let err = decrement_and_enqueue(&mut tx, &manifest, None)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("chunks_refcount_nonneg"),
            "expected CHECK constraint violation, got: {err}"
        );
    }
}
