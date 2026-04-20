//! `ExecutorService` gRPC implementation for [`SchedulerGrpc`].
//!
//! Worker-facing RPCs: the `BuildExecution` bidirectional stream and
//! the `Heartbeat` unary RPC. Split from `mod.rs` (P0356) — heartbeat
//! bounds-checking and the stream message-dispatch tree change on a
//! schedule independent of the client-facing SchedulerService RPCs.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::{info, instrument, warn};

use rio_proto::ExecutorService;

use crate::actor::{ActorCommand, HeartbeatPayload};

use super::SchedulerGrpc;

/// Monotonic per-stream epoch source. Each `BuildExecution` stream gets
/// a fresh epoch on open; the reader task echoes it on
/// `ExecutorDisconnected`. The actor compares against
/// `ExecutorState::stream_epoch` to drop a stale disconnect from a
/// prior stream (I-056a's late-disconnect half — connect-before-
/// disconnect ordering observed live during deploy churn). Process-
/// global (not per-`SchedulerGrpc`) since `SchedulerGrpc` is `Clone`d
/// per-connection and all clones must share the sequence.
static STREAM_EPOCH_SEQ: AtomicU64 = AtomicU64::new(0);

/// Upper bound on distinct `derivation_path` values one
/// `BuildExecution` stream may push to `LogBuffers`. Per
/// `[Single build per pod, no knob]` a legitimate stream pushes for
/// exactly ONE; 8 covers reassign/retry slop. A compromised worker
/// streaming fabricated paths would otherwise create unbounded
/// DashMap entries that `flush_periodic` iterates serially with one
/// S3 PUT each — flusher starvation + memory growth + S3 cost.
const MAX_DRVS_PER_STREAM: usize = 8;

#[tonic::async_trait]
impl ExecutorService for SchedulerGrpc {
    type BuildExecutionStream = ReceiverStream<Result<rio_proto::types::SchedulerMessage, Status>>;

    // r[impl proto.stream.bidi]
    #[instrument(skip(self, request), fields(rpc = "BuildExecution"))]
    async fn build_execution(
        &self,
        request: Request<tonic::Streaming<rio_proto::types::ExecutorMessage>>,
    ) -> Result<Response<Self::BuildExecutionStream>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        self.check_actor_alive()?;
        // r[impl sec.executor.identity-token]
        // Bind this stream to the HMAC-attested intent the pod was
        // spawned for. A compromised builder cannot mint a token for
        // another pod's intent → cannot hijack its `stream_tx` (the
        // actor rejects on `auth_intent` mismatch) → cannot receive
        // its `WorkAssignment.assignment_token` → cannot poison its
        // outputs. `None` in dev mode (no HMAC key configured).
        let auth_intent = self.require_executor(&request)?;
        let mut stream = request.into_inner();

