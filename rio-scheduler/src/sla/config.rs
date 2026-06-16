//! ADR-023 operator-facing SLA config: tier ladder, cold-start probe
//! shapes, hard ceilings. Loaded from `[sla]` in `scheduler.toml` (helm
//! `scheduler.sla`). Mandatory — every deployment carries an `[sla]`
//! block (helm renders it from chart defaults; tests use
//! [`SlaConfig::test_default`]).

use std::collections::{HashMap, HashSet};
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
    /// `r[sched.sla.hwclass.capacity-types+2]`: `solve_full` and the
    /// controller's `all_cells`/`fallback_cell` iterate THIS, not
    /// `CapacityType::ALL`, so an od-only class never generates a
    /// `(h, Spot)` cell — structurally preventing the
    /// conflicting-requirements ICE loop a requirement-based exclusion
    /// would cause. Default `[Spot, Od]` (both; since M1 no shipped
    /// class narrows it — metal runs spot+od with od failover).
    #[serde(default = "default_capacity_types")]
    pub capacity_types: Vec<CapacityType>,
    /// live_050(c): typed capacity-degradation ladder. Names the
    /// sibling classes (generation rungs, e.g. `hi-ebs-x86-g7` under
    /// `hi-ebs-x86`) that join this class's HOSTING CLOSURE — every
    /// intent solved into this class also carries the rung classes'
    /// cells in `hw_class_names`, so ICE evidence on this class's
    /// cells leaves the walk somewhere to advance TO. Membership
    /// authority ONLY: the realized walk ORDER is derived from cost
    /// (capacity-major, then price; name-hash disambiguator within a
    /// band — the controller's `cell_rank`). `None` ⇒ no ladder
    /// (single-rung class, today's shape for mid/lo/metal/fetcher).
    /// Unrelated to `[sla].ladder_budget` (the explore-ladder budget).
    #[serde(default)]
    pub ladder: Option<CapacityLadder>,
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
            ladder: None,
        }
    }
}

// r[impl ctrl.nodeclaim.capacity-ladder]
/// live_050(c) — the §5-S graceful-degradation directive's typed form.
/// An ordered list of generation-rung sibling classes for one
/// hw-class. The rungs derive the class's hosting closure
/// ([`SlaConfig::retain_hosting_cells`] adds each rung's hosting-valid
/// `(class × capacity_types)` cells to every emitted intent), so the
/// existing IceBackoff mask-walk + cost ranking can ADVANCE to a rung
/// when this class's own cells are unfulfillable, instead of starving.
///
/// MEMBERSHIP authority only (the recorded option-(a) form): the
/// declared order is documentation of operator intent; the realized
/// walk order is derived from cost — capacity-major (spot before od),
/// then price, with the controller's `cell_rank` name-hash as the
/// within-band disambiguator. Declaring a rung here never reorders the
/// walk; it guarantees the rung is IN it.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CapacityLadder {
    /// Rung-sibling classes, most-preferred first (operator intent;
    /// see the type doc for what order does and does not govern).
    /// `validate_shape` rejects an empty list, an undeclared class, a
    /// self-rung, and duplicates.
    pub rungs: Vec<LadderRung>,
}

/// One rung of a [`CapacityLadder`]: a reference to a declared sibling
/// hw-class (e.g. the gen-7 twin of a gen-8 class). A struct (not a
/// bare string) so future per-rung axes land as fields, not a parallel
/// encoding.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LadderRung {
    /// Key into `[sla.hw_classes]` — must resolve (`validate_shape`).
    pub class: HwClassName,
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
    /// reads on `nodeSelectorTerms`). Pinned to the shared
    /// [`rio_common::cell_wire`] alphabet (bug_094: this vocabulary
    /// was open-coded here AND in the controller's `sketch.rs` with
    /// only same-crate round-trip tests guarding agreement).
    pub fn label(self) -> &'static str {
        rio_common::cell_wire::WireCapacity::from(self).karpenter_label()
    }

    /// Total decode over the shared capacity alphabet
    /// (`"spot"` / `"od"` / `"on-demand"` — [`rio_common::cell_wire`]).
    pub fn parse(s: &str) -> Option<Self> {
        rio_common::cell_wire::WireCapacity::parse(s).map(Self::from)
    }
}

impl From<CapacityType> for rio_common::cell_wire::WireCapacity {
    fn from(c: CapacityType) -> Self {
        match c {
            CapacityType::Spot => Self::Spot,
            CapacityType::Od => Self::OnDemand,
        }
    }
}

impl From<rio_common::cell_wire::WireCapacity> for CapacityType {
    fn from(c: rio_common::cell_wire::WireCapacity) -> Self {
        match c {
            rio_common::cell_wire::WireCapacity::Spot => Self::Spot,
            rio_common::cell_wire::WireCapacity::OnDemand => Self::Od,
        }
    }
}

/// `"h:cap"` ↔ `Cell` for the controller's `unfulfillable_cells` wire
/// encoding and `sla_ema_state.key` strings. Pinned to the shared
/// [`rio_common::cell_wire`] decoder; epoch-suffixed cell EVENTS
/// (`"h:cap@epoch"`) are rejected here — this codec serves the
/// epoch-less key lanes (price keys, `ice_masked_cells`,
/// `sla_ema_state.key`), and pre-pin it already rejected `'@'`
/// strings (the capacity token failed to parse). The ack apply plan
/// decodes evidence-plane entries through
/// [`rio_common::cell_wire::decode_cell_event`] directly, where the
/// epoch is load-bearing.
pub fn parse_cell(s: &str) -> Option<Cell> {
    let p = rio_common::cell_wire::decode_cell_event(s).ok()?;
    if p.epoch.is_some() {
        return None;
    }
    Some((p.hw_class, p.capacity.into()))
}

