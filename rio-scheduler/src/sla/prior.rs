//! ADR-023 §2.10 fleet-learned priors. A never-seen `ModelKey` gets a
//! prior in the (S, P, Q, a, b) basis (same parameters [`super::fit`]
//! produces) from one of three sources, then [`partial_pool`] blends
//! that prior with the per-key fit as samples accrue.
//!
//! Pure functions only — the caller owns the `seed` map and the fleet
//! aggregate; this module does no DB I/O.

use std::collections::HashMap;

use super::config::ProbeShape;
use super::types::ModelKey;

/// (S, P, Q, a, b) flat tuple. Distinct from [`super::types::FittedParams`]
/// (which carries the staged enums + explore state) because the prior
/// machinery only needs the five scalars the fitter regresses on.
#[derive(Debug, Clone, PartialEq)]
pub struct FitParams {
    pub s: f64,
    pub p: f64,
    pub q: f64,
    pub a: f64,
    pub b: f64,
}

/// Provenance of a prior, surfaced in `SizingExplain` so an operator
/// can tell "this came from your seed table" from "this is the fleet
/// median" from "this is just the cold-start probe rephrased".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriorSource {
    Seed,
    Fleet,
    Operator,
}

/// Inputs to [`prior_for`]. `seed` is the operator-curated per-key
/// table; `fleet` is the cross-tenant aggregate ([`fleet_median`]
/// returns `None` until enough keys have a Coupled fit).
pub struct PriorSources {
    pub seed: HashMap<ModelKey, FitParams>,
    pub fleet: Option<FitParams>,
    pub operator: ProbeShape,
    pub default_tier_p90: f64,
}

// r[impl sched.sla.prior-partial-pool]
/// Precedence: explicit seed → fleet median (clamped to a [0.5×, 2×]
/// band around the operator probe) → operator probe rephrased into the
/// (S,P,Q,a,b) basis. Seed is exact-key only — no prefix match; an
/// operator who wants "all `ghc-*` get this" writes an override, not a
/// seed.
pub fn prior_for(key: &ModelKey, src: &PriorSources) -> (FitParams, PriorSource) {
    if let Some(s) = src.seed.get(key) {
        return (s.clone(), PriorSource::Seed);
    }
    if let Some(f) = &src.fleet {
        return (
            clamp_to_operator(f, &src.operator, src.default_tier_p90),
            PriorSource::Fleet,
        );
    }
    (
        operator_to_spq(&src.operator, src.default_tier_p90),
        PriorSource::Operator,
    )
}

/// Map the operator's linear probe `{cpu, mem_per_core, mem_base}` into
/// the (S, P, Q, a, b) basis. ADR §2.10: a build that the operator
/// expects to take `tier_p90` on `probe.cpu` cores, half-serial half-
/// parallel, with `M(c) = mem_base + c·mem_per_core`.
///
/// `b = 1` (linear M(c) in log-log is slope 1 only at one point, but
/// b=1 is the closest power-law to "linear in c"); `a = ln(M(1))`;
/// `P = (p90/2)·cpu` (the parallel half scaled to 1 core);
/// `S = (p90/2)·(1 − 1/cpu)`; `Q = 0`.
pub fn operator_to_spq(probe: &ProbeShape, tier_p90: f64) -> FitParams {
    let half = tier_p90 / 2.0;
    FitParams {
        s: half * (1.0 - 1.0 / probe.cpu),
        p: half * probe.cpu,
        q: 0.0,
        a: ((probe.mem_base + probe.mem_per_core) as f64).ln(),
        b: 1.0,
    }
}

/// Shrinkage blend: `w·θ_pname + (1−w)·θ_prior` with `w = n_eff /
/// (n_eff + n0)`. At `n_eff = n0` the per-key fit and the prior weigh
/// equally; at `n_eff = 0` the result is pure prior. ADR's `n0 = 3`
/// means three effective samples already dominate the prior (w = 0.5).
pub fn partial_pool(
    theta_pname: &FitParams,
    n_eff: f64,
    theta_prior: &FitParams,
    n0: f64,
) -> FitParams {
    let w = n_eff / (n_eff + n0);
    let blend = |a: f64, b: f64| w * a + (1.0 - w) * b;
    FitParams {
        s: blend(theta_pname.s, theta_prior.s),
        p: blend(theta_pname.p, theta_prior.p),
        q: blend(theta_pname.q, theta_prior.q),
        a: blend(theta_pname.a, theta_prior.a),
        b: blend(theta_pname.b, theta_prior.b),
    }
}

/// Clamp each fleet parameter to `[0.5×, 2×]` of the operator-derived
/// value. A fleet median that drifts more than 2× from the operator's
/// stated probe is suspicious (one noisy tenant dominating, or the
/// operator's probe is stale) — either way the operator's number wins
/// at the band edge. Parameters whose operator value is zero (Q) are
/// passed through unclamped: a [0.5×0, 2×0] band is degenerate.
fn clamp_to_operator(fleet: &FitParams, op: &ProbeShape, tier_p90: f64) -> FitParams {
    let basis = operator_to_spq(op, tier_p90);
    FitParams {
        s: clamp_field(fleet.s, basis.s, "s"),
        p: clamp_field(fleet.p, basis.p, "p"),
        q: clamp_field(fleet.q, basis.q, "q"),
        a: clamp_field(fleet.a, basis.a, "a"),
        b: clamp_field(fleet.b, basis.b, "b"),
    }
}

