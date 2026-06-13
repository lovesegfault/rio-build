use nalgebra::{DMatrix, DVector};

use super::types::{
    DiskBytes, DurationFit, FitDf, MemBytes, MemFit, RawCores, RawDiskP90, RefSeconds, RingNEff,
};

// r[impl sched.sla.disk-reaches-ephemeral-storage+2]
/// live_049 L2: the typed disk-axis envelope — `floor ≤ fitted ≤
/// ceiling` BY CONSTRUCTION (the constructor clamps; R9's ordering
/// law). The disk request gets the same lifecycle cores/mem have:
///
/// - `fitted` = the per-pname WITNESSED disk fit when the ingest
///   producer minted one (live060-c: `aggregate_disk_p90` gates
///   consumption at `DISK_WITNESS_MIN_PEAKS` observed peaks and
///   shrink-floors at `newest_peak × DISK_SHRINK_HEADROOM` — a single
///   unrepresentative build cannot retire the prior at activation;
///   probe-shaped fits included, their single-row read widened to the
///   full aggregate), else the cold-start PRIOR (`sla.defaultDisk`)
///   that a WITNESSED population retires — mirroring the mem axis's
///   observed-or-probe-shape form, NOT a blend toward the chart
///   constant (a 100 GiB blend would never shrink to a small fitted
///   value). On the live prjquota-less fleet every completion records
///   a NULL disk peak (the live060-a precondition), so the prior
///   stands fleet-wide until provisioning lands — the gate makes that
///   activation safe, not different;
/// - `floor` = the probe's own scratch footprint ([`Self::FLOOR_BYTES`]
///   — unpack + outputs of a bounded probe; mirrors the trivial-builder
///   `LOCAL_DISK_BYTES` lane in `solve.rs`, census-pinned);
/// - `ceiling` = `sla.maxDisk` — the operator bound (the solve-side
///   `.min(max_disk)` clamps are this law's other impl sites).
///
/// This envelope is the SOLE constructor for EVERY dispatch lane's
/// disk request (bug_132/R24): the lanes do not "benefit
/// automatically" from sharing inputs — they are UNABLE to emit
/// anything else, because every emission seam (`IntentDecision`,
/// `SolveResult`, `SolveFullResult`/`AdmissibleSet`,
/// `ExploreDecision`) carries [`DiskRequest`], a newtype mintable
/// only here. A raw `disk_p90` projection no longer type-checks at
/// any seam; rustc is the lane census.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskFitEnvelope {
    pub floor: DiskBytes,
    pub fitted: DiskBytes,
    pub ceiling: DiskBytes,
}

impl DiskFitEnvelope {
    /// Minimal scratch a probe build needs (source unpack + outputs).
    /// Mirrors `solve::LOCAL_DISK_BYTES` (1 GiB — the trivial-builder
    /// disk const); the agreement is pinned by `disk_envelope_floor_
    /// mirrors_local_disk` below.
    pub const FLOOR_BYTES: u64 = 1 << 30;

    /// Derive the envelope. Ordering law `floor ≤ fitted ≤ ceiling`
    /// holds by construction for every input (an inverted
    /// configuration — ceiling below the floor — saturates at the
    /// ceiling: the operator bound wins, typed).
    pub fn derive(observed_p90: Option<DiskBytes>, prior: u64, ceiling: u64) -> Self {
        let floor = Self::FLOOR_BYTES.min(ceiling);
        let fitted = observed_p90
            .map_or(prior, |d| d.0)
            .clamp(floor, ceiling.max(floor));
        Self {
            floor: DiskBytes(floor),
            fitted: DiskBytes(fitted),
            ceiling: DiskBytes(ceiling),
        }
    }

    /// The dispatch request — the fitted point of the envelope,
    /// already floor/ceiling-lawful by [`Self::derive`].
    pub fn request(&self) -> DiskRequest {
        DiskRequest(self.fitted.0)
    }

    /// The one-step constructor every dispatch lane uses:
    /// derive-then-request. The ONLY public mint of [`DiskRequest`].
    pub fn fit(observed_p90: Option<DiskBytes>, prior: u64, ceiling: u64) -> DiskRequest {
        Self::derive(observed_p90, prior, ceiling).request()
    }

    // r[impl sched.sla.disk-polarity-fork]
    /// The Feasible-lane REJECT predicate, single-sourced (bug_128's
    /// sibling discipline applied here too): a witnessed observation
    /// above the operator ceiling is the genuine "cannot fit" gate —
    /// c-invariant, evaluated on the producer's RAW witnessed face
    /// ([`RawDiskP90`], demanded BY SIGNATURE — merged_bug_002: the
    /// sizing-face shrink floor is anti-conservative here; a floored
    /// consult falsely rejected every all-fitting population whose
    /// newest peak sat in `(ceiling/1.2, ceiling]`, and passing the
    /// floored field no longer type-checks). The raw face is still
    /// gated: `None` below the population gate, so a single
    /// unwitnessed build cannot reject a tier fleet-wide (the
    /// single-sample hazard has a reject face too — the gate, not the
    /// floor, is that protection); the weighted p90 of a witnessed
    /// all-fitting population is bounded by its max observation, so
    /// no all-fitting population can trip this. Never the clamped
    /// request, which cannot exceed the ceiling — quantifier: census(test: w10ad_lane_equality_at_the_fit_quantifier) —.
    /// `solve_tier`'s tier gate, `evaluate_cell`'s per-cell gate,
    /// `explain`'s `disk-ceiling` label, and `classify_ceiling`'s
    /// metric mirror all read THIS, so the explain/metric surfaces
    /// mirror the solve gates by construction.
    pub fn exceeds_ceiling(observed_p90: Option<RawDiskP90>, ceiling: u64) -> bool {
        observed_p90.is_some_and(|d| d.bytes() > ceiling)
    }
}

