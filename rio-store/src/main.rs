use clap::Parser;
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;
use tonic::transport::Server;
use tracing::{error, info};

use rio_proto::ChunkServiceServer;
use rio_proto::StoreServiceServer;
use rio_store::grpc::{ChunkServiceImpl, StoreServiceImpl};

// Two-struct config split — see rio-common/src/config.rs for rationale.

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    listen_addr: String,
    database_url: String,
    metrics_addr: std::net::SocketAddr,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:9002".into(),
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
    rio_store::describe_metrics();

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

    // grpc.health.v1.Health. Starts NOT_SERVING (the `set_serving::<>` call
    // below flips it). K8s readiness probe hits this — NOT_SERVING until
    // migrations complete means the Service doesn't route to a half-booted
    // pod. The "" (empty-string) service name is the conventional "whole
    // server" check; we don't bother with per-service granularity since
    // every RPC here needs PG, which is the one thing we're checking.
    //
    // Ordering: health_reporter() → build services → set_serving() →
    // serve(). The set_serving happens BEFORE serve() blocks, which means
    // the very first health check after listen returns SERVING. That's
    // correct: by the time we're listening, migrations are done. If
    // migrations failed, the `?` above already bailed.
    let (health_reporter, health_service) = tonic_health::server::health_reporter();

    // Build gRPC services.
    //
    // TODO(phase3a): construct ChunkBackend from config, wrap in ChunkCache,
    // pass the SAME Arc<ChunkCache> to both StoreServiceImpl (via
    // with_chunk_backend) and ChunkServiceImpl. One cache, shared — a chunk
    // warmed by GetPath is hot for GetChunk. Also spawn cache_server if
    // cache_http_addr is configured (needs ChunkCache for reassembly).
    // With cache=None below, ChunkService returns FAILED_PRECONDITION,
    // which is the right answer for an inline-only store.
    let store_service = StoreServiceImpl::new(pool.clone());
    let chunk_service = ChunkServiceImpl::new(pool, None);
    let max_msg_size = rio_proto::max_message_size();

    let addr = cfg.listen_addr.parse()?;
    info!(addr = %addr, max_msg_size, "starting gRPC server");

    // PG is connected, migrations applied, services constructed.
    // Everything that can fail-fast has. SERVING.
    //
    // The type param is the service struct, not the generated Server
    // wrapper. tonic-health uses it for the per-service name (clients
    // can check "rio.store.v1.StoreService" specifically). We only
    // register one — the empty-string "whole server" check falls through
    // to this when no specific service is named.
    health_reporter
        .set_serving::<StoreServiceServer<StoreServiceImpl>>()
        .await;

    Server::builder()
        .add_service(health_service)
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
    fn config_defaults_are_stable() {
        let d = Config::default();
        assert_eq!(d.listen_addr, "0.0.0.0:9002");
        assert_eq!(d.metrics_addr.to_string(), "0.0.0.0:9092");
        assert!(d.database_url.is_empty());
    }

    #[test]
    fn cli_args_parse_help() {
        use clap::CommandFactory;
        CliArgs::command().debug_assert();
    }
}
