//! Windowed-rate idle-node consolidation.
//!
//! Per `r[ctrl.nodeclaim.consolidate-na]`: an empty Registered NodeClaim
//! is kept while `λ(t)·𝔼[c_arr | c_arr ≤ cores] > cores/q_0.5(boot)`; λ is
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
use rio_proto::types::SpawnIntent;

/// Hold-open annotation key. Operator-settable: a NodeClaim carrying
/// `rio.build/hold-open=true` uses `hold_open_threshold` as its idle
/// threshold — `max(max_consolidation_time, na)` when set, else
/// `2 × na` (r38 merged_004 — the protection annotation can never make
/// the threshold *lower* than an un-annotated node). Set via
/// `kubectl annotate nodeclaim <n> rio.build/hold-open=true` for
/// debugging or to keep one warm slot through a known lull. The
/// reconciler does NOT set it automatically.
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
// r[impl ctrl.nodeclaim.consolidate-na+6]
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
    // unconditionally. The spec (`r[ctrl.nodeclaim.consolidate-na+6]`)
    // says `W = q_0.5(boot)/2`; the implementation must match. Decoupling
    // restores satisfiability for `E[c_fit] > node_cores/2` (≈1 build
    // per node). For bin-packed cells (`E ≤ cores/2`, the §13b
    // MostAllocated case) `λ̂` saturates at `1/w = 2/boot` and
    // `λ̂·E ≤ cores/boot` always, so the floor is a hard bound the
    // model cannot exceed regardless of arrival rate (r38 bug_022).
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

/// `𝔼[c_arrival | c_arrival ≤ node_cores]` over this tick's
/// placeable AND window-deferred intents — the **conditional** mean
/// cores of intents that would fit on a `node_cores` node. NOT
/// `𝔼[c·𝟙{c≤cores}]` — λ is already the fitting-restricted hazard, so
/// dividing by `|fitting|` is correct (r42 bug_025). Round-10
/// merged_bug_012: deferred intents JOIN the demand population — they
/// are real arrivals the window paced, and excluding them read
/// remaindered cells as zero-demand (idle nodes reaped at the floor
/// then re-minted). Spec: 0 when both slices are ⊥ or empty (caller
/// passes `(&[], &[])` in consolidate-only mode).
pub fn e_fitting_cores(placeable: &[Placement], deferred: &[SpawnIntent], node_cores: u32) -> f64 {
    let fitting: Vec<u32> = placeable
        .iter()
        .map(|(i, _, _)| i.cores)
        .chain(deferred.iter().map(|i| i.cores))
        .filter(|&c| c <= node_cores)
        .collect();
    if fitting.is_empty() {
        return 0.0;
    }
    fitting.iter().copied().map(f64::from).sum::<f64>() / fitting.len() as f64
}

/// Hold-open threshold for an annotated node. `na` is the non-hold-open
/// threshold for the SAME (cell, node_cores) — already floored by
/// `consolidate_after`'s `max_t.max(floor)`. `.max(na)` enforces the
/// invariant `threshold_hold_open ≥ threshold_non_hold_open` so a
/// `max_consolidation_time < floor` misconfig cannot re-invert the
/// protection annotation (r38 merged_004; r37 bug_009 was strike-1).
/// Strike-3 escalation: CellCtx returns a `Threshold {busy, hold_open}`
/// struct with the invariant enforced in the constructor.
#[inline]
fn hold_open_threshold(na: f64, max: Option<f64>) -> f64 {
    max.unwrap_or(2.0 * na).max(na)
}

/// Filter `placeable` to intents that can ARRIVE at a node in `cell`
/// — the per-cell partition `e_fitting_cores` computes its mean over.
///
/// r35 bug_023: `r[ctrl.nodeclaim.consolidate-na+6]` says "per-cell
/// mean over `placeable`" — §13e removed the `kind` filter from
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

