//! First-Fit-Decreasing bin-packing simulation.
//!
//! Per `r[ctrl.nodeclaim.ffd-sim]`: sort intents `(ready, c*)` descending
//! (Ready before forecast, large before small), bin-select MostAllocated
//! on the `allocatable` divisor — the same scoring `kube-scheduler-packed`
//! (B2) uses, so the simulation's `placeable` set predicts what the real
//! scheduler will do once B12 routes pods to it. The `unplaced` residual
//! is `cover_deficit`'s (B8) per-cell input.

use std::collections::{BTreeMap, HashMap};

use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use rio_crds::karpenter::{NodeClaim, NodeClaimStatus};
use rio_proto::types::SpawnIntent;

use super::sketch::{CapacityType, Cell, CellSketches};

/// Karpenter's well-known capacity-type label key. Values: `"spot"` /
/// `"on-demand"` (NOT the PG/helm `"od"` form — `cap_from_label`
/// maps).
pub const CAPACITY_TYPE_LABEL: &str = "karpenter.sh/capacity-type";

/// hw-class label key. The scheduler emits this on each
/// `node_affinity` term (`r[sched.sla.hw-class]`); B8's
/// `create_nodeclaim` stamps it on `metadata.labels` so
/// [`LiveNode::from`] can recover the cell without re-reading the
/// scheduler's `[sla.hw_classes]` map.
pub const HW_CLASS_LABEL: &str = "rio.build/hw-class";

/// View of one owned NodeClaim for FFD + consolidation. Built from the
/// typed `NodeClaim` (B4) so condition/allocatable/label parsing lives
/// in one `From` impl.
#[derive(Debug, Clone)]
pub struct LiveNode {
    /// `metadata.name` — the delete key.
    pub name: String,
    /// Backing `Node` name once `Registered=True`; `None` in-flight.
    pub node_name: Option<String>,
    /// `status.conditions[type=Registered].status == "True"`. FFD treats
    /// in-flight (`!registered`) claims as projected capacity (their
    /// `status.capacity`, populated at Launch); Registered claims use
    /// `status.allocatable` minus [`Self::requested`].
    pub registered: bool,
    /// `(hw_class, capacity_type)` recovered from `metadata.labels`.
    /// `None` ⇔ labels absent/malformed (a freshly-`create()`d claim
    /// before Karpenter resolves capacity-type, or a non-B8 claim that
    /// leaked the [`OWNER_LABEL`](super::OWNER_LABEL)). FFD skips
    /// cell-less nodes — no intent's `A_open` can match `None`.
    pub cell: Option<Cell>,
    /// `(cores, mem_bytes, disk_bytes)` from `status.allocatable`
    /// (preferred) or `status.capacity` (in-flight fallback). Whole
    /// cores: `7910m` → 7, matching `SpawnIntent.cores`' unit.
    pub allocatable: (u32, u64, u64),
    /// `(cores, mem_bytes, disk_bytes)` already requested by pods on
    /// the backing Node. `From<NodeClaim>` sets this to `(0,0,0)`.
    // TODO: populate from `Σ Pod.spec.containers[].resources.requests`
    // over `spec.nodeName == self.node_name` (needs a pod-by-node
    // informer cache; the apiserver list-per-tick is too chatty at
    // ~10s × N_nodes). B12 routes builder pods via `schedulerName:
    // rio-packed` so they land on these claims now; until the sum is
    // wired `free()` on a Registered node over-reports by whatever's
    // already bound. Drift is observable as
    // `placement_sim_mismatch_total{reason=menu_gap}` staying zero
    // while `ffd_unplaced_cores` is high with `nodeclaim_live
    // {state=registered}` non-zero.
    pub requested: (u32, u64, u64),
    /// `metadata.creationTimestamp` as unix-epoch seconds. `None` only
    /// on a just-`create()`d object before the apiserver round-trip.
    pub created_secs: Option<f64>,
    /// `metadata.annotations`. B10's hold-open ε reads
    /// `rio.build/hold-open`.
    pub annotations: BTreeMap<String, String>,
    /// Full `status` for B10/B11's condition reads (idle-since,
    /// `Launched=False` ICE detection).
    pub status: NodeClaimStatus,
}

/// `metav1.Time` → unix-epoch seconds. kube 3.0 wraps `jiff::Timestamp`;
/// integer seconds suffice for boot-time arithmetic (typical boot ≈
/// 30–120s).
fn time_secs(t: &Time) -> f64 {
    t.0.as_second() as f64
}

impl LiveNode {
    /// Remaining placeable `(cores, mem, disk)`. Registered →
    /// `allocatable − requested` (saturating: a mis-accounted node
    /// reads as 0-free, not underflow). In-flight → `allocatable`
    /// (nothing scheduled yet by construction).
    pub fn free(&self) -> (u32, u64, u64) {
        if self.registered {
            (
                self.allocatable.0.saturating_sub(self.requested.0),
                self.allocatable.1.saturating_sub(self.requested.1),
                self.allocatable.2.saturating_sub(self.requested.2),
            )
        } else {
            self.allocatable
        }
    }

