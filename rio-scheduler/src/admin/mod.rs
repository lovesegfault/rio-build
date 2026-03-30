//! AdminService gRPC implementation.
//!
//! All RPCs are fully implemented as of phase4a: `GetBuildLogs`,
//! `ClusterStatus`, `DrainExecutor`, `TriggerGC`, `ListExecutors`,
//! `ListBuilds`, `ClearPoison`, `ListTenants`, `CreateTenant`,
//! `GetBuildGraph`, `GetSizeClassStatus`.
//!
//! Per-RPC bodies live in submodules (`logs`, `gc`, `tenants`,
//! `builds`, `workers`, `graph`, `sizeclass`). This file holds only the
//! [`AdminServiceImpl`] state struct + thin wrapper methods that
//! delegate into the submodules. Split from a single 861L file (P0383)
//! after collision count hit 20.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Instant, SystemTime};

use aws_sdk_s3::Client as S3Client;
use sqlx::PgPool;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::instrument;

use rio_common::tenant::NormalizedName;
use rio_proto::AdminService;
use rio_proto::types::{
    BuildLogChunk, ClearPoisonRequest, ClearPoisonResponse, ClusterStatusResponse,
    CreateTenantRequest, CreateTenantResponse, DrainExecutorRequest, DrainExecutorResponse,
    GcProgress, GcRequest, GetBuildGraphRequest, GetBuildGraphResponse, GetBuildLogsRequest,
    GetCapacityManifestRequest, GetCapacityManifestResponse, GetSizeClassStatusRequest,
    GetSizeClassStatusResponse, ListBuildsRequest, ListBuildsResponse, ListExecutorsRequest,
    ListExecutorsResponse, ListPoisonedResponse, ListTenantsResponse, PoisonedDerivation,
};

use crate::actor::{ActorCommand, ActorHandle};
use crate::logs::LogBuffers;

mod builds;
mod executors;
mod gc;
mod graph;
mod logs;
mod sizeclass;
mod tenants;

pub use gc::spawn_store_size_refresh;

pub struct AdminServiceImpl {
    /// Shared with `SchedulerGrpc` — same Arc, same DashMap.
    log_buffers: Arc<LogBuffers>,
    /// `None` when `RIO_LOG_S3_BUCKET` is unset. In that case, completed-
    /// build logs are unserveable (the flusher never wrote them). Ring
    /// buffer still serves active builds.
    s3: Option<(S3Client, String)>, // (client, bucket)
    pool: PgPool,
    /// For `ClusterStatus` / `DrainExecutor` — sends query commands into
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

