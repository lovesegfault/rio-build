//! StoreService + ChunkService gRPC server implementations.
//!
//! Submodules:
//! - `put_path` — write-ahead upload flow (Steps 1-6) + `common` shared
//!   with `put_path_batch`
//! - `put_path_batch` — atomic multi-output upload
//! - `get_path` — streaming NAR download (inline/chunked reassembly)
//! - `queries` — read RPCs (QueryPathInfo, FindMissingPaths, …)
//! - `sign` — narinfo signing + sig-visibility gate
//! - `chunk` — ChunkService (GetChunk)
//! - `admin` — StoreAdminService (GC, VerifyChunks, …)
//!
//! This file holds [`StoreServiceImpl`] (struct + builders), shared
//! status-mapping helpers, and the `StoreService` trait impl. Every
//! trait method delegates to an inherent `_impl` method in a submodule
//! so `self.pool`/`self.chunk_backend` field access works without
//! making fields `pub(super)`.

use std::sync::Arc;

use sqlx::PgPool;
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, error, instrument};

use rio_proto::StoreService;
use rio_proto::types::{
    AddSignaturesRequest, AddSignaturesResponse, AppendHwPerfSampleRequest,
    BatchGetManifestRequest, BatchGetManifestResponse, BatchQueryPathInfoRequest,
    BatchQueryPathInfoResponse, FindMissingPathsRequest, FindMissingPathsResponse, GetPathRequest,
    PathInfo, PutPathBatchRequest, PutPathBatchResponse, PutPathRequest, PutPathResponse,
    QueryPathFromHashPartRequest, QueryPathInfoRequest, QueryRealisationRequest, Realisation,
    RegisterRealisationRequest, RegisterRealisationResponse, TenantQuotaRequest,
    TenantQuotaResponse,
};
use rio_proto::validated::ValidatedPathInfo;

use rio_common::grpc::StatusExt;
use rio_common::limits::MAX_NAR_SIZE;

use crate::backend::ChunkBackend;
use crate::cas::{self, ChunkCache};
use crate::metadata;
use crate::signing::TenantSigner;
use crate::substitute::{SubstituteError, Substituter};

mod admin;
mod chunk;
mod directory;
mod drv_blob;
mod get_path;
mod put_path;
mod put_path_batch;
mod put_path_chunked;
mod queries;
mod sign;

pub use admin::{StoreAdminServiceImpl, spawn_store_gauge_tick};
pub use chunk::{ChunkServiceImpl, GET_CHUNKS_K};
pub use directory::DirectoryServiceImpl;
pub use drv_blob::DrvBlobServiceImpl;
pub use put_path::common::NarIngestEnvelopeCfg;

/// Default cap on paths in a FindMissingPaths request (DoS guard).
/// Matches `rio_nix::protocol::wire::MAX_COLLECTION_COUNT` — the gateway
/// is the trust boundary and already enforces 1M at wire-read time, so a
/// tighter store-side cap only rejects batches the gateway already
/// admitted (I-016: 10k→100k; I-130: 100k→1M after hello-deep-1024x sent
/// 153,934). 1M × ~80 bytes ≈ 80 MB worst case. Runtime-configurable via
/// `RIO_MAX_BATCH_PATHS` (StoreServiceImpl field). GC `extra_roots`
/// has its own separate cap (`MAX_GC_EXTRA_ROOTS` in `admin`).
pub const DEFAULT_MAX_BATCH_PATHS: usize = 1_048_576;

/// Validate a store path string: must parse as a well-formed Nix store path
/// (`/nix/store/<32-char-nixbase32>-<name>`). Rejects malformed paths, path
/// traversal attempts, and oversized strings at the RPC boundary.
pub(crate) fn validate_store_path(s: &str) -> Result<(), Status> {
    rio_nix::store_path::StorePath::parse(s)
        .map(|_| ())
        .status_invalid(&format!("invalid store path {s:?}"))
}

/// Map a storage-backend anyhow error to a Status, distinguishing
/// permanent auth/config failures from transient ones.
///
/// [`rio_common::grpc::internal`] maps everything to `Internal`, which a client
/// treats as retriable. For an STS AccessDenied (IRSA misconfigured,
/// IAM policy missing s3:PutObject) that means the builder retries
/// forever: the scheduler sees InfrastructureFailure, re-dispatches,
/// the builder rebuilds, the upload fails the same way, loop.
/// Observed: 12 derivations × 146 cycles in 6 minutes before manual
/// intervention.
///
/// Inspects the anyhow chain for [`BackendAuthError`] (set by
/// `S3ChunkBackend` ops when the SDK error matches known auth
/// signatures). If present → `FailedPrecondition` with a message that
/// names the fix. Otherwise → same as [`rio_common::grpc::internal`].
///
/// [`BackendAuthError`]: crate::backend::BackendAuthError
pub(crate) fn storage_error(context: &str, e: anyhow::Error) -> Status {
    error!(context, error = %e, "storage backend error");
    // downcast_ref checks the innermost source; BackendAuthError is
    // always the root (anyhow::Error::new(BackendAuthError).context(...)).
    if e.downcast_ref::<crate::backend::BackendAuthError>()
        .is_some()
    {
        Status::failed_precondition(
            "storage backend authentication failed; check S3 credentials/IAM permissions",
        )
    } else {
        Status::internal("storage operation failed")
    }
}

/// Map a `MetadataError` to a gRPC status with a precise code.
///
/// The key value of the typed error: retriable failures
/// (connection/serialization/placeholder-race) get retriable codes
/// (`unavailable`/`aborted`) so clients back off and retry; corruption
/// (invariant/malformed/corrupt-manifest) gets non-retriable codes so
/// clients fail fast. A flat everything-is-internal mapping would make a
/// transient PG hiccup look the same as a corrupt database.
///
/// Logs the full error (including sqlx source chain) server-side; the
/// gRPC message is a scrubbed summary.
pub(crate) fn metadata_status(context: &str, e: metadata::MetadataError) -> Status {
    use metadata::MetadataError as M;
    match &e {
        // I-145: serialization failure is an EXPECTED outcome under
        // concurrent write contention. Client retries on `aborted`;
        // logging at ERROR floods the log with spurious entries.
        M::Serialization => debug!(
            context,
            error = %e,
            "metadata layer: serialization conflict (client retries)"
        ),
        _ => error!(context, error = %e, "metadata layer error"),
    }
    match e {
        M::NotFound => Status::not_found("not found"),
        M::Conflict(_) => Status::already_exists("conflict: path already exists"),
        M::RealisationConflict {
            existing,
            attempted,
            ..
        } => Status::already_exists(format!(
            "realisation conflict: existing {existing}, attempted {attempted}"
        )),
        M::Connection(_) => Status::unavailable("database connection failed; retry"),
        M::Serialization => Status::aborted("transaction serialization failure; retry"),
        M::Deadlock(_) => Status::aborted("transaction deadlock detected; retry"),
        M::PlaceholderMissing { .. } => {
            Status::aborted("upload placeholder concurrently deleted; retry")
        }
        M::CorruptManifest { .. } => Status::data_loss("stored manifest data is corrupt"),
        // Backpressure: PG pool exhausted, signature count cap, etc.
        // Client should retry with backoff. Distinct from Connection
        // (unavailable → try-another-replica): this is "slow down",
        // not "go elsewhere".
        M::ResourceExhausted(msg) => Status::resource_exhausted(msg),
        M::InvariantViolation(_) | M::MalformedRow(_) | M::Other(_) => {
            Status::internal("storage operation failed")
        }
    }
}

/// PutPath-scoped wrapper around [`metadata_status`]: increments
/// `rio_store_putpath_retries_total{reason}` for retriable variants
/// (the ones that map to `aborted`/`unavailable` and which the worker
/// upload loop retries) before delegating. Same I-145 site as the
/// log-level special-case above; separate fn because `metadata_status`
/// is called from read RPCs (QueryPathInfo etc.) where the counter
/// would be a misnomer.
pub(crate) fn putpath_metadata_status(context: &str, e: metadata::MetadataError) -> Status {
    use metadata::MetadataError as M;
    let reason = match &e {
        M::Serialization => Some("serialization"),
        M::Deadlock(_) => Some("deadlock"),
        M::PlaceholderMissing { .. } => Some("placeholder_missing"),
        M::Connection(_) => Some("connection"),
        M::ResourceExhausted(_) => Some("resource_exhausted"),
        // Non-retriable (NotFound/Conflict/Invariant/Malformed/Corrupt/
        // Other) — not counted; the client won't retry an `internal`/
        // `data_loss`/`already_exists`.
        _ => None,
    };
    if let Some(reason) = reason {
        metrics::counter!("rio_store_putpath_retries_total", "reason" => reason).increment(1);
    }
    metadata_status(context, e)
}