// r[impl sched.sla.disk-reaches-ephemeral-storage+2]
/// bug_132 (R24 — laws by construction): the dispatch disk value,
/// UNWRITABLE except through [`DiskFitEnvelope`] (the sole awaiter of
/// the floor/ceiling law). The inner field is private and there is no
/// other constructor — `DiskFitEnvelope::fit`/`request` mint it with
/// `floor ≤ value ≤ ceiling` applied, and
/// [`Self::with_reactive_floor`] is the one lawful post-solve
/// modification (the per-drv reactive floor at the emission
/// chokepoint, ceiling re-applied). Every dispatch lane's output type
/// (`IntentDecision`, `SolveResult`, `AdmissibleSet`,
/// `SolveFullResult::BestEffort`, `ExploreDecision`) carries this
/// newtype, so the wave-8 defect class — a lane keeping a floor-less
/// open-coded `disk_p90` projection beside the envelope — no longer
/// compiles. Enforcement tier: compile-sealed (rustc is the census;
/// no lint arm needed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskRequest(u64);

impl DiskRequest {
    /// The wire value (`SpawnIntent.disk_bytes`).
    pub const fn bytes(self) -> u64 {
        self.0
    }

    /// The emission chokepoint's reactive-floor overlay
    /// (`resource_floor.disk_bytes`, clamped at the live ceiling) —
    /// the ONE lawful post-constructor modification; the result is
    /// still a lawful request (`≤ ceiling`, `≥` both floors).
    pub fn with_reactive_floor(self, floor_bytes: u64, ceiling: u64) -> Self {
        Self(self.0.max(floor_bytes).min(ceiling))
    }
}

// r[impl sched.sla.hw-class.sample-weight-ordinal]
// r[impl sched.sla.fit-nnls]  (weights are part of the fit contract)
/// Ordinal recency halflife (samples). No wall-clock arm: a key built
/// monthly would otherwise asymptote at `n_eff ≈ 1` and never leave
/// §Exploration. ADR-023 L142-143.
const ORDINAL_HALFLIFE: f64 = 20.0;

/// Per-sample weight `0.5^(age/20) · 0.5^vdist`. `ordinal_age` is the
/// count of samples NEWER than this one for the key (0 = newest).
/// `vdist` is the ordinal version-distance (count of distinct version
/// strings between this sample and current).
pub fn sample_weight(ordinal_age: u32, vdist: u32) -> f64 {
    0.5f64.powf(f64::from(ordinal_age) / ORDINAL_HALFLIFE) * 0.5f64.powi(vdist as i32)
}

