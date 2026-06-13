//! Per-key refit: `Vec<BuildSampleRow>` → [`FittedParams`].
//!
//! The [`SlaEstimator`](super::SlaEstimator) cache calls [`refit`] once per
//! touched key on each refresh tick; the fit itself is pure (no DB, no I/O)
//! so it can be unit-tested against synthetic rows.

use std::collections::{BTreeSet, HashMap, HashSet};

use super::alpha;
use super::bootstrap::{WeightedSample, t_min_ci};
use super::fit::{
    StageGate, compute_vdists, fit_memory, kish_n_eff, sample_weight, weighted_quantile,
};
use super::hw::{HwTable, K};
use super::prior::{FitParams, PriorSources, partial_pool, prior_for};
use super::solve::Tier;
use super::types::{
    DiskBytes, DurationFit, ExploreState, FitDf, FittedParams, MemBytes, MemFit, ModelKey,
    RawCores, RawDiskP90, RefSeconds, RingNEff, WallSeconds,
};
use crate::db::BuildSampleRow;

/// Bootstrap replicates per CI recompute. 500 keeps the 80% CI stable to
/// ~2% across reseeds while staying <1ms per key on the refit path.
const BOOTSTRAP_REPS: usize = 500;

/// Partial-pool shrinkage `n0`. ADR-023 §2.10: at `n_eff = 3` the
/// per-key fit and the prior weigh equally; below that the prior
/// dominates.
const PARTIAL_POOL_N0: f64 = 3.0;

/// Hard floor on bootstrap-CI recompute interval. Decoupled from any
/// halflife (ordinal weighting has none): this is purely a
/// rate-limiter on the expensive 500×NNLS bootstrap under completion
/// storms.
const CI_DEBOUNCE_SECS: f64 = 30.0;

// r[impl sched.sla.hw-class.anchor-slots]
/// `cap`-slot sample ring with anchor reservation. ADR-023 L145: one
/// slot per distinct `cpu_limit` holds the highest-weight (lowest
/// vdist, then newest) sample at that c and is never displaced by
/// recency; remaining slots are recency-FIFO. [`Self::weighted_rows`]
/// applies a weight floor `0.5^vdist / n_anchors` to anchor rows so the
/// NNLS design matrix stays full-rank after convergence (when every
/// fresh sample lands at the same c* and the anchored explore-rows
/// would otherwise recency-decay to numerical zero).
///
/// Rows must be pushed completed_at-ascending (oldest first) — matches
/// `refit`'s input contract.
#[derive(Debug)]
pub struct AnchorRing {
    cap: usize,
    /// Push-ordered (oldest first).
    rows: Vec<(BuildSampleRow, u32 /* vdist */)>,
    /// `cpu_limit` (rounded) → index into `rows` of the anchor.
    anchors: HashMap<u32, usize>,
}

impl AnchorRing {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            rows: Vec::with_capacity(cap),
            anchors: HashMap::new(),
        }
    }

    /// Build from a completed_at-ascending slice + parallel vdists.
    /// `cap` here is the eviction threshold; `refit` passes the slice
    /// length so eviction is a no-op (DB already capped — and
    /// `trim_build_samples{,_batch}` preserves one row per distinct
    /// `cpu_limit`, so the DB cap honors the same anchor invariant).
    pub fn from_rows(cap: usize, rows: &[&BuildSampleRow], vdists: &[u32]) -> Self {
        debug_assert_eq!(rows.len(), vdists.len());
        let mut ring = Self::new(cap);
        for (r, &vd) in rows.iter().zip(vdists) {
            ring.push((*r).clone(), vd);
        }
        ring
    }

    /// Append a row (newest so far). Evicts the oldest non-anchor if
    /// over `cap`. O(n) eviction is fine for n ≤ 32+1.
    pub fn push(&mut self, row: BuildSampleRow, vdist: u32) {
        let c_key = row.cpu_limit_cores.map(|c| c.round() as u32);
        let i = self.rows.len();
        self.rows.push((row, vdist));
        // Anchor selection: lowest-vdist (highest version-weight),
        // tie-break newest. The newest row is always the anchor for a
        // first-seen c, so explore-ladder probes anchor immediately.
        if let Some(c) = c_key {
            match self.anchors.get(&c) {
                Some(&j) if self.rows[j].1 < vdist => {}
                _ => {
                    self.anchors.insert(c, i);
                }
            }
        }
        if self.rows.len() > self.cap {
            let anchor_set: HashSet<usize> = self.anchors.values().copied().collect();
            if let Some(victim) = (0..self.rows.len()).find(|i| !anchor_set.contains(i)) {
                self.rows.remove(victim);
                for v in self.anchors.values_mut() {
                    if *v > victim {
                        *v -= 1;
                    }
                }
            }
        }
    }

    /// Count of distinct `cpu_limit` values retained. Feeds
    /// [`FittedParams::n_distinct_c`] → [`super::fit::z_q`] df.
    pub fn n_distinct_c(&self) -> u32 {
        self.anchors.len() as u32
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// `(row, vdist, weight)` over all retained rows, oldest-first.
    /// `weight = sample_weight(ordinal_age, vdist)` with anchor floor
    /// `max(weight, 0.5^vdist / n_anchors)`. Ordinal age is over
    /// retained rows (newest = 0).
    pub fn weighted_rows(&self) -> impl Iterator<Item = (&BuildSampleRow, u32, f64)> + '_ {
        let anchor_set: HashSet<usize> = self.anchors.values().copied().collect();
        let n = self.rows.len();
        let n_anchors = self.anchors.len().max(1) as f64;
        self.rows.iter().enumerate().map(move |(i, (r, vd))| {
            let age = (n - 1 - i) as u32;
            let mut w = sample_weight(age, *vd);
            if anchor_set.contains(&i) {
                w = w.max(0.5f64.powi(*vd as i32) / n_anchors);
            }
            (r, *vd, w)
        })
    }
}

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
    prev: Option<&FittedParams>,
    tiers: &[Tier],
    hw: &HwTable,
    priors: Option<&PriorSources>,
) -> FittedParams {
    let now = now_epoch();
    // Filter to rows that can sit on a c-axis. Everything below — vdists,
    // weights, n_eff, span, fits — is computed on this consistent subset.
    let fit_rows: Vec<&BuildSampleRow> = rows
        .iter()
        .filter(|r| r.cpu_limit_cores.is_some())
        .collect();

    if fit_rows.is_empty() {
        return probe_only(key, rows, aggregate_disk_p90(rows, &[]));
    }

    let versions: Vec<_> = fit_rows.iter().map(|r| r.version.clone()).collect();
    let current_v = fit_rows.last().and_then(|r| r.version.clone());
    let vdists = compute_vdists(&versions, current_v.as_deref());

    // r[impl sched.sla.hw-class.anchor-slots]
    // r[impl sched.sla.hw-class.sample-weight-ordinal]
    // Anchor ring: identifies one anchor per distinct cpu_limit and
    // applies the weight floor. cap=len so eviction is a no-op here
    // (DB already capped at `ring_buffer`); the floor is the active
    // behaviour. Weights are ordinal — `completed_at` is no longer
    // read for weighting (only for the CI-debounce floor below).
    let ring = AnchorRing::from_rows(fit_rows.len(), &fit_rows, &vdists);
    let w: Vec<f64> = ring.weighted_rows().map(|(_, _, w)| w).collect();
    // Pre-filter ring n_eff: gates `als_fit`, `fit_memory`, the
    // partial-pool blend, and `headroom()` — all of which run on the
    // unfiltered ring. The dispatch gates at snapshot.rs:778 +
    // solve.rs:413 do NOT read this directly: they gate on `!Probe`,
    // and the als-gate below (`n_eff_ring < 3.0 || span < 4.0` →
    // `Probe`) is what actually enforces ring cardinality. The
    // POST-filter `fit_df` (for `z_q` / CI debounce / `is_outlier`) is
    // computed below after the `idx`/p̄ collinearity drop. Distinct
    // newtypes so a reader cannot get the other (R6B4 / bug_012).
    let n_eff_ring = RingNEff(kish_n_eff(&w));

    let cs: Vec<f64> = fit_rows
        .iter()
        .map(|r| r.cpu_limit_cores.unwrap())
        .collect();
    // r[impl sched.sla.hw-ref-seconds]
    // Time-domain → reference-seconds is now α-dependent (ALS step A
    // re-normalizes each round). Here we collect raw wall-clock + the
    // per-row K=3 factor; `ts` (ref-seconds) is computed AFTER ALS at
    // the converged α. Memory is NOT normalized (M(c) is fitted on raw
    // bytes — peak RSS is workload-dominated, not core-throughput).
    let walls: Vec<f64> = fit_rows.iter().map(|r| r.duration_secs).collect();
    let factors: Vec<Option<[f64; K]>> = fit_rows
        .iter()
        .map(|r| r.hw_class.as_deref().and_then(|h| hw.factor(h)))
        .collect();
    let ms: Vec<u64> = fit_rows
        .iter()
        .map(|r| r.peak_memory_bytes as u64)
        .collect();

    // ADR-023 §2.4 Capped stage: observed p̄ = recency-weighted p90 of
    // avg_cores; entered once any sample is unsaturated (peak < 0.85·limit).
    // Samples with c > p̄ are dropped from the duration fit — their basis
    // column 1/min(c,p̄) collapses to the constant 1/p̄ (collinear with S).
    let p_bar = observed_p_bar(&fit_rows, &w);
    let idx: Vec<usize> = if p_bar.is_finite() {
        let kept: Vec<usize> = (0..cs.len()).filter(|&i| cs[i] <= p_bar).collect();
        if kept.len() >= 2 {
            kept
        } else {
            (0..cs.len()).collect()
        }
    } else {
        (0..cs.len()).collect()
    };
    let cs_f: Vec<f64> = idx.iter().map(|&i| cs[i]).collect();
    let walls_f: Vec<f64> = idx.iter().map(|&i| walls[i]).collect();
    let factors_f: Vec<_> = idx.iter().map(|&i| factors[i]).collect();
    let w_f: Vec<f64> = idx.iter().map(|&i| w[i]).collect();
    // z_q inputs MUST describe the post-filter row set: `als_fit`,
    // `sigma_resid`, `log_residuals`, and the bootstrap CI all run on
    // `(cs_f, w_f)`, so the df/Σw that `z_q()` reads must be stated at
    // the same granularity. Distinct newtype from `n_eff_ring` above.
    // The dispatch gates at snapshot.rs:778 + solve.rs:413 do NOT read
    // this — they gate on `!Probe` only (R6B4: `!Probe ⟹
    // n_eff_ring≥3 ∧ span≥4` from the als-gate below). See
    // docs/REVIEW.md §Granularity-coupling.
    let fit_df = FitDf(kish_n_eff(&w_f));
    let sum_w_f: f64 = w_f.iter().sum();
    let n_distinct_c_f = cs_f
        .iter()
        .map(|c| c.round() as u32)
        .collect::<BTreeSet<_>>()
        .len() as u32;

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
    let gate = StageGate {
        n_eff: n_eff_ring.0,
        span,
        p_bar,
        prev_usl: matches!(prev.map(|p| &p.fit), Some(DurationFit::Usl { .. })),
    };
    // r[impl sched.sla.prior-partial-pool]
    // Resolve the prior FIRST: `als_fit` needs `θ_prior.α` (ADR L547:
    // seed → fleet-median → uniform precedence, same machinery as the
    // M(c) prior); the (S,P,Q,a,b) blend below needs the rest. With
    // priors disabled (`None`) α-prior degrades to UNIFORM.
    let theta_prior = priors.map(|src| prior_for(key, src));
    let alpha_prior = theta_prior
        .as_ref()
        .map_or(alpha::UNIFORM, |(p, _)| p.alpha);
    // r[impl sched.sla.hw-class.alpha-als+2]
    // When the rank gate never passes — single hw_class, all-NULL, or
    // isotropic factors — als_fit returns (fit_duration_staged, prior,
    // 1): the pre-ALS behaviour. The I/O-saturation `ioseq` seed (ADR
    // L547 first arm) is Task A9 once `io_bytes` lands in
    // BuildSampleRow; until then the prior chain bottoms out at UNIFORM.
    //
    // The n_eff gate is intentionally PRE-filter: the p̄ collinearity
    // drop can leave 2 post-filter rows (kept.len()≥2 above), which is
    // sufficient for the 2-param Capped/Amdahl fit. `z_q()` (fit.rs:37)
    // floors df at 3 as the backstop for the stored post-filter
    // `fit_df`/`n_distinct_c_f`, so a 2-row post-filter fit gets the
    // widest (most conservative) prediction interval rather than being
    // rejected outright.
    let (mut fit, sigma, alpha) = if n_eff_ring.0 < 3.0 || span < 4.0 {
        (DurationFit::Probe, 0.2, alpha_prior)
    } else {
        let (f, s, a, rounds) =
            alpha::als_fit(&cs_f, &walls_f, &factors_f, &w_f, &gate, alpha_prior);
        if matches!(rounds, alpha::AlsRounds::CapHit) {
            ::metrics::counter!(
                "rio_scheduler_sla_als_round_cap_hit_total",
                "tenant" => key.tenant.clone()
            )
            .increment(1);
        }
        // R6B4 tripwire: `als_fit` must never return Probe on the
        // ≥3∧≥4× branch — if a future early-return does, the dispatch
        // gates' `!Probe ⟹ n_eff_ring≥3 ∧ span≥4` invariant breaks.
        debug_assert!(!matches!(f, DurationFit::Probe));
        (f, s, a)
    };
    // Reference-seconds at the converged α — feeds hw_bias / residuals /
    // bootstrap CI below (all of which want T_ref-domain).
    let scale = |i: usize| alpha::dot(alpha, factors[i].unwrap_or([1.0; K]));
    let ts: Vec<f64> = (0..walls.len()).map(|i| walls[i] * scale(i)).collect();
    let ts_f: Vec<f64> = idx.iter().map(|&i| ts[i]).collect();
    // bug_072: the degenerate-Independent arm consumes THE mem
    // aggregator's all-rows fold — same evidence universe as the
    // probe arm (one aggregation fn per scalar axis, every arm).
    let (mut mem, weak) = fit_memory(&cs, &ms, &w, n_eff_ring.0, aggregate_mem_p90(rows, &w));
    if weak {
        ::metrics::counter!(
            "rio_scheduler_sla_mem_fit_weak_total",
            "tenant" => key.tenant.clone()
        )
        .increment(1);
    }

    // Shrinkage blend: w·θ_pname + (1−w)·θ_prior with w = n_eff/(n_eff+n0).
    // Probe fits have no θ_pname so they record provenance only (the
    // explore path doesn't read the curve anyway). Non-Probe fits get
    // their (S,P,Q,a,b) blended toward the prior — at n_eff=3 it's a
    // 50/50 mix; by n_eff≈30 the prior is <10% and effectively gone.
    // α is NOT re-blended here: `als_fit` warm-starts at `α_prior` (so a
    // rank-gated / under-determined design returns it unchanged) and its
    // output respects the simplex constraint, which a post-hoc linear
    // blend would not. The per-round ridge is toward the previous
    // iterate (NOT `α_prior`) — see `r[sched.sla.hw-class.alpha-als+2]`.
    let prior_source = theta_prior.map(|(theta_prior, prov)| {
        if !matches!(fit, DurationFit::Probe) {
            let theta_pname = extract_fit_params(&fit, &mem, alpha);
            let pooled = partial_pool(&theta_pname, n_eff_ring.0, &theta_prior, PARTIAL_POOL_N0);
            apply_pooled(&mut fit, &mut mem, &pooled);
        }
        prov
    });

    // r[impl sched.sla.disk-reaches-ephemeral-storage+2]
    // live_049 L2 as repaired by bug_070: disk is a c-INDEPENDENT
    // scalar (sched.sla.disk-scalar) — its evidence universe is EVERY peaked sample — quantifier: census(test: w11_bc_axis_arm_census) —
    // never an implicit property of which Vec an arm
    // collected. The previous emptiness-gated fallback engaged only
    // when the c-axis subset carried ZERO peaks, so one peaked c-axis
    // row silently dropped every peaked legacy row (p90 of N+1
    // collapsed to 1). The one chokepoint now folds BOTH populations
    // always: ring weights where the row holds a c-axis seat, unit
    // weights elsewhere. Both polarity faces destructure from the ONE
    // mint (sched.sla.disk-polarity-fork).
    let disk_fork = aggregate_disk_p90(rows, &w);
    let (disk_p90, disk_p90_raw) = match disk_fork {
        Some(f) => (Some(f.floored), Some(f.raw)),
        None => (None, None),
    };

    let log_residuals: Vec<f64> = if matches!(fit, DurationFit::Probe) {
        Vec::new()
    } else {
        cs_f.iter()
            .zip(&ts_f)
            .map(|(&c, &t)| (t / fit.t_at(RawCores(c)).0).ln())
            .collect()
    };
    // Hartigan dip on the residual distribution. Multimodal ⇔ the
    // single-curve model is structurally wrong for this key (two
    // workloads sharing a pname). Emitted per-refit, not per-sample —
    // it's a property of the ring as a whole. ≤32 points → O(n²) dip
    // is <10µs; the metric is the operator signal, not a hard gate.
    if super::dip::is_multimodal(&log_residuals) {
        ::metrics::counter!(
            "rio_scheduler_sla_residual_multimodal_total",
            "tenant" => key.tenant.clone()
        )
        .increment(1);
    }

    // r[impl sched.sla.reassign-schmitt]
    // Bootstrap CI is the expensive bit (~500 NNLS refits). Debounce so a
    // burst of completions on one key doesn't refit-storm: keep prev CI
    // unless the point estimate moved by half a CI width, n_eff jumped,
    // or it's been long enough. Probe fits skip CI entirely (no T_min).
    let (ci, ci_at) = if matches!(fit, DurationFit::Probe) {
        (None, None)
    } else if should_recompute_ci(prev, &fit, fit_df, now) {
        let ws: Vec<WeightedSample> = cs_f
            .iter()
            .zip(&ts_f)
            .zip(&w_f)
            .map(|((&c, &t), &w)| WeightedSample { c, t, w })
            .collect();
        let unfreeze_q = matches!(fit, DurationFit::Usl { .. });
        (
            t_min_ci(&ws, BOOTSTRAP_REPS, fit.p_bar().0, unfreeze_q),
            Some(now),
        )
    } else {
        (
            prev.and_then(|p| p.t_min_ci),
            prev.and_then(|p| p.ci_computed_at),
        )
    };
    let tier = reassign_tier(
        prev.and_then(|p| p.tier.as_deref()),
        ci,
        hw.min_factor(alpha),
        tiers,
    );

    FittedParams {
        key: key.clone(),
        hw_bias: hw_bias(&fit_rows, &cs, &ts, &fit),
        alpha,
        fit,
        mem,
        disk_p90,
        disk_p90_raw,
        sigma_resid: sigma,
        log_residuals,
        n_eff_ring,
        fit_df,
        n_distinct_c: n_distinct_c_f,
        sum_w: sum_w_f,
        span,
        explore,
        t_min_ci: ci,
        ci_computed_at: ci_at,
        tier,
        prior_source,
        // §A17: any row with is_fixed_output=true marks the key FOD.
        // (pname, system) is stable across a FOD's lifetime so one row
        // suffices; pre-057 NULL → false (no exclusion, old behavior).
        is_fod: rows.iter().any(|r| r.is_fixed_output == Some(true)),
    }
}

