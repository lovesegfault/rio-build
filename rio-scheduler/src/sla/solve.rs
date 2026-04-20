use std::collections::BTreeMap;

use rand::{Rng, RngExt};
use serde::{Deserialize, Serialize};

use super::config::SlaConfig;
use super::cost::{Band, Cap, CostTable, IceBackoff};
use super::explore;
use super::fit::headroom;
use super::hw::HwTable;
use super::r#override::ResolvedTarget;
use super::quantile;
use super::types::{DiskBytes, FittedParams, MemBytes, RawCores};

/// `prefer_local_build = true` → trivially short. Smallest pod the
/// controller will spawn.
const LOCAL_CORES: u32 = 1;
const LOCAL_MEM_BYTES: u64 = 2 << 30;
const LOCAL_DISK_BYTES: u64 = 1 << 30;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tier {
    pub name: String,
    #[serde(default)]
    pub p50: Option<f64>,
    #[serde(default)]
    pub p90: Option<f64>,
    #[serde(default)]
    pub p99: Option<f64>,
}

impl Tier {
    /// The single percentile this tier is bounded by, in canonical
    /// precedence p90→p50→p99 (`r[sched.sla.reassign-schmitt]`). All
    /// consumers that need "the tier's bound" — `solve_tiers()` sort
    /// order, `reassign_tier()` Schmitt walk — go through this so they
    /// agree on which percentile is binding. Returns wall-seconds (tier
    /// targets are operator-facing, not reference-seconds).
    pub fn binding_bound(&self) -> Option<f64> {
        self.p90.or(self.p50).or(self.p99)
    }
}

#[derive(Debug, Clone)]
pub struct Ceilings {
    pub max_cores: f64,
    pub max_mem: u64,
    pub max_disk: u64,
    pub default_disk: u64,
}

#[derive(Debug, Clone)]
pub enum SolveResult {
    Feasible {
        tier: String,
        c_star: RawCores,
        mem: MemBytes,
        disk: DiskBytes,
        /// `rio.build/hw-band` + `karpenter.sh/capacity-type` (+
        /// optionally `rio.build/storage`). `None` under
        /// `hw_cost_source = None` (band-agnostic [`solve_mvp`]).
        node_selector: Option<BTreeMap<String, String>>,
    },
    BestEffort {
        c: RawCores,
        mem: MemBytes,
        disk: DiskBytes,
        node_selector: Option<BTreeMap<String, String>>,
    },
}

impl SolveResult {
    pub fn cores(&self) -> RawCores {
        match self {
            Self::Feasible { c_star, .. } => *c_star,
            Self::BestEffort { c, .. } => *c,
        }
    }
    pub fn mem(&self) -> MemBytes {
        match self {
            Self::Feasible { mem, .. } | Self::BestEffort { mem, .. } => *mem,
        }
    }
    pub fn disk(&self) -> DiskBytes {
        match self {
            Self::Feasible { disk, .. } | Self::BestEffort { disk, .. } => *disk,
        }
    }
    pub fn node_selector(&self) -> Option<&BTreeMap<String, String>> {
        match self {
            Self::Feasible { node_selector, .. } | Self::BestEffort { node_selector, .. } => {
                node_selector.as_ref()
            }
        }
    }
}

/// One feasible `(band, cap)` cell from [`solve_full`]'s inner loop.
/// `e_cost` is the comparable scalar for [`softmax_pick`].
#[derive(Debug, Clone)]
pub struct Candidate {
    pub band: Band,
    pub cap: Cap,
    pub c_star: RawCores,
    pub mem: MemBytes,
    pub e_cost: f64,
}

impl Candidate {
    /// Pod nodeSelector for this `(band, cap)`. Storage is left to the
    /// controller's per-pool `rio.build/storage` selector (the solve
    /// doesn't yet bias storage; phase-14 wires `disk_p90` → nvme).
    pub fn selector(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("rio.build/hw-band".into(), self.band.label().into()),
            ("karpenter.sh/capacity-type".into(), self.cap.label().into()),
        ])
    }
}

/// Dispatch-time prediction snapshot. Stored on
/// [`SchedHint::last_intent`] when the derivation is dispatched so
/// completion can score actual-vs-predicted without re-reading the
/// estimator (which may have refit on a different curve by then).
///
/// [`SchedHint::last_intent`]: crate::state::SchedHint::last_intent
#[derive(Debug, Clone, Default)]
pub struct SlaPrediction {
    /// `T(c)` at the dispatched core count, in **reference-seconds**
    /// (the fit is built from hw-normalized samples; `t_at()` returns
    /// `RefSeconds`). [`super::metrics::score_completion`] normalizes
    /// the actual wall-clock by the completion's hw_class factor before
    /// dividing. `None` for cold-start / override / drv-hint paths
    /// where there is no fitted curve.
    pub wall_secs: Option<f64>,
    /// `M(c)` (post-headroom) the controller was asked to reserve.
    pub mem_bytes: u64,
    /// Tier the solve picked (`SolveResult::Feasible.tier`). `None` for
    /// `BestEffort` / probe / override.
    pub tier: Option<String>,
    /// p90 target of `tier` at dispatch time. Captured here so
    /// completion's hit/miss check is robust to the tier ladder being
    /// reconfigured mid-build.
    pub tier_p90: Option<f64>,
}

/// Derivation-declared sizing hints (from drv.env / drv.system-features).
#[derive(Debug, Default, Clone)]
pub struct DrvHints {
    pub enable_parallel_building: Option<bool>,
    pub prefer_local_build: Option<bool>,
    /// `requiredSystemFeatures` — drives [`super::explore::next`]'s
    /// per-feature probe-shape override (e.g. `kvm` builds want a high
    /// mem floor regardless of core count).
    pub required_features: Vec<String>,
}

