//! gRPC client for Rio agents

use anyhow::{Context, Result, bail};
use rio_common::proto::rio_agent_client::RioAgentClient;
use rio_common::proto::{
    BuildUpdate, GetCompletedBuildRequest, QueueBuildRequest, SubscribeToBuildRequest,
};
use tonic::Streaming;
use tonic::transport::Channel;

use crate::cluster::ClusterInfo;
use crate::evaluator::BuildInfo;

/// Rio gRPC client
pub struct RioClient {
    client: RioAgentClient<Channel>,
}

impl RioClient {
    /// Connect to a Rio agent
    pub async fn connect(url: &str) -> Result<Self> {
        let client = RioAgentClient::connect(url.to_string())
            .await
            .with_context(|| format!("Failed to connect to agent at {}", url))?;

        Ok(Self { client })
    }

    /// Submit a build and get a stream of updates
    ///
    /// Handles all response types: BuildAssigned, AlreadyBuilding, AlreadyCompleted, NoEligibleAgents
    pub async fn submit_build(
        &mut self,
        build_info: BuildInfo,
        cluster_info: &ClusterInfo,
    ) -> Result<Streaming<BuildUpdate>> {
        // Convert dependency paths to strings for the protocol
        let dependency_paths: Vec<String> = build_info
            .dependency_paths
            .iter()
            .map(|p| p.as_str().to_string())
            .collect();

        let derivation_path = build_info.drv_path.as_str().to_string();

        // Submit the build to the leader
        let request = QueueBuildRequest {
            derivation_path: derivation_path.clone(),
            derivation: build_info.drv_nar_bytes,
            dependency_paths,
            platform: build_info.platform.clone(),
            required_features: build_info.required_features.clone(),
            timeout_seconds: None,
        };

        let response = self
            .client
            .queue_build(request)
            .await
            .context("Failed to queue build")?
            .into_inner();

        // Handle all possible response types
        match response.result {
            Some(rio_common::proto::queue_build_response::Result::Assigned(assigned)) => {
                tracing::info!(
                    "Build assigned to agent: {} for derivation: {}",
                    assigned.agent_id,
                    assigned.derivation_path
                );

                // Wait briefly for coordinator to notice assignment and start build
                // The coordinator polls every 100ms, so 200ms should be enough
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;

                // Subscribe to the build on this agent (already connected to leader)
                let subscribe_request = SubscribeToBuildRequest {
                    derivation_path: assigned.derivation_path,
                };

                let stream = self
                    .client
                    .subscribe_to_build(subscribe_request)
                    .await
                    .context("Failed to subscribe to build")?
                    .into_inner();

                Ok(stream)
            }

            Some(rio_common::proto::queue_build_response::Result::AlreadyBuilding(already)) => {
                tracing::info!(
                    "Build already in progress on agent {}, subscribing to existing build...",
                    already.agent_id
                );

                // Connect to the agent that's currently building
                let mut agent_client = connect_to_agent(&already.agent_id, cluster_info).await?;

                // Subscribe to the in-progress build
                let subscribe_request = SubscribeToBuildRequest {
                    derivation_path: already.derivation_path,
                };

                let stream = agent_client
                    .client
                    .subscribe_to_build(subscribe_request)
                    .await
                    .context("Failed to subscribe to existing build")?
                    .into_inner();

                Ok(stream)
            }

            Some(rio_common::proto::queue_build_response::Result::AlreadyCompleted(completed)) => {
                tracing::info!(
                    "Build recently completed on agent {}, fetching from cache...",
                    completed.agent_id
                );

                // Connect to the agent with cached outputs
                let mut agent_client = connect_to_agent(&completed.agent_id, cluster_info).await?;

                // Get the completed build from cache
                let request = GetCompletedBuildRequest {
                    derivation_path: completed.derivation_path,
                };

                let stream = agent_client
                    .client
                    .get_completed_build(request)
                    .await
                    .context("Failed to get completed build from cache")?
                    .into_inner();

                Ok(stream)
            }

            Some(rio_common::proto::queue_build_response::Result::NoAgents(no_agents)) => {
                bail!(
                    "No agents available for platform '{}' with features {:?}: {}",
                    build_info.platform,
                    build_info.required_features,
                    no_agents.reason
                );
            }

            None => {
                bail!("No result in QueueBuildResponse");
            }
        }
    }
}

/// Connect to a specific agent by ID
///
/// Looks up the agent in the cluster info (O(1)) and connects to its address.
pub async fn connect_to_agent(agent_id: &str, cluster_info: &ClusterInfo) -> Result<RioClient> {
    let agent = cluster_info
        .get_agent(agent_id)
        .with_context(|| format!("Agent {} not found in cluster", agent_id))?;

    tracing::debug!("Connecting to agent {} at {}", agent_id, agent.address);

    RioClient::connect(&agent.address).await
}
