//! Worker scoring and selection.
//!
//! Replaces `dispatch.rs`'s first-eligible-worker FIFO with a scored
//! selection: pick the worker that will complete this derivation
//! FASTEST, accounting for transfer cost (inputs not yet cached) and
//! current load.
//!
//! # Scoring (scheduler.md:53-59)
//!
//! `score = transfer_cost * W_locality + load_fraction * W_load`
//!
//! Both terms in [0, 1]. Lowest score wins.
//!
//! - **transfer_cost**: normalized count of input paths the worker
//!   DOESN'T have cached (via bloom filter). A worker with everything
//!   cached → 0. A worker with nothing → 1.
//! - **load_fraction**: running/max. A worker at 50% capacity → 0.5.
//!
//! W_locality=0.7, W_load=0.3 by default. Locality weighted higher:
//! fetching a GB of inputs takes MINUTES; dispatching to a busy worker
//! costs a queue slot. The fetch cost usually dominates.
//!
//! # Closure approximation
//!
//! "Which inputs does this derivation need?" → children's
//! `expected_output_paths`. Not perfect (doesn't include input SOURCES,
//! just input DERIVATION outputs) but covers the bulk of transfer cost
//! for typical builds.

use std::collections::HashMap;

use crate::dag::DerivationDag;
use crate::state::{DerivationState, WorkerId, WorkerState};

/// Weight for transfer-cost term. Higher = locality matters more.
const W_LOCALITY: f64 = 0.7;
/// Weight for load term. Higher = spreading load matters more.
const W_LOAD: f64 = 0.3;

/// Select the best worker for a derivation.
///
/// Hard filter first (has_capacity, can_build, size_class match), then
/// score the survivors, return the lowest. `None` if nobody passes the
/// filter — caller defers the derivation.
///
/// `target_class` is D7's size-class filter. `None` = no class filtering
/// (backward compat, or size-classes not configured).
pub fn best_worker(
    workers: &HashMap<WorkerId, WorkerState>,
    drv: &DerivationState,
    dag: &DerivationDag,
    target_class: Option<&str>,
) -> Option<WorkerId> {
    // --- Hard filter ---
    let candidates: Vec<&WorkerState> = workers
        .values()
        .filter(|w| {
            w.has_capacity()
                && w.can_build(&drv.system, &drv.required_features)
                // Size-class filter: if a target is specified, worker
                // must match. If no target (None), all workers pass.
                // Worker with size_class=None also passes — backward
                // compat with pre-D7 workers.
                && match (target_class, w.size_class.as_deref()) {
                    (None, _) => true,         // no filter
                    (Some(_), None) => true,   // worker unclassified = wildcard
                    (Some(t), Some(wc)) => t == wc,
                }
        })
        .collect();

    if candidates.is_empty() {
        return None;
    }

    // Short-circuit: one candidate → no scoring needed. Common in
    // small deployments or when only one worker has the right features.
    if candidates.len() == 1 {
        return Some(candidates[0].worker_id.clone());
    }

    // --- Closure approximation: children's expected output paths ---
    // These are the paths this derivation NEEDS as inputs. A worker
    // that has them cached skips the fetch.
    //
    // Collected into a Vec (not iterating lazily) because we scan it
    // once per candidate. For N candidates × M paths, that's N*M bloom
    // queries. M is typically <100; N is typically <10. Cheap.
    let input_paths: Vec<String> = dag
        .get_children(&drv.drv_hash)
        .into_iter()
        .filter_map(|child| dag.node(&child))
        .flat_map(|child| child.expected_output_paths.iter().cloned())
        .collect();

    // --- Score each candidate ---
    // Normalize transfer_cost: divide by max across candidates. This
    // makes the term relative — a worker missing 5 paths when everyone
    // misses 5 gets cost=1.0 (no locality advantage for anyone); a
    // worker missing 0 when others miss 5 gets cost=0.0.
    let missing_counts: Vec<usize> = candidates
        .iter()
        .map(|w| count_missing(w, &input_paths))
        .collect();
    // max_missing could be 0 (all workers have everything, or no input
    // paths). Division by zero → transfer_cost = 0 for everyone, which
    // is correct (locality doesn't discriminate).
    let max_missing = missing_counts.iter().copied().max().unwrap_or(0).max(1);

    let (best_idx, _best_score) = candidates
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let transfer_cost = missing_counts[i] as f64 / max_missing as f64;
            let load_fraction = if w.max_builds > 0 {
                w.running_builds.len() as f64 / w.max_builds as f64
            } else {
                // max_builds=0 shouldn't happen (has_capacity would
                // fail) but be defensive.
                1.0
            };
            let score = transfer_cost * W_LOCALITY + load_fraction * W_LOAD;
            (i, score)
        })
        // min_by for lowest score. Ties break by iteration order
        // (HashMap order, which is random). That's fine — a tie means
        // the workers are equally good; random is fair.
        //
        // partial_cmp on f64: our scores are in [0,1], never NaN
        // (all inputs are finite, no division by NaN). unwrap is safe.
        .min_by(|(_, a), (_, b)| a.partial_cmp(b).expect("scores are never NaN"))
        .expect("candidates non-empty (checked above)");

    Some(candidates[best_idx].worker_id.clone())
}