/// Resolve the caller's tenant from a verified identity: gateway JWT
/// (`TenantClaims.sub`) first, else the HMAC assignment token's
/// `tenant` claim parsed as a UUID. `None` when neither carries a
/// tenant (dev mode, service-token caller) or the claim does not
/// parse.
///
/// This is the ONE mapping from caller identity to tenant, shared by
/// the castore read side ([`directory::DirectoryServiceImpl`]'s
/// `castore_tenant_id`) and the upload write side (`PutPathChunked`'s
/// `path_tenants` junction inserts) — the read queries join
/// `path_tenants` on exactly the tenant resolved here, so a write side
/// that resolved the tenant differently (e.g. JWT-only) would commit
/// paths its own uploader cannot read back.
// r[impl store.castore.tenant-scope+3]
fn resolve_tenant_id(
    jwt_sub: Option<uuid::Uuid>,
    hmac_claims: Option<&rio_auth::hmac::AssignmentClaims>,
) -> Option<uuid::Uuid> {
    jwt_sub.or_else(|| hmac_claims?.tenant.as_deref()?.parse().ok())
}

/// Drive a streaming-RPC drain future to completion, bounded by
/// [`rio_common::grpc::GRPC_STREAM_TIMEOUT`].
///
/// Every server-streaming RPC spawns its producer task and hands tonic
/// the channel receiver. A half-open client otherwise parks that task
/// on `tx.send()` forever, pinning whatever the producer has buffered —
/// `tonic_builder()` sets no h2 keepalive, so this stream timeout is
/// the only backstop. On timeout: warn (RPC name + timeout) and push a
/// `DEADLINE_EXCEEDED` into the stream for a client that is still
/// reading.
///
/// The budget is ABSOLUTE — right for the streams this guards, whose
/// total size is bounded (one NAR, one directory walk, one blob). It is
/// wrong for `GetChunks`, where the client reuses one bidi stream for a
/// whole file's fill and lifetime scales with file size: that RPC has
/// its own IDLE-based watchdog (`chunk.rs`, `stream_idle_timeout`).
///
/// Returns `Some(output)` when the drain completed within the timeout,
/// `None` when it timed out — callers layer site-specific logging or
/// success-path metrics on top.
pub(super) async fn drain_with_timeout<T, F: Future>(
    rpc: &'static str,
    tx: &tokio::sync::mpsc::Sender<Result<T, Status>>,
    fut: F,
) -> Option<F::Output> {
    match tokio::time::timeout(rio_common::grpc::GRPC_STREAM_TIMEOUT, fut).await {
        Ok(out) => Some(out),
        Err(_) => {
            tracing::warn!(
                rpc,
                timeout = ?rio_common::grpc::GRPC_STREAM_TIMEOUT,
                "stream timed out"
            );
            let _ = tx
                .send(Err(Status::deadline_exceeded(format!(
                    "{rpc} stream timeout"
                ))))
                .await;
            None
        }
    }
}

/// The StoreService gRPC server.
///
/// NAR content lives in `manifests.inline_blob` (small NARs) or as
/// FastCDC chunks (large NARs). Inline blobs are stored directly in PG.
pub struct StoreServiceImpl {
    pool: PgPool,
    /// Chunk storage for NARs ≥ INLINE_THRESHOLD. `None` disables chunking
    /// entirely (all NARs go inline, regardless of size).
    chunk_backend: Option<Arc<dyn ChunkBackend>>,
    /// Cache for chunk reads (GetPath). Created once at construction;
    /// shared across all GetPath calls (the moka LRU and singleflight map
    /// are process-wide). `None` iff `chunk_backend` is None — they're
    /// paired.
    ///
    /// `Arc` because the spawned GetPath streaming task needs an owned
    /// handle (the task outlives the `&self` method call).
    chunk_cache: Option<Arc<ChunkCache>>,
    /// Tenant-aware ed25519 signer for narinfo. Wraps the cluster
    /// `Signer` + PG pool for per-tenant key lookup. `None` = signing
    /// disabled (paths stored without our signature; still serveable,
    /// just unverified). Arc because both PutPath branches need it and
    /// the inline branch doesn't have a good place to hold a reference
    /// across the await.
    signer: Option<Arc<TenantSigner>>,
    /// HMAC verifier for assignment tokens on PutPath. When Some, a
    /// PutPath without a valid `x-rio-assignment-token` metadata
    /// header → PERMISSION_DENIED. When Some + valid token: the
    /// uploaded path must be in `claims.expected_outputs`.
    ///
    /// None = accept all callers (dev mode, same as pre-Phase-3b).
    hmac_verifier: Option<Arc<rio_auth::hmac::HmacVerifier>>,
    /// HMAC verifier for `x-rio-service-token` (SEPARATE key from
    /// `hmac_verifier`). When Some + token verifies + `caller` is in
    /// `service_bypass_callers` → skip the assignment-token check.
    /// Transport-agnostic — see [`rio_auth::hmac::ServiceClaims`].
    service_verifier: Option<Arc<rio_auth::hmac::HmacVerifier>>,
    /// Service-token callers that may skip the assignment-token check
    /// via a valid `x-rio-service-token`. Default
    /// `["rio-gateway", "rio-scheduler"]`.
    service_bypass_callers: Vec<String>,
    /// Global budget for in-flight NAR bytes across ALL concurrent PutPath
    /// AND upstream-substitution handlers. Each acquires `chunk.len()`
    /// permits before extending its `nar_data: Vec<u8>`; permits release on
    /// handler drop. Default `8 * MAX_NAR_SIZE` (32 GiB) — lets 8× max-size
    /// uploads run in parallel before the 9th blocks. Configurable via
    /// `store.toml nar_buffer_budget_bytes` (or `.with_nar_budget()` in
    /// tests). main.rs wires this same sealed [`crate::budget::NarBudget`]
    /// into the `Substituter` so both ingest paths draw from one pool
    /// under one tenant ledger (the raw semaphore is module-private
    /// to `crate::budget` — merged_bug_005's seal).
    ///
    /// NOT shared with GetPath's chunk cache — that's moka-bounded separately
    /// (chunk_cache above). This bounds ONLY the per-request accumulation
    /// Vec, which is the OOM vector: 10 × 4 GiB = 40 GiB RSS.
    // r[impl store.put.nar-bytes-budget+6]
    // r[impl store.budget.cost-axis]
    nar_budget: crate::budget::NarBudget,
    /// Typed envelope knobs for the ingest plane's budget waits and
    /// holds ([`NarIngestEnvelopeCfg`]) — wait grace at the
    /// `accumulate_chunk` chokepoint, hold envelope over stream
    /// residency and the persist span. Production default; tests
    /// shrink via `.with_nar_ingest_envelope()` (the R17 violability
    /// lane).
    nar_ingest_envelope: NarIngestEnvelopeCfg,
    /// Upstream binary-cache substituter. `None` disables substitution
    /// (QueryPathInfo/GetPath miss → NotFound immediately, pre-P0462
    /// behavior). `Some` → on miss, try each of the requesting tenant's
    /// configured upstreams before returning NotFound.
    substituter: Option<Arc<Substituter>>,
    /// Max concurrent S3 chunk uploads per `put_chunked` call. Bounds
    /// the PutPath→S3 fan-out so a single large NAR (>1000 chunks)
    /// doesn't saturate the aws-sdk connection pool. Default
    /// [`cas::DEFAULT_CHUNK_UPLOAD_CONCURRENCY`] (8); override via
    /// `.with_chunk_upload_max_concurrent()`.
    chunk_upload_max_concurrent: usize,
    /// Cap on paths in a FindMissingPaths request (DoS guard). Default
    /// [`DEFAULT_MAX_BATCH_PATHS`] (1M); override via
    /// `.with_max_batch_paths()`.
    max_batch_paths: usize,
    /// `GetPath` chunk-prefetch depth: max in-flight `get_verified()`
    /// futures inside `.buffered()`. Throughput ceiling on a cold moka
    /// cache is `K × CHUNK_AVG / s3_ttfb` — at K=8 (the old hardcoded
    /// value), cold S3-standard (~45 ms TTFB) caps at ~11 MB/s and
    /// real-world ~1.4 MB/s under load, making a 250 MB path take ~3 m.
    /// Default [`DEFAULT_CHUNK_PREFETCH_K`] (64); per-stream memory
    /// cost is `K × CHUNK_MAX` (≤ 16 MiB at 64). Override via
    /// `.with_chunk_prefetch_k()`.
    chunk_prefetch_k: usize,
    /// Count of GetPath body-stream tasks currently writing. Incremented
    /// synchronously in `stream_path` BEFORE the response is returned,
    /// decremented on the spawned task's drop (any exit path).
    /// `main.rs` polls this via [`wait_for_active_drain`] in the
    /// `spawn_drain_task_ext` after-grace hook so SIGTERM doesn't tear
    /// down the listener while a multi-second NAR stream is mid-flight
    /// — the contract `componentscaler/decide.rs MAX_SCALE_DOWN_STEP`
    /// already assumes.
    ///
    /// [`wait_for_active_drain`]: rio_common::server::wait_for_active_drain
    active_get_path_streams: Arc<std::sync::atomic::AtomicUsize>,
    /// Bounded budget for waiting out a concurrent same-path uploader
    /// before surfacing `ABORTED` (`r[store.put.concurrent-wait]`).
    /// Default [`DEFAULT_CONCURRENT_PUT_WAIT`]; tests use millisecond
    /// budgets via `.with_concurrent_put_wait()`.
    concurrent_put_wait: std::time::Duration,
}

