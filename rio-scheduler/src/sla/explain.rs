//! `rio-cli sla explain` — per-derivation solve trace.
//!
//! [`explain`] re-runs the [`super::solve::solve_mvp`] tier walk in
//! dry-run mode, recording every `continue` reason instead of returning
//! on the first feasible tier. The CLI renders the result as a
//! candidate table so an operator can see *why* a key landed where it
//! did ("`fast` rejected: c*=18 > p̄=12"; "`normal` feasible at c*=6").

use super::fit::headroom;
use super::r#override::ResolvedTarget;
use super::solve::{Ceilings, Tier};
use super::types::{DurationFit, FittedParams, MemFit, ModelKey, RawCores};

/// One tier's solve attempt. `c_star`/`mem` are populated as far as the
/// solve got before the binding constraint fired (e.g. a `serial-floor`
/// rejection has neither — β≥0 means the quadratic was never formed).
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateRow {
    pub tier: String,
    pub c_star: Option<f64>,
    pub mem: Option<u64>,
    /// `"p90"` (no p90 target on this tier) | `"serial-floor"` (β≥0) |
    /// `"discriminant"` (disc<0) | `"core-ceiling"` (c*>cap_c) |
    /// `"mem-ceiling"` | `"disk-ceiling"` | `"-"` (feasible).
    pub binding_constraint: String,
    pub feasible: bool,
}

#[derive(Debug, Clone)]
pub struct ExplainResult {
    pub key: ModelKey,
    /// One-line `DurationFit` description (`"Amdahl S=30.0 P=2000.0"` …),
    /// or `"(no fit — cold-start probe)"` when `fit` is `None`.
    pub fit_summary: String,
    /// Where the model parameters came from. Phase-7: `"per-key"` (a
    /// cached fit for this exact key) or `"none"` (cold start). Phase-9
    /// adds `"fleet-prior"` when partial pooling supplies the curve.
    pub prior_source: String,
    /// Human-readable description of the override that short-circuited
    /// the solve, or `None` if no `forced_cores` override applied. A
    /// tier-only override does NOT short-circuit (solve still runs); it
    /// shows up as the only row in `candidates` instead.
    pub override_applied: Option<String>,
    pub candidates: Vec<CandidateRow>,
}

