//! Rebalancer algorithm tests. Pure, no PG.
//!
//! The convergence test carries the verification marker; the rest
//! cover edge cases called out in the module doc (degenerate cutoffs,
//! min_samples gate, n_classes<=1).

use super::*;

/// Deterministic jitter: walks a triangle wave across [0, span).
/// No RNG → no seed management, no cross-platform float divergence.
/// The convergence assertion range is wide enough (see below) that
/// the exact jitter shape doesn't matter; we just need a spread so
/// the bimodal clusters have width.
fn jitter(i: usize, span: f64) -> f64 {
    (i % 10) as f64 * span / 10.0
}

// r[verify sched.rebalancer.sita-e]
#[test]
fn bimodal_converges_to_midpoint_in_3_iters() {
    // 100 samples in [10.0, 11.0) + 100 samples in [300.0, 315.0).
    // Total load ≈ 100*10.5 + 100*307.5 = 31800. Half = 15900.
    // First 100 contribute ~1050, so the raw SITA-E boundary sits
    // ~48 samples into the 300s cluster → durations[148] ≈ 307.
    //
    // EMA α=0.3 starting from 60.0:
    //   iter1: 0.3*307 + 0.7*60   ≈ 134
    //   iter2: 0.3*307 + 0.7*134  ≈ 186
    //   iter3: 0.3*307 + 0.7*186  ≈ 222
    //
    // After 3 iters we're 1-(0.7)^3 ≈ 65.7% of the way from the
    // initial guess to the raw target. The assertion checks the
    // cutoff has CLEARED the 10s cluster (>50) and not overshot the
    // 300s cluster (<310) — "between clusters", per the EMA-tolerance
    // note in the plan doc. Exact equal-load convergence would need
    // more iters or α=1.0.
    let mut samples: Vec<(f64, i64)> = Vec::new();
    for i in 0..100 {
        samples.push((10.0 + jitter(i, 1.0), 0));
    }
    for i in 0..100 {
        samples.push((300.0 + jitter(i, 15.0), 0));
    }

    let cfg = RebalancerConfig {
        min_samples: 50,
        ema_alpha: 0.3,
        lookback_days: 7,
    };
    let mut cutoffs = vec![60.0]; // initial guess, far from either cluster

    for _ in 0..3 {
        let result = compute_cutoffs(&samples, 2, &cutoffs, &cfg).unwrap();
        cutoffs = result.new_cutoffs;
    }

    assert!(
        cutoffs[0] > 50.0 && cutoffs[0] < 310.0,
        "cutoff {:.2} not between clusters after 3 EMA iters",
        cutoffs[0]
    );

    // Monotone convergence: one MORE iter should move strictly closer
    // to the raw target (i.e., cutoff increases, since we started
    // below). Proves the EMA is actually blending, not a no-op.
    let next = compute_cutoffs(&samples, 2, &cutoffs, &cfg).unwrap();
    assert!(
        next.new_cutoffs[0] > cutoffs[0],
        "iter 4 ({:.2}) did not move past iter 3 ({:.2}) — EMA stalled?",
        next.new_cutoffs[0],
        cutoffs[0]
    );

    // Load fractions under the raw (α=1, no-smooth) cutoffs should be
    // ~0.5 each — that's the whole point of SITA-E. This exercises
    // the "shape mismatch → skip smoothing" path (prev_cutoffs=[] has
    // length 0, raw_cutoffs has length 1). Fraction is under the RAW
    // bisect cutoff, so balance should be near-perfect.
    let raw = compute_cutoffs(&samples, 2, &[], &cfg).unwrap();
    // Self-validating precondition: the test's own premise (2 classes →
    // 2 fractions) must hold before the real assert means anything.
    // A threshold-shortcut bug that returned load_fractions=[] would
    // make the indexing panic; this gives a readable failure instead.
    assert_eq!(
        raw.load_fractions.len(),
        2,
        "2 classes must yield 2 fractions"
    );
    for (i, &f) in raw.load_fractions.iter().enumerate() {
        assert!(
            (f - 0.5).abs() < 0.05,
            "raw SITA-E fraction[{i}]={f:.3} not within 0.05 of 0.5"
        );
    }
}

