//! Raft node creation and configuration
//!
//! # Initialization Pattern
//!
//! Raft initialization requires careful ordering to avoid "no leader" errors:
//!
//! ## For Bootstrap (single-node leader):
//! ```ignore
//! // 1. Create uninitialized Raft
//! let (raft, sm_store) = create_uninitialized_raft(id, addr, data_dir).await?;
//!
//! // 2. Start gRPC server (MUST run before step 3!)
//! tokio::spawn(serve_with_listener(listener, agent));
//! tokio::time::sleep(Duration::from_millis(500)).await;
//!
//! // 3. Initialize as leader (server is now ready for self-replication)
//! initialize_single_node_leader(&raft, id, addr).await?;
//! ```
//!
//! ## For Join (learner joining cluster):
//! ```ignore
//! // 1. Create uninitialized Raft
//! let (raft, sm_store) = join_cluster(id, addr, data_dir).await?;
//!
//! // 2. Start gRPC server
//! tokio::spawn(serve_with_listener(listener, agent));
//!
//! // 3. Call JoinCluster RPC on seed - leader will add us
//! // DO NOT call initialize() - Raft syncs state from leader automatically
//! ```
//!
//! **IMPORTANT:** Always use `Agent::bootstrap()` or `Agent::join()` which handle
//! this sequence correctly. Only use these low-level functions for testing.

use anyhow::{Context, Result};
use camino::Utf8Path;
use openraft::{Config, Raft};
use std::sync::Arc;

use crate::raft_network::NetworkFactory;
use crate::state_machine::Node;
use crate::storage::{NodeId, StateMachineStore, TypeConfig, new_storage};

/// Create an uninitialized Raft instance
///
/// WARNING: This creates an uninitialized Raft instance. For bootstrap scenarios,
/// you MUST call `initialize_single_node_leader()` after the gRPC server is running.
/// For join scenarios, do NOT call initialize - the leader will add you.
///
/// Consider using the higher-level `Agent::bootstrap()` or `Agent::join()` instead.
pub async fn create_uninitialized_raft(
    node_id: NodeId,
    rpc_addr: String,
    data_dir: &Utf8Path,
) -> Result<(Arc<Raft<TypeConfig>>, StateMachineStore)> {
    // Create storage
    let (log_store, sm_store) = new_storage(data_dir)
        .await
        .context("Failed to create Raft storage")?;

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

    // Create Raft instance (uninitialized)
    let raft = Raft::new(node_id, config, network, log_store, sm_store)
        .await
        .context("Failed to create Raft instance")?;

    tracing::debug!(node_id = %node_id, rpc_addr, "Created uninitialized Raft instance");

    Ok((Arc::new(raft), sm_store_clone))
}

/// Initialize a Raft instance as a single-node leader
///
/// MUST be called after the gRPC server is running and accepting connections.
/// This completes the bootstrap process started by `create_uninitialized_raft()`.
///
/// # Safety Requirements
/// 1. gRPC server must be running on `rpc_addr`
/// 2. Raft instance must be uninitialized (no prior `initialize()` call)
/// 3. This should only be called for single-node bootstrap, not for joining
pub async fn initialize_single_node_leader(
    raft: &Arc<Raft<TypeConfig>>,
    node_id: NodeId,
    rpc_addr: String,
) -> Result<()> {
    let mut nodes = std::collections::BTreeMap::new();
    nodes.insert(node_id, Node { rpc_addr });

    raft.initialize(nodes)
        .await
        .context("Failed to initialize single-node cluster")?;

    tracing::info!(node_id = %node_id, "Initialized as single-node leader");

    Ok(())
}

/// Create a Raft instance for joining an existing cluster
///
/// Creates an uninitialized Raft instance that will join an existing cluster.
/// The instance will sync state from the leader after add_learner/change_membership.
///
/// # Safety
/// - gRPC server MUST be running before calling JoinCluster RPC on the seed
/// - Do NOT call initialize() - the leader will add this node to the cluster
pub async fn join_cluster(
    node_id: NodeId,
    rpc_addr: String,
    data_dir: &Utf8Path,
) -> Result<(Arc<Raft<TypeConfig>>, StateMachineStore)> {
    // Create uninitialized Raft instance (will sync from leader)
    create_uninitialized_raft(node_id, rpc_addr, data_dir).await
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

        // Create and initialize Raft
        let (raft, _sm_store) = create_uninitialized_raft(node_id, rpc_addr.clone(), temp_path)
            .await
            .expect("Failed to create Raft");

        initialize_single_node_leader(&raft, node_id, rpc_addr)
            .await
            .expect("Failed to initialize");

        // Verify it's initialized as leader
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let metrics = raft.metrics().borrow().clone();
        assert_eq!(metrics.current_leader, Some(node_id));
    }
}