/// Default `GetPath` chunk-prefetch depth. See
/// [`StoreServiceImpl::with_chunk_prefetch_k`].
pub const DEFAULT_CHUNK_PREFETCH_K: usize = 64;

/// Default budget for waiting out a concurrent same-path uploader
/// (`r[store.put.concurrent-wait]`). Sized to cover a typical chunked
/// S3 upload of a large NAR (tens of seconds) while staying well under
/// `rio_common::grpc::GRPC_STREAM_TIMEOUT` (300 s) so a waiting loser
/// resolves inside the client's own RPC deadline — the gateway's
/// re-send retry (`gw.put.aborted-retry`, ~6 s spread over 8 attempts)
/// then multiplies the effective coverage for pathologically slow
/// winners.
pub const DEFAULT_CONCURRENT_PUT_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// Default global NAR buffer budget: 8 × MAX_NAR_SIZE (32 GiB on 64-bit).
/// `tokio::sync::Semaphore` max permits is `usize::MAX >> 3`; this fits
/// comfortably on 64-bit. Cast of the shared
/// [`rio_common::limits::DEFAULT_STORE_NAR_BUDGET_BYTES`] — the same
/// constant the xtask deploy derives the memory limit from (D4:
/// `limit := budget + STORE_NON_NAR_RESERVE_BYTES`), so the binary
/// default and the deployed limit cannot drift apart.
pub(crate) const DEFAULT_NAR_BUDGET: usize =
    rio_common::limits::DEFAULT_STORE_NAR_BUDGET_BYTES as usize;

// E-2(ii) backstop (live_047/R-C): the None-default budget admits one
// whole-NAR substitution reservation; the config-plane premise is the
// validate() floor `nar_buffer_budget_bytes >= MAX_NAR_SIZE`.
const _: () = assert!(MAX_NAR_SIZE as usize <= DEFAULT_NAR_BUDGET);

// The declared-mode tenant cap sits below this plane's default pool
// (so the cap — not the pool — is the binding constraint for one
// tenant; merged_bug_005). The substitute plane pins the same
// relation against its own pool const.
const _: () = assert!(crate::budget::TENANT_RESERVATION_CAP as usize <= DEFAULT_NAR_BUDGET);

/// Witness: this request is NOT an end-user tenant session — the
/// deny-tenants polarity check passed (`reject_end_user_tenant`).
/// The builder-internal batch data fetches REQUIRE one, so the
/// deliberate sig-visibility gate-skip on those RPCs cannot become a
/// tenant-side bypass by arm deletion: a batch handler without the
/// check does not compile.
#[must_use]
pub(crate) struct EndUserRejected(());

/// Witness for scheduler-managed realisation writes: a VERIFIED
/// allowlisted service caller, or dev mode (no service verifier
/// configured). Sole producer: `require_service_caller`.
#[must_use]
pub(crate) enum ServiceCallerOk {
    /// HMAC-verified `x-rio-service-token` from an allowlisted caller.
    Verified {
        /// The verified `ServiceClaims.caller`.
        #[allow(dead_code)] // named for tracing/debug; policy is the variant
        caller: String,
    },
    /// No service verifier configured (dev mode) — accept.
    DevMode,
}

