//! See rio-scheduler/tests/metrics_registered.rs for rationale.

use rio_test_support::metrics::{assert_emitted_metrics_described, assert_spec_metrics_described};

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
