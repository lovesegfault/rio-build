use super::*;
use crate::domain::{DerivationEdge, DerivationNode};
use rio_test_support::fixtures::{make_derivation_node, test_drv_path};
use rstest::rstest;

/// Proto fixture → domain. `rio_test_support::make_derivation_node`
/// returns the proto type (shared with gateway/gRPC tests); the DAG
/// operates on domain types. b03 will migrate the fixture itself.
fn make_node(tag: &str, system: &str) -> DerivationNode {
    make_derivation_node(tag, system).into()
}

fn make_edge(parent_tag: &str, child_tag: &str) -> DerivationEdge {
    rio_test_support::fixtures::make_edge(parent_tag, child_tag).into()
}

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

// r[verify sched.merge.wanted-outputs+2]
/// A second build merging an already-known node UNIONs its wanted set
/// into the existing node's (it must never shrink — build B's `{out}`
/// must not un-want build A's still-needed `dev`), the empty "all
/// wanted" sentinel saturates the union, and a rolled-back merge
/// restores the pre-merge wanted set.
#[test]
fn test_merge_unions_wanted_outputs_on_existing_node() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let mut node = make_node("hashW", "x86_64-linux");
    node.wanted_output_names = vec!["out".into()];
    dag.merge(Uuid::new_v4(), &[node.clone()], &[], "")?;
    assert_eq!(dag.nodes["hashW"].wanted_output_names, vec!["out"]);

    // Second build wants a different output → union.
    node.wanted_output_names = vec!["dev".into()];
    dag.merge(Uuid::new_v4(), &[node.clone()], &[], "")?;
    assert_eq!(
        dag.nodes["hashW"].wanted_output_names,
        vec!["dev", "out"],
        "second build's wanted set must UNION into the existing node, not replace it"
    );

    // Third build wants everything (empty sentinel) → saturates to all.
    node.wanted_output_names = vec![];
    dag.merge(Uuid::new_v4(), &[node.clone()], &[], "")?;
    assert!(
        dag.nodes["hashW"].wanted_output_names.is_empty(),
        "all ∪ anything = all (the empty sentinel saturates the union)"
    );

    // A failed merge (cycle) must restore the pre-merge wanted set —
    // the rejected build's wanted growth must not stick.
    let mut dag = DerivationDag::new();
    let mut a = make_node("hashWA", "x86_64-linux");
    a.wanted_output_names = vec!["out".into()];
    dag.merge(Uuid::new_v4(), &[a.clone()], &[], "")?;
    // The resident parent must be edge-admissible under the
    // creation-scoped edge rule (sched.merge.edge-creation-scoped) for
    // the joining submission's hashWA→hashWB edge to land at all (a
    // foreign-parent edge would be skipped, no cycle would form, and
    // the merge would succeed) — same staging as
    // test_cycle_via_new_edge_between_existing_nodes.
    dag.nodes.get_mut("hashWA").unwrap().topdown_pruned = true;
    let mut b = make_node("hashWB", "x86_64-linux");
    b.wanted_output_names = vec![];
    a.wanted_output_names = vec!["dev".into()];
    let cycle = vec![make_edge("hashWA", "hashWB"), make_edge("hashWB", "hashWA")];
    assert!(dag.merge(Uuid::new_v4(), &[a, b], &cycle, "").is_err());
    assert_eq!(
        dag.nodes["hashWA"].wanted_output_names,
        vec!["out"],
        "rollback must restore the pre-merge wanted set"
    );
    Ok(())
}

/// Alongside the node-level union, `merge` records WHICH build
/// contributed WHICH wanted set in `wanted_by_build`, on both the
/// new-node path and the existing-node path. The per-build entry is the
/// submission's own set (empty = that build wants ALL declared
/// outputs), independent of what the union saturates to.
#[test]
fn test_merge_records_per_build_contribution() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let build1 = Uuid::new_v4();
    let build2 = Uuid::new_v4();
    let mut node = make_node("hashC", "x86_64-linux");

    // New-node path: build1 wants only `out`.
    node.wanted_output_names = vec!["out".into()];
    dag.merge(build1, &[node.clone()], &[], "")?;
    assert_eq!(
        dag.nodes["hashC"].wanted_by_build.get(&build1),
        Some(&vec!["out".to_string()]),
        "new-node path must record the submitting build's contribution"
    );

    // Existing-node path: build2 wants all (empty sentinel).
    node.wanted_output_names = vec![];
    dag.merge(build2, &[node.clone()], &[], "")?;
    let n = &dag.nodes["hashC"];
    assert_eq!(
        n.wanted_by_build.get(&build2),
        Some(&Vec::<String>::new()),
        "existing-node path must record the second build's contribution \
         (empty = all declared outputs)"
    );
    assert_eq!(
        n.wanted_by_build.get(&build1),
        Some(&vec!["out".to_string()]),
        "the first build's contribution must not be overwritten by the second's"
    );
    // The stored node-level union still saturates exactly as before.
    assert!(n.wanted_output_names.is_empty());
    Ok(())
}

/// One submission carrying the SAME drv twice with different wanted
/// sets (e.g. two roots whose closures both name it): the recorded
/// per-build contribution is the union of both occurrences, not
/// whichever came last, and a failed merge restores the exact pre-merge
/// contribution (the rollback bookkeeping captures the prior before the
/// first mutation and replays in reverse, so the true pre-merge value
/// sticks).
#[test]
fn test_merge_duplicate_drv_in_submission_unions_contribution() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let build_a = Uuid::new_v4();
    let build_b = Uuid::new_v4();

    // build_a pre-existing with the empty all-wanted sentinel, so the
    // node-level union stays saturated throughout and only the
    // per-build contribution is exercised below.
    let mut node = make_node("hashDU", "x86_64-linux");
    node.wanted_output_names = vec![];
    dag.merge(build_a, &[node.clone()], &[], "")?;

    // build_b's submission lists the same drv twice with different
    // wanted sets → its contribution is the union of both occurrences.
    let mut occ1 = node.clone();
    occ1.wanted_output_names = vec!["out".into()];
    let mut occ2 = node.clone();
    occ2.wanted_output_names = vec!["dev".into()];
    dag.merge(build_b, &[occ1, occ2], &[], "")?;
    let n = &dag.nodes["hashDU"];
    assert_eq!(
        n.wanted_by_build.get(&build_b),
        Some(&vec!["dev".to_string(), "out".to_string()]),
        "the second occurrence must union into the first, not overwrite it"
    );
    assert_eq!(
        n.wanted_by_build.get(&build_a),
        Some(&Vec::<String>::new()),
        "the other build's all-wanted contribution must be untouched"
    );

    // A FAILED merge (cycle) by build_b, again carrying the drv twice:
    // rollback must restore build_b's contribution to the pre-merge
    // value ({dev, out}), not to the mid-merge value of either
    // occurrence.
    let mut occ3 = node.clone();
    occ3.wanted_output_names = vec!["man".into()];
    let mut occ4 = node.clone();
    occ4.wanted_output_names = vec!["lib".into()];
    let nodes = vec![
        occ3,
        occ4,
        make_node("hashDUy", "x86_64-linux"),
        make_node("hashDUz", "x86_64-linux"),
    ];
    let cycle = vec![
        make_edge("hashDUy", "hashDUz"),
        make_edge("hashDUz", "hashDUy"),
    ];
    assert!(dag.merge(build_b, &nodes, &cycle, "").is_err());
    let n = &dag.nodes["hashDU"];
    assert_eq!(
        n.wanted_by_build.get(&build_b),
        Some(&vec!["dev".to_string(), "out".to_string()]),
        "rollback must restore the pre-merge contribution, not a mid-merge union"
    );
    assert_eq!(n.wanted_by_build.get(&build_a), Some(&Vec::<String>::new()));
    Ok(())
}

/// One submission carrying the SAME drv twice where BOTH occurrences
/// grow a pre-existing node's non-saturated wanted union: a failed
/// merge must restore the exact pre-merge wanted set. `wanted_grown`
/// records the node once per growth — twice here, the second prior
/// being the already-grown value — so rollback must replay the entries
/// in reverse for the first-captured (true pre-merge) value to stick.
/// (The duplicate-drv test above keeps the union saturated via the
/// empty all-wanted sentinel, which never records a second growth.)
#[test]
fn test_merge_rollback_duplicate_drv_restores_wanted_union() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let build_a = Uuid::new_v4();
    let build_b = Uuid::new_v4();

    // Pre-existing node with a non-saturated wanted set {out}.
    let mut node = make_node("hashWG", "x86_64-linux");
    node.wanted_output_names = vec!["out".into()];
    dag.merge(build_a, &[node.clone()], &[], "")?;

    // build_b's submission carries the drv twice; each occurrence grows
    // the union ({out} → {dev,out} → {dev,man,out}). The merge then
    // fails on a cycle between two other nodes.
    let mut occ1 = node.clone();
    occ1.wanted_output_names = vec!["dev".into()];
    let mut occ2 = node.clone();
    occ2.wanted_output_names = vec!["man".into()];
    let nodes = vec![
        occ1,
        occ2,
        make_node("hashWGy", "x86_64-linux"),
        make_node("hashWGz", "x86_64-linux"),
    ];
    let cycle = vec![
        make_edge("hashWGy", "hashWGz"),
        make_edge("hashWGz", "hashWGy"),
    ];
    assert!(dag.merge(build_b, &nodes, &cycle, "").is_err());
    assert_eq!(
        dag.nodes["hashWG"].wanted_output_names,
        vec!["out"],
        "rollback must restore the true pre-merge wanted set, not the \
         intermediate union captured by the second occurrence"
    );
    Ok(())
}

/// Duplicate-drv failed merge where the pre-existing node is ALSO
/// retriable-on-resubmit: occurrence 1 takes the resubmit-reset path
/// (old node moved aside, fresh replacement inserted with the
/// carried-over union), occurrence 2 then grows the FRESH node's union
/// and records a contribution. Rollback restores the old node
/// wholesale from `removed_retriable`; the `wanted_grown` /
/// `contributions_recorded` entries describe the discarded replacement
/// and must NOT be replayed onto the restored node — otherwise the
/// rejected build's wanted names leak into the stored union and a
/// stray `wanted_by_build` entry appears for a build that is not in
/// `interested_builds`.
#[test]
fn test_merge_rollback_duplicate_drv_on_retriable_node_restores_exactly() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let build_a = Uuid::new_v4();
    let build_b = Uuid::new_v4();

    // Pre-existing node wanting {out} with build_a's contribution,
    // then left retriable (a cancelled build's leftover).
    let mut node = make_node("hashRW", "x86_64-linux");
    node.wanted_output_names = vec!["out".into()];
    dag.merge(build_a, &[node.clone()], &[], "")?;
    dag.nodes
        .get_mut("hashRW")
        .unwrap()
        .set_status_for_test(DerivationStatus::Cancelled);

    // build_b's FAILED merge (cycle) carries the drv twice with wanted
    // sets that grow the union; occurrence 1 resubmit-resets the node,
    // occurrence 2 mutates the fresh replacement.
    let mut occ1 = node.clone();
    occ1.wanted_output_names = vec!["dev".into()];
    let mut occ2 = node.clone();
    occ2.wanted_output_names = vec!["man".into()];
    let nodes = vec![
        occ1,
        occ2,
        make_node("hashRWy", "x86_64-linux"),
        make_node("hashRWz", "x86_64-linux"),
    ];
    let cycle = vec![
        make_edge("hashRWy", "hashRWz"),
        make_edge("hashRWz", "hashRWy"),
    ];
    assert!(dag.merge(build_b, &nodes, &cycle, "").is_err());

    let n = &dag.nodes["hashRW"];
    assert_eq!(
        n.wanted_output_names,
        vec!["out"],
        "the wholesale-restored node must keep its pre-merge wanted set; \
         the rejected merge's union must not leak into the stored fallback"
    );
    assert!(
        !n.wanted_by_build.contains_key(&build_b),
        "no contribution may appear for the rejected build on the restored \
         node (entries follow interested_builds membership)"
    );
    assert_eq!(
        n.wanted_by_build.get(&build_a),
        Some(&vec!["out".to_string()]),
        "the prior build's contribution survives the restore untouched"
    );
    assert_eq!(
        n.interested_builds,
        HashSet::from([build_a]),
        "interest must be exactly the pre-merge set"
    );
    Ok(())
}

/// The resubmit-reset path destructively removes a retriable node and
/// re-inserts fresh state; prior interest AND prior per-build
/// contributions are carried over so the other still-interested builds'
/// wants survive the reset (contributions follow interest membership).
#[test]
fn test_merge_resubmit_reset_carries_contributions() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let build1 = Uuid::new_v4();
    let mut node = make_node("hashRC", "x86_64-linux");
    node.wanted_output_names = vec!["dev".into()];
    dag.merge(build1, &[node.clone()], &[], "")?;
    dag.nodes
        .get_mut("hashRC")
        .unwrap()
        .set_status_for_test(DerivationStatus::Cancelled);

    // build2 resubmits wanting only `out` → reset; build1's carried-over
    // interest keeps its `dev` contribution.
    let build2 = Uuid::new_v4();
    node.wanted_output_names = vec!["out".into()];
    dag.merge(build2, &[node.clone()], &[], "")?;
    let n = &dag.nodes["hashRC"];
    assert!(n.interested_builds.contains(&build1));
    assert_eq!(
        n.wanted_by_build.get(&build1),
        Some(&vec!["dev".to_string()]),
        "carried-over interest must keep its contribution across the reset"
    );
    assert_eq!(
        n.wanted_by_build.get(&build2),
        Some(&vec!["out".to_string()]),
        "the resubmitter's own contribution must be recorded on the reset node"
    );
    Ok(())
}

/// A failed merge (cycle) must restore per-build contributions exactly:
/// an entry the merge ADDED is removed, and an entry the merge GREW
/// (the same build re-merging the node with a different wanted set) is
/// restored to its prior value — mirroring the `wanted_grown` rollback.
#[test]
fn test_merge_rollback_restores_contributions() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let build_a = Uuid::new_v4();
    let build_b = Uuid::new_v4();
    let mut x = make_node("hashRB", "x86_64-linux");
    x.wanted_output_names = vec!["out".into()];
    dag.merge(build_a, &[x.clone()], &[], "")?;

    // Failed merge by a DIFFERENT build: its added entry must be removed.
    x.wanted_output_names = vec!["dev".into()];
    let nodes = vec![
        x.clone(),
        make_node("hashRBy", "x86_64-linux"),
        make_node("hashRBz", "x86_64-linux"),
    ];
    let cycle = vec![
        make_edge("hashRBy", "hashRBz"),
        make_edge("hashRBz", "hashRBy"),
    ];
    assert!(dag.merge(build_b, &nodes, &cycle, "").is_err());
    let n = &dag.nodes["hashRB"];
    assert!(
        !n.wanted_by_build.contains_key(&build_b),
        "a rolled-back merge must not leave the rejected build's contribution"
    );
    assert_eq!(
        n.wanted_by_build.get(&build_a),
        Some(&vec!["out".to_string()])
    );

    // Failed merge by the SAME build growing its own prior entry: the
    // prior contribution must be restored (not removed, not left at the
    // mid-merge unioned value).
    assert!(dag.merge(build_a, &nodes, &cycle, "").is_err());
    let n = &dag.nodes["hashRB"];
    assert_eq!(
        n.wanted_by_build.get(&build_a),
        Some(&vec!["out".to_string()]),
        "rollback must restore an overwritten contribution to its pre-merge value"
    );
    assert!(
        n.interested_builds.contains(&build_a),
        "interest from the prior successful merge must survive the rollback"
    );
    Ok(())
}

/// Contributions follow interest membership on the way out too:
/// `remove_build_interest` (the cancel path) and
/// `remove_build_interest_and_reap` (terminal-build cleanup) drop the
/// build's `wanted_by_build` entry together with its `interested_builds`
/// membership, leaving other builds' contributions untouched.
#[test]
fn test_remove_build_interest_drops_contribution() -> anyhow::Result<()> {
    let mut node = make_node("hashRM", "x86_64-linux");
    let build1 = Uuid::new_v4();
    let build2 = Uuid::new_v4();

    // remove_build_interest (cancel path).
    let mut dag = DerivationDag::new();
    node.wanted_output_names = vec!["out".into()];
    dag.merge(build1, &[node.clone()], &[], "")?;
    node.wanted_output_names = vec!["dev".into()];
    dag.merge(build2, &[node.clone()], &[], "")?;
    dag.remove_build_interest(build1);
    let n = &dag.nodes["hashRM"];
    assert!(
        !n.wanted_by_build.contains_key(&build1),
        "removed interest must take the build's contribution with it"
    );
    assert_eq!(
        n.wanted_by_build.get(&build2),
        Some(&vec!["dev".to_string()]),
        "the remaining build's contribution must be untouched"
    );

    // remove_build_interest_and_reap (terminal cleanup). The node is
    // shared with build2 so it survives the reap; build1's entry goes.
    let mut dag = DerivationDag::new();
    node.wanted_output_names = vec!["out".into()];
    dag.merge(build1, &[node.clone()], &[], "")?;
    node.wanted_output_names = vec!["dev".into()];
    dag.merge(build2, &[node.clone()], &[], "")?;
    dag.remove_build_interest_and_reap(build1);
    let n = &dag.nodes["hashRM"];
    assert!(!n.wanted_by_build.contains_key(&build1));
    assert_eq!(
        n.wanted_by_build.get(&build2),
        Some(&vec!["dev".to_string()])
    );
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
    let states: HashMap<_, _> = dag.compute_initial_states(&newly).into_iter().collect();

    // B has no deps -> Ready; A has dep on B -> Queued. Assert both
    // are present (the previous for-loop form passed vacuously on
    // `vec![]`).
    assert_eq!(states.len(), 2);
    assert_eq!(states["hashB"], DerivationStatus::Ready);
    assert_eq!(states["hashA"], DerivationStatus::Queued);
    Ok(())
}

/// Reset-on-resubmit, single-node cases. Resubmitting a stuck/terminal
/// derivation must reset it so it flows through `compute_initial_states`
/// and re-dispatches. Without the reset, the resubmitted build hangs
/// forever: merge adds interest but the node stays terminal, and
/// `compute_initial_states` only iterates `newly_inserted`.
///
/// I-169 made Poisoned conditionally retriable (under
/// `POISON_RESUBMIT_RETRY_LIMIT`): I-167's `?id=` patch poisoned, then
/// 27k DependencyFailed dependents re-derived from the still-poisoned
/// parent on every resubmit → fail-fast. The former
/// `test_merge_does_not_reset_poisoned` is superseded by the
/// `poisoned_at_limit` case here (bound holds at limit) plus
/// `merge_reset_parent_child::poisoned_under_limit` (resets under limit).
#[rstest]
// Cancelled node still in DAG during TERMINAL_CLEANUP_DELAY (reap hasn't
// run yet) — resubmit must reset so the new build doesn't hang.
#[case::cancelled(DerivationStatus::Cancelled, 0, true, true, DerivationStatus::Created)]
// Failed: non-terminal but stuck without a retry driver. build1 still
// interested → preserved across reset so the stuck build also benefits.
#[case::failed(DerivationStatus::Failed, 0, false, true, DerivationStatus::Created)]
// I-169 bound: at resubmit_cycles >= LIMIT, Poisoned stays Poisoned on
// resubmit. 24h TTL or ClearPoison are the only overrides.
// r[verify sched.merge.poisoned-resubmit-bounded+2]
#[case::poisoned_at_limit(
    DerivationStatus::Poisoned,
    crate::state::POISON_RESUBMIT_RETRY_LIMIT,
    false,
    false,
    DerivationStatus::Poisoned
)]
fn merge_reset_single_node(
    #[case] prior: DerivationStatus,
    #[case] retry: u32,
    #[case] clear_interest: bool,
    #[case] expect_reset: bool,
    #[case] expect_status: DerivationStatus,
) -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let build1 = Uuid::new_v4();
    let nodes = vec![make_node("h", "x86_64-linux")];
    dag.merge(build1, &nodes, &[], "")?;
    {
        let n = dag.nodes.get_mut("h").unwrap();
        n.set_status_for_test(prior);
        n.retry.resubmit_cycles = retry;
        if clear_interest {
            n.interested_builds.clear();
        }
    }

    let build2 = Uuid::new_v4();
    let result = dag.merge(build2, &nodes, &[], "")?;

    assert_eq!(
        result.newly_inserted.contains("h"),
        expect_reset,
        "{prior:?} retry={retry}: newly_inserted membership"
    );
    let n = &dag.nodes["h"];
    assert_eq!(n.status(), expect_status);
    // build2 always gains interest (reset arm OR pre-existing-node arm).
    assert!(n.interested_builds.contains(&build2));
    // Prior interest preserved across reset (or untouched on non-reset).
    assert_eq!(n.interested_builds.contains(&build1), !clear_interest);
    if expect_reset {
        // Reset → compute_initial_states drives to Ready (no deps).
        let states = dag.compute_initial_states(&result.newly_inserted);
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].1, DerivationStatus::Ready);
    } else {
        assert!(result.reset_on_resubmit.is_empty());
    }
    Ok(())
}

