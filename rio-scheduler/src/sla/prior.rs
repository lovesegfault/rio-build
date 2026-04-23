//! ADR-023 §2.10 fleet-learned priors. A never-seen `ModelKey` gets a
//! prior in the (S, P, Q, a, b) basis (same parameters [`super::fit`]
//! produces) from one of three sources, then [`partial_pool`] blends
//! that prior with the per-key fit as samples accrue.
//!
//! Pure functions only — the caller owns the `seed` map and the fleet
//! aggregate; this module does no DB I/O.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::alpha::{UNIFORM, dot, simplex_project};
use super::config::{ProbeShape, SlaConfig};
use super::hw::{HwTable, K};
use super::types::ModelKey;

/// (S, P, Q, a, b) flat tuple. Distinct from [`super::types::FittedParams`]
/// (which carries the staged enums + explore state) because the prior
/// machinery only needs the five scalars the fitter regresses on.
///
/// All time-domain fields (`s`, `p`, `q`) are in this cluster's
/// reference-second basis (`r[sched.sla.hw-ref-seconds]`). Construct via
/// `ingest::extract_fit_params` / [`ValidatedSeedCorpus::into_seed_map`] /
/// [`operator_to_spq`]; never from wall-seconds directly. (`a`, `b` are
/// in raw bytes / dimensionless and are NOT hw-normalized.)
#[derive(Debug, Clone, PartialEq)]
pub struct FitParams {
    pub s: f64,
    pub p: f64,
    pub q: f64,
    pub a: f64,
    pub b: f64,
    /// Per-pname K=3 hardware mixture (ADR-023 §Hardware heterogeneity).
    /// Carried alongside (S,P,Q,a,b) so the same fleet-median /
    /// partial-pool / clamp machinery applies; consumers that need a
    /// simplex point re-project ([`super::alpha::simplex_project`]) after
    /// any per-component operation that can leave Σα ≠ 1.
    pub alpha: super::alpha::Alpha,
}

/// Seed-map key: `(pname, system)`. Tenant-agnostic — a seed corpus is
/// meant to be portable across deployments, and a never-seen `(pname,
/// system)` is never-seen regardless of which tenant first asks for it.
/// Contrast [`ModelKey`] (the cache key), which IS tenant-scoped.
pub type SeedKey = (String, String);

/// On-disk / on-wire corpus shape. `ref_hw_class` is the exporter's
/// [`HwTable::reference`] — the hw_class whose factor is closest to 1.0
/// in THEIR table — so an importer can rescale time-domain params via
/// its own `factor[ref_hw_class]`.
///
/// **v1/v2 compat:** v1 JSON had `{ref_hw_class, entries[{..., n}]}`.
/// v2 adds `ref_factor_vec` and per-entry `{version, alpha, n_eff}`.
/// Every v2 field is `#[serde(default)]` so v1 files still parse;
/// `n` is also `#[serde(default)]` so a v2-proto-shaped JSON (which
/// carries `n_eff` not `n`) parses too.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SeedCorpus {
    /// Exporter's full per-hw_class factor vector at export time. v2;
    /// empty on v1 import.
    #[serde(default)]
    pub ref_factor_vec: Vec<f64>,
    pub ref_hw_class: String,
    pub entries: Vec<SeedEntry>,
}

/// One `(pname, system)` row. `p_bar`/`n` are informational (round-trip
/// fidelity, operator inspection); only `(s, p, q, a, b)` feed the prior.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SeedEntry {
    pub pname: String,
    pub system: String,
    /// Package version at export time. v2; empty on v1 import.
    #[serde(default)]
    pub version: String,
    pub s: f64,
    pub p: f64,
    pub q: f64,
    pub p_bar: f64,
    pub a: f64,
    pub b: f64,
    /// v1 truncated effective-sample count. `#[serde(default)]` so a
    /// v2-proto-shaped JSON (carries `n_eff` only) still parses.
    #[serde(default)]
    pub n: u32,
    /// §13a per-hw_class residual-bias vector. v2; empty on v1 import.
    #[serde(default)]
    pub alpha: Vec<f64>,
    /// Effective sample count (the f64 the partial-pool weighting uses).
    /// v2; 0.0 on v1 import — callers fall back to `n as f64` when
    /// `n_eff == 0.0`.
    #[serde(default)]
    pub n_eff: f64,
}

impl From<&SeedCorpus> for rio_proto::types::SeedCorpus {
    fn from(c: &SeedCorpus) -> Self {
        Self {
            ref_factor_vec: c.ref_factor_vec.clone(),
            ref_hw_class: c.ref_hw_class.clone(),
            entries: c
                .entries
                .iter()
                .map(|e| rio_proto::types::SeedEntry {
                    pname: e.pname.clone(),
                    system: e.system.clone(),
                    version: e.version.clone(),
                    s: e.s,
                    p: e.p,
                    q: e.q,
                    p_bar: e.p_bar,
                    a: e.a,
                    b: e.b,
                    alpha: e.alpha.clone(),
                    // Prefer the f64 n_eff; fall back to v1 `n` so a v1
                    // corpus round-tripped through proto carries it.
                    n_eff: if e.n_eff > 0.0 {
                        e.n_eff
                    } else {
                        f64::from(e.n)
                    },
                })
                .collect(),
        }
    }
}

impl From<rio_proto::types::SeedCorpus> for SeedCorpus {
    fn from(c: rio_proto::types::SeedCorpus) -> Self {
        Self {
            ref_factor_vec: c.ref_factor_vec,
            ref_hw_class: c.ref_hw_class,
            entries: c
                .entries
                .into_iter()
                .map(|e| SeedEntry {
                    pname: e.pname,
                    system: e.system,
                    version: e.version,
                    s: e.s,
                    p: e.p,
                    q: e.q,
                    p_bar: e.p_bar,
                    a: e.a,
                    b: e.b,
                    n: e.n_eff as u32,
                    alpha: e.alpha,
                    n_eff: e.n_eff,
                })
                .collect(),
        }
    }
}

