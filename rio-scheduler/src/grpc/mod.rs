//! gRPC service implementations for SchedulerService and ExecutorService.
//!
//! Both services run in the same scheduler binary. They communicate with the
//! DAG actor via the `ActorHandle`.
//!
//! The actual `impl` blocks live in submodules — `scheduler_service`
//! (client-facing RPCs) and `executor_service` (the pull-protocol
//! unaries). This file holds only the shared [`SchedulerGrpc`] state
//! struct, constructors, and common helpers. Split from a single 1087L
//! file (P0356) after collision count hit 33 — SubmitBuild/WatchBuild
//! changes no longer conflict with executor-surface changes.

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

/// The caller's attested tenant identity — the typed witness every
/// tenant-scoped read of durable build state requires.
///
/// Sole producer: [`SchedulerGrpc::require_tenant`] (the
/// interceptor-reconciling chokepoint: jwt-mode rejection + jti
/// revocation). A handler path that fetches tenant-scoped rows without
/// passing through the gate does not typecheck — `bug_213` was exactly
/// such a path (the `WatchBuild` terminal-row fallback fetched the
/// settled verdict with no tenant bind, streaming it to foreign
/// tenants post-cleanup).
///
/// `tenant() == None` is the dev-mode posture (no JWT pubkey
/// configured, no claims attached): tenant-scoped queries bind NULL
/// and deliberately match every row.
// r[impl sched.tenant.authz+3]
#[derive(Debug, Clone)]
pub(crate) struct CallerTenant {
    /// `Some((claims.sub, claims.jti))` — attested + revocation-checked.
    tenant: Option<(Uuid, String)>,
}

impl CallerTenant {
    /// The attested tenant id (`claims.sub`); `None` in dev mode.
    pub(crate) fn tenant(&self) -> Option<Uuid> {
        self.tenant.as_ref().map(|(sub, _)| *sub)
    }

    /// The revocation-checked token id, kept for the `builds.jwt_jti`
    /// audit insert (`r[gw.jwt.issue]`).
    pub(crate) fn jti(&self) -> Option<&str> {
        self.tenant.as_ref().map(|(_, jti)| jti.as_str())
    }
}

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
    /// [`Self::require_executor`] rejects `ExecutorService` unaries
    /// without a valid `x-rio-executor-token`.
    pub(super) hmac_key: Option<Arc<HmacKey>>,
    /// Service-HMAC verifier (the `x-rio-service-token` family — the
    /// SEPARATE key the admin surfaces verify). Verifies the store's
    /// kind-attested materialization credential
    /// (`ServiceClaims { caller: "rio-store" }`) on the
    /// materialization-only ExecutorService operations
    /// (ListMaterializationJobs, kind=MATERIALIZATION PullAssignment,
    /// materialization ReportOutcome, ReportMaterializationProgress).
    /// `None` + `hmac_key: None` = full dev mode (open); `None` +
    /// `hmac_key: Some` = closed for materialization operations (no
    /// acceptable credential exists).
    pub(super) service_verifier: Option<Arc<HmacKey>>,
    /// sh-036.1: store client + breaker mirror for the off-actor
    /// `FindMissingPaths` probe in `submit_build`
    /// (`scheduler_service.rs`). Bundled so a test can't set one half
    /// of the feature without the other.
    pub(super) off_actor_probe: OffActorProbe,
}

