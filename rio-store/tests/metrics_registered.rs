//! See rio-scheduler/tests/metrics_registered.rs for rationale.
//!
//! Store's `describe_metrics()` also contains two pre-register
//! `metrics::gauge!().set(0.0)` calls (s3_deletes_pending/stuck) —
//! the build-script grep picks those up from lib.rs, and they ARE
//! described, so they pass. Not a false positive.

use rio_test_support::metrics::{assert_emitted_metrics_described, assert_spec_metrics_described};

/// Metric names from observability.md's Store Metrics table.
const STORE_METRICS: &[&str] = &[
    "rio_store_put_path_total",
    "rio_store_put_path_duration_seconds",
    "rio_store_put_path_bytes_total",
    "rio_store_get_path_bytes_total",
    "rio_store_integrity_failures_total",
    "rio_store_chunks_total",
    "rio_store_chunk_dedup_ratio",
    "rio_store_s3_requests_total",
    "rio_store_chunk_cache_hits_total",
    "rio_store_chunk_cache_misses_total",
    "rio_store_gc_path_resurrected_total",
    "rio_store_hmac_rejected_total",
    "rio_store_hmac_bypass_total",
    "rio_store_s3_deletes_pending",
    "rio_store_s3_deletes_stuck",
    "rio_store_gc_chunk_resurrected_total",
    "rio_store_gc_path_swept_total",
    "rio_store_gc_s3_key_enqueued_total",
];

const EMITTED_METRICS: &str = include_str!(concat!(env!("OUT_DIR"), "/emitted_metrics.txt"));

// r[verify obs.metric.store]
#[test]
fn all_spec_metrics_have_describe_call() {
    assert_spec_metrics_described(STORE_METRICS, rio_store::describe_metrics, "rio-store");
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
