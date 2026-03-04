use clap::Parser;
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;
use tonic::transport::Server;
use tracing::{error, info};

use rio_proto::store::chunk_service_server::ChunkServiceServer;
use rio_proto::store::store_service_server::StoreServiceServer;
use rio_store::grpc::{ChunkServiceImpl, StoreServiceImpl};

// Two-struct config split — see rio-common/src/config.rs for rationale.

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    listen_addr: String,
    backend: String,
    base_dir: String,
    /// S3 bucket (required if `backend = "s3"`, ignored for filesystem).
    /// `Option` so "missing + filesystem backend" isn't an error.
    s3_bucket: Option<String>,
    s3_prefix: String,
    s3_max_retries: u32,
    s3_attempt_timeout_secs: u64,
    database_url: String,
    metrics_addr: std::net::SocketAddr,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:9002".into(),
            backend: "filesystem".into(),
            base_dir: "/var/rio/store".into(),
            s3_bucket: None,
            s3_prefix: "nars".into(),
            s3_max_retries: 5,
            s3_attempt_timeout_secs: 30,
            database_url: String::new(),
            metrics_addr: "0.0.0.0:9092".parse().unwrap(),
        }
    }
}

#[derive(Parser, Serialize, Default)]
#[command(
    name = "rio-store",
    about = "NAR content-addressable store for rio-build"
)]
struct CliArgs {
    /// gRPC listen address
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    listen_addr: Option<String>,

    /// Storage backend (filesystem or s3)
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    backend: Option<String>,

    /// Base directory for filesystem backend
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    base_dir: Option<String>,

    /// S3 bucket name (for S3 backend)
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    s3_bucket: Option<String>,

    /// S3 key prefix (for S3 backend)
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    s3_prefix: Option<String>,

    /// Maximum S3 operation retry attempts (AWS SDK default: 3)
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    s3_max_retries: Option<u32>,

    /// Per-attempt timeout for S3 operations in seconds
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    s3_attempt_timeout_secs: Option<u64>,

    /// PostgreSQL connection URL
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    database_url: Option<String>,

    /// Prometheus metrics listen address
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    metrics_addr: Option<std::net::SocketAddr>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = CliArgs::parse();
    let cfg: Config = rio_common::config::load("store", cli)?;
    let _otel_guard = rio_common::observability::init_tracing("store")?;

    anyhow::ensure!(
        !cfg.database_url.is_empty(),
        "database_url is required (set --database-url, RIO_DATABASE_URL, or store.toml)"
    );

    let _root_guard = tracing::info_span!("store", component = "store").entered();
    info!(version = env!("CARGO_PKG_VERSION"), "starting rio-store");

    rio_common::observability::init_metrics(cfg.metrics_addr)?;

    // Connect to PostgreSQL
    info!(url = %cfg.database_url, "connecting to PostgreSQL");
    let pool = PgPoolOptions::new()
        .max_connections(20)
        .connect(&cfg.database_url)
        .await?;
    info!("PostgreSQL connection established");

    sqlx::migrate!("../migrations")
        .run(&pool)
        .await
        .inspect_err(|e| error!(error = %e, "database migrations failed"))?;
    info!("database migrations applied");

    // NarBackend is gone (E1). Inline NARs live in manifests.inline_blob.
    // ChunkBackend (C2: S3/fs/memory impls) + ChunkCache (C4: moka) +
    // put_chunked (C3) + cache_server (B3) are library-complete and
    // tested but not yet wired here — constructing them from cfg.backend
    // + cfg.s3_bucket etc. is TODO(phase3a). The config fields stay so
    // that diff is purely additive.
    //
    // Until then: inline-only storage, ChunkService RPCs return
    // FAILED_PRECONDITION, cache_server not spawned. All correct for
    // a store without chunk dedup enabled.
    info!(
        backend = %cfg.backend,
        base_dir = %cfg.base_dir,
        s3_bucket = ?cfg.s3_bucket,
        "backend config (inline-only until phase3a wires ChunkBackend here)"
    );
    let _ = (
        cfg.backend,
        cfg.base_dir,
        cfg.s3_bucket,
        cfg.s3_prefix,
        cfg.s3_max_retries,
        cfg.s3_attempt_timeout_secs,
    );

    // Build gRPC services.
    //
    // TODO(phase3a): construct ChunkBackend from cfg.backend +
    // cfg.s3_bucket etc., wrap in ChunkCache, pass the SAME
    // Arc<ChunkCache> to both StoreServiceImpl (via with_chunk_backend)
    // and ChunkServiceImpl. One cache, shared — a chunk warmed by
    // GetPath is hot for GetChunk. Also spawn cache_server if
    // cache_http_addr is configured (needs ChunkCache for reassembly).
    // With cache=None below, ChunkService returns FAILED_PRECONDITION,
    // which is the right answer for an inline-only store.
    let store_service = StoreServiceImpl::new(pool.clone());
    let chunk_service = ChunkServiceImpl::new(pool, None);
    let max_msg_size = rio_proto::max_message_size();

    let addr = cfg.listen_addr.parse()?;
    info!(addr = %addr, max_msg_size, "starting gRPC server");

    Server::builder()
        .add_service(StoreServiceServer::new(store_service).max_decoding_message_size(max_msg_size))
        .add_service(ChunkServiceServer::new(chunk_service))
        .serve(addr)
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_match_phase2a() {
        let d = Config::default();
        assert_eq!(d.listen_addr, "0.0.0.0:9002");
        assert_eq!(d.backend, "filesystem");
        assert_eq!(d.base_dir, "/var/rio/store");
        assert_eq!(d.s3_bucket, None);
        assert_eq!(d.s3_prefix, "nars");
        assert_eq!(d.s3_max_retries, 5);
        assert_eq!(d.s3_attempt_timeout_secs, 30);
        assert_eq!(d.metrics_addr.to_string(), "0.0.0.0:9092");
        assert!(d.database_url.is_empty());
    }

    #[test]
    fn cli_args_parse_help() {
        use clap::CommandFactory;
        CliArgs::command().debug_assert();
    }
}
