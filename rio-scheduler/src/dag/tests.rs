use super::*;
use rio_proto::dag::{DerivationEdge, DerivationNode};
use rio_test_support::fixtures::{make_derivation_node as make_node, make_edge, test_drv_path};

/// Build a test node with an EXPLICIT `drv_path` (for deep-chain tests
/// that generate their own valid 32-char-hash paths).
fn make_node_with_path(drv_hash: &str, drv_path: &str, system: &str) -> DerivationNode {
    DerivationNode {
        drv_path: drv_path.to_string(),
        drv_hash: drv_hash.to_string(),
        ..make_node(drv_hash, system)
    }
}

/// Build a test edge from explicit full paths.
fn make_edge_with_paths(parent: &str, child: &str) -> DerivationEdge {
    DerivationEdge {
        parent_drv_path: parent.to_string(),
        child_drv_path: child.to_string(),
    }
}

#[test]
fn test_merge_empty_dag() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let build_id = Uuid::new_v4();
    let nodes = vec![make_node("hash1", "x86_64-linux")];
    let edges = vec![];

    let newly = dag.merge(build_id, &nodes, &edges, "")?.newly_inserted;
    assert_eq!(newly.len(), 1);
    assert!(dag.nodes.contains_key("hash1"));
    assert!(dag.nodes["hash1"].interested_builds.contains(&build_id));
    Ok(())
}

#[test]
fn test_merge_dedup() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let build1 = Uuid::new_v4();
    let build2 = Uuid::new_v4();
    let nodes = vec![make_node("hash1", "x86_64-linux")];

    let newly1 = dag.merge(build1, &nodes, &[], "")?.newly_inserted;
    assert_eq!(newly1.len(), 1);

    let result2 = dag.merge(build2, &nodes, &[], "")?;
    assert_eq!(result2.newly_inserted.len(), 0); // Already exists
    assert_eq!(result2.interest_added, vec!["hash1"]);

    let node = &dag.nodes["hash1"];
    assert!(node.interested_builds.contains(&build1));
    assert!(node.interested_builds.contains(&build2));
    Ok(())
}

#[test]
fn test_edges_and_deps() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let build_id = Uuid::new_v4();
    let nodes = vec![
        make_node("hashA", "x86_64-linux"),
        make_node("hashB", "x86_64-linux"),
        make_node("hashC", "x86_64-linux"),
    ];
    // A depends on B and C
    let edges = vec![make_edge("hashA", "hashB"), make_edge("hashA", "hashC")];

    dag.merge(build_id, &nodes, &edges, "")?;

    // A has deps, B and C don't
    assert!(!dag.all_deps_completed("hashA"));
    assert!(dag.all_deps_completed("hashB"));
    assert!(dag.all_deps_completed("hashC"));

    // Check parent/child relationships
    assert_eq!(dag.children["hashA"].len(), 2);
    assert!(dag.get_parents("hashB").iter().any(|h| h == "hashA"));
    assert!(dag.get_parents("hashC").iter().any(|h| h == "hashA"));
    Ok(())
}

#[test]
fn test_initial_states() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let build_id = Uuid::new_v4();
    let nodes = vec![
        make_node("hashA", "x86_64-linux"),
        make_node("hashB", "x86_64-linux"),
    ];
    let edges = vec![make_edge("hashA", "hashB")];

    let newly = dag.merge(build_id, &nodes, &edges, "")?.newly_inserted;
    let states = dag.compute_initial_states(&newly);

    // B has no deps -> Ready; A has dep on B -> Queued
    for (hash, status) in &states {
        if hash == "hashB" {
            assert_eq!(*status, DerivationStatus::Ready);
        } else if hash == "hashA" {
            assert_eq!(*status, DerivationStatus::Queued);
        }
    }
    Ok(())
}

