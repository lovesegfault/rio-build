//! gRPC client transport: channel construction, h2 tuning, and the
//! generic [`ProtoClient`] connect helpers.
//!
//! - Connection: [`connect_single`] / [`connect`] build
//!   `"http(s)://{addr}"` from a `host:port` string, apply the
//!   process-global TLS config (see
//!   [`rio_common::grpc::init_client_tls`]), connect, and apply
//!   [`max_message_size`].
//! - Retry: [`connect_with_retry`] / [`connect_forever`] for
//!   shutdown-aware cold-start connect loops.
//!
//! `StoreService` data-plane helpers (NAR collect/chunk loops,
//! `QueryPathInfo`/`GetPath` wrappers) live in [`store`] and are
//! re-exported here so callers keep using `rio_proto::client::*`.

pub mod balance;
pub use balance::BalancedChannel;

pub mod retry;
pub use retry::{RetryError, connect_forever, connect_with_retry};

pub mod store;
pub use store::{
    NAR_CHUNK_SIZE, NarCollectError, batch_get_manifest, batch_query_path_info, chunk_nar_for_put,
    collect_nar_stream, collect_nar_stream_to_writer, get_path_nar, get_path_nar_to_file,
    query_path_info_opt,
};

use std::time::Duration;

use rio_common::grpc::{
    H2_INITIAL_CONN_WINDOW, H2_INITIAL_STREAM_WINDOW, client_tls, max_message_size,
};
use tonic::transport::Channel;

use crate::StoreServiceClient;

/// Connect to a gRPC endpoint at `host:port` and return a raw [`Channel`].
///
/// Scheme and TLS wiring depend on the process-global
/// [`rio_common::grpc::client_tls`]: set → `https://` + `.tls_config()`;
/// unset → `http://` (plaintext). The global is initialized by
/// [`rio_common::grpc::init_client_tls`] (via `bootstrap()` in each
/// binary's main); if never called (or called with `None`), plaintext.
///
/// 10s connect timeout: tonic's default is UNBOUNDED. A stale address
/// (e.g., scheduler pod killed → replacement has new IP, but DNS TTL /
/// caller's cached addr hasn't updated) hangs forever on TCP SYN.
/// Observed in lifecycle test: controller's cleanup() never logged
/// "starting drain" — stuck in the admin connect after the scheduler
/// leader was killed mid-run. 10s is enough for a real connect
/// (even cross-AZ) and bounds the failure mode.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Apply h2 keepalive + flow-control window tuning.
///
/// **Keepalive** (30s PING interval, 10s PONG timeout, while-idle):
/// detects half-open connections in ~40s (next PING + PONG timeout)
/// instead of falling through to kernel TCP keepalive --- Linux default
/// `tcp_keepalive_time` is 7200s (2h). Without this, an ungracefully
/// dead peer (SIGKILL, netsplit, OOM-kill --- anything that skips FIN)
/// leaves the connection silently stuck until the kernel notices.
///
/// `while_idle`: send PINGs even with no in-flight requests. Without
/// it, an idle channel never probes --- the peer can vanish and the
/// next RPC blocks until kernel TCP timeout. With it, the h2 layer
/// fires `GoAway` proactively, all streams error, callers reconnect.
///
/// **Flow-control windows** (1 MiB stream / 16 MiB conn / adaptive):
/// see [`H2_INITIAL_STREAM_WINDOW`]. Mirrored server-side via
/// [`rio_common::server::tonic_builder`] — h2 windows are per-direction.
///
/// Factored out after I-048c: the balanced channel diverged from
/// `connect_store_lazy` and went 7 minutes dark on scheduler SIGKILL.
pub(crate) fn with_h2_keepalive(ep: tonic::transport::Endpoint) -> tonic::transport::Endpoint {
    with_h2_throughput(ep)
        .http2_keep_alive_interval(Duration::from_secs(30))
        .keep_alive_timeout(Duration::from_secs(10))
        .keep_alive_while_idle(true)
}

