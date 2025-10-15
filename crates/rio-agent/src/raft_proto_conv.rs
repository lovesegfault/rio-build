//! Protobuf conversions for Raft types
//!
//! Uses extension traits to convert between openraft types and protobuf.
//! Extension traits avoid orphan rule issues.

use openraft::raft::{
    AppendEntriesRequest as RaftAppendEntriesRequest,
    AppendEntriesResponse as RaftAppendEntriesResponse, VoteRequest as RaftVoteRequest,
    VoteResponse as RaftVoteResponse,
};
use openraft::{Entry, EntryPayload, LogId, Membership, Vote};
use rio_common::proto;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

use crate::state_machine::{Node, RaftCommand};
use crate::storage::{NodeId, TypeConfig};

// ============================================================================
// Extension traits for conversions
// ============================================================================

pub trait ToProto<T> {
    fn to_proto(&self) -> T;
}

pub trait FromProto<T>: Sized {
    fn from_proto(pb: &T) -> Self;
}

// Special case for LogId which needs Vote context
pub trait FromProtoWithVote<T>: Sized {
    fn from_proto_with_vote(pb: &T, vote: &Vote<NodeId>) -> Self;
}

// ============================================================================
// Vote
// ============================================================================

impl ToProto<proto::Vote> for Vote<NodeId> {
    fn to_proto(&self) -> proto::Vote {
        proto::Vote {
            leader_id: Some(proto::LeaderId {
                term: self.leader_id().term,
                node_id: self.leader_id().node_id.to_string(),
            }),
            committed: self.is_committed(),
        }
    }
}

impl FromProto<proto::Vote> for Vote<NodeId> {
    fn from_proto(pb: &proto::Vote) -> Self {
        let leader_id = pb.leader_id.as_ref().expect("Vote missing leader_id");
        let node_id = uuid::Uuid::parse_str(&leader_id.node_id).expect("Invalid UUID");

        if pb.committed {
            Vote::new_committed(leader_id.term, node_id)
        } else {
            Vote::new(leader_id.term, node_id)
        }
    }
}

// ============================================================================
// LogId
// ============================================================================

impl ToProto<proto::LogId> for LogId<NodeId> {
    fn to_proto(&self) -> proto::LogId {
        proto::LogId {
            term: self.leader_id.term,
            index: self.index,
        }
    }
}

impl FromProtoWithVote<proto::LogId> for LogId<NodeId> {
    fn from_proto_with_vote(pb: &proto::LogId, vote: &Vote<NodeId>) -> Self {
        LogId::new(
            openraft::CommittedLeaderId::new(pb.term, vote.leader_id().node_id),
            pb.index,
        )
    }
}

// ============================================================================
// Node
// ============================================================================

impl ToProto<proto::Node> for Node {
    fn to_proto(&self) -> proto::Node {
        proto::Node {
            rpc_addr: self.rpc_addr.clone(),
        }
    }
}

impl FromProto<proto::Node> for Node {
    fn from_proto(pb: &proto::Node) -> Self {
        Node {
            rpc_addr: pb.rpc_addr.clone(),
        }
    }
}

// ============================================================================
// Membership
// ============================================================================

impl ToProto<proto::Membership> for Membership<NodeId, Node> {
    fn to_proto(&self) -> proto::Membership {
        let configs = self
            .get_joint_config()
            .iter()
            .map(|config| proto::NodeIdSet {
                node_ids: config.iter().map(|id| id.to_string()).collect(),
            })
            .collect();

        let nodes = self
            .nodes()
            .map(|(id, node)| (id.to_string(), node.to_proto()))
            .collect();

        proto::Membership { configs, nodes }
    }
}

