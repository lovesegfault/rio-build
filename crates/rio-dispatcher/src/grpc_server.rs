use rio_common::BuilderId;
use rio_common::proto::{
    BuilderStatus, ExecuteBuildRequest, ExecuteBuildResponse, GetBuilderStatusRequest,
    HeartbeatRequest, HeartbeatResponse, RegisterBuilderRequest, RegisterBuilderResponse,
    build_service_server::{BuildService, BuildServiceServer},
};
use tonic::{Request, Response, Status, transport::Server};
use tracing::{error, info};

use crate::build_queue::{BuildJob, BuildQueue};
use crate::builder_pool::BuilderPool;
use crate::scheduler::Scheduler;

pub struct BuildServiceImpl {
    pub(crate) builder_pool: BuilderPool,
    pub(crate) build_queue: BuildQueue,
    pub(crate) scheduler: Scheduler,
}

impl BuildServiceImpl {
    pub fn new(builder_pool: BuilderPool, build_queue: BuildQueue, scheduler: Scheduler) -> Self {
        Self {
            builder_pool,
            build_queue,
            scheduler,
        }
    }
}

#[tonic::async_trait]
impl BuildService for BuildServiceImpl {
    #[tracing::instrument(skip(self, request), fields(builder_id = request.get_ref().builder_id.as_str()))]
    async fn register_builder(
        &self,
        request: Request<RegisterBuilderRequest>,
    ) -> Result<Response<RegisterBuilderResponse>, Status> {
        let req = request.into_inner();

        info!(
            "Received builder registration request: id={}, platforms={:?}",
            req.builder_id, req.platforms
        );

        let builder_id = BuilderId::from_string(req.builder_id);

        match self
            .builder_pool
            .register_builder(builder_id, req.endpoint, req.platforms, req.features)
            .await
        {
            Ok(()) => Ok(Response::new(RegisterBuilderResponse {
                success: true,
                message: "Builder registered successfully".to_string(),
            })),
            Err(e) => {
                error!("Failed to register builder: {}", e);
                Ok(Response::new(RegisterBuilderResponse {
                    success: false,
                    message: format!("Registration failed: {}", e),
                }))
            }
        }
    }

    type HeartbeatStream =
        tokio_stream::wrappers::ReceiverStream<Result<HeartbeatResponse, Status>>;

    async fn heartbeat(
        &self,
        request: Request<tonic::Streaming<HeartbeatRequest>>,
    ) -> Result<Response<Self::HeartbeatStream>, Status> {
        let mut stream = request.into_inner();
        let (tx, rx) = tokio::sync::mpsc::channel(100);

        tokio::spawn(async move {
            while let Ok(Some(heartbeat)) = stream.message().await {
                info!(
                    "Received heartbeat from builder {}: load={}",
                    heartbeat.builder_id, heartbeat.current_load
                );

                // TODO: Update builder status in pool
                // TODO: Send back any commands/config updates

                let response = HeartbeatResponse { commands: vec![] };

                if tx.send(Ok(response)).await.is_err() {
                    break;
                }
            }
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }

    type ExecuteBuildStream =
        tokio_stream::wrappers::ReceiverStream<Result<ExecuteBuildResponse, Status>>;

    #[tracing::instrument(skip(self, request), fields(job_id = request.get_ref().job_id.as_str()))]
    async fn execute_build(
        &self,
        request: Request<ExecuteBuildRequest>,
    ) -> Result<Response<Self::ExecuteBuildStream>, Status> {
        let req = request.into_inner();
        info!("Received build request: job_id={}", req.job_id);

        // TODO: Parse derivation to extract platform
        // For now, assume x86_64-linux
        let platform = "x86_64-linux".to_string();

        // Create build job
        let job = BuildJob::new(format!("/nix/store/{}.drv", req.job_id), platform);

        // Enqueue the job
        let job_id = self.build_queue.enqueue(job.clone()).await;
        info!("Enqueued build job {}", job_id);

        // Select a builder
        match self.scheduler.select_builder(&job).await {
            Some(builder_id) => {
                info!("Selected builder {} for job {}", builder_id, job_id);

                // TODO: Actually send job to builder via gRPC client
                // TODO: Stream build logs back

                // For now, return success immediately
                let (tx, rx) = tokio::sync::mpsc::channel(10);

                tokio::spawn(async move {
                    // Placeholder: send a completion message
                    use rio_common::proto::{BuildCompleted, ExecuteBuildResponse};
                    let _ = tx
                        .send(Ok(ExecuteBuildResponse {
                            job_id: job_id.to_string(),
                            update: Some(
                                rio_common::proto::execute_build_response::Update::Completed(
                                    BuildCompleted {
                                        output_paths: vec![],
                                        duration_ms: 0,
                                    },
                                ),
                            ),
                        }))
                        .await;
                });

                Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
                    rx,
                )))
            }
            None => {
                error!("No builder available for job {}", job_id);
                Err(Status::unavailable("No builder available"))
            }
        }
    }

    async fn get_builder_status(
        &self,
        request: Request<GetBuilderStatusRequest>,
    ) -> Result<Response<BuilderStatus>, Status> {
        let builder_id = BuilderId::from_string(request.into_inner().builder_id);

        match self.builder_pool.get_builder(&builder_id).await {
            Some(builder_info) => Ok(Response::new(BuilderStatus {
                builder_id: builder_info.id.to_string(),
                state: builder_info.status.state,
                capacity: builder_info.status.capacity,
                available_capacity: builder_info.status.available_capacity,
                current_jobs: builder_info.status.current_jobs,
                total_builds: builder_info.status.total_builds,
                successful_builds: builder_info.status.successful_builds,
                failed_builds: builder_info.status.failed_builds,
            })),
            None => Err(Status::not_found("Builder not found")),
        }
    }
}

/// Start the gRPC server on the specified address
pub async fn start_grpc_server(
    addr: std::net::SocketAddr,
    builder_pool: BuilderPool,
    build_queue: BuildQueue,
    scheduler: Scheduler,
) -> anyhow::Result<()> {
    let service = BuildServiceImpl::new(builder_pool, build_queue, scheduler);

    info!("Starting gRPC server on {}", addr);

    Server::builder()
        .add_service(BuildServiceServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}
