//! See rio-scheduler/tests/metrics_registered.rs for rationale.

use rio_test_support::metrics::{assert_emitted_metrics_described, assert_spec_metrics_described};

/// Metric names from observability.md's Controller Metrics table.
const CONTROLLER_METRICS: &[&str] = &[
    "rio_controller_reconcile_duration_seconds",
    "rio_controller_reconcile_errors_total",
    "rio_controller_scaling_decisions_total",
    "rio_controller_workerpool_replicas",
    "rio_controller_gc_runs_total",
];

const EMITTED_METRICS: &str = include_str!(concat!(env!("OUT_DIR"), "/emitted_metrics.txt"));

// r[verify obs.metric.controller]
#[test]
fn all_spec_metrics_have_describe_call() {
    assert_spec_metrics_described(
        CONTROLLER_METRICS,
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
