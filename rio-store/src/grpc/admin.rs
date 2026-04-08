//! StoreAdminService: TriggerGC + VerifyChunks + upstream CRUD + GetLoad.
//!
//! The scheduler's `AdminService.TriggerGC` proxies here after
//! populating `extra_roots` from live builds (via GcRoots actor
//! command). Separate service from StoreService so admin ops can
//! have separate RBAC (more privileged than PutPath).

use std::sync::Arc;

use sqlx::PgPool;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::{debug, info, instrument, warn};

use rio_common::grpc::StatusExt;

use rio_proto::types::{
    AddUpstreamRequest, GcProgress, GcRequest, GetLoadRequest, GetLoadResponse,
    ListUpstreamsRequest, ListUpstreamsResponse, RemoveUpstreamRequest, UpstreamInfo,
    VerifyChunksProgress, VerifyChunksRequest,
};

use crate::backend::chunk::ChunkBackend;
use crate::gc;
use crate::metadata;

/// StoreAdminService gRPC server.
pub struct StoreAdminServiceImpl {
    pool: PgPool,
    /// Chunk backend for sweep's key_for (enqueue to pending_s3_
    /// deletes). None = inline-only store, sweep does CASCADE
    /// delete only (no chunk refcounting).
    chunk_backend: Option<Arc<dyn ChunkBackend>>,
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
}

/// Cap on `TriggerGc.extra_roots`. Separate from
/// [`crate::grpc::DEFAULT_MAX_BATCH_PATHS`] (1M, sized for client closure
/// queries) — GC mark runs under an exclusive lock that blocks PutPath, so
/// the unnest() must stay bounded; the GcRoots actor sends ~tens.
const MAX_GC_EXTRA_ROOTS: usize = 100_000;

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
    /// Read-only. No `--repair`: deleting the PG row would be the
    /// wrong move if the object is recoverable (backup, manual
    /// re-upload). The operator decides.
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
                // mid-upload row (grace TTL), and the object SHOULD
                // exist once `uploaded_at` is set.
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
    uuid::Uuid::parse_str(s).status_invalid(&format!("invalid tenant_id {s:?}"))
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
        .status_invalid(&format!("trusted_key {k:?}: bad base64"))?;
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
    use crate::test_helpers::StoreSeed;
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
