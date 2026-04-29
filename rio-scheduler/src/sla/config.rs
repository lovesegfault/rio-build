//! ADR-023 operator-facing SLA config: tier ladder, cold-start probe
//! shapes, hard ceilings. Loaded from `[sla]` in `scheduler.toml` (helm
//! `scheduler.sla`). Mandatory — every deployment carries an `[sla]`
//! block (helm renders it from chart defaults; tests use
//! [`SlaConfig::test_default`]).

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::solve::{Ceilings, Tier};

// r[impl sched.sla.hw-class.config]
/// One hw-class: a node-label conjunction (STAMPED on Nodes) plus the
/// Karpenter instance-type requirements that PROVISION that hardware.
/// Labels are ANDed within a class; classes are OR'd across the
/// `hw_classes` map when serialized to `nodeSelectorTerms`.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HwClassDef {
    /// `key=value` Node labels stamped post-launch (e.g.
    /// `rio.build/hw-band=mid`). Pod `nodeAffinity` matches these.
    pub labels: Vec<NodeLabelMatch>,
    /// Karpenter `spec.requirements` for NodeClaims targeting this
    /// hw-class — `karpenter.k8s.aws/instance-generation In [7]`,
    /// `kubernetes.io/arch In [amd64]`, etc. These are instance-TYPE
    /// properties Karpenter's discovery knows; `rio.build/*` labels
    /// are NOT (they're stamped on Nodes after launch). Putting a
    /// `rio.build/*` key here matches 0 instance types →
    /// `InsufficientCapacityError` → claim GC'd ~1s later.
    #[serde(default)]
    pub requirements: Vec<NodeSelectorReq>,
    /// `EC2NodeClass` name for NodeClaims targeting this hw-class
    /// (`rio-default` / `rio-nvme` / `rio-metal`). Per-class because
    /// the band-loop NodePool template this replaces selected the
    /// nodeClass by `$stor` — nvme classes need
    /// `instanceStorePolicy: RAID0` (only on `rio-nvme`); a single
    /// scalar default would launch nvme builders on the EBS root
    /// volume.
    #[serde(default)]
    pub node_class: String,
    /// Per-class capacity ceiling for `evaluate_cell`'s
    /// `ClassCeiling` gate — the configured catalog of what this
    /// hw-class's `requirements` *permit* Karpenter to launch, as
    /// distinct from `CostTable.cells` (what it has *observed*
    /// launching, which is a self-reinforcing sample). A `c*` above
    /// this is rejected for THIS class only; the global `[sla].maxCores`
    /// is the union ceiling. `0` rejected by `validate()`.
    #[serde(default)]
    pub max_cores: u32,
    /// Per-class memory ceiling — see [`Self::max_cores`].
    #[serde(default)]
    pub max_mem: u64,
}

/// `{key, operator, values}` — same shape as k8s
/// `NodeSelectorRequirement` / Karpenter's requirement entry. Local
/// struct so the TOML field names are stable (`key`/`operator`/
/// `values`) and `deny_unknown_fields` applies.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NodeSelectorReq {
    pub key: String,
    pub operator: String,
    #[serde(default)]
    pub values: Vec<String>,
}

/// Single `key=value` node-label match.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(deny_unknown_fields)]
pub struct NodeLabelMatch {
    pub key: String,
    pub value: String,
}

/// Karpenter capacity-type axis. Serialized lowercase; `"on-demand"`
/// accepted as alias for `od` (Karpenter's own label value).
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum CapacityType {
    Spot,
    #[serde(alias = "on-demand")]
    Od,
}

impl CapacityType {
    pub const ALL: [Self; 2] = [Self::Spot, Self::Od];

    /// `karpenter.sh/capacity-type` label value (the string Karpenter
    /// reads on `nodeSelectorTerms`).
    pub fn label(self) -> &'static str {
        match self {
            Self::Spot => "spot",
            Self::Od => "on-demand",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "spot" => Some(Self::Spot),
            "od" | "on-demand" => Some(Self::Od),
            _ => None,
        }
    }
}

/// `"h:cap"` ↔ `Cell` for the controller's `unfulfillable_cells` wire
/// encoding and `sla_ema_state.key` strings.
pub fn parse_cell(s: &str) -> Option<Cell> {
    let (h, c) = s.rsplit_once(':')?;
    Some((h.to_string(), CapacityType::parse(c)?))
}

pub fn cell_label((h, c): &Cell) -> String {
    format!("{h}:{}", c.label())
}

/// Operator-chosen hw-class identifier (key into
/// [`SlaConfig::hw_classes`]).
pub type HwClassName = String;

/// `(hw-class, capacity-type)` cell — the unit of capacity forecasting
/// and lead-time learning.
pub type Cell = (HwClassName, CapacityType);

/// `"intel-8-nvme:spot"` ↔ `("intel-8-nvme", Spot)` — string-keyed map
/// so the helm template / TOML can carry [`Cell`]-keyed tables without
/// nested objects.
mod cell_key_serde {
    use super::{CapacityType, Cell};
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;

    pub fn serialize<S: serde::Serializer>(
        m: &HashMap<Cell, f64>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        let flat: HashMap<String, f64> = m
            .iter()
            .map(|((h, c), v)| {
                let cap = match c {
                    CapacityType::Spot => "spot",
                    CapacityType::Od => "od",
                };
                (format!("{h}:{cap}"), *v)
            })
            .collect();
        flat.serialize(s)
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        d: D,
    ) -> Result<HashMap<Cell, f64>, D::Error> {
        let flat = HashMap::<String, f64>::deserialize(d)?;
        flat.into_iter()
            .map(|(k, v)| {
                let (h, c) = k
                    .rsplit_once(':')
                    .ok_or_else(|| serde::de::Error::custom("expected h:cap"))?;
                let cap = match c {
                    "spot" => CapacityType::Spot,
                    "od" | "on-demand" => CapacityType::Od,
                    _ => return Err(serde::de::Error::custom("cap must be spot|od")),
                };
                Ok(((h.to_string(), cap), v))
            })
            .collect()
    }
}

