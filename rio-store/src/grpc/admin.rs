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
use tracing::{debug, info, instrument, warn};

use rio_common::grpc::StatusExt;
use rio_nix::refscan::{CandidateSet, RefScanSink};
use rio_proto::types::{
    AddUpstreamRequest, GcProgress, GcRequest, GetLoadRequest, GetLoadResponse,
    ListUpstreamsRequest, ListUpstreamsResponse, PinPathRequest, PinPathResponse,
    RemoveUpstreamRequest, ResignPathsRequest, ResignPathsResponse, UpstreamInfo,
    VerifyChunksProgress, VerifyChunksRequest,
};

use crate::backend::chunk::ChunkBackend;
use crate::cas::ChunkCache;
use crate::gc;
use crate::metadata::{self, ManifestKind};
use crate::signing::TenantSigner;

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
    ///
    /// `TenantSigner` wrapper, but ResignPaths only ever uses the
    /// cluster key via `ts.cluster()` — backfilled historical paths
    /// have no per-tenant attribution (they predate whatever
    /// tenant_id→path association the build had). Sharing the same
    /// `Arc<TenantSigner>` as StoreServiceImpl guarantees the cluster
    /// key is the SAME one PutPath falls back to.
    signer: Option<Arc<TenantSigner>>,
    /// Process-wide shutdown token. Threaded into the `run_gc`
    /// spawn so a multi-minute sweep can bail between batches on
    /// SIGTERM instead of surviving past the pod's grace period
    /// and getting SIGKILLed mid-transaction. Defaults to a fresh
    /// (never-cancelled) token — tests that don't `.with_shutdown()`
    /// get the old infinite behavior.
    shutdown: rio_common::signal::Token,
}

impl StoreAdminServiceImpl {
    pub fn new(pool: PgPool, chunk_backend: Option<Arc<dyn ChunkBackend>>) -> Self {
        Self {
            pool,
            chunk_backend,
            chunk_cache: None,
            signer: None,
            shutdown: rio_common::signal::Token::new(),
        }
    }

