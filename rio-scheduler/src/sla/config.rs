//! ADR-023 operator-facing SLA config: tier ladder, cold-start probe
//! shapes, hard ceilings. Loaded from `[sla]` in `scheduler.toml` (helm
//! `scheduler.sla`). Mandatory — every deployment carries an `[sla]`
//! block (helm renders it from chart defaults; tests use
//! [`SlaConfig::test_default`]).

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::solve::{Ceilings, Tier};

/// `[sla]` table. `deny_unknown_fields` so a typo'd key under `[sla]`
/// fails loud at startup instead of silently defaulting.
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
    /// Softmax temperature for the per-`(band, cap)` cost pick.
    /// Lower → greedier (always cheapest); higher → more spread. 0.3
    /// gives ~85% mass on the cheapest when the runner-up is 1.5× the
    /// price.
    #[serde(default = "default_hw_softmax_temp")]
    pub hw_softmax_temp: f64,
    /// Seconds a spawned pod may sit Pending before its `(band, cap)`
    /// is ICE-backed-off and the build re-solved excluding it.
    #[serde(default = "default_hw_fallback_after_secs")]
    pub hw_fallback_after_secs: f64,
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

fn default_hw_softmax_temp() -> f64 {
    0.3
}
fn default_hw_fallback_after_secs() -> f64 {
    120.0
}

fn default_ring_buffer() -> u32 {
    32
}
fn default_halflife() -> f64 {
    7.0 * 86400.0
}

/// Cold-start probe shape: `mem = mem_base + cpu × mem_per_core`.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
                cpu: 1.0,
                mem_per_core: 1 << 30,
                mem_base: 1 << 30,
                deadline_secs: default_probe_deadline_secs(),
            },
            feature_probes: HashMap::new(),
            max_cores: 2.0,
            max_mem: 2 << 30,
            max_disk: 6 << 30,
            default_disk: 2 << 30,
            ring_buffer: default_ring_buffer(),
            halflife_secs: default_halflife(),
            seed_corpus: None,
            hw_cost_source: None,
            hw_softmax_temp: default_hw_softmax_temp(),
            hw_fallback_after_secs: default_hw_fallback_after_secs(),
            cluster: String::new(),
        }
    }

    /// Startup-time bounds checks. `&self` (not `&mut`) so it composes
    /// with [`rio_common::config::ValidateConfig::validate`]; sorting
    /// is provided separately by [`Self::solve_tiers`].
    ///
    /// `probe.cpu ∈ [1, max_cores]`: was `[4, max_cores/4]` to give the
    /// explore walk span≥4 on both halve and ×4 sides, but that floor
    /// blocks VM-test pools where `max_cores < 16` from booting at all.
    /// A degenerate-span `p̄` fit is recoverable (next sample fixes it);
    /// a config that won't load is not.
    pub fn validate(&self) -> anyhow::Result<()> {
        let hi = self.max_cores;
        anyhow::ensure!(
            self.probe.cpu >= 1.0 && self.probe.cpu <= hi,
            "sla.probe.cpu must be in [1, max_cores={hi}]; got {}",
            self.probe.cpu
        );
        anyhow::ensure!(
            self.tiers.iter().any(|t| t.name == self.default_tier),
            "sla.default_tier {:?} not in sla.tiers (known: {:?})",
            self.default_tier,
            self.tiers.iter().map(|t| &t.name).collect::<Vec<_>>()
        );
        anyhow::ensure!(
            self.max_cores.is_finite() && self.max_cores > 0.0,
            "sla.max_cores must be finite and positive, got {}",
            self.max_cores
        );
        anyhow::ensure!(
            self.halflife_secs.is_finite() && self.halflife_secs > 0.0,
            "sla.halflife_secs must be finite and positive, got {}",
            self.halflife_secs
        );
        Ok(())
    }

    /// Tiers sorted tightest-first (lowest target wins; a tier with no
    /// targets sorts last). [`super::solve::solve_mvp`] iterates in
    /// order and returns the first feasible tier, so tightest-first
    /// means a build that CAN hit `fast` does, instead of settling for
    /// `normal`.
    pub fn solve_tiers(&self) -> Vec<Tier> {
        let mut tiers = self.tiers.clone();
        tiers.sort_by_key(|t| {
            t.p50
                .or(t.p90)
                .or(t.p99)
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
            default_tier: "normal".into(),
            probe: ProbeShape {
                cpu: 4.0,
                mem_per_core: 2 << 30,
                mem_base: 4 << 30,
                deadline_secs: default_probe_deadline_secs(),
            },
            feature_probes: HashMap::new(),
            max_cores: 64.0,
            max_mem: 256 << 30,
            max_disk: 200 << 30,
            default_disk: 20 << 30,
            ring_buffer: default_ring_buffer(),
            halflife_secs: default_halflife(),
            seed_corpus: None,
            hw_cost_source: None,
            hw_softmax_temp: default_hw_softmax_temp(),
            hw_fallback_after_secs: default_hw_fallback_after_secs(),
            cluster: String::new(),
        }
    }

    #[test]
    fn rejects_probe_cpu_gt_maxcores() {
        let mut cfg = base();
        cfg.probe.cpu = 96.0; // max_cores=64
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("[1, max_cores=64]"), "{err}");
    }

    #[test]
    fn rejects_probe_cpu_lt_1() {
        let mut cfg = base();
        cfg.probe.cpu = 0.5;
        assert!(cfg.validate().is_err());
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
        cfg.probe.cpu = 1.0;
        cfg.validate().unwrap();
        cfg.probe.cpu = 64.0; // = max_cores
        cfg.validate().unwrap();
    }

    #[test]
    fn accepts_tiny_pool() {
        // pre-relaxation this was unrepresentable: max_cores=8 →
        // [4, 2] = ∅. VM-test pools live here.
        let mut cfg = base();
        cfg.max_cores = 8.0;
        cfg.probe.cpu = 2.0;
        cfg.validate().unwrap();
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
}
