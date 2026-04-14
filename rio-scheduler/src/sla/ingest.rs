//! Per-key refit: `Vec<BuildSampleRow>` → [`FittedParams`].
//!
//! The [`SlaEstimator`](super::SlaEstimator) cache calls [`refit`] once per
//! touched key on each refresh tick; the fit itself is pure (no DB, no I/O)
//! so it can be unit-tested against synthetic rows.

use std::collections::HashSet;
use std::time::Duration;

use super::bootstrap::{WeightedSample, t_min_ci};
use super::fit::{
    compute_vdists, fit_duration, fit_memory, kish_n_eff, sample_weight, sigma_resid,
    weighted_quantile,
};
use super::solve::Tier;
use super::types::{
    DiskBytes, DurationFit, ExploreState, FittedParams, MemBytes, MemFit, ModelKey, RawCores,
    RefSeconds, WallSeconds,
};
use crate::db::BuildSampleRow;

/// Bootstrap replicates per CI recompute. 500 keeps the 80% CI stable to
/// ~2% across reseeds while staying <1ms per key on the refit path.
const BOOTSTRAP_REPS: usize = 500;

/// Refit one `(pname, system, tenant)` key from its ring-buffer of recent
/// samples (≤32 rows, completed_at-ascending — `rows.last()` is newest).
///
/// `prev` is the previously-cached fit for this key — feeds the CI
/// debounce (`should_recompute_ci`) and the Schmitt-trigger tier
/// hysteresis (`reassign_tier`). `tiers` is the operator tier ladder
/// sorted tightest-first (as from [`super::config::SlaConfig::solve_tiers`]);
/// empty → tier reassignment is a no-op.
///
/// Rows lacking `cpu_limit_cores` are dropped from the fit set entirely:
/// without the control variable a sample can't sit on the T(c) or M(c)
/// curve, and keeping it would desync the parallel `cs`/`ts`/`w` slices
/// (`fit_duration` debug-asserts equal length). Such rows come from old
/// executors / non-k8s test runs / recovered derivations and are rare in
/// steady state.
pub fn refit(
    key: &ModelKey,
    rows: &[BuildSampleRow],
    halflife_secs: f64,
    prev: Option<&FittedParams>,
    tiers: &[Tier],
) -> FittedParams {
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

    let (sigma, log_residuals) = if matches!(fit, DurationFit::Probe) {
        (0.2, Vec::new())
    } else {
        let lr: Vec<f64> = cs
            .iter()
            .zip(&ts)
            .map(|(&c, &t)| (t / fit.t_at(RawCores(c)).0).ln())
            .collect();
        (sigma_resid(&cs, &ts, &w, &fit), lr)
    };

    // r[impl sched.sla.reassign-schmitt]
    // Bootstrap CI is the expensive bit (~500 NNLS refits). Debounce so a
    // burst of completions on one key doesn't refit-storm: keep prev CI
    // unless the point estimate moved by half a CI width, n_eff jumped,
    // or it's been long enough. Probe fits skip CI entirely (no T_min).
    let (ci, ci_at) = if matches!(fit, DurationFit::Probe) {
        (None, None)
    } else if should_recompute_ci(prev, &fit, n_eff, now, halflife_secs) {
        let ws: Vec<WeightedSample> = cs
            .iter()
            .zip(&ts)
            .zip(&w)
            .map(|((&c, &t), &w)| WeightedSample { c, t, w })
            .collect();
        (t_min_ci(&ws, BOOTSTRAP_REPS, fit.p_bar().0), Some(now))
    } else {
        (
            prev.and_then(|p| p.t_min_ci),
            prev.and_then(|p| p.ci_computed_at),
        )
    };
    let tier = reassign_tier(prev.and_then(|p| p.tier.as_deref()), ci, tiers);

    FittedParams {
        key: key.clone(),
        fit,
        mem,
        disk_p90,
        sigma_resid: sigma,
        log_residuals,
        n_eff,
        span,
        explore,
        t_min_ci: ci,
        ci_computed_at: ci_at,
        tier,
    }
}

