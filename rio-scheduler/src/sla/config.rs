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
/// One hw-class: a node-label conjunction. Labels are ANDed within a
/// class; classes are OR'd across the `hw_classes` map when serialized
/// to `nodeSelectorTerms`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HwClassDef {
    pub labels: Vec<NodeLabelMatch>,
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
    /// EWMA half-life for sample ageing. Feeds
    /// [`super::SlaEstimator::new`].
    #[serde(default = "default_halflife")]
    pub halflife_secs: f64,
    /// JSON [`super::prior::SeedCorpus`] loaded at startup into the
    /// seed-prior table. ADR-023 §2.10: lets a fresh deployment skip
    /// the cold-start probe ladder for known pnames. Unset → seed table
    /// starts empty (still fillable via `ImportSlaCorpus`).
    #[serde(default)]
    pub seed_corpus: Option<PathBuf>,
    /// Phase-13 hw-band cost source. `None` → cost-ranking disabled
    /// (`solve_full` falls back to `solve_mvp`'s band-agnostic path).
    /// `Some(Static)` → seed prices only; `Some(Spot)` → live EC2
    /// spot-price poll (lease-gated).
    #[serde(default)]
    pub hw_cost_source: Option<super::cost::HwCostSource>,
    /// h → node-label conjunction. Empty = static-mode (admissible-set
    /// solve unreachable, every dispatch falls through to MVP solve).
    #[serde(default)]
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
    /// Immutable across reloads unless `--allow-reference-change` is
    /// passed — see [`SlaConfig::validate_reload`]. MUST appear in
    /// `hw_classes` when set.
    #[serde(default)]
    pub reference_hw_class: Option<HwClassName>,
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
    /// Part-B: `EC2NodeClass` name the NodeClaim reconciler stamps.
    #[serde(default)]
    pub node_class_ref: Option<String>,
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
fn default_max_lead_time() -> f64 {
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
fn default_halflife() -> f64 {
    7.0 * 86400.0
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
            halflife_secs: default_halflife(),
            seed_corpus: None,
            hw_cost_source: None,
            hw_classes: HashMap::new(),
            hw_cost_tolerance: default_hw_cost_tolerance(),
            hw_explore_epsilon: default_hw_explore_epsilon(),
            hw_bench_mem_floor: default_hw_bench_mem_floor(),
            lead_time_seed: HashMap::new(),
            max_fleet_cores: default_max_fleet_cores(),
            ladder_budget: default_ladder_budget(),
            reference_hw_class: None,
            max_forecast_cores_per_tenant: default_max_forecast_cores_per_tenant(),
            max_keys_per_tenant: default_max_keys_per_tenant(),
            max_lead_time: default_max_lead_time(),
            max_consolidation_time: None,
            consolidate_explore_epsilon: default_consolidate_explore_epsilon(),
            max_node_claims_per_cell_per_tick: default_max_node_claims_per_cell_per_tick(),
            node_class_ref: None,
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
            self.halflife_secs.is_finite() && self.halflife_secs > 0.0,
            "sla.halflife_secs must be finite and positive, got {}",
            self.halflife_secs
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
            anyhow::ensure!(
                !def.labels.is_empty(),
                "sla.hwClasses[{h}].labels must be non-empty"
            );
        }
        if let Some(ref_h) = &self.reference_hw_class {
            anyhow::ensure!(
                self.hw_classes.contains_key(ref_h),
                "sla.referenceHwClass={ref_h} not in sla.hwClasses"
            );
        }
        self.probe.validate("sla.probe", hi)?;
        for (feat, p) in &self.feature_probes {
            p.validate(&format!("sla.feature_probes[{feat}]"), hi)?;
        }
        Ok(())
    }

    /// Called on SIGHUP config-reload. `allow_ref_change` from CLI
    /// flag. Runs [`Self::validate`] then rejects a
    /// `reference_hw_class` change unless explicitly allowed —
    /// changing the reference invalidates every stored ref-second
    /// (corpus, EMA state, ring buffers) so it MUST be paired with
    /// `rio-cli sla reset --all` + corpus re-import.
    pub fn validate_reload(&self, prev: &Self, allow_ref_change: bool) -> Result<(), String> {
        self.validate().map_err(|e| e.to_string())?;
        if self.reference_hw_class != prev.reference_hw_class && !allow_ref_change {
            return Err(format!(
                "sla.referenceHwClass changed {:?}→{:?}; pass --allow-reference-change \
                 AND run `rio-cli sla reset --all` + corpus re-import (all ref-seconds invalidated)",
                prev.reference_hw_class, self.reference_hw_class
            ));
        }
        Ok(())
    }

    /// Tiers sorted tightest-first (lowest target wins; a tier with no
    /// targets sorts last). [`super::solve::solve_mvp`] iterates in
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

    /// Hard ceilings for [`super::solve::solve_mvp`].
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

    // r[verify sched.sla.hw-class.config]
    #[test]
    fn rejects_reference_hw_class_change_without_flag() {
        let mut cfg = base();
        cfg.hw_classes.insert(
            "intel-7-ebs".into(),
            HwClassDef {
                labels: vec![NodeLabelMatch {
                    key: "k".into(),
                    value: "v".into(),
                }],
            },
        );
        cfg.reference_hw_class = Some("intel-7-ebs".into());
        let prev = SlaConfig {
            reference_hw_class: Some("amd-8-ebs".into()),
            ..cfg.clone()
        };
        let err = cfg.validate_reload(&prev, false).unwrap_err();
        assert!(err.contains("--allow-reference-change"), "{err}");
        assert!(cfg.validate_reload(&prev, true).is_ok());
        // Same ref → no flag needed.
        assert!(cfg.validate_reload(&cfg.clone(), false).is_ok());
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
        cfg.reference_hw_class = Some("nope".into());
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("not in sla.hwClasses"), "{err}");
    }

    #[test]
    fn rejects_empty_hw_class_labels() {
        let mut cfg = base();
        cfg.hw_classes
            .insert("h".into(), HwClassDef { labels: vec![] });
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("hwClasses[h].labels"), "{err}");
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
