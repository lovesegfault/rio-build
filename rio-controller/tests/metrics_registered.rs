//! See rio-scheduler/tests/metrics_registered.rs for rationale.

// r[verify obs.metric.controller+2]
// r[verify obs.metric.consolidate-threshold]
rio_test_support::metrics_suite! {
    describe_fn: rio_controller::describe_metrics,
    crate_name: "rio-controller",
    prefix: "rio_controller_",
    // Tight floor = current count; catches accidental row-delete.
    // Bump intentionally when adding a metric.
    histogram_buckets: rio_controller::HISTOGRAM_BUCKETS,
    spec_floor: 6,
    emit_floor: 3,
    default_buckets_ok: [],
}

// r[verify obs.metric.pg-iam]
#[test]
fn pg_iam_shared_family_described() {
    // The shared rio_pg_iam_* family escapes the per-crate prefix
    // filter above (different prefix), so assert it explicitly: each
    // PG consumer must register it from its own describe_metrics().
    rio_test_support::metrics::assert_spec_metrics_described(
        &[
            "rio_pg_iam_mint_failures_total",
            "rio_pg_iam_token_minted_timestamp_seconds",
        ],
        rio_controller::describe_metrics,
        "rio-controller",
    );
}
