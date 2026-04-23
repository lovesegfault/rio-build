//! ADR-023 Algorithm 3: q-quantile of the geometric-lognormal mixture
//! `T_total = G·T·exp(ε)`, `G~Geom(p)` (attempts incl. success),
//! `ε~N(0,σ²)`. Deterministic — finite-sum CDF + log-space bisection,
//! no Monte Carlo.

use statrs::distribution::{ContinuousCDF, Normal};

/// CDF of `T_total` at `x`. Truncates the geometric tail at
/// `p^k < 1e-6` (≤20 terms for p≤0.5). `p=0` collapses to the pure
/// lognormal `Φ(ln(x/t)/σ)`.
pub(super) fn cdf(x: f64, t: f64, sigma: f64, p: f64) -> f64 {
    let sigma = sigma.max(1e-3);
    let n01 = Normal::standard();
    if p == 0.0 {
        return n01.cdf((x / t).ln() / sigma);
    }
    let k_max = ((1e-6f64).ln() / p.ln()).ceil() as usize;
    (1..=k_max)
        .map(|k| (1.0 - p) * p.powi(k as i32 - 1) * n01.cdf((x / (k as f64 * t)).ln() / sigma))
        .sum()
}

// r[impl sched.sla.quantile-geo-lognormal]
// r[impl sched.sla.hw-class.zq-inflation]
/// q-quantile of `T_total = G·T·exp(ε)`, `G~Geom(p)`, `ε~N(0,σ²)`.
/// Requires `q≤0.99`, `p≤0.5`. Deterministic (no MC).
///
/// `z_q` is the Student-t prediction-interval factor from
/// [`super::fit::z_q`] — fixed for a given `(fit, q)` so callers hoist
/// it out of the bisection. `σ` is inflated to `σ' = σ·z_q/Φ⁻¹(q)`
/// (ADR-023 alg-quantile L602) so the geometric mixture's lognormal
/// component carries parameter uncertainty. At `q=0.5`, `Φ⁻¹(q)=0` and
/// the ratio is `0/0`; the median is σ-insensitive so the branch uses
/// raw σ (under-inflates by ~9%; immaterial).
pub fn quantile(q: f64, t: f64, sigma: f64, p: f64, z_q: f64) -> f64 {
    debug_assert!(q <= 0.99 && p <= 0.5);
    let sigma = sigma.max(1e-3);
    if p == 0.0 {
        // Pure lognormal: small-n_eff widening is `exp(σ·z_q)` directly
        // (ADR-023 alg-quantile L600).
        return t * (sigma * z_q).exp();
    }
    let n01 = Normal::standard();
    let ppf_q = n01.inverse_cdf(q);
    let sigma_p = if ppf_q.abs() < 1e-9 {
        sigma
    } else {
        sigma * z_q / ppf_q
    };
    let k_max = ((1e-6f64).ln() / p.ln()).ceil() as usize;
    let mut lo = t * (-3.0 * sigma_p).exp();
    let mut hi = k_max as f64 * t * (sigma_p * (ppf_q + 1.0).max(3.0)).exp();
    debug_assert!(cdf(lo, t, sigma_p, p) < q && q < cdf(hi, t, sigma_p, p));
    while (hi / lo).ln() > 1e-3 {
        let mid = (lo * hi).sqrt();
        if cdf(mid, t, sigma_p, p) < q {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (lo * hi).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Asymptotic z_q (Φ⁻¹(0.9)) — recovers the pre-A6 behaviour.
    const Z90: f64 = 1.2815515655446004;

    // r[verify sched.sla.quantile-geo-lognormal]
    #[test]
    fn quantile_p0_is_pure_lognormal() {
        let q = quantile(0.9, 100.0, 0.2, 0.0, Z90);
        // Φ⁻¹(0.9) ≈ 1.2816 → 100·exp(0.2·1.2816) ≈ 129.22
        assert!((q - 100.0 * (0.2 * Z90).exp()).abs() < 0.1, "got {q}");
    }

    // r[verify sched.sla.hw-class.zq-inflation]
    #[test]
    fn quantile_on_demand_uses_zq_not_ppf() {
        // p=0 branch: T·exp(σ·z_q). At z_q=1.89 (n_eff=3), q=0.9, σ=0.2:
        let got = quantile(0.9, 100.0, 0.2, 0.0, 1.89);
        let expected = 100.0 * (0.2 * 1.89_f64).exp();
        assert!((got - expected).abs() / expected < 1e-6, "{got}");
        // …NOT exp(σ·Φ⁻¹(q)) — the asymptotic value would be ~129.
        assert!(got > 145.0);
    }

    // r[verify sched.sla.hw-class.zq-inflation]
    #[test]
    fn quantile_sigma_prime_inflation_spot() {
        // σ' = σ·z_q/Φ⁻¹(q). At q=0.9, Φ⁻¹=1.2816, z_q=1.89 → σ'≈σ·1.474.
        // The geometric mixture's spread widens with σ', so the inflated
        // quantile must exceed the asymptotic-z_q one.
        let inflated = quantile(0.9, 100.0, 0.2, 0.3, 1.89);
        let raw = quantile(0.9, 100.0, 0.2, 0.3, Z90);
        assert!(inflated > raw * 1.05, "{inflated} vs {raw}");
    }

    #[test]
    fn quantile_median_branch_finite() {
        // q=0.5 → Φ⁻¹(q)=0; σ' branch must not divide by zero.
        let q = quantile(0.5, 100.0, 0.2, 0.3, 0.0);
        assert!(q.is_finite() && q > 0.0);
    }

    #[test]
    fn quantile_monotone_in_q() {
        let (t, sigma, p) = (100.0, 0.2, 0.3);
        let n01 = Normal::standard();
        let qs = [0.5, 0.7, 0.9, 0.95, 0.99];
        let xs: Vec<f64> = qs
            .iter()
            .map(|&q| quantile(q, t, sigma, p, n01.inverse_cdf(q)))
            .collect();
        for w in xs.windows(2) {
            assert!(w[0] < w[1], "non-monotone: {xs:?}");
        }
    }

    #[test]
    fn quantile_monotone_in_p() {
        // More preemption → larger quantile.
        let (q, t, sigma) = (0.9, 100.0, 0.2);
        let ps = [0.0, 0.1, 0.3, 0.49];
        let xs: Vec<f64> = ps.iter().map(|&p| quantile(q, t, sigma, p, Z90)).collect();
        for w in xs.windows(2) {
            assert!(w[0] < w[1], "non-monotone in p: {xs:?}");
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]
        #[test]
        fn cdf_of_quantile_is_q(
            q in 0.5..0.99f64,
            t in 10.0..1000.0f64,
            sigma in 0.05..0.4f64,
            p in 0.0..0.49f64,
        ) {
            // z_q = Φ⁻¹(q) ⇒ σ' = σ (uninflated), so cdf(σ) round-trips.
            let zq = Normal::standard().inverse_cdf(q);
            let x = quantile(q, t, sigma, p, zq);
            let cq = cdf(x, t, sigma, p);
            // Bisection halts at ln(hi/lo)<1e-3 ⇒ x within 0.1%; CDF slope
            // (≈ 1/(σ√(2π)) in z-space) bounds |Δq| ≲ 1e-3/σ ≤ 0.02.
            prop_assert!((cq - q).abs() < 0.02, "cdf({x})={cq}, want {q}");
        }

        // r[verify sched.sla.hw-class.zq-inflation]
        /// On-demand (`p=0`) is `T·exp(σ·z_q)` — strictly monotone in
        /// z_q. The spot path (`p>0`) is NOT generally monotone in σ':
        /// when the q-quantile lands just left of a geometric mode
        /// `k·T`, widening σ' moves that component's mass leftward,
        /// raising F(x) and *lowering* the quantile. ADR-023 already
        /// accepts centroid-z_q under-coverage at extrapolation; the
        /// targeted `quantile_sigma_prime_inflation_spot` test above
        /// covers the typical-case inflation direction.
        #[test]
        fn quantile_monotone_in_zq_on_demand(
            q in 0.5..0.99f64,
            t in 10.0..1000.0f64,
            sigma in 0.05..0.4f64,
            zq in 0.0..3.0f64,
        ) {
            // p=0 branch is `T·exp(σ·z_q)` — strictly monotone over the
            // whole q range.
            let a = quantile(q, t, sigma, 0.0, zq);
            let b = quantile(q, t, sigma, 0.0, zq + 0.1);
            prop_assert!(b >= a);
        }
    }
}
