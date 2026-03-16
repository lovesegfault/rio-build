//! Worker scoring and selection.
//!
//! Replaces `dispatch.rs`'s first-eligible-worker FIFO with a scored
//! selection: pick the worker that will complete this derivation
//! FASTEST, accounting for transfer cost (inputs not yet cached) and
// r[impl sched.classify.smallest-covering]
// r[impl sched.classify.mem-bump]
//! current load.
//!
//! # Scoring (scheduler.md:54-61)
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

use serde::{Deserialize, Serialize};

use crate::dag::DerivationDag;
use crate::state::{DerivationState, DrvHash, WorkerId, WorkerState};

/// One size-class in the operator's cutoff config.
///
/// Classes form an ordered sequence: "small" < "medium" < "large" by
/// `cutoff_secs`. A derivation is routed to the SMALLEST class whose
/// cutoff covers its estimated duration — this concentrates quick
/// builds on cheap workers and reserves big iron for slow builds.
///
/// `mem_limit_bytes` is a guard: if a derivation's known peak memory
/// exceeds its duration-class's limit, it gets bumped to the next class
/// regardless of duration. A 10-second build that OOMs on a 4GB worker
/// isn't actually a small build.
///
/// Lives here (not main.rs) because the actor needs it: dispatch.rs
/// calls `classify()` with a ref to the config vec, and completion.rs
/// looks up `cutoff_for()` for misclassification detection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SizeClassConfig {
    pub name: String,
    /// Max estimated duration to route here. A build estimated at 25s
    /// goes to the smallest class with cutoff ≥ 25.
    pub cutoff_secs: f64,
    /// If a build's ema_peak_memory exceeds this, bump to the next
    /// class even if duration fits. `u64::MAX` = no memory check.
    pub mem_limit_bytes: u64,
}

/// Classify a derivation into a size-class.
///
/// `None` = no classification (size-classes not configured, optional feature).
/// `Some(name)` = route to workers whose `size_class` matches.
///
/// Algorithm:
/// 1. Empty config → None (no filtering; all workers are candidates)
/// 2. Sort classes by cutoff (idempotent if already sorted)
/// 3. Find smallest class where `est_dur ≤ cutoff`
/// 4. If that class's mem_limit is exceeded → bump to next larger
/// 5. If est_dur exceeds ALL cutoffs → largest class (builds gotta go
///    somewhere; the largest class is "everything else")
///
/// Taking `&[SizeClassConfig]` owned-sort internally rather than
/// requiring the caller to pre-sort: classify() is called per-dispatch
/// and the sort is ~3-element. Cheaper than maintaining an invariant.
pub fn classify(
    est_dur: f64,
    peak_mem: Option<f64>,
    classes: &[SizeClassConfig],
) -> Option<String> {
    if classes.is_empty() {
        // If no size_classes configured, skip classification entirely —
        // optional feature, not required for single-pool deployments.
        return None;
    }

    // Sort by cutoff. Small-N (typically 2-4 classes); the clone is
    // ~100 bytes and the sort is trivial. Not worth caching sorted
    // order on the actor — this runs once per dispatch decision, not
    // per-packet.
    //
    // total_cmp (not partial_cmp) — main.rs validates cutoff_secs is
    // finite at startup, but total_cmp is defense-in-depth: it defines
    // NaN as "largest" under IEEE 754 total order, so a hypothetical
    // NaN cutoff would sort to the end rather than panicking the
    // scheduler on every dispatch.
    let mut sorted: Vec<&SizeClassConfig> = classes.iter().collect();
    sorted.sort_by(|a, b| a.cutoff_secs.total_cmp(&b.cutoff_secs));

    // Find the smallest class whose cutoff covers est_dur, then check
    // memory. If memory forces a bump, we want the NEXT class — hence
    // iterating by index so we can peek ahead.
    for (i, class) in sorted.iter().enumerate() {
        if est_dur > class.cutoff_secs {
            // Duration doesn't fit; try next class.
            continue;
        }
        // Duration fits. Check memory.
        if let Some(mem) = peak_mem
            && mem > class.mem_limit_bytes as f64
        {
            // Memory bump: return the NEXT class if there is one.
            // If this is already the largest, return it anyway — a
            // build has to go somewhere, and "largest" is the best
            // we've got. The misclassification detector will catch
            // the resulting OOM and log it.
            return Some(
                sorted
                    .get(i + 1)
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| class.name.clone()),
            );
        }
        return Some(class.name.clone());
    }

    // est_dur exceeded every cutoff. Route to the largest class —
    // that's where slow builds belong. Returning None here would mean
    // "no worker can take this," which is wrong: a 2-hour build should
    // go to the big workers, not get stuck.
    sorted.last().map(|c| c.name.clone())
}

