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

use super::config::ProbeShape;
use super::hw::HwTable;
use super::types::ModelKey;

/// (S, P, Q, a, b) flat tuple. Distinct from [`super::types::FittedParams`]
/// (which carries the staged enums + explore state) because the prior
/// machinery only needs the five scalars the fitter regresses on.
#[derive(Debug, Clone, PartialEq)]
pub struct FitParams {
    pub s: f64,
    pub p: f64,
    pub q: f64,
    pub a: f64,
    pub b: f64,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeedCorpus {
    pub ref_hw_class: String,
    pub entries: Vec<SeedEntry>,
}

/// One `(pname, system)` row. `p_bar`/`n` are informational (round-trip
/// fidelity, operator inspection); only `(s, p, q, a, b)` feed the prior.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeedEntry {
    pub pname: String,
    pub system: String,
    pub s: f64,
    pub p: f64,
    pub q: f64,
    pub p_bar: f64,
    pub a: f64,
    pub b: f64,
    pub n: u32,
}

impl SeedCorpus {
    /// Read + parse from `path`. Called once at startup from
    /// [`super::SlaEstimator::new`] when `[sla].seed_corpus` is set.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let bytes = std::fs::read(path)
            .map_err(|e| anyhow::anyhow!("read seed_corpus {}: {e}", path.display()))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| anyhow::anyhow!("parse seed_corpus {}: {e}", path.display()))
    }

    /// Project to the seed map, rescaling time-domain params (S, P) by
    /// `factor[self.ref_hw_class] / factor[hw.reference]`. The exporter
    /// recorded (S, P) in THEIR reference-seconds; locally that hw_class
    /// has some `factor` ≠ 1.0 if it's not our reference. Q (contention,
    /// per-core) and (a, b) (memory, raw bytes) are NOT rescaled —
    /// neither is hw-throughput-dominated.
    ///
    /// Unknown `ref_hw_class` → factor 1.0 (pass through unscaled). The
    /// returned `f64` is the applied factor, for the RPC response.
    pub fn into_seed_map(self, hw: &HwTable) -> (HashMap<SeedKey, FitParams>, f64) {
        let scale = hw.factor(&self.ref_hw_class) / hw.factor(&hw.reference);
        let map = self
            .entries
            .into_iter()
            .map(|e| {
                (
                    (e.pname, e.system),
                    FitParams {
                        s: e.s * scale,
                        p: e.p * scale,
                        q: e.q,
                        a: e.a,
                        b: e.b,
                    },
                )
            })
            .collect();
        (map, scale)
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
    /// `[sla].default_tier`. Feeds [`operator_to_spq`]'s "a build the
    /// operator expects to take `target` on `probe.cpu` cores".
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
    FitParams {
        s: clamp_field(fleet.s, basis.s, "s", false),
        p: clamp_field(fleet.p, basis.p, "p", false),
        q: clamp_field(fleet.q, basis.q, "q", false),
        a: clamp_field(fleet.a, basis.a, "a", true),
        b: clamp_field(fleet.b, basis.b, "b", false),
    }
}

/// Median-of-tenant-medians over the caller's fitted-key set. One vote
/// per tenant regardless of how many `ModelKey`s that tenant has fit —
/// a tenant with 10k builds of one package can't drag the fleet prior
/// (Sybil-resistant in the "noisy neighbour" sense, not the crypto
/// sense). Returns `None` below `min_keys` so an empty/cold fleet falls
/// through to [`PriorSource::Operator`].
///
/// Caller is expected to pre-filter to keys whose `MemFit` is
/// [`Coupled`](super::types::MemFit::Coupled) — `FitParams.{a,b}` are
/// only meaningful for those.
pub fn fleet_median(all: &[(ModelKey, FitParams)], min_keys: usize) -> Option<FitParams> {
    if all.len() < min_keys {
        return None;
    }
    let mut by_tenant: HashMap<&str, Vec<&FitParams>> = HashMap::new();
    for (k, f) in all {
        by_tenant.entry(k.tenant.as_str()).or_default().push(f);
    }
    let tenant_medians: Vec<FitParams> = by_tenant.values().map(|v| median_fitparams(v)).collect();
    Some(median_fitparams(&tenant_medians.iter().collect::<Vec<_>>()))
}

/// Per-field median. Even-length → mean of the two straddling elements
/// (the five fields are independent so there's no "the median row").
fn median_fitparams(v: &[&FitParams]) -> FitParams {
    debug_assert!(!v.is_empty());
    let med = |proj: fn(&FitParams) -> f64| -> f64 {
        let mut xs: Vec<f64> = v.iter().map(|f| proj(f)).collect();
        xs.sort_by(f64::total_cmp);
        let n = xs.len();
        if n % 2 == 1 {
            xs[n / 2]
        } else {
            (xs[n / 2 - 1] + xs[n / 2]) / 2.0
        }
    };
    FitParams {
        s: med(|f| f.s),
        p: med(|f| f.p),
        q: med(|f| f.q),
        a: med(|f| f.a),
        b: med(|f| f.b),
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
        };
        let prior = FitParams {
            s: 0.0,
            p: 0.0,
            q: 0.0,
            a: 0.0,
            b: 0.0,
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
        };
        let fleet = FitParams {
            s: 10.0,
            p: 20.0,
            q: 30.0,
            a: 40.0,
            b: 50.0,
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
        FitParams { s, p, q, a, b }
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
        let m = fleet_median(&all, 20).expect("60 ≥ 20");
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
        assert!(fleet_median(&all, 20).is_none());
        assert!(fleet_median(&all, 2).is_some());
    }

    #[test]
    fn corpus_roundtrip_rescales_time_domain() {
        // Exporter recorded on ref="aws-6-ebs". Our hw table says that
        // class is 1.5× our reference → S/P scale by 1.5; Q/a/b don't.
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
            }],
        };
        let json = serde_json::to_string(&corpus).unwrap();
        let parsed: SeedCorpus = serde_json::from_str(&json).unwrap();
        let mut hw = HashMap::new();
        hw.insert("aws-5-ebs".into(), 1.0); // local reference
        hw.insert("aws-6-ebs".into(), 1.5);
        let (map, scale) = parsed.into_seed_map(&HwTable::from_map(hw));
        assert!((scale - 1.5).abs() < 1e-9);
        let fp = &map[&("hello".into(), "x86_64-linux".into())];
        assert!((fp.s - 15.0).abs() < 1e-9);
        assert!((fp.p - 150.0).abs() < 1e-9);
        assert!((fp.q - 0.02).abs() < 1e-9, "q not rescaled");
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
            }],
        };
        let (map, scale) = corpus.into_seed_map(&HwTable::default());
        assert_eq!(scale, 1.0);
        assert_eq!(map[&("x".into(), "x86_64-linux".into())].s, 5.0);
    }

    #[test]
    fn fleet_median_per_field_median_even() {
        // Two tenants, one key each → tenant_medians has 2 entries →
        // even-length per-field median = mean.
        let all = vec![
            (tkey("t0", "a"), fp(10.0, 100.0, 0.0, 20.0, 0.8)),
            (tkey("t1", "b"), fp(30.0, 300.0, 0.2, 24.0, 1.2)),
        ];
        let m = fleet_median(&all, 2).unwrap();
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
}