/// Resubmitting a Cancelled derivation must reset it so it flows through
/// `compute_initial_states` and re-dispatches. Without the reset, the
/// resubmitted build hangs forever: merge adds interest but the node stays
/// terminal, and `compute_initial_states` only iterates `newly_inserted`.
///
/// Scenario: build1 times out (per-build timeout → `cancel_build_derivations`
/// → drv Cancelled, interest removed). Reap misses it (`was_interested`
/// guard in `remove_build_interest_and_reap` is false — interest already
/// gone). User resubmits as build2.
#[test]
fn test_merge_resets_cancelled_on_resubmit() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let build1 = Uuid::new_v4();
    let nodes = vec![make_node("hashR", "x86_64-linux")];

    // Build 1: merge, then simulate per-build-timeout cancel.
    dag.merge(build1, &nodes, &[], "")?;
    dag.nodes
        .get_mut("hashR")
        .expect("hashR")
        .set_status_for_test(DerivationStatus::Cancelled);
    // cancel_build_derivations removes interest BEFORE reap runs, so the
    // node sits in the DAG with interested_builds=∅ and status=Cancelled.
    dag.nodes
        .get_mut("hashR")
        .expect("hashR")
        .interested_builds
        .clear();

    // Build 2: resubmit same drv.
    let build2 = Uuid::new_v4();
    let result = dag.merge(build2, &nodes, &[], "")?;

    // The Cancelled node must be reset-on-resubmit: treated as newly
    // inserted so compute_initial_states picks it up.
    assert!(
        result.newly_inserted.contains("hashR"),
        "Cancelled node should be reset and appear in newly_inserted on resubmit"
    );
    let node = &dag.nodes["hashR"];
    assert_eq!(
        node.status(),
        DerivationStatus::Created,
        "reset node should be Created (fresh state), not Cancelled"
    );
    assert!(node.interested_builds.contains(&build2));

    // compute_initial_states should now drive it to Ready (no deps).
    let states = dag.compute_initial_states(&result.newly_inserted);
    assert_eq!(states.len(), 1);
    assert_eq!(states[0].0, "hashR");
    assert_eq!(
        states[0].1,
        DerivationStatus::Ready,
        "reset no-dep node should be Ready for dispatch"
    );
    Ok(())
}

/// Same reset-on-resubmit for `Failed` (non-terminal but stuck if no
/// retry driver). Also verifies prior `interested_builds` are preserved
/// so another stuck build benefits from the reset.
#[test]
fn test_merge_resets_failed_preserves_interest() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let build1 = Uuid::new_v4();
    let nodes = vec![make_node("hashF", "x86_64-linux")];

    dag.merge(build1, &nodes, &[], "")?;
    dag.nodes
        .get_mut("hashF")
        .expect("hashF")
        .set_status_for_test(DerivationStatus::Failed);
    // build1 still interested (Failed ≠ terminal, interest not removed).

    let build2 = Uuid::new_v4();
    let result = dag.merge(build2, &nodes, &[], "")?;

    assert!(result.newly_inserted.contains("hashF"));
    let node = &dag.nodes["hashF"];
    assert_eq!(node.status(), DerivationStatus::Created);
    // Both builds interested: build1 carried over from the removed
    // Failed node, build2 from this merge.
    assert!(
        node.interested_builds.contains(&build1),
        "prior interest must be preserved across reset"
    );
    assert!(node.interested_builds.contains(&build2));
    Ok(())
}

/// Negative: Poisoned must NOT be reset on resubmit. It has a 24h TTL for
/// a reason (failed on 3+ workers). `handle_merge_dag` handles this via
/// `first_dep_failed` → build fails fast with a clear error.
#[test]
fn test_merge_does_not_reset_poisoned() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let build1 = Uuid::new_v4();
    let nodes = vec![make_node("hashP", "x86_64-linux")];

    dag.merge(build1, &nodes, &[], "")?;
    dag.nodes
        .get_mut("hashP")
        .expect("hashP")
        .set_status_for_test(DerivationStatus::Poisoned);

    let build2 = Uuid::new_v4();
    let result = dag.merge(build2, &nodes, &[], "")?;

    // Poisoned stays poisoned — NOT in newly_inserted.
    assert!(
        !result.newly_inserted.contains("hashP"),
        "Poisoned must not be reset on resubmit (24h TTL is authoritative)"
    );
    assert_eq!(dag.nodes["hashP"].status(), DerivationStatus::Poisoned);
    Ok(())
}

