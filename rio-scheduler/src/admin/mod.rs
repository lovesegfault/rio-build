//! AdminService gRPC implementation.
//!
//! The operator/controller surface: `ClusterStatus`, `ListExecutors`
//! (open-attempt backed), `ListOpenAttempts`, `ListBuilds`,
//! `ClearPoison`, `ListTenants`, `CreateTenant`, `GetBuildGraph`,
//! `GetSpawnIntents`, `TriggerGC`, the SLA family. The stream-era
//! `DrainExecutor`, `DebugListExecutors` and `ReportExecutorTermination`
//! RPCs were removed by the proto sweep (cordon + cancel/Job-delete,
//! `ListOpenAttempts` + the Job census, and `ReportAttemptOutcome` are
//! the successors).
//!
//! Per-RPC bodies live in submodules (`gc`, `tenants`,
//! `builds`, `workers`, `graph`, `spawn_intents`). This file holds only the
//! [`AdminServiceImpl`] state struct + thin wrapper methods that
//! delegate into the submodules. Split from a single 861L file (P0383)
//! after collision count hit 20.

use std::sync::Arc;
use std::time::{Instant, SystemTime};

use sqlx::PgPool;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use rio_common::grpc::StatusExt;
use tracing::instrument;

use rio_common::tenant::NormalizedName;
use rio_proto::AdminService;
use rio_proto::types::ClearSlaOverrideRequest;
use rio_proto::types::{
    AckSpawnedIntentsRequest, AppendInterruptSampleRequest, CancelBuildRequest,
    CancelBuildResponse, ClearPoisonRequest, ClearPoisonResponse, ClusterStatusResponse,
    CreateTenantRequest, CreateTenantResponse, DeleteTenantRequest, DeleteTenantResponse,
    ExportSlaCorpusRequest, ExportSlaCorpusResponse, GcProgress, GcRequest, GetBuildGraphRequest,
    GetBuildGraphResponse, GetHwClassConfigResponse, GetSlaMispredictorsRequest,
    GetSlaMispredictorsResponse, GetSpawnIntentsRequest, GetSpawnIntentsResponse,
    HwClassSampledRequest, HwClassSampledResponse, ImportSlaCorpusRequest, ImportSlaCorpusResponse,
    InjectBuildSampleRequest, InspectBuildDagRequest, InspectBuildDagResponse, ListBuildsRequest,
    ListBuildsResponse, ListExecutorsRequest, ListExecutorsResponse, ListPoisonedResponse,
    ListSlaOverridesRequest, ListSlaOverridesResponse, ListTenantsResponse,
    MintExecutorTokensRequest, MintExecutorTokensResponse, PoisonedDerivation,
    ResetSlaModelRequest, SetSlaOverrideRequest, SlaDefaultsResponse, SlaExplainRequest,
    SlaExplainResponse, SlaOverride, SlaStatusRequest, SlaStatusResponse,
};
use uuid::Uuid;

use crate::actor::{ActorCommand, ActorHandle, AdminQuery};
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
mod sla;
mod spawn_intents;
mod tenants;

pub use gc::spawn_store_size_refresh;
pub use sla::duration_fit_from_status;

pub struct AdminServiceImpl {
    pool: PgPool,
    /// For `ClusterStatus` and the actor-backed admin queries — sends
    /// query commands into the actor event loop. `ClusterSnapshot`
    /// bypasses backpressure (`send_unchecked`): the autoscaler needs a
    /// reading especially when saturated. Dropping the query under load
    /// would blind the controller exactly when it needs to scale up.
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
    /// (`AppendInterruptSample`, `CancelBuild`). `None` = dev mode
    /// (accept all) — same pass-through pattern as the store's
    /// assignment-token verifier. The threat: builders share port 9001
    /// with this service (CCNP allows scheduler:9001 at L4 only), and a
    /// compromised builder could forge `interrupt_samples` to bias
    /// λ\[h\] or cancel arbitrary builds. See `r[sec.authz.service-token]`.
    service_verifier: Option<Arc<rio_auth::hmac::HmacVerifier>>,
    /// §13c-2: the same `Arc<RwLock<CostTable>>` `main.rs` shares with
    /// the actor + cost pollers. `GetHwClassConfig` reads
    /// `catalog_ceilings()` to fold `min(catalog, cfg)` before
    /// serialize. Held directly (not via `query_actor`) so the RPC stays
    /// actor-independent — a wedged actor doesn't block the controller's
    /// 300s `HwClassConfig` refresh.
    cost_table: Arc<parking_lot::RwLock<crate::sla::cost::CostTable>>,
}

