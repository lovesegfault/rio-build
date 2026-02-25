//! rio-gateway binary entry point.
//!
//! Connects to the scheduler and store gRPC services, then starts an
//! SSH server that speaks the Nix worker protocol.

use clap::Parser;
use rio_proto::scheduler::scheduler_service_client::SchedulerServiceClient;
use rio_proto::store::store_service_client::StoreServiceClient;
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

    // Connect to gRPC services
    let max_msg = rio_proto::max_message_size();

    let store_endpoint = format!("http://{}", args.store_addr);
    info!(addr = %store_endpoint, "connecting to store service");
    let store_client = StoreServiceClient::connect(store_endpoint)
        .await?
        .max_decoding_message_size(max_msg)
        .max_encoding_message_size(max_msg);

    let scheduler_endpoint = format!("http://{}", args.scheduler_addr);
    info!(addr = %scheduler_endpoint, "connecting to scheduler service");
    let scheduler_client = SchedulerServiceClient::connect(scheduler_endpoint)
        .await?
        .max_decoding_message_size(max_msg)
        .max_encoding_message_size(max_msg);

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