/// `[sla]` table. `deny_unknown_fields` so a typo'd key under `[sla]`
/// fails loud at startup instead of silently defaulting — this is also
/// what makes the legacy-softmax-field-rejection test work.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlaConfig {
    /// Tier ladder. [`SlaConfig::solve_tiers`] returns these sorted
    /// tightest-first regardless of TOML order; helm renders them
    /// pre-sorted anyway so the sort is belt-and-suspenders.
    pub tiers: Vec<Tier>,
    /// Tier name builds land in unless tenant overrides. MUST appear in
    /// `tiers` — checked by [`SlaConfig::validate`].
    pub default_tier: String,
    /// Cold-start probe sizing for never-seen `ModelKey`s.
    pub probe: ProbeShape,
    /// Per-`requiredSystemFeatures` probe overrides (e.g. `kvm` builds
    /// want a high mem floor regardless of core count). Missing key →
    /// fall back to `probe`.
    #[serde(default)]
    pub feature_probes: HashMap<String, ProbeShape>,
    /// Hard cap on `c*` — solve REJECTS a tier whose `c*` exceeds this
    /// (does not clamp). Also caps the explore halve/×4 walk.
    pub max_cores: f64,
    pub max_mem: u64,
    pub max_disk: u64,
    /// Disk request when no `disk_p90` sample exists yet.
    pub default_disk: u64,
    /// Per-key sample ring (rows kept for refit). Feeds
    /// [`super::SlaEstimator::new`].
    #[serde(default = "default_ring_buffer")]
    pub ring_buffer: u32,
    /// JSON [`super::prior::SeedCorpus`] loaded at startup into the
    /// seed-prior table. ADR-023 §2.10: lets a fresh deployment skip
    /// the cold-start probe ladder for known pnames. Unset → seed table
    /// starts empty (still fillable via `ImportSlaCorpus`).
    #[serde(default)]
    pub seed_corpus: Option<PathBuf>,
    /// Phase-13 hw-band cost source. `Static` → seed prices only;
    /// `Spot` → live EC2 spot-price poll (lease-gated; IRSA
    /// `ec2:DescribeSpotPriceHistory` on the scheduler SA).
    pub hw_cost_source: super::cost::HwCostSource,
    /// h → node-label conjunction. MANDATORY (ADR-023 §13a). Non-empty
    /// — checked by [`SlaConfig::validate`].
    pub hw_classes: HashMap<HwClassName, HwClassDef>,
    /// Admissible-set cost slack: a `(h, cap)` cell within
    /// `(1 + hw_cost_tolerance)` × min-cost stays admissible. Range
    /// `[0, 0.5]` — checked by [`SlaConfig::validate`].
    #[serde(default = "default_hw_cost_tolerance")]
    pub hw_cost_tolerance: f64,
    /// ε-greedy explore rate over the admissible set. Range `[0, 0.2]`.
    #[serde(default = "default_hw_explore_epsilon")]
    pub hw_explore_epsilon: f64,
    /// hw-bench pod memory floor (bytes). The K=3 STREAM-triad bench
    /// must out-size LLC; below this the bench is not scheduled.
    #[serde(default = "default_hw_bench_mem_floor")]
    pub hw_bench_mem_floor: u64,
    /// Per-`(h, cap)` cold-start lead-time prior (seconds). Keys are
    /// `"h:cap"` strings (`cell_key_serde` handles the flatten).
    #[serde(default, with = "cell_key_serde")]
    pub lead_time_seed: HashMap<Cell, f64>,
    /// Fleet-wide core ceiling for the forecast pass.
    #[serde(default = "default_max_fleet_cores")]
    pub max_fleet_cores: u32,
    /// Seconds the explore ladder may spend across all rungs before
    /// the build is forced onto the floor tier.
    #[serde(default = "default_ladder_budget")]
    pub ladder_budget: f64,
    /// hw-class whose bench result anchors the ref-second normalization.
    /// Immutable across restarts unless `--allow-reference-change` is
    /// passed — see [`super::check_reference_epoch`]. MUST appear in
    /// `hw_classes`.
    pub reference_hw_class: HwClassName,
    /// §Threat-model gap (d): per-tenant cap on forecast cores so one
    /// tenant's DAG can't crowd out the fleet forecast.
    #[serde(default = "default_max_forecast_cores_per_tenant")]
    pub max_forecast_cores_per_tenant: u32,
    /// §Threat-model: per-tenant `Estimator` cache cap (LRU evicts
    /// past this).
    #[serde(default = "default_max_keys_per_tenant")]
    pub max_keys_per_tenant: usize,
    /// Part-B: NodeClaim lead-time ceiling (seconds) before the cell is
    /// marked infeasible for the tick.
    #[serde(default = "default_max_lead_time")]
    pub max_lead_time: f64,
    /// Part-B: NodeClaim consolidation grace (seconds). `None` →
    /// Karpenter default.
    #[serde(default)]
    pub max_consolidation_time: Option<f64>,
    /// Part-B: ε-greedy explore rate for the consolidation pass.
    #[serde(default = "default_consolidate_explore_epsilon")]
    pub consolidate_explore_epsilon: f64,
    /// Part-B: per-tick NodeClaim creation throttle per cell.
    #[serde(default = "default_max_node_claims_per_cell_per_tick")]
    pub max_node_claims_per_cell_per_tick: u32,
    /// Cluster identifier for `sla_ema_state` / `interrupt_samples`
    /// scoping. ADR-023 §2.13: under the global-DB topology multiple
    /// regions share one PG; without this every scheduler upserts the
    /// SAME `key` and reads every region's interrupt rows. Helm sets
    /// `scheduler.sla.cluster = .Values.karpenter.clusterName`.
    /// Empty (single-cluster default) matches the migration-043
    /// `DEFAULT ''` so greenfield deploys need no config.
    #[serde(default)]
    pub cluster: String,
}

