//! See rio-scheduler/tests/metrics_registered.rs for rationale.

// r[verify obs.metric.controller]
rio_test_support::metrics_suite! {
    describe_fn: rio_controller::describe_metrics,
    crate_name: "rio-controller",
    // Tight floor = current count; catches accidental row-delete.
    // Bump intentionally when adding a metric.
    spec_floor: 6,
    emit_floor: 3,
    default_buckets_ok: [],
}