impl AdminServiceImpl {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: PgPool,
        actor: ActorHandle,
        store_addr: String,
        store_size_bytes: Arc<std::sync::atomic::AtomicU64>,
        leader: crate::lease::LeaderState,
        shutdown: rio_common::signal::Token,
        cluster: String,
        sla_config: Arc<crate::sla::config::SlaConfig>,
        service_verifier: Option<Arc<rio_auth::hmac::HmacVerifier>>,
        cost_table: Arc<parking_lot::RwLock<crate::sla::cost::CostTable>>,
    ) -> Self {
        Self {
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
            cost_table,
        }
    }

    /// Actor-dead check. Delegates to the shared
    /// [`actor_guards::check_actor_alive`](crate::grpc::actor_guards)
    /// so the error string stays in lockstep with `SchedulerGrpc`.
    fn check_actor_alive(&self) -> Result<(), Status> {
        crate::grpc::actor_guards::check_actor_alive(&self.actor)
    }

    /// Leader guard. Admin RPCs mutate state (ClearPoison,
    /// CreateTenant, TriggerGC) or reflect leader-owned state
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
    /// (poisoning λ\[h\]), cancel arbitrary builds, or set SLA
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
    type TriggerGCStream = ReceiverStream<Result<GcProgress, Status>>;

    /// Cluster-wide counts for the controller's autoscaling loop.
    ///
    /// The controller spawns one-shot Jobs up to `max_concurrent` from
    /// `queued_derivations`. `queued_derivations` is the primary signal —
    /// that's how many ready-to-build derivations are waiting for a
    /// worker, counted from DAG status (always the sum of
    /// `queued_by_system`). `running_derivations` is secondary (for
    /// "scale-down is safe when queue=0 AND running is below
    /// capacity"). The executor counts are the busy view (one open
    /// pull-mode attempt per in-flight derivation); `draining_executors`
    /// is always 0 — per-executor drain retired with the stream
    /// session.
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

    /// Busy-fleet view over the durable open-attempt rows (one entry
    /// per open pull-mode attempt — the pod that pulled it). Kept so
    /// existing CLI/dashboard/controller callers keep a working
    /// endpoint until the 1d proto sweep; `ListOpenAttempts` is the
    /// attempt-keyed form of the same view.
    // r[impl sched.admin.list-executors+2]
    #[instrument(skip(self, request), fields(rpc = "ListExecutors"))]
    async fn list_executors(
        &self,
        request: Request<ListExecutorsRequest>,
    ) -> Result<Response<ListExecutorsResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        let req = request.into_inner();
        let db = crate::db::SchedulerDb::new(self.pool.clone());
        let resp =
            executors::list_executors(&db, &req.status_filter, self.leader.leader_for()).await?;
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
        // grpc-web compatibility: returning Err(Status) from a
        // server-streaming handler makes tonic emit a Trailers-Only
        // response the browser can't read. ALL error paths return
        // Ok(stream-yielding-Err); the handler never returns Err.
        if let Err(status) = self
            .ensure_service_caller(request.metadata(), &["rio-cli"])
            .and_then(|_| self.ensure_leader())
            .and_then(|_| self.check_actor_alive())
        {
            return Ok(Response::new(gc::err_stream(status)));
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
            Err(s) => gc::err_stream(s),
        };
        Ok(Response::new(stream))
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

    // Pull-mode attempt lifecycle surface (additive): the controller's
    // unified pod-terminal intake and the open-attempt view. Nothing in
    // production calls these until a pool opts into pull mode; every
    // existing RPC is untouched.

    /// Controller reports one pull-mode attempt's terminal status (the
    /// C4/C5 unification). Idempotent by attempt identity; the only
    /// write is the first-writer-wins reason fill on an existing
    /// classification row. A report for an identity with no attempt —
    /// a pod that died without ever completing a pull — is acknowledged
    /// and charges nothing (the no-attempt no-op rule).
    // r[impl ctrl.report.attempt-outcome]
    #[instrument(skip(self, request), fields(rpc = "ReportAttemptOutcome"))]
    async fn report_attempt_outcome(
        &self,
        request: Request<rio_proto::types::ReportAttemptOutcomeRequest>,
    ) -> Result<Response<rio_proto::types::ReportAttemptOutcomeResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        // Same gate as ReportExecutorTermination: a forged terminal
        // report from a compromised builder must not be able to touch
        // attempt classification.
        self.ensure_service_caller(request.metadata(), &["rio-controller"])?;
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let req = request.into_inner();
        if req.intent_id.is_empty() && req.job_name.is_empty() && req.exec_id.is_empty() {
            return Err(Status::invalid_argument(
                "at least one of intent_id, job_name, exec_id is required",
            ));
        }
        let exec_id = if req.exec_id.is_empty() {
            None
        } else {
            Some(
                req.exec_id
                    .parse::<Uuid>()
                    .map_err(|_| Status::invalid_argument("exec_id must be a UUID when present"))?,
            )
        };
        let identity = crate::actor::pull::AttemptIdentity {
            intent_id: (!req.intent_id.is_empty()).then_some(req.intent_id),
            job_name: (!req.job_name.is_empty()).then_some(req.job_name),
            exec_id,
        };
        let reason = rio_proto::types::AttemptTerminalReason::try_from(req.reason)
            .unwrap_or(rio_proto::types::AttemptTerminalReason::Unspecified);
        let node_name = (!req.node_name.is_empty()).then_some(req.node_name);

        let result = query_actor(&self.actor, |reply| ActorCommand::ReportAttemptOutcome {
            identity,
            reason,
            node_name,
            reply,
        })
        .await?;
        match result {
            Ok(()) => Ok(Response::new(
                rio_proto::types::ReportAttemptOutcomeResponse {},
            )),
            Err(crate::actor::PullRejection::NotLeader)
            | Err(crate::actor::PullRejection::StaleGeneration) => {
                Err(Status::unavailable("not leader (standby replica)"))
            }
            Err(crate::actor::PullRejection::TokenMismatch) => {
                Err(Status::permission_denied("attempt identity mismatch"))
            }
            Err(crate::actor::PullRejection::Internal(msg)) => Err(Status::internal(msg)),
        }
    }

    /// Ledger-backed open-attempt view: the controller's busy-signal
    /// bridge, the cancel/preempt read, the OA2 clustering input, and
    /// the operator fleet view. Every row is an attempt minted by the
    /// pull transaction (the only execution writer left). Leader-read
    /// with the same `ensure_leader` discipline as `ListExecutors`;
    /// `leader_for_secs` carries the same fail-closed freshness input.
    // r[impl sched.admin.list-open-attempts+2]
    #[instrument(skip(self, request), fields(rpc = "ListOpenAttempts"))]
    async fn list_open_attempts(
        &self,
        request: Request<rio_proto::types::ListOpenAttemptsRequest>,
    ) -> Result<Response<rio_proto::types::ListOpenAttemptsResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        // Read-path gate (r[sched.sla.threat.read-path-auth] posture):
        // the view names other tenants' in-flight derivation paths and
        // node bindings, so it is service-token gated like the other
        // topology-revealing reads. The controller (busy bridge), CLI,
        // and dashboard are the legitimate consumers.
        self.ensure_service_caller(
            request.metadata(),
            &["rio-controller", "rio-cli", "rio-dashboard"],
        )?;
        self.ensure_leader()?;
        let db = crate::db::SchedulerDb::new(self.pool.clone());
        let rows = db
            .list_open_pull_attempts()
            .await
            .status_internal("list_open_pull_attempts")?;
        let attempts = rows
            .into_iter()
            .map(|r| rio_proto::types::OpenAttempt {
                intent_id: r.drv_hash,
                derivation: r.drv_path,
                exec_id: r.exec_id.to_string(),
                executor_id: r.executor_id,
                source_node: r.source_node.unwrap_or_default(),
                generation: r.generation.max(0) as u64,
                assigned_at_age_secs: r.age_secs.max(0.0) as u64,
                // Deadline enrichment (the intent deadline) is consumer
                // work when a consumer needs it; 0 = unknown for now.
                deadline_secs: 0,
            })
            .collect();
        Ok(Response::new(rio_proto::types::ListOpenAttemptsResponse {
            attempts,
            leader_for_secs: self.leader.leader_for().map_or(0, |d| d.as_secs()),
        }))
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
        // store the original timestamp for display). `failed_executors`
        // is the attempt-ledger aggregate (the only failure-history
        // record since migration 075), so poisons list the executors
        // whose failures charged them.
        let db = crate::db::SchedulerDb::new(self.pool.clone());
        let rows = db
            .load_poisoned_display()
            .await
            .status_internal("load_poisoned_display")?;
        let derivations = rows
            .into_iter()
            .map(
                |(drv_path, failed_executors, poisoned_secs_ago)| PoisonedDerivation {
                    drv_path,
                    failed_executors,
                    poisoned_secs_ago: poisoned_secs_ago.max(0.0) as u64,
                },
            )
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
        // r[impl sched.sla.threat.read-path-auth]
        // Gated on payload sensitivity, not verb: builders share port
        // 9001 and the response leaks per-tenant DAG topology
        // (intent_id == drv_hash, system, required_features). The
        // credential previously carried here (`executor_token`) now
        // mints via `MintExecutorTokens` (controller-only) so
        // dashboard/CLI hold plain data.
        self.ensure_service_caller(
            request.metadata(),
            &["rio-controller", "rio-dashboard", "rio-cli"],
        )?;
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let resp = spawn_intents::get_spawn_intents(&self.actor, request.into_inner()).await?;
        Ok(Response::new(resp))
    }

    /// Mint per-intent `RIO_EXECUTOR_TOKEN`s. Controller-only: a signed
    /// credential is the most sensitive payload class
    /// (`r[sched.sla.threat.read-path-auth]`); dashboard/CLI never held
    /// a use for it.
    #[instrument(skip(self, request), fields(rpc = "MintExecutorTokens"))]
    async fn mint_executor_tokens(
        &self,
        request: Request<MintExecutorTokensRequest>,
    ) -> Result<Response<MintExecutorTokensResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_service_caller(request.metadata(), &["rio-controller"])?;
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let resp = spawn_intents::mint_executor_tokens(&self.actor, request.into_inner()).await?;
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
                observed_instance_types: req.observed_instance_types,
                bound_intents: req.bound_intents,
            })
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(()))
    }

    /// ADR-023 §13a: per-hw_class **per-dimension distinct-tenant**
    /// bench count, for the controller's `rio.build/hw-bench-needed`
    /// annotation gate. bug_013: same unit (tenants) + granularity
    /// (per-dim) as `cross_tenant_median`'s `min_tenants` gate so one
    /// tenant cannot mark a foreign hw_class fully-benched. Reflects
    /// the estimator's last `HwTable::load` (~60s stale at worst), NOT
    /// a live PG count.
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
        let per_dim = query_actor(&self.actor, |reply| {
            ActorCommand::Admin(AdminQuery::SlaHwSampled {
                hw_classes: req.hw_classes,
                reply,
            })
        })
        .await?;
        let sampled_count = per_dim
            .into_iter()
            .map(|(h, n)| (h, rio_proto::types::HwDimCounts { per_dim: n.into() }))
            .collect();
        Ok(Response::new(HwClassSampledResponse {
            sampled_count,
            // merged_bug_001: single source of truth — the controller's
            // bench-needed gate compares against THIS, not a duplicated
            // constant. r3 R3B5 reconciled unit+granularity but left
            // value at 3; the scheduler's gate is 5.
            trust_threshold: Some(crate::sla::FLEET_MEDIAN_MIN_TENANTS as u32),
        }))
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
        // Snapshot the catalog + resolved global once for the whole
        // iteration — avoids 2N lock acquisitions and a (theoretical)
        // TOCTOU between the `.0` and `.1` reads if the lock ever
        // races a `carry_catalog`.
        let (catalog, global) = {
            let ct = self.cost_table.read();
            (ct.catalog_ceilings().clone(), ct.resolved_global())
        };
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
                let requirements = def
                    .requirements
                    .iter()
                    .map(|r| rio_proto::types::NodeSelectorRequirement {
                        key: r.key.clone(),
                        operator: r.operator.clone(),
                        values: r.values.clone(),
                    })
                    .collect();
                let taints = def
                    .taints
                    .iter()
                    .map(|t| rio_proto::types::NodeTaint {
                        key: t.key.clone(),
                        value: t.value.clone(),
                        effect: t.effect.clone(),
                    })
                    .collect();
                (
                    h.clone(),
                    rio_proto::types::HwClassLabels {
                        labels,
                        requirements,
                        node_class: def.node_class.clone(),
                        // §13c-2 r[impl scheduler.sla.ceiling.controller-mirror]:
                        // ship `min(catalog, cfg)` with each falling to
                        // global. Wire stays nonzero (`validate()` rejects
                        // global=0 and `Some(0)` overrides; the catalog
                        // cores axis floors at 1 via `derive_ceilings`'
                        // `.max(1)`, mem is a real instance type's
                        // memory); the controller's `ceilings_for`
                        // `>0` filter survives.
                        max_cores: self.sla_config.class_ceilings(h, &catalog, global).0,
                        max_mem: self.sla_config.class_ceilings(h, &catalog, global).1,
                        taints,
                        provides_features: def.provides_features.clone(),
                        max_fleet_cores: def.max_fleet_cores,
                        capacity_types: def
                            .capacity_types
                            .iter()
                            .map(|c| c.label().to_string())
                            .collect(),
                    },
                )
            })
            .collect();
        Ok(Response::new(GetHwClassConfigResponse {
            hw_classes,
            global_max_cores: global.0,
            global_max_mem: global.1,
        }))
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

    #[instrument(skip(self, request), fields(rpc = "GetSlaDefaults"))]
    async fn get_sla_defaults(
        &self,
        request: Request<()>,
    ) -> Result<Response<SlaDefaultsResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        // r[impl sched.sla.threat.read-path-auth]
        self.ensure_service_caller(request.metadata(), &["rio-cli"])?;
        self.ensure_leader()?;
        // §13c-3: report the EFFECTIVE (boot-resolved, catalog-derived
        // under Spot) global ceiling, not the configured `Option<>`
        // override — the operator querying `GetSlaDefaults` wants to
        // know what the solve actually caps at.
        let resolved = self.cost_table.read().resolved_global();
        Ok(Response::new(sla::defaults_from_config(
            &self.sla_config,
            resolved,
        )))
    }

    #[instrument(skip(self, request), fields(rpc = "GetSlaMispredictors"))]
    async fn get_sla_mispredictors(
        &self,
        request: Request<GetSlaMispredictorsRequest>,
    ) -> Result<Response<GetSlaMispredictorsResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        // r[impl sched.sla.threat.read-path-auth]
        self.ensure_service_caller(request.metadata(), &["rio-cli"])?;
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let r = request.into_inner();
        let entries = query_actor(&self.actor, |reply| {
            ActorCommand::Admin(AdminQuery::SlaMispredictors {
                top_n: r.top_n,
                reply,
            })
        })
        .await?;
        Ok(Response::new(sla::mispredictors_to_proto(entries)))
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
        // r[impl sched.sla.threat.corpus-clamp+3]
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
