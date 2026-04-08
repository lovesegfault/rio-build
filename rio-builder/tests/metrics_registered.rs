//! See rio-scheduler/tests/metrics_registered.rs for rationale.
//!
//! Histogram-bucket check catches `rio_builder_upload_references_count`
//! shipping with no `HISTOGRAM_BUCKET_MAP` entry (P0363 — same bug
//! class as P0321's `build_graph_edges`). Every sample with >10 refs
//! was landing in `+Inf`.

// r[verify obs.metric.builder]
rio_test_support::metrics_suite! {
    describe_fn: rio_builder::describe_metrics,
    crate_name: "rio-builder",
    spec_floor: 10,
    emit_floor: 15,
    default_buckets_ok: [
        // gRPC GetPath + stream drain — sub-second typical, [0.005..10.0] fits.
        "rio_builder_fuse_fetch_duration_seconds",
    ],
}
