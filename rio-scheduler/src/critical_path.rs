//! Critical-path priority computation.
//!
//! Priority = `est_duration + max(children's priority)`. Bottom-up: a
//! leaf's priority is just its own duration; a root's priority is the
//! sum of durations along the longest path through its subtree.
// r[impl sched.critical-path.incremental]
//!
//! Dispatching highest-priority-first means: work on the derivation
//! whose completion unblocks the most remaining work. A short derivation
//! that's on every build's critical path gets scheduled before a long
//! one that's off to the side.
//!
//! # Why this matters
//!
//! Without critical-path, FIFO dispatch means a 5-second derivation
//! submitted just after a 2-hour derivation waits 2 hours even if the
//! 5-second one is blocking 50 downstream builds. With critical-path,
//! the 5-second one gets priority `5 + sum(downstream)` which is huge.
//!
//! # Incremental vs full
//!
//! - `compute_initial`: full bottom-up sweep for newly-merged nodes.
//!   Called once per SubmitBuild. O(new nodes + affected existing nodes).
//! - `update_ancestors`: incremental walk UP from a completed node.
//!   Called once per completion. O(affected ancestors) — typically
//!   small (only ancestors whose max-child-priority CHANGED).
//! - Periodic full sweep on Tick (~60s) catches any drift from the
//!   incremental updates. Belt-and-suspenders.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::dag::DerivationDag;
use crate::estimator::Estimator;
use crate::state::DrvHash;

/// Compute priorities for newly-inserted nodes (bottom-up).
///
/// Called after `dag.merge()` in the actor. Populates `est_duration`
/// (from the Estimator) and `priority` (bottom-up from children).
///
/// Existing nodes are NOT recomputed unless a new subgraph connects to
/// them with a higher-priority path (scheduler.md:473). This means:
/// - A new build that shares an existing derivation doesn't disturb
///   the existing derivation's priority UNLESS the new build's subgraph
///   makes that derivation more critical (on a longer path).
///
/// # Algorithm
///
/// 1. For each newly-inserted node, set `est_duration` from Estimator.
/// 2. Topo-sort the newly-inserted nodes (children before parents). We
///    can't use a global topo-sort — the DAG has existing nodes mixed
///    in. Instead: BFS from leaves, tracking in-degree within the new set.
/// 3. In topo order, `priority = est_duration + max(child.priority)`.
///    Children might be existing nodes (already have priority) or
///    newly-inserted (processed earlier in topo order).
/// 4. For existing nodes that new edges connect to: if their priority
///    would increase, update it AND propagate up their ancestors
///    (same as `update_ancestors`).
pub fn compute_initial(
    dag: &mut DerivationDag,
    estimator: &Estimator,
    newly_inserted: &HashSet<DrvHash>,
) {
    if newly_inserted.is_empty() {
        return;
    }

    // --- Step 1: set est_duration for new nodes ---
    for hash in newly_inserted {
        if let Some(state) = dag.node_mut(hash) {
            let est = estimator.estimate(
                state.pname.as_deref(),
                &state.system,
                state.input_srcs_nar_size,
            );
            state.est_duration = est;
        }
    }

    // --- Step 2: topo-sort within the new set ---
    // In-degree = number of PARENTS in the new set. Leaves (in-degree
    // 0) go first. Kahn's algorithm.
    //
    // Priority flows UP from children to parents, so we need children
    // processed first. For Kahn's algorithm, edges point child→parent
    // (data flow). A node's in-degree = number of children in the new
    // set. When all children are processed (in-degree = 0), process
    // the node.
    let mut in_degree: HashMap<DrvHash, usize> = HashMap::new();
    for hash in newly_inserted {
        let children_in_new = dag
            .get_children(hash)
            .into_iter()
            .filter(|c| newly_inserted.contains(c))
            .count();
        in_degree.insert(hash.clone(), children_in_new);
    }

    let mut queue: VecDeque<DrvHash> = in_degree
        .iter()
        .filter(|&(_, &d)| d == 0)
        .map(|(h, _)| h.clone())
        .collect();

    let mut topo_order = Vec::with_capacity(newly_inserted.len());
    while let Some(hash) = queue.pop_front() {
        topo_order.push(hash.clone());
        // Processing this node → decrement parents' in-degree.
        for parent in dag.get_parents(&hash) {
            if let Some(d) = in_degree.get_mut(&parent) {
                *d -= 1;
                if *d == 0 {
                    queue.push_back(parent);
                }
            }
        }
    }

    // --- Step 3: compute priority in topo order ---
    for hash in &topo_order {
        let children = dag.get_children(hash);
        // Max child priority. Children might be new (just computed)
        // OR existing (already had priority from a prior merge).
        // Either way, their priority is set by now.
        //
        // f64::max with 0.0 as identity: a leaf (no children) gets
        // priority = est_duration + 0.0 = est_duration.
        let max_child: f64 = children
            .iter()
            .filter_map(|c| dag.node(c).map(|n| n.priority))
            .fold(0.0, f64::max);

        if let Some(state) = dag.node_mut(hash) {
            state.priority = state.est_duration + max_child;
        }
    }

    // --- Step 4: propagate to existing nodes if priority increased ---
    // A new edge might connect a new node to an existing one. The
    // existing node's priority might now need to go up (it's on a
    // longer path). Walk up from the TOPS of the new subgraph.
    //
    // "Tops" = newly-inserted nodes whose parents are NOT in
    // newly_inserted (i.e., they connect to existing nodes, or they're
    // roots).
    for hash in newly_inserted {
        let parents = dag.get_parents(hash);
        let has_existing_parent = parents.iter().any(|p| !newly_inserted.contains(p));
        if has_existing_parent {
            // This new node connects to existing. Propagate up.
            // update_ancestors takes the CHILD hash and walks up.
            update_ancestors(dag, hash);
        }
    }
}

