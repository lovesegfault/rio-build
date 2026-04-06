//! Worker scoring and selection.
//!
//! Replaces `dispatch.rs`'s first-eligible-worker FIFO with a scored
//! selection: pick the worker that will complete this derivation
//! FASTEST, accounting for transfer cost (inputs not yet cached) and
// r[impl sched.classify.smallest-covering]
// r[impl sched.classify.mem-bump]
//! input locality.
//!
//! # Scoring (scheduler.md:54-61)
//!
//! `score = transfer_cost` — lowest wins.
//!
//! - **transfer_cost**: normalized count of input paths the worker
//!   DOESN'T have cached (via bloom filter). A worker with everything
//!   cached → 0. A worker with nothing → 1.
//!
//! P0537: with one build per pod, `has_capacity()` already filters busy
//! workers, so every candidate has zero running. The old `load_fraction`
//! term (running/max, weighted W_LOAD=0.3 vs W_LOCALITY=0.7) is moot —
//! locality alone discriminates among idle candidates.
//!
//! # Closure approximation
//!
//! "Which inputs does this derivation need?" → children's
//! `expected_output_paths`. Not perfect (doesn't include input SOURCES,
//! just input DERIVATION outputs) but covers the bulk of transfer cost
//! for typical builds.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use rio_proto::types::ExecutorKind;

use crate::dag::DerivationDag;
use crate::state::{DerivationState, DrvHash, ExecutorId, ExecutorState};

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
    /// If a build's ema_peak_cpu_cores exceeds this, bump to the next
    /// class even if duration fits (mirrors mem-bump). `None` = no CPU
    /// check. Optional so existing TOML without `cpu_limit_cores` keeps
    /// working. When Some, main.rs validate_config enforces is_finite
    /// && >0 at startup (same shape as cutoff_secs / P0415 backoff_*
    /// bounds; bughunt-mc238 found this gap — `c > NaN` at :128 would
    /// silently disable the bump, `c > neg` would always-bump). P0424.
    #[serde(default)]
    pub cpu_limit_cores: Option<f64>,
}

/// One fetcher size-class in the operator's config. Unlike
/// [`SizeClassConfig`] there is NO `cutoff_secs` / `mem_limit_bytes`:
/// FODs have no a-priori signal (excluded from `build_samples`,
/// ADR-019), so routing is reactive only — a FOD that fails on
/// `classes[i]` retries on `classes[i+1]`. The scheduler needs only
/// the ORDER (smallest→largest) to compute "next larger"; per-class
/// resources live on the controller side (`FetcherPool.spec.classes`).
///
/// `[[fetcher_size_classes]]` array-of-tables in scheduler.toml,
/// parallel to `[[size_classes]]`. Empty = feature off (single
/// fetcher pool, no class filter — original behavior).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetcherSizeClassConfig {
    pub name: String,
}

/// Next-larger fetcher class after `current`, or `None` if `current`
/// is already the largest (or unknown). Clamps at the top: a FOD
/// that OOMs on the largest class stays there until poison.
///
/// Config order is authoritative (smallest→largest); this does NOT
/// sort. Unlike builder classes there's no `cutoff_secs` to sort by
/// — the operator declares the order.
pub fn next_fetcher_class(current: &str, classes: &[FetcherSizeClassConfig]) -> Option<String> {
    let idx = classes.iter().position(|c| c.name == current)?;
    classes.get(idx + 1).map(|c| c.name.clone())
}