impl StoreServiceImpl {
    /// Create a new StoreService with inline-only storage (no chunking).
    ///
    /// All NARs go into `manifests.inline_blob` regardless of size.
    /// Existing test harnesses call this; they don't need a chunk backend.
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            chunk_backend: None,
            chunk_cache: None,
            signer: None,
            hmac_verifier: None,
            service_verifier: None,
            service_bypass_callers: vec!["rio-gateway".to_string(), "rio-scheduler".to_string()],
            nar_budget: crate::budget::NarBudget::new(DEFAULT_NAR_BUDGET),
            nar_ingest_envelope: NarIngestEnvelopeCfg::default(),
            substituter: None,
            chunk_upload_max_concurrent: cas::DEFAULT_CHUNK_UPLOAD_CONCURRENCY,
            max_batch_paths: DEFAULT_MAX_BATCH_PATHS,
            chunk_prefetch_k: DEFAULT_CHUNK_PREFETCH_K,
            active_get_path_streams: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            concurrent_put_wait: DEFAULT_CONCURRENT_PUT_WAIT,
        }
    }

    /// Attach an externally-owned `ChunkCache`. Builder-style.
    ///
    /// The cache carries its backend inside (accessible via
    /// `ChunkCache::backend()`). StoreServiceImpl extracts it for
    /// the write path — PutPath calls `backend.put()` directly
    /// (no point caching freshly-written chunks nothing asked for).
    ///
    /// Use this when you want ONE cache shared across multiple
    /// services. main.rs constructs one `Arc<ChunkCache>`, passes
    /// clones here + to `ChunkServiceImpl::new` — a chunk warmed by
    /// either is hot for both. Without this call, the service is inline-only (all
    /// NARs go into `manifests.inline_blob` regardless of size).
    pub fn with_chunk_cache(mut self, cache: Arc<ChunkCache>) -> Self {
        self.chunk_backend = Some(cache.backend());
        self.chunk_cache = Some(cache);
        self
    }

    /// Enable upstream binary-cache substitution. Builder-style.
    /// Without this, QueryPathInfo/GetPath miss → NotFound directly.
    pub fn with_substituter(mut self, substituter: Arc<Substituter>) -> Self {
        self.substituter = Some(substituter);
        self
    }

    /// Enable HMAC verification on PutPath assignment tokens.
    /// Builder-style — chains after `new()` or `with_chunk_cache()`.
    /// Takes `Arc` so `main.rs` shares one verifier with
    /// `DirectoryServiceImpl` (one copy of the key bytes in memory).
    pub fn with_hmac_verifier(mut self, verifier: Arc<rio_auth::hmac::HmacVerifier>) -> Self {
        self.hmac_verifier = Some(verifier);
        self
    }

    /// Enable `x-rio-service-token` verification on PutPath. Builder-
    /// style. Verifier is keyed with `RIO_SERVICE_HMAC_KEY_PATH`
    /// (NOT the assignment key — separate secret). Takes `Arc` so
    /// `main.rs` can share one verifier with `StoreAdminServiceImpl`.
    pub fn with_service_hmac_verifier(
        mut self,
        verifier: Arc<rio_auth::hmac::HmacVerifier>,
    ) -> Self {
        self.service_verifier = Some(verifier);
        self
    }

    /// Set the `ServiceClaims.caller` allowlist for service-token
    /// bypass. Replaces the constructor default
    /// (`["rio-gateway", "rio-scheduler"]`).
    pub fn with_service_bypass_callers(mut self, callers: Vec<String>) -> Self {
        self.service_bypass_callers = callers;
        self
    }

    /// Enable narinfo signing with the given tenant-aware signer.
    ///
    /// Builder-style: `StoreServiceImpl::new(pool).with_signer(ts)`.
    /// Chains after either `new()` or `with_chunk_cache()`. The
    /// `TenantSigner` wraps the cluster key + pool — per-tenant key
    /// lookup happens at sign time, not construction time.
    pub fn with_signer(mut self, signer: TenantSigner) -> Self {
        self.signer = Some(Arc::new(signer));
        self
    }

    /// Borrow the signer Arc (PutPathBatch resolves tenant→key once
    /// per stream, not per output). Returning `Option<&Arc>` keeps
    /// the Arc-wrapping detail internal.
    pub fn signer(&self) -> Option<&Arc<TenantSigner>> {
        self.signer.as_ref()
    }

    /// Override the global NAR buffer budget (total permits across all
    /// concurrent PutPath handlers). Builder-style. Tests use small values
    /// (e.g., `10 * 4096`) to exercise backpressure without 32 GiB of RAM.
    pub fn with_nar_budget(mut self, bytes: usize) -> Self {
        self.nar_budget =
            crate::budget::NarBudget::new(rio_common::semaphore_permits(bytes as u64));
        self
    }

    /// Override the per-call chunk-upload concurrency bound. Builder-style.
    /// main.rs threads `RIO_CHUNK_UPLOAD_MAX_CONCURRENT` here. Tests can
    /// pass small N to exercise the bound without thousands of chunks.
    pub fn with_chunk_upload_max_concurrent(mut self, n: usize) -> Self {
        self.chunk_upload_max_concurrent = n;
        self
    }

    /// Override the FindMissingPaths batch-size cap. Builder-style.
    /// main.rs threads `RIO_MAX_BATCH_PATHS` here.
    pub fn with_max_batch_paths(mut self, n: usize) -> Self {
        self.max_batch_paths = n;
        self
    }

    /// Override the GetPath chunk-prefetch depth. Builder-style.
    /// main.rs threads `RIO_CHUNK_PREFETCH_K` here.
    pub fn with_chunk_prefetch_k(mut self, k: usize) -> Self {
        self.chunk_prefetch_k = k;
        self
    }

    /// Override the concurrent same-path upload wait budget
    /// (`r[store.put.concurrent-wait]`). Builder-style. Tests use
    /// millisecond budgets to exercise the bounded-timeout path without
    /// real waiting.
    pub fn with_concurrent_put_wait(mut self, budget: std::time::Duration) -> Self {
        self.concurrent_put_wait = budget;
        self
    }

    /// Handle to the active-GetPath-stream counter for SIGTERM drain.
    /// main.rs passes this to `wait_for_active_drain` in the
    /// `spawn_drain_task_ext` after-grace hook.
    pub fn active_get_path_streams_handle(&self) -> Arc<std::sync::atomic::AtomicUsize> {
        Arc::clone(&self.active_get_path_streams)
    }

    /// Borrow the sealed NAR budget. main.rs clones this into the
    /// `Substituter` so PutPath and substitution share ONE pool under
    /// ONE tenant ledger (clones share identity — merged_bug_005);
    /// tests inspect `available_permits` to assert backpressure (the
    /// read face; reads are not debits).
    pub fn nar_budget(&self) -> &crate::budget::NarBudget {
        &self.nar_budget
    }

    /// Verify `x-rio-service-token` and return the allowlisted caller
    /// name. `None` when: no verifier configured, no header present,
    /// signature/expiry invalid, or `caller` not in
    /// [`Self::service_bypass_callers`]. Used by both the PutPath
    /// HMAC-bypass and the [`Self::request_tenant_id`]
    /// `x-rio-probe-tenant-id` gate.
    fn verified_service_caller<T>(&self, request: &Request<T>) -> Option<String> {
        let sv = self.service_verifier.as_ref()?;
        let tok = request
            .metadata()
            .get(rio_proto::SERVICE_TOKEN_HEADER)
            .and_then(|v| v.to_str().ok())?;
        let claims = sv.verify::<rio_auth::hmac::ServiceClaims>(tok).ok()?;
        self.service_bypass_callers
            .iter()
            .any(|a| a == &claims.caller)
            .then_some(claims.caller)
    }

    /// Extract `tenant_id` for substitution / tenant-filtering.
    ///
    /// Precedence:
    /// 1. JWT-interceptor extension (`x-rio-tenant-token` verified by
    ///    `rio_auth::jwt_interceptor`) — the gateway-forwarded path.
    /// 2. `x-rio-probe-tenant-id` header — ONLY honoured when the
    ///    request also carries a valid `x-rio-service-token` from an
    ///    allowlisted caller. Lets the scheduler (which can't forward a
    ///    JWT at dispatch time — `r[sched.dispatch.fod-substitute]`)
    ///    assert tenant context without opening an unauthenticated
    ///    self-select gap.
    ///
    /// `Ok(None)` ONLY when no tenant context was REQUESTED (no JWT,
    /// no probe header): downstream runs anonymous by design.
    ///
    /// merged_bug_003 (owner-signed Q3, bughunt-3): a probe header
    /// that IS present while its service token is absent,
    /// unverifiable (expiry/HMAC/verifier unset), not allowlisted, or
    /// the header unparseable, REJECTS `UNAUTHENTICATED` — never the
    /// silent anonymous downgrade. The downgrade ran the probe
    /// tenant-blind and answered missing-with-empty-substitutable,
    /// wire-identical to confirmed 404s: routine HMAC-rotation skew
    /// folded to ConfirmedMissing → terminal FailFast of substitutable
    /// builds, and the anonymous pass-through re-opened the
    /// cross-tenant sig-visibility laundering the per-tenant probe
    /// partition exists to prevent (capability fault answering in the
    /// honest path's type — hazard (eeee)'s RPC-boundary form). The
    /// scheduler's probe call maps this refusal to conservative
    /// ReArm.
    ///
    /// ── SIGNED 2026-06-07 (owner, bughunt-3 fix-wave §5-S Q3) ──
    /// A present-but-unverifiable service token at the store boundary
    /// is an UNAUTHENTICATED refusal — fail closed; an ABSENT token
    /// stays anonymous as today. The refusal is an uncharged
    /// capability fault, never evidence. `FindMissingPathsResponse`
    /// gains `probe_ran_tenant_scoped` (types.proto field 4) so
    /// `can_confirm` derives from the response, never sender intent
    /// (both halves: posture + structure). Leading alternative (b)
    /// (report-all-indeterminate on verification failure) was
    /// REJECTED: quieter under rotation skew but leaves `can_confirm`
    /// sender-derived. ─────────────────────────────────────────────
    fn request_tenant_id<T>(&self, request: &Request<T>) -> Result<Option<uuid::Uuid>, Status> {
        if let Some(jwt) = request
            .extensions()
            .get::<rio_auth::jwt::TenantClaims>()
            .map(|c| c.sub)
        {
            return Ok(Some(jwt));
        }
        // r[impl sched.dispatch.fod-substitute+3]
        let Some(hdr) = request
            .metadata()
            .get(rio_proto::PROBE_TENANT_ID_HEADER)
            .and_then(|v| v.to_str().ok())
        else {
            // No tenant context requested — anonymous by design.
            return Ok(None);
        };
        // r[impl store.substitute.unverifiable-token-rejects]
        if self.verified_service_caller(request).is_none() {
            return Err(Status::unauthenticated(
                "x-rio-probe-tenant-id present but the service token is \
                 absent, unverifiable, or not allowlisted — refusing the \
                 tenant-blind downgrade (rotated service HMAC?)",
            ));
        }
        hdr.parse().map(Some).map_err(|_| {
            Status::unauthenticated("x-rio-probe-tenant-id present but not a valid UUID")
        })
    }

    // r[impl store.api.batch-query+2]
    // r[impl store.api.batch-manifest+3]
    /// Reject gateway-forwarded end-user tenant tokens on the
    /// builder-internal batch RPCs (`BatchQueryPathInfo`,
    /// `BatchGetManifest`). These intentionally skip
    /// `r[store.substitute.tenant-sig-visibility]` (the gate would add
    /// per-path PG hits and defeat I-110); turning the documented
    /// "builder is the only caller, sends no token" into an enforced
    /// invariant means the skip can't be exploited as a tenant-side
    /// gate bypass.
    ///
    /// Anonymous (builder, no token) and service-token-with-probe
    /// (scheduler) pass through — only the JWT-interceptor extension
    /// (an `x-rio-tenant-token` the gateway forwarded from an ssh-ng
    /// client) is rejected. Zero PG cost.
    fn reject_end_user_tenant<T>(
        &self,
        request: &Request<T>,
        rpc: &'static str,
    ) -> Result<EndUserRejected, Status> {
        if request
            .extensions()
            .get::<rio_auth::jwt::TenantClaims>()
            .is_some()
        {
            return Err(Status::permission_denied(format!(
                "{rpc} is builder-internal; use the per-path RPC for tenant-scoped reads"
            )));
        }
        Ok(EndUserRejected(()))
    }

    /// Witness-producing service-caller gate for scheduler-managed
    /// write RPCs (`RegisterRealisation`). Dev-mode (`service_verifier
    /// = None`) passes through as its own variant, matching the
    /// `hmac_verifier=None` semantics elsewhere — production always
    /// configures both (and boot key-coherence refuses the half-keyed
    /// JWT states).
    fn require_service_caller<T>(
        &self,
        request: &Request<T>,
        denial: &'static str,
    ) -> Result<ServiceCallerOk, Status> {
        if self.service_verifier.is_none() {
            return Ok(ServiceCallerOk::DevMode);
        }
        match self.verified_service_caller(request) {
            Some(caller) => Ok(ServiceCallerOk::Verified { caller }),
            None => Err(Status::permission_denied(denial)),
        }
    }

    /// Caller-identity gate for the NAR-index RPCs (`GetNarIndex`,
    /// `GetNarIndexBatch`) — same ladder as
    /// `ChunkServiceImpl::require_caller_identity`. The index is a
    /// path's complete file listing with sizes and per-file BLAKE3
    /// digests, and nar hashes travel separately from the content they
    /// name, so serving it anonymously is a cross-tenant metadata
    /// oracle. Identity-only, like the chunk gate: the index namespace
    /// is content-addressed and deliberately not tenant-scoped; JWT
    /// callers are additionally refused by
    /// `Self::reject_end_user_tenant`.
    // r[impl store.index.rpc+1]
    fn require_caller_identity<T>(&self, request: &Request<T>) -> Result<(), Status> {
        // The verified identity is discarded — only its existence
        // matters here.
        directory::caller_identity(
            request,
            self.hmac_verifier.as_ref(),
            "GetNarIndex requires a caller identity: send a JWT or an HMAC \
             assignment token",
        )
        .map(|_| ())
    }

    // r[impl store.substitute.upstream]
    /// On local miss: if the tenant has upstreams configured, try
    /// substituting. Returns `Ok(Some)` if fetched+ingested, `Ok(None)`
    /// on miss or if substitution is disabled/tenant-less.
    async fn try_substitute_on_miss(
        &self,
        tenant_id: Option<uuid::Uuid>,
        store_path: &str,
    ) -> Result<Option<ValidatedPathInfo>, Status> {
        let (Some(sub), Some(tid)) = (&self.substituter, tenant_id) else {
            return Ok(None);
        };
        // Admission gating happens INSIDE `try_substitute`'s moka init
        // future (leader-only); the no-substituter early-return above
        // is the only pre-gate filter this layer needs.
        sub.try_substitute(tid, store_path).await.map_err(|e| {
            tracing::warn!(error = %e, store_path, "substitution failed");
            substitute_status(e)
        })
    }

    /// Clean up an uploading placeholder after a PutPath error and record
    /// the error metric. Call this on any error path AFTER
    /// `insert_manifest_uploading` returned `Some(claim)` (i.e., we own the
    /// placeholder).
    async fn abort_upload(&self, store_path_hash: &[u8], claim: uuid::Uuid) {
        crate::ingest::abort_placeholder(&self.pool, store_path_hash, claim).await;
        metrics::counter!("rio_store_put_path_total", "result" => "error").increment(1);
    }
}