fn default_hw_cost_tolerance() -> f64 {
    0.15
}
fn default_hw_explore_epsilon() -> f64 {
    0.02
}
fn default_hw_bench_mem_floor() -> u64 {
    8 * 1024 * 1024 * 1024
}
fn default_max_fleet_cores() -> u32 {
    10_000
}
fn default_ladder_budget() -> f64 {
    600.0
}
fn default_max_forecast_cores_per_tenant() -> u32 {
    2_000
}
fn default_max_keys_per_tenant() -> usize {
    50_000
}
pub(super) fn default_max_lead_time() -> f64 {
    600.0
}
fn default_consolidate_explore_epsilon() -> f64 {
    0.02
}
fn default_max_node_claims_per_cell_per_tick() -> u32 {
    8
}

fn default_ring_buffer() -> u32 {
    32
}

/// Cold-start probe shape: `mem = mem_base + cpu × mem_per_core`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeShape {
    pub cpu: f64,
    pub mem_per_core: u64,
    pub mem_base: u64,
    /// `activeDeadlineSeconds` for unfitted (probe/explore) builds —
    /// D7 in the legacy-sizer removal: fitted keys derive a deadline
    /// from `wall_p99`, unfitted ones fall back to this.
    #[serde(default = "default_probe_deadline_secs")]
    pub deadline_secs: u32,
}

pub(crate) fn default_probe_deadline_secs() -> u32 {
    3600
}

impl ProbeShape {
    /// Bounds-check this shape under `[sla].max_cores = max_cores`.
    /// `label` names the config path (`"sla.probe"` /
    /// `"sla.feature_probes[kvm]"`) so the error message points the
    /// operator at the field. Validating here (not inline in
    /// [`SlaConfig::validate`]) means a future ProbeShape field can't
    /// be half-validated — there is one method, called from every
    /// `[sla]` site that holds a `ProbeShape`.
    pub fn validate(&self, label: &str, max_cores: f64) -> anyhow::Result<()> {
        let hi = max_cores / 4.0;
        anyhow::ensure!(
            self.cpu >= 4.0 && self.cpu <= hi,
            "{label}.cpu must be in [4, max_cores/4={hi}] so both explore \
             paths reach span≥4; got {} with max_cores={max_cores}",
            self.cpu
        );
        // `solve_intent_for` floors `SpawnIntent.deadline_secs` at the
        // probe value; the controller takes it verbatim as
        // `activeDeadlineSeconds` and derives the worker's
        // `daemon_timeout = deadline − 90s`. At the old 60s floor both
        // timers tied (the worker `.max(60)` clamp masked the negative
        // slack) so K8s SIGKILL raced `CompletionReport{TimedOut}`.
        // 180s leaves the 90s slack + ~30s cold-start + a meaningful
        // build window.
        anyhow::ensure!(
            self.deadline_secs >= 180,
            "{label}.deadline_secs must be >= 180, got {}",
            self.deadline_secs
        );
        Ok(())
    }
}

impl SlaConfig {
    /// Upper bound for time-domain seed-corpus parameters (S, P, Q) in
    /// reference-seconds, for `r[sched.sla.threat.corpus-clamp]`. Not a
    /// real timeout — a "this is pathological" gate. `P` is the 1-core
    /// parallel time (`T(1)·1` in the Amdahl basis) so it can legitimately
    /// be `wall × cores`; the bound is therefore `7d × max_cores` (~38 Ms
    /// at `max_cores=64`, well above any real seed but well below the
    /// `1e12` / `f64::MAX` an adversary would inject to NaN/Inf the
    /// solver).
    pub fn build_timeout_ref(&self) -> f64 {
        7.0 * 86400.0 * self.max_cores
    }

    /// Minimal `[sla]` block for tests and `Default for DagActorConfig`:
    /// single best-effort tier, 1-core probe, tiny ceilings sized for a
    /// VM-test pool. Production deployments override every field via
    /// helm `scheduler.sla`; this exists so a bare `DagActor::new(..,
    /// Default::default(), ..)` produces a usable actor without each
    /// test hand-rolling an `[sla]` literal.
    pub fn test_default() -> Self {
        Self {
            tiers: vec![Tier {
                name: "normal".into(),
                p50: None,
                p90: None,
                p99: None,
            }],
            default_tier: "normal".into(),
            probe: ProbeShape {
                cpu: 4.0,
                mem_per_core: 1 << 30,
                mem_base: 1 << 30,
                deadline_secs: default_probe_deadline_secs(),
            },
            feature_probes: HashMap::new(),
            max_cores: 16.0,
            max_mem: 2 << 30,
            max_disk: 6 << 30,
            default_disk: 2 << 30,
            ring_buffer: default_ring_buffer(),
            seed_corpus: None,
            hw_cost_source: super::cost::HwCostSource::Static,
            hw_classes: HashMap::from([(
                "test-hw".into(),
                HwClassDef {
                    labels: vec![NodeLabelMatch {
                        key: "rio.build/hw-class".into(),
                        value: "test-hw".into(),
                    }],
                    requirements: vec![NodeSelectorReq {
                        key: "kubernetes.io/os".into(),
                        operator: "In".into(),
                        values: vec!["linux".into()],
                    }],
                    node_class: "rio-default".into(),
                    max_cores: 16,
                    max_mem: 2 << 30,
                },
            )]),
            hw_cost_tolerance: default_hw_cost_tolerance(),
            hw_explore_epsilon: default_hw_explore_epsilon(),
            hw_bench_mem_floor: default_hw_bench_mem_floor(),
            lead_time_seed: HashMap::new(),
            max_fleet_cores: default_max_fleet_cores(),
            ladder_budget: default_ladder_budget(),
            reference_hw_class: "test-hw".into(),
            max_forecast_cores_per_tenant: default_max_forecast_cores_per_tenant(),
            max_keys_per_tenant: default_max_keys_per_tenant(),
            max_lead_time: default_max_lead_time(),
            max_consolidation_time: None,
            consolidate_explore_epsilon: default_consolidate_explore_epsilon(),
            max_node_claims_per_cell_per_tick: default_max_node_claims_per_cell_per_tick(),
            cluster: String::new(),
        }
    }

