//! Raft node creation and configuration

use anyhow::{Context, Result};
use camino::Utf8Path;
use openraft::{Config, Raft};
use std::collections::BTreeSet;
use std::sync::Arc;

use crate::raft_network::NetworkFactory;
use crate::state_machine::Node;
use crate::storage::{NodeId, StateMachineStore, TypeConfig, new_storage};

/// Create and bootstrap a single-node Raft cluster
///
/// Returns the Raft instance and a cloneable state machine store for querying.
/// NodeId is the agent's UUID (not truncated).
pub async fn bootstrap_single_node(
    node_id: NodeId,
    rpc_addr: String,
    data_dir: &Utf8Path,
) -> Result<(Arc<Raft<TypeConfig>>, StateMachineStore)> {
    // Create storage
    let (log_store, sm_store) = new_storage(data_dir)
        .await
        .context("Failed to create Raft storage")?;

    // Clone state machine store for returning (it's cloneable)
    let sm_store_clone = sm_store.clone();

    // Create network
    let network = NetworkFactory::new();

    // Add self to network
    network
        .add_node(
            node_id,
            Node {
                rpc_addr: rpc_addr.clone(),
            },
        )
        .await;

    // Configure Raft
    let config = Config {
        heartbeat_interval: 500,    // 500ms
        election_timeout_min: 1500, // 1.5s
        election_timeout_max: 3000, // 3s
        ..Default::default()
    };

    let config = Arc::new(config.validate().context("Invalid Raft config")?);

    // Create Raft instance
    let raft = Raft::new(node_id, config, network, log_store, sm_store)
        .await
        .context("Failed to create Raft instance")?;

    // Bootstrap as single-node cluster
    let mut nodes = BTreeSet::new();
    nodes.insert(node_id);

    raft.initialize(nodes)
        .await
        .context("Failed to initialize single-node cluster")?;

    tracing::info!(node_id = %node_id, rpc_addr, "Bootstrapped single-node Raft cluster");

    Ok((Arc::new(raft), sm_store_clone))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_bootstrap_single_node() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let temp_path = Utf8Path::from_path(temp_dir.path()).expect("Invalid UTF-8 path");

        let node_id = uuid::Uuid::new_v4();
        let rpc_addr = "localhost:50051".to_string();

        let (raft, _sm_store) = bootstrap_single_node(node_id, rpc_addr, temp_path)
            .await
            .expect("Failed to bootstrap");

        // Verify it becomes leader
        // In a single-node cluster, it should immediately become leader
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let metrics = raft.metrics().borrow().clone();
        assert_eq!(metrics.current_leader, Some(node_id));
    }
}
