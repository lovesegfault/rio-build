//! rio-scheduler binary entry point.

use clap::Parser;
use tracing::info;

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

    info!(
        listen_addr = %args.listen_addr,
        store_addr = %args.store_addr,
        "rio-scheduler is a placeholder — implementation in Step 5"
    );

    Ok(())
}
