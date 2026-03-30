//! `GetCapacityManifest` RPC tests.
//!
//! Mirrors `admin/manifest.rs`. The bucketing math
//! (`Estimator::bucketed_estimate`) is thoroughly covered in
//! `estimator.rs` (7 tests); this file exercises the gRPC wiring.

use super::*;

/// Empty queue → empty response (not error). The controller's
/// manifest-diff reconciler handles empty manifests by using its
/// operator-configured floor for all capacity. Matches the
/// pre-T3 stub behavior — no regression for the controller.
// r[verify sched.admin.capacity-manifest]
#[tokio::test]
async fn test_get_capacity_manifest_empty_queue() -> anyhow::Result<()> {
    let (svc, _actor, _task, _db) = setup_svc_default().await;

    let resp = svc
        .get_capacity_manifest(Request::new(GetCapacityManifestRequest::default()))
        .await?
        .into_inner();

    assert!(
        resp.estimates.is_empty(),
        "no DAG → no ready queue → empty manifest"
    );
    Ok(())
}

// TODO(user-contribution): Plan P0501 T4 exit criterion —
//   "Ready queue with 3 derivations: 2 with history, 1 cold → response has 2 estimates"
//
// Needs BOTH actor state axes populated: DAG nodes at Ready status AND
// Estimator history entries. The sibling `compute_size_class_snapshot`
// at actor/tests/misc.rs:484 uses the DIRECT-DagActor pattern (no spawn,
// call compute methods directly) — but only tests configured cutoffs,
// not a populated DAG. No existing test in the codebase joins DAG Ready
// state × Estimator history.
//
// Three approaches:
//
// A. actor/tests/ direct-DagActor + test-only injection (recommended):
//    compute_capacity_manifest is pub(crate). Build DagActor directly,
//    inject DAG nodes + Estimator history via test-only setters (need
//    to add these — neither exists today). Tests the omit-cold filter
//    directly. ~30 lines + 2 new #[cfg(test)] setters on DagActor.
//
// B. Integration via MergeDag + build_history PG (highest fidelity):
//    INSERT into build_history, merge_dag() with 3 no-dep nodes (→ all
//    Ready), force Tick for estimator refresh, call the RPC. Tests the
//    real path end-to-end. ~50 lines; slower (PG + Tick wait).
//
// C. Skip — matches sibling's test bar (lowest cost):
//    The sibling r[verify sched.admin.sizeclass-status] tests RPC wiring
//    without a populated DAG. bucketed_estimate has 7 tests; the join
//    loop is a filter_map. Risk: OMISSION semantics (cold-start → None,
//    no-pname → None) are untested at the join — a refactor that breaks
//    status() comparison or pname extraction would pass estimator tests.
//
// The plan's exit criterion explicitly wants A or B. But C is what the
// codebase currently does for the sibling. Your call on whether
// capacity-manifest's omission behavior is load-bearing enough to
// break precedent.
