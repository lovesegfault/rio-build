//! Wire roundtrip tests for proto message defaults.
//!
//! Proto3 scalar fields have implicit defaults (bool → false, int → 0,
//! string → ""). These tests pin that the default survives an
//! encode/decode cycle — i.e., a sender that constructs `::default()`
//! and a receiver that decodes the same bytes agree on the field value.
//! This is the wire-compatibility guarantee for newly added fields:
//! an old sender that doesn't know the field omits it on the wire,
//! and the new receiver reads the proto3 default.

use prost::Message;
use rio_proto::types::{
    DerivationNode, GetSizeClassStatusResponse, HeartbeatRequest, SizeClassStatus,
};

/// `store_degraded` (field 9) defaults to false through a full
/// encode/decode cycle. Wire-compatibility: an old worker that
/// doesn't know field 9 sends nothing for it; the scheduler reads
/// `false` (healthy). If this ever flips to `true` by default,
/// every legacy worker instantly looks store-degraded.
#[test]
fn heartbeat_request_store_degraded_default_false() {
    let req = HeartbeatRequest::default();
    let bytes = req.encode_to_vec();
    let decoded = HeartbeatRequest::decode(&*bytes).unwrap();
    assert!(!decoded.store_degraded);
}

/// `SizeClassStatus` roundtrip. The dashboard (P0236) and autoscaler
/// (P0234) both decode this; a field-number collision or type mismatch
/// would silently zero a field on decode. Pin all six fields survive.
///
/// double fields get NaN-safety from prost (encodes NaN, decodes NaN)
/// but we don't send NaN — the actor computes from finite cutoffs.
/// Pinning a specific nonzero value here catches "field not wired"
/// (which would read as 0.0).
#[test]
fn sizeclass_status_proto_roundtrip() {
    let orig = GetSizeClassStatusResponse {
        classes: vec![
            SizeClassStatus {
                name: "small".into(),
                effective_cutoff_secs: 62.5,
                configured_cutoff_secs: 60.0,
                queued: 5,
                running: 3,
                sample_count: 142,
                queued_by_system: [("x86_64-linux".into(), 4), ("aarch64-linux".into(), 1)].into(),
                running_by_system: [("x86_64-linux".into(), 3)].into(),
            },
            SizeClassStatus {
                name: "large".into(),
                effective_cutoff_secs: 1800.0,
                configured_cutoff_secs: 1800.0,
                queued: 0,
                running: 1,
                sample_count: 17,
                queued_by_system: Default::default(),
                running_by_system: [("aarch64-linux".into(), 1)].into(),
            },
        ],
    };
    let bytes = orig.encode_to_vec();
    let decoded = GetSizeClassStatusResponse::decode(&*bytes).unwrap();
    assert_eq!(orig, decoded);
}

/// `is_content_addressed` (field 11) survives encode/decode at `true`.
/// Catches proto syntax errors before downstream plans (P0250+) hit them:
/// a malformed field declaration would either fail protoc or silently
/// drop to default `false` on decode. Also pins the wire-compat default:
/// an old gateway that doesn't know field 11 omits it; the scheduler
/// reads `false` (input-addressed — the pre-CA-cutoff status quo).
#[test]
fn derivation_node_is_content_addressed_roundtrip() {
    let node = DerivationNode {
        is_content_addressed: true,
        ..Default::default()
    };
    let bytes = node.encode_to_vec();
    let decoded = DerivationNode::decode(&*bytes).unwrap();
    assert!(decoded.is_content_addressed);

    // Default decode is `false` (input-addressed).
    let default_decoded =
        DerivationNode::decode(&*DerivationNode::default().encode_to_vec()).unwrap();
    assert!(!default_decoded.is_content_addressed);
}

/// P0376 domain split: all four .proto files (types / dag / build_types /
/// admin_types) share `package rio.types;` → prost merges into one module.
/// This test is a COMPILE-TIME smoke: if a message moved but wasn't added
/// to build.rs's compile list, or a re-export in lib.rs points at a type
/// that no longer exists, this `use` fails to resolve.
///
/// Also pins the dual-path invariant: `rio_proto::dag::DerivationNode` and
/// `rio_proto::types::DerivationNode` are the SAME type (identity assertion
/// via From/Into is trivial-always-true, but the type-alias equality is
/// what we actually want — both paths compile and refer to one struct).
#[test]
#[allow(clippy::useless_conversion)]
fn p0376_domain_split_reexports_resolve() {
    // dag.proto → rio_proto::dag re-exports
    let _: rio_proto::dag::DerivationNode = rio_proto::types::DerivationNode::default();
    let _: rio_proto::dag::DerivationEdge = rio_proto::types::DerivationEdge::default();
    let _: rio_proto::dag::DerivationEvent = rio_proto::types::DerivationEvent::default();
    let _: rio_proto::dag::GraphNode = rio_proto::types::GraphNode::default();
    let _: rio_proto::dag::GetBuildGraphResponse =
        rio_proto::types::GetBuildGraphResponse::default();

    // build_types.proto → rio_proto::build_types re-exports
    let _: rio_proto::build_types::SubmitBuildRequest =
        rio_proto::types::SubmitBuildRequest::default();
    let _: rio_proto::build_types::BuildResult = rio_proto::types::BuildResult::default();
    let _: rio_proto::build_types::BuildEvent = rio_proto::types::BuildEvent::default();
    let _: rio_proto::build_types::ExecutorMessage = rio_proto::types::ExecutorMessage::default();
    let _: rio_proto::build_types::SchedulerMessage = rio_proto::types::SchedulerMessage::default();
    let _: rio_proto::build_types::HeartbeatRequest = rio_proto::types::HeartbeatRequest::default();
    let _: rio_proto::build_types::BuildResultStatus =
        rio_proto::types::BuildResultStatus::default();

    // admin_types.proto: compiled into types module (no re-export module —
    // admin types are AdminService-only, not shared). Resolution check:
    let _: rio_proto::types::ClusterStatusResponse =
        rio_proto::types::ClusterStatusResponse::default();
    let _: rio_proto::types::SizeClassStatus = rio_proto::types::SizeClassStatus::default();
    let _: rio_proto::types::ExecutorInfo = rio_proto::types::ExecutorInfo::default();

    // Original types.proto residual primitives:
    let _: rio_proto::types::PathInfo = rio_proto::types::PathInfo::default();
    let _: rio_proto::types::ResourceUsage = rio_proto::types::ResourceUsage::default();
    let _: rio_proto::types::BloomFilter = rio_proto::types::BloomFilter::default();
}