/// Apply h2 flow-control window tuning only (no keepalive). See
/// [`H2_INITIAL_STREAM_WINDOW`].
///
/// Separate from [`with_h2_keepalive`] so the eager [`connect_channel`]
/// path can take the throughput fix without keepalive: under heavy
/// parallel-process load (workspace nextest), `keep_alive_while_idle`
/// PING/PONG on a freshly-eager-connected channel raced and produced
/// spurious GoAway → EIO in fetch tests. The lazy/balanced paths
/// (long-lived channels, the production builder path) take both via
/// [`with_h2_keepalive`].
// r[impl proto.h2.adaptive-window]
pub(crate) fn with_h2_throughput(ep: tonic::transport::Endpoint) -> tonic::transport::Endpoint {
    ep.http2_adaptive_window(true)
        .initial_stream_window_size(Some(H2_INITIAL_STREAM_WINDOW))
        .initial_connection_window_size(Some(H2_INITIAL_CONN_WINDOW))
}

/// Build an `Endpoint` for `host:port`: scheme-select from
/// [`client_tls`], `from_shared`, [`CONNECT_TIMEOUT`], conditional
/// `tls_config`. Callers layer their own `with_h2_*` tuning and
/// connect mode (eager vs lazy) on top.
///
/// Shared by [`connect_channel`] (eager + throughput-only) and
/// [`connect_store_lazy`] (lazy + keepalive) — the endpoint
/// construction is identical, only the h2 wrapper and connect call
/// differ.
fn build_endpoint(addr: &str) -> anyhow::Result<tonic::transport::Endpoint> {
    // `client_tls()` collapses both "OnceLock not initialized" and
    // "initialized with None" to plaintext. Tests that never call
    // init_client_tls stay plaintext.
    let (scheme, tls) = match client_tls() {
        Some(tls) => ("https", Some(tls)),
        None => ("http", None),
    };
    let mut ep =
        Channel::from_shared(format!("{scheme}://{addr}"))?.connect_timeout(CONNECT_TIMEOUT);
    if let Some(tls) = tls {
        ep = ep.tls_config(tls)?;
    }
    Ok(ep)
}

pub async fn connect_channel(addr: &str) -> anyhow::Result<Channel> {
    with_h2_throughput(build_endpoint(addr)?)
        .connect()
        .await
        .map_err(Into::into)
}

// ===========================================================================
// Generic typed-client construction
// ===========================================================================

/// Per-client constants for the generic [`connect`] / [`BalancedChannel`]
/// path. Implemented for each tonic-generated `XServiceClient<Channel>`;
/// the impl supplies the `grpc.health.v1` service name (what
/// [`BalancedChannel`] probes) and the TLS-domain override (cert SAN —
/// what the balanced channel verifies pod-IP connections against).
///
/// Collapses what was previously 9 near-identical `connect_X` /
/// `connect_X_balanced` wrappers (each varying only in client type +
/// these two strings) to two generics ([`connect_single`] /
/// [`connect`]) + small per-client impls. The
/// `max_{en,de}coding_message_size` pair is applied ONCE in
/// [`ProtoClient::wrap`], so per-binary connect blocks can't drift on
/// where it's set.
pub trait ProtoClient: Sized {
    /// `grpc.health.v1` service name to probe. For services hosted on
    /// the scheduler this is `rio.scheduler.SchedulerService` even for
    /// `ExecutorServiceClient` / `AdminServiceClient` — the scheduler's
    /// leader-toggle only flips the named SchedulerService entry, and
    /// all three share the same port + leader gate.
    const HEALTH_SERVICE: &'static str;
    /// TLS domain (cert SAN) for pod-IP connections. The balanced
    /// channel connects to `https://<pod-ip>:<port>` but verifies
    /// against this name. Matches `dnsNames` in
    /// `infra/helm/rio-build/templates/cert-manager.yaml`.
    const TLS_DOMAIN: &'static str;
    /// Construct from a `Channel` and apply [`max_message_size`]. The
    /// generated `XServiceClient::new` doesn't share a trait, so each
    /// impl spells the three calls out — but at least they live next
    /// to each other instead of scattered across 9 wrapper fns.
    fn wrap(ch: Channel) -> Self;
}

/// Macro for the repetitive [`ProtoClient`] impls: each generated
/// client has the same `new(ch).max_decoding_….max_encoding_…` shape.
macro_rules! proto_client {
    ($ty:ty, $health:expr, $domain:expr) => {
        impl ProtoClient for $ty {
            const HEALTH_SERVICE: &'static str = $health;
            const TLS_DOMAIN: &'static str = $domain;
            fn wrap(ch: Channel) -> Self {
                <$ty>::new(ch)
                    .max_decoding_message_size(max_message_size())
                    .max_encoding_message_size(max_message_size())
            }
        }
    };
}

