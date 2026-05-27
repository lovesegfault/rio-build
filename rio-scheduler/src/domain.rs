//! Scheduler-internal domain types decoupled from `rio_proto` wire types.
//!
//! The DAG, state machine, and dispatch/completion pipelines operate on
//! these instead of `rio_proto::types::*` directly. Conversion happens
//! once at the actor boundary (top of `validate_and_ingest` /
//! `handle_completion`); everything downstream is wire-agnostic.
//!
//! Why a separate layer instead of using proto types end-to-end:
//!
//! - **Field name stability.** Proto field renames (or proto3's
//!   `optional`-wrapping churn) currently ripple through `dag/`,
//!   `state/`, `merge.rs`, and `completion.rs`. With a domain layer the
//!   blast radius is the `From` impl.
//! - **Invariants at the boundary.** `BuildResult::from` normalizes the
//!   raw `i32` status to a `BuildResultStatus` enum once;
//!   `DerivationNode::from` decodes `ca_modular_hash` once. Downstream
//!   code reads typed fields instead of re-validating.
//! - **Prost coupling.** `prost_types::Timestamp` and `bytes::Bytes`
//!   leak into hot-path code today; domain types use `SystemTime` /
//!   `Vec<u8>`.
//!
//! `From<proto>` is provided for every type so the gRPC layer (and
//! tests that still construct proto structs) can `.into()` at the seam.
//! [`ActorCommand`](crate::actor::ActorCommand) variants intentionally
//! keep proto-typed fields for now — `actor/tests/` and
//! `rio-test-support` build them directly and migrating those is a
//! separate (b03) integration step.
//!
//! `ResourceUsage` and `ExecutorKind` are NOT wrapped:
//! `ResourceUsage` is a leaf telemetry struct that flows straight back
//! out via `AdminService.ListExecutors` (round-trip would be
//! `proto→domain→proto` for no gain), and `ExecutorKind` is a plain
//! `#[repr(i32)]` enum already used as a domain value.

use std::time::SystemTime;

use rio_proto::types as proto;

/// Domain mirror of [`rio_proto::types::DerivationNode`].
///
/// Carries every proto field the scheduler reads.
#[derive(Debug, Clone)]
pub struct DerivationNode {
    pub drv_path: String,
    pub drv_hash: String,
    pub pname: String,
    pub system: String,
    pub required_features: Vec<String>,
    pub output_names: Vec<String>,
    pub is_fixed_output: bool,
    pub expected_output_paths: Vec<String>,
    /// Opaque ATerm blob the scheduler only stores and forwards
    /// (`WorkAssignment.drv_content`); never parsed. `Vec<u8>` instead
    /// of prost's `Bytes` so this module doesn't pull in the `bytes`
    /// crate — the one extra copy at the boundary is per-merge, not
    /// per-tick.
    pub drv_content: Vec<u8>,
    pub is_content_addressed: bool,
    /// Decoded `ca_modular_hash` — `Some` iff the wire field was
    /// exactly 32 bytes. The proto carries raw `bytes`; downstream
    /// (`DerivationState::try_from_node`, `check_cached_outputs`
    /// floating-CA lane) wants `[u8; 32]`. Decoding once here means
    /// callers branch on `Option`, not length.
    pub ca_modular_hash: Option<[u8; 32]>,
    pub needs_resolve: bool,
    /// ADR-023 sizing inputs — gateway extracts from drv.env. All
    /// optional: absent ≠ false/empty (see dag.proto field comments).
    pub version: Option<String>,
    pub enable_parallel_building: Option<bool>,
    pub enable_parallel_checking: Option<bool>,
    pub prefer_local_build: Option<bool>,
}

impl From<proto::DerivationNode> for DerivationNode {
    fn from(n: proto::DerivationNode) -> Self {
        let drv_content = n.drv_content.to_vec();
        let (output_names, expected_output_paths) =
            backfill_outputs_from_drv(n.output_names, n.expected_output_paths, &drv_content);
        Self {
            ca_modular_hash: n.ca_modular_hash.as_slice().try_into().ok(),
            drv_path: n.drv_path,
            drv_hash: n.drv_hash,
            pname: n.pname,
            system: n.system,
            required_features: n.required_features,
            output_names,
            is_fixed_output: n.is_fixed_output,
            expected_output_paths,
            drv_content,
            is_content_addressed: n.is_content_addressed,
            needs_resolve: n.needs_resolve,
            version: n.version,
            enable_parallel_building: n.enable_parallel_building,
            enable_parallel_checking: n.enable_parallel_checking,
            prefer_local_build: n.prefer_local_build,
        }
    }
}