/// Project `(DurationFit, MemFit)` → flat `(S, P, Q, a, b)`. `MemFit::
/// Independent` has no (a, b); we substitute `(ln p90, 0)` so the pooled
/// `a` still lands somewhere sensible if the prior is Coupled (b=0 ⇔
/// flat M(c), which is what Independent means).
fn extract_fit_params(fit: &DurationFit, mem: &MemFit, alpha: alpha::Alpha) -> FitParams {
    let (s, p, q) = fit.spq();
    let (a, b) = match mem {
        MemFit::Coupled { a, b, .. } => (*a, *b),
        MemFit::Independent { p90 } => ((p90.0.max(1) as f64).ln(), 0.0),
    };
    FitParams {
        s,
        p,
        q,
        a,
        b,
        alpha,
    }
}

/// Write pooled `(S, P, Q, a, b)` back into the fit/mem variants
/// in-place. Variant is preserved (an Amdahl fit stays Amdahl, just with
/// shrunk S/P); `p_bar` is structural (observed saturation), not a
/// regressed scalar, so it's left untouched.
fn apply_pooled(fit: &mut DurationFit, mem: &mut MemFit, pooled: &FitParams) {
    match fit {
        DurationFit::Probe => {}
        DurationFit::Amdahl { s, p } => {
            *s = RefSeconds(pooled.s);
            *p = RefSeconds(pooled.p);
        }
        DurationFit::Capped { s, p, .. } => {
            *s = RefSeconds(pooled.s);
            *p = RefSeconds(pooled.p);
        }
        DurationFit::Usl { s, p, q, .. } => {
            *s = RefSeconds(pooled.s);
            *p = RefSeconds(pooled.p);
            *q = pooled.q;
        }
    }
    if let MemFit::Coupled { a, b, .. } = mem {
        *a = pooled.a;
        *b = pooled.b;
    }
}

/// Per-hw_class residual bias for THIS key: `median(t_ref / T_ref(c))`
/// over each hw_class's samples. Gated on ≥3 samples per class — fewer
/// → that class is omitted (caller defaults to 1.0).
///
/// `ts` are ALREADY hw-normalized (reference-seconds), so a bias ≠ 1.0
/// means this pname's scaling on that hw_class disagrees with the
/// fleet-wide CRC32-bench factor (e.g. mem-bandwidth-bound builds see
/// less speedup on a fast-core class than the bench predicts).
// r[impl sched.sla.hw-ref-seconds]
fn hw_bias(
    rows: &[&BuildSampleRow],
    cs: &[f64],
    ts: &[f64],
    fit: &DurationFit,
) -> HashMap<String, f64> {
    if matches!(fit, DurationFit::Probe) {
        return HashMap::new();
    }
    let mut by_class: HashMap<String, Vec<f64>> = HashMap::new();
    for ((r, &c), &t) in rows.iter().zip(cs).zip(ts) {
        let Some(h) = r.hw_class.as_deref() else {
            continue;
        };
        let pred = fit.t_at(RawCores(c)).0;
        if pred > 0.0 && pred.is_finite() {
            by_class.entry(h.to_owned()).or_default().push(t / pred);
        }
    }
    by_class
        .into_iter()
        .filter(|(_, v)| v.len() >= 3)
        .map(|(h, v)| (h, median(&v)))
        .collect()
}

/// MAD-based outlier gate for one new sample against the PREVIOUS fit.
///
/// A sample is an outlier if its absolute log-residual against `fit`'s
/// curve exceeds `3 · 1.4826 · MAD(prev_residuals)` — the standard
/// 3σ-equivalent under a normal-MAD scale (1.4826·MAD ≈ σ for normal
/// data). The MAD is floored at `sigma_resid / 1.4826` (so a near-zero
/// MAD on a tight fit doesn't reject everything) and at the relative
/// poll-granularity `dt_poll / wall_t` (both wall-seconds → hw-invariant:
/// a 1s cgroup poll on a 10s build is ±10% noise on its own; don't call
/// that an outlier).
///
/// `ref_t` is the hw-normalized reference-seconds duration (matching
/// `fit`'s curve); `wall_t` is the raw wall-clock duration. The
/// log-residual needs the former; the poll-granularity floor needs the
/// latter — keeping both explicit prevents the unit mismatch that
/// `c6163485` left behind.
///
/// Gated on `n_eff ≥ 5`: with fewer effective samples MAD is unstable
/// and the explore ladder is still walking — rejecting then would
/// throw away exactly the diversity the fit needs.
// r[impl sched.sla.outlier-mad-reject]
pub fn is_outlier(
    ref_t: f64,
    wall_t: f64,
    sample_c: f64,
    fit: &FittedParams,
    dt_poll: f64,
) -> bool {
    if fit.fit_df.0 < 5.0 || fit.log_residuals.is_empty() {
        return false;
    }
    let predicted = fit.fit.t_at(RawCores(sample_c)).0;
    if !predicted.is_finite() || predicted <= 0.0 || ref_t <= 0.0 {
        return false;
    }
    // Center on the residual median: `log_residuals` are computed
    // against the post-pooled fit, so a divergent prior puts a
    // systematic offset on every residual. MAD cancels it; the test
    // value must too, or on-curve samples get flagged when n_eff is in
    // the partial-pool window.
    let med = median(&fit.log_residuals);
    let log_resid = ((ref_t / predicted).ln() - med).abs();
    let mad = median_abs_dev(&fit.log_residuals);
    let floor = (fit.sigma_resid / 1.4826).max(dt_poll / wall_t);
    log_resid > 3.0 * 1.4826 * mad.max(floor)
}

