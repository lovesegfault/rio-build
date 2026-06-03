//! Settled-row identity and displacement arbitration — the ONE owner
//! of every decision about a SETTLED (`completed`/`skipped`) persisted
//! derivation row meeting an incoming re-creation.
//!
//! Round-16 structural consolidation (bug_073 / MP2 per-consumer
//! divergence): the identity matcher, the displacement arbitration,
//! and the victim-side evidence-rank decode live in one module so no
//! consumer can re-derive any of them with different semantics. The
//! two prior divergence channels are closed by construction:
//!
//! - the victim-side rank decode is INTERNAL and fail-closed
//!   ([`arbitrate_settled_row`] takes the persisted string;
//!   `parse_lossy`'s displacer-conservative floor cannot reach a
//!   victim through any call site);
//! - the identity axes are enumerated once in
//!   [`settled_row_identity_matches`]; the SQL freeze guard in
//!   `db/batch.rs` mirrors them and the axis-isolated conformance
//!   test in `db/tests/batch.rs` drives BOTH implementations over the
//!   same single-axis mutations (`sched.persist.settled-identity-freeze`).

use std::collections::HashMap;

/// The content-bound basis on which a settled row matched an incoming
/// re-creation — returned by [`settled_row_identity_matches`] so the
/// caller can count rejoins that the pre-M_070 matcher would have
/// REFUSED (`rio_scheduler_merge_stripped_rejoin_total`, the
/// would-have-bricked success signal). The basis NEVER feeds
/// arbitration or ranking: a match is a match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettledMatchBasis {
    /// Agreement on at least one non-empty expected output path.
    PathAgreement,
    /// Byte-equal LIVE CA modular hash.
    HashMatch,
    /// Byte-equal PRESERVED stripped claim (M_070): the incoming
    /// re-presents exactly the claim the strip removed. Match basis
    /// only — never ranked, never vetoing (a differing preserved value
    /// falls through to the dual-anchor clause instead of rejecting:
    /// an unverified value cannot contradict anything).
    PreservedClaim,
    /// Dual byte-anchor: the row's persisted evidence rank is
    /// byte-anchored (`path_bound_bytes`/`verified_built` — its
    /// recorded identity was DERIVED from bytes text-CA-bound to the
    /// declared path), and the incoming claims the same path with
    /// matching public attributes and no contradicting evidence. The
    /// declared `drv_path` is itself a text content-address of the
    /// definition, so both sides anchor to the same bytes; demanding
    /// extra positive evidence here is what bricked stripped
    /// floating-CA rebuilds (merged_bug_038: every expected path
    /// empty + live hash NULL after the strip = nothing left to
    /// agree on).
    DualAnchor,
}

impl SettledMatchBasis {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::PathAgreement => "path_agreement",
            Self::HashMatch => "hash_match",
            Self::PreservedClaim => "preserved_claim",
            Self::DualAnchor => "dual_anchor",
        }
    }

    /// Bases the pre-M_070 matcher did not admit — the
    /// would-have-bricked population, counted at the Step 0.5 join.
    pub(crate) fn is_stripped_rejoin(self) -> bool {
        matches!(self, Self::PreservedClaim | Self::DualAnchor)
    }
}