/// sh-036.1 off-actor `FindMissingPaths` probe wiring — store client
/// plus the actor's `cache_breaker.is_open()` mirror.
///
/// Both halves are sourced from the same locals in `main.rs`, both fed
/// into `DagActorPlumbing` adjacently, and both `None`/`Arc::default()`
/// together in test constructors. Bundling them structurally couples
/// the two halves and keeps `SchedulerGrpc::new` under the
/// too-many-arguments threshold.
#[derive(Clone, Default)]
pub struct OffActorProbe {
    /// Store client — same lazy channel as the actor's
    /// (`tonic::Channel` is cheap-clone). `None` in test constructors
    /// → handler threads `precomputed_probe = None` and the actor's
    /// in-actor probe fallback runs (today's behaviour).
    pub store_client: Option<rio_proto::StoreServiceClient<tonic::transport::Channel>>,
    /// Read-only mirror of the actor's `cache_breaker.is_open()`,
    /// shared via `DagActorPlumbing`. The off-actor FMP probe
    /// replicates `find_missing_with_breaker`'s conditional timeout:
    /// `if breaker_open {grpc_timeout} else {MERGE_FMP_TIMEOUT}`. The
    /// breaker FOLD stays actor-side.
    pub breaker_open: Arc<AtomicBool>,
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
            off_actor_probe: OffActorProbe::default(),
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
            off_actor_probe: OffActorProbe::default(),
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
    ///
    /// `off_actor_probe`: sh-036.1 off-actor FMP probe — same lazy
    /// store channel as the actor's, plus the actor's
    /// `cache_breaker.is_open()` mirror for the conditional timeout.
    pub fn new(
        actor: ActorHandle,
        db: SchedulerDb,
        is_leader: Arc<AtomicBool>,
        jwt_mode: bool,
        hmac_key: Option<Arc<HmacKey>>,
        service_verifier: Option<Arc<HmacKey>>,
        off_actor_probe: OffActorProbe,
    ) -> Self {
        Self {
            actor,
            db: Some(db),
            is_leader,
            jwt_mode,
            hmac_key,
            service_verifier,
            off_actor_probe,
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

    // r[impl sched.tenant.authz+3]
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
    pub(super) async fn require_tenant<T>(&self, req: &Request<T>) -> Result<CallerTenant, Status> {
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
                Ok(CallerTenant {
                    tenant: Some((claims.sub, claims.jti.clone())),
                })
            }
            None if self.jwt_mode => Err(Status::unauthenticated(
                "SchedulerService requires x-rio-tenant-token in JWT mode",
            )),
            None => Ok(CallerTenant { tenant: None }),
        }
    }

    // r[impl sec.executor.identity-token+3]
    /// Extract and verify `x-rio-executor-token`, returning the full
    /// HMAC-attested identity ([`VerifiedExecutor`] — claims + the
    /// fence key minted from the bytes that verified). Mirrors
    /// [`Self::require_tenant`] for the worker-facing service: when an
    /// HMAC key is configured, a missing or invalid token is
    /// `Unauthenticated`; when no key is configured (dev mode),
    /// `Ok(None)`.
    ///
    /// The Err carries the typed [`ExecutorAuthDetail`] beside the
    /// wire status (E1): this is the KNOWING arm — the header probe
    /// and the HMAC verify are where absent/malformed/bad-signature/
    /// expired are distinguishable — and the detail must survive the
    /// module boundary typed, because the terminal-rejection decision
    /// (and therefore the counting site) lives in `credential_for`,
    /// where a metadata-carrier failure may still be recovered by a
    /// verifying body token. Counting here would tally recovered
    /// carrier failures as rejections.
    ///
    /// Called per-unary by the `ExecutorService` handlers
    /// (`PullAssignment` binds the body's `intent_id` AND `kind` to
    /// the token's; `ReportOutcome` verifies the same identity before
    /// consuming the attempt). A compromised builder
    /// holds a token for ITS OWN intent+kind only — it cannot mint one
    /// for another pod's, and cannot self-promote `kind` to receive
    /// work routed past its CNP airgap boundary.
    pub(super) fn require_executor<T>(
        &self,
        req: &Request<T>,
    ) -> Result<Option<VerifiedExecutor>, ExecutorAuthFailure> {
        let Some(key) = &self.hmac_key else {
            return Ok(None);
        };
        let Some(value) = req.metadata().get(rio_proto::EXECUTOR_TOKEN_HEADER) else {
            return Err(ExecutorAuthFailure {
                detail: ExecutorAuthDetail::Absent,
                status: Status::unauthenticated(
                    "ExecutorService requires x-rio-executor-token when HMAC is configured",
                ),
            });
        };
        let token = value.to_str().map_err(|_| ExecutorAuthFailure {
            detail: ExecutorAuthDetail::Malformed,
            status: Status::unauthenticated("x-rio-executor-token: non-ASCII value"),
        })?;
        let claims: ExecutorClaims = key.verify(token).map_err(|e| ExecutorAuthFailure {
            detail: ExecutorAuthDetail::from_hmac_error(&e),
            status: Status::unauthenticated(format!(
                "x-rio-executor-token verification failed: {e}"
            )),
        })?;
        // r[impl sched.executor.confirm-fence]
        // Verification-success site 1 of 2: the fence key is minted
        // from EXACTLY the bytes the HMAC check just accepted.
        Ok(Some(VerifiedExecutor {
            claims,
            fence_key: ConfirmFenceKey::from_verified_carrier(token.as_bytes()),
        }))
    }
}