// r[impl sched.sla.threat.corpus-clamp+2]
/// Reject one entry whose params are non-finite or outside the
/// `[sla]`-derived bounds. The seed table is operator-supplied (via
/// `[sla].seedCorpus` file or `ImportSlaCorpus` RPC) and is exempt from
/// the `clamp_to_operator` band that guards the fleet aggregate, so
/// a single `s = 1e308` row would otherwise propagate verbatim into
/// [`partial_pool`] → `T(c)` → `solve_intent_for`.
///
/// Bounds are coarse — they catch adversarial / corrupt inputs, not
/// "this seed is a bit off". A legitimate seed that fails here is a
/// config bug (e.g. `max_cores` too small for the exporting cluster).
///
/// **Spec-MUST fields** (range-checked per
/// `r[sched.sla.threat.corpus-clamp]` — they feed
/// [`ValidatedSeedCorpus::into_seed_map`] → [`partial_pool`]):
/// `s`, `p`, `q`, `a`, `b`, `n_eff`, `alpha[]`.
///
/// **Round-trip-only fields** (operator inspection / export-import
/// fidelity; NOT read by `into_seed_map` — finiteness check at most,
/// never bounded by the *importer's* config): `p_bar`, `n`, `version`.
/// Bounding these by local `cfg` would reject a valid corpus exported
/// from a larger cluster (e.g. `Capped{p̄=90}` from `max_cores=128`
/// fails on import to `max_cores=64`).
pub fn validate_seed_entry(e: &SeedEntry, cfg: &SlaConfig) -> Result<(), String> {
    let bt_ref = cfg.build_timeout_ref();
    macro_rules! check {
        ($f:expr, $name:literal, $lo:expr, $hi:expr) => {
            if !$f.is_finite() || !($lo..=$hi).contains(&$f) {
                return Err(format!(
                    "{}={}: {} out of [{}, {}]",
                    e.pname, $name, $f, $lo, $hi
                ));
            }
        };
    }
    check!(e.s, "S", 0.0, bt_ref);
    check!(e.p, "P", 0.0, bt_ref);
    check!(e.q, "Q", 0.0, bt_ref);
    // p̄ is round-trip-only (see header) — finiteness only.
    if !e.p_bar.is_finite() {
        return Err(format!("{}: p̄ non-finite", e.pname));
    }
    // `a`/`b` are `MemFit::Coupled` log-log params (`ln M = a + b·ln c`),
    // NOT bytes. `a = ln M(1)` so `[0, ln(max_mem)]`; `b` is the slope
    // and may be slightly negative on mem-independent workloads where
    // the regression fits noise — |b|>2 means M ∝ c^±2, pathological.
    check!(e.a, "a", 0.0, (cfg.max_mem as f64).ln());
    check!(e.b, "b", -2.0, 2.0);
    check!(e.n_eff, "n_eff", 0.0, 32.0);
    for (d, &x) in e.alpha.iter().enumerate() {
        if !x.is_finite() || !(0.0..=1.0).contains(&x) {
            return Err(format!("{}: α[{d}]={x} out of [0, 1]", e.pname));
        }
    }
    Ok(())
}

/// Reject a corpus whose `ref_factor_vec` is out-of-band or any of
/// whose entries fail [`validate_seed_entry`]. Chokepoint: callers go
/// through [`ValidatedSeedCorpus::validate`] (the only constructor),
/// not this directly. `pub` for the existing direct-validation tests.
pub fn validate_corpus(c: &SeedCorpus, cfg: &SlaConfig) -> Result<(), String> {
    for (d, &f) in c.ref_factor_vec.iter().enumerate() {
        if !f.is_finite() || !(0.1..=10.0).contains(&f) {
            return Err(format!("ref_factor_vec[{d}]={f} out of [0.1, 10]"));
        }
    }
    c.entries
        .iter()
        .try_for_each(|e| validate_seed_entry(e, cfg))
}

/// A [`SeedCorpus`] that has passed [`validate_corpus`]. The private
/// field with single constructor [`Self::validate`] means an
/// unvalidated corpus cannot reach [`Self::into_seed_map`] /
/// `pending_seed` / `import_seed`. Both ingress paths (the
/// `[sla].seedCorpus` file at startup via [`SeedCorpus::load`], and
/// `AdminService.ImportSlaCorpus` at runtime) must construct one or
/// the code doesn't compile; a future third ingress (e.g. a streaming
/// `SyncSlaCorpus`) inherits the same guarantee.
// r[impl sched.sla.threat.corpus-clamp+2]
#[derive(Debug)]
pub struct ValidatedSeedCorpus(SeedCorpus);

impl ValidatedSeedCorpus {
    /// The only constructor. Runs [`validate_corpus`] against `cfg`.
    pub fn validate(raw: SeedCorpus, cfg: &SlaConfig) -> Result<Self, String> {
        validate_corpus(&raw, cfg)?;
        Ok(Self(raw))
    }

    /// Entry count. Accessor since the inner corpus is private.
    pub fn entries_len(&self) -> usize {
        self.0.entries.len()
    }

    /// Exporter's reference hw_class. Accessor for the RPC response.
    pub fn ref_hw_class(&self) -> &str {
        &self.0.ref_hw_class
    }

    /// Test-only bypass: wrap without validating. For tests of
    /// `into_seed_map`'s rescale logic that don't want to thread a
    /// full `SlaConfig` through.
    #[cfg(test)]
    pub(super) fn assume_valid(raw: SeedCorpus) -> Self {
        Self(raw)
    }

    /// Project to the seed map, rescaling time-domain params (S, P, Q)
    /// by `factor[self.ref_hw_class] / factor[hw.reference]`. The
    /// exporter recorded (S, P, Q) in THEIR reference-seconds; locally
    /// that hw_class has some `factor` ≠ 1.0 if it's not our reference.
    /// Q is regressed against the same hw-normalized samples as S and P
    /// (units: ref-seconds/core in the exporter's basis), so it
    /// rescales the same way. (a, b) — memory, raw bytes — are NOT
    /// rescaled.
    ///
    /// Unknown `ref_hw_class` → factor 1.0 (pass through unscaled). The
    /// returned `f64` is the applied factor, for the RPC response.
    /// Per-entry α (proto field 10, v2): re-projected onto Δ² since the
    /// wire value is untrusted; v1 / empty → [`UNIFORM`]. α is NOT
    /// rescaled — it's a mixture weight, dimensionless across fleets.
    pub fn into_seed_map(self, hw: &HwTable) -> (HashMap<SeedKey, FitParams>, f64) {
        let f = |h: &str| hw.factor(h).map_or(1.0, |v| dot(UNIFORM, v));
        let scale = f(&self.0.ref_hw_class) / f(&hw.reference);
        let map = self
            .0
            .entries
            .into_iter()
            .map(|e| {
                let alpha =
                    <[f64; K]>::try_from(e.alpha.as_slice()).map_or(UNIFORM, simplex_project);
                (
                    (e.pname, e.system),
                    FitParams {
                        s: e.s * scale,
                        p: e.p * scale,
                        q: e.q * scale,
                        a: e.a,
                        b: e.b,
                        alpha,
                    },
                )
            })
            .collect();
        (map, scale)
    }
}

