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

/// One `hw_perf_samples` row, jsonb `factor` parsed to `[f64; K]`.
/// `submitting_tenant` is `None` for pre-M_054 rows and for builders
/// without a tenant context (e.g. probe pods).
#[derive(Debug, Clone)]
pub struct HwPerfSampleRow {
    pub hw_class: String,
    pub pod_id: String,
    pub submitting_tenant: Option<String>,
    pub factor: [f64; K],
}

/// Aggregated per-hw_class factor: K=3 vector + distinct-pod count.
/// `factor` is `[1.0; K]` when `pod_ids < 3` (untrusted; pass-through).
#[derive(Debug, Clone, Copy)]
pub struct HwFactor {
    pub factor: [f64; K],
    pub pod_ids: u32,
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
    /// regardless of the ≥3 trust floor. For the `HwClassSampled`
    /// handler (A11): "this hw_class has N pod benches; ≥3 means
    /// `factor()` is trusted".
    pub fn distinct_pod_ids(&self, hw_class: &str) -> u32 {
        self.factors.get(hw_class).map(|f| f.pod_ids).unwrap_or(0)
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
        // hw_class → submitting_tenant → Vec<[f64;K]>
        let mut by_class: HashMap<String, HashMap<Option<String>, Vec<[f64; K]>>> = HashMap::new();
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
                let factor =
                    cross_tenant_median(&tenants, pod_ids, super::FLEET_MEDIAN_MIN_TENANTS);
                (h, HwFactor { factor, pod_ids })
            })
            .collect()
    }

    /// Snapshot `hw_perf_samples` (7-day window, mirroring the dropped
    /// `hw_perf_factors` view's recency filter) and [`Self::aggregate`].
    /// Legacy scalar rows (pre-M_054 backfill `{"alu": <f64>}`) default
    /// `membw`/`ioseq` to 1.0.
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
                .map(|(h, factor)| (h, HwFactor { factor, pod_ids: 3 }))
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
/// 1. **Pod gate:** `n_pods < HW_MIN_PODS` → `[1.0; K]`. With <3
///    distinct pods the per-tenant median is one or two samples.
/// 2. **Tenant gate:** `by_tenant.len() < min_tenants` → `[1.0; K]`. A
///    "median-of-tenant-medians" with one tenant IS that tenant's
///    median; with two, the fleet median is one of them. The caller
///    must supply `min_tenants` (no default) — see
///    [`super::FLEET_MEDIAN_MIN_TENANTS`].
/// 3. Per tenant: 3·MAD reject, then per-dimension median.
/// 4. Across tenants: per-dimension median.
/// 5. Clamp `[FLOOR, CEIL]` post-aggregation so MAD sees raw values.
///
/// `min_tenants` is a parameter (not a const reference) so a new
/// median-of-medians consumer (e.g. §13b's per-cell bias table) can't
/// silently skip the gate — the type forces them to supply a value.
pub(super) fn cross_tenant_median(
    by_tenant: &HashMap<Option<String>, Vec<[f64; K]>>,
    n_pods: u32,
    min_tenants: usize,
) -> [f64; K] {
    if n_pods < HW_MIN_PODS || by_tenant.len() < min_tenants {
        return [1.0; K];
    }
    let tenant_medians: Vec<[f64; K]> = by_tenant.values().map(|v| mad_reject_median(v)).collect();
    let mut m = per_dim_median(&tenant_medians);
    for d in &mut m {
        *d = d.clamp(HW_FACTOR_SANITY_FLOOR, HW_FACTOR_SANITY_CEIL);
    }
    m
}

/// jsonb `{"alu":a,"membw":b,"ioseq":c}` → `[a,b,c]`. Missing keys
/// default to 1.0 (legacy scalar rows have only `alu` after M_054's
/// `USING jsonb_build_object('alu', factor)`).
fn parse_factor(j: &serde_json::Value) -> [f64; K] {
    let get = |k: &str| j.get(k).and_then(serde_json::Value::as_f64).unwrap_or(1.0);
    [get("alu"), get("membw"), get("ioseq")]
}