/// bug_152: the resubmit bound must fire via NATURAL accumulation of
/// poison→resubmit→poison cycles, with NO direct injection of the limit
/// value. Before the `resubmit_cycles` split, `retry.count` was both the
/// per-cycle `max_retries` gate (capped at 2) and the cross-cycle
/// `POISON_RESUBMIT_RETRY_LIMIT` gate (6) — `2 < 6` was permanently
/// true and this loop never terminated.
// r[verify sched.merge.poisoned-resubmit-bounded+2]
#[test]
fn test_poison_resubmit_bound_fires_via_natural_accumulation() -> anyhow::Result<()> {
    use crate::state::POISON_RESUBMIT_RETRY_LIMIT;
    let mut dag = DerivationDag::new();
    let nodes = vec![make_node("nat", "x86_64-linux")];
    dag.merge(Uuid::new_v4(), &nodes, &[], "")?;

    // Cycle 1..=LIMIT: poison (resubmit_cycles untouched — natural
    // poison only sets status), resubmit (merge increments cycles).
    for cycle in 1..=POISON_RESUBMIT_RETRY_LIMIT {
        dag.nodes
            .get_mut("nat")
            .unwrap()
            .set_status_for_test(DerivationStatus::Poisoned);
        let result = dag.merge(Uuid::new_v4(), &nodes, &[], "")?;
        assert!(
            result.newly_inserted.contains("nat"),
            "cycle {cycle}: under limit → reset"
        );
        let n = &dag.nodes["nat"];
        assert_eq!(n.status(), DerivationStatus::Created);
        assert_eq!(
            n.retry.resubmit_cycles, cycle,
            "cycle {cycle}: resubmit_cycles incremented"
        );
        assert_eq!(n.retry.count, 0, "cycle {cycle}: per-cycle count fresh");
    }

    // Cycle LIMIT+1: poison again, resubmit → bound MUST fire.
    dag.nodes
        .get_mut("nat")
        .unwrap()
        .set_status_for_test(DerivationStatus::Poisoned);
    let result = dag.merge(Uuid::new_v4(), &nodes, &[], "")?;
    assert!(
        !result.newly_inserted.contains("nat"),
        "at limit → NOT reset (bound fired)"
    );
    assert_eq!(
        dag.nodes["nat"].status(),
        DerivationStatus::Poisoned,
        "bug_152: bound must fire via natural accumulation; \
         pre-fix retry.count was capped at max_retries=2 < LIMIT=6 → \
         this resubmit would loop forever"
    );
    Ok(())
}

/// Reset-on-resubmit, parent→child cascade cases. `DependencyFailed` is
/// retriable because it's a DERIVED state: reset lets
/// `compute_initial_states` re-evaluate `any_dep_terminally_failed`
/// fresh. Whether the child ALSO resets depends on its prior status +
/// retry count; the parent's re-derived state follows.
#[rstest]
// Worker-side TimedOut → handle_timeout_failure: child Cancelled, parent
// DependencyFailed cascade. Resubmit resets BOTH; child→Ready, parent→Queued.
#[case::timeout_cascade(DerivationStatus::Cancelled, 0, true, DerivationStatus::Queued)]
// I-169: Poisoned with resubmit_cycles < LIMIT resets; resubmit_cycles
// incremented so the bound accumulates; surfaced in reset_on_resubmit for
// db.clear_poison.
// r[verify sched.merge.poisoned-resubmit-bounded+2]
#[case::poisoned_under_limit(DerivationStatus::Poisoned, 1, true, DerivationStatus::Queued)]
// DependencyFailed reset is self-correcting when dep is STILL Poisoned (at
// limit): compute_initial_states re-checks any_dep_terminally_failed and
// re-derives parent as DependencyFailed. Same fast-fail, via reset path.
#[case::depfailed_rederives(
    DerivationStatus::Poisoned,
    crate::state::POISON_RESUBMIT_RETRY_LIMIT,
    false,
    DerivationStatus::DependencyFailed
)]
fn merge_reset_parent_child(
    #[case] child_prior: DerivationStatus,
    #[case] child_retry: u32,
    #[case] expect_child_reset: bool,
    #[case] expect_parent_initial: DerivationStatus,
) -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let nodes = vec![
        make_node("parent", "x86_64-linux"),
        make_node("child", "x86_64-linux"),
    ];
    let edges = vec![make_edge("parent", "child")];
    dag.merge(Uuid::new_v4(), &nodes, &edges, "")?;
    {
        let c = dag.nodes.get_mut("child").unwrap();
        c.set_status_for_test(child_prior);
        c.retry.resubmit_cycles = child_retry;
    }
    dag.nodes
        .get_mut("parent")
        .unwrap()
        .set_status_for_test(DerivationStatus::DependencyFailed);

    let result = dag.merge(Uuid::new_v4(), &nodes, &edges, "")?;

    // Parent always resets (DependencyFailed is retriable → re-evaluate).
    assert!(result.newly_inserted.contains("parent"));
    assert_eq!(result.newly_inserted.contains("child"), expect_child_reset);
    if expect_child_reset {
        assert_eq!(dag.nodes["child"].status(), DerivationStatus::Created);
        // resubmit_cycles incremented so the I-169 bound accumulates;
        // retry.count reset to 0 → fresh per-cycle max_retries budget.
        assert_eq!(dag.nodes["child"].retry.resubmit_cycles, child_retry + 1);
        assert_eq!(dag.nodes["child"].retry.count, 0);
        if child_prior == DerivationStatus::Poisoned {
            assert!(
                result.reset_on_resubmit.iter().any(|h| h == "child"),
                "reset Poisoned node surfaced for db.clear_poison"
            );
        }
    } else {
        assert_eq!(dag.nodes["child"].status(), child_prior);
    }

    let states: HashMap<_, _> = dag
        .compute_initial_states(&result.newly_inserted)
        .into_iter()
        .collect();
    assert_eq!(states["parent"], expect_parent_initial);
    if expect_child_reset {
        assert_eq!(states["child"], DerivationStatus::Ready);
    }
    Ok(())
}

#[test]
fn test_initial_states_with_prepoisoned_dep() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let build1 = Uuid::new_v4();

    // Build 1: just the leaf.
    let leaf_nodes = vec![make_node("leafP", "x86_64-linux")];
    dag.merge(build1, &leaf_nodes, &[], "")?;

    // Poison it (at the resubmit-retry limit so re-merge doesn't reset it).
    {
        let leaf = dag.nodes.get_mut("leafP").expect("leafP");
        leaf.set_status_for_test(DerivationStatus::Poisoned);
        leaf.retry.resubmit_cycles = crate::state::POISON_RESUBMIT_RETRY_LIMIT;
    }

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
    // B is resident and not re-created, so the creation-scoped edge gate
    // would skip the new edge; mark it topdown-pruned (the carve-out that
    // legitimately admits dependency top-ups onto a resident parent) so
    // the dfs-from-pre-existing-parent cycle coverage stays exercised.
    dag.nodes.get_mut("hashB").unwrap().topdown_pruned = true;
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

    // Step 2: merge B1 again with nodes {A, C, D} and a C->D->C cycle
    // among the NEWLY-inserted nodes (A is resident and not re-created,
    // so the creation-scoped edge gate would skip edges parented on it).
    // Regression guard: rollback must not clear B1 from A even though
    // B1 was already interested in A from step 1.
    let nodes_acd = vec![
        make_node("hashA", "x86_64-linux"),
        make_node("hashC", "x86_64-linux"),
        make_node("hashD", "x86_64-linux"),
    ];
    let cycle_edges = vec![make_edge("hashC", "hashD"), make_edge("hashD", "hashC")];
    let result = dag.merge(b1, &nodes_acd, &cycle_edges, "");
    assert!(result.is_err(), "cycle should be rejected");

    // Step 3: A should STILL have B1 interest (was present before the
    // failed merge). C and D should be gone entirely (newly inserted).
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
    assert!(
        !dag.nodes.contains_key("hashD"),
        "newly-inserted D should be rolled back"
    );
    Ok(())
}

/// Resubmit-reset destructively removes a retriable node before
/// validation. If the merge then fails (cycle), rollback must restore
/// the prior node verbatim — status, interested_builds,
/// retry.resubmit_cycles, path_to_hash entry, AND pre-existing edges
/// keyed on its hash. Without restore, `{retriable-X, cycle}` wipes X
/// and resets its resubmit counter, defeating
/// `POISON_RESUBMIT_RETRY_LIMIT` (I-169).
// r[verify sched.merge.poisoned-resubmit-bounded+2]
#[test]
fn test_cyclic_merge_restores_removed_retriable() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let b1 = Uuid::new_v4();

    // B1: W depends on X. X then fails with retry.resubmit_cycles=1.
    dag.merge(
        b1,
        &[
            make_node("hashW", "x86_64-linux"),
            make_node("hashX", "x86_64-linux"),
        ],
        &[make_edge("hashW", "hashX")],
        "",
    )?;
    {
        let x = dag.node_mut("hashX").expect("hashX");
        x.set_status_for_test(DerivationStatus::Failed);
        x.retry.resubmit_cycles = 1;
    }
    let x_path = test_drv_path("hashX");

    // B2: resubmit X plus a cycle A↔B. Resubmit-reset removes X before
    // the cycle is detected; rollback must restore it.
    let b2 = Uuid::new_v4();
    let result = dag.merge(
        b2,
        &[
            make_node("hashX", "x86_64-linux"),
            make_node("hashA", "x86_64-linux"),
            make_node("hashB", "x86_64-linux"),
        ],
        &[make_edge("hashA", "hashB"), make_edge("hashB", "hashA")],
        "",
    );
    assert!(matches!(result, Err(DagError::CycleDetected)));

    // X restored exactly as it was.
    let x = dag.node("hashX").expect("hashX restored after rollback");
    assert_eq!(
        x.status(),
        DerivationStatus::Failed,
        "prior status restored"
    );
    assert_eq!(
        x.retry.resubmit_cycles, 1,
        "retry.resubmit_cycles toward poison-limit preserved"
    );
    assert_eq!(
        x.interested_builds,
        HashSet::from([b1]),
        "prior interest restored, b2 NOT added"
    );
    assert_eq!(
        dag.hash_for_path(&x_path).map(|h| h.as_str()),
        Some("hashX"),
        "path_to_hash entry restored"
    );
    // Pre-existing W→X edge intact in BOTH directions.
    assert!(
        dag.children
            .get("hashW")
            .is_some_and(|c| c.contains("hashX")),
        "children[W]∋X survives"
    );
    assert!(
        dag.parents
            .get("hashX")
            .is_some_and(|p| p.contains("hashW")),
        "parents[X]∋W survives (not over-scrubbed by newly_inserted cleanup)"
    );
    // Cycle nodes fully rolled back.
    assert!(!dag.nodes.contains_key("hashA"));
    assert!(!dag.nodes.contains_key("hashB"));
    Ok(())
}

/// `InvalidDrvPath` mid-loop must roll back EVERYTHING the merge has
/// touched so far: earlier-iteration fresh inserts, interest added to
/// pre-existing nodes, and removed retriable nodes. Previously the `?`
/// dropped all rollback state on the floor.
// r[verify sched.merge.poisoned-resubmit-bounded+2]
#[test]
fn test_invalid_drv_path_rolls_back_everything() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let b1 = Uuid::new_v4();

    // Pre-seed: GOOD-PRE (will gain b2 interest) and X (Failed, retry=2).
    dag.merge(
        b1,
        &[
            make_node("good-pre", "x86_64-linux"),
            make_node("hashX", "x86_64-linux"),
        ],
        &[],
        "",
    )?;
    {
        let x = dag.node_mut("hashX").expect("hashX");
        x.set_status_for_test(DerivationStatus::Failed);
        x.retry.resubmit_cycles = 1;
    }

    // B2: [good-pre (interest), X (resubmit-reset), GOOD-NEW (fresh), BAD].
    // BAD has an unparseable drv_path → InvalidDrvPath after the first
    // three iterations have already mutated state.
    let b2 = Uuid::new_v4();
    let result = dag.merge(
        b2,
        &[
            make_node("good-pre", "x86_64-linux"),
            make_node("hashX", "x86_64-linux"),
            make_node("good-new", "x86_64-linux"),
            make_node_with_path("bad", "not-a-store-path", "x86_64-linux"),
        ],
        &[],
        "",
    );
    assert!(matches!(result, Err(DagError::InvalidDrvPath { .. })));

    // X restored verbatim.
    let x = dag.node("hashX").expect("hashX restored");
    assert_eq!(x.status(), DerivationStatus::Failed);
    assert_eq!(x.retry.resubmit_cycles, 1);
    assert_eq!(x.interested_builds, HashSet::from([b1]));
    // Earlier-iteration fresh insert rolled back.
    assert!(
        !dag.nodes.contains_key("good-new"),
        "fresh node from earlier loop iteration must be rolled back"
    );
    // Interest added to pre-existing node reverted.
    assert_eq!(
        dag.node("good-pre").expect("good-pre").interested_builds,
        HashSet::from([b1]),
        "b2 interest in pre-existing node must be reverted"
    );
    assert!(!dag.nodes.contains_key("bad"));
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
    // hashA is resident and not re-created, so its half of the cycle is
    // only admissible through the topdown-pruned carve-out
    // (sched.merge.edge-creation-scoped).
    dag.nodes.get_mut("hashA").unwrap().topdown_pruned = true;
    let cycle_nodes = vec![
        make_node("hashA", "x86_64-linux"),
        make_node("hashC", "x86_64-linux"),
    ];
    let cycle_edges = vec![make_edge("hashA", "hashC"), make_edge("hashC", "hashA")];
    dag.merge(b1, &cycle_nodes, &cycle_edges, "").unwrap_err();
    dag.nodes.get_mut("hashA").unwrap().topdown_pruned = false;
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
    assert_eq!(
        reaped.reaped_paths,
        vec![p_a.clone()],
        "hashA should be reaped (terminal, no interest); returns drv_path for log-buffer discard"
    );
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

// ---------------------------------------------------------------------------
// Large-DAG perf bound (I-139)
// ---------------------------------------------------------------------------

/// Build a synthetic wide DAG: `n` nodes, each node `i` (for `i >= fanout`)
/// depends on `fanout` earlier nodes `i-1..i-fanout`. ~`n*fanout` edges.
fn make_wide_dag(n: usize, fanout: usize) -> (Vec<DerivationNode>, Vec<DerivationEdge>) {
    let path = |i: usize| format!("/nix/store/{i:032}-n{i}.drv");
    let nodes: Vec<_> = (0..n)
        .map(|i| make_node_with_path(&format!("h{i:08}"), &path(i), "x86_64-linux"))
        .collect();
    let mut edges = Vec::with_capacity(n * fanout);
    for i in fanout..n {
        for j in 1..=fanout {
            edges.push(make_edge_with_paths(&path(i), &path(i - j)));
        }
    }
    (nodes, edges)
}

/// I-139: in-memory `merge()` + `compute_initial_states()` must stay
/// sub-linear-ish on a 100k-node / ~500k-edge DAG. The original report
/// was 153k nodes / 837k edges → >300s end-to-end; this asserts the
/// in-memory phase isn't the bottleneck (it should be well under 10s
/// even in debug). If THIS test fails, the bug is in `dag/mod.rs`. If
/// it passes but `handle_merge_dag` is still slow, the bug is in the
/// actor wrapper (per-node DB round-trips).
///
/// Release-mode `cargo test -p rio-scheduler --release merge_large_dag`
/// to get a representative number; debug is ~5-10× slower but the
/// 10s bound has plenty of headroom either way.
#[test]
fn test_merge_large_dag_perf_bound() -> anyhow::Result<()> {
    const N: usize = 100_000;
    const FANOUT: usize = 5;
    let (nodes, edges) = make_wide_dag(N, FANOUT);
    assert_eq!(edges.len(), (N - FANOUT) * FANOUT);

    let mut dag = DerivationDag::new();
    let build_id = Uuid::new_v4();

    let t = std::time::Instant::now();
    let result = dag.merge(build_id, &nodes, &edges, "")?;
    let merge_elapsed = t.elapsed();

    let t = std::time::Instant::now();
    let states = dag.compute_initial_states(&result.newly_inserted);
    let cis_elapsed = t.elapsed();

    eprintln!(
        "I-139 bench: {N} nodes / {} edges — merge {merge_elapsed:?}, \
         compute_initial_states {cis_elapsed:?}",
        edges.len()
    );

    assert_eq!(result.newly_inserted.len(), N);
    assert_eq!(states.len(), N);
    assert!(
        merge_elapsed.as_secs() < 10,
        "in-memory merge of {N} nodes took {merge_elapsed:?} (>10s); \
         O(n²) regression in dag::merge"
    );
    assert!(
        cis_elapsed.as_secs() < 10,
        "compute_initial_states on {N} nodes took {cis_elapsed:?} (>10s)"
    );
    Ok(())
}

/// I-140: time the per-completion / per-admin-RPC hot operations at the
/// 153k-node scale that stalled dispatch in prod. These are all in-memory;
/// the bound is "no accidental O(n²)". Prints raw timings for diagnosis.
#[test]
fn test_large_dag_hot_ops_perf_bound() -> anyhow::Result<()> {
    const N: usize = 150_000;
    const FANOUT: usize = 5;
    let (nodes, edges) = make_wide_dag(N, FANOUT);

    let mut dag = DerivationDag::new();
    let build_id = Uuid::new_v4();
    let result = dag.merge(build_id, &nodes, &edges, "")?;
    // Mark first FANOUT nodes Completed (leaves) so find_newly_ready /
    // update_ancestors have realistic work to do.
    for i in 0..FANOUT {
        let h = format!("h{i:08}");
        dag.node_mut(&h)
            .unwrap()
            .set_status_for_test(DerivationStatus::Completed);
    }
    // Put remaining nodes in Queued (compute_initial_states would do this).
    for i in FANOUT..N {
        let h = format!("h{i:08}");
        dag.node_mut(&h)
            .unwrap()
            .set_status_for_test(DerivationStatus::Queued);
    }

    let sla = crate::sla::SlaEstimator::new(&crate::sla::config::SlaConfig::test_default());
    let builds = std::collections::HashMap::new();

    macro_rules! time {
        ($name:literal, $bound_ms:literal, $body:expr) => {{
            let t = std::time::Instant::now();
            let r = $body;
            let el = t.elapsed();
            eprintln!("I-140 bench [{}]: {:?}", $name, el);
            assert!(
                el.as_millis() < $bound_ms,
                "I-140: {} took {:?} on {N}-node DAG (>{}ms bound)",
                $name,
                el,
                $bound_ms
            );
            r
        }};
    }

    time!("build_summary", 500, dag.build_summary(build_id));
    time!("find_newly_ready", 100, dag.find_newly_ready("h00000000"));
    time!("iter_nodes-count", 500, dag.iter_nodes().count());
    time!(
        "update_ancestors",
        2000,
        crate::critical_path::update_ancestors(&mut dag, "h00000000")
    );
    time!(
        "compute_initial(critpath)",
        15000,
        crate::critical_path::compute_initial(&mut dag, &sla, &builds, &result.newly_inserted)
    );
    time!(
        "full_sweep",
        15000,
        crate::critical_path::full_sweep(&mut dag, &sla, &builds)
    );

    // update_ancestors when the completed node WAS the unique max-child
    // (priority strictly higher than siblings) — the walk reaches the
    // full DAG depth. Set node 0's priority above its siblings,
    // propagate up, then complete it and propagate the drop back up.
    //
    // Node 0 was set Completed above (i=0 in the FANOUT loop) — flip it
    // back to Queued first, otherwise the priority bump is invisible
    // (terminal children are filtered) and both timed cases below
    // measure a depth-1 no-op.
    dag.node_mut("h00000000")
        .unwrap()
        .set_status_for_test(DerivationStatus::Queued);
    dag.node_mut("h00000000").unwrap().sched.priority = 1e9;
    time!(
        "update_ancestors(propagate-up)",
        15000,
        crate::critical_path::update_ancestors(&mut dag, "h00000000")
    );
    dag.node_mut("h00000000")
        .unwrap()
        .set_status_for_test(DerivationStatus::Completed);
    time!(
        "update_ancestors(deep)",
        15000,
        crate::critical_path::update_ancestors(&mut dag, "h00000000")
    );

    // remove_build_interest_and_reap on a sole-interest build with all
    // nodes terminal: K reaps × O(degree) each ≈ O(E). Regression guard
    // for the O(K×N) `values_mut()` full-scan in `remove_node` (~2e10
    // ops at this scale → would blow well past 15s).
    for i in FANOUT..N {
        let h = format!("h{i:08}");
        dag.node_mut(&h)
            .unwrap()
            .set_status_for_test(DerivationStatus::Completed);
    }
    let reaped = time!(
        "reap-all",
        2000,
        dag.remove_build_interest_and_reap(build_id)
    );
    assert_eq!(
        reaped.reaped_paths.len(),
        N,
        "all sole-interest terminal nodes reaped"
    );
    Ok(())
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
// BuildSummary: critpath_remaining + assigned_executors (P0270)
// ---------------------------------------------------------------------------

/// Walk a node through Created→Queued→Ready→Assigned→Running. The state
/// machine is strict; each intermediate is required.
fn advance_to_running(dag: &mut DerivationDag, hash: &str, worker: &str) {
    let n = dag.node_mut(hash).expect(hash);
    n.transition(DerivationStatus::Queued).unwrap();
    n.transition(DerivationStatus::Ready).unwrap();
    n.transition(DerivationStatus::Assigned).unwrap();
    n.transition(DerivationStatus::Running).unwrap();
    n.assigned_executor = Some(worker.into());
}

/// Plan doc T3: 2 running + 1 queued → assigned_executors.len() == 2.
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
        s.assigned_executors,
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
    dag.node_mut("big").unwrap().sched.priority = 100.0;
    dag.node_mut("small").unwrap().sched.priority = 5.0;

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
        s.assigned_executors.is_empty(),
        "completed node's assigned_executor is cleared by the real transition path, \
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

    dag.node_mut("a-drv").unwrap().sched.priority = 10.0;
    dag.node_mut("b-drv").unwrap().sched.priority = 999.0;

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
    assert!(s.assigned_executors.is_empty());
    assert_eq!(s.queued, 1);
    // critpath still reflects the queued node — it's non-terminal.
    // Default priority is 0.0 (we didn't set it), so critpath is 0.
    // That's correct: no estimate = 0s ETA. In practice compute_initial
    // would have set a real value.
    assert_eq!(s.critpath_remaining, 0.0);

    Ok(())
}

