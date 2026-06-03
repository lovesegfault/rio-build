//! In-memory derivation DAG.
//!
//! The DAG tracks all derivation nodes and their dependency edges across all
//! concurrent builds. Nodes are deduplicated by `drv_hash`, which SubmitBuild
//! ingress binds to the declared `.drv` store path (`drv_path`); CA content
//! identity is tracked separately via the 32-byte modular hash
//! (`ca_modular_hash`), which keys realisations, not the DAG. Each node
//! tracks which builds are interested in it.

use std::collections::{BTreeSet, HashMap, HashSet};

use uuid::Uuid;

use crate::state::{
    DefinitionEvidence, DerivationState, DerivationStatus, DrvHash, union_wanted_saturating,
};

/// CA-cutoff cascade result-size cap. Bounds the number of nodes
/// transitioned `Queued`→`Skipped` per cascade so an adversarial DAG
/// (e.g., a linear chain of 100k Queued nodes, or a single node with
/// 100k Queued parents) cannot stall the single-threaded completion
/// handler. The BFS in [`DerivationDag::speculative_cascade_reachable`]
/// stops pushing once `reachable.len() == MAX_CASCADE_NODES` — a hard
/// bound on the result, not on the number of `expand()` calls. Ample
/// for real-world DAGs, bounded for adversarial ones.
pub const MAX_CASCADE_NODES: usize = 1000;

/// Errors from DAG operations.
#[derive(Debug, thiserror::Error)]
pub enum DagError {
    #[error("dependency cycle detected")]
    CycleDetected,
    #[error("invalid drv_path {path:?}: {source}")]
    InvalidDrvPath {
        path: String,
        source: rio_nix::store_path::StorePathError,
    },
    /// A submission claimed authoritative inline derivation content for a
    /// node that already holds DIFFERENT authoritative content. Those
    /// bytes are the only copy of their derivation anywhere, so silently
    /// joining would let the second submitter redefine what the first
    /// one's build runs and what recovery rebuilds. Returned while the
    /// existing claim is live or `Failed` (still owned by the retry
    /// machinery — `RefusedInFlight`), or settled with a definition-
    /// evidence rank ≥ the displacer's (`RefusedSettledOutranked`;
    /// authoritative displacers carry `content_bound_claim`, so a
    /// settled authoritative claim is never redefined by another
    /// authoritative claim) — once it parks in a terminal failure
    /// state the byte-different submission displaces it instead of
    /// being rejected. Maps to `FAILED_PRECONDITION` (client-actionable
    /// conflict, not an internal invariant breach).
    #[error(
        "conflicting authoritative derivation content for {drv_path}: a node \
         with different inline content already exists"
    )]
    AuthoritativeContentMismatch { drv_path: String },
    /// A store-backed (non-authoritative) submission's verifiable identity
    /// (system, output names, fixed-output flag, content-addressed flag,
    /// declared expected output paths, plus content-bound evidence — a
    /// shared fixed-output path or a matching CA modular hash) conflicts
    /// with a node that carries authoritative inline content and is
    /// either still in flight or settled without being strictly
    /// outranked. Joining would attribute that node's results to a
    /// derivation that either provably differs or cannot be shown to be
    /// the same; erasing a settled (Completed/Skipped) record takes
    /// STRICTLY higher definition evidence than the settled node's rank
    /// (`sched.merge.evidence-ranked-displacement`) — a bare store-backed
    /// echo (`unverified_claim`) never qualifies, while an
    /// ingress-byte-bound submission (`path_bound_bytes`) displaces a
    /// content-bound squat. Maps to `FAILED_PRECONDITION`; once the
    /// conflicting node sits in a terminal FAILURE state the verifiable
    /// definition displaces it regardless of rank. (An identity-MATCHING
    /// store-backed submission never sees this error: it joins the node,
    /// or displaces it without error when the node is poison-locked —
    /// terminal failure, no longer retriable on resubmit.) A legitimate
    /// bare-resubmission claimant hitting this against a settled squat
    /// is self-service: upload the genuine `.drv` to the store and
    /// resubmit — the merge-time store-evidence check
    /// (`sched.merge.store-evidence-displacement+2`) verifies the claim
    /// against the store's text-CA-bound bytes and raises it past the
    /// squat's rank.
    #[error(
        "derivation {drv_path} carries authoritative inline content that \
         conflicts with this submission's declared identity; if the \
         existing record is a squat on your derivation's path, upload \
         your .drv to the store and resubmit store-backed — the \
         scheduler verifies the store derivation and displaces the squat"
    )]
    ConflictingInFlightContent { drv_path: String },
    /// The inverse direction of [`Self::ConflictingInFlightContent`]: a
    /// submission claiming authoritative inline content landed on an
    /// existing STORE-BACKED node that is eligible for the
    /// resubmit-reset, and its verifiable identity does not match the
    /// existing node's. Admitting it would let the claim redefine a
    /// parked (failed / cancelled / poison-reset) store-backed
    /// definition through the reset, carrying the prior builds' interest
    /// onto bytes only the claimant can see. An identity-matching claim
    /// is admitted through the normal resubmit-reset instead, and an
    /// authoritative claim never displaces a store-backed node. Maps to
    /// `FAILED_PRECONDITION` (client-actionable conflict, not an
    /// internal invariant breach).
    #[error(
        "authoritative inline content for {drv_path} conflicts with the \
         existing store-backed derivation definition (no matching \
         content-bound identity evidence); if this is your own \
         derivation, re-upload the .drv and resubmit store-backed, ask \
         an operator to clear the parked definition, or wait for its \
         retention window to expire"
    )]
    AuthoritativeClaimIdentityConflict { drv_path: String },
}

/// Three-valued comparison of two recorded modular hashes.
///
/// Shared by BOTH identity matchers ([`verifiable_identity_matches`]
/// and the settled-row matcher in `actor::merge`) so the hash clause
/// cannot drift between them: a present-but-DIFFERENT pair is a
/// conflict in its own right (`Differs` vetoes the match outright),
/// never silently folded into "no hash evidence". A side that records
/// no usable hash (absent, or a persisted blob that is not 32 bytes)
/// contributes `Absent` — the matchers then fall back to path evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModularHashEvidence {
    /// Both sides record a 32-byte hash and the bytes are equal.
    Match,
    /// Both sides record a 32-byte hash and the bytes DIFFER — the two
    /// definitions are provably not the same derivation; the matchers
    /// MUST veto regardless of path agreement.
    Differs,
    /// At least one side records no usable hash: no hash evidence in
    /// either direction.
    Absent,
}

// r[impl sched.merge.identity-hash-veto]
/// Classify the modular-hash relationship between two identity records.
/// Accepts raw byte slices so both matchers (in-memory `[u8; 32]` and
/// persisted `bytea`) normalize through the same length check.
pub(crate) fn modular_hash_evidence(
    existing: Option<&[u8]>,
    incoming: Option<&[u8]>,
) -> ModularHashEvidence {
    match (existing, incoming) {
        (Some(a), Some(b)) if a.len() == 32 && b.len() == 32 => {
            if a == b {
                ModularHashEvidence::Match
            } else {
                ModularHashEvidence::Differs
            }
        }
        _ => ModularHashEvidence::Absent,
    }
}

/// `sched.merge.authoritative-conflict` cross-check: does the submission's
/// verifiable identity agree with an existing authoritative node?
///
/// Public attributes are compared first: `system`, output names
/// (order-insensitive), the fixed-output flag, the content-addressed flag,
/// and expected output paths for output names where BOTH sides declare one
/// (an empty path carries no information — floating-CA outputs declare
/// `""`). Public attributes alone are never sufficient — they are copyable
/// from the victim's public derivation — so a match additionally requires
/// at least one piece of CONTENT-BOUND evidence:
///
///   - fixed-output: at least one output whose expected path is non-empty
///     on both sides and equal (the path commits to the declared output
///     hash, and ingress binds it to the authoritative bytes), or
///   - floating-CA: a byte-equal `ca_modular_hash` on both sides (forging
///     it requires a SHA-256 preimage). The hook fallback is built from an
///     already-RESOLVED derivation (no `inputDrvs`), so its modular hash
///     equals the full-form hash a store-backed submission of the same
///     derivation computes — identical content still joins.
///
/// Present-but-DIFFERING modular hashes veto the match outright
/// (`sched.merge.identity-hash-veto` via [`modular_hash_evidence`]) —
/// path agreement cannot override a provable definition difference. No
/// evidence at all (the degenerate floating-CA case where a side
/// carries no hash and no path agreement exists) is likewise a conflict
/// and falls through to the reject-in-flight / displace-when-terminal
/// arms.
///
/// Also used in the INVERSE direction
/// (`sched.merge.authoritative-claim-no-redefine`): an incoming
/// authoritative claim landing on a store-backed node that is eligible
/// for the resubmit-reset must prove the same verifiable identity, with
/// the same evidence rules and the same no-evidence-is-conflict stance —
/// `existing` is then the store-backed node and `node` the claimant.
pub(crate) fn verifiable_identity_matches(
    existing: &DerivationState,
    node: &crate::domain::DerivationNode,
) -> bool {
    if existing.system != node.system
        || existing.is_fixed_output != node.is_fixed_output
        || existing.ca.is_ca != node.is_content_addressed
    {
        return false;
    }
    let mut existing_names: Vec<&str> = existing.output_names.iter().map(String::as_str).collect();
    let mut incoming_names: Vec<&str> = node.output_names.iter().map(String::as_str).collect();
    existing_names.sort_unstable();
    incoming_names.sort_unstable();
    if existing_names != incoming_names {
        return false;
    }
    // r[impl sched.merge.identity-hash-veto]
    // Present-but-differing modular hashes are a conflict in their own
    // right — two definitions whose hashes both exist and differ are
    // provably different derivations, and path agreement (copyable
    // public data) must not override that. Veto BEFORE the path fold.
    let hash_evidence = modular_hash_evidence(
        existing.ca.modular_hash.as_ref().map(|h| h.as_slice()),
        node.ca_modular_hash.as_ref().map(|h| h.as_slice()),
    );
    if hash_evidence == ModularHashEvidence::Differs {
        return false;
    }
    let existing_paths: HashMap<&str, &str> = existing
        .output_names
        .iter()
        .zip(existing.expected_output_paths.iter())
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
        if let Some(existing_path) = existing_paths.get(name.as_str())
            && !existing_path.is_empty()
        {
            if *existing_path != path.as_str() {
                return false;
            }
            // Both sides declare this output's path and they agree.
            path_evidence = true;
        }
    }
    if path_evidence || hash_evidence == ModularHashEvidence::Match {
        return true;
    }
    // CLAIMS-FREE incoming against a STORE-ANCHORED resident: beyond
    // the public attributes (already equal above), the submission
    // declares NOTHING — every expected path empty, no modular hash.
    // Nothing it asserts can contradict the resident definition, a
    // resident join is interest-only (no row rewrite, no
    // displacement), and a bare resident's claims are re-verified
    // against the store at dispatch before anything is signed —
    // refusing here would demand evidence from a submission that
    // makes no claims, which is the shape every hash-less
    // re-reference of a resident floating node takes. Restricted to
    // NON-authoritative residents: an authoritative claim's identity
    // is unverifiable until byte-anchored, and a claims-free join
    // would let such a squat CAPTURE bare submitters (the
    // squat-capture defense pinned by
    // floating_ca_squat_without_evidence_conflicts_in_flight). The
    // positive-evidence bar also stays where re-CREATION happens —
    // the settled-ROW matcher (its joins refresh the row's creation
    // snapshot) and the displacement arbitration.
    if !existing.drv_content_authoritative
        && node.ca_modular_hash.is_none()
        && node.expected_output_paths.iter().all(|p| p.is_empty())
    {
        return true;
    }
    // r[impl sched.persist.settled-identity-freeze+2]
    // M_070 bases, resident form — the row matcher's twin clauses
    // (settled_row_identity_matches): a STRIPPED resident node
    // (floating-CA/deferred-IA: every expected path empty, live hash
    // None) retains no classical evidence, so without these an honest
    // resident-window rebuild reads as a conflict and the settled
    // bare x bare gate arm would refuse it — the resident-window form
    // of the merged_bug_038 brick. Preserved-claim: byte-equal
    // re-presentation of the stripped declaration (positive basis
    // only; differing preserved values fall through, never veto).
    // Dual-anchor: a byte-anchored node's identity was derived from
    // the text-CA bytes its declared path names; an incoming claim of
    // the same path with agreeing public attributes and no
    // contradicting evidence anchors to the same definition.
    if existing.ca.modular_hash.is_none()
        && let (Some(preserved), Some(incoming)) = (
            existing.ca.modular_hash_stripped.as_ref(),
            node.ca_modular_hash.as_ref(),
        )
        && preserved == incoming
    {
        return true;
    }
    // Dual-anchor is restricted to NON-authoritative incomings: an
    // authoritative claim's bytes are bound to themselves, not to the
    // declared path (the reverse-squat axiom), so there is no second
    // anchor — and granting it identity-by-rank would let a
    // hook-fallback-shaped claim with deliberately evidence-free
    // claims ride the resubmit-reset into adopting its bytes over a
    // byte-anchored definition (the claim-no-redefine guard's
    // population). Bare and inline-bound incomings are the production
    // rebuild shapes and carry no adoptable bytes.
    !node.drv_content_authoritative && existing.evidence >= DefinitionEvidence::PathBoundBytes
}

/// Per-hash grant minted by the actor's merge-time store-evidence
/// verification (Steps 0.5/0.6) and consumed by
/// [`DerivationDag::merge_with_evidence`] at node creation. A grant
/// means the store's text-CA-bound bytes verified the submission's
/// claimed identity (possibly MODULO an unverifiable declared modular
/// hash — then `stripped_declared_hash` carries the strip): the
/// created node displaces with `path_bound_bytes` standing, records
/// the BYTE-DERIVED resolve flag (never the submitter's echo,
/// `sched.dispatch.claims-derived+3`), and applies the strip with
/// M_070 preservation when present
/// (`sched.merge.store-evidence-displacement+2` — one verdict, one
/// consequence, identical at the merge and dispatch consumers).
#[derive(Debug, Clone, Copy)]
pub struct StoreEvidenceGrant {
    /// Dispatch-resolve requirement derived from the verified bytes.
    pub needs_resolve: bool,
    /// `Some(declared)` iff the verdict was
    /// `VerifiedExceptDeclaredHash`: the created node's live hash is
    /// cleared and the declared value preserved out-of-band (M_070).
    pub stripped_declared_hash: Option<[u8; 32]>,
}

