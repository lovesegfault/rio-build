//! §13b anchor+bulk deficit cover.
//!
//! Per `r[ctrl.nodeclaim.anchor-bulk]`: for each `(h,cap)` cell with
//! unplaced demand, mint `1×anchor` (smallest type fitting
//! `max_U(c*,M,D)`) plus `⌈(Σc* − anchor.cores)/bulk.cores⌉ × bulk`
//! (cheapest \$/core type meeting `median_U(M/c*)`), capped at
//! `sla.maxNodeClaimsPerCellPerTick` and the `sla.maxFleetCores`
//! budget. Cells iterate round-robin from a rotating start so no cell
//! starves under sustained pressure.
//!
//! This module is the pure-compute half: cell assignment, anchor/bulk
//! selection, claim-count math, and NodeClaim spec construction. The
//! `Api::create` side-effect lives in
//! [`super::NodeClaimPoolReconciler::cover_deficit`].

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::ResourceRequirements;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::api::ObjectMeta;
use rio_crds::karpenter::{NodeClaim, NodeClaimSpec, NodeClassRef, NodeSelectorRequirementWithMin};
use rio_proto::types::SpawnIntent;
use serde::{Deserialize, Serialize};

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

/// One instance shape in a cell's menu. Helm-populated
/// (`[nodeclaim_pool.instance_menu]` in the ConfigMap-mounted
/// `controller.toml`): the controller doesn't run
/// the scheduler's live `DescribeSpotPriceHistory` poll, so
/// `price_per_vcpu_hr` is the static seed — sufficient for the
/// "cheapest \$/core" bulk pick (the relative ranking is what matters,
/// not absolute price).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceType {
    /// vCPU count. Unit-consistent with `SpawnIntent.cores` (whole
    /// cores).
    pub cores: u32,
    /// RAM bytes.
    pub mem_bytes: u64,
    /// Ephemeral-storage bytes (instance-store + root EBS).
    pub disk_bytes: u64,
    /// Static seed `$/vCPU·hr`. Ranks bulk candidates; never billed.
    pub price_per_vcpu_hr: f64,
}

/// Group `unplaced` by the cheapest cell in each intent's `A_open`.
/// Intents with empty `A_open` (hw-agnostic, or every cell's lead-time
/// shorter than `eta_seconds`) are dropped — `cover_deficit` can't
/// target a cell-less intent and the legacy `Pool` reconciler picks
/// those up.
///
/// `BTreeMap` so iteration order is deterministic (round-robin
/// rotation acts on a sorted universe; flapping order would defeat the
/// no-starvation guarantee).
pub fn assign_to_cells<'a>(
    unplaced: &'a [SpawnIntent],
    sketches: &CellSketches,
    cell_price: impl Fn(&Cell) -> f64,
) -> BTreeMap<Cell, Vec<&'a SpawnIntent>> {
    let mut by_cell: BTreeMap<Cell, Vec<&SpawnIntent>> = BTreeMap::new();
    for i in unplaced {
        let open = a_open(i, sketches);
        let Some(cheapest) = open
            .into_iter()
            .min_by(|a, b| cell_price(a).total_cmp(&cell_price(b)))
        else {
            continue;
        };
        by_cell.entry(cheapest).or_default().push(i);
    }
    by_cell
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

/// Smallest (by `price_per_vcpu_hr`, then `cores`) menu entry covering
/// `(c, m, d)`. `None` ⇔ no entry fits — the menu-gap base case
/// (config-load asserts `max_menu ≥ (maxCores,maxMem,maxDisk)` so
/// prod-unreachable; surfaced as
/// `rio_controller_placement_sim_mismatch_total{reason="menu_gap"}`).
pub fn pick_anchor(menu: &[InstanceType], c: u32, m: u64, d: u64) -> Option<&InstanceType> {
    menu.iter()
        .filter(|t| t.cores >= c && t.mem_bytes >= m && t.disk_bytes >= d)
        .min_by(|a, b| {
            a.price_per_vcpu_hr
                .total_cmp(&b.price_per_vcpu_hr)
                .then(a.cores.cmp(&b.cores))
        })
}

/// Cheapest \$/core menu entry whose `mem/cores` ratio ≥ `med_ratio`
/// (so a node-full of median-shaped intents fits on memory). Falls
/// back to `anchor` when nothing meets the ratio (degenerate menu).
pub fn pick_bulk<'a>(
    menu: &'a [InstanceType],
    med_ratio: f64,
    anchor: &'a InstanceType,
) -> &'a InstanceType {
    menu.iter()
        .filter(|t| t.cores > 0 && (t.mem_bytes as f64 / f64::from(t.cores)) >= med_ratio)
        .min_by(|a, b| {
            a.price_per_vcpu_hr
                .total_cmp(&b.price_per_vcpu_hr)
                .then(a.cores.cmp(&b.cores))
        })
        .unwrap_or(anchor)
}