/// Incrementally update ancestor priorities after a node's priority
/// might have changed (completion, or new subgraph connection).
///
/// Walks UP from `from` via parents. For each ancestor, recomputes
/// `priority = est_duration + max(non-terminal children's priority)`.
/// If unchanged → stop walking this branch (dirty-flag propagation).
///
/// # Why "non-terminal children"
///
/// A Completed child is no longer on the critical path — it's DONE.
/// Its work doesn't block anything. Excluding it from the max means
/// the parent's priority drops when a child completes, which is
/// correct: the parent is less urgent now (less remaining work below).
///
/// Poisoned/DependencyFailed children are also excluded — they'll
/// never complete, so including their priority would be misleading.
///
/// # O(affected ancestors)
///
/// The dirty-flag stop (unchanged → don't walk further) keeps this
/// small in practice. A completion typically only affects a few
/// ancestors before the max-child-priority stabilizes.
pub fn update_ancestors(dag: &mut DerivationDag, from: &str) {
    // BFS up. Visited set prevents re-processing in diamond-shaped DAGs
    // (A→B, A→C, B→D, C→D: walking up from D reaches A twice).
    let mut visited: HashSet<DrvHash> = HashSet::new();
    let mut queue: VecDeque<DrvHash> = dag.get_parents(from).into_iter().collect();

    while let Some(hash) = queue.pop_front() {
        if !visited.insert(hash.clone()) {
            continue;
        }

        let children = dag.get_children(&hash);
        // Max priority among NON-TERMINAL children. Terminal children
        // contribute 0 (their work is done or never-will-be-done).
        let max_child: f64 = children
            .iter()
            .filter_map(|c| {
                dag.node(c).and_then(|n| {
                    if n.status().is_terminal() {
                        None
                    } else {
                        Some(n.priority)
                    }
                })
            })
            .fold(0.0, f64::max);

        let Some(state) = dag.node_mut(&hash) else {
            continue;
        };

        let new_priority = state.est_duration + max_child;
        // Dirty-flag: if priority didn't change, our ancestors'
        // priorities won't change either (we're the only path through
        // us). Stop this branch.
        //
        // f64 exact equality is fine here: we're comparing a value we
        // SET to the same arithmetic we're about to do. Either the
        // inputs changed (different result) or they didn't (same
        // result, no float drift).
        if new_priority == state.priority {
            continue;
        }

        state.priority = new_priority;
        // Changed → walk further up.
        for parent in dag.get_parents(&hash) {
            queue.push_back(parent);
        }
    }
}

