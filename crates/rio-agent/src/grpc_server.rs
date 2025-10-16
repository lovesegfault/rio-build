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

        // Get Raft instance and state machine
        let raft = &self.agent.raft;
        let sm_store = &self.agent.state_machine;

        // Check if build already in progress or completed
        let drv_path: camino::Utf8PathBuf = req.derivation_path.clone().into();

        let check_result = {
            let cluster_state = &sm_store.data.read().cluster;

            if let Some(tracker) = cluster_state.builds_in_progress.get(&drv_path) {
                // Return BuildInfo with current status
                use rio_common::proto::BuildStatus as ProtoBuildStatus;

                let proto_status = match tracker.status {
                    crate::state_machine::BuildStatus::Queued => ProtoBuildStatus::Queued as i32,
                    crate::state_machine::BuildStatus::Claimed => ProtoBuildStatus::Claimed as i32,
                    crate::state_machine::BuildStatus::Building => {
                        ProtoBuildStatus::Building as i32
                    }
                };

                Some(queue_build_response::Result::BuildInfo(BuildInfo {
                    derivation_path: req.derivation_path.clone(),
                    status: proto_status,
                    agent_id: tracker.agent_id.map(|id| id.to_string()),
                    suggested_agents: tracker
                        .suggested_agents
                        .iter()
                        .map(|id| id.to_string())
                        .collect(),
                }))
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

        // Extract response from Raft
        match raft_response.data {
            crate::state_machine::RaftResponse::BuildQueued {
                derivation_path,
                suggested_agents,
            } => {
                tracing::info!(
                    "Build {} queued, {} suggested agents",
                    derivation_path,
                    suggested_agents.len()
                );

                let response = QueueBuildResponse {
                    result: Some(queue_build_response::Result::BuildInfo(BuildInfo {
                        derivation_path: derivation_path.to_string(),
                        status: rio_common::proto::BuildStatus::Queued as i32,
                        agent_id: None,
                        suggested_agents: suggested_agents
                            .iter()
                            .map(|id| id.to_string())
                            .collect(),
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
    ///
    /// Supports late joiners: sends catch-up logs before streaming live updates.
    async fn subscribe_to_build(
        &self,
        request: Request<SubscribeToBuildRequest>,
    ) -> Result<Response<Self::SubscribeToBuildStream>, Status> {
        let req = request.into_inner();
        let drv_path = req.derivation_path.clone();

        tracing::info!("Client subscribing to build: {}", drv_path);

        // Create a channel for this subscriber
        let (tx, rx) = tokio::sync::mpsc::channel(100);

        // Check if build is currently running on this agent
        let mut current = self.agent.current_build.lock().await;
        if let Some(ref mut build) = *current
            && build.drv_path.as_str() == drv_path
        {
            // Send catch-up logs first (for late joiners)
            let catch_up_logs = build.get_catch_up_logs();
            tracing::info!(
                "Sending {} catch-up log lines to late joiner",
                catch_up_logs.len()
            );

            for log in catch_up_logs {
                if tx.send(Ok(log)).await.is_err() {
                    return Err(Status::internal("Failed to send catch-up logs"));
                }
            }

            // Add subscriber for live updates
            build.subscribers.push(tx);
            drop(current); // Release lock

            return Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
                rx,
            )));
        }
        drop(current); // Release lock before querying state machine

        // Not currently building on this agent - check Raft state
        let state = self.agent.state_machine.data.read();
        let cluster = &state.cluster;

        // Convert string to DerivationPath for hashmap lookups
        let drv_path_key: rio_common::DerivationPath = drv_path.clone().into();

        // Check if build recently completed (serve from cache)
        if let Some(completed) = cluster.completed_builds.get(&drv_path_key) {
            tracing::info!(
                "Build {} already completed on agent {}, client should use GetCompletedBuild",
                drv_path,
                completed.agent_id
            );
            return Err(Status::already_exists(format!(
                "Build completed on agent {}, use GetCompletedBuild",
                completed.agent_id
            )));
        }

        // Check if build is in progress on different agent
        if let Some(tracker) = cluster.builds_in_progress.get(&drv_path_key) {
            match tracker.agent_id {
                Some(agent_id) if agent_id != self.agent.id => {
                    // Build is on a different agent
                    tracing::info!(
                        "Build {} is on agent {}, not this agent ({})",
                        drv_path,
                        agent_id,
                        self.agent.id
                    );
                    return Err(Status::failed_precondition(format!(
                        "Build is running on agent {}, not this agent",
                        agent_id
                    )));
                }
                None => {
                    // Build queued but not claimed yet
                    return Err(Status::not_found(format!(
                        "Build {} is queued but not claimed yet",
                        drv_path
                    )));
                }
                _ => {
                    // Build claimed by this agent, continue
                }
            }
        }

        // Build not found anywhere
        Err(Status::not_found("Build not found in cluster"))
    }

    /// Get cluster members
    async fn get_cluster_members(
        &self,
        _request: Request<GetClusterMembersRequest>,
    ) -> Result<Response<ClusterMembers>, Status> {
        let raft = &self.agent.raft;
        let sm_store = &self.agent.state_machine;

        // Get current metrics to determine leader
        // NodeId is now Uuid, so we can convert directly to String
        let metrics = raft.metrics().borrow().clone();
        let leader_id = metrics
            .current_leader
            .map(|uuid| uuid.to_string())
            .unwrap_or_default();

        // Query state machine for agent list
        let cluster_state = &sm_store.data.read().cluster;
        let agents = cluster_state
            .agents
            .iter()
            .map(|(agent_id, agent)| {
                let agent_info = AgentInfo {
                    id: agent.id.to_string(),
                    address: agent.address.to_string(), // Convert Url to String for protobuf
                    platforms: agent.platforms.clone(),
                    features: agent.features.clone(),
                    status: match agent.status {
                        crate::state_machine::AgentStatus::Available => {
                            AgentStatus::Available as i32
                        }
                        crate::state_machine::AgentStatus::Busy => AgentStatus::Busy as i32,
                        crate::state_machine::AgentStatus::Down => AgentStatus::Down as i32,
                    },
                    capacity: None, // TODO: Add BuilderCapacity in Phase 3
                };
                (agent_id.to_string(), agent_info)
            })
            .collect();

        let response = ClusterMembers { agents, leader_id };

        Ok(Response::new(response))
    }

    type GetCompletedBuildStream =
        tokio_stream::wrappers::ReceiverStream<Result<BuildUpdate, Status>>;

    /// Get completed build outputs from cache
    ///
    /// Serves outputs from recently completed builds (5 minute cache).
    async fn get_completed_build(
        &self,
        request: Request<GetCompletedBuildRequest>,
    ) -> Result<Response<Self::GetCompletedBuildStream>, Status> {
        let req = request.into_inner();
        let drv_path = req.derivation_path.clone();

        tracing::info!("Client requesting completed build: {}", drv_path);

        // Query state machine for completed build
        let (output_paths, agent_id) = {
            let state = self.agent.state_machine.data.read();
            let drv_path_key: rio_common::DerivationPath = drv_path.clone().into();

            let completed = state
                .cluster
                .completed_builds
                .get(&drv_path_key)
                .ok_or_else(|| {
                    Status::not_found(format!(
                        "Build {} not in cache (may have expired)",
                        drv_path
                    ))
                })?;

            (completed.output_paths.clone(), completed.agent_id)
        }; // Lock released here

        // Verify this agent has the outputs
        if agent_id != self.agent.id {
            return Err(Status::failed_precondition(format!(
                "Build outputs are on agent {}, not this agent",
                agent_id
            )));
        }

        // Verify outputs still exist in /nix/store (not garbage collected)
        for path in &output_paths {
            if tokio::fs::metadata(path).await.is_err() {
                return Err(Status::not_found(format!(
                    "Build outputs no longer in store (garbage collected): {}",
                    path
                )));
            }
        }

        tracing::info!("Streaming {} output paths from cache", output_paths.len());

        // Create channel for streaming
        let (tx, rx) = tokio::sync::mpsc::channel(100);

        // Convert output paths to strings for nar_exporter
        let output_paths_str: Vec<String> = output_paths
            .iter()
            .map(|p| p.as_str().to_string())
            .collect();

        // Spawn task to stream outputs
        let drv_path_clone: rio_common::DerivationPath = drv_path.clone().into();
        let output_paths_for_msg = output_paths_str.clone();
        tokio::spawn(async move {
            // Stream NAR outputs
            if let Err(e) = crate::nar_exporter::stream_outputs(
                &output_paths_str,
                drv_path_clone.clone(),
                vec![tx.clone()],
            )
            .await
            {
                tracing::error!("Failed to stream completed build outputs: {}", e);
                return;
            }

            // Send completion message after outputs
            use rio_common::proto::{BuildCompleted, BuildUpdate, build_update};
            let completion = BuildUpdate {
                derivation_path: drv_path_clone.as_str().to_string(),
                update: Some(build_update::Update::Completed(BuildCompleted {
                    output_paths: output_paths_for_msg,
                    duration_ms: 0, // Not tracked for cached builds
                })),
            };

            if let Err(e) = tx.send(Ok(completion)).await {
                tracing::error!("Failed to send completion message: {}", e);
            }
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }

    /// Get build status (Phase 1: unimplemented)
    async fn get_build_status(
        &self,
        request: Request<GetBuildStatusRequest>,
    ) -> Result<Response<BuildStatusResponse>, Status> {
        let req = request.into_inner();
        let drv_path: camino::Utf8PathBuf = req.derivation_path.into();

        let cluster_state = &self.agent.state_machine.data.read().cluster;

        // Check if build is in progress
        if let Some(tracker) = cluster_state.builds_in_progress.get(&drv_path) {
            use rio_common::proto::BuildState;

            let state = match tracker.status {
                crate::state_machine::BuildStatus::Queued => BuildState::Queued,
                crate::state_machine::BuildStatus::Claimed => BuildState::Queued, // Map Claimed to Queued for proto
                crate::state_machine::BuildStatus::Building => BuildState::Building,
            };

            return Ok(Response::new(BuildStatusResponse {
                derivation_path: drv_path.to_string(),
                state: state as i32,
                agent_id: tracker.agent_id.map(|id| id.to_string()),
                error: None,
            }));
        }

        // Check if build completed
        if let Some(_completed) = cluster_state.completed_builds.get(&drv_path) {
            return Ok(Response::new(BuildStatusResponse {
                derivation_path: drv_path.to_string(),
                state: rio_common::proto::BuildState::Completed as i32,
                agent_id: None, // Client should use AlreadyCompleted from QueueBuild
                error: None,
            }));
        }

        // Build not found
        Ok(Response::new(BuildStatusResponse {
            derivation_path: drv_path.to_string(),
            state: rio_common::proto::BuildState::NotFound as i32,
            agent_id: None,
            error: None,
        }))
    }

    /// Join cluster (Phase 3.3: Multi-node support)
    async fn join_cluster(
        &self,
        request: Request<JoinClusterRequest>,
    ) -> Result<Response<JoinClusterResponse>, Status> {
        let req = request.into_inner();

        let joining_agent = req
            .agent_info
            .ok_or_else(|| Status::invalid_argument("agent_info is required"))?;

        tracing::info!(
            "Agent {} requesting to join cluster at {}",
            joining_agent.id,
            joining_agent.address
        );

        let raft = &self.agent.raft;
        let sm_store = &self.agent.state_machine;

        // Check if this agent is the leader
        let metrics = raft.metrics().borrow().clone();
        let current_leader = metrics.current_leader;

        if current_leader != Some(self.agent.id) {
            // Not leader - return error with leader info
            let leader_id = current_leader
                .map(|id| id.to_string())
                .unwrap_or_else(|| "unknown".to_string());

            return Ok(Response::new(JoinClusterResponse {
                success: false,
                message: format!("Not leader. Current leader: {}", leader_id),
            }));
        }

        // Parse joining agent info
        let joining_id = uuid::Uuid::parse_str(&joining_agent.id)
            .map_err(|e| Status::invalid_argument(format!("Invalid agent ID: {}", e)))?;

        let joining_addr = url::Url::parse(&joining_agent.address)
            .map_err(|e| Status::invalid_argument(format!("Invalid agent address: {}", e)))?;

        // Check if agent ID already exists
        {
            let cluster_state = &sm_store.data.read().cluster;
            if cluster_state.agents.contains_key(&joining_id) {
                return Ok(Response::new(JoinClusterResponse {
                    success: false,
                    message: format!("Agent ID {} already exists in cluster", joining_id),
                }));
            }
        }

        // Add node to Raft as learner first (safe addition)
        tracing::info!("Adding agent {} as learner to Raft", joining_id);

        let node = crate::state_machine::Node {
            rpc_addr: joining_addr.to_string(),
        };

        raft.add_learner(joining_id, node, true)
            .await
            .map_err(|e| Status::internal(format!("Failed to add learner: {}", e)))?;

        // Propose AgentJoined to register in cluster state
        let agent_info = crate::state_machine::AgentInfo {
            id: joining_id,
            address: joining_addr,
            platforms: joining_agent.platforms,
            features: joining_agent.features,
            status: crate::state_machine::AgentStatus::Available,
            last_heartbeat: chrono::Utc::now(),
        };

        let cmd = crate::state_machine::RaftCommand::AgentJoined {
            id: joining_id,
            info: agent_info,
        };

        raft.client_write(cmd)
            .await
            .map_err(|e| Status::internal(format!("Failed to propose AgentJoined: {}", e)))?;

        // Promote learner to voting member
        tracing::info!("Promoting agent {} to voting member", joining_id);

        // Get current voters and add the new one
        let metrics = raft.metrics().borrow().clone();
        let current_membership = metrics.membership_config.membership();
        let mut new_voters: Vec<uuid::Uuid> = current_membership.voter_ids().collect();
        if !new_voters.contains(&joining_id) {
            new_voters.push(joining_id);
        }

        // change_membership with retain=true adds these as voters, keeping existing learners
        raft.change_membership(new_voters, true)
            .await
            .map_err(|e| Status::internal(format!("Failed to change membership: {}", e)))?;

        tracing::info!("Agent {} successfully joined cluster", joining_id);

        Ok(Response::new(JoinClusterResponse {
            success: true,
            message: format!("Successfully joined cluster as {}", joining_id),
        }))
    }

    /// Report build result (internal - called by non-leader agents)
    ///
    /// Non-leader agents call this on the leader to propose BuildCompleted/BuildFailed.
    /// Any agent can receive this call, but only the leader can commit to Raft.
    async fn report_build_result(
        &self,
        request: Request<ReportBuildResultRequest>,
    ) -> Result<Response<ReportBuildResultResponse>, Status> {
        use rio_common::proto::report_build_result_request::Result as BuildResult;

        let req = request.into_inner();
        let derivation_path: rio_common::DerivationPath = req.derivation_path.clone().into();

        tracing::info!("Received build result report for {}", req.derivation_path);

        // Build the appropriate Raft command based on result
        let cmd = match req.result {
            Some(BuildResult::Completed(completed)) => {
                let output_paths: Vec<rio_common::DerivationPath> =
                    completed.output_paths.iter().map(|p| p.into()).collect();

                crate::state_machine::RaftCommand::BuildCompleted {
                    derivation_path,
                    output_paths,
                }
            }
            Some(BuildResult::Failed(failed)) => crate::state_machine::RaftCommand::BuildFailed {
                derivation_path,
                error: failed.error,
            },
            None => {
                return Err(Status::invalid_argument("No result in request"));
            }
        };

        // Propose to Raft (this agent might be leader or follower)
        // If follower, client_write will fail with ForwardToLeader error
        let raft = &self.agent.raft;
        match raft.client_write(cmd).await {
            Ok(_) => {
                tracing::info!("Build result proposal committed to Raft");
                Ok(Response::new(ReportBuildResultResponse {
                    success: true,
                    message: "Build result committed".to_string(),
                }))
            }
            Err(e) => {
                let error_msg = e.to_string();
                // Check if we need to forward to leader
                if error_msg.contains("has to forward request to") {
                    // This agent is not the leader, caller should retry with leader
                    Err(Status::failed_precondition(format!(
                        "Not leader, forward to leader: {}",
                        error_msg
                    )))
                } else {
                    Err(Status::internal(format!("Raft proposal failed: {}", e)))
                }
            }
        }
    }
}

/// Start the gRPC server
pub async fn serve(listen_addr: String, agent: Agent) -> Result<()> {
    let addr = listen_addr
        .parse()
        .with_context(|| format!("Invalid listen address: {}", listen_addr))?;

    let agent = Arc::new(agent);

    // Create RioAgent service (CLI-facing)
    let rio_agent_service = RioAgentService {
        agent: agent.clone(),
    };

    // Create RaftInternal service (agent-to-agent Raft communication)
    let raft_internal_service = crate::raft_grpc::RaftInternalService::new(agent.raft.clone());

    tracing::info!(
        "gRPC server listening on {} (RioAgent + RaftInternal services)",
        addr
    );

    Server::builder()
        .add_service(RioAgentServer::new(rio_agent_service))
        .add_service(
            rio_common::proto::raft_internal_server::RaftInternalServer::new(raft_internal_service),
        )
        .serve(addr)
        .await
        .context("gRPC server error")?;

    Ok(())
}

/// Start the gRPC server with a pre-bound listener
///
/// Used internally by Agent::bootstrap() to ensure server is ready before Raft initializes.
pub async fn serve_with_listener(
    listener: tokio::net::TcpListener,
    agent: Arc<Agent>,
) -> Result<()> {
    // Create RioAgent service (CLI-facing)
    let rio_agent_service = RioAgentService {
        agent: agent.clone(),
    };

    // Create RaftInternal service (agent-to-agent Raft communication)
    let raft_internal_service = crate::raft_grpc::RaftInternalService::new(agent.raft.clone());

    let addr = listener
        .local_addr()
        .context("Failed to get local address")?;
    tracing::info!(
        "gRPC server listening on {} (RioAgent + RaftInternal services)",
        addr
    );

    Server::builder()
        .add_service(RioAgentServer::new(rio_agent_service))
        .add_service(
            rio_common::proto::raft_internal_server::RaftInternalServer::new(raft_internal_service),
        )
        .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
        .await
        .context("gRPC server error")?;

    Ok(())
}
