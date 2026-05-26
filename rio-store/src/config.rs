//! rio-store binary configuration.
//!
//! Layered-config-loaded (TOML + `RIO_` env vars + CLI flags) via
//! [`rio_common::config::load`]. See `rio-common/src/config.rs` for the
//! two-struct (Config + CliArgs) split rationale.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::backend::{
    ChunkBackend, FilesystemChunkBackend, MemoryChunkBackend, S3ChunkBackend, TieredChunkBackend,
};
use crate::cas::ChunkCache;
use rio_common::s3::DEFAULT_S3_MAX_ATTEMPTS;

/// Chunk storage backend selection.
///
/// Serde internally-tagged (`kind`): TOML writes
/// `[chunk_backend]\nkind = "s3"\nbucket = "..."`. The tag field is
/// `kind` not the serde default `type` — `type` is a Rust keyword
/// and would need `r#type` everywhere we match on it.
///
/// There is no default: every NAR is chunked (P0583 dropped the
/// `inline_blob` PG column), so a store without a chunk backend cannot
/// store anything. `Config::validate` rejects a missing
/// `[chunk_backend]` at startup.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ChunkBackendKind {
    /// In-process `HashMap`. Chunks do not survive a restart and are
    /// not shared across replicas — tests and single-process dev
    /// loops only. Never deploy this.
    Memory,
    /// Local filesystem. 256-subdir fanout by hash prefix (same layout
    /// as git objects). `base_dir` is created at startup.
    Filesystem { base_dir: PathBuf },
    /// S3-compatible. Credentials come from the aws-sdk's default chain
    /// (env vars, instance profile, etc) — NOT in this config. We're
    /// not putting secrets in a TOML file.
    S3 { bucket: String, prefix: String },
    // r[impl infra.express.cache-tier]
    /// Two-tier: per-AZ S3 Express read-through cache over authoritative
    /// S3 standard. `express_bucket = None` (or unset) degrades to the
    /// plain `S3` shape — replicas in AZs without Express still
    /// function. The Express bucket is per-AZ: either set explicitly
    /// (`express_bucket` — single-AZ / zone-pinned deployments, VM
    /// tests) or selected per pod at startup from
    /// `express_bucket_by_zone` keyed by this pod's zone (P0554, see
    /// [`Config::resolve_express_bucket`]). See ADR-023 (tiered chunk
    /// backend).
    Tiered {
        /// Authoritative S3 standard bucket.
        bucket: String,
        /// Key prefix shared by both tiers.
        prefix: String,
        /// Per-AZ S3 Express directory bucket (`*--x-s3` suffix).
        /// `None` = no cache tier in this AZ. Wins over
        /// `express_bucket_by_zone` when both are set.
        #[serde(default)]
        express_bucket: Option<String>,
        /// Per-AZ Express bucket map keyed by zone NAME (the value of
        /// the `topology.kubernetes.io/zone` label, e.g. `us-east-2a`),
        /// for per-pod selection when `express_bucket` is unset: at
        /// startup [`Config::resolve_express_bucket`] looks up
        /// [`Config::node_zone`] here and the matching bucket becomes
        /// this replica's local cache tier. Populated by helm/xtask
        /// from the `express_bucket_by_zone` terraform output. TOML:
        /// a `[chunk_backend.express_bucket_by_zone]` table; env: ONE
        /// JSON-object string —
        /// `RIO_CHUNK_BACKEND__EXPRESS_BUCKET_BY_ZONE='{"us-east-2a":"…--x-s3"}'`
        /// (the `__`-nested env convention has no per-entry syntax for
        /// maps, so the whole map travels as a single value). Empty
        /// (default) = no per-pod selection.
        #[serde(default, deserialize_with = "bucket_by_zone_from_map_or_json")]
        express_bucket_by_zone: BTreeMap<String, String>,
        /// Express cache-tier eviction sweep tuning (P0585). Only
        /// consulted when `express_bucket` is set. TOML
        /// `[chunk_backend.express]`, env `RIO_CHUNK_BACKEND__EXPRESS__*`.
        #[serde(default)]
        express: ExpressConfig,
    },
}

/// Deserialize `express_bucket_by_zone` from either a real map (TOML
/// table, compiled-defaults layer) or a single JSON-object string (the
/// env layer). The layered loader's `RIO_*`/`__` env convention has no
/// syntax for individual map entries, so helm renders the whole map as
/// one JSON value
/// (`RIO_CHUNK_BACKEND__EXPRESS_BUCKET_BY_ZONE='{"us-east-2a":"…"}'`);
/// TOML keeps the natural table form. An empty/whitespace string is an
/// empty map (helm omits the var instead, but a templating layer
/// rendering `""` must not fail config load).
fn bucket_by_zone_from_map_or_json<'de, D>(de: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum MapOrJsonString {
        Map(BTreeMap<String, String>),
        Json(String),
    }

    match MapOrJsonString::deserialize(de)? {
        MapOrJsonString::Map(m) => Ok(m),
        MapOrJsonString::Json(s) => {
            let s = s.trim();
            if s.is_empty() {
                return Ok(BTreeMap::new());
            }
            serde_json::from_str(s).map_err(|e| {
                D::Error::custom(format!(
                    "express_bucket_by_zone: expected a JSON object of \
                     zone→bucket pairs when given as a string \
                     (RIO_CHUNK_BACKEND__EXPRESS_BUCKET_BY_ZONE): {e}"
                ))
            })
        }
    }
}

/// Outcome of per-pod Express bucket selection — see
/// [`select_express_bucket`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpressSelection {
    /// `express_bucket` was set explicitly; the zone map was not
    /// consulted.
    Explicit(String),
    /// Selected from `express_bucket_by_zone` by this pod's zone.
    Selected { zone: String, bucket: String },
    /// No local cache tier for this replica. `reason` names exactly
    /// which prerequisite was missing.
    Disabled { reason: String },
}

// r[impl infra.express.cache-tier]
/// Per-pod Express bucket selection (P0554). Pure so the decision
/// table is unit-testable; [`Config::resolve_express_bucket`] applies
/// it to the loaded config and logs the outcome.
///
/// Precedence: a non-empty explicit `express_bucket` always wins
/// (single-AZ / zone-pinned deployments, VM tests). Otherwise the
/// bucket is `express_bucket_by_zone[node_zone]`; any missing piece
/// (empty map, unknown zone, zone not in the map) selects nothing and
/// the replica runs with `local = None` — direct S3-standard reads,
/// degraded but never down, and never a different AZ's bucket.
pub fn select_express_bucket(
    express_bucket: Option<&str>,
    by_zone: &BTreeMap<String, String>,
    node_zone: Option<&str>,
) -> ExpressSelection {
    // Whitespace-only explicit bucket counts as unset so a templating
    // layer rendering "" can't mask the zone map.
    if let Some(b) = express_bucket.filter(|b| !b.trim().is_empty()) {
        return ExpressSelection::Explicit(b.to_string());
    }
    if by_zone.is_empty() {
        return ExpressSelection::Disabled {
            reason: "express_bucket unset and express_bucket_by_zone is empty".into(),
        };
    }
    let Some(zone) = node_zone.map(str::trim).filter(|z| !z.is_empty()) else {
        return ExpressSelection::Disabled {
            reason: "express_bucket_by_zone is set but the pod zone is unknown \
                     (RIO_NODE_ZONE unset or empty — is the topology.kubernetes.io/zone \
                     pod label present? It requires the PodTopologyLabelsAdmission \
                     plugin, beta in Kubernetes 1.35)"
                .into(),
        };
    };
    match by_zone.get(zone) {
        Some(bucket) => ExpressSelection::Selected {
            zone: zone.to_string(),
            bucket: bucket.clone(),
        },
        None => ExpressSelection::Disabled {
            reason: format!(
                "zone {zone:?} has no entry in express_bucket_by_zone (mapped zones: {})",
                by_zone.keys().cloned().collect::<Vec<_>>().join(", ")
            ),
        },
    }
}

/// S3 Express eviction-sweep tuning (design overview §9 / ADR-023): the
/// per-AZ directory bucket is a bounded read-through cache, and because
/// directory-bucket lifecycle rules are age-based only, this
/// application-level sweep is what enforces the byte budget. The elected
/// sweeper lists the bucket every `sweep_interval_secs`; when the total
/// exceeds `target_bytes × evict_high_watermark` it deletes
/// oldest-by-`LastModified` objects until back under
/// `target_bytes × evict_low_watermark`. See
/// [`crate::backend::express_sweep`].
///
/// Nested under the `tiered` chunk-backend variant so the whole cache
/// tier is configured in one place: TOML `[chunk_backend.express]`, env
/// `RIO_CHUNK_BACKEND__EXPRESS__<FIELD>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ExpressConfig {
    /// Target Express bucket size in bytes. Default 8 TiB
    /// (8_796_093_022_208) — matches the provisioning assumption in
    /// `infra/eks/s3-express.tf`. Set via
    /// `RIO_CHUNK_BACKEND__EXPRESS__TARGET_BYTES`.
    pub target_bytes: u64,
    /// Eviction trigger: a sweep starts deleting only when the listed
    /// total exceeds `target_bytes × evict_high_watermark`. Default 1.10.
    /// Set via `RIO_CHUNK_BACKEND__EXPRESS__EVICT_HIGH_WATERMARK`.
    pub evict_high_watermark: f64,
    /// Eviction floor: once triggered, the sweep deletes oldest objects
    /// until the total is back under `target_bytes × evict_low_watermark`.
    /// Must be ≤ `evict_high_watermark`. Default 0.90. Set via
    /// `RIO_CHUNK_BACKEND__EXPRESS__EVICT_LOW_WATERMARK`.
    pub evict_low_watermark: f64,
    /// Sweep cadence in seconds. Default 3600 (hourly — the full
    /// ListObjectsV2 pass is ~130k requests at the 8 TiB design point,
    /// fine hourly, wasteful much faster). `0` disables the sweeper
    /// entirely (the bucket then grows until the age-based S3 Lifecycle
    /// expiration). Set via
    /// `RIO_CHUNK_BACKEND__EXPRESS__SWEEP_INTERVAL_SECS`.
    pub sweep_interval_secs: u64,
}

