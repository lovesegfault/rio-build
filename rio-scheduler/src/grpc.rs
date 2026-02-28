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

        // Validate DAG nodes before passing to the actor. Proto types have
        // all-public fields with no validation; an empty drv_hash would
        // become a DAG primary key, empty drv_path breaks the reverse
        // index, and empty system never matches any worker (derivation
        // stuck in Ready forever). Bound node count to protect memory.
        if req.nodes.len() > rio_common::limits::MAX_DAG_NODES {
            return Err(Status::invalid_argument(format!(
                "too many nodes: {} (max {})",
                req.nodes.len(),
                rio_common::limits::MAX_DAG_NODES
            )));
        }
        if req.edges.len() > rio_common::limits::MAX_DAG_EDGES {
            return Err(Status::invalid_argument(format!(
                "too many edges: {} (max {})",
                req.edges.len(),
                rio_common::limits::MAX_DAG_EDGES
            )));
        }
        for node in &req.nodes {
            if node.drv_hash.is_empty() {
                return Err(Status::invalid_argument("node drv_hash must be non-empty"));
            }
            if node.drv_path.is_empty() {
                return Err(Status::invalid_argument(format!(
                    "node {} drv_path must be non-empty",
                    node.drv_hash
                )));
            }
            if node.system.is_empty() {
                return Err(Status::invalid_argument(format!(
                    "node {} system must be non-empty",
                    node.drv_hash
                )));
            }
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

        rio_common::task::spawn_monitored("submit-build-bridge", async move {
            loop {
                match broadcast_rx.recv().await {
                    Ok(event) => {
                        if tx.send(Ok(event)).await.is_err() {
                            break; // Client disconnected
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        break; // Build terminal (sender dropped) or actor shut down
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(
                            lagged = n,
                            "build event subscriber lagged, some events lost"
                        );
                        // Lagged means we permanently missed n events. If
                        // BuildCompleted was among them, the client would hang
                        // forever. Send DATA_LOSS so the gateway fails cleanly
                        // instead of silently hanging.
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

        rio_common::task::spawn_monitored("watch-build-bridge", async move {
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
                        // Send DATA_LOSS so the client fails cleanly instead
                        // of silently hanging on a missed terminal event.
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
                worker_id: worker_id.clone().into(),
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
                                })
                                .await
                                .is_err()
                            {
                                warn!("actor channel closed while sending completion");
                                break;
                            }
                        }
                        rio_proto::types::worker_message::Msg::LogBatch(_log) => {
                            // TODO(phase2b): buffer and forward build logs to gateway.
                            // Phase 2b spec: 64-line/100ms batching, per-derivation ring
                            // buffer, async S3 flush on completion.
                        }
                        rio_proto::types::worker_message::Msg::Progress(_progress) => {
                            // TODO(phase2b): forward progress updates (resource usage,
                            // build phase) via OpenTelemetry trace propagation.
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
        if req.supported_features.len() > MAX_HEARTBEAT_FEATURES {
            return Err(Status::invalid_argument(format!(
                "heartbeat supported_features has {} entries (max {})",
                req.supported_features.len(),
                MAX_HEARTBEAT_FEATURES
            )));
        }
        if req.running_builds.len() > MAX_HEARTBEAT_RUNNING_BUILDS {
            return Err(Status::invalid_argument(format!(
                "heartbeat running_builds has {} entries (max {})",
                req.running_builds.len(),
                MAX_HEARTBEAT_RUNNING_BUILDS
            )));
        }

        let cmd = ActorCommand::Heartbeat {
            worker_id: req.worker_id.into(),
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
mod tests {
    use super::*;
    use crate::actor::tests::{make_test_node, setup_actor};
    use rio_proto::scheduler::scheduler_service_server::SchedulerServiceServer;
    use rio_proto::worker::worker_service_client::WorkerServiceClient;
    use rio_proto::worker::worker_service_server::WorkerServiceServer;
    use rio_test_support::TestDb;
    use std::time::Duration;
    use tokio_stream::StreamExt;

    static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../migrations");

    /// T6: End-to-end BuildExecution bidirectional stream.
    ///
    /// Spins up an in-process WorkerServiceServer backed by a real actor,
    /// connects a mock worker via gRPC, sends WorkerRegister + Heartbeat,
    /// submits a build via SchedulerService, receives WorkAssignment on the
    /// stream, sends CompletionReport, verifies build completes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_build_execution_stream_end_to_end() {
        let db = TestDb::new(&MIGRATOR).await;
        let (handle, _actor_task) = setup_actor(db.pool.clone()).await;

        // Spin up in-process gRPC server (SchedulerService + WorkerService).
        let grpc = SchedulerGrpc::new(handle.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

        let _server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(SchedulerServiceServer::new(grpc.clone()))
                .add_service(WorkerServiceServer::new(grpc))
                .serve_with_incoming(incoming)
                .await
                .expect("test gRPC server should run");
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let endpoint = format!("http://{addr}");
        let channel = tonic::transport::Channel::from_shared(endpoint)
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut worker_client = WorkerServiceClient::new(channel.clone());
        let mut sched_client =
            rio_proto::scheduler::scheduler_service_client::SchedulerServiceClient::new(channel);

        // Open BuildExecution stream. First message MUST be WorkerRegister.
        let (stream_tx, stream_rx) = mpsc::channel::<rio_proto::types::WorkerMessage>(32);
        stream_tx
            .send(rio_proto::types::WorkerMessage {
                msg: Some(rio_proto::types::worker_message::Msg::Register(
                    rio_proto::types::WorkerRegister {
                        worker_id: "e2e-worker".into(),
                    },
                )),
            })
            .await
            .unwrap();

        let outbound = tokio_stream::wrappers::ReceiverStream::new(stream_rx);
        let mut inbound = worker_client
            .build_execution(outbound)
            .await
            .expect("BuildExecution stream should open")
            .into_inner();

        // Send Heartbeat to fully register (stream + heartbeat).
        worker_client
            .heartbeat(rio_proto::types::HeartbeatRequest {
                worker_id: "e2e-worker".into(),
                system: "x86_64-linux".into(),
                supported_features: vec![],
                max_builds: 1,
                running_builds: vec![],
                resources: None,
                local_paths: None,
            })
            .await
            .expect("heartbeat should succeed");

        // Submit a build via SchedulerService.
        let submit_req = rio_proto::types::SubmitBuildRequest {
            tenant_id: "".into(),
            priority_class: "scheduled".into(),
            nodes: vec![make_test_node(
                "e2e-hash",
                "/nix/store/e2e-hash.drv",
                "x86_64-linux",
            )],
            edges: vec![],
            max_silent_time: 0,
            build_timeout: 0,
            build_cores: 0,
            keep_going: false,
        };
        let mut event_stream = sched_client
            .submit_build(submit_req)
            .await
            .expect("SubmitBuild should succeed")
            .into_inner();

        // Worker should receive WorkAssignment on the BuildExecution stream.
        let assignment = tokio::time::timeout(Duration::from_secs(5), inbound.next())
            .await
            .expect("assignment should arrive within 5s")
            .expect("stream should not close")
            .expect("assignment should not be an error");
        let work = match assignment.msg {
            Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) => a,
            other => panic!("expected WorkAssignment, got {other:?}"),
        };
        assert_eq!(work.drv_path, "/nix/store/e2e-hash.drv");

        // Send CompletionReport back on the stream.
        stream_tx
            .send(rio_proto::types::WorkerMessage {
                msg: Some(rio_proto::types::worker_message::Msg::Completion(
                    rio_proto::types::CompletionReport {
                        drv_path: work.drv_path.clone(),
                        result: Some(rio_proto::types::BuildResult {
                            status: rio_proto::types::BuildResultStatus::Built.into(),
                            error_msg: "".into(),
                            times_built: 1,
                            start_time: None,
                            stop_time: None,
                            built_outputs: vec![rio_proto::types::BuiltOutput {
                                output_name: "out".into(),
                                output_path: "/nix/store/e2e-output".into(),
                                output_hash: vec![0u8; 32],
                            }],
                        }),
                        assignment_token: work.assignment_token.clone(),
                    },
                )),
            })
            .await
            .expect("completion send should succeed");

        // Build event stream should emit BuildCompleted.
        let mut saw_completed = false;
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(5), event_stream.next()).await;
            match ev {
                Ok(Some(Ok(event))) => {
                    if let Some(rio_proto::types::build_event::Event::Completed(_)) = event.event {
                        saw_completed = true;
                        break;
                    }
                }
                Ok(Some(Err(e))) => panic!("event stream error: {e}"),
                Ok(None) => break, // stream closed
                Err(_) => panic!("timed out waiting for BuildCompleted"),
            }
        }
        assert!(
            saw_completed,
            "BuildCompleted event should be emitted after worker sends CompletionReport"
        );
    }

    /// SubmitBuild with an empty drv_hash in a node should be rejected at
    /// the gRPC boundary (proto types have no validation; an empty hash
    /// would become a DAG primary key).
    #[tokio::test]
    async fn test_submit_build_rejects_empty_drv_hash() {
        let db = TestDb::new(&MIGRATOR).await;
        let (handle, _task) = setup_actor(db.pool.clone()).await;
        let grpc = SchedulerGrpc::new(handle);

        let mut bad_node = make_test_node("h", "/nix/store/h.drv", "x86_64-linux");
        bad_node.drv_hash = String::new(); // empty!

        let req = Request::new(rio_proto::types::SubmitBuildRequest {
            nodes: vec![bad_node],
            edges: vec![],
            ..Default::default()
        });

        let result = grpc.submit_build(req).await;
        assert!(result.is_err(), "empty drv_hash should be rejected");
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(
            status.message().contains("drv_hash"),
            "error should mention drv_hash: {}",
            status.message()
        );
    }

    /// SubmitBuild with an empty drv_path should be rejected.
    #[tokio::test]
    async fn test_submit_build_rejects_empty_drv_path() {
        let db = TestDb::new(&MIGRATOR).await;
        let (handle, _task) = setup_actor(db.pool.clone()).await;
        let grpc = SchedulerGrpc::new(handle);

        let mut bad_node = make_test_node("h", "/nix/store/h.drv", "x86_64-linux");
        bad_node.drv_path = String::new(); // empty!

        let req = Request::new(rio_proto::types::SubmitBuildRequest {
            nodes: vec![bad_node],
            edges: vec![],
            ..Default::default()
        });

        let result = grpc.submit_build(req).await;
        assert!(result.is_err(), "empty drv_path should be rejected");
        assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
    }

    /// SubmitBuild with an empty system should be rejected. An empty system
    /// never matches any worker's system (e.g., "x86_64-linux"), so the
    /// derivation would sit in Ready forever with no feedback.
    #[tokio::test]
    async fn test_submit_build_rejects_empty_system() {
        let db = TestDb::new(&MIGRATOR).await;
        let (handle, _task) = setup_actor(db.pool.clone()).await;
        let grpc = SchedulerGrpc::new(handle);

        let mut bad_node = make_test_node("h", "/nix/store/h.drv", "x86_64-linux");
        bad_node.system = String::new(); // empty!

        let req = Request::new(rio_proto::types::SubmitBuildRequest {
            nodes: vec![bad_node],
            edges: vec![],
            ..Default::default()
        });

        let result = grpc.submit_build(req).await;
        assert!(result.is_err(), "empty system should be rejected");
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(
            status.message().contains("system"),
            "error should mention system: {}",
            status.message()
        );
    }

    /// SubmitBuild with an unrecognized priority_class should be rejected
    /// at the gRPC boundary (PriorityClass::FromStr). Previously this leaked
    /// as a PostgreSQL CHECK constraint violation in Status::internal.
    #[tokio::test]
    async fn test_submit_build_rejects_invalid_priority_class() {
        let db = TestDb::new(&MIGRATOR).await;
        let (handle, _task) = setup_actor(db.pool.clone()).await;
        let grpc = SchedulerGrpc::new(handle);

        let req = Request::new(rio_proto::types::SubmitBuildRequest {
            nodes: vec![make_test_node("h", "/nix/store/h.drv", "x86_64-linux")],
            edges: vec![],
            priority_class: "urgent".into(), // not in {ci, interactive, scheduled}
            ..Default::default()
        });

        let result = grpc.submit_build(req).await;
        assert!(result.is_err(), "invalid priority_class should be rejected");
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(
            status.message().contains("priority_class"),
            "error should mention priority_class: {}",
            status.message()
        );
    }

    /// SubmitBuild with a non-empty tenant_id that's not a valid UUID should
    /// be rejected at the gRPC boundary, not leak as a PostgreSQL cast error.
    #[tokio::test]
    async fn test_submit_build_rejects_invalid_tenant_id() {
        let db = TestDb::new(&MIGRATOR).await;
        let (handle, _task) = setup_actor(db.pool.clone()).await;
        let grpc = SchedulerGrpc::new(handle);

        let req = Request::new(rio_proto::types::SubmitBuildRequest {
            nodes: vec![make_test_node("h", "/nix/store/h.drv", "x86_64-linux")],
            edges: vec![],
            tenant_id: "customer-foo".into(), // not a UUID
            ..Default::default()
        });

        let result = grpc.submit_build(req).await;
        assert!(result.is_err(), "invalid tenant_id should be rejected");
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(
            status.message().contains("tenant_id"),
            "error should mention tenant_id: {}",
            status.message()
        );
    }

    /// SubmitBuild with more edges than MAX_DAG_EDGES should be rejected
    /// (DoS prevention: O(edges) merge loop).
    #[tokio::test]
    async fn test_submit_build_rejects_too_many_edges() {
        let db = TestDb::new(&MIGRATOR).await;
        let (handle, _task) = setup_actor(db.pool.clone()).await;
        let grpc = SchedulerGrpc::new(handle);

        // Construct MAX_DAG_EDGES+1 edges. Content doesn't matter — rejection
        // happens before any path validation.
        let too_many: Vec<_> = (0..rio_common::limits::MAX_DAG_EDGES + 1)
            .map(|i| rio_proto::types::DerivationEdge {
                parent_drv_path: format!("/nix/store/{i}-parent.drv"),
                child_drv_path: format!("/nix/store/{i}-child.drv"),
            })
            .collect();

        let req = Request::new(rio_proto::types::SubmitBuildRequest {
            nodes: vec![make_test_node("h", "/nix/store/h.drv", "x86_64-linux")],
            edges: too_many,
            ..Default::default()
        });

        let result = grpc.submit_build(req).await;
        assert!(result.is_err(), "too many edges should be rejected");
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(
            status.message().contains("edges"),
            "error should mention edges: {}",
            status.message()
        );
    }

    /// Heartbeat with too many running_builds entries should be rejected.
    /// Heartbeats bypass backpressure (send_unchecked), so unbounded payload
    /// would stall the actor event loop with no backpressure signal.
    #[tokio::test]
    async fn test_heartbeat_rejects_too_many_running_builds() {
        let db = TestDb::new(&MIGRATOR).await;
        let (handle, _task) = setup_actor(db.pool.clone()).await;
        let grpc = SchedulerGrpc::new(handle);

        let too_many: Vec<String> = (0..1001).map(|i| format!("/nix/store/{i}.drv")).collect();

        let req = Request::new(rio_proto::types::HeartbeatRequest {
            worker_id: "test-worker".into(),
            system: "x86_64-linux".into(),
            supported_features: vec![],
            max_builds: 1,
            running_builds: too_many,
            resources: None,
            local_paths: None,
        });

        let result = grpc.heartbeat(req).await;
        assert!(
            result.is_err(),
            "heartbeat with >1000 running_builds should be rejected"
        );
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(
            status.message().contains("running_builds"),
            "error should mention running_builds: {}",
            status.message()
        );
    }
}
