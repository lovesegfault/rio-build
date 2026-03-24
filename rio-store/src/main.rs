use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;
use tonic::transport::Server;
use tracing::{error, info};

use rio_proto::ChunkServiceServer;
use rio_proto::StoreAdminServiceServer;
use rio_proto::StoreServiceServer;
use rio_store::backend::chunk::{ChunkBackend, FilesystemChunkBackend, S3ChunkBackend};
use rio_store::cache_server::{self, CacheServerState};
use rio_store::cas::ChunkCache;
use rio_store::grpc::{ChunkServiceImpl, StoreAdminServiceImpl, StoreServiceImpl};
use rio_store::signing::{Signer, TenantSigner};

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
    /// Global NAR reassembly buffer budget in bytes — total permits
    /// across ALL concurrent PutPath handlers. Each handler acquires
    /// `chunk.len()` permits before extending its accumulation Vec.
    /// None → DEFAULT_NAR_BUDGET (8 × MAX_NAR_SIZE = 32 GiB). Lower
    /// this on small-memory nodes; raise it if you have >8 concurrent
    /// max-size uploads and RAM to match.
    nar_buffer_budget_bytes: Option<u64>,
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
    /// HMAC key file for verifying assignment tokens on PutPath.
    /// SAME file as scheduler's `hmac_key_path`. Unset = accept
    /// all PutPath callers (dev mode).
    hmac_key_path: Option<PathBuf>,
    /// JWT verification. `key_path` → ConfigMap mount at
    /// `/etc/rio/jwt/ed25519_pubkey` (same mount as scheduler — one
    /// gateway signing key → one pubkey across all verifier services).
    /// Unset = interceptor inert (dev mode). SIGHUP reloads from the
    /// same path. Set via `RIO_JWT__KEY_PATH`.
    jwt: rio_common::config::JwtConfig,
    /// Client-cert CNs/SAN-DNSNames that bypass HMAC verification
    /// on PutPath. The gateway handles `nix copy --to` and has no
    /// assignment token; its client cert identity is allowlisted
    /// instead. Matching checks CN first, then each SAN DNSName
    /// entry — either match grants bypass. SAN matching enables
    /// cert-manager-issued certs that put identity in SAN
    /// extensions and leave CN empty (modern best practice).
    /// Default: `["rio-gateway"]`.
    hmac_bypass_cns: Vec<String>,
    /// Allow binary cache HTTP requests without `Authorization:
    /// Bearer` header. Default `false` — Bearer token required
    /// (mapped to tenant via `tenants.cache_token` column). Set
    /// `true` explicitly for single-tenant/dev deployments.
    cache_allow_unauthenticated: bool,
    /// Seconds to wait after SIGTERM between set_not_serving() and
    /// exit. Gives kubelet readinessProbe (periodSeconds: 5) time to
    /// observe NOT_SERVING + endpoint-controller to propagate. 0 =
    /// no drain. Default 6 (= 5 + 1).
    drain_grace_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:9002".into(),
            database_url: String::new(),
            metrics_addr: rio_common::default_addr(9092),
            chunk_backend: ChunkBackendKind::default(),
            // 2 GiB. Matches ChunkCache::DEFAULT_CACHE_CAPACITY_BYTES
            // — the constant is crate-private so duplicated here,
            // but the config_defaults_are_stable test catches drift.
            chunk_cache_capacity_bytes: 2 * 1024 * 1024 * 1024,
            nar_buffer_budget_bytes: None,
            signing_key_path: None,
            cache_http_addr: None,
            // 9102 = gRPC (9002) + 100. Same +100 pattern as
            // gateway (9090→9190). Only used when TLS is on.
            health_addr: rio_common::default_addr(9102),
            tls: rio_common::tls::TlsConfig::default(),
            hmac_key_path: None,
            jwt: rio_common::config::JwtConfig::default(),
            hmac_bypass_cns: vec!["rio-gateway".into()],
            cache_allow_unauthenticated: false,
            drain_grace_secs: 6,
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

    /// Drain grace period in seconds (0 = disabled)
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    drain_grace_secs: Option<u64>,
}

impl rio_common::config::ValidateConfig for Config {
    /// Only one check today (database_url) but creates the hook for
    /// gc.*, chunk_backend.*, signing.* bounds as they become
    /// operator-settable.
    fn validate(&self) -> anyhow::Result<()> {
        use rio_common::config::ensure_required as required;
        required(&self.database_url, "database_url", "store")?;
        Ok(())
    }
}