/// Per-derivation `(cores, mem, disk)` for a [`SpawnIntent`]. Wraps
/// [`solve_mvp`] with the cold-start / drv-hint branching that the
/// snapshot path and dispatch's resource-fit filter both need, so the
/// two cannot diverge.
///
/// - `prefer_local_build = Some(true)` → minimal (1 core), no learning.
/// - `enable_parallel_building = Some(false)` → 1 core, fitted mem/disk
///   (the build is serial by declaration; exploring c is wasted, but
///   mem/disk still come from the per-key fit when available).
/// - No fit / `n_eff < 3` / `span < 4 ∧ !frozen` → [`explore::next`]
///   drives the saturation-gated ×4/÷2 ladder from `cfg.probe`.
/// - Otherwise → `solve_mvp` against `tiers` / `ceil`.
///
/// Cores are `ceil`-ed to a whole-core `u32` (k8s `resources.requests`
/// granularity); mem/disk are byte-exact (controller rounds at the pod
/// template).
///
/// `override_` (from [`SlaEstimator::resolved_override`]) is consulted
/// FIRST: `forced_cores` short-circuits the model entirely; `forced_mem`
/// applies in any branch (with or without `forced_cores`). A `tier`
/// override restricts the solve ladder to that tier.
///
/// [`SpawnIntent`]: rio_proto::types::SpawnIntent
/// [`SlaEstimator::resolved_override`]: super::SlaEstimator::resolved_override
// r[impl sched.sla.intent-from-solve]
pub fn intent_for(
    fit: Option<&FittedParams>,
    hints: &DrvHints,
    override_: Option<&ResolvedTarget>,
    cfg: &SlaConfig,
    tiers: &[Tier],
    ceil: &Ceilings,
) -> (u32, u64, u64) {
    // Feature-aware probe + headroom-scaled mem hoisted ONCE so every
    // early-return branch reads the same definitions as `solve_mvp` /
    // `solve_full` / `explore::next`. Before, the forced_cores /
    // serial branches read `f.mem.at(c)` raw (no `× headroom(n_eff)` —
    // breaking r[sched.sla.headroom-confidence-scaled]) and `probe_mem`
    // ignored `feature_probes` (the exact case `kvm` overrides exist
    // for: a 1-core kvm build needs the high mem floor).
    let probe = hints
        .required_features
        .iter()
        .find_map(|f| cfg.feature_probes.get(f))
        .unwrap_or(&cfg.probe);
    let probe_mem = (probe.cpu * probe.mem_per_core as f64 + probe.mem_base as f64) as u64;
    let fit_mem_at =
        |c: f64| fit.map(|f| (f.mem.at(RawCores(c)).0 as f64 * headroom(f.n_eff)) as u64);
    // disk_p90 is core-independent (r[sched.sla.disk-scalar]); resolve
    // and clamp once so every early-return branch is self-consistent
    // for `explain.rs`. The `solve_intent_for` chokepoint applies the
    // same ceiling (alongside mem and the reactive floor), so this is
    // belt-and-braces.
    let fit_disk = fit
        .and_then(|f| f.disk_p90)
        .map_or(ceil.default_disk, |d| d.0)
        .min(ceil.max_disk);
    // Operator override beats everything — including drv-declared hints.
    // If the operator forced 12 cores on a `prefer_local_build` drv,
    // they had a reason. Mem unset → fall back to the fit's M(c) at the
    // forced core count (or probe mem if no fit).
    if let Some(o) = override_
        && let Some(c) = o.forced_cores
    {
        let cores = (c.ceil() as u32).max(1);
        let mem = o
            .forced_mem
            .unwrap_or_else(|| fit_mem_at(c).unwrap_or(probe_mem));
        return (cores, mem, fit_disk);
    }
    // Drv hints pin ONLY cores; mem/disk fall back to the fit when
    // available. `resource_floor` is per-drv_hash, so a new version of
    // a known disk-heavy serial pname must start at the observed p90,
    // not re-climb the OOM/DiskPressure ladder from probe defaults.
    // `forced_mem` is hoisted so it overrides mem in EVERY branch below
    // (the doc-promise above) — `rio sla override --mem` on a serial
    // drv was silently dropped before.
    let forced_mem = override_.and_then(|o| o.forced_mem);
    if hints.prefer_local_build == Some(true) {
        // Trivial builders (writeText, runCommand, symlinkJoin) have no
        // pname → never fitted → must NOT fall back to ceil.default_disk
        // (100 GiB chart default, sized for cold-probe of an unknown
        // package). Mirror LOCAL_MEM_BYTES with a minimal disk const.
        let disk = fit
            .and_then(|f| f.disk_p90)
            .map_or(LOCAL_DISK_BYTES, |d| d.0)
            .min(ceil.max_disk);
        return (LOCAL_CORES, forced_mem.unwrap_or(LOCAL_MEM_BYTES), disk);
    }
    if hints.enable_parallel_building == Some(false) {
        let mem = forced_mem.or_else(|| fit_mem_at(1.0)).unwrap_or(probe_mem);
        return (1, mem, fit_disk);
    }
    let fit = match fit {
        Some(f)
            if f.n_eff >= 3.0 && (f.span >= 4.0 || explore::frozen(&f.explore, ceil.max_cores)) =>
        {
            f
        }
        _ => {
            // Explore ladder: saturation-gated ×4/÷2 from cfg.probe.
            let d = explore::next(fit, cfg, hints);
            let mem = forced_mem.unwrap_or(d.mem.0);
            return (
                (d.c.0.ceil() as u32).max(1),
                mem,
                d.disk.0.min(ceil.max_disk),
            );
        }
    };
    // tier override: solve against ONLY that tier's bounds. An unknown
    // name yields an empty ladder → BestEffort (operator error surfaces
    // via `sla explain`, not a silent fallback to the default ladder).
    let pinned: Vec<Tier>;
    let tiers = match override_.and_then(|o| o.tier.as_deref()) {
        Some(name) => {
            pinned = tiers.iter().filter(|t| t.name == name).cloned().collect();
            &pinned
        }
        None => tiers,
    };
    let r = solve_mvp(fit, tiers, ceil);
    // ceil + clamp ≥1: solve_mvp's c_star is already ≥1.0 but
    // BestEffort.c can be fractional below 1 for degenerate fits.
    let cores = (r.cores().0.ceil() as u32).max(1);
    let mem = forced_mem.unwrap_or(r.mem().0);
    (cores, mem, r.disk().0)
}