/// Observed parallelism cap p̄: recency-weighted p90 of per-sample
/// `avg_cores = cpu_seconds_total / duration_secs`.
///
/// Returns `∞` (no cap → Amdahl stage) unless at least one sample is
/// **unsaturated** (`peak_cpu < 0.85·cpu_limit`): only an unsaturated
/// sample is evidence the build can't soak the cores it was given. Rows
/// missing `cpu_seconds_total` are skipped from the quantile.
///
/// `avg_cores = cpu_seconds / wall` is hw-invariant (both terms scale by
/// the same `α·factor`), so this is computed on raw wall — no `HwTable`
/// dependency, and no circular α-dep into the ALS gate it feeds.
/// THE avg-cores producer (bug_014, R33: one quantity, one producing
/// fn): `cpu_seconds_total / duration_secs`, TOTAL over its domain —
/// `None` when the wall is non-positive, so the distant producer-side
/// gate (the completion path binds `duration_secs > 0.0`) is locally
/// irrelevant to every consumer. Both consult sites (`observed_p_bar`,
/// `derive_explore_state`) import; neither re-decides the restriction.
fn avg_cores(r: &BuildSampleRow) -> Option<f64> {
    r.cpu_seconds_total
        .filter(|_| r.duration_secs > 0.0)
        .map(|ct| ct / r.duration_secs)
}

fn observed_p_bar(rows: &[&BuildSampleRow], w: &[f64]) -> f64 {
    let any_unsat = rows.iter().any(|r| {
        matches!(
            (r.peak_cpu_cores, r.cpu_limit_cores),
            (Some(pk), Some(lim)) if pk < 0.85 * lim
        )
    });
    if !any_unsat {
        return f64::INFINITY;
    }
    let (avg, aw): (Vec<f64>, Vec<f64>) = rows
        .iter()
        .zip(w)
        .filter_map(|(r, &wi)| avg_cores(r).map(|a| (a, wi)))
        .unzip();
    if avg.is_empty() {
        f64::INFINITY
    } else {
        weighted_quantile(&avg, &aw, 0.9)
    }
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
/// [`CI_DEBOUNCE_SECS`] of the last bootstrap regardless of the above
/// — bounds the per-key bootstrap rate under completion storms.
pub(super) fn should_recompute_ci(
    prev: Option<&FittedParams>,
    new_fit: &DurationFit,
    new_fit_df: FitDf,
    now: f64,
) -> bool {
    let Some(prev) = prev else { return true };
    // Floor first — it's unconditional, and `ci_computed_at` is set
    // even when bootstrap returned `None` (rank-deficient resamples), so
    // a None-CI key still rate-limits to once per 30s.
    if let Some(at) = prev.ci_computed_at
        && now - at < CI_DEBOUNCE_SECS
    {
        return false;
    }
    let Some((plo, phi)) = prev.t_min_ci else {
        return true;
    };
    if prev.fit_df.0 > 0.0 && (prev.fit_df.0 - new_fit_df.0).abs() / prev.fit_df.0 > 0.5 {
        return true;
    }
    let width = phi.0 - plo.0;
    (prev.fit.t_min().0 - new_fit.t_min().0).abs() > width / 2.0
}

/// Schmitt-trigger tier reassignment with a 0.85/1.05 deadband on
/// [`Tier::binding_bound`]. `tiers` must be sorted tightest-first.
/// Returns the new tier name, or `prev` unchanged when there is no CI or
/// no bounded tiers.
///
/// `min_factor`: ref→wall denorm at the slowest admitted hw_class,
/// mirroring `solve_intent_for`'s deadline path ([`HwTable::min_factor`]).
/// `ci` is in reference-seconds; tier bounds are operator-facing wall-
/// seconds, so the comparison uses `hi_wall = ci.hi / min_factor`.
///
/// Starting position is `prev`'s index in the bounded-tier list (or
/// loosest if `prev` is None / unbounded / unknown). From there,
/// **promote** while `hi_wall < 0.85 · tighter.binding_bound`, **demote**
/// while `hi_wall > 1.05 · current.binding_bound`. The 20-point deadband
/// means a key oscillating around a tier boundary stays put instead of
/// flapping on every refit.
// r[impl sched.sla.reassign-schmitt]
pub(super) fn reassign_tier(
    prev: Option<&str>,
    ci: Option<(RefSeconds, RefSeconds)>,
    min_factor: f64,
    tiers: &[Tier],
) -> Option<String> {
    let Some((_, hi)) = ci else {
        return prev.map(String::from);
    };
    let hi_wall = hi.0 / min_factor;
    let bounded: Vec<(&str, f64)> = tiers
        .iter()
        .filter_map(|t| t.binding_bound().map(|b| (t.name.as_str(), b)))
        .collect();
    if bounded.is_empty() {
        return prev.map(String::from);
    }
    let mut i = prev
        .and_then(|p| bounded.iter().position(|(n, _)| *n == p))
        .unwrap_or(bounded.len() - 1);
    loop {
        if i > 0 && hi_wall < 0.85 * bounded[i - 1].1 {
            i -= 1;
        } else if i + 1 < bounded.len() && hi_wall > 1.05 * bounded[i].1 {
            i += 1;
        } else {
            break;
        }
    }
    Some(bounded[i].0.to_string())
}

// r[impl sched.sla.disk-reaches-ephemeral-storage+2]
/// live_049 L2: the disk p90 over EVERY row carrying a peak, unit
/// weights — the probe-shaped fit's disk aggregate. Pre-fix the
/// probe_only arm read ONLY the last row (a serial pname with five
/// observed peaks whose newest row lacked one fell back to the 100 GiB
/// chart default forever — the un-fitted population the live ramp
/// measured at ~189-201 GiB/pod). bug_070: this fn is now THE
/// disk-axis chokepoint for every arm (full-fit and probe alike) —
/// the evidence universe is always the full row set via
/// [`axis_samples`], so a subset-quantile is unwritable rather than
/// comment-policed.
// r[impl sched.sla.one-aggregator+2]
/// THE mem-axis aggregation chokepoint (bug_072 — the disk fn's
/// structurally identical c-independent sibling): the recency-weighted
/// p90 the `MemFit::Independent` variant doc promises, over EVERY sample — quantifier: census(test: w11_bc_axis_arm_census) —
/// (the mem peak column is non-optional, so the all-rows
/// aggregate is always available) — ring weights where the row holds a
/// c-axis seat, unit weights elsewhere. Consumed by the probe arm AND
/// the full-fit degenerate-Independent arm (`fit_memory`), so
/// estimator quality no longer depends on which arm a pname lands in.
fn aggregate_mem_p90(rows: &[BuildSampleRow], fit_w: &[f64]) -> MemBytes {
    let (vals, ws) = axis_samples(rows, fit_w, |r| Some(r.peak_memory_bytes as f64));
    if vals.is_empty() {
        return MemBytes(0);
    }
    MemBytes(weighted_quantile(&vals, &ws, 0.9) as u64)
}

/// The two polarity faces of the warm disk fit, minted together
/// behind the ONE witness gate (sched.sla.disk-polarity-fork): `raw`
/// is the witnessed weighted p90 (the reject/explain face), `floored`
/// is `max(p90, newest x DISK_SHRINK_HEADROOM)` (the sizing face).
/// One mint site ⇒ the faces cannot diverge in `Some`-ness.
pub(super) struct DiskP90Fork {
    pub raw: RawDiskP90,
    pub floored: DiskBytes,
}

// r[impl sched.sla.one-aggregator+2]
// r[impl sched.sla.disk-polarity-fork]
fn aggregate_disk_p90(rows: &[BuildSampleRow], fit_w: &[f64]) -> Option<DiskP90Fork> {
    let (vals, ws) = axis_samples(rows, fit_w, |r| r.peak_disk_bytes.map(|b| b as f64));
    // live060-c: the population gate lives INSIDE the one producer —
    // BOTH polarity faces are gated (the single-sample hazard has a
    // reject face too; gating in `DiskFitEnvelope::derive` instead
    // would leave `exceeds_ceiling` consuming an un-gated quantity).
    // The reader set and each reader's direction are DERIVED, not
    // narrated: see the polarity-rider census
    // (`w13_polarity_rider_census`, this module's test tier) — every
    // `disk_p90`/`disk_p90_raw` consult site carries a committed
    // {direction, units} row and the raw/floored split binds at
    // consumer SIGNATURES (merged_bug_002: the retired prose census
    // here claimed measure-compatibility for the reject readers that
    // the shrink floor falsifies).
    // `vals` is completed_at-ascending (refit's input contract), so
    // the last element IS the newest observed peak.
    if vals.len() < DISK_WITNESS_MIN_PEAKS {
        return None;
    }
    let p90 = weighted_quantile(&vals, &ws, 0.9);
    let newest = vals.last().copied().unwrap_or(0.0);
    Some(DiskP90Fork {
        raw: RawDiskP90(DiskBytes(p90 as u64)),
        floored: DiskBytes(p90.max(newest * DISK_SHRINK_HEADROOM) as u64),
    })
}

/// live060-c: the disk fit's WITNESS GATE — the warm fit is consumed
/// only at a witnessed population; below this many observed peaks the
/// producer mints `None` and the `sla.defaultDisk` prior stands at
/// every consumer (today's de-facto fleet behavior: the live builder
/// fleet records NO peaks until prjquota provisioning lands, and the
/// FIRST sparse observations after it must not retire the prior — a
/// single unrepresentative small build would collapse the disk
/// request fleet-wide). Mirrors the in-crate `n_eff >= 3` als-gate
/// idiom. R17-VIOLABLE typed envelope constant with the
/// measured-at-first-samples rider: re-derive against the first
/// soak's real peak distribution once provisioning produces fleet
/// observations (the owner-queue readback names this re-derivation).
const DISK_WITNESS_MIN_PEAKS: usize = 3;

/// live060-c: the SHRINK FLOOR's headroom over the newest observed
/// peak — the witnessed fit never sits below recent observed reality
/// plus this margin (`max(p90, newest_peak × 1.2)`).
/// Prior-independent by design (the producer cannot see the
/// envelope's prior): in the shrink direction it is the hysteresis
/// law; in the growth direction it only ADDS headroom above the
/// newest peak (conservative). R17-VIOLABLE, same
/// measured-at-first-samples rider as the gate.
const DISK_SHRINK_HEADROOM: f64 = 1.2;

// r[impl sched.sla.one-weight-law]
/// Shared population law for the per-axis aggregation chokepoints:
/// the evidence universe is ALWAYS the full row set — quantifier: census(test: w11_bc_axis_arm_census) — and EVERY row's
/// fold weight derives from the ONE decay law
/// [`sample_weight`]`(ordinal_age, vdist)` — quantifier: census(test: w12_ad_weight_census) —
/// total over sub-populations (merged_bug_022). A row holding a c-axis
/// ring seat (`cpu_limit_cores.is_some()`, in row order) carries its
/// ring weight from `fit_w` (the same law plus the anchor floor,
/// computed once in `refit`); a row without a seat (legacy
/// no-cpu_limit history) derives the same law from the shared
/// completed_at ordering — the slice index IS its ordinal (the
/// pre-fix "they carry no ordinal" premise was FALSE: the slice is
/// completed_at-ascending) — with vdist floored at 0. `fit_w` empty
/// (the probe arm computes no ring) ⇒ every row takes the unseated
/// law. There is NO exempt default: a flat unit weight let the oldest
/// evidence structurally outweigh all fresh evidence until ring
/// eviction, pinning disk_p90 at legacy peaks and falsely tripping
/// `exceeds_ceiling` tier rejection.
fn axis_samples(
    rows: &[BuildSampleRow],
    fit_w: &[f64],
    value: impl Fn(&BuildSampleRow) -> Option<f64>,
) -> (Vec<f64>, Vec<f64>) {
    let n = rows.len();
    let unseated = |i: usize| sample_weight((n - 1 - i) as u32, 0);
    let mut seat = 0usize;
    let mut vals = Vec::new();
    let mut ws = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        let wgt = if r.cpu_limit_cores.is_some() && !fit_w.is_empty() {
            let x = fit_w.get(seat).copied();
            seat += 1;
            // A ring shorter than the seat walk is a refit-construction
            // bug (both derive from the same fit_rows slice) — fall to
            // the law, never to an exempt flat weight.
            debug_assert!(x.is_some(), "fit_w shorter than the seat walk");
            x.unwrap_or_else(|| unseated(i))
        } else {
            unseated(i)
        };
        if let Some(v) = value(r) {
            vals.push(v);
            ws.push(wgt);
        }
    }
    (vals, ws)
}

