use std::sync::Arc;

use clap::Parser;
use sqlx::postgres::PgPoolOptions;
use tonic::transport::Server;
use tracing::{error, info};

use rio_proto::store::chunk_service_server::ChunkServiceServer;
use rio_proto::store::store_service_server::StoreServiceServer;
use rio_store::backend::NarBackend;
use rio_store::backend::filesystem::FilesystemBackend;
use rio_store::backend::s3::S3Backend;
use rio_store::grpc::{ChunkServiceStub, StoreServiceImpl};

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

    // Connect to PostgreSQL
    info!(url = %args.database_url, "connecting to PostgreSQL");
    let pool = PgPoolOptions::new()
        .max_connections(20)
        .connect(&args.database_url)
        .await?;
    info!("PostgreSQL connection established");

    // Verify database connectivity (migrations are applied externally via
    // `sqlx migrate run` or during deployment — the `sqlx::migrate!()` macro
    // requires the `migrate` feature which conflicts with rusqlite's bundled
    // sqlite in the workspace).
    sqlx::query("SELECT 1")
        .execute(&pool)
        .await
        .inspect_err(|e| error!(error = %e, "database connectivity check failed"))?;
    info!("database connectivity verified (ensure migrations are applied)");

    // Initialize backend
    let backend: Arc<dyn NarBackend> = match args.backend.as_str() {
        "filesystem" => {
            info!(base_dir = %args.base_dir, "using filesystem backend");
            Arc::new(FilesystemBackend::new(&args.base_dir))
        }
        "s3" => {
            let bucket = args
                .s3_bucket
                .ok_or_else(|| anyhow::anyhow!("--s3-bucket is required for S3 backend"))?;
            info!(bucket = %bucket, prefix = %args.s3_prefix, "using S3 backend");
            let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            let s3_client = aws_sdk_s3::Client::new(&aws_config);
            Arc::new(S3Backend::new(s3_client, bucket, args.s3_prefix))
        }
        other => {
            anyhow::bail!("unknown backend: {other} (expected 'filesystem' or 's3')");
        }
    };

    // Build gRPC services
    let store_service = StoreServiceImpl::new(backend, pool);
    let max_msg_size = rio_proto::max_message_size();

    let addr = args.listen_addr.parse()?;
    info!(addr = %addr, max_msg_size, "starting gRPC server");

    Server::builder()
        .add_service(StoreServiceServer::new(store_service).max_decoding_message_size(max_msg_size))
        .add_service(ChunkServiceServer::new(ChunkServiceStub))
        .serve(addr)
        .await?;

    Ok(())
}
