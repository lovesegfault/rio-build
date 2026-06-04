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
    /// falls through to the byte-bound clauses instead of rejecting:
    /// an unverified value cannot contradict anything — but it can no
    /// longer ride a BARE submission into a match either, round-17
    /// merged_bug_020).
    PreservedClaim,
    /// Both sides byte-anchored to the same declared path, hashes
    /// undecided: the row's persisted rank is byte-anchored AND the
    /// incoming is INLINE-BOUND (non-authoritative inline content —
    /// SubmitBuild ingress text-CA-bound its bytes to the same
    /// declared path, `sched.merge.ingress-inline-drv-binding`). Two
    /// byte anchors to one text-CA path are anchors to one
    /// definition; a differing UNVERIFIED declared hash between them
    /// (masked vs input form, the round-15 divergence) cannot
    /// contradict that. This is the warm inline rejoin that
    /// silence-gating DualAnchor alone would have re-bricked — the
    /// two clauses land atomically (round-17 merged_bug_020).
    StrippedHashMatch,
    /// Row byte-anchor × incoming identity-SILENCE: the row's
    /// persisted evidence rank is byte-anchored
    /// (`path_bound_bytes`/`verified_built` — its recorded identity
    /// was DERIVED from bytes text-CA-bound to the declared path),
    /// and the incoming claims the same path with matching public
    /// attributes while presenting NO identity content of its own —
    /// no declared modular hash, no non-empty expected path. The
    /// silence IS the gate (round-17 merged_bug_020): nothing
    /// submitter-controlled enters the match, so nothing can be
    /// forged through it; a bare submission that ADDS an
    /// uncorroborated hash or path no longer reaches this clause
    /// (pre-fix it did — absence-of-veto over submitter-controlled
    /// fields, the exact shape `sched.evidence.positive-witness`
    /// forbids). Hash-free resubmission of a stripped byte-anchored
    /// row (merged_bug_038's remediation shape) still rejoins HERE.
    DualAnchor,
}

impl SettledMatchBasis {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::PathAgreement => "path_agreement",
            Self::HashMatch => "hash_match",
            Self::PreservedClaim => "preserved_claim",
            Self::StrippedHashMatch => "stripped_hash_match",
            Self::DualAnchor => "dual_anchor",
        }
    }

    /// Bases the pre-M_070 matcher did not admit — the
    /// would-have-bricked population, counted at the Step 0.5 join.
    pub(crate) fn is_stripped_rejoin(self) -> bool {
        matches!(
            self,
            Self::PreservedClaim | Self::StrippedHashMatch | Self::DualAnchor
        )
    }

    // r[impl sched.persist.settled-identity-freeze+4]
    /// The POSITIVE WITNESS each basis grants on — exhaustive, no
    /// wildcard: a future basis does not compile until its witness is
    /// declared here, and the witness-enumeration test
    /// (`every_basis_declares_a_positive_witness`) is this family's
    /// pre-registered F2 trigger definition (an in-governance
    /// recurrence = a basis whose declared witness turns out to admit
    /// submitter-controlled content). "Identity-silence" is a witness
    /// in the strict sense: it proves NO submitter-controlled operand
    /// entered the match.
    pub(crate) fn positive_witness(self) -> &'static str {
        match self {
            Self::PathAgreement => "agreed non-empty expected output path",
            Self::HashMatch => "byte-equal live ca_modular_hash",
            Self::PreservedClaim => "byte-equal preserved stripped claim (M_070)",
            Self::StrippedHashMatch => {
                "ingress text-CA byte binding on both sides                  (byte-anchored row rank x inline-bound incoming)"
            }
            Self::DualAnchor => {
                "byte-anchored row rank x incoming identity-silence                  (no submitter-controlled identity content enters)"
            }
        }
    }
}