/// No usable c-axis samples → emit a Probe placeholder so the explore
/// ladder (Task 5.2) can pick a first c. The newest row (if any) seeds
/// `last_wall`; memory comes from THE mem aggregator over every sample
/// (bug_072 — the pre-fix newest-row read erased the multi-sample
/// consensus the variant doc promises).
fn probe_only(
    key: &ModelKey,
    rows: &[BuildSampleRow],
    disk_fork: Option<DiskP90Fork>,
) -> FittedParams {
    let last = rows.last();
    let (disk_p90, disk_p90_raw) = match disk_fork {
        Some(f) => (Some(f.floored), Some(f.raw)),
        None => (None, None),
    };
    FittedParams {
        key: key.clone(),
        fit: DurationFit::Probe,
        mem: MemFit::Independent {
            p90: aggregate_mem_p90(rows, &[]),
        },
        disk_p90,
        disk_p90_raw,
        sigma_resid: 0.2,
        log_residuals: Vec::new(),
        n_eff_ring: RingNEff(0.0),
        fit_df: FitDf(0.0),
        n_distinct_c: 0,
        sum_w: 0.0,
        span: 1.0,
        explore: derive_explore_state(&[], last),
        t_min_ci: None,
        ci_computed_at: None,
        tier: None,
        hw_bias: HashMap::new(),
        alpha: alpha::UNIFORM,
        prior_source: None,
        is_fod: last.is_some_and(|r| r.is_fixed_output == Some(true)),
    }
}

