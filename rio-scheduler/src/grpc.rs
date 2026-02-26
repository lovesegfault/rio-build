//! gRPC service implementations for SchedulerService and WorkerService.
//!
//! Both services run in the same scheduler binary. They communicate with the
//! DAG actor via the `ActorHandle`.

use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::{info, instrument, warn};
use uuid::Uuid;

use rio_proto::scheduler::scheduler_service_server::SchedulerService;
use rio_proto::worker::worker_service_server::WorkerService;

use crate::actor::{ActorCommand, ActorError, ActorHandle};
use crate::state::BuildOptions;

/// Shared scheduler state passed to gRPC handlers.
#[derive(Clone)]
pub struct SchedulerGrpc {
    actor: ActorHandle,
}

impl SchedulerGrpc {
    /// Create a new gRPC service with the given actor handle.
    pub fn new(actor: ActorHandle) -> Self {
        Self { actor }
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
    fn actor_error_to_status(err: ActorError) -> Status {
        match err {
            ActorError::BuildNotFound(id) => Status::not_found(format!("build not found: {id}")),
            ActorError::Backpressure => {
                Status::resource_exhausted("scheduler is overloaded, please retry later")
            }
            ActorError::ChannelSend => Status::internal("scheduler actor is unavailable"),
            ActorError::Database(e) => Status::internal(format!("database error: {e}")),
            ActorError::Internal(msg) => Status::internal(msg),
        }
    }
}

// ---------------------------------------------------------------------------
// SchedulerService implementation
// ---------------------------------------------------------------------------

#[tonic::async_trait]
impl SchedulerService for SchedulerGrpc {
    type SubmitBuildStream = ReceiverStream<Result<rio_proto::types::BuildEvent, Status>>;

