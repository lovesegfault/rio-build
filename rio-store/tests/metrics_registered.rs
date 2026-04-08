//! See rio-scheduler/tests/metrics_registered.rs for rationale.
//!
//! Store's `describe_metrics()` also contains two pre-register
//! `metrics::gauge!().set(0.0)` calls (s3_deletes_pending/stuck) —
//! the build-script grep picks those up from lib.rs, and they ARE
//! described, so they pass. Not a false positive.
//!
//! Histogram-bucket check: I-156 — `rio_store_put_path_duration_seconds`
//! shipped without a `HISTOGRAM_BUCKET_MAP` entry. Before `init_metrics`
//! installed a global `set_buckets()` default, that meant SUMMARY mode
//! (no `_bucket` series), and the dashboard's `histogram_quantile()`
//! PutPath p99 panel showed "No data".

// r[verify obs.metric.store]
rio_test_support::metrics_suite! {
    describe_fn: rio_store::describe_metrics,
    crate_name: "rio-store",
    spec_floor: 12,
    emit_floor: 15,
    default_buckets_ok: [
        // Inline NARs ~5-50ms (PG round-trip), chunked S3 ~100ms-10s.
        // [0.005..10.0] fits.
        "rio_store_put_path_duration_seconds",
    ],
}
