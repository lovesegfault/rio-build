//! gRPC service implementations for SchedulerService and WorkerService.
//!
//! Both services run in the same scheduler binary. They communicate with the
//! DAG actor via the `ActorHandle`.
// r[impl proto.stream.bidi]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::broadcast;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::{info, instrument, warn};
use uuid::Uuid;

use rio_proto::SchedulerService;
use rio_proto::WorkerService;

use crate::actor::{ActorCommand, ActorError, ActorHandle, MergeDagRequest};
use crate::logs::LogBuffers;
use crate::state::BuildOptions;

/// Shared scheduler state passed to gRPC handlers.
#[derive(Clone)]
pub struct SchedulerGrpc {
    actor: ActorHandle,
    /// Per-derivation log ring buffers. Written directly by the
    /// BuildExecution recv task (bypasses the actor), read by
    /// AdminService.GetBuildLogs and drained by the S3 flusher on
    /// completion. `Arc` because `SchedulerGrpc` is `Clone`d per-connection
    /// and all handlers + the spawned recv tasks need the same buffers.
    log_buffers: Arc<LogBuffers>,
    /// PG pool for WatchBuild's event-log replay. `Option` so
    /// `new_for_tests` can skip it (None → broadcast-only, no
    /// replay). Production always sets it — main.rs already has
    /// the pool for the DB handle.
    pool: Option<sqlx::PgPool>,
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
    fn check_actor_alive(&self) -> Result<(), Status> {
        if !self.actor.is_alive() {
            return Err(Status::unavailable(
                "scheduler actor is unavailable (panicked or exited)",
            ));
        }
        Ok(())
    }