/// Map a [`SubstituteError`] that escaped `do_substitute`'s per-upstream
/// loop to a gRPC `Status`. Caller: `try_substitute_on_miss`
/// (QueryPathInfo/GetPath unary fallback).
///
/// HashMismatch/SizeMismatch never reach here in practice: per-upstream
/// integrity errors are swallowed as try-next-upstream inside
/// `do_substitute`. Only errors that abort the whole substitution reach
/// this arm.
pub(super) fn substitute_status(e: SubstituteError) -> Status {
    match e {
        SubstituteError::Fetch(_) => Status::unavailable("upstream substitute fetch failed"),
        SubstituteError::TooLarge { what, limit } => Status::resource_exhausted(format!(
            "upstream substitute {what} exceeds {limit}-byte cap"
        )),
        // Raced: cross-replica/PutPath uploader holds the placeholder
        // (same-replica callers coalesce at moka and never reach
        // `claim_placeholder` concurrently). The NAR fetch may still be
        // seconds–minutes away — the gateway's 2-attempt/250ms
        // `r[gw.store.transient-retry]` budget can't outlast it, so
        // `Unavailable` here would surface a hard error where the
        // pre-substitute behaviour was a benign `valid=false`. Map to
        // `NotFound` instead: gateway treats it as miss (caller
        // re-probes later). The walk-era scheduler caller died with
        // Phase D-prime; the materialization executor's walk runs
        // in-process against the substituter and never crosses this
        // gRPC mapping. Moka didn't cache `Err` either way.
        SubstituteError::Raced => Status::not_found("substitution in progress on another replica"),
        // Owner-side stall abort (`r[store.substitute.stall-abort]`):
        // the claim was released in place, so a retry re-claims
        // immediately — `Unavailable` (transient, retryable), the same
        // shape as an upstream fetch failure. Never a miss: the path
        // may well exist upstream; only this download wedged.
        SubstituteError::Stalled { .. } => {
            Status::unavailable("upstream download stalled; claim released — retry")
        }
        // Hold-envelope abort (the budget law's clock, merged_bug_021):
        // the leg held NAR-budget permits past its typed transfer
        // deadline — an adversarial trickle or a black-holed persist.
        // Same transient-retryable posture as `Stalled` (the claim is
        // aborted on the same error path; a retry re-claims), and the
        // budget permits were credited back by drop, so the retry can
        // make progress.
        SubstituteError::HoldDeadlineExceeded { .. } => {
            Status::unavailable("nar-budget hold exceeded its transfer deadline; retry")
        }
        // Cost-axis refusal (the budget law's per-tenant cap): a local
        // capacity refusal, typed retryable — same posture as the
        // admission gate's saturation answer.
        SubstituteError::TenantBudgetExhausted { .. } => {
            Status::resource_exhausted("tenant nar-budget reservation cap reached; retry")
        }
        // Upstream-429: genuinely transient — `Unavailable` so the
        // scheduler's 8-attempt backoff retries. A bare 429 (no
        // `Retry-After`) is STILL a rate-limit, not a miss; the
        // previous `Busy{None}` overload conflated it with `Raced` and
        // demoted to build-from-source.
        SubstituteError::RateLimited { .. } => Status::unavailable("upstream rate-limited; retry"),
        SubstituteError::Admission(a) => a.into(),
        // merged_bug_005: present-but-untrusted is a typed refusal —
        // `FailedPrecondition` (the tenant's trusted_keys don't match
        // what the upstream serves; fixable by configuration, not by
        // retry). Never `NotFound`: the path IS present upstream, and
        // a miss answer here would re-launder the refusal one RPC up.
        SubstituteError::UntrustedPresent => Status::failed_precondition(
            "upstream narinfo present but no signature verified against trusted_keys",
        ),
        // merged_bug_046: stored-row/upstream content disagreement is
        // the same typed-refusal posture one axis over —
        // `FailedPrecondition` (this upstream claims different bytes
        // than the stored row; fixable by upstream content or
        // configuration, not by retry). Never `NotFound`: a miss
        // answer here would re-launder the refusal one RPC up.
        SubstituteError::ContentMismatch => Status::failed_precondition(
            "upstream narinfo claims different bytes than the stored row",
        ),
        SubstituteError::HashMismatch { .. }
        | SubstituteError::SizeMismatch { .. }
        | SubstituteError::NarInfo(_)
        | SubstituteError::Ingest(_) => Status::internal("substitute ingest failed"),
    }
}

