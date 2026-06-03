//! Pure closure-evidence decision kernel.
//!
//! This crate is the dependency-free home of the closure-evidence
//! campaign's decision surface: the trust classification of a node's
//! current child-set projection ([`closure_evidence`] →
//! [`ClosureEvidence`]) together with its CBMC proof harnesses
//! (`#[cfg(kani)] mod proofs`), and the four-arm Unobtainable routing
//! ([`routing`]).
//!
//! ## Why a separate crate
//!
//! The classifier used to live in `rio-scheduler/src/dag/mod.rs`
//! (`DerivationDag::closure_evidence`) and the predicates in
//! `rio-scheduler/src/actor/merge.rs`. The logic is unchanged by the
//! move — `DerivationDag::closure_evidence` is now a thin projection
//! shim that gathers (node presence, the per-child produced-ness) out
//! of its node/edge maps and calls this kernel — but the verification
//! economics are not: a kani proof
//! harness's goto model closes over its host crate's artifact context,
//! and inside rio-scheduler that context carries the HashMap/HashSet
//! node storage, Arc-backed identifiers, and the crate's full reachable
//! code, which pushes every harness past a merge-gate CBMC budget. In
//! this crate the harnesses' call graph is exactly the kernel: no
//! dependencies, no hash maps, no I/O. The same tactic as
//! `rio-retry-kernel` (the retry campaign's kernel split) and
//! `rio-store/src/logs/kernel.rs`, applied to the closure-evidence
//! campaign's Phase-2 assurance deliverable. Keep it that way — this
//! crate must not grow dependencies.
//!
//! ## What the classifier is
//!
//! [`closure_evidence`] answers one question for one node: may this
//! node's child set be trusted as evidence about its dependency
//! closure? The scheduler's roots-only prune
//! (`sched.merge.substitute-topdown`) deliberately merges kept nodes
//! without their dependency closures; the merge-time pruned-origin
//! selection gate exempts a kept node from the `origin = 'pruned'`
//! materialization-job classification only on
//! [`ClosureEvidence::Vouched`]. Since bug_390 (bughunt wave) EVERY
//! production consumer — the merge-time gate, the consumption routing,
//! and the park re-evaluation — classifies over the scheduler's
//! DURABLE relation (`classify_durable_evidence{,_in_tx}`, which
//! reports its strict three-part criterion in this alphabet); the
//! in-memory child set is reap-truncatable and must never decide a
//! verdict. This fold is the alphabet's verified semantics — the
//! durable classifier mirrors its cell map cell-for-cell.
//!
//! ## Inputs are projections, not state
//!
//! The kernel never sees the DAG. The caller projects, per node:
//!
//! - `present`: the node exists in the projection (an absent node's
//!   evidence is vacuously Holed — there is nothing to vouch);
//! - `children`: `None` when the DAG has no child-set entry for the
//!   node, otherwise one `bool` per declared child edge — `true` iff
//!   that child is present in the DAG with a produced status
//!   (Completed/Skipped). A child edge whose node is missing from the
//!   DAG projects as `false` (un-produced), exactly as the original
//!   `is_some_and` lookup did.
//!
//! The child set is an `IntoIterator` rather than a slice so the
//! scheduler's projection stays allocation-free and keeps the original
//! short-circuit (`Iterator::all`): the kernel stops consuming children
//! at the first un-produced one. The proof harnesses instantiate it
//! with bounded arrays (the cfg(kani) bounded representation — concrete
//! structure, symbolic values), so the goto model carries plain index
//! loops and no iterator-adapter state beyond `core::slice::Iter`.
//!
//! ## What the proofs establish
//!
//! Over the full bounded input domain (every presence value × every
//! child set up to `PROOF_CHILD_BOUND` children):
//!
//! - the classifier's **exhaustive case analysis** — absent/no-entry →
//!   Holed; empty child set → ChildlessLeaf; all children produced →
//!   Vouched; otherwise Pending — exactly, totally, and panic-free
//!   (`check_classifier_exhaustive_case_analysis`);
//! - the **Vouched iff** — evidence is Vouched exactly when the node is
//!   present and has a non-empty all-produced child set
//!   (`check_vouched_iff_nonempty_all_produced`).

pub mod establish;
pub mod pull;
pub mod routing;

