//! Critical-path priority computation.
//!
//! Priority = `est_duration + max(non-terminal children's priority)`.
//! Bottom-up: a leaf's priority is just its own duration; a root's
//! priority is the sum of durations along the longest *remaining* path
//! through its subtree.
// r[impl sched.critical-path.incremental]
//!
//! Dispatching highest-priority-first approximates LPT once a node's
//! children become terminal: a node's priority decays toward its own
//! `est_duration`, so long derivations start early enough to overlap
//! the tail. While children remain non-terminal, priority is the
//! longest-remaining-chain length — derivations on the critical chain
//! sort ahead of derivations on shorter side branches.
//!
//! # One recurrence, three entry points
//!
//! All three entry points run the same [`topo_recompute`] over a chosen
//! `scope` (children-first Kahn order, then `est_duration +
//! nonterminal_child_max(...)`). They differ only in scope:
//!
//! - [`compute_initial`]: scope = `newly_inserted` (per SubmitBuild),
//!   then propagates upward into pre-existing ancestors at the boundary.
//! - [`update_ancestors`]: scope = upward-reachable cone from a
//!   completed/changed node (per completion).
//! - [`full_sweep`]: scope = all non-terminal nodes (Tick, ~60s,
//!   belt-and-suspenders).
//!
//! Because every entry point reduces to the same recurrence, the spec
//! invariant — "incremental updates and the periodic full sweep
//! converge on the same priorities" — holds by construction.

use std::collections::{HashMap, HashSet, VecDeque};

use uuid::Uuid;

use crate::dag::DerivationDag;
use crate::sla::SlaEstimator;
use crate::sla::types::ModelKey;
use crate::state::{BuildInfo, DerivationState, DrvHash};

/// Fallback duration when the SLA cache has no fit for a key (cold
/// start, or `pname` absent so no key can be built). 60s sits between
/// "trivial" and "heavy" — close enough to the fleet median that a
/// fresh derivation neither hogs the front of the queue nor starves
/// behind known-long work. ADR-023's per-key fit replaces this after
/// the first completion.
pub const DEFAULT_DURATION_SECS: f64 = 60.0;

/// Build the SLA [`ModelKey`] for `state` via
/// [`DerivationState::attributed_tenant`] (shared with
/// `solve_intent_for` / `write_build_sample`). `None` when `pname` is
/// absent (raw/FOD derivation) — nothing to key on.
fn model_key_for(state: &DerivationState, builds: &HashMap<Uuid, BuildInfo>) -> Option<ModelKey> {
    Some(ModelKey {
        pname: state.pname.as_ref()?.clone(),
        system: state.system.clone(),
        tenant: state
            .attributed_tenant(builds)
            .map(|u| u.to_string())
            .unwrap_or_default(),
    })
}

