//! gRPC service implementations for SchedulerService and ExecutorService.
//!
//! Both services run in the same scheduler binary. They communicate with the
//! DAG actor via the `ActorHandle`.
//!
//! The actual `impl` blocks live in submodules — `scheduler_service`
//! (client-facing RPCs) and `worker_service` (worker streaming +
//! heartbeat). This file holds only the shared [`SchedulerGrpc`] state
//! struct, constructors, and common helpers. Split from a single 1087L
//! file (P0356) after collision count hit 33 — SubmitBuild/WatchBuild
//! changes no longer conflict with heartbeat/stream-dispatch changes.

pub(crate) mod actor_guards;
mod executor_service;
mod scheduler_service;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::sync::broadcast;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Status};
use tracing::warn;
use uuid::Uuid;

use rio_auth::hmac::{ExecutorClaims, HmacKey};
use rio_common::grpc::StatusExt;
use rio_common::tenant::NormalizedName;

use crate::actor::{ActorCommand, ActorError, ActorHandle, BuildEventReceivers};
use crate::db::SchedulerDb;

/// Shared scheduler state passed to gRPC handlers.
#[derive(Clone)]
pub struct SchedulerGrpc {
    // Fields `pub(super)` so the per-service submodules
    // (scheduler_service.rs, worker_service.rs) can read them
    // directly. Inherent-impl methods on `SchedulerGrpc` defined
    // in a child module can't reach private fields of the parent
    // module's struct.
    pub(super) actor: ActorHandle,
    /// DB handle for tenant resolve / jti revocation. `Option` so
    /// `new_for_tests` can skip it (None → no tenant resolve).
    /// Production always sets it — main.rs constructs the same
    /// `SchedulerDb` for the actor. Holds `SchedulerDb` (not bare
    /// `PgPool`) so all SQL goes through the `db/` module.
    pub(super) db: Option<SchedulerDb>,
    /// Shared with the lease loop. When false (standby), all
    /// handlers return UNAVAILABLE immediately — clients with
    /// a health-aware balanced channel route to the leader
    /// instead. Tests default to `true` (always-leader).
    is_leader: Arc<AtomicBool>,
    /// True when a JWT pubkey is configured. The interceptor is
    /// permissive-on-absent-header (worker/health/admin callers
    /// don't carry tenant tokens), so SchedulerService handlers
    /// must close that gap themselves: when `jwt_mode` is set,
    /// [`Self::require_tenant`] rejects requests without
    /// interceptor-attached `TenantClaims`. See
    /// `r[sched.tenant.authz]`.
    pub(super) jwt_mode: bool,
    /// Assignment-HMAC key, reused as the executor-identity verifier
    /// (`r[sec.executor.identity-token]`). `None` = dev mode
    /// (token-less ExecutorService calls accepted). When `Some`,
    /// [`Self::require_executor`] rejects `BuildExecution` /
    /// `Heartbeat` without a valid `x-rio-executor-token`.
    pub(super) hmac_key: Option<Arc<HmacKey>>,
    /// Service-HMAC verifier (the `x-rio-service-token` family — the
    /// SEPARATE key the admin surfaces verify). Verifies the store's
    /// kind-attested materialization credential
    /// (`ServiceClaims { caller: "rio-store" }`) on the
    /// materialization-only ExecutorService operations
    /// (ListMaterializationJobs, kind=MATERIALIZATION PullAssignment,
    /// materialization ReportOutcome). `None` + `hmac_key: None` = full
    /// dev mode (open); `None` + `hmac_key: Some` = closed for
    /// materialization operations (no acceptable credential exists).
    pub(super) service_verifier: Option<Arc<HmacKey>>,
}

impl SchedulerGrpc {
    /// Test-only constructor. Production uses [`new`](Self::new).
    #[cfg(test)]
    pub fn new_for_tests(actor: ActorHandle) -> Self {
        Self {
            actor,
            db: None,
            is_leader: Arc::new(AtomicBool::new(true)),
            jwt_mode: false,
            hmac_key: None,
            service_verifier: None,
        }
    }

    /// Test constructor with a PG pool for tenant-resolution tests.
    /// Like `new_for_tests` but with a pool so `resolve_tenant` works.
    #[cfg(test)]
    pub fn new_for_tests_with_pool(actor: ActorHandle, pool: sqlx::PgPool) -> Self {
        Self {
            actor,
            db: Some(SchedulerDb::new(pool)),
            is_leader: Arc::new(AtomicBool::new(true)),
            jwt_mode: false,
            hmac_key: None,
            service_verifier: None,
        }
    }

