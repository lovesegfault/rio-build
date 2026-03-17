//! StoreAdminService: TriggerGC + PinPath/UnpinPath.
//!
//! The scheduler's `AdminService.TriggerGC` proxies here after
//! populating `extra_roots` from live builds (via GcRoots actor
//! command). Separate service from StoreService so admin ops can
//! have separate RBAC (more privileged than PutPath).

use std::io::Write;
use std::sync::Arc;

use sqlx::PgPool;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::{debug, error, info, instrument, warn};

use rio_nix::refscan::{CandidateSet, RefScanSink};
use rio_proto::types::{
    GcProgress, GcRequest, PinPathRequest, PinPathResponse, ResignPathsRequest, ResignPathsResponse,
};

use crate::backend::chunk::ChunkBackend;
use crate::cas::ChunkCache;
use crate::gc;
use crate::gc::{GC_LOCK_ID, GC_MARK_LOCK_ID};
use crate::metadata::{self, ManifestKind};
use crate::signing::Signer;

/// StoreAdminService gRPC server.
pub struct StoreAdminServiceImpl {
    pool: PgPool,
    /// Chunk backend for sweep's key_for (enqueue to pending_s3_
    /// deletes). None = inline-only store, sweep does CASCADE
    /// delete only (no chunk refcounting).
    chunk_backend: Option<Arc<dyn ChunkBackend>>,
    /// Chunk cache for ResignPaths NAR reassembly (chunked paths).
    /// None = inline-only store; chunked manifests fail with a
    /// clear error instead of silently skipping.
    chunk_cache: Option<Arc<ChunkCache>>,
    /// ed25519 key for re-signing narinfo after backfilled refs
    /// change the fingerprint. None = refs are updated but no new
    /// signature is appended (old sig won't verify against the new
    /// fingerprint; caller must re-sign out-of-band or accept
    /// unsigned paths).
    signer: Option<Arc<Signer>>,
}

impl StoreAdminServiceImpl {
    pub fn new(pool: PgPool, chunk_backend: Option<Arc<dyn ChunkBackend>>) -> Self {
        Self {
            pool,
            chunk_backend,
            chunk_cache: None,
            signer: None,
        }
    }

    /// Enable NAR reassembly for chunked paths in ResignPaths.
    /// Builder-style; chains after `new()`. Without this, the
    /// backfill can only process inline-stored paths (chunked
    /// paths fail with a clear "no chunk backend" error and stay
    /// `refs_backfilled=FALSE` for retry once the cache is wired).
    pub fn with_chunk_cache(mut self, cache: Arc<ChunkCache>) -> Self {
        self.chunk_cache = Some(cache);
        self
    }

    /// Enable narinfo re-signing when backfilled refs change the
    /// fingerprint. Builder-style. Takes an `Arc` so main.rs can
    /// share one key between StoreServiceImpl (PutPath signing)
    /// and StoreAdminServiceImpl (ResignPaths re-signing) — same
    /// key, same `Sig:` line in the narinfo either way.
    pub fn with_signer(mut self, signer: Arc<Signer>) -> Self {
        self.signer = Some(signer);
        self
    }

    /// Per-row body for ResignPaths wet-run: reassemble NAR, scan
    /// for refs, UPDATE if changed. Extracted so the batch loop's
    /// error handling is a single match arm (anything that bubbles
    /// out here → warn + `failed += 1` + leave `refs_backfilled=
    /// FALSE` for retry on the next pass).
    ///
    /// Returns `Ok(true)` if refs changed, `Ok(false)` if unchanged.
    /// Either way the row is marked `refs_backfilled = TRUE` on
    /// success — "backfilled" means "scanned", not "had new refs".
    async fn resign_one(
        &self,
        store_path_hash: &[u8],
        store_path: &str,
        nar_hash: &[u8],
        nar_size: i64,
        old_refs: &[String],
        candidates: &CandidateSet,
    ) -> Result<bool, Status> {
        // --- 1. Reassemble NAR → RefScanSink ------------------------
        // get_manifest is the one inline-vs-chunked branch point (see
        // metadata/queries.rs). Both arms feed the sink directly —
        // no intermediate Vec, just streaming writes. RefScanSink's
        // seam-buffer handles chunk-boundary straddling for us.
        let manifest = metadata::get_manifest(&self.pool, store_path)
            .await
            .map_err(|e| crate::grpc::metadata_status("ResignPaths: get_manifest", e))?
            .ok_or_else(|| {
                // Batch SELECT joins on manifests.status='complete',
                // so this is a concurrent-delete race (GC, manual
                // surgery). Fail the row; next pass won't see it.
                Status::not_found(format!(
                    "manifest gone for {store_path} (concurrent delete?)"
                ))
            })?;

        let mut sink = RefScanSink::new(candidates.hashes());
        match manifest {
            ManifestKind::Inline(bytes) => {
                // io::Write on an in-memory sink is infallible; the
                // map_err is just type plumbing for ? ergonomics.
                sink.write_all(&bytes)
                    .map_err(|e| Status::internal(format!("refscan write: {e}")))?;
            }
            ManifestKind::Chunked(entries) => {
                let cache = self.chunk_cache.as_ref().ok_or_else(|| {
                    Status::failed_precondition(
                        "path is chunked but admin service has no chunk_cache; \
                         wire .with_chunk_cache() in main.rs",
                    )
                })?;
                // Sequential fetch — no K=8 prefetch here. Backfill
                // is a one-shot maintenance job, not the hot GetPath
                // path; simplicity over throughput. The sink writes
                // are in-memory (no backpressure to hide with
                // prefetch). If the backfill proves too slow on a
                // 10M-path store, the prefetch pattern from
                // get_path.rs drops in cleanly (both loops iterate
                // the same `entries` vec).
                for (hash, _size) in &entries {
                    let chunk = cache.get_verified(hash).await.map_err(|e| {
                        Status::data_loss(format!("chunk reassembly for {store_path}: {e}"))
                    })?;
                    sink.write_all(&chunk)
                        .map_err(|e| Status::internal(format!("refscan write: {e}")))?;
                }
            }
        }

        // --- 2. Resolve found hashes → full paths -------------------
        // resolve() sorts lexicographically — MUST be stable because
        // the fingerprint hashes over the sorted list (Nix uses a
        // BTreeSet for the same reason; see narinfo::fingerprint).
        let new_refs = candidates.resolve(&sink.into_found());

        // --- 3. Compare + UPDATE ------------------------------------
        // old_refs came from PG TEXT[] (order preserved but not
        // guaranteed sorted — pre-fix rows are '{}' anyway, but
        // belt-and-suspenders: sort both before comparing). If
        // unchanged, just flip refs_backfilled and we're done — old
        // signature still covers the same fingerprint.
        let mut old_sorted = old_refs.to_vec();
        old_sorted.sort();
        let refs_changed = old_sorted != new_refs;

        if !refs_changed {
            sqlx::query("UPDATE narinfo SET refs_backfilled = TRUE WHERE store_path_hash = $1")
                .bind(store_path_hash)
                .execute(&self.pool)
                .await
                .map_err(|e| crate::grpc::internal_error("ResignPaths: mark backfilled", e))?;
            return Ok(false);
        }

        // Refs changed → old signature (if any) covers a stale
        // fingerprint. Append a fresh one if we have a key. Per the
        // remediation doc (02-empty-references §4.2): APPEND, don't
        // replace — Nix verifies against ANY sig from a trusted key;
        // the stale sig is harmless noise (won't verify, ignored).
        let new_sig: Option<String> = self.signer.as_ref().map(|signer| {
            // nar_hash is BYTEA in PG → Vec<u8>. fingerprint() wants
            // &[u8;32]. Schema says SHA-256 so this holds; if it
            // doesn't, the DB is corrupt — pad/truncate to 32 rather
            // than panic (corrupt hash → bad sig → unverifiable,
            // which surfaces the corruption where it matters).
            let mut nh = [0u8; 32];
            let take = nar_hash.len().min(32);
            nh[..take].copy_from_slice(&nar_hash[..take]);
            let fp = rio_nix::narinfo::fingerprint(store_path, &nh, nar_size as u64, &new_refs);
            signer.sign(&fp)
        });

        // Single UPDATE: refs + (optional) sig-append + backfilled
        // flag. Single statement = atomic without an explicit
        // transaction — if this fails, the row stays exactly as it
        // was (refs_backfilled=FALSE → retried next pass).
        match new_sig {
            Some(sig) => {
                sqlx::query(
                    r#"UPDATE narinfo
                       SET "references" = $1,
                           signatures = array_append(signatures, $2),
                           refs_backfilled = TRUE
                       WHERE store_path_hash = $3"#,
                )
                .bind(&new_refs)
                .bind(&sig)
                .bind(store_path_hash)
                .execute(&self.pool)
                .await
                .map_err(|e| crate::grpc::internal_error("ResignPaths: update refs+sig", e))?;
                debug!(store_path, refs = new_refs.len(), key = %self.signer.as_ref().unwrap().key_name(), "backfilled refs + re-signed");
            }
            None => {
                sqlx::query(
                    r#"UPDATE narinfo
                       SET "references" = $1,
                           refs_backfilled = TRUE
                       WHERE store_path_hash = $2"#,
                )
                .bind(&new_refs)
                .bind(store_path_hash)
                .execute(&self.pool)
                .await
                .map_err(|e| crate::grpc::internal_error("ResignPaths: update refs", e))?;
                debug!(
                    store_path,
                    refs = new_refs.len(),
                    "backfilled refs (no signer — not re-signed)"
                );
            }
        }

        Ok(true)
    }
}

