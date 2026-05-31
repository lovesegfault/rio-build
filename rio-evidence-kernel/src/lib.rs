//! Pure closure-evidence decision kernel for the scheduler's
//! `topdown_pruned` / `closure_hole` lifecycle.
//!
//! This crate is the dependency-free home of the closure-evidence
//! campaign's decision surface: the trust classification of a node's
//! current DAG child set ([`closure_evidence`] → [`ClosureEvidence`])
//! and the two predicates every stamp gate, clear site, and
//! dispatch-time guard key on it ([`must_substitute`] /
//! [`closure_vouched`]), together with their CBMC proof harnesses
//! (`#[cfg(kani)] mod proofs`).
//!
//! ## Why a separate crate
//!
//! The classifier used to live in `rio-scheduler/src/dag/mod.rs`
//! (`DerivationDag::closure_evidence`) and the predicates in
//! `rio-scheduler/src/actor/merge.rs`. The logic is unchanged by the
//! move — `DerivationDag::closure_evidence` is now a thin projection
//! shim that gathers (node presence, the `closure_hole` breadcrumb, the
//! per-child produced-ness) out of its node/edge maps and calls this
//! kernel — but the verification economics are not: a kani proof
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
//! without their dependency closures; the `topdown_pruned` mark records
//! that debt and the `closure_hole` breadcrumb records that an
//! un-produced child was later removed out from under a surviving
//! parent (`sched.evidence.closure-hole`). Both bits exist so that no
//! decision site ever reads a truncated child set as a vouched closure:
//!
//! - a **marked** node with **Broken** evidence must complete via
//!   substitution — a from-source dispatch is doomed (the worker
//!   ENOENTs on inputDrvs that were never merged); that conjunction is
//!   [`must_substitute`], the single predicate behind the dispatch-time
//!   carve-out, the pull-admission refusal, the downgrade re-spawn key,
//!   and the reap-hook fail-fast;
//! - a mark may be **cleared** (and a stamp **exempted**) only on
//!   [`ClosureEvidence::Vouched`] — at least one child, every one of
//!   them produced, and no closure hole; that judgment is
//!   [`closure_vouched`].
//!
//! ## Inputs are projections, not state
//!
//! The kernel never sees the DAG. The caller projects, per node:
//!
//! - `present`: the node exists in the DAG (an absent node's evidence
//!   is vacuously Broken — there is nothing to vouch);
//! - `closure_hole`: the breadcrumb bit on the node;
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
//! Over the full bounded input domain (every presence/hole/mark
//! combination × every child set up to `PROOF_CHILD_BOUND` children):
//!
//! - the classifier's **exhaustive case analysis** — absent → Broken;
//!   holed → Broken; childless (no entry or empty) → Broken; all
//!   children produced → Vouched; otherwise Pending — exactly, totally,
//!   and panic-free (`check_classifier_exhaustive_case_analysis`);
//! - **marked + Broken ⇒ must_substitute** — a marked node that is
//!   holed, childless, or absent is never dispatchable from source
//!   (`check_marked_broken_must_substitute`);
//! - **Vouched ⇒ ¬must_substitute** — vouched evidence never routes a
//!   node to forced substitution, marked or not
//!   (`check_vouched_never_must_substitute`);
//! - **unmarked ⇒ inert** — without the mark the evidence bits gate
//!   nothing, however broken they are
//!   (`check_unmarked_evidence_inert`);
//! - **hole OR-monotonicity** — setting the `closure_hole` bit can only
//!   move a node's evidence to Broken (never toward Vouched/Pending),
//!   never turns a must-substitute verdict off, and a holed child set
//!   never vouches however many of its surviving children are produced
//!   (`check_hole_breaks_and_never_vouches`);
//! - the **Vouched iff** — evidence is Vouched exactly when the node is
//!   present, un-holed, and has a non-empty all-produced child set
//!   (`check_vouched_iff_nonempty_all_produced`);
//! - the [`must_substitute`] / [`closure_vouched`] **function
//!   contracts** (`#[kani::ensures]`, verified by `proof_for_contract`
//!   harnesses over their full input domains).

/// Trust classification of a node's current DAG child set as evidence
/// about its dependency closure — the single judgment behind every
/// `topdown_pruned` stamp gate, clear site, and dispatch-time guard.
/// Computed by [`closure_evidence`].
///
/// `Broken` means "the current child set must NOT vouch for a
/// from-source dispatch": the node is absent, has no children at all
/// (a fired prune dropped its closure from the submission, or every
/// child was reaped), or carries the `closure_hole` breadcrumb — an
/// un-produced child was removed out from under it, by the
/// terminal-build reap, by a poison-clear removal (admin ClearPoison or
/// the poison-TTL sweep), or by leader-failover recovery dropping the
/// edge to one — so whatever children survive are a truncated view of
/// its input closure.
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

