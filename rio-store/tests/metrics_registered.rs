//! See rio-scheduler/tests/metrics_registered.rs for rationale.
//!
//! Store's `describe_metrics()` also contains two pre-register
//! `metrics::gauge!().set(0.0)` calls (s3_deletes_pending/stuck) —
//! the build-script grep picks those up from lib.rs, and they ARE
//! described, so they pass. Not a false positive.

use rio_test_support::metrics::{
    assert_emitted_metrics_described, assert_histograms_have_buckets, assert_spec_metrics_described,
};

/// Metric names from observability.md's Store Metrics table.
/// Derived at build time via build.rs → spec_metrics.txt.
const SPEC_METRICS_RAW: &str = include_str!(concat!(env!("OUT_DIR"), "/spec_metrics.txt"));

const EMITTED_METRICS: &str = include_str!(concat!(env!("OUT_DIR"), "/emitted_metrics.txt"));

// r[verify obs.metric.store]
#[test]
fn all_spec_metrics_have_describe_call() {
    let spec_metrics: Vec<&str> = SPEC_METRICS_RAW.lines().filter(|l| !l.is_empty()).collect();
    // Floor-check: obs.md's Store Metrics table has ≥12 rows.
    // Guards against vacuous pass if the grep path breaks.
    assert!(
        spec_metrics.len() >= 12,
        "spec_metrics.txt has only {} entries — build.rs grep broken?",
        spec_metrics.len()
    );
    assert_spec_metrics_described(&spec_metrics, rio_store::describe_metrics, "rio-store");
}

// r[verify obs.metric.store]
#[test]
fn all_emitted_metrics_are_described() {
    assert_emitted_metrics_described(
        EMITTED_METRICS,
        15,
        rio_store::describe_metrics,
        "rio-store",
    );
}

// r[verify obs.metric.store]
// I-156: rio_store_put_path_duration_seconds shipped without a
// HISTOGRAM_BUCKET_MAP entry. Before init_metrics installed a global
// set_buckets() default, that meant SUMMARY mode (no _bucket series),
// and the dashboard's histogram_quantile() PutPath p99 panel showed
// "No data". This test was missing from rio-store entirely.
#[test]
fn all_histograms_have_bucket_config() {
    use rio_common::observability::HISTOGRAM_BUCKET_MAP;

    // rio_store_put_path_duration_seconds: inline NARs ~5-50ms (PG
    // round-trip), chunked S3 ~100ms-10s. [0.005..10.0] fits.
    const DEFAULT_BUCKETS_OK: &[&str] = &["rio_store_put_path_duration_seconds"];

    assert_histograms_have_buckets(
        rio_store::describe_metrics,
        HISTOGRAM_BUCKET_MAP,
        DEFAULT_BUCKETS_OK,
        "rio-store",
    );
}