impl SeedCorpus {
    /// Read + parse + validate from `path`. Called once at startup
    /// from [`super::SlaEstimator::new`] when `[sla].seed_corpus` is
    /// set. Returns a [`ValidatedSeedCorpus`] so the load path cannot
    /// bypass [`validate_corpus`].
    pub fn load(path: &Path, cfg: &SlaConfig) -> anyhow::Result<ValidatedSeedCorpus> {
        let bytes = std::fs::read(path)
            .map_err(|e| anyhow::anyhow!("read seed_corpus {}: {e}", path.display()))?;
        let raw: Self = serde_json::from_slice(&bytes)
            .map_err(|e| anyhow::anyhow!("parse seed_corpus {}: {e}", path.display()))?;
        ValidatedSeedCorpus::validate(raw, cfg)
            .map_err(|e| anyhow::anyhow!("validate seed_corpus {}: {e}", path.display()))
    }
}

/// Provenance of a prior, surfaced in `SizingExplain` so an operator
/// can tell "this came from your seed table" from "this is the fleet
/// median" from "this is just the cold-start probe rephrased".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriorSource {
    Seed,
    Fleet,
    Operator,
}

impl PriorSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Seed => "seed",
            Self::Fleet => "fleet",
            Self::Operator => "operator",
        }
    }
}

/// Inputs to [`prior_for`]. `seed` is the operator-curated per-key
/// table; `fleet` is the cross-tenant aggregate ([`fleet_median`]
/// returns `None` until enough keys have a Coupled fit).
#[derive(Debug, Clone)]
pub struct PriorSources {
    pub seed: HashMap<SeedKey, FitParams>,
    pub fleet: Option<FitParams>,
    pub operator: ProbeShape,
    /// [`Tier::binding_bound`](super::solve::Tier::binding_bound) of
    /// `[sla].default_tier`, in **reference-seconds** (converted from
    /// the wall-second config value × `factor[hw.reference]` each
    /// refresh by [`super::SlaEstimator`]). Feeds [`operator_to_spq`]'s
    /// "a build the operator expects to take `target` on `probe.cpu`
    /// cores".
    pub default_tier_target: f64,
}

// r[impl sched.sla.prior-partial-pool]
/// Precedence: explicit seed → fleet median (clamped to a [0.5×, 2×]
/// band around the operator probe) → operator probe rephrased into the
/// (S,P,Q,a,b) basis. Seed is exact-`(pname, system)` only — no prefix
/// match, no tenant scope; an operator who wants "all `ghc-*` get this"
/// writes an override, not a seed.
pub fn prior_for(key: &ModelKey, src: &PriorSources) -> (FitParams, PriorSource) {
    if let Some(s) = src.seed.get(&(key.pname.clone(), key.system.clone())) {
        return (s.clone(), PriorSource::Seed);
    }
    if let Some(f) = &src.fleet {
        return (
            clamp_to_operator(f, &src.operator, src.default_tier_target),
            PriorSource::Fleet,
        );
    }
    (
        operator_to_spq(&src.operator, src.default_tier_target),
        PriorSource::Operator,
    )
}

/// Map the operator's linear probe `{cpu, mem_per_core, mem_base}` into
/// the (S, P, Q, a, b) basis. ADR §2.10: a build that the operator
/// expects to take `tier_target` on `probe.cpu` cores, half-serial
/// half-parallel, with `M(c) = mem_base + c·mem_per_core`.
///
/// `b = 1` (linear M(c) in log-log is slope 1 only at one point, but
/// b=1 is the closest power-law to "linear in c"); `a = ln(M(1))`;
/// `P = (target/2)·cpu` (the parallel half scaled to 1 core);
/// `S = target/2`; `Q = 0`. Then `T(probe.cpu) = S + P/cpu = target`.
pub fn operator_to_spq(probe: &ProbeShape, tier_target: f64) -> FitParams {
    let half = tier_target / 2.0;
    FitParams {
        s: half,
        p: half * probe.cpu,
        q: 0.0,
        a: ((probe.mem_base + probe.mem_per_core) as f64).ln(),
        b: 1.0,
        // Operator probe has no per-pname mixture; UNIFORM is the ADR
        // L547 last-resort fallback and the basis `clamp_to_operator`
        // bands fleet-α against.
        alpha: UNIFORM,
    }
}

/// Shrinkage blend: `w·θ_pname + (1−w)·θ_prior` with `w = n_eff /
/// (n_eff + n0)`. At `n_eff = n0` the per-key fit and the prior weigh
/// equally; at `n_eff = 0` the result is pure prior. ADR's `n0 = 3`
/// means three effective samples already dominate the prior (w = 0.5).
pub fn partial_pool(
    theta_pname: &FitParams,
    n_eff: f64,
    theta_prior: &FitParams,
    n0: f64,
) -> FitParams {
    let w = n_eff / (n_eff + n0);
    let blend = |a: f64, b: f64| w * a + (1.0 - w) * b;
    FitParams {
        s: blend(theta_pname.s, theta_prior.s),
        p: blend(theta_pname.p, theta_prior.p),
        q: blend(theta_pname.q, theta_prior.q),
        a: blend(theta_pname.a, theta_prior.a),
        b: blend(theta_pname.b, theta_prior.b),
        // Convex combination of two simplex points stays on the simplex.
        alpha: std::array::from_fn(|d| blend(theta_pname.alpha[d], theta_prior.alpha[d])),
    }
}

/// Clamp each fleet parameter to `[0.5×, 2×]` of the operator-derived
/// value. A fleet median that drifts more than 2× from the operator's
/// stated probe is suspicious (one noisy tenant dominating, or the
/// operator's probe is stale) — either way the operator's number wins
/// at the band edge. Parameters whose operator value is zero (Q) are
/// passed through unclamped: a [0.5×0, 2×0] band is degenerate.
fn clamp_to_operator(fleet: &FitParams, op: &ProbeShape, tier_target: f64) -> FitParams {
    let basis = operator_to_spq(op, tier_target);
    // ADR L547: per-component [0.5×, 2×] on α against the operator basis
    // (= UNIFORM). Per-component clamp can leave Σα ≠ 1; re-project so
    // the prior `als_fit` receives is on the simplex. The divergence
    // gauge is reported per-dimension as `alpha_d` so a fleet drifting
    // toward, say, ioseq-dominant is observable.
    const DIM: [&str; K] = ["alpha_alu", "alpha_membw", "alpha_ioseq"];
    let alpha = simplex_project(std::array::from_fn(|d| {
        clamp_field(fleet.alpha[d], basis.alpha[d], DIM[d], false)
    }));
    FitParams {
        s: clamp_field(fleet.s, basis.s, "s", false),
        p: clamp_field(fleet.p, basis.p, "p", false),
        q: clamp_field(fleet.q, basis.q, "q", false),
        a: clamp_field(fleet.a, basis.a, "a", true),
        b: clamp_field(fleet.b, basis.b, "b", false),
        alpha,
    }
}

