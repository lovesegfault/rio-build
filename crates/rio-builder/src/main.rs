use anyhow::Result;
use rio_common::Platform;
use tracing::{error, info, Level};

mod builder;
mod executor;

use builder::Builder;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .with_target(false)
        .init();

    info!("Starting Rio Builder v{}", env!("CARGO_PKG_VERSION"));

    // TODO: Load configuration from file/env
    let dispatcher_endpoint = std::env::var("DISPATCHER_ENDPOINT")
        .unwrap_or_else(|_| "http://localhost:50051".to_string());

    // Detect current platform
    let current_platform = if cfg!(target_os = "linux") && cfg!(target_arch = "x86_64") {
        Platform::X86_64Linux
    } else if cfg!(target_os = "linux") && cfg!(target_arch = "aarch64") {
        Platform::Aarch64Linux
    } else if cfg!(target_os = "macos") && cfg!(target_arch = "x86_64") {
        Platform::X86_64Darwin
    } else if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
        Platform::Aarch64Darwin
    } else {
        Platform::Other(format!(
            "{}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        ))
    };

    info!("Builder platform: {}", current_platform);

    // Create builder
    let mut builder = Builder::new(
        dispatcher_endpoint,
        vec![current_platform],
        vec![], // No special features for now
    );

    // Connect to dispatcher
    if let Err(e) = builder.connect().await {
        error!("Failed to connect to dispatcher: {}", e);
        return Err(e);
    }

    // Register with dispatcher
    if let Err(e) = builder.register().await {
        error!("Failed to register with dispatcher: {}", e);
        return Err(e);
    }

    // Start heartbeat loop
    if let Err(e) = builder.start_heartbeat_loop().await {
        error!("Failed to start heartbeat loop: {}", e);
        return Err(e);
    }

    info!(
        "Rio Builder {} started successfully and registered with dispatcher",
        builder.id()
    );

    // Keep running
    tokio::signal::ctrl_c().await?;
    info!("Shutting down...");

    Ok(())
}
