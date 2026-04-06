//! See rio-scheduler/tests/metrics_registered.rs for rationale.

use rio_test_support::metrics::{assert_emitted_metrics_described, assert_spec_metrics_described};

/// Metric names from observability.md's Gateway Metrics table.
const GATEWAY_METRICS: &[&str] = &[
    "rio_gateway_connections_total",
    "rio_gateway_connections_active",
    "rio_gateway_opcodes_total",
    "rio_gateway_opcode_duration_seconds",
    "rio_gateway_handshakes_total",
    "rio_gateway_channels_active",
    "rio_gateway_errors_total",
    "rio_gateway_bytes_total",
];

const EMITTED_METRICS: &str = include_str!(concat!(env!("OUT_DIR"), "/emitted_metrics.txt"));

// r[verify obs.metric.gateway]
#[test]
fn all_spec_metrics_have_describe_call() {
    assert_spec_metrics_described(
        GATEWAY_METRICS,
        rio_gateway::describe_metrics,
        "rio-gateway",
    );
}

// r[verify obs.metric.gateway]
#[test]
fn all_emitted_metrics_are_described() {
    assert_emitted_metrics_described(
        EMITTED_METRICS,
        5,
        rio_gateway::describe_metrics,
        "rio-gateway",
    );
}
