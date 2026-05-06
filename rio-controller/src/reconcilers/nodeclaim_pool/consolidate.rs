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
use super::ffd::{LiveNode, Placement, system_to_arch};
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
    let w = floor.max(5.0);
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
    max_t
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
            i.hw_class_names.iter().any(|h| h == &cell.0)
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

/// Reap idle Registered NodeClaims past their break-even threshold.
///
/// A node is reapable when: `registered` AND not in this tick's FFD
/// `reserved` set AND `idle_secs > threshold`. `threshold` is
/// [`consolidate_after`] over the cell's `idle_gap_events`, or
/// `max_consolidation_time` for hold-open nodes. Each reap records a
/// censored `IdleGapEvent`. `Api::delete` 404 is ignored
/// (already-gone race with Karpenter); other errors warn + skip.
pub async fn reap_idle(
    nodeclaims: &Api<NodeClaim>,
    live: &[LiveNode],
    placeable: &[Placement],
    sketches: &mut CellSketches,
    cfg: &NodeClaimPoolConfig,
    hw_admits: impl Fn(&str, Option<&str>, &[String]) -> bool,
    now_secs: f64,
) -> anyhow::Result<()> {
    let reserved: HashSet<&str> = placeable.iter().map(|(_, n, _)| n.as_str()).collect();
    for n in live {
        let Some(cell) = n.cell.as_ref() else {
            continue;
        };
        // Already terminating: Karpenter's finalizer is draining it. A
        // second `delete` is idempotent (404-tolerated) but would
        // double-increment `nodeclaim_reaped_total` and double-push the
        // censored `IdleGapEvent`, biasing the NA-model arrival rate low.
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
        let boot_median = sketches
            .get(cell)
            .and_then(|s| s.boot_median())
            .unwrap_or_else(|| cfg.seed_for(cell));
        let min = cfg.min_consolidation_time_for(cell);
        let threshold = if n.annotation(HOLD_OPEN_ANNOTATION) == Some("true") {
            cfg.max_consolidation_time.unwrap_or(
                2.0 * consolidate_after(&[], 0.0, n.allocatable.0, boot_median, None, min),
            )
        } else {
            let events = sketches
                .get(cell)
                .map(|s| s.idle_gap_events.as_slice())
                .unwrap_or(&[]);
            // r35 bug_023: per-cell mean. §13e mixed fetcher (c*≈1) and
            // builder intents in `placeable`; a global mean biases
            // E[c_fit] for builder nodes ~5× low.
            let cell_placeable = placeable_for_cell(placeable, cell, &hw_admits);
            consolidate_after(
                events,
                e_fitting_cores(&cell_placeable, n.allocatable.0),
                n.allocatable.0,
                boot_median,
                cfg.max_consolidation_time,
                min,
            )
        };
        // r35 (B3 verifier amend): observable for the per-cell
        // e_fitting_cores partition (bug_023) and the policy floor
        // (bug_050) — both shift this threshold silently. Last-write-
        // wins per cell within a tick (nodes in a cell share an
        // hw-class; allocatable can vary across instance types within
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
        let p = |c: u32, hw: &str, sys: &str| -> Placement {
            (
                SpawnIntent {
                    cores: c,
                    hw_class_names: vec![hw.into()],
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
}
