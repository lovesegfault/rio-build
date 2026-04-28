//! ADR-023 §Hardware heterogeneity: reference-second normalization.
//!
//! `build_samples.duration_secs` is wall-clock on whatever hw_class the
//! pod ran on. Before fitting T(c), [`super::ingest::refit`] maps each
//! sample's wall-clock to the **reference timeline** via
//! `wall × (α[pname] · factor[hw_class])`, with α the per-pname K=3
//! mixture from [`super::alpha`]. A sample with no `hw_class` (NULL —
//! old executor / non-k8s / informer race) or an hw_class with <3
//! distinct pod samples passes through at `factor=[1.0; K]`.
//!
//! `factor` is the K=3 microbench vector `[alu, membw, ioseq]` (M_054).
//! Aggregation is **app-side median-of-medians** (ADR-023 §Threat-model
//! gap (b)): group by `(hw_class, submitting_tenant)`, per-dimension
//! median + 3·MAD reject within each tenant-group, then per-dimension
//! median across tenant-groups. One tenant flooding `hw_perf_samples`
//! contributes one rank to the fleet median, not N. The dropped
//! `hw_perf_factors` view did a flat row-median — see
//! `rio_store::migrations::M_054`.
//!
//! Memory is NOT normalized: peak RSS is dominated by the workload, not
//! core throughput. ADR-023's M(c) is fitted on raw bytes.

use std::collections::{HashMap, HashSet};

use crate::db::SchedulerDb;

/// Sanity floor for hw factors. A pathological `hw_perf_samples` row
/// (bad bench, clock skew) could yield a factor near 0; `ref_secs /
/// min_factor()` would then blow the deadline up ~×100 (capped at 24h,
/// but wasteful). Clamp at 0.25 — i.e., assume no admitted hw_class is
/// more than 4× slower than the reference.
pub(crate) const HW_FACTOR_SANITY_FLOOR: f64 = 0.25;

/// Sanity ceiling, mirror of [`HW_FACTOR_SANITY_FLOOR`]: assume no
/// admitted hw_class is more than 4× FASTER than the reference.
/// Belt-and-suspenders for `r[sec.boundary.grpc-hmac]` on
/// `AppendHwPerfSample` — even with `pod_id` derived from claims, a
/// compromised builder holding one valid token can write its one row
/// at an absurd `factor`; the clamp bounds the blast radius of that
/// one rank.
pub(crate) const HW_FACTOR_SANITY_CEIL: f64 = 4.0;

/// Minimum distinct `pod_id` count before an hw_class's aggregated
/// `factor` is trusted. Below this, [`cross_tenant_median`] returns
/// `[1.0; K]` (pass-through) and [`HwTable::factor`] returns `None`.
pub(crate) const HW_MIN_PODS: u32 = 3;

/// K=3 microbench dimensions: `[alu, membw, ioseq]`. Index constants
/// rather than a struct so per-dimension reductions stay `for d in 0..K`.
pub const K: usize = 3;

/// One `hw_perf_samples` row, jsonb `factor` parsed to per-dimension
/// `Option<f64>`. `None` ⇔ key absent in the jsonb (bug_037: per-dim
/// presence carried end-to-end so a `bench_needed=false` row
/// contributes nothing to membw/ioseq medians, instead of a `1.0`
/// placeholder that drags the fleet median to reference).
/// `submitting_tenant` is `None` for pre-M_054 rows and for builders
/// without a tenant context (e.g. probe pods).
#[derive(Debug, Clone)]
pub struct HwPerfSampleRow {
    pub hw_class: String,
    pub pod_id: String,
    pub submitting_tenant: Option<String>,
    pub factor: [Option<f64>; K],
}

/// `cross_tenant_median`'s output: the per-dimension fleet median
/// AND the per-dimension population it was computed over.
///
/// `tenants_with_dim[d]` is the EXACT population `factor[d]` was
/// computed over — the post-`mad_reject_median` `tenant_medians`
/// vector, NOT the raw row set. The `HwClassSampled` RPC reports
/// `tenants_with_dim` so the controller's `bench_needed` gate and the
/// scheduler's `min_tenants` gate read the same number (bug_009: a
/// pre-filter raw-row count diverges when MAD rejects a tenant's only
/// `Some(d)` row → calibration deadlock at the threshold boundary).
#[derive(Debug, Clone, Copy)]
pub struct FleetMedian {
    pub factor: [f64; K],
    pub tenants_with_dim: [u32; K],
}

/// Aggregated per-hw_class factor: K=3 vector + distinct-pod count +
/// per-dimension distinct-tenant count. `factor[d]` is `1.0` when
/// `pod_ids < 3` OR `tenants_with_dim[d] < min_tenants` (untrusted
/// pass-through, applied per-dim — bug_013).
#[derive(Debug, Clone, Copy)]
pub struct HwFactor {
    pub factor: [f64; K],
    pub pod_ids: u32,
    /// See [`FleetMedian::tenants_with_dim`]. Populated ONLY by
    /// destructuring `cross_tenant_median`'s return — no separate
    /// computation exists (bug_009 structural close).
    pub tenants_with_dim: [u32; K],
}

/// Per-hw_class aggregated K=3 microbench factor. Built app-side from
/// `hw_perf_samples` each [`super::SlaEstimator::refresh`] tick. Cheap
/// to clone (a few dozen entries).
// r[impl sched.sla.hw-ref-seconds]
#[derive(Debug, Clone, Default)]
pub struct HwTable {
    factors: HashMap<String, HwFactor>,
    /// Admitted hw_class whose `factor[alu]` is closest to 1.0 (the
    /// calibration target). Ties broken lexicographically. Informational
    /// (SlaExplain); the normalization itself doesn't special-case it.
    pub reference: String,
}

