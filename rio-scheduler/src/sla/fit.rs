// TODO(ADR-023): drop once Phase 2 lands
#![allow(dead_code)]

use std::time::Duration;

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
}