    /// `(status, last_transition_secs)` for condition `type_`. `None` ⇔
    /// the condition isn't present (Karpenter writes `Launched`/
    /// `Registered` lazily — absence ≠ `False`).
    pub fn cond(&self, type_: &str) -> Option<(&str, f64)> {
        self.status
            .conditions
            .iter()
            .find(|c| c.type_ == type_)
            .map(|c| (c.status.as_str(), time_secs(&c.last_transition_time)))
    }

    /// Seconds since `metadata.creationTimestamp`. `None` if creation
    /// time is absent (apiserver hasn't round-tripped yet).
    pub fn age_secs(&self, now_secs: f64) -> Option<f64> {
        self.created_secs.map(|c| now_secs - c)
    }

    /// `Registered.lastTransitionTime − creationTimestamp`: the
    /// Karpenter+kubelet boot overhead. `Some` only when
    /// `Registered=True`. The value B9's [`super::sketch::CellState::
    /// record`] feeds into the `boot_active` DDSketch.
    pub fn boot_secs(&self) -> Option<f64> {
        let created = self.created_secs?;
        match self.cond("Registered")? {
            ("True", t) => Some(t - created),
            _ => None,
        }
    }

    /// Seconds since the node became idle (Karpenter's `Empty=True`
    /// transition). Falls back to `Registered=True` transition when
    /// `Empty` is absent — a node with no pod ever scheduled has been
    /// idle since registration. `None` for in-flight claims.
    pub fn idle_secs(&self, now_secs: f64) -> Option<f64> {
        if !self.registered {
            return None;
        }
        let since = match self.cond("Empty") {
            Some(("True", t)) => t,
            Some(_) => return None,
            None => self.cond("Registered")?.1,
        };
        Some(now_secs - since)
    }

    /// Read `metadata.annotations[key]`.
    pub fn annotation(&self, key: &str) -> Option<&str> {
        self.annotations.get(key).map(String::as_str)
    }
}

impl From<NodeClaim> for LiveNode {
    fn from(nc: NodeClaim) -> Self {
        let status = nc.status.unwrap_or_default();
        let registered = status
            .conditions
            .iter()
            .any(|c| c.type_ == "Registered" && c.status == "True");
        let cell = nc.metadata.labels.as_ref().and_then(|l| {
            let h = l.get(HW_CLASS_LABEL)?;
            let cap = cap_from_label(l.get(CAPACITY_TYPE_LABEL)?)?;
            Some(Cell(h.clone(), cap))
        });
        // Prefer allocatable (kubelet-reported, post-reserved); fall
        // back to capacity (Karpenter's launch-time projection).
        let allocatable = status
            .allocatable
            .as_ref()
            .or(status.capacity.as_ref())
            .map_or((0, 0, 0), parse_resources);
        Self {
            name: nc.metadata.name.unwrap_or_default(),
            node_name: status.node_name.clone(),
            registered,
            cell,
            allocatable,
            requested: (0, 0, 0),
            created_secs: nc.metadata.creation_timestamp.as_ref().map(time_secs),
            annotations: nc.metadata.annotations.unwrap_or_default(),
            status,
        }
    }
}

/// `(intent, target_nodeclaim_name, in_flight)` for placeable intents.
/// `in_flight = !registered` so the consolidator can distinguish
/// "reserved on a live node" from "reserved on a node that hasn't
/// landed yet".
pub type Placement = (SpawnIntent, String, bool);

