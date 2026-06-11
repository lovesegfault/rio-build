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
        "rio_gateway_build_resync_rate_paced_total",
        "WatchBuild re-attach cycles paced by the wall-clock rate axis (token \
         bucket: RATE_MAX cycles per RATE_WINDOW sustained), labeled by the \
         death cause that charged the cycle (cause: resync_signal | transport \
         | eof_without_terminal | reattach_cycle_failed). Every death arm pays \
         this axis through the one next_backoff chokepoint (merged_bug_083); \
         refill is time-based, so paced cycles cannot drain the evidence. \
         Sustained growth means a durably-slow event consumer (or undersized \
         broadcast buffer) charging the scheduler one O(DAG) snapshot per \
         cycle -- find the slow consumer, not a scheduler outage."
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

    // r[impl obs.metric.alert-counter-seeded]
    seed_alert_counters();
}

/// One boot-seeded counter family (name + closed label axis). Mirrors
/// `rio_test_support::metrics::SeededCounter` so the alert-parity test
/// (tests/alert_metrics.rs) consumes this exact table — the store/
/// scheduler/controller pattern, adopted when the store ScaledObject's
/// abort-aware trigger made `rio_gateway_putpath_aborted_retries_total`
/// the gateway's first alert-referenced counter.
pub struct SeededSeries {
    pub name: &'static str,
    pub label: Option<(&'static str, &'static [&'static str])>,
}

/// The `attempt` label axis of `rio_gateway_putpath_aborted_retries_total`:
/// the emit site charges `attempt.to_string()` for attempt in
/// 1..=`PUT_PATH_ABORTED_MAX_ATTEMPTS` (handler/grpc.rs — attempt is
/// incremented before the emit, and the budget check caps it at MAX).
/// The closed production set, pinned to the const by
/// `putpath_retry_attempt_axis_matches_the_emit_law`.
pub const PUTPATH_ABORTED_RETRY_ATTEMPTS: &[&str] = &["1", "2", "3", "4", "5", "6", "7", "8"];

/// Every alert-`expr:`-referenced rio_gateway counter, born at 0 at
/// boot on every replica (the parity test fails when a
/// PrometheusRule/ScaledObject references a counter missing here).
/// The store ScaledObject's demand-side inhibitor trigger
/// (`sum(rate(rio_gateway_putpath_aborted_retries_total[2m]))`) is the
/// founding member: a scale-down inhibitor evaluating an absent series
/// until the first abort is exactly the birth-gap class (bug_322) the
/// seed table exists to kill.
pub const ALERT_SEEDED_COUNTERS: &[SeededSeries] = &[SeededSeries {
    name: "rio_gateway_putpath_aborted_retries_total",
    label: Some(("attempt", PUTPATH_ABORTED_RETRY_ATTEMPTS)),
}];

/// Birth every [`ALERT_SEEDED_COUNTERS`] series at 0 (tail of
/// [`describe_metrics`] — `rio_common::server::bootstrap` installs the
/// exporter immediately before, so the seeds land on the scrape
/// surface).
fn seed_alert_counters() {
    for s in ALERT_SEEDED_COUNTERS {
        match s.label {
            None => metrics::counter!(s.name).absolute(0),
            Some((axis, values)) => {
                for v in values {
                    metrics::counter!(s.name, axis => *v).absolute(0);
                }
            }
        }
    }
}

#[cfg(test)]
mod alert_seed_tests {
    use super::*;

    /// The seeded `attempt` axis IS the emit law's value set:
    /// `(1..=PUT_PATH_ABORTED_MAX_ATTEMPTS).to_string()` — machine-
    /// derived from the same const the emit site bounds itself by, so
    /// widening the retry budget without widening the seed product
    /// fails here instead of leaving the new attempt series birth-
    /// gapped.
    #[test]
    fn putpath_retry_attempt_axis_matches_the_emit_law() {
        let expect: Vec<String> = (1..=crate::handler::grpc::PUT_PATH_ABORTED_MAX_ATTEMPTS)
            .map(|a| a.to_string())
            .collect();
        let got: Vec<&str> = PUTPATH_ABORTED_RETRY_ATTEMPTS.to_vec();
        assert_eq!(
            got,
            expect.iter().map(String::as_str).collect::<Vec<_>>(),
            "PUTPATH_ABORTED_RETRY_ATTEMPTS must equal the emit site's \
             1..=PUT_PATH_ABORTED_MAX_ATTEMPTS string product"
        );
    }
}