/// Row-level twin of [`crate::dag::verifiable_identity_matches`]: does an
/// incoming submission node prove the same identity as a SETTLED
/// (completed/skipped) persisted derivation row?
///
/// r[impl sched.persist.settled-identity-freeze+4]
/// Public attributes must match (system, sorted output names, the
/// fixed-output flag, the content-addressed flag, and expected output
/// paths for names where BOTH sides declare one), and every basis
/// grants on a declared POSITIVE WITNESS
/// ([`SettledMatchBasis::positive_witness`]): agreement on a non-empty
/// expected output path, a byte-equal LIVE CA modular hash, a byte-equal
/// PRESERVED stripped claim (M_070), the double byte anchor of an
/// inline-bound incoming against a byte-anchored row
/// ([`SettledMatchBasis::StrippedHashMatch`]), or — for byte-anchored
/// rows — incoming identity-SILENCE
/// ([`SettledMatchBasis::DualAnchor`], round-17 merged_bug_020: a bare
/// incoming that ADDS an uncorroborated hash or path matches nothing).
/// The live-hash clause is shared
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
    // AUTHORITATIVE incomings stop HERE (round-17 merged_bug_020,
    // hoisted above the preserved clause): authoritative bytes are
    // bound to themselves, not to the declared path, and a settled
    // join refreshes the row's creation snapshot — so an authoritative
    // re-presentation of the row's own stripped claim was a
    // SELF-DISPLACEMENT channel (strip your claim, re-present it
    // authoritatively, slip your bytes into the settled record).
    // Classical evidence (path agreement / live-hash match, both
    // checked above) remains the only authoritative match surface.
    if node.drv_content_authoritative {
        return None;
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
    // Both byte-bound clauses require the row anchor; the decode is
    // STRICT (round-16 bug_073): an undecodable persisted rank yields
    // no anchor BY TYPE.
    let row_byte_anchored = matches!(
        row.evidence_rank.parse::<crate::state::DefinitionEvidence>(),
        Ok(rank) if rank >= crate::state::DefinitionEvidence::PathBoundBytes
    );
    if !row_byte_anchored {
        return None;
    }
    // r[impl sched.persist.settled-identity-freeze+4]
    // StrippedHashMatch: the incoming carries inline NON-authoritative
    // content — ingress text-CA-bound those bytes to this same
    // declared path (sched.merge.ingress-inline-drv-binding), so both
    // sides hold a byte anchor to one definition. A differing
    // unverified declared hash between them (masked vs input form)
    // cannot contradict that; this is the warm inline rejoin.
    if !node.drv_content.is_empty() {
        return Some(SettledMatchBasis::StrippedHashMatch);
    }
    // r[impl sched.persist.settled-identity-freeze+4]
    // DualAnchor, SILENCE-GATED (round-17 merged_bug_020): a bare
    // incoming rejoins a byte-anchored row only when it presents NO
    // identity content — no declared modular hash, no non-empty
    // expected path. A bare submission ADDING an uncorroborated hash
    // or path is not silent and matches nothing here: pre-fix it rode
    // this clause (absence-of-veto over submitter-controlled fields)
    // into the settled row's creation-snapshot refresh — the forged
    // "created" channel sched.evidence.positive-witness exists to
    // forbid.
    if node.ca_modular_hash.is_none() && node.expected_output_paths.iter().all(|p| p.is_empty()) {
        return Some(SettledMatchBasis::DualAnchor);
    }
    None
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

    // r[verify sched.persist.settled-identity-freeze+4]
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

        // INVERTED RESIDUAL PIN (round-17 merged_bug_020): a BARE
        // incoming carrying a hash that matches nothing recorded is
        // the forgery shape — pre-fix it fell through to DualAnchor
        // (this very test pinned that as "not a veto"); now it is
        // refused. The differing value still does not VETO (the
        // preserved clause has no veto direction): an INLINE-BOUND
        // incoming with the same differing hash rejoins via the
        // double byte anchor below.
        assert!(
            settled_row_identity_matches(
                &stripped_floating_row(Some(claim)),
                &floating_incoming(Some([0xDD; 32]))
            )
            .is_none(),
            "bare + uncorroborated differing hash must not match (forgery shape)"
        );

        // INVERTED RESIDUAL PIN (round-17 merged_bug_020): bare
        // incoming ADDING a hash to a row with no preserved value —
        // also the forgery shape, also refused now.
        assert!(
            settled_row_identity_matches(
                &stripped_floating_row(None),
                &floating_incoming(Some(claim))
            )
            .is_none(),
            "bare + added hash against a hashless row must not match"
        );

        // The warm INLINE rejoin those two shapes used to launder
        // through: an inline-bound incoming (ingress text-CA-bound its
        // bytes to the declared path) with a DIFFERING unverified hash
        // — both sides byte-anchored, the hashes cannot contradict.
        let mut inline_differing = floating_incoming(Some([0xDD; 32]));
        inline_differing.drv_content = b"Derive(...)".to_vec();
        assert_eq!(
            settled_row_identity_matches(&stripped_floating_row(Some(claim)), &inline_differing),
            Some(SettledMatchBasis::StrippedHashMatch),
            "inline-bound incoming rejoins on the double byte anchor"
        );
        let mut inline_no_hash = floating_incoming(None);
        inline_no_hash.drv_content = b"Derive(...)".to_vec();
        assert_eq!(
            settled_row_identity_matches(&stripped_floating_row(None), &inline_no_hash),
            Some(SettledMatchBasis::StrippedHashMatch),
            "inline-bound incoming rejoins a hashless byte-anchored row"
        );
    }

    // r[verify sched.persist.settled-identity-freeze+4]
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

        // Undecodable rank: dual-anchor fails CLOSED via the
        // module-owned STRICT decode (Err -> no basis, round-16
        // bug_073) — NOT via parse_lossy, which never reaches a
        // victim (round-17 merged_bug_090 site 3 re-trued this
        // attribution; see the module header and
        // arbitrate_settled_row's doc for the ownership argument).
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