// ---------------------------------------------------------------------------
// CA early-cutoff cascade (P0252)
// ---------------------------------------------------------------------------

/// Build a linear chain of `n` nodes where node[0] depends on nothing
/// and node[i+1] depends on node[i]. Returns the DAG with all nodes
/// `Queued` except node[0] which is `Completed` (the CA trigger).
///
/// Edge direction: parent→child means parent DEPENDS ON child. So
/// node[1] is the parent of node[0] (node[1] needs node[0]).
fn chain_dag(n: usize) -> DerivationDag {
    let mut dag = DerivationDag::new();
    let build = Uuid::new_v4();
    let nodes: Vec<_> = (0..n)
        .map(|i| {
            make_node_with_path(
                &format!("h{i:05}"),
                &format!("/nix/store/{i:032}-n{i}.drv"),
                "x86_64-linux",
            )
        })
        .collect();
    // node[i+1] depends on node[i]: parent=i+1, child=i.
    let edges: Vec<_> = (0..n - 1)
        .map(|i| {
            make_edge_with_paths(
                &format!("/nix/store/{:032}-n{}.drv", i + 1, i + 1),
                &format!("/nix/store/{i:032}-n{i}.drv"),
            )
        })
        .collect();
    dag.merge(build, &nodes, &edges, "").unwrap();
    // Node 0: Completed (the CA trigger). Nodes 1..n: Queued.
    dag.node_mut("h00000")
        .unwrap()
        .set_status_for_test(DerivationStatus::Completed);
    for i in 1..n {
        dag.node_mut(&format!("h{i:05}"))
            .unwrap()
            .set_status_for_test(DerivationStatus::Queued);
    }
    dag
}

// r[verify sched.ca.cutoff-propagate+2]
/// A→B→C chain: A completes with unchanged CA output. Cascade skips
/// B (only incomplete dep was A), then C (only incomplete dep was B).
/// Neither ran; both end Skipped.
#[test]
fn ca_cutoff_cascades_through_chain() {
    let mut dag = chain_dag(3);
    // Preconditions.
    assert_eq!(
        dag.node("h00000").unwrap().status(),
        DerivationStatus::Completed
    );
    assert_eq!(
        dag.node("h00001").unwrap().status(),
        DerivationStatus::Queued
    );
    assert_eq!(
        dag.node("h00002").unwrap().status(),
        DerivationStatus::Queued
    );

    let (skipped, cap_hit) = dag.cascade_cutoff("h00000", |_| true);

    assert!(!cap_hit, "3-node chain is nowhere near the cap");
    assert_eq!(skipped.len(), 2, "B and C both skipped");
    // Order is stack-based (LIFO): B first, then C.
    assert_eq!(skipped[0], "h00001");
    assert_eq!(skipped[1], "h00002");
    assert_eq!(
        dag.node("h00001").unwrap().status(),
        DerivationStatus::Skipped
    );
    assert_eq!(
        dag.node("h00002").unwrap().status(),
        DerivationStatus::Skipped
    );
    // A stays Completed (trigger, not skipped).
    assert_eq!(
        dag.node("h00000").unwrap().status(),
        DerivationStatus::Completed
    );
}

// r[verify sched.preempt.never-running]
/// A→B: A completes unchanged, but B is already Running. CA cutoff
/// must NOT touch it. Running builds complete on their own; wasted
/// CPU but correct output.
#[test]
fn ca_cutoff_skips_running() {
    let mut dag = chain_dag(2);
    // B is Running (worker already picked it up before A completed).
    dag.node_mut("h00001")
        .unwrap()
        .set_status_for_test(DerivationStatus::Running);

    let (skipped, cap_hit) = dag.cascade_cutoff("h00000", |_| true);

    assert!(!cap_hit);
    assert_eq!(skipped.len(), 0, "Running node NEVER skipped");
    assert_eq!(
        dag.node("h00001").unwrap().status(),
        DerivationStatus::Running,
        "r[sched.preempt.never-running]: Running stays Running"
    );
}

// r[verify sched.ca.cutoff-propagate+2]
/// A has two deps: B (CA, completes unchanged) and C (still Queued).
/// A is NOT eligible — it has another incomplete dep. Only when ALL
/// deps are terminal can we skip.
#[test]
fn ca_cutoff_not_eligible_with_incomplete_sibling() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let build = Uuid::new_v4();
    dag.merge(
        build,
        &[
            make_node("A", "x86_64-linux"),
            make_node("B", "x86_64-linux"),
            make_node("C", "x86_64-linux"),
        ],
        &[make_edge("A", "B"), make_edge("A", "C")],
        "",
    )?;
    dag.node_mut("A")
        .unwrap()
        .set_status_for_test(DerivationStatus::Queued);
    dag.node_mut("B")
        .unwrap()
        .set_status_for_test(DerivationStatus::Completed);
    dag.node_mut("C")
        .unwrap()
        .set_status_for_test(DerivationStatus::Queued);

    let (skipped, _) = dag.cascade_cutoff("B", |_| true);
    assert_eq!(
        skipped.len(),
        0,
        "A has incomplete dep C → not eligible for cutoff"
    );
    assert_eq!(
        dag.node("A").unwrap().status(),
        DerivationStatus::Queued,
        "A stays Queued (C is still incomplete)"
    );
    Ok(())
}

// r[verify sched.ca.cutoff-propagate+2]
/// Depth cap: chain of MAX_CASCADE_NODES+2 nodes (1 trigger +
/// MAX_CASCADE_NODES+1 Queued). The cascade processes MAX_CASCADE_NODES
/// iterations, skipping MAX_CASCADE_NODES nodes; the (MAX+1)th stays
/// Queued and cap_hit=true.
#[test]
fn ca_cutoff_depth_cap() {
    // 1 completed trigger + (MAX+1) queued = MAX+2 total.
    // Iterations 0..MAX-1 skip nodes 1..MAX (= MAX nodes).
    // Iteration MAX hits the cap → node MAX+1 stays Queued.
    let n = MAX_CASCADE_NODES + 2;
    let mut dag = chain_dag(n);

    let (skipped, cap_hit) = dag.cascade_cutoff("h00000", |_| true);

    assert!(cap_hit, "chain of {n} should hit depth cap");
    assert_eq!(
        skipped.len(),
        MAX_CASCADE_NODES,
        "exactly MAX_CASCADE_NODES nodes skipped before cap"
    );
    // Last skipped: node[MAX_CASCADE_NODES].
    assert_eq!(
        dag.node(&format!("h{MAX_CASCADE_NODES:05}"))
            .unwrap()
            .status(),
        DerivationStatus::Skipped,
        "node at depth cap was skipped"
    );
    // Node beyond cap: stays Queued.
    let beyond = MAX_CASCADE_NODES + 1;
    assert_eq!(
        dag.node(&format!("h{beyond:05}")).unwrap().status(),
        DerivationStatus::Queued,
        "node beyond cap stays Queued (cascade truncated)"
    );
}

// r[verify sched.ca.cutoff-propagate+2]
/// Defensive guard: the verify closure gates which nodes are
/// Skipped. A node that fails verification (output doesn't exist
/// in store) is NOT skipped AND the cascade doesn't continue
/// through it — its descendants stay Queued.
///
/// This is the bughunt-mc196 self-match defense: ca_output_unchanged
/// can be true for a first-ever build (PutPath inserts content_index
/// BEFORE BuildComplete). Without verify, downstream never-built
/// nodes would be Skipped.
#[test]
fn ca_cutoff_verify_gates_cascade() {
    let mut dag = chain_dag(4);
    // A(h00000)=Completed, B(h00001)/C(h00002)/D(h00003)=Queued.
    // verify rejects C → only B is Skipped; C and D stay Queued.
    let (skipped, _) = dag.cascade_cutoff("h00000", |h| h.as_str() != "h00002");
    assert_eq!(
        skipped,
        vec!["h00001".to_string()],
        "only B skipped; C failed verify so cascade stops there"
    );
    assert_eq!(
        dag.node("h00001").unwrap().status(),
        DerivationStatus::Skipped
    );
    assert_eq!(
        dag.node("h00002").unwrap().status(),
        DerivationStatus::Queued,
        "C failed verify → NOT skipped"
    );
    assert_eq!(
        dag.node("h00003").unwrap().status(),
        DerivationStatus::Queued,
        "D depends on unverified C → cascade didn't reach it"
    );
}

// r[verify sched.ca.cutoff-propagate+2]
/// `find_cutoff_eligible_speculative` with NON-EMPTY `provisional_
/// skipped` set. The non-empty path (the OR-branch `provisional_
/// skipped.contains(d)` in the all-deps-terminal check) is only
/// reachable from the private `verify_cutoff_candidates` speculative
/// walk — `cascade_cutoff` uses empty set for the initial iteration, then
/// an empty set. Without this test, an inverted-contains bug (or a
/// typo that checks `!provisional_skipped.contains(d)`) is invisible.
///
/// Scenario: chain A→B→C. A Completed, B+C Queued.
///   - speculative(A, {})  → [B]   (B's only dep A is terminal)
///   - speculative(B, {B}) → [C]   (C's only dep B is provisional)
///
/// With the OR-branch inverted, the second call returns [] and the
/// cascade's batch-verification prewalk misses C entirely → C never
/// included in the FindMissingPaths batch → never verified → never
/// Skipped (silent cascade truncation).
#[test]
fn speculative_provisional_skipped_makes_parent_eligible() {
    let dag = chain_dag(3);
    // Preconditions from chain_dag: A=h00000 Completed,
    // B=h00001 Queued, C=h00002 Queued.
    assert_eq!(
        dag.node("h00000").unwrap().status(),
        DerivationStatus::Completed
    );

    // Empty provisional set → same as the non-speculative walk.
    let step1 = dag.find_cutoff_eligible_speculative("h00000", &HashSet::new());
    assert_eq!(
        step1,
        vec!["h00001".to_string()],
        "B eligible: only dep A is Completed (terminal)"
    );

    // NON-EMPTY provisional: B speculated-as-Skipped.
    let provisional: HashSet<DrvHash> = ["h00001".into()].into_iter().collect();
    let step2 = dag.find_cutoff_eligible_speculative("h00001", &provisional);
    assert_eq!(
        step2,
        vec!["h00002".to_string()],
        "C eligible: only dep B is in provisional_skipped \
         (the OR-branch — inverted-contains bug returns [] here)"
    );

    // Sanity: provisional set EXCLUDES nodes from the candidate list
    // (B should NOT appear in step2 — it's speculated-as-Skipped, so
    // it's a "parent of" target, not a target itself).
    assert!(
        !step2.iter().any(|h| h.as_str() == "h00001"),
        "provisional-skipped nodes are excluded from eligible list"
    );
}

/// Verify closure rejects ALL → nothing Skipped. Simulates a
/// first-ever build where no downstream outputs exist in store.
#[test]
fn ca_cutoff_verify_rejects_all() {
    let mut dag = chain_dag(3);
    let (skipped, cap_hit) = dag.cascade_cutoff("h00000", |_| false);
    assert_eq!(skipped.len(), 0, "nothing verified → nothing skipped");
    assert!(!cap_hit);
    assert_eq!(
        dag.node("h00001").unwrap().status(),
        DerivationStatus::Queued
    );
    assert_eq!(
        dag.node("h00002").unwrap().status(),
        DerivationStatus::Queued
    );
}

/// Skipped counts as completed in build_summary: a build where
/// everything is Skipped should look fully completed to
/// check_build_completion.
#[test]
fn build_summary_skipped_counts_as_completed() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let build = Uuid::new_v4();
    dag.merge(
        build,
        &[
            make_node("done", "x86_64-linux"),
            make_node("skip", "x86_64-linux"),
        ],
        &[],
        "",
    )?;
    dag.node_mut("done")
        .unwrap()
        .set_status_for_test(DerivationStatus::Completed);
    dag.node_mut("skip")
        .unwrap()
        .set_status_for_test(DerivationStatus::Skipped);

    let s = dag.build_summary(build);
    assert_eq!(s.completed, 2, "Skipped counts in completed bucket");
    assert_eq!(s.failed, 0);
    assert_eq!(s.queued, 0);
    assert_eq!(s.total, 2);
    Ok(())
}

// r[verify sched.ca.cutoff-propagate+2]
/// H1 regression (P0399): verify-rejected parent of Skipped must be
/// Ready-promotable, not stuck Queued.
///
/// Chain A→B→C: A completes unchanged. Cascade verifies B (output in
/// store) → B Skipped. Cascade REJECTS C (output NOT in store) → C
/// stays Queued. C's only dep is B (now Skipped). Without the fix,
/// `find_newly_ready(B)` returns [] because `all_deps_completed(C)`
/// checked `== Completed` only. With the fix (matches!
/// Completed|Skipped), it returns [C].
///
/// The completion handler runs `find_newly_ready` per-Skipped after
/// the cascade (T2), so C is promoted to Ready instead of hanging
/// Queued forever.
#[test]
fn cascade_rejected_parent_promoted_not_stuck() {
    let mut dag = chain_dag(3);
    // A=h00000 Completed, B=h00001 Queued, C=h00002 Queued.
    // Verify accepts B only — C's output does NOT exist in store
    // (first-build guard; bughunt-mc196).
    let (skipped, _) = dag.cascade_cutoff("h00000", |h| h.as_str() == "h00001");

    assert_eq!(
        skipped,
        vec!["h00001".to_string()],
        "only B skipped; C rejected by verify"
    );
    assert_eq!(
        dag.node("h00001").unwrap().status(),
        DerivationStatus::Skipped
    );
    assert_eq!(
        dag.node("h00002").unwrap().status(),
        DerivationStatus::Queued,
        "C stays Queued after cascade (verify rejected)"
    );

    // H1 CORE ASSERTION: find_newly_ready from the Skipped node
    // must return C. Pre-fix, all_deps_completed(C) was false
    // (B's status Skipped != Completed) → [] → C stuck forever.
    let ready = dag.find_newly_ready("h00001");
    assert_eq!(
        ready,
        vec!["h00002".to_string()],
        "C must be Ready-promotable: only dep B is Skipped (output-equivalent)"
    );

    // The completion-handler loop (T2) would now transition C.
    // Simulate it here at the DAG level:
    for s in &skipped {
        for r in dag.find_newly_ready(s) {
            dag.node_mut(&r)
                .unwrap()
                .transition(DerivationStatus::Ready)
                .unwrap();
        }
    }
    assert_eq!(
        dag.node("h00002").unwrap().status(),
        DerivationStatus::Ready,
        "post-loop: C is Ready, not stuck Queued"
    );
}

// r[verify sched.merge.dedup+2]
/// H2 regression (P0399): merge a new node X depending on
/// pre-existing Skipped Y. compute_initial_states must return X as
/// Ready, not Queued.
///
/// Pre-fix, all_deps_completed(X) was false (Y Skipped != Completed)
/// → X goes Queued. Y is terminal; no event ever calls
/// find_newly_ready(Y) for X → stuck forever.
#[test]
fn merge_new_node_depending_on_skipped_goes_ready() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let build1 = Uuid::new_v4();

    // Build 1: Y alone.
    dag.merge(build1, &[make_node("Y", "x86_64-linux")], &[], "")?;
    // Y is Skipped (from a prior cascade in build 1).
    dag.node_mut("Y")
        .unwrap()
        .set_status_for_test(DerivationStatus::Skipped);

    // Build 2: X depending on pre-existing Y.
    let build2 = Uuid::new_v4();
    let newly = dag
        .merge(
            build2,
            &[
                make_node("X", "x86_64-linux"),
                make_node("Y", "x86_64-linux"),
            ],
            &[make_edge("X", "Y")],
            "",
        )?
        .newly_inserted;
    // Dedup: Y already exists, only X is newly inserted.
    assert_eq!(newly, HashSet::from(["X".into()]));

    // H2 CORE ASSERTION: X must be Ready, not Queued. Its only
    // dep Y is Skipped (output-equivalent; CA cutoff verified the
    // output exists in the store).
    let transitions = dag.compute_initial_states(&newly);
    assert_eq!(transitions.len(), 1);
    assert_eq!(transitions[0].0, "X");
    assert_eq!(
        transitions[0].1,
        DerivationStatus::Ready,
        "X with Skipped-only dep must go Ready (pre-fix went Queued → hang)"
    );
    Ok(())
}

/// Negative guard: all_deps_completed must NOT accept
/// failure-terminal states (Poisoned/DependencyFailed/Cancelled).
/// Those are terminal but their outputs do NOT exist in the store.
/// A node depending on them must cascade DependencyFailed via
/// any_dep_terminally_failed, NOT go Ready.
///
/// Ensures T1 didn't over-widen to `is_terminal()`.
#[test]
fn all_deps_completed_rejects_failure_terminal() -> anyhow::Result<()> {
    for bad_status in [
        DerivationStatus::Poisoned,
        DerivationStatus::DependencyFailed,
        DerivationStatus::Cancelled,
    ] {
        let mut dag = DerivationDag::new();
        let build = Uuid::new_v4();
        dag.merge(
            build,
            &[
                make_node("X", "x86_64-linux"),
                make_node("Y", "x86_64-linux"),
            ],
            &[make_edge("X", "Y")],
            "",
        )?;
        dag.node_mut("Y").unwrap().set_status_for_test(bad_status);

        assert!(
            !dag.all_deps_completed("X"),
            "{bad_status:?} dep → all_deps_completed must be FALSE \
             (output unavailable; X should cascade DependencyFailed, not go Ready)"
        );
    }

    // Positive control: Completed and Skipped DO satisfy.
    for ok_status in [DerivationStatus::Completed, DerivationStatus::Skipped] {
        let mut dag = DerivationDag::new();
        let build = Uuid::new_v4();
        dag.merge(
            build,
            &[
                make_node("X", "x86_64-linux"),
                make_node("Y", "x86_64-linux"),
            ],
            &[make_edge("X", "Y")],
            "",
        )?;
        dag.node_mut("Y").unwrap().set_status_for_test(ok_status);

        assert!(
            dag.all_deps_completed("X"),
            "{ok_status:?} dep → all_deps_completed must be TRUE (output available)"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// find_roots: build-scoped parent filter (bug_022)
// ---------------------------------------------------------------------------

// r[verify sched.dag.build-scoped-roots]
/// `find_roots(build_id)` must scope the parent check to parents
/// interested in THAT build. A derivation that's a root for build X
/// but has a parent from build Y (merged DAG) is still X's root.
///
/// Scenario:
///   Build X: {shared}                    — shared is X's root (no parent in X)
///   Build Y: {parent_y → shared}         — shared is NOT Y's root (parent_y depends on it)
///
/// Old unscoped check: shared has global parent parent_y → not a
/// root for ANYONE → find_roots(X) returns [] → X stalls.
#[test]
fn test_find_roots_build_scoped() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let build_x = Uuid::new_v4();
    let build_y = Uuid::new_v4();

    // Build X: just "shared". shared is X's root.
    dag.merge(build_x, &[make_node("shared", "x86_64-linux")], &[], "")?;

    // Build Y: parent_y → shared. shared is NOT Y's root.
    dag.merge(
        build_y,
        &[
            make_node("parent_y", "x86_64-linux"),
            make_node("shared", "x86_64-linux"),
        ],
        &[make_edge("parent_y", "shared")],
        "",
    )?;

    // X's roots: {shared}. parent_y is NOT interested in X → doesn't
    // disqualify shared as X's root.
    let roots_x = dag.find_roots(build_x);
    assert_eq!(
        roots_x.len(),
        1,
        "X should have exactly 1 root (shared); got {roots_x:?}"
    );
    assert!(
        roots_x.iter().any(|h| h == "shared"),
        "shared must be X's root despite having parent_y in the global DAG"
    );

    // Y's roots: {parent_y}. shared has parent_y (interested in Y)
    // → not Y's root.
    let roots_y = dag.find_roots(build_y);
    assert_eq!(
        roots_y.len(),
        1,
        "Y should have exactly 1 root (parent_y); got {roots_y:?}"
    );
    assert!(
        roots_y.iter().any(|h| h == "parent_y"),
        "parent_y must be Y's root"
    );
    assert!(
        !roots_y.iter().any(|h| h == "shared"),
        "shared has Y-interested parent → not Y's root"
    );

    Ok(())
}

/// Reap must remove an orphaned terminal node even if a prior
/// `remove_build_interest` already stripped this build_id.
///
/// `cancel_build_derivations` calls `remove_build_interest` (for ready-
/// queue cleanup) before `handle_cleanup_terminal_build` calls
/// `remove_build_interest_and_reap`. The previous `was_interested`
/// guard saw `interested_builds.remove(&b)` return false (already
/// removed) and reaped nothing — every cancelled/fail-fast/timed-out
/// build leaked its sole-interest nodes for the process lifetime.
#[test]
fn test_reap_after_prior_remove_interest() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let b = Uuid::new_v4();
    dag.merge(b, &[make_node("reap-h", "x86_64-linux")], &[], "")?;
    dag.node_mut("reap-h")
        .unwrap()
        .set_status_for_test(DerivationStatus::Cancelled);

    // Prior strip — interested_builds is now ∅ but node still in DAG.
    let _ = dag.remove_build_interest(b);
    assert!(dag.nodes.contains_key("reap-h"));

    let reaped = dag.remove_build_interest_and_reap(b);
    assert_eq!(
        reaped.reaped_paths.len(),
        1,
        "reap must be idempotent w.r.t. prior interest-strip"
    );
    assert!(!dag.nodes.contains_key("reap-h"));
    Ok(())
}

/// Reap must NOT remove a Poisoned node with `interested_builds=∅`.
///
/// Recovered-poisoned nodes (`from_poisoned_row`) have empty
/// `interested_builds` from birth and are TTL-tracked. Reaping them on
/// the first build completion post-recovery would silently disable
/// poison-TTL. Regression guard for the explicit `!= Poisoned`
/// exclusion that replaced the `was_interested` side-effect guard.
#[test]
fn test_reap_preserves_poisoned() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let b = Uuid::new_v4();
    dag.merge(b, &[make_node("poison-h", "x86_64-linux")], &[], "")?;
    {
        let n = dag.node_mut("poison-h").unwrap();
        n.set_status_for_test(DerivationStatus::Poisoned);
        n.interested_builds.clear(); // recovered-poisoned shape
    }

    let reaped = dag.remove_build_interest_and_reap(Uuid::new_v4());
    assert!(
        reaped.reaped_paths.is_empty(),
        "Poisoned nodes are TTL-tracked, never reaped here"
    );
    assert!(dag.nodes.contains_key("poison-h"));
    Ok(())
}