    /// Wire the process-wide shutdown token. Builder-style; chains
    /// after `new()`. Without this, GC runs to completion even
    /// under SIGTERM (the old behavior — fine for tests, wrong for
    /// production where terminationGracePeriodSeconds is finite).
    pub fn with_shutdown(mut self, shutdown: rio_common::signal::Token) -> Self {
        self.shutdown = shutdown;
        self
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
    pub fn with_signer(mut self, signer: Arc<TenantSigner>) -> Self {
        self.signer = Some(signer);
        self
    }

    /// Compute `(checked_out / max_connections)` for the PG pool.
    ///
    /// `pool.size()` is total connections (idle + in-use); `num_idle()`
    /// is the idle subset. Both are non-atomic separate loads, so
    /// `size - idle` can momentarily go negative under churn —
    /// `saturating_sub` floors at 0. `max_connections` from
    /// `pool.options()` is the configured ceiling (the `pgMax
    /// Connections` chart value), not the high-water mark; that's the
    /// denominator the I-105 cliff is measured against (Aurora's
    /// `max_connections / replicas`, not "however many we happen to
    /// have open").
    ///
    /// Extracted so the gRPC handler and any future periodic gauge
    /// updater share one formula.
    pub(crate) fn pg_pool_utilization(pool: &PgPool) -> f32 {
        let max = pool.options().get_max_connections().max(1);
        let in_use = pool.size().saturating_sub(pool.num_idle() as u32);
        in_use as f32 / max as f32
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
                sink.write_all(&bytes).status_internal("refscan write")?;
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
                    sink.write_all(&chunk).status_internal("refscan write")?;
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
            // Cluster key only — historical paths have no tenant attribution.
            // Even if the original PutPath used a tenant key, we don't know
            // which tenant here (backfill reads narinfo rows, not builds).
            signer.cluster().sign(&fp)
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
                // Invariant: new_sig is Some iff self.signer is Some —
                // the `.map()` that produced new_sig above ties them.
                // This arm only runs when signer exists, so unwrap is
                // structurally safe (not runtime-faith).
                debug!(store_path, refs = new_refs.len(), key = %self.signer.as_ref().unwrap().cluster_key_name(), "backfilled refs + re-signed");
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

/// Default batch size for ResignPaths when the client passes 0.
/// Each path is a NAR-reassembly + refscan pass; 100 keeps per-call
/// latency bounded while amortizing the gRPC roundtrip.
const RESIGN_BATCH_DEFAULT: i64 = 100;

/// Cap on `TriggerGc.extra_roots`. Separate from
/// [`crate::grpc::DEFAULT_MAX_BATCH_PATHS`] (1M, sized for client closure
/// queries) — GC mark runs under an exclusive lock that blocks PutPath, so
/// the unnest() must stay bounded; the GcRoots actor sends ~tens.
const MAX_GC_EXTRA_ROOTS: usize = 100_000;
/// Hard cap on ResignPaths batch size. Larger batches hold the batch
/// SELECT's snapshot longer and bloat the per-call response.
const RESIGN_BATCH_MAX: i64 = 1000;

type TriggerGcStream = ReceiverStream<Result<GcProgress, Status>>;
type VerifyChunksStream = ReceiverStream<Result<VerifyChunksProgress, Status>>;

const VERIFY_BATCH_DEFAULT: u32 = 1000;
const VERIFY_BATCH_MAX: u32 = 5000;

#[tonic::async_trait]
impl rio_proto::StoreAdminService for StoreAdminServiceImpl {
    type TriggerGCStream = TriggerGcStream;
    type VerifyChunksStream = VerifyChunksStream;

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

        // Bound extra_roots BEFORE spawning. A 10M-element array stalls
        // the mark CTE on unnest() — GC no longer blocks PutPath
        // (I-192), but a multi-minute mark still ties up GC_LOCK_ID and
        // skews `paths_resurrected`. Separate from
        // DEFAULT_MAX_BATCH_PATHS (1M, sized for client closures) —
        // GcRoots actor sends ~tens, 100k is already implausible here.
        rio_common::grpc::check_bound("extra_roots", req.extra_roots.len(), MAX_GC_EXTRA_ROOTS)?;
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
            "TriggerGC: starting"
        );

        // Channel for streaming progress. spawn run_gc so this
        // handler returns quickly and the client gets progress
        // updates as they happen. run_gc owns all the advisory-
        // lock choreography; we just forward its terminal Err
        // into the stream if it returns one.
        //
        // Shutdown token threaded through: a multi-minute sweep
        // checks it between batches and bails with Aborted. The
        // advisory lock's scopeguard detach fires on task drop
        // either way (SIGKILL after grace → connection closes →
        // PG releases), but bailing early is cleaner and lets
        // the client see a proper terminal message instead of
        // stream-reset.
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let pool = self.pool.clone();
        let chunk_backend = self.chunk_backend.clone();
        let shutdown = self.shutdown.clone();
        let params = gc::GcParams {
            dry_run: req.dry_run,
            force: req.force,
            grace_hours,
            extra_roots: req.extra_roots,
        };

        tokio::spawn(async move {
            if let Err(e) = gc::run_gc(&pool, chunk_backend, params, tx.clone(), &shutdown).await {
                // run_gc already warn!-logged the detail. Forward
                // the terminal Err into the stream so the client's
                // stream.next() sees it. Progress Ok-messages were
                // sent directly by run_gc; this is the error-only
                // tail.
                let _ = tx.send(Err(e)).await;
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    // TODO(P0431): PinPath/UnpinPath/ResignPaths have zero callers —
    // server-implemented and tested, waiting on rio-cli pin/unpin/resign
    // subcommands. Only TriggerGC is wired today.

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
            hex::decode(&req.cursor).status_invalid("cursor must be hex-encoded store_path_hash")?
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

    /// PG↔backend chunk consistency audit. Streams progress so the
    /// operator sees activity during a multi-minute scan; missing
    /// hashes surface per-batch (no need to wait for the full sweep
    /// to start `aws s3 ls`-ing).
    ///
    /// I-040 diagnostic. The I-007 prefix-normalize fix changed the
    /// S3 key format mid-deployment; chunks written before the fix
    /// landed at `chunks//chunks/...`, chunks after at `chunks/
    /// chunks/...`. PG had refcount=1 deleted=false (correct);
    /// `exists_batch`'s HeadObject 404'd at the new key. This RPC
    /// surfaces exactly that gap — PG-says-yes, backend-says-no —
    /// without guessing key formats (it asks `key_for`, the same
    /// function the read path uses).
    ///
    /// Read-only. No `--repair`: a missing chunk could be at a legacy
    /// key (and `S3ChunkBackend::get`'s fallback already covers that
    /// on the read path), so deleting the PG row would be the wrong
    /// move. The operator decides — sync the legacy keys, restore
    /// from backup, or accept the loss.
    #[instrument(skip(self, request), fields(rpc = "VerifyChunks"))]
    async fn verify_chunks(
        &self,
        request: Request<VerifyChunksRequest>,
    ) -> Result<Response<Self::VerifyChunksStream>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let req = request.into_inner();

        // Inline-only stores have nothing to verify (no chunk
        // backend, no chunks table content). FAILED_PRECONDITION not
        // INTERNAL — config issue, not bug.
        let Some(backend) = self.chunk_backend.clone() else {
            return Err(Status::failed_precondition(
                "VerifyChunks: no chunk backend configured (inline-only store)",
            ));
        };

        let batch_size = match req.batch_size {
            0 => VERIFY_BATCH_DEFAULT,
            n => n.min(VERIFY_BATCH_MAX),
        } as i64;

        info!(batch_size, "VerifyChunks: starting");

        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let pool = self.pool.clone();
        let shutdown = self.shutdown.clone();

        tokio::spawn(async move {
            let mut scanned = 0u64;
            let mut missing = 0u64;
            // Keyset cursor: blake3_hash > $cursor ORDER BY blake3_hash.
            // Starts at the empty bytea (sorts before any 32-byte hash).
            // OFFSET would re-scan rows linearly per page → O(N²) total
            // for a 100k-chunk store; keyset is O(N) overall.
            let mut cursor: Vec<u8> = Vec::new();

            loop {
                if shutdown.is_cancelled() {
                    let _ = tx
                        .send(Err(Status::aborted("VerifyChunks: shutdown")))
                        .await;
                    return;
                }

                // deleted = FALSE only: a deleted=true row is awaiting
                // S3-delete (drain.rs); the object might or might not
                // be there yet. Either way it's not a verification
                // target. refcount=0 IS verified — it could be a
                // PutChunk-then-crash (grace TTL), and the object
                // SHOULD exist.
                let rows: Vec<(Vec<u8>,)> = match sqlx::query_as(
                    "SELECT blake3_hash FROM chunks \
                     WHERE deleted = FALSE AND blake3_hash > $1 \
                     ORDER BY blake3_hash LIMIT $2",
                )
                .bind(&cursor)
                .bind(batch_size)
                .fetch_all(&pool)
                .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = tx
                            .send(Err(Status::internal(format!(
                                "VerifyChunks: PG batch query failed: {e}"
                            ))))
                            .await;
                        return;
                    }
                };
                if rows.is_empty() {
                    break;
                }

                // Advance cursor BEFORE filtering — if every row is
                // shape-invalid (32-byte invariant says: never), the
                // loop still progresses. Last row's hash, not the
                // last accepted.
                cursor.clone_from(&rows.last().unwrap().0);

                // Shape filter: chunks PK is BYTEA but every writer
                // inserts exactly 32 bytes. A wrong-length row is a
                // schema-invariant violation — warn + skip rather
                // than poisoning the whole scan over one corrupt
                // row. Same defensive carve-out enqueue_chunk_deletes
                // uses.
                let hashes: Vec<[u8; 32]> = rows
                    .into_iter()
                    .filter_map(|(h,)| match <[u8; 32]>::try_from(h.as_slice()) {
                        Ok(a) => Some(a),
                        Err(_) => {
                            warn!(
                                len = h.len(),
                                "VerifyChunks: chunk hash wrong length, skipping"
                            );
                            None
                        }
                    })
                    .collect();
                scanned += hashes.len() as u64;

                let exists = match backend.exists_batch(&hashes).await {
                    Ok(e) => e,
                    Err(e) => {
                        // Transient S3 failure surfaces a partial
                        // result. Stream the error so the operator
                        // sees the boundary (last scanned was N) and
                        // can resume. No partial done=true.
                        let _ = tx
                            .send(Err(Status::unavailable(format!(
                                "VerifyChunks: backend exists_batch failed at \
                                 scanned={scanned}: {e}"
                            ))))
                            .await;
                        return;
                    }
                };

                let missing_hashes: Vec<Vec<u8>> = hashes
                    .iter()
                    .zip(&exists)
                    .filter(|&(_, &ok)| !ok)
                    .map(|(h, _)| h.to_vec())
                    .collect();
                missing += missing_hashes.len() as u64;

                if tx
                    .send(Ok(VerifyChunksProgress {
                        scanned,
                        missing,
                        missing_hashes,
                        done: false,
                    }))
                    .await
                    .is_err()
                {
                    // Client hung up — bail; no need to keep
                    // HeadObject-ing.
                    return;
                }
            }

            info!(scanned, missing, "VerifyChunks: complete");
            let _ = tx
                .send(Ok(VerifyChunksProgress {
                    scanned,
                    missing,
                    missing_hashes: vec![],
                    done: true,
                }))
                .await;
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    // ────────────────────────────────────────────────────────────────
    // Upstream CRUD — per-tenant binary-cache substitution config.
    // r[impl store.substitute.upstream]
    // ────────────────────────────────────────────────────────────────

    #[instrument(skip(self, request), fields(rpc = "ListUpstreams"))]
    async fn list_upstreams(
        &self,
        request: Request<ListUpstreamsRequest>,
    ) -> Result<Response<ListUpstreamsResponse>, Status> {
        let req = request.into_inner();
        let tenant_id = parse_tenant_id(&req.tenant_id)?;
        let ups = metadata::upstreams::list_for_tenant(&self.pool, tenant_id)
            .await
            .map_err(|e| super::metadata_status("ListUpstreams", e))?;
        Ok(Response::new(ListUpstreamsResponse {
            upstreams: ups.into_iter().map(upstream_to_proto).collect(),
        }))
    }

    #[instrument(skip(self, request), fields(rpc = "AddUpstream"))]
    async fn add_upstream(
        &self,
        request: Request<AddUpstreamRequest>,
    ) -> Result<Response<UpstreamInfo>, Status> {
        let req = request.into_inner();
        let tenant_id = parse_tenant_id(&req.tenant_id)?;

        // Validate url: must be http(s). Anything else (file://, ssh)
        // is not a binary-cache URL we know how to fetch.
        if !req.url.starts_with("https://") && !req.url.starts_with("http://") {
            return Err(Status::invalid_argument(
                "url must start with https:// or http://",
            ));
        }

        // Validate sig_mode. Empty → keep (the proto default maps to
        // the migration's DEFAULT 'keep').
        let sig_mode = metadata::SigMode::parse(&req.sig_mode).ok_or_else(|| {
            Status::invalid_argument(format!(
                "sig_mode must be keep|add|replace, got {:?}",
                req.sig_mode
            ))
        })?;

        // Validate trusted_keys format: `name:base64(32-byte-pubkey)`.
        // Same format Nix's `trusted-public-keys` uses.
        for k in &req.trusted_keys {
            validate_trusted_key(k)?;
        }

        let priority = if req.priority == 0 { 50 } else { req.priority };

        let up = metadata::upstreams::insert(
            &self.pool,
            tenant_id,
            &req.url,
            priority,
            &req.trusted_keys,
            sig_mode,
        )
        .await
        .map_err(|e| super::metadata_status("AddUpstream: insert", e))?;

        Ok(Response::new(upstream_to_proto(up)))
    }

    #[instrument(skip(self, request), fields(rpc = "RemoveUpstream"))]
    async fn remove_upstream(
        &self,
        request: Request<RemoveUpstreamRequest>,
    ) -> Result<Response<()>, Status> {
        let req = request.into_inner();
        let tenant_id = parse_tenant_id(&req.tenant_id)?;
        let n = metadata::upstreams::delete(&self.pool, tenant_id, &req.url)
            .await
            .map_err(|e| super::metadata_status("RemoveUpstream: delete", e))?;
        if n == 0 {
            return Err(Status::not_found(format!(
                "upstream {} not found for tenant {tenant_id}",
                req.url
            )));
        }
        Ok(Response::new(()))
    }

    /// Per-replica load snapshot for the ComponentScaler reconciler.
    ///
    /// Side-effect: also publishes `rio_store_pg_pool_utilization` so
    /// Prometheus sees the same value the controller acted on. The
    /// controller's 10s tick is the de-facto gauge update cadence —
    /// no separate background updater. If no ComponentScaler is
    /// deployed, the gauge stays at its pre-registered 0.0 (which is
    /// truthful: with `replicas` chart-fixed there's nothing acting
    /// on it anyway).
    // r[impl store.admin.get-load]
    // r[impl obs.metric.store-pg-pool]
    #[instrument(skip(self, _request), fields(rpc = "GetLoad"))]
    async fn get_load(
        &self,
        _request: Request<GetLoadRequest>,
    ) -> Result<Response<GetLoadResponse>, Status> {
        let util = Self::pg_pool_utilization(&self.pool);
        metrics::gauge!("rio_store_pg_pool_utilization").set(util as f64);
        debug!(pg_pool_utilization = util, "GetLoad");
        Ok(Response::new(GetLoadResponse {
            pg_pool_utilization: util,
        }))
    }
}

/// Parse the proto's string tenant_id into a Uuid. Proto uses string
/// (not bytes) for UUIDs; this is the one conversion point.
fn parse_tenant_id(s: &str) -> Result<uuid::Uuid, Status> {
    uuid::Uuid::parse_str(s)
        .map_err(|e| Status::invalid_argument(format!("invalid tenant_id {s:?}: {e}")))
}

/// Validate a `name:base64(pubkey)` trusted-key entry. Rejects
/// unparseable base64 and wrong-length pubkeys (ed25519 is 32 bytes).
fn validate_trusted_key(k: &str) -> Result<(), Status> {
    let (name, b64) = k.split_once(':').ok_or_else(|| {
        Status::invalid_argument(format!("trusted_key {k:?}: missing ':' separator"))
    })?;
    if name.is_empty() {
        return Err(Status::invalid_argument(format!(
            "trusted_key {k:?}: empty key name"
        )));
    }
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| Status::invalid_argument(format!("trusted_key {k:?}: bad base64: {e}")))?;
    if bytes.len() != 32 {
        return Err(Status::invalid_argument(format!(
            "trusted_key {k:?}: pubkey must be 32 bytes (ed25519), got {}",
            bytes.len()
        )));
    }
    Ok(())
}

fn upstream_to_proto(u: metadata::Upstream) -> UpstreamInfo {
    UpstreamInfo {
        id: u.id,
        tenant_id: u.tenant_id.to_string(),
        url: u.url,
        priority: u.priority,
        trusted_keys: u.trusted_keys,
        sig_mode: u.sig_mode.as_str().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::GC_LOCK_ID;
    use crate::test_helpers::{StoreSeed, path_hash};
    use rio_proto::StoreAdminService;
    use rio_test_support::TestDb;

    /// GetLoad reflects checked-out connections as a fraction of
    /// `max_connections`.
    ///
    /// sqlx's `size()`/`num_idle()` are non-atomic separate loads,
    /// TestDb's pool may have background activity (min-connections
    /// maintenance, reaper), and PoolConnection's Drop returns the
    /// connection via a spawned task (NOT synchronously) — so we
    /// don't assert an exact baseline or post-release value. We
    /// assert only: (a) range `[0, 1]`, (b) with two connections
    /// HELD, utilization is at least `2/max`. The "in-use, not size
    /// or num_idle" property is documented at `pg_pool_utilization`
    /// and follows from the formula by inspection.
    // r[verify store.admin.get-load]
    // r[verify obs.metric.store-pg-pool]
    #[tokio::test]
    async fn get_load_tracks_in_use_connections() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);
        let max = db.pool.options().get_max_connections() as f32;

        async fn load(svc: &StoreAdminServiceImpl) -> f32 {
            svc.get_load(Request::new(GetLoadRequest {}))
                .await
                .expect("get_load")
                .into_inner()
                .pg_pool_utilization
        }

        let baseline = load(&svc).await;
        assert!(
            (0.0..=1.0).contains(&baseline),
            "baseline utilization out of range: {baseline}"
        );

        let c1 = db.pool.acquire().await.expect("acquire");
        let c2 = db.pool.acquire().await.expect("acquire");
        let busy = load(&svc).await;
        assert!(
            busy >= 2.0 / max - f32::EPSILON,
            "two held connections → util ≥ 2/max ({}); got {busy}",
            2.0 / max
        );
        assert!(busy <= 1.0, "utilization must not exceed 1.0; got {busy}");
        drop((c1, c2));
    }
    use rio_test_support::fixtures::test_store_path;

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
            store_path: test_store_path("doesnt-exist"),
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
        let path = test_store_path("exists");
        StoreSeed::raw_path(&path)
            .with_nar_size(42)
            .seed(&db.pool)
            .await;

        // Pin.
        let resp = svc
            .pin_path(Request::new(PinPathRequest {
                store_path: path.clone(),
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
                store_path: path.clone(),
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

    /// T2: extra_roots bounded at MAX_GC_EXTRA_ROOTS. cap+1 → reject.
    #[tokio::test]
    async fn trigger_gc_rejects_oversized_extra_roots() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);
        // Syntactically valid paths (validate_store_path passes each).
        let roots: Vec<String> = (0..super::MAX_GC_EXTRA_ROOTS + 1)
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
    /// `created_at` is forced 7 days into the past so the row is
    /// sweep-eligible under any non-zero grace window. `ca` and
    /// `references` are the two axes the gate pivots on.
    async fn seed_narinfo_for_gate(pool: &PgPool, name: &str, refs: &[String], ca: Option<&str>) {
        let mut seed = StoreSeed::path(name)
            .with_refs(refs)
            .with_nar_size(42)
            .created_hours_ago(7 * 24);
        if let Some(c) = ca {
            seed = seed.with_ca(c);
        }
        seed.seed(pool).await;
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
        StoreSeed::path(name)
            .with_nar_size(42)
            .with_refs_backfilled(false)
            .with_inline_blob(b"")
            .seed(pool)
            .await
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
        let sph = path_hash(&path);
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
            let path = test_store_path(name);
            sqlx::query(
                r#"INSERT INTO narinfo
                   (store_path_hash, store_path, nar_hash, nar_size, "references", refs_backfilled)
                   VALUES ($1, $2, $3, 42, '{}', $4)"#,
            )
            .bind(path_hash(&path))
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
        let cluster = crate::signing::Signer::parse(&format!("test-resign-1:{seed_b64}"))
            .expect("valid test signer");
        // ResignPaths only uses the cluster key — the tenant_keys table
        // is empty in this test DB, but that's irrelevant since resign_one
        // calls `signer.cluster().sign()` unconditionally.
        let ts = TenantSigner::new(cluster, db.pool.clone());
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None).with_signer(Arc::new(ts));

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
        let bad_path = test_store_path("bad");
        let bad_sph = path_hash(&bad_path);
        sqlx::query(
            r#"INSERT INTO narinfo
               (store_path_hash, store_path, nar_hash, nar_size, refs_backfilled)
               VALUES ($1, $2, $3, 0, FALSE)"#,
        )
        .bind(&bad_sph)
        .bind(&bad_path)
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
                .bind(&bad_path)
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

    // ────────────────────────────────────────────────────────────────
    // Upstream CRUD
    // ────────────────────────────────────────────────────────────────

    fn valid_trusted_key() -> String {
        use base64::Engine;
        format!(
            "test-key:{}",
            base64::engine::general_purpose::STANDARD.encode([0u8; 32])
        )
    }

    #[tokio::test]
    async fn add_list_remove_upstream_roundtrip() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);
        let tid = rio_test_support::seed_tenant(&db.pool, "ups-rpc").await;

        // Add.
        let added = svc
            .add_upstream(Request::new(AddUpstreamRequest {
                tenant_id: tid.to_string(),
                url: "https://cache.example".into(),
                priority: 30,
                trusted_keys: vec![valid_trusted_key()],
                sig_mode: "add".into(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(added.id > 0);
        assert_eq!(added.sig_mode, "add");
        assert_eq!(added.priority, 30);

        // List.
        let listed = svc
            .list_upstreams(Request::new(ListUpstreamsRequest {
                tenant_id: tid.to_string(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(listed.upstreams.len(), 1);
        assert_eq!(listed.upstreams[0].url, "https://cache.example");

        // Remove.
        svc.remove_upstream(Request::new(RemoveUpstreamRequest {
            tenant_id: tid.to_string(),
            url: "https://cache.example".into(),
        }))
        .await
        .unwrap();

        // Remove again → NotFound.
        let err = svc
            .remove_upstream(Request::new(RemoveUpstreamRequest {
                tenant_id: tid.to_string(),
                url: "https://cache.example".into(),
            }))
            .await
            .expect_err("already deleted");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn add_upstream_validates_inputs() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);
        let tid = rio_test_support::seed_tenant(&db.pool, "ups-val").await;

        // Bad sig_mode.
        let err = svc
            .add_upstream(Request::new(AddUpstreamRequest {
                tenant_id: tid.to_string(),
                url: "https://x.example".into(),
                priority: 50,
                trusted_keys: vec![],
                sig_mode: "junk".into(),
            }))
            .await
            .expect_err("bad sig_mode");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);

        // Bad url scheme.
        let err = svc
            .add_upstream(Request::new(AddUpstreamRequest {
                tenant_id: tid.to_string(),
                url: "ftp://x.example".into(),
                priority: 50,
                trusted_keys: vec![],
                sig_mode: "keep".into(),
            }))
            .await
            .expect_err("bad url");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);

        // Bad trusted_key (wrong pubkey length).
        let short_key = format!(
            "k:{}",
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [0u8; 16])
        );
        let err = svc
            .add_upstream(Request::new(AddUpstreamRequest {
                tenant_id: tid.to_string(),
                url: "https://x.example".into(),
                priority: 50,
                trusted_keys: vec![short_key],
                sig_mode: "keep".into(),
            }))
            .await
            .expect_err("bad key length");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);

        // Bad tenant_id.
        let err = svc
            .add_upstream(Request::new(AddUpstreamRequest {
                tenant_id: "not-a-uuid".into(),
                url: "https://x.example".into(),
                priority: 50,
                trusted_keys: vec![],
                sig_mode: "keep".into(),
            }))
            .await
            .expect_err("bad tenant_id");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn add_upstream_defaults() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);
        let tid = rio_test_support::seed_tenant(&db.pool, "ups-def").await;

        // Empty sig_mode + priority=0 → keep + 50.
        let added = svc
            .add_upstream(Request::new(AddUpstreamRequest {
                tenant_id: tid.to_string(),
                url: "https://x.example".into(),
                priority: 0,
                trusted_keys: vec![],
                sig_mode: String::new(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(added.sig_mode, "keep");
        assert_eq!(added.priority, 50);
    }

    /// VerifyChunks: PG-yes-backend-no surfaces in `missing_hashes`.
    /// One chunk in PG and the MemoryChunkBackend (consistent), one in
    /// PG only (the I-040 case). Batch boundary forced (batch_size=1)
    /// so the keyset cursor advances and the per-batch progress shape
    /// is observable.
    #[tokio::test]
    async fn verify_chunks_reports_missing() {
        use crate::backend::chunk::{ChunkBackend, MemoryChunkBackend};
        use crate::test_helpers::ChunkSeed;

        let db = TestDb::new(&crate::MIGRATOR).await;
        let backend: Arc<dyn ChunkBackend> = Arc::new(MemoryChunkBackend::new());

        // Consistent: refcount=1 in PG, present in backend.
        let h_ok = ChunkSeed::new(0x10).with_refcount(1).seed(&db.pool).await;
        backend
            .put(&h_ok, bytes::Bytes::from_static(b"ok"))
            .await
            .unwrap();

        // The I-040 case: refcount=1 in PG, NOT in backend.
        let h_missing = ChunkSeed::new(0x20).with_refcount(1).seed(&db.pool).await;

        // deleted=true in PG → MUST be skipped (it's awaiting drain;
        // backend state is undefined). Tag 0x05 sorts before 0x10 so
        // a buggy WHERE that ignores `deleted` would scan it first.
        // ChunkSeed synthesizes hash as [tag, 0, 0, ...] — bind the
        // returned hash, not a literal.
        let h_deleted = ChunkSeed::new(0x05).with_refcount(0).seed(&db.pool).await;
        sqlx::query("UPDATE chunks SET deleted = TRUE WHERE blake3_hash = $1")
            .bind(h_deleted.as_slice())
            .execute(&db.pool)
            .await
            .unwrap();

        let svc = StoreAdminServiceImpl::new(db.pool.clone(), Some(backend));
        let resp = svc
            .verify_chunks(Request::new(VerifyChunksRequest { batch_size: 1 }))
            .await
            .unwrap();
        let mut stream = resp.into_inner();

        // Drain the stream. Two PG rows pass the filter (0x10, 0x20)
        // → with batch_size=1, expect two progress frames + one done
        // frame. Collect across all frames so the test doesn't depend
        // on which batch a hash lands in (PG sort is by blake3_hash,
        // so it's deterministic — but pinning that here would make
        // the test brittle for no gain).
        use tokio_stream::StreamExt;
        let mut all_missing: Vec<Vec<u8>> = Vec::new();
        let mut final_progress = None;
        while let Some(p) = stream.next().await {
            let p = p.expect("progress frame, not Err");
            all_missing.extend(p.missing_hashes.clone());
            final_progress = Some(p);
        }
        let p = final_progress.expect("at least one progress frame");

        assert!(p.done, "stream ends with done=true");
        assert_eq!(p.scanned, 2, "deleted=true row excluded");
        assert_eq!(p.missing, 1);
        assert_eq!(all_missing, vec![h_missing.to_vec()]);
        assert!(
            !all_missing.contains(&h_ok.to_vec()),
            "backend-consistent chunk not flagged"
        );
    }

    /// Inline-only store (no chunk_backend) → FAILED_PRECONDITION.
    /// Config issue, not bug — operator gets a clear "wire a backend"
    /// instead of an empty success.
    #[tokio::test]
    async fn verify_chunks_no_backend_precondition() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);

        let err = svc
            .verify_chunks(Request::new(VerifyChunksRequest { batch_size: 0 }))
            .await
            .expect_err("no backend → error");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    }
}