/// MAD-based outlier gate for one new sample against the PREVIOUS fit.
///
/// A sample is an outlier if its absolute log-residual against `fit`'s
/// curve exceeds `3 · 1.4826 · MAD(prev_residuals)` — the standard
/// 3σ-equivalent under a normal-MAD scale (1.4826·MAD ≈ σ for normal
/// data). The MAD is floored at `sigma_resid / 1.4826` (so a near-zero
/// MAD on a tight fit doesn't reject everything) and at the relative
/// poll-granularity `dt_poll / sample_t` (a 1s cgroup poll on a 10s
/// build is ±10% noise on its own; don't call that an outlier).
///
/// Gated on `n_eff ≥ 5`: with fewer effective samples MAD is unstable
/// and the explore ladder is still walking — rejecting then would
/// throw away exactly the diversity the fit needs.
// r[impl sched.sla.outlier-mad-reject]
pub fn is_outlier(sample_t: f64, sample_c: f64, fit: &FittedParams, dt_poll: f64) -> bool {
    if fit.n_eff < 5.0 || fit.log_residuals.is_empty() {
        return false;
    }
    let predicted = fit.fit.t_at(RawCores(sample_c)).0;
    if !predicted.is_finite() || predicted <= 0.0 || sample_t <= 0.0 {
        return false;
    }
    let log_resid = (sample_t / predicted).ln().abs();
    let mad = median_abs_dev(&fit.log_residuals);
    let floor = (fit.sigma_resid / 1.4826).max(dt_poll / sample_t);
    log_resid > 3.0 * 1.4826 * mad.max(floor)
}

/// Median absolute deviation: `median(|r_i - median(r)|)`. Unweighted —
/// the residuals already came from weighted fits, and MAD's robustness
/// is the point (one wild residual contributes one rank, not a weight).
fn median_abs_dev(residuals: &[f64]) -> f64 {
    let med = median(residuals);
    let devs: Vec<f64> = residuals.iter().map(|r| (r - med).abs()).collect();
    median(&devs)
}

