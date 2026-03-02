//! rio-gateway binary entry point.
//!
//! Connects to the scheduler and store gRPC services, then starts an
//! SSH server that speaks the Nix worker protocol.

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
    rio_common::observability::init_from_env()?;

    let _root_guard = tracing::info_span!("gateway", component = "gateway").entered();
    info!(version = env!("CARGO_PKG_VERSION"), "starting rio-gateway");

    rio_common::observability::init_metrics(args.metrics_addr)?;

    // Connect to gRPC services
    info!(addr = %args.store_addr, "connecting to store service");
    let store_client = rio_proto::client::connect_store(&args.store_addr).await?;

    info!(addr = %args.scheduler_addr, "connecting to scheduler service");
    let scheduler_client = rio_proto::client::connect_scheduler(&args.scheduler_addr).await?;

    // Load SSH keys
    let host_key = rio_gateway::load_or_generate_host_key(&args.host_key)?;
    let authorized_keys = rio_gateway::load_authorized_keys(&args.authorized_keys)?;

    // Start SSH server
    let server = rio_gateway::GatewayServer::new(store_client, scheduler_client, authorized_keys);

    info!(
        listen_addr = %args.listen_addr,
        scheduler_addr = %args.scheduler_addr,
        store_addr = %args.store_addr,
        "rio-gateway ready"
    );

    server.run(host_key, args.listen_addr).await?;

    Ok(())
}
