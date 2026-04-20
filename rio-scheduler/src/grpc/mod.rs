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
use std::sync::atomic::{AtomicBool, AtomicU64};

use futures_util::StreamExt;
use tokio::sync::broadcast;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Status};
use tracing::warn;
use uuid::Uuid;

use rio_auth::hmac::{ExecutorClaims, HmacKey};
use rio_common::grpc::StatusExt;
use rio_common::tenant::NormalizedName;

use crate::actor::{ActorCommand, ActorError, ActorHandle};
use crate::db::SchedulerDb;
use crate::logs::LogBuffers;

/// Shared scheduler state passed to gRPC handlers.
#[derive(Clone)]
pub struct SchedulerGrpc {
    // Fields `pub(super)` so the per-service submodules
    // (scheduler_service.rs, worker_service.rs) can read them
    // directly. Inherent-impl methods on `SchedulerGrpc` defined
    // in a child module can't reach private fields of the parent
    // module's struct.
    pub(super) actor: ActorHandle,
    /// Per-derivation log ring buffers. Written directly by the
    /// BuildExecution recv task (bypasses the actor), read by
    /// AdminService.GetBuildLogs and drained by the S3 flusher on
    /// completion. `Arc` because `SchedulerGrpc` is `Clone`d per-connection
    /// and all handlers + the spawned recv tasks need the same buffers.
    pub(super) log_buffers: Arc<LogBuffers>,
    /// DB handle for tenant resolve / jti revocation / WatchBuild
    /// event-log replay. `Option` so `new_for_tests` can skip it
    /// (None → broadcast-only, no replay, no tenant resolve).
    /// Production always sets it — main.rs constructs the same
    /// `SchedulerDb` for the actor. Holds `SchedulerDb` (not bare
    /// `PgPool`) so all SQL goes through the `db/` module; raw pool
    /// is reachable via [`SchedulerDb::pool`] where the event-log
    /// free fn needs it.
    pub(super) db: Option<SchedulerDb>,
    /// Shared with the lease loop. When false (standby), all
    /// handlers return UNAVAILABLE immediately — clients with
    /// a health-aware balanced channel route to the leader
    /// instead. Tests default to `true` (always-leader).
    is_leader: Arc<AtomicBool>,
    /// Shared with the lease loop. The `worker-stream-reader` loop
    /// captures this at stream-open and breaks if it changes —
    /// generation-fences open worker streams so an ex-leader doesn't
    /// forward `ProcessCompletion` for a generation it no longer owns
    /// (r[sched.lease.standby-drops-writes]). Tests default to `1`.
    pub(super) generation: Arc<AtomicU64>,
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
}

impl SchedulerGrpc {
    /// Test-only constructor: makes a FRESH `LogBuffers`, not shared with
    /// any flusher. Production MUST use [`with_log_buffers`](Self::with_log_buffers)
    /// — a fresh DashMap means the flusher drains an unrelated empty buffer
    /// forever while real logs pile up here (silent total log loss).
    ///
    /// `#[cfg(test)]` makes prod misuse a compile error, not a runtime
    /// footgun.
    #[cfg(test)]
    pub fn new_for_tests(actor: ActorHandle) -> Self {
        Self {
            actor,
            log_buffers: Arc::new(LogBuffers::new()),
            db: None,
            is_leader: Arc::new(AtomicBool::new(true)),
            generation: Arc::new(AtomicU64::new(1)),
            jwt_mode: false,
            hmac_key: None,
        }
    }

    /// Test constructor with a PG pool for tenant-resolution tests.
    /// Like `new_for_tests` but with a pool so `resolve_tenant` works.
    #[cfg(test)]
    pub fn new_for_tests_with_pool(actor: ActorHandle, pool: sqlx::PgPool) -> Self {
        Self {
            actor,
            log_buffers: Arc::new(LogBuffers::new()),
            db: Some(SchedulerDb::new(pool)),
            is_leader: Arc::new(AtomicBool::new(true)),
            generation: Arc::new(AtomicU64::new(1)),
            jwt_mode: false,
            hmac_key: None,
        }
    }

