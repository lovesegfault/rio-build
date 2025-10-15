//! Cluster discovery and leader identification
//!
//! Discovers Rio agent cluster by querying seed agents.

use anyhow::{Context, Result};
use rio_common::proto::rio_agent_client::RioAgentClient;
use rio_common::proto::{AgentInfo, GetClusterMembersRequest};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use url::Url;

/// Cluster information with leader details
#[derive(Debug, Clone)]
pub struct ClusterInfo {
    /// Current Raft leader ID
    pub leader_id: String,

    /// Leader's gRPC address
    pub leader_address: String,

    /// All agents in the cluster (keyed by agent ID for O(1) lookup)
    pub agents: HashMap<String, AgentInfo>,

    /// When this cluster info was discovered
    pub discovered_at: Instant,
}

impl ClusterInfo {
    /// Check if cluster info is stale (older than TTL)
    pub fn is_stale(&self, ttl: Duration) -> bool {
        self.discovered_at.elapsed() > ttl
    }

    /// Get an agent by ID (O(1) lookup)
    pub fn get_agent(&self, agent_id: &str) -> Option<&AgentInfo> {
        self.agents.get(agent_id)
    }

    /// Find the leader agent from the agent map (O(1) lookup)
    fn find_leader(leader_id: &str, agents: &HashMap<String, AgentInfo>) -> Result<String> {
        agents
            .get(leader_id)
            .map(|agent| agent.address.clone())
            .with_context(|| format!("Leader {} not found in cluster", leader_id))
    }

    /// Get the number of agents in the cluster
    pub fn agent_count(&self) -> usize {
        self.agents.len()
    }
}

/// Discover cluster by querying seed agents
///
/// Tries each seed agent URL until one responds with cluster membership info.
/// Returns ClusterInfo with current leader and all agents.
#[tracing::instrument(skip(seed_urls))]
pub async fn discover_cluster(seed_urls: &[Url]) -> Result<ClusterInfo> {
    tracing::info!("Discovering cluster from {} seed agents", seed_urls.len());

    let mut last_error = None;

    for seed_url in seed_urls {
        tracing::debug!("Trying seed agent: {}", seed_url);

        match try_discover_from_agent(seed_url).await {
            Ok(cluster_info) => {
                tracing::info!(
                    "Discovered cluster: leader={}, agents={}",
                    cluster_info.leader_id,
                    cluster_info.agents.len()
                );
                return Ok(cluster_info);
            }
            Err(e) => {
                tracing::warn!("Failed to discover from {}: {}", seed_url, e);
                last_error = Some(e);
            }
        }
    }

    // All seed agents failed
    Err(last_error
        .unwrap_or_else(|| anyhow::anyhow!("No seed agents provided for cluster discovery")))
    .context("Failed to discover cluster from any seed agent")
}

/// Try to discover cluster from a single agent
async fn try_discover_from_agent(agent_url: &Url) -> Result<ClusterInfo> {
    // Connect to agent with timeout
    let client = tokio::time::timeout(
        Duration::from_secs(5),
        RioAgentClient::connect(agent_url.to_string()),
    )
    .await
    .context("Connection timeout")?
    .with_context(|| format!("Failed to connect to agent: {}", agent_url))?;

    // Call GetClusterMembers RPC
    let mut client = client;
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        client.get_cluster_members(GetClusterMembersRequest {}),
    )
    .await
    .context("RPC timeout")?
    .context("GetClusterMembers RPC failed")?
    .into_inner();

    // Validate response
    if response.leader_id.is_empty() {
        anyhow::bail!("Agent returned empty leader_id - cluster may not be initialized");
    }

    if response.agents.is_empty() {
        anyhow::bail!("Agent returned empty agent list");
    }

    // Find leader's address
    let leader_address = ClusterInfo::find_leader(&response.leader_id, &response.agents)?;

    Ok(ClusterInfo {
        leader_id: response.leader_id,
        leader_address,
        agents: response.agents,
        discovered_at: Instant::now(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cluster_info_is_stale() {
        let info = ClusterInfo {
            leader_id: "leader-1".to_string(),
            leader_address: "http://localhost:50051".to_string(),
            agents: HashMap::new(),
            discovered_at: Instant::now() - Duration::from_secs(70),
        };

        assert!(info.is_stale(Duration::from_secs(60)));
        assert!(!info.is_stale(Duration::from_secs(120)));
    }

    #[test]
    fn test_find_leader() {
        let mut agents = HashMap::new();
        agents.insert(
            "agent-1".to_string(),
            AgentInfo {
                id: "agent-1".to_string(),
                address: "http://agent1:50051".to_string(),
                platforms: vec![],
                features: vec![],
                status: 0,
                capacity: None,
            },
        );
        agents.insert(
            "leader-1".to_string(),
            AgentInfo {
                id: "leader-1".to_string(),
                address: "http://leader:50051".to_string(),
                platforms: vec![],
                features: vec![],
                status: 0,
                capacity: None,
            },
        );

        let leader_addr = ClusterInfo::find_leader("leader-1", &agents).unwrap();
        assert_eq!(leader_addr, "http://leader:50051");
    }

    #[test]
    fn test_find_leader_not_found() {
        let mut agents = HashMap::new();
        agents.insert(
            "agent-1".to_string(),
            AgentInfo {
                id: "agent-1".to_string(),
                address: "http://agent1:50051".to_string(),
                platforms: vec![],
                features: vec![],
                status: 0,
                capacity: None,
            },
        );

        let result = ClusterInfo::find_leader("missing-leader", &agents);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not found in cluster")
        );
    }
}
