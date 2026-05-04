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
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
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
    /// Node taints applied to NodeClaims targeting this hw-class
    /// (chained after the universal `rio.build/builder` taint).
    /// `r[ctrl.nodeclaim.taints.hwclass]`: e.g. metal classes carry
    /// `rio.build/kvm=true:NoSchedule` so non-kvm pods stay off.
    #[serde(default)]
    pub taints: Vec<NodeTaint>,
    /// `requiredSystemFeatures` this hw-class can host. The
    /// [`features_compatible`] bidirectional ∅-guard routes intents:
    /// a class with `provides_features=["kvm"]` accepts ONLY kvm
    /// intents; an empty `provides_features` accepts ONLY featureless
    /// intents. §13c: replaces the pre-§13c static metal NodePool
    /// routing.
    #[serde(default)]
    pub provides_features: Vec<String>,
    /// Per-hw-class fleet-core sub-budget. The controller's
    /// `cover_deficit` clamps this class's per-tick mint at
    /// `min(global_remaining, max_fleet_cores − live_h − created_h)`
    /// summed across capacity-types (per-hwClass, NOT per-Cell — a
    /// per-Cell cap would let spot+od each hit it independently → 2×
    /// $/hr exposure). `None` ⇒ global-only.
    #[serde(default)]
    pub max_fleet_cores: Option<u32>,
    /// Capacity-types this hw-class is permitted to provision.
    /// `r[sched.sla.hwclass.capacity-types]`: `solve_full` and the
    /// controller's `all_cells`/`fallback_cell` iterate THIS, not
    /// `CapacityType::ALL`, so an od-only class (e.g. metal) never
    /// generates a `(h, Spot)` cell — structurally preventing the
    /// conflicting-requirements ICE loop a requirement-based exclusion
    /// would cause. Default `[Spot, Od]` (both).
    #[serde(default = "default_capacity_types")]
    pub capacity_types: Vec<CapacityType>,
}

fn default_capacity_types() -> Vec<CapacityType> {
    CapacityType::ALL.to_vec()
}

/// Hand-rolled `Default` so `capacity_types` is `[Spot, Od]` (matching
/// the serde default), NOT `vec![]`. A derived `Default` would give an
/// empty Vec, which `validate()` rejects and would silently make every
/// `..Default::default()` test fixture unprovisionable.
impl Default for HwClassDef {
    fn default() -> Self {
        Self {
            labels: Vec::new(),
            requirements: Vec::new(),
            node_class: String::new(),
            max_cores: 0,
            max_mem: 0,
            taints: Vec::new(),
            provides_features: Vec::new(),
            max_fleet_cores: None,
            capacity_types: default_capacity_types(),
        }
    }
}

/// `{key, value, effect}` Node taint. Same shape as k8s
/// `core/v1.Taint` (minus `timeAdded`). Local struct so the TOML field
/// names are stable and `deny_unknown_fields` applies.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NodeTaint {
    pub key: String,
    #[serde(default)]
    pub value: String,
    pub effect: String,
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

/// `kubernetes.io/arch` — the well-known node label every kubelet
/// registers. Used by [`SlaConfig::reference_hw_class_for_system`] to
/// arch-match an `HwClassDef`'s labels against `SpawnIntent.system`.
pub const ARCH_LABEL: &str = "kubernetes.io/arch";