    /// Leader guard. Admin RPCs mutate state (DrainExecutor,
    /// ClearPoison, CreateTenant, TriggerGC) or reflect actor state
    /// (ClusterStatus, ListExecutors) — standby has no actor authority
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
            total_executors: snap.total_executors,
            active_executors: snap.active_executors,
            draining_executors: snap.draining_executors,
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
    #[instrument(skip(self, request), fields(rpc = "ListExecutors"))]
    async fn list_executors(
        &self,
        request: Request<ListExecutorsRequest>,
    ) -> Result<Response<ListExecutorsResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let req = request.into_inner();
        let resp = executors::list_executors(&self.actor, &req.status_filter).await?;
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
            req.cursor.as_deref(),
        )
        .await?;
        Ok(Response::new(resp))
    }

    #[instrument(skip(self, request), fields(rpc = "TriggerGC"))]
    async fn trigger_gc(
        &self,
        request: Request<GcRequest>,
    ) -> Result<Response<Self::TriggerGCStream>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let req = request.into_inner();
        let stream =
            gc::trigger_gc(&self.actor, &self.store_addr, self.shutdown.clone(), req).await?;
        Ok(Response::new(stream))
    }

    /// Mark a worker draining: `has_capacity()` returns false, dispatch
    /// skips it. In-flight builds continue. Called by:
    ///   - Worker's SIGTERM handler (step 1 of drain)
    ///   - Controller's BuilderPool finalizer cleanup
    ///
    /// `force=true` reassigns in-flight builds — the worker's nix-daemon
    /// keeps running them (we can't reach into its process tree) but the
    /// scheduler redispatches to fresh workers. Wasteful but unblocks.
    ///
    /// Unknown executor_id → `accepted=false, running=0`. NOT an error:
    /// SIGTERM may race with stream close (ExecutorDisconnected removes
    /// the entry). The caller proceeds as if drain succeeded.
    ///
    /// Empty executor_id → InvalidArgument. Catches the proto-default
    /// (empty string) before it gets interpreted as "worker named ''
    /// not found."
    #[instrument(skip(self, request), fields(rpc = "DrainExecutor"))]
    async fn drain_executor(
        &self,
        request: Request<DrainExecutorRequest>,
    ) -> Result<Response<DrainExecutorResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let req = request.into_inner();

        if req.executor_id.is_empty() {
            return Err(Status::invalid_argument("executor_id is required"));
        }

        // send_unchecked: drain MUST land even under backpressure. A
        // shutting-down worker accepting new assignments is a feedback
        // loop into MORE load — exactly what we don't want.
        let executor_id = req.executor_id.into();
        let force = req.force;
        let result = self
            .actor
            .query_unchecked(|reply| ActorCommand::DrainExecutor {
                executor_id,
                force,
                reply,
            })
            .await
            .map_err(crate::grpc::SchedulerGrpc::actor_error_to_status)?;

        Ok(Response::new(DrainExecutorResponse {
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

    // r[impl sched.admin.list-poisoned]
    #[instrument(skip(self, request), fields(rpc = "ListPoisoned"))]
    async fn list_poisoned(
        &self,
        request: Request<()>,
    ) -> Result<Response<ListPoisonedResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        // DB is the source of truth for poisoned_at (the in-memory DAG
        // reconstructs Instant from elapsed_secs at startup but doesn't
        // store the original timestamp for display).
        let db = crate::db::SchedulerDb::new(self.pool.clone());
        let rows = db
            .load_poisoned_derivations()
            .await
            .map_err(|e| Status::internal(format!("load_poisoned_derivations: {e}")))?;
        let derivations = rows
            .into_iter()
            .map(|r| PoisonedDerivation {
                drv_path: r.drv_path,
                failed_executors: r.failed_builders,
                poisoned_secs_ago: r.elapsed_secs.max(0.0) as u64,
            })
            .collect();
        Ok(Response::new(ListPoisonedResponse { derivations }))
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
        let resp = tenants::list_tenants(&db).await?;
        Ok(Response::new(resp))
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
        let db = crate::db::SchedulerDb::new(self.pool.clone());
        let resp = tenants::create_tenant(&db, req).await?;
        Ok(Response::new(resp))
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

    /// Per-derivation resource estimates for the controller's manifest
    /// reconciler (ADR-020 sizing=Manifest mode). Sibling to
    /// `cluster_status` — that RPC returns the scalar count, this
    /// returns the detailed shape.
    ///
    /// TODO(P0501): T2+T3 land the real implementation — walk the
    /// ready queue, look up Estimator EMA per (pname, system), apply
    /// headroom + bucketing via bucketed_estimate(). Until then,
    /// empty response (correct for "no queue" case; controller uses
    /// the floor when manifest is empty).
    // r[impl sched.admin.capacity-manifest]
    #[instrument(skip(self, request), fields(rpc = "GetCapacityManifest"))]
    async fn get_capacity_manifest(
        &self,
        request: Request<GetCapacityManifestRequest>,
    ) -> Result<Response<GetCapacityManifestResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        self.check_actor_alive()?;
        Ok(Response::new(GetCapacityManifestResponse {
            estimates: vec![],
        }))
    }
}

#[cfg(test)]
mod tests;
