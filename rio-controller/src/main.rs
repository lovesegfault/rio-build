//! rio-controller binary.
//!
//! Two concurrent things: the WorkerPool Controller::run (event-
//! driven reconcile) and the Autoscaler::run (30s poll loop).
//! The Controller terminates on SIGTERM via graceful_shutdown_on;
//! the Autoscaler is spawn_monitored with its own shutdown token.

use std::sync::Arc;

use clap::Parser;
use futures_util::StreamExt;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::batch::v1::Job;
use kube::runtime::{Controller, watcher};
use kube::{Api, Client};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use rio_controller::crds::workerpool::WorkerPool;
use rio_controller::crds::workerpoolset::WorkerPoolSet;
use rio_controller::reconcilers::{Ctx, workerpool, workerpoolset};
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
    /// rio-store gRPC address. Injected as `RIO_STORE_ADDR` into
    /// worker pod containers by the WorkerPool reconciler (workers
    /// connect to the store directly for PutPath/GetPath).
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
    /// GC cron interval (hours). 0 = disabled (reconciler not
    /// spawned). The cron calls StoreAdminService.TriggerGC with
    /// default params (dry_run=false, force=false, store's 2h
    /// grace). `store_addr` is the connect target — StoreAdminService
    /// is hosted on the store's gRPC port alongside StoreService.
    gc_interval_hours: u64,
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
            metrics_addr: rio_common::default_addr(9094),
            // Same +100 pattern as gateway/worker.
            health_addr: rio_common::default_addr(9194),
            // Match ScalingTiming::default(). Duplicated rather
            // than .as_secs()-ing from the Default impl to avoid
            // a const-fn dance — keep them in sync when changing.
            autoscaler_poll_secs: 30,
            autoscaler_scale_up_window_secs: 30,
            autoscaler_scale_down_window_secs: 600,
            autoscaler_min_interval_secs: 30,
            // 24h: typical store growth between sweeps is a few
            // thousand paths. Lower values are fine for VM tests.
            gc_interval_hours: 24,
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

impl rio_common::config::ValidateConfig for Config {
    /// Bounds checks on operator-settable fields. Extracted from
    /// `main()` so the checks are unit-testable without spinning up
    /// the full controller (kube-client connect, reconciler spawn).
    /// Every `ensure!` documents a specific crash or silent-wrong
    /// that occurs AFTER startup if the bad value gets through.
    fn validate(&self) -> anyhow::Result<()> {
        use rio_common::config::ensure_required as required;
        required(&self.scheduler_addr, "scheduler_addr", "controller")?;
        // `tokio::time::interval(ZERO)` panics. Autoscaler::run feeds
        // `from_secs(self.autoscaler_poll_secs)` into interval() —
        // `autoscaler_poll_secs = 0` would panic inside spawn_monitored
        // (logged, controller survives, but autoscaling silently dead).
        // Fail fast at config load instead.
        anyhow::ensure!(
            self.autoscaler_poll_secs > 0,
            "autoscaler_poll_secs must be positive (tokio::time::interval panics on ZERO)"
        );
        // These three feed Duration::from_secs but NOT tokio::interval —
        // 0 value is DEGRADED (thrash / no-cooldown) not PANIC. Still
        // reject to prevent operator foot-shooting.
        anyhow::ensure!(
            self.autoscaler_scale_up_window_secs > 0,
            "autoscaler_scale_up_window_secs must be > 0 (got {}); 0 → no cooldown → thrash",
            self.autoscaler_scale_up_window_secs
        );
        anyhow::ensure!(
            self.autoscaler_scale_down_window_secs > 0,
            "autoscaler_scale_down_window_secs must be > 0 (got {}); 0 → no cooldown → thrash",
            self.autoscaler_scale_down_window_secs
        );
        anyhow::ensure!(
            self.autoscaler_min_interval_secs > 0,
            "autoscaler_min_interval_secs must be > 0 (got {}); 0 → no rate-limit on scale ops",
            self.autoscaler_min_interval_secs
        );
        Ok(())
    }
}

