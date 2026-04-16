//! Algorithm 2 (ADR-023 §3.2): saturation-gated exploration.
//!
//! Drives the cold-start probe ladder before a key has enough span/n_eff
//! for `solve_mvp`. Walks `c` away from `cfg.probe.cpu` — ×4 up while
//! the last build saturated AND missed its tier's p90, ÷2 down
//! otherwise — until the observed `[min_c, max_c]` span reaches 4× (the
//! fit gate), or hits a wall (`max_cores` / `1`). At that point the
//! ladder freezes and `intent_for` switches to the solve path.

use super::config::{ProbeShape, SlaConfig};
use super::metrics;
use super::solve::DrvHints;
use super::types::{DiskBytes, FittedParams, MemBytes, RawCores};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExploreDecision {
    pub c: RawCores,
    pub mem: MemBytes,
    pub disk: DiskBytes,
}

/// Next explore step. `fit=None` (never-seen key) → cold-start at the
/// probe shape (or a feature-specific override). `fit=Some` with the
/// ladder still walking → ×4 / ÷2 from the explore state. Ladder
/// frozen → re-emit `max_c` (the solve gate in `intent_for` will pick
/// `solve_mvp` instead, but a frozen-yet-immature fit — e.g. n_eff<3
/// after a version bump — falls through here and should hold position).
// r[impl sched.sla.explore-saturation-gate]
// r[impl sched.sla.explore-x4-first-bump]
// r[impl sched.sla.explore-freeze]
pub fn next(fit: Option<&FittedParams>, cfg: &SlaConfig, hints: &DrvHints) -> ExploreDecision {
    let probe = hints
        .required_features
        .iter()
        .find_map(|f| cfg.feature_probes.get(f))
        .unwrap_or(&cfg.probe);
    let mem_for = |c: f64| MemBytes((c * probe.mem_per_core as f64 + probe.mem_base as f64) as u64);
    let disk = DiskBytes(cfg.default_disk);

    let Some(f) = fit else {
        return decision(probe.cpu, mem_for, disk);
    };
    let st = &f.explore;
    // First sample landed but min/max not yet diverse → treat as cold.
    if st.max_c.0 <= 0.0 {
        return decision(probe.cpu, mem_for, disk);
    }
    if frozen(st, cfg.max_cores) {
        return decision(st.max_c.0, mem_for, disk);
    }
    let target_p90 = tier_p90(f, cfg);
    if st.saturated && st.last_wall.0 > target_p90 {
        let c_up = (st.max_c.0 * 4.0).min(cfg.max_cores);
        if st.distinct_c >= 3 && c_up >= cfg.max_cores {
            metrics::suspicious_scaling(&f.key.tenant);
        }
        decision(c_up, mem_for, disk)
    } else {
        let c_down = (st.min_c.0 / 2.0).floor().max(1.0);
        decision(c_down, mem_for, disk)
    }
}

/// Freeze predicate, shared with [`super::solve::intent_for`]'s gate so
/// "explore done" and "solve takes over" agree on the same boundary.
pub(crate) fn frozen(st: &super::types::ExploreState, max_cores: f64) -> bool {
    let span = if st.min_c.0 > 0.0 {
        st.max_c.0 / st.min_c.0
    } else {
        1.0
    };
    span >= 4.0 || st.max_c.0 >= max_cores || st.min_c.0 <= 1.0
}

fn decision(c: f64, mem_for: impl Fn(f64) -> MemBytes, disk: DiskBytes) -> ExploreDecision {
    ExploreDecision {
        c: RawCores(c),
        mem: mem_for(c),
        disk,
    }
}

/// p90 of `fit.tier` if assigned, else `cfg.default_tier`, else 1200s.
/// Uses the config-side tier list (not `solve_tiers()`) since we only
/// need a name lookup.
fn tier_p90(fit: &FittedParams, cfg: &SlaConfig) -> f64 {
    cfg.tiers
        .iter()
        .find(|t| Some(&t.name) == fit.tier.as_ref())
        .or_else(|| cfg.tiers.iter().find(|t| t.name == cfg.default_tier))
        .and_then(|t| t.p90)
        .unwrap_or(1200.0)
}

