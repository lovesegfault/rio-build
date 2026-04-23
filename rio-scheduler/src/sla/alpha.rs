//! Per-pname K=3 hardware mixture α ∈ Δ² (ADR-023 §Hardware heterogeneity).
//!
//! [`super::hw::HwTable`] gives a per-hw_class K=3 microbench vector
//! `factor[h] = [alu, membw, ioseq]`. The effective scalar speedup for a
//! pname on hardware `h` is the dot product `α[pname] · factor[h]`, with
//! `α` on the probability simplex Δ^{K−1}. An I/O-bound pname converges
//! to `α ≈ [0, 0, 1]` and routes to `storage=nvme`; a compute-bound one
//! to `α ≈ [1, 0, 0]` and routes to fast-core generations.
//!
//! `α` and the `T_ref(c)` NNLS curve are jointly estimated by [`als_fit`]:
//! a bounded alternating-least-squares that flips between
//! [`super::fit::fit_duration_staged`] (NNLS on `wall · (α·factor)`) and
//! [`fit_alpha`] (simplex-projected gradient descent on the
//! c-residualized speedup `T_ref(c)/wall ≈ α·factor`). The alternation is
//! heuristic — neither block is closed-form descent on a single
//! objective — so it is hard-capped at 5 rounds; non-convergence is
//! emitted as `rio_scheduler_sla_als_round_cap_hit_total` and is a
//! §Phasing-13a empirical gate.

use super::fit::{StageGate, fit_duration_staged};
use super::hw::{HW_FACTOR_SANITY_CEIL, HW_FACTOR_SANITY_FLOOR, K};
use super::types::{DurationFit, RawCores};

/// Per-pname mixture weight on the K=3 simplex. `Σα = 1, α_d ≥ 0`.
pub type Alpha = [f64; K];

/// Uniform prior `[1/K; K]`. ADR-023 L547 last-resort fallback (after
/// per-pname I/O-saturation seed and fleet-median, plumbed via
/// [`super::prior::FitParams`]). Also the operator-probe basis
/// `clamp_to_operator` bands fleet-α against, and the value α stays at
/// when the rank gate never passes — keeping the model at the phase-10
/// scalar behaviour.
pub const UNIFORM: Alpha = [1.0 / K as f64; K];

/// `α · factor`, clamped to the same `[FLOOR, CEIL]` band as the scalar
/// path. The clamp lives here (not at the `factor()` accessor) so the
/// per-dimension bounds compose: `dot([1,0,0], [4,4,4]) = 4` and
/// `dot([⅓,⅓,⅓], [4,0.25,0.25]) = 1.5` both stay inside `[0.25, 4]`
/// without per-dim pre-clamping pulling the latter to a different value.
#[inline]
pub fn dot(alpha: Alpha, factor: [f64; K]) -> f64 {
    alpha
        .iter()
        .zip(factor)
        .map(|(a, f)| a * f)
        .sum::<f64>()
        .clamp(HW_FACTOR_SANITY_FLOOR, HW_FACTOR_SANITY_CEIL)
}

/// One row of the simplex-LS design: c-residualized speedup ≈ α·factor.
#[derive(Debug, Clone, Copy)]
pub struct AlphaSample {
    /// `exp(−ε̂_i) = T_ref(c_i) / wall_i`. c-independent in expectation
    /// at the ALS fixed point (ADR-023 L549 deconfounding).
    pub speedup_residual: f64,
    /// `factor[h_i]` — bench'd hw_class only (NULL/unbench'd rows are
    /// excluded; their `factor=[1;K]` is redundant with the simplex
    /// constraint and would mis-pair the true h's residual with the
    /// reference vector).
    pub factor: [f64; K],
    /// Per-sample weight (same ordinal/vdist weight as the NNLS).
    pub weight: f64,
}

/// Ridge weight `λ_α` toward the prior. Capped so the ridge "never
/// outweighs a single observed sample" (ADR-023 L545): with K=3 factors
/// in `[0.25, 4]` the per-sample gradient norm is `O(10)`; `λ=0.1`
/// contributes ≤ `0.2 · ‖α−prior‖ ≤ 0.2`.
pub const LAMBDA_RIDGE: f64 = 0.1;