/// `N = min(need, per_tick_cap, budget_fit)` where
/// `need = 1 + ⌈(Σc − anchor)/bulk⌉` and
/// `budget_fit = 1 + ⌊(budget − anchor)/bulk⌋`.
/// `0` when the budget can't cover even the anchor (`budget <
/// anchor.cores`). Integer math throughout — `bulk.cores ≥ 1` by menu
/// construction.
pub fn claim_count(sum_c: u32, anchor_c: u32, bulk_c: u32, per_tick_cap: u32, budget: u32) -> u32 {
    if budget < anchor_c {
        return 0;
    }
    let bulk_c = bulk_c.max(1);
    let need = 1 + sum_c.saturating_sub(anchor_c).div_ceil(bulk_c);
    let budget_fit = 1 + (budget - anchor_c) / bulk_c;
    need.min(per_tick_cap).min(budget_fit)
}

/// Median of `xs`. Empty → 0 (so `pick_bulk`'s ratio gate degenerates
/// to "any type passes"). Even-length → lower-middle (no fp-mean —
/// the value feeds a `≥` gate so either middle is fine; lower keeps
/// the gate looser).
pub fn median(mut xs: Vec<f64>) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.sort_by(f64::total_cmp);
    xs[(xs.len() - 1) / 2]
}

