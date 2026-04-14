// TODO(ADR-023): drop once Phase 2 lands
#![allow(dead_code)]

use std::time::Duration;

use nalgebra::{DMatrix, DVector};

use super::types::{DurationFit, RawCores, RefSeconds};

// r[impl sched.sla.fit-nnls]  (weights are part of the fit contract)
pub fn sample_weight(age: Duration, halflife_secs: f64, vdist: u32) -> f64 {
    0.5f64.powf(age.as_secs_f64() / halflife_secs) * 0.5f64.powi(vdist as i32)
}

pub fn kish_n_eff(w: &[f64]) -> f64 {
    let s: f64 = w.iter().sum();
    let s2: f64 = w.iter().map(|x| x * x).sum();
    if s2 == 0.0 { 0.0 } else { s * s / s2 }
}

/// Ordinal version-distance: count of distinct version strings between sample and current.
/// `samples` must be sorted by completed_at ascending. Returns vec same length as samples.
pub fn compute_vdists(versions: &[Option<String>], current: Option<&str>) -> Vec<u32> {
    // Walk from newest to oldest, counting distinct versions seen (excluding current).
    let mut seen = std::collections::HashSet::new();
    if let Some(c) = current {
        seen.insert(c.to_string());
    }
    let base = usize::from(current.is_some());
    let mut out = vec![0u32; versions.len()];
    for (i, v) in versions.iter().enumerate().rev() {
        if let Some(v) = v {
            seen.insert(v.clone());
        }
        out[i] = (seen.len() - base) as u32;
    }
    out
}

/// Lawson-Hanson active-set NNLS: min ||Ax - b||² s.t. x ≥ 0.
fn nnls(a: &DMatrix<f64>, b: &DVector<f64>) -> DVector<f64> {
    let (_, m) = a.shape();
    let mut x = DVector::zeros(m);
    let mut passive = vec![false; m];
    for _ in 0..(3 * m) {
        let r = b - a * &x;
        let w = a.transpose() * &r;
        let Some((j, &wj)) = w
            .iter()
            .enumerate()
            .filter(|(i, _)| !passive[*i])
            .max_by(|a, b| a.1.total_cmp(b.1))
        else {
            break;
        };
        if wj <= 1e-10 {
            break;
        }
        passive[j] = true;
        loop {
            let cols: Vec<usize> = (0..m).filter(|i| passive[*i]).collect();
            let ap = a.select_columns(&cols);
            let z_p = ap
                .clone()
                .svd(true, true)
                .solve(b, 1e-10)
                .expect("svd solve");
            if z_p.iter().all(|&v| v > 1e-10) {
                for (k, &ci) in cols.iter().enumerate() {
                    x[ci] = z_p[k];
                }
                break;
            }
            let alpha = cols
                .iter()
                .enumerate()
                .filter(|(k, _)| z_p[*k] <= 0.0)
                .map(|(k, &ci)| x[ci] / (x[ci] - z_p[k]))
                .fold(f64::INFINITY, f64::min);
            for (k, &ci) in cols.iter().enumerate() {
                x[ci] += alpha * (z_p[k] - x[ci]);
                if x[ci] <= 1e-10 {
                    passive[ci] = false;
                    x[ci] = 0.0;
                }
            }
        }
    }
    x
}

/// Fit T(c) = S + P/min(c,p̄) + Q·c via weighted NNLS.
/// Design matrix column-normalized; weights applied as sqrt(w_i) row-scaling.
pub fn fit_duration(
    cs: &[f64],
    ts: &[f64],
    w: &[f64],
    unfreeze_q: bool,
    p_bar: f64,
) -> DurationFit {
    debug_assert_eq!(cs.len(), ts.len());
    debug_assert_eq!(cs.len(), w.len());
    let n = cs.len();
    let cols = if unfreeze_q { 3 } else { 2 };
    let mut a = DMatrix::zeros(n, cols);
    let mut b = DVector::zeros(n);
    for i in 0..n {
        let sw = w[i].sqrt();
        a[(i, 0)] = sw;
        a[(i, 1)] = sw / cs[i].min(p_bar);
        if unfreeze_q {
            a[(i, 2)] = sw * cs[i];
        }
        b[i] = sw * ts[i];
    }
    let norms: Vec<f64> = (0..cols).map(|j| a.column(j).norm().max(1e-12)).collect();
    for j in 0..cols {
        a.column_mut(j).scale_mut(1.0 / norms[j]);
    }
    let x = nnls(&a, &b);
    let s = RefSeconds(x[0] / norms[0]);
    let p = RefSeconds(x[1] / norms[1]);
    if unfreeze_q {
        DurationFit::Usl {
            s,
            p,
            q: x[2] / norms[2],
            p_bar: RawCores(p_bar),
        }
    } else if p_bar.is_finite() {
        DurationFit::Capped {
            s,
            p,
            p_bar: RawCores(p_bar),
        }
    } else {
        DurationFit::Amdahl { s, p }
    }
}

