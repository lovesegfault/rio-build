//! Digest-bearing `SubmitBuild` handling (ADR-024 P2a).
//!
//! When every node in a submission carries `drv_digest`, dependency
//! edges derive from `input_drv_digests` and the request's `edges`
//! list is ignored. The legacy edges path is untouched: a submission
//! with no digests flows through `SubmitBuild` exactly as before.
//!
//! Two hard boundaries enforced here, both submit-time rejects:
//!
//! - **No silent half-modes.** A submission where some nodes carry
//!   digests and some don't is `INVALID_ARGUMENT`. The C13 hazard this
//!   guards: edges are ignored in digest mode, so a node that joined a
//!   digest submission without digest references would silently lose
//!   ALL its dependency edges and dispatch concurrently with its
//!   inputs — a mis-build, not an error.
//! - **Every referenced digest must be known.** Before the submission
//!   reaches the actor, every `drv_digest` and `input_drv_digests`
//!   entry must resolve against this submission's own nodes or the
//!   store's `drv_blobs` (shared PG, tenant-scoped — see
//!   [`SchedulerDb::resolve_drv_digests`]). The reject names ALL
//!   missing digests, not first-fail: the client's stale-ack recovery
//!   re-`Has`-es exactly that list, re-uploads, and resubmits. Store
//!   verification failure (PG down) also rejects — deny on failure,
//!   never accept unverified digests.
//!
//! [`SchedulerDb::resolve_drv_digests`]: crate::db::SchedulerDb::resolve_drv_digests

use std::collections::{HashMap, HashSet};

use tonic::Status;

use rio_proto::types::{DerivationEdge, DerivationNode};

/// blake3 digest length — the only valid length for `drv_digest` /
/// `input_drv_digests` entries.
const DIGEST_LEN: usize = 32;