// r[impl sched.sla.hwclass.provides.bidir]
/// Bidirectional ∅-guard feature-match predicate. Single source for
/// the §13c/D3/D10/I-181 routing rule — open-coding this at ≥4 sites
/// (T2/T10/D10 chokepoint + worker `passes_intent_filter`) lets them
/// drift, and drift here is "kvm intent routed to non-kvm cell" or
/// "metal node absorbs non-kvm build".
///
/// `true` iff every `required` feature is in `provides` AND
/// `required.is_empty() == provides.is_empty()`. The second clause is
/// the bidirectional guard: a class providing `[kvm]` rejects
/// featureless intents (so metal doesn't absorb non-kvm); a class
/// providing `[]` rejects `[kvm]` intents (so non-metal isn't picked
/// for kvm — `[]⊆anything` would otherwise let it through). Subset (not
/// equality) on the populated side keeps the door open for
/// `provides=[kvm, big-parallel]` hosting `required=[kvm]`.
pub fn features_compatible(required: &[String], provides: &[String]) -> bool {
    required.iter().all(|f| provides.contains(f)) && required.is_empty() == provides.is_empty()
}

impl SlaConfig {
    /// `[sla.hw_classes.$h].capacity_types`. Unknown `h` → `ALL` (no
    /// restriction). §Mode-invariant: every cell-set generator
    /// (`solve_full`, controller `all_cells`/`fallback_cell`) reads
    /// THIS so an od-only class structurally never produces a
    /// `(h, Spot)` cell.
    pub fn capacity_types_for(&self, h: &str) -> &[CapacityType] {
        self.hw_classes
            .get(h)
            .map_or(CapacityType::ALL.as_slice(), |d| &d.capacity_types)
    }

    /// `[sla.hw_classes.$h].provides_features`. Unknown `h` → `&[]`.
    pub fn provides_for(&self, h: &str) -> &[String] {
        self.hw_classes.get(h).map_or(&[], |d| &d.provides_features)
    }

    /// Per-class `(max_cores, max_mem)` from `hw_classes[h]`. Mirrors
    /// the controller's `HwClassConfig::ceilings_for`. Unknown class →
    /// `(u32::MAX, u64::MAX)` (no per-class ceiling — global only).
    /// Disk has no per-class ceiling (global-only via `SlaCeilings`);
    /// the chokepoint filters cores+mem only by design.
    ///
    /// §Partition-single-source: every scheduler-side per-class ceiling
    /// check (`solve_full`, [`Self::reference_hw_class_for_system`],
    /// the `all_candidates` capacity-fallback, the post-finalize
    /// chokepoint [`Self::retain_hosting_classes`]) calls THIS.
    pub fn class_ceilings(&self, h: &str) -> (u32, u64) {
        self.hw_classes
            .get(h)
            .map_or((u32::MAX, u64::MAX), |d| (d.max_cores, d.max_mem))
    }

    /// `reference_hw_class` if its `kubernetes.io/arch` label matches
    /// `system`'s arch (or is absent — arch-agnostic class) AND
    /// [`Self::class_ceilings`] hosts `(cores, mem)` AND
    /// [`features_compatible`] holds for `(features, provides)`, else
    /// the first (sorted) `hw_classes` entry that does. `None` ⇔
    /// `system` unmappable OR no configured class hosts that arch at
    /// that size with those features — caller emits empty
    /// `hw_class_names` so the controller's `fallback_cell` reaches its
    /// OWN `None` → `no_hosting_class`.
    pub fn reference_hw_class_for_system(
        &self,
        system: &str,
        cores: u32,
        mem: u64,
        features: &[String],
    ) -> Option<&str> {
        let arch = rio_common::k8s::system_to_k8s_arch(system)?;
        let matches = |h: &str| {
            self.hw_classes.get(h).is_some_and(|d| {
                d.labels
                    .iter()
                    .find(|l| l.key == ARCH_LABEL)
                    .is_none_or(|l| l.value == arch)
                    && features_compatible(features, &d.provides_features)
            }) && {
                let (cc, cm) = self.class_ceilings(h);
                cores <= cc && mem <= cm
            }
        };
        if matches(&self.reference_hw_class) {
            return Some(&self.reference_hw_class);
        }
        let mut hs: Vec<&str> = self.hw_classes.keys().map(String::as_str).collect();
        hs.sort_unstable();
        hs.into_iter().find(|h| matches(h))
    }

