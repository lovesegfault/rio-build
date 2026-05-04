//! ADR-023 operator-facing SLA config: tier ladder, cold-start probe
//! shapes, hard ceilings. Loaded from `[sla]` in `scheduler.toml` (helm
//! `scheduler.sla`). Mandatory — every deployment carries an `[sla]`
//! block (helm renders it from chart defaults; tests use
//! [`SlaConfig::test_default`]).

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::solve::{Ceilings, Tier};

// §13c-3 constants: resolve clamps and threat-surface clamps.
//
// `MAX_*_HARD` are the THREAT-SURFACE clamps — they bound seed-corpus
// imports (`build_timeout_ref`, `prior.rs` `e.a` ≤ ln(MAX_MEM_HARD))
// regardless of catalog drift. They are NOT operator budgets.
//
// `MAX_*_GLOBAL` / `MIN_*` are the RESOLVE clamps for the
// `[sla].maxCores`/`maxMem`-unset case under Spot:
// `resolved = max(catalog).clamp(MIN_*, MAX_*_GLOBAL)`.

/// PriorityClass bucket count (Part-B packs cores into `1..1024`
/// PriorityClass values). Also the ref-secs scalar for
/// [`build_timeout_ref`]. Hard structural bound — `validate_shape()`
/// rejects any `maxCores ≥ 1024`.
pub const MAX_CORES_HARD: f64 = 1024.0;
/// Resolve clamp: `MAX_CORES_HARD − 1` because `validate_shape()` is a
/// strict `< 1024`.
pub const MAX_CORES_GLOBAL: f64 = MAX_CORES_HARD - 1.0;
/// 32 TiB — ~1.3× the largest AWS instance (u-24tb1.metal is 24 TiB).
/// Threat-surface clamp for seed-corpus `e.a = ln M(1)`.
pub const MAX_MEM_HARD: u64 = 32 << 40;
/// Resolve clamp; mem has no PriorityClass-style structural bound below
/// the threat-surface ceiling.
pub const MAX_MEM_GLOBAL: u64 = MAX_MEM_HARD;
/// Resolve floor: from the [`SlaConfig::validate_shape`] doc-comment
/// derivation `probe.cpu ≥ 4 ∧ probe.cpu ≤ max_cores/4 ⇒ max_cores ≥ 16`.
pub const MIN_CORES: f64 = 16.0;
/// Resolve floor; conservative — every probe shape's
/// `mem_base + 4·mem_per_core` clears 1 GiB.
pub const MIN_MEM: u64 = 1 << 30;

/// Upper bound for time-domain seed-corpus parameters (S, P, Q) in
/// reference-seconds, for `r[sched.sla.threat.corpus-clamp]`. Not a
/// real timeout — a "this is pathological" gate. `P` is the 1-core
/// parallel time (`T(1)·1` in the Amdahl basis) so it can legitimately
/// be `wall × cores`; the bound is therefore `7d × MAX_CORES_HARD`
/// (~620 Ms — well above any real seed but well below the `1e12` /
/// `f64::MAX` an adversary would inject to NaN/Inf the solver).
///
/// §13c-3: was `7d × cfg.max_cores`; changed to a constant so the
/// persisted seed corpus is decoupled from catalog drift on the cores
/// axis (a restart that derives a smaller global must not reject the
/// previously-loadable corpus). The mem-axis equivalent is
/// `prior.rs`'s `e.a ≤ ln(MAX_MEM_HARD)`.
// r[impl sched.sla.threat.corpus-clamp+3]
pub fn build_timeout_ref() -> f64 {
    7.0 * 86400.0 * MAX_CORES_HARD
}

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
    /// §13c-2: optional **operator-tightening** override on the
    /// per-class capacity ceiling. The *catalog* ceiling (largest real
    /// instance type matching this class's `requirements`, derived at
    /// boot via `describe_instance_types`) is the physical bound; this
    /// can only TIGHTEN below it. `None` (the default) → catalog wins;
    /// catalog absent (static cost source) → falls to `[sla].maxCores`.
    /// `Some(0)` rejected by `validate()`; `None` falls to global.
    pub max_cores: Option<u32>,
    /// Per-class memory ceiling override — see [`Self::max_cores`].
    pub max_mem: Option<u64>,
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
            max_cores: None,
            max_mem: None,
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
    /// `tiers` — checked by [`SlaConfig::validate_shape`].
    pub default_tier: String,
    /// Cold-start probe sizing for never-seen `ModelKey`s.
    pub probe: ProbeShape,
    /// Per-`requiredSystemFeatures` probe overrides (e.g. `kvm` builds
    /// want a high mem floor regardless of core count). Missing key →
    /// fall back to `probe`.
    #[serde(default)]
    pub feature_probes: HashMap<String, ProbeShape>,
    /// §13c-3: optional hard cap on `c*` — solve REJECTS a tier whose
    /// `c*` exceeds this (does not clamp). Also caps the explore
    /// halve/×4 walk via [`Ceilings`].
    ///
    /// `None` (the default) under [`super::cost::HwCostSource::Spot`]
    /// → derived at boot from the catalog max via
    /// [`SlaConfig::resolve_globals`]. `None` under
    /// [`super::cost::HwCostSource::Static`] → boot fail (no catalog
    /// to derive from).
    ///
    /// serde derives default `None` for `Option<>` fields when the
    /// key is absent — no `#[serde(default)]` needed; do NOT add one.
    /// `deny_unknown_fields` only rejects EXTRA keys, not ABSENT ones.
    // r[impl scheduler.sla.global.optional]
    pub max_cores: Option<f64>,
    /// §13c-3: optional global mem ceiling — see [`Self::max_cores`].
    pub max_mem: Option<u64>,
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
    /// — checked by [`SlaConfig::validate_shape`].
    pub hw_classes: HashMap<HwClassName, HwClassDef>,
    /// Admissible-set cost slack: a `(h, cap)` cell within
    /// `(1 + hw_cost_tolerance)` × min-cost stays admissible. Range
    /// `[0, 0.5]` — checked by [`SlaConfig::validate_shape`].
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
    /// §13c-2: AWS bare-metal `instance-size` suffixes, used by
    /// [`super::catalog::derive_ceilings`] to synthesize the
    /// `instance-size {In|NotIn}` partition the controller's
    /// `cover::build_nodeclaim` applies (`nodeClass == rio-metal` →
    /// `In`, else `NotIn`). MUST match `karpenter.metalSizes` /
    /// `controller.toml [nodeclaim_pool] metal_sizes` — helm renders
    /// all three from the one `karpenter.metalSizes` value. Empty →
    /// no partition (vmtest, single-pool clusters).
    #[serde(default)]
    pub metal_sizes: Vec<String>,
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
    /// [`SlaConfig::validate_shape`]) means a future ProbeShape field can't
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