// r[verify sched.dag.build-scoped-roots]
/// Sanity: a node with NO global parents is still a root (the
/// is_none_or/is_some_and inversion didn't break the base case).
#[test]
fn test_find_roots_no_parents_still_root() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let build_id = Uuid::new_v4();
    dag.merge(build_id, &[make_node("solo", "x86_64-linux")], &[], "")?;

    let roots = dag.find_roots(build_id);
    assert_eq!(roots.len(), 1);
    assert!(roots.iter().any(|h| h == "solo"));
    Ok(())
}

// r[verify sched.ca.cutoff-propagate+2]
/// bug_383: result-size cap, not pop-count cap. With one trigger and
/// 5×MAX direct Queued parents (fanout >> 1), the OLD pop-count cap
/// returned 5×MAX nodes (one pop, 5×MAX pushes) and reported
/// `cap_hit=false` (pops=1 < MAX). The fix bounds `reachable.len()`
/// inside the inner push loop.
#[test]
fn ca_cutoff_result_cap_with_fanout() {
    let mut dag = DerivationDag::new();
    let n_parents = MAX_CASCADE_NODES * 5;
    let path = |i: usize| format!("/nix/store/{i:032}-n{i}.drv");
    let mut nodes = vec![make_node_with_path("trig", &path(0), "x86_64-linux")];
    let mut edges = Vec::with_capacity(n_parents);
    for i in 1..=n_parents {
        nodes.push(make_node_with_path(
            &format!("p{i:05}"),
            &path(i),
            "x86_64-linux",
        ));
        edges.push(make_edge_with_paths(&path(i), &path(0)));
    }
    dag.merge(Uuid::new_v4(), &nodes, &edges, "").unwrap();
    dag.node_mut("trig")
        .unwrap()
        .set_status_for_test(DerivationStatus::Completed);
    for i in 1..=n_parents {
        dag.node_mut(&format!("p{i:05}"))
            .unwrap()
            .set_status_for_test(DerivationStatus::Queued);
    }

    let (skipped, cap_hit) = dag.cascade_cutoff("trig", |_| true);

    assert!(
        cap_hit,
        "single high-fanout expand must report cap_hit (was: pops=1 → false)"
    );
    assert_eq!(
        skipped.len(),
        MAX_CASCADE_NODES,
        "result bounded at MAX_CASCADE_NODES exactly (was: 5×MAX)"
    );
}

/// bug_470: `merge()` upgrades a pre-existing node's empty traceparent,
/// but the mutation wasn't tracked for rollback. After a cycle reject,
/// the rejected build's traceparent permanently stuck; the next
/// submitter's `is_empty()` check at the upgrade site is false, so the
/// build that actually drives the node never links its trace.
#[test]
fn test_cyclic_merge_reverts_traceparent_upgrade() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();

    // B1 merges X with traceparent="" (recovery/poison-reset path).
    let b1 = Uuid::new_v4();
    dag.merge(b1, &[make_node("hashX", "x86_64-linux")], &[], "")?;
    assert_eq!(dag.node("hashX").expect("hashX").traceparent, "");

    // B2 merges {X, A↔B cycle} with a real traceparent. The X-upgrade
    // fires before the cycle is detected.
    let b2 = Uuid::new_v4();
    let result = dag.merge(
        b2,
        &[
            make_node("hashX", "x86_64-linux"),
            make_node("hashA", "x86_64-linux"),
            make_node("hashB", "x86_64-linux"),
        ],
        &[make_edge("hashA", "hashB"), make_edge("hashB", "hashA")],
        "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
    );
    assert!(matches!(result, Err(DagError::CycleDetected)));

    assert_eq!(
        dag.node("hashX").expect("hashX").traceparent,
        "",
        "rejected build's traceparent must not stick (was: permanently set)"
    );

    // B3 (the build that actually drives X) now gets its trace linked.
    let b3 = Uuid::new_v4();
    let r3 = dag.merge(b3, &[make_node("hashX", "x86_64-linux")], &[], "00-b3-01")?;
    assert_eq!(r3.traceparent_upgraded, vec!["hashX"]);
    assert_eq!(dag.node("hashX").expect("hashX").traceparent, "00-b3-01");
    Ok(())
}

// r[verify sched.merge.dep-failed-transitive]
/// bug_051: `compute_initial_states` decided every node against the
/// pre-call snapshot. For chain A→B→X with X already Poisoned
/// (non-retriable) and A,B newly inserted: B sees X→DepFailed; A sees
/// B=Created→Queued. Under keepGoing=true the runtime cascade is never
/// reached for merge-seeded DepFailed — A stays Queued forever.
#[rstest]
#[case::two_hop(&["B"])]
#[case::three_hop(&["C", "B"])]
fn test_initial_states_transitive_dep_failed(#[case] mids: &[&str]) -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();

    // Pre-insert X as Poisoned at the resubmit-retry limit
    // (non-retriable; merge won't reset it).
    dag.merge(Uuid::new_v4(), &[make_node("X", "x86_64-linux")], &[], "")?;
    {
        let x = dag.nodes.get_mut("X").expect("X");
        x.set_status_for_test(DerivationStatus::Poisoned);
        x.retry.resubmit_cycles = crate::state::POISON_RESUBMIT_RETRY_LIMIT;
    }

    // Merge chain A→[mids…]→X.
    let mut chain: Vec<&str> = vec!["A"];
    chain.extend_from_slice(mids);
    chain.push("X");
    let nodes: Vec<_> = chain.iter().map(|h| make_node(h, "x86_64-linux")).collect();
    let edges: Vec<_> = chain.windows(2).map(|w| make_edge(w[0], w[1])).collect();
    let newly = dag
        .merge(Uuid::new_v4(), &nodes, &edges, "")?
        .newly_inserted;
    // X already existed and is non-retriable → not in newly_inserted.
    assert!(!newly.contains("X"));

    let states: HashMap<_, _> = dag.compute_initial_states(&newly).into_iter().collect();
    assert_eq!(states.len(), chain.len() - 1);
    for h in &chain[..chain.len() - 1] {
        assert_eq!(
            states[*h],
            DerivationStatus::DependencyFailed,
            "{h} must be DependencyFailed transitively (was: A→Queued, build hangs)"
        );
    }
    Ok(())
}

/// bug_401 correctness half: `remove_node` scrubs only the removed
/// node's neighbors via the bidirectional-edge invariant — not all
/// edge sets, and not too few.
#[test]
fn test_remove_node_scrubs_only_neighbors() -> anyhow::Result<()> {
    // Diamond a→{b,c}→d.
    let mut dag = DerivationDag::new();
    dag.merge(
        Uuid::new_v4(),
        &[
            make_node("a", "x86_64-linux"),
            make_node("b", "x86_64-linux"),
            make_node("c", "x86_64-linux"),
            make_node("d", "x86_64-linux"),
        ],
        &[
            make_edge("a", "b"),
            make_edge("a", "c"),
            make_edge("b", "d"),
            make_edge("c", "d"),
        ],
        "",
    )?;

    dag.remove_node(&"b".into());

    assert!(!dag.nodes.contains_key("b"));
    assert!(!dag.children.contains_key("b"), "children[b] scrubbed");
    assert!(!dag.parents.contains_key("b"), "parents[b] scrubbed");
    assert_eq!(
        dag.children["a"],
        HashSet::from(["c".into()]),
        "a's children: b scrubbed, c retained"
    );
    assert_eq!(
        dag.parents["d"],
        HashSet::from(["c".into()]),
        "d's parents: b scrubbed, c retained"
    );
    // Unrelated entries untouched.
    assert_eq!(dag.children["c"], HashSet::from(["d".into()]));
    assert_eq!(dag.parents["c"], HashSet::from(["a".into()]));
    Ok(())
}

// ── sched.merge.authoritative-conflict ──────────────────────────────────

/// Helper: a content-bound (floating-CA-shaped) node carrying
/// authoritative inline derivation content, the shape produced by the
/// gateway's content-bound hook fallback.
fn authoritative_node(tag: &str, content: &[u8]) -> DerivationNode {
    let mut n = make_node(tag, "x86_64-linux");
    n.drv_content = content.to_vec();
    n.drv_content_authoritative = true;
    n.is_content_addressed = true;
    n.expected_output_paths = vec![String::new()];
    // The realisation key ingress binds to the bytes; the merge gate uses
    // it as the floating-CA content evidence.
    n.ca_modular_hash = Some([0xAB; 32]);
    n
}

// r[verify sched.merge.authoritative-conflict+6]
#[test]
fn authoritative_collision_requires_byte_equality() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let b1 = Uuid::new_v4();
    let b2 = Uuid::new_v4();
    let b3 = Uuid::new_v4();

    dag.merge(b1, &[authoritative_node("auth", b"Derive-A")], &[], "")?;

    // Different authoritative bytes for the same drv_hash → rejected,
    // existing node untouched, no interest recorded for the rejecter.
    let err = dag
        .merge(b2, &[authoritative_node("auth", b"Derive-B")], &[], "")
        .unwrap_err();
    assert!(matches!(err, DagError::AuthoritativeContentMismatch { .. }));
    let node = dag.node("auth").unwrap();
    assert_eq!(node.drv_content, b"Derive-A");
    assert!(!node.interested_builds.contains(&b2));

    // Byte-identical resubmission (the legitimate hook producer) joins.
    let res = dag.merge(b3, &[authoritative_node("auth", b"Derive-A")], &[], "")?;
    assert!(res.newly_inserted.is_empty());
    assert!(dag.node("auth").unwrap().interested_builds.contains(&b3));
    Ok(())
}

// r[verify sched.merge.authoritative-conflict+6]
#[test]
fn conflicting_identity_against_inflight_authoritative_rejected() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let b1 = Uuid::new_v4();
    let b2 = Uuid::new_v4();

    dag.merge(b1, &[authoritative_node("squat", b"Derive-A")], &[], "")?;

    // Store-backed submission with a conflicting verifiable identity
    // (different system) while the squatting node is in flight → rejected.
    let mut victim = make_node("squat", "aarch64-linux");
    victim.is_content_addressed = true;
    let err = dag.merge(b2, &[victim], &[], "").unwrap_err();
    assert!(matches!(err, DagError::ConflictingInFlightContent { .. }));
    assert!(!dag.node("squat").unwrap().interested_builds.contains(&b2));
    Ok(())
}

// r[verify sched.merge.authoritative-conflict+6]
/// Displacement of a conflicting authoritative squat is scoped to
/// terminal FAILURE states: the verifiable store-backed definition wins
/// against a parked failure, but never against a settled success (see
/// `conflicting_identity_rejected_on_settled_authoritative_node`).
#[rstest]
#[case::poisoned(DerivationStatus::Poisoned)]
#[case::cancelled(DerivationStatus::Cancelled)]
#[case::dependency_failed(DerivationStatus::DependencyFailed)]
fn conflicting_identity_displaces_terminal_authoritative_node(
    #[case] parked: DerivationStatus,
) -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let b1 = Uuid::new_v4();
    let b2 = Uuid::new_v4();

    dag.merge(b1, &[authoritative_node("squat2", b"Derive-A")], &[], "")?;
    dag.nodes
        .get_mut("squat2")
        .unwrap()
        .set_status_for_test(parked);

    // Once the squatting node is parked in a terminal failure state, the
    // verifiable definition displaces it: fresh node, no inherited
    // interest, no rejection.
    let mut victim = make_node("squat2", "aarch64-linux");
    victim.is_content_addressed = true;
    let res = dag.merge(b2, &[victim], &[], "")?;
    assert!(res.newly_inserted.contains("squat2"));
    // Displacement is surfaced to the actor via `displaced` (for poison /
    // accounting reconciliation), never via `reset_on_resubmit`.
    assert!(res.displaced.iter().any(|h| h.as_str() == "squat2"));
    assert!(res.reset_on_resubmit.is_empty());
    let node = dag.node("squat2").unwrap();
    assert_eq!(node.system, "aarch64-linux");
    assert!(node.drv_content.is_empty());
    assert!(!node.drv_content_authoritative);
    assert!(node.interested_builds.contains(&b2));
    assert!(!node.interested_builds.contains(&b1));
    Ok(())
}

// r[verify sched.merge.authoritative-conflict+6]
/// A conflicting store-backed submission against a SETTLED
/// (Completed/Skipped) authoritative node is rejected, never displaces:
/// the settled record — its identity, inline bytes, and interest
/// accounting — survives intact. Displacing it would let an unverified
/// submitter claim erase the record of a successful build (bug_076).
#[rstest]
#[case::completed(DerivationStatus::Completed)]
#[case::skipped(DerivationStatus::Skipped)]
fn conflicting_identity_rejected_on_settled_authoritative_node(
    #[case] settled: DerivationStatus,
) -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let b1 = Uuid::new_v4();
    let b2 = Uuid::new_v4();

    dag.merge(b1, &[authoritative_node("settled", b"Derive-A")], &[], "")?;
    dag.nodes
        .get_mut("settled")
        .unwrap()
        .set_status_for_test(settled);

    let mut conflicting = make_node("settled", "aarch64-linux");
    conflicting.is_content_addressed = true;
    let err = dag.merge(b2, &[conflicting], &[], "").unwrap_err();
    assert!(matches!(err, DagError::ConflictingInFlightContent { .. }));

    // The settled node is byte-for-byte what it was before the attempt.
    let node = dag.node("settled").unwrap();
    assert_eq!(node.status(), settled, "settled status untouched");
    assert_eq!(node.system, "x86_64-linux");
    assert_eq!(node.drv_content, b"Derive-A");
    assert!(node.drv_content_authoritative);
    assert!(node.interested_builds.contains(&b1));
    assert!(!node.interested_builds.contains(&b2));
    Ok(())
}

// r[verify sched.merge.authoritative-conflict+6]
#[test]
fn matching_identity_joins_authoritative_node() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let b1 = Uuid::new_v4();
    let b2 = Uuid::new_v4();

    dag.merge(b1, &[authoritative_node("join", b"Derive-A")], &[], "")?;

    // Same verifiable identity WITH content evidence (the matching CA
    // modular hash a store-backed submission of the same resolved
    // derivation computes) → joins as before; bytes untouched.
    let mut same = make_node("join", "x86_64-linux");
    same.is_content_addressed = true;
    same.ca_modular_hash = Some([0xAB; 32]);
    let res = dag.merge(b2, &[same], &[], "")?;
    assert!(res.newly_inserted.is_empty());
    let node = dag.node("join").unwrap();
    assert!(node.interested_builds.contains(&b2));
    assert_eq!(node.drv_content, b"Derive-A");
    assert!(node.drv_content_authoritative);
    Ok(())
}

// r[verify sched.merge.authoritative-conflict+6]
#[test]
fn authoritative_bytes_ignored_when_existing_node_is_store_backed() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let b1 = Uuid::new_v4();
    let b2 = Uuid::new_v4();

    // Store-backed node first…
    dag.merge(b1, &[make_node("store-backed", "x86_64-linux")], &[], "")?;
    // …then an authoritative submission for the same drv_hash: joins,
    // bytes ignored (the store remains the source of truth).
    let res = dag.merge(
        b2,
        &[authoritative_node("store-backed", b"Derive-X")],
        &[],
        "",
    )?;
    assert!(res.newly_inserted.is_empty());
    let node = dag.node("store-backed").unwrap();
    assert!(node.interested_builds.contains(&b2));
    assert!(node.drv_content.is_empty());
    assert!(!node.drv_content_authoritative);
    Ok(())
}

// r[verify sched.merge.authoritative-claim-no-redefine]
/// The inverse direction of the gate: an authoritative claim landing on
/// a STORE-BACKED node that is eligible for the resubmit-reset must not
/// be able to redefine it. With a conflicting verifiable identity
/// (different system here) the claim is rejected and the parked
/// store-backed node is left exactly as it was — still store-backed,
/// same status, no interest recorded for the rejecter.
#[rstest]
#[case::failed(DerivationStatus::Failed)]
#[case::cancelled(DerivationStatus::Cancelled)]
#[case::dependency_failed(DerivationStatus::DependencyFailed)]
#[case::poisoned_under_budget(DerivationStatus::Poisoned)]
fn authoritative_claim_rejected_on_retriable_store_backed_node(
    #[case] prior: DerivationStatus,
) -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let b1 = Uuid::new_v4();
    let b2 = Uuid::new_v4();

    // Store-backed (verifiable) definition first, then parked retriable.
    let mut store_backed = make_node("claim-park", "aarch64-linux");
    store_backed.is_content_addressed = true;
    dag.merge(b1, &[store_backed], &[], "")?;
    dag.nodes
        .get_mut("claim-park")
        .unwrap()
        .set_status_for_test(prior);
    let prior_cycles = dag.node("claim-park").unwrap().retry.resubmit_cycles;

    // Authoritative claim with a conflicting identity (x86_64 vs the
    // parked aarch64 definition) → rejected, nothing adopted.
    let err = dag
        .merge(
            b2,
            &[authoritative_node("claim-park", b"Derive-EVIL")],
            &[],
            "",
        )
        .unwrap_err();
    assert!(matches!(
        err,
        DagError::AuthoritativeClaimIdentityConflict { .. }
    ));
    let node = dag.node("claim-park").unwrap();
    assert_eq!(node.status(), prior, "parked status untouched");
    assert_eq!(node.system, "aarch64-linux");
    assert!(node.drv_content.is_empty(), "no bytes adopted");
    assert!(!node.drv_content_authoritative, "still store-backed");
    assert_eq!(node.retry.resubmit_cycles, prior_cycles);
    assert!(node.interested_builds.contains(&b1));
    assert!(!node.interested_builds.contains(&b2));
    Ok(())
}

// r[verify sched.merge.authoritative-claim-no-redefine]
/// An authoritative claim whose verifiable identity MATCHES the parked
/// store-backed node (same public attributes plus content-bound
/// evidence) is the legitimate hook-fallback retry: it is admitted
/// through the normal resubmit-reset — bytes adopted, prior interest
/// carried, resubmit cycle accumulated — and never via displacement.
/// Covers both evidence forms: a byte-equal CA modular hash and a
/// shared non-empty fixed-output expected path.
#[test]
fn authoritative_claim_with_matching_identity_resets_store_backed_node() -> anyhow::Result<()> {
    // Floating-CA evidence: matching modular hash.
    let mut dag = DerivationDag::new();
    let b1 = Uuid::new_v4();
    let b2 = Uuid::new_v4();
    let mut store_backed = make_node("claim-ca", "x86_64-linux");
    store_backed.is_content_addressed = true;
    store_backed.expected_output_paths = vec![String::new()];
    store_backed.ca_modular_hash = Some([0xAB; 32]);
    dag.merge(b1, &[store_backed], &[], "")?;
    dag.nodes
        .get_mut("claim-ca")
        .unwrap()
        .set_status_for_test(DerivationStatus::Failed);
    let prior_cycles = dag.node("claim-ca").unwrap().retry.resubmit_cycles;

    let res = dag.merge(b2, &[authoritative_node("claim-ca", b"Derive-A")], &[], "")?;
    assert!(res.newly_inserted.contains("claim-ca"));
    assert!(
        res.reset_on_resubmit
            .iter()
            .any(|h| h.as_str() == "claim-ca"),
        "admitted through the resubmit-reset"
    );
    assert!(
        res.displaced.is_empty(),
        "never displaces a store-backed node"
    );
    let node = dag.node("claim-ca").unwrap();
    assert_eq!(node.drv_content, b"Derive-A", "claim bytes adopted");
    assert!(node.drv_content_authoritative);
    assert!(
        node.interested_builds.contains(&b1),
        "prior interest carried"
    );
    assert!(node.interested_builds.contains(&b2));
    assert_eq!(node.retry.resubmit_cycles, prior_cycles + 1);

    // Fixed-output evidence: shared non-empty expected path.
    let mut dag = DerivationDag::new();
    let b3 = Uuid::new_v4();
    let b4 = Uuid::new_v4();
    let fod_path = "/nix/store/ffffffffffffffffffffffffffffffff-fod-out";
    let mut fod_store_backed = make_node("claim-fod", "x86_64-linux");
    fod_store_backed.is_fixed_output = true;
    fod_store_backed.is_content_addressed = true;
    fod_store_backed.expected_output_paths = vec![fod_path.to_string()];
    dag.merge(b3, &[fod_store_backed], &[], "")?;
    dag.nodes
        .get_mut("claim-fod")
        .unwrap()
        .set_status_for_test(DerivationStatus::Cancelled);

    let mut fod_claim = authoritative_node("claim-fod", b"Derive-FOD");
    fod_claim.is_fixed_output = true;
    fod_claim.expected_output_paths = vec![fod_path.to_string()];
    fod_claim.ca_modular_hash = None;
    let res = dag.merge(b4, &[fod_claim], &[], "")?;
    assert!(
        res.reset_on_resubmit
            .iter()
            .any(|h| h.as_str() == "claim-fod")
    );
    let node = dag.node("claim-fod").unwrap();
    assert_eq!(node.drv_content, b"Derive-FOD");
    assert!(node.drv_content_authoritative);
    assert!(node.interested_builds.contains(&b3));
    assert!(node.interested_builds.contains(&b4));
    Ok(())
}