/// Re-run the tier walk for `key`, recording every reject reason.
///
/// Mirrors [`super::solve::solve_mvp`] gate-for-gate so the table the
/// operator sees matches what dispatch did. Drift between the two is a
/// bug — both consume the same `tiers`/`ceil`/`fit` inputs.
///
/// `override_` precedence matches [`super::solve::intent_for`]: a
/// `forced_cores` pin short-circuits the model entirely (the trace is a
/// single synthetic "override" row). A tier-only override doesn't
/// short-circuit; instead the candidate table is filtered to that tier
/// so the operator sees how the solve fared against the pinned target.
pub fn explain(
    key: &ModelKey,
    fit: Option<&FittedParams>,
    tiers: &[Tier],
    ceil: &Ceilings,
    override_: Option<&ResolvedTarget>,
) -> ExplainResult {
    // forced_cores short-circuit — same as intent_for's first branch.
    if let Some(o) = override_
        && let Some(c) = o.forced_cores
    {
        return ExplainResult {
            key: key.clone(),
            fit_summary: fit
                .map(summarize_fit)
                .unwrap_or_else(|| "(bypassed)".into()),
            prior_source: if fit.is_some() { "per-key" } else { "none" }.into(),
            override_applied: Some(format!(
                "forced cores={c} mem={}",
                o.forced_mem
                    .map(|m| format!("{:.1}Gi", m as f64 / (1u64 << 30) as f64))
                    .unwrap_or_else(|| "(M(c))".into())
            )),
            candidates: vec![CandidateRow {
                tier: "(override)".into(),
                c_star: Some(c),
                mem: o.forced_mem,
                binding_constraint: "-".into(),
                feasible: true,
            }],
        };
    }

    let Some(fit) = fit else {
        return ExplainResult {
            key: key.clone(),
            fit_summary: "(no fit — cold-start probe)".into(),
            prior_source: "none".into(),
            override_applied: None,
            candidates: Vec::new(),
        };
    };

    // tier-only override: filter the ladder to the pinned tier so the
    // table answers "would this key hit the tier I asked for?"
    let pinned = override_.and_then(|o| o.tier.as_deref());
    let walk: Vec<&Tier> = tiers
        .iter()
        .filter(|t| pinned.is_none_or(|p| p == t.name))
        .collect();

    let (s, p, q) = fit.fit.spq();
    let cap_c = fit.fit.p_bar().0.min(fit.fit.c_opt().0).min(ceil.max_cores);
    let h = headroom(fit.n_eff);
    let disk = fit.disk_p90.map(|d| d.0).unwrap_or(ceil.default_disk);

    let mut candidates = Vec::with_capacity(walk.len());
    for tier in walk {
        let mut row = CandidateRow {
            tier: tier.name.clone(),
            c_star: None,
            mem: None,
            binding_constraint: "-".into(),
            feasible: false,
        };
        let Some(p90) = tier.p90 else {
            row.binding_constraint = "p90".into();
            candidates.push(row);
            continue;
        };
        let beta = s - (-1.2816 * fit.sigma_resid).exp() * p90;
        if beta >= 0.0 {
            row.binding_constraint = "serial-floor".into();
            candidates.push(row);
            continue;
        }
        let disc = beta * beta - 4.0 * q * p;
        if disc < 0.0 {
            row.binding_constraint = "discriminant".into();
            candidates.push(row);
            continue;
        }
        let denom = -0.5 * (beta + beta.signum() * disc.sqrt());
        let c_star = if denom.abs() < 1e-12 {
            f64::INFINITY
        } else {
            (p / denom).max(1.0)
        };
        row.c_star = Some(c_star);
        if c_star > cap_c {
            row.binding_constraint = "core-ceiling".into();
            candidates.push(row);
            continue;
        }
        let mem = (fit.mem.at(RawCores(c_star)).0 as f64 * h) as u64;
        row.mem = Some(mem);
        if mem > ceil.max_mem {
            row.binding_constraint = "mem-ceiling".into();
            candidates.push(row);
            continue;
        }
        if disk > ceil.max_disk {
            row.binding_constraint = "disk-ceiling".into();
            candidates.push(row);
            continue;
        }
        row.feasible = true;
        candidates.push(row);
    }

    ExplainResult {
        key: key.clone(),
        fit_summary: summarize_fit(fit),
        prior_source: "per-key".into(),
        override_applied: pinned.map(|t| format!("tier pinned to {t:?}")),
        candidates,
    }
}

