use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};

use super::alpha;
use super::config::{CapacityType, Cell, HwClassDef, HwClassName, SlaConfig};
use super::cost::CostTable;
use super::explore;
use super::fit::headroom;
use super::hw::{HW_FACTOR_SANITY_FLOOR, HwTable};
use super::r#override::ResolvedTarget;
use super::quantile;
use super::types::{DiskBytes, DurationFit, FittedParams, MemBytes, RawCores};

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
    /// precedence p90→p50→p99 (`r[sched.sla.reassign-schmitt]`). `None`
    /// for a no-bounds tier — callers treat that as "always met".
    ///
    /// Single source of truth: the explore ×4/÷2 gate, envelope hit/miss
    /// scoring, prior basis, `solve_tiers()` sort order, and the
    /// `reassign_tier()` Schmitt walk all read this so a p50-only tier
    /// (legal config — `validate()` doesn't require p90) gets ONE answer.
    /// Returns wall-seconds (tier targets are operator-facing, not
    /// reference-seconds).
    pub fn binding_bound(&self) -> Option<f64> {
        self.p90.or(self.p50).or(self.p99)
    }

    /// Startup-time bounds checks. Called from
    /// [`SlaConfig::validate`](super::config::SlaConfig::validate) so a
    /// negative/NaN bound fails loud at startup instead of wrapping to 0
    /// in `solve_tiers()`' `(d * 1000.0) as u64` sort key (which would
    /// silently sort the broken tier as "tightest").
    pub fn validate(&self) -> anyhow::Result<()> {
        for (field, v) in [("p50", self.p50), ("p90", self.p90), ("p99", self.p99)] {
            if let Some(v) = v {
                anyhow::ensure!(
                    v.is_finite() && v > 0.0,
                    "sla.tiers[{}].{field} must be finite and positive, got {v}",
                    self.name
                );
            }
        }
        Ok(())
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
    },
    BestEffort {
        c: RawCores,
        mem: MemBytes,
        disk: DiskBytes,
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
}

/// `rio_scheduler_sla_infeasible_total{reason=…,tenant=…}` label
/// values. The type-safe single emit point for the metric — every
/// caller routes through [`Self::emit`] so the label string can't drift
/// from `observability.md`.
///
/// ADR-023 §Observability splits the legacy `ceiling_exhausted` reason
/// into the four specific ceilings + `interrupt_runaway` so the alert
/// names which dimension is binding. [`classify_ceiling`] computes the
/// split for the [`solve_mvp`] BestEffort fallthrough.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfeasibleReason {
    /// `S ≥ bound` at the loosest tier — even infinite cores can't help.
    SerialFloor,
    /// `M(c*)·headroom > sla.maxMem` at the feasible `c*`.
    MemCeiling,
    /// `disk_p90 > sla.maxDisk` (c-independent).
    DiskCeiling,
    /// Envelope infeasible at `cap_c = min(p̄, c_opt, sla.maxCores)`.
    CoreCeiling,
    /// `λ` makes spot infeasible at every `c` (preemption-rate runaway).
    InterruptRunaway,
    /// All `(hw_class, cap)` cells ICE-masked at the terminal tier.
    CapacityExhausted,
}

impl InfeasibleReason {
    /// All variants, in ADR-023 §Observability order (the order the
    /// `infeasible_reasons_complete` test pins).
    pub const ALL: [Self; 6] = [
        Self::SerialFloor,
        Self::MemCeiling,
        Self::DiskCeiling,
        Self::CoreCeiling,
        Self::InterruptRunaway,
        Self::CapacityExhausted,
    ];

    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::SerialFloor => "serial_floor",
            Self::MemCeiling => "mem_ceiling",
            Self::DiskCeiling => "disk_ceiling",
            Self::CoreCeiling => "core_ceiling",
            Self::InterruptRunaway => "interrupt_runaway",
            Self::CapacityExhausted => "capacity_exhausted",
        }
    }

    /// Increment `rio_scheduler_sla_infeasible_total{reason=…,tenant=…}`.
    /// Single emit point — grep `\.emit(` to find every infeasible call
    /// site.
    pub fn emit(self, tenant: &str) {
        metrics::counter!(
            "rio_scheduler_sla_infeasible_total",
            "reason" => self.as_str(),
            "tenant" => tenant.to_owned()
        )
        .increment(1);
    }
}

/// Which of the four ceiling reasons bound at the loosest bounded tier
/// when [`solve_mvp`] / [`solve_full`] fell through to `BestEffort`.
/// Mirrors `solve_mvp`'s per-tier reject gates so the metric label
/// matches what `sla explain` shows. `InterruptRunaway` /
/// `CapacityExhausted` are gated separately by callers (those need λ /
/// ICE state this fn doesn't see).
pub fn classify_ceiling(fit: &FittedParams, tiers: &[Tier], ceil: &Ceilings) -> InfeasibleReason {
    let cap_c = fit.fit.p_bar().0.min(fit.fit.c_opt().0).min(ceil.max_cores);
    let h = headroom(fit.n_eff);
    if fit.disk_p90.is_some_and(|d| d.0 > ceil.max_disk) {
        return InfeasibleReason::DiskCeiling;
    }
    // Loosest bounded tier — the last one solve_mvp's reject-not-clamp
    // loop tried. `unreachable` arm: callers gate on ≥1 bounded tier.
    let Some(tier) = tiers.iter().rev().find(|t| t.binding_bound().is_some()) else {
        return InfeasibleReason::CoreCeiling;
    };
    match explain_envelope(fit, tier, cap_c) {
        (None, "serial-floor") => InfeasibleReason::SerialFloor,
        (None, _) => InfeasibleReason::CoreCeiling,
        (Some(c), _) if (fit.mem.at(RawCores(c)).0 as f64 * h) as u64 > ceil.max_mem => {
            InfeasibleReason::MemCeiling
        }
        // Envelope feasible AND mem under ceiling at the loosest tier
        // yet solve_mvp returned BestEffort — only reachable if a
        // tighter tier's feasible c* tripped mem and the loosest one
        // didn't (shouldn't happen with tiers sorted tight→loose).
        (Some(_), _) => InfeasibleReason::CoreCeiling,
    }
}

