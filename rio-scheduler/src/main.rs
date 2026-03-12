//! rio-scheduler binary entry point.
//!
//! Starts the gRPC server, connects to PostgreSQL, and spawns the DAG actor.

use std::sync::Arc;

use clap::Parser;
use serde::{Deserialize, Serialize};
use tracing::info;

use rio_proto::AdminServiceServer;
use rio_proto::SchedulerServiceServer;
use rio_proto::WorkerServiceServer;
use rio_scheduler::actor::ActorHandle;
use rio_scheduler::admin::AdminServiceImpl;
use rio_scheduler::db::SchedulerDb;
use rio_scheduler::grpc::SchedulerGrpc;

// Two-struct config split — see rio-common/src/config.rs for rationale.

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    listen_addr: String,
    store_addr: String,
    database_url: String,
    metrics_addr: std::net::SocketAddr,
    tick_interval_secs: u64,
    /// S3 bucket for build-log flush. `None` = flush disabled.
    /// Env: `RIO_LOG_S3_BUCKET`. Wired into LogFlusher in main().
    log_s3_bucket: Option<String>,
    log_s3_prefix: String,
    /// Size-class cutoff config. Empty = disabled (all workers get all
    /// builds). Workers declare their class in heartbeat; scheduler
    /// routes by estimated duration. TOML array-of-tables:
    ///   [[size_classes]]
    ///   name = "small"
    ///   cutoff_secs = 30.0
    ///   mem_limit_bytes = 1073741824
    /// No CLI override — this is structural deploy config, not a knob
    /// you tweak per-invocation. Change it in scheduler.toml.
    size_classes: Vec<rio_scheduler::SizeClassConfig>,
    /// Plaintext health listen address for K8s probes when mTLS is on.
    /// Shares the same HealthReporter as the main server → leadership
    /// toggles propagate. Only listens if server TLS is configured.
    health_addr: std::net::SocketAddr,
    /// mTLS for BOTH server (workers, gateway, controller incoming)
    /// AND client (store outgoing). Set via `RIO_TLS__*`.
    tls: rio_common::tls::TlsConfig,
    /// HMAC key file for signing assignment tokens. The store
    /// verifies on PutPath with the SAME key. Unset = unsigned
    /// tokens (dev mode). Generate: `openssl rand -out /path 32`.
    hmac_key_path: Option<std::path::PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:9001".into(),
            store_addr: String::new(),
            database_url: String::new(),
            metrics_addr: "0.0.0.0:9091".parse().unwrap(),
            tick_interval_secs: 10,
            log_s3_bucket: None,
            log_s3_prefix: "logs".into(),
            size_classes: Vec::new(),
            // 9101 = gRPC (9001) + 100. Same +100 pattern as
            // gateway. Only used when server TLS is configured.
            health_addr: "0.0.0.0:9101".parse().unwrap(),
            tls: rio_common::tls::TlsConfig::default(),
            hmac_key_path: None,
        }
    }
}

#[derive(Parser, Serialize, Default)]
#[command(
    name = "rio-scheduler",
    about = "DAG-aware build scheduler for rio-build"
)]
struct CliArgs {
    /// gRPC listen address for SchedulerService + WorkerService
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    listen_addr: Option<String>,

    /// rio-store gRPC address
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    store_addr: Option<String>,

    /// PostgreSQL connection URL
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    database_url: Option<String>,

    /// Prometheus metrics listen address
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    metrics_addr: Option<std::net::SocketAddr>,

    /// Tick interval for housekeeping (seconds)
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    tick_interval_secs: Option<u64>,

    /// S3 bucket for build-log gzip flush (unset = flush disabled)
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    log_s3_bucket: Option<String>,

    /// S3 key prefix for build logs
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    log_s3_prefix: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // rustls CryptoProvider MUST be installed before any TLS
    // use. kube-leader-election → kube → hyper-rustls enables
    // `ring`; rio-proto → aws-sdk enables `aws-lc-rs`. With BOTH
    // active, rustls 0.23 can't auto-select and PANICS on first
    // TLS handshake (the Lease loop's K8s API call). Pin aws-lc-rs.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let cli = CliArgs::parse();
    let cfg: Config = rio_common::config::load("scheduler", cli)?;
    let _otel_guard = rio_common::observability::init_tracing("scheduler")?;