/// Trust classification of a node's child set as evidence about its
/// dependency closure — the judgment behind the merge-time
/// pruned-origin selection gate, the consumption routing, and the park
/// re-evaluation; the shape the scheduler's durable classifier
/// (`classify_durable_evidence`) reports its three-part criterion in.
/// Computed by [`closure_evidence`].
///
/// The structural-leaf-vs-pruned-root ambiguity (merged_bug_301): a
/// `ChildlessLeaf` cell cannot tell a genuine dep-less leaf from a
/// pruned ROOT whose closure was deliberately dropped — the two need
/// opposite dispositions (a leaf is from-source-viable; a pruned root
/// is doomed). Every consumer therefore pairs this cell with the job
/// ORIGIN conjunct (`origin != 'pruned'`); the conjunct is
/// load-bearing, never decorative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClosureEvidence {
    /// At least one child, every child produced (Completed/Skipped):
    /// the dependency closure is in the store, so a from-source
    /// dispatch is not doomed and the pruned-origin classification is
    /// not needed.
    Vouched,
    /// At least one child, but not every child is produced yet: the
    /// closure is buildable but not yet built (normal dep gating).
    Pending,
    /// A child set that is present but EMPTY: a structural leaf. The
    /// cell is from-source-viable for a non-pruned origin (a leaf has
    /// no closure to be missing) — but a structural leaf is
    /// indistinguishable here from a pruned ROOT whose closure was
    /// deliberately dropped, so the origin conjunct stays load-bearing
    /// at every consumer (merged_bug_301).
    ChildlessLeaf,
    /// Absent node, no child-set entry, or stale produced evidence
    /// (children produced but no live co-owning voucher — the
    /// previous-generation shape): the evidence affirmatively must NOT
    /// vouch for a from-source dispatch.
    Holed,
}