/// Backfill `expected_output_paths` (and, if absent, `output_names`)
/// from the inlined ATerm when the submitter did not provide them.
///
/// The gateway always populates both (parallel, in `drv.outputs()`
/// order), but `SubmitBuild` is also driven directly over gRPC (tests,
/// future CLI/admin paths) with only `drv_content` + `output_names`.
/// Leaving `expected_output_paths` empty there is not a benign
/// degradation: the dispatch-time HMAC assignment token copies it into
/// its `expected_outputs` claim, and rio-store rejects every non-CA
/// upload whose path is not in that claim — so a direct submission's
/// build always completes and then fails its upload with
/// `PermissionDenied`, classified as an infrastructure failure and
/// retried until the build is reaped (P0560 round 3b finding (c)). The
/// scheduler has the `.drv` bytes; deriving the paths here makes the
/// claim authorize exactly the derivation's own outputs, the same set
/// the gateway would have sent.
///
/// Rules:
/// - `expected_output_paths` already provided → passthrough untouched
///   (the gateway path; also floating-CA's `[""]` convention).
/// - empty + `drv_content` parses:
///   - `output_names` provided → resolve each name against the parsed
///     outputs (unknown name → `""`, matching the floating-CA "unknown
///     until built" convention rather than inventing a path).
///   - `output_names` empty → take both names and paths from the drv.
/// - empty + no/unparseable `drv_content` → leave empty (sparse node;
///   dispatch falls back to its store-derived seeds and the upload
///   keeps the pre-existing behavior).
fn backfill_outputs_from_drv(
    output_names: Vec<String>,
    expected_output_paths: Vec<String>,
    drv_content: &[u8],
) -> (Vec<String>, Vec<String>) {
    if !expected_output_paths.is_empty() || drv_content.is_empty() {
        return (output_names, expected_output_paths);
    }
    let Some(drv) = std::str::from_utf8(drv_content)
        .ok()
        .and_then(|s| rio_nix::derivation::Derivation::parse(s).ok())
    else {
        return (output_names, expected_output_paths);
    };
    let outputs = drv.outputs();
    if outputs.is_empty() {
        return (output_names, expected_output_paths);
    }
    if output_names.is_empty() {
        let (names, paths) = outputs
            .iter()
            .map(|o| (o.name().to_string(), o.path().to_string()))
            .unzip();
        tracing::debug!(
            outputs = outputs.len(),
            "backfilled output names + expected paths from inlined drv_content"
        );
        return (names, paths);
    }
    let paths: Vec<String> = output_names
        .iter()
        .map(|name| {
            outputs
                .iter()
                .find(|o| o.name() == name)
                .map(|o| o.path().to_string())
                .unwrap_or_default()
        })
        .collect();
    tracing::debug!(
        outputs = paths.len(),
        "backfilled expected output paths from inlined drv_content"
    );
    (output_names, paths)
}

/// Domain mirror of [`rio_proto::types::DerivationEdge`].
#[derive(Debug, Clone)]
pub struct DerivationEdge {
    pub parent_drv_path: String,
    pub child_drv_path: String,
}

impl From<proto::DerivationEdge> for DerivationEdge {
    fn from(e: proto::DerivationEdge) -> Self {
        Self {
            parent_drv_path: e.parent_drv_path,
            child_drv_path: e.child_drv_path,
        }
    }
}

/// Domain mirror of [`rio_proto::types::BuiltOutput`].
#[derive(Debug, Clone)]
pub struct BuiltOutput {
    pub output_name: String,
    pub output_path: String,
    /// Raw NAR SHA-256. Kept as `Vec<u8>` (not `[u8; 32]`) because the
    /// CA-compare path tolerates absent/short hashes from older workers
    /// — `complete_ca_bookkeeping` already length-checks at point of
    /// use.
    pub output_hash: Vec<u8>,
}

impl From<proto::BuiltOutput> for BuiltOutput {
    fn from(o: proto::BuiltOutput) -> Self {
        Self {
            output_name: o.output_name,
            output_path: o.output_path,
            output_hash: o.output_hash.to_vec(),
        }
    }
}

/// Domain mirror of [`rio_proto::types::BuildResult`].
///
/// `status` is normalized from the wire `i32` to the enum here so the
/// completion pipeline matches on a typed value. Unknown wire values
/// map to `Unspecified` (same fallback `handle_completion` already
/// applied inline — moved here so it happens once at the boundary).
#[derive(Debug, Clone)]
pub struct BuildResult {
    pub status: proto::BuildResultStatus,
    pub error_msg: String,
    pub start_time: Option<SystemTime>,
    pub stop_time: Option<SystemTime>,
    pub built_outputs: Vec<BuiltOutput>,
}

impl From<proto::BuildResult> for BuildResult {
    fn from(r: proto::BuildResult) -> Self {
        let status = proto::BuildResultStatus::try_from(r.status).unwrap_or_else(|_| {
            tracing::warn!(
                status = r.status,
                "unknown BuildResultStatus from worker, treating as Unspecified"
            );
            proto::BuildResultStatus::Unspecified
        });
        Self {
            status,
            error_msg: r.error_msg,
            start_time: r.start_time.and_then(to_system_time),
            stop_time: r.stop_time.and_then(to_system_time),
            built_outputs: r.built_outputs.into_iter().map(Into::into).collect(),
        }
    }
}

impl BuildResult {
    /// Wall-clock build duration if both timestamps are present and
    /// ordered. Replaces the ad-hoc `prost_types::Timestamp` arithmetic
    /// scattered across completion.rs.
    pub fn duration(&self) -> Option<std::time::Duration> {
        self.stop_time?.duration_since(self.start_time?).ok()
    }
}

