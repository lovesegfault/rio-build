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
/// Carries every proto field the scheduler reads. `drv_content` stays
/// `bytes::Bytes` (prost's zero-copy type) — it's an opaque ATerm blob
/// the scheduler only stores and forwards, never parses.
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
    pub input_srcs_nar_size: u64,
    pub is_content_addressed: bool,
    /// Decoded `ca_modular_hash` — `Some` iff the wire field was
    /// exactly 32 bytes. The proto carries raw `bytes`; downstream
    /// (`DerivationState::try_from_node`, `check_cached_outputs`
    /// floating-CA lane) wants `[u8; 32]`. Decoding once here means
    /// callers branch on `Option`, not length.
    pub ca_modular_hash: Option<[u8; 32]>,
    pub needs_resolve: bool,
}

impl From<proto::DerivationNode> for DerivationNode {
    fn from(n: proto::DerivationNode) -> Self {
        Self {
            ca_modular_hash: n.ca_modular_hash.as_slice().try_into().ok(),
            drv_path: n.drv_path,
            drv_hash: n.drv_hash,
            pname: n.pname,
            system: n.system,
            required_features: n.required_features,
            output_names: n.output_names,
            is_fixed_output: n.is_fixed_output,
            expected_output_paths: n.expected_output_paths,
            drv_content: n.drv_content.to_vec(),
            input_srcs_nar_size: n.input_srcs_nar_size,
            is_content_addressed: n.is_content_addressed,
            needs_resolve: n.needs_resolve,
        }
    }
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
            // `times_built` dropped: scheduler never reads it
            // (repeat-build counting is gateway-side).
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
