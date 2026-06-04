//! SSH gateway and Nix protocol frontend for rio-build.
//!
//! Terminates SSH connections, speaks the Nix worker protocol, and
//! translates protocol operations into gRPC calls to the scheduler
//! and store services.

pub mod config;
pub mod drv_cache;
pub mod handler;
pub(crate) mod quota;
pub(crate) mod ratelimit;
pub mod server;
pub mod session;
pub(crate) mod translate;

pub use quota::QuotaCache;
pub use ratelimit::{RateLimitConfig, TenantLimiter};
pub use server::{
    AUTHORIZED_KEYS_POLL_INTERVAL, GatewayServer, load_authorized_keys, load_or_generate_host_key,
    spawn_authorized_keys_watcher,
};

/// Per-crate histogram bucket overrides, passed to
/// `rio_common::server::bootstrap` → `init_metrics`.
///
/// Gateway has no histograms needing custom buckets — all are
/// HTTP/SSH-latency-shaped and fit the global `[0.005..10.0]` default.
pub const HISTOGRAM_BUCKETS: &[(&str, &[f64])] = &[];

/// Registers prometheus metric descriptions. The help strings here are
/// the source for `docs/ref/metrics.typ` — see
/// `xtask/src/regen/docs_data.rs::metrics()` for the data-flow.
// r[impl obs.metric.gateway]
pub fn describe_metrics() {
    use metrics::{describe_counter, describe_gauge, describe_histogram};

    describe_counter!(
        "rio_gateway_connections_total",
        "Total SSH connections (labeled by result: new/accepted/rejected/rejected_jwt)"
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
        "Currently active protocol sessions (one per accepted nix-daemon exec, \
         not SSH-level open channels); the gateway autoscaling signal"
    );
    describe_counter!(
        "rio_gateway_errors_total",
        "Protocol errors (labeled by type; type=session also carries \
         stage=tcp-accepted|auth-attempted|authenticated|channel-open)"
    );
    describe_counter!(
        "rio_gateway_bytes_total",
        "Bytes forwarded to/from SSH client (labeled by direction: rx/tx)"
    );
    describe_counter!(
        "rio_gateway_jwt_mint_degraded_total",
        "JWT mint failed but jwt.required=false, degraded to tenant_name fallback"
    );
    describe_counter!(
        "rio_gateway_jwt_refreshed_total",
        "Session JWT re-minted on a long-lived SSH connection (cached token near expiry)"
    );
    describe_counter!(
        "rio_gateway_jwt_refresh_failed_total",
        "Session JWT re-mint failed; stale token kept (downstream will reject ExpiredSignature)"
    );
    describe_counter!(
        "rio_gateway_auth_degraded_total",
        "SSH auth accepted but tenant identity degraded to single-tenant mode \
         (labeled by reason: interior_whitespace = authorized_keys comment has \
         a space where a dash was intended; invalid_utf8 = comment bytes are \
         not valid UTF-8 — rewrite the comment as plain UTF-8)"
    );
    describe_counter!(
        "rio_gateway_quota_rejections_total",
        "SubmitBuild rejected because tenant is over store quota (labeled by tenant)"
    );
    describe_counter!(
        "rio_gateway_putpath_aborted_retries_total",
        "PutPath retries on store Code::Aborted (labeled by attempt). \
         attempt=PUT_PATH_ABORTED_MAX_ATTEMPTS means budget exhausted and the \
         error surfaced to the client (I-168)."
    );
    describe_counter!(
        "rio_gateway_log_tail_reconnects_total",
        "Build-log TailLog subscriptions re-opened against rio-store (labeled by \
         reason: open_failed = the TailLog RPC itself was rejected, the live \
         tail is dark until the store is reachable; stream_ended = an \
         established stream closed before the derivation finished, normal \
         during a store deploy; gap_observed = the store's stream jumped past \
         the relay floor and the subscription re-opened at the gap to give the \
         missing span one more chance — occasional increments are normal under \
         tail fan-out drops, a sustained rate on one derivation means its \
         stored log has a hole, check rio_store_log_read_data_loss_total). A \
         sustained open_failed rate means every watched build's live tail is \
         degraded fleet-wide; the lines remain durable in the store and \
         readable via `rio-cli logs` regardless."
    );
}