/// Row-level twin of [`crate::dag::verifiable_identity_matches`]: does an
/// incoming submission node prove the same identity as a SETTLED
/// (completed/skipped) persisted derivation row?
///
/// r[impl sched.persist.settled-identity-freeze+3]
/// Public attributes must match (system, sorted output names, the
/// fixed-output flag, the content-addressed flag, and expected output
/// paths for names where BOTH sides declare one), and at least one piece
/// of content-bound evidence is required: agreement on a non-empty
/// expected output path, a byte-equal LIVE CA modular hash, a byte-equal
/// PRESERVED stripped claim (M_070), or — for byte-anchored rows — the
/// dual anchor of the declared path itself (see
/// [`SettledMatchBasis::DualAnchor`]). The live-hash clause is shared
/// with the resident matcher through
/// [`crate::dag::modular_hash_evidence`] — present-but-differing LIVE
/// hashes veto the match outright (`sched.merge.identity-hash-veto`); a
/// predicate added to one matcher belongs in the shared helper. The
/// preserved clause deliberately has NO veto direction: it is consulted
/// only after the live clause and only as a positive basis.
pub(crate) fn settled_row_identity_matches(
    row: &crate::db::SettledIdentityRow,
    node: &crate::domain::DerivationNode,
) -> Option<SettledMatchBasis> {
    if row.system != node.system
        || row.is_fixed_output != node.is_fixed_output
        || row.is_ca != node.is_content_addressed
    {
        return None;
    }
    let mut row_names: Vec<&str> = row.output_names.iter().map(String::as_str).collect();
    let mut incoming_names: Vec<&str> = node.output_names.iter().map(String::as_str).collect();
    row_names.sort_unstable();
    incoming_names.sort_unstable();
    if row_names != incoming_names {
        return None;
    }
    // r[impl sched.merge.identity-hash-veto]
    // Shared with the resident matcher: present-but-differing LIVE
    // hashes veto before any path agreement is considered.
    let hash_evidence = crate::dag::modular_hash_evidence(
        row.ca_modular_hash.as_deref(),
        node.ca_modular_hash.as_ref().map(|h| h.as_slice()),
    );
    if hash_evidence == crate::dag::ModularHashEvidence::Differs {
        return None;
    }
    let row_paths: HashMap<&str, &str> = row
        .output_names
        .iter()
        .zip(row.expected_output_paths.iter())
        .map(|(n, p)| (n.as_str(), p.as_str()))
        .collect();
    let mut path_evidence = false;
    for (name, path) in node
        .output_names
        .iter()
        .zip(node.expected_output_paths.iter())
    {
        if path.is_empty() {
            continue;
        }
        if let Some(row_path) = row_paths.get(name.as_str())
            && !row_path.is_empty()
        {
            if *row_path != path.as_str() {
                return None;
            }
            path_evidence = true;
        }
    }
    if path_evidence {
        return Some(SettledMatchBasis::PathAgreement);
    }
    if hash_evidence == crate::dag::ModularHashEvidence::Match {
        return Some(SettledMatchBasis::HashMatch);
    }
    // M_070 preserved-claim basis: only consulted when the LIVE hash
    // produced no decision (row hash NULL post-strip). Positive-only —
    // a differing preserved value falls through (never a veto: the
    // value is unverified and cannot contradict).
    if row.ca_modular_hash.is_none()
        && let (Some(preserved), Some(incoming)) = (
            row.ca_modular_hash_stripped.as_deref(),
            node.ca_modular_hash.as_ref(),
        )
        && preserved == incoming.as_slice()
    {
        return Some(SettledMatchBasis::PreservedClaim);
    }
    // Dual byte-anchor: byte-anchored rows are identity-bound to the
    // declared path's text-CA bytes; the incoming names the same path
    // (rows are keyed by it), public attributes agree, and nothing
    // vetoed above. The decode is STRICT (round-16 bug_073): an
    // undecodable persisted rank yields no dual anchor BY TYPE — the
    // old parse_lossy floor happened to fail this clause closed, but
    // only by accident of the floor's direction; the module-owned
    // strict decode makes the victim axis structurally floor-free.
    // Restricted to NON-authoritative incomings, same as the resident
    // twin: an authoritative claim's bytes are bound to themselves,
    // not to the declared path — no second anchor — and a matching
    // identity admits the re-creation into the settled row's
    // creation-snapshot refresh, which must not be reachable by an
    // evidence-free byte-carrying claim.
    if node.drv_content_authoritative {
        return None;
    }
    match row
        .evidence_rank
        .parse::<crate::state::DefinitionEvidence>()
    {
        Ok(rank) if rank >= crate::state::DefinitionEvidence::PathBoundBytes => {
            Some(SettledMatchBasis::DualAnchor)
        }
        Ok(_) => None,
        Err(_) => None,
    }
}

/// Verdict of [`arbitrate_settled_row`]: what a conflicting
/// re-creation of a settled derivation row may do, decided
/// EXHAUSTIVELY over the row's persisted evidence rank and the
/// incoming submission's shape-derived rank. No `!=` catch-all: every
/// (row, incoming) cell of the 4x4 matrix is an explicit decision
/// (fix-discipline R5), pinned by the matrix test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SettledArbitration {
    /// The incoming rank strictly outranks the row's content binding:
    /// displace with no store fetch.
    DisplaceByRank,
    /// A bare store-backed echo against a displaceable row: the
    /// budgeted store-evidence fetch decides.
    NeedsStoreEvidence,
    /// Refused, with remediation GENERATED from the refusing arm.
    Refuse(String),
}