/// Worker-side timeout scenario: `BuildResultStatus::TimedOut` →
/// `handle_timeout_failure` → child Cancelled, parent DependencyFailed
/// (cascade). Resubmit must reset BOTH so the retry actually dispatches.
///
/// `DependencyFailed` is retriable because it's a DERIVED state: reset
/// lets `compute_initial_states` re-evaluate `any_dep_terminally_failed`
/// fresh. Child reset → no longer terminally failed → parent goes Queued.
#[test]
fn test_merge_resets_timeout_cascade() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let build1 = Uuid::new_v4();
    let nodes = vec![
        make_node("parentT", "x86_64-linux"),
        make_node("childT", "x86_64-linux"),
    ];
    let edges = vec![make_edge("parentT", "childT")];

    dag.merge(build1, &nodes, &edges, "")?;

    // Simulate handle_timeout_failure: child → Cancelled, parent → DependencyFailed.
    dag.nodes
        .get_mut("childT")
        .expect("childT")
        .set_status_for_test(DerivationStatus::Cancelled);
    dag.nodes
        .get_mut("parentT")
        .expect("parentT")
        .set_status_for_test(DerivationStatus::DependencyFailed);

    // Resubmit.
    let build2 = Uuid::new_v4();
    let result = dag.merge(build2, &nodes, &edges, "")?;

    // Both reset → both in newly_inserted.
    assert!(
        result.newly_inserted.contains("childT"),
        "Cancelled child must reset on resubmit (worker-side TimedOut case)"
    );
    assert!(
        result.newly_inserted.contains("parentT"),
        "DependencyFailed parent must reset on resubmit (re-evaluate deps fresh)"
    );

    // Both now Created.
    assert_eq!(dag.nodes["childT"].status(), DerivationStatus::Created);
    assert_eq!(dag.nodes["parentT"].status(), DerivationStatus::Created);

    // compute_initial_states re-derives: child no-deps → Ready, parent → Queued.
    let states: HashMap<_, _> = dag
        .compute_initial_states(&result.newly_inserted)
        .into_iter()
        .collect();
    assert_eq!(states["childT"], DerivationStatus::Ready);
    assert_eq!(
        states["parentT"],
        DerivationStatus::Queued,
        "parent re-derived as Queued (dep no longer terminally failed)"
    );
    Ok(())
}

/// DependencyFailed reset is self-correcting when dep is STILL Poisoned:
/// `compute_initial_states` re-checks `any_dep_terminally_failed` and
/// puts the parent back in DependencyFailed. Same fast-fail, just via
/// the reset-then-reevaluate path instead of the pre-existing-node path.
#[test]
fn test_merge_resets_depfailed_but_rederives_if_dep_still_poisoned() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let build1 = Uuid::new_v4();
    let nodes = vec![
        make_node("parentD", "x86_64-linux"),
        make_node("childD", "x86_64-linux"),
    ];
    let edges = vec![make_edge("parentD", "childD")];

    dag.merge(build1, &nodes, &edges, "")?;
    dag.nodes
        .get_mut("childD")
        .expect("childD")
        .set_status_for_test(DerivationStatus::Poisoned);
    dag.nodes
        .get_mut("parentD")
        .expect("parentD")
        .set_status_for_test(DerivationStatus::DependencyFailed);

    let build2 = Uuid::new_v4();
    let result = dag.merge(build2, &nodes, &edges, "")?;

    // Parent reset (DependencyFailed is retriable), child NOT (Poisoned isn't).
    assert!(result.newly_inserted.contains("parentD"));
    assert!(!result.newly_inserted.contains("childD"));
    assert_eq!(dag.nodes["childD"].status(), DerivationStatus::Poisoned);

    // compute_initial_states re-derives parent as DependencyFailed (dep still poisoned).
    let states = dag.compute_initial_states(&result.newly_inserted);
    assert_eq!(states.len(), 1);
    assert_eq!(states[0].0, "parentD");
    assert_eq!(
        states[0].1,
        DerivationStatus::DependencyFailed,
        "parent re-derived as DependencyFailed — dep still Poisoned, same fast-fail outcome"
    );
    Ok(())
}

#[test]
fn test_initial_states_with_prepoisoned_dep() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let build1 = Uuid::new_v4();

    // Build 1: just the leaf.
    let leaf_nodes = vec![make_node("leafP", "x86_64-linux")];
    dag.merge(build1, &leaf_nodes, &[], "")?;

    // Poison it.
    dag.nodes
        .get_mut("leafP")
        .expect("leafP")
        .set_status_for_test(DerivationStatus::Poisoned);

    assert!(!dag.any_dep_terminally_failed("leafP")); // no deps

    // Build 2: parent depending on the poisoned leaf.
    let build2 = Uuid::new_v4();
    let parent_nodes = vec![
        make_node("parentP", "x86_64-linux"),
        make_node("leafP", "x86_64-linux"),
    ];
    let edges = vec![make_edge("parentP", "leafP")];
    let newly = dag.merge(build2, &parent_nodes, &edges, "")?.newly_inserted;

    // Only parentP is newly inserted (leafP already existed).
    assert_eq!(newly, HashSet::from(["parentP".into()]));
    assert!(dag.any_dep_terminally_failed("parentP"));

    // compute_initial_states should return DependencyFailed for parentP.
    let states = dag.compute_initial_states(&newly);
    assert_eq!(states.len(), 1);
    assert_eq!(states[0].0, "parentP");
    assert_eq!(
        states[0].1,
        DerivationStatus::DependencyFailed,
        "node with pre-poisoned dep should be DependencyFailed, not Queued"
    );
    Ok(())
}