// Scheduler-hosted: SchedulerService, ExecutorService, AdminService all
// share the scheduler's port + leader gate, so the same health-service
// name + TLS domain. Store-hosted: StoreService, StoreAdminService,
// ChunkService share the store's.
proto_client!(
    crate::SchedulerServiceClient<Channel>,
    balance::SCHEDULER_HEALTH_SERVICE,
    balance::SCHEDULER_TLS_DOMAIN
);
proto_client!(
    crate::ExecutorServiceClient<Channel>,
    balance::SCHEDULER_HEALTH_SERVICE,
    balance::SCHEDULER_TLS_DOMAIN
);
proto_client!(
    crate::AdminServiceClient<Channel>,
    balance::SCHEDULER_HEALTH_SERVICE,
    balance::SCHEDULER_TLS_DOMAIN
);
proto_client!(
    crate::StoreServiceClient<Channel>,
    balance::STORE_HEALTH_SERVICE,
    balance::STORE_TLS_DOMAIN
);
proto_client!(
    crate::StoreAdminServiceClient<Channel>,
    balance::STORE_HEALTH_SERVICE,
    balance::STORE_TLS_DOMAIN
);

/// Connect to a single-channel `addr` and wrap in a typed client.
///
/// For tests and non-K8s callers (rio-cli, VM-test fixtures). K8s
/// daemons use [`connect`] which dispatches balance-vs-single from
/// [`UpstreamAddrs`](rio_common::config::UpstreamAddrs).
pub async fn connect_single<C: ProtoClient>(addr: &str) -> anyhow::Result<C> {
    connect_channel(addr).await.map(C::wrap)
}

// r[impl proto.client.balanced]
/// Dispatch balance-vs-single from an
/// [`UpstreamAddrs`](rio_common::config::UpstreamAddrs) triple.
///
/// `balance_host = None` → eager single-channel via `addr`.
/// `balance_host = Some(host)` → health-aware [`BalancedChannel`] over
/// `host:balance_port`, returned as the second tuple element so the
/// caller can hold the probe-loop guard for process lifetime.
///
/// Replaces the ~40L `match cfg.X_balance_host { None => connect_X,
/// Some(h) => connect_X_balanced }` block that was open-coded in every
/// daemon's `main()`. The balanced path applies [`max_message_size`]
/// via [`ProtoClient::wrap`] — same as the single path — fixing the
/// pre-consolidation drift where builder vs gateway applied it at
/// different layers.
pub async fn connect<C: ProtoClient>(
    addrs: &rio_common::config::UpstreamAddrs,
) -> anyhow::Result<(C, Option<BalancedChannel>)> {
    let (ch, guard) = connect_raw::<C>(addrs).await?;
    Ok((C::wrap(ch), guard))
}

/// [`connect`] but return the unwrapped `Channel`. For callers (the
/// builder's `StoreClients`) that wrap the same channel in MULTIPLE
/// generated clients — `connect::<StoreServiceClient>` would discard
/// the channel inside the typed client, and tonic clients don't expose
/// `into_inner()`. The `C` type parameter still picks the
/// `HEALTH_SERVICE` / `TLS_DOMAIN` constants for the balanced path.
pub async fn connect_raw<C: ProtoClient>(
    addrs: &rio_common::config::UpstreamAddrs,
) -> anyhow::Result<(Channel, Option<BalancedChannel>)> {
    match &addrs.balance_host {
        None => {
            tracing::info!(addr = %addrs.addr, service = C::HEALTH_SERVICE, "connecting (single-channel)");
            Ok((connect_channel(&addrs.addr).await?, None))
        }
        Some(host) => {
            tracing::info!(
                %host, port = addrs.balance_port, service = C::HEALTH_SERVICE,
                "connecting (health-aware balanced)"
            );
            let bc = BalancedChannel::for_client::<C>(host.clone(), addrs.balance_port).await?;
            Ok((bc.channel(), Some(bc)))
        }
    }
}

