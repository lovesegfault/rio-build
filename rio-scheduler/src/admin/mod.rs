//! AdminService gRPC implementation.
//!
//! All RPCs are fully implemented as of phase4a: `GetBuildLogs`,
//! `ClusterStatus`, `DrainWorker`, `TriggerGC`, `ListWorkers`,
//! `ListBuilds`, `ClearPoison`, `ListTenants`, `CreateTenant`.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Instant, SystemTime};

use aws_sdk_s3::Client as S3Client;
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::{debug, instrument};

use rio_common::tenant::NormalizedName;
use rio_proto::AdminService;
use rio_proto::types::{
    BuildLogChunk, ClearPoisonRequest, ClearPoisonResponse, ClusterStatusResponse,
    CreateTenantRequest, CreateTenantResponse, DrainWorkerRequest, DrainWorkerResponse, GcProgress,
    GcRequest, GetBuildGraphRequest, GetBuildGraphResponse, GetBuildLogsRequest,
    GetSizeClassStatusRequest, GetSizeClassStatusResponse, ListBuildsRequest, ListBuildsResponse,
    ListTenantsResponse, ListWorkersRequest, ListWorkersResponse, TenantInfo,
};

use crate::actor::{ActorCommand, ActorHandle};
use crate::logs::LogBuffers;

mod builds;
mod graph;
mod logs;
mod sizeclass;
mod workers;

pub struct AdminServiceImpl {
    /// Shared with `SchedulerGrpc` — same Arc, same DashMap.
    log_buffers: Arc<LogBuffers>,
    /// `None` when `RIO_LOG_S3_BUCKET` is unset. In that case, completed-
    /// build logs are unserveable (the flusher never wrote them). Ring
    /// buffer still serves active builds.
    s3: Option<(S3Client, String)>, // (client, bucket)
    pool: PgPool,
    /// For `ClusterStatus` / `DrainWorker` — sends query commands into
    /// the actor event loop. `ClusterSnapshot` bypasses backpressure
    /// (`send_unchecked`): the autoscaler needs a reading especially
    /// when saturated. Dropping the query under load would blind the
    /// controller exactly when it needs to scale up.
    actor: ActorHandle,
    /// Process start time. `ClusterStatusResponse.uptime_since` wants a
    /// wall-clock `Timestamp`, but we don't want to capture `SystemTime`
    /// at startup and risk it being wrong if the system clock jumps
    /// forward during boot. Instead: `Instant` is monotonic; compute
    /// `SystemTime::now() - started_at.elapsed()` at request time.
    /// That's the correct "when did we start, in CURRENT wall-clock
    /// terms" answer even across NTP adjustments.
    started_at: Instant,
    /// Store gRPC address for TriggerGC proxy. The scheduler's
    /// `AdminService.TriggerGC` collects extra_roots via
    /// `ActorCommand::GcRoots`, then proxies to the store's
    /// `StoreAdminService.TriggerGC`. GC runs IN the store (it
    /// owns the chunk backend); scheduler contributes roots.
    store_addr: String,
    /// Cached store size for `ClusterStatus.store_size_bytes`.
    /// Updated by a 60s background task via
    /// `SELECT COALESCE(SUM(nar_size), 0) FROM narinfo`. Default 0
    /// until the first refresh fires. Keeps `ClusterStatus` fast
    /// (it's on the autoscaler's 30s poll path).
    store_size_bytes: Arc<std::sync::atomic::AtomicU64>,
    /// Shared with the lease loop (same Arc as `SchedulerGrpc`).
    /// Admin RPCs mutate state via the actor — standby must refuse.
    is_leader: Arc<AtomicBool>,
    /// For aborting long-running proxy tasks (TriggerGC forward).
    /// Parent token, not serve_shutdown — the forward task should
    /// exit IMMEDIATELY on SIGTERM (store-side GC continues; we
    /// just stop forwarding progress to a client who's about to be
    /// disconnected anyway).
    shutdown: rio_common::signal::Token,
}