impl FromProto<proto::Membership> for Membership<NodeId, Node> {
    fn from_proto(pb: &proto::Membership) -> Self {
        let configs: Vec<BTreeSet<NodeId>> = pb
            .configs
            .iter()
            .map(|set| {
                set.node_ids
                    .iter()
                    .map(|id| uuid::Uuid::parse_str(id).expect("Invalid UUID"))
                    .collect()
            })
            .collect();

        let nodes: BTreeMap<NodeId, Node> = pb
            .nodes
            .iter()
            .map(|(id, node)| {
                (
                    uuid::Uuid::parse_str(id).expect("Invalid UUID"),
                    Node::from_proto(node),
                )
            })
            .collect();

        // Membership::new returns Result in openraft 0.9
        // Membership::new doesn't return Result in our version - it just returns Membership
        Membership::new(configs, nodes)
    }
}

// ============================================================================
// Entry
// ============================================================================

impl ToProto<proto::Entry> for Entry<TypeConfig> {
    fn to_proto(&self) -> proto::Entry {
        let log_id = Some(self.log_id.to_proto());

        let payload = match &self.payload {
            EntryPayload::Blank => Some(proto::entry::Payload::Blank(proto::EmptyPayload {})),
            EntryPayload::Normal(cmd) => {
                let serialized = serde_json::to_vec(cmd).expect("Failed to serialize command");
                Some(proto::entry::Payload::Normal(serialized))
            }
            EntryPayload::Membership(m) => Some(proto::entry::Payload::Membership(m.to_proto())),
        };

        proto::Entry { log_id, payload }
    }
}

impl FromProtoWithVote<proto::Entry> for Entry<TypeConfig> {
    fn from_proto_with_vote(pb: &proto::Entry, vote: &Vote<NodeId>) -> Self {
        let log_id_pb = pb.log_id.as_ref().expect("Entry missing log_id");
        let log_id = LogId::from_proto_with_vote(log_id_pb, vote);

        let payload = match pb.payload.as_ref().expect("Entry missing payload") {
            proto::entry::Payload::Blank(_) => EntryPayload::Blank,
            proto::entry::Payload::Normal(bytes) => {
                let cmd: RaftCommand =
                    serde_json::from_slice(bytes).expect("Failed to deserialize command");
                EntryPayload::Normal(cmd)
            }
            proto::entry::Payload::Membership(m) => {
                EntryPayload::Membership(Membership::from_proto(m))
            }
        };

        Entry { log_id, payload }
    }
}

// ============================================================================
// AppendEntries RPC
// ============================================================================

impl ToProto<proto::AppendEntriesRequest> for RaftAppendEntriesRequest<TypeConfig> {
    fn to_proto(&self) -> proto::AppendEntriesRequest {
        proto::AppendEntriesRequest {
            vote: Some(self.vote.to_proto()),
            prev_log_id: self.prev_log_id.as_ref().map(|id| id.to_proto()),
            entries: self.entries.iter().map(|e| e.to_proto()).collect(),
            leader_commit: self.leader_commit.as_ref().map(|id| id.to_proto()),
        }
    }
}

impl FromProto<proto::AppendEntriesRequest> for RaftAppendEntriesRequest<TypeConfig> {
    fn from_proto(pb: &proto::AppendEntriesRequest) -> Self {
        let vote = Vote::from_proto(pb.vote.as_ref().expect("AppendEntries missing vote"));

        let entries: Vec<Entry<TypeConfig>> = pb
            .entries
            .iter()
            .map(|e| Entry::from_proto_with_vote(e, &vote))
            .collect();

        RaftAppendEntriesRequest {
            vote: vote.clone(),
            prev_log_id: pb
                .prev_log_id
                .as_ref()
                .map(|id| LogId::from_proto_with_vote(id, &vote)),
            entries,
            leader_commit: pb
                .leader_commit
                .as_ref()
                .map(|id| LogId::from_proto_with_vote(id, &vote)),
        }
    }
}

