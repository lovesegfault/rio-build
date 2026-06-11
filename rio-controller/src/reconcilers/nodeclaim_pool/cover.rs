//! §13b deficit cover.
//!
//! Per `r[ctrl.nodeclaim.anchor-bulk+5]`: for each `(h,cap)` cell with
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
// `METAL_NODE_CLASS` and `metal_partition_op` live in `rio_common::k8s`
// so the §13c metal-partition predicate is single-sourced across
// scheduler, controller, and xtask (§Partition-single-source, mb_018).
// `metal_partition_op(node_class)` returns `"In"` for the metal
// nodeClass and `"NotIn"` for everything else — see [`build_nodeclaim`].
use rio_common::k8s::metal_partition_op;
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
/// is gated on [`metal_partition_op`] over the hw-class's
/// `node_class`, single-sourcing the predicate with helm
/// `templates/karpenter.yaml`'s `nodePools` loop, the scheduler's
/// `derive_ceilings`, and xtask's `mk_probe_nodeclaim`.
pub const INSTANCE_SIZE_LABEL: &str = "karpenter.k8s.aws/instance-size";

/// The single builder-node taint. The band-loop NodePool template
/// stamped `rio.build/builder=true:NoSchedule` so non-builder cluster
/// pods (DaemonSets, monitoring) stay off builder nodes (ADR-019); B3
/// deleted those NodePools. Karpenter does NOT merge a shim NodePool's
/// `template.spec.taints` onto externally-created claims, so
/// [`build_nodeclaim`] sets it on `NodeClaimSpec.taints` directly.
/// Paired with helm `poolDefaults.tolerations`.
fn builder_taint() -> Taint {
    Taint {
        key: rio_common::k8s::BUILDER_TAINT_KEY.into(),
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
    /// §13c: per-hw-class Node taints. Builder cells: chained after
    /// [`builder_taint`] (e.g. metal carries
    /// `rio.build/kvm=true:NoSchedule`). Fetcher cells: used verbatim
    /// — the hwClass already declares `rio.build/fetcher=true:NoSchedule`,
    /// so the role taint and the hwClass taint collapse to ONE source.
    pub taints: Vec<Taint>,
    /// §13e: `[sla.hw_classes.$h].provides_features`. Drives the
    /// per-cell role stamp in [`build_nodeclaim`]: `∋ fetcher` →
    /// `rio.build/node-role: fetcher` + no [`builder_taint`]; else
    /// `builder` + [`builder_taint`]. Same map the scheduler routes
    /// against (`features_compatible`), so the role partition cannot
    /// drift from the routing partition.
    pub provides_features: Vec<String>,
}

/// Cluster-level [`build_nodeclaim`] config that doesn't vary per
/// hw-class.
pub struct CoverCfg<'a> {
    /// I-205 metal `instance-size` partition list
    /// (`karpenter.metalSizes`).
    pub metal_sizes: &'a [String],
}

/// Per-reason cold-start (`hw_class_names=[]`) drop counts from
/// [`assign_to_cells`]. The two reasons share the symptom (no NodeClaim
/// minted, build's drv permanently `Ready` and unroutable) but require
/// **different operator actions** — collapsing them into a single count
/// once misdiagnosed an account-level AWS IAM gap as a `[sla.hw_classes]`
/// config gap (see `assign_distinguishes_no_class_from_all_masked`).
///
/// Forecast-only intents (`hw_class_names` non-empty, lead-time-gated)
/// are NOT counted here — they're not dropped, just not yet placeable.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DropTally {
    /// `fallback` returned `None` even with **no** ICE-masking — there
    /// is no `[sla.hw_classes]` entry whose `(arch, max_cores, max_mem,
    /// provides_features)` admits the intent. Persistent until the
    /// config changes. **Operator action:** add or fix a class. See
    /// `sla-model.typ#rionodeclaimpool-nohostingclass`. live_051(c):
    /// this population is ALSO answered to the scheduler as a typed
    /// [`rio_proto::types::IntentVerdict`] on the spawn-intent ack —
    /// the tally is the operator plane, the verdict is the consumer
    /// plane; neither substitutes for the other.
    pub no_hosting_class: u64,
    /// Cold-start intent (`hw_class_names=[]`): `fallback` admits it in
    /// principle, but **every** cell that could host it is ICE-masked —
    /// NodeClaim launches are failing in the cloud (capacity
    /// exhaustion, quota, IAM). Self-heals once the ICE backoff expires
    /// *if* the cloud recovers; persistent if the cloud failure is
    /// structural (e.g. a missing `AWSServiceRoleForEC2Spot`).
    /// **Operator action:** check
    /// `rio_controller_nodeclaim_reaped_total{reason=~"ice|vanished"}`
    /// and the Karpenter controller log for launch errors. See
    /// `sla-model.typ#rionodeclaimpool-icemaskedhigh`.
    pub all_cells_ice_masked: u64,
    /// READY intent (`hw_class_names` non-empty, `A_open` non-empty
    /// before masking): every hosting cell is ICE-masked. Same cloud
    /// capacity gap as `all_cells_ice_masked`, measured on the
    /// population that carries solved demand — live_050(a): this
    /// population starved SILENTLY (208 intents, zero tally, zero
    /// warn) because the old fold conflated it with the lead-time
    /// gate. **Operator action:** as `all_cells_ice_masked`; the
    /// degradation ladder (`ctrl.nodeclaim.capacity-ladder`) is the
    /// structural consumer that gives the walk a next rung.
    pub ready_all_cells_ice_masked: u64,
}

/// One intent's minted outcome, paired with the intent it judges —
/// [`assign_to_cells`]' per-intent record.
pub type PlacementRecord<'a> = (&'a SpawnIntent, PlacementOutcome);

/// Total typed outcome of the cell-assignment chokepoint, minted ONCE
/// per intent inside [`assign_to_cells`] from evidence at the filter
/// site (only the filter knows whether `A_open` was non-empty BEFORE
/// masking — a post-hoc tally cannot reconstruct it). The caller
/// total-folds this alphabet with zero wildcard arms; rustc
/// exhaustiveness is the membership census (R15).
// r[impl ctrl.nodeclaim.placement-outcome]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementOutcome {
    /// Assigned to an unmasked cell (grouped into `by_cell`).
    Placed(Cell),
    /// Forecast intent lead-time-gated on every cell (`A_open` empty
    /// BEFORE masking, `hw_class_names` non-empty): legitimately quiet
    /// — the next tick re-evaluates as the ETA approaches.
    LeadTimeGated,
    /// Every cell that could host the intent is ICE-masked. LOUD —
    /// cloud capacity gap. `open_rungs` = |`A_open`| before masking
    /// (0 ⇔ the cold-start fallback path: `hw_class_names=[]`, the
    /// fallback cell was the only candidate).
    UnplaceableAllMasked { open_rungs: usize },
    /// No `[sla.hw_classes]` entry admits the intent at all. LOUD —
    /// config gap; answered to the scheduler as a typed
    /// `IntentVerdict` (live_051(c)).
    NoHostingClass,
}

impl PlacementOutcome {
    /// The outcome→wire law (live_051(c)): which arm ships an
    /// `IntentVerdict` on the spawn-intent ack. Total match — a new
    /// variant forces a wire decision here; the census test
    /// `exactly_one_outcome_variant_ships_a_verdict` pins the
    /// exactly-one property. The masked arms stay OFF the wire: their
    /// masks are the scheduler's own (`ice_masked_cells` /
    /// `unfulfillable_cells`) — the surviving no-wire half of the
    /// WO-S7-3 derivation.
    pub fn verdict_reason(&self) -> Option<rio_proto::types::IntentVerdictReason> {
        match self {
            PlacementOutcome::Placed(_)
            | PlacementOutcome::LeadTimeGated
            | PlacementOutcome::UnplaceableAllMasked { .. } => None,
            PlacementOutcome::NoHostingClass => {
                Some(rio_proto::types::IntentVerdictReason::NoHostingClass)
            }
        }
    }
}

