// gRPC server for receiving build jobs from dispatcher

use crate::executor::Executor;
use rio_common::proto::{
    BuilderStatus, ExecuteBuildRequest, ExecuteBuildResponse, GetBuilderStatusRequest,
    HeartbeatRequest, HeartbeatResponse, RegisterBuilderRequest, RegisterBuilderResponse,
    build_service_server::{BuildService, BuildServiceServer},
};
use tonic::{Request, Response, Status, transport::Server};
use tracing::{info, warn};

#[allow(dead_code)]
pub struct BuilderServiceImpl {
    executor: Executor,
}

#[allow(dead_code)]
impl BuilderServiceImpl {
    pub fn new(executor: Executor) -> Self {
        Self { executor }
    }
}

#[tonic::async_trait]
impl BuildService for BuilderServiceImpl {
    async fn register_builder(
        &self,
        _request: Request<RegisterBuilderRequest>,
    ) -> Result<Response<RegisterBuilderResponse>, Status> {
        // Builders don't handle registration requests - they initiate them
        Err(Status::unimplemented(
            "Builders don't accept registration requests",
        ))
    }

    type HeartbeatStream =
        tokio_stream::wrappers::ReceiverStream<Result<HeartbeatResponse, Status>>;

    async fn heartbeat(
        &self,
        _request: Request<tonic::Streaming<HeartbeatRequest>>,
    ) -> Result<Response<Self::HeartbeatStream>, Status> {
        // Builders don't handle heartbeat streams - they send them
        Err(Status::unimplemented(
            "Builders don't accept heartbeat streams",
        ))
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

        let (tx, rx) = tokio::sync::mpsc::channel(100);

        // Execute build in background
        let executor = self.executor.clone();
        tokio::spawn(async move {
            match executor.execute_build(req).await {
                Ok(responses) => {
                    // Send all responses
                    for response in responses {
                        if tx.send(Ok(response)).await.is_err() {
                            warn!("Client disconnected during build");
                            break;
                        }
                    }
                }
                Err(e) => {
                    warn!("Build execution failed: {:?}", e);
                    // TODO: Send BuildFailed response
                }
            }
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }

    async fn get_builder_status(
        &self,
        _request: Request<GetBuilderStatusRequest>,
    ) -> Result<Response<BuilderStatus>, Status> {
        // TODO: Return actual builder status
        Err(Status::unimplemented("Builder status not yet implemented"))
    }
}

/// Start the gRPC server on the specified address
#[allow(dead_code)]
pub async fn start_grpc_server(
    addr: std::net::SocketAddr,
    executor: Executor,
) -> anyhow::Result<()> {
    let service = BuilderServiceImpl::new(executor);

    info!("Starting builder gRPC server on {}", addr);

    Server::builder()
        .add_service(BuildServiceServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}
