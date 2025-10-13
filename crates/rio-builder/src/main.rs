use anyhow::Result;
use clap::Parser;
use rio_common::Platform;
use tracing::{Level, error, info};

mod builder;
mod executor;
mod grpc_server;

use builder::Builder;
use executor::Executor;
use grpc_server::start_grpc_server;

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

    /// gRPC server address for receiving build requests
    #[arg(long, env = "RIO_GRPC_ADDR", default_value = "0.0.0.0:50052")]
    grpc_addr: String,

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
    info!("gRPC server address: {}", cli.grpc_addr);

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

    // Connect to dispatcher with retries
    let max_retries = 10;
    let mut retry_count = 0;
    let mut delay = std::time::Duration::from_secs(1);

    loop {
        match builder.connect().await {
            Ok(()) => break,
            Err(e) => {
                retry_count += 1;
                if retry_count >= max_retries {
                    error!(
                        "Failed to connect to dispatcher after {} attempts: {}",
                        retry_count, e
                    );
                    return Err(e);
                }
                info!(
                    "Connection attempt {} failed, retrying in {:?}...",
                    retry_count, delay
                );
                tokio::time::sleep(delay).await;
                delay = std::cmp::min(delay * 2, std::time::Duration::from_secs(30));
            }
        }
    }

    info!("Connected to dispatcher successfully");

    // Register with dispatcher
    if let Err(e) = builder.register().await {
        error!("Failed to register with dispatcher: {}", e);
        return Err(e);
    }

    info!("Rio Builder {} registered with dispatcher", builder.id());

    // Parse gRPC address
    let grpc_addr: std::net::SocketAddr =
        cli.grpc_addr.parse().expect("Invalid gRPC address format");

    // Create executor for handling build requests
    let executor = Executor::new();

    // Start gRPC server in a separate task
    let grpc_server_handle = tokio::spawn(async move {
        if let Err(e) = start_grpc_server(grpc_addr, executor).await {
            error!("gRPC server error: {}", e);
        }
    });

    // Start heartbeat loop in a separate task
    let heartbeat_handle = tokio::spawn(async move {
        if let Err(e) = builder.start_heartbeat_loop().await {
            error!("Heartbeat loop error: {}", e);
        }
    });

    info!(
        "Rio Builder started successfully (gRPC server running on {})",
        grpc_addr
    );

    // Wait for either task to complete or Ctrl+C
    tokio::select! {
        _ = grpc_server_handle => {
            error!("gRPC server terminated unexpectedly");
        }
        _ = heartbeat_handle => {
            error!("Heartbeat loop terminated unexpectedly");
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Shutting down...");
        }
    }

    Ok(())
}
