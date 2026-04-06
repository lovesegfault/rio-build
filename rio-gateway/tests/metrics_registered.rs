//! See rio-scheduler/tests/metrics_registered.rs for rationale.

use rio_test_support::metrics::{
    assert_emitted_metrics_described, assert_histograms_have_buckets, assert_spec_metrics_described,
};

/// Metric names from observability.md's Gateway Metrics table.
/// Derived at build time via build.rs → spec_metrics.txt.
const SPEC_METRICS_RAW: &str = include_str!(concat!(env!("OUT_DIR"), "/spec_metrics.txt"));

const EMITTED_METRICS: &str = include_str!(concat!(env!("OUT_DIR"), "/emitted_metrics.txt"));

// r[verify obs.metric.gateway]
#[test]
fn all_spec_metrics_have_describe_call() {
    let spec_metrics: Vec<&str> = SPEC_METRICS_RAW.lines().filter(|l| !l.is_empty()).collect();
    // Floor-check: obs.md's Gateway Metrics table has ≥8 rows.
    // If the build.rs grep path breaks (obs.md moved, table
    // reformatted), spec_metrics drops to 0 → test passes
    // vacuously. This guard catches that class.
    assert!(
        spec_metrics.len() >= 8,
        "spec_metrics.txt has only {} entries — build.rs grep broken?",
        spec_metrics.len()
    );
    assert_spec_metrics_described(&spec_metrics, rio_gateway::describe_metrics, "rio-gateway");
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

// r[verify obs.metric.gateway]
#[test]
fn all_histograms_have_bucket_config() {
    use rio_common::observability::HISTOGRAM_BUCKET_MAP;

    // rio_gateway_opcode_duration_seconds: per-opcode latency. The
    // fast opcodes (QueryPathInfo, IsValidPath, …) are sub-second
    // gRPC round-trips and [0.005..10.0] fits. The build opcodes
    // (BuildDerivation, BuildPathsWithResults) span minutes-hours,
    // but those durations are tracked by
    // rio_scheduler_build_duration_seconds with proper buckets — the
    // opcode histogram's job is the FAST opcodes' p99, where +Inf for
    // build opcodes is acceptable noise.
    const DEFAULT_BUCKETS_OK: &[&str] = &["rio_gateway_opcode_duration_seconds"];

    assert_histograms_have_buckets(
        rio_gateway::describe_metrics,
        HISTOGRAM_BUCKET_MAP,
        DEFAULT_BUCKETS_OK,
        "rio-gateway",
    );
}