    // Client TLS init BEFORE connect_store. One config, all outgoing
    // connections. server_name is a fallback; K8s DNS addressing
    // means :authority from the URL ("rio-store") is the actual SAN
    // match, so this just needs to be A valid SAN of SOME target.
    rio_proto::client::init_client_tls(
        rio_common::tls::load_client_tls(&cfg.tls)
            .map_err(|e| anyhow::anyhow!("TLS config: {e}"))?,
    );
    if cfg.tls.is_configured() {
        info!("client mTLS enabled for outgoing gRPC");
    }

    anyhow::ensure!(
        !cfg.store_addr.is_empty(),
        "store_addr is required (set --store-addr, RIO_STORE_ADDR, or scheduler.toml)"
    );
    anyhow::ensure!(
        !cfg.database_url.is_empty(),
        "database_url is required (set --database-url, RIO_DATABASE_URL, or scheduler.toml)"
    );

    let _root_guard = tracing::info_span!("scheduler", component = "scheduler").entered();
    info!(
        version = env!("CARGO_PKG_VERSION"),
        "starting rio-scheduler"
    );

    // Graceful shutdown: cancelled on SIGTERM/SIGINT. Cloned into each
    // background loop; .cancelled_owned() for serve_with_shutdown.
    // Enables atexit handlers (LLVM coverage profraw flush, tracing
    // shutdown) by letting main() return normally.
    //
    // Shutdown chain for the actor: serve returns → SchedulerGrpc +
    // AdminService drop their ActorHandle clones → tick-loop + lease-
    // loop also break and drop theirs → all mpsc::Sender clones drop →
    // actor's rx.recv() returns None → actor exits → drops
    // event_persist_tx → event-persister also exits (channel-close).
    // event_log::spawn doesn't need a token; it self-terminates.
    let shutdown = rio_common::signal::shutdown_signal();

    rio_common::observability::init_metrics(cfg.metrics_addr)?;
    rio_scheduler::describe_metrics();

    // Connect to PostgreSQL
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(10)
        .connect(&cfg.database_url)
        .await?;

    info!("connected to PostgreSQL");

    sqlx::migrate!("../migrations").run(&pool).await?;
    info!("database migrations applied");

    let db = SchedulerDb::new(pool.clone());

    // Connect to store for scheduler-side cache checks (closes TOCTOU between
    // gateway FindMissingPaths and DAG merge). Non-fatal if connect fails;
    // the actor will skip cache checks and log a warning.
    let store_client = match rio_proto::client::connect_store(&cfg.store_addr).await {
        Ok(client) => {
            info!(store_addr = %cfg.store_addr, "connected to store for cache checks");
            Some(client)
        }
        Err(e) => {
            tracing::warn!(
                store_addr = %cfg.store_addr,
                error = %e,
                "failed to connect to store; scheduler-side cache check disabled"
            );
            None
        }
    };

    // Shared log ring buffers. Written by the BuildExecution recv task
    // (inside SchedulerGrpc), drained by the flusher, read by AdminService.
    let log_buffers = std::sync::Arc::new(rio_scheduler::logs::LogBuffers::new());

    // Log flusher + AdminService S3: both need the same S3 client (if
    // configured). Build it once, clone where needed.
    // Without RIO_LOG_S3_BUCKET, logs are ring-buffer-only (lost on
    // restart, still live-servable while running) and AdminService can
    // only serve active-derivation logs.
    let (log_flush_tx, admin_s3) = match &cfg.log_s3_bucket {
        Some(bucket) => {
            let aws_cfg = aws_config::defaults(aws_config::BehaviorVersion::latest())
                .load()
                .await;
            let s3 = aws_sdk_s3::Client::new(&aws_cfg);
            let (flush_tx, flush_rx) = tokio::sync::mpsc::channel(1000);
            let flusher = rio_scheduler::logs::LogFlusher::new(
                s3.clone(),
                bucket.clone(),
                cfg.log_s3_prefix.clone(),
                pool.clone(),
                Arc::clone(&log_buffers),
            );
            let _flusher_handle = flusher.spawn(flush_rx);
            info!(bucket = %bucket, prefix = %cfg.log_s3_prefix, "log flusher spawned");
            (Some(flush_tx), Some((s3, bucket.clone())))
        }
        None => {
            tracing::warn!(
                "RIO_LOG_S3_BUCKET not set; build logs will be ring-buffer-only \
                 (lost on scheduler restart, AdminService can't serve completed logs)"
            );
            (None, None)
        }
    };