    // r[impl sched.grpc.leader-guard]
    /// Return UNAVAILABLE when this replica is not the leader.
    /// Called at the top of every handler, before any actor
    /// interaction. Standby replicas keep the gRPC server up
    /// (so the process is Ready from K8s's PoV) but refuse all
    /// RPCs — clients with a health-aware balanced channel see
    /// NOT_SERVING from grpc.health.v1 and route elsewhere.
    ///
    /// A bare `Status::unavailable` (not `Status::failed_precondition`)
    /// because tonic's p2c balancer ejects endpoints on
    /// UNAVAILABLE-at-connection but NOT on RPC-level errors;
    /// clients retry on UNAVAILABLE by convention (health-aware
    /// balancer has already removed us, so retry goes to leader).
    fn ensure_leader(&self) -> Result<(), Status> {
        if !self.is_leader.load(Ordering::Relaxed) {
            return Err(Status::unavailable("not leader (standby replica)"));
        }
        Ok(())
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
            // INTERNAL they'd surface the error to the user.
            ActorError::ChannelSend => Status::unavailable("scheduler actor is unavailable"),
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
    async fn send_and_await<R>(
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
/// Shared by `SubmitBuild` (here) and `ListBuilds` (admin/mod.rs).
///
/// The gateway sends the tenant NAME (from the `authorized_keys` entry's
/// comment field); the scheduler resolves it here. Empty name → `Ok(None)`
/// (single-tenant mode; no PG roundtrip). Unknown name →
/// `Status::invalid_argument`. PG error → `Status::internal`.
//
// TODO(P0298): introduce a `NormalizedName` newtype. This is the 4th
// callsite patching trim/empty normalization ad-hoc (gateway, store,
// controller all have tenant lookups). Newtype enforces at construction.
pub(crate) async fn resolve_tenant_name(
    pool: &sqlx::PgPool,
    name: &str,
) -> Result<Option<Uuid>, Status> {
    // Trim before lookup: gateway sends `tenant_name` from the
    // authorized_keys comment field, which may carry whitespace.
    // " team-a " must resolve the same as "team-a".
    let name = name.trim();
    if name.is_empty() {
        return Ok(None);
    }
    sqlx::query_scalar("SELECT tenant_id FROM tenants WHERE tenant_name = $1")
        .bind(name)
        .fetch_optional(pool)
        .await
        .map_err(|e| Status::internal(format!("tenant lookup failed: {e}")))?
        .ok_or_else(|| Status::invalid_argument(format!("unknown tenant: {name}")))
        .map(Some)
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
// SchedulerService implementation
// ---------------------------------------------------------------------------

#[tonic::async_trait]
impl SchedulerService for SchedulerGrpc {
    type SubmitBuildStream = ReceiverStream<Result<rio_proto::types::BuildEvent, Status>>;

    #[instrument(skip(self, request), fields(rpc = "SubmitBuild", build_id = tracing::field::Empty, tenant_id = tracing::field::Empty))]
    async fn submit_build(
        &self,
        request: Request<rio_proto::types::SubmitBuildRequest>,
    ) -> Result<Response<Self::SubmitBuildStream>, Status> {
        // Link to the gateway's trace BEFORE doing anything else. The
        // #[instrument] span is already entered by the time we're here;
        // link_parent adds an OTel span LINK to the client's traceparent
        // — NOT a parent. This span keeps its own trace_id; Jaeger shows
        // two traces connected by the link. Everything below (actor calls,
        // DB writes, store RPCs) inherits THIS span's trace_id.
        // See TODO(phase4b) at nix/tests/scenarios/observability.nix:269.
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        self.check_actor_alive()?;

        // Grab JWT Claims BEFORE into_inner() consumes the request.
        // Extensions are part of the Request wrapper, not the proto
        // body — into_inner() drops them. Clone is cheap: Claims is
        // 4 fields (Uuid + 2×i64 + short String).
        //
        // `None` is the common path: dev mode (no pubkey configured),
        // dual-mode fallback (gateway in SSH-comment mode, no header),
        // or VM tests (no interceptor in test harness). `Some` only
        // when the gateway is in JWT mode AND set the header AND the
        // interceptor verified it — i.e., we have a cryptographically
        // attested jti to check against the revocation table.
        let jwt_claims = request
            .extensions()
            .get::<rio_common::jwt::Claims>()
            .cloned();

        let req = request.into_inner();

        // Check backpressure before sending to actor
        if self.actor.is_backpressured() {
            return Err(Status::resource_exhausted(
                "scheduler is overloaded, please retry later",
            ));
        }

        // Validate DAG nodes before passing to the actor. Proto types have
        // all-public fields with no validation; an empty drv_hash would
        // become a DAG primary key, empty drv_path breaks the reverse
        // index, and empty system never matches any worker (derivation
        // stuck in Ready forever). Bound node count to protect memory.
        rio_common::grpc::check_bound("nodes", req.nodes.len(), rio_common::limits::MAX_DAG_NODES)?;
        rio_common::grpc::check_bound("edges", req.edges.len(), rio_common::limits::MAX_DAG_EDGES)?;
        for node in &req.nodes {
            if node.drv_hash.is_empty() {
                return Err(Status::invalid_argument("node drv_hash must be non-empty"));
            }
            // Structural validation: drv_path must parse as a valid
            // /nix/store/{32-char-nixbase32}-{name}.drv path. Checking
            // only !is_empty() would let a garbage path like "/tmp/evil"
            // become a DAG key. StorePath::parse catches: missing
            // /nix/store/ prefix, bad hash length, bad nixbase32 chars,
            // path traversal, oversized names.
            match rio_nix::store_path::StorePath::parse(&node.drv_path) {
                Ok(sp) if sp.is_derivation() => {}
                Ok(_) => {
                    return Err(Status::invalid_argument(format!(
                        "node {} drv_path {:?} is not a .drv path",
                        node.drv_hash, node.drv_path
                    )));
                }
                Err(e) => {
                    return Err(Status::invalid_argument(format!(
                        "node {} drv_path {:?} is malformed: {e}",
                        node.drv_hash, node.drv_path
                    )));
                }
            }
            if node.system.is_empty() {
                return Err(Status::invalid_argument(format!(
                    "node {} system must be non-empty",
                    node.drv_hash
                )));
            }
            // Gateway caps per-node at 64 KB; this is a defensive
            // upper bound (256 KB). Per-node — the 16 MB TOTAL budget
            // is gateway-enforced; here we just stop one malformed
            // node from being pathological.
            const MAX_DRV_CONTENT_BYTES: usize = 256 * 1024;
            rio_common::grpc::check_bound(
                "node.drv_content",
                node.drv_content.len(),
                MAX_DRV_CONTENT_BYTES,
            )?;
        }

        // UUID v7 (time-ordered, RFC 9562): the high 48 bits are Unix-ms
        // timestamp, so lexicographic sort == chronological sort. This makes
        // the S3 log key space `logs/{build_id}/...` naturally prefix-scannable
        // by time range, gives chronologically sorted `ls` output for
        // debugging, and improves PG index locality on builds.build_id
        // (recent builds cluster at the end of the index, not scattered
        // randomly like v4).
        //
        // Test code still uses v4 (~60 sites in actor/tests/) — test IDs
        // don't need ordering; changing them is pure churn.
        let build_id = Uuid::now_v7();
        let (reply_tx, reply_rx) = oneshot::channel();

        let options = BuildOptions {
            max_silent_time: req.max_silent_time,
            build_timeout: req.build_timeout,
            build_cores: req.build_cores,
        };

        // r[impl sched.tenant.resolve]
        // Tenant resolution: proto field carries tenant NAME (from gateway's
        // authorized_keys comment); resolve to UUID here via the tenants
        // table. Empty string → None (single-tenant mode). Unknown name →
        // InvalidArgument. Keeps gateway PG-free (stateless N-replica HA).
        let tenant_name = req.tenant_name.trim();
        let tenant_id = if tenant_name.is_empty() {
            None
        } else {
            let pool = self.pool.as_ref().ok_or_else(|| {
                Status::failed_precondition("tenant lookup requires database connection")
            })?;
            resolve_tenant_name(pool, tenant_name).await?
        };

        // r[impl gw.jwt.verify] — scheduler-side jti revocation check.
        //
        // Gateway stays PG-free (stateless N-replica HA); the scheduler
        // already has the pool open for tenant resolve above, so the
        // revocation lookup piggybacks here. Store does NOT duplicate
        // this — SubmitBuild is the ingress choke point for builds;
        // everything downstream trusts that the scheduler validated.
        //
        // `jti` is read from the interceptor-attached Claims extension,
        // NOT from a proto body field. The gateway never constructs
        // `SubmitBuildRequest.jwt_jti` — that field does not exist.
        // Zero wire redundancy: the jti travels once (inside the JWT),
        // is parsed once (by the interceptor), and is read once (here).
        // See r[gw.jwt.issue] for the mint-side story.
        //
        // Revoked → UNAUTHENTICATED, same code as a bad signature or
        // expired token. From the client's perspective "your token is
        // no longer valid" is one failure mode regardless of WHY.
        //
        // No-Claims (dev/dual-mode) → skip. Can't revoke what wasn't
        // presented. In JWT mode, Claims are ALWAYS present by the
        // time we get here: the interceptor either attached them or
        // returned UNAUTHENTICATED upstream, never a third state.
        if let Some(claims) = &jwt_claims {
            // Same pool-presence gate as tenant resolve. If pool is
            // None AND Claims are Some, something is misconfigured
            // (JWT mode requires PG for revocation); fail loud.
            let pool = self.pool.as_ref().ok_or_else(|| {
                Status::failed_precondition(
                    "jti revocation check requires database connection \
                     (JWT mode enabled but scheduler pool is None)",
                )
            })?;
            // EXISTS — short-circuits at first match, no row data
            // transferred. PK index on jti makes this O(log n). The
            // table is small (revocations are rare events) so this
            // is ~1 index page hit.
            let revoked: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM jwt_revoked WHERE jti = $1)")
                    .bind(&claims.jti)
                    .fetch_one(pool)
                    .await
                    .map_err(|e| Status::internal(format!("jti revocation lookup failed: {e}")))?;
            if revoked {
                return Err(Status::unauthenticated("token revoked"));
            }
        }

        // Capture the current span's traceparent BEFORE sending to the
        // actor. Span context does not cross the mpsc channel boundary;
        // the actor task's `handle_merge_dag` #[instrument] span is a
        // fresh root. Carrying traceparent as plain data lets dispatch
        // embed the gateway-linked trace in WorkAssignment.
        let traceparent = rio_proto::interceptor::current_traceparent();
        let req = MergeDagRequest {
            build_id,
            tenant_id,
            priority_class: if req.priority_class.is_empty() {
                crate::state::PriorityClass::default()
            } else {
                req.priority_class
                    .parse()
                    .map_err(|e| Status::invalid_argument(format!("priority_class: {e}")))?
            },
            nodes: req.nodes,
            edges: req.edges,
            options,
            keep_going: req.keep_going,
            traceparent,
        };
        let cmd = ActorCommand::MergeDag {
            req,
            reply: reply_tx,
        };

        let bcast = self.send_and_await(cmd, reply_rx).await?;
        // Record build_id + tenant_id on the span (declared Empty in #[instrument]).
        // Per observability.md these are required structured-log fields.
        tracing::Span::current().record("build_id", build_id.to_string());
        if let Some(tid) = tenant_id {
            tracing::Span::current().record("tenant_id", tracing::field::display(tid));
        }
        info!(build_id = %build_id, "build submitted");
        // No replay: fresh build, MergeDag subscribed BEFORE seq=1
        // (Started) was emitted. last_seq=0, no gap. Pure broadcast.
        let mut resp = Response::new(bridge_build_events("submit-build-bridge", bcast, None));
        // Initial metadata: build_id. Reaches the client as soon as
        // this function returns Ok — BEFORE bridge_build_events' task
        // sends event 0. If we SIGTERM between here and event 0, the
        // gateway has build_id and can WatchBuild-reconnect. Closes
        // the "empty build event stream" gap (phase4a remediation 20).
        //
        // UUID.to_string() is always ASCII-hex-and-dashes — the
        // .parse::<MetadataValue<Ascii>>() cannot fail. expect() not
        // unwrap() so the message is greppable if this invariant ever breaks.
        resp.metadata_mut().insert(
            rio_proto::BUILD_ID_HEADER,
            build_id
                .to_string()
                .parse()
                .expect("UUID string is always valid ASCII metadata"),
        );
        Ok(resp)
    }

    type WatchBuildStream = ReceiverStream<Result<rio_proto::types::BuildEvent, Status>>;

    #[instrument(skip(self, request), fields(rpc = "WatchBuild"))]
    async fn watch_build(
        &self,
        request: Request<rio_proto::types::WatchBuildRequest>,
    ) -> Result<Response<Self::WatchBuildStream>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let req = request.into_inner();
        let build_id = Self::parse_build_id(&req.build_id)?;

        let (reply_tx, reply_rx) = oneshot::channel();

        let cmd = ActorCommand::WatchBuild {
            build_id,
            since_sequence: req.since_sequence,
            reply: reply_tx,
        };

        let (bcast, last_seq) = self.send_and_await(cmd, reply_rx).await?;

        // Replay IF: pool available AND there's a gap to fill.
        // `since_sequence >= last_seq` → gateway already saw
        // everything, empty range, skip the PG round-trip.
        // `since_sequence == 0 && last_seq == 0` → build just
        // started, nothing emitted yet — common case, cheap exit.
        let replay = match &self.pool {
            Some(pool) if req.since_sequence < last_seq => Some(EventReplay {
                pool: pool.clone(),
                build_id,
                since: req.since_sequence,
                last_seq,
            }),
            _ => None,
        };

        Ok(Response::new(bridge_build_events(
            "watch-build-bridge",
            bcast,
            replay,
        )))
    }

    #[instrument(skip(self, request), fields(rpc = "QueryBuildStatus"))]
    async fn query_build_status(
        &self,
        request: Request<rio_proto::types::QueryBuildRequest>,
    ) -> Result<Response<rio_proto::types::BuildStatus>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let req = request.into_inner();
        let build_id = Self::parse_build_id(&req.build_id)?;

        let (reply_tx, reply_rx) = oneshot::channel();

        let cmd = ActorCommand::QueryBuildStatus {
            build_id,
            reply: reply_tx,
        };

        let status = self.send_and_await(cmd, reply_rx).await?;
        Ok(Response::new(status))
    }

    #[instrument(skip(self, request), fields(rpc = "CancelBuild"))]
    async fn cancel_build(
        &self,
        request: Request<rio_proto::types::CancelBuildRequest>,
    ) -> Result<Response<rio_proto::types::CancelBuildResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let req = request.into_inner();
        let build_id = Self::parse_build_id(&req.build_id)?;

        let (reply_tx, reply_rx) = oneshot::channel();

        let cmd = ActorCommand::CancelBuild {
            build_id,
            reason: req.reason,
            reply: reply_tx,
        };

        let cancelled = self.send_and_await(cmd, reply_rx).await?;

        Ok(Response::new(rio_proto::types::CancelBuildResponse {
            cancelled,
        }))
    }

