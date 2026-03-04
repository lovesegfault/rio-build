//! gRPC service implementations for SchedulerService and WorkerService.
//!
//! Both services run in the same scheduler binary. They communicate with the
//! DAG actor via the `ActorHandle`.

use std::sync::Arc;

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
}

impl SchedulerGrpc {
    /// Create a new gRPC service with the given actor handle.
    ///
    /// This makes a FRESH `LogBuffers`. For production (where the flusher
    /// needs the same buffers), use [`with_log_buffers`](Self::with_log_buffers)
    /// instead. This constructor is kept for tests where no flusher runs.
    pub fn new(actor: ActorHandle) -> Self {
        Self {
            actor,
            log_buffers: Arc::new(LogBuffers::new()),
        }
    }

    /// Create with an externally-owned `LogBuffers`. Production `main.rs`
    /// uses this so the LogFlusher (separate task) drains the SAME buffers
    /// that the BuildExecution recv task writes to. If you use `new()` in
    /// production, the flusher would drain an unrelated empty DashMap
    /// forever while the real logs pile up here — a silent total log loss.
    pub fn with_log_buffers(actor: ActorHandle, log_buffers: Arc<LogBuffers>) -> Self {
        Self { actor, log_buffers }
    }

    /// Access the shared log ring buffers. Exposed for `AdminService` (C9).
    pub fn log_buffers(&self) -> Arc<LogBuffers> {
        self.log_buffers.clone()
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

    /// Convert an ActorError to a tonic Status.
    pub(crate) fn actor_error_to_status(err: ActorError) -> Status {
        match err {
            ActorError::BuildNotFound(id) => Status::not_found(format!("build not found: {id}")),
            ActorError::Backpressure => {
                Status::resource_exhausted("scheduler is overloaded, please retry later")
            }
            ActorError::ChannelSend => Status::internal("scheduler actor is unavailable"),
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

/// Bridge a `broadcast::Receiver<BuildEvent>` into a tonic streaming response.
///
/// On `Lagged`, sends `DATA_LOSS` so the client fails cleanly instead of
/// silently hanging on a missed terminal event. Lagged means we permanently
/// missed n events; if `BuildCompleted` was among them the client would hang
/// forever waiting for a terminal event that will never arrive.
pub(crate) fn bridge_build_events(
    task_name: &'static str,
    mut bcast: broadcast::Receiver<rio_proto::types::BuildEvent>,
) -> ReceiverStream<Result<rio_proto::types::BuildEvent, Status>> {
    let (tx, rx) = mpsc::channel(256);
    rio_common::task::spawn_monitored(task_name, async move {
        loop {
            match bcast.recv().await {
                Ok(event) => {
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

    #[instrument(skip(self, request), fields(rpc = "SubmitBuild", build_id = tracing::field::Empty))]
    async fn submit_build(
        &self,
        request: Request<rio_proto::types::SubmitBuildRequest>,
    ) -> Result<Response<Self::SubmitBuildStream>, Status> {
        // Link into the gateway's trace BEFORE doing anything else. The
        // #[instrument] span is already entered by the time we're here;
        // link_parent stitches it to the client's trace_id. Everything
        // below (actor calls, DB writes, store RPCs) inherits this span.
        rio_proto::interceptor::link_parent(&request);
        self.check_actor_alive()?;
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
            // /nix/store/{32-char-nixbase32}-{name}.drv path. Previously
            // only checked !is_empty() — a garbage path like "/tmp/evil"
            // would become a DAG key. StorePath::parse catches: missing
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

        let req = MergeDagRequest {
            build_id,
            tenant_id: if req.tenant_id.is_empty() {
                None
            } else {
                // Parse at the boundary: a malformed UUID (e.g., "customer-foo")
                // would otherwise leak as a PostgreSQL text-to-uuid cast error.
                Some(
                    req.tenant_id
                        .parse::<uuid::Uuid>()
                        .map_err(|e| {
                            Status::invalid_argument(format!("invalid tenant_id UUID: {e}"))
                        })?
                        .to_string(),
                )
            },
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
        };
        let cmd = ActorCommand::MergeDag {
            req,
            reply: reply_tx,
        };

        let bcast = self.send_and_await(cmd, reply_rx).await?;
        // Record build_id on the span (declared Empty in #[instrument]).
        // Per observability.md:204 this is a required structured-log field.
        tracing::Span::current().record("build_id", build_id.to_string());
        info!(build_id = %build_id, "build submitted");
        Ok(Response::new(bridge_build_events(
            "submit-build-bridge",
            bcast,
        )))
    }

    type WatchBuildStream = ReceiverStream<Result<rio_proto::types::BuildEvent, Status>>;

    #[instrument(skip(self, request), fields(rpc = "WatchBuild"))]
    async fn watch_build(
        &self,
        request: Request<rio_proto::types::WatchBuildRequest>,
    ) -> Result<Response<Self::WatchBuildStream>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.check_actor_alive()?;
        let req = request.into_inner();
        let build_id = Self::parse_build_id(&req.build_id)?;

        let (reply_tx, reply_rx) = oneshot::channel();

        let cmd = ActorCommand::WatchBuild {
            build_id,
            since_sequence: req.since_sequence,
            reply: reply_tx,
        };

        let bcast = self.send_and_await(cmd, reply_rx).await?;
        Ok(Response::new(bridge_build_events(
            "watch-build-bridge",
            bcast,
        )))
    }

    #[instrument(skip(self, request), fields(rpc = "QueryBuildStatus"))]
    async fn query_build_status(
        &self,
        request: Request<rio_proto::types::QueryBuildRequest>,
    ) -> Result<Response<rio_proto::types::BuildStatus>, Status> {
        rio_proto::interceptor::link_parent(&request);
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
        let log_buffers = self.log_buffers.clone();
        let worker_id_for_recv = worker_id.clone();

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
                        rio_proto::types::worker_message::Msg::Completion(report) => {
                            let drv_path = report.drv_path.clone();
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
                                })
                                .await
                                .is_err()
                            {
                                warn!("actor channel closed while sending completion");
                                break;
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
                            let _ = actor_for_recv.try_send(ActorCommand::ForwardLogBatch {
                                drv_path,
                                batch: log,
                            });
                        }
                        rio_proto::types::worker_message::Msg::Progress(_progress) => {
                            // ProgressUpdate still dropped. For size-class
                            // routing we don't need a live sample stream:
                            // CompletionReport.peak_memory_bytes carries the
                            // kernel-tracked VmHWM (lifetime peak, one read
                            // at build-end) and that's what the estimator
                            // consumes. Mid-build ResourceUsage would only
                            // matter for live pre-emption / migration, which
                            // isn't in scope.
                            //
                            // TODO(phase3a): cpu_cores needs polling (no
                            // kernel-tracked peak equivalent). That's when
                            // ProgressUpdate becomes load-bearing.
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
            system: req.system,
            supported_features: req.supported_features,
            max_builds: req.max_builds,
            running_builds: req.running_builds,
            bloom,
            size_class,
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
            // TODO(phase3a): actual leader generation from Kubernetes Lease.
            // Phase 2a has a single scheduler instance; constant 1 is correct.
            generation: 1,
        }))
    }
}

// ---------------------------------------------------------------------------
// T6: BuildExecution bidirectional stream e2e (8.8)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