/// §13d (r30 mb_012): canonical bidirectional ∅-guard moved to
/// `rio_common::k8s` so the controller's consumer-side backstop
/// (`fallback_cell`, FFD `simulate` agnostic filter) shares the same
/// predicate as the scheduler's producer chokepoint. Re-exported here
/// so existing scheduler callers stay unchanged. See the docstring on
/// the canonical definition for the routing semantics.
pub use rio_common::k8s::features_compatible;

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
    /// §13c-2: each axis is `min(catalog, cfg)` with each falling to
    /// the global ceiling when absent. The catalog ceiling is the
    /// largest real instance type matching this class's `requirements`
    /// (derived at boot from `describe_instance_types`,
    /// [`super::catalog::derive_ceilings`]); the cfg override can only
    /// *tighten* (`validate_resolved()` enforces `0 < n ≤ global`). Empty
    /// `catalog` (static cost source / fetch failure) → global.
    ///
    /// §13c-3: `global` is the resolved global passed in by every
    /// caller from `&CostTable::resolved_global()` — `SlaConfig` no
    /// longer carries the *effective* global (only the *configured*
    /// `Option<>` override). Separate-params (not `&CostTable`) keeps
    /// `SlaConfig` free of a `CostTable` dependency.
    ///
    /// §Partition-single-source: every scheduler-side per-class ceiling
    /// check (`solve_full`, [`Self::reference_hw_class_for_system`],
    /// the `all_candidates` capacity-fallback, the post-finalize
    /// chokepoint [`Self::retain_hosting_cells`]) calls THIS.
    // r[impl scheduler.sla.ceiling.uncatalogued-fallback]
    // r[impl scheduler.sla.ceiling.config-tightens-only]
    pub fn class_ceilings(
        &self,
        h: &str,
        catalog: &super::catalog::CatalogCeilings,
        global: (u32, u64),
    ) -> (u32, u64) {
        let Some(d) = self.hw_classes.get(h) else {
            return (u32::MAX, u64::MAX);
        };
        let cat = catalog.get(h).copied().unwrap_or(global);
        let cfg = (
            d.max_cores.unwrap_or(global.0),
            d.max_mem.unwrap_or(global.1),
        );
        (cat.0.min(cfg.0), cat.1.min(cfg.1))
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
        catalog: &super::catalog::CatalogCeilings,
        global: (u32, u64),
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
                let (cc, cm) = self.class_ceilings(h, catalog, global);
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

    /// STRIKE-7 (r30 §13d): single post-finalize chokepoint. Every
    /// `hw_class_names` producer in `solve_intent_for` lands here with
    /// finalized `(cores, mem)` AND a `Vec<Cell>` (BEFORE
    /// [`super::solve::cells_to_selector_terms`]) so every placement
    /// constraint axis is a *typed parameter*, not data the chokepoint
    /// silently drops by not looking. r29's STRIKE-6 chokepoint
    /// operated on `(terms, names)` and filtered (size, features) —
    /// bug_042 found arch missing, mb_033 found capacity-type missing,
    /// two missing axes one round after it shipped. The `Vec<Cell>`
    /// shape forces an r31 reviewer adding a 5th axis to change the
    /// signature.
    ///
    /// Predicate is the conjunction of:
    /// - **arch** — `kubernetes.io/arch` from `system_to_k8s_arch
    ///   (system)` ⟺ `pod.rs::nix_systems_to_k8s_arch(systems)` writes
    ///   `nodeSelector{kubernetes.io/arch}`. (bug_042)
    /// - **features** — `features_compatible(required,
    ///   provides_for(h))` ⟺ `wants_kvm(pool)` writes
    ///   `nodeSelector{rio.build/kvm}` (`provides ∋ kvm ⟺ labels ∋
    ///   {rio.build/kvm: true}`, helm-test-pinned). (mb_012)
    /// - **size** — `(cores, mem) ≤ class_ceilings(h)` ⟺ pod
    ///   requests ≤ Node allocatable. (r29 bug_019)
    /// - **capacity-type** — `cap ∈ capacity_types_for(h)` ⟺
    ///   `cells_to_selector_terms` writes `nodeAffinity
    ///   {karpenter.sh/capacity-type In [cap]}`. (mb_033)
    ///
    /// A class whose ceiling/arch/cap can't host the build would route
    /// the controller to a cell that mints a Node the pod's
    /// `nodeSelector`/`nodeAffinity` can never bind to — permanently
    /// Pending. The producer paths SHOULD have filtered
    /// (correctness-of-intent); this is the §"Function becomes total"
    /// backstop — correctness-of-output regardless of
    /// correctness-of-producer. Stripped cells are `warn!`ed; with the
    /// `h_all` arch-filter (snapshot.rs) a strip here is a producer
    /// regression signal, not log spam.
    pub fn retain_hosting_cells(
        &self,
        cells: Vec<Cell>,
        system: &str,
        demand: (u32, u64),
        required_features: &[String],
        catalog: &super::catalog::CatalogCeilings,
        global: (u32, u64),
    ) -> Vec<Cell> {
        let (cores, mem) = demand;
        // bug_042: arch axis. `None` (unmappable / `builtin`) → arch is
        // a no-op (everything kept) — mirrors `cells_to_selector_terms`
        // dropping unknown classes and the controller's
        // `system_to_arch(i.system).is_none()` returning no candidate.
        let want_arch = rio_common::k8s::system_to_k8s_arch(system);
        cells
            .into_iter()
            .filter(|(h, cap)| {
                // Unknown class → no per-class constraint on ANY axis
                // (mirrors `class_ceilings`' `(MAX, MAX)` backstop for
                // size). `cells_to_selector_terms` already drops unknown
                // classes from `(terms, names)` — this branch keeps the
                // chokepoint a pass-through, not an arch/feature/cap
                // gate it can't evaluate. Calling `features_compatible
                // (_, provides_for(unknown)=[])` would wrongly strip
                // kvm intents on unknown classes.
                let Some(d) = self.hw_classes.get(h) else {
                    return true;
                };
                // Arch: class label `kubernetes.io/arch` matches OR is
                // absent (arch-agnostic class hosts any arch).
                let arch_ok = want_arch.is_none_or(|a| {
                    d.labels
                        .iter()
                        .find(|l| l.key == ARCH_LABEL)
                        .is_none_or(|l| l.value == a)
                });
                // §13c D10: FULL bidirectional features_compatible (NOT
                // half-predicate `provides⊄required` — that misses
                // `required=[kvm], provides=[]` because ∅⊆anything).
                let feat_ok = features_compatible(required_features, &d.provides_features);
                // Size: per-class ceiling (catalog ∩ cfg ∩ global).
                let (cc, cm) = self.class_ceilings(h, catalog, global);
                let size_ok = cores <= cc && mem <= cm;
                // Capacity-type: an od-only class structurally never
                // hosts a `(h, Spot)` cell. The producer paths (`solve_
                // full` over `cost.cells`, the `Some(cap)` bypass)
                // SHOULD emit only configured caps — this is the
                // backstop. (mb_033)
                let cap_ok = d.capacity_types.contains(cap);
                let ok = arch_ok && feat_ok && size_ok && cap_ok;
                if !ok {
                    tracing::warn!(
                        %h, ?cap, cores, mem, class_cap = ?(cc, cm),
                        ?required_features, provides = ?d.provides_features,
                        ?want_arch, arch_ok, feat_ok, size_ok, cap_ok,
                        "hw_class cell stripped at post-finalize chokepoint — \
                         producer-path arch/feature/size/cap-filter regressed?"
                    );
                }
                ok
            })
            .collect()
    }

    /// §13c-3: resolve the effective global `(max_cores, max_mem)`
    /// from the configured override and the boot-time catalog.
    ///
    /// - `Some(c)`/`Some(m)` → `(c as u32, m)` (already shape-validated
    ///   by [`Self::validate_shape`]).
    /// - `None` ∧ `Static` → ERROR (no catalog to derive from; also
    ///   gated in [`Self::validate_shape`], so this is a backstop).
    /// - `None` ∧ `Spot` ∧ empty catalog → ERROR with actionable text
    ///   (the operator opted into discovery, discovery unavailable —
    ///   this is a config error, not a fallback).
    /// - `None` ∧ `Spot` ∧ non-empty catalog →
    ///   `(max(catalog.cores).clamp(MIN_CORES, MAX_CORES_GLOBAL) as u32,
    ///    max(catalog.mem).clamp(MIN_MEM, MAX_MEM_GLOBAL))`.
    ///
    /// Returns `(resolved_global, source)` where `source` is the
    /// human-readable origin (`"sla.maxCores/maxMem"` / `"derived from
    /// catalog max"`) for [`Self::validate_resolved`]'s error message.
    // r[impl scheduler.sla.global.derive]
    // r[impl scheduler.sla.global.spot-empty-fails]
    pub fn resolve_globals(
        &self,
        catalog: &super::catalog::CatalogCeilings,
    ) -> anyhow::Result<((u32, u64), &'static str)> {
        if let (Some(c), Some(m)) = (self.max_cores, self.max_mem) {
            return Ok(((c as u32, m), "sla.maxCores/maxMem"));
        }
        // Backstop: `validate_shape()` already rejects partial-Some and
        // Static+None, but keep an actionable error here so a caller
        // that skipped `validate_shape()` doesn't fall through into the
        // catalog-derive arm with a half-set override.
        anyhow::ensure!(
            matches!(self.hw_cost_source, super::cost::HwCostSource::Spot),
            "§13c-3: sla.maxCores/maxMem unset under hwCostSource=static. \
             Static mode has no instance-type catalog to derive a global \
             ceiling from; set sla.maxCores and sla.maxMem explicitly."
        );
        anyhow::ensure!(
            !catalog.is_empty(),
            "§13c-3: hwCostSource=spot but instance-type catalog fetch \
             returned 0 types. Either: (a) check IRSA \
             ec2:DescribeInstanceTypes permissions; (b) set sla.maxCores \
             explicitly; (c) use hwCostSource=static."
        );
        let cat_c = catalog.values().map(|&(c, _)| c).max().unwrap_or(0);
        let cat_m = catalog.values().map(|&(_, m)| m).max().unwrap_or(0);
        let rc = (cat_c as f64).clamp(MIN_CORES, MAX_CORES_GLOBAL) as u32;
        let rm = cat_m.clamp(MIN_MEM, MAX_MEM_GLOBAL);
        Ok(((rc, rm), "derived from catalog max"))
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
            max_cores: Some(16.0),
            max_mem: Some(2 << 30),
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
                    max_cores: Some(16),
                    max_mem: Some(2 << 30),
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
            metal_sizes: Vec::new(),
        }
    }

    /// §13c-3 pass-1 (config-load): every check that does NOT depend
    /// on the resolved global. `&self` (not `&mut`) so it composes
    /// with [`rio_common::config::ValidateConfig::validate`]; sorting
    /// is provided separately by [`Self::solve_tiers`].
    ///
    /// The static-requires-`Some` rule is pass-1 because the catalog
    /// fetch (and hence pass-2) only runs under `Spot` — pass-2 never
    /// executes under `Static`.
    ///
    /// `max_cores < 1024` keeps the PriorityClass-bucket index in
    /// range (Part-B packs cores into `1..1024` PriorityClass values).
    /// `probe.cpu ≥ 4` is the half of `probe.cpu ∈ [4, max_cores/4]`
    /// that doesn't need the resolved global (the `≤ /4` half is
    /// pass-2). The two together force `max_cores ≥ 16`
    /// (= [`MIN_CORES`]) — VM-test pools satisfy this via
    /// [`Self::test_default`].
    // r[impl scheduler.sla.global.static-requires-some]
    pub fn validate_shape(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.tiers.iter().any(|t| t.name == self.default_tier),
            "sla.default_tier {:?} not in sla.tiers (known: {:?})",
            self.default_tier,
            self.tiers.iter().map(|t| &t.name).collect::<Vec<_>>()
        );
        for t in &self.tiers {
            t.validate()?;
        }
        // §13c-3: maxCores/maxMem are jointly Some or jointly None —
        // a partial override is a config-shape error, not a derive
        // input. Static mode requires Some (no catalog to derive from).
        match (self.max_cores, self.max_mem) {
            (Some(c), Some(m)) => {
                anyhow::ensure!(
                    c.is_finite() && c > 0.0,
                    "sla.max_cores must be finite and positive, got {c}"
                );
                anyhow::ensure!(
                    c < MAX_CORES_HARD,
                    "sla.maxCores < {MAX_CORES_HARD} required \
                     (PriorityClass bucket range), got {c}"
                );
                anyhow::ensure!(m > 0, "sla.max_mem must be positive, got {m}");
            }
            (None, None) => {
                anyhow::ensure!(
                    matches!(self.hw_cost_source, super::cost::HwCostSource::Spot),
                    "§13c-3: sla.maxCores/maxMem unset under \
                     hwCostSource=static. Static mode has no instance-type \
                     catalog to derive a global ceiling from; set \
                     sla.maxCores and sla.maxMem explicitly."
                );
            }
            (Some(_), None) | (None, Some(_)) => anyhow::bail!(
                "§13c-3: sla.maxCores and sla.maxMem must both be set or \
                 both unset (got maxCores={:?}, maxMem={:?}); a partial \
                 override would derive only one axis from the catalog.",
                self.max_cores,
                self.max_mem
            ),
        }
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
        // The probe `≤ max_cores/4` half is pass-2 (needs the resolved
        // global). The `≥ 4` and `deadline ≥ 180` halves are
        // shape-checkable here so a bad probe fails fast at config
        // load instead of post-AWS-call.
        anyhow::ensure!(
            self.probe.cpu >= 4.0,
            "sla.probe.cpu must be ≥ 4 so both explore paths reach span≥4; got {}",
            self.probe.cpu
        );
        anyhow::ensure!(
            self.probe.deadline_secs >= 180,
            "sla.probe.deadline_secs must be >= 180, got {}",
            self.probe.deadline_secs
        );
        for (feat, p) in &self.feature_probes {
            anyhow::ensure!(
                p.cpu >= 4.0,
                "sla.feature_probes[{feat}].cpu must be ≥ 4; got {}",
                p.cpu
            );
            anyhow::ensure!(
                p.deadline_secs >= 180,
                "sla.feature_probes[{feat}].deadline_secs must be >= 180, got {}",
                p.deadline_secs
            );
        }
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
            // §13c-2 r[impl scheduler.sla.ceiling.config-tightens-only]:
            // a per-class ceiling is an OPTIONAL tightening override —
            // unset → fall to catalog/global. The `n > 0` half is
            // shape; the `≤ global` half is pass-2.
            if let Some(n) = def.max_cores {
                anyhow::ensure!(
                    n > 0,
                    "sla.hwClasses[{h}].max_cores={n} must be > 0; remove \
                     the per-class override (the boot-time catalog derives \
                     the physical ceiling)"
                );
            }
            if let Some(n) = def.max_mem {
                anyhow::ensure!(
                    n > 0,
                    "sla.hwClasses[{h}].max_mem={n} must be > 0; remove \
                     the per-class override (the boot-time catalog derives \
                     the physical ceiling)"
                );
            }
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
        Ok(())
    }

    /// §13c-3 pass-2 (post-derive): every check that DOES depend on
    /// the resolved global. Runs in `main.rs` after
    /// [`Self::resolve_globals`] (so under `Spot` it surfaces ~30s
    /// later than pass-1, after the catalog fetch). `source` names the
    /// resolved-global origin (`"sla.maxCores/maxMem"` / `"derived
    /// from catalog max"`) for the error message.
    ///
    /// `probe.cpu ≤ resolved/4`: gives the explore walk span≥4 on the
    /// ×4 side. Per-class `Some(n) ≤ resolved`: a per-class override
    /// is tightening-only.
    ///
    /// A per-class `Some(n)` that is `≤ resolved` but `> catalog[h]`
    /// `warn!`s instead of erroring — the override has no effect (the
    /// physical bound wins) and erroring would force the operator to
    /// remove a config line they may want for documentation.
    pub fn validate_resolved(
        &self,
        global: (u32, u64),
        catalog: &super::catalog::CatalogCeilings,
        source: &str,
    ) -> anyhow::Result<()> {
        let (gc, gm) = global;
        let hi = gc as f64;
        self.probe.validate("sla.probe", hi)?;
        for (feat, p) in &self.feature_probes {
            p.validate(&format!("sla.feature_probes[{feat}]"), hi)?;
        }
        for (h, def) in &self.hw_classes {
            if let Some(n) = def.max_cores {
                anyhow::ensure!(
                    n <= gc,
                    "sla.hwClasses[{h}].max_cores={n} > resolved global \
                     {gc}c ({source}); remove the per-class override or \
                     raise the physical ceiling via Karpenter requirements"
                );
                if let Some(&(cc, _)) = catalog.get(h)
                    && n > cc
                {
                    tracing::warn!(
                        %h, override_cores = n, catalog_cores = cc,
                        "sla.hwClasses max_cores override has no effect — \
                         catalog ceiling for this class is lower (config \
                         tightening-only); use Karpenter requirements to \
                         raise the physical ceiling"
                    );
                }
            }
            if let Some(n) = def.max_mem {
                anyhow::ensure!(
                    n <= gm,
                    "sla.hwClasses[{h}].max_mem={n} > resolved global \
                     {gm} bytes ({source}); remove the per-class override \
                     or raise the physical ceiling via Karpenter requirements"
                );
                if let Some(&(_, cm)) = catalog.get(h)
                    && n > cm
                {
                    tracing::warn!(
                        %h, override_mem = n, catalog_mem = cm,
                        "sla.hwClasses max_mem override has no effect — \
                         catalog ceiling for this class is lower (config \
                         tightening-only); use Karpenter requirements"
                    );
                }
            }
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
}

impl Ceilings {
    /// §13c-3: construct the actor-side ceiling carrier from the
    /// boot-resolved global. Replaces the deleted `SlaConfig::ceilings()`
    /// (which read the now-`Option<>` raw config field). The actor
    /// constructs this once at spawn from
    /// `cost_table.read().resolved_global()` so every solve consumer
    /// sees the *effective* (catalog-derived under Spot) global, not
    /// the *configured* `Option<>`.
    pub fn from_resolved(cfg: &SlaConfig, resolved: (u32, u64)) -> Self {
        Self {
            max_cores: resolved.0 as f64,
            max_mem: resolved.1,
            max_disk: cfg.max_disk,
            default_disk: cfg.default_disk,
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
            max_cores: Some(64),
            max_mem: Some(256 << 30),
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
            max_cores: Some(64.0),
            max_mem: Some(256 << 30),
            max_disk: 200 << 30,
            default_disk: 20 << 30,
            ..SlaConfig::test_default()
        }
    }

    /// `(global_cores, global_mem)` for `class_ceilings` etc. fixture
    /// callers, derived from `base()`'s `Some(64)`/`Some(256GiB)`.
    fn base_global() -> (u32, u64) {
        (64, 256 << 30)
    }

    /// `validate_shape() ∘ validate_resolved()` against the configured
    /// `Some(max_cores, max_mem)` (the pre-§13c-3 `validate()` shape,
    /// kept so the existing test corpus exercises both passes).
    fn validate_both(cfg: &SlaConfig) -> anyhow::Result<()> {
        cfg.validate_shape()?;
        let global = (
            cfg.max_cores.expect("test fixture sets Some") as u32,
            cfg.max_mem.expect("test fixture sets Some"),
        );
        cfg.validate_resolved(global, &Default::default(), "sla.maxCores/maxMem")
    }

    #[test]
    fn rejects_probe_cpu_outside_span_range() {
        let mut cfg = base();
        cfg.probe.cpu = 32.0; // > max_cores/4 = 16
        let err = validate_both(&cfg).unwrap_err().to_string();
        assert!(err.contains("sla.probe.cpu"), "{err}");
        cfg.probe.cpu = 2.0; // < 4
        assert!(validate_both(&cfg).is_err());
    }

    #[test]
    fn validate_rejects_nonpositive_tier_bound() {
        let mut cfg = base();
        // Negative: `(d * 1000.0) as u64` would wrap to 0 → broken tier
        // sorts as "tightest" in solve_tiers().
        cfg.tiers[0].p90 = Some(-300.0);
        let err = validate_both(&cfg).unwrap_err().to_string();
        assert!(
            err.contains("tiers[normal].p90") && err.contains("-300"),
            "{err}"
        );
        // NaN: same wrap, plus NaN poisons binding_bound() comparisons.
        cfg.tiers[0].p90 = None;
        cfg.tiers[0].p50 = Some(f64::NAN);
        let err = validate_both(&cfg).unwrap_err().to_string();
        assert!(err.contains("tiers[normal].p50"), "{err}");
        // Zero: degenerate (no build can hit a 0s target).
        cfg.tiers[0].p50 = None;
        cfg.tiers[0].p99 = Some(0.0);
        assert!(validate_both(&cfg).is_err());
        // Positive control.
        cfg.tiers[0].p99 = Some(300.0);
        validate_both(&cfg).unwrap();
    }

    #[test]
    fn rejects_unknown_default_tier() {
        let mut cfg = base();
        cfg.default_tier = "fast".into();
        let err = validate_both(&cfg).unwrap_err().to_string();
        assert!(err.contains("not in sla.tiers"), "{err}");
    }

    #[test]
    fn accepts_probe_cpu_at_bounds() {
        let mut cfg = base();
        cfg.probe.cpu = 4.0;
        validate_both(&cfg).unwrap();
        cfg.probe.cpu = 16.0; // = max_cores/4
        validate_both(&cfg).unwrap();
    }

    #[test]
    fn rejects_probe_deadline_under_180() {
        let mut cfg = base();
        cfg.probe.deadline_secs = 60;
        let err = validate_both(&cfg).unwrap_err().to_string();
        assert!(
            err.contains("sla.probe.deadline_secs must be >= 180"),
            "{err}"
        );
        cfg.probe.deadline_secs = 180;
        validate_both(&cfg).unwrap();

        cfg.feature_probes.insert(
            "kvm".into(),
            ProbeShape {
                cpu: 4.0,
                mem_per_core: 0,
                mem_base: 0,
                deadline_secs: 120,
            },
        );
        let err = validate_both(&cfg).unwrap_err().to_string();
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
        let err = validate_both(&cfg).unwrap_err().to_string();
        assert!(
            err.contains("feature_probes[kvm].cpu") && err.contains("max_cores=64"),
            "{err}"
        );
        cfg.feature_probes.get_mut("kvm").unwrap().cpu = 2.0;
        assert!(validate_both(&cfg).is_err(), "<4 also rejected");
        cfg.feature_probes.get_mut("kvm").unwrap().cpu = 16.0;
        validate_both(&cfg).unwrap();
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
        let err = validate_both(&cfg).unwrap_err().to_string();
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
        let cfg = base();
        let c = Ceilings::from_resolved(&cfg, base_global());
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
        validate_both(&sla).unwrap();
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
        let err = validate_both(&cfg).unwrap_err().to_string();
        assert!(
            err.contains("c7a.xlarge") && err.contains("[a-z0-9-]"),
            "{err}"
        );
        // Positive control: dash-separated key passes.
        cfg.hw_classes.remove("c7a.xlarge");
        cfg.hw_classes
            .insert("c7a-xlarge".into(), test_def("k", "v"));
        validate_both(&cfg).unwrap();
    }

    // r[verify sched.sla.hw-class.config]
    #[test]
    fn rejects_empty_hw_classes() {
        let mut cfg = base();
        // Populated (from test_default) → valid.
        validate_both(&cfg).unwrap();
        // Empty → ADR-023 §13a is mandatory; validate() must reject.
        cfg.hw_classes.clear();
        let err = validate_both(&cfg).unwrap_err().to_string();
        assert!(err.contains("hwClasses is mandatory"), "{err}");
    }

    #[test]
    fn rejects_max_cores_ge_1024() {
        let mut cfg = base();
        cfg.max_cores = Some(1024.0);
        let err = validate_both(&cfg).unwrap_err().to_string();
        assert!(err.contains("maxCores < 1024"), "{err}");
    }

    /// §13c-3 RED-FIRST: `validate_shape()` accepts `None`/`None` under
    /// Spot (catalog will derive at boot), rejects under Static (no
    /// catalog), rejects partial-Some.
    // r[verify scheduler.sla.global.optional]
    // r[verify scheduler.sla.global.static-requires-some]
    #[test]
    fn validate_shape_optional_global() {
        // None/None under Spot — accepted.
        let mut cfg = base();
        cfg.hw_cost_source = super::super::cost::HwCostSource::Spot;
        cfg.max_cores = None;
        cfg.max_mem = None;
        cfg.validate_shape().expect("Spot + None/None is valid");

        // None/None under Static — boot fail.
        cfg.hw_cost_source = super::super::cost::HwCostSource::Static;
        let err = cfg.validate_shape().unwrap_err().to_string();
        assert!(err.contains("hwCostSource=static"), "{err}");
        assert!(err.contains("set sla.maxCores"), "fix named: {err}");

        // Partial Some — boot fail under either source.
        cfg.hw_cost_source = super::super::cost::HwCostSource::Spot;
        cfg.max_cores = Some(64.0);
        cfg.max_mem = None;
        let err = cfg.validate_shape().unwrap_err().to_string();
        assert!(err.contains("both be set or both unset"), "{err}");
        cfg.max_cores = None;
        cfg.max_mem = Some(256 << 30);
        assert!(
            cfg.validate_shape().is_err(),
            "partial Some(maxMem) rejected"
        );
    }

    /// §13c-3 RED-FIRST: `resolve_globals()` derives the effective
    /// global from the catalog when unset, clamps to `[MIN_*, MAX_*_GLOBAL]`,
    /// boot-fails on Spot+empty-catalog+None, passes through `Some`.
    // r[verify scheduler.sla.global.derive]
    // r[verify scheduler.sla.global.spot-empty-fails]
    #[test]
    fn resolve_globals_derives_from_catalog() {
        use super::super::catalog::CatalogCeilings;
        let mut cfg = base();
        cfg.hw_cost_source = super::super::cost::HwCostSource::Spot;
        cfg.max_cores = None;
        cfg.max_mem = None;

        // Empty catalog + Spot + None → boot fail with actionable text.
        let err = cfg
            .resolve_globals(&CatalogCeilings::new())
            .unwrap_err()
            .to_string();
        assert!(err.contains("0 types"), "{err}");
        assert!(err.contains("IRSA"), "{err}");
        assert!(err.contains("hwCostSource=static"), "{err}");

        // Non-empty catalog → max(catalog), clamped to MIN/MAX_GLOBAL.
        let cat: CatalogCeilings = std::collections::HashMap::from([
            ("h1".into(), (96u32, 768u64 << 30)),
            ("h2".into(), (192u32, 1536u64 << 30)),
        ]);
        let ((c, m), src) = cfg.resolve_globals(&cat).unwrap();
        assert_eq!((c, m), (192, 1536 << 30));
        assert_eq!(src, "derived from catalog max");

        // Catalog above MAX_CORES_GLOBAL → clamped.
        let huge: CatalogCeilings =
            std::collections::HashMap::from([("h1".into(), (2000u32, MAX_MEM_HARD * 2))]);
        let ((c, m), _) = cfg.resolve_globals(&huge).unwrap();
        assert_eq!(c, MAX_CORES_GLOBAL as u32);
        assert_eq!(m, MAX_MEM_GLOBAL);

        // Catalog below MIN_CORES → floored.
        let tiny: CatalogCeilings =
            std::collections::HashMap::from([("h1".into(), (4u32, 1u64 << 28))]);
        let ((c, m), _) = cfg.resolve_globals(&tiny).unwrap();
        assert_eq!(c, MIN_CORES as u32);
        assert_eq!(m, MIN_MEM);

        // Some → that value, source "sla.maxCores/maxMem".
        cfg.max_cores = Some(128.0);
        cfg.max_mem = Some(512 << 30);
        let ((c, m), src) = cfg.resolve_globals(&cat).unwrap();
        assert_eq!((c, m), (128, 512 << 30));
        assert_eq!(src, "sla.maxCores/maxMem");

        // Static + None → boot fail (backstop; validate_shape also rejects).
        cfg.hw_cost_source = super::super::cost::HwCostSource::Static;
        cfg.max_cores = None;
        cfg.max_mem = None;
        let err = cfg.resolve_globals(&cat).unwrap_err().to_string();
        assert!(err.contains("hwCostSource=static"), "{err}");
    }

    /// §13c-3: `validate_resolved()` rejects per-class `Some(n) > global`
    /// with a source-attributed message; passes per-class `Some(n) ≤ global`.
    #[test]
    fn validate_resolved_per_class_within_global() {
        let mut cfg = base();
        cfg.hw_classes.get_mut("test-hw").unwrap().max_cores = Some(300);
        let err = cfg
            .validate_resolved(
                (200, 256 << 30),
                &Default::default(),
                "derived from catalog max",
            )
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("max_cores=300 > resolved global 200c"),
            "{err}"
        );
        assert!(
            err.contains("derived from catalog max"),
            "source attributed: {err}"
        );
        assert!(err.contains("Karpenter requirements"), "fix named: {err}");

        cfg.hw_classes.get_mut("test-hw").unwrap().max_cores = Some(100);
        cfg.validate_resolved((200, 256 << 30), &Default::default(), "x")
            .expect("per-class Some(100) ≤ global 200 passes");
    }

    #[test]
    fn rejects_hw_cost_tolerance_out_of_range() {
        for bad in [-0.01, 0.6, f64::NAN] {
            let mut cfg = base();
            cfg.hw_cost_tolerance = bad;
            assert!(validate_both(&cfg).is_err(), "{bad} should be rejected");
        }
        let mut cfg = base();
        cfg.hw_explore_epsilon = 0.3;
        assert!(validate_both(&cfg).is_err());
    }

    #[test]
    fn rejects_reference_not_in_hw_classes() {
        let mut cfg = base();
        cfg.reference_hw_class = "nope".into();
        let err = validate_both(&cfg).unwrap_err().to_string();
        assert!(err.contains("not in sla.hwClasses"), "{err}");
    }

    #[test]
    fn rejects_empty_hw_class_labels() {
        let mut cfg = base();
        cfg.hw_classes
            .insert("test-hw".into(), HwClassDef::default());
        let err = validate_both(&cfg).unwrap_err().to_string();
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
        let err = validate_both(&cfg).unwrap_err().to_string();
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
        let err = validate_both(&cfg).unwrap_err().to_string();
        assert!(err.contains("rio.build/hw-band"), "{err}");
        assert!(err.contains("Node-stamp"), "{err}");

        let mut cfg = base();
        cfg.hw_classes
            .get_mut("test-hw")
            .unwrap()
            .node_class
            .clear();
        let err = validate_both(&cfg).unwrap_err().to_string();
        assert!(
            err.contains("node_class must name an EC2NodeClass"),
            "{err}"
        );

        // §13c-2 r[verify scheduler.sla.ceiling.config-tightens-only]:
        // per-class ceilings are an OPTIONAL tightening override.
        // Some(0) and Some(>global) rejected; None always OK; Some(=global)
        // OK. The error message names the fix (`xtask` doesn't apply here
        // post-redirect — the catalog is boot-derived, so the fix is
        // "remove the override or raise global").
        for (mc, mm, expect) in [
            (Some(0u32), Some(1u64), Some("max_cores=0 must be > 0")),
            (Some(64), Some(0), Some("max_mem=0 must be > 0")),
            (
                Some(65),
                Some(1),
                Some("max_cores=65 > resolved global 64c"),
            ),
            (
                Some(64),
                Some((256u64 << 30) + 1),
                Some("> resolved global"),
            ),
            (Some(64), Some(256 << 30), None),
            (None, None, None),
            (None, Some(256 << 30), None),
            (Some(64), None, None),
        ] {
            let mut cfg = base();
            let def = cfg.hw_classes.get_mut("test-hw").unwrap();
            def.max_cores = mc;
            def.max_mem = mm;
            match (expect, validate_both(&cfg)) {
                (Some(want), Err(e)) => {
                    assert!(e.to_string().contains(want), "({mc:?},{mm:?}): {e}")
                }
                (None, Ok(())) => {}
                (e, r) => panic!("({mc:?},{mm:?}): expect {e:?}, got {r:?}"),
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
            "metal_sizes",
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
    /// cross-field-checked by [`SlaConfig::validate_shape`]. The exhaustive
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
            cfg.reference_hw_class_for_system(
                "x86_64-linux",
                1,
                0,
                &[],
                &Default::default(),
                base_global()
            ),
            Some("mid-x86")
        );
        // aarch64 → reference is amd64, fall through to first arch-match
        // (sorted: agnostic has no arch label → matches anything).
        assert_eq!(
            cfg.reference_hw_class_for_system(
                "aarch64-linux",
                1,
                0,
                &[],
                &Default::default(),
                base_global()
            ),
            Some("agnostic")
        );
        // Drop agnostic → mid-arm wins.
        cfg.hw_classes.remove("agnostic");
        assert_eq!(
            cfg.reference_hw_class_for_system(
                "aarch64-linux",
                1,
                0,
                &[],
                &Default::default(),
                base_global()
            ),
            Some("mid-arm")
        );
        // Unmappable system → None.
        assert_eq!(
            cfg.reference_hw_class_for_system(
                "riscv64-linux",
                1,
                0,
                &[],
                &Default::default(),
                base_global()
            ),
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
            cfg.reference_hw_class_for_system(
                "x86_64-linux",
                1,
                0,
                &kvm,
                &Default::default(),
                base_global()
            ),
            Some("metal-x86")
        );
        // non-kvm intent → std-x86; metal must NOT be picked
        // (∅-guard: required=[], provides=[kvm] → incompatible).
        assert_eq!(
            cfg.reference_hw_class_for_system(
                "x86_64-linux",
                1,
                0,
                &[],
                &Default::default(),
                base_global()
            ),
            Some("std-x86")
        );
        // No metal class for arm + kvm intent → None.
        assert_eq!(
            cfg.reference_hw_class_for_system(
                "aarch64-linux",
                1,
                0,
                &kvm,
                &Default::default(),
                base_global()
            ),
            None
        );
    }

    /// `Vec<&str>` of the surviving hw-class names — for terse
    /// `retain_hosting_cells` assertions.
    fn names(cells: &[Cell]) -> Vec<&str> {
        cells.iter().map(|(h, _)| h.as_str()).collect()
    }

    /// §13c T2/D10: `retain_hosting_cells` applies the FULL
    /// bidirectional [`features_compatible`] predicate. Half-predicate
    /// (`provides⊄required`) misses `required=[kvm], provides=[]`
    /// (∅⊆anything) → std-x86 leaks → pod has kvm nodeSelector, node
    /// lacks label → permanently Pending.
    #[test]
    fn retain_hosting_cells_filters_features() {
        let mut cfg = base();
        cfg.hw_classes = HashMap::from([
            ("std-x86".into(), test_def("rio.build/hw-class", "std")),
            (
                "metal-x86".into(),
                test_def_provides("rio.build/hw-class", "metal", &["kvm"]),
            ),
        ]);
        let cat = super::super::catalog::CatalogCeilings::new();
        let kvm = vec!["kvm".to_string()];
        let cells = || -> Vec<Cell> {
            vec![
                ("std-x86".into(), CapacityType::Od),
                ("metal-x86".into(), CapacityType::Od),
            ]
        };
        // kvm intent: std-x86 (provides=[]) MUST be stripped; metal kept.
        let kept =
            cfg.retain_hosting_cells(cells(), "x86_64-linux", (1, 0), &kvm, &cat, base_global());
        assert_eq!(
            names(&kept),
            vec!["metal-x86"],
            "std-x86 stripped for kvm intent"
        );
        // non-kvm intent: metal-x86 (provides=[kvm]) MUST be stripped.
        let kept =
            cfg.retain_hosting_cells(cells(), "x86_64-linux", (1, 0), &[], &cat, base_global());
        assert_eq!(
            names(&kept),
            vec!["std-x86"],
            "metal-x86 stripped for non-kvm intent"
        );
    }

    /// §13d STRIKE-7 (r30 bug_042): `retain_hosting_cells` arch-filters.
    /// `h_all` is feature-partitioned only — a kvm `x86_64-linux` intent
    /// gets `h_all=[metal-arm, metal-x86]` (both `provides=[kvm]`), and
    /// neither `solve_full` nor the r29 STRIKE-6 chokepoint arch-filter,
    /// so `hw_class_names` ships both archs to the controller. The pod's
    /// `nodeSelector{kubernetes.io/arch}` makes placement correct, but
    /// the controller's `cells_of(intent)` still mints a wrong-arch
    /// NodeClaim that sits idle while the right-arch one is never minted.
    // r[verify sched.sla.hwclass.provides]
    #[test]
    fn retain_hosting_cells_filters_arch() {
        let mut cfg = base();
        cfg.hw_classes = HashMap::from([
            (
                "metal-x86".into(),
                test_def_provides(ARCH_LABEL, "amd64", &["kvm"]),
            ),
            (
                "metal-arm".into(),
                test_def_provides(ARCH_LABEL, "arm64", &["kvm"]),
            ),
            // arch-agnostic class (no kubernetes.io/arch label) — must
            // NOT be stripped regardless of `system`.
            (
                "metal-any".into(),
                test_def_provides("rio.build/hw-band", "metal", &["kvm"]),
            ),
        ]);
        let cat = super::super::catalog::CatalogCeilings::new();
        let kvm = vec!["kvm".to_string()];
        let cells: Vec<Cell> = vec![
            ("metal-x86".into(), CapacityType::Od),
            ("metal-arm".into(), CapacityType::Od),
            ("metal-any".into(), CapacityType::Od),
        ];
        // x86 intent → metal-arm stripped, metal-x86 + metal-any kept.
        let kept = cfg.retain_hosting_cells(
            cells.clone(),
            "x86_64-linux",
            (1, 0),
            &kvm,
            &cat,
            base_global(),
        );
        let mut kept = names(&kept);
        kept.sort_unstable();
        assert_eq!(
            kept,
            vec!["metal-any", "metal-x86"],
            "wrong-arch metal-arm stripped for x86_64-linux"
        );
        // arm intent → metal-x86 stripped.
        let kept = cfg.retain_hosting_cells(
            cells.clone(),
            "aarch64-linux",
            (1, 0),
            &kvm,
            &cat,
            base_global(),
        );
        let mut kept = names(&kept);
        kept.sort_unstable();
        assert_eq!(
            kept,
            vec!["metal-any", "metal-arm"],
            "wrong-arch metal-x86 stripped for aarch64-linux"
        );
        // unmappable system → arch axis is a no-op (everything kept).
        let kept = cfg.retain_hosting_cells(cells, "builtin", (1, 0), &kvm, &cat, base_global());
        assert_eq!(kept.len(), 3, "unmappable system → arch-agnostic");
    }

    /// §13d STRIKE-7 (r30 mb_033): `retain_hosting_cells` filters
    /// `cap ∈ capacity_types_for(h)`. The bypass-path `Some(cap)` arm
    /// (`reference_hw_class_for_system` doesn't take `cap`) and the
    /// `all_candidates` capacity-fallback both can ship a `(h, cap)`
    /// the class doesn't host. The controller's `cover_deficit`
    /// iterates `all_cells()` (which yields only configured caps) so
    /// `by_cell[(metal, Spot)]` is never visited — build hangs forever
    /// with no NodeClaim, no metric, no warn.
    #[test]
    fn retain_hosting_cells_filters_capacity() {
        let mut cfg = base();
        let mut metal = test_def_provides(ARCH_LABEL, "amd64", &["kvm"]);
        metal.capacity_types = vec![CapacityType::Od];
        cfg.hw_classes = HashMap::from([("metal-x86".into(), metal)]);
        let cat = super::super::catalog::CatalogCeilings::new();
        let kvm = vec!["kvm".to_string()];
        let cells: Vec<Cell> = vec![
            ("metal-x86".into(), CapacityType::Spot),
            ("metal-x86".into(), CapacityType::Od),
        ];
        let kept =
            cfg.retain_hosting_cells(cells, "x86_64-linux", (1, 0), &kvm, &cat, base_global());
        assert_eq!(
            kept,
            vec![("metal-x86".to_string(), CapacityType::Od)],
            "phantom (metal-x86, Spot) stripped — class is od-only"
        );
        // Unknown class → no cap constraint (mirrors size's MAX backstop).
        let kept = cfg.retain_hosting_cells(
            vec![("ghost".into(), CapacityType::Spot)],
            "x86_64-linux",
            (1, 0),
            &[],
            &cat,
            base_global(),
        );
        assert_eq!(kept.len(), 1, "unknown class → no cap constraint");
    }

    /// §13d STRIKE-7 contract test: enumerate the placement-constraint
    /// axes the chokepoint must filter on. Per-axis, perturb ONE cell
    /// and assert it's stripped while the baseline cell survives.
    /// Self-documenting list of axes — an r31 reviewer adding a 5th
    /// axis MUST add a row here, or the chokepoint passes a constraint
    /// it can't see.
    #[test]
    fn retain_hosting_cells_axis_enumeration() {
        let mut cfg = base();
        cfg.max_cores = Some(256.0);
        // baseline: amd64 + kvm + 64-core + od-only.
        let mut ok_def = test_def_provides(ARCH_LABEL, "amd64", &["kvm"]);
        ok_def.max_cores = Some(64);
        ok_def.capacity_types = vec![CapacityType::Od];
        // size-perturbed: same arch/features/cap but max_cores=4.
        let mut lo = ok_def.clone();
        lo.max_cores = Some(4);
        // arch-perturbed: arm64 instead of amd64.
        let arm = test_def_provides(ARCH_LABEL, "arm64", &["kvm"]);
        // features-perturbed: no provides_features.
        let std = test_def(ARCH_LABEL, "amd64");
        cfg.hw_classes = HashMap::from([
            ("metal-x86".into(), ok_def),
            ("lo-x86".into(), lo),
            ("metal-arm".into(), arm),
            ("std-x86".into(), std),
        ]);
        let cat = super::super::catalog::CatalogCeilings::new();
        let global = (256u32, 256u64 << 30);
        let ok: Cell = ("metal-x86".into(), CapacityType::Od);
        // The axis ⨯ perturbed-cell list. Adding a 5th axis without a
        // row here means the contract test can't verify the chokepoint
        // sees it — RED-first the new axis.
        let axes: &[(&str, Cell)] = &[
            ("arch", ("metal-arm".into(), CapacityType::Od)),
            ("features", ("std-x86".into(), CapacityType::Od)),
            ("size", ("lo-x86".into(), CapacityType::Od)),
            ("cap", ("metal-x86".into(), CapacityType::Spot)),
        ];
        for (axis, bad) in axes {
            let kept = cfg.retain_hosting_cells(
                vec![ok.clone(), bad.clone()],
                "x86_64-linux",
                (8, 0),
                &["kvm".to_string()],
                &cat,
                global,
            );
            assert_eq!(
                kept,
                vec![ok.clone()],
                "axis={axis}: bad cell {bad:?} not stripped"
            );
        }
    }

    /// bug_019 / STRIKE-6: `reference_hw_class_for_system` size-filters
    /// via [`SlaConfig::class_ceilings`] so a `--cores=48` bypass-path
    /// override picks a class that can HOST 48, not the arch-matched
    /// reference whose `max_cores=32`.
    #[test]
    fn reference_hw_class_for_system_size_filters() {
        let cat = super::super::catalog::CatalogCeilings::new();
        let mut cfg = base();
        // global cap 256/256GiB so the per-class Some(128) isn't masked.
        cfg.max_cores = Some(256.0);
        let mut mid = test_def(ARCH_LABEL, "amd64");
        mid.max_cores = Some(32);
        let mut hi = test_def(ARCH_LABEL, "amd64");
        hi.max_cores = Some(128);
        cfg.hw_classes = HashMap::from([("mid".into(), mid), ("hi".into(), hi)]);
        cfg.reference_hw_class = "mid".into();
        // 48 > mid.max_cores=32 → fall through to hi (128 ≥ 48).
        assert_eq!(
            cfg.reference_hw_class_for_system(
                "x86_64-linux",
                48,
                0,
                &[],
                &cat,
                (256u32, 256u64 << 30)
            ),
            Some("hi"),
            "mid.max_cores=32 cannot host 48; must pick hi"
        );
        // 16 ≤ 32 → reference still wins.
        assert_eq!(
            cfg.reference_hw_class_for_system(
                "x86_64-linux",
                16,
                0,
                &[],
                &cat,
                (256u32, 256u64 << 30)
            ),
            Some("mid")
        );
        // 200 > every class → None (controller no_hosting_class).
        assert_eq!(
            cfg.reference_hw_class_for_system(
                "x86_64-linux",
                200,
                0,
                &[],
                &cat,
                (256u32, 256u64 << 30)
            ),
            None
        );
        // mem dimension: hi.max_mem = test_def's 256GiB.
        assert_eq!(
            cfg.reference_hw_class_for_system(
                "x86_64-linux",
                1,
                512 << 30,
                &[],
                &cat,
                (256u32, 256u64 << 30)
            ),
            None,
            "no class hosts 512GiB mem"
        );
    }

    /// STRIKE-6 structural guarantee: [`SlaConfig::retain_hosting_cells`]
    /// strips ANY cell whose class can't host `(cores, mem)`,
    /// regardless of which producer leaked it.
    #[test]
    fn retain_hosting_cells_filters_any_producer_leak() {
        let cat = super::super::catalog::CatalogCeilings::new();
        let mut cfg = base();
        cfg.max_cores = Some(256.0);
        let mut mid = test_def("rio.build/hw-class", "mid");
        mid.max_cores = Some(32);
        let mut hi = test_def("rio.build/hw-class", "hi");
        hi.max_cores = Some(128);
        cfg.hw_classes = HashMap::from([("mid".into(), mid), ("hi".into(), hi)]);
        // Hand-construct a producer leak: mid can't host 48, hi can.
        let kept = cfg.retain_hosting_cells(
            vec![
                ("mid".into(), CapacityType::Od),
                ("hi".into(), CapacityType::Od),
                ("mid".into(), CapacityType::Spot),
            ],
            "x86_64-linux",
            (48, 0),
            &[],
            &cat,
            (256u32, 256u64 << 30),
        );
        assert_eq!(
            names(&kept),
            vec!["hi"],
            "mid (max_cores=32) stripped at 48"
        );
        // Unknown class → (MAX, MAX) → never stripped.
        let kept = cfg.retain_hosting_cells(
            vec![("ghost".into(), CapacityType::Od)],
            "x86_64-linux",
            (999, u64::MAX),
            &[],
            &cat,
            (256u32, 256u64 << 30),
        );
        assert_eq!(
            names(&kept),
            vec!["ghost"],
            "unknown class = no per-class ceiling"
        );
        // All stripped → empty (controller fallback_cell path).
        let kept = cfg.retain_hosting_cells(
            vec![("mid".into(), CapacityType::Od)],
            "x86_64-linux",
            (48, 0),
            &[],
            &cat,
            (256u32, 256u64 << 30),
        );
        assert!(kept.is_empty());
    }

    /// §13c-2 r[verify scheduler.sla.ceiling.uncatalogued-fallback]:
    /// `class_ceilings` is `min(catalog_or_global, cfg_or_global)` per
    /// axis. Catalog absent + cfg `None` → global. Catalog present →
    /// physical bound. Cfg can only TIGHTEN below catalog.
    /// r[verify scheduler.sla.ceiling.config-tightens-only]
    #[test]
    fn class_ceilings_min_of_catalog_and_cfg() {
        let mut cfg = base();
        cfg.max_cores = Some(192.0);
        cfg.max_mem = Some(1024 << 30);
        // h1: no cfg override (None/None).
        let mut h1 = test_def("k", "v");
        h1.max_cores = None;
        h1.max_mem = None;
        // h2: cfg tightens cores to 32.
        let mut h2 = test_def("k", "v");
        h2.max_cores = Some(32);
        h2.max_mem = None;
        cfg.hw_classes = HashMap::from([("h1".into(), h1), ("h2".into(), h2)]);

        // No catalog: fall to global on both axes (h1) or cfg (h2 cores).
        let empty = super::super::catalog::CatalogCeilings::new();
        assert_eq!(
            cfg.class_ceilings("h1", &empty, (192u32, 1024u64 << 30)),
            (192, 1024 << 30)
        );
        assert_eq!(
            cfg.class_ceilings("h2", &empty, (192u32, 1024u64 << 30)),
            (32, 1024 << 30)
        );
        // Unknown class → (MAX, MAX) regardless of catalog.
        assert_eq!(
            cfg.class_ceilings("ghost", &empty, (192u32, 1024u64 << 30)),
            (u32::MAX, u64::MAX)
        );

        // Catalog says h1 tops at (96, 768GiB).
        let cat: super::super::catalog::CatalogCeilings =
            HashMap::from([("h1".into(), (96u32, 768u64 << 30))]);
        assert_eq!(
            cfg.class_ceilings("h1", &cat, (192u32, 1024u64 << 30)),
            (96, 768 << 30),
            "catalog physical bound applied"
        );
        // h2 not in catalog → catalog falls to global; cfg=Some(32) tightens.
        assert_eq!(
            cfg.class_ceilings("h2", &cat, (192u32, 1024u64 << 30)),
            (32, 1024 << 30)
        );
        // cfg can tighten below catalog.
        cfg.hw_classes.get_mut("h1").unwrap().max_cores = Some(48);
        assert_eq!(
            cfg.class_ceilings("h1", &cat, (192u32, 1024u64 << 30)),
            (48, 768 << 30),
            "cfg=Some(48) tightens below catalog=96"
        );

        // r[verify scheduler.sla.ceiling.config-tightens-only]
        // Threat surface: a malicious/buggy AWS API response with a
        // catalog ceiling ABOVE global must NOT raise the effective
        // ceiling. With cfg=None the formula's `cfg.unwrap_or(global)`
        // arm is the operator-asserted bound; `min` clamps element-wise.
        cfg.hw_classes.get_mut("h1").unwrap().max_cores = None;
        let huge: super::super::catalog::CatalogCeilings =
            HashMap::from([("h1".into(), (1000u32, 8u64 << 40))]);
        assert_eq!(
            cfg.class_ceilings("h1", &huge, (192u32, 1024u64 << 30)),
            (192, 1024 << 30),
            "catalog above global is clamped to global by cfg.unwrap_or(global)"
        );
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
            metal_sizes: _,        // (free)   instance-size suffix strings
        } = cfg;
        // Silence unused-binding on the one (cell) field we kept by
        // name; the destructure itself is the load-bearing part.
        let _ = lead_time_seed;

        // ---- (cell) lead_time_seed: key.0 ∈ hw_classes ----
        let mut bad_key = base();
        bad_key
            .lead_time_seed
            .insert(("nonexistent".into(), CapacityType::Od), 30.0);
        let err = validate_both(&bad_key)
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
            validate_both(&bad_val).is_err(),
            "non-finite lead_time_seed value must be rejected"
        );
        // > max_lead_time (default 600.0).
        let mut bad_val = with_h();
        bad_val
            .lead_time_seed
            .insert(("intel-7".into(), CapacityType::Spot), 6000.0);
        assert!(
            validate_both(&bad_val).is_err(),
            "lead_time_seed value > max_lead_time must be rejected"
        );
        // Positive control: valid key + valid value passes.
        let mut ok = with_h();
        ok.lead_time_seed
            .insert(("intel-7".into(), CapacityType::Spot), 30.0);
        validate_both(&ok).expect("valid lead_time_seed should pass");
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
