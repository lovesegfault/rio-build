//! gRPC service implementations for SchedulerService and WorkerService.
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
// r[impl proto.stream.bidi]

pub(crate) mod actor_guards;
mod scheduler_service;
mod worker_service;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::sync::broadcast;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tonic::Status;
use tracing::warn;
use uuid::Uuid;

use rio_common::tenant::NormalizedName;

use crate::actor::{ActorCommand, ActorError, ActorHandle};
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
    /// PG pool for WatchBuild's event-log replay. `Option` so
    /// `new_for_tests` can skip it (None → broadcast-only, no
    /// replay). Production always sets it — main.rs already has
    /// the pool for the DB handle.
    pub(super) pool: Option<sqlx::PgPool>,
    /// Shared with the lease loop. When false (standby), all
    /// handlers return UNAVAILABLE immediately — clients with
    /// a health-aware balanced channel route to the leader
    /// instead. Tests default to `true` (always-leader).
    is_leader: Arc<AtomicBool>,
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
            pool: None,
            is_leader: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Test constructor with a PG pool for tenant-resolution tests.
    /// Like `new_for_tests` but with a pool so `resolve_tenant` works.
    #[cfg(test)]
    pub fn new_for_tests_with_pool(actor: ActorHandle, pool: sqlx::PgPool) -> Self {
        Self {
            actor,
            log_buffers: Arc::new(LogBuffers::new()),
            pool: Some(pool),
            is_leader: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Create with an externally-owned `LogBuffers`. Production `main.rs`
    /// uses this so the LogFlusher (separate task) drains the SAME buffers
    /// that the BuildExecution recv task writes to.
    ///
    /// `pool`: for WatchBuild's PG event-log replay. main.rs already
    /// has it (same pool as `SchedulerDb`).
    pub fn with_log_buffers(
        actor: ActorHandle,
        log_buffers: Arc<LogBuffers>,
        pool: sqlx::PgPool,
        is_leader: Arc<AtomicBool>,
    ) -> Self {
        Self {
            actor,
            log_buffers,
            pool: Some(pool),
            is_leader,
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
    pub(crate) fn actor_error_to_status(err: ActorError) -> Status {
        match err {
            ActorError::BuildNotFound(id) => Status::not_found(format!("build not found: {id}")),
            ActorError::Backpressure => {
                Status::resource_exhausted("scheduler is overloaded, please retry later")
            }
            // ChannelSend = actor's mpsc receiver dropped. Either the
            // actor panicked OR it exited on its shutdown-token arm
            // during drain. UNAVAILABLE (retriable) not INTERNAL —
            // BalancedChannel clients retry on the next replica; with
            // INTERNAL they'd surface the error to the user. Same
            // string as `actor_guards::check_actor_alive` so operators
            // grep for one signature, not two.
            ActorError::ChannelSend => {
                Status::unavailable("scheduler actor is unavailable (panicked or exited)")
            }
            ActorError::Database(e) => Status::internal(format!("database error: {e}")),
            ActorError::Dag(e) => Status::internal(format!("DAG merge failed: {e}")),
            ActorError::MissingDbId { .. } => Status::internal(err.to_string()),
            // UNAVAILABLE — gateway/client sees this as a retriable error.
            // They should back off and retry; the breaker auto-closes in 30s
            // or on the next successful probe.
            ActorError::StoreUnavailable => Status::unavailable(
                "store service is unreachable; cache-check circuit breaker is open",
            ),
        }
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
    pub(crate) fn parse_build_id(s: &str) -> Result<Uuid, Status> {
        s.parse()
            .map_err(|_| Status::invalid_argument("invalid build_id UUID"))
    }
}

/// Resolve a tenant name to its UUID, mapping errors to gRPC `Status`.
/// Shared by `SubmitBuild` / `ResolveTenant` (here) and `ListBuilds`
/// (admin/mod.rs).
///
/// The gateway sends the tenant NAME (from the `authorized_keys` entry's
/// comment field); the scheduler resolves it here. [`NormalizedName`]
/// guarantees non-empty/trimmed at the type level — the caller handles
/// the empty-string → single-tenant-mode branch *before* calling (see
/// [`NormalizedName::from_maybe_empty`]). Unknown name →
/// `Status::invalid_argument`. PG error → `Status::internal`.
pub(crate) async fn resolve_tenant_name(
    pool: &sqlx::PgPool,
    name: &NormalizedName,
) -> Result<Uuid, Status> {
    sqlx::query_scalar("SELECT tenant_id FROM tenants WHERE tenant_name = $1")
        .bind(name.as_str())
        .fetch_optional(pool)
        .await
        .map_err(|e| Status::internal(format!("tenant lookup failed: {e}")))?
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
/// On `Lagged`, sends `DATA_LOSS` so the client fails cleanly instead of
/// silently hanging on a missed terminal event. Lagged means we permanently
/// missed n events; if `BuildCompleted` was among them the client would hang
/// forever waiting for a terminal event that will never arrive.
pub(crate) fn bridge_build_events(
    task_name: &'static str,
    mut bcast: broadcast::Receiver<rio_proto::types::BuildEvent>,
    replay: Option<EventReplay>,
) -> ReceiverStream<Result<rio_proto::types::BuildEvent, Status>> {
    let (tx, rx) = mpsc::channel(256);
    rio_common::task::spawn_monitored(task_name, async move {
        // Phase 1: PG replay. Best-effort — on error, fall through.
        // `dedup_watermark` starts at 0 (no dedup) and is raised to
        // last_seq ONLY if replay succeeds. On PG failure we DON'T
        // dedup — the broadcast ring might have events we'd otherwise
        // skip, and a double is better than a hole.
        let mut dedup_watermark = 0u64;
        if let Some(r) = replay {
            match crate::db::read_event_log(&r.pool, r.build_id, r.since, r.last_seq).await {
                Ok(rows) => {
                    // Replay succeeded (even if empty — the
                    // persister may have dropped some under
                    // backpressure, but we know nothing NEW is
                    // coming ≤ last_seq). Safe to dedup.
                    dedup_watermark = r.last_seq;
                    for (seq, bytes) in rows {
                        use prost::Message;
                        match rio_proto::types::BuildEvent::decode(&bytes[..]) {
                            Ok(event) => {
                                if tx.send(Ok(event)).await.is_err() {
                                    return; // client gone
                                }
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
                }
                Err(e) => {
                    // PG unreachable. Fall through to broadcast-only.
                    // handle_watch_build's terminal re-send covers
                    // the "build already done" case. Non-terminal
                    // builds: the gateway misses some history but
                    // sees live events. Degraded, not dead.
                    warn!(
                        build_id = %r.build_id,
                        since = r.since,
                        last_seq = r.last_seq,
                        error = %e,
                        "event-log replay failed (PG unreachable?); \
                         falling through to broadcast-only, gap possible"
                    );
                }
            }
        }

        // Phase 2: broadcast drain.
        loop {
            match bcast.recv().await {
                Ok(event) => {
                    // Dedup: PG already delivered seq ≤ watermark.
                    // The broadcast ring (1024 cap) holds recent
                    // events — some were emitted BEFORE subscribe
                    // and have seq ≤ last_seq. Skip those.
                    if event.sequence <= dedup_watermark {
                        continue;
                    }
                    if tx.send(Ok(event)).await.is_err() {
                        break; // client disconnected
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(
                        lagged = n,
                        "build event subscriber lagged, some events lost"
                    );
                    let _ = tx
                        .send(Err(Status::data_loss(format!(
                            "missed {n} build events; re-subscribe via WatchBuild"
                        ))))
                        .await;
                    break;
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