/// Default threshold for the empty-refs safety gate. 10% is
/// intentionally low: in a healthy post-fix store, empty-ref non-CA
/// paths should be ~0% (only genuinely ref-free outputs like static
/// binaries). 10% gives headroom for legitimate cases without allowing
/// a pre-fix store (where it'd be ~100%) through.
const GC_EMPTY_REFS_THRESHOLD_PCT: f64 = 10.0;

/// Default batch size for ResignPaths when the client passes 0.
/// Each path is a NAR-reassembly + refscan pass; 100 keeps per-call
/// latency bounded while amortizing the gRPC roundtrip.
const RESIGN_BATCH_DEFAULT: i64 = 100;
/// Hard cap on ResignPaths batch size. Larger batches hold the batch
/// SELECT's snapshot longer and bloat the per-call response.
const RESIGN_BATCH_MAX: i64 = 1000;

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
        error!(
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

type TriggerGcStream = ReceiverStream<Result<GcProgress, Status>>;

#[tonic::async_trait]
impl rio_proto::StoreAdminService for StoreAdminServiceImpl {
    type TriggerGCStream = TriggerGcStream;

    /// Two-phase GC: mark (recursive CTE from roots) + sweep
    /// (DELETE CASCADE + chunk decrement + pending_s3_deletes).
    /// Streams progress updates: one message after mark (scanned
    /// count), one after sweep (collected + bytes), final with
    /// is_complete=true.
    ///
    /// `grace_period_hours`: None = default (2h). Some(0) = zero
    /// grace (explicit). Protects paths
    /// created in the last N hours from collection even if not
    /// yet referenced.
    ///
    /// `extra_roots`: scheduler-populated live-build output paths.
    /// May not be in narinfo yet (worker hasn't uploaded) — mark's
    /// CTE handles this gracefully (unnest of non-existent path
    /// = 0 rows, the root itself stays in reachable).
    ///
    /// `dry_run`: compute stats, ROLLBACK sweep tx. Operator sees
    /// "would delete N paths, free M bytes" without committing.
    #[instrument(skip(self, request), fields(rpc = "TriggerGC"))]
    async fn trigger_gc(
        &self,
        request: Request<GcRequest>,
    ) -> Result<Response<Self::TriggerGCStream>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let req = request.into_inner();

        // Bound extra_roots BEFORE spawning. Mark runs under
        // GC_MARK_LOCK_ID exclusive; a 10M-element array stalls the CTE
        // on unnest() and blocks every PutPath (shared side of same lock)
        // for the duration. Reuse MAX_BATCH_PATHS — 10k live-build outputs
        // is already implausible; GcRoots actor sends ~tens.
        rio_common::grpc::check_bound(
            "extra_roots",
            req.extra_roots.len(),
            crate::grpc::MAX_BATCH_PATHS,
        )?;
        // Syntactically valid store paths only. Not-in-narinfo is fine
        // (in-flight outputs, mark's seed-d handles it); garbage strings
        // are not — the CTE join on store_path = store_path against junk
        // is dead weight at best, a false match at worst.
        for root in &req.extra_roots {
            crate::grpc::validate_store_path(root)?;
        }

        // proto3 `optional uint32`: None = unset (use default 2h),
        // Some(0) = explicit zero grace (useful for tests + pre-shutdown GC).
        // Distinguishing these matters — treating 0 as "use default"
        // would make zero-grace impossible to request.
        //
        // Clamp at the gRPC boundary too (mark.rs also clamps — defense in
        // depth against callers that bypass admin.rs). u32 > i32::MAX wraps
        // negative → make_interval(hours => negative) → grace protects
        // nothing. 24*365 = one-year ceiling; log shows effective value.
        let grace_hours = req.grace_period_hours.unwrap_or(2).min(24 * 365);

        info!(
            dry_run = req.dry_run,
            grace_hours,
            extra_roots = req.extra_roots.len(),
            "TriggerGC: starting mark phase"
        );

        // Channel for streaming progress. spawn the actual GC
        // so this handler returns quickly and the client gets
        // progress updates as they happen.
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let pool = self.pool.clone();
        let chunk_backend = self.chunk_backend.clone();

        tokio::spawn(async move {
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
            let mut lock_conn = match pool.acquire().await {
                Ok(c) => c,
                Err(e) => {
                    warn!(error = %e, "TriggerGC: pool acquire for advisory lock failed");
                    let _ = tx
                        .send(Err(Status::internal(format!("pool acquire: {e}"))))
                        .await;
                    return;
                }
            };
            let lock_acquired: bool = match sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
                .bind(GC_LOCK_ID)
                .fetch_one(&mut *lock_conn)
                .await
            {
                Ok(b) => b,
                Err(e) => {
                    warn!(error = %e, "TriggerGC: advisory lock query failed");
                    let _ = tx
                        .send(Err(Status::internal(format!("advisory lock: {e}"))))
                        .await;
                    return;
                }
            };
            if !lock_acquired {
                info!("TriggerGC: another GC is already running, returning early");
                let _ = tx
                    .send(Ok(GcProgress {
                        paths_scanned: 0,
                        paths_collected: 0,
                        bytes_freed: 0,
                        is_complete: true,
                        current_path: "already running (concurrent GC in progress)".into(),
                    }))
                    .await;
                return;
            }
            // lock_conn held for the whole GC; explicit unlock at
            // the end via gc_unlock.
            //
            // scopeguard detaches on ANY exit not going through gc_unlock —
            // including task cancellation (client drops the stream → tonic
            // may abort this tokio::spawn) and panics. detach() removes the
            // connection from the pool; dropping the detached connection
            // closes it → PG releases the session-scoped lock.
            //
            // Without this, cancel/panic would leave the connection in the
            // pool with the lock held → next TriggerGC gets "already running"
            // until sqlx recycles that pooled connection (possibly hours).
            //
            // gc_unlock DEFUSES the scopeguard (ScopeGuard::into_inner)
            // and explicitly unlocks + returns conn to pool (cheaper
            // than detach on the happy path).
            let lock_conn = scopeguard::guard(lock_conn, |c| {
                let _ = c.detach();
            });
            async fn gc_unlock(
                conn: scopeguard::ScopeGuard<
                    sqlx::pool::PoolConnection<sqlx::Postgres>,
                    impl FnOnce(sqlx::pool::PoolConnection<sqlx::Postgres>),
                >,
            ) {
                let mut conn = scopeguard::ScopeGuard::into_inner(conn);
                if let Err(e) = sqlx::query("SELECT pg_advisory_unlock($1)")
                    .bind(GC_LOCK_ID)
                    .execute(&mut *conn)
                    .await
                {
                    warn!(error = %e, "TriggerGC: advisory unlock failed");
                }
            }

            // r[impl store.gc.empty-refs-gate]
            // Safety gate: refuse if >threshold% of sweep-eligible narinfo
            // have empty refs (pre-refscan data). Runs AFTER advisory lock
            // so two gated requests don't race on the check. Skip if
            // force=true, but still log so the override is visible.
            if !req.force {
                if let Err(e) =
                    check_empty_refs_gate(&pool, grace_hours, GC_EMPTY_REFS_THRESHOLD_PCT).await
                {
                    let _ = tx.send(Err(e)).await;
                    gc_unlock(lock_conn).await;
                    return;
                }
            } else {
                warn!("TriggerGC: force=true — bypassing empty-refs safety gate");
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
            let mark_lock_conn = match pool.acquire().await {
                Ok(c) => c,
                Err(e) => {
                    warn!(error = %e, "TriggerGC: pool acquire for mark lock failed");
                    let _ = tx
                        .send(Err(Status::internal(format!("mark lock acquire: {e}"))))
                        .await;
                    gc_unlock(lock_conn).await;
                    return;
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
                warn!(error = %e, "TriggerGC: mark advisory lock query failed");
                let _ = tx
                    .send(Err(Status::internal(format!("mark lock: {e}"))))
                    .await;
                gc_unlock(lock_conn).await;
                return; // mark_lock_guard detached by scopeguard
            }

            let unreachable =
                match gc::mark::compute_unreachable(&pool, grace_hours, &req.extra_roots).await {
                    Ok(u) => u,
                    Err(e) => {
                        warn!(error = %e, "TriggerGC: mark phase failed");
                        let _ = tx
                            .send(Err(Status::internal(format!("mark phase: {e}"))))
                            .await;
                        gc_unlock(lock_conn).await;
                        return; // mark_lock_guard detached by scopeguard
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
                warn!(error = %e, "TriggerGC: mark advisory unlock failed (continuing — conn drop will release)");
            }
            drop(mark_lock_conn); // returns to pool (lock already released)

            // Progress after mark: scanned count. We don't have
            // a "total paths" count cheaply (would need COUNT(*)
            // on narinfo), so paths_scanned = unreachable count
            // (what mark found). Not ideal but informative.
            let _ = tx
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
                "TriggerGC: mark complete, starting sweep"
            );

            // --- Sweep phase ---
            let stats =
                match gc::sweep::sweep(&pool, chunk_backend.as_ref(), unreachable, req.dry_run)
                    .await
                {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(error = %e, "TriggerGC: sweep phase failed");
                        let _ = tx
                            .send(Err(Status::internal(format!("sweep phase: {e}"))))
                            .await;
                        gc_unlock(lock_conn).await;
                        return;
                    }
                };

            // Final progress: complete with stats.
            let _ = tx
                .send(Ok(GcProgress {
                    paths_scanned: stats.paths_deleted, // reuse for "found unreachable"
                    paths_collected: stats.paths_deleted,
                    bytes_freed: stats.bytes_freed,
                    is_complete: true,
                    current_path: if req.dry_run {
                        format!(
                            "dry run: would delete {} paths, {} chunks, free {} bytes",
                            stats.paths_deleted, stats.chunks_deleted, stats.bytes_freed
                        )
                    } else {
                        format!(
                            "complete: {} paths deleted, {} chunks, {} S3 keys enqueued, {} bytes freed",
                            stats.paths_deleted, stats.chunks_deleted,
                            stats.s3_keys_enqueued, stats.bytes_freed
                        )
                    },
                }))
                .await;

            gc_unlock(lock_conn).await;
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    /// Pin a store path to gc_roots. FK ensures the path exists
    /// in narinfo (CASCADE DELETE removes the pin if narinfo
    /// goes — manual intervention, schema reset).
    #[instrument(skip(self), fields(rpc = "PinPath", path = %request.get_ref().store_path))]
    async fn pin_path(
        &self,
        request: Request<PinPathRequest>,
    ) -> Result<Response<PinPathResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let req = request.into_inner();
        crate::grpc::validate_store_path(&req.store_path)?;
        // Compute store_path_hash from the path. narinfo keys on
        // SHA-256 of the store path string.
        use sha2::Digest;
        let hash: Vec<u8> = sha2::Sha256::digest(req.store_path.as_bytes()).to_vec();

        // INSERT into gc_roots. FK to narinfo: if the path isn't
        // in narinfo, PG rejects with a foreign-key violation.
        // ON CONFLICT DO UPDATE: re-pin updates the source/
        // timestamp (idempotent — pinning an already-pinned path
        // is fine).
        match sqlx::query(
            r#"
            INSERT INTO gc_roots (store_path_hash, source)
            VALUES ($1, $2)
            ON CONFLICT (store_path_hash) DO UPDATE
                SET source = EXCLUDED.source, pinned_at = now()
            "#,
        )
        .bind(&hash)
        .bind(&req.source)
        .execute(&self.pool)
        .await
        {
            Ok(_) => {
                info!(path = %req.store_path, source = %req.source, "pinned");
                Ok(Response::new(PinPathResponse {
                    success: true,
                    message: String::new(),
                }))
            }
            Err(e) => {
                // FK violation (path not in narinfo) → NotFound.
                // Anything else → Internal. Use SQLSTATE 23503
                // (foreign_key_violation) — stable across locales
                // and PG versions, unlike string matching.
                let is_fk_violation = e
                    .as_database_error()
                    .and_then(|d| d.code())
                    .is_some_and(|c| c == "23503");
                if is_fk_violation {
                    Ok(Response::new(PinPathResponse {
                        success: false,
                        message: format!("path not in store: {}", req.store_path),
                    }))
                } else {
                    // Log the full sqlx chain server-side; don't leak it.
                    // Same pattern as grpc::internal_error (mod.rs).
                    Err(crate::grpc::internal_error("PinPath: insert gc_roots", e))
                }
            }
        }
    }

    /// Unpin: DELETE from gc_roots. Idempotent — unpinning a
    /// non-pinned path is a no-op (DELETE rowcount 0).
    #[instrument(skip(self), fields(rpc = "UnpinPath", path = %request.get_ref().store_path))]
    async fn unpin_path(
        &self,
        request: Request<PinPathRequest>,
    ) -> Result<Response<PinPathResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let req = request.into_inner();
        crate::grpc::validate_store_path(&req.store_path)?;
        use sha2::Digest;
        let hash: Vec<u8> = sha2::Sha256::digest(req.store_path.as_bytes()).to_vec();

        sqlx::query("DELETE FROM gc_roots WHERE store_path_hash = $1")
            .bind(&hash)
            .execute(&self.pool)
            .await
            .map_err(|e| crate::grpc::internal_error("UnpinPath: delete gc_roots", e))?;

        info!(path = %req.store_path, "unpinned");
        Ok(Response::new(PinPathResponse {
            success: true,
            message: String::new(),
        }))
    }

    /// Backfill references + re-sign for paths uploaded before the worker
    /// refscan fix. Migration 010 marks all pre-existing rows
    /// `refs_backfilled = FALSE`; this RPC walks them in
    /// `store_path_hash` order (cursor-paginated) so the caller can
    /// resume after a crash without re-processing.
    ///
    /// Candidate-set strategy (PR 4b design decision): the original
    /// upload knew its derivation's input closure — we don't. The
    /// `deriver` column has the .drv path but the .drv content may
    /// not be in the store. Rather than chase it, we scan against ALL
    /// paths with `created_at <= this.created_at` (a path can only
    /// reference paths that existed when it was built). Correct but
    /// broader than the true input closure; false positives need a
    /// 32-byte nixbase32 collision (2^-160, cryptographically
    /// negligible). Bounded by total store size — fine at current
    /// scale, revisit if the store grows past ~10M paths.
    #[instrument(skip(self), fields(rpc = "ResignPaths", cursor = %request.get_ref().cursor, batch = request.get_ref().batch_size))]
    async fn resign_paths(
        &self,
        request: Request<ResignPathsRequest>,
    ) -> Result<Response<ResignPathsResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let req = request.into_inner();

        // Decode cursor (hex store_path_hash) → bytes. Empty = start.
        // store_path_hash is SHA-256 = 32 bytes = 64 hex chars.
        let cursor_bytes: Vec<u8> = if req.cursor.is_empty() {
            Vec::new()
        } else {
            hex::decode(&req.cursor).map_err(|_| {
                Status::invalid_argument("cursor must be hex-encoded store_path_hash")
            })?
        };

        let batch = match req.batch_size as i64 {
            0 => RESIGN_BATCH_DEFAULT,
            n => n.min(RESIGN_BATCH_MAX),
        };

        // Cursor-paginated scan. `store_path_hash > $1` with an empty
        // initial cursor works because BYTEA '' sorts before every
        // non-empty BYTEA in PG (memcmp semantics). The partial index
        // `narinfo_refs_backfill_pending_idx` (migration 010) covers
        // `WHERE refs_backfilled = FALSE` so this is an index-range
        // scan, not a seq-scan of narinfo.
        //
        // JOIN on manifests.status='complete': a path with no
        // complete manifest can't have its NAR reassembled (would
        // fail inside resign_one anyway). Filter here so such rows
        // stay `refs_backfilled=FALSE` without burning a `failed`
        // tick — they'll be picked up once their upload completes.
        //
        // EXTRACT(EPOCH ...)::bigint: sqlx has no chrono/time
        // feature in this workspace, so pull created_at as epoch
        // seconds (i64) for the in-memory candidate filter.
        // Second-resolution is plenty: "which paths existed before
        // this one" doesn't need microseconds.
        type BatchRow = (Vec<u8>, String, Vec<u8>, i64, Vec<String>, i64);
        let rows: Vec<BatchRow> = sqlx::query_as(
            r#"
            SELECT n.store_path_hash, n.store_path, n.nar_hash, n.nar_size,
                   n."references",
                   EXTRACT(EPOCH FROM n.created_at)::bigint
            FROM narinfo n
            JOIN manifests m ON m.store_path_hash = n.store_path_hash
            WHERE n.refs_backfilled = FALSE
              AND m.status = 'complete'
              AND n.store_path_hash > $1
            ORDER BY n.store_path_hash
            LIMIT $2
            "#,
        )
        .bind(&cursor_bytes)
        .bind(batch)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| crate::grpc::internal_error("ResignPaths: select batch", e))?;

        let next_cursor = rows
            .last()
            .map(|(h, ..)| hex::encode(h))
            .unwrap_or_default();

        if req.dry_run {
            // Survey mode: report how many pending paths exist in this
            // batch without touching them. Caller can page through to
            // sum the total queue depth.
            let processed = rows.len() as u32;
            info!(processed, next_cursor = %next_cursor, "resign dry-run batch");
            return Ok(Response::new(ResignPathsResponse {
                cursor: next_cursor,
                processed,
                refs_changed: 0,
                failed: 0,
            }));
        }

        if rows.is_empty() {
            // Nothing to do — skip the (potentially large) universe
            // fetch entirely. Empty cursor signals "done" to the
            // caller's pagination loop.
            return Ok(Response::new(ResignPathsResponse {
                cursor: String::new(),
                processed: 0,
                refs_changed: 0,
                failed: 0,
            }));
        }

        // --- Candidate universe (fetched ONCE per batch) ------------
        // All store paths + their creation epoch. Filter in-memory per
        // row below (`created_at <= this.created_at`). This trades one
        // full-table scan per BATCH for N per-row range scans — at
        // batch_size=100 that's a 100× saving. Memory cost: ~100B/path
        // (path string + i64); a 1M-path store is ~100MB, fine for a
        // maintenance job that runs once.
        //
        // Sorted by created_epoch so the per-row filter is a single
        // partition_point lookup (binary search) instead of a full
        // Vec re-filter — O(log n) per row vs. O(n).
        let all_paths: Vec<(String, i64)> = sqlx::query_as(
            "SELECT store_path, EXTRACT(EPOCH FROM created_at)::bigint \
             FROM narinfo ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| crate::grpc::internal_error("ResignPaths: load candidate universe", e))?;

        // --- Per-row body -------------------------------------------
        // Counters: processed = rows that REACHED the end (marked
        // TRUE); failed = rows that errored (still FALSE, retried
        // next pass). These are disjoint — a failed row is NOT in
        // `processed`. Matches the proto field semantics.
        let mut processed = 0u32;
        let mut refs_changed = 0u32;
        let mut failed = 0u32;

        for (hash, store_path, nar_hash, nar_size, old_refs, created_epoch) in &rows {
            // Candidate set: everything with created_at <= this row's.
            // partition_point on the sorted universe gives the prefix
            // length; slicing avoids cloning the Vec.
            let cutoff = all_paths.partition_point(|(_, t)| *t <= *created_epoch);
            let cands = CandidateSet::from_paths(all_paths[..cutoff].iter().map(|(p, _)| p));

            match self
                .resign_one(hash, store_path, nar_hash, *nar_size, old_refs, &cands)
                .await
            {
                Ok(changed) => {
                    processed += 1;
                    if changed {
                        refs_changed += 1;
                    }
                }
                Err(e) => {
                    // Leave refs_backfilled=FALSE — next pass retries.
                    // warn! not error!: a single bad path (corrupt
                    // manifest, S3 blip) shouldn't page someone; the
                    // `failed` counter in the response is the signal.
                    warn!(store_path = %store_path, code = ?e.code(), msg = %e.message(), "resign row failed; leaving for retry");
                    failed += 1;
                }
            }
        }

        info!(processed, refs_changed, failed, next_cursor = %next_cursor, "resign wet batch done");
        Ok(Response::new(ResignPathsResponse {
            cursor: next_cursor,
            processed,
            refs_changed,
            failed,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_proto::StoreAdminService;
    use rio_test_support::TestDb;

    /// PinPath for a path NOT in narinfo → FK violation → success=
    /// false with "path not in store" message. Verifies the SQLSTATE
    /// 23503 check (replacing the old string-match on "foreign key",
    /// which is locale/version-dependent).
    #[tokio::test]
    async fn pin_path_fk_violation_returns_not_found() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);

        // Pin a path that doesn't exist in narinfo. The FK to
        // narinfo.store_path_hash will reject with SQLSTATE 23503.
        let req = Request::new(PinPathRequest {
            store_path: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-doesnt-exist".into(),
            source: "test".into(),
        });
        let resp = svc.pin_path(req).await.expect("returns Ok(response)");
        let resp = resp.into_inner();
        assert!(!resp.success, "FK violation → success=false");
        assert!(
            resp.message.contains("path not in store"),
            "message should say path not in store, got: {}",
            resp.message
        );
    }

    #[tokio::test]
    async fn pin_path_roundtrip() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);

        // Seed narinfo so the FK passes.
        let path = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-exists";
        use sha2::Digest;
        let hash: Vec<u8> = sha2::Sha256::digest(path.as_bytes()).to_vec();
        sqlx::query(
            "INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size) \
             VALUES ($1, $2, $3, 42)",
        )
        .bind(&hash)
        .bind(path)
        .bind(vec![0u8; 32])
        .execute(&db.pool)
        .await
        .unwrap();

        // Pin.
        let resp = svc
            .pin_path(Request::new(PinPathRequest {
                store_path: path.into(),
                source: "test-roundtrip".into(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(resp.success, "pin should succeed");

        // Verify gc_roots has the row.
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM gc_roots")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(count.0, 1, "gc_roots has 1 row");

        // Unpin.
        let resp = svc
            .unpin_path(Request::new(PinPathRequest {
                store_path: path.into(),
                source: String::new(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(resp.success, "unpin should succeed");

        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM gc_roots")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(count.0, 0, "gc_roots empty after unpin");
    }

    /// Concurrent TriggerGC calls are serialized via advisory
    /// lock. Second call returns immediately with "already running".
    #[tokio::test]
    async fn trigger_gc_advisory_lock_serializes() {
        use futures_util::StreamExt;
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);

        // Acquire the advisory lock MANUALLY on a held connection,
        // simulating an in-flight GC. Then call TriggerGC and
        // verify it returns "already running".
        let mut lock_conn = db.pool.acquire().await.unwrap();
        let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(GC_LOCK_ID)
            .fetch_one(&mut *lock_conn)
            .await
            .unwrap();
        assert!(locked, "initial manual lock should succeed");

        // TriggerGC should see lock held → return "already running".
        let resp = svc
            .trigger_gc(Request::new(GcRequest {
                dry_run: true,
                grace_period_hours: Some(24),
                extra_roots: vec![],
                force: false,
            }))
            .await
            .unwrap();
        let mut stream = resp.into_inner();
        let msg = stream.next().await.expect("one message").unwrap();
        assert!(msg.is_complete, "should be complete immediately");
        assert!(
            msg.current_path.contains("already running"),
            "should report already running, got: {}",
            msg.current_path
        );

        // Release manual lock; next TriggerGC should proceed.
        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(GC_LOCK_ID)
            .execute(&mut *lock_conn)
            .await
            .unwrap();
        drop(lock_conn);

        let resp = svc
            .trigger_gc(Request::new(GcRequest {
                dry_run: true,
                grace_period_hours: Some(24),
                extra_roots: vec![],
                force: false,
            }))
            .await
            .unwrap();
        let mut stream = resp.into_inner();
        // Empty store → mark finds nothing → one "mark complete"
        // progress + one final. Drain to completion.
        let mut last = None;
        while let Some(msg) = stream.next().await {
            last = Some(msg.unwrap());
        }
        let last = last.expect("at least one message");
        assert!(last.is_complete, "final message should be complete");
        assert!(
            !last.current_path.contains("already running"),
            "should NOT be 'already running' after lock released"
        );
    }

    /// T2: extra_roots bounded at MAX_BATCH_PATHS (10_000). 10_001 → reject.
    #[tokio::test]
    async fn trigger_gc_rejects_oversized_extra_roots() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);
        // Syntactically valid paths (validate_store_path passes each).
        let roots: Vec<String> = (0..crate::grpc::MAX_BATCH_PATHS + 1)
            .map(|i| rio_test_support::fixtures::test_store_path(&format!("r{i}")))
            .collect();
        let err = svc
            .trigger_gc(Request::new(GcRequest {
                extra_roots: roots,
                dry_run: true,
                grace_period_hours: Some(24),
                force: false,
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(
            err.message().contains("extra_roots"),
            "message should name the field, got: {}",
            err.message()
        );
    }

    /// T2: extra_roots rejects syntactically invalid store paths.
    #[tokio::test]
    async fn trigger_gc_rejects_malformed_extra_root() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);
        let err = svc
            .trigger_gc(Request::new(GcRequest {
                extra_roots: vec!["not-a-store-path".into()],
                dry_run: true,
                grace_period_hours: Some(24),
                force: false,
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    /// T5: PinPath rejects malformed store paths (path traversal,
    /// garbage) with INVALID_ARGUMENT, BEFORE touching the DB.
    #[tokio::test]
    async fn pin_path_rejects_malformed() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);
        let err = svc
            .pin_path(Request::new(PinPathRequest {
                store_path: "../../etc/passwd".into(),
                source: "t".into(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    /// T5: UnpinPath rejects malformed store paths.
    #[tokio::test]
    async fn unpin_path_rejects_malformed() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);
        let err = svc
            .unpin_path(Request::new(PinPathRequest {
                store_path: "not-a-path".into(),
                source: String::new(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    /// T5: PinPath sqlx errors are scrubbed — generic message only.
    /// Force a non-FK DB error by closing the pool first.
    #[tokio::test]
    async fn pin_path_internal_error_is_scrubbed() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);
        // Close the pool: all acquires fail with PoolClosed, which is
        // NOT a FK violation (23503), so it hits the internal_error path.
        db.pool.close().await;
        let err = svc
            .pin_path(Request::new(PinPathRequest {
                store_path: rio_test_support::fixtures::test_store_path("p"),
                source: "t".into(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Internal);
        // The message must NOT contain sqlx/postgres internals.
        assert!(
            !err.message().to_lowercase().contains("pool"),
            "message leaked pool detail: {}",
            err.message()
        );
        assert!(
            !err.message().to_lowercase().contains("sqlx"),
            "message leaked sqlx: {}",
            err.message()
        );
        // internal_error() emits this generic string.
        assert_eq!(err.message(), "storage operation failed");
    }

    // --- GC empty-refs safety gate (remediation 02) ---

    /// Seed a narinfo+manifests row pair for gate tests.
    /// `created_at` is forced into the past so the row is sweep-eligible
    /// under any non-zero grace window. `ca` and `references` are the
    /// two axes the gate pivots on.
    async fn seed_narinfo_for_gate(pool: &PgPool, name: &str, refs: &[String], ca: Option<&str>) {
        use sha2::Digest;
        let path = rio_test_support::fixtures::test_store_path(name);
        let hash: Vec<u8> = sha2::Sha256::digest(path.as_bytes()).to_vec();
        // created_at = 7 days ago — comfortably past any test grace.
        sqlx::query(
            r#"INSERT INTO narinfo
               (store_path_hash, store_path, nar_hash, nar_size, "references", ca, created_at)
               VALUES ($1, $2, $3, 42, $4, $5, now() - interval '7 days')"#,
        )
        .bind(&hash)
        .bind(&path)
        .bind(vec![0u8; 32])
        .bind(refs)
        .bind(ca)
        .execute(pool)
        .await
        .expect("seed narinfo");
        // Manifest must be 'complete' — gate query joins on this.
        sqlx::query("INSERT INTO manifests (store_path_hash, status) VALUES ($1, 'complete')")
            .bind(&hash)
            .execute(pool)
            .await
            .expect("seed manifest");
    }

    /// Drain the TriggerGC stream to the first Err, or return the final
    /// Ok(GcProgress) if no Err arrives. GC gate failures arrive as an
    /// Err(Status) over the stream (the handler returns Ok(stream) then
    /// the spawned task sends Err).
    async fn drain_gc_stream(
        stream: &mut (impl futures_util::Stream<Item = Result<GcProgress, Status>> + Unpin),
    ) -> Result<GcProgress, Status> {
        use futures_util::StreamExt;
        let mut last_ok = None;
        while let Some(msg) = stream.next().await {
            match msg {
                Ok(p) => last_ok = Some(p),
                Err(e) => return Err(e),
            }
        }
        Ok(last_ok.expect("stream produced at least one message"))
    }

    /// r[verify store.gc.empty-refs-gate]
    /// Store with >10% empty-ref non-CA paths past grace → gate refuses
    /// with FailedPrecondition. Message names the ratio + suggests force.
    #[tokio::test]
    async fn gc_refuses_on_high_empty_ref_ratio() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);

        // 8 empty-ref non-CA + 2 with-refs = 80% empty. Well above 10%.
        for i in 0..8 {
            seed_narinfo_for_gate(&db.pool, &format!("empty-{i}"), &[], None).await;
        }
        let good_ref = vec![rio_test_support::fixtures::test_store_path("dep")];
        for i in 0..2 {
            seed_narinfo_for_gate(&db.pool, &format!("good-{i}"), &good_ref, None).await;
        }

        let resp = svc
            .trigger_gc(Request::new(GcRequest {
                dry_run: true,
                grace_period_hours: Some(1),
                extra_roots: vec![],
                force: false,
            }))
            .await
            .expect("handler returns Ok(stream)");
        let mut stream = resp.into_inner();

        let err = drain_gc_stream(&mut stream)
            .await
            .expect_err("gate should refuse");
        assert_eq!(
            err.code(),
            tonic::Code::FailedPrecondition,
            "gate returns FailedPrecondition, got {:?}: {}",
            err.code(),
            err.message()
        );
        // Message contains ratio + force hint.
        let msg = err.message();
        assert!(
            msg.contains("GC refused"),
            "message should say GC refused: {msg}"
        );
        assert!(
            msg.contains("8/10") && msg.contains("80.0%"),
            "message should show 8/10 (80.0%) ratio: {msg}"
        );
        assert!(
            msg.contains("force=true"),
            "message should hint force override: {msg}"
        );
    }

    /// r[verify store.gc.empty-refs-gate]
    /// Same high-ratio setup, but `force=true` bypasses the gate.
    /// GC proceeds to mark+sweep (dry-run so nothing actually deleted).
    #[tokio::test]
    async fn gc_force_overrides_empty_refs_gate() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);

        // 100% empty-ref non-CA.
        for i in 0..5 {
            seed_narinfo_for_gate(&db.pool, &format!("empty-{i}"), &[], None).await;
        }

        let resp = svc
            .trigger_gc(Request::new(GcRequest {
                dry_run: true, // don't actually sweep, just verify gate is bypassed
                grace_period_hours: Some(1),
                extra_roots: vec![],
                force: true,
            }))
            .await
            .expect("handler returns Ok(stream)");
        let mut stream = resp.into_inner();

        // Gate bypassed → mark+sweep runs → stream ends with Ok(complete).
        let final_msg = drain_gc_stream(&mut stream)
            .await
            .expect("force=true should bypass gate and complete");
        assert!(
            final_msg.is_complete,
            "force=true should let GC complete, got: {final_msg:?}"
        );
        assert!(
            !final_msg.current_path.contains("already running"),
            "should not be the advisory-lock early-return path"
        );
    }

    /// r[verify store.gc.empty-refs-gate]
    /// CA paths with empty refs are NOT counted toward the threshold
    /// (fetchurl outputs etc. legitimately have no runtime deps).
    /// 100% of paths have empty refs but they're all CA → gate passes.
    #[tokio::test]
    async fn gc_gate_excludes_ca_paths_from_ratio() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);

        // All empty-ref but all CA. Numerator = 0, pct = 0%.
        for i in 0..5 {
            seed_narinfo_for_gate(
                &db.pool,
                &format!("ca-{i}"),
                &[],
                Some("fixed:r:sha256:abc"),
            )
            .await;
        }

        let resp = svc
            .trigger_gc(Request::new(GcRequest {
                dry_run: true,
                grace_period_hours: Some(1),
                extra_roots: vec![],
                force: false,
            }))
            .await
            .expect("handler returns Ok(stream)");
        let mut stream = resp.into_inner();

        let final_msg = drain_gc_stream(&mut stream)
            .await
            .expect("CA-only store should pass gate");
        assert!(final_msg.is_complete);
    }

    /// r[verify store.gc.empty-refs-gate]
    /// Paths INSIDE the grace window don't count toward the ratio
    /// (they're protected from sweep anyway). Seed recent empty-ref
    /// paths + use a large grace → denominator = 0 → gate passes.
    #[tokio::test]
    async fn gc_gate_excludes_paths_inside_grace() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);

        // seed_narinfo_for_gate sets created_at = 7 days ago.
        // With grace = 30 days (720h), they're INSIDE grace → excluded.
        for i in 0..5 {
            seed_narinfo_for_gate(&db.pool, &format!("recent-{i}"), &[], None).await;
        }

        let resp = svc
            .trigger_gc(Request::new(GcRequest {
                dry_run: true,
                grace_period_hours: Some(720), // 30 days > 7 days
                extra_roots: vec![],
                force: false,
            }))
            .await
            .expect("handler returns Ok(stream)");
        let mut stream = resp.into_inner();

        let final_msg = drain_gc_stream(&mut stream)
            .await
            .expect("paths inside grace don't trip gate");
        assert!(final_msg.is_complete);
    }

    /// Seed a narinfo + complete-manifest row pair with refs_backfilled
    /// =FALSE (migration 010's "pre-fix" state). The batch SELECT joins
    /// on manifests.status='complete', so both rows are needed even for
    /// dry-run tests. Inline blob is empty — dry-run never scans it.
    async fn seed_narinfo_pending_backfill(pool: &PgPool, name: &str) -> Vec<u8> {
        use sha2::Digest;
        let path = rio_test_support::fixtures::test_store_path(name);
        let hash: Vec<u8> = sha2::Sha256::digest(path.as_bytes()).to_vec();
        sqlx::query(
            r#"INSERT INTO narinfo
               (store_path_hash, store_path, nar_hash, nar_size, "references", refs_backfilled)
               VALUES ($1, $2, $3, 42, '{}', FALSE)"#,
        )
        .bind(&hash)
        .bind(&path)
        .bind(vec![0u8; 32])
        .execute(pool)
        .await
        .expect("seed pending-backfill narinfo");
        sqlx::query(
            "INSERT INTO manifests (store_path_hash, status, inline_blob) \
             VALUES ($1, 'complete', ''::bytea)",
        )
        .bind(&hash)
        .execute(pool)
        .await
        .expect("seed pending-backfill manifest");
        hash
    }

    /// Wet-run seeder: distinct hash-part per path (so the ref-scanner
    /// can discriminate), explicit `created_at` offset (so the
    /// "candidates with created_at ≤ this" filter is deterministic),
    /// and an actual `inline_blob` to scan. Returns the full store
    /// path string for refs assertions.
    ///
    /// `hash_part` must be 32 valid nixbase32 chars (use '0'..'9' or
    /// 'a'/'b'/'c'/etc. padded — NOT 'e'/'o'/'t'/'u', those are
    /// excluded from the alphabet). `test_store_path` uses all-'a'
    /// which would collapse every path to the same CandidateSet entry.
    async fn seed_backfill_path(
        pool: &PgPool,
        hash_part: &str,
        name: &str,
        days_ago: i32,
        blob: &[u8],
    ) -> String {
        use sha2::Digest;
        assert_eq!(hash_part.len(), 32, "hash_part must be 32 nixbase32 chars");
        let path = format!("/nix/store/{hash_part}-{name}");
        let sph: Vec<u8> = sha2::Sha256::digest(path.as_bytes()).to_vec();
        // nar_hash = SHA-256(blob) so fingerprint() gets a real digest
        // (signature verify is out of scope here, but no reason to
        // seed garbage). nar_size = blob.len() for the same reason.
        let nar_hash: Vec<u8> = sha2::Sha256::digest(blob).to_vec();
        sqlx::query(
            r#"INSERT INTO narinfo
               (store_path_hash, store_path, nar_hash, nar_size, "references",
                refs_backfilled, created_at)
               VALUES ($1, $2, $3, $4, '{}', FALSE, now() - make_interval(days => $5))"#,
        )
        .bind(&sph)
        .bind(&path)
        .bind(&nar_hash)
        .bind(blob.len() as i64)
        .bind(days_ago)
        .execute(pool)
        .await
        .expect("seed wet narinfo");
        sqlx::query(
            "INSERT INTO manifests (store_path_hash, status, inline_blob) \
             VALUES ($1, 'complete', $2)",
        )
        .bind(&sph)
        .bind(blob)
        .execute(pool)
        .await
        .expect("seed wet manifest");
        path
    }

    /// ResignPaths dry_run cursor pagination: seed 3 pending rows,
    /// walk them in batches of 2 → first call returns 2 + cursor,
    /// second returns 1 + empty cursor. Verifies migration 010's
    /// column + partial index + the cursor encoding roundtrip.
    #[tokio::test]
    async fn resign_paths_dry_run_paginates() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);

        // Seed 3 pending + 1 already-backfilled (TRUE) + 1 post-fix
        // (NULL). Only the 3 FALSE rows should show up.
        for name in ["backfill-a", "backfill-b", "backfill-c"] {
            seed_narinfo_pending_backfill(&db.pool, name).await;
        }
        // Control rows that must NOT appear in the batch.
        for (name, state) in [("done", Some(true)), ("postfix", None::<bool>)] {
            use sha2::Digest;
            let path = rio_test_support::fixtures::test_store_path(name);
            let hash: Vec<u8> = sha2::Sha256::digest(path.as_bytes()).to_vec();
            sqlx::query(
                r#"INSERT INTO narinfo
                   (store_path_hash, store_path, nar_hash, nar_size, "references", refs_backfilled)
                   VALUES ($1, $2, $3, 42, '{}', $4)"#,
            )
            .bind(&hash)
            .bind(&path)
            .bind(vec![0u8; 32])
            .bind(state)
            .execute(&db.pool)
            .await
            .expect("seed control narinfo");
        }

        // Page 1: batch=2 from empty cursor.
        let resp1 = svc
            .resign_paths(Request::new(ResignPathsRequest {
                cursor: String::new(),
                batch_size: 2,
                dry_run: true,
            }))
            .await
            .expect("dry-run ok")
            .into_inner();
        assert_eq!(resp1.processed, 2);
        assert!(!resp1.cursor.is_empty(), "cursor should advance");
        // Cursor decodes back to a 32-byte SHA-256.
        assert_eq!(hex::decode(&resp1.cursor).expect("hex").len(), 32);

        // Page 2: resume from cursor → 1 left, then done.
        let resp2 = svc
            .resign_paths(Request::new(ResignPathsRequest {
                cursor: resp1.cursor,
                batch_size: 2,
                dry_run: true,
            }))
            .await
            .expect("dry-run ok")
            .into_inner();
        assert_eq!(resp2.processed, 1);
        assert!(
            !resp2.cursor.is_empty(),
            "cursor is last-seen, not empty-on-short-batch"
        );

        // Page 3: past the last row → empty.
        let resp3 = svc
            .resign_paths(Request::new(ResignPathsRequest {
                cursor: resp2.cursor,
                batch_size: 2,
                dry_run: true,
            }))
            .await
            .expect("dry-run ok")
            .into_inner();
        assert_eq!(resp3.processed, 0);
        assert!(resp3.cursor.is_empty(), "empty cursor signals done");
    }

    /// ResignPaths wet-run happy path: three paths with distinct
    /// hash-parts and controlled creation times. B's blob embeds A's
    /// hash; C's blob doesn't. After backfill: B.refs=[A], C.refs=[],
    /// all three refs_backfilled=TRUE, refs_changed=1 (just B).
    ///
    /// Also verifies the created_at filter: B is newer than A so A is
    /// in B's candidate set. A self-referencing test is implicit —
    /// A's blob doesn't contain A's own hash, so A.refs stays [].
    #[tokio::test]
    async fn resign_paths_wet_run_backfills_refs() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);

        // A: oldest. Blob is arbitrary bytes with no nixbase32 hash
        // substrings. '0' is valid nixbase32 but we don't have 32 of
        // them in a row — the scanner skips on the 'X'.
        let hash_a = "00000000000000000000000000000000";
        let path_a = seed_backfill_path(&db.pool, hash_a, "pkg-a", 30, b"X leaf content X").await;

        // B: newer. Blob contains A's hash embedded mid-stream —
        // this is what a real NAR looks like (paths appear as
        // /nix/store/<hash>-<name> strings inside binaries/scripts).
        // Pad with non-nixbase32 bytes ('T' is not in the alphabet)
        // so ONLY hash_a's window is a valid candidate.
        let blob_b = format!("TT padding TT /nix/store/{hash_a}-pkg-a TT more TT");
        let path_b = seed_backfill_path(
            &db.pool,
            "11111111111111111111111111111111",
            "pkg-b",
            20,
            blob_b.as_bytes(),
        )
        .await;

        // C: also newer. Blob has nothing that looks like a hash.
        let path_c = seed_backfill_path(
            &db.pool,
            "22222222222222222222222222222222",
            "pkg-c",
            10,
            b"T plain T",
        )
        .await;

        // Run the wet backfill (no signer — refs update only).
        let resp = svc
            .resign_paths(Request::new(ResignPathsRequest {
                cursor: String::new(),
                batch_size: 10,
                dry_run: false,
            }))
            .await
            .expect("wet run succeeds")
            .into_inner();

        assert_eq!(resp.processed, 3, "all three rows processed");
        assert_eq!(resp.refs_changed, 1, "only B had refs added");
        assert_eq!(resp.failed, 0);

        // Verify DB state directly — the response counters are the
        // contract, but the actual row state is what GC reads.
        let (refs_b, done_b): (Vec<String>, Option<bool>) = sqlx::query_as(
            r#"SELECT "references", refs_backfilled FROM narinfo WHERE store_path = $1"#,
        )
        .bind(&path_b)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(refs_b, vec![path_a.clone()], "B references A");
        assert_eq!(done_b, Some(true));

        // A and C: refs unchanged (empty), but still marked done.
        for p in [&path_a, &path_c] {
            let (refs, done): (Vec<String>, Option<bool>) = sqlx::query_as(
                r#"SELECT "references", refs_backfilled FROM narinfo WHERE store_path = $1"#,
            )
            .bind(p)
            .fetch_one(&db.pool)
            .await
            .unwrap();
            assert!(refs.is_empty(), "{p}: refs should be empty, got {refs:?}");
            assert_eq!(done, Some(true), "{p}: refs_backfilled should be TRUE");
        }
    }

    /// Wet-run with a signer: when refs change, a fresh signature is
    /// APPENDED (not replacing the old one — per remediation 02 §4.2
    /// the stale sig is harmless noise, Nix just ignores it). When
    /// refs DON'T change, signatures are left alone.
    #[tokio::test]
    async fn resign_paths_wet_run_appends_signature_on_change() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let seed_b64 =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [0x42u8; 32]);
        let signer =
            Signer::parse(&format!("test-resign-1:{seed_b64}")).expect("valid test signer");
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None).with_signer(Arc::new(signer));

        // A: leaf; B references A (blob embeds A's hash).
        let hash_a = "33333333333333333333333333333333";
        let path_a = seed_backfill_path(&db.pool, hash_a, "dep", 30, b"T no refs here T").await;
        let blob_b = format!("T see /nix/store/{hash_a}-dep T");
        let path_b = seed_backfill_path(
            &db.pool,
            "44444444444444444444444444444444",
            "app",
            20,
            blob_b.as_bytes(),
        )
        .await;

        // Pre-seed B with an "old" signature — simulates the pre-fix
        // state where PutPath signed empty refs. We want to see this
        // one preserved AND a new one appended.
        sqlx::query(
            "UPDATE narinfo SET signatures = ARRAY['old-key:stale-sig'] WHERE store_path = $1",
        )
        .bind(&path_b)
        .execute(&db.pool)
        .await
        .unwrap();

        let resp = svc
            .resign_paths(Request::new(ResignPathsRequest {
                cursor: String::new(),
                batch_size: 10,
                dry_run: false,
            }))
            .await
            .expect("wet run succeeds")
            .into_inner();
        assert_eq!(resp.refs_changed, 1);

        // B: old sig preserved + new sig appended.
        let (sigs_b, refs_b): (Vec<String>, Vec<String>) =
            sqlx::query_as(r#"SELECT signatures, "references" FROM narinfo WHERE store_path = $1"#)
                .bind(&path_b)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(refs_b, vec![path_a.clone()]);
        assert_eq!(sigs_b.len(), 2, "old sig + new sig");
        assert_eq!(sigs_b[0], "old-key:stale-sig", "old sig untouched");
        assert!(
            sigs_b[1].starts_with("test-resign-1:"),
            "new sig uses our key name, got: {}",
            sigs_b[1]
        );

        // A: refs unchanged → signatures untouched (still empty).
        let (sigs_a,): (Vec<String>,) =
            sqlx::query_as("SELECT signatures FROM narinfo WHERE store_path = $1")
                .bind(&path_a)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert!(
            sigs_a.is_empty(),
            "unchanged refs → no re-sign, got: {sigs_a:?}"
        );
    }

    /// Wet-run with NO signer: refs still update, but no signature
    /// is appended. Verifies the None-signer branch is genuinely a
    /// no-op on signatures (not e.g. appending an empty string).
    #[tokio::test]
    async fn resign_paths_wet_run_no_signer_updates_refs_only() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None); // no .with_signer

        let hash_a = "55555555555555555555555555555555";
        seed_backfill_path(&db.pool, hash_a, "lib", 30, b"T T").await;
        let blob_b = format!("T /nix/store/{hash_a}-lib T");
        let path_b = seed_backfill_path(
            &db.pool,
            "66666666666666666666666666666666",
            "bin",
            20,
            blob_b.as_bytes(),
        )
        .await;

        let resp = svc
            .resign_paths(Request::new(ResignPathsRequest {
                cursor: String::new(),
                batch_size: 10,
                dry_run: false,
            }))
            .await
            .expect("wet run succeeds")
            .into_inner();
        assert_eq!(resp.refs_changed, 1);

        let (sigs, refs, done): (Vec<String>, Vec<String>, Option<bool>) = sqlx::query_as(
            r#"SELECT signatures, "references", refs_backfilled FROM narinfo WHERE store_path = $1"#,
        )
        .bind(&path_b)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(refs.len(), 1, "refs backfilled");
        assert!(sigs.is_empty(), "no signer → no sig appended");
        assert_eq!(done, Some(true));
    }

    /// Wet-run failure isolation: a path with no manifest is filtered
    /// out by the JOIN (doesn't burn a `failed` tick); a path with a
    /// manifest row but NULL inline_blob + no manifest_data (invariant
    /// violation, see metadata::get_manifest) fails cleanly and
    /// leaves refs_backfilled=FALSE while the good path proceeds.
    #[tokio::test]
    async fn resign_paths_wet_run_isolates_failures() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);

        // Good path: will process fine.
        let good = seed_backfill_path(
            &db.pool,
            "77777777777777777777777777777777",
            "good",
            10,
            b"T fine T",
        )
        .await;

        // Bad path: narinfo + manifest (status=complete, inline_blob
        // =NULL) but NO manifest_data → get_manifest returns
        // InvariantViolation. Constructed raw because
        // seed_backfill_path always sets inline_blob.
        use sha2::Digest;
        let bad_path = "/nix/store/88888888888888888888888888888888-bad";
        let bad_sph: Vec<u8> = sha2::Sha256::digest(bad_path.as_bytes()).to_vec();
        sqlx::query(
            r#"INSERT INTO narinfo
               (store_path_hash, store_path, nar_hash, nar_size, refs_backfilled)
               VALUES ($1, $2, $3, 0, FALSE)"#,
        )
        .bind(&bad_sph)
        .bind(bad_path)
        .bind(vec![0u8; 32])
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO manifests (store_path_hash, status, inline_blob) \
             VALUES ($1, 'complete', NULL)",
        )
        .bind(&bad_sph)
        .execute(&db.pool)
        .await
        .unwrap();

        let resp = svc
            .resign_paths(Request::new(ResignPathsRequest {
                cursor: String::new(),
                batch_size: 10,
                dry_run: false,
            }))
            .await
            .expect("handler returns Ok even with per-row failures")
            .into_inner();

        assert_eq!(resp.processed, 1, "good path processed");
        assert_eq!(resp.failed, 1, "bad path counted as failed");
        assert_eq!(resp.refs_changed, 0);

        // Good path is marked done; bad path stays FALSE for retry.
        let (done_good,): (Option<bool>,) =
            sqlx::query_as("SELECT refs_backfilled FROM narinfo WHERE store_path = $1")
                .bind(&good)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(done_good, Some(true));

        let (done_bad,): (Option<bool>,) =
            sqlx::query_as("SELECT refs_backfilled FROM narinfo WHERE store_path = $1")
                .bind(bad_path)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(done_bad, Some(false), "failed row stays FALSE for retry");
    }

    /// ResignPaths rejects malformed cursors cleanly (not a 500).
    #[tokio::test]
    async fn resign_paths_rejects_bad_cursor() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);

        let err = svc
            .resign_paths(Request::new(ResignPathsRequest {
                cursor: "not-hex!".to_string(),
                batch_size: 10,
                dry_run: true,
            }))
            .await
            .expect_err("bad cursor");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }
}