#[test]
fn test_find_newly_ready() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let build_id = Uuid::new_v4();
    let nodes = vec![
        make_node("hashA", "x86_64-linux"),
        make_node("hashB", "x86_64-linux"),
    ];
    let edges = vec![make_edge("hashA", "hashB")];

    dag.merge(build_id, &nodes, &edges, "")?;

    // Set B to completed, A to queued
    dag.nodes
        .get_mut("hashB")
        .expect("hashB")
        .set_status_for_test(DerivationStatus::Completed);
    dag.nodes
        .get_mut("hashA")
        .expect("hashA")
        .set_status_for_test(DerivationStatus::Queued);

    let ready = dag.find_newly_ready("hashB");
    assert_eq!(ready, vec!["hashA".to_string()]);
    Ok(())
}

// -----------------------------------------------------------------------
// Cycle detection
// -----------------------------------------------------------------------

/// A cyclic DAG should be rejected, with all newly-inserted nodes rolled back.
#[test]
fn test_merge_rejects_cycle() {
    let mut dag = DerivationDag::new();
    let build_id = Uuid::new_v4();

    // A depends on B, B depends on A — cycle
    let nodes = vec![
        make_node("hashA", "x86_64-linux"),
        make_node("hashB", "x86_64-linux"),
    ];
    let edges = vec![
        make_edge("hashA", "hashB"),
        make_edge("hashB", "hashA"), // cycle!
    ];

    let result = dag.merge(build_id, &nodes, &edges, "");
    assert!(result.is_err(), "cyclic DAG should be rejected");
    assert_eq!(
        dag.nodes.len(),
        0,
        "no nodes should remain after cycle rollback"
    );
    assert_eq!(dag.children.len(), 0, "edges should be rolled back");
    assert_eq!(dag.parents.len(), 0, "edges should be rolled back");
}

/// An indirect cycle (A -> B -> C -> A) should also be detected.
#[test]
fn test_merge_rejects_indirect_cycle() {
    let mut dag = DerivationDag::new();
    let build_id = Uuid::new_v4();

    let nodes = vec![
        make_node("hashA", "x86_64-linux"),
        make_node("hashB", "x86_64-linux"),
        make_node("hashC", "x86_64-linux"),
    ];
    // A depends on B, B depends on C, C depends on A — indirect cycle
    let edges = vec![
        make_edge("hashA", "hashB"),
        make_edge("hashB", "hashC"),
        make_edge("hashC", "hashA"),
    ];

    let result = dag.merge(build_id, &nodes, &edges, "");
    assert!(result.is_err(), "indirect cycle should be rejected");
    assert_eq!(dag.nodes.len(), 0);
}

/// A valid DAG merged after a cycle-rejected attempt should succeed.
#[test]
fn test_merge_after_cycle_rollback() {
    let mut dag = DerivationDag::new();
    let build_id = Uuid::new_v4();

    // First: try to insert a cycle (should fail and rollback)
    let cyclic_nodes = vec![
        make_node("hashA", "x86_64-linux"),
        make_node("hashB", "x86_64-linux"),
    ];
    let cyclic_edges = vec![make_edge("hashA", "hashB"), make_edge("hashB", "hashA")];
    assert!(
        dag.merge(build_id, &cyclic_nodes, &cyclic_edges, "")
            .is_err()
    );

    // Second: insert a valid DAG with the same nodes (should succeed)
    let valid_edges = vec![make_edge("hashA", "hashB")];
    let result = dag.merge(build_id, &cyclic_nodes, &valid_edges, "");
    assert!(
        result.is_ok(),
        "valid merge after rollback should succeed: {result:?}"
    );
    assert_eq!(dag.nodes.len(), 2);
}