fn hex(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// Outcome of [`classify_and_derive_edges`] for a digest-bearing
/// submission.
#[derive(Debug)]
pub(super) struct DigestEdges {
    /// Edges derived from in-submission `input_drv_digests`
    /// references. External references are appended by the caller
    /// after store resolution.
    pub edges: Vec<DerivationEdge>,
    /// `(parent_drv_path, digest)` pairs for input digests that did
    /// not match any node in this submission — they must resolve
    /// against the store's `drv_blobs` or the submission is rejected.
    pub external: Vec<(String, Vec<u8>)>,
    /// Every node's own `(digest, drv_path)` — the caller verifies
    /// each exists in the store (the skeleton contract: the client /
    /// gateway uploaded all drv blobs before submitting).
    pub own: Vec<(Vec<u8>, String)>,
}

/// Classify a submission and derive edges when it is digest-bearing.
///
/// Returns `Ok(None)` for a legacy submission (no node carries
/// `drv_digest` or `input_drv_digests` — the caller uses the
/// request's `edges` unchanged), `Ok(Some(_))` for an all-digest
/// submission, and `INVALID_ARGUMENT` for mixed or structurally
/// malformed digest fields.
// r[impl sched.submit.digest-edges]
pub(super) fn classify_and_derive_edges(
    nodes: &[DerivationNode],
) -> Result<Option<DigestEdges>, Status> {
    let bearing = nodes.iter().filter(|n| !n.drv_digest.is_empty()).count();
    if bearing == 0 {
        // Legacy mode — but a digest-less node referencing inputs by
        // digest is a half-mode too (its references would be silently
        // dropped). Reject rather than ignore.
        if let Some(n) = nodes.iter().find(|n| !n.input_drv_digests.is_empty()) {
            return Err(Status::invalid_argument(format!(
                "node {} carries input_drv_digests without drv_digest \
                 (digest-bearing submissions must set drv_digest on every node)",
                n.drv_hash
            )));
        }
        return Ok(None);
    }
    if bearing < nodes.len() {
        let missing: Vec<&str> = nodes
            .iter()
            .filter(|n| n.drv_digest.is_empty())
            .take(5)
            .map(|n| n.drv_hash.as_str())
            .collect();
        return Err(Status::invalid_argument(format!(
            "mixed submission: {bearing}/{} nodes carry drv_digest; \
             digest-less nodes (first 5): {missing:?} — no silent half-modes, \
             either every node carries digests or none does",
            nodes.len()
        )));
    }

    // All nodes carry digests. Validate shape + uniqueness, index by
    // digest, then derive edges.
    let mut by_digest: HashMap<&[u8], &str> = HashMap::with_capacity(nodes.len());
    for node in nodes {
        if node.drv_digest.len() != DIGEST_LEN {
            return Err(Status::invalid_argument(format!(
                "node {} drv_digest must be {DIGEST_LEN} bytes (got {})",
                node.drv_hash,
                node.drv_digest.len()
            )));
        }
        // Two distinct nodes with one digest is inconsistent by
        // construction (the digest keys the canonical drv content,
        // which embeds the output paths — one content, one drv_path).
        // Same-node duplicates are already rejected via drv_hash.
        if by_digest
            .insert(node.drv_digest.as_slice(), node.drv_path.as_str())
            .is_some()
        {
            return Err(Status::invalid_argument(format!(
                "duplicate drv_digest {} in nodes[]",
                hex(&node.drv_digest)
            )));
        }
    }

    let mut edges = Vec::new();
    let mut external = Vec::new();
    for node in nodes {
        for d in &node.input_drv_digests {
            if d.len() != DIGEST_LEN {
                return Err(Status::invalid_argument(format!(
                    "node {} input_drv_digest must be {DIGEST_LEN} bytes (got {})",
                    node.drv_hash,
                    d.len()
                )));
            }
            match by_digest.get(d.as_slice()) {
                Some(child_path) => edges.push(DerivationEdge {
                    parent_drv_path: node.drv_path.clone(),
                    child_drv_path: (*child_path).to_string(),
                }),
                None => external.push((node.drv_path.clone(), d.clone())),
            }
        }
    }
    rio_common::grpc::check_bound("edges", edges.len(), rio_common::limits::MAX_DAG_EDGES)?;

    let own = nodes
        .iter()
        .map(|n| (n.drv_digest.clone(), n.drv_path.clone()))
        .collect();
    Ok(Some(DigestEdges {
        edges,
        external,
        own,
    }))
}

/// Build the bulk-verify digest set (own ∪ external, deduped) and the
/// reject message for unresolved digests.
///
/// `resolved` maps digest → stored drv_path. Verification:
///
/// - every digest must be in `resolved` — collect ALL misses and
///   reject naming each (`FAILED_PRECONDITION`: the client re-Has-es
///   the listed digests, re-uploads, resubmits);
/// - a node's own digest must resolve to the node's claimed
///   `drv_path` — a mismatch means the skeleton lies about its
///   content binding (the store verified digest↔drv_path at put);
/// - external references become edges to the resolved drv_path. If
///   that path is not in the scheduler's DAG the merge drops the edge
///   (existing warn-skip semantics) — the referenced drv is known to
///   the store but not part of any live build, so output-level
///   readiness is the cache classifier's job, same as a legacy
///   submission whose dependency closure was gateway-pruned.
// r[impl sched.submit.digest-verify]
pub(super) fn verify_resolved(
    d: &DigestEdges,
    resolved: &HashMap<Vec<u8>, String>,
) -> Result<Vec<DerivationEdge>, Status> {
    let mut missing: Vec<String> = Vec::new();
    let mut seen_missing: HashSet<&[u8]> = HashSet::new();
    for (digest, claimed_path) in &d.own {
        match resolved.get(digest) {
            None => {
                if seen_missing.insert(digest.as_slice()) {
                    missing.push(hex(digest));
                }
            }
            Some(stored_path) if stored_path != claimed_path => {
                return Err(Status::invalid_argument(format!(
                    "node drv_digest {} is stored under drv_path {stored_path:?} \
                     but the submission claims {claimed_path:?}",
                    hex(digest)
                )));
            }
            Some(_) => {}
        }
    }
    let mut external_edges = Vec::with_capacity(d.external.len());
    for (parent_path, digest) in &d.external {
        match resolved.get(digest) {
            Some(child_path) => external_edges.push(DerivationEdge {
                parent_drv_path: parent_path.clone(),
                child_drv_path: child_path.clone(),
            }),
            None => {
                if seen_missing.insert(digest.as_slice()) {
                    missing.push(hex(digest));
                }
            }
        }
    }
    if !missing.is_empty() {
        missing.sort();
        // The message format is a CONTRACT: the build client's
        // stale-ack recovery parses the digest list back out of it
        // (`r[bc.submit.stale-ack-once]`). Formatter and parser share
        // rio_proto::submit_reject so they cannot drift.
        return Err(Status::failed_precondition(
            rio_proto::submit_reject::missing_drv_digests_message(&missing),
        ));
    }
    Ok(external_edges)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(tag: &str, digest: Option<u8>, inputs: &[u8]) -> DerivationNode {
        let mut n = rio_test_support::fixtures::make_derivation_node(tag, "x86_64-linux");
        if let Some(b) = digest {
            n.drv_digest = vec![b; 32];
        }
        n.input_drv_digests = inputs.iter().map(|b| vec![*b; 32]).collect();
        n
    }

    #[test]
    fn legacy_submission_passes_through() {
        let nodes = vec![node("a", None, &[]), node("b", None, &[])];
        assert!(classify_and_derive_edges(&nodes).unwrap().is_none());
    }

    #[test]
    fn mixed_submission_rejected() {
        let nodes = vec![node("a", Some(1), &[]), node("b", None, &[])];
        let e = classify_and_derive_edges(&nodes).unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
        assert!(e.message().contains("mixed submission"), "{}", e.message());
    }

    #[test]
    fn inputs_without_own_digest_rejected() {
        let nodes = vec![node("a", None, &[2])];
        let e = classify_and_derive_edges(&nodes).unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
        assert!(
            e.message().contains("without drv_digest"),
            "{}",
            e.message()
        );
    }

    #[test]
    fn wrong_length_digest_rejected() {
        let mut n = node("a", Some(1), &[]);
        n.drv_digest = vec![1; 31];
        let e = classify_and_derive_edges(&[n]).unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
        assert!(e.message().contains("32 bytes"), "{}", e.message());
    }

    #[test]
    fn duplicate_digest_rejected() {
        let nodes = vec![node("a", Some(1), &[]), node("b", Some(1), &[])];
        let e = classify_and_derive_edges(&nodes).unwrap_err();
        assert!(
            e.message().contains("duplicate drv_digest"),
            "{}",
            e.message()
        );
    }

    #[test]
    fn edges_derive_from_in_submission_digests() {
        // parent(3) → {a(1), b(2)}; a, b leaves.
        let nodes = vec![
            node("a", Some(1), &[]),
            node("b", Some(2), &[]),
            node("p", Some(3), &[1, 2]),
        ];
        let d = classify_and_derive_edges(&nodes).unwrap().unwrap();
        assert!(d.external.is_empty());
        let pairs: Vec<(String, String)> = d
            .edges
            .iter()
            .map(|e| (e.parent_drv_path.clone(), e.child_drv_path.clone()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                (nodes[2].drv_path.clone(), nodes[0].drv_path.clone()),
                (nodes[2].drv_path.clone(), nodes[1].drv_path.clone()),
            ]
        );
    }

    #[test]
    fn unknown_digest_collected_as_external() {
        let nodes = vec![node("p", Some(3), &[9])];
        let d = classify_and_derive_edges(&nodes).unwrap().unwrap();
        assert!(d.edges.is_empty());
        assert_eq!(d.external, vec![(nodes[0].drv_path.clone(), vec![9; 32])]);
    }

    #[test]
    fn verify_lists_all_missing_digests() {
        let nodes = vec![node("a", Some(1), &[8]), node("b", Some(2), &[9])];
        let d = classify_and_derive_edges(&nodes).unwrap().unwrap();
        // Store knows only node a's own digest: b's own + both
        // externals are missing — ALL three must be named.
        let resolved = HashMap::from([(vec![1u8; 32], nodes[0].drv_path.clone())]);
        let e = verify_resolved(&d, &resolved).unwrap_err();
        assert_eq!(e.code(), tonic::Code::FailedPrecondition);
        for missing in [vec![2u8; 32], vec![8; 32], vec![9; 32]] {
            assert!(
                e.message().contains(&hex(&missing)),
                "reject must name {} — got: {}",
                hex(&missing),
                e.message()
            );
        }
    }

    #[test]
    fn verify_rejects_drv_path_mismatch() {
        let nodes = vec![node("a", Some(1), &[])];
        let d = classify_and_derive_edges(&nodes).unwrap().unwrap();
        let resolved = HashMap::from([(vec![1u8; 32], "/nix/store/other.drv".to_string())]);
        let e = verify_resolved(&d, &resolved).unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
        assert!(e.message().contains("stored under"), "{}", e.message());
    }

    #[test]
    fn verify_resolves_external_to_edge() {
        let nodes = vec![node("p", Some(3), &[9])];
        let d = classify_and_derive_edges(&nodes).unwrap().unwrap();
        let resolved = HashMap::from([
            (vec![3u8; 32], nodes[0].drv_path.clone()),
            (vec![9u8; 32], "/nix/store/known-child.drv".to_string()),
        ]);
        let ext = verify_resolved(&d, &resolved).unwrap();
        assert_eq!(ext.len(), 1);
        assert_eq!(ext[0].parent_drv_path, nodes[0].drv_path);
        assert_eq!(ext[0].child_drv_path, "/nix/store/known-child.drv");
    }
}
