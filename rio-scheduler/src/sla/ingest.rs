//! Per-key refit: `Vec<BuildSampleRow>` → [`FittedParams`].
//!
//! The [`SlaEstimator`](super::SlaEstimator) cache calls [`refit`] once per
//! touched key on each refresh tick; the fit itself is pure (no DB, no I/O)
//! so it can be unit-tested against synthetic rows.

// TODO(ADR-023): drop once Phase 2 lands
#![allow(dead_code)]

use std::collections::HashSet;
use std::time::Duration;

use super::fit::{
    compute_vdists, fit_duration, fit_memory, kish_n_eff, sample_weight, sigma_resid,
    weighted_quantile,
};
use super::types::{
    DiskBytes, DurationFit, ExploreState, FittedParams, MemBytes, MemFit, ModelKey, RawCores,
    WallSeconds,
};
use crate::db::BuildSampleRow;

/// Refit one `(pname, system, tenant)` key from its ring-buffer of recent
/// samples (≤32 rows, completed_at-ascending — `rows.last()` is newest).
///
/// Rows lacking `cpu_limit_cores` are dropped from the fit set entirely:
/// without the control variable a sample can't sit on the T(c) or M(c)
/// curve, and keeping it would desync the parallel `cs`/`ts`/`w` slices
/// (`fit_duration` debug-asserts equal length). Such rows come from old
/// executors / non-k8s test runs / recovered derivations and are rare in
/// steady state.
pub fn refit(key: &ModelKey, rows: &[BuildSampleRow], halflife_secs: f64) -> FittedParams {
    let now = now_epoch();
    // Filter to rows that can sit on a c-axis. Everything below — vdists,
    // weights, n_eff, span, fits — is computed on this consistent subset.
    let fit_rows: Vec<&BuildSampleRow> = rows
        .iter()
        .filter(|r| r.cpu_limit_cores.is_some())
        .collect();

    if fit_rows.is_empty() {
        return probe_only(key, rows.last());
    }

    let versions: Vec<_> = fit_rows.iter().map(|r| r.version.clone()).collect();
    let current_v = fit_rows.last().and_then(|r| r.version.clone());
    let vdists = compute_vdists(&versions, current_v.as_deref());

    let w: Vec<f64> = fit_rows
        .iter()
        .zip(&vdists)
        .map(|(r, &vd)| {
            let age = Duration::from_secs_f64((now - r.completed_at).max(0.0));
            sample_weight(age, halflife_secs, vd)
        })
        .collect();
    let n_eff = kish_n_eff(&w);

    let cs: Vec<f64> = fit_rows
        .iter()
        .map(|r| r.cpu_limit_cores.unwrap())
        .collect();
    let ts: Vec<f64> = fit_rows.iter().map(|r| r.duration_secs).collect();
    let ms: Vec<u64> = fit_rows
        .iter()
        .map(|r| r.peak_memory_bytes as u64)
        .collect();

    // ExploreState reads only current-version (vdist==0) cpu_limits — the
    // explore ladder shouldn't count probes from a prior version toward
    // "distinct c seen".
    let cur_cs: Vec<f64> = fit_rows
        .iter()
        .zip(&vdists)
        .filter(|(_, v)| **v == 0)
        .map(|(r, _)| r.cpu_limit_cores.unwrap())
        .collect();
    let explore = derive_explore_state(&cur_cs, fit_rows.last().copied());

    // Fit gates (ADR-023 §2.4): need ≥3 effective samples AND ≥4× span on
    // the current-version c set before trusting NNLS over a probe.
    let span = if cur_cs.is_empty() {
        1.0
    } else {
        let max = cur_cs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let min = cur_cs.iter().copied().fold(f64::INFINITY, f64::min);
        max / min
    };
    let fit = if n_eff < 3.0 || span < 4.0 {
        DurationFit::Probe
    } else {
        fit_duration(&cs, &ts, &w, false, f64::INFINITY)
    };
    let mem = fit_memory(&cs, &ms, &w, n_eff);

    let disk: Vec<f64> = fit_rows
        .iter()
        .filter_map(|r| r.peak_disk_bytes.map(|b| b as f64))
        .collect();
    let disk_w: Vec<f64> = fit_rows
        .iter()
        .zip(&w)
        .filter(|(r, _)| r.peak_disk_bytes.is_some())
        .map(|(_, &wi)| wi)
        .collect();
    let disk_p90 =
        (!disk.is_empty()).then(|| DiskBytes(weighted_quantile(&disk, &disk_w, 0.9) as u64));

    let sigma = if matches!(fit, DurationFit::Probe) {
        0.2
    } else {
        sigma_resid(&cs, &ts, &w, &fit)
    };

    FittedParams {
        key: key.clone(),
        fit,
        mem,
        disk_p90,
        sigma_resid: sigma,
        n_eff,
        span,
        explore,
        t_min_ci: None,
        tier: None,
    }
}