    // r[impl sched.tenant.resolve]
    /// Name→UUID resolution exposed as an RPC for the gateway's JWT
    /// mint path. Same `resolve_tenant_name` helper as SubmitBuild's
    /// inline resolve — one source of truth for the lookup.
    ///
    /// NOT leader-gated: tenant lookup is a read-only PG query, no
    /// actor interaction, safe on any replica. The gateway calls this
    /// during SSH `auth_publickey` (before any build submission), so
    /// gating on leadership would make SSH auth latency depend on
    /// leader-election state. A standby replica with a pool can
    /// answer just as correctly as the leader.
    ///
    /// Empty name → InvalidArgument (not Ok("") — the caller shouldn't
    /// be calling at all for single-tenant mode; empty here means a
    /// bug in the gateway's gate). This differs from SubmitBuild's
    /// inline resolve (empty → Ok(None)), which is intentional:
    /// SubmitBuild's empty-name is a VALID state (single-tenant),
    /// ResolveTenant's empty-name is a CALLER ERROR.
    #[instrument(skip(self, request), fields(rpc = "ResolveTenant"))]
    async fn resolve_tenant(
        &self,
        request: Request<rio_proto::scheduler::ResolveTenantRequest>,
    ) -> Result<Response<rio_proto::scheduler::ResolveTenantResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let req = request.into_inner();