impl HwTable {
    /// `wall_secs × (α · factor[hw_class])`. Unknown / `None` / <3-pod
    /// hw_class → `factor := [1;K]` (pass-through). Time-domain only —
    /// call on `duration_secs` and `cpu_seconds_total`, NOT on memory
    /// or disk. The dot-product clamp lives in [`super::alpha::dot`].
    pub fn normalize(
        &self,
        wall_secs: f64,
        hw_class: Option<&str>,
        alpha: super::alpha::Alpha,
    ) -> f64 {
        let f = hw_class.and_then(|h| self.factor(h)).unwrap_or([1.0; K]);
        wall_secs * super::alpha::dot(alpha, f)
    }

    /// K=3 `factor[hw_class]`, or `None` if unknown / <3 distinct pods.
    /// `None` callers default to `[1;K]` for the T_ref fit (bias-neutral
    /// pass-through, ADR-023 L539) and **exclude** the row from the α-fit
    /// (ADR-023 L541). Per-dimension clamped to `[FLOOR, CEIL]` so a
    /// poisoned bench row's blast radius is bounded before any α-dot.
    pub fn factor(&self, hw_class: &str) -> Option<[f64; K]> {
        self.factors
            .get(hw_class)
            .filter(|f| f.pod_ids >= HW_MIN_PODS)
            .map(|f| {
                f.factor
                    .map(|d| d.clamp(HW_FACTOR_SANITY_FLOOR, HW_FACTOR_SANITY_CEIL))
            })
    }

    /// Distinct `pod_id` count for `hw_class` across the 7-day window,
    /// regardless of the ≥3 trust floor. For SlaExplain / debug; the
    /// `HwClassSampled` handler reads [`Self::distinct_tenants_per_dim`].
    pub fn distinct_pod_ids(&self, hw_class: &str) -> u32 {
        self.factors.get(hw_class).map(|f| f.pod_ids).unwrap_or(0)
    }

    /// Per-dimension distinct-tenant count for `hw_class`: how many
    /// `submitting_tenant` buckets contributed a `Some(d)`
    /// tenant-median. `[0; K]` for an unknown class. For the
    /// `HwClassSampled` handler — the controller's `bench_needed =
    /// ∃d: count[d] < trust_threshold` reads `cross_tenant_median`'s
    /// per-dim `min_tenants` gate population (bug_009: same vector —
    /// populated by `cross_tenant_median` itself; merged_bug_001:
    /// same VALUE — `trust_threshold` is `FLEET_MEDIAN_MIN_TENANTS`
    /// shipped over the wire).
    pub fn distinct_tenants_per_dim(&self, hw_class: &str) -> [u32; K] {
        self.factors
            .get(hw_class)
            .map(|f| f.tenants_with_dim)
            .unwrap_or([0; K])
    }

    /// Iterate `(hw_class, &[f64; K])` for trusted (≥3-pod) classes. For
    /// [`super::cost`]'s per-band `h_dagger` scan. Caller dots with the
    /// per-pname α (which lives on [`super::types::FittedParams`], not
    /// here) via [`super::alpha::dot`].
    pub fn iter(&self) -> impl Iterator<Item = (&String, &[f64; K])> {
        self.factors
            .iter()
            .filter(|(_, f)| f.pod_ids >= HW_MIN_PODS)
            .map(|(k, f)| (k, &f.factor))
    }

    /// Smallest `α · factor[h]` across all trusted (≥3-pod) classes, or
    /// 1.0 when none. `ref_secs / min_factor(α)` is the worst-case
    /// (slowest-node) wall-clock for THIS pname's mixture — used by
    /// `solve_intent_for`'s deadline de-norm so `activeDeadlineSeconds`
    /// budgets for the slowest band a pod could land on.
    pub fn min_factor(&self, alpha: super::alpha::Alpha) -> f64 {
        self.iter()
            .map(|(_, f)| super::alpha::dot(alpha, *f))
            .min_by(f64::total_cmp)
            .unwrap_or(1.0)
            // Guard against pathological microbench rows; see const doc.
            .max(HW_FACTOR_SANITY_FLOOR)
    }

    /// Smallest single-dimension factor across all trusted classes:
    /// `min_h min_d factor[h][d]`. Since `dot(α, f)` is linear in α
    /// over the simplex, `min_{α∈Δ} dot(α, f[h]) = min_d f[h][d]`
    /// (vertex), so this is the worst-case `min_factor(α)` over ALL
    /// pnames — a conservative hoisted-constant bound for backstop
    /// denorm where the per-key α is unavailable. NOT the centroid
    /// `min_factor(UNIFORM)`, which is anti-conservative (bug_012).
    /// Clamped per [`super::alpha::dot`]'s `[FLOOR, CEIL]` so
    /// `min_factor_any_alpha() ≤ min_factor(α)` is unconditional.
    pub fn min_factor_any_alpha(&self) -> f64 {
        let v = self
            .iter()
            .flat_map(|(_, f)| f.iter().copied())
            .fold(f64::INFINITY, f64::min);
        if v.is_finite() {
            v.clamp(HW_FACTOR_SANITY_FLOOR, HW_FACTOR_SANITY_CEIL)
        } else {
            1.0
        }
    }

