//! gRPC server implementation for Rio agent

use anyhow::{Context, Result};
use rio_common::proto::rio_agent_server::{RioAgent, RioAgentServer};
use rio_common::proto::*;
use std::sync::Arc;
use tonic::transport::Server;
use tonic::{Request, Response, Status};

use crate::agent::Agent;

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
    /// Queue a build (Phase 3: Raft-coordinated assignment)
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

        // Get Raft instance (required)
        let raft = self.agent.raft.as_ref().ok_or_else(|| {
            Status::failed_precondition("Agent not in Raft mode - use --bootstrap")
        })?;

        let sm_store = self
            .agent
            .state_machine
            .as_ref()
            .ok_or_else(|| Status::internal("State machine not initialized"))?;

        // Check if build already in progress or completed
        let drv_path: camino::Utf8PathBuf = req.derivation_path.clone().into();

        let check_result = {
            let cluster_state = &sm_store.data.read().cluster;

            if let Some(tracker) = cluster_state.builds_in_progress.get(&drv_path) {
                Some(queue_build_response::Result::AlreadyBuilding(
                    AlreadyBuilding {
                        agent_id: tracker.agent_id.to_string(),
                        derivation_path: req.derivation_path.clone(),
                    },
                ))
            } else if let Some(completed) = cluster_state.completed_builds.get(&drv_path) {
                Some(queue_build_response::Result::AlreadyCompleted(
                    AlreadyCompleted {
                        agent_id: completed.agent_id.to_string(),
                        derivation_path: req.derivation_path.clone(),
                    },
                ))
            } else {
                None
            }
        }; // Read lock released here

        // Return early if build already exists
        if let Some(result) = check_result {
            let response = QueueBuildResponse {
                result: Some(result),
            };
            return Ok(Response::new(response));
        }

        // Propose BuildQueued to Raft
        let cmd = crate::state_machine::RaftCommand::BuildQueued {
            top_level: req.derivation_path.clone().into(),
            derivation_nar: req.derivation,
            dependencies: req.dependency_paths.iter().map(|s| s.into()).collect(),
            platform: req.platform,
            features: req.required_features,
        };

        let raft_response = raft
            .client_write(cmd)
            .await
            .map_err(|e| Status::internal(format!("Raft proposal failed: {}", e)))?;

        // Extract assignment from Raft response
        match raft_response.data {
            crate::state_machine::RaftResponse::BuildAssigned {
                agent_id,
                derivation_path,
            } => {
                tracing::info!("Build {} assigned to agent {}", derivation_path, agent_id);

                let response = QueueBuildResponse {
                    result: Some(queue_build_response::Result::Assigned(BuildAssigned {
                        agent_id: agent_id.to_string(),
                        derivation_path: derivation_path.to_string(),
                    })),
                };

                Ok(Response::new(response))
            }
            _ => Err(Status::internal("Unexpected Raft response type")),
        }
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

    /// Get cluster members
    async fn get_cluster_members(
        &self,
        _request: Request<GetClusterMembersRequest>,
    ) -> Result<Response<ClusterMembers>, Status> {
        // Check if Raft is enabled
        let raft = self
            .agent
            .raft
            .as_ref()
            .ok_or_else(|| Status::unimplemented("Raft not enabled (Phase 1 mode)"))?;

        let sm_store = self
            .agent
            .state_machine
            .as_ref()
            .ok_or_else(|| Status::internal("State machine not initialized"))?;

        // Get current metrics to determine leader
        // NodeId is now Uuid, so we can convert directly to String
        let metrics = raft.metrics().borrow().clone();
        let leader_id = metrics
            .current_leader
            .map(|uuid| uuid.to_string())
            .unwrap_or_default();

        // Query state machine for agent list
        let cluster_state = &sm_store.data.read().cluster;
        let agents: Vec<AgentInfo> = cluster_state
            .agents
            .values()
            .map(|agent| AgentInfo {
                id: agent.id.to_string(),
                address: agent.address.to_string(), // Convert Url to String for protobuf
                platforms: agent.platforms.clone(),
                features: agent.features.clone(),
                status: match agent.status {
                    crate::state_machine::AgentStatus::Available => AgentStatus::Available as i32,
                    crate::state_machine::AgentStatus::Busy => AgentStatus::Busy as i32,
                    crate::state_machine::AgentStatus::Down => AgentStatus::Down as i32,
                },
                capacity: None, // TODO: Add BuilderCapacity in Phase 3
            })
            .collect();

        let response = ClusterMembers { agents, leader_id };

        Ok(Response::new(response))
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