pub fn cell_label((h, c): &Cell) -> String {
    use rio_common::cell_wire::CELL_SEP;
    format!("{h}{CELL_SEP}{}", c.label())
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
    use super::Cell;
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;

    pub fn serialize<S: serde::Serializer>(
        m: &HashMap<Cell, f64>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        // Canonical wire form via the shared alphabet (bug_094: this
        // was the fourth open-coded copy of the vocabulary).
        let flat: HashMap<String, f64> = m
            .iter()
            .map(|((h, c), v)| {
                (
                    rio_common::cell_wire::encode_cell_event(h, (*c).into(), None),
                    *v,
                )
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
                let p = rio_common::cell_wire::decode_cell_event(&k)
                    .map_err(serde::de::Error::custom)?;
                // Helm/TOML keys are epoch-less by grammar.
                if p.epoch.is_some() {
                    return Err(serde::de::Error::custom(
                        "cell key must be h:cap (no @epoch suffix)",
                    ));
                }
                Ok(((p.hw_class, p.capacity.into()), v))
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
    /// sh-008: per-SOFT-feature sizing bias. Keyed on the I-204
    /// soft-feature names (`big-parallel` / `benchmark`); applied to
    /// the recorded `DerivationState.soft_features` partition. Restricts
    /// `solve_intent_for`'s `h_all` candidate set to classes whose
    /// [`Self::class_ceilings`] hosts at least `min_cores` — bias not
    /// constraint: when no class qualifies the restriction degrades to
    /// the unbiased set so the candidate set is never emptied. Default
    /// empty → no bias. See [`Self::soft_min_cores_for`].
    #[serde(default)]
    pub soft_feature_sizing: HashMap<String, SoftSizingHint>,
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
    /// sh-012 (D4 cores axis): the cpu-utilization corroboration band
    /// for an `ExecutorVariantFailure` to mint a `ComputeBound`
    /// witness — `cpu_seconds_total / (assigned_deadline ×
    /// assigned_cores) >= compute_bound_threshold`. Default `0.8`;
    /// must be in `(0.0, 1.0]` ([`Self::validate_shape`]).
    #[serde(default = "default_compute_bound_threshold")]
    pub compute_bound_threshold: f64,
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
    // (no consolidateExploreEpsilon — r41 bug_032: rendered, parsed,
    // never read. The consolidation pass is the windowed Nelson-Aalen
    // model (`nodeclaim_pool/consolidate.rs::consolidate_after`), not
    // ε-greedy; `NodeClaimPoolConfig` never carried it. An operator who
    // tuned `scheduler.sla.consolidateExploreEpsilon` observed no
    // change. Deleted rather than wired — there is no consolidation
    // explore loop to wire INTO.)
    /// DEPRECATED-IGNORED (live_049 L1, WO-S7-1): the flat per-cell-
    /// per-tick mint cap is RETIRED from the mint law — minting is
    /// bounded by demand (the FFD bin count over placeable-gated
    /// footprints) and the fleet budget (`maxFleetCores`), the two
    /// quantities with safety meaning
    /// (`ctrl.nodeclaim.mint-deficit-proportional`). The field is
    /// RETAINED parse-only: `SlaConfig` is `deny_unknown_fields` by
    /// design (fails-loud below), so deleting it would brick every
    /// helm-rendered scheduler config at boot; the serde default also
    /// keeps absent keys parsing. NOTHING reads it (the R12
    /// cap-reader census, cover.rs tests, pins code-readers at zero).
    #[serde(default = "default_max_node_claims_per_cell_per_tick")]
    pub max_node_claims_per_cell_per_tick: u32,
    /// Cluster identifier for `sla_ema_state` / `interrupt_samples`
    /// scoping. ADR-023 §2.13: under the global-DB topology multiple
    /// regions share one PG; without this every scheduler upserts the
    /// SAME `key` and reads every region's interrupt rows. Helm sets
    /// `scheduler.sla.cluster = .Values.karpenter.clusterName`.
    /// Empty (single-cluster default) matches the 043_sla_hardening
    /// `DEFAULT ''` so greenfield deploys need no config. Normalized
    /// at this serde seam by the ONE identity law (merged_bug_067:
    /// trim; post-trim-empty = the single-cluster default — the same
    /// law as the controller's `ClusterId::new` and the chart's
    /// `rio.clusterIdentity` mint), so every SQL bind site — the
    /// λ-filter, the EMA scope, the `interrupt_samples.cluster` stamp,
    /// and any future consumer — reads the same alphabet the exposure
    /// uids are minted from. One-time re-key note: a deployment that
    /// previously ran a whitespace-padded value re-keys its
    /// EMA/interrupt scope on upgrade (the padded scope's rows age
    /// out) — the fix taking effect, not a residual.
    #[serde(default, deserialize_with = "trim_string")]
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
    /// live_050(d): committed launch-evidence exclusion — AWS
    /// `instance-size` tokens that EXIST in `describe_instance_types`
    /// but have NO launchable capacity in the deployment region
    /// (either market). [`super::catalog::derive_ceilings`] synthesizes
    /// an `instance-size NotIn` requirement from this list for EVERY
    /// class (one mint — a loose class cannot re-import a phantom into
    /// its ceiling or the global), and the helm template projects the
    /// same list as a per-class requirement row so NodeClaims carry it
    /// to Karpenter's fleet selection. Grounding law: API existence is
    /// NOT launchability; ceiling candidacy is grounded by
    /// exclusion-only negative evidence (this list), never by
    /// cap-at-largest-observed-launch (see the catalog module doc's
    /// "Why not launch-observed"). Helm renders
    /// `karpenter.unlaunchableSizes`. Empty → no exclusion (vmtest).
    #[serde(default)]
    pub unlaunchable_sizes: Vec<String>,
}

/// merged_bug_067: the cluster-identity normalization seam — ONE
/// deserializer covers every consumer of `[sla].cluster` (the
/// λ-filter binds, the EMA scope, the `interrupt_samples.cluster`
/// stamp), mirroring the controller's `ClusterId::new` trim law so
/// the two binaries' identity alphabets cannot drift at the config
/// boundary.
fn trim_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    Ok(raw.trim().to_string())
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
fn default_max_node_claims_per_cell_per_tick() -> u32 {
    8
}

fn default_ring_buffer() -> u32 {
    32
}

fn default_compute_bound_threshold() -> f64 {
    0.8
}

/// Cold-start probe shape: `mem = mem_base + cpu × mem_per_core`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeShape {
    pub cpu: f64,
    pub mem_per_core: u64,
    pub mem_base: u64,
    /// `activeDeadlineSeconds` for unfitted (probe/explore) builds.
    /// Fitted keys derive a deadline from `wall_p99`; unfitted ones
    /// fall back to this.
    #[serde(default = "default_probe_deadline_secs")]
    pub deadline_secs: u32,
}

pub(crate) fn default_probe_deadline_secs() -> u32 {
    3600
}

/// `[sla.soft_feature_sizing.$feat]` table — sh-008. SIZING bias for a
/// soft feature (keyed on the I-204 `softFeatures` names). Separate
/// from [`ProbeShape`]: that is "first-attempt cold-start shape", this
/// is "regardless of fit, candidate hwClasses must host at least
/// `min_cores`". They compose — a `big-parallel` drv with no fit
/// cold-starts at `feature_probes.big-parallel.cpu`; once fitted,
/// `solve_full` cost-ranks over the `min_cores`-biased `h_all`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoftSizingHint {
    /// Candidate hwClasses MUST have a per-class core ceiling
    /// ([`SlaConfig::class_ceilings`]) of at least this. Bias not
    /// constraint — degrades when no class qualifies.
    pub min_cores: u32,
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

/// `karpenter.sh/capacity-type` — Karpenter's well-known capacity
/// label. ONE const for both the producer
/// (`solve::cells_to_selector_terms`, the affinity-term emit) and the
/// consumer (the ack apply plan's arm-echo decode) so the two sides
/// cannot drift (merged_bug_134: the literal was mirrored at both
/// sites). The controller keeps its own consts for the NODE-label
/// lane (`ffd::CAPACITY_TYPE_LABEL`, informer `LABEL_CAPACITY_TYPE`)
/// — those read kubelet labels, not this wire echo.
pub const LABEL_CAPACITY_TYPE: &str = "karpenter.sh/capacity-type";

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

    /// merged_bug_043(1): the hosting-class CONFIG CENSUS — one u64
    /// over the sorted per-class hosting axes (name, cfg ceilings,
    /// capacity types, provided features). The consecutive-verdict
    /// budget's RESET KEY: the live_051(c) law re-opens the heal
    /// window on "a hosting-class config reload", and pre-fix that
    /// was approximated by raw byte-equality of the controller's
    /// verdict detail — which embeds per-solve `cores`/`mem_bytes`,
    /// so routine refit/price jitter restarted the count forever and
    /// structurally defeated the budget for exactly the churn
    /// population it shipped to kill. Demand jitter is NOT a config
    /// change; this census is the typed axis.
    pub fn hosting_census(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut names: Vec<&String> = self.hw_classes.keys().collect();
        names.sort_unstable();
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for name in names {
            let d = &self.hw_classes[name];
            name.hash(&mut h);
            d.max_cores.hash(&mut h);
            d.max_mem.hash(&mut h);
            d.capacity_types.hash(&mut h);
            d.provides_features.hash(&mut h);
        }
        h.finish()
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
    /// largest real instance type matching this class's
    /// `requirements`, with `cores − 1` for the kubelet reserve
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
    // r[impl scheduler.sla.ceiling.uncatalogued-fallback+2]
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
        // sh-016 (a): an EMPTY catalog (Static cost source / API
        // failure at boot) falls to global — graceful degradation,
        // every class over-permits identically. A NON-EMPTY catalog
        // missing `h` means h's `requirements` matched zero AWS
        // instance types (operator typo / nonexistent SKU like gen-7
        // x86 c/m/r local-nvme): ceiling (0,0) so the class fails
        // every size gate and is structurally excluded from emission.
        // Pre-fix h fell to global → with cheap spot price + low
        // lead-time seed it became `e_min` for ~every drv and the
        // τ-band collapsed around a phantom that no real instance
        // could host.
        let cat =
            catalog
                .get(h)
                .copied()
                .unwrap_or(if catalog.is_empty() { global } else { (0, 0) });
        let cfg = (
            d.max_cores.unwrap_or(global.0),
            d.max_mem.unwrap_or(global.1),
        );
        (cat.0.min(cfg.0), cat.1.min(cfg.1))
    }

    /// sh-008: the effective `min_cores` bias for a derivation's
    /// recorded soft-feature partition — `max` over the configured
    /// `soft_feature_sizing` entries that match. `None` when `soft` is
    /// empty or no entry matches (the common case → no `h_all` bias).
    pub fn soft_min_cores_for(&self, soft: &[String]) -> Option<u32> {
        soft.iter()
            .filter_map(|f| self.soft_feature_sizing.get(f))
            .map(|h| h.min_cores)
            .max()
    }

    /// Arch + features routing predicate for hwClass `h`: the class's
    /// `kubernetes.io/arch` label matches `system`'s arch (or is
    /// absent — arch-agnostic class) AND [`features_compatible`] holds
    /// for `(features, provides_features)`. `None` arch (unmappable
    /// `system`, e.g. `builtin`) is a no-op on the arch axis —
    /// arch-agnostic, may land anywhere; mirrors
    /// [`Self::retain_hosting_cells`]' `want_arch.is_none_or(...)`.
    ///
    /// §Partition-single-source: this is the ONE predicate
    /// [`Self::reference_hw_class_for_system`] (intent producer) and
    /// [`Self::max_lead_for`] (forecast pre-solve gate) both call so
    /// the scheduler-side forecast horizon CANNOT drift from the class
    /// set the solve actually routes to (r33 bug_007 / A4). Size
    /// ([`Self::class_ceilings`]) is deliberately NOT here — the
    /// pre-solve gate has no `(cores, mem)` to check; the post-solve
    /// gate over `intent.hw_class_names` catches the residual.
    /// Unknown `h` ⇒ `false`.
    pub fn class_routes(&self, h: &str, arch: Option<&str>, features: &[String]) -> bool {
        self.hw_classes.get(h).is_some_and(|d| {
            arch.is_none_or(|a| {
                d.labels
                    .iter()
                    .find(|l| l.key == ARCH_LABEL)
                    .is_none_or(|l| l.value == a)
            }) && features_compatible(features, &d.provides_features)
        })
    }

    /// Per-intent forecast horizon: `max(lead_time_seed[(h, cap)])`
    /// over hwClasses [`Self::class_routes`] admits for `(system,
    /// features)`. **Seed-based approximation** of the controller's
    /// `a_open` per-cell filter (`eta < lead_time(c)`): the controller
    /// reads its learned per-cell sketch quantile (`CellSketches::
    /// lead_time`, §13b), which has no return channel to the scheduler
    /// (`AckSpawnedIntentsRequest` carries `registered_cells` and
    /// `unfulfillable_cells` but not the gauge). When learned drifts
    /// **above** the seed (e.g. metal boots slower than `probe-boot`'s
    /// one-shot measurement), this gate over-drops forecast intents
    /// the controller would have pre-warmed for — bounded loss of
    /// pre-warm latency, not correctness. The inverse (learned <
    /// seed) is acknowledged at the budget-gate sort (snapshot.rs).
    /// Avoids admitting intents the controller would unconditionally
    /// drop —
    /// pre-`r33 bug_007` the global `max(values)` raised the horizon
    /// 30× when `metal-{x86,arm}:od` seed=600 was added, admitting
    /// non-metal intents the controller's per-cell gate rejects.
    /// (r34 merged_bug_006)
    ///
    /// Empty seed map / no matching class ⇒ `0.0` (forecast disabled
    /// for this intent — every `eta ≥ 0` fails the gate).
    /// Arch-unmappable systems (`builtin`) are arch-agnostic via
    /// [`Self::class_routes`], so they retain the pre-fix global-max
    /// behaviour (size aside).
    pub fn max_lead_for(&self, system: &str, features: &[String]) -> f64 {
        let arch = rio_common::k8s::system_to_k8s_arch(system);
        self.lead_time_seed
            .iter()
            .filter(|((h, _), _)| self.class_routes(h, arch, features))
            .map(|(_, &v)| v)
            .fold(0.0, f64::max)
    }

    /// `reference_hw_class` if [`Self::class_routes`] admits it for
    /// `(system, features)` AND [`Self::class_ceilings`] hosts
    /// `(cores, mem)` AND it hosts the `cap` pin (when pinned), else
    /// the first (sorted) `hw_classes` entry that does. `None` ⇔
    /// `system` unmappable AND `features` empty (no constraint axis to
    /// route on — r35 B1) OR no configured class hosts that
    /// arch/feature/capacity at that size — caller emits empty
    /// `hw_class_names` so the controller's `fallback_cell` reaches its
    /// OWN `None` → `no_hosting_class`. An arch-unmappable system with
    /// non-empty `features` (`system="builtin"` FODs) IS routed — by
    /// feature alone, arch axis a no-op (r35 B1, §13d
    /// placement⊇provisioning STRIKE-2).
    ///
    /// merged_bug_067: `cap` is a TYPED parameter, not a caller-side
    /// post-filter — the STRIKE-7 chokepoint doc catalogs exactly this
    /// axis-omission class (bug_042 arch, mb_033 capacity-type): a
    /// "does a hosting class exist" helper that omits an adjudicated
    /// axis becomes an axis-omission factory for its callers (the
    /// capacity-blind PinGated adjudication pre-empted the pin-aware
    /// walk while a pin-honoring sibling existed). `None` = the axis
    /// is unconstrained (every pre-existing call site's semantics).
    #[allow(clippy::too_many_arguments)]
    pub fn reference_hw_class_for_system(
        &self,
        system: &str,
        cores: u32,
        mem: u64,
        features: &[String],
        catalog: &super::catalog::CatalogCeilings,
        global: (u32, u64),
        cap: Option<CapacityType>,
    ) -> Option<&str> {
        // r35 B1 (§13e B5): builtin FODs are arch-agnostic. Unmappable
        // system → arch is a no-op (everything matches); the feature
        // filter still constrains to `fetcher-*`. Mirrors
        // `retain_hosting_cells`'s `is_none_or` and `class_routes`'
        // `Option<&str>` semantics. The `arch.is_none() &&
        // features.is_empty()` guard preserves the featureless arm —
        // symmetric with `fallback_cell` so the operator pin path
        // (`bypass_cells` Some(cap) arm) cannot route a featureless
        // arch-unmappable intent to arbitrary cells.
        let arch = rio_common::k8s::system_to_k8s_arch(system);
        if arch.is_none() && features.is_empty() {
            return None;
        }
        let matches = |h: &str| {
            self.class_routes(h, arch, features)
                && cap.is_none_or(|c| self.capacity_types_for(h).contains(&c))
                && {
                    let (cc, cm) = self.class_ceilings(h, catalog, global);
                    // merged_bug_016: feasibility compares the
                    // CONSTRUCTED container quantity (the shared
                    // footprint law), never the bare solve — same
                    // form as `retain_hosting_cells`' size gate.
                    cores <= cc && rio_common::footprint::container_mem_bytes(mem) <= cm
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
    ///   provides_for(h))` ⟺ `cells_to_selector_terms` writes
    ///   `nodeAffinity{rio.build/kvm}` (`provides ∋ kvm ⟺ labels ∋
    ///   {rio.build/kvm: true}`, helm-test-pinned; pool-static
    ///   nodeSelector deleted r33 bug_002). (mb_012, r34 mb_004)
    /// - **size** — `cores ≤ class_ceilings(h).0 ∧
    ///   container_mem_bytes(mem) ≤ class_ceilings(h).1` ⟺ pod
    ///   requests ≤ Node allocatable. The shipped margins, all of
    ///   them (the pre-merged_bug_016 text here claimed "mem is
    ///   unmargined … never pinned to the ceiling, so the gap is
    ///   unhittable" — falsified on both halves: the gap was the
    ///   dead band): cores — the catalog-derived 1-core kubelet
    ///   reserve (`derive_ceilings` emits `instance_cores − 1`);
    ///   mem — the ×0.9 allocatable margin (`derive_ceilings`,
    ///   catalog.rs: kubeReserved + evictionHard + vmMemoryOverhead)
    ///   on the ceiling side, AND the worker pad + container floor
    ///   on the demand side via the shared footprint law
    ///   (`rio_common::footprint::container_mem_bytes` — the
    ///   constructed quantity this gate compares). mem IS routinely
    ///   pinned at its cap: the dispatch clamp, the at-cap floor
    ///   catch-up, and the StaleSolve re-solve all pin at
    ///   `max_hostable_solve_mem(ceiling)` (the solve-domain cap),
    ///   so pinned demand renders a container of exactly the
    ///   ceiling — inside this gate, never in a band above it.
    ///   (r29 bug_019, r40 bug_013, bughunt-11 merged_bug_016)
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
    /// correctness-of-producer.
    ///
    /// Stripped cells are `warn!`ed. The producer paths filter on every
    /// axis (arch via `h_all` / `reference_hw_class_for_system`,
    /// features via `features_compatible`, size via `class_ceilings`,
    /// capacity via `capacity_types_for` — `bypass_cells` `Some(cap)`
    /// gates the operator's pin since r31 mb_003). A strip here is a
    /// producer regression signal — the chokepoint SHOULD NOT be
    /// reached. The known residual is the `Some(memo)` arm's
    /// `all_candidates` capacity-fallback (memo-keyed, not per-poll —
    /// low blast radius; r32 candidate).
    #[allow(clippy::too_many_arguments)]
    pub fn retain_hosting_cells(
        &self,
        cells: Vec<Cell>,
        system: &str,
        demand: (u32, u64),
        required_features: &[String],
        catalog: &super::catalog::CatalogCeilings,
        global: (u32, u64),
        cap_pin: Option<CapacityType>,
    ) -> Vec<Cell> {
        let (cores, mem) = demand;
        // bug_042: arch axis. `None` (unmappable / `builtin`) → arch is
        // a no-op (everything kept) — mirrors `cells_to_selector_terms`
        // dropping unknown classes and the controller's `fallback_cell`
        // / FFD `agnostic` arch=None pass-through (r35 B1: featured
        // arch-unmappable intents route by feature alone there too).
        let want_arch = rio_common::k8s::system_to_k8s_arch(system);
        // ONE hosting predicate for the two passes below (producer
        // validation, then ladder-rung expansion) so the closure can
        // never admit a cell the strip would refuse. `None` ⇔ unknown
        // class. Per-axis booleans + the class ceiling are returned so
        // the producer pass can attribute its strip warn.
        let hosts = |h: &HwClassName, cap: &CapacityType| {
            let d = self.hw_classes.get(h)?;
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
            // r[impl ctrl.pool.gate-superset]
            // merged_bug_016 (the dead-band close): the mem axis
            // compares the CONSTRUCTED container quantity —
            // `rio_common::footprint::container_mem_bytes(solve)` —
            // against the ceiling, the SAME quantity — quantifier: census(retain_hosting_gate_equals_shared_law_oracle) — the controller's
            // provisioning partition (`cover::sizing` via
            // `intent_pod_footprint`) and its `fallback_cell`
            // admission predicate compare. The pre-fix raw `mem <= cm`
            // admitted the `(cm − pad, cm]` band that provisioning
            // rejects PADDED — the infinite advisory requeue strand of
            // exactly the largest builds.
            let (cc, cm) = self.class_ceilings(h, catalog, global);
            let size_ok = cores <= cc && rio_common::footprint::container_mem_bytes(mem) <= cm;
            // Capacity-type: an od-only class structurally never
            // hosts a `(h, Spot)` cell. The producer paths (`solve_
            // full` over `cost.cells`, the `Some(cap)` bypass)
            // SHOULD emit only configured caps — this is the
            // backstop. (mb_033)
            let cap_ok = d.capacity_types.contains(cap);
            Some((arch_ok, feat_ok, size_ok, cap_ok, (cc, cm)))
        };
        let mut out: Vec<Cell> = cells
            .into_iter()
            .filter(|(h, cap)| {
                // merged_bug_004: the operator pin is DEMAND, not
                // config — a pinned emission may carry only
                // pin-capacity cells. The axis is class-INDEPENDENT
                // (no `hw_classes` lookup needed), so it binds even
                // the unknown-class pass-through below. Every producer
                // arm honors the pin (memo filter + `all_candidates ∩
                // {cap}` fallback, bypass Some-arm, StaleSolve pinned
                // arm), so an off-pin cell reaching this chokepoint is
                // a producer regression exactly like a wrong-arch one.
                if !cap_pin.is_none_or(|p| cap == &p) {
                    tracing::warn!(
                        %h, ?cap, ?cap_pin, cores, mem,
                        "hw_class cell stripped at post-finalize chokepoint — \
                         off-pin capacity under an operator `--capacity` pin \
                         (producer pin filter regressed?)"
                    );
                    return false;
                }
                // Unknown class → no per-class constraint on ANY axis
                // (mirrors `class_ceilings`' `(MAX, MAX)` backstop for
                // size). `cells_to_selector_terms` already drops unknown
                // classes from `(terms, names)` — this branch keeps the
                // chokepoint a pass-through, not an arch/feature/cap
                // gate it can't evaluate. Calling `features_compatible
                // (_, provides_for(unknown)=[])` would wrongly strip
                // kvm intents on unknown classes.
                let Some((arch_ok, feat_ok, size_ok, cap_ok, class_cap)) = hosts(h, cap) else {
                    return true;
                };
                let ok = arch_ok && feat_ok && size_ok && cap_ok;
                if !ok {
                    // merged_bug_002 (the warn's predicate re-derived):
                    // the arch/feature/cap axes are solve-time facts a
                    // producer arm enforces — a strip on any of them IS
                    // a producer-path filter regression. The SIZE axis
                    // alone is different: demand/ceiling drift between
                    // solve and finalize is the stale-solve channel,
                    // and the emission classifier re-routes that
                    // population upstream on BOTH arms (the memo arm's
                    // post-overlay survival check + the no-memo
                    // classify), so a pure-size strip reaching this
                    // chokepoint means a demand mutation landed BETWEEN
                    // classification and the strip — name that, never
                    // "producer regression" (the pre-fix misattribution
                    // was the only signal the silent-empty channel
                    // had).
                    if arch_ok && feat_ok && cap_ok {
                        tracing::warn!(
                            %h, ?cap, cores, mem, ?class_cap,
                            "hw_class cell stripped at post-finalize chokepoint \
                             on the SIZE axis alone — demand/ceiling drifted \
                             after classification (the stale-solve channel is \
                             re-routed upstream; reaching this strip is a \
                             classify-coverage bug, not a producer regression)"
                        );
                    } else {
                        tracing::warn!(
                            %h, ?cap, cores, mem, ?class_cap,
                            ?required_features,
                            provides = ?self.provides_for(h),
                            ?want_arch, arch_ok, feat_ok, size_ok, cap_ok,
                            "hw_class cell stripped at post-finalize chokepoint — \
                             producer-path arch/feature/cap-filter regressed?"
                        );
                    }
                }
                ok
            })
            .collect();
        // r[impl ctrl.nodeclaim.capacity-ladder]
        // live_050(c) — the hosting-closure derivation (one mint site,
        // every consumer derives): each retained class's declared
        // ladder rungs join the closure as `(rung × rung.capacity_
        // types)` cells, gated by the SAME hosting predicate as the
        // producer cells. A rung failing the predicate is SKIPPED
        // quietly (an unhostable rung is not a rung — e.g. the demand
        // exceeds the rung's smaller catalog ceiling); only PRODUCER
        // cells warn on strip, preserving the regression-signal
        // contract above. Membership is deadband-independent by
        // design: the solve's admissible set prices the walk, the
        // ladder guarantees the walk has rungs — otherwise a price
        // gap > τ would silently strand the intent on a single rung
        // and ICE evidence would have nowhere to advance it (the
        // live_050 hang's structural exposure).
        //
        // r[impl sched.sla.ladder-transit]
        // merged_bug_101 + merged_bug_015: the derivation is a
        // WORKLIST FIXPOINT over TWO SEPARATED relations.
        // REACHABILITY: the walk transits EVERY declared ladder edge — quantifier: census(ladder_transits_declared_edges_independent_of_rung_admission) —
        // of a walked class — a walked rung enqueues once (walked-set
        // cycle guard) whether or not it minted any cell for THIS
        // demand. ADMISSION: the pin filter and the `hosts` predicate
        // gate only which CELLS join `out`, never which edges are
        // walked — an unhostable or pin-refused rung mints no cells
        // (kill-isolation preserved: the closure can never admit a
        // cell the strip would refuse) but its declared tail still
        // transits. The pre-merged_bug_015 walk enqueued a rung only
        // if it became a MEMBER (≥1 cell admitted), so a zero-cell
        // mid-rung — a spot-only class under an od pin, a
        // smaller-ceiling rung — silently severed the operator's
        // declared g8→g7→g6 tail in exactly the multi-generation
        // capacity event the ladder exists for (the §5-S
        // graceful-degradation directive; the live_050 strand shape).
        // The "can never silently truncate" claim binds to
        // `ladder_transits_declared_edges_independent_of_rung_admission`
        // (the pin × ceiling × hosting product table) — any future
        // per-cell filter added to this loop composes against
        // admission only. Seeds are the RETAINED classes only (a
        // stripped producer cell is already a regression signal, not
        // a closure seed). Deterministic append order: breadth-first
        // — retained-cell order × declared-rung order × the rung's
        // capacity_types order; dedup against everything already
        // present.
        let mut walked: HashSet<HwClassName> = HashSet::new();
        let mut queue: Vec<HwClassName> = Vec::new();
        for (h, _) in &out {
            if walked.insert(h.clone()) {
                queue.push(h.clone());
            }
        }
        let mut qi = 0;
        while qi < queue.len() {
            let parent = queue[qi].clone();
            qi += 1;
            let Some(ladder) = self.hw_classes.get(&parent).and_then(|d| d.ladder.as_ref()) else {
                continue;
            };
            for rung in &ladder.rungs {
                for cap in self.capacity_types_for(&rung.class) {
                    // merged_bug_004: the expansion seam inherits the
                    // pin axis its producers enforce — an operator
                    // `--capacity` pin honored by every producer arm
                    // was silently WIDENED here (rung cells minted at
                    // every configured capacity type), and the
                    // controller's spot-first `cell_rank` then bound
                    // od-pinned builds onto spot rung nodes. A rung
                    // cap outside the pin is skipped quietly, exactly
                    // like an unhostable rung (a rung the pin forbids
                    // is not a rung for THIS demand).
                    if !cap_pin.is_none_or(|p| *cap == p) {
                        continue;
                    }
                    let cell = (rung.class.clone(), *cap);
                    if out.contains(&cell) {
                        continue;
                    }
                    if hosts(&rung.class, cap).is_some_and(|(a, f, s, c, _)| a && f && s && c) {
                        out.push(cell);
                    }
                }
                // merged_bug_015 (transit-without-mint): the rung
                // class joins the WALK unconditionally — declared
                // edges are reachability, independent of whether THIS
                // demand minted any cell at the rung. The walked-set
                // makes every class enqueue at most once, so declared
                // cycles terminate; an undeclared rung class walks
                // nothing (its ladder lookup is None).
                if walked.insert(rung.class.clone()) {
                    queue.push(rung.class.clone());
                }
            }
        }
        out
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
    // r[impl scheduler.sla.global.derive+2]
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
            soft_feature_sizing: HashMap::new(),
            max_cores: Some(16.0),
            max_mem: Some(2 << 30),
            max_disk: 6 << 30,
            default_disk: 2 << 30,
            compute_bound_threshold: default_compute_bound_threshold(),
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
                    ladder: None,
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
            max_node_claims_per_cell_per_tick: default_max_node_claims_per_cell_per_tick(),
            cluster: String::new(),
            metal_sizes: Vec::new(),
            unlaunchable_sizes: Vec::new(),
        }
    }

    /// Defaults baseline for `rio-scheduler::config::Config::default()`.
    ///
    /// The binary's `Config::default()` is the defaults layer that the
    /// helm-rendered `scheduler.toml` is merged ONTO. Layer merging is
    /// per-key (not per-table), so a field set here is what the running
    /// process sees whenever the operator's TOML omits that key.
    ///
    /// That means this baseline MUST NOT pre-populate fields whose
    /// *absence* is the operator's deliberate choice:
    ///
    /// - `max_cores` / `max_mem`: §13c-3 contract — `None` under
    ///   `hwCostSource=spot` means "derive from the EC2 instance-type
    ///   catalog at boot". A `Some(16.0)` baseline (the
    ///   [`Self::test_default`] value) survives a TOML that omits
    ///   `[sla].maxCores` and short-circuits
    ///   [`Self::resolve_globals`] BEFORE the catalog derive runs —
    ///   the resolved global pins to 16, `probe.cpu=16` then fails
    ///   `validate_resolved` (`probe.cpu ≤ maxCores/4 = 4`), and the
    ///   scheduler crash-loops at boot.
    /// - `hw_classes`: the config layering deep-merges hashmaps (later
    ///   layers override per key). A `test-hw` entry here persists
    ///   alongside the operator's classes; with
    ///   `requirements = [kubernetes.io/os In [linux]]` it is a
    ///   match-everything phantom class the solver would route to.
    ///
    /// Everything else inherits from [`Self::test_default`]: those
    /// fields are either always rendered by the chart (so the baseline
    /// never wins) or non-`Option` scalars whose value is benign.
    ///
    /// This is NOT a usable config on its own — `[sla]` is mandatory
    /// (the chart always renders it). A deployment that omits the table
    /// fails [`Self::validate_shape`] at boot (`hw_classes` empty,
    /// `reference_hw_class` not in `hw_classes`). Tests that need a
    /// usable config without a TOML use [`Self::test_default`].
    pub fn defaults_baseline() -> Self {
        Self {
            max_cores: None,
            max_mem: None,
            hw_classes: HashMap::new(),
            ..Self::test_default()
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
        // merged_bug_262: `[sla] max_lead_time = inf` is valid TOML and
        // previously panicked DagActor::new at boot (IceBackoff's raw
        // from_secs_f64). The constructor now clamps, and this rejects
        // the misconfiguration loudly at config load.
        anyhow::ensure!(
            self.max_lead_time.is_finite() && self.max_lead_time > 0.0,
            "sla.max_lead_time must be finite and positive, got {} \
         (non-finite values previously crash-looped the scheduler at boot)",
            self.max_lead_time
        );
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
            self.compute_bound_threshold > 0.0 && self.compute_bound_threshold <= 1.0,
            "sla.compute_bound_threshold must be in (0.0, 1.0], got {}",
            self.compute_bound_threshold
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
        for (feat, h) in &self.soft_feature_sizing {
            anyhow::ensure!(
                h.min_cores > 0 && (h.min_cores as f64) < MAX_CORES_HARD,
                "sla.soft_feature_sizing[{feat}].min_cores must be in \
                 (0, {MAX_CORES_HARD}); got {}",
                h.min_cores
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
            // merged_bug_039 (the strict ArmDecode's SAME-COMMIT
            // precondition): the capacity key is PRODUCER-OWNED —
            // `cells_to_selector_terms` emits the authoritative
            // `karpenter.sh/capacity-type` requirement per cell, so a
            // label copy here makes every spawn echo carry TWO
            // capacity requirements and the strict decode refuses the
            // whole Ack forever (the controller redelivers a
            // forever-refusing echo). Pre-reservation this booted
            // cleanly and silently mis-armed (the decode peeked the
            // label copy, order-sensitively). Refusing at BOOT is the
            // honest breaking change: the config was already wrong.
            for l in &def.labels {
                anyhow::ensure!(
                    l.key != LABEL_CAPACITY_TYPE,
                    "sla.hwClasses[{h}].labels key {:?} shadows the \
                     producer-owned capacity axis — the scheduler emits the \
                     authoritative {LABEL_CAPACITY_TYPE} requirement per cell \
                     (cells_to_selector_terms); remove it from `labels` and \
                     use `capacityTypes` to constrain capacity",
                    l.key
                );
            }
            // live_050(c): ladder shape. A declared ladder whose rungs
            // don't resolve is a dangling hosting closure — the intent
            // would name a class no informer/catalog row backs.
            // Rejecting at boot keeps the closure derivation
            // (`retain_hosting_cells`) total over declared classes.
            if let Some(ladder) = &def.ladder {
                anyhow::ensure!(
                    !ladder.rungs.is_empty(),
                    "sla.hwClasses[{h}].ladder.rungs must be non-empty — \
                     remove the `ladder` key for a single-rung class"
                );
                let mut seen = std::collections::HashSet::new();
                for r in &ladder.rungs {
                    anyhow::ensure!(
                        r.class != *h,
                        "sla.hwClasses[{h}].ladder names itself as a rung — \
                         a class is always its own first rung; declare only \
                         the degradation siblings"
                    );
                    anyhow::ensure!(
                        self.hw_classes.contains_key(&r.class),
                        "sla.hwClasses[{h}].ladder rung {:?} not in \
                         sla.hwClasses — every rung must be a declared \
                         hw-class (the rung IS a class row: ceilings, \
                         labels, and capacity types all come from it)",
                        r.class
                    );
                    anyhow::ensure!(
                        seen.insert(&r.class),
                        "sla.hwClasses[{h}].ladder rung {:?} declared twice",
                        r.class
                    );
                }
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
        // merged_bug_016 (the R29-boundary-clause shape on the mem
        // axis): the resolved global must host at least one CONTAINER
        // under the shared footprint law. A global below
        // `CONTAINER_MEM_MIN_BYTES` collapses the hostable solve
        // domain to EMPTY (`max_hostable_solve_mem(gm) = None`):
        // every solve renders a container above the global, every
        // gate refuses, and the floor/clamp funnels pin at a
        // zero-margin cap — refuse the config instead of booting a
        // scheduler that can never dispatch a build.
        anyhow::ensure!(
            rio_common::footprint::max_hostable_solve_mem(gm).is_some(),
            "resolved global max_mem={gm} ({source}) is below the container \
             floor {} (rio_common::footprint::CONTAINER_MEM_MIN_BYTES): no \
             solve can render a hostable container — raise sla.maxMem or fix \
             the catalog derivation",
            rio_common::footprint::CONTAINER_MEM_MIN_BYTES,
        );
        // r[impl scheduler.sla.global.derive+2]
        // live_051(a)/(f2) + merged_bug_062: the global MUST NOT
        // exceed every class's JOINT hosting ceiling — demand admitted
        // at such a global is hostable by NO class, and pre-disclosure
        // the first symptom was the silent empty-cells churn (the
        // cancelled-python-builds verdict). The disclosure consumes
        // the SAME chokepoint the enforcement plane consumes
        // (`class_ceilings` = catalog ∩ cfg ∩ global): the pre-fix
        // per-axis compare against RAW catalog maxima was silent for
        // two verified-reachable forms of its own message — (1) legal
        // per-class tightening overrides (the raw maxima never see
        // them), and (2) the joint phantom: `resolve_globals` mints
        // from independent cross-class per-axis maxima, for which
        // `gc==max_cc ∧ gm==max_cm` holds BY CONSTRUCTION — the exact
        // phantom `derive_ceilings`' doc forbids within a class,
        // recreated one level up. An uncatalogued un-overridden class
        // falls to global ONLY when the catalog itself is empty
        // (Static / API-failed boot — every class is uncatalogued and
        // the fallback restores the pre-derive over-permits floor);
        // when the catalog has data the class is excluded ((0,0) —
        // the §13c-2 uncatalogued-fallback law, sh-016) and does not
        // contribute to hosting. On the
        // operator-override arm this is a signed operator act: the
        // doctrine is disclose-don't-wedge, so this WARNs with the
        // delta and both provenances rather than erroring.
        let best = self
            .hw_classes
            .keys()
            .map(|h| (h.as_str(), self.class_ceilings(h, catalog, global)))
            .max_by(|a, b| (a.1.1, a.1.0).cmp(&(b.1.1, b.1.0)));
        let hosted = self.hw_classes.keys().any(|h| {
            let (cc, cm) = self.class_ceilings(h, catalog, global);
            gc <= cc && gm <= cm
        });
        if !hosted {
            let (best_h, (bcc, bcm)) = best.unwrap_or(("<none>", (0, 0)));
            tracing::warn!(
                global_cores = gc,
                global_mem = gm,
                best_class = best_h,
                best_class_cores = bcc,
                best_class_mem = bcm,
                global_source = source,
                class_source = "effective class ceilings (catalog ∩ cfg)",
                "resolved global ceiling exceeds EVERY class ceiling \
                 JOINTLY — demand sized at the global can be hosted by no \
                 class (live_051: such demand churned as no_hosting_class \
                 until operators cancelled the builds; merged_bug_062: \
                 per-class tightening overrides and cross-class per-axis \
                 maxima both land here). Check sla.maxCores/maxMem and the \
                 per-class overrides against the catalog, or the \
                 MIN_CORES/MIN_MEM floor clamp on a degenerate catalog."
            );
        }
        let hi = gc as f64;
        self.probe.validate("sla.probe", hi)?;
        for (feat, p) in &self.feature_probes {
            p.validate(&format!("sla.feature_probes[{feat}]"), hi)?;
        }
        // sh-008: a soft min_cores above the resolved global is a
        // no-op (degrades to the unbiased set every time) — disclose
        // not reject: bias not constraint, so a misconfigured value
        // never prevents dispatch.
        for (feat, hint) in &self.soft_feature_sizing {
            if hint.min_cores > gc {
                tracing::warn!(
                    %feat, min_cores = hint.min_cores, global_cores = gc,
                    global_source = source,
                    "sla.soft_feature_sizing min_cores exceeds the resolved \
                     global ceiling — the bias will degrade to the unbiased \
                     candidate set on every solve (no class can host it)"
                );
            }
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

/// Snake-case `[sla]` TOML keys the helm template MUST render.
/// `tpl.contains` is a substring check — the template writes each as
/// `name = ...` or `[sla.name]` so the bare snake_case key is sufficient.
///
/// Class-level guard for merged_bug_056 (helm forgot `hw_cost_source`
/// → §13a unreachable in production). The unit test
/// `tests::helm_keys_complete` asserts this list ∪
/// [`HELM_NOT_RENDERED_SLA_KEYS`] covers every [`SlaConfig`] field;
/// `xtask lint helm-sla` asserts each appears in the rendered chart.
pub const HELM_RENDERED_SLA_KEYS: &[&str] = &[
    "tiers",
    "default_tier",
    "probe",
    "feature_probes",
    "soft_feature_sizing",
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
    "max_node_claims_per_cell_per_tick",
    "cluster",
    "metal_sizes",
    "unlaunchable_sizes",
];

/// `[sla]` keys intentionally NOT rendered by helm (with rationale).
/// Adding a field here requires justifying why operators never set it.
pub const HELM_NOT_RENDERED_SLA_KEYS: &[(&str, &str)] = &[
    ("ring_buffer", "internal refit window; not operator-tuned"),
    (
        "seed_corpus",
        "file path — corpus loads via ImportSlaCorpus RPC in k8s",
    ),
    (
        "compute_bound_threshold",
        "sh-012 D4 cores corroboration band; serde-defaulted (0.8), not operator-tuned",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// merged_bug_067 R-3D: the scheduler's SQL scope axis consumes
    /// the NORMALIZED cluster alphabet at every bind site — the trim
    /// lives at the ONE serde seam, so every consumer (λ-filter, EMA
    /// scope, the `interrupt_samples.cluster` stamp) and every future
    /// one reads the same alphabet the controller's `ClusterId::new`
    /// mints uids from. TRUE RED at 83e596f0c: `left: " prod-eu " /
    /// right: "prod-eu"` — pre-fix the field was a bare
    /// `#[serde(default)] String` bound raw into SQL, so ids
    /// differing only in whitespace passed the render gate as
    /// distinct, stayed distinct on the scheduler's λ-filter axis,
    /// and trim-collided on the controller's uid axis with no warn.
    #[test]
    fn sla_cluster_trims_at_the_config_seam() {
        let base_toml = r#"
            tiers = [{ name = "normal" }]
            default_tier = "normal"
            max_cores = 64.0
            max_mem = 1
            max_disk = 1
            default_disk = 1
            hw_cost_source = "static"
            reference_hw_class = "intel-8-nvme"
            [probe]
            cpu = 4.0
            mem_per_core = 1
            mem_base = 1
            [hw_classes.intel-8-nvme]
            labels = [
              { key = "karpenter.k8s.aws/instance-generation", value = "8" },
            ]
            requirements = [
              { key = "karpenter.k8s.aws/instance-generation", operator = "In", values = ["8"] },
            ]
            node_class = "rio-nvme"
            max_cores = 64
            max_mem = 1
        "#;
        let with_cluster = |cluster_line: &str| -> SlaConfig {
            toml::from_str(&format!("{cluster_line}\n{base_toml}")).unwrap()
        };
        let sla = with_cluster("cluster = \" prod-eu \"");
        assert_eq!(
            sla.cluster, "prod-eu",
            "the config seam normalizes; SQL binds consume the trimmed alphabet"
        );
        let sla = with_cluster("cluster = \"  \"");
        assert_eq!(
            sla.cluster, "",
            "whitespace-only normalizes to the single-cluster default"
        );
        let sla = with_cluster("");
        assert_eq!(sla.cluster, "", "the default stays the empty default");
    }

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
            ladder: None,
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
    /// Minimal catalog entry for the global/exclusion tests (amd64,
    /// no nvme) — mirrors catalog::tests::ce without the cross-module
    /// cfg(test) dependency.
    fn cat_entry(name: &str, cores: u32, mem_gib: u64) -> super::super::catalog::CatalogEntry {
        shipped_cat_entry(
            &name
                .chars()
                .take_while(|c| c.is_ascii_alphabetic())
                .collect::<String>(),
            &name
                .chars()
                .skip_while(|c| c.is_ascii_alphabetic())
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>(),
            name.split_once('.').map(|(_, s)| s).unwrap_or(""),
            cores,
            mem_gib,
            "amd64",
            0,
        )
    }

    /// Catalog entry from explicit Karpenter attrs (the phantom-shape
    /// census synthesizes these from each class's own requirements).
    fn shipped_cat_entry(
        category: &str,
        generation: &str,
        size: &str,
        cores: u32,
        mem_gib: u64,
        arch: &str,
        nvme_gb: i64,
    ) -> super::super::catalog::CatalogEntry {
        let mut labels = std::collections::BTreeMap::new();
        labels.insert("karpenter.k8s.aws/instance-category", category.to_owned());
        labels.insert(
            "karpenter.k8s.aws/instance-generation",
            generation.to_owned(),
        );
        labels.insert("karpenter.k8s.aws/instance-size", size.to_owned());
        labels.insert(ARCH_LABEL, arch.to_owned());
        labels.insert("karpenter.k8s.aws/instance-local-nvme", nvme_gb.to_string());
        labels.insert(
            "karpenter.k8s.aws/instance-cpu-manufacturer",
            "intel".to_owned(),
        );
        super::super::catalog::CatalogEntry {
            name: format!("{category}{generation}i.{size}"),
            cores,
            mem_bytes: mem_gib << 30,
            labels,
        }
    }

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
            # merged_bug_016: validate_resolved refuses globals below
            # the container floor (512 MiB) — 1 GiB keeps this
            # label-parsing fixture above the boundary.
            max_mem = 1073741824
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

    /// live_050(c): the capacity-degradation ladder is TYPED config —
    /// a `ladder = { rungs = [{ class = "..." }] }` row on a hw-class
    /// parses into the closed `CapacityLadder` shape and validates.
    /// Pre-fix red (run verbatim at aa1ba4371): the field did not
    /// exist and `deny_unknown_fields` refused the TOML — `unknown
    /// field `ladder`, expected one of `labels`, `requirements`, …` —
    /// the absence pin for the R7/R5' strawman disclosures.
    // r[verify ctrl.nodeclaim.capacity-ladder]
    #[test]
    fn ladder_declares_typed_rungs_in_config() {
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
            reference_hw_class = "intel-8"
            [probe]
            cpu = 4.0
            mem_per_core = 1
            mem_base = 1
            [hw_classes.intel-8]
            labels = [{ key = "rio.build/hw-band", value = "hi" }]
            requirements = [
              { key = "karpenter.k8s.aws/instance-generation", operator = "In", values = ["8"] },
            ]
            node_class = "rio-default"
            ladder = { rungs = [{ class = "intel-7" }] }
            [hw_classes.intel-7]
            labels = [{ key = "rio.build/hw-band", value = "hi-g7" }]
            requirements = [
              { key = "karpenter.k8s.aws/instance-generation", operator = "In", values = ["7"] },
            ]
            node_class = "rio-default"
        "#;
        let sla: SlaConfig = toml::from_str(toml).unwrap();
        let ladder = sla.hw_classes["intel-8"].ladder.as_ref().unwrap();
        assert_eq!(
            ladder.rungs,
            vec![LadderRung {
                class: "intel-7".into()
            }]
        );
        assert!(
            sla.hw_classes["intel-7"].ladder.is_none(),
            "absent key parses as None (single-rung class)"
        );
        sla.validate_shape().unwrap();
    }

    /// R7 (the ladder-shape violation reds): a ladder naming an
    /// undeclared class, an empty rung list, a self-rung, or a
    /// duplicate rung is REJECTED at validate with a typed error.
    /// Pre-fix the field cannot parse at all (the absence pin above).
    // r[verify ctrl.nodeclaim.capacity-ladder]
    #[test]
    fn ladder_shape_violations_rejected_at_validate() {
        let rungs_of = |classes: &[&str]| {
            Some(CapacityLadder {
                rungs: classes
                    .iter()
                    .map(|c| LadderRung { class: (*c).into() })
                    .collect(),
            })
        };
        // Undeclared rung class.
        let mut cfg = base();
        cfg.hw_classes.insert("parent".into(), test_def("k", "v"));
        cfg.hw_classes.get_mut("parent").unwrap().ladder = rungs_of(&["ghost-rung"]);
        let err = validate_both(&cfg).unwrap_err().to_string();
        assert!(
            err.contains("ghost-rung") && err.contains("not in"),
            "undeclared rung names the dangling class: {err}"
        );
        // Empty rung list.
        cfg.hw_classes.get_mut("parent").unwrap().ladder = Some(CapacityLadder { rungs: vec![] });
        let err = validate_both(&cfg).unwrap_err().to_string();
        assert!(err.contains("non-empty"), "{err}");
        // Self-rung.
        cfg.hw_classes.get_mut("parent").unwrap().ladder = rungs_of(&["parent"]);
        let err = validate_both(&cfg).unwrap_err().to_string();
        assert!(err.contains("names itself"), "{err}");
        // Duplicate rung.
        cfg.hw_classes.insert("rung-a".into(), test_def("k2", "v2"));
        cfg.hw_classes.get_mut("parent").unwrap().ladder = rungs_of(&["rung-a", "rung-a"]);
        let err = validate_both(&cfg).unwrap_err().to_string();
        assert!(err.contains("declared twice"), "{err}");
        // Positive control: a resolving, deduped, non-self ladder
        // passes.
        cfg.hw_classes.get_mut("parent").unwrap().ladder = rungs_of(&["rung-a"]);
        validate_both(&cfg).unwrap();
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
    // r[verify scheduler.sla.global.derive+2]
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

    /// merged_bug_039 config red (the strict-decode precondition):
    /// a `karpenter.sh/capacity-type` key in `hw_classes.labels`
    /// shadows the producer-owned capacity axis — pre-fix this BOOTED
    /// CLEANLY (validate_shape banned only `rio.build/*` in
    /// *requirements*) and made `cells_to_selector_terms` emit a label
    /// copy ahead of the authoritative requirement, which the pre-fix
    /// peek decoded order-sensitively. Post-fix it refuses boot naming
    /// the key and the authoritative emission site. Pre-fix, verbatim:
    /// `validate_both(&cfg)` returned Ok (the collision booted).
    #[test]
    fn hw_class_label_cannot_shadow_the_capacity_key() {
        let mut cfg = base();
        cfg.hw_classes
            .get_mut("test-hw")
            .unwrap()
            .labels
            .push(NodeLabelMatch {
                key: LABEL_CAPACITY_TYPE.into(),
                value: "on-demand".into(),
            });
        let err = validate_both(&cfg).unwrap_err().to_string();
        assert!(err.contains(LABEL_CAPACITY_TYPE), "{err}");
        assert!(err.contains("producer-owned"), "{err}");
        assert!(
            err.contains("cells_to_selector_terms"),
            "the refusal names the authoritative emission site: {err}"
        );
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

    /// Completeness: every `SlaConfig` serde field is listed in
    /// [`HELM_RENDERED_SLA_KEYS`] ∪ [`HELM_NOT_RENDERED_SLA_KEYS`].
    /// serde_json emits every struct field (incl. `Option::None` as
    /// null, empty maps as `{}`), so adding a field without classifying
    /// it (rendered vs. not-rendered + rationale) fails here.
    ///
    /// The chart-coverage check (does the helm template actually render
    /// each RENDERED key?) is `xtask lint helm-sla` — split out so the
    /// crate's unit tests don't need an `include_str!` reaching into
    /// `infra/helm/`.
    #[test]
    fn helm_keys_complete() {
        let v = serde_json::to_value(SlaConfig::test_default()).unwrap();
        let actual: std::collections::BTreeSet<&str> =
            v.as_object().unwrap().keys().map(String::as_str).collect();
        let listed: std::collections::BTreeSet<&str> = HELM_RENDERED_SLA_KEYS
            .iter()
            .copied()
            .chain(HELM_NOT_RENDERED_SLA_KEYS.iter().map(|(k, _)| *k))
            .collect();
        assert_eq!(
            actual, listed,
            "\nSlaConfig serde fields ≠ HELM_RENDERED_SLA_KEYS ∪ HELM_NOT_RENDERED_SLA_KEYS — \
             add the new field to one of the two lists (in this module, not the test)"
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
                base_global(),
                None,
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
                base_global(),
                None,
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
                base_global(),
                None,
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
                base_global(),
                None,
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
                base_global(),
                None,
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
                base_global(),
                None,
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
                base_global(),
                None,
            ),
            None
        );
    }

    /// r35 B1 (§13d placement⊇provisioning STRIKE-2): an arch-unmappable
    /// system carrying `required_features` is genuinely arch-agnostic —
    /// it routes by feature alone. `system="builtin"` FODs declare
    /// `required_features=["fetcher"]`; `system_to_k8s_arch("builtin")`
    /// returns `None` so the pre-fix `?`-early-return dropped to
    /// `hw_class_names=[]` → no per-intent affinity, no provisioner, pod
    /// permanently Pending with no alert (bug_003). A featureless
    /// arch-unmappable system (`darwin-pdp11`) keeps the `None` arm —
    /// no constraint axis to route on means no class can host it.
    #[test]
    fn reference_hw_class_for_system_builtin_routes_by_feature() {
        let mut cfg = base();
        cfg.hw_classes = HashMap::from([
            (
                "fetcher-x86".into(),
                test_def_provides(ARCH_LABEL, "amd64", &["fetcher"]),
            ),
            (
                "fetcher-arm".into(),
                test_def_provides(ARCH_LABEL, "arm64", &["fetcher"]),
            ),
            ("mid-x86".into(), test_def(ARCH_LABEL, "amd64")),
        ]);
        cfg.reference_hw_class = "mid-x86".into();
        let fetcher = vec!["fetcher".to_string()];
        // builtin FOD (`arch=None`, features=["fetcher"]) → first
        // (sorted) class providing the feature. Pre-B1 this returned
        // `None` (arch `?`-early-return), leaving `hw_class_names=[]`
        // and no provisioning path.
        assert_eq!(
            cfg.reference_hw_class_for_system(
                "builtin",
                1,
                2 << 30,
                &fetcher,
                &Default::default(),
                base_global(),
                None,
            ),
            Some("fetcher-arm"),
            "builtin FOD must route by feature to a fetcher class"
        );
        // featureless arch-unmappable → still None (no constraint axis
        // to route on; the early-return is preserved for this arm).
        assert_eq!(
            cfg.reference_hw_class_for_system(
                "darwin-pdp11",
                1,
                2 << 30,
                &[],
                &Default::default(),
                base_global(),
                None,
            ),
            None,
            "featureless arch-unmappable system stays unroutable"
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
    /// (∅⊆anything) → std-x86 leaks → kvm pod CrashLoopBackOff on
    /// ENXIO `/dev/kvm` (no metal node minted; pool-static nodeSelector
    /// deleted r33 bug_002).
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
        let kept = cfg.retain_hosting_cells(
            cells(),
            "x86_64-linux",
            (1, 0),
            &kvm,
            &cat,
            base_global(),
            None,
        );
        assert_eq!(
            names(&kept),
            vec!["metal-x86"],
            "std-x86 stripped for kvm intent"
        );
        // non-kvm intent: metal-x86 (provides=[kvm]) MUST be stripped.
        let kept = cfg.retain_hosting_cells(
            cells(),
            "x86_64-linux",
            (1, 0),
            &[],
            &cat,
            base_global(),
            None,
        );
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
            None,
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
            None,
        );
        let mut kept = names(&kept);
        kept.sort_unstable();
        assert_eq!(
            kept,
            vec!["metal-any", "metal-arm"],
            "wrong-arch metal-x86 stripped for aarch64-linux"
        );
        // unmappable system → arch axis is a no-op (everything kept).
        let kept =
            cfg.retain_hosting_cells(cells, "builtin", (1, 0), &kvm, &cat, base_global(), None);
        assert_eq!(kept.len(), 3, "unmappable system → arch-agnostic");
    }

    /// §13d STRIKE-7 (r30 mb_033): `retain_hosting_cells` filters
    /// `cap ∈ capacity_types_for(h)`. Since r31 mb_003 the bypass-path
    /// `Some(cap)` arm gates `cap ∈ capacity_types_for(h)` itself (the
    /// chokepoint is now backstop-only there, not steady-state); the
    /// remaining producer hole is the `all_candidates` capacity-fallback
    /// (memo-keyed). A `(h, cap)` the class doesn't host would route the
    /// controller's `cover_deficit` to a `by_cell[(metal, Spot)]` entry
    /// `all_cells()` (configured caps only) never visits — build hangs
    /// forever with no NodeClaim, no metric, no warn.
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
        let kept = cfg.retain_hosting_cells(
            cells,
            "x86_64-linux",
            (1, 0),
            &kvm,
            &cat,
            base_global(),
            None,
        );
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
            None,
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
                None,
            );
            assert_eq!(
                kept,
                vec![ok.clone()],
                "axis={axis}: bad cell {bad:?} not stripped"
            );
        }
    }

    // r[verify ctrl.pool.gate-superset]
    /// **W11-Z (scheduler leg, merged_bug_016 HIGH)** — *proposition:
    /// the scheduler size gate compares the CONSTRUCTED container
    /// quantity (`rio_common::footprint::container_mem_bytes`), so no
    /// solve it admits can be provisioning-rejected by the padded
    /// partition; population: the band boundary cells on a class
    /// ceiling `cm` — `cap' = max_hostable_solve_mem(cm)` (kept),
    /// `cap' + 1` (stripped), `cm − pad + 1` (band interior,
    /// stripped), `cm` (the funnel pin, stripped).*
    ///
    /// Pre-fix RED (verbatim, the raw `mem <= cm` gate): every band
    /// cell was KEPT — `band cell mem=68451041281 (cap'+1) admitted
    /// by the size gate — its padded container (68719476737 > cm
    /// 68719476736) is provisioning-rejected: the dead band` — the
    /// admitted-but-unprovisionable population the controller then
    /// requeued forever as advisory OverCap.
    #[test]
    fn retain_hosting_cells_size_gate_compares_padded_container() {
        let pad = rio_common::footprint::WORKER_MEM_OVERHEAD_BYTES;
        let cm: u64 = 64 << 30;
        let cap_prime =
            rio_common::footprint::max_hostable_solve_mem(cm).expect("64 GiB hosts a container");
        let mut cfg = base();
        cfg.max_cores = Some(256.0);
        let mut def = test_def(ARCH_LABEL, "amd64");
        def.max_cores = Some(64);
        def.max_mem = Some(cm);
        cfg.hw_classes = HashMap::from([("hi-x86".into(), def)]);
        let cat = super::super::catalog::CatalogCeilings::new();
        let global = (256u32, 256u64 << 30);
        let cell: Cell = ("hi-x86".into(), CapacityType::Spot);
        for (mem, hosted) in [
            (cap_prime, true),
            (cap_prime + 1, false),
            (cm - pad + 1, false),
            (cm, false),
        ] {
            let kept = cfg.retain_hosting_cells(
                vec![cell.clone()],
                "x86_64-linux",
                (8, mem),
                &[],
                &cat,
                global,
                None,
            );
            let container = rio_common::footprint::container_mem_bytes(mem);
            if hosted {
                assert_eq!(
                    kept,
                    vec![cell.clone()],
                    "solve mem={mem} renders container {container} <= cm {cm} — \
                     must be admitted (over-refusal would shrink the designed \
                     hostable region)"
                );
            } else {
                assert!(
                    kept.is_empty(),
                    "band cell mem={mem} admitted by the size gate — its padded \
                     container ({container} > cm {cm}) is provisioning-rejected: \
                     the dead band"
                );
            }
        }
    }

    // r[verify ctrl.pool.gate-superset]
    /// **W11-AA (scheduler side)** — *proposition: the scheduler's
    /// hosting gate EQUALS the shared footprint law over the
    /// [GEN-SET] band-boundary population
    /// (`rio_common::footprint::band_boundary_cells`, rendered from
    /// the shared maps — never hand-typed per side); population:
    /// ceilings at the container floor, 1 GiB, the ×0.9-margin
    /// shape, 64 GiB, and a sub-floor non-hosting ceiling, × the
    /// generated boundary cells of each.* The controller-side twin
    /// (`fallback_and_sizing_equal_shared_law_oracle`,
    /// nodeclaim_pool) quantifies the same generated population
    /// through its real gates; both sides equal to one oracle ⟹
    /// provisioning admits ⊇ placement admits (in fact equality on
    /// the mem axis). STRAWMAN RED (the per-side-constant
    /// reintroduction this test exists to refuse): reverting either
    /// side to its bare compare (`mem <= cm`) flips the knife-edge
    /// cells `cap' + 1 ..= ceiling` — the exact commit-1 W11-Z reds,
    /// re-derived here over the full generated population.
    #[test]
    fn retain_hosting_gate_equals_shared_law_oracle() {
        let mut cfg = base();
        cfg.max_cores = Some(256.0);
        let cat = super::super::catalog::CatalogCeilings::new();
        let global = (256u32, 1u64 << 60);
        for cm in [
            rio_common::footprint::CONTAINER_MEM_MIN_BYTES,
            1 << 30,
            (64u64 << 30) / 10 * 9, // the derive_ceilings ×0.9 shape
            64 << 30,
            rio_common::footprint::CONTAINER_MEM_MIN_BYTES - 1, // hosts nothing
        ] {
            let mut def = test_def(ARCH_LABEL, "amd64");
            def.max_cores = Some(64);
            def.max_mem = Some(cm);
            cfg.hw_classes = HashMap::from([("probe".into(), def)]);
            let cell: Cell = ("probe".into(), CapacityType::Spot);
            for mem in rio_common::footprint::band_boundary_cells(cm) {
                let kept = cfg.retain_hosting_cells(
                    vec![cell.clone()],
                    "x86_64-linux",
                    (8, mem),
                    &[],
                    &cat,
                    global,
                    None,
                );
                let oracle = rio_common::footprint::container_mem_bytes(mem) <= cm;
                assert_eq!(
                    !kept.is_empty(),
                    oracle,
                    "scheduler gate diverged from the shared law at \
                     (cm={cm}, mem={mem}): gate={}, oracle={oracle} — a \
                     per-side constant re-opened the band",
                    !kept.is_empty()
                );
            }
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
                (256u32, 256u64 << 30),
                None,
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
                (256u32, 256u64 << 30),
                None,
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
                (256u32, 256u64 << 30),
                None,
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
                (256u32, 256u64 << 30),
                None,
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
            None,
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
            None,
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
            None,
        );
        assert!(kept.is_empty());
    }

    /// live_050(c) — the hosting-closure derivation. Certifies: *a
    /// retained class's declared ladder rungs join the closure as
    /// `(rung × rung.capacity_types)` cells, deadband-independent,
    /// deduped, gated by the SAME hosting predicate as producer
    /// cells.* The membership half of W7-E (the scheduler side of the
    /// walk: the closure is what the emitted intent CAN advance to).
    // r[verify ctrl.nodeclaim.capacity-ladder]
    #[test]
    fn ladder_rungs_join_the_hosting_closure() {
        let cat = super::super::catalog::CatalogCeilings::new();
        let mut cfg = base();
        let mut parent = test_def("rio.build/hw-band", "hi");
        parent.ladder = Some(CapacityLadder {
            rungs: vec![LadderRung {
                class: "parent-g7".into(),
            }],
        });
        // Rung class is od-only: the closure must follow the RUNG's
        // capacity_types, not the parent's.
        let mut rung = test_def("rio.build/hw-band", "hi-g7");
        rung.capacity_types = vec![CapacityType::Od];
        cfg.hw_classes = HashMap::from([("parent".into(), parent), ("parent-g7".into(), rung)]);
        let kept = cfg.retain_hosting_cells(
            vec![
                ("parent".into(), CapacityType::Spot),
                ("parent".into(), CapacityType::Od),
            ],
            "x86_64-linux",
            (1, 0),
            &[],
            &cat,
            base_global(),
            None,
        );
        assert_eq!(
            kept,
            vec![
                ("parent".into(), CapacityType::Spot),
                ("parent".into(), CapacityType::Od),
                ("parent-g7".into(), CapacityType::Od),
            ],
            "rung joins with ITS capacity types only ((parent-g7, spot) \
             not minted for an od-only rung); producer cells unchanged \
             and first"
        );
        // Dedup: a producer that already emitted the rung cell (e.g.
        // the deadband admitted it) does not get a duplicate.
        let kept = cfg.retain_hosting_cells(
            vec![
                ("parent".into(), CapacityType::Od),
                ("parent-g7".into(), CapacityType::Od),
            ],
            "x86_64-linux",
            (1, 0),
            &[],
            &cat,
            base_global(),
            None,
        );
        assert_eq!(
            kept,
            vec![
                ("parent".into(), CapacityType::Od),
                ("parent-g7".into(), CapacityType::Od),
            ],
            "already-present rung cell not duplicated"
        );
    }

    // r[verify ctrl.nodeclaim.capacity-ladder]
    /// **W10-AE (merged_bug_101)** — *a closure is a fixpoint: a
    /// declared multi-generation chain (g8→g7→g6) expands
    /// TRANSITIVELY — the law's own quantifier is ≥2 transitions* (the
    /// flat one-transition sibling above is kept but NOT load-bearing
    /// for this property). Pre-fix `parents` was snapshotted from the
    /// retained cells before any rung joined, so a rung class's own
    /// declared ladder was never walked: the operator's declared g6
    /// fallback was silently dead config in exactly the
    /// multi-generation capacity event the ladder exists for (the
    /// graceful-degradation directive: a declared fallback chain is
    /// never silently dead). Cycle-guard pinned: a rung pointing BACK
    /// at its parent re-walks nothing (each class enqueues at most
    /// once).
    #[test]
    fn ladder_closure_is_a_transitive_fixpoint() {
        let cat = super::super::catalog::CatalogCeilings::new();
        let mut cfg = base();
        let rungs_of = |classes: &[&str]| {
            Some(CapacityLadder {
                rungs: classes
                    .iter()
                    .map(|c| LadderRung { class: (*c).into() })
                    .collect(),
            })
        };
        let mut g8 = test_def("rio.build/hw-band", "g8");
        g8.ladder = rungs_of(&["gen-g7"]);
        let mut g7 = test_def("rio.build/hw-band", "g7");
        g7.ladder = rungs_of(&["gen-g6"]);
        let g6 = test_def("rio.build/hw-band", "g6");
        cfg.hw_classes = HashMap::from([
            ("gen-g8".into(), g8),
            ("gen-g7".into(), g7),
            ("gen-g6".into(), g6),
        ]);
        let kept = cfg.retain_hosting_cells(
            vec![("gen-g8".into(), CapacityType::Od)],
            "x86_64-linux",
            (1, 0),
            &[],
            &cat,
            base_global(),
            None,
        );
        assert!(
            kept.contains(&("gen-g7".into(), CapacityType::Od)),
            "first transition: g7 joins (the flat law); got {kept:?}"
        );
        assert!(
            kept.contains(&("gen-g6".into(), CapacityType::Od)),
            "SECOND transition: the rung's own declared ladder is \
             walked — g6 joins the closure (pre-fix: the operator's \
             declared g8→g7→g6 chain was silently truncated at g7); \
             got {kept:?}"
        );
        // Cycle guard: g6 declares g8 (a back-edge). The walk
        // terminates and adds nothing new (every class already
        // walked once).
        let mut g6_cyclic = test_def("rio.build/hw-band", "g6");
        g6_cyclic.ladder = rungs_of(&["gen-g8"]);
        cfg.hw_classes.insert("gen-g6".into(), g6_cyclic);
        let kept_cyclic = cfg.retain_hosting_cells(
            vec![("gen-g8".into(), CapacityType::Od)],
            "x86_64-linux",
            (1, 0),
            &[],
            &cat,
            base_global(),
            None,
        );
        // Termination is the guarded property; the back-edge's
        // CONTRIBUTION is legitimate closure growth — g6's declared
        // fallback to g8 adds the parent's OTHER capacity cell
        // ((g8, Spot); the producer had emitted only (g8, Od)).
        // Every class walked exactly once.
        let mut want_cyclic = kept.clone();
        want_cyclic.push(("gen-g8".into(), CapacityType::Spot));
        assert_eq!(
            kept_cyclic, want_cyclic,
            "a declared cycle terminates after one walk per class, \
             with the back-edge's cells joined"
        );
    }

    // r[verify sched.sla.ladder-transit]
    /// **W11-AD (merged_bug_015)** — *proposition: every DECLARED
    /// rung reachable from the head is walked — a mid-chain rung
    /// minting ZERO cells does not sever the declared tail;
    /// population: the pin × ceiling × hosting product table, all
    /// zero-cell mid-rung cells (od-pinned spot-only rung;
    /// small-ceiling rung; wrong-arch rung), each on a declared
    /// g8→g7→g6 chain whose g6 tail must mint.* The pre-amendment
    /// fixpoint test (`ladder_closure_is_a_transitive_fixpoint`)
    /// quantified over fully-hostable unpinned chains only — these
    /// rows are the cells it never covered, and the walk's "can
    /// never silently truncate" claim binds HERE (the lowercase
    /// modal rode the lexicon's demote arm unbound while its
    /// machine witness quantified a strictly narrower domain).
    ///
    /// Pre-fix RED (the member-only worklist: a rung joined the walk
    /// iff ≥1 of its cells joined the closure, and both per-cell
    /// filters before membership were silent): every row severed at
    /// g7 — `axis=pin: declared tail severed at the zero-cell
    /// mid-rung — g6's cells missing from {kept:?}`.
    #[test]
    fn ladder_transits_declared_edges_independent_of_rung_admission() {
        let cat = super::super::catalog::CatalogCeilings::new();
        let rungs_of = |classes: &[&str]| {
            Some(CapacityLadder {
                rungs: classes
                    .iter()
                    .map(|c| LadderRung { class: (*c).into() })
                    .collect(),
            })
        };
        // Three zero-cell-mid-rung variants of g7, one per admission
        // axis the walk must NOT couple to reachability.
        let mk_chain = |g7_mut: &dyn Fn(&mut HwClassDef)| {
            let mut g8 = test_def("rio.build/hw-band", "g8");
            g8.ladder = rungs_of(&["gen-g7"]);
            let mut g7 = test_def("rio.build/hw-band", "g7");
            g7.ladder = rungs_of(&["gen-g6"]);
            g7_mut(&mut g7);
            let g6 = test_def("rio.build/hw-band", "g6");
            let mut cfg = base();
            cfg.hw_classes = HashMap::from([
                ("gen-g8".into(), g8),
                ("gen-g7".into(), g7),
                ("gen-g6".into(), g6),
            ]);
            cfg
        };
        // One product-table row: (axis label, the g7 perturbation,
        // the capacity pin, the demand).
        type Row<'a> = (
            &'a str,
            &'a dyn Fn(&mut HwClassDef),
            Option<CapacityType>,
            (u32, u64),
        );
        let rows: &[Row<'_>] = &[
            // Pin axis: g7 is spot-only; the demand carries an od pin
            // — the pin filter admits zero g7 cells.
            (
                "pin",
                &|g7: &mut HwClassDef| g7.capacity_types = vec![CapacityType::Spot],
                Some(CapacityType::Od),
                (1, 0),
            ),
            // Ceiling axis: g7's cores ceiling is below the demand —
            // the hosting predicate admits zero g7 cells.
            (
                "ceiling",
                &|g7: &mut HwClassDef| g7.max_cores = Some(4),
                None,
                (8, 0),
            ),
            // Hosting (arch) axis: g7 is arm-only for an x86 intent.
            (
                "arch",
                &|g7: &mut HwClassDef| {
                    g7.labels.push(NodeLabelMatch {
                        key: ARCH_LABEL.into(),
                        value: "arm64".into(),
                    })
                },
                None,
                (1, 0),
            ),
        ];
        for (axis, g7_mut, pin, demand) in rows {
            let cfg = mk_chain(g7_mut);
            let seed_cap = pin.unwrap_or(CapacityType::Od);
            let kept = cfg.retain_hosting_cells(
                vec![("gen-g8".into(), seed_cap)],
                "x86_64-linux",
                *demand,
                &[],
                &cat,
                base_global(),
                *pin,
            );
            // The zero-cell mid-rung minted nothing for THIS demand…
            assert!(
                !kept.iter().any(|(h, _)| h == "gen-g7"),
                "axis={axis}: the perturbed g7 rung must mint zero cells \
                 (admission unchanged) — got {kept:?}"
            );
            // …but the declared tail behind it still mints.
            assert!(
                kept.iter().any(|(h, _)| h == "gen-g6"),
                "axis={axis}: declared tail severed at the zero-cell \
                 mid-rung — g6's cells missing from {kept:?}"
            );
        }
    }

    /// Kill-isolation for the closure: a rung that fails the hosting
    /// predicate on any axis (size / arch / features) mints no CELLS
    /// — its declared EDGES still transit (transit-without-mint,
    /// `sched.sla.ladder-transit`). The closure can never admit a
    /// cell the producer strip would refuse.
    // r[verify ctrl.nodeclaim.capacity-ladder]
    #[test]
    fn ladder_rung_outside_its_envelope_not_added() {
        let cat = super::super::catalog::CatalogCeilings::new();
        let mut cfg = base();
        cfg.max_cores = Some(256.0);
        let mut parent = test_def("rio.build/hw-band", "hi");
        parent.max_cores = Some(128);
        parent.ladder = Some(CapacityLadder {
            rungs: vec![
                LadderRung {
                    class: "small-rung".into(),
                },
                LadderRung {
                    class: "arm-rung".into(),
                },
                LadderRung {
                    class: "kvm-rung".into(),
                },
            ],
        });
        // Size: rung ceiling 32 < demand 48.
        let mut small = test_def("rio.build/hw-band", "g7");
        small.max_cores = Some(32);
        // Arch: arm rung for an x86 intent.
        let mut arm = test_def("rio.build/hw-band", "g7");
        arm.labels.push(NodeLabelMatch {
            key: ARCH_LABEL.into(),
            value: "arm64".into(),
        });
        // Features: kvm-providing rung for a featureless intent.
        let kvm = test_def_provides("rio.build/hw-band", "g7", &["kvm"]);
        cfg.hw_classes = HashMap::from([
            ("parent".into(), parent),
            ("small-rung".into(), small),
            ("arm-rung".into(), arm),
            ("kvm-rung".into(), kvm),
        ]);
        let kept = cfg.retain_hosting_cells(
            vec![("parent".into(), CapacityType::Od)],
            "x86_64-linux",
            (48, 0),
            &[],
            &cat,
            (256u32, 256u64 << 30),
            None,
        );
        assert_eq!(
            kept,
            vec![("parent".into(), CapacityType::Od)],
            "size-, arch-, and feature-incompatible rungs all skipped"
        );
    }

    /// Regression pin (the quiet edge): a config with NO ladders
    /// leaves `retain_hosting_cells` byte-identical to the pre-ladder
    /// behavior — the closure derivation is inert for single-rung
    /// classes. (The pre-fix tree IS this behavior; the reverse-
    /// strawman transcript for R5 shows the wired closure's absence.)
    // r[verify ctrl.nodeclaim.capacity-ladder]
    #[test]
    fn no_ladder_leaves_closure_unchanged() {
        let cat = super::super::catalog::CatalogCeilings::new();
        let mut cfg = base();
        cfg.hw_classes = HashMap::from([
            ("a".into(), test_def("k", "a")),
            ("b".into(), test_def("k", "b")),
        ]);
        let cells = vec![
            ("a".into(), CapacityType::Spot),
            ("b".into(), CapacityType::Od),
        ];
        let kept = cfg.retain_hosting_cells(
            cells.clone(),
            "x86_64-linux",
            (1, 0),
            &[],
            &cat,
            base_global(),
            None,
        );
        assert_eq!(kept, cells, "no ladder ⇒ closure == producer cells");
    }

    /// Option-(a) AGREEMENT census (R15): the ladder field is
    /// MEMBERSHIP authority; the ranking machinery executes over
    /// exactly that membership. Product-iterates every declared
    /// (parent, rung) pair FROM the parsed config alphabet and asserts
    /// closure == producer ∪ (rung × rung.capacity_types ∩ hosting) —
    /// one authority, never two unchecked ones for one walk.
    // r[verify ctrl.nodeclaim.capacity-ladder]
    #[test]
    fn ladder_membership_agreement_census() {
        let cat = super::super::catalog::CatalogCeilings::new();
        let mut cfg = base();
        let mut p1 = test_def("rio.build/hw-band", "hi");
        p1.ladder = Some(CapacityLadder {
            rungs: vec![LadderRung { class: "r1".into() }],
        });
        let mut p2 = test_def("rio.build/hw-band", "lo");
        p2.ladder = Some(CapacityLadder {
            rungs: vec![
                LadderRung { class: "r1".into() },
                LadderRung { class: "r2".into() },
            ],
        });
        let mut r2 = test_def("rio.build/hw-band", "lo-g6");
        r2.capacity_types = vec![CapacityType::Od];
        cfg.hw_classes = HashMap::from([
            ("p1".into(), p1),
            ("p2".into(), p2),
            ("r1".into(), test_def("rio.build/hw-band", "g7")),
            ("r2".into(), r2),
        ]);
        cfg.reference_hw_class = "p1".into();
        validate_both(&cfg).unwrap();
        // Census rows derive from the parsed alphabet, not author rows.
        for (parent, def) in cfg.hw_classes.iter().filter(|(_, d)| d.ladder.is_some()) {
            let producer = vec![(parent.clone(), CapacityType::Od)];
            let kept = cfg.retain_hosting_cells(
                producer.clone(),
                "x86_64-linux",
                (1, 0),
                &[],
                &cat,
                base_global(),
                None,
            );
            let mut expect = producer;
            for rung in &def.ladder.as_ref().unwrap().rungs {
                for cap in cfg.capacity_types_for(&rung.class) {
                    let cell = (rung.class.clone(), *cap);
                    if !expect.contains(&cell) {
                        expect.push(cell);
                    }
                }
            }
            assert_eq!(
                kept, expect,
                "closure({parent}) == declared membership × rung capacity types"
            );
        }
    }

    /// The cell-config census [GEN-SET] (R15; W7-G's totality half):
    /// derives the FULL supply-cell universe from the SHIPPED helm
    /// values — `scheduler.sla.hwClasses` parsed in-test (serde-saphyr,
    /// the workspace YAML stack) — as classes × capacityTypes (absent
    /// ⇒ [spot, on-demand]) + the declared ladder edges, and pins it
    /// against the committed expectation (the generator's own output
    /// at this commit; generating command in the commit body). Fails
    /// on ANY membership drift — closure tomorrow, not completeness
    /// today. Then the STRONG half: the parsed rows are assembled into
    /// the production `SlaConfig`, `validate_shape` must accept the
    /// shipped chart (every rung resolves, every seed row keys a
    /// class), and the closure law re-derives every declared ladder
    /// edge through `retain_hosting_cells` — the shipped values and
    /// the Rust laws cannot drift apart silently.
    /// Shared mirror of the helm values subset the shipped-values
    /// censuses read (tolerant — no deny_unknown_fields; the chart has
    /// many keys these tests don't consume).
    mod shipped {
        #[derive(serde::Deserialize)]
        pub struct Root {
            pub scheduler: SchedV,
            pub karpenter: KarpV,
        }
        #[derive(serde::Deserialize)]
        pub struct KarpV {
            #[serde(rename = "metalSizes")]
            pub metal_sizes: Vec<String>,
            #[serde(rename = "unlaunchableSizes")]
            pub unlaunchable_sizes: Vec<String>,
        }
        #[derive(serde::Deserialize)]
        pub struct SchedV {
            pub sla: SlaV,
        }
        #[derive(serde::Deserialize)]
        pub struct SlaV {
            #[serde(rename = "hwClasses")]
            pub hw_classes: std::collections::BTreeMap<String, ClassV>,
            #[serde(rename = "referenceHwClass")]
            pub reference_hw_class: String,
            #[serde(rename = "leadTimeSeed")]
            pub lead_time_seed: std::collections::BTreeMap<String, f64>,
            #[serde(rename = "maxLeadTime")]
            pub max_lead_time: f64,
        }
        #[derive(serde::Deserialize)]
        pub struct ClassV {
            #[serde(rename = "nodeClass")]
            pub node_class: String,
            pub labels: Vec<KvV>,
            pub requirements: Vec<ReqV>,
            #[serde(rename = "capacityTypes")]
            pub capacity_types: Option<Vec<String>>,
            pub ladder: Option<LadderV>,
            #[serde(rename = "providesFeatures")]
            pub provides_features: Option<Vec<String>>,
            #[serde(rename = "maxCores")]
            pub max_cores: Option<u32>,
            #[serde(rename = "maxMem")]
            pub max_mem: Option<u64>,
            #[serde(rename = "maxFleetCores")]
            pub _max_fleet_cores: Option<u32>,
            pub taints: Option<Vec<TaintV>>,
        }
        #[derive(serde::Deserialize)]
        pub struct KvV {
            pub key: String,
            pub value: String,
        }
        #[derive(serde::Deserialize)]
        pub struct ReqV {
            pub key: String,
            pub operator: String,
            pub values: Option<Vec<String>>,
        }
        #[derive(serde::Deserialize)]
        pub struct LadderV {
            pub rungs: Vec<RungV>,
        }
        #[derive(serde::Deserialize)]
        pub struct RungV {
            pub class: String,
        }
        #[derive(serde::Deserialize)]
        pub struct TaintV {
            pub key: String,
            pub value: String,
            pub effect: String,
        }
        pub fn parse() -> Root {
            // RUNTIME env var, not the compile-time `env!` macro: under
            // the nix nextest sandbox the compile dir is long gone —
            // cargo/nextest set CARGO_MANIFEST_DIR to the remapped
            // workspace member at run time, and the workspace fileset
            // carries the chart values (the docs/gen/metrics.json
            // precedent in rio-test-support::metrics).
            let path = format!(
                "{}/../infra/helm/rio-build/values.yaml",
                std::env::var("CARGO_MANIFEST_DIR")
                    .expect("CARGO_MANIFEST_DIR set by cargo/nextest")
            );
            let body = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read shipped values at {path}: {e}"));
            serde_saphyr::from_str(&body).expect("shipped values parse")
        }
    }

    /// Build the production `HwClassDef` map from the parsed shipped
    /// rows (field-by-field, the same projection the helm template +
    /// TOML parse performs).
    fn shipped_hw_classes(sla_v: &shipped::SlaV) -> HashMap<HwClassName, HwClassDef> {
        sla_v
            .hw_classes
            .iter()
            .map(|(h, d)| {
                (
                    h.clone(),
                    HwClassDef {
                        labels: d
                            .labels
                            .iter()
                            .map(|l| NodeLabelMatch {
                                key: l.key.clone(),
                                value: l.value.clone(),
                            })
                            .collect(),
                        requirements: d
                            .requirements
                            .iter()
                            .map(|r| NodeSelectorReq {
                                key: r.key.clone(),
                                operator: r.operator.clone(),
                                values: r.values.clone().unwrap_or_default(),
                            })
                            .collect(),
                        node_class: d.node_class.clone(),
                        max_cores: d.max_cores,
                        max_mem: d.max_mem,
                        taints: d
                            .taints
                            .iter()
                            .flatten()
                            .map(|t| NodeTaint {
                                key: t.key.clone(),
                                value: t.value.clone(),
                                effect: t.effect.clone(),
                            })
                            .collect(),
                        provides_features: d.provides_features.clone().unwrap_or_default(),
                        max_fleet_cores: None,
                        capacity_types: d
                            .capacity_types
                            .as_ref()
                            .map(|caps| {
                                caps.iter()
                                    .map(|c| CapacityType::parse(c).expect("shipped cap token"))
                                    .collect()
                            })
                            .unwrap_or_else(default_capacity_types),
                        ladder: d.ladder.as_ref().map(|l| CapacityLadder {
                            rungs: l
                                .rungs
                                .iter()
                                .map(|r| LadderRung {
                                    class: r.class.clone(),
                                })
                                .collect(),
                        }),
                    },
                )
            })
            .collect()
    }

    // r[verify ctrl.nodeclaim.capacity-ladder]
    /// **The spot+od doctrine witness (owner directive, 2026-06-10;
    /// the W7-D mechanics template at the VALUES level)** — *the
    /// SHIPPED hi class serves spot first and fails over to od when
    /// the spot plane ICEs*: parse values.yaml, build the production
    /// `SlaConfig` rows for the representative class (hi-nvme-x86 +
    /// its g7 rung), drive the production emission; the FIRST
    /// emission's closure carries the spot cells (the doctrine's
    /// spot-preferred half — RED against the pre-directive od-only
    /// chart: zero spot cells existed in the closure, the reversal
    /// transcript in the commit body), then `cell_wire` ICE marks on
    /// the WHOLE spot plane leave a non-empty all-od unmasked set on
    /// the next emission (the od-fallback half; the WO-S7-4 evidence
    /// path, leg-B codec). No order enforcement anywhere: membership
    /// (closure) + cost ranking + read-time `A \ masked` compose to
    /// the directive's semantics. Catalog lane not engaged (ceilings
    /// fall to the global — this red probes the capacity axis, not
    /// sizing; the sizing composition is
    /// `phantom_ceiling_rung_advances_not_starves`).
    #[tokio::test]
    async fn shipped_hi_spot_plane_ice_fails_over_to_on_demand() {
        use rio_common::cell_wire::{EvidenceEpoch, WireCapacity, encode_cell_event};
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        crate::actor::tests::seed_default_tenant(&db.pool).await;
        let mut actor = crate::actor::tests::helpers::bare_actor_hw_builders_only(db.pool.clone());
        // The SHIPPED rows under test: the representative hi class +
        // its declared rung, straight from values.yaml.
        let root = shipped::parse();
        let shipped_classes = shipped_hw_classes(&root.scheduler.sla);
        let mut classes = HashMap::new();
        for h in ["hi-nvme-x86", "hi-nvme-x86-g7"] {
            classes.insert(h.to_string(), shipped_classes[h].clone());
        }
        actor.sla_config.hw_classes = classes;
        actor.sla_config.reference_hw_class = "hi-nvme-x86".into();
        // bug_119: the shipped classes replaced the fixture's set —
        // re-arm the membership snapshot (sched.sla.class-membership).
        crate::actor::tests::helpers::rearm_membership(&mut actor);
        let mut m = std::collections::HashMap::new();
        m.insert("hi-nvme-x86".to_string(), 2.0);
        m.insert("hi-nvme-x86-g7".to_string(), 1.4);
        actor
            .sla_estimator
            .seed_hw(crate::sla::hw::HwTable::from_map(m));
        crate::actor::tests::helpers::seed_fit(&actor, "test-pkg");
        actor.test_inject_ready("d-doctrine", Some("test-pkg"), "x86_64-linux", false);

        let cells_of = |i: &rio_proto::types::SpawnIntent| -> Vec<(String, String)> {
            i.hw_class_names
                .iter()
                .zip(&i.node_affinity)
                .map(|(h, t)| {
                    let cap = t
                        .match_expressions
                        .iter()
                        .find(|e| e.key == "karpenter.sh/capacity-type")
                        .map(|e| e.values[0].clone())
                        .unwrap_or_default();
                    (h.clone(), cap)
                })
                .collect()
        };

        // Emission 1: the closure carries the spot plane (RED against
        // the od-only chart) AND the od plane (membership, not a mask
        // reaction).
        let snap = actor.compute_spawn_intents(&Default::default());
        let i1 = snap
            .intents
            .iter()
            .find(|i| i.intent_id == "d-doctrine")
            .expect("emitted")
            .clone();
        let c1 = cells_of(&i1);
        assert!(
            c1.iter().any(|(_, cap)| cap == "spot"),
            "the doctrine's spot-preferred half: the SHIPPED hi closure \
             admits spot cells (pre-directive od-only chart: none): {c1:?}"
        );
        assert!(
            c1.iter().any(|(_, cap)| cap == "on-demand"),
            "the od plane is in the closure from the FIRST emission \
             (membership): {c1:?}"
        );

        // The spot plane ICEs (vanish-as-ICE / unfulfillable evidence
        // through the production codec: encode -> decode -> the same
        // `apply_mark_event` fold the ack-apply plane commits — the
        // wire round-trip kept, one seam in; `handle_ack_spawned_
        // intents` is actor-private, the W7-D leg-B convention is the
        // codec + the epoch'd mark fold).
        for h in ["hi-nvme-x86", "hi-nvme-x86-g7"] {
            let wire = encode_cell_event(h, WireCapacity::Spot, Some(EvidenceEpoch(1)));
            let ev = rio_common::cell_wire::decode_cell_event(&wire).expect("codec round-trip");
            actor
                .ice
                .apply_mark_event(&(ev.hw_class, ev.capacity.into()), ev.epoch);
        }

        // Emission 2: the unmasked set is NON-EMPTY and ALL od — od
        // serves exactly when spot cannot.
        let snap2 = actor.compute_spawn_intents(&Default::default());
        let i2 = snap2
            .intents
            .iter()
            .find(|i| i.intent_id == "d-doctrine")
            .expect("re-emitted")
            .clone();
        let masked: std::collections::HashSet<(String, String)> = snap2
            .ice_masked_cells
            .iter()
            .map(|s| {
                let (h, cap) = parse_cell(s).expect("snapshot cell decodes");
                (h, cap.label().to_string())
            })
            .collect();
        let unmasked: Vec<(String, String)> = cells_of(&i2)
            .into_iter()
            .filter(|c| !masked.contains(c))
            .collect();
        assert!(
            !unmasked.is_empty(),
            "the walk has an od rung to serve (never starve while a \
             buyable plane exists)"
        );
        assert!(
            unmasked.iter().all(|(_, cap)| cap == "on-demand"),
            "with the whole spot plane masked, od serves: {unmasked:?}"
        );
    }

    // r[verify ctrl.nodeclaim.capacity-ladder]
    #[test]
    fn shipped_values_cell_universe_census() {
        let root = shipped::parse();
        let sla_v = root.scheduler.sla;

        // 1) The derived universe == the committed [GEN-SET] expectation.
        let mut cells: Vec<String> = Vec::new();
        let mut edges: Vec<String> = Vec::new();
        for (h, d) in &sla_v.hw_classes {
            let caps = d
                .capacity_types
                .clone()
                .unwrap_or_else(|| vec!["spot".into(), "on-demand".into()]);
            for c in &caps {
                cells.push(format!("{h}:{c}"));
            }
            if let Some(l) = &d.ladder {
                for r in &l.rungs {
                    edges.push(format!("{h} -> {}", r.class));
                }
            }
        }
        let expect_cells: Vec<&str> = vec![
            "fetcher-arm:spot",
            "fetcher-arm:on-demand",
            "fetcher-x86:spot",
            "fetcher-x86:on-demand",
            "hi-ebs-arm:spot",
            "hi-ebs-arm:on-demand",
            "hi-ebs-arm-g7:spot",
            "hi-ebs-arm-g7:on-demand",
            "hi-ebs-x86:spot",
            "hi-ebs-x86:on-demand",
            "hi-ebs-x86-g7:spot",
            "hi-ebs-x86-g7:on-demand",
            "hi-nvme-arm:spot",
            "hi-nvme-arm:on-demand",
            "hi-nvme-arm-g7:spot",
            "hi-nvme-arm-g7:on-demand",
            "hi-nvme-x86:spot",
            "hi-nvme-x86:on-demand",
            "hi-nvme-x86-g7:spot",
            "hi-nvme-x86-g7:on-demand",
            "lo-ebs-arm:spot",
            "lo-ebs-arm:on-demand",
            "lo-ebs-x86:spot",
            "lo-ebs-x86:on-demand",
            "lo-nvme-arm:spot",
            "lo-nvme-arm:on-demand",
            "lo-nvme-x86:spot",
            "lo-nvme-x86:on-demand",
            // M1 (owner-signed, bughunt-9): metal joined spot+od —
            // the spot cells entered the shipped universe.
            "metal-arm:spot",
            "metal-arm:on-demand",
            "metal-x86:spot",
            "metal-x86:on-demand",
            "mid-ebs-arm:spot",
            "mid-ebs-arm:on-demand",
            "mid-ebs-x86:spot",
            "mid-ebs-x86:on-demand",
            "mid-nvme-arm:spot",
            "mid-nvme-arm:on-demand",
            "mid-nvme-x86:spot",
            "mid-nvme-x86:on-demand",
        ];
        assert_eq!(
            cells, expect_cells,
            "shipped cell universe drifted — regenerate the [GEN-SET] \
             (commit body) and re-derive the ladder/posture consequences"
        );
        assert_eq!(
            edges,
            vec![
                "hi-ebs-arm -> hi-ebs-arm-g7",
                "hi-ebs-x86 -> hi-ebs-x86-g7",
                "hi-nvme-arm -> hi-nvme-arm-g7",
                "hi-nvme-x86 -> hi-nvme-x86-g7",
            ],
            "declared ladder edges drifted"
        );

        // 2) The STRONG half: shipped rows through the production
        //    parser laws. Assemble the real SlaConfig and validate.
        let mut cfg = base();
        cfg.max_cores = Some(384.0);
        cfg.max_mem = Some(4096 << 30);
        cfg.reference_hw_class = sla_v.reference_hw_class.clone();
        cfg.max_lead_time = sla_v.max_lead_time;
        cfg.hw_classes = shipped_hw_classes(&sla_v);
        cfg.lead_time_seed = sla_v
            .lead_time_seed
            .iter()
            .map(|(k, v)| (parse_cell(k).expect("shipped seed key decodes"), *v))
            .collect();
        cfg.validate_shape()
            .expect("the SHIPPED chart values pass the production validator");

        // 3) The closure law re-derives every declared edge: for each
        //    ladder'd parent, retain_hosting_cells(parent cells) ==
        //    parent cells + the rung's od cell (the shipped od-only
        //    posture).
        let cat = super::super::catalog::CatalogCeilings::new();
        let arch_of = |d: &HwClassDef| {
            d.labels
                .iter()
                .find(|l| l.key == ARCH_LABEL)
                .map(|l| l.value.clone())
        };
        for (h, d) in cfg.hw_classes.clone() {
            let Some(ladder) = &d.ladder else { continue };
            let system = match arch_of(&d).as_deref() {
                Some("arm64") => "aarch64-linux",
                _ => "x86_64-linux",
            };
            let producer: Vec<Cell> = d
                .capacity_types
                .iter()
                .map(|cap| (h.clone(), *cap))
                .collect();
            let kept = cfg.retain_hosting_cells(
                producer.clone(),
                system,
                (1, 0),
                &[],
                &cat,
                (384u32, 4096u64 << 30),
                None,
            );
            let mut expect = producer;
            for rung in &ladder.rungs {
                for cap in cfg.capacity_types_for(&rung.class) {
                    expect.push((rung.class.clone(), *cap));
                }
            }
            assert_eq!(
                kept, expect,
                "shipped closure({h}) == parent cells + rung od cells"
            );
        }
    }

    /// R21 / W7-T (live_051(a) — the loose-class import kill):
    /// certifies *a catalog containing a phantom top type matched by a
    /// class WITHOUT its own exclusion row yields a global equal to
    /// the max HONEST class ceiling, not the phantom* — through the
    /// production `derive_ceilings ∘ resolve_globals` composition with
    /// the committed exclusion active. The exclusion binds at the
    /// derive seam for EVERY class (one mint), so the loose class
    /// cannot re-import. Pre-fix left reproduced in-test via the
    /// violability lane (empty exclusion ⇒ phantom global 383).
    // r[verify scheduler.sla.global.derive+2]
    #[test]
    fn loose_class_does_not_set_the_global() {
        let catalog_rows = vec![
            cat_entry("r8i.96xlarge", 384, 3072),
            cat_entry("c8a.48xlarge", 192, 384),
        ];
        let mk_class = || {
            let mut d = test_def("rio.build/hw-band", "hi");
            d.max_cores = None;
            d.max_mem = None;
            d.requirements = vec![
                NodeSelectorReq {
                    key: "karpenter.k8s.aws/instance-category".into(),
                    operator: "In".into(),
                    values: vec!["c".into(), "m".into(), "r".into()],
                },
                NodeSelectorReq {
                    key: "karpenter.k8s.aws/instance-generation".into(),
                    operator: "In".into(),
                    values: vec!["8".into()],
                },
            ];
            d
        };
        let mut cfg = base();
        cfg.max_cores = None;
        cfg.max_mem = None;
        cfg.hw_cost_source = super::super::cost::HwCostSource::Spot;
        cfg.hw_classes = HashMap::from([
            ("honest".into(), mk_class()),
            // The LOOSE class: identical requirements, no exclusion row
            // of its own — the live fetcher-Gt-5 shape.
            ("loose".into(), mk_class()),
        ]);
        let shipped_exclusions: Vec<String> = ["96xlarge", "metal-96xl"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let ceilings = super::super::catalog::derive_ceilings(
            &catalog_rows,
            &cfg.hw_classes,
            &[],
            &shipped_exclusions,
        );
        let ((gc, _), src) = cfg.resolve_globals(&ceilings).unwrap();
        assert_eq!(
            (gc, src),
            (191, "derived from catalog max"),
            "global == max honest class ceiling — the loose class \
             cannot re-import the phantom"
        );
        // Violability lane (the pre-fix left): no committed exclusion
        // ⇒ the loose class imports the phantom into the global.
        let ceilings =
            super::super::catalog::derive_ceilings(&catalog_rows, &cfg.hw_classes, &[], &[]);
        let ((gc, _), _) = cfg.resolve_globals(&ceilings).unwrap();
        assert_eq!(gc, 383, "violability: empty exclusion reproduces the left");
    }

    /// R22 / W7-U (live_051(f2) — the boot WARN rider): certifies *an
    /// operator override above every class ceiling boots with the
    /// disclosure WARN naming the delta and both provenances*;
    /// kill-isolation: an override within the class range stays
    /// silent. WARN not error — the override is a signed operator act
    /// (disclose-don't-wedge). Pre-fix left: silent boot; the first
    /// symptom was live_051(b)'s empty-cells churn.
    // r[verify scheduler.sla.global.derive+2]
    #[test]
    #[tracing_test::traced_test]
    fn overridden_global_above_every_class_warns_at_boot() {
        let mut cfg = base();
        // merged_bug_062: the disclosure now reads the EFFECTIVE
        // class ceilings (catalog ∩ cfg) over the CONFIGURED classes
        // — a catalog row for an undeclared class hosts nothing, and
        // base()'s classes are cfg-bounded at (64, 256 GiB). Declare
        // h1 un-overridden so the within-range face is genuinely
        // (jointly) hostable.
        cfg.hw_classes.clear();
        let mut h1 = test_def("rio.build/hw-class", "h1");
        h1.max_cores = None;
        h1.max_mem = None;
        cfg.hw_classes.insert("h1".into(), h1);
        let catalog: super::super::catalog::CatalogCeilings =
            HashMap::from([("h1".into(), (191u32, 345u64 << 30))]);
        // Kill-isolation FIRST (cumulative log capture): an override
        // within the class range is silent.
        cfg.validate_resolved((191, 345 << 30), &catalog, "sla.maxCores/maxMem")
            .unwrap();
        assert!(
            !logs_contain("exceeds EVERY class ceiling"),
            "within-range override stays silent"
        );
        // The disclosure: an override above every class ceiling WARNs
        // with the delta and both provenances.
        cfg.validate_resolved((400, 4096 << 30), &catalog, "sla.maxCores/maxMem")
            .unwrap();
        assert!(
            logs_contain("exceeds EVERY class ceiling"),
            "the unhostable-global foot-gun is named before the first \
             solve consumes it"
        );
    }

    /// **W9-AC (merged_bug_062)** — *the boot disclosure consumes the
    /// enforcement chokepoint*: WARN iff no single class JOINTLY hosts
    /// the resolved global against the effective `class_ceilings`
    /// (catalog ∩ cfg) — the message's own predicate. The pre-fix
    /// per-axis compare against RAW catalog maxima was silent for two
    /// verified-reachable forms of its own stated condition:
    /// (1) legal per-class tightening overrides (`class_ceilings =
    /// min(cat, cfg)` — the raw maxima never see them); (2) the joint
    /// phantom — `resolve_globals` mints from independent cross-class
    /// per-axis maxima, for which `gc==max_cc ∧ gm==max_cm` holds BY
    /// CONSTRUCTION (the exact phantom `derive_ceilings`' doc forbids
    /// within a class, recreated one level up). Dual face: a jointly
    /// hostable global stays silent.
    // r[verify scheduler.sla.global.derive+2]
    #[test]
    #[tracing_test::traced_test]
    fn boot_warn_fires_on_joint_unhostability() {
        let mut cfg = base();
        cfg.hw_classes.clear();
        let mut wide = test_def("rio.build/hw-class", "wide");
        wide.max_cores = None;
        wide.max_mem = None;
        cfg.hw_classes.insert("h-cpu".into(), wide.clone());
        cfg.hw_classes.insert("h-mem".into(), wide.clone());
        // Joint phantom: h-cpu hosts the cores axis, h-mem the mem
        // axis — the per-axis maxima (64, 256 GiB), exactly what
        // resolve_globals mints, fit NO single class.
        let catalog: super::super::catalog::CatalogCeilings = HashMap::from([
            ("h-cpu".into(), (64u32, 8u64 << 30)),
            ("h-mem".into(), (2u32, 256u64 << 30)),
        ]);
        // Dual face FIRST (cumulative log capture): a global jointly
        // hosted by h-cpu stays silent.
        cfg.validate_resolved((64, 8 << 30), &catalog, "sla.maxCores/maxMem")
            .unwrap();
        assert!(
            !logs_contain("hosted by no class"),
            "jointly hostable global stays silent"
        );
        // Form 2 — the joint phantom fires.
        cfg.validate_resolved((64, 256 << 30), &catalog, "derived from catalog max")
            .unwrap();
        assert!(
            logs_contain("hosted by no class"),
            "the joint phantom (cross-class per-axis maxima) must fire \
             the boot disclosure (merged_bug_062 form 2)"
        );
    }

    /// W9-AC form 1 (merged_bug_062): a legal per-class TIGHTENING
    /// override fleet — every class cfg-tightened below the global —
    /// fires the disclosure even though the raw catalog maxima still
    /// cover the global per-axis.
    // r[verify scheduler.sla.global.derive+2]
    #[test]
    #[tracing_test::traced_test]
    fn boot_warn_fires_on_override_tightened_fleet() {
        let mut cfg = base();
        cfg.hw_classes.clear();
        let mut tight = test_def("rio.build/hw-class", "tight");
        tight.max_cores = Some(32);
        tight.max_mem = Some(64 << 30);
        cfg.hw_classes.insert("h1".into(), tight);
        // The catalog covers the global per-axis — the pre-fix check
        // was structurally silent here.
        let catalog: super::super::catalog::CatalogCeilings =
            HashMap::from([("h1".into(), (64u32, 256u64 << 30))]);
        cfg.validate_resolved((64, 256 << 30), &catalog, "sla.maxCores/maxMem")
            .unwrap();
        assert!(
            logs_contain("hosted by no class"),
            "an override-tightened fleet must fire the boot disclosure \
             (merged_bug_062 form 1)"
        );
    }

    /// The global-provenance census (R15): hand-written law table over
    /// the (override × cost-source × catalog-emptiness) product — 8
    /// cells from the alphabet, each arm's law asserted against
    /// `resolve_globals`. The oracle is this table, not the impl.
    // r[verify scheduler.sla.global.derive+2]
    #[test]
    fn resolve_globals_provenance_census() {
        use super::super::cost::HwCostSource as Src;
        let cat_full: super::super::catalog::CatalogCeilings =
            HashMap::from([("h".into(), (191u32, 345u64 << 30))]);
        let cat_empty = super::super::catalog::CatalogCeilings::new();
        // (override?, source, catalog-nonempty?) -> expected
        // Ok((cores, source-str)) / Err.
        #[allow(clippy::type_complexity)]
        let law: Vec<((bool, Src, bool), Option<(u32, &str)>)> = vec![
            ((true, Src::Spot, true), Some((400, "sla.maxCores/maxMem"))),
            ((true, Src::Spot, false), Some((400, "sla.maxCores/maxMem"))),
            (
                (true, Src::Static, true),
                Some((400, "sla.maxCores/maxMem")),
            ),
            (
                (true, Src::Static, false),
                Some((400, "sla.maxCores/maxMem")),
            ),
            ((false, Src::Static, true), None),
            ((false, Src::Static, false), None),
            ((false, Src::Spot, false), None),
            (
                (false, Src::Spot, true),
                Some((191, "derived from catalog max")),
            ),
        ];
        for ((ovr, src, nonempty), expect) in law {
            let mut cfg = base();
            cfg.hw_cost_source = src;
            if ovr {
                cfg.max_cores = Some(400.0);
                cfg.max_mem = Some(4096 << 30);
            } else {
                cfg.max_cores = None;
                cfg.max_mem = None;
            }
            let cat = if nonempty { &cat_full } else { &cat_empty };
            let got = cfg.resolve_globals(cat);
            match expect {
                Some((cores, source)) => {
                    let ((gc, _), s) = got.unwrap_or_else(|e| {
                        panic!("cell ({ovr},{src:?},{nonempty}) must resolve: {e}")
                    });
                    assert_eq!((gc, s), (cores, source), "cell ({ovr},{src:?},{nonempty})");
                }
                None => assert!(got.is_err(), "cell ({ovr},{src:?},{nonempty}) must refuse"),
            }
        }
    }

    /// The class-requirements × phantom-shapes product census
    /// [GEN-SET] (live_051(a) — the structural loose-class kill):
    /// iterates EVERY parsed shipped `sla.hwClasses` row × EVERY
    /// committed exclusion token, synthesizes a catalog entry that
    /// MATCHES the class's own requirements but carries the excluded
    /// size (premise-reachability: the phantom would win the argmax
    /// absent the exclusion), and asserts the class's EFFECTIVE
    /// matcher — requirements ∧ metal partition ∧ the committed
    /// class-wide exclusion — excludes it. Cells from the parsed
    /// values and the parsed exclusion list; a future loose class is a
    /// test failure, not a live hang.
    // r[verify scheduler.sla.ceiling.catalog-derived+4]
    // r[verify scheduler.sla.global.derive+2]
    #[test]
    fn shipped_class_requirements_exclude_every_phantom_shape() {
        let root = shipped::parse();
        let classes = shipped_hw_classes(&root.scheduler.sla);
        let metal_sizes = root.karpenter.metal_sizes.clone();
        let exclusions = root.karpenter.unlaunchable_sizes.clone();
        assert!(!exclusions.is_empty(), "committed exclusion list present");
        for (h, def) in &classes {
            // Synthesize attrs the class's own requirements admit.
            let mut category = "c".to_string();
            let mut generation = "8".to_string();
            for r in &def.requirements {
                match (r.key.as_str(), r.operator.as_str()) {
                    ("karpenter.k8s.aws/instance-category", "In") => {
                        category = r.values.first().cloned().unwrap_or(category);
                    }
                    ("karpenter.k8s.aws/instance-generation", "In") => {
                        generation = r.values.iter().max().cloned().unwrap_or(generation.clone());
                    }
                    ("karpenter.k8s.aws/instance-generation", "Gt") => {
                        let n: i64 = r.values[0].parse().unwrap();
                        generation = (n + 3).to_string();
                    }
                    _ => {}
                }
            }
            let arch = def
                .labels
                .iter()
                .find(|l| l.key == ARCH_LABEL)
                .map(|l| l.value.clone())
                .unwrap_or_else(|| "amd64".into());
            let nvme = if def
                .requirements
                .iter()
                .any(|r| r.key == "karpenter.k8s.aws/instance-local-nvme")
            {
                5700
            } else {
                0
            };
            let is_metal = def.node_class == "rio-metal";
            let control_size = if is_metal { "metal-48xl" } else { "48xlarge" };
            for excluded in &exclusions {
                let phantom =
                    shipped_cat_entry(&category, &generation, excluded, 384, 3072, &arch, nvme);
                let control =
                    shipped_cat_entry(&category, &generation, control_size, 192, 384, &arch, nvme);
                let out = super::super::catalog::derive_ceilings(
                    &[phantom, control],
                    &HashMap::from([(h.clone(), def.clone())]),
                    &metal_sizes,
                    &exclusions,
                );
                // Premise-reachability: the control row matches by
                // construction (attrs synthesized from the class's own
                // requirements), so every class MUST resolve — an
                // absent class would hide a phantom behind a vacuous
                // census row.
                let (cores, _) = *out.get(h).unwrap_or_else(|| {
                    panic!("{h} x {excluded}: control row must match the class")
                });
                assert_eq!(
                    cores, 191,
                    "{h} x {excluded}: the effective matcher excludes \
                     the phantom shape (got the control ceiling)"
                );
            }
        }
    }

    /// W7-O — the exclusion-list census [GEN-SET]: every bare-metal
    /// size token observable in the live c/m/r catalog (the
    /// enumeration committed below; generating command in the commit
    /// body) is in the SHIPPED `karpenter.metalSizes` — the
    /// author-typed partition list can no longer rot silently
    /// (live_050(d): metal-96xl's omission leaked 96xl metal rows into
    /// the band ceilings). Also pins the committed exclusion list
    /// itself (rev-3/rev-4 content).
    // r[verify scheduler.sla.ceiling.catalog-derived+4]
    #[test]
    fn shipped_metal_sizes_cover_the_catalog_enumeration() {
        let root = shipped::parse();
        // The catalog's own metal-size enumeration at this commit
        // ([GEN-SET] output; regenerate via the commit-body command
        // when AWS ships a new metal size).
        let observed = [
            "metal",
            "metal-16xl",
            "metal-24xl",
            "metal-32xl",
            "metal-48xl",
            "metal-96xl",
        ];
        for size in observed {
            assert!(
                root.karpenter.metal_sizes.contains(&size.to_string()),
                "metal size {size} missing from karpenter.metalSizes — \
                 the partition list rotted (live_050(d))"
            );
        }
        assert_eq!(
            root.karpenter.metal_sizes.len(),
            observed.len(),
            "metalSizes carries exactly the enumerated tokens — a \
             stale extra entry is also drift"
        );
        assert_eq!(
            root.karpenter.unlaunchable_sizes,
            vec!["96xlarge".to_string(), "metal-96xl".to_string()],
            "the committed launch-evidence exclusion (rev-3/rev-4 \
             content as chart defaults)"
        );
    }

    /// §13c-2 r[verify scheduler.sla.ceiling.uncatalogued-fallback+2]:
    /// `class_ceilings` is `min(catalog_or_excluded, cfg_or_global)` per
    /// axis. Empty catalog → fall to global (graceful degradation).
    /// Non-empty catalog + h missing → `(0,0)` (excluded). Catalog
    /// present → physical bound. Cfg can only TIGHTEN below catalog.
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

        // EMPTY catalog (Static cost source / describe_instance_types
        // failed at boot): fall to global on both axes (h1) or cfg
        // (h2 cores). Graceful degradation — vmtest-Static and
        // AWS-timeout boot keep the old over-permits semantics.
        let empty = super::super::catalog::CatalogCeilings::new();
        assert_eq!(
            cfg.class_ceilings("h1", &empty, (192u32, 1024u64 << 30)),
            (192, 1024 << 30),
            "empty catalog → fall to global (graceful degradation)"
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
        // sh-016 (a): NON-EMPTY catalog + h2 not in catalog → ceiling
        // (0,0). The class's `requirements` matched zero AWS instance
        // types (operator typo / nonexistent SKU like gen-7 x86 c/m/r
        // local-nvme); a (0,0) ceiling fails every size gate
        // (`solve_full`'s `c_star <= cm.0`, `retain_hosting_cells`,
        // the capacity-pin filter) — the class is structurally
        // excluded from emission. Pre-fix h2 fell to global → with
        // cheap spot price + low lead-time it became `e_min` for
        // ~every drv and the τ-band collapsed around a phantom.
        assert_eq!(
            cfg.class_ceilings("h2", &cat, (192u32, 1024u64 << 30)),
            (0, 0),
            "non-empty catalog + missing-h → (0,0); class excluded"
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
            soft_feature_sizing: _,               // (free)   key = soft-feature string (I-204 set)
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
            max_node_claims_per_cell_per_tick: _, // (scalar)
            cluster: _,            // (scalar)
            metal_sizes: _,        // (free)   instance-size suffix strings
            unlaunchable_sizes: _, // (free)   instance-size suffix strings
            compute_bound_threshold: _, // (scalar)
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

    /// bug_094 pin: the serde ATTRIBUTE literals on [`CapacityType`]
    /// (`rename_all = "lowercase"`, `alias = "on-demand"`) cannot
    /// reference consts, so this asserts their behavior agrees with
    /// the shared [`rio_common::cell_wire`] alphabet — if either side
    /// drifts, this names the law. The codec fns themselves
    /// (`label`/`parse`/`parse_cell`/`cell_label`/`cell_key_serde`)
    /// are pinned by construction (they call through `cell_wire`).
    #[test]
    fn capacity_serde_attrs_agree_with_cell_wire_alphabet() {
        use rio_common::cell_wire::{CAPACITY_OD, CAPACITY_ON_DEMAND, CAPACITY_SPOT};
        // rename_all = "lowercase" emits exactly the canonical wire
        // tokens.
        assert_eq!(
            serde_json::to_string(&CapacityType::Spot).unwrap(),
            format!("\"{CAPACITY_SPOT}\"")
        );
        assert_eq!(
            serde_json::to_string(&CapacityType::Od).unwrap(),
            format!("\"{CAPACITY_OD}\"")
        );
        // The attr-literal alias accepts the Karpenter label form —
        // the same third token `WireCapacity::parse` accepts.
        for tok in [CAPACITY_SPOT, CAPACITY_OD, CAPACITY_ON_DEMAND] {
            let via_serde: CapacityType = serde_json::from_str(&format!("\"{tok}\"")).unwrap();
            let via_alphabet = CapacityType::parse(tok).unwrap();
            assert_eq!(via_serde, via_alphabet, "token {tok:?}");
        }
    }
}