    /// Stable hash of the **solve-relevant projection**: per entry,
    /// `(hw_class, factor[K].map(clamp), pod_ids >= HW_MIN_PODS)` —
    /// exactly what [`Self::factor`] / [`Self::iter`] / [`Self::len`]
    /// expose to [`super::solve::solve_full`]. NOT raw `pod_ids`: solve
    /// reads it only as the `>= HW_MIN_PODS` trust bool, and `pod_ids`
    /// is monotone under one-shot Jobs (every annotated builder writes a
    /// row), so hashing the raw count would re-roll ε_h ~every 60s in
    /// steady state — the same selector-drift reap r1/r2 each fixed.
    ///
    /// **Hashes quantized accessor output, NOT raw state** (merged_bug_018):
    /// `factor[d]` is a re-derived median over noisy ±20% bench rows;
    /// hashed at 1% buckets (`(clamp(x)·100).round()`). Quantum is ≤
    /// τ/10 of the τ=15% solve tolerance so steady-state bench noise
    /// lands in one bucket → `inputs_gen` stable. Within a bucket
    /// `c*`/τ-membership MAY differ by one step (`ceil`/threshold are
    /// discontinuous) — bounded one-tick staleness, NOT a guarantee
    /// that bucket-equal ⇒ solve-output-equal.
    ///
    /// Feeds [`super::solve::SolveInputs::inputs_gen`] (the derived
    /// `inputs_gen`). Sorted by key so iteration order is irrelevant.
    /// Untrusted (`pod_ids < HW_MIN_PODS`) entries are filtered — they
    /// are invisible to every solve accessor, so their appearance in
    /// `factors` (e.g. a 1-pod row landing) is NOT a solve-relevant
    /// change. Crossing the 2→3 threshold IS: the key enters the hash.
    pub fn solve_relevant_hash(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut keys: Vec<_> = self
            .factors
            .iter()
            .filter(|(_, f)| f.pod_ids >= HW_MIN_PODS)
            .map(|(k, _)| k)
            .collect();
        keys.sort();
        let mut h = std::hash::DefaultHasher::new();
        for k in keys {
            k.hash(&mut h);
            for d in self.factors[k].factor {
                ((d.clamp(HW_FACTOR_SANITY_FLOOR, HW_FACTOR_SANITY_CEIL) * 100.0).round() as i64)
                    .hash(&mut h);
            }
        }
        h.finish()
    }

    /// Distinct hw_classes with ≥3 pod samples. For SlaStatus.
    pub fn len(&self) -> usize {
        self.factors
            .values()
            .filter(|f| f.pod_ids >= HW_MIN_PODS)
            .count()
    }

    /// `len() == 0`. Clippy `len_without_is_empty`.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// App-side median-of-medians (ADR-023 §Threat-model gap (b)).
    ///
    /// Group by `(hw_class, submitting_tenant)`, then per hw_class call
    /// `cross_tenant_median` with `FLEET_MEDIAN_MIN_TENANTS` — the pod
    /// gate, tenant-count gate, MAD-reject, fleet median, and
    /// clamp all live there so a future caller can't omit a step. The
    /// entry is kept (with `factor := [1.0; K]` when gated) so
    /// [`Self::distinct_pod_ids`] reports the count.
    // r[impl sched.sla.threat.hw-median-of-medians]
    pub fn aggregate(rows: &[HwPerfSampleRow]) -> HashMap<String, HwFactor> {
        // hw_class → submitting_tenant → Vec<[Option<f64>;K]>
        type ByTenant = HashMap<Option<String>, Vec<[Option<f64>; K]>>;
        let mut by_class: HashMap<String, ByTenant> = HashMap::new();
        // hw_class → distinct pod_ids
        let mut pods: HashMap<String, HashSet<String>> = HashMap::new();
        for r in rows {
            by_class
                .entry(r.hw_class.clone())
                .or_default()
                .entry(r.submitting_tenant.clone())
                .or_default()
                .push(r.factor);
            pods.entry(r.hw_class.clone())
                .or_default()
                .insert(r.pod_id.clone());
        }
        by_class
            .into_iter()
            .map(|(h, tenants)| {
                let pod_ids = pods[&h].len() as u32;
                // bug_009: count and gate from ONE vector. The
                // standalone pre-MAD raw-row count that lived here is
                // deleted; `tenants_with_dim` is now structurally the
                // gate's own population.
                let FleetMedian {
                    factor,
                    tenants_with_dim,
                } = cross_tenant_median(&tenants, pod_ids, super::FLEET_MEDIAN_MIN_TENANTS);
                (
                    h,
                    HwFactor {
                        factor,
                        pod_ids,
                        tenants_with_dim,
                    },
                )
            })
            .collect()
    }

    /// Snapshot `hw_perf_samples` (7-day window, mirroring the dropped
    /// `hw_perf_factors` view's recency filter) and [`Self::aggregate`].
    /// Legacy scalar rows (pre-M_054 backfill `{"alu": <f64>}`) and
    /// `bench_needed=false` rows parse `membw`/`ioseq` as `None` —
    /// they contribute nothing to those dims' median (bug_037).
    pub async fn load(db: &SchedulerDb) -> anyhow::Result<Self> {
        let raw: Vec<(String, String, Option<String>, serde_json::Value)> = sqlx::query_as(
            "SELECT hw_class, pod_id, submitting_tenant, factor \
             FROM hw_perf_samples WHERE measured_at > now() - interval '7 days'",
        )
        .fetch_all(db.pool())
        .await?;
        let rows: Vec<HwPerfSampleRow> = raw
            .into_iter()
            .map(|(hw_class, pod_id, submitting_tenant, j)| HwPerfSampleRow {
                hw_class,
                pod_id,
                submitting_tenant,
                factor: parse_factor(&j),
            })
            .collect();
        Ok(Self::from_aggregate(Self::aggregate(&rows)))
    }

    /// Build from a pre-aggregated map; computes `reference`.
    fn from_aggregate(factors: HashMap<String, HwFactor>) -> Self {
        // reference = trusted class whose factor.alu is closest to 1.0
        // (the calibration target). Ties → lexicographic for determinism.
        let reference = factors
            .iter()
            .filter(|(_, f)| f.pod_ids >= HW_MIN_PODS)
            .min_by(|(ka, va), (kb, vb)| {
                (va.factor[0] - 1.0)
                    .abs()
                    .partial_cmp(&(vb.factor[0] - 1.0).abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| ka.cmp(kb))
            })
            .map(|(k, _)| k.clone())
            .unwrap_or_default();
        Self { factors, reference }
    }

    /// Test constructor: bypass PG. Scalar `f64` wrapped as the
    /// **isotropic** vector `[v; K]` with `pod_ids=3` (trusted), so
    /// `α · factor = v` for any α on the simplex — tests written against
    /// the old scalar API stay correct under arbitrary α. Values pass
    /// through UNCLAMPED so tests can probe the per-consumer `[FLOOR,
    /// CEIL]` clamps; for chokepoint testing use [`Self::from_raw`].
    #[cfg(test)]
    pub fn from_map(factors: HashMap<String, f64>) -> Self {
        Self::from_factors(factors.into_iter().map(|(h, v)| (h, [v; K])).collect())
    }