/// Log-residual sigma: stddev of ln(obs/fit) weighted by w_i.
pub fn sigma_resid(cs: &[f64], ts: &[f64], w: &[f64], fit: &DurationFit) -> f64 {
    let lr: Vec<f64> = cs
        .iter()
        .zip(ts)
        .map(|(&c, &t)| (t / fit.t_at(RawCores(c)).0).ln())
        .collect();
    let sw: f64 = w.iter().sum();
    let mean: f64 = lr.iter().zip(w).map(|(r, wi)| r * wi).sum::<f64>() / sw;
    let var: f64 = lr
        .iter()
        .zip(w)
        .map(|(r, wi)| wi * (r - mean).powi(2))
        .sum::<f64>()
        / sw;
    var.sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kish_n_eff_uniform() {
        assert!((kish_n_eff(&[1.0; 10]) - 10.0).abs() < 1e-9);
    }

    #[test]
    fn kish_n_eff_one_dominant() {
        assert!(kish_n_eff(&[100.0, 1.0, 1.0, 1.0]) < 2.0);
    }

    #[test]
    fn sample_weight_halflife() {
        let w = sample_weight(Duration::from_secs(7 * 86400), 7.0 * 86400.0, 1);
        assert!((w - 0.25).abs() < 1e-6);
    }

    #[test]
    fn vdists_ordinal() {
        let v = vec![
            Some("1.0".into()),
            Some("1.0".into()),
            Some("1.1".into()),
            Some("2.0".into()),
        ];
        let d = compute_vdists(&v, Some("2.0"));
        assert_eq!(d, vec![2, 2, 1, 0]); // 2.0 is current; 1.1 is 1 away; 1.0 is 2 away
    }

    #[test]
    fn nnls_recovers_amdahl_exact() {
        let cs = [4.0, 8.0, 16.0, 32.0];
        let ts: Vec<f64> = cs.iter().map(|c| 30.0 + 2000.0 / c).collect();
        let w = vec![1.0; 4];
        let DurationFit::Amdahl { s, p } = fit_duration(&cs, &ts, &w, false, f64::INFINITY) else {
            panic!()
        };
        assert!((s.0 - 30.0).abs() / 30.0 < 0.01);
        assert!((p.0 - 2000.0).abs() / 2000.0 < 0.01);
    }

    #[test]
    fn nnls_recovers_usl() {
        let cs = [2.0, 4.0, 8.0, 16.0, 32.0, 64.0];
        let ts: Vec<f64> = cs.iter().map(|c| 30.0 + 2000.0 / c + 0.5 * c).collect();
        let w = vec![1.0; 6];
        let DurationFit::Usl { s, p, q, .. } = fit_duration(&cs, &ts, &w, true, f64::INFINITY)
        else {
            panic!()
        };
        assert!((s.0 - 30.0).abs() < 2.0);
        assert!((p.0 - 2000.0).abs() < 50.0);
        assert!((q - 0.5).abs() < 0.05);
    }

    #[test]
    fn nnls_nonneg_constraint() {
        // Data that would fit negative S in unconstrained LS → NNLS should clamp S=0
        let cs = [4.0, 8.0, 16.0];
        let ts: Vec<f64> = cs.iter().map(|c| 1000.0 / c - 5.0).collect(); // S would be -5
        let w = vec![1.0; 3];
        let DurationFit::Amdahl { s, .. } = fit_duration(&cs, &ts, &w, false, f64::INFINITY) else {
            panic!()
        };
        assert!(s.0 >= 0.0);
    }

    #[test]
    fn sigma_resid_of_perfect_fit_near_zero() {
        let cs = [4.0, 8.0, 16.0, 32.0];
        let ts: Vec<f64> = cs.iter().map(|c| 30.0 + 2000.0 / c).collect();
        let w = vec![1.0; 4];
        let fit = fit_duration(&cs, &ts, &w, false, f64::INFINITY);
        assert!(sigma_resid(&cs, &ts, &w, &fit) < 1e-3);
    }
}
