//! Binary startup boilerplate shared across rio-* binaries.
//!
//! Two layers extracted here:
//!
//! 1. [`bootstrap`] — the 6-step cold-start prologue (crypto provider,
//!    config load, tracing init, TLS load, shutdown signal, metrics)
//!    that was 5×-duplicated across controller/gateway/scheduler/store/
//!    worker main.rs. ~40L per binary → one call.
//!
//! 2. [`spawn_health_plaintext`] / [`spawn_drain_task`] — tonic server
//!    lifecycle helpers, extracted earlier from three near-identical
//!    copies (scheduler:644, store:462, gateway:329). Any tonic-health
//!    upgrade or shutdown-signal change had to be three-way synced.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::{Router, http::StatusCode, routing::get};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;
use tonic_health::pb::health_server::{Health, HealthServer};

use crate::config::{CommonConfig, ValidateConfig};
use crate::grpc::{H2_INITIAL_CONN_WINDOW, H2_INITIAL_STREAM_WINDOW};
use crate::observability::OtelGuard;
use crate::signal::Token;
use crate::task::spawn_monitored;

/// `tonic::transport::Server::builder()` with h2 flow-control window
/// tuning applied. Use this for every component's main gRPC server
/// instead of bare `Server::builder()`.
///
/// h2 windows are per-direction; the server's send rate on a `GetPath`
/// stream is bounded by the CLIENT-advertised stream window (which
/// `rio-proto::client::with_h2_throughput` controls), but the server's
/// own connection window caps aggregate egress across concurrent
/// streams. The h2 default 64 KiB was the 30 MB/s wall on builder NAR
/// fetch (I-180). Mirrored client-side in `rio-proto::client`; see
/// [`H2_INITIAL_STREAM_WINDOW`] for the rationale.
// r[impl proto.h2.adaptive-window]
pub fn tonic_builder() -> tonic::transport::Server {
    tonic::transport::Server::builder()
        .http2_adaptive_window(Some(true))
        .initial_stream_window_size(Some(H2_INITIAL_STREAM_WINDOW))
        .initial_connection_window_size(Some(H2_INITIAL_CONN_WINDOW))
}

/// Projects the crate-local `Config` to its embedded
/// [`CommonConfig`]. Every binary's `Config` carries
/// `#[serde(flatten)] common: CommonConfig`; this trait names that
/// field so [`bootstrap`] (which loads the full `Config` via figment)
/// can read the shared `tls` / `metrics_addr` / `drain_grace` without
/// knowing the concrete type.
///
/// `metric_labels` is a method (not a `CommonConfig` field) because
/// it's derived at runtime from other config — rio-builder computes
/// `role={builder,fetcher}` from `executor_kind`. All other binaries
/// take the default empty.
pub trait HasCommonConfig {
    /// The embedded [`CommonConfig`]. Always `&self.common`.
    fn common(&self) -> &CommonConfig;
    /// Global labels attached to every metric this binary exports.
    /// Default empty. rio-builder overrides to add
    /// `role={builder,fetcher}` so fetcher pods are distinguishable
    /// despite sharing the binary.
    fn metric_labels(&self) -> Vec<(&'static str, String)> {
        vec![]
    }
}