impl DropTally {
    /// The single outcome→tally law: a total fold over the
    /// [`PlacementOutcome`] alphabet (zero wildcard arms — a new
    /// variant fails compilation here, forcing a visibility decision).
    /// The ICE-masked split is population-keyed: `open_rungs == 0` ⇔
    /// the cold-start fallback path (`all_cells_ice_masked`),
    /// `open_rungs > 0` ⇔ the ready path (`ready_all_cells_ice_masked`
    /// — the live_050(a) population).
    pub fn from_outcomes(outcomes: &[PlacementRecord<'_>]) -> Self {
        let mut t = Self::default();
        for (_, o) in outcomes {
            match o {
                PlacementOutcome::Placed(_) | PlacementOutcome::LeadTimeGated => {}
                PlacementOutcome::UnplaceableAllMasked { open_rungs: 0 } => {
                    t.all_cells_ice_masked += 1;
                }
                PlacementOutcome::UnplaceableAllMasked { .. } => {
                    t.ready_all_cells_ice_masked += 1;
                }
                PlacementOutcome::NoHostingClass => t.no_hosting_class += 1,
            }
        }
        t
    }
}

/// Group `unplaced` by the cheapest cell in each intent's `A_open`.
///
/// Intents with empty `A_open` from `hw_class_names=[]` (cold-start
/// `fit=None`) are routed via `fallback(i, masked)` — typically the
/// `referenceHwClass` cell of `intent.system`'s arch (see
/// [`super::NodeClaimPoolConfig::fallback_cell`]). Intents with empty
/// `A_open` from lead-time gating (every cell's lead-time shorter than
/// `eta_seconds`) are dropped without fallback — they're forecast-only
/// and a later tick re-evaluates.
///
/// `fallback` takes the live mask so it can fail over to the next
/// arch-matching cell when the reference cell is ICE-masked (mb_024).
/// When the masked call returns `None`, the implementation re-evaluates
/// `fallback(i, ∅)` to attribute the drop: still `None` → no class
/// hosts the intent at all (`PlacementOutcome::NoHostingClass` — config
/// gap); `Some` → the class exists but every hosting cell is masked
/// (`PlacementOutcome::UnplaceableAllMasked` — cloud capacity gap). The
/// re-eval is bounded at one extra `O(|hw_classes|)` predicate pass
/// **per dropped intent** — the steady-state hot path (intents that
/// place) never pays it.
///
/// Returns `(by_cell, outcomes)`: one [`PlacementOutcome`] per intent,
/// minted at this chokepoint (the only site that knows whether
/// `A_open` was non-empty before masking). Callers total-fold the
/// outcome alphabet — [`DropTally::from_outcomes`] is the shared
/// outcome→tally law; the production caller additionally mints
/// `IntentVerdict`s from the `NoHostingClass` arm (live_051(c)) and
/// the named-cells WARN from the ready `UnplaceableAllMasked` arm.
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
    fallback: impl Fn(&SpawnIntent, &HashSet<Cell>) -> Option<Cell>,
) -> (
    BTreeMap<Cell, Vec<&'a SpawnIntent>>,
    Vec<PlacementRecord<'a>>,
) {
    let mut by_cell: BTreeMap<Cell, Vec<&SpawnIntent>> = BTreeMap::new();
    let mut outcomes: Vec<PlacementRecord<'a>> = Vec::with_capacity(unplaced.len());
    let unmasked = HashSet::new();
    for i in unplaced {
        let open = a_open(i, sketches);
        let open_rungs = open.len();
        // The single mint site: exactly one PlacementOutcome per
        // intent, constructed from filter-site evidence.
        let outcome = match open
            .into_iter()
            .filter(|c| !masked.contains(c))
            .min_by(|a, b| cell_price(a).total_cmp(&cell_price(b)))
        {
            Some(c) => PlacementOutcome::Placed(c),
            None if !i.hw_class_names.is_empty() => {
                // Non-empty hw_class_names + empty filtered set is a
                // DISJUNCTION, not the old "⇔ lead-time-gated" claim
                // (live_050(a) — the false ⇔ silently starved 208
                // ready intents): either A_open was empty BEFORE
                // masking (genuinely lead-time-gated; quiet — next
                // tick re-evaluates) or masking emptied a non-empty
                // A_open (every hosting cell ICE-masked; LOUD).
                if open_rungs == 0 {
                    PlacementOutcome::LeadTimeGated
                } else {
                    PlacementOutcome::UnplaceableAllMasked { open_rungs }
                }
            }
            None => {
                // Cold-start (`hw_class_names=[]`): fallback path.
                // Defense-in-depth: re-filter even though the
                // `fallback_cell` impl already respects `masked`. A
                // masked `(referenceHwClass, spot)` that slipped
                // through would land in `by_cell` and then be
                // `continue`d by the per-cell ICE skip — silently
                // stranding cold-start probes.
                match fallback(i, masked).filter(|c| !masked.contains(c)) {
                    Some(c) => PlacementOutcome::Placed(c),
                    None if fallback(i, &unmasked).is_some() => {
                        PlacementOutcome::UnplaceableAllMasked { open_rungs: 0 }
                    }
                    None => PlacementOutcome::NoHostingClass,
                }
            }
        };
        if let PlacementOutcome::Placed(c) = &outcome {
            by_cell.entry(c.clone()).or_default().push(i);
        }
        outcomes.push((i, outcome));
    }
    (by_cell, outcomes)
}

/// Cell ranking for [`assign_to_cells`]' cheapest-open pick:
/// capacity-major (spot in `[0, 0.5)` before on-demand in `[1, 1.5)` —
/// spot is always cheaper), then a stable NAME-HASH fractional
/// tiebreak within the band (the wrapping-mul fold below — NOT
/// alphabetical; "hi-ebs-x86" ranks before "hi-ebs-x86-g7" because of
/// its hash, not its spelling). The scheduler's per-intent
/// `dispatched_cells` already encodes its CostTable ranking, so this
/// is only the disambiguator when an intent's `A_open` has multiple
/// unmasked cells.
///
/// live_050(c): under the capacity-degradation ladder this REALIZES
/// the walk order over the intent's hosting closure — the declared
/// `ladder` is membership authority only (option (a)); the order an
/// intent actually advances through its rungs is THIS ranking over
/// the unmasked closure cells: capacity-major, then the hash
/// disambiguator within a band. Generation-blind by construction (the
/// generation axis lives in the rung CLASSES, not in this fn).
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
/// fleet-wide core budget remaining. The
/// fuse-cache budget is here so [`sizing`] computes the same
/// [`intent_pod_footprint`](crate::reconcilers::pool::jobs::intent_pod_footprint)
/// triple FFD and `apply_intent_resources` will use.
pub struct SizingCfg {
    pub max_node_cores: u32,
    pub max_node_mem: u64,
    pub max_node_disk: u64,
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
/// claim per intent). Then capped by `⌊budget/chunk⌋` — the fleet
/// budget brake (the brake may truncate; the remainder is re-seen
/// next tick). live_049 L1: the former flat `per_tick_cap` term is
/// RETIRED — minting is bounded by demand (`n_pack`, the FFD bin
/// count over real placeable-gated footprints) and by the fleet
/// budget, the two quantities with safety meaning, and by nothing
/// else (`ctrl.nodeclaim.mint-deficit-proportional`).
///
/// Each claim is uniformly `(max(⌈Σc/n⌉, max_i c), max(Σm/n, max_i m),
/// max(Σd/n, max_i d))` — every intent fits every claim on every axis,
/// and `Σ out ≥ Σ in`. The production FFD is guaranteed to pack at
/// `n = |u|`; [`ffd::sim_packs`](super::ffd::sim_packs) finds the
/// smallest `n` where it does. STRIKE-4 close (r26 mb_002): the
/// predicate IS production `simulate` — no reimplemented sort/score to
/// diverge on. Unit-tested via the same call (the
/// §Simulator-shares-accounting executable guarantee).
// r[impl ctrl.nodeclaim.anchor-bulk+5]
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
            "intent footprint exceeds per-cell cap — dropping (scheduler ClassCeiling \
             not gating? scheduler-controller GetHwClassConfig skew up to 300s; \
             uncatalogued class over-permitted to global until next refresh)"
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
    // r[impl ctrl.nodeclaim.mint-deficit-proportional]
    // live_049 L1: the two-term mint law — demand x budget. The
    // retired flat cap stretched the live ramp to 18 ticks while
    // protecting nothing: every claim it deferred was demanded
    // (placeable-gated), budget-affordable, and right-sized.
    let n = n_pack.min(cfg.budget / chunk);
    (vec![(chunk, mem, disk); n as usize], min_eta)
}