    /// STRIKE-6 (r29 bug_019): single post-finalize chokepoint. Every
    /// `hw_class_names` producer in `solve_intent_for` lands here with
    /// finalized `(cores, mem)`. A class whose [`Self::class_ceilings`]
    /// `< (cores, mem)` would route the controller to a cell whose
    /// `cover::sizing` `exceeds_cell_cap`-drops it forever (build never
    /// provisions). The producer paths SHOULD have filtered
    /// (correctness-of-intent: pick the right class, preserve operator's
    /// cap-pin); this is the §"Function becomes total" backstop —
    /// correctness-of-output regardless of correctness-of-producer.
    /// Stripped classes are logged so a producer regression is visible.
    pub fn retain_hosting_classes(
        &self,
        terms: Vec<rio_proto::types::NodeSelectorTerm>,
        names: Vec<String>,
        cores: u32,
        mem: u64,
        required_features: &[String],
    ) -> (Vec<rio_proto::types::NodeSelectorTerm>, Vec<String>) {
        terms
            .into_iter()
            .zip(names)
            .filter(|(_, h)| {
                let (cc, cm) = self.class_ceilings(h);
                // §13c D10: FULL bidirectional features_compatible (NOT
                // half-predicate `provides⊄required` — that misses
                // `required=[kvm], provides=[]` because ∅⊆anything).
                // Unknown class → no feature constraint (the
                // `!contains_key` guard mirrors size's MAX backstop;
                // calling `features_compatible(_, provides_for(unknown)=[])`
                // would wrongly strip kvm intents on unknown classes).
                let feat_ok = !self.hw_classes.contains_key(h)
                    || features_compatible(required_features, self.provides_for(h));
                let ok = cores <= cc && mem <= cm && feat_ok;
                if !ok {
                    tracing::warn!(
                        %h, cores, mem, class_cap = ?(cc, cm),
                        ?required_features, provides = ?self.provides_for(h),
                        "hw_class stripped at post-finalize chokepoint — \
                         producer-path size/feature-filter regressed?"
                    );
                }
                ok
            })
            .unzip()
    }

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
                    taints: vec![],
                    provides_features: vec![],
                    max_fleet_cores: None,
                    capacity_types: default_capacity_types(),
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
            anyhow::ensure!(
                !def.capacity_types.is_empty(),
                "sla.hwClasses[{h}].capacity_types must be non-empty \
                 (default is [spot, on-demand]; explicit empty would make \
                 the class unprovisionable)"
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
            taints: vec![],
            provides_features: vec![],
            max_fleet_cores: None,
            capacity_types: default_capacity_types(),
        }
    }

    /// `test_def` + `provides_features` — for §13c routing tests.
    fn test_def_provides(k: &str, v: &str, provides: &[&str]) -> HwClassDef {
        let mut d = test_def(k, v);
        d.provides_features = provides.iter().map(|s| (*s).into()).collect();
        d
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
    /// bug_039: `reference_hw_class_for_system` arch-matches so the
    /// bypass-path `--capacity` cell doesn't emit `arch In [amd64]` for
    /// an aarch64 build. `(1, 0)` = trivially-hosted on every class so
    /// the size filter is a no-op for this arch-only test.
    #[test]
    fn reference_hw_class_for_system_arch_matches() {
        let mut cfg = base();
        cfg.hw_classes = HashMap::from([
            ("mid-x86".into(), test_def(ARCH_LABEL, "amd64")),
            ("mid-arm".into(), test_def(ARCH_LABEL, "arm64")),
            ("agnostic".into(), test_def("rio.build/hw-band", "mid")),
        ]);
        cfg.reference_hw_class = "mid-x86".into();
        // x86_64 → reference matches.
        assert_eq!(
            cfg.reference_hw_class_for_system("x86_64-linux", 1, 0, &[]),
            Some("mid-x86")
        );
        // aarch64 → reference is amd64, fall through to first arch-match
        // (sorted: agnostic has no arch label → matches anything).
        assert_eq!(
            cfg.reference_hw_class_for_system("aarch64-linux", 1, 0, &[]),
            Some("agnostic")
        );
        // Drop agnostic → mid-arm wins.
        cfg.hw_classes.remove("agnostic");
        assert_eq!(
            cfg.reference_hw_class_for_system("aarch64-linux", 1, 0, &[]),
            Some("mid-arm")
        );
        // Unmappable system → None.
        assert_eq!(
            cfg.reference_hw_class_for_system("riscv64-linux", 1, 0, &[]),
            None
        );
    }

    /// §13c T2: `reference_hw_class_for_system` feature-filters via
    /// [`features_compatible`]. A `--capacity` bypass-path kvm intent
    /// must pick a metal class; a non-kvm intent must NEVER pick metal.
    // r[verify sched.sla.hwclass.provides]
    #[test]
    fn reference_hw_class_for_system_feature_filters() {
        let mut cfg = base();
        cfg.hw_classes = HashMap::from([
            ("std-x86".into(), test_def(ARCH_LABEL, "amd64")),
            (
                "metal-x86".into(),
                test_def_provides(ARCH_LABEL, "amd64", &["kvm"]),
            ),
        ]);
        cfg.reference_hw_class = "std-x86".into();
        let kvm = vec!["kvm".to_string()];
        // kvm intent → metal-x86 (only class with provides=[kvm]).
        assert_eq!(
            cfg.reference_hw_class_for_system("x86_64-linux", 1, 0, &kvm),
            Some("metal-x86")
        );
        // non-kvm intent → std-x86; metal must NOT be picked
        // (∅-guard: required=[], provides=[kvm] → incompatible).
        assert_eq!(
            cfg.reference_hw_class_for_system("x86_64-linux", 1, 0, &[]),
            Some("std-x86")
        );
        // No metal class for arm + kvm intent → None.
        assert_eq!(
            cfg.reference_hw_class_for_system("aarch64-linux", 1, 0, &kvm),
            None
        );
    }

    /// §13c T2/D10: `retain_hosting_classes` applies the FULL
    /// bidirectional [`features_compatible`] predicate. Half-predicate
    /// (`provides⊄required`) misses `required=[kvm], provides=[]`
    /// (∅⊆anything) → std-x86 leaks → pod has kvm nodeSelector, node
    /// lacks label → permanently Pending.
    #[test]
    fn retain_hosting_classes_feature_filters() {
        let mut cfg = base();
        cfg.hw_classes = HashMap::from([
            ("std-x86".into(), test_def("rio.build/hw-class", "std")),
            (
                "metal-x86".into(),
                test_def_provides("rio.build/hw-class", "metal", &["kvm"]),
            ),
        ]);
        let term = |v: &str| rio_proto::types::NodeSelectorTerm {
            match_expressions: vec![rio_proto::types::NodeSelectorRequirement {
                key: "rio.build/hw-class".into(),
                operator: "In".into(),
                values: vec![v.into()],
            }],
        };
        let kvm = vec!["kvm".to_string()];
        // kvm intent: std-x86 (provides=[]) MUST be stripped; metal kept.
        let (_, names) = cfg.retain_hosting_classes(
            vec![term("std"), term("metal")],
            vec!["std-x86".into(), "metal-x86".into()],
            1,
            0,
            &kvm,
        );
        assert_eq!(names, vec!["metal-x86"], "std-x86 stripped for kvm intent");
        // non-kvm intent: metal-x86 (provides=[kvm]) MUST be stripped.
        let (_, names) = cfg.retain_hosting_classes(
            vec![term("std"), term("metal")],
            vec!["std-x86".into(), "metal-x86".into()],
            1,
            0,
            &[],
        );
        assert_eq!(
            names,
            vec!["std-x86"],
            "metal-x86 stripped for non-kvm intent"
        );
    }

    /// bug_019 / STRIKE-6: `reference_hw_class_for_system` size-filters
    /// via [`SlaConfig::class_ceilings`] so a `--cores=48` bypass-path
    /// override picks a class that can HOST 48, not the arch-matched
    /// reference whose `max_cores=32`.
    #[test]
    fn reference_hw_class_for_system_size_filters() {
        let mut cfg = base();
        let mut mid = test_def(ARCH_LABEL, "amd64");
        mid.max_cores = 32;
        let mut hi = test_def(ARCH_LABEL, "amd64");
        hi.max_cores = 128;
        cfg.hw_classes = HashMap::from([("mid".into(), mid), ("hi".into(), hi)]);
        cfg.reference_hw_class = "mid".into();
        // 48 > mid.max_cores=32 → fall through to hi (128 ≥ 48).
        assert_eq!(
            cfg.reference_hw_class_for_system("x86_64-linux", 48, 0, &[]),
            Some("hi"),
            "mid.max_cores=32 cannot host 48; must pick hi"
        );
        // 16 ≤ 32 → reference still wins.
        assert_eq!(
            cfg.reference_hw_class_for_system("x86_64-linux", 16, 0, &[]),
            Some("mid")
        );
        // 256 > every class → None (controller no_hosting_class).
        assert_eq!(
            cfg.reference_hw_class_for_system("x86_64-linux", 256, 0, &[]),
            None
        );
        // mem dimension: hi.max_mem = test_def's 256GiB.
        assert_eq!(
            cfg.reference_hw_class_for_system("x86_64-linux", 1, 512 << 30, &[]),
            None,
            "no class hosts 512GiB mem"
        );
    }

    /// STRIKE-6 structural guarantee: [`SlaConfig::retain_hosting_classes`]
    /// strips ANY `(term, name)` pair whose class can't host
    /// `(cores, mem)`, regardless of which producer leaked it. Terms and
    /// names stay parallel (lockstep zip).
    #[test]
    fn retain_hosting_classes_filters_any_producer_leak() {
        let mut cfg = base();
        let mut mid = test_def("rio.build/hw-class", "mid");
        mid.max_cores = 32;
        let mut hi = test_def("rio.build/hw-class", "hi");
        hi.max_cores = 128;
        cfg.hw_classes = HashMap::from([("mid".into(), mid), ("hi".into(), hi)]);
        let term = |v: &str| rio_proto::types::NodeSelectorTerm {
            match_expressions: vec![rio_proto::types::NodeSelectorRequirement {
                key: "rio.build/hw-class".into(),
                operator: "In".into(),
                values: vec![v.into()],
            }],
        };
        // Hand-construct a producer leak: mid can't host 48, hi can.
        let (terms, names) = cfg.retain_hosting_classes(
            vec![term("mid"), term("hi"), term("mid")],
            vec!["mid".into(), "hi".into(), "mid".into()],
            48,
            0,
            &[],
        );
        assert_eq!(names, vec!["hi"], "mid (max_cores=32) stripped at 48");
        assert_eq!(terms.len(), 1, "terms parallel to names");
        assert_eq!(terms[0].match_expressions[0].values[0], "hi");
        // Unknown class → (MAX, MAX) → never stripped.
        let (_, names) = cfg.retain_hosting_classes(
            vec![term("ghost")],
            vec!["ghost".into()],
            999,
            u64::MAX,
            &[],
        );
        assert_eq!(names, vec!["ghost"], "unknown class = no per-class ceiling");
        // All stripped → both empty (controller fallback_cell path).
        let (terms, names) =
            cfg.retain_hosting_classes(vec![term("mid")], vec!["mid".into()], 48, 0, &[]);
        assert!(terms.is_empty() && names.is_empty());
    }

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

    /// §13c T1b: bidirectional ∅-guard. Single canonical predicate for
    /// D3/D10/I-181 routing — open-coding at ≥4 sites lets them drift.
    // r[verify sched.sla.hwclass.provides.bidir]
    #[test]
    fn features_compatible_bidirectional_guard() {
        let s = |xs: &[&str]| -> Vec<String> { xs.iter().map(|s| (*s).into()).collect() };
        // Both empty → compatible.
        assert!(features_compatible(&[], &[]));
        // Exact match → compatible.
        assert!(features_compatible(&s(&["kvm"]), &s(&["kvm"])));
        // required=[], provides=[kvm] → INcompatible (∅-guard: metal
        // must not absorb non-kvm).
        assert!(!features_compatible(&[], &s(&["kvm"])));
        // required=[kvm], provides=[] → INcompatible (∅-guard: non-metal
        // must not host kvm; []⊆anything would otherwise let it through).
        assert!(!features_compatible(&s(&["kvm"]), &[]));
        // Subset on populated side → compatible (provides=[kvm,bp]
        // hosts required=[kvm]).
        assert!(features_compatible(&s(&["kvm"]), &s(&["kvm", "bp"])));
        // required ⊄ provides → incompatible.
        assert!(!features_compatible(&s(&["kvm", "bp"]), &s(&["kvm"])));
    }

    /// §13c T1: new HwClassDef fields default correctly when absent
    /// from TOML, and `capacity_types` accepts both `od` and
    /// `on-demand` aliases.
    #[test]
    fn hwclassdef_new_fields_defaults_and_serde() {
        // Absent → serde defaults: empty vecs, None, ALL capacity-types.
        let d: HwClassDef = toml::from_str(
            r#"
            labels = [{key="k",value="v"}]
            requirements = [{key="k",operator="In",values=["v"]}]
            node_class = "rio-default"
            max_cores = 1
            max_mem = 1
        "#,
        )
        .unwrap();
        assert!(d.taints.is_empty());
        assert!(d.provides_features.is_empty());
        assert_eq!(d.max_fleet_cores, None);
        assert_eq!(d.capacity_types, vec![CapacityType::Spot, CapacityType::Od]);
        // Explicit od-only via Karpenter alias.
        let d: HwClassDef = toml::from_str(
            r#"
            labels = [{key="k",value="v"}]
            requirements = [{key="k",operator="In",values=["v"]}]
            node_class = "rio-metal"
            max_cores = 1
            max_mem = 1
            capacity_types = ["on-demand"]
            provides_features = ["kvm"]
            max_fleet_cores = 5000
            taints = [{key="rio.build/kvm",value="true",effect="NoSchedule"}]
        "#,
        )
        .unwrap();
        assert_eq!(d.capacity_types, vec![CapacityType::Od]);
        assert_eq!(d.provides_features, vec!["kvm"]);
        assert_eq!(d.max_fleet_cores, Some(5000));
        assert_eq!(d.taints[0].key, "rio.build/kvm");
    }

    /// §13c: `capacity_types_for` / `provides_for` accessors.
    #[test]
    fn capacity_types_for_and_provides_for() {
        let mut cfg = base();
        let mut metal = test_def(ARCH_LABEL, "amd64");
        metal.capacity_types = vec![CapacityType::Od];
        metal.provides_features = vec!["kvm".into()];
        cfg.hw_classes.insert("metal-x86".into(), metal);
        assert_eq!(cfg.capacity_types_for("metal-x86"), &[CapacityType::Od]);
        assert_eq!(
            cfg.capacity_types_for("test-hw"),
            &[CapacityType::Spot, CapacityType::Od]
        );
        // Unknown → ALL (no restriction).
        assert_eq!(cfg.capacity_types_for("nope"), CapacityType::ALL.as_slice());
        assert_eq!(cfg.provides_for("metal-x86"), &["kvm".to_string()]);
        assert!(cfg.provides_for("test-hw").is_empty());
        assert!(cfg.provides_for("nope").is_empty());
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