/// E1: WHICH executor-credential fault the knowing arm observed —
/// minted exactly where the distinction exists (the header probe and
/// the HMAC verify), before everything merges into the wire's single
/// `Unauthenticated`. Observability metadata on the SAME judgment:
/// no consumer re-adjudicates from this value; the wire contract is
/// untouched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ExecutorAuthDetail {
    /// No executor credential presented on any carrier.
    Absent,
    /// A credential was presented but is not a decodable token:
    /// non-ASCII metadata bytes, wrong `.`-part count, base64 or
    /// JSON decode failure.
    Malformed,
    /// The token decodes but its HMAC tag does not verify — tampered
    /// bytes or a wrong/rotated key.
    BadSignature,
    /// Signature valid; the expiry claim is in the past. The
    /// clock-before-epoch refusal folds here too: when the time axis
    /// cannot be read the token cannot be proven within lifetime, so
    /// verification fails closed on the same axis.
    Expired,
}

impl ExecutorAuthDetail {
    pub(super) fn as_label(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Malformed => "malformed",
            Self::BadSignature => "bad_signature",
            Self::Expired => "expired",
        }
    }

    /// THE one fold of [`rio_auth::hmac::HmacError`] into the detail
    /// alphabet — exhaustive (no wildcard arm), so an HmacError
    /// variant addition is a compile error here, not a silent
    /// mis-label.
    pub(super) fn from_hmac_error(e: &rio_auth::hmac::HmacError) -> Self {
        use rio_auth::hmac::HmacError as E;
        match e {
            E::Format(_) | E::Base64(_) | E::Json(_) => Self::Malformed,
            E::InvalidSignature => Self::BadSignature,
            E::Expired { .. } | E::Clock(_) => Self::Expired,
            // Load-time variants: `verify()` never constructs them
            // (they belong to `HmacKey::load`). Premise-asserted and
            // classified malformed for totality.
            E::Io { .. } | E::EmptyKey => {
                debug_assert!(
                    false,
                    "load-time HmacError variant reached verify-time fold"
                );
                Self::Malformed
            }
        }
    }
}

/// A failed executor-credential verification: the wire [`Status`]
/// plus the typed detail, produced together at the knowing arm so no
/// later site can re-derive (and mis-derive) the reason from the
/// status string. The counting decision is NOT made here — see
/// `credential_for`: only a terminal rejection counts.
pub(super) struct ExecutorAuthFailure {
    pub(super) detail: ExecutorAuthDetail,
    pub(super) status: Status,
}

/// The confirm-fence key: SHA-256 hex of EXACTLY the carrier bytes
/// that verified (merged_bug_078). Provenance type — the field is
/// private and the only constructor is [`ConfirmFenceKey::from_verified_carrier`],
/// called at the two verification-success sites
/// ([`SchedulerGrpc::require_executor`]'s metadata arm and
/// `credential_for`'s body-verification arm), so a handler cannot
/// re-derive a key from raw request carriers: garbage metadata + a
/// valid body token now keys on the BODY hash — the carrier that
/// authenticated — closing the de-key-the-fence / dodge-the-screen
/// cell. Disclosed boundary: the `ActorCommand::PullAssignment.
/// executor_token_sha256` conduit is a plain `Option<String>`
/// ([`ConfirmFenceKey::into_wire`]) — raw carriers exist ONLY at the
/// gRPC layer, so verifier-only minting covers every derivation site
/// and the actor receives hash-or-nothing.
pub(crate) struct ConfirmFenceKey(String);