/// Reconstruct [`ExploreState`] inputs (min_c/max_c/distinct/saturated/
/// last_wall) from observed current-version cpu_limits. The freeze
/// predicate is evaluated by callers via [`super::explore::frozen`] —
/// refit doesn't have the config-side `max_cores` so cannot precompute it.
fn derive_explore_state(cur_cs: &[f64], last: Option<&BuildSampleRow>) -> ExploreState {
    let distinct: HashSet<u64> = cur_cs.iter().map(|c| c.to_bits()).collect();
    let (min_c, max_c) = cur_cs
        .iter()
        .fold((f64::INFINITY, 0.0_f64), |(lo, hi), &c| {
            (lo.min(c), hi.max(c))
        });
    let min_c = if min_c.is_finite() { min_c } else { 0.0 };
    // "Saturated" = last build's mean utilisation (avg-cores / limit)
    // exceeded 40% — i.e. the build actually used the cores it was
    // given, so probing higher is worth it. The ratio derives through
    // THE one producer (`avg_cores` — bug_014): pre-fix this site
    // re-decided the domain restriction inline with NO zero-duration
    // guard while observed_p_bar filtered the identical ratio, so a
    // positive-cpu/zero-wall row read +inf > 0.4 and spuriously
    // marked the key saturated.
    let saturated = last
        .and_then(|r| {
            avg_cores(r)
                .zip(r.cpu_limit_cores)
                .map(|(a, lim)| a / lim > 0.4)
        })
        .unwrap_or(false);
    ExploreState {
        distinct_c: distinct.len() as u8,
        min_c: RawCores(min_c),
        max_c: RawCores(max_c),
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
        refit(&key(), rows, None, &[], &HwTable::default(), None)
    }

    fn r_hw(rows: &[BuildSampleRow], hw: &HwTable) -> FittedParams {
        refit(&key(), rows, None, &[], hw, None)
    }

    /// **W12-AJ (bug_014)** — *the avg-cores domain restriction is
    /// decided ONCE, in the single producer; population: both consult
    /// sites.* A positive-cpu/zero-wall row (manual-SQL/fixture-only:
    /// the production writer gate held in every era — the triage
    /// correction; pure defensive-parity): pre-fix
    /// `derive_explore_state` computed `secs / 0.0 / lim = +inf > 0.4`
    /// inline and spuriously marked the key saturated, while sibling
    /// `observed_p_bar` filtered the identical ratio — two consumers
    /// re-deciding one derived quantity's domain. Post-fix both
    /// import `avg_cores` and agree: None.
    #[test]
    fn w12_aj_avg_cores_domain_decided_once() {
        let mut z = row(4.0, 0.0);
        z.cpu_seconds_total = Some(50.0); // positive cpu, zero wall
        assert_eq!(avg_cores(&z), None, "the producer refuses zero wall");
        let e = derive_explore_state(&[], Some(&z));
        assert!(
            !e.saturated,
            "a zero-wall row cannot mark the key saturated (pre-fix: \
             +inf > 0.4 — the inline re-decision)"
        );
        // The live direction preserved: a genuine saturated row still
        // saturates (avg/limit = 0.5 > 0.4) and the producer agrees
        // with the raw ratio on its lawful domain.
        let r = row(4.0, 100.0);
        assert_eq!(avg_cores(&r), Some(2.0));
        assert!(derive_explore_state(&[], Some(&r)).saturated);
    }

    fn r_prior(rows: &[BuildSampleRow], priors: &PriorSources) -> FittedParams {
        refit(&key(), rows, None, &[], &HwTable::default(), Some(priors))
    }

    /// Like `row` but with explicit peak_cpu and avg_cores so Capped-stage
    /// detection (`observed_p_bar`) has data to chew on.
    fn row_util(c: f64, t: f64, peak: f64, avg: f64) -> BuildSampleRow {
        BuildSampleRow {
            peak_cpu_cores: Some(peak),
            cpu_seconds_total: Some(t * avg),
            ..row(c, t)
        }
    }

    #[test]
    fn refit_amdahl_when_span_and_neff_sufficient() {
        let rows: Vec<_> = [4.0, 8.0, 16.0, 32.0, 64.0]
            .into_iter()
            .map(|c| row(c, 30.0 + 2000.0 / c))
            .collect();
        let f = r(&rows);
        assert!(matches!(f.fit, DurationFit::Amdahl { .. }), "{:?}", f.fit);
        assert!(f.n_eff_ring.0 > 4.9, "n_eff_ring={:?}", f.n_eff_ring);
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
        assert_eq!(f.n_eff_ring, RingNEff(0.0));
        assert_eq!(f.fit_df, FitDf(0.0));
        assert_eq!(f.explore.distinct_c, 0);
    }

    // ─── Task 10.3: hw normalization ─────────────────────────────────────

    // r[verify sched.sla.hw-ref-seconds]
    #[test]
    fn refit_normalizes_mixed_hw_to_same_t() {
        // hw_class A: factor=1.0, T(c)=30+2000/c → wall = T(c).
        // hw_class B: factor=2.0 (twice as fast) → wall = T(c)/2.
        // After normalize(): both map to T_ref(c) = 30+2000/c. The fit
        // should recover (S,P) ≈ (30,2000) regardless of the hw mix.
        let mut m = std::collections::HashMap::new();
        m.insert("A".to_string(), 1.0);
        m.insert("B".to_string(), 2.0);
        let hw = HwTable::from_map(m);

        let mk = |c: f64, h: &str, f: f64| BuildSampleRow {
            hw_class: Some(h.into()),
            duration_secs: (30.0 + 2000.0 / c) / f,
            ..row(c, 0.0) // duration_secs overwritten above; row() filler
        };
        let rows = vec![
            mk(4.0, "A", 1.0),
            mk(8.0, "B", 2.0),
            mk(16.0, "A", 1.0),
            mk(32.0, "B", 2.0),
            mk(64.0, "A", 1.0),
        ];
        let f = r_hw(&rows, &hw);
        let DurationFit::Amdahl { s, p } = f.fit else {
            panic!("expected Amdahl, got {:?}", f.fit);
        };
        assert!((s.0 - 30.0).abs() < 1.0, "S={s:?}");
        assert!((p.0 - 2000.0).abs() < 5.0, "P={p:?}");
        // hw_bias: each class has ≥3 samples? A=3, B=2 → only A reported.
        // A's bias is median(t_ref/T_ref(c)) = 1.0 (perfect data).
        assert!((f.hw_bias.get("A").copied().unwrap_or(0.0) - 1.0).abs() < 0.01);
        assert!(!f.hw_bias.contains_key("B"), "<3 samples → omitted");
    }

    #[test]
    fn refit_hw_bias_detects_per_pname_disagreement() {
        // Fleet says B is 2× faster (factor=2.0), but THIS pname only
        // sees 1.5× on B (mem-bandwidth-bound). After normalization,
        // B-samples land at t_ref = wall × 2.0 = T_ref(c) × (2.0/1.5)
        // → bias[B] ≈ 1.33.
        let mut m = std::collections::HashMap::new();
        m.insert("A".to_string(), 1.0);
        m.insert("B".to_string(), 2.0);
        let hw = HwTable::from_map(m);
        let mk = |c: f64, h: &str, real_f: f64| BuildSampleRow {
            hw_class: Some(h.into()),
            duration_secs: (30.0 + 2000.0 / c) / real_f,
            ..row(c, 0.0)
        };
        let rows = vec![
            mk(4.0, "A", 1.0),
            mk(8.0, "A", 1.0),
            mk(16.0, "A", 1.0),
            mk(4.0, "B", 1.5),
            mk(32.0, "B", 1.5),
            mk(64.0, "B", 1.5),
        ];
        let f = r_hw(&rows, &hw);
        // The fit averages A (bias 1.0) and B (bias ~1.33) samples, so
        // the absolute (S,P) is contaminated — the per-class hw_bias is
        // exactly the corrective the solve will apply.
        let b = f.hw_bias.get("B").copied().expect("B has 3 samples");
        assert!(b > 1.2 && b < 1.5, "bias[B]={b} (want ~1.33)");
    }

    // ─── Task 9.1: observed p̄ + Capped stage ─────────────────────────────

    #[test]
    fn capped_stage_enters_on_first_unsaturated() {
        // True T(c) = 30 + 2000/min(c, 8). c∈{2,4,8} are saturated (peak ≈
        // 0.95·c ≥ 0.85·c); c∈{16,32} cap at eff≈8 → peak≈7.6 < 0.85·limit
        // → unsaturated → Capped stage entered.
        // avg_cores = 0.9·min(c,8) = [1.8, 3.6, 7.2, 7.2, 7.2] → p90 = 7.2.
        let mk = |c: f64| {
            let eff = c.min(8.0);
            row_util(c, 30.0 + 2000.0 / eff, eff * 0.95, eff * 0.9)
        };
        let rows: Vec<_> = [2.0, 4.0, 8.0, 16.0, 32.0].into_iter().map(mk).collect();
        let f = r(&rows);
        let DurationFit::Capped { s, p, p_bar } = f.fit else {
            panic!("expected Capped, got {:?}", f.fit)
        };
        assert!((p_bar.0 - 7.2).abs() < 0.1, "p̄={}", p_bar.0);
        assert!((s.0 - 30.0).abs() < 1.0, "s={}", s.0);
        assert!((p.0 - 2000.0).abs() < 50.0, "p={}", p.0);
    }

    #[test]
    fn capped_drops_samples_above_pbar() {
        // Same setup; p̄=7.2 → samples at c∈{8,16,32} dropped (basis col
        // 1/min(c,7.2)=const for those). log_residuals length reflects the
        // filtered fit set.
        let mk = |c: f64| {
            let eff = c.min(8.0);
            row_util(c, 30.0 + 2000.0 / eff, eff * 0.95, eff * 0.9)
        };
        let rows: Vec<_> = [2.0, 4.0, 8.0, 16.0, 32.0].into_iter().map(mk).collect();
        let f = r(&rows);
        assert!(matches!(f.fit, DurationFit::Capped { .. }));
        assert_eq!(
            f.log_residuals.len(),
            2,
            "c>{:.1} dropped → 2 of 5 kept",
            f.fit.p_bar().0
        );
    }

    #[test]
    fn capped_not_entered_when_all_saturated() {
        // peak = 0.95·c everywhere → no unsaturated sample → p̄=∞ → Amdahl.
        let rows: Vec<_> = [2.0, 4.0, 8.0, 16.0, 32.0]
            .into_iter()
            .map(|c| row_util(c, 30.0 + 2000.0 / c, c * 0.95, c * 0.9))
            .collect();
        let f = r(&rows);
        assert!(matches!(f.fit, DurationFit::Amdahl { .. }), "{:?}", f.fit);
        assert_eq!(f.log_residuals.len(), 5, "no filter when p̄=∞");
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
                1.0,
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
                1.0,
                &tiers
            ),
            Some("normal".into())
        );
        // Already at tightest → no further promote.
        assert_eq!(
            reassign_tier(
                Some("fast"),
                Some((RefSeconds(10.0), RefSeconds(50.0))),
                1.0,
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
                1.0,
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
                1.0,
                &tiers
            ),
            Some("normal".into())
        );
        // Already at loosest → no further demote.
        assert_eq!(
            reassign_tier(
                Some("slow"),
                Some((RefSeconds(5000.0), RefSeconds(9000.0))),
                1.0,
                &tiers
            ),
            Some("slow".into())
        );
    }

    #[test]
    fn schmitt_no_ci_keeps_prev() {
        let tiers = ladder();
        assert_eq!(
            reassign_tier(Some("normal"), None, 1.0, &tiers),
            Some("normal".into())
        );
        assert_eq!(reassign_tier(None, None, 1.0, &tiers), None);
        // Empty tier list → keeps prev.
        assert_eq!(
            reassign_tier(
                Some("x"),
                Some((RefSeconds(1.0), RefSeconds(2.0))),
                1.0,
                &[]
            ),
            Some("x".into())
        );
    }

    #[test]
    fn schmitt_no_prev_walks_from_loosest() {
        let tiers = ladder();
        // No prev, ci.hi=250 → start at slow, promote to normal (250<0.85·1200),
        // promote to fast (250<0.85·300).
        assert_eq!(
            reassign_tier(
                None,
                Some((RefSeconds(200.0), RefSeconds(250.0))),
                1.0,
                &tiers
            ),
            Some("fast".into())
        );
        // No prev, ci.hi=2000 → start at slow, 2000>0.85·1200 (no promote),
        // 2000<1.05·3600 (no demote) → slow.
        assert_eq!(
            reassign_tier(
                None,
                Some((RefSeconds(1000.0), RefSeconds(2000.0))),
                1.0,
                &tiers
            ),
            Some("slow".into())
        );
    }

    // r[verify sched.sla.reassign-schmitt]
    #[test]
    fn schmitt_monotone_with_mixed_percentile_tiers() {
        // solve_tiers() and reassign_tier() must agree on which percentile
        // is binding. With mixed p50/p90, fast.binding_bound=600 and
        // normal.binding_bound=300 → sorts [normal, fast].
        let mut solved = vec![
            Tier {
                name: "fast".into(),
                p50: Some(60.0),
                p90: Some(600.0),
                p99: None,
            },
            Tier {
                name: "normal".into(),
                p50: Some(120.0),
                p90: Some(300.0),
                p99: None,
            },
        ];
        // SlaConfig::solve_tiers() sort, inlined (SlaConfig isn't Default).
        solved.sort_by_key(|t| {
            t.binding_bound()
                .map(|d| (d * 1000.0) as u64)
                .unwrap_or(u64::MAX)
        });
        assert_eq!(solved[0].name, "normal", "tightest binding_bound first");
        // ci.hi=640 > 1.05·600 → demote off fast. But the only looser
        // bounded tier is normal (bound=300 < 640) — the walk would
        // mis-land there if sort-key (p50) ≠ binding-key (p90). With both
        // on binding_bound(), bounded=[normal:300, fast:600], start at
        // fast (idx 1), 640>1.05·600 → can't demote past end → stays fast.
        let got = reassign_tier(
            Some("fast"),
            Some((RefSeconds(500.0), RefSeconds(640.0))),
            1.0,
            &solved,
        );
        assert_ne!(
            got,
            Some("normal".into()),
            "must not demote into a TIGHTER bound"
        );
    }

    #[test]
    fn schmitt_denormalizes_ref_to_wall() {
        let tiers = ladder();
        // min_factor=2.0 (slowest hw is 2× reference): ci.hi=500 ref-sec
        // → 250 wall-sec on the slowest band; 250 < 0.85·300 → promote.
        // Without denorm, 500 > 255 → would stay normal.
        assert_eq!(
            reassign_tier(
                Some("normal"),
                Some((RefSeconds(400.0), RefSeconds(500.0))),
                2.0,
                &tiers
            ),
            Some("fast".into())
        );
    }

    fn prev_with_ci(t_min: f64, lo: f64, hi: f64, fit_df: f64, at: f64) -> FittedParams {
        FittedParams {
            key: key(),
            fit: DurationFit::Amdahl {
                s: RefSeconds(t_min),
                p: RefSeconds(0.0),
            },
            mem: MemFit::Independent { p90: MemBytes(0) },
            disk_p90: None,
            disk_p90_raw: None,
            sigma_resid: 0.1,
            log_residuals: Vec::new(),
            n_eff_ring: RingNEff(fit_df),
            fit_df: FitDf(fit_df),
            n_distinct_c: 5,
            sum_w: fit_df,
            span: 8.0,
            explore: ExploreState {
                distinct_c: 3,
                min_c: RawCores(1.0),
                max_c: RawCores(32.0),
                saturated: false,
                last_wall: WallSeconds(0.0),
            },
            t_min_ci: Some((RefSeconds(lo), RefSeconds(hi))),
            ci_computed_at: Some(at),
            tier: None,
            hw_bias: HashMap::new(),
            alpha: alpha::UNIFORM,
            prior_source: None,
            is_fod: false,
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
            FitDf(5.0),
            1020.0
        ));
        // Same prev, now=1040 → elapsed=40s > 30s → ΔT_min trigger fires.
        assert!(should_recompute_ci(
            Some(&prev),
            &new_fit,
            FitDf(5.0),
            1040.0
        ));
    }

    #[test]
    fn debounce_recomputes_on_neff_jump() {
        let new_fit = DurationFit::Amdahl {
            s: RefSeconds(100.0),
            p: RefSeconds(0.0),
        };
        // ΔT_min=0, but fit_df 5→12 (>50% jump) → recompute.
        let prev = prev_with_ci(100.0, 50.0, 150.0, 5.0, 1000.0);
        assert!(should_recompute_ci(
            Some(&prev),
            &new_fit,
            FitDf(12.0),
            1040.0
        ));
        // fit_df 5→6 (<50%) and ΔT_min=0 → keep.
        assert!(!should_recompute_ci(
            Some(&prev),
            &new_fit,
            FitDf(6.0),
            1040.0
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
        assert!(fit.fit_df.0 >= 5.0, "precondition: fit_df={:?}", fit.fit_df);
        assert!(!fit.log_residuals.is_empty());
        let pred = fit.fit.t_at(RawCores(8.0)).0;
        // 10× predicted → ln(10)≈2.3 absolute log-resid. 5% noise gives
        // MAD ~0.03; gate = 3·1.4826·max(MAD, σ/1.4826) ≈ 3σ ≈ 0.15.
        // factor=1 here (test data are reference-seconds) → ref_t==wall_t.
        assert!(
            is_outlier(pred * 10.0, pred * 10.0, 8.0, &fit, 1.0),
            "10× → flagged"
        );
        // 1.2× predicted → ln(1.2)≈0.18, borderline; 1.05× must pass.
        assert!(
            !is_outlier(pred * 1.05, pred * 1.05, 8.0, &fit, 1.0),
            "5% → kept"
        );
    }

    #[test]
    fn mad_gated_below_neff_5() {
        let mut fit = outlier_fit();
        fit.fit_df = FitDf(4.0);
        let pred = fit.fit.t_at(RawCores(8.0)).0;
        assert!(
            !is_outlier(pred * 10.0, pred * 10.0, 8.0, &fit, 1.0),
            "fit_df=4 → never flag"
        );
        // No residuals (Probe fit) → never flag regardless of fit_df.
        fit.fit_df = FitDf(10.0);
        fit.log_residuals.clear();
        assert!(!is_outlier(pred * 10.0, pred * 10.0, 8.0, &fit, 1.0));
    }

    #[test]
    fn mad_floor_from_dt_poll() {
        // Perfect fit (zero MAD, zero σ) → floor must come from
        // dt_poll/wall_t. At c=8 predicted=280; dt_poll=28 → floor=0.1,
        // gate = 3·1.4826·0.1 ≈ 0.44.
        let rows: Vec<_> = [4.0, 8.0, 12.0, 16.0, 32.0, 64.0]
            .into_iter()
            .map(|c| row(c, 30.0 + 2000.0 / c))
            .collect();
        let fit = r(&rows);
        assert!(fit.sigma_resid < 1e-3, "perfect fit");
        assert!(fit.fit_df.0 >= 5.0, "fit_df={:?}", fit.fit_df);
        let pred = fit.fit.t_at(RawCores(8.0)).0; // ≈ 280
        // ln(1.3)≈0.26 < 0.44 → kept; ln(2)≈0.69 > 0.44 → flagged.
        assert!(!is_outlier(pred * 1.3, pred * 1.3, 8.0, &fit, 28.0));
        assert!(is_outlier(pred * 2.0, pred * 2.0, 8.0, &fit, 28.0));
        // dt_poll=0 → floor 0 → near-zero gate → 1.3× IS flagged.
        assert!(is_outlier(pred * 1.3, pred * 1.3, 8.0, &fit, 0.0));
    }

    // r[verify sched.sla.outlier-mad-reject]
    #[test]
    fn mad_floor_is_hw_invariant() {
        // The dt_poll floor is a wall-second ÷ wall-second ratio, so the
        // outlier verdict must be identical for any hw_factor. Perfect
        // fit (zero σ/MAD), dt_poll=1s.
        let rows: Vec<_> = [4.0, 8.0, 12.0, 16.0, 32.0, 64.0]
            .into_iter()
            .map(|c| row(c, 30.0 + 2000.0 / c))
            .collect();
        let fit = r(&rows);
        assert!(fit.sigma_resid < 1e-3, "perfect fit");
        let pred = fit.fit.t_at(RawCores(8.0)).0;
        // wall_t=5, dt_poll=1 → floor=0.2, gate≈0.89.
        assert!(
            !is_outlier(pred * 1.5, 5.0, 8.0, &fit, 1.0),
            "ln(1.5)≈0.405 < 0.89"
        );
        assert!(
            is_outlier(pred * 3.0, 5.0, 8.0, &fit, 1.0),
            "ln(3)≈1.10 > 0.89"
        );
        // factor-3 hw observing the SAME ref_t residual: wall_t=5/3≈1.67
        // → floor=0.6, gate≈2.67. The 1.5× residual is still kept —
        // pre-6956f6ea passed ref_t (≈420) for wall_t, giving floor≈0.0024
        // and gate≈0.011, which WOULD have flagged it.
        assert!(
            !is_outlier(pred * 1.5, 5.0 / 3.0, 8.0, &fit, 1.0),
            "wall_t=1.67 → floor=0.6, gate≈2.67; ln(1.5)≈0.405 still kept"
        );
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
        assert!(should_recompute_ci(None, &f, FitDf(5.0), 1000.0));
        // prev exists but bootstrap has never run → recompute.
        let mut prev = prev_with_ci(100.0, 50.0, 150.0, 5.0, 1000.0);
        prev.t_min_ci = None;
        prev.ci_computed_at = None;
        assert!(should_recompute_ci(Some(&prev), &f, FitDf(5.0), 1000.0));
    }

    #[test]
    fn recompute_ci_floor_gates_none_ci() {
        // Bootstrap ran (ci_computed_at=Some) but returned None
        // (rank-deficient resamples). The 30s floor must still apply —
        // not refit-storm 500×NNLS every tick.
        let f = DurationFit::Amdahl {
            s: RefSeconds(100.0),
            p: RefSeconds(0.0),
        };
        let mut prev = prev_with_ci(100.0, 50.0, 150.0, 6.0, 100.0);
        prev.t_min_ci = None;
        assert!(
            !should_recompute_ci(Some(&prev), &f, FitDf(6.0), 101.0),
            "within 30s floor"
        );
        assert!(
            should_recompute_ci(Some(&prev), &f, FitDf(6.0), 200.0),
            "past floor"
        );
    }

    // r[verify sched.sla.outlier-mad-reject]
    #[test]
    fn mad_robust_to_divergent_prior() {
        // True curve T(c)=30+2000/c; prior says P=500 (4× too small).
        // Partial pooling drags the fitted curve toward the prior, so
        // every log_residual carries a systematic positive offset. The
        // MAD test value must be centered on that median offset — an
        // on-curve sample is NOT an outlier.
        let rows: Vec<_> = [4.0, 8.0, 12.0, 16.0, 32.0, 64.0]
            .into_iter()
            .map(|c| row(c, 30.0 + 2000.0 / c))
            .collect();
        let priors = PriorSources {
            seed: HashMap::new(),
            fleet: Some(FitParams {
                s: 30.0,
                p: 500.0,
                q: 0.0,
                a: ((256_u64 << 20) as f64).ln(),
                b: 1.0,
                alpha: alpha::UNIFORM,
            }),
            operator: super::super::config::ProbeShape {
                cpu: 4.0,
                mem_per_core: 1 << 30,
                mem_base: 1 << 30,
                deadline_secs: 3600,
            },
            default_tier_target: 300.0,
        };
        let fit = r_prior(&rows, &priors);
        assert!(fit.fit_df.0 >= 5.0);
        let med = median(&fit.log_residuals);
        assert!(
            med.abs() > 0.05,
            "precondition: prior shifted residuals (med={med})"
        );
        // On-curve at c=8 → T=280. Must NOT be flagged.
        assert!(
            !is_outlier(280.0, 280.0, 8.0, &fit, 1.0),
            "on-curve sample survives a divergent prior"
        );
    }

    // ─── A5: anchor-slot ring buffer ─────────────────────────────────────

    fn mock_sample(c: f64, vdist_tag: u32) -> (BuildSampleRow, u32) {
        (
            BuildSampleRow {
                cpu_limit_cores: Some(c),
                duration_secs: 30.0 + 2000.0 / c,
                completed_at: now_epoch(),
                ..Default::default()
            },
            vdist_tag,
        )
    }

    // r[verify sched.sla.hw-class.anchor-slots]
    #[test]
    fn anchor_slots_preserve_span_after_convergence() {
        // 3 explore samples at c={4,16,32}, then 40 converged samples at
        // c=8. Without anchors: ring holds 32×c=8, design matrix rank-1.
        // With anchors: c={4,16,32} survive (never displaced by recency).
        let mut ring = AnchorRing::new(32);
        for c in [4.0, 16.0, 32.0] {
            let (r, vd) = mock_sample(c, 0);
            ring.push(r, vd);
        }
        for _ in 0..40 {
            let (r, vd) = mock_sample(8.0, 0);
            ring.push(r, vd);
        }
        let distinct: HashSet<u32> = ring
            .weighted_rows()
            .map(|(r, _, _)| r.cpu_limit_cores.unwrap().round() as u32)
            .collect();
        assert!(distinct.contains(&4));
        assert!(distinct.contains(&16));
        assert!(distinct.contains(&32));
        assert!(distinct.contains(&8));
        assert_eq!(ring.n_distinct_c(), 4);
        assert_eq!(ring.len(), 32, "capped at 32");
    }

    // r[verify sched.sla.hw-class.anchor-slots]
    #[test]
    fn anchor_weight_floor_prevents_rank_degeneration() {
        // Anchor at c=4 has ordinal_age≈31 → recency weight 0.5^(31/20)
        // ≈ 0.341. Floor: 0.5^0 / n_anchors = 1/3 ≈ 0.333. At age 31 the
        // recency weight still exceeds the floor; push past 32 so the
        // ring is full and the anchor's age is pinned at 31 (oldest
        // retained), but at vdist=2 the unfloored weight would be
        // 0.341 · 0.25 ≈ 0.085 — floor lifts to 0.25/3 ≈ 0.083. Use
        // a wider gap: 200 pushes → anchors age past the recency
        // window entirely (ordinal_age = 31 still — they're retained at
        // the front), so test the floor against a vdist-decayed anchor.
        let mut ring = AnchorRing::new(32);
        let (r, _) = mock_sample(4.0, 0);
        ring.push(r, 3); // vdist=3 → 0.5^3 = 0.125 unfloored cap
        let (r, _) = mock_sample(32.0, 0);
        ring.push(r, 3);
        for _ in 0..200 {
            let (r, vd) = mock_sample(8.0, 0);
            ring.push(r, vd);
        }
        // c=4 anchor: ordinal_age = 31 (oldest retained), vdist=3.
        // Unfloored: 0.5^(31/20) · 0.5^3 ≈ 0.341 · 0.125 ≈ 0.0427.
        // Floor: 0.5^3 / 3 ≈ 0.0417. Recency still wins by a hair —
        // so go further: vdist=5 → unfloored 0.341·0.03125=0.0107;
        // floor 0.03125/3=0.0104. The floor is `0.5^vd / n_anchors`,
        // which is by construction ≤ the vdist-only weight at age=0;
        // its job is to bound the ORDINAL-decay arm, not vdist. Reset
        // and test the ordinal arm directly with vdist=0 anchors and a
        // very full ring.
        let mut ring = AnchorRing::new(32);
        let (r, _) = mock_sample(4.0, 0);
        ring.push(r, 0);
        let (r, _) = mock_sample(32.0, 0);
        ring.push(r, 0);
        for _ in 0..30 {
            let (r, vd) = mock_sample(8.0, 0);
            ring.push(r, vd);
        }
        // 32 retained, c=4 anchor at index 0 → age=31. Unfloored
        // 0.5^1.55 ≈ 0.341. Floor 1/3 ≈ 0.333. Unfloored wins. Now
        // grow the cap so age can exceed the floor crossover
        // (age > 20·log₂(n_anchors) = 20·log₂(3) ≈ 31.7).
        let mut ring = AnchorRing::new(64);
        let (r, _) = mock_sample(4.0, 0);
        ring.push(r, 0);
        let (r, _) = mock_sample(32.0, 0);
        ring.push(r, 0);
        for _ in 0..62 {
            let (r, vd) = mock_sample(8.0, 0);
            ring.push(r, vd);
        }
        // c=4 anchor at age=63: unfloored 0.5^3.15 ≈ 0.113. Floor 1/3.
        let (_, _, w4) = ring
            .weighted_rows()
            .find(|(r, _, _)| (r.cpu_limit_cores.unwrap() - 4.0).abs() < 0.1)
            .unwrap();
        assert!(
            w4 >= 1.0 / 3.0 - 1e-6,
            "anchor weight {w4} below floor 1/3 (unfloored ≈ 0.113)"
        );
    }

    #[test]
    fn anchor_ring_prefers_low_vdist_then_newest() {
        let mut ring = AnchorRing::new(32);
        // c=8 at vdist=2 (older), then vdist=0 (newer).
        ring.push(mock_sample(8.0, 0).0, 2);
        ring.push(mock_sample(8.0, 0).0, 0);
        ring.push(mock_sample(8.0, 0).0, 1);
        // Anchor for c=8 must be the vdist=0 row (index 1).
        assert_eq!(ring.anchors[&8], 1, "lowest-vdist wins");
        // Tie-break: two vdist=0 rows → newest.
        ring.push(mock_sample(8.0, 0).0, 0);
        assert_eq!(ring.anchors[&8], 3, "tie-break newest");
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(64))]
        // r[verify sched.sla.hw-class.anchor-slots]
        #[test]
        fn anchor_ring_n_distinct_c_le_len(
            cs in proptest::collection::vec(1u32..16, 1..100),
        ) {
            let mut ring = AnchorRing::new(32);
            for &c in &cs {
                ring.push(mock_sample(f64::from(c), 0).0, 0);
            }
            proptest::prop_assert!(ring.n_distinct_c() as usize <= ring.len());
            proptest::prop_assert!(ring.len() <= 32.max(ring.n_distinct_c() as usize));
            // Every distinct c pushed survives as an anchor.
            let pushed: HashSet<u32> = cs.iter().copied().collect();
            proptest::prop_assert_eq!(ring.n_distinct_c() as usize, pushed.len());
        }
    }

    #[test]
    fn refit_reports_n_distinct_c_and_sum_w() {
        let rows: Vec<_> = [4.0, 8.0, 16.0, 32.0, 64.0]
            .into_iter()
            .map(|c| row(c, 30.0 + 2000.0 / c))
            .collect();
        let f = r(&rows);
        assert_eq!(f.n_distinct_c, 5);
        // Σw with ordinal weights age={4,3,2,1,0}, vdist=0, anchor floor
        // 1/5=0.2: ages 4..=0 → 0.5^{0.2,0.15,0.1,0.05,0} =
        // {0.871,0.901,0.933,0.966,1.0}; all > 0.2 so floor inert.
        assert!(f.sum_w > 4.5 && f.sum_w < 5.0, "Σw={}", f.sum_w);
    }

    #[test]
    fn z_q_inputs_post_filter() {
        // r[verify sched.sla.hw-class.zq-inflation]
        // 8 samples at c={2,4,8,16,32,64,128,256} with workload p̄≈12:
        // c≤12 saturated (peak=avg=c), c>12 unsaturated (peak=avg=12).
        // observed_p_bar → ~12, so the collinearity filter keeps only
        // c∈{2,4,8}. Stored z_q inputs MUST describe that 3-row subset
        // — NOT the 8-row pre-filter ring (bug_023: anti-conservative
        // df overstated → z_q under-widened).
        let cs = [2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0, 256.0];
        let rows: Vec<_> = cs
            .into_iter()
            .map(|c: f64| {
                let p = c.min(12.0);
                row_util(c, 30.0 + 240.0 / p, p, p)
            })
            .collect();
        let f = r(&rows);
        assert_eq!(f.n_distinct_c, 3, "post-filter distinct c, NOT 8");
        assert!(
            f.fit_df.0 > 2.5 && f.fit_df.0 < 3.5,
            "post-filter fit_df≈3 NOT 8; got {:?}",
            f.fit_df
        );
        assert!(
            f.n_eff_ring.0 > 7.5,
            "pre-filter n_eff_ring≈8; got {:?}",
            f.n_eff_ring
        );
        assert!(
            f.sum_w > 2.0 && f.sum_w < 3.0,
            "post-filter Σw over 3 rows NOT 8; got {}",
            f.sum_w
        );
    }
}