impl rio_common::server::HasCommonConfig for Config {
    fn tls(&self) -> &rio_common::tls::TlsConfig {
        &self.tls
    }
    fn metrics_addr(&self) -> std::net::SocketAddr {
        self.metrics_addr
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = CliArgs::parse();
    let rio_common::server::Bootstrap::<Config> { cfg, shutdown, .. } =
        rio_common::server::bootstrap(
            "controller",
            cli,
            rio_proto::client::init_client_tls,
            rio_controller::describe_metrics,
        )?;

    // store_addr is injected into worker pod containers as
    // RIO_STORE_ADDR. Workers with an empty store addr fail their
    // first PutPath with a tonic malformed-URI error — deep inside
    // a spawned task, easy to miss. Warn loudly at startup.
    if cfg.store_addr.is_empty() {
        warn!(
            "RIO_STORE_ADDR not set; worker pods will get empty RIO_STORE_ADDR \
             env (PutPath will fail with malformed URI)."
        );
    }

    let _root_guard = tracing::info_span!("controller", component = "controller").entered();
    info!(
        version = env!("CARGO_PKG_VERSION"),
        "starting rio-controller"
    );

    // ---- K8s client ----
    // try_default reads in-cluster config (service account token
    // at /var/run/secrets/kubernetes.io/serviceaccount/) or
    // KUBECONFIG for local dev. `?` — no kube client = useless
    // controller, fail loud.
    let client = Client::try_default().await?;
    info!("kubernetes client connected");

    // ---- Scheduler clients (autoscaler + reconcilers) ----
    // Retry until connected via connect_with_retry (shutdown-aware,
    // exponential backoff). All rio-* pods start in parallel via
    // helm; this process can reach here before the scheduler Service
    // has endpoints. Pod stays not-Ready (health server below hasn't
    // spawned) while retrying. Observed 2/2 coverage-full failures
    // 2026-03-16 before retry was added (CrashLoopBackOff ate the
    // 180s test budget).
    //
    // Once connected: hold the channel for process lifetime. Balanced
    // when scheduler_balance_host is set — the standby returns
    // UNAVAILABLE on all RPCs, so ClusterIP round-robin fails ~50% of
    // ticks; balanced channel health-probes pod IPs and routes only
    // to the leader. Guard held in _balance_guard (dropping it stops
    // the probe loop). Single-channel mode: dev/test only.
    let (admin, _balance_guard) = match rio_proto::client::connect_with_retry(
        &shutdown,
        || async {
            match &cfg.scheduler_balance_host {
                None => {
                    info!(addr = %cfg.scheduler_addr, "connecting to scheduler (single-channel)");
                    let admin = rio_proto::client::connect_admin(&cfg.scheduler_addr).await?;
                    anyhow::Ok((admin, None))
                }
                Some(host) => {
                    info!(
                        %host, port = cfg.scheduler_balance_port,
                        "connecting to scheduler (health-aware balanced)"
                    );
                    let (admin, bc) = rio_proto::client::balance::connect_admin_balanced(
                        host.clone(),
                        cfg.scheduler_balance_port,
                    )
                    .await?;
                    Ok((admin, Some(bc)))
                }
            }
        },
        None,
    )
    .await
    {
        Ok(pair) => pair,
        Err(rio_proto::client::RetryError::Cancelled) => return Ok(()),
        Err(e @ rio_proto::client::RetryError::Exhausted { .. }) => {
            unreachable!("infinite retries cannot exhaust: {e}")
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
        admin: admin.clone(),
        scheduler_addr: cfg.scheduler_addr.clone(),
        scheduler_balance_host: cfg.scheduler_balance_host.clone(),
        scheduler_balance_port: cfg.scheduler_balance_port,
        store_addr: cfg.store_addr.clone(),
        recorder: recorder.clone(),
    });

    // ---- WorkerPool controller ----
    // .owns(StatefulSet): when a StatefulSet with our
    // ownerReference changes, enqueue the owning WorkerPool.
    // That's how status updates propagate: reconciler patches
    // StatefulSet → StatefulSet controller updates its status →
    // we get notified → re-reconcile → patch WorkerPool.status.
    //
    // graceful_shutdown_on: SIGTERM cancels the token (registered
    // eagerly at top of main()), which drains in-flight reconciles.
    // K8s sends SIGTERM on pod delete.
    let pools: Api<WorkerPool> = Api::all(client.clone());
    let stses: Api<StatefulSet> = Api::all(client.clone());
    // .owns(Job): ephemeral-mode pools spawn one Job per build
    // (reconcilers/workerpool/ephemeral.rs). Without this, re-spawn
    // latency is the 10s poll interval; with it, kube-runtime
    // watch fires on Job status change → <1s re-enqueue. Doesn't
    // change poll semantics, just adds reactive triggers.
    let jobs: Api<Job> = Api::all(client.clone());
    let wp_controller = Controller::new(pools, watcher::Config::default())
        .owns(stses, watcher::Config::default())
        .owns(jobs, watcher::Config::default())
        .graceful_shutdown_on(shutdown.clone().cancelled_owned())
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

    // ---- WorkerPoolSet controller ----
    // .owns(WorkerPool): the WPS reconciler SSA-applies one child
    // WorkerPool per size class (ownerReferences → WPS). When a
    // child's status changes (e.g. replicas go ready), re-enqueue
    // the parent WPS so its per-class status refresh picks it up.
    //
    // Separate Api<WorkerPool> client: can't reuse `pools` above —
    // `Controller::new(pools, ...)` moved it. kube Client is an
    // Arc internally so cloning is cheap.
    let wps_api: Api<WorkerPoolSet> = Api::all(client.clone());
    let wp_children: Api<WorkerPool> = Api::all(client.clone());
    let wps_controller = Controller::new(wps_api, watcher::Config::default())
        .owns(wp_children, watcher::Config::default())
        .graceful_shutdown_on(shutdown.clone().cancelled_owned())
        .run(workerpoolset::reconcile, workerpoolset::error_policy, ctx)
        .for_each(|res| async move {
            match res {
                Ok((obj, _action)) => {
                    tracing::debug!(wps = %obj.name, "reconciled");
                }
                Err(e) => {
                    tracing::debug!(error = %e, "wps reconcile loop error");
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
    let autoscaler = Autoscaler::new(client.clone(), admin.clone(), timing, recorder);
    rio_common::task::spawn_monitored("autoscaler", autoscaler.run(shutdown.clone()));

    // ---- DisruptionTarget watcher ----
    // Pod watcher: K8s sets DisruptionTarget=True on a pod BEFORE
    // eviction (node drain, spot interrupt). We fire DrainWorker
    // {force:true} → scheduler preempts in-flight builds (cgroup.
    // kill + reassign) in seconds instead of burning the 2h
    // terminationGracePeriodSeconds. SIGTERM self-drain (force=
    // false) is the fallback if this task misses the window.
    //
    // spawn_monitored: if the watcher panics, logged; controller
    // keeps reconciling. Loses fast-preemption but not correctness
    // (SIGTERM drain still runs).
    rio_common::task::spawn_monitored(
        "disruption-watcher",
        workerpool::disruption::run(client, admin, shutdown.clone()),
    );

    // ---- GC cron ----
    // Gated on gc_interval_hours > 0. 0 = disabled (operators who
    // want manual-only GC via rio-cli). Also gated on store_addr
    // non-empty — we already warned above if it's empty (workers
    // will break too); don't also spawn a cron that will never
    // connect. Both gates log so the absence is diagnosable.
    //
    // No leader-gate: controller is single-replica by design
    // (controller.md — no leader election). If replicas>1 by
    // misconfig, the store's GC_LOCK_ID advisory lock serializes
    // concurrent TriggerGC calls (see gc_schedule module doc).
    if cfg.gc_interval_hours > 0 && !cfg.store_addr.is_empty() {
        let gc_tick = std::time::Duration::from_secs(cfg.gc_interval_hours * 3600);
        rio_common::task::spawn_monitored(
            "gc-cron",
            rio_controller::reconcilers::gc_schedule::run(
                cfg.store_addr.clone(),
                gc_tick,
                shutdown.clone(),
            ),
        );
    } else {
        info!(
            gc_interval_hours = cfg.gc_interval_hours,
            store_addr_set = !cfg.store_addr.is_empty(),
            "GC cron disabled"
        );
    }

    info!("controller running");
    // Both controllers run until SIGTERM (graceful_shutdown_on
    // drains in-flight reconciles). tokio::join! polls both
    // concurrently on THIS task — no separate spawn. Semantics:
    //   - Ok(()) from ONE: join! continues polling the OTHER until
    //     it also completes (graceful-shutdown waits for both drains).
    //   - Panic in ONE: unwinds through join! immediately — the OTHER
    //     is NOT polled to completion (process exits via unwind).
    // This is the intended behavior: panics propagate (no JoinHandle
    // silent-swallow), Ok-exits wait for sibling (no half-drained
    // state on shutdown).
    tokio::join!(wp_controller, wps_controller);

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
    use rio_common::config::ValidateConfig as _;

    #[test]
    fn config_defaults_are_stable() {
        let d = Config::default();
        assert!(d.scheduler_addr.is_empty(), "required, no default");
        assert_eq!(d.metrics_addr.to_string(), "0.0.0.0:9094");
        assert_eq!(d.health_addr.to_string(), "0.0.0.0:9194");
        assert_eq!(d.gc_interval_hours, 24, "GC cron defaults to daily");
    }

    #[test]
    fn cli_args_parse_help() {
        use clap::CommandFactory;
        CliArgs::command().debug_assert();
    }

    // figment::Jail standing-guard tests — see rio-test-support/src/config.rs.
    // When you add Config.newfield: ADD IT to both assert blocks below.

    rio_test_support::jail_roundtrip!(
        "controller",
        r#"
        autoscaler_poll_secs = 5
        autoscaler_scale_down_window_secs = 77
        gc_interval_hours = 0

        [tls]
        cert_path = "/etc/tls/cert.pem"
        "#,
        |cfg: Config| {
            assert_eq!(
                cfg.autoscaler_poll_secs, 5,
                "autoscaler timing knobs must thread through figment"
            );
            assert_eq!(cfg.autoscaler_scale_down_window_secs, 77);
            assert_eq!(cfg.gc_interval_hours, 0);
            assert_eq!(
                cfg.tls.cert_path.as_deref(),
                Some(std::path::Path::new("/etc/tls/cert.pem")),
                "[tls] table must thread through figment into TlsConfig"
            );
            // Unspecified sub-field defaults via #[serde(default)]
            // on TlsConfig (partial table must work).
            assert!(cfg.tls.key_path.is_none());
        }
    );

    rio_test_support::jail_defaults!("controller", "autoscaler_poll_secs = 30", |cfg: Config| {
        assert!(!cfg.tls.is_configured());
        assert!(cfg.scheduler_balance_host.is_none());
        assert_eq!(cfg.autoscaler_scale_up_window_secs, 30);
        assert_eq!(cfg.autoscaler_scale_down_window_secs, 600);
        assert_eq!(cfg.autoscaler_min_interval_secs, 30);
        assert_eq!(cfg.gc_interval_hours, 24);
    });

    // -----------------------------------------------------------------------
    // validate_config rejection tests — spreads the P0409 pattern
    // (rio-scheduler/src/main.rs) to the controller.
    // -----------------------------------------------------------------------

    /// All required fields filled with valid values — so rejection
    /// tests can patch ONE field and prove that specific check fires.
    /// `Config::default()` leaves `scheduler_addr` empty, which
    /// validate_config rejects BEFORE reaching the bounds checks we
    /// want to test.
    fn test_valid_config() -> Config {
        Config {
            scheduler_addr: "http://localhost:9000".into(),
            store_addr: "http://localhost:9001".into(),
            ..Config::default()
        }
    }

    /// `autoscaler_poll_secs = 0` → `tokio::time::interval(ZERO)`
    /// panics inside Autoscaler::run. validate_config catches at
    /// startup instead of a panic inside spawn_monitored (logged,
    /// controller survives, autoscaling silently dead).
    #[test]
    fn config_rejects_zero_autoscaler_poll() {
        let cfg = Config {
            autoscaler_poll_secs: 0,
            ..test_valid_config()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("autoscaler_poll_secs"), "{err}");
    }

    #[test]
    fn config_rejects_empty_scheduler_addr() {
        let cfg = Config {
            scheduler_addr: String::new(),
            ..test_valid_config()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("scheduler_addr"), "{err}");
    }

    /// Whitespace-only scheduler_addr must be rejected as empty.
    /// Regression guard for `ensure_required`'s trim — pre-helper,
    /// bare `is_empty()` accepted `"   "`, startup failed later at
    /// gRPC connect with a cryptic transport error.
    #[test]
    fn config_rejects_whitespace_scheduler_addr() {
        let mut cfg = test_valid_config();
        cfg.scheduler_addr = "   ".into();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("scheduler_addr is required"),
            "whitespace-only scheduler_addr must be rejected as empty, got: {err}"
        );
    }

    /// Baseline: `test_valid_config()` itself passes — proves the
    /// rejection tests above are testing ONLY their mutation.
    #[test]
    fn config_accepts_valid() {
        test_valid_config()
            .validate()
            .expect("valid config should pass");
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

        // Test assertion display, not parse-path.
        #[allow(clippy::disallowed_methods)]
        let response = String::from_utf8_lossy(&buf);
        assert!(
            response.starts_with("HTTP/1.1 200 OK"),
            "probe expects 200: {response}"
        );
        assert!(response.contains("Connection: close"));
    }
}