        let name = req.tenant_name.trim();
        if name.is_empty() {
            return Err(Status::invalid_argument(
                "tenant_name is empty (gateway should gate single-tenant mode before calling)",
            ));
        }

        let pool = self.pool.as_ref().ok_or_else(|| {
            Status::failed_precondition("tenant resolution requires database connection")
        })?;

        // `resolve_tenant_name` returns Ok(None) for empty input and
        // Err(InvalidArgument) for unknown. We already handled empty
        // above, so None here is unreachable — but match defensively.
        let tenant_id = resolve_tenant_name(pool, name)
            .await?
            .ok_or_else(|| Status::internal("resolve_tenant_name returned None for non-empty"))?;

        Ok(Response::new(rio_proto::scheduler::ResolveTenantResponse {
            tenant_id: tenant_id.to_string(),
        }))
    }
}

// ---------------------------------------------------------------------------
// WorkerService implementation
// ---------------------------------------------------------------------------

#[tonic::async_trait]
impl WorkerService for SchedulerGrpc {
    type BuildExecutionStream = ReceiverStream<Result<rio_proto::types::SchedulerMessage, Status>>;

    #[instrument(skip(self, request), fields(rpc = "BuildExecution"))]
    async fn build_execution(
        &self,
        request: Request<tonic::Streaming<rio_proto::types::WorkerMessage>>,
    ) -> Result<Response<Self::BuildExecutionStream>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let mut stream = request.into_inner();