    /// Startup-time bounds checks. `&self` (not `&mut`) so it composes
    /// with [`rio_common::config::ValidateConfig::validate`]; sorting
    /// is provided separately by [`Self::solve_tiers`].
    ///
    /// `probe.cpu ∈ [4, max_cores/4]`: gives the explore walk span≥4 on
    /// both halve and ×4 sides. `max_cores < 1024` keeps the
    /// PriorityClass-bucket index in range (Part-B packs cores into
    /// `1..1024` PriorityClass values). The two together force
    /// `max_cores ≥ 16` — VM-test pools satisfy this via
    /// [`Self::test_default`].
    pub fn validate(&self) -> anyhow::Result<()> {
        let hi = self.max_cores;
        anyhow::ensure!(
            self.tiers.iter().any(|t| t.name == self.default_tier),
            "sla.default_tier {:?} not in sla.tiers (known: {:?})",
            self.default_tier,
            self.tiers.iter().map(|t| &t.name).collect::<Vec<_>>()
        );
        for t in &self.tiers {
            t.validate()?;
        }
        anyhow::ensure!(
            self.max_cores.is_finite() && self.max_cores > 0.0,
            "sla.max_cores must be finite and positive, got {}",
            self.max_cores
        );
        anyhow::ensure!(
            self.max_cores < 1024.0,
            "sla.maxCores < 1024 required (PriorityClass bucket range), got {}",
            self.max_cores
        );
        anyhow::ensure!(
            (0.0..=0.5).contains(&self.hw_cost_tolerance),
            "sla.hwCostTolerance must be in [0, 0.5], got {}",
            self.hw_cost_tolerance
        );
        anyhow::ensure!(
            (0.0..=0.2).contains(&self.hw_explore_epsilon),
            "sla.hwExploreEpsilon must be in [0, 0.2], got {}",
            self.hw_explore_epsilon
        );
        for (h, def) in &self.hw_classes {
            // bug_038: the charset constraint is enforced at every gRPC
            // sink (`AppendHwPerfSample`, `AppendInterruptSample`) but
            // was NOT checked here at the trusted-but-fallible source.
            // An operator key like `c7a.xlarge` booted cleanly; every
            // sample for it was then silently rejected with `warn!` only.
            anyhow::ensure!(
                rio_common::limits::is_hw_class_name(h),
                "sla.hwClasses key {h:?} must match [a-z0-9-]{{1,64}} \
                 (rejected by AppendHwPerfSample / AppendInterruptSample otherwise)"
            );
            anyhow::ensure!(
                !def.labels.is_empty(),
                "sla.hwClasses[{h}].labels must be non-empty"
            );
            anyhow::ensure!(
                !def.requirements.is_empty(),
                "sla.hwClasses[{h}].requirements must be non-empty \
                 (Karpenter instance-type selectors, e.g. \
                 karpenter.k8s.aws/instance-generation In [7])"
            );
            anyhow::ensure!(
                !def.node_class.is_empty(),
                "sla.hwClasses[{h}].node_class must name an EC2NodeClass \
                 (rio-default / rio-nvme / rio-metal)"
            );
            anyhow::ensure!(
                def.max_cores > 0 && f64::from(def.max_cores) <= self.max_cores,
                "sla.hwClasses[{h}].max_cores must be in [1, sla.maxCores={}], got {}",
                self.max_cores,
                def.max_cores
            );
            anyhow::ensure!(
                def.max_mem > 0 && def.max_mem <= self.max_mem,
                "sla.hwClasses[{h}].max_mem must be in [1, sla.maxMem={}], got {}",
                self.max_mem,
                def.max_mem
            );
            for r in &def.requirements {
                anyhow::ensure!(
                    !r.key.starts_with("rio.build/"),
                    "sla.hwClasses[{h}].requirements key {:?} is a Node-stamp \
                     label, not an instance-type property — Karpenter's \
                     discovery doesn't know it (matches 0 types). Put it in \
                     `labels` instead.",
                    r.key
                );
            }
        }
        anyhow::ensure!(
            !self.hw_classes.is_empty(),
            "sla.hwClasses is mandatory (ADR-023 §13a; populate scheduler.sla.hwClasses in helm values)"
        );
        anyhow::ensure!(
            self.hw_classes.contains_key(&self.reference_hw_class),
            "sla.referenceHwClass={} not in sla.hwClasses",
            self.reference_hw_class
        );
        for ((h, cap), v) in &self.lead_time_seed {
            anyhow::ensure!(
                self.hw_classes.contains_key(h),
                "sla.leadTimeSeed key {h:?} not in sla.hwClasses"
            );
            anyhow::ensure!(
                v.is_finite() && *v > 0.0 && *v <= self.max_lead_time,
                "sla.leadTimeSeed[{h}:{cap:?}] = {v} must be in (0, maxLeadTime={}]",
                self.max_lead_time
            );
        }
        self.probe.validate("sla.probe", hi)?;
        for (feat, p) in &self.feature_probes {
            p.validate(&format!("sla.feature_probes[{feat}]"), hi)?;
        }
        Ok(())
    }