    /// Test constructor: bypass PG with explicit K=3 vectors at
    /// `pod_ids=3` (trusted). Unclamped — see [`Self::from_map`].
    #[cfg(test)]
    pub fn from_factors(factors: HashMap<String, [f64; K]>) -> Self {
        Self::from_aggregate(
            factors
                .into_iter()
                .map(|(h, factor)| {
                    (
                        h,
                        HwFactor {
                            factor,
                            pod_ids: 3,
                            // "trusted in all K dims" — matches the
                            // `pod_ids: 3` semantics. Reports
                            // ≥threshold so config-seeded classes
                            // satisfy the controller's bench gate.
                            tenants_with_dim: [super::FLEET_MEDIAN_MIN_TENANTS as u32; K],
                        },
                    )
                })
                .collect(),
        )
    }

    /// Test constructor: bypass PG, mirroring `aggregate`'s chokepoint
    /// clamp. Use to verify that pathological raw values are bounded
    /// before any consumer (including [`Self::iter`]) sees them.
    #[cfg(test)]
    pub fn from_raw(raw: HashMap<String, f64>) -> Self {
        Self::from_map(
            raw.into_iter()
                .map(|(h, f)| (h, f.clamp(HW_FACTOR_SANITY_FLOOR, HW_FACTOR_SANITY_CEIL)))
                .collect(),
        )
    }
}

/// ADR-023 §Threat-model gap (b) chokepoint: every gate the
/// median-of-medians defence depends on, in order, so a caller can't
/// omit one. Returns `[1.0; K]` (untrusted pass-through) when EITHER
/// gate trips; otherwise the per-tenant MAD-rejected median, then the
/// per-dimension median across tenants, clamped to `[FLOOR, CEIL]`.
///
/// 1. **Pod gate (per-row):** `n_pods < HW_MIN_PODS` → `[1.0; K]`.
///    With <3 distinct pods the per-tenant median is one or two
///    samples.
/// 2. Per tenant: 3·MAD reject, then per-dimension median.
/// 3. **Tenant gate (per-dim, bug_013):** for each dimension `d`, if
///    fewer than `min_tenants` tenant-medians have `Some(d)` →
///    `factor[d] = 1.0`. A "median-of-tenant-medians" with one tenant
///    IS that tenant's median; with two, the fleet median is one of
///    them. The caller must supply `min_tenants` (no default) — see
///    [`super::FLEET_MEDIAN_MIN_TENANTS`]. This gate is applied at
///    the same granularity as the aggregate it protects: bug_037's
///    `[Option<f64>; K]` per-dim presence created a per-dim
///    cardinality axis; gating only on `by_tenant.len()` (per-row)
///    let one tenant writing K=3 capture the membw/ioseq median while
///    `min_tenants` honest alu-only tenants satisfied the row count
///    (`r[sched.sla.threat.hw-median-of-medians]` violated by one).
/// 4. Across tenants: per-dimension median.
/// 5. Clamp `[FLOOR, CEIL]` post-aggregation so MAD sees raw values.
///
/// `min_tenants` is a parameter (not a const reference) so a new
/// median-of-medians consumer (e.g. §13b's per-cell bias table) can't
/// silently skip the gate — the type forces them to supply a value.
///
/// **Per-dim presence (bug_037):** input rows carry `[Option<f64>; K]`;
/// the per-tenant MAD-median and the cross-tenant median both operate
/// per-dimension over the rows where that dim is `Some`. A tenant with
/// zero `membw` rows contributes nothing to membw's fleet median (it
/// is NOT a `1.0` rank). The pod gate stays per-row; the tenant gate
/// is per-dim (bug_013) so the `< min_tenants → 1.0` fallback fires
/// per dimension.
pub(super) fn cross_tenant_median(
    by_tenant: &HashMap<Option<String>, Vec<[Option<f64>; K]>>,
    n_pods: u32,
    min_tenants: usize,
) -> FleetMedian {
    if n_pods < HW_MIN_PODS {
        return FleetMedian {
            factor: [1.0; K],
            tenants_with_dim: [0; K],
        };
    }
    let tenant_medians: Vec<[Option<f64>; K]> =
        by_tenant.values().map(|v| mad_reject_median(v)).collect();
    // bug_009: `tenants_with_dim` and the `min_tenants` gate read the
    // SAME `tenant_medians` vector. No future filter can sit between
    // the count and the gate — they're one stack frame.
    let tenants_with_dim: [u32; K] =
        std::array::from_fn(|d| tenant_medians.iter().filter(|r| r[d].is_some()).count() as u32);
    // Per-dim: collect Some(d) tenant-medians, gate on count, median,
    // clamp. Inlined (not `per_dim_median(...).map(...)`) so the
    // `min_tenants` check sees the per-dim `xs.len()` directly.
    let factor = std::array::from_fn(|d| {
        let mut xs: Vec<f64> = tenant_medians.iter().filter_map(|r| r[d]).collect();
        debug_assert_eq!(xs.len() as u32, tenants_with_dim[d]);
        if xs.len() < min_tenants {
            return 1.0;
        }
        xs.sort_by(f64::total_cmp);
        xs[xs.len() / 2].clamp(HW_FACTOR_SANITY_FLOOR, HW_FACTOR_SANITY_CEIL)
    });
    FleetMedian {
        factor,
        tenants_with_dim,
    }
}

/// jsonb `{"alu":a,"membw":b,"ioseq":c}` → `[Some(a),Some(b),Some(c)]`.
/// Missing keys → `None` (bug_037: NOT `1.0` — that's a sentinel
/// collision with "reference-class measurement"). Legacy scalar rows
/// (M_054's `USING jsonb_build_object('alu', factor)`) and
/// `bench_needed=false` rows both yield `[Some(alu), None, None]`.
fn parse_factor(j: &serde_json::Value) -> [Option<f64>; K] {
    let get = |k: &str| j.get(k).and_then(serde_json::Value::as_f64);
    [get("alu"), get("membw"), get("ioseq")]
}

