//! rio-scheduler binary entry point.
//!
//! Starts the gRPC server, connects to PostgreSQL, and spawns the DAG actor.

use clap::Parser;
use serde::{Deserialize, Serialize};
use tracing::info;

use rio_proto::admin::admin_service_server::AdminServiceServer;
use rio_proto::scheduler::scheduler_service_server::SchedulerServiceServer;
use rio_proto::worker::worker_service_server::WorkerServiceServer;
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
    /// S3 bucket for build-log flush (phase2b). `None` = flush disabled.
    /// Env: `RIO_LOG_S3_BUCKET`. Not yet wired to a consumer — C8 does that.
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
    size_classes: Vec<rio_scheduler::assignment::SizeClassConfig>,
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
    let cli = CliArgs::parse();
    let cfg: Config = rio_common::config::load("scheduler", cli)?;
    let _otel_guard = rio_common::observability::init_tracing("scheduler")?;

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

    rio_common::observability::init_metrics(cfg.metrics_addr)?;

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
                log_buffers.clone(),
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
        metrics::gauge!("rio_scheduler_cutoff_seconds", "class" => class.name.clone())
            .set(class.cutoff_secs);
    }
    if !cfg.size_classes.is_empty() {
        info!(
            classes = ?cfg.size_classes.iter().map(|c| &c.name).collect::<Vec<_>>(),
            "size-class routing enabled"
        );
    }

    // Spawn the DAG actor
    let actor = ActorHandle::spawn(db, store_client, log_flush_tx, cfg.size_classes);
    info!("DAG actor spawned");

    // Create gRPC services. All three get the SAME Arc<LogBuffers>:
    // SchedulerGrpc writes, AdminService reads (live), LogFlusher drains
    // (on completion). SchedulerGrpc::new() would make a SEPARATE buffer,
    // silently breaking the whole pipeline — hence with_log_buffers().
    let grpc_service = SchedulerGrpc::with_log_buffers(actor.clone(), log_buffers.clone());
    let admin_service = AdminServiceImpl::new(log_buffers, admin_s3, pool);

    // Start periodic tick task
    let tick_actor = actor.clone();
    let tick_interval = std::time::Duration::from_secs(cfg.tick_interval_secs);
    rio_common::task::spawn_monitored("tick-loop", async move {
        let mut interval = tokio::time::interval(tick_interval);
        loop {
            interval.tick().await;
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

    info!(
        listen_addr = %listen_addr,
        store_addr = %cfg.store_addr,
        max_message_size,
        log_s3_bucket = ?cfg.log_s3_bucket,
        "starting gRPC server"
    );

    tonic::transport::Server::builder()
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
        .serve(listen_addr)
        .await?;

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
        // Phase2c: size-classes off by default (backward compat).
        assert!(d.size_classes.is_empty());
    }

    #[test]
    fn cli_args_parse_help() {
        use clap::CommandFactory;
        CliArgs::command().debug_assert();
    }
}
