//! rio-store binary configuration.
//!
//! Figment-loaded (TOML + `RIO_` env vars + CLI flags) via
//! [`rio_common::config::load`]. See `rio-common/src/config.rs` for the
//! two-struct (Config + CliArgs) split rationale.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use serde::{Deserialize, Serialize};
use tracing::info;

use rio_common::s3::DEFAULT_S3_MAX_ATTEMPTS;
use rio_store::backend::{ChunkBackend, FilesystemChunkBackend, S3ChunkBackend};
use rio_store::cas::ChunkCache;

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
pub(crate) enum ChunkBackendKind {
    /// No chunk backend. All NARs inline in PG. ChunkService returns
    /// FAILED_PRECONDITION.
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

// r[impl store.netpol.egress+2]
// Egress targets are exactly what's configured here: postgres
// (`database_url`) and the chunk backend (S3 or filesystem). The
// `store-egress` CiliumNetworkPolicy in infra/helm/rio-build/templates/
// networkpolicy.yaml allows CoreDNS + postgres:5432 (toEndpoints +
// postgresCidr) + S3-VPC-endpoint:443 only — tracey doesn't scan YAML;
// this Config is the scannable anchor.
#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Config {
    pub listen_addr: std::net::SocketAddr,
    pub database_url: String,
    #[serde(flatten)]
    pub common: rio_common::config::CommonConfig,
    /// Where chunks live. Default: inline (no backend). See
    /// [`ChunkBackendKind`] for TOML syntax.
    pub chunk_backend: ChunkBackendKind,
    /// moka LRU capacity for chunk reads, in bytes. Default 2 GiB.
    /// One cache shared by StoreService + ChunkService — a chunk
    /// warmed by either is hot for both. Only relevant when
    /// chunk_backend != inline.
    pub chunk_cache_capacity_bytes: u64,
    /// Global NAR reassembly buffer budget in bytes — total permits
    /// across ALL concurrent PutPath handlers. Each handler acquires
    /// `chunk.len()` permits before extending its accumulation Vec.
    /// None → DEFAULT_NAR_BUDGET (8 × MAX_NAR_SIZE = 32 GiB). Lower
    /// this on small-memory nodes; raise it if you have >8 concurrent
    /// max-size uploads and RAM to match.
    pub nar_buffer_budget_bytes: Option<u64>,
    /// ed25519 narinfo signing key path (Nix secret-key format:
    /// `name:base64-seed`). None = signing disabled (paths stored
    /// without our signature; still serveable, just unverified). The
    /// key file should be mode 0600 and NOT in git.
    pub signing_key_path: Option<PathBuf>,
    /// HMAC key file for verifying assignment tokens on PutPath.
    /// SAME file as scheduler's `hmac_key_path`. Unset = accept
    /// all PutPath callers (dev mode).
    pub hmac_key_path: Option<PathBuf>,
    /// JWT verification. `key_path` → ConfigMap mount at
    /// `/etc/rio/jwt/ed25519_pubkey` (same mount as scheduler — one
    /// gateway signing key → one pubkey across all verifier services).
    /// Unset = interceptor inert (dev mode). SIGHUP reloads from the
    /// same path. Set via `RIO_JWT__KEY_PATH`.
    pub jwt: rio_common::config::JwtConfig,
    /// HMAC key file for verifying `x-rio-service-token` on PutPath.
    /// SEPARATE from `hmac_key_path` (different secret). Unset =
    /// service-token bypass disabled (gateway falls back to mTLS
    /// CN-allowlist). Set via `RIO_SERVICE_HMAC_KEY_PATH`.
    pub service_hmac_key_path: Option<PathBuf>,
    /// `ServiceClaims.caller` values whose `x-rio-service-token` is
    /// honoured (PutPath HMAC-bypass and `x-rio-probe-tenant-id` gate).
    /// Default: `["rio-gateway", "rio-scheduler"]`.
    pub service_bypass_callers: Vec<String>,
    /// Max concurrent S3 chunk uploads per `put_chunked` call.
    /// Default 32 — with `RIO_SUBSTITUTE_MAX_CONCURRENT` (scheduler
    /// side; helm-default 16, code-default 256, P0473) this caps
    /// total in-flight S3 puts at ~512 under helm settings, under
    /// the aws-sdk's ~1024 default pool. Raise if the
    /// store runs with a larger aws-sdk pool; lower (min 1) if you see
    /// `DispatchFailure` in store logs during large-NAR ingest.
    /// Set via `RIO_CHUNK_UPLOAD_MAX_CONCURRENT`.
    pub chunk_upload_max_concurrent: usize,
    /// Max aws-sdk retry attempts per S3 operation (PutObject,
    /// GetObject, HeadObject). Default 10 — raised from the aws-sdk
    /// default of 3 because S3-compatible backends (rustfs, MinIO)
    /// recycle connections more aggressively than AWS S3, surfacing
    /// as transient `DispatchFailure` that the sdk's standard retry
    /// policy handles but exhausts at 3. Set via `RIO_S3_MAX_ATTEMPTS`.
    pub s3_max_attempts: u32,
    /// Cap on paths in a FindMissingPaths request (DoS guard).
    /// Default 1M — ~80 MB of path strings per request. Set via
    /// `RIO_MAX_BATCH_PATHS`.
    pub max_batch_paths: usize,
    /// PG connection pool size. Default 50 (was hardcoded 20). The
    /// QueryPathInfo / FindMissingPaths hot path under autoscaled
    /// builder load (60+ builders × ~100 input paths each at fan-out)
    /// is bottlenecked on `sqlx::pool::acquire`, not query latency
    /// (PK lookups). Aurora handles hundreds of connections per
    /// instance; raise this with `replicas` for thousands-of-builds
    /// scale. Set via `RIO_PG_MAX_CONNECTIONS`.
    pub pg_max_connections: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_addr: rio_common::default_addr(9002),
            database_url: String::new(),
            common: rio_common::config::CommonConfig::new(9092),
            chunk_backend: ChunkBackendKind::default(),
            // 2 GiB. Matches ChunkCache::DEFAULT_CACHE_CAPACITY_BYTES
            // — the constant is crate-private so duplicated here,
            // but the config_defaults_are_stable test catches drift.
            chunk_cache_capacity_bytes: 2 * 1024 * 1024 * 1024,
            nar_buffer_budget_bytes: None,
            signing_key_path: None,
            hmac_key_path: None,
            jwt: rio_common::config::JwtConfig::default(),
            service_hmac_key_path: None,
            service_bypass_callers: vec!["rio-gateway".into(), "rio-scheduler".into()],
            chunk_upload_max_concurrent: rio_store::cas::DEFAULT_CHUNK_UPLOAD_CONCURRENCY,
            s3_max_attempts: DEFAULT_S3_MAX_ATTEMPTS,
            max_batch_paths: rio_store::grpc::DEFAULT_MAX_BATCH_PATHS,
            pg_max_connections: DEFAULT_PG_MAX_CONNECTIONS,
        }
    }
}

