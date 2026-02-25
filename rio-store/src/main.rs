use clap::Parser;
use tracing::info;

#[derive(Parser, Debug)]
#[command(
    name = "rio-store",
    about = "NAR content-addressable store for rio-build"
)]
struct Args {
    /// gRPC listen address
    #[arg(long, env = "RIO_STORE_LISTEN_ADDR", default_value = "0.0.0.0:9002")]
    listen_addr: String,

    /// Storage backend (filesystem or s3)
    #[arg(long, env = "RIO_STORE_BACKEND", default_value = "filesystem")]
    backend: String,

    /// Base directory for filesystem backend
    #[arg(long, env = "RIO_STORE_BASE_DIR", default_value = "/var/rio/store")]
    base_dir: String,

    /// S3 bucket name (for S3 backend)
    #[arg(long, env = "RIO_S3_BUCKET")]
    s3_bucket: Option<String>,

    /// S3 key prefix (for S3 backend)
    #[arg(long, env = "RIO_S3_PREFIX", default_value = "nars")]
    s3_prefix: String,

    /// PostgreSQL connection URL
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    /// Prometheus metrics listen address
    #[arg(long, env = "RIO_METRICS_ADDR", default_value = "0.0.0.0:9092")]
    metrics_addr: std::net::SocketAddr,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let log_format = rio_common::observability::log_format_from_env();
    rio_common::observability::init_logging(log_format, None)?;

    let _root_guard = tracing::info_span!("store", component = "store").entered();
    info!(version = env!("CARGO_PKG_VERSION"), "starting rio-store");

    rio_common::observability::init_metrics(args.metrics_addr)?;

    info!("rio-store is a placeholder — implementation in Step 4");
    // TODO: Step 4 implementation
    // - Connect to PostgreSQL
    // - Initialize backend (filesystem or S3)
    // - Start gRPC server

    Ok(())
}
