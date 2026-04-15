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
/// q-quantile of `T_total = G·T·exp(ε)`, `G~Geom(p)`, `ε~N(0,σ²)`.
/// Requires `q≤0.99`, `p≤0.5`. Deterministic (no MC).
pub fn quantile(q: f64, t: f64, sigma: f64, p: f64) -> f64 {
    debug_assert!(q <= 0.99 && p <= 0.5);
    let sigma = sigma.max(1e-3);
    let n01 = Normal::standard();
    if p == 0.0 {
        return t * (sigma * n01.inverse_cdf(q)).exp();
    }
    let k_max = ((1e-6f64).ln() / p.ln()).ceil() as usize;
    let mut lo = t * (-3.0 * sigma).exp();
    let mut hi = k_max as f64 * t * (sigma * (n01.inverse_cdf(q) + 1.0).max(3.0)).exp();
    debug_assert!(cdf(lo, t, sigma, p) < q && q < cdf(hi, t, sigma, p));
    while (hi / lo).ln() > 1e-3 {
        let mid = (lo * hi).sqrt();
        if cdf(mid, t, sigma, p) < q {
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

    // r[verify sched.sla.quantile-geo-lognormal]
    #[test]
    fn quantile_p0_is_pure_lognormal() {
        let q = quantile(0.9, 100.0, 0.2, 0.0);
        // Φ⁻¹(0.9) ≈ 1.2816 → 100·exp(0.2·1.2816) ≈ 129.22
        assert!((q - 100.0 * (0.2 * 1.2816f64).exp()).abs() < 0.1, "got {q}");
    }

    #[test]
    fn quantile_monotone_in_q() {
        let (t, sigma, p) = (100.0, 0.2, 0.3);
        let qs = [0.5, 0.7, 0.9, 0.95, 0.99];
        let xs: Vec<f64> = qs.iter().map(|&q| quantile(q, t, sigma, p)).collect();
        for w in xs.windows(2) {
            assert!(w[0] < w[1], "non-monotone: {xs:?}");
        }
    }

    #[test]
    fn quantile_monotone_in_p() {
        // More preemption → larger quantile.
        let (q, t, sigma) = (0.9, 100.0, 0.2);
        let ps = [0.0, 0.1, 0.3, 0.49];
        let xs: Vec<f64> = ps.iter().map(|&p| quantile(q, t, sigma, p)).collect();
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
            let x = quantile(q, t, sigma, p);
            let cq = cdf(x, t, sigma, p);
            // Bisection halts at ln(hi/lo)<1e-3 ⇒ x within 0.1%; CDF slope
            // (≈ 1/(σ√(2π)) in z-space) bounds |Δq| ≲ 1e-3/σ ≤ 0.02.
            prop_assert!((cq - q).abs() < 0.02, "cdf({x})={cq}, want {q}");
        }
    }
}