/// Result of a successful `merge()` operation. Surfaces all the rollback
/// state that `merge()` already tracks internally, so callers can invoke
/// `rollback_merge()` if their own post-merge persistence fails.
#[derive(Debug)]
pub struct MergeResult {
    /// Hashes of nodes newly inserted by this merge (not pre-existing).
    pub newly_inserted: HashSet<DrvHash>,
    /// Edges newly added by this merge as (parent_hash, child_hash) pairs.
    pub new_edges: Vec<(DrvHash, DrvHash)>,
    /// Hashes of pre-existing nodes that gained build_id interest.
    /// Rollback removes interest only from these (not from nodes where
    /// build_id was already present from a prior merge).
    pub interest_added: Vec<DrvHash>,
    /// Subset of `newly_inserted` that were pre-existing retriable nodes
    /// (`Cancelled`/`Failed`/`DependencyFailed`/`Poisoned`-under-limit)
    /// removed and reinserted fresh by the resubmit-retry path. Surfaced
    /// so the actor can `db.clear_poison` them — `batch_upsert_derivations`'
    /// ON CONFLICT does not touch `poisoned_at`/`failed_builders` for
    /// same-definition re-creations.
    pub reset_on_resubmit: Vec<DrvHash>,
    /// Subset of `reset_on_resubmit` where the removed retriable node
    /// carried authoritative inline content and the re-creating
    /// submission is store-backed (an authority takeover through the
    /// resubmit-reset, e.g. an identity-matching store-backed
    /// resubmission of a parked authoritative claim). Disjoint from
    /// `displaced`. These rows get the full definition-change
    /// accumulator reset inside the recreate-refresh upsert
    /// (`sched.merge.displaced-failure-reset`), and their in-memory
    /// state starts with a fresh poison-resubmit budget — so the actor
    /// must NOT also run `clear_poison_batch` on them (it would
    /// re-increment `resubmit_cycles` in PG and diverge from memory).
    /// Their children edges are scrubbed exactly like displacement
    /// (recorded in `displaced_scrubbed_edges`), and the actor chains
    /// these hashes into the durable parent-side `derivation_edges`
    /// delete (`sched.merge.displaced-edge-scrub`).
    pub authority_takeovers: Vec<DrvHash>,
    /// Subset of `newly_inserted` that DISPLACED a terminal authoritative
    /// node (`sched.merge.authoritative-conflict`): a store-backed
    /// submission whose verifiable identity conflicted with the terminal
    /// node, an identity-MATCHING store-backed submission landing on a
    /// poison-locked node no longer retriable on resubmit, or an
    /// authoritative submission with byte-different content landing on a
    /// node parked in a terminal failure state. Mutually exclusive with
    /// `reset_on_resubmit`: a hash appears in exactly one of the two.
    /// Unlike the resubmit-reset path, displacement carries NO prior
    /// interest and starts `resubmit_cycles` at 0 — the fresh node is
    /// the replacing definition taking over the hash, not a retry of
    /// the squat. The actor uses this to reset the displaced row's
    /// poison/failure accounting (including its resource floors), to
    /// scrub its persisted dependency edges, and to prune the displaced
    /// hash from prior interested builds' completion accounting.
    pub displaced: Vec<DrvHash>,
    /// Full prior state of every pre-existing node this merge
    /// destructively removed — resubmit-reset of retriable nodes and
    /// displacement of terminal authoritative-content nodes
    /// (`sched.merge.authoritative-conflict`). `rollback_merge`
    /// re-inserts these AFTER scrubbing `newly_inserted` so a failed
    /// merge restores the exact pre-merge DAG (status,
    /// `interested_builds`, `retry.count`). Without this, a
    /// `{retriable-X, cycle}` submission would wipe X and reset its
    /// `POISON_RESUBMIT_RETRY_LIMIT` accumulator (I-169).
    pub removed_retriable: Vec<(DrvHash, DerivationState)>,
    /// Dependency (children) edges scrubbed from displaced or
    /// taken-over nodes (`sched.merge.displaced-edge-scrub`), keyed by
    /// the removed hash. `rollback_merge` re-attaches them when
    /// restoring the node, and `persist_merge_to_db` mirrors the scrub
    /// durably by deleting those rows' parent-side `derivation_edges`
    /// in the same transaction.
    pub displaced_scrubbed_edges: Vec<(DrvHash, HashSet<DrvHash>)>,
    /// Submitted edges skipped by the creation-scoped edge gate
    /// (`sched.merge.edge-creation-scoped`) whose parent does NOT carry
    /// the `closure_hole` breadcrumb: their parent is a resident node
    /// this submission did NOT (re)create and that is not a
    /// topdown-pruned root awaiting its dependency top-up, and the edge
    /// did not already exist. They never enter the in-memory DAG, are
    /// never persisted (Batch 3 sources `derivation_edges` rows from
    /// `new_edges`), and are not part of rollback. Surfaced so the
    /// actor can count them (`rio_scheduler_merge_foreign_edge_skipped_total`)
    /// — this hostile-or-bug shape is disjoint from
    /// `rejoin_parent_edges_skipped`.
    pub foreign_parent_edges_skipped: Vec<(DrvHash, DrvHash)>,
    /// Submitted edges skipped by the creation-scoped edge gate whose
    /// parent DOES carry the `closure_hole` breadcrumb: the signature of
    /// a legitimate full-closure rejoin of a node whose children were
    /// reaped out from under it (the submitter re-declares the full
    /// inputDrvs set, but the gate cannot attach edges to a resident
    /// node it did not re-create). Surfaced separately
    /// (`rio_scheduler_merge_rejoin_edge_skipped_total`, debug-level)
    /// because this shape is expected traffic, not hostility — and the
    /// vetoed parent deliberately stays OUT of `healed_parents`.
    pub rejoin_parent_edges_skipped: Vec<(DrvHash, DrvHash)>,
    // r[impl sched.merge.heal-accepted-edges+1]
    /// Parents whose ENTIRE declared edge set was accepted by this
    /// merge (every declared edge attached or an exact re-declaration;
    /// any gate-skipped or unresolvable-child edge vetoes). This is the
    /// heal TRIGGER and the `topdown_pruned` clear-candidate seed — but
    /// acceptance alone no longer heals (round-15 merged_bug_073:
    /// "every declared edge accepted" is satisfiable by a SUBSET
    /// re-declaration of the surviving children, which would launder
    /// reap-truncation evidence into Vouched). The heal itself is
    /// `healed_parents`.
    pub accepted_edge_parents: Vec<DrvHash>,
    // r[impl sched.merge.heal-accepted-edges+1]
    /// Parents this merge may HEAL: accepted trigger ∧ positive
    /// re-supply coverage — every recorded missing child
    /// (`ClosureHole::missing`, the 069 witness set) is among the
    /// parent's post-insert children. Un-holed parents are trivially
    /// covered (subset over the empty set), preserving the stale-PG
    /// total-clear semantics: the persisted bit can outlive the
    /// in-memory hole (reap+reinsert, poisoned-stub recovery, lost
    /// best-effort heal write), so an un-holed accepted parent still
    /// clears the column. The paired [`HealWitness`] is constructible
    /// ONLY by the coverage branch in `merge()` — possession proves
    /// the subset check ran (`sched.evidence.positive-witness`).
    pub healed_parents: Vec<(DrvHash, HealWitness)>,
    /// Accepted-trigger parents REFUSED the heal: holed, and the
    /// re-declared set does not cover the witness set (subset
    /// re-declaration, junk top-up, or the recovery LOST_WITNESS
    /// sentinel — uncoverable by construction). Surfaced via
    /// `rio_scheduler_closure_heal_refused_total`; the hole (and its
    /// fail-fast routing) survives.
    pub heal_refused_parents: Vec<DrvHash>,
    /// Hashes of pre-existing nodes whose empty `traceparent` was
    /// upgraded to `submitter_traceparent` by this merge. Rollback
    /// clears it back to `""` so a rejected build's trace ID does not
    /// permanently stick (subsequent submitters would see `is_empty()
    /// == false` and never link THEIR trace).
    pub traceparent_upgraded: Vec<DrvHash>,
    /// Pre-union `wanted_output_names` of pre-existing nodes whose
    /// wanted set was grown by this merge's `union_wanted`. Rollback
    /// restores the prior value — a leaked union is only ever
    /// conservative (the empty "all wanted" sentinel saturates) but
    /// would still let a rejected build's wanted set permanently widen
    /// the cache-hit criterion for everyone else.
    pub wanted_grown: Vec<(DrvHash, Vec<String>)>,
    /// Pre-existing nodes where this merge recorded the submitting
    /// build's `wanted_by_build` contribution, with the prior entry
    /// value (`None` = the build had no entry). Rollback removes the
    /// inserted entry or restores the prior value of one this merge
    /// grew — a leaked contribution would make the rejected build look
    /// live-and-interested to the effective-wanted computation.
    /// Newly-inserted and resubmit-reset nodes don't need this:
    /// rollback removes the former wholesale and restores the latter's
    /// full prior state.
    pub contributions_recorded: Vec<(DrvHash, Option<Vec<String>>)>,
}

/// Verdict of [`DerivationDag::displace`] — the single displacement
/// primitive every merge arm delegates to
/// (`sched.merge.evidence-ranked-displacement`). Non-`Displaced`
/// verdicts leave the victim untouched; the calling arm maps them to
/// its own `DagError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DisplaceVerdict {
    /// The victim was removed with full displacement bookkeeping.
    Displaced,
    /// The victim is live / `Failed` (non-terminal): first-writer-wins
    /// while a claim is in flight or owned by the retry machinery.
    RefusedInFlight,
    /// The victim is store-anchored (no authoritative bytes):
    /// categorical refusal — the store holds at most one text-CA
    /// `.drv` per path, so nothing outranks it about a store-backed
    /// definition.
    RefusedStoreAnchored,
    /// The victim settled (`Completed`/`Skipped`) and its evidence
    /// rank is ≥ the displacer's: a settled record is erased only by
    /// STRICTLY higher definition evidence.
    RefusedSettledOutranked,
}

/// Borrowed displacement bookkeeping threaded into
/// [`DerivationDag::displace`] by the merge loop: the same three
/// accumulators every arm already feeds (`removed_retriable` for
/// rollback, `displaced_scrubbed_edges` for edge restoration and the
/// durable parent-side delete, `displaced` for the link prune /
/// accumulator reset / accounting). A struct instead of three
/// parameters so the primitive's call sites cannot mis-wire one.
pub(crate) struct DisplacementBookkeeping<'a> {
    pub(crate) removed_retriable: &'a mut Vec<(DrvHash, DerivationState)>,
    pub(crate) displaced_scrubbed_edges: &'a mut Vec<(DrvHash, HashSet<DrvHash>)>,
    pub(crate) displaced: &'a mut Vec<DrvHash>,
}

/// Zero-size proof that the positive re-supply coverage check ran for
/// a healed parent: `missing(p) ⊆ post-insert children(p)`. The only
/// constructor is the coverage branch in [`DerivationDag::merge`] —
/// holding one (even by reference) is what authorizes
/// [`crate::state::ClosureHole::clear_for_heal`]
/// (`sched.evidence.positive-witness`); no other code path can mint
/// the token, so an absence-of-objection heal is unwritable.
#[derive(Debug)]
pub struct HealWitness(());

/// Result of [`DerivationDag::remove_build_interest_and_reap`].
#[derive(Debug, Default)]
pub struct ReapOutcome {
    /// `drv_path`s of the reaped nodes, captured before removal (the
    /// caller discards their log buffers; `path_for_hash` would no
    /// longer resolve them afterwards).
    pub reaped_paths: Vec<String>,
    /// Deduped surviving parents of the reaped nodes: nodes still in the
    /// DAG that just lost at least one child to this reap. The caller
    /// re-evaluates these (fail-fast a stranded topdown-pruned root,
    /// promote a now-ready Queued parent) — nothing else would, because
    /// `find_newly_ready` only fires on completions, never on removals.
    pub surviving_parents: Vec<DrvHash>,
    /// The subset of `surviving_parents` that lost at least one
    /// UN-PRODUCED child to this reap — exactly the nodes whose
    /// in-memory `closure_hole` breadcrumb this reap just set. Reported
    /// separately so the leader-gated survivor hook can persist the
    /// breadcrumb (`migrations/064`+`069`, `set_closure_holes`)
    /// without re-deriving "holed by THIS reap" from node state (which
    /// would also re-fire for holes set by earlier reaps and miss
    /// survivors the hook's verdict loop skips as terminal /
    /// zero-interest).
    pub holed_parents: Vec<(DrvHash, Vec<DrvHash>)>,
}

/// Trust classification of a node's current DAG child set as evidence
/// about its dependency closure — the single judgment behind every
/// `topdown_pruned` stamp gate, clear site, and dispatch-time guard.
/// Computed by [`DerivationDag::closure_evidence`].
///
/// `Broken` means "the current child set must NOT vouch for a
/// from-source dispatch": the node is absent, has no children at all
/// (a fired prune dropped its closure from the submission, or every
/// child was reaped), or carries the `closure_hole` breadcrumb — an
/// un-produced child was removed out from under it, by the
/// terminal-build reap (see the breadcrumb loop in
/// [`DerivationDag::remove_build_interest_and_reap`]), by a
/// poison-clear removal (admin ClearPoison or the poison-TTL sweep), or
/// by leader-failover recovery dropping the edge to one (the
/// recovery-time stamp in `load_dag_from_rows`) — so whatever children
/// survive are a truncated view of its input closure.
/// The breadcrumb survives the node's own completion or skip (those
/// transitions do not repair the truncation), is carried across a
/// resubmit-reset of the node (a `rollback_merge` restores the prior
/// state wholesale), and is NOT consumed by the mark-clear / fail-fast
/// handling of the `topdown_pruned` mark it qualifies (the fail-fast is
/// mark-only, so the breadcrumb survives to re-stamp the directed
/// resubmit); it is dropped only by the merge-time heal when a full
/// merge re-declares the node's edges (the post-reconciliation pass in
/// `handle_merge_dag`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClosureEvidence {
    /// At least one child, every child produced (Completed/Skipped),
    /// and no closure hole: the dependency closure is in the store, so
    /// a from-source dispatch is not doomed and a `topdown_pruned`
    /// mark may be cleared.
    Vouched,
    /// At least one child and no closure hole, but not every child is
    /// produced yet: keep any mark (the children can still be reaped
    /// unbuilt later) and re-judge once they are produced or reaped.
    Pending,
    /// Absent, childless, or closure-holed: the child set must not
    /// vouch for a from-source dispatch.
    Broken,
}

