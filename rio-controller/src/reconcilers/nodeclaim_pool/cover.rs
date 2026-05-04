//! §13b deficit cover.
//!
//! Per `r[ctrl.nodeclaim.anchor-bulk+4]`: for each `(h,cap)` cell with
//! unplaced demand, mint `n` uniform NodeClaims sized so the production
//! FFD packs every fitting intent (over-cap dropped with
//! `exceeds_cell_cap` metric — see [`sizing`]), capped at
//! `sla.maxNodeClaimsPerCellPerTick` and the `sla.maxFleetCores`
//! budget. Karpenter resolves each claim's `resources.requests`
//! against the hw-class's `requirements` (instance-type properties) to
//! pick the actual instance type. Cells iterate round-robin from a
//! rotating start so no cell starves under sustained pressure.
//!
//! This module is the pure-compute half: cell assignment, claim-count
//! math, and NodeClaim spec construction. The `Api::create` side-effect
//! lives in [`super::NodeClaimPoolReconciler::cover_deficit`].

use std::collections::{BTreeMap, HashSet};

use k8s_openapi::api::core::v1::{ResourceRequirements, Taint};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::api::ObjectMeta;
use rio_crds::karpenter::{NodeClaim, NodeClaimSpec, NodeClassRef, NodeSelectorRequirementWithMin};
use rio_proto::types::{NodeSelectorRequirement, SpawnIntent};

use super::ffd::{CAPACITY_TYPE_LABEL, HW_CLASS_LABEL, a_open};
use super::sketch::{CapacityType, Cell, CellSketches};

/// `karpenter.sh/nodepool` label key. Karpenter's state-tracking
/// (drift/consolidation lookups) requires every NodeClaim to name a
/// NodePool — even though §13b's claims are rio-created, not pool-
/// provisioned. The shim pool (`limits:{cpu:0}`, `budgets:[{nodes:0}]`)
/// satisfies the lookup without ever provisioning or disrupting.
pub const NODEPOOL_LABEL: &str = "karpenter.sh/nodepool";

/// The shim NodePool name. Helm `templates/karpenter.yaml` installs it.
pub const SHIM_NODEPOOL: &str = "rio-nodeclaim-shim";

/// `spec.nodeClassRef.group` for AWS EC2NodeClass. Karpenter v1 made
/// this required (was optional in v1beta1).
const NODE_CLASS_GROUP: &str = "karpenter.k8s.aws";
const NODE_CLASS_KIND: &str = "EC2NodeClass";

/// NodeClaim annotation carrying the per-cell `min(eta_seconds)` over
/// the deficit intents that triggered the claim — the soonest demand
/// the claim must meet. [`super::sketch::CellSketches::observe_registered`]
/// reads this on `Registered=True` so `z = boot − eta` is the real
/// z-correction, not `boot − 0`.
pub const FORECAST_ETA_ANNOTATION: &str = "rio.build/forecast-eta-secs";

/// `karpenter.k8s.aws/instance-size` requirement key — the I-205
/// metal-partition. Same list as the shim NodePool's template
/// requirement (`karpenter.metalSizes`); the operator (`In`/`NotIn`)
/// is gated on whether the hw-class's `node_class` is
/// [`METAL_NODE_CLASS`], single-sourcing the predicate with helm
/// `templates/karpenter.yaml`'s `nodePools` loop.
pub const INSTANCE_SIZE_LABEL: &str = "karpenter.k8s.aws/instance-size";

/// EC2NodeClass name that selects the BIOS AMI / metal partition. A
/// hw-class with `node_class == METAL_NODE_CLASS` gets `instance-size
/// In <metalSizes>`; every other class gets `NotIn`.
pub const METAL_NODE_CLASS: &str = "rio-metal";

/// The single builder-node taint. The band-loop NodePool template
/// stamped `rio.build/builder=true:NoSchedule` so non-builder cluster
/// pods (DaemonSets, monitoring) stay off builder nodes (ADR-019); B3
/// deleted those NodePools. Karpenter does NOT merge a shim NodePool's
/// `template.spec.taints` onto externally-created claims, so
/// [`build_nodeclaim`] sets it on `NodeClaimSpec.taints` directly.
/// Paired with helm `poolDefaults.tolerations`.
fn builder_taint() -> Taint {
    Taint {
        key: "rio.build/builder".into(),
        value: Some("true".into()),
        effect: "NoSchedule".into(),
        ..Default::default()
    }
}

/// Per-hw-class context for [`build_nodeclaim`] — everything the
/// `[sla.hw_classes.$h]` entry carries that the deleted band-loop
/// NodePool template stamped. Bundled so [`build_nodeclaim`] is a pure
/// projection of `(HwClassCtx, Cell, sizing)` with no controller-side
/// defaults to forget.
pub struct HwClassCtx {
    /// EC2NodeClass name (`rio-default` / `rio-nvme` / `rio-metal`).
    pub node_class: String,
    /// `(k, v)` Node-stamp labels (`rio.build/hw-band` etc.).
    pub labels: Vec<(String, String)>,
    /// Karpenter instance-type `spec.requirements`.
    pub requirements: Vec<NodeSelectorRequirement>,
    /// §13c: per-hw-class Node taints (chained after
    /// [`builder_taint`]). e.g. metal classes carry
    /// `rio.build/kvm=true:NoSchedule`.
    pub taints: Vec<Taint>,
}

/// Cluster-level [`build_nodeclaim`] config that doesn't vary per
/// hw-class.
pub struct CoverCfg<'a> {
    /// I-205 metal `instance-size` partition list
    /// (`karpenter.metalSizes`).
    pub metal_sizes: &'a [String],
}

/// Group `unplaced` by the cheapest cell in each intent's `A_open`.
///
/// Intents with empty `A_open` from `hw_class_names=[]` (cold-start
/// `fit=None`) are routed via `fallback(i)` — typically the
/// `referenceHwClass` cell of `intent.system`'s arch (see
/// [`super::NodeClaimPoolConfig::fallback_cell`]). Intents with empty
/// `A_open` from lead-time gating (every cell's lead-time shorter than
/// `eta_seconds`) are dropped without fallback — they're forecast-only
/// and a later tick re-evaluates. `fallback → None` (no configured cell
/// hosts that arch at that size) increments the returned dropped count;
/// caller emits
/// `rio_controller_nodeclaim_intent_dropped_total{reason=no_hosting_class}`.
///
/// `masked` cells (ICE-hit this tick or scheduler-reported
/// `ice_masked_cells`) are filtered from each intent's `A_open` BEFORE
/// the cheapest-pick — so an intent whose `A_open = [(h,spot),(h,od)]`
/// fails over to `(h,od)` when spot is ICE-masked instead of being
/// assigned to a cell `cover_deficit` then skips.
///
/// `BTreeMap` so iteration order is deterministic (round-robin
/// rotation acts on a sorted universe; flapping order would defeat the
/// no-starvation guarantee).
pub fn assign_to_cells<'a>(
    unplaced: &'a [SpawnIntent],
    sketches: &CellSketches,
    masked: &HashSet<Cell>,
    cell_price: impl Fn(&Cell) -> f64,
    fallback: impl Fn(&SpawnIntent) -> Option<Cell>,
) -> (BTreeMap<Cell, Vec<&'a SpawnIntent>>, u64) {
    let mut by_cell: BTreeMap<Cell, Vec<&SpawnIntent>> = BTreeMap::new();
    let mut dropped = 0u64;
    for i in unplaced {
        let open = a_open(i, sketches);
        let cheapest = open
            .into_iter()
            .filter(|c| !masked.contains(c))
            .min_by(|a, b| cell_price(a).total_cmp(&cell_price(b)))
            .or_else(|| {
                if !i.hw_class_names.is_empty() {
                    // Non-empty hw_class_names + empty A_open ⇔
                    // forecast intent lead-time-gated on every cell.
                    // No fallback — next tick re-evaluates.
                    return None;
                }
                // Fallback is filtered through `masked` too: a masked
                // `(referenceHwClass, spot)` would land in `by_cell`
                // and then be `continue`d by the per-cell ICE skip —
                // silently stranding cold-start probes.
                let c = fallback(i).filter(|c| !masked.contains(c));
                if c.is_none() {
                    dropped += 1;
                }
                c
            });
        let Some(cheapest) = cheapest else { continue };
        by_cell.entry(cheapest).or_default().push(i);
    }
    (by_cell, dropped)
}