#[cfg(test)]
mod disk_axis_tests {
    use super::*;
    use crate::sla::types::ModelKey;

    fn key() -> ModelKey {
        ModelKey {
            pname: "p".into(),
            system: "x86_64-linux".into(),
            tenant: "t".into(),
        }
    }

    /// One sample with an optional c-axis seat and disk peak.
    fn disk_row(c: Option<f64>, t: f64, peak_disk: Option<i64>) -> BuildSampleRow {
        BuildSampleRow {
            pname: "p".into(),
            system: "x86_64-linux".into(),
            tenant: "t".into(),
            duration_secs: t,
            peak_memory_bytes: 256 << 20,
            cpu_limit_cores: c,
            cpu_seconds_total: c.map(|cc| t * cc * 0.5),
            peak_disk_bytes: peak_disk,
            completed_at: now_epoch(),
            ..Default::default()
        }
    }

    // r[verify sched.sla.one-aggregator+2]
    /// **W11-BC — the [GEN-SET] (axis, arm) census (the
    /// sched.sla.one-aggregator belt).** Generator: scan ingest.rs +
    /// fit.rs production source (test modules stripped) for the
    /// per-axis evidence reads. Committed expectation (re-derived on
    /// any red):
    ///
    /// - `peak_disk_bytes` reads in ingest.rs: exactly ONE — the disk
    ///   chokepoint's `axis_samples` closure;
    /// - mem-peak reads in ingest.rs: exactly TWO — the mem
    ///   chokepoint's closure + the COUPLED fit's c-axis regression
    ///   input `ms` (a fitted curve consumes per-row values; it is
    ///   not a quantile arm);
    /// - fit.rs: ZERO private quantiles over raw `ms` (the degenerate
    ///   arm consumes the caller-provided aggregate) and zero disk
    ///   reads.
    ///
    /// A new arm that collects its own Vec instead of routing through
    /// the axis's one aggregator drifts a count here.
    #[test]
    fn w11_bc_axis_arm_census() {
        let strip = |src: &str| {
            src.split_once("#[cfg(test)]\nmod tests")
                .map_or(src, |(p, _)| p)
                .to_owned()
        };
        let ingest = strip(include_str!("ingest.rs"));
        let fit = strip(include_str!("fit.rs"));
        assert_eq!(
            ingest.matches("peak_disk_bytes").count(),
            1,
            "disk evidence reads route through the one chokepoint"
        );
        assert_eq!(
            ingest.matches("peak_memory_bytes").count(),
            2,
            "mem evidence reads: the chokepoint closure + the coupled \
             fit's regression input only"
        );
        assert_eq!(
            fit.matches("peak_disk_bytes").count() + fit.matches("weighted_quantile(&mf").count(),
            0,
            "fit.rs holds no private per-axis quantile"
        );
    }