    #[instrument(skip(self, request), fields(rpc = "SubmitBuild"))]
    async fn submit_build(
        &self,
        request: Request<rio_proto::types::SubmitBuildRequest>,
    ) -> Result<Response<Self::SubmitBuildStream>, Status> {
        self.check_actor_alive()?;
        let req = request.into_inner();

        // Check backpressure before sending to actor
        if self.actor.is_backpressured() {
            return Err(Status::resource_exhausted(
                "scheduler is overloaded, please retry later",
            ));
        }

        let build_id = Uuid::new_v4();
        let (reply_tx, reply_rx) = oneshot::channel();

        let options = BuildOptions {
            max_silent_time: req.max_silent_time,
            build_timeout: req.build_timeout,
            build_cores: req.build_cores,
        };

        let cmd = ActorCommand::MergeDag {
            build_id,
            tenant_id: if req.tenant_id.is_empty() {
                None
            } else {
                Some(req.tenant_id)
            },
            priority_class: if req.priority_class.is_empty() {
                "scheduled".to_string()
            } else {
                req.priority_class
            },
            nodes: req.nodes,
            edges: req.edges,
            options,
            keep_going: req.keep_going,
            reply: reply_tx,
        };

        self.actor
            .send(cmd)
            .await
            .map_err(Self::actor_error_to_status)?;

        let broadcast_rx = reply_rx
            .await
            .map_err(|_| Status::internal("actor dropped reply channel"))?
            .map_err(Self::actor_error_to_status)?;

        // Bridge broadcast::Receiver to mpsc::Receiver for tonic streaming
        let (tx, rx) = mpsc::channel(256);
        let mut broadcast_rx = broadcast_rx;

        tokio::spawn(async move {
            loop {
                match broadcast_rx.recv().await {
                    Ok(event) => {
                        if tx.send(Ok(event)).await.is_err() {
                            break; // Client disconnected
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        break; // Build completed
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(
                            lagged = n,
                            "build event subscriber lagged, some events lost"
                        );
                    }
                }
            }
        });

        info!(build_id = %build_id, "build submitted");
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    type WatchBuildStream = ReceiverStream<Result<rio_proto::types::BuildEvent, Status>>;

    #[instrument(skip(self, request), fields(rpc = "WatchBuild"))]
    async fn watch_build(
        &self,
        request: Request<rio_proto::types::WatchBuildRequest>,
    ) -> Result<Response<Self::WatchBuildStream>, Status> {
        self.check_actor_alive()?;
        let req = request.into_inner();
        let build_id: Uuid = req
            .build_id
            .parse()
            .map_err(|_| Status::invalid_argument("invalid build_id UUID"))?;

        let (reply_tx, reply_rx) = oneshot::channel();

        let cmd = ActorCommand::WatchBuild {
            build_id,
            since_sequence: req.since_sequence,
            reply: reply_tx,
        };

        self.actor
            .send(cmd)
            .await
            .map_err(Self::actor_error_to_status)?;

        let broadcast_rx = reply_rx
            .await
            .map_err(|_| Status::internal("actor dropped reply channel"))?
            .map_err(Self::actor_error_to_status)?;

        // Bridge broadcast to mpsc
        let (tx, rx) = mpsc::channel(256);
        let mut broadcast_rx = broadcast_rx;

        tokio::spawn(async move {
            loop {
                match broadcast_rx.recv().await {
                    Ok(event) => {
                        if tx.send(Ok(event)).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(lagged = n, "WatchBuild subscriber lagged, some events lost");
                    }
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    #[instrument(skip(self, request), fields(rpc = "QueryBuildStatus"))]
    async fn query_build_status(
        &self,
        request: Request<rio_proto::types::QueryBuildRequest>,
    ) -> Result<Response<rio_proto::types::BuildStatus>, Status> {
        self.check_actor_alive()?;
        let req = request.into_inner();
        let build_id: Uuid = req
            .build_id
            .parse()
            .map_err(|_| Status::invalid_argument("invalid build_id UUID"))?;

        let (reply_tx, reply_rx) = oneshot::channel();

        let cmd = ActorCommand::QueryBuildStatus {
            build_id,
            reply: reply_tx,
        };

        self.actor
            .send(cmd)
            .await
            .map_err(Self::actor_error_to_status)?;

        let status = reply_rx
            .await
            .map_err(|_| Status::internal("actor dropped reply channel"))?
            .map_err(Self::actor_error_to_status)?;

        Ok(Response::new(status))
    }

    #[instrument(skip(self, request), fields(rpc = "CancelBuild"))]
    async fn cancel_build(
        &self,
        request: Request<rio_proto::types::CancelBuildRequest>,
    ) -> Result<Response<rio_proto::types::CancelBuildResponse>, Status> {
        self.check_actor_alive()?;
        let req = request.into_inner();
        let build_id: Uuid = req
            .build_id
            .parse()
            .map_err(|_| Status::invalid_argument("invalid build_id UUID"))?;

        let (reply_tx, reply_rx) = oneshot::channel();

        let cmd = ActorCommand::CancelBuild {
            build_id,
            reason: req.reason,
            reply: reply_tx,
        };

        self.actor
            .send(cmd)
            .await
            .map_err(Self::actor_error_to_status)?;

        let cancelled = reply_rx
            .await
            .map_err(|_| Status::internal("actor dropped reply channel"))?
            .map_err(Self::actor_error_to_status)?;

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
                worker_id: worker_id.clone(),
                stream_tx: actor_tx,
            })
            .await
            .map_err(|_| Status::unavailable("scheduler actor unavailable"))?;

        // Bridge actor_rx -> output_tx, wrapping in Ok()
        tokio::spawn(async move {
            while let Some(msg) = actor_rx.recv().await {
                if output_tx.send(Ok(msg)).await.is_err() {
                    break;
                }
            }
        });

        // Spawn a task to read worker messages and forward to the actor
        let actor_for_recv = self.actor.clone();
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
                                    worker_id: worker_id_for_recv.clone(),
                                    drv_hash: drv_path,
                                    result,
                                })
                                .await
                                .is_err()
                            {
                                warn!("actor channel closed while sending completion");
                                break;
                            }
                        }
                        rio_proto::types::worker_message::Msg::LogBatch(_log) => {
                            // TODO: buffer and forward build logs
                        }
                        rio_proto::types::worker_message::Msg::Progress(_progress) => {
                            // TODO: forward progress updates
                        }
                    }
                }
            }

            // Stream closed: worker disconnected. Use blocking send — if this
            // is dropped due to backpressure, running derivations won't be
            // reassigned and will hang forever.
            if actor_for_recv
                .send_unchecked(ActorCommand::WorkerDisconnected {
                    worker_id: worker_id_for_recv,
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
        self.check_actor_alive()?;
        let req = request.into_inner();

        if req.worker_id.is_empty() {
            return Err(Status::invalid_argument("worker_id is required"));
        }

        let cmd = ActorCommand::Heartbeat {
            worker_id: req.worker_id,
            system: req.system,
            supported_features: req.supported_features,
            max_builds: req.max_builds,
            running_builds: req.running_builds,
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
            generation: 1, // TODO: actual leader generation
        }))
    }
}