fn median(v: &[f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = s.len();
    if n % 2 == 1 {
        s[n / 2]
    } else {
        (s[n / 2 - 1] + s[n / 2]) / 2.0
    }
}

/// Debounce gate for the bootstrap CI. Recompute when:
///   - no previous fit, or previous fit had no CI;
///   - n_eff moved >50% (ring filled / drained);
///   - the new T_min point estimate moved by more than half the previous
///     CI width (estimate has plausibly left the old interval).
///
/// Otherwise hold the previous CI. Hard floor: never recompute within
/// `min(30s, halflife/10)` of the last bootstrap regardless of the above
/// — bounds the per-key bootstrap rate under completion storms.
pub(super) fn should_recompute_ci(
    prev: Option<&FittedParams>,
    new_fit: &DurationFit,
    new_n_eff: f64,
    now: f64,
    halflife_secs: f64,
) -> bool {
    let Some(prev) = prev else { return true };
    let Some((plo, phi)) = prev.t_min_ci else {
        return true;
    };
    if let Some(at) = prev.ci_computed_at
        && now - at < 30.0_f64.min(halflife_secs / 10.0)
    {
        return false;
    }
    if prev.n_eff > 0.0 && (prev.n_eff - new_n_eff).abs() / prev.n_eff > 0.5 {
        return true;
    }
    let width = phi.0 - plo.0;
    (prev.fit.t_min().0 - new_fit.t_min().0).abs() > width / 2.0
}

/// Schmitt-trigger tier reassignment with a 0.85/1.05 deadband on
/// `binding_bound` (= p90 if set, else p50, else p99). `tiers` must be
/// sorted tightest-first. Returns the new tier name, or `prev` unchanged
/// when there is no CI or no bounded tiers.
///
/// Starting position is `prev`'s index in the bounded-tier list (or
/// loosest if `prev` is None / unbounded / unknown). From there,
/// **promote** while `ci.hi < 0.85 · tighter.binding_bound`, **demote**
/// while `ci.hi > 1.05 · current.binding_bound`. The 20-point deadband
/// means a key oscillating around a tier boundary stays put instead of
/// flapping on every refit.
// r[impl sched.sla.reassign-schmitt]
pub(super) fn reassign_tier(
    prev: Option<&str>,
    ci: Option<(RefSeconds, RefSeconds)>,
    tiers: &[Tier],
) -> Option<String> {
    let Some((_, hi)) = ci else {
        return prev.map(String::from);
    };
    let binding = |t: &Tier| t.p90.or(t.p50).or(t.p99);
    let bounded: Vec<(&str, f64)> = tiers
        .iter()
        .filter_map(|t| binding(t).map(|b| (t.name.as_str(), b)))
        .collect();
    if bounded.is_empty() {
        return prev.map(String::from);
    }
    let mut i = prev
        .and_then(|p| bounded.iter().position(|(n, _)| *n == p))
        .unwrap_or(bounded.len() - 1);
    loop {
        if i > 0 && hi.0 < 0.85 * bounded[i - 1].1 {
            i -= 1;
        } else if i + 1 < bounded.len() && hi.0 > 1.05 * bounded[i].1 {
            i += 1;
        } else {
            break;
        }
    }
    Some(bounded[i].0.to_string())
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
        log_residuals: Vec::new(),
        n_eff: 0.0,
        span: 1.0,
        explore: derive_explore_state(&[], last),
        t_min_ci: None,
        ci_computed_at: None,
        tier: None,
    }
}

