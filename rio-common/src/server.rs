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

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;
use tonic::transport::ClientTlsConfig;
use tonic_health::pb::health_server::{Health, HealthServer};

use crate::config::ValidateConfig;
use crate::observability::OtelGuard;
use crate::signal::Token;
use crate::task::spawn_monitored;
use crate::tls::TlsConfig;

/// Accessor trait for the two config fields [`bootstrap`] needs. All 5
/// binary `Config` structs already have `tls: TlsConfig` and
/// `metrics_addr: SocketAddr` — this trait names that shape so
/// `bootstrap()` can be generic over the crate-local `Config` type.
pub trait HasCommonConfig {
    fn tls(&self) -> &TlsConfig;
    fn metrics_addr(&self) -> SocketAddr;
}

/// What [`bootstrap`] hands back. Destructure in `main()`:
///
/// ```ignore
/// let Bootstrap { cfg, shutdown, .. } = rio_common::server::bootstrap(
///     "gateway", cli,
///     rio_proto::client::init_client_tls,
///     rio_gateway::describe_metrics,
/// )?;
/// ```
///
/// `_otel` stays bound for the lifetime of `main()` via the `..`
/// destructure (the struct itself is dropped at end-of-main, flushing
/// buffered OTel spans).
pub struct Bootstrap<C> {
    pub cfg: C,
    pub shutdown: Token,
    _otel: OtelGuard,
}

/// The 6-step cold-start prologue every rio binary runs before its own
/// wiring. Canonical order:
///
/// 1. `rustls` crypto provider install (aws-lc-rs; must precede any TLS
///    use — dual ring+aws-lc-rs feature activation panics otherwise)
/// 2. figment config load (defaults → TOML → env → CLI)
/// 3. tracing init (returns OtelGuard, held by `Bootstrap`)
/// 4. client TLS load + `init_tls` callback — `rio-common` is a leaf
///    crate and can't name `rio_proto::client::init_client_tls`
///    directly, so the caller passes it as a fn pointer. The
///    `info!("client mTLS enabled")` log fires here, not at the call
///    site, so it lands once regardless of which binary.
/// 5. `ValidateConfig::validate` bounds check (after TLS load so a bad
///    TLS path surfaces BEFORE a bad required-field — TLS errors are
///    more actionable; a missing `scheduler_addr` often masks "wrong
///    ConfigMap mounted")
/// 6. shutdown signal + metrics exporter + `describe_metrics` callback
///
/// Returns before the root span / version `info!` — worker's root span
/// carries `worker_id`, others don't, so that stays at the call site.
pub fn bootstrap<C, A>(
    component: &'static str,
    cli: A,
    init_tls: fn(Option<ClientTlsConfig>),
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

    let cfg: C = crate::config::load(component, cli)?;
    let _otel = crate::observability::init_tracing(component)?;

    let client_tls = crate::tls::load_client_tls(cfg.tls())?;
    if cfg.tls().is_configured() {
        tracing::info!("client mTLS enabled for outgoing gRPC");
    }
    init_tls(client_tls);

    cfg.validate()?;

    let shutdown = crate::signal::shutdown_signal();
    crate::observability::init_metrics(cfg.metrics_addr())?;
    describe_metrics();

    Ok(Bootstrap {
        cfg,
        shutdown,
        _otel,
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
///     std::time::Duration::from_secs(cfg.drain_grace_secs),
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::*;

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