/// Exhaustive settled-row displacement arbitration
/// (`sched.merge.store-evidence-displacement+3`).
///
/// Soundness arguments per row rank:
/// - `VerifiedBuilt` / `PathBoundBytes`: byte-anchored — the recorded
///   identity was derived from bytes text-CA-bound to the declared
///   path; store bytes cannot contradict it, so nothing displaces it.
/// - `ContentBoundClaim` (settled authoritative record): displaced by
///   strictly higher byte-bound evidence (ingress path-bound bytes),
///   or by a bare echo whose claim the STORE proves; an authoritative
///   claim of equal rank proves nothing about the path and refuses.
/// - `UnverifiedClaim` (settled bare echo — the merged_bug_043 row
///   population): the store outranks every claim ABOUT it, but it is
///   itself the LOWEST evidence class: byte-bound submissions
///   (>= `path_bound_bytes`) displace by rank, and a bare resubmission
///   may prove its claim through store evidence. It is NEVER displaced
///   by `ContentBoundClaim` rank alone (the reverse-squat guard:
///   authoritative bytes are bound to themselves, not to the declared
///   path — rank-alone displacement would let a hook-fallback-shaped
///   forgery erase genuine store-backed history).
///
/// Takes the row's PERSISTED rank string, not a pre-decoded rank: the
/// victim-side decode is owned HERE, fail-CLOSED (round-16 bug_073).
/// `parse_lossy`'s unverified_claim floor is conservative only for a
/// DISPLACER; flooring a VICTIM demotes a byte-anchored settled row to
/// the displaceable arms — exactly the protection the persisted rank
/// exists to provide. An undecodable rank (a future rank-widening
/// migration read by older code; M_067's CHECK keeps it impossible
/// today) refuses with operator remediation: the immutable arm, never
/// the floor. Taking `&str` makes the floored-victim bug unwritable at
/// every call site by construction.
pub(crate) fn arbitrate_settled_row(
    row_evidence_rank: &str,
    incoming_rank: crate::state::DefinitionEvidence,
    incoming_is_bare: bool,
) -> SettledArbitration {
    use crate::state::DefinitionEvidence as E;
    let Ok(row_rank) = row_evidence_rank.parse::<E>() else {
        tracing::warn!(
            value = row_evidence_rank,
            "undecodable persisted evidence_rank on a settled row; refusing arbitration"
        );
        return SettledArbitration::Refuse(
            "the settled record's evidence rank could not be decoded by this \
             scheduler version; nothing may displace it until an operator \
             investigates (a newer rank value read by older code?)"
                .to_string(),
        );
    };
    match (row_rank, incoming_rank) {
        // Byte-anchored rows: immutable to every claim class.
        (E::VerifiedBuilt | E::PathBoundBytes, _) => SettledArbitration::Refuse(
            "the settled record's identity is byte-anchored (derived from \
             bytes content-bound to the declared path) and cannot be \
             displaced by any claim; if you believe it is wrong, ask an \
             operator to investigate"
                .to_string(),
        ),
        // Content-bound rows: strictly higher evidence displaces.
        (E::ContentBoundClaim, E::PathBoundBytes | E::VerifiedBuilt) => {
            SettledArbitration::DisplaceByRank
        }
        (E::ContentBoundClaim, E::UnverifiedClaim) if incoming_is_bare => {
            SettledArbitration::NeedsStoreEvidence
        }
        (E::ContentBoundClaim, E::ContentBoundClaim | E::UnverifiedClaim) => {
            SettledArbitration::Refuse(
                "an inline claim of equal or lower evidence cannot displace \
                 it. If your .drv is uploaded to the store, resubmit \
                 store-backed (the scheduler verifies the store derivation \
                 and displaces a squatting record automatically); or \
                 resubmit with inline non-authoritative bytes (ingress \
                 binds them to the declared path); otherwise ask an \
                 operator to clear the record"
                    .to_string(),
            )
        }
        // Bare-echo rows (merged_bug_043): byte-bound submissions
        // displace by rank...
        (E::UnverifiedClaim, E::PathBoundBytes | E::VerifiedBuilt) => {
            SettledArbitration::DisplaceByRank
        }
        // ...a bare resubmission may prove its claim via the store...
        (E::UnverifiedClaim, E::UnverifiedClaim) if incoming_is_bare => {
            SettledArbitration::NeedsStoreEvidence
        }
        // ...and the reverse-squat guard: never by ContentBoundClaim
        // rank alone.
        (E::UnverifiedClaim, E::ContentBoundClaim | E::UnverifiedClaim) => {
            SettledArbitration::Refuse(
                "an authoritative inline claim cannot displace a settled \
                 store-backed record by rank alone (its bytes are bound to \
                 themselves, not to the declared path). Resubmit with \
                 inline non-authoritative bytes or store-backed so the \
                 claim can be verified against the declared path"
                    .to_string(),
            )
        }
    }
}