/// A new edge between two PRE-EXISTING nodes (no new nodes inserted)
/// can create a cycle. The DFS must start from edge endpoints, not just
/// newly-inserted nodes.
#[test]
fn test_cycle_via_new_edge_between_existing_nodes() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let build1 = Uuid::new_v4();

    // Insert A and B separately with A->B edge.
    let nodes = vec![
        make_node("hashA", "x86_64-linux"),
        make_node("hashB", "x86_64-linux"),
    ];
    let initial_edges = vec![make_edge("hashA", "hashB")];
    dag.merge(build1, &nodes, &initial_edges, "")?;
    assert_eq!(dag.nodes.len(), 2);

    // Now merge the SAME nodes (no new inserts) with a B->A edge.
    // This creates a cycle via a new edge between two existing nodes.
    let build2 = Uuid::new_v4();
    let cycle_edge = vec![make_edge("hashB", "hashA")];
    let result = dag.merge(build2, &nodes, &cycle_edge, "");

    assert!(
        result.is_err(),
        "cycle via new edge between existing nodes should be detected"
    );
    // Rollback: the new edge should be gone, but the original A->B stays.
    assert!(
        dag.children
            .get("hashA")
            .is_some_and(|c| c.contains("hashB")),
        "original A->B edge should survive rollback"
    );
    assert!(
        !dag.children
            .get("hashB")
            .is_some_and(|c| c.contains("hashA")),
        "cycle-creating B->A edge should be rolled back"
    );
    Ok(())
}

/// When a build merges successfully, then later merges again with a cycle,
/// rollback must NOT clear the build's interest from nodes that already
/// had it from the prior successful merge.
#[test]
fn test_cycle_rollback_preserves_prior_interest() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let b1 = Uuid::new_v4();

    // Step 1: merge B1 with node A only — succeeds. A.interested = {B1}.
    let nodes_a = vec![make_node("hashA", "x86_64-linux")];
    dag.merge(b1, &nodes_a, &[], "")?;
    assert!(
        dag.nodes
            .get("hashA")
            .expect("hashA")
            .interested_builds
            .contains(&b1),
        "B1 interest in A should be set after successful merge"
    );

    // Step 2: merge B1 again with nodes {A, C} and cycle A->C->A — fails.
    // Regression guard: rollback must not clear B1 from A even though
    // B1 was already interested in A from step 1.
    let nodes_ac = vec![
        make_node("hashA", "x86_64-linux"),
        make_node("hashC", "x86_64-linux"),
    ];
    let cycle_edges = vec![make_edge("hashA", "hashC"), make_edge("hashC", "hashA")];
    let result = dag.merge(b1, &nodes_ac, &cycle_edges, "");
    assert!(result.is_err(), "cycle should be rejected");

    // Step 3: A should STILL have B1 interest (was present before the
    // failed merge). C should be gone entirely (was newly inserted).
    assert!(
        dag.nodes
            .get("hashA")
            .expect("hashA")
            .interested_builds
            .contains(&b1),
        "B1 interest in A from prior successful merge must survive rollback"
    );
    assert!(
        !dag.nodes.contains_key("hashC"),
        "newly-inserted C should be rolled back"
    );
    Ok(())
}

/// The path_to_hash reverse index must stay in sync with nodes across
/// merge, rollback, and reap operations.
#[test]
fn test_path_to_hash_consistency() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let b1 = Uuid::new_v4();
    let p_a = test_drv_path("hashA");
    let p_b = test_drv_path("hashB");
    let p_c = test_drv_path("hashC");

    // Merge: index should be populated.
    let nodes = vec![
        make_node("hashA", "x86_64-linux"),
        make_node("hashB", "x86_64-linux"),
    ];
    dag.merge(b1, &nodes, &[], "")?;
    assert_eq!(dag.hash_for_path(&p_a).map(|h| h.as_str()), Some("hashA"));
    assert_eq!(dag.hash_for_path(&p_b).map(|h| h.as_str()), Some("hashB"));
    assert_eq!(dag.hash_for_path("/nix/store/nonexistent.drv"), None);

    // Cycle rollback: newly-inserted node's path entry must be removed.
    let cycle_nodes = vec![
        make_node("hashA", "x86_64-linux"),
        make_node("hashC", "x86_64-linux"),
    ];
    let cycle_edges = vec![make_edge("hashA", "hashC"), make_edge("hashC", "hashA")];
    dag.merge(b1, &cycle_nodes, &cycle_edges, "").unwrap_err();
    assert_eq!(
        dag.hash_for_path(&p_c),
        None,
        "rollback must remove path index for newly-inserted node"
    );
    assert_eq!(
        dag.hash_for_path(&p_a).map(|h| h.as_str()),
        Some("hashA"),
        "rollback must preserve path index for pre-existing node"
    );

    // Reap: terminal orphaned node's path entry must be removed.
    // First, mark A as terminal so it's eligible for reaping.
    dag.node_mut("hashA")
        .expect("hashA")
        .transition(DerivationStatus::Queued)?;
    dag.node_mut("hashA")
        .expect("hashA")
        .transition(DerivationStatus::Ready)?;
    dag.node_mut("hashA")
        .expect("hashA")
        .transition(DerivationStatus::Assigned)?;
    dag.node_mut("hashA")
        .expect("hashA")
        .transition(DerivationStatus::Running)?;
    dag.node_mut("hashA")
        .expect("hashA")
        .transition(DerivationStatus::Completed)?;
    let reaped = dag.remove_build_interest_and_reap(b1);
    assert_eq!(reaped, 1, "hashA should be reaped (terminal, no interest)");
    assert_eq!(
        dag.hash_for_path(&p_a),
        None,
        "reap must remove path index for reaped node"
    );
    // B is not terminal, so it survives reaping.
    assert_eq!(dag.hash_for_path(&p_b).map(|h| h.as_str()), Some("hashB"));
    Ok(())
}

