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
use rio_proto::types::{DerivationNode, GetSpawnIntentsResponse, HeartbeatRequest};

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

/// `GetSpawnIntentsResponse` roundtrip. The controller's pool
/// reconcilers decode this; a field-number collision or type mismatch
/// would silently zero a field on decode. Pin all SpawnIntent fields
/// survive — `cores`/`mem_bytes`/`deadline_secs` drive pod resources,
/// `kind`/`system`/`required_features` drive client-side filtering.
#[test]
fn spawn_intents_proto_roundtrip() {
    let orig = GetSpawnIntentsResponse {
        intents: vec![
            rio_proto::types::SpawnIntent {
                intent_id: "i-abc".into(),
                cores: 8,
                mem_bytes: 17_179_869_184,
                disk_bytes: 42_949_672_960,
                node_selector: [("rio.build/hw-band".into(), "mid".into())].into(),
                kind: rio_proto::types::ExecutorKind::Builder.into(),
                system: "x86_64-linux".into(),
                required_features: vec!["kvm".into()],
                deadline_secs: 600,
                executor_token: "tok".into(),
                node_affinity: vec![rio_proto::types::NodeSelectorTerm {
                    match_expressions: vec![rio_proto::types::NodeSelectorRequirement {
                        key: "rio.build/hw-band".into(),
                        operator: "In".into(),
                        values: vec!["mid".into(), "hi".into()],
                    }],
                }],
                eta_seconds: 0.0,
                ready: true,
                hw_class_names: vec!["intel-8".into()],
            },
            rio_proto::types::SpawnIntent {
                intent_id: "i-fod".into(),
                cores: 2,
                mem_bytes: 4 << 30,
                disk_bytes: 16 << 30,
                node_selector: Default::default(),
                kind: rio_proto::types::ExecutorKind::Fetcher.into(),
                system: "aarch64-linux".into(),
                required_features: vec![],
                deadline_secs: 300,
                executor_token: String::new(),
                node_affinity: vec![],
                eta_seconds: 42.5,
                ready: false,
                hw_class_names: vec![],
            },
        ],
        queued_by_system: [("x86_64-linux".into(), 4), ("aarch64-linux".into(), 1)].into(),
        dead_nodes: vec!["ip-10-0-1-42".into()],
        ice_masked_cells: vec!["mid:spot".into()],
    };
    let bytes = orig.encode_to_vec();
    let decoded = GetSpawnIntentsResponse::decode(&*bytes).unwrap();
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