impl ToProto<proto::AppendEntriesResponse> for RaftAppendEntriesResponse<NodeId> {
    fn to_proto(&self) -> proto::AppendEntriesResponse {
        // AppendEntriesResponse is an enum: Success | PartialSuccess(log_id) | HigherVote(vote) | Conflict
        match self {
            RaftAppendEntriesResponse::Success => proto::AppendEntriesResponse {
                vote: None,
                success: true,
                match_log_id: None,
                conflict: false,
            },
            RaftAppendEntriesResponse::PartialSuccess(log_id_opt) => proto::AppendEntriesResponse {
                vote: None,
                success: true,
                match_log_id: log_id_opt.as_ref().map(|id| id.to_proto()),
                conflict: false,
            },
            RaftAppendEntriesResponse::HigherVote(vote) => proto::AppendEntriesResponse {
                vote: Some(vote.to_proto()),
                success: false,
                match_log_id: None,
                conflict: true,
            },
            RaftAppendEntriesResponse::Conflict => proto::AppendEntriesResponse {
                vote: None,
                success: false,
                match_log_id: None,
                conflict: true,
            },
        }
    }
}

impl FromProto<proto::AppendEntriesResponse> for RaftAppendEntriesResponse<NodeId> {
    fn from_proto(pb: &proto::AppendEntriesResponse) -> Self {
        if pb.success {
            if let Some(match_log_id) = &pb.match_log_id {
                // PartialSuccess with matching log
                let vote = pb
                    .vote
                    .as_ref()
                    .map(|v| Vote::from_proto(v))
                    .unwrap_or_else(|| Vote::new(match_log_id.term, NodeId::nil()));
                RaftAppendEntriesResponse::PartialSuccess(Some(LogId::from_proto_with_vote(
                    match_log_id,
                    &vote,
                )))
            } else {
                // Full success
                RaftAppendEntriesResponse::Success
            }
        } else if pb.conflict {
            // HigherVote
            let vote = Vote::from_proto(pb.vote.as_ref().expect("HigherVote missing vote"));
            RaftAppendEntriesResponse::HigherVote(vote)
        } else {
            // Conflict (old log doesn't match)
            RaftAppendEntriesResponse::Conflict
        }
    }
}

// ============================================================================
// Vote RPC
// ============================================================================

impl ToProto<proto::VoteRequest> for RaftVoteRequest<NodeId> {
    fn to_proto(&self) -> proto::VoteRequest {
        proto::VoteRequest {
            vote: Some(self.vote.to_proto()),
            last_log_id: self.last_log_id.as_ref().map(|id| id.to_proto()),
        }
    }
}

impl FromProto<proto::VoteRequest> for RaftVoteRequest<NodeId> {
    fn from_proto(pb: &proto::VoteRequest) -> Self {
        let vote = Vote::from_proto(pb.vote.as_ref().expect("VoteRequest missing vote"));

        RaftVoteRequest {
            vote: vote.clone(),
            last_log_id: pb
                .last_log_id
                .as_ref()
                .map(|id| LogId::from_proto_with_vote(id, &vote)),
        }
    }
}

impl ToProto<proto::VoteResponse> for RaftVoteResponse<NodeId> {
    fn to_proto(&self) -> proto::VoteResponse {
        proto::VoteResponse {
            vote: Some(self.vote.to_proto()),
            vote_granted: self.vote_granted,
            last_log_id: self.last_log_id.as_ref().map(|id| id.to_proto()),
        }
    }
}

impl FromProto<proto::VoteResponse> for RaftVoteResponse<NodeId> {
    fn from_proto(pb: &proto::VoteResponse) -> Self {
        let vote = Vote::from_proto(pb.vote.as_ref().expect("VoteResponse missing vote"));

        RaftVoteResponse {
            vote: vote.clone(),
            vote_granted: pb.vote_granted,
            last_log_id: pb
                .last_log_id
                .as_ref()
                .map(|id| LogId::from_proto_with_vote(id, &vote)),
        }
    }
}