fn clamp_field(fleet: f64, op: f64, param: &'static str) -> f64 {
    if op.abs() < f64::EPSILON {
        return fleet;
    }
    let (lo, hi) = if op > 0.0 {
        (0.5 * op, 2.0 * op)
    } else {
        (2.0 * op, 0.5 * op)
    };
    let clamped = fleet.clamp(lo, hi);
    if clamped != fleet {
        ::metrics::gauge!("rio_scheduler_sla_prior_divergence", "param" => param).set(fleet / op);
    }
    clamped
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn probe() -> ProbeShape {
        ProbeShape {
            cpu: 4.0,
            mem_per_core: 2 * GIB,
            mem_base: 4 * GIB,
        }
    }

    fn key(pname: &str) -> ModelKey {
        ModelKey {
            pname: pname.into(),
            system: "x86_64-linux".into(),
            tenant: "t0".into(),
        }
    }

    // r[verify sched.sla.prior-partial-pool]
    #[test]
    fn partial_pool_at_n0_is_mostly_prior() {
        let theta = FitParams {
            s: 100.0,
            p: 1000.0,
            q: 0.1,
            a: 20.0,
            b: 1.5,
        };
        let prior = FitParams {
            s: 0.0,
            p: 0.0,
            q: 0.0,
            a: 0.0,
            b: 0.0,
        };
        // n_eff=1, n0=3 → w=0.25 → 0.75·prior + 0.25·θ
        let pooled = partial_pool(&theta, 1.0, &prior, 3.0);
        assert!((pooled.s - 25.0).abs() < 1e-9);
        assert!((pooled.p - 250.0).abs() < 1e-9);
        assert!((pooled.q - 0.025).abs() < 1e-9);
        assert!((pooled.a - 5.0).abs() < 1e-9);
        assert!((pooled.b - 0.375).abs() < 1e-9);
        // n_eff=n0 → exact midpoint
        let mid = partial_pool(&theta, 3.0, &prior, 3.0);
        assert!((mid.p - 500.0).abs() < 1e-9);
    }

    #[test]
    fn operator_probe_maps_to_spq_basis() {
        let fp = operator_to_spq(&probe(), 300.0);
        // half=150; P=150·4=600; S=150·(1−1/4)=112.5
        assert!((fp.p - 600.0).abs() < 1e-9);
        assert!((fp.s - 112.5).abs() < 1e-9);
        assert_eq!(fp.q, 0.0);
        assert_eq!(fp.b, 1.0);
        assert!((fp.a - ((6 * GIB) as f64).ln()).abs() < 1e-9);
    }

    #[test]
    fn prior_precedence_seed_beats_fleet() {
        let seeded = FitParams {
            s: 1.0,
            p: 2.0,
            q: 3.0,
            a: 4.0,
            b: 5.0,
        };
        let fleet = FitParams {
            s: 10.0,
            p: 20.0,
            q: 30.0,
            a: 40.0,
            b: 50.0,
        };
        let mut seed = HashMap::new();
        seed.insert(key("hello"), seeded.clone());
        let src = PriorSources {
            seed,
            fleet: Some(fleet.clone()),
            operator: probe(),
            default_tier_p90: 300.0,
        };
        // exact-key seed wins
        let (fp, prov) = prior_for(&key("hello"), &src);
        assert_eq!(prov, PriorSource::Seed);
        assert_eq!(fp, seeded);
        // miss → fleet (clamped)
        let (fp, prov) = prior_for(&key("world"), &src);
        assert_eq!(prov, PriorSource::Fleet);
        assert_ne!(fp, fleet); // clamped, since fleet.b=50 ≫ 2×1.0
        // no fleet → operator
        let src2 = PriorSources {
            seed: HashMap::new(),
            fleet: None,
            operator: probe(),
            default_tier_p90: 300.0,
        };
        let (fp, prov) = prior_for(&key("world"), &src2);
        assert_eq!(prov, PriorSource::Operator);
        assert_eq!(fp, operator_to_spq(&probe(), 300.0));
    }

    #[test]
    fn clamp_to_operator_band() {
        let basis = operator_to_spq(&probe(), 300.0); // s=112.5, p=600, q=0, a=ln(6Gi), b=1
        // p=2000 > 2×600 → clamps to 1200; b=0.2 < 0.5×1 → clamps to 0.5
        let fleet = FitParams {
            s: 100.0,
            p: 2000.0,
            q: 0.3,
            a: basis.a,
            b: 0.2,
        };
        let clamped = clamp_to_operator(&fleet, &probe(), 300.0);
        assert!((clamped.p - 1200.0).abs() < 1e-9);
        assert!((clamped.b - 0.5).abs() < 1e-9);
        // s=100 is within [56.25, 225] → unchanged
        assert!((clamped.s - 100.0).abs() < 1e-9);
        // q: operator basis is 0 → passed through
        assert!((clamped.q - 0.3).abs() < 1e-9);
    }
}
