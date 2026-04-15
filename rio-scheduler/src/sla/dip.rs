//! Hartigan-Hartigan dip test for unimodality on per-key log-residuals.
//!
//! ADR-023 phase-7: a multimodal residual distribution means the
//! single-curve `T(c)` model is wrong for this key — typically two
//! distinct workloads sharing a `pname` (e.g. `cargo test` vs `cargo
//! build`, or input-dependent fast/slow paths). [`is_multimodal`] fires
//! `rio_scheduler_sla_residual_multimodal_total{tenant}` from
//! [`super::ingest::refit`] so an operator can split the key.
//!
//! n is bounded by the ring buffer (≤32) so the O(n²) brute-force dip
//! and the small-n critical-value table are sufficient — no need for
//! the full Hartigan iterative algorithm or Monte-Carlo p-values.

/// Dip statistic: minimum sup-distance between the empirical CDF and
/// any unimodal CDF. Brute-force over candidate modes: for each `m`,
/// the closest unimodal CDF is the greatest-convex-minorant of `F_n`
/// on `[0,m]` joined to the least-concave-majorant on `[m,n)`; the dip
/// at `m` is the larger of the two max gaps. The Hartigan dip is the
/// minimum over `m`, halved (the unimodal fit is allowed to sit between
/// `F_n−D` and `F_n+D`, so the GCM/LCM envelope width is `2D`).
///
/// Input need not be sorted. n<4 or zero-range → 0.0 (degenerate;
/// caller treats as unimodal).
pub fn dip(x: &[f64]) -> f64 {
    let n = x.len();
    if n < 4 {
        return 0.0;
    }
    let mut x: Vec<f64> = x.to_vec();
    x.sort_by(f64::total_cmp);
    if x[n - 1] - x[0] < 1e-12 {
        return 0.0;
    }
    // Mid-step ECDF: F_n(x[i]) = (i + 0.5)/n. Mid-step (vs i/n or
    // (i+1)/n) keeps the dip symmetric under reflection.
    let f: Vec<f64> = (0..n).map(|i| (i as f64 + 0.5) / n as f64).collect();
    let mut best = f64::INFINITY;
    for m in 0..n {
        let dl = hull_gap(&x[..=m], &f[..=m], false);
        let dr = hull_gap(&x[m..], &f[m..], true);
        best = best.min(dl.max(dr));
    }
    best / 2.0
}

/// Max vertical gap between `(x,f)` and its convex (`upper=false`,
/// gap = `f − hull`) or concave (`upper=true`, gap = `hull − f`) hull.
/// `x` must be sorted ascending. O(n) hull via Andrew's monotone chain
/// (one half), O(n) gap via linear scan along hull segments — O(n²)
/// total over all `m` in [`dip`], fine for n≤32.
fn hull_gap(x: &[f64], f: &[f64], upper: bool) -> f64 {
    let n = x.len();
    if n <= 2 {
        return 0.0;
    }
    let mut h: Vec<usize> = Vec::with_capacity(n);
    for i in 0..n {
        while h.len() >= 2 {
            let (a, b) = (h[h.len() - 2], h[h.len() - 1]);
            let cross = (x[b] - x[a]) * (f[i] - f[a]) - (f[b] - f[a]) * (x[i] - x[a]);
            // Lower hull: pop while right-turn (cross ≤ 0). Upper: pop
            // while left-turn (cross ≥ 0).
            if (upper && cross >= 0.0) || (!upper && cross <= 0.0) {
                h.pop();
            } else {
                break;
            }
        }
        h.push(i);
    }
    let mut gap: f64 = 0.0;
    let mut seg = 0;
    for i in 0..n {
        while seg + 1 < h.len() && x[h[seg + 1]] <= x[i] {
            seg += 1;
        }
        let (a, b) = (h[seg], h[(seg + 1).min(h.len() - 1)]);
        let yh = if x[b] > x[a] {
            f[a] + (f[b] - f[a]) * (x[i] - x[a]) / (x[b] - x[a])
        } else {
            f[a]
        };
        let d = if upper { yh - f[i] } else { f[i] - yh };
        gap = gap.max(d);
    }
    gap
}