/// Full priority sweep of all non-terminal nodes.
///
/// Called periodically from Tick (~60s) as belt-and-suspenders over
/// the incremental updates. Catches any drift (rounding accumulation,
/// missed edge case in the incremental logic).
///
/// Cost: O(V + E) for the topo-sort + one pass. For a 10k-node DAG,
/// ~1ms. Every 60s is negligible.
pub fn full_sweep(dag: &mut DerivationDag, estimator: &Estimator) {
    // Collect all non-terminal hashes. Terminal nodes don't need
    // priority (they won't be dispatched).
    let non_terminal: HashSet<DrvHash> = dag
        .iter_nodes()
        .filter(|(_, s)| !s.status().is_terminal())
        .map(|(h, _)| h.into())
        .collect();

    // Reuse compute_initial — it's the same bottom-up algorithm.
    // Treating all non-terminal nodes as "newly inserted" is a valid
    // re-interpretation: compute_initial sets est_duration (idempotent;
    // same estimator, same result) and recomputes priority bottom-up.
    compute_initial(dag, estimator, &non_terminal);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::DerivationDag;
    use crate::state::DerivationStatus;
    use rio_test_support::fixtures::test_drv_path;
    use uuid::Uuid;

    /// Build a DerivationNode proto for test DAG construction.
    fn node(tag: &str) -> rio_proto::types::DerivationNode {
        rio_proto::types::DerivationNode {
            drv_path: test_drv_path(tag),
            drv_hash: tag.to_string(),
            pname: tag.to_string(),
            system: "x86_64-linux".into(),
            required_features: vec![],
            output_names: vec!["out".into()],
            is_fixed_output: false,
            expected_output_paths: vec![],
            drv_content: Vec::new(),
            input_srcs_nar_size: 0,
        }
    }

    fn edge(parent: &str, child: &str) -> rio_proto::types::DerivationEdge {
        rio_proto::types::DerivationEdge {
            parent_drv_path: test_drv_path(parent),
            child_drv_path: test_drv_path(child),
        }
    }

    /// Estimator with known values: a=10s, b=20s, c=30s, else default.
    fn test_estimator() -> Estimator {
        let mut e = Estimator::default();
        e.refresh(vec![
            ("a".into(), "x86_64-linux".into(), 10.0, None, None),
            ("b".into(), "x86_64-linux".into(), 20.0, None, None),
            ("c".into(), "x86_64-linux".into(), 30.0, None, None),
        ]);
        e
    }

    #[test]
    fn leaf_priority_equals_duration() {
        let mut dag = DerivationDag::new();
        let merge = dag.merge(Uuid::new_v4(), &[node("a")], &[], "").unwrap();

        compute_initial(&mut dag, &test_estimator(), &merge.newly_inserted);

        assert_eq!(dag.node("a").unwrap().priority, 10.0);
    }

    #[test]
    fn chain_priority_accumulates() {
        // a→b→c: a depends on b, b depends on c.
        // c is leaf: priority = 30
        // b: 20 + 30 = 50
        // a: 10 + 50 = 60
        let mut dag = DerivationDag::new();
        let merge = dag
            .merge(
                Uuid::new_v4(),
                &[node("a"), node("b"), node("c")],
                &[edge("a", "b"), edge("b", "c")],
                "",
            )
            .unwrap();

        compute_initial(&mut dag, &test_estimator(), &merge.newly_inserted);

        assert_eq!(dag.node("c").unwrap().priority, 30.0);
        assert_eq!(dag.node("b").unwrap().priority, 50.0);
        assert_eq!(dag.node("a").unwrap().priority, 60.0);
    }

    #[test]
    fn diamond_takes_max_path() {
        //     a
        //    / \
        //   b   c
        //    \ /
        //     d
        // d is leaf. b and c both have d as child.
        // a has BOTH b and c as children; priority = a_dur + max(b.prio, c.prio).
        //
        // Using default estimator (everything 30s):
        // d: 30
        // b: 30 + 30 = 60
        // c: 30 + 30 = 60
        // a: 30 + max(60, 60) = 90
        let mut dag = DerivationDag::new();
        let merge = dag
            .merge(
                Uuid::new_v4(),
                &[node("a"), node("b"), node("c"), node("d")],
                &[
                    edge("a", "b"),
                    edge("a", "c"),
                    edge("b", "d"),
                    edge("c", "d"),
                ],
                "",
            )
            .unwrap();

        let est = Estimator::default(); // everything = 30s
        compute_initial(&mut dag, &est, &merge.newly_inserted);

        assert_eq!(dag.node("a").unwrap().priority, 90.0);
    }

    #[test]
    fn new_subgraph_raises_existing_priority() {
        // First merge: a→b (a depends on b). b=20, a=10+20=30.
        let mut dag = DerivationDag::new();
        let merge1 = dag
            .merge(
                Uuid::new_v4(),
                &[node("a"), node("b")],
                &[edge("a", "b")],
                "",
            )
            .unwrap();
        compute_initial(&mut dag, &test_estimator(), &merge1.newly_inserted);
        assert_eq!(dag.node("a").unwrap().priority, 30.0);

        // Second merge: b depends on c (new). c=30. Now b has a child
        // with priority 30. b was 20; now 20+30=50. a should propagate:
        // 10+50=60.
        let merge2 = dag
            .merge(Uuid::new_v4(), &[node("c")], &[edge("b", "c")], "")
            .unwrap();
        compute_initial(&mut dag, &test_estimator(), &merge2.newly_inserted);

        // c is new: priority = 30 (leaf).
        assert_eq!(dag.node("c").unwrap().priority, 30.0);
        // b should have been RAISED: was 20, now 20+30=50.
        assert_eq!(
            dag.node("b").unwrap().priority,
            50.0,
            "existing b should get raised priority from new child c"
        );
        // a should propagate: was 30, now 10+50=60.
        assert_eq!(
            dag.node("a").unwrap().priority,
            60.0,
            "a should propagate from b's raise"
        );
    }

    // r[verify sched.critical-path.incremental]
    #[test]
    fn update_ancestors_on_completion() {
        // a→b→c. c completes. b's priority should DROP (c no longer
        // contributes). a should propagate.
        let mut dag = DerivationDag::new();
        let merge = dag
            .merge(
                Uuid::new_v4(),
                &[node("a"), node("b"), node("c")],
                &[edge("a", "b"), edge("b", "c")],
                "",
            )
            .unwrap();
        compute_initial(&mut dag, &test_estimator(), &merge.newly_inserted);
        assert_eq!(dag.node("b").unwrap().priority, 50.0);
        assert_eq!(dag.node("a").unwrap().priority, 60.0);

        // Complete c. (Normally completion.rs does this, then calls
        // update_ancestors.)
        dag.node_mut("c")
            .unwrap()
            .transition(DerivationStatus::Completed)
            .unwrap();
        update_ancestors(&mut dag, "c");

        // c is terminal → excluded from b's max-child. b: 20+0=20.
        assert_eq!(dag.node("b").unwrap().priority, 20.0);
        // a: 10+20=30.
        assert_eq!(dag.node("a").unwrap().priority, 30.0);
    }

    #[test]
    fn update_ancestors_stops_on_unchanged() {
        // a→b, a→c. b=20, c=30. a = 10 + max(20,30) = 40.
        // c completes. c is excluded. a's max-child is now b=20.
        // a: 10+20=30. Changed → would propagate (but a has no parents).
        //
        // Now: a→b, a→c, d→a. Complete b (smaller child). a's max
        // is still c=30. a unchanged → d should NOT be walked.
        let mut dag = DerivationDag::new();
        let merge = dag
            .merge(
                Uuid::new_v4(),
                &[node("a"), node("b"), node("c"), node("d")],
                &[edge("a", "b"), edge("a", "c"), edge("d", "a")],
                "",
            )
            .unwrap();
        let est = Estimator::default(); // all 30s
        compute_initial(&mut dag, &est, &merge.newly_inserted);

        // Verify initial: b=30, c=30, a=30+30=60, d=30+60=90.
        assert_eq!(dag.node("a").unwrap().priority, 60.0);
        assert_eq!(dag.node("d").unwrap().priority, 90.0);

        // Complete b. a's max-child: still c=30. a unchanged (60).
        // d should NOT be recomputed (dirty-flag stops at a).
        //
        // We can't directly observe "d wasn't touched", but we CAN
        // verify d's priority is still correct (90). If the stop
        // logic was broken, d might get wrongly recomputed using
        // a stale max (like treating a as 0).
        dag.node_mut("b")
            .unwrap()
            .transition(DerivationStatus::Completed)
            .unwrap();
        update_ancestors(&mut dag, "b");

        assert_eq!(dag.node("a").unwrap().priority, 60.0, "a unchanged");
        assert_eq!(dag.node("d").unwrap().priority, 90.0, "d unchanged");
    }

    #[test]
    fn full_sweep_idempotent() {
        let mut dag = DerivationDag::new();
        let merge = dag
            .merge(
                Uuid::new_v4(),
                &[node("a"), node("b"), node("c")],
                &[edge("a", "b"), edge("b", "c")],
                "",
            )
            .unwrap();
        let est = test_estimator();
        compute_initial(&mut dag, &est, &merge.newly_inserted);

        let before_a = dag.node("a").unwrap().priority;
        let before_b = dag.node("b").unwrap().priority;

        // Full sweep should produce the SAME priorities (same input).
        full_sweep(&mut dag, &est);

        assert_eq!(dag.node("a").unwrap().priority, before_a);
        assert_eq!(dag.node("b").unwrap().priority, before_b);
    }
}