#[cfg(test)]
mod matcher_tests {
    use super::*;

    fn settled_row(hash: Option<Vec<u8>>) -> crate::db::SettledIdentityRow {
        crate::db::SettledIdentityRow {
            drv_hash: "h".into(),
            drv_path: "/nix/store/h.drv".into(),
            system: "x86_64-linux".into(),
            output_names: vec!["out".into()],
            expected_output_paths: vec!["/nix/store/agreed-out".into()],
            is_fixed_output: false,
            is_ca: true,
            ca_modular_hash: hash,
            ca_modular_hash_stripped: None,
            evidence_rank: "content_bound_claim".into(),
        }
    }

    fn incoming(hash: Option<[u8; 32]>) -> crate::domain::DerivationNode {
        crate::domain::DerivationNode {
            drv_hash: "h".into(),
            drv_path: "/nix/store/h.drv".into(),
            pname: String::new(),
            system: "x86_64-linux".into(),
            output_names: vec!["out".into()],
            expected_output_paths: vec!["/nix/store/agreed-out".into()],
            is_fixed_output: false,
            is_content_addressed: true,
            ca_modular_hash: hash,
            ca_modular_hash_stripped: None,
            drv_content: Vec::new(),
            drv_content_authoritative: false,
            required_features: Vec::new(),
            wanted_output_names: Vec::new(),
            explicitly_requested: false,
            needs_resolve: false,
            version: None,
            enable_parallel_building: None,
            enable_parallel_checking: None,
            prefer_local_build: None,
        }
    }

    // r[verify sched.merge.identity-hash-veto]
    /// Settled-row matcher twin: a present-but-differing LIVE modular
    /// hash vetoes the match even though the (copyable, public)
    /// expected output paths agree. Pre-fix the differing hash was
    /// folded into "no hash evidence" and path agreement carried the
    /// match.
    #[test]
    fn settled_row_differing_hash_vetoes_despite_path_agreement() {
        let row = settled_row(Some(vec![0xAA; 32]));
        assert!(
            settled_row_identity_matches(&row, &incoming(Some([0xBB; 32]))).is_none(),
            "present-but-differing hashes are a definition conflict"
        );
        assert_eq!(
            settled_row_identity_matches(&row, &incoming(Some([0xAA; 32]))),
            Some(SettledMatchBasis::PathAgreement),
            "byte-equal hashes still match (path agreement ranks first)"
        );
        assert_eq!(
            settled_row_identity_matches(&row, &incoming(None)),
            Some(SettledMatchBasis::PathAgreement),
            "an absent incoming hash falls back to path evidence"
        );
        // A persisted blob of the wrong width is "not recorded", never
        // a veto (legacy rows).
        let short = settled_row(Some(vec![0xAA; 16]));
        assert!(settled_row_identity_matches(&short, &incoming(Some([0xBB; 32]))).is_some());
    }

    /// A stripped settled row in the merged_bug_038 mainstream shape:
    /// floating-CA, every expected output path EMPTY, live hash NULL
    /// (moved to the preservation column), rank raised to
    /// `path_bound_bytes` by the strip arm.
    fn stripped_floating_row(preserved: Option<[u8; 32]>) -> crate::db::SettledIdentityRow {
        crate::db::SettledIdentityRow {
            expected_output_paths: vec![String::new()],
            ca_modular_hash: None,
            ca_modular_hash_stripped: preserved.map(|h| h.to_vec()),
            evidence_rank: "path_bound_bytes".into(),
            ..settled_row(None)
        }
    }

    /// Floating incoming: declares no paths (the gateway's floating
    /// shape) — pre-M_070 such a node could NEVER match a stripped row.
    fn floating_incoming(hash: Option<[u8; 32]>) -> crate::domain::DerivationNode {
        crate::domain::DerivationNode {
            expected_output_paths: vec![String::new()],
            ..incoming(hash)
        }
    }

