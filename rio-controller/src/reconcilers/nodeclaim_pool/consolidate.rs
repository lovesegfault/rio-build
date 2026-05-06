//! Windowed-rate idle-node consolidation.
//!
//! Per `r[ctrl.nodeclaim.consolidate-na]`: an empty Registered NodeClaim
//! is kept while `λ(t)·𝔼[c_arr·𝟙{≤cores}] > cores/q_0.5(boot)`; λ is
//! the windowed empirical arrival rate over `[t, t+W)` on
//! right-censored `idle_gap` events (W = `q_0.5(boot)/2`). The first
//! `t` at which the inequality flips false is `consolidate_after(t)` —
//! floored at `q_0.5(boot)/2` so a transient lull can't collapse to
//! always-delete.

use std::collections::{HashMap, HashSet};

use kube::Api;
use kube::api::DeleteParams;
use rio_crds::karpenter::NodeClaim;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use super::NodeClaimPoolConfig;
use super::ffd::{LiveNode, Placement, cells_of, system_to_arch};
use super::sketch::{Cell, CellSketches};

/// Hold-open annotation key. Operator-settable: a NodeClaim carrying
/// `rio.build/hold-open=true` uses `max_consolidation_time` as its idle
/// threshold instead of the NA break-even. Set via `kubectl annotate
/// nodeclaim <n> rio.build/hold-open=true` for debugging or to keep one
/// warm slot through a known lull. The reconciler does NOT set it
/// automatically.
pub const HOLD_OPEN_ANNOTATION: &str = "rio.build/hold-open";

/// Ring-buffer cap for `CellState.idle_gap_events`. NA hazard reads the
/// most recent window; older events are stale (idle-gap distribution
/// drifts with workload). 256 ≈ ~2.5KiB jsonb per cell.
const IDLE_GAP_RING: usize = 256;

/// One observed gap between a node going idle and the next intent
/// arriving (or the node being reaped — `censored=true`). Persisted as
/// jsonb (`nodeclaim_cell_state.idle_gap_events`); shape changes need
/// no migration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdleGapEvent {
    /// Seconds the node was idle.
    pub gap_secs: f64,
    /// `true` if the node was reaped before the next arrival
    /// (right-censored observation).
    pub censored: bool,
}

/// First `t` at which `λ(t)·E[c_fit] ≤ cores/boot_median` — the
/// break-even where keeping the node idle costs more than the expected
/// boot-avoided. Floored at `boot_median/2` per
/// `r[ctrl.nodeclaim.consolidate-na]`; ceiling at `max` (default
/// `2×max(uncensored gap)`).
///
/// `λ(t)` is a windowed rate over `[t, t+w)` with `w = boot_median/2`
/// (≥5s): `uncensored_hits_in_window / (w · n_at_risk(t))`. A
/// finite-difference of `H(t)` over a fixed 1s grid degenerates to
/// zero whenever no event lands in `(t, t+1]`, so the scan would
/// return at the floor on any sparse/non-integer event distribution
/// regardless of arrival rate. The windowed form is non-zero as long
/// as any uncensored event falls within `boot_median/2` of `t`. Step
/// `w/2` keeps overlapping coverage.
// r[impl ctrl.nodeclaim.consolidate-na+4]
pub fn consolidate_after(
    events: &[IdleGapEvent],
    e_fitting_cores: f64,
    node_cores: u32,
    boot_median: f64,
    max: Option<f64>,
    min: Option<f64>,
) -> f64 {
    // r35 bug_050: per-class operator floor. §13e routed Fetcher Pools
    // through `nodeclaim_pool`, dropping Karpenter's 10m
    // `consolidateAfter` to the NA-model's `boot_median/2` (~15s for a
    // 30s-boot fetcher). The model floor caps undershoot from a
    // transient lull; `min` is the *policy* floor — what the operator
    // pays per idle-second is theirs to weigh against a re-boot. `max`
    // of the two: a dense-arrival fetcher cell still uses the higher
    // NA threshold; a sparse one gets the policy floor instead of
    // burn-a-boot-every-15s.
    let floor = (boot_median / 2.0).max(min.unwrap_or(0.0));
    // RHS: cost of NOT having the node = cores worth of capacity that
    // takes boot_median to recover. boot_median.max(1) avoids div-by-0
    // on a degenerate sketch.
    let rhs = f64::from(node_cores) / boot_median.max(1.0);
    let max_t = max.unwrap_or_else(|| {
        2.0 * events
            .iter()
            .filter(|e| !e.censored)
            .map(|e| e.gap_secs)
            .fold(floor, f64::max)
    });
    // r37 merged_010: `w` is the estimator's smoothing window — NOT the
    // policy floor. Coupling them broke the keep-condition: `λ ≤ 1/w`
    // and `E[c_fit] ≤ node_cores`, so `λ·E > cores/boot_median`
    // requires `w < boot_median`. The bug_050 (`fetcher-*`) and qa-fix-b
    // (`*`) policy floors BOTH exceed `boot_median`, making the NA model
    // structurally dead — `consolidate_after` returned `floor`
    // unconditionally. The spec (`r[ctrl.nodeclaim.consolidate-na+4]`)
    // says `W = q_0.5(boot)/2`; the implementation must match. Decoupling
    // restores the invariant and makes the per-class `min` what it was
    // designed to be: a floor the model can EXCEED under load.
    let w = (boot_median / 2.0).max(5.0);
    let lambda_at = |t: f64| {
        let n_at_risk = events.iter().filter(|e| e.gap_secs >= t).count().max(1) as f64;
        let hits = events
            .iter()
            .filter(|e| !e.censored && e.gap_secs >= t && e.gap_secs < t + w)
            .count() as f64;
        hits / (w * n_at_risk)
    };
    let mut t = floor;
    while t < max_t {
        if lambda_at(t) * e_fitting_cores <= rhs {
            return t.max(floor);
        }
        t += w / 2.0;
    }
    // r37 merged_010: when `max_consolidation_time < floor` the loop
    // body never runs (`t = floor ≥ max_t` on entry) and we'd return
    // `max_t < floor`, silently defeating the bug_050/qa-fix-b policy
    // floor (helm-settable via `scheduler.sla.maxConsolidationTime`).
    max_t.max(floor)
}