/// Build a NodeClaim for `cell` requesting `it`'s `(cores, mem, disk)`.
///
/// - `metadata.generateName`: `rio-nc-<h>-<cap>-` (k8s appends 5
///   random chars). NodeClaims aren't idempotent-named — each tick's
///   cover may legitimately create another for the same cell.
/// - `metadata.labels`: [`HW_CLASS_LABEL`] + [`CAPACITY_TYPE_LABEL`]
///   (so [`super::ffd::LiveNode::from`] recovers `cell` next tick),
///   [`NODEPOOL_LABEL`] = [`SHIM_NODEPOOL`] (Karpenter state-tracking),
///   and the [`super::OWNER_LABEL`] selector.
/// - `spec.requirements`: the cell's hw-class label conjunction (the
///   same `(k, In, [v])` shape `cells_to_selector_terms` emits) AND
///   `karpenter.sh/capacity-type In [<cap>]`. Karpenter resolves to
///   the smallest fitting instance type within that conjunction.
/// - `spec.resources.requests`: `{cpu, memory, ephemeral-storage}` =
///   `it`. Karpenter uses these as the floor for instance-type
///   selection; `spec.requirements` constrains the family.
pub fn build_nodeclaim(
    cell: &Cell,
    it: &InstanceType,
    hw_labels: &[(String, String)],
    node_class_ref: &str,
) -> NodeClaim {
    let cap_label = match cell.1 {
        CapacityType::Spot => "spot",
        CapacityType::OnDemand => "on-demand",
    };
    let (owner_k, owner_v) = super::OWNER_LABEL
        .split_once('=')
        .expect("OWNER_LABEL is k=v");
    let labels: BTreeMap<String, String> = [
        (HW_CLASS_LABEL.into(), cell.0.clone()),
        (CAPACITY_TYPE_LABEL.into(), cap_label.into()),
        (NODEPOOL_LABEL.into(), SHIM_NODEPOOL.into()),
        (owner_k.into(), owner_v.into()),
    ]
    .into();
    let mut requirements: Vec<NodeSelectorRequirementWithMin> = hw_labels
        .iter()
        .map(|(k, v)| NodeSelectorRequirementWithMin {
            key: k.clone(),
            operator: "In".into(),
            values: vec![v.clone()],
            min_values: None,
        })
        .collect();
    requirements.push(NodeSelectorRequirementWithMin {
        key: CAPACITY_TYPE_LABEL.into(),
        operator: "In".into(),
        values: vec![cap_label.into()],
        min_values: None,
    });
    let requests: BTreeMap<String, Quantity> = [
        ("cpu".into(), Quantity(it.cores.to_string())),
        ("memory".into(), Quantity(it.mem_bytes.to_string())),
        (
            "ephemeral-storage".into(),
            Quantity(it.disk_bytes.to_string()),
        ),
    ]
    .into();
    NodeClaim {
        metadata: ObjectMeta {
            generate_name: Some(format!("rio-nc-{}-{}-", cell.0, cell.1.as_str())),
            labels: Some(labels),
            ..Default::default()
        },
        spec: NodeClaimSpec {
            node_class_ref: NodeClassRef {
                group: NODE_CLASS_GROUP.into(),
                kind: NODE_CLASS_KIND.into(),
                name: node_class_ref.into(),
            },
            requirements,
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

    fn it(cores: u32, mem_gib: u64, disk_gib: u64, p: f64) -> InstanceType {
        InstanceType {
            cores,
            mem_bytes: mem_gib * GI,
            disk_bytes: disk_gib * GI,
            price_per_vcpu_hr: p,
        }
    }

    fn intent(id: &str, cores: u32, mem: u64, cells: &[(&str, CapacityType)]) -> SpawnIntent {
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
            ready: Some(true),
            hw_class_names,
            node_affinity,
            ..Default::default()
        }
    }

    // --- anchor / bulk -------------------------------------------------

    /// r[ctrl.nodeclaim.anchor-bulk]: anchor = smallest type fitting
    /// `max_U(c*,M,D)`. U = {(32,64G), (4,8G)}; menu = {xl(4c,16G),
    /// 8xl(32c,128G)}. max_U = (32,64G) → only 8xl fits → anchor=8xl.
    // r[verify ctrl.nodeclaim.anchor-bulk]
    #[test]
    fn anchor_is_smallest_fitting_max_u() {
        let menu = [it(4, 16, 100, 0.04), it(32, 128, 200, 0.05)];
        let a = pick_anchor(&menu, 32, 64 * GI, GI).expect("8xl fits");
        assert_eq!(a.cores, 32);
        // Both fit → cheapest wins (then smallest on tie).
        let a2 = pick_anchor(&menu, 4, 8 * GI, GI).unwrap();
        assert_eq!(a2.cores, 4, "xl is cheaper");
        // Equal price → smallest cores tiebreak.
        let menu2 = [it(8, 32, 100, 0.04), it(4, 16, 100, 0.04)];
        assert_eq!(pick_anchor(&menu2, 2, GI, GI).unwrap().cores, 4);
    }

    /// `|U|=1`, no fitting type → `None` (caller emits
    /// `placement_sim_mismatch_total{reason=menu_gap}`).
    #[test]
    fn menu_gap_base_case_surfaces_mismatch_metric() {
        let menu = [it(4, 16, 100, 0.04), it(8, 32, 100, 0.04)];
        assert!(pick_anchor(&menu, 64, GI, GI).is_none(), "cores gap");
        assert!(pick_anchor(&menu, 4, 256 * GI, GI).is_none(), "mem gap");
        assert!(pick_anchor(&menu, 4, GI, 999 * GI).is_none(), "disk gap");
        assert!(pick_anchor(&[], 1, 1, 1).is_none(), "empty menu");
    }

    /// bulk = cheapest \$/core meeting `median_U(M/c*)`. menu has a
    /// cheap-but-mem-thin type and a pricier mem-fat type; med_ratio
    /// excludes the thin one.
    #[test]
    fn bulk_meets_median_mem_ratio() {
        let anchor = it(32, 128, 200, 0.05);
        // thin: 2G/core; fat: 8G/core.
        let menu = [
            it(16, 32, 100, 0.03),
            it(16, 128, 100, 0.04),
            anchor.clone(),
        ];
        // med_ratio = 4G/core → thin (2G/core) excluded; fat is cheapest
        // remaining.
        let b = pick_bulk(&menu, 4.0 * GI as f64, &anchor);
        assert_eq!(b.mem_bytes, 128 * GI);
        // med_ratio = 1G/core → thin passes and is cheapest.
        let b2 = pick_bulk(&menu, GI as f64, &anchor);
        assert_eq!(b2.mem_bytes, 32 * GI);
        // Nothing meets ratio → falls back to anchor.
        let b3 = pick_bulk(&menu, 999.0 * GI as f64, &anchor);
        assert_eq!(b3.cores, 32);
    }

    // --- claim_count ---------------------------------------------------

    /// budget=40, anchor=32c, bulk=16c, per_tick=8, Σc=100.
    /// need = 1+⌈68/16⌉ = 6; budget_fit = 1+⌊8/16⌋ = 1. → N=1.
    #[test]
    fn n_respects_budget_and_per_tick_cap() {
        assert_eq!(claim_count(100, 32, 16, 8, 40), 1, "budget binds");
        // budget=200 → budget_fit=1+⌊168/16⌋=11; per_tick=8 binds → 6? no:
        // need=6, per_tick=8, budget_fit=11 → N=6 (need binds).
        assert_eq!(claim_count(100, 32, 16, 8, 200), 6, "need binds");
        // per_tick=3 binds.
        assert_eq!(claim_count(100, 32, 16, 3, 200), 3, "per_tick binds");
        // budget < anchor → 0.
        assert_eq!(claim_count(100, 32, 16, 8, 16), 0, "can't afford anchor");
        // Σc ≤ anchor → need=1 (anchor only).
        assert_eq!(claim_count(20, 32, 16, 8, 200), 1, "anchor covers Σc");
        // bulk=0 guard (menu shouldn't emit, but div-by-0 panics):
        // bulk_c.max(1) → need=1+68=69, budget_fit=1+168 → per_tick=8.
        assert_eq!(claim_count(100, 32, 0, 8, 200), 8);
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
            intent("a", 4, GI, &[h1, h2]),
            intent("b", 2, GI, &[h2]),
            // hw-agnostic (empty cells) → dropped.
            SpawnIntent {
                intent_id: "agn".into(),
                cores: 4,
                ready: Some(true),
                ..Default::default()
            },
        ];
        // h1 cheaper.
        let price = |c: &Cell| if c.0 == "h1" { 0.03 } else { 0.05 };
        let by = assign_to_cells(&unplaced, &CellSketches::default(), price);
        assert_eq!(by.len(), 2);
        let h1k = Cell("h1".into(), CapacityType::Spot);
        let h2k = Cell("h2".into(), CapacityType::Spot);
        assert_eq!(by[&h1k].len(), 1);
        assert_eq!(by[&h1k][0].intent_id, "a", "a's cheapest-open is h1");
        assert_eq!(by[&h2k].len(), 1);
        assert_eq!(by[&h2k][0].intent_id, "b", "b only opens h2");
    }

    #[test]
    fn assign_empty_unplaced_empty_output() {
        assert!(assign_to_cells(&[], &CellSketches::default(), |_| 0.0).is_empty());
    }

    // --- median --------------------------------------------------------

    #[test]
    fn median_edge_cases() {
        assert_eq!(median(vec![]), 0.0);
        assert_eq!(median(vec![5.0]), 5.0);
        assert_eq!(median(vec![3.0, 1.0, 2.0]), 2.0);
        // Even-length → lower-middle.
        assert_eq!(median(vec![4.0, 1.0, 3.0, 2.0]), 2.0);
    }

    // --- build_nodeclaim -----------------------------------------------

    /// Asserts the full wire shape: generateName, labels (hw-class +
    /// cap-type + shim-nodepool + owner), nodeClassRef, requirements
    /// (hw-label conjunction AND capacity-type), resources.requests.
    #[test]
    fn build_nodeclaim_spec_shape() {
        let cell = Cell("mid-ebs-x86".into(), CapacityType::Spot);
        let shape = it(8, 32, 100, 0.04);
        let hw_labels = vec![
            ("kubernetes.io/arch".into(), "amd64".into()),
            ("karpenter.k8s.aws/instance-category".into(), "m".into()),
        ];
        let nc = build_nodeclaim(&cell, &shape, &hw_labels, "rio-default");

        let meta = &nc.metadata;
        assert_eq!(
            meta.generate_name.as_deref(),
            Some("rio-nc-mid-ebs-x86-spot-")
        );
        let labels = meta.labels.as_ref().unwrap();
        assert_eq!(labels[HW_CLASS_LABEL], "mid-ebs-x86");
        assert_eq!(labels[CAPACITY_TYPE_LABEL], "spot");
        assert_eq!(labels[NODEPOOL_LABEL], SHIM_NODEPOOL);
        assert_eq!(labels["rio.build/nodeclaim-pool"], "builder");

        let spec = &nc.spec;
        assert_eq!(spec.node_class_ref.name, "rio-default");
        assert_eq!(spec.node_class_ref.group, "karpenter.k8s.aws");
        assert_eq!(spec.node_class_ref.kind, "EC2NodeClass");
        // 2 hw-labels + 1 capacity-type.
        assert_eq!(spec.requirements.len(), 3);
        let cap_req = spec
            .requirements
            .iter()
            .find(|r| r.key == CAPACITY_TYPE_LABEL)
            .unwrap();
        assert_eq!(cap_req.values, vec!["spot"]);
        let arch_req = spec
            .requirements
            .iter()
            .find(|r| r.key == "kubernetes.io/arch")
            .unwrap();
        assert_eq!(arch_req.operator, "In");
        assert_eq!(arch_req.values, vec!["amd64"]);

        let req = spec.resources.as_ref().unwrap().requests.as_ref().unwrap();
        assert_eq!(req["cpu"], Quantity("8".into()));
        assert_eq!(req["memory"], Quantity((32 * GI).to_string()));
        assert_eq!(req["ephemeral-storage"], Quantity((100 * GI).to_string()));
    }

    #[test]
    fn build_nodeclaim_on_demand_label_form() {
        let cell = Cell("h".into(), CapacityType::OnDemand);
        let nc = build_nodeclaim(&cell, &it(4, 8, 50, 0.1), &[], "x");
        // Karpenter label form is "on-demand", NOT the PG/helm "od".
        assert_eq!(
            nc.metadata.labels.as_ref().unwrap()[CAPACITY_TYPE_LABEL],
            "on-demand"
        );
        // generateName uses the PG/helm form (DNS-safe, matches
        // `Cell::to_string`).
        assert_eq!(nc.metadata.generate_name.as_deref(), Some("rio-nc-h-od-"));
    }
}