    /// **W11-BB (bug_072, red-first)** — *the probe arm's memory
    /// estimate aggregates over the consensus, never the single
    /// newest sample.* Five legacy (no-cpu_limit) rows — the
    /// population the live fleet docs name — with peaks
    /// [10,10,10,10,2] GiB, newest last (a cache-warm rebuild):
    /// pre-fix `probe_only` minted `MemFit::Independent` from the
    /// newest row alone (2 GiB), erasing the multi-sample consensus
    /// and burning the OOM floor-doubling ladder (while one anomalous
    /// LARGE newest sample would pin the estimate high) — against the
    /// variant doc's own "recency-weighted p90" promise. Post-fix the
    /// one mem aggregator folds every sample.
    #[test]
    fn w11_bb_probe_arm_memory_aggregates_over_the_consensus() {
        const GI: i64 = 1 << 30;
        let mem_row = |peak: i64| BuildSampleRow {
            pname: "p".into(),
            system: "x86_64-linux".into(),
            tenant: "t".into(),
            duration_secs: 100.0,
            peak_memory_bytes: peak,
            cpu_limit_cores: None,
            completed_at: now_epoch(),
            ..Default::default()
        };
        let mut rows: Vec<BuildSampleRow> = (0..4).map(|_| mem_row(10 * GI)).collect();
        rows.push(mem_row(2 * GI));
        let f = refit(
            &key(),
            &rows,
            None,
            &[],
            &super::super::hw::HwTable::default(),
            None,
        );
        let MemFit::Independent { p90 } = f.mem else {
            panic!("no c-axis rows ⇒ probe arm");
        };
        assert!(
            p90.0 >= 9 * (GI as u64),
            "mem p90 over the 5-sample consensus (~10 GiB), never the newest single sample (2 GiB): got {} GiB",
            p90.0 / (1 << 30)
        );
    }

    // r[verify sched.sla.disk-reaches-ephemeral-storage+2]
    /// **W11-BA (bug_070, red-first)** — *the disk quantile's evidence
    /// universe is every peaked sample regardless of axis mix.* One
    /// peaked c-axis row (5 GiB, a cache-warm rebuild) + nine peaked
    /// legacy rows (~200 GiB serial history): pre-fix the
    /// emptiness-gated fallback saw a NON-empty c-axis peak set and
    /// silently dropped every legacy peak — p90 of N+1 collapsed to
    /// p90 of 1 (the 5 GiB estimate shipped; the 1 GiB envelope floor
    /// passes it; recovery costs the reactive bump ladder). Post-fix
    /// the one chokepoint folds both populations always.
    #[test]
    fn w11_ba_disk_quantile_folds_every_peaked_sample_across_axis_mix() {
        const GI: i64 = 1 << 30;
        let mut rows: Vec<BuildSampleRow> = (0..9)
            .map(|i| disk_row(None, 100.0, Some((200 + i) * GI)))
            .collect();
        rows.push(disk_row(Some(4.0), 50.0, Some(5 * GI)));
        let f = refit(
            &key(),
            &rows,
            None,
            &[],
            &super::super::hw::HwTable::default(),
            None,
        );
        let got = f.disk_p90.expect("peaked samples must fit").0;
        assert!(
            got >= 190 * (GI as u64),
            "p90 over ALL 10 peaked samples (~200 GiB), never p90-of-1 (5 GiB): got {} GiB",
            got / (1 << 30)
        );
    }

    // r[verify sched.sla.disk-reaches-ephemeral-storage+2]
    /// **W12-AH (bug_040, red-first)** — *the probe_only disk aggregate
    /// is pinned at its OWN arm, by a STRUCTURAL witness of that arm.*
    /// The predecessor test ("probe_only_disk_p90_aggregates_all_
    /// observed_rows") carried one Some(4.0) c-axis row, so
    /// `fit_rows.is_empty()` was FALSE and the fixture took the
    /// FULL-FIT path: its asserted Probe came from the als-gate and
    /// its disk assertion was satisfied by the full-fit chokepoint —
    /// the line-176 `aggregate_disk_p90(rows, &[])` argument was
    /// pinned by NOTHING (a None/wrong-population rewrite passed the
    /// whole suite incl. the W11-BC read-count census), and the
    /// verify marker above attested coverage that did not exist —
    /// vacuous from birth.
    ///
    /// This fixture has ZERO c-axis rows (the arm's only entry
    /// condition) and asserts the arm structurally: probe_only's
    /// distinctive `n_eff_ring == 0.0 ∧ sum_w == 0.0` signature — the
    /// full-fit path computes a Kish n_eff over a non-empty ring and
    /// CANNOT mint it. Three peaked rows (the witnessed-population
    /// floor the disk fit consumes) + one peak-less newest row: the
    /// aggregate covers every observed peak, never the newest row's
    /// absent one. Red (strawman, transcript in the commit body): a
    /// None-rewrite of the probe_only disk argument fails HERE and
    /// nowhere else in the suite.
    #[test]
    fn w12_ah_probe_only_disk_aggregate_pinned_at_its_own_arm() {
        const GI: i64 = 1 << 30;
        let rows = vec![
            // Peaked legacy rows — no c-axis seat in this fixture's ring.
            disk_row(None, 100.0, Some(2 * GI)),
            disk_row(None, 101.0, Some(3 * GI)),
            disk_row(None, 102.0, Some(3 * GI)),
            // Newest row: still no seat, and no peak recorded.
            disk_row(None, 100.2, None),
        ];
        let f = refit(&key(), &rows, None, &[], &HwTable::default(), None);
        // STRUCTURAL arm witness: only probe_only mints this triple.
        assert!(
            matches!(f.fit, DurationFit::Probe) && f.n_eff_ring.0 == 0.0 && f.sum_w == 0.0,
            "the fixture must take the probe_only arm itself \
             (n_eff_ring={}, sum_w={}) — a sibling gate's Probe carries \
             a non-empty ring",
            f.n_eff_ring.0,
            f.sum_w
        );
        let p90 = f.disk_p90.expect(
            "probe_only consumes THE disk aggregate over every observed \
             row — a peak-less newest row erases nothing (strawman None \
             at the :176 argument fails exactly here)",
        );
        assert!(
            p90.0 >= 2 * GI as u64 && p90.0 <= 4 * GI as u64,
            "the witnessed fit over the observed peaks (2,3,3 GiB) — \
             p90 = 3 GiB, shrink-floored at newest×1.2 = 3.6 GiB \
             (live060-c); got {}",
            p90.0
        );
    }

    // r[verify sched.sla.disk-reaches-ephemeral-storage+2]
    /// Cold rows (zero peaks anywhere) stay None — no evidence is no
    /// evidence; the envelope's prior arm (the chart default) is the
    /// designed cold-start posture (kill-isolation for W7-H: the
    /// aggregate never invents data).
    #[test]
    fn no_disk_observations_stay_unfitted() {
        let rows = vec![
            disk_row(Some(4.0), 100.0, None),
            disk_row(None, 101.0, None),
        ];
        let f = refit(&key(), &rows, None, &[], &HwTable::default(), None);
        assert_eq!(f.disk_p90, None);
    }

    // r[verify sched.sla.disk-reaches-ephemeral-storage+2]
    /// **W12-LC (live060-c, red-first)** — *the warm disk fit is
    /// consumed only at a witnessed population and never shrinks below
    /// recent observed reality + headroom; population: the
    /// sparse-first-observations regime that activation (live060-a's
    /// prjquota provisioning) creates.* Pre-fix red (transcript in the
    /// commit body): `DiskFitEnvelope::derive` retired the prior the
    /// moment any warm p90 existed — the FIRST observation after
    /// provisioning, a single unrepresentative 2 GiB build, collapsed
    /// the 100 GiB prior to 2 GiB fleet-wide. Post-fix the prior
    /// stands until DISK_WITNESS_MIN_PEAKS observations exist, and the
    /// witnessed fit floors at newest×DISK_SHRINK_HEADROOM. The
    /// constants are R17-violable with the measured-at-first-samples
    /// rider (re-derived at the first soak; this test pins the
    /// boundary AT them).
    #[test]
    fn w12_lc_warm_fit_consumed_only_at_witnessed_population() {
        const GI: i64 = 1 << 30;
        let prior = 100 * GI as u64;
        let ceiling = 200 * GI as u64;
        // The premature-shrink attack: one small build post-provisioning.
        let one = vec![disk_row(None, 100.0, Some(2 * GI))];
        let f = refit(&key(), &one, None, &[], &HwTable::default(), None);
        assert_eq!(
            f.disk_p90, None,
            "below the witness gate the producer mints None (pre-fix: \
             Some(2 GiB) — the prior retired on ONE observation)"
        );
        let req = super::super::fit::DiskFitEnvelope::fit(f.disk_p90, prior, ceiling);
        assert_eq!(
            req.bytes(),
            prior,
            "the request holds the prior below the gate"
        );
        // Two observations: still below the gate.
        let two = vec![
            disk_row(None, 100.0, Some(2 * GI)),
            disk_row(None, 101.0, Some(2 * GI)),
        ];
        let f = refit(&key(), &two, None, &[], &HwTable::default(), None);
        assert_eq!(f.disk_p90, None, "n=2 < DISK_WITNESS_MIN_PEAKS");
        // At the gate: the fit consumes, and the shrink FLOORS at the
        // newest peak × headroom (8 GiB build newest ⇒ floor 9.6 GiB
        // even though p90 of [2,2,8] lands at 8).
        let three = vec![
            disk_row(None, 100.0, Some(2 * GI)),
            disk_row(None, 101.0, Some(2 * GI)),
            disk_row(None, 102.0, Some(8 * GI)),
        ];
        let f = refit(&key(), &three, None, &[], &HwTable::default(), None);
        let fit = f.disk_p90.expect("witnessed at n=3");
        let want = (8.0 * GI as f64 * 1.2) as u64;
        assert_eq!(
            fit.0, want,
            "the shrink floors at newest×1.2 — no shrink below recent \
             observed reality + headroom"
        );
        let req = super::super::fit::DiskFitEnvelope::fit(f.disk_p90, prior, ceiling);
        assert_eq!(req.bytes(), want, "the envelope consumes the witnessed fit");
    }

    // r[verify sched.sla.disk-reaches-ephemeral-storage+2]
    /// **W12-LC2 (live060-c; the steady-state direction preserved)** —
    /// at a witnessed population of representative peaks the warm fit
    /// governs exactly as the verified downward path intends: the
    /// request lands in the observed band (p90 floored at newest×1.2),
    /// FAR below the cold-start prior — the sizer learns down; the
    /// gate does not re-pin the prior.
    #[test]
    fn w12_lc2_witnessed_population_governs_the_downward_path() {
        const GI: i64 = 1 << 30;
        let rows: Vec<BuildSampleRow> = (0..6)
            .map(|i| disk_row(None, 100.0 + i as f64, Some(10 * GI)))
            .collect();
        let f = refit(&key(), &rows, None, &[], &HwTable::default(), None);
        let fit = f.disk_p90.expect("witnessed");
        assert_eq!(
            fit.0,
            (10.0 * GI as f64 * 1.2) as u64,
            "representative peaks: p90 = newest ⇒ the floor IS the fit \
             (observed reality + 20% headroom)"
        );
        let req =
            super::super::fit::DiskFitEnvelope::fit(f.disk_p90, 100 * GI as u64, 200 * GI as u64);
        assert!(
            req.bytes() <= 12 * GI as u64 && req.bytes() >= 10 * GI as u64,
            "the request lands in the observed band, not at the prior: {}",
            req.bytes()
        );
    }