/// One feasible `(h, cap)` cell from [`solve_full`]'s inner loop.
/// `e_cost_upper` (ADR-023 L621) is the comparable scalar for the
/// admissible-set Schmitt deadband; `factor`/`lambda` are kept so the
/// re-filter can recompute `e_cost(c*)` without re-reading the tables.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub cell: Cell,
    pub c_star: u32,
    pub e_cost_upper: f64,
    /// Per-h `(α·factor[h]) / bias[h]` — what `T_ref(c)` is divided by.
    pub factor: f64,
    /// Per-h Poisson interrupt rate (0.0 for `Od`).
    pub lambda: f64,
}

/// Re-filtered admissible set + sizing. The deterministic output of
/// [`solve_full`] given `(fit, override, hw, cost, cfg)` — what the
/// memo caches.
#[derive(Debug, Clone)]
pub struct AdmissibleSet {
    /// Re-filtered cells — all satisfy fit-at-c*, cost-at-c* ≤
    /// (1+τ)E_min, c*_{h,cap} ≥ c*/k. Never empty (the argmax cell
    /// provably survives all three).
    pub cells: Vec<Cell>,
    pub c_star: u32,
    pub mem_bytes: u64,
    pub disk_bytes: u64,
}

/// One memo entry: `(A', candidates, tier)`. `all_candidates` lets the
/// per-dispatch ε_h draw and the controller's `GetSpawnIntents`
/// per-cell deficit see every feasible cell, not just `A'`.
#[derive(Debug, Clone)]
pub struct SolveMemo {
    pub a: AdmissibleSet,
    pub all_candidates: Vec<Candidate>,
    pub tier: String,
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
    /// [`Tier::binding_bound`] of `tier` at dispatch time. Captured here
    /// so completion's hit/miss check is robust to the tier ladder being
    /// reconfigured mid-build. `None` ⇔ `tier` is `None` OR the picked
    /// tier has no bounds.
    pub tier_target: Option<f64>,
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
            if f.n_eff >= 3.0
                && (f.span >= 4.0 || explore::frozen(&f.explore, ceil.max_cores))
                // `ingest::refit` sets `fit=Probe` whenever `span<4`,
                // independent of `frozen()`; without this filter the
                // `n_eff≥3 ∧ span<4 ∧ frozen` region (reachable via
                // the `min_c<=1` wall clause) hands a Probe fit to
                // `solve_mvp` → `t_at=∞` → cap_c=max_cores instead of
                // explore's `max_c`.
                && !matches!(f.fit, DurationFit::Probe) =>
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
    // Infeasible-at-any-tier — the operator-facing alerting hook
    // (observability.md). Gated on "≥1 tier had bounds": a pure
    // best-effort ladder (`tiers=[{p*:None}]`) or an empty ladder
    // (unknown pinned-tier override) is not an exhaust event. Emitted
    // HERE (the actual sizing decision-point for the non-hw-cost path)
    // so `solve_mvp` stays pure for tier-name lookup callers.
    if matches!(r, SolveResult::BestEffort { .. })
        && tiers
            .iter()
            .any(|t| t.p50.is_some() || t.p90.is_some() || t.p99.is_some())
    {
        classify_ceiling(fit, tiers, ceil).emit(&fit.key.tenant);
    }
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
    // r[impl sched.sla.hw-class.zq-inflation]
    // z_q is constant per (fit, q) — hoist out of the bisection so each
    // `feasible(c)` call is pure CDF arithmetic.
    let bounds: Vec<(f64, f64, f64)> = [(0.5, tier.p50), (0.9, tier.p90), (0.99, tier.p99)]
        .into_iter()
        .filter_map(|(q, b)| b.map(|b| (q, b, fit.z_q(q))))
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
            .all(|&(q, bound, zq)| quantile::quantile(q, t, fit.sigma_resid, p, zq) <= bound)
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
    if n > c_lo + 1.0 && feasible(n - 1.0) {
        return Some(RawCores(n - 1.0));
    }
    // `n = ⌈hi⌉` may be `⌈cap_c⌉ > cap_c` (USL c_opt non-integer); past
    // c_opt, T(c) increases, so `feasible(n)` is NOT implied by
    // `feasible(hi)`. When neither `n-1` nor `n` is feasible, no
    // integer in `[c_lo, cap_c]` is — fall through to the next tier.
    feasible(n).then_some(RawCores(n))
}

/// [`solve_envelope`] wrapper for [`super::explain::explain`]: returns
/// `(c_star, reason)` where `reason` is a `binding_constraint` string.
/// Mirrors `solve_mvp`'s per-tier outcome so the explain table can't
/// drift from what dispatch actually did.
///
/// `(Some(c), "-")` → feasible at `c`; `(Some(cap_c), "no-bounds")` →
/// no-bounds tier (matches `solve_envelope`'s `cap_c` early return);
/// `(None, "serial-floor")` → `S` alone breaches the bound (no `c`
/// helps); `(None, "envelope")` → infeasible at `cap_c` for some other
/// reason (p50/p99 bound, or USL contention floor).
pub fn explain_envelope(
    fit: &FittedParams,
    tier: &Tier,
    cap_c: f64,
) -> (Option<f64>, &'static str) {
    if tier.p50.is_none() && tier.p90.is_none() && tier.p99.is_none() {
        return (Some(cap_c), "no-bounds");
    }
    match solve_envelope(fit, tier, 1.0, cap_c, 1.0, 0.0) {
        Some(c) => (Some(c.0), "-"),
        None => {
            // Distinguish "serial floor breaches the bound" from the
            // general "envelope rejected at cap_c" so `sla explain`
            // points the operator at S vs the tier targets. S is
            // hw_factor=1 / lambda=0 here (matches solve_mvp).
            let (s, _, _) = fit.fit.spq();
            if tier.binding_bound().is_some_and(|b| s >= b) {
                (None, "serial-floor")
            } else {
                (None, "envelope")
            }
        }
    }
}