/// FFD-simulate placing `intents` onto `live`. Returns
/// `(placeable, unplaced)`.
///
/// **Sort**: `(ready, cores, mem_bytes)` descending, `intent_id`
/// ascending tiebreak (stable across ticks). `ready` is the explicit
/// proto field 13 discriminator — NOT `eta_seconds == 0.0`, which a
/// forecast intent with overdue deps can hit (bug_030).
///
/// **A_open**: a Ready intent's admissible cells are its full
/// `cells_of` set. A forecast intent's are filtered to
/// `eta_seconds < lead_time[cell]` — only place on cells that will be
/// up before the intent's deps complete. Empty `A_open` (hw-agnostic
/// intent, or all cells too slow) → unplaced.
///
/// **Bin-select**: among `live` nodes whose `cell ∈ A_open` AND whose
/// running `free` covers `(cores, mem, disk)`, pick MostAllocated:
/// max `(allocatable − free + cores) / allocatable` on the cpu axis.
/// `free` is the running tally (decremented per placement) so the
/// score sees prior placements within this tick — matching
/// kube-scheduler-packed's per-pod scoring on the live node state.
// r[impl ctrl.nodeclaim.ffd-sim]
pub fn simulate(
    intents: &[SpawnIntent],
    live: &[LiveNode],
    sketches: &CellSketches,
) -> (Vec<Placement>, Vec<SpawnIntent>) {
    // Running free per node. Cell-less nodes excluded up front: no
    // intent can match them, and excluding here keeps the score loop's
    // `cell.unwrap` infallible.
    let mut free: HashMap<&str, (u32, u64, u64)> = live
        .iter()
        .filter(|n| n.cell.is_some())
        .map(|n| (n.name.as_str(), n.free()))
        .collect();

    let mut sorted: Vec<SpawnIntent> = intents.to_vec();
    sorted.sort_by(|a, b| {
        let k = |i: &SpawnIntent| (i.ready.unwrap_or(true), i.cores, i.mem_bytes);
        k(b).cmp(&k(a)).then_with(|| a.intent_id.cmp(&b.intent_id))
    });

    let mut placeable = Vec::with_capacity(sorted.len());
    let mut unplaced = Vec::new();
    for i in sorted {
        let open = a_open(&i, sketches);
        let best = live
            .iter()
            .filter(|n| n.cell.as_ref().is_some_and(|c| open.contains(c)))
            .filter(|n| {
                free.get(n.name.as_str())
                    .is_some_and(|f| f.0 >= i.cores && f.1 >= i.mem_bytes && f.2 >= i.disk_bytes)
            })
            .max_by(|a, b| {
                // MostAllocated on cpu: highest post-placement
                // utilization wins. `allocatable.0.max(1)`: a 0-core
                // node (status not yet populated) scores 0 instead of
                // NaN — and was already filtered by the `free >= cores`
                // check unless `i.cores == 0`.
                let score = |n: &LiveNode| -> f64 {
                    let f = free[n.name.as_str()];
                    let alloc = n.allocatable.0.max(1);
                    f64::from(alloc - f.0 + i.cores) / f64::from(alloc)
                };
                score(a).total_cmp(&score(b))
            });
        match best {
            Some(n) => {
                let f = free.get_mut(n.name.as_str()).expect("filtered above");
                f.0 -= i.cores;
                f.1 -= i.mem_bytes;
                f.2 -= i.disk_bytes;
                placeable.push((i, n.name.clone(), !n.registered));
            }
            None => unplaced.push(i),
        }
    }
    (placeable, unplaced)
}

/// Map `karpenter.sh/capacity-type` label values to [`CapacityType`].
/// Distinct from [`CapacityType::parse`] which takes the PG/helm
/// `"spot"`/`"od"` form (migration 059 CHECK constraint). Karpenter's
/// label canon is `"spot"`/`"on-demand"`.
fn cap_from_label(s: &str) -> Option<CapacityType> {
    match s {
        "spot" => Some(CapacityType::Spot),
        "on-demand" => Some(CapacityType::OnDemand),
        _ => None,
    }
}

/// Recover `(hw_class, cap)` cells from a SpawnIntent's parallel
/// `(hw_class_names, node_affinity)` arrays. One cell per term; terms
/// missing a `karpenter.sh/capacity-type` requirement are dropped
/// (hw-agnostic mode emits empty arrays, so the zip is empty).
pub fn cells_of(i: &SpawnIntent) -> Vec<Cell> {
    i.hw_class_names
        .iter()
        .zip(&i.node_affinity)
        .filter_map(|(h, t)| {
            let cap = t
                .match_expressions
                .iter()
                .find(|r| r.key == CAPACITY_TYPE_LABEL)?
                .values
                .first()?;
            Some(Cell(h.clone(), cap_from_label(cap)?))
        })
        .collect()
}

/// Admissible-open cell set for `i`: Ready → all of `cells_of(i)`;
/// forecast → those with `eta_seconds < lead_time[cell]`. B8's
/// `cover_deficit` reuses this (the cheapest-open-cell choice operates
/// on the same set FFD placed against).
pub fn a_open(i: &SpawnIntent, sketches: &CellSketches) -> Vec<Cell> {
    let cells = cells_of(i);
    if i.ready.unwrap_or(true) {
        return cells;
    }
    cells
        .into_iter()
        .filter(|c| i.eta_seconds < sketches.lead_time(c))
        .collect()
}

/// `(cores, mem_bytes, disk_bytes)` from a `cpu`/`memory`/
/// `ephemeral-storage` Quantity map. Missing keys → 0.
fn parse_resources(m: &BTreeMap<String, Quantity>) -> (u32, u64, u64) {
    let q = |k: &str| m.get(k).map(|q| q.0.as_str());
    (
        // SpawnIntent.cores is whole cores (jobs.rs writes
        // `Quantity(cores.to_string())`); truncate millicores so the
        // `free.0 >= i.cores` comparison is unit-consistent.
        q("cpu").map_or(0, |s| (parse_cpu_millis(s) / 1000) as u32),
        q("memory").map_or(0, parse_bytes),
        q("ephemeral-storage").map_or(0, parse_bytes),
    )
}