    // Emit cutoff gauges BEFORE spawn. Static config — set once,
    // never changes. Operators correlate with class_queue_depth:
    // "small=30s cutoff and 100 queued there → scale small pool."
    // Empty config → no gauges emitted (size-classes disabled).
    for class in &cfg.size_classes {
        // Reject NaN/inf/zero/negative cutoffs at startup. TOML supports
        // `nan` and `inf` as float literals; without this check, a typo
        // like `cutoff_secs = nan` would crash the scheduler on every
        // dispatch (the pre-total_cmp sort panicked on NaN). Fail fast
        // with a message pointing at the offending class.
        anyhow::ensure!(
            class.cutoff_secs.is_finite() && class.cutoff_secs > 0.0,
            "size_classes[{}].cutoff_secs must be finite and positive, got {}",
            class.name,
            class.cutoff_secs
        );
        metrics::gauge!("rio_scheduler_cutoff_seconds", "class" => class.name.clone())
            .set(class.cutoff_secs);
    }
    if !cfg.size_classes.is_empty() {
        info!(
            classes = ?cfg.size_classes.iter().map(|c| &c.name).collect::<Vec<_>>(),
            "size-class routing enabled"
        );
    }

    // ---- Leader election (gated on RIO_LEASE_NAME) ----
    // None → non-K8s mode: is_leader=true immediately, generation
    // stays at 1. VM tests and single-scheduler deployments hit
    // this path.
    //
    // Some → K8s mode: is_leader=false until the lease loop
    // acquires. Standby replicas merge DAGs (state warm) but
    // don't dispatch (dispatch_ready early-returns). On acquire,
    // the lease loop increments generation and flips is_leader;
    // workers see the new gen in their next heartbeat and reject
    // stale-gen assignments from the old leader.
    //
    // The generation Arc is constructed HERE (not inside the
    // actor) so both the actor and the lease task share the same
    // instance. spawn_with_leader injects it into the actor,
    // REPLACING the actor's default Arc(1) — same init value,
    // shared reference.
    let lease_cfg = rio_scheduler::lease::LeaseConfig::from_env();
    let generation = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1));
    let leader = match &lease_cfg {
        Some(cfg) => {
            info!(
                lease = %cfg.lease_name,
                namespace = %cfg.namespace,
                holder = %cfg.holder_id,
                "lease-based leader election enabled"
            );
            rio_scheduler::lease::LeaderState::pending(Arc::clone(&generation))
        }
        None => {
            info!("no RIO_LEASE_NAME set; running as sole leader (non-K8s mode)");
            rio_scheduler::lease::LeaderState::always_leader(Arc::clone(&generation))
        }
    };
    // Clone for the health toggle loop + lease loop BEFORE
    // moving into spawn. Both need the same shared Arcs the actor
    // gets; spawn_with_leader consumes the LeaderState.
    let is_leader_for_health = Arc::clone(&leader.is_leader);
    let recovery_complete_for_lease = Arc::clone(&leader.recovery_complete);

    // Spawn the event-log persister. Bounded mpsc + single drain
    // task → FIFO write ordering (fire-and-forget spawns would
    // race on the PG pool). emit_build_event try_sends here; if
    // backed up, the broadcast still carries the event — only a
    // mid-backlog gateway reconnect loses it.
    let event_persist_tx = rio_scheduler::event_log::spawn(pool.clone());

    // Load HMAC signer for assignment tokens. None path = disabled
    // (unsigned tokens, dev mode). Bad path / empty file = startup
    // error (operator configured it, failing silently = workers can
    // upload arbitrary paths = security surprise).
    let hmac_signer = rio_common::hmac::HmacSigner::load(cfg.hmac_key_path.as_deref())
        .map_err(|e| anyhow::anyhow!("HMAC key load: {e}"))?;
    if hmac_signer.is_some() {
        info!("HMAC assignment token signing enabled");
    }

    // Spawn the DAG actor — now with the shared leader state.
    let actor = ActorHandle::spawn_with_leader(
        db,
        store_client,
        log_flush_tx,
        cfg.size_classes,
        Some(leader),
        Some(event_persist_tx),
        hmac_signer,
    );
    info!("DAG actor spawned");

    // Spawn the lease loop (if configured). AFTER actor spawn so
    // the actor's generation is already the shared Arc — when the
    // lease acquires and increments, the actor sees it.
    if let Some(lease_cfg) = lease_cfg {
        // Reconstruct LeaderState from the SAME Arcs. We moved
        // the original into spawn_with_leader; clone the
        // underlying atomics back out. (They're Arc<Atomic*>;
        // clone is cheap and shares the instance.)
        let lease_state = rio_scheduler::lease::LeaderState {
            generation,
            is_leader: Arc::clone(&is_leader_for_health),
            recovery_complete: recovery_complete_for_lease,
        };
        // Pass actor.clone() for fire-and-forget LeaderAcquired.
        // The lease loop does NOT block on recovery — it keeps
        // renewing while the actor handles LeaderAcquired.
        rio_common::task::spawn_monitored(
            "lease-loop",
            rio_scheduler::lease::run_lease_loop(
                lease_cfg,
                lease_state,
                actor.clone(),
                shutdown.clone(),
            ),
        );
    }

    // grpc.health.v1.Health. SERVING iff is_leader. K8s Service
    // routes only to SERVING pods → only to the leader. Standby
    // replicas stay live (liveness probe passes) but not ready.
    //
    // Toggle loop tracks is_leader every 1s. In non-K8s mode
    // is_leader=true immediately → first iteration sets SERVING.
    // In K8s standby mode: stays NOT_SERVING until lease acquire.
    //
    // r[impl ctrl.probe.named-service]
    // CRITICAL: the K8s readinessProbe MUST specify
    // `grpc.service: rio.scheduler.SchedulerService` — the named
    // service, not empty-string. set_not_serving only affects the
    // named service (see the health_toggle_not_serving test).
    // Empty-string stays SERVING forever after the first
    // set_serving, so a standby would pass readiness on "" and
    // K8s would route to it. The scheduler.yaml manifest MUST have:
    //   readinessProbe:
    //     grpc:
    //       port: 9001
    //       service: rio.scheduler.SchedulerService
    let (health_reporter, health_service) = tonic_health::server::health_reporter();
    {
        // Own task for the toggle loop. Captures the reporter
        // clone (HealthReporter is Clone) and the is_leader Arc.
        // Checks every second — short enough that leadership
        // transitions surface quickly (K8s readiness probe period
        // is typically 5-10s, so we update before it checks).
        //
        // Why not watch the AtomicBool directly: there's no async
        // wake-on-change for atomics. A tokio::sync::watch channel
        // would give that, but then the lease task and
        // dispatch_ready both need to be adapted to use watch
        // instead of AtomicBool. Polling at 1Hz is simpler and
        // the 1s lag is imperceptible (K8s probes poll slower).
        //
        // Edge-triggered: only call set_serving/set_not_serving
        // on a TRANSITION, not every iteration. tonic-health
        // set_* is an async RwLock write + broadcast to Watch
        // subscribers — not expensive, but calling it 1Hz for
        // no reason wakes any grpc Health.Watch clients (K8s
        // probes don't use Watch, but other tooling might).
        let reporter = health_reporter.clone();
        let is_leader = is_leader_for_health;
        let health_shutdown = shutdown.clone();
        rio_common::task::spawn_monitored("health-toggle-loop", async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            // `prev`: what we LAST set the reporter to. Starts
            // None so the first iteration unconditionally sets
            // (either SERVING or NOT_SERVING depending on
            // is_leader at that moment). Option<bool> not bool:
            // "haven't set anything yet" is distinct from both
            // true and false.
            let mut prev: Option<bool> = None;
            loop {
                tokio::select! {
                    _ = health_shutdown.cancelled() => {
                        tracing::debug!("health-toggle-loop shutting down");
                        break;
                    }
                    _ = interval.tick() => {}
                }
                let now = is_leader.load(std::sync::atomic::Ordering::Relaxed);
                if prev != Some(now) {
                    if now {
                        reporter
                            .set_serving::<SchedulerServiceServer<SchedulerGrpc>>()
                            .await;
                        tracing::debug!("health: SERVING (is_leader=true)");
                    } else {
                        reporter
                            .set_not_serving::<SchedulerServiceServer<SchedulerGrpc>>()
                            .await;
                        tracing::debug!("health: NOT_SERVING (is_leader=false, standby)");
                    }
                    prev = Some(now);
                }
            }
        });
    }

    // Create gRPC services. All three get the SAME Arc<LogBuffers>:
    // SchedulerGrpc writes, AdminService reads (live), LogFlusher drains
    // (on completion). The test-only new_for_tests() constructor makes a
    // SEPARATE buffer — it's cfg(test) gated so prod can't accidentally
    // use it and silently break the pipeline.
    let grpc_service =
        SchedulerGrpc::with_log_buffers(actor.clone(), Arc::clone(&log_buffers), pool.clone());

    // Background refresh for ClusterStatus.store_size_bytes — 60s PG poll
    // on the shared DB. Keeps ClusterStatus fast (autoscaler's 30s path).
    let store_size_bytes = rio_scheduler::admin::spawn_store_size_refresh(pool.clone());

    let admin_service = AdminServiceImpl::new(
        log_buffers,
        admin_s3,
        pool,
        actor.clone(),
        cfg.store_addr.clone(),
        store_size_bytes,
    );

    // Start periodic tick task
    let tick_actor = actor.clone();
    let tick_interval = std::time::Duration::from_secs(cfg.tick_interval_secs);
    let tick_shutdown = shutdown.clone();
    rio_common::task::spawn_monitored("tick-loop", async move {
        let mut interval = tokio::time::interval(tick_interval);
        loop {
            tokio::select! {
                _ = tick_shutdown.cancelled() => {
                    tracing::debug!("tick-loop shutting down");
                    break;
                }
                _ = interval.tick() => {}
            }
            // If the actor is dead (channel closed), stop ticking.
            if tick_actor
                .try_send(rio_scheduler::actor::ActorCommand::Tick)
                .is_err()
                && !tick_actor.is_alive()
            {
                tracing::warn!("actor channel closed, stopping tick loop");
                break;
            }
        }
    });

    // Start gRPC server
    let listen_addr: std::net::SocketAddr = cfg.listen_addr.parse()?;
    let max_message_size = rio_proto::max_message_size();

    // Server TLS: if configured, the main port requires client certs.
    // K8s gRPC probes can't do mTLS — so we spawn a SECOND server on
    // `health_addr` with ONLY the health service, plaintext, SHARING
    // the same HealthReporter. The health-toggle loop above writes
    // to that reporter → both servers see the status change → probe
    // on the plaintext port correctly reflects leadership.
    //
    // If the plaintext port had a FRESH reporter, it would never be
    // set to NOT_SERVING (no toggle loop for it) → standby always
    // appears Ready → K8s routes to a non-leader → cluster split.
    // Shared reporter is load-bearing.
    let server_tls = rio_common::tls::load_server_tls(&cfg.tls)
        .map_err(|e| anyhow::anyhow!("server TLS config: {e}"))?;

    // We need a second `HealthServer` instance for the plaintext
    // listener that shares the SAME status map as the main one.
    // `HealthServer<HealthService>` is Clone (tonic-generated
    // wrapper over an `Arc<RwLock<HashMap>>`), so cloning shares
    // the underlying state — the toggle loop writes once, both
    // servers see it.
    let health_service_plain = health_service.clone();

    if let Some(ref tls) = server_tls {
        // Spawn the plaintext health-only server. It runs
        // concurrently with the main (blocking) serve() below.
        let health_addr = cfg.health_addr;
        info!(addr = %health_addr, "spawning plaintext health server for K8s probes (mTLS on main port)");
        let health_plain_shutdown = shutdown.clone();
        rio_common::task::spawn_monitored("health-plaintext", async move {
            if let Err(e) = tonic::transport::Server::builder()
                .add_service(health_service_plain)
                .serve_with_shutdown(health_addr, health_plain_shutdown.cancelled_owned())
                .await
            {
                tracing::error!(error = %e, "plaintext health server failed");
            }
        });
        info!("server mTLS enabled — clients must present CA-signed certs");
        // Shadow `health_service` is already moved into the main
        // builder below (it's used once). We cloned BEFORE moving.
        // Discard `tls` temp — we only needed the `is_some()` check
        // for the conditional. The actual ServerTlsConfig goes into
        // `builder.tls_config()` next. `let _ = tls` suppresses the
        // unused-borrow warning from the ref pattern.
        let _ = tls;
    }

    info!(
        listen_addr = %listen_addr,
        store_addr = %cfg.store_addr,
        max_message_size,
        log_s3_bucket = ?cfg.log_s3_bucket,
        tls = server_tls.is_some(),
        "starting gRPC server"
    );

    let mut builder = tonic::transport::Server::builder();
    if let Some(tls) = server_tls {
        builder = builder.tls_config(tls)?;
    }
    builder
        .add_service(health_service)
        .add_service(
            SchedulerServiceServer::new(grpc_service.clone())
                .max_decoding_message_size(max_message_size)
                .max_encoding_message_size(max_message_size),
        )
        .add_service(
            WorkerServiceServer::new(grpc_service)
                .max_decoding_message_size(max_message_size)
                .max_encoding_message_size(max_message_size),
        )
        .add_service(
            AdminServiceServer::new(admin_service)
                .max_decoding_message_size(max_message_size)
                .max_encoding_message_size(max_message_size),
        )
        .serve_with_shutdown(listen_addr, shutdown.cancelled_owned())
        .await?;

    info!("scheduler shut down cleanly");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_are_stable() {
        let d = Config::default();
        assert_eq!(d.listen_addr, "0.0.0.0:9001");
        assert_eq!(d.metrics_addr.to_string(), "0.0.0.0:9091");
        assert_eq!(d.tick_interval_secs, 10);
        // Phase2a required these; no default.
        assert!(d.store_addr.is_empty());
        assert!(d.database_url.is_empty());
        // Phase2b additions — off by default.
        assert_eq!(d.log_s3_bucket, None);
        assert_eq!(d.log_s3_prefix, "logs");
        // Size-classes: optional feature, off by default.
        assert!(d.size_classes.is_empty());
        // Phase3b: plaintext health port for K8s probes when mTLS on.
        assert_eq!(d.health_addr.to_string(), "0.0.0.0:9101");
        // Phase3b: TLS off by default (dev mode, VM tests).
        assert!(!d.tls.is_configured());
    }

    #[test]
    fn cli_args_parse_help() {
        use clap::CommandFactory;
        CliArgs::command().debug_assert();
    }

    // -----------------------------------------------------------------------
    // gRPC health service wiring smoke tests.
    //
    // These validate the tonic-health integration pattern used by all
    // three binaries (scheduler/store/gateway). They live HERE (not in
    // each crate) because the pattern is identical and testing it once
    // proves the wiring — the per-crate variation is just WHEN
    // set_serving is called (post-migrations vs post-connect), which
    // main() sequences and the VM tests cover e2e.
    // -----------------------------------------------------------------------

    /// Spin up a tonic server with ONLY the health service on an
    /// ephemeral port, return the address + reporter handle.
    ///
    /// The server task is detached — fine for tests; the process exits
    /// when the test fn returns. No graceful shutdown needed (no
    /// resources to clean up; the listener's socket closes on drop).
    async fn spawn_health_server() -> (std::net::SocketAddr, tonic_health::server::HealthReporter) {
        let (reporter, service) = tonic_health::server::health_reporter();
        // Port 0 → kernel assigns. Read back the bound addr before
        // spawning — serve() consumes the listener so we can't ask later.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(service)
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        (addr, reporter)
    }

    /// Fresh health_reporter() → NOT_SERVING until set_serving is called.
    /// K8s readiness probe failing during boot is correct: the Service
    /// shouldn't route to a half-initialized pod.
    ///
    /// tonic-health's DEFAULT behavior for a service that was never
    /// registered is "Unknown" (gRPC NotFound). The empty-string "" check
    /// (whole server) defaults to SERVING unless explicitly set otherwise.
    /// So we check a NAMED service that hasn't been set — that's the
    /// realistic boot race: K8s probes before main() reaches set_serving.
    #[tokio::test]
    async fn health_not_serving_before_set() -> anyhow::Result<()> {
        use tonic_health::pb::{HealthCheckRequest, health_client::HealthClient};

        let (addr, _reporter) = spawn_health_server().await;
        let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))?
            .connect()
            .await?;
        let mut client = HealthClient::new(channel);

        // Named service, never registered → NotFound (which K8s treats
        // as a probe failure, same as NOT_SERVING for readiness purposes).
        let result = client
            .check(HealthCheckRequest {
                service: "rio.scheduler.SchedulerService".into(),
            })
            .await;
        let status = result.expect_err("unregistered service should be NotFound");
        assert_eq!(
            status.code(),
            tonic::Code::NotFound,
            "probe failure before boot completes — K8s won't route to this pod"
        );
        Ok(())
    }

    #[tokio::test]
    async fn health_serving_after_set() -> anyhow::Result<()> {
        use tonic_health::pb::{
            HealthCheckRequest, health_check_response::ServingStatus, health_client::HealthClient,
        };

        let (addr, reporter) = spawn_health_server().await;

        // The same call main() makes. Type param = the service impl.
        reporter
            .set_serving::<SchedulerServiceServer<SchedulerGrpc>>()
            .await;

        let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))?
            .connect()
            .await?;
        let mut client = HealthClient::new(channel);

        // tonic-health derives the name from S::NAME (NamedService trait,
        // tonic-generated from proto `package rio.scheduler; service
        // SchedulerService`). The test originally guessed ".v1" — there
        // isn't one. This assertion CATCHES proto-package drift: if
        // someone adds versioning to scheduler.proto, this fails and
        // whoever did it updates the K8s probe config to match.
        let resp = client
            .check(HealthCheckRequest {
                service: "rio.scheduler.SchedulerService".into(),
            })
            .await?
            .into_inner();
        assert_eq!(
            ServingStatus::try_from(resp.status)?,
            ServingStatus::Serving,
            "set_serving → SERVING → K8s routes to this pod"
        );

        // The empty-string "whole server" check — K8s probes send this
        // when no service name is configured (the common case; per-
        // service granularity is rarely needed). tonic-health registers
        // under BOTH the named service AND "" on any set_serving call.
        let resp_empty = client
            .check(HealthCheckRequest {
                service: String::new(),
            })
            .await?
            .into_inner();
        assert_eq!(
            ServingStatus::try_from(resp_empty.status)?,
            ServingStatus::Serving,
            "empty-string check also SERVING after any set_serving"
        );
        Ok(())
    }

    /// The health service is Clone, so we can serve it from TWO
    /// tonic servers (mTLS main port + plaintext health port)
    /// with ONE shared HealthReporter. The toggle loop writes to the
    /// reporter once; BOTH servers see the status change. If we created
    /// a fresh reporter for the plaintext port, it would never toggle
    /// → standby always SERVING → K8s routes to non-leader.
    #[tokio::test]
    async fn health_service_clone_shares_reporter_state() -> anyhow::Result<()> {
        use tonic_health::pb::{
            HealthCheckRequest, health_check_response::ServingStatus, health_client::HealthClient,
        };

        let (reporter, health_service) = tonic_health::server::health_reporter();
        let health_service_clone = health_service.clone();

        // Spawn TWO servers, each on its own port, each with its
        // own clone of the health service.
        let l1 = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr1 = l1.local_addr()?;
        let l2 = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr2 = l2.local_addr()?;

        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(health_service)
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(l1))
                .await
                .unwrap();
        });
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(health_service_clone)
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(l2))
                .await
                .unwrap();
        });

        let ch1 = tonic::transport::Channel::from_shared(format!("http://{addr1}"))?
            .connect()
            .await?;
        let ch2 = tonic::transport::Channel::from_shared(format!("http://{addr2}"))?
            .connect()
            .await?;
        let mut c1 = HealthClient::new(ch1);
        let mut c2 = HealthClient::new(ch2);

        // Set SERVING via the ONE reporter. BOTH servers should see it.
        reporter
            .set_serving::<SchedulerServiceServer<SchedulerGrpc>>()
            .await;
        let req = || HealthCheckRequest {
            service: "rio.scheduler.SchedulerService".into(),
        };
        let s1 = ServingStatus::try_from(c1.check(req()).await?.into_inner().status)?;
        let s2 = ServingStatus::try_from(c2.check(req()).await?.into_inner().status)?;
        assert_eq!(s1, ServingStatus::Serving, "server 1 sees SERVING");
        assert_eq!(
            s2,
            ServingStatus::Serving,
            "server 2 (cloned service) ALSO sees SERVING — shared state"
        );

        // Toggle NOT_SERVING. BOTH should flip — proving the clone
        // shares the underlying status map, not a snapshot.
        reporter
            .set_not_serving::<SchedulerServiceServer<SchedulerGrpc>>()
            .await;
        let s1 = ServingStatus::try_from(c1.check(req()).await?.into_inner().status)?;
        let s2 = ServingStatus::try_from(c2.check(req()).await?.into_inner().status)?;
        assert_eq!(s1, ServingStatus::NotServing);
        assert_eq!(
            s2,
            ServingStatus::NotServing,
            "clone tracks toggles — standby on plaintext port would \
             correctly show NOT_SERVING → K8s excludes it"
        );
        Ok(())
    }

    /// set_not_serving flips back. The health toggle loop uses this
    /// to gate on is_leader: standby replicas stay NOT_SERVING so the
    /// K8s Service routes only to the leader.
    #[tokio::test]
    async fn health_toggle_not_serving() -> anyhow::Result<()> {
        use tonic_health::pb::{
            HealthCheckRequest, health_check_response::ServingStatus, health_client::HealthClient,
        };

        let (addr, reporter) = spawn_health_server().await;
        reporter
            .set_serving::<SchedulerServiceServer<SchedulerGrpc>>()
            .await;
        let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))?
            .connect()
            .await?;
        let mut client = HealthClient::new(channel);

        // SERVING → NOT_SERVING → SERVING. The leader-toggle pattern.
        //
        // IMPORTANT: `set_not_serving::<S>()` only flips the NAMED
        // service, NOT the empty-string "" check. The first
        // `set_serving` registers "" as SERVING and nothing toggles it
        // back. So the K8s readinessProbe MUST be configured with
        // `grpc.service: rio.scheduler.SchedulerService` explicitly.
        // The kustomize manifest needs this. Without it, a standby
        // scheduler would pass readiness on the "" check and K8s would
        // route to a non-leader.
        //
        // async fn not closure: a closure borrowing `&mut client` +
        // returning an async block that uses the borrow doesn't have a
        // stable-Rust spelling (the borrow's lifetime can't outlive the
        // closure call but the async block escapes). `async fn` dodges
        // this entirely.
        async fn check(
            client: &mut HealthClient<tonic::transport::Channel>,
        ) -> Result<ServingStatus, tonic::Status> {
            client
                .check(HealthCheckRequest {
                    // NAMED service, not "" — set_not_serving only
                    // affects this. See above for why this matters.
                    service: "rio.scheduler.SchedulerService".into(),
                })
                .await
                .map(|r| ServingStatus::try_from(r.into_inner().status).unwrap())
        }

        assert_eq!(check(&mut client).await?, ServingStatus::Serving);

        reporter
            .set_not_serving::<SchedulerServiceServer<SchedulerGrpc>>()
            .await;
        assert_eq!(
            check(&mut client).await?,
            ServingStatus::NotServing,
            "set_not_serving → K8s stops routing (standby scheduler)"
        );

        reporter
            .set_serving::<SchedulerServiceServer<SchedulerGrpc>>()
            .await;
        assert_eq!(
            check(&mut client).await?,
            ServingStatus::Serving,
            "re-acquired leadership → resume traffic"
        );
        Ok(())
    }
}