/// Per-dimension median over the rows where that dim is `Some`.
/// Upper-middle for even `n` (matches `cost::interrupt_rate`'s
/// convention). `None` when zero rows have dim `d` — the caller
/// decides the fallback.
fn per_dim_median(v: &[[Option<f64>; K]]) -> [Option<f64>; K] {
    std::array::from_fn(|d| {
        let mut xs: Vec<f64> = v.iter().filter_map(|r| r[d]).collect();
        if xs.is_empty() {
            return None;
        }
        xs.sort_by(f64::total_cmp);
        Some(xs[xs.len() / 2])
    })
}

/// Per-dimension 3·MAD outlier reject, then [`per_dim_median`] of
/// survivors. A row is rejected if **any** measured dimension's
/// `|x − med| > 3·1.4826·MAD` (a poisoned bench result poisons the
/// whole vector). A row's `None` dim is not an outlier (unmeasured ≠
/// out-of-band) and never triggers rejection. `MAD == 0` / `med =
/// None` → no rejection on that dimension.
fn mad_reject_median(v: &[[Option<f64>; K]]) -> [Option<f64>; K] {
    let med = per_dim_median(v);
    let mad: [Option<f64>; K] = {
        let dev: Vec<[Option<f64>; K]> = v
            .iter()
            .map(|r| std::array::from_fn(|d| Some((r[d]? - med[d]?).abs())))
            .collect();
        per_dim_median(&dev)
    };
    let kept: Vec<[Option<f64>; K]> = v
        .iter()
        .copied()
        .filter(|r| {
            (0..K).all(|d| match (r[d], med[d], mad[d]) {
                (Some(x), Some(m), Some(mad_d)) if mad_d != 0.0 => {
                    (x - m).abs() <= 3.0 * 1.4826 * mad_d
                }
                _ => true,
            })
        })
        .collect();
    per_dim_median(if kept.is_empty() { v } else { &kept })
}

#[cfg(test)]
mod tests {
    use super::super::alpha::{Alpha, UNIFORM, dot};
    use super::*;

    /// Pure-alu α reproduces the pre-A7 `factor[0]` projection.
    const ALU: Alpha = [1.0, 0.0, 0.0];

    fn row(hw: &str, pod: &str, tenant: Option<&str>, alu: f64) -> HwPerfSampleRow {
        HwPerfSampleRow {
            hw_class: hw.into(),
            pod_id: pod.into(),
            submitting_tenant: tenant.map(String::from),
            factor: [Some(alu), Some(1.0), Some(1.0)],
        }
    }

    /// Shorthand: all-K-present row.
    const fn s(v: [f64; K]) -> [Option<f64>; K] {
        [Some(v[0]), Some(v[1]), Some(v[2])]
    }

    #[test]
    fn normalize_passes_through_unknown() {
        let t = HwTable::default();
        assert_eq!(t.normalize(100.0, None, UNIFORM), 100.0);
        assert_eq!(t.normalize(100.0, Some("aws-7-ebs"), UNIFORM), 100.0);
    }

    #[test]
    fn normalize_scales_known() {
        let mut m = HashMap::new();
        m.insert("aws-5-ebs".into(), 1.0);
        m.insert("aws-8-nvme".into(), 2.0);
        let t = HwTable::from_map(m);
        // Isotropic [2;K] · any α = 2.0 → 50s wall → 100 reference-sec.
        assert_eq!(t.normalize(50.0, Some("aws-8-nvme"), UNIFORM), 100.0);
        assert_eq!(t.normalize(50.0, Some("aws-8-nvme"), ALU), 100.0);
        assert_eq!(t.normalize(100.0, Some("aws-5-ebs"), UNIFORM), 100.0);
        assert_eq!(t.reference, "aws-5-ebs");
    }

    #[test]
    fn min_factor_clamped_at_sanity_floor() {
        let mut m = HashMap::new();
        m.insert("slow".into(), 0.01);
        let t = HwTable::from_map(m);
        assert_eq!(t.min_factor(UNIFORM), HW_FACTOR_SANITY_FLOOR);
        // Empty table → 1.0 (above floor, unchanged).
        assert_eq!(HwTable::default().min_factor(UNIFORM), 1.0);
    }

    /// bug_012: `min_factor(UNIFORM)` is the simplex CENTROID, not the
    /// minimum. `dot(α, f)` is linear in α over Δ² so `min_α dot(α, f)
    /// = min_d f[d]` (vertex). The backstop's "worst-case across all
    /// pnames" denorm needs the vertex bound; the centroid is
    /// anti-conservative — under-budgets vertex-α builds on anisotropic
    /// hw → premature cancel → poison.
    #[test]
    fn min_factor_any_alpha_is_simplex_min() {
        let mut m = HashMap::new();
        m.insert("slow".into(), [0.25, 2.0, 2.0]);
        m.insert("ref".into(), [1.0, 1.0, 1.0]);
        let t = HwTable::from_factors(m);
        // (1) The gap: simplex-min finds the 0.25 vertex; centroid
        // averages it away. dot(UNIFORM, [0.25,2,2]) = 1.417,
        // dot(UNIFORM, [1,1,1]) = 1.0 → min_factor(UNIFORM) = 1.0.
        assert_eq!(t.min_factor_any_alpha(), 0.25);
        assert_eq!(t.min_factor(UNIFORM), 1.0);
        // (2) Lower-bound invariant: min_factor_any_alpha() ≤
        // min_factor(α) for any α on the simplex (vertex ≤ any convex
        // combination). Unconditional because both sides clamp per
        // dot()'s [FLOOR, CEIL].
        for alpha in [[1.0, 0.0, 0.0], UNIFORM, [0.0, 0.0, 1.0]] {
            assert!(
                t.min_factor_any_alpha() <= t.min_factor(alpha),
                "min_factor_any_alpha()={} > min_factor({alpha:?})={}",
                t.min_factor_any_alpha(),
                t.min_factor(alpha),
            );
        }
        // (3) Empty-table parity with min_factor: 1.0 (no trusted
        // classes → reference timeline).
        assert_eq!(HwTable::default().min_factor_any_alpha(), 1.0);
    }