/// Regression test for stack overflow in the recursive DFS cycle check.
/// The recursive version blew the ~2MB tokio stack at ~10-15k depth.
/// The iterative version should handle arbitrary depth bounded only by heap.
#[test]
fn test_cycle_detection_deep_linear_chain_no_overflow() {
    let mut dag = DerivationDag::new();
    let build_id = Uuid::new_v4();

    // 10k-node linear chain: node[i] depends on node[i+1].
    // No cycle. With the old recursive DFS, this recursed 10k frames
    // (~1.5MB) which was close to the stack limit; 50k would panic.
    const DEPTH: usize = 10_000;
    let nodes: Vec<_> = (0..DEPTH)
        .map(|i| {
            make_node_with_path(
                &format!("hash{i:05}"),
                &format!("/nix/store/{i:032}-n{i}.drv"),
                "x86_64-linux",
            )
        })
        .collect();
    let edges: Vec<_> = (0..DEPTH - 1)
        .map(|i| {
            make_edge_with_paths(
                &format!("/nix/store/{i:032}-n{i}.drv"),
                &format!("/nix/store/{:032}-n{}.drv", i + 1, i + 1),
            )
        })
        .collect();

    // Must not panic (stack overflow) and must succeed (no cycle).
    let result = dag.merge(build_id, &nodes, &edges, "");
    assert!(result.is_ok(), "acyclic deep chain should merge");
    assert_eq!(dag.nodes.len(), DEPTH);
}

/// Deep chain with a back-edge at the very end: cycle must be detected
/// at depth.
#[test]
fn test_cycle_detection_deep_chain_with_back_edge() {
    let mut dag = DerivationDag::new();
    let build_id = Uuid::new_v4();

    const DEPTH: usize = 5_000;
    let nodes: Vec<_> = (0..DEPTH)
        .map(|i| {
            make_node_with_path(
                &format!("hash{i:05}"),
                &format!("/nix/store/{i:032}-n{i}.drv"),
                "x86_64-linux",
            )
        })
        .collect();
    let mut edges: Vec<_> = (0..DEPTH - 1)
        .map(|i| {
            make_edge_with_paths(
                &format!("/nix/store/{i:032}-n{i}.drv"),
                &format!("/nix/store/{:032}-n{}.drv", i + 1, i + 1),
            )
        })
        .collect();
    // Back-edge from the deepest node to the root: cycle.
    edges.push(make_edge_with_paths(
        &format!("/nix/store/{:032}-n{}.drv", DEPTH - 1, DEPTH - 1),
        &format!("/nix/store/{:032}-n{}.drv", 0, 0),
    ));

    let result = dag.merge(build_id, &nodes, &edges, "");
    assert!(result.is_err(), "cycle at depth must be detected");
    assert_eq!(dag.nodes.len(), 0, "rollback must clear all nodes");
}

// ---------------------------------------------------------------------------
// canonical() interning
// ---------------------------------------------------------------------------

#[test]
fn test_canonical_returns_pointer_equal_arc() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let nodes = vec![make_node("canon-hash", "x86_64-linux")];
    dag.merge(Uuid::new_v4(), &nodes, &[], "")?;

    // Two calls to canonical() return ptr-equal clones — both are refcount
    // bumps of the same Arc stored as the key in `nodes`.
    let a = dag.canonical("canon-hash").expect("inserted above");
    let b = dag.canonical("canon-hash").expect("inserted above");
    assert!(DrvHash::ptr_eq(&a, &b), "canonical must be ptr-stable");

    // And a FRESH construction from the same string is NOT ptr-equal —
    // it's a distinct alloc. canonical() exchanges it for the interned one.
    let fresh = DrvHash::from("canon-hash");
    assert_eq!(fresh, a, "structurally equal");
    assert!(!DrvHash::ptr_eq(&fresh, &a), "but distinct alloc");
    Ok(())
}