/// Classify a node's current child set as closure evidence (see
/// [`ClosureEvidence`]): absent node or no child-set entry → `Holed`;
/// an empty child set → `ChildlessLeaf`; at least one child and all of
/// them produced → `Vouched`; otherwise `Pending`.
///
/// `children` is the per-declared-child produced-ness projection:
/// `None` when the DAG has no child-set entry for the node, otherwise
/// one `bool` per child edge (`true` iff that child is present with a
/// produced status). The fold short-circuits at the first un-produced
/// child, exactly as the original `Iterator::all` did.
pub fn closure_evidence<I>(present: bool, children: Option<I>) -> ClosureEvidence
where
    I: IntoIterator<Item = bool>,
{
    if !present {
        return ClosureEvidence::Holed;
    }
    let Some(children) = children else {
        return ClosureEvidence::Holed;
    };

    // One pass, short-circuiting: `any_child` distinguishes the empty
    // child set (ChildlessLeaf) from a non-empty one; `all_produced` falls to
    // false at the first un-produced child and the loop stops there
    // (the remaining children cannot change the verdict).
    let mut any_child = false;
    let mut all_produced = true;
    for produced in children {
        any_child = true;
        if !produced {
            all_produced = false;
            break;
        }
    }
    if !any_child {
        return ClosureEvidence::ChildlessLeaf;
    }
    if all_produced {
        ClosureEvidence::Vouched
    } else {
        ClosureEvidence::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Convenience: classify over a slice of child produced-ness bits.
    fn classify(present: bool, children: Option<&[bool]>) -> ClosureEvidence {
        closure_evidence(present, children.map(|c| c.iter().copied()))
    }

    #[test]
    fn absent_node_is_holed() {
        assert_eq!(classify(false, None), ClosureEvidence::Holed);
        assert_eq!(classify(false, Some(&[true, true])), ClosureEvidence::Holed);
    }

    #[test]
    fn childless_node_cells() {
        // No child-set entry at all: holed (nothing is known).
        assert_eq!(classify(true, None), ClosureEvidence::Holed);
        // A present-but-EMPTY child set: a structural leaf.
        assert_eq!(classify(true, Some(&[])), ClosureEvidence::ChildlessLeaf);
    }

    #[test]
    fn all_produced_children_vouch() {
        assert_eq!(classify(true, Some(&[true])), ClosureEvidence::Vouched);
        assert_eq!(
            classify(true, Some(&[true, true, true])),
            ClosureEvidence::Vouched
        );
    }

    #[test]
    fn unproduced_child_is_pending() {
        assert_eq!(classify(true, Some(&[false])), ClosureEvidence::Pending);
        assert_eq!(
            classify(true, Some(&[true, false, true])),
            ClosureEvidence::Pending
        );
    }

    /// Differential pin against an independent restatement of the
    /// classifier (the documentation's if-chain over pre-computed
    /// predicates), exhaustively over every child set of up to 8
    /// children — the unit-test half of what the kani harnesses prove
    /// over symbolic bounded inputs.
    #[test]
    fn classifier_matches_case_analysis_exhaustively() {
        fn reference(present: bool, children: Option<&[bool]>) -> ClosureEvidence {
            if !present {
                return ClosureEvidence::Holed;
            }
            match children {
                None => ClosureEvidence::Holed,
                Some([]) => ClosureEvidence::ChildlessLeaf,
                Some(c) if c.iter().all(|&p| p) => ClosureEvidence::Vouched,
                Some(_) => ClosureEvidence::Pending,
            }
        }

        for present in [false, true] {
            // No child-set entry.
            assert_eq!(classify(present, None), reference(present, None));
            // Every child set up to 8 children.
            for n in 0..=8usize {
                for bits in 0..(1u32 << n) {
                    let children: Vec<bool> = (0..n).map(|i| bits & (1 << i) != 0).collect();
                    assert_eq!(
                        classify(present, Some(&children)),
                        reference(present, Some(&children)),
                        "present={present} children={children:?}"
                    );
                }
            }
        }
    }
}

#[cfg(kani)]
mod proofs {
    //! CBMC proof harnesses for the closure-evidence kernel.
    //!
    //! Domain bounds, stated once: child sets are bounded at
    //! [`PROOF_CHILD_BOUND`] children, each child's produced-ness a free
    //! symbolic bool, the child-set length symbolic in
    //! `0..=PROOF_CHILD_BOUND`, and the "no child-set entry" case (`None`)
    //! a free symbolic choice. Presence is a free symbolic bool. The
    //! bound is a
    //! solver budget, not a hidden precondition: the classifier folds the
    //! child set with a short-circuiting loop whose verdict is decided by
    //! (a) emptiness and (b) the position of the first un-produced child,
    //! so any counterexample over a longer child set has a witness within
    //! the bound (emptiness is length-0 and "first un-produced child
    //! exists" is witnessed at length ≤ 2).
    //!
    //! The tracey verify markers for these harnesses live at the
    //! `kani-rio-evidence-kernel` wiring point in nix/kani.nix, not here —
    //! same discipline as the VM-test subtests list.

    use super::*;

    /// Child-set bound for every harness: 4 covers empty, singleton, the
    /// first-unproduced-child witness, and a multi-child tail.
    pub const PROOF_CHILD_BOUND: usize = 4;

    /// One arbitrary bounded child projection: `None` (no child-set
    /// entry) or `Some` of up to [`PROOF_CHILD_BOUND`] symbolic
    /// produced-ness bits. Returned as a fixed array + length so
    /// harnesses can re-read the prefix when stating postconditions
    /// (the iterator handed to the kernel is constructed fresh from it
    /// each time).
    fn any_children() -> (bool, [bool; PROOF_CHILD_BOUND], usize) {
        let has_entry: bool = kani::any();
        let bits: [bool; PROOF_CHILD_BOUND] = kani::any();
        let n: usize = kani::any();
        kani::assume(n <= PROOF_CHILD_BOUND);
        (has_entry, bits, n)
    }

    /// Run the kernel classifier over the bounded projection.
    fn classify_bounded(
        present: bool,
        has_entry: bool,
        bits: &[bool; PROOF_CHILD_BOUND],
        n: usize,
    ) -> ClosureEvidence {
        let children = if has_entry {
            Some(bits[..n].iter().copied())
        } else {
            None
        };
        closure_evidence(present, children)
    }

    /// The classifier's exhaustive case analysis, over the full bounded
    /// domain: absent → Broken; childless (no entry or empty) →
    /// Broken; all children produced → Vouched; otherwise Pending. The
    /// four cases are mutually exclusive and jointly exhaustive, so
    /// this also proves the classifier total and panic-free over the
    /// domain.
    #[kani::proof]
    #[kani::unwind(6)]
    fn check_classifier_exhaustive_case_analysis() {
        let present: bool = kani::any();
        let (has_entry, bits, n) = any_children();

        let ev = classify_bounded(present, has_entry, &bits, n);

        if !present {
            assert_eq!(ev, ClosureEvidence::Holed, "absent node must be Holed");
        } else if !has_entry {
            assert_eq!(
                ev,
                ClosureEvidence::Holed,
                "no child-set entry must be Holed"
            );
        } else if n == 0 {
            assert_eq!(
                ev,
                ClosureEvidence::ChildlessLeaf,
                "an empty child set is the structural-leaf cell"
            );
        } else {
            let mut all_produced = true;
            let mut i = 0;
            while i < n {
                if !bits[i] {
                    all_produced = false;
                }
                i += 1;
            }
            if all_produced {
                assert_eq!(
                    ev,
                    ClosureEvidence::Vouched,
                    "non-empty all-produced child set must be Vouched"
                );
            } else {
                assert_eq!(
                    ev,
                    ClosureEvidence::Pending,
                    "non-empty not-all-produced child set must be Pending"
                );
            }
        }
    }

    /// The Vouched iff: evidence is Vouched exactly when the node is
    /// present and has a non-empty all-produced child set — the
    /// criterion the pruned-origin exemption is keyed on. Childless
    /// child sets never vouch (the brokenNeverVouches direction), and
    /// produced non-empty child sets always do (the exemption is not
    /// vacuously withheld).
    #[kani::proof]
    #[kani::unwind(6)]
    fn check_vouched_iff_nonempty_all_produced() {
        let present: bool = kani::any();
        let (has_entry, bits, n) = any_children();

        let ev = classify_bounded(present, has_entry, &bits, n);

        let mut all_produced = true;
        let mut i = 0;
        while i < n {
            if !bits[i] {
                all_produced = false;
            }
            i += 1;
        }
        let should_vouch = present && has_entry && n > 0 && all_produced;

        assert_eq!(ev == ClosureEvidence::Vouched, should_vouch);
    }
}