/// Cell ranking for [`assign_to_cells`]' cheapest-open pick: spot
/// before on-demand (always cheaper), then alphabetical hw-class
/// (stable tie-break). The scheduler's per-intent `dispatched_cells`
/// already encodes its CostTable ranking, so this is only the
/// disambiguator when an intent's `A_open` has multiple unmasked cells.
pub fn cell_rank(c: &Cell) -> f64 {
    let cap = match c.1 {
        CapacityType::Spot => 0.0,
        CapacityType::OnDemand => 1.0,
    };
    // Stable hash of hw-class name → fractional tiebreak in [0,1).
    let h: u64 =
        c.0.bytes()
            .fold(0u64, |a, b| a.wrapping_mul(31).wrapping_add(u64::from(b)));
    cap + (h as f64 / u64::MAX as f64) * 0.5
}

/// Deterministic round-robin: sorted cell universe rotated by
/// `tick % len`. With per-tick budget caps, a fixed iteration order
/// would let early cells absorb the budget every tick and starve late
/// ones; rotating the start spreads it.
pub fn cells_round_robin(mut cells: Vec<Cell>, tick: u64) -> Vec<Cell> {
    if cells.is_empty() {
        return cells;
    }
    cells.sort();
    let off = (tick % cells.len() as u64) as usize;
    cells.rotate_left(off);
    cells
}

/// Caps for [`sizing`]: per-NodeClaim ceilings on each axis, plus the
/// fleet-wide core budget remaining and the per-tick cell cap. The
/// fuse-cache budget is here so [`sizing`] computes the same
/// [`intent_pod_footprint`](crate::reconcilers::pool::jobs::intent_pod_footprint)
/// triple FFD and `apply_intent_resources` will use.
pub struct SizingCfg {
    pub max_node_cores: u32,
    pub max_node_mem: u64,
    pub max_node_disk: u64,
    pub per_tick_cap: u32,
    pub budget: u32,
    pub fuse_cache_bytes: u64,
}

/// Per-claim `(cores, mem, ephemeral-storage)` requests covering `u`'s
/// deficit, plus `min(eta_seconds)` for [`FORECAST_ETA_ANNOTATION`].
/// kube-scheduler's NodeResourcesFit **sums** all three axes across
/// bound pods (per-pod emptyDir `sizeLimit` is enforcement, not the
/// fit-check), so each claim must cover its share of `Σ footprint` —
/// not just `max(footprint)`.
///
/// `n` starts at `max(⌈Σc/max_c⌉, ⌈Σm/max_m⌉, ⌈Σd/max_d⌉)` (the
/// 3-axis lower bound so no claim exceeds its per-NodeClaim cap) and
/// iterates upward until [`ffd::sim_packs`](super::ffd::sim_packs) —
/// the production [`ffd::simulate`](super::ffd::simulate) on `n`
/// uniform synthetic claims — packs every fitting intent. `Σ/n` is a lower
/// bound on bin-packing, not a guarantee. Upper bound `n = |u|` (one
/// claim per intent). Then capped by `per_tick_cap` and
/// `⌊budget/chunk⌋` (the cap may truncate; the remainder is re-seen
/// next tick).
///
/// Each claim is uniformly `(max(⌈Σc/n⌉, max_i c), max(Σm/n, max_i m),
/// max(Σd/n, max_i d))` — every intent fits every claim on every axis,
/// and `Σ out ≥ Σ in`. The production FFD is guaranteed to pack at
/// `n = |u|`; [`ffd::sim_packs`](super::ffd::sim_packs) finds the
/// smallest `n` where it does. STRIKE-4 close (r26 mb_002): the
/// predicate IS production `simulate` — no reimplemented sort/score to
/// diverge on. Unit-tested via the same call (the
/// §Simulator-shares-accounting executable guarantee).
// r[impl ctrl.nodeclaim.anchor-bulk+4]
pub fn sizing(cell: &Cell, u: &[&SpawnIntent], cfg: &SizingCfg) -> (Vec<(u32, u64, u64)>, f64) {
    use crate::reconcilers::pool::jobs::intent_pod_footprint;
    if u.is_empty() {
        return (Vec::new(), f64::MAX);
    }
    // STRIKE-5 (r27 mb_006): per-cell `cfg.max_node_*` (from
    // `HwClassDef.max_cores`) means the upstream "≤ global cap"
    // invariant is no longer "≤ per-cell cap". An over-cap intent has no
    // valid claim of ANY n (its pod requests `intent_pod_footprint(i)`,
    // not the claim's `(c,m,d)`); a clamped claim would just loop
    // (mint→Pending→re-mint). Filter and DROP — `assign_to_cells`' next
    // tick re-evaluates if the scheduler re-solves with the per-cell
    // ceiling, otherwise the intent stays dropped here.
    //
    // STRIKE-6 (r29 bug_019): the three scheduler-side producer holes
    // (override-bypass `fallback_cell`, `--capacity` `all_candidates`-
    // fallback, no-memo `Some(cap)` `reference_hw_class_for_system`) are
    // closed via the post-finalize chokepoint at
    // `snapshot.rs::solve_intent_for` (`SlaConfig::retain_hosting_
    // classes`). This backstop now fires only on version-skew
    // (controller `hw_classes` ≠ scheduler's) or a producer bypassing
    // `solve_intent_for` entirely.
    let (fits, over): (Vec<&SpawnIntent>, Vec<&SpawnIntent>) = u.iter().copied().partition(|i| {
        let (ic, im, id) = intent_pod_footprint(i, cfg.fuse_cache_bytes);
        ic <= cfg.max_node_cores && im <= cfg.max_node_mem && id <= cfg.max_node_disk
    });
    for i in &over {
        let (ic, im, id) = intent_pod_footprint(i, cfg.fuse_cache_bytes);
        tracing::warn!(
            intent_id = %i.intent_id, cell = %cell, footprint = ?(ic, im, id),
            cap = ?(cfg.max_node_cores, cfg.max_node_mem, cfg.max_node_disk),
            "intent footprint exceeds per-cell cap — dropping (scheduler ClassCeiling not gating?)"
        );
        ::metrics::counter!(
            "rio_controller_nodeclaim_intent_dropped_total",
            "reason" => "exceeds_cell_cap",
        )
        .increment(1);
    }
    if fits.is_empty() {
        return (Vec::new(), f64::MAX);
    }
    let (sum_c, sum_m, sum_d, max_c, max_m, max_d) = fits
        .iter()
        .map(|i| intent_pod_footprint(i, cfg.fuse_cache_bytes))
        .fold(
            (0u32, 0u64, 0u64, 0u32, 0u64, 0u64),
            |(c, m, d, mc, mm, md), (ic, im, id)| {
                (c + ic, m + im, d + id, mc.max(ic), mm.max(im), md.max(id))
            },
        );
    let min_eta = fits.iter().map(|i| i.eta_seconds).fold(f64::MAX, f64::min);
    let claim_at = |n: u32| {
        (
            sum_c.div_ceil(n).max(max_c).max(1),
            (sum_m / u64::from(n)).max(max_m),
            (sum_d / u64::from(n)).max(max_d),
        )
    };
    let n_lo = claim_count((sum_c, sum_m, sum_d), cfg);
    let n_hi = fits.len() as u32;
    // After the over-cap filter, every footprint ≤ cap on every axis, so
    // `claim_at(n_hi) = (max_c, max_m, max_d) ≤ cap` and `n_lo =
    // ⌈Σ/cap⌉ ≤ ⌈Σ/max_axis⌉ ≤ |fits|`. The find-loop terminates at
    // `n_hi` at the latest (one bin per intent trivially packs).
    debug_assert!(
        n_lo <= n_hi,
        "n_lo {n_lo} > n_hi {n_hi} after over-cap filter"
    );
    let n_pack = (n_lo..=n_hi)
        .find(|&n| super::ffd::sim_packs(cell, &fits, claim_at(n), n, cfg.fuse_cache_bytes))
        .unwrap_or(n_hi);
    let (chunk, mem, disk) = claim_at(n_pack);
    if cfg.budget < chunk {
        return (Vec::new(), min_eta);
    }
    let n = n_pack.min(cfg.per_tick_cap).min(cfg.budget / chunk);
    (vec![(chunk, mem, disk); n as usize], min_eta)
}