#[cfg(test)]
mod witness_tests {
    use super::*;

    // r[verify sched.persist.settled-identity-freeze+4]
    /// THE F2 TRIGGER DEFINITION for the settled-identity family
    /// (round-17 bet consequence #2): every match basis declares a
    /// positive witness, exhaustively — `positive_witness()` is a
    /// no-wildcard match, so a new basis fails to compile until its
    /// witness is declared, and this test asserts no declared witness
    /// is empty or describes submitter-controlled content as its sole
    /// operand. If a future round finds a basis whose declared witness
    /// admitted submitter-controlled operands IN GOVERNANCE, the
    /// pre-registered F2 escalation (typed positive-witness lattice)
    /// activates for this family.
    #[test]
    fn every_basis_declares_a_positive_witness() {
        let all = [
            SettledMatchBasis::PathAgreement,
            SettledMatchBasis::HashMatch,
            SettledMatchBasis::PreservedClaim,
            SettledMatchBasis::StrippedHashMatch,
            SettledMatchBasis::DualAnchor,
        ];
        for basis in all {
            // Exhaustiveness: a new variant must join `all` (the
            // match inside positive_witness already compile-forces
            // the declaration).
            match basis {
                SettledMatchBasis::PathAgreement
                | SettledMatchBasis::HashMatch
                | SettledMatchBasis::PreservedClaim
                | SettledMatchBasis::StrippedHashMatch
                | SettledMatchBasis::DualAnchor => {}
            }
            let witness = basis.positive_witness();
            assert!(!witness.is_empty(), "{basis:?} declares no witness");
            // Every witness names a scheduler- or ingress-owned
            // operand: byte anchors, recorded claims, or proven
            // silence. None may be a bare submitter assertion.
            assert!(
                witness.contains("byte")
                    || witness.contains("agreed")
                    || witness.contains("silence"),
                "{basis:?} witness must name an owned operand: {witness}"
            );
            // Label stability: the metric label is the snake_case of
            // the variant — a rename breaks dashboards.
            assert!(!basis.as_str().is_empty());
        }
    }

    // r[verify sched.persist.settled-identity-freeze+4]
    // r[verify sched.evidence.positive-witness]
    /// SOUNDNESS CELLS (amended R2: parity proves agreement, these
    /// prove REFUSAL): hostile incomings staged against a row in the
    /// production strip shape — every cell asserts the absolute
    /// verdict, not cross-implementation agreement. The Rust matcher
    /// half; the SQL twin's mirrored cells live in db/tests/batch.rs.
    #[test]
    fn hostile_incomings_refused_at_every_silence_breach() {
        let row = crate::db::SettledIdentityRow {
            drv_hash: "h".into(),
            drv_path: "/nix/store/h.drv".into(),
            system: "x86_64-linux".into(),
            output_names: vec!["out".into()],
            expected_output_paths: vec![String::new()],
            is_fixed_output: false,
            is_ca: true,
            ca_modular_hash: None,
            ca_modular_hash_stripped: Some(vec![0xCC; 32]),
            evidence_rank: "path_bound_bytes".into(),
        };
        let bare = |hash: Option<[u8; 32]>, path: &str| crate::domain::DerivationNode {
            drv_hash: "h".into(),
            drv_path: "/nix/store/h.drv".into(),
            pname: String::new(),
            system: "x86_64-linux".into(),
            output_names: vec!["out".into()],
            expected_output_paths: vec![path.to_string()],
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
        };

        // Cell 1: bare + forged hash (differs from preserved) → refuse.
        assert!(settled_row_identity_matches(&row, &bare(Some([0xDD; 32]), "")).is_none());
        // Cell 2: bare + added expected path (row has none) → refuse:
        // a path is identity content; presenting one breaches silence.
        assert!(
            settled_row_identity_matches(&row, &bare(None, "/nix/store/attacker-out")).is_none()
        );
        // Cell 3: authoritative content + matching preserved hash →
        // refuse (authoritative bytes bind to themselves, never to the
        // declared path; rejoin would adopt them into the snapshot).
        let mut auth = bare(Some([0xCC; 32]), "");
        auth.drv_content = b"Derive(...)".to_vec();
        auth.drv_content_authoritative = true;
        assert!(
            settled_row_identity_matches(&row, &auth).is_none(),
            "authoritative-vs-stripped self-displacement refused"
        );
        // Cell 4 (the surviving honest shapes): silence rejoins;
        // byte-equal preserved rejoins.
        assert_eq!(
            settled_row_identity_matches(&row, &bare(None, "")),
            Some(SettledMatchBasis::DualAnchor)
        );
        assert_eq!(
            settled_row_identity_matches(&row, &bare(Some([0xCC; 32]), "")),
            Some(SettledMatchBasis::PreservedClaim)
        );
    }
}