/// `prost_types::Timestamp` → `SystemTime`. Out-of-range values (proto
/// allows ±10000 years) clamp to `None` rather than panicking — a
/// malformed worker timestamp shouldn't take down the actor.
fn to_system_time(ts: prost_types::Timestamp) -> Option<SystemTime> {
    SystemTime::try_from(ts).ok()
}

/// Convert a borrowed proto-node slice to owned domain nodes.
/// Convenience for the handful of call sites that hold
/// `&[proto::DerivationNode]` (test fixtures, gRPC layer).
pub fn nodes_from_proto(nodes: Vec<proto::DerivationNode>) -> Vec<DerivationNode> {
    nodes.into_iter().map(Into::into).collect()
}

/// See [`nodes_from_proto`].
pub fn edges_from_proto(edges: Vec<proto::DerivationEdge>) -> Vec<DerivationEdge> {
    edges.into_iter().map(Into::into).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two-output ATerm used by the backfill tests. `out` and `dev`
    /// have distinct, recognizable store paths.
    const TWO_OUTPUT_ATERM: &str = r#"Derive([("dev","/nix/store/dddddddddddddddddddddddddddddddd-hello-dev","",""),("out","/nix/store/oooooooooooooooooooooooooooooooo-hello","","")],[],["/nix/store/abc-source.sh"],"x86_64-linux","/bin/bash",["-e","script.sh"],[("name","hello"),("system","x86_64-linux")])"#;

    fn proto_node(
        output_names: Vec<String>,
        expected_output_paths: Vec<String>,
        drv_content: &str,
    ) -> proto::DerivationNode {
        proto::DerivationNode {
            drv_path: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello.drv".into(),
            drv_hash: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello.drv".into(),
            output_names,
            expected_output_paths,
            drv_content: drv_content.as_bytes().to_vec(),
            system: "x86_64-linux".into(),
            ..Default::default()
        }
    }

    /// gRPC-direct submissions (tests, CLI) send `drv_content` +
    /// `output_names` but no `expected_output_paths`. The conversion
    /// must derive the paths from the ATerm so the dispatch-time HMAC
    /// claim authorizes the drv's own outputs (P0560 round 3b finding
    /// (c): empty claims → every upload PermissionDenied → infra-retry
    /// loop → reaped build).
    #[test]
    fn backfills_expected_output_paths_from_drv_content() {
        let node: DerivationNode = proto_node(vec!["out".into()], vec![], TWO_OUTPUT_ATERM).into();
        assert_eq!(node.output_names, vec!["out".to_string()]);
        assert_eq!(
            node.expected_output_paths,
            vec!["/nix/store/oooooooooooooooooooooooooooooooo-hello".to_string()],
            "path must be resolved by NAME so it stays parallel to output_names"
        );

        // No names either → both come from the drv, parallel and in drv
        // output order.
        let node: DerivationNode = proto_node(vec![], vec![], TWO_OUTPUT_ATERM).into();
        assert_eq!(
            node.output_names,
            vec!["dev".to_string(), "out".to_string()]
        );
        assert_eq!(
            node.expected_output_paths,
            vec![
                "/nix/store/dddddddddddddddddddddddddddddddd-hello-dev".to_string(),
                "/nix/store/oooooooooooooooooooooooooooooooo-hello".to_string(),
            ]
        );

        // A name the drv does not declare resolves to "" (unknown), not
        // a fabricated path.
        let node: DerivationNode =
            proto_node(vec!["out".into(), "doc".into()], vec![], TWO_OUTPUT_ATERM).into();
        assert_eq!(
            node.expected_output_paths,
            vec![
                "/nix/store/oooooooooooooooooooooooooooooooo-hello".to_string(),
                String::new(),
            ]
        );
    }

    /// Provided values pass through untouched (gateway path, including
    /// the floating-CA `[""]` convention), and nodes without parseable
    /// drv_content stay as submitted.
    #[test]
    fn backfill_leaves_provided_or_sparse_nodes_alone() {
        // Gateway-style: expected paths provided → passthrough.
        let node: DerivationNode = proto_node(
            vec!["out".into()],
            vec!["/nix/store/cccccccccccccccccccccccccccccccc-custom".into()],
            TWO_OUTPUT_ATERM,
        )
        .into();
        assert_eq!(
            node.expected_output_paths,
            vec!["/nix/store/cccccccccccccccccccccccccccccccc-custom".to_string()]
        );

        // Floating-CA convention: [""] is "provided", not "missing".
        let node: DerivationNode =
            proto_node(vec!["out".into()], vec![String::new()], TWO_OUTPUT_ATERM).into();
        assert_eq!(node.expected_output_paths, vec![String::new()]);

        // Sparse node (no drv_content) → nothing to derive from.
        let node: DerivationNode = proto_node(vec!["out".into()], vec![], "").into();
        assert!(node.expected_output_paths.is_empty());

        // Unparseable drv_content → leave empty rather than guess.
        let node: DerivationNode = proto_node(vec!["out".into()], vec![], "not an aterm").into();
        assert!(node.expected_output_paths.is_empty());
    }
}
