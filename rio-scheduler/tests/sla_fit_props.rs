//! Property invariants on ADR-023 SLA fit/solve.
//!
//! These pin structural properties of `T(c)`, `headroom`, and `solve_mvp` that
//! must hold for *all* parameter values, independent of the unit-test point
//! samples in `sla::{fit,solve}::tests`.

use proptest::prelude::*;
use rio_scheduler::sla::fit::headroom;
use rio_scheduler::sla::solve::{Ceilings, SolveResult, Tier, solve_mvp};
use rio_scheduler::sla::types::*;

fn ceil() -> Ceilings {
    Ceilings {
        max_cores: 64.0,
        max_mem: 256 << 30,
        max_disk: 200 << 30,
        default_disk: 20 << 30,
    }
}

fn mk_fit(s: f64, p: f64, q: f64, sigma: f64) -> FittedParams {
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
                p_bar: RawCores(f64::INFINITY),
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
        n_eff: 1e6,
        n_distinct_c: 1_000_000,
        sum_w: 1e6,
        span: 8.0,
        explore: ExploreState {
            distinct_c: 3,
            min_c: RawCores(4.0),
            max_c: RawCores(32.0),
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

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// USL `T(c) = S + P/c + Q·c` has `dT/dc = -P/c² + Q ≤ 0` for `c ≤ √(P/Q) = c_opt`,
    /// so `T` is monotone non-increasing on `[1, c_opt]`.
    // r[verify sched.sla.fit-nnls]
    #[test]
    fn t_monotone_decreasing_on_1_to_copt(s in 1.0..500f64, p in 10.0..50000f64, q in 0.0..1f64) {
        let fit = DurationFit::Usl {
            s: RefSeconds(s),
            p: RefSeconds(p),
            q,
            p_bar: RawCores(f64::INFINITY),
        };
        let copt = fit.c_opt().0.clamp(2.0, 256.0) as u32;
        let xs: Vec<f64> = (1..=copt).map(|c| fit.t_at(RawCores(c as f64)).0).collect();
        prop_assert!(xs.windows(2).all(|w| w[0] >= w[1] - 1e-6));
    }

    /// `solve_mvp` rejects (does not clamp) `c* > cap_c`, and `cap_c ≤ max_cores`,
    /// so any `Feasible` result has `c* ≤ max_cores`.
    // r[verify sched.sla.solve-citardauq]
    #[test]
    fn solve_never_returns_cstar_gt_maxcores(
        s in 1.0..500f64,
        p in 10.0..50000f64,
        sigma in 0.01..0.4f64,
        p90 in 60.0..3600f64,
    ) {
        let fit = mk_fit(s, p, 0.0, sigma);
        let r = solve_mvp(
            &fit,
            &[Tier { name: "t".into(), p50: None, p90: Some(p90), p99: None }],
            &ceil(),
        );
        if let SolveResult::Feasible { c_star, .. } = r {
            prop_assert!(c_star.0 <= 64.0);
        }
    }

    /// `headroom(n) = 1.25 + 0.7/√max(n,1)` is monotone non-increasing in `n ≥ 1`.
    // r[verify sched.sla.headroom-confidence-scaled]
    #[test]
    fn headroom_monotone_decreasing_in_n(n in 1.0..100f64) {
        prop_assert!(headroom(n) >= headroom(n + 1.0));
    }

    /// If `solve_mvp` returns `Feasible{c*}` for tier `p90`, then the model's predicted
    /// p90 at `c*` — i.e. `T(c*) · exp(z₉₀ · σ)` — actually meets the tier (1% fp slack).
    #[test]
    fn solve_feasible_actually_meets_tier(
        s in 1.0..200f64,
        p in 100.0..20000f64,
        sigma in 0.01..0.3f64,
        p90 in 120.0..3600f64,
    ) {
        let fit = mk_fit(s, p, 0.0, sigma);
        if let SolveResult::Feasible { c_star, .. } = solve_mvp(
            &fit,
            &[Tier { name: "t".into(), p50: None, p90: Some(p90), p99: None }],
            &ceil(),
        ) {
            let predicted_p90 = fit.fit.t_at(c_star).0 * (1.2816 * sigma).exp();
            prop_assert!(
                predicted_p90 <= p90 * 1.01,
                "c*={} predicted_p90={} tier={}",
                c_star.0,
                predicted_p90,
                p90
            );
        }
    }
}