impl Default for ExpressConfig {
    fn default() -> Self {
        Self {
            // 8 TiB.
            target_bytes: 8_796_093_022_208,
            evict_high_watermark: 1.10,
            evict_low_watermark: 0.90,
            sweep_interval_secs: 3600,
        }
    }
}

/// NAR compression codec for binary-cache-compat objects
/// (`nar/*.nar.<ext>`).
///
/// Lowercase serde names (`zstd`/`xz`/`none`) are exactly the values a
/// stock-Nix `.narinfo` `Compression:` field carries, so the config
/// string and the published metadata can never disagree on spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CompatCompression {
    /// zstd (`nar/*.nar.zst`). Default — best compress-speed/ratio
    /// trade-off for the synchronous post-commit write path.
    Zstd,
    /// xz (`nar/*.nar.xz`). Smaller objects, much slower to compress —
    /// only worth it when S3 storage cost dominates over PutPath
    /// latency.
    Xz,
    /// No compression (`nar/*.nar`). Debugging / pre-compressed
    /// content only.
    None,
}

/// When the binary-cache-compat write runs relative to the PutPath
/// PostgreSQL commit.
///
/// Single-variant on purpose: ADR-022 §10 documents `async` (bounded
/// background queue, lower PutPath latency, larger reconciler backlog
/// on crash) as a future option that is NOT implemented in the first
/// pass. Modeling the knob as an enum now means turning that on later
/// is a config value, not a config-schema break.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CompatWriteMode {
    /// Write the `.narinfo` + compressed NAR synchronously inside the
    /// PutPath handler, after the PG transaction commits. A failure is
    /// logged + metered but never fails the RPC.
    SyncAfterCommit,
}

// r[impl store.compat.runtime-toggle]
/// Stock-Nix binary-cache compatibility layer (ADR-022 Design Overview
/// §10). When `enabled`, every committed path is *additionally*
/// written to S3-standard as a stock-Nix object pair
/// (`{store-path-hash}.narinfo` + `nar/{file-hash}.nar.<ext>`), so
/// `nix copy --from s3://bucket` substitutes with no rio process
/// running. Runtime config, not a build flag: toggling OFF stops new
/// compat writes (existing objects stay); toggling ON resumes for
/// subsequent puts. Never affects chunked storage either way.
///
/// How the objects get written (`compat::writer`): paths committed
/// through the buffered upload RPCs (`PutPath`/`PutPathBatch` — the
/// gateway surface) are published synchronously after the PG commit;
/// paths committed via `PutPathChunked` or upstream substitution keep
/// `narinfo.compat_file_hash IS NULL` and are backfilled by the P0582
/// compat reconciler off the upload hot path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct BinaryCacheCompat {
    /// Master switch: `true` (default) additionally publishes every
    /// committed path to the compat bucket as a stock-Nix `.narinfo` +
    /// compressed-NAR pair; `false` ("pure rio mode") stops new compat
    /// writes (existing objects stay). Default ON is the
    /// migration-phase posture: the S3 bucket stays readable by plain
    /// Nix and PG loss degrades to the stock-Nix path instead of an
    /// outage. Flip OFF once every consumer is rio-aware to roughly
    /// halve S3 storage. Set via `RIO_BINARY_CACHE_COMPAT__ENABLED`.
    pub enabled: bool,
    /// Bucket for compat objects. `None` (default) = the chunk
    /// backend's S3-standard bucket (`chunk_backend.bucket`). Set only
    /// to publish the binary cache somewhere other than the chunk
    /// store. Must be non-empty when set. Set via
    /// `RIO_BINARY_CACHE_COMPAT__BUCKET`.
    pub bucket: Option<String>,
    /// NAR compression codec (`zstd`/`xz`/`none`). Default `zstd`. Set
    /// via `RIO_BINARY_CACHE_COMPAT__COMPRESSION`.
    pub compression: CompatCompression,
    /// When the compat write happens relative to the PutPath PG
    /// commit. Only `sync_after_commit` exists today (see
    /// [`CompatWriteMode`]).
    pub write_mode: CompatWriteMode,
    /// Poll cadence (seconds) of the compat reconciler — the
    /// background loop that finds committed paths whose
    /// `narinfo.compat_file_hash` is NULL (paths committed via
    /// `PutPathChunked` or upstream substitution, paths ingested while
    /// compat was OFF, and inline writes that failed), reassembles
    /// their NARs from the chunk store, and publishes the compat
    /// object pair off the upload hot path. Within one tick the
    /// backlog drains continuously (batch after batch); the interval
    /// is the idle re-poll cadence. `0` disables the reconciler
    /// (inline writes still happen); ignored entirely when `enabled`
    /// is false. Default 30. Set via
    /// `RIO_BINARY_CACHE_COMPAT__RECONCILE_INTERVAL_SECS`.
    pub reconcile_interval_secs: u64,
}

impl Default for BinaryCacheCompat {
    fn default() -> Self {
        Self {
            // ON by default per ADR-022 §10: compat is the migration
            // on-ramp and the PG-outage substitution floor. Operators
            // opt OUT once all consumers are rio-aware.
            enabled: true,
            bucket: None,
            compression: CompatCompression::Zstd,
            write_mode: CompatWriteMode::SyncAfterCommit,
            // 30s idle re-poll matches the plan's "sleep 30s if empty"
            // and the GC-drain cadence; the steady-state poll is an
            // index-only probe of narinfo_compat_pending_idx (M_066).
            reconcile_interval_secs: 30,
        }
    }
}