impl rio_common::server::HasCommonConfig for Config {
    fn tls(&self) -> &rio_common::tls::TlsConfig {
        &self.tls
    }
    fn metrics_addr(&self) -> std::net::SocketAddr {
        self.metrics_addr
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = CliArgs::parse();
    // Store doesn't dial out via rio-proto (S3 has its own auth), so
    // init_client_tls is a harmless no-op — the OnceLock gets set but
    // never read. Passing it keeps all 5 bootstrap() calls uniform.
    let rio_common::server::Bootstrap::<Config> {
        cfg,
        shutdown,
        otel_guard: _otel_guard,
    } = rio_common::server::bootstrap(
        "store",
        cli,
        rio_proto::client::init_client_tls,
        rio_store::describe_metrics,
    )?;

    let _root_guard = tracing::info_span!("store", component = "store").entered();
    info!(version = env!("CARGO_PKG_VERSION"), "starting rio-store");

    let pool = init_db_pool(&cfg.database_url).await?;

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

    // Two-stage shutdown — see rio_common::server::spawn_drain_task
    // for the INDEPENDENT-token rationale + proof test. Closure flips
    // the NAMED StoreService (BalancedChannel probe target).
    let serve_shutdown = rio_common::signal::Token::new();
    {
        let reporter = health_reporter.clone();
        rio_common::server::spawn_drain_task(
            shutdown.clone(),
            serve_shutdown.clone(),
            std::time::Duration::from_secs(cfg.drain_grace_secs),
            move || async move {
                reporter
                    .set_not_serving::<StoreServiceServer<StoreServiceImpl>>()
                    .await;
            },
        );
    }

    let chunk_cache =
        init_chunk_backend(&cfg.chunk_backend, cfg.chunk_cache_capacity_bytes).await?;

    // Load the narinfo signing key. `None` path → `None` signer (not
    // an error — signing is optional). Bad path / bad format → `?`
    // (operator configured a key; failing silently = unsigned paths
    // = security surprise).
    let signer = Signer::load(cfg.signing_key_path.as_deref())
        .map_err(|e| anyhow::anyhow!("signing key load failed: {e}"))?;
    if signer.is_some() {
        info!(path = ?cfg.signing_key_path, "narinfo signing enabled");
    }

    // HMAC verifier for assignment tokens. Same key file as the
    // scheduler's signer. None → accept all PutPath (dev mode).
    let hmac_verifier = rio_common::hmac::HmacVerifier::load(cfg.hmac_key_path.as_deref())
        .map_err(|e| anyhow::anyhow!("HMAC key load: {e}"))?;
    if hmac_verifier.is_some() {
        info!("HMAC assignment token verification enabled on PutPath");
    }

    // StoreServiceImpl: inline-only vs chunked based on cache. The
    // signer + hmac_verifier chain after either constructor.
    let store_service = match &chunk_cache {
        None => StoreServiceImpl::new(pool.clone()),
        Some(cache) => StoreServiceImpl::with_chunk_cache(pool.clone(), Arc::clone(cache)),
    };
    let store_service = match signer {
        // Wrap the cluster Signer in a TenantSigner — per-tenant key
        // lookup hits `tenant_keys` on the same PG pool. `pool.clone()`
        // is cheap (Arc bump). Paths without tenant attribution (mTLS
        // bypass, dev mode) fall through to the cluster key inside
        // resolve_once (via maybe_sign); no extra DB roundtrip on the None path.
        Some(s) => store_service.with_signer(TenantSigner::new(s, pool.clone())),
        None => store_service,
    };
    let store_service = match hmac_verifier {
        Some(v) => store_service.with_hmac_verifier(v),
        None => store_service,
    };
    // HMAC bypass CN allowlist. Always set (default is ["rio-gateway"]);
    // an empty Vec means NO bypass — every PutPath needs a token.
    // with_hmac_bypass_cns replaces the constructor default unconditionally
    // so the config is the single source of truth (unlike nar_buffer_budget
    // above which only overrides when explicitly set).
    let store_service = store_service.with_hmac_bypass_cns(cfg.hmac_bypass_cns);
    // NAR buffer budget override. None → constructor already set
    // DEFAULT_NAR_BUDGET (32 GiB); Some → replace the semaphore.
    // `as usize`: lossless on 64-bit; on 32-bit (not a supported
    // target) it would truncate, but so would DEFAULT_NAR_BUDGET.
    let store_service = match cfg.nar_buffer_budget_bytes {
        Some(budget) => {
            info!(
                budget_bytes = budget,
                "NAR buffer budget overridden from config"
            );
            store_service.with_nar_budget(budget as usize)
        }
        None => store_service,
    };

    // ChunkServiceImpl: same cache Arc. None → FAILED_PRECONDITION
    // on GetChunk, which is correct for an inline-only store (there
    // ARE no chunks to get).
    let chunk_service = ChunkServiceImpl::new(pool.clone(), chunk_cache.clone());

    // StoreAdminServiceImpl: TriggerGC + PinPath/UnpinPath +
    // ResignPaths backfill. Gets the chunk backend directly (for
    // key_for in sweep's pending_s3_deletes enqueue) AND the
    // chunk cache (for ResignPaths NAR reassembly) AND the signer
    // (for ResignPaths re-sign). None for inline-only stores —
    // sweep does CASCADE delete only, ResignPaths handles inline
    // blobs without the cache.
    //
    // Signer is shared with StoreServiceImpl (same Arc) so
    // PutPath and ResignPaths produce sigs under the SAME key.
    //
    // Also spawn GC background tasks (orphan scanner + orphan-chunk
    // sweep + drain). All periodic (15min / 1h / 30s).
    // spawn_monitored: if one panics, logged; store keeps serving
    // (degraded GC, not down).
    let chunk_backend_for_gc: Option<Arc<dyn ChunkBackend>> =
        chunk_cache.as_ref().map(|c| c.backend());
    let admin_service = {
        let mut s = StoreAdminServiceImpl::new(pool.clone(), chunk_backend_for_gc.clone())
            .with_shutdown(shutdown.clone());
        if let Some(cache) = &chunk_cache {
            s = s.with_chunk_cache(Arc::clone(cache));
        }
        if let Some(signer) = store_service.signer() {
            s = s.with_signer(signer);
        }
        s
    };
    rio_store::gc::orphan::spawn_scanner(
        pool.clone(),
        chunk_backend_for_gc.clone(),
        shutdown.clone(),
    );
    rio_store::gc::sweep::spawn_orphan_chunk_sweep(
        pool.clone(),
        chunk_backend_for_gc.clone(),
        shutdown.clone(),
    );
    if let Some(backend) = chunk_backend_for_gc {
        rio_store::gc::drain::spawn_drain_task(pool.clone(), backend, shutdown.clone());
    }

    if let Some(http_addr) = cfg.cache_http_addr {
        spawn_cache_http(
            http_addr,
            pool.clone(),
            chunk_cache.clone(),
            cfg.cache_allow_unauthenticated,
            shutdown.clone(),
        );
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

    // Server TLS + plaintext health split. K8s gRPC probes can't do
    // mTLS, so when TLS is on, spawn a second plaintext listener
    // with ONLY health, sharing the SAME HealthReporter so
    // set_serving above propagates. See rio_common::server docs.
    let server_tls = rio_common::tls::load_server_tls(&cfg.tls)?;
    if server_tls.is_some() {
        rio_common::server::spawn_health_plaintext(
            health_service.clone(),
            cfg.health_addr,
            serve_shutdown.clone(),
        );
        info!("server mTLS enabled — clients must present CA-signed certs");
    }

    // JWT pubkey from ConfigMap mount + SIGHUP reload loop. One
    // gateway signing key → one pubkey across all verifier services →
    // same ConfigMap mount path, same SIGHUP rotation story as
    // scheduler. See load_and_wire_jwt docstring for None→inert /
    // Some→fail-fast. Parent shutdown token: reload loop stops on
    // SIGTERM instantly, same disposition as orphan-scanner/GC-drain.
    let jwt_pubkey = rio_common::jwt_interceptor::load_and_wire_jwt(
        cfg.jwt.key_path.as_deref(),
        shutdown.clone(),
    )?;

    info!(
        addr = %addr,
        max_msg_size,
        tls = server_tls.is_some(),
        jwt = jwt_pubkey.is_some(),
        "starting gRPC server"
    );

    let mut builder = Server::builder();
    if let Some(tls) = server_tls {
        builder = builder.tls_config(tls)?;
    }
    builder
        // JWT tenant-token verify layer. jwt_pubkey computed above.
        // Installed unconditionally for type stability (see
        // scheduler/main.rs for the full note).
        //
        // Permissive-on-absent matters MORE for store than scheduler:
        // workers call StoreService.PutPath with HMAC assignment tokens
        // (no JWT). If absent-header were a rejection, worker uploads
        // would break the moment the pubkey is configured. Same layer
        // wrapping ChunkService/StoreAdminService is harmless — those
        // callers never set x-rio-tenant-token either.
        .layer(tonic::service::InterceptorLayer::new(
            rio_common::jwt_interceptor::jwt_interceptor(jwt_pubkey),
        ))
        .add_service(health_service)
        .add_service(StoreServiceServer::new(store_service).max_decoding_message_size(max_msg_size))
        .add_service(ChunkServiceServer::new(chunk_service))
        .add_service(StoreAdminServiceServer::new(admin_service))
        .serve_with_shutdown(addr, serve_shutdown.cancelled_owned())
        .await?;

    info!("store shut down cleanly");
    Ok(())
}

// ── bootstrap helpers (extracted from main) ──────────────────────────

/// Connect to PostgreSQL and run migrations. URL is logged with
/// password redacted.
async fn init_db_pool(database_url: &str) -> anyhow::Result<sqlx::PgPool> {
    info!(
        url = %rio_common::config::redact_db_url(database_url),
        "connecting to PostgreSQL"
    );
    let pool = PgPoolOptions::new()
        .max_connections(20)
        .connect(database_url)
        .await?;
    info!("PostgreSQL connection established");

    sqlx::migrate!("../migrations")
        .run(&pool)
        .await
        .inspect_err(|e| error!(error = %e, "database migrations failed"))?;
    info!("database migrations applied");

    Ok(pool)
}

/// Construct the chunk backend + ONE shared `ChunkCache`.
///
/// The cache Arc is cloned into all three consumers
/// (`StoreServiceImpl`, `ChunkServiceImpl`, `CacheServerState`) — a
/// chunk warmed by GetPath is hot for GetChunk and for the
/// binary-cache `/nar/` endpoint.
///
/// `?` on backend construction: filesystem mkdir fail or S3
/// bad-region means we can't store chunks — startup error, not
/// degraded mode. Inline backend can't fail (returns `None`).
async fn init_chunk_backend(
    kind: &ChunkBackendKind,
    cache_capacity_bytes: u64,
) -> anyhow::Result<Option<Arc<ChunkCache>>> {
    Ok(match kind {
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
                cache_capacity_bytes,
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
                cache_capacity_bytes,
            )))
        }
    })
}