#[tonic::async_trait]
impl StoreService for StoreServiceImpl {
    /// Upload a store path. See the `put_path` module for the write-ahead flow.
    #[instrument(skip(self, request), fields(rpc = "PutPath"))]
    async fn put_path(
        &self,
        request: Request<Streaming<PutPathRequest>>,
    ) -> Result<Response<PutPathResponse>, Status> {
        self.put_path_impl(request).await
    }

    /// Upload multiple store paths atomically. See the `put_path_batch`
    /// module for the one-transaction flow.
    #[instrument(skip(self, request), fields(rpc = "PutPathBatch"))]
    async fn put_path_batch(
        &self,
        request: Request<Streaming<PutPathBatchRequest>>,
    ) -> Result<Response<PutPathBatchResponse>, Status> {
        self.put_path_batch_impl(request).await
    }

    /// ADR-022 §6 chunked output upload. See the `put_path_chunked`
    /// module for the validate → verify → commit flow.
    #[instrument(skip(self, request), fields(rpc = "PutPathChunked"))]
    async fn put_path_chunked(
        &self,
        request: Request<Streaming<rio_proto::types::PutPathChunkedRequest>>,
    ) -> Result<Response<rio_proto::types::PutPathChunkedResponse>, Status> {
        self.put_path_chunked_impl(request).await
    }

    type GetPathStream = get_path::GetPathStream;

    /// Download a store path's NAR. See the `get_path` module for the streaming flow.
    #[instrument(skip(self, request), fields(rpc = "GetPath"))]
    async fn get_path(
        &self,
        request: Request<GetPathRequest>,
    ) -> Result<Response<Self::GetPathStream>, Status> {
        self.get_path_impl(request).await
    }

    /// Query metadata for a single store path. See the `queries` module.
    #[instrument(skip(self, request), fields(rpc = "QueryPathInfo"))]
    async fn query_path_info(
        &self,
        request: Request<QueryPathInfoRequest>,
    ) -> Result<Response<PathInfo>, Status> {
        self.query_path_info_impl(request).await
    }

    #[instrument(skip(self, request), fields(rpc = "BatchQueryPathInfo"))]
    async fn batch_query_path_info(
        &self,
        request: Request<BatchQueryPathInfoRequest>,
    ) -> Result<Response<BatchQueryPathInfoResponse>, Status> {
        self.batch_query_path_info_impl(request).await
    }

    #[instrument(skip(self, request), fields(rpc = "BatchGetManifest"))]
    async fn batch_get_manifest(
        &self,
        request: Request<BatchGetManifestRequest>,
    ) -> Result<Response<BatchGetManifestResponse>, Status> {
        self.batch_get_manifest_impl(request).await
    }

    #[instrument(skip(self, request), fields(rpc = "FindMissingPaths"))]
    async fn find_missing_paths(
        &self,
        request: Request<FindMissingPathsRequest>,
    ) -> Result<Response<FindMissingPathsResponse>, Status> {
        self.find_missing_paths_impl(request).await
    }

    #[instrument(skip(self, request), fields(rpc = "QueryPathFromHashPart"))]
    async fn query_path_from_hash_part(
        &self,
        request: Request<QueryPathFromHashPartRequest>,
    ) -> Result<Response<PathInfo>, Status> {
        self.query_path_from_hash_part_impl(request).await
    }

    #[instrument(skip(self, request), fields(rpc = "AddSignatures"))]
    async fn add_signatures(
        &self,
        request: Request<AddSignaturesRequest>,
    ) -> Result<Response<AddSignaturesResponse>, Status> {
        self.add_signatures_impl(request).await
    }

    #[instrument(skip(self, request), fields(rpc = "RegisterRealisation"))]
    async fn register_realisation(
        &self,
        request: Request<RegisterRealisationRequest>,
    ) -> Result<Response<RegisterRealisationResponse>, Status> {
        self.register_realisation_impl(request).await
    }

    #[instrument(skip(self, request), fields(rpc = "QueryRealisation"))]
    async fn query_realisation(
        &self,
        request: Request<QueryRealisationRequest>,
    ) -> Result<Response<Realisation>, Status> {
        self.query_realisation_impl(request).await
    }

    #[instrument(skip(self, request), fields(rpc = "TenantQuota"))]
    async fn tenant_quota(
        &self,
        request: Request<TenantQuotaRequest>,
    ) -> Result<Response<TenantQuotaResponse>, Status> {
        self.tenant_quota_impl(request).await
    }

    /// ADR-023 phase-10: builder writes one `hw_perf_samples` row.
    /// Upsert on `(hw_class, pod_id)` — the `hw_perf_factors` view's
    /// median is over ALL rows (only the `HAVING` is distinct-pod), so
    /// without the UNIQUE constraint (M_046) a single pod spamming N
    /// inserts would dominate the median once 3 honest pods exist.
    ///
    /// **Identity from claims, not body.** The "one rank in a median"
    /// defense only holds if `pod_id` identifies the caller. Builders
    /// are untrusted (`r[sec.boundary.grpc-hmac]`), so `pod_id` is
    /// derived from the verified assignment-token's
    /// `claims.executor_id` (scheduler-signed at dispatch); the body
    /// `pod_id` is IGNORED. Without this gate a compromised builder
    /// could fabricate N distinct `pod_id` values and own the median
    /// for any `hw_class`. Service-token callers (gateway) have no
    /// business here and are rejected. Dev mode (`hmac_verifier` is
    /// None) falls back to the body field — same as PutPath dev-mode.
    ///
    /// `hw_class` remains body-supplied: a valid token holder can write
    /// its one row to a foreign `hw_class`, but that's one rank in that
    /// class's median, bounded by `HW_FACTOR_SANITY_CEIL` in `HwTable`.
    /// `hw_class` is bounded at [`rio_common::limits::MAX_HW_CLASS_LEN`]
    /// chars of `[a-z0-9-]` — the unique key is `(hw_class, pod_id)`, so
    /// without a format bound a compromised builder could spam distinct
    /// multi-MB strings and fill the table (M_041's "one row per pod
    /// start" assumed honest callers). `factor_json` is parsed,
    /// per-dimension validated, then REBUILT from the three scalars so
    /// extra keys / padding never reach PG.
    ///
    /// `submitting_tenant` is derived from `claims.tenant` (signed at
    /// dispatch), NOT the body — `r[sched.sla.threat.
    /// hw-median-of-medians]` keys on it; a body field would let one
    /// compromised builder fabricate ≥5 tenant identities.
    // r[impl sched.sla.hw-bench-append-only]
    // r[impl sec.boundary.grpc-hmac]
    // r[impl sched.sla.threat.hw-median-of-medians]
    #[instrument(skip(self, request), fields(rpc = "AppendHwPerfSample"))]
    async fn append_hw_perf_sample(
        &self,
        request: Request<AppendHwPerfSampleRequest>,
    ) -> Result<Response<()>, Status> {
        let (pod_id, tenant) = match self.verify_assignment_token(&request)? {
            put_path::IngestAuthority::Builder(claims) => (claims.executor_id, claims.tenant),
            // Dev mode (no HMAC verifier): body pod_id, NULL tenant.
            put_path::IngestAuthority::DevMode => (request.get_ref().pod_id.clone(), None),
            // The divergent service-caller policy, as a VISIBLE arm:
            // PutPath* admit the bypass (gateway/scheduler uploads);
            // this RPC's one-rank-in-a-median defense requires pod_id
            // from a scheduler-signed assignment token, so service
            // callers are rejected outright. TenantJwt is
            // PutPathChunked-only (source uploads), reject too.
            put_path::IngestAuthority::ServiceBypass { .. }
            | put_path::IngestAuthority::TenantJwt => {
                metrics::counter!("rio_store_hmac_rejected_total",
                                  "reason" => "service_caller_not_permitted")
                .increment(1);
                return Err(Status::permission_denied(
                    "AppendHwPerfSample: service-token callers not permitted; \
                     pod_id must come from an assignment token",
                ));
            }
        };
        let req = request.into_inner();
        if !rio_common::limits::is_hw_class_name(&req.hw_class) || pod_id.is_empty() {
            return Err(Status::invalid_argument(
                "hw_class must be 1-64 chars of [a-z0-9-]; pod_id required",
            ));
        }
        // K=3 jsonb factor: parse + validate every PRESENT dimension
        // here (NOT a CHECK constraint) so a malformed payload is an
        // InvalidArgument, not a PG error surfaced as Internal.
        // bug_037: `membw`/`ioseq` are optional — `bench_needed=false`
        // and tmpfs-O_DIRECT-EINVAL both omit dims rather than write
        // a `1.0` placeholder; the scheduler's per-dim median ignores
        // absent dims. `alu` stays mandatory (always measured). The
        // INSERTed jsonb is REBUILT from the validated scalars — the
        // raw `Value` is body-supplied and unbounded (extra keys,
        // MB-scale padding); rebuilding makes that structurally
        // impossible.
        let raw: serde_json::Value = serde_json::from_str(&req.factor_json).map_err(|e| {
            Status::invalid_argument(format!("factor_json must be a JSON object: {e}"))
        })?;
        let dim = |d: &str| -> Result<Option<f64>, Status> {
            let Some(raw_v) = raw.get(d) else {
                return Ok(None);
            };
            let v = raw_v
                .as_f64()
                .ok_or_else(|| Status::invalid_argument(format!("factor_json.{d} not a number")))?;
            if !v.is_finite() || v <= 0.0 {
                return Err(Status::invalid_argument(format!(
                    "factor_json.{d}={v} must be finite and > 0"
                )));
            }
            Ok(Some(v))
        };
        let alu = dim("alu")?.ok_or_else(|| {
            Status::invalid_argument("factor_json.alu missing (mandatory dimension)")
        })?;
        let mut factor = serde_json::Map::new();
        factor.insert("alu".into(), alu.into());
        if let Some(v) = dim("membw")? {
            factor.insert("membw".into(), v.into());
        }
        if let Some(v) = dim("ioseq")? {
            factor.insert("ioseq".into(), v.into());
        }
        let factor = serde_json::Value::Object(factor);
        sqlx::query!(
            "INSERT INTO hw_perf_samples (hw_class, pod_id, factor, submitting_tenant) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (hw_class, pod_id) \
             DO UPDATE SET factor = EXCLUDED.factor, \
                           submitting_tenant = EXCLUDED.submitting_tenant, \
                           measured_at = now()",
            req.hw_class,
            pod_id,
            sqlx::types::Json(factor) as _,
            tenant,
        )
        .execute(&self.pool)
        .await
        .status_internal("AppendHwPerfSample: insert")?;
        Ok(Response::new(()))
    }