// r[impl sched.sla.hw-class.zq-inflation]
/// Student-t prediction-interval factor evaluated at the design
/// centroid (ADR-023 L193-197):
///
/// `z_q = t_{q, max(3, min(n_eff, n_distinct_c) − n_par)} · √(1 + 1/Σw)`
///
/// Both `n_eff` and `n_distinct_c` must bind: Kish n_eff measures
/// weight dispersion, not design-point support, and post-convergence
/// the latter is the limiting quantity. df is floored at 3 — trades
/// small-sample conservatism against the `n_eff ≥ 3` gate already
/// excluding df<1. At large `(n_eff, n_distinct_c, Σw)` this
/// asymptotes to `Φ⁻¹(q)` (≈ 1.2816 at q=0.9).
///
/// `sum_w` is `Σw_i` over the ring — NOT `n_eff` (they coincide only
/// when every weight is exactly 1).
pub fn z_q(q: f64, fit_df: FitDf, n_distinct_c: u32, n_par: u32, sum_w: f64) -> f64 {
    let df = (fit_df.0.min(f64::from(n_distinct_c)) - f64::from(n_par)).max(3.0);
    // Hill's t-quantile (distrs); golden-pinned in `t_ppf_golden`.
    let t = distrs::StudentsT::ppf(q, df);
    t * (1.0 + 1.0 / sum_w.max(1.0)).sqrt()
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

/// Lawson-Hanson tolerance. Shared between the KKT gradient gate, the
/// SVD-solve tolerance, the inner accept-gate, and the inner
/// alpha-filter so the four can never drift: the algorithm's
/// termination invariant is **accept-gate fails ⇒ alpha-filter
/// nonempty** (≥1 column drops per inner iteration); a `z_p[k] ∈
/// (0, NNLS_TOL]` that fails the accept-gate but is excluded from alpha
/// would yield `alpha = ∞` → `x` becomes ∞/NaN → inner loop spins
/// forever inside `DagActor::handle_tick`.
const NNLS_TOL: f64 = 1e-10;

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
        if wj <= NNLS_TOL {
            break;
        }
        passive[j] = true;
        // Hard bound: each inner iteration drops ≥1 passive column, so
        // `m` is the worst case. Defense-in-depth — even if a future
        // edit reintroduces a tolerance gap, the actor cannot hang.
        for _ in 0..m {
            let cols: Vec<usize> = (0..m).filter(|i| passive[*i]).collect();
            let ap = a.select_columns(&cols);
            let z_p = ap
                .clone()
                .svd(true, true)
                .solve(b, NNLS_TOL)
                .expect("svd solve");
            if z_p.iter().all(|&v| v > NNLS_TOL) {
                for (k, &ci) in cols.iter().enumerate() {
                    x[ci] = z_p[k];
                }
                break;
            }
            let alpha = cols
                .iter()
                .enumerate()
                .filter(|(k, _)| z_p[*k] <= NNLS_TOL)
                .map(|(k, &ci)| x[ci] / (x[ci] - z_p[k]))
                .fold(f64::INFINITY, f64::min);
            for (k, &ci) in cols.iter().enumerate() {
                x[ci] += alpha * (z_p[k] - x[ci]);
                if x[ci] <= NNLS_TOL {
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
    fit_nnls(cs, ts, w, unfreeze_q, p_bar, None)
}

/// 3-column fit with Tikhonov regularization on Q only: appends a
/// `[0, 0, √λ]` row with target 0 to the design matrix before
/// normalization, shrinking Q toward zero. As `λ → ∞` the fit
/// degenerates to the 2-column Amdahl/Capped.
pub fn fit_duration_ridge(
    cs: &[f64],
    ts: &[f64],
    w: &[f64],
    p_bar: f64,
    lambda: f64,
) -> DurationFit {
    fit_nnls(cs, ts, w, true, p_bar, Some(lambda))
}

fn fit_nnls(
    cs: &[f64],
    ts: &[f64],
    w: &[f64],
    unfreeze_q: bool,
    p_bar: f64,
    ridge_q: Option<f64>,
) -> DurationFit {
    debug_assert_eq!(cs.len(), ts.len());
    debug_assert_eq!(cs.len(), w.len());
    debug_assert!(ridge_q.is_none() || unfreeze_q);
    let n = cs.len();
    let cols = if unfreeze_q { 3 } else { 2 };
    let rows = n + usize::from(ridge_q.is_some());
    let mut a = DMatrix::zeros(rows, cols);
    let mut b = DVector::zeros(rows);
    for i in 0..n {
        let sw = w[i].sqrt();
        a[(i, 0)] = sw;
        a[(i, 1)] = sw / cs[i].min(p_bar);
        if unfreeze_q {
            a[(i, 2)] = sw * cs[i];
        }
        b[i] = sw * ts[i];
    }
    if let Some(lambda) = ridge_q {
        a[(n, 2)] = lambda.sqrt();
    }
    let norms: Vec<f64> = (0..cols).map(|j| a.column(j).norm().max(1e-12)).collect();
    for (j, &norm) in norms.iter().enumerate() {
        a.column_mut(j).scale_mut(1.0 / norm);
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

/// Inputs to [`fit_duration_staged`]'s stage-selection gate. ADR-023 §2.4
/// Table 1: USL is entered at `n_eff ≥ 10 ∧ span ≥ 8× ∧ ΔAICc < −2` and
/// exits back to Capped/Amdahl at `n_eff < 7` (hysteresis). `prev_usl`
/// latches the stage across refits within a version-epoch — vdist jumps
/// reset `span` and decay `n_eff`, naturally re-entering the entry gate.
pub struct StageGate {
    pub n_eff: f64,
    pub span: f64,
    pub p_bar: f64,
    pub prev_usl: bool,
}

/// Staged duration fit: 2-param (Amdahl / Capped) by default; unfreezes Q
/// (USL) when the gate permits AND the 3-param fit is preferred by ΔAICc.
/// Q is ridge-regularized with `λ = σ_amdahl² · n` so a noisy small-n fit
/// can't run away with a large Q. Returns `(fit, σ_resid)`.
pub fn fit_duration_staged(
    cs: &[f64],
    ts: &[f64],
    w: &[f64],
    gate: &StageGate,
) -> (DurationFit, f64) {
    let amdahl = fit_duration(cs, ts, w, false, gate.p_bar);
    let sigma_a = sigma_resid(cs, ts, w, &amdahl);
    let n = cs.len() as f64;
    // n ≥ 5 keeps the AICc small-sample correction term finite (n−k−1 > 0
    // for k=3); the n_eff/span gates are on the UNfiltered sample stats.
    let try_usl = cs.len() >= 5
        && if gate.prev_usl {
            gate.n_eff >= 7.0
        } else {
            gate.n_eff >= 10.0 && gate.span >= 8.0
        };
    if !try_usl {
        return (amdahl, sigma_a);
    }
    let usl = fit_duration_ridge(cs, ts, w, gate.p_bar, sigma_a.powi(2) * n);
    let sigma_u = sigma_resid(cs, ts, w, &usl);
    if gate.prev_usl {
        return (usl, sigma_u);
    }
    let aicc = |k: f64, sigma: f64| {
        let rss = (n * sigma.powi(2)).max(1e-300);
        n * (rss / n).ln() + 2.0 * k + 2.0 * k * (k + 1.0) / (n - k - 1.0)
    };
    let delta = aicc(3.0, sigma_u) - aicc(2.0, sigma_a);
    if delta < -2.0 {
        (usl, sigma_u)
    } else {
        (amdahl, sigma_a)
    }
}

// r[impl sched.sla.headroom-confidence-scaled]
pub fn headroom(n_eff: RingNEff) -> f64 {
    1.25 + 0.7 / n_eff.0.max(1.0).sqrt()
}

/// Closed-form weighted least squares for `y = a + b·x`. Returns `(a, b, σ)` where σ is
/// the weighted RMS residual. Degenerate (zero x-variance) input yields a non-finite slope.
fn wls_loglinear(x: &[f64], y: &[f64], w: &[f64]) -> (f64, f64, f64) {
    let sw: f64 = w.iter().sum();
    let sx: f64 = x.iter().zip(w).map(|(xi, wi)| wi * xi).sum();
    let sy: f64 = y.iter().zip(w).map(|(yi, wi)| wi * yi).sum();
    let sxx: f64 = x.iter().zip(w).map(|(xi, wi)| wi * xi * xi).sum();
    let sxy: f64 = x
        .iter()
        .zip(y)
        .zip(w)
        .map(|((xi, yi), wi)| wi * xi * yi)
        .sum();
    let denom = sw * sxx - sx * sx;
    // Cauchy–Schwarz gives denom ≥ 0; near-zero ⇒ rank-deficient design (constant x).
    if denom <= 1e-10 * (sw * sxx).max(1.0) {
        return (sy / sw, f64::NAN, 0.0);
    }
    let b = (sw * sxy - sx * sy) / denom;
    let a = (sy - b * sx) / sw;
    let ssr: f64 = x
        .iter()
        .zip(y)
        .zip(w)
        .map(|((xi, yi), wi)| wi * (yi - (a + b * xi)).powi(2))
        .sum();
    (a, b, (ssr / sw).sqrt())
}

pub(super) fn weighted_quantile(x: &[f64], w: &[f64], q: f64) -> f64 {
    let mut idx: Vec<usize> = (0..x.len()).collect();
    idx.sort_by(|&i, &j| x[i].total_cmp(&x[j]));
    let total: f64 = w.iter().sum();
    let mut cum = 0.0;
    for &i in &idx {
        cum += w[i];
        if cum / total >= q {
            return x[i];
        }
    }
    x[*idx.last().unwrap()]
}

/// IRLS τ-quantile regression on `y = a + b·x` with prior weights `w`. Reweights by the
/// pinball-loss subgradient (`τ` above the line, `1−τ` below, divided by |resid|) for up
/// to 30 iterations. Returns `(a, b, R¹)` where R¹ is the Koenker–Machado pseudo-R¹,
/// `1 − V(τ|fit)/V(τ|intercept-only)`.
fn irls_quantile(x: &[f64], y: &[f64], w: &[f64], tau: f64) -> (f64, f64, f64) {
    let (mut a, mut b, _) = wls_loglinear(x, y, w);
    for _ in 0..30 {
        let irls_w: Vec<f64> = x
            .iter()
            .zip(y)
            .zip(w)
            .map(|((&xi, &yi), &wi)| {
                let r = yi - (a + b * xi);
                let asym = if r >= 0.0 { tau } else { 1.0 - tau };
                wi * asym / r.abs().max(1e-6)
            })
            .collect();
        let (na, nb, _) = wls_loglinear(x, y, &irls_w);
        let converged = (na - a).abs() < 1e-6 && (nb - b).abs() < 1e-6;
        a = na;
        b = nb;
        if converged {
            break;
        }
    }
    let pinball = |a: f64, b: f64| -> f64 {
        x.iter()
            .zip(y)
            .zip(w)
            .map(|((&xi, &yi), &wi)| {
                let u = yi - (a + b * xi);
                wi * u * (tau - if u < 0.0 { 1.0 } else { 0.0 })
            })
            .sum()
    };
    let v_fit = pinball(a, b);
    let q_y = weighted_quantile(y, w, tau);
    let v_null = pinball(q_y, 0.0);
    let r1 = if v_null > 0.0 {
        1.0 - v_fit / v_null
    } else {
        0.0
    };
    (a, b, r1)
}

// r[impl sched.sla.mem-coupled]
/// Fit `log M = a + b·log c` at p90 via IRLS quantile regression. Gates on n_eff≥10 and
/// Koenker–Machado R¹≥0.7; below either threshold falls back to plain WLS with `r1=0.0`
/// as a small-n sentinel (caller applies a Student-t PI factor). Degenerate design
/// (constant c → undefined slope) falls through to an independent weighted p90.
///
/// Returns `(fit, weak)` where `weak = n_eff ≥ 10 ∧ R¹ < 0.7` (or
/// non-finite IRLS) — i.e. enough data for the coupled fit, but the
/// fit was rejected. Drives `rio_scheduler_sla_mem_fit_weak_total`.
pub fn fit_memory(
    cs: &[f64],
    ms: &[u64],
    w: &[f64],
    n_eff: f64,
    fallback_p90: MemBytes,
) -> (MemFit, bool) {
    // `.max(1.0)` floors before `.ln()`: completion.rs persists
    // `peak_memory_bytes = 0` as a legitimate sample point, but
    // `ln(0) = -∞` and `wls_loglinear` has no NaN handling — `-∞ - (-∞)
    // = NaN` collapses an entire key from `Coupled` to `Independent`.
    // `ln(1) = 0` is a benign low outlier IRLS down-weights. The
    // percentile-doesn't-drag rationale survives as a property of
    // the arm, not of this local `ms`: every `Independent` fallback
    // aggregates RAW un-floored bytes — the degenerate arm below
    // returns the CALLER-provided `fallback_p90` (refit's one mem
    // aggregator over the full row set, bug_072), which never sees
    // this floor; the floored values feed only the `lm` regression
    // inputs. (bug_026: the pre-fix text claimed the path "still uses
    // raw ms" — dataflow the bug_072 rewire deleted.) `cs` is floored
    // symmetrically (`cpu_limit_cores` is `>= 1` in practice).
    let lc: Vec<f64> = cs.iter().map(|c| c.max(1.0).ln()).collect();
    let lm: Vec<f64> = ms.iter().map(|m| (*m as f64).max(1.0).ln()).collect();
    let mut weak = false;
    if n_eff >= 10.0 {
        let (a, b, r1) = irls_quantile(&lc, &lm, w, 0.9);
        if r1 >= 0.7 && a.is_finite() && b.is_finite() {
            return (MemFit::Coupled { a, b, r1 }, false);
        }
        weak = true;
    }
    let (a, b, _sig) = wls_loglinear(&lc, &lm, w);
    if !a.is_finite() || !b.is_finite() {
        // bug_072: the degenerate design falls to THE mem
        // aggregator's all-rows p90 (provided by the caller — refit's
        // single chokepoint over the full row set), not a private
        // subset quantile over the c-axis rows alone.
        return (MemFit::Independent { p90: fallback_p90 }, weak);
    }
    (MemFit::Coupled { a, b, r1: 0.0 }, weak)
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

    /// Golden values (scipy.stats.t.ppf) pinning the Student-t quantile
    /// across distribution-crate changes — `z_q` scales every SLA bound.
    ///
    /// Tolerance 5e-5: distrs implements Hill's Algorithm 396, whose
    /// absolute error at the df=3 floor is ~2e-5 (it is ≤1e-8 for
    /// df ≥ 10). That is ~0.001% of z_q — far below what the sizing
    /// bound can resolve — while still failing loudly on a wrong tail,
    /// wrong df, or wrong algorithm (those are off by ≥1e-2).
    #[test]
    fn t_ppf_golden() {
        assert!((distrs::StudentsT::ppf(0.9, 3.0) - 1.637_744_353_696_210_2).abs() < 5e-5);
        assert!((distrs::StudentsT::ppf(0.95, 3.0) - 2.353_363_434_801_826_4).abs() < 5e-5);
        assert!((distrs::StudentsT::ppf(0.975, 10.0) - 2.228_138_851_964_938_5).abs() < 5e-5);
        assert!((distrs::StudentsT::ppf(0.9, 30.0) - 1.310_415_025_391_374_2).abs() < 5e-5);
    }

    #[test]
    fn kish_n_eff_uniform() {
        assert!((kish_n_eff(&[1.0; 10]) - 10.0).abs() < 1e-9);
    }

    #[test]
    fn kish_n_eff_one_dominant() {
        assert!(kish_n_eff(&[100.0, 1.0, 1.0, 1.0]) < 2.0);
    }

    // r[verify sched.sla.hw-class.sample-weight-ordinal]
    #[test]
    fn sample_weight_ordinal_halflife() {
        // 20 samples ago, vdist=0 → weight 0.5
        assert!((sample_weight(20, 0) - 0.5).abs() < 1e-9);
        // 0 samples ago, vdist=1 → weight 0.5
        assert!((sample_weight(0, 1) - 0.5).abs() < 1e-9);
        // 40 samples ago, vdist=2 → 0.25 · 0.25
        assert!((sample_weight(40, 2) - 0.0625).abs() < 1e-9);
    }

    // r[verify sched.sla.hw-class.sample-weight-ordinal]
    #[test]
    fn sample_weight_monthly_key_retains_neff() {
        // The bug ordinal weighting fixes: a monthly-built key under
        // wall-clock decay (halflife=7d) would asymptote at n_eff≈1.
        // Ordinal: a 32-slot ring at vdist=0 gives
        // n_eff = (Σ0.5^(i/20))² / Σ0.5^(2i/20) ≈ 18.7 — enough to
        // reach the USL stage.
        let w: Vec<f64> = (0..32).map(|i| sample_weight(i, 0)).collect();
        assert!(kish_n_eff(&w) > 15.0, "n_eff = {}", kish_n_eff(&w));
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(128))]
        // r[verify sched.sla.hw-class.sample-weight-ordinal]
        #[test]
        fn sample_weight_monotone_decreasing(age in 0u32..200, vd in 0u32..10) {
            proptest::prop_assert!(sample_weight(age + 1, vd) <= sample_weight(age, vd));
            proptest::prop_assert!(sample_weight(age, vd + 1) <= sample_weight(age, vd));
        }
    }

    // r[verify sched.sla.hw-class.zq-inflation]
    #[test]
    fn z_q_widens_at_low_neff() {
        // fit_df=3, n_distinct_c=3, n_par=2, sum_w=2.5.
        // df = max(3, min(3,3)-2) = 3; t_{0.9,3}=1.638; ×√(1+1/2.5)=1.937.
        let z = z_q(0.9, FitDf(3.0), 3, 2, 2.5);
        assert!((z - 1.937).abs() < 0.01, "z={z}");
    }

    #[test]
    fn z_q_asymptotes_to_ppf() {
        // Large fit_df, n_distinct_c, sum_w → Φ⁻¹(0.9)=1.2816.
        let z = z_q(0.9, FitDf(1e6), 1_000_000, 2, 1e6);
        assert!((z - 1.2816).abs() < 0.001, "z={z}");
    }

    // r[verify sched.sla.hw-class.zq-inflation]
    #[test]
    fn z_q_n_distinct_c_binds_post_convergence() {
        // n_eff=20 but n_distinct_c=3 (post-convergence: 20 effective
        // samples all at the same c) → df binds on n_distinct_c, not
        // n_eff. This is the case anchor-slots prevent from being
        // worse: without anchors n_distinct_c→1 and df floors at 3
        // forever.
        let z_bound = z_q(0.9, FitDf(20.0), 3, 2, 18.0);
        let z_unbound = z_q(0.9, FitDf(20.0), 20, 2, 18.0);
        assert!(z_bound > z_unbound + 0.2, "{z_bound} vs {z_unbound}");
    }

    #[test]
    fn z_q_sum_w_floored_at_1() {
        // sum_w<1 (heavily decayed ring) must not blow up √(1+1/Σw).
        let z = z_q(0.9, FitDf(5.0), 5, 2, 0.0);
        assert!(z.is_finite() && z > 0.0);
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
    fn usl_stage_at_n12_span10x() {
        // True USL: T = 30 + 2000/c + 0.5·c. n=12, span=20/2=10× → entry
        // gate met. Amdahl can't capture the +0.5·c tail → ΔAICc < −2 →
        // Usl chosen.
        let cs = [
            2.0, 3.0, 4.0, 5.0, 6.0, 8.0, 10.0, 12.0, 14.0, 16.0, 18.0, 20.0,
        ];
        let ts: Vec<f64> = cs.iter().map(|c| 30.0 + 2000.0 / c + 0.5 * c).collect();
        let w = vec![1.0; 12];
        let gate = StageGate {
            n_eff: 12.0,
            span: 10.0,
            p_bar: f64::INFINITY,
            prev_usl: false,
        };
        let (fit, sigma) = fit_duration_staged(&cs, &ts, &w, &gate);
        let DurationFit::Usl { q, .. } = fit else {
            panic!("expected Usl, got {fit:?}")
        };
        assert!((q - 0.5).abs() < 0.1, "q={q}");
        assert!(sigma < 0.01, "near-perfect 3-param fit, σ={sigma}");
    }

    #[test]
    fn usl_stays_capped_at_n8() {
        // Same true USL data; n_eff=8 < 10 → entry gate NOT met → 2-param.
        let cs = [2.0, 3.0, 4.0, 6.0, 8.0, 12.0, 16.0, 20.0];
        let ts: Vec<f64> = cs.iter().map(|c| 30.0 + 2000.0 / c + 0.5 * c).collect();
        let w = vec![1.0; 8];
        let gate = StageGate {
            n_eff: 8.0,
            span: 10.0,
            p_bar: f64::INFINITY,
            prev_usl: false,
        };
        let (fit, _) = fit_duration_staged(&cs, &ts, &w, &gate);
        assert!(matches!(fit, DurationFit::Amdahl { .. }), "{fit:?}");
        // Hysteresis: prev_usl latches → at n_eff=8 (≥7) we STAY Usl.
        let latched = StageGate {
            prev_usl: true,
            ..gate
        };
        let (fit, _) = fit_duration_staged(&cs, &ts, &w, &latched);
        assert!(matches!(fit, DurationFit::Usl { .. }), "latched: {fit:?}");
        // Exit at n_eff < 7.
        let exit = StageGate {
            n_eff: 6.0,
            prev_usl: true,
            ..gate
        };
        let (fit, _) = fit_duration_staged(&cs, &ts, &w, &exit);
        assert!(matches!(fit, DurationFit::Amdahl { .. }), "exit: {fit:?}");
    }

    #[test]
    fn usl_rejected_when_aicc_prefers_amdahl() {
        // True Amdahl (Q=0) + tiny noise. n=12, span=10× → gate met, but
        // 3-param doesn't beat 2-param by ΔAICc < −2 → stays Amdahl.
        let cs = [
            2.0, 3.0, 4.0, 5.0, 6.0, 8.0, 10.0, 12.0, 14.0, 16.0, 18.0, 20.0,
        ];
        let ts: Vec<f64> = cs
            .iter()
            .enumerate()
            .map(|(i, c)| (30.0 + 2000.0 / c) * (1.0 + 0.02 * (i as f64 * 1.3).sin()))
            .collect();
        let w = vec![1.0; 12];
        let gate = StageGate {
            n_eff: 12.0,
            span: 10.0,
            p_bar: f64::INFINITY,
            prev_usl: false,
        };
        let (fit, _) = fit_duration_staged(&cs, &ts, &w, &gate);
        assert!(matches!(fit, DurationFit::Amdahl { .. }), "{fit:?}");
    }

    #[test]
    fn ridge_shrinks_q_toward_zero() {
        let cs = [2.0, 4.0, 8.0, 16.0, 32.0, 64.0];
        let ts: Vec<f64> = cs.iter().map(|c| 30.0 + 2000.0 / c + 0.5 * c).collect();
        let w = vec![1.0; 6];
        // λ=0 → unregularized → Q≈0.5 (matches nnls_recovers_usl).
        let DurationFit::Usl { q: q0, .. } = fit_duration_ridge(&cs, &ts, &w, f64::INFINITY, 0.0)
        else {
            panic!()
        };
        assert!((q0 - 0.5).abs() < 0.05, "q0={q0}");
        // λ huge → Q shrunk toward 0.
        let DurationFit::Usl { q: q_hi, .. } =
            fit_duration_ridge(&cs, &ts, &w, f64::INFINITY, 1e12)
        else {
            panic!()
        };
        assert!(q_hi < 0.01, "q_hi={q_hi}");
        assert!(q_hi < q0);
    }

    #[test]
    fn sigma_resid_of_perfect_fit_near_zero() {
        let cs = [4.0, 8.0, 16.0, 32.0];
        let ts: Vec<f64> = cs.iter().map(|c| 30.0 + 2000.0 / c).collect();
        let w = vec![1.0; 4];
        let fit = fit_duration(&cs, &ts, &w, false, f64::INFINITY);
        assert!(sigma_resid(&cs, &ts, &w, &fit) < 1e-3);
    }

    #[test]
    fn headroom_at_1() {
        assert!((headroom(RingNEff(1.0)) - 1.95).abs() < 1e-6);
    }

    #[test]
    fn headroom_at_100() {
        assert!((headroom(RingNEff(100.0)) - 1.32).abs() < 1e-2);
    }

    #[test]
    fn headroom_clamps_below_1() {
        assert_eq!(headroom(RingNEff(0.1)), headroom(RingNEff(1.0)));
    }

    // r[verify sched.sla.mem-coupled]
    #[test]
    fn fit_memory_recovers_loglinear_at_n15() {
        // True model: log M = 2.0 + 0.7·log c, ±2.5% deterministic multiplicative noise.
        let cs: Vec<f64> = (1..=15).map(|i| (i * 2) as f64).collect();
        let ms: Vec<u64> = cs
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let noise = 1.0 + 0.05 * ((i as f64 * 2.399).sin() - 0.0);
                ((2.0 + 0.7 * c.ln()).exp() * noise) as u64
            })
            .collect();
        let w = vec![1.0; 15];
        let (MemFit::Coupled { b, r1, .. }, weak) = fit_memory(&cs, &ms, &w, 15.0, MemBytes(0))
        else {
            panic!("expected Coupled")
        };
        assert!((b - 0.7).abs() < 0.15, "b={b}");
        assert!(r1 >= 0.7, "r1={r1}");
        assert!(!weak);
    }

    #[test]
    fn fit_memory_small_n_uses_ols() {
        let cs = [4.0, 8.0, 16.0];
        let ms = [1000u64, 1500, 2200];
        let (MemFit::Coupled { r1, .. }, weak) = fit_memory(&cs, &ms, &[1.0; 3], 3.0, MemBytes(0))
        else {
            panic!("expected Coupled")
        };
        assert_eq!(r1, 0.0); // small-n sentinel
        assert!(!weak, "n_eff<10 → not weak (small-n is expected)");
    }

    #[test]
    fn fit_memory_reports_weak_on_low_r1() {
        // n_eff=15 with mem uncorrelated to c (constant + noise) →
        // IRLS R¹ < 0.7 → weak=true. Wires
        // `rio_scheduler_sla_mem_fit_weak_total` (described but never
        // emitted before).
        let cs: Vec<f64> = (1..=15).map(|i| (i * 2) as f64).collect();
        let ms: Vec<u64> = (1..=15)
            .map(|i| (1_000_000.0 * (1.0 + 0.05 * (i as f64 * 2.399).sin())) as u64)
            .collect();
        let (_, weak) = fit_memory(&cs, &ms, &[1.0; 15], 15.0, MemBytes(0));
        assert!(weak, "uncorrelated mem at n_eff=15 → weak");
    }

    /// Regression: accept-gate at `> NNLS_TOL`, alpha-filter at `<= 0.0`
    /// → a `z_p[k] ∈ (0, NNLS_TOL]` failed accept yet was excluded from
    /// alpha → `fold(∞, min) = ∞` → inner `loop {}` never broke. Ran
    /// sync inside `DagActor::handle_tick` so the scheduler froze.
    #[test]
    fn nnls_tiny_positive_zp_terminates() {
        // 1-col design where aᵀb / aᵀa ∈ (0, NNLS_TOL]: a=[[1e6]],
        // b=[1e-5] → z_p = 1e-11. Under the old `<= 0.0` filter this
        // hangs forever; with the unified threshold it terminates.
        let a = DMatrix::from_row_slice(1, 1, &[1e6]);
        let b = DVector::from_vec(vec![1e-5]);
        let start = std::time::Instant::now();
        let x = nnls(&a, &b);
        assert!(
            start.elapsed() < std::time::Duration::from_millis(200),
            "nnls must terminate (was: hang)"
        );
        assert!(x[0].is_finite());
    }

    /// Regression: `peak_memory_bytes = 0` (deliberately persisted by
    /// completion.rs) yields `ln(0) = -∞`; `wls_loglinear` then NaNs and
    /// the whole key collapsed from `Coupled` to `Independent`.
    #[test]
    fn fit_memory_tolerates_zero_sample() {
        // 10 log-linear points + one zero. The zero must not poison the
        // fit — it gets floored to `ln(1)=0` and IRLS down-weights it.
        let cs: Vec<f64> = [1.0, 2.0, 4.0, 8.0, 16.0]
            .into_iter()
            .cycle()
            .take(10)
            .chain([4.0])
            .collect();
        let ms: Vec<u64> = cs[..10]
            .iter()
            .map(|c| ((20.0 + 0.5 * c.ln()).exp()) as u64)
            .chain([0u64])
            .collect();
        let w = vec![1.0; 11];
        let (MemFit::Coupled { a, b, .. }, _) = fit_memory(&cs, &ms, &w, 11.0, MemBytes(0)) else {
            panic!("zero sample must not collapse Coupled → Independent");
        };
        assert!(a.is_finite() && b.is_finite(), "a={a} b={b}");
    }

    #[test]
    fn fit_memory_degenerate_falls_back_independent() {
        // All same c → slope undefined. bug_072: the degenerate arm
        // carries the CALLER-provided all-rows aggregate through (the
        // one mem chokepoint computes it; this pins the plumbing).
        let cs = [4.0, 4.0, 4.0];
        let ms = [1000u64, 1100, 1050];
        assert!(matches!(
            fit_memory(&cs, &ms, &[1.0; 3], 3.0, MemBytes(4096)),
            (
                MemFit::Independent {
                    p90: MemBytes(4096)
                },
                false
            )
        ));
    }

    #[test]
    fn memfit_at_roundtrips() {
        let f = MemFit::Coupled {
            a: 2.0,
            b: 0.7,
            r1: 0.9,
        };
        let m = f.at(RawCores(10.0)).0 as f64;
        let expected = (2.0 + 0.7 * 10.0_f64.ln()).exp();
        assert!((m - expected).abs() / expected < 1e-3);
        assert_eq!(
            MemFit::Independent {
                p90: MemBytes(4096)
            }
            .at(RawCores(64.0)),
            MemBytes(4096)
        );
    }
}

#[cfg(test)]
mod disk_envelope_tests {
    use super::*;

    // r[verify sched.sla.disk-reaches-ephemeral-storage+2]
    /// **R9 + W7-I** — the envelope ordering law `floor ≤ fitted ≤
    /// ceiling` over the ADVERSARIAL population (hand-oracle rows,
    /// never the impl's own min/max expression): zero observations,
    /// small observation, huge outlier above the ceiling, a prior
    /// above the ceiling, and an inverted configuration (ceiling below
    /// the floor — the operator bound saturates, typed). Each row pins
    /// the exact fitted value.
    #[test]
    fn disk_envelope_ordering_law_product() {
        const GI: u64 = 1 << 30;
        // (observed, prior, ceiling, want_fitted)
        let rows: &[(Option<u64>, u64, u64, u64)] = &[
            (None, 100 * GI, 200 * GI, 100 * GI),           // cold: prior
            (Some(2 * GI), 100 * GI, 200 * GI, 2 * GI),     // warm: p90 retires prior
            (Some(500 * GI), 100 * GI, 200 * GI, 200 * GI), // outlier: ceiling clamps
            (None, 500 * GI, 200 * GI, 200 * GI),           // prior above ceiling: clamps
            (Some(1), 100 * GI, 200 * GI, GI),              // degenerate tiny obs: floor
            (Some(2 * GI), 100 * GI, GI / 2, GI / 2),       // inverted: ceiling wins
        ];
        for &(obs, prior, ceiling, want) in rows {
            let e = DiskFitEnvelope::derive(obs.map(DiskBytes), prior, ceiling);
            assert_eq!(e.fitted.0, want, "row ({obs:?}, {prior}, {ceiling})");
            assert!(
                e.floor.0 <= e.fitted.0 && e.fitted.0 <= e.ceiling.0.max(e.floor.0),
                "ordering law on ({obs:?}, {prior}, {ceiling}): {e:?}"
            );
            assert_eq!(
                e.request().bytes(),
                e.fitted.0,
                "the request IS the fitted point"
            );
        }
    }

    /// The floor mirrors the trivial-builder `LOCAL_DISK_BYTES` lane
    /// (solve.rs, private const 1 GiB) — the literal pin keeps the two
    /// lanes in lockstep; a change to either side fails one of the two
    /// pins (this one or `intent_for_prefer_local_is_minimal`).
    #[test]
    fn disk_envelope_floor_mirrors_local_disk() {
        assert_eq!(DiskFitEnvelope::FLOOR_BYTES, 1 << 30);
    }
}