/// Binary-cache HTTP server (narinfo + nar.zst routes).
///
/// Spawned concurrently with the gRPC server (which blocks on serve()
/// in main()). This is optional; the gRPC store works without it.
/// `chunk_cache` is the same Arc as GetPath — `/nar/` reassembly
/// hits the same moka LRU.
fn spawn_cache_http(
    http_addr: std::net::SocketAddr,
    pool: sqlx::PgPool,
    chunk_cache: Option<Arc<ChunkCache>>,
    allow_unauthenticated: bool,
    shutdown: rio_common::signal::Token,
) {
    let state = Arc::new(CacheServerState {
        pool,
        chunk_cache,
        allow_unauthenticated,
    });
    let router = cache_server::router(state);
    rio_common::task::spawn_monitored("cache-http-server", async move {
        info!(addr = %http_addr, "starting binary-cache HTTP server");
        match tokio::net::TcpListener::bind(http_addr).await {
            Ok(listener) => {
                if let Err(e) = axum::serve(listener, router)
                    .with_graceful_shutdown(shutdown.cancelled_owned())
                    .await
                {
                    error!(error = %e, "cache HTTP server failed");
                }
            }
            Err(e) => {
                error!(error = %e, addr = %http_addr, "cache HTTP bind failed");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_common::config::ValidateConfig as _;

    #[test]
    fn config_defaults_are_stable() {
        let d = Config::default();
        assert_eq!(d.listen_addr, "0.0.0.0:9002");
        assert_eq!(d.metrics_addr.to_string(), "0.0.0.0:9092");
        assert!(d.database_url.is_empty());
        // Chunk backend off by default for backward-compat with pre-chunking configs.
        assert!(matches!(d.chunk_backend, ChunkBackendKind::Inline));
        // Matches ChunkCache::DEFAULT_CACHE_CAPACITY_BYTES. If that
        // constant changes, update this — the test catches drift.
        assert_eq!(d.chunk_cache_capacity_bytes, 2 * 1024 * 1024 * 1024);
        // NAR budget override: None → DEFAULT_NAR_BUDGET (grpc/mod.rs).
        assert!(d.nar_buffer_budget_bytes.is_none());
        assert!(d.signing_key_path.is_none());
        assert!(d.cache_http_addr.is_none());
        // Plaintext health listener for K8s probes when mTLS is on the main port.
        assert_eq!(d.health_addr.to_string(), "0.0.0.0:9102");
        assert!(!d.tls.is_configured());
        // HMAC bypass allowlist defaults to rio-gateway (backward-compat
        // with the pre-allowlist hardcoded CN check).
        assert_eq!(d.hmac_bypass_cns, vec!["rio-gateway".to_string()]);
        // Binary cache auth required by default — fail loud on misconfigured deployments.
        assert!(!d.cache_allow_unauthenticated);
        assert_eq!(d.drain_grace_secs, 6);
        // JWT verification off by default (interceptor inert until
        // ConfigMap mount configured via RIO_JWT__KEY_PATH).
        assert!(d.jwt.key_path.is_none());
        assert!(!d.jwt.required);
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

    /// Env-var tagged-enum parsing via the real rio_common::config::load
    /// path (Serialized::defaults → Env::prefixed("RIO_").split("__") →
    /// extract). The deploy overlays set chunk_backend this way —
    /// regression guard for kustomization.yaml.
    ///
    /// The defaults layer serializes Inline as {kind: "inline"}; figment's
    /// per-key merge must correctly replace it with {kind: "s3",
    /// bucket: ..., prefix: ...} from the env layer. Half-merges (stale
    /// kind, orphan fields) would fail tagged-enum deserialization.
    ///
    /// Jail: serializes env mutation under a global mutex. The closure's
    /// Result<(), figment::Error> return type is figment's API (208-byte
    /// error) — clippy allow is local to these tests.
    #[test]
    #[allow(clippy::result_large_err)]
    fn chunk_backend_kind_env_s3() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("RIO_CHUNK_BACKEND__KIND", "s3");
            jail.set_env("RIO_CHUNK_BACKEND__BUCKET", "rio-chunks");
            jail.set_env("RIO_CHUNK_BACKEND__PREFIX", "");
            let cfg: Config = rio_common::config::load("store", CliArgs::default()).unwrap();
            match cfg.chunk_backend {
                ChunkBackendKind::S3 { bucket, prefix } => {
                    assert_eq!(bucket, "rio-chunks");
                    assert_eq!(prefix, "");
                }
                other => panic!("env vars must override default Inline; got {other:?}"),
            }
            Ok(())
        });
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn chunk_backend_kind_env_filesystem() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("RIO_CHUNK_BACKEND__KIND", "filesystem");
            jail.set_env("RIO_CHUNK_BACKEND__BASE_DIR", "/var/lib/chunks");
            let cfg: Config = rio_common::config::load("store", CliArgs::default()).unwrap();
            match cfg.chunk_backend {
                ChunkBackendKind::Filesystem { base_dir } => {
                    assert_eq!(base_dir, PathBuf::from("/var/lib/chunks"));
                }
                other => panic!("expected Filesystem; got {other:?}"),
            }
            Ok(())
        });
    }

    /// P0218 T2: nar_buffer_budget_bytes TOML roundtrip via the real
    /// `rio_common::config::load` path. Jail changes cwd to a temp dir;
    /// `./store.toml` in there is picked up by load()'s `{component}.toml`
    /// layer.
    ///
    /// The "value reaches StoreServiceImpl" half of this roundtrip is the
    /// `with_nar_budget` builder test at grpc/put_path.rs —
    /// `with_nar_budget(N)` → `available_permits() == N`. This test
    /// covers the config-parse side; main()'s match at startup glues
    /// the two.
    #[test]
    #[allow(clippy::result_large_err)]
    fn nar_buffer_budget_toml_roundtrip() {
        figment::Jail::expect_with(|jail| {
            jail.create_file("store.toml", "nar_buffer_budget_bytes = 12345")?;
            let cfg: Config = rio_common::config::load("store", CliArgs::default()).unwrap();
            assert_eq!(
                cfg.nar_buffer_budget_bytes,
                Some(12345),
                "store.toml nar_buffer_budget_bytes must thread through figment"
            );
            Ok(())
        });
    }

    /// Absent from TOML → None, not Some(0). The struct-level
    /// `#[serde(default)]` handles absence via Default::default(),
    /// which sets None. main()'s match then keeps DEFAULT_NAR_BUDGET.
    #[test]
    #[allow(clippy::result_large_err)]
    fn nar_buffer_budget_absent_is_none() {
        figment::Jail::expect_with(|jail| {
            jail.create_file("store.toml", r#"listen_addr = "0.0.0.0:9002""#)?;
            let cfg: Config = rio_common::config::load("store", CliArgs::default()).unwrap();
            assert!(
                cfg.nar_buffer_budget_bytes.is_none(),
                "absent key must not serialize to Some; got {:?}",
                cfg.nar_buffer_budget_bytes
            );
            Ok(())
        });
    }

    #[test]
    fn cli_args_parse_help() {
        use clap::CommandFactory;
        CliArgs::command().debug_assert();
    }

    // figment::Jail standing-guard tests — see rio-test-support/src/config.rs.
    // When you add Config.newfield: ADD IT to both assert blocks below.

    rio_test_support::jail_roundtrip!(
        "store",
        r#"
        nar_buffer_budget_bytes = 99999
        hmac_bypass_cns = ["rio-gateway", "rio-admin"]
        chunk_cache_capacity_bytes = 123456

        [chunk_backend]
        kind = "filesystem"
        base_dir = "/custom/path"

        [tls]
        cert_path = "/etc/tls/cert.pem"

        [jwt]
        required = true
        "#,
        |cfg: Config| {
            assert_eq!(cfg.nar_buffer_budget_bytes, Some(99999));
            assert_eq!(cfg.chunk_cache_capacity_bytes, 123456);
            assert_eq!(cfg.hmac_bypass_cns, vec!["rio-gateway", "rio-admin"]);
            assert!(
                matches!(cfg.chunk_backend, ChunkBackendKind::Filesystem { .. }),
                "[chunk_backend] table must thread through figment"
            );
            assert_eq!(
                cfg.tls.cert_path.as_deref(),
                Some(std::path::Path::new("/etc/tls/cert.pem")),
                "[tls] table must thread through figment into TlsConfig"
            );
            assert!(
                cfg.jwt.required,
                "[jwt] table must thread through figment into JwtConfig"
            );
            // Unspecified sub-field defaults via #[serde(default)]
            // on the sub-struct (partial table must work).
            assert_eq!(cfg.jwt.resolve_timeout_ms, 500);
        }
    );

    rio_test_support::jail_defaults!("store", r#"listen_addr = "0.0.0.0:9002""#, |cfg: Config| {
        assert!(matches!(cfg.chunk_backend, ChunkBackendKind::Inline));
        assert!(cfg.nar_buffer_budget_bytes.is_none());
        assert!(!cfg.tls.is_configured());
        assert_eq!(cfg.jwt, rio_common::config::JwtConfig::default());
        assert_eq!(cfg.hmac_bypass_cns, vec!["rio-gateway".to_string()]);
        assert!(cfg.signing_key_path.is_none());
        assert!(cfg.hmac_key_path.is_none());
        assert!(cfg.cache_http_addr.is_none());
        assert!(!cfg.cache_allow_unauthenticated);
    });

    // -----------------------------------------------------------------------
    // validate_config rejection tests — spreads the P0409 pattern
    // (rio-scheduler/src/main.rs) to the store.
    // -----------------------------------------------------------------------

    /// `Config::default()` leaves `database_url` empty, which
    /// validate_config rejects. Fill it with a placeholder so the
    /// returned config passes as-is.
    fn test_valid_config() -> Config {
        Config {
            database_url: "postgres://localhost/rio".into(),
            ..Config::default()
        }
    }

    #[test]
    fn config_rejects_empty_database_url() {
        let cfg = Config {
            database_url: String::new(),
            ..test_valid_config()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("database_url"), "{err}");
    }

    /// Whitespace-only database_url must be rejected as empty.
    /// Regression guard for `ensure_required`'s trim — pre-helper,
    /// bare `is_empty()` accepted `"   "`, sqlx connect failed later
    /// with a cryptic URL-parse error buried in startup logs.
    #[test]
    fn config_rejects_whitespace_database_url() {
        let mut cfg = test_valid_config();
        cfg.database_url = "   ".into();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("database_url is required"),
            "whitespace-only database_url must be rejected as empty, got: {err}"
        );
    }

    /// Baseline: `test_valid_config()` itself passes — proves
    /// rejection tests test ONLY their mutation.
    #[test]
    fn config_accepts_valid() {
        test_valid_config()
            .validate()
            .expect("valid config should pass");
    }
}