/// What [`bootstrap`] hands back. Destructure in `main()`:
///
/// ```ignore
/// let Bootstrap { cfg, shutdown, otel_guard: _otel_guard } =
///     rio_common::server::bootstrap(
///         "gateway", cli,
///         rio_gateway::describe_metrics,
///     )?;
/// ```
///
/// `otel_guard` MUST be kept alive for the duration of `main()` —
/// destructuring with `..` drops it immediately, tearing down the OTLP
/// batch processor (zero spans exported). Bind it explicitly to a
/// `_`-prefixed local so it drops at end-of-main, flushing buffered
/// OTel spans.
#[must_use = "Bootstrap holds the OtelGuard — dropping it tears down tracing"]
pub struct Bootstrap<C> {
    pub cfg: C,
    /// Process-wide cancellation token. Fires on SIGTERM/SIGINT.
    /// Clone for every background loop and pass `.cancelled_owned()`
    /// to tonic/axum graceful-shutdown.
    pub shutdown: Token,
    /// Independent token for the main listener's `serve_with_shutdown`.
    /// NOT a child of `shutdown` — see [`spawn_drain_task`] for why.
    /// Binaries without a tonic listener (controller, builder) ignore
    /// this.
    pub serve_shutdown: Token,
    /// Must be kept alive for the duration of `main()` — destructuring
    /// with `..` will drop it immediately and tear down the OTLP
    /// exporter. Bind explicitly: `otel_guard: _otel_guard`.
    pub otel_guard: OtelGuard,
    /// The component root span, entered. Every subsequent log/span in
    /// `main()` parents under this. Bind to a `_`-prefixed local; it
    /// drops at end-of-main.
    pub root_span: tracing::span::EnteredSpan,
}

/// The 6-step cold-start prologue every rio binary runs before its own
/// wiring. Canonical order:
///
/// 1. `rustls` crypto provider install (aws-lc-rs; must precede any TLS
///    use — dual ring+aws-lc-rs feature activation panics otherwise)
/// 2. tracing init (returns OtelGuard, held by `Bootstrap`). Runs BEFORE
///    config load so figment errors land in structured logs — tracing
///    reads only env vars (`RUST_LOG`, `RIO_LOG_FORMAT`,
///    `RIO_OTEL_ENDPOINT`), no dependency on the figment-loaded config.
/// 3. figment config load (defaults → TOML → env → CLI)
/// 4. client TLS load + [`crate::grpc::init_client_tls`] — sets the
///    process-global OnceLock that `rio-proto::client::connect_*`
///    reads. The `info!("client mTLS enabled")` log fires here so it
///    lands once regardless of which binary.
/// 5. `ValidateConfig::validate` bounds check (after TLS load so a bad
///    TLS path surfaces BEFORE a bad required-field — TLS errors are
///    more actionable; a missing `scheduler_addr` often masks "wrong
///    ConfigMap mounted")
/// 6. shutdown signal + metrics exporter + `describe_metrics` callback
/// 7. root span (`component = {component}`) + version `info!`
///
/// The root span is created with `component` as its only field.
/// rio-builder records `executor_id` separately after resolving it
/// (root-span fields can't be added post-creation, but a child span /
/// log field is observably equivalent for the JSON log line).
pub fn bootstrap<C, A>(
    component: &'static str,
    cli: A,
    describe_metrics: fn(),
) -> anyhow::Result<Bootstrap<C>>
where
    C: DeserializeOwned + Default + Serialize + ValidateConfig + HasCommonConfig,
    A: Serialize,
{
    // rustls CryptoProvider MUST be installed before any TLS use.
    // kube → hyper-rustls enables `ring`; rio-proto → aws-sdk enables
    // `aws-lc-rs`. With BOTH active, rustls 0.23 can't auto-select and
    // PANICS on first TLS handshake. Pin aws-lc-rs (rustls default,
    // faster than ring). `let _`: Err if already installed — can't
    // happen at top-of-main, discard.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let otel_guard = crate::observability::init_tracing(component)?;
    let cfg: C = crate::config::load(component, cli)
        .inspect_err(|e| tracing::error!(error = %e, "config load failed"))?;

    let common = cfg.common();
    let client_tls = crate::tls::load_client_tls(&common.tls)?;
    if common.tls.is_configured() {
        tracing::info!("client mTLS enabled for outgoing gRPC");
    }
    crate::grpc::init_client_tls(client_tls);

    cfg.validate()?;

    let shutdown = crate::signal::shutdown_signal();
    crate::observability::init_metrics(common.metrics_addr, &cfg.metric_labels())?;
    describe_metrics();

    // Span name is the static "rio" with `component` as a field —
    // `info_span!` requires a literal name, and the field is what
    // log queries / OTel filter on anyway.
    let root_span = tracing::info_span!("rio", component).entered();
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "starting rio-{component}"
    );

    Ok(Bootstrap {
        cfg,
        shutdown,
        serve_shutdown: Token::new(),
        otel_guard,
        root_span,
    })
}