#[test]
fn uniform_samples_give_degenerate_cutoffs() {
    // 200 samples all at exactly 60s. Every bisect hits the same
    // duration → all cutoffs collapse to 60.0. The algorithm must
    // pass this through unchanged (NOT dedupe — P0230's zip would
    // truncate against a shorter vec and leave stale config entries).
    let samples: Vec<(f64, i64)> = vec![(60.0, 0); 200];
    let cfg = RebalancerConfig {
        min_samples: 50,
        ema_alpha: 0.3,
        lookback_days: 7,
    };
    let result = compute_cutoffs(&samples, 3, &[], &cfg).unwrap();

    // 3 classes → 2 cutoffs, both at 60.0.
    assert_eq!(result.new_cutoffs.len(), 2, "no dedup — length preserved");
    for &c in &result.new_cutoffs {
        assert!((c - 60.0).abs() < 0.01, "degenerate cutoff {c} != 60.0");
    }

    // Load fractions: all samples land in class 0 (duration == cutoff
    // → `<` in the partition walk does not advance). Fractions should
    // be [1.0, 0.0, 0.0]. Matches assignment::classify semantics.
    assert_eq!(result.load_fractions.len(), 3);
    assert!((result.load_fractions[0] - 1.0).abs() < 1e-9);
    assert!(result.load_fractions[1].abs() < 1e-9);
    assert!(result.load_fractions[2].abs() < 1e-9);

    // And every fraction is finite — no NaN leaked through.
    for &f in &result.load_fractions {
        assert!(f.is_finite(), "load fraction {f} is not finite");
    }
}

#[test]
fn below_min_samples_returns_none() {
    let samples: Vec<(f64, i64)> = vec![(10.0, 0); 50];
    let cfg = RebalancerConfig {
        min_samples: 100, // 50 < 100
        ema_alpha: 0.3,
        lookback_days: 7,
    };
    assert!(compute_cutoffs(&samples, 2, &[], &cfg).is_none());

    // Boundary: exactly at threshold → proceed.
    let at = vec![(10.0, 0); 100];
    assert!(compute_cutoffs(&at, 2, &[], &cfg).is_some());
}

#[test]
fn single_class_has_no_cutoffs() {
    // n_classes=1 → no boundaries. Trivial result with one load
    // fraction = 1.0 (everything is in the only class).
    let samples: Vec<(f64, i64)> = (0..200).map(|i| (i as f64, 0)).collect();
    let cfg = RebalancerConfig {
        min_samples: 50,
        ..Default::default()
    };
    let result = compute_cutoffs(&samples, 1, &[], &cfg).unwrap();
    assert!(result.new_cutoffs.is_empty());
    assert_eq!(result.load_fractions, vec![1.0]);
    assert_eq!(result.sample_count, 200);
}

#[test]
fn all_zero_durations_uniform_fractions() {
    // Pathological: every sample is 0s (e.g., 200 cache-hit builds
    // that the completion path shouldn't have written but did). total
    // = 0 → naive fraction math is 0/0 = NaN. compute_load_fractions
    // special-cases this to uniform 1/N.
    let samples: Vec<(f64, i64)> = vec![(0.0, 0); 200];
    let cfg = RebalancerConfig {
        min_samples: 50,
        ..Default::default()
    };
    let result = compute_cutoffs(&samples, 4, &[], &cfg).unwrap();

    // 4 classes → 3 cutoffs. Each bisect hits idx 0 (all cumsums are
    // 0, target is 0, partition_point for `c < 0` is 0). So all
    // cutoffs are durations[0] = 0.0.
    assert_eq!(result.new_cutoffs.len(), 3);
    for &c in &result.new_cutoffs {
        assert_eq!(c, 0.0);
    }

    // Fractions uniform, not NaN.
    for &f in &result.load_fractions {
        assert!(f.is_finite());
        assert!((f - 0.25).abs() < 1e-9);
    }
}

#[test]
fn ema_smoothing_blends_toward_raw() {
    // Direct EMA check: make the raw cutoff land at a known value,
    // start prev far away, assert one step moves α of the distance.
    //
    // 100 samples at 100s. Raw cutoff for 2 classes bisects at
    // half-load = idx 49 → 100.0.
    let samples: Vec<(f64, i64)> = vec![(100.0, 0); 100];
    let cfg = RebalancerConfig {
        min_samples: 50,
        ema_alpha: 0.3,
        lookback_days: 7,
    };

    // First-run (no prev) gives raw directly.
    let raw = compute_cutoffs(&samples, 2, &[], &cfg).unwrap();
    assert!((raw.new_cutoffs[0] - 100.0).abs() < 1e-9);

    // With prev=[0.0], one step: 0.3*100 + 0.7*0 = 30.0.
    let step1 = compute_cutoffs(&samples, 2, &[0.0], &cfg).unwrap();
    assert!((step1.new_cutoffs[0] - 30.0).abs() < 1e-9);

    // α=1.0 → no smoothing, raw through.
    let cfg_instant = RebalancerConfig {
        ema_alpha: 1.0,
        ..cfg
    };
    let instant = compute_cutoffs(&samples, 2, &[0.0], &cfg_instant).unwrap();
    assert!((instant.new_cutoffs[0] - 100.0).abs() < 1e-9);
}