// r[impl ctrl.nodeclaim.budget.per-class]
/// §13c per-hw-class fleet-core sub-budget for `cover_deficit`'s
/// per-Cell loop. `min(global_remaining, class_cap − class_live −
/// class_created)` where:
/// - `global_remaining = max_fleet_cores − Σ live.allocatable −
///   Σ created_this_tick`
/// - `class_cap` = `HwClassDef.max_fleet_cores` for `cell.0` (`None` ⇒
///   global-only)
/// - `class_live` = Σ `n.allocatable.0` over live nodes whose `n.cell`
///   has `cell.0 == h` (sums spot+od — per-hwClass, NOT per-Cell)
/// - `class_created` = `created_h[h]` — cores minted this tick for ANY
///   `(h, *)` cell, accumulated across the per-Cell loop so spot's spend
///   subtracts from od's budget (otherwise each cap-type hits cap
///   independently → 2× $/hr exposure)
pub fn class_budget(
    global_remaining: u32,
    class_cap: Option<u32>,
    live: &[super::ffd::LiveNode],
    h: &str,
    class_created: u32,
) -> u32 {
    let class_remaining = class_cap.map_or(u32::MAX, |cap| {
        let class_live: u32 = live
            .iter()
            .filter(|n| n.cell.as_ref().is_some_and(|c| c.0 == h))
            .map(|n| n.allocatable.0)
            .sum();
        cap.saturating_sub(class_live.saturating_add(class_created))
    });
    global_remaining.min(class_remaining)
}

/// 3-axis `⌈Σ/max⌉` lower bound on `n` — the fewest claims such that
/// no per-claim request exceeds `max_node_*`. NOT a packing guarantee
/// (bin-packing's `Σ/cap` is a lower bound only); [`sizing`] iterates
/// upward from here.
pub fn claim_count(sum: (u32, u64, u64), cfg: &SizingCfg) -> u32 {
    let (sum_c, sum_m, sum_d) = sum;
    let need_c = sum_c.div_ceil(cfg.max_node_cores.max(1));
    let need_m = u32::try_from(sum_m.div_ceil(cfg.max_node_mem.max(1))).unwrap_or(u32::MAX);
    let need_d = u32::try_from(sum_d.div_ceil(cfg.max_node_disk.max(1))).unwrap_or(u32::MAX);
    need_c.max(need_m).max(need_d).max(1)
}

