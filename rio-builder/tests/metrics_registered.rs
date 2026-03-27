//! See rio-scheduler/tests/metrics_registered.rs for rationale.

use rio_test_support::metrics::{
    assert_emitted_metrics_described, assert_histograms_have_buckets, assert_spec_metrics_described,
};

/// Metric names from observability.md's Worker Metrics table.
/// Derived at build time via build.rs → spec_metrics.txt.
const SPEC_METRICS_RAW: &str = include_str!(concat!(env!("OUT_DIR"), "/spec_metrics.txt"));

const EMITTED_METRICS: &str = include_str!(concat!(env!("OUT_DIR"), "/emitted_metrics.txt"));

// r[verify obs.metric.builder]
#[test]
fn all_spec_metrics_have_describe_call() {
    let spec_metrics: Vec<&str> = SPEC_METRICS_RAW.lines().filter(|l| !l.is_empty()).collect();
    // Floor-check: obs.md's Worker Metrics table has ≥10 rows.
    // Guards against vacuous pass if the grep path breaks.
    assert!(
        spec_metrics.len() >= 10,
        "spec_metrics.txt has only {} entries — build.rs grep broken?",
        spec_metrics.len()
    );
    assert_spec_metrics_described(&spec_metrics, rio_builder::describe_metrics, "rio-builder");
}

// r[verify obs.metric.builder]
#[test]
fn all_emitted_metrics_are_described() {
    assert_emitted_metrics_described(
        EMITTED_METRICS,
        15,
        rio_builder::describe_metrics,
        "rio-builder",
    );
}

// r[verify obs.metric.builder]
// Catches rio_builder_upload_references_count shipping with no
// HISTOGRAM_BUCKET_MAP entry (P0363 — same bug class as P0321's
// build_graph_edges). Every sample with >10 refs was landing in +Inf.
// The describe-only test above doesn't catch this: the metric was
// described, emitted, and scraped fine — just with useless buckets.
#[test]
fn all_histograms_have_bucket_config() {
    use rio_common::observability::HISTOGRAM_BUCKET_MAP;

    // rio_builder_fuse_fetch_duration_seconds: gRPC GetPath + stream
    // drain — sub-second typical, [0.005..10.0] fits.
    const DEFAULT_BUCKETS_OK: &[&str] = &["rio_builder_fuse_fetch_duration_seconds"];

    assert_histograms_have_buckets(
        rio_builder::describe_metrics,
        HISTOGRAM_BUCKET_MAP,
        DEFAULT_BUCKETS_OK,
        "rio-builder",
    );
}