    /// Create with an externally-owned `LogBuffers`. Production `main.rs`
    /// uses this so the LogFlusher (separate task) drains the SAME buffers
    /// that the BuildExecution recv task writes to.
    ///
    /// `pool`: for WatchBuild's PG event-log replay. main.rs already
    /// has it (same pool as `SchedulerDb`).
    ///
    /// `jwt_mode`: whether a JWT pubkey is configured (drives
    /// `require_tenant`). `hmac_key`: assignment-HMAC key, reused as
    /// the executor-identity verifier (drives `require_executor`).
    pub fn with_log_buffers(
        actor: ActorHandle,
        log_buffers: Arc<LogBuffers>,
        db: SchedulerDb,
        is_leader: Arc<AtomicBool>,
        generation: Arc<AtomicU64>,
        jwt_mode: bool,
        hmac_key: Option<Arc<HmacKey>>,
    ) -> Self {
        Self {
            actor,
            log_buffers,
            db: Some(db),
            is_leader,
            generation,
            jwt_mode,
            hmac_key,
        }
    }

    /// Access the shared log ring buffers. Exposed for `AdminService`.
    pub fn log_buffers(&self) -> Arc<LogBuffers> {
        Arc::clone(&self.log_buffers)
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

/// Parameters for PG-backed event replay on WatchBuild.
///
/// Set only when `since < last_seq` (gap exists) AND a pool is
/// available. SubmitBuild never sets this: fresh build, last_seq=0,
/// no gap possible.
pub(crate) struct EventReplay {
    pub pool: sqlx::PgPool,
    pub build_id: Uuid,
    /// Gateway's last-seen sequence. PG replay lower bound (exclusive).
    pub since: u64,
    /// Actor's last-emitted sequence at subscribe time. PG replay
    /// upper bound (inclusive) AND broadcast dedup watermark.
    pub last_seq: u64,
}

/// Bridge a `broadcast::Receiver<BuildEvent>` into a tonic streaming response.
///
/// Two-phase when `replay` is set:
///   1. PG replay: stream `WHERE seq > since AND seq <= last_seq`
///      from `build_event_log`. Closes the gap between what the
///      gateway saw before disconnect and what the new subscribe
///      will carry. Best-effort — if PG is down, fall through to
///      broadcast-only (the terminal-event re-send in
///      handle_watch_build is the safety net).
///   2. Broadcast drain with dedup: skip `seq <= last_seq`. Those
///      may ALSO be in the broadcast ring (1024-event buffer), and
///      PG already delivered them in phase 1. Without dedup the
///      gateway sees events twice.
///
/// `replay = None` → pure broadcast drain, no dedup (SubmitBuild).
///
/// On `Lagged`, the bridge logs and CONTINUES (does not break or send
/// `DATA_LOSS`). Tokio's `RecvError::Lagged(n)` repositions the receiver
/// to the oldest in-ring event — it's still subscribed. Breaking here
/// drops the receiver → `receiver_count() == 0` → orphan-watcher
/// (`r[sched.backstop.orphan-watcher]`) starts the 5-min grace timer.
/// Under sustained event burst (large DAG, many concurrent drvs emitting
/// Log lines) the gateway can't drain fast enough and the bridge re-lags
/// every reconnect, so the receiver keeps dropping → orphan-watcher
/// eventually cancels a perfectly-watched build (I-144).
///
/// The gap is acceptable: Log events are recoverable via S3 (LogFlusher);
/// Derivation/Progress events are UX-only. A terminal event lost in the
/// gap is recovered by the Closed → `EofWithoutTerminal` → WatchBuild
/// reconnect → `handle_watch_build` terminal-resend path (≤60s delay
/// from `TERMINAL_CLEANUP_DELAY`).
pub(crate) fn bridge_build_events(
    task_name: &'static str,
    mut bcast: broadcast::Receiver<rio_proto::types::BuildEvent>,
    replay: Option<EventReplay>,
) -> ReceiverStream<Result<rio_proto::types::BuildEvent, Status>> {
    let (tx, rx) = mpsc::channel(256);
    rio_common::task::spawn_monitored(task_name, async move {
        // Phase 1: PG replay. Best-effort — on error, fall through.
        // `dedup_watermark` starts at 0 (no dedup) and is raised to
        // the highest seq PG ACTUALLY RETURNED — NOT `r.last_seq`. The
        // persister's `try_send` drops under backpressure (event.rs
        // `Full` arm), so PG can return `Ok` with rows missing at the
        // tail. `handle_watch_build`'s safety-net terminal resend goes
        // out at `seq = last_seq` post-subscribe; if we deduped at
        // `last_seq` and PG never delivered it, that resend is
        // suppressed and the gateway loops `EofWithoutTerminal`
        // forever. On PG failure mid-stream we keep whatever the
        // watermark reached — the broadcast ring might have events
        // we'd otherwise skip, and a double is better than a hole.
        let mut dedup_watermark = 0u64;
        if let Some(r) = replay {
            // Row stream (not Vec): a fresh `since=0` watch on a
            // 153k-node DAG would otherwise materialize ≥300k rows ×
            // ~400B per concurrent watcher BEFORE the mpsc(256)
            // backpressure can apply. Forwarding row-by-row lets the
            // send `.await` propagate backpressure to the PG cursor.
            let mut rows = std::pin::pin!(crate::db::read_event_log(
                &r.pool, r.build_id, r.since, r.last_seq
            ));
            let mut max_seen = r.since;
            let mut errored = false;
            while let Some(row) = rows.next().await {
                match row {
                    Ok((seq, bytes)) => {
                        use prost::Message;
                        match rio_proto::types::BuildEvent::decode(&bytes[..]) {
                            Ok(event) => {
                                if tx.send(Ok(event)).await.is_err() {
                                    return; // client gone
                                }
                                max_seen = seq;
                            }
                            Err(e) => {
                                // Corrupt row — written by us, so
                                // this is a bug. Skip it; the seq
                                // gap is visible to the client.
                                warn!(
                                    build_id = %r.build_id,
                                    seq,
                                    error = %e,
                                    "event-log replay: prost decode failed (corrupt row, skipping)"
                                );
                            }
                        }
                    }
                    Err(e) => {
                        // PG unreachable / cursor error. Fall through
                        // to broadcast-only. handle_watch_build's
                        // terminal re-send covers the "build already
                        // done" case. Non-terminal builds: the gateway
                        // misses some history but sees live events.
                        // Degraded, not dead.
                        warn!(
                            build_id = %r.build_id,
                            since = r.since,
                            last_seq = r.last_seq,
                            error = %e,
                            "event-log replay failed (PG unreachable?); \
                             falling through to broadcast-only, gap possible"
                        );
                        errored = true;
                        break;
                    }
                }
            }
            // Watermark = highest seq actually forwarded. If PG
            // errored before yielding ANY row, leave it at 0 (same
            // semantics as before: no dedup on PG failure). If PG
            // yielded a partial prefix then errored, dedup that
            // prefix — those rows were delivered.
            if !errored || max_seen > r.since {
                dedup_watermark = max_seen;
            }
        }

        // Phase 2: broadcast drain.
        loop {
            match bcast.recv().await {
                Ok(event) => {
                    use rio_proto::types::build_event::Event;
                    // Dedup: PG already delivered seq ≤ watermark.
                    // The broadcast ring (1024 cap) holds recent
                    // events — some were emitted BEFORE subscribe
                    // and have seq ≤ last_seq. Skip those.
                    //
                    // Log is EXEMPT: it is never persisted to PG
                    // (event.rs filters it from the persister) AND
                    // reuses the last persisted seq without bumping
                    // (event.rs Log arm). After a reconnect-with-
                    // replay, `dedup_watermark = last_seq` and every
                    // live Log line arrives at `seq = last_seq` →
                    // would be dropped here until the next non-Log
                    // event bumps the counter — an unbounded live-log
                    // blackout for a single long-running drv. Log
                    // can never be a Phase-1 duplicate (PG never had
                    // it), so skipping the check is safe.
                    if event.sequence <= dedup_watermark
                        && !matches!(event.event, Some(Event::Log(_)))
                    {
                        continue;
                    }
                    if tx.send(Ok(event)).await.is_err() {
                        break; // client disconnected
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    // I-144: do NOT break. Breaking drops `bcast` →
                    // `receiver_count() == 0` → orphan-watcher cancels
                    // the build after grace even though the gateway is
                    // still attached and would reconnect. Under burst
                    // (large DAG initial dispatch, or hundreds of drvs
                    // emitting Log lines) the gateway can't keep up and
                    // re-lags on every reconnect — receiver_count stays
                    // 0 long enough for orphan-cancel.
                    //
                    // The receiver is still valid post-Lagged (tokio
                    // repositions it to the oldest in-ring event). The
                    // gap is acceptable: Log recoverable via S3; a
                    // missed terminal event surfaces via Closed →
                    // EofWithoutTerminal → WatchBuild reconnect →
                    // handle_watch_build terminal-resend.
                    warn!(
                        lagged = n,
                        "build event subscriber lagged; {n} events skipped, continuing"
                    );
                    metrics::counter!("rio_scheduler_broadcast_lagged_total").increment(n);
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