// r[verify sched.merge.authoritative-claim-no-redefine]
/// Degenerate-evidence parity with the store-backed→authoritative arm:
/// a parked retriable store-backed floating-CA node that carries NO
/// content-bound evidence (no modular hash) cannot be claimed by an
/// authoritative submission — no evidence is a conflict, not a match.
#[test]
fn authoritative_claim_without_evidence_is_rejected() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let b1 = Uuid::new_v4();
    let b2 = Uuid::new_v4();

    // Same public attributes as the claimant (system, CA flag, output
    // set) but no modular hash on the parked node → no evidence.
    let mut store_backed = make_node("claim-noev", "x86_64-linux");
    store_backed.is_content_addressed = true;
    store_backed.expected_output_paths = vec![String::new()];
    dag.merge(b1, &[store_backed], &[], "")?;
    dag.nodes
        .get_mut("claim-noev")
        .unwrap()
        .set_status_for_test(DerivationStatus::Failed);

    let err = dag
        .merge(
            b2,
            &[authoritative_node("claim-noev", b"Derive-A")],
            &[],
            "",
        )
        .unwrap_err();
    assert!(matches!(
        err,
        DagError::AuthoritativeClaimIdentityConflict { .. }
    ));
    let node = dag.node("claim-noev").unwrap();
    assert!(!node.drv_content_authoritative);
    assert!(node.drv_content.is_empty());
    assert!(!node.interested_builds.contains(&b2));
    Ok(())
}

// r[verify sched.merge.authoritative-conflict+6]
/// Match-displacement of a poison-locked squat: an identity-MATCHING
/// store-backed submission must DISPLACE (not join) an authoritative
/// node that sits in a terminal failure state no longer retriable on
/// resubmit (poison budget exhausted) — otherwise the locked claim
/// would capture every later legitimate submission of the derivation
/// for the rest of its poison TTL. FOD-shaped here: path agreement is
/// the content evidence, exactly the join-evidence the squat used.
#[test]
fn fod_matching_identity_displaces_poisoned_over_budget_squat() -> anyhow::Result<()> {
    use crate::state::POISON_RESUBMIT_RETRY_LIMIT;
    let mut dag = DerivationDag::new();
    let squatter = Uuid::new_v4();
    let victim = Uuid::new_v4();
    let fod_path = "/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-fod-out";

    let mut squat = authoritative_node("locked-fod", b"Derive-FOD");
    squat.is_fixed_output = true;
    squat.expected_output_paths = vec![fod_path.to_string()];
    squat.ca_modular_hash = None;
    dag.merge(squatter, &[squat], &[], "")?;
    {
        let n = dag.nodes.get_mut("locked-fod").unwrap();
        n.set_status_for_test(DerivationStatus::Poisoned);
        n.retry.resubmit_cycles = POISON_RESUBMIT_RETRY_LIMIT;
    }

    // Same verifiable identity (shared non-empty expected path).
    let mut store_backed = make_node("locked-fod", "x86_64-linux");
    store_backed.is_fixed_output = true;
    store_backed.is_content_addressed = true;
    store_backed.expected_output_paths = vec![fod_path.to_string()];
    let res = dag.merge(victim, &[store_backed], &[], "")?;

    assert!(res.newly_inserted.contains("locked-fod"));
    assert!(
        res.displaced.iter().any(|h| h.as_str() == "locked-fod"),
        "match-displacement, not a join"
    );
    assert!(
        res.reset_on_resubmit.is_empty(),
        "not the resubmit-reset path"
    );
    let n = dag.node("locked-fod").unwrap();
    assert!(n.drv_content.is_empty());
    assert!(!n.drv_content_authoritative);
    assert_eq!(
        n.interested_builds,
        HashSet::from([victim]),
        "no inherited interest"
    );
    assert_eq!(n.retry.resubmit_cycles, 0, "fresh poison budget");
    Ok(())
}

// r[verify sched.merge.authoritative-conflict+6]
/// Match-displacement is scoped to LOCKED terminal failures only: a
/// successfully finished authoritative node (Completed or Skipped)
/// keeps the join semantics for an identity-matching store-backed
/// submission, so cache-hit dedup of an already-built derivation is
/// not lost.
#[rstest]
#[case::completed(DerivationStatus::Completed)]
#[case::skipped(DerivationStatus::Skipped)]
fn matching_identity_joins_completed_authoritative_node(
    #[case] terminal_ok: DerivationStatus,
) -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let b1 = Uuid::new_v4();
    let b2 = Uuid::new_v4();

    dag.merge(b1, &[authoritative_node("done-auth", b"Derive-A")], &[], "")?;
    dag.nodes
        .get_mut("done-auth")
        .unwrap()
        .set_status_for_test(terminal_ok);

    let mut same = make_node("done-auth", "x86_64-linux");
    same.is_content_addressed = true;
    same.ca_modular_hash = Some([0xAB; 32]);
    let res = dag.merge(b2, &[same], &[], "")?;
    assert!(res.newly_inserted.is_empty(), "joins, not displaced");
    assert!(res.displaced.is_empty());
    let node = dag.node("done-auth").unwrap();
    assert_eq!(node.status(), terminal_ok, "terminal-success state kept");
    assert_eq!(node.drv_content, b"Derive-A");
    assert!(node.drv_content_authoritative);
    assert!(node.interested_builds.contains(&b2));
    Ok(())
}

// r[verify sched.merge.authoritative-conflict+6]
// r[verify sched.merge.displaced-failure-reset+2]
/// An UNDER-budget poisoned squat is still retriable on resubmit, so an
/// identity-matching store-backed submission takes the normal
/// resubmit-reset (interest carried, the squat's bytes replaced by the
/// store-backed definition) — the match-displacement arm must not
/// pre-empt it. Because the removed node was authoritative and the
/// re-creating submission is store-backed, this is an authority
/// takeover: the fresh node is a different definition, so it starts
/// with a fresh poison-resubmit budget and is surfaced in
/// `authority_takeovers` (so the actor skips the cycle-incrementing
/// `clear_poison_batch` for it).
#[test]
fn matching_identity_resets_poisoned_under_budget_squat() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let squatter = Uuid::new_v4();
    let resubmitter = Uuid::new_v4();

    dag.merge(
        squatter,
        &[authoritative_node("under-budget", b"Derive-A")],
        &[],
        "",
    )?;
    {
        let n = dag.nodes.get_mut("under-budget").unwrap();
        n.set_status_for_test(DerivationStatus::Poisoned);
        n.retry.resubmit_cycles = 1; // under POISON_RESUBMIT_RETRY_LIMIT
    }

    let mut same = make_node("under-budget", "x86_64-linux");
    same.is_content_addressed = true;
    same.ca_modular_hash = Some([0xAB; 32]);
    let res = dag.merge(resubmitter, &[same], &[], "")?;

    assert!(res.newly_inserted.contains("under-budget"));
    assert!(
        res.reset_on_resubmit
            .iter()
            .any(|h| h.as_str() == "under-budget"),
        "resubmit-reset, not displacement"
    );
    assert!(res.displaced.is_empty());
    assert!(
        res.authority_takeovers
            .iter()
            .any(|h| h.as_str() == "under-budget"),
        "authoritative→store-backed flip surfaced as an authority takeover"
    );
    let n = dag.node("under-budget").unwrap();
    assert!(n.interested_builds.contains(&squatter), "interest carried");
    assert!(n.interested_builds.contains(&resubmitter));
    assert_eq!(
        n.retry.resubmit_cycles, 0,
        "definition change: the squat's consumed poison budget does not carry over"
    );
    assert!(n.drv_content.is_empty(), "store-backed definition adopted");
    assert!(!n.drv_content_authoritative);
    Ok(())
}

// r[verify sched.merge.displaced-failure-reset+2]
/// Contrast pin for the authority-takeover carve-out: SAME-definition
/// resubmits keep accumulating the poison-resubmit budget and are never
/// reported as authority takeovers — a store-backed node resubmitted
/// store-backed, and an authoritative node resubmitted with
/// byte-identical content, both increment `resubmit_cycles` exactly as
/// before.
#[test]
fn store_backed_resubmit_is_not_an_authority_takeover() -> anyhow::Result<()> {
    // Store-backed → store-backed.
    let mut dag = DerivationDag::new();
    let b1 = Uuid::new_v4();
    let b2 = Uuid::new_v4();
    dag.merge(b1, &[make_node("same-store", "x86_64-linux")], &[], "")?;
    {
        let n = dag.nodes.get_mut("same-store").unwrap();
        n.set_status_for_test(DerivationStatus::Failed);
        n.retry.resubmit_cycles = 1;
    }
    let res = dag.merge(b2, &[make_node("same-store", "x86_64-linux")], &[], "")?;
    assert!(
        res.reset_on_resubmit
            .iter()
            .any(|h| h.as_str() == "same-store")
    );
    assert!(
        res.authority_takeovers.is_empty(),
        "store→store resubmit is not a takeover"
    );
    assert_eq!(
        dag.node("same-store").unwrap().retry.resubmit_cycles,
        2,
        "same-definition resubmit keeps accumulating the budget"
    );

    // Authoritative → byte-identical authoritative.
    let mut dag = DerivationDag::new();
    let b3 = Uuid::new_v4();
    let b4 = Uuid::new_v4();
    dag.merge(b3, &[authoritative_node("same-auth", b"Derive-A")], &[], "")?;
    {
        let n = dag.nodes.get_mut("same-auth").unwrap();
        n.set_status_for_test(DerivationStatus::Failed);
        n.retry.resubmit_cycles = 1;
    }
    let res = dag.merge(b4, &[authoritative_node("same-auth", b"Derive-A")], &[], "")?;
    assert!(
        res.reset_on_resubmit
            .iter()
            .any(|h| h.as_str() == "same-auth")
    );
    assert!(
        res.authority_takeovers.is_empty(),
        "byte-identical authoritative retry is not a takeover"
    );
    assert_eq!(dag.node("same-auth").unwrap().retry.resubmit_cycles, 2);
    Ok(())
}

// r[verify sched.merge.authoritative-conflict+6]
/// A merge that match-displaces a poison-locked squat but fails on a
/// LATER node in the same submission must restore the squat exactly —
/// the match-displacement rides the same rollback container as the
/// conflict-displacement.
#[test]
fn rollback_restores_a_match_displaced_poisoned_squat() -> anyhow::Result<()> {
    use crate::state::POISON_RESUBMIT_RETRY_LIMIT;
    let mut dag = DerivationDag::new();
    let squatter = Uuid::new_v4();
    let victim = Uuid::new_v4();

    dag.merge(
        squatter,
        &[authoritative_node("locked-rb", b"Derive-A")],
        &[],
        "",
    )?;
    {
        let n = dag.nodes.get_mut("locked-rb").unwrap();
        n.set_status_for_test(DerivationStatus::Poisoned);
        n.retry.resubmit_cycles = POISON_RESUBMIT_RETRY_LIMIT;
    }

    let mut displacing = make_node("locked-rb", "x86_64-linux");
    displacing.is_content_addressed = true;
    displacing.ca_modular_hash = Some([0xAB; 32]);
    let result = dag.merge(
        victim,
        &[
            displacing,
            make_node_with_path("bad", "not-a-store-path", "x86_64-linux"),
        ],
        &[],
        "",
    );
    assert!(matches!(result, Err(DagError::InvalidDrvPath { .. })));

    let n = dag.node("locked-rb").expect("squat restored");
    assert_eq!(n.status(), DerivationStatus::Poisoned);
    assert_eq!(n.drv_content, b"Derive-A");
    assert!(n.drv_content_authoritative);
    assert_eq!(n.retry.resubmit_cycles, POISON_RESUBMIT_RETRY_LIMIT);
    assert_eq!(n.interested_builds, HashSet::from([squatter]));
    Ok(())
}

// r[verify sched.merge.authoritative-conflict+6]
/// The gate is evaluated BEFORE the resubmit-reset, so an authoritative
/// node that is merely `Failed` (non-terminal — the retry machinery
/// still owns it) cannot be silently redefined by different
/// authoritative bytes — while the legitimate byte-identical retry still
/// flows through the resubmit-reset (interest carry + cycle increment).
/// Terminal failure states are covered by
/// `authoritative_redefinition_displaces_parked_terminal_failure`.
#[test]
fn authoritative_redefinition_rejected_while_failed() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let b1 = Uuid::new_v4();
    let b2 = Uuid::new_v4();
    let b3 = Uuid::new_v4();

    dag.merge(b1, &[authoritative_node("retriable", b"Derive-A")], &[], "")?;
    dag.nodes
        .get_mut("retriable")
        .unwrap()
        .set_status_for_test(DerivationStatus::Failed);

    // Different authoritative bytes → rejected while Failed; node
    // untouched.
    let err = dag
        .merge(b2, &[authoritative_node("retriable", b"Derive-B")], &[], "")
        .unwrap_err();
    assert!(matches!(err, DagError::AuthoritativeContentMismatch { .. }));
    let node = dag.node("retriable").unwrap();
    assert_eq!(node.drv_content, b"Derive-A");
    assert_eq!(node.status(), DerivationStatus::Failed);
    assert!(!node.interested_builds.contains(&b2));

    // Byte-identical retry (the legitimate hook producer) is admitted and
    // takes the resubmit-reset path: fresh node, prior interest carried,
    // poison-cycle accumulator incremented.
    let prior_cycles = dag.node("retriable").unwrap().retry.resubmit_cycles;
    let res = dag.merge(b3, &[authoritative_node("retriable", b"Derive-A")], &[], "")?;
    assert!(res.newly_inserted.contains("retriable"));
    assert!(
        res.reset_on_resubmit
            .iter()
            .any(|h| h.as_str() == "retriable")
    );
    assert!(res.displaced.is_empty());
    let node = dag.node("retriable").unwrap();
    assert!(node.interested_builds.contains(&b1));
    assert!(node.interested_builds.contains(&b3));
    assert_eq!(node.retry.resubmit_cycles, prior_cycles + 1);
    Ok(())
}

// r[verify sched.merge.authoritative-conflict+6]
/// A byte-different authoritative submission DISPLACES an authoritative
/// claim parked in a terminal failure state (Poisoned at any budget,
/// Cancelled, DependencyFailed): the hook-fallback population submits
/// authoritatively and has no store-backed form, so without this arm a
/// failed pre-squat would lock those victims out of the hash for the
/// rest of its poison TTL. The fresh node carries the new bytes, only
/// the new submitter's interest, and a fresh poison budget; it is
/// surfaced via `displaced` (not `reset_on_resubmit`) so all the
/// displacement bookkeeping applies.
#[rstest]
#[case::poisoned_over_budget(DerivationStatus::Poisoned, true)]
#[case::poisoned_under_budget(DerivationStatus::Poisoned, false)]
#[case::cancelled(DerivationStatus::Cancelled, false)]
#[case::dependency_failed(DerivationStatus::DependencyFailed, false)]
fn authoritative_redefinition_displaces_parked_terminal_failure(
    #[case] prior: DerivationStatus,
    #[case] over_budget: bool,
) -> anyhow::Result<()> {
    use crate::state::POISON_RESUBMIT_RETRY_LIMIT;
    let mut dag = DerivationDag::new();
    let squatter = Uuid::new_v4();
    let victim = Uuid::new_v4();

    dag.merge(
        squatter,
        &[authoritative_node("auth-displace", b"Derive-squat")],
        &[],
        "",
    )?;
    {
        let n = dag.nodes.get_mut("auth-displace").unwrap();
        n.set_status_for_test(prior);
        if over_budget {
            n.retry.resubmit_cycles = POISON_RESUBMIT_RETRY_LIMIT;
        }
    }

    let res = dag.merge(
        victim,
        &[authoritative_node("auth-displace", b"Derive-victim")],
        &[],
        "",
    )?;
    assert!(res.newly_inserted.contains("auth-displace"));
    assert!(
        res.displaced.iter().any(|h| h.as_str() == "auth-displace"),
        "terminal-failure squat displaced, not joined or reset"
    );
    assert!(
        res.reset_on_resubmit.is_empty(),
        "not the resubmit-reset path"
    );
    let n = dag.node("auth-displace").unwrap();
    assert_eq!(n.drv_content, b"Derive-victim", "redefinition's bytes win");
    assert!(n.drv_content_authoritative);
    assert_eq!(
        n.interested_builds,
        HashSet::from([victim]),
        "no inherited interest"
    );
    assert_eq!(n.retry.resubmit_cycles, 0, "fresh poison budget");
    Ok(())
}

// r[verify sched.merge.authoritative-conflict+6]
/// A successfully finished authoritative definition (Completed or
/// Skipped) is never redefined: byte-different authoritative content is
/// still rejected, so an attacker cannot rewrite an already-built
/// derivation out from under the builds that produced or consumed it.
#[rstest]
#[case::completed(DerivationStatus::Completed)]
#[case::skipped(DerivationStatus::Skipped)]
fn authoritative_redefinition_still_rejected_after_success(
    #[case] terminal_ok: DerivationStatus,
) -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let b1 = Uuid::new_v4();
    let b2 = Uuid::new_v4();

    dag.merge(b1, &[authoritative_node("auth-done", b"Derive-A")], &[], "")?;
    dag.nodes
        .get_mut("auth-done")
        .unwrap()
        .set_status_for_test(terminal_ok);

    let err = dag
        .merge(b2, &[authoritative_node("auth-done", b"Derive-B")], &[], "")
        .unwrap_err();
    assert!(matches!(err, DagError::AuthoritativeContentMismatch { .. }));
    let node = dag.node("auth-done").unwrap();
    assert_eq!(node.status(), terminal_ok, "built definition untouched");
    assert_eq!(node.drv_content, b"Derive-A");
    assert!(!node.interested_builds.contains(&b2));
    Ok(())
}

// r[verify sched.merge.authoritative-conflict+6]
/// A merge that auth-displaces a poison-locked squat but fails on a
/// LATER node in the same submission must restore the squat exactly —
/// the auth-vs-auth displacement rides the same rollback container as
/// the store-backed displacement arms.
#[test]
fn rollback_restores_an_auth_displaced_locked_squat() -> anyhow::Result<()> {
    use crate::state::POISON_RESUBMIT_RETRY_LIMIT;
    let mut dag = DerivationDag::new();
    let squatter = Uuid::new_v4();
    let victim = Uuid::new_v4();

    dag.merge(
        squatter,
        &[authoritative_node("auth-rb", b"Derive-A")],
        &[],
        "",
    )?;
    {
        let n = dag.nodes.get_mut("auth-rb").unwrap();
        n.set_status_for_test(DerivationStatus::Poisoned);
        n.retry.resubmit_cycles = POISON_RESUBMIT_RETRY_LIMIT;
    }

    let result = dag.merge(
        victim,
        &[
            authoritative_node("auth-rb", b"Derive-B"),
            make_node_with_path("bad", "not-a-store-path", "x86_64-linux"),
        ],
        &[],
        "",
    );
    assert!(matches!(result, Err(DagError::InvalidDrvPath { .. })));

    let n = dag.node("auth-rb").expect("squat restored");
    assert_eq!(n.status(), DerivationStatus::Poisoned);
    assert_eq!(n.drv_content, b"Derive-A");
    assert!(n.drv_content_authoritative);
    assert_eq!(n.retry.resubmit_cycles, POISON_RESUBMIT_RETRY_LIMIT);
    assert_eq!(n.interested_builds, HashSet::from([squatter]));
    Ok(())
}

// r[verify sched.merge.authoritative-conflict+6]
/// A poison-budget-exhausted authoritative squat is terminal and gets
/// displaced by the conflicting verifiable (store-backed) definition —
/// fresh node without inherited interest or failure history, surfaced in
/// `MergeResult::displaced` (and NOT in `reset_on_resubmit`).
#[test]
fn conflicting_identity_displaces_poisoned_over_budget_squat() -> anyhow::Result<()> {
    use crate::state::POISON_RESUBMIT_RETRY_LIMIT;
    let mut dag = DerivationDag::new();
    let squatter = Uuid::new_v4();
    let victim = Uuid::new_v4();

    dag.merge(
        squatter,
        &[authoritative_node("squat3", b"Derive-A")],
        &[],
        "",
    )?;
    {
        let n = dag.nodes.get_mut("squat3").unwrap();
        n.set_status_for_test(DerivationStatus::Poisoned);
        n.retry.resubmit_cycles = POISON_RESUBMIT_RETRY_LIMIT;
    }

    let mut node = make_node("squat3", "aarch64-linux");
    node.is_content_addressed = true;
    let res = dag.merge(victim, &[node], &[], "")?;

    assert!(res.newly_inserted.contains("squat3"));
    assert!(res.displaced.iter().any(|h| h.as_str() == "squat3"));
    assert!(res.reset_on_resubmit.is_empty());
    let n = dag.node("squat3").unwrap();
    assert_eq!(n.system, "aarch64-linux");
    assert!(n.drv_content.is_empty());
    assert!(!n.drv_content_authoritative);
    assert_eq!(n.interested_builds, HashSet::from([victim]));
    assert_eq!(n.retry.resubmit_cycles, 0);
    Ok(())
}