    /// `normalize` and `factor` clamp to `[FLOOR, CEIL]` so a single
    /// poisoned `hw_perf_samples` row can't blow T(c) fitting up by
    /// orders of magnitude. The store's `AppendHwPerfSample` already
    /// derives `pod_id` from claims (one rank per token), but `factor`
    /// is still body-supplied — this is the bound on that one rank.
    #[test]
    fn normalize_and_factor_clamped() {
        let mut m = HashMap::new();
        m.insert("fast".into(), 1e6);
        m.insert("slow".into(), 1e-6);
        let t = HwTable::from_map(m);
        assert_eq!(
            t.normalize(10.0, Some("fast"), UNIFORM),
            10.0 * HW_FACTOR_SANITY_CEIL
        );
        assert_eq!(
            t.normalize(10.0, Some("slow"), UNIFORM),
            10.0 * HW_FACTOR_SANITY_FLOOR
        );
        assert_eq!(t.factor("fast"), Some([HW_FACTOR_SANITY_CEIL; K]));
        assert_eq!(t.factor("slow"), Some([HW_FACTOR_SANITY_FLOOR; K]));
        // Unknown still passes through at 1.0 (within band).
        assert_eq!(t.normalize(10.0, Some("unknown"), UNIFORM), 10.0);
        assert!(t.factor("unknown").is_none());
    }

    /// Regression: `aggregate` clamps at the chokepoint so `iter()`
    /// returns `[FLOOR, CEIL]` values. Before,
    /// `normalize`/`factor`/`min_factor` clamped per-consumer but
    /// `iter()` returned raw — a pathological row dropped an entire
    /// hw_class out of `solve_full` for both Spot and OnDemand.
    #[test]
    fn load_clamps_pathological_factor() {
        let mut m = HashMap::new();
        m.insert("slow".into(), 0.01);
        m.insert("fast".into(), 50.0);
        let t = HwTable::from_raw(m);
        let by_name: HashMap<_, _> = t.iter().map(|(k, v)| (k.clone(), dot(ALU, *v))).collect();
        assert_eq!(by_name["slow"], HW_FACTOR_SANITY_FLOOR);
        assert_eq!(by_name["fast"], HW_FACTOR_SANITY_CEIL);
    }

    /// ADR-023 §Threat-model gap (b): one tenant flooding
    /// `hw_perf_samples` contributes one rank to the fleet median, not
    /// N. Row-median would be dragged to the attacker's value;
    /// median-of-medians stays at the honest cluster.
    #[test]
    // r[verify sched.sla.threat.hw-median-of-medians]
    fn hw_factor_median_of_medians_resists_single_tenant() {
        let mut rows = Vec::new();
        // 4 honest tenants, 1 pod each, alu ∈ [1.45, 1.55].
        for (i, alu) in [1.45, 1.48, 1.52, 1.55].into_iter().enumerate() {
            rows.push(row(
                "intel-8",
                &format!("h{i}"),
                Some(&format!("t{i}")),
                alu,
            ));
        }
        // 1 attacker, 1000 pods at alu=10.0.
        for i in 0..1000 {
            rows.push(row("intel-8", &format!("a{i}"), Some("attacker"), 10.0));
        }
        let agg = HwTable::aggregate(&rows);
        // Tenant medians: {1.45, 1.48, 1.52, 1.55, 10.0} → fleet med 1.52
        // (upper-middle of 5). Row-median would be 10.0.
        let f = agg["intel-8"].factor[0];
        assert!(
            (f - 1.5).abs() < 0.1,
            "median-of-medians resists flood: got {f}, want ~1.5 (row-median would be 10.0)"
        );
        assert_eq!(agg["intel-8"].pod_ids, 1004);

        // Tenant gate: 3 distinct tenants < FLEET_MEDIAN_MIN_TENANTS (5)
        // → `[1.0; K]` even though pod gate (3 ≥ 3) passes. With 3
        // tenants the "fleet median" is one of them — not an aggregate.
        // Before `cross_tenant_median` extracted the gate, `aggregate`
        // skipped this check entirely.
        let under = vec![
            row("intel-8", "p0", Some("t0"), 1.5),
            row("intel-8", "p1", Some("t1"), 1.5),
            row("intel-8", "p2", Some("t2"), 1.5),
        ];
        let agg = HwTable::aggregate(&under);
        assert_eq!(
            agg["intel-8"].factor, [1.0; K],
            "3 tenants < FLEET_MEDIAN_MIN_TENANTS → untrusted pass-through"
        );
        assert_eq!(agg["intel-8"].pod_ids, 3, "pod count still reported");

        // All rows in the `None` tenant bucket (pre-claims-tenant rows
        // / dev mode) → 1 distinct tenant → gated. Regression for the
        // merged_bug_001 coupling: before claims-derived tenant, EVERY
        // row had `submitting_tenant=NULL`, so the gate would have
        // disabled normalization fleet-wide had it been enforced.
        let null_tenant: Vec<_> = (0..10)
            .map(|i| row("intel-8", &format!("p{i}"), None, 1.5))
            .collect();
        let agg = HwTable::aggregate(&null_tenant);
        assert_eq!(
            agg["intel-8"].factor, [1.0; K],
            "all-NULL tenant → 1 bucket < min_tenants → gated"
        );
    }