/// Median-of-tenant-medians over the caller's fitted-key set. One vote
/// per tenant regardless of how many `ModelKey`s that tenant has fit —
/// a tenant with 10k builds of one package can't drag the fleet prior
/// (Sybil-resistant in the "noisy neighbour" sense, not the crypto
/// sense). Returns `None` below `min_keys` (empty/cold fleet) OR below
/// `min_tenants` (a "median" of one tenant's median is just that
/// tenant) so both cases fall through to [`PriorSource::Operator`].
///
/// Caller is expected to pre-filter to keys whose `MemFit` is
/// [`Coupled`](super::types::MemFit::Coupled) — `FitParams.{a,b}` are
/// only meaningful for those.
pub fn fleet_median(
    all: &[(ModelKey, FitParams)],
    min_keys: usize,
    min_tenants: usize,
) -> Option<FitParams> {
    if all.len() < min_keys {
        return None;
    }
    let mut by_tenant: HashMap<&str, Vec<&FitParams>> = HashMap::new();
    for (k, f) in all {
        by_tenant.entry(k.tenant.as_str()).or_default().push(f);
    }
    if by_tenant.len() < min_tenants {
        return None;
    }
    let tenant_medians: Vec<FitParams> = by_tenant.values().map(|v| median_fitparams(v)).collect();
    Some(median_fitparams(&tenant_medians.iter().collect::<Vec<_>>()))
}

/// Per-field median. Even-length → mean of the two straddling elements
/// (the five fields are independent so there's no "the median row").
fn median_fitparams(v: &[&FitParams]) -> FitParams {
    debug_assert!(!v.is_empty());
    fn med(v: &[&FitParams], proj: impl Fn(&FitParams) -> f64) -> f64 {
        let mut xs: Vec<f64> = v.iter().map(|f| proj(f)).collect();
        xs.sort_by(f64::total_cmp);
        let n = xs.len();
        if n % 2 == 1 {
            xs[n / 2]
        } else {
            (xs[n / 2 - 1] + xs[n / 2]) / 2.0
        }
    }
    FitParams {
        s: med(v, |f| f.s),
        p: med(v, |f| f.p),
        q: med(v, |f| f.q),
        a: med(v, |f| f.a),
        b: med(v, |f| f.b),
        // Per-dimension marginal median (the geometric median on Δ² has
        // no closed form and the marginal is what the (S,P,Q,a,b) median
        // already is). Re-project: marginal median of simplex points is
        // not guaranteed Σ=1 (e.g. med{[1,0,0],[0,1,0],[0,0,1]}=[0;3]).
        alpha: simplex_project(std::array::from_fn(|d| med(v, |f| f.alpha[d]))),
    }
}

