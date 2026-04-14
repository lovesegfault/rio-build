// TODO(ADR-023): drop once Phase 2 lands
#![allow(dead_code)]

use super::fit::headroom;
use super::types::{DiskBytes, FittedParams, MemBytes, RawCores};

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

// r[impl sched.sla.solve-citardauq]
// r[impl sched.sla.solve-reject-not-clamp]
pub fn solve_mvp(fit: &FittedParams, tiers: &[Tier], ceil: &Ceilings) -> SolveResult {
    let (s, p, q) = fit.fit.spq();
    let cap_c = fit.fit.p_bar().0.min(fit.fit.c_opt().0).min(ceil.max_cores);
    let h = headroom(fit.n_eff);
    let disk = fit.disk_p90.map(|d| d.0).unwrap_or(ceil.default_disk);

    for tier in tiers {
        let Some(p90) = tier.p90 else { continue };
        let beta = s - (-1.2816 * fit.sigma_resid).exp() * p90;
        if beta >= 0.0 {
            continue; // target ≤ serial floor
        }
        let disc = beta * beta - 4.0 * q * p;
        if disc < 0.0 {
            continue;
        }
        let denom = -0.5 * (beta + beta.signum() * disc.sqrt());
        let c_star = if denom.abs() < 1e-12 {
            f64::INFINITY
        } else {
            (p / denom).max(1.0)
        };
        if c_star > cap_c {
            continue; // REJECT (not clamp)
        }
        let mem = (fit.mem.at(RawCores(c_star)).0 as f64 * h) as u64;
        if mem > ceil.max_mem {
            continue;
        }
        if disk > ceil.max_disk {
            continue;
        }
        return SolveResult::Feasible {
            tier: tier.name.clone(),
            c_star: RawCores(c_star),
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
            tier: None,
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
    fn ceil() -> Ceilings {
        Ceilings {
            max_cores: 64.0,
            max_mem: 256 << 30,
            max_disk: 200 << 30,
            default_disk: 20 << 30,
        }
    }

    #[test]
    fn citardauq_degenerates_to_amdahl_at_q0() {
        let fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
        let SolveResult::Feasible { c_star, tier, .. } =
            solve_mvp(&fit, &[t("t0", 300.0)], &ceil())
        else {
            panic!()
        };
        // β=30-e^{-0.128}·300≈-234 → c*=2000/234≈8.54
        assert!((c_star.0 - 8.54).abs() < 0.3, "got {}", c_star.0);
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
    #[test]
    fn beta_nonneg_rejects_tier() {
        // S=400 > target → infeasible
        let fit = mk_fit(400.0, 100.0, 0.0, f64::INFINITY, 0.1);
        assert!(matches!(
            solve_mvp(&fit, &[t("t0", 300.0)], &ceil()),
            SolveResult::BestEffort { .. }
        ));
    }
}