    /// Production constructor.
    ///
    /// `db`: for tenant resolve / jti revocation. main.rs already
    /// has it (same pool as the actor's `SchedulerDb`).
    ///
    /// `jwt_mode`: whether a JWT pubkey is configured (drives
    /// `require_tenant`). `hmac_key`: assignment-HMAC key, reused as
    /// the executor-identity verifier (drives `require_executor`).
    /// `service_verifier`: service-HMAC verifier for the store's
    /// materialization credential (drives `require_store_service`).
    pub fn new(
        actor: ActorHandle,
        db: SchedulerDb,
        is_leader: Arc<AtomicBool>,
        jwt_mode: bool,
        hmac_key: Option<Arc<HmacKey>>,
        service_verifier: Option<Arc<HmacKey>>,
    ) -> Self {
        Self {
            actor,
            db: Some(db),
            is_leader,
            jwt_mode,
            hmac_key,
            service_verifier,
        }
    }

    /// Check if the actor is alive; return UNAVAILABLE if dead (panicked).
    /// Delegates to [`actor_guards::check_actor_alive`].
    pub(super) fn check_actor_alive(&self) -> Result<(), Status> {
        actor_guards::check_actor_alive(&self.actor)
    }

    /// Return UNAVAILABLE when this replica is not the leader.
    /// Delegates to [`actor_guards::ensure_leader`].
    pub(super) fn ensure_leader(&self) -> Result<(), Status> {
        actor_guards::ensure_leader(&self.is_leader)
    }

    /// Convert an ActorError to a tonic Status.
    /// Delegates to [`actor_guards::actor_error_to_status`] — kept as a
    /// wrapper so existing `Self::actor_error_to_status` call sites and
    /// the test at `grpc/tests.rs` stay unchanged.
    pub(crate) fn actor_error_to_status(err: ActorError) -> Status {
        actor_guards::actor_error_to_status(err)
    }

    /// Send a command to the actor and await its oneshot reply, mapping
    /// errors to Status. Combines the `send().await? + reply_rx.await??`
    /// pattern that appears in every request handler.
    pub(super) async fn send_and_await<R>(
        &self,
        cmd: ActorCommand,
        reply_rx: oneshot::Receiver<Result<R, ActorError>>,
    ) -> Result<R, Status> {
        self.actor
            .send(cmd)
            .await
            .map_err(Self::actor_error_to_status)?;
        reply_rx
            .await
            .map_err(|_| Status::internal("actor dropped reply channel"))?
            .map_err(Self::actor_error_to_status)
    }

    /// Parse a build_id string into a Uuid with a standard error message.
    /// Includes the parse error detail so CLI users see why it's invalid.
    pub(crate) fn parse_build_id(s: &str) -> Result<Uuid, Status> {
        s.parse().status_invalid("invalid build_id UUID")
    }

    // r[impl sched.tenant.authz+2]
    // r[impl gw.jwt.verify]
    /// Single chokepoint reconciling the permissive interceptor's third
    /// state ("header absent → no Claims attached, request passes") with
    /// per-RPC tenant authorization AND jti revocation.
    ///
    /// Returns the interceptor-attached `(TenantClaims.sub, jti)` when
    /// present. When `jwt_mode` is set and no Claims are attached,
    /// returns `Unauthenticated` — closes the gap that lets an untrusted
    /// builder (which reaches :9001 for ExecutorService and never sets
    /// `x-rio-tenant-token`) call SchedulerService RPCs token-less.
    /// `Ok(None)` only in dev mode (no pubkey configured).
    ///
    /// **jti revocation** (`r[gw.jwt.verify]`): when Claims ARE present,
    /// `claims.jti` is checked against `jwt_revoked` and a hit returns
    /// `Unauthenticated("token revoked")`. This is the scheduler-level
    /// revocation invariant — the gateway interceptor stays PG-free
    /// (stateless N-replica HA), and the store does NOT duplicate it
    /// (SubmitBuild is the ingress choke point for builds; everything
    /// downstream of an accepted submission inherits its validation).
    /// Hoisted from a SubmitBuild-only inline block: `CancelBuild` /
    /// `WatchBuild` / `QueryBuildStatus` are independent client-facing
    /// ingress points reachable directly with a leaked token.
    pub(super) async fn require_tenant<T>(
        &self,
        req: &Request<T>,
    ) -> Result<Option<(Uuid, String)>, Status> {
        match req.extensions().get::<rio_auth::jwt::TenantClaims>() {
            Some(claims) => {
                // Same db-presence gate as tenant resolve. If db is
                // None AND Claims are Some, something is misconfigured
                // (JWT mode requires PG for revocation); fail loud.
                let db = self.db.as_ref().ok_or_else(|| {
                    Status::failed_precondition(
                        "jti revocation check requires database connection \
                         (JWT mode enabled but scheduler pool is None)",
                    )
                })?;
                if db
                    .is_jwt_revoked(&claims.jti)
                    .await
                    .status_internal("jti revocation lookup failed")?
                {
                    return Err(Status::unauthenticated("token revoked"));
                }
                Ok(Some((claims.sub, claims.jti.clone())))
            }
            None if self.jwt_mode => Err(Status::unauthenticated(
                "SchedulerService requires x-rio-tenant-token in JWT mode",
            )),
            None => Ok(None),
        }
    }

