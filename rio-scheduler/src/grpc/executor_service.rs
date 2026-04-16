//! `ExecutorService` gRPC implementation for [`SchedulerGrpc`].
//!
//! Worker-facing RPCs: the `BuildExecution` bidirectional stream and
//! the `Heartbeat` unary RPC. Split from `mod.rs` (P0356) — heartbeat
//! bounds-checking and the stream message-dispatch tree change on a
//! schedule independent of the client-facing SchedulerService RPCs.

use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::{info, instrument, warn};

use rio_proto::ExecutorService;

use crate::actor::{ActorCommand, HeartbeatPayload};

use super::SchedulerGrpc;

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

        // Register the worker stream with the actor (blocking send — must not drop).
        self.actor
            .send_unchecked(ActorCommand::ExecutorConnected {
                executor_id: executor_id.as_str().into(),
                stream_tx: actor_tx,
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

        rio_common::task::spawn_monitored("worker-stream-reader", async move {
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
                                    final_resources: report.final_resources,
                                })
                                .await
                                .is_err()
                            {
                                warn!("actor channel closed while sending completion");
                                break;
                            }
                        }
                        rio_proto::types::executor_message::Msg::Progress(_) => {
                            // ADR-023 SLA sizing reads cgroup telemetry
                            // from CompletionReport.final_resources, not
                            // mid-build Progress. The arm stays for
                            // protocol compat (worker still sends it).
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
                .send_unchecked(ActorCommand::ExecutorDisconnected {
                    executor_id: executor_id_for_recv.into(),
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

        if req.executor_id.is_empty() {
            return Err(Status::invalid_argument("executor_id is required"));
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

        // size_class: empty-string in proto → None. Proto doesn't have
        // Option for strings; empty is the conventional "unset." An
        // actually-empty-named class makes no sense (operator config
        // validation would reject it), so this mapping is lossless.
        let size_class = (!req.size_class.is_empty()).then_some(req.size_class);
        // intent_id: same empty→None mapping. Empty = Static-sized pod
        // (no SpawnIntent annotation on the pod template).
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
            size_class,
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