/// The global derivation DAG maintained by the actor.
#[derive(Debug, Default)]
pub struct DerivationDag {
    /// All derivation nodes, keyed by drv_hash.
    nodes: HashMap<DrvHash, DerivationState>,
    /// Forward edges: parent drv_hash -> set of child drv_hashes.
    /// A "parent" depends on its "children" (children must complete first).
    children: HashMap<DrvHash, HashSet<DrvHash>>,
    /// Reverse edges: child drv_hash -> set of parent drv_hashes.
    /// Used to find which derivations become ready when a child completes.
    parents: HashMap<DrvHash, HashSet<DrvHash>>,
    /// Reverse index: drv_path -> drv_hash.
    /// Eliminates O(n) scans in completion handling (gRPC layer receives
    /// drv_path from workers but the DAG is keyed by drv_hash).
    path_to_hash: HashMap<String, DrvHash>,
    /// I-204: `requiredSystemFeatures` values stripped from
    /// `DerivationState.required_features` at insertion. nixpkgs treats
    /// `big-parallel` / `benchmark` as capability HINTS (any multi-core
    /// box qualifies), unlike `kvm` / `nixos-test` which are hardware
    /// gates. Stripping here means both spawn-snapshot and dispatch see
    /// the same gate-only set without threading a param through 50
    /// `rejection_reason` call sites. The DB persists the unstripped
    /// original (merge.rs writes `node.required_features`).
    ///
    /// D6: `floor_hint` seeding removed (legacy size-class names);
    /// SLA `solve_intent_for` + `resource_floor` clamp own initial
    /// sizing now. Strip-only.
    soft_features: Vec<String>,
}

impl DerivationDag {
    /// Create an empty DAG.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set soft features. Called once from `DagActor::with_soft_features`
    /// before any merge/recovery; stripping in `insert_recovered_node`
    /// and the merge path read this. No-op if called with an empty vec.
    pub fn set_soft_features(&mut self, soft: Vec<String>) {
        self.soft_features = soft;
    }

    // r[impl sched.dispatch.soft-features]
    /// Strip configured soft features from `state.required_features`.
    /// D6: `floor_hint` seeding removed — SLA sizing owns the initial
    /// shape; reactive `resource_floor` doubling owns the climb.
    ///
    /// §13e + r35: routes through [`DerivationState::set_required_features`]
    /// so `effective_features` re-derives atomically. This is the
    /// FOURTH derivation site (after the three constructors) — a
    /// constructor-only chokepoint would leave `effective_features`
    /// permanently desynced after the soft strip (the 4/4-validator-
    /// converged blocker: `rejection_reason`'s `feature-missing`
    /// clause would still see the soft feature, silently regressing
    /// I-204).
    fn apply_soft_features(&self, state: &mut DerivationState) {
        if self.soft_features.is_empty() {
            return;
        }
        let stripped: Vec<String> = state
            .required_features()
            .iter()
            .filter(|f| !self.soft_features.iter().any(|sf| sf == *f))
            .cloned()
            .collect();
        state.set_required_features(stripped);
    }

    /// Insert a pre-built node (Phase 3b state recovery). No cycle
    /// check — recovered edges come from PG which was validated at
    /// merge time. No interested_builds population — that's done
    /// separately from the build_derivations join.
    ///
    /// If the node already exists (shouldn't — recover_from_pg
    /// clears the DAG first), the existing one is kept (first-wins,
    /// no overwrite). warn! since it indicates a double-insert bug.
    pub fn insert_recovered_node(&mut self, mut state: DerivationState) {
        let hash = state.drv_hash.clone();
        if self.nodes.contains_key(&hash) {
            tracing::warn!(drv_hash = %hash, "duplicate recovered node (skipping)");
            return;
        }
        self.apply_soft_features(&mut state);
        self.path_to_hash
            .insert(state.drv_path().to_string(), hash.clone());
        self.nodes.insert(hash, state);
    }

    /// Insert a recovered edge. No cycle check (PG was validated
    /// at merge time). Idempotent — HashSet::insert is a no-op for
    /// existing entries.
    pub fn insert_recovered_edge(&mut self, parent: DrvHash, child: DrvHash) {
        self.children
            .entry(parent.clone())
            .or_default()
            .insert(child.clone());
        self.parents.entry(child).or_default().insert(parent);
    }

    /// Look up a derivation state by hash.
    pub fn node(&self, drv_hash: &str) -> Option<&DerivationState> {
        self.nodes.get(drv_hash)
    }

    /// Look up a mutable derivation state by hash.
    pub fn node_mut(&mut self, drv_hash: &str) -> Option<&mut DerivationState> {
        self.nodes.get_mut(drv_hash)
    }

    /// Whether a derivation exists in the DAG.
    pub fn contains(&self, drv_hash: &str) -> bool {
        self.nodes.contains_key(drv_hash)
    }

    /// Look up a drv_hash by drv_path (O(1) via reverse index).
    pub fn hash_for_path(&self, drv_path: &str) -> Option<&DrvHash> {
        self.path_to_hash.get(drv_path)
    }

    /// Look up a drv_path by drv_hash (O(1) via the node map).
    pub fn path_for_hash(&self, drv_hash: &str) -> Option<&str> {
        self.nodes.get(drv_hash).map(|s| s.drv_path().as_ref())
    }

    /// Resolve drv_hash → drv_path, falling back to the hash string if
    /// the node isn't in the DAG. Used for `derivation_path` fields in
    /// `BuildEvent`s — better to emit SOMETHING (the hash is still a
    /// useful identifier) than empty.
    pub fn path_or_hash_fallback(&self, drv_hash: &str) -> String {
        self.path_for_hash(drv_hash)
            .map(String::from)
            .unwrap_or_else(|| {
                tracing::warn!(%drv_hash, "no DAG node for hash; using hash as path fallback");
                drv_hash.to_string()
            })
    }

    /// Resolve drv_path → `DerivationState.db_id` via the reverse
    /// index. `None` if the path isn't in the DAG or `db_id` is unset
    /// (PG insert hasn't happened yet).
    pub fn db_id_for_path(&self, drv_path: &str) -> Option<Uuid> {
        self.path_to_hash
            .get(drv_path)
            .and_then(|h| self.nodes.get(h.as_str()))
            .and_then(|s| s.db_id)
    }

    /// Returns a clone of the canonical `DrvHash` stored as a key in `nodes`.
    ///
    /// "Canonical" means: the Arc allocated when the node was FIRST inserted
    /// by `merge()`. All subsequent clones via `path_to_hash`, `children`,
    /// `parents`, `get_parents`/`get_children`, `compute_initial_states`, etc.
    /// share this same Arc (ptr-equal). Cloning the canonical DrvHash is an
    /// atomic refcount bump — no allocation.
    ///
    /// Returns `None` if the hash isn't in the DAG. The `&str` signature
    /// accepts both raw strings and `&DrvHash` (via deref coercion).
    ///
    /// Use this when you have a FRESH `DrvHash` (e.g., constructed from a
    /// proto string) and want to exchange it for the existing canonical one,
    /// so downstream clones don't proliferate distinct Arc allocations.
    pub fn canonical(&self, hash: &str) -> Option<DrvHash> {
        self.nodes.get_key_value(hash).map(|(k, _)| k.clone())
    }

    /// Iterate all (drv_hash, state) pairs.
    pub fn iter_nodes(&self) -> impl Iterator<Item = (&str, &DerivationState)> {
        self.nodes.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Iterate all derivation states (without keys).
    pub fn iter_values(&self) -> impl Iterator<Item = &DerivationState> {
        self.nodes.values()
    }

    // r[impl sched.merge.evidence-ranked-displacement]
    /// THE displacement primitive: every merge arm that removes a
    /// pre-existing node in favor of an incoming definition delegates
    /// the decision AND the bookkeeping here, so the
    /// settled-protection predicate exists exactly once and a future
    /// arm cannot re-introduce a divergent carve-out.
    ///
    /// Verdict contract (in evaluation order):
    /// - victim absent → `debug_assert` (caller bug: arms only call
    ///   this for resident victims) and `RefusedInFlight` as the safe
    ///   release no-op;
    /// - NON-SETTLED store-anchored victim (no authoritative bytes —
    ///   its truth lives in the store, which holds at most one
    ///   text-CA `.drv` per path) → `RefusedStoreAnchored`: while the
    ///   claim is live or parked in the retry machinery, in-flight
    ///   joins are dedup and dispatch re-verifies bare claims against
    ///   the store before signing. SETTLED bare victims are NOT
    ///   exempt (sched.merge.store-evidence-displacement+2,
    ///   rank-uniform; bug_072): a settled bare node's recorded
    ///   outputs are served as cache hits, so its protection is the
    ///   same strict rank rule as every other settled form — a
    ///   cache-hit squat at `unverified_claim` is displaced by
    ///   byte-anchored standing;
    /// - non-terminal victim → `RefusedInFlight` (first-writer-wins
    ///   while the claim is live or owned by the retry machinery);
    /// - settled victim (`Completed`/`Skipped`) with
    ///   `victim.evidence >= displacer` → `RefusedSettledOutranked`
    ///   (strict-inequality rank rule: a settled record is erased
    ///   only by STRICTLY higher definition evidence);
    /// - otherwise → `Displaced` with the shared bookkeeping: the
    ///   prior state rides `removed_retriable` for rollback, the
    ///   children edges are scrubbed
    ///   (`sched.merge.displaced-edge-scrub`), and the hash joins
    ///   `displaced` for the durable link prune / accumulator reset /
    ///   accounting. Terminal-FAILURE victims (Poisoned / Cancelled /
    ///   DependencyFailed) displace REGARDLESS of rank — the
    ///   anti-squat arms' existing semantics: a parked failure must
    ///   not lock later legitimate submissions out of the hash.
    fn displace(
        &mut self,
        drv_hash: &DrvHash,
        displacer: DefinitionEvidence,
        bk: &mut DisplacementBookkeeping<'_>,
    ) -> DisplaceVerdict {
        let Some(victim) = self.nodes.get(drv_hash) else {
            debug_assert!(false, "displace() called for an absent victim");
            return DisplaceVerdict::RefusedInFlight;
        };
        let status = victim.status();
        let settled = matches!(
            status,
            DerivationStatus::Completed | DerivationStatus::Skipped
        );
        // r[impl sched.merge.store-evidence-displacement+2]
        // Store-anchored exemption is scoped to NON-settled victims;
        // settled bare nodes are rank-arbitrated below (rank-uniform
        // with the row arbitration — bug_072: the categorical
        // exemption is what let a cache-hit squat sit undisplaceable
        // while serving forged outputs).
        if !victim.drv_content_authoritative && !settled {
            return DisplaceVerdict::RefusedStoreAnchored;
        }
        if !status.is_terminal() {
            return DisplaceVerdict::RefusedInFlight;
        }
        if settled && victim.evidence >= displacer {
            return DisplaceVerdict::RefusedSettledOutranked;
        }
        let old = self
            .nodes
            .remove(drv_hash)
            .expect("resident victim checked above");
        bk.removed_retriable.push((drv_hash.clone(), old));
        // r[impl sched.merge.displaced-edge-scrub+2]
        // The displacing submission is a different definition: drop
        // the dependency edges earlier submissions attached to this
        // hash so the fresh node's dep set is exactly this
        // submission's edge list (recorded for rollback restoration).
        let scrubbed = self.scrub_dependency_edges(drv_hash);
        if !scrubbed.is_empty() {
            bk.displaced_scrubbed_edges
                .push((drv_hash.clone(), scrubbed));
        }
        bk.displaced.push(drv_hash.clone());
        DisplaceVerdict::Displaced
    }

    /// Merge a set of nodes and edges from a new build into the global DAG.
    ///
    /// Returns a `MergeResult` with the hashes of newly-inserted nodes,
    /// newly-added edges, and pre-existing nodes that gained build interest.
    /// Existing nodes get the new `build_id` added to `interested_builds`.
    ///
    /// If the merge would create a cycle, returns `Err(DagError::CycleDetected)`
    /// and rolls back all newly-inserted nodes/edges/interest.
    ///
    /// Callers that perform additional persistence (e.g., DB writes) after a
    /// successful merge should call `rollback_merge()` with the returned
    /// `MergeResult` fields if their persistence fails, to avoid in-memory
    /// DAG state drifting from the DB.
    ///
    /// Equivalent to [`Self::merge_with_evidence`] with an empty
    /// store-evidence set: every displacement decision sees only the
    /// submission's ingress shape rank.
    // Production enters via merge_with_evidence (the actor threads the
    // store-evidence set); this convenience wrapper keeps the ~190
    // existing test call sites stable, so outside cfg(test) it has no
    // callers by design.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn merge(
        &mut self,
        build_id: Uuid,
        nodes: &[crate::domain::DerivationNode],
        edges: &[crate::domain::DerivationEdge],
        submitter_traceparent: &str,
    ) -> Result<MergeResult, DagError> {
        self.merge_with_evidence(
            build_id,
            nodes,
            edges,
            submitter_traceparent,
            &HashMap::new(),
        )
    }

