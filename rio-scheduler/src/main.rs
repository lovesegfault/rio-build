//! rio-scheduler binary entry point.
//!
//! Starts the gRPC server, connects to PostgreSQL, and spawns the DAG actor.

use clap::Parser;
use tracing::info;

use rio_proto::scheduler::scheduler_service_server::SchedulerServiceServer;
use rio_proto::worker::worker_service_server::WorkerServiceServer;
use rio_scheduler::actor::ActorHandle;
use rio_scheduler::db::SchedulerDb;
use rio_scheduler::grpc::SchedulerGrpc;

#[derive(Parser, Debug)]
#[command(
    name = "rio-scheduler",
    about = "DAG-aware build scheduler for rio-build"
)]
struct Args {
    /// gRPC listen address for SchedulerService + WorkerService
    #[arg(
        long,
        env = "RIO_SCHEDULER_LISTEN_ADDR",
        default_value = "0.0.0.0:9001"
    )]
    listen_addr: String,

    /// rio-store gRPC address
    #[arg(long, env = "RIO_STORE_ADDR")]
    store_addr: String,

    /// PostgreSQL connection URL
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    /// Prometheus metrics listen address
    #[arg(long, env = "RIO_METRICS_ADDR", default_value = "0.0.0.0:9091")]
    metrics_addr: std::net::SocketAddr,

    /// Tick interval for housekeeping (seconds)
    #[arg(long, env = "RIO_TICK_INTERVAL_SECS", default_value = "10")]
    tick_interval_secs: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let log_format = rio_common::observability::log_format_from_env();
    rio_common::observability::init_logging(log_format, None)?;

    let _root_guard = tracing::info_span!("scheduler", component = "scheduler").entered();
    info!(
        version = env!("CARGO_PKG_VERSION"),
        "starting rio-scheduler"
    );

    rio_common::observability::init_metrics(args.metrics_addr)?;

    // Connect to PostgreSQL
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(10)
        .connect(&args.database_url)
        .await?;

    info!("connected to PostgreSQL");

    sqlx::migrate!("../migrations").run(&pool).await?;
    info!("database migrations applied");

    let db = SchedulerDb::new(pool);

    // Connect to store for scheduler-side cache checks (closes TOCTOU between
    // gateway FindMissingPaths and DAG merge). Non-fatal if connect fails;
    // the actor will skip cache checks and log a warning.
    let store_client = match rio_proto::client::connect_store(&args.store_addr).await {
        Ok(client) => {
            info!(store_addr = %args.store_addr, "connected to store for cache checks");
            Some(client)
        }
        Err(e) => {
            tracing::warn!(
                store_addr = %args.store_addr,
                error = %e,
                "failed to connect to store; scheduler-side cache check disabled"
            );
            None
        }
    };

    // Spawn the DAG actor
    let actor = ActorHandle::spawn(db, store_client);
    info!("DAG actor spawned");

    // Create gRPC service
    let grpc_service = SchedulerGrpc::new(actor.clone());

    // Start periodic tick task
    let tick_actor = actor.clone();
    let tick_interval = std::time::Duration::from_secs(args.tick_interval_secs);
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
    let listen_addr: std::net::SocketAddr = args.listen_addr.parse()?;
    let max_message_size = rio_proto::max_message_size();

    info!(
        listen_addr = %listen_addr,
        store_addr = %args.store_addr,
        max_message_size,
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
        .serve(listen_addr)
        .await?;

    Ok(())
}
