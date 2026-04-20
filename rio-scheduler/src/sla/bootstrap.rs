//! Weighted-case-pairs bootstrap for the T_min 80% confidence interval.
//!
//! Each bootstrap replicate draws `n` indices from the sample set with
//! probability ∝ `w_i` (the recency × version-distance weight already
//! computed in `ingest::refit`), refits T(c) on the resampled set with
//! UNIT weights (resampling encodes the prior weights — re-weighting
//! the refit would double-count), and records that replicate's T_min.
//! The 10th/90th percentiles of the T_min distribution form the 80% CI.
//!
//! Seeded deterministically from sample content so the CI is
//! reproducible across estimator ticks for an unchanged ring buffer.

use std::collections::HashSet;

use rand::SeedableRng;
use rand::distr::{Distribution, weighted::WeightedIndex};
use rand::rngs::StdRng;

use super::fit::fit_duration;
use super::types::{RawCores, RefSeconds};

/// One bootstrap input: `(c, t)` with prior weight `w`.
pub struct WeightedSample {
    pub c: f64,
    pub t: f64,
    pub w: f64,
}

/// 80% bootstrap CI on T_min = T(min(p̄, c_opt)). `None` when fewer than
/// 100 replicates produced a valid fit (rank-deficient resamples — i.e.
/// every draw landed on the same c — are skipped).
///
/// `reps`: bootstrap replicates (500 in production).
/// `p_bar`: parallelism cap to evaluate T at; `f64::INFINITY` for an
/// uncapped Amdahl fit (T_min → S).
/// `unfreeze_q`: forward to [`fit_duration`] so replicates fit the same
/// model class as the outer fit — a USL outer fit with Amdahl
/// replicates evaluates at `p̄` instead of `c_opt` and biases the CI
/// low by ~`2√(PQ)`.
// r[impl sched.sla.reassign-schmitt]
pub fn t_min_ci(
    samples: &[WeightedSample],
    reps: usize,
    p_bar: f64,
    unfreeze_q: bool,
) -> Option<(RefSeconds, RefSeconds)> {
    let dist = WeightedIndex::new(samples.iter().map(|s| s.w)).ok()?;
    // Seed from sample content so CI is reproducible across ticks for an
    // unchanged ring; folds (c,t) bits — w omitted because it shifts on
    // every refit (age-dependent) and would defeat reproducibility.
    let seed: u64 = samples
        .iter()
        .map(|s| s.c.to_bits() ^ s.t.to_bits())
        .fold(0, |a, b| a.wrapping_add(b));
    let mut rng = StdRng::seed_from_u64(seed);
    let n = samples.len();
    let unit_w = vec![1.0; n];

    let mut tmins = Vec::with_capacity(reps);
    for _ in 0..reps {
        let idx: Vec<usize> = (0..n).map(|_| dist.sample(&mut rng)).collect();
        let cs: Vec<f64> = idx.iter().map(|&i| samples[i].c).collect();
        // Rank-deficient: all draws hit the same c → NNLS design has one
        // independent row; skip rather than emit a garbage S.
        let distinct: HashSet<u64> = cs.iter().map(|c| c.to_bits()).collect();
        if distinct.len() < 2 {
            continue;
        }
        let ts: Vec<f64> = idx.iter().map(|&i| samples[i].t).collect();
        let fit = fit_duration(&cs, &ts, &unit_w, unfreeze_q, p_bar);
        let c_eval = p_bar.min(fit.c_opt().0);
        tmins.push(fit.t_at(RawCores(c_eval)).0);
    }

    if tmins.len() < 100 {
        return None;
    }
    tmins.sort_by(|a, b| a.total_cmp(b));
    let lo = tmins[tmins.len() / 10];
    let hi = tmins[tmins.len() * 9 / 10];
    Some((RefSeconds(lo), RefSeconds(hi)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ci_brackets_true_tmin() {
        // T(c) = 30 + 2000/c, noiseless. True T_min (Amdahl, p̄=∞) is S=30.
        let samples: Vec<_> = [4.0, 8.0, 16.0, 32.0]
            .iter()
            .map(|&c| WeightedSample {
                c,
                t: 30.0 + 2000.0 / c,
                w: 1.0,
            })
            .collect();
        let (lo, hi) = t_min_ci(&samples, 500, f64::INFINITY, false).unwrap();
        assert!(lo.0 < 35.0, "lo={} should bracket 30 from below", lo.0);
        assert!(hi.0 > 28.0, "hi={} should bracket 30 from above", hi.0);
        assert!(lo.0 <= hi.0);
    }

    #[test]
    fn rank_deficient_returns_none() {
        // All samples at c=4 → every resample is rank-deficient → 0 valid
        // replicates → None.
        let samples: Vec<_> = (0..8)
            .map(|_| WeightedSample {
                c: 4.0,
                t: 100.0,
                w: 1.0,
            })
            .collect();
        assert!(t_min_ci(&samples, 500, f64::INFINITY, false).is_none());
    }

    #[test]
    fn deterministic_for_same_input() {
        let samples: Vec<_> = [4.0, 8.0, 16.0, 32.0]
            .iter()
            .map(|&c| WeightedSample {
                c,
                t: 30.0 + 2000.0 / c,
                w: 1.0,
            })
            .collect();
        let a = t_min_ci(&samples, 500, f64::INFINITY, false).unwrap();
        let b = t_min_ci(&samples, 500, f64::INFINITY, false).unwrap();
        assert_eq!(a.0.0.to_bits(), b.0.0.to_bits());
        assert_eq!(a.1.0.to_bits(), b.1.0.to_bits());
    }

    #[test]
    fn empty_input_returns_none() {
        assert!(t_min_ci(&[], 500, f64::INFINITY, false).is_none());
    }

    #[test]
    fn ci_brackets_usl_tmin() {
        // T(c) = 30 + 2000/c + 0.5·c. True c_opt = √(2000/0.5) ≈ 63.2,
        // T_min ≈ 93.2. With unfreeze_q=false replicates fit Amdahl and
        // evaluate at p̄=∞ → T(∞)≈30, CI ~[28,35] — does NOT bracket 93.
        let cs = [
            4.0, 6.0, 8.0, 12.0, 16.0, 24.0, 32.0, 40.0, 48.0, 56.0, 60.0, 64.0,
        ];
        let samples: Vec<_> = cs
            .iter()
            .map(|&c| WeightedSample {
                c,
                t: 30.0 + 2000.0 / c + 0.5 * c,
                w: 1.0,
            })
            .collect();
        let (lo, hi) = t_min_ci(&samples, 500, f64::INFINITY, true).unwrap();
        assert!(lo.0 > 60.0, "lo={} (Amdahl replicates → ~30)", lo.0);
        assert!(hi.0 > 80.0, "hi={} must approach T_min≈93.25", hi.0);
        let t_min = 30.0 + 2.0 * (2000.0_f64 * 0.5).sqrt();
        assert!(
            lo.0 <= t_min + 1.0 && hi.0 >= t_min - 1.0,
            "[{}, {}] brackets T_min={t_min}",
            lo.0,
            hi.0
        );
    }
}
