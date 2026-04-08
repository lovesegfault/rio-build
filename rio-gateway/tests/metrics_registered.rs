//! See rio-scheduler/tests/metrics_registered.rs for rationale.

// r[verify obs.metric.gateway]
rio_test_support::metrics_suite! {
    describe_fn: rio_gateway::describe_metrics,
    crate_name: "rio-gateway",
    spec_floor: 8,
    emit_floor: 5,
    default_buckets_ok: [
        // Per-opcode latency. The fast opcodes (QueryPathInfo,
        // IsValidPath, …) are sub-second gRPC round-trips and
        // [0.005..10.0] fits. The build opcodes (BuildDerivation,
        // BuildPathsWithResults) span minutes-hours, but those
        // durations are tracked by rio_scheduler_build_duration_seconds
        // with proper buckets — the opcode histogram's job is the FAST
        // opcodes' p99, where +Inf for build opcodes is acceptable noise.
        "rio_gateway_opcode_duration_seconds",
    ],
}
