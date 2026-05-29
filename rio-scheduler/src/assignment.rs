//! Approximate input closure shared by the pull mint and the GC
//! live-pin path.
//!
//! The stream-era worker-selection half (`best_executor()`, the
//! hard-filter/warm-gate two-pass and the per-clause rejection
//! diagnostic) was deleted with the placement layer, and
//! `statically_eligible` retired with the executors map: the pull
//! protocol has no scheduler-side placement decision and no in-memory
//! fleet to filter — the controller's spawn gate owns source
//! eligibility per AD2 and its `NoEligibleSource` report is the
//! fleet-exhaust path.

use crate::dag::DerivationDag;
use crate::state::DrvHash;

/// Approximate input closure: the derivation's DAG children's
/// expected output paths PLUS its own `inputSrcs` (already-built
/// store paths declared in the ATerm, not represented as DAG nodes).
///
/// This is what the derivation NEEDS as inputs — its dependencies'
/// outputs and direct sources. Not perfect (misses transitive
/// closure of `inputSrcs`), but covers the bulk of what the
/// worker's FUSE will actually fetch. For a shallow DAG (leaf drv
/// with substituted/cached deps) `inputSrcs` is the ONLY signal —
/// without it the prefetch hint is empty and the worker
/// serial-fetches every input on first `lstat()`.
///
/// Used by the pull mint and the assignment-time GC live-pin path
/// (`scheduler_live_pins`) to approximate what the build will read.
///
/// Cheap: DAG iteration only, no store RPCs, no ATerm parse (the
/// parse happened once at merge time → `DerivationState.input_srcs`).
/// For a derivation with 20 dependencies each with 2 outputs +
/// 30 `inputSrcs`: ~70 string clones, ~1μs.
pub(crate) fn approx_input_closure(dag: &DerivationDag, drv_hash: &DrvHash) -> Vec<String> {
    let from_children = dag
        .get_children(drv_hash)
        .into_iter()
        .filter_map(|child| dag.node(&child))
        .flat_map(|child| {
            // Prefer REALIZED output_paths (populated at completion time
            // from the worker's BuildResult.built_outputs) over
            // expected_output_paths (populated at merge time from the
            // proto). For a floating-CA child, expected_output_paths is
            // `[""]` (the path is unknown pre-build) but output_paths
            // has the actual realized path once the child completes.
            // For IA children, expected_output_paths is correct and
            // output_paths is empty until completion — fall through.
            if child.output_paths.is_empty() {
                child.expected_output_paths.iter()
            } else {
                child.output_paths.iter()
            }
        });
    let from_srcs = dag
        .node(drv_hash)
        .map(|s| s.input_srcs.iter())
        .into_iter()
        .flatten();
    // inputSrcs first: they're declared in the ATerm (exact), while
    // dag-children outputs are an approximation (may over-include
    // unused multi-output siblings).
    from_srcs
        .chain(from_children)
        // Filter empties: a floating-CA child that hasn't completed yet
        // has expected_output_paths=[""] and output_paths=[]. The ""
        // would be a no-op PrefetchHint entry; cleaner to drop it here.
        .filter(|p| !p.is_empty())
        .cloned()
        .collect()
}