    /// [`Self::merge`] with merge-time STORE evidence: hashes in
    /// `store_evidence` were verified by the actor against the store's
    /// own text-CA-enforced `.drv` bytes
    /// (`sched.merge.store-evidence-displacement+2`), so their incoming
    /// nodes displace with `PathBoundBytes` standing instead of their
    /// bare ingress shape rank. The set only ever RAISES a displacer's
    /// rank — victims' protection ranks are read from their own state.
    /// Each grant additionally carries the byte-derived facts the
    /// created node must record ([`StoreEvidenceGrant`]).
    // r[impl sched.merge.poisoned-resubmit-bounded+2]
    // r[impl sched.merge.evidence-ranked-displacement]
    pub fn merge_with_evidence(
        &mut self,
        build_id: Uuid,
        nodes: &[crate::domain::DerivationNode],
        edges: &[crate::domain::DerivationEdge],
        submitter_traceparent: &str,
        store_evidence: &HashMap<DrvHash, StoreEvidenceGrant>,
    ) -> Result<MergeResult, DagError> {
        let mut newly_inserted = HashSet::new();
        let mut reset_on_resubmit = Vec::new();
        let mut authority_takeovers: Vec<DrvHash> = Vec::new();
        let mut displaced: Vec<DrvHash> = Vec::new();
        // Children edges scrubbed from displaced nodes, for rollback
        // restoration and the durable parent-side edge delete.
        let mut displaced_scrubbed_edges: Vec<(DrvHash, HashSet<DrvHash>)> = Vec::new();
        // Track newly-inserted edges for rollback (pairs of hashes)
        let mut new_edges: Vec<(DrvHash, DrvHash)> = Vec::new();
        // Submitted edges skipped because their parent is a resident node
        // this submission did not (re)create (sched.merge.edge-creation-scoped),
        // split by whether the parent carries the closure_hole breadcrumb
        // (rejoin signature) or not (hostile / gateway-bug signature).
        let mut foreign_parent_edges_skipped: Vec<(DrvHash, DrvHash)> = Vec::new();
        let mut rejoin_parent_edges_skipped: Vec<(DrvHash, DrvHash)> = Vec::new();
        // Per-parent edge-admission bookkeeping for healed_parents
        // (sched.merge.heal-accepted-edges+1): the TRIGGER is computed iff every
        // declared edge naming it was accepted (attached or already
        // present). Vetoes: gate-skipped edge (either kind) or an edge
        // whose child endpoint did not resolve.
        let mut edge_parents_declared: HashSet<DrvHash> = HashSet::new();
        let mut edge_parents_vetoed: HashSet<DrvHash> = HashSet::new();
        // Track pre-existing nodes that gained interest in this merge, so
        // rollback only removes interest from these (not from nodes where
        // build_id was already present from a prior successful merge).
        let mut interest_added: Vec<DrvHash> = Vec::new();
        // Pre-union wanted_output_names of pre-existing nodes whose
        // wanted set was grown by this merge, so rollback can restore
        // them. A leaked union is only ever conservative (more outputs
        // required for a cache hit) but would still violate the
        // exact-pre-merge-DAG restore invariant.
        let mut wanted_grown: Vec<(DrvHash, Vec<String>)> = Vec::new();
        // Prior `wanted_by_build` entry (None = absent) of pre-existing
        // nodes where this merge recorded the submitting build's
        // contribution, so rollback can remove/restore it.
        let mut contributions_recorded: Vec<(DrvHash, Option<Vec<String>>)> = Vec::new();
        // Full prior state of every pre-existing node destructively
        // removed below (resubmit-reset of retriable nodes, displacement
        // of terminal authoritative-content squats). rollback_merge
        // restores these so a failed merge leaves the DAG exactly as it
        // was (status, interest set, retry.resubmit_cycles toward
        // POISON_RESUBMIT_RETRY_LIMIT all preserved).
        let mut removed_retriable: Vec<(DrvHash, DerivationState)> = Vec::new();
        // Pre-existing nodes whose empty traceparent was upgraded below.
        let mut traceparent_upgraded: Vec<DrvHash> = Vec::new();

        // Insert or update nodes
        for node in nodes {
            // Exchange the fresh proto-derived hash for the canonical one
            // from a prior merge if this node already exists. canonical()
            // returns an Arc-clone of the existing map key, so downstream
            // pushes into interest_added don't proliferate distinct allocs.
            // If the node is new, our fresh Arc BECOMES the canonical one
            // when inserted below.
            let drv_hash = self
                .canonical(node.drv_hash.as_str())
                .unwrap_or_else(|| node.drv_hash.as_str().into());

            // r[impl sched.merge.authoritative-conflict+6]
            // Authoritative-content protection. Evaluated BEFORE the
            // resubmit-reset below so the existing node is examined in
            // EVERY lifecycle state — running it after the reset would
            // structurally exempt retriable nodes (Failed / Cancelled /
            // DependencyFailed / Poisoned-under-budget), letting the only
            // copy of their derivation be silently redefined with
            // inherited interest. When the existing node carries the only
            // copy of its derivation (content-bound hook fallback), a
            // later submission for the same drv_hash must not be able to
            // redefine it:
            //   - another AUTHORITATIVE submission must be byte-identical
            //     (identical hook resubmissions — the only legitimate
            //     producer — are) while the existing claim is live,
            //     Failed (still owned by the retry machinery), or
            //     finished successfully; once the existing claim is
            //     parked in a TERMINAL FAILURE state (Poisoned /
            //     Cancelled / DependencyFailed), a byte-different
            //     authoritative submission DISPLACES it instead — the
            //     hook-fallback population submits authoritatively and
            //     has no store-backed form, so without this arm a failed
            //     squat would lock those victims out of the hash for the
            //     rest of its poison TTL;
            //   - a store-backed (non-authoritative) submission whose
            //     verifiable identity conflicts is rejected while the
            //     node is in flight OR finished successfully
            //     (Completed/Skipped — a settled record is never
            //     erased by an unverified claim), and DISPLACES it once
            //     the node sits in a terminal FAILURE state — which
            //     includes a poison-budget-exhausted squat — (the
            //     verifiable definition wins; prior interest is NOT
            //     carried and the fresh node starts with a fresh poison
            //     budget: it is a different definition, not a retry of
            //     the old one); an identity-MATCHING store-backed
            //     submission joins as usual, EXCEPT when the node sits
            //     in a poison-locked terminal failure state (no longer
            //     retriable on resubmit): then it displaces the node
            //     too, so a locked claim cannot capture later legitimate
            //     submissions for the rest of its poison TTL;
            //   - the INVERSE direction is gated too
            //     (sched.merge.authoritative-claim-no-redefine): an
            //     authoritative claim landing on a STORE-BACKED node
            //     that is eligible for the resubmit-reset must prove the
            //     same verifiable identity or be rejected — on a match
            //     it is admitted through the normal reset and its bytes
            //     are adopted; it never displaces the store-backed node.
            // Store-backed existing nodes are otherwise exempt — live,
            // or terminal but not retriable on resubmit: their truth
            // lives in the store, so a later submission's bytes are
            // ignored and the node joins as before.
            // Per-node displacer standing: the ingress shape rank,
            // raised to PathBoundBytes when the actor verified this
            // hash against the store's own text-CA-enforced bytes
            // (sched.merge.store-evidence-displacement+2). Computed once
            // per node, consumed by every displace() call in the arms.
            let displacer_evidence = {
                let shape = DefinitionEvidence::from_node_shape(node);
                if store_evidence.contains_key(&drv_hash) {
                    shape.max(DefinitionEvidence::PathBoundBytes)
                } else {
                    shape
                }
            };
            if let Some(existing) = self.nodes.get(&drv_hash) {
                if node.drv_content_authoritative && existing.drv_content_authoritative {
                    if existing.drv_content != node.drv_content {
                        // Byte-different authoritative content. The
                        // displacement primitive owns the inner
                        // decision (sched.merge.evidence-ranked-
                        // displacement): a claim that finished
                        // successfully (Completed/Skipped) is never
                        // redefined by an equal-or-lower-ranked
                        // claimant, a live or Failed claim keeps
                        // first-writer-wins (RefusedInFlight), and a
                        // claim parked in a terminal failure state is
                        // displaced by the redefinition — same
                        // bookkeeping as the store-backed displacement
                        // arm below, so the link prune, in-tx
                        // accumulator reset, edge scrub, and
                        // accounting all apply to it.
                        let verdict = self.displace(
                            &drv_hash,
                            displacer_evidence,
                            &mut DisplacementBookkeeping {
                                removed_retriable: &mut removed_retriable,
                                displaced_scrubbed_edges: &mut displaced_scrubbed_edges,
                                displaced: &mut displaced,
                            },
                        );
                        if verdict != DisplaceVerdict::Displaced {
                            self.rollback_merge(
                                &newly_inserted,
                                &new_edges,
                                &interest_added,
                                &traceparent_upgraded,
                                &wanted_grown,
                                &contributions_recorded,
                                build_id,
                                removed_retriable,
                                displaced_scrubbed_edges,
                            );
                            return Err(DagError::AuthoritativeContentMismatch {
                                drv_path: node.drv_path.clone(),
                            });
                        }
                    }
                } else if !node.drv_content_authoritative && existing.drv_content_authoritative {
                    let identity_matches = verifiable_identity_matches(existing, node);
                    // Poison-locked: a terminal failure state the
                    // resubmit-reset can no longer clear (today this is
                    // exactly Poisoned with the resubmit budget
                    // exhausted; written broadly as defense-in-depth).
                    // An identity-MATCHING store-backed submission must
                    // displace such a node rather than join it —
                    // joining would attach the submitter to a build
                    // that can never run again, so the locked claim
                    // would capture every later legitimate submission
                    // of the derivation for the rest of its poison TTL.
                    let locked_terminal_failure = existing.status().is_terminal()
                        && !matches!(
                            existing.status(),
                            DerivationStatus::Completed | DerivationStatus::Skipped
                        )
                        && !existing.is_retriable_on_resubmit();
                    if !identity_matches || locked_terminal_failure {
                        // The displacement primitive owns the inner
                        // decision (sched.merge.evidence-ranked-
                        // displacement). Settled (Completed/Skipped)
                        // victims are protected by the strict rank
                        // rule — a bare store-backed echo
                        // (UnverifiedClaim) never erases the record of
                        // a successful build (bug_076), while an
                        // ingress-byte-bound or store-evidence-backed
                        // displacer (PathBoundBytes) strictly outranks
                        // a content-bound squat and reclaims the hash.
                        // Terminal-failure victims displace regardless
                        // of rank: the fresh store-backed definition is
                        // inserted below as a brand-new node
                        // (prior=None → no interest carry,
                        // resubmit_cycles=0, not in reset_on_resubmit);
                        // the prior state rides removed_retriable so
                        // rollback_merge restores it if a LATER node in
                        // this same submission fails the merge.
                        let verdict = self.displace(
                            &drv_hash,
                            displacer_evidence,
                            &mut DisplacementBookkeeping {
                                removed_retriable: &mut removed_retriable,
                                displaced_scrubbed_edges: &mut displaced_scrubbed_edges,
                                displaced: &mut displaced,
                            },
                        );
                        if verdict != DisplaceVerdict::Displaced {
                            // Reachable on an identity conflict against
                            // a live, Failed, or settled-and-outranking
                            // (Completed/Skipped) node — a poison-locked
                            // node is a terminal failure by
                            // construction, so the identity-MATCHING
                            // case never lands here.
                            self.rollback_merge(
                                &newly_inserted,
                                &new_edges,
                                &interest_added,
                                &traceparent_upgraded,
                                &wanted_grown,
                                &contributions_recorded,
                                build_id,
                                removed_retriable,
                                displaced_scrubbed_edges,
                            );
                            return Err(DagError::ConflictingInFlightContent {
                                drv_path: node.drv_path.clone(),
                            });
                        }
                    }
                } else if !node.drv_content_authoritative
                    && !existing.drv_content_authoritative
                    && matches!(
                        existing.status(),
                        DerivationStatus::Completed | DerivationStatus::Skipped
                    )
                    && !verifiable_identity_matches(existing, node)
                {
                    // r[impl sched.merge.store-evidence-displacement+2]
                    // Settled bare x bare identity conflict (bug_072):
                    // the store-backed join exemption is for nodes
                    // whose truth lives in the store — but a SETTLED
                    // bare node's recorded outputs are served as a
                    // cache hit to every joiner, so a conflicting
                    // incoming claim must never silently attach (the
                    // genuine owner would be handed a squat's forged
                    // outputs). The displacement primitive owns the
                    // decision, rank-uniform with every other settled
                    // victim form: a store-evidence-granted incoming
                    // (PathBoundBytes, verified by Step 0.6 against
                    // the store's own text-CA bytes) displaces an
                    // UnverifiedClaim/ContentBoundClaim squat; an
                    // ungranted conflicting echo is REFUSED — fail
                    // closed, never join-by-default. Identity-MATCHING
                    // bare resubmissions (incl. the M_070
                    // preserved-claim/dual-anchor bases) never reach
                    // this arm and keep the join semantics.
                    let verdict = self.displace(
                        &drv_hash,
                        displacer_evidence,
                        &mut DisplacementBookkeeping {
                            removed_retriable: &mut removed_retriable,
                            displaced_scrubbed_edges: &mut displaced_scrubbed_edges,
                            displaced: &mut displaced,
                        },
                    );
                    if verdict != DisplaceVerdict::Displaced {
                        self.rollback_merge(
                            &newly_inserted,
                            &new_edges,
                            &interest_added,
                            &traceparent_upgraded,
                            &wanted_grown,
                            &contributions_recorded,
                            build_id,
                            removed_retriable,
                            displaced_scrubbed_edges,
                        );
                        return Err(DagError::ConflictingInFlightContent {
                            drv_path: node.drv_path.clone(),
                        });
                    }
                } else if node.drv_content_authoritative
                    && !existing.drv_content_authoritative
                    && existing.is_retriable_on_resubmit()
                    && !verifiable_identity_matches(existing, node)
                {
                    // r[impl sched.merge.authoritative-claim-no-redefine]
                    // The claim would otherwise be ADOPTED by the
                    // resubmit-reset below (`is_retriable_on_resubmit`
                    // is exactly the reset's admission set), redefining
                    // a parked store-backed definition with bytes only
                    // the claimant can see — so a conflicting identity
                    // is rejected here, leaving the existing node
                    // untouched. A matching identity falls through and
                    // takes the normal reset (bytes adopted, prior
                    // interest carried, resubmit cycle accumulated).
                    // Live and non-retriable store-backed nodes never
                    // reach this arm: they keep the join-and-ignore
                    // semantics, and an authoritative claim never
                    // displaces a store-backed node.
                    self.rollback_merge(
                        &newly_inserted,
                        &new_edges,
                        &interest_added,
                        &traceparent_upgraded,
                        &wanted_grown,
                        &contributions_recorded,
                        build_id,
                        removed_retriable,
                        displaced_scrubbed_edges,
                    );
                    return Err(DagError::AuthoritativeClaimIdentityConflict {
                        drv_path: node.drv_path.clone(),
                    });
                }
            }

            // Resubmit-retry: if the existing node is Cancelled or Failed,
            // remove it so the else-branch below re-inserts fresh state and
            // it flows through `compute_initial_states` → `newly_inserted`.
            // The authoritative-conflict gate above has already admitted
            // this submission (byte-identical authoritative retry,
            // identity-matching store-backed resubmit, or a store-backed
            // existing node), so the reset only ever recreates the SAME
            // derivation definition. A displaced node is already gone from
            // `self.nodes` at this point, so `prior` stays `None` for it.
            // Defense-in-depth: reap now removes Cancelled nodes for terminal
            // builds, so the reset is only load-bearing during the
            // TERMINAL_CLEANUP_DELAY window or for nodes shared with a
            // still-active build at cancel time. Without it, merge adds
            // interest but `compute_initial_states` only iterates
            // newly-inserted, and `handle_merge_dag`'s pre-existing-node
            // match ignores Cancelled — the resubmitted build would hang.
            //
            // Edges are NOT scrubbed for same-definition resets (including
            // byte-identical authoritative retries): `children`/`parents`
            // are keyed by hash string, so they stay valid across the
            // remove+reinsert and the merge's own edge loop re-adds this
            // submission's edges idempotently. An authority takeover
            // (authoritative squat replaced by a store-backed definition)
            // is a definition change and gets the same children scrub as
            // displacement — see below.
            // Because the (possibly reap-truncated) child set survives the
            // reset without being re-declared, the `closure_hole`
            // breadcrumb that qualifies it must ride along in the carry
            // below.
            //
            // Prior interested_builds are carried over so any OTHER build
            // that was stuck on this Cancelled node also benefits from the
            // reset. resubmit_cycles is carried over so the
            // Poisoned-resubmit bound (POISON_RESUBMIT_RETRY_LIMIT)
            // accumulates across resubmits — without this, every reset
            // would start at 0 and the bound would never fire (I-169).
            // closure_hole is carried because the breadcrumb records the
            // relation between the node's child set and its pruned input
            // closure: the reset keeps the (possibly truncated) edges and
            // does not re-declare them, so the truncation evidence must
            // outlive the retry — dropped, a re-pruning merge of a holed
            // node with produced survivors reads as Vouched and both stamp
            // gates skip the mark, re-arming the doomed from-source
            // dispatch. topdown_pruned is deliberately NOT carried: the
            // incoming merge's own prune/stamp decision re-derives it (a
            // re-pruning merge sees Broken evidence via the carried hole
            // and re-stamps; a full merge re-declares the edges and the
            // heal clears the carried hole).
            let prior = if self
                .nodes
                .get(&drv_hash)
                .is_some_and(DerivationState::is_retriable_on_resubmit)
            {
                let old = self.nodes.remove(&drv_hash).expect("just checked is_some");
                // Authority takeover: the removed retriable node carried
                // authoritative inline content and the re-creating
                // submission is store-backed (the gate above only admits
                // this shape on a verifiable identity match). The fresh
                // node is the store-backed definition taking over the
                // hash, so the squat's consumed poison budget must not
                // carry over (sched.merge.displaced-failure-reset).
                let authority_flip =
                    old.drv_content_authoritative && !node.drv_content_authoritative;
                if authority_flip {
                    // r[impl sched.merge.displaced-edge-scrub+2]
                    // Definition change ⇒ the same children scrub as
                    // displacement: the squat's dependency edges must not
                    // carry onto the store-backed definition taking over
                    // the hash. Recorded in displaced_scrubbed_edges so a
                    // failed merge restores them with the node, and so the
                    // persist deletes the parent-side rows in the same
                    // transaction. Same-definition resets (the
                    // !authority_flip path) keep their edges.
                    let scrubbed = self.scrub_dependency_edges(&drv_hash);
                    if !scrubbed.is_empty() {
                        displaced_scrubbed_edges.push((drv_hash.clone(), scrubbed));
                    }
                }
                let carry = (
                    old.interested_builds.clone(),
                    old.retry.resubmit_cycles,
                    old.wanted_output_names.clone(),
                    old.wanted_by_build.clone(),
                    old.closure_hole.clone(),
                    authority_flip,
                );
                removed_retriable.push((drv_hash.clone(), old));
                Some(carry)
            } else {
                None
            };

            // Every mutation in this branch MUST be tracked for
            // `rollback_merge` — a failed merge restores the exact
            // pre-merge DAG. Currently: `interested_builds` (via
            // `interest_added`), `traceparent` (via
            // `traceparent_upgraded`), `wanted_output_names` (via
            // `wanted_grown`), and `wanted_by_build` (via
            // `contributions_recorded`).
            if let Some(existing) = self.nodes.get_mut(&drv_hash) {
                // Node already exists: add this build's interest.
                // `insert` returns true iff build_id was not already present.
                if existing.interested_builds.insert(build_id) {
                    interest_added.push(drv_hash.clone());
                }
                // Demand-driven completeness: a second build wanting
                // MORE outputs of an already-known node grows the
                // in-memory wanted set (union; empty = "all" saturates).
                // Never shrink — build B's `{out}` must not un-want
                // build A's still-needed `dev`.
                if existing.wanted_output_names != node.wanted_output_names {
                    let prior = existing.wanted_output_names.clone();
                    existing.union_wanted(&node.wanted_output_names);
                    if existing.wanted_output_names != prior {
                        wanted_grown.push((drv_hash.clone(), prior));
                    }
                }
                // Per-build contribution: remember WHAT this submission
                // wanted so the effective-wanted computation can stop
                // counting it once this build goes terminal. Union (not
                // overwrite) so a submission that carries the same drv
                // twice with different wanted sets accumulates both
                // occurrences instead of keeping only the last one. The
                // prior entry (usually absent; present when the same
                // build re-merges the node) is captured BEFORE the
                // mutation for rollback, mirroring `wanted_grown`;
                // rollback_merge replays these in reverse, so the
                // first-captured (true pre-merge) value is the one that
                // sticks even when one merge records the node twice.
                let prior_contribution = existing.wanted_by_build.get(&build_id).cloned();
                existing
                    .wanted_by_build
                    .entry(build_id)
                    .and_modify(|w| union_wanted_saturating(w, &node.wanted_output_names))
                    .or_insert_with(|| node.wanted_output_names.clone());
                contributions_recorded.push((drv_hash.clone(), prior_contribution));
                // First submitter's traceparent wins — but recovery/
                // poison-reset set "", which isn't a submitter. Upgrade
                // an empty traceparent so a user submitting after
                // failover gets their trace linked to the worker span.
                if existing.traceparent.is_empty() && !submitter_traceparent.is_empty() {
                    existing.traceparent = submitter_traceparent.to_string();
                    traceparent_upgraded.push(drv_hash);
                }
            } else {
                // New node. try_from_node validates drv_path: StorePath::parse.
                // Invalid paths fail the whole merge with inline rollback of
                // EVERYTHING this merge has touched so far (nodes inserted in
                // earlier loop iterations, edges, interest, removed retriable
                // nodes). The gRPC layer also validates upfront and returns
                // INVALID_ARGUMENT, so this is belt-and-suspenders — but it's
                // the only thing protecting us if the actor is ever driven by
                // something other than the gRPC layer.
                let mut state = match DerivationState::try_from_node(node) {
                    Ok(s) => s,
                    Err(source) => {
                        self.rollback_merge(
                            &newly_inserted,
                            &new_edges,
                            &interest_added,
                            &traceparent_upgraded,
                            &wanted_grown,
                            &contributions_recorded,
                            build_id,
                            removed_retriable,
                            displaced_scrubbed_edges,
                        );
                        return Err(DagError::InvalidDrvPath {
                            path: node.drv_path.clone(),
                            source,
                        });
                    }
                };
                // I-204: strip soft features at insertion so the
                // I-181/I-176 snapshot filter and rejection_reason's
                // `feature-missing` clause both see hardware-gate
                // features only. With `soft_features=[big-parallel]`,
                // a `{big-parallel}` derivation becomes ∅-feature →
                // featureless pools count it (subset vacuously true),
                // kvm pool skips it (I-181 ∅ guard).
                self.apply_soft_features(&mut state);
                // r[impl sched.merge.evidence-ranked-displacement]
                // Merge-time store evidence is a PathBoundBytes SOURCE
                // (sched.derivation.evidence-rank): the actor verified
                // this hash's incoming identity against the store's own
                // text-CA-enforced bytes, so the created node — and the
                // row the creation-scoped upsert persists from it —
                // carries the verified standing, not the bare echo's
                // shape rank. Same value the displacement verdict used.
                state.evidence = state.evidence.max(displacer_evidence);
                // r[impl sched.dispatch.claims-derived+3]
                // A store-evidence-created node's resolve flag is the
                // BYTE-DERIVED one computed at the classification site
                // (VerifiedDefinition.needs_resolve) — the proto echo
                // try_from_node copied is overwritten so nothing
                // downstream of a verified creation can consult the
                // submitter's claim.
                if let Some(grant) = store_evidence.get(&drv_hash) {
                    state.ca.needs_resolve = grant.needs_resolve;
                    // r[impl sched.merge.store-evidence-displacement+2]
                    // Strip consequence parity (merged_bug_020/038):
                    // when the verification verdict was
                    // VerifiedExceptDeclaredHash, the grant carries the
                    // strip — the created node sheds the submitter's
                    // unverifiable declared hash exactly like the
                    // dispatch strip arm (an unverifiable claim is no
                    // claim), and the claim is PRESERVED out-of-band
                    // (M_070) so the row this node settles into stays
                    // matchable. One verdict, one consequence, at both
                    // consumers.
                    if let Some(stripped) = grant.stripped_declared_hash {
                        state.ca.modular_hash = None;
                        state.ca.modular_hash_stripped = Some(stripped);
                    }
                }
                state.interested_builds.insert(build_id);
                // The submitting build's per-build contribution — the
                // counterpart of the existing-node path's insert.
                state
                    .wanted_by_build
                    .insert(build_id, node.wanted_output_names.clone());
                // Carry over interested_builds + resubmit_cycles from the
                // removed retriable node (if any) — other stuck builds
                // get the reset too; resubmit_cycles is INCREMENTED here
                // (the reset itself IS the cycle event) so the
                // POISON_RESUBMIT_RETRY_LIMIT bound accumulates.
                // retry.count stays at the fresh-state default (0) → full
                // per-cycle max_retries budget restored on every resubmit.
                // The prior wanted set is unioned in too — the carried-over
                // interested builds still want their outputs; replacing the
                // set with only the resubmitter's would shrink it. Their
                // per-build contributions are carried alongside (they
                // follow interest membership); `or_insert` keeps the
                // resubmitter's own entry authoritative. The closure-hole
                // breadcrumb rides along too — the kept edges were not
                // re-declared by this reset (see the carry site above).
                //
                // r[impl sched.merge.displaced-failure-reset+2]
                // Exception: an authority takeover (the removed node was
                // authoritative, the re-creating submission store-backed)
                // is a definition change, not a retry of the prior
                // definition — it starts with a fresh poison budget
                // (cycles = 0, mirroring the in-tx row reset) and is
                // surfaced in `authority_takeovers` so the actor skips
                // the cycle-incrementing `clear_poison_batch` for it.
                if let Some((
                    prior_interest,
                    prior_cycles,
                    prior_wanted,
                    prior_contributions,
                    prior_closure_hole,
                    authority_flip,
                )) = prior
                {
                    state.interested_builds.extend(prior_interest);
                    state.retry.resubmit_cycles = if authority_flip { 0 } else { prior_cycles + 1 };
                    if authority_flip {
                        authority_takeovers.push(drv_hash.clone());
                    }
                    state.union_wanted(&prior_wanted);
                    for (b, w) in prior_contributions {
                        state.wanted_by_build.entry(b).or_insert(w);
                    }
                    // r[impl sched.closure.witness-epoch]
                    // The witness rides a same-definition resubmit and
                    // dies at an authority takeover — the same boundary
                    // that already scrubbed the squat's edges and reset
                    // its poison budget above. Carrying it across would
                    // demand the genuine definition "re-supply" the
                    // squat's junk children, which its real inputDrvs
                    // can never contain: every re-declaration would be
                    // heal-refused and the node permanently
                    // fail-fast-routed (round-16 bug_011). The durable
                    // half (PG flag+rows, kept alive by the upsert's
                    // OR semantics) is cleared by the merge
                    // transaction's definition-change clear in
                    // `persist_merge_to_db`.
                    state.closure_hole = prior_closure_hole.carry_across(if authority_flip {
                        crate::state::DefinitionTransition::AuthorityTakeover
                    } else {
                        crate::state::DefinitionTransition::SameDefinition
                    });
                    reset_on_resubmit.push(drv_hash.clone());
                }
                state.traceparent = submitter_traceparent.to_string();
                // All three inserts clone the SAME Arc — they're mutually
                // ptr-equal. This is what makes path_to_hash.get().cloned()
                // canonical, which makes the edge-insert loop canonical, etc.
                self.path_to_hash
                    .insert(state.drv_path().to_string(), drv_hash.clone());
                self.nodes.insert(drv_hash.clone(), state);
                newly_inserted.insert(drv_hash);
            }
        }

        // Insert edges
        //
        // SubmitBuild ingress guarantees both endpoints of every edge name
        // nodes of the same request (sched.merge.ingress-edge-endpoints),
        // so in production these lookups only miss on path aliasing (a
        // node joined under a different drv_path than the resident one)
        // or when merge() is driven by a non-gRPC caller. The warn-skip
        // stays as the last line of defense: an unresolved endpoint must
        // not attach edges to nodes the submission never declared.
        //
        // Ingress only constrains edges to THIS request's nodes; it cannot
        // know which of them are resident. A node's dependency set is
        // intrinsic to its `.drv`, so only the submission that (re)creates
        // a node may define its children — a later submission that merely
        // joins a resident node may re-declare the existing edges (silent
        // no-ops) but must not extend them, or any submitter could attach
        // a failing junk dependency to someone else's resident node. The
        // one exception is a topdown-pruned root: its creating submission
        // deliberately dropped the dependency edges, and a later full
        // merge is expected to top them up while the node is resident
        // (see handle_substitute_complete).
        for edge in edges {
            // Edges reference drv_path; resolve to drv_hash
            let Some(parent_hash) = self
                .path_to_hash
                .get(edge.parent_drv_path.as_str())
                .cloned()
            else {
                tracing::warn!(
                    parent_path = %edge.parent_drv_path,
                    "edge references unknown parent drv_path, skipping"
                );
                continue;
            };
            // Every edge whose parent resolves declares that parent's
            // (partial or full) dependency set — track it for the
            // healed_parents computation regardless of what happens to
            // the edge below.
            edge_parents_declared.insert(parent_hash.clone());
            let Some(child_hash) = self.path_to_hash.get(edge.child_drv_path.as_str()).cloned()
            else {
                tracing::warn!(
                    child_path = %edge.child_drv_path,
                    "edge references unknown child drv_path, skipping"
                );
                // An unresolvable child means this parent's declared set
                // could not be fully attached → its child set is not
                // representative of its closure → veto the heal.
                edge_parents_vetoed.insert(parent_hash);
                continue;
            };

            // r[impl sched.merge.edge-creation-scoped]
            // Creation-scoped edge gate: attach the edge only when this
            // submission (re)created the parent (plain insert,
            // resubmit-reset, displacement, authority takeover — all of
            // which land in `newly_inserted`) or the resident parent is a
            // topdown-pruned root awaiting its dependency top-up.
            // Re-declarations of an edge that already exists stay accepted
            // as silent no-ops (the gateway re-emits each parent's full
            // edge set on every full-closure join).
            let already_present = self
                .children
                .get(&parent_hash)
                .is_some_and(|cs| cs.contains(&child_hash));
            let parent_creation_scoped = newly_inserted.contains(&parent_hash)
                || self
                    .nodes
                    .get(&parent_hash)
                    .is_some_and(|s| s.topdown_pruned);
            if !parent_creation_scoped && !already_present {
                // r[impl sched.merge.heal-accepted-edges+1]
                // A gate-skipped edge vetoes its parent's heal either way;
                // the split below only decides observability. A parent
                // carrying the closure_hole breadcrumb is the legitimate
                // post-reap rejoin signature (full-closure re-declaration
                // of a node whose children were reaped); anything else is
                // a hostile direct submitter or a gateway bug.
                edge_parents_vetoed.insert(parent_hash.clone());
                let parent_holed = self
                    .nodes
                    .get(&parent_hash)
                    .is_some_and(|s| s.closure_hole.is_holed());
                if parent_holed {
                    tracing::debug!(
                        parent_path = %edge.parent_drv_path,
                        child_path = %edge.child_drv_path,
                        %build_id,
                        "edge parent is a closure-holed resident node not (re)created \
                         by this submission; skipping rejoin dependency edge \
                         (hole stays until the node is re-created)"
                    );
                    rejoin_parent_edges_skipped.push((parent_hash, child_hash));
                } else {
                    tracing::warn!(
                        parent_path = %edge.parent_drv_path,
                        child_path = %edge.child_drv_path,
                        %build_id,
                        "edge parent is a resident node not (re)created by this \
                         submission; skipping foreign dependency edge"
                    );
                    foreign_parent_edges_skipped.push((parent_hash, child_hash));
                }
                continue;
            }

            let inserted_child = self
                .children
                .entry(parent_hash.clone())
                .or_default()
                .insert(child_hash.clone());
            self.parents
                .entry(child_hash.clone())
                .or_default()
                .insert(parent_hash.clone());

            if inserted_child {
                new_edges.push((parent_hash, child_hash));
            }
        }

        // r[impl sched.merge.heal-accepted-edges+1]
        // r[impl sched.evidence.positive-witness]
        // accepted = declared − vetoed (the TRIGGER); healed = accepted
        // ∧ witness-set coverage. The subset check runs over the
        // post-insert children map — both operands scheduler-owned
        // (sched.evidence.positive-witness): the witness set was
        // recorded by the truncation that removed the children, and
        // the children map reflects what THIS merge actually attached.
        let accepted_edge_parents: Vec<DrvHash> = edge_parents_declared
            .into_iter()
            .filter(|p| !edge_parents_vetoed.contains(p))
            .collect();
        let mut healed_parents: Vec<(DrvHash, HealWitness)> = Vec::new();
        let mut heal_refused_parents: Vec<DrvHash> = Vec::new();
        for p in &accepted_edge_parents {
            let covered = match self.nodes.get(p).map(|s| s.closure_hole.missing()) {
                // Un-holed (or no longer resident): trivially covered —
                // the stale-PG total-clear semantics (see the field doc).
                None => true,
                Some(missing) if missing.is_empty() => true,
                Some(missing) => {
                    let children = self.children.get(p);
                    missing
                        .iter()
                        .all(|c| children.is_some_and(|cs| cs.contains(c)))
                }
            };
            if covered {
                // The ONLY HealWitness constructor (see the type doc).
                healed_parents.push((p.clone(), HealWitness(())));
            } else {
                heal_refused_parents.push(p.clone());
            }
        }

        // Cycle check: DFS from each newly-inserted node AND from each parent
        // endpoint of new edges. The latter catches cycles formed by new edges
        // between two pre-existing nodes (no new nodes inserted, so the
        // newly_inserted loop alone would miss them) — under the
        // creation-scoped edge gate above this is only reachable through
        // the topdown-pruned carve-out, but the coverage stays.
        let mut color: HashMap<String, u8> = HashMap::new();
        let mut dfs_starts: Vec<&str> = newly_inserted.iter().map(|s| s.as_str()).collect();
        for (parent, _child) in &new_edges {
            // Only need to DFS from edge endpoints not already covered by
            // newly_inserted (avoids redundant work).
            if !newly_inserted.contains(parent) {
                dfs_starts.push(parent.as_str());
            }
        }
        for start in dfs_starts {
            if self.has_cycle_from(start, &mut color) {
                // Rollback: remove newly-inserted edges and nodes
                self.rollback_merge(
                    &newly_inserted,
                    &new_edges,
                    &interest_added,
                    &traceparent_upgraded,
                    &wanted_grown,
                    &contributions_recorded,
                    build_id,
                    removed_retriable,
                    displaced_scrubbed_edges,
                );
                return Err(DagError::CycleDetected);
            }
        }

        Ok(MergeResult {
            newly_inserted,
            reset_on_resubmit,
            authority_takeovers,
            displaced,
            new_edges,
            interest_added,
            removed_retriable,
            displaced_scrubbed_edges,
            foreign_parent_edges_skipped,
            rejoin_parent_edges_skipped,
            accepted_edge_parents,
            healed_parents,
            heal_refused_parents,
            traceparent_upgraded,
            wanted_grown,
            contributions_recorded,
        })
    }