    /// `cross_tenant_median` is the chokepoint: the same gates apply
    /// when called directly (e.g. by a future §13b per-cell consumer).
    /// `min_tenants` is a parameter so the caller MUST supply it.
    #[test]
    fn cross_tenant_median_gates_directly() {
        let mut by_tenant: HashMap<Option<String>, Vec<[Option<f64>; K]>> = HashMap::new();
        for t in 0..5 {
            by_tenant.insert(Some(format!("t{t}")), vec![s([1.5, 1.0, 1.0])]);
        }
        // Both gates pass → median; tenants_with_dim reports the gate
        // population (5 per dim — every tenant-median is Some).
        let m = cross_tenant_median(&by_tenant, 5, 5);
        assert_eq!(m.factor, [1.5, 1.0, 1.0]);
        assert_eq!(m.tenants_with_dim, [5; K]);
        // Pod gate trips → [1.0;K] AND [0;K] (no tenant_medians built).
        let m = cross_tenant_median(&by_tenant, 2, 5);
        assert_eq!(m.factor, [1.0; K]);
        assert_eq!(m.tenants_with_dim, [0; K]);
        // Tenant gate trips (caller asked for 6).
        assert_eq!(cross_tenant_median(&by_tenant, 5, 6).factor, [1.0; K]);
        // Clamp applies post-median.
        let mut huge: HashMap<Option<String>, Vec<[Option<f64>; K]>> = HashMap::new();
        for t in 0..5 {
            huge.insert(Some(format!("t{t}")), vec![s([1e6, 1.0, 1.0])]);
        }
        assert_eq!(
            cross_tenant_median(&huge, 5, 5).factor[0],
            HW_FACTOR_SANITY_CEIL,
            "chokepoint clamp"
        );
    }

    /// bug_013: per-dim `min_tenants` gate. 1 tenant with a real
    /// `membw=2.0` measurement + 4 tenants with `membw=None`
    /// (`bench_needed=false` / O_DIRECT-EINVAL) → `factor[membw] ==
    /// 1.0` (GATED), NOT 2.0. bug_037 removed the `1.0` placeholder
    /// so the one `Some(2.0)` tenant became the membw fleet median —
    /// but a 1-tenant median IS that tenant's value, so
    /// `r[sched.sla.threat.hw-median-of-medians]` was violated by
    /// one. The tenant gate now applies per-dimension at the same
    /// granularity as the per-dimension aggregate it protects.
    #[test]
    // r[verify sched.sla.threat.hw-median-of-medians]
    fn placeholder_free_membw_median() {
        let mut by_tenant: HashMap<Option<String>, Vec<[Option<f64>; K]>> = HashMap::new();
        by_tenant.insert(Some("t0".into()), vec![[Some(1.0), Some(2.0), Some(1.5)]]);
        for t in 1..5 {
            // alu-only rows (`bench_needed=false` shape).
            by_tenant.insert(Some(format!("t{t}")), vec![[Some(1.0), None, None]]);
        }
        let m = cross_tenant_median(&by_tenant, 5, 5);
        assert_eq!(m.factor[0], 1.0, "alu: 5 tenants at 1.0 → median 1.0");
        assert_eq!(
            m.factor[1], 1.0,
            "membw: 1 tenant Some(2.0) < min_tenants=5 → gated 1.0 (NOT 2.0)"
        );
        assert_eq!(
            m.factor[2], 1.0,
            "ioseq: 1 tenant Some(1.5) < min_tenants=5 → gated 1.0"
        );
        assert_eq!(m.tenants_with_dim, [5, 1, 1]);
        // ≥ min_tenants per-dim → median engages. 5 tenants alu+ioseq,
        // 0 membw → alu/ioseq pass, membw still gated.
        let mut none_membw: HashMap<Option<String>, Vec<[Option<f64>; K]>> = HashMap::new();
        for t in 0..5 {
            none_membw.insert(Some(format!("t{t}")), vec![[Some(1.3), None, Some(1.4)]]);
        }
        let m = cross_tenant_median(&none_membw, 5, 5);
        assert_eq!(m.factor, [1.3, 1.0, 1.4], "membw: 0 < min_tenants → 1.0");
        assert_eq!(m.tenants_with_dim, [5, 0, 5]);
        // 4 tenants Some(membw) — still under min_tenants=5 → gated.
        // Two colluding tenants (and even four) cannot capture.
        let mut four_membw: HashMap<Option<String>, Vec<[Option<f64>; K]>> = HashMap::new();
        for t in 0..4 {
            four_membw.insert(Some(format!("t{t}")), vec![s([1.0, 3.0, 1.0])]);
        }
        four_membw.insert(Some("t4".into()), vec![[Some(1.0), None, Some(1.0)]]);
        assert_eq!(
            cross_tenant_median(&four_membw, 5, 5).factor[1],
            1.0,
            "membw: 4 tenants < min_tenants=5 → gated"
        );
    }

    /// Aggregate floor: <3 distinct pods → `factor := [1.0; K]` but the
    /// entry is kept so [`HwTable::distinct_pod_ids`] reports 0/1/2.
    #[test]
    fn aggregate_under_3_pods_is_passthrough() {
        let rows = vec![
            row("new-hw", "p0", Some("t"), 3.0),
            row("new-hw", "p1", Some("t"), 3.0),
        ];
        let t = HwTable::from_aggregate(HwTable::aggregate(&rows));
        assert!(t.factor("new-hw").is_none(), "<3 pods → untrusted");
        assert_eq!(t.normalize(10.0, Some("new-hw"), UNIFORM), 10.0);
        assert_eq!(t.distinct_pod_ids("new-hw"), 2);
        assert_eq!(t.len(), 0, "<3-pod classes excluded from len/iter");
        assert_eq!(t.distinct_pod_ids("absent"), 0);
        assert_eq!(t.distinct_tenants_per_dim("absent"), [0; K]);
    }