/// Parse a Kubernetes CPU Quantity string to millicores.
/// `"64"` → 64000, `"64000m"` → 64000, `"1.5"` → 1500, `"1k"` →
/// 1_000_000. Malformed → `warn!` + 0.
///
/// Handles all decimal-SI suffixes (`n`/`u`/`m`/`k`/`M`/`G`/`T`/`P`/
/// `E`). apimachinery's `Quantity.String()` canonicalizes a DecimalSI
/// value of exactly N×1000 cores as `"Nk"` (rule: largest suffix with
/// no fractional digits) — Karpenter's `status.allocatable.cpu` is a
/// `v1.ResourceList` Quantity, so a 1000-core node serializes as `"1k"`.
/// Binary-SI (`Ki`/`Mi`) is unhandled — never emitted for CPU.
pub(crate) fn parse_cpu_millis(q: &str) -> u64 {
    // Suffix → multiplier (cores). Longest first so `"m"` doesn't
    // shadow nothing-relevant here, but consistent with the idiom.
    let (num, mult): (&str, f64) = [
        ("n", 1e-9),
        ("u", 1e-6),
        ("m", 1e-3),
        ("k", 1e3),
        ("M", 1e6),
        ("G", 1e9),
        ("T", 1e12),
        ("P", 1e15),
        ("E", 1e18),
    ]
    .iter()
    .find_map(|(s, m)| q.strip_suffix(*s).map(|n| (n, *m)))
    .unwrap_or((q, 1.0));
    num.parse::<f64>()
        .map(|c| (c * mult * 1000.0).round() as u64)
        .unwrap_or_else(|_| {
            tracing::warn!(quantity = %q, "unparseable CPU Quantity; treating as 0");
            0
        })
}