/// Lazy-connect store client with HTTP/2 keepalive.
///
/// Unlike [`connect_single`], this does NOT establish a TCP connection at
/// call time — the channel connects on first RPC and RE-RESOLVES DNS on
/// each reconnect. This is the difference between "connection pinned to
/// the pod IP that DNS resolved to at startup" (eager) and "connection
/// follows the Service's current endpoint" (lazy).
///
/// The eager variant breaks on store rollout: old pod terminates → TCP
/// connection drops → eager Channel's cached IP is stale → RPCs fail
/// with `Unavailable` forever. Lazy re-resolves and reconnects on the
/// next RPC.
///
/// Keepalive (30s interval, 10s timeout, while-idle) detects half-open
/// connections within ~40s instead of waiting for kernel TCP timeout
/// (minutes). Without while-idle, an idle channel wouldn't notice the
/// peer vanished until the next RPC — keepalive surfaces it proactively.
///
/// Returns `Err` only on malformed `addr` (scheme parse) or bad TLS
/// config — never on connection failure (that's deferred to first RPC).
/// So callers can drop their retry loop: the channel ALWAYS constructs.
pub fn connect_store_lazy(addr: &str) -> anyhow::Result<StoreServiceClient<Channel>> {
    let ch = with_h2_keepalive(build_endpoint(addr)?).connect_lazy();
    Ok(StoreServiceClient::wrap(ch))
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use std::time::Duration;

    /// Regression guard: h2 flow-control windows must stay above the
    /// 64 KiB default. A revert to defaults reintroduces the ~30 MB/s
    /// cross-AZ ceiling on `GetPath` (I-180) — silently, since nothing
    /// fails, throughput just drops. There's no public accessor on
    /// `Endpoint` to inspect the configured window, so this asserts the
    /// constants directly; `with_h2_keepalive` is the single application
    /// site (covered by `connect_closed_port_fails_fast_then_succeeds`
    /// going through it via `connect_channel`).
    // r[verify proto.h2.adaptive-window]
    #[test]
    #[allow(
        clippy::assertions_on_constants,
        reason = "the constants ARE the contract — this guards against a \
                  silent revert to h2 defaults"
    )]
    fn h2_window_floor_not_regressed() {
        const H2_DEFAULT: u32 = 65_535;
        assert!(
            H2_INITIAL_STREAM_WINDOW > H2_DEFAULT,
            "stream window {} must exceed h2 default {} (I-180 30 MB/s ceiling)",
            H2_INITIAL_STREAM_WINDOW,
            H2_DEFAULT
        );
        assert!(
            H2_INITIAL_CONN_WINDOW >= H2_INITIAL_STREAM_WINDOW,
            "connection window must be at least the stream window"
        );
    }

    /// Retry loop pattern: connect to a closed port, assert it fails
    /// fast (not hang), bind the port, assert next attempt succeeds.
    /// This is the contract the main.rs retry loops depend on:
    /// closed port = fast Err, not 10s CONNECT_TIMEOUT hang.
    ///
    /// NOT start_paused: real TCP sockets + auto-advancing mock clock
    /// fires CONNECT_TIMEOUT spuriously while the kernel does real work.
    #[tokio::test]
    async fn connect_closed_port_fails_fast_then_succeeds() {
        // Reserve a port, then close the listener — port is now free
        // but nothing's listening. connect() should get ECONNREFUSED
        // in <100ms (kernel fast-path, no SYN retry).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        // First attempt: refused, fast.
        let t0 = std::time::Instant::now();
        let err = connect_channel(&addr.to_string()).await.unwrap_err();
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "closed port should fail fast (ECONNREFUSED), got {err:?} after {:?}",
            t0.elapsed()
        );

        // Bind a real gRPC server on that port. connect_channel only
        // needs the transport (HTTP/2 handshake) to come up — a
        // tonic-health service suffices (already a non-dev dep for
        // balance.rs, same pattern as balance.rs:426-439).
        let (_reporter, health_svc) = tonic_health::server::health_reporter();
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        let server = tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(health_svc)
                .serve_with_incoming(incoming),
        );

        // Simulate the retry loop: poll until Ok. Bounded at 10 tries
        // (= 20s with the real 2s sleep; here 50ms so the test is fast).
        let mut ch = None;
        for _ in 0..10 {
            match connect_channel(&addr.to_string()).await {
                Ok(c) => {
                    ch = Some(c);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
        assert!(ch.is_some(), "connect never succeeded after port opened");

        server.abort();
    }
}