    /// Pure PG read. The index is written atomically with the
    /// manifest-complete flip (ADR-022 §6) and cannot be recomputed
    /// from the stored per-file chunks, so a `'complete'` path with no
    /// index row is `DATA_LOSS`, not a cache miss. `NotFound` if the
    /// path has no complete manifest. Builder-internal like
    /// `BatchGetManifest`: the response carries `file_digest`
    /// capability tokens, so end-user tenants are refused.
    // r[impl store.index.rpc+1]
    #[instrument(skip(self, request), fields(rpc = "GetNarIndex"))]
    async fn get_nar_index(
        &self,
        request: Request<rio_proto::types::GetNarIndexRequest>,
    ) -> Result<Response<rio_proto::types::NarIndex>, Status> {
        // Identity before anything else — an anonymous caller learns
        // nothing about any nar_hash, not even whether it exists.
        self.require_caller_identity(&request)?;
        let _ = self.reject_end_user_tenant(&request, "GetNarIndex")?;
        let nar_hash = parse_nar_hash(&request.into_inner().nar_hash)?;
        let bytes = self.lookup_nar_index(&nar_hash).await?;
        let index = crate::nar_index::decode_entries(&bytes)
            .map_err(|e| Status::data_loss(format!("corrupt nar_index row: {e}")))?;
        Ok(Response::new(index))
    }

    type GetNarIndexBatchStream =
        tokio_stream::wrappers::ReceiverStream<Result<rio_proto::types::NarIndexResponse, Status>>;

    /// Per-`nar_hash` responses, request order preserved. `index` is
    /// absent for unknown or incomplete paths — every complete path
    /// has its index from the moment it becomes visible (written in
    /// the same transaction as the status flip). Builder-internal:
    /// end-user tenants refused.
    // r[impl store.index.rpc+1]
    #[instrument(skip(self, request), fields(rpc = "GetNarIndexBatch"))]
    async fn get_nar_index_batch(
        &self,
        request: Request<rio_proto::types::GetNarIndexBatchRequest>,
    ) -> Result<Response<Self::GetNarIndexBatchStream>, Status> {
        // Same identity-first ordering as GetNarIndex: the gate fires
        // before the size check and any PG work.
        self.require_caller_identity(&request)?;
        let _ = self.reject_end_user_tenant(&request, "GetNarIndexBatch")?;
        let req = request.into_inner();
        if req.nar_hashes.len() > self.max_batch_paths {
            return Err(Status::invalid_argument(format!(
                "GetNarIndexBatch: {} hashes exceeds max {}",
                req.nar_hashes.len(),
                self.max_batch_paths,
            )));
        }
        let pool = self.pool.clone();
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        // The drain timeout also bounds a max-batch request
        // (`max_batch_paths` serial PG round trips).
        rio_common::task::spawn_monitored("get-nar-index-batch", async move {
            let drain = async {
                for raw in req.nar_hashes {
                    let resp = match parse_nar_hash(&raw) {
                        Err(e) => Err(e),
                        Ok(h) => match metadata::get_nar_index(&pool, &h).await {
                            Err(e) => Err(metadata_status("GetNarIndexBatch", e)),
                            Ok(opt) => Ok(rio_proto::types::NarIndexResponse {
                                nar_hash: raw,
                                index: match opt {
                                    None => None,
                                    Some(b) => match crate::nar_index::decode_entries(&b) {
                                        Ok(idx) => {
                                            metrics::counter!(
                                                "rio_store_nar_index_cache_hits_total"
                                            )
                                            .increment(1);
                                            Some(idx)
                                        }
                                        Err(e) => {
                                            let _ = tx
                                                .send(Err(Status::data_loss(format!(
                                                    "corrupt nar_index row: {e}"
                                                ))))
                                                .await;
                                            return;
                                        }
                                    },
                                },
                            }),
                        },
                    };
                    if tx.send(resp).await.is_err() {
                        return; // client disconnected
                    }
                }
            };
            let _ = drain_with_timeout("GetNarIndexBatch", &tx, drain).await;
        });
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }
}

/// 32-byte nar_hash from a wire request, or `INVALID_ARGUMENT`.
fn parse_nar_hash(raw: &[u8]) -> Result<[u8; 32], Status> {
    raw.try_into().map_err(|_| {
        Status::invalid_argument(format!("nar_hash must be 32 bytes, got {}", raw.len()))
    })
}

