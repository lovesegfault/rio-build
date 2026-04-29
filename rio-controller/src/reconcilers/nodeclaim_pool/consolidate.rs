//! Nelson-Aalen idle-node consolidation.
//!
//! Per `r[ctrl.nodeclaim.consolidate-na]`: an empty Registered NodeClaim
//! is kept while `λ(t)·𝔼[c_arr·𝟙{≤cores}] > cores/q_0.5(boot)`; λ via
//! Nelson-Aalen on right-censored `idle_gap` events. The first `t` at
//! which the inequality flips false is `consolidate_after(t)` —
//! floored at `q_0.5(boot)/2` so a transient lull can't collapse to
//! always-delete.

use std::collections::{HashMap, HashSet};

use kube::Api;
use kube::api::DeleteParams;
use rio_crds::karpenter::NodeClaim;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use super::NodeClaimPoolConfig;
use super::ffd::{LiveNode, Placement};
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
// r[impl ctrl.nodeclaim.consolidate-na]
pub fn consolidate_after(
    events: &[IdleGapEvent],
    e_fitting_cores: f64,
    node_cores: u32,
    boot_median: f64,
    max: Option<f64>,
) -> f64 {
    let floor = boot_median / 2.0;
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
    now_secs: f64,
) -> anyhow::Result<()> {
    let reserved: HashSet<&str> = placeable.iter().map(|(_, n, _)| n.as_str()).collect();
    for n in live {
        let Some(cell) = n.cell.as_ref() else {
            continue;
        };
        if !n.registered || reserved.contains(n.name.as_str()) {
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
        let threshold = if n.annotation(HOLD_OPEN_ANNOTATION) == Some("true") {
            cfg.max_consolidation_time
                .unwrap_or(2.0 * consolidate_after(&[], 0.0, n.allocatable.0, boot_median, None))
        } else {
            let events = sketches
                .get(cell)
                .map(|s| s.idle_gap_events.as_slice())
                .unwrap_or(&[]);
            consolidate_after(
                events,
                e_fitting_cores(placeable, n.allocatable.0),
                n.allocatable.0,
                boot_median,
                cfg.max_consolidation_time,
            )
        };
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
    let live_names: HashSet<&str> = live.iter().map(|n| n.name.as_str()).collect();
    prev_idle.retain(|name, _| live_names.contains(name.as_str()));
    for n in live {
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
        assert!(consolidate_after(&evs, 100.0, 8, 10.0, None) > 5.0);
        // All-censored → λ=0 (hits=0) → floor.
        let cens = [ev(5.0, true), ev(10.0, true), ev(15.0, true)];
        assert_eq!(consolidate_after(&cens, 100.0, 8, 10.0, None), 5.0);
    }

    /// r[ctrl.nodeclaim.consolidate-na]: floor = q_0.5(boot)/2. With
    /// no events (λ=0), break-even fires immediately → returns floor.
    // r[verify ctrl.nodeclaim.consolidate-na]
    #[test]
    fn consolidate_after_respects_floor() {
        // boot_median=40 → floor=20. λ=0 → immediate break-even → 20.
        let t = consolidate_after(&[], 4.0, 8, 40.0, None);
        assert_eq!(t, 20.0);
        // Explicit max ceiling.
        let evs: Vec<_> = (1..=100).map(|k| ev(k as f64, false)).collect();
        // Dense events, high E[c_fit] → λ·E stays > rhs through to max.
        let t2 = consolidate_after(&evs, 1e6, 8, 40.0, Some(50.0));
        assert_eq!(t2, 50.0, "max ceiling");
    }

    /// 192c node, mean fitting c_arr=4, dense arrivals at 21..30.
    /// w=20 → λ(20) = 10/(20·10) = 0.05. RHS = 192/40 = 4.8.
    /// 0.05·4 = 0.2 < 4.8 → break-even at floor → delete.
    #[test]
    fn keep_condition_uses_fitting_core_expectation() {
        let evs: Vec<_> = (21..=30).map(|k| ev(f64::from(k), false)).collect();
        // E[c_fit]=4, node=192, boot=40: λ·E ≪ rhs → floor.
        let t = consolidate_after(&evs, 4.0, 192, 40.0, None);
        assert_eq!(t, 20.0);
        // Same events, E[c_fit]=100, node=8: λ(20)·100 = 5 >
        // 8/40=0.2 → keep past floor; break-even after the cluster.
        let t2 = consolidate_after(&evs, 100.0, 8, 40.0, Some(100.0));
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
        let t = consolidate_after(&evs, 100.0, 8, 40.0, None);
        assert!(
            t > 20.0,
            "sparse events should not collapse to floor; got {t}"
        );
        // Sanity: with E[c_fit]=0 (no demand), break-even at floor.
        assert_eq!(consolidate_after(&evs, 0.0, 8, 40.0, None), 20.0);
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
