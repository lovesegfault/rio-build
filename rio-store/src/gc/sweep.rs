//! Sweep phase: delete unreachable paths + decrement chunks + enqueue S3.
// r[impl store.gc.two-phase]

use std::sync::Arc;
use std::time::Duration;

use sqlx::{Connection, PgPool, Postgres, Transaction};
use tracing::{info, instrument, warn};

use crate::backend::chunk::ChunkBackend;

use super::{GcStats, decrement_and_enqueue};

/// Terminal state of [`sweep`] other than success.
#[derive(Debug)]
pub enum SweepAbort {
    /// Process shutdown token fired between batches. Partial
    /// progress committed; caller should release advisory lock
    /// and return Aborted to the client.
    Shutdown,
    /// Database error. Transaction rolled back (sqlx drops the
    /// uncommitted tx on error-return).
    Db(sqlx::Error),
}

impl From<sqlx::Error> for SweepAbort {
    fn from(e: sqlx::Error) -> Self {
        SweepAbort::Db(e)
    }
}

/// Batch size for sweep transactions. Each batch is a single tx:
/// N narinfo DELETEs + chunk refcount decrements + pending_s3_deletes
/// INSERTs. Small enough that a batch-rollback on conflict doesn't
/// waste much; large enough to amortize tx overhead.
#[cfg(not(test))]
const SWEEP_BATCH_SIZE: usize = 100;
#[cfg(test)]
const SWEEP_BATCH_SIZE: usize = 2;

/// Grace period before a standalone chunk (refcount=0) becomes
/// GC-eligible.
///
/// # Why a grace window exists
///
/// `cas::put_chunked` upserts the `chunks` row BEFORE uploading to
/// S3 (write-ahead). If the upload crashes, the row sits at
/// refcount=0 with `uploaded_at IS NULL`. A retry of the same
/// PutPath bumps refcount and clears `deleted`; the grace window
/// gives that retry time to land before the orphan reaper fires.
///
/// # Why 300s
///
/// Long enough to cover a stalled-then-retried PutPath; short
/// enough that a genuinely abandoned chunk leaks storage for only
/// 5 minutes before the next sweep reaps it. Compare
/// `orphan::STALE_THRESHOLD` (2h) — that's for stale `uploading`
/// manifests, whose false-positive reaping is costlier (a whole
/// NAR re-upload).
///
/// `i64` to match the bind pattern in `orphan.rs` (`make_interval(
/// secs => $1)` accepts a bigint bind — PG casts it to double
/// internally for the `secs` named argument).
pub const CHUNK_GRACE_SECS: i64 = 300;

/// Sweep interval for the orphan-chunk reaper. 1h: orphan chunks
/// only accumulate on crashes mid-`cas::put_chunked` (rare), and
/// a chunk leaked for an extra hour costs only its storage
/// footprint. 15min (matching `orphan::SCAN_INTERVAL`) would be
/// harmless but wasteful — the partial-index scan is cheap, but
/// not free.
#[cfg(not(test))]
const ORPHAN_CHUNK_SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);
#[cfg(test)]
const ORPHAN_CHUNK_SWEEP_INTERVAL: Duration = Duration::from_millis(200);

/// Re-check whether `store_path_hash` has a referrer outside the
/// `sweep_unreachable` temp table, or a direct `gc_roots` /
/// `scheduler_live_pins` entry. See the call-site comment in
/// [`sweep`] for the GIN/anti-join rationale.
async fn recheck_has_live_referrer(
    tx: &mut Transaction<'_, Postgres>,
    store_path_hash: &[u8],
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT
          EXISTS (
            SELECT 1 FROM narinfo n
             WHERE n."references" @> ARRAY[
                     (SELECT store_path FROM narinfo WHERE store_path_hash = $1)
                   ]
               AND NOT EXISTS (
                 SELECT 1 FROM sweep_unreachable su
                  WHERE su.path_hash = n.store_path_hash
               )
             LIMIT 1
          )
          OR EXISTS (SELECT 1 FROM gc_roots WHERE store_path_hash = $1)
          OR EXISTS (SELECT 1 FROM scheduler_live_pins WHERE store_path_hash = $1)
        "#,
    )
    .bind(store_path_hash)
    .fetch_one(&mut **tx)
    .await
}

/// Walk `store_path_hash`'s reference closure within
/// `sweep_unreachable` and DELETE all closure members from the temp
/// table. Bounded to nodes already in the table (the JOIN), so
/// terminates and stays ≤ |unreachable|.
async fn closure_remove_from_unreachable(
    tx: &mut Transaction<'_, Postgres>,
    store_path_hash: &[u8],
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        WITH RECURSIVE closure(path_hash) AS (
            SELECT $1::bytea
          UNION
            SELECT dep.store_path_hash
              FROM closure c
              JOIN narinfo n ON n.store_path_hash = c.path_hash
              JOIN narinfo dep ON dep.store_path = ANY(n."references")
              JOIN sweep_unreachable su ON su.path_hash = dep.store_path_hash
        )
        DELETE FROM sweep_unreachable
         WHERE path_hash IN (SELECT path_hash FROM closure)
        "#,
    )
    .bind(store_path_hash)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Sweep unreachable paths. For each:
