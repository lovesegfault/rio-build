//! rio-gateway binary entry point.

use clap::Parser;
use tracing::info;

#[derive(Parser, Debug)]
#[command(
    name = "rio-gateway",
    about = "SSH gateway and Nix protocol frontend for rio-build"
)]
struct Args {
    /// SSH listen address
    #[arg(long, env = "RIO_LISTEN_ADDR", default_value = "0.0.0.0:2222")]
    listen_addr: std::net::SocketAddr,

    /// rio-scheduler gRPC address
    #[arg(long, env = "RIO_SCHEDULER_ADDR")]
    scheduler_addr: String,

    /// rio-store gRPC address
    #[arg(long, env = "RIO_STORE_ADDR")]
    store_addr: String,

    /// SSH host key path
    #[arg(long, env = "RIO_HOST_KEY", default_value = "/tmp/rio_host_key")]
    host_key: std::path::PathBuf,

    /// Authorized keys file path
    #[arg(
        long,
        env = "RIO_AUTHORIZED_KEYS",
        default_value = "/tmp/rio_authorized_keys"
    )]
    authorized_keys: std::path::PathBuf,

    /// Prometheus metrics listen address
    #[arg(long, env = "RIO_METRICS_ADDR", default_value = "0.0.0.0:9090")]
    metrics_addr: std::net::SocketAddr,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let log_format = rio_common::observability::log_format_from_env();
    rio_common::observability::init_logging(log_format, None)?;

    let _root_guard = tracing::info_span!("gateway", component = "gateway").entered();
    info!(version = env!("CARGO_PKG_VERSION"), "starting rio-gateway");

    rio_common::observability::init_metrics(args.metrics_addr)?;

    info!(
        listen_addr = %args.listen_addr,
        scheduler_addr = %args.scheduler_addr,
        store_addr = %args.store_addr,
        "rio-gateway is a placeholder — implementation in Step 6"
    );

    Ok(())
}