#[test]
fn test_canonical_returns_none_for_unknown() {
    let dag = DerivationDag::new();
    assert!(dag.canonical("never-inserted").is_none());
}

/// The interning INVARIANT: all DrvHash clones flowing out of DAG accessors
/// are ptr-equal to the canonical key in `nodes`. This holds because
/// `merge()` inserts clones of the SAME local Arc into `nodes`,
/// `path_to_hash`, and `newly_inserted` — everything downstream reads
/// from those maps.
///
/// This test verifies the invariant end-to-end across a multi-merge
/// scenario with edges (the case where `path_to_hash.get().cloned()`
/// feeds into `children`/`parents`).
#[test]
fn test_interning_invariant_across_maps() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let b1 = Uuid::new_v4();
    let nodes = vec![
        make_node("parent", "x86_64-linux"),
        make_node("child", "x86_64-linux"),
    ];
    let edges = vec![make_edge("parent", "child")];
    let result = dag.merge(b1, &nodes, &edges, "")?;

    let parent_canon = dag.canonical("parent").unwrap();
    let child_canon = dag.canonical("child").unwrap();

    // newly_inserted entries are canonical (cloned from same Arc as nodes key).
    let ni_parent = result.newly_inserted.get("parent").unwrap();
    let ni_child = result.newly_inserted.get("child").unwrap();
    assert!(DrvHash::ptr_eq(ni_parent, &parent_canon));
    assert!(DrvHash::ptr_eq(ni_child, &child_canon));

    // path_to_hash values are canonical.
    let pth = dag.hash_for_path(&test_drv_path("parent")).unwrap();
    assert!(DrvHash::ptr_eq(pth, &parent_canon));

    // get_parents / get_children return canonical.
    let parents_of_child = dag.get_parents("child");
    assert!(DrvHash::ptr_eq(&parents_of_child[0], &parent_canon));
    let children_of_parent = dag.get_children("parent");
    assert!(DrvHash::ptr_eq(&children_of_parent[0], &child_canon));

    // compute_initial_states returns canonical.
    let states = dag.compute_initial_states(&result.newly_inserted);
    for (h, _) in &states {
        let canon = dag.canonical(h).unwrap();
        assert!(DrvHash::ptr_eq(h, &canon));
    }

    // --- The key case: second merge of the same node. ---
    // Without canonical() interning: interest_added holds a fresh
    // Arc (from proto string). With it: exchanged via canonical()
    // upfront, so it's ptr-equal.
    let b2 = Uuid::new_v4();
    let result2 = dag.merge(b2, &nodes, &edges, "")?;
    assert_eq!(result2.interest_added.len(), 2);
    for h in &result2.interest_added {
        let canon = dag.canonical(h).unwrap();
        assert!(
            DrvHash::ptr_eq(h, &canon),
            "interest_added entry must be canonical (was the D5 fix)"
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// BuildSummary: critpath_remaining + assigned_workers (P0270)
// ---------------------------------------------------------------------------

/// Walk a node through Created→Queued→Ready→Assigned→Running. The state
/// machine is strict; each intermediate is required.
fn advance_to_running(dag: &mut DerivationDag, hash: &str, worker: &str) {
    let n = dag.node_mut(hash).expect(hash);
    n.transition(DerivationStatus::Queued).unwrap();
    n.transition(DerivationStatus::Ready).unwrap();
    n.transition(DerivationStatus::Assigned).unwrap();
    n.transition(DerivationStatus::Running).unwrap();
    n.assigned_worker = Some(worker.into());
}

/// Plan doc T3: 2 running + 1 queued → assigned_workers.len() == 2.
/// Plus dedup: a third running drv on the SAME worker as the first
/// must not inflate the count. BTreeSet collection guarantees both
/// dedup and sorted order.
#[test]
fn build_summary_assigned_workers_dedup() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let build = Uuid::new_v4();
    // 4 independent nodes (no edges — we're testing the summary pass,
    // not DAG topology).
    dag.merge(
        build,
        &[
            make_node("r1", "x86_64-linux"),
            make_node("r2", "x86_64-linux"),
            make_node("r3", "x86_64-linux"),
            make_node("q1", "x86_64-linux"),
        ],
        &[],
        "",
    )?;

    // r1, r2 on distinct workers; r3 on same worker as r1 (dedup case).
    // q1 stays Queued.
    advance_to_running(&mut dag, "r1", "worker-alpha");
    advance_to_running(&mut dag, "r2", "worker-beta");
    advance_to_running(&mut dag, "r3", "worker-alpha");
    dag.node_mut("q1")
        .unwrap()
        .transition(DerivationStatus::Queued)?;

    let s = dag.build_summary(build);

    // Precondition: the setup actually produced the shape we claim.
    // Without this, a "3 running" setup bug would let the main
    // assert pass for the wrong reason (e.g., if advance_to_running
    // silently failed a transition and left r3 in Created → queued
    // bucket → running=2 by accident).
    assert_eq!(s.running, 3, "setup: 3 running expected");
    assert_eq!(s.queued, 1, "setup: 1 queued expected");
    assert_eq!(s.total, 4);

    // The main assert: 2 distinct workers, sorted.
    assert_eq!(
        s.assigned_workers,
        vec!["worker-alpha", "worker-beta"],
        "dedup(3 running on 2 workers) = 2 workers, BTreeSet-sorted"
    );

    Ok(())
}

/// critpath_remaining = max(priority) across NON-terminal. A completed
/// node's priority is stale (only ancestors get recomputed by
/// update_ancestors); including it would over-report.
#[test]
fn build_summary_critpath_excludes_terminal() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let build = Uuid::new_v4();
    dag.merge(
        build,
        &[
            make_node("big", "x86_64-linux"),
            make_node("small", "x86_64-linux"),
        ],
        &[],
        "",
    )?;

    // Directly set priorities (priority is pub; normally populated
    // via compute_initial but we're testing build_summary's max-
    // over-non-terminal filter, not the priority computation itself).
    dag.node_mut("big").unwrap().priority = 100.0;
    dag.node_mut("small").unwrap().priority = 5.0;

    // Both non-terminal: max is 100.
    let s = dag.build_summary(build);
    assert_eq!(s.critpath_remaining, 100.0);

    // Complete "big" — it goes terminal but keeps its stale
    // priority=100. build_summary must exclude it.
    advance_to_running(&mut dag, "big", "w");
    dag.node_mut("big")
        .unwrap()
        .transition(DerivationStatus::Completed)?;

    let s = dag.build_summary(build);
    assert_eq!(
        s.critpath_remaining, 5.0,
        "terminal 'big' (stale priority=100) must be excluded; only 'small'=5 remains"
    );

    // Terminal nodes also contribute no worker.
    assert!(
        s.assigned_workers.is_empty(),
        "completed node's assigned_worker is cleared by the real transition path, \
         but even if it weren't, Running|Assigned arm is the only collector"
    );

    Ok(())
}