/// `log_domain`: `a = ln(M(1))` is in log-space, so a "2× drift" guard
/// is additive `[op − ln 2, op + ln 2]` (= `[M_op/2, 2·M_op]` in bytes),
/// not multiplicative `[0.5·op, 2·op]` (= `[√M_op, M_op²]`). The
/// divergence ratio is reported as `exp(fleet − op)` so the alert
/// threshold (`> 2 ∨ < 0.5`) keeps its meaning across both domains.
fn clamp_field(fleet: f64, op: f64, param: &'static str, log_domain: bool) -> f64 {
    let g = ::metrics::gauge!("rio_scheduler_sla_prior_divergence", "param" => param);
    if op.abs() < f64::EPSILON {
        // Degenerate band (operator basis 0, e.g. q): pass fleet
        // through and report "in band" so a previously-set value for
        // this param doesn't stick.
        g.set(1.0);
        return fleet;
    }
    let (lo, hi, ratio) = if log_domain {
        let ln2 = std::f64::consts::LN_2;
        (op - ln2, op + ln2, (fleet - op).exp())
    } else if op > 0.0 {
        (0.5 * op, 2.0 * op, fleet / op)
    } else {
        (2.0 * op, 0.5 * op, fleet / op)
    };
    // Unconditional: in-band → ratio ∈ [0.5, 2.0] and the alert
    // (`> 2 ∨ < 0.5`) auto-clears when fleet converges. Gating on
    // `clamped != fleet` left the last out-of-band ratio stale forever.
    g.set(ratio);
    fleet.clamp(lo, hi)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn probe() -> ProbeShape {
        ProbeShape {
            cpu: 4.0,
            mem_per_core: 2 * GIB,
            mem_base: 4 * GIB,
            deadline_secs: 3600,
        }
    }

    fn key(pname: &str) -> ModelKey {
        ModelKey {
            pname: pname.into(),
            system: "x86_64-linux".into(),
            tenant: "t0".into(),
        }
    }

    // r[verify sched.sla.prior-partial-pool]
    #[test]
    fn partial_pool_at_n0_is_mostly_prior() {
        let theta = FitParams {
            s: 100.0,
            p: 1000.0,
            q: 0.1,
            a: 20.0,
            b: 1.5,
            alpha: UNIFORM,
        };
        let prior = FitParams {
            s: 0.0,
            p: 0.0,
            q: 0.0,
            a: 0.0,
            b: 0.0,
            alpha: UNIFORM,
        };
        // n_eff=1, n0=3 → w=0.25 → 0.75·prior + 0.25·θ
        let pooled = partial_pool(&theta, 1.0, &prior, 3.0);
        assert!((pooled.s - 25.0).abs() < 1e-9);
        assert!((pooled.p - 250.0).abs() < 1e-9);
        assert!((pooled.q - 0.025).abs() < 1e-9);
        assert!((pooled.a - 5.0).abs() < 1e-9);
        assert!((pooled.b - 0.375).abs() < 1e-9);
        // n_eff=n0 → exact midpoint
        let mid = partial_pool(&theta, 3.0, &prior, 3.0);
        assert!((mid.p - 500.0).abs() < 1e-9);
    }

    #[test]
    fn operator_probe_maps_to_spq_basis() {
        let fp = operator_to_spq(&probe(), 300.0);
        // half=150; P=150·4=600; S=150
        assert!((fp.p - 600.0).abs() < 1e-9);
        assert!((fp.s - 150.0).abs() < 1e-9);
        // Invariant: T(probe.cpu) = S + P/cpu = p90.
        assert!((fp.s + fp.p / 4.0 - 300.0).abs() < 1e-9);
        assert_eq!(fp.q, 0.0);
        assert_eq!(fp.b, 1.0);
        assert!((fp.a - ((6 * GIB) as f64).ln()).abs() < 1e-9);
    }

    #[test]
    fn prior_precedence_seed_beats_fleet() {
        let seeded = FitParams {
            s: 1.0,
            p: 2.0,
            q: 3.0,
            a: 4.0,
            b: 5.0,
            alpha: UNIFORM,
        };
        let fleet = FitParams {
            s: 10.0,
            p: 20.0,
            q: 30.0,
            a: 40.0,
            b: 50.0,
            alpha: UNIFORM,
        };
        let mut seed = HashMap::new();
        seed.insert(("hello".into(), "x86_64-linux".into()), seeded.clone());
        let src = PriorSources {
            seed,
            fleet: Some(fleet.clone()),
            operator: probe(),
            default_tier_target: 300.0,
        };
        // exact-(pname,system) seed wins — tenant-agnostic, so a t1 key
        // hits the same seed.
        let (fp, prov) = prior_for(&key("hello"), &src);
        assert_eq!(prov, PriorSource::Seed);
        assert_eq!(fp, seeded);
        let (_, prov) = prior_for(&tkey("t1", "hello"), &src);
        assert_eq!(prov, PriorSource::Seed);
        // miss → fleet (clamped)
        let (fp, prov) = prior_for(&key("world"), &src);
        assert_eq!(prov, PriorSource::Fleet);
        assert_ne!(fp, fleet); // clamped, since fleet.b=50 ≫ 2×1.0
        // no fleet → operator
        let src2 = PriorSources {
            seed: HashMap::new(),
            fleet: None,
            operator: probe(),
            default_tier_target: 300.0,
        };
        let (fp, prov) = prior_for(&key("world"), &src2);
        assert_eq!(prov, PriorSource::Operator);
        assert_eq!(fp, operator_to_spq(&probe(), 300.0));
    }

    #[test]
    fn clamp_to_operator_band() {
        let basis = operator_to_spq(&probe(), 300.0); // s=150, p=600, q=0, a=ln(6Gi), b=1
        // p=2000 > 2×600 → clamps to 1200; b=0.2 < 0.5×1 → clamps to 0.5
        let fleet = FitParams {
            s: 100.0,
            p: 2000.0,
            q: 0.3,
            a: basis.a,
            b: 0.2,
            alpha: UNIFORM,
        };
        let clamped = clamp_to_operator(&fleet, &probe(), 300.0);
        assert!((clamped.p - 1200.0).abs() < 1e-9);
        assert!((clamped.b - 0.5).abs() < 1e-9);
        // s=100 is within [75, 300] → unchanged
        assert!((clamped.s - 100.0).abs() < 1e-9);
        // q: operator basis is 0 → passed through
        assert!((clamped.q - 0.3).abs() < 1e-9);
    }

    // r[verify sched.sla.prior-partial-pool]
    #[test]
    fn prior_divergence_gauge_resets_in_band() {
        use rio_test_support::metrics::CountingRecorder;
        let rec = CountingRecorder::default();
        let key = "rio_scheduler_sla_prior_divergence{param=p}";
        // Out-of-band: 10/3 ≈ 3.33 > 2.
        ::metrics::with_local_recorder(&rec, || {
            clamp_field(10.0, 3.0, "p", false);
        });
        let v = rec.gauge_value(key).expect("set out-of-band");
        assert!((v - 10.0 / 3.0).abs() < 1e-9, "got {v}");
        // In-band: 3.3/3 = 1.1. Gauge must follow, not stick at 3.33.
        ::metrics::with_local_recorder(&rec, || {
            clamp_field(3.3, 3.0, "p", false);
        });
        let v = rec.gauge_value(key).expect("still set");
        assert!(
            (v - 1.1).abs() < 1e-9,
            "in-band refit resets gauge to current ratio (got {v}, not stale 3.33)"
        );
        // Degenerate band (op≈0) → reports 1.0, not stale.
        ::metrics::with_local_recorder(&rec, || {
            clamp_field(10.0, 0.0, "p", false);
        });
        let v = rec.gauge_value(key).unwrap();
        assert!((v - 1.0).abs() < 1e-9, "op≈0 clears to 1.0 (got {v})");
    }

    fn fp(s: f64, p: f64, q: f64, a: f64, b: f64) -> FitParams {
        FitParams {
            s,
            p,
            q,
            a,
            b,
            alpha: UNIFORM,
        }
    }

    fn fpa(s: f64, alpha: super::super::alpha::Alpha) -> FitParams {
        FitParams {
            alpha,
            ..fp(s, 0.0, 0.0, 0.0, 0.0)
        }
    }

    // r[verify sched.sla.hw-class.alpha-als]
    /// ADR L547: fleet-median α is per-dimension marginal median across
    /// fitted pnames (same machinery as the M(c) prior), re-projected
    /// onto Δ². Median-of-tenant-medians: 2 tenants × 3 keys each;
    /// fleet α should land at the cross-tenant per-dim median of the
    /// per-tenant per-dim medians.
    #[test]
    fn fleet_median_alpha_is_per_dim_median() {
        // t0's 3 α: [.9,.05,.05] [.7,.2,.1] [.5,.3,.2] → per-dim med [.7,.2,.1]
        // t1's 3 α: [.1,.1,.8]   [.2,.3,.5] [.3,.4,.3] → per-dim med [.2,.3,.5]
        // cross-tenant per-dim med: mean of {.7,.2}, {.2,.3}, {.1,.5}
        //   = [.45, .25, .3] → Σ=1.0 already on simplex.
        let mut all = Vec::new();
        for (t, a) in [
            ("t0", [0.9, 0.05, 0.05]),
            ("t0", [0.7, 0.2, 0.1]),
            ("t0", [0.5, 0.3, 0.2]),
            ("t1", [0.1, 0.1, 0.8]),
            ("t1", [0.2, 0.3, 0.5]),
            ("t1", [0.3, 0.4, 0.3]),
        ] {
            all.push((tkey(t, "p"), fpa(1.0, a)));
        }
        let m = fleet_median(&all, 2, 2).expect("6 keys ≥ 2, 2 tenants ≥ 2");
        let want = [0.45, 0.25, 0.3];
        for (d, w) in want.iter().enumerate() {
            assert!(
                (m.alpha[d] - w).abs() < 1e-9,
                "α[{d}]={} want {w}",
                m.alpha[d]
            );
        }
        assert!((m.alpha.iter().sum::<f64>() - 1.0).abs() < 1e-9);
        // Per-component clamp around UNIFORM=[⅓;3]: band [⅙, ⅔].
        // [.45,.25,.3] all inside → unchanged after clamp+reproject.
        let clamped = clamp_to_operator(&m, &probe(), 600.0);
        for (d, w) in want.iter().enumerate() {
            assert!((clamped.alpha[d] - w).abs() < 1e-9);
        }
    }

    /// Clamp pulls a vertex-heavy fleet α (e.g. fleet is all-ioseq
    /// pnames) toward the UNIFORM band, then re-projects.
    #[test]
    fn fleet_alpha_clamped_to_uniform_band_then_reprojected() {
        let f = FitParams {
            alpha: [0.05, 0.05, 0.9],
            ..fp(1.0, 1.0, 0.0, 1.0, 1.0)
        };
        let c = clamp_to_operator(&f, &probe(), 600.0);
        // raw clamp: [.05→⅙, .05→⅙, .9→⅔] = [⅙,⅙,⅔], Σ=1.0 → projection
        // is identity. ioseq capped at ⅔ (the band edge).
        assert!((c.alpha[2] - 2.0 / 3.0).abs() < 1e-9, "{:?}", c.alpha);
        assert!((c.alpha.iter().sum::<f64>() - 1.0).abs() < 1e-9);
        assert!(c.alpha.iter().all(|&x| x >= 0.0));
    }

    /// v2 SeedEntry α round-trips through `into_seed_map`; v1 (empty) and
    /// malformed (wrong length) default to UNIFORM.
    #[test]
    fn seed_entry_alpha_projected_or_uniform() {
        let corpus = SeedCorpus {
            ref_hw_class: "ref".into(),
            ref_factor_vec: vec![],
            entries: vec![
                SeedEntry {
                    pname: "v2".into(),
                    alpha: vec![0.5, 0.6, 0.2], // Σ=1.3 → projected
                    ..Default::default()
                },
                SeedEntry {
                    pname: "v1".into(),
                    alpha: vec![], // → UNIFORM
                    ..Default::default()
                },
                SeedEntry {
                    pname: "bad".into(),
                    alpha: vec![1.0], // len≠K → UNIFORM
                    ..Default::default()
                },
            ],
        };
        let (map, _) = ValidatedSeedCorpus::assume_valid(corpus).into_seed_map(&HwTable::default());
        let v2 = &map[&("v2".into(), String::new())];
        assert!((v2.alpha.iter().sum::<f64>() - 1.0).abs() < 1e-9);
        assert!(v2.alpha[0] > v2.alpha[2], "rank order survives projection");
        assert_eq!(map[&("v1".into(), String::new())].alpha, UNIFORM);
        assert_eq!(map[&("bad".into(), String::new())].alpha, UNIFORM);
    }

    fn tkey(tenant: &str, pname: &str) -> ModelKey {
        ModelKey {
            pname: pname.into(),
            system: "x86_64-linux".into(),
            tenant: tenant.into(),
        }
    }

    #[test]
    fn fleet_median_is_median_of_tenant_medians() {
        // 60 keys across 3 tenants: t0 has 40 (all p=100), t1 has 10
        // (all p=500), t2 has 10 (all p=900). A flat median over 60
        // would give p=100 (t0 dominates); median-of-tenant-medians
        // gives median{100, 500, 900} = 500.
        let mut all = Vec::new();
        for i in 0..40 {
            all.push((
                tkey("t0", &format!("a{i}")),
                fp(10.0, 100.0, 0.0, 20.0, 1.0),
            ));
        }
        for i in 0..10 {
            all.push((
                tkey("t1", &format!("b{i}")),
                fp(50.0, 500.0, 0.0, 22.0, 1.0),
            ));
        }
        for i in 0..10 {
            all.push((
                tkey("t2", &format!("c{i}")),
                fp(90.0, 900.0, 0.0, 24.0, 1.0),
            ));
        }
        assert_eq!(all.len(), 60);
        let m = fleet_median(&all, 20, 2).expect("60 ≥ 20, 3 tenants ≥ 2");
        assert!((m.p - 500.0).abs() < 1e-9, "got {}", m.p);
        assert!((m.s - 50.0).abs() < 1e-9);
        assert!((m.a - 22.0).abs() < 1e-9);
    }

    #[test]
    fn fleet_median_none_below_min_keys() {
        let all = vec![
            (tkey("t0", "a"), fp(1.0, 1.0, 0.0, 1.0, 1.0)),
            (tkey("t1", "b"), fp(2.0, 2.0, 0.0, 2.0, 1.0)),
        ];
        assert!(fleet_median(&all, 20, 1).is_none());
        assert!(fleet_median(&all, 2, 1).is_some());
    }

    #[test]
    fn fleet_median_none_below_min_tenants() {
        // 60 keys all from one tenant: passes min_keys=50, but a
        // "median-of-tenant-medians" of one tenant is just that tenant's
        // median — must fall through to PriorSource::Operator.
        let all: Vec<_> = (0..60)
            .map(|i| {
                (
                    tkey("t0", &format!("p{i}")),
                    fp(10.0, 100.0, 0.0, 20.0, 1.0),
                )
            })
            .collect();
        assert!(fleet_median(&all, 50, 2).is_none(), "1 tenant < 2");
        // Adding one key from a second tenant crosses the threshold.
        let mut all = all;
        all.push((tkey("t1", "x"), fp(50.0, 500.0, 0.0, 22.0, 1.0)));
        assert!(fleet_median(&all, 50, 2).is_some(), "2 tenants ≥ 2");
    }

    #[test]
    fn corpus_roundtrip_rescales_time_domain() {
        // Exporter recorded on ref="aws-6-ebs". Our hw table says that
        // class is 1.5× our reference → S/P/Q scale by 1.5; a/b don't.
        let corpus = SeedCorpus {
            ref_hw_class: "aws-6-ebs".into(),
            entries: vec![SeedEntry {
                pname: "hello".into(),
                system: "x86_64-linux".into(),
                s: 10.0,
                p: 100.0,
                q: 0.02,
                p_bar: 8.0,
                a: 22.0,
                b: 0.6,
                n: 12,
                ..Default::default()
            }],
            ..Default::default()
        };
        let json = serde_json::to_string(&corpus).unwrap();
        let parsed: SeedCorpus = serde_json::from_str(&json).unwrap();
        let mut hw = HashMap::new();
        hw.insert("aws-5-ebs".into(), 1.0); // local reference
        hw.insert("aws-6-ebs".into(), 1.5);
        let (map, scale) =
            ValidatedSeedCorpus::assume_valid(parsed).into_seed_map(&HwTable::from_map(hw));
        assert!((scale - 1.5).abs() < 1e-9);
        let fp = &map[&("hello".into(), "x86_64-linux".into())];
        assert!((fp.s - 15.0).abs() < 1e-9);
        assert!((fp.p - 150.0).abs() < 1e-9);
        assert!(
            (fp.q - 0.03).abs() < 1e-9,
            "q rescaled (ref-sec/core in exporter's basis)"
        );
        assert!((fp.a - 22.0).abs() < 1e-9, "a (mem) not rescaled");
    }

    #[test]
    fn corpus_unknown_ref_passes_through() {
        let corpus = SeedCorpus {
            ref_hw_class: "mystery-hw".into(),
            entries: vec![SeedEntry {
                pname: "x".into(),
                system: "x86_64-linux".into(),
                s: 5.0,
                p: 50.0,
                q: 0.0,
                p_bar: 4.0,
                a: 20.0,
                b: 1.0,
                n: 3,
                ..Default::default()
            }],
            ..Default::default()
        };
        let (map, scale) =
            ValidatedSeedCorpus::assume_valid(corpus).into_seed_map(&HwTable::default());
        assert_eq!(scale, 1.0);
        assert_eq!(map[&("x".into(), "x86_64-linux".into())].s, 5.0);
    }

    /// v1 JSON (no `ref_factor_vec`/`version`/`alpha`/`n_eff`) still
    /// parses via `#[serde(default)]`. v2-proto JSON (no `n`) parses
    /// with `n=0`. The proto round-trip carries `n_eff` (preferred) and
    /// derives `n` from it.
    #[test]
    fn corpus_v1_v2_compat() {
        // v1 on-disk shape (what existing `[sla].seed_corpus` files have).
        let v1 = r#"{"ref_hw_class":"aws-5-ebs","entries":[
            {"pname":"x","system":"y","s":1,"p":2,"q":0,"p_bar":4,"a":5,"b":6,"n":7}
        ]}"#;
        let parsed: SeedCorpus = serde_json::from_str(v1).expect("v1 parses");
        let e = &parsed.entries[0];
        assert_eq!(e.n, 7);
        assert_eq!(e.n_eff, 0.0, "v1 had no n_eff");
        assert!(e.alpha.is_empty());
        assert!(e.version.is_empty());
        assert!(parsed.ref_factor_vec.is_empty());
        // v1 → proto: n_eff falls back to n.
        let proto = rio_proto::types::SeedCorpus::from(&parsed);
        assert_eq!(proto.entries[0].n_eff, 7.0);
        // proto → Rust: n derived from n_eff.
        let back = SeedCorpus::from(proto);
        assert_eq!(back.entries[0].n, 7);
        assert_eq!(back.entries[0].n_eff, 7.0);

        // v2-proto-shaped JSON (no `n`, has `n_eff`).
        let v2 = r#"{"ref_factor_vec":[1.0,1.5],"ref_hw_class":"r","entries":[
            {"pname":"x","system":"y","version":"1.2","s":1,"p":2,"q":0,
             "p_bar":4,"a":5,"b":6,"alpha":[0.1],"n_eff":12.5}
        ]}"#;
        let parsed: SeedCorpus = serde_json::from_str(v2).expect("v2 parses");
        assert_eq!(parsed.entries[0].n, 0);
        assert_eq!(parsed.entries[0].n_eff, 12.5);
        assert_eq!(parsed.entries[0].alpha, vec![0.1]);
        assert_eq!(parsed.ref_factor_vec, vec![1.0, 1.5]);
    }

    #[test]
    fn fleet_median_per_field_median_even() {
        // Two tenants, one key each → tenant_medians has 2 entries →
        // even-length per-field median = mean.
        let all = vec![
            (tkey("t0", "a"), fp(10.0, 100.0, 0.0, 20.0, 0.8)),
            (tkey("t1", "b"), fp(30.0, 300.0, 0.2, 24.0, 1.2)),
        ];
        let m = fleet_median(&all, 2, 2).unwrap();
        assert!((m.s - 20.0).abs() < 1e-9);
        assert!((m.p - 200.0).abs() < 1e-9);
        assert!((m.q - 0.1).abs() < 1e-9);
        assert!((m.b - 1.0).abs() < 1e-9);
    }

    #[test]
    fn fleet_prior_clamped_and_divergence_path() {
        // Fleet says p=5000; operator basis at p90=300, cpu=4 gives
        // p=600 → clamps to 1200. The divergence gauge is emitted as a
        // side effect (not asserted here — no recorder); we assert the
        // clamp itself, which is the gate that triggers emission.
        let a_op = ((6 * GIB) as f64).ln();
        let a_13gi = ((13 * GIB) as f64).ln();
        let src = PriorSources {
            seed: HashMap::new(),
            fleet: Some(fp(150.0, 5000.0, 0.0, a_13gi, 1.0)),
            operator: probe(),
            default_tier_target: 300.0,
        };
        let (got, prov) = prior_for(&key("anything"), &src);
        assert_eq!(prov, PriorSource::Fleet);
        assert!((got.p - 1200.0).abs() < 1e-9, "p clamped to 2×600");
        // a: log-domain band is [ln(6Gi)−ln2, ln(6Gi)+ln2] = [ln(3Gi),
        // ln(12Gi)]. ln(13Gi) is above → clamps to ln(12Gi). The old
        // multiplicative band [11.3, 45.2] (= [√M_op, M_op²] in bytes)
        // would have passed it through.
        assert!(
            (got.a - (a_op + std::f64::consts::LN_2)).abs() < 1e-9,
            "a clamped to ln(12Gi) (got {})",
            got.a
        );
    }

    // r[verify sched.sla.prior-partial-pool]
    #[test]
    fn operator_probe_invariant_t_at_cpu_equals_p90() {
        for cpu in [1.0, 2.0, 4.0, 16.0] {
            let p = ProbeShape { cpu, ..probe() };
            let fp = operator_to_spq(&p, 300.0);
            assert!(
                (fp.s + fp.p / cpu - 300.0).abs() < 1e-9,
                "cpu={cpu}: T(cpu)={} ≠ 300",
                fp.s + fp.p / cpu
            );
        }
    }

    #[test]
    fn clamp_field_log_domain_a_is_additive() {
        let a_op = ((6 * GIB) as f64).ln();
        // 1 TiB ≫ 12 GiB → clamps to op + ln2 (= ln(12 GiB)).
        let got = clamp_field(((1_u64 << 40) as f64).ln(), a_op, "a", true);
        assert!(
            (got - (a_op + std::f64::consts::LN_2)).abs() < 1e-9,
            "1 TiB clamps to ln(12Gi); got {got}"
        );
        // 4 GiB ∈ [3Gi, 12Gi] → unchanged.
        let in_band = ((4 * GIB) as f64).ln();
        assert!((clamp_field(in_band, a_op, "a", true) - in_band).abs() < 1e-9);
    }

    #[test]
    fn clamp_field_s_not_bypassed_at_cpu_1() {
        // probe.cpu=1 → basis.s = p90/2 = 150 (was 0, which tripped the
        // op≈0 bypass and left fleet.s unclamped).
        let p1 = ProbeShape {
            cpu: 1.0,
            ..probe()
        };
        let basis = operator_to_spq(&p1, 300.0);
        assert!((basis.s - 150.0).abs() < 1e-9);
        let clamped = clamp_to_operator(&fp(500.0, 100.0, 0.0, basis.a, 1.0), &p1, 300.0);
        assert!(
            (clamped.s - 300.0).abs() < 1e-9,
            "s=500 clamps to 2×150=300; got {}",
            clamped.s
        );
    }

    fn vcfg() -> SlaConfig {
        SlaConfig {
            max_cores: 64.0,
            max_mem: 256 * GIB,
            ..SlaConfig::test_default()
        }
    }

    fn ok_entry() -> SeedEntry {
        SeedEntry {
            pname: "hello".into(),
            system: "x86_64-linux".into(),
            version: "2.12".into(),
            s: 30.0,
            p: 600.0,
            q: 0.01,
            p_bar: 8.0,
            a: 22.0,
            b: 0.5,
            n: 5,
            n_eff: 5.0,
            alpha: vec![0.2, 0.8],
        }
    }

    // r[verify sched.sla.threat.corpus-clamp+2]
    #[test]
    fn validate_rejects_non_finite() {
        let cfg = vcfg();
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1e12] {
            for setter in [
                |e: &mut SeedEntry, v| e.s = v,
                |e: &mut SeedEntry, v| e.p = v,
                |e: &mut SeedEntry, v| e.q = v,
                |e: &mut SeedEntry, v| e.a = v,
                |e: &mut SeedEntry, v| e.b = v,
                |e: &mut SeedEntry, v| e.n_eff = v,
            ] {
                let mut e = ok_entry();
                setter(&mut e, bad);
                assert!(
                    validate_seed_entry(&e, &cfg).is_err(),
                    "{bad} accepted on a scalar field"
                );
            }
            // alpha vector: each element checked.
            let mut e = ok_entry();
            e.alpha = vec![0.5, bad];
            assert!(validate_seed_entry(&e, &cfg).is_err(), "α={bad} accepted");
        }
        // -1.0 rejected on every range-checked scalar EXCEPT b (log-log
        // slope, may be negative on mem-independent workloads).
        for setter in [
            |e: &mut SeedEntry, v| e.s = v,
            |e: &mut SeedEntry, v| e.p = v,
            |e: &mut SeedEntry, v| e.q = v,
            |e: &mut SeedEntry, v| e.a = v,
            |e: &mut SeedEntry, v| e.n_eff = v,
        ] {
            let mut e = ok_entry();
            setter(&mut e, -1.0);
            assert!(validate_seed_entry(&e, &cfg).is_err());
        }
        let mut e = ok_entry();
        e.b = -0.5;
        assert!(
            validate_seed_entry(&e, &cfg).is_ok(),
            "b<0 accepted (export→import roundtrip on mem-indep fit)"
        );
        e.b = -3.0;
        assert!(validate_seed_entry(&e, &cfg).is_err(), "|b|>2 rejected");
        // p̄ is round-trip-only: finiteness checked, NOT bounded by the
        // importer's max_cores. A `Capped{p̄=90}` exported from a
        // max_cores=128 cluster must import cleanly to max_cores=64.
        assert_eq!(cfg.max_cores, 64.0);
        let mut e = ok_entry();
        e.p_bar = 90.0;
        assert!(
            validate_seed_entry(&e, &cfg).is_ok(),
            "p̄=90 > importer max_cores=64 must be Ok (round-trip-only)"
        );
        e.p_bar = -1.0;
        assert!(
            validate_seed_entry(&e, &cfg).is_ok(),
            "p̄=-1 finite → Ok (no range check on round-trip-only field)"
        );
        e.p_bar = f64::NAN;
        assert!(
            validate_seed_entry(&e, &cfg).is_err(),
            "p̄=NaN must still be rejected"
        );
        // Control: ok_entry passes.
        assert!(validate_seed_entry(&ok_entry(), &cfg).is_ok());
    }

    // r[verify sched.sla.threat.corpus-clamp+2]
    #[test]
    fn validate_clamps_ranges() {
        let cfg = vcfg();
        let bt_ref = cfg.build_timeout_ref();
        // S/P/Q ∈ [0, bt_ref]
        let mut e = ok_entry();
        e.s = bt_ref + 1.0;
        assert!(validate_seed_entry(&e, &cfg).is_err(), "S > bt_ref");
        let mut e = ok_entry();
        e.q = -0.1;
        assert!(validate_seed_entry(&e, &cfg).is_err(), "Q < 0");
        // p̄ finite-only (round-trip field; 0 is the export sentinel for ∞)
        let mut e = ok_entry();
        e.p_bar = 0.0;
        assert!(
            validate_seed_entry(&e, &cfg).is_ok(),
            "p̄=0 is the ∞ sentinel from export_corpus"
        );
        // n_eff ∈ [0, 32]
        let mut e = ok_entry();
        e.n_eff = 33.0;
        assert!(validate_seed_entry(&e, &cfg).is_err(), "n_eff > 32");
        // a ∈ [0, ln(max_mem)], b ∈ [-2, 2] (log-log domain, NOT bytes)
        let mut e = ok_entry();
        e.a = (cfg.max_mem as f64).ln() + 1.0;
        assert!(validate_seed_entry(&e, &cfg).is_err(), "a > ln(max_mem)");
        // α ∈ [0, 1]^K
        let mut e = ok_entry();
        e.alpha = vec![1.1];
        assert!(validate_seed_entry(&e, &cfg).is_err(), "α > 1");
        // ref_factor_vec ∈ [0.1, 10]^K
        let bad_vec = SeedCorpus {
            ref_hw_class: "x".into(),
            ref_factor_vec: vec![0.05, 1.0, 1.0],
            entries: vec![ok_entry()],
        };
        assert!(validate_corpus(&bad_vec, &cfg).is_err(), "factor < 0.1");
        let bad_vec = SeedCorpus {
            ref_factor_vec: vec![1.0, 11.0],
            ..bad_vec
        };
        assert!(validate_corpus(&bad_vec, &cfg).is_err(), "factor > 10");
        // validate_corpus also checks every entry.
        let bad_entry = SeedCorpus {
            ref_hw_class: "x".into(),
            ref_factor_vec: vec![1.0],
            entries: vec![ok_entry(), {
                let mut e = ok_entry();
                e.s = f64::NAN;
                e
            }],
        };
        assert!(validate_corpus(&bad_entry, &cfg).is_err());
        // Clean corpus passes.
        let ok = SeedCorpus {
            ref_hw_class: "x".into(),
            ref_factor_vec: vec![1.0, 2.0],
            entries: vec![ok_entry()],
        };
        assert!(validate_corpus(&ok, &cfg).is_ok());
    }

    // r[verify sched.sla.threat.corpus-clamp+2]
    #[test]
    fn validate_accepts_v1_entry_defaults() {
        // v1 JSON has no version/alpha/n_eff/ref_factor_vec → all
        // serde-default to empty/0.0. The validator must accept those
        // defaults so a v1 corpus still imports.
        let cfg = vcfg();
        let v1 = SeedEntry {
            pname: "hello".into(),
            system: "x86_64-linux".into(),
            s: 30.0,
            p: 600.0,
            q: 0.0,
            p_bar: 8.0,
            a: 22.0,
            b: 0.5,
            n: 5,
            ..Default::default()
        };
        assert!(validate_seed_entry(&v1, &cfg).is_ok());
        let c = SeedCorpus {
            ref_hw_class: "x".into(),
            entries: vec![v1],
            ..Default::default()
        };
        assert!(validate_corpus(&c, &cfg).is_ok(), "empty ref_factor_vec ok");
    }
}
