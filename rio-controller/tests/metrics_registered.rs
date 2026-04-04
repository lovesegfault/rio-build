//! See rio-scheduler/tests/metrics_registered.rs for rationale.

use rio_test_support::metrics::{
    assert_emitted_metrics_described, assert_histograms_have_buckets, assert_spec_metrics_described,
};

/// Metric names from observability.md's Controller Metrics table.
/// Derived at build time via build.rs → spec_metrics.txt.
const SPEC_METRICS_RAW: &str = include_str!(concat!(env!("OUT_DIR"), "/spec_metrics.txt"));

const EMITTED_METRICS: &str = include_str!(concat!(env!("OUT_DIR"), "/emitted_metrics.txt"));

// r[verify obs.metric.controller]
#[test]
fn all_spec_metrics_have_describe_call() {
    let spec_metrics: Vec<&str> = SPEC_METRICS_RAW.lines().filter(|l| !l.is_empty()).collect();
    // Floor-check: obs.md's Controller Metrics table has ≥6 unique
    // rows (tight floor = current count; catches accidental row-
    // delete). Guards against vacuous pass if the grep path breaks.
    // Bump intentionally when adding a metric.
    assert!(
        spec_metrics.len() >= 6,
        "spec_metrics.txt has only {} entries — build.rs grep broken?",
        spec_metrics.len()
    );
    assert_spec_metrics_described(
        &spec_metrics,
        rio_controller::describe_metrics,
        "rio-controller",
    );
}

// r[verify obs.metric.controller]
#[test]
fn all_emitted_metrics_are_described() {
    assert_emitted_metrics_described(
        EMITTED_METRICS,
        3,
        rio_controller::describe_metrics,
        "rio-controller",
    );
}

// r[verify obs.metric.controller]
#[test]
fn all_histograms_have_bucket_config() {
    use rio_common::observability::HISTOGRAM_BUCKET_MAP;

    assert_histograms_have_buckets(
        rio_controller::describe_metrics,
        HISTOGRAM_BUCKET_MAP,
        &[],
        "rio-controller",
    );
}
