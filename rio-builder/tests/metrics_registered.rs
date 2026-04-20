//! See rio-scheduler/tests/metrics_registered.rs for rationale.
//!
//! Histogram-bucket check catches `rio_builder_upload_references_count`
//! shipping with no `HISTOGRAM_BUCKETS` entry (P0363 — same bug
//! class as P0321's `build_graph_edges`). Every sample with >10 refs
//! was landing in `+Inf`.

// r[verify obs.metric.builder]
rio_test_support::metrics_suite! {
    describe_fn: rio_builder::describe_metrics,
    crate_name: "rio-builder",
    prefix: "rio_builder_",
    histogram_buckets: rio_builder::HISTOGRAM_BUCKETS,
    spec_floor: 10,
    emit_floor: 15,
    default_buckets_ok: [],
}
