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
            Some(rio_common::proto::queue_build_response::Result::BuildInfo(info)) => {
                tracing::info!(
                    "Build status: {:?}, agent: {:?}, {} suggested agents",
                    info.status,
                    info.agent_id,
                    info.suggested_agents.len()
                );

                // Check if build already has an agent (claimed or building)
                let agent_id = if let Some(agent_id) = info.agent_id {
                    // Build already claimed or building
                    tracing::info!("Build already claimed/building on agent {}", agent_id);
                    agent_id
                } else {
                    // Build queued, wait for claim
                    tracing::info!("Build queued, waiting for agent to claim...");
                    wait_for_claim(
                        &mut self.client,
                        &derivation_path,
                        std::time::Duration::from_secs(30),
                    )
                    .await
                    .context("Build was never claimed by any agent")?
                };

                tracing::info!("Connecting to agent: {}", agent_id);

                // Connect to the agent
                let mut agent_client = connect_to_agent(&agent_id, cluster_info).await?;

                // Subscribe to the build
                let subscribe_request = SubscribeToBuildRequest { derivation_path };

                let stream = agent_client
                    .client
                    .subscribe_to_build(subscribe_request)
                    .await
                    .context("Failed to subscribe to build")?
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

/// Wait for a build to be claimed by an agent
///
/// Polls GetBuildStatus until the build has agent_id set (Claimed or Building status).
/// Returns the agent_id that claimed the build.
async fn wait_for_claim(
    client: &mut RioAgentClient<Channel>,
    derivation_path: &str,
    timeout: std::time::Duration,
) -> Result<String> {
    use rio_common::proto::{BuildState, GetBuildStatusRequest};

    let start = std::time::Instant::now();
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(200));

    loop {
        interval.tick().await;

        if start.elapsed() > timeout {
            bail!("Timeout waiting for build to be claimed");
        }

        let status_response = client
            .get_build_status(GetBuildStatusRequest {
                derivation_path: derivation_path.to_string(),
            })
            .await
            .context("Failed to get build status")?
            .into_inner();

        // Check if build has been claimed (has agent_id)
        if let Some(agent_id) = status_response.agent_id
            && !agent_id.is_empty()
        {
            // Build has been claimed or is building
            return Ok(agent_id);
        }

        // Check if build failed before being claimed
        if status_response.state == BuildState::Failed as i32 {
            bail!("Build failed before being claimed");
        }

        tracing::debug!(
            "Build still queued (state: {}), waiting for claim...",
            status_response.state
        );
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