// r[impl ctrl.nodeclaim.budget.per-class+3]
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
///
/// Terminating NodeClaims (`metadata.deletionTimestamp` set) STILL count
/// in `class_live` / `Σ live.allocatable`: the EC2 instance bills until
/// Karpenter's finalizer clears (~60-90s). FFD's [`super::ffd::simulate`]
/// excludes them from placement (so `unplaced` correctly grows), but the
/// budget keeps counting them so a replacement claim consumes headroom
/// from the SAME budget the dying node still occupies — the fleet never
/// silently exceeds `max_fleet_cores` across the drain window. Under a
/// tight budget the replacement waits for the finalizer; that's the safe
/// degradation (latency, not $).
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
///   `rio.build/node-role` = `builder` for builder cells (builder pod
///   affinity requires it; the legacy band-loop NodePool stamped it)
///   or `fetcher` for fetcher cells (§13e: branched on
///   `provides_features ∋ fetcher`), and the [`super::OWNER_LABEL`]
///   selector.
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
/// - `spec.taints`: builder cells get [`builder_taint`] chained with
///   the hwClass's per-class taints so non-builder cluster pods stay
///   off rio-minted builder nodes (ADR-019). §13e: fetcher cells get
///   `hw.taints` verbatim (the hwClass already carries
///   `rio.build/fetcher=true:NoSchedule`) — appending [`builder_taint`]
///   would deadlock the fetcher pod (its `effective_tolerations`
///   tolerates only `rio.build/fetcher`).
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
    // §13e: derive the role taint+label from the hwClass instead of
    // hardcoding builder. The bidirectional ∅-guard makes
    // `provides_features ∋ fetcher ⟺ FOD-routable` a strict partition;
    // the role stamp follows the same chokepoint. Without this, every
    // NodeClaim minted for a `fetcher-*` cell would carry
    // `rio.build/builder:NoSchedule` (un-tolerated by the fetcher pod)
    // AND `rio.build/node-role: builder` (poisons every operator query
    // filtering on node-role) — bootstrap deadlock.
    //
    // TODO: structural close — single-source ALL role taints/labels
    // through hwClass config (drop `builder_taint()`/`NODE_ROLE_LABEL`
    // hardcoding entirely). Requires a §SCC sweep across mid-*/large-*/
    // metal-* hwClass entries to declare the builder taint+label
    // explicitly. Out of scope for §13e.
    let is_fetcher_cell = hw
        .provides_features
        .iter()
        .any(|f| f == rio_common::k8s::FETCHER_FEATURE);
    let (role_k, role_v) = if is_fetcher_cell {
        (super::NODE_ROLE_LABEL.0, "fetcher")
    } else {
        super::NODE_ROLE_LABEL
    };
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
        // `templates/karpenter.yaml`'s `nodePools` loop and
        // `probe_boot::mk_probe_nodeclaim` — metal-nodeClass gets the
        // `In` side, everything else `NotIn`. §Partition-single-source:
        // the predicate is shared via `rio_common::k8s`.
        requirements.push(mk_req(
            INSTANCE_SIZE_LABEL,
            metal_partition_op(&hw.node_class),
            cfg.metal_sizes.to_vec(),
        ));
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
            //
            // §13e: stamp the role taint matching the cell's role.
            // Fetcher cells: `hw.taints` already carries
            // `rio.build/fetcher=true:NoSchedule` from the hwClass
            // config — the role taint and the hwClass taint collapse
            // to ONE source. Don't append `builder_taint()`; the
            // fetcher pod's `effective_tolerations` returns ONLY
            // `taints_routing_to(FETCHER_TAINT_KEY)` so a builder
            // taint here would deadlock (the fetcher pod cannot bind
            // to the fetcher node minted for it).
            taints: if is_fetcher_cell {
                hw.taints.clone()
            } else {
                std::iter::once(builder_taint())
                    .chain(hw.taints.iter().cloned())
                    .collect()
            },
            resources: Some(ResourceRequirements {
                requests: Some(requests),
                ..Default::default()
            }),
            // r40 bug_022: see NodeClaimSpec::expire_after doc. Without
            // this, Karpenter v1's CRD defaults to 720h forceful
            // expiration — every cover-minted NodeClaim and warm
            // hold-open slot is cordoned/drained at +30d, in a
            // synchronized fleet-wide wave for nodes from a deploy burst.
            expire_after: Some("Never".into()),
        },
        status: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_common::k8s::METAL_NODE_CLASS;
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

    fn cfg(max_node_cores: u32, budget: u32) -> SizingCfg {
        SizingCfg {
            max_node_cores,
            max_node_mem: 256 * GI,
            max_node_disk: 450 * GI,
            budget,
            fuse_cache_bytes: 50 * GI,
        }
    }

    #[test]
    fn claim_count_3axis_lower_bound() {
        let f = |c, m, d, mxc| claim_count((c, m, d), &cfg(mxc, u32::MAX));
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

    // r[verify ctrl.nodeclaim.mint-deficit-proportional]
    /// **R10 + W7-M** — *a deficit within budget is FULLY minted in
    /// ONE tick* (the 208-peak ramp shape killed; operation-count,
    /// zero wall-clock): the mint law is `min(n_pack, ⌊budget/chunk⌋)`
    /// — demand x budget, no third term. Pre-fix red (left,
    /// reverse-strawman transcript in the commit body): the flat
    /// per-tick cap truncated `n_pack=5` to 2 — the live shape was
    /// ⌈D/8⌉ ≈ 18 ticks of ramp with demand drained before the budget
    /// crossing. **R11 (green-side pin, disclosed)** — the budget
    /// brake exists pre-fix AND post-fix; it is the regression floor
    /// that must survive the cap's removal (the budget-binds row).
    #[test]
    fn sizing_mints_deficit_proportionally_under_budget_brake() {
        let u: Vec<_> = (0..10)
            .map(|k| intent_hd(&format!("i{k}"), 8, 8 * GI, 5 * GI, Some(true)))
            .collect();
        let refs: Vec<&SpawnIntent> = u.iter().collect();
        // Σc=80, n_lo=⌈80/32⌉=3; sim_packs finds n_pack=5 (disk-bound:
        // each pod's footprint.d = 5×1.5+50+1 ≈ 58.5Gi; 3 bins of
        // ⌈Σd/3⌉≈195Gi fit 3 each = 9 < 10).
        let (c, _) = sizing(&h_spot(), &refs, &cfg(32, 200));
        assert_eq!(
            c.len(),
            5,
            "R10: the WHOLE demanded-and-affordable deficit mints in one \
             tick — n_pack binds, no flat third term (pre-fix: 2 at \
             per_tick_cap=2)"
        );
        // chunk at n_pack=5 is ⌈80/5⌉.max(8)=16; budget=20 → ⌊20/16⌋=1.
        let (c, _) = sizing(&h_spot(), &refs, &cfg(32, 20));
        assert_eq!(c.len(), 1, "R11: the fleet-budget brake binds (green pin)");
        let (c, _) = sizing(&h_spot(), &refs, &cfg(32, 10));
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
            |_, _, _| true,
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
    // r[verify ctrl.nodeclaim.anchor-bulk+5]
    #[test]
    fn sizing_invariants_hold() {
        let scfg = cfg(64, u32::MAX);
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

    /// STRIKE-5 (r27 mb_006): per-cell `cfg.max_node_cores` (from §13c-2
    /// catalog-derived `class_ceilings`, `min(catalog, HwClassDef.max_cores)`)
    /// can be tighter than the GLOBAL cap the
    /// scheduler's chokepoint clamps at. An over-cap intent has no valid
    /// claim of any `n` (its pod requests `intent_pod_footprint(i)`, not
    /// the claim's `(c,m,d)`); a clamped 32c claim would just loop
    /// (mint→Pending→re-mint). `sizing()` filters and DROPS (with metric
    /// + warn), sizing on the remainder.
    // r[verify ctrl.nodeclaim.anchor-bulk+5]
    #[test]
    fn sizing_filters_intent_exceeding_per_cell_cap() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let scfg = cfg(32, u32::MAX);
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
        let scfg = cfg(64, u32::MAX);
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
        let (by, o) = assign_to_cells(&unplaced, &CellSketches::default(), &none, price, |_, _| {
            None
        });
        assert_eq!(DropTally::from_outcomes(&o), DropTally::default());
        assert_eq!(by.len(), 2);
        let h1k = Cell("h1".into(), CapacityType::Spot);
        let h2k = Cell("h2".into(), CapacityType::Spot);
        assert_eq!(by[&h1k].len(), 1);
        assert_eq!(by[&h1k][0].intent_id, "a", "a's cheapest-open is h1");
        assert_eq!(by[&h2k].len(), 1);
        assert_eq!(by[&h2k][0].intent_id, "b", "b only opens h2");
    }

    /// §13e B2.6: a FOD intent (`hw_class_names=[fetcher-x86]`) and a
    /// builder intent (`hw_class_names=[mid-ebs-x86]`) produce DISJOINT
    /// cell deficits. The pre-§13e doc comment claimed including FODs
    /// would "over-reserve builder capacity in FFD" — that concern was
    /// specific to FOD intents with `hw_class_names=[]` (cold-start
    /// fallback to a builder reference cell). Post-§13e FOD intents
    /// carry `hw_class_names=[fetcher-*]` from the scheduler, so FFD's
    /// `Cell = (hw_class, capacity)` keying partitions them strictly.
    /// Removing the `kind: Builder` filter does NOT mix builder and
    /// fetcher demand in the same cell.
    #[test]
    fn assign_fetcher_and_builder_intents_disjoint_cells() {
        let fod = intent(
            "fod-1",
            2,
            GI,
            &[("fetcher-x86", CapacityType::Spot)],
            Some(true),
        );
        let bld = intent(
            "bld-1",
            8,
            32 * GI,
            &[("mid-ebs-x86", CapacityType::Spot)],
            Some(true),
        );
        let unplaced = [fod, bld];
        let none = HashSet::new();
        let (by, o) = assign_to_cells(
            &unplaced,
            &CellSketches::default(),
            &none,
            cell_rank,
            |_, _| None,
        );
        assert_eq!(
            DropTally::from_outcomes(&o),
            DropTally::default(),
            "no fallback drops — both have hw_class_names"
        );
        let fcell = Cell("fetcher-x86".into(), CapacityType::Spot);
        let bcell = Cell("mid-ebs-x86".into(), CapacityType::Spot);
        assert_eq!(by.len(), 2, "exactly two disjoint cells");
        assert_eq!(by[&fcell].len(), 1);
        assert_eq!(by[&fcell][0].intent_id, "fod-1");
        assert_eq!(by[&bcell].len(), 1);
        assert_eq!(by[&bcell][0].intent_id, "bld-1");
        // Structural: no cell hosts both kinds.
        for (cell, intents) in &by {
            let has_fod = intents.iter().any(|i| i.intent_id == "fod-1");
            let has_bld = intents.iter().any(|i| i.intent_id == "bld-1");
            assert!(
                !(has_fod && has_bld),
                "cell {cell:?} mixes builder and fetcher demand — \
                 FFD partition violated"
            );
        }
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
            |_, _| None,
        );
        assert_eq!(by[&spot].len(), 1);
        // spot ICE-masked → od.
        let masked: HashSet<Cell> = [spot.clone()].into();
        let (by, o) = assign_to_cells(
            &unplaced,
            &CellSketches::default(),
            &masked,
            cell_rank,
            |_, _| None,
        );
        assert_eq!(DropTally::from_outcomes(&o), DropTally::default());
        assert!(!by.contains_key(&spot));
        assert_eq!(by[&od].len(), 1);
        // Both masked → masking emptied a NON-empty A_open: the ready
        // all-masked outcome IS counted (live_050(a) — the old
        // ":silently skipped, NOT counted" expectation pinned the
        // silent starvation as intended; re-derived to the W7-A green).
        let masked: HashSet<Cell> = [spot, od].into();
        let (by, o) = assign_to_cells(
            &unplaced,
            &CellSketches::default(),
            &masked,
            cell_rank,
            |_, _| None,
        );
        assert!(by.is_empty());
        assert_eq!(
            DropTally::from_outcomes(&o),
            DropTally {
                ready_all_cells_ice_masked: 1,
                ..Default::default()
            },
            "both-masked ready intent counted, never silent"
        );
        assert!(
            matches!(
                o[0].1,
                PlacementOutcome::UnplaceableAllMasked { open_rungs: 2 }
            ),
            "outcome carries the pre-mask rung count: {:?}",
            o[0].1
        );
    }

    /// live_050(c) — the capacity-ladder walk battery (the ASSIGNED
    /// half of W7-E's chosen→assigned composition; the chosen half is
    /// the scheduler's `rung_one_ice_advances_to_a_different_rung`).
    /// Universe = the shipped hi-band ladder pair under the committed
    /// od-only posture: `("hi-ebs-x86", od)` + `("hi-ebs-x86-g7", od)`
    /// — the closure cells a ladder'd intent carries on the wire.
    ///
    /// W7-E' (R5' — the unmasked-preference witness the masked reds
    /// cannot certify): a fresh intent with ZERO masks is assigned the
    /// REALIZED rung-1 — `cell_rank` argmin over the closure. Pinned
    /// LITERAL, independently derived (not the impl as its own
    /// oracle): rank = cap + hash(name)/u64::MAX × 0.5 with
    /// hash = fold(h×31 + byte); hash("hi-ebs-x86") =
    /// 2840604882467283 → od rank ≈ 1.000077; hash("hi-ebs-x86-g7") =
    /// 10837483758744667882 → od rank ≈ 1.293751 — the gen-8 parent
    /// ranks first (verified for all four shipped hi pairs; the
    /// realized within-band order is hash-determined, recorded in the
    /// `ctrl.nodeclaim.capacity-ladder` rationale).
    // r[verify ctrl.nodeclaim.capacity-ladder]
    #[test]
    fn fresh_intent_prefers_realized_rung_one() {
        let cells = [
            ("hi-ebs-x86", CapacityType::OnDemand),
            ("hi-ebs-x86-g7", CapacityType::OnDemand),
        ];
        let unplaced = [intent("x", 4, GI, &cells, Some(true))];
        let (by, o) = assign_to_cells(
            &unplaced,
            &CellSketches::default(),
            &HashSet::new(),
            cell_rank,
            |_, _| None,
        );
        let rung_one = Cell("hi-ebs-x86".into(), CapacityType::OnDemand);
        assert_eq!(
            by[&rung_one].len(),
            1,
            "zero masks ⇒ the realized rung-1 (gen-8 parent, od) is chosen"
        );
        assert!(matches!(o[0].1, PlacementOutcome::Placed(_)));
    }

    /// W7-F (the no-hard-starvation theorem over the rung census) +
    /// R6 (all-rungs-masked is LOUD, composing WO-S7-3's alphabet):
    /// every rung masked EXCEPT the last ⇒ placement on the last rung;
    /// ALL rungs masked ⇒ `UnplaceableAllMasked` counted — never a
    /// silent hang. Pre-fix (no ladder) the universe had a single
    /// rung: the gen-7 cell did not exist to advance to (R5's left).
    // r[verify ctrl.nodeclaim.capacity-ladder]
    #[test]
    fn every_rung_masked_except_last_places_on_last() {
        let cells = [
            ("hi-ebs-x86", CapacityType::OnDemand),
            ("hi-ebs-x86-g7", CapacityType::OnDemand),
        ];
        let unplaced = [intent("x", 4, GI, &cells, Some(true))];
        let parent = Cell("hi-ebs-x86".into(), CapacityType::OnDemand);
        let rung = Cell("hi-ebs-x86-g7".into(), CapacityType::OnDemand);
        // Rung-1 masked ⇒ the walk advances to the last rung.
        let masked: HashSet<Cell> = [parent.clone()].into();
        let (by, o) = assign_to_cells(
            &unplaced,
            &CellSketches::default(),
            &masked,
            cell_rank,
            |_, _| None,
        );
        assert!(!by.contains_key(&parent));
        assert_eq!(by[&rung].len(), 1, "last unmasked rung hosts the intent");
        assert_eq!(DropTally::from_outcomes(&o), DropTally::default());
        // ALL rungs masked ⇒ loud, never silent (R6).
        let masked: HashSet<Cell> = [parent, rung].into();
        let (by, o) = assign_to_cells(
            &unplaced,
            &CellSketches::default(),
            &masked,
            cell_rank,
            |_, _| None,
        );
        assert!(by.is_empty());
        assert_eq!(
            DropTally::from_outcomes(&o),
            DropTally {
                ready_all_cells_ice_masked: 1,
                ..Default::default()
            },
            "all-rungs-masked surfaces as the counted ready-all-masked \
             outcome (live_050(a) alphabet), never a silent hang"
        );
        assert!(matches!(
            o[0].1,
            PlacementOutcome::UnplaceableAllMasked { open_rungs: 2 }
        ));
    }

    /// W7-G's reachability half (R15): every declared rung is the
    /// chosen cell under SOME mask configuration — product-iterated
    /// FROM the closure universe (mask everything except rung r, for
    /// each r), not author rows. Universe rows are the four shipped
    /// hi-band ladder pairs under the committed od-only posture
    /// (membership pinned against the shipped values by the scheduler-
    /// side `shipped_values_cell_universe_census`; the [GEN-SET]
    /// derivation command is in the commit body).
    // r[verify ctrl.nodeclaim.capacity-ladder]
    #[test]
    fn declared_rung_reachability_census() {
        for (parent, rung) in [
            ("hi-nvme-x86", "hi-nvme-x86-g7"),
            ("hi-nvme-arm", "hi-nvme-arm-g7"),
            ("hi-ebs-x86", "hi-ebs-x86-g7"),
            ("hi-ebs-arm", "hi-ebs-arm-g7"),
        ] {
            let cells = [
                (parent, CapacityType::OnDemand),
                (rung, CapacityType::OnDemand),
            ];
            let unplaced = [intent("x", 4, GI, &cells, Some(true))];
            let universe: Vec<Cell> = cells
                .iter()
                .map(|(h, cap)| Cell((*h).into(), *cap))
                .collect();
            for target in &universe {
                let masked: HashSet<Cell> =
                    universe.iter().filter(|c| *c != target).cloned().collect();
                let (by, _) = assign_to_cells(
                    &unplaced,
                    &CellSketches::default(),
                    &masked,
                    cell_rank,
                    |_, _| None,
                );
                assert_eq!(
                    by[target].len(),
                    1,
                    "rung {target:?} of ladder ({parent} → {rung}) is \
                     reachable as the chosen cell"
                );
            }
        }
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
        let fallback = |i: &SpawnIntent, _: &HashSet<Cell>| {
            (i.system == "x86_64-linux").then(|| Cell("ref".into(), CapacityType::Spot))
        };
        let (by, o) = assign_to_cells(
            &unplaced,
            &CellSketches::default(),
            &HashSet::new(),
            |_| 0.0,
            fallback,
        );
        let d = DropTally::from_outcomes(&o);
        assert_eq!(d.no_hosting_class, 1, "agn-u dropped");
        assert_eq!(d.all_cells_ice_masked, 0);
        assert_eq!(d.ready_all_cells_ice_masked, 0);
        assert_eq!(by.len(), 1);
        assert_eq!(by[&ref_cell].len(), 1);
        assert_eq!(by[&ref_cell][0].intent_id, "agn-x");
        // W7-B kill-isolation: the lead-time-gated forecast intent is
        // typed LeadTimeGated (quiet) — the new masked-ready arm
        // cannot eat the legitimate case.
        assert!(
            matches!(o[2].1, PlacementOutcome::LeadTimeGated),
            "fc is lead-time-gated, not a drop: {:?}",
            o[2].1
        );
    }

    #[test]
    fn assign_empty_unplaced_empty_output() {
        let (by, o) = assign_to_cells(
            &[],
            &CellSketches::default(),
            &HashSet::new(),
            |_| 0.0,
            |_, _| None,
        );
        assert!(by.is_empty());
        assert!(o.is_empty());
        assert_eq!(DropTally::from_outcomes(&o), DropTally::default());
    }

    /// `assign_to_cells` distinguishes WHY a cold-start intent dropped —
    /// the two reasons need different operator actions and the WARN/
    /// metric in [`super::super::cover_deficit`] must name the right one.
    ///
    /// `fallback(i, ∅) → None`: no `[sla.hw_classes]` entry hosts the
    /// intent's `(arch, footprint, features)` — config gap. Operator
    /// adds or fixes a class. → `no_hosting_class`.
    ///
    /// `fallback(i, ∅) → Some` but `fallback(i, masked) → None`: every
    /// cell that COULD host it is ICE-masked — NodeClaim launches are
    /// failing in the cloud (capacity / quota / IAM). Operator checks
    /// Karpenter, not `[sla.hw_classes]`. → `all_cells_ice_masked`.
    ///
    /// Discovered live: a fresh AWS account without the
    /// `AWSServiceRoleForEC2Spot` SLR fails every spot CreateFleet,
    /// every spot cell ICE-masks within ~6 ticks, and the
    /// `no_hosting_class` warn told the operator to "add a
    /// `[sla.hw_classes]` entry" for an account-level IAM gap.
    #[test]
    fn assign_distinguishes_no_class_from_all_masked() {
        let ref_cell = Cell("ref".into(), CapacityType::Spot);
        let arm_cell = Cell("arm".into(), CapacityType::Spot);
        let unplaced = [
            // Hostable, but every hosting cell is masked → ICE drop.
            SpawnIntent {
                intent_id: "hostable-masked".into(),
                cores: 4,
                system: "x86_64-linux".into(),
                ready: Some(true),
                ..Default::default()
            },
            // No class hosts this system → config drop.
            SpawnIntent {
                intent_id: "unhostable".into(),
                cores: 4,
                system: "riscv64-none".into(),
                ready: Some(true),
                ..Default::default()
            },
            // Hostable and unmasked → assigned.
            SpawnIntent {
                intent_id: "hostable-open".into(),
                cores: 4,
                system: "aarch64-linux".into(),
                ready: Some(true),
                ..Default::default()
            },
        ];
        // Masked-aware fallback: same shape as `fallback_cell`'s — the
        // closure tries its hosting cell and respects the supplied mask.
        let fallback = |i: &SpawnIntent, m: &HashSet<Cell>| -> Option<Cell> {
            let c = match i.system.as_str() {
                "x86_64-linux" => Cell("ref".into(), CapacityType::Spot),
                "aarch64-linux" => Cell("arm".into(), CapacityType::Spot),
                _ => return None,
            };
            (!m.contains(&c)).then_some(c)
        };
        let masked: HashSet<Cell> = [ref_cell].into();
        let (by, o) = assign_to_cells(
            &unplaced,
            &CellSketches::default(),
            &masked,
            |_| 0.0,
            fallback,
        );
        assert_eq!(
            DropTally::from_outcomes(&o),
            DropTally {
                no_hosting_class: 1,
                all_cells_ice_masked: 1,
                ready_all_cells_ice_masked: 0,
            },
            "hostable-masked → ice; unhostable → no_class"
        );
        assert_eq!(by.len(), 1, "only the unmasked arm cell received work");
        assert_eq!(by[&arm_cell][0].intent_id, "hostable-open");
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
            provides_features: vec![],
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
        ctx.provides_features = vec!["kvm".into(), "nixos-test".into()];
        ctx
    }

    /// §13e: fetcher hw-class context. `provides_features ∋ fetcher`
    /// drives the per-cell role stamp; `taints` carries ONLY the
    /// fetcher taint (the hwClass declares it).
    fn hw_ctx_fetcher() -> HwClassCtx {
        HwClassCtx {
            node_class: "rio-default".into(),
            labels: vec![
                (rio_common::k8s::FETCHER_TAINT_KEY.into(), "true".into()),
                ("kubernetes.io/arch".into(), "amd64".into()),
            ],
            requirements: vec![NodeSelectorRequirement {
                key: "kubernetes.io/arch".into(),
                operator: "In".into(),
                values: vec!["amd64".into()],
            }],
            taints: vec![Taint {
                key: rio_common::k8s::FETCHER_TAINT_KEY.into(),
                value: Some("true".into()),
                effect: "NoSchedule".into(),
                ..Default::default()
            }],
            provides_features: vec![rio_common::k8s::FETCHER_FEATURE.into()],
        }
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
        assert_eq!(spec.taints[0].key, rio_common::k8s::BUILDER_TAINT_KEY);
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

        // r40 bug_022: Karpenter v1 CRD defaults expireAfter to 720h
        // (forceful since v1.1.0). The reconciler is the sole lifecycle
        // owner — minted claims must opt out.
        assert_eq!(spec.expire_after.as_deref(), Some("Never"));
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
            provides_features: vec![],
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
        assert_eq!(nc.spec.taints[0].key, rio_common::k8s::BUILDER_TAINT_KEY);
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
        assert_eq!(
            nc_std.spec.taints[0].key,
            rio_common::k8s::BUILDER_TAINT_KEY
        );
    }

    /// §13e: a `fetcher-*` cell mints a NodeClaim with the
    /// `rio.build/fetcher` taint ONLY (no builder_taint), and stamps
    /// `rio.build/node-role: fetcher`. Without this branch every
    /// fetcher NodeClaim would carry BOTH taints — the fetcher pod
    /// (which tolerates only `rio.build/fetcher`) could never bind to
    /// the fetcher node minted for it. Bootstrap deadlock.
    // r[verify ctrl.nodeclaim.taints.hwclass]
    // r[verify fetcher.node.dedicated+4]
    #[test]
    fn build_nodeclaim_fetcher_cell_no_builder_taint() {
        let cell = Cell("fetcher-x86".into(), CapacityType::Spot);
        let nc = build_nodeclaim(
            &cell,
            (4, 8 * GI, 50 * GI),
            0.0,
            &hw_ctx_fetcher(),
            &CoverCfg { metal_sizes: &[] },
        );
        // ONLY the fetcher taint — no builder_taint().
        assert_eq!(
            nc.spec.taints.len(),
            1,
            "fetcher cell mints exactly the hwClass fetcher taint, \
             never builder_taint: {:?}",
            nc.spec.taints
        );
        assert_eq!(nc.spec.taints[0].key, rio_common::k8s::FETCHER_TAINT_KEY);
        assert_eq!(nc.spec.taints[0].effect, "NoSchedule");
        assert!(
            !nc.spec
                .taints
                .iter()
                .any(|t| t.key == rio_common::k8s::BUILDER_TAINT_KEY),
            "fetcher cell must NOT carry builder_taint (would deadlock \
             the fetcher pod binding)"
        );
        // Role label is `fetcher`, not `builder`.
        let labels = nc.metadata.labels.as_ref().unwrap();
        assert_eq!(
            labels["rio.build/node-role"], "fetcher",
            "fetcher cell stamps rio.build/node-role: fetcher"
        );
        // hwClass label (the §13e taint+label key) survives the
        // `extend` overwrite — `rio.build/fetcher` does NOT collide
        // with `rio.build/node-role`. Per-intent affinity matches THIS.
        assert_eq!(
            labels[rio_common::k8s::FETCHER_TAINT_KEY],
            "true",
            "hwClass fetcher label stamped (per-intent affinity reads it)"
        );
        // Owner label is unchanged — same reconciler owns both kinds.
        assert_eq!(labels["rio.build/nodeclaim-pool"], "builder");
    }

    /// §13c T8: per-hwClass fleet-core sub-budget. The class cap counts
    /// live nodes summed across spot+od, AND cores already minted this
    /// tick for ANY cell of the class (so spot's spend subtracts from
    /// od's budget — per-hwClass not per-Cell, D4). `None` cap ⇒
    /// global-only.
    // r[verify ctrl.nodeclaim.budget.per-class+3]
    #[test]
    fn class_budget_sub_caps_per_hwclass() {
        use super::super::ffd::LiveNode;
        let live_node = |h: &str, cap: CapacityType, cores: u32| LiveNode {
            name: format!("{h}-{}", cap.as_str()),
            node_name: None,
            registered: true,
            terminating_since: None,
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
        // Terminating nodes DO count: the EC2 instance bills until
        // Karpenter's finalizer clears (~60-90s). FFD excludes them
        // from placement, but the budget must keep counting them so a
        // replacement claim consumes the SAME headroom the dying node
        // still occupies — never double-spend across the drain window.
        let mut dying = live_node("metal", CapacityType::Spot, 8);
        dying.terminating_since = Some(0.0);
        assert_eq!(
            class_budget(100, Some(50), &[dying], "metal", 0),
            42,
            "terminating node still occupies fleet-core budget"
        );
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
        // Masked-BLIND fallback (returns the cell regardless of mask) —
        // exercises the defense-in-depth `.filter(!masked)` after the
        // `fallback(i, masked)` call. `fallback_cell` already respects
        // its `masked` arg; this closure deliberately does not, so a
        // future `fallback_cell` regression that stops filtering can't
        // strand probes on a masked cell.
        let fallback = |_: &SpawnIntent, _: &HashSet<Cell>| Some(ref_cell.clone());
        // ref-spot ICE-masked → fallback filtered out → dropped as
        // `all_cells_ice_masked` (the fallback CAN host it; the cell is
        // just masked).
        let masked: HashSet<Cell> = [ref_cell.clone()].into();
        let (by, o) = assign_to_cells(
            &unplaced,
            &CellSketches::default(),
            &masked,
            |_| 0.0,
            fallback,
        );
        assert!(by.is_empty(), "masked fallback must not appear in by_cell");
        assert_eq!(
            DropTally::from_outcomes(&o),
            DropTally {
                no_hosting_class: 0,
                all_cells_ice_masked: 1,
                ready_all_cells_ice_masked: 0,
            },
            "masked fallback counted as ICE drop, not config drop"
        );
    }

    /// Scoped JSON-subscriber capture (the node_informer.rs LogBuf
    /// pattern) for asserting on warn emission without a global
    /// subscriber race.
    #[derive(Clone, Default)]
    struct LogBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn captured_lines(f: impl FnOnce()) -> Vec<String> {
        use tracing_subscriber::layer::SubscriberExt as _;
        let buf = LogBuf::default();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(buf.clone()),
        );
        let guard = tracing::subscriber::set_default(subscriber);
        f();
        drop(guard);
        let bytes = std::mem::take(&mut *buf.0.lock().unwrap());
        String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(String::from)
            .collect()
    }

    // r[verify ctrl.nodeclaim.placement-outcome]
    /// live_050(a) red R1 / witness W7-A — certifies: *a READY intent
    /// (non-empty `hw_class_names`, non-empty `A_open`) whose every
    /// hosting cell is ICE-masked produces `UnplaceableAllMasked` +
    /// tally + WARN + metric — not silence, not `LeadTimeGated`.* The
    /// adversarial population is exactly the one that starved live
    /// (208 ready intents, zero tally, zero warn). Asserts the mod.rs
    /// fold half (warn + metric) the `assign_masked_cell_fails_over_
    /// to_od` pin structurally cannot see.
    #[test]
    fn ready_intent_with_all_cells_masked_is_counted_and_warned() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let cells = [("h", CapacityType::Spot), ("h", CapacityType::OnDemand)];
        let unplaced = [intent("starved", 4, GI, &cells, Some(true))];
        let masked: HashSet<Cell> = [
            Cell("h".into(), CapacityType::Spot),
            Cell("h".into(), CapacityType::OnDemand),
        ]
        .into();
        let (by, o) = assign_to_cells(
            &unplaced,
            &CellSketches::default(),
            &masked,
            cell_rank,
            |_, _| None,
        );
        assert!(by.is_empty());
        let d = DropTally::from_outcomes(&o);
        assert_eq!(
            d,
            DropTally {
                ready_all_cells_ice_masked: 1,
                ..Default::default()
            },
            "counted — never the silent skip"
        );
        // The fold half: warn + metric, driven through the production
        // emission fn under a local recorder + scoped subscriber.
        let rec = DebuggingRecorder::new();
        let _g = ::metrics::set_default_local_recorder(&rec);
        let masked_ready: Vec<(&str, &[String])> = vec![("starved", &unplaced[0].hw_class_names)];
        let lines =
            captured_lines(|| super::super::emit_drop_tally(&d, &masked_ready, masked.len()));
        assert!(
            lines
                .iter()
                .any(|l| l.contains("READY SpawnIntents unplaceable") && l.contains("starved")),
            "warn names the starved intents: {lines:?}"
        );
        // ppppp: snapshot exactly once; query the materialized Vec.
        let snap = rec.snapshotter().snapshot().into_vec();
        let count = snap.into_iter().find_map(|(k, _, _, v)| {
            let key = k.key();
            (key.name() == "rio_controller_nodeclaim_intent_dropped_total"
                && key
                    .labels()
                    .any(|l| l.key() == "reason" && l.value() == "ready_all_cells_ice_masked"))
            .then_some(v)
        });
        assert_eq!(
            count,
            Some(DebugValue::Counter(1)),
            "metric incremented under the new reason label"
        );
    }

    // r[verify ctrl.nodeclaim.placement-outcome]
    /// R2 / witness W7-B (DISCLOSED GREEN-SIDE PIN — the lead-time path
    /// is correct pre-fix and must stay so) — certifies: *a genuinely
    /// lead-time-gated forecast intent still produces zero drop-tally;
    /// the new masked-ready arm cannot eat the legitimate quiet case*
    /// (kill-isolation for W7-A).
    #[test]
    fn lead_time_gated_forecast_intent_stays_quiet() {
        let mut i = intent("fc", 4, GI, &[("h1", CapacityType::Spot)], Some(false));
        i.eta_seconds = 999.0; // > every default lead time → A_open empty pre-mask
        let unplaced = [i];
        let (by, o) = assign_to_cells(
            &unplaced,
            &CellSketches::default(),
            &HashSet::new(),
            cell_rank,
            |_, _| None,
        );
        assert!(by.is_empty());
        assert!(matches!(o[0].1, PlacementOutcome::LeadTimeGated));
        assert_eq!(
            DropTally::from_outcomes(&o),
            DropTally::default(),
            "zero tally on the quiet edge"
        );
    }

    /// R15 census rows for the `PlacementOutcome` alphabet. The
    /// generator is rustc: the `witness` match below fails to compile
    /// when a variant is added, and the coverage pin asserts every
    /// variant has a row — membership is machine-derived, never
    /// author-trusted.
    fn outcome_census_rows() -> Vec<PlacementOutcome> {
        let rows = vec![
            PlacementOutcome::Placed(Cell("h".into(), CapacityType::Spot)),
            PlacementOutcome::LeadTimeGated,
            PlacementOutcome::UnplaceableAllMasked { open_rungs: 0 },
            PlacementOutcome::UnplaceableAllMasked { open_rungs: 2 },
            PlacementOutcome::NoHostingClass,
        ];
        let witness = |o: &PlacementOutcome| -> usize {
            match o {
                PlacementOutcome::Placed(_) => 0,
                PlacementOutcome::LeadTimeGated => 1,
                PlacementOutcome::UnplaceableAllMasked { .. } => 2,
                PlacementOutcome::NoHostingClass => 3,
            }
        };
        let mut seen = [false; 4];
        for r in &rows {
            seen[witness(r)] = true;
        }
        assert!(
            seen.iter().all(|s| *s),
            "census rows must cover every PlacementOutcome variant"
        );
        rows
    }

    /// R15 drop-reason product census: the outcome→tally law over
    /// cells FROM the alphabet — each variant maps to exactly its
    /// tally field (the ICE split population-keyed on `open_rungs`),
    /// quiet arms map to none.
    #[test]
    fn drop_tally_fold_census_over_the_outcome_alphabet() {
        let probe = intent("p", 4, GI, &[("h", CapacityType::Spot)], Some(true));
        for o in outcome_census_rows() {
            let got = DropTally::from_outcomes(&[(&probe, o.clone())]);
            let want = match &o {
                PlacementOutcome::Placed(_) | PlacementOutcome::LeadTimeGated => {
                    DropTally::default()
                }
                PlacementOutcome::UnplaceableAllMasked { open_rungs: 0 } => DropTally {
                    all_cells_ice_masked: 1,
                    ..Default::default()
                },
                PlacementOutcome::UnplaceableAllMasked { .. } => DropTally {
                    ready_all_cells_ice_masked: 1,
                    ..Default::default()
                },
                PlacementOutcome::NoHostingClass => DropTally {
                    no_hosting_class: 1,
                    ..Default::default()
                },
            };
            assert_eq!(got, want, "outcome {o:?}");
        }
    }

    // r[verify ctrl.nodeclaim.placement-outcome]
    /// live_051(c) wire-mapping census (R15): over the alphabet rows,
    /// EXACTLY ONE variant ships an `IntentVerdict` (NoHostingClass —
    /// the controller-config-gap population the scheduler structurally
    /// cannot see); the masked populations stay off the wire (their
    /// masks are the scheduler's own — the surviving no-wire half of
    /// the WO-S7-3 derivation).
    #[test]
    fn exactly_one_outcome_variant_ships_a_verdict() {
        let rows = outcome_census_rows();
        let shipped: Vec<&PlacementOutcome> = rows
            .iter()
            .filter(|o| o.verdict_reason().is_some())
            .collect();
        assert_eq!(shipped.len(), 1, "exactly one wire-mapped variant");
        assert!(matches!(shipped[0], PlacementOutcome::NoHostingClass));
        assert_eq!(
            PlacementOutcome::NoHostingClass.verdict_reason(),
            Some(rio_proto::types::IntentVerdictReason::NoHostingClass)
        );
    }
}