/// Default PG pool size. Raised from sqlx's 10 (and the prior hardcoded
/// 20) after I-076: 60 autoscaled builders at hello-shallow fan-out
/// drove `acquired_after_secs=16` on QueryPathInfo. The query is a PK
/// lookup; the bottleneck is connection acquisition.
pub(crate) const DEFAULT_PG_MAX_CONNECTIONS: u32 = 50;

#[derive(Parser, Serialize, Default)]
#[command(
    name = "rio-store",
    about = "NAR content-addressable store for rio-build"
)]
pub(crate) struct CliArgs {
    /// gRPC listen address
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    listen_addr: Option<std::net::SocketAddr>,

    /// PostgreSQL connection URL
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    database_url: Option<String>,

    /// Prometheus metrics listen address
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    metrics_addr: Option<std::net::SocketAddr>,
}

impl rio_common::config::ValidateConfig for Config {
    /// Reject operator-settable config that produces a silent hang or
    /// degenerate state at startup, not at first use. Every field that
    /// meets that bar is checked here.
    fn validate(&self) -> anyhow::Result<()> {
        use rio_common::config::ensure_required as required;
        use rio_common::limits::MIN_NAR_CHUNK_CHARGE;
        required(&self.database_url, "database_url", "store")?;
        // 0 → buffer_unordered(0) returns Pending forever (no waker):
        // every put_chunked silently hangs the data plane.
        anyhow::ensure!(
            self.chunk_upload_max_concurrent >= 1,
            "chunk_upload_max_concurrent must be >= 1 (0 hangs uploads); \
             set RIO_CHUNK_UPLOAD_MAX_CONCURRENT"
        );
        // 0 → aws-sdk RetryConfig::with_max_attempts(0) makes zero
        // attempts: every S3 op fails immediately.
        anyhow::ensure!(
            self.s3_max_attempts >= 1,
            "s3_max_attempts must be >= 1; set RIO_S3_MAX_ATTEMPTS"
        );
        // < MIN_NAR_CHUNK_CHARGE → Semaphore::new(n<256); every PutPath
        // acquire_many(chunk.len().max(256)) is Pending forever. There
        // is no "unlimited" sentinel; unset for the 32 GiB default.
        anyhow::ensure!(
            self.nar_buffer_budget_bytes
                .is_none_or(|b| b >= MIN_NAR_CHUNK_CHARGE as u64),
            "nar_buffer_budget_bytes must be >= {MIN_NAR_CHUNK_CHARGE} \
             (smaller hangs all uploads); unset RIO_NAR_BUFFER_BUDGET_BYTES \
             for the 32 GiB default — there is no 'unlimited' sentinel"
        );
        // 0 → PgPoolOptions max_connections(0) → PoolTimedOut after 30s
        // with a misleading message.
        anyhow::ensure!(
            self.pg_max_connections >= 1,
            "pg_max_connections must be >= 1; set RIO_PG_MAX_CONNECTIONS"
        );
        // 0 → every FindMissingPaths rejected with InvalidArgument.
        anyhow::ensure!(
            self.max_batch_paths >= 1,
            "max_batch_paths must be >= 1; set RIO_MAX_BATCH_PATHS"
        );
        Ok(())
    }
}