/// 1. `SELECT chunk_list FOR UPDATE` (TOCTOU guard vs PutPath
///    incrementing a refcount we're about to decrement)
/// 2. `DELETE realisations` for this path (NO FK to narinfo —
///    explicit delete prevents dangling wopQueryRealisation rows)
/// 3. `DELETE narinfo` (CASCADE → manifests/manifest_data)
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
///
/// `shutdown` is checked at each batch boundary (BEFORE `pool.begin`).
/// If fired, returns [`SweepAbort::Shutdown`] — the in-progress batch
/// already committed (previous iteration), the next batch never
/// starts. Safe point: no transaction open, no locks held other than
/// the caller's advisory GC lock (which the caller releases).
pub async fn sweep(
    pool: &PgPool,
    chunk_backend: Option<&Arc<dyn ChunkBackend>>,
    unreachable: Vec<Vec<u8>>,
    dry_run: bool,
    shutdown: &rio_common::signal::Token,
) -> Result<GcStats, SweepAbort> {
    let mut stats = GcStats::default();

    if unreachable.is_empty() {
        // Skip connection acquire + temp-table setup for no-op sweeps.
        return Ok(stats);
    }

    // Dedicated connection: the sweep_unreachable temp table below is
    // session-scoped and must survive across the batch-transaction
    // boundaries in the loop. pool.begin() would acquire a FRESH
    // connection each time — temp table invisible. Acquiring once
    // and begin()-ing on this conn keeps the session (and temp table)
    // alive for the whole sweep. Temp table drops automatically when
    // conn returns to the pool and PG eventually recycles it; the
    // defensive DROP IF EXISTS handles the case where a prior sweep
    // crashed mid-run and this call happens to reacquire that same
    // pooled connection with stale state.
    let mut conn = pool.acquire().await?;

    // Temp-table anti-join for the reference re-check. Before P0449
    // the re-check bound the WHOLE `unreachable` Vec<bytea> as $2
    // inside the per-path loop: N paths × N-element bytea[] = O(N²)
    // wire bytes. A 10k-path sweep sent ~3GB of $2 traffic, and
    // `<> ALL(array_param)` is an unindexed linear scan PG-side.
    // Populating once here is O(N) wire; NOT EXISTS against the
    // PRIMARY KEY is an index probe per-row.
    sqlx::query("DROP TABLE IF EXISTS sweep_unreachable")
        .execute(&mut *conn)
        .await?;
    sqlx::query("CREATE TEMP TABLE sweep_unreachable (path_hash bytea PRIMARY KEY)")
        .execute(&mut *conn)
        .await?;
    sqlx::query("INSERT INTO sweep_unreachable (path_hash) SELECT unnest($1::bytea[])")
        .bind(&unreachable)
        .execute(&mut *conn)
        .await?;

    // Pass 1 (whole-sweep): drain resurrections from sweep_unreachable.
    // For each candidate, re-check for a live referrer; if found,
    // closure-delete the candidate and its reference tree from the temp
    // table. After this pass, sweep_unreachable is settled w.r.t.
    // uploads that landed before pass-1 started — so the delete loop
    // below cannot commit Z-in-batch-N before observing that
    // Y-in-batch-N+1 (Y→Z) was resurrected. The delete loop re-runs
    // the same re-check under FOR UPDATE; that remains the
    // LOAD-BEARING guard for uploads landing DURING the sweep.
    for batch in unreachable.chunks(SWEEP_BATCH_SIZE) {
        if shutdown.is_cancelled() {
            return Err(SweepAbort::Shutdown);
        }
        let mut tx = conn.begin().await?;
        for store_path_hash in batch {
            // Cheap PK probe: skip items an earlier closure-delete
            // already removed (avoids the heavier referrer query).
            let still_in: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM sweep_unreachable WHERE path_hash = $1)",
            )
            .bind(store_path_hash)
            .fetch_one(&mut *tx)
            .await?;
            if still_in && recheck_has_live_referrer(&mut tx, store_path_hash).await? {
                closure_remove_from_unreachable(&mut tx, store_path_hash).await?;
            }
        }
        // Only the temp table changed. Always commit (even dry-run)
        // so the delete loop sees the settled state.
        tx.commit().await?;
    }

    let total = unreachable.len();
    for (i, batch) in unreachable.chunks(SWEEP_BATCH_SIZE).enumerate() {
        // Progress gauge: paths NOT yet processed (including this batch).
        // Emitted at batch boundary so an operator watching a long sweep
        // sees `remaining` ticking down per SWEEP_BATCH_SIZE commit. Set
        // to 0 after the loop. dry_run included — the gauge measures
        // sweep-loop progress, not committed deletes (that's
        // `gc_path_swept_total`).
        metrics::gauge!("rio_store_gc_sweep_paths_remaining")
            .set((total - i * SWEEP_BATCH_SIZE) as f64);
        // Shutdown check at batch boundary — safe point (no tx
        // open). A large sweep (thousands of batches × ~100ms each)
        // would otherwise survive SIGTERM grace → pod SIGKILLed
        // mid-transaction → next GC run starts from scratch anyway
        // (advisory lock released by connection close). Bailing here
        // is strictly better: committed batches stay committed,
        // caller sees a clean Aborted status.
        if shutdown.is_cancelled() {
            info!(
                swept = stats.paths_deleted,
                remaining = unreachable.len() as u64 - stats.paths_deleted,
                "sweep: shutdown signal received, aborting at batch boundary"
            );
            return Err(SweepAbort::Shutdown);
        }
        let mut tx = conn.begin().await?;

        // Within-batch two-pass: lock + re-check every batch item before
        // any narinfo DELETE. The whole-sweep resurrection drain above
        // settled sweep_unreachable for uploads that landed BEFORE the
        // sweep; this remaining split + the still_unreachable filter
        // below catch a PutPath landing DURING this delete loop where
        // the resurrecting path is later in the same batch.
        let mut to_delete: Vec<(&Vec<u8>, Option<Vec<u8>>)> = Vec::with_capacity(batch.len());

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

            // Step 1b: reference re-check. Mark's CTE took a
            // point-in-time MVCC snapshot; a PutPath that committed
            // AFTER that snapshot (during mark, or between mark and
            // now) may have written references=[this_path] —
            // including `'uploading'` placeholders, which carry
            // references from insert. Re-check via GIN index before
            // deleting. If found: skip, increment resurrected metric.
            // I-192: this is the LOAD-BEARING mark-vs-PutPath guard
            // (there is no advisory lock).
            // r[impl store.gc.sweep-recheck]
            //
            // The subquery resolves hash→path because narinfo."references"
            // is TEXT[] (store_path strings, not hashes). The GIN index
            // (migration 008) makes `"references" @> ARRAY[$path]` an
            // index scan. I-145: the previous `$path = ANY("references")`
            // form is semantically equivalent but does NOT use GIN — PG's
            // array-GIN opclass supports `@>`/`<@`/`&&`/`=` only, and the
            // planner does not rewrite `scalar = ANY(arrcol)` into `@>`.
            // At 100k+ narinfo rows that was a ~1.3s seqscan per swept
            // path. EXPLAIN-verified: `@>` → Bitmap Index Scan on
            // idx_narinfo_references_gin even with the InitPlan subquery.
            //
            // The NOT EXISTS anti-join against sweep_unreachable
            // excludes referrers that are themselves in the unreachable
            // set. Without this, mutual-reference cycles (A→B, B→A) and
            // self-references (A→A) are never swept: the re-check sees
            // an intra-set referrer and skips both paths forever. The
            // temp table holds the WHOLE `unreachable` set (not just
            // `batch`) — a cycle may span SWEEP_BATCH_SIZE boundaries.
            // r[impl store.gc.sweep-cycle-reclaim]
            //
            // Also re-check `gc_roots` and `scheduler_live_pins`: a
            // gc_roots pin or scheduler dispatch that landed between mark
            // and now is a direct root on THIS path that mark's snapshot
            // missed. Both tables key on store_path_hash (PK / first
            // index column) so each EXISTS is a point probe.
            if recheck_has_live_referrer(&mut tx, store_path_hash).await? {
                tracing::debug!(
                    store_path_hash = %hex::encode(store_path_hash),
                    "GC sweep: path resurrected (new referrer after mark), skipping"
                );
                metrics::counter!("rio_store_gc_path_resurrected_total").increment(1);
                stats.paths_resurrected += 1;
                // Transitive resurrection: this path is now live, so
                // its own references (and theirs, recursively) must
                // not be excluded by the anti-join above when later
                // batch entries are checked.
                closure_remove_from_unreachable(&mut tx, store_path_hash).await?;
                continue;
            }
            to_delete.push((store_path_hash, chunk_list));
        }

        // A closure-delete from a LATER item in the lock loop above may
        // have removed an EARLIER candidate from sweep_unreachable.
        // One batch probe.
        let still_unreachable: std::collections::HashSet<Vec<u8>> = if to_delete.is_empty() {
            std::collections::HashSet::new()
        } else {
            let candidate_hashes: Vec<Vec<u8>> =
                to_delete.iter().map(|(h, _)| (*h).clone()).collect();
            sqlx::query_scalar(
                "SELECT path_hash FROM sweep_unreachable WHERE path_hash = ANY($1::bytea[])",
            )
            .bind(&candidate_hashes)
            .fetch_all(&mut *tx)
            .await?
            .into_iter()
            .collect()
        };

        for (store_path_hash, chunk_list) in to_delete {
            if !still_unreachable.contains(store_path_hash) {
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

            // Step 2a': DELETE path_tenants for this path. NOT via
            // CASCADE — path_tenants has NO FK to narinfo
            // (012_path_tenants.sql). Without this explicit DELETE,
            // orphaned rows survive the sweep and grant wrong-tenant
            // visibility when a different tenant later re-uploads the
            // same store path (the stale row still JOINs in the
            // r[store.gc.tenant-retention] CTE arm).
            // r[impl store.gc.sweep-path-tenants]
            sqlx::query("DELETE FROM path_tenants WHERE store_path_hash = $1")
                .bind(store_path_hash)
                .execute(&mut *tx)
                .await?;

            // Step 2b: DELETE narinfo. CASCADE takes manifests,
            // manifest_data (but NOT realisations — see step 2a above).
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
    metrics::gauge!("rio_store_gc_sweep_paths_remaining").set(0.0);

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
/// These are chunks left by an interrupted `cas::put_chunked` that
/// no subsequent retry ever claimed. The main [`sweep`] above only
/// touches chunks as a SIDE EFFECT of path deletion (it decrements
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
#[instrument(skip(pool, chunk_backend))]
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
    // to amortize. A pathological crash mid-upload of a many-chunk
    // NAR produces a large candidate set; batching keeps each tx
    // bounded.
    for batch in candidates.chunks(SWEEP_BATCH_SIZE) {
        // r[impl store.chunk.lock-order]
        // Sort before binding to ANY($1): the outer SELECT returns rows
        // in PG scan order (not sorted); a concurrent rollback path
        // (delete_manifest_chunked_uploading) sorts ITS input. If we
        // don't sort here, sweep locks in SELECT order while rollback
        // locks in sort order — overlapping sets → circular wait →
        // 40P01. Sorting here makes lock-acquisition order match.
        //
        // Inline sort+retry instead of with_sorted_retry: this fn
        // returns sqlx::Error, not MetadataError, and the conversion
        // back would be lossy. The pattern is identical — sort once,
        // retry once on 40P01.
        let mut hashes: Vec<Vec<u8>> = batch.iter().map(|(h, _)| h.clone()).collect();
        hashes.sort_unstable();

        let mut attempt = 0;
        let (zd, bf) = loop {
            match sweep_orphan_batch(pool, &hashes, chunk_backend).await {
                // 40P01 deadlock_detected. This module returns
                // sqlx::Error (not MetadataError), so inline the
                // SQLSTATE check instead of matching the Deadlock
                // variant. Single retry — see with_sorted_retry doc.
                Err(sqlx::Error::Database(db))
                    if db.code().as_deref() == Some("40P01") && attempt == 0 =>
                {
                    warn!(error = %db, "40P01 on orphan-chunk sweep batch; retrying once");
                    tokio::time::sleep(crate::metadata::jitter()).await;
                    attempt += 1;
                }
                r => break r?,
            }
        };
        chunks_deleted += zd;
        bytes_freed += bf;
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

/// Transaction body for one [`sweep_orphan_chunks`] batch. Split out so
/// the outer loop can retry the whole txn on 40P01 (PG aborts the full
/// txn on deadlock, not just the failing statement).
///
/// `hashes` MUST already be sorted — caller's responsibility (see
/// `r[store.chunk.lock-order]`). Returns `(chunks_deleted, bytes_freed)`.
async fn sweep_orphan_batch(
    pool: &PgPool,
    hashes: &[Vec<u8>],
    chunk_backend: Option<&Arc<dyn ChunkBackend>>,
) -> Result<(u64, u64), sqlx::Error> {
    let mut tx = pool.begin().await?;

    // Inner UPDATE: re-check refcount=0 + deleted=FALSE at execution
    // time. `RETURNING` gives us the rows that actually flipped — the
    // difference between `hashes.len()` and `zeroed.len()` is the
    // count of chunks resurrected (PutPath claimed them) between outer
    // SELECT and now. No metric for THIS window yet — distinct from
    // drain.rs's `rio_store_gc_chunk_resurrected_total`, which counts
    // resurrection between sweep's enqueue and drain's S3-delete (a
    // later, wider window). This SELECT→UPDATE gap is a single PG
    // roundtrip; not expected to be hot.
    //
    // No FOR UPDATE needed on the outer SELECT: the UPDATE's WHERE
    // clause IS the guard. PG's row-level locking for UPDATE
    // serializes against the PutPath UPSERT on the same blake3_hash.
    let zeroed: Vec<(Vec<u8>, i64)> = sqlx::query_as(
        r#"
        UPDATE chunks SET deleted = TRUE, uploaded_at = NULL
         WHERE blake3_hash = ANY($1)
           AND refcount = 0 AND deleted = FALSE
        RETURNING blake3_hash, size
        "#,
    )
    .bind(hashes)
    .fetch_all(&mut *tx)
    .await?;

    let zd = zeroed.len() as u64;
    let bf = zeroed.iter().map(|(_, s)| *s as u64).sum::<u64>();

    // Enqueue S3 keys for zeroed chunks. If chunk_backend is None
    // (inline-only store), there are no S3 keys to delete — `zeroed`
    // is empty (no chunked uploads ever happened) and this is a
    // no-op. The Option-check inside the helper is belt-and-suspenders.
    super::enqueue_chunk_deletes(&mut tx, &zeroed, chunk_backend).await?;

    tx.commit().await?;
    Ok((zd, bf))
}

/// Spawn the periodic orphan-chunk sweeper. Runs
/// [`sweep_orphan_chunks`] every `ORPHAN_CHUNK_SWEEP_INTERVAL`
/// with [`CHUNK_GRACE_SECS`] grace. Errors logged; next iteration
/// retries. Exits cleanly when `shutdown` is cancelled.
///
/// Same `spawn_periodic` shape as `orphan::spawn_scanner` /
/// `drain::spawn_drain_task` — `MissedTickBehavior::Skip`, so a
/// slow sweep (large orphan backlog after a mass worker crash)
/// doesn't queue up back-to-back runs.
pub fn spawn_orphan_chunk_sweep(
    pool: PgPool,
    chunk_backend: Option<Arc<dyn ChunkBackend>>,
    shutdown: rio_common::signal::Token,
) -> tokio::task::JoinHandle<()> {
    rio_common::task::spawn_periodic(
        "gc-orphan-chunk-sweep",
        ORPHAN_CHUNK_SWEEP_INTERVAL,
        shutdown,
        move || {
            let pool = pool.clone();
            let chunk_backend = chunk_backend.clone();
            async move {
                if let Err(e) =
                    sweep_orphan_chunks(&pool, chunk_backend.as_ref(), CHUNK_GRACE_SECS).await
                {
                    warn!(error = %e, "orphan chunk sweep failed (will retry next interval)");
                }
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::TenantSeed;
    use crate::test_helpers::{ChunkSeed, StoreSeed, path_hash};
    use rio_test_support::TestDb;
    use rio_test_support::fixtures::test_store_path;

    /// Never-cancelled token for sweep tests that don't exercise
    /// the shutdown path.
    fn no_shutdown() -> rio_common::signal::Token {
        rio_common::signal::Token::new()
    }

    /// Sweep must DELETE realisations rows pointing to swept paths.
    /// realisations has NO FK to narinfo (002_store.sql:134); without
    /// the explicit DELETE, dangling rows → wopQueryRealisation returns
    /// a path that 404s on fetch.
    #[tokio::test]
    async fn sweep_deletes_realisations() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Seed a complete path + a realisation pointing to it.
        let path = test_store_path("sweep-target");
        let hash = StoreSeed::raw_path(&path).seed(&db.pool).await;
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
        let stats = sweep(&db.pool, None, vec![hash.clone()], false, &no_shutdown())
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
        let hash = StoreSeed::path("dryrun").seed(&db.pool).await;

        let stats = sweep(&db.pool, None, vec![hash.clone()], true, &no_shutdown())
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
        let p = test_store_path("resurrected");
        let p_hash = StoreSeed::raw_path(&p).seed(&db.pool).await;

        // Q: references P. Seeded AFTER mark (simulating the race:
        // mark returned [p_hash], THEN PutPath for Q completed).
        StoreSeed::path("referrer")
            .with_refs(&[&p])
            .seed(&db.pool)
            .await;

        // Sweep with P in the unreachable list. The reference re-check should
        // find Q.references=[P] → skip P → paths_resurrected=1.
        let stats = sweep(&db.pool, None, vec![p_hash.clone()], false, &no_shutdown())
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

    /// I-192: same race as `sweep_resurrected_path_skipped`, but the
    /// new referrer is an `'uploading'` PLACEHOLDER (not a complete
    /// path). This is the precise mark-vs-PutPath case the now-removed
    /// `GC_MARK_LOCK_ID` advisory lock guarded against: mark snapshot
    /// at T0 → `insert_manifest_uploading(P, refs=[Q])` commits at T1
    /// → sweep at T2. The re-check scans ALL narinfo (no
    /// `status='complete'` filter), so the placeholder's `references`
    /// column is enough to resurrect Q. The grace window protects P
    /// itself; this proves Q (P's reference, past grace) is also
    /// protected — without an advisory lock.
    // r[verify store.gc.sweep-recheck]
    #[tokio::test]
    async fn sweep_recheck_sees_uploading_placeholder() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Q: old, no roots — mark would (and did) declare unreachable.
        let q = test_store_path("placeholder-ref-target");
        let q_hash = StoreSeed::raw_path(&q)
            .created_hours_ago(48)
            .seed(&db.pool)
            .await;

        // Simulate "mark already returned [Q]" by passing it directly
        // to sweep. Now P's placeholder commits — exactly the race
        // window the lock used to (redundantly) close.
        let p = test_store_path("placeholder-referrer");
        let p_hash = path_hash(&p);
        let inserted = crate::metadata::insert_manifest_uploading(
            &db.pool,
            &p_hash,
            &p,
            std::slice::from_ref(&q),
        )
        .await
        .unwrap();
        assert!(inserted);

        // Sanity: P is status='uploading', nar_size=0 — a real
        // placeholder, not a complete path.
        let (status, nar_size): (String, i64) = sqlx::query_as(
            "SELECT m.status::text, n.nar_size FROM manifests m \
             JOIN narinfo n USING (store_path_hash) WHERE m.store_path_hash = $1",
        )
        .bind(&p_hash)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(status, "uploading");
        assert_eq!(nar_size, 0);

        // Sweep with Q in unreachable. Re-check must see P's
        // placeholder narinfo.references @> [Q] → resurrect.
        let stats = sweep(&db.pool, None, vec![q_hash.clone()], false, &no_shutdown())
            .await
            .unwrap();
        assert_eq!(
            stats.paths_deleted, 0,
            "Q must NOT be deleted — uploading placeholder P references it"
        );
        assert_eq!(stats.paths_resurrected, 1);

        let q_exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM narinfo WHERE store_path_hash = $1)")
                .bind(&q_hash)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert!(q_exists, "Q's narinfo must survive sweep");
    }

    /// Transitive resurrection: when Y is resurrected because live P
    /// references it, Y's own dependency Z (also in the unreachable
    /// set) must NOT be swept — Y is now a live referrer of Z.
    /// Without the closure-delete from sweep_unreachable, the
    /// anti-join keeps excluding Y as a referrer of Z and Z is
    /// deleted, leaving live P→Y→Z dangling.
    #[tokio::test]
    async fn sweep_resurrection_is_transitive() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Z: leaf, marked unreachable.
        let z = test_store_path("transitive-z");
        let z_hash = StoreSeed::raw_path(&z).seed(&db.pool).await;
        // Y: references Z, marked unreachable.
        let y = test_store_path("transitive-y");
        let y_hash = StoreSeed::raw_path(&y)
            .with_refs(&[&z])
            .seed(&db.pool)
            .await;
        // P: references Y. NOT in unreachable (the post-mark live
        // referrer that triggers Y's resurrection).
        StoreSeed::path("transitive-p")
            .with_refs(&[&y])
            .seed(&db.pool)
            .await;

        // Forward order [Y, Z]: Y's pass-1 re-check resurrects + closure-
        // deletes Z; Z's pass-1 re-check then sees Y as live.
        let stats = sweep(
            &db.pool,
            None,
            vec![y_hash.clone(), z_hash.clone()],
            false,
            &no_shutdown(),
        )
        .await
        .unwrap();
        assert_eq!(stats.paths_deleted, 0, "neither Y nor Z deleted");
        assert_eq!(stats.paths_resurrected, 2, "Y resurrected by P; Z by Y");

        for (h, name) in [(&y_hash, "Y"), (&z_hash, "Z")] {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM narinfo WHERE store_path_hash = $1)",
            )
            .bind(h)
            .fetch_one(&db.pool)
            .await
            .unwrap();
            assert!(exists, "{name} must survive (transitive resurrection)");
        }
    }

    /// Same as `sweep_resurrection_is_transitive` but with batch order
    /// reversed. Z's pass-1 re-check finds no live referrer (Y is in
    /// sweep_unreachable, anti-join excludes it) → Z is a delete
    /// candidate. Y's pass-1 re-check then resurrects and closure-
    /// deletes Z from sweep_unreachable. Pass-2's filter sees Z is
    /// gone and skips it. Single-pass would have already deleted Z.
    #[tokio::test]
    async fn sweep_resurrection_is_transitive_reverse_order() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let z = test_store_path("transitive-rev-z");
        let z_hash = StoreSeed::raw_path(&z).seed(&db.pool).await;
        let y = test_store_path("transitive-rev-y");
        let y_hash = StoreSeed::raw_path(&y)
            .with_refs(&[&z])
            .seed(&db.pool)
            .await;
        StoreSeed::path("transitive-rev-p")
            .with_refs(&[&y])
            .seed(&db.pool)
            .await;

        // Z FIRST — the order the single-pass implementation got wrong.
        let stats = sweep(
            &db.pool,
            None,
            vec![z_hash.clone(), y_hash.clone()],
            false,
            &no_shutdown(),
        )
        .await
        .unwrap();
        assert_eq!(stats.paths_deleted, 0, "neither Y nor Z deleted");
        assert_eq!(
            stats.paths_resurrected, 2,
            "Y by P (pass-1); Z by filter (pass-2)"
        );

        for (h, name) in [(&y_hash, "Y"), (&z_hash, "Z")] {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM narinfo WHERE store_path_hash = $1)",
            )
            .bind(h)
            .fetch_one(&db.pool)
            .await
            .unwrap();
            assert!(
                exists,
                "{name} must survive (reverse-order transitive resurrection)"
            );
        }
    }

    /// Cross-batch reverse order: Z in batch 1, Y in batch 2 (forced
    /// via a filler item with cfg(test) SWEEP_BATCH_SIZE=2). Without
    /// the whole-sweep resurrection drain, batch 1's tx commits Z
    /// deleted before batch 2 ever observes P→Y→Z.
    #[tokio::test]
    async fn sweep_resurrection_is_transitive_cross_batch() {
        const _: () = assert!(SWEEP_BATCH_SIZE == 2, "test assumes batch size 2");
        let db = TestDb::new(&crate::MIGRATOR).await;

        let z = test_store_path("xbatch-z");
        let z_hash = StoreSeed::raw_path(&z).seed(&db.pool).await;
        let filler_hash = StoreSeed::path("xbatch-filler").seed(&db.pool).await;
        let y = test_store_path("xbatch-y");
        let y_hash = StoreSeed::raw_path(&y)
            .with_refs(&[&z])
            .seed(&db.pool)
            .await;
        StoreSeed::path("xbatch-p")
            .with_refs(&[&y])
            .seed(&db.pool)
            .await;

        // [Z, filler] = batch 1; [Y] = batch 2.
        let stats = sweep(
            &db.pool,
            None,
            vec![z_hash.clone(), filler_hash, y_hash.clone()],
            false,
            &no_shutdown(),
        )
        .await
        .unwrap();
        assert_eq!(stats.paths_deleted, 1, "only filler deleted");
        assert_eq!(stats.paths_resurrected, 2, "Y by P; Z by Y");

        for (h, name) in [(&y_hash, "Y"), (&z_hash, "Z")] {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM narinfo WHERE store_path_hash = $1)",
            )
            .bind(h)
            .fetch_one(&db.pool)
            .await
            .unwrap();
            assert!(exists, "{name} must survive (cross-batch resurrection)");
        }
    }

    /// Re-check must consult `gc_roots` and `scheduler_live_pins`: a
    /// gc_roots pin or scheduler dispatch between mark and sweep is a
    /// direct root mark's snapshot missed.
    #[tokio::test]
    async fn sweep_recheck_sees_late_pins() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // P: pinned via gc_roots after mark.
        let p_hash = StoreSeed::path("late-gc-root").seed(&db.pool).await;
        sqlx::query("INSERT INTO gc_roots (store_path_hash, source) VALUES ($1, 'test')")
            .bind(&p_hash)
            .execute(&db.pool)
            .await
            .unwrap();

        // Q: pinned via scheduler_live_pins after mark.
        let q_hash = StoreSeed::path("late-live-pin").seed(&db.pool).await;
        sqlx::query(
            "INSERT INTO scheduler_live_pins (store_path_hash, drv_hash) VALUES ($1, 'drv')",
        )
        .bind(&q_hash)
        .execute(&db.pool)
        .await
        .unwrap();

        let stats = sweep(
            &db.pool,
            None,
            vec![p_hash.clone(), q_hash.clone()],
            false,
            &no_shutdown(),
        )
        .await
        .unwrap();
        assert_eq!(stats.paths_deleted, 0);
        assert_eq!(stats.paths_resurrected, 2);

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM narinfo")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(count, 2, "both pinned paths survive");
    }

    /// Sweep must DELETE path_tenants rows for swept paths.
    /// path_tenants has NO FK to narinfo (012_path_tenants.sql);
    /// without the explicit DELETE, orphaned rows survive and grant
    /// wrong-tenant visibility when a different tenant later
    /// re-uploads the same store path.
    // r[verify store.gc.sweep-path-tenants]
    #[tokio::test]
    async fn sweep_deletes_path_tenants() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Seed path + tenant + path_tenants row.
        let hash = StoreSeed::path("tenant-swept").seed(&db.pool).await;
        let tenant_id = TenantSeed::new("sweeper").seed(&db.pool).await;
        sqlx::query("INSERT INTO path_tenants (store_path_hash, tenant_id) VALUES ($1, $2)")
            .bind(&hash)
            .bind(tenant_id)
            .execute(&db.pool)
            .await
            .unwrap();

        // Sweep.
        let stats = sweep(&db.pool, None, vec![hash.clone()], false, &no_shutdown())
            .await
            .unwrap();
        assert_eq!(stats.paths_deleted, 1);

        // path_tenants row ALSO gone (explicit DELETE, not CASCADE).
        let pt_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM path_tenants")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(
            pt_count, 0,
            "sweep should delete path_tenants rows for swept path (no FK CASCADE)"
        );
    }

    /// Mutual-reference cycles (A→B, B→A) and self-references (A→A)
    /// must be swept when both sides are in the unreachable set.
    /// Without the sweep_unreachable anti-join in the re-check, the
    /// re-check sees an intra-set referrer and skips both forever.
    // r[verify store.gc.sweep-cycle-reclaim]
    #[tokio::test]
    async fn sweep_reclaims_two_cycle() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // A↔B cycle: A references B, B references A.
        let path_a = test_store_path("cycle-a");
        let path_b = test_store_path("cycle-b");
        let hash_a = StoreSeed::raw_path(&path_a)
            .with_refs(&[&path_b])
            .seed(&db.pool)
            .await;
        let hash_b = StoreSeed::raw_path(&path_b)
            .with_refs(&[&path_a])
            .seed(&db.pool)
            .await;

        // Self-reference: C→C.
        let path_c = test_store_path("self-ref");
        let hash_c = StoreSeed::raw_path(&path_c)
            .with_refs(&[&path_c])
            .seed(&db.pool)
            .await;

        // Sweep all three. The re-check must exclude intra-batch
        // referrers → all three swept, none stuck at resurrected.
        let stats = sweep(
            &db.pool,
            None,
            vec![hash_a, hash_b, hash_c],
            false,
            &no_shutdown(),
        )
        .await
        .unwrap();
        assert_eq!(
            stats.paths_deleted, 3,
            "A↔B cycle + C self-ref all swept (intra-batch referrers excluded)"
        );
        assert_eq!(
            stats.paths_resurrected, 0,
            "no path should be stuck at resurrected — cycle members are \
             NOT genuine referrers"
        );

        // All narinfo gone.
        let narinfo_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM narinfo")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(narinfo_count, 0, "all three narinfo rows deleted");
    }

    /// If nobody references the path, sweep proceeds normally (no false-positive resurrection).
    #[tokio::test]
    async fn sweep_unreferenced_path_deleted() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let hash = StoreSeed::path("unreferenced").seed(&db.pool).await;

        let stats = sweep(&db.pool, None, vec![hash], false, &no_shutdown())
            .await
            .unwrap();
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
        let path = test_store_path("chunked");
        let sp_hash = path_hash(&path);

        // Seed two chunks at refcount=1 (will zero + enqueue).
        let chunk_h1 = ChunkSeed::new(0xAA)
            .with_refcount(1)
            .with_size(1000)
            .seed(&db.pool)
            .await;
        let chunk_h2 = ChunkSeed::new(0xBB)
            .with_refcount(1)
            .with_size(2000)
            .seed(&db.pool)
            .await;

        // Seed narinfo + manifest (chunked: inline_blob NULL → StoreSeed
        // default). manifest_data with chunk_list seeded separately.
        let seeded = StoreSeed::raw_path(&path).seed(&db.pool).await;
        assert_eq!(seeded, sp_hash);
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
            .bind(&sp_hash)
            .bind(&chunk_list)
            .execute(&db.pool)
            .await
            .unwrap();

        // Sweep with a backend → decrement + enqueue.
        let backend: Arc<dyn ChunkBackend> = Arc::new(MemoryChunkBackend::new());
        let stats = sweep(
            &db.pool,
            Some(&backend),
            vec![sp_hash.clone()],
            false,
            &no_shutdown(),
        )
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

    /// Seed a standalone chunk at refcount=0 with a backdated
    /// `created_at`. Thin wrapper keeping the (seed, size, age) call
    /// shape the orphan-chunk tests below use heavily.
    async fn seed_orphan_chunk(pool: &PgPool, seed: u8, size: i64, age_secs: i64) -> [u8; 32] {
        ChunkSeed::new(seed)
            .with_size(size)
            .age_secs(age_secs)
            .seed(pool)
            .await
    }

    // r[verify store.chunk.grace-ttl]
    /// The three-way partition the grace-TTL guard must uphold:
    ///
    /// | Chunk | refcount | age vs grace | Expected |
    /// |-------|----------|--------------|----------|
    /// | young | 0        | within       | survives |
    /// | old   | 0        | past         | reaped   |
    /// | live  | 1        | past         | survives |
    ///
    /// A broken grace check would reap `young` (interrupted-upload
    /// race). A broken refcount check would reap `live` (data loss).
    /// We assert both negatives in one test because a single-axis
    /// test ("young survives") would pass trivially if the function
    /// did nothing at all — `old`'s deletion proves the sweep fires,
    /// so the survivals MEAN something.
    #[tokio::test]
    async fn orphan_chunk_grace_ttl_partitions_correctly() {
        use crate::backend::chunk::{ChunkBackend, MemoryChunkBackend};
        let db = TestDb::new(&crate::MIGRATOR).await;
        let backend: Arc<dyn ChunkBackend> = Arc::new(MemoryChunkBackend::new());

        // Grace = 100s. Young at 10s, old at 200s.
        let grace = 100i64;
        let young = seed_orphan_chunk(&db.pool, 0xA1, 500, 10).await;
        let old = seed_orphan_chunk(&db.pool, 0xA2, 700, 200).await;

        // `live`: old but refcount=1 (a manifest claimed it). Seed
        // via the same helper then bump refcount — cheaper than a
        // full manifest fixture, and the sweep only reads `chunks`.
        let live = seed_orphan_chunk(&db.pool, 0xA3, 900, 200).await;
        sqlx::query("UPDATE chunks SET refcount = 1 WHERE blake3_hash = $1")
            .bind(live.as_slice())
            .execute(&db.pool)
            .await
            .unwrap();

        // Sweep.
        let (deleted, bytes) = sweep_orphan_chunks(&db.pool, Some(&backend), grace)
            .await
            .unwrap();

        // Exactly `old`. bytes_freed is `old`'s size — this is the
        // "proves nothing" guard: if the sweep reaped everything,
        // deleted would be 3 and bytes would be 2100. Asserting the
        // exact values catches both over-reap and under-reap.
        assert_eq!(deleted, 1, "only `old` should be reaped");
        assert_eq!(bytes, 700, "bytes_freed must match `old`'s size exactly");

        // `old` flipped to deleted=true; `young` and `live` did not.
        let is_deleted = |h: [u8; 32]| {
            let pool = db.pool.clone();
            async move {
                sqlx::query_scalar::<_, bool>("SELECT deleted FROM chunks WHERE blake3_hash = $1")
                    .bind(h.as_slice())
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            }
        };
        assert!(is_deleted(old).await, "old chunk → deleted=true");
        assert!(
            !is_deleted(young).await,
            "young chunk within grace → untouched"
        );
        assert!(
            !is_deleted(live).await,
            "referenced chunk (refcount>0) → untouched"
        );

        // S3 key enqueued for `old` only.
        let enqueued: Vec<(Vec<u8>,)> =
            sqlx::query_as("SELECT blake3_hash FROM pending_s3_deletes")
                .fetch_all(&db.pool)
                .await
                .unwrap();
        assert_eq!(enqueued.len(), 1, "exactly one S3 key enqueued");
        assert_eq!(
            enqueued[0].0,
            old.as_slice(),
            "enqueued key is `old`'s hash"
        );
    }

    /// Clock advance: chunk within grace survives first sweep,
    /// then we backdate it (simulating time passing — PG's now()
    /// is real wallclock, we can't tokio::time::pause it), then
    /// second sweep reaps it.
    ///
    /// The plan doc's sketch said "Advance clock past grace" — this
    /// is the closest we can get in a unit test against real PG.
    /// Backdating `created_at` has the same effect as advancing
    /// now(): the delta `now() - created_at` grows.
    #[tokio::test]
    async fn orphan_chunk_survives_then_reaped_after_grace() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let grace = 100i64;
        let hash = seed_orphan_chunk(&db.pool, 0xB1, 1234, 10).await;

        // First sweep: within grace → survives.
        let (deleted, _) = sweep_orphan_chunks(&db.pool, None, grace).await.unwrap();
        assert_eq!(deleted, 0, "within grace: nothing reaped");
        let still_there: bool =
            sqlx::query_scalar("SELECT NOT deleted FROM chunks WHERE blake3_hash = $1")
                .bind(hash.as_slice())
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert!(still_there, "chunk still alive after first sweep");

        // Advance "clock" by backdating. 10s → 200s ago.
        sqlx::query(
            "UPDATE chunks SET created_at = now() - make_interval(secs => 200) \
             WHERE blake3_hash = $1",
        )
        .bind(hash.as_slice())
        .execute(&db.pool)
        .await
        .unwrap();

        // Second sweep: past grace → reaped.
        let (deleted, bytes) = sweep_orphan_chunks(&db.pool, None, grace).await.unwrap();
        assert_eq!(deleted, 1, "past grace: reaped");
        assert_eq!(bytes, 1234);
    }

    /// Resurrection race: PutPath claims the chunk between outer
    /// SELECT and inner UPDATE. We can't interleave with
    /// sweep_orphan_chunks' internal loop from a unit test (same
    /// limitation as orphan.rs's TOCTOU test), so we assert the
    /// INVARIANT: the inner UPDATE's WHERE re-checks refcount=0.
    ///
    /// Seed an old refcount=0 chunk → it IS a candidate. Then
    /// simulate PutPath's UPSERT (refcount=1, deleted=false).
    /// Sweep → the outer SELECT would have found it (had we run
    /// it first), but the inner UPDATE's `WHERE refcount=0` must
    /// skip it.
    #[tokio::test]
    async fn orphan_chunk_resurrected_by_putpath_survives() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let grace = 100i64;
        // Old enough to be a candidate.
        let hash = seed_orphan_chunk(&db.pool, 0xC1, 333, 200).await;

        // Simulate PutPath's chunk UPSERT (metadata/chunked.rs:117):
        // refcount += 1, deleted = false. This is what would land
        // in the gap between outer SELECT and inner UPDATE in the
        // real race.
        sqlx::query(
            "UPDATE chunks SET refcount = refcount + 1, deleted = false \
             WHERE blake3_hash = $1",
        )
        .bind(hash.as_slice())
        .execute(&db.pool)
        .await
        .unwrap();

        // Sweep. The inner UPDATE's `WHERE refcount = 0` must reject.
        let (deleted, _) = sweep_orphan_chunks(&db.pool, None, grace).await.unwrap();
        assert_eq!(
            deleted, 0,
            "resurrected chunk (refcount now 1) must NOT be reaped — \
             inner UPDATE's WHERE refcount=0 re-check"
        );

        // Chunk is alive: refcount=1, deleted=false.
        let (refcount, deleted): (i32, bool) =
            sqlx::query_as("SELECT refcount, deleted FROM chunks WHERE blake3_hash = $1")
                .bind(hash.as_slice())
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(refcount, 1);
        assert!(!deleted);
    }

    /// Regression: concurrent orphan-chunk sweep vs rollback on
    /// overlapping chunk hashes MUST NOT deadlock.
    ///
    /// Before P0495's sort in sweep_orphan_chunks, sweep bound its
    /// ANY($1) in outer-SELECT scan order while rollback
    /// (delete_manifest_chunked_uploading) bound in sort order. With
    /// an overlapping hash set, sweep locks in one order while
    /// rollback locks in another → circular wait → SQLSTATE 40P01.
    ///
    /// After the sort, both acquire row locks in the same canonical
    /// byte order — no circular wait possible. The 5s timeout makes
    /// a regression fail fast (PG's deadlock_timeout is 1s; a real
    /// deadlock shows as one side 40P01'ing and the test failing on
    /// the error, OR — under the right interleaving — the timeout
    /// trips before PG's detector fires).
    // r[verify store.chunk.lock-order]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sweep_vs_rollback_no_deadlock() {
        use tokio::time::timeout;

        let db = TestDb::new(&crate::MIGRATOR).await;

        // Seed 100 chunks at refcount=0, created_at=1h ago →
        // sweep-eligible (past grace). All overlap with the rollback
        // set below. Refcount starts at 0 so rollback's decrement
        // would go negative — we seed via upgrade_manifest_to_chunked
        // which bumps to refcount=1, making both the sweep (refcount=0
        // check fails → no-op on those rows) AND rollback (1→0) valid
        // concurrently. For THIS test we want the DEADLOCK surface,
        // not the row-state: sweep's UPDATE and rollback's UPDATE
        // both take row locks on the SAME hashes in the SAME txn
        // window. Whether each UPDATE flips state doesn't matter —
        // row locks are acquired regardless.
        //
        // Seed strategy: 100 chunks at refcount=1, old. Sweep's outer
        // SELECT (refcount=0) won't find them, so call
        // sweep_orphan_batch directly with the hash list to force the
        // row-lock acquisition. Rollback decrements 1→0.
        let hashes: Vec<Vec<u8>> = (0u8..100).map(|i| vec![i; 32]).collect();
        let sizes: Vec<i64> = vec![1024; 100];

        // Seed placeholder + upgrade → chunks at refcount=1.
        let sph = vec![0xAAu8; 32];
        crate::metadata::insert_manifest_uploading(
            &db.pool,
            &sph,
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-p495-test",
            &[],
        )
        .await
        .unwrap();
        crate::metadata::upgrade_manifest_to_chunked(&db.pool, &sph, b"ml", &hashes, &sizes)
            .await
            .unwrap();

        // Sweep side: call sweep_orphan_batch directly with the
        // sorted hash list (matching what sweep_orphan_chunks does
        // post-sort). This forces row-lock acquisition on all 100
        // chunks. The UPDATE's WHERE refcount=0 won't match (they're
        // at 1), but locks are acquired before the WHERE filter.
        //
        // Rollback side: delete_manifest_chunked_uploading with a
        // REVERSED copy of hashes — this was the pathological order
        // before both sides sorted.
        let mut hashes_sorted = hashes.clone();
        hashes_sorted.sort_unstable();
        let mut hashes_rev = hashes.clone();
        hashes_rev.reverse();

        let pool_a = db.pool.clone();
        let pool_b = db.pool.clone();
        let sph_b = sph.clone();

        let task_sweep =
            tokio::spawn(async move { sweep_orphan_batch(&pool_a, &hashes_sorted, None).await });
        let task_rollback = tokio::spawn(async move {
            crate::metadata::delete_manifest_chunked_uploading(&pool_b, &sph_b, &hashes_rev).await
        });

        let (rs, rr) = timeout(Duration::from_secs(5), async {
            tokio::try_join!(task_sweep, task_rollback).expect("tasks should not panic")
        })
        .await
        .expect("concurrent sweep+rollback must complete within 5s — deadlock detected");

        rs.expect("sweep batch should succeed (or retry-once past 40P01)");
        rr.expect("rollback should succeed (or retry-once past 40P01)");

        // Ground truth: all refcounts → 0 (rollback decremented them).
        let sum: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(refcount),0) FROM chunks WHERE blake3_hash = ANY($1)",
        )
        .bind(&hashes)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(sum, 0, "rollback decremented all 100 chunks to zero");
    }
}