/// `𝔼[c_arrival · 𝟙{c_arrival ≤ node_cores}]` over this tick's
/// placeable intents — mean cores of intents that would fit on a
/// `node_cores` node. Spec: 0 when intents is ⊥ or empty (caller
/// passes `&[]` in consolidate-only mode).
pub fn e_fitting_cores(placeable: &[Placement], node_cores: u32) -> f64 {
    let fitting: Vec<u32> = placeable
        .iter()
        .map(|(i, _, _)| i.cores)
        .filter(|&c| c <= node_cores)
        .collect();
    if fitting.is_empty() {
        return 0.0;
    }
    fitting.iter().copied().map(f64::from).sum::<f64>() / fitting.len() as f64
}

/// Filter `placeable` to intents that can ARRIVE at a node in `cell`
/// — the per-cell partition `e_fitting_cores` computes its mean over.
///
/// r35 bug_023: `r[ctrl.nodeclaim.consolidate-na+4]` says "per-cell
/// mean over `intents`" — §13e removed the `kind` filter from
/// `GetSpawnIntents`, so a global mean over `placeable` blends fetcher
/// (`c*≈1`) and builder intents and biases `E[c_fit]` for builder
/// nodes ~5× low. The filter is the SAME predicate FFD's `simulate`
/// uses for the agnostic-fallback gate (NOT a `hw_class_names.is_
/// empty()` pass-through, which over-counts hw-agnostic featureless
/// intents on fetcher cells — the `rio.build/fetcher:NoSchedule` taint
/// repels them at runtime, so counting them inflates the cell's
/// hold-open). Returns owned `Vec<Placement>` (clone): for ≤200
/// intents per tick the alloc is noise next to the `Api::delete`.
///
/// r35 B1 ripple: `hw_admits` arch arg is `Option<&str>` (mirrors
/// FFD's `hw_admits` and `HwClassConfig::matches_arch`). The
/// agnostic-fallback gate is "at least one non-trivial constraint
/// axis" — `arch=None ∧ features=[]` is unroutable; `arch=None ∧
/// features=[fetcher]` is the `system="builtin"` FOD case (features
/// route, `matches_arch(_, None)` passes through). Pre-B1 this
/// `is_some_and(|a| ...)` short-circuit dropped builtin FODs, so a
/// fetcher cell whose only demand is builtin FODs read as zero
/// placeable → `reap_idle` reaped it.
pub fn placeable_for_cell(
    placeable: &[Placement],
    cell: &Cell,
    hw_admits: impl Fn(&str, Option<&str>, &[String]) -> bool,
) -> Vec<Placement> {
    placeable
        .iter()
        .filter(|(i, _, _)| {
            // r37 bug_002 (§Simulator-shares-accounting): the
            // (intent, cell) membership predicate is `cells_of(i)` —
            // both the hw_class half AND the capacity-type half.
            // `hw_class_names`-only matching counted spot-only intents
            // as demand for the on-demand cell (and vice versa),
            // inflating `e_fitting_cores`. `cells_of(i)` is what FFD's
            // `simulate` and cover's `assign_to_cells` use; routing
            // here through the same fn keeps the consolidator's view of
            // demand consistent with the placer's.
            cells_of(i).iter().any(|c| c == cell)
                || (i.hw_class_names.is_empty() && {
                    let a = system_to_arch(&i.system);
                    let f = i.required_features.as_slice();
                    (a.is_some() || !f.is_empty()) && hw_admits(&cell.0, a, f)
                })
        })
        .cloned()
        .collect()
}

/// Append `e` to `cell`'s ring-buffered `idle_gap_events`.
fn push_idle_gap(sketches: &mut CellSketches, cell: &Cell, e: IdleGapEvent) {
    let evs = &mut sketches.cell_mut(cell).idle_gap_events;
    if evs.len() >= IDLE_GAP_RING {
        evs.remove(0);
    }
    evs.push(e);
}

/// Per-cell consolidation context. r37 bug_002/006/009 close: every
/// per-cell quantity `reap_idle` reads is computed ONCE here from one
/// `cell` and one `placeable` slice. The hold-open arm, the non-hold-
/// open arm, and the gauge emission all read the same `CellCtx` —
/// they cannot diverge. Phase-0 of `reap_idle` builds one per
/// `cfg.all_cells()` (so every cell's gauge gets a 0-or-actual write,
/// per bug_006); a live node carrying an unconfigured cell (mid-rollout
/// drift) gets one derived on demand.
struct CellCtx {
    /// Idle-gap events for the cell. Cloned: the `&` borrow into
    /// `CellSketches` cannot span the mutating reap loop
    /// (`push_idle_gap` takes `&mut`). ≤`IDLE_GAP_RING` (256) events
    /// per cell — the alloc is noise next to the `Api::delete`.
    events: Vec<IdleGapEvent>,
    /// `placeable_for_cell(placeable, cell, hw_admits)` — the per-cell
    /// partition `e_fitting_cores` averages over.
    cell_placeable: Vec<Placement>,
    /// `q_0.5(boot[cell])`, or `cfg.seed_for(cell)` for a cold cell.
    boot_median: f64,
    /// Per-class operator floor (`minConsolidationTime`).
    min: Option<f64>,
}

impl CellCtx {
    fn new(
        cell: &Cell,
        placeable: &[Placement],
        sketches: &CellSketches,
        cfg: &NodeClaimPoolConfig,
        hw_admits: &impl Fn(&str, Option<&str>, &[String]) -> bool,
    ) -> Self {
        Self {
            events: sketches
                .get(cell)
                .map(|s| s.idle_gap_events.clone())
                .unwrap_or_default(),
            cell_placeable: placeable_for_cell(placeable, cell, hw_admits),
            boot_median: sketches
                .get(cell)
                .and_then(|s| s.boot_median())
                .unwrap_or_else(|| cfg.seed_for(cell)),
            min: cfg.min_consolidation_time_for(cell),
        }
    }