// r[impl store.netpol.egress+2]
// Egress targets are exactly what's configured here: postgres
// (`database_url`) and the chunk backend (S3 or filesystem). The
// `store-egress` CiliumNetworkPolicy in infra/helm/rio-build/templates/
// networkpolicy.yaml allows CoreDNS + postgres:5432 (toEndpoints +
// postgresCidr) + S3-VPC-endpoint:443 only — tracey doesn't scan YAML;
// this Config is the scannable anchor.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct Config {
    /// gRPC listen address.
    pub listen_addr: std::net::SocketAddr,
    /// PostgreSQL connection URL. Required.
    pub database_url: String,
    #[serde(flatten)]
    pub common: rio_common::config::CommonConfig,
    /// Where chunks live. **Required** — every NAR is chunked, so a
    /// store without a backend cannot accept or serve anything.
    /// `validate()` rejects `None` at startup with the list of valid
    /// kinds. See [`ChunkBackendKind`] for TOML syntax.
    pub chunk_backend: Option<ChunkBackendKind>,
    /// Availability-zone NAME this replica's pod landed in (the value
    /// of the `topology.kubernetes.io/zone` label, e.g. `us-east-2a`).
    /// Consumed only by per-pod Express bucket selection for the
    /// `tiered` chunk backend ([`Config::resolve_express_bucket`]).
    /// In Kubernetes, helm renders this from the POD's own
    /// `topology.kubernetes.io/zone` label via the downward API —
    /// KEP-4742's `PodTopologyLabelsAdmission` plugin (beta in
    /// Kubernetes 1.35) copies the label from the node onto the pod at
    /// binding, so no IMDS call and no `nodes/get` RBAC is needed.
    /// Unset or empty (label absent — admission plugin disabled) means
    /// "zone unknown": no bucket is selected and the tiered backend
    /// runs with `local = None`. Set via `RIO_NODE_ZONE`.
    pub node_zone: Option<String>,
    /// Stock-Nix binary-cache compatibility layer (ADR-022 §10):
    /// also write `.narinfo` + `nar/*.nar.zst` objects to S3-standard
    /// so plain `nix` clients can substitute from the bucket without
    /// rio. Default ON. See [`BinaryCacheCompat`].
    pub binary_cache_compat: BinaryCacheCompat,
    /// moka LRU capacity for chunk reads, in bytes. Default 2 GiB.
    /// One cache shared by StoreService + ChunkService — a chunk
    /// warmed by either is hot for both.
    pub chunk_cache_capacity_bytes: u64,
    /// Global NAR reassembly buffer budget in bytes — total permits
    /// across ALL concurrent PutPath handlers. Each handler acquires
    /// `chunk.len()` permits before extending its accumulation Vec.
    /// None → DEFAULT_NAR_BUDGET (8 × MAX_NAR_SIZE = 32 GiB). Must be
    /// ≥ MAX_NAR_SIZE (4 GiB) — a smaller budget deadlocks any caller
    /// charging a NAR larger than the budget. Lower toward MAX_NAR_SIZE
    /// on small-memory nodes (concurrency drops to 1); raise it if you
    /// have >8 concurrent max-size uploads and RAM to match.
    ///
    /// NOT the only bound on resident NAR buffers: eager indexing
    /// (`nar_index_concurrency`) holds up to `nar_index_concurrency ×
    /// MAX_NAR_SIZE` (16 GiB at defaults) of upload buffers *past* the
    /// handler's lifetime, outside this budget's accounting. Size node
    /// memory for the sum of both.
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
    /// service-token bypass unavailable; gateway PutPath rejected
    /// unless `hmac_key_path` is also unset (full dev-mode). Set via
    /// `RIO_SERVICE_HMAC_KEY_PATH`.
    pub service_hmac_key_path: Option<PathBuf>,
    /// `ServiceClaims.caller` values whose `x-rio-service-token` is
    /// honoured (PutPath HMAC-bypass and `x-rio-probe-tenant-id` gate).
    /// Default: `["rio-gateway", "rio-scheduler"]`.
    pub service_bypass_callers: Vec<String>,
    /// Max concurrent S3 chunk uploads per `put_chunked` call.
    /// Default 8. Per-replica `r[store.substitute.admission]` bounds
    /// concurrent `put_chunked` calls; `substitute_admission_permits
    /// × this` is the per-replica in-flight PutObject ceiling. Raise
    /// if the store runs with a larger aws-sdk pool; lower (min 1)
    /// if you see `DispatchFailure` in store logs during large-NAR
    /// ingest. Set via `RIO_CHUNK_UPLOAD_MAX_CONCURRENT`.
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
    /// `GetPath` chunk-prefetch depth (`.buffered(K)`). Cold-cache
    /// throughput ceiling is `K × CHUNK_AVG / s3_ttfb`; per-stream
    /// memory cost is `K × CHUNK_MAX` (≤ 16 MiB at 64). Default 64.
    /// Set via `RIO_CHUNK_PREFETCH_K`.
    pub chunk_prefetch_k: usize,
    /// Max time to wait for in-flight `GetPath` body streams to
    /// complete after the drain-grace sleep, before tearing down the
    /// listener on SIGTERM. `terminationGracePeriodSeconds` MUST cover
    /// `drain_grace + stream_drain` + slack, or kubelet SIGKILLs
    /// mid-wait. Default 90 s. Set via `RIO_STREAM_DRAIN_SECS`.
    #[serde(rename = "stream_drain_secs", with = "rio_common::config::secs")]
    #[schemars(with = "u64")]
    pub stream_drain: std::time::Duration,
    /// PG connection pool size. Default 50 (was hardcoded 20). The
    /// QueryPathInfo / FindMissingPaths hot path under autoscaled
    /// builder load (60+ builders × ~100 input paths each at fan-out)
    /// is bottlenecked on `sqlx::pool::acquire`, not query latency
    /// (PK lookups). Aurora handles hundreds of connections per
    /// instance; raise this with `replicas` for thousands-of-builds
    /// scale. Set via `RIO_PG_MAX_CONNECTIONS`.
    pub pg_max_connections: u32,
    /// Per-replica cap on concurrent `try_substitute` calls. Excess
    /// queue server-side up to `SUBSTITUTE_ADMISSION_WAIT` (25 s),
    /// then `RESOURCE_EXHAUSTED` (transient; client retries). Additive
    /// to `nar_buffer_budget_bytes` — this bounds COUNT, that bounds
    /// BYTES. `None` (default) derives from the PG pool via
    /// [`derive_substitute_admission_cap`]: `(pg_max × 3).clamp(64,
    /// 128)`. Env: `RIO_SUBSTITUTE_ADMISSION_PERMITS`.
    #[serde(default)]
    pub substitute_admission_permits: Option<usize>,
    /// Max concurrent eager NAR-index computations spawned by the
    /// legacy `PutPath`/`PutPathBatch` paths (P0557). After a
    /// successful upload the handler `try_acquire`s a permit; if one
    /// is free it spawns `nar_ls` over the still-in-RAM NAR so
    /// `GetNarIndex`/`GetDirectory` see the path immediately. If not,
    /// the path falls back to the `indexer_loop` (≤5 s pickup) — no
    /// queueing, no added upload latency. Each in-flight computation
    /// holds its NAR buffer past the handler's lifetime (outside
    /// `nar_buffer_budget_bytes`), so this also bounds that extra RSS
    /// to `nar_index_concurrency × NAR size`. `0` disables eager
    /// indexing entirely (everything defers to the `indexer_loop`).
    /// Default 4. Set via `RIO_NAR_INDEX_CONCURRENCY`.
    pub nar_index_concurrency: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_addr: rio_common::default_addr(9002),
            database_url: String::new(),
            common: rio_common::config::CommonConfig::new(9092),
            // Required field: None here so an unconfigured store fails
            // validate() with a clear error instead of silently picking
            // a backend.
            chunk_backend: None,
            // Zone unknown unless the deployment provides it (helm
            // downward-API env). Outside K8s there is nothing to read.
            node_zone: None,
            binary_cache_compat: BinaryCacheCompat::default(),
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
            chunk_upload_max_concurrent: crate::cas::DEFAULT_CHUNK_UPLOAD_CONCURRENCY,
            s3_max_attempts: DEFAULT_S3_MAX_ATTEMPTS,
            max_batch_paths: crate::grpc::DEFAULT_MAX_BATCH_PATHS,
            chunk_prefetch_k: crate::grpc::DEFAULT_CHUNK_PREFETCH_K,
            stream_drain: std::time::Duration::from_secs(90),
            pg_max_connections: DEFAULT_PG_MAX_CONNECTIONS,
            substitute_admission_permits: None,
            nar_index_concurrency: crate::nar_index::DEFAULT_NAR_INDEX_CONCURRENCY,
        }
    }
}

/// Default PG pool size. Raised from sqlx's 10 (and the prior hardcoded
/// 20) after I-076: 60 autoscaled builders at hello-shallow fan-out
/// drove `acquired_after_secs=16` on QueryPathInfo. The query is a PK
/// lookup; the bottleneck is connection acquisition.
pub const DEFAULT_PG_MAX_CONNECTIONS: u32 = 50;

/// Derive the default `substitute_admission_permits` from the PG pool
/// size. Spike 0.2 verified the substitute path holds a PG connection
/// only per-query (via `&PgPool`), never across the upstream HTTP/NAR
/// fetch — so admitted callers >> `pg_max` doesn't starve the pool. 3×
/// gives headroom for the (typical) case where most admitted calls are
/// parked on upstream I/O; the `[64, 128]` clamp keeps tiny dev pools
/// from throttling to single digits and huge pools from admitting
/// unbounded HTTP fan-out. 128 (not 256) keeps the per-replica S3
/// fan-out self-consistent: 128 admitted × `S3_PUT_CONCURRENCY` (8) =
/// 1024, the S3 single-prefix steady-state ceiling.
pub fn derive_substitute_admission_cap(pg_max: u32) -> usize {
    (pg_max as usize * 3).clamp(64, 128)
}

#[derive(Parser, Serialize, Default)]
#[command(
    name = "rio-store",
    about = "NAR content-addressable store for rio-build"
)]
pub struct CliArgs {
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
        use rio_common::limits::MAX_NAR_SIZE;
        required(&self.database_url, "database_url", "store")?;
        // Every NAR is chunked (P0583 dropped inline_blob storage), so
        // a store without a chunk backend cannot store or serve
        // anything. Reject at boot, not at the first PutPath.
        anyhow::ensure!(
            self.chunk_backend.is_some(),
            "chunk_backend is required: set [chunk_backend] kind to one of \
             `filesystem`, `s3`, `tiered`, or `memory` (tests only) — \
             e.g. RIO_CHUNK_BACKEND__KIND=s3 RIO_CHUNK_BACKEND__BUCKET=..."
        );
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
        // < MAX_NAR_SIZE → `acquire_many(n)` for a NAR sized in
        // (budget, MAX_NAR_SIZE] parks forever (not error) and
        // FIFO-blocks everything queued behind it. `limits.rs`
        // documents budget >= MAX_NAR_SIZE as the no-self-deadlock
        // invariant; enforce it at boot, not at the first 4 GiB NAR.
        anyhow::ensure!(
            self.nar_buffer_budget_bytes
                .is_none_or(|b| b >= MAX_NAR_SIZE),
            "nar_buffer_budget_bytes must be >= MAX_NAR_SIZE ({MAX_NAR_SIZE}) \
             (smaller deadlocks uploads); unset RIO_NAR_BUFFER_BUDGET_BYTES \
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
        // 0 → Semaphore::new(0) → every try_substitute_on_miss queues
        // for SUBSTITUTE_ADMISSION_WAIT then ResourceExhausted; store
        // never substitutes. None is fine (derived from pg_max).
        anyhow::ensure!(
            self.substitute_admission_permits.is_none_or(|n| n >= 1),
            "substitute_admission_permits must be >= 1; unset \
             RIO_SUBSTITUTE_ADMISSION_PERMITS to derive from pg_max_connections"
        );
        // Express eviction-sweep watermarks: a NaN/negative factor or an
        // inverted pair (low > high) makes the sweeper silently never
        // evict or thrash list+delete every tick — degenerate states an
        // operator only notices when the bucket blows past its budget.
        // Reject at boot. Only reachable on the tiered variant (the only
        // place the sweep config exists).
        if let Some(ChunkBackendKind::Tiered {
            express,
            express_bucket_by_zone,
            ..
        }) = &self.chunk_backend
        {
            // Per-pod selection map: an empty zone key can never match a
            // real zone label, and an empty bucket value would (when this
            // pod's zone selects it) build an S3 client against a bucket
            // literally named "" — both are templating-layer accidents
            // (helm/xtask render real names or omit the entry). Reject at
            // boot, not at the first chunk read in that one AZ.
            anyhow::ensure!(
                express_bucket_by_zone
                    .iter()
                    .all(|(zone, bucket)| !zone.trim().is_empty() && !bucket.trim().is_empty()),
                "chunk_backend.express_bucket_by_zone must not contain empty zone keys or \
                 empty bucket names; fix the map passed via \
                 RIO_CHUNK_BACKEND__EXPRESS_BUCKET_BY_ZONE (or drop the entry)"
            );
            anyhow::ensure!(
                express.target_bytes >= 1,
                "chunk_backend.express.target_bytes must be >= 1; \
                 set RIO_CHUNK_BACKEND__EXPRESS__TARGET_BYTES"
            );
            anyhow::ensure!(
                express.evict_high_watermark.is_finite()
                    && express.evict_low_watermark.is_finite()
                    && express.evict_high_watermark > 0.0
                    && express.evict_low_watermark > 0.0,
                "chunk_backend.express.evict_{{high,low}}_watermark must be finite and > 0; \
                 set RIO_CHUNK_BACKEND__EXPRESS__EVICT_HIGH_WATERMARK / \
                 RIO_CHUNK_BACKEND__EXPRESS__EVICT_LOW_WATERMARK"
            );
            anyhow::ensure!(
                express.evict_low_watermark <= express.evict_high_watermark,
                "chunk_backend.express.evict_low_watermark ({}) must be <= \
                 evict_high_watermark ({}) — an inverted pair makes every sweep \
                 trigger and immediately stop",
                express.evict_low_watermark,
                express.evict_high_watermark
            );
        }
        // binary_cache_compat.bucket: absent means "use the chunk
        // backend's S3-standard bucket"; present must be a real bucket
        // name. Some("") (e.g. a templating layer rendering an empty
        // value instead of omitting the env var) would point the compat
        // writer at a bucket literally named "" — reject at boot, not
        // at the first compat write. `enabled` itself needs no check:
        // with `bucket` unset the writer publishes into whatever chunk
        // backend is configured (for filesystem/memory that is its
        // `blobs/` namespace — dev parity, readable as a `file://`
        // binary cache), and a dedicated S3 bucket is only built when
        // `bucket` is set.
        anyhow::ensure!(
            self.binary_cache_compat
                .bucket
                .as_ref()
                .is_none_or(|b| !b.trim().is_empty()),
            "binary_cache_compat.bucket must be a non-empty bucket name when set; \
             unset RIO_BINARY_CACHE_COMPAT__BUCKET to use the chunk backend's bucket"
        );
        Ok(())
    }
}

