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
        // P0556: same SizeClassStatus shape, cutoffs zeroed.
        fod_classes: vec![SizeClassStatus {
            name: "tiny".into(),
            queued: 7,
            running: 2,
            queued_by_system: [("x86_64-linux".into(), 7)].into(),
            ..Default::default()
        }],
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

/// All four data-type .proto files (types / dag / build_types /
/// admin_types) share `package rio.types;` → prost merges into one
/// module. COMPILE-TIME smoke: if a message moved files but wasn't added
/// to build.rs's compile list, this `use` fails to resolve. One
/// representative type per source file.
#[test]
fn types_module_merges_all_proto_files() {
    let _ = rio_proto::types::DerivationNode::default(); // dag.proto
    let _ = rio_proto::types::SubmitBuildRequest::default(); // build_types.proto
    let _ = rio_proto::types::ClusterStatusResponse::default(); // admin_types.proto
    let _ = rio_proto::types::PathInfo::default(); // types.proto
}
