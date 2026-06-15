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
use rio_proto::types::{DerivationNode, GetSpawnIntentsResponse, ReportOutcomeRequest};

/// `ReportOutcomeRequest.exec_id` defaults to empty through a full
/// encode/decode cycle (the no-attempt no-op key: an empty/unknown
/// exec_id is acknowledged and ignored). Wire-compatibility for the
/// pull-mode report payload: a sender that omits the field and a
/// receiver decoding the same bytes agree on the empty default.
#[test]
fn report_outcome_request_exec_id_default_empty() {
    let req = ReportOutcomeRequest::default();
    let bytes = req.encode_to_vec();
    let decoded = ReportOutcomeRequest::decode(&*bytes).unwrap();
    assert!(decoded.exec_id.is_empty());
    assert!(decoded.report.is_none());
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
                node_affinity: vec![rio_proto::types::NodeSelectorTerm {
                    match_expressions: vec![rio_proto::types::NodeSelectorRequirement {
                        key: "rio.build/hw-band".into(),
                        operator: "In".into(),
                        values: vec!["mid".into(), "hi".into()],
                    }],
                }],
                eta_seconds: 0.0,
                ready: Some(true),
                hw_class_names: vec!["intel-8".into()],
                disk_headroom_factor: Some(1.32),
                excluded_nodes: vec!["ip-10-0-1-5.internal".into()],
                resubmit_cycle: 3,
                // bug_121: the non-default value, so the wire actually
                // carries the pin (the silent-decay red rode absence).
                capacity_pin: Some("od".into()),
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
                node_affinity: vec![],
                eta_seconds: 42.5,
                ready: Some(false),
                hw_class_names: vec![],
                disk_headroom_factor: None,
                excluded_nodes: vec![],
                resubmit_cycle: 0,
                capacity_pin: None,
            },
        ],
        queued_by_system: [("x86_64-linux".into(), 4), ("aarch64-linux".into(), 1)].into(),
        // Round-10 merged_bug_006: the forecast population class
        // roundtrips beside its Ready sibling — distinct counts so a
        // field-number collision between the two maps cannot decode
        // cleanly.
        forecast_by_system: [("aarch64-linux".into(), 2)].into(),
        ice_masked_cells: vec!["mid:spot".into()],
        // Round-9 B3: the truncation-honesty flag roundtrips (true is
        // the non-default value, so the wire actually carries it).
        truncated: true,
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

/// `SourceRoot.root_node` (field 6) back-compat: an old eval worker
/// that doesn't know the field encodes nothing for it, and the
/// coordinator decodes `None` — which it must treat as a directory
/// root keyed by `dir_digest` (the only shape old workers ever sent).
/// Bytes are crafted without tag 6 (an old-shape encode), not just
/// `::default()`, so a future non-optional re-declaration that changes
/// absent-field semantics trips this test.
#[test]
fn source_root_without_root_node_decodes_as_dir_root() {
    let old_shape = rio_proto::evaljob::SourceRoot {
        store_path: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-src".into(),
        dir_digest: vec![7; 32],
        nar_hash: vec![9; 32],
        nar_size: 123,
        origin: "/home/user/src".into(),
        root_node: None,
    };
    // An unset message field encodes nothing (prost emits the tag only
    // for `Some`), so these are exactly the bytes an old worker sends.
    let bytes = old_shape.encode_to_vec();
    let decoded = rio_proto::evaljob::SourceRoot::decode(&*bytes).unwrap();
    assert!(decoded.root_node.is_none());
    assert_eq!(decoded.dir_digest, vec![7; 32]);
    assert_eq!(decoded.origin, "/home/user/src");

    // New-shape roundtrip: a populated RootNode survives.
    let file_root = rio_proto::evaljob::SourceRoot {
        root_node: Some(rio_proto::castore::RootNode {
            node: Some(rio_proto::castore::root_node::Node::File(
                rio_proto::castore::FileEntry {
                    name: vec![],
                    digest: vec![1; 32],
                    size: 4,
                    executable: true,
                },
            )),
        }),
        ..old_shape
    };
    let decoded = rio_proto::evaljob::SourceRoot::decode(&*file_root.encode_to_vec()).unwrap();
    assert_eq!(decoded.root_node, file_root.root_node);
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

/// `AppendLogAck.open_coverage_next_line` back-compat (the wire-1
/// coverage-ack letter, read-side-first): bytes encoded WITHOUT tag 2 —
/// a legacy server's chunk ack — decode to `None`, the legacy
/// semantics (a plain chunk ack; the client trims nothing at open).
/// Presence is the open-ack discriminator, so `Some(0)` and `None`
/// must stay distinguishable through the wire: an open ack with empty
/// coverage is typed as the letter (no trim, but the letter was
/// spoken) while absence means the letter was never sent.
#[test]
fn append_log_ack_open_coverage_absent_is_legacy() {
    use rio_proto::store::AppendLogAck;

    // A legacy chunk ack: only field 1 on the wire.
    let legacy = AppendLogAck {
        durable_through_line: 41,
        open_coverage_next_line: None,
    };
    let bytes = legacy.encode_to_vec();
    let decoded = AppendLogAck::decode(&*bytes).unwrap();
    assert_eq!(decoded.durable_through_line, 41);
    assert_eq!(
        decoded.open_coverage_next_line, None,
        "absent tag 2 decodes as the legacy chunk-ack semantics"
    );

    // The open-time coverage ack: presence survives the roundtrip,
    // including at the zero value (presence != value).
    for watermark in [0u64, 7] {
        let open_ack = AppendLogAck {
            durable_through_line: watermark.saturating_sub(1),
            open_coverage_next_line: Some(watermark),
        };
        let decoded = AppendLogAck::decode(&*open_ack.encode_to_vec()).unwrap();
        assert_eq!(decoded.open_coverage_next_line, Some(watermark));
    }
}
