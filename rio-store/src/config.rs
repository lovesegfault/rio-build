//! rio-store binary configuration.
//!
//! Layered-config-loaded (TOML + `RIO_` env vars + CLI flags) via
//! [`rio_common::config::load`]. See `rio-common/src/config.rs` for the
//! two-struct (Config + CliArgs) split rationale.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::backend::{ChunkBackend, FilesystemChunkBackend, S3ChunkBackend};
use crate::cas::ChunkCache;
use rio_common::s3::DEFAULT_S3_MAX_ATTEMPTS;

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
#[derive(Debug, Serialize, Deserialize, Default, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ChunkBackendKind {
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

// r[impl store.netpol.egress+3]
// Egress targets are exactly what's configured here: postgres
// (`database_url`), the chunk backend (S3 or filesystem), and the
// scheduler's ExecutorService (`materialization.scheduler_addr` — the
// materialization executor's poll/claim/report edge). The
// `store-egress` CiliumNetworkPolicy in infra/helm/rio-build/templates/
// networkpolicy.yaml allows CoreDNS + postgres:5432 (toEndpoints +
// postgresCidr) + scheduler:9001 (toEndpoints) + S3-VPC-endpoint:443
// only — tracey doesn't scan YAML; this Config is the scannable anchor.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct Config {
    /// gRPC listen address.
    pub listen_addr: std::net::SocketAddr,
    /// PostgreSQL connection URL. Required.
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
    /// Global NAR buffer budget in bytes — total permits across ALL
    /// concurrent NAR ingests, shared by PutPath (per-chunk charges)
    /// and upstream substitution (one whole-NAR reservation per leg,
    /// live_047/R-C). None → DEFAULT_NAR_BUDGET (8 × MAX_NAR_SIZE =
    /// 32 GiB). Floor: MAX_NAR_SIZE (4 GiB) — the budget MUST admit
    /// one whole-NAR reservation, or a declared-near-MAX substitution
    /// becomes a permanently-unsatisfiable fair-FIFO head that
    /// starves every later budget waiter (the former "lower this on
    /// small-memory nodes" use-case is retired: a sub-4 GiB budget
    /// was a wedge, not a tuning). Raise it if you have >8 concurrent
    /// max-size ingests and RAM to match.
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
    /// Default 8. Per-replica `r[store.substitute.admission+2]` bounds
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
    /// Owner-side download-stall window for upstream substitution: a
    /// NAR download with no body bytes for this long is aborted by its
    /// own owner — the placeholder claim is released in place (claim
    /// cleared, durable `stall_count` incremented) so the next attempt
    /// re-claims immediately. Also the threshold competing replicas
    /// apply when deciding whether a live mid-download claim is
    /// download-stalled (frozen `last_progress_at`); persist-phase and
    /// PutPath claims are never stall-reclaimed. Default 180 s = 6
    /// missed-progress heartbeats — S3 multipart pauses and admission
    /// queueing inside a download don't trip it. Floor: must be >= 2x
    /// the 30s placeholder heartbeat (>= 60s) — a window below the
    /// heartbeat granularity would depose every healthy owner between
    /// two of its own progress writes (merged_bug_082); 0 would brick
    /// substitution outright. Env: `RIO_SUBSTITUTE_STALL_SECS`.
    #[serde(rename = "substitute_stall_secs", with = "rio_common::config::secs")]
    #[schemars(with = "u64")]
    pub substitute_stall: std::time::Duration,
    /// Per-replica budget for resident build-log ingest buffer bytes
    /// across all concurrent `LogService.AppendLog` streams. Each stream
    /// reserves `2 × log_cut_threshold_bytes` (its worst-case resident
    /// buffer: one chunk mid-cut plus one refilling) at open and holds
    /// the reservation for the stream's lifetime; a stream that cannot
    /// reserve is rejected with `RESOURCE_EXHAUSTED` and the builder
    /// retries against another replica. Deliberately separate from
    /// `nar_buffer_budget_bytes` so log ingest and NAR ingest cannot
    /// starve each other. Default 1 GiB; bounded above at 2^60 (a
    /// budget beyond addressable memory is a typo, and semaphore
    /// permit math saturates rather than panics under the bound).
    /// Env: `RIO_LOG_BYTES_BUDGET`.
    pub log_bytes_budget: u64,
    /// Per-replica cap on concurrent `LogService.AppendLog` streams.
    /// The count twin of `log_bytes_budget` (whichever is exhausted
    /// first wins). Default 256. Env: `RIO_LOG_MAX_STREAMS`.
    pub log_max_streams: usize,
    /// Per-execution cap on accepted log bytes (post-truncation content
    /// plus a per-line overhead charge) over the lifetime of one
    /// execution's log. Stream-fatal (`RESOURCE_EXHAUSTED`) when
    /// exceeded. Default 1 GiB. Env: `RIO_LOG_INGEST_BYTE_CAP`.
    pub log_ingest_byte_cap: u64,
    /// Per-execution cap on durably committed log chunks. A builder
    /// fabricating forward line-number gaps gets one S3 object per
    /// contiguous run; the byte cap already bounds the total but this
    /// caps the object count directly. Stream-fatal when exceeded.
    /// Default 100000. Env: `RIO_LOG_MAX_CHUNKS_PER_EXEC`.
    pub log_max_chunks_per_exec: u32,
    /// Periodic chunk-cut cadence for `AppendLog` streams: a non-empty
    /// ingest buffer is flushed to S3 at least this often, so a
    /// scheduler-visible log is never more than this far behind the
    /// builder. Also the basis for the gray-failure staleness abort
    /// (2× this). Default 60 s. Env: `RIO_LOG_CUT_INTERVAL_SECS`.
    #[serde(rename = "log_cut_interval_secs", with = "rio_common::config::secs")]
    #[schemars(with = "u64")]
    pub log_cut_interval: std::time::Duration,
    /// Size-triggered chunk cut: a chunk is cut as soon as the ingest
    /// buffer holds this many uncompressed bytes, without waiting for
    /// the periodic timer. Default 8 MiB (≈2 MiB compressed at the
    /// typical 4:1 log ratio). Env: `RIO_LOG_CUT_THRESHOLD_BYTES`.
    pub log_cut_threshold_bytes: u64,
    /// Comma-separated CORS allowed origins for gRPC-Web `TailLog`
    /// requests from the dashboard SPA. Same format and rationale as the
    /// scheduler's `dashboard.cors_allow_origins` (the store now serves
    /// one browser-facing RPC). Empty (default) = no browser origin is
    /// allowed; native gRPC callers are unaffected. Env:
    /// `RIO_LOG_CORS_ALLOW_ORIGINS`.
    pub log_cors_allow_origins: String,
    /// URL template the cross-replica `TailLog` proxy uses to dial the
    /// store replica holding an execution's live ingest stream. Every
    /// `{pod}` is replaced with the owning replica's
    /// `log_ingest_sessions.replica_pod` value; an identity that
    /// parses as an IPv6 address is bracketed automatically, so the
    /// template always uses the bare `{pod}` form (`[{pod}]` is
    /// rejected by validation). Default EMPTY = the cross-replica
    /// proxy is disabled, fail-closed: a bare binary cannot know the
    /// deployment's pod-addressing scheme, and a wrong guess silently
    /// misroutes instead of failing. The helm chart supplies the real
    /// template (`store.logPeerUrlTemplate`, default
    /// `http://{pod}:9002` paired with `status.podIP` via the downward
    /// API as the replica identity). With the proxy disabled or a
    /// template that does not resolve, cross-replica live tails
    /// degrade to the history-only view (counted by
    /// `rio_store_log_tail_proxy_failures_total`); a read never
    /// fails over it. Env: `RIO_LOG_PEER_URL_TEMPLATE`.
    pub log_peer_url_template: String,
    /// Substitution-replacement campaign (design §8): the store-side
    /// materialization-job executor. `[materialization]` table in
    /// store.toml. Env: `RIO_MATERIALIZATION__*`. Phase B activated the
    /// executor at the deployment layer (helm values default ON); this
    /// struct's default stays `false` — a bare binary spawns no
    /// executor task set.
    pub materialization: MaterializationConfig,
    /// Build-log retention, in days since the execution *started*
    /// (`drv_executions.started_at` — the only timestamp every
    /// execution has). The hourly TTL sweep deletes older executions'
    /// manifest rows and chunk objects. On EKS this is deployed from
    /// the terraform variable `log_retention_days` (xtask deploy
    /// `--set store.logRetentionDays`), and the S3 `logs/` lifecycle
    /// backstop expires at the SAME variable + 7 days of slack — the
    /// two deleters derive from one source and cannot drift. Operators
    /// outside that chain must keep any bucket lifecycle at or above
    /// this value plus slack (an undercutting lifecycle hard-deletes
    /// chunks the read path still references). Default 30. Env:
    /// `RIO_LOG_RETENTION_DAYS`.
    pub log_retention_days: u32,
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
            chunk_upload_max_concurrent: crate::cas::DEFAULT_CHUNK_UPLOAD_CONCURRENCY,
            s3_max_attempts: DEFAULT_S3_MAX_ATTEMPTS,
            max_batch_paths: crate::grpc::DEFAULT_MAX_BATCH_PATHS,
            chunk_prefetch_k: crate::grpc::DEFAULT_CHUNK_PREFETCH_K,
            stream_drain: std::time::Duration::from_secs(90),
            pg_max_connections: DEFAULT_PG_MAX_CONNECTIONS,
            substitute_admission_permits: None,
            substitute_stall: crate::substitute::DEFAULT_SUBSTITUTE_STALL_WINDOW,
            log_bytes_budget: 1024 * 1024 * 1024,
            log_max_streams: 256,
            log_ingest_byte_cap: crate::logs::ingest::DEFAULT_PER_EXEC_BYTE_CAP,
            log_max_chunks_per_exec: 100_000,
            log_cut_interval: crate::logs::ingest::DEFAULT_CUT_INTERVAL,
            log_cut_threshold_bytes: crate::logs::ingest::DEFAULT_CUT_THRESHOLD_BYTES,
            log_cors_allow_origins: String::new(),
            // Empty = the cross-replica live-tail proxy is DISABLED,
            // fail-closed: a bare binary cannot know the deployment's
            // pod-addressing scheme, and a wrong guess silently
            // misroutes (resolves but dials the wrong pod) instead of
            // failing. The helm chart supplies the real template
            // (values.yaml `logPeerUrlTemplate`); readers on a
            // template-less deployment get the history-only view.
            log_peer_url_template: String::new(),
            materialization: MaterializationConfig::default(),
            log_retention_days: 30,
        }
    }
}