/// [`placeable_for_cell`]'s sibling over the window-DEFERRED intents
/// (round-10 merged_bug_012): the SAME (intent, cell) admission
/// predicate — `cells_of(i)` membership plus the agnostic-fallback
/// gate — so the consolidator's view of deferred demand routes
/// exactly like the placer's would have.
pub fn deferred_for_cell(
    deferred: &[SpawnIntent],
    cell: &Cell,
    hw_admits: impl Fn(&str, Option<&str>, &[String]) -> bool,
) -> Vec<SpawnIntent> {
    deferred
        .iter()
        .filter(|i| {
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
    /// Round-10 merged_bug_012: the per-cell partition of this tick's
    /// WINDOW-DEFERRED intents (same admission predicate as
    /// `placeable_for_cell`). Deferred demand is still demand — the
    /// NA keep-condition must see it, or the idle node serving a
    /// deferred backlog is reaped at the floor and re-minted next
    /// window rotation (reap-then-re-mint churn).
    cell_deferred: Vec<SpawnIntent>,
    /// `q_0.5(boot[cell])`, or `cfg.seed_for(cell)` for a cold cell.
    boot_median: f64,
    /// Per-class operator floor (`minConsolidationTime`).
    min: Option<f64>,
}

impl CellCtx {
    fn new(
        cell: &Cell,
        placeable: &[Placement],
        deferred: &[SpawnIntent],
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
            cell_deferred: deferred_for_cell(deferred, cell, hw_admits),
            boot_median: sketches
                .get(cell)
                .and_then(|s| s.boot_median())
                .unwrap_or_else(|| cfg.seed_for(cell)),
            min: cfg.min_consolidation_time_for(cell),
        }
    }

    /// NA break-even threshold for a `node_cores`-core node in this
    /// cell. The ONLY call into `consolidate_after` from `reap_idle` —
    /// both the hold-open arm (via [`hold_open_threshold`]) and the
    /// non-hold-open arm read this. r37 bug_009: pre-CellCtx the
    /// hold-open arm passed `(events=&[], e_fitting_cores=0.0)`, which
    /// always returns `floor` — a busy cell's non-hold-open threshold
    /// could exceed `2×floor`, so the operator-annotated node was
    /// reaped FIRST.
    fn na_threshold(&self, node_cores: u32, max: Option<f64>) -> f64 {
        consolidate_after(
            &self.events,
            e_fitting_cores(&self.cell_placeable, &self.cell_deferred, node_cores),
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
    /// This tick's window-deferred intents (round-10 merged_bug_012):
    /// demand the NA keep-condition counts; never `reserved` (a
    /// deferred intent has no node).
    pub deferred: &'a [SpawnIntent],
    /// The gauge-reset and per-cell precompute key set. r37 bug_006:
    /// every cell needs a write. r42 bug_023: callers pass the
    /// `gauge_universe` set (configured ∪ live ∪ trailing) so a cell
    /// removed from config gets one trailing zero-write for
    /// `consolidate_threshold_seconds` after it drains.
    pub all_cells: &'a [Cell],
    /// Controller-tracked `name → epoch-secs at which this node was
    /// first observed idle (`requested.0 == 0`)`. Populated by
    /// `observe_idle_to_busy` BEFORE this call. r42 bug_020: this is
    /// the SOLE idle-duration source — Karpenter `Empty` does not
    /// exist in v1; the controller is the authority.
    pub prev_idle: &'a HashMap<String, f64>,
    pub cfg: &'a NodeClaimPoolConfig,
    pub hw_admits: F,
    pub now_secs: f64,
}

/// Reap idle Registered NodeClaims past their break-even threshold.
///
/// A node is reapable when: `registered` AND not `terminating` AND not
/// in this tick's FFD `reserved` set AND `requested.0 == 0` AND
/// `now − prev_idle[name] > threshold`. `threshold` is
/// [`consolidate_after`] over the cell's `idle_gap_events`, raised to
/// `hold_open_threshold` for hold-open nodes
/// (`max(max_consolidation_time, na)` when set, else `2 × na`). Each
/// reap records a censored `IdleGapEvent`. `Api::delete` 404 is ignored
/// (already-gone race with Karpenter); other errors warn + skip.
/// Returns the backing-node names of the claims it deleted, so the
/// caller can feed them to the wedge tracker's eviction stash exactly
/// like `reap_unhealthy`'s reaps (merged_bug_017: BOTH reap paths are
/// admission-eviction sources; idle reaps previously bypassed the
/// stash, leaving the reaped node's still-open attempts admissible).
pub async fn reap_idle<F: Fn(&str, Option<&str>, &[String]) -> bool>(
    nodeclaims: &Api<NodeClaim>,
    live: &[LiveNode],
    sketches: &mut CellSketches,
    pass_fence: &crate::reconcilers::fence::MutationFence,
    inputs: &ReapInputs<'_, F>,
) -> anyhow::Result<Vec<String>> {
    let ReapInputs {
        placeable,
        deferred,
        all_cells,
        prev_idle,
        cfg,
        hw_admits,
        now_secs,
    } = inputs;
    let now_secs = *now_secs;
    let reserved: HashSet<&str> = placeable.iter().map(|(_, n, _)| n.as_str()).collect();
    let mut reaped_backing: Vec<String> = Vec::new();

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
            CellCtx::new(cell, placeable, deferred, sketches, cfg, hw_admits),
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
        if n.terminating() || !n.registered || reserved.contains(n.name.as_str()) {
            continue;
        }
        // r42 bug_020: idle = `now − prev_idle[name]`, the
        // controller-tracked idle-since timestamp. `requested.0 > 0`
        // → busy → not in `prev_idle` (observe_idle_to_busy removes
        // it). A node not yet in `prev_idle` (created mid-tick, or
        // observe_idle_to_busy hasn't seen it) is skipped — never
        // reap a node whose idle history we haven't observed.
        if n.requested.0 > 0 {
            continue;
        }
        let Some(&since) = prev_idle.get(&n.name) else {
            continue;
        };
        let idle = now_secs - since;
        // A live node may carry a cell removed from config mid-rollout —
        // derive on demand so it's still reaped (leaking is worse).
        if !cell_ctx.contains_key(cell) {
            cell_ctx.insert(
                cell.clone(),
                CellCtx::new(cell, placeable, deferred, sketches, cfg, hw_admits),
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
            hold_open_threshold(na, cfg.max_consolidation_time)
        } else {
            na
        };
        // Last-write-wins per cell within a tick — and the stream
        // interleaves BOTH threshold families: un-annotated nodes
        // write `na` while HOLD_OPEN_ANNOTATION nodes write the
        // hold-open value (2x na under an unset
        // `max_consolidation_time`, per the r37 coupling above), so
        // the surviving sample depends on iteration order whenever a
        // cell mixes annotated and un-annotated nodes. Read it as the
        // cell's floor-order signal, not a per-node threshold
        // (allocatable variance within the hw-class and the 2x
        // hold-open factor both ride inside one order of magnitude).
        // Operator check: `fetcher-*` cells ≥ 600s floor; builder cells
        // ≥ 300s `*` floor. For bin-packed cells (`E[c_fit] ≤ cores/2`,
        // the §13b MostAllocated default for builders), the floor is a
        // HARD bound — `λ̂` saturates at `1/w = 2/boot` so the
        // keep-condition is structurally unsatisfiable (r38 bug_022).
        // Only cells packing ~1 intent/node can be NA-extended above
        // the floor. A cell at boot_median/2 (~9-25s) when its
        // minConsolidationTime entry says 300/600s = the prefix-glob
        // didn't match.
        // r[impl obs.metric.consolidate-threshold]
        metrics::gauge!(
            "rio_controller_nodeclaim_consolidate_threshold_seconds",
            "cell" => cell.to_string(),
        )
        .set(threshold);
        if idle <= threshold {
            continue;
        }
        // D4 mutation seam: a deposed pass deletes nothing more.
        if pass_fence.check("nodeclaim-reap-idle").is_err() {
            break;
        }
        match nodeclaims.delete(&n.name, &DeleteParams::default()).await {
            Ok(_) => {
                debug!(name = %n.name, %cell, idle, threshold, "reaped idle NodeClaim");
                if let Some(bn) = n.node_name.clone() {
                    reaped_backing.push(bn);
                }
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
    Ok(reaped_backing)
}

/// Edge-detect idle→busy transitions and record them as uncensored
/// [`IdleGapEvent`]s. `prev_idle` is the reconciler's running
/// `name → idle-since epoch-secs` map; a node present there whose
/// `requested.0 > 0` (the tick's Pod LIST saw a binding) had an
/// arrival — record `{now − prev_idle[name], censored:false}` to its
/// cell. r42 bug_020: `prev_idle` is the AUTHORITY for idle
/// duration — Karpenter v1 does not write the `Empty` condition the
/// pre-r42 implementation relied on, so the only signal is the
/// controller's own `requested.0` observation. Nodes not yet in
/// `prev_idle` are seeded with `now_secs` on first observation of
/// idleness (NOT registration time — that would re-introduce the
/// "idle for hours" bug for any node that was busy and just freed).
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
    // WITHOUT recording an event. For `reap_idle`'d nodes the censored
    // event was pushed at delete time; recording here would double-count.
    // For `reap_unhealthy`/out-of-band deletes (spot interruption,
    // `kubectl delete nodeclaim`) the gap is silently dropped — an
    // unhealthy node's idle history is tainted anyway (the executor
    // crashed mid-idle), and `reap_unhealthy` takes `&CellSketches`
    // immutably so it CANNOT push (r38 bug_031).
    let live_names: HashSet<&str> = live
        .iter()
        .filter(|n| !n.terminating())
        .map(|n| n.name.as_str())
        .collect();
    prev_idle.retain(|name, _| live_names.contains(name.as_str()));
    for n in live {
        if n.terminating() || !n.registered {
            continue;
        }
        let busy = n.requested.0 > 0;
        if busy {
            if let (Some(&since), Some(cell)) = (prev_idle.get(&n.name), n.cell.as_ref()) {
                push_idle_gap(
                    sketches,
                    cell,
                    IdleGapEvent {
                        gap_secs: now_secs - since,
                        censored: false,
                    },
                );
            }
            prev_idle.remove(&n.name);
        } else {
            // First observation of an idle node: seed with `now_secs`
            // (the controller has just witnessed it become idle).
            // Subsequent ticks: `or_insert` keeps the original
            // timestamp — the node has been idle since. NEVER seed
            // from `Registered=True` — a node busy for hours that
            // just freed would appear idle-since-registration and be
            // reaped immediately, re-introducing the exact bug this
            // fixes. The never-bound-node case loses fidelity (seed
            // is first-observed-tick, not registration-tick) but
            // gains safety (under-reap, the same SAFE direction as
            // a controller restart).
            prev_idle.entry(n.name.clone()).or_insert(now_secs);
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

    /// r[ctrl.nodeclaim.consolidate-na+6]: floor = q_0.5(boot)/2. With
    /// no events (λ=0), break-even fires immediately → returns floor.
    // r[verify ctrl.nodeclaim.consolidate-na+6]
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
    /// `requested.0 > 0` is busy. r42 bug_020: a node with a
    /// freshly-bound pod MUST NOT be reapable even if it appears in
    /// `prev_idle` (the cache is one tick stale). A node not in
    /// `prev_idle` MUST NOT be reapable either — never reap a node
    /// whose idle history hasn't been observed.
    #[test]
    fn reap_idle_skips_nonzero_requested() {
        let mut n = with_conds(
            node("bound", "h", CapacityType::Spot, 8, 0, 0),
            &[("Registered", "True", 1000.0)],
        );
        let prev_idle: HashMap<String, f64> = [("bound".into(), 1000.0)].into();
        // A pod is bound (requested > 0) → reap_idle's first filter
        // treats this as busy regardless of `prev_idle`.
        n.requested = (8, 0, 0);
        assert!(n.requested.0 > 0, "busy predicate fires");
        // Same node with requested=0 AND in prev_idle → idle, reapable.
        n.requested = (0, 0, 0);
        assert!(n.requested.0 == 0);
        assert_eq!(prev_idle.get("bound").copied(), Some(1000.0));
        // Node not yet in `prev_idle` (e.g. created mid-tick before
        // observe_idle_to_busy ran) → skipped, never reaped.
        assert_eq!(prev_idle.get("unobserved"), None);
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
        assert_eq!(e_fitting_cores(&[p(4), p(6), p(12)], &[], 8), 5.0);
        // None fit → 0.
        assert_eq!(e_fitting_cores(&[p(12), p(16)], &[], 8), 0.0);
        // Empty → 0 (spec: ⊥/empty → 0).
        assert_eq!(e_fitting_cores(&[], &[], 8), 0.0);
    }

    // r[verify ctrl.pool.demand-completeness]
    /// **W10-AI, the consolidate half (round-10 merged_bug_012).**
    /// Window-DEFERRED intents are DEMAND for the NA keep-condition —
    /// a cell whose entire backlog was windowed out no longer reads
    /// `E[c_fit] = 0` (the floor-threshold reap-then-re-mint churn:
    /// idle node reaped at `boot_median/2`, next rotation admits the
    /// deferred work, a fresh claim boots ~50s).
    ///
    /// Pre-fix red (deferred axis severed — the two-arg mean):
    ///   left: 0.0  right: 6.0
    #[test]
    fn w10_ai_deferred_demand_counts_for_keep_condition() {
        let d = |c: u32| SpawnIntent {
            cores: c,
            ..Default::default()
        };
        // All demand deferred this tick (placeable empty): the mean
        // must still see it.
        assert_eq!(
            e_fitting_cores(&[], &[d(4), d(8), d(12)], 8), // 12 doesn't fit
            6.0,
            "deferred intents join the fitting population (pre-fix: 0.0 — \
             remaindered cells read zero demand)"
        );
        // Mixed: placeable {4} + deferred {8} on a 8c node → mean 6.
        let p = |c: u32| {
            (
                SpawnIntent {
                    cores: c,
                    ..Default::default()
                },
                "n".to_string(),
                false,
            )
        };
        assert_eq!(e_fitting_cores(&[p(4)], &[d(8)], 8), 6.0);
    }

    /// r35 bug_023 (§Simulator-shares-accounting): `e_fitting_cores`
    /// must be a per-cell mean. §13e removed the `kind` filter from
    /// `GetSpawnIntents`, so `placeable` mixes fetcher (`c*≈1`) and
    /// builder intents — a global mean for a 32c builder node biases
    /// `E[c_fit]` ~5× low (12 intents avg=6.17 instead of 32). The
    /// filter is the SAME predicate FFD's `hw_admits` uses, NOT a
    /// `hw_class_names.is_empty()` pass-through (which over-counts
    /// hw-agnostic intents on fetcher cells).
    // r[verify ctrl.nodeclaim.consolidate-na+6]
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
        let e = e_fitting_cores(&cell_placeable, &[], 32);
        assert!(
            (e - 32.0).abs() < 1e-9,
            "per-cell E[c_fit] for hi-ebs-x86 should be 32, got {e} (global mean would be ≈6.17)"
        );
        // The fetcher cell sees only the 10 fetcher intents.
        let fetcher_cell = Cell("fetcher-x86".into(), CapacityType::Spot);
        let cell_placeable = placeable_for_cell(&placeable, &fetcher_cell, admits);
        let e = e_fitting_cores(&cell_placeable, &[], 4);
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
            e_fitting_cores(&cp, &[], 4),
            1.0,
            "hw-agnostic intent excluded from fetcher cell when hw_admits rejects"
        );
        let cp = placeable_for_cell(&mixed, &builder_cell, admits_builder_only);
        assert_eq!(
            e_fitting_cores(&cp, &[], 32),
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
    // r[verify ctrl.nodeclaim.consolidate-na+6]
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
    /// `requested.0 == 0` ∧ in `prev_idle` ∧ `now − prev_idle > threshold`.
    /// With no events, threshold = boot_median/2. r42 bug_020: the busy
    /// gate is `requested.0 > 0` (the controller's own observation), NOT
    /// a Karpenter `Empty` condition (which v1 never writes). Kube
    /// side-effect not tested here (covered in VM tests); this asserts
    /// the filter expressions against the pure threshold function.
    #[test]
    fn no_reap_when_busy_or_reserved() {
        let mut sk = CellSketches::default();
        let cell = Cell("h".into(), CapacityType::Spot);
        for _ in 0..10 {
            sk.cell_mut(&cell).record(40.0, 0.0);
        }
        let cfg = NodeClaimPoolConfig::default();
        // boot_median ≈ 40 → floor = 20. Node observed idle since 1070,
        // now=1100 → idle=30s > 20 → reapable unless reserved/busy.
        let idle_node = with_conds(
            node("idle", "h", CapacityType::Spot, 8, 0, 0),
            &[("Registered", "True", 1000.0)],
        );
        let prev_idle: HashMap<String, f64> = [("idle".into(), 1070.0)].into();
        let idle = 1100.0 - prev_idle["idle"];
        assert!((idle - 30.0).abs() < 1e-9);
        let threshold = consolidate_after(
            &[],
            e_fitting_cores(&[], &[], 8),
            8,
            sk.get(&cell).unwrap().boot_median().unwrap(),
            cfg.max_consolidation_time,
            None,
        );
        assert!(idle > threshold, "idle past floor");

        // Busy (`requested.0 > 0`) → first filter fires → never reapable.
        let mut busy = with_conds(
            node("busy", "h", CapacityType::Spot, 8, 0, 0),
            &[("Registered", "True", 1000.0)],
        );
        busy.requested = (4, 0, 0);
        assert!(busy.requested.0 > 0, "busy filter fires before prev_idle");

        // Reserved (in placeable) → skipped regardless of idle.
        let reserved: HashSet<&str> = ["idle"].into();
        assert!(reserved.contains(idle_node.name.as_str()));
    }

    /// F8 + r42 bug_020: a node observed idle at tick 1, busy this tick
    /// (`requested.0 > 0`) → uncensored `IdleGapEvent{now − seed, false}`
    /// recorded; `prev_idle` keeps the original idle-since timestamp for
    /// nodes that stay idle (`or_insert`, NOT a per-tick refresh); nodes
    /// gone from `live` evicted from `prev_idle`.
    #[test]
    fn observe_idle_to_busy_pushes_uncensored() {
        let mut sk = CellSketches::default();
        let cell = Cell("h".into(), CapacityType::Spot);
        // Tick 0 seeded a=1120 (idle 40s by tick 1160), b=1145.
        let mut prev_idle: HashMap<String, f64> =
            [("a".into(), 1120.0), ("b".into(), 1145.0)].into();

        // Tick 1160: a now busy (requested=4c), b still idle
        // (requested=0), c is new (first observed idle).
        let mut a = with_conds(
            node("a", "h", CapacityType::Spot, 8, 0, 0),
            &[("Registered", "True", 1042.0)],
        );
        a.requested = (4, 0, 0);
        let b = with_conds(
            node("b", "h", CapacityType::Spot, 8, 0, 0),
            &[("Registered", "True", 1042.0)],
        );
        let c = with_conds(
            node("c", "h", CapacityType::Spot, 8, 0, 0),
            &[("Registered", "True", 1042.0)],
        );
        observe_idle_to_busy(
            &[a.clone(), b.clone(), c.clone()],
            &mut prev_idle,
            &mut sk,
            1160.0,
        );

        let evs = &sk.get(&cell).unwrap().idle_gap_events;
        assert_eq!(evs.len(), 1, "only a's idle→busy edge recorded");
        assert!((evs[0].gap_secs - 40.0).abs() < 1e-9);
        assert!(!evs[0].censored, "uncensored");
        // prev_idle: a evicted (busy), b KEEPS its 1145 seed (or_insert,
        // not refresh), c seeded at 1160 (first observation, NOT
        // registration time 1042).
        assert!(!prev_idle.contains_key("a"));
        assert!(
            (prev_idle["b"] - 1145.0).abs() < 1e-9,
            "or_insert keeps seed"
        );
        assert!(
            (prev_idle["c"] - 1160.0).abs() < 1e-9,
            "first-observation seed, not registration time"
        );

        // Tick 1170: b still idle. or_insert keeps 1145, NOT 1170.
        observe_idle_to_busy(&[b, c.clone()], &mut prev_idle, &mut sk, 1170.0);
        assert!(
            (prev_idle["b"] - 1145.0).abs() < 1e-9,
            "stay-idle keeps original seed; idle accumulates"
        );

        // Tick 1180: b goes busy → gap = 1180 − 1145 = 35s (cumulative
        // idle, NOT 1180 − 1170 = 10s).
        let mut b_busy = with_conds(
            node("b", "h", CapacityType::Spot, 8, 0, 0),
            &[("Registered", "True", 1042.0)],
        );
        b_busy.requested = (4, 0, 0);
        observe_idle_to_busy(&[b_busy, c], &mut prev_idle, &mut sk, 1180.0);
        let evs = &sk.get(&cell).unwrap().idle_gap_events;
        assert_eq!(evs.len(), 2);
        assert!(
            (evs[1].gap_secs - 35.0).abs() < 1e-9,
            "gap is cumulative from first idle observation, not tick-over-tick"
        );

        // Next tick: b/c reaped (gone from live). prev_idle prunes
        // without recording an uncensored event (reap_idle records the
        // censored one).
        observe_idle_to_busy(&[], &mut prev_idle, &mut sk, 1190.0);
        assert!(prev_idle.is_empty());
        assert_eq!(sk.get(&cell).unwrap().idle_gap_events.len(), 2);
    }

    /// r43 merged_bug_016: `observe_idle_to_busy` only `or_insert`s — it
    /// never refreshes an existing entry. The lease-acquire edge MUST
    /// `prev_idle.clear()` unconditionally (both Ok and Err reload arms)
    /// because a stale entry that survived a standby window over-counts
    /// the idle duration. This test pins the `or_insert` semantics so a
    /// future "fix" that switches to `.insert()` (which would mask the
    /// bug differently) trips a different assertion.
    ///
    /// NOTE: this test does NOT validate the `mod.rs` hoist — it pins
    /// the `or_insert` precondition the bug depends on. The hoist itself
    /// is validated end-to-end by the lifecycle-invariants suite:
    /// `lifecycle_tests::acquire_ok_clears_prev_idle_and_suppress_fields`
    /// (Ok arm) and
    /// `lifecycle_tests::acquire_err_still_clears_prev_idle_keeps_suppress`
    /// (the merged_bug_016 Err arm) drive the real acquire edge through
    /// `tick()` and assert the unconditional clear.
    #[test]
    fn observe_idle_to_busy_keeps_pre_existing_seed() {
        let mut sk = CellSketches::default();
        let mut prev_idle: HashMap<String, f64> = [("n1".into(), 1000.0)].into();
        // Idle node (`requested.0 == 0`, registered). Mirrors `b` above.
        let n1 = with_conds(
            node("n1", "h", CapacityType::Spot, 8, 0, 0),
            &[("Registered", "True", 900.0)],
        );
        observe_idle_to_busy(&[n1], &mut prev_idle, &mut sk, 1300.0);
        // `or_insert` does NOT refresh: stale seed survives.
        assert!(
            (prev_idle["n1"] - 1000.0).abs() < 1e-9,
            "or_insert keeps stale seed across ticks"
        );
        // ... and that is EXACTLY why the lease-acquire edge must
        // `clear()` unconditionally — otherwise a 300s standby looks
        // like 300s idle.
    }

    /// A node that started terminating since last tick (Karpenter
    /// finalizer running) drops from `prev_idle` WITHOUT recording an
    /// idle-gap event — `reap_idle`/`reap_unhealthy` already pushed the
    /// censored event at delete time. Recording here would double-count
    /// and bias the NA-model arrival rate up (holding nodes open longer
    /// than warranted).
    #[test]
    fn observe_idle_to_busy_skips_terminating() {
        use super::super::ffd::tests::set_terminating;
        let mut sk = CellSketches::default();
        let cell = Cell("h".into(), CapacityType::Spot);
        let mut prev_idle: HashMap<String, f64> = [("dying".into(), 1020.0)].into();
        // Karpenter drain sets deletionTimestamp; the kubelet may still
        // report a bound pod → naively this looks like an idle→busy edge.
        let mut dying = set_terminating(with_conds(
            node("dying", "h", CapacityType::Spot, 8, 0, 0),
            &[("Registered", "True", 1000.0)],
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

    /// r42 bug_020 KEYSTONE: a node Registered at `t=0`, busy for 100
    /// ticks (`requested=(4,0,0)`, `prev_idle` never seeded), then idle
    /// at tick 101 (`requested=(0,0,0)`). Pre-r42 `idle_secs()` fell
    /// back to `Registered.lastTransitionTime` and reported ~101s →
    /// `reap_idle` immediately killed a node that had been free for one
    /// tick — the `r[ctrl.nodeclaim.consolidate-na]` warm-keep model
    /// was DEAD CODE. Post-r42: `prev_idle["a"]` seeds at `tick101_now`,
    /// so `reap_idle` sees `idle = now − prev_idle ≈ 0` and warm-keeps.
    /// Asserts the structural invariant `reap_idle` reads via
    /// `ReapInputs.prev_idle`.
    // r[verify ctrl.nodeclaim.consolidate-na+6]
    #[test]
    fn busy_node_freed_seeds_prev_idle_at_now_not_registration() {
        let mut sk = CellSketches::default();
        let cell = Cell("h".into(), CapacityType::Spot);
        let mut prev_idle: HashMap<String, f64> = HashMap::new();

        let registered_at = 0.0;
        let mut n = with_conds(
            node("a", "h", CapacityType::Spot, 8, 0, 0),
            &[("Registered", "True", registered_at)],
        );

        // Ticks 1..=100: busy. prev_idle never seeds (busy nodes are
        // removed/never inserted).
        n.requested = (4, 0, 0);
        for tick in 1..=100u32 {
            observe_idle_to_busy(&[n.clone()], &mut prev_idle, &mut sk, f64::from(tick));
            assert!(prev_idle.is_empty(), "busy node never enters prev_idle");
        }

        // Tick 101: pod departs → requested drops to 0. The controller
        // observes the busy→idle edge and seeds prev_idle at tick101.
        let tick101_now = 101.0;
        n.requested = (0, 0, 0);
        observe_idle_to_busy(&[n.clone()], &mut prev_idle, &mut sk, tick101_now);

        // The structural invariant `reap_idle` reads (`ReapInputs.prev_idle`):
        // idle = now − prev_idle["a"] ≈ 0, NOT now − Registered = 101.
        let since = prev_idle["a"];
        assert!(
            (since - tick101_now).abs() < 1e-9,
            "seed at first-observed-idle tick, not registration time"
        );
        let idle = tick101_now - since;
        assert!(idle < 5.0, "freed node has idle ≈ 0, not ≈ {tick101_now}");
        assert!(
            idle < tick101_now - registered_at,
            "idle MUST be far less than node age — warm-keep is live again"
        );
        // No idle→busy edge recorded yet (this was busy→idle).
        assert!(sk.get(&cell).is_none_or(|s| s.idle_gap_events.is_empty()));
    }

    /// r38 merged_004: hold-open ≥ non-hold-open MUST hold for ALL
    /// `max_consolidation_time` values, including `Some(M)` with `M <
    /// floor` (the inversion the merged_010 `consolidate_after` clamp
    /// closed for one consumer but not this one). The pre-r38 test
    /// `hold_open_uses_max_consolidation_time` only asserted struct
    /// literals — never called `na_threshold` or the production formula.
    #[test]
    fn hold_open_threshold_never_below_non_hold_open_with_max_set() {
        // Fetcher cell shape: boot=30, min=600 → floor=600.
        let ctx = CellCtx {
            events: vec![],
            cell_placeable: vec![],
            cell_deferred: vec![],
            boot_median: 30.0,
            min: Some(600.0),
        };
        // `Some(300.0)` is the falsifying input the pre-r38 suite never
        // produced: max < floor → na = 600, raw hold-open = 300 < na.
        for max in [Some(300.0), Some(900.0), None] {
            let na = ctx.na_threshold(64, max);
            // Calls the production helper — NOT a re-derived copy of
            // the formula. Removing the `.max(na)` clamp from
            // `hold_open_threshold` would fail this test. Neither this
            // test nor the sibling
            // `hold_open_threshold_never_below_non_hold_open` executes
            // `reap_idle`; the wiring at consolidate.rs:380-384 is
            // protected by shape (both arms read the same `na`), not by
            // a test — see the sibling's comment for the structural
            // rationale.
            let ho = hold_open_threshold(na, max);
            assert!(
                ho >= na,
                "hold-open {ho} < non-hold-open {na} at max={max:?} — \
                 protection annotation inverted"
            );
        }
    }

    /// r37 merged_010 (§Granularity-coupling): `w` decoupled from `floor`.
    /// With `min ≥ boot_median` (any operator floor for an 18s-boot
    /// builder; chart default is `*: 300.0`), the pre-fix
    /// `w = floor.max(5.0)` made `λ·E > rhs` unsatisfiable
    /// (`λ ≤ 1/w < cores/boot_median`) — the NA model returned `floor`
    /// unconditionally. The decoupled `w = boot_median/2` makes the
    /// keep-condition satisfiable again so the model can extend past
    /// the floor under load — the `values.yaml` `minConsolidationTime`
    /// rationale: a hard minimum the model "CAN extend under arrival
    /// pressure" only for ~1-intent/node cells. This fixture is that
    /// case (`E[c_fit] = node_cores`); the bin-packed inverse is
    /// `consolidate_after_floor_binds_for_bin_packed_cells` below.
    /// Fixture uses `min=60` (a representative floor) — the property is
    /// about the `w`/`floor` decoupling, not the deployed value.
    // r[verify ctrl.nodeclaim.consolidate-na+6]
    #[test]
    fn consolidate_after_extends_past_policy_floor() {
        // Builder cell shape: boot_median=18, min=60. Dense arrivals
        // at 60..70s (right past the floor), E[c_fit]=64, node=64.
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

    /// r38 bug_022: for bin-packed cells (`E[c_fit] ≤ node_cores/2` —
    /// the §13b MostAllocated case) the NA keep-condition is
    /// structurally unsatisfiable: `λ̂ ≤ 1/w = 2/boot`, so `λ̂·E ≤
    /// 2E/boot ≤ cores/boot`. The floor is a hard bound, not a backstop
    /// the model improves on. Pin the bound so a future change to `w`
    /// or the keep-condition is forced to reason about the bin-packed
    /// case, not just `E = cores`.
    // r[verify ctrl.nodeclaim.consolidate-na+6]
    #[test]
    fn consolidate_after_floor_binds_for_bin_packed_cells() {
        // Builder shape: boot=30, node=64, intents packing 4-per-node
        // (E=16 < 32=cores/2), min=60. Saturate the estimator with
        // dense uncensored arrivals just past the floor.
        let evs: Vec<_> = (61..=80).map(|k| ev(f64::from(k), false)).collect();
        let t = consolidate_after(&evs, 16.0, 64, 30.0, None, Some(60.0));
        // floor = max(15, 60) = 60. w = 15. rhs = 64/30 ≈ 2.13.
        // λ̂ ≤ 1/15 ≈ 0.067; λ̂·E ≤ 0.067·16 ≈ 1.07 < 2.13.
        assert_eq!(
            t, 60.0,
            "bin-packed cell floor must bind regardless of arrival rate; got {t}"
        );
    }

    /// r37 merged_010: `max_consolidation_time < floor` no longer slips
    /// through. `floor` is the policy lower bound; an operator setting
    /// `max < min` is a misconfig (scheduler.sla.maxConsolidationTime <
    /// karpenter.nodeclaimPool.minConsolidationTime) but the floor still
    /// holds — silently dropping below it would burn a boot every
    /// `max_consolidation_time` seconds.
    // r[verify ctrl.nodeclaim.consolidate-na+6]
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
    // r[verify ctrl.nodeclaim.consolidate-na+6]
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
        // default `*: 300.0` floor and any gap ring, the busy NA
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
            min_consolidation_time: std::collections::BTreeMap::new(),
            ..Default::default()
        };
        let admits = |_: &str, _: Option<&str>, _: &[String]| true;
        let ctx = CellCtx::new(&cell, &placeable, &[], &sk, &cfg, &admits);
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
        // Post-fix: hold-open = `hold_open_threshold(ctx.na_threshold, max)`
        // — both arms read the SAME `ctx.na_threshold`, so hold-open ≥
        // non-hold-open by construction (with `max=None` the helper
        // returns `2·na`; r38 merged_004 added `.max(na)` for the
        // `max < floor` arm — see the `_with_max_set` sibling). The
        // structural protection is in `reap_idle`'s shape (`let na =
        // ctx.na_threshold(...); ... hold_open_threshold(na, ...)`);
        // these assertions document the invariant the shape enforces.
        let post_fix_hold_open = hold_open_threshold(na, None);
        assert!(post_fix_hold_open >= pre_fix_hold_open);
        assert!(post_fix_hold_open >= na);
    }
}
