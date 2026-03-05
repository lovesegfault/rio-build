use std::path::PathBuf;

use clap::Parser;
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;
use tonic::transport::Server;
use tracing::{error, info};

use rio_proto::ChunkServiceServer;
use rio_proto::StoreServiceServer;
use rio_store::grpc::{ChunkServiceImpl, StoreServiceImpl};

// Two-struct config split — see rio-common/src/config.rs for rationale.

/// Chunk storage backend selection.
///
/// Serde internally-tagged (`kind`): TOML writes
/// `[chunk_backend]\nkind = "s3"\nbucket = "..."`. The tag field is
/// `kind` not the serde default `type` — `type` is a Rust keyword
/// and would need `r#type` everywhere we match on it.
///
/// Default is `Inline`: backward-compatible with existing deployments
/// that have no chunk-backend config. All NARs go into PG
/// `manifests.inline_blob` regardless of size. Fine for dev/CI;
/// production wants `s3` or `filesystem`.
#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum ChunkBackendKind {
    /// No chunk backend. All NARs inline in PG. ChunkService returns
    /// FAILED_PRECONDITION. cache_server can only serve inline paths.
    #[default]
    Inline,
    /// Local filesystem. 256-subdir fanout by hash prefix (same layout
    /// as git objects). `base_dir` is created at startup.
    Filesystem { base_dir: PathBuf },
    /// S3-compatible. Credentials come from the aws-sdk's default chain
    /// (env vars, instance profile, etc) — NOT in this config. We're
    /// not putting secrets in a TOML file.
    S3 { bucket: String, prefix: String },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    listen_addr: String,
    database_url: String,
    metrics_addr: std::net::SocketAddr,
    /// Where chunks live. Default: inline (no backend). See
    /// [`ChunkBackendKind`] for TOML syntax.
    chunk_backend: ChunkBackendKind,
    /// moka LRU capacity for chunk reads, in bytes. Default 2 GiB.
    /// One cache shared by StoreService + ChunkService + cache_server
    /// — a chunk warmed by any is hot for all. Only relevant when
    /// chunk_backend != inline.
    chunk_cache_capacity_bytes: u64,
    /// ed25519 narinfo signing key path (Nix secret-key format:
    /// `name:base64-seed`). None = signing disabled (paths stored
    /// without our signature; still serveable, just unverified). The
    /// key file should be mode 0600 and NOT in git.
    signing_key_path: Option<PathBuf>,
    /// Binary-cache HTTP listen address (narinfo + nar.zst routes).
    /// None = don't spawn. Separate from listen_addr (that's gRPC);
    /// Nix clients hit this with plain HTTP GETs.
    cache_http_addr: Option<std::net::SocketAddr>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:9002".into(),
            database_url: String::new(),
            metrics_addr: "0.0.0.0:9092".parse().unwrap(),
            chunk_backend: ChunkBackendKind::default(),
            // 2 GiB. Matches ChunkCache::DEFAULT_CACHE_CAPACITY_BYTES
            // — the constant is crate-private so duplicated here,
            // but the config_defaults_are_stable test catches drift.
            chunk_cache_capacity_bytes: 2 * 1024 * 1024 * 1024,
            signing_key_path: None,
            cache_http_addr: None,
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
        // Phase3a: chunk backend off by default (backward-compat).
        assert!(matches!(d.chunk_backend, ChunkBackendKind::Inline));
        // Matches ChunkCache::DEFAULT_CACHE_CAPACITY_BYTES. If that
        // constant changes, update this — the test catches drift.
        assert_eq!(d.chunk_cache_capacity_bytes, 2 * 1024 * 1024 * 1024);
        assert!(d.signing_key_path.is_none());
        assert!(d.cache_http_addr.is_none());
    }

    /// TOML parsing for the tagged enum via figment (what main.rs
    /// actually uses via rio_common::config::load). The `kind` tag +
    /// lowercase variant names are load-bearing — the NixOS module
    /// writes TOML with these exact strings. A silent rename would
    /// break every deployment with chunk_backend configured.
    ///
    /// Testing via figment (not raw toml crate) catches figment-
    /// specific deserialization quirks. figment's tagged-enum
    /// handling is a known past pain point.
    fn parse_toml(s: &str) -> Config {
        use figment::Figment;
        use figment::providers::{Format, Toml};
        Figment::from(Toml::string(s)).extract().unwrap()
    }

    #[test]
    fn chunk_backend_kind_toml_inline() {
        let cfg = parse_toml(
            r#"
            [chunk_backend]
            kind = "inline"
            "#,
        );
        assert!(matches!(cfg.chunk_backend, ChunkBackendKind::Inline));
    }

    #[test]
    fn chunk_backend_kind_toml_filesystem() {
        let cfg = parse_toml(
            r#"
            [chunk_backend]
            kind = "filesystem"
            base_dir = "/var/lib/rio-store/chunks"
            "#,
        );
        match cfg.chunk_backend {
            ChunkBackendKind::Filesystem { base_dir } => {
                assert_eq!(base_dir, PathBuf::from("/var/lib/rio-store/chunks"));
            }
            other => panic!("expected Filesystem, got {other:?}"),
        }
    }

    #[test]
    fn chunk_backend_kind_toml_s3() {
        let cfg = parse_toml(
            r#"
            [chunk_backend]
            kind = "s3"
            bucket = "my-nar-chunks"
            prefix = "prod/"
            "#,
        );
        match cfg.chunk_backend {
            ChunkBackendKind::S3 { bucket, prefix } => {
                assert_eq!(bucket, "my-nar-chunks");
                assert_eq!(prefix, "prod/");
            }
            other => panic!("expected S3, got {other:?}"),
        }
    }

    /// No [chunk_backend] section at all → default (Inline). This is
    /// the backward-compat path: pre-phase3a configs have no such
    /// section and should keep working.
    #[test]
    fn chunk_backend_kind_absent_defaults_inline() {
        let cfg = parse_toml(
            r#"
            listen_addr = "0.0.0.0:9002"
            "#,
        );
        assert!(matches!(cfg.chunk_backend, ChunkBackendKind::Inline));
    }

    #[test]
    fn cli_args_parse_help() {
        use clap::CommandFactory;
        CliArgs::command().debug_assert();
    }
}
