use rio_common::proto::{
    build_service_server::{BuildService, BuildServiceServer},
    BuilderStatus, ExecuteBuildRequest, ExecuteBuildResponse, GetBuilderStatusRequest,
    HeartbeatRequest, HeartbeatResponse, RegisterBuilderRequest, RegisterBuilderResponse,
};
use rio_common::BuilderId;
use tonic::{transport::Server, Request, Response, Status};
use tracing::{error, info};

use crate::builder_pool::BuilderPool;

pub struct BuildServiceImpl {
    builder_pool: BuilderPool,
}

impl BuildServiceImpl {
    pub fn new(builder_pool: BuilderPool) -> Self {
        Self { builder_pool }
    }
}

#[tonic::async_trait]
impl BuildService for BuildServiceImpl {
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
            .register_builder(
                builder_id,
                req.endpoint,
                req.platforms,
                req.features,
            )
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

    type HeartbeatStream = tokio_stream::wrappers::ReceiverStream<Result<HeartbeatResponse, Status>>;

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

    async fn execute_build(
        &self,
        _request: Request<ExecuteBuildRequest>,
    ) -> Result<Response<Self::ExecuteBuildStream>, Status> {
        // TODO: Implement build execution
        Err(Status::unimplemented(
            "Build execution not yet implemented",
        ))
    }

    async fn get_builder_status(
        &self,
        request: Request<GetBuilderStatusRequest>,
    ) -> Result<Response<BuilderStatus>, Status> {
        let builder_id = BuilderId::from_string(request.into_inner().builder_id);

        match self.builder_pool.get_builder(&builder_id).await {
            Some(builder_info) => Ok(Response::new(builder_info.status)),
            None => Err(Status::not_found("Builder not found")),
        }
    }
}

/// Start the gRPC server on the specified address
pub async fn start_grpc_server(
    addr: std::net::SocketAddr,
    builder_pool: BuilderPool,
) -> anyhow::Result<()> {
    let service = BuildServiceImpl::new(builder_pool);

    info!("Starting gRPC server on {}", addr);

    Server::builder()
        .add_service(BuildServiceServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}