/// Probe shape for a feature set without a full config — used by the
/// `[sla]`-unconfigured fallback in `intent_for`.
pub(super) fn fallback_probe() -> ProbeShape {
    ProbeShape {
        cpu: 4.0,
        mem_per_core: 1 << 30,
        mem_base: 4 << 30,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sla::config::{ProbeShape, Tier};
    use crate::sla::types::{DurationFit, ExploreState, MemFit, ModelKey, WallSeconds};
    use std::collections::HashMap;

    fn cfg() -> SlaConfig {
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
            ring_buffer: 32,
            halflife_secs: 7.0 * 86400.0,
            seed_corpus: None,
            hw_cost_source: None,
            hw_softmax_temp: 0.3,
            hw_fallback_after_secs: 120.0,
        }
    }

    fn fit(st: ExploreState) -> FittedParams {
        FittedParams {
            key: ModelKey {
                pname: "p".into(),
                system: "x".into(),
                tenant: "t".into(),
            },
            fit: DurationFit::Probe,
            mem: MemFit::Independent { p90: MemBytes(0) },
            disk_p90: None,
            sigma_resid: 0.2,
            log_residuals: Vec::new(),
            n_eff: 1.0,
            span: 1.0,
            explore: st,
            t_min_ci: None,
            ci_computed_at: None,
            tier: None,
            hw_bias: Default::default(),
            prior_source: None,
        }
    }

    fn st(min: f64, max: f64, distinct: u8, sat: bool, wall: f64) -> ExploreState {
        ExploreState {
            distinct_c: distinct,
            min_c: RawCores(min),
            max_c: RawCores(max),
            frozen: false,
            saturated: sat,
            last_wall: WallSeconds(wall),
        }
    }

    #[test]
    fn empty_fit_returns_probe() {
        let d = next(None, &cfg(), &DrvHints::default());
        assert_eq!(d.c.0, 4.0);
        // mem = 4·2Gi + 4Gi = 12Gi
        assert_eq!(d.mem.0, 12 << 30);
        assert_eq!(d.disk.0, 20 << 30);
    }

    // r[verify sched.sla.explore-x4-first-bump]
    #[test]
    fn x4_bump_on_saturated_slow() {
        // One sample at c=4, saturated, wall=1500 > p90=1200 → bump to 16.
        let f = fit(st(4.0, 4.0, 1, true, 1500.0));
        let d = next(Some(&f), &cfg(), &DrvHints::default());
        assert_eq!(d.c.0, 16.0);
        // mem follows probe shape at the bumped c.
        assert_eq!(d.mem.0, 16 * (2 << 30) + (4 << 30));
    }

    // r[verify sched.sla.explore-saturation-gate]
    #[test]
    fn halve_on_unsaturated() {
        // c=8, NOT saturated → halve to 4.
        let f = fit(st(8.0, 8.0, 1, false, 1500.0));
        assert_eq!(next(Some(&f), &cfg(), &DrvHints::default()).c.0, 4.0);
        // Saturated but FAST (wall<p90) → also halve: it hit target with
        // headroom, so probe smaller to find the floor.
        let f = fit(st(8.0, 8.0, 1, true, 600.0));
        assert_eq!(next(Some(&f), &cfg(), &DrvHints::default()).c.0, 4.0);
    }

    // r[verify sched.sla.explore-freeze]
    #[test]
    fn freeze_at_span4() {
        // span = 16/4 = 4 → frozen, re-emit max_c.
        let f = fit(st(4.0, 16.0, 2, true, 1500.0));
        assert_eq!(next(Some(&f), &cfg(), &DrvHints::default()).c.0, 16.0);
        // max_c at ceiling → frozen.
        let f = fit(st(32.0, 64.0, 2, true, 1500.0));
        assert_eq!(next(Some(&f), &cfg(), &DrvHints::default()).c.0, 64.0);
        // min_c hit floor → frozen.
        let f = fit(st(1.0, 2.0, 2, false, 100.0));
        assert_eq!(next(Some(&f), &cfg(), &DrvHints::default()).c.0, 2.0);
    }

    #[test]
    fn x4_clamps_at_max_cores() {
        // c=32, saturated+slow → ×4=128, clamped to 64.
        let f = fit(st(32.0, 32.0, 1, true, 1500.0));
        assert_eq!(next(Some(&f), &cfg(), &DrvHints::default()).c.0, 64.0);
    }

    #[test]
    fn halve_floors_at_1() {
        let f = fit(st(2.0, 3.0, 2, false, 100.0));
        assert_eq!(next(Some(&f), &cfg(), &DrvHints::default()).c.0, 1.0);
    }

    #[test]
    fn feature_probe_overrides_default() {
        let mut c = cfg();
        c.feature_probes.insert(
            "kvm".into(),
            ProbeShape {
                cpu: 8.0,
                mem_per_core: 4 << 30,
                mem_base: 16 << 30,
            },
        );
        let hints = DrvHints {
            required_features: vec!["kvm".into()],
            ..Default::default()
        };
        let d = next(None, &c, &hints);
        assert_eq!(d.c.0, 8.0);
        assert_eq!(d.mem.0, 8 * (4 << 30) + (16 << 30));
        // Feature not in feature_probes → fall back to default probe.
        let hints = DrvHints {
            required_features: vec!["big-parallel".into()],
            ..Default::default()
        };
        assert_eq!(next(None, &c, &hints).c.0, 4.0);
    }
}