/// critpath_remaining is build-scoped: a node in the DAG that is NOT
/// interested in this build doesn't contribute, even if its priority
/// is higher. Guards against a regression where the
/// interested_builds filter gets dropped and we accidentally max
/// across the whole DAG.
#[test]
fn build_summary_critpath_build_scoped() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let build_a = Uuid::new_v4();
    let build_b = Uuid::new_v4();

    dag.merge(build_a, &[make_node("a-drv", "x86_64-linux")], &[], "")?;
    dag.merge(build_b, &[make_node("b-drv", "x86_64-linux")], &[], "")?;

    dag.node_mut("a-drv").unwrap().priority = 10.0;
    dag.node_mut("b-drv").unwrap().priority = 999.0;

    let s = dag.build_summary(build_a);
    assert_eq!(
        s.critpath_remaining, 10.0,
        "build_a's critpath must ignore b-drv (priority 999, different build)"
    );
    assert_eq!(s.total, 1, "build_a sees only its own node");

    Ok(())
}

/// Empty worker set when no derivations are Assigned/Running. A build
/// where everything is Queued (no worker yet picked anything up) has
/// an empty worker list — not a None, not a panic.
#[test]
fn build_summary_no_running_empty_workers() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let build = Uuid::new_v4();
    dag.merge(build, &[make_node("q", "x86_64-linux")], &[], "")?;
    dag.node_mut("q")
        .unwrap()
        .transition(DerivationStatus::Queued)?;

    let s = dag.build_summary(build);
    assert!(s.assigned_workers.is_empty());
    assert_eq!(s.queued, 1);
    // critpath still reflects the queued node — it's non-terminal.
    // Default priority is 0.0 (we didn't set it), so critpath is 0.
    // That's correct: no estimate = 0s ETA. In practice compute_initial
    // would have set a real value.
    assert_eq!(s.critpath_remaining, 0.0);

    Ok(())
}