/// Count how many input paths the worker's bloom filter says are MISSING.
///
/// No bloom = worst case (assume missing everything). Better to
/// over-estimate transfer cost than under-estimate — over means we
/// might pick a less-optimal worker, under means we pick one that'll
/// spend time fetching we didn't account for.
fn count_missing(worker: &WorkerState, input_paths: &[String]) -> usize {
    let Some(bloom) = &worker.bloom else {
        // No filter = everything missing. This is the pessimistic
        // assumption — a worker that doesn't report its cache gets
        // no locality bonus. Incentivizes workers to send the filter.
        return input_paths.len();
    };

    input_paths
        .iter()
        .filter(|p| !bloom.maybe_contains(p))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_common::bloom::BloomFilter;
    use std::collections::HashSet;
    use std::time::Instant;

    fn make_worker(id: &str, max: u32, running: u32) -> WorkerState {
        let mut w = WorkerState {
            worker_id: id.into(),
            system: Some("x86_64-linux".into()),
            supported_features: vec![],
            max_builds: max,
            running_builds: (0..running).map(|i| format!("run-{i}").into()).collect(),
            stream_tx: None,
            last_heartbeat: Instant::now(),
            missed_heartbeats: 0,
            bloom: None,
            size_class: None,
        };
        // has_capacity needs stream_tx. Fake it.
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        w.stream_tx = Some(tx);
        w
    }

    fn make_drv() -> DerivationState {
        let proto = rio_proto::types::DerivationNode {
            drv_path: rio_test_support::fixtures::test_drv_path("test-drv"),
            drv_hash: "test-drv".into(),
            pname: "test".into(),
            system: "x86_64-linux".into(),
            required_features: vec![],
            output_names: vec!["out".into()],
            is_fixed_output: false,
            expected_output_paths: vec![],
        };
        DerivationState::try_from_node(&proto).unwrap()
    }

    fn workers_map(ws: Vec<WorkerState>) -> HashMap<WorkerId, WorkerState> {
        ws.into_iter().map(|w| (w.worker_id.clone(), w)).collect()
    }

    #[test]
    fn no_candidates_returns_none() {
        let workers = workers_map(vec![make_worker("full", 2, 2)]); // at capacity
        let dag = DerivationDag::new();
        assert_eq!(best_worker(&workers, &make_drv(), &dag, None), None);
    }

    #[test]
    fn single_candidate_short_circuits() {
        let workers = workers_map(vec![make_worker("only", 4, 1)]);
        let dag = DerivationDag::new();
        let result = best_worker(&workers, &make_drv(), &dag, None);
        assert_eq!(result.as_deref(), Some("only"));
    }

    #[test]
    fn prefers_lower_load() {
        // Two workers, no bloom (locality = 0 for both since no input
        // paths either). Load is the only discriminator.
        let workers = workers_map(vec![
            make_worker("busy", 4, 3), // load = 0.75
            make_worker("idle", 4, 0), // load = 0.0
        ]);
        let dag = DerivationDag::new();
        let result = best_worker(&workers, &make_drv(), &dag, None);
        assert_eq!(result.as_deref(), Some("idle"));
    }

    #[test]
    fn prefers_worker_with_inputs_cached() {
        // Two equally-loaded workers. One has the inputs cached (bloom
        // says yes), the other doesn't. Locality should dominate.
        let mut has_inputs = make_worker("has-inputs", 4, 1);
        let mut bloom = BloomFilter::new(100, 0.01);
        bloom.insert("/nix/store/input-a");
        bloom.insert("/nix/store/input-b");
        has_inputs.bloom = Some(bloom);

        let no_inputs = make_worker("no-inputs", 4, 1); // bloom = None

        let workers = workers_map(vec![has_inputs, no_inputs]);

        // Build a DAG: drv depends on child, child has the input paths.
        let mut dag = DerivationDag::new();
        let child_proto = rio_proto::types::DerivationNode {
            drv_path: rio_test_support::fixtures::test_drv_path("child"),
            drv_hash: "child".into(),
            pname: "child".into(),
            system: "x86_64-linux".into(),
            required_features: vec![],
            output_names: vec!["out".into()],
            is_fixed_output: false,
            expected_output_paths: vec!["/nix/store/input-a".into(), "/nix/store/input-b".into()],
        };
        let drv_proto = rio_proto::types::DerivationNode {
            drv_path: rio_test_support::fixtures::test_drv_path("test-drv"),
            drv_hash: "test-drv".into(),
            pname: "test".into(),
            system: "x86_64-linux".into(),
            required_features: vec![],
            output_names: vec!["out".into()],
            is_fixed_output: false,
            expected_output_paths: vec![],
        };
        dag.merge(
            uuid::Uuid::new_v4(),
            &[drv_proto.clone(), child_proto],
            &[rio_proto::types::DerivationEdge {
                parent_drv_path: rio_test_support::fixtures::test_drv_path("test-drv"),
                child_drv_path: rio_test_support::fixtures::test_drv_path("child"),
            }],
        )
        .unwrap();

        let drv = DerivationState::try_from_node(&drv_proto).unwrap();
        let result = best_worker(&workers, &drv, &dag, None);

        // has-inputs: missing=0, load=0.25. score = 0*0.7 + 0.25*0.3 = 0.075
        // no-inputs: missing=2 (no bloom → all missing), normalized=1.0.
        //            score = 1.0*0.7 + 0.25*0.3 = 0.775
        assert_eq!(result.as_deref(), Some("has-inputs"));
    }

    #[test]
    fn locality_can_override_load() {
        // Worker A: has inputs, busier. Worker B: no inputs, idle.
        // W_locality > W_load → A should still win for reasonable loads.
        let mut a = make_worker("a-has-inputs", 4, 2); // load = 0.5
        let mut bloom = BloomFilter::new(100, 0.01);
        bloom.insert("/nix/store/big-input");
        a.bloom = Some(bloom);

        let b = make_worker("b-idle", 4, 0); // load = 0.0, bloom = None

        let workers = workers_map(vec![a, b]);

        // DAG with one input path that only A has.
        let mut dag = DerivationDag::new();
        let child_proto = rio_proto::types::DerivationNode {
            drv_path: rio_test_support::fixtures::test_drv_path("child"),
            drv_hash: "child".into(),
            pname: "child".into(),
            system: "x86_64-linux".into(),
            required_features: vec![],
            output_names: vec!["out".into()],
            is_fixed_output: false,
            expected_output_paths: vec!["/nix/store/big-input".into()],
        };
        let drv_proto = rio_proto::types::DerivationNode {
            drv_path: rio_test_support::fixtures::test_drv_path("test-drv"),
            drv_hash: "test-drv".into(),
            ..make_drv_proto()
        };
        dag.merge(
            uuid::Uuid::new_v4(),
            &[drv_proto.clone(), child_proto],
            &[rio_proto::types::DerivationEdge {
                parent_drv_path: rio_test_support::fixtures::test_drv_path("test-drv"),
                child_drv_path: rio_test_support::fixtures::test_drv_path("child"),
            }],
        )
        .unwrap();

        let drv = DerivationState::try_from_node(&drv_proto).unwrap();
        let result = best_worker(&workers, &drv, &dag, None);

        // A: cost=0, load=0.5. score = 0*0.7 + 0.5*0.3 = 0.15
        // B: cost=1, load=0.0. score = 1*0.7 + 0.0*0.3 = 0.70
        // A wins despite being busier — locality matters more.
        assert_eq!(result.as_deref(), Some("a-has-inputs"));
    }

    #[test]
    fn size_class_filter() {
        let mut small = make_worker("small", 4, 0);
        small.size_class = Some("small".into());
        let mut large = make_worker("large", 4, 0);
        large.size_class = Some("large".into());

        let workers = workers_map(vec![small, large]);
        let dag = DerivationDag::new();

        // Target=large → only large passes.
        let result = best_worker(&workers, &make_drv(), &dag, Some("large"));
        assert_eq!(result.as_deref(), Some("large"));

        // Target=None → both pass (filter disabled).
        let result = best_worker(&workers, &make_drv(), &dag, None);
        assert!(result.is_some()); // either one
    }

    #[test]
    fn unclassified_worker_is_wildcard() {
        // Worker with no size_class should pass ANY target filter.
        // Backward compat: pre-D7 workers don't declare a class.
        let unclassified = make_worker("old-worker", 4, 0); // size_class=None
        let workers = workers_map(vec![unclassified]);
        let dag = DerivationDag::new();

        let result = best_worker(&workers, &make_drv(), &dag, Some("large"));
        assert_eq!(result.as_deref(), Some("old-worker"));
    }

    #[test]
    fn count_missing_no_bloom_pessimistic() {
        let worker = make_worker("w", 4, 0); // bloom = None
        let inputs = vec!["/a".into(), "/b".into(), "/c".into()];
        assert_eq!(count_missing(&worker, &inputs), 3); // all missing
    }

    fn make_drv_proto() -> rio_proto::types::DerivationNode {
        rio_proto::types::DerivationNode {
            drv_path: rio_test_support::fixtures::test_drv_path("test-drv"),
            drv_hash: "test-drv".into(),
            pname: "test".into(),
            system: "x86_64-linux".into(),
            required_features: vec![],
            output_names: vec!["out".into()],
            is_fixed_output: false,
            expected_output_paths: vec![],
        }
    }

    // Ensure the import of HashSet is used (WorkerState construction uses it indirectly via make_worker).
    const _: fn() = || {
        let _: HashSet<()> = HashSet::new();
    };
}
