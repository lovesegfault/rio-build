//! gRPC server implementation for Rio agent

use anyhow::{Context, Result};
use rio_common::proto::rio_agent_server::{RioAgent, RioAgentServer};
use rio_common::proto::*;
use std::sync::Arc;
use tonic::transport::Server;
use tonic::{Request, Response, Status};

use crate::agent::Agent;
use crate::builder;

/// gRPC service implementation
pub struct RioAgentService {
    agent: Arc<Agent>,
}

impl RioAgentService {
    /// Create a new RioAgentService
    pub fn new(agent: Agent) -> Self {
        Self {
            agent: Arc::new(agent),
        }
    }
}

#[tonic::async_trait]
impl RioAgent for RioAgentService {
    /// Queue a build (Phase 1: stores drv, starts build, returns BuildAssigned)
    async fn queue_build(
        &self,
        request: Request<QueueBuildRequest>,
    ) -> Result<Response<QueueBuildResponse>, Status> {
        let req = request.into_inner();

        tracing::info!(
            "Received build request for {} (platform: {}, features: {:?})",
            req.derivation_path,
            req.platform,
            req.required_features
        );

        // Start the build
        builder::start_build(&self.agent, req.derivation_path.clone(), req.derivation)
            .await
            .map_err(|e| Status::internal(format!("Failed to start build: {}", e)))?;

        // Return BuildAssigned response
        let response = QueueBuildResponse {
            result: Some(queue_build_response::Result::Assigned(BuildAssigned {
                agent_id: self.agent.id.to_string(),
                derivation_path: req.derivation_path,
            })),
        };

        Ok(Response::new(response))
    }

    type SubscribeToBuildStream =
        tokio_stream::wrappers::ReceiverStream<Result<BuildUpdate, Status>>;

    /// Subscribe to build updates
    async fn subscribe_to_build(
        &self,
        request: Request<SubscribeToBuildRequest>,
    ) -> Result<Response<Self::SubscribeToBuildStream>, Status> {
        let req = request.into_inner();

        tracing::info!("Client subscribing to build: {}", req.derivation_path);

        // Create a channel for this subscriber
        let (tx, rx) = tokio::sync::mpsc::channel(100);

        // Add subscriber to current build
        let mut current = self.agent.current_build.lock().await;
        if let Some(ref mut build) = *current {
            if build.drv_path.as_str() == req.derivation_path {
                build.subscribers.push(tx);
            } else {
                return Err(Status::not_found("Different build in progress"));
            }
        } else {
            return Err(Status::not_found("No build in progress"));
        }

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }

    /// Get cluster members (Phase 1: unimplemented)
    async fn get_cluster_members(
        &self,
        _request: Request<GetClusterMembersRequest>,
    ) -> Result<Response<ClusterMembers>, Status> {
        Err(Status::unimplemented("Phase 1: No cluster support yet"))
    }

    type GetCompletedBuildStream =
        tokio_stream::wrappers::ReceiverStream<Result<BuildUpdate, Status>>;

    /// Get completed build outputs (Phase 1: unimplemented)
    async fn get_completed_build(
        &self,
        _request: Request<GetCompletedBuildRequest>,
    ) -> Result<Response<Self::GetCompletedBuildStream>, Status> {
        Err(Status::unimplemented("Phase 1: No build caching yet"))
    }

    /// Get build status (Phase 1: unimplemented)
    async fn get_build_status(
        &self,
        _request: Request<GetBuildStatusRequest>,
    ) -> Result<Response<BuildStatusResponse>, Status> {
        Err(Status::unimplemented("Phase 1: No status queries yet"))
    }

    /// Join cluster (Phase 1: unimplemented)
    async fn join_cluster(
        &self,
        _request: Request<JoinClusterRequest>,
    ) -> Result<Response<JoinClusterResponse>, Status> {
        Err(Status::unimplemented("Phase 1: No cluster support yet"))
    }

    /// Fetch pending build (Phase 1: unimplemented)
    async fn fetch_pending_build(
        &self,
        _request: Request<FetchPendingBuildRequest>,
    ) -> Result<Response<FetchPendingBuildResponse>, Status> {
        Err(Status::unimplemented(
            "Phase 1: No agent-to-agent communication yet",
        ))
    }
}

/// Start the gRPC server
pub async fn serve(listen_addr: String, agent: Agent) -> Result<()> {
    let addr = listen_addr
        .parse()
        .with_context(|| format!("Invalid listen address: {}", listen_addr))?;

    let agent = Arc::new(agent);
    let service = RioAgentService {
        agent: agent.clone(),
    };

    tracing::info!("gRPC server listening on {}", addr);

    Server::builder()
        .add_service(RioAgentServer::new(service))
        .serve(addr)
        .await
        .context("gRPC server error")?;

    Ok(())
}