/// Per-dimension median of `v`. Upper-middle for even `n` (matches
/// `cost::interrupt_rate`'s convention). Empty → `[1.0; K]`.
fn per_dim_median(v: &[[f64; K]]) -> [f64; K] {
    if v.is_empty() {
        return [1.0; K];
    }
    let mut out = [0.0; K];
    for (d, slot) in out.iter_mut().enumerate() {
        let mut xs: Vec<f64> = v.iter().map(|r| r[d]).collect();
        xs.sort_by(f64::total_cmp);
        *slot = xs[xs.len() / 2];
    }
    out
}

/// Per-dimension 3·MAD outlier reject, then [`per_dim_median`] of
/// survivors. A row is rejected if **any** dimension's `|x − med| >
/// 3·1.4826·MAD` (a poisoned bench result poisons the whole vector).
/// `MAD == 0` → no rejection on that dimension.
fn mad_reject_median(v: &[[f64; K]]) -> [f64; K] {
    let med = per_dim_median(v);
    let mad: [f64; K] = {
        let dev: Vec<[f64; K]> = v
            .iter()
            .map(|r| std::array::from_fn(|d| (r[d] - med[d]).abs()))
            .collect();
        per_dim_median(&dev)
    };
    let kept: Vec<[f64; K]> = v
        .iter()
        .copied()
        .filter(|r| (0..K).all(|d| mad[d] == 0.0 || (r[d] - med[d]).abs() <= 3.0 * 1.4826 * mad[d]))
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
            factor: [alu, 1.0, 1.0],
        }
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
        let mut by_tenant: HashMap<Option<String>, Vec<[f64; K]>> = HashMap::new();
        for t in 0..5 {
            by_tenant.insert(Some(format!("t{t}")), vec![[1.5, 1.0, 1.0]]);
        }
        // Both gates pass → median.
        assert_eq!(cross_tenant_median(&by_tenant, 5, 5), [1.5, 1.0, 1.0]);
        // Pod gate trips.
        assert_eq!(cross_tenant_median(&by_tenant, 2, 5), [1.0; K]);
        // Tenant gate trips (caller asked for 6).
        assert_eq!(cross_tenant_median(&by_tenant, 5, 6), [1.0; K]);
        // Clamp applies post-median.
        let mut huge: HashMap<Option<String>, Vec<[f64; K]>> = HashMap::new();
        for t in 0..5 {
            huge.insert(Some(format!("t{t}")), vec![[1e6, 1.0, 1.0]]);
        }
        assert_eq!(
            cross_tenant_median(&huge, 5, 5)[0],
            HW_FACTOR_SANITY_CEIL,
            "chokepoint clamp"
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
    }

    /// Legacy `{"alu": x}` rows default `membw`/`ioseq` to 1.0.
    #[test]
    fn parse_factor_defaults_missing_dims() {
        assert_eq!(
            parse_factor(&serde_json::json!({"alu": 2.0})),
            [2.0, 1.0, 1.0]
        );
        assert_eq!(
            parse_factor(&serde_json::json!({"alu": 0.5, "membw": 0.8, "ioseq": 1.2})),
            [0.5, 0.8, 1.2]
        );
        assert_eq!(parse_factor(&serde_json::json!({})), [1.0, 1.0, 1.0]);
    }

    /// MAD-reject discards a single per-tenant outlier before the
    /// tenant-median is taken; the cross-tenant median then ignores
    /// the outlier entirely.
    #[test]
    fn mad_reject_discards_tenant_outlier() {
        let v = [
            [1.0, 1.0, 1.0],
            [1.1, 1.0, 1.0],
            [0.9, 1.0, 1.0],
            [1.05, 1.0, 1.0],
            [50.0, 1.0, 1.0],
        ];
        let m = mad_reject_median(&v);
        assert!(
            (m[0] - 1.05).abs() < 0.2,
            "outlier 50.0 rejected, got {m:?}"
        );
    }

    /// ADR-023 §Threat-model gap (b) raised the floor 2→5: with 2
    /// tenants the "fleet median" is one of them.
    #[test]
    fn fleet_median_min_tenants_is_5() {
        assert_eq!(crate::sla::FLEET_MEDIAN_MIN_TENANTS, 5);
    }
}