/// Build a NodeClaim for `cell` requesting `(cores, mem, disk)`.
///
/// - `metadata.generateName`: `rio-nc-<h>-<cap>-` (k8s appends 5
///   random chars). NodeClaims aren't idempotent-named — each tick's
///   cover may legitimately create another for the same cell.
/// - `metadata.labels`: the hw-class's full `[sla.hw_classes.$h]`
///   label conjunction (so the launched Node carries
///   `rio.build/hw-band` / `rio.build/storage` exactly as the legacy
///   NodePool template stamped — these are NODE labels, not
///   instance-type properties) PLUS [`HW_CLASS_LABEL`] +
///   [`CAPACITY_TYPE_LABEL`] (so [`super::ffd::LiveNode::from`]
///   recovers `cell` next tick), [`NODEPOOL_LABEL`] =
///   [`SHIM_NODEPOOL`] (Karpenter state-tracking),
///   [`super::NODE_ROLE_LABEL`] = `builder` (builder pod affinity
///   requires it; the legacy band-loop NodePool stamped it), and the
///   [`super::OWNER_LABEL`] selector.
/// - `spec.nodeClassRef`: the hw-class's EC2NodeClass — `rio-nvme` for
///   nvme storage tiers (so `instanceStorePolicy: RAID0` applies),
///   `rio-metal` for metal, `rio-default` otherwise. Per-class via
///   [`HwClassCtx::node_class`].
/// - `spec.requirements`: ONLY labels Karpenter's instance-type
///   discovery knows — the hw-class's `requirements`
///   (`karpenter.k8s.aws/instance-generation In [7]`,
///   `kubernetes.io/arch In [amd64]`, …), `karpenter.sh/capacity-type
///   In [<cap>]`, and `karpenter.k8s.aws/instance-size {In|NotIn}
///   <metal_sizes>` (I-205 partition; operator gated on
///   `hw.node_class == METAL_NODE_CLASS`). Putting `rio.build/*` here
///   matches 0 instance types → Karpenter posts `Launched=False
///   reason=InsufficientCapacity` and GCs the claim ~1s later (the
///   live B8 finding).
/// - `spec.taints`: the single [`builder_taint`] so non-builder
///   cluster pods stay off rio-minted builder nodes (ADR-019).
/// - `spec.resources.requests`: `{cpu, memory, ephemeral-storage}`.
///   Karpenter uses these as the floor for instance-type selection;
///   `spec.requirements` constrains the family.
pub fn build_nodeclaim(
    cell: &Cell,
    req: (u32, u64, u64),
    forecast_eta_secs: f64,
    hw: &HwClassCtx,
    cfg: &CoverCfg<'_>,
) -> NodeClaim {
    let cap_label = match cell.1 {
        CapacityType::Spot => "spot",
        CapacityType::OnDemand => "on-demand",
    };
    let (owner_k, owner_v) = super::OWNER_LABEL
        .split_once('=')
        .expect("OWNER_LABEL is k=v");
    let (role_k, role_v) = super::NODE_ROLE_LABEL;
    // hw.labels (rio.build/hw-band, rio.build/storage,
    // kubernetes.io/arch, …) are STAMPED onto the Node via
    // metadata.labels — Karpenter copies NodeClaim labels to the
    // launched Node. The legacy NodePool template did this; B3 deleted
    // those NodePools, so the controller must stamp directly.
    let mut labels: BTreeMap<String, String> = hw.labels.iter().cloned().collect();
    labels.extend([
        (HW_CLASS_LABEL.into(), cell.0.clone()),
        (CAPACITY_TYPE_LABEL.into(), cap_label.into()),
        (NODEPOOL_LABEL.into(), SHIM_NODEPOOL.into()),
        (role_k.into(), role_v.into()),
        (owner_k.into(), owner_v.into()),
    ]);
    let mk_req = |key: &str, op: &str, values: Vec<String>| NodeSelectorRequirementWithMin {
        key: key.into(),
        operator: op.into(),
        values,
        min_values: None,
    };
    // requirements: ONLY instance-type-discovery labels. The hw-class's
    // `requirements` field carries karpenter.k8s.aws/* + arch
    // (validated by SlaConfig::validate to exclude rio.build/*).
    let mut requirements: Vec<_> = hw
        .requirements
        .iter()
        .map(|r| mk_req(&r.key, &r.operator, r.values.clone()))
        .collect();
    requirements.push(mk_req(CAPACITY_TYPE_LABEL, "In", vec![cap_label.into()]));
    if !cfg.metal_sizes.is_empty() {
        // I-205 partition: same predicate as helm
        // `templates/karpenter.yaml`'s `nodePools` loop —
        // metal-nodeClass gets the `In` side, everything else `NotIn`.
        let op = if hw.node_class == METAL_NODE_CLASS {
            "In"
        } else {
            "NotIn"
        };
        requirements.push(mk_req(INSTANCE_SIZE_LABEL, op, cfg.metal_sizes.to_vec()));
    }
    let requests: BTreeMap<String, Quantity> = [
        ("cpu".into(), Quantity(req.0.to_string())),
        ("memory".into(), Quantity(req.1.to_string())),
        ("ephemeral-storage".into(), Quantity(req.2.to_string())),
    ]
    .into();
    // Stamp the forecast eta when finite and >0 (Ready intents have
    // eta=0; an all-Ready cell needs no z-correction). `f64::MAX`
    // (empty deficit fold identity) is also skipped.
    let annotations = (forecast_eta_secs > 0.0 && forecast_eta_secs.is_finite()).then(|| {
        [(
            FORECAST_ETA_ANNOTATION.into(),
            forecast_eta_secs.to_string(),
        )]
        .into_iter()
        .collect()
    });
    NodeClaim {
        metadata: ObjectMeta {
            generate_name: Some(format!("rio-nc-{}-{}-", cell.0, cell.1.as_str())),
            labels: Some(labels),
            annotations,
            ..Default::default()
        },
        spec: NodeClaimSpec {
            node_class_ref: NodeClassRef {
                group: NODE_CLASS_GROUP.into(),
                kind: NODE_CLASS_KIND.into(),
                name: hw.node_class.clone(),
            },
            requirements,
            // r[impl ctrl.nodeclaim.taints.hwclass]
            // §13c: per-hwClass taints chained after the universal
            // builder taint. e.g. metal classes carry
            // `rio.build/kvm=true:NoSchedule`.
            taints: std::iter::once(builder_taint())
                .chain(hw.taints.iter().cloned())
                .collect(),
            resources: Some(ResourceRequirements {
                requests: Some(requests),
                ..Default::default()
            }),
        },
        status: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_proto::types::{NodeSelectorRequirement, NodeSelectorTerm};

    const GI: u64 = 1 << 30;

    fn intent(
        id: &str,
        cores: u32,
        mem: u64,
        cells: &[(&str, CapacityType)],
        ready: Option<bool>,
    ) -> SpawnIntent {
        let (hw_class_names, node_affinity) = cells
            .iter()
            .map(|(h, cap)| {
                let cap_label = match cap {
                    CapacityType::Spot => "spot",
                    CapacityType::OnDemand => "on-demand",
                };
                let term = NodeSelectorTerm {
                    match_expressions: vec![NodeSelectorRequirement {
                        key: CAPACITY_TYPE_LABEL.into(),
                        operator: "In".into(),
                        values: vec![cap_label.into()],
                    }],
                };
                ((*h).to_string(), term)
            })
            .unzip();
        SpawnIntent {
            intent_id: id.into(),
            cores,
            mem_bytes: mem,
            disk_bytes: GI,
            ready,
            hw_class_names,
            node_affinity,
            ..Default::default()
        }
    }

    // --- sizing / claim_count ------------------------------------------

    fn cfg(max_node_cores: u32, per_tick_cap: u32, budget: u32) -> SizingCfg {
        SizingCfg {
            max_node_cores,
            max_node_mem: 256 * GI,
            max_node_disk: 450 * GI,
            per_tick_cap,
            budget,
            fuse_cache_bytes: 50 * GI,
        }
    }

    #[test]
    fn claim_count_3axis_lower_bound() {
        let f = |c, m, d, mxc| claim_count((c, m, d), &cfg(mxc, u32::MAX, u32::MAX));
        assert_eq!(f(100, 0, 0, 32), 4, "cores binds: ⌈100/32⌉");
        assert_eq!(f(20, 0, 0, 64), 1, "Σc < max_c");
        // Σm binds: 192×{1c,8Gi} → Σm=1536Gi at 256Gi cap → 6.
        assert_eq!(f(192, 1536 * GI, 0, 64), 6, "mem axis binds");
        assert_eq!(f(10, 0, 1800 * GI, 64), 4, "disk binds: ⌈1800/450⌉");
        assert_eq!(f(0, 0, 0, 32), 1, "empty floors at 1");
    }

    fn h_spot() -> Cell {
        Cell("h".into(), CapacityType::Spot)
    }

    #[test]
    fn sizing_respects_budget_and_per_tick_cap() {
        let u: Vec<_> = (0..10)
            .map(|k| intent_hd(&format!("i{k}"), 8, 8 * GI, 5 * GI, Some(true)))
            .collect();
        let refs: Vec<&SpawnIntent> = u.iter().collect();
        // Σc=80, n_lo=⌈80/32⌉=3; sim_packs finds n_pack=5 (disk-bound:
        // each pod's footprint.d = 5×1.5+50+1 ≈ 58.5Gi; 3 bins of
        // ⌈Σd/3⌉≈195Gi fit 3 each = 9 < 10).
        let (c, _) = sizing(&h_spot(), &refs, &cfg(32, 8, 200));
        assert_eq!(c.len(), 5, "n_pack binds");
        let (c, _) = sizing(&h_spot(), &refs, &cfg(32, 2, 200));
        assert_eq!(c.len(), 2, "per_tick binds");
        // chunk at n_pack=5 is ⌈80/5⌉.max(8)=16; budget=20 → ⌊20/16⌋=1.
        let (c, _) = sizing(&h_spot(), &refs, &cfg(32, 8, 20));
        assert_eq!(c.len(), 1, "budget binds");
        let (c, _) = sizing(&h_spot(), &refs, &cfg(32, 8, 10));
        assert!(c.is_empty(), "budget < chunk");
    }

    /// Oracle: feed `claims` back as synthetic LiveNodes to the
    /// production FFD sim and assert all `intents` place. The
    /// §Simulator-shares-accounting executable guarantee — sizing
    /// produces what FFD can pack.
    ///
    /// INDEPENDENT synthetic-env construction so a regression in
    /// [`super::super::ffd::sim_packs`]'s env (the impl predicate's
    /// `eta_seconds=f64::MIN` clone, `cell:Some` LiveNode literal,
    /// `hw_arch=|_,_|true`) is detectable. NOT `sim_packs` itself —
    /// `sizing()` already verified `sim_packs(.., n_pack)==true`, so
    /// re-calling it is `f(x)==f(x)` (r27 bug_001). The two synthetic
    /// envs share INTENT (neutralize lead-time gate, fully-registered
    /// empty bins of the right cell) but diverge in CONSTRUCTION
    /// (`ffd::tests::node` here sets `node_name=Some`, `created_secs`;
    /// sim_packs's inline literal doesn't). Uniform claims only
    /// (sizing's output is `vec![bin; n]`).
    fn oracle_places_all(
        cell: &Cell,
        intents: &[SpawnIntent],
        claims: &[(u32, u64, u64)],
        fuse: u64,
    ) -> bool {
        use super::super::ffd;
        let Some(&bin) = claims.first() else {
            return intents.is_empty();
        };
        // Same eta-neutralization as sim_packs (independently
        // constructed): a_open's `eta < lead_time` filter would
        // otherwise drop forecast intents against
        // `CellSketches::default()`'s lead_time=0.
        let neutralized: Vec<SpawnIntent> = intents
            .iter()
            .map(|i| SpawnIntent {
                eta_seconds: f64::MIN,
                ..i.clone()
            })
            .collect();
        let nodes: Vec<_> = (0..claims.len())
            .map(|k| ffd::tests::node(&format!("oracle{k}"), &cell.0, cell.1, bin.0, bin.1, bin.2))
            .collect();
        ffd::simulate(
            &neutralized,
            &nodes,
            &CellSketches::default(),
            &std::collections::HashMap::new(),
            fuse,
            |_, _| true,
        )
        .1
        .is_empty()
    }

    fn intent_hd(id: &str, cores: u32, mem: u64, disk: u64, ready: Option<bool>) -> SpawnIntent {
        let mut i = intent(id, cores, mem, &[("h", CapacityType::Spot)], ready);
        i.disk_bytes = disk;
        i.disk_headroom_factor = Some(1.5);
        i
    }

    /// STRIKE-3 (mb_009): direct cases. Unconstrained budget so sizing
    /// covers ALL intents (FFD oracle), and every claim ≤ per-axis cap.
    // r[verify ctrl.nodeclaim.anchor-bulk+4]
    #[test]
    fn sizing_invariants_hold() {
        let scfg = cfg(64, u32::MAX, u32::MAX);
        let check = |name: &str, intents: Vec<SpawnIntent>| {
            let refs: Vec<&SpawnIntent> = intents.iter().collect();
            let (claims, _) = sizing(&h_spot(), &refs, &scfg);
            assert!(
                oracle_places_all(&h_spot(), &intents, &claims, scfg.fuse_cache_bytes),
                "{name}: FFD-oracle leaves unplaced; claims={claims:?}"
            );
            for (k, &(c, m, d)) in claims.iter().enumerate() {
                assert!(
                    c <= scfg.max_node_cores,
                    "{name}: claim[{k}].cores {c} > cap"
                );
                assert!(m <= scfg.max_node_mem, "{name}: claim[{k}].mem {m} > cap");
                assert!(d <= scfg.max_node_disk, "{name}: claim[{k}].disk {d} > cap");
            }
        };
        // mb_009 inverse: 8×{2c,8Gi,10Gi} uniform. Old code: n=1,
        // (16,8Gi,66Gi); Σm=64Gi,Σd=528Gi unaddressed.
        check(
            "uniform-8",
            (0..8)
                .map(|k| intent_hd(&format!("u{k}"), 2, 8 * GI, 10 * GI, Some(true)))
                .collect(),
        );
        // mb_024(1) anchor: [{32c,200Gi},{32c,2Gi}×7].
        let mut a: Vec<_> = vec![intent_hd("big", 32, 200 * GI, 5 * GI, Some(true))];
        a.extend((0..7).map(|k| intent_hd(&format!("s{k}"), 32, 2 * GI, 5 * GI, Some(true))));
        check("anchor", a);
        // B0-arch counter-ex: 2×200Gi + 8×10Gi — second 200Gi must
        // place (single-anchor would fail).
        let mut b: Vec<_> = (0..2)
            .map(|k| intent_hd(&format!("b{k}"), 4, 200 * GI, 5 * GI, Some(true)))
            .collect();
        b.extend((0..8).map(|k| intent_hd(&format!("t{k}"), 4, 10 * GI, 5 * GI, Some(true))));
        check("two-outliers", b);
        // B0-inverse: 192×{1c,8Gi,2Gi} — Σm=1536Gi at 256Gi cap → n=6;
        // each claim's mem ≤ 256Gi.
        check(
            "mem-bound",
            (0..192)
                .map(|k| intent_hd(&format!("m{k}"), 1, 8 * GI, 2 * GI, Some(true)))
                .collect(),
        );
        // Empty.
        let (c, e) = sizing(&h_spot(), &[], &scfg);
        assert!(c.is_empty());
        assert_eq!(e, f64::MAX);
    }

    /// STRIKE-4 (r26 mb_002): mixed `ready` values. The pre-r26
    /// open-coded predicate sorted `(c, m)` only; production
    /// `simulate` sorts `(ready, c, m)` — under MostAllocated scoring
    /// the orders pack differently. With `sim_packs` IS `simulate`,
    /// sizing finds the `n` where production FFD packs.
    #[test]
    fn sizing_mixed_ready_ffd_oracle() {
        // 2 ready 4c + 2 forecast 6c on 10c-cap nodes (low mem/disk so
        // those axes don't bind). simulate's ready-first order at n=2
        // places both 4c on one bin via MostAllocated, then a 6c on
        // the other — second 6c stranded. cores-only sort would have
        // packed at n=2. sim_packs (= simulate) finds n_pack > 2.
        let mut u = vec![
            intent_hd("r0", 4, GI, GI, Some(true)),
            intent_hd("r1", 4, GI, GI, Some(true)),
        ];
        u.extend((0..2).map(|k| intent_hd(&format!("f{k}"), 6, GI, GI, Some(false))));
        let scfg = SizingCfg {
            max_node_cores: 10,
            max_node_mem: 256 * GI,
            max_node_disk: 450 * GI,
            per_tick_cap: u32::MAX,
            budget: u32::MAX,
            fuse_cache_bytes: 50 * GI,
        };
        let refs: Vec<&SpawnIntent> = u.iter().collect();
        let (claims, _) = sizing(&h_spot(), &refs, &scfg);
        assert!(
            oracle_places_all(&h_spot(), &u, &claims, scfg.fuse_cache_bytes),
            "mixed-ready FFD-oracle leaves unplaced; claims={claims:?}"
        );
    }

    /// STRIKE-5 (r27 mb_006): per-cell `cfg.max_node_cores` (from
    /// `HwClassDef.max_cores`) can be tighter than the GLOBAL cap the
    /// scheduler's chokepoint clamps at. An over-cap intent has no valid
    /// claim of any `n` (its pod requests `intent_pod_footprint(i)`, not
    /// the claim's `(c,m,d)`); a clamped 32c claim would just loop
    /// (mint→Pending→re-mint). `sizing()` filters and DROPS (with metric
    /// + warn), sizing on the remainder.
    // r[verify ctrl.nodeclaim.anchor-bulk+4]
    #[test]
    fn sizing_filters_intent_exceeding_per_cell_cap() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let scfg = cfg(32, u32::MAX, u32::MAX);
        let dropped_count = |rec: &DebuggingRecorder| {
            rec.snapshotter()
                .snapshot()
                .into_vec()
                .into_iter()
                .find_map(|(k, _, _, v)| {
                    let key = k.key();
                    (key.name() == "rio_controller_nodeclaim_intent_dropped_total"
                        && key
                            .labels()
                            .any(|l| l.key() == "reason" && l.value() == "exceeds_cell_cap"))
                    .then_some(v)
                })
        };
        // All over → empty + metric.
        {
            let rec = DebuggingRecorder::new();
            let _g = ::metrics::set_default_local_recorder(&rec);
            let over = intent_hd("o", 48, 8 * GI, 5 * GI, Some(true));
            let (claims, eta) = sizing(&h_spot(), &[&over], &scfg);
            assert!(
                claims.is_empty(),
                "48c@cap=32 must drop, not clamp: {claims:?}"
            );
            assert_eq!(eta, f64::MAX, "all-dropped → no eta to forecast");
            assert_eq!(dropped_count(&rec), Some(DebugValue::Counter(1)));
        }
        // Mixed → over filtered, fits sized normally, oracle places.
        {
            let rec = DebuggingRecorder::new();
            let _g = ::metrics::set_default_local_recorder(&rec);
            let over = intent_hd("o", 48, 8 * GI, 5 * GI, Some(true));
            let f0 = intent_hd("f0", 16, 8 * GI, 5 * GI, Some(true));
            let f1 = intent_hd("f1", 16, 8 * GI, 5 * GI, Some(true));
            let u = [&over, &f0, &f1];
            let (claims, _) = sizing(&h_spot(), &u, &scfg);
            assert_eq!(claims.len(), 1, "Σc(fits)=32 at cap=32 → n=1");
            assert_eq!(claims[0].0, 32);
            assert!(
                oracle_places_all(
                    &h_spot(),
                    &[f0.clone(), f1.clone()],
                    &claims,
                    scfg.fuse_cache_bytes
                ),
                "fits-only must pack"
            );
            assert_eq!(dropped_count(&rec), Some(DebugValue::Counter(1)));
        }
        // Mem axis over.
        {
            let rec = DebuggingRecorder::new();
            let _g = ::metrics::set_default_local_recorder(&rec);
            let over_m = intent_hd("om", 4, 512 * GI, 5 * GI, Some(true));
            let (claims, _) = sizing(&h_spot(), &[&over_m], &scfg);
            assert!(claims.is_empty(), "mem 512Gi@cap=256Gi must drop");
            assert_eq!(dropped_count(&rec), Some(DebugValue::Counter(1)));
        }
    }

    /// Hand-rolled property check (proptest-equivalent without the dep,
    /// matching `ffd::tests::ffd_never_overcommits`): 100 random intent
    /// vecs, fixed-seed LCG so failures are reproducible. `ready` is
    /// varied — the input axis a reimplemented packing predicate would
    /// diverge on.
    #[test]
    fn sizing_random_intents_ffd_oracle() {
        let scfg = cfg(64, u32::MAX, u32::MAX);
        let mut s = 0x5eed_0000_u64;
        let mut next = |n: u64| {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (s >> 33) % n
        };
        for case in 0..100 {
            let len = 1 + next(40) as usize;
            let intents: Vec<_> = (0..len)
                .map(|k| {
                    let ready = match next(3) {
                        0 => Some(true),
                        1 => Some(false),
                        _ => None,
                    };
                    intent_hd(
                        &format!("c{case}i{k}"),
                        1 + next(32) as u32,
                        (1 + next(128)) * GI,
                        (1 + next(80)) * GI,
                        ready,
                    )
                })
                .collect();
            let refs: Vec<&SpawnIntent> = intents.iter().collect();
            let (claims, _) = sizing(&h_spot(), &refs, &scfg);
            assert!(
                oracle_places_all(&h_spot(), &intents, &claims, scfg.fuse_cache_bytes),
                "case {case}: FFD leaves unplaced; len={len} claims={claims:?}"
            );
            for &(c, m, d) in &claims {
                assert!(
                    c <= scfg.max_node_cores && m <= scfg.max_node_mem && d <= scfg.max_node_disk,
                    "case {case}: cap"
                );
            }
        }
    }

    // --- round-robin ---------------------------------------------------

    #[test]
    fn cells_round_robin_from_rotating_start() {
        let cs = || {
            vec![
                Cell("a".into(), CapacityType::Spot),
                Cell("c".into(), CapacityType::Spot),
                Cell("b".into(), CapacityType::Spot),
            ]
        };
        // tick=0 → sorted: a,b,c.
        let r0 = cells_round_robin(cs(), 0);
        assert_eq!(r0[0].0, "a");
        assert_eq!(r0[1].0, "b");
        assert_eq!(r0[2].0, "c");
        // tick=1 → rotated: b,c,a.
        let r1 = cells_round_robin(cs(), 1);
        assert_eq!(r1[0].0, "b");
        // tick=2 → c,a,b.
        let r2 = cells_round_robin(cs(), 2);
        assert_eq!(r2[0].0, "c");
        // tick=3 wraps → a,b,c.
        assert_eq!(cells_round_robin(cs(), 3)[0].0, "a");
        // No cell starves: over 3 ticks, every cell leads once.
        let leads: std::collections::HashSet<_> = (0..3)
            .map(|t| cells_round_robin(cs(), t)[0].0.clone())
            .collect();
        assert_eq!(leads.len(), 3);
        // Empty input → empty.
        assert!(cells_round_robin(vec![], 5).is_empty());
    }

    // --- assign_to_cells -----------------------------------------------

    #[test]
    fn assign_unplaced_picks_cheapest_open() {
        let h1 = ("h1", CapacityType::Spot);
        let h2 = ("h2", CapacityType::Spot);
        let unplaced = [
            intent("a", 4, GI, &[h1, h2], Some(true)),
            intent("b", 2, GI, &[h2], Some(true)),
        ];
        // h1 cheaper.
        let price = |c: &Cell| if c.0 == "h1" { 0.03 } else { 0.05 };
        let none = HashSet::new();
        let (by, d) = assign_to_cells(&unplaced, &CellSketches::default(), &none, price, |_| None);
        assert_eq!(d, 0);
        assert_eq!(by.len(), 2);
        let h1k = Cell("h1".into(), CapacityType::Spot);
        let h2k = Cell("h2".into(), CapacityType::Spot);
        assert_eq!(by[&h1k].len(), 1);
        assert_eq!(by[&h1k][0].intent_id, "a", "a's cheapest-open is h1");
        assert_eq!(by[&h2k].len(), 1);
        assert_eq!(by[&h2k][0].intent_id, "b", "b only opens h2");
    }

    /// ICE-masked spot cell → intent with `A_open=[spot,od]` fails
    /// over to od (cheapest UNmasked). Prevents the "assigned to
    /// masked cell → cover_deficit skips → intent stranded" hole.
    #[test]
    fn assign_masked_cell_fails_over_to_od() {
        let cells = [("h", CapacityType::Spot), ("h", CapacityType::OnDemand)];
        let unplaced = [intent("x", 4, GI, &cells, Some(true))];
        let spot = Cell("h".into(), CapacityType::Spot);
        let od = Cell("h".into(), CapacityType::OnDemand);
        // No mask → spot (cell_rank: spot < od).
        let (by, _) = assign_to_cells(
            &unplaced,
            &CellSketches::default(),
            &HashSet::new(),
            cell_rank,
            |_| None,
        );
        assert_eq!(by[&spot].len(), 1);
        // spot ICE-masked → od.
        let masked: HashSet<Cell> = [spot.clone()].into();
        let (by, d) = assign_to_cells(
            &unplaced,
            &CellSketches::default(),
            &masked,
            cell_rank,
            |_| None,
        );
        assert_eq!(d, 0);
        assert!(!by.contains_key(&spot));
        assert_eq!(by[&od].len(), 1);
        // Both masked → A_open empties; non-empty hw_class_names ⇒ NOT
        // fallback-routed (next tick re-evaluates).
        let masked: HashSet<Cell> = [spot, od].into();
        let (by, d) = assign_to_cells(
            &unplaced,
            &CellSketches::default(),
            &masked,
            cell_rank,
            |_| None,
        );
        assert!(by.is_empty());
        assert_eq!(d, 0);
    }

    /// Cold-start: `hw_class_names=[]` → routed via `fallback`.
    /// `fallback → None` → counted in `dropped`. Forecast intent with
    /// non-empty `hw_class_names` but lead-time-gated A_open → NOT
    /// fallback-routed, NOT counted (next tick re-evaluates).
    #[test]
    fn assign_hw_agnostic_uses_fallback() {
        let ref_cell = Cell("ref".into(), CapacityType::Spot);
        let unplaced = [
            // hw-agnostic, x86 → fallback gives ref cell.
            SpawnIntent {
                intent_id: "agn-x".into(),
                cores: 4,
                system: "x86_64-linux".into(),
                ready: Some(true),
                ..Default::default()
            },
            // hw-agnostic, unmappable → fallback None → dropped.
            SpawnIntent {
                intent_id: "agn-u".into(),
                cores: 4,
                ready: Some(true),
                ..Default::default()
            },
            // Non-empty hw_class_names + forecast + eta>lead_time →
            // A_open empty → NOT fallback-routed (silently skipped).
            {
                let mut i = intent("fc", 4, GI, &[("h1", CapacityType::Spot)], Some(false));
                i.eta_seconds = 999.0;
                i
            },
        ];
        let fallback = |i: &SpawnIntent| {
            (i.system == "x86_64-linux").then(|| Cell("ref".into(), CapacityType::Spot))
        };
        let (by, d) = assign_to_cells(
            &unplaced,
            &CellSketches::default(),
            &HashSet::new(),
            |_| 0.0,
            fallback,
        );
        assert_eq!(d, 1, "agn-u dropped");
        assert_eq!(by.len(), 1);
        assert_eq!(by[&ref_cell].len(), 1);
        assert_eq!(by[&ref_cell][0].intent_id, "agn-x");
    }

    #[test]
    fn assign_empty_unplaced_empty_output() {
        let (by, d) = assign_to_cells(
            &[],
            &CellSketches::default(),
            &HashSet::new(),
            |_| 0.0,
            |_| None,
        );
        assert!(by.is_empty());
        assert_eq!(d, 0);
    }

    #[test]
    fn cell_rank_spot_before_od() {
        let s = Cell("h".into(), CapacityType::Spot);
        let o = Cell("h".into(), CapacityType::OnDemand);
        assert!(cell_rank(&s) < cell_rank(&o));
        // Same cap → deterministic on name (any order, but stable).
        let a = Cell("a".into(), CapacityType::Spot);
        let b = Cell("b".into(), CapacityType::Spot);
        assert_ne!(cell_rank(&a), cell_rank(&b));
    }

    // --- build_nodeclaim -----------------------------------------------

    fn hw_ctx(node_class: &str) -> HwClassCtx {
        HwClassCtx {
            node_class: node_class.into(),
            // Live B8 finding: labels are the [sla.hw_classes] STAMPS
            // (rio.build/* + arch). These go in metadata.labels, NOT
            // spec.requirements.
            labels: vec![
                ("rio.build/hw-band".into(), "mid".into()),
                ("rio.build/storage".into(), "ebs".into()),
                ("kubernetes.io/arch".into(), "amd64".into()),
            ],
            requirements: vec![
                NodeSelectorRequirement {
                    key: "karpenter.k8s.aws/instance-generation".into(),
                    operator: "In".into(),
                    values: vec!["6".into(), "7".into()],
                },
                NodeSelectorRequirement {
                    key: "kubernetes.io/arch".into(),
                    operator: "In".into(),
                    values: vec!["amd64".into()],
                },
            ],
            taints: vec![],
        }
    }

    /// Metal hw-class context with the §13c kvm taint.
    fn hw_ctx_metal() -> HwClassCtx {
        let mut ctx = hw_ctx(METAL_NODE_CLASS);
        ctx.taints = vec![Taint {
            key: "rio.build/kvm".into(),
            value: Some("true".into()),
            effect: "NoSchedule".into(),
            ..Default::default()
        }];
        ctx
    }

    /// Asserts the full wire shape: generateName, labels (hw-class
    /// CONJUNCTION stamps + hw-class/cap-type/shim-nodepool/owner),
    /// nodeClassRef (per-hw-class), taints (builder NoSchedule),
    /// requirements (hw_requirements + capacity-type + metal-NotIn
    /// ONLY — no `rio.build/*`), resources.requests.
    #[test]
    fn build_nodeclaim_spec_shape() {
        let cell = Cell("mid-ebs-x86".into(), CapacityType::Spot);
        let metal = vec!["metal".into(), "metal-24xl".into()];
        let nc = build_nodeclaim(
            &cell,
            (8, 32 * GI, 100 * GI),
            25.0,
            &hw_ctx("rio-nvme"),
            &CoverCfg {
                metal_sizes: &metal,
            },
        );

        let meta = &nc.metadata;
        // F7: forecast eta stamped as annotation.
        assert_eq!(
            meta.annotations.as_ref().unwrap()[FORECAST_ETA_ANNOTATION],
            "25"
        );
        assert_eq!(
            meta.generate_name.as_deref(),
            Some("rio-nc-mid-ebs-x86-spot-")
        );
        let labels = meta.labels.as_ref().unwrap();
        assert_eq!(labels[HW_CLASS_LABEL], "mid-ebs-x86");
        assert_eq!(labels[CAPACITY_TYPE_LABEL], "spot");
        assert_eq!(labels[NODEPOOL_LABEL], SHIM_NODEPOOL);
        assert_eq!(labels["rio.build/nodeclaim-pool"], "builder");
        // Builder pod affinity requires `node-role In [builder]` — the
        // legacy band-loop NodePool template stamped it; B3 deleted
        // those, so cover must stamp it directly (live B8 finding).
        assert_eq!(labels["rio.build/node-role"], "builder");
        // hw-class conjunction stamped onto Node (legacy NodePool
        // template behaviour, now controller's responsibility post-B3).
        assert_eq!(labels["rio.build/hw-band"], "mid");
        assert_eq!(labels["rio.build/storage"], "ebs");
        assert_eq!(labels["kubernetes.io/arch"], "amd64");

        let spec = &nc.spec;
        // mb_002: per-hw-class nodeClass (was scalar "rio-default";
        // nvme classes need rio-nvme so instanceStorePolicy:RAID0
        // applies).
        assert_eq!(spec.node_class_ref.name, "rio-nvme");
        assert_eq!(spec.node_class_ref.group, "karpenter.k8s.aws");
        assert_eq!(spec.node_class_ref.kind, "EC2NodeClass");
        // mb_002: builder taint stamped (band-loop NodePool template
        // carried it; ADR-019 isolation).
        assert_eq!(spec.taints, vec![builder_taint()]);
        assert_eq!(spec.taints[0].key, "rio.build/builder");
        assert_eq!(spec.taints[0].effect, "NoSchedule");
        // 2 hw-reqs + capacity-type + instance-size NotIn. NO
        // rio.build/* — those match 0 instance types and trigger
        // Karpenter ICE-GC.
        assert_eq!(spec.requirements.len(), 4);
        assert!(
            !spec
                .requirements
                .iter()
                .any(|r| r.key.starts_with("rio.build/")),
            "rio.build/* are stamps not instance-type properties: {:?}",
            spec.requirements
        );
        let gen_req = spec
            .requirements
            .iter()
            .find(|r| r.key == "karpenter.k8s.aws/instance-generation")
            .unwrap();
        assert_eq!(gen_req.values, vec!["6", "7"]);
        let cap_req = spec
            .requirements
            .iter()
            .find(|r| r.key == CAPACITY_TYPE_LABEL)
            .unwrap();
        assert_eq!(cap_req.values, vec!["spot"]);
        let size_req = spec
            .requirements
            .iter()
            .find(|r| r.key == INSTANCE_SIZE_LABEL)
            .unwrap();
        assert_eq!(size_req.operator, "NotIn");
        assert_eq!(size_req.values, vec!["metal", "metal-24xl"]);

        let req = spec.resources.as_ref().unwrap().requests.as_ref().unwrap();
        assert_eq!(req["cpu"], Quantity("8".into()));
        assert_eq!(req["memory"], Quantity((32 * GI).to_string()));
        assert_eq!(req["ephemeral-storage"], Quantity((100 * GI).to_string()));
    }

    /// Empty hw_requirements + empty metal_sizes (kwok/vmtest) → only
    /// capacity-type requirement (Karpenter v1 CRD requires ≥1).
    #[test]
    fn build_nodeclaim_on_demand_label_form() {
        let cell = Cell("h".into(), CapacityType::OnDemand);
        let hw = HwClassCtx {
            node_class: "x".into(),
            labels: vec![],
            requirements: vec![],
            taints: vec![],
        };
        let nc = build_nodeclaim(
            &cell,
            (4, 8 * GI, 50 * GI),
            0.0,
            &hw,
            &CoverCfg { metal_sizes: &[] },
        );
        // eta=0 (all-Ready cell) → no annotation.
        assert!(nc.metadata.annotations.is_none());
        // Karpenter label form is "on-demand", NOT the PG/helm "od".
        assert_eq!(
            nc.metadata.labels.as_ref().unwrap()[CAPACITY_TYPE_LABEL],
            "on-demand"
        );
        // generateName uses the PG/helm form (DNS-safe, matches
        // `Cell::to_string`).
        assert_eq!(nc.metadata.generate_name.as_deref(), Some("rio-nc-h-od-"));
        // hw_reqs=[] + metal=[] → only capacity-type req.
        assert_eq!(nc.spec.requirements.len(), 1);
        assert_eq!(nc.spec.requirements[0].key, CAPACITY_TYPE_LABEL);
    }

    /// I-205 partition: `node_class == METAL_NODE_CLASS` →
    /// `instance-size In <metalSizes>` (not NotIn). Single-sources
    /// the predicate with helm `templates/karpenter.yaml`. §13c T7:
    /// metal hwClass taints chained after `builder_taint()`.
    // r[verify ctrl.nodeclaim.taints.hwclass]
    #[test]
    fn build_nodeclaim_metal_nodeclass_gets_in_side() {
        let cell = Cell("metal-x86".into(), CapacityType::OnDemand);
        let metal = vec!["metal".into(), "metal-24xl".into()];
        let nc = build_nodeclaim(
            &cell,
            (64, 256 * GI, 500 * GI),
            0.0,
            &hw_ctx_metal(),
            &CoverCfg {
                metal_sizes: &metal,
            },
        );
        let size_req = nc
            .spec
            .requirements
            .iter()
            .find(|r| r.key == INSTANCE_SIZE_LABEL)
            .unwrap();
        assert_eq!(size_req.operator, "In");
        assert_eq!(size_req.values, vec!["metal", "metal-24xl"]);
        assert_eq!(nc.spec.node_class_ref.name, METAL_NODE_CLASS);
        // §13c T7: metal hwClass taint chained after builder_taint().
        assert_eq!(nc.spec.taints.len(), 2, "builder_taint + kvm taint");
        assert_eq!(nc.spec.taints[0].key, "rio.build/builder");
        assert_eq!(nc.spec.taints[1].key, "rio.build/kvm");
        assert_eq!(nc.spec.taints[1].effect, "NoSchedule");
        // §13c N2: (metal, od) capacity-type requirement is exactly one
        // `In` (cell-derived; no hwClass-side requirement to conflict).
        let cap_reqs: Vec<_> = nc
            .spec
            .requirements
            .iter()
            .filter(|r| r.key == CAPACITY_TYPE_LABEL)
            .collect();
        assert_eq!(cap_reqs.len(), 1, "exactly one capacity-type requirement");
        assert_eq!(cap_reqs[0].operator, "In");
        assert_eq!(cap_reqs[0].values, vec!["on-demand"]);

        // Non-metal ctx → only builder_taint.
        let nc_std = build_nodeclaim(
            &Cell("mid-ebs-x86".into(), CapacityType::Spot),
            (8, 32 * GI, 100 * GI),
            0.0,
            &hw_ctx("rio-default"),
            &CoverCfg {
                metal_sizes: &metal,
            },
        );
        assert_eq!(nc_std.spec.taints.len(), 1);
        assert_eq!(nc_std.spec.taints[0].key, "rio.build/builder");
    }

    /// §13c T8: per-hwClass fleet-core sub-budget. The class cap counts
    /// live nodes summed across spot+od, AND cores already minted this
    /// tick for ANY cell of the class (so spot's spend subtracts from
    /// od's budget — per-hwClass not per-Cell, D4). `None` cap ⇒
    /// global-only.
    // r[verify ctrl.nodeclaim.budget.per-class]
    #[test]
    fn class_budget_sub_caps_per_hwclass() {
        use super::super::ffd::LiveNode;
        let live_node = |h: &str, cap: CapacityType, cores: u32| LiveNode {
            name: format!("{h}-{}", cap.as_str()),
            node_name: None,
            registered: true,
            cell: Some(Cell(h.into(), cap)),
            instance_type: None,
            allocatable: (cores, 0, 0),
            requested: (0, 0, 0),
            created_secs: Some(0.0),
            annotations: Default::default(),
            status: Default::default(),
        };
        // No class cap → global only.
        assert_eq!(class_budget(100, None, &[], "metal", 0), 100);
        // Class cap=50, no live, nothing created → min(100, 50) = 50.
        assert_eq!(class_budget(100, Some(50), &[], "metal", 0), 50);
        // Class cap < global remaining → class wins.
        // live: metal-spot 8c + metal-od 16c → class_live=24 (sums caps).
        let live = vec![
            live_node("metal", CapacityType::Spot, 8),
            live_node("metal", CapacityType::OnDemand, 16),
            // Different hw-class — must NOT count.
            live_node("std", CapacityType::Spot, 99),
        ];
        // 50 − 24 − 0 = 26 < global 100.
        assert_eq!(class_budget(100, Some(50), &live, "metal", 0), 26);
        // class_created accumulates: spot iteration minted 20 → od
        // budget = 50 − 24 − 20 = 6.
        assert_eq!(class_budget(100, Some(50), &live, "metal", 20), 6);
        // Saturating: created exceeds cap → 0.
        assert_eq!(class_budget(100, Some(50), &live, "metal", 999), 0);
        // Global remaining < class budget → global wins.
        assert_eq!(class_budget(5, Some(50), &live, "metal", 0), 5);
        // Cell-less nodes don't count.
        let mut nameless = live_node("metal", CapacityType::Spot, 999);
        nameless.cell = None;
        assert_eq!(class_budget(100, Some(50), &[nameless], "metal", 0), 50);
    }

    /// mb_024(2): fallback path is filtered through `masked` —
    /// hw-agnostic intent whose fallback cell is ICE-masked is
    /// `dropped` (NOT routed to a cell `cover_deficit` then skips,
    /// silently stranding cold-start probes).
    #[test]
    fn assign_fallback_filtered_by_masked() {
        let unplaced = [SpawnIntent {
            intent_id: "agn".into(),
            cores: 4,
            system: "x86_64-linux".into(),
            ready: Some(true),
            ..Default::default()
        }];
        let ref_cell = Cell("ref".into(), CapacityType::Spot);
        let fallback = |_: &SpawnIntent| Some(ref_cell.clone());
        // ref-spot ICE-masked → fallback filtered out → dropped.
        let masked: HashSet<Cell> = [ref_cell.clone()].into();
        let (by, d) = assign_to_cells(
            &unplaced,
            &CellSketches::default(),
            &masked,
            |_| 0.0,
            fallback,
        );
        assert!(by.is_empty(), "masked fallback must not appear in by_cell");
        assert_eq!(d, 1, "masked fallback counted in dropped");
    }
}
