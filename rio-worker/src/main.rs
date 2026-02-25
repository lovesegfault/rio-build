//! rio-worker binary entry point.

use clap::Parser;
use tracing::info;

#[derive(Parser, Debug)]
#[command(
    name = "rio-worker",
    about = "Build executor with FUSE store for rio-build"
)]
struct Args {
    /// Worker ID (defaults to hostname)
    #[arg(long, env = "RIO_WORKER_ID")]
    worker_id: Option<String>,

    /// rio-scheduler gRPC address
    #[arg(long, env = "RIO_SCHEDULER_ADDR")]
    scheduler_addr: String,

    /// rio-store gRPC address
    #[arg(long, env = "RIO_STORE_ADDR")]
    store_addr: String,

    /// Maximum concurrent builds
    #[arg(long, env = "RIO_WORKER_MAX_BUILDS", default_value = "1")]
    max_builds: u32,

    /// System architecture (auto-detected if not set)
    #[arg(long, env = "RIO_WORKER_SYSTEM")]
    system: Option<String>,

    /// FUSE cache directory
    #[arg(long, env = "RIO_FUSE_CACHE_DIR", default_value = "/var/rio/cache")]
    fuse_cache_dir: std::path::PathBuf,

    /// FUSE cache size limit in GB
    #[arg(long, env = "RIO_FUSE_CACHE_SIZE_GB", default_value = "50")]
    fuse_cache_size_gb: u64,

    /// Overlay base directory
    #[arg(
        long,
        env = "RIO_OVERLAY_BASE_DIR",
        default_value = "/var/rio/overlays"
    )]
    overlay_base_dir: std::path::PathBuf,

    /// Prometheus metrics listen address
    #[arg(long, env = "RIO_METRICS_ADDR", default_value = "0.0.0.0:9093")]
    metrics_addr: std::net::SocketAddr,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let log_format = rio_common::observability::log_format_from_env();
    rio_common::observability::init_logging(log_format, None)?;

    let worker_id = args.worker_id.unwrap_or_else(|| {
        nix::unistd::gethostname()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_else(|| "unknown".to_string())
    });

    let _root_guard =
        tracing::info_span!("worker", component = "worker", worker_id = %worker_id).entered();
    info!(version = env!("CARGO_PKG_VERSION"), "starting rio-worker");

    rio_common::observability::init_metrics(args.metrics_addr)?;

    info!(
        %worker_id,
        scheduler_addr = %args.scheduler_addr,
        store_addr = %args.store_addr,
        max_builds = args.max_builds,
        "rio-worker is a placeholder — implementation in Step 7"
    );

    Ok(())
}