/// Spawn a background task that refreshes `store_size_bytes` every 60s
/// via a PG query on the shared store DB. Scheduler already has the pool
/// (same database as the store). Follows the `scheduler_live_pins`
/// cross-layer precedent.
pub fn spawn_store_size_refresh(
    pool: PgPool,
    shutdown: rio_common::signal::Token,
) -> Arc<std::sync::atomic::AtomicU64> {
    let size = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let size_clone = Arc::clone(&size);
    rio_common::task::spawn_monitored("store-size-refresh", async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    tracing::debug!("store-size-refresh shutting down");
                    break;
                }
                _ = interval.tick() => {}
            }
            match sqlx::query_scalar::<_, i64>(
                "SELECT COALESCE(SUM(nar_size), 0)::bigint FROM narinfo",
            )
            .fetch_one(&pool)
            .await
            {
                Ok(bytes) => {
                    size_clone.store(bytes.max(0) as u64, std::sync::atomic::Ordering::Relaxed);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "store_size refresh failed");
                }
            }
        }
    });
    size
}

impl AdminServiceImpl {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        log_buffers: Arc<LogBuffers>,
        s3: Option<(S3Client, String)>,
        pool: PgPool,
        actor: ActorHandle,
        store_addr: String,
        store_size_bytes: Arc<std::sync::atomic::AtomicU64>,
        is_leader: Arc<AtomicBool>,
        shutdown: rio_common::signal::Token,
    ) -> Self {
        Self {
            log_buffers,
            s3,
            pool,
            actor,
            started_at: Instant::now(),
            store_addr,
            store_size_bytes,
            is_leader,
            shutdown,
        }
    }

    /// Actor-dead check. Delegates to the shared
    /// [`actor_guards::check_actor_alive`](crate::grpc::actor_guards)
    /// so the error string stays in lockstep with `SchedulerGrpc`.
    fn check_actor_alive(&self) -> Result<(), Status> {
        crate::grpc::actor_guards::check_actor_alive(&self.actor)
    }

    /// Leader guard. Admin RPCs mutate state (DrainWorker,
    /// ClearPoison, CreateTenant, TriggerGC) or reflect actor state
    /// (ClusterStatus, ListWorkers) — standby has no actor authority
    /// and its view is stale. Delegates to the shared
    /// [`actor_guards::ensure_leader`](crate::grpc::actor_guards).
    fn ensure_leader(&self) -> Result<(), Status> {
        crate::grpc::actor_guards::ensure_leader(&self.is_leader)
    }
}

#[tonic::async_trait]
impl AdminService for AdminServiceImpl {
    type GetBuildLogsStream = ReceiverStream<Result<BuildLogChunk, Status>>;
    type TriggerGCStream = ReceiverStream<Result<GcProgress, Status>>;

    #[instrument(skip(self, request), fields(rpc = "GetBuildLogs"))]
    async fn get_build_logs(
        &self,
        request: Request<GetBuildLogsRequest>,
    ) -> Result<Response<Self::GetBuildLogsStream>, Status> {
        rio_proto::interceptor::link_parent(&request);

        // grpc-web compatibility: ALL error paths return Ok(stream-yielding-
        // Err) instead of Err(Status). Returning Err makes tonic emit a
        // Trailers-Only response (grpc-status in HEADERS, empty body).
        // Envoy's grpc_web filter passes that through with zero body — and
        // browser fetch can't read HTTP trailers, so the dashboard sees a
        // 200 with no error signal. Yielding Err from the stream makes
        // tonic emit HEADERS → TRAILERS, which Envoy encodes as a 0x80
        // body frame the browser CAN read.
        // r[impl dash.journey.build-to-logs]
        if let Err(status) = self.ensure_leader() {
            return Ok(Response::new(logs::err_stream(status)));
        }
        let req = request.into_inner();
        let stream = logs::get_build_logs(&self.log_buffers, &self.s3, &self.pool, req).await;
        Ok(Response::new(stream))
    }