/// Next-larger builder class after `current`, or `None` if `current`
/// is already the largest (or unknown, or `classes` empty). Unlike
/// [`next_fetcher_class`], builder classes are ordered by
/// `cutoff_secs` (config order is NOT authoritative — `classify()`
/// sorts internally), so "next larger" = smallest cutoff strictly
/// greater than `current`'s. I-177: reactive promotion for non-FOD
/// OOMs (bootstrap-glibc on a tiny builder → floor=small).
pub fn next_builder_class(current: &str, classes: &[SizeClassConfig]) -> Option<String> {
    let current_cutoff = cutoff_for(current, classes)?;
    classes
        .iter()
        .filter(|c| c.cutoff_secs > current_cutoff)
        .min_by(|a, b| a.cutoff_secs.total_cmp(&b.cutoff_secs))
        .map(|c| c.name.clone())
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
    peak_cpu: Option<f64>,
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
    // memory + CPU. If either forces a bump, we want the NEXT class —
    // hence iterating by index so we can peek ahead.
    for (i, class) in sorted.iter().enumerate() {
        if est_dur > class.cutoff_secs {
            // Duration doesn't fit; try next class.
            continue;
        }
        // Duration fits. Check memory.
        let mem_over = peak_mem.is_some_and(|m| m > class.mem_limit_bytes as f64);
        // r[impl sched.classify.cpu-bump]
        // CPU-bump: if ema_peak_cpu_cores exceeds the duration-chosen
        // class's cpu_limit, bump to the next larger class. Mirrors
        // mem-bump. `None` limit = no CPU check (default).
        let cpu_over = peak_cpu
            .zip(class.cpu_limit_cores)
            .is_some_and(|(c, limit)| c > limit);
        if mem_over || cpu_over {
            // Resource bump: return the NEXT class if there is one.
            // If this is already the largest, return it anyway — a
            // build has to go somewhere, and "largest" is the best
            // we've got. The misclassification detector will catch
            // the resulting OOM/CPU-starve and log it.
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

/// First clause of [`hard_filter`] that rejects, or `None` if it
/// accepts. Used by `InspectBuildDag` to answer "why is this Ready drv
/// not dispatching?" without a trace build (I-062: 4 stuck-FOD
/// recurrences with every visible field clean — the rejection was in
/// something the diagnostic API didn't expose).
///
/// MUST mirror [`hard_filter`]'s clauses exactly; `hard_filter` is
/// implemented as `rejection_reason(..).is_none()` so the two cannot
/// drift. The reason strings are stable identifiers (CLI/dashboard
/// match on them), not prose — keep them terse.
///
/// `target_class` is the size-class filter as in `hard_filter`. The
/// diagnostic call site passes `None` (it doesn't know the class); the
/// size-class clause therefore won't show up in diagnostics — that's
/// acceptable since size-class mismatches are already visible via
/// `DebugExecutorState.size_class` vs the operator's class config.
pub fn rejection_reason(
    w: &ExecutorState,
    drv: &DerivationState,
    target_class: Option<&str>,
) -> Option<&'static str> {
    if drv.is_fixed_output != (w.kind == ExecutorKind::Fetcher) {
        return Some("kind-mismatch");
    }
    // has_capacity()'s constituents, individually so the reason names
    // the SPECIFIC gate. Order matches has_capacity()'s short-circuit.
    if w.is_draining() {
        return Some("draining");
    }
    if w.store_degraded {
        return Some("store-degraded");
    }
    if !w.is_registered() {
        // stream_tx.is_none() OR systems.is_empty(). The diagnostic
        // already exposes has_stream and systems separately; one
        // reason here is fine since either being false means "not
        // ready to dispatch to".
        return Some("not-registered");
    }
    // I-095: stream_tx is Some but the receiver was dropped (gRPC
    // stream gone). ExecutorDisconnected is queued in the mailbox
    // but dispatch_ready may run first — without this gate, every
    // ready drv in the pass picks this executor, try_send fails
    // "channel closed", drv resets to Ready, next drv picks it
    // again. Observed: 100+ WARN/tick, actor saturated, RPCs time
    // out. Distinct from not-registered so the gauge accounting in
    // handle_executor_disconnected (which keys on is_registered())
    // stays correct.
    if w.stream_tx.as_ref().is_some_and(|tx| tx.is_closed()) {
        return Some("stream-closed");
    }
    if w.running_build.is_some() {
        return Some("at-capacity");
    }
    // can_build()'s two checks, separated.
    if !w.systems.iter().any(|s| s == &drv.system) {
        return Some("system-mismatch");
    }
    if !drv
        .required_features
        .iter()
        .all(|f| w.supported_features.contains(f))
    {
        return Some("feature-missing");
    }
    if drv.failed_builders.contains(&w.executor_id) {
        return Some("failed-on");
    }
    match (target_class, w.size_class.as_deref()) {
        (None, _) => {}
        (Some(_), None) => return Some("size-class-undeclared"),
        (Some(t), Some(wc)) if t != wc => return Some("size-class-mismatch"),
        (Some(_), Some(_)) => {}
    }
    if let Some(est) = drv.est_memory_bytes
        && let Some(r) = &w.last_resources
        && r.memory_total_bytes != 0
        && r.memory_total_bytes < est
    {
        return Some("resource-fit");
    }
    None
}

/// Hard filter: executor-kind, capacity, system/feature match,
/// not-previously-failed, and size-class. The SAME predicate that both
/// warm-pass and cold-fallback use in [`best_executor`] — extracted so
/// the two passes can't drift. Implemented via [`rejection_reason`] so
/// the diagnostic and the filter cannot diverge.
fn hard_filter(w: &ExecutorState, drv: &DerivationState, target_class: Option<&str>) -> bool {
    // r[impl sched.dispatch.fod-to-fetcher]
    // r[impl sched.assign.resource-fit]
    // Clause-by-clause logic lives in `rejection_reason` (above) so the
    // dispatch path and the diagnostic cannot drift. The kind gate is
    // first (cheap boolean, airgap boundary: a builder NEVER sees a
    // FOD, a fetcher NEVER sees arbitrary code). Size-class: workers
    // MUST declare a class when the scheduler is classifying, or
    // they're rejected (visible misconfig, not silent routing).
    // Resource-fit: three unknown-ceiling cases (est=None, no
    // heartbeat resources, memory_total_bytes==0 = "unknown" per
    // builder cgroup.rs:667) all pass.
    rejection_reason(w, drv, target_class).is_none()
}

/// Select the best worker for a derivation.
///
/// Hard filter first (has_capacity, can_build, size_class match), then
/// score the survivors, return the lowest. `None` if nobody passes the
/// filter — caller defers the derivation.
///
/// `target_class` is the size-class filter. `None` = size-classes not
/// configured on this scheduler (optional feature).
pub fn best_executor(
    workers: &HashMap<ExecutorId, ExecutorState>,
    drv: &DerivationState,
    dag: &DerivationDag,
    target_class: Option<&str>,
) -> Option<ExecutorId> {
    // r[impl sched.assign.warm-gate]
    // --- Hard filter: warm-gate two-pass ---
    // First pass: warm workers only. This is the normal path once a
    // worker has ACKed its initial PrefetchHint (PrefetchComplete).
    let warm_candidates: Vec<&ExecutorState> = workers
        .values()
        .filter(|w| w.warm && hard_filter(w, drv, target_class))
        .collect();

    // Fallback: no warm worker passes → relax the gate. Single-
    // worker clusters, mass scale-up where ALL workers are fresh —
    // can't deadlock. The gate is an optimization, not a correctness
    // constraint. Cold workers still go through the SAME hard_filter
    // (capacity/features/class) — we're only relaxing `warm`.
    let candidates: Vec<&ExecutorState> = if !warm_candidates.is_empty() {
        warm_candidates
    } else {
        let cold: Vec<&ExecutorState> = workers
            .values()
            .filter(|w| hard_filter(w, drv, target_class))
            .collect();
        if !cold.is_empty() {
            tracing::debug!(
                cold_count = cold.len(),
                drv_system = %drv.system,
                "warm-gate fallback: no warm worker passes filter, dispatching cold"
            );
            metrics::counter!("rio_scheduler_warm_gate_fallback_total").increment(1);
        }
        cold
    };

    if candidates.is_empty() {
        return None;
    }

    // Short-circuit: one candidate → no scoring needed. Common in
    // small deployments or when only one worker has the right features.
    if candidates.len() == 1 {
        return Some(candidates[0].executor_id.clone());
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
    let missing_counts: Vec<usize> = candidates
        .iter()
        .map(|w| count_missing(w, &input_paths))
        .collect();

    // P0537: locality is the sole discriminator (see module doc). Lowest
    // missing-count wins. min_by_key on the integer count — no f64
    // partial_cmp needed. Ties break by iteration order (HashMap order,
    // random) — a tie means the workers are equally good; random is fair.
    let best_idx = (0..candidates.len())
        .min_by_key(|&i| missing_counts[i])
        .expect("candidates non-empty (checked above)");

    Some(candidates[best_idx].executor_id.clone())
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
/// - [`best_executor`] for bloom-locality scoring (workers with these
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
        })
        // Filter empties: a floating-CA child that hasn't completed yet
        // has expected_output_paths=[""] and output_paths=[]. The ""
        // would be a no-op bloom lookup and a no-op PrefetchHint entry,
        // but cleaner to drop it here.
        .filter(|p| !p.is_empty())
        .cloned()
        .collect()
}

/// spend time fetching we didn't account for.
fn count_missing(worker: &ExecutorState, input_paths: &[String]) -> usize {
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

    fn make_worker(id: &str, _max: u32, running: u32) -> ExecutorState {
        let mut w = ExecutorState::new(id.into());
        w.systems = vec!["x86_64-linux".into()];
        // P0537 stage 1: max param ignored (always-1). Kept so ~40
        // callers don't churn here. With stage 4's Option semantics,
        // running>0 → Some (busy), running==0 → None (idle).
        w.running_build = (running > 0).then(|| "run-0".into());
        // Tests default to warm — warm-gate coverage lives in the
        // dedicated tests below that flip `warm=false` explicitly.
        w.warm = true;
        // has_capacity needs an OPEN stream_tx (I-095: is_closed()
        // is now part of the gate). forget(rx) keeps the channel
        // open for the test's lifetime — a few dozen tiny receivers
        // leaked per test process; nextest runs each test in its own
        // process so they don't accumulate.
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        std::mem::forget(rx);
        w.stream_tx = Some(tx);
        w
    }

    /// Like `make_worker` but with a CLOSED stream_tx — the receiver
    /// is dropped, so `tx.is_closed()` is true. Models the window
    /// between gRPC stream drop and ExecutorDisconnected processing.
    fn make_worker_closed_stream(id: &str) -> ExecutorState {
        let mut w = ExecutorState::new(id.into());
        w.systems = vec!["x86_64-linux".into()];
        w.warm = true;
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(rx);
        w.stream_tx = Some(tx);
        w
    }

    fn make_drv() -> DerivationState {
        DerivationState::try_from_node(&make_derivation_node("test-drv", "x86_64-linux")).unwrap()
    }

    fn workers_map(ws: Vec<ExecutorState>) -> HashMap<ExecutorId, ExecutorState> {
        ws.into_iter().map(|w| (w.executor_id.clone(), w)).collect()
    }

    // r[verify sched.dispatch.fod-to-fetcher]
    // r[verify sched.dispatch.no-fod-fallback]
    /// 4-cell matrix: `is_fixed_output × executor.kind`. The XOR check
    /// in `hard_filter` must reject both cross-kind routings. The
    /// `true×Builder` rejection also covers `no-fod-fallback`: even an
    /// idle builder fails the filter, so there's no FOD→builder path
    /// under any load condition.
    #[test]
    fn hard_filter_kind_matrix() {
        let mk_worker = |kind| {
            let mut w = make_worker("e", 4, 0);
            w.kind = kind;
            w
        };
        let mk_drv = |is_fod| {
            let mut d = make_drv();
            d.is_fixed_output = is_fod;
            d
        };

        // FOD → Fetcher: passes (rest of filter permitting)
        assert!(
            hard_filter(&mk_worker(ExecutorKind::Fetcher), &mk_drv(true), None),
            "FOD must route to fetcher"
        );
        // FOD → Builder: rejected (airgap boundary)
        assert!(
            !hard_filter(&mk_worker(ExecutorKind::Builder), &mk_drv(true), None),
            "FOD must NOT route to builder (airgap)"
        );
        // non-FOD → Fetcher: rejected (arbitrary code on open egress)
        assert!(
            !hard_filter(&mk_worker(ExecutorKind::Fetcher), &mk_drv(false), None),
            "non-FOD must NOT route to fetcher"
        );
        // non-FOD → Builder: passes (rest of filter permitting)
        assert!(
            hard_filter(&mk_worker(ExecutorKind::Builder), &mk_drv(false), None),
            "non-FOD must route to builder"
        );
    }

    // r[verify sched.dispatch.fod-builtin-any-arch]
    /// `system="builtin"` FOD matches a fetcher of EITHER arch
    /// (every executor appends `"builtin"` to its advertised
    /// `systems` at startup). An arch-specific FOD (`system=
    /// "x86_64-linux"`, e.g. `pkgs.fetchgit` inheriting stdenv's
    /// system) matches only fetchers advertising that arch.
    #[test]
    fn hard_filter_builtin_matches_any_fetcher() {
        let mk_fetcher = |systems: &[&str]| {
            let mut w = make_worker("f", 4, 0);
            w.kind = ExecutorKind::Fetcher;
            w.systems = systems.iter().map(|s| s.to_string()).collect();
            w
        };
        let mk_fod = |system: &str| {
            let mut d =
                DerivationState::try_from_node(&make_derivation_node("fod", system)).unwrap();
            d.is_fixed_output = true;
            d
        };

        let arm = mk_fetcher(&["aarch64-linux", "builtin"]);
        let x86 = mk_fetcher(&["x86_64-linux", "builtin"]);

        // builtin FOD: both arches eligible (overflow to either).
        assert!(hard_filter(&arm, &mk_fod("builtin"), None));
        assert!(hard_filter(&x86, &mk_fod("builtin"), None));
        // x86_64-linux FOD: only x86 fetcher; arm rejects.
        assert!(hard_filter(&x86, &mk_fod("x86_64-linux"), None));
        assert!(!hard_filter(&arm, &mk_fod("x86_64-linux"), None));
        assert_eq!(
            rejection_reason(&arm, &mk_fod("x86_64-linux"), None),
            Some("system-mismatch")
        );
    }

    /// Kind check runs BEFORE capacity — a full builder still rejects
    /// FODs for the right reason (wrong kind), not by accident (no
    /// capacity). Pins the ordering so a refactor that puts kind LAST
    /// doesn't silently accept FODs on an idle misconfigured builder.
    #[test]
    fn hard_filter_kind_check_precedes_capacity() {
        let mut fetcher_full = make_worker("f", 1, 1); // at capacity
        fetcher_full.kind = ExecutorKind::Fetcher;
        let mut fod = make_drv();
        fod.is_fixed_output = true;
        // Kind matches → falls through to capacity check → fails there.
        assert!(!hard_filter(&fetcher_full, &fod, None));

        let mut builder_idle = make_worker("b", 4, 0); // idle
        builder_idle.kind = ExecutorKind::Builder;
        // Kind mismatch → rejected regardless of idle capacity.
        assert!(!hard_filter(&builder_idle, &fod, None));
    }

    /// `rejection_reason` names each clause; `hard_filter` is its
    /// `is_none()`. Exhaustive over the reasons the diagnostic surfaces
    /// (size-class is exercised separately by the classify tests).
    /// ExecutorState isn't Clone (stream_tx), so each case rebuilds.
    #[test]
    fn rejection_reason_per_clause() {
        let drv = make_drv();

        // Baseline: idle builder, non-FOD drv, system match → ACCEPT.
        let w = make_worker("w", 1, 0);
        assert_eq!(rejection_reason(&w, &drv, None), None);
        assert!(hard_filter(&w, &drv, None));

        // kind-mismatch
        let mut fetcher = make_worker("w", 1, 0);
        fetcher.kind = ExecutorKind::Fetcher;
        assert_eq!(
            rejection_reason(&fetcher, &drv, None),
            Some("kind-mismatch")
        );

        // draining (admin) — checked before capacity, so a busy
        // draining worker reports "draining", not "at-capacity".
        let mut draining = make_worker("w", 1, 1);
        draining.draining = true;
        assert_eq!(rejection_reason(&draining, &drv, None), Some("draining"));

        // draining_hb (worker SIGTERM via heartbeat) — same gate via
        // is_draining()'s OR.
        let mut draining_hb = make_worker("w", 1, 0);
        draining_hb.draining_hb = true;
        assert_eq!(rejection_reason(&draining_hb, &drv, None), Some("draining"));

        // store-degraded
        let mut degraded = make_worker("w", 1, 0);
        degraded.store_degraded = true;
        assert_eq!(
            rejection_reason(&degraded, &drv, None),
            Some("store-degraded")
        );

        // not-registered (no stream)
        let mut no_stream = make_worker("w", 1, 0);
        no_stream.stream_tx = None;
        assert_eq!(
            rejection_reason(&no_stream, &drv, None),
            Some("not-registered")
        );

        // stream-closed (I-095): stream_tx Some but receiver dropped.
        // Distinct from not-registered — is_registered() stays true
        // (gauge accounting), but dispatch must skip.
        let closed = make_worker_closed_stream("w");
        assert!(closed.is_registered(), "is_registered ignores is_closed()");
        assert!(!closed.has_capacity(), "has_capacity gates on is_closed()");
        assert_eq!(rejection_reason(&closed, &drv, None), Some("stream-closed"));
        assert!(!hard_filter(&closed, &drv, None));

        // at-capacity
        let busy = make_worker("w", 1, 1);
        assert_eq!(rejection_reason(&busy, &drv, None), Some("at-capacity"));

        // system-mismatch
        let mut aarch_drv = make_drv();
        "aarch64-linux".clone_into(&mut aarch_drv.system);
        assert_eq!(
            rejection_reason(&w, &aarch_drv, None),
            Some("system-mismatch")
        );

        // feature-missing
        let mut feat_drv = make_drv();
        feat_drv.required_features = vec!["kvm".into()];
        assert_eq!(
            rejection_reason(&w, &feat_drv, None),
            Some("feature-missing")
        );

        // failed-on
        let mut failed_drv = make_drv();
        failed_drv.failed_builders.insert("w".into());
        assert_eq!(rejection_reason(&w, &failed_drv, None), Some("failed-on"));

        // resource-fit
        let mut tight = make_worker("w", 1, 0);
        tight.last_resources = Some(rio_proto::types::ResourceUsage {
            memory_total_bytes: 4 << 30,
            ..Default::default()
        });
        let mut big_drv = make_drv();
        big_drv.est_memory_bytes = Some(8 << 30);
        assert_eq!(
            rejection_reason(&tight, &big_drv, None),
            Some("resource-fit")
        );

        // hard_filter == rejection_reason.is_none() for every case
        // above. Spot-check one of each polarity.
        assert!(!hard_filter(&busy, &drv, None));
        assert!(hard_filter(&tight, &drv, None)); // est=None → fits
    }

    #[test]
    fn no_candidates_returns_none() {
        let workers = workers_map(vec![make_worker("full", 2, 2)]); // at capacity
        let dag = DerivationDag::new();
        assert_eq!(best_executor(&workers, &make_drv(), &dag, None), None);
    }

    #[test]
    fn failed_worker_excluded() {
        // Two workers, both idle + capable. drv has worker-a in
        // failed_builders → best_executor MUST pick worker-b. Without
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
        drv.failed_builders.insert("worker-a".into());

        let chosen = best_executor(&workers, &drv, &dag, None);
        assert_eq!(
            chosen,
            Some("worker-b".into()),
            "worker-a excluded via failed_builders → worker-b is the only candidate"
        );

        // ALL workers failed → None (caller defers). This is where
        // poison detection kicks in (failed_builders.len() >= 3 →
        // handle_transient_failure poisons).
        drv.failed_builders.insert("worker-b".into());
        assert_eq!(
            best_executor(&workers, &drv, &dag, None),
            None,
            "all workers in failed_builders → nobody eligible"
        );
    }

    /// When `worker_count < poison_threshold` and ALL workers have failed
    /// this derivation, `best_executor` returns None (everyone fails the
    /// `failed_builders` exclusion in `hard_filter`). This is CORRECT at
    /// the assignment layer — `best_executor` has no business poisoning.
    ///
    /// The starvation this used to cause (derivation stuck in Ready
    /// forever, never poisoned because `len=2 < threshold=3`, never
    /// dispatched because all workers excluded) is FIXED at two layers
    /// (I-065): `handle_transient_failure` poisons immediately on the
    /// failure-report path; `dispatch_ready` poisons as a backstop for
    /// paths that bypass it (worker disconnect, recovery reconcile).
    /// Both call `failed_builders_exhausts_fleet`, which is kind-aware
    /// — the original check (`self.executors.keys().all(...)`) counted
    /// fetchers against a builder drv and never tripped on mixed
    /// clusters. See actor/{dispatch.rs,completion.rs}.
    #[test]
    fn all_workers_failed_below_threshold_poisons_upstream() {
        // 2-worker cluster — below the default poison_threshold of 3.
        let workers = workers_map(vec![
            make_worker("worker-a", 4, 0),
            make_worker("worker-b", 4, 0),
        ]);
        let dag = DerivationDag::new();
        let mut drv = make_drv();

        // Both workers failed. failed_builders.len() == 2 < 3 →
        // PoisonConfig::is_poisoned alone would NOT poison, but
        // handle_transient_failure's worker_count clamp now does.
        drv.failed_builders.insert("worker-a".into());
        drv.failed_builders.insert("worker-b".into());

        // best_executor still returns None (both excluded via
        // failed_builders) — correct. The caller
        // (handle_transient_failure) poisons BEFORE this state is
        // reachable in the dispatch loop, so the None here is never
        // observed in production.
        assert_eq!(
            best_executor(&workers, &drv, &dag, None),
            None,
            "all workers in failed_builders → best_executor correctly returns \
             None; handle_transient_failure poisons upstream via \
             worker_count clamp so this Ready-but-undispatchable state \
             is never reached"
        );

        // Sanity: adding a third worker makes dispatch possible again
        // (relevant for the case where a fresh worker connects BEFORE
        // the 2nd failure lands — no poison, dispatch resumes).
        let mut workers3 = workers;
        let c = make_worker("worker-c", 4, 0);
        workers3.insert(c.executor_id.clone(), c);
        assert_eq!(
            best_executor(&workers3, &drv, &dag, None),
            Some("worker-c".into()),
            "fresh worker not in failed_builders → dispatch resumes"
        );
    }

    #[test]
    fn single_candidate_short_circuits() {
        let workers = workers_map(vec![make_worker("only", 4, 0)]);
        let dag = DerivationDag::new();
        let result = best_executor(&workers, &make_drv(), &dag, None);
        assert_eq!(result.as_deref(), Some("only"));
    }

    #[test]
    fn prefers_worker_with_inputs_cached() {
        // Two idle workers. One has the inputs cached (bloom says
        // yes), the other doesn't. Locality is the sole discriminator
        // (P0537: load-fraction is gone — candidates are always idle).
        let mut has_inputs = make_worker("has-inputs", 4, 0);
        let mut bloom = BloomFilter::new(100, 0.01);
        bloom.insert("/nix/store/input-a");
        bloom.insert("/nix/store/input-b");
        has_inputs.bloom = Some(bloom);

        let no_inputs = make_worker("no-inputs", 4, 0); // bloom = None

        let workers = workers_map(vec![has_inputs, no_inputs]);

        // Build a DAG: drv depends on child, child has the input paths.
        let mut dag = DerivationDag::new();
        let child_proto = rio_proto::dag::DerivationNode {
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
        let result = best_executor(&workers, &drv, &dag, None);

        // has-inputs: missing=0; no-inputs: missing=2 (no bloom → all
        // missing). min_by_key picks has-inputs.
        assert_eq!(result.as_deref(), Some("has-inputs"));
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
        let result = best_executor(&workers, &make_drv(), &dag, Some("large"));
        assert_eq!(result.as_deref(), Some("large"));

        // Target=None → both pass (filter disabled).
        let result = best_executor(&workers, &make_drv(), &dag, None);
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

        let result = best_executor(&workers, &make_drv(), &dag, Some("large"));
        assert_eq!(
            result, None,
            "unclassified worker must be rejected when scheduler is classifying"
        );

        // Sanity: same worker IS accepted when scheduler not classifying.
        let result = best_executor(&workers, &make_drv(), &dag, None);
        assert_eq!(result.as_deref(), Some("misconfigured"));
    }

    // r[verify sched.assign.resource-fit]
    // ADR-020 §5: worker.memory_total_bytes >= drv.est_memory_bytes
    // as a hard filter. Uses `hard_filter` directly (not
    // `best_executor`) to isolate the resource-fit arm from warm-
    // gate/scoring — the filter's pass/fail is what's under test.
    #[test]
    fn resource_fit_filter() {
        const GIB: u64 = 1 << 30;

        // Plan T3 case matrix. Worker memory via last_resources;
        // drv estimate via est_memory_bytes.
        let mk_worker = |mem_total: u64| {
            let mut w = make_worker("w", 4, 0);
            w.last_resources = Some(rio_proto::types::ResourceUsage {
                memory_total_bytes: mem_total,
                ..Default::default()
            });
            w
        };
        let mk_drv = |est: Option<u64>| {
            let mut d = make_drv();
            d.est_memory_bytes = est;
            d
        };

        // Worker 8Gi, drv est 6Gi → fits (6 ≤ 8).
        assert!(
            hard_filter(&mk_worker(8 * GIB), &mk_drv(Some(6 * GIB)), None),
            "8Gi worker must fit 6Gi drv"
        );

        // Worker 8Gi, drv est 12Gi → doesn't fit.
        assert!(
            !hard_filter(&mk_worker(8 * GIB), &mk_drv(Some(12 * GIB)), None),
            "8Gi worker must REJECT 12Gi drv"
        );

        // Worker 64Gi, drv est 6Gi → fits (overflow routing: a small
        // drv CAN go to a big pod if that's what's idle).
        assert!(
            hard_filter(&mk_worker(64 * GIB), &mk_drv(Some(6 * GIB)), None),
            "64Gi worker must fit 6Gi drv (overflow routing)"
        );

        // Worker 0 (cgroup memory.max=max → unlimited), drv 128Gi
        // → fits. rio-builder cgroup.rs:667 unwrap_or(0) → 0 means
        // "unknown ceiling", never "zero memory".
        assert!(
            hard_filter(&mk_worker(0), &mk_drv(Some(128 * GIB)), None),
            "memory_total_bytes=0 (unlimited cgroup) must fit any drv"
        );

        // Drv est=None (cold start: no history) → any worker fits,
        // even an 8Gi one. The manifest omits cold-start drvs too
        // (controller uses operator floor) — consistent with "no
        // estimate means no constraint".
        assert!(
            hard_filter(&mk_worker(8 * GIB), &mk_drv(None), None),
            "est=None (cold start) must fit any worker"
        );
    }

    // r[verify sched.assign.resource-fit]
    // Worker with last_resources=None (no heartbeat resources yet)
    // → unknown ceiling → fits. Separate from the 0-ceiling case:
    // this is "heartbeat didn't carry resources" (proto field
    // absent), not "heartbeat said unlimited". Both mean
    // always-fits, but via different match arms.
    #[test]
    fn resource_fit_no_heartbeat_resources_fits() {
        let w = make_worker("fresh", 4, 0); // last_resources defaults to None
        assert!(w.last_resources.is_none(), "precondition: no resources");
        let mut d = make_drv();
        d.est_memory_bytes = Some(128 << 30); // 128Gi

        assert!(
            hard_filter(&w, &d, None),
            "worker with no heartbeat resources must be treated as unknown-fits"
        );
    }

    // r[verify sched.assign.resource-fit]
    // Resource-fit is AND-ed with size-class, not replacing it. A
    // worker that passes size-class but fails resource-fit → reject.
    // A worker that passes resource-fit but fails size-class → reject.
    // Proves the filter slots in the same position as has_capacity
    // (hard filter preceding scoring) without disrupting Static mode.
    #[test]
    fn resource_fit_composes_with_size_class() {
        const GIB: u64 = 1 << 30;

        let mut small_8gi = make_worker("small-8gi", 4, 0);
        small_8gi.size_class = Some("small".into());
        small_8gi.last_resources = Some(rio_proto::types::ResourceUsage {
            memory_total_bytes: 8 * GIB,
            ..Default::default()
        });

        let mut drv_12gi = make_drv();
        drv_12gi.est_memory_bytes = Some(12 * GIB);

        // target=small matches size_class, but 8Gi < 12Gi → reject.
        // Proves resource-fit runs AFTER size-class passes.
        assert!(
            !hard_filter(&small_8gi, &drv_12gi, Some("small")),
            "size-class match must not bypass resource-fit"
        );

        // target=large fails size_class (worker is "small"), even
        // though 8Gi > 6Gi would pass resource-fit → reject.
        // Proves size-class still works (exit criterion).
        let mut drv_6gi = make_drv();
        drv_6gi.est_memory_bytes = Some(6 * GIB);
        assert!(
            !hard_filter(&small_8gi, &drv_6gi, Some("large")),
            "resource-fit match must not bypass size-class"
        );

        // Both pass → accept.
        assert!(
            hard_filter(&small_8gi, &drv_6gi, Some("small")),
            "both filters passing must accept"
        );
    }

    #[test]
    fn count_missing_no_bloom_pessimistic() {
        let worker = make_worker("w", 4, 0); // bloom = None
        let inputs = vec!["/a".into(), "/b".into(), "/c".into()];
        assert_eq!(count_missing(&worker, &inputs), 3); // all missing
    }

    // r[verify sched.assign.warm-gate]
    // Warm-gate: warm worker wins over cold even when cold is
    // otherwise better. With zero warm workers, falls back to cold
    // and increments the fallback metric.
    #[test]
    fn warm_gate_prefers_warm_worker() {
        // P0537: both idle (single-build). Warm-gate must pick warm
        // even when cold is otherwise indistinguishable.
        let mut warm = make_worker("warm", 4, 0);
        warm.warm = true;
        let mut cold = make_worker("cold", 4, 0);
        cold.warm = false;

        let workers = workers_map(vec![warm, cold]);
        let dag = DerivationDag::new();

        // Warm-gate first-pass: only "warm" passes. Cold is
        // filtered out. "warm" is the ONLY candidate → picked.
        let result = best_executor(&workers, &make_drv(), &dag, None);
        assert_eq!(
            result.as_deref(),
            Some("warm"),
            "warm-gate first-pass must pick warm over cold"
        );
    }

    // r[verify sched.assign.warm-gate]
    // r[verify obs.metric.scheduler]
    #[test]
    fn warm_gate_fallback_when_no_warm_workers() {
        use rio_test_support::metrics::CountingRecorder;
        let recorder = CountingRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);

        // All-cold cluster (fresh scale-up or single-worker). The gate
        // MUST fall back — the scheduler can't deadlock.
        let mut a = make_worker("a", 4, 1);
        a.warm = false;
        let mut b = make_worker("b", 4, 0);
        b.warm = false;

        let workers = workers_map(vec![a, b]);
        let dag = DerivationDag::new();

        let before = recorder.get("rio_scheduler_warm_gate_fallback_total{}");

        // With zero warm candidates, fallback uses cold workers with
        // the SAME hard_filter (capacity/features) then scores.
        // "b" has lower load (0.0 vs 0.25) → wins.
        let result = best_executor(&workers, &make_drv(), &dag, None);
        assert_eq!(
            result.as_deref(),
            Some("b"),
            "fallback must dispatch cold and still prefer lower-load"
        );
        // P0299 EC: fallback path increments fallback_total. Without
        // this assert, a refactor that removed the metric (or moved
        // the increment to a branch that doesn't fire) would be
        // silent — the PICK assert above doesn't prove the counter.
        assert_eq!(
            recorder.get("rio_scheduler_warm_gate_fallback_total{}") - before,
            1,
            "fallback path must increment rio_scheduler_warm_gate_fallback_total exactly once"
        );
    }

    #[test]
    fn warm_gate_is_per_worker_not_global() {
        // Warm-gate per spec: a second worker registering while the
        // first is still warming does not delay builds the second
        // (already warm) worker can take. Prove: cold+warm both
        // eligible → warm wins, cold is NOT blocking dispatch.
        let mut cold = make_worker("cold-still-warming", 4, 0);
        cold.warm = false;
        let warm = make_worker("warm-ready", 4, 0);

        let workers = workers_map(vec![cold, warm]);
        let dag = DerivationDag::new();

        let result = best_executor(&workers, &make_drv(), &dag, None);
        assert_eq!(
            result.as_deref(),
            Some("warm-ready"),
            "a cold worker's presence must not block dispatch to a warm one"
        );
    }

    // ----- classify() tests -----

    /// Index of a class name in the cutoff-sorted order. For asserting
    /// "bump never goes down" properties — string names don't compare
    /// directly, but sorted indices do.
    fn class_index(name: Option<&str>, classes: &[SizeClassConfig]) -> usize {
        let Some(name) = name else {
            return 0;
        };
        let mut sorted: Vec<&SizeClassConfig> = classes.iter().collect();
        sorted.sort_by(|a, b| a.cutoff_secs.total_cmp(&b.cutoff_secs));
        sorted.iter().position(|c| c.name == name).unwrap_or(0)
    }

    fn mk_class(
        name: &str,
        cutoff: f64,
        mem_limit: u64,
        cpu_limit: Option<f64>,
    ) -> SizeClassConfig {
        SizeClassConfig {
            name: name.into(),
            cutoff_secs: cutoff,
            mem_limit_bytes: mem_limit,
            cpu_limit_cores: cpu_limit,
        }
    }

    fn classes() -> Vec<SizeClassConfig> {
        vec![
            mk_class("small", 30.0, 1 << 30, Some(2.0)),
            mk_class("large", 3600.0, 16 << 30, Some(16.0)),
        ]
    }

    #[test]
    fn classify_empty_config_returns_none() {
        // Empty config = optional feature off, no filtering.
        assert_eq!(classify(100.0, None, None, &[]), None);
        assert_eq!(classify(0.1, Some(1e12), None, &[]), None);
    }

    #[test]
    fn classify_by_duration() {
        let c = classes();
        // 10s → small (smallest class covering 10s)
        assert_eq!(classify(10.0, None, None, &c).as_deref(), Some("small"));
        // 30s exactly → small (≤ is inclusive)
        assert_eq!(classify(30.0, None, None, &c).as_deref(), Some("small"));
        // 31s → large
        assert_eq!(classify(31.0, None, None, &c).as_deref(), Some("large"));
        // 3600s exactly → large
        assert_eq!(classify(3600.0, None, None, &c).as_deref(), Some("large"));
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
            classify(10.0, Some(2.0 * (1 << 30) as f64), None, &c).as_deref(),
            Some("large")
        );
        // 10s + 500 MiB → small (under limit).
        assert_eq!(
            classify(10.0, Some(500.0 * (1 << 20) as f64), None, &c).as_deref(),
            Some("small")
        );
        // None peak_mem → no bump (no data, don't guess).
        assert_eq!(classify(10.0, None, None, &c).as_deref(), Some("small"));
    }

    #[test]
    fn classify_memory_bump_at_largest_stays_largest() {
        let c = classes();
        // 100s → large. 32 GiB > 16 GiB limit, but there's no larger
        // class → stays large. Build has to go SOMEWHERE.
        assert_eq!(
            classify(100.0, Some(32.0 * (1 << 30) as f64), None, &c).as_deref(),
            Some("large")
        );
    }

    // r[verify sched.classify.cpu-bump]
    #[test]
    fn classify_cpu_bump() {
        let c = classes();
        // 10s would be small by duration, but 4.0 cores > 2.0 limit →
        // bump to large. Mirrors mem-bump.
        assert_eq!(
            classify(10.0, None, Some(4.0), &c).as_deref(),
            Some("large")
        );
        // 10s + 1.5 cores → small (under 2.0 limit).
        assert_eq!(
            classify(10.0, None, Some(1.5), &c).as_deref(),
            Some("small")
        );
        // None peak_cpu → no bump (no data, don't guess).
        assert_eq!(classify(10.0, None, None, &c).as_deref(), Some("small"));
        // cpu_limit_cores=None on class → no cpu check for that class.
        let no_cpu_limit = vec![mk_class("small", 30.0, u64::MAX, None)];
        assert_eq!(
            classify(10.0, None, Some(64.0), &no_cpu_limit).as_deref(),
            Some("small"),
            "class without cpu_limit_cores doesn't bump on CPU"
        );
    }

    #[test]
    fn classify_cpu_bump_at_largest_stays_largest() {
        let c = classes();
        // 100s → large. 32 cores > 16 limit, but no larger class.
        assert_eq!(
            classify(100.0, None, Some(32.0), &c).as_deref(),
            Some("large")
        );
    }

    /// CPU-bump is monotone: adding CPU pressure never moves a
    /// derivation to a SMALLER class. Property-style sweep across
    /// (dur, mem, cpu) — if this fails, the bump logic can route
    /// a high-CPU build to a smaller worker, which would starve it.
    #[test]
    fn cpu_bump_monotone() {
        let c = classes();
        for dur in [0.1, 10.0, 25.0, 30.0, 31.0, 100.0, 3600.0, 36000.0] {
            for mem in [None, Some(5e8), Some(2e9), Some(2e10)] {
                for cpu in [0.5, 1.5, 2.0, 4.0, 8.0, 16.0, 32.0] {
                    let without_cpu = classify(dur, mem, None, &c);
                    let with_cpu = classify(dur, mem, Some(cpu), &c);
                    let idx_without = class_index(without_cpu.as_deref(), &c);
                    let idx_with = class_index(with_cpu.as_deref(), &c);
                    assert!(
                        idx_with >= idx_without,
                        "cpu-bump went DOWN: dur={dur} mem={mem:?} cpu={cpu} → \
                         without={without_cpu:?}(idx {idx_without}) \
                         with={with_cpu:?}(idx {idx_with})"
                    );
                }
            }
        }
    }

    #[test]
    fn classify_overflow_duration_picks_largest() {
        let c = classes();
        // 10 hours > every cutoff. Goes to largest, not None.
        // Returning None would strand slow builds forever.
        assert_eq!(classify(36000.0, None, None, &c).as_deref(), Some("large"));
    }

    #[test]
    fn classify_nan_cutoff_does_not_panic() {
        // main.rs rejects NaN cutoffs at startup with an operator-facing
        // error. But classify() is still defense-in-depth: total_cmp
        // puts NaN at the end of the sort (IEEE 754 total order), so a
        // NaN cutoff acts like "infinite cutoff" — it becomes the
        // overflow bucket. The important property: no panic.
        let c = vec![
            mk_class("normal", 30.0, u64::MAX, None),
            mk_class("broken", f64::NAN, u64::MAX, None),
        ];
        // 10s fits in "normal" → that wins (NaN sorted last).
        assert_eq!(classify(10.0, None, None, &c).as_deref(), Some("normal"));
        // 100s overflows "normal". The NaN class is "largest" by
        // total_cmp, so it's the overflow target. `100.0 > NaN` is
        // false so the duration-fits check at line ~107 passes.
        assert_eq!(classify(100.0, None, None, &c).as_deref(), Some("broken"));
    }

    #[test]
    fn classify_unsorted_config() {
        // Operator might list large before small in toml. Sort
        // internally so order doesn't matter.
        let c = vec![
            mk_class("large", 3600.0, u64::MAX, None),
            mk_class("small", 30.0, u64::MAX, None),
        ];
        // 10s must go to small (SMALLEST covering class), not large
        // (first in config). If we didn't sort, this would pick large.
        assert_eq!(classify(10.0, None, None, &c).as_deref(), Some("small"));
    }

    /// I-095 regression: a closed-channel executor must NOT be picked.
    /// Before the fix, dispatch_ready would pick it for every ready
    /// drv in the pass (try_send fails → reset_to_ready → next drv
    /// picks it again), saturating the actor. With the fix, hard_filter
    /// rejects it so best_executor returns the live one (or None).
    #[test]
    fn best_executor_skips_closed_stream() {
        let drv = make_drv();
        let dag = DerivationDag::new();

        // Only candidate has a closed channel → no executor.
        let dead = make_worker_closed_stream("dead");
        let map = workers_map(vec![dead]);
        assert_eq!(best_executor(&map, &drv, &dag, None), None);

        // Closed + live → live is picked. Loop to guard against
        // accidental ordering-dependence (HashMap iteration).
        for _ in 0..10 {
            let dead = make_worker_closed_stream("dead");
            let live = make_worker("live", 1, 0);
            let map = workers_map(vec![dead, live]);
            assert_eq!(
                best_executor(&map, &drv, &dag, None).as_deref(),
                Some("live")
            );
        }
    }

    #[test]
    fn cutoff_for_lookup() {
        let c = classes();
        assert_eq!(cutoff_for("small", &c), Some(30.0));
        assert_eq!(cutoff_for("large", &c), Some(3600.0));
        assert_eq!(cutoff_for("nonexistent", &c), None);
    }
}