// r[impl sched.sla.solve-citardauq]
// r[impl sched.sla.solve-reject-not-clamp]
pub fn solve_mvp(fit: &FittedParams, tiers: &[Tier], ceil: &Ceilings) -> SolveResult {
    debug_assert!(
        !matches!(fit.fit, DurationFit::Probe),
        "solve_mvp called with DurationFit::Probe — gate must filter"
    );
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
        };
    }
    // Pure: callers emit `rio_scheduler_sla_infeasible_total{reason=…}`
    // at the sizing decision-point — same convention as `best_effort`'s
    // doc below. `solve_mvp` is also called for tier-NAME lookup
    // (`solve_intent_for`'s predicted block) where the metric must not
    // fire; emitting here double-counted infeasible drvs.
    best_effort(fit, cap_c, h, disk, ceil)
}

/// `BestEffort` constructor shared by [`solve_mvp`] / [`solve_full`]:
/// returns the clamped fallback shape. Callers emit the appropriate
/// `rio_scheduler_sla_infeasible_total{reason=…}` (ceiling vs capacity)
/// before calling — the gate differs between mvp/full.
///
/// D4: clamp at `ceil.max_*` — the Feasible arm gates on `mem/disk >
/// ceil` (reject-not-clamp); BestEffort must clamp so
/// `solve_intent_for`'s floor.max() composes (a `disk_p90 > max_disk`
/// here used to spawn a permanently-Pending pod). `cap_c` already
/// includes `c_opt` so a USL fit isn't pushed into its slowdown region.
fn best_effort(fit: &FittedParams, cap_c: f64, h: f64, disk: u64, ceil: &Ceilings) -> SolveResult {
    let mem = ((fit.mem.at(RawCores(cap_c)).0 as f64 * h) as u64).min(ceil.max_mem);
    SolveResult::BestEffort {
        c: RawCores(cap_c),
        mem: MemBytes(mem),
        disk: DiskBytes(disk.min(ceil.max_disk)),
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

/// Admissible-set [`solve_full`] outcome. `Feasible` carries the
/// memoizable [`SolveMemo`]; `BestEffort` carries the full `H × cap`
/// cell list (no admissible-set filtering).
#[derive(Debug, Clone)]
pub enum SolveFullResult {
    Feasible(SolveMemo),
    BestEffort {
        c: u32,
        mem_bytes: u64,
        disk_bytes: u64,
        cells: Vec<Cell>,
    },
}

/// `E[cost]^upper` for one cell at core count `c` (ADR-023 L621):
/// `price · c · (T(c)/factor)/3600 · e^{σ²/2} / (1-p)`. `p` is clamped
/// at 0.499 — callers gate `p(C)>0.5` separately so the divisor
/// stays `> 0.5`.
fn e_cost_upper(fit: &FittedParams, c: u32, factor: f64, lambda: f64, price: f64) -> f64 {
    let t = fit.fit.t_at(RawCores(f64::from(c))).0 / factor;
    let p = if lambda > 0.0 {
        (1.0 - (-lambda * t).exp()).min(0.499)
    } else {
        0.0
    };
    price * f64::from(c) * t / 3600.0 * (fit.sigma_resid.powi(2) / 2.0).exp() / (1.0 - p)
}

/// Per-h effective scalar factor for `fit`: `(α·factor[h]) / bias[h]`,
/// floored at [`HW_FACTOR_SANITY_FLOOR`]. ADR-023 L612 — `T(c, h) =
/// T_ref(c) / factor`. Unknown / <3-pod h → 1.0.
fn hw_factor_for(fit: &FittedParams, hw: &HwTable, h: &str) -> f64 {
    let f = hw.factor(h).unwrap_or([1.0; super::hw::K]);
    let bias = fit.hw_bias.get(h).copied().unwrap_or(1.0);
    (alpha::dot(fit.alpha, f) / bias).max(HW_FACTOR_SANITY_FLOOR)
}

// r[impl sched.sla.hw-class.admissible-set]
/// Algorithm `alg-estimate-full` (ADR-023 L781-812): per-`(h, cap)`
/// envelope solve + admissible-set + re-filter.
///
/// For each tier (tightest-first), enumerate `(h, cap) ∈ h_set ×
/// {spot, od}`: gate spot on `p(C) ≤ 0.5`; bisect c* satisfying the
/// envelope; gate on mem ceiling and `smallest_fitting`; compute
/// `E[cost]^upper`. If ≥1 candidate survives, build the admissible
/// set with Schmitt deadband (`τ_enter=τ`, `τ_exit=1.3τ`), take
/// `c* = max_A c*_{h,cap}`, then re-filter `A` by fit-at-c*,
/// cost-at-c* ≤ (1+τ)E_min, and capacity-ratio `c*_{h,cap} ≥ c*/k`.
///
/// **Deterministic** — the ICE mask and ε_h draw are applied at
/// dispatch time outside this function (read-time mask), so the
/// result is memoizable on `(fit_hash, override_hash, inputs_gen)`.
/// `prev_a` (the previous tick's admissible set for THIS key) feeds
/// the Schmitt deadband.
///
/// `h_set` is `cfg.hw_classes.keys()` for the unrestricted solve;
/// the ε_h pin passes a singleton.
#[allow(clippy::too_many_arguments)]
pub fn solve_full(
    fit: &FittedParams,
    tiers: &[Tier],
    hw: &HwTable,
    cost: &CostTable,
    ceil: &Ceilings,
    cfg: &SlaConfig,
    h_set: &[HwClassName],
    prev_a: &HashSet<Cell>,
) -> SolveFullResult {
    debug_assert!(
        !matches!(fit.fit, DurationFit::Probe),
        "solve_full called with DurationFit::Probe — gate must filter"
    );
    let cap_c = fit.fit.p_bar().0.min(fit.fit.c_opt().0).min(ceil.max_cores);
    let hr = headroom(fit.n_eff);
    let disk = fit.disk_p90.map(|d| d.0).unwrap_or(ceil.default_disk);
    let tau = cfg.hw_cost_tolerance;
    let k = 2.0;

    for tier in tiers {
        let mut candidates: Vec<Candidate> = Vec::with_capacity(h_set.len() * 2);
        for h in h_set {
            let factor = hw_factor_for(fit, hw, h);
            for cap in CapacityType::ALL {
                let cell = (h.clone(), cap);
                let lambda = match cap {
                    CapacityType::Spot => cost.lambda_for(h),
                    CapacityType::Od => 0.0,
                };
                // p(cap_c) > 0.5 ⇒ spot infeasible regardless of c
                // (the geometric retry tail blows every quantile).
                if cap == CapacityType::Spot {
                    let t_cap = fit.fit.t_at(RawCores(cap_c)).0 / factor;
                    if 1.0 - (-lambda * t_cap).exp() > 0.5 {
                        continue;
                    }
                }
                let c_lo = if cap == CapacityType::Spot {
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
                let c_star = (c_star.0.ceil() as u32).max(1);
                let mem = (fit.mem.at(RawCores(f64::from(c_star))).0 as f64 * hr) as u64;
                if mem > ceil.max_mem || disk > ceil.max_disk {
                    continue;
                }
                let Some(price) = cost.smallest_fitting(&cell, c_star, mem) else {
                    continue; // menu non-empty AND no type fits → reject
                };
                candidates.push(Candidate {
                    cell,
                    c_star,
                    e_cost_upper: e_cost_upper(fit, c_star, factor, lambda, price),
                    factor,
                    lambda,
                });
            }
        }
        if candidates.is_empty() {
            continue;
        }
        // Schmitt deadband: τ_enter for cells not in prev_A, τ_exit
        // (=1.3τ) for cells already in.
        let e_min = candidates
            .iter()
            .map(|c| c.e_cost_upper)
            .fold(f64::INFINITY, f64::min);
        let in_a: Vec<&Candidate> = candidates
            .iter()
            .filter(|c| {
                let thresh = if prev_a.contains(&c.cell) {
                    1.0 + 1.3 * tau
                } else {
                    1.0 + tau
                };
                c.e_cost_upper <= thresh * e_min
            })
            .collect();
        // c* = max over A — SLA-correct on the slowest h ∈ A.
        let c_star = in_a.iter().map(|c| c.c_star).max().expect("e_min ∈ A");
        let mem = (fit.mem.at(RawCores(f64::from(c_star))).0 as f64 * hr) as u64;
        // Re-filter at c*: type-fit, cost-at-c* ≤ (1+τ)E_min, and
        // capacity-ratio c*_{h,cap} ≥ c*/k. The argmax cell has
        // c*_{h,cap} = c*, e_cost(c*) = e_cost_upper, and was admitted
        // by smallest_fitting at its own c* ≤ c* — but a larger
        // shared c* MAY exceed its menu, so the argmax-survives proof
        // relies on the e_cost(c*) check using the candidate's OWN
        // factor/lambda (it does) and on the menu degrading to EMA
        // when empty. The non-empty guarantee is `debug_assert`ed.
        let cells: Vec<Cell> = in_a
            .iter()
            .filter(|c| {
                f64::from(c.c_star) >= f64::from(c_star) / k
                    && cost
                        .smallest_fitting(&c.cell, c_star, mem)
                        .is_some_and(|price| {
                            e_cost_upper(fit, c_star, c.factor, c.lambda, price)
                                <= (1.0 + tau) * e_min
                        })
            })
            .map(|c| c.cell.clone())
            .collect();
        debug_assert!(
            !cells.is_empty(),
            "argmax cell must survive re-filter (A'≠∅)"
        );
        return SolveFullResult::Feasible(SolveMemo {
            a: AdmissibleSet {
                cells,
                c_star,
                mem_bytes: mem,
                disk_bytes: disk.min(ceil.max_disk),
            },
            all_candidates: candidates,
            tier: tier.name.clone(),
        });
    }
    // All tiers infeasible → BestEffort over the full H×cap. The
    // capacity-exhausted vs ceiling-exhausted distinction (and the
    // metric emit) is the dispatch wrapper's job — this function is
    // pure for memoization.
    let cells: Vec<Cell> = h_set
        .iter()
        .flat_map(|h| CapacityType::ALL.map(|c| (h.clone(), c)))
        .collect();
    let mem = ((fit.mem.at(RawCores(cap_c)).0 as f64 * hr) as u64).min(ceil.max_mem);
    SolveFullResult::BestEffort {
        c: (cap_c.ceil() as u32).max(1),
        mem_bytes: mem,
        disk_bytes: disk.min(ceil.max_disk),
        cells,
    }
}

/// Per-key solve memo. Keyed on `(fit_hash, override_hash)`; the stored
/// `inputs_gen` is checked against the cache's atomic counter — a miss
/// means the shared inputs (HwTable / CostTable / SlaConfig) changed,
/// and the stored entry's `a.cells` becomes the Schmitt `prev_a`.
///
/// `inputs_gen` is bumped by the actor on `HwTable`/`CostTable` refresh
/// and `SlaConfig` reload (ADR-023 L616). ICE state is NOT in
/// `inputs_gen` — the read-time mask touches only intents whose cached
/// `A` intersects the masked cell.
#[derive(Debug, Default)]
pub struct SolveCache {
    inputs_gen: AtomicU64,
    entries: DashMap<(u64, u64), (u64, SolveMemo)>,
}

impl SolveCache {
    /// Bump the fleet-wide solve-input generation. Call on
    /// `HwTable`/`CostTable` refresh and `SlaConfig` reload.
    pub fn bump_inputs_gen(&self) {
        self.inputs_gen.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inputs_gen(&self) -> u64 {
        self.inputs_gen.load(Ordering::Relaxed)
    }

    /// Read the memo for `(fit_hash, override_hash)` at the current
    /// generation, or compute via `f(prev_a)` and insert. `prev_a` is
    /// the previous generation's `A'` (Schmitt history), empty on
    /// first sight.
    pub fn get_or_insert_with(
        &self,
        fit_hash: u64,
        override_hash: u64,
        f: impl FnOnce(&HashSet<Cell>) -> Option<SolveMemo>,
    ) -> Option<SolveMemo> {
        let g = self.inputs_gen();
        let key = (fit_hash, override_hash);
        // Copy out under guard (DashMap shard non-reentrant), then
        // decide: hit if stored gen matches; otherwise stored A' is
        // prev_a for the recompute.
        let prior = self.entries.get(&key).map(|e| e.clone());
        if let Some((stored_g, memo)) = &prior
            && *stored_g == g
        {
            return Some(memo.clone());
        }
        let prev_a: HashSet<Cell> = prior
            .map(|(_, m)| m.a.cells.into_iter().collect())
            .unwrap_or_default();
        let memo = f(&prev_a)?;
        self.entries.insert(key, (g, memo.clone()));
        Some(memo)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Stable hash of the solve-relevant fields of `fit`. Changes whenever
/// a refit moves the curve enough to change the solve output.
pub fn fit_hash(fit: &FittedParams) -> u64 {
    let mut h = std::hash::DefaultHasher::new();
    fit.key.pname.hash(&mut h);
    fit.key.system.hash(&mut h);
    fit.key.tenant.hash(&mut h);
    let (s, p, q) = fit.fit.spq();
    for x in [
        s,
        p,
        q,
        fit.fit.p_bar().0,
        fit.sigma_resid,
        fit.n_eff,
        fit.sum_w,
    ] {
        x.to_bits().hash(&mut h);
    }
    fit.n_distinct_c.hash(&mut h);
    fit.disk_p90.map(|d| d.0).hash(&mut h);
    match &fit.mem {
        super::types::MemFit::Coupled { a, b, .. } => {
            a.to_bits().hash(&mut h);
            b.to_bits().hash(&mut h);
        }
        super::types::MemFit::Independent { p90 } => p90.0.hash(&mut h),
    }
    for a in fit.alpha {
        a.to_bits().hash(&mut h);
    }
    h.finish()
}

/// Stable hash of the solve-relevant override fields. `None` → 0.
pub fn override_hash(o: Option<&ResolvedTarget>) -> u64 {
    let Some(o) = o else { return 0 };
    let mut h = std::hash::DefaultHasher::new();
    o.forced_cores.map(f64::to_bits).hash(&mut h);
    o.forced_mem.hash(&mut h);
    o.tier.hash(&mut h);
    h.finish()
}

/// Map `cells` to `nodeSelectorTerms` — OR of (h-label conjunction AND
/// `karpenter.sh/capacity-type=cap`). ADR-023 L650: capacity-type MUST
/// be in the term — the solve may admit `(h, od)` while rejecting
/// `(h, spot)` on interruption tail. An `h` not in `hw_classes` is
/// dropped.
pub fn cells_to_selector_terms(
    cells: &[Cell],
    hw_classes: &HashMap<HwClassName, HwClassDef>,
) -> Vec<rio_proto::types::NodeSelectorTerm> {
    use rio_proto::types::{NodeSelectorRequirement, NodeSelectorTerm};
    cells
        .iter()
        .filter_map(|(h, cap)| {
            let def = hw_classes.get(h)?;
            let mut match_expressions: Vec<NodeSelectorRequirement> = def
                .labels
                .iter()
                .map(|l| NodeSelectorRequirement {
                    key: l.key.clone(),
                    operator: "In".into(),
                    values: vec![l.value.clone()],
                })
                .collect();
            match_expressions.push(NodeSelectorRequirement {
                key: "karpenter.sh/capacity-type".into(),
                operator: "In".into(),
                values: vec![cap.label().into()],
            });
            Some(NodeSelectorTerm { match_expressions })
        })
        .collect()
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
            // Asymptotic-n so z_q → Φ⁻¹(q) and the closed-form test
            // expectations below (which use 1.2816/2.3263) hold. Tests
            // that exercise small-n widening or headroom set these
            // explicitly.
            n_eff: 1e6,
            n_distinct_c: 1_000_000,
            sum_w: 1e6,
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
            alpha: crate::sla::alpha::UNIFORM,
            prior_source: None,
            is_fod: false,
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
    // r[verify sched.sla.tier-envelope]
    #[test]
    fn envelope_non_integer_cap_c_no_feasible_integer_is_none() {
        // USL S=0, P=2000, Q=4.327 → c_opt=√(2000/4.327)≈21.50.
        // T(21)=186.105, T(21.5)=186.054, T(22)=186.103; σ clamped to
        // 1e-3 so q90≈T·1.00128. With bound 186.32: feasible(21.5) is
        // true (q=186.29), feasible(21) and feasible(22) are both
        // false (q=186.34) → no integer in [1, 21.5] satisfies the
        // bound. Before the `feasible(n)` guard this returned
        // Some(22.0) — admitting the build to a tier whose bound it
        // cannot meet.
        let fit = mk_fit(0.0, 2000.0, 4.327, f64::INFINITY, 0.0);
        assert_eq!(
            solve_envelope(&fit, &t("x", 186.32), 1.0, 21.5, 1.0, 0.0),
            None,
            "⌈cap_c⌉ > cap_c with no feasible integer → None, not Some(⌈cap_c⌉)"
        );
        // Control: at a slightly looser bound, 21 IS feasible.
        assert_eq!(
            solve_envelope(&fit, &t("x", 186.5), 1.0, 21.5, 1.0, 0.0)
                .unwrap()
                .0,
            21.0
        );
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

    // r[verify sched.sla.explore-freeze]
    #[test]
    fn intent_for_probe_fit_frozen_at_floor_routes_to_explore() {
        // n_eff≥3 ∧ span<4 ∧ frozen (via min_c<=1 wall clause): the
        // gate used to admit this Probe fit to solve_mvp → t_at=∞ →
        // cap_c=max_cores=64. The explore arm correctly returns
        // st.max_c=2 when frozen.
        let mut f = mk_fit(0.0, 0.0, 0.0, f64::INFINITY, 0.2);
        f.fit = DurationFit::Probe;
        f.n_eff = 3.0;
        f.span = 2.0;
        f.explore = ExploreState {
            distinct_c: 2,
            min_c: RawCores(1.0),
            max_c: RawCores(2.0),
            saturated: false,
            last_wall: WallSeconds(100.0),
        };
        assert!(explore::frozen(&f.explore, 64.0), "precondition: frozen");
        let (c, _, _) = intent_for(
            Some(&f),
            &DrvHints::default(),
            None,
            &cfg(),
            &[t("normal", 1200.0)],
            &ceil(),
        );
        assert_eq!(
            c, 2,
            "Probe fit (frozen, span<4) routes to explore → max_c=2, NOT max_cores=64"
        );
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
        let want = ((2u64 << 30) as f64 * headroom(fit.n_eff)) as u64;
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
        assert_eq!(m, (raw as f64 * headroom(fit.n_eff)) as u64);
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
    // ─── solve_full (admissible set, alg-estimate-full) ─────────────

    use proptest::prelude::*;

    fn hw_three() -> HwTable {
        let mut m = HashMap::new();
        m.insert("intel-6".into(), 1.0);
        m.insert("intel-7".into(), 1.4);
        m.insert("intel-8".into(), 2.0);
        HwTable::from_map(m)
    }

    fn h_set() -> Vec<HwClassName> {
        vec!["intel-6".into(), "intel-7".into(), "intel-8".into()]
    }

    fn cfg_hw() -> SlaConfig {
        let mut c = cfg();
        for h in h_set() {
            c.hw_classes.insert(
                h.clone(),
                super::super::config::HwClassDef {
                    labels: vec![super::super::config::NodeLabelMatch {
                        key: "rio.build/hw-class".into(),
                        value: h,
                    }],
                },
            );
        }
        c
    }

    // r[verify sched.sla.hw-class.admissible-set]
    #[test]
    fn admissible_set_schmitt_deadband() {
        // 3 hw_classes at factor 1.0/1.4/2.0, equal price (seed). The
        // fastest h needs the FEWEST cores to hit p90, so e_cost ∝ c·T
        // is lowest there → e_min at (intel-8, spot). Slow classes'
        // c*·T ratio determines admission against τ.
        let fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
        let cfg = cfg_hw();
        let r = solve_full(
            &fit,
            &[t("normal", 300.0)],
            &hw_three(),
            &CostTable::default(),
            &ceil(),
            &cfg,
            &h_set(),
            &HashSet::new(),
        );
        let SolveFullResult::Feasible(memo) = r else {
            panic!("Feasible expected")
        };
        // All 6 (h,cap) candidates feasible at p90=300 (intel-6 c*≈9,
        // intel-8 c*≈4); admissible set is governed by τ=0.15.
        assert_eq!(memo.all_candidates.len(), 6);
        let e_min = memo
            .all_candidates
            .iter()
            .map(|c| c.e_cost_upper)
            .fold(f64::INFINITY, f64::min);
        for cell in &memo.a.cells {
            let cand = memo
                .all_candidates
                .iter()
                .find(|c| c.cell == *cell)
                .unwrap();
            assert!(
                cand.e_cost_upper <= (1.0 + cfg.hw_cost_tolerance) * e_min + 1e-9,
                "{cell:?} e_cost {} > (1+τ)·{e_min}",
                cand.e_cost_upper
            );
        }
        // c* = max over A → SLA-correct on slowest admitted h.
        let max_c_in_a = memo
            .all_candidates
            .iter()
            .filter(|c| memo.a.cells.contains(&c.cell))
            .map(|c| c.c_star)
            .max()
            .unwrap();
        assert_eq!(memo.a.c_star, max_c_in_a);
        // The argmax cell (highest c*_{h,cap}) MUST be in A'.
        let argmax = memo
            .all_candidates
            .iter()
            .filter(|c| memo.a.cells.contains(&c.cell))
            .max_by_key(|c| c.c_star)
            .unwrap();
        assert!(memo.a.cells.contains(&argmax.cell), "argmax cell survives");

        // Schmitt: a cell at e=1.18·e_min is OUT fresh (τ=0.15) but IN
        // when prev_a contains it (τ_exit=0.195). Construct such a
        // cell via a price bump on intel-6.
        let mut cost = CostTable::default();
        cost.set_price(
            "intel-6",
            CapacityType::Spot,
            super::super::cost::ON_DEMAND_SEED * 0.35 * 1.18,
            1e9,
        );
        let prev: HashSet<Cell> = [("intel-6".into(), CapacityType::Spot)].into();
        let SolveFullResult::Feasible(m_fresh) = solve_full(
            &fit,
            &[t("normal", 300.0)],
            &hw_three(),
            &cost,
            &ceil(),
            &cfg,
            &h_set(),
            &HashSet::new(),
        ) else {
            panic!()
        };
        let SolveFullResult::Feasible(m_prev) = solve_full(
            &fit,
            &[t("normal", 300.0)],
            &hw_three(),
            &cost,
            &ceil(),
            &cfg,
            &h_set(),
            &prev,
        ) else {
            panic!()
        };
        // The deadband can only widen membership, never shrink.
        assert!(
            m_prev.a.cells.len() >= m_fresh.a.cells.len(),
            "Schmitt τ_exit ≥ τ_enter ⇒ prev_a never shrinks A"
        );
    }

    #[test]
    fn refilter_drops_capacity_ratio_violators() {
        // intel-8 (factor 2.0) needs ~half the cores of intel-6 (1.0)
        // to hit p90=80. S=5, P=2000: c6≈31, c8≈15 → 15 < 31/2=15.5.
        // Faster hw is normally e_min so the slow class is COST-dropped
        // before the cap-ratio check; compensate with prices
        // (intel-6 cheap, intel-8 dear) so BOTH are cost-admissible
        // and the cap-ratio re-filter is the binding constraint.
        let fit = mk_fit(5.0, 2000.0, 0.0, f64::INFINITY, 0.1);
        let mut cfg = cfg_hw();
        cfg.hw_cost_tolerance = 0.5;
        let mut cost = CostTable::default();
        for cap in CapacityType::ALL {
            cost.set_price("intel-6", cap, 0.009, 1e9);
            cost.set_price("intel-8", cap, 0.043, 1e9);
        }
        let hs: Vec<HwClassName> = vec!["intel-6".into(), "intel-8".into()];
        let SolveFullResult::Feasible(memo) = solve_full(
            &fit,
            &[t("tight", 80.0)],
            &hw_three(),
            &cost,
            &ceil(),
            &cfg,
            &hs,
            &HashSet::new(),
        ) else {
            panic!()
        };
        let by_h = |h: &str| {
            memo.all_candidates
                .iter()
                .find(|c| c.cell.0 == h && c.cell.1 == CapacityType::Spot)
                .map(|c| c.c_star)
        };
        let c6 = by_h("intel-6").expect("intel-6 feasible");
        let c8 = by_h("intel-8").expect("intel-8 feasible");
        assert!(
            f64::from(c8) < f64::from(c6) / 2.0,
            "fixture: c8={c8} < c6={c6}/2 so cap-ratio binds"
        );
        assert_eq!(memo.a.c_star, c6, "c* = max over cost-admissible A");
        assert!(
            !memo
                .a
                .cells
                .contains(&("intel-8".into(), CapacityType::Spot)),
            "c*_{{intel-8}}={c8} < c*/k={} → re-filter drops",
            f64::from(c6) / 2.0
        );
        assert!(
            memo.a.cells.iter().any(|c| c.0 == "intel-6"),
            "argmax (intel-6) survives"
        );
    }

    #[test]
    fn spot_infeasible_when_p_gt_half() {
        // λ huge ⇒ p(cap_c) > 0.5 for every h ⇒ spot rejected,
        // on-demand survives. Gamma-Poisson pooling means a single 1/1
        // ratio is pulled to ~seed; the data must show "many
        // interrupts over little exposure" to overwhelm the n_λ
        // prior (≈ 1e6/86400 ≈ 11.6/s).
        let fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
        let mut lambda = HashMap::new();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        for h in h_set() {
            lambda.insert(
                h,
                super::super::cost::RatioEma {
                    numerator: 1e6,
                    denominator: 1.0,
                    updated_at: now,
                },
            );
        }
        let cost = CostTable::from_parts(HashMap::new(), lambda);
        let SolveFullResult::Feasible(memo) = solve_full(
            &fit,
            &[t("normal", 1200.0)],
            &hw_three(),
            &cost,
            &ceil(),
            &cfg_hw(),
            &h_set(),
            &HashSet::new(),
        ) else {
            panic!()
        };
        assert!(
            memo.a.cells.iter().all(|(_, c)| *c == CapacityType::Od),
            "spot must be rejected when p>0.5: A={:?}",
            memo.a.cells
        );
    }

    /// Regression: `hw_factor_for` returned `dot/bias` unclamped; with
    /// a pathological factor row OR a large `hw_bias` denominator the
    /// effective factor went ≪ 0.25 → `t = T(c)/factor` blew up →
    /// `feasible(cap_c) = false` → cell dropped out.
    #[test]
    fn hw_factor_floors_after_bias_division() {
        let mut fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
        let mut m = HashMap::new();
        m.insert("h".into(), 0.3);
        let hw = HwTable::from_map(m);
        fit.hw_bias.insert("h".into(), 3.0);
        // Raw 0.3/3.0 = 0.1; must floor to 0.25.
        assert_eq!(hw_factor_for(&fit, &hw, "h"), HW_FACTOR_SANITY_FLOOR);
        // Unknown h → 1.0.
        assert_eq!(hw_factor_for(&fit, &hw, "unknown"), 1.0);
    }

    #[test]
    fn cells_to_selector_terms_or_of_ands() {
        let cfg = cfg_hw();
        let cells = vec![
            ("intel-8".into(), CapacityType::Spot),
            ("intel-7".into(), CapacityType::Od),
        ];
        let terms = cells_to_selector_terms(&cells, &cfg.hw_classes);
        assert_eq!(terms.len(), 2, "one term per cell");
        // Each term: h-label conjunction PLUS capacity-type.
        for term in &terms {
            assert!(
                term.match_expressions
                    .iter()
                    .any(|r| r.key == "karpenter.sh/capacity-type")
            );
            assert!(
                term.match_expressions
                    .iter()
                    .any(|r| r.key == "rio.build/hw-class")
            );
        }
        // capacity-type encoded per-term (spot ≠ od).
        let caps: HashSet<&str> = terms
            .iter()
            .flat_map(|t| {
                t.match_expressions
                    .iter()
                    .filter(|r| r.key == "karpenter.sh/capacity-type")
                    .flat_map(|r| r.values.iter().map(String::as_str))
            })
            .collect();
        assert_eq!(caps, HashSet::from(["spot", "on-demand"]));
        // h not in hw_classes → dropped.
        assert!(
            cells_to_selector_terms(&[("nope".into(), CapacityType::Spot)], &cfg.hw_classes)
                .is_empty()
        );
    }

    #[test]
    fn memo_key_includes_inputs_gen() {
        let fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
        let cache = SolveCache::default();
        let fh = fit_hash(&fit);
        let calls = std::cell::Cell::new(0);
        let go = |prev: &HashSet<Cell>| {
            calls.set(calls.get() + 1);
            match solve_full(
                &fit,
                &[t("normal", 1200.0)],
                &hw_three(),
                &CostTable::default(),
                &ceil(),
                &cfg_hw(),
                &h_set(),
                prev,
            ) {
                SolveFullResult::Feasible(m) => Some(m),
                _ => None,
            }
        };
        let m1 = cache.get_or_insert_with(fh, 0, go).unwrap();
        let m1b = cache.get_or_insert_with(fh, 0, go).unwrap();
        assert_eq!(calls.get(), 1, "second call hits memo");
        assert_eq!(m1.a.cells, m1b.a.cells);
        // Different override_hash → miss.
        cache.get_or_insert_with(fh, 7, go).unwrap();
        assert_eq!(calls.get(), 2);
        // bump_inputs_gen → miss; prev_a from old entry.
        cache.bump_inputs_gen();
        cache
            .get_or_insert_with(fh, 0, |p| {
                assert_eq!(p.len(), m1.a.cells.len(), "prev_a comes from old-gen entry");
                go(p)
            })
            .unwrap();
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn disk_ceiling_checked_before_explore() {
        // disk_p90 > max_disk: solve_full returns BestEffort (clamped),
        // and the per-tier disk gate rejects every tier so no
        // candidates exist. The dispatch wrapper emits DiskCeiling.
        let mut fit = mk_fit(30.0, 2000.0, 0.0, f64::INFINITY, 0.1);
        fit.disk_p90 = Some(DiskBytes(300 << 30)); // > ceil.max_disk=200Gi
        let r = solve_full(
            &fit,
            &[t("normal", 1200.0)],
            &hw_three(),
            &CostTable::default(),
            &ceil(),
            &cfg_hw(),
            &h_set(),
            &HashSet::new(),
        );
        let SolveFullResult::BestEffort { disk_bytes, .. } = r else {
            panic!("disk > ceil → BestEffort, got {r:?}")
        };
        assert_eq!(disk_bytes, 200 << 30, "clamped");
        assert_eq!(
            classify_ceiling(&fit, &[t("normal", 1200.0)], &ceil()),
            InfeasibleReason::DiskCeiling
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        // r[verify sched.sla.hw-class.admissible-set]
        /// ADR-023 L636-638: the argmax cell trivially survives all
        /// three re-filter checks ⇒ A' provably non-empty.
        #[test]
        fn argmax_cell_always_survives_refilter(
            s in 5.0f64..100.0,
            p in 200.0f64..5000.0,
            sigma in 0.05f64..0.35,
            p90 in 100.0f64..3000.0,
            tau in 0.01f64..0.5,
        ) {
            let fit = mk_fit(s, p, 0.0, f64::INFINITY, sigma);
            let mut cfg = cfg_hw();
            cfg.hw_cost_tolerance = tau;
            match solve_full(
                &fit, &[t("x", p90)], &hw_three(), &CostTable::default(),
                &ceil(), &cfg, &h_set(), &HashSet::new(),
            ) {
                SolveFullResult::Feasible(memo) => {
                    prop_assert!(!memo.a.cells.is_empty(), "A' ≠ ∅");
                    prop_assert!(
                        memo.all_candidates
                            .iter()
                            .filter(|c| memo.a.cells.contains(&c.cell))
                            .any(|c| c.c_star == memo.a.c_star),
                        "argmax cell ∈ A'"
                    );
                }
                SolveFullResult::BestEffort { .. } => {}
            }
        }
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

    #[test]
    fn intent_for_emits_serial_floor_on_all_bounded_infeasible() {
        let rec = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = rec.snapshotter();
        metrics::with_local_recorder(&rec, || {
            // T(c) ≫ 10 for all c ≤ 64: S=1e6. n_eff/span force the
            // solve branch (not explore). S alone breaches the bound
            // → classify_ceiling = SerialFloor (not the legacy
            // catch-all `ceiling_exhausted`).
            let fit = mk_fit(1e6, 0.0, 0.0, f64::INFINITY, 0.1);
            intent_for(
                Some(&fit),
                &DrvHints::default(),
                None,
                &cfg(),
                &[t("normal", 10.0)],
                &ceil(),
            );
        });
        assert_eq!(infeasible_count(&snapshotter, "serial_floor"), 1);
        assert_eq!(infeasible_count(&snapshotter, "core_ceiling"), 0);
    }

    /// `solve_mvp` is pure: emission is at the sizing decision-point
    /// (`intent_for` / `solve_full`), not inside the solve. Regression
    /// for `solve_intent_for`'s predicted-block re-calling `solve_mvp`
    /// for tier-name lookup and double-counting infeasible drvs.
    #[test]
    fn solve_mvp_is_pure() {
        let rec = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = rec.snapshotter();
        metrics::with_local_recorder(&rec, || {
            let fit = mk_fit(1e6, 0.0, 0.0, f64::INFINITY, 0.1);
            let r = solve_mvp(&fit, &[t("normal", 10.0)], &ceil());
            assert!(matches!(r, SolveResult::BestEffort { .. }));
        });
        for r in InfeasibleReason::ALL {
            assert_eq!(
                infeasible_count(&snapshotter, r.as_str()),
                0,
                "solve_mvp must not emit; callers do at the decision-point"
            );
        }
    }

    #[test]
    fn intent_for_no_emit_on_best_effort_tier() {
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
            intent_for(
                Some(&fit),
                &DrvHints::default(),
                None,
                &cfg(),
                &[unbounded],
                &ceil(),
            );
            // Empty ladder (unknown pinned-tier) → BestEffort but NOT
            // an exhaust event.
            intent_for(Some(&fit), &DrvHints::default(), None, &cfg(), &[], &ceil());
        });
        for r in InfeasibleReason::ALL {
            assert_eq!(infeasible_count(&snapshotter, r.as_str()), 0);
        }
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
