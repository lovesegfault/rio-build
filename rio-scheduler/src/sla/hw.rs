//! ADR-023 §Hardware heterogeneity: reference-second normalization.
//!
//! `build_samples.duration_secs` is wall-clock on whatever hw_class the
//! pod ran on. Before fitting T(c), [`super::ingest::refit`] maps each
//! sample's wall-clock to the **reference timeline** via
//! [`HwTable::normalize`]: `wall × factor[hw_class]`, where `factor` is
//! the per-hw_class median microbench result from the `hw_perf_factors`
//! view (migration 041). A sample with no `hw_class` (NULL — old
//! executor / non-k8s / informer race) or an hw_class with <3 distinct
//! pod samples passes through at `factor=1.0`.
//!
//! Memory is NOT normalized: peak RSS is dominated by the workload, not
//! core throughput. ADR-023's M(c) is fitted on raw bytes.

use std::collections::HashMap;

use crate::db::SchedulerDb;

/// Sanity floor for [`HwTable::min_factor`]. A pathological
/// `hw_perf_samples` row (bad bench, clock skew) could yield a factor
/// near 0; `ref_secs / min_factor()` would then blow the deadline up
/// ~×100 (capped at 24h, but wasteful). Clamp at 0.25 — i.e., assume no
/// admitted hw_class is more than 4× slower than the reference.
const HW_FACTOR_SANITY_FLOOR: f64 = 0.25;

/// Per-hw_class median microbench factor. Built from the
/// `hw_perf_factors` view each [`super::SlaEstimator::refresh`] tick.
/// Cheap to clone (a few dozen entries).
// r[impl sched.sla.hw-ref-seconds]
#[derive(Debug, Clone, Default)]
pub struct HwTable {
    factors: HashMap<String, f64>,
    /// Slowest admitted hw_class — `factor` closest to 1.0 from below.
    /// Informational (SlaExplain); the normalization itself doesn't
    /// special-case it.
    pub reference: String,
}

impl HwTable {
    /// `wall_secs × factor[hw_class]`. Unknown / `None` hw_class →
    /// factor 1.0 (pass-through). Time-domain only — call on
    /// `duration_secs` and `cpu_seconds_total`, NOT on memory or disk.
    pub fn normalize(&self, wall_secs: f64, hw_class: Option<&str>) -> f64 {
        wall_secs
            * hw_class
                .and_then(|h| self.factors.get(h))
                .copied()
                .unwrap_or(1.0)
    }

    /// Factor for `hw_class`, or 1.0 if unknown / <3 distinct pods.
    /// Exposed for `ingest::hw_bias`'s per-(pname, hw_class)
    /// residual computation.
    pub fn factor(&self, hw_class: &str) -> f64 {
        self.factors.get(hw_class).copied().unwrap_or(1.0)
    }

    /// Iterate `(hw_class, factor)`. For [`super::cost`]'s
    /// per-band `h_dagger` scan.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &f64)> {
        self.factors.iter()
    }

    /// Smallest factor across all known hw_classes, or 1.0 when empty.
    /// `ref_secs / min_factor()` is the worst-case (slowest-node)
    /// wall-clock — used by `solve_intent_for`'s deadline de-norm so
    /// `activeDeadlineSeconds` budgets for the slowest band a pod could
    /// land on.
    pub fn min_factor(&self) -> f64 {
        self.factors
            .values()
            .copied()
            .min_by(f64::total_cmp)
            .unwrap_or(1.0)
            // Guard against pathological microbench rows; see const doc.
            .max(HW_FACTOR_SANITY_FLOOR)
    }

    /// Distinct hw_classes with ≥3 pod samples. For SlaStatus.
    pub fn len(&self) -> usize {
        self.factors.len()
    }

    /// `len() == 0`. Clippy `len_without_is_empty`.
    pub fn is_empty(&self) -> bool {
        self.factors.is_empty()
    }

    /// Snapshot the `hw_perf_factors` view. The view's `HAVING
    /// count(DISTINCT pod_id) >= 3` floor means a freshly-admitted
    /// hw_class is absent (factor 1.0) until three pods have benched.
    pub async fn load(db: &SchedulerDb) -> anyhow::Result<Self> {
        let rows: Vec<(String, f64)> =
            sqlx::query_as("SELECT hw_class, factor FROM hw_perf_factors")
                .fetch_all(db.pool())
                .await?;
        let factors: HashMap<String, f64> = rows.into_iter().collect();
        // reference = class whose factor is closest to 1.0 (the
        // calibration target). Ties → lexicographic for determinism.
        let reference = factors
            .iter()
            .min_by(|(ka, va), (kb, vb)| {
                (**va - 1.0)
                    .abs()
                    .partial_cmp(&(**vb - 1.0).abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| ka.cmp(kb))
            })
            .map(|(k, _)| k.clone())
            .unwrap_or_default();
        Ok(Self { factors, reference })
    }

    /// Test constructor: bypass PG.
    #[cfg(test)]
    pub fn from_map(factors: HashMap<String, f64>) -> Self {
        let reference = factors
            .iter()
            .min_by(|(_, a), (_, b)| {
                (**a - 1.0)
                    .abs()
                    .partial_cmp(&(**b - 1.0).abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(k, _)| k.clone())
            .unwrap_or_default();
        Self { factors, reference }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_passes_through_unknown() {
        let t = HwTable::default();
        assert_eq!(t.normalize(100.0, None), 100.0);
        assert_eq!(t.normalize(100.0, Some("aws-7-ebs")), 100.0);
    }

    #[test]
    fn normalize_scales_known() {
        let mut m = HashMap::new();
        m.insert("aws-5-ebs".into(), 1.0);
        m.insert("aws-8-nvme".into(), 2.0);
        let t = HwTable::from_map(m);
        // Fast hw (factor=2.0) ran in 50s wall → 100 reference-seconds.
        assert_eq!(t.normalize(50.0, Some("aws-8-nvme")), 100.0);
        // Reference hw (factor=1.0) ran in 100s wall → 100 ref-seconds.
        assert_eq!(t.normalize(100.0, Some("aws-5-ebs")), 100.0);
        assert_eq!(t.reference, "aws-5-ebs");
    }

    #[test]
    fn min_factor_clamped_at_sanity_floor() {
        let mut m = HashMap::new();
        m.insert("slow".into(), 0.01);
        let t = HwTable::from_map(m);
        assert_eq!(t.min_factor(), HW_FACTOR_SANITY_FLOOR);
        // Empty table → 1.0 (above floor, unchanged).
        assert_eq!(HwTable::default().min_factor(), 1.0);
    }
}
