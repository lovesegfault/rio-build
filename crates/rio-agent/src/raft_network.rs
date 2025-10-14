//! Raft network implementation using gRPC
//!
//! Implements openraft's RaftNetwork trait to enable Raft nodes to communicate.

use openraft::error::{InstallSnapshotError, NetworkError, RPCError, RaftError, RemoteError};
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::state_machine::Node;
use crate::storage::{NodeId, TypeConfig};

/// Raft network factory
#[derive(Clone)]
pub struct NetworkFactory {
    /// Map of node_id → Node info (for looking up addresses)
    nodes: Arc<RwLock<HashMap<NodeId, Node>>>,
}

impl NetworkFactory {
    pub fn new() -> Self {
        Self {
            nodes: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Add a node to the network
    pub async fn add_node(&self, node_id: NodeId, node: Node) {
        let mut nodes = self.nodes.write().await;
        nodes.insert(node_id, node);
    }
}

impl RaftNetworkFactory<TypeConfig> for NetworkFactory {
    type Network = RaftNetworkConnection;

    async fn new_client(&mut self, target: NodeId, node: &Node) -> Self::Network {
        RaftNetworkConnection {
            target,
            addr: node.rpc_addr.clone(),
        }
    }
}

/// Network connection to a specific Raft node
pub struct RaftNetworkConnection {
    target: NodeId,
    addr: String,
}

impl RaftNetwork<TypeConfig> for RaftNetworkConnection {
    async fn append_entries(
        &mut self,
        _req: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<NodeId>, RPCError<NodeId, Node, RaftError<NodeId>>> {
        // Phase 2.4: Single-node cluster doesn't need network communication
        // Phase 3+: Will implement gRPC call to target agent
        tracing::debug!(target = self.target, "append_entries (not implemented)");
        Err(RPCError::Network(NetworkError::new(&std::io::Error::new(
            std::io::ErrorKind::Other,
            "Multi-node not implemented yet",
        ))))
    }

    async fn install_snapshot(
        &mut self,
        _req: InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<NodeId>,
        RPCError<NodeId, Node, RaftError<NodeId, InstallSnapshotError>>,
    > {
        // Phase 2.4: Not needed for single-node
        tracing::debug!(target = self.target, "install_snapshot (not implemented)");
        Err(RPCError::Network(NetworkError::new(&std::io::Error::new(
            std::io::ErrorKind::Other,
            "Multi-node not implemented yet",
        ))))
    }

    async fn vote(
        &mut self,
        _req: VoteRequest<NodeId>,
        _option: RPCOption,
    ) -> Result<VoteResponse<NodeId>, RPCError<NodeId, Node, RaftError<NodeId>>> {
        // Phase 2.4: Single-node doesn't need voting
        tracing::debug!(target = self.target, "vote (not implemented)");
        Err(RPCError::Network(NetworkError::new(&std::io::Error::new(
            std::io::ErrorKind::Other,
            "Multi-node not implemented yet",
        ))))
    }
}
