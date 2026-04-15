use super::config::SlaConfig;
use super::explore;
use super::fit::headroom;
use super::r#override::ResolvedTarget;
use super::quantile;
use super::types::{DiskBytes, FittedParams, MemBytes, RawCores};

/// Fallback ceilings when `[sla]` is unconfigured. The actor stores
/// `cfg.sla.ceilings()` (or this) once at construction so dispatch and
/// the snapshot read the same struct.
pub const DEFAULT_CEILINGS: Ceilings = Ceilings {
    max_cores: 64.0,
    max_mem: 256 << 30,
    max_disk: 200 << 30,
    default_disk: 20 << 30,
};

/// `prefer_local_build = true` → trivially short. Smallest pod the
/// controller will spawn.
const LOCAL_CORES: u32 = 1;
const LOCAL_MEM_BYTES: u64 = 2 << 30;

#[derive(Debug, Clone)]
pub struct Tier {
    pub name: String,
    pub p50: Option<f64>,
    pub p90: Option<f64>,
    pub p99: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct Ceilings {
    pub max_cores: f64,
    pub max_mem: u64,
    pub max_disk: u64,
    pub default_disk: u64,
}

#[derive(Debug, Clone)]
pub enum SolveResult {
    Feasible {
        tier: String,
        c_star: RawCores,
        mem: MemBytes,
        disk: DiskBytes,
    },
    BestEffort {
        c: RawCores,
        mem: MemBytes,
        disk: DiskBytes,
    },
}

impl SolveResult {
    pub fn cores(&self) -> RawCores {
        match self {
            Self::Feasible { c_star, .. } => *c_star,
            Self::BestEffort { c, .. } => *c,
        }
    }
    pub fn mem(&self) -> MemBytes {
        match self {
            Self::Feasible { mem, .. } | Self::BestEffort { mem, .. } => *mem,
        }
    }
    pub fn disk(&self) -> DiskBytes {
        match self {
            Self::Feasible { disk, .. } | Self::BestEffort { disk, .. } => *disk,
        }
    }
}

/// Dispatch-time prediction snapshot. Stored on
/// [`SchedHint::sla_predicted`] when the derivation is dispatched so
/// completion can score actual-vs-predicted without re-reading the
/// estimator (which may have refit on a different curve by then).
///
/// [`SchedHint::sla_predicted`]: crate::state::SchedHint::sla_predicted
#[derive(Debug, Clone, Default)]
pub struct SlaPrediction {
    /// `T(c)` at the dispatched core count. `None` for cold-start /
    /// override / drv-hint paths where there is no fitted curve.
    pub wall_secs: Option<f64>,
    /// `M(c)` (post-headroom) the controller was asked to reserve.
    pub mem_bytes: u64,
    /// Tier the solve picked (`SolveResult::Feasible.tier`). `None` for
    /// `BestEffort` / probe / override.
    pub tier: Option<String>,
    /// p90 target of `tier` at dispatch time. Captured here so
    /// completion's hit/miss check is robust to the tier ladder being
    /// reconfigured mid-build.
    pub tier_p90: Option<f64>,
}

/// Derivation-declared sizing hints (from drv.env / drv.system-features).
#[derive(Debug, Default, Clone)]
pub struct DrvHints {
    pub enable_parallel_building: Option<bool>,
    pub prefer_local_build: Option<bool>,
    /// `requiredSystemFeatures` — drives [`super::explore::next`]'s
    /// per-feature probe-shape override (e.g. `kvm` builds want a high
    /// mem floor regardless of core count).
    pub required_features: Vec<String>,
}

/// Per-derivation `(cores, mem, disk)` for a [`SpawnIntent`]. Wraps
/// [`solve_mvp`] with the cold-start / drv-hint branching that the
/// snapshot path and dispatch's resource-fit filter both need, so the
/// two cannot diverge.
///
/// - `prefer_local_build = Some(true)` → minimal (1 core), no learning.
/// - `enable_parallel_building = Some(false)` → 1 core, probe mem/disk
///   (the build is serial by declaration; exploring c is wasted).
/// - No fit / `n_eff < 3` / `span < 4 ∧ !frozen` → [`explore::next`]
///   drives the saturation-gated ×4/÷2 ladder from `cfg.probe`.
/// - Otherwise → `solve_mvp` against `tiers` / `ceil`. Empty `tiers`
///   (`[sla]` unconfigured) → `solve_mvp` returns `BestEffort` at
///   `min(p̄, max_cores)`, so unconfigured deployments still get the
///   fitted-curve cap instead of a hardcoded tier target.
///
/// `cfg=None` (`[sla]` absent) keeps the explore branch on a fixed
/// fallback probe — the actor still constructs a `Ceilings` from
/// [`DEFAULT_CEILINGS`] so the solve branch is unaffected.
///
/// Cores are `ceil`-ed to a whole-core `u32` (k8s `resources.requests`
/// granularity); mem/disk are byte-exact (controller rounds at the pod
/// template).
///
/// `override_` (from [`SlaEstimator::resolved_override`]) is consulted
/// FIRST: `forced_cores`/`forced_mem` short-circuit the model entirely
/// — the operator pinned this key, learning is suspended. A pin with
/// only `tier`/`capacity` set falls through to the normal branch (the
/// tier filter happens at solve, not here; phase-7 wires it).
///
/// [`SpawnIntent`]: rio_proto::types::SpawnIntent
/// [`SlaEstimator::resolved_override`]: super::SlaEstimator::resolved_override
// r[impl sched.sla.intent-from-solve]
pub fn intent_for(
    fit: Option<&FittedParams>,
    hints: &DrvHints,
    override_: Option<&ResolvedTarget>,
    cfg: Option<&SlaConfig>,
    tiers: &[Tier],
    ceil: &Ceilings,
) -> (u32, u64, u64) {
    // Operator override beats everything — including drv-declared hints.
    // If the operator forced 12 cores on a `prefer_local_build` drv,
    // they had a reason. Mem unset → fall back to the fit's M(c) at the
    // forced core count (or probe mem if no fit).
    if let Some(o) = override_
        && let Some(c) = o.forced_cores
    {
        let cores = (c.ceil() as u32).max(1);
        let mem = o.forced_mem.unwrap_or_else(|| {
            fit.map(|f| f.mem.at(RawCores(c)).0).unwrap_or_else(|| {
                cfg.map(|s| {
                    (s.probe.cpu * s.probe.mem_per_core as f64 + s.probe.mem_base as f64) as u64
                })
                .unwrap_or(8 << 30)
            })
        });
        let disk = fit
            .and_then(|f| f.disk_p90)
            .map_or(ceil.default_disk, |d| d.0);
        return (cores, mem, disk);
    }
    if hints.prefer_local_build == Some(true) {
        return (LOCAL_CORES, LOCAL_MEM_BYTES, ceil.default_disk);
    }
    let probe_mem = cfg
        .map(|c| (c.probe.cpu * c.probe.mem_per_core as f64 + c.probe.mem_base as f64) as u64)
        .unwrap_or(8 << 30);
    if hints.enable_parallel_building == Some(false) {
        return (1, probe_mem, ceil.default_disk);
    }
    let fit = match fit {
        Some(f)
            if f.n_eff >= 3.0 && (f.span >= 4.0 || explore::frozen(&f.explore, ceil.max_cores)) =>
        {
            f
        }
        _ => {
            // Explore ladder. With `[sla]` unconfigured, fall back to a
            // fixed probe shape so the snapshot path stays deterministic.
            return match cfg {
                Some(cfg) => {
                    let d = explore::next(fit, cfg, hints);
                    ((d.c.0.ceil() as u32).max(1), d.mem.0, d.disk.0)
                }
                None => {
                    let p = explore::fallback_probe();
                    (p.cpu as u32, probe_mem, ceil.default_disk)
                }
            };
        }
    };
    let r = solve_mvp(fit, tiers, ceil);
    // ceil + clamp ≥1: solve_mvp's c_star is already ≥1.0 but
    // BestEffort.c can be fractional below 1 for degenerate fits.
    let cores = (r.cores().0.ceil() as u32).max(1);
    (cores, r.mem().0, r.disk().0)
}

// r[impl sched.sla.tier-envelope]
/// Min `c ∈ [c_lo, cap_c]` satisfying `∀(q,bound)∈tier:
/// quantile(q; T(c)/hw_factor, σ, p(c)) ≤ bound`, where
/// `p(c) = 1-exp(-λ·T(c))` is the per-attempt preemption probability
/// under Poisson rate `λ`. Returns `None` if infeasible at `cap_c`.
///
/// `feasible(c)` is monotone on `[c_lo, cap_c]` because callers pass
/// `cap_c ≤ min(p̄, c_opt)` where `T(c)` is decreasing — so plain
/// bisection finds the boundary.
pub fn solve_envelope(
    fit: &FittedParams,
    tier: &Tier,
    c_lo: f64,
    cap_c: f64,
    hw_factor: f64,
    lambda: f64,
) -> Option<RawCores> {
    let bounds: Vec<(f64, f64)> = [(0.5, tier.p50), (0.9, tier.p90), (0.99, tier.p99)]
        .into_iter()
        .filter_map(|(q, b)| b.map(|b| (q, b)))
        .collect();
    if bounds.is_empty() {
        return Some(RawCores(c_lo)); // best-effort tier
    }
    let feasible = |c: f64| -> bool {
        let t = fit.fit.t_at(RawCores(c)).0 / hw_factor;
        let p = if lambda > 0.0 {
            1.0 - (-lambda * t).exp()
        } else {
            0.0
        };
        if p > 0.5 {
            return false;
        }
        bounds
            .iter()
            .all(|&(q, bound)| quantile::quantile(q, t, fit.sigma_resid, p) <= bound)
    };
    if !feasible(cap_c) {
        return None;
    }
    if feasible(c_lo) {
        return Some(RawCores(c_lo.ceil()));
    }
    // Invariant: lo infeasible, hi feasible.
    let (mut lo, mut hi) = (c_lo, cap_c);
    while hi - lo > 0.5 {
        let mid = (lo + hi) / 2.0;
        if feasible(mid) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    Some(RawCores(hi.ceil()))
}

// r[impl sched.sla.solve-citardauq]
// r[impl sched.sla.solve-reject-not-clamp]
pub fn solve_mvp(fit: &FittedParams, tiers: &[Tier], ceil: &Ceilings) -> SolveResult {
    let cap_c = fit.fit.p_bar().0.min(fit.fit.c_opt().0).min(ceil.max_cores);
    let h = headroom(fit.n_eff);
    let disk = fit.disk_p90.map(|d| d.0).unwrap_or(ceil.default_disk);

    for tier in tiers {
        // hw_factor=1.0 / lambda=0.0: phase-13 wires the per-hw_class
        // factor and per-capacity-type preemption rate; until then the
        // envelope solve degenerates to the scalar-p90 closed form ±1
        // core when only `p90` is set.
        let Some(c_star) = solve_envelope(fit, tier, 1.0, cap_c, 1.0, 0.0) else {
            continue; // REJECT (not clamp)
        };
        let mem = (fit.mem.at(c_star).0 as f64 * h) as u64;
        if mem > ceil.max_mem {
            continue;
        }
        if disk > ceil.max_disk {
            continue;
        }
        return SolveResult::Feasible {
            tier: tier.name.clone(),
            c_star,
            mem: MemBytes(mem),
            disk: DiskBytes(disk),
        };
    }
    let c = fit.fit.p_bar().0.min(ceil.max_cores);
    let mem = (fit.mem.at(RawCores(c)).0 as f64 * h) as u64;
    SolveResult::BestEffort {
        c: RawCores(c),
        mem: MemBytes(mem),
        disk: DiskBytes(disk),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sla::types::*;

    fn mk_fit(s: f64, p: f64, q: f64, p_bar: f64, sigma: f64) -> FittedParams {
        FittedParams {
            key: ModelKey {
                pname: "x".into(),
                system: "x".into(),
                tenant: "x".into(),
            },
            fit: if q > 0.0 {
                DurationFit::Usl {
                    s: RefSeconds(s),
                    p: RefSeconds(p),
                    q,
                    p_bar: RawCores(p_bar),
                }
            } else if p_bar.is_finite() {
                DurationFit::Capped {
                    s: RefSeconds(s),
                    p: RefSeconds(p),
                    p_bar: RawCores(p_bar),
                }
            } else {
                DurationFit::Amdahl {
                    s: RefSeconds(s),
                    p: RefSeconds(p),
                }
            },
            mem: MemFit::Independent {
                p90: MemBytes(2 << 30),
            },
            disk_p90: Some(DiskBytes(10 << 30)),
            sigma_resid: sigma,
            log_residuals: Vec::new(),
            n_eff: 10.0,
            span: 8.0,
            explore: ExploreState {
                distinct_c: 3,
                min_c: RawCores(4.0),
                max_c: RawCores(32.0),
                frozen: false,
                saturated: false,
                last_wall: WallSeconds(0.0),
            },
            t_min_ci: None,
            ci_computed_at: None,
            tier: None,
            hw_bias: Default::default(),
            prior_source: None,
        }
    }
    fn t(name: &str, p90: f64) -> Tier {
        Tier {
            name: name.into(),
            p50: None,
            p90: Some(p90),
            p99: None,
        }
    }
    fn t3(p50: f64, p90: f64, p99: f64) -> Tier {
        Tier {
            name: "x".into(),
            p50: Some(p50),
            p90: Some(p90),
            p99: Some(p99),
        }
    }
    fn ceil() -> Ceilings {
        Ceilings {
            max_cores: 64.0,
            max_mem: 256 << 30,
            max_disk: 200 << 30,
            default_disk: 20 << 30,
        }
    }

    #[test]
    fn envelope_degenerates_to_amdahl_at_q0() {
        let fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
        let SolveResult::Feasible { c_star, tier, .. } =
            solve_mvp(&fit, &[t("t0", 300.0)], &ceil())
        else {
            panic!()
        };
        // β=30-e^{-0.128}·300≈-234 → c*=2000/234≈8.54 → ceil 9
        assert_eq!(c_star.0, 9.0, "ceil(8.54)");
        assert_eq!(tier, "t0");
    }
    #[test]
    fn rejects_not_clamps_when_cstar_exceeds_cap() {
        // r[verify sched.sla.solve-reject-not-clamp]
        let fit = mk_fit(30.0, 100.0, 0.0, 4.0, 0.1); // p̄=4 caps it
        let SolveResult::Feasible { tier, .. } =
            solve_mvp(&fit, &[t("t0", 60.0), t("t1", 600.0)], &ceil())
        else {
            panic!()
        };
        assert_eq!(tier, "t1"); // t0 needs >4 cores → rejected, lands t1
    }
    #[test]
    fn discriminant_negative_rejects_tier() {
        let fit = mk_fit(30.0, 2000.0, 5.0, f64::INFINITY, 0.1);
        assert!(matches!(
            solve_mvp(&fit, &[t("t0", 50.0)], &ceil()),
            SolveResult::BestEffort { .. }
        ));
    }
    // r[verify sched.sla.tier-envelope]
    #[test]
    fn envelope_tight_p99_forces_higher_c() {
        // p90=300 alone (σ=0.2): T·e^{0.2·1.2816}≤300 → T≤232.2 → c*≈9.89.
        // Adding p99=320:       T·e^{0.2·2.3263}≤320 → T≤200.9 → c*≈11.7.
        // (p50=250 is slack: T≤250 → c*≈9.1.) Bisection halts at
        // hi-lo<0.5 then ceils, so allow ±1 around the analytic
        // boundary; the load-bearing property is the inequality.
        let fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.2);
        let c_p90 = solve_envelope(&fit, &t("x", 300.0), 1.0, 64.0, 1.0, 0.0).unwrap();
        let c_full = solve_envelope(&fit, &t3(250.0, 300.0, 320.0), 1.0, 64.0, 1.0, 0.0).unwrap();
        assert!((10.0..=11.0).contains(&c_p90.0), "p90-only c*={}", c_p90.0);
        assert!((12.0..=13.0).contains(&c_full.0), "full c*={}", c_full.0);
        assert!(
            c_full.0 > c_p90.0,
            "p99 must bind: full={} p90-only={}",
            c_full.0,
            c_p90.0
        );
    }
    #[test]
    fn envelope_loose_p99_permits_spot() {
        // λ=0.002 (spot preemption): at c=14, T≈173 → p≈0.29; geometric
        // tail inflates p99. Loose p99=2000 absorbs it; tight p99 doesn't.
        let fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.2);
        let loose = solve_envelope(&fit, &t3(250.0, 300.0, 2000.0), 1.0, 64.0, 1.0, 0.002);
        let tight = solve_envelope(&fit, &t3(250.0, 300.0, 320.0), 1.0, 64.0, 1.0, 0.002);
        assert!(loose.is_some(), "loose p99 should be feasible under spot λ");
        assert!(
            tight.is_none() || tight.unwrap().0 > loose.unwrap().0,
            "tight p99 must reject or demand more cores: tight={tight:?} loose={loose:?}"
        );
    }
    #[test]
    fn envelope_degenerate_single_bound_matches_scalar_within_1core() {
        // tier {p90:300} only, σ=0.1, q=0, λ=0 → must agree with the
        // closed-form citardauq scalar solve to ±1 core.
        let fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
        let env = solve_envelope(&fit, &t("x", 300.0), 1.0, 64.0, 1.0, 0.0).unwrap();
        // Scalar citardauq: β = S - e^{-1.2816σ}·p90, c* = P / -β
        let beta = 30.0 - (-1.2816f64 * 0.1).exp() * 300.0;
        let scalar = 2000.0 / -beta;
        assert!(
            (env.0 - scalar).abs() <= 1.0,
            "envelope={} scalar={scalar:.2}",
            env.0
        );
    }
    #[test]
    fn envelope_infeasible_at_cap_is_none() {
        let fit = mk_fit(400.0, 100.0, 0.0, f64::INFINITY, 0.1);
        assert!(solve_envelope(&fit, &t("x", 300.0), 1.0, 64.0, 1.0, 0.0).is_none());
    }
    #[test]
    fn envelope_no_bounds_is_best_effort_floor() {
        let fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
        let tier = Tier {
            name: "be".into(),
            p50: None,
            p90: None,
            p99: None,
        };
        assert_eq!(
            solve_envelope(&fit, &tier, 1.0, 64.0, 1.0, 0.0).unwrap().0,
            1.0
        );
    }

    #[test]
    fn beta_nonneg_rejects_tier() {
        // S=400 > target → infeasible
        let fit = mk_fit(400.0, 100.0, 0.0, f64::INFINITY, 0.1);
        assert!(matches!(
            solve_mvp(&fit, &[t("t0", 300.0)], &ceil()),
            SolveResult::BestEffort { .. }
        ));
    }

    // ─── intent_for branching ───────────────────────────────────────

    fn intent(fit: Option<&FittedParams>, hints: &DrvHints) -> (u32, u64, u64) {
        intent_for(fit, hints, None, None, &[t("normal", 1200.0)], &ceil())
    }

    // r[verify sched.sla.override-precedence]
    #[test]
    fn override_forced_cores_bypasses_fit() {
        // Fitted curve says ~2 cores against p90=1200; override forces 12.
        let fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
        let o = ResolvedTarget {
            forced_cores: Some(12.0),
            forced_mem: Some(32 << 30),
            ..Default::default()
        };
        let (c, m, _) = intent_for(
            Some(&fit),
            &DrvHints::default(),
            Some(&o),
            None,
            &[t("normal", 1200.0)],
            &ceil(),
        );
        assert_eq!(c, 12);
        assert_eq!(m, 32 << 30);
    }
    #[test]
    fn override_beats_prefer_local() {
        // Operator pin trumps drv-declared `prefer_local_build`.
        let o = ResolvedTarget {
            forced_cores: Some(12.0),
            ..Default::default()
        };
        let (c, _, _) = intent_for(
            None,
            &DrvHints {
                prefer_local_build: Some(true),
                ..Default::default()
            },
            Some(&o),
            None,
            &[],
            &ceil(),
        );
        assert_eq!(c, 12);
    }
    #[test]
    fn override_without_cores_falls_through() {
        // tier-only override (no forced_cores) → normal solve path.
        let o = ResolvedTarget {
            tier: Some("fast".into()),
            ..Default::default()
        };
        assert_eq!(
            intent_for(None, &DrvHints::default(), Some(&o), None, &[], &ceil()).0,
            4, // fallback probe
        );
    }

    #[test]
    fn intent_for_prefer_local_is_minimal() {
        let (c, m, _) = intent(
            None,
            &DrvHints {
                prefer_local_build: Some(true),
                ..Default::default()
            },
        );
        assert_eq!(c, 1);
        assert_eq!(m, LOCAL_MEM_BYTES);
    }
    #[test]
    fn intent_for_serial_pins_one_core() {
        // enable_parallel_building=false beats a fitted curve.
        let fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
        let (c, m, _) = intent(
            Some(&fit),
            &DrvHints {
                enable_parallel_building: Some(false),
                ..Default::default()
            },
        );
        assert_eq!(c, 1);
        assert_eq!(m, 8 << 30); // fallback probe mem (no [sla])
    }
    #[test]
    fn intent_for_cold_start_is_probe() {
        // No [sla] config → fixed fallback probe (4 cores, 8 GiB).
        assert_eq!(
            intent(None, &DrvHints::default()),
            (4, 8 << 30, ceil().default_disk)
        );
        // n_eff < 3 → still explore/probe path. mk_fit's explore state
        // is min=4/max=32 → frozen() = true (span 8) → solve gate would
        // pass if n_eff≥3; below that, explore::next emits max_c=32 with
        // a config or fallback 4 without one.
        let mut fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
        fit.n_eff = 2.0;
        assert_eq!(intent(Some(&fit), &DrvHints::default()).0, 4);
    }
    #[test]
    fn intent_for_fitted_uses_solve() {
        // r[verify sched.sla.intent-from-solve]
        let fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
        let (c, _, d) = intent(Some(&fit), &DrvHints::default());
        // Against p90=1200: c*≈1.95 → ceil 2.
        assert_eq!(c, 2, "ceil(solve_mvp.c_star)");
        assert_eq!(d, 10 << 30, "disk_p90 from fit");
    }
}