/// `/healthz` + `/readyz` axum router for kubelet HTTP probes.
///
/// | Route | K8s probe | Semantics |
/// |---|---|---|
/// | `/healthz` | liveness | 200 unconditionally — handler reached ⇒ process alive + runtime responsive. |
/// | `/readyz` | readiness | 200 iff `ready` is `true`; 503 otherwise. |
///
/// `ready` is the binary's own gate (e.g. rio-builder flips it on the
/// first accepted heartbeat; rio-controller passes a constant `true`
/// — it has no "connected but not ready" state).
///
/// Serve via [`spawn_axum`] for graceful shutdown wiring. Distinct
/// from [`spawn_health_plaintext`] (gRPC `grpc.health.v1.Health` for
/// tonic-served binaries) — this is the plain-HTTP variant for
/// binaries with no tonic listener.
pub fn health_router(ready: Arc<AtomicBool>) -> Router {
    Router::new()
        .route("/healthz", get(async || StatusCode::OK))
        .route(
            "/readyz",
            get(async move || {
                if ready.load(Ordering::Relaxed) {
                    StatusCode::OK
                } else {
                    StatusCode::SERVICE_UNAVAILABLE
                }
            }),
        )
}

/// Spawn an axum server on `addr` with `with_graceful_shutdown` wired
/// to `shutdown.cancelled_owned()`. Consolidates the three prior
/// hand-rolled variants (rio-controller raw-TCP, rio-builder no-
/// graceful-shutdown, rio-store correct).
///
/// Bind failure logs and returns (the spawned task ends; kubelet
/// liveness fails → pod restart → self-healing). Serve failure logs.
///
/// No explicit read timeout: these are kubelet-only LAN endpoints
/// behind cluster-network firewalling; hyper's connection handling
/// is sufficient and adding `tower-http` for one timeout layer is
/// not warranted.
pub fn spawn_axum(
    name: &'static str,
    addr: SocketAddr,
    router: Router,
    shutdown: Token,
) -> tokio::task::JoinHandle<()> {
    spawn_monitored(name, async move {
        tracing::info!(addr = %addr, server = name, "starting HTTP server");
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                if let Err(e) = axum::serve(listener, router)
                    .with_graceful_shutdown(shutdown.cancelled_owned())
                    .await
                {
                    tracing::error!(error = %e, server = name, "HTTP server failed");
                }
            }
            Err(e) => {
                tracing::error!(error = %e, addr = %addr, server = name, "HTTP bind failed");
            }
        }
    })
}

/// Spawn a plaintext tonic server with ONLY `grpc.health.v1.Health`, on a
/// dedicated port, sharing the SAME `HealthReporter` state as the caller's
/// main server.
///
/// **Why separate port:** K8s gRPC readiness probes can't do mTLS. When the
/// main port is mTLS, the probe needs a plaintext endpoint. The health_service
/// passed here is a `.clone()` of the one on the main port — cloning
/// `HealthServer<HealthService>` shares the underlying `Arc<RwLock<HashMap>>`
/// status map, so `set_serving()` / `set_not_serving()` on the reporter
/// propagates to BOTH ports. See `r[sched.health.shared-reporter]`.
///
/// **Why `cancelled_owned`:** the spawned task outlives the caller's stack
/// frame, so it needs owned access to the token. Pass the CHILD token (not
/// the parent) — health server should survive the drain window same as the
/// main server (K8s probe gets NOT_SERVING during drain, not ECONNREFUSED).
///
/// **Caller decides whether to call this.** Scheduler/store gate on
/// `server_tls.is_some()` (only need plaintext when main is mTLS). Gateway
/// always calls (its main listener is SSH, not tonic — health is always
/// separate).
///
/// Generic over `T: Health` because `tonic_health::server::health_reporter()`
/// returns `HealthServer<impl Health>` (opaque type) — callers can't name the
/// concrete `HealthService` type, so this function can't either.
pub fn spawn_health_plaintext<T: Health>(
    health_service: HealthServer<T>,
    health_addr: SocketAddr,
    shutdown: CancellationToken,
) {
    tracing::info!(addr = %health_addr, "spawning plaintext health server for K8s probes");
    spawn_monitored("health-plaintext", async move {
        if let Err(e) = tonic::transport::Server::builder()
            .add_service(health_service)
            .serve_with_shutdown(health_addr, shutdown.cancelled_owned())
            .await
        {
            tracing::error!(error = %e, "plaintext health server failed");
        }
    });
}