    /// bug_013: `aggregate` tracks per-dim distinct-tenant count so the
    /// `HwClassSampled` RPC reports the same unit `cross_tenant_median`
    /// gates on. One tenant writing K=3 across 3 pods → `tenants_with_
    /// dim = [1; K]` (not 3) → controller's `bench_needed` stays true.
    #[test]
    fn aggregate_tracks_tenants_per_dim() {
        let row_k = |pod: &str, tenant: &str, f: [Option<f64>; K]| HwPerfSampleRow {
            hw_class: "intel-8".into(),
            pod_id: pod.into(),
            submitting_tenant: Some(tenant.into()),
            factor: f,
        };
        // 1 tenant, 3 pods, K=3-bench (the denial-of-calibration shape).
        let rows = vec![
            row_k("p0", "attacker", s([2.0, 2.0, 2.0])),
            row_k("p1", "attacker", s([2.0, 2.0, 2.0])),
            row_k("p2", "attacker", s([2.0, 2.0, 2.0])),
        ];
        let agg = HwTable::aggregate(&rows);
        assert_eq!(agg["intel-8"].pod_ids, 3);
        assert_eq!(
            agg["intel-8"].tenants_with_dim, [1; K],
            "1 tenant × 3 pods → 1 tenant per dim, not 3"
        );
        // 4 honest alu-only tenants + 1 K=3 attacker → alu has 5
        // tenants, membw/ioseq have 1. The HwClassSampled response
        // reports [5, 1, 1] so `bench_needed = ∃d: count[d] < 3` is
        // true and honest pods K=3-bench, denying the capture.
        let mut rows = rows;
        for t in 0..4 {
            rows.push(row_k(
                &format!("h{t}"),
                &format!("t{t}"),
                [Some(1.0), None, None],
            ));
        }
        let agg = HwTable::aggregate(&rows);
        assert_eq!(agg["intel-8"].tenants_with_dim, [5, 1, 1]);
        let t = HwTable::from_aggregate(agg);
        assert_eq!(t.distinct_tenants_per_dim("intel-8"), [5, 1, 1]);
    }

    /// Legacy `{"alu": x}` rows / `bench_needed=false` rows yield
    /// `None` for absent dims (bug_037: NOT a `1.0` placeholder).
    #[test]
    fn parse_factor_absent_dims_are_none() {
        assert_eq!(
            parse_factor(&serde_json::json!({"alu": 2.0})),
            [Some(2.0), None, None]
        );
        assert_eq!(
            parse_factor(&serde_json::json!({"alu": 0.5, "membw": 0.8, "ioseq": 1.2})),
            [Some(0.5), Some(0.8), Some(1.2)]
        );
        assert_eq!(parse_factor(&serde_json::json!({})), [None; K]);
    }

    /// MAD-reject discards a single per-tenant outlier before the
    /// tenant-median is taken; the cross-tenant median then ignores
    /// the outlier entirely. A row's `None` dim never triggers
    /// rejection (unmeasured ≠ out-of-band).
    #[test]
    fn mad_reject_discards_tenant_outlier() {
        let v = [
            s([1.0, 1.0, 1.0]),
            s([1.1, 1.0, 1.0]),
            s([0.9, 1.0, 1.0]),
            s([1.05, 1.0, 1.0]),
            s([50.0, 1.0, 1.0]),
        ];
        let m = mad_reject_median(&v);
        assert!(
            (m[0].unwrap() - 1.05).abs() < 0.2,
            "outlier 50.0 rejected, got {m:?}"
        );
        // None dim doesn't trigger rejection: alu-only row at 1.0
        // survives the MAD pass even though membw/ioseq are absent.
        let mixed = [
            [Some(1.0), None, None],
            s([1.1, 2.0, 2.0]),
            s([0.9, 2.0, 2.0]),
        ];
        let m = mad_reject_median(&mixed);
        assert_eq!(m[0], Some(1.0), "None-dim row not rejected");
        assert_eq!(m[1], Some(2.0));
    }

    /// ADR-023 §Threat-model gap (b) raised the floor 2→5: with 2
    /// tenants the "fleet median" is one of them.
    #[test]
    fn fleet_median_min_tenants_is_5() {
        assert_eq!(crate::sla::FLEET_MEDIAN_MIN_TENANTS, 5);
    }

    /// bug_009: `tenants_with_dim` MUST be the post-MAD-reject
    /// population — the same vector `cross_tenant_median`'s
    /// `min_tenants` gate reads. Divergent tenant t4's only
    /// `Some(membw)` row is MAD-rejected for an alu outlier; its
    /// tenant-median is `[Some, None, None]`. Pre-fix `aggregate`
    /// counted raw rows (`any(|r| r[membw].is_some())` → true via the
    /// rejected row) and reported `tenants_with_dim[membw]==5`; the
    /// scheduler's gate saw `xs.len()==4<5` → calibration deadlock.
    #[test]
    // r[verify sched.sla.threat.hw-median-of-medians]
    fn cross_tenant_median_count_is_post_mad() {
        let row_k = |pod: &str, tenant: &str, f: [Option<f64>; K]| HwPerfSampleRow {
            hw_class: "h".into(),
            pod_id: pod.into(),
            submitting_tenant: Some(tenant.into()),
            factor: f,
        };
        let mut rows = Vec::new();
        // 4 honest tenants: 1 K=3 row each at the reference cluster.
        for t in 0..4 {
            rows.push(row_k(
                &format!("p{t}"),
                &format!("t{t}"),
                s([1.0, 1.0, 1.0]),
            ));
        }
        // Divergent tenant t4: 2 alu-only rows clustered at ~1.0 + 1
        // K=3 row with alu=10·med. MAD: med[alu]=1.0, mad[alu]=0.02,
        // threshold≈0.089 → the K=3 row is rejected; kept rows are
        // alu-only → t4's tenant-median = [Some(1.0), None, None].
        rows.push(row_k("p4a", "t4", [Some(0.98), None, None]));
        rows.push(row_k("p4b", "t4", [Some(1.0), None, None]));
        rows.push(row_k("p4c", "t4", [Some(10.0), Some(2.0), Some(2.0)]));

        let agg = HwTable::aggregate(&rows);
        assert_eq!(
            agg["h"].tenants_with_dim[1], 4,
            "membw: t4's only Some(membw) row was MAD-rejected → t4 \
             contributes None → 4 tenants, not 5 (count == gate population)"
        );
        assert_eq!(
            agg["h"].tenants_with_dim[2], 4,
            "ioseq: same row carried the only Some(ioseq) for t4"
        );
        assert_eq!(agg["h"].tenants_with_dim[0], 5, "alu: all 5 contribute");
        // Gate sees the same 4: factor[membw] pinned at 1.0.
        assert_eq!(agg["h"].factor[1], 1.0, "membw: 4 < min_tenants=5 → gated");
    }
}