/// Store-owned materialization jobs (design §8) — THE substitution
/// mechanism. The executor spawns whenever a scheduler address is
/// configured (PD-D2) — a bare binary with no address spawns nothing
/// and needs no scheduler.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct MaterializationConfig {
    /// Concurrent jobs per replica (design §2.2 item 1: default 8).
    pub executor_concurrency: usize,
    /// Poll interval for ListMaterializationJobs (seconds; jitter added).
    pub poll_interval_secs: u64,
    /// Scheduler ExecutorService address (the store→scheduler edge:
    /// ListMaterializationJobs / PullAssignment / ReportOutcome).
    /// THE executor spawn condition (PD-D2): non-empty spawns the
    /// claim loops; empty disables the executor (the schedulerless
    /// pure-store deployment stays valid). The VM/helm wiring sets it
    /// explicitly.
    pub scheduler_addr: String,
    /// Bounded intra-job substitution fan-out (live_047/R-C,
    /// `store.materialize.path-fold`): per-job path-resolution window
    /// width F — at most this many concurrent path resolutions per
    /// walk. Default 4. Total executor pressure on the admission gate
    /// is bounded STRUCTURALLY by the pod path-slot pool (P =
    /// effective admission cap / 2, `store.materialize.gate-share`),
    /// independent of F and `executor_concurrency` — both are
    /// shaping-only levers: at full worker occupancy walks run ~1
    /// slot each (job-grain); at low occupancy walks widen to F
    /// (path-grain). 1 = the serial walk as the F=1 instance of the
    /// same code shape. Env: `RIO_MATERIALIZATION__PATH_FANOUT`.
    pub path_fanout: usize,
}

