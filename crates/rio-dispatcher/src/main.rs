use anyhow::Result;
use clap::Parser;
use std::net::SocketAddr;
use tracing::{Level, info};

mod build_queue;
mod builder_pool;
mod dispatcher;
mod grpc_server;
mod scheduler;
mod ssh_server;

use builder_pool::BuilderPool;

/// Rio Dispatcher - Fleet manager for distributed Nix builds
#[derive(Parser, Debug)]
#[command(name = "rio-dispatcher")]
#[command(version, about, long_about = None)]
struct Cli {
    /// gRPC server address for builder communication
    #[arg(long, env = "RIO_GRPC_ADDR", default_value = "0.0.0.0:50051")]
    grpc_addr: String,

    /// SSH server address for Nix client connections
    #[arg(long, env = "RIO_SSH_ADDR", default_value = "0.0.0.0:2222")]
    ssh_addr: String,

    /// Path to SSH host key (generated if not exists)
    #[arg(long, env = "RIO_SSH_HOST_KEY")]
    ssh_host_key: Option<String>,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long, env = "RIO_LOG_LEVEL", default_value = "info")]
    log_level: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize tracing with configured log level
    let log_level = match cli.log_level.to_lowercase().as_str() {
        "trace" => Level::TRACE,
        "debug" => Level::DEBUG,
        "info" => Level::INFO,
        "warn" => Level::WARN,
        "error" => Level::ERROR,
        _ => Level::INFO,
    };

    tracing_subscriber::fmt()
        .with_max_level(log_level)
        .with_target(false)
        .init();

    info!("Starting Rio Dispatcher v{}", env!("CARGO_PKG_VERSION"));
    info!("gRPC address: {}", cli.grpc_addr);
    info!("SSH address: {}", cli.ssh_addr);

    // Initialize builder pool
    let builder_pool = BuilderPool::new();

    // Parse gRPC server address
    let grpc_addr: SocketAddr = cli.grpc_addr.parse()?;

    info!("gRPC server will listen on {}", grpc_addr);

    // TODO: Start SSH server on cli.ssh_addr
    // TODO: Load/generate SSH host key from cli.ssh_host_key

    // Start gRPC server
    // This will block until shutdown
    grpc_server::start_grpc_server(grpc_addr, builder_pool).await?;

    info!("Shutting down...");
    Ok(())
}