impl ConfirmFenceKey {
    /// Mint from the carrier bytes that JUST verified. Deliberately
    /// `pub(in crate::grpc)`: only the credential layer (the one
    /// layer that sees raw carriers) can construct a key, and only
    /// at its verification-success sites.
    pub(in crate::grpc) fn from_verified_carrier(bytes: &[u8]) -> Self {
        use sha2::Digest as _;
        Self(hex::encode(sha2::Sha256::digest(bytes)))
    }

    /// Surrender the hex hash for the actor command conduit (the
    /// disclosed `Option<String>` boundary). Consuming `self` keeps
    /// the one-key-one-command shape explicit.
    pub(in crate::grpc) fn into_wire(self) -> String {
        self.0
    }
}

/// The one value coupling executor identity and fence key
/// (merged_bug_078's keystone): both halves come from the SAME
/// verification event, so they cannot diverge again — there is no
/// way to authenticate on one carrier and fence on another.
pub(crate) struct VerifiedExecutor {
    /// The HMAC-attested claims (intent binding + kind).
    pub(in crate::grpc) claims: ExecutorClaims,
    /// The fence key minted from the bytes that verified.
    pub(in crate::grpc) fence_key: ConfirmFenceKey,
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
/// path.
///
/// A state-channel `Lagged` gap is healed STRUCTURALLY: the bridge sends
/// one in-stream `ResyncRequired` per Lagged streak (re-armed by the
/// next successfully forwarded state event), and the gateway re-attaches
/// via a fresh `WatchBuild` whose snapshot reconcile recovers ALL display
/// state (`r[gw.resync.loss-signal+1]`) — no per-event-type gap
/// compensation anywhere. Terminals stay covered even without the
/// signal: snapshot while resident, durable builds row after cleanup
/// (`r[sched.watch.terminal-from-durable-row]`).
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
        // One ResyncRequired per state-Lagged streak: armed at attach,
        // re-armed by every successfully forwarded STATE event,
        // disarmed when the signal is sent. The gateway resyncs from
        // one fresh snapshot however many events the streak dropped.
        let mut resync_armed = true;
        loop {
            let recv = tokio::select! {
                biased;
                r = bcast.recv() => StateOrLog::State(r),
                r = log_bcast.recv(), if !log_closed => StateOrLog::Log(r),
            };
            match recv {
                StateOrLog::State(Ok(event)) => {
                    if tx.send(Ok(event)).await.is_err() {
                        break; // client disconnected
                    }
                    // A state event reached the consumer: a future
                    // Lagged is a NEW streak.
                    resync_armed = true;
                }
                StateOrLog::Log(Ok(event)) => {
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
                    // r[impl gw.resync.loss-signal+1]
                    // Tell THIS watcher its event stream has a gap:
                    // one in-stream ResyncRequired per streak. The
                    // gateway re-attaches with zero backoff and
                    // recovers everything from the snapshot reconcile
                    // — the bridge never enumerates which event types
                    // the gap may have contained. build_id is left
                    // empty: the stream itself is the build identity,
                    // and the synthesized signal has no emitter row.
                    if resync_armed {
                        resync_armed = false;
                        let signal = rio_proto::types::BuildEvent {
                            build_id: String::new(),
                            timestamp: None,
                            event: Some(rio_proto::types::build_event::Event::ResyncRequired(
                                rio_proto::types::ResyncRequired {},
                            )),
                        };
                        if tx.send(Ok(signal)).await.is_err() {
                            break; // client disconnected
                        }
                    }
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
// ExecutorService e2e
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