    // r[impl sec.executor.identity-token+2]
    /// Extract and verify `x-rio-executor-token`, returning the full
    /// HMAC-attested [`ExecutorClaims`]. Mirrors
    /// [`Self::require_tenant`] for the worker-facing service: when an
    /// HMAC key is configured, a missing or invalid token is
    /// `Unauthenticated`; when no key is configured (dev mode),
    /// `Ok(None)`.
    ///
    /// Called by `build_execution` (binds the stream to the intent the
    /// pod was spawned for) and `heartbeat` (binds the body's
    /// `intent_id` AND `kind` to the token's). A compromised builder
    /// holds a token for ITS OWN intent+kind only — it cannot mint one
    /// for another pod's, and cannot self-promote `kind` to receive
    /// work routed past its CNP airgap boundary.
    pub(super) fn require_executor<T>(
        &self,
        req: &Request<T>,
    ) -> Result<Option<ExecutorClaims>, Status> {
        let Some(key) = &self.hmac_key else {
            return Ok(None);
        };
        let token = req
            .metadata()
            .get(rio_proto::EXECUTOR_TOKEN_HEADER)
            .ok_or_else(|| {
                Status::unauthenticated(
                    "ExecutorService requires x-rio-executor-token when HMAC is configured",
                )
            })?
            .to_str()
            .map_err(|_| Status::unauthenticated("x-rio-executor-token: non-ASCII value"))?;
        let claims: ExecutorClaims = key.verify(token).map_err(|e| {
            Status::unauthenticated(format!("x-rio-executor-token verification failed: {e}"))
        })?;
        Ok(Some(claims))
    }
}

/// Resolve a tenant name to its UUID, mapping errors to gRPC `Status`.
/// Shared by `SubmitBuild` / `ResolveTenant` (here) and `ListBuilds`
/// (admin/mod.rs).
///
/// [`NormalizedName`] guarantees non-empty/trimmed at the type level —
/// the caller handles the empty-string → single-tenant-mode branch
/// *before* calling (see [`NormalizedName::from_maybe_empty`]). Unknown
/// name → `Status::invalid_argument`. PG error → `Status::internal`.
/// SQL lives in [`SchedulerDb::lookup_tenant_id`] — the gRPC layer
/// holds no inline queries.
pub(crate) async fn resolve_tenant_name(
    db: &SchedulerDb,
    name: &NormalizedName,
) -> Result<Uuid, Status> {
    db.lookup_tenant_id(name.as_str())
        .await
        .status_internal("tenant lookup failed")?
        .ok_or_else(|| Status::invalid_argument(format!("unknown tenant: {name}")))
}

