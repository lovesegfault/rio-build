//! Static executor/derivation eligibility plus the approximate input
//! closure shared by the pull mint and the GC live-pin path.
//!
//! The stream-era worker-selection half (`best_executor()`, the
//! hard-filter/warm-gate two-pass and the per-clause rejection
//! diagnostic) was deleted with the placement layer; the pull protocol
//! has no scheduler-side placement decision (the controller's spawn
//! gate owns source eligibility per AD2).

use rio_proto::types::ExecutorKind;

use crate::dag::DerivationDag;
use crate::state::{DerivationState, DrvHash, ExecutorState};

/// Static eligibility: would `w` be a candidate for `drv` ignoring
/// per-tick dynamic state (capacity / draining / degraded)?
///
/// Sole remaining consumer is the completion-time fleet-exhaust
/// snapshot (E1's fleet arm feeding `retry_policy::placeable`), which
/// reads the stream-era `executors` map — empty on a pull-mode fleet,
/// so `placeable()` answers `NoEligibleWorkers` (never a poison) and
/// the AD2 spawn-gate exhaustion check (`NoEligibleSource`) is the
/// production fleet-exhaust path. Retires with the executors map.
pub fn statically_eligible(w: &ExecutorState, drv: &DerivationState) -> bool {
    drv.is_fixed_output == (w.kind == ExecutorKind::Fetcher)
        && w.is_registered()
        && w.systems.iter().any(|s| s == &drv.system)
        // §13e + r35: read the EFFECTIVE feature set, not the raw
        // declaration, so a misconfigured FOD is not mis-classified as
        // fleet-exhausted.
        && drv
            .effective_features()
            .as_slice()
            .iter()
            .all(|f| w.supported_features.contains(f))
}

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

#[cfg(test)]
mod tests {
    use super::*;
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
}