rio_common::impl_has_common_config!(Config);

impl Config {
    /// Apply per-pod Express bucket selection (P0554) to the loaded
    /// config, in place. Call once at startup, after config load /
    /// validation and BEFORE [`init_chunk_backend`] and the
    /// express-sweep spawn — both read the resolved `express_bucket`.
    ///
    /// Only the `tiered` backend is affected; other kinds are a no-op.
    /// Exactly one log line states the outcome, and when no bucket is
    /// selected it names the missing piece (empty map / unknown zone /
    /// unmapped zone) so an operator can tell from the startup log why
    /// a replica runs without its cache tier.
    pub fn resolve_express_bucket(&mut self) {
        let node_zone = self.node_zone.as_deref();
        let Some(ChunkBackendKind::Tiered {
            express_bucket,
            express_bucket_by_zone,
            ..
        }) = &mut self.chunk_backend
        else {
            return;
        };
        match select_express_bucket(express_bucket.as_deref(), express_bucket_by_zone, node_zone) {
            ExpressSelection::Explicit(bucket) => {
                info!(
                    %bucket,
                    "express cache tier: using explicitly configured express_bucket"
                );
            }
            ExpressSelection::Selected { zone, bucket } => {
                info!(
                    %zone,
                    %bucket,
                    "express cache tier: selected this pod's bucket from express_bucket_by_zone"
                );
                *express_bucket = Some(bucket);
            }
            ExpressSelection::Disabled { reason } => {
                info!(
                    %reason,
                    "express cache tier disabled for this replica (local=None, direct \
                     S3-standard reads)"
                );
                // Normalize a whitespace-only explicit value to None so
                // init_chunk_backend never builds a client for bucket "".
                *express_bucket = None;
            }
        }
    }
}