/// Parse a Kubernetes memory/storage Quantity string to bytes.
/// Handles binary-SI (`Ki`/`Mi`/`Gi`/`Ti`/`Pi`/`Ei`), decimal-SI
/// (`k`/`K`/`M`/`G`/`T`/`P`/`E`), and bare numbers (incl.
/// DecimalExponent like `"1e6"`). Malformed → 0. Two-char binary
/// suffixes are checked first so `"31Gi"` doesn't strip as `"31G"+"i"`.
///
/// `pub(crate)`: B8's `cover_deficit` parses the instance-menu
/// `mem_bytes`/`disk_bytes` from the same Quantity form.
pub(crate) fn parse_bytes(q: &str) -> u64 {
    const BIN: [(&str, u64); 6] = [
        ("Ei", 1 << 60),
        ("Pi", 1 << 50),
        ("Ti", 1 << 40),
        ("Gi", 1 << 30),
        ("Mi", 1 << 20),
        ("Ki", 1 << 10),
    ];
    const DEC: [(&str, f64); 7] = [
        ("E", 1e18),
        ("P", 1e15),
        ("T", 1e12),
        ("G", 1e9),
        ("M", 1e6),
        ("k", 1e3),
        ("K", 1e3),
    ];
    for (s, m) in BIN {
        if let Some(n) = q.strip_suffix(s) {
            return n.parse::<f64>().map_or(0, |v| (v * m as f64) as u64);
        }
    }
    for (s, m) in DEC {
        if let Some(n) = q.strip_suffix(s) {
            return n.parse::<f64>().map_or(0, |v| (v * m) as u64);
        }
    }
    q.parse::<f64>().map_or(0, |v| v as u64)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use rio_proto::types::{NodeSelectorRequirement, NodeSelectorTerm};

    const GI: u64 = 1 << 30;

    // --- builders ------------------------------------------------------

    fn nc(name: &str, registered: bool) -> NodeClaim {
        // `Condition` has non-Default `last_transition_time` (Time);
        // build via JSON so the test stays decoupled from k8s-openapi's
        // jiff/chrono choice.
        let status: NodeClaimStatus = serde_json::from_value(serde_json::json!({
            "conditions": [{
                "type": "Registered",
                "status": if registered { "True" } else { "False" },
                "lastTransitionTime": "2026-01-01T00:00:00Z",
                "reason": "", "message": "",
            }],
            "nodeName": registered.then(|| format!("node-{name}")),
            "allocatable": { "cpu": "8", "memory": "32Gi", "ephemeral-storage": "100Gi" },
        }))
        .unwrap();
        NodeClaim {
            metadata: kube::api::ObjectMeta {
                name: Some(name.into()),
                labels: Some(
                    [
                        (HW_CLASS_LABEL.into(), "mid-ebs-x86".into()),
                        (CAPACITY_TYPE_LABEL.into(), "spot".into()),
                    ]
                    .into(),
                ),
                ..Default::default()
            },
            spec: Default::default(),
            status: Some(status),
        }
    }

    /// LiveNode with `cell`, `allocatable` cores/mem/disk, registered.
    /// `requested` defaults 0.
    pub(crate) fn node(
        name: &str,
        hw: &str,
        cap: CapacityType,
        cores: u32,
        mem: u64,
        disk: u64,
    ) -> LiveNode {
        LiveNode {
            name: name.into(),
            node_name: Some(format!("node-{name}")),
            registered: true,
            cell: Some(Cell(hw.into(), cap)),
            allocatable: (cores, mem, disk),
            requested: (0, 0, 0),
            created_secs: Some(1000.0),
            annotations: BTreeMap::new(),
            status: NodeClaimStatus::default(),
        }
    }

    /// Set `(type, status, lastTransitionTime)` conditions on `n.status`.
    /// Built via JSON: `Condition` has non-Default `last_transition_time`.
    pub(crate) fn with_conds(mut n: LiveNode, conds: &[(&str, &str, f64)]) -> LiveNode {
        let cs: Vec<_> = conds
            .iter()
            .map(|(ty, st, t)| {
                serde_json::json!({
                    "type": ty, "status": st,
                    "lastTransitionTime": format!("1970-01-01T{:02}:{:02}:{:02}Z",
                        (*t as u64) / 3600, ((*t as u64) % 3600) / 60, (*t as u64) % 60),
                    "reason": "", "message": "",
                })
            })
            .collect();
        n.status.conditions = serde_json::from_value(serde_json::json!(cs)).unwrap();
        n
    }

    /// SpawnIntent targeting `cells` (one affinity term per).
    fn intent(id: &str, cores: u32, mem: u64, cells: &[(&str, CapacityType)]) -> SpawnIntent {
        let (hw_class_names, node_affinity) = cells
            .iter()
            .map(|(h, cap)| {
                let cap_label = match cap {
                    CapacityType::Spot => "spot",
                    CapacityType::OnDemand => "on-demand",
                };
                let term = NodeSelectorTerm {
                    match_expressions: vec![
                        NodeSelectorRequirement {
                            key: HW_CLASS_LABEL.into(),
                            operator: "In".into(),
                            values: vec![(*h).into()],
                        },
                        NodeSelectorRequirement {
                            key: CAPACITY_TYPE_LABEL.into(),
                            operator: "In".into(),
                            values: vec![cap_label.into()],
                        },
                    ],
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

    fn forecast(mut i: SpawnIntent, eta: f64) -> SpawnIntent {
        i.ready = Some(false);
        i.eta_seconds = eta;
        i
    }

    fn placed_on<'a>(p: &'a [Placement], id: &str) -> &'a str {
        &p.iter().find(|(i, _, _)| i.intent_id == id).unwrap().1
    }

    // --- LiveNode parsing ----------------------------------------------

    #[test]
    fn live_node_from_nodeclaim_reads_registered() {
        let live: LiveNode = nc("a", true).into();
        assert_eq!(live.name, "a");
        assert!(live.registered);
        assert_eq!(live.node_name.as_deref(), Some("node-a"));
        assert_eq!(
            live.cell,
            Some(Cell("mid-ebs-x86".into(), CapacityType::Spot))
        );
        assert_eq!(live.allocatable, (8, 32 * GI, 100 * GI));
        assert_eq!(live.requested, (0, 0, 0));
        assert_eq!(live.free(), (8, 32 * GI, 100 * GI));

        let inflight: LiveNode = nc("b", false).into();
        assert!(!inflight.registered);
        assert!(inflight.node_name.is_none());
    }

    /// `cond()` reads `(status, lastTransitionTime)`; `boot_secs()` =
    /// Registered.transition − created; `age_secs()` = now − created.
    #[test]
    fn live_node_cond_boot_age() {
        let n = with_conds(
            node("a", "h", CapacityType::Spot, 8, GI, GI),
            &[("Launched", "True", 1010.0), ("Registered", "True", 1042.0)],
        );
        assert_eq!(n.cond("Launched"), Some(("True", 1010.0)));
        assert_eq!(n.cond("Registered"), Some(("True", 1042.0)));
        assert_eq!(n.cond("Drifted"), None);
        assert_eq!(n.boot_secs(), Some(42.0));
        assert_eq!(n.age_secs(1100.0), Some(100.0));
        // Registered=False → no boot_secs.
        let nf = with_conds(
            node("b", "h", CapacityType::Spot, 8, GI, GI),
            &[("Registered", "False", 1042.0)],
        );
        assert_eq!(nf.boot_secs(), None);
        // No created_secs → no boot/age.
        let mut nc = n.clone();
        nc.created_secs = None;
        assert_eq!(nc.boot_secs(), None);
        assert_eq!(nc.age_secs(1100.0), None);
    }

    /// `idle_secs()`: Empty=True → since-transition; Empty=False →
    /// None (busy); no Empty cond → since-Registered. In-flight → None.
    #[test]
    fn live_node_idle_secs() {
        let base = node("a", "h", CapacityType::Spot, 8, GI, GI);
        let empty = with_conds(
            base.clone(),
            &[("Registered", "True", 1042.0), ("Empty", "True", 1100.0)],
        );
        assert_eq!(empty.idle_secs(1160.0), Some(60.0));
        let busy = with_conds(base.clone(), &[("Empty", "False", 1100.0)]);
        assert_eq!(busy.idle_secs(1160.0), None);
        let never_scheduled = with_conds(base.clone(), &[("Registered", "True", 1042.0)]);
        assert_eq!(never_scheduled.idle_secs(1160.0), Some(118.0));
        let mut inflight = base;
        inflight.registered = false;
        assert_eq!(inflight.idle_secs(1160.0), None);
    }

    #[test]
    fn live_node_from_statusless_nodeclaim() {
        let nc = NodeClaim {
            metadata: kube::api::ObjectMeta {
                name: Some("fresh".into()),
                ..Default::default()
            },
            spec: Default::default(),
            status: None,
        };
        let live: LiveNode = nc.into();
        assert!(!live.registered, "no status → not registered");
        assert_eq!(live.cell, None, "no labels → no cell");
        assert_eq!(live.allocatable, (0, 0, 0));
    }

    #[test]
    fn live_node_capacity_fallback_when_allocatable_absent() {
        // In-flight: Karpenter has populated `capacity` at Launch but
        // kubelet hasn't reported `allocatable` yet.
        let status: NodeClaimStatus = serde_json::from_value(serde_json::json!({
            "capacity": { "cpu": "16", "memory": "64Gi" },
        }))
        .unwrap();
        let nc = NodeClaim {
            metadata: Default::default(),
            spec: Default::default(),
            status: Some(status),
        };
        let live: LiveNode = nc.into();
        assert_eq!(live.allocatable, (16, 64 * GI, 0));
    }

    #[test]
    fn free_subtracts_requested_only_when_registered() {
        let mut n = node("a", "h", CapacityType::Spot, 8, 32 * GI, 100 * GI);
        n.requested = (3, 8 * GI, 10 * GI);
        assert_eq!(n.free(), (5, 24 * GI, 90 * GI));
        n.registered = false;
        assert_eq!(
            n.free(),
            (8, 32 * GI, 100 * GI),
            "in-flight ignores requested"
        );
        // Saturating: over-accounted node reads 0, not underflow.
        n.registered = true;
        n.requested = (99, 0, 0);
        assert_eq!(n.free().0, 0);
    }

    // --- Quantity parsing ----------------------------------------------

    #[test]
    fn parse_bytes_forms() {
        assert_eq!(parse_bytes("0"), 0);
        assert_eq!(parse_bytes("1024"), 1024);
        assert_eq!(parse_bytes("1Ki"), 1024);
        assert_eq!(parse_bytes("31Gi"), 31 * GI);
        assert_eq!(parse_bytes("1.5Gi"), (1.5 * GI as f64) as u64);
        assert_eq!(parse_bytes("2Ti"), 2 << 40);
        assert_eq!(parse_bytes("100M"), 100_000_000);
        assert_eq!(parse_bytes("1k"), 1_000);
        assert_eq!(parse_bytes("1K"), 1_000);
        // DecimalExponent: lowercase `e` falls through to bare parse.
        assert_eq!(parse_bytes("1e6"), 1_000_000);
        assert_eq!(parse_bytes(""), 0);
        assert_eq!(parse_bytes("garbage"), 0);
        assert_eq!(parse_bytes("Gi"), 0, "suffix-only → 0");
    }

    #[test]
    fn parse_cpu_millis_forms() {
        assert_eq!(parse_cpu_millis("64"), 64_000);
        assert_eq!(parse_cpu_millis("136000m"), 136_000);
        assert_eq!(parse_cpu_millis("1.5"), 1_500);
        assert_eq!(parse_cpu_millis("0"), 0);
        assert_eq!(parse_cpu_millis("0m"), 0);
        assert_eq!(parse_cpu_millis("garbage"), 0);
        assert_eq!(parse_cpu_millis(""), 0);
        // Decimal-SI suffixes — apimachinery canonicalizes round
        // multiples of 1000 to these. `"1k"` → 0 was the bug.
        assert_eq!(parse_cpu_millis("1k"), 1_000_000);
        assert_eq!(parse_cpu_millis("10k"), 10_000_000);
        assert_eq!(parse_cpu_millis("2M"), 2_000_000_000);
        assert_eq!(parse_cpu_millis("999"), 999_000); // just below k
        assert_eq!(parse_cpu_millis("1500m"), 1_500); // m via table
        assert_eq!(parse_cpu_millis("500u"), 1); // 0.5 millicore rounds
    }

    #[test]
    fn parse_resources_truncates_millicores() {
        let m: BTreeMap<String, Quantity> = [
            ("cpu".into(), Quantity("7910m".into())),
            ("memory".into(), Quantity("31Gi".into())),
        ]
        .into();
        assert_eq!(parse_resources(&m), (7, 31 * GI, 0));
    }

    // --- cells_of / a_open ---------------------------------------------

    #[test]
    fn cells_of_zips_hw_names_with_cap_label() {
        let i = intent(
            "x",
            4,
            GI,
            &[("h1", CapacityType::Spot), ("h2", CapacityType::OnDemand)],
        );
        assert_eq!(
            cells_of(&i),
            vec![
                Cell("h1".into(), CapacityType::Spot),
                Cell("h2".into(), CapacityType::OnDemand),
            ]
        );
        // hw-agnostic: empty arrays → empty cells.
        assert!(cells_of(&SpawnIntent::default()).is_empty());
    }

    #[test]
    fn cap_from_label_karpenter_forms() {
        assert_eq!(cap_from_label("spot"), Some(CapacityType::Spot));
        assert_eq!(cap_from_label("on-demand"), Some(CapacityType::OnDemand));
        assert_eq!(cap_from_label("od"), None, "PG form, not Karpenter label");
    }

    // --- simulate ------------------------------------------------------

    #[test]
    fn ffd_empty_nodes_all_unplaced() {
        let intents = [
            intent("a", 4, GI, &[("h", CapacityType::Spot)]),
            intent("b", 2, GI, &[("h", CapacityType::Spot)]),
        ];
        let (p, u) = simulate(&intents, &[], &CellSketches::default());
        assert!(p.is_empty());
        assert_eq!(u.len(), 2);
    }

    /// r[ctrl.nodeclaim.ffd-sim]: Ready before forecast, large before
    /// small. 2×8-core nodes; intents: ready-4c, forecast-8c, ready-6c.
    /// Sort = [ready-6c, ready-4c, forecast-8c]. ready-6c → n1 (free 2),
    /// ready-4c → n2 (free 4), forecast-8c → unplaced.
    // r[verify ctrl.nodeclaim.ffd-sim]
    #[test]
    fn ffd_ready_before_forecast_large_before_small() {
        let h = ("h", CapacityType::Spot);
        let nodes = [
            node("n1", "h", CapacityType::Spot, 8, 64 * GI, 100 * GI),
            node("n2", "h", CapacityType::Spot, 8, 64 * GI, 100 * GI),
        ];
        // forecast-8c needs A_open ∋ h:spot → seed lead_time > eta.
        let mut sk = CellSketches::default();
        sk.cell_mut(&Cell("h".into(), CapacityType::Spot))
            .z_active
            .add(60.0);
        let intents = [
            intent("ready-4c", 4, GI, &[h]),
            forecast(intent("forecast-8c", 8, GI, &[h]), 10.0),
            intent("ready-6c", 6, GI, &[h]),
        ];
        let (p, u) = simulate(&intents, &nodes, &sk);
        assert_eq!(p.len(), 2);
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].intent_id, "forecast-8c");
        // ready-6c placed first (largest ready) → n1 (both empty, n1
        // listed first → equal score, max_by keeps last → actually n2).
        // MostAllocated tiebreak on equal-empty nodes: max_by returns
        // the LAST max, so n2. Then ready-4c sees n2 at 6/8 vs n1 at
        // 0/8 → picks n2? No: n2 free=2 < 4. → n1.
        assert_eq!(placed_on(&p, "ready-6c"), "n2");
        assert_eq!(placed_on(&p, "ready-4c"), "n1");
    }

    /// MostAllocated picks the node that ends up most-utilized.
    /// A(12c, 4 used), B(12c, 0 used). Intent 4c: A→(4+4)/12=0.67,
    /// B→(0+4)/12=0.33 → A.
    #[test]
    fn ffd_most_allocated_bin_select() {
        let mut a = node("A", "h", CapacityType::Spot, 12, 64 * GI, 100 * GI);
        a.requested = (4, 0, 0);
        let b = node("B", "h", CapacityType::Spot, 12, 64 * GI, 100 * GI);
        let intents = [intent("x", 4, GI, &[("h", CapacityType::Spot)])];
        let (p, u) = simulate(&intents, &[a, b], &CellSketches::default());
        assert!(u.is_empty());
        assert_eq!(placed_on(&p, "x"), "A");
    }

    /// MostAllocated tracks the running tally: after placing on B,
    /// the next intent sees B as more-allocated than A.
    #[test]
    fn ffd_most_allocated_tracks_running_free() {
        let nodes = [
            node("A", "h", CapacityType::Spot, 8, 64 * GI, 100 * GI),
            node("B", "h", CapacityType::Spot, 8, 64 * GI, 100 * GI),
        ];
        // Three 3c intents. First → B (equal score, max_by last).
        // Second: A=3/8, B=6/8 → B. Third: B free=2<3 → A.
        let intents = [
            intent("i1", 3, GI, &[("h", CapacityType::Spot)]),
            intent("i2", 3, GI, &[("h", CapacityType::Spot)]),
            intent("i3", 3, GI, &[("h", CapacityType::Spot)]),
        ];
        let (p, u) = simulate(&intents, &nodes, &CellSketches::default());
        assert!(u.is_empty());
        assert_eq!(placed_on(&p, "i1"), "B");
        assert_eq!(placed_on(&p, "i2"), "B");
        assert_eq!(placed_on(&p, "i3"), "A");
    }

    /// Forecast intent with `eta=40s`, A={h1,h2}. lead_time[h1]=30,
    /// lead_time[h2]=60. A_open={h2}; h1 nodes ineligible.
    #[test]
    fn ffd_a_open_gates_forecast_by_lead_time() {
        let mut sk = CellSketches::default();
        sk.cell_mut(&Cell("h1".into(), CapacityType::Spot))
            .z_active
            .add(30.0);
        sk.cell_mut(&Cell("h2".into(), CapacityType::Spot))
            .z_active
            .add(60.0);
        let nodes = [
            node("n-h1", "h1", CapacityType::Spot, 8, 64 * GI, 100 * GI),
            node("n-h2", "h2", CapacityType::Spot, 8, 64 * GI, 100 * GI),
        ];
        let i = forecast(
            intent(
                "fc",
                4,
                GI,
                &[("h1", CapacityType::Spot), ("h2", CapacityType::Spot)],
            ),
            40.0,
        );
        // A_open directly: only h2.
        assert_eq!(a_open(&i, &sk), vec![Cell("h2".into(), CapacityType::Spot)]);
        let (p, u) = simulate(&[i], &nodes, &sk);
        assert!(u.is_empty());
        assert_eq!(placed_on(&p, "fc"), "n-h2");
        // Ready intent on the same cells ignores lead_time.
        let r = intent(
            "rd",
            4,
            GI,
            &[("h1", CapacityType::Spot), ("h2", CapacityType::Spot)],
        );
        assert_eq!(a_open(&r, &sk).len(), 2);
    }

    #[test]
    fn ffd_affinity_mismatch_unplaced() {
        let nodes = [node("n", "h1", CapacityType::Spot, 8, 64 * GI, 100 * GI)];
        let intents = [intent("x", 4, GI, &[("h2", CapacityType::Spot)])];
        let (p, u) = simulate(&intents, &nodes, &CellSketches::default());
        assert!(p.is_empty());
        assert_eq!(u.len(), 1);
        // Same hw, wrong cap → also mismatch.
        let intents = [intent("y", 4, GI, &[("h1", CapacityType::OnDemand)])];
        let (p, u) = simulate(&intents, &nodes, &CellSketches::default());
        assert!(p.is_empty());
        assert_eq!(u.len(), 1);
    }

    #[test]
    fn ffd_cell_less_node_ineligible() {
        let mut n = node("n", "h", CapacityType::Spot, 8, 64 * GI, 100 * GI);
        n.cell = None;
        let intents = [intent("x", 4, GI, &[("h", CapacityType::Spot)])];
        let (p, u) = simulate(&intents, &[n], &CellSketches::default());
        assert!(p.is_empty());
        assert_eq!(u.len(), 1);
    }

    #[test]
    fn ffd_hw_agnostic_intent_unplaced() {
        // Empty node_affinity → cells_of empty → A_open empty → no
        // node matches. cover_deficit (B8) handles these via the
        // legacy path / hw-agnostic NodeClass.
        let nodes = [node("n", "h", CapacityType::Spot, 8, 64 * GI, 100 * GI)];
        let i = SpawnIntent {
            intent_id: "agn".into(),
            cores: 4,
            ready: Some(true),
            ..Default::default()
        };
        let (p, u) = simulate(&[i], &nodes, &CellSketches::default());
        assert!(p.is_empty());
        assert_eq!(u.len(), 1);
    }

    #[test]
    fn ffd_in_flight_node_placement_flagged() {
        let mut n = node("n", "h", CapacityType::Spot, 8, 64 * GI, 100 * GI);
        n.registered = false;
        let intents = [intent("x", 4, GI, &[("h", CapacityType::Spot)])];
        let (p, _) = simulate(&intents, &[n], &CellSketches::default());
        assert_eq!(p.len(), 1);
        assert!(p[0].2, "in_flight = !registered");
    }

    /// FFD never overcommits any node on any axis. Deterministic
    /// many-intent / many-node check (proptest equivalent without the
    /// dep): three node sizes, intents that overflow total capacity.
    #[test]
    fn ffd_never_overcommits() {
        let h = ("h", CapacityType::Spot);
        let nodes = [
            node("s", "h", CapacityType::Spot, 4, 8 * GI, 50 * GI),
            node("m", "h", CapacityType::Spot, 8, 32 * GI, 100 * GI),
            node("l", "h", CapacityType::Spot, 16, 64 * GI, 200 * GI),
        ];
        // 20 intents × 3c = 60c demand; capacity = 28c → ≤9 place.
        let intents: Vec<_> = (0..20)
            .map(|k| intent(&format!("i{k}"), 3, 4 * GI, &[h]))
            .collect();
        let (p, u) = simulate(&intents, &nodes, &CellSketches::default());
        assert_eq!(p.len() + u.len(), 20);
        for n in &nodes {
            let (c, m, d) = p
                .iter()
                .filter(|(_, nn, _)| nn == &n.name)
                .fold((0u32, 0u64, 0u64), |(c, m, d), (i, _, _)| {
                    (c + i.cores, m + i.mem_bytes, d + i.disk_bytes)
                });
            assert!(
                c <= n.allocatable.0,
                "{}: cpu {} > {}",
                n.name,
                c,
                n.allocatable.0
            );
            assert!(m <= n.allocatable.1, "{}: mem", n.name);
            assert!(d <= n.allocatable.2, "{}: disk", n.name);
        }
        // Exactly ⌊4/3⌋+⌊8/3⌋+⌊16/3⌋ = 1+2+5 = 8 place (FFD packs
        // largest-first, here uniform 3c so just bin-fills).
        assert_eq!(p.len(), 8);
    }

    #[test]
    fn ffd_intent_id_tiebreak_stable() {
        // Equal (ready, cores, mem) → intent_id ascending. Ensures
        // deterministic placement across ticks (no flapping).
        let nodes = [node("n", "h", CapacityType::Spot, 4, 64 * GI, 100 * GI)];
        let intents = [
            intent("zz", 4, GI, &[("h", CapacityType::Spot)]),
            intent("aa", 4, GI, &[("h", CapacityType::Spot)]),
        ];
        let (p, u) = simulate(&intents, &nodes, &CellSketches::default());
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].0.intent_id, "aa", "intent_id asc tiebreak");
        assert_eq!(u[0].intent_id, "zz");
    }
}