/// Exact direct-input seed set for the attested input closure
/// (`WorkAssignment.input_closure` /
/// `AssignmentClaims.input_closure_digest`, the P0589 §6.3 server-side
/// refscan attestation).
///
/// Unlike [`approx_input_closure`] — a best-effort prefetch hint that
/// silently degrades (recovered nodes lose `drv_content`/`input_srcs`,
/// and recovery drops DAG edges to children that completed before the
/// restart) — the attested closure must NEVER be narrower than the
/// build's true input closure: the builder uses it as the
/// reference-scan candidate set and cannot widen it (the store checks
/// the digest), so an omitted path means references to it are silently
/// missing from the uploaded narinfo and GC can collect
/// still-referenced paths.
///
/// The seeds are therefore derived from the node's parsed derivation —
/// the ground truth for direct inputs: `inputSrcs` ∪ the outputs of
/// every `inputDrvs` entry, each entry resolved through the DAG.
/// Returns `None` whenever the exact set cannot be established:
///
///   - no / unparseable `drv_content` (recovery-loaded node, or the
///     gateway didn't inline the `.drv`),
///   - an `inputDrvs` entry has no DAG node (e.g. it completed before
///     a scheduler restart and was not re-loaded), or
///   - that node's output paths are not all known yet.
///
/// `None` → the dispatch site sends an empty closure/digest and the
/// builder falls back to its own drv-parsed closure BFS, which is
/// complete by construction. This keeps the invariant structural:
/// state the scheduler cannot prove complete degrades to "no
/// attestation", never to a silently narrower attestation — no
/// recovery-path bookkeeping to keep in sync.
// r[impl sched.dispatch.input-roots+2]
pub(crate) fn attested_input_seeds(dag: &DerivationDag, drv_hash: &DrvHash) -> Option<Vec<String>> {
    let node = dag.node(drv_hash)?;
    let drv = std::str::from_utf8(&node.drv_content)
        .ok()
        .and_then(|s| rio_nix::derivation::Derivation::parse(s).ok())?;

    let mut seeds: Vec<String> = drv.input_srcs().iter().cloned().collect();
    for input_drv_path in drv.input_drvs().keys() {
        let child = dag
            .hash_for_path(input_drv_path)
            .and_then(|h| dag.node(h))?;
        // Prefer realized output paths (covers floating-CA, whose
        // expected paths are "" pre-build); fall back to the
        // merge-time expected paths (IA / fixed-CA). Either list may
        // over-include sibling outputs the parent doesn't consume —
        // harmless for a refscan candidate set (a path only becomes a
        // recorded reference if its hash actually appears in the
        // output bytes). What is NOT allowed is an unknown output
        // path: any empty entry means the seed set might not cover a
        // consumed output → no attestation.
        let paths = if child.output_paths.is_empty() {
            &child.expected_output_paths
        } else {
            &child.output_paths
        };
        if paths.is_empty() || paths.iter().any(String::is_empty) {
            return None;
        }
        seeds.extend(paths.iter().cloned());
    }
    Some(seeds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::DerivationState;
    use rio_test_support::fixtures::make_derivation_node;

    /// Shallow DAG: leaf node (no DAG children) with `inputSrcs` —
    /// `approx_input_closure` must return the inputSrcs, not empty.
    /// This is the `nix-bench#hello-shallow` shape: deps substituted/
    /// cached so they're not DAG nodes, only listed in the ATerm.
    #[test]
    fn approx_input_closure_includes_input_srcs_for_leaf() {
        let mut dag = DerivationDag::new();
        let mut leaf =
            DerivationState::try_from_node(&make_derivation_node("leaf", "x86_64-linux").into())
                .unwrap();
        let src_a = rio_test_support::fixtures::test_store_path("gcc-13.2.0");
        let src_b = rio_test_support::fixtures::test_store_path("glibc-2.39");
        leaf.input_srcs = vec![src_a.clone(), src_b.clone()];
        dag.insert_recovered_node(leaf);

        let got = approx_input_closure(&dag, &"leaf".into());
        assert_eq!(got.len(), 2, "leaf with 2 inputSrcs → 2 prefetch paths");
        assert!(got.contains(&src_a));
        assert!(got.contains(&src_b));
    }

    /// Node with BOTH a DAG child and inputSrcs → union of child's
    /// outputs and own inputSrcs. Order is srcs-then-children:
    /// declared inputs (exact) come before dag-children outputs
    /// (approximation, may over-include).
    #[test]
    fn approx_input_closure_unions_children_and_srcs() {
        let mut dag = DerivationDag::new();
        let child_out = rio_test_support::fixtures::test_store_path("child-out");
        let mut child =
            DerivationState::try_from_node(&make_derivation_node("child", "x86_64-linux").into())
                .unwrap();
        child.expected_output_paths = vec![child_out.clone()];
        dag.insert_recovered_node(child);

        let src = rio_test_support::fixtures::test_store_path("source-tarball");
        let mut parent =
            DerivationState::try_from_node(&make_derivation_node("parent", "x86_64-linux").into())
                .unwrap();
        parent.input_srcs = vec![src.clone()];
        dag.insert_recovered_node(parent);
        dag.insert_recovered_edge("parent".into(), "child".into());

        let got = approx_input_closure(&dag, &"parent".into());
        assert_eq!(got, vec![src, child_out]);
    }

    /// Parent node whose `drv_content` is a real ATerm with one
    /// inputDrv (`child_drv_path`, output "out") and one inputSrc.
    fn make_attest_parent(child_drv_path: &str, src: &str) -> DerivationState {
        let parent_out = rio_test_support::fixtures::test_store_path("attest-parent-out");
        let aterm = format!(
            r#"Derive([("out","{parent_out}","","")],[("{child_drv_path}",["out"])],["{src}"],"x86_64-linux","/bin/sh",[],[("out","{parent_out}")])"#
        );
        let mut node = make_derivation_node("attest-parent", "x86_64-linux");
        node.drv_content = aterm.into_bytes();
        DerivationState::try_from_node(&node.into()).unwrap()
    }

    /// Happy path: parsed drv with a resolvable inputDrv child →
    /// seeds = inputSrcs ∪ the child's realized outputs.
    // r[verify sched.dispatch.input-roots+2]
    #[test]
    fn attested_seeds_resolve_parsed_drv_inputs() {
        let mut dag = DerivationDag::new();
        let child_drv_path = rio_test_support::fixtures::test_drv_path("attest-child");
        let child_out = rio_test_support::fixtures::test_store_path("attest-child-out");
        let mut child = DerivationState::try_from_node(
            &make_derivation_node("attest-child", "x86_64-linux").into(),
        )
        .unwrap();
        child.output_paths = vec![child_out.clone()];
        dag.insert_recovered_node(child);

        let src = rio_test_support::fixtures::test_store_path("attest-src");
        dag.insert_recovered_node(make_attest_parent(&child_drv_path, &src));
        dag.insert_recovered_edge("attest-parent".into(), "attest-child".into());

        let got = attested_input_seeds(&dag, &"attest-parent".into())
            .expect("parsed drv with resolvable inputs is attestable");
        assert!(got.contains(&src), "inputSrcs entry missing: {got:?}");
        assert!(
            got.contains(&child_out),
            "inputDrv child's realized output missing: {got:?}"
        );
    }

    /// Recovery shape: `from_recovery_row` clears `drv_content`, so the
    /// exact direct-input set cannot be established → no attestation,
    /// even though a DAG child with a known output exists (the
    /// approximation would have produced a non-empty — and possibly
    /// narrower-than-true — seed set).
    // r[verify sched.dispatch.input-roots+2]
    #[test]
    fn attested_seeds_none_without_drv_content() {
        let mut dag = DerivationDag::new();
        let mut child = DerivationState::try_from_node(
            &make_derivation_node("attest-child", "x86_64-linux").into(),
        )
        .unwrap();
        child.output_paths = vec![rio_test_support::fixtures::test_store_path(
            "attest-child-out",
        )];
        dag.insert_recovered_node(child);

        // No drv_content (recovered / not inlined).
        let parent = DerivationState::try_from_node(
            &make_derivation_node("attest-parent", "x86_64-linux").into(),
        )
        .unwrap();
        dag.insert_recovered_node(parent);
        dag.insert_recovered_edge("attest-parent".into(), "attest-child".into());

        assert!(
            attested_input_seeds(&dag, &"attest-parent".into()).is_none(),
            "no parsed .drv → must not attest"
        );
    }

    /// An inputDrv that is not in the DAG (e.g. it completed before a
    /// scheduler restart and was not re-loaded) → no attestation.
    // r[verify sched.dispatch.input-roots+2]
    #[test]
    fn attested_seeds_none_when_input_drv_unresolvable() {
        let mut dag = DerivationDag::new();
        let missing_child = rio_test_support::fixtures::test_drv_path("attest-gone-child");
        let src = rio_test_support::fixtures::test_store_path("attest-src");
        dag.insert_recovered_node(make_attest_parent(&missing_child, &src));

        assert!(
            attested_input_seeds(&dag, &"attest-parent".into()).is_none(),
            "inputDrv missing from DAG → must not attest"
        );
    }

    /// An inputDrv child whose output paths aren't known yet (floating-
    /// CA placeholder "" and no realized paths) → no attestation.
    // r[verify sched.dispatch.input-roots+2]
    #[test]
    fn attested_seeds_none_when_child_outputs_unknown() {
        let mut dag = DerivationDag::new();
        let child_drv_path = rio_test_support::fixtures::test_drv_path("attest-child");
        let mut child = DerivationState::try_from_node(
            &make_derivation_node("attest-child", "x86_64-linux").into(),
        )
        .unwrap();
        child.expected_output_paths = vec![String::new()];
        dag.insert_recovered_node(child);

        let src = rio_test_support::fixtures::test_store_path("attest-src");
        dag.insert_recovered_node(make_attest_parent(&child_drv_path, &src));
        dag.insert_recovered_edge("attest-parent".into(), "attest-child".into());

        assert!(
            attested_input_seeds(&dag, &"attest-parent".into()).is_none(),
            "unknown child output path → must not attest"
        );
    }
}
