use anyhow::Result;
use clap::Parser;
use rio_common::Platform;
use tracing::{Level, error, info};

mod builder;
mod executor;
mod grpc_server;

use builder::Builder;

/// Rio Builder - Worker node for executing Nix builds
#[derive(Parser, Debug)]
#[command(name = "rio-builder")]
#[command(version, about, long_about = None)]
struct Cli {
    /// Dispatcher endpoint to connect to
    #[arg(
        long,
        env = "RIO_DISPATCHER_ENDPOINT",
        default_value = "http://localhost:50051"
    )]
    dispatcher_endpoint: String,

    /// Platforms this builder supports (comma-separated, auto-detected if not provided)
    #[arg(long, env = "RIO_PLATFORMS", value_delimiter = ',')]
    platforms: Vec<String>,

    /// Features this builder supports (e.g., kvm, benchmark)
    #[arg(long, env = "RIO_FEATURES", value_delimiter = ',')]
    features: Vec<String>,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long, env = "RIO_LOG_LEVEL", default_value = "info")]
    log_level: String,
}

fn detect_platform() -> Platform {
    if cfg!(target_os = "linux") && cfg!(target_arch = "x86_64") {
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
    }
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

    info!("Starting Rio Builder v{}", env!("CARGO_PKG_VERSION"));
    info!("Dispatcher endpoint: {}", cli.dispatcher_endpoint);

    // Determine platforms to advertise
    let platforms: Vec<Platform> = if cli.platforms.is_empty() {
        // Auto-detect platform
        let detected = detect_platform();
        info!("Auto-detected platform: {}", detected);
        vec![detected]
    } else {
        // Use provided platforms
        let parsed: Vec<Platform> = cli
            .platforms
            .iter()
            .map(|s| s.parse().expect("Invalid platform string"))
            .collect();
        info!("Using configured platforms: {:?}", parsed);
        parsed
    };

    if !cli.features.is_empty() {
        info!("Builder features: {:?}", cli.features);
    }

    // Create builder
    let mut builder = Builder::new(cli.dispatcher_endpoint, platforms, cli.features);

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