/// Spawn the drain-on-SIGTERM task: wait for `parent` cancel, flip health
/// to NOT_SERVING via `set_not_serving`, sleep `grace`, then cancel
/// `serve_shutdown`.
///
/// **Two-stage shutdown with an INDEPENDENT serve token.** The
/// `serve_shutdown` token is the one tonic's `Server::serve_with_shutdown`
/// awaits — it must NOT be `parent.child_token()`. `child_token()`
/// cascades parent→child synchronously: `parent.cancel()` would set
/// `serve_shutdown.is_cancelled() = true` instantly, giving zero drain
/// window. The drain task is the ONLY path to `serve_shutdown`
/// cancellation; if it somehow died, the process hangs until SIGKILL at
/// `terminationGracePeriodSeconds` — acceptable, the body below cannot
/// realistically panic.
///
/// **The `set_not_serving` closure handles service-name divergence.**
/// Scheduler + store flip a NAMED service via
/// `reporter.set_not_serving::<S>()` (the client-side BalancedChannel
/// probes `rio.{component}.{Name}Service` to find the leader). Gateway
/// flips the EMPTY-STRING service via `reporter.set_service_status("",
/// NotServing)` — K8s's `grpc:` probe with no `service:` field checks
/// `""`. Caller wraps whichever variant in the closure.
///
/// Extracted from three near-identical ~20L blocks (scheduler:454-477,
/// store:244-264, gateway:310-336). The 11th paired-main.rs pattern —
/// see this module's header for the first ten.
///
/// ```ignore
/// // in scheduler/main.rs:
/// rio_common::server::spawn_drain_task(
///     shutdown.clone(),
///     serve_shutdown.clone(),
///     cfg.common.drain_grace,
///     move || async move {
///         reporter.set_not_serving::<SchedulerServiceServer<_>>().await
///     },
/// );
/// ```
// r[impl common.drain.not-serving-before-exit]
pub fn spawn_drain_task<F, Fut>(
    parent: CancellationToken,
    serve_shutdown: CancellationToken,
    grace: std::time::Duration,
    set_not_serving: F,
) where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    spawn_monitored("drain-on-sigterm", async move {
        parent.cancelled().await;
        set_not_serving().await;
        tracing::info!(
            grace_secs = grace.as_secs(),
            "SIGTERM: health=NOT_SERVING, draining"
        );
        // Zero-grace (tests) skips the sleep entirely rather than
        // registering a timer. `sleep(0)` would still yield once.
        if !grace.is_zero() {
            tokio::time::sleep(grace).await;
        }
        serve_shutdown.cancel();
    });
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;

    /// `/healthz` is unconditional; `/readyz` tracks the flag.
    #[tokio::test]
    async fn health_router_liveness_and_readiness() {
        let ready = Arc::new(AtomicBool::new(false));
        let app = health_router(Arc::clone(&ready));

        let resp = app
            .clone()
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "liveness is unconditional");

        let resp = app
            .clone()
            .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

        ready.store(true, Ordering::Relaxed);
        let resp = app
            .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// `spawn_axum` shuts down cleanly when the token fires (no
    /// dangling listener — the JoinHandle completes).
    #[tokio::test]
    async fn spawn_axum_graceful_shutdown() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let shutdown = Token::new();
        let handle = spawn_axum(
            "test",
            addr,
            health_router(Arc::new(AtomicBool::new(true))),
            shutdown.clone(),
        );
        // Let it bind.
        tokio::task::yield_now().await;
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("server should exit on shutdown within 5s")
            .expect("server task should not panic");
    }

    /// spawn_drain_task sequencing: parent cancel → set_not_serving
    /// called → sleep(grace) → serve_shutdown cancelled. ORDER is
    /// load-bearing: NOT_SERVING must flip BEFORE serve_shutdown fires
    /// so a K8s probe during the drain window sees NOT_SERVING, not
    /// ECONNREFUSED.
    ///
    /// Paused clock (`start_paused`) makes the 6s grace deterministic
    /// — no wall-clock sleep. `advance()` moves mock time; a few
    /// `yield_now()` calls let the spawned task poll through each
    /// stage before we assert.
    ///
    /// Complements rio-scheduler's
    /// `drain_sets_not_serving_before_child_cancel` (integration-ish,
    /// exercises the real tonic-health reporter over a real TCP
    /// channel). This test is the fast unit for the helper itself.
    // r[verify common.drain.not-serving-before-exit]
    #[tokio::test(start_paused = true)]
    async fn drain_task_sets_not_serving_before_shutdown() {
        let parent = CancellationToken::new();
        let serve_shutdown = CancellationToken::new();
        let called = Arc::new(AtomicBool::new(false));
        let c = Arc::clone(&called);

        spawn_drain_task(
            parent.clone(),
            serve_shutdown.clone(),
            Duration::from_secs(6),
            move || async move { c.store(true, Ordering::SeqCst) },
        );

        // Pre-SIGTERM: nothing has fired. Spawned task is parked on
        // parent.cancelled().await.
        assert!(!called.load(Ordering::SeqCst));
        assert!(!serve_shutdown.is_cancelled());

        parent.cancel();
        // parent.cancel() wakes the spawned task's cancelled() await;
        // it then runs set_not_serving() (our atomic store) and
        // registers the 6s sleep. Yield a few times to let it get
        // there — the drain body has two await points before sleep
        // (cancelled(), closure), plus spawn_monitored wraps the
        // future, so three yields is ample. More doesn't hurt.
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        assert!(
            called.load(Ordering::SeqCst),
            "set_not_serving closure must run immediately after parent cancel"
        );
        assert!(
            !serve_shutdown.is_cancelled(),
            "serve_shutdown must NOT fire before grace elapses — \
             the drain window is the whole point"
        );

        // Advance past grace. sleep(6s) completes; serve_shutdown
        // fires. advance() auto-yields to timers; one more yield_now
        // lets the post-sleep .cancel() run.
        tokio::time::advance(Duration::from_secs(7)).await;
        tokio::task::yield_now().await;
        assert!(
            serve_shutdown.is_cancelled(),
            "serve_shutdown must fire after grace elapses"
        );
    }

    /// Zero grace: the `if !grace.is_zero()` skip. serve_shutdown
    /// fires on the SAME scheduler tick as set_not_serving — the
    /// drain body never awaits a timer. Tests that need instant
    /// shutdown (no mocked time) pass `Duration::ZERO`.
    ///
    /// Distinct from the nonzero case above: that proves the grace
    /// delay is observed; this proves grace=0 short-circuits it.
    #[tokio::test]
    async fn drain_task_zero_grace_fires_immediately() {
        let parent = CancellationToken::new();
        let serve_shutdown = CancellationToken::new();
        let called = Arc::new(AtomicBool::new(false));
        let c = Arc::clone(&called);

        spawn_drain_task(
            parent.clone(),
            serve_shutdown.clone(),
            Duration::ZERO,
            move || async move { c.store(true, Ordering::SeqCst) },
        );

        parent.cancel();
        // No timer registered → no `advance()` needed. Just yield
        // enough for the spawned task to run to completion.
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        assert!(called.load(Ordering::SeqCst), "closure ran");
        assert!(
            serve_shutdown.is_cancelled(),
            "zero grace → serve_shutdown fires without a sleep"
        );
    }
}