/// Max ALS rounds (ADR-023 L555).
pub const ALS_MAX_ROUNDS: u8 = 5;
/// ALS convergence threshold on `‖Δα‖₁` (ADR-023 L555).
pub const ALS_DELTA_TOL: f64 = 1e-2;

/// `cos(5°)`. Pairwise centred-angle gate for [`fit_alpha`]'s rank check.
const THETA_MIN_COS: f64 = 0.996_194_7;

/// Euclidean projection onto the probability simplex Δ^{K−1}
/// (Duchi et al. 2008, Fig. 1). O(K log K).
pub fn simplex_project(v: [f64; K]) -> Alpha {
    let mut u = v;
    u.sort_unstable_by(|a, b| b.total_cmp(a));
    let mut cum = 0.0;
    let mut rho = 0;
    for (i, &ui) in u.iter().enumerate() {
        cum += ui;
        if ui + (1.0 - cum) / (i as f64 + 1.0) > 0.0 {
            rho = i;
        }
    }
    let cum_rho: f64 = u[..=rho].iter().sum();
    let tau = (cum_rho - 1.0) / (rho as f64 + 1.0);
    v.map(|x| (x - tau).max(0.0))
}

/// Identifiability gate (ADR-023 L543): ≥2 distinct factor vectors with
/// pairwise angular spread > 5° after centring (subtracting the per-
/// vector mean projects out the `(1,…,1)` direction the simplex
/// constraint already supplies). Below the gate the simplex-LS is
/// under-determined and the active set picks an arbitrary vertex.
fn rank_gate(samples: &[AlphaSample]) -> bool {
    let mut distinct: Vec<[f64; K]> = samples.iter().map(|s| s.factor).collect();
    distinct.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    distinct.dedup_by(|a, b| a.iter().zip(&*b).all(|(x, y)| (x - y).abs() < 1e-6));
    if distinct.len() < 2 {
        return false;
    }
    let centre = |v: [f64; K]| -> [f64; K] {
        let m = v.iter().sum::<f64>() / K as f64;
        v.map(|x| x - m)
    };
    let cos = |a: [f64; K], b: [f64; K]| -> f64 {
        let d: f64 = a.iter().zip(b).map(|(x, y)| x * y).sum();
        let na: f64 = a.iter().map(|x| x * x).sum::<f64>().sqrt();
        let nb: f64 = b.iter().map(|x| x * x).sum::<f64>().sqrt();
        if na * nb < 1e-12 { 1.0 } else { d / (na * nb) }
    };
    for i in 0..distinct.len() {
        for j in (i + 1)..distinct.len() {
            if cos(centre(distinct[i]), centre(distinct[j])).abs() < THETA_MIN_COS {
                return true;
            }
        }
    }
    false
}

// r[impl sched.sla.hw-class.alpha-als]
/// Simplex-constrained weighted LS via projected gradient descent:
///
/// `min_{α∈Δ} Σ w_i (α·factor_i − y_i)² + λ‖α−α_prior‖²`
///
/// Returns `prior` unchanged when the rank gate fails. The Dirichlet-MAP
/// ridge keeps within-family near-collinear (alu, membw) pairs from
/// running off to a vertex. K=3 / ≤200 iters / ≤32 samples → <50µs.
pub fn fit_alpha(samples: &[AlphaSample], prior: Alpha, lambda_ridge: f64) -> Alpha {
    if !rank_gate(samples) {
        return prior;
    }
    // PGD step size 1/L with L = ‖∇²J‖₂. The Hessian is 2·FᵀWF + 2λI;
    // bound its spectral norm by the trace `2·Σ w_i‖f_i‖² + 2λ` (tight
    // enough at K=3). Guarantees monotone descent without line search.
    let lip: f64 = 2.0
        * samples
            .iter()
            .map(|s| s.weight * s.factor.iter().map(|f| f * f).sum::<f64>())
            .sum::<f64>()
        + 2.0 * lambda_ridge;
    let lr = 1.0 / lip.max(1e-6);
    let mut alpha = prior;
    for _ in 0..500 {
        let mut grad = [0.0; K];
        for s in samples {
            let pred: f64 = alpha.iter().zip(s.factor).map(|(a, f)| a * f).sum();
            let err = pred - s.speedup_residual;
            for (d, g) in grad.iter_mut().enumerate() {
                *g += 2.0 * s.weight * err * s.factor[d];
            }
        }
        for (d, g) in grad.iter_mut().enumerate() {
            *g += 2.0 * lambda_ridge * (alpha[d] - prior[d]);
        }
        let step: [f64; K] = std::array::from_fn(|d| alpha[d] - lr * grad[d]);
        let next = simplex_project(step);
        let delta: f64 = next.iter().zip(alpha).map(|(n, a)| (n - a).abs()).sum();
        alpha = next;
        if delta < 1e-6 {
            break;
        }
    }
    alpha
}

