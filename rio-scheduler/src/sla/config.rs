//! ADR-023 operator-facing SLA config: tier ladder, cold-start probe
//! shapes, hard ceilings. Loaded from `[sla]` in `scheduler.toml` (helm
//! `scheduler.sla`). Absent → SLA-mode disabled (Phase-2 const fallback
//! path stays active).
//!
//! [`Tier`] is intentionally a config-local mirror of
//! [`super::solve::Tier`]: solve.rs is being reworked concurrently
//! (Phase-2.7) so this module keeps its own serde-carrying copy and
//! converts at the boundary via [`SlaConfig::solve_tiers`]. Once both
//! land the two collapse into one.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::solve::Ceilings;

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
    /// Slice of the per-pod ephemeral-storage budget reserved for the
    /// FUSE page cache (rio-mountd warm path).
    pub fuse_cache_budget: u64,
    /// Slice reserved for build-log ring buffer.
    pub log_budget: u64,
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

/// Config-side tier descriptor. Mirrors [`super::solve::Tier`] but
/// carries serde derives + per-field `#[serde(default)]` so a tier
/// like `{name: best-effort}` (no targets) deserializes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tier {
    pub name: String,
    #[serde(default)]
    pub p50: Option<f64>,
    #[serde(default)]
    pub p90: Option<f64>,
    #[serde(default)]
    pub p99: Option<f64>,
}

impl From<&Tier> for super::solve::Tier {
    fn from(t: &Tier) -> Self {
        Self {
            name: t.name.clone(),
            p50: t.p50,
            p90: t.p90,
            p99: t.p99,
        }
    }
}

/// Cold-start probe shape: `mem = mem_base + cpu × mem_per_core`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeShape {
    pub cpu: f64,
    pub mem_per_core: u64,
    pub mem_base: u64,
}

impl SlaConfig {
    /// Startup-time bounds checks. `&self` (not `&mut`) so it composes
    /// with [`rio_common::config::ValidateConfig::validate`]; sorting
    /// is provided separately by [`Self::solve_tiers`].
    ///
    /// `probe.cpu ∈ [4, max_cores/4]`: the explore walk both ×4-bumps
    /// and halves from the probe point; with span < 4 on either side
    /// the fit's `p̄` estimate is garbage (single-point regression).
    /// `probe.cpu = 4` gives halve-room down to 1; `= max_cores/4`
    /// gives ×4-room up to `max_cores`.
    pub fn validate(&self) -> anyhow::Result<()> {
        let hi = self.max_cores / 4.0;
        anyhow::ensure!(
            self.probe.cpu >= 4.0 && self.probe.cpu <= hi,
            "sla.probe.cpu must be in [4, max_cores/4={hi}] so both x4-bump \
             and halve paths reach span>=4; got {}",
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

    /// Tiers converted to [`super::solve::Tier`] and sorted
    /// tightest-first (lowest target wins; a tier with no targets sorts
    /// last). [`super::solve::solve_mvp`] iterates in order and returns
    /// the first feasible tier, so tightest-first means a build that
    /// CAN hit `fast` does, instead of settling for `normal`.
    pub fn solve_tiers(&self) -> Vec<super::solve::Tier> {
        let mut tiers: Vec<super::solve::Tier> = self.tiers.iter().map(Into::into).collect();
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
            },
            feature_probes: HashMap::new(),
            max_cores: 64.0,
            max_mem: 256 << 30,
            max_disk: 200 << 30,
            default_disk: 20 << 30,
            fuse_cache_budget: 8 << 30,
            log_budget: 1 << 30,
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
    fn rejects_probe_cpu_gt_maxcores_div4() {
        let mut cfg = base();
        cfg.probe.cpu = 32.0; // max_cores=64 → hi=16
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("[4, max_cores/4=16]"), "{err}");
    }

    #[test]
    fn rejects_probe_cpu_lt_4() {
        let mut cfg = base();
        cfg.probe.cpu = 2.0;
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
        cfg.probe.cpu = 4.0;
        cfg.validate().unwrap();
        cfg.probe.cpu = 16.0; // = 64/4
        cfg.validate().unwrap();
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