/// Compute priorities for newly-inserted nodes (bottom-up).
///
/// Called after `dag.merge()` in the actor. Populates `est_duration`
/// (SLA `T_min` for cached keys, [`DEFAULT_DURATION_SECS`] otherwise)
/// and `priority` (bottom-up from children).
///
/// Existing nodes are NOT recomputed unless a new subgraph connects to
/// them with a higher-priority path (scheduler.md:473). This means:
/// - A new build that shares an existing derivation doesn't disturb
///   the existing derivation's priority UNLESS the new build's subgraph
///   makes that derivation more critical (on a longer path).
///
/// # Algorithm
///
/// 1. For each newly-inserted node, set `est_duration` from the SLA cache.
/// 2. [`topo_recompute`] over `newly_inserted`: children-first Kahn,
///    then `priority = est_duration + nonterminal_child_max(...)`.
///    Children might be existing nodes (already have priority) or
///    newly-inserted (processed earlier in topo order).
/// 3. For existing nodes that new edges connect to: propagate up their
///    ancestors via [`update_ancestors`].
pub fn compute_initial(
    dag: &mut DerivationDag,
    sla: &SlaEstimator,
    builds: &HashMap<Uuid, BuildInfo>,
    newly_inserted: &HashSet<DrvHash>,
) {
    if newly_inserted.is_empty() {
        return;
    }

    // --- Step 1: set est_duration for new nodes ---
    for hash in newly_inserted {
        if let Some(state) = dag.node_mut(hash) {
            state.sched.est_duration = model_key_for(state, builds)
                .and_then(|k| sla.ref_estimate(&k))
                .unwrap_or(DEFAULT_DURATION_SECS);
        }
    }

    // --- Steps 2+3: topo-sort + recompute within the new set ---
    topo_recompute(dag, newly_inserted);

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

/// `max(non-terminal children's priority)` — the recurrence's right
/// operand. Terminal children contribute 0: a Completed child is no
/// longer on the critical path (its work is done); a Poisoned/
/// DependencyFailed child will never complete (including its priority
/// would be misleading). A leaf returns 0.0.
fn nonterminal_child_max(dag: &DerivationDag, hash: &str) -> f64 {
    dag.get_children(hash)
        .iter()
        .filter_map(|c| {
            dag.node(c).and_then(|n| {
                if n.status().is_terminal() {
                    None
                } else {
                    Some(n.sched.priority)
                }
            })
        })
        .fold(0.0, f64::max)
}

/// Recompute `priority` for every node in `scope` in children-first
/// topo order via [`DerivationDag::kahn_topo`]. Reads children OUTSIDE
/// `scope` as-is (their priority is already settled). The single
/// implementation of the recurrence — all three entry points call
/// through here so they cannot diverge.
fn topo_recompute(dag: &mut DerivationDag, scope: &HashSet<DrvHash>) {
    for hash in dag.kahn_topo(scope) {
        let max_child = nonterminal_child_max(dag, &hash);
        if let Some(state) = dag.node_mut(&hash) {
            state.sched.priority = state.sched.est_duration + max_child;
        }
    }
}

/// Incrementally update ancestor priorities after a node's priority
/// might have changed (completion, or new subgraph connection).
///
/// BFS-collects the upward-reachable cone from `get_parents(from)`,
/// then [`topo_recompute`]s the whole cone. The Kahn order guarantees
/// every ancestor reads its children's *post-update* priorities — a
/// skip-level edge (E→D plus E→A→B→D, ubiquitous in Nix:
/// pkg→glibc + pkg→stdenv→…→glibc) cannot leave E reading A's stale
/// value, because Kahn won't emit E until A is processed. The previous
/// BFS+dirty-flag walk dequeued E at depth 1 before A at depth 2, and
/// the visited-set blocked the corrective re-visit.
///
/// O(ancestors + their out-degree) — same as the old BFS paid to walk
/// the cone; the dirty-flag early-stop is dropped (it was the
/// correctness hazard).
pub fn update_ancestors(dag: &mut DerivationDag, from: &str) {
    let mut cone: HashSet<DrvHash> = HashSet::new();
    let mut queue: VecDeque<DrvHash> = dag.get_parents(from).into_iter().collect();
    while let Some(hash) = queue.pop_front() {
        if cone.insert(hash.clone()) {
            for parent in dag.get_parents(&hash) {
                queue.push_back(parent);
            }
        }
    }
    topo_recompute(dag, &cone);
}

/// Full priority sweep of all non-terminal nodes.
///
/// Called periodically from Tick (~60s) as belt-and-suspenders over
/// the incremental updates. Catches any drift (rounding accumulation,
/// missed edge case in the incremental logic).
///
/// Cost: O(V + E) for the topo-sort + one pass. For a 10k-node DAG,
/// ~1ms. Every 60s is negligible.
pub fn full_sweep(dag: &mut DerivationDag, sla: &SlaEstimator, builds: &HashMap<Uuid, BuildInfo>) {
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
    // same SLA cache, same result) and recomputes priority bottom-up.
    compute_initial(dag, sla, builds, &non_terminal);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::DerivationDag;
    use crate::sla::types::{
        DurationFit, ExploreState, FittedParams, MemBytes, MemFit, RawCores, RefSeconds,
        WallSeconds,
    };
    use crate::state::DerivationStatus;
    use rio_test_support::fixtures::{make_derivation_node, make_edge};
    use uuid::Uuid;

    /// Build a domain `DerivationNode` for test DAG construction.
    ///
    /// `pname` is set to `tag` (not the fixture's `"test-pkg"` default)
    /// because the SLA cache keys on (pname, system, tenant) — the test
    /// assertions below depend on `node("a")` having `pname == "a"`.
    fn node(tag: &str) -> crate::domain::DerivationNode {
        crate::domain::DerivationNode {
            pname: tag.to_string(),
            ..make_derivation_node(tag, "x86_64-linux").into()
        }
    }

    fn edge(parent: &str, child: &str) -> crate::domain::DerivationEdge {
        make_edge(parent, child).into()
    }

    /// Seed one Amdahl fit with `T_min == s` (p=0 → t_at(∞)=s). Tenant
    /// `""` matches `model_key_for`'s attribution when `builds` is empty.
    fn seed(sla: &SlaEstimator, pname: &str, t_min: f64) {
        sla.seed(FittedParams {
            key: ModelKey {
                pname: pname.into(),
                system: "x86_64-linux".into(),
                tenant: String::new(),
            },
            fit: DurationFit::Amdahl {
                s: RefSeconds(t_min),
                p: RefSeconds(0.0),
            },
            mem: MemFit::Independent { p90: MemBytes(0) },
            disk_p90: None,
            sigma_resid: 0.0,
            log_residuals: vec![],
            n_eff: 5.0,
            span: 1.0,
            explore: ExploreState {
                distinct_c: 1,
                min_c: RawCores(1.0),
                max_c: RawCores(1.0),
                saturated: false,
                last_wall: WallSeconds(t_min),
            },
            t_min_ci: None,
            ci_computed_at: None,
            tier: None,
            hw_bias: HashMap::new(),
            prior_source: None,
        });
    }

    /// SLA cache seeded with known T_min: a=10s, b=20s, c=30s, else
    /// [`DEFAULT_DURATION_SECS`].
    fn test_sla() -> SlaEstimator {
        let e = SlaEstimator::new(&crate::sla::config::SlaConfig::test_default());
        seed(&e, "a", 10.0);
        seed(&e, "b", 20.0);
        seed(&e, "c", 30.0);
        e
    }

    /// Unfitted-key cache: every node falls back to [`DEFAULT_DURATION_SECS`].
    fn empty_sla() -> SlaEstimator {
        SlaEstimator::new(&crate::sla::config::SlaConfig::test_default())
    }

    fn no_builds() -> HashMap<Uuid, BuildInfo> {
        HashMap::new()
    }

    #[test]
    fn leaf_priority_equals_duration() {
        let mut dag = DerivationDag::new();
        let merge = dag.merge(Uuid::new_v4(), &[node("a")], &[], "").unwrap();

        compute_initial(&mut dag, &test_sla(), &no_builds(), &merge.newly_inserted);

        assert_eq!(dag.node("a").unwrap().sched.priority, 10.0);
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

        compute_initial(&mut dag, &test_sla(), &no_builds(), &merge.newly_inserted);

        assert_eq!(dag.node("c").unwrap().sched.priority, 30.0);
        assert_eq!(dag.node("b").unwrap().sched.priority, 50.0);
        assert_eq!(dag.node("a").unwrap().sched.priority, 60.0);
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
        // Using empty SLA cache (everything DEFAULT_DURATION_SECS=60s):
        // d: 60
        // b: 60 + 60 = 120
        // c: 60 + 60 = 120
        // a: 60 + max(120, 120) = 180
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

        compute_initial(&mut dag, &empty_sla(), &no_builds(), &merge.newly_inserted);

        assert_eq!(dag.node("a").unwrap().sched.priority, 180.0);
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
        compute_initial(&mut dag, &test_sla(), &no_builds(), &merge1.newly_inserted);
        assert_eq!(dag.node("a").unwrap().sched.priority, 30.0);

        // Second merge: b depends on c (new). c=30. Now b has a child
        // with priority 30. b was 20; now 20+30=50. a should propagate:
        // 10+50=60.
        let merge2 = dag
            .merge(Uuid::new_v4(), &[node("c")], &[edge("b", "c")], "")
            .unwrap();
        compute_initial(&mut dag, &test_sla(), &no_builds(), &merge2.newly_inserted);

        // c is new: priority = 30 (leaf).
        assert_eq!(dag.node("c").unwrap().sched.priority, 30.0);
        // b should have been RAISED: was 20, now 20+30=50.
        assert_eq!(
            dag.node("b").unwrap().sched.priority,
            50.0,
            "existing b should get raised priority from new child c"
        );
        // a should propagate: was 30, now 10+50=60.
        assert_eq!(
            dag.node("a").unwrap().sched.priority,
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
        compute_initial(&mut dag, &test_sla(), &no_builds(), &merge.newly_inserted);
        assert_eq!(dag.node("b").unwrap().sched.priority, 50.0);
        assert_eq!(dag.node("a").unwrap().sched.priority, 60.0);

        // Complete c. (Normally completion.rs does this, then calls
        // update_ancestors.)
        dag.node_mut("c")
            .unwrap()
            .transition(DerivationStatus::Completed)
            .unwrap();
        update_ancestors(&mut dag, "c");

        // c is terminal → excluded from b's max-child. b: 20+0=20.
        assert_eq!(dag.node("b").unwrap().sched.priority, 20.0);
        // a: 10+20=30.
        assert_eq!(dag.node("a").unwrap().sched.priority, 30.0);
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
        compute_initial(&mut dag, &empty_sla(), &no_builds(), &merge.newly_inserted);

        // Verify initial: b=60, c=60, a=60+60=120, d=60+120=180.
        assert_eq!(dag.node("a").unwrap().sched.priority, 120.0);
        assert_eq!(dag.node("d").unwrap().sched.priority, 180.0);

        // Complete b. a's max-child: still c=60. a unchanged (120).
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

        assert_eq!(dag.node("a").unwrap().sched.priority, 120.0, "a unchanged");
        assert_eq!(dag.node("d").unwrap().sched.priority, 180.0, "d unchanged");
    }

    /// bug_041: `full_sweep` previously read terminal children's stale
    /// pre-completion priorities (step 3 had no `is_terminal()` filter),
    /// reverting the decrement that `update_ancestors` correctly applied.
    /// After the unification both go through `nonterminal_child_max`.
    // r[verify sched.critical-path.incremental]
    #[test]
    fn full_sweep_excludes_terminal_children() {
        // a→b→c, a/b/c = 10/20/30s.
        let mut dag = DerivationDag::new();
        let merge = dag
            .merge(
                Uuid::new_v4(),
                &[node("a"), node("b"), node("c")],
                &[edge("a", "b"), edge("b", "c")],
                "",
            )
            .unwrap();
        let sla = test_sla();
        compute_initial(&mut dag, &sla, &no_builds(), &merge.newly_inserted);
        assert_eq!(dag.node("a").unwrap().sched.priority, 60.0);

        // Complete c; incremental update drops b→20, a→30.
        dag.node_mut("c")
            .unwrap()
            .transition(DerivationStatus::Completed)
            .unwrap();
        update_ancestors(&mut dag, "c");
        assert_eq!(dag.node("b").unwrap().sched.priority, 20.0);
        assert_eq!(dag.node("a").unwrap().sched.priority, 30.0);

        // Full sweep must agree, not revert a to 60.
        full_sweep(&mut dag, &sla, &no_builds());
        assert_eq!(
            dag.node("b").unwrap().sched.priority,
            20.0,
            "full_sweep must exclude terminal c from b's max-child"
        );
        assert_eq!(
            dag.node("a").unwrap().sched.priority,
            30.0,
            "full_sweep must not revert a (was: re-included completed c → 60)"
        );
    }

    /// bug_433: skip-level edge E→D plus E→A→B→D. The old BFS+visited
    /// dequeued E at depth 1 before A at depth 2; E read A's stale
    /// priority and the visited-set blocked the corrective re-visit.
    /// Kahn over the cone makes the bug unrepresentable.
    // r[verify sched.critical-path.incremental]
    #[test]
    fn update_ancestors_skip_level_converges() {
        // E→A, A→B, B→D, E→D (skip-level). All 60s (empty SLA).
        let mut dag = DerivationDag::new();
        let merge = dag
            .merge(
                Uuid::new_v4(),
                &[node("E"), node("A"), node("B"), node("D")],
                &[
                    edge("E", "A"),
                    edge("A", "B"),
                    edge("B", "D"),
                    edge("E", "D"),
                ],
                "",
            )
            .unwrap();
        compute_initial(&mut dag, &empty_sla(), &no_builds(), &merge.newly_inserted);
        // D=60, B=120, A=180, E=max(A,D)+60=240.
        assert_eq!(dag.node("E").unwrap().sched.priority, 240.0);

        // Complete D. B→60, A→120, E→max(A=120, D-terminal)+60=180.
        dag.node_mut("D")
            .unwrap()
            .transition(DerivationStatus::Completed)
            .unwrap();
        update_ancestors(&mut dag, "D");

        assert_eq!(dag.node("B").unwrap().sched.priority, 60.0);
        assert_eq!(dag.node("A").unwrap().sched.priority, 120.0);
        assert_eq!(
            dag.node("E").unwrap().sched.priority,
            180.0,
            "E must read A's POST-update priority (was: stale 180 → E=240)"
        );
    }

    /// Structural invariant: incrementally completing leaves with
    /// `update_ancestors` after each MUST yield the same priorities as
    /// a `full_sweep` from the same DAG state. Seeded-random 50-node
    /// DAG; deterministic across runs.
    // r[verify sched.critical-path.incremental]
    #[test]
    fn full_sweep_agrees_with_incremental() {
        use rand::{RngExt, SeedableRng, rngs::StdRng};

        let mut rng = StdRng::seed_from_u64(0xDA6C1);
        const N: usize = 50;
        let nodes: Vec<_> = (0..N).map(|i| node(&format!("n{i:02}"))).collect();
        // Each node i>0 depends on up to 3 random earlier nodes (acyclic
        // by construction, with skip-level edges).
        let mut edges = Vec::new();
        for i in 1..N {
            let fanout = rng.random_range(1..=3.min(i));
            let mut picked: HashSet<usize> = HashSet::new();
            while picked.len() < fanout {
                picked.insert(rng.random_range(0..i));
            }
            for j in picked {
                edges.push(edge(&format!("n{i:02}"), &format!("n{j:02}")));
            }
        }
        let mut dag = DerivationDag::new();
        let merge = dag.merge(Uuid::new_v4(), &nodes, &edges, "").unwrap();
        let sla = empty_sla();
        compute_initial(&mut dag, &sla, &no_builds(), &merge.newly_inserted);

        // Complete n00..n09 in dependency order (n_i depends only on
        // j<i, so this is always a valid completion prefix) — exercises
        // 10 sequential update_ancestors calls against state mutated by
        // prior calls. Previously a leaf-filter yielded exactly ["n00"]
        // (fanout≥1 for all i≥1) so only 1 iteration ran (bug_115).
        for i in 0..10 {
            let h = format!("n{i:02}");
            dag.node_mut(&h)
                .unwrap()
                .transition(DerivationStatus::Completed)
                .unwrap();
            update_ancestors(&mut dag, &h);
        }
        let snapshot: HashMap<String, f64> = (0..N)
            .map(|i| {
                let h = format!("n{i:02}");
                let p = dag.node(&h).unwrap().sched.priority;
                (h, p)
            })
            .collect();

        full_sweep(&mut dag, &sla, &no_builds());

        for (h, p) in &snapshot {
            assert_eq!(
                dag.node(h).unwrap().sched.priority,
                *p,
                "full_sweep diverged from incremental at {h}"
            );
        }
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
        let sla = test_sla();
        compute_initial(&mut dag, &sla, &no_builds(), &merge.newly_inserted);

        let before_a = dag.node("a").unwrap().sched.priority;
        let before_b = dag.node("b").unwrap().sched.priority;

        // Full sweep should produce the SAME priorities (same input).
        full_sweep(&mut dag, &sla, &no_builds());

        assert_eq!(dag.node("a").unwrap().sched.priority, before_a);
        assert_eq!(dag.node("b").unwrap().sched.priority, before_b);
    }
}
