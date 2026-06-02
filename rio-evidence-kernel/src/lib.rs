//! Pure closure-evidence decision kernel.
//!
//! This crate is the dependency-free home of the closure-evidence
//! campaign's decision surface: the trust classification of a node's
//! current DAG child set ([`closure_evidence`] → [`ClosureEvidence`])
//! and the predicate layered on it ([`closure_vouched`]), together
//! with their CBMC proof harnesses (`#[cfg(kani)] mod proofs`).
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
//! node's *current* child set be trusted as evidence about its
//! dependency closure? The scheduler's roots-only prune
//! (`sched.merge.substitute-topdown`) deliberately merges kept nodes
//! without their dependency closures; the merge-time pruned-origin
//! selection gate exempts a kept node from the `origin = 'pruned'`
//! materialization-job classification only on
//! [`ClosureEvidence::Vouched`] — at least one child, every one of
//! them produced; that judgment is [`closure_vouched`]. (The
//! settlement-time judgments classify over the scheduler's durable
//! relation instead — `classify_durable_evidence`, the strict
//! three-part criterion — so this in-memory classifier's only consumer
//! is the merge-time gate. The walk-era mark/breadcrumb inputs and the
//! `must_substitute` predicate died with the evidence columns.)
//!
//! ## Inputs are projections, not state
//!
//! The kernel never sees the DAG. The caller projects, per node:
//!
//! - `present`: the node exists in the DAG (an absent node's evidence
//!   is vacuously Broken — there is nothing to vouch);
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
//! - the classifier's **exhaustive case analysis** — absent → Broken;
//!   childless (no entry or empty) → Broken; all children produced →
//!   Vouched; otherwise Pending — exactly, totally, and panic-free
//!   (`check_classifier_exhaustive_case_analysis`);
//! - the **Vouched iff** — evidence is Vouched exactly when the node is
//!   present and has a non-empty all-produced child set
//!   (`check_vouched_iff_nonempty_all_produced`);
//! - the [`closure_vouched`] **function contract** (`#[kani::ensures]`,
//!   verified by a `proof_for_contract` harness over the full evidence
//!   alphabet).

pub mod pull;

/// Trust classification of a node's current DAG child set as evidence
/// about its dependency closure — the judgment behind the merge-time
/// pruned-origin selection gate, and the shape the scheduler's durable
/// classifier (`classify_durable_evidence`) reports its three-part
/// criterion in. Computed by [`closure_evidence`].
///
/// `Broken` means "the current child set must NOT vouch for a
/// from-source dispatch": the node is absent or has no children at all
/// (a fired prune dropped its closure from the submission, or every
/// child was reaped).
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
    /// Absent or childless: the child set must not vouch for a
    /// from-source dispatch.
    Broken,
}

/// Classify a node's current child set as closure evidence (see
/// [`ClosureEvidence`]): absent node → `Broken`; no child-set entry or
/// no children → `Broken`; at least one child and all of them produced
/// → `Vouched`; otherwise `Pending`.
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
        return ClosureEvidence::Broken;
    }
    let Some(children) = children else {
        return ClosureEvidence::Broken;
    };

    // One pass, short-circuiting: `any_child` distinguishes the empty
    // child set (Broken) from a non-empty one; `all_produced` falls to
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
        return ClosureEvidence::Broken;
    }
    if all_produced {
        ClosureEvidence::Vouched
    } else {
        ClosureEvidence::Pending
    }
}

/// True when the evidence is [`ClosureEvidence::Vouched`]: at least one
/// child, every one of them already produced — the only children shape
/// that says the node's dependency closure exists in the store, so a
/// from-source dispatch is not doomed.
///
/// Sole consumer: the merge-time pruned-origin selection gate (a kept
/// closure-dropped node is exempted from the `origin = 'pruned'`
/// classification only when its closure is vouched).
#[cfg_attr(
    kani,
    kani::ensures(|verdict: &bool| *verdict == (evidence == ClosureEvidence::Vouched))
)]
pub fn closure_vouched(evidence: ClosureEvidence) -> bool {
    evidence == ClosureEvidence::Vouched
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Convenience: classify over a slice of child produced-ness bits.
    fn classify(present: bool, children: Option<&[bool]>) -> ClosureEvidence {
        closure_evidence(present, children.map(|c| c.iter().copied()))
    }

    #[test]
    fn absent_node_is_broken() {
        assert_eq!(classify(false, None), ClosureEvidence::Broken);
        assert_eq!(
            classify(false, Some(&[true, true])),
            ClosureEvidence::Broken
        );
    }

    #[test]
    fn childless_node_is_broken() {
        assert_eq!(classify(true, None), ClosureEvidence::Broken);
        assert_eq!(classify(true, Some(&[])), ClosureEvidence::Broken);
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

    #[test]
    fn closure_vouched_only_on_vouched() {
        assert!(closure_vouched(ClosureEvidence::Vouched));
        assert!(!closure_vouched(ClosureEvidence::Pending));
        assert!(!closure_vouched(ClosureEvidence::Broken));
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
                return ClosureEvidence::Broken;
            }
            match children {
                None => ClosureEvidence::Broken,
                Some([]) => ClosureEvidence::Broken,
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
            assert_eq!(ev, ClosureEvidence::Broken, "absent node must be Broken");
        } else if !has_entry || n == 0 {
            assert_eq!(ev, ClosureEvidence::Broken, "childless node must be Broken");
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

        assert_eq!(closure_vouched(ev), should_vouch);
        assert_eq!(ev == ClosureEvidence::Vouched, should_vouch);
    }

    /// `proof_for_contract` form of [`closure_vouched`]'s
    /// `#[kani::ensures]` clause, over the full evidence alphabet.
    #[kani::proof_for_contract(closure_vouched)]
    fn check_closure_vouched_contract() {
        let evidence = match kani::any::<u8>() {
            0 => ClosureEvidence::Vouched,
            1 => ClosureEvidence::Pending,
            _ => ClosureEvidence::Broken,
        };
        let _ = closure_vouched(evidence);
    }
}
