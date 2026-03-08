//! SSH gateway and Nix protocol frontend for rio-build.
//!
//! Terminates SSH connections, speaks the Nix worker protocol, and
//! translates protocol operations into gRPC calls to the scheduler
//! and store services.

pub mod handler;
pub mod server;
pub mod session;
pub mod translate;

pub use server::{GatewayServer, load_authorized_keys, load_or_generate_host_key};

/// Register `# HELP` descriptions for all gateway metrics.
///
/// Call from `main()` immediately after `init_metrics()`. Without this,
/// `/metrics` serves bare names with no `# HELP` lines — Grafana UIs and
/// `promtool check metrics` have nothing to show. Descriptions sourced
/// from docs/src/observability.md (the Gateway Metrics table).
///
/// `metrics::describe_*!` are fire-and-forget: they register metadata with
/// whatever recorder is installed. Safe to call before or after the metric
/// is first emitted; the exporter merges description with value at scrape
/// time. Calling twice is a no-op (first description wins).
// r[impl obs.metric.gateway]
pub fn describe_metrics() {
    use metrics::{describe_counter, describe_gauge, describe_histogram};

    describe_counter!(
        "rio_gateway_connections_total",
        "Total SSH connections (labeled by result: new/accepted/rejected)"
    );
    describe_gauge!(
        "rio_gateway_connections_active",
        "Currently active SSH connections"
    );
    describe_counter!(
        "rio_gateway_opcodes_total",
        "Protocol opcodes handled (labeled by opcode name)"
    );
    describe_histogram!(
        "rio_gateway_opcode_duration_seconds",
        "Per-opcode handling latency"
    );
    describe_counter!(
        "rio_gateway_handshakes_total",
        "Protocol handshakes completed (labeled by result: success/rejected/failure)"
    );
    describe_gauge!(
        "rio_gateway_channels_active",
        "Currently active SSH channels"
    );
    describe_counter!(
        "rio_gateway_errors_total",
        "Protocol errors (labeled by type)"
    );
}