/// No usable c-axis samples → emit a Probe placeholder so the explore
/// ladder (Task 5.2) can pick a first c. `last` (if any) seeds `last_wall`.
fn probe_only(key: &ModelKey, last: Option<&BuildSampleRow>) -> FittedParams {
    FittedParams {
        key: key.clone(),
        fit: DurationFit::Probe,
        mem: MemFit::Independent {
            p90: MemBytes(last.map(|r| r.peak_memory_bytes as u64).unwrap_or(0)),
        },
        disk_p90: last
            .and_then(|r| r.peak_disk_bytes)
            .map(|b| DiskBytes(b as u64)),
        sigma_resid: 0.2,
        n_eff: 0.0,
        span: 1.0,
        explore: derive_explore_state(&[], last),
        t_min_ci: None,
        tier: None,
    }
}

/// Reconstruct [`ExploreState`] from observed current-version cpu_limits.
/// `frozen` is always false here — the explore controller (Task 5.2) flips
/// it once the ladder converges; refit only reports what was sampled.
fn derive_explore_state(cur_cs: &[f64], last: Option<&BuildSampleRow>) -> ExploreState {
    let distinct: HashSet<u64> = cur_cs.iter().map(|c| c.to_bits()).collect();
    let (min_c, max_c) = cur_cs
        .iter()
        .fold((f64::INFINITY, 0.0_f64), |(lo, hi), &c| {
            (lo.min(c), hi.max(c))
        });
    // "Saturated" = last build's mean utilisation (cpu-seconds / wall /
    // limit) exceeded 40% — i.e. the build actually used the cores it was
    // given, so probing higher is worth it.
    let saturated = last
        .and_then(|r| {
            r.cpu_seconds_total
                .zip(r.cpu_limit_cores)
                .map(|(secs, lim)| secs / r.duration_secs / lim > 0.4)
        })
        .unwrap_or(false);
    ExploreState {
        distinct_c: distinct.len() as u8,
        min_c: RawCores(if min_c.is_finite() { min_c } else { 0.0 }),
        max_c: RawCores(max_c),
        frozen: false,
        saturated,
        last_wall: WallSeconds(last.map(|r| r.duration_secs).unwrap_or(0.0)),
    }
}

fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(c: f64, t: f64) -> BuildSampleRow {
        BuildSampleRow {
            pname: "p".into(),
            system: "x86_64-linux".into(),
            tenant: "t".into(),
            duration_secs: t,
            peak_memory_bytes: (256 << 20) + (c as i64 * (8 << 20)),
            cpu_limit_cores: Some(c),
            cpu_seconds_total: Some(t * c * 0.5),
            completed_at: now_epoch(),
            ..Default::default()
        }
    }

    fn key() -> ModelKey {
        ModelKey {
            pname: "p".into(),
            system: "x86_64-linux".into(),
            tenant: "t".into(),
        }
    }

    #[test]
    fn refit_amdahl_when_span_and_neff_sufficient() {
        let rows: Vec<_> = [4.0, 8.0, 16.0, 32.0, 64.0]
            .into_iter()
            .map(|c| row(c, 30.0 + 2000.0 / c))
            .collect();
        let f = refit(&key(), &rows, 7.0 * 86400.0);
        assert!(matches!(f.fit, DurationFit::Amdahl { .. }), "{:?}", f.fit);
        assert!(f.n_eff > 4.9, "n_eff={}", f.n_eff);
        assert!(f.span >= 16.0, "span={}", f.span);
        assert_eq!(f.explore.distinct_c, 5);
        assert!(f.explore.saturated, "util 0.5 > 0.4 gate");
    }

    #[test]
    fn refit_probe_when_span_too_small() {
        let rows: Vec<_> = [8.0, 8.0, 16.0]
            .into_iter()
            .map(|c| row(c, 30.0 + 2000.0 / c))
            .collect();
        let f = refit(&key(), &rows, 7.0 * 86400.0);
        assert!(matches!(f.fit, DurationFit::Probe));
        assert!(f.span < 4.0);
    }

    #[test]
    fn refit_drops_rows_without_cpu_limit() {
        let mut rows: Vec<_> = [4.0, 8.0, 16.0, 32.0, 64.0]
            .into_iter()
            .map(|c| row(c, 30.0 + 2000.0 / c))
            .collect();
        // Inject a row with no cpu_limit — must not desync cs/ts/w.
        rows.insert(
            2,
            BuildSampleRow {
                duration_secs: 999.0,
                cpu_limit_cores: None,
                completed_at: now_epoch(),
                ..Default::default()
            },
        );
        let f = refit(&key(), &rows, 7.0 * 86400.0);
        assert!(matches!(f.fit, DurationFit::Amdahl { .. }));
        assert_eq!(f.explore.distinct_c, 5);
    }

    #[test]
    fn refit_empty_is_probe() {
        let f = refit(&key(), &[], 7.0 * 86400.0);
        assert!(matches!(f.fit, DurationFit::Probe));
        assert_eq!(f.n_eff, 0.0);
        assert_eq!(f.explore.distinct_c, 0);
    }
}