// r[verify sched.merge.authoritative-conflict+6]
/// Cancelled is terminal: a conflicting store-backed definition takes the
/// displacement path (fresh node, no inherited interest), NOT the
/// interest-carrying resubmit-reset.
#[test]
fn conflicting_identity_displaces_cancelled_authoritative_node_without_interest()
-> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let squatter = Uuid::new_v4();
    let victim = Uuid::new_v4();

    dag.merge(
        squatter,
        &[authoritative_node("squat4", b"Derive-A")],
        &[],
        "",
    )?;
    dag.nodes
        .get_mut("squat4")
        .unwrap()
        .set_status_for_test(DerivationStatus::Cancelled);

    let mut node = make_node("squat4", "aarch64-linux");
    node.is_content_addressed = true;
    let res = dag.merge(victim, &[node], &[], "")?;

    assert!(res.newly_inserted.contains("squat4"));
    assert!(res.displaced.iter().any(|h| h.as_str() == "squat4"));
    assert!(res.reset_on_resubmit.is_empty());
    let n = dag.node("squat4").unwrap();
    assert!(!n.interested_builds.contains(&squatter));
    assert!(n.interested_builds.contains(&victim));
    assert!(!n.drv_content_authoritative);
    Ok(())
}

// r[verify sched.merge.authoritative-conflict+6]
/// Failed is NOT terminal (the retry machinery still owns it): a
/// conflicting store-backed submission is rejected, not displaced.
#[test]
fn conflicting_identity_rejected_while_failed() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let squatter = Uuid::new_v4();
    let victim = Uuid::new_v4();

    dag.merge(
        squatter,
        &[authoritative_node("squat5", b"Derive-A")],
        &[],
        "",
    )?;
    dag.nodes
        .get_mut("squat5")
        .unwrap()
        .set_status_for_test(DerivationStatus::Failed);

    let mut node = make_node("squat5", "aarch64-linux");
    node.is_content_addressed = true;
    let err = dag.merge(victim, &[node], &[], "").unwrap_err();
    assert!(matches!(err, DagError::ConflictingInFlightContent { .. }));
    let n = dag.node("squat5").unwrap();
    assert_eq!(n.status(), DerivationStatus::Failed);
    assert_eq!(n.drv_content, b"Derive-A");
    assert!(n.drv_content_authoritative);
    assert!(!n.interested_builds.contains(&victim));
    Ok(())
}

// r[verify sched.merge.authoritative-conflict+6]
/// A merge that displaces a poisoned-over-budget squat but fails on a
/// LATER node in the same submission must restore the squat exactly
/// (status, bytes, interest, poison accumulator) — displacement rides the
/// same rollback container as the resubmit-reset.
#[test]
fn rollback_restores_a_displaced_poisoned_squat() -> anyhow::Result<()> {
    use crate::state::POISON_RESUBMIT_RETRY_LIMIT;
    let mut dag = DerivationDag::new();
    let squatter = Uuid::new_v4();
    let victim = Uuid::new_v4();

    dag.merge(
        squatter,
        &[authoritative_node("squat6", b"Derive-A")],
        &[],
        "",
    )?;
    {
        let n = dag.nodes.get_mut("squat6").unwrap();
        n.set_status_for_test(DerivationStatus::Poisoned);
        n.retry.resubmit_cycles = POISON_RESUBMIT_RETRY_LIMIT;
    }

    let mut displacing = make_node("squat6", "aarch64-linux");
    displacing.is_content_addressed = true;
    let result = dag.merge(
        victim,
        &[
            displacing,
            make_node_with_path("bad", "not-a-store-path", "x86_64-linux"),
        ],
        &[],
        "",
    );
    assert!(matches!(result, Err(DagError::InvalidDrvPath { .. })));

    let n = dag.node("squat6").expect("squat restored");
    assert_eq!(n.status(), DerivationStatus::Poisoned);
    assert_eq!(n.drv_content, b"Derive-A");
    assert!(n.drv_content_authoritative);
    assert_eq!(n.system, "x86_64-linux");
    assert_eq!(n.retry.resubmit_cycles, POISON_RESUBMIT_RETRY_LIMIT);
    assert_eq!(n.interested_builds, HashSet::from([squatter]));
    Ok(())
}

// ── sched.merge.displaced-edge-scrub ────────────────────────────────────

// r[verify sched.merge.displaced-edge-scrub+2]
/// Displacement scrubs the squatter's dependency (children) edges: the
/// displacing fresh node's initial state is computed against ITS OWN
/// declared dependency set (none here), not the squatter's — whether the
/// squatter-attached child is terminally failed (which would otherwise
/// seed the fresh node `DependencyFailed`) or merely incomplete (which
/// would park it `Queued` forever).
#[rstest]
#[case::terminal_failed_child(DerivationStatus::Poisoned)]
#[case::incomplete_child(DerivationStatus::Queued)]
fn displacement_scrubs_inherited_dependency_edges(
    #[case] junk_status: DerivationStatus,
) -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let squatter = Uuid::new_v4();
    let victim = Uuid::new_v4();

    // Squatter: authoritative squat plus its own junk node, with an
    // attacker-attached dependency edge squat → junk.
    dag.merge(
        squatter,
        &[
            authoritative_node("edge-squat", b"Derive-squat"),
            make_node("edge-junk", "x86_64-linux"),
        ],
        &[make_edge("edge-squat", "edge-junk")],
        "",
    )?;
    dag.nodes
        .get_mut("edge-junk")
        .unwrap()
        .set_status_for_test(junk_status);
    dag.nodes
        .get_mut("edge-squat")
        .unwrap()
        .set_status_for_test(DerivationStatus::DependencyFailed);

    // Victim: conflicting store-backed identity (different system)
    // displaces the parked terminal squat.
    let mut victim_node = make_node("edge-squat", "aarch64-linux");
    victim_node.is_content_addressed = true;
    let res = dag.merge(victim, &[victim_node], &[], "")?;
    assert!(res.displaced.iter().any(|h| h.as_str() == "edge-squat"));

    // The fresh node's dependency set is exactly the displacing
    // submission's (none): the squatter's edge is gone in BOTH directions.
    assert!(
        dag.get_children("edge-squat").is_empty(),
        "squatter's squat→junk edge scrubbed"
    );
    assert!(
        !dag.get_parents("edge-junk")
            .iter()
            .any(|h| h.as_str() == "edge-squat"),
        "junk child no longer lists the displaced hash as a dependent"
    );
    // And the initial-state seed reflects that: Ready, not
    // DependencyFailed (terminal junk) or Queued (incomplete junk).
    let states: HashMap<_, _> = dag
        .compute_initial_states(&res.newly_inserted)
        .into_iter()
        .collect();
    assert_eq!(
        states["edge-squat"],
        DerivationStatus::Ready,
        "displacing node seeds from its own (empty) dependency set"
    );
    Ok(())
}

// r[verify sched.merge.displaced-edge-scrub+2]
/// The scrub is children-direction only: nodes that DEPEND ON the
/// displaced hash keep their edges (they want its output, whichever
/// definition produces it), while the displaced node's own dependency
/// edges are dropped.
#[test]
fn displacement_preserves_dependent_edges() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let squatter = Uuid::new_v4();
    let victim = Uuid::new_v4();

    // dependent → squat → junk
    dag.merge(
        squatter,
        &[
            make_node("edge-dependent", "x86_64-linux"),
            authoritative_node("edge-squat-p", b"Derive-squat"),
            make_node("edge-junk-p", "x86_64-linux"),
        ],
        &[
            make_edge("edge-dependent", "edge-squat-p"),
            make_edge("edge-squat-p", "edge-junk-p"),
        ],
        "",
    )?;
    dag.nodes
        .get_mut("edge-squat-p")
        .unwrap()
        .set_status_for_test(DerivationStatus::DependencyFailed);

    let mut victim_node = make_node("edge-squat-p", "aarch64-linux");
    victim_node.is_content_addressed = true;
    let res = dag.merge(victim, &[victim_node], &[], "")?;
    assert!(res.displaced.iter().any(|h| h.as_str() == "edge-squat-p"));

    // Children direction scrubbed…
    assert!(
        dag.get_children("edge-squat-p").is_empty(),
        "squat→junk scrubbed"
    );
    assert!(
        !dag.get_parents("edge-junk-p")
            .iter()
            .any(|h| h.as_str() == "edge-squat-p")
    );
    // …parents direction preserved.
    assert!(
        dag.get_children("edge-dependent")
            .iter()
            .any(|h| h.as_str() == "edge-squat-p"),
        "dependent→squat edge preserved"
    );
    assert!(
        dag.get_parents("edge-squat-p")
            .iter()
            .any(|h| h.as_str() == "edge-dependent"),
        "displaced hash still lists its dependent"
    );
    Ok(())
}

// r[verify sched.merge.displaced-edge-scrub+2]
/// A merge that displaces a squat (scrubbing its dependency edges) but
/// fails on a LATER node in the same submission must restore the squat
/// WITH its scrubbed edges — the pre-merge DAG exactly.
#[test]
fn rollback_restores_displaced_dependency_edges() -> anyhow::Result<()> {
    use crate::state::POISON_RESUBMIT_RETRY_LIMIT;
    let mut dag = DerivationDag::new();
    let squatter = Uuid::new_v4();
    let victim = Uuid::new_v4();

    dag.merge(
        squatter,
        &[
            authoritative_node("edge-squat-rb", b"Derive-squat"),
            make_node("edge-junk-rb", "x86_64-linux"),
        ],
        &[make_edge("edge-squat-rb", "edge-junk-rb")],
        "",
    )?;
    {
        let n = dag.nodes.get_mut("edge-squat-rb").unwrap();
        n.set_status_for_test(DerivationStatus::Poisoned);
        n.retry.resubmit_cycles = POISON_RESUBMIT_RETRY_LIMIT;
    }

    let mut displacing = make_node("edge-squat-rb", "aarch64-linux");
    displacing.is_content_addressed = true;
    let result = dag.merge(
        victim,
        &[
            displacing,
            make_node_with_path("bad", "not-a-store-path", "x86_64-linux"),
        ],
        &[],
        "",
    );
    assert!(matches!(result, Err(DagError::InvalidDrvPath { .. })));

    // The squat is restored together with its dependency edge.
    let n = dag.node("edge-squat-rb").expect("squat restored");
    assert_eq!(n.status(), DerivationStatus::Poisoned);
    assert!(n.drv_content_authoritative);
    assert!(
        dag.get_children("edge-squat-rb")
            .iter()
            .any(|h| h.as_str() == "edge-junk-rb"),
        "scrubbed squat→junk edge restored on rollback"
    );
    assert!(
        dag.get_parents("edge-junk-rb")
            .iter()
            .any(|h| h.as_str() == "edge-squat-rb"),
        "reverse direction restored too"
    );
    Ok(())
}

// r[verify sched.merge.displaced-edge-scrub+2]
/// The authority takeover through the resubmit-reset (identity-matching
/// store-backed resubmission of a parked, still-retriable authoritative
/// claim) is a definition change: the squat's dependency (children)
/// edges must not carry onto the taken-over node — whether the
/// squatter-attached child is terminally failed (would seed
/// `DependencyFailed`) or merely incomplete (would park it `Queued`).
/// The dependent (parents) direction is preserved.
#[rstest]
#[case::terminal_failed_child(DerivationStatus::Poisoned)]
#[case::incomplete_child(DerivationStatus::Queued)]
fn authority_takeover_scrubs_inherited_dependency_edges(
    #[case] junk_status: DerivationStatus,
) -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let squatter = Uuid::new_v4();
    let victim = Uuid::new_v4();

    // Squatter: dependent → squat → junk, squat carries authoritative
    // bytes and parks retriable (Poisoned under budget).
    dag.merge(
        squatter,
        &[
            authoritative_node("ats-squat", b"Derive-ats"),
            make_node("ats-junk", "x86_64-linux"),
            make_node("ats-dependent", "x86_64-linux"),
        ],
        &[
            make_edge("ats-squat", "ats-junk"),
            make_edge("ats-dependent", "ats-squat"),
        ],
        "",
    )?;
    dag.nodes
        .get_mut("ats-junk")
        .unwrap()
        .set_status_for_test(junk_status);
    {
        let n = dag.nodes.get_mut("ats-squat").unwrap();
        n.set_status_for_test(DerivationStatus::Poisoned);
        n.retry.resubmit_cycles = 1; // under POISON_RESUBMIT_RETRY_LIMIT
    }

    // Victim: identity-matching store-backed resubmission → authority
    // takeover via the resubmit-reset (NOT displacement).
    let mut takeover = make_node("ats-squat", "x86_64-linux");
    takeover.is_content_addressed = true;
    takeover.ca_modular_hash = Some([0xAB; 32]);
    let res = dag.merge(victim, &[takeover], &[], "")?;

    assert!(
        res.reset_on_resubmit
            .iter()
            .any(|h| h.as_str() == "ats-squat"),
        "takeover goes through the resubmit-reset"
    );
    assert!(
        res.authority_takeovers
            .iter()
            .any(|h| h.as_str() == "ats-squat")
    );
    assert!(res.displaced.is_empty(), "not a displacement");

    // Children direction scrubbed…
    assert!(
        dag.get_children("ats-squat").is_empty(),
        "squat→junk edge must not carry onto the taken-over definition"
    );
    assert!(
        !dag.get_parents("ats-junk")
            .iter()
            .any(|h| h.as_str() == "ats-squat"),
        "junk no longer lists the taken-over hash as a dependent"
    );
    // …parents direction preserved, interest carried.
    assert!(
        dag.get_children("ats-dependent")
            .iter()
            .any(|h| h.as_str() == "ats-squat"),
        "dependent→squat edge preserved"
    );
    assert!(
        dag.get_parents("ats-squat")
            .iter()
            .any(|h| h.as_str() == "ats-dependent")
    );
    let n = dag.node("ats-squat").unwrap();
    assert!(n.interested_builds.contains(&squatter), "interest carried");
    assert!(n.interested_builds.contains(&victim));

    // Initial state seeds from the takeover's own (empty) dependency set.
    let states: HashMap<_, _> = dag
        .compute_initial_states(&res.newly_inserted)
        .into_iter()
        .collect();
    assert_eq!(
        states["ats-squat"],
        DerivationStatus::Ready,
        "taken-over node seeds Ready, not from the squatter's junk child"
    );
    Ok(())
}

// r[verify sched.merge.displaced-edge-scrub+2]
/// A merge that takes over a parked authoritative claim (scrubbing its
/// dependency edges) but fails on a LATER node in the same submission
/// must restore the squat verbatim — status, authoritative flag,
/// resubmit budget, and its children edges in both directions.
#[test]
fn rollback_restores_takeover_scrubbed_edges() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let squatter = Uuid::new_v4();
    let victim = Uuid::new_v4();

    dag.merge(
        squatter,
        &[
            authoritative_node("ats-rb-squat", b"Derive-ats-rb"),
            make_node("ats-rb-junk", "x86_64-linux"),
        ],
        &[make_edge("ats-rb-squat", "ats-rb-junk")],
        "",
    )?;
    {
        let n = dag.nodes.get_mut("ats-rb-squat").unwrap();
        n.set_status_for_test(DerivationStatus::Poisoned);
        n.retry.resubmit_cycles = 1;
    }

    // Identity-matching store-backed takeover plus an invalid second
    // node: the merge fails AFTER the takeover scrub ran.
    let mut takeover = make_node("ats-rb-squat", "x86_64-linux");
    takeover.is_content_addressed = true;
    takeover.ca_modular_hash = Some([0xAB; 32]);
    let result = dag.merge(
        victim,
        &[
            takeover,
            make_node_with_path("bad", "not-a-store-path", "x86_64-linux"),
        ],
        &[],
        "",
    );
    assert!(matches!(result, Err(DagError::InvalidDrvPath { .. })));

    let n = dag.node("ats-rb-squat").expect("squat restored");
    assert_eq!(n.status(), DerivationStatus::Poisoned);
    assert!(n.drv_content_authoritative, "authoritative claim restored");
    assert_eq!(n.retry.resubmit_cycles, 1, "consumed budget restored");
    assert!(
        dag.get_children("ats-rb-squat")
            .iter()
            .any(|h| h.as_str() == "ats-rb-junk"),
        "scrubbed squat→junk edge restored on rollback"
    );
    assert!(
        dag.get_parents("ats-rb-junk")
            .iter()
            .any(|h| h.as_str() == "ats-rb-squat"),
        "reverse direction restored too"
    );
    Ok(())
}

// r[verify sched.merge.displaced-edge-scrub+2]
/// Contrast pin: a SAME-definition resubmit (store-backed → store-backed)
/// keeps the node's dependency edges — the scrub is scoped to definition
/// changes only.
#[test]
fn same_definition_resubmit_keeps_dependency_edges() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let b1 = Uuid::new_v4();
    let b2 = Uuid::new_v4();

    dag.merge(
        b1,
        &[
            make_node("sdr-x", "x86_64-linux"),
            make_node("sdr-dep", "x86_64-linux"),
        ],
        &[make_edge("sdr-x", "sdr-dep")],
        "",
    )?;
    dag.nodes
        .get_mut("sdr-x")
        .unwrap()
        .set_status_for_test(DerivationStatus::Failed);

    let res = dag.merge(b2, &[make_node("sdr-x", "x86_64-linux")], &[], "")?;
    assert!(
        res.reset_on_resubmit.iter().any(|h| h.as_str() == "sdr-x"),
        "same-definition resubmit-reset"
    );
    assert!(res.authority_takeovers.is_empty());
    assert!(
        dag.get_children("sdr-x")
            .iter()
            .any(|h| h.as_str() == "sdr-dep"),
        "same-definition reset keeps its dependency edges"
    );
    // Parked behind its (incomplete) dependency, exactly as before.
    let states: HashMap<_, _> = dag
        .compute_initial_states(&res.newly_inserted)
        .into_iter()
        .collect();
    assert_eq!(states["sdr-x"], DerivationStatus::Queued);
    Ok(())
}

// ── sched.merge.edge-creation-scoped ────────────────────────────────────

// r[verify sched.merge.edge-creation-scoped]
/// A submission that merely JOINS a resident node may not extend its
/// dependency set: the foreign edge is skipped (recorded, not inserted,
/// not part of `new_edges`) and the resident node's readiness is decided
/// by its own dependency set, not the joiner's junk child.
#[test]
fn test_foreign_parent_edge_skipped_on_resident_join() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let b1 = Uuid::new_v4();
    let b2 = Uuid::new_v4();

    // b1 creates X (no dependencies).
    dag.merge(b1, &[make_node("fps-x", "x86_64-linux")], &[], "")?;

    // b2 joins X and tries to attach its own junk child Y to it.
    let res = dag.merge(
        b2,
        &[
            make_node("fps-x", "x86_64-linux"),
            make_node("fps-y", "x86_64-linux"),
        ],
        &[make_edge("fps-x", "fps-y")],
        "",
    )?;

    assert!(res.new_edges.is_empty(), "foreign edge must not be added");
    assert_eq!(
        res.foreign_parent_edges_skipped.len(),
        1,
        "skip recorded for observability"
    );
    assert_eq!(res.foreign_parent_edges_skipped[0].0.as_str(), "fps-x");
    assert_eq!(res.foreign_parent_edges_skipped[0].1.as_str(), "fps-y");
    // r[verify sched.merge.heal-accepted-edges+1]
    assert!(
        !res.healed_parents
            .iter()
            .any(|(h, _)| h.as_str() == "fps-x"),
        "a parent with a gate-skipped declared edge must not be healed"
    );
    assert!(
        res.newly_inserted.contains("fps-y"),
        "the junk node itself is admitted (it is b2's own node)"
    );
    assert!(
        res.interest_added.iter().any(|h| h.as_str() == "fps-x"),
        "b2 still joins X"
    );
    assert!(
        dag.get_children("fps-x").is_empty(),
        "X's dependency set is unchanged"
    );

    // Even with the junk child terminally failed, X is unaffected — the
    // edge never existed.
    dag.nodes
        .get_mut("fps-y")
        .unwrap()
        .set_status_for_test(DerivationStatus::Poisoned);
    assert!(
        !dag.any_dep_terminally_failed("fps-x"),
        "X must not be poisoned-by-association with the skipped junk child"
    );
    Ok(())
}

// r[verify sched.merge.edge-creation-scoped]
/// The topdown-pruned carve-out: a resident pruned root deliberately had
/// its dependency edges dropped by its creating submission, so a later
/// full merge MAY top them up without re-creating the root.
#[test]
fn test_topdown_pruned_resident_parent_accepts_dep_topup() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let b1 = Uuid::new_v4();
    let b2 = Uuid::new_v4();

    // b1 creates R alone (the topdown prune dropped its deps).
    dag.merge(b1, &[make_node("tdp-r", "x86_64-linux")], &[], "")?;
    dag.nodes.get_mut("tdp-r").unwrap().topdown_pruned = true;

    // b2 full-merges {app, R, glibc} with app→R and R→glibc, without
    // re-creating R (it is resident and live).
    let res = dag.merge(
        b2,
        &[
            make_node("tdp-app", "x86_64-linux"),
            make_node("tdp-r", "x86_64-linux"),
            make_node("tdp-glibc", "x86_64-linux"),
        ],
        &[
            make_edge("tdp-app", "tdp-r"),
            make_edge("tdp-r", "tdp-glibc"),
        ],
        "",
    )?;

    assert!(
        res.foreign_parent_edges_skipped.is_empty(),
        "pruned-root top-up must not be treated as a foreign edge"
    );
    assert_eq!(res.new_edges.len(), 2, "both edges accepted");
    // r[verify sched.merge.heal-accepted-edges+1]
    assert!(
        res.healed_parents
            .iter()
            .any(|(h, _)| h.as_str() == "tdp-r")
            && res
                .healed_parents
                .iter()
                .any(|(h, _)| h.as_str() == "tdp-app"),
        "every parent whose declared edges were all accepted is healed \
         (topdown carve-out and newly-inserted alike)"
    );
    assert!(
        dag.get_children("tdp-r")
            .iter()
            .any(|h| h.as_str() == "tdp-glibc"),
        "R gained its dependency"
    );
    assert!(
        dag.get_children("tdp-app")
            .iter()
            .any(|h| h.as_str() == "tdp-r"),
        "app→R accepted (parent newly inserted)"
    );
    Ok(())
}