    // r[verify sched.persist.settled-identity-freeze+3]
    /// THE merged_bug_038 kill, matcher half (deploy blocker; depth-3
    /// fix-child of e47c330a0 x 9d83580f6 <- 1c8cc6877 <- f0a8ffcc9):
    /// a stripped floating-CA settled row has zero classical evidence
    /// (paths all empty, live hash NULL). The M_070 bases must admit
    /// byte-equal re-presentations (PreservedClaim) and hash-free
    /// resubmissions of byte-anchored rows (DualAnchor); everything
    /// the old matcher admitted stays admitted, and the live-hash
    /// veto is untouched.
    #[test]
    fn settled_row_stripped_floating_rejoins_via_m070_bases() {
        let claim = [0xCC; 32];

        // Pre-fix shape: same submission re-presented (gateway stamps
        // the same declared hash) — preserved-claim basis.
        assert_eq!(
            settled_row_identity_matches(
                &stripped_floating_row(Some(claim)),
                &floating_incoming(Some(claim))
            ),
            Some(SettledMatchBasis::PreservedClaim),
            "byte-equal preserved claim rejoins"
        );

        // Resubmission WITHOUT the declared hash (the old remediation
        // text's advice): dual-anchor basis on the byte-anchored rank.
        assert_eq!(
            settled_row_identity_matches(
                &stripped_floating_row(Some(claim)),
                &floating_incoming(None)
            ),
            Some(SettledMatchBasis::DualAnchor),
            "hash-free resubmission rejoins a byte-anchored row"
        );

        // DIFFERING preserved value: never a veto — falls through to
        // dual-anchor (an unverified value cannot contradict).
        assert_eq!(
            settled_row_identity_matches(
                &stripped_floating_row(Some(claim)),
                &floating_incoming(Some([0xDD; 32]))
            ),
            Some(SettledMatchBasis::DualAnchor),
            "differing preserved value falls through, not a veto"
        );

        // Row WITHOUT a preserved value (pre-M_070 history): the
        // dual anchor still rejoins — the basis is the rank, not the
        // preserved bytes.
        assert_eq!(
            settled_row_identity_matches(
                &stripped_floating_row(None),
                &floating_incoming(Some(claim))
            ),
            Some(SettledMatchBasis::DualAnchor),
            "byte-anchored row without preserved bytes still rejoins"
        );
    }

    // r[verify sched.persist.settled-identity-freeze+3]
    /// The new bases must NOT widen matching for non-anchored rows:
    /// a bare-claim row (unverified_claim) with no evidence stays
    /// unmatched without classical agreement, an undecodable rank
    /// fails CLOSED, public-attribute conflicts still refuse on every
    /// basis, and the live-hash veto precedes both new bases.
    #[test]
    fn settled_row_m070_bases_stay_rank_gated_and_vetoed() {
        // Non-anchored row, no preserved value, no paths: NO basis.
        let bare = crate::db::SettledIdentityRow {
            evidence_rank: "unverified_claim".into(),
            ..stripped_floating_row(None)
        };
        assert!(
            settled_row_identity_matches(&bare, &floating_incoming(Some([0xCC; 32]))).is_none(),
            "an unanchored bare row gains nothing from M_070"
        );

        // Same row WITH a byte-equal preserved claim: PreservedClaim
        // applies at ANY rank (the claim equality is the evidence).
        let bare_preserved = crate::db::SettledIdentityRow {
            evidence_rank: "unverified_claim".into(),
            ..stripped_floating_row(Some([0xCC; 32]))
        };
        assert_eq!(
            settled_row_identity_matches(&bare_preserved, &floating_incoming(Some([0xCC; 32]))),
            Some(SettledMatchBasis::PreservedClaim),
        );

        // Undecodable rank: dual-anchor fails CLOSED (parse_lossy
        // floors to unverified_claim).
        let garbled = crate::db::SettledIdentityRow {
            evidence_rank: "garbled-rank".into(),
            ..stripped_floating_row(None)
        };
        assert!(
            settled_row_identity_matches(&garbled, &floating_incoming(None)).is_none(),
            "undecodable rank must not grant the dual anchor"
        );

        // Public-attribute conflict refuses regardless of bases.
        let mut sys_conflict = floating_incoming(Some([0xCC; 32]));
        sys_conflict.system = "aarch64-linux".into();
        assert!(
            settled_row_identity_matches(&stripped_floating_row(Some([0xCC; 32])), &sys_conflict)
                .is_none(),
            "public attributes still gate every basis"
        );

        // LIVE hash veto precedes the new bases: a row with a LIVE
        // differing hash refuses even if the preserved value matches.
        let live_differs = crate::db::SettledIdentityRow {
            ca_modular_hash: Some(vec![0xEE; 32]),
            ..stripped_floating_row(Some([0xCC; 32]))
        };
        assert!(
            settled_row_identity_matches(&live_differs, &floating_incoming(Some([0xCC; 32])))
                .is_none(),
            "live-hash veto precedes preserved-claim"
        );
    }
}