/// Look up the cutoff for a class name. For misclassification detection:
/// "did actual duration exceed 2× the cutoff we routed this to?"
///
/// `None` if the name isn't in the config (shouldn't happen — we only
/// store names that came from `classify()`, but be defensive against
/// config changes mid-run).
pub fn cutoff_for(class_name: &str, classes: &[SizeClassConfig]) -> Option<f64> {
    classes
        .iter()
        .find(|c| c.name == class_name)
        .map(|c| c.cutoff_secs)
}

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
/// `target_class` is the size-class filter. `None` = size-classes not
/// configured on this scheduler (optional feature).
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
                // Exclude workers that previously failed this
                // derivation. failed_workers is populated by
                // handle_transient_failure (explicit report) AND
                // reassign_derivations (worker disconnected mid-
                // build — that's also a failed attempt on THAT
                // worker's infrastructure). Without this, a
                // transient fail would re-dispatch to the SAME
                // broken worker (2-worker cluster with one bad
                // worker → oscillate forever until poison threshold).
                && !drv.failed_workers.contains(&w.worker_id)
                // Size-class filter. If the scheduler is classifying
                // (target is Some), workers MUST declare a class.
                // An unclassified worker when the scheduler has
                // size_classes configured is a misconfiguration —
                // rejecting it makes the problem visible instead
                // of silently routing 10-hour builds to a spot
                // instance that declares nothing.
                && match (target_class, w.size_class.as_deref()) {
                    (None, _) => true,          // scheduler not classifying
                    (Some(_), None) => false,   // misconfigured worker: reject
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
    let input_paths = approx_input_closure(dag, &drv.drv_hash);

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
/// Approximate input closure: the derivation's DAG children's
/// expected output paths.
///
/// This is what the derivation NEEDS as inputs — its dependencies'
/// outputs. Not perfect (misses `input_srcs` and transitive closure),
/// but covers the bulk of what the worker's FUSE will actually fetch.
///
/// Used by:
/// - [`best_worker`] for bloom-locality scoring (workers with these
///   paths cached are preferred)
/// - dispatch.rs for [`PrefetchHint`] (tell the chosen worker to
///   warm these before the build starts)
///
/// Both callers want the SAME approximation — if the scoring says
/// "w1 has most of these cached," the prefetch hint for w1 should
/// be "the few it DOESN'T have" (bloom-filtered). Inconsistent
/// approximations would mean scoring on one set, hinting from
/// another. Extracting this fn guarantees they agree.
///
/// Cheap: DAG iteration only, no store RPCs, no ATerm parse. The
/// scheduler has all this state in memory already (populated at
/// merge time). For a derivation with 20 dependencies each with
/// 2 outputs: 40 string clones, ~1μs.
///
/// [`PrefetchHint`]: rio_proto::types::PrefetchHint
pub(crate) fn approx_input_closure(dag: &DerivationDag, drv_hash: &DrvHash) -> Vec<String> {
    dag.get_children(drv_hash)
        .into_iter()
        .filter_map(|child| dag.node(&child))
        .flat_map(|child| child.expected_output_paths.iter().cloned())
        .collect()
}

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
    use rio_test_support::fixtures::{make_derivation_node, make_edge};
    use std::time::Instant;

    fn make_worker(id: &str, max: u32, running: u32) -> WorkerState {
        let mut w = WorkerState {
            worker_id: id.into(),
            systems: vec!["x86_64-linux".into()],
            supported_features: vec![],
            max_builds: max,
            running_builds: (0..running).map(|i| format!("run-{i}").into()).collect(),
            stream_tx: None,
            last_heartbeat: Instant::now(),
            missed_heartbeats: 0,
            bloom: None,
            size_class: None,
            draining: false,
            connected_since: Instant::now(),
            last_resources: None,
        };
        // has_capacity needs stream_tx. Fake it.
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        w.stream_tx = Some(tx);
        w
    }

    fn make_drv() -> DerivationState {
        DerivationState::try_from_node(&make_derivation_node("test-drv", "x86_64-linux")).unwrap()
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
    fn failed_worker_excluded() {
        // Two workers, both idle + capable. drv has worker-a in
        // failed_workers → best_worker MUST pick worker-b. Without
        // the exclusion, a 2-worker cluster with one broken worker
        // would oscillate: fail → reassign to same worker → fail
        // → ... until poison threshold. With exclusion: second
        // attempt goes to worker-b, succeeds.
        let workers = workers_map(vec![
            make_worker("worker-a", 4, 0),
            make_worker("worker-b", 4, 0),
        ]);
        let dag = DerivationDag::new();
        let mut drv = make_drv();
        drv.failed_workers.insert("worker-a".into());

        let chosen = best_worker(&workers, &drv, &dag, None);
        assert_eq!(
            chosen,
            Some("worker-b".into()),
            "worker-a excluded via failed_workers → worker-b is the only candidate"
        );

        // ALL workers failed → None (caller defers). This is where
        // poison detection kicks in (failed_workers.len() >= 3 →
        // handle_transient_failure poisons).
        drv.failed_workers.insert("worker-b".into());
        assert_eq!(
            best_worker(&workers, &drv, &dag, None),
            None,
            "all workers in failed_workers → nobody eligible"
        );
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
            expected_output_paths: vec!["/nix/store/input-a".into(), "/nix/store/input-b".into()],
            ..make_derivation_node("child", "x86_64-linux")
        };
        let drv_proto = make_derivation_node("test-drv", "x86_64-linux");
        dag.merge(
            uuid::Uuid::new_v4(),
            &[drv_proto.clone(), child_proto],
            &[make_edge("test-drv", "child")],
            "",
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
            expected_output_paths: vec!["/nix/store/big-input".into()],
            ..make_derivation_node("child", "x86_64-linux")
        };
        let drv_proto = make_derivation_node("test-drv", "x86_64-linux");
        dag.merge(
            uuid::Uuid::new_v4(),
            &[drv_proto.clone(), child_proto],
            &[make_edge("test-drv", "child")],
            "",
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
    fn unclassified_worker_rejected_when_scheduler_classified() {
        // If the scheduler has size_classes configured (target is Some),
        // a worker that doesn't declare a class is a misconfiguration.
        // Rejecting it surfaces the problem instead of silently
        // wildcarding — the old (Some(_), None) => true behavior could
        // route a 10-hour "large" build to a spot instance that just
        // never set RIO_SIZE_CLASS.
        let unclassified = make_worker("misconfigured", 4, 0); // size_class=None
        let workers = workers_map(vec![unclassified]);
        let dag = DerivationDag::new();

        let result = best_worker(&workers, &make_drv(), &dag, Some("large"));
        assert_eq!(
            result, None,
            "unclassified worker must be rejected when scheduler is classifying"
        );

        // Sanity: same worker IS accepted when scheduler not classifying.
        let result = best_worker(&workers, &make_drv(), &dag, None);
        assert_eq!(result.as_deref(), Some("misconfigured"));
    }

    #[test]
    fn count_missing_no_bloom_pessimistic() {
        let worker = make_worker("w", 4, 0); // bloom = None
        let inputs = vec!["/a".into(), "/b".into(), "/c".into()];
        assert_eq!(count_missing(&worker, &inputs), 3); // all missing
    }

    // ----- classify() tests -----

    fn classes() -> Vec<SizeClassConfig> {
        vec![
            SizeClassConfig {
                name: "small".into(),
                cutoff_secs: 30.0,
                mem_limit_bytes: 1 << 30, // 1 GiB
            },
            SizeClassConfig {
                name: "large".into(),
                cutoff_secs: 3600.0,
                mem_limit_bytes: 16 << 30, // 16 GiB
            },
        ]
    }

    #[test]
    fn classify_empty_config_returns_none() {
        // Empty config = optional feature off, no filtering.
        assert_eq!(classify(100.0, None, &[]), None);
        assert_eq!(classify(0.1, Some(1e12), &[]), None);
    }

    #[test]
    fn classify_by_duration() {
        let c = classes();
        // 10s → small (smallest class covering 10s)
        assert_eq!(classify(10.0, None, &c).as_deref(), Some("small"));
        // 30s exactly → small (≤ is inclusive)
        assert_eq!(classify(30.0, None, &c).as_deref(), Some("small"));
        // 31s → large
        assert_eq!(classify(31.0, None, &c).as_deref(), Some("large"));
        // 3600s exactly → large
        assert_eq!(classify(3600.0, None, &c).as_deref(), Some("large"));
    }

    // r[verify sched.classify.mem-bump]
    // 10s would classify "small" by duration alone, but 2 GiB > the
    // 1 GiB mem_limit on "small" → bumps to "large". Proves the
    // mem_limit_bytes gate at classify():115-119 fires.
    #[test]
    fn classify_memory_bump() {
        let c = classes();
        // 10s would be small, but 2 GiB > 1 GiB limit → bump to large.
        assert_eq!(
            classify(10.0, Some(2.0 * (1 << 30) as f64), &c).as_deref(),
            Some("large")
        );
        // 10s + 500 MiB → small (under limit).
        assert_eq!(
            classify(10.0, Some(500.0 * (1 << 20) as f64), &c).as_deref(),
            Some("small")
        );
        // None peak_mem → no bump (no data, don't guess).
        assert_eq!(classify(10.0, None, &c).as_deref(), Some("small"));
    }

    #[test]
    fn classify_memory_bump_at_largest_stays_largest() {
        let c = classes();
        // 100s → large. 32 GiB > 16 GiB limit, but there's no larger
        // class → stays large. Build has to go SOMEWHERE.
        assert_eq!(
            classify(100.0, Some(32.0 * (1 << 30) as f64), &c).as_deref(),
            Some("large")
        );
    }

    #[test]
    fn classify_overflow_duration_picks_largest() {
        let c = classes();
        // 10 hours > every cutoff. Goes to largest, not None.
        // Returning None would strand slow builds forever.
        assert_eq!(classify(36000.0, None, &c).as_deref(), Some("large"));
    }

    #[test]
    fn classify_nan_cutoff_does_not_panic() {
        // main.rs rejects NaN cutoffs at startup with an operator-facing
        // error. But classify() is still defense-in-depth: total_cmp
        // puts NaN at the end of the sort (IEEE 754 total order), so a
        // NaN cutoff acts like "infinite cutoff" — it becomes the
        // overflow bucket. The important property: no panic.
        let c = vec![
            SizeClassConfig {
                name: "normal".into(),
                cutoff_secs: 30.0,
                mem_limit_bytes: u64::MAX,
            },
            SizeClassConfig {
                name: "broken".into(),
                cutoff_secs: f64::NAN,
                mem_limit_bytes: u64::MAX,
            },
        ];
        // 10s fits in "normal" → that wins (NaN sorted last).
        assert_eq!(classify(10.0, None, &c).as_deref(), Some("normal"));
        // 100s overflows "normal". The NaN class is "largest" by
        // total_cmp, so it's the overflow target. `100.0 > NaN` is
        // false so the duration-fits check at line ~107 passes.
        assert_eq!(classify(100.0, None, &c).as_deref(), Some("broken"));
    }

    #[test]
    fn classify_unsorted_config() {
        // Operator might list large before small in toml. Sort
        // internally so order doesn't matter.
        let c = vec![
            SizeClassConfig {
                name: "large".into(),
                cutoff_secs: 3600.0,
                mem_limit_bytes: u64::MAX,
            },
            SizeClassConfig {
                name: "small".into(),
                cutoff_secs: 30.0,
                mem_limit_bytes: u64::MAX,
            },
        ];
        // 10s must go to small (SMALLEST covering class), not large
        // (first in config). If we didn't sort, this would pick large.
        assert_eq!(classify(10.0, None, &c).as_deref(), Some("small"));
    }

    #[test]
    fn cutoff_for_lookup() {
        let c = classes();
        assert_eq!(cutoff_for("small", &c), Some(30.0));
        assert_eq!(cutoff_for("large", &c), Some(3600.0));
        assert_eq!(cutoff_for("nonexistent", &c), None);
    }
}