/// Construct the chunk backend + ONE shared `ChunkCache`.
///
/// The cache Arc is cloned into both consumers (`StoreServiceImpl`,
/// `ChunkServiceImpl`) — a chunk warmed by GetPath is hot for
/// GetChunk.
///
/// `?` on backend construction: filesystem mkdir fail or S3
/// bad-region means we can't store chunks — startup error, not
/// degraded mode.
pub async fn init_chunk_backend(
    kind: &ChunkBackendKind,
    cache_capacity_bytes: u64,
    s3_max_attempts: u32,
) -> anyhow::Result<Arc<ChunkCache>> {
    Ok(match kind {
        ChunkBackendKind::Memory => {
            info!("chunk backend: memory (in-process, test/dev only — chunks do not persist)");
            let backend: Arc<dyn ChunkBackend> = Arc::new(MemoryChunkBackend::new());
            Arc::new(ChunkCache::with_capacity(backend, cache_capacity_bytes))
        }
        ChunkBackendKind::Filesystem { base_dir } => {
            info!(base_dir = %base_dir.display(), "chunk backend: filesystem");
            // Eagerly creates the 256-subdir fanout. `?` — if the
            // disk is read-only or the path is garbage, better to
            // fail here than on the first PutPath with a cryptic
            // ENOENT deep in the put() call.
            let backend: Arc<dyn ChunkBackend> = Arc::new(FilesystemChunkBackend::new(base_dir)?);
            Arc::new(ChunkCache::with_capacity(backend, cache_capacity_bytes))
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
            Arc::new(ChunkCache::with_capacity(backend, cache_capacity_bytes))
        }
        ChunkBackendKind::Tiered {
            bucket,
            prefix,
            express_bucket,
            // Already folded into `express_bucket` by
            // `Config::resolve_express_bucket` (main.rs runs it before
            // this) — the raw map is not consulted here.
            express_bucket_by_zone: _,
            // Sweep tuning is consumed by `backend::express_sweep` (spawned
            // from main.rs), not by the read/write backend built here.
            express: _,
        } => {
            info!(
                %bucket,
                %prefix,
                express_bucket = express_bucket.as_deref().unwrap_or("<none>"),
                s3_max_attempts,
                "chunk backend: tiered (S3 standard + per-AZ Express cache)"
            );
            // Different retry budgets per tier. Remote is authoritative
            // — a failed read there means "data unreachable", worth the
            // full `s3_max_attempts` (default 10). Express is a
            // best-effort cache that must fall through quickly on
            // throttle/5xx; 2 attempts cover a transient connection
            // reset, anything worse shows up in
            // `rio_store_tiered_local_errors_total`. Both clients share
            // the env credential/region chain; the SDK routes Express
            // traffic by the `--x-s3` bucket-name suffix.
            const EXPRESS_MAX_ATTEMPTS: u32 = 2;
            let remote_client = rio_common::s3::default_client(s3_max_attempts).await;
            let remote = S3ChunkBackend::new(remote_client, bucket.clone(), prefix.clone());
            let local = match express_bucket {
                Some(b) => {
                    let local_client = rio_common::s3::default_client(EXPRESS_MAX_ATTEMPTS).await;
                    Some(S3ChunkBackend::new(local_client, b.clone(), prefix.clone()))
                }
                None => None,
            };
            let backend: Arc<dyn ChunkBackend> = Arc::new(TieredChunkBackend::new(local, remote));
            Arc::new(ChunkCache::with_capacity(backend, cache_capacity_bytes))
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
        // Chunk backend is required — no default. validate() rejects None.
        assert!(d.chunk_backend.is_none());
        // Zone unknown unless the deployment provides RIO_NODE_ZONE
        // (helm downward-API pod label). None → no per-pod Express
        // bucket selection.
        assert!(d.node_zone.is_none());
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
        assert_eq!(d.max_batch_paths, crate::grpc::DEFAULT_MAX_BATCH_PATHS);
        // r[verify store.get.chunk-prefetch]
        assert_eq!(d.chunk_prefetch_k, 64);
        assert_eq!(d.stream_drain, std::time::Duration::from_secs(90));
        assert_eq!(d.pg_max_connections, DEFAULT_PG_MAX_CONNECTIONS);
        // None → main.rs derives via derive_substitute_admission_cap.
        assert!(d.substitute_admission_permits.is_none());
        // 4 eager nar_ls passes in flight; 0 would silently disable
        // the optimization, larger risks unbounded post-handler RSS.
        assert_eq!(d.nar_index_concurrency, 4);
        // r[verify store.compat.runtime-toggle]
        // Binary-cache compat defaults ON (ADR-022 §10 migration
        // posture), no dedicated bucket (→ chunk backend's bucket),
        // zstd, synchronous post-commit write, 30s reconciler idle
        // poll. Changing any of these changes what a bare deployment
        // publishes to S3 — deliberate only.
        assert!(d.binary_cache_compat.enabled);
        assert!(d.binary_cache_compat.bucket.is_none());
        assert_eq!(d.binary_cache_compat.compression, CompatCompression::Zstd);
        assert_eq!(
            d.binary_cache_compat.write_mode,
            CompatWriteMode::SyncAfterCommit
        );
        assert_eq!(d.binary_cache_compat.reconcile_interval_secs, 30);
    }

    #[test]
    fn derive_admission_cap_clamps() {
        // (pg_max × 3).clamp(64, 128). DEFAULT_PG_MAX_CONNECTIONS=50
        // → 150 → ceil-clamped to 128.
        assert_eq!(derive_substitute_admission_cap(50), 128);
        // Floor: tiny dev pool doesn't throttle to single digits.
        assert_eq!(derive_substitute_admission_cap(1), 64);
        assert_eq!(derive_substitute_admission_cap(21), 64);
        // Ceiling: huge pool doesn't admit unbounded HTTP fan-out.
        // 128 × S3_PUT_CONCURRENCY(8) = 1024 = S3 prefix ceiling.
        assert_eq!(derive_substitute_admission_cap(100), 128);
        assert_eq!(derive_substitute_admission_cap(10_000), 128);
    }

    /// `chunk_upload_max_concurrent=0` → `buffer_unordered(0)` hangs
    /// every put_chunked permanently. validate() must reject at
    /// startup, not silently hang the data plane.
    #[test]
    fn validate_rejects_zero_upload_concurrency() {
        let cfg = Config {
            database_url: "postgres://x".into(),
            chunk_backend: Some(ChunkBackendKind::Memory),
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
            chunk_backend: Some(ChunkBackendKind::Memory),
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
            chunk_backend: Some(ChunkBackendKind::Memory),
            nar_buffer_budget_bytes: Some(0),
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("nar_buffer_budget_bytes"), "got: {err}");
    }

    /// Any budget < MAX_NAR_SIZE makes `acquire_many(total)` for a
    /// legal NAR sized in (budget, MAX_NAR_SIZE] park forever.
    #[test]
    fn validate_rejects_budget_below_max_nar_size() {
        use rio_common::limits::MAX_NAR_SIZE;
        for bad in [100u64, MAX_NAR_SIZE - 1] {
            let cfg = Config {
                database_url: "postgres://x".into(),
                chunk_backend: Some(ChunkBackendKind::Memory),
                nar_buffer_budget_bytes: Some(bad),
                ..Default::default()
            };
            let err = cfg.validate().unwrap_err().to_string();
            assert!(err.contains("nar_buffer_budget_bytes"), "got: {err}");
        }
        // Exactly MAX_NAR_SIZE is the floor (concurrency = 1).
        let at_floor = Config {
            database_url: "postgres://x".into(),
            chunk_backend: Some(ChunkBackendKind::Memory),
            nar_buffer_budget_bytes: Some(MAX_NAR_SIZE),
            ..Default::default()
        };
        assert!(at_floor.validate().is_ok());
        // None (unset) is fine — that's the 32 GiB default.
        let ok = Config {
            database_url: "postgres://x".into(),
            chunk_backend: Some(ChunkBackendKind::Memory),
            nar_buffer_budget_bytes: None,
            ..Default::default()
        };
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn validate_rejects_zero_pg_connections() {
        let cfg = Config {
            database_url: "postgres://x".into(),
            chunk_backend: Some(ChunkBackendKind::Memory),
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
            chunk_backend: Some(ChunkBackendKind::Memory),
            max_batch_paths: 0,
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("max_batch_paths"), "got: {err}");
    }

    /// `substitute_admission_permits=Some(0)` → `Semaphore::new(0)` →
    /// every `try_substitute_on_miss` queues for the full wait then
    /// returns `ResourceExhausted`; store silently never substitutes.
    #[test]
    fn validate_rejects_zero_admission_permits() {
        let cfg = Config {
            database_url: "postgres://x".into(),
            chunk_backend: Some(ChunkBackendKind::Memory),
            substitute_admission_permits: Some(0),
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("substitute_admission_permits"), "got: {err}");
        // None (unset) is fine — derived from pg_max_connections.
        let ok = Config {
            database_url: "postgres://x".into(),
            chunk_backend: Some(ChunkBackendKind::Memory),
            substitute_admission_permits: None,
            ..Default::default()
        };
        assert!(ok.validate().is_ok());
    }

    /// `binary_cache_compat.bucket = Some("")` (a templating layer
    /// rendering an empty value instead of omitting the env var) would
    /// point the compat writer at a bucket literally named "".
    /// validate() rejects it at boot; `None` and a real name both pass.
    #[test]
    fn validate_rejects_empty_compat_bucket() {
        for bad in ["", "   "] {
            let cfg = Config {
                database_url: "postgres://x".into(),
                chunk_backend: Some(ChunkBackendKind::Memory),
                binary_cache_compat: BinaryCacheCompat {
                    bucket: Some(bad.into()),
                    ..BinaryCacheCompat::default()
                },
                ..Default::default()
            };
            let err = cfg.validate().unwrap_err().to_string();
            assert!(err.contains("binary_cache_compat.bucket"), "got: {err}");
        }
        // A real bucket name passes; the default (None → chunk backend
        // bucket) is covered by config_accepts_valid below.
        let ok = Config {
            database_url: "postgres://x".into(),
            chunk_backend: Some(ChunkBackendKind::Memory),
            binary_cache_compat: BinaryCacheCompat {
                bucket: Some("nix-cache".into()),
                ..BinaryCacheCompat::default()
            },
            ..Default::default()
        };
        assert!(ok.validate().is_ok());
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

    /// TOML parsing for the tagged enum via the config crate (what
    /// main.rs actually uses via rio_common::config::load). The `kind`
    /// tag + lowercase variant names are load-bearing — the NixOS
    /// module writes TOML with these exact strings. A silent rename
    /// would break every deployment with chunk_backend configured.
    ///
    /// Testing via the config crate (not raw toml crate) catches
    /// loader-specific deserialization quirks (the Value layer the
    /// config crate deserializes through). Tagged-enum handling in
    /// that layer is a known past pain point.
    fn parse_toml(s: &str) -> Config {
        ::config::Config::builder()
            .add_source(::config::File::from_str(s, ::config::FileFormat::Toml))
            .build()
            .unwrap()
            .try_deserialize()
            .unwrap()
    }

    #[test]
    fn chunk_backend_kind_toml_memory() {
        let cfg = parse_toml(
            r#"
            [chunk_backend]
            kind = "memory"
            "#,
        );
        assert!(matches!(cfg.chunk_backend, Some(ChunkBackendKind::Memory)));
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
            Some(ChunkBackendKind::Filesystem { base_dir }) => {
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
            Some(ChunkBackendKind::S3 { bucket, prefix }) => {
                assert_eq!(bucket, "my-nar-chunks");
                assert_eq!(prefix, "prod/");
            }
            other => panic!("expected S3, got {other:?}"),
        }
    }

    #[test]
    fn chunk_backend_kind_toml_tiered() {
        let cfg = parse_toml(
            r#"
            [chunk_backend]
            kind = "tiered"
            bucket = "rio-chunks"
            prefix = "prod"
            express_bucket = "rio-chunk-cache--use1-az4--x-s3"
            "#,
        );
        match cfg.chunk_backend {
            Some(ChunkBackendKind::Tiered {
                bucket,
                prefix,
                express_bucket,
                express_bucket_by_zone,
                express,
            }) => {
                assert_eq!(bucket, "rio-chunks");
                assert_eq!(prefix, "prod");
                assert_eq!(
                    express_bucket.as_deref(),
                    Some("rio-chunk-cache--use1-az4--x-s3")
                );
                // No [chunk_backend.express_bucket_by_zone] table →
                // empty map (no per-pod selection).
                assert!(express_bucket_by_zone.is_empty());
                // No [chunk_backend.express] table → compiled defaults
                // (8 TiB target, 1.10/0.90 watermarks, hourly sweep).
                assert_eq!(express, ExpressConfig::default());
            }
            other => panic!("expected Tiered, got {other:?}"),
        }
    }

    /// `[chunk_backend.express]` table threads through the config-crate
    /// Value layer into the variant's nested struct, with unspecified
    /// fields falling back to the struct-level `#[serde(default)]`.
    #[test]
    fn chunk_backend_kind_toml_tiered_express_overrides() {
        let cfg = parse_toml(
            r#"
            [chunk_backend]
            kind = "tiered"
            bucket = "rio-chunks"
            prefix = ""
            express_bucket = "rio-chunk-cache--use2-az1--x-s3"

            [chunk_backend.express]
            target_bytes = 1099511627776
            evict_high_watermark = 1.25
            sweep_interval_secs = 600
            "#,
        );
        match cfg.chunk_backend {
            Some(ChunkBackendKind::Tiered { express, .. }) => {
                assert_eq!(express.target_bytes, 1_099_511_627_776);
                assert_eq!(express.evict_high_watermark, 1.25);
                // Unspecified → default.
                assert_eq!(express.evict_low_watermark, 0.90);
                assert_eq!(express.sweep_interval_secs, 600);
            }
            other => panic!("expected Tiered, got {other:?}"),
        }
    }

    /// ExpressConfig defaults are the spec'd values (design overview §9):
    /// 8 TiB target, 1.10 / 0.90 watermarks, hourly sweep. Changing any
    /// of these changes how much S3 Express a bare tiered deployment
    /// retains — deliberate only.
    #[test]
    fn express_config_defaults_are_stable() {
        let d = ExpressConfig::default();
        assert_eq!(d.target_bytes, 8_796_093_022_208);
        assert_eq!(d.evict_high_watermark, 1.10);
        assert_eq!(d.evict_low_watermark, 0.90);
        assert_eq!(d.sweep_interval_secs, 3600);
    }

    /// Inverted watermarks (low > high) are rejected at boot — the sweep
    /// would otherwise trigger and immediately stop on every tick.
    #[test]
    fn validate_rejects_inverted_express_watermarks() {
        let cfg = Config {
            database_url: "postgres://x".into(),
            chunk_backend: Some(ChunkBackendKind::Tiered {
                bucket: "rio-chunks".into(),
                prefix: String::new(),
                express_bucket: Some("rio-chunk-cache--use2-az1--x-s3".into()),
                express_bucket_by_zone: BTreeMap::new(),
                express: ExpressConfig {
                    evict_high_watermark: 0.8,
                    evict_low_watermark: 1.2,
                    ..ExpressConfig::default()
                },
            }),
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("evict_low_watermark"), "got: {err}");

        // NaN / zero factors also rejected.
        for (high, low) in [(f64::NAN, 0.9), (1.1, 0.0)] {
            let cfg = Config {
                database_url: "postgres://x".into(),
                chunk_backend: Some(ChunkBackendKind::Tiered {
                    bucket: "rio-chunks".into(),
                    prefix: String::new(),
                    express_bucket: None,
                    express_bucket_by_zone: BTreeMap::new(),
                    express: ExpressConfig {
                        evict_high_watermark: high,
                        evict_low_watermark: low,
                        ..ExpressConfig::default()
                    },
                }),
                ..Default::default()
            };
            let err = cfg.validate().unwrap_err().to_string();
            assert!(err.contains("watermark"), "got: {err}");
        }

        // The defaults pass.
        let ok = Config {
            database_url: "postgres://x".into(),
            chunk_backend: Some(ChunkBackendKind::Tiered {
                bucket: "rio-chunks".into(),
                prefix: String::new(),
                express_bucket: Some("rio-chunk-cache--use2-az1--x-s3".into()),
                express_bucket_by_zone: BTreeMap::new(),
                express: ExpressConfig::default(),
            }),
            ..Default::default()
        };
        assert!(ok.validate().is_ok());
    }

    /// Empty zone keys / empty bucket values in the per-pod selection
    /// map are templating accidents that would either never match or
    /// build an S3 client against a bucket named "" — rejected at boot.
    #[test]
    fn validate_rejects_empty_zone_map_entries() {
        for (zone, bucket) in [
            ("", "rio-build-chunk-cache--use2-az1--x-s3"),
            ("us-east-2a", " "),
        ] {
            let cfg = Config {
                database_url: "postgres://x".into(),
                chunk_backend: Some(ChunkBackendKind::Tiered {
                    bucket: "rio-chunks".into(),
                    prefix: String::new(),
                    express_bucket: None,
                    express_bucket_by_zone: BTreeMap::from([(
                        zone.to_string(),
                        bucket.to_string(),
                    )]),
                    express: ExpressConfig::default(),
                }),
                ..Default::default()
            };
            let err = cfg.validate().unwrap_err().to_string();
            assert!(err.contains("express_bucket_by_zone"), "got: {err}");
        }
        // A well-formed map passes.
        let ok = Config {
            database_url: "postgres://x".into(),
            chunk_backend: Some(ChunkBackendKind::Tiered {
                bucket: "rio-chunks".into(),
                prefix: String::new(),
                express_bucket: None,
                express_bucket_by_zone: BTreeMap::from([(
                    "us-east-2a".to_string(),
                    "rio-build-chunk-cache--use2-az1--x-s3".to_string(),
                )]),
                express: ExpressConfig::default(),
            }),
            ..Default::default()
        };
        assert!(ok.validate().is_ok());
    }

    /// `express_bucket` omitted → `None`. A replica scheduled in an AZ
    /// without S3 Express runs degraded (S3-standard only, functional);
    /// helm omits the key rather than supplying an empty string.
    #[test]
    fn chunk_backend_kind_toml_tiered_no_express() {
        let cfg = parse_toml(
            r#"
            [chunk_backend]
            kind = "tiered"
            bucket = "rio-chunks"
            prefix = ""
            "#,
        );
        match cfg.chunk_backend {
            Some(ChunkBackendKind::Tiered { express_bucket, .. }) => {
                assert!(express_bucket.is_none());
            }
            other => panic!("expected Tiered, got {other:?}"),
        }
    }

    /// No [chunk_backend] section at all → `None`, which `validate()`
    /// rejects with an error naming the valid kinds. The backend is a
    /// required field — there is no implicit default.
    #[test]
    fn chunk_backend_kind_absent_is_rejected() {
        let cfg = parse_toml(
            r#"
            listen_addr = "0.0.0.0:9002"
            database_url = "postgres://x"
            "#,
        );
        assert!(cfg.chunk_backend.is_none());
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("chunk_backend"), "names the field: {err}");
        for kind in ["filesystem", "s3", "memory"] {
            assert!(err.contains(kind), "error must name `{kind}`: {err}");
        }
    }

    /// Env-var tagged-enum parsing via the real rio_common::config::load
    /// path (compiled defaults → `RIO_`-prefixed env with `__` nesting →
    /// deserialize). The deploy overlays set chunk_backend this way —
    /// regression guard for kustomization.yaml.
    ///
    /// The defaults layer serializes the absent backend as null; the
    /// per-key merge must correctly replace it with
    /// {kind: "s3", bucket: ..., prefix: ...} from the env layer.
    /// Half-merges (stale kind, orphan fields) would fail tagged-enum
    /// deserialization.
    ///
    /// Jail: serializes env mutation under a global mutex.
    #[test]
    fn chunk_backend_kind_env_s3() {
        rio_test_support::Jail::expect_with(|jail| {
            jail.set_env("RIO_CHUNK_BACKEND__KIND", "s3");
            jail.set_env("RIO_CHUNK_BACKEND__BUCKET", "rio-chunks");
            jail.set_env("RIO_CHUNK_BACKEND__PREFIX", "");
            let cfg: Config = rio_common::config::load("store", CliArgs::default()).unwrap();
            match cfg.chunk_backend {
                Some(ChunkBackendKind::S3 { bucket, prefix }) => {
                    assert_eq!(bucket, "rio-chunks");
                    assert_eq!(prefix, "");
                }
                other => panic!("env vars must populate the backend; got {other:?}"),
            }
            Ok(())
        });
    }

    #[test]
    fn chunk_backend_kind_env_filesystem() {
        rio_test_support::Jail::expect_with(|jail| {
            jail.set_env("RIO_CHUNK_BACKEND__KIND", "filesystem");
            jail.set_env("RIO_CHUNK_BACKEND__BASE_DIR", "/var/lib/chunks");
            let cfg: Config = rio_common::config::load("store", CliArgs::default()).unwrap();
            match cfg.chunk_backend {
                Some(ChunkBackendKind::Filesystem { base_dir }) => {
                    assert_eq!(base_dir, PathBuf::from("/var/lib/chunks"));
                }
                other => panic!("expected Filesystem; got {other:?}"),
            }
            Ok(())
        });
    }

    /// P0554 wires `express_bucket` from a per-pod env var (downward-API
    /// `topology.kubernetes.io/zone` → AZ → bucket-name template). The
    /// `Option<String>` field must round-trip via the env layer.
    #[test]
    fn chunk_backend_kind_env_tiered() {
        rio_test_support::Jail::expect_with(|jail| {
            jail.set_env("RIO_CHUNK_BACKEND__KIND", "tiered");
            jail.set_env("RIO_CHUNK_BACKEND__BUCKET", "rio-chunks");
            jail.set_env("RIO_CHUNK_BACKEND__PREFIX", "");
            jail.set_env(
                "RIO_CHUNK_BACKEND__EXPRESS_BUCKET",
                "rio-chunk-cache--use1-az4--x-s3",
            );
            let cfg: Config = rio_common::config::load("store", CliArgs::default()).unwrap();
            match cfg.chunk_backend {
                Some(ChunkBackendKind::Tiered {
                    bucket,
                    express_bucket,
                    ..
                }) => {
                    assert_eq!(bucket, "rio-chunks");
                    assert_eq!(
                        express_bucket.as_deref(),
                        Some("rio-chunk-cache--use1-az4--x-s3")
                    );
                }
                other => panic!("expected Tiered; got {other:?}"),
            }
            Ok(())
        });
    }

    /// The express sweep knobs ride the same `__`-nested env path the
    /// rest of the tagged enum uses (`RIO_CHUNK_BACKEND__EXPRESS__*`),
    /// including numeric/float coercion via the env layer's
    /// `try_parsing`. Absent vars → compiled defaults.
    #[test]
    fn chunk_backend_kind_env_tiered_express_overrides() {
        rio_test_support::Jail::expect_with(|jail| {
            jail.set_env("RIO_CHUNK_BACKEND__KIND", "tiered");
            jail.set_env("RIO_CHUNK_BACKEND__BUCKET", "rio-chunks");
            jail.set_env("RIO_CHUNK_BACKEND__PREFIX", "");
            jail.set_env(
                "RIO_CHUNK_BACKEND__EXPRESS_BUCKET",
                "rio-chunk-cache--use2-az1--x-s3",
            );
            jail.set_env("RIO_CHUNK_BACKEND__EXPRESS__TARGET_BYTES", "1073741824");
            jail.set_env("RIO_CHUNK_BACKEND__EXPRESS__EVICT_LOW_WATERMARK", "0.5");
            jail.set_env("RIO_CHUNK_BACKEND__EXPRESS__SWEEP_INTERVAL_SECS", "0");
            let cfg: Config = rio_common::config::load("store", CliArgs::default()).unwrap();
            match cfg.chunk_backend {
                Some(ChunkBackendKind::Tiered { express, .. }) => {
                    assert_eq!(express.target_bytes, 1_073_741_824);
                    assert_eq!(express.evict_low_watermark, 0.5);
                    // 0 = sweeper disabled.
                    assert_eq!(express.sweep_interval_secs, 0);
                    // Untouched field keeps its default.
                    assert_eq!(express.evict_high_watermark, 1.10);
                }
                other => panic!("expected Tiered; got {other:?}"),
            }
            Ok(())
        });
    }

    /// `EXPRESS_BUCKET` env var omitted → `express_bucket: None`.
    /// Helm omits the var on AZs without S3 Express; the env layer must
    /// surface the absence as `None` (via `#[serde(default)]`) rather
    /// than failing tagged-enum deserialization on a missing key.
    #[test]
    fn chunk_backend_kind_env_tiered_no_express() {
        rio_test_support::Jail::expect_with(|jail| {
            jail.set_env("RIO_CHUNK_BACKEND__KIND", "tiered");
            jail.set_env("RIO_CHUNK_BACKEND__BUCKET", "rio-chunks");
            jail.set_env("RIO_CHUNK_BACKEND__PREFIX", "");
            let cfg: Config = rio_common::config::load("store", CliArgs::default()).unwrap();
            match cfg.chunk_backend {
                Some(ChunkBackendKind::Tiered { express_bucket, .. }) => {
                    assert!(
                        express_bucket.is_none(),
                        "absent EXPRESS_BUCKET env var must default to None"
                    );
                }
                other => panic!("expected Tiered; got {other:?}"),
            }
            Ok(())
        });
    }

    /// `[chunk_backend.express_bucket_by_zone]` as a real TOML table:
    /// the natural form for file-based config. Keys are zone NAMES
    /// (`topology.kubernetes.io/zone` values), values are directory
    /// bucket names.
    #[test]
    fn chunk_backend_kind_toml_tiered_zone_map() {
        let cfg = parse_toml(
            r#"
            node_zone = "us-east-2a"

            [chunk_backend]
            kind = "tiered"
            bucket = "rio-chunks"
            prefix = ""

            [chunk_backend.express_bucket_by_zone]
            "us-east-2a" = "rio-build-chunk-cache--use2-az1--x-s3"
            "us-east-2b" = "rio-build-chunk-cache--use2-az2--x-s3"
            "#,
        );
        assert_eq!(cfg.node_zone.as_deref(), Some("us-east-2a"));
        match cfg.chunk_backend {
            Some(ChunkBackendKind::Tiered {
                express_bucket,
                express_bucket_by_zone,
                ..
            }) => {
                assert!(express_bucket.is_none());
                assert_eq!(express_bucket_by_zone.len(), 2);
                assert_eq!(
                    express_bucket_by_zone.get("us-east-2a").map(String::as_str),
                    Some("rio-build-chunk-cache--use2-az1--x-s3")
                );
            }
            other => panic!("expected Tiered, got {other:?}"),
        }
    }

    /// P0554: the env layer carries the whole map as ONE JSON-object
    /// string (helm renders `RIO_CHUNK_BACKEND__EXPRESS_BUCKET_BY_ZONE`
    /// from the values map with `toJson`) and the pod's zone as
    /// `RIO_NODE_ZONE` (downward-API pod label). Both must round-trip
    /// through the real loader; the JSON string must come back as a
    /// typed map.
    #[test]
    fn chunk_backend_kind_env_tiered_zone_map_json() {
        rio_test_support::Jail::expect_with(|jail| {
            jail.set_env("RIO_CHUNK_BACKEND__KIND", "tiered");
            jail.set_env("RIO_CHUNK_BACKEND__BUCKET", "rio-chunks");
            jail.set_env("RIO_CHUNK_BACKEND__PREFIX", "");
            jail.set_env(
                "RIO_CHUNK_BACKEND__EXPRESS_BUCKET_BY_ZONE",
                r#"{"us-east-2a":"rio-build-chunk-cache--use2-az1--x-s3","us-east-2c":"rio-build-chunk-cache--use2-az3--x-s3"}"#,
            );
            jail.set_env("RIO_NODE_ZONE", "us-east-2c");
            let cfg: Config = rio_common::config::load("store", CliArgs::default()).unwrap();
            assert_eq!(cfg.node_zone.as_deref(), Some("us-east-2c"));
            match cfg.chunk_backend {
                Some(ChunkBackendKind::Tiered {
                    express_bucket,
                    express_bucket_by_zone,
                    ..
                }) => {
                    assert!(express_bucket.is_none());
                    assert_eq!(express_bucket_by_zone.len(), 2);
                    assert_eq!(
                        express_bucket_by_zone.get("us-east-2c").map(String::as_str),
                        Some("rio-build-chunk-cache--use2-az3--x-s3")
                    );
                }
                other => panic!("expected Tiered; got {other:?}"),
            }
            Ok(())
        });
    }

    /// Malformed JSON in the env-var form must fail config load with an
    /// error that names the field — not silently come back as an empty
    /// map (which would quietly disable the cache tier fleet-wide).
    #[test]
    fn chunk_backend_kind_env_tiered_zone_map_bad_json_rejected() {
        rio_test_support::Jail::expect_with(|jail| {
            jail.set_env("RIO_CHUNK_BACKEND__KIND", "tiered");
            jail.set_env("RIO_CHUNK_BACKEND__BUCKET", "rio-chunks");
            jail.set_env("RIO_CHUNK_BACKEND__PREFIX", "");
            jail.set_env(
                "RIO_CHUNK_BACKEND__EXPRESS_BUCKET_BY_ZONE",
                "us-east-2a=not-json",
            );
            let err = rio_common::config::load::<Config, _>("store", CliArgs::default())
                .unwrap_err()
                .to_string();
            assert!(err.contains("express_bucket_by_zone"), "got: {err}");
            Ok(())
        });
    }

    /// A templating layer that renders the env var as an empty string
    /// (helm normally omits it instead) must load as an EMPTY map — no
    /// parse error, no spurious one-entry map — so the replica degrades
    /// to direct S3-standard reads exactly like an absent var.
    #[test]
    fn chunk_backend_kind_env_tiered_zone_map_empty_string_is_empty_map() {
        rio_test_support::Jail::expect_with(|jail| {
            jail.set_env("RIO_CHUNK_BACKEND__KIND", "tiered");
            jail.set_env("RIO_CHUNK_BACKEND__BUCKET", "rio-chunks");
            jail.set_env("RIO_CHUNK_BACKEND__PREFIX", "");
            jail.set_env("RIO_CHUNK_BACKEND__EXPRESS_BUCKET_BY_ZONE", "");
            let cfg: Config = rio_common::config::load("store", CliArgs::default()).unwrap();
            match cfg.chunk_backend {
                Some(ChunkBackendKind::Tiered {
                    express_bucket_by_zone,
                    ..
                }) => {
                    assert!(
                        express_bucket_by_zone.is_empty(),
                        "empty env string must deserialize to an empty map, got \
                         {express_bucket_by_zone:?}"
                    );
                }
                other => panic!("expected Tiered; got {other:?}"),
            }
            Ok(())
        });
    }

    /// Selection decision table (P0554). Pure function — no env, no
    /// loader. Explicit bucket wins; otherwise zone+map must both be
    /// present and matching; every other combination selects nothing
    /// and the reason names the missing piece.
    #[test]
    fn express_selection_decision_table() {
        let map = BTreeMap::from([
            (
                "us-east-2a".to_string(),
                "rio-build-chunk-cache--use2-az1--x-s3".to_string(),
            ),
            (
                "us-east-2b".to_string(),
                "rio-build-chunk-cache--use2-az2--x-s3".to_string(),
            ),
        ]);
        let empty = BTreeMap::new();

        // Explicit bucket wins regardless of map/zone.
        assert_eq!(
            select_express_bucket(Some("explicit--use2-az1--x-s3"), &map, Some("us-east-2a")),
            ExpressSelection::Explicit("explicit--use2-az1--x-s3".into())
        );
        // Whitespace-only explicit value does NOT mask the map.
        assert_eq!(
            select_express_bucket(Some("  "), &map, Some("us-east-2a")),
            ExpressSelection::Selected {
                zone: "us-east-2a".into(),
                bucket: "rio-build-chunk-cache--use2-az1--x-s3".into(),
            }
        );
        // Zone present + mapped → that zone's bucket (and only that one).
        assert_eq!(
            select_express_bucket(None, &map, Some("us-east-2b")),
            ExpressSelection::Selected {
                zone: "us-east-2b".into(),
                bucket: "rio-build-chunk-cache--use2-az2--x-s3".into(),
            }
        );
        // Empty map → disabled, reason names the map.
        match select_express_bucket(None, &empty, Some("us-east-2a")) {
            ExpressSelection::Disabled { reason } => {
                assert!(reason.contains("express_bucket_by_zone"), "got: {reason}");
            }
            other => panic!("expected Disabled, got {other:?}"),
        }
        // Zone unknown (env unset or label missing → empty string) →
        // disabled, reason names RIO_NODE_ZONE / the pod label.
        for zone in [None, Some(""), Some("   ")] {
            match select_express_bucket(None, &map, zone) {
                ExpressSelection::Disabled { reason } => {
                    assert!(reason.contains("RIO_NODE_ZONE"), "got: {reason}");
                    assert!(
                        reason.contains("topology.kubernetes.io/zone"),
                        "got: {reason}"
                    );
                }
                other => panic!("expected Disabled, got {other:?}"),
            }
        }
        // Zone present but unmapped (AZ without Express) → disabled,
        // reason names the zone and the mapped zones.
        match select_express_bucket(None, &map, Some("us-east-2c")) {
            ExpressSelection::Disabled { reason } => {
                assert!(reason.contains("us-east-2c"), "got: {reason}");
                assert!(reason.contains("us-east-2a"), "got: {reason}");
            }
            other => panic!("expected Disabled, got {other:?}"),
        }
    }

    /// `Config::resolve_express_bucket` folds the selection into
    /// `express_bucket` in place (main.rs runs it before the backend is
    /// built and before the sweeper spawn — both read `express_bucket`).
    #[test]
    fn resolve_express_bucket_mutates_tiered_config() {
        let map = BTreeMap::from([(
            "us-east-2a".to_string(),
            "rio-build-chunk-cache--use2-az1--x-s3".to_string(),
        )]);

        // Selected: zone matches the map → express_bucket populated.
        let mut cfg = Config {
            database_url: "postgres://x".into(),
            node_zone: Some("us-east-2a".into()),
            chunk_backend: Some(ChunkBackendKind::Tiered {
                bucket: "rio-chunks".into(),
                prefix: String::new(),
                express_bucket: None,
                express_bucket_by_zone: map.clone(),
                express: ExpressConfig::default(),
            }),
            ..Default::default()
        };
        cfg.resolve_express_bucket();
        match &cfg.chunk_backend {
            Some(ChunkBackendKind::Tiered { express_bucket, .. }) => {
                assert_eq!(
                    express_bucket.as_deref(),
                    Some("rio-build-chunk-cache--use2-az1--x-s3")
                );
            }
            other => panic!("expected Tiered, got {other:?}"),
        }

        // Unmapped zone → stays None (local=None, degraded not down).
        let mut cfg = Config {
            database_url: "postgres://x".into(),
            node_zone: Some("us-east-2c".into()),
            chunk_backend: Some(ChunkBackendKind::Tiered {
                bucket: "rio-chunks".into(),
                prefix: String::new(),
                express_bucket: None,
                express_bucket_by_zone: map.clone(),
                express: ExpressConfig::default(),
            }),
            ..Default::default()
        };
        cfg.resolve_express_bucket();
        match &cfg.chunk_backend {
            Some(ChunkBackendKind::Tiered { express_bucket, .. }) => {
                assert!(express_bucket.is_none());
            }
            other => panic!("expected Tiered, got {other:?}"),
        }

        // Explicit bucket is left untouched (wins over the map).
        let mut cfg = Config {
            database_url: "postgres://x".into(),
            node_zone: Some("us-east-2a".into()),
            chunk_backend: Some(ChunkBackendKind::Tiered {
                bucket: "rio-chunks".into(),
                prefix: String::new(),
                express_bucket: Some("explicit--use2-az9--x-s3".into()),
                express_bucket_by_zone: map,
                express: ExpressConfig::default(),
            }),
            ..Default::default()
        };
        cfg.resolve_express_bucket();
        match &cfg.chunk_backend {
            Some(ChunkBackendKind::Tiered { express_bucket, .. }) => {
                assert_eq!(express_bucket.as_deref(), Some("explicit--use2-az9--x-s3"));
            }
            other => panic!("expected Tiered, got {other:?}"),
        }

        // Non-tiered kinds are a no-op (no panic, no mutation).
        let mut cfg = Config {
            database_url: "postgres://x".into(),
            node_zone: Some("us-east-2a".into()),
            chunk_backend: Some(ChunkBackendKind::Memory),
            ..Default::default()
        };
        cfg.resolve_express_bucket();
        assert!(matches!(cfg.chunk_backend, Some(ChunkBackendKind::Memory)));
    }

    /// Full `[binary_cache_compat]` table through the config-crate
    /// Value layer (same loader main.rs uses). The lowercase enum
    /// strings are load-bearing: `compression = "zstd"|"xz"|"none"`
    /// must match what the published `.narinfo` `Compression:` field
    /// will say, and the NixOS module / helm values write these exact
    /// strings.
    #[test]
    fn binary_cache_compat_toml() {
        let cfg = parse_toml(
            r#"
            [binary_cache_compat]
            enabled = false
            bucket = "nix-cache"
            compression = "xz"
            write_mode = "sync_after_commit"
            reconcile_interval_secs = 0
            "#,
        );
        assert!(!cfg.binary_cache_compat.enabled);
        assert_eq!(cfg.binary_cache_compat.bucket.as_deref(), Some("nix-cache"));
        assert_eq!(cfg.binary_cache_compat.compression, CompatCompression::Xz);
        assert_eq!(
            cfg.binary_cache_compat.write_mode,
            CompatWriteMode::SyncAfterCommit
        );
        // 0 = reconciler disabled (the "I only ever want inline writes"
        // escape hatch).
        assert_eq!(cfg.binary_cache_compat.reconcile_interval_secs, 0);
    }

    /// Partial `[binary_cache_compat]` table: unspecified fields fall
    /// back to the struct-level `#[serde(default)]` — in particular
    /// `enabled` stays `true` when an operator only pins the bucket or
    /// codec. A partial table silently flipping the toggle OFF would
    /// stop compat publication without anyone asking for it.
    #[test]
    fn binary_cache_compat_partial_table_keeps_default_on() {
        let cfg = parse_toml(
            r#"
            [binary_cache_compat]
            bucket = "nix-cache"
            "#,
        );
        assert!(cfg.binary_cache_compat.enabled);
        assert_eq!(cfg.binary_cache_compat.bucket.as_deref(), Some("nix-cache"));
        assert_eq!(cfg.binary_cache_compat.compression, CompatCompression::Zstd);
    }

    // r[verify store.compat.runtime-toggle]
    /// The runtime toggle via the env layer — the path helm actually
    /// uses (store.yaml renders `RIO_BINARY_CACHE_COMPAT__*`). The
    /// disable switch is the whole point: `ENABLED=false` must reach
    /// the parsed config, and the nested-struct fields must round-trip
    /// alongside it.
    #[test]
    fn binary_cache_compat_env_disable() {
        rio_test_support::Jail::expect_with(|jail| {
            jail.set_env("RIO_BINARY_CACHE_COMPAT__ENABLED", "false");
            jail.set_env("RIO_BINARY_CACHE_COMPAT__BUCKET", "nix-cache");
            jail.set_env("RIO_BINARY_CACHE_COMPAT__COMPRESSION", "none");
            let cfg: Config = rio_common::config::load("store", CliArgs::default()).unwrap();
            assert!(
                !cfg.binary_cache_compat.enabled,
                "RIO_BINARY_CACHE_COMPAT__ENABLED=false must disable the compat layer"
            );
            assert_eq!(cfg.binary_cache_compat.bucket.as_deref(), Some("nix-cache"));
            assert_eq!(cfg.binary_cache_compat.compression, CompatCompression::None);
            Ok(())
        });
    }

    /// No `RIO_BINARY_CACHE_COMPAT__*` env vars at all → compiled
    /// default (enabled, chunk-backend bucket, zstd). This is what a
    /// helm deployment that omits the values block gets.
    #[test]
    fn binary_cache_compat_env_absent_is_default_on() {
        rio_test_support::Jail::expect_with(|_jail| {
            let cfg: Config = rio_common::config::load("store", CliArgs::default()).unwrap();
            assert!(cfg.binary_cache_compat.enabled);
            assert!(cfg.binary_cache_compat.bucket.is_none());
            assert_eq!(cfg.binary_cache_compat.compression, CompatCompression::Zstd);
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
    fn nar_buffer_budget_toml_roundtrip() {
        rio_test_support::Jail::expect_with(|jail| {
            jail.create_file("store.toml", "nar_buffer_budget_bytes = 12345")?;
            let cfg: Config = rio_common::config::load("store", CliArgs::default()).unwrap();
            assert_eq!(
                cfg.nar_buffer_budget_bytes,
                Some(12345),
                "store.toml nar_buffer_budget_bytes must thread through the config layers"
            );
            Ok(())
        });
    }

    /// Absent from TOML → None, not Some(0). The struct-level
    /// `#[serde(default)]` handles absence via Default::default(),
    /// which sets None. main()'s match then keeps DEFAULT_NAR_BUDGET.
    #[test]
    fn nar_buffer_budget_absent_is_none() {
        rio_test_support::Jail::expect_with(|jail| {
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

    // Jailed standing-guard tests — see rio-test-support/src/config.rs.
    // When you add Config.newfield: ADD IT to both assert blocks below.

    rio_test_support::jail_roundtrip!(
        "store",
        r#"
        nar_buffer_budget_bytes = 99999
        chunk_cache_capacity_bytes = 123456
        chunk_upload_max_concurrent = 64
        s3_max_attempts = 5
        node_zone = "us-east-2a"

        [chunk_backend]
        kind = "filesystem"
        base_dir = "/custom/path"

        [binary_cache_compat]
        enabled = false
        compression = "none"

        [jwt]
        required = true
        "#,
        |cfg: Config| {
            assert_eq!(cfg.nar_buffer_budget_bytes, Some(99999));
            assert_eq!(cfg.chunk_cache_capacity_bytes, 123456);
            assert_eq!(cfg.chunk_upload_max_concurrent, 64);
            assert_eq!(cfg.s3_max_attempts, 5);
            assert_eq!(
                cfg.node_zone.as_deref(),
                Some("us-east-2a"),
                "node_zone must thread through the config layers"
            );
            assert!(
                matches!(cfg.chunk_backend, Some(ChunkBackendKind::Filesystem { .. })),
                "[chunk_backend] table must thread through the config layers"
            );
            assert!(
                !cfg.binary_cache_compat.enabled,
                "[binary_cache_compat] table must thread through the config layers"
            );
            assert_eq!(cfg.binary_cache_compat.compression, CompatCompression::None);
            assert!(
                cfg.jwt.required,
                "[jwt] table must thread through the config layers into JwtConfig"
            );
            // Unspecified sub-field defaults via #[serde(default)]
            // on the sub-struct (partial table must work).
            assert!(cfg.binary_cache_compat.bucket.is_none());
        }
    );

    rio_test_support::jail_defaults!("store", r#"listen_addr = "0.0.0.0:9002""#, |cfg: Config| {
        assert!(cfg.chunk_backend.is_none());
        assert!(cfg.node_zone.is_none());
        assert!(cfg.nar_buffer_budget_bytes.is_none());
        assert_eq!(cfg.jwt, rio_common::config::JwtConfig::default());
        // ADR-022 binary-cache compat: absent section → default ON,
        // chunk-backend bucket, zstd, sync-after-commit.
        assert_eq!(cfg.binary_cache_compat, BinaryCacheCompat::default());
        assert!(cfg.binary_cache_compat.enabled);
        assert!(cfg.signing_key_path.is_none());
        assert!(cfg.hmac_key_path.is_none());
        assert_eq!(
            cfg.chunk_upload_max_concurrent,
            crate::cas::DEFAULT_CHUNK_UPLOAD_CONCURRENCY
        );
        assert_eq!(cfg.s3_max_attempts, DEFAULT_S3_MAX_ATTEMPTS);
        assert_eq!(
            cfg.nar_index_concurrency,
            crate::nar_index::DEFAULT_NAR_INDEX_CONCURRENCY
        );
    });

    // -----------------------------------------------------------------------
    // validate_config rejection tests — spreads the P0409 pattern
    // (rio-scheduler/src/main.rs) to the store.
    // -----------------------------------------------------------------------

    /// `Config::default()` leaves `database_url` empty and
    /// `chunk_backend` unset, both of which validate_config rejects.
    /// Fill them with placeholders so the returned config passes as-is.
    fn test_valid_config() -> Config {
        Config {
            database_url: "postgres://localhost/rio".into(),
            chunk_backend: Some(ChunkBackendKind::Memory),
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
