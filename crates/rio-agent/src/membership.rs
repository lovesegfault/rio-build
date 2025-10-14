//! Cluster membership management

use anyhow::{Context, Result};
use chrono::Utc;
use openraft::Raft;
use std::sync::Arc;

use crate::state_machine::{AgentInfo, AgentStatus, RaftCommand};
use crate::storage::TypeConfig;
use rio_common::AgentId;

/// Register this agent in the cluster
///
/// Proposes AgentJoined command to Raft and waits for commit.
pub async fn register_agent(
    raft: &Arc<Raft<TypeConfig>>,
    agent_id: AgentId,
    rpc_addr: String,
    platforms: Vec<String>,
    features: Vec<String>,
) -> Result<()> {
    // Parse address as URL (add http:// if missing)
    let address = if rpc_addr.starts_with("http://") || rpc_addr.starts_with("https://") {
        url::Url::parse(&rpc_addr).context("Invalid URL")?
    } else {
        url::Url::parse(&format!("http://{}", rpc_addr)).context("Invalid address")?
    };

    let info = AgentInfo {
        id: agent_id,
        address,
        platforms,
        features,
        status: AgentStatus::Available,
        last_heartbeat: Utc::now(),
    };

    let cmd = RaftCommand::AgentJoined { id: agent_id, info };

    // Propose to Raft
    raft.client_write(cmd)
        .await
        .context("Failed to propose AgentJoined")?;

    tracing::info!(agent_id = %agent_id, "Agent registered in cluster");

    Ok(())
}