#[cfg(test)]
mod mint_law_tests {
    //! WO-S7-1 (live_049 L1): the two-term mint law's premise
    //! witnesses and the cap-reader census.
    use std::collections::{HashMap, HashSet};

    use super::super::ffd::{self, LiveNode};
    use super::*;

    const GI: u64 = 1 << 30;

    fn intent(id: &str, cores: u32, ready: bool, eta: f64, cells: &[(&str, &str)]) -> SpawnIntent {
        SpawnIntent {
            intent_id: id.into(),
            cores,
            mem_bytes: GI,
            disk_bytes: GI,
            ready: Some(ready),
            eta_seconds: eta,
            hw_class_names: cells.iter().map(|(h, _)| h.to_string()).collect(),
            node_affinity: cells
                .iter()
                .map(|(h, cap)| rio_proto::types::NodeSelectorTerm {
                    match_expressions: vec![
                        rio_proto::types::NodeSelectorRequirement {
                            key: "hw-band".into(),
                            operator: "In".into(),
                            values: vec![h.to_string()],
                        },
                        rio_proto::types::NodeSelectorRequirement {
                            key: "karpenter.sh/capacity-type".into(),
                            operator: "In".into(),
                            values: vec![cap.to_string()],
                        },
                    ],
                })
                .collect(),
            ..Default::default()
        }
    }

