use anyhow::Result;
use std::net::SocketAddr;
use tracing::{info, Level};

mod build_queue;
mod builder_pool;
mod dispatcher;
mod grpc_server;
mod scheduler;
mod ssh_server;

use builder_pool::BuilderPool;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .with_target(false)
        .init();

    info!("Starting Rio Dispatcher v{}", env!("CARGO_PKG_VERSION"));

    // Initialize builder pool
    let builder_pool = BuilderPool::new();

    // Configure gRPC server address
    let grpc_addr: SocketAddr = "0.0.0.0:50051".parse()?;

    info!("gRPC server will listen on {}", grpc_addr);

    // Start gRPC server
    // This will block until shutdown
    grpc_server::start_grpc_server(grpc_addr, builder_pool).await?;

    info!("Shutting down...");
    Ok(())
}