    /// Cluster-wide counts for the controller's autoscaling loop.
    ///
    /// The controller computes `desired_replicas = queued_derivations /
    /// target_value` (clamped to `[min, max]`) and patches the worker
    /// StatefulSet. `queued_derivations` is the primary signal — that's
    /// how many ready-to-build derivations are waiting for worker slots.
    /// `running_derivations` is secondary (for "scale-down is safe when
    /// queue=0 AND running is below capacity").
    ///
    /// `store_size_bytes` is a cached value from the 60s background
    /// refresh task — NOT a live PG query (this endpoint is on the
    /// autoscaler's 30s hot path). See `spawn_store_size_refresh`.
    #[instrument(skip(self, request), fields(rpc = "ClusterStatus"))]
    async fn cluster_status(
        &self,
        request: Request<()>,
    ) -> Result<Response<ClusterStatusResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        self.check_actor_alive()?;

        // send_unchecked: autoscaler MUST get a reading under saturation.
        // See ActorCommand::ClusterSnapshot doc for rationale.
        let snap = self
            .actor
            .query_unchecked(|reply| ActorCommand::ClusterSnapshot { reply })
            .await
            .map_err(crate::grpc::SchedulerGrpc::actor_error_to_status)?;

        // SystemTime::now() - elapsed → "start time in CURRENT wall-clock
        // terms." checked_sub: if elapsed > now (clock jumped way back),
        // UNIX_EPOCH is a less-wrong answer than panicking.
        let uptime_since = SystemTime::now()
            .checked_sub(self.started_at.elapsed())
            .unwrap_or(SystemTime::UNIX_EPOCH);