    /// Tiers sorted tightest-first (lowest target wins; a tier with no
    /// targets sorts last). [`super::solve::solve_tier`] iterates in
    /// order and returns the first feasible tier, so tightest-first
    /// means a build that CAN hit `fast` does, instead of settling for
    /// `normal`.
    pub fn solve_tiers(&self) -> Vec<Tier> {
        let mut tiers = self.tiers.clone();
        // Sort by `Tier::binding_bound` so the sort key agrees with
        // `reassign_tier` / `explore::tier_target` on what "tightest"
        // means; no-bounds tiers (None) sort last.
        tiers.sort_by_key(|t| {
            t.binding_bound()
                .map(|d| (d * 1000.0) as u64)
                .unwrap_or(u64::MAX)
        });
        tiers
    }

    /// Hard ceilings for [`super::solve::solve_tier`].
    pub fn ceilings(&self) -> Ceilings {
        Ceilings {
            max_cores: self.max_cores,
            max_mem: self.max_mem,
            max_disk: self.max_disk,
            default_disk: self.default_disk,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal valid `requirements` for test fixtures (validate()
    /// requires non-empty + no `rio.build/*`).
    fn test_req() -> Vec<NodeSelectorReq> {
        vec![NodeSelectorReq {
            key: "kubernetes.io/os".into(),
            operator: "In".into(),
            values: vec!["linux".into()],
        }]
    }

    /// Minimal valid `HwClassDef` with a `(k,v)` label.
    fn test_def(k: &str, v: &str) -> HwClassDef {
        HwClassDef {
            labels: vec![NodeLabelMatch {
                key: k.into(),
                value: v.into(),
            }],
            requirements: test_req(),
            node_class: "rio-default".into(),
            max_cores: 64,
            max_mem: 256 << 30,
        }
    }

    fn base() -> SlaConfig {
        SlaConfig {
            tiers: vec![Tier {
                name: "normal".into(),
                p50: None,
                p90: Some(1200.0),
                p99: None,
            }],
            probe: ProbeShape {
                cpu: 4.0,
                mem_per_core: 2 << 30,
                mem_base: 4 << 30,
                deadline_secs: default_probe_deadline_secs(),
            },
            max_cores: 64.0,
            max_mem: 256 << 30,
            max_disk: 200 << 30,
            default_disk: 20 << 30,
            ..SlaConfig::test_default()
        }
    }

    #[test]
    fn rejects_probe_cpu_outside_span_range() {
        let mut cfg = base();
        cfg.probe.cpu = 32.0; // > max_cores/4 = 16
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("sla.probe.cpu"), "{err}");
        cfg.probe.cpu = 2.0; // < 4
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_nonpositive_tier_bound() {
        let mut cfg = base();
        // Negative: `(d * 1000.0) as u64` would wrap to 0 → broken tier
        // sorts as "tightest" in solve_tiers().
        cfg.tiers[0].p90 = Some(-300.0);
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("tiers[normal].p90") && err.contains("-300"),
            "{err}"
        );
        // NaN: same wrap, plus NaN poisons binding_bound() comparisons.
        cfg.tiers[0].p90 = None;
        cfg.tiers[0].p50 = Some(f64::NAN);
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("tiers[normal].p50"), "{err}");
        // Zero: degenerate (no build can hit a 0s target).
        cfg.tiers[0].p50 = None;
        cfg.tiers[0].p99 = Some(0.0);
        assert!(cfg.validate().is_err());
        // Positive control.
        cfg.tiers[0].p99 = Some(300.0);
        cfg.validate().unwrap();
    }

    #[test]
    fn rejects_unknown_default_tier() {
        let mut cfg = base();
        cfg.default_tier = "fast".into();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("not in sla.tiers"), "{err}");
    }

    #[test]
    fn accepts_probe_cpu_at_bounds() {
        let mut cfg = base();
        cfg.probe.cpu = 4.0;
        cfg.validate().unwrap();
        cfg.probe.cpu = 16.0; // = max_cores/4
        cfg.validate().unwrap();
    }

    #[test]
    fn rejects_probe_deadline_under_180() {
        let mut cfg = base();
        cfg.probe.deadline_secs = 60;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("sla.probe.deadline_secs must be >= 180"),
            "{err}"
        );
        cfg.probe.deadline_secs = 180;
        cfg.validate().unwrap();

