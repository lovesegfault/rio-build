use anyhow::Result;
use camino::Utf8PathBuf;
use clap::Parser;
use std::net::SocketAddr;
use tracing::{Level, info};

mod build_queue;
mod builder_pool;
mod dispatcher;
mod grpc_server;
mod nix_store;
mod scheduler;
mod ssh_server;

use build_queue::BuildQueue;
use builder_pool::BuilderPool;
use scheduler::Scheduler;
use ssh_server::{SshConfig, SshHandler, SshServer};

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
    ssh_host_key: Option<Utf8PathBuf>,

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

    // Initialize shared components
    let builder_pool = BuilderPool::new();
    let _build_queue = BuildQueue::new();
    let _scheduler = Scheduler::new(builder_pool.clone());

    // Parse addresses
    let grpc_addr: SocketAddr = cli.grpc_addr.parse()?;
    let ssh_addr: SocketAddr = cli.ssh_addr.parse()?;

    info!("gRPC server will listen on {}", grpc_addr);
    info!("SSH server will listen on {}", ssh_addr);

    // Load or generate SSH host key
    let host_key = SshServer::load_or_generate_host_key(cli.ssh_host_key.as_deref()).await?;
    info!("SSH host key loaded");

    // Create SSH server configuration
    let ssh_config = SshConfig {
        addr: ssh_addr,
        host_key,
    };

    let ssh_server = SshServer::new(ssh_config);
    let ssh_handler = SshHandler::new();

    // Start servers concurrently
    tokio::select! {
        result = grpc_server::start_grpc_server(grpc_addr, builder_pool.clone()) => {
            info!("gRPC server exited");
            result?;
        }
        result = ssh_server.start(ssh_handler) => {
            info!("SSH server exited");
            result?;
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Received Ctrl+C, shutting down...");
        }
    }

    info!("Shutting down...");
    Ok(())
}
