//! rio-controller binary.
//!
//! Two concurrent things: the WorkerPool Controller::run (event-
//! driven reconcile) and the Autoscaler::run (30s poll loop).
//! Merged via `futures::select` — either can make progress.
//!
//! Build reconciler runs as a second Controller::run, merged
//! with the WorkerPool controller via futures::join. Both
//! terminate on SIGTERM (shutdown_on_signal).

use std::sync::Arc;

use clap::Parser;
use futures_util::StreamExt;
use k8s_openapi::api::apps::v1::StatefulSet;
use kube::runtime::{Controller, watcher};
use kube::{Api, Client};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use rio_controller::crds::build::Build;
use rio_controller::crds::workerpool::WorkerPool;
use rio_controller::reconcilers::{Ctx, build, workerpool};
use rio_controller::scaling::Autoscaler;

// ----- config (figment two-struct) --------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    /// rio-scheduler gRPC address. AdminService + SchedulerService
    /// on the same port. Required — no sensible default.
    scheduler_addr: String,
    /// Headless Service host. Used two ways:
    ///   1. Passed to workers as `RIO_SCHEDULER_BALANCE_HOST`.
    ///   2. Used by THIS process's autoscaler for leader-aware
    ///      ClusterStatus polling. `None` → single-channel fallback
    ///      via `scheduler_addr` (ClusterIP — round-robins to the
    ///      standby ~50% of the time with replicas=2).
    ///
    /// In K8s: set to `rio-scheduler-headless` via env.
    scheduler_balance_host: Option<String>,
    scheduler_balance_port: u16,
    /// rio-store gRPC address. Build reconciler fetches .drv
    /// content from here. Required for Build CRDs; WorkerPool-
    /// only deployments can leave it empty (Build reconciler
    /// errors on first apply, operator sees it in logs).
    store_addr: String,
    /// Prometheus metrics listen address.
    metrics_addr: std::net::SocketAddr,
    /// HTTP /healthz listen address. K8s livenessProbe hits this.
    health_addr: std::net::SocketAddr,
    /// Autoscaler poll interval (seconds). Default 30s; VM tests
    /// override to 3s so a scale decision happens within the test
    /// timeout.
    autoscaler_poll_secs: u64,
    autoscaler_scale_up_window_secs: u64,
    /// Scale-down stabilization window. Default 600s (K8s HPA
    /// convention). VM tests shorten this to ~10s to observe a
    /// full up→down cycle within the test timeout.
    autoscaler_scale_down_window_secs: u64,
    autoscaler_min_interval_secs: u64,
    /// mTLS client config for outgoing gRPC (scheduler + store).
    /// Set via `RIO_TLS__*`. The controller's K8s API connection
    /// has its own TLS (kube client, in-cluster service account
    /// CA) — this is only for rio-internal gRPC.
    tls: rio_common::tls::TlsConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            scheduler_addr: String::new(),
            scheduler_balance_host: None,
            scheduler_balance_port: 9001,
            store_addr: String::new(),
            // 9094: gateway=9090, scheduler=9091, store=9092,
            // worker=9093. Controller is next.
            metrics_addr: "0.0.0.0:9094".parse().unwrap(),
            // Same +100 pattern as gateway/worker.
            health_addr: "0.0.0.0:9194".parse().unwrap(),
            // Match ScalingTiming::default(). Duplicated rather
            // than .as_secs()-ing from the Default impl to avoid
            // a const-fn dance — keep them in sync when changing.
            autoscaler_poll_secs: 30,
            autoscaler_scale_up_window_secs: 30,
            autoscaler_scale_down_window_secs: 600,
            autoscaler_min_interval_secs: 30,
            tls: rio_common::tls::TlsConfig::default(),
        }
    }
}

#[derive(Parser, Serialize, Default)]
#[command(name = "rio-controller", about = "Kubernetes operator for rio-build")]
struct CliArgs {
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    scheduler_addr: Option<String>,

    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    store_addr: Option<String>,

    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    metrics_addr: Option<std::net::SocketAddr>,

    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    health_addr: Option<std::net::SocketAddr>,
}