        cfg.feature_probes.insert(
            "kvm".into(),
            ProbeShape {
                cpu: 4.0,
                mem_per_core: 0,
                mem_base: 0,
                deadline_secs: 120,
            },
        );
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("feature_probes[kvm]"), "{err}");
    }

    #[test]
    fn rejects_feature_probe_cpu_out_of_range() {
        let mut cfg = base();
        cfg.feature_probes.insert(
            "kvm".into(),
            ProbeShape {
                cpu: 96.0, // > max_cores=64
                mem_per_core: 0,
                mem_base: 0,
                deadline_secs: 3600,
            },
        );
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("feature_probes[kvm].cpu") && err.contains("max_cores=64"),
            "{err}"
        );
        cfg.feature_probes.get_mut("kvm").unwrap().cpu = 2.0;
        assert!(cfg.validate().is_err(), "<4 also rejected");
        cfg.feature_probes.get_mut("kvm").unwrap().cpu = 16.0;
        cfg.validate().unwrap();
    }

    #[test]
    fn rejects_unknown_probe_field() {
        // `deadline_sec` (no trailing s) — typo'd key under a nested
        // struct must fail loud at deserialize, not silently default.
        let r: Result<ProbeShape, _> = serde_json::from_str(
            r#"{"cpu":4.0,"mem_per_core":1,"mem_base":1,"deadline_sec":7200}"#,
        );
        assert!(
            r.unwrap_err().to_string().contains("unknown field"),
            "ProbeShape must deny_unknown_fields"
        );
    }

    #[test]
    fn rejects_unknown_tier_field() {
        // `p9O` (letter O, not zero).
        let r: Result<Tier, _> = serde_json::from_str(r#"{"name":"x","p9O":300}"#);
        assert!(
            r.unwrap_err().to_string().contains("unknown field"),
            "Tier must deny_unknown_fields"
        );
    }

    #[test]
    fn rejects_feature_probe_cpu_gt_maxcores() {
        let mut cfg = base();
        cfg.feature_probes.insert(
            "kvm".into(),
            ProbeShape {
                cpu: 96.0, // > max_cores=64
                mem_per_core: 0,
                mem_base: 0,
                deadline_secs: 3600,
            },
        );
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("feature_probes[kvm]"), "{err}");
        assert!(err.contains("[4, max_cores/4=16]"), "{err}");
    }

    #[test]
    fn probe_deadline_secs_defaults_when_absent() {
        let p: ProbeShape = serde_json::from_str(
            r#"{"cpu": 4.0, "mem_per_core": 2147483648, "mem_base": 4294967296}"#,
        )
        .unwrap();
        assert_eq!(p.deadline_secs, 3600);
    }

    #[test]
    fn tiers_sorted_tightest_first() {
        let mut cfg = base();
        cfg.tiers = vec![
            Tier {
                name: "best-effort".into(),
                p50: None,
                p90: None,
                p99: None,
            },
            Tier {
                name: "slow".into(),
                p50: None,
                p90: Some(3600.0),
                p99: None,
            },
            Tier {
                name: "fast".into(),
                p50: Some(180.0),
                p90: Some(300.0),
                p99: Some(480.0),
            },
            Tier {
                name: "normal".into(),
                p50: Some(720.0),
                p90: Some(1200.0),
                p99: None,
            },
        ];
        let sorted: Vec<_> = cfg.solve_tiers().into_iter().map(|t| t.name).collect();
        assert_eq!(sorted, ["fast", "normal", "slow", "best-effort"]);
    }

    #[test]
    fn ceilings_projection() {
        let c = base().ceilings();
        assert_eq!(c.max_cores, 64.0);
        assert_eq!(c.default_disk, 20 << 30);
    }

    #[test]
    fn hw_classes_parses_label_conjunction() {
        let toml = r#"
            tiers = [{ name = "normal" }]
            default_tier = "normal"
            max_cores = 64.0
            max_mem = 1
            max_disk = 1
            default_disk = 1
            hw_cost_tolerance = 0.15
            hw_explore_epsilon = 0.02
            hw_cost_source = "static"
            reference_hw_class = "intel-8-nvme"
            [probe]
            cpu = 4.0
            mem_per_core = 1
            mem_base = 1
            [hw_classes.intel-8-nvme]
            labels = [
              { key = "karpenter.k8s.aws/instance-cpu-manufacturer", value = "intel" },
              { key = "karpenter.k8s.aws/instance-generation", value = "8" },
              { key = "rio.build/storage", value = "nvme" },
            ]
            requirements = [
              { key = "karpenter.k8s.aws/instance-generation", operator = "In", values = ["8"] },
            ]
            node_class = "rio-nvme"
            max_cores = 64
            max_mem = 1
            [lead_time_seed]
            "intel-8-nvme:spot" = 45.0
            "intel-8-nvme:od" = 38.0
        "#;
        let sla: SlaConfig = toml::from_str(toml).unwrap();
        assert_eq!(sla.hw_classes.len(), 1);
        assert_eq!(sla.hw_classes["intel-8-nvme"].labels.len(), 3);
        assert_eq!(sla.hw_cost_tolerance, 0.15);
        assert_eq!(
            sla.lead_time_seed[&("intel-8-nvme".into(), CapacityType::Spot)],
            45.0
        );
        assert_eq!(
            sla.lead_time_seed[&("intel-8-nvme".into(), CapacityType::Od)],
            38.0
        );
        sla.validate().unwrap();
    }

    /// bug_038: `c7a.xlarge` (dot) booted cleanly, then every
    /// `AppendHwPerfSample` for it was silently rejected at the gRPC
    /// sink. The charset constraint MUST be enforced at the config
    /// source, not just the N untrusted sinks.
    // r[verify sched.sla.hw-class.config]
    #[test]
    fn validate_rejects_hw_class_dot() {
        let mut cfg = base();
        cfg.hw_classes
            .insert("c7a.xlarge".into(), test_def("k", "v"));
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("c7a.xlarge") && err.contains("[a-z0-9-]"),
            "{err}"
        );
        // Positive control: dash-separated key passes.
        cfg.hw_classes.remove("c7a.xlarge");
        cfg.hw_classes
            .insert("c7a-xlarge".into(), test_def("k", "v"));
        cfg.validate().unwrap();
    }

    // r[verify sched.sla.hw-class.config]
    #[test]
    fn rejects_empty_hw_classes() {
        let mut cfg = base();
        // Populated (from test_default) → valid.
        cfg.validate().unwrap();
        // Empty → ADR-023 §13a is mandatory; validate() must reject.
        cfg.hw_classes.clear();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("hwClasses is mandatory"), "{err}");
    }

    #[test]
    fn rejects_max_cores_ge_1024() {
        let mut cfg = base();
        cfg.max_cores = 1024.0;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("maxCores < 1024"), "{err}");
    }

    #[test]
    fn rejects_hw_cost_tolerance_out_of_range() {
        for bad in [-0.01, 0.6, f64::NAN] {
            let mut cfg = base();
            cfg.hw_cost_tolerance = bad;
            assert!(cfg.validate().is_err(), "{bad} should be rejected");
        }
        let mut cfg = base();
        cfg.hw_explore_epsilon = 0.3;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_reference_not_in_hw_classes() {
        let mut cfg = base();
        cfg.reference_hw_class = "nope".into();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("not in sla.hwClasses"), "{err}");
    }

    #[test]
    fn rejects_empty_hw_class_labels() {
        let mut cfg = base();
        cfg.hw_classes
            .insert("test-hw".into(), HwClassDef::default());
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("hwClasses[test-hw].labels"), "{err}");
    }

    /// `requirements` must be non-empty AND no `rio.build/*` keys
    /// (those are Node-stamps, invisible to Karpenter instance-type
    /// discovery — putting one here matched 0 types live and looped
    /// the controller).
    #[test]
    fn rejects_hw_class_requirements_shape() {
        let mut cfg = base();
        cfg.hw_classes
            .get_mut("test-hw")
            .unwrap()
            .requirements
            .clear();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("requirements must be non-empty"), "{err}");

        let mut cfg = base();
        cfg.hw_classes
            .get_mut("test-hw")
            .unwrap()
            .requirements
            .push(NodeSelectorReq {
                key: "rio.build/hw-band".into(),
                operator: "In".into(),
                values: vec!["mid".into()],
            });
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("rio.build/hw-band"), "{err}");
        assert!(err.contains("Node-stamp"), "{err}");

        let mut cfg = base();
        cfg.hw_classes
            .get_mut("test-hw")
            .unwrap()
            .node_class
            .clear();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("node_class must name an EC2NodeClass"),
            "{err}"
        );

        // Per-class ceilings: 0 rejected; > global rejected; = global ok.
        for (mc, mm, expect) in [
            (0, 1, Some("max_cores must be in [1")),
            (64, 0, Some("max_mem must be in [1")),
            (65, 1, Some("max_cores must be in [1, sla.maxCores=64]")),
            (64, (256u64 << 30) + 1, Some("max_mem must be in [1")),
            (64, 256 << 30, None),
        ] {
            let mut cfg = base();
            let def = cfg.hw_classes.get_mut("test-hw").unwrap();
            def.max_cores = mc;
            def.max_mem = mm;
            match (expect, cfg.validate()) {
                (Some(want), Err(e)) => {
                    assert!(e.to_string().contains(want), "({mc},{mm}): {e}")
                }
                (None, Ok(())) => {}
                (e, r) => panic!("({mc},{mm}): expect {e:?}, got {r:?}"),
            }
        }
    }

    #[test]
    fn legacy_softmax_fields_rejected() {
        // `deny_unknown_fields` makes the old keys hard-fail at
        // deserialize, not silently ignored.
        let toml = r#"
            tiers = [{ name = "normal" }]
            default_tier = "normal"
            max_cores = 64.0
            max_mem = 1
            max_disk = 1
            default_disk = 1
            hw_softmax_temp = 0.3
            [probe]
            cpu = 4.0
            mem_per_core = 1
            mem_base = 1
        "#;
        let err = toml::from_str::<SlaConfig>(toml).unwrap_err().to_string();
        assert!(err.contains("unknown field"), "{err}");
    }

    /// Class-level guard for merged_bug_056 (helm forgot
    /// `hw_cost_source` → §13a unreachable in production). Asserts
    /// every `[sla]` TOML key appears verbatim in the helm template,
    /// and that the key list itself is complete (so adding a field
    /// without listing it here ALSO fails).
    #[test]
    fn helm_renders_every_sla_key() {
        const TPL: &str = include_str!("../../../infra/helm/rio-build/templates/scheduler.yaml");

        // Snake-case TOML field names that the helm `[sla]` block MUST
        // render. `TPL.contains` is a substring check — the template
        // writes each as `name = ...` or `[sla.name]` so the bare
        // snake_case key is sufficient.
        const RENDERED: &[&str] = &[
            "tiers",
            "default_tier",
            "probe",
            "feature_probes",
            "max_cores",
            "max_mem",
            "max_disk",
            "default_disk",
            "hw_cost_source",
            "hw_classes",
            "hw_cost_tolerance",
            "hw_explore_epsilon",
            "hw_bench_mem_floor",
            "lead_time_seed",
            "max_fleet_cores",
            "ladder_budget",
            "reference_hw_class",
            "max_forecast_cores_per_tenant",
            "max_keys_per_tenant",
            "max_lead_time",
            "max_consolidation_time",
            "consolidate_explore_epsilon",
            "max_node_claims_per_cell_per_tick",
            "cluster",
        ];
        // Intentionally NOT helm-rendered (with rationale). Adding a
        // field here requires justifying why operators never set it.
        const NOT_RENDERED: &[(&str, &str)] = &[
            ("ring_buffer", "internal refit window; not operator-tuned"),
            (
                "seed_corpus",
                "file path — corpus loads via ImportSlaCorpus RPC in k8s",
            ),
        ];

        for k in RENDERED {
            assert!(
                TPL.contains(k),
                "[sla] key `{k}` not rendered by infra/helm/rio-build/templates/scheduler.yaml \
                 — add a `{{{{- with .X }}}}` block, or move to NOT_RENDERED with rationale"
            );
        }

        // Completeness: serde_json emits every struct field (incl.
        // `Option::None` as null, empty maps as `{{}}`); compare its
        // top-level key set against RENDERED ∪ NOT_RENDERED so a new
        // `SlaConfig` field that nobody listed fails here.
        let v = serde_json::to_value(SlaConfig::test_default()).unwrap();
        let actual: std::collections::BTreeSet<&str> =
            v.as_object().unwrap().keys().map(String::as_str).collect();
        let listed: std::collections::BTreeSet<&str> = RENDERED
            .iter()
            .copied()
            .chain(NOT_RENDERED.iter().map(|(k, _)| *k))
            .collect();
        assert_eq!(
            actual, listed,
            "\nSlaConfig serde fields ≠ RENDERED ∪ NOT_RENDERED — \
             add the new field to one of the two lists above"
        );
    }

    /// Tripwire: every map-keyed `SlaConfig` field whose key-space is
    /// drawn from another field (today: `hw_classes`) MUST be
    /// cross-field-checked by [`SlaConfig::validate`]. The exhaustive
    /// destructure below names EVERY field with NO `..` rest pattern,
    /// so adding a field to `SlaConfig` is a compile error here until
    /// it is classified. r2 bug_038 (`hw_classes` charset) and r6
    /// bug_019 (`lead_time_seed` membership/range) are the same
    /// "trusted-but-fallible source" gap; this test makes a third
    /// instance a build break instead of a round-7 finding.
    #[test]
    fn validate_covers_every_map_key() {
        let cfg = base();
        // Exhaustive destructure — NO `..`. Adding a field = compile
        // error here. Per-field comment classifies the key-space:
        //   (universe)  the field IS the reference set; nothing to check
        //   (free)      key-space is open (feature names, file paths) —
        //               no cross-field membership to enforce
        //   (cell)      key ⊇ HwClassName → MUST be ∈ hw_classes;
        //               assertion below
        //   (scalar)    not a map
        let SlaConfig {
            tiers: _,                             // (scalar) Vec<Tier>; per-tier validate()
            default_tier: _,                      // (scalar) checked ∈ tiers
            probe: _,                             // (scalar) ProbeShape::validate
            feature_probes: _,                    // (free)   key = requiredSystemFeatures string
            max_cores: _,                         // (scalar)
            max_mem: _,                           // (scalar)
            max_disk: _,                          // (scalar)
            default_disk: _,                      // (scalar)
            ring_buffer: _,                       // (scalar)
            seed_corpus: _,                       // (scalar) Option<PathBuf>
            hw_cost_source: _,                    // (scalar)
            hw_classes: _,                        // (universe) the reference set itself
            hw_cost_tolerance: _,                 // (scalar)
            hw_explore_epsilon: _,                // (scalar)
            hw_bench_mem_floor: _,                // (scalar)
            lead_time_seed,        // (cell)   key.0 MUST ∈ hw_classes — asserted below
            max_fleet_cores: _,    // (scalar)
            ladder_budget: _,      // (scalar)
            reference_hw_class: _, // (scalar) HwClassName; checked ∈ hw_classes
            max_forecast_cores_per_tenant: _, // (scalar)
            max_keys_per_tenant: _, // (scalar)
            max_lead_time: _,      // (scalar)
            max_consolidation_time: _, // (scalar)
            consolidate_explore_epsilon: _, // (scalar)
            max_node_claims_per_cell_per_tick: _, // (scalar)
            cluster: _,            // (scalar)
        } = cfg;
        // Silence unused-binding on the one (cell) field we kept by
        // name; the destructure itself is the load-bearing part.
        let _ = lead_time_seed;

        // ---- (cell) lead_time_seed: key.0 ∈ hw_classes ----
        let mut bad_key = base();
        bad_key
            .lead_time_seed
            .insert(("nonexistent".into(), CapacityType::Od), 30.0);
        let err = bad_key
            .validate()
            .expect_err("lead_time_seed key 'nonexistent' ∉ hw_classes must be rejected");
        assert!(
            err.to_string().contains("nonexistent"),
            "error should name the bad key: {err}"
        );

        // ---- (cell) lead_time_seed: value range ----
        // Give the key a real hw_class so only the VALUE is wrong.
        let with_h = || {
            let mut c = base();
            c.hw_classes
                .insert("intel-7".into(), test_def("rio.build/hw-class", "intel-7"));
            c
        };
        // Non-finite.
        let mut bad_val = with_h();
        bad_val
            .lead_time_seed
            .insert(("intel-7".into(), CapacityType::Spot), f64::NAN);
        assert!(
            bad_val.validate().is_err(),
            "non-finite lead_time_seed value must be rejected"
        );
        // > max_lead_time (default 600.0).
        let mut bad_val = with_h();
        bad_val
            .lead_time_seed
            .insert(("intel-7".into(), CapacityType::Spot), 6000.0);
        assert!(
            bad_val.validate().is_err(),
            "lead_time_seed value > max_lead_time must be rejected"
        );
        // Positive control: valid key + valid value passes.
        let mut ok = with_h();
        ok.lead_time_seed
            .insert(("intel-7".into(), CapacityType::Spot), 30.0);
        ok.validate().expect("valid lead_time_seed should pass");
    }

    #[test]
    fn cell_key_serde_roundtrip() {
        let mut cfg = base();
        cfg.lead_time_seed
            .insert(("h".into(), CapacityType::Spot), 1.0);
        cfg.lead_time_seed
            .insert(("h".into(), CapacityType::Od), 2.0);
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains(r#""h:spot":1.0"#), "{json}");
        let back: SlaConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.lead_time_seed, cfg.lead_time_seed);
    }
}