/// 5% critical values of the dip statistic (Hartigan & Hartigan 1985,
/// Table 1), for the n we care about. Linearly interpolated between
/// table points; clamped to the n=5 / n=30 endpoints outside.
const CRIT_05: &[(usize, f64)] = &[
    (5, 0.1810),
    (10, 0.1179),
    (15, 0.0954),
    (20, 0.0830),
    (25, 0.0742),
    (30, 0.0679),
    (32, 0.0659),
];

fn crit_05(n: usize) -> f64 {
    if n <= CRIT_05[0].0 {
        return CRIT_05[0].1;
    }
    for w in CRIT_05.windows(2) {
        let ((n0, c0), (n1, c1)) = (w[0], w[1]);
        if n <= n1 {
            let t = (n - n0) as f64 / (n1 - n0) as f64;
            return c0 + t * (c1 - c0);
        }
    }
    CRIT_05[CRIT_05.len() - 1].1
}

/// `true` ⇔ dip(x) exceeds the 5% critical value for `x.len()` —
/// reject unimodality at p<0.05. n<5 → always `false` (table floor).
pub fn is_multimodal(x: &[f64]) -> bool {
    let n = x.len();
    n >= 5 && dip(x) > crit_05(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bimodal_clusters_reject_unimodality() {
        // Two tight clusters at ±0.3 — textbook bimodal.
        let mut r = Vec::new();
        for i in 0..10 {
            r.push(-0.3 + 0.001 * i as f64);
            r.push(0.3 + 0.001 * i as f64);
        }
        let d = dip(&r);
        assert!(d > crit_05(20), "dip={d} crit={}", crit_05(20));
        assert!(is_multimodal(&r));
    }

    #[test]
    fn unimodal_gaussian_is_not_flagged() {
        // 20 quantiles of N(0, 0.1): symmetric, single bump.
        let r: Vec<f64> = (1..=20).map(|i| 0.1 * inv_norm(i as f64 / 21.0)).collect();
        let d = dip(&r);
        assert!(d <= crit_05(20), "dip={d} crit={}", crit_05(20));
        assert!(!is_multimodal(&r));
    }

    #[test]
    fn uniform_is_not_flagged() {
        let r: Vec<f64> = (0..20).map(|i| i as f64 / 19.0).collect();
        // Uniform ECDF is exactly its own GCM=LCM → dip ≈ 0.
        assert!(dip(&r) < 0.02);
        assert!(!is_multimodal(&r));
    }

    #[test]
    fn degenerate_inputs() {
        assert_eq!(dip(&[]), 0.0);
        assert_eq!(dip(&[1.0, 1.0, 1.0, 1.0]), 0.0);
        assert!(!is_multimodal(&[0.0, 0.1, 0.2]));
    }

    #[test]
    fn crit_interpolates() {
        assert!((crit_05(5) - 0.1810).abs() < 1e-9);
        assert!((crit_05(32) - 0.0659).abs() < 1e-9);
        let mid = crit_05(12);
        assert!(mid < crit_05(10) && mid > crit_05(15));
    }

    /// Acklam's rational approximation to Φ⁻¹ — test-only, ~1e-9 rel
    /// error on (0,1). Avoids a dev-dep just for one fixture.
    fn inv_norm(p: f64) -> f64 {
        let a = [
            -3.969_683_028_665_376e1,
            2.209_460_984_245_205e2,
            -2.759_285_104_469_687e2,
            1.383_577_518_672_69e2,
            -3.066_479_806_614_716e1,
            2.506_628_277_459_239,
        ];
        let b = [
            -5.447_609_879_822_406e1,
            1.615_858_368_580_409e2,
            -1.556_989_798_598_866e2,
            6.680_131_188_771_972e1,
            -1.328_068_155_288_572e1,
        ];
        let q = p - 0.5;
        let r = q * q;
        q * (((((a[0] * r + a[1]) * r + a[2]) * r + a[3]) * r + a[4]) * r + a[5])
            / (((((b[0] * r + b[1]) * r + b[2]) * r + b[3]) * r + b[4]) * r + 1.0)
    }
}