// r[impl sched.sla.hw-class.alpha-als]
/// Bounded ALS: alternate `(NNLS on wall·(α·factor[h]))` ↔ `(fit_alpha on
/// T_ref(c)/wall)` until `‖Δα‖₁ < 10⁻²` or [`ALS_MAX_ROUNDS`].
///
/// `factors[i] = None` for NULL / unbench'd hw_class: those rows feed the
/// NNLS step at `factor := [1;K]` (bias-neutral pass-through, ADR-023
/// L539) but are excluded from the α-step (L541). When *every* row is
/// `None` — or the inner [`fit_alpha`] rank-gate fails — α stays at
/// `prior` and the result is the plain `fit_duration_staged` output, so
/// this is a strict generalization of the pre-ALS path.
///
/// Returns `(T_ref fit, σ_resid, α, rounds)`. `rounds == ALS_MAX_ROUNDS`
/// ⇔ cap hit; caller emits `_als_round_cap_hit_total{tenant}`.
pub fn als_fit(
    cs: &[f64],
    walls: &[f64],
    factors: &[Option<[f64; K]>],
    w: &[f64],
    gate: &StageGate,
    prior: Alpha,
) -> (DurationFit, f64, Alpha, u8) {
    debug_assert_eq!(cs.len(), walls.len());
    debug_assert_eq!(cs.len(), factors.len());
    debug_assert_eq!(cs.len(), w.len());
    let mut alpha = prior;
    let mut fit = DurationFit::Probe;
    let mut sigma = 0.2;
    for round in 0..ALS_MAX_ROUNDS {
        // ── Step A: NNLS on wall_i · (α · factor[h_i]) ───────────────────
        let ts: Vec<f64> = walls
            .iter()
            .zip(factors)
            .map(|(&wall, f)| wall * dot(alpha, f.unwrap_or([1.0; K])))
            .collect();
        (fit, sigma) = fit_duration_staged(cs, &ts, w, gate);
        // ── Step B: simplex-LS on T_ref(c_i)/wall_i ≈ α · factor[h_i] ────
        // Bench'd-only; residual must be positive-finite (Probe → ∞).
        let samples: Vec<AlphaSample> = cs
            .iter()
            .zip(walls)
            .zip(factors)
            .zip(w)
            .filter_map(|(((&c, &wall), f), &weight)| {
                let factor = (*f)?;
                let t_ref = fit.t_at(RawCores(c)).0;
                (t_ref.is_finite() && wall > 0.0).then_some(AlphaSample {
                    speedup_residual: t_ref / wall,
                    factor,
                    weight,
                })
            })
            .collect();
        let next = fit_alpha(&samples, prior, LAMBDA_RIDGE);
        let delta: f64 = next.iter().zip(alpha).map(|(n, a)| (n - a).abs()).sum();
        alpha = next;
        if delta < ALS_DELTA_TOL {
            return (fit, sigma, alpha, round + 1);
        }
    }
    (fit, sigma, alpha, ALS_MAX_ROUNDS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn on_simplex(a: Alpha) -> bool {
        (a.iter().sum::<f64>() - 1.0).abs() < 1e-9 && a.iter().all(|&x| x >= 0.0)
    }

    fn s(y: f64, f: [f64; K]) -> AlphaSample {
        AlphaSample {
            speedup_residual: y,
            factor: f,
            weight: 1.0,
        }
    }

    #[test]
    fn simplex_project_stays_on_simplex() {
        for v in [
            [0.7, 0.5, -0.1],
            [-5.0, -5.0, -5.0],
            [10.0, 0.0, 0.0],
            [1.0 / 3.0; 3],
            [0.0, 0.0, 0.0],
        ] {
            let p = simplex_project(v);
            assert!(on_simplex(p), "{v:?} → {p:?}");
        }
        // Already on simplex → idempotent.
        let p = simplex_project([0.2, 0.3, 0.5]);
        assert!((p[0] - 0.2).abs() < 1e-12 && (p[2] - 0.5).abs() < 1e-12);
    }

    // r[verify sched.sla.hw-class.alpha-als]
    #[test]
    fn alpha_recovers_pure_alu() {
        // 4 hw_classes with linearly-independent factor vecs; pname is
        // pure-ALU (true α=[1,0,0]) so speedup_residual = factor.alu.
        let factors = [
            [1.0, 1.0, 1.0],
            [1.5, 1.2, 0.9],
            [1.8, 1.0, 1.1],
            [1.3, 2.0, 1.5],
        ];
        let samples: Vec<_> = factors.iter().map(|f| s(f[0], *f)).collect();
        let alpha = fit_alpha(&samples, UNIFORM, 0.01);
        assert!((alpha[0] - 1.0).abs() < 0.05, "α = {alpha:?}");
        assert!(alpha[1] < 0.05 && alpha[2] < 0.05, "α = {alpha:?}");
    }

    // r[verify sched.sla.hw-class.alpha-als]
    #[test]
    fn alpha_stays_at_prior_below_rank_gate() {
        let prior = [0.6, 0.3, 0.1];
        // 1 distinct vec → < 2.
        let one = vec![s(1.5, [1.5, 1.2, 0.9])];
        assert_eq!(fit_alpha(&one, prior, 0.01), prior);
        // 2 vecs, parallel after centring (one is isotropic [v;K] →
        // centred = 0): angular spread fails.
        let iso = vec![s(1.0, [1.0, 1.0, 1.0]), s(2.0, [2.0, 2.0, 2.0])];
        assert_eq!(fit_alpha(&iso, prior, 0.01), prior);
        // Empty.
        assert_eq!(fit_alpha(&[], prior, 0.01), prior);
    }

    /// Ridge pulls toward the prior; with λ→0 the fit lands on the LS
    /// optimum, with λ large it stays near the prior.
    #[test]
    fn alpha_ridge_pulls_toward_prior() {
        let factors = [[1.5, 1.2, 0.9], [1.8, 1.0, 1.1], [1.3, 2.0, 1.5]];
        let samples: Vec<_> = factors.iter().map(|f| s(f[0], *f)).collect();
        let free = fit_alpha(&samples, UNIFORM, 0.0);
        let ridged = fit_alpha(&samples, UNIFORM, 1e4);
        let d_free: f64 = free.iter().zip(UNIFORM).map(|(a, p)| (a - p).abs()).sum();
        let d_ridged: f64 = ridged.iter().zip(UNIFORM).map(|(a, p)| (a - p).abs()).sum();
        assert!(d_ridged < d_free, "ridge → closer to prior");
        assert!(d_ridged < 0.05, "λ=1e4 ≈ prior: {ridged:?}");
    }

    #[test]
    fn dot_clamped() {
        assert_eq!(dot([1.0, 0.0, 0.0], [1e6, 1.0, 1.0]), HW_FACTOR_SANITY_CEIL);
        assert_eq!(
            dot([1.0, 0.0, 0.0], [1e-6, 1.0, 1.0]),
            HW_FACTOR_SANITY_FLOOR
        );
        // Isotropic factor [v;K] · any-simplex-α = v.
        assert!((dot([0.2, 0.3, 0.5], [1.7; K]) - 1.7).abs() < 1e-12);
    }

    // r[verify sched.sla.hw-class.alpha-als]
    #[test]
    fn als_converges_within_5_rounds() {
        // True T_ref(c) = 30 + 2000/c on 4 hw_classes; table-driven over
        // 3 ground-truth α. ALS must recover (S, P, α) and converge < 5.
        let factors: [[f64; K]; 4] = [
            [1.0, 1.0, 1.0],
            [1.6, 1.1, 0.8],
            [1.2, 1.9, 1.0],
            [0.9, 1.0, 2.2],
        ];
        let cs: Vec<f64> = [4.0, 8.0, 16.0, 32.0, 64.0]
            .into_iter()
            .cycle()
            .take(20)
            .collect();
        for true_alpha in [[1.0, 0.0, 0.0], [0.0, 0.5, 0.5], [0.2, 0.5, 0.3]] {
            let mut walls = Vec::new();
            let mut hf = Vec::new();
            for (i, &c) in cs.iter().enumerate() {
                let f = factors[i % 4];
                let scale: f64 = true_alpha.iter().zip(f).map(|(a, x)| a * x).sum();
                walls.push((30.0 + 2000.0 / c) / scale);
                hf.push(Some(f));
            }
            let w = vec![1.0; cs.len()];
            let gate = StageGate {
                n_eff: 20.0,
                span: 16.0,
                p_bar: f64::INFINITY,
                prev_usl: false,
            };
            let (fit, sigma, alpha, rounds) = als_fit(&cs, &walls, &hf, &w, &gate, UNIFORM);
            assert!(
                rounds < ALS_MAX_ROUNDS,
                "α_true={true_alpha:?}: rounds={rounds}"
            );
            assert!(on_simplex(alpha), "{alpha:?}");
            let d: f64 = alpha
                .iter()
                .zip(true_alpha)
                .map(|(a, t)| (a - t).abs())
                .sum();
            assert!(d < 0.12, "α_true={true_alpha:?}: got {alpha:?} (‖Δ‖₁={d})");
            let (s, p, _) = fit.spq();
            assert!((s - 30.0).abs() < 4.0, "S={s}");
            assert!((p - 2000.0).abs() / 2000.0 < 0.05, "P={p}");
            assert!(sigma < 0.05, "near-perfect data σ={sigma}");
        }
    }

    /// All-unbench'd rows (factors=None): α stays at prior, fit equals
    /// plain `fit_duration_staged` on raw wall — the pre-ALS behaviour.
    #[test]
    fn als_degrades_to_staged_when_all_unbenchd() {
        let cs = [4.0, 8.0, 16.0, 32.0];
        let walls: Vec<f64> = cs.iter().map(|c| 30.0 + 2000.0 / c).collect();
        let hf = vec![None; 4];
        let w = vec![1.0; 4];
        let gate = StageGate {
            n_eff: 4.0,
            span: 8.0,
            p_bar: f64::INFINITY,
            prev_usl: false,
        };
        let (fit, _, alpha, rounds) = als_fit(&cs, &walls, &hf, &w, &gate, UNIFORM);
        assert_eq!(alpha, UNIFORM);
        assert_eq!(rounds, 1, "rank-gate fails round 1 → Δα=0 → converged");
        let (s, p, _) = fit.spq();
        assert!((s - 30.0).abs() < 1.0 && (p - 2000.0).abs() < 5.0);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        #[test]
        fn simplex_project_always_on_simplex(
            a in -10.0f64..10.0, b in -10.0f64..10.0, c in -10.0f64..10.0,
        ) {
            prop_assert!(on_simplex(simplex_project([a, b, c])));
        }

        // r[verify sched.sla.hw-class.alpha-als]
        #[test]
        fn als_fit_alpha_on_simplex(
            seeds in prop::collection::vec((1u8..5, 0.5f64..3.0, 0.5f64..3.0, 0.5f64..3.0), 4..16),
            a0 in 0.0f64..1.0, a1 in 0.0f64..1.0,
        ) {
            // Arbitrary factor rows + arbitrary ground-truth α → output α
            // is ALWAYS on the simplex (regardless of rank-gate / rounds).
            let true_alpha = simplex_project([a0, a1, 1.0 - a0 - a1]);
            let cs: Vec<f64> = seeds.iter().map(|(c, ..)| f64::from(*c) * 4.0).collect();
            let factors: Vec<Option<[f64; K]>> =
                seeds.iter().map(|(_, x, y, z)| Some([*x, *y, *z])).collect();
            let walls: Vec<f64> = cs.iter().zip(&factors).map(|(c, f)| {
                let scale: f64 = true_alpha.iter().zip(f.unwrap()).map(|(a, x)| a * x).sum();
                (30.0 + 2000.0 / c) / scale.max(0.1)
            }).collect();
            let w = vec![1.0; cs.len()];
            let gate = StageGate {
                n_eff: cs.len() as f64, span: 16.0,
                p_bar: f64::INFINITY, prev_usl: false,
            };
            let (_, _, alpha, rounds) = als_fit(&cs, &walls, &factors, &w, &gate, UNIFORM);
            prop_assert!(on_simplex(alpha), "α={alpha:?}");
            prop_assert!((1..=ALS_MAX_ROUNDS).contains(&rounds));
        }
    }
}