    // r[verify sched.sla.disk-polarity-fork]
    /// **W13-R (merged_bug_002, red-first)** — *no all-fitting
    /// population is ceiling-rejected; population: the
    /// (ceiling/1.2, ceiling] band, the live-measured shape.*
    /// Six observed peaks 185..=190 GiB under the shipped 200 GiB
    /// ceiling — every observation fits, the newest (190 GiB) sits in
    /// the band where newest x 1.2 = 228 GiB crosses the ceiling.
    /// Pre-fix red (transcript in the commit body): the single
    /// floored field fed the reject gates, so the band population was
    /// falsely rejected as cannot-fit, self-renewing on each refit.
    #[test]
    fn w13_r_all_fitting_band_population_is_never_ceiling_rejected() {
        const GI: i64 = 1 << 30;
        let ceiling = 200 * GI as u64;
        let rows: Vec<BuildSampleRow> = (0..6)
            .map(|i| disk_row(None, 100.0 + i as f64, Some((185 + i) * GI)))
            .collect();
        let f = refit(&key(), &rows, None, &[], &HwTable::default(), None);
        assert!(
            !super::super::fit::DiskFitEnvelope::exceeds_ceiling(f.disk_p90_raw, ceiling),
            "every observed peak fits under the ceiling — the reject \
             face must consume the RAW witnessed p90, never the \
             sizing-face shrink floor (conservative-for-sizing is \
             anti-conservative-for-reject)"
        );
        let raw = f.disk_p90_raw.expect("witnessed").bytes();
        assert!(
            raw <= 190 * GI as u64,
            "the raw face is the weighted p90, bounded by the max \
             observation: {raw}"
        );
    }

    // r[verify sched.sla.disk-polarity-fork]
    /// **W13-R2 (merged_bug_002)** — *the sizing face still floors on
    /// the same band population: live060-c's shrink hysteresis is
    /// byte-stable across the fork.* The floored face carries
    /// `max(p90, newest x 1.2)` exactly as the pre-fork single field
    /// did, and the envelope request consumes it (clamped at the
    /// ceiling) — the fork moves the REJECT readers off the floor, it
    /// never weakens the sizing protection.
    #[test]
    fn w13_r2_sizing_face_keeps_the_shrink_floor_on_the_band() {
        const GI: i64 = 1 << 30;
        let ceiling = 200 * GI as u64;
        let rows: Vec<BuildSampleRow> = (0..6)
            .map(|i| disk_row(None, 100.0 + i as f64, Some((185 + i) * GI)))
            .collect();
        let f = refit(&key(), &rows, None, &[], &HwTable::default(), None);
        let floored = f.disk_p90.expect("witnessed").0;
        assert_eq!(
            floored,
            (190.0 * GI as f64 * 1.2) as u64,
            "the sizing face floors at newest x DISK_SHRINK_HEADROOM \
             — byte-identical to the pre-fork mint"
        );
        let req = super::super::fit::DiskFitEnvelope::fit(f.disk_p90, 100 * GI as u64, ceiling);
        assert_eq!(
            req.bytes(),
            ceiling,
            "the envelope clamps the floored face at the operator \
             ceiling — the request law is unchanged"
        );
    }

    // r[verify sched.sla.one-weight-law]
    /// **W12-AD (merged_bug_022, red-first)** — *the aggregator's weight
    /// law is total over sub-populations: BOTH directions of the mixed
    /// ring.* The INVERSE-population fixture the wave-11 seal never
    /// drove: 3 STALE legacy peaks (200 GiB, oldest, unseated) ahead of
    /// 29 fresh seated rows (5 GiB peaks). Pre-fix red: unseated rows
    /// took flat weight 1.0 — the newest seat row's weight — so the
    /// stale trio held ~14% of the fold mass, `disk_p90` stayed pinned
    /// at the 200 GiB legacy peak and `exceeds_ceiling` falsely
    /// rejected at a 50 GiB ceiling. Post-fix the trio decays under the
    /// one law (full-slice ordinal ages 29..31 ⇒ ~5% of the mass):
    /// fresh evidence wins the quantile and the tier rejection clears.
    /// The legacy-is-truth direction is preserved by W11-BA/BB above —
    /// both directions of the aggregator are now sealed.
    #[test]
    fn w12_ad_inverse_population_fresh_evidence_wins() {
        const GI: i64 = 1 << 30;
        let mut rows: Vec<BuildSampleRow> = (0..3)
            .map(|_| disk_row(None, 100.0, Some(200 * GI)))
            .collect();
        rows.extend((0..29).map(|_| disk_row(Some(4.0), 50.0, Some(5 * GI))));
        let f = refit(&key(), &rows, None, &[], &HwTable::default(), None);
        let got = f.disk_p90.expect("peaked samples fit");
        assert!(
            got.0 <= 10 * GI as u64,
            "disk_p90 follows the fresh population (~5 GiB) once the \
             stale trio decays under the one weight law; got {} GiB \
             (pre-fix: pinned at the 200 GiB legacy peak)",
            got.0 / (1 << 30)
        );
        assert!(
            !super::super::fit::DiskFitEnvelope::exceeds_ceiling(f.disk_p90_raw, 50 * GI as u64),
            "the false tier rejection clears once fresh evidence wins"
        );
    }

    // ── The sub-population WEIGHT CENSUS (merged_bug_022; [GEN-SET];
    //    the §2 census riders (a)+(b)) ──────────────────────────────
    //
    // Universe DERIVED from `mod.rs` declarations (jurisdiction face);
    // members = production `weighted_quantile(` fold sites per module.
    // Committed corpus map below; the completeness assert keeps it
    // honest against the declared module tree.

    /// The committed corpus: every `pub mod` of `sla/` embedded. The
    /// jurisdiction face (`weight_census_universe_covers_module_tree`)
    /// REDs if a new sla module is declared without joining this map.
    fn weight_census_corpus() -> Vec<(&'static str, &'static str)> {
        vec![
            ("alpha", include_str!("alpha.rs")),
            ("bootstrap", include_str!("bootstrap.rs")),
            ("catalog", include_str!("catalog.rs")),
            ("config", include_str!("config.rs")),
            ("cost", include_str!("cost.rs")),
            ("dip", include_str!("dip.rs")),
            ("explain", include_str!("explain.rs")),
            ("explore", include_str!("explore.rs")),
            ("fit", include_str!("fit.rs")),
            ("hw", include_str!("hw.rs")),
            ("ingest", include_str!("ingest.rs")),
            ("metrics", include_str!("metrics.rs")),
            ("override", include_str!("override.rs")),
            ("prior", include_str!("prior.rs")),
            ("quantile", include_str!("quantile.rs")),
            ("solve", include_str!("solve.rs")),
            ("types", include_str!("types.rs")),
        ]
    }

    /// `pub mod X;` declarations parsed from a mod.rs source (the
    /// jurisdiction derivation — never a hand crate-list).
    fn parse_mod_decls(mod_src: &str) -> Vec<String> {
        mod_src
            .lines()
            .filter_map(|l| {
                l.trim()
                    .strip_prefix("pub mod ")
                    .and_then(|r| r.strip_suffix(';'))
                    .map(|m| m.trim_start_matches("r#").to_owned())
            })
            .collect()
    }

    fn strip_test_mod(src: &str) -> &str {
        src.split_once("#[cfg(test)]\nmod").map_or(src, |(p, _)| p)
    }

    /// Count production `weighted_quantile(` sites per module;
    /// grammar-refusal: an alias/rename of the fold fn ERRORS (a call
    /// through `wq(` would evade the count — refuse, never green).
    fn scan_weight_folds(corpus: &[(&str, &str)]) -> Result<Vec<(String, usize)>, String> {
        let mut out = Vec::new();
        for (name, src) in corpus {
            let prod = strip_test_mod(src);
            if prod.contains("weighted_quantile as") {
                return Err(format!(
                    "{name}: aliased weighted_quantile import — the census \
                     cannot classify calls through a rename; refused"
                ));
            }
            out.push((
                (*name).to_owned(),
                prod.matches("weighted_quantile(").count(),
            ));
        }
        Ok(out)
    }

    /// Extract the production `axis_samples` body (the one weight-law
    /// producer seam) for the law-consult pin.
    fn axis_samples_body(ingest_src: &str) -> &str {
        let prod = strip_test_mod(ingest_src);
        let start = prod.find("fn axis_samples").expect("producer present");
        let rest = &prod[start..];
        let end = rest[3..].find("\nfn ").map_or(rest.len(), |i| i + 3);
        &rest[..end]
    }

    // r[verify sched.sla.one-weight-law]
    /// **The weight census, jurisdiction + population faces (CE-1).**
    /// The scan universe derives from `mod.rs` (a hand module-list
    /// drifts RED here); population floor: the walk derived ≥1 fold
    /// site; the WO-named EXPECTED members verified (fit.rs: the
    /// definition and the IRLS pinball baseline; ingest.rs:
    /// observed_p_bar and the mem and disk chokepoints; everything else
    /// zero). A new fold site in the derived module universe drifts a count
    /// — re-derive the expectation consciously, never widen silently.
    #[test]
    fn w12_ad_weight_census() {
        let corpus = weight_census_corpus();
        // Jurisdiction: every declared module is in the corpus.
        let declared = parse_mod_decls(include_str!("mod.rs"));
        assert!(!declared.is_empty(), "population floor: mod.rs parses");
        for m in &declared {
            assert!(
                corpus.iter().any(|(n, _)| n == m),
                "sla module `{m}` declared in mod.rs but missing from the \
                 weight-census corpus — jurisdiction gap (add the embed)"
            );
        }
        // Membership: committed per-module fold-site expectation.
        let got = scan_weight_folds(&corpus).expect("no alias evasion in tree");
        let total: usize = got.iter().map(|(_, c)| c).sum();
        assert!(total >= 1, "population floor: ≥1 derived fold site");
        for (name, count) in &got {
            let want = match name.as_str() {
                "fit" => 2,    // the definition + irls_quantile's v_null
                "ingest" => 3, // observed_p_bar + mem + disk chokepoints
                _ => 0,
            };
            assert_eq!(
                *count, want,
                "weighted_quantile( sites in {name}.rs drifted from the \
                 committed census — a new fold must consume the one \
                 weight law (re-derive, never hand-wave)"
            );
        }
        // Law-consult pin: the producer body consults the law and carries
        // NO exempt flat-weight default.
        let body = axis_samples_body(include_str!("ingest.rs"));
        assert!(
            body.contains("sample_weight("),
            "axis_samples consults the one decay law"
        );
        assert!(
            !body.contains("1.0"),
            "axis_samples carries an exempt flat weight — the \
             merged_bug_022 hole reopened"
        );
    }

    /// **Weight-census planted reds (riders (b)): each face's oracle
    /// driven through the same walk path as production.**
    #[test]
    fn w12_ad_weight_census_planted_reds() {
        // (1) ENROLLMENT plant: an in-grammar uncensused fold member —
        // the count comparison is the oracle (a strawman module with a
        // private fold drifts its count from the committed 0).
        let strawman =
            "fn rogue(p: &[f64], w: &[f64]) -> f64 {\n    weighted_quantile(p, w, 0.9)\n}\n";
        let got = scan_weight_folds(&[("strawman", strawman)]).unwrap();
        assert_eq!(got[0].1, 1, "the walk FINDS the planted fold");
        assert_ne!(got[0].1, 0, "…and it drifts from the committed 0 — RED");
        // (2) JURISDICTION plant: a module declared outside the
        // previously-scanned population auto-joins the derived universe;
        // the corpus-completeness assert is the oracle.
        let decls = parse_mod_decls("pub mod fit;\npub mod phantom;\n");
        assert!(decls.contains(&"phantom".to_owned()));
        let corpus = weight_census_corpus();
        assert!(
            !corpus.iter().any(|(n, _)| *n == "phantom"),
            "the planted module is NOT in the corpus — the live census's \
             completeness loop REDs on exactly this state"
        );
        // (3) GRAMMAR-REFUSAL plant: an aliased import must ERROR, never
        // silently green.
        let evader = "use super::fit::weighted_quantile as wq;\nfn f(v: &[f64], w: &[f64]) -> f64 { wq(v, w, 0.9) }\n";
        assert!(
            scan_weight_folds(&[("evader", evader)]).is_err(),
            "alias evasion refused"
        );
        // (4) EXEMPT-DEFAULT plant (the WO's strawman arm): a producer
        // body re-growing a flat weight REDs the law-consult pin.
        let exempt = "fn axis_samples(rows: &[Row]) -> Vec<f64> {\n    let w = if seated { ring } else { 1.0 };\n}\nfn next() {}\n";
        let body = axis_samples_body(exempt);
        assert!(
            body.contains("1.0"),
            "the totality pin REDs on the planted exempt arm"
        );
    }
}