/// Bridge a build's broadcast receivers into a tonic streaming response.
///
/// `first` is the snapshot-first attach message
/// (`r[sched.watch.snapshot-first]`): WatchBuild passes the
/// `BuildSnapshot` event computed atomically with the subscription;
/// SubmitBuild passes `None` (its receivers were registered before the
/// build's first event was emitted, so there is no missed state to
/// summarize). The bridge sends it before draining the broadcast — the
/// client always sees current-state-then-live-events, with no replay
/// and no dedup.
///
/// On `Lagged`, the bridge logs and CONTINUES (does not break or send
/// `DATA_LOSS`). Tokio's `RecvError::Lagged(n)` repositions the receiver
/// to the oldest in-ring event — it's still subscribed. Breaking here
/// drops the receiver → `receiver_count() == 0` → orphan-watcher
/// (`r[sched.backstop.orphan-watcher]`) starts the 5-min grace timer.
/// Under sustained event burst (large DAG initial dispatch) the gateway
/// can't drain fast enough and the bridge re-lags every reconnect, so
/// the receiver keeps dropping → orphan-watcher eventually cancels a
/// perfectly-watched build (I-144).
///
/// State and Log events arrive on separate broadcast rings
/// (`r[gw.activity.stop-parity]`). Log volume cannot lag the state ring;
/// state-channel `Lagged` should now be very rare (only initial dispatch
/// burst > 4096). Log-channel `Lagged` is expected under chatty parallel
/// builds and is debug-level: S3 + AdminService is the authoritative log
/// path. A *state-channel* terminal lost to `Lagged` is recovered by the
/// Closed → `EofWithoutTerminal` → WatchBuild reconnect → snapshot path
/// (the snapshot reports the terminal state directly).
pub(crate) fn bridge_build_events(
    task_name: &'static str,
    rx: BuildEventReceivers,
    first: Option<Box<rio_proto::types::BuildEvent>>,
) -> ReceiverStream<Result<rio_proto::types::BuildEvent, Status>> {
    enum StateOrLog<T> {
        State(T),
        Log(T),
    }
    let BuildEventReceivers {
        state: mut bcast,
        log: mut log_bcast,
    } = rx;
    let (tx, rx) = mpsc::channel(256);
    rio_common::task::spawn_monitored(task_name, async move {
        // Phase 1: the snapshot-first attach message (WatchBuild only).
        if let Some(first) = first
            && tx.send(Ok(*first)).await.is_err()
        {
            return; // client gone
        }

        // Phase 2: merge-drain state + log broadcast rings.
        //
        // `biased` toward state: under contention a state-transition
        // (Derivation/Completed) is forwarded before backlogged log
        // batches. This is a fairness preference, not the correctness
        // guarantee — the guarantee is the channel split itself.
        let mut log_closed = false;
        loop {
            let recv = tokio::select! {
                biased;
                r = bcast.recv() => StateOrLog::State(r),
                r = log_bcast.recv(), if !log_closed => StateOrLog::Log(r),
            };
            match recv {
                StateOrLog::State(Ok(event)) | StateOrLog::Log(Ok(event)) => {
                    if tx.send(Ok(event)).await.is_err() {
                        break; // client disconnected
                    }
                }
                StateOrLog::State(Err(broadcast::error::RecvError::Closed)) => break,
                StateOrLog::Log(Err(broadcast::error::RecvError::Closed)) => {
                    // Log channel closed (cleanup) but state still
                    // open — keep draining state. The arm guard above
                    // stops re-polling the closed log receiver.
                    log_closed = true;
                    continue;
                }
                StateOrLog::State(Err(broadcast::error::RecvError::Lagged(n))) => {
                    // I-144: do NOT break. Breaking drops `bcast` →
                    // `receiver_count() == 0` → orphan-watcher cancels
                    // the build after grace even though the gateway is
                    // still attached and would reconnect.
                    //
                    // Post-split this should be rare (only initial
                    // dispatch burst > BUILD_EVENT_BUFFER_SIZE). The
                    // receiver is still valid (tokio repositions to
                    // oldest in-ring). A missed terminal surfaces via
                    // Closed → EofWithoutTerminal → WatchBuild
                    // reconnect → snapshot (which reports the terminal
                    // state directly); a missed per-drv Completed
                    // surfaces via the gateway's act.drv terminal-drain.
                    warn!(
                        task = task_name,
                        skipped = n,
                        "state-event subscriber lagged; {n} events skipped, continuing"
                    );
                    metrics::counter!("rio_scheduler_broadcast_lagged_total", "kind" => "state")
                        .increment(n);
                    continue;
                }
                StateOrLog::Log(Err(broadcast::error::RecvError::Lagged(n))) => {
                    // Expected under chatty parallel builds. Log loss
                    // is acceptable — S3 + AdminService is
                    // authoritative; this is the live convenience
                    // tail. debug-level so chatty builds don't spam
                    // the scheduler log.
                    tracing::debug!(
                        task = task_name,
                        skipped = n,
                        "log-event subscriber lagged; {n} log batches skipped"
                    );
                    metrics::counter!("rio_scheduler_broadcast_lagged_total", "kind" => "log")
                        .increment(n);
                    continue;
                }
            }
        }
    });
    ReceiverStream::new(rx)
}

// ---------------------------------------------------------------------------
// BuildExecution bidirectional stream e2e
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