// r[verify sched.merge.edge-creation-scoped]
/// A displacing submission (re)creates the displaced hash, so its own
/// dependency edges for that hash are attached — displacement is a
/// re-creation, not a join.
#[test]
fn test_displacing_submission_attaches_own_edges() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let squatter = Uuid::new_v4();
    let victim = Uuid::new_v4();

    // Authoritative squat parked in a terminal failure state.
    dag.merge(
        squatter,
        &[authoritative_node("ecs-h", b"Derive-squat")],
        &[],
        "",
    )?;
    dag.nodes
        .get_mut("ecs-h")
        .unwrap()
        .set_status_for_test(DerivationStatus::DependencyFailed);

    // Victim: conflicting store-backed identity displaces the squat and
    // declares its own dependency H→D2 in the same submission.
    let mut displacing = make_node("ecs-h", "aarch64-linux");
    displacing.is_content_addressed = true;
    let res = dag.merge(
        victim,
        &[displacing, make_node("ecs-d2", "aarch64-linux")],
        &[make_edge("ecs-h", "ecs-d2")],
        "",
    )?;

    assert!(res.displaced.iter().any(|h| h.as_str() == "ecs-h"));
    assert!(
        res.foreign_parent_edges_skipped.is_empty(),
        "displacement is a re-creation: its own edges are not foreign"
    );
    // r[verify sched.merge.heal-accepted-edges+1]
    assert!(
        res.healed_parents
            .iter()
            .any(|(h, _)| h.as_str() == "ecs-h"),
        "a displacing re-creation whose edges were all accepted is healed"
    );
    assert!(
        res.new_edges
            .iter()
            .any(|(p, c)| p.as_str() == "ecs-h" && c.as_str() == "ecs-d2"),
        "displacing submission's own edge attached"
    );
    assert!(
        dag.get_children("ecs-h")
            .iter()
            .any(|h| h.as_str() == "ecs-d2"),
        "fresh node's dependency set is the displacing submission's"
    );
    Ok(())
}

// ── sched.merge.heal-accepted-edges+1 ─────────────────────────────────────

// r[verify sched.merge.heal-accepted-edges+1]
/// A joining submission with one accepted re-declaration AND one
/// gate-skipped extension is NOT healed: the partial acceptance means its
/// declared set is not what the DAG holds, so reap-truncation evidence
/// must not be cleared on its strength.
#[test]
fn test_healed_parents_excludes_gate_skipped_parent() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let b1 = Uuid::new_v4();
    let b2 = Uuid::new_v4();

    // b1 creates X→D (X's true dependency set).
    dag.merge(
        b1,
        &[
            make_node("hgs-x", "x86_64-linux"),
            make_node("hgs-d", "x86_64-linux"),
        ],
        &[make_edge("hgs-x", "hgs-d")],
        "",
    )?;

    // b2 joins X, re-declares X→D (accepted no-op) and tries to extend
    // X→J (gate-skipped). One veto → X is not healed.
    let res = dag.merge(
        b2,
        &[
            make_node("hgs-x", "x86_64-linux"),
            make_node("hgs-d", "x86_64-linux"),
            make_node("hgs-j", "x86_64-linux"),
        ],
        &[make_edge("hgs-x", "hgs-d"), make_edge("hgs-x", "hgs-j")],
        "",
    )?;

    assert_eq!(
        res.foreign_parent_edges_skipped.len(),
        1,
        "the extension edge is gate-skipped"
    );
    assert!(
        !res.healed_parents
            .iter()
            .any(|(h, _)| h.as_str() == "hgs-x"),
        "one gate-skipped declared edge vetoes the parent's heal even \
         though another declared edge was accepted"
    );
    Ok(())
}

// r[verify sched.merge.heal-accepted-edges+1]
/// A joining submission whose declared edges are ALL exact
/// re-declarations of existing edges IS healed: its declared set and the
/// DAG's child set agree, which is exactly the "child set is
/// representative again" condition the heal exists for.
#[test]
fn test_healed_parents_includes_already_present_redeclaration() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let b1 = Uuid::new_v4();
    let b2 = Uuid::new_v4();

    dag.merge(
        b1,
        &[
            make_node("hrd-x", "x86_64-linux"),
            make_node("hrd-d", "x86_64-linux"),
        ],
        &[make_edge("hrd-x", "hrd-d")],
        "",
    )?;

    // b2 joins and re-declares the full existing edge set, extending
    // nothing.
    let res = dag.merge(
        b2,
        &[
            make_node("hrd-x", "x86_64-linux"),
            make_node("hrd-d", "x86_64-linux"),
        ],
        &[make_edge("hrd-x", "hrd-d")],
        "",
    )?;

    assert!(res.new_edges.is_empty(), "re-declaration adds nothing");
    assert!(res.foreign_parent_edges_skipped.is_empty());
    assert!(
        res.healed_parents
            .iter()
            .any(|(h, _)| h.as_str() == "hrd-x"),
        "a parent whose every declared edge is an accepted re-declaration is healed"
    );
    Ok(())
}

// r[verify sched.merge.heal-accepted-edges+1]
/// THE merged_bug_073 kill test: a holed parent whose full merge
/// re-declares only the SURVIVING children (an exact, fully-accepted
/// subset of what the DAG already holds) must NOT heal — acceptance of
/// every declared edge is the laundering channel when the declared set
/// silently omits what the reap removed. Coverage demands the missing
/// child back.
// r[verify sched.evidence.positive-witness]
#[test]
fn test_subset_redeclaration_does_not_heal_closure_hole() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let b1 = Uuid::new_v4();
    let b2 = Uuid::new_v4();

    // b1 creates X→{S, M}; a truncation removes M and stamps the witness.
    dag.merge(
        b1,
        &[
            make_node("shx-x", "x86_64-linux"),
            make_node("shx-s", "x86_64-linux"),
            make_node("shx-m", "x86_64-linux"),
        ],
        &[make_edge("shx-x", "shx-s"), make_edge("shx-x", "shx-m")],
        "",
    )?;
    dag.remove_node(&"shx-m".into());
    dag.nodes
        .get_mut("shx-x")
        .unwrap()
        .closure_hole
        .stamp(["shx-m".into()]);

    // b2 re-creates X re-declaring ONLY the survivor: every declared
    // edge is accepted (exact re-declaration), the trigger fires —
    // and the heal must still be refused.
    let res = dag.merge(
        b2,
        &[
            make_node("shx-x", "x86_64-linux"),
            make_node("shx-s", "x86_64-linux"),
        ],
        &[make_edge("shx-x", "shx-s")],
        "",
    )?;
    assert!(
        res.foreign_parent_edges_skipped.is_empty() && res.rejoin_parent_edges_skipped.is_empty(),
        "fixture premise: the subset re-declaration is fully accepted"
    );
    assert!(
        res.accepted_edge_parents
            .iter()
            .any(|h| h.as_str() == "shx-x"),
        "fixture premise: X is in the accepted trigger set"
    );
    assert!(
        !res.healed_parents
            .iter()
            .any(|(h, _)| h.as_str() == "shx-x"),
        "a fully-accepted SUBSET re-declaration must not heal: shx-m is \
         missing and was not re-supplied"
    );
    assert!(
        res.heal_refused_parents
            .iter()
            .any(|h| h.as_str() == "shx-x"),
        "the refusal is surfaced, not silent"
    );
    assert!(
        dag.nodes.get("shx-x").unwrap().closure_hole.is_holed(),
        "the witness set survives the refused heal"
    );
    Ok(())
}

// r[verify sched.merge.heal-accepted-edges+1]
/// Junk top-up refused: re-supplying SOMETHING is not re-supplying the
/// MISSING thing. A re-creation that attaches a brand-new child while
/// still omitting the reaped one keeps the hole.
// r[verify sched.evidence.positive-witness]
#[test]
fn test_junk_topup_does_not_heal_closure_hole() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let b1 = Uuid::new_v4();
    let b2 = Uuid::new_v4();
    dag.merge(
        b1,
        &[
            make_node("jtx-x", "x86_64-linux"),
            make_node("jtx-m", "x86_64-linux"),
        ],
        &[make_edge("jtx-x", "jtx-m")],
        "",
    )?;
    dag.remove_node(&"jtx-m".into());
    {
        let x = dag.nodes.get_mut("jtx-x").unwrap();
        x.closure_hole.stamp(["jtx-m".into()]);
        // Pruned root: the carve-out is what ADMITS the top-up edge at
        // all (a plain resident join's extensions are gate-skipped and
        // vetoed before coverage is even consulted).
        x.topdown_pruned = true;
    }

    let res = dag.merge(
        b2,
        &[
            make_node("jtx-x", "x86_64-linux"),
            make_node("jtx-j", "x86_64-linux"),
        ],
        &[make_edge("jtx-x", "jtx-j")],
        "",
    )?;
    assert!(
        !res.healed_parents
            .iter()
            .any(|(h, _)| h.as_str() == "jtx-x"),
        "a new child that is not the missing child does not cover the witness"
    );
    assert!(
        res.heal_refused_parents
            .iter()
            .any(|h| h.as_str() == "jtx-x"),
        "junk top-up is a refused heal"
    );
    assert!(dag.nodes.get("jtx-x").unwrap().closure_hole.is_holed());
    Ok(())
}

// r[verify sched.merge.heal-accepted-edges+1]
/// Coverage matrix: full re-supply heals (witness consumed); partial
/// re-supply of a multi-child witness is refused.
#[test]
fn test_heal_coverage_full_vs_partial() -> anyhow::Result<()> {
    for (resupply_both, expect_heal) in [(true, true), (false, false)] {
        let mut dag = DerivationDag::new();
        let b1 = Uuid::new_v4();
        let b2 = Uuid::new_v4();
        dag.merge(
            b1,
            &[
                make_node("cvx-x", "x86_64-linux"),
                make_node("cvx-m1", "x86_64-linux"),
                make_node("cvx-m2", "x86_64-linux"),
            ],
            &[make_edge("cvx-x", "cvx-m1"), make_edge("cvx-x", "cvx-m2")],
            "",
        )?;
        dag.remove_node(&"cvx-m1".into());
        dag.remove_node(&"cvx-m2".into());
        {
            let x = dag.nodes.get_mut("cvx-x").unwrap();
            x.closure_hole.stamp(["cvx-m1".into(), "cvx-m2".into()]);
            x.topdown_pruned = true;
        }

        let mut nodes = vec![
            make_node("cvx-x", "x86_64-linux"),
            make_node("cvx-m1", "x86_64-linux"),
        ];
        let mut edges = vec![make_edge("cvx-x", "cvx-m1")];
        if resupply_both {
            nodes.push(make_node("cvx-m2", "x86_64-linux"));
            edges.push(make_edge("cvx-x", "cvx-m2"));
        }
        let res = dag.merge(b2, &nodes, &edges, "")?;
        assert_eq!(
            res.healed_parents
                .iter()
                .any(|(h, _)| h.as_str() == "cvx-x"),
            expect_heal,
            "full coverage heals; partial coverage is refused (both={resupply_both})"
        );
        assert_eq!(
            res.heal_refused_parents
                .iter()
                .any(|h| h.as_str() == "cvx-x"),
            !expect_heal,
        );
    }
    Ok(())
}

// r[verify sched.merge.heal-accepted-edges+1]
/// The recovery LOST_WITNESS sentinel is uncoverable by construction:
/// no re-supply can name it, so the heal stays refused until operator
/// intervention re-creates the truncation record.
#[test]
fn test_lost_witness_sentinel_never_heals() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let b1 = Uuid::new_v4();
    let b2 = Uuid::new_v4();
    dag.merge(
        b1,
        &[
            make_node("lwx-x", "x86_64-linux"),
            make_node("lwx-d", "x86_64-linux"),
        ],
        &[make_edge("lwx-x", "lwx-d")],
        "",
    )?;
    // Recovery found the flag set but the side rows gone.
    dag.nodes.get_mut("lwx-x").unwrap().closure_hole =
        crate::state::ClosureHole::from_recovery_flag(true);

    let res = dag.merge(
        b2,
        &[
            make_node("lwx-x", "x86_64-linux"),
            make_node("lwx-d", "x86_64-linux"),
        ],
        &[make_edge("lwx-x", "lwx-d")],
        "",
    )?;
    assert!(
        !res.healed_parents
            .iter()
            .any(|(h, _)| h.as_str() == "lwx-x"),
        "the sentinel cannot be covered — fail-closed"
    );
    assert!(
        res.heal_refused_parents
            .iter()
            .any(|h| h.as_str() == "lwx-x")
    );
    Ok(())
}

// r[verify sched.merge.heal-accepted-edges+1]
/// Scale shape: a 10k-child witness set is covered by a full re-supply
/// in one merge — the subset check is witness-set-sized (≤ the parent's
/// direct out-degree), never closure-sized.
#[test]
fn test_heal_coverage_scales_to_wide_witness() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let b1 = Uuid::new_v4();
    let b2 = Uuid::new_v4();
    let n = 10_000;
    let child = |i: usize| format!("wsx-c{i}");
    let mut nodes = vec![make_node("wsx-x", "x86_64-linux")];
    let mut edges = Vec::with_capacity(n);
    for i in 0..n {
        nodes.push(make_node(&child(i), "x86_64-linux"));
        edges.push(make_edge("wsx-x", &child(i)));
    }
    dag.merge(b1, &nodes, &edges, "")?;
    for i in 0..n {
        dag.remove_node(&child(i).into());
    }
    {
        let x = dag.nodes.get_mut("wsx-x").unwrap();
        x.closure_hole
            .stamp((0..n).map(|i| crate::dag::DrvHash::from(child(i))));
        // Pruned root, so the re-supply edges are admitted (see the
        // junk-topup test).
        x.topdown_pruned = true;
    }

    let res = dag.merge(b2, &nodes, &edges, "")?;
    assert!(
        res.healed_parents
            .iter()
            .any(|(h, _)| h.as_str() == "wsx-x"),
        "full 10k re-supply covers the witness"
    );
    assert!(
        dag.nodes.get("wsx-x").unwrap().closure_hole.is_holed(),
        "merge only computes the heal; the hole must stay stamped until \
         the actor clears it with the witness — see the actor-side heal \
         test"
    );
    Ok(())
}

// r[verify sched.merge.heal-accepted-edges+1]
/// All three creation-scope admission arms produce healed parents: a
/// newly-inserted parent, a topdown-pruned resident parent taking its
/// dependency top-up, and a resubmit-reset re-creation.
#[test]
fn test_healed_parents_creation_scoped_parents_included() -> anyhow::Result<()> {
    // Arm 1: newly-inserted parent.
    {
        let mut dag = DerivationDag::new();
        let res = dag.merge(
            Uuid::new_v4(),
            &[
                make_node("hcs-new", "x86_64-linux"),
                make_node("hcs-dep", "x86_64-linux"),
            ],
            &[make_edge("hcs-new", "hcs-dep")],
            "",
        )?;
        assert!(
            res.healed_parents
                .iter()
                .any(|(h, _)| h.as_str() == "hcs-new"),
            "newly-inserted parent with accepted edges is healed"
        );
    }

    // Arm 2: topdown-pruned resident parent accepting its top-up.
    {
        let mut dag = DerivationDag::new();
        dag.merge(
            Uuid::new_v4(),
            &[make_node("hcs-r", "x86_64-linux")],
            &[],
            "",
        )?;
        dag.nodes.get_mut("hcs-r").unwrap().topdown_pruned = true;
        // Simulate the truncation breadcrumb the top-up is healing —
        // the missing child is the one the top-up RE-SUPPLIES
        // (heal-accepted-edges+1 coverage; a top-up of anything else
        // is refused, see test_junk_topup_does_not_heal_closure_hole).
        dag.nodes
            .get_mut("hcs-r")
            .unwrap()
            .closure_hole
            .stamp(["hcs-glibc".into()]);
        let res = dag.merge(
            Uuid::new_v4(),
            &[
                make_node("hcs-r", "x86_64-linux"),
                make_node("hcs-glibc", "x86_64-linux"),
            ],
            &[make_edge("hcs-r", "hcs-glibc")],
            "",
        )?;
        assert!(
            res.healed_parents
                .iter()
                .any(|(h, _)| h.as_str() == "hcs-r"),
            "topdown-pruned resident parent taking its top-up is healed"
        );
        assert!(
            dag.nodes.get("hcs-r").unwrap().closure_hole.is_holed(),
            "merge() only COMPUTES healed_parents; the closure_hole \
             mutation itself is actor-side"
        );
    }

    // Arm 3: resubmit-reset re-creation of a retriable node.
    {
        let mut dag = DerivationDag::new();
        let b1 = Uuid::new_v4();
        dag.merge(
            b1,
            &[
                make_node("hcs-f", "x86_64-linux"),
                make_node("hcs-d2", "x86_64-linux"),
            ],
            &[make_edge("hcs-f", "hcs-d2")],
            "",
        )?;
        dag.nodes
            .get_mut("hcs-f")
            .unwrap()
            .set_status_for_test(DerivationStatus::Failed);
        // Resubmit re-creates the failed node with the same edges.
        let res = dag.merge(
            Uuid::new_v4(),
            &[
                make_node("hcs-f", "x86_64-linux"),
                make_node("hcs-d2", "x86_64-linux"),
            ],
            &[make_edge("hcs-f", "hcs-d2")],
            "",
        )?;
        assert!(
            res.reset_on_resubmit.iter().any(|h| h.as_str() == "hcs-f"),
            "fixture premise: the resubmit reset fired"
        );
        assert!(
            res.healed_parents
                .iter()
                .any(|(h, _)| h.as_str() == "hcs-f"),
            "a resubmit-reset re-creation with accepted edges is healed"
        );
    }
    Ok(())
}

// r[verify sched.merge.heal-accepted-edges+1]
/// An edge whose child endpoint does not resolve vetoes its parent's
/// heal: the parent's declared set could not be fully attached, so its
/// child set is not representative of its closure.
#[test]
fn test_healed_parents_unresolvable_child_endpoint_vetoes_parent() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();

    // One submission declaring P→missing (the child node is not part of
    // the submission and not resident — e.g. a non-gRPC driver bug).
    let res = dag.merge(
        Uuid::new_v4(),
        &[make_node("huc-p", "x86_64-linux")],
        &[make_edge("huc-p", "huc-missing")],
        "",
    )?;

    assert!(res.new_edges.is_empty(), "unresolvable edge is skipped");
    assert!(
        !res.healed_parents
            .iter()
            .any(|(h, _)| h.as_str() == "huc-p"),
        "an unresolvable child endpoint vetoes the parent's heal"
    );
    Ok(())
}

// r[verify sched.merge.heal-accepted-edges+1]
/// Gate-skips are classified by the parent's closure_hole breadcrumb:
/// holed parent → rejoin signature (separate vec/metric, debug-level);
/// un-holed parent → hostile/bug signature (existing vec/metric, warn).
/// Both shapes veto the heal.
#[test]
fn test_foreign_skip_classified_by_parent_hole() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let b1 = Uuid::new_v4();

    // Two resident nodes created without children; one carries the
    // truncation breadcrumb.
    dag.merge(
        b1,
        &[
            make_node("cls-holed", "x86_64-linux"),
            make_node("cls-clean", "x86_64-linux"),
        ],
        &[],
        "",
    )?;
    dag.nodes
        .get_mut("cls-holed")
        .unwrap()
        .closure_hole
        .stamp(["cls-reaped-child".into()]);

    // A later join tries to attach a child to each.
    let res = dag.merge(
        Uuid::new_v4(),
        &[
            make_node("cls-holed", "x86_64-linux"),
            make_node("cls-clean", "x86_64-linux"),
            make_node("cls-dep", "x86_64-linux"),
        ],
        &[
            make_edge("cls-holed", "cls-dep"),
            make_edge("cls-clean", "cls-dep"),
        ],
        "",
    )?;

    assert_eq!(
        res.rejoin_parent_edges_skipped.len(),
        1,
        "the holed parent's skip is classified as a rejoin"
    );
    assert_eq!(res.rejoin_parent_edges_skipped[0].0.as_str(), "cls-holed");
    assert_eq!(
        res.foreign_parent_edges_skipped.len(),
        1,
        "the clean parent's skip is classified as hostile/bug"
    );
    assert_eq!(res.foreign_parent_edges_skipped[0].0.as_str(), "cls-clean");
    assert!(
        !res.healed_parents
            .iter()
            .any(|(h, _)| h.as_str() == "cls-holed")
            && !res
                .healed_parents
                .iter()
                .any(|(h, _)| h.as_str() == "cls-clean"),
        "both skip shapes veto the heal"
    );
    assert!(
        dag.nodes.get("cls-holed").unwrap().closure_hole.is_holed(),
        "the rejoin-shaped skip does not clear the hole (only a \
         re-creation can)"
    );
    Ok(())
}

