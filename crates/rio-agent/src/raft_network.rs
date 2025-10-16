//! Raft network implementation using gRPC
//!
//! Implements openraft's RaftNetwork trait to enable Raft nodes to communicate.

use openraft::error::{InstallSnapshotError, NetworkError, RPCError, RaftError};
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

impl Default for NetworkFactory {
    fn default() -> Self {
        Self::new()
    }
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
    #[allow(dead_code)] // Will be used for gRPC calls in Phase 3
    addr: String,
}

impl RaftNetwork<TypeConfig> for RaftNetworkConnection {
    async fn append_entries(
        &mut self,
        req: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<NodeId>, RPCError<NodeId, Node, RaftError<NodeId>>> {
        use crate::raft_proto_conv::{FromProto, ToProto};
        use rio_common::proto::raft_internal_client::RaftInternalClient;

        tracing::debug!(target = %self.target, addr = %self.addr, "Forwarding append_entries to target");

        // Connect to target agent
        let url = if self.addr.starts_with("http://") || self.addr.starts_with("https://") {
            self.addr.clone()
        } else {
            format!("http://{}", self.addr)
        };

        let mut client = RaftInternalClient::connect(url.clone())
            .await
            .map_err(|e| {
                tracing::warn!("Failed to connect to {}: {}", url, e);
                RPCError::Network(NetworkError::new(&e))
            })?;

        // Convert to proto
        let pb_req = req.to_proto();

        // Send RPC
        let pb_resp = client
            .append_entries(pb_req)
            .await
            .map_err(|e| {
                tracing::warn!("append_entries RPC failed to {}: {}", url, e);
                RPCError::Network(NetworkError::new(&e))
            })?
            .into_inner();

        // Convert response back
        let raft_resp = AppendEntriesResponse::from_proto(&pb_resp);

        Ok(raft_resp)
    }

    async fn install_snapshot(
        &mut self,
        req: InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<NodeId>,
        RPCError<NodeId, Node, RaftError<NodeId, InstallSnapshotError>>,
    > {
        use crate::raft_proto_conv::{FromProto, ToProto};
        use rio_common::proto::raft_internal_client::RaftInternalClient;
        use rio_common::proto::{InstallSnapshotRequest as PbRequest, install_snapshot_request};

        tracing::debug!(target = %self.target, addr = %self.addr, "Forwarding install_snapshot to target");

        // Connect to target agent
        let url = if self.addr.starts_with("http://") || self.addr.starts_with("https://") {
            self.addr.clone()
        } else {
            format!("http://{}", self.addr)
        };

        let mut client = RaftInternalClient::connect(url.clone())
            .await
            .map_err(|e| RPCError::Network(NetworkError::new(&e)))?;

        // Create streaming request
        let (tx, rx) = tokio::sync::mpsc::channel(4);

        // Send meta chunk
        let meta_chunk = PbRequest {
            payload: Some(install_snapshot_request::Payload::Meta(
                rio_common::proto::SnapshotMeta {
                    vote: Some(req.vote.to_proto()),
                    last_log_id: req.meta.last_log_id.as_ref().map(|id| id.to_proto()),
                    last_membership: Some(req.meta.last_membership.membership().to_proto()),
                    snapshot_id: req.meta.snapshot_id.clone(),
                },
            )),
        };

        tx.send(meta_chunk)
            .await
            .map_err(|e| RPCError::Network(NetworkError::new(&e)))?;

        // Send data chunks (1MB chunks)
        let chunk_size = 1024 * 1024;
        for chunk in req.data.chunks(chunk_size) {
            let data_chunk = PbRequest {
                payload: Some(install_snapshot_request::Payload::Chunk(chunk.to_vec())),
            };
            tx.send(data_chunk)
                .await
                .map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        }

        drop(tx); // Close stream

        // Send RPC
        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        let pb_resp = client
            .install_snapshot(stream)
            .await
            .map_err(|e| RPCError::Network(NetworkError::new(&e)))?
            .into_inner();

        // Convert response
        let raft_resp = InstallSnapshotResponse {
            vote: FromProto::from_proto(
                pb_resp
                    .vote
                    .as_ref()
                    .expect("protobuf InstallSnapshotResponse must have vote field"),
            ),
        };

        Ok(raft_resp)
    }

    async fn vote(
        &mut self,
        req: VoteRequest<NodeId>,
        _option: RPCOption,
    ) -> Result<VoteResponse<NodeId>, RPCError<NodeId, Node, RaftError<NodeId>>> {
        use crate::raft_proto_conv::{FromProto, ToProto};
        use rio_common::proto::raft_internal_client::RaftInternalClient;

        tracing::debug!(target = %self.target, addr = %self.addr, "Forwarding vote to target");

        // Connect to target agent
        let url = if self.addr.starts_with("http://") || self.addr.starts_with("https://") {
            self.addr.clone()
        } else {
            format!("http://{}", self.addr)
        };

        let mut client = RaftInternalClient::connect(url.clone())
            .await
            .map_err(|e| RPCError::Network(NetworkError::new(&e)))?;

        // Convert to proto
        let pb_req = req.to_proto();

        // Send RPC
        let pb_resp = client
            .vote(pb_req)
            .await
            .map_err(|e| RPCError::Network(NetworkError::new(&e)))?
            .into_inner();

        // Convert response back
        let raft_resp = VoteResponse::from_proto(&pb_resp);

        Ok(raft_resp)
    }
}