fn summarize_fit(f: &FittedParams) -> String {
    let head = match &f.fit {
        DurationFit::Probe => "Probe".into(),
        DurationFit::Amdahl { s, p } => format!("Amdahl S={:.1} P={:.1}", s.0, p.0),
        DurationFit::Capped { s, p, p_bar } => {
            format!("Capped S={:.1} P={:.1} p̄={:.1}", s.0, p.0, p_bar.0)
        }
        DurationFit::Usl { s, p, q, p_bar } => {
            format!("Usl S={:.1} P={:.1} Q={:.4} p̄={:.1}", s.0, p.0, q, p_bar.0)
        }
    };
    let mem = match &f.mem {
        MemFit::Coupled { a, b, .. } => format!("M(c)=exp({a:.2}+{b:.2}·ln c)"),
        MemFit::Independent { p90 } => {
            format!("M=p90 {:.1}Gi", p90.0 as f64 / (1u64 << 30) as f64)
        }
    };
    format!("{head} σ={:.3} n_eff={:.1} | {mem}", f.sigma_resid, f.n_eff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sla::types::*;

    fn mk_fit(s: f64, p: f64, q: f64, p_bar: f64) -> FittedParams {
        FittedParams {
            key: key(),
            fit: if q > 0.0 {
                DurationFit::Usl {
                    s: RefSeconds(s),
                    p: RefSeconds(p),
                    q,
                    p_bar: RawCores(p_bar),
                }
            } else {
                DurationFit::Capped {
                    s: RefSeconds(s),
                    p: RefSeconds(p),
                    p_bar: RawCores(p_bar),
                }
            },
            mem: MemFit::Independent {
                p90: MemBytes(2 << 30),
            },
            disk_p90: Some(DiskBytes(10 << 30)),
            sigma_resid: 0.1,
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
        }
    }
    fn key() -> ModelKey {
        ModelKey {
            pname: "x".into(),
            system: "x".into(),
            tenant: "x".into(),
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
    fn records_every_tier_with_reason() {
        // t0=60 → c* > p̄=4 → core-ceiling; t1=600 → feasible.
        let fit = mk_fit(30.0, 100.0, 0.0, 4.0);
        let r = explain(
            &key(),
            Some(&fit),
            &[t("t0", 60.0), t("t1", 600.0)],
            &ceil(),
            None,
        );
        assert_eq!(r.candidates.len(), 2);
        assert_eq!(r.candidates[0].tier, "t0");
        assert_eq!(r.candidates[0].binding_constraint, "core-ceiling");
        assert!(!r.candidates[0].feasible);
        assert!(r.candidates[0].c_star.is_some());
        assert_eq!(r.candidates[1].tier, "t1");
        assert!(r.candidates[1].feasible);
        assert_eq!(r.candidates[1].binding_constraint, "-");
        assert_eq!(r.prior_source, "per-key");
    }

    #[test]
    fn serial_floor_has_no_cstar() {
        // S=400 > target → β≥0, quadratic never formed.
        let fit = mk_fit(400.0, 100.0, 0.0, 64.0);
        let r = explain(&key(), Some(&fit), &[t("t0", 300.0)], &ceil(), None);
        assert_eq!(r.candidates[0].binding_constraint, "serial-floor");
        assert!(r.candidates[0].c_star.is_none());
    }

    #[test]
    fn discriminant_reject() {
        let fit = mk_fit(30.0, 2000.0, 5.0, 64.0);
        let r = explain(&key(), Some(&fit), &[t("t0", 50.0)], &ceil(), None);
        assert_eq!(r.candidates[0].binding_constraint, "discriminant");
    }

    #[test]
    fn forced_cores_override_short_circuits() {
        let o = ResolvedTarget {
            forced_cores: Some(12.0),
            forced_mem: Some(32 << 30),
            ..Default::default()
        };
        let r = explain(&key(), None, &[t("t0", 300.0)], &ceil(), Some(&o));
        assert_eq!(r.candidates.len(), 1);
        assert_eq!(r.candidates[0].tier, "(override)");
        assert_eq!(r.candidates[0].c_star, Some(12.0));
        assert!(r.override_applied.is_some());
    }

    #[test]
    fn tier_only_override_filters_ladder() {
        let fit = mk_fit(30.0, 100.0, 0.0, 64.0);
        let o = ResolvedTarget {
            tier: Some("t1".into()),
            ..Default::default()
        };
        let r = explain(
            &key(),
            Some(&fit),
            &[t("t0", 60.0), t("t1", 600.0)],
            &ceil(),
            Some(&o),
        );
        assert_eq!(r.candidates.len(), 1);
        assert_eq!(r.candidates[0].tier, "t1");
        assert!(r.override_applied.as_deref().unwrap().contains("t1"));
    }

    #[test]
    fn no_fit_is_cold_start() {
        let r = explain(&key(), None, &[t("t0", 300.0)], &ceil(), None);
        assert!(r.candidates.is_empty());
        assert_eq!(r.prior_source, "none");
        assert!(r.fit_summary.contains("cold-start"));
    }
}