// r[impl sched.evidence.closure-hole]
/// Classify a node's current child set as closure evidence (see
/// [`ClosureEvidence`]): absent node → `Broken`; `closure_hole` set →
/// `Broken`; no child-set entry or no children → `Broken`; at least one
/// child and all of them produced → `Vouched`; otherwise `Pending`.
///
/// `children` is the per-declared-child produced-ness projection:
/// `None` when the DAG has no child-set entry for the node, otherwise
/// one `bool` per child edge (`true` iff that child is present with a
/// produced status). The fold short-circuits at the first un-produced
/// child, exactly as the original `Iterator::all` did.
///
/// Every `topdown_pruned` decision site judges the child set through
/// this one classifier so no site can drift into trusting a
/// removal-truncated child set: the closure-hole rule's "MUST classify
/// as Broken closure evidence … however many of its surviving children
/// are produced" clause is the `closure_hole` early return, proven by
/// `check_hole_breaks_and_never_vouches` in `mod proofs`.
pub fn closure_evidence<I>(
    present: bool,
    closure_hole: bool,
    children: Option<I>,
) -> ClosureEvidence
where
    I: IntoIterator<Item = bool>,
{
    if !present {
        return ClosureEvidence::Broken;
    }
    if closure_hole {
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

// r[impl sched.merge.substitute-topdown+12]
/// True when the node carries the `topdown_pruned` mark AND its closure
/// evidence is [`ClosureEvidence::Broken`] (absent, childless, or
/// closure-holed): its dependency closure was dropped from the
/// submission and the current child set cannot vouch for it, so the
/// node must complete via substitution — a from-source dispatch is
/// doomed (the worker ENOENTs on inputDrvs that were never merged).
///
/// This is the single predicate behind the dispatch-time carve-out and
/// fail-fast arms, the pull-admission refusal in `admit_pull`, the
/// downgrade re-spawn key in `handle_substitute_complete`, and the
/// reap-hook fail-fast in `handle_cleanup_terminal_build` — a
/// closure-holed survivor is treated exactly like a childless node at
/// every guard. Unmarked nodes are never affected, whatever their
/// evidence (`sched.evidence.closure-hole`'s inert-on-unmarked clause,
/// proven by `check_unmarked_evidence_inert`).
#[cfg_attr(
    kani,
    kani::ensures(|verdict: &bool| *verdict
        == (topdown_pruned && evidence == ClosureEvidence::Broken))
)]
pub fn must_substitute(topdown_pruned: bool, evidence: ClosureEvidence) -> bool {
    topdown_pruned && evidence == ClosureEvidence::Broken
}