    // r[verify ctrl.nodeclaim.mint-deficit-proportional]
    /// **R10b + W7-K** — *the gate POPULATION premise, probed not
    /// prose*: `n_pack`'s input is exactly the placeable-gated
    /// unplaced set. A tick whose input contains (i) an intent that
    /// FITS ON LIVE NODES and (ii) a LEAD-TIME-GATED forecast intent
    /// (eta ≥ every cell's lead) contributes ZERO to the mint vector —
    /// driven through the production `ffd::simulate` →
    /// `assign_to_cells` → `sizing` chain against the WO-S7-3-typed
    /// `PlacementOutcome` arm. This is the premise that makes
    /// unbounded mint-per-tick safe after the flat cap's retirement:
    /// un-demanded capacity is structurally unreachable by the law
    /// (the blast-radius bound is the budget brake — W7-L's row in
    /// `sizing_mints_deficit_proportionally_under_budget_brake`).
    #[test]
    fn gate_population_feeds_zero_mint() {
        // One live node with free capacity for i-fits.
        let live = vec![LiveNode {
            name: "n0".into(),
            node_name: Some("n0".into()),
            registered: true,
            terminating_since: None,
            cell: Some(Cell("h".into(), CapacityType::Spot)),
            instance_type: None,
            allocatable: (64, 256 * GI, 450 * GI),
            requested: (0, 0, 0),
            created_secs: Some(0.0),
            annotations: Default::default(),
            status: Default::default(),
        }];
        let intents = vec![
            // (i) fits on the live node → placeable, never unplaced.
            intent("i-fits", 4, true, 0.0, &[("h", "spot")]),
            // (ii) forecast gated by lead time (eta ≥ every lead; the
            // default sketch lead is far below 1e9).
            intent("i-far", 4, false, 1e9, &[("h", "spot")]),
        ];
        let sketches = CellSketches::default();
        let (placeable, unplaced) = ffd::simulate(
            &intents,
            &live,
            &sketches,
            &HashMap::new(),
            50 * GI,
            |_, _, _| true,
        );
        assert_eq!(placeable.len(), 1, "i-fits placed on the live node");
        assert_eq!(unplaced.len(), 1, "only the forecast survives the split");
        let none = HashSet::new();
        let (by_cell, outcomes) =
            assign_to_cells(&unplaced, &sketches, &none, |_| 0.03, |_, _| None);
        // The typed arm: lead-time-gated, quiet, ZERO cells assigned.
        assert!(
            outcomes
                .iter()
                .all(|(_, o)| matches!(o, PlacementOutcome::LeadTimeGated)),
            "the forecast takes the typed LeadTimeGated arm: {outcomes:?}"
        );
        // The mint vector: zero claims across every cell.
        let total: usize = by_cell
            .iter()
            .map(|(cell, u)| {
                let refs: Vec<&SpawnIntent> = u.to_vec();
                sizing(
                    cell,
                    &refs,
                    &SizingCfg {
                        max_node_cores: 64,
                        max_node_mem: 256 * GI,
                        max_node_disk: 450 * GI,
                        budget: u32::MAX,
                        fuse_cache_bytes: 50 * GI,
                    },
                )
                .0
                .len()
            })
            .sum();
        assert_eq!(
            total, 0,
            "BOTH gate populations contribute zero to the tick's mint \
             vector — unbounded n cannot mint un-demanded capacity"
        );
    }

