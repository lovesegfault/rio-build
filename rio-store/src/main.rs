use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;
use tonic::transport::Server;
use tracing::{error, info};

use rio_proto::ChunkServiceServer;
use rio_proto::StoreServiceServer;
use rio_store::backend::chunk::{ChunkBackend, FilesystemChunkBackend, S3ChunkBackend};
use rio_store::cache_server::{self, CacheServerState};
use rio_store::cas::ChunkCache;
use rio_store::grpc::{ChunkServiceImpl, StoreServiceImpl};
use rio_store::signing::Signer;

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
    /// Plaintext gRPC health listen address. K8s gRPC probes can't
    /// do mTLS, so when server TLS is enabled, the main port's
    /// health check is unreachable to K8s. This spawns a second
    /// tonic server with ONLY `grpc.health.v1.Health`, plaintext,
    /// sharing the SAME HealthReporter so status toggles propagate.
    /// Only listens if server TLS is configured — plaintext main
    /// port already serves health, no second listener needed.
    health_addr: std::net::SocketAddr,
    /// mTLS for both server (incoming gRPC) and client (none
    /// currently — store doesn't dial out via rio-proto, only
    /// S3 which has its own auth). Set via `RIO_TLS__*`.
    tls: rio_common::tls::TlsConfig,
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
            // 9102 = gRPC (9002) + 100. Same +100 pattern as
            // gateway (9090→9190). Only used when TLS is on.
            health_addr: "0.0.0.0:9102".parse().unwrap(),
            tls: rio_common::tls::TlsConfig::default(),
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
    // rustls CryptoProvider install. Phase 3b enables tonic
    // tls-aws-lc; without this, first TLS handshake panics.
    // Store is a gRPC SERVER (incoming TLS) — the S3 client has
    // its own TLS stack (aws-sdk's rustls) but it's the same
    // aws-lc-rs feature, so one install covers both.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

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

    // Construct the chunk backend + ONE shared ChunkCache. The cache
    // Arc is cloned into all three consumers (StoreServiceImpl,
    // ChunkServiceImpl, CacheServerState) — a chunk warmed by GetPath
    // is hot for GetChunk and for the binary-cache /nar/ endpoint.
    //
    // `?` on backend construction: filesystem mkdir fail or S3
    // bad-region means we can't store chunks — startup error, not
    // degraded mode. Inline backend can't fail (just None).
    let chunk_cache: Option<Arc<ChunkCache>> = match &cfg.chunk_backend {
        ChunkBackendKind::Inline => {
            info!("chunk backend: inline (all NARs in PG manifests.inline_blob)");
            None
        }
        ChunkBackendKind::Filesystem { base_dir } => {
            info!(base_dir = %base_dir.display(), "chunk backend: filesystem");
            // Eagerly creates the 256-subdir fanout. `?` — if the
            // disk is read-only or the path is garbage, better to
            // fail here than on the first PutPath with a cryptic
            // ENOENT deep in the put() call.
            let backend: Arc<dyn ChunkBackend> = Arc::new(FilesystemChunkBackend::new(base_dir)?);
            Some(Arc::new(ChunkCache::with_capacity(
                backend,
                cfg.chunk_cache_capacity_bytes,
            )))
        }
        ChunkBackendKind::S3 { bucket, prefix } => {
            info!(%bucket, %prefix, "chunk backend: S3");
            // Credentials from the aws-sdk default chain (env vars,
            // IMDS, etc). NOT in our config — we don't want secrets
            // in TOML. If credentials are missing, the first PutPath
            // will fail with a clear AWS error; we don't eagerly
            // verify here (would need a HeadBucket or similar, and
            // credentials might not be available YET if IMDS is
            // slow — better to start serving and fail the first
            // chunk op than to race IMDS).
            let aws_cfg = aws_config::load_from_env().await;
            let client = aws_sdk_s3::Client::new(&aws_cfg);
            let backend: Arc<dyn ChunkBackend> =
                Arc::new(S3ChunkBackend::new(client, bucket.clone(), prefix.clone()));
            Some(Arc::new(ChunkCache::with_capacity(
                backend,
                cfg.chunk_cache_capacity_bytes,
            )))
        }
    };

    // Load the narinfo signing key. `None` path → `None` signer (not
    // an error — signing is optional). Bad path / bad format → `?`
    // (operator configured a key; failing silently = unsigned paths
    // = security surprise).
    let signer = Signer::load(cfg.signing_key_path.as_deref())
        .map_err(|e| anyhow::anyhow!("signing key load failed: {e}"))?;
    if signer.is_some() {
        info!(path = ?cfg.signing_key_path, "narinfo signing enabled");
    }

    // StoreServiceImpl: inline-only vs chunked based on cache. The
    // signer chains after either constructor (builder-style).
    let store_service = match &chunk_cache {
        None => StoreServiceImpl::new(pool.clone()),
        Some(cache) => StoreServiceImpl::with_chunk_cache(pool.clone(), Arc::clone(cache)),
    };
    let store_service = match signer {
        Some(s) => store_service.with_signer(s),
        None => store_service,
    };

    // ChunkServiceImpl: same cache Arc. None → FAILED_PRECONDITION
    // on GetChunk, which is correct for an inline-only store (there
    // ARE no chunks to get).
    let chunk_service = ChunkServiceImpl::new(pool.clone(), chunk_cache.clone());

    // Binary-cache HTTP server (narinfo + nar.zst routes). Spawned
    // concurrently with the gRPC server (which blocks on serve()
    // below). Only if configured — this is optional; the gRPC
    // store works without it.
    if let Some(http_addr) = cfg.cache_http_addr {
        let state = Arc::new(CacheServerState {
            pool: pool.clone(),
            // Same Arc again. /nar/ reassembly hits the same moka
            // LRU as GetPath.
            chunk_cache: chunk_cache.clone(),
        });
        let router = cache_server::router(state);
        rio_common::task::spawn_monitored("cache-http-server", async move {
            info!(addr = %http_addr, "starting binary-cache HTTP server");
            match tokio::net::TcpListener::bind(http_addr).await {
                Ok(listener) => {
                    if let Err(e) = axum::serve(listener, router).await {
                        error!(error = %e, "cache HTTP server failed");
                    }
                }
                Err(e) => {
                    error!(error = %e, addr = %http_addr, "cache HTTP bind failed");
                }
            }
        });
    }

    let max_msg_size = rio_proto::max_message_size();

    let addr = cfg.listen_addr.parse()?;

    // PG is connected, migrations applied, services constructed.
    // Everything that can fail-fast has. SERVING.
    //
    // The type param is the service struct, not the generated Server
    // wrapper. tonic-health uses it for the per-service name (clients
    // can check "rio.store.StoreService" specifically). We only
    // register one — the empty-string "whole server" check falls through
    // to this when no specific service is named.
    health_reporter
        .set_serving::<StoreServiceServer<StoreServiceImpl>>()
        .await;

    // Server TLS + plaintext health split. Same pattern as scheduler:
    // K8s gRPC probes can't do mTLS, so when TLS is on, spawn a
    // second plaintext listener with ONLY health, sharing the SAME
    // HealthReporter so set_serving above propagates. Store has no
    // leader-toggle (always SERVING once booted) so the shared-
    // reporter requirement is less sharp than scheduler's, but it's
    // the same architectural pattern — no reason to diverge.
    let server_tls = rio_common::tls::load_server_tls(&cfg.tls)
        .map_err(|e| anyhow::anyhow!("server TLS config: {e}"))?;
    let health_service_plain = health_service.clone();
    if server_tls.is_some() {
        let health_addr = cfg.health_addr;
        info!(addr = %health_addr, "spawning plaintext health server for K8s probes (mTLS on main port)");
        rio_common::task::spawn_monitored("health-plaintext", async move {
            if let Err(e) = Server::builder()
                .add_service(health_service_plain)
                .serve(health_addr)
                .await
            {
                tracing::error!(error = %e, "plaintext health server failed");
            }
        });
        info!("server mTLS enabled — clients must present CA-signed certs");
    }

    info!(addr = %addr, max_msg_size, tls = server_tls.is_some(), "starting gRPC server");

    let mut builder = Server::builder();
    if let Some(tls) = server_tls {
        builder = builder.tls_config(tls)?;
    }
    builder
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
        // Phase3b: plaintext health for K8s probes when mTLS on.
        assert_eq!(d.health_addr.to_string(), "0.0.0.0:9102");
        assert!(!d.tls.is_configured());
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