    /// Iterative DFS cycle detection with three-color marking.
    /// color: 0=white (unvisited), 1=gray (in stack), 2=black (done).
    /// A back-edge to a gray node indicates a cycle.
    ///
    /// Iterative (not recursive) to avoid stack overflow on deep chains:
    /// MAX_DAG_NODES=1M × ~150B/frame ≈ 150MB >> 2MB default tokio stack.
    fn has_cycle_from(&self, start: &str, color: &mut HashMap<String, u8>) -> bool {
        // Short-circuit if start already visited (color map persists across
        // multiple has_cycle_from calls in merge()).
        match color.get(start) {
            Some(1) => return true,
            Some(2) => return false,
            _ => {}
        }

        // Explicit stack: each frame is (node, children_iterator_state).
        // We store the child vec snapshot + index to resume iteration after
        // returning from a child's subtree.
        // Using Vec<String> snapshots avoids borrow-checker conflicts with
        // `color.insert(node.to_string(), ...)` inside the loop.
        struct Frame {
            node: String,
            children: Vec<DrvHash>,
            next_child: usize,
        }

        let children_of = |n: &str| -> Vec<DrvHash> {
            self.children
                .get(n)
                .map(|set| set.iter().cloned().collect())
                .unwrap_or_default()
        };

        let mut stack: Vec<Frame> = vec![Frame {
            node: start.to_string(),
            children: children_of(start),
            next_child: 0,
        }];
        color.insert(start.to_string(), 1);

        while let Some(frame) = stack.last_mut() {
            if frame.next_child < frame.children.len() {
                let child = frame.children[frame.next_child].clone();
                frame.next_child += 1;

                match color.get(child.as_str()) {
                    Some(1) => return true, // back edge → cycle
                    Some(2) => continue,    // already fully explored
                    _ => {
                        // Unvisited: push a new frame (descend).
                        let grandchildren = children_of(&child);
                        let node = child.to_string();
                        color.insert(node.clone(), 1);
                        stack.push(Frame {
                            node,
                            children: grandchildren,
                            next_child: 0,
                        });
                    }
                }
            } else {
                // All children visited: mark black and pop.
                let done = stack.pop().expect("stack nonempty in else branch");
                color.insert(done.node, 2);
            }
        }

        false
    }