impl Default for MaterializationConfig {
    fn default() -> Self {
        Self {
            executor_concurrency: 8,
            poll_interval_secs: 1,
            scheduler_addr: String::new(),
            path_fanout: 4,
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

// r[impl store.materialize.gate-share]
/// The EFFECTIVE substitute-admission capacity — override included.
/// THE one value main.rs constructs the gate from AND derives the
/// executor path-slot pool from (live_047/R-C): a formula-only pool
/// size would let the documented `substitute_admission_permits`
/// override hand the executor the entire gate (e.g. override 32 with
/// formula-P 32 = 100 % of the effective cap — the invariant inverted
/// through an existing config surface).
pub fn effective_substitute_admission_cap(overridden: Option<usize>, pg_max: u32) -> usize {
    overridden.unwrap_or_else(|| derive_substitute_admission_cap(pg_max))
}

// r[impl store.materialize.gate-share]
/// Executor path-slot pool size: P = effective_cap / 2
/// (`store.materialize.gate-share`). The executor alone can never
/// saturate the admission gate — ≥ cap/2 permits remain available to
/// RPC miss traffic at all times, independent of `path_fanout` and
/// `executor_concurrency` (the d1f18610d n=32 envelope made
/// structural). Deliberately NO floor: a cap of 1 would make P = 0
/// and wedge every walk at the baseline acquire — that override is
/// REJECTED at validate() (`substitute_admission_permits >= 2`)
/// rather than floored, because a floored P = 1 = cap silently hands
/// the executor 100 % of a cap-1 gate, the exact inversion this
/// derivation exists to prevent.
pub fn derive_executor_path_slots(effective_cap: usize) -> usize {
    effective_cap / 2
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
        // E-2(i), live_047/R-C: the budget MUST admit one whole-NAR
        // reservation (`declared.max(MIN_NAR_CHUNK_CHARGE)` <
        // MAX_NAR_SIZE by the `>=` declared-size rejection). Below the
        // floor, a near-MAX reservation is a permanently-unsatisfiable
        // fair-FIFO head: it starves every later budget waiter
        // (substitution AND PutPath) forever — the config-violable
        // premise of the no-deadlock head-progress argument. Subsumes
        // the former MIN_NAR_CHUNK_CHARGE floor (PutPath's per-chunk
        // Pending-forever hang at < 256). There is no "unlimited"
        // sentinel; unset for the 32 GiB default.
        anyhow::ensure!(
            self.nar_buffer_budget_bytes
                .is_none_or(|b| b >= rio_common::limits::MAX_NAR_SIZE),
            "nar_buffer_budget_bytes must be >= {} (MAX_NAR_SIZE — one whole-NAR \
             reservation; a smaller budget wedges any near-max substitution at \
             the FIFO head forever); unset RIO_NAR_BUFFER_BUDGET_BYTES for the \
             32 GiB default — there is no 'unlimited' sentinel",
            rio_common::limits::MAX_NAR_SIZE
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
        // < 2 → either Semaphore::new(0) (store never substitutes) or,
        // at exactly 1, an executor path-slot pool of P = cap/2 = 0 —
        // every materialization walk wedges at the baseline slot
        // acquire (live_047/R-C; rejection over flooring: a floored
        // P = 1 = cap hands the executor the WHOLE cap-1 gate). None
        // is fine (derived from pg_max).
        anyhow::ensure!(
            self.substitute_admission_permits.is_none_or(|n| n >= 2),
            "substitute_admission_permits must be >= 2 (1 makes the executor \
             path-slot pool empty and wedges every materialization walk); unset \
             RIO_SUBSTITUTE_ADMISSION_PERMITS to derive from pg_max_connections"
        );
        // E-1: the driver floors the window at 1 defensively, but a
        // configured 0 is a typo — reject at startup, not silently
        // reinterpret (the validate-over-clamp convention).
        anyhow::ensure!(
            self.materialization.path_fanout >= 1,
            "materialization.path_fanout must be >= 1; set \
             RIO_MATERIALIZATION__PATH_FANOUT"
        );
        // 0 → Semaphore::new(0) → every AppendLog open is rejected with
        // RESOURCE_EXHAUSTED: build logs silently stop being stored.
        anyhow::ensure!(
            self.log_max_streams >= 1,
            "log_max_streams must be >= 1; set RIO_LOG_MAX_STREAMS"
        );
        // 0 → contiguous_prefix drains degenerate to per-batch chunks
        // and the per-stream byte reservation is 0 (every stream
        // admitted, no memory bound).
        anyhow::ensure!(
            self.log_cut_threshold_bytes >= 1,
            "log_cut_threshold_bytes must be >= 1; set RIO_LOG_CUT_THRESHOLD_BYTES"
        );
        // The byte budget must admit at least one stream's reservation
        // (2 × cut threshold), or every AppendLog open is rejected and
        // build logs silently stop being stored.
        anyhow::ensure!(
            self.log_bytes_budget >= self.log_cut_threshold_bytes.saturating_mul(2),
            "log_bytes_budget ({}) must be >= 2 × log_cut_threshold_bytes ({}) \
             or no AppendLog stream can ever be admitted; set RIO_LOG_BYTES_BUDGET",
            self.log_bytes_budget,
            self.log_cut_threshold_bytes
        );
        anyhow::ensure!(
            self.log_bytes_budget <= 1u64 << 60,
            "log_bytes_budget ({}) exceeds 2^60 — beyond addressable memory, \
             almost certainly a typo; set RIO_LOG_BYTES_BUDGET",
            self.log_bytes_budget
        );
        // 0 → the periodic cut interval is zero → a busy-loop of empty
        // cuts; and the gray-failure staleness bound (2×) is zero → every
        // stream aborts on its first tick.
        anyhow::ensure!(
            !self.log_cut_interval.is_zero(),
            "log_cut_interval_secs must be >= 1; set RIO_LOG_CUT_INTERVAL_SECS"
        );
        // merged_bug_082: a stall window of 0 disables matching
        // entirely (bricked substitution: every owner instantly
        // strikeable / no takeover ever matches NULL comparisons), and
        // one below 2x the heartbeat granularity deposes healthy
        // owners between two of their own progress writes. Same 2x
        // staleness-bound convention as log_cut_interval.
        anyhow::ensure!(
            !self.substitute_stall.is_zero()
                && self.substitute_stall >= 2 * crate::ingest::PLACEHOLDER_HEARTBEAT_INTERVAL,
            "substitute_stall_secs must be >= {} (2x the placeholder heartbeat); \
             set RIO_SUBSTITUTE_STALL_SECS",
            2 * crate::ingest::PLACEHOLDER_HEARTBEAT_INTERVAL.as_secs()
        );
        // 0 → every stream hits the chunk cap at its FIRST cut and
        // aborts: build logs silently stop being stored.
        anyhow::ensure!(
            self.log_max_chunks_per_exec >= 1,
            "log_max_chunks_per_exec must be >= 1; set RIO_LOG_MAX_CHUNKS_PER_EXEC"
        );
        // The byte cap bounds an execution's TOTAL log; the cut
        // threshold is ONE chunk's target size. A total cap below one
        // chunk's worth means the threshold is unreachable dead config
        // and any log that size aborts at the cap before a
        // threshold-triggered cut — at the degenerate end (cap 0),
        // every stream aborts on its first batch.
        anyhow::ensure!(
            self.log_ingest_byte_cap >= self.log_cut_threshold_bytes,
            "log_ingest_byte_cap ({}) must be >= log_cut_threshold_bytes ({}): \
             a per-execution total-log cap smaller than one chunk's cut \
             threshold aborts streams at the cap before they can cut a \
             chunk; set RIO_LOG_INGEST_BYTE_CAP",
            self.log_ingest_byte_cap,
            self.log_cut_threshold_bytes
        );
        // 0 → the sweep deletes every log on its first tick. The
        // scheduler's equivalent knob has carried the same guard since
        // it shipped; a store that retains nothing is always a
        // misconfiguration.
        anyhow::ensure!(
            self.log_retention_days >= 1,
            "log_retention_days must be >= 1; set RIO_LOG_RETENTION_DAYS"
        );
        // Empty disables the cross-replica proxy (fail-closed). A
        // non-empty template without `{pod}` resolves every peer to
        // the same URI, so every cross-replica tail relays to one
        // (probably wrong) pod — a confusing misroute that looks like
        // it works in a single-replica deployment and silently breaks
        // at two. The bracketed `[{pod}]` form is rejected outright:
        // uri_for now brackets IPv6 identities itself (the
        // address-form rule lives in exactly one place), so a pinned
        // bracketed template would double-bracket every dial.
        if !self.log_peer_url_template.is_empty() {
            anyhow::ensure!(
                self.log_peer_url_template.contains("{pod}"),
                "log_peer_url_template must contain the literal `{{pod}}` \
                 placeholder (got {:?}); set RIO_LOG_PEER_URL_TEMPLATE",
                self.log_peer_url_template
            );
            anyhow::ensure!(
                !self.log_peer_url_template.contains("[{pod}]"),
                "log_peer_url_template must use the bare `{{pod}}` placeholder \
                 (got {:?}): the store now brackets IPv6 identities itself, so \
                 drop the literal brackets (e.g. `http://{{pod}}:9002`); set \
                 RIO_LOG_PEER_URL_TEMPLATE",
                self.log_peer_url_template
            );
        }
        // Substitution-replacement materialization executor (Phase A:
        // dormant; the bounds hold whether or not the flag is on so a
        // flip-on never trips over degenerate knobs).
        // 0 → zero claim loops: `enabled = true` silently does nothing.
        anyhow::ensure!(
            self.materialization.executor_concurrency >= 1,
            "materialization.executor_concurrency must be >= 1, got {} \
             (0 spawns no claim loops); set RIO_MATERIALIZATION__EXECUTOR_CONCURRENCY",
            self.materialization.executor_concurrency
        );
        // 0 → a busy poll loop against the scheduler's leader.
        anyhow::ensure!(
            self.materialization.poll_interval_secs >= 1,
            "materialization.poll_interval_secs must be >= 1, got {} \
             (0 busy-polls the scheduler); set RIO_MATERIALIZATION__POLL_INTERVAL_SECS",
            self.materialization.poll_interval_secs
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
pub async fn init_chunk_backend(
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
        assert_eq!(d.max_batch_paths, crate::grpc::DEFAULT_MAX_BATCH_PATHS);
        // r[verify store.get.chunk-prefetch]
        assert_eq!(d.chunk_prefetch_k, 64);
        assert_eq!(d.stream_drain, std::time::Duration::from_secs(90));
        assert_eq!(d.pg_max_connections, DEFAULT_PG_MAX_CONNECTIONS);
        // None → main.rs derives via derive_substitute_admission_cap.
        assert!(d.substitute_admission_permits.is_none());
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

    /// Any budget below one whole-NAR reservation is rejected; the
    /// sub-MIN_NAR_CHUNK_CHARGE case (PutPath's per-chunk
    /// Pending-forever hang at < 256) is subsumed by the MAX_NAR_SIZE
    /// floor (live_047/R-C).
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

    // r[verify store.materialize.gate-share]
    /// E-3 (live_047/R-C): P = effective_admission_cap / 2, pinned
    /// through the REAL symbols on BOTH arms — formula (None →
    /// derive_substitute_admission_cap) and override (Some(o) → o) —
    /// no mirrored literals: the relation is computed from the same
    /// functions main.rs feeds the gate and the pool.
    #[test]
    fn path_slot_pool_is_half_the_effective_admission_cap() {
        // Formula arm: both clamp edges and the linear region.
        for pg in [1u32, 20, 50, 1000] {
            let cap = effective_substitute_admission_cap(None, pg);
            assert_eq!(cap, derive_substitute_admission_cap(pg), "pg={pg}");
            assert_eq!(derive_executor_path_slots(cap), cap / 2, "pg={pg}");
            // The formula floor (64) keeps P >= 32 on every
            // formula-derived deployment.
            assert!(derive_executor_path_slots(cap) >= 32, "pg={pg}");
        }
        // Override arm: P tracks the EFFECTIVE value (the inversion
        // EM-2 closed); the validate() >= 2 floor keeps P >= 1.
        for o in [2usize, 3, 32, 64, 127, 128, 256] {
            let cap = effective_substitute_admission_cap(Some(o), 20);
            assert_eq!(cap, o, "override must win verbatim");
            assert_eq!(derive_executor_path_slots(cap), o / 2, "o={o}");
            assert!(derive_executor_path_slots(cap) >= 1, "o={o}");
        }
    }

    // r[verify store.materialize.gate-share]
    /// live_047/R-C (WO-R7-3): an admission-permit override below 2
    /// makes the executor path-slot pool P = cap/2 = 0 — every walk
    /// wedges at the baseline slot acquire. Rejected at validate()
    /// rather than floored: a floored P = 1 = cap silently hands the
    /// executor 100% of a cap-1 gate, the exact inversion the
    /// gate-share invariant exists to prevent.
    #[test]
    fn validate_rejects_admission_cap_below_two() {
        let cfg = Config {
            database_url: "postgres://x".into(),
            substitute_admission_permits: Some(1),
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("substitute_admission_permits"), "got: {err}");
        // Boundary: 2 admits one executor slot + one RPC permit.
        let ok = Config {
            database_url: "postgres://x".into(),
            substitute_admission_permits: Some(2),
            ..Default::default()
        };
        ok.validate().expect("cap 2 is the floor");
    }

    /// E-2(i), live_047/R-C: the deployed NAR budget MUST admit one
    /// whole-NAR reservation — `nar_buffer_budget_bytes >=
    /// MAX_NAR_SIZE` — or a declared-near-MAX single-shot reservation
    /// becomes a permanently-unsatisfiable fair-FIFO head that starves
    /// every later budget waiter (the config-violable head-progress
    /// premise; the `log_bytes_budget >= 2 × log_cut_threshold_bytes`
    /// one-reservation floor is the precedent one lane over).
    #[test]
    fn validate_rejects_nar_budget_below_one_reservation() {
        use rio_common::limits::MAX_NAR_SIZE;
        let cfg = Config {
            database_url: "postgres://x".into(),
            nar_buffer_budget_bytes: Some(MAX_NAR_SIZE - 1),
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("nar_buffer_budget_bytes"), "got: {err}");
        // Boundary: exactly one reservation is admissible (reservations
        // are < MAX_NAR_SIZE by the declared-size `>=` rejection).
        let ok = Config {
            database_url: "postgres://x".into(),
            nar_buffer_budget_bytes: Some(MAX_NAR_SIZE),
            ..Default::default()
        };
        ok.validate()
            .expect("budget == MAX_NAR_SIZE admits one reservation");
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

    /// `substitute_admission_permits=Some(0)` → `Semaphore::new(0)` →
    /// every `try_substitute_on_miss` queues for the full wait then
    /// returns `ResourceExhausted`; store silently never substitutes.
    #[test]
    fn validate_rejects_zero_admission_permits() {
        let cfg = Config {
            database_url: "postgres://x".into(),
            substitute_admission_permits: Some(0),
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("substitute_admission_permits"), "got: {err}");
        // None (unset) is fine — derived from pg_max_connections.
        let ok = Config {
            database_url: "postgres://x".into(),
            substitute_admission_permits: None,
            ..Default::default()
        };
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn validate_rejects_zero_log_max_streams() {
        let cfg = Config {
            database_url: "postgres://x".into(),
            log_max_streams: 0,
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("log_max_streams"), "got: {err}");
    }

    #[test]
    fn validate_rejects_zero_log_cut_threshold() {
        let cfg = Config {
            database_url: "postgres://x".into(),
            log_cut_threshold_bytes: 0,
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("log_cut_threshold_bytes"), "got: {err}");
    }

    /// A budget that cannot admit even one stream's reservation
    /// (2 × cut threshold) silently rejects every AppendLog open.
    #[test]
    fn validate_rejects_log_budget_below_one_reservation() {
        let cfg = Config {
            database_url: "postgres://x".into(),
            log_bytes_budget: 1024,
            log_cut_threshold_bytes: 4096,
            // Keep the cap ≥ threshold so only the budget rule fires.
            log_ingest_byte_cap: 4096,
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("log_bytes_budget"), "got: {err}");
    }

    #[test]
    fn validate_rejects_zero_log_cut_interval() {
        let cfg = Config {
            database_url: "postgres://x".into(),
            log_cut_interval: std::time::Duration::ZERO,
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("log_cut_interval_secs"), "got: {err}");
    }

    /// merged_bug_082: a stall window of 0 bricks substitution and one
    /// below 2x the 30s heartbeat deposes healthy owners between two of
    /// their own progress writes. Floor: 60s; the boundary value is
    /// accepted.
    #[test]
    fn validate_rejects_zero_substitute_stall() {
        let cfg = Config {
            database_url: "postgres://x".into(),
            substitute_stall: std::time::Duration::ZERO,
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("substitute_stall_secs"), "got: {err}");
    }

    #[test]
    fn validate_rejects_sub_heartbeat_substitute_stall_29s() {
        let cfg = Config {
            database_url: "postgres://x".into(),
            substitute_stall: std::time::Duration::from_secs(29),
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("substitute_stall_secs"), "got: {err}");
    }

    #[test]
    fn validate_rejects_sub_floor_substitute_stall_59s() {
        let cfg = Config {
            database_url: "postgres://x".into(),
            substitute_stall: std::time::Duration::from_secs(59),
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("substitute_stall_secs"), "got: {err}");
    }

    #[test]
    fn validate_accepts_substitute_stall_at_the_60s_floor() {
        let cfg = Config {
            database_url: "postgres://x".into(),
            substitute_stall: std::time::Duration::from_secs(60),
            ..Default::default()
        };
        cfg.validate().expect("60s = 2x heartbeat is the floor");
    }

    /// `RIO_LOG_MAX_CHUNKS_PER_EXEC=0` would make every stream abort at
    /// its FIRST cut: build logs silently stop being stored.
    #[test]
    fn validate_rejects_zero_log_max_chunks() {
        let cfg = Config {
            database_url: "postgres://x".into(),
            log_max_chunks_per_exec: 0,
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("log_max_chunks_per_exec"), "got: {err}");
    }

    /// A per-execution total-log cap below one chunk's cut threshold
    /// makes the threshold unreachable and aborts any log that size at
    /// the cap before it can cut a chunk.
    #[test]
    fn validate_rejects_log_byte_cap_below_cut_threshold() {
        let cfg = Config {
            database_url: "postgres://x".into(),
            log_ingest_byte_cap: 1024,
            log_cut_threshold_bytes: 4096,
            // Keep the budget ≥ 2× threshold so only the cap rule fires.
            log_bytes_budget: 8192,
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("log_ingest_byte_cap"), "got: {err}");
    }

    /// `RIO_LOG_RETENTION_DAYS=0` would make the hourly sweep delete
    /// every build log on its first tick.
    #[test]
    fn validate_rejects_zero_log_retention() {
        let cfg = Config {
            database_url: "postgres://x".into(),
            log_retention_days: 0,
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("log_retention_days"), "got: {err}");
    }

    /// A peer URL template without `{pod}` resolves every peer to the
    /// same URI — every cross-replica tail relays to one (probably
    /// wrong) pod.
    #[test]
    fn validate_rejects_peer_template_without_pod_placeholder() {
        let cfg = Config {
            database_url: "postgres://x".into(),
            log_peer_url_template: "http://rio-store:9002".into(),
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("log_peer_url_template"), "got: {err}");
    }

    /// The bracketed template `[{pod}]` was the pre-fail-closed
    /// default; uri_for now brackets IPv6 identities itself, so a
    /// pinned bracketed template would double-bracket every dial.
    #[test]
    fn validate_rejects_bracketed_pod_placeholder() {
        let cfg = Config {
            database_url: "postgres://x".into(),
            log_peer_url_template: "http://[{pod}]:9002".into(),
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("[{pod}]"),
            "the bracketed form must be rejected with the migration note; got: {err}"
        );
    }

    /// Empty = the cross-replica proxy is disabled, fail-closed; a
    /// deployment that wants the proxy must say where peers live.
    #[test]
    fn validate_accepts_empty_peer_template() {
        let cfg = Config {
            database_url: "postgres://x".into(),
            log_peer_url_template: String::new(),
            ..Default::default()
        };
        cfg.validate()
            .expect("empty template is the disabled posture");
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
    /// path (compiled defaults → `RIO_`-prefixed env with `__` nesting →
    /// deserialize). The deploy overlays set chunk_backend this way —
    /// regression guard for kustomization.yaml.
    ///
    /// The defaults layer serializes Inline as {kind: "inline"}; the
    /// defaults layer's per-key merge must correctly replace it with
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
    fn chunk_backend_kind_env_filesystem() {
        rio_test_support::Jail::expect_with(|jail| {
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
                "[chunk_backend] table must thread through the config layers"
            );
            assert!(
                cfg.jwt.required,
                "[jwt] table must thread through the config layers into JwtConfig"
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
            crate::cas::DEFAULT_CHUNK_UPLOAD_CONCURRENCY
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