        // The first message MUST be a WorkerRegister with the worker_id.
        // This ensures the stream and heartbeat use the same identity.
        let first = stream
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("empty BuildExecution stream"))?;
        let worker_id = match first.msg {
            Some(rio_proto::types::worker_message::Msg::Register(reg)) => {
                if reg.worker_id.is_empty() {
                    return Err(Status::invalid_argument(
                        "WorkerRegister.worker_id is empty",
                    ));
                }
                reg.worker_id
            }
            _ => {
                return Err(Status::invalid_argument(
                    "first BuildExecution message must be WorkerRegister",
                ));
            }
        };
        info!(worker_id = %worker_id, "worker stream opened");

        // Create the internal channel for the actor to send SchedulerMessages to this worker.
        let (actor_tx, mut actor_rx) = mpsc::channel::<rio_proto::types::SchedulerMessage>(256);

        // Create the output channel wrapping messages in Result for tonic.
        let (output_tx, output_rx) =
            mpsc::channel::<Result<rio_proto::types::SchedulerMessage, Status>>(256);

        // Register the worker stream with the actor (blocking send — must not drop).
        self.actor
            .send_unchecked(ActorCommand::WorkerConnected {
                worker_id: worker_id.as_str().into(),
                stream_tx: actor_tx,
            })
            .await
            .map_err(|_| Status::unavailable("scheduler actor unavailable"))?;

        // Bridge actor_rx -> output_tx, wrapping in Ok()
        rio_common::task::spawn_monitored("build-exec-bridge", async move {
            while let Some(msg) = actor_rx.recv().await {
                if output_tx.send(Ok(msg)).await.is_err() {
                    break;
                }
            }
        });

        // Spawn a task to read worker messages and forward to the actor
        let actor_for_recv = self.actor.clone();
        let log_buffers = Arc::clone(&self.log_buffers);
        let worker_id_for_recv = worker_id.clone();
        // For the Progress arm's proactive ema. None in bare-actor
        // tests (new_for_tests) — Progress becomes a no-op there,
        // which is what those tests want anyway.
        let pool_for_recv = self.pool.clone();

        rio_common::task::spawn_monitored("worker-stream-reader", async move {
            loop {
                let msg = match stream.message().await {
                    Ok(Some(m)) => m,
                    Ok(None) => break, // clean disconnect
                    Err(e) => {
                        warn!(
                            worker_id = %worker_id_for_recv,
                            error = %e,
                            "worker stream read error, treating as disconnect"
                        );
                        break;
                    }
                };
                if let Some(inner) = msg.msg {
                    match inner {
                        rio_proto::types::worker_message::Msg::Register(_) => {
                            warn!(
                                worker_id = %worker_id_for_recv,
                                "duplicate WorkerRegister on established stream, ignoring"
                            );
                        }
                        rio_proto::types::worker_message::Msg::Ack(ack) => {
                            info!(
                                worker_id = %worker_id_for_recv,
                                drv_path = %ack.drv_path,
                                "worker acknowledged assignment"
                            );
                        }
                        rio_proto::types::worker_message::Msg::Completion(mut report) => {
                            let drv_path = std::mem::take(&mut report.drv_path);
                            // A CompletionReport with result: None is malformed, but
                            // we must not silently drop it — the derivation would hang
                            // in Running forever. Synthesize an InfrastructureFailure.
                            let result = report.result.unwrap_or_else(|| {
                                warn!(
                                    worker_id = %worker_id_for_recv,
                                    drv_path = %drv_path,
                                    "completion with None result, synthesizing InfrastructureFailure"
                                );
                                rio_proto::types::BuildResult {
                                    status:
                                        rio_proto::types::BuildResultStatus::InfrastructureFailure
                                            .into(),
                                    error_msg: "worker sent CompletionReport with no result"
                                        .into(),
                                    ..Default::default()
                                }
                            });
                            // Use blocking send for completion — dropping it would
                            // leave the derivation stuck in Running.
                            if actor_for_recv
                                .send_unchecked(ActorCommand::ProcessCompletion {
                                    worker_id: worker_id_for_recv.clone().into(),
                                    drv_key: drv_path,
                                    result,
                                    peak_memory_bytes: report.peak_memory_bytes,
                                    output_size_bytes: report.output_size_bytes,
                                    peak_cpu_cores: report.peak_cpu_cores,
                                })
                                .await
                                .is_err()
                            {
                                warn!("actor channel closed while sending completion");
                                break;
                            }
                        }
                        rio_proto::types::worker_message::Msg::Progress(progress) => {
                            // Proactive ema: if a running build's
                            // cgroup memory.peak already exceeds what
                            // the EMA predicted, overwrite the EMA NOW
                            // — the next submit of this drv gets
                            // right-sized before this build even
                            // finishes. Same penalty-overwrite
                            // semantics as completion.rs:363's
                            // r[sched.classify.penalty-overwrite],
                            // just triggered earlier (mid-build
                            // instead of post-completion).
                            //
                            // r[sched.preempt.never-running] holds:
                            // advisory only, never kills/migrates.
                            // No CancelBuild, no reassignment — the
                            // only side effect is a db write.
                            //
                            // Worker populates ONLY
                            // resources.memory_used_bytes with cgroup
                            // memory.peak (see rio-worker runtime.rs);
                            // 0 = "couldn't read memory.peak" (ENOENT
                            // during daemon-spawn), same no-signal
                            // sentinel as completion's peak_mem filter.
                            //
                            // `tokio::spawn`: the db write is
                            // fire-and-forget. `.await`ing inline
                            // would stall the stream loop on PG
                            // latency — a slow PG roundtrip would
                            // backpressure the worker's log batches
                            // and completion reports. Progress is
                            // advisory; a dropped or failed update is
                            // caught by the next 10s tick (memory.peak
                            // is monotone — no info lost).
                            if let (Some(pool), Some(res)) = (&pool_for_recv, &progress.resources)
                                && res.memory_used_bytes > 0
                            {
                                let db = crate::db::SchedulerDb::new(pool.clone());
                                let drv_path = progress.drv_path;
                                let observed = res.memory_used_bytes;
                                tokio::spawn(async move {
                                    match db
                                        .update_ema_peak_memory_proactive(&drv_path, observed)
                                        .await
                                    {
                                        Ok(true) => {
                                            metrics::counter!(
                                                "rio_scheduler_ema_proactive_updates_total"
                                            )
                                            .increment(1);
                                        }
                                        Ok(false) => {
                                            // observed ≤ ema, or no
                                            // build_history row yet
                                            // (first ever build of this
                                            // pname). Expected path.
                                        }
                                        Err(e) => {
                                            warn!(
                                                drv_path = %drv_path,
                                                error = %e,
                                                "proactive ema update failed"
                                            );
                                        }
                                    }
                                });
                            }
                        }
                        rio_proto::types::worker_message::Msg::LogBatch(log) => {
                            // Two-step: buffer (never blocks on actor), then forward.
                            //
                            // 1. Ring buffer write — direct, no actor involvement.
                            //    This is the durability path: even if the actor is
                            //    backpressured or the gateway stream lags, the lines
                            //    land here and are serveable via AdminService.
                            log_buffers.push(&log);

                            // 2. Gateway forward — via actor (it owns the
                            //    drv_path→hash→interested_builds resolution and the
                            //    broadcast senders). `try_send`, NOT send_unchecked:
                            //    if the actor channel is backpressured (80% full,
                            //    hysteresis), we drop the gateway-forward. The ring
                            //    buffer already has the lines; the gateway misses
                            //    *live* logs but can still get them via AdminService.
                            //
                            //    This is the opposite tradeoff from ProcessCompletion
                            //    (which MUST use send_unchecked — a dropped completion
                            //    leaves a derivation stuck Running forever). A dropped
                            //    log batch is a degraded-mode nuisance, not a hang.
                            let drv_path = log.derivation_path.clone();
                            if actor_for_recv
                                .try_send(ActorCommand::ForwardLogBatch {
                                    drv_path,
                                    batch: log,
                                })
                                .is_err()
                            {
                                metrics::counter!("rio_scheduler_log_forward_dropped_total")
                                    .increment(1);
                            }
                        }
                    }
                }
            }

            // Stream closed: worker disconnected. Use blocking send — if this
            // is dropped due to backpressure, running derivations won't be
            // reassigned and will hang forever.
            if actor_for_recv
                .send_unchecked(ActorCommand::WorkerDisconnected {
                    worker_id: worker_id_for_recv.into(),
                })
                .await
                .is_err()
            {
                warn!("actor channel closed while sending worker disconnect");
            }
        });

        Ok(Response::new(ReceiverStream::new(output_rx)))
    }

    #[instrument(skip(self, request), fields(rpc = "Heartbeat"))]
    async fn heartbeat(
        &self,
        request: Request<rio_proto::types::HeartbeatRequest>,
    ) -> Result<Response<rio_proto::types::HeartbeatResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let req = request.into_inner();

        if req.worker_id.is_empty() {
            return Err(Status::invalid_argument("worker_id is required"));
        }

        // Bound heartbeat payload sizes. Heartbeats bypass backpressure
        // (send_unchecked below), so an unbounded running_builds list from
        // a malicious/buggy worker would allocate megabytes and stall the
        // actor event loop during reconciliation with no backpressure signal.
        const MAX_HEARTBEAT_FEATURES: usize = 64;
        const MAX_HEARTBEAT_RUNNING_BUILDS: usize = 1000;
        // A worker advertising thousands of systems is buggy or
        // hostile. 16 covers native + linux-builder + the four
        // cross-arch targets × two OSes.
        const MAX_HEARTBEAT_SYSTEMS: usize = 16;
        rio_common::grpc::check_bound("systems", req.systems.len(), MAX_HEARTBEAT_SYSTEMS)?;
        rio_common::grpc::check_bound(
            "supported_features",
            req.supported_features.len(),
            MAX_HEARTBEAT_FEATURES,
        )?;
        rio_common::grpc::check_bound(
            "running_builds",
            req.running_builds.len(),
            MAX_HEARTBEAT_RUNNING_BUILDS,
        )?;

        // Bound the bloom filter. 1 MiB = 8M bits = ~800k items at 1%
        // FPR — WAY more than any worker would realistically cache.
        // Above this, the worker is either buggy or hostile.
        const MAX_BLOOM_BYTES: usize = 1024 * 1024;

        // Parse the bloom filter. from_wire validates algorithm/version/
        // sizes; reject the whole heartbeat on validation failure
        // (worker is sending garbage — don't silently drop the filter
        // and score it as "no locality info"; that masks the bug).
        let bloom = req
            .local_paths
            .map(|p| {
                rio_common::grpc::check_bound("local_paths.data", p.data.len(), MAX_BLOOM_BYTES)?;
                rio_common::bloom::BloomFilter::from_wire(
                    p.data,
                    p.hash_count,
                    p.num_bits,
                    p.hash_algorithm,
                    p.version,
                )
                .map_err(|e| Status::invalid_argument(format!("invalid bloom filter: {e}")))
            })
            .transpose()?;

        // size_class: empty-string in proto → None. Proto doesn't have
        // Option for strings; empty is the conventional "unset." An
        // actually-empty-named class makes no sense (operator config
        // validation would reject it), so this mapping is lossless.
        let size_class = (!req.size_class.is_empty()).then_some(req.size_class);

        let cmd = ActorCommand::Heartbeat {
            worker_id: req.worker_id.into(),
            systems: req.systems,
            supported_features: req.supported_features,
            max_builds: req.max_builds,
            running_builds: req.running_builds,
            bloom,
            size_class,
            resources: req.resources,
            store_degraded: req.store_degraded,
        };

        // Heartbeats bypass backpressure: dropping a heartbeat under load
        // would cause a false worker timeout -> reassignment -> more load.
        // Same pattern as WorkerConnected/WorkerDisconnected.
        self.actor
            .send_unchecked(cmd)
            .await
            .map_err(Self::actor_error_to_status)?;

        Ok(Response::new(rio_proto::types::HeartbeatResponse {
            accepted: true,
            // Same Arc<AtomicU64> the actor reads for WorkAssignment.generation
            // (dispatch.rs single-load). The lease task writes on each
            // leadership acquisition. Non-K8s mode: stays at 1.
            generation: self.actor.leader_generation(),
        }))
    }
}

// ---------------------------------------------------------------------------
// BuildExecution bidirectional stream e2e
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
