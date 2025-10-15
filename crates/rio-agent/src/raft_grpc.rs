//! gRPC handlers for Raft internal communication
//!
//! Implements RaftInternal service for agent-to-agent Raft RPC.

use openraft::Raft;
use rio_common::proto::raft_internal_server::RaftInternal;
use rio_common::proto::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use std::sync::Arc;
use tonic::{Request, Response, Status};

use crate::raft_proto_conv::{FromProto, ToProto};
use crate::storage::TypeConfig;

/// Raft internal RPC service
pub struct RaftInternalService {
    raft: Arc<Raft<TypeConfig>>,
}

impl RaftInternalService {
    pub fn new(raft: Arc<Raft<TypeConfig>>) -> Self {
        Self { raft }
    }
}

#[tonic::async_trait]
impl RaftInternal for RaftInternalService {
    /// Handle AppendEntries RPC from leader
    async fn append_entries(
        &self,
        request: Request<AppendEntriesRequest>,
    ) -> Result<Response<AppendEntriesResponse>, Status> {
        let pb_req = request.into_inner();

        // Convert proto to openraft type using extension trait
        let raft_req = <openraft::raft::AppendEntriesRequest<TypeConfig>>::from_proto(&pb_req);

        // Forward to Raft instance
        let raft_resp = self
            .raft
            .append_entries(raft_req)
            .await
            .map_err(|e| Status::internal(format!("Raft append_entries failed: {}", e)))?;

        // Convert response back to proto
        let pb_resp = raft_resp.to_proto();

        Ok(Response::new(pb_resp))
    }

    /// Handle Vote RPC during leader election
    async fn vote(&self, request: Request<VoteRequest>) -> Result<Response<VoteResponse>, Status> {
        let pb_req = request.into_inner();

        // Convert proto to openraft type
        let raft_req = <openraft::raft::VoteRequest<crate::storage::NodeId>>::from_proto(&pb_req);

        // Forward to Raft instance
        let raft_resp = self
            .raft
            .vote(raft_req)
            .await
            .map_err(|e| Status::internal(format!("Raft vote failed: {}", e)))?;

        // Convert response back to proto
        let pb_resp = raft_resp.to_proto();

        Ok(Response::new(pb_resp))
    }

    /// Handle InstallSnapshot RPC from leader
    async fn install_snapshot(
        &self,
        request: Request<tonic::Streaming<InstallSnapshotRequest>>,
    ) -> Result<Response<InstallSnapshotResponse>, Status> {
        let mut stream = request.into_inner();

        // Collect snapshot chunks
        let mut meta: Option<rio_common::proto::SnapshotMeta> = None;
        let mut snapshot_data = Vec::new();

        while let Some(chunk) = stream
            .message()
            .await
            .map_err(|e| Status::internal(format!("Stream error: {}", e)))?
        {
            match chunk.payload {
                Some(rio_common::proto::install_snapshot_request::Payload::Meta(m)) => {
                    meta = Some(m);
                }
                Some(rio_common::proto::install_snapshot_request::Payload::Chunk(data)) => {
                    snapshot_data.extend_from_slice(&data);
                }
                None => {
                    return Err(Status::invalid_argument("Empty snapshot chunk"));
                }
            }
        }

        let meta = meta.ok_or_else(|| Status::invalid_argument("Missing snapshot meta"))?;

        // Convert to openraft InstallSnapshotRequest
        let vote = crate::raft_proto_conv::FromProto::from_proto(
            meta.vote.as_ref().expect("Snapshot missing vote"),
        );

        // Build openraft snapshot
        let snapshot_meta = openraft::SnapshotMeta {
            last_log_id: meta.last_log_id.as_ref().map(|id| {
                crate::raft_proto_conv::FromProtoWithVote::from_proto_with_vote(id, &vote)
            }),
            last_membership: openraft::StoredMembership::new(
                meta.last_log_id.as_ref().map(|id| {
                    crate::raft_proto_conv::FromProtoWithVote::from_proto_with_vote(id, &vote)
                }),
                meta.last_membership
                    .as_ref()
                    .map(|m| crate::raft_proto_conv::FromProto::from_proto(m))
                    .expect("Snapshot missing membership"),
            ),
            snapshot_id: meta.snapshot_id.clone(),
        };

        let raft_req = openraft::raft::InstallSnapshotRequest {
            vote,
            meta: snapshot_meta,
            offset: 0,
            data: snapshot_data,
            done: true,
        };

        // Forward to Raft instance
        let raft_resp = self
            .raft
            .install_snapshot(raft_req)
            .await
            .map_err(|e| Status::internal(format!("Raft install_snapshot failed: {}", e)))?;

        // Convert response
        let pb_resp = InstallSnapshotResponse {
            vote: Some(raft_resp.vote.to_proto()),
        };

        Ok(Response::new(pb_resp))
    }
}