/// Reconstruct [`ExploreState`] from observed current-version cpu_limits.
/// `frozen` mirrors [`super::explore::frozen`] (span≥4 ∨ max_c at
/// ceiling ∨ min_c at floor) so `intent_for`'s `f.explore.frozen` and
/// `explore::frozen(&f.explore, …)` agree even when the actor's
/// `max_cores` differs from the in-ladder one — `frozen` here uses
/// span/floor only (the ceiling check needs config the refit doesn't
/// have; `intent_for` re-checks against `ceil.max_cores`).
fn derive_explore_state(cur_cs: &[f64], last: Option<&BuildSampleRow>) -> ExploreState {
    let distinct: HashSet<u64> = cur_cs.iter().map(|c| c.to_bits()).collect();
    let (min_c, max_c) = cur_cs
        .iter()
        .fold((f64::INFINITY, 0.0_f64), |(lo, hi), &c| {
            (lo.min(c), hi.max(c))
        });
    let min_c = if min_c.is_finite() { min_c } else { 0.0 };
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
    let span = if min_c > 0.0 { max_c / min_c } else { 1.0 };
    ExploreState {
        distinct_c: distinct.len() as u8,
        min_c: RawCores(min_c),
        max_c: RawCores(max_c),
        frozen: max_c > 0.0 && (span >= 4.0 || min_c <= 1.0),
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

    fn r(rows: &[BuildSampleRow]) -> FittedParams {
        refit(&key(), rows, 7.0 * 86400.0, None, &[])
    }

    #[test]
    fn refit_amdahl_when_span_and_neff_sufficient() {
        let rows: Vec<_> = [4.0, 8.0, 16.0, 32.0, 64.0]
            .into_iter()
            .map(|c| row(c, 30.0 + 2000.0 / c))
            .collect();
        let f = r(&rows);
        assert!(matches!(f.fit, DurationFit::Amdahl { .. }), "{:?}", f.fit);
        assert!(f.n_eff > 4.9, "n_eff={}", f.n_eff);
        assert!(f.span >= 16.0, "span={}", f.span);
        assert_eq!(f.explore.distinct_c, 5);
        assert!(f.explore.saturated, "util 0.5 > 0.4 gate");
        // First-time fit (prev=None) → CI computed.
        assert!(f.t_min_ci.is_some(), "first refit bootstraps CI");
        assert!(f.ci_computed_at.is_some());
    }

    #[test]
    fn refit_probe_when_span_too_small() {
        let rows: Vec<_> = [8.0, 8.0, 16.0]
            .into_iter()
            .map(|c| row(c, 30.0 + 2000.0 / c))
            .collect();
        let f = r(&rows);
        assert!(matches!(f.fit, DurationFit::Probe));
        assert!(f.span < 4.0);
        assert!(f.t_min_ci.is_none(), "Probe fit → no CI");
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
        let f = r(&rows);
        assert!(matches!(f.fit, DurationFit::Amdahl { .. }));
        assert_eq!(f.explore.distinct_c, 5);
    }

    #[test]
    fn refit_empty_is_probe() {
        let f = r(&[]);
        assert!(matches!(f.fit, DurationFit::Probe));
        assert_eq!(f.n_eff, 0.0);
        assert_eq!(f.explore.distinct_c, 0);
    }

    // ─── Task 4.3: Schmitt-trigger + debounce ─────────────────────────────

    fn tier(name: &str, p90: f64) -> Tier {
        Tier {
            name: name.into(),
            p50: None,
            p90: Some(p90),
            p99: None,
        }
    }

    fn ladder() -> Vec<Tier> {
        // Tightest-first.
        vec![
            tier("fast", 300.0),
            tier("normal", 1200.0),
            tier("slow", 3600.0),
        ]
    }

    // r[verify sched.sla.reassign-schmitt]
    #[test]
    fn schmitt_promotes_only_below_85pct() {
        let tiers = ladder();
        // ci.hi = 250 < 0.85·300 = 255 → promote normal→fast.
        assert_eq!(
            reassign_tier(
                Some("normal"),
                Some((RefSeconds(200.0), RefSeconds(250.0))),
                &tiers
            ),
            Some("fast".into())
        );
        // ci.hi = 260: 260 > 255 (no promote) AND 260 < 1.05·1200 (no
        // demote) → stays normal. Deadband holds.
        assert_eq!(
            reassign_tier(
                Some("normal"),
                Some((RefSeconds(200.0), RefSeconds(260.0))),
                &tiers
            ),
            Some("normal".into())
        );
        // Already at tightest → no further promote.
        assert_eq!(
            reassign_tier(
                Some("fast"),
                Some((RefSeconds(10.0), RefSeconds(50.0))),
                &tiers
            ),
            Some("fast".into())
        );
    }

    #[test]
    fn schmitt_demotes_only_above_105pct() {
        let tiers = ladder();
        // ci.hi = 1300 > 1.05·1200 = 1260 → demote normal→slow.
        assert_eq!(
            reassign_tier(
                Some("normal"),
                Some((RefSeconds(900.0), RefSeconds(1300.0))),
                &tiers
            ),
            Some("slow".into())
        );
        // ci.hi = 1250: 1250 < 1260 (no demote) AND 1250 > 0.85·300 (no
        // promote) → stays normal.
        assert_eq!(
            reassign_tier(
                Some("normal"),
                Some((RefSeconds(900.0), RefSeconds(1250.0))),
                &tiers
            ),
            Some("normal".into())
        );
        // Already at loosest → no further demote.
        assert_eq!(
            reassign_tier(
                Some("slow"),
                Some((RefSeconds(5000.0), RefSeconds(9000.0))),
                &tiers
            ),
            Some("slow".into())
        );
    }

    #[test]
    fn schmitt_no_ci_keeps_prev() {
        let tiers = ladder();
        assert_eq!(
            reassign_tier(Some("normal"), None, &tiers),
            Some("normal".into())
        );
        assert_eq!(reassign_tier(None, None, &tiers), None);
        // Empty tier list → keeps prev.
        assert_eq!(
            reassign_tier(Some("x"), Some((RefSeconds(1.0), RefSeconds(2.0))), &[]),
            Some("x".into())
        );
    }

    #[test]
    fn schmitt_no_prev_walks_from_loosest() {
        let tiers = ladder();
        // No prev, ci.hi=250 → start at slow, promote to normal (250<0.85·1200),
        // promote to fast (250<0.85·300).
        assert_eq!(
            reassign_tier(None, Some((RefSeconds(200.0), RefSeconds(250.0))), &tiers),
            Some("fast".into())
        );
        // No prev, ci.hi=2000 → start at slow, 2000>0.85·1200 (no promote),
        // 2000<1.05·3600 (no demote) → slow.
        assert_eq!(
            reassign_tier(None, Some((RefSeconds(1000.0), RefSeconds(2000.0))), &tiers),
            Some("slow".into())
        );
    }

    fn prev_with_ci(t_min: f64, lo: f64, hi: f64, n_eff: f64, at: f64) -> FittedParams {
        FittedParams {
            key: key(),
            fit: DurationFit::Amdahl {
                s: RefSeconds(t_min),
                p: RefSeconds(0.0),
            },
            mem: MemFit::Independent { p90: MemBytes(0) },
            disk_p90: None,
            sigma_resid: 0.1,
            log_residuals: Vec::new(),
            n_eff,
            span: 8.0,
            explore: ExploreState {
                distinct_c: 3,
                min_c: RawCores(1.0),
                max_c: RawCores(32.0),
                frozen: false,
                saturated: false,
                last_wall: WallSeconds(0.0),
            },
            t_min_ci: Some((RefSeconds(lo), RefSeconds(hi))),
            ci_computed_at: Some(at),
            tier: None,
        }
    }

    #[test]
    fn debounce_skips_ci_within_30s() {
        let new_fit = DurationFit::Amdahl {
            s: RefSeconds(500.0),
            p: RefSeconds(0.0),
        };
        // prev CI computed at t=1000, now=1020 → elapsed=20s < 30s → skip
        // even though ΔT_min=400 > width/2=50.
        let prev = prev_with_ci(100.0, 50.0, 150.0, 5.0, 1000.0);
        assert!(!should_recompute_ci(
            Some(&prev),
            &new_fit,
            5.0,
            1020.0,
            7.0 * 86400.0
        ));
        // Same prev, now=1040 → elapsed=40s > 30s → ΔT_min trigger fires.
        assert!(should_recompute_ci(
            Some(&prev),
            &new_fit,
            5.0,
            1040.0,
            7.0 * 86400.0
        ));
    }

    #[test]
    fn debounce_recomputes_on_neff_jump() {
        let new_fit = DurationFit::Amdahl {
            s: RefSeconds(100.0),
            p: RefSeconds(0.0),
        };
        // ΔT_min=0, but n_eff 5→12 (>50% jump) → recompute.
        let prev = prev_with_ci(100.0, 50.0, 150.0, 5.0, 1000.0);
        assert!(should_recompute_ci(
            Some(&prev),
            &new_fit,
            12.0,
            1040.0,
            7.0 * 86400.0
        ));
        // n_eff 5→6 (<50%) and ΔT_min=0 → keep.
        assert!(!should_recompute_ci(
            Some(&prev),
            &new_fit,
            6.0,
            1040.0,
            7.0 * 86400.0
        ));
    }

    // ─── Task 5.1: MAD outlier rejection ─────────────────────────────────

    /// Build a fit from 6 close-to-curve samples (n_eff≈6, span=16),
    /// then probe a 7th at `mult × predicted`.
    fn outlier_fit() -> FittedParams {
        // True: T = 30 + 2000/c, ±5% deterministic noise.
        let rows: Vec<_> = [4.0, 8.0, 12.0, 16.0, 32.0, 64.0]
            .into_iter()
            .enumerate()
            .map(|(i, c)| {
                let noise = 1.0 + 0.05 * (i as f64 * 1.7).sin();
                row(c, (30.0 + 2000.0 / c) * noise)
            })
            .collect();
        r(&rows)
    }

    // r[verify sched.sla.outlier-mad-reject]
    #[test]
    fn mad_flags_10x_at_neff_6() {
        let fit = outlier_fit();
        assert!(fit.n_eff >= 5.0, "precondition: n_eff={}", fit.n_eff);
        assert!(!fit.log_residuals.is_empty());
        let pred = fit.fit.t_at(RawCores(8.0)).0;
        // 10× predicted → ln(10)≈2.3 absolute log-resid. 5% noise gives
        // MAD ~0.03; gate = 3·1.4826·max(MAD, σ/1.4826) ≈ 3σ ≈ 0.15.
        assert!(is_outlier(pred * 10.0, 8.0, &fit, 1.0), "10× → flagged");
        // 1.2× predicted → ln(1.2)≈0.18, borderline; 1.05× must pass.
        assert!(!is_outlier(pred * 1.05, 8.0, &fit, 1.0), "5% → kept");
    }

    #[test]
    fn mad_gated_below_neff_5() {
        let mut fit = outlier_fit();
        fit.n_eff = 4.0;
        let pred = fit.fit.t_at(RawCores(8.0)).0;
        assert!(
            !is_outlier(pred * 10.0, 8.0, &fit, 1.0),
            "n_eff=4 → never flag"
        );
        // No residuals (Probe fit) → never flag regardless of n_eff.
        fit.n_eff = 10.0;
        fit.log_residuals.clear();
        assert!(!is_outlier(pred * 10.0, 8.0, &fit, 1.0));
    }

    #[test]
    fn mad_floor_from_dt_poll() {
        // Perfect fit (zero MAD, zero σ) → floor must come from
        // dt_poll/sample_t. At c=8 predicted=280; dt_poll=28 → floor=0.1,
        // gate = 3·1.4826·0.1 ≈ 0.44.
        let rows: Vec<_> = [4.0, 8.0, 12.0, 16.0, 32.0, 64.0]
            .into_iter()
            .map(|c| row(c, 30.0 + 2000.0 / c))
            .collect();
        let fit = r(&rows);
        assert!(fit.sigma_resid < 1e-3, "perfect fit");
        assert!(fit.n_eff >= 5.0, "n_eff={}", fit.n_eff);
        let pred = fit.fit.t_at(RawCores(8.0)).0; // ≈ 280
        // ln(1.3)≈0.26 < 0.44 → kept; ln(2)≈0.69 > 0.44 → flagged.
        assert!(!is_outlier(pred * 1.3, 8.0, &fit, 28.0));
        assert!(is_outlier(pred * 2.0, 8.0, &fit, 28.0));
        // dt_poll=0 → floor 0 → near-zero gate → 1.3× IS flagged.
        assert!(is_outlier(pred * 1.3, 8.0, &fit, 0.0));
    }

    #[test]
    fn median_helpers() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[1.0, 2.0, 3.0, 4.0]), 2.5);
        assert_eq!(median(&[]), 0.0);
        assert!((median_abs_dev(&[1.0, 2.0, 3.0, 4.0, 100.0]) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn debounce_first_time_always_recomputes() {
        let f = DurationFit::Amdahl {
            s: RefSeconds(100.0),
            p: RefSeconds(0.0),
        };
        assert!(should_recompute_ci(None, &f, 5.0, 1000.0, 7.0 * 86400.0));
        // prev exists but has no CI → recompute.
        let mut prev = prev_with_ci(100.0, 50.0, 150.0, 5.0, 1000.0);
        prev.t_min_ci = None;
        assert!(should_recompute_ci(
            Some(&prev),
            &f,
            5.0,
            1000.0,
            7.0 * 86400.0
        ));
    }
}