// r[impl sched.sla.tier-envelope]
/// Min `c ∈ [c_lo, cap_c]` satisfying `∀(q,bound)∈tier:
/// quantile(q; T(c)/hw_factor, σ, p(c)) ≤ bound`, where
/// `p(c) = 1-exp(-λ·T(c))` is the per-attempt preemption probability
/// under Poisson rate `λ`. Returns `None` if infeasible at `cap_c`.
///
/// `feasible(c)` is monotone on `[c_lo, cap_c]` because callers pass
/// `cap_c ≤ min(p̄, c_opt)` where `T(c)` is decreasing — so plain
/// bisection finds the boundary.
pub fn solve_envelope(
    fit: &FittedParams,
    tier: &Tier,
    c_lo: f64,
    cap_c: f64,
    hw_factor: f64,
    lambda: f64,
) -> Option<RawCores> {
    let bounds: Vec<(f64, f64)> = [(0.5, tier.p50), (0.9, tier.p90), (0.99, tier.p99)]
        .into_iter()
        .filter_map(|(q, b)| b.map(|b| (q, b)))
        .collect();
    if bounds.is_empty() {
        // No-bounds tier: max useful cores. Returning `c_lo` (=1 in
        // `solve_mvp`) made the post-loop `BestEffort` arm unreachable
        // under helm-default `tiers=[..., {best-effort}]` and dispatched
        // chromium-class builds at 1 core → 24h-deadline-loop.
        return Some(RawCores(cap_c));
    }
    let feasible = |c: f64| -> bool {
        let t = fit.fit.t_at(RawCores(c)).0 / hw_factor;
        let p = if lambda > 0.0 {
            1.0 - (-lambda * t).exp()
        } else {
            0.0
        };
        if p > 0.5 {
            return false;
        }
        bounds
            .iter()
            .all(|&(q, bound)| quantile::quantile(q, t, fit.sigma_resid, p) <= bound)
    };
    if !feasible(cap_c) {
        return None;
    }
    if feasible(c_lo) {
        return Some(RawCores(c_lo.ceil()));
    }
    // Invariant: lo infeasible, hi feasible.
    let (mut lo, mut hi) = (c_lo, cap_c);
    while hi - lo > 0.5 {
        let mid = (lo + hi) / 2.0;
        if feasible(mid) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    // Bisection halts at `hi - lo < 0.5` so `hi` may sit anywhere in
    // `(c*, c* + 0.5)`; `ceil(hi)` then over-allocates by 1 core when
    // `hi` straddles an integer (e.g. c*=8.9 → hi=9.3 → ceil=10 but 9
    // is feasible). Probe `n-1` once — `feasible` is cheap.
    let n = hi.ceil();
    Some(RawCores(if n > c_lo + 1.0 && feasible(n - 1.0) {
        n - 1.0
    } else {
        n
    }))
}

// r[impl sched.sla.solve-citardauq]
// r[impl sched.sla.solve-reject-not-clamp]
pub fn solve_mvp(fit: &FittedParams, tiers: &[Tier], ceil: &Ceilings) -> SolveResult {
    let cap_c = fit.fit.p_bar().0.min(fit.fit.c_opt().0).min(ceil.max_cores);
    let h = headroom(fit.n_eff);
    let disk = fit.disk_p90.map(|d| d.0).unwrap_or(ceil.default_disk);

    for tier in tiers {
        // hw_factor=1.0 / lambda=0.0: phase-13 wires the per-hw_class
        // factor and per-capacity-type preemption rate; until then the
        // envelope solve degenerates to the scalar-p90 closed form ±1
        // core when only `p90` is set.
        let Some(c_star) = solve_envelope(fit, tier, 1.0, cap_c, 1.0, 0.0) else {
            continue; // REJECT (not clamp)
        };
        let mem = (fit.mem.at(c_star).0 as f64 * h) as u64;
        if mem > ceil.max_mem {
            continue;
        }
        if disk > ceil.max_disk {
            continue;
        }
        return SolveResult::Feasible {
            tier: tier.name.clone(),
            c_star,
            mem: MemBytes(mem),
            disk: DiskBytes(disk),
            node_selector: None,
        };
    }
    // Infeasible-at-any-tier — the operator-facing alerting hook
    // (observability.md). Gated on "≥1 tier had bounds": a pure
    // best-effort ladder (`tiers=[{p*:None}]`) or an empty ladder
    // (unknown pinned-tier override) is not an exhaust event.
    if tiers
        .iter()
        .any(|t| t.p50.is_some() || t.p90.is_some() || t.p99.is_some())
    {
        super::cost::ceiling_exhausted();
    }
    // D4: clamp at `ceil.max_*` — the Feasible arm above gates on
    // `mem/disk > ceil` (reject-not-clamp); BestEffort must clamp so
    // `solve_intent_for`'s floor.max() composes (a `disk_p90 > max_disk`
    // here used to spawn a permanently-Pending pod). `cap_c` already
    // includes `c_opt` so a USL fit isn't pushed into its slowdown region.
    let mem = ((fit.mem.at(RawCores(cap_c)).0 as f64 * h) as u64).min(ceil.max_mem);
    SolveResult::BestEffort {
        c: RawCores(cap_c),
        mem: MemBytes(mem),
        disk: DiskBytes(disk.min(ceil.max_disk)),
        node_selector: None,
    }
}

/// `c` such that `λ · T(c)/factor = ln 2` — the core count where the
/// per-attempt preemption probability hits 50%. Used as the floor of
/// the spot-envelope search: below `c_λ`, spot is rejected outright
/// (`p > 0.5` gate), so there's no point bisecting there.
fn c_lambda(fit: &FittedParams, lambda: f64, factor: f64) -> f64 {
    // T(c) ≤ ln2·factor/λ. With T(c)=S+P/c (Amdahl on [1, p̄]):
    // c ≥ P / (ln2·factor/λ - S). Non-positive denom ⇒ infeasible at
    // any c (S alone breaches) — return ∞ so the cap_c gate rejects.
    let (s, p, _) = fit.fit.spq();
    let target = std::f64::consts::LN_2 * factor / lambda;
    if target <= s {
        return f64::INFINITY;
    }
    (p / (target - s)).max(1.0)
}

// r[impl sched.sla.solve-per-band-cap]
/// Algorithm 4: per-`(band, cap)` envelope solve + cost softmax.
///
/// For each tier (tightest-first), enumerate all 6 `(band, cap)`
/// cells, skip ICE-backed-off ones, run [`solve_envelope`] with that
/// cell's `(h†_factor, λ)`, gate on `p(C) ≤ 0.5` for spot, gate on
/// mem/disk ceilings, compute `E[cost]`. If ≥1 candidate survives,
/// [`softmax_pick`] one and return `Feasible` with its nodeSelector.
/// If no tier yields a candidate, fall through to band-agnostic
/// `BestEffort`.
///
/// `temp` is the softmax temperature — `0.0` = greedy
/// argmin, higher = more spread.
#[allow(clippy::too_many_arguments)]
pub fn solve_full(
    fit: &FittedParams,
    tiers: &[Tier],
    hw: &HwTable,
    cost: &CostTable,
    ceil: &Ceilings,
    ice: &IceBackoff,
    temp: f64,
    rng: &mut impl Rng,
) -> SolveResult {
    let cap_c = fit.fit.p_bar().0.min(fit.fit.c_opt().0).min(ceil.max_cores);
    let h = headroom(fit.n_eff);
    let disk = fit.disk_p90.map(|d| d.0).unwrap_or(ceil.default_disk);

    for tier in tiers {
        let mut candidates = Vec::with_capacity(6);
        for band in Band::ALL {
            for cap in Cap::ALL {
                if ice.is_infeasible(band, cap) {
                    continue;
                }
                let (_, factor) = hw.h_dagger(&fit.key.pname, band, &fit.hw_bias);
                let lambda = match cap {
                    Cap::Spot => cost.lambda_band(band),
                    Cap::OnDemand => 0.0,
                };
                // p(cap_c) > 0.5 ⇒ spot infeasible regardless of c
                // (the geometric retry tail blows every quantile).
                if cap == Cap::Spot {
                    let t_cap = fit.fit.t_at(RawCores(cap_c)).0 / factor;
                    let p_at_cap = 1.0 - (-lambda * t_cap).exp();
                    if p_at_cap > 0.5 {
                        continue;
                    }
                }
                let c_lo = if cap == Cap::Spot {
                    c_lambda(fit, lambda, factor).ceil()
                } else {
                    1.0
                };
                if c_lo > cap_c {
                    continue;
                }
                let Some(c_star) = solve_envelope(fit, tier, c_lo, cap_c, factor, lambda) else {
                    continue;
                };
                let mem = (fit.mem.at(c_star).0 as f64 * h) as u64;
                if mem > ceil.max_mem || disk > ceil.max_disk {
                    continue;
                }
                let e_cost = cost.expected_cost(band, cap, c_star, mem, fit, factor, lambda);
                candidates.push(Candidate {
                    band,
                    cap,
                    c_star,
                    mem: MemBytes(mem),
                    e_cost,
                });
            }
        }
        if let Some(chosen) = softmax_pick(&candidates, temp, rng) {
            return SolveResult::Feasible {
                tier: tier.name.clone(),
                c_star: chosen.c_star,
                mem: chosen.mem,
                disk: DiskBytes(disk),
                node_selector: Some(chosen.selector()),
            };
        }
    }
    if ice.exhausted() {
        super::cost::capacity_exhausted();
    } else if tiers
        .iter()
        .any(|t| t.p50.is_some() || t.p90.is_some() || t.p99.is_some())
    {
        super::cost::ceiling_exhausted();
    }
    // D4: clamp at `ceil.max_*` — the Feasible arm above gates on
    // `mem/disk > ceil` (reject-not-clamp); BestEffort must clamp so
    // `solve_intent_for`'s floor.max() composes. `cap_c` already
    // includes `c_opt` so a USL fit isn't pushed into its slowdown
    // region (e.g. p̄=∞ with c_opt=20 must NOT return 64).
    let mem = ((fit.mem.at(RawCores(cap_c)).0 as f64 * h) as u64).min(ceil.max_mem);
    SolveResult::BestEffort {
        c: RawCores(cap_c),
        mem: MemBytes(mem),
        disk: DiskBytes(disk.min(ceil.max_disk)),
        node_selector: None,
    }
}

/// Weighted-random pick over `candidates` by `exp(-e_cost/min/temp)`.
/// `temp ≤ 0` or `len()==1` → argmin. `None` for empty input.
///
/// Normalizing by `min(e_cost)` makes the softmax scale-free: a 2×
/// price ratio gets the same weight split whether the absolute costs
/// are $0.01 or $10. The exponent is clamped to avoid `inf` on
/// degenerate inputs.
pub fn softmax_pick<'a>(
    candidates: &'a [Candidate],
    temp: f64,
    rng: &mut impl Rng,
) -> Option<&'a Candidate> {
    match candidates {
        [] => return None,
        [only] => return Some(only),
        _ => {}
    }
    let min = candidates
        .iter()
        .map(|c| c.e_cost)
        .fold(f64::INFINITY, f64::min)
        .max(f64::EPSILON);
    if temp <= 0.0 {
        return candidates
            .iter()
            .min_by(|a, b| a.e_cost.partial_cmp(&b.e_cost).unwrap());
    }
    let weights: Vec<f64> = candidates
        .iter()
        .map(|c| ((-c.e_cost / min) / temp).clamp(-700.0, 0.0).exp())
        .collect();
    let total: f64 = weights.iter().sum();
    if total <= 0.0 || !total.is_finite() {
        return candidates
            .iter()
            .min_by(|a, b| a.e_cost.partial_cmp(&b.e_cost).unwrap());
    }
    let mut r = rng.random::<f64>() * total;
    for (c, w) in candidates.iter().zip(&weights) {
        r -= w;
        if r <= 0.0 {
            return Some(c);
        }
    }
    candidates.last()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sla::types::*;

    fn mk_fit(s: f64, p: f64, q: f64, p_bar: f64, sigma: f64) -> FittedParams {
        FittedParams {
            key: ModelKey {
                pname: "x".into(),
                system: "x".into(),
                tenant: "x".into(),
            },
            fit: if q > 0.0 {
                DurationFit::Usl {
                    s: RefSeconds(s),
                    p: RefSeconds(p),
                    q,
                    p_bar: RawCores(p_bar),
                }
            } else if p_bar.is_finite() {
                DurationFit::Capped {
                    s: RefSeconds(s),
                    p: RefSeconds(p),
                    p_bar: RawCores(p_bar),
                }
            } else {
                DurationFit::Amdahl {
                    s: RefSeconds(s),
                    p: RefSeconds(p),
                }
            },
            mem: MemFit::Independent {
                p90: MemBytes(2 << 30),
            },
            disk_p90: Some(DiskBytes(10 << 30)),
            sigma_resid: sigma,
            log_residuals: Vec::new(),
            n_eff: 10.0,
            span: 8.0,
            explore: ExploreState {
                distinct_c: 3,
                min_c: RawCores(4.0),
                max_c: RawCores(32.0),
                saturated: false,
                last_wall: WallSeconds(0.0),
            },
            t_min_ci: None,
            ci_computed_at: None,
            tier: None,
            hw_bias: Default::default(),
            prior_source: None,
        }
    }
    fn t(name: &str, p90: f64) -> Tier {
        Tier {
            name: name.into(),
            p50: None,
            p90: Some(p90),
            p99: None,
        }
    }
    fn t3(p50: f64, p90: f64, p99: f64) -> Tier {
        Tier {
            name: "x".into(),
            p50: Some(p50),
            p90: Some(p90),
            p99: Some(p99),
        }
    }
    fn ceil() -> Ceilings {
        Ceilings {
            max_cores: 64.0,
            max_mem: 256 << 30,
            max_disk: 200 << 30,
            default_disk: 20 << 30,
        }
    }
    fn cfg() -> SlaConfig {
        SlaConfig {
            probe: super::super::config::ProbeShape {
                cpu: 4.0,
                mem_per_core: 1 << 30,
                mem_base: 4 << 30,
                deadline_secs: 3600,
            },
            max_cores: 64.0,
            max_mem: 256 << 30,
            max_disk: 200 << 30,
            default_disk: 20 << 30,
            ..SlaConfig::test_default()
        }
    }

    #[test]
    fn envelope_degenerates_to_amdahl_at_q0() {
        let fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
        let SolveResult::Feasible { c_star, tier, .. } =
            solve_mvp(&fit, &[t("t0", 300.0)], &ceil())
        else {
            panic!()
        };
        // β=30-e^{-0.128}·300≈-234 → c*=2000/234≈8.54 → ceil 9
        assert_eq!(c_star.0, 9.0, "ceil(8.54)");
        assert_eq!(tier, "t0");
    }
    #[test]
    fn rejects_not_clamps_when_cstar_exceeds_cap() {
        // r[verify sched.sla.solve-reject-not-clamp]
        let fit = mk_fit(30.0, 100.0, 0.0, 4.0, 0.1); // p̄=4 caps it
        let SolveResult::Feasible { tier, .. } =
            solve_mvp(&fit, &[t("t0", 60.0), t("t1", 600.0)], &ceil())
        else {
            panic!()
        };
        assert_eq!(tier, "t1"); // t0 needs >4 cores → rejected, lands t1
    }
    #[test]
    fn discriminant_negative_rejects_tier() {
        // USL fit, q=5 → c_opt=√(2000/5)=20. Tier infeasible → BestEffort
        // at cap_c=min(p̄,c_opt,max_cores)=20, NOT min(p̄,max_cores)=64
        // (T(20)≈230s vs T(64)≈381s — past c_opt is slower AND wider).
        let fit = mk_fit(30.0, 2000.0, 5.0, f64::INFINITY, 0.1);
        let SolveResult::BestEffort { c, .. } = solve_mvp(&fit, &[t("t0", 50.0)], &ceil()) else {
            panic!()
        };
        assert_eq!(c.0, 20.0, "BestEffort caps at c_opt, not max_cores");
    }
    #[test]
    fn best_effort_clamps_disk_at_ceiling() {
        // disk_p90 > max_disk: every tier rejects (disk gate) →
        // BestEffort. Unclamped, this used to emit a 300Gi
        // ephemeral-storage request → permanently-Pending pod.
        let mut fit = mk_fit(400.0, 100.0, 0.0, f64::INFINITY, 0.1);
        fit.disk_p90 = Some(DiskBytes(300 << 30)); // > ceil.max_disk=200Gi
        let SolveResult::BestEffort { disk, .. } = solve_mvp(&fit, &[t("t0", 300.0)], &ceil())
        else {
            panic!()
        };
        assert_eq!(disk.0, 200 << 30, "clamped to ceil.max_disk");
    }
    // r[verify sched.sla.tier-envelope]
    #[test]
    fn envelope_tight_p99_forces_higher_c() {
        // p90=300 alone (σ=0.2): T·e^{0.2·1.2816}≤300 → T≤232.2 → c*≈9.89.
        // Adding p99=320:       T·e^{0.2·2.3263}≤320 → T≤200.9 → c*≈11.7.
        // (p50=250 is slack: T≤250 → c*≈9.1.) Bisection halts at
        // hi-lo<0.5 then ceils, so allow ±1 around the analytic
        // boundary; the load-bearing property is the inequality.
        let fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.2);
        let c_p90 = solve_envelope(&fit, &t("x", 300.0), 1.0, 64.0, 1.0, 0.0).unwrap();
        let c_full = solve_envelope(&fit, &t3(250.0, 300.0, 320.0), 1.0, 64.0, 1.0, 0.0).unwrap();
        assert!((10.0..=11.0).contains(&c_p90.0), "p90-only c*={}", c_p90.0);
        assert!((12.0..=13.0).contains(&c_full.0), "full c*={}", c_full.0);
        assert!(
            c_full.0 > c_p90.0,
            "p99 must bind: full={} p90-only={}",
            c_full.0,
            c_p90.0
        );
    }
    #[test]
    fn envelope_loose_p99_permits_spot() {
        // λ=0.002 (spot preemption): at c=14, T≈173 → p≈0.29; geometric
        // tail inflates p99. Loose p99=2000 absorbs it; tight p99 doesn't.
        let fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.2);
        let loose = solve_envelope(&fit, &t3(250.0, 300.0, 2000.0), 1.0, 64.0, 1.0, 0.002);
        let tight = solve_envelope(&fit, &t3(250.0, 300.0, 320.0), 1.0, 64.0, 1.0, 0.002);
        assert!(loose.is_some(), "loose p99 should be feasible under spot λ");
        assert!(
            tight.is_none() || tight.unwrap().0 > loose.unwrap().0,
            "tight p99 must reject or demand more cores: tight={tight:?} loose={loose:?}"
        );
    }
    #[test]
    fn envelope_degenerate_single_bound_matches_scalar_within_1core() {
        // tier {p90:300} only, σ=0.1, q=0, λ=0 → must agree with the
        // closed-form citardauq scalar solve to ±1 core.
        let fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
        let env = solve_envelope(&fit, &t("x", 300.0), 1.0, 64.0, 1.0, 0.0).unwrap();
        // Scalar citardauq: β = S - e^{-1.2816σ}·p90, c* = P / -β
        let beta = 30.0 - (-1.2816f64 * 0.1).exp() * 300.0;
        let scalar = 2000.0 / -beta;
        assert!(
            (env.0 - scalar).abs() <= 1.0,
            "envelope={} scalar={scalar:.2}",
            env.0
        );
    }
    #[test]
    fn envelope_ceil_does_not_overallocate() {
        // c* ≈ 8.9 (T·e^{0.128} ≤ 289.5 → T ≤ 254.7 → c* = 2000/224.7).
        // Bisection from (1, 64) lands hi≈9.37 (lo≈8.88, gap<0.5); raw
        // `hi.ceil()` would return 10 but feasible(9) holds (T(9)=252.2,
        // q=286.7 ≤ 289.5). The post-ceil `feasible(n-1)` probe catches
        // the +1 over-allocation.
        let fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
        let c = solve_envelope(&fit, &t("x", 289.5), 1.0, 64.0, 1.0, 0.0).unwrap();
        assert_eq!(
            c.0, 9.0,
            "feasible(9) holds; ceil(hi≈9.37)=10 over-allocates"
        );
    }
    #[test]
    fn envelope_infeasible_at_cap_is_none() {
        let fit = mk_fit(400.0, 100.0, 0.0, f64::INFINITY, 0.1);
        assert!(solve_envelope(&fit, &t("x", 300.0), 1.0, 64.0, 1.0, 0.0).is_none());
    }
    #[test]
    fn envelope_no_bounds_returns_cap_c() {
        // No-bounds tier → max useful cores (cap_c), not c_lo. Under
        // helm-default tiers=[fast,normal,slow,{best-effort}] a build
        // infeasible at the bounded tiers must land at cap_c, not 1.
        let fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
        let tier = Tier {
            name: "be".into(),
            p50: None,
            p90: None,
            p99: None,
        };
        assert_eq!(
            solve_envelope(&fit, &tier, 1.0, 64.0, 1.0, 0.0).unwrap().0,
            64.0
        );
        // USL fit: cap_c = c_opt = 20.
        let usl = mk_fit(30.0, 2000.0, 5.0, f64::INFINITY, 0.1);
        assert_eq!(
            solve_envelope(&usl, &tier, 1.0, 20.0, 1.0, 0.0).unwrap().0,
            20.0
        );
    }

    #[test]
    fn beta_nonneg_rejects_tier() {
        // S=400 > target → infeasible
        let fit = mk_fit(400.0, 100.0, 0.0, f64::INFINITY, 0.1);
        assert!(matches!(
            solve_mvp(&fit, &[t("t0", 300.0)], &ceil()),
            SolveResult::BestEffort { .. }
        ));
    }

    // ─── intent_for branching ───────────────────────────────────────

    fn intent(fit: Option<&FittedParams>, hints: &DrvHints) -> (u32, u64, u64) {
        intent_for(fit, hints, None, &cfg(), &[t("normal", 1200.0)], &ceil())
    }

    // r[verify sched.sla.override-precedence]
    #[test]
    fn override_forced_cores_bypasses_fit() {
        // Fitted curve says ~2 cores against p90=1200; override forces 12.
        let fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
        let o = ResolvedTarget {
            forced_cores: Some(12.0),
            forced_mem: Some(32 << 30),
            ..Default::default()
        };
        let (c, m, _) = intent_for(
            Some(&fit),
            &DrvHints::default(),
            Some(&o),
            &cfg(),
            &[t("normal", 1200.0)],
            &ceil(),
        );
        assert_eq!(c, 12);
        assert_eq!(m, 32 << 30);
    }
    #[test]
    fn override_beats_prefer_local() {
        // Operator pin trumps drv-declared `prefer_local_build`.
        let o = ResolvedTarget {
            forced_cores: Some(12.0),
            ..Default::default()
        };
        let (c, _, _) = intent_for(
            None,
            &DrvHints {
                prefer_local_build: Some(true),
                ..Default::default()
            },
            Some(&o),
            &cfg(),
            &[],
            &ceil(),
        );
        assert_eq!(c, 12);
    }
    #[test]
    fn override_tier_filters_solve_ladder() {
        // Against [fast(p90=300), slow(p90=1200)] this fit lands on
        // fast at c≈9. With tier=slow pinned, fast is filtered out →
        // c≈2. With tier=unknown → empty ladder → BestEffort at cap_c.
        let fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
        let tiers = [t("fast", 300.0), t("slow", 1200.0)];
        let with = |tier: &str| {
            intent_for(
                Some(&fit),
                &DrvHints::default(),
                Some(&ResolvedTarget {
                    tier: Some(tier.into()),
                    ..Default::default()
                }),
                &cfg(),
                &tiers,
                &ceil(),
            )
            .0
        };
        let unpinned = intent_for(
            Some(&fit),
            &DrvHints::default(),
            None,
            &cfg(),
            &tiers,
            &ceil(),
        )
        .0;
        assert_eq!(unpinned, 9);
        assert_eq!(with("slow"), 2, "pinned tier=slow skips fast");
        assert_eq!(with("fast"), 9);
        assert_eq!(with("nope"), 64, "unknown tier → BestEffort cap_c");
    }
    #[test]
    fn override_mem_applies_without_cores() {
        // forced_mem alone: solve runs for cores, mem is forced.
        let fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
        let o = ResolvedTarget {
            forced_mem: Some(64 << 30),
            ..Default::default()
        };
        let (c, m, _) = intent_for(
            Some(&fit),
            &DrvHints::default(),
            Some(&o),
            &cfg(),
            &[t("normal", 1200.0)],
            &ceil(),
        );
        assert_eq!(c, 2, "cores still solved");
        assert_eq!(m, 64 << 30, "mem forced");
        // Also applies on the explore path (no fit).
        let (c, m, _) = intent_for(None, &DrvHints::default(), Some(&o), &cfg(), &[], &ceil());
        assert_eq!(c, 4, "probe.cpu");
        assert_eq!(m, 64 << 30);
    }

    #[test]
    fn intent_for_prefer_local_is_minimal() {
        let hints = DrvHints {
            prefer_local_build: Some(true),
            ..Default::default()
        };
        let (c, m, d) = intent(None, &hints);
        assert_eq!((c, m, d), (1, LOCAL_MEM_BYTES, LOCAL_DISK_BYTES));
        // With a fit: cores/mem stay minimal (semantic: trivial build),
        // but disk follows disk_p90 — `resource_floor` is per-drv_hash
        // so a fresh version would otherwise re-climb DiskPressure.
        let fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
        let (c, m, d) = intent(Some(&fit), &hints);
        assert_eq!((c, m, d), (1, LOCAL_MEM_BYTES, 10 << 30));
    }
    #[test]
    fn intent_for_serial_pins_one_core() {
        // enable_parallel_building=false pins ONLY cores=1; mem/disk
        // come from the fit (M(1) × headroom, disk_p90), not probe
        // defaults.
        let fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
        let hints = DrvHints {
            enable_parallel_building: Some(false),
            ..Default::default()
        };
        let (c, m, d) = intent(Some(&fit), &hints);
        assert_eq!(c, 1);
        let want = ((2u64 << 30) as f64 * headroom(10.0)) as u64;
        assert_eq!(m, want, "MemFit.at(1) × headroom(n_eff), not probe_mem");
        assert_eq!(d, 10 << 30, "fit.disk_p90, not default_disk");
        // No fit → probe defaults.
        let (c, m, d) = intent(None, &hints);
        assert_eq!((c, m, d), (1, 8 << 30, ceil().default_disk));
    }
    // r[verify sched.sla.headroom-confidence-scaled]
    #[test]
    fn intent_for_serial_applies_headroom_on_coupled_fit() {
        // MemFit::Coupled.at() is the regression line (~p50), NOT a
        // quantile. The serial early-return must scale by
        // headroom(n_eff) just like solve_mvp does. Regression: it
        // returned the raw fit → under-provision → OOM.
        let mut fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
        fit.mem = MemFit::Coupled {
            a: 21.5,
            b: 0.3,
            r1: 0.9,
        };
        fit.n_eff = 3.0;
        let hints = DrvHints {
            enable_parallel_building: Some(false),
            ..Default::default()
        };
        let (_, m, _) = intent(Some(&fit), &hints);
        let raw = 21.5f64.exp() as u64;
        let want = (raw as f64 * headroom(3.0)) as u64;
        assert_eq!(m, want);
        assert!(m > raw * 3 / 2, "headroom(3.0)≈1.65 must inflate raw");
    }
    #[test]
    fn intent_for_forced_cores_applies_headroom() {
        let mut fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
        fit.mem = MemFit::Coupled {
            a: 21.5,
            b: 0.3,
            r1: 0.9,
        };
        let o = ResolvedTarget {
            forced_cores: Some(12.0),
            ..Default::default()
        };
        let (c, m, _) = intent_for(
            Some(&fit),
            &DrvHints::default(),
            Some(&o),
            &cfg(),
            &[],
            &ceil(),
        );
        assert_eq!(c, 12);
        let raw = fit.mem.at(RawCores(12.0)).0;
        assert_eq!(m, (raw as f64 * headroom(10.0)) as u64);
        assert!(m > raw, "must include headroom factor");
    }
    #[test]
    fn intent_for_probe_mem_honors_feature_probes() {
        // `kvm` builds want a high mem floor regardless of core count
        // (config.rs:28-30). The serial / forced_cores cold-start
        // branches must consult `feature_probes`, not `cfg.probe`.
        let mut cfg = cfg();
        cfg.feature_probes.insert(
            "kvm".into(),
            super::super::config::ProbeShape {
                cpu: 4.0,
                mem_per_core: 2 << 30,
                mem_base: 16 << 30,
                deadline_secs: 7200,
            },
        );
        let hints = DrvHints {
            enable_parallel_building: Some(false),
            required_features: vec!["kvm".into()],
            ..Default::default()
        };
        let (c, m, _) = intent_for(None, &hints, None, &cfg, &[], &ceil());
        assert_eq!(c, 1);
        // 4×2Gi + 16Gi = 24Gi (kvm probe), NOT 4×1Gi + 4Gi = 8Gi (default).
        assert_eq!(m, 24 << 30, "feature_probes[kvm], not cfg.probe");
    }
    #[test]
    fn serial_honors_forced_mem() {
        // forced_mem (no forced_cores) must override mem in the
        // prefer_local / serial early-return branches too — the
        // doc-promise on `intent_for` says "any branch".
        let o = ResolvedTarget {
            forced_mem: Some(64 << 30),
            ..Default::default()
        };
        let go = |hints| intent_for(None, &hints, Some(&o), &cfg(), &[], &ceil());
        let (c, m, _) = go(DrvHints {
            enable_parallel_building: Some(false),
            ..Default::default()
        });
        assert_eq!((c, m), (1, 64 << 30), "serial: cores pinned, mem forced");
        let (c, m, _) = go(DrvHints {
            prefer_local_build: Some(true),
            ..Default::default()
        });
        assert_eq!((c, m), (1, 64 << 30), "prefer_local: mem forced");
    }
    #[test]
    fn intent_for_cold_start_is_probe() {
        // No fit → explore::next emits cfg.probe (4 cores, 4×1+4=8 GiB).
        assert_eq!(
            intent(None, &DrvHints::default()),
            (4, 8 << 30, ceil().default_disk)
        );
        // n_eff < 3 → still explore path. mk_fit's explore state is
        // min=4/max=32 → frozen() = true (span 8) → solve gate would
        // pass if n_eff≥3; below that, explore::next walks the ladder
        // from the fit's existing range.
        let mut fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
        fit.n_eff = 2.0;
        let (c, _, _) = intent(Some(&fit), &DrvHints::default());
        assert!(c >= 4, "explore ladder ≥ probe.cpu, got {c}");
    }
    // r[verify sched.sla.intent-from-solve]
    #[test]
    fn intent_for_clamps_disk_at_ceil_in_all_branches() {
        // disk_p90 = 300 GiB > ceil.max_disk = 200 GiB. Every branch
        // that reads `disk_p90` must clamp — the `solve_intent_for`
        // chokepoint is the production guard, but `intent_for` stays
        // self-consistent for `explain.rs`.
        let mut fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
        fit.disk_p90 = Some(DiskBytes(300 << 30));
        let max = ceil().max_disk;
        // forced_cores override.
        let o = ResolvedTarget {
            forced_cores: Some(12.0),
            ..Default::default()
        };
        let (_, _, d) = intent_for(
            Some(&fit),
            &DrvHints::default(),
            Some(&o),
            &cfg(),
            &[],
            &ceil(),
        );
        assert!(d <= max, "forced_cores: disk {d} > max_disk {max}");
        // prefer_local_build.
        let (_, _, d) = intent(
            Some(&fit),
            &DrvHints {
                prefer_local_build: Some(true),
                ..Default::default()
            },
        );
        assert!(d <= max, "prefer_local: disk {d} > max_disk {max}");
        // enable_parallel_building=false.
        let (_, _, d) = intent(
            Some(&fit),
            &DrvHints {
                enable_parallel_building: Some(false),
                ..Default::default()
            },
        );
        assert!(d <= max, "serial: disk {d} > max_disk {max}");
        // explore ladder (n_eff < 3 with a fit).
        let mut explore_fit = fit.clone();
        explore_fit.n_eff = 2.0;
        let (_, _, d) = intent(Some(&explore_fit), &DrvHints::default());
        assert!(d <= max, "explore: disk {d} > max_disk {max}");
    }
    // ─── solve_full / softmax (Algorithm 4) ─────────────────────────

    use rand::SeedableRng;
    use rand::rngs::StdRng;
    use std::collections::HashMap;

    fn hw_mid_only() -> HwTable {
        // Only mid-band has bench data; hi/lo fall back to factor=1.0
        // (the empty-band h_dagger path).
        let mut m = HashMap::new();
        m.insert("aws-7-ebs-mid".into(), 1.0);
        m.insert("aws-7-nvme-mid".into(), 1.2);
        HwTable::from_map(m)
    }

    fn cand(band: Band, cap: Cap, e_cost: f64) -> Candidate {
        Candidate {
            band,
            cap,
            c_star: RawCores(4.0),
            mem: MemBytes(2 << 30),
            e_cost,
        }
    }

    // r[verify sched.sla.solve-per-band-cap]
    #[test]
    fn picks_cheapest_feasible_band_cap() {
        let fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
        let cost = CostTable::default(); // seed: spot < on-demand
        let mut rng = StdRng::seed_from_u64(0);
        let r = solve_full(
            &fit,
            &[t("normal", 1200.0)],
            &hw_mid_only(),
            &cost,
            &ceil(),
            &IceBackoff::default(),
            0.0, // greedy
            &mut rng,
        );
        let SolveResult::Feasible { node_selector, .. } = r else {
            panic!("expected Feasible")
        };
        let ns = node_selector.expect("solve_full sets selector");
        // Greedy + seed prices ⇒ spot. Band depends on h_dagger; with
        // mid-only hw table, all bands solve at factor≤1.0 so cheapest
        // $/vCPU·hr (lo-band spot) wins.
        assert_eq!(ns.get("karpenter.sh/capacity-type").unwrap(), "spot");
        assert_eq!(ns.get("rio.build/hw-band").unwrap(), "lo");
    }

    #[test]
    fn spot_infeasible_when_p_gt_half() {
        // λ huge ⇒ p(cap_c) > 0.5 for every band ⇒ spot rejected,
        // on-demand survives. Fix temp=0 (greedy) so the pick is
        // deterministic.
        let fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
        let mut lambda = HashMap::new();
        for b in Band::ALL {
            lambda.insert(
                b,
                super::super::cost::RatioEma {
                    numerator: 1.0,
                    denominator: 1.0, // λ=1/s — absurd
                    updated_at: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs_f64(),
                },
            );
        }
        let cost = CostTable::from_parts(HashMap::new(), lambda);
        let mut rng = StdRng::seed_from_u64(0);
        let r = solve_full(
            &fit,
            &[t("normal", 1200.0)],
            &hw_mid_only(),
            &cost,
            &ceil(),
            &IceBackoff::default(),
            0.0,
            &mut rng,
        );
        let SolveResult::Feasible { node_selector, .. } = r else {
            panic!()
        };
        assert_eq!(
            node_selector.unwrap().get("karpenter.sh/capacity-type"),
            Some(&"on-demand".to_string()),
            "spot must be rejected when p>0.5"
        );
    }

    #[test]
    fn h_dagger_is_per_pname_effective_slowest() {
        // Two mid-band classes: ebs factor=1.0, nvme factor=1.2.
        // With no bias, h† = ebs (slowest = lowest factor). With this
        // pname biased 1.5× SLOWER on nvme (bias>1 = under-performs
        // the bench), nvme's effective factor 1.2/1.5=0.8 < ebs's
        // 1.0/1.0 → h† flips to nvme.
        let hw = hw_mid_only();
        let no_bias = HashMap::new();
        let (h, f) = hw.h_dagger("foo", Band::Mid, &no_bias);
        assert_eq!(h, "aws-7-ebs-mid");
        assert!((f - 1.0).abs() < 1e-9);

        let mut bias = HashMap::new();
        bias.insert("aws-7-nvme-mid".into(), 1.5);
        let (h, f) = hw.h_dagger("foo", Band::Mid, &bias);
        assert_eq!(
            h, "aws-7-nvme-mid",
            "per-pname bias flips effective-slowest"
        );
        assert!((f - 0.8).abs() < 1e-9);

        // Band with no hw_classes → ("", 1.0).
        let (h, f) = hw.h_dagger("foo", Band::Hi, &no_bias);
        assert_eq!(h, "");
        assert!((f - 1.0).abs() < 1e-9);
    }

    /// Regression: `h_dagger` returned `f / bias` unclamped; with a
    /// pathological factor row OR a large `hw_bias` denominator the
    /// effective factor went ≪ 0.25 → `t = T(c)/factor` blew up →
    /// `feasible(cap_c) = false` → entire band dropped out for both
    /// Spot and OnDemand.
    #[test]
    fn h_dagger_floors_after_bias_division() {
        use super::super::hw::HW_FACTOR_SANITY_FLOOR;
        let mut m = HashMap::new();
        m.insert("aws-7-ebs-mid".into(), 0.3);
        let hw = HwTable::from_map(m);
        let mut bias = HashMap::new();
        bias.insert("aws-7-ebs-mid".into(), 3.0);
        // Raw 0.3/3.0 = 0.1; must floor to 0.25.
        let (_, f) = hw.h_dagger("foo", Band::Mid, &bias);
        assert_eq!(f, HW_FACTOR_SANITY_FLOOR);
    }

    #[test]
    fn solve_full_survives_pathological_hw_factor() {
        // T(cap_c)=30+2000/64≈61s, tier p90=600. With factor=0.01,
        // t=6100 → infeasible → mid drops out. With FLOOR=0.25, t=244
        // → feasible. Hi/Lo bands have no hw_classes → factor=1.0 →
        // would survive anyway, so check Mid specifically.
        let fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
        let mut m = HashMap::new();
        m.insert("aws-7-ebs-mid".into(), 0.01);
        let hw = HwTable::from_map(m);
        let mut bias = HashMap::new();
        // Further: bias-divides 0.01 / 10 = 0.001. Floor must catch.
        bias.insert("aws-7-ebs-mid".into(), 10.0);
        let mut fit_b = fit.clone();
        fit_b.hw_bias = bias;
        let ice = IceBackoff::default();
        // Mark all non-Mid cells so the solve must pick Mid or fail.
        for c in Cap::ALL {
            ice.mark(Band::Hi, c);
            ice.mark(Band::Lo, c);
        }
        let mut rng = StdRng::seed_from_u64(0);
        let r = solve_full(
            &fit_b,
            &[t("normal", 600.0)],
            &hw,
            &CostTable::default(),
            &ceil(),
            &ice,
            0.0,
            &mut rng,
        );
        assert!(
            matches!(r, SolveResult::Feasible { .. }),
            "pathological factor must not drop band: {r:?}"
        );
    }

    // ─── infeasible_total emission ──────────────────────────────────

    fn infeasible_count(snap: &metrics_util::debugging::Snapshotter, reason: &str) -> u64 {
        use metrics_util::debugging::DebugValue;
        snap.snapshot()
            .into_vec()
            .into_iter()
            .find_map(|(ck, _, _, v)| {
                let k = ck.key();
                (k.name() == "rio_scheduler_sla_infeasible_total"
                    && k.labels()
                        .any(|l| l.key() == "reason" && l.value() == reason))
                .then_some(match v {
                    DebugValue::Counter(c) => c,
                    _ => 0,
                })
            })
            .unwrap_or(0)
    }

    /// Regression: `rio_scheduler_sla_infeasible_total` was registered +
    /// documented but `capacity_exhausted()` had ZERO non-test callers.
    /// observability.md:147 documented an alerting hook that could not
    /// fire.
    #[test]
    fn solve_full_emits_capacity_exhausted_when_ice_exhausted() {
        let rec = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = rec.snapshotter();
        metrics::with_local_recorder(&rec, || {
            let fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
            let ice = IceBackoff::default();
            for b in Band::ALL {
                for c in Cap::ALL {
                    ice.mark(b, c);
                }
            }
            let mut rng = StdRng::seed_from_u64(0);
            let r = solve_full(
                &fit,
                &[t("normal", 1200.0)],
                &hw_mid_only(),
                &CostTable::default(),
                &ceil(),
                &ice,
                0.0,
                &mut rng,
            );
            assert!(matches!(r, SolveResult::BestEffort { .. }));
        });
        assert_eq!(infeasible_count(&snapshotter, "capacity_exhausted"), 1);
    }

    #[test]
    fn solve_mvp_emits_ceiling_exhausted_on_all_bounded_infeasible() {
        let rec = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = rec.snapshotter();
        metrics::with_local_recorder(&rec, || {
            // T(c) ≫ 10 for all c ≤ 64: S=1e6.
            let fit = mk_fit(1e6, 0.0, 0.0, f64::INFINITY, 0.1);
            let r = solve_mvp(&fit, &[t("normal", 10.0)], &ceil());
            assert!(matches!(r, SolveResult::BestEffort { .. }));
        });
        assert_eq!(infeasible_count(&snapshotter, "ceiling_exhausted"), 1);
    }

    #[test]
    fn solve_mvp_no_emit_on_best_effort_tier() {
        let rec = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = rec.snapshotter();
        metrics::with_local_recorder(&rec, || {
            let fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
            let unbounded = Tier {
                name: "be".into(),
                p50: None,
                p90: None,
                p99: None,
            };
            // Unbounded tier returns Feasible at cap_c (solve_envelope's
            // no-bounds arm), so no BestEffort fallthrough.
            solve_mvp(&fit, &[unbounded], &ceil());
            // Empty ladder (unknown pinned-tier) → BestEffort but NOT
            // an exhaust event.
            solve_mvp(&fit, &[], &ceil());
        });
        assert_eq!(infeasible_count(&snapshotter, "ceiling_exhausted"), 0);
        assert_eq!(infeasible_count(&snapshotter, "capacity_exhausted"), 0);
    }

    #[test]
    fn softmax_spreads_at_temp03() {
        // Two candidates at 1.0 vs 1.5 cost. At temp=0.3, the cheap
        // one should win clearly (>70%) but not always (<98%).
        let cands = [
            cand(Band::Lo, Cap::Spot, 1.0),
            cand(Band::Hi, Cap::OnDemand, 1.5),
        ];
        let mut rng = StdRng::seed_from_u64(42);
        let mut cheap = 0;
        for _ in 0..10_000 {
            if softmax_pick(&cands, 0.3, &mut rng).unwrap().e_cost == 1.0 {
                cheap += 1;
            }
        }
        let frac = cheap as f64 / 10_000.0;
        assert!(
            (0.70..0.98).contains(&frac),
            "temp=0.3 should spread but favor cheapest; got {frac}"
        );
        // temp=0 → greedy: always cheapest.
        for _ in 0..100 {
            assert_eq!(softmax_pick(&cands, 0.0, &mut rng).unwrap().e_cost, 1.0);
        }
        // Single candidate → that one.
        assert_eq!(
            softmax_pick(&cands[..1], 0.3, &mut rng).unwrap().band,
            Band::Lo
        );
        assert!(softmax_pick(&[], 0.3, &mut rng).is_none());
    }

    #[test]
    fn ice_backoff_excludes_cell() {
        let fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
        let ice = IceBackoff::default();
        // Mark every spot cell infeasible.
        for b in Band::ALL {
            ice.mark(b, Cap::Spot);
        }
        let mut rng = StdRng::seed_from_u64(0);
        let r = solve_full(
            &fit,
            &[t("normal", 1200.0)],
            &hw_mid_only(),
            &CostTable::default(),
            &ceil(),
            &ice,
            0.0,
            &mut rng,
        );
        let SolveResult::Feasible { node_selector, .. } = r else {
            panic!()
        };
        assert_eq!(
            node_selector.unwrap().get("karpenter.sh/capacity-type"),
            Some(&"on-demand".to_string())
        );
    }

    #[test]
    fn intent_for_fitted_uses_solve() {
        // r[verify sched.sla.intent-from-solve]
        let fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
        let (c, _, d) = intent(Some(&fit), &DrvHints::default());
        // Against p90=1200: c*≈1.95 → ceil 2.
        assert_eq!(c, 2, "ceil(solve_mvp.c_star)");
        assert_eq!(d, 10 << 30, "disk_p90 from fit");
    }
}
