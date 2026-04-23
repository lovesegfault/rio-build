//! AdminService gRPC implementation.
//!
//! All RPCs are fully implemented as of phase4a: `GetBuildLogs`,
//! `ClusterStatus`, `DrainExecutor`, `TriggerGC`, `ListExecutors`,
//! `ListBuilds`, `ClearPoison`, `ListTenants`, `CreateTenant`,
//! `GetBuildGraph`, `GetSpawnIntents`.
//!
//! Per-RPC bodies live in submodules (`logs`, `gc`, `tenants`,
//! `builds`, `workers`, `graph`, `spawn_intents`). This file holds only the
//! [`AdminServiceImpl`] state struct + thin wrapper methods that
//! delegate into the submodules. Split from a single 861L file (P0383)
//! after collision count hit 20.

use std::sync::Arc;
use std::time::{Instant, SystemTime};

use aws_sdk_s3::Client as S3Client;
use sqlx::PgPool;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use rio_common::grpc::StatusExt;
use tracing::instrument;

use rio_common::tenant::NormalizedName;
use rio_proto::AdminService;
use rio_proto::types::ClearSlaOverrideRequest;
use rio_proto::types::{
    AckSpawnedIntentsRequest, AppendInterruptSampleRequest, BuildLogChunk, CancelBuildRequest,
    CancelBuildResponse, ClearPoisonRequest, ClearPoisonResponse, ClusterStatusResponse,
    CreateTenantRequest, CreateTenantResponse, DebugExecutorState, DebugListExecutorsResponse,
    DeleteTenantRequest, DeleteTenantResponse, DrainExecutorRequest, DrainExecutorResponse,
    ExportSlaCorpusRequest, ExportSlaCorpusResponse, GcProgress, GcRequest, GetBuildGraphRequest,
    GetBuildGraphResponse, GetBuildLogsRequest, GetHwClassConfigResponse, GetSpawnIntentsRequest,
    GetSpawnIntentsResponse, HwClassSampledRequest, HwClassSampledResponse, ImportSlaCorpusRequest,
    ImportSlaCorpusResponse, InjectBuildSampleRequest, InspectBuildDagRequest,
    InspectBuildDagResponse, ListBuildsRequest, ListBuildsResponse, ListExecutorsRequest,
    ListExecutorsResponse, ListPoisonedResponse, ListSlaOverridesRequest, ListSlaOverridesResponse,
    ListTenantsResponse, PoisonedDerivation, ReportExecutorTerminationRequest,
    ReportExecutorTerminationResponse, ResetSlaModelRequest, SetSlaOverrideRequest,
    SlaExplainRequest, SlaExplainResponse, SlaOverride, SlaStatusRequest, SlaStatusResponse,
    TerminationReason,
};
use uuid::Uuid;

use crate::actor::{ActorCommand, ActorHandle, AdminQuery};
use crate::logs::LogBuffers;
use crate::sla::types::ModelKey;

/// `actor.query_unchecked` + `actor_error_to_status`. Every admin
/// handler that round-trips a oneshot through the actor uses this —
/// the call bypasses backpressure (diagnostic/operator queries must
/// land especially under saturation; see I-056) and maps the
/// `ActorError` into the canonical gRPC `Status`.
pub(super) async fn query_actor<R>(
    actor: &ActorHandle,
    mk: impl FnOnce(tokio::sync::oneshot::Sender<R>) -> ActorCommand,
) -> Result<R, Status> {
    actor
        .query_unchecked(mk)
        .await
        .map_err(crate::grpc::SchedulerGrpc::actor_error_to_status)
}

