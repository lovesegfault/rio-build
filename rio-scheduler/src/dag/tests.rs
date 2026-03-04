use super::*;
use rio_proto::types::{DerivationEdge, DerivationNode};
use rio_test_support::fixtures::test_drv_path;

/// Build a test node. `drv_path` is auto-generated from `tag` via [`test_drv_path`].
fn make_node(tag: &str, system: &str) -> DerivationNode {
    DerivationNode {
        drv_path: test_drv_path(tag),
        drv_hash: tag.to_string(),
        pname: String::new(),
        system: system.to_string(),
        required_features: vec![],
        output_names: vec!["out".to_string()],
        is_fixed_output: false,
        expected_output_paths: vec![],
        drv_content: Vec::new(),
    }
}

/// Build a test node with an EXPLICIT `drv_path` (for deep-chain tests
/// that generate their own valid 32-char-hash paths).
fn make_node_with_path(drv_hash: &str, drv_path: &str, system: &str) -> DerivationNode {
    DerivationNode {
        drv_path: drv_path.to_string(),
        drv_hash: drv_hash.to_string(),
        pname: String::new(),
        system: system.to_string(),
        required_features: vec![],
        output_names: vec!["out".to_string()],
        is_fixed_output: false,
        expected_output_paths: vec![],
        drv_content: Vec::new(),
    }
}

/// Build a test edge from tags. Paths are auto-generated via [`test_drv_path`].
fn make_edge(parent_tag: &str, child_tag: &str) -> DerivationEdge {
    DerivationEdge {
        parent_drv_path: test_drv_path(parent_tag),
        child_drv_path: test_drv_path(child_tag),
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

    let newly = dag.merge(build_id, &nodes, &edges)?.newly_inserted;
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

    let newly1 = dag.merge(build1, &nodes, &[])?.newly_inserted;
    assert_eq!(newly1.len(), 1);

    let result2 = dag.merge(build2, &nodes, &[])?;
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

    dag.merge(build_id, &nodes, &edges)?;

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

    let newly = dag.merge(build_id, &nodes, &edges)?.newly_inserted;
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

#[test]
fn test_initial_states_with_prepoisoned_dep() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let build1 = Uuid::new_v4();

    // Build 1: just the leaf.
    let leaf_nodes = vec![make_node("leafP", "x86_64-linux")];
    dag.merge(build1, &leaf_nodes, &[])?;

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
    let newly = dag.merge(build2, &parent_nodes, &edges)?.newly_inserted;

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

    dag.merge(build_id, &nodes, &edges)?;

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
// Group 2: Cycle detection
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

    let result = dag.merge(build_id, &nodes, &edges);
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

    let result = dag.merge(build_id, &nodes, &edges);
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
    assert!(dag.merge(build_id, &cyclic_nodes, &cyclic_edges).is_err());

    // Second: insert a valid DAG with the same nodes (should succeed)
    let valid_edges = vec![make_edge("hashA", "hashB")];
    let result = dag.merge(build_id, &cyclic_nodes, &valid_edges);
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
    dag.merge(build1, &nodes, &initial_edges)?;
    assert_eq!(dag.nodes.len(), 2);

    // Now merge the SAME nodes (no new inserts) with a B->A edge.
    // This creates a cycle via a new edge between two existing nodes.
    let build2 = Uuid::new_v4();
    let cycle_edge = vec![make_edge("hashB", "hashA")];
    let result = dag.merge(build2, &nodes, &cycle_edge);

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
    dag.merge(b1, &nodes_a, &[])?;
    assert!(
        dag.nodes
            .get("hashA")
            .expect("hashA")
            .interested_builds
            .contains(&b1),
        "B1 interest in A should be set after successful merge"
    );

    // Step 2: merge B1 again with nodes {A, C} and cycle A->C->A — fails.
    // Pre-fix: rollback would clear B1 from A even though B1 was already
    // interested in A from step 1.
    let nodes_ac = vec![
        make_node("hashA", "x86_64-linux"),
        make_node("hashC", "x86_64-linux"),
    ];
    let cycle_edges = vec![make_edge("hashA", "hashC"), make_edge("hashC", "hashA")];
    let result = dag.merge(b1, &nodes_ac, &cycle_edges);
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
    dag.merge(b1, &nodes, &[])?;
    assert_eq!(dag.hash_for_path(&p_a).map(|h| h.as_str()), Some("hashA"));
    assert_eq!(dag.hash_for_path(&p_b).map(|h| h.as_str()), Some("hashB"));
    assert_eq!(dag.hash_for_path("/nix/store/nonexistent.drv"), None);

    // Cycle rollback: newly-inserted node's path entry must be removed.
    let cycle_nodes = vec![
        make_node("hashA", "x86_64-linux"),
        make_node("hashC", "x86_64-linux"),
    ];
    let cycle_edges = vec![make_edge("hashA", "hashC"), make_edge("hashC", "hashA")];
    dag.merge(b1, &cycle_nodes, &cycle_edges).unwrap_err();
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
    let result = dag.merge(build_id, &nodes, &edges);
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

    let result = dag.merge(build_id, &nodes, &edges);
    assert!(result.is_err(), "cycle at depth must be detected");
    assert_eq!(dag.nodes.len(), 0, "rollback must clear all nodes");
}

// ---------------------------------------------------------------------------
// D5: canonical() interning
// ---------------------------------------------------------------------------

#[test]
fn test_canonical_returns_pointer_equal_arc() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let nodes = vec![make_node("canon-hash", "x86_64-linux")];
    dag.merge(Uuid::new_v4(), &nodes, &[])?;

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
    let result = dag.merge(b1, &nodes, &edges)?;

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

    // --- The ONE case D5 fixed: second merge of the same node. ---
    // Before: interest_added held a fresh Arc (from proto string).
    // After: exchanged via canonical() upfront, so it's ptr-equal.
    let b2 = Uuid::new_v4();
    let result2 = dag.merge(b2, &nodes, &edges)?;
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