impl StoreServiceImpl {
    /// `nar_index` PG read for `GetNarIndex`. No recompute path: the
    /// index is written in the same transaction that makes the
    /// manifest `'complete'`, and the source material for a recompute
    /// (the NAR byte stream) is not persisted. A complete path with a
    /// missing row is therefore storage corruption (`DATA_LOSS`), and
    /// an unknown `nar_hash` is `NOT_FOUND`.
    async fn lookup_nar_index(&self, nar_hash: &[u8; 32]) -> Result<Vec<u8>, Status> {
        if let Some(b) = metadata::get_nar_index(&self.pool, nar_hash)
            .await
            .map_err(|e| metadata_status("GetNarIndex", e))?
        {
            metrics::counter!("rio_store_nar_index_cache_hits_total").increment(1);
            return Ok(b);
        }
        let store_path = metadata::path_by_nar_hash(&self.pool, nar_hash)
            .await
            .map_err(|e| metadata_status("GetNarIndex", e))?
            .ok_or_else(|| Status::not_found("no complete manifest for nar_hash"))?;
        error!(store_path, "complete path has no nar_index row");
        Err(Status::data_loss(format!(
            "no nar_index row for {store_path}: the index is written with the \
             complete transaction and cannot be recomputed"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `storage_error` maps `BackendAuthError` anywhere in the anyhow
    /// chain to `FailedPrecondition`. This is the contract the builder
    /// relies on to distinguish "retry" (`Internal`) from "give up"
    /// (`FailedPrecondition`).
    #[test]
    fn storage_error_auth_maps_to_failed_precondition() {
        use crate::backend::BackendAuthError;

        // The shape S3ChunkBackend::put() produces: marker at the root,
        // detailed message as context.
        let e = anyhow::Error::new(BackendAuthError)
            .context("S3 PutObject failed for chunks/ab/abab...: AccessDenied");
        let status = storage_error("test", e);
        assert_eq!(
            status.code(),
            tonic::Code::FailedPrecondition,
            "BackendAuthError must map to FailedPrecondition (non-retriable)"
        );
        // Message names the fix per feedback policy: "if code knows the
        // right value, put it in the error verbatim".
        assert!(
            status.message().contains("S3 credentials")
                || status.message().contains("IAM permissions"),
            "auth error message should name the fix, got: {}",
            status.message()
        );
    }

    /// Non-auth errors fall through to `Internal` — same behavior as
    /// the old `internal_error` (retriable).
    #[test]
    fn storage_error_other_maps_to_internal() {
        let e = anyhow::anyhow!("S3 PutObject failed: connection reset");
        let status = storage_error("test", e);
        assert_eq!(
            status.code(),
            tonic::Code::Internal,
            "non-auth error must map to Internal (retriable)"
        );
        assert_eq!(status.message(), "storage operation failed");
    }

    // r[verify sched.dispatch.fod-substitute+3]
    /// `x-rio-probe-tenant-id` is honoured ONLY behind a valid
    /// allowlisted service-token. An unauthenticated request (or one
    /// from a non-allowlisted caller) cannot self-select a tenant.
    #[tokio::test]
    async fn request_tenant_id_probe_header_gated_on_service_token() {
        use rio_auth::hmac::{HmacSigner, HmacVerifier, ServiceClaims};
        let key = b"probe-gate-test-key-32-bytes!!!!".to_vec();
        let pool = sqlx::PgPool::connect_lazy("postgres://unused").unwrap();
        let svc = StoreServiceImpl::new(pool)
            .with_service_hmac_verifier(Arc::new(HmacVerifier::from_key(key.clone())));
        let tid = uuid::Uuid::new_v4();
        let mk = |caller: Option<&str>| {
            let mut r = Request::new(());
            r.metadata_mut().insert(
                rio_proto::PROBE_TENANT_ID_HEADER,
                tid.to_string().parse().unwrap(),
            );
            if let Some(c) = caller {
                let tok = HmacSigner::from_key(key.clone()).sign(&ServiceClaims {
                    caller: c.into(),
                    expiry_unix: u64::MAX,
                    instance: None,
                });
                r.metadata_mut()
                    .insert(rio_proto::SERVICE_TOKEN_HEADER, tok.parse().unwrap());
            }
            r
        };
        // merged_bug_003 (Q3): a PRESENT probe header with a missing
        // token REJECTS — the pre-fix assertion here was
        // `assert_eq!(..., None)` ("No service-token → ignored"),
        // i.e. the silent tenant-blind downgrade, codified.
        assert!(
            svc.request_tenant_id(&mk(None))
                .is_err_and(|s| s.code() == tonic::Code::Unauthenticated),
            "probe header without a service token must reject, not downgrade"
        );
        // Allowlisted caller → honoured.
        assert_eq!(
            svc.request_tenant_id(&mk(Some("rio-scheduler"))).unwrap(),
            Some(tid)
        );
        // Non-allowlisted caller: the token VERIFIED but its caller
        // cannot assert tenant context — reject (pre-fix: silent
        // anonymous, the same laundering one rung up).
        assert!(
            svc.request_tenant_id(&mk(Some("rogue")))
                .is_err_and(|s| s.code() == tonic::Code::Unauthenticated),
            "non-allowlisted caller must reject, not downgrade"
        );
        // A garbage (unverifiable) token with the probe header
        // rejects — HMAC-rotation skew shows up loud, never as
        // confirmed 404s.
        let mut garbage = Request::new(());
        garbage.metadata_mut().insert(
            rio_proto::PROBE_TENANT_ID_HEADER,
            tid.to_string().parse().unwrap(),
        );
        garbage.metadata_mut().insert(
            rio_proto::SERVICE_TOKEN_HEADER,
            "not-a-real-token".parse().unwrap(),
        );
        assert!(
            svc.request_tenant_id(&garbage)
                .is_err_and(|s| s.code() == tonic::Code::Unauthenticated),
            "an unverifiable token must reject, not downgrade"
        );
        // ABSENT probe header + absent token: anonymous by design —
        // no tenant context was requested.
        assert_eq!(svc.request_tenant_id(&Request::new(())).unwrap(), None);
    }

    /// `AppendHwPerfSample` is one-row-per-`(hw_class, pod_id)`: a
    /// second call upserts, it does not append. Regression for the
    /// "one rank in a median" claim — pre-M_046 a single pod could
    /// stuff the `hw_perf_factors` median with N rows.
    #[tokio::test]
    async fn append_hw_perf_sample_upserts_on_duplicate_pod() {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreServiceImpl::new(db.pool.clone());
        let mk = |f: f64| {
            Request::new(AppendHwPerfSampleRequest {
                hw_class: "aws-8-ebs-hi".into(),
                pod_id: "p0".into(),
                factor_json: format!(r#"{{"alu":{f},"membw":1.0,"ioseq":1.0}}"#),
            })
        };
        svc.append_hw_perf_sample(mk(0.9)).await.unwrap();
        svc.append_hw_perf_sample(mk(1.1)).await.unwrap();
        let (n, alu): (i64, f64) = sqlx::query_as(
            "SELECT count(*), max((factor->>'alu')::float8) FROM hw_perf_samples \
             WHERE hw_class = 'aws-8-ebs-hi' AND pod_id = 'p0'",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(n, 1, "duplicate (hw_class, pod_id) must upsert, not append");
        assert!((alu - 1.1).abs() < 1e-9, "upsert keeps latest factor");
    }

    /// Per-dimension validation: `alu` missing, present-but-non-
    /// numeric, NaN, ≤0 → InvalidArgument. bug_037: `membw`/`ioseq`
    /// MAY be absent (per-dim presence carried end-to-end); a
    /// `bench_needed=false` builder sends `{"alu":x}` only and the
    /// scheduler's per-dim median ignores absent dims. Present dims
    /// are still validated.
    #[tokio::test]
    async fn append_hw_perf_sample_rejects_malformed_factor() {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreServiceImpl::new(db.pool.clone());
        for bad in [
            "not json",
            "{}",
            r#"{"membw":1.0,"ioseq":1.0}"#, // alu mandatory
            r#"{"alu":1.0,"membw":"x","ioseq":1.0}"#,
            r#"{"alu":1.0,"membw":0.0,"ioseq":1.0}"#,
            r#"{"alu":-1.0,"membw":1.0,"ioseq":1.0}"#,
        ] {
            let err = svc
                .append_hw_perf_sample(Request::new(AppendHwPerfSampleRequest {
                    hw_class: "aws-8-ebs-hi".into(),
                    pod_id: "p0".into(),
                    factor_json: bad.into(),
                }))
                .await
                .expect_err(bad);
            assert_eq!(err.code(), tonic::Code::InvalidArgument, "input: {bad}");
        }
        // Missing membw/ioseq is now valid; stored jsonb omits them.
        svc.append_hw_perf_sample(Request::new(AppendHwPerfSampleRequest {
            hw_class: "aws-8-ebs-hi".into(),
            pod_id: "p-alu-only".into(),
            factor_json: r#"{"alu":1.5}"#.into(),
        }))
        .await
        .expect("alu-only is valid");
        let stored: serde_json::Value =
            sqlx::query_scalar("SELECT factor FROM hw_perf_samples WHERE pod_id = 'p-alu-only'")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(stored, serde_json::json!({"alu": 1.5}));
    }
}