    /// NA break-even threshold for a `node_cores`-core node in this
    /// cell. The ONLY call into `consolidate_after` from `reap_idle` —
    /// both the hold-open arm (`2.0 * na_threshold`) and the non-hold-
    /// open arm read this. r37 bug_009: pre-CellCtx the hold-open arm
    /// passed `(events=&[], e_fitting_cores=0.0)`, which always returns
    /// `floor` — a busy cell's non-hold-open threshold could exceed
    /// `2×floor`, so the operator-annotated node was reaped FIRST.
    fn na_threshold(&self, node_cores: u32, max: Option<f64>) -> f64 {
        consolidate_after(
            &self.events,
            e_fitting_cores(&self.cell_placeable, node_cores),
            node_cores,
            self.boot_median,
            max,
            self.min,
        )
    }
}

/// Per-tick read-only inputs for [`reap_idle`]. Bundled so the
/// `nodeclaims`/`live`/`sketches` ↔ `inputs` borrow split is visible
/// at the callsite (mutable state vs. read-only context) and the
/// signature stays inside the `too_many_arguments` budget after the
/// r37 bug_006 `all_cells` addition.
pub struct ReapInputs<'a, F: Fn(&str, Option<&str>, &[String]) -> bool> {
    /// FFD placements for this tick (`reserved` + per-cell partition).
    pub placeable: &'a [Placement],
    /// `cfg.all_cells(&hw_config)` — the gauge-reset and per-cell
    /// precompute key set. r37 bug_006: every cell needs a write.
    pub all_cells: &'a [Cell],
    pub cfg: &'a NodeClaimPoolConfig,
    pub hw_admits: F,
    pub now_secs: f64,
}

/// Reap idle Registered NodeClaims past their break-even threshold.
///
/// A node is reapable when: `registered` AND not `terminating` AND not
/// in this tick's FFD `reserved` set AND `idle_secs > threshold`.
/// `threshold` is [`consolidate_after`] over the cell's
/// `idle_gap_events`, or `max_consolidation_time` for hold-open nodes.
/// Each reap records a censored `IdleGapEvent`. `Api::delete` 404 is
/// ignored (already-gone race with Karpenter); other errors warn + skip.
pub async fn reap_idle<F: Fn(&str, Option<&str>, &[String]) -> bool>(
    nodeclaims: &Api<NodeClaim>,
    live: &[LiveNode],
    sketches: &mut CellSketches,
    inputs: &ReapInputs<'_, F>,
) -> anyhow::Result<()> {
    let ReapInputs {
        placeable,
        all_cells,
        cfg,
        hw_admits,
        now_secs,
    } = inputs;
    let now_secs = *now_secs;
    let reserved: HashSet<&str> = placeable.iter().map(|(_, n, _)| n.as_str()).collect();

    // Phase 0: per-cell context, computed once per cell. r37 bug_006:
    // iterate all_cells so a cell with no idle/registered/unreserved
    // nodes gets a 0 gauge write instead of staling at the last value
    // (matches `emit_live_gauges`'s `cfg.all_cells()` convention and
    // `lib.rs` describe_gauge!'s "0 when no idle nodes" promise).
    let mut cell_ctx: HashMap<Cell, CellCtx> = HashMap::with_capacity(all_cells.len());
    for cell in *all_cells {
        metrics::gauge!(
            "rio_controller_nodeclaim_consolidate_threshold_seconds",
            "cell" => cell.to_string(),
        )
        .set(0.0);
        cell_ctx.insert(
            cell.clone(),
            CellCtx::new(cell, placeable, sketches, cfg, hw_admits),
        );
    }

    for n in live {
        let Some(cell) = n.cell.as_ref() else {
            continue;
        };
        // qa-fix-b: terminating nodes are Karpenter's finalizer's
        // problem. A second `delete` is idempotent (404-tolerated) but
        // would double-increment `nodeclaim_reaped_total` and double-
        // push the censored `IdleGapEvent`, biasing the NA-model
        // arrival rate low.
        if n.terminating || !n.registered || reserved.contains(n.name.as_str()) {
            continue;
        }
        // Busy = Karpenter Empty=False, OR PodRequestedCache saw a
        // binding before Karpenter flipped the condition (the same
        // race `observe_idle_to_busy` guards). Without the
        // `requested.0==0` check, a tight-fit node whose pod just
        // bound (so `free()=0` → not in `reserved`) but whose Empty
        // condition is stale/unwritten can be reaped mid-build.
        let Some(idle) = n.idle_secs(now_secs).filter(|_| n.requested.0 == 0) else {
            continue;
        };
        // A live node may carry a cell removed from config mid-rollout —
        // derive on demand so it's still reaped (leaking is worse).
        if !cell_ctx.contains_key(cell) {
            cell_ctx.insert(
                cell.clone(),
                CellCtx::new(cell, placeable, sketches, cfg, hw_admits),
            );
        }
        let ctx = &cell_ctx[cell];
        // r37 bug_009: hold-open and non-hold-open read the SAME
        // na_threshold. With `cfg.max_consolidation_time` unset, the
        // hold-open node holds 2× whatever the NA model recommends for
        // un-annotated nodes in the same cell — the protection
        // annotation cannot invert.
        let na = ctx.na_threshold(n.allocatable.0, cfg.max_consolidation_time);
        let threshold = if n.annotation(HOLD_OPEN_ANNOTATION) == Some("true") {
            cfg.max_consolidation_time.unwrap_or(2.0 * na)
        } else {
            na
        };
        // Last-write-wins per cell within a tick (nodes in a cell share
        // an hw-class; allocatable can vary across instance types within
        // the class but the threshold's order of magnitude doesn't).
        // Operator check: `fetcher-*` cells ≥ 600s floor; builder cells
        // ≥ 60s `*` floor unless λ·E[c_fit] holds them higher. A cell
        // at boot_median/2 (~9-25s) when its minConsolidationTime entry
        // says 60/600s = the prefix-glob didn't match.
        metrics::gauge!(
            "rio_controller_nodeclaim_consolidate_threshold_seconds",
            "cell" => cell.to_string(),
        )
        .set(threshold);
        if idle <= threshold {
            continue;
        }
        match nodeclaims.delete(&n.name, &DeleteParams::default()).await {
            Ok(_) => {
                debug!(name = %n.name, %cell, idle, threshold, "reaped idle NodeClaim");
                metrics::counter!(
                    "rio_controller_nodeclaim_reaped_total",
                    "reason" => "idle",
                    "cell" => cell.to_string(),
                )
                .increment(1);
                push_idle_gap(
                    sketches,
                    cell,
                    IdleGapEvent {
                        gap_secs: idle,
                        censored: true,
                    },
                );
            }
            Err(kube::Error::Api(ae)) if ae.code == 404 => {}
            Err(e) => warn!(name = %n.name, error = %e, "idle NodeClaim delete failed; skipping"),
        }
    }
    Ok(())
}