// r[verify sched.merge.authoritative-conflict+6]
/// Floating-CA squat scenario: public attributes (system, output names,
/// flags) are copyable from the victim's public derivation and floating-CA
/// expected paths are empty by construction, so a store-backed submission
/// with NO content evidence (no CA modular hash, or a different one) must
/// NOT silently join an in-flight authoritative node.
#[test]
fn floating_ca_squat_without_evidence_conflicts_in_flight() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let squatter = Uuid::new_v4();
    let victim = Uuid::new_v4();

    dag.merge(
        squatter,
        &[authoritative_node("squat-ev", b"Derive-A")],
        &[],
        "",
    )?;

    // Same public attributes, no modular hash at all → no evidence.
    let mut no_hash = make_node("squat-ev", "x86_64-linux");
    no_hash.is_content_addressed = true;
    let err = dag.merge(victim, &[no_hash], &[], "").unwrap_err();
    assert!(matches!(err, DagError::ConflictingInFlightContent { .. }));

    // Same public attributes, DIFFERENT modular hash → still no evidence.
    let mut wrong_hash = make_node("squat-ev", "x86_64-linux");
    wrong_hash.is_content_addressed = true;
    wrong_hash.ca_modular_hash = Some([0xCD; 32]);
    let err = dag.merge(victim, &[wrong_hash], &[], "").unwrap_err();
    assert!(matches!(err, DagError::ConflictingInFlightContent { .. }));

    let node = dag.node("squat-ev").unwrap();
    assert!(node.drv_content_authoritative, "squat untouched");
    assert!(!node.interested_builds.contains(&victim));
    Ok(())
}

// r[verify sched.merge.authoritative-conflict+6]
/// Once the no-evidence conflict target sits in a terminal FAILURE
/// state, the store-backed definition displaces it instead of being
/// rejected — same displacement semantics as any other
/// verifiable-identity conflict. (A settled Completed/Skipped target is
/// rejected instead — see
/// `conflicting_identity_rejected_on_settled_authoritative_node`.)
#[test]
fn floating_ca_squat_without_evidence_displaced_when_terminal() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let squatter = Uuid::new_v4();
    let victim = Uuid::new_v4();

    dag.merge(
        squatter,
        &[authoritative_node("squat-ev2", b"Derive-A")],
        &[],
        "",
    )?;
    dag.nodes
        .get_mut("squat-ev2")
        .unwrap()
        .set_status_for_test(DerivationStatus::Poisoned);

    let mut no_hash = make_node("squat-ev2", "x86_64-linux");
    no_hash.is_content_addressed = true;
    let res = dag.merge(victim, &[no_hash], &[], "")?;
    assert!(res.newly_inserted.contains("squat-ev2"));
    assert!(res.displaced.iter().any(|h| h.as_str() == "squat-ev2"));
    let node = dag.node("squat-ev2").unwrap();
    assert!(node.drv_content.is_empty());
    assert!(!node.drv_content_authoritative);
    assert!(node.interested_builds.contains(&victim));
    assert!(!node.interested_builds.contains(&squatter));
    Ok(())
}

// r[verify sched.merge.authoritative-conflict+6]
/// Fixed-output derivations carry their content commitment in the expected
/// output path (derived from the declared hash and bound to the bytes at
/// ingress), so agreement on a non-empty path is sufficient evidence — no
/// modular hash required.
#[test]
fn fod_path_agreement_is_sufficient_evidence() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let b1 = Uuid::new_v4();
    let b2 = Uuid::new_v4();
    let fod_path = "/nix/store/ffffffffffffffffffffffffffffffff-fod-out";

    let mut fod_auth = authoritative_node("fod-join", b"Derive-FOD");
    fod_auth.is_fixed_output = true;
    fod_auth.expected_output_paths = vec![fod_path.to_string()];
    fod_auth.ca_modular_hash = None;
    dag.merge(b1, &[fod_auth], &[], "")?;

    let mut store_backed = make_node("fod-join", "x86_64-linux");
    store_backed.is_fixed_output = true;
    store_backed.is_content_addressed = true;
    store_backed.expected_output_paths = vec![fod_path.to_string()];
    let res = dag.merge(b2, &[store_backed], &[], "")?;
    assert!(res.newly_inserted.is_empty(), "joins, not displaced");
    let node = dag.node("fod-join").unwrap();
    assert!(node.interested_builds.contains(&b2));
    assert_eq!(node.drv_content, b"Derive-FOD", "bytes untouched");
    assert!(node.drv_content_authoritative);
    Ok(())
}

// r[verify sched.merge.authoritative-conflict+6]
/// The conflict gate must keep holding for a node REBUILT FROM PG after a
/// leader failover (bug_007): `from_poisoned_row` restores the
/// authoritative bytes/flag/identity, so a recovered poisoned squat is
/// judged exactly like the live node was — byte-different authoritative
/// content displaces the terminal-failure claim through the explicit
/// displacement path (never a silent adoption that carries the squat's
/// interest), the byte-identical resubmit is admitted, and a conflicting
/// store-backed definition is displaced rather than silently joined onto
/// attacker content.
#[test]
fn recovered_poisoned_squat_keeps_authoritative_gate() -> anyhow::Result<()> {
    fn recovered_squat(tag: &str) -> crate::state::DerivationState {
        let base = crate::db::RecoveryDerivationRow {
            drv_content: Some(b"Derive-A".to_vec()),
            is_ca: true,
            expected_output_paths: vec![String::new()],
            status: "poisoned".into(),
            ..crate::db::RecoveryDerivationRow::test_default(tag, "x86_64-linux")
        };
        crate::state::DerivationState::from_poisoned_row(crate::db::PoisonedDerivationRow {
            base,
            elapsed_secs: 60.0,
        })
        .expect("recovered poisoned row is valid")
    }

    // (a) Post-failover authoritative submission with DIFFERENT bytes:
    // the recovered squat is parked in a terminal failure state, so the
    // redefinition DISPLACES it — exactly like it would have pre-failover
    // — rather than being silently adopted through the resubmit-reset
    // (which would carry the squat's interest onto the new bytes and
    // consume its budget). Pre-fix the recovered stub carried
    // drv_content_authoritative=false and the gate did not hold at all.
    let mut dag = DerivationDag::new();
    dag.insert_recovered_node(recovered_squat("rec-squat"));
    let redefiner = Uuid::new_v4();
    let res = dag.merge(
        redefiner,
        &[authoritative_node("rec-squat", b"Derive-B")],
        &[],
        "",
    )?;
    assert!(
        res.displaced.iter().any(|h| h.as_str() == "rec-squat"),
        "explicit displacement, not a silent adoption"
    );
    assert!(res.reset_on_resubmit.is_empty());
    let node = dag.node("rec-squat").unwrap();
    assert!(node.drv_content_authoritative);
    assert_eq!(node.drv_content, b"Derive-B", "redefinition's bytes win");
    assert_ne!(node.status(), DerivationStatus::Poisoned, "fresh node");
    assert_eq!(node.retry.resubmit_cycles, 0, "fresh poison budget");
    assert_eq!(node.interested_builds, HashSet::from([redefiner]));

    // (b) Byte-identical authoritative resubmit (the legitimate hook
    // producer retrying after failover) is admitted through the normal
    // resubmit-reset, keeps the bytes, and carries the new interest.
    let mut dag = DerivationDag::new();
    dag.insert_recovered_node(recovered_squat("rec-retry"));
    let producer = Uuid::new_v4();
    let res = dag.merge(
        producer,
        &[authoritative_node("rec-retry", b"Derive-A")],
        &[],
        "",
    )?;
    assert!(res.reset_on_resubmit.contains(&"rec-retry".into()));
    let node = dag.node("rec-retry").unwrap();
    assert!(node.drv_content_authoritative);
    assert_eq!(node.drv_content, b"Derive-A");
    assert_ne!(node.status(), DerivationStatus::Poisoned);
    assert!(node.interested_builds.contains(&producer));

    // (c) A conflicting store-backed definition displaces the recovered
    // (terminal) squat instead of joining it — same as pre-failover.
    let mut dag = DerivationDag::new();
    dag.insert_recovered_node(recovered_squat("rec-displace"));
    let victim = Uuid::new_v4();
    let mut displacing = make_node("rec-displace", "aarch64-linux");
    displacing.is_content_addressed = true;
    let res = dag.merge(victim, &[displacing], &[], "")?;
    assert!(res.displaced.contains(&"rec-displace".into()));
    let node = dag.node("rec-displace").unwrap();
    assert!(!node.drv_content_authoritative);
    assert_eq!(node.system, "aarch64-linux");
    assert_eq!(node.interested_builds, HashSet::from([victim]));
    Ok(())
}

// r[verify sched.merge.evidence-ranked-displacement]
/// THE displacement primitive's verdict matrix: every cell of
/// (victim anchoring × victim status × victim rank × displacer rank)
/// that the contract distinguishes, asserted directly against
/// `displace()` so the decision order (store-anchored → in-flight →
/// settled-rank → displaced) and the rank rule cannot drift from the
/// spec. Includes the deliberate strict-inequality cell: a settled
/// `ContentBoundClaim` squat IS displaced by a `PathBoundBytes`
/// displacer (its bytes were text-CA-bound at ingress — no store
/// fetch needed), while `VerifiedBuilt` is unreachable by any
/// displacer.
#[rstest]
// Store-anchored victims: categorical refusal, regardless of status or ranks.
#[case::store_anchored_running(
    false,
    DerivationStatus::Running,
    DefinitionEvidence::UnverifiedClaim,
    DefinitionEvidence::PathBoundBytes,
    DisplaceVerdict::RefusedStoreAnchored
)]
#[case::store_anchored_poisoned(
    false,
    DerivationStatus::Poisoned,
    DefinitionEvidence::UnverifiedClaim,
    DefinitionEvidence::PathBoundBytes,
    DisplaceVerdict::RefusedStoreAnchored
)]
#[case::store_anchored_completed(
    false,
    DerivationStatus::Completed,
    DefinitionEvidence::VerifiedBuilt,
    DefinitionEvidence::PathBoundBytes,
    DisplaceVerdict::RefusedStoreAnchored
)]
// In-flight victims: live and Failed (non-terminal) keep first-writer-wins.
#[case::live_running(
    true,
    DerivationStatus::Running,
    DefinitionEvidence::ContentBoundClaim,
    DefinitionEvidence::PathBoundBytes,
    DisplaceVerdict::RefusedInFlight
)]
#[case::live_failed(
    true,
    DerivationStatus::Failed,
    DefinitionEvidence::ContentBoundClaim,
    DefinitionEvidence::PathBoundBytes,
    DisplaceVerdict::RefusedInFlight
)]
#[case::live_ready(
    true,
    DerivationStatus::Ready,
    DefinitionEvidence::ContentBoundClaim,
    DefinitionEvidence::VerifiedBuilt,
    DisplaceVerdict::RefusedInFlight
)]
// Terminal-failure victims: displaced REGARDLESS of rank (anti-squat).
#[case::failure_poisoned_low_displacer(
    true,
    DerivationStatus::Poisoned,
    DefinitionEvidence::VerifiedBuilt,
    DefinitionEvidence::UnverifiedClaim,
    DisplaceVerdict::Displaced
)]
#[case::failure_cancelled(
    true,
    DerivationStatus::Cancelled,
    DefinitionEvidence::ContentBoundClaim,
    DefinitionEvidence::UnverifiedClaim,
    DisplaceVerdict::Displaced
)]
#[case::failure_dep_failed(
    true,
    DerivationStatus::DependencyFailed,
    DefinitionEvidence::ContentBoundClaim,
    DefinitionEvidence::UnverifiedClaim,
    DisplaceVerdict::Displaced
)]
// Settled victims: strict-inequality rank rule.
#[case::settled_echo_refused(
    true,
    DerivationStatus::Completed,
    DefinitionEvidence::ContentBoundClaim,
    DefinitionEvidence::UnverifiedClaim,
    DisplaceVerdict::RefusedSettledOutranked
)]
#[case::settled_equal_refused(
    true,
    DerivationStatus::Completed,
    DefinitionEvidence::ContentBoundClaim,
    DefinitionEvidence::ContentBoundClaim,
    DisplaceVerdict::RefusedSettledOutranked
)]
#[case::settled_strictly_outranked_displaced(
    true,
    DerivationStatus::Completed,
    DefinitionEvidence::ContentBoundClaim,
    DefinitionEvidence::PathBoundBytes,
    DisplaceVerdict::Displaced
)]
#[case::settled_skipped_strictly_outranked(
    true,
    DerivationStatus::Skipped,
    DefinitionEvidence::ContentBoundClaim,
    DefinitionEvidence::PathBoundBytes,
    DisplaceVerdict::Displaced
)]
#[case::settled_verified_built_unreachable(
    true,
    DerivationStatus::Completed,
    DefinitionEvidence::VerifiedBuilt,
    DefinitionEvidence::PathBoundBytes,
    DisplaceVerdict::RefusedSettledOutranked
)]
#[case::settled_path_bound_vs_path_bound(
    true,
    DerivationStatus::Completed,
    DefinitionEvidence::PathBoundBytes,
    DefinitionEvidence::PathBoundBytes,
    DisplaceVerdict::RefusedSettledOutranked
)]
fn displace_verdict_matrix(
    #[case] victim_authoritative: bool,
    #[case] victim_status: DerivationStatus,
    #[case] victim_rank: DefinitionEvidence,
    #[case] displacer_rank: DefinitionEvidence,
    #[case] expected: DisplaceVerdict,
) -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let owner = Uuid::new_v4();
    let node = if victim_authoritative {
        authoritative_node("verdict", b"Derive-squat")
    } else {
        make_node("verdict", "x86_64-linux")
    };
    dag.merge(owner, &[node], &[], "")?;
    {
        let n = dag.nodes.get_mut("verdict").unwrap();
        n.set_status_for_test(victim_status);
        n.evidence = victim_rank;
    }

    let mut removed_retriable = Vec::new();
    let mut displaced_scrubbed_edges = Vec::new();
    let mut displaced = Vec::new();
    let verdict = dag.displace(
        &"verdict".into(),
        displacer_rank,
        &mut DisplacementBookkeeping {
            removed_retriable: &mut removed_retriable,
            displaced_scrubbed_edges: &mut displaced_scrubbed_edges,
            displaced: &mut displaced,
        },
    );
    assert_eq!(verdict, expected);

    if expected == DisplaceVerdict::Displaced {
        assert!(dag.node("verdict").is_none(), "victim removed");
        assert_eq!(removed_retriable.len(), 1, "prior state rides rollback");
        assert_eq!(removed_retriable[0].0.as_str(), "verdict");
        assert_eq!(removed_retriable[0].1.status(), victim_status);
        assert_eq!(displaced, vec![DrvHash::from("verdict")]);
    } else {
        let n = dag.node("verdict").expect("refusal leaves the victim");
        assert_eq!(n.status(), victim_status, "victim untouched");
        assert_eq!(n.evidence, victim_rank, "victim rank untouched");
        assert!(removed_retriable.is_empty());
        assert!(displaced.is_empty());
        assert!(displaced_scrubbed_edges.is_empty());
    }
    Ok(())
}

// r[verify sched.merge.evidence-ranked-displacement]
/// The deliberate strict-inequality cell END TO END through the merge
/// gate: an ingress-byte-bound store-backed submission (non-empty
/// non-authoritative `drv_content` → `PathBoundBytes` at ingress)
/// whose identity conflicts with a SETTLED content-bound squat
/// displaces it — no store fetch, no operator — while the bare echo
/// form of the same submission stays rejected. Pins the R7 sentence
/// of the spec rule.
#[test]
fn settled_squat_displaced_by_ingress_byte_bound_submission() -> anyhow::Result<()> {
    // Bare store-backed echo first: rejected (must-not-regress half).
    let mut dag = DerivationDag::new();
    let squatter = Uuid::new_v4();
    let victim = Uuid::new_v4();
    dag.merge(
        squatter,
        &[authoritative_node("squat", b"Derive-squat")],
        &[],
        "",
    )?;
    dag.nodes
        .get_mut("squat")
        .unwrap()
        .set_status_for_test(DerivationStatus::Completed);
    let mut echo = make_node("squat", "aarch64-linux");
    echo.is_content_addressed = true;
    let err = dag.merge(victim, &[echo.clone()], &[], "").unwrap_err();
    assert!(
        matches!(err, DagError::ConflictingInFlightContent { .. }),
        "bare echo (UnverifiedClaim) cannot erase a settled record: {err}"
    );
    assert!(dag.node("squat").is_some(), "squat survives the echo");

    // Same submission shape, but ingress-byte-bound: the inline bytes
    // were text-CA-validated at SubmitBuild admission
    // (sched.merge.ingress-inline-drv-binding), so the node ranks
    // PathBoundBytes and strictly outranks the settled
    // ContentBoundClaim squat.
    let mut byte_bound = echo;
    byte_bound.drv_content = b"Derive-genuine".to_vec();
    byte_bound.drv_content_authoritative = false;
    let res = dag.merge(victim, &[byte_bound], &[], "")?;
    assert!(
        res.displaced.contains(&"squat".into()),
        "ingress-byte-bound submission displaces the settled squat"
    );
    let node = dag.node("squat").unwrap();
    assert!(!node.drv_content_authoritative);
    assert_eq!(node.system, "aarch64-linux", "displacer's identity wins");
    assert_eq!(node.evidence, DefinitionEvidence::PathBoundBytes);
    assert_eq!(node.interested_builds, HashSet::from([victim]));
    Ok(())
}

// r[verify sched.merge.evidence-ranked-displacement]
/// merge_with_evidence's store-evidence set raises a bare store-backed
/// displacer to PathBoundBytes standing — the c3 enrichment's contract
/// with the gate — while the same submission without the set entry
/// stays rejected (the empty-set delegation of merge() is
/// behavior-identical to HEAD).
#[test]
fn store_evidence_set_raises_displacer_standing() -> anyhow::Result<()> {
    let mut dag = DerivationDag::new();
    let squatter = Uuid::new_v4();
    let victim = Uuid::new_v4();
    dag.merge(
        squatter,
        &[authoritative_node("sev", b"Derive-squat")],
        &[],
        "",
    )?;
    dag.nodes
        .get_mut("sev")
        .unwrap()
        .set_status_for_test(DerivationStatus::Completed);

    let mut echo = make_node("sev", "aarch64-linux");
    echo.is_content_addressed = true;
    // Forged echo: the submitter claims needs_resolve — the evidence
    // map's byte-derived value (false) must win on the created node.
    echo.needs_resolve = true;

    // Without evidence: rejected.
    let err = dag
        .merge_with_evidence(victim, &[echo.clone()], &[], "", &HashMap::new())
        .unwrap_err();
    assert!(matches!(err, DagError::ConflictingInFlightContent { .. }));

    // With the hash in the store-evidence set: displaced.
    let evidence: HashMap<DrvHash, bool> = HashMap::from([("sev".into(), false)]);
    let res = dag.merge_with_evidence(victim, &[echo], &[], "", &evidence)?;
    assert!(res.displaced.contains(&"sev".into()));
    assert_eq!(
        dag.node("sev").unwrap().evidence,
        DefinitionEvidence::PathBoundBytes,
        "store-evidence-backed creation ranks PathBoundBytes"
    );
    // r[verify sched.dispatch.claims-derived+2]
    assert!(
        !dag.node("sev").unwrap().ca.needs_resolve,
        "store-evidence-created node carries the BYTE-DERIVED resolve \
         flag from the map, not the submitter's forged echo"
    );
    Ok(())
}

// r[verify sched.merge.identity-hash-veto]
/// Resident-matcher twin: a present-but-differing modular hash vetoes
/// `verifiable_identity_matches` even when the (public, copyable)
/// expected output paths agree byte-for-byte. Pre-fix the differing
/// hash was treated as merely "no hash evidence" and path agreement
/// carried the match — letting a provably different definition join or
/// displace as identical.
#[test]
fn differing_modular_hash_vetoes_identity_match_despite_path_agreement() {
    let mint_existing = |hash: Option<[u8; 32]>| {
        let row = crate::db::RecoveryDerivationRow {
            is_ca: true,
            expected_output_paths: vec!["/nix/store/agreed-out".into()],
            ca_modular_hash: hash.map(|h| h.to_vec()),
            ..crate::db::RecoveryDerivationRow::test_default("hv", "x86_64-linux")
        };
        crate::state::DerivationState::from_recovery_row(row, DerivationStatus::Ready)
            .expect("state mints")
    };
    let mint_incoming = |hash: Option<[u8; 32]>| {
        let mut n = make_node("hv", "x86_64-linux");
        n.is_content_addressed = true;
        n.expected_output_paths = vec!["/nix/store/agreed-out".into()];
        n.ca_modular_hash = hash;
        n
    };

    let existing = mint_existing(Some([0xAA; 32]));
    assert!(
        !crate::dag::verifiable_identity_matches(&existing, &mint_incoming(Some([0xBB; 32]))),
        "present-but-differing hashes are a definition conflict; path \
         agreement must not override"
    );
    assert!(
        crate::dag::verifiable_identity_matches(&existing, &mint_incoming(Some([0xAA; 32]))),
        "byte-equal hashes still match"
    );
    assert!(
        crate::dag::verifiable_identity_matches(&existing, &mint_incoming(None)),
        "an absent incoming hash falls back to path evidence"
    );
    let no_hash_existing = mint_existing(None);
    assert!(
        crate::dag::verifiable_identity_matches(
            &no_hash_existing,
            &mint_incoming(Some([0xBB; 32]))
        ),
        "an absent existing hash falls back to path evidence"
    );
}