rio_common::impl_has_common_config!(Config);

/// Construct the chunk backend + ONE shared `ChunkCache`.
///
/// The cache Arc is cloned into both consumers (`StoreServiceImpl`,
/// `ChunkServiceImpl`) — a chunk warmed by GetPath is hot for
/// GetChunk.
///
/// `?` on backend construction: filesystem mkdir fail or S3
/// bad-region means we can't store chunks — startup error, not
/// degraded mode. Inline backend can't fail (returns `None`).
pub(crate) async fn init_chunk_backend(
    kind: &ChunkBackendKind,
    cache_capacity_bytes: u64,
    s3_max_attempts: u32,
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
            info!(%bucket, %prefix, s3_max_attempts, "chunk backend: S3");
            // Credentials from the aws-sdk default chain (env vars,
            // IMDS, etc). NOT in our config — we don't want secrets
            // in TOML. If credentials are missing, the first PutPath
            // will fail with a clear AWS error; we don't eagerly
            // verify here (would need a HeadBucket or similar, and
            // credentials might not be available YET if IMDS is
            // slow — better to start serving and fail the first
            // chunk op than to race IMDS).
            //
            // r[impl store.cas.s3-retry]
            // Two departures from aws-sdk defaults (raised
            // max_attempts, stalled-stream protection OFF) — see
            // rio_common::s3::default_client for the full rationale.
            // Shared with rio-scheduler's log flusher so the two
            // services don't drift on credential/endpoint/retry
            // resolution.
            let client = rio_common::s3::default_client(s3_max_attempts).await;
            let backend: Arc<dyn ChunkBackend> =
                Arc::new(S3ChunkBackend::new(client, bucket.clone(), prefix.clone()));
            Some(Arc::new(ChunkCache::with_capacity(
                backend,
                cache_capacity_bytes,
            )))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_common::config::ValidateConfig as _;

    #[test]
    fn config_defaults_are_stable() {
        let d = Config::default();
        assert_eq!(d.listen_addr.to_string(), "[::]:9002");
        assert_eq!(d.common.metrics_addr.to_string(), "[::]:9092");
        assert!(d.database_url.is_empty());
        // Chunk backend off by default for backward-compat with pre-chunking configs.
        assert!(matches!(d.chunk_backend, ChunkBackendKind::Inline));
        // Matches ChunkCache::DEFAULT_CACHE_CAPACITY_BYTES. If that
        // constant changes, update this — the test catches drift.
        assert_eq!(d.chunk_cache_capacity_bytes, 2 * 1024 * 1024 * 1024);
        // NAR budget override: None → DEFAULT_NAR_BUDGET (grpc/mod.rs).
        assert!(d.nar_buffer_budget_bytes.is_none());
        assert!(d.signing_key_path.is_none());
        // with the pre-allowlist hardcoded CN check).
        assert_eq!(d.common.drain_grace, std::time::Duration::from_secs(6));
        // JWT verification off by default (interceptor inert until
        // ConfigMap mount configured via RIO_JWT__KEY_PATH).
        assert!(d.jwt.key_path.is_none());
        assert!(!d.jwt.required);
        assert_eq!(d.max_batch_paths, rio_store::grpc::DEFAULT_MAX_BATCH_PATHS);
        assert_eq!(d.pg_max_connections, DEFAULT_PG_MAX_CONNECTIONS);
    }

    /// `chunk_upload_max_concurrent=0` → `buffer_unordered(0)` hangs
    /// every put_chunked permanently. validate() must reject at
    /// startup, not silently hang the data plane.
    #[test]
    fn validate_rejects_zero_upload_concurrency() {
        let cfg = Config {
            database_url: "postgres://x".into(),
            chunk_upload_max_concurrent: 0,
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("chunk_upload_max_concurrent"),
            "error names the field: {err}"
        );
    }

    /// Discovered alongside chunk_upload_max_concurrent: aws-sdk
    /// RetryConfig::with_max_attempts(0) makes zero attempts.
    #[test]
    fn validate_rejects_zero_s3_attempts() {
        let cfg = Config {
            database_url: "postgres://x".into(),
            s3_max_attempts: 0,
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("s3_max_attempts"), "got: {err}");
    }

    /// `nar_buffer_budget_bytes=Some(0)` → `Semaphore::new(0)` → every
    /// PutPath `acquire_many(≥256)` Pending forever, store wedged
    /// silently with green health checks.
    #[test]
    fn validate_rejects_zero_nar_budget() {
        let cfg = Config {
            database_url: "postgres://x".into(),
            nar_buffer_budget_bytes: Some(0),
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("nar_buffer_budget_bytes"), "got: {err}");
    }

    /// Any budget < MIN_NAR_CHUNK_CHARGE has identical Pending-forever
    /// behavior because `acquire_many` floors at 256.
    #[test]
    fn validate_rejects_sub_min_nar_budget() {
        let cfg = Config {
            database_url: "postgres://x".into(),
            nar_buffer_budget_bytes: Some(100),
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("nar_buffer_budget_bytes"), "got: {err}");
        // None (unset) is fine — that's the 32 GiB default.
        let ok = Config {
            database_url: "postgres://x".into(),
            nar_buffer_budget_bytes: None,
            ..Default::default()
        };
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn validate_rejects_zero_pg_connections() {
        let cfg = Config {
            database_url: "postgres://x".into(),
            pg_max_connections: 0,
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("pg_max_connections"), "got: {err}");
    }

    #[test]
    fn validate_rejects_zero_max_batch_paths() {
        let cfg = Config {
            database_url: "postgres://x".into(),
            max_batch_paths: 0,
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("max_batch_paths"), "got: {err}");
    }

    // r[verify store.cas.s3-retry]
    /// The spec pins `RIO_S3_MAX_ATTEMPTS` default at 10. aws-sdk's
    /// out-of-box is 3 — insufficient for S3-compatible backends that
    /// recycle idle connections aggressively. If someone changes
    /// DEFAULT_S3_MAX_ATTEMPTS without reading the spec, this fails.
    #[test]
    fn s3_retry_default_matches_spec() {
        assert_eq!(DEFAULT_S3_MAX_ATTEMPTS, 10);
        assert_eq!(Config::default().s3_max_attempts, 10);
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
        chunk_cache_capacity_bytes = 123456
        chunk_upload_max_concurrent = 64
        s3_max_attempts = 5

        [chunk_backend]
        kind = "filesystem"
        base_dir = "/custom/path"

        [jwt]
        required = true
        "#,
        |cfg: Config| {
            assert_eq!(cfg.nar_buffer_budget_bytes, Some(99999));
            assert_eq!(cfg.chunk_cache_capacity_bytes, 123456);
            assert_eq!(cfg.chunk_upload_max_concurrent, 64);
            assert_eq!(cfg.s3_max_attempts, 5);
            assert!(
                matches!(cfg.chunk_backend, ChunkBackendKind::Filesystem { .. }),
                "[chunk_backend] table must thread through figment"
            );
            assert!(
                cfg.jwt.required,
                "[jwt] table must thread through figment into JwtConfig"
            );
            // Unspecified sub-field defaults via #[serde(default)]
            // on the sub-struct (partial table must work).
        }
    );

    rio_test_support::jail_defaults!("store", r#"listen_addr = "0.0.0.0:9002""#, |cfg: Config| {
        assert!(matches!(cfg.chunk_backend, ChunkBackendKind::Inline));
        assert!(cfg.nar_buffer_budget_bytes.is_none());
        assert_eq!(cfg.jwt, rio_common::config::JwtConfig::default());
        assert!(cfg.signing_key_path.is_none());
        assert!(cfg.hmac_key_path.is_none());
        assert_eq!(
            cfg.chunk_upload_max_concurrent,
            rio_store::cas::DEFAULT_CHUNK_UPLOAD_CONCURRENCY
        );
        assert_eq!(cfg.s3_max_attempts, DEFAULT_S3_MAX_ATTEMPTS);
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

    /// Baseline: `test_valid_config()` itself passes — proves
    /// rejection tests test ONLY their mutation.
    #[test]
    fn config_accepts_valid() {
        test_valid_config()
            .validate()
            .expect("valid config should pass");
    }
}