// ----- main --------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // rustls CryptoProvider MUST be installed before any TLS
    // use. kube → hyper-rustls enables the `ring` feature;
    // rio-proto → aws-sdk enables `aws-lc-rs`. With BOTH active,
    // rustls 0.23 can't auto-select and PANICS on first TLS
    // connect (kube::Client::try_default below). Pick aws-lc-rs
    // — it's rustls's default and faster than ring.
    //
    // `let _`: returns Err if already installed (can't happen —
    // this is the first line of main). Discard it.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let cli = CliArgs::parse();
    let cfg: Config = rio_common::config::load("controller", cli)?;
    let _otel_guard = rio_common::observability::init_tracing("controller")?;

    // Cancellation for the non-kube-runtime loops (autoscaler,
    // health). kube-rs's .shutdown_on_signal() installs its OWN
    // SIGTERM/SIGINT watcher for the Controller loops — tokio's
    // signal() is broadcast, both watchers fire. This token is
    // for the spawn_monitored tasks that kube-rs doesn't manage.
    let shutdown = rio_common::signal::shutdown_signal();

    // Client TLS init BEFORE connect_admin. The controller connects
    // lazily per-reconcile (Ctx holds String addrs) — all those
    // connect calls go through rio_proto::client::connect_* which
    // reads this OnceLock.
    rio_proto::client::init_client_tls(
        rio_common::tls::load_client_tls(&cfg.tls)
            .map_err(|e| anyhow::anyhow!("TLS config: {e}"))?,
    );
    if cfg.tls.is_configured() {
        info!("client mTLS enabled for outgoing gRPC");
    }

    anyhow::ensure!(
        !cfg.scheduler_addr.is_empty(),
        "scheduler_addr is required (set --scheduler-addr, RIO_SCHEDULER_ADDR, or controller.toml)"
    );
    // `tokio::time::interval(ZERO)` panics. Autoscaler::run feeds
    // `from_secs(cfg.autoscaler_poll_secs)` into interval() —
    // `autoscaler_poll_secs = 0` would panic inside spawn_monitored
    // (logged, controller survives, but autoscaling silently dead).
    // Fail fast at config load instead.
    anyhow::ensure!(
        cfg.autoscaler_poll_secs > 0,
        "autoscaler_poll_secs must be positive (tokio::time::interval panics on ZERO)"
    );
    // WorkerPool-only deployments legitimately leave store_addr
    // empty (doc comment on Config.store_addr). Build CRDs fail
    // their first reconcile with a tonic malformed-URI error —
    // deep inside error_policy backoff, easy to miss. Warn loudly
    // at startup so the operator sees it in `kubectl logs` tail.
    if cfg.store_addr.is_empty() {
        warn!(
            "RIO_STORE_ADDR not set; Build CRDs will fail (connect_store \
             gets empty URI). Fine for WorkerPool-only deployments."
        );
    }

    let _root_guard = tracing::info_span!("controller", component = "controller").entered();
    info!(
        version = env!("CARGO_PKG_VERSION"),
        "starting rio-controller"
    );

    rio_common::observability::init_metrics(cfg.metrics_addr)?;
    rio_controller::describe_metrics();

    // ---- K8s client ----
    // try_default reads in-cluster config (service account token
    // at /var/run/secrets/kubernetes.io/serviceaccount/) or
    // KUBECONFIG for local dev. `?` — no kube client = useless
    // controller, fail loud.
    let client = Client::try_default().await?;
    info!("kubernetes client connected");

    // ---- Scheduler clients (autoscaler + reconcilers) ----
    // Retry until connected. All rio-* pods start in parallel via
    // helm; under coverage-instrumented binaries + the k3s flannel
    // CNI race (`/run/flannel/subnet.env` written at t≈186s while
    // pod sandboxes start at t≈185s), this process can reach this
    // line before the scheduler Service has endpoints. connect_admin
    // uses eager .connect().await --- refused --> Err. The previous `?`
    // here meant process-exit --> CrashLoopBackOff --> 10s/20s/40s
    // kubelet backoff --> 180s test budget exhausted (observed 2/2
    // coverage-full failures 2026-03-16). Retry internally instead:
    // pod stays not-Ready (health server below hasn't spawned), so
    // the Deployment's Available condition correctly gates on this.
    //
    // Once connected: hold the channel for process lifetime. Balanced
    // when scheduler_balance_host is set --- the standby returns
    // UNAVAILABLE on all RPCs, so ClusterIP round-robin fails ~50% of
    // ticks; balanced channel health-probes pod IPs and routes only
    // to the leader. BOTH AdminServiceClient (autoscaler ClusterStatus
    // polls) AND SchedulerServiceClient (Build reconciler SubmitBuild/
    // WatchBuild/CancelBuild) wrap the SAME channel --- one probe
    // loop, one endpoint set. Guard held in _balance_guard (dropping
    // it stops the probe loop).
    //
    // Single-channel mode: two separate connects (two TCP conns).
    // Dev/test only (single replica, no standby to round-robin to).
    let (admin, sched_client, _balance_guard) = loop {
        let result: anyhow::Result<_> = match &cfg.scheduler_balance_host {
            None => {
                info!(addr = %cfg.scheduler_addr, "connecting to scheduler (single-channel)");
                async {
                    let admin = rio_proto::client::connect_admin(&cfg.scheduler_addr).await?;
                    let sched = rio_proto::client::connect_scheduler(&cfg.scheduler_addr).await?;
                    Ok((admin, sched, None))
                }
                .await
            }
            Some(host) => {
                info!(
                    %host, port = cfg.scheduler_balance_port,
                    "connecting to scheduler (health-aware balanced)"
                );
                async {
                    let (admin, bc) = rio_proto::client::balance::connect_admin_balanced(
                        host.clone(),
                        cfg.scheduler_balance_port,
                    )
                    .await?;
                    // Reuse the same balanced Channel for the
                    // SchedulerServiceClient --- ONE probe loop,
                    // BOTH clients route only to the leader.
                    let sched = rio_proto::SchedulerServiceClient::new(bc.channel())
                        .max_decoding_message_size(rio_proto::max_message_size())
                        .max_encoding_message_size(rio_proto::max_message_size());
                    Ok((admin, sched, Some(bc)))
                }
                .await
            }
        };
        match result {
            Ok(triple) => break triple,
            Err(e) => {
                warn!(error = %e, "scheduler connect failed; retrying in 2s (pod stays not-Ready)");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    };

    // ---- Health server ----
    // AFTER kube + scheduler connect: liveness should pass only
    // once the things we depend on are reachable. Before this line,
    // pod is not-ready; after, /healthz returns 200.
    //
    // Simple axum with always-200 /healthz. No readiness gate
    // beyond "process is up" — the controller has no "I'm
    // connected but not yet leading" state (no leader election;
    // single replica per controller.md design).
    spawn_health_server(cfg.health_addr, shutdown.clone());

    // ---- Events Recorder ----
    // Reporter identifies US (the controller) in emitted events.
    // `kubectl get events` shows `rio-controller` in the SOURCE
    // column. instance=None → K8s uses pod name (from metadata.
    // name downward API if set, else hostname).
    let recorder = kube::runtime::events::Recorder::new(
        client.clone(),
        kube::runtime::events::Reporter {
            controller: "rio-controller".into(),
            instance: None,
        },
    );

    // ---- Context ----
    let ctx = Arc::new(Ctx {
        client: client.clone(),
        scheduler: sched_client,
        admin: admin.clone(),
        scheduler_addr: cfg.scheduler_addr.clone(),
        scheduler_balance_host: cfg.scheduler_balance_host.clone(),
        scheduler_balance_port: cfg.scheduler_balance_port,
        store_addr: cfg.store_addr.clone(),
        recorder: recorder.clone(),
        watching: Arc::new(dashmap::DashMap::new()),
    });

    // ---- WorkerPool controller ----
    // .owns(StatefulSet): when a StatefulSet with our
    // ownerReference changes, enqueue the owning WorkerPool.
    // That's how status updates propagate: reconciler patches
    // StatefulSet → StatefulSet controller updates its status →
    // we get notified → re-reconcile → patch WorkerPool.status.
    //
    // shutdown_on_signal: SIGTERM → graceful stop (drains
    // in-flight reconciles). K8s sends SIGTERM on pod delete.
    let pools: Api<WorkerPool> = Api::all(client.clone());
    let stses: Api<StatefulSet> = Api::all(client.clone());
    let wp_controller = Controller::new(pools, watcher::Config::default())
        .owns(stses, watcher::Config::default())
        .shutdown_on_signal()
        .run(workerpool::reconcile, workerpool::error_policy, ctx.clone())
        .for_each(|res| async move {
            match res {
                Ok((obj, _action)) => {
                    tracing::debug!(pool = %obj.name, "reconciled");
                }
                Err(e) => {
                    // Already logged by error_policy; this is
                    // the controller-runtime's own wrapper error
                    // (ObjectNotFound etc). debug not warn —
                    // these are normal (delete race).
                    tracing::debug!(error = %e, "reconcile loop error");
                }
            }
        });

    // ---- Autoscaler ----
    // Separate task. spawn_monitored: if it panics, logged;
    // controller keeps reconciling (spec changes still apply),
    // just no autoscale. Better than the whole pod dying.
    let timing = rio_controller::scaling::ScalingTiming {
        poll_interval: std::time::Duration::from_secs(cfg.autoscaler_poll_secs),
        scale_up_window: std::time::Duration::from_secs(cfg.autoscaler_scale_up_window_secs),
        scale_down_window: std::time::Duration::from_secs(cfg.autoscaler_scale_down_window_secs),
        min_scale_interval: std::time::Duration::from_secs(cfg.autoscaler_min_interval_secs),
    };
    info!(?timing, "autoscaler timing");
    let autoscaler = Autoscaler::new(client.clone(), admin, timing, recorder);
    rio_common::task::spawn_monitored("autoscaler", autoscaler.run(shutdown.clone()));

    // ---- Build controller ----
    // No `.owns()` — Builds don't own K8s children. The watch
    // task patches status directly; no need for child-triggered
    // re-reconcile.
    let builds: Api<Build> = Api::all(client);
    let build_controller = Controller::new(builds, watcher::Config::default())
        .shutdown_on_signal()
        .run(build::reconcile, build::error_policy, ctx)
        .for_each(|res| async move {
            match res {
                Ok((obj, _)) => tracing::debug!(build = %obj.name, "reconciled"),
                Err(e) => tracing::debug!(error = %e, "Build reconcile loop error"),
            }
        });

    info!("controller running");
    // Both controllers run until SIGTERM. `join!` not `select!`:
    // both should drain in-flight reconciles on shutdown, not
    // whichever finishes first kills the other.
    futures_util::future::join(wp_controller, build_controller).await;

    info!("controller shutting down");
    Ok(())
}

/// Spawn a trivial /healthz server. Always 200 — reaching the
/// handler proves the process is alive. No readiness distinction
/// (controller has no "connected but not ready" state).
///
/// Raw TCP listener speaking minimal HTTP — axum is not in
/// rio-controller's deps and adding it for one /healthz endpoint
/// is heavy. K8s livenessProbe sends `GET /healthz HTTP/1.1` and
/// checks for `200` — that's all we need to match.
fn spawn_health_server(addr: std::net::SocketAddr, shutdown: rio_common::signal::Token) {
    rio_common::task::spawn_monitored("health-server", async move {
        info!(addr = %addr, "starting HTTP health server");
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                warn!(error = %e, addr = %addr, "health bind failed");
                return;
            }
        };
        loop {
            let (mut stream, _) = tokio::select! {
                biased;
                _ = shutdown.cancelled() => return,
                r = listener.accept() => match r {
                    Ok(pair) => pair,
                    Err(_) => continue, // accept fail is transient; retry
                },
            };
            // Fire-and-forget: don't block accept on one slow client.
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                // Read the request line (or at least enough to not
                // RST). Writing before reading + dropping the
                // stream → kernel sends RST (data was in the recv
                // buffer). K8s probe doesn't care (it got the 200
                // before RST) but it's sloppy and breaks tests.
                //
                // 512 bytes is enough for any reasonable probe
                // request. We don't inspect it — K8s sends GET
                // /healthz. If someone else sends POST /foo they
                // still get 200. Not a real HTTP server.
                let mut buf = [0u8; 512];
                let _ = stream.read(&mut buf).await;

                // Connection: close — no keep-alive. Probe is
                // one-shot per periodSeconds.
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\n\
                          Content-Length: 2\r\n\
                          Connection: close\r\n\
                          \r\n\
                          ok",
                    )
                    .await;
                // shutdown flushes and sends FIN. Without it,
                // dropping the stream might RST if the client
                // is still sending. Belt-and-suspenders.
                let _ = stream.shutdown().await;
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_are_stable() {
        let d = Config::default();
        assert!(d.scheduler_addr.is_empty(), "required, no default");
        assert_eq!(d.metrics_addr.to_string(), "0.0.0.0:9094");
        assert_eq!(d.health_addr.to_string(), "0.0.0.0:9194");
    }

    #[test]
    fn cli_args_parse_help() {
        use clap::CommandFactory;
        CliArgs::command().debug_assert();
    }

    /// Health server speaks enough HTTP to satisfy a K8s probe.
    /// Actual socket test — proves the bytes are right.
    #[tokio::test]
    async fn health_server_responds_200() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // Ephemeral port.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener); // release so spawn_health_server can bind
        spawn_health_server(addr, rio_common::signal::Token::new());

        // Give it a tick to bind.
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();

        let response = String::from_utf8_lossy(&buf);
        assert!(
            response.starts_with("HTTP/1.1 200 OK"),
            "probe expects 200: {response}"
        );
        assert!(response.contains("Connection: close"));
    }
}