/// Edge-detect idle→busy transitions and record them as uncensored
/// [`IdleGapEvent`]s. `prev_idle` is the reconciler's running
/// `name → idle_secs` map from the previous tick; a node present there
/// whose `idle_secs` is now `None` (Karpenter `Empty=False`) OR whose
/// `requested.0 > 0` (PodRequestedCache saw a binding before Karpenter
/// flipped the condition) had an arrival — record `{prev_idle[name],
/// censored:false}` to its cell. `prev_idle` is then refreshed to
/// `idle_secs(now)` for nodes still idle and pruned of names absent
/// from `live` (reaped/gone — `reap_idle` records the censored event).
///
/// Called from `reconcile_once` after `list_live_nodeclaims` (so
/// `requested` is populated) and before `reap_idle`. Without this every
/// `IdleGapEvent` is censored → `λ(t)=0` → `consolidate_after =
/// boot_median/2` floor regardless of arrival rate.
pub fn observe_idle_to_busy(
    live: &[LiveNode],
    prev_idle: &mut HashMap<String, f64>,
    sketches: &mut CellSketches,
    now_secs: f64,
) {
    // Terminating nodes excluded from `live_names` AND iteration: a node
    // that started terminating since last tick should drop from `prev_idle`
    // WITHOUT recording an event — the censored event was (or will be)
    // pushed by `reap_idle`/`reap_unhealthy` at delete time. Recording
    // here would double-count, and Karpenter flipping `Empty=False`
    // during drain would log a bogus uncensored "arrival".
    let live_names: HashSet<&str> = live
        .iter()
        .filter(|n| !n.terminating)
        .map(|n| n.name.as_str())
        .collect();
    prev_idle.retain(|name, _| live_names.contains(name.as_str()));
    for n in live {
        if n.terminating {
            continue;
        }
        let idle = n.idle_secs(now_secs);
        let busy = idle.is_none() || n.requested.0 > 0;
        if busy {
            if let (Some(&gap), Some(cell)) = (prev_idle.get(&n.name), n.cell.as_ref()) {
                push_idle_gap(
                    sketches,
                    cell,
                    IdleGapEvent {
                        gap_secs: gap,
                        censored: false,
                    },
                );
            }
            prev_idle.remove(&n.name);
        } else if let Some(idle) = idle {
            prev_idle.insert(n.name.clone(), idle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::ffd::tests::{node, with_conds};
    use super::super::sketch::CapacityType;
    use super::*;
    use rio_proto::types::SpawnIntent;

    fn ev(gap: f64, censored: bool) -> IdleGapEvent {
        IdleGapEvent {
            gap_secs: gap,
            censored,
        }
    }

    /// Censoring distinction in `consolidate_after`'s windowed λ:
    /// censored events count toward at-risk but NOT hits. With
    /// `[5.0u, 10.0u, 15.0c]`, w=floor=5, t=5: at-risk=3, hits=1
    /// (event 5.0 in [5,10)) → λ=1/15. With all-censored → λ=0
    /// everywhere → returns floor.
    #[test]
    fn censored_events_at_risk_only() {
        // boot_median=10 → floor=w=5. node=8 → rhs=0.8.
        // [5u,10u,15c]: t=5: at-risk=3, hits=1 (5.0∈[5,10))
        // → λ=1/(5·3)≈0.067. λ·E_fit must exceed rhs to keep:
        // E_fit=100 → 6.67 > 0.8 → continues past floor.
        let evs = [ev(5.0, false), ev(10.0, false), ev(15.0, true)];
        assert!(consolidate_after(&evs, 100.0, 8, 10.0, None, None) > 5.0);
        // All-censored → λ=0 (hits=0) → floor.
        let cens = [ev(5.0, true), ev(10.0, true), ev(15.0, true)];
        assert_eq!(consolidate_after(&cens, 100.0, 8, 10.0, None, None), 5.0);
    }

    /// r[ctrl.nodeclaim.consolidate-na+4]: floor = q_0.5(boot)/2. With
    /// no events (λ=0), break-even fires immediately → returns floor.
    // r[verify ctrl.nodeclaim.consolidate-na+4]
    #[test]
    fn consolidate_after_respects_floor() {
        // boot_median=40 → floor=20. λ=0 → immediate break-even → 20.
        let t = consolidate_after(&[], 4.0, 8, 40.0, None, None);
        assert_eq!(t, 20.0);
        // Explicit max ceiling.
        let evs: Vec<_> = (1..=100).map(|k| ev(k as f64, false)).collect();
        // Dense events, high E[c_fit] → λ·E stays > rhs through to max.
        let t2 = consolidate_after(&evs, 1e6, 8, 40.0, Some(50.0), None);
        assert_eq!(t2, 50.0, "max ceiling");
    }

    /// 192c node, mean fitting c_arr=4, dense arrivals at 21..30.
    /// w=20 → λ(20) = 10/(20·10) = 0.05. RHS = 192/40 = 4.8.
    /// 0.05·4 = 0.2 < 4.8 → break-even at floor → delete.
    #[test]
    fn keep_condition_uses_fitting_core_expectation() {
        let evs: Vec<_> = (21..=30).map(|k| ev(f64::from(k), false)).collect();
        // E[c_fit]=4, node=192, boot=40: λ·E ≪ rhs → floor.
        let t = consolidate_after(&evs, 4.0, 192, 40.0, None, None);
        assert_eq!(t, 20.0);
        // Same events, E[c_fit]=100, node=8: λ(20)·100 = 5 >
        // 8/40=0.2 → keep past floor; break-even after the cluster.
        let t2 = consolidate_after(&evs, 100.0, 8, 40.0, Some(100.0), None);
        assert!(t2 >= 30.0, "kept while λ·E > rhs; t2={t2}");
    }

    /// Regression for the 1s-finite-difference degeneracy: sparse
    /// non-integer events. The old `(H(t+1)-H(t))/1.0` form was zero
    /// at t=20 (no event in (20,21]) so the scan returned floor=20.0
    /// regardless of arrival rate. Windowed estimator with w=20
    /// sees event 25.3 in [20,40) → λ>0 → scan proceeds past floor.
    #[test]
    fn consolidate_after_sparse_events_nondegenerate() {
        let evs = [ev(25.3, false), ev(47.1, false), ev(80.9, false)];
        // E[c_fit]=100, node=8, boot=40: rhs=0.2. λ(20)=1/(20·3)≈0.017,
        // λ·E≈1.67 > 0.2 → does NOT return at floor.
        let t = consolidate_after(&evs, 100.0, 8, 40.0, None, None);
        assert!(
            t > 20.0,
            "sparse events should not collapse to floor; got {t}"
        );
        // Sanity: with E[c_fit]=0 (no demand), break-even at floor.
        assert_eq!(consolidate_after(&evs, 0.0, 8, 40.0, None, None), 20.0);
    }

    /// `reap_idle`'s busy predicate matches `observe_idle_to_busy`:
    /// `requested.0 > 0` is busy even when Karpenter hasn't yet
    /// written `Empty=False`. A node with a freshly-bound pod
    /// (requested>0, Empty unwritten → idle_secs falls back to
    /// since-Registered) MUST NOT be reapable.
    #[test]
    fn reap_idle_skips_nonzero_requested() {
        let mut n = with_conds(
            node("bound", "h", CapacityType::Spot, 8, 0, 0),
            &[("Registered", "True", 1000.0)],
        );
        // No Empty condition → idle_secs = now − Registered.
        assert_eq!(n.idle_secs(1012.0), Some(12.0));
        // But a pod is bound (requested > 0) → reap_idle's filter
        // treats this as busy.
        n.requested = (8, 0, 0);
        assert_eq!(
            n.idle_secs(1012.0).filter(|_| n.requested.0 == 0),
            None,
            "requested>0 → not reapable even with idle_secs.is_some()"
        );
        // Same node with requested=0 → idle, reapable.
        n.requested = (0, 0, 0);
        assert_eq!(
            n.idle_secs(1012.0).filter(|_| n.requested.0 == 0),
            Some(12.0)
        );
    }

    #[test]
    fn e_fitting_cores_mean_of_fitting() {
        let p = |c: u32| -> Placement {
            (
                SpawnIntent {
                    cores: c,
                    ..Default::default()
                },
                "n".into(),
                false,
            )
        };
        // node=8: intents 4,6,12 → fitting={4,6}, mean=5.
        assert_eq!(e_fitting_cores(&[p(4), p(6), p(12)], 8), 5.0);
        // None fit → 0.
        assert_eq!(e_fitting_cores(&[p(12), p(16)], 8), 0.0);
        // Empty → 0 (spec: ⊥/empty → 0).
        assert_eq!(e_fitting_cores(&[], 8), 0.0);
    }

    /// r35 bug_023 (§Simulator-shares-accounting): `e_fitting_cores`
    /// must be a per-cell mean. §13e removed the `kind` filter from
    /// `GetSpawnIntents`, so `placeable` mixes fetcher (`c*≈1`) and
    /// builder intents — a global mean for a 32c builder node biases
    /// `E[c_fit]` ~5× low (12 intents avg=6.17 instead of 32). The
    /// filter is the SAME predicate FFD's `hw_admits` uses, NOT a
    /// `hw_class_names.is_empty()` pass-through (which over-counts
    /// hw-agnostic intents on fetcher cells).
    // r[verify ctrl.nodeclaim.consolidate-na+4]
    #[test]
    fn e_fitting_cores_per_cell_excludes_cross_kind() {
        use rio_proto::types::{NodeSelectorRequirement, NodeSelectorTerm};
        // r37 bug_002: post-fix `placeable_for_cell` routes through
        // `cells_of(i)`, which zips `hw_class_names` with the parallel
        // `node_affinity` term carrying `karpenter.sh/capacity-type`.
        // The pre-fix fixture left `node_affinity` empty (only the
        // hw_class half of the cell key was tested) — under the new
        // predicate `cells_of` returns `[]` and every intent is
        // unplaceable. The scheduler always emits both arrays in
        // parallel; the fixture now matches that shape (`cap="spot"` to
        // keep the existing `CapacityType::Spot` cell assertions green).
        let p = |c: u32, hw: &str, sys: &str| -> Placement {
            (
                SpawnIntent {
                    cores: c,
                    hw_class_names: vec![hw.into()],
                    node_affinity: vec![NodeSelectorTerm {
                        match_expressions: vec![NodeSelectorRequirement {
                            key: super::super::ffd::CAPACITY_TYPE_LABEL.into(),
                            operator: "In".into(),
                            values: vec!["spot".into()],
                        }],
                    }],
                    system: sys.into(),
                    ..Default::default()
                },
                "n".into(),
                false,
            )
        };
        let mut placeable: Vec<Placement> = (0..10)
            .map(|_| p(1, "fetcher-x86", "x86_64-linux"))
            .collect();
        placeable.push(p(32, "hi-ebs-x86", "x86_64-linux"));
        placeable.push(p(32, "hi-ebs-x86", "x86_64-linux"));
        // Global mean over all 12: (10×1 + 2×32)/12 ≈ 6.17. Per-cell
        // mean for hi-ebs-x86: 32.
        let builder_cell = Cell("hi-ebs-x86".into(), CapacityType::Spot);
        let admits = |_: &str, _: Option<&str>, _: &[String]| true;
        let cell_placeable = placeable_for_cell(&placeable, &builder_cell, admits);
        let e = e_fitting_cores(&cell_placeable, 32);
        assert!(
            (e - 32.0).abs() < 1e-9,
            "per-cell E[c_fit] for hi-ebs-x86 should be 32, got {e}              (global mean would be ≈6.17)"
        );
        // The fetcher cell sees only the 10 fetcher intents.
        let fetcher_cell = Cell("fetcher-x86".into(), CapacityType::Spot);
        let cell_placeable = placeable_for_cell(&placeable, &fetcher_cell, admits);
        let e = e_fitting_cores(&cell_placeable, 4);
        assert!((e - 1.0).abs() < 1e-9, "fetcher cell E[c_fit]=1, got {e}");
        // hw-agnostic intents (`hw_class_names=[]`) only count on cells
        // `hw_admits` accepts — NOT a pass-through for every cell.
        let agnostic: Placement = (
            SpawnIntent {
                cores: 16,
                hw_class_names: vec![],
                system: "x86_64-linux".into(),
                ..Default::default()
            },
            "n".into(),
            false,
        );
        let mixed = vec![p(1, "fetcher-x86", "x86_64-linux"), agnostic];
        let admits_builder_only = |h: &str, _: Option<&str>, _: &[String]| h.starts_with("hi-");
        let cp = placeable_for_cell(&mixed, &fetcher_cell, admits_builder_only);
        assert_eq!(
            e_fitting_cores(&cp, 4),
            1.0,
            "hw-agnostic intent excluded from fetcher cell when hw_admits rejects"
        );
        let cp = placeable_for_cell(&mixed, &builder_cell, admits_builder_only);
        assert_eq!(
            e_fitting_cores(&cp, 32),
            16.0,
            "hw-agnostic intent admitted on builder cell"
        );

        // r35 B1 ripple: a `system="builtin"` FOD (`hw_class_names=[]`,
        // `arch=None`, `features=["fetcher"]`) routes by FEATURES.
        // Pre-B1 the `is_some_and(|a| ...)` short-circuit dropped it
        // (arch=None ⇒ never placeable for any cell) so a fetcher cell
        // whose only demand is builtin FODs read as zero placeable →
        // `reap_idle` reaped the cell its own demand was waiting for.
        let builtin_fod: Placement = (
            SpawnIntent {
                cores: 1,
                hw_class_names: vec![],
                system: "builtin".into(),
                required_features: vec!["fetcher".into()],
                ..Default::default()
            },
            "n".into(),
            false,
        );
        let admits_fetcher = |h: &str, _: Option<&str>, f: &[String]| {
            h.starts_with("fetcher-") && f.contains(&"fetcher".into())
        };
        let cp = placeable_for_cell(
            std::slice::from_ref(&builtin_fod),
            &fetcher_cell,
            admits_fetcher,
        );
        assert_eq!(
            cp.len(),
            1,
            "builtin FOD must be counted as placeable for the fetcher cell — \
             arch=None routes by features, mirroring FFD's agnostic-fallback gate"
        );
        // arch=None ∧ features=[] is genuinely unroutable: still excluded.
        let unroutable: Placement = (
            SpawnIntent {
                cores: 1,
                hw_class_names: vec![],
                system: "builtin".into(),
                required_features: vec![],
                ..Default::default()
            },
            "n".into(),
            false,
        );
        let cp = placeable_for_cell(&[unroutable], &fetcher_cell, |_, _, _| true);
        assert!(
            cp.is_empty(),
            "arch=None ∧ features=[] is unroutable — the (a.is_some() || \
             !f.is_empty()) gate excludes it from every cell"
        );
    }

    /// r35 bug_050 (§Granularity-coupling): §13e silently reduced
    /// fetcher idle grace from Karpenter's `consolidateAfter: 10m` to
    /// the NA-model floor `boot_median/2` (~15s). The operator-settable
    /// per-class `min_consolidation_time` is the structural close — the
    /// NA model is correct, the policy floor was missing.
    // r[verify ctrl.nodeclaim.consolidate-na+4]
    #[test]
    fn consolidate_after_respects_min_floor() {
        // boot_median=30 → NA floor = 15. With λ=0 (no events) the
        // break-even fires at the floor — but `min=600` overrides it.
        let t = consolidate_after(&[], 1.0, 1, 30.0, None, Some(600.0));
        assert_eq!(t, 600.0, "min floor overrides boot_median/2 floor");
        // Without `min` the old behavior holds (boot_median/2 floor).
        assert_eq!(consolidate_after(&[], 1.0, 1, 30.0, None, None), 15.0);
        // `min` < `boot_median/2` → NA floor wins (max of the two).
        assert_eq!(consolidate_after(&[], 1.0, 1, 30.0, None, Some(5.0)), 15.0);
    }

    #[test]
    fn idle_gap_ring_caps() {
        let mut sk = CellSketches::default();
        let cell = Cell("h".into(), CapacityType::Spot);
        for k in 0..(IDLE_GAP_RING + 10) {
            push_idle_gap(&mut sk, &cell, ev(k as f64, false));
        }
        let evs = &sk.get(&cell).unwrap().idle_gap_events;
        assert_eq!(evs.len(), IDLE_GAP_RING);
        // Oldest dropped: first remaining is k=10.
        assert_eq!(evs[0].gap_secs, 10.0);
    }

    /// `reap_idle`'s reapability filter: registered ∧ ¬reserved ∧
    /// idle > threshold. With no events, threshold = boot_median/2.
    /// Kube side-effect not tested here (covered in VM tests); this
    /// asserts the filter via a fake `live` set against the pure
    /// threshold function.
    #[test]
    fn no_reap_when_busy_or_reserved() {
        let mut sk = CellSketches::default();
        let cell = Cell("h".into(), CapacityType::Spot);
        for _ in 0..10 {
            sk.cell_mut(&cell).record(40.0, 0.0);
        }
        let cfg = NodeClaimPoolConfig::default();
        // boot_median ≈ 40 → floor = 20. Node idle 30s > 20 → reapable
        // unless reserved/busy.
        let idle_node = with_conds(
            node("idle", "h", CapacityType::Spot, 8, 0, 0),
            &[("Registered", "True", 1000.0), ("Empty", "True", 1070.0)],
        );
        // now=1100 → idle=30s.
        assert_eq!(idle_node.idle_secs(1100.0), Some(30.0));
        let threshold = consolidate_after(
            &[],
            e_fitting_cores(&[], 8),
            8,
            sk.get(&cell).unwrap().boot_median().unwrap(),
            cfg.max_consolidation_time,
            None,
        );
        assert!(30.0 > threshold, "idle past floor");

        // Busy (Empty=False) → idle_secs=None → never reapable.
        let busy = with_conds(
            node("busy", "h", CapacityType::Spot, 8, 0, 0),
            &[("Registered", "True", 1000.0), ("Empty", "False", 1070.0)],
        );
        assert_eq!(busy.idle_secs(1100.0), None);

        // Reserved (in placeable) → skipped regardless of idle.
        let reserved: HashSet<&str> = ["idle"].into();
        assert!(reserved.contains(idle_node.name.as_str()));
    }

    /// F8: a node idle 40s last tick, busy this tick (`requested.0>0`)
    /// → uncensored `IdleGapEvent{40.0,false}` recorded; `prev_idle`
    /// updated for nodes that stay idle; nodes gone from `live` evicted
    /// from `prev_idle`.
    #[test]
    fn observe_idle_to_busy_pushes_uncensored() {
        let mut sk = CellSketches::default();
        let cell = Cell("h".into(), CapacityType::Spot);
        let mut prev_idle: HashMap<String, f64> = [("a".into(), 40.0), ("b".into(), 15.0)].into();

        // Tick: a now busy (requested=4c), b still idle (Empty=True at
        // 1100, requested=0), c is new (idle since registered=1042).
        let mut a = with_conds(
            node("a", "h", CapacityType::Spot, 8, 0, 0),
            &[("Registered", "True", 1042.0), ("Empty", "False", 1150.0)],
        );
        a.requested = (4, 0, 0);
        let b = with_conds(
            node("b", "h", CapacityType::Spot, 8, 0, 0),
            &[("Registered", "True", 1042.0), ("Empty", "True", 1100.0)],
        );
        let c = with_conds(
            node("c", "h", CapacityType::Spot, 8, 0, 0),
            &[("Registered", "True", 1042.0)],
        );
        observe_idle_to_busy(&[a, b, c], &mut prev_idle, &mut sk, 1160.0);

        let evs = &sk.get(&cell).unwrap().idle_gap_events;
        assert_eq!(evs.len(), 1, "only a's idle→busy edge recorded");
        assert!((evs[0].gap_secs - 40.0).abs() < 1e-9);
        assert!(!evs[0].censored, "uncensored");
        // prev_idle: a evicted (busy), b updated to 60s, c added at 118s.
        assert!(!prev_idle.contains_key("a"));
        assert!((prev_idle["b"] - 60.0).abs() < 1e-9);
        assert!((prev_idle["c"] - 118.0).abs() < 1e-9);

        // Next tick: b reaped (gone from live). prev_idle prunes b
        // without recording an uncensored event (reap_idle records the
        // censored one).
        observe_idle_to_busy(&[], &mut prev_idle, &mut sk, 1170.0);
        assert!(prev_idle.is_empty());
        assert_eq!(sk.get(&cell).unwrap().idle_gap_events.len(), 1);
    }

    /// A node that started terminating since last tick (Karpenter
    /// finalizer running) drops from `prev_idle` WITHOUT recording an
    /// idle-gap event — `reap_idle`/`reap_unhealthy` already pushed the
    /// censored event at delete time. Recording here would double-count
    /// AND, when Karpenter flips `Empty=False` during drain, would log
    /// a bogus uncensored "arrival" that biases the NA-model arrival
    /// rate up (holding nodes open longer than warranted).
    #[test]
    fn observe_idle_to_busy_skips_terminating() {
        use super::super::ffd::tests::terminating;
        let mut sk = CellSketches::default();
        let cell = Cell("h".into(), CapacityType::Spot);
        let mut prev_idle: HashMap<String, f64> = [("dying".into(), 80.0)].into();
        // Karpenter drain flips Empty=False AND sets deletionTimestamp
        // → naively this looks like an idle→busy edge.
        let mut dying = terminating(with_conds(
            node("dying", "h", CapacityType::Spot, 8, 0, 0),
            &[("Registered", "True", 1000.0), ("Empty", "False", 1090.0)],
        ));
        dying.requested = (4, 0, 0);
        observe_idle_to_busy(&[dying], &mut prev_idle, &mut sk, 1100.0);
        assert!(
            sk.get(&cell).is_none_or(|s| s.idle_gap_events.is_empty()),
            "terminating drain is not an arrival; no idle-gap event"
        );
        assert!(
            !prev_idle.contains_key("dying"),
            "terminating node pruned from prev_idle without an event"
        );
    }

    /// Hold-open annotation → threshold = max_consolidation_time
    /// instead of NA break-even.
    #[test]
    fn hold_open_uses_max_consolidation_time() {
        let mut n = node("ho", "h", CapacityType::Spot, 8, 0, 0);
        n.annotations
            .insert(HOLD_OPEN_ANNOTATION.into(), "true".into());
        assert_eq!(n.annotation(HOLD_OPEN_ANNOTATION), Some("true"));
        // The reap_idle logic reads this; threshold becomes the cfg
        // value (or 2×floor default).
        let cfg = NodeClaimPoolConfig {
            max_consolidation_time: Some(300.0),
            ..Default::default()
        };
        assert_eq!(cfg.max_consolidation_time, Some(300.0));
    }

    /// r37 merged_010 (§Granularity-coupling): `w` decoupled from `floor`.
    /// With `min ≥ boot_median` (the chart default `*: 60.0` for an 18s-
    /// boot builder), the pre-fix `w = floor.max(5.0)` made `λ·E > rhs`
    /// unsatisfiable (`λ ≤ 1/w < cores/boot_median`) — the NA model
    /// returned `floor` unconditionally. The decoupled `w = boot_median/2`
    /// makes the keep-condition satisfiable again so the model can extend
    /// past the floor under load, which is the documented purpose of the
    /// floor (`values.yaml: "the knob is a *floor* the model can exceed"`).
    // r[verify ctrl.nodeclaim.consolidate-na+4]
    #[test]
    fn consolidate_after_extends_past_policy_floor() {
        // Builder cell shape: boot_median=18, min=60 (qa-fix-b chart
        // default). Dense arrivals at 60..70s (right past the floor),
        // E[c_fit]=64, node=64.
        let evs: Vec<_> = (61..=70).map(|k| ev(f64::from(k), false)).collect();
        let t = consolidate_after(&evs, 64.0, 64, 18.0, None, Some(60.0));
        // floor = max(9, 60) = 60. w = max(9, 5) = 9. rhs = 64/18 ≈ 3.56.
        // λ(60) over [60,69) = 8 hits / (9·10) ≈ 0.0889; λ·E ≈ 5.69 > 3.56
        // → extend past floor (returns 73.5: first t where the window is empty).
        assert!(
            t > 60.0,
            "NA model must extend past min floor under load; got {t}"
        );
        // Pre-fix this returned exactly 60.0 unconditionally.
    }

    /// r37 merged_010: `max_consolidation_time < floor` no longer slips
    /// through. `floor` is the policy lower bound; an operator setting
    /// `max < min` is a misconfig (scheduler.sla.maxConsolidationTime <
    /// karpenter.nodeclaimPool.minConsolidationTime) but the floor still
    /// holds — silently dropping below it would burn a boot every
    /// `max_consolidation_time` seconds.
    // r[verify ctrl.nodeclaim.consolidate-na+4]
    #[test]
    fn consolidate_after_max_t_clamped_at_floor() {
        // boot=18, min=60 → floor=60. max=10 < floor.
        assert_eq!(
            consolidate_after(&[], 0.0, 8, 18.0, Some(10.0), Some(60.0)),
            60.0
        );
        // No min: floor=9. max=5 < 9.
        assert_eq!(consolidate_after(&[], 0.0, 8, 18.0, Some(5.0), None), 9.0);
    }

    /// r37 bug_002 (§Simulator-shares-accounting): `placeable_for_cell`
    /// must respect the capacity-type half of the cell key. An intent
    /// whose admissible set is `[(h, OnDemand)]` only — `--capacity=
    /// on-demand` override, ICE-masked spot, InterruptRunaway — is NOT
    /// demand for `(h, Spot)`. Counting it inflates `e_fitting_cores` and
    /// over-extends the spot cell's hold-open.
    // r[verify ctrl.nodeclaim.consolidate-na+4]
    #[test]
    fn placeable_for_cell_respects_capacity_type() {
        use rio_proto::types::{NodeSelectorRequirement, NodeSelectorTerm};
        let with_cap = |cap: &str| -> Placement {
            (
                SpawnIntent {
                    cores: 8,
                    hw_class_names: vec!["hi-ebs-x86".into()],
                    node_affinity: vec![NodeSelectorTerm {
                        match_expressions: vec![NodeSelectorRequirement {
                            key: super::super::ffd::CAPACITY_TYPE_LABEL.into(),
                            operator: "In".into(),
                            values: vec![cap.into()],
                        }],
                    }],
                    ..Default::default()
                },
                "n".into(),
                false,
            )
        };
        let on_demand_only = vec![with_cap("on-demand")];
        let spot_cell = Cell("hi-ebs-x86".into(), CapacityType::Spot);
        let od_cell = Cell("hi-ebs-x86".into(), CapacityType::OnDemand);
        let admits = |_: &str, _: Option<&str>, _: &[String]| true;
        assert!(
            placeable_for_cell(&on_demand_only, &spot_cell, admits).is_empty(),
            "on-demand-only intent is NOT demand for the spot cell"
        );
        assert_eq!(
            placeable_for_cell(&on_demand_only, &od_cell, admits).len(),
            1,
            "on-demand-only intent IS demand for the on-demand cell"
        );
    }

    /// r37 bug_009 (§Permissive-restrictive asymmetry, applied to time):
    /// a hold-open node must never be reaped BEFORE an un-annotated node
    /// in the same cell. With the per-cell `CellCtx`, both arms read the
    /// same `na_threshold`, so hold-open ≥ non-hold-open by construction.
    #[test]
    fn hold_open_threshold_never_below_non_hold_open() {
        use rio_proto::types::{NodeSelectorRequirement, NodeSelectorTerm};
        let mut sk = CellSketches::default();
        let cell = Cell("h".into(), CapacityType::Spot);
        // Fixture: boot=18 (small → tight `w`), gaps clustered just
        // past the model floor `boot/2≈9`, and `min_consolidation_time`
        // cleared so the policy floor doesn't dominate. With the chart
        // default `*: 60.0` floor and any gap ring, the busy NA
        // threshold is bounded by `max_t ≤ 2×max(floor, gap)` and
        // cannot strictly exceed `2×floor` unless gaps exceed the
        // floor — the bug_009 inversion is structurally about the
        // hold-open arm reading a degenerate `consolidate_after(&[],
        // 0.0, ...)` (always = floor) while the busy arm reads the
        // events; the policy floor is orthogonal noise here.
        for _ in 0..10 {
            sk.cell_mut(&cell).record(18.0, 0.0);
        }
        for k in 10..=20 {
            push_idle_gap(&mut sk, &cell, ev(f64::from(k), false));
        }
        let placeable: Vec<Placement> = (0..20)
            .map(|_| {
                (
                    SpawnIntent {
                        cores: 8,
                        hw_class_names: vec!["h".into()],
                        node_affinity: vec![NodeSelectorTerm {
                            match_expressions: vec![NodeSelectorRequirement {
                                key: super::super::ffd::CAPACITY_TYPE_LABEL.into(),
                                operator: "In".into(),
                                values: vec!["spot".into()],
                            }],
                        }],
                        ..Default::default()
                    },
                    "n".into(),
                    false,
                )
            })
            .collect();
        let cfg = NodeClaimPoolConfig {
            min_consolidation_time: HashMap::new(),
            ..Default::default()
        };
        let admits = |_: &str, _: Option<&str>, _: &[String]| true;
        let ctx = CellCtx::new(&cell, &placeable, &sk, &cfg, &admits);
        let na = ctx.na_threshold(8, None);
        // Pre-fix hold-open arm: `2.0 * consolidate_after(&[], 0.0, ...)`
        // — always `2×floor` (`λ=0` with no events). With dense arrivals
        // the busy arm extends past `2×floor` and the operator-protected
        // node would be reaped FIRST.
        let pre_fix_hold_open =
            2.0 * consolidate_after(&[], 0.0, 8, ctx.boot_median, None, ctx.min);
        assert!(
            na > pre_fix_hold_open,
            "PRECONDITION: busy NA threshold {na} must exceed pre-fix hold-open {pre_fix_hold_open}"
        );
        // Post-fix: hold-open = `2 × ctx.na_threshold` — both arms read
        // the SAME `ctx.na_threshold`, so hold-open ≥ non-hold-open by
        // construction. The structural protection is in `reap_idle`'s
        // shape (`let na = ctx.na_threshold(...); ... 2.0 * na`); these
        // assertions document the invariant the shape enforces.
        assert!(2.0 * na >= pre_fix_hold_open);
        assert!(2.0 * na >= na);
    }
}