        // The first message MUST be a ExecutorRegister with the executor_id.
        // This ensures the stream and heartbeat use the same identity.
        let first = stream
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("empty BuildExecution stream"))?;
        let executor_id = match first.msg {
            Some(rio_proto::types::executor_message::Msg::Register(reg)) => {
                if reg.executor_id.is_empty() {
                    return Err(Status::invalid_argument(
                        "ExecutorRegister.executor_id is empty",
                    ));
                }
                reg.executor_id
            }
            _ => {
                return Err(Status::invalid_argument(
                    "first BuildExecution message must be ExecutorRegister",
                ));
            }
        };
        info!(executor_id = %executor_id, "worker stream opened");

        // Create the internal channel for the actor to send SchedulerMessages to this worker.
        let (actor_tx, mut actor_rx) = mpsc::channel::<rio_proto::types::SchedulerMessage>(256);

        // Create the output channel wrapping messages in Result for tonic.
        let (output_tx, output_rx) =
            mpsc::channel::<Result<rio_proto::types::SchedulerMessage, Status>>(256);

        // Per-stream epoch: starts at 1 (0 = "no stream yet" in
        // ExecutorState::new). Captured into the reader closure below
        // and echoed on ExecutorDisconnected.
        let stream_epoch = STREAM_EPOCH_SEQ.fetch_add(1, Ordering::Relaxed) + 1;

        // Register the worker stream with the actor (blocking send — must not drop).
        self.actor
            .send_unchecked(ActorCommand::ExecutorConnected {
                executor_id: executor_id.as_str().into(),
                stream_tx: actor_tx,
                stream_epoch,
                auth_intent,
            })
            .await
            .map_err(Self::actor_error_to_status)?;

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
        let executor_id_for_recv = executor_id.clone();
        // r[impl sched.lease.standby-drops-writes]
        // Generation-fence the stream: capture the lease generation at
        // open-time. `ensure_leader()` above only checks at open; the
        // reader loop below sends `ProcessCompletion`/`PrefetchComplete`
        // via `send_unchecked` for the stream's lifetime. If the lease
        // is lost (or flapped) mid-stream, an ex-leader would otherwise
        // forward a `CompletionReport` and write terminal PG state for
        // a generation it no longer owns. Breaking the loop closes the
        // stream → worker reconnects to the new leader.
        let is_leader = Arc::clone(&self.is_leader);
        let generation = Arc::clone(&self.generation);
        let stream_gen = generation.load(std::sync::atomic::Ordering::Acquire);

        rio_common::task::spawn_monitored("worker-stream-reader", async move {
            let mut seen_drvs: std::collections::HashSet<String> = std::collections::HashSet::new();
            let stream_is_stale = || {
                !is_leader.load(std::sync::atomic::Ordering::SeqCst)
                    || generation.load(std::sync::atomic::Ordering::Acquire) != stream_gen
            };
            loop {
                let msg = match stream.message().await {
                    Ok(Some(m)) => m,
                    Ok(None) => break, // clean disconnect
                    Err(e) => {
                        warn!(
                            executor_id = %executor_id_for_recv,
                            error = %e,
                            "worker stream read error, treating as disconnect"
                        );
                        break;
                    }
                };
                if let Some(inner) = msg.msg {
                    match inner {
                        rio_proto::types::executor_message::Msg::Register(_) => {
                            warn!(
                                executor_id = %executor_id_for_recv,
                                "duplicate ExecutorRegister on established stream, ignoring"
                            );
                        }
                        rio_proto::types::executor_message::Msg::Ack(ack) => {
                            info!(
                                executor_id = %executor_id_for_recv,
                                drv_path = %ack.drv_path,
                                "worker acknowledged assignment"
                            );
                        }
                        rio_proto::types::executor_message::Msg::PrefetchComplete(pc) => {
                            if stream_is_stale() {
                                info!(
                                    executor_id = %executor_id_for_recv,
                                    "lease lost/flapped mid-stream; closing worker stream"
                                );
                                break;
                            }
                            // r[sched.assign.warm-gate]: worker ACKed
                            // the initial PrefetchHint. Forward to
                            // the actor which flips ExecutorState.warm.
                            // send_unchecked (not try_send): dropping
                            // this under backpressure would leave a
                            // warmed worker permanently cold in the
                            // scheduler's view — idle capacity right
                            // when the scheduler is saturated.
                            if actor_for_recv
                                .send_unchecked(ActorCommand::PrefetchComplete {
                                    executor_id: executor_id_for_recv.clone().into(),
                                    paths_fetched: pc.paths_fetched,
                                })
                                .await
                                .is_err()
                            {
                                warn!("actor channel closed while sending PrefetchComplete");
                                break;
                            }
                        }
                        rio_proto::types::executor_message::Msg::Completion(mut report) => {
                            if stream_is_stale() {
                                info!(
                                    executor_id = %executor_id_for_recv,
                                    "lease lost/flapped mid-stream; closing worker stream"
                                );
                                break;
                            }
                            let drv_path = std::mem::take(&mut report.drv_path);
                            // A CompletionReport with result: None is malformed, but
                            // we must not silently drop it — the derivation would hang
                            // in Running forever. Synthesize an InfrastructureFailure.
                            let result = report.result.unwrap_or_else(|| {
                                warn!(
                                    executor_id = %executor_id_for_recv,
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
                                    executor_id: executor_id_for_recv.clone().into(),
                                    drv_key: drv_path,
                                    result,
                                    peak_memory_bytes: report.peak_memory_bytes,
                                    peak_cpu_cores: report.peak_cpu_cores,
                                    node_name: report.node_name,
                                    hw_class: report.hw_class,
                                    final_resources: report.final_resources,
                                })
                                .await
                                .is_err()
                            {
                                warn!("actor channel closed while sending completion");
                                break;
                            }
                        }
                        rio_proto::types::executor_message::Msg::Phase(phase) => {
                            // Same try_send semantics as ForwardLogBatch:
                            // a dropped phase update is cosmetic (nom
                            // misses one phase column refresh), not a hang.
                            if actor_for_recv
                                .try_send(ActorCommand::ForwardPhase { phase })
                                .is_err()
                            {
                                metrics::counter!("rio_scheduler_log_forward_dropped_total")
                                    .increment(1);
                            }
                        }
                        rio_proto::types::executor_message::Msg::LogBatch(log) => {
                            // Two-step: buffer (never blocks on actor), then forward.
                            //
                            // 0. Per-stream distinct-path cap. The worker is NOT
                            //    trusted; `push()` only gates on `sealed` so a
                            //    fabricated path always creates a fresh DashMap
                            //    entry that `flush_periodic` then iterates with
                            //    one S3 PUT each. The actor's `hash_for_path`
                            //    gate runs AFTER push and only drops the
                            //    gateway-forward, not the buffer entry.
                            if !seen_drvs.contains(&log.derivation_path) {
                                if seen_drvs.len() >= MAX_DRVS_PER_STREAM {
                                    metrics::counter!(
                                        "rio_scheduler_log_unknown_drv_dropped_total"
                                    )
                                    .increment(1);
                                    continue;
                                }
                                seen_drvs.insert(log.derivation_path.clone());
                            }
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

            // The seal's job (block late batches between CompletionReport
            // and flusher drain) is done once this stream is closed — no
            // more batches CAN arrive. Branch on whether a completion
            // landed: sealed → unseal (leave the buffer for the flusher's
            // drain — discard would race it and lose the log); NOT sealed
            // → discard (no completion → fake path or aborted build →
            // reap the entry so periodic-flush stops iterating it). A
            // legitimate in-flight drv whose worker disconnected
            // mid-build loses its in-memory tail; periodic-flush already
            // snapshotted to S3 and the next assignment's logs replace
            // it from scratch anyway.
            for drv in &seen_drvs {
                if log_buffers.is_sealed(drv) {
                    log_buffers.unseal(drv);
                } else {
                    log_buffers.discard(drv);
                }
            }

            // Stream closed: worker disconnected. Use blocking send — if this
            // is dropped due to backpressure, running derivations won't be
            // reassigned and will hang forever.
            if actor_for_recv
                .send_unchecked(ActorCommand::ExecutorDisconnected {
                    executor_id: executor_id_for_recv.into(),
                    stream_epoch,
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
        // r[impl sec.executor.identity-token]
        let auth_intent = self.require_executor(&request)?;
        let req = request.into_inner();

        if req.executor_id.is_empty() {
            return Err(Status::invalid_argument("executor_id is required"));
        }
        // Body `intent_id` MUST equal the token's. The actor's
        // `worker.intent_id` (set from this body field) is what
        // dispatch matches and what `handle_worker_connected` checks
        // on reconnect — binding it here means it's
        // cryptographically attested. A spoofed
        // `Heartbeat{draining:true}` for another executor needs that
        // executor's intent_id, which the attacker has no token for.
        if let Some(ref ai) = auth_intent
            && req.intent_id != *ai
        {
            return Err(Status::unauthenticated(
                "heartbeat intent_id does not match x-rio-executor-token",
            ));
        }

        // Bound heartbeat payload sizes. Heartbeats bypass backpressure
        // (send_unchecked below), so unbounded payloads from a
        // malicious/buggy worker would stall the actor event loop with
        // no backpressure signal.
        const MAX_HEARTBEAT_FEATURES: usize = 64;
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

        // intent_id: empty-string in proto → None. Proto doesn't have
        // Option for strings; empty is the conventional "unset." Empty
        // = Static-sized pod (no SpawnIntent annotation on the pod
        // template).
        let intent_id = (!req.intent_id.is_empty()).then_some(req.intent_id);

        // kind: prost encodes enums as i32; decode via try_from.
        // Unknown value (future proto version) → Builder (safe default:
        // an unrecognized-kind executor won't receive FODs, so no
        // airgap violation). 0 = Builder (wire default for pre-ADR-019
        // executors that don't send this field).
        let kind = rio_proto::types::ExecutorKind::try_from(req.kind)
            .unwrap_or(rio_proto::types::ExecutorKind::Builder);

        let cmd = ActorCommand::Heartbeat(HeartbeatPayload {
            executor_id: req.executor_id.into(),
            systems: req.systems,
            supported_features: req.supported_features,
            running_build: req.running_build,
            resources: req.resources,
            store_degraded: req.store_degraded,
            draining: req.draining,
            kind,
            intent_id,
        });

        // Heartbeats bypass backpressure: dropping a heartbeat under load
        // would cause a false worker timeout -> reassignment -> more load.
        // Same pattern as ExecutorConnected/ExecutorDisconnected.
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
