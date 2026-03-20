//! See rio-scheduler/tests/metrics_registered.rs for rationale.

use rio_test_support::metrics::{
    assert_emitted_metrics_described, assert_histograms_have_buckets, assert_spec_metrics_described,
};

/// Metric names from observability.md's Worker Metrics table.
const WORKER_METRICS: &[&str] = &[
    "rio_worker_builds_total",
    "rio_worker_builds_active",
    "rio_worker_uploads_total",
    "rio_worker_build_duration_seconds",
    "rio_worker_fuse_cache_size_bytes",
    "rio_worker_fuse_cache_hits_total",
    "rio_worker_fuse_cache_misses_total",
    "rio_worker_fuse_fetch_duration_seconds",
    "rio_worker_fuse_fallback_reads_total", // ← the one remediation §2a adds
    "rio_worker_overlay_teardown_failures_total",
    "rio_worker_prefetch_total",
    "rio_worker_upload_bytes_total",
    "rio_worker_fuse_fetch_bytes_total",
    "rio_worker_fuse_circuit_open",
    "rio_worker_cpu_fraction",
    "rio_worker_memory_fraction",
];

const EMITTED_METRICS: &str = include_str!(concat!(env!("OUT_DIR"), "/emitted_metrics.txt"));

// r[verify obs.metric.worker]
#[test]
fn all_spec_metrics_have_describe_call() {
    assert_spec_metrics_described(WORKER_METRICS, rio_worker::describe_metrics, "rio-worker");
}

// r[verify obs.metric.worker]
#[test]
fn all_emitted_metrics_are_described() {
    assert_emitted_metrics_described(
        EMITTED_METRICS,
        15,
        rio_worker::describe_metrics,
        "rio-worker",
    );
}

// r[verify obs.metric.worker]
// Catches rio_worker_upload_references_count shipping with no
// HISTOGRAM_BUCKET_MAP entry (P0363 — same bug class as P0321's
// build_graph_edges). Every sample with >10 refs was landing in +Inf.
// The describe-only test above doesn't catch this: the metric was
// described, emitted, and scraped fine — just with useless buckets.
#[test]
fn all_histograms_have_bucket_config() {
    use rio_common::observability::HISTOGRAM_BUCKET_MAP;

    // rio_worker_fuse_fetch_duration_seconds: gRPC GetPath + stream
    // drain — sub-second typical, [0.005..10.0] fits.
    const DEFAULT_BUCKETS_OK: &[&str] = &["rio_worker_fuse_fetch_duration_seconds"];

    assert_histograms_have_buckets(
        rio_worker::describe_metrics,
        HISTOGRAM_BUCKET_MAP,
        DEFAULT_BUCKETS_OK,
        "rio-worker",
    );
}