/// True when the evidence is [`ClosureEvidence::Vouched`]: at least one
/// child, every one of them already produced, and no `closure_hole`
/// breadcrumb — the only children shape that says the node's dependency
/// closure exists in the store, so a from-source dispatch is not doomed.
///
/// Used by the stamp gates (a kept closure-dropped node is exempted
/// from the `topdown_pruned` stamp only when its closure is vouched)
/// and by the clear sites (the post-reconciliation pass, the
/// completion-time clear, and the lazy clear in
/// `handle_substitute_complete`), so the stamps and the clears always
/// judge the same criterion.
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
    fn classify(present: bool, hole: bool, children: Option<&[bool]>) -> ClosureEvidence {
        closure_evidence(present, hole, children.map(|c| c.iter().copied()))
    }

    #[test]
    fn absent_node_is_broken() {
        assert_eq!(classify(false, false, None), ClosureEvidence::Broken);
        assert_eq!(
            classify(false, false, Some(&[true, true])),
            ClosureEvidence::Broken
        );
        assert_eq!(
            classify(false, true, Some(&[true])),
            ClosureEvidence::Broken
        );
    }

    #[test]
    fn holed_node_is_broken_regardless_of_children() {
        assert_eq!(classify(true, true, None), ClosureEvidence::Broken);
        assert_eq!(classify(true, true, Some(&[])), ClosureEvidence::Broken);
        assert_eq!(
            classify(true, true, Some(&[true, true, true])),
            ClosureEvidence::Broken,
            "produced survivors of a truncated child set must not vouch"
        );
        assert_eq!(
            classify(true, true, Some(&[false])),
            ClosureEvidence::Broken
        );
    }

    #[test]
    fn childless_node_is_broken() {
        assert_eq!(classify(true, false, None), ClosureEvidence::Broken);
        assert_eq!(classify(true, false, Some(&[])), ClosureEvidence::Broken);
    }

    #[test]
    fn all_produced_children_vouch() {
        assert_eq!(
            classify(true, false, Some(&[true])),
            ClosureEvidence::Vouched
        );
        assert_eq!(
            classify(true, false, Some(&[true, true, true])),
            ClosureEvidence::Vouched
        );
    }

    #[test]
    fn unproduced_child_is_pending() {
        assert_eq!(
            classify(true, false, Some(&[false])),
            ClosureEvidence::Pending
        );
        assert_eq!(
            classify(true, false, Some(&[true, false, true])),
            ClosureEvidence::Pending
        );
    }

    #[test]
    fn must_substitute_requires_mark_and_broken() {
        for ev in [
            ClosureEvidence::Vouched,
            ClosureEvidence::Pending,
            ClosureEvidence::Broken,
        ] {
            // Unmarked nodes are never forced to substitution.
            assert!(!must_substitute(false, ev));
        }
        assert!(must_substitute(true, ClosureEvidence::Broken));
        assert!(!must_substitute(true, ClosureEvidence::Vouched));
        assert!(!must_substitute(true, ClosureEvidence::Pending));
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
        fn reference(present: bool, hole: bool, children: Option<&[bool]>) -> ClosureEvidence {
            if !present || hole {
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
            for hole in [false, true] {
                // No child-set entry.
                assert_eq!(
                    classify(present, hole, None),
                    reference(present, hole, None)
                );
                // Every child set up to 8 children.
                for n in 0..=8usize {
                    for bits in 0..(1u32 << n) {
                        let children: Vec<bool> = (0..n).map(|i| bits & (1 << i) != 0).collect();
                        assert_eq!(
                            classify(present, hole, Some(&children)),
                            reference(present, hole, Some(&children)),
                            "present={present} hole={hole} children={children:?}"
                        );
                    }
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
    //! a free symbolic choice. Presence, the hole bit, and the
    //! `topdown_pruned` mark are free symbolic bools. The bound is a
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
        hole: bool,
        has_entry: bool,
        bits: &[bool; PROOF_CHILD_BOUND],
        n: usize,
    ) -> ClosureEvidence {
        let children = if has_entry {
            Some(bits[..n].iter().copied())
        } else {
            None
        };
        closure_evidence(present, hole, children)
    }

    /// The classifier's exhaustive case analysis, over the full bounded
    /// domain: absent → Broken; holed → Broken; childless (no entry or
    /// empty) → Broken; all children produced → Vouched; otherwise
    /// Pending. The five cases are mutually exclusive and jointly
    /// exhaustive, so this also proves the classifier total and
    /// panic-free over the domain.
    #[kani::proof]
    #[kani::unwind(6)]
    fn check_classifier_exhaustive_case_analysis() {
        let present: bool = kani::any();
        let hole: bool = kani::any();
        let (has_entry, bits, n) = any_children();

        let ev = classify_bounded(present, hole, has_entry, &bits, n);

        if !present {
            assert_eq!(ev, ClosureEvidence::Broken, "absent node must be Broken");
        } else if hole {
            assert_eq!(ev, ClosureEvidence::Broken, "holed node must be Broken");
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

    /// Marked + Broken ⇒ must_substitute, across every Broken-producing
    /// input shape: absent, holed (any children), childless. A marked
    /// node whose child set cannot vouch for its dropped closure is
    /// never dispatchable from source — the
    /// `sched.merge.substitute-topdown` "MUST NOT be dispatched as a
    /// from-source build" clause at the predicate level.
    #[kani::proof]
    #[kani::unwind(6)]
    fn check_marked_broken_must_substitute() {
        let present: bool = kani::any();
        let hole: bool = kani::any();
        let (has_entry, bits, n) = any_children();

        let ev = classify_bounded(present, hole, has_entry, &bits, n);

        // Whenever the classifier judges Broken, the marked node must
        // substitute…
        if ev == ClosureEvidence::Broken {
            assert!(must_substitute(true, ev));
        }
        // …and the named Broken-producing shapes really do produce it:
        // absent, holed, and childless nodes.
        if !present || hole || !has_entry || n == 0 {
            assert_eq!(ev, ClosureEvidence::Broken);
            assert!(must_substitute(true, ev));
        }
    }

    /// Vouched ⇒ ¬must_substitute, for any mark state: vouched evidence
    /// never routes a node to forced substitution, and a marked node
    /// with a vouched child set is dispatchable (the mark is cleared on
    /// exactly this evidence, never fail-fasted on it).
    #[kani::proof]
    #[kani::unwind(6)]
    fn check_vouched_never_must_substitute() {
        let marked: bool = kani::any();

        // Direct form over the evidence alphabet.
        assert!(!must_substitute(marked, ClosureEvidence::Vouched));
        assert!(!must_substitute(marked, ClosureEvidence::Pending));

        // And through the classifier: any input that classifies Vouched
        // (or Pending) is not must-substitute.
        let present: bool = kani::any();
        let hole: bool = kani::any();
        let (has_entry, bits, n) = any_children();
        let ev = classify_bounded(present, hole, has_entry, &bits, n);
        if ev != ClosureEvidence::Broken {
            assert!(!must_substitute(marked, ev));
        }
    }

    /// Unmarked ⇒ inert: without the `topdown_pruned` mark, no evidence
    /// state — however broken — produces a must_substitute verdict. The
    /// `sched.evidence.closure-hole` rule's "the breadcrumb MAY be set
    /// on unmarked parents, where it MUST stay inert for dispatch
    /// decisions (every consumer also requires the mark)" clause.
    #[kani::proof]
    #[kani::unwind(6)]
    fn check_unmarked_evidence_inert() {
        let present: bool = kani::any();
        let hole: bool = kani::any();
        let (has_entry, bits, n) = any_children();
        let ev = classify_bounded(present, hole, has_entry, &bits, n);
        assert!(
            !must_substitute(false, ev),
            "an unmarked node is never forced to substitution, whatever its evidence"
        );
    }

    /// The hole's OR-monotonicity and never-vouches clauses: setting the
    /// `closure_hole` bit forces Broken (it can only move evidence away
    /// from Vouched/Pending, never toward them), a holed child set never
    /// vouches however many of its surviving children are produced, and
    /// the bit never turns a must-substitute verdict OFF — the
    /// stale-true-is-safe asymmetry the breadcrumb's OR-on-conflict
    /// persistence relies on.
    #[kani::proof]
    #[kani::unwind(6)]
    fn check_hole_breaks_and_never_vouches() {
        let present: bool = kani::any();
        let marked: bool = kani::any();
        let (has_entry, bits, n) = any_children();

        let ev_without_hole = classify_bounded(present, false, has_entry, &bits, n);
        let ev_with_hole = classify_bounded(present, true, has_entry, &bits, n);

        // The holed classification is Broken for every input shape
        // (absent nodes are Broken on both sides).
        assert_eq!(
            ev_with_hole,
            ClosureEvidence::Broken,
            "a holed (or absent) node's evidence is always Broken"
        );

        // A holed child set never vouches, however many of its
        // surviving children are produced.
        assert!(!closure_vouched(ev_with_hole));

        // OR-monotonicity of the verdict: setting the hole bit never
        // un-sets must_substitute.
        if must_substitute(marked, ev_without_hole) {
            assert!(
                must_substitute(marked, ev_with_hole),
                "setting the closure hole must never make a must-substitute node dispatchable"
            );
        }
    }

    /// The Vouched iff: evidence is Vouched exactly when the node is
    /// present, un-holed, and has a non-empty all-produced child set —
    /// the criterion the mark clear and the stamp exemption are keyed
    /// on. Childless or holed child sets never vouch (the
    /// brokenNeverVouches direction), and produced non-empty child sets
    /// always do (the clear is not vacuously withheld).
    #[kani::proof]
    #[kani::unwind(6)]
    fn check_vouched_iff_nonempty_all_produced() {
        let present: bool = kani::any();
        let hole: bool = kani::any();
        let (has_entry, bits, n) = any_children();

        let ev = classify_bounded(present, hole, has_entry, &bits, n);

        let mut all_produced = true;
        let mut i = 0;
        while i < n {
            if !bits[i] {
                all_produced = false;
            }
            i += 1;
        }
        let should_vouch = present && !hole && has_entry && n > 0 && all_produced;

        assert_eq!(closure_vouched(ev), should_vouch);
        assert_eq!(ev == ClosureEvidence::Vouched, should_vouch);
    }

    /// `proof_for_contract` form of [`must_substitute`]'s
    /// `#[kani::ensures]` clause, over the full input domain (every
    /// mark × every evidence variant). Keeping the contract verified
    /// makes it available to a future `stub_verified` caller.
    #[kani::proof_for_contract(must_substitute)]
    fn check_must_substitute_contract() {
        let marked: bool = kani::any();
        let evidence = match kani::any::<u8>() {
            0 => ClosureEvidence::Vouched,
            1 => ClosureEvidence::Pending,
            _ => ClosureEvidence::Broken,
        };
        let _ = must_substitute(marked, evidence);
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