mod builds;
mod executors;
mod gc;
mod graph;
mod logs;
mod sla;
mod spawn_intents;
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
    /// `ActorCommand::Admin(AdminQuery::GcRoots`, then proxies to the store's
    /// `StoreAdminService.TriggerGC`. GC runs IN the store (it
    /// owns the chunk backend); scheduler contributes roots.
    store_addr: String,
    /// Cached store size for `ClusterStatus.store_size_bytes`.
    /// Updated by a 60s background task via
    /// `SELECT COALESCE(SUM(nar_size), 0) FROM narinfo`. Default 0
    /// until the first refresh fires. Keeps `ClusterStatus` fast
    /// (it's on the autoscaler's 30s poll path).
    store_size_bytes: Arc<std::sync::atomic::AtomicU64>,
    /// Shared with the lease loop (same Arcs as `SchedulerGrpc`).
    /// Admin RPCs mutate state via the actor — standby must refuse.
    /// Carries `leader_for()` for `ListExecutorsResponse.
    /// leader_for_secs` (controller orphan-reap fail-closed gate).
    leader: crate::lease::LeaderState,
    /// For aborting long-running proxy tasks (TriggerGC forward).
    /// Parent token, not serve_shutdown — the forward task should
    /// exit IMMEDIATELY on SIGTERM (store-side GC continues; we
    /// just stop forwarding progress to a client who's about to be
    /// disconnected anyway).
    shutdown: rio_common::signal::Token,
    /// `[sla].cluster` — written into `interrupt_samples.cluster` so
    /// `CostTable::refresh_lambda`'s `WHERE cluster = $1` matches.
    /// Empty for the single-cluster default (matches the `DEFAULT ''`
    /// migration-043 column).
    cluster: String,
    /// Full `[sla]` block — `import_sla_corpus` constructs a
    /// `prior::ValidatedSeedCorpus` against this BEFORE the corpus
    /// reaches the actor (`r[sched.sla.threat.corpus-clamp]`). `Arc`
    /// because `DagActor` holds the same.
    sla_config: Arc<crate::sla::config::SlaConfig>,
    /// Verifies `x-rio-service-token` for controller-only mutating RPCs
    /// (`AppendInterruptSample`, `DrainExecutor`). `None` = dev mode
    /// (accept all) — same pass-through pattern as the store's
    /// assignment-token verifier. The threat: builders share port 9001
    /// with this service (CCNP allows scheduler:9001 at L4 only), and a
    /// compromised builder could forge `interrupt_samples` to bias
    /// λ\[h\] or drain arbitrary executors. See `r[sec.authz.service-token]`.
    service_verifier: Option<Arc<rio_auth::hmac::HmacVerifier>>,
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
        leader: crate::lease::LeaderState,
        shutdown: rio_common::signal::Token,
        cluster: String,
        sla_config: Arc<crate::sla::config::SlaConfig>,
        service_verifier: Option<Arc<rio_auth::hmac::HmacVerifier>>,
    ) -> Self {
        Self {
            log_buffers,
            s3,
            pool,
            actor,
            started_at: Instant::now(),
            store_addr,
            store_size_bytes,
            leader,
            shutdown,
            cluster,
            sla_config,
            service_verifier,
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
        crate::grpc::actor_guards::ensure_leader(&self.leader.is_leader_arc())
    }

    /// Flip to standby. For tests exercising the in-stream-err leader
    /// guard on `TriggerGC` (the field's name collides with a private
    /// `lease::*::is_leader()` method during resolution from grandchild
    /// modules, so a method is the cleanest accessor).
    #[cfg(test)]
    pub(super) fn force_standby(&self) {
        self.leader.on_lose();
    }

    /// Gate for mutating RPCs. Verifies `x-rio-service-token`
    /// (HMAC-signed [`ServiceClaims`]) and checks
    /// `claims.caller ∈ allowed`. `service_verifier == None` → dev-mode
    /// pass-through (parity with the store's assignment-token verifier).
    ///
    /// Builders share port 9001 with this service; without this gate a
    /// compromised builder could write straight into `interrupt_samples`
    /// (poisoning λ\[h\]), drain arbitrary executors, or set SLA
    /// overrides to bias the solver fleet-wide.
    ///
    /// MUST gate every mutating RPC. The canonical list lives in
    /// `tests::mutating_rpcs_require_service_token` — adding a new
    /// mutating RPC means adding it there too (so the test fails if
    /// the gate is forgotten); read-only RPCs go in that test's
    /// header comment instead.
    ///
    /// Thin wrapper over the shared
    /// [`rio_auth::hmac::ensure_service_caller`] (also used by
    /// rio-store's `StoreAdminServiceImpl`).
    ///
    /// [`ServiceClaims`]: rio_auth::hmac::ServiceClaims
    // r[impl sec.authz.service-token]
    fn ensure_service_caller(
        &self,
        md: &tonic::metadata::MetadataMap,
        allowed: &[&str],
    ) -> Result<(), Status> {
        rio_auth::hmac::ensure_service_caller(md, self.service_verifier.as_deref(), allowed)?;
        Ok(())
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
    /// The controller spawns one-shot Jobs up to `max_concurrent` from
    /// `queued_derivations`. `queued_derivations` is the primary signal —
    /// that's how many ready-to-build derivations are waiting for a
    /// worker. `running_derivations` is secondary (for "scale-down is
    /// safe when queue=0 AND running is below capacity").
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

        // I-163: read the watch-cached snapshot (no mailbox round-trip).
        // The autoscaler MUST get a reading under saturation —
        // `send_unchecked` bypassed backpressure but still queued
        // behind 9.5k commands (~47s wait for a 37µs handler). The
        // cached value is ≤1 Tick stale; for a 30s autoscaler poll
        // that's noise.
        // r[impl sched.admin.snapshot-cached]
        let snap = self.actor.cluster_snapshot_cached();

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
            substituting_derivations: snap.substituting_derivations,
            queued_by_system: snap.queued_by_system.clone(),
            store_size_bytes: self
                .store_size_bytes
                .load(std::sync::atomic::Ordering::Relaxed),
            uptime_since: Some(prost_types::Timestamp::from(uptime_since)),
        }))
    }

    // r[impl sched.admin.list-executors]
    #[instrument(skip(self, request), fields(rpc = "ListExecutors"))]
    async fn list_executors(
        &self,
        request: Request<ListExecutorsRequest>,
    ) -> Result<Response<ListExecutorsResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let req = request.into_inner();
        let resp =
            executors::list_executors(&self.actor, &req.status_filter, self.leader.leader_for())
                .await?;
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
        let db = crate::db::SchedulerDb::new(self.pool.clone());
        let tenant_filter = match NormalizedName::from_maybe_empty(&req.tenant_filter) {
            None => None,
            Some(name) => Some(crate::grpc::resolve_tenant_name(&db, &name).await?),
        };
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
        // grpc-web compatibility: same Trailers-Only constraint as
        // get_build_logs above. ALL error paths return Ok(stream-
        // yielding-Err); the handler never returns Err.
        if let Err(status) = self
            .ensure_service_caller(request.metadata(), &["rio-cli"])
            .and_then(|_| self.ensure_leader())
            .and_then(|_| self.check_actor_alive())
        {
            return Ok(Response::new(logs::err_stream(status)));
        }
        let req = request.into_inner();
        // `service_verifier` doubles as the signer for OUTGOING calls
        // — `HmacSigner` and `HmacVerifier` are aliases of `HmacKey`
        // (same secret, scheduler both gates incoming and authorizes
        // outgoing with it). r[store.admin.service-gate].
        let stream = match gc::trigger_gc(
            &self.actor,
            &self.store_addr,
            self.service_verifier.clone(),
            self.shutdown.clone(),
            req,
        )
        .await
        {
            Ok(s) => s,
            Err(s) => logs::err_stream(s),
        };
        Ok(Response::new(stream))
    }

    /// Mark a worker draining: `has_capacity()` returns false, dispatch
    /// skips it. In-flight builds continue. Called by:
    ///   - Controller's Pool finalizer cleanup
    ///   - rio-cli `drain` command (operator workstation)
    ///
    /// Builders do NOT call this (their self-drain goodbye was removed
    /// — heartbeat `draining=true` + stream-close are the deregistration
    /// path; the service-token gate below excludes builders by design).
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
        // Service-token gate: builders share this port; a compromised
        // builder draining arbitrary executors is a DoS. rio-cli runs
        // from operator workstations (outside builder netns) but uses
        // the same shared key — both legitimate callers allowlisted.
        self.ensure_service_caller(request.metadata(), &["rio-controller", "rio-cli"])?;
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
        let result = query_actor(&self.actor, |reply| ActorCommand::DrainExecutor {
            executor_id,
            force,
            reply,
        })
        .await?;

        Ok(Response::new(DrainExecutorResponse {
            accepted: result.accepted,
            busy: result.busy,
        }))
    }

    /// Operator cancel — service-token gated, dispatches
    /// `ActorCommand::CancelBuild{caller_tenant: None}` (bypasses the
    /// tenant-ownership check that `SchedulerService::cancel_build`
    /// applies). rio-cli holds a service-HMAC identity, not a
    /// tenant-JWT identity, so it cannot reach
    /// `SchedulerService.CancelBuild` in JWT mode (`r[sched.tenant.authz]`).
    // r[impl admin.rpc.cancel-build]
    #[instrument(skip(self, request), fields(rpc = "CancelBuild"))]
    async fn cancel_build(
        &self,
        request: Request<CancelBuildRequest>,
    ) -> Result<Response<CancelBuildResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_service_caller(request.metadata(), &["rio-cli", "rio-controller"])?;
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let req = request.into_inner();
        let build_id = Uuid::parse_str(&req.build_id)
            .map_err(|e| Status::invalid_argument(format!("invalid build_id: {e}")))?;

        let cancelled = query_actor(&self.actor, |reply| ActorCommand::CancelBuild {
            build_id,
            caller_tenant: None,
            reason: req.reason,
            reply,
        })
        .await?
        .map_err(crate::grpc::SchedulerGrpc::actor_error_to_status)?;

        Ok(Response::new(CancelBuildResponse { cancelled }))
    }

    /// Controller reports a builder/fetcher Pod's k8s termination
    /// reason. OOMKilled/DiskPressure → promote `resource_floor`
    /// for whatever drv was running at disconnect; other reasons →
    /// no-op. Replaces the bare-disconnect OOM heuristic.
    ///
    /// Unknown executor_id → `promoted=false`, NOT an error: the
    /// controller's reconcile loop re-reports the same Pod every
    /// ~10s for `JOB_TTL_SECS=600`; the actor's first-report-wins
    /// dedup means subsequent calls are expected to miss.
    #[instrument(skip(self, request), fields(rpc = "ReportExecutorTermination"))]
    async fn report_executor_termination(
        &self,
        request: Request<ReportExecutorTerminationRequest>,
    ) -> Result<Response<ReportExecutorTerminationResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        // Service-token gate: a forged
        // `{reason: OOMKilled, executor_id: <any>}` from a compromised
        // builder would promote `resource_floor` for arbitrary
        // derivations → fleet-wide over-allocation. Same threat surface
        // as `append_interrupt_sample`.
        self.ensure_service_caller(request.metadata(), &["rio-controller"])?;
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let req = request.into_inner();

        if req.executor_id.is_empty() {
            return Err(Status::invalid_argument("executor_id is required"));
        }
        let reason = TerminationReason::try_from(req.reason).unwrap_or(TerminationReason::Unknown);
        let executor_id = req.executor_id.into();

        let promoted = query_actor(&self.actor, |reply| {
            ActorCommand::ReportExecutorTermination {
                executor_id,
                reason,
                reply,
            }
        })
        .await?;

        Ok(Response::new(ReportExecutorTerminationResponse {
            promoted,
        }))
    }

    #[instrument(skip(self, request), fields(rpc = "ClearPoison"))]
    async fn clear_poison(
        &self,
        request: Request<ClearPoisonRequest>,
    ) -> Result<Response<ClearPoisonResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_service_caller(request.metadata(), &["rio-cli"])?;
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let req = request.into_inner();
        if req.derivation_hash.is_empty() {
            return Err(Status::invalid_argument("derivation_hash is required"));
        }
        let drv_hash: crate::state::DrvHash = req.derivation_hash.into();
        let cleared = query_actor(&self.actor, |reply| ActorCommand::ClearPoison {
            drv_hash,
            reply,
        })
        .await?;
        Ok(Response::new(ClearPoisonResponse { cleared }))
    }

    // r[impl sched.admin.list-poisoned]
    #[instrument(skip(self, request), fields(rpc = "ListPoisoned"))]
    async fn list_poisoned(
        &self,
        request: Request<()>,
    ) -> Result<Response<ListPoisonedResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        // r[impl sched.sla.threat.read-path-auth]
        self.ensure_service_caller(request.metadata(), &["rio-cli", "rio-dashboard"])?;
        self.ensure_leader()?;
        // DB is the source of truth for poisoned_at (the in-memory DAG
        // reconstructs Instant from elapsed_secs at startup but doesn't
        // store the original timestamp for display).
        let db = crate::db::SchedulerDb::new(self.pool.clone());
        let rows = db
            .load_poisoned_derivations()
            .await
            .status_internal("load_poisoned_derivations")?;
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
        // r[impl sched.sla.threat.read-path-auth]
        self.ensure_service_caller(request.metadata(), &["rio-cli", "rio-dashboard"])?;
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
        self.ensure_service_caller(request.metadata(), &["rio-cli"])?;
        self.ensure_leader()?;
        let req = request.into_inner();
        let db = crate::db::SchedulerDb::new(self.pool.clone());
        let resp = tenants::create_tenant(&db, req).await?;
        Ok(Response::new(resp))
    }

    // r[impl sched.admin.delete-tenant]
    #[instrument(skip(self, request), fields(rpc = "DeleteTenant"))]
    async fn delete_tenant(
        &self,
        request: Request<DeleteTenantRequest>,
    ) -> Result<Response<DeleteTenantResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_service_caller(request.metadata(), &["rio-cli"])?;
        self.ensure_leader()?;
        let req = request.into_inner();
        let db = crate::db::SchedulerDb::new(self.pool.clone());
        let resp = tenants::delete_tenant(&db, req).await?;
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
        // r[impl sched.sla.threat.read-path-auth]
        self.ensure_service_caller(request.metadata(), &["rio-cli", "rio-dashboard"])?;
        self.ensure_leader()?;
        let req = request.into_inner();
        let db = crate::db::SchedulerDb::new(self.pool.clone());
        let resp = graph::get_build_graph(&db, &req.build_id, None).await?;
        Ok(Response::new(resp))
    }

    /// ADR-023 spawn-intent stream: one `SpawnIntent` per Ready
    /// derivation, filtered server-side by `{kind, systems, features}`.
    #[instrument(skip(self, request), fields(rpc = "GetSpawnIntents"))]
    async fn get_spawn_intents(
        &self,
        request: Request<GetSpawnIntentsRequest>,
    ) -> Result<Response<GetSpawnIntentsResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let resp = spawn_intents::get_spawn_intents(&self.actor, request.into_inner()).await?;
        Ok(Response::new(resp))
    }

    /// Controller acked it created Jobs for these intents → arm the
    /// Pending-watch (ICE-backoff) timer for each band-targeted one.
    /// Fire-and-forget: a dropped ack means delayed ICE detection,
    /// not a false mark, so `send_unchecked` is correct under
    /// backpressure.
    #[instrument(skip(self, request), fields(rpc = "AckSpawnedIntents"))]
    async fn ack_spawned_intents(
        &self,
        request: Request<AckSpawnedIntentsRequest>,
    ) -> Result<Response<()>, Status> {
        rio_proto::interceptor::link_parent(&request);
        // Service-token gate: a forged ack from a compromised builder
        // arms the ICE-backoff timer for arbitrary intent_ids → false
        // ICE marks bias hw-band selection.
        self.ensure_service_caller(request.metadata(), &["rio-controller"])?;
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let req = request.into_inner();
        self.actor
            .send_unchecked(ActorCommand::AckSpawnedIntents {
                spawned: req.spawned,
                unfulfillable_cells: req.unfulfillable_cells,
                registered_cells: req.registered_cells,
            })
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(()))
    }

    /// ADR-023 §13a: per-hw_class distinct-pod_id bench count, for the
    /// controller's `rio.build/hw-bench-needed` annotation gate.
    /// Reflects the estimator's last `HwTable::load` (~60s stale at
    /// worst), NOT a live PG count.
    #[instrument(skip(self, request), fields(rpc = "HwClassSampled"))]
    async fn hw_class_sampled(
        &self,
        request: Request<HwClassSampledRequest>,
    ) -> Result<Response<HwClassSampledResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        // Service-token gate: hw-class topology is operational
        // telemetry on par with `SlaStatus`. The threat is a
        // compromised builder enumerating which hw_classes are
        // under-sampled to target bench-poisoning at exactly those.
        self.ensure_service_caller(request.metadata(), &["rio-controller"])?;
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let req = request.into_inner();
        let sampled_count = query_actor(&self.actor, |reply| {
            ActorCommand::Admin(AdminQuery::SlaHwSampled {
                hw_classes: req.hw_classes,
                reply,
            })
        })
        .await?;
        Ok(Response::new(HwClassSampledResponse { sampled_count }))
    }

    /// ADR-023 §13a: read-only dump of `[sla.hw_classes]` (h → label
    /// conjunction) so the controller's post-bind annotator matches a
    /// bound Node's labels against the OPERATOR'S schema instead of a
    /// hardcoded 4-key guess. Static config — no actor round-trip.
    #[instrument(skip(self, request), fields(rpc = "GetHwClassConfig"))]
    async fn get_hw_class_config(
        &self,
        request: Request<()>,
    ) -> Result<Response<GetHwClassConfigResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        // Service-token gate: hw-class label schema is operational
        // config; same threat surface as `HwClassSampled` (a
        // compromised builder enumerating which label-conjunctions
        // map to under-sampled classes).
        self.ensure_service_caller(request.metadata(), &["rio-controller"])?;
        self.ensure_leader()?;
        let hw_classes = self
            .sla_config
            .hw_classes
            .iter()
            .map(|(h, def)| {
                let labels = def
                    .labels
                    .iter()
                    .map(|l| rio_proto::types::NodeLabelMatch {
                        key: l.key.clone(),
                        value: l.value.clone(),
                    })
                    .collect();
                (h.clone(), rio_proto::types::HwClassLabels { labels })
            })
            .collect();
        Ok(Response::new(GetHwClassConfigResponse { hw_classes }))
    }

    /// Actor in-memory DAG snapshot for a build — the exact view
    /// `dispatch_ready()` sees, not PG. `executor_has_stream=false`
    /// on an Assigned/Running derivation is the I-025 signal:
    /// PG-vs-stream-pool mismatch, dispatch stuck forever.
    #[instrument(skip(self, request), fields(rpc = "InspectBuildDag"))]
    async fn inspect_build_dag(
        &self,
        request: Request<InspectBuildDagRequest>,
    ) -> Result<Response<InspectBuildDagResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        // r[impl sched.sla.threat.read-path-auth]
        self.ensure_service_caller(request.metadata(), &["rio-cli", "rio-dashboard"])?;
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let req = request.into_inner();
        let build_id: Uuid = req
            .build_id
            .parse()
            .map_err(|_| Status::invalid_argument("invalid build_id UUID"))?;
        let (derivations, live_executor_ids) = query_actor(&self.actor, |reply| {
            ActorCommand::Admin(AdminQuery::InspectBuildDag { build_id, reply })
        })
        .await?;
        Ok(Response::new(InspectBuildDagResponse {
            derivations,
            live_executor_ids,
        }))
    }

    /// Actor in-memory executor map snapshot. Unlike `ListExecutors`
    /// (PG `last_seen`), this reads `self.executors` — exactly what
    /// `dispatch_ready()` filters on. I-048b/c diagnostic.
    ///
    /// NO `ensure_leader()`: a standby's actor map is empty (cleared
    /// on lease loss) and that's USEFUL — `rio-cli workers --diff`
    /// against a standby shows every PG-known worker as PG-only,
    /// confirming "the actor I asked has no streams" rather than
    /// rejecting with FAILED_PRECONDITION and leaving the operator
    /// to guess. Also: NO `check_actor_alive()` — if the actor task
    /// died, `debug_query_workers()`'s `rx.await` returns `ChannelSend`
    /// and the mapped Status surfaces that more precisely than the
    /// guard's generic "actor not alive".
    #[instrument(skip(self, request), fields(rpc = "DebugListExecutors"))]
    async fn debug_list_executors(
        &self,
        request: Request<()>,
    ) -> Result<Response<DebugListExecutorsResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let workers = self
            .actor
            .debug_query_workers()
            .await
            .map_err(crate::grpc::SchedulerGrpc::actor_error_to_status)?;
        let executors = workers
            .into_iter()
            .map(|w| DebugExecutorState {
                executor_id: w.executor_id,
                has_stream: w.has_stream,
                is_registered: w.is_registered,
                warm: w.warm,
                kind: w.kind as i32,
                systems: w.systems,
                last_heartbeat_ago_secs: w.last_heartbeat_ago_secs,
                running_build: w.running_build,
                draining: w.draining,
                store_degraded: w.store_degraded,
            })
            .collect();
        Ok(Response::new(DebugListExecutorsResponse { executors }))
    }

    // ─── ADR-023 SLA overrides ────────────────────────────────────────

    #[instrument(skip(self, request), fields(rpc = "SetSlaOverride"))]
    async fn set_sla_override(
        &self,
        request: Request<SetSlaOverrideRequest>,
    ) -> Result<Response<SlaOverride>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_service_caller(request.metadata(), &["rio-cli"])?;
        self.ensure_leader()?;
        let o = request
            .into_inner()
            .r#override
            .ok_or_else(|| Status::invalid_argument("override is required"))?;
        let row = sla::row_from_proto(&o)?;
        let db = crate::db::SchedulerDb::new(self.pool.clone());
        let inserted = db
            .insert_sla_override(&row)
            .await
            .status_internal("insert_sla_override")?;
        Ok(Response::new(sla::row_to_proto(&inserted)))
    }

    #[instrument(skip(self, request), fields(rpc = "ListSlaOverrides"))]
    async fn list_sla_overrides(
        &self,
        request: Request<ListSlaOverridesRequest>,
    ) -> Result<Response<ListSlaOverridesResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        // r[impl sched.sla.threat.read-path-auth]
        self.ensure_service_caller(request.metadata(), &["rio-cli"])?;
        self.ensure_leader()?;
        let pname = request.into_inner().pname;
        let db = crate::db::SchedulerDb::new(self.pool.clone());
        let rows = db
            .read_sla_overrides(&self.cluster, pname.as_deref())
            .await
            .status_internal("read_sla_overrides")?;
        Ok(Response::new(ListSlaOverridesResponse {
            overrides: rows.iter().map(sla::row_to_proto).collect(),
        }))
    }

    #[instrument(skip(self, request), fields(rpc = "ClearSlaOverride"))]
    async fn clear_sla_override(
        &self,
        request: Request<ClearSlaOverrideRequest>,
    ) -> Result<Response<()>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_service_caller(request.metadata(), &["rio-cli"])?;
        self.ensure_leader()?;
        let id = request.into_inner().id;
        let db = crate::db::SchedulerDb::new(self.pool.clone());
        db.delete_sla_override(id)
            .await
            .status_internal("delete_sla_override")?;
        Ok(Response::new(()))
    }

    #[instrument(skip(self, request), fields(rpc = "ResetSlaModel"))]
    async fn reset_sla_model(
        &self,
        request: Request<ResetSlaModelRequest>,
    ) -> Result<Response<()>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_service_caller(request.metadata(), &["rio-cli"])?;
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let r = request.into_inner();
        let key = ModelKey {
            pname: r.pname,
            system: r.system,
            tenant: r.tenant,
        };
        // PG first, then evict: if the DELETE fails the cache stays
        // intact (operator retries); if evict fails the next refresh
        // tick re-reads an empty ring and overwrites with a Probe fit
        // anyway.
        let db = crate::db::SchedulerDb::new(self.pool.clone());
        db.delete_build_samples_for_key(&key.pname, &key.system, &key.tenant)
            .await
            .status_internal("delete_build_samples_for_key")?;
        query_actor(&self.actor, |reply| {
            ActorCommand::Admin(AdminQuery::SlaEvict {
                key: key.clone(),
                reply,
            })
        })
        .await?;
        Ok(Response::new(()))
    }

    #[instrument(skip(self, request), fields(rpc = "SlaStatus"))]
    async fn sla_status(
        &self,
        request: Request<SlaStatusRequest>,
    ) -> Result<Response<SlaStatusResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        // r[impl sched.sla.threat.read-path-auth]
        self.ensure_service_caller(request.metadata(), &["rio-cli"])?;
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let r = request.into_inner();
        let key = ModelKey {
            pname: r.pname,
            system: r.system,
            tenant: r.tenant,
        };
        let (fit, active) = query_actor(&self.actor, |reply| {
            ActorCommand::Admin(AdminQuery::SlaStatus { key, reply })
        })
        .await?;
        Ok(Response::new(sla::status_from_fit(
            fit.as_ref(),
            active.as_ref(),
        )))
    }

    #[instrument(skip(self, request), fields(rpc = "SlaExplain"))]
    async fn sla_explain(
        &self,
        request: Request<SlaExplainRequest>,
    ) -> Result<Response<SlaExplainResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        // r[impl sched.sla.threat.read-path-auth]
        self.ensure_service_caller(request.metadata(), &["rio-cli"])?;
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let r = request.into_inner();
        let key = ModelKey {
            pname: r.pname,
            system: r.system,
            tenant: r.tenant,
        };
        let result = query_actor(&self.actor, |reply| {
            ActorCommand::Admin(AdminQuery::SlaExplain { key, reply })
        })
        .await?;
        Ok(Response::new(sla::explain_to_proto(&result)))
    }

    #[instrument(skip(self, request), fields(rpc = "ExportSlaCorpus"))]
    async fn export_sla_corpus(
        &self,
        request: Request<ExportSlaCorpusRequest>,
    ) -> Result<Response<ExportSlaCorpusResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        // r[impl sched.sla.threat.read-path-auth]
        self.ensure_service_caller(request.metadata(), &["rio-cli"])?;
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let r = request.into_inner();
        let corpus = query_actor(&self.actor, |reply| {
            ActorCommand::Admin(AdminQuery::SlaExportCorpus {
                tenant: r.tenant,
                min_n: r.min_n,
                reply,
            })
        })
        .await?;
        let entries = corpus.entries.len() as u32;
        let json = serde_json::to_string(&corpus)
            .map_err(|e| Status::internal(format!("serialize corpus: {e}")))?;
        // Populate BOTH `json` (v1, on-disk format for `[sla].seed_corpus`)
        // and `corpus` (v2, typed wire body).
        let proto_corpus = rio_proto::types::SeedCorpus::from(&corpus);
        Ok(Response::new(ExportSlaCorpusResponse {
            json,
            entries,
            corpus: Some(proto_corpus),
        }))
    }

    #[instrument(skip(self, request), fields(rpc = "ImportSlaCorpus"))]
    async fn import_sla_corpus(
        &self,
        request: Request<ImportSlaCorpusRequest>,
    ) -> Result<Response<ImportSlaCorpusResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_service_caller(request.metadata(), &["rio-cli"])?;
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let r = request.into_inner();
        // Prefer the typed v2 body when present (non-empty entries) so
        // range-validation runs on proto-typed fields. Fall back to the
        // v1 `json` string for old clients / on-disk-file passthrough.
        let corpus: crate::sla::prior::SeedCorpus = match r.corpus {
            Some(c) if !c.entries.is_empty() => c.into(),
            _ => serde_json::from_str(&r.json)
                .map_err(|e| Status::invalid_argument(format!("parse corpus json: {e}")))?,
        };
        // r[impl sched.sla.threat.corpus-clamp+2]
        // Gap (c): is_finite + range checks BEFORE the corpus reaches
        // the actor / priors.seed. The seed table bypasses
        // clamp_to_operator, so a single `s = 1e308` would otherwise
        // propagate verbatim into partial_pool → T(c) → solve.
        // `ValidatedSeedCorpus` is the only type the actor accepts —
        // both this RPC and the startup `[sla].seedCorpus` load go
        // through the same constructor, so neither path can skip it.
        let corpus = crate::sla::prior::ValidatedSeedCorpus::validate(corpus, &self.sla_config)
            .map_err(|e| Status::invalid_argument(format!("corpus rejected: {e}")))?;
        let ref_hw_class = corpus.ref_hw_class().to_owned();
        let (n, scale) = query_actor(&self.actor, |reply| {
            ActorCommand::Admin(AdminQuery::SlaImportCorpus { corpus, reply })
        })
        .await?;
        Ok(Response::new(ImportSlaCorpusResponse {
            entries_loaded: n as u32,
            ref_hw_class,
            rescale_factor: scale,
        }))
    }

    /// VM-test fixture: write one synthetic `build_samples` row.
    /// Compile-gated on `feature = "test-fixtures"` (default-on; same
    /// pattern as rio-builder's `RIO_BUILDER_SCRIPT` — crate2nix bakes
    /// features at lock-time so a per-crate override hook doesn't
    /// exist) AND runtime-gated on `RIO_ADMIN_TEST_FIXTURES` so a
    /// misrouted prod call is refused even with admin auth. The env var
    /// is only set by the sla-sizing VM scenario's standalone fixture.
    #[instrument(skip(self, request), fields(rpc = "InjectBuildSample"))]
    async fn inject_build_sample(
        &self,
        request: Request<InjectBuildSampleRequest>,
    ) -> Result<Response<()>, Status> {
        #[cfg(feature = "test-fixtures")]
        {
            rio_proto::interceptor::link_parent(&request);
            // Service-token gate first (defence-in-depth: env-gate
            // below is NOT authz; a misconfigured prod with the var set
            // is still gated).
            self.ensure_service_caller(request.metadata(), &["rio-cli"])?;
            self.ensure_leader()?;
            if std::env::var_os("RIO_ADMIN_TEST_FIXTURES").is_none() {
                return Err(Status::permission_denied(
                    "InjectBuildSample is a test fixture; set RIO_ADMIN_TEST_FIXTURES to enable",
                ));
            }
            let r = request.into_inner();
            let db = crate::db::SchedulerDb::new(self.pool.clone());
            db.write_build_sample(&crate::db::BuildSampleRow {
                pname: r.pname,
                system: r.system,
                tenant: r.tenant,
                duration_secs: r.duration_secs,
                peak_memory_bytes: r.peak_memory_bytes,
                cpu_limit_cores: r.cpu_limit_cores,
                cpu_seconds_total: r.cpu_seconds_total,
                version: r.version,
                hw_class: r.hw_class,
                ..Default::default()
            })
            .await
            .map_err(|e| Status::internal(format!("write_build_sample: {e}")))?;
            Ok(Response::new(()))
        }
        #[cfg(not(feature = "test-fixtures"))]
        {
            let _ = request;
            Err(Status::unimplemented(
                "InjectBuildSample requires a test-fixtures build",
            ))
        }
    }

    /// ADR-023 phase-13: append one `interrupt_samples` row. Called by
    /// the controller's spot-interrupt watcher (no test-fixture gate —
    /// this is a production write path). NOT leader-gated: the
    /// controller's balanced channel routes to the leader anyway, and
    /// the table is append-only so a stray standby write is harmless.
    ///
    /// Idempotent on `event_uid`: the watcher consumes
    /// `.applied_objects()` which re-yields every still-extant Event on
    /// relist/reconnect/controller-restart. `ON CONFLICT (event_uid)
    /// WHERE event_uid IS NOT NULL DO NOTHING` (partial unique index,
    /// M_047) deduplicates `kind='interrupt'` rows so λ's numerator
    /// doesn't inflate; exposure rows pass `event_uid=None` and are
    /// unconstrained.
    #[instrument(skip(self, request), fields(rpc = "AppendInterruptSample"))]
    async fn append_interrupt_sample(
        &self,
        request: Request<AppendInterruptSampleRequest>,
    ) -> Result<Response<()>, Status> {
        rio_proto::interceptor::link_parent(&request);
        // Service-token gate: a single forged
        // `{hw_class:"x-x-x-hi", kind:"exposure", value:1e12}` from a
        // compromised builder drives λ[Hi]≈0 → fleet-wide solver bias.
        // Per the threat model "the worker is NOT trusted".
        self.ensure_service_caller(request.metadata(), &["rio-controller"])?;
        let r = request.into_inner();
        // Defense-in-depth input validation: lands regardless of the
        // token gate. `MAX_HW_CLASS_LEN` charset gate so garbage
        // hw_class can't pollute the table even in dev mode.
        if !matches!(r.kind.as_str(), "interrupt" | "exposure") {
            return Err(Status::invalid_argument(format!(
                "kind must be 'interrupt' or 'exposure', got {:?}",
                r.kind
            )));
        }
        if !r.value.is_finite() || r.value < 0.0 {
            return Err(Status::invalid_argument(format!(
                "value must be finite and non-negative, got {}",
                r.value
            )));
        }
        if !rio_common::limits::is_hw_class_name(&r.hw_class) {
            return Err(Status::invalid_argument(format!(
                "hw_class {:?} must be 1..={} chars of [a-z0-9-]",
                r.hw_class,
                rio_common::limits::MAX_HW_CLASS_LEN
            )));
        }
        sqlx::query(
            "INSERT INTO interrupt_samples (cluster, hw_class, kind, value, event_uid) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (event_uid) WHERE event_uid IS NOT NULL DO NOTHING",
        )
        .bind(&self.cluster)
        .bind(&r.hw_class)
        .bind(&r.kind)
        .bind(r.value)
        .bind(r.event_uid.as_deref())
        .execute(&self.pool)
        .await
        .map_err(|e| Status::internal(format!("append_interrupt_sample: {e}")))?;
        Ok(Response::new(()))
    }
}

#[cfg(test)]
mod tests;