        Ok(Response::new(ClusterStatusResponse {
            total_workers: snap.total_workers,
            active_workers: snap.active_workers,
            draining_workers: snap.draining_workers,
            pending_builds: snap.pending_builds,
            active_builds: snap.active_builds,
            queued_derivations: snap.queued_derivations,
            running_derivations: snap.running_derivations,
            store_size_bytes: self
                .store_size_bytes
                .load(std::sync::atomic::Ordering::Relaxed),
            uptime_since: Some(prost_types::Timestamp::from(uptime_since)),
        }))
    }

    // r[impl sched.admin.list-workers]
    #[instrument(skip(self, request), fields(rpc = "ListWorkers"))]
    async fn list_workers(
        &self,
        request: Request<ListWorkersRequest>,
    ) -> Result<Response<ListWorkersResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let req = request.into_inner();
        let resp = workers::list_workers(&self.actor, &req.status_filter).await?;
        Ok(Response::new(resp))
    }

    #[instrument(skip(self, request), fields(rpc = "ListBuilds"))]
    async fn list_builds(
        &self,
        request: Request<ListBuildsRequest>,
    ) -> Result<Response<ListBuildsResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        let req = request.into_inner();
        // Empty filter → no tenant filter (list all). Non-empty →
        // resolve to UUID. Unknown name → InvalidArgument (the CLI
        // tool surfaces it verbatim).
        let tenant_filter = match NormalizedName::from_maybe_empty(&req.tenant_filter) {
            None => None,
            Some(name) => Some(crate::grpc::resolve_tenant_name(&self.pool, &name).await?),
        };
        let db = crate::db::SchedulerDb::new(self.pool.clone());
        let resp = builds::list_builds(
            &db,
            &req.status_filter,
            tenant_filter,
            if req.limit == 0 { 100 } else { req.limit },
            req.offset,
        )
        .await?;
        Ok(Response::new(resp))
    }

    /// Proxy to store's `StoreAdminService.TriggerGC` after
    /// populating `extra_roots` from the scheduler's live builds.
    ///
    /// Flow:
    /// 1. `ActorCommand::GcRoots` → collect expected_output_paths
    ///    from all non-terminal derivations. These may not be in
    ///    narinfo yet (worker hasn't uploaded); the store's mark
    ///    phase includes them as root seeds so in-flight outputs
    ///    aren't collected.
    /// 2. Connect to store, call TriggerGC with the populated
    ///    extra_roots + client's dry_run/grace_period.
    /// 3. Proxy the store's GCProgress stream back to the client.
    ///
    /// If store is unreachable: UNAVAILABLE (not UNIMPLEMENTED —
    /// the RPC IS implemented, store is just down). Client retries.
    #[instrument(skip(self, request), fields(rpc = "TriggerGC"))]
    async fn trigger_gc(
        &self,
        request: Request<GcRequest>,
    ) -> Result<Response<Self::TriggerGCStream>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let mut req = request.into_inner();

        // Step 1: collect extra_roots from the actor. send_unchecked
        // bypasses backpressure — GC is operator-initiated, rare,
        // and should work even when the scheduler is saturated.
        let mut extra_roots = self
            .actor
            .query_unchecked(|reply| ActorCommand::GcRoots { reply })
            .await
            .map_err(crate::grpc::SchedulerGrpc::actor_error_to_status)?;

        // Merge with any client-provided extra_roots (unusual but
        // allowed — maybe the client has additional pins).
        req.extra_roots.append(&mut extra_roots);
        let extra_count = req.extra_roots.len();

        debug!(
            dry_run = req.dry_run,
            grace_hours = ?req.grace_period_hours,
            extra_roots = extra_count,
            "proxying TriggerGC to store with live-build roots"
        );

        // Step 2: connect to store admin service. Same TLS config
        // as connect_store (OnceLock CLIENT_TLS).
        let mut store_admin = rio_proto::client::connect_store_admin(&self.store_addr)
            .await
            .map_err(|e| Status::unavailable(format!("store admin connect failed: {e}")))?;

        // Step 3: proxy the call. The store's stream becomes OUR
        // stream — we wrap it in a forwarding task. inject_current
        // so the store's link_parent can stitch the trace chain.
        let mut tonic_req = tonic::Request::new(req);
        rio_proto::interceptor::inject_current(tonic_req.metadata_mut());
        let store_stream = store_admin
            .trigger_gc(tonic_req)
            .await
            .map_err(|e| Status::internal(format!("store TriggerGC failed: {e}")))?
            .into_inner();

        // Forward store's progress stream to the client. A small
        // channel + forwarding task: the store stream isn't
        // directly compatible with our TriggerGCStream type (we
        // declare it as ReceiverStream).
        let (tx, rx) = mpsc::channel::<Result<GcProgress, Status>>(8);
        let shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            let mut store_stream = store_stream;
            loop {
                // biased: check shutdown first. A store-side sweep
                // can go minutes between progress messages; without
                // this arm the task holds the store channel alive
                // past main()'s lease_loop.await.
                let msg = tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => {
                        tracing::debug!("TriggerGC forward: shutdown, dropping store stream");
                        break;
                    }
                    m = store_stream.message() => m,
                };
                match msg {
                    Ok(Some(progress)) => {
                        if tx.send(Ok(progress)).await.is_err() {
                            // Client disconnected. Let the store-
                            // side GC finish (it's already running);
                            // just stop forwarding.
                            break;
                        }
                    }
                    Ok(None) => {
                        // Store stream EOF (GC complete). Drop tx
                        // → client sees stream end.
                        break;
                    }
                    Err(e) => {
                        // Store error mid-stream. Forward the error;
                        // client decides whether to retry.
                        let _ = tx.send(Err(e)).await;
                        break;
                    }
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    /// Mark a worker draining: `has_capacity()` returns false, dispatch
    /// skips it. In-flight builds continue. Called by:
    ///   - Worker's SIGTERM handler (step 1 of drain)
    ///   - Controller's WorkerPool finalizer cleanup
    ///
    /// `force=true` reassigns in-flight builds — the worker's nix-daemon
    /// keeps running them (we can't reach into its process tree) but the
    /// scheduler redispatches to fresh workers. Wasteful but unblocks.
    ///
    /// Unknown worker_id → `accepted=false, running=0`. NOT an error:
    /// SIGTERM may race with stream close (WorkerDisconnected removes
    /// the entry). The caller proceeds as if drain succeeded.
    ///
    /// Empty worker_id → InvalidArgument. Catches the proto-default
    /// (empty string) before it gets interpreted as "worker named ''
    /// not found."
    #[instrument(skip(self, request), fields(rpc = "DrainWorker"))]
    async fn drain_worker(
        &self,
        request: Request<DrainWorkerRequest>,
    ) -> Result<Response<DrainWorkerResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let req = request.into_inner();

        if req.worker_id.is_empty() {
            return Err(Status::invalid_argument("worker_id is required"));
        }

        // send_unchecked: drain MUST land even under backpressure. A
        // shutting-down worker accepting new assignments is a feedback
        // loop into MORE load — exactly what we don't want.
        let worker_id = req.worker_id.into();
        let force = req.force;
        let result = self
            .actor
            .query_unchecked(|reply| ActorCommand::DrainWorker {
                worker_id,
                force,
                reply,
            })
            .await
            .map_err(crate::grpc::SchedulerGrpc::actor_error_to_status)?;

        Ok(Response::new(DrainWorkerResponse {
            accepted: result.accepted,
            running_builds: result.running_builds,
        }))
    }

    #[instrument(skip(self, request), fields(rpc = "ClearPoison"))]
    async fn clear_poison(
        &self,
        request: Request<ClearPoisonRequest>,
    ) -> Result<Response<ClearPoisonResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let req = request.into_inner();
        if req.derivation_hash.is_empty() {
            return Err(Status::invalid_argument("derivation_hash is required"));
        }
        let drv_hash: crate::state::DrvHash = req.derivation_hash.into();
        let cleared = self
            .actor
            .query_unchecked(|reply| ActorCommand::ClearPoison { drv_hash, reply })
            .await
            .map_err(crate::grpc::SchedulerGrpc::actor_error_to_status)?;
        Ok(Response::new(ClearPoisonResponse { cleared }))
    }

    // r[impl sched.admin.list-tenants]
    #[instrument(skip(self, request), fields(rpc = "ListTenants"))]
    async fn list_tenants(
        &self,
        request: Request<()>,
    ) -> Result<Response<ListTenantsResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        let db = crate::db::SchedulerDb::new(self.pool.clone());
        let rows = db
            .list_tenants()
            .await
            .map_err(|e| Status::internal(format!("db: {e}")))?;
        Ok(Response::new(ListTenantsResponse {
            tenants: rows.into_iter().map(tenant_row_to_proto).collect(),
        }))
    }

    // r[impl sched.admin.create-tenant]
    #[instrument(skip(self, request), fields(rpc = "CreateTenant"))]
    async fn create_tenant(
        &self,
        request: Request<CreateTenantRequest>,
    ) -> Result<Response<CreateTenantResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        let req = request.into_inner();
        // NormalizedName trims + rejects empty AND interior whitespace.
        // The same normalization runs at every read path (gateway
        // server.rs, cache auth.rs, SubmitBuild) — storing the
        // normalized form here means every `WHERE tenant_name = $1`
        // lookup matches. Without it, " team-a " never matches
        // 'team-a' and the operator spends an afternoon staring at
        // invisible whitespace. Interior-whitespace rejection
        // (`"team a"` → `InteriorWhitespace`) catches the typo class
        // where an authorized_keys comment has a space instead of a
        // dash — the tenant would be unreachable otherwise.
        let tenant_name = NormalizedName::new(&req.tenant_name)
            .map_err(|e| Status::invalid_argument(format!("invalid tenant_name: {e}")))?;
        let cache_token = req.cache_token.as_deref().map(str::trim);
        if cache_token.is_some_and(str::is_empty) {
            return Err(Status::invalid_argument(
                "cache_token must not be empty string (omit field for no-cache-auth)",
            ));
        }
        // u32→i32 / u64→i64 would wrap to negative for out-of-range
        // values (PG stores INTEGER/BIGINT signed). GC with negative
        // retention is undefined downstream.
        let gc_retention_hours = req
            .gc_retention_hours
            .map(|h| {
                i32::try_from(h).map_err(|_| {
                    Status::invalid_argument("gc_retention_hours out of range (max 2^31-1)")
                })
            })
            .transpose()?;
        let gc_max_store_bytes = req
            .gc_max_store_bytes
            .map(|b| {
                i64::try_from(b).map_err(|_| {
                    Status::invalid_argument("gc_max_store_bytes out of range (max 2^63-1)")
                })
            })
            .transpose()?;
        let db = crate::db::SchedulerDb::new(self.pool.clone());
        let row = db
            .create_tenant(
                &tenant_name,
                gc_retention_hours,
                gc_max_store_bytes,
                cache_token,
            )
            .await
            .map_err(|e| Status::internal(format!("db: {e}")))?
            .ok_or_else(|| {
                Status::already_exists(format!(
                    "tenant '{tenant_name}' already exists (or cache_token collision)"
                ))
            })?;
        Ok(Response::new(CreateTenantResponse {
            tenant: Some(tenant_row_to_proto(row)),
        }))
    }

    /// PG-backed DAG snapshot for dashboard viz. No actor round-trip —
    /// this reads PG directly (same pattern as ListBuilds/ListTenants).
    /// Works for completed builds too (actor state is gone, PG persists).
    ///
    /// Leader-guarded: standby's PG view is correct (replicas see the
    /// same DB) but guarding keeps all admin RPCs uniform — operator
    /// tooling points at the leader VIP, period.
    #[instrument(skip(self, request), fields(rpc = "GetBuildGraph"))]
    async fn get_build_graph(
        &self,
        request: Request<GetBuildGraphRequest>,
    ) -> Result<Response<GetBuildGraphResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        let req = request.into_inner();
        let db = crate::db::SchedulerDb::new(self.pool.clone());
        let resp = graph::get_build_graph(&db, &req.build_id, None).await?;
        Ok(Response::new(resp))
    }

    /// SITA-E size-class status snapshot. Actor supplies effective vs
    /// configured cutoffs + queued/running counts; DB supplies sample
    /// counts (partitioned by effective cutoff band).
    ///
    /// Returns empty `classes` if size-class routing is disabled (the
    /// actor's `size_classes` is empty). CLI/dashboard can render that
    /// as "feature off" without a separate error path.
    #[instrument(skip(self, request), fields(rpc = "GetSizeClassStatus"))]
    async fn get_size_class_status(
        &self,
        request: Request<GetSizeClassStatusRequest>,
    ) -> Result<Response<GetSizeClassStatusResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let db = crate::db::SchedulerDb::new(self.pool.clone());
        let resp = sizeclass::get_size_class_status(&self.actor, &db).await?;
        Ok(Response::new(resp))
    }
}

fn tenant_row_to_proto(row: crate::db::TenantRow) -> TenantInfo {
    TenantInfo {
        tenant_id: row.tenant_id.to_string(),
        tenant_name: row.tenant_name,
        gc_retention_hours: row.gc_retention_hours as u32,
        gc_max_store_bytes: row.gc_max_store_bytes.map(|b| b as u64),
        created_at: Some(prost_types::Timestamp {
            seconds: row.created_at,
            nanos: 0,
        }),
        has_cache_token: row.has_cache_token,
    }
}

#[cfg(test)]
mod tests;