    /// Rollback a failed merge: remove newly-inserted nodes and edges,
    /// remove build interest from pre-existing nodes that gained it during
    /// this merge (but not from nodes where build_id was already present),
    /// and restore every node the merge destructively removed
    /// (resubmit-reset and displacement alike), re-attaching the
    /// dependency edges the displacement arms scrubbed.
    // The parameters mirror `MergeResult`'s fields one-to-one (the two
    // inline callers in `merge()` hold them as locals, not as a
    // `MergeResult`); a struct param would just rename the args.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn rollback_merge(
        &mut self,
        newly_inserted: &HashSet<DrvHash>,
        new_edges: &[(DrvHash, DrvHash)],
        interest_added: &[DrvHash],
        traceparent_upgraded: &[DrvHash],
        wanted_grown: &[(DrvHash, Vec<String>)],
        contributions_recorded: &[(DrvHash, Option<Vec<String>>)],
        build_id: Uuid,
        removed_retriable: Vec<(DrvHash, DerivationState)>,
        displaced_scrubbed_edges: Vec<(DrvHash, HashSet<DrvHash>)>,
    ) {
        // Remove newly-inserted edges
        for (parent, child) in new_edges {
            if let Some(children) = self.children.get_mut(parent) {
                children.remove(child);
                if children.is_empty() {
                    self.children.remove(parent);
                }
            }
            if let Some(parents) = self.parents.get_mut(child) {
                parents.remove(parent);
                if parents.is_empty() {
                    self.parents.remove(child);
                }
            }
        }

        // Hashes being restored below — their children[X]/parents[X]
        // entries are pre-existing and must NOT be scrubbed: the
        // same-definition resubmit-reset removes only nodes[X] (never
        // edges), while the displacement arms and the authority-takeover
        // reset remove nodes[X] plus the children direction, which is
        // re-attached from `displaced_scrubbed_edges` at the end of this
        // rollback. The new_edges loop above already reverted any edges
        // THIS merge added to them.
        // Owned (not borrowed) because the per-field restore loops at the
        // bottom also consult it, after `removed_retriable` is consumed.
        let restoring: HashSet<DrvHash> =
            removed_retriable.iter().map(|(h, _)| h.clone()).collect();

        // Remove newly-inserted nodes (and their path index entries)
        for hash in newly_inserted {
            if let Some(state) = self.nodes.remove(hash) {
                self.path_to_hash.remove(state.drv_path().as_str());
            }
            // Also clean up any edge entries keyed on this hash — but only
            // for truly-fresh nodes. For restored nodes (resubmit-reset or
            // displacement), these entries
            // are the prior node's pre-existing edges; scrubbing them would
            // leave dangling one-way edges (e.g. children[W]∋X survives,
            // parents[X]∋W gone → X completes but W never sees it).
            if !restoring.contains(hash) {
                self.children.remove(hash);
                self.parents.remove(hash);
            }
        }

        // Restore the nodes this merge destructively removed
        // (resubmit-reset or displacement). Runs AFTER newly_inserted
        // removal so the fresh
        // replacement (same hash key) is gone first.
        for (hash, state) in removed_retriable {
            self.path_to_hash
                .insert(state.drv_path().to_string(), hash.clone());
            self.nodes.insert(hash, state);
        }

        // r[impl sched.merge.displaced-edge-scrub+2]
        // Re-attach the dependency edges scrubbed by the displacement
        // arms and the authority-takeover reset, so a failed merge
        // restores the removed node together with its (possibly
        // squatter-attached) children edges — exactly the pre-merge DAG.
        // Runs after the node restore so both endpoints exist again.
        for (hash, children) in displaced_scrubbed_edges {
            for c in &children {
                self.parents
                    .entry(c.clone())
                    .or_default()
                    .insert(hash.clone());
            }
            self.children.entry(hash).or_default().extend(children);
        }

        // Remove build interest only from pre-existing nodes that gained it
        // during THIS merge. Nodes where build_id was already present from a
        // prior successful merge are left untouched.
        for hash in interest_added {
            if let Some(state) = self.nodes.get_mut(hash) {
                state.interested_builds.remove(&build_id);
            }
        }

        // Revert traceparent upgrades on pre-existing nodes. The prior
        // value was always "" (the upgrade only fires on `is_empty()`).
        for hash in traceparent_upgraded {
            if let Some(state) = self.nodes.get_mut(hash) {
                state.traceparent.clear();
            }
        }

        // Restore the pre-union wanted set of pre-existing nodes whose
        // wanted set this merge grew. Reverse order so that, when one
        // submission carries the same drv twice and both occurrences
        // grow the union (the node is recorded twice, the second prior
        // being the already-grown value), the first-captured (true
        // pre-merge) value is the one that sticks — mirroring the
        // `contributions_recorded` restore below.
        for (hash, prior) in wanted_grown.iter().rev() {
            // Entries for resubmit-reset hashes describe the discarded
            // fresh replacement; the wholesale restore above already
            // carries the exact pre-merge state.
            if restoring.contains(hash) {
                continue;
            }
            if let Some(state) = self.nodes.get_mut(hash) {
                state.wanted_output_names = prior.clone();
            }
        }

        // Undo the submitting build's per-build contribution on
        // pre-existing nodes where this merge recorded it: remove the
        // entry it inserted (`None` prior), or restore the pre-merge
        // value of an entry it grew. Reverse order so that, when the
        // same node is recorded twice in one merge (a submission
        // carrying the same drv twice), the oldest (true pre-merge)
        // prior is the one that sticks.
        for (hash, prior) in contributions_recorded.iter().rev() {
            // Entries for resubmit-reset hashes describe the discarded
            // fresh replacement; the wholesale restore above already
            // carries the exact pre-merge state.
            if restoring.contains(hash) {
                continue;
            }
            if let Some(state) = self.nodes.get_mut(hash) {
                match prior {
                    Some(w) => {
                        state.wanted_by_build.insert(build_id, w.clone());
                    }
                    None => {
                        state.wanted_by_build.remove(&build_id);
                    }
                }
            }
        }
    }

    // r[impl sched.state.transitions]
    /// Check whether all dependencies of a derivation are satisfied
    /// (output available). `Completed` and `Skipped` are both
    /// acceptable: `Skipped` means CA cutoff verified the output
    /// already exists in the store (see [`Self::cascade_cutoff`]'s
    /// verify closure). A dep in any other state — including other
    /// terminal states like `Poisoned`/`DependencyFailed`/`Cancelled`
    /// — means the output is NOT available; the caller should route
    /// through [`Self::any_dep_terminally_failed`] instead (cascades
    /// `DependencyFailed` rather than promoting to `Ready`).
    pub fn all_deps_completed(&self, drv_hash: &str) -> bool {
        let Some(children) = self.children.get(drv_hash) else {
            return true; // No dependencies
        };

        children.iter().all(|child_hash| {
            self.nodes.get(child_hash).is_some_and(|n| {
                matches!(
                    n.status(),
                    DerivationStatus::Completed | DerivationStatus::Skipped
                )
            })
        })
    }

    /// Classify `drv_hash`'s current child set as closure evidence (see
    /// [`ClosureEvidence`]): absent node → `Broken`; `closure_hole` set
    /// → `Broken`; no children → `Broken`; at least one child and all
    /// of them Completed/Skipped → `Vouched`; otherwise `Pending`.
    ///
    /// Every `topdown_pruned` decision site (the stamp gates in
    /// `validate_and_ingest` / `persist_merge_to_db`, the
    /// produced-children clear sites, and the dispatch-time / reap-time
    /// guards) judges the child set through this one classifier so no
    /// site can drift into trusting a removal-truncated child set. The
    /// hole input is set by [`Self::remove_build_interest_and_reap`],
    /// by the recovery-time stamp in `load_dag_from_rows` (an edge to
    /// an un-produced terminal child dropped at load), and by the
    /// poison-clear removals (admin `ClearPoison` and the poison-TTL
    /// sweep), and healed by the merge-time edge re-declaration.
    // r[impl sched.evidence.positive-witness]
    pub fn closure_evidence(&self, drv_hash: &str) -> ClosureEvidence {
        let Some(node) = self.nodes.get(drv_hash) else {
            return ClosureEvidence::Broken;
        };
        if node.closure_hole.is_holed() {
            return ClosureEvidence::Broken;
        }
        let Some(children) = self.children.get(drv_hash) else {
            return ClosureEvidence::Broken;
        };
        if children.is_empty() {
            return ClosureEvidence::Broken;
        }
        let all_produced = children.iter().all(|child_hash| {
            self.nodes.get(child_hash).is_some_and(|n| {
                matches!(
                    n.status(),
                    DerivationStatus::Completed | DerivationStatus::Skipped
                )
            })
        });
        if all_produced {
            ClosureEvidence::Vouched
        } else {
            ClosureEvidence::Pending
        }
    }

    /// Check whether any dependency is in a terminal failure state.
    ///
    /// Used during merge: when a newly inserted node depends on an
    /// already-`Poisoned`/`DependencyFailed` existing node, the new node can
    /// never complete. Without this check it would go to `Queued` and stay
    /// there forever (never Ready since dep != Completed, never cascaded
    /// since cascade only runs on *transition to* Poisoned).
    pub fn any_dep_terminally_failed(&self, drv_hash: &str) -> bool {
        let Some(children) = self.children.get(drv_hash) else {
            return false;
        };
        children.iter().any(|child_hash| {
            self.nodes.get(child_hash).is_some_and(|n| {
                matches!(
                    n.status(),
                    DerivationStatus::Poisoned
                        | DerivationStatus::DependencyFailed
                        | DerivationStatus::Cancelled
                )
            })
        })
    }

    // r[impl sched.state.transitions]
    /// Canonical 3-way revert/reset target for a node whose own status is
    /// being recomputed from its dependencies' CURRENT states. Mirrors
    /// [`Self::compute_initial_states`]: terminally-failed dep →
    /// `DependencyFailed`; all deps satisfied → `Ready`; otherwise
    /// `Queued`.
    ///
    /// Exists so callers can't repeat the 2-way `Ready|Queued` mistake
    /// that drops the `any_dep_terminally_failed` check — a node reverted
    /// to `Queued` with a `Poisoned` dep is stuck forever
    /// (`find_newly_ready` only fires on dep→Completed; `poison_and_
    /// cascade` only fires on dep *transitioning to* terminal-failure).
    /// See [`Self::any_dep_terminally_failed`].
    pub fn revert_target_for(&self, drv_hash: &str) -> DerivationStatus {
        if self.any_dep_terminally_failed(drv_hash) {
            DerivationStatus::DependencyFailed
        } else if self.all_deps_completed(drv_hash) {
            DerivationStatus::Ready
        } else {
            DerivationStatus::Queued
        }
    }

    /// Get all parent drv_hashes that depend on the given child.
    pub fn get_parents(&self, child_hash: &str) -> Vec<DrvHash> {
        self.parents
            .get(child_hash)
            .map(|p| p.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Kahn's topo-sort over `scope`, children-before-parents.
    ///
    /// In-degree of a node = number of its children that are also in
    /// `scope`. When all in-scope children have been emitted (in-degree
    /// 0), emit the node. Edges to nodes OUTSIDE `scope` are ignored —
    /// callers that need their state read it directly (they're already
    /// settled by the time `scope` is processed). Shared by
    /// [`crate::critical_path`] (priority flows up from children) and
    /// [`Self::compute_initial_states`] (DependencyFailed flows up from
    /// children). `merge()` guarantees `scope` is acyclic.
    pub(crate) fn kahn_topo(&self, scope: &HashSet<DrvHash>) -> Vec<DrvHash> {
        use std::collections::VecDeque;
        let mut in_degree: HashMap<DrvHash, usize> = scope
            .iter()
            .map(|h| {
                let d = self
                    .children
                    .get(h)
                    .map(|cs| cs.iter().filter(|c| scope.contains(*c)).count())
                    .unwrap_or(0);
                (h.clone(), d)
            })
            .collect();
        let mut queue: VecDeque<DrvHash> = in_degree
            .iter()
            .filter(|&(_, &d)| d == 0)
            .map(|(h, _)| h.clone())
            .collect();
        let mut order = Vec::with_capacity(scope.len());
        while let Some(hash) = queue.pop_front() {
            for parent in self.get_parents(&hash) {
                if let Some(d) = in_degree.get_mut(&parent) {
                    *d -= 1;
                    if *d == 0 {
                        queue.push_back(parent);
                    }
                }
            }
            order.push(hash);
        }
        order
    }

    /// Get all child drv_hashes that the given parent depends on.
    ///
    /// Mirror of `get_parents`. Critical-path computation walks DOWN
    /// (children), completion handling walks UP (parents). Both
    /// directions needed.
    pub fn get_children(&self, parent_hash: &str) -> Vec<DrvHash> {
        self.children
            .get(parent_hash)
            .map(|c| c.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Find all derivations that become ready (all deps completed) after a
    /// given derivation completes. Only returns derivations currently in Queued state.
    pub fn find_newly_ready(&self, completed_hash: &str) -> Vec<DrvHash> {
        let mut ready = Vec::new();

        for parent_hash in self.get_parents(completed_hash) {
            if let Some(node) = self.nodes.get(&parent_hash)
                && node.status() == DerivationStatus::Queued
                && self.all_deps_completed(&parent_hash)
            {
                ready.push(parent_hash);
            }
        }

        ready
    }

    // r[impl sched.ca.cutoff-propagate+2]
    /// Walk downstream from a CA-unchanged completion. Return derivations
    /// whose ONLY remaining incomplete dependency was the just-completed
    /// (or just-skipped) node — i.e., status is `Queued` and all deps are
    /// now terminal.
    ///
    /// Treats nodes in `provisional_skipped` as-if-terminal. Used by
    /// `speculative_cascade_reachable` (batch-verification prewalk +
    /// cascade transition walk): verification needs to speculate "if B
    /// WERE skipped, would C be eligible?" to batch the store RPC
    /// across all reachable candidates.
    ///
    /// Only `Queued` is checked (never touch `Running` —
    /// `r[sched.preempt.never-running]`). A `Ready` node has
    /// `all_deps_completed() == true`, which means its inputs were
    /// already fully built — nothing for CA cutoff to skip there, so
    /// excluding Ready is correct.
    ///
    /// The `is_terminal()` check technically accepts
    /// `Poisoned`/`DependencyFailed`/`Cancelled` deps, but in the
    /// single-threaded actor those states cascade DependencyFailed
    /// immediately (see `cascade_dependency_failure`), so a `Queued`
    /// node's terminal deps are in practice only `Completed` or
    /// `Skipped`.
    pub fn find_cutoff_eligible_speculative(
        &self,
        completed: &str,
        provisional_skipped: &HashSet<DrvHash>,
    ) -> Vec<DrvHash> {
        let mut eligible = Vec::new();
        for parent_hash in self.get_parents(completed) {
            let Some(state) = self.nodes.get(&parent_hash) else {
                continue;
            };
            // Queued only — never Running. Provisional-skipped nodes
            // are excluded too (they're being speculated AS skipped,
            // so they're "parents of" targets, not targets themselves).
            if state.status() != DerivationStatus::Queued
                || provisional_skipped.contains(&parent_hash)
            {
                continue;
            }
            // Eligible iff ALL deps now terminal (or provisionally
            // skipped). Other incomplete deps → not eligible.
            let all_terminal = self
                .children
                .get(&parent_hash)
                .map(|deps| {
                    deps.iter().all(|d| {
                        provisional_skipped.contains(d)
                            || self.nodes.get(d).is_some_and(|n| n.status().is_terminal())
                    })
                })
                .unwrap_or(true);
            if all_terminal {
                eligible.push(parent_hash);
            }
        }
        eligible
    }

    /// Generic BFS walk collecting all nodes reachable via
    /// `expand(current, visited)` from `trigger`, capped at `max_nodes`
    /// results. Pure, non-mutating — the caller decides what to do with
    /// the result (transition, store-verify, persist).
    ///
    /// Used by:
    /// - [`Self::cascade_cutoff`] — Queued→Skipped propagation
    /// - [`crate::actor::DagActor`]'s `verify_cutoff_candidates` —
    ///   over-approximate candidate collection for a batched
    ///   FindMissingPaths RPC
    ///
    /// The `expand` closure receives `(current, &visited_so_far)` so
    /// it can implement speculative-skipped semantics (see
    /// [`Self::find_cutoff_eligible_speculative`] — treats
    /// provisional-visited nodes as already-Skipped).
    ///
    /// Returns `(reachable, cap_hit)`. Deduplication via `visited`
    /// HashSet — diamond DAGs are safe.
    ///
    /// Associated fn (not `&self`) to avoid overlapping borrows when
    /// `expand` needs to call `&self` methods while the caller is
    /// `&mut self` (see [`Self::cascade_cutoff`]).
    pub fn speculative_cascade_reachable<F>(
        trigger: &DrvHash,
        max_nodes: usize,
        mut expand: F,
    ) -> (Vec<DrvHash>, bool)
    where
        F: FnMut(&DrvHash, &HashSet<DrvHash>) -> Vec<DrvHash>,
    {
        let mut reachable = Vec::new();
        let mut visited: HashSet<DrvHash> = HashSet::new();
        let mut frontier = vec![trigger.clone()];
        let mut cap_hit = false;
        'walk: while let Some(current) = frontier.pop() {
            for next in expand(&current, &visited) {
                if visited.insert(next.clone()) {
                    reachable.push(next.clone());
                    // Result-size cap (NOT pop-count): a single
                    // high-fanout `expand()` would otherwise blow past
                    // `max_nodes` before the next outer-loop check.
                    if reachable.len() >= max_nodes {
                        cap_hit = true;
                        break 'walk;
                    }
                    frontier.push(next);
                }
            }
        }
        (reachable, cap_hit)
    }

    // r[impl sched.ca.cutoff-propagate+2]
    /// Cascade CA-cutoff Skip transitions starting from a trigger node.
    ///
    /// Two-phase: (1) BFS-collect all verified-eligible downstream
    /// nodes via [`Self::speculative_cascade_reachable`], passing the
    /// visited set to [`Self::find_cutoff_eligible_speculative`] so
    /// already-collected nodes are treated as-if-Skipped for
    /// transitive eligibility (A unchanged → B would-skip → C depended
    /// only on B → C eligible). (2) Bulk-transition the collected set
    /// to `Skipped`. The `&mut self.nodes` second pass happens AFTER
    /// the `&self` walk returns — no borrow overlap.
    ///
    /// The `verify` closure gates against the self-match hazard
    /// (bughunt-mc196): `ca_output_unchanged` can be `true` for a
    /// FIRST-EVER build because PutPath inserts the content_index row
    /// before BuildComplete arrives, so ContentLookup matches the
    /// just-uploaded output. Without verification, downstream nodes
    /// would be Skipped even though their outputs have NEVER been
    /// built. The completion handler passes a closure that checks
    /// `expected_output_paths` exist in the store; tests pass `|_| true`
    /// for pure walk testing. Unverified nodes are filtered out of
    /// `expand`'s return → not added to visited → not treated
    /// as-if-Skipped → their parents stay ineligible (cascade halts).
    ///
    /// Result-capped at [`MAX_CASCADE_NODES`] — at most that many
    /// nodes are transitioned to `Skipped` per call. Returns
    /// `(skipped_hashes, cap_hit)`.
    pub fn cascade_cutoff(
        &mut self,
        trigger: &str,
        mut verify: impl FnMut(&DrvHash) -> bool,
    ) -> (Vec<DrvHash>, bool) {
        let Some(start) = self.canonical(trigger) else {
            return (Vec::new(), false);
        };
        let (candidates, cap_hit) =
            Self::speculative_cascade_reachable(&start, MAX_CASCADE_NODES, |current, visited| {
                self.find_cutoff_eligible_speculative(current, visited)
                    .into_iter()
                    .filter(&mut verify)
                    .collect()
            });
        let mut skipped = Vec::with_capacity(candidates.len());
        for hash in candidates {
            // Queued→Skipped. All candidates were Queued when
            // collected (find_cutoff_eligible_speculative only
            // returns Queued nodes); the HashSet dedup in the
            // walker guarantees each hash appears once.
            if let Some(state) = self.nodes.get_mut(&hash)
                && state.transition(DerivationStatus::Skipped).is_ok()
            {
                // Skipped is a completion through a non-substitution
                // path: it ends any substitution chain that left this
                // node parked Queued (a downgraded walk waiting on the
                // dep this cascade just resolved), so drop the
                // chain-scoped spent-forgiveness set — same as the
                // other completion sites.
                state.never_forgive_paths.clear();
                skipped.push(hash);
            }
        }
        (skipped, cap_hit)
    }

    /// Remove a build's interest from all its derivations.
    /// Returns derivation hashes that are now orphaned (no builds interested).
    pub fn remove_build_interest(&mut self, build_id: Uuid) -> Vec<DrvHash> {
        let mut orphaned = Vec::new();

        for (hash, state) in &mut self.nodes {
            state.interested_builds.remove(&build_id);
            // Contributions follow interest membership.
            state.wanted_by_build.remove(&build_id);
            if state.interested_builds.is_empty() && !state.status().is_terminal() {
                orphaned.push(hash.clone());
            }
        }

        orphaned
    }

    /// Remove a build's interest from all its derivations, and reap (delete)
    /// any nodes that are now orphaned (no builds interested) AND in a terminal
    /// state. Returns a [`ReapOutcome`]: the reaped nodes' `drv_path`s
    /// (captured BEFORE removal so the caller can discard their log buffers —
    /// `path_for_hash` would not resolve afterwards) plus the deduped
    /// surviving parents that just lost children to the reap, so the caller
    /// can re-evaluate them. The survivor collection lives here — NOT in
    /// [`Self::remove_node`] — because `remove_node` is shared with the
    /// poison-TTL sweep and admin ClearPoison, where the removal exists to
    /// reset the node for a fresh re-merge and must not trigger parent
    /// re-evaluation.
    ///
    /// Survivors that lost at least one UN-PRODUCED child (status not
    /// Completed/Skipped at reap time) additionally get the in-memory
    /// `closure_hole` breadcrumb set, and are reported back as
    /// [`ReapOutcome::holed_parents`]: their remaining DAG children no
    /// longer represent their pruned input closure, so the actor's
    /// children-keyed `topdown_pruned` verdicts must not trust the
    /// truncated set. The IN-MEMORY breadcrumb is set HERE (not in the
    /// leader-gated survivor hook) so a standby's DAG — which loses the
    /// children all the same — carries it too; the PG persistence of
    /// the breadcrumb (`migrations/064`) is leader-class and lives in
    /// the hook, fed by `holed_parents`. The poison-TTL sweep and admin
    /// ClearPoison perform the same capture-stamp-persist sequence at
    /// their own call sites when they delete a Poisoned (by definition
    /// un-produced) child via [`Self::remove_node`] — `remove_node`
    /// itself stays parent-agnostic. The one truncation still left
    /// un-stamped is the expired-at-load poison shape at recovery (see
    /// the field's doc).
    ///
    /// This prevents unbounded DAG growth for long-running schedulers.
    /// Non-terminal orphaned nodes are preserved (they may be mid-build for
    /// a different code path, though this shouldn't happen in practice).
    pub fn remove_build_interest_and_reap(&mut self, build_id: Uuid) -> ReapOutcome {
        let mut to_reap = Vec::new();

        for (hash, state) in &mut self.nodes {
            state.interested_builds.remove(&build_id);
            // Contributions follow interest membership.
            state.wanted_by_build.remove(&build_id);
            // Exclude Poisoned: recovered-poisoned nodes have
            // interested_builds=∅ from birth (from_poisoned_row at
            // state/derivation.rs) and are TTL-tracked — reaping them
            // would silently disable poison-TTL. The previous
            // `was_interested` guard achieved that exclusion as a side
            // effect, but it also defeated reap entirely for cancelled/
            // failed/timed-out builds: cancel_build_derivations strips
            // interest first, so was_interested was always false here.
            if state.interested_builds.is_empty()
                && state.status().is_terminal()
                && state.status() != DerivationStatus::Poisoned
            {
                let unproduced = !matches!(
                    state.status(),
                    DerivationStatus::Completed | DerivationStatus::Skipped
                );
                to_reap.push((hash.clone(), state.drv_path().to_string(), unproduced));
            }
        }

        let mut surviving_parents: BTreeSet<DrvHash> = BTreeSet::new();
        let mut holed_parents: std::collections::BTreeMap<DrvHash, Vec<DrvHash>> =
            std::collections::BTreeMap::new();
        let mut reaped_paths = Vec::with_capacity(to_reap.len());
        for (hash, path, unproduced) in to_reap {
            // Capture the parents BEFORE `remove_node` scrubs the edge maps —
            // afterwards neither the reaped hash nor its former parents are
            // recoverable from the DAG.
            if let Some(ps) = self.parents.get(&hash) {
                surviving_parents.extend(ps.iter().cloned());
                if unproduced {
                    for p in ps {
                        holed_parents
                            .entry(p.clone())
                            .or_default()
                            .push(hash.clone());
                    }
                }
            }
            self.remove_node(&hash);
            reaped_paths.push(path);
        }
        // Drop entries that were themselves reaped (or otherwise no longer
        // exist) so only true survivors are reported.
        surviving_parents.retain(|p| self.nodes.contains_key(p));
        holed_parents.retain(|p, _| self.nodes.contains_key(p));
        // Breadcrumb the survivors whose reaped children include an
        // un-produced one — recording WHICH children went missing
        // (round-15 C6c1): their child set is no longer representative
        // of their input closure, and the witness set is what a later
        // heal must positively cover (sched.evidence.positive-witness).
        for (parent, missing) in &holed_parents {
            if let Some(state) = self.nodes.get_mut(parent) {
                state.closure_hole.stamp(missing.iter().cloned());
            }
        }
        ReapOutcome {
            reaped_paths,
            surviving_parents: surviving_parents.into_iter().collect(),
            holed_parents: holed_parents.into_iter().collect(),
        }
    }

    /// Scrub the DEPENDENCY (children) direction of a node's edges:
    /// remove `children[hash]` and drop `hash` from `parents[c]` for each
    /// former child `c`. Returns the scrubbed child set so the caller can
    /// record it for rollback restoration.
    ///
    /// Used by the displacement arms of `merge()` and by the
    /// authority-takeover path of the resubmit-reset
    /// (`sched.merge.displaced-edge-scrub`): the replacing submission is
    /// a DIFFERENT definition, so the dependency edges earlier
    /// submissions attached to the hash must not constrain the fresh node
    /// — `compute_initial_states` would otherwise seed it
    /// `DependencyFailed` (or park it `Queued` forever) from a dependency
    /// set the replacing submission never declared. The PARENTS
    /// direction is deliberately preserved: nodes that depend on this
    /// hash still want its output, whichever definition produces it.
    /// Same-definition resubmit-resets keep their edge-preserving
    /// semantics (same definition ⇒ same dependency set).
    fn scrub_dependency_edges(&mut self, hash: &DrvHash) -> HashSet<DrvHash> {
        let children = self.children.remove(hash).unwrap_or_default();
        for c in &children {
            if let Some(ps) = self.parents.get_mut(c) {
                ps.remove(hash);
                if ps.is_empty() {
                    self.parents.remove(c);
                }
            }
        }
        children
    }

    /// Remove a single node and scrub all edge references to it.
    ///
    /// Used by poison-clear paths (admin ClearPoison, TTL expiry) so the
    /// next merge treats the derivation as newly-inserted: it receives full
    /// proto fields and flows through `compute_initial_states`. Resetting
    /// status in-place instead would leave the node stuck — `merge()` on a
    /// pre-existing non-retriable node only touches `interested_builds` +
    /// `traceparent`, and `compute_initial_states` only iterates
    /// `newly_inserted` — so it would sit in Created forever.
    pub fn remove_node(&mut self, hash: &DrvHash) {
        if let Some(state) = self.nodes.remove(hash) {
            self.path_to_hash.remove(state.drv_path().as_str());
        }
        // The bidirectional edge invariant (every entry in `children[P]`
        // is mirrored by `parents[C]∋P`, and vice versa) means
        // `children[hash]` is exactly the set of C with `parents[C]∋hash`,
        // and `parents[hash]` exactly the set of P with `children[P]∋hash`.
        // Scrub only those — O(degree), not O(N). The previous
        // `values_mut()` full scan made `remove_build_interest_and_reap`
        // O(K×N) for K reaped nodes (≈10¹⁰ ops on a 100k-node sole-build
        // completion → minutes of single-threaded actor stall).
        for c in self.children.remove(hash).into_iter().flatten() {
            if let Some(ps) = self.parents.get_mut(&c) {
                ps.remove(hash);
            }
        }
        for p in self.parents.remove(hash).into_iter().flatten() {
            if let Some(cs) = self.children.get_mut(&p) {
                cs.remove(hash);
            }
        }
    }

    /// Determine initial states for newly merged derivations.
    ///
    /// Derivations with no incomplete dependencies go to Queued, then
    /// immediately to Ready. Others stay in Created until they become Queued
    /// (when their dependencies complete).
    /// Derivations whose dependency is already Poisoned/DependencyFailed go
    /// directly to DependencyFailed (pre-poisoned detection).
    ///
    /// Returns lists of (drv_hash, new_status) transitions.
    // r[impl sched.merge.dep-failed-transitive]
    pub fn compute_initial_states(
        &self,
        newly_inserted: &HashSet<DrvHash>,
    ) -> Vec<(DrvHash, DerivationStatus)> {
        // Topo (children-first) so transitive DependencyFailed propagates
        // within this call. Iterating `newly_inserted` in arbitrary order
        // and reading the pre-call snapshot means for chain A→B→X
        // (X pre-Poisoned, A and B newly inserted): B sees X→DepFailed,
        // but A sees B=Created→Queued. Under keepGoing=true the only
        // cascade trigger is the runtime-poison epilogue, never reached
        // for merge-seeded DepFailed — A would stay Queued forever. The
        // `will_fail` set lets A see B's just-decided DepFailed.
        let mut transitions = Vec::with_capacity(newly_inserted.len());
        let mut will_fail: HashSet<DrvHash> = HashSet::new();

        for drv_hash in self.kahn_topo(newly_inserted) {
            let dep_failed_in_this_call = self
                .children
                .get(&drv_hash)
                .is_some_and(|cs| cs.iter().any(|c| will_fail.contains(c)));
            if self.all_deps_completed(&drv_hash) {
                // No deps or all deps already completed -> directly to ready
                // We go created -> queued -> ready
                transitions.push((drv_hash, DerivationStatus::Ready));
            } else if self.any_dep_terminally_failed(&drv_hash) || dep_failed_in_this_call {
                // A dep is already poisoned/failed (or just decided
                // DependencyFailed in THIS call). This node cannot
                // complete. Mark DependencyFailed so the build
                // terminates instead of hanging forever with this node
                // stuck in Queued.
                will_fail.insert(drv_hash.clone());
                transitions.push((drv_hash, DerivationStatus::DependencyFailed));
            } else {
                // Has incomplete deps -> queued (waiting for deps)
                transitions.push((drv_hash, DerivationStatus::Queued));
            }
        }

        transitions
    }
    // r[impl sched.dag.build-scoped-roots]
    /// Find all root derivations for a build.
    /// These are the top-level derivations the client actually wants built.
    ///
    /// A derivation is a root FOR THIS BUILD if no parent *interested
    /// in this build* depends on it. The global `parents` map includes
    /// parents from all merged builds — a derivation that's a root for
    /// build X may have a parent from build Y. Using the unscoped
    /// parent set incorrectly marks X's root as a non-root, stalling
    /// X's dispatch (bug_022).
    pub fn find_roots(&self, build_id: Uuid) -> Vec<DrvHash> {
        self.nodes
            .iter()
            .filter(|(_, state)| state.interested_builds.contains(&build_id))
            .filter(|(hash, _)| {
                // No parent interested in THIS build depends on it.
                !self.parents.get(hash.as_str()).is_some_and(|parents| {
                    parents.iter().any(|p| {
                        self.nodes
                            .get(p)
                            .is_some_and(|ps| ps.interested_builds.contains(&build_id))
                    })
                })
            })
            .map(|(hash, _)| hash.clone())
            .collect()
    }

    /// Compute summary counts for a build.
    ///
    /// Single pass over all DAG nodes. Besides the status-bucket
    /// counts, also collects:
    /// - `critpath_remaining`: max priority across non-terminal nodes.
    ///   Priority = est_duration + max(children priority), so the
    ///   build's root(s) hold the critical-path ETA. Taking the max
    ///   over ALL non-terminal nodes (not just `find_roots`) is
    ///   correct AND more robust: if X has a build-interested
    ///   parent P, then P.sched.priority ≥ X.sched.priority (P includes X in
    ///   its max-child), so the max is achieved at a node with no
    ///   build-interested parent anyway. This sidesteps the
    ///   `find_roots` global-vs-build-scoped-parent question.
    /// - `assigned_executors`: deduplicated WorkerIds with an
    ///   Assigned/Running derivation in this build. BTreeSet for
    ///   sorted iteration → deterministic proto wire order.
    pub fn build_summary(&self, build_id: Uuid) -> BuildSummary {
        let mut summary = BuildSummary::default();
        let mut workers: BTreeSet<String> = BTreeSet::new();

        for state in self.nodes.values() {
            if !state.interested_builds.contains(&build_id) {
                continue;
            }
            summary.total += 1;
            match state.status() {
                // Skipped counts as completed: output-equivalent (CA
                // cutoff means it would've produced byte-identical
                // output to what's already in the store). Build
                // accounting (check_build_completion) sees it as done.
                DerivationStatus::Completed | DerivationStatus::Skipped => summary.completed += 1,
                // Substituting counts as running (in-flight, output
                // not yet present locally) so progress reporting and
                // the gateway's "running/queued" counts stay accurate.
                DerivationStatus::Running
                | DerivationStatus::Assigned
                | DerivationStatus::Substituting => {
                    summary.running += 1;
                    // assigned_executor is Some exactly in these two
                    // states (cleared on every terminal transition +
                    // reset_to_ready). Defensive if_let anyway.
                    if let Some(w) = &state.assigned_executor {
                        workers.insert(w.to_string());
                    }
                }
                DerivationStatus::Failed
                | DerivationStatus::Poisoned
                | DerivationStatus::DependencyFailed
                // Cancelled counts as failed from the build-summary
                // perspective: the build didn't produce its output.
                // Distinct terminal reason but same "not done" bucket
                // for the total/completed/failed accounting the gateway
                // shows.
                | DerivationStatus::Cancelled => summary.failed += 1,
                DerivationStatus::Ready | DerivationStatus::Queued | DerivationStatus::Created => {
                    summary.queued += 1;
                }
            }
            // Critical-path: max priority across non-terminal. Terminal
            // nodes keep their stale priority (only ancestors are
            // recomputed on completion), so including them would
            // over-report.
            if !state.status().is_terminal() {
                summary.critpath_remaining = summary.critpath_remaining.max(state.sched.priority);
            }
        }

        summary.assigned_executors = workers.into_iter().collect();
        summary
    }
}

/// Summary counts for a build's derivations.
#[derive(Debug, Default, Clone)]
pub struct BuildSummary {
    pub total: u32,
    pub completed: u32,
    pub running: u32,
    pub failed: u32,
    pub queued: u32,
    /// Max `priority` across non-terminal nodes — the critical-path
    /// ETA in seconds. 0.0 when nothing's left (all terminal).
    pub critpath_remaining: f64,
    /// Deduplicated, sorted worker IDs currently assigned/running
    /// derivations in this build.
    pub assigned_executors: Vec<String>,
}

#[cfg(test)]
mod tests;