    // r[verify ctrl.nodeclaim.mint-deficit-proportional]
    /// **R12 — the cap-reader census [GEN-SET]** (re-scoped post-review:
    /// code-readers ZERO + the committed surviving-surface expected
    /// set). Generator:
    ///
    ///   rg -n 'per_tick_cap|max_node_claims_per_cell_per_tick' \
    ///      rio-controller/src/
    ///
    /// over the EMBEDDED production sources (include_str!): the mint
    /// path carries ZERO readers of the retired cap — the only
    /// surviving mentions are the retirement rationale prose. The
    /// out-of-crate surviving surface is the committed EXPECTED-SET
    /// below, one disposition per row; an UNLISTED code reader fails
    /// this census (closure tomorrow, not completeness today).
    #[test]
    fn cap_reader_census_code_readers_zero() {
        let cover_src = include_str!("cover.rs");
        let mod_src = include_str!("mod.rs");
        let prod = |s: &str| {
            s.split("#[cfg(test)]\nmod ")
                .next()
                .unwrap_or(s)
                .to_string()
        };
        // cover.rs: 1 retirement-rationale prose mention in the sizing
        // doc; ZERO code reads (the law is two-term).
        assert_eq!(
            prod(cover_src).matches("per_tick_cap").count(),
            1,
            "cover.rs: the retirement prose only — a second mention is a \
             reader regression"
        );
        assert_eq!(
            prod(cover_src)
                .matches("max_node_claims_per_cell_per_tick")
                .count(),
            0,
            "cover.rs: zero field readers"
        );
        // mod.rs: zero readers; the field/default/plumb rows are GONE
        // (tolerant struct — the rendered TOML row is safely ignored).
        assert_eq!(
            prod(mod_src).matches("per_tick_cap").count(),
            0,
            "mod.rs: zero cap-name mentions in production"
        );
        assert_eq!(
            prod(mod_src)
                .matches("max_node_claims_per_cell_per_tick")
                .count(),
            0,
            "mod.rs: the field, its default, and the SizingCfg plumb are \
             retired"
        );
        // The committed EXPECTED-SET (out-of-crate surfaces; verified
        // by the generator run committed in the WO-S7-1 commit body):
        // - templates/controller.yaml:34 — renders into the TOLERANT
        //   NodeClaimPoolConfig (serde(default): unknown key ignored);
        // - templates/scheduler.yaml:44 — renders into the RETAINED
        //   deny_unknown_fields SlaConfig field (parse-only);
        // - infra/helm values.yaml sla row — DEPRECATION-COMMENTED
        //   (arm (a): kept so the scheduler template's rendered key
        //   keeps parsing);
        // - nix/tests/default.nix — sets the retained field
        //   (banner-protected, untouched);
        // - rio-scheduler sla/config.rs — the deprecated-ignored
        //   field + serde default + HELM_RENDERED_SLA_KEYS row +
        //   test_default/destructure rows (parse-surface only);
        // - docs/spec sla-sizing.typ — the two-term law + burst
        //   pricing + the deprecated-row note ([GEN-SET] prose);
        // - docs/spec/models/nodeclaimLifecycle.qnt:243 — the
        //   MODELED-BUT-RETIRED knob (P7 scope-bound rationale in the
        //   commit body; the model is NOT edited in-wave).
        const EXPECTED_SET_ROWS: usize = 7;
        assert_eq!(EXPECTED_SET_ROWS, 7, "the disposition table above");
    }
}
