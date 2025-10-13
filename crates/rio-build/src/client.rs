//! gRPC client for Rio agents

use anyhow::{Context, Result};
use rio_common::proto::rio_agent_client::RioAgentClient;
use rio_common::proto::{BuildUpdate, QueueBuildRequest, SubscribeToBuildRequest};
use tonic::Streaming;
use tonic::transport::Channel;

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
    /// Phase 1: Simplified - only handles BuildAssigned response
    pub async fn submit_build(&mut self, build_info: BuildInfo) -> Result<Streaming<BuildUpdate>> {
        // Convert dependency paths to strings for the protocol
        let dependency_paths: Vec<String> = build_info
            .dependency_paths
            .iter()
            .map(|p| p.as_str().to_string())
            .collect();

        // Submit the build to the agent
        let request = QueueBuildRequest {
            derivation_path: build_info.drv_path.as_str().to_string(),
            derivation: build_info.drv_nar_bytes,
            dependency_paths,
            platform: build_info.platform,
            required_features: build_info.required_features,
            timeout_seconds: None,
        };

        let response = self
            .client
            .queue_build(request)
            .await
            .context("Failed to queue build")?
            .into_inner();

        // Phase 1: We only expect BuildAssigned
        let assigned = response
            .result
            .and_then(|r| {
                if let rio_common::proto::queue_build_response::Result::Assigned(a) = r {
                    Some(a)
                } else {
                    None
                }
            })
            .context("Expected BuildAssigned response (Phase 1)")?;

        tracing::info!(
            "Build assigned to agent: {} for derivation: {}",
            assigned.agent_id,
            assigned.derivation_path
        );

        // Subscribe to the build to get logs and outputs
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
}
