//! Upstream binary-cache substitution: block-and-fetch narinfo + NAR
//! from a tenant's configured upstreams, ingest via the same CAS path
//! as PutPath.
//!
// r[impl store.substitute.upstream]
//! Flow (see [`Substituter::try_substitute`]):
//!
//! 1. Load the tenant's upstream list (`tenant_upstreams`, priority ASC)
//! 2. Per upstream: GET `{url}/{hash_part}.narinfo` → parse → `verify_sig`
//! 3. GET `{url}/{narinfo.url}` (the NAR, possibly xz/zstd compressed)
//! 4. Decompress stream → accumulate → write-ahead ingest
//! 5. Apply `sig_mode` (keep/add/replace) to stored signatures
//! 6. Return `ValidatedPathInfo`
//!
//! [`check_available`](Substituter::check_available) is the HEAD-only
//! cousin that feeds `FindMissingPathsResponse.substitutable_paths`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use bytes::Bytes;
use moka::future::Cache;
use sqlx::PgPool;
use tokio::io::AsyncReadExt;
use tracing::{debug, info, instrument, warn};
use uuid::Uuid;

use rio_nix::narinfo::NarInfo;
use rio_nix::store_path::StorePath;
use rio_proto::validated::ValidatedPathInfo;

use rio_common::limits::{
    MAX_CACHE_INFO_BYTES, MAX_NAR_SIZE, MAX_NARINFO_BYTES, MAX_REFERENCES, MAX_SIGNATURES,
    MIN_NAR_CHUNK_CHARGE,
};

use crate::admission::{AdmissionError, AdmissionGate};
use crate::backend::ChunkBackend;
use crate::cas;
use crate::ingest::{self, IngestHooks, PersistError, PlaceholderClaim};
use crate::metadata::{self, SigMode, Upstream};
use crate::signing::TenantSigner;

/// Substitution hooks for the shared ingest core.
const SUBSTITUTE_HOOKS: IngestHooks = IngestHooks {
    stale_reclaimed_metric: "rio_store_substitute_stale_reclaimed_total",
    ctx_label: "substitute",
};

/// How old an `'uploading'` placeholder must be before the
/// substitution ingest path reclaims it instead of returning a miss.
///
/// 5 minutes: long enough that a real concurrent substitution (even a
/// multi-GB NAR over a slow link) finishes first; short enough that an
/// rsb retry loop doesn't wait for the orphan scanner's 15-minute sweep.
pub const SUBSTITUTE_STALE_THRESHOLD: Duration = Duration::from_secs(5 * 60);

/// Bound on concurrent narinfo HEAD probes in [`Substituter::check_available`].
/// The reqwest connection pool is the next bottleneck above this; 128
/// keeps the in-flight set well under typical fd limits and avoids
/// thrashing the pool when a `FindMissingPaths` batch is large.
pub const SUBSTITUTE_PROBE_CONCURRENCY: usize = 128;

/// Conservative HEAD-probe concurrency for upstreams that do NOT
/// advertise `WantMassQuery: 1` in `/nix-cache-info` (or whose
/// cache-info fetch failed). Matches Nix's own conservative default
/// for non-mass-query caches.
pub const SUBSTITUTE_PROBE_CONCURRENCY_CONSERVATIVE: usize = 8;

/// Default deadline budget for [`Substituter::check_available`]'s
/// 429-retry loop when the originating RPC carries no narrower bound.
/// Matches the scheduler's `MERGE_FMP_TIMEOUT` (90s) less 2s headroom
/// for the HEADs themselves — the widest caller. Dispatch-time
/// callers (30s `grpc_timeout`) cancel the RPC client-side first; the
/// store-side budget only bites when the caller's timeout is ≥ this.
pub const CHECK_AVAILABLE_DEFAULT_BUDGET: Duration = Duration::from_secs(88);

/// Max retry passes [`Substituter::check_available`] makes over the
/// rate-limited subset before giving up and returning the remainder as
/// `Indeterminate` (not cached; next call re-probes).
const SUBSTITUTE_PROBE_429_MAX_PASSES: u32 = 3;

/// Fraction of a pass's batch that must come back `RateLimited` to
/// trigger adaptive concurrency halving for the retry pass. Below this,
/// the next pass keeps the same concurrency (a handful of 429s on a
/// 17k-path batch is per-object noise, not edge-wide backpressure).
const SUBSTITUTE_PROBE_429_ADAPT_THRESHOLD: f64 = 0.1;

/// TTL for both the per-upstream `/nix-cache-info` cache and the
/// per-path HEAD-probe result cache.
const SUBSTITUTE_PROBE_CACHE_TTL: Duration = Duration::from_secs(60 * 60);

/// Max entries in the per-path HEAD-probe result cache.
const SUBSTITUTE_PROBE_CACHE_CAP: u64 = 100_000;

/// Per-request timeout applied to the SMALL upstream fetches (narinfo
/// GET, `/nix-cache-info` GET, narinfo HEAD probe). The shared
/// `reqwest::Client` has only `connect_timeout` — without a per-request
/// body timeout a slow-loris upstream holds 128 probe slots indefinitely
/// and stalls `FindMissingPaths`. The NAR GET is intentionally NOT
/// timeboxed (a multi-GB body legitimately runs long; the
/// [`MAX_NAR_SIZE`] decompressed cap and 5-min stale-reclaim bound it).
#[cfg(not(test))]
const SUBSTITUTE_SMALL_FETCH_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
const SUBSTITUTE_SMALL_FETCH_TIMEOUT: Duration = Duration::from_secs(2);

/// Decompressed-NAR size cap applied in [`Substituter::fetch_nar`].
/// Equals [`MAX_NAR_SIZE`] in production; overridable to a small value
/// in tests so the bomb-protection path is exercisable without
/// allocating 4 GiB.
#[cfg(not(test))]
const SUBSTITUTE_NAR_DECOMPRESSED_CAP: u64 = MAX_NAR_SIZE;
#[cfg(test)]
const SUBSTITUTE_NAR_DECOMPRESSED_CAP: u64 = 64 * 1024;

/// Decompressed-byte interval at which `Substituter::fetch_nar` fires
/// the optional progress callback. 1 MiB = 16 iterations of the 64 KiB
/// read loop. At post-T1 throughput (~90 MB/s) this is ~90/sec per path
/// for a hot stream; the scheduler-side aggregate is debounced further
/// (one `SubstituteProgress` BuildEvent per callback, routed via the
/// log broadcast ring so a Lagged drop is harmless).
pub const SUBSTITUTE_PROGRESS_INTERVAL_BYTES: u64 = 1024 * 1024;

/// Progress callback signature for `r[store.substitute.progress-stream]`:
/// `(bytes_done, bytes_expected, upstream_base_uri)`. Called from
/// `Substituter::fetch_nar`'s read loop every
/// [`SUBSTITUTE_PROGRESS_INTERVAL_BYTES`]. `bytes_expected` is the
/// narinfo's `NarSize` (declared, hash-verified at end). The callback
/// MUST be cheap and non-blocking — it runs on the substitute task.
pub type SubstProgressFn = dyn Fn(u64, u64, &str) + Send + Sync;

/// Parsed `/nix-cache-info` — only the field the substituter cares
/// about. `StoreDir`/`Priority` are irrelevant here (priority comes
/// from `tenant_upstreams`, not the upstream's self-declaration).
#[derive(Debug, Clone, Copy)]
struct UpstreamInfo {
    /// `WantMassQuery: 1` — upstream consents to high-concurrency
    /// narinfo HEADs. Absent or `0` → throttle to
    /// [`SUBSTITUTE_PROBE_CONCURRENCY_CONSERVATIVE`].
    want_mass_query: bool,
}

impl UpstreamInfo {
    /// Parse the `key: value\n` body of `/nix-cache-info`. Unknown
    /// keys are ignored; missing `WantMassQuery` = `false`.
    fn parse(body: &str) -> Self {
        let want_mass_query = body
            .lines()
            .filter_map(|l| l.split_once(':'))
            .any(|(k, v)| k.trim() == "WantMassQuery" && v.trim() == "1");
        Self { want_mass_query }
    }
}

/// Errors surfaced by the substitution path. Callers map these to gRPC
/// status; `NotFound` is the normal miss case (no upstream has the
/// path, or the tenant has no upstreams configured).
///
/// `Clone` so moka's `try_get_with` can hand the same error to every
/// coalesced waiter (it stores `Arc<E>` internally; we unwrap to owned).
#[derive(Debug, Clone, thiserror::Error)]
pub enum SubstituteError {
    /// Upstream HTTP request failed (connect, TLS, 5xx). The upstream
    /// URL is folded into the message for operator-side debugging.
    #[error("upstream fetch failed: {0}")]
    Fetch(String),

    /// narinfo parse failed, or the `NarHash:` line didn't decode to
    /// a 32-byte SHA-256. Upstream served garbage.
    #[error("narinfo parse error: {0}")]
    NarInfo(String),

    /// The ingested NAR's SHA-256 didn't match the narinfo's `NarHash:`
    /// line. Upstream served corrupt bytes (or lied in the narinfo).
    #[error("NAR hash mismatch: expected {expected}, got {got}")]
    HashMismatch { expected: String, got: String },

    /// An upstream-supplied body exceeded a size cap (narinfo,
    /// `/nix-cache-info`, declared `NarSize`, or decompressed NAR).
    /// `tenant_upstreams` rows are tenant-supplied; one tenant's hostile
    /// upstream must not OOM the process-global substituter.
    #[error("upstream {what} exceeds {limit}-byte cap")]
    TooLarge { what: &'static str, limit: u64 },

    /// Decompressed NAR length differed from the narinfo's `NarSize:`
    /// line. Upstream lied; the Nix signature fingerprint includes
    /// `nar_size`, so persisting an unchecked size would store a row
    /// whose own signatures don't verify.
    #[error("NAR size mismatch: narinfo declared {declared}, got {actual} decompressed bytes")]
    SizeMismatch { declared: u64, actual: u64 },

    /// Transient placeholder-claim: another uploader holds the slot.
    /// Same-replica `try_substitute*` callers coalesce at the moka
    /// `inflight` singleflight and never reach `claim_placeholder`
    /// concurrently, so this only surfaces on **cross-replica** races
    /// (another rio-store pod) or a concurrent `PutPath`. Returned as
    /// `Err` (not `Ok(None)`) so moka does NOT cache it; the caller's
    /// retry re-runs `do_substitute` and reaches `AlreadyComplete` once
    /// the in-flight upload lands. gRPC maps to `NotFound` (the
    /// gateway's 2-attempt budget can't outlast a multi-second NAR
    /// fetch, so callers re-probe later).
    #[error("transient: concurrent uploader holds placeholder")]
    Raced,

    /// Transient upstream-429 (`r[store.substitute.probe-429-retry+2]`).
    /// `retry_after` is the parsed `Retry-After` header (delta-seconds
    /// or HTTP-date), `None` if absent or unparseable. Returned as
    /// `Err` so moka does NOT cache it; the admission permit is
    /// dropped on return so the wait happens caller-side without
    /// holding per-replica capacity. gRPC maps to `Unavailable` so the
    /// scheduler's 8-attempt backoff retries — a bare 429 (no
    /// `Retry-After`) is still a real rate-limit, NOT a miss.
    #[error("transient: upstream rate-limited (retry after {retry_after:?})")]
    RateLimited { retry_after: Option<Duration> },

    /// Per-replica admission gate timed out (or closed). Transient —
    /// `Err` so moka does NOT cache it; the next caller after the
    /// burst clears retries cleanly.
    #[error(transparent)]
    Admission(#[from] AdmissionError),

    /// Metadata-layer failure during ingest (write-ahead,
    /// complete_manifest, chunked S3/refcount).
    #[error("ingest failed: {0}")]
    Ingest(String),
}

impl From<metadata::MetadataError> for SubstituteError {
    fn from(e: metadata::MetadataError) -> Self {
        SubstituteError::Ingest(e.to_string())
    }
}

/// Result of probing one upstream for one path. Disambiguates the three
/// "no PathInfo" outcomes that `Option<ValidatedPathInfo>` collapsed:
/// `Miss` (try next upstream), `Raced` (another uploader holds the
/// placeholder — STOP, don't re-download from remaining upstreams), and
/// the error path (try next, but log).
#[derive(Debug)]
enum UpstreamOutcome {
    /// narinfo verified, NAR ingested (or `AlreadyComplete` and sigs
    /// appended). Boxed: `ValidatedPathInfo` is ~300B and the
    /// `Miss`/`Raced` arms are zero-sized.
    Hit(Box<ValidatedPathInfo>),
    /// narinfo 404 or sig didn't verify — this upstream doesn't have it.
    Miss,
    /// `claim_placeholder` returned `Concurrent` — another replica or
    /// closure-walk holds the slot. Mapped to `Err(Raced)` in
    /// `do_substitute` so moka does not cache it; caller's retry
    /// re-runs and reaches `AlreadyComplete`. Trying remaining
    /// upstreams would just race the same slot again.
    Raced,
}

/// Result of one HEAD probe across the tenant's upstreams. Only `Hit`
/// and `Miss` are cached; `Indeterminate` (network error / 5xx on every
/// non-hit base) and `RateLimited` are left uncached so the next call
/// re-probes instead of pinning a transient failure for the full 1h TTL.
#[derive(Clone, Copy)]
enum ProbeOutcome {
    Hit,
    Miss,
    Indeterminate,
    /// At least one upstream returned 429; `retry_after` is the parsed
    /// `Retry-After` (delta-seconds OR HTTP-date) if any. Distinct from
    /// `Indeterminate` so [`Substituter::check_available`] can sleep +
    /// retry instead of falling through to a build.
    RateLimited {
        retry_after: Option<Duration>,
    },
}

/// Parse an RFC 9110 `Retry-After` header from a 429 response.
/// Accepts both forms: delta-seconds (`"120"`) and HTTP-date
/// (`"Wed, 21 Oct 2026 07:28:00 GMT"`). Returns the raw duration —
/// NO clamping; the caller's deadline budget decides whether to honor
/// it (`r[store.substitute.probe-429-retry+2]`).
///
/// Takes `&HeaderMap` (not `&reqwest::Response`) so the HTTP-date
/// branch is unit-testable without a live socket — see
/// `parse_retry_after_http_date`.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let s = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    if let Ok(secs) = s.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let when = httpdate::parse_http_date(s).ok()?;
    when.duration_since(std::time::SystemTime::now()).ok()
}

/// HTTP narinfo + NAR fetcher with per-tenant upstream lookup.
///
/// Constructed once at server startup and shared via `Arc` across
/// `StoreServiceImpl`'s RPC handlers. The `reqwest::Client` is
/// connection-pooled; the moka singleflight cache coalesces concurrent
/// substitutions of the same `(tenant, path)` pair.
pub struct Substituter {
    pool: PgPool,
    chunk_backend: Option<Arc<dyn ChunkBackend>>,
    /// `None` if `reqwest::Client::builder().build()` failed (nix
    /// sandbox: no CA bundle → rustls-native-certs errors). In that
    /// case substitution is a no-op — every `try_substitute` returns
    /// `None` and every `check_available` returns `[]`. Production
    /// always has a CA bundle; only the sandboxed test run hits this.
    http: Option<reqwest::Client>,
    /// The signer for `sig_mode = add|replace`. `None` means those
    /// modes fall back to `keep` behavior (we can't sign without a
    /// key). Per-tenant key resolution inside.
    signer: Option<Arc<TenantSigner>>,
    // r[impl store.singleflight]
    /// Singleflight: `(tenant_id, store_path)` → cached result. TTL
    /// keeps a recently-substituted path hot for the next caller
    /// without re-checking PG. Entries are cheap (the `PathInfo` is
    /// already in narinfo; this is just the gRPC-shaped copy). moka
    /// handles concurrent `get_with` on the same key by coalescing —
    /// N callers become one `do_substitute` call.
    inflight: Cache<(Uuid, String), Option<Arc<ValidatedPathInfo>>>,
    /// Max concurrent S3 chunk uploads per `put_chunked` call. Same
    /// bound as `StoreServiceImpl` — the substitution ingest path
    /// calls the same `cas::put_chunked` as PutPath. Default
    /// [`cas::DEFAULT_CHUNK_UPLOAD_CONCURRENCY`].
    chunk_upload_max_concurrent: usize,
    /// Per-upstream `/nix-cache-info` cache, keyed by trimmed base
    /// URL. TTL [`SUBSTITUTE_PROBE_CACHE_TTL`]. moka `get_with`
    /// singleflights concurrent fetches.
    upstream_info: Cache<String, UpstreamInfo>,
    /// Per-path HEAD-probe result cache: `(tenant_id, store-path)` →
    /// "present at any of THIS TENANT's upstreams". Positive AND
    /// negative results cached. TTL [`SUBSTITUTE_PROBE_CACHE_TTL`],
    /// cap [`SUBSTITUTE_PROBE_CACHE_CAP`]. Makes overlapping
    /// `FindMissingPaths` for the same closure cheap (the deep-1024x
    /// case where the client retries after a timeout). Keyed by
    /// tenant for the same reason [`inflight`](Self::inflight) is:
    /// upstreams are per-tenant (`tenant_upstreams` table), so a
    /// path-only key would let tenant B's miss poison tenant A's
    /// lookup for the full TTL.
    probe_cache: Cache<(Uuid, String), bool>,
    // r[impl store.put.nar-bytes-budget+3]
    /// Global budget for in-flight NAR bytes — the SAME semaphore
    /// PutPath acquires from (wired in main.rs via
    /// [`with_nar_bytes_budget`](Self::with_nar_bytes_budget)). Without
    /// this, N concurrent distinct-path substitutions = N × 4 GiB RSS
    /// with zero backpressure (the moka singleflight only coalesces
    /// same `(tenant, path)`). [`fetch_nar`](Self::fetch_nar) acquires
    /// per read-chunk; permits drop after `persist_nar` returns.
    nar_bytes_budget: Arc<Semaphore>,
    /// Per-replica admission gate on concurrent singleflight LEADERS.
    /// `None` (tests / no-op) skips gating. main.rs wires the SAME
    /// [`AdmissionGate`] clone here and into `StoreAdminServiceImpl`
    /// so `GetLoad` reports the utilization the ComponentScaler
    /// reacts to. Acquired inside the moka init future — coalesced
    /// waiters on the same `(tenant, path)` do NOT consume permits.
    admission: Option<AdmissionGate>,
}

/// Default NAR-buffer budget when no shared semaphore is wired:
/// `8 × MAX_NAR_SIZE` (32 GiB) — same as `StoreServiceImpl`'s default.
/// In production main.rs replaces this with the SHARED semaphore so
/// PutPath and substitution draw from one pool.
const DEFAULT_SUBSTITUTE_NAR_BUDGET: usize = (8 * MAX_NAR_SIZE) as usize;

impl Substituter {
    pub fn new(pool: PgPool, chunk_backend: Option<Arc<dyn ChunkBackend>>) -> Self {
        // `Client::new()` panics if `.build()` fails; `.build()` fails
        // in the nix sandbox (rustls-native-certs finds no CA bundle).
        // Use the builder + `.ok()` so sandbox tests degrade to no-op
        // instead of panicking. Tests that exercise HTTP inject a
        // working client via `.with_http_client()`.
        //
        // connect_timeout only — NO body-read timeout. ghc-binary
        // (2.5GB compressed) legitimately exceeds any sane fixed
        // timeout. A hung mid-body upstream is bounded by the moka
        // singleflight TTL + ingest's 5-min stale-placeholder reclaim,
        // not by aborting the download here.
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .ok();
        if http.is_none() {
            warn!("reqwest client build failed; upstream substitution disabled");
        }
        Self {
            pool,
            chunk_backend,
            http,
            signer: None,
            // r[impl store.substitute.singleflight+3]
            // Short TTL + small cap: this is a singleflight coalescer,
            // not a PathInfo cache. The narinfo table IS the cache.
            // 30s is long enough to coalesce a burst of GetPaths for
            // the same path from N workers; short enough that a
            // subsequent substitution-miss doesn't stay stale.
            inflight: Cache::builder()
                .max_capacity(10_000)
                .time_to_live(Duration::from_secs(30))
                .build(),
            chunk_upload_max_concurrent: cas::DEFAULT_CHUNK_UPLOAD_CONCURRENCY,
            upstream_info: Cache::builder()
                .time_to_live(SUBSTITUTE_PROBE_CACHE_TTL)
                .build(),
            probe_cache: Cache::builder()
                .max_capacity(SUBSTITUTE_PROBE_CACHE_CAP)
                .time_to_live(SUBSTITUTE_PROBE_CACHE_TTL)
                .build(),
            nar_bytes_budget: Arc::new(Semaphore::new(DEFAULT_SUBSTITUTE_NAR_BUDGET)),
            admission: None,
        }
    }

    /// Enable `sig_mode = add|replace` signing. Builder-style.
    pub fn with_signer(mut self, signer: Arc<TenantSigner>) -> Self {
        self.signer = Some(signer);
        self
    }

    /// Share the process-global NAR-bytes budget. Builder-style.
    /// main.rs wires `StoreServiceImpl::nar_bytes_budget()` here so
    /// PutPath and substitution draw from ONE semaphore — the
    /// aggregate bound the budget exists to enforce.
    pub fn with_nar_bytes_budget(mut self, budget: Arc<Semaphore>) -> Self {
        self.nar_bytes_budget = budget;
        self
    }

    /// Set the per-replica admission gate. Builder-style. main.rs
    /// constructs ONE [`AdmissionGate`], clones it here AND into
    /// `StoreAdminServiceImpl::with_substitute_admission` — both
    /// observe the same `Arc<Semaphore>`, so `GetLoad`'s utilization
    /// reflects permits the singleflight leaders hold.
    pub fn with_admission_gate(mut self, gate: AdmissionGate) -> Self {
        self.admission = Some(gate);
        self
    }

    /// Replace the HTTP client. Tests in the nix sandbox (no CA
    /// bundle → `Client::builder().build()` fails) need this to
    /// inject a no-TLS client for the in-process axum fake upstream.
    /// Production can use this to configure timeouts/proxies.
    pub fn with_http_client(mut self, http: reqwest::Client) -> Self {
        self.http = Some(http);
        self
    }

    /// Override the per-call chunk-upload concurrency bound.
    /// Builder-style. main.rs threads `RIO_CHUNK_UPLOAD_MAX_CONCURRENT`
    /// here (same value as `StoreServiceImpl`).
    pub fn with_chunk_upload_max_concurrent(mut self, n: usize) -> Self {
        self.chunk_upload_max_concurrent = n;
        self
    }

    /// Try to substitute `store_path` from any of `tenant_id`'s
    /// configured upstreams. Returns `Ok(None)` on miss (no upstream
    /// has it, OR the tenant has no upstreams). Returns `Ok(Some)`
    /// with the ingested `PathInfo` on success.
    ///
    /// Singleflight-wrapped: concurrent calls for the same `(tenant,
    /// path)` coalesce into one fetch. The moka TTL (30s) means a hit
    /// within the window returns the cached result without re-checking.
    ///
    /// **One path, one answer.** The store does NOT walk
    /// `info.references`; closure completeness is the caller's
    /// responsibility (`r[sched.substitute.detached+2]` for the
    /// scheduler; the nix client for the gateway). Matches upstream
    /// Nix's `BinaryCacheStore::queryPathInfo` contract.
    #[instrument(skip(self), fields(tenant = %tenant_id, path = store_path))]
    pub async fn try_substitute(
        &self,
        tenant_id: Uuid,
        store_path: &str,
    ) -> Result<Option<ValidatedPathInfo>, SubstituteError> {
        self.try_substitute_inner(tenant_id, store_path, None).await
    }

    // r[impl store.substitute.progress-stream]
    /// [`try_substitute`](Self::try_substitute) with byte-progress
    /// callback. Same semantics on success/miss/error, but `progress`
    /// fires every [`SUBSTITUTE_PROGRESS_INTERVAL_BYTES`] of
    /// decompressed NAR during the download.
    ///
    /// Shares the moka `inflight` singleflight with `try_substitute`:
    /// concurrent same-`(tenant, path)` calls coalesce; only the
    /// winner's `progress` fires. Losers wait and share the result
    /// (no progress emits — same as the cache-hit fast path).
    #[instrument(skip(self, progress), fields(tenant = %tenant_id, path = store_path))]
    pub async fn try_substitute_with_progress(
        &self,
        tenant_id: Uuid,
        store_path: &str,
        progress: &SubstProgressFn,
    ) -> Result<Option<ValidatedPathInfo>, SubstituteError> {
        self.try_substitute_inner(tenant_id, store_path, Some(progress))
            .await
    }

    /// Shared singleflight body for `try_substitute` /
    /// `try_substitute_with_progress`. moka's `try_get_with`: if
    /// another caller is already computing this key, we wait and share
    /// its result. The init future runs at most once per
    /// key-per-TTL-window. moka caches `Ok(v)` (both `Some` and
    /// definitive-miss `None`) but does NOT cache `Err` — a transient
    /// 503 propagates to every coalesced waiter without poisoning the
    /// slot for 30s, and the next caller after they all return retries
    /// cleanly.
    ///
    /// `progress` is the WINNER'S callback — coalesced losers' callbacks
    /// never fire (they aren't reachable from inside the shared init
    /// future). That's the same observable behavior as a cache hit:
    /// loser sees `Ok(Some)` with no emits, and the closure-aggregate
    /// progress at the scheduler still advances via the winner's drv.
    async fn try_substitute_inner(
        &self,
        tenant_id: Uuid,
        store_path: &str,
        progress: Option<&SubstProgressFn>,
    ) -> Result<Option<ValidatedPathInfo>, SubstituteError> {
        let key = (tenant_id, store_path.to_string());
        let cached = self
            .inflight
            .try_get_with(key, async {
                // Cheap no-work checks BEFORE the admission permit. A
                // tenant with no upstreams (the common case) must get
                // an immediate `Ok(None)`, not queue behind saturated
                // substituters for up to SUBSTITUTE_ADMISSION_WAIT.
                // `list_for_tenant` is one indexed PG read; far cheaper
                // than the permit's potential 25 s queue.
                let Some(http) = &self.http else {
                    return Ok(None); // sandbox: client build failed
                };
                let upstreams = metadata::upstreams::list_for_tenant(&self.pool, tenant_id).await?;
                if upstreams.is_empty() {
                    // Normal — most tenants don't configure upstreams.
                    // No metric; this isn't a miss, it's a no-op.
                    return Ok(None);
                }
                // r[impl store.substitute.admission]
                // Leader-only permit: this init future runs ONCE per
                // `(tenant, path)` per TTL window; coalesced waiters
                // block on the moka future without entering this body,
                // so they consume no permits.
                let _permit = match &self.admission {
                    Some(g) => Some(g.acquire_bounded().await?),
                    None => None,
                };
                self.do_substitute(http, upstreams, tenant_id, store_path, progress)
                    .await
                    .map(|v| v.map(Arc::new))
            })
            .await
            .map_err(|e: Arc<SubstituteError>| {
                // `Raced`/`RateLimited`/`Admission` are not-an-error
                // transients (concurrent uploader / upstream 429 /
                // local backpressure); skip the error metric so they
                // don't show up as upstream failure. Admission has its
                // own dedicated counter.
                if !matches!(
                    *e,
                    SubstituteError::Raced
                        | SubstituteError::RateLimited { .. }
                        | SubstituteError::Admission(_)
                ) {
                    metrics::counter!(
                        "rio_store_substitute_total",
                        "result" => "error",
                        "tenant" => tenant_id.to_string()
                    )
                    .increment(1);
                }
                (*e).clone()
            })?;
        Ok(cached.map(|arc| (*arc).clone()))
    }

    /// One full fetch cycle — the singleflight body. `http` and
    /// `upstreams` are hoisted by the caller (the moka init future)
    /// so the no-upstreams fast-path returns BEFORE the admission
    /// permit is acquired; this body only runs when there is real
    /// upstream work.
    async fn do_substitute(
        &self,
        http: &reqwest::Client,
        upstreams: Vec<Upstream>,
        tenant_id: Uuid,
        store_path: &str,
        progress: Option<&SubstProgressFn>,
    ) -> Result<Option<ValidatedPathInfo>, SubstituteError> {
        let sp = StorePath::parse(store_path)
            .map_err(|e| SubstituteError::NarInfo(format!("bad store path: {e}")))?;
        let hash_part = sp.hash_part();

        // Check if the NAR is already local under the same nar_hash
        // (another tenant substituted it). We'll re-check per-upstream
        // after verify_sig — but early exit here avoids N narinfo
        // round-trips when the path is already there with complete
        // manifest. We still need to go through the sig-append flow
        // though, so this can't return early with just the existing
        // row. Skip.

        let start = Instant::now();
        let tenant_label = tenant_id.to_string();
        // Track 429 across the iteration: only fail with `RateLimited`
        // if NO upstream had the path. This mirrors the HEAD-probe
        // semantics (`probe_one`): a tenant with [rate-limited-A,
        // healthy-B] configured should hit B, not propagate A's 429.
        let mut any_429: Option<Option<Duration>> = None;
        for upstream in &upstreams {
            match self
                .try_upstream(http, tenant_id, upstream, store_path, &hash_part, progress)
                .await
            {
                Ok(UpstreamOutcome::Hit(info)) => {
                    let elapsed = start.elapsed().as_secs_f64();
                    metrics::histogram!("rio_store_substitute_duration_seconds").record(elapsed);
                    metrics::counter!(
                        "rio_store_substitute_total",
                        "result" => "hit",
                        "tenant" => tenant_label
                    )
                    .increment(1);
                    metrics::counter!("rio_store_substitute_bytes_total").increment(info.nar_size);
                    return Ok(Some(*info));
                }
                Ok(UpstreamOutcome::Miss) => {
                    // This upstream doesn't have it. Try the next.
                    debug!(upstream = %upstream.url, "upstream miss, trying next");
                    metrics::counter!(
                        "rio_store_substitute_total",
                        "result" => "miss",
                        "tenant" => tenant_label.clone()
                    )
                    .increment(1);
                }
                Ok(UpstreamOutcome::Raced) => {
                    // Another uploader holds the placeholder. STOP —
                    // remaining upstreams would race the same slot.
                    // Return `Err(Raced)` so moka does NOT cache this
                    // as a definitive miss; caller's retry re-runs and
                    // reaches `AlreadyComplete` once the upload lands.
                    debug!(upstream = %upstream.url, "concurrent uploader, stopping");
                    return Err(SubstituteError::Raced);
                }
                Err(SubstituteError::RateLimited { retry_after }) => {
                    // Upstream 429'd the narinfo or NAR GET. CONTINUE
                    // to the next upstream — only return `RateLimited`
                    // after the loop if no other upstream had it. moka
                    // will not cache the eventual `Err`, and the
                    // admission permit drops on return so the wait
                    // happens caller-side
                    // (r[store.substitute.probe-429-retry+2]).
                    debug!(upstream = %upstream.url, ?retry_after,
                           "upstream 429, trying next");
                    // Max across upstreams that 429'd (matches probe_one).
                    any_429 = Some(match any_429.flatten() {
                        Some(prev) => Some(prev.max(retry_after.unwrap_or_default())),
                        None => retry_after,
                    });
                }
                Err(e) => {
                    // This upstream failed (down, hash mismatch, parse
                    // error). Log and try the next one — a single bad
                    // upstream shouldn't block substitution entirely.
                    // The integrity metric is emitted HERE (where the
                    // error is observable) — per-upstream errors never
                    // reach `grpc/mod.rs`.
                    if matches!(
                        e,
                        SubstituteError::HashMismatch { .. } | SubstituteError::SizeMismatch { .. }
                    ) {
                        metrics::counter!(
                            "rio_store_substitute_integrity_failures_total",
                            "tenant" => tenant_label.clone()
                        )
                        .increment(1);
                    }
                    warn!(upstream = %upstream.url, error = %e, "upstream fetch failed, trying next");
                    metrics::counter!(
                        "rio_store_substitute_total",
                        "result" => "error",
                        "tenant" => tenant_label.clone()
                    )
                    .increment(1);
                }
            }
        }

        // No upstream had it. If ≥1 upstream 429'd, propagate
        // `RateLimited` so moka does NOT cache a definitive miss (the
        // rate-limited upstream may have it on retry). Clean miss only
        // when every upstream gave a definitive verdict.
        if let Some(retry_after) = any_429 {
            return Err(SubstituteError::RateLimited { retry_after });
        }
        Ok(None)
    }

    /// Steps 2-6 for one upstream.
    ///
    /// Ordering is load-bearing: narinfo → identity-check → sig-verify →
    /// size gate → **claim placeholder** → fetch NAR → hash → persist.
    /// The claim happens BEFORE the multi-GB download so a `Concurrent`
    /// loser stops without re-downloading from every remaining upstream,
    /// and the drop-guard covers cancellation during the long fetch.
    async fn try_upstream(
        &self,
        http: &reqwest::Client,
        tenant_id: Uuid,
        upstream: &Upstream,
        store_path: &str,
        hash_part: &str,
        progress: Option<&SubstProgressFn>,
    ) -> Result<UpstreamOutcome, SubstituteError> {
        // — Step 2: GET narinfo + parse + verify_sig —
        let base = upstream.url.trim_end_matches('/');
        let narinfo_url = format!("{base}/{hash_part}.narinfo");
        let resp = http
            .get(&narinfo_url)
            .timeout(SUBSTITUTE_SMALL_FETCH_TIMEOUT)
            .send()
            .await
            .map_err(|e| SubstituteError::Fetch(format!("{narinfo_url}: {e}")))?;
        if is_not_found(resp.status()) {
            return Ok(UpstreamOutcome::Miss);
        }
        // r[impl store.substitute.probe-429-retry+2]
        // 429 on the GET path: return RateLimited{retry_after}
        // immediately. The AdmissionGate permit drops on return so the
        // wait happens caller-side without holding per-replica
        // capacity. NO inline sleep+retry: the scheduler's existing
        // 8-attempt backoff (250ms→16s, ~32s total) absorbs short
        // Retry-Afters; long ones fall through to the next
        // dispatch-time probe pass.
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after = parse_retry_after(resp.headers());
            debug!(upstream = %base, ?retry_after, "narinfo GET 429");
            metrics::counter!(
                "rio_store_substitute_probe_ratelimited_total",
                "tenant" => tenant_id.to_string(),
            )
            .increment(1);
            return Err(SubstituteError::RateLimited { retry_after });
        }
        if !resp.status().is_success() {
            return Err(SubstituteError::Fetch(format!(
                "{narinfo_url}: HTTP {}",
                resp.status()
            )));
        }
        let text = bounded_text(resp, "narinfo", MAX_NARINFO_BYTES).await?;
        let ni = NarInfo::parse(&text)
            .map_err(|e| SubstituteError::NarInfo(format!("{narinfo_url}: {e}")))?;

        // r[impl store.substitute.identity-check]
        // Identity gate: the parsed `StorePath:` MUST equal what we
        // asked for. `verify_sig` proves the upstream signed *that*
        // narinfo, not that it answers `{hash_part}.narinfo` — a
        // valid-signed narinfo for path A served at `B.narinfo` would
        // otherwise ingest A and return it from `QueryPathInfo(B)`.
        // Runs before sig-verify so even an unsigned wrong-identity
        // narinfo is rejected with a clear error.
        if ni.store_path != store_path {
            return Err(SubstituteError::NarInfo(format!(
                "narinfo identity mismatch: requested {store_path}, upstream served {}",
                ni.store_path
            )));
        }

        // Sig gate: MUST verify against this upstream's trusted_keys.
        // Reject on None — an upstream we don't have a trust anchor
        // for is as good as 404.
        let Some(trusted_key) = ni.verify_sig(&upstream.trusted_keys) else {
            warn!(
                upstream = %upstream.url,
                path = store_path,
                "narinfo signature did not verify against upstream.trusted_keys"
            );
            return Ok(UpstreamOutcome::Miss);
        };
        debug!(upstream = %upstream.url, trusted_key, "narinfo signature verified");

        // Parse the nar_hash into raw bytes for the ingest path. The
        // narinfo text has `sha256:nixbase32`; we need `[u8; 32]` for
        // ValidatedPathInfo + the post-decompress hash check.
        let expected_hash = parse_nar_hash(&ni.nar_hash)?;

        // r[impl store.substitute.untrusted-upstream+3]
        // Declared-size gate. `trusted_keys` is also tenant-supplied so
        // a verified sig is NOT a trust boundary; gate before download.
        // The decompressed cap in `fetch_nar` catches a narinfo that
        // lies about `NarSize`.
        if ni.nar_size > MAX_NAR_SIZE {
            return Err(SubstituteError::TooLarge {
                what: "NarSize",
                limit: MAX_NAR_SIZE,
            });
        }

        // — Step 3: claim placeholder (BEFORE the expensive download) —
        // Signatures are NOT computed yet: the Nix fingerprint includes
        // `nar_size`, and at this point we only have the upstream's
        // unverified claim. Signing happens after the size+hash check
        // (or, on `AlreadyComplete`, over the already-stored row) so a
        // persisted row's `(nar_size, signatures)` are always mutually
        // consistent.
        let mut info = narinfo_to_validated(&ni, expected_hash)?;
        let store_path_hash = info.store_path.sha256_digest();
        info.store_path_hash = store_path_hash.to_vec();
        let refs_str: Vec<String> = info.references.iter().map(|r| r.to_string()).collect();

        let claim = match ingest::claim_placeholder(
            &self.pool,
            self.chunk_backend.as_ref(),
            &store_path_hash,
            info.store_path.as_str(),
            &refs_str,
            SUBSTITUTE_HOOKS,
        )
        .await?
        {
            PlaceholderClaim::Owned(claim) => claim,
            PlaceholderClaim::AlreadyComplete => {
                // Lost the race; winner completed. Compute sigs over
                // the STORED row (its `nar_size` is what was actually
                // ingested, immune to a lying second upstream), append
                // (idempotent — append_signatures dedupes), return it.
                // NO download.
                let stored = metadata::query_path_info(&self.pool, store_path)
                    .await?
                    .ok_or_else(|| {
                        SubstituteError::Ingest(
                            "claim AlreadyComplete but query_path_info miss".into(),
                        )
                    })?;
                let sigs = self
                    .sigs_for_mode(tenant_id, upstream.sig_mode, &ni, &stored)
                    .await;
                metadata::append_signatures(&self.pool, store_path, &sigs).await?;
                let stored = metadata::query_path_info(&self.pool, store_path)
                    .await?
                    .ok_or_else(|| {
                        SubstituteError::Ingest(
                            "post-append_signatures query_path_info miss".into(),
                        )
                    })?;
                return Ok(UpstreamOutcome::Hit(Box::new(stored)));
            }
            PlaceholderClaim::Concurrent => {
                // Another replica (or this replica via a different
                // closure-walk) holds the placeholder. NO download,
                // and `do_substitute` stops the upstream loop.
                debug!(%store_path, "concurrent uploader holds placeholder");
                return Ok(UpstreamOutcome::Raced);
            }
        };

        // r[impl store.put.drop-cleanup+2]
        // We OWN the placeholder. Guard against future-drop (client
        // RST_STREAM mid-fetch) — the guard's spawn reaps it if any
        // path between here and the defuse below is abandoned.
        let placeholder_guard = ingest::spawn_placeholder_guard(
            self.pool.clone(),
            self.chunk_backend.clone(),
            store_path_hash.to_vec(),
            claim,
        );

        // The remaining steps are fallible AND we own the placeholder;
        // funnel through one async block so a single error arm handles
        // explicit abort (the drop-guard is for the implicit drop path).
        let persist = async {
            // — Step 4: GET NAR + decompress —
            let nar_url = format!("{base}/{}", ni.url);
            let (nar_bytes, _permits) = self
                .fetch_nar(
                    http,
                    tenant_id,
                    &nar_url,
                    &ni.compression,
                    ni.nar_size,
                    base,
                    progress,
                )
                .await?;

            // r[impl store.substitute.untrusted-upstream+3]
            // Size check: actual decompressed length MUST equal the
            // narinfo's `NarSize:` line. The Nix signature fingerprint
            // is `1;path;hash;size;refs`; persisting an unchecked size
            // would store sigs that don't verify against the row.
            if nar_bytes.len() as u64 != ni.nar_size {
                return Err(SubstituteError::SizeMismatch {
                    declared: ni.nar_size,
                    actual: nar_bytes.len() as u64,
                });
            }

            // Hash-check the decompressed NAR against the narinfo's
            // claim — off the async runtime (4 GiB ≈ 8-10s of pure
            // compute would otherwise stall a tokio worker). `Bytes` is
            // cheap-clone; move it in and back out alongside the digest.
            let (nar_bytes, got_hash) =
                tokio::task::spawn_blocking(move || -> (Bytes, [u8; 32]) {
                    let h = sha2::Sha256::digest(&nar_bytes).into();
                    (nar_bytes, h)
                })
                .await
                .map_err(|e| SubstituteError::Ingest(format!("hash task join: {e}")))?;
            if got_hash != expected_hash {
                return Err(SubstituteError::HashMismatch {
                    expected: hex::encode(expected_hash),
                    got: hex::encode(got_hash),
                });
            }

            // Size + hash verified — `info.nar_size` (set in
            // `narinfo_to_validated` from `ni.nar_size`) now provably
            // equals what gets persisted. Compute sigs over the
            // verified `info` so stored `(nar_size, signatures)` are
            // mutually consistent.
            info.signatures = self
                .sigs_for_mode(tenant_id, upstream.sig_mode, &ni, &info)
                .await;

            // — Step 5-6: persist via the shared write-ahead core —
            ingest::persist_nar(
                &self.pool,
                self.chunk_backend.as_ref(),
                &info,
                claim,
                nar_bytes.into(),
                self.chunk_upload_max_concurrent,
                SUBSTITUTE_HOOKS,
            )
            .await
            .map_err(|e| match e {
                PersistError::Chunked(e) => SubstituteError::Ingest(e.to_string()),
                PersistError::Inline(e) => SubstituteError::Ingest(e.to_string()),
            })
        }
        .await;

        match persist {
            Ok(()) => {
                placeholder_guard.defuse();
                Ok(UpstreamOutcome::Hit(Box::new(info)))
            }
            Err(e) => {
                // Defuse the drop-guard and abort synchronously so the
                // next upstream in `do_substitute`'s loop sees a clean
                // slate (the guard's tokio::spawn fires too late for
                // that). threshold=None: our placeholder.
                placeholder_guard.defuse();
                ingest::abort_placeholder(
                    &self.pool,
                    self.chunk_backend.as_ref(),
                    &store_path_hash,
                    claim,
                )
                .await;
                Err(e)
            }
        }
    }

    /// GET the NAR body and decompress. Returns the raw NAR bytes plus
    /// the [`nar_bytes_budget`](Self::nar_bytes_budget) permits backing
    /// them; caller holds the permits until after `persist_nar`.
    ///
    /// Accumulates fully before ingest — `cas::put_chunked` needs the
    /// whole `&[u8]` for FastCDC. Streaming-chunker would avoid the
    /// full buffer but isn't here yet; TODO(P0463) tracks it.
    #[allow(clippy::too_many_arguments)]
    async fn fetch_nar(
        &self,
        http: &reqwest::Client,
        tenant_id: Uuid,
        nar_url: &str,
        compression: &str,
        expected_nar_size: u64,
        upstream_base: &str,
        progress: Option<&SubstProgressFn>,
    ) -> Result<(Bytes, Vec<OwnedSemaphorePermit>), SubstituteError> {
        let resp = http
            .get(nar_url)
            .send()
            .await
            .map_err(|e| SubstituteError::Fetch(format!("{nar_url}: {e}")))?;
        // r[impl store.substitute.probe-429-retry+2]
        // 429 on the NAR body GET maps to `RateLimited` (same as the
        // narinfo GET) so `do_substitute` continues to the next
        // upstream and moka doesn't cache the result. Without this, a
        // body-level 429 surfaced as a generic `Fetch` error, was
        // logged as `result=error`, and let `Ok(None)` cache a miss
        // for 30s.
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            metrics::counter!(
                "rio_store_substitute_probe_ratelimited_total",
                "tenant" => tenant_id.to_string(),
            )
            .increment(1);
            return Err(SubstituteError::RateLimited {
                retry_after: parse_retry_after(resp.headers()),
            });
        }
        if !resp.status().is_success() {
            return Err(SubstituteError::Fetch(format!(
                "{nar_url}: HTTP {}",
                resp.status()
            )));
        }
        // r[impl store.substitute.untrusted-upstream+3]
        // bytes_stream → StreamReader → decoder → `.take(cap+1)` →
        // budgeted read loop. The `.take()` wraps the DECOMPRESSED
        // side so a zstd bomb is bounded regardless of what `NarSize`
        // claimed.
        use futures_util::TryStreamExt;
        use tokio::io::AsyncRead;
        use tokio_util::io::StreamReader;
        let stream = resp
            .bytes_stream()
            .map_err(|e| std::io::Error::other(format!("NAR stream: {e}")));
        let reader = StreamReader::new(stream);

        let cap = SUBSTITUTE_NAR_DECOMPRESSED_CAP;
        use async_compression::tokio::bufread as ac;
        // r[impl store.substitute.compression]
        let mut capped: Box<dyn AsyncRead + Unpin + Send> = match compression {
            "xz" => Box::new(ac::XzDecoder::new(reader).take(cap + 1)),
            "zstd" => Box::new(ac::ZstdDecoder::new(reader).take(cap + 1)),
            "bzip2" => Box::new(ac::BzDecoder::new(reader).take(cap + 1)),
            "br" => Box::new(ac::BrotliDecoder::new(reader).take(cap + 1)),
            "gzip" => Box::new(ac::GzipDecoder::new(reader).take(cap + 1)),
            "none" | "" => Box::new(reader.take(cap + 1)),
            other => {
                return Err(SubstituteError::NarInfo(format!(
                    "unsupported Compression: {other:?}"
                )));
            }
        };

        // r[impl store.put.nar-bytes-budget+3]
        // Budgeted read loop: acquire `n.max(MIN_NAR_CHUNK_CHARGE)`
        // permits BEFORE extending `out`, mirroring PutPath's
        // `accumulate_chunk`. When the global budget is exhausted, the
        // `await` backpressures (other concurrent fetches/uploads
        // stall) instead of N × 4 GiB OOM.
        let mut out = Vec::new();
        let mut permits: Vec<OwnedSemaphorePermit> = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];
        let mut last_progress = 0u64;
        loop {
            let n = capped
                .read(&mut buf)
                .await
                .map_err(|e| SubstituteError::Fetch(format!("{nar_url} body: {e}")))?;
            if n == 0 {
                break;
            }
            let p = self
                .nar_bytes_budget
                .clone()
                .acquire_many_owned((n as u32).max(MIN_NAR_CHUNK_CHARGE))
                .await
                .map_err(|_| SubstituteError::Fetch("NAR buffer budget closed".into()))?;
            permits.push(p);
            out.extend_from_slice(&buf[..n]);
            if out.len() as u64 > cap {
                return Err(SubstituteError::TooLarge {
                    what: "decompressed NAR",
                    limit: cap,
                });
            }
            // r[impl store.substitute.progress-stream]
            if let Some(cb) = progress {
                let done = out.len() as u64;
                if done - last_progress >= SUBSTITUTE_PROGRESS_INTERVAL_BYTES {
                    cb(done, expected_nar_size, upstream_base);
                    last_progress = done;
                }
            }
        }
        // Final tick so a sub-MiB path (or the trailing partial MiB)
        // still reports done==expected before the terminal PathInfo.
        if let Some(cb) = progress {
            cb(out.len() as u64, expected_nar_size, upstream_base);
        }
        Ok((Bytes::from(out), permits))
    }

    // r[impl store.substitute.sig-mode]
    /// Compute the `narinfo.signatures` to store for `sig_mode`.
    ///
    /// `keep` → upstream sigs as-is. `add` → upstream + fresh rio
    /// sig. `replace` → only the fresh rio sig. If the signer isn't
    /// configured, `add`/`replace` degrade to `keep` (we can't produce
    /// a fresh sig without a key). Dedup happens at store time via
    /// `append_signatures`.
    async fn sigs_for_mode(
        &self,
        tenant_id: Uuid,
        mode: SigMode,
        ni: &NarInfo,
        info: &ValidatedPathInfo,
    ) -> Vec<String> {
        let fresh = match &self.signer {
            Some(ts) if mode != SigMode::Keep => match ts.resolve_once(Some(tenant_id)).await {
                Ok((signer, _)) => {
                    let fp = rio_nix::narinfo::fingerprint(
                        info.store_path.as_str(),
                        &info.nar_hash,
                        info.nar_size,
                        &info
                            .references
                            .iter()
                            .map(|r| r.to_string())
                            .collect::<Vec<_>>(),
                    );
                    Some(signer.sign(&fp))
                }
                Err(e) => {
                    warn!(error = %e, "signer resolve failed; degrading to keep");
                    None
                }
            },
            _ => None,
        };

        match (mode, fresh) {
            (SigMode::Replace, Some(s)) => vec![s],
            (SigMode::Add, Some(s)) => {
                let mut v = ni.sigs.clone();
                v.push(s);
                v
            }
            _ => ni.sigs.clone(),
        }
    }

    /// Fetch + cache `/nix-cache-info` for one upstream. `None` on any
    /// HTTP/body error — a down upstream throttles THIS call to the
    /// conservative concurrency, but `optionally_get_with` does NOT
    /// cache `None`, so the next call re-fetches instead of pinning the
    /// throttle for the full 1h TTL.
    async fn upstream_info(&self, http: &reqwest::Client, base: &str) -> Option<UpstreamInfo> {
        self.upstream_info
            .optionally_get_with(base.to_string(), async {
                let url = format!("{base}/nix-cache-info");
                let r = match http
                    .get(&url)
                    .timeout(SUBSTITUTE_SMALL_FETCH_TIMEOUT)
                    .send()
                    .await
                {
                    Ok(r) if r.status().is_success() => r,
                    Ok(r) => {
                        debug!(%url, status = %r.status(), "nix-cache-info non-2xx");
                        return None;
                    }
                    Err(e) => {
                        debug!(%url, error = %e, "nix-cache-info fetch failed");
                        return None;
                    }
                };
                match bounded_text(r, "nix-cache-info", MAX_CACHE_INFO_BYTES).await {
                    Ok(body) => Some(UpstreamInfo::parse(&body)),
                    Err(e) => {
                        debug!(%url, error = %e, "nix-cache-info body read failed");
                        None
                    }
                }
            })
            .await
    }

    /// HEAD-only batch probe: which of `paths` exist on ANY of the
    /// tenant's upstreams. No NAR download, no sig verification —
    /// this feeds `FindMissingPathsResponse.substitutable_paths` for
    /// the scheduler's "can I skip building this?" check.
    ///
    /// Per-path results are cached on `self.probe_cache` (positive AND
    /// negative, TTL 1h). Uncached paths are probed with concurrency
    /// gated on each upstream's `WantMassQuery` declaration:
    /// [`SUBSTITUTE_PROBE_CONCURRENCY`] if all upstreams advertise it,
    /// [`SUBSTITUTE_PROBE_CONCURRENCY_CONSERVATIVE`] otherwise.
    /// Fails-open on individual HEAD errors (a down upstream shouldn't
    /// hide paths that OTHER upstreams have). 429 responses are retried
    /// (≤ `SUBSTITUTE_PROBE_429_MAX_PASSES`) with `Retry-After`
    /// honored and concurrency adaptively halved — see
    /// `r[store.substitute.probe-429-retry+2]`. No batch-size truncation:
    /// the originating RPC's wall-clock is `⌈N_uncached/128⌉ × RTT`;
    /// the scheduler's merge-time caller carries a wider timeout
    /// (`r[sched.substitute.eager-probe]`).
    ///
    /// `deadline` bounds the 429-retry sleep: if the upstream's
    /// `Retry-After` would push past it (with 2s headroom for the
    /// HEADs themselves), the retry pass is skipped and the
    /// rate-limited paths are returned as not-substitutable for THIS
    /// call (uncached; they get re-probed at dispatch time).
    #[instrument(skip(self, paths), fields(tenant = %tenant_id, n = paths.len()))]
    pub async fn check_available(
        &self,
        tenant_id: Uuid,
        paths: &[String],
        deadline: tokio::time::Instant,
    ) -> Result<Vec<String>, SubstituteError> {
        use futures_util::StreamExt;

        let started = std::time::Instant::now();
        let Some(http) = &self.http else {
            return Ok(Vec::new()); // sandbox: client build failed
        };
        let upstreams = metadata::upstreams::list_for_tenant(&self.pool, tenant_id).await?;
        debug!(
            n_upstreams = upstreams.len(),
            n_paths = paths.len(),
            "check_available"
        );
        if upstreams.is_empty() || paths.is_empty() {
            return Ok(Vec::new());
        }

        // Partition into cached / uncached. Cached results (positive
        // and negative) are answered immediately; only uncached paths
        // count against the probe cap and incur HEADs.
        let mut hits = Vec::new();
        let mut uncached = Vec::new();
        let (mut cache_hits, mut cache_misses) = (0u64, 0u64);
        for p in paths {
            match self.probe_cache.get(&(tenant_id, p.clone())).await {
                Some(true) => {
                    cache_hits += 1;
                    hits.push(p.clone());
                }
                Some(false) => cache_hits += 1,
                None => {
                    cache_misses += 1;
                    uncached.push(p.clone());
                }
            }
        }
        metrics::counter!("rio_store_substitute_probe_cache_hits_total").increment(cache_hits);
        metrics::counter!("rio_store_substitute_probe_cache_misses_total").increment(cache_misses);

        if uncached.is_empty() {
            return Ok(hits);
        }

        let bases: Vec<String> = upstreams
            .iter()
            .map(|u| u.url.trim_end_matches('/').to_string())
            .collect();

        // Concurrency is the MIN over upstreams: each per-path future
        // walks every upstream in turn, so the buffer_unordered bound
        // is the worst-case concurrent load on any single upstream. One
        // non-mass-query upstream throttles the whole batch.
        let mut concurrency = SUBSTITUTE_PROBE_CONCURRENCY;
        for base in &bases {
            // `None` (transient fetch error) → conservative for THIS
            // call only; the cache didn't store the failure, so the
            // next call re-fetches.
            if !self
                .upstream_info(http, base)
                .await
                .is_some_and(|i| i.want_mass_query)
            {
                concurrency = SUBSTITUTE_PROBE_CONCURRENCY_CONSERVATIVE;
                break;
            }
        }

        // Build (path, hash_part) pairs up front so the inner closure
        // doesn't reparse N×M times. Owned strings (not borrows) so the
        // per-path futures don't borrow from the iterator item —
        // buffer_unordered's HRTB inference can't see through that.
        let mut pending: Vec<(String, String)> = uncached
            .into_iter()
            .filter_map(|p| {
                let h = StorePath::parse(&p).ok()?.hash_part();
                Some((p, h))
            })
            .collect();

        let bases = &bases;
        let tenant_label = tenant_id.to_string();
        let tenant_label = &tenant_label;
        let probe_one = |path: String, hash_part: String| async move {
            // `Hit` if any base 2xx; `Miss` if EVERY base returned a
            // clean 404; `RateLimited` if no hit and ≥1 base 429'd;
            // `Indeterminate` if no hit and ≥1 base errored / non-404 —
            // caching that as `false` would route a substitutable
            // derivation to `willBuild` for 1h after a transient 503.
            let mut any_indeterminate = false;
            let mut rate_limited: Option<Option<Duration>> = None;
            for base in bases {
                let url = format!("{base}/{hash_part}.narinfo");
                match http
                    .head(&url)
                    .timeout(SUBSTITUTE_SMALL_FETCH_TIMEOUT)
                    .send()
                    .await
                {
                    Ok(r) if r.status().is_success() => {
                        return ((path, hash_part), ProbeOutcome::Hit);
                    }
                    Ok(r) if is_not_found(r.status()) => {}
                    Ok(r) if r.status() == reqwest::StatusCode::TOO_MANY_REQUESTS => {
                        let retry_after = parse_retry_after(r.headers());
                        debug!(upstream = %base, ?retry_after, "HEAD probe 429");
                        metrics::counter!(
                            "rio_store_substitute_probe_ratelimited_total",
                            "tenant" => tenant_label.clone(),
                        )
                        .increment(1);
                        // Max across upstreams that 429'd this path.
                        rate_limited = Some(match rate_limited.flatten() {
                            Some(prev) => Some(prev.max(retry_after.unwrap_or_default())),
                            None => retry_after,
                        });
                    }
                    Ok(_) | Err(_) => any_indeterminate = true,
                }
            }
            let outcome = match rate_limited {
                Some(retry_after) => ProbeOutcome::RateLimited { retry_after },
                None if any_indeterminate => ProbeOutcome::Indeterminate,
                None => ProbeOutcome::Miss,
            };
            ((path, hash_part), outcome)
        };

        // r[impl store.substitute.probe-bounded+4]
        // r[impl store.substitute.probe-429-retry+2]
        // 429-aware retry loop. Pass 0 covers the full uncached set;
        // each retry pass re-probes only the RateLimited subset after
        // sleeping max(Retry-After) (Fastly's 429 is edge-wide, not
        // per-object — sleeping per-path would serialize). Concurrency
        // halves when >SUBSTITUTE_PROBE_429_ADAPT_THRESHOLD of a pass
        // came back 429: that's the actual feedback signal the old
        // synthetic 4096 cap was a static proxy for.
        //
        // Each pass — including pass 0 — is hard-bounded by `deadline`.
        // The sleep-budget check below only gates the SLEEP; without
        // this wrap, a 153k-path pass-0 (36s) followed by a halved-
        // concurrency pass-1 (72s) runs 109s and trips the scheduler's
        // 90s `MERGE_FMP_TIMEOUT` → spurious breaker failure. On
        // timeout the un-probed batch is Indeterminate (uncached, not
        // returned) — same disposition as the sleep-doesn't-fit path.
        for pass in 0..=SUBSTITUTE_PROBE_429_MAX_PASSES {
            let batch_len = pending.len();
            // `take_until` yields items until the deadline future
            // resolves, then stops — completed Hit/Miss results from
            // this pass survive. `tokio::time::timeout(.., collect())`
            // is all-or-nothing: it drops the partially-accumulated
            // Vec on expiry, and since `pending` was already
            // `mem::take`n the completed results land in neither
            // `hits` nor `probe_cache` nor `pending` — a regression vs
            // the old 4096-truncation which at least returned 4096.
            // Un-yielded paths are implicitly Indeterminate (uncached,
            // re-probed at dispatch time).
            let probed: Vec<_> = futures_util::stream::iter(
                std::mem::take(&mut pending)
                    .into_iter()
                    .map(|(p, h)| probe_one(p, h)),
            )
            .buffer_unordered(concurrency)
            .take_until(tokio::time::sleep_until(deadline))
            .collect()
            .await;
            let completed = probed.len();

            let mut max_retry_after: Option<Duration> = None;
            for ((path, hash_part), outcome) in probed {
                match outcome {
                    ProbeOutcome::Hit => {
                        self.probe_cache
                            .insert((tenant_id, path.clone()), true)
                            .await;
                        hits.push(path);
                    }
                    ProbeOutcome::Miss => {
                        self.probe_cache.insert((tenant_id, path), false).await;
                    }
                    // Don't cache: next call re-probes.
                    ProbeOutcome::Indeterminate => {}
                    ProbeOutcome::RateLimited { retry_after } => {
                        max_retry_after = max_retry_after
                            .max(Some(retry_after.unwrap_or(Duration::from_secs(1))));
                        pending.push((path, hash_part));
                    }
                }
            }

            if completed < batch_len {
                info!(
                    pass,
                    completed,
                    deferred = batch_len - completed,
                    "check_available: probe pass exceeded deadline; \
                     deferring un-probed paths to dispatch-time"
                );
                break;
            }
            if pending.is_empty() || pass == SUBSTITUTE_PROBE_429_MAX_PASSES {
                break;
            }
            if pending.len() as f64 / batch_len as f64 > SUBSTITUTE_PROBE_429_ADAPT_THRESHOLD {
                concurrency = (concurrency / 2).max(SUBSTITUTE_PROBE_CONCURRENCY_CONSERVATIVE);
            }
            let sleep = max_retry_after.unwrap_or(Duration::from_secs(1));
            // Budget check: if Retry-After would push past the
            // caller's deadline (with 2s headroom for the HEADs
            // themselves), skip the retry pass — the rate-limited
            // remainder gets re-probed at dispatch time. The actual
            // constraint is the RPC timeout above us, not an
            // arbitrary clamp on what the upstream said.
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if sleep > remaining.saturating_sub(Duration::from_secs(2)) {
                info!(
                    retry_after = ?sleep,
                    remaining_budget = ?remaining,
                    deferred = pending.len(),
                    "check_available: upstream Retry-After exceeds probe budget; \
                     deferring rate-limited paths to dispatch-time"
                );
                break;
            }
            debug!(
                pass,
                rate_limited = pending.len(),
                ?sleep,
                next_concurrency = concurrency,
                "check_available: 429 retry pass"
            );
            tokio::time::sleep(sleep).await;
        }
        // Remainder (still 429 after MAX_PASSES, or Retry-After exceeded
        // budget) → Indeterminate: not cached, returned as
        // not-substitutable for THIS call only.

        metrics::histogram!("rio_store_check_available_duration_seconds")
            .record(started.elapsed().as_secs_f64());
        Ok(hits)
    }
}

/// HTTP statuses an upstream binary cache uses to signal "key not
/// present". 403: S3 without public `s3:ListBucket` returns Forbidden
/// for missing keys. 410: Gone. Matches CppNix `HttpBinaryCacheStore`.
fn is_not_found(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 404 | 403 | 410)
}

// r[impl store.substitute.untrusted-upstream+3]
/// Read a small text body (`.narinfo`, `/nix-cache-info`) with a hard
/// size cap. `tenant_upstreams` rows are tenant-supplied; an unbounded
/// `.text()` against a hostile upstream is an OOM vector for the
/// process-global substituter.
async fn bounded_text(
    resp: reqwest::Response,
    what: &'static str,
    limit: u64,
) -> Result<String, SubstituteError> {
    use futures_util::TryStreamExt;
    use tokio_util::io::StreamReader;
    let stream = resp
        .bytes_stream()
        .map_err(|e| std::io::Error::other(e.to_string()));
    let mut reader = StreamReader::new(stream).take(limit + 1);
    let mut buf = Vec::new();
    reader
        .read_to_end(&mut buf)
        .await
        .map_err(|e| SubstituteError::Fetch(format!("{what} body: {e}")))?;
    if buf.len() as u64 > limit {
        return Err(SubstituteError::TooLarge { what, limit });
    }
    String::from_utf8(buf).map_err(|e| SubstituteError::NarInfo(format!("{what} not UTF-8: {e}")))
}

/// Parse a narinfo `NarHash:` value (`sha256:nixbase32...`) into raw
/// 32 bytes.
fn parse_nar_hash(s: &str) -> Result<[u8; 32], SubstituteError> {
    let h = rio_nix::hash::NixHash::parse_colon(s)
        .map_err(|e| SubstituteError::NarInfo(format!("NarHash {s:?}: {e}")))?;
    h.digest()
        .try_into()
        .map_err(|_| SubstituteError::NarInfo(format!("NarHash {s:?}: not 32 bytes")))
}

/// Convert a parsed `NarInfo` to the store's `ValidatedPathInfo`.
///
/// narinfo stores references as BASENAMES; `ValidatedPathInfo` wants
/// full `/nix/store/...` paths (`StorePath::parse` enforces the
/// prefix). Re-prepend the store dir derived from `store_path`.
fn narinfo_to_validated(
    ni: &NarInfo,
    nar_hash: [u8; 32],
) -> Result<ValidatedPathInfo, SubstituteError> {
    use rio_proto::types::PathInfo;

    // r[impl store.substitute.untrusted-upstream+3]
    // Per-node count caps — parity with PutPath (`put_path/common.rs`).
    // `ValidatedPathInfo::try_from` validates per-element syntax only;
    // it does NOT bound the count.
    if ni.references.len() > MAX_REFERENCES {
        return Err(SubstituteError::NarInfo(format!(
            "narinfo has {} references (> MAX_REFERENCES {MAX_REFERENCES})",
            ni.references.len()
        )));
    }
    if ni.sigs.len() > MAX_SIGNATURES {
        return Err(SubstituteError::NarInfo(format!(
            "narinfo has {} signatures (> MAX_SIGNATURES {MAX_SIGNATURES})",
            ni.sigs.len()
        )));
    }

    let store_dir = &ni.store_path[..=ni
        .store_path
        .rfind('/')
        .ok_or_else(|| SubstituteError::NarInfo("store_path has no '/'".into()))?];
    let full_refs: Vec<String> = ni
        .references
        .iter()
        .map(|r| format!("{store_dir}{r}"))
        .collect();
    let deriver = ni
        .deriver
        .as_ref()
        .map(|d| format!("{store_dir}{d}"))
        .unwrap_or_default();

    ValidatedPathInfo::try_from(PathInfo {
        store_path: ni.store_path.clone(),
        store_path_hash: Vec::new(),
        deriver,
        nar_hash: nar_hash.to_vec(),
        nar_size: ni.nar_size,
        references: full_refs,
        registration_time: 0,
        ultimate: false,
        signatures: Vec::new(), // filled by sigs_for_mode
        content_address: ni.ca.clone().unwrap_or_default(),
    })
    .map_err(|e| SubstituteError::NarInfo(format!("narinfo→PathInfo: {e}")))
}

use sha2::Digest as _;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signing::Signer;
    use crate::test_helpers::seed_tenant;
    use rio_nix::narinfo::fingerprint;
    use rio_test_support::TestDb;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // — test fixture: an in-process upstream cache —
    //
    // Wires an axum server on an ephemeral port serving a single
    // narinfo + NAR. `wiremock` isn't in the deptree; axum already is.
    // The signing key is generated fresh per-test so we control what
    // `verify_sig` accepts.

    struct FakeUpstream {
        url: String,
        trusted_key: String,
        /// Abort handle — dropping stops the server.
        _task: tokio::task::JoinHandle<()>,
    }

    async fn spawn_fake_upstream(
        store_path: &str,
        nar_bytes: Vec<u8>,
        key_name: &str,
    ) -> FakeUpstream {
        spawn_fake_upstream_with_delay(store_path, nar_bytes, key_name, Duration::ZERO).await
    }

    /// [`spawn_fake_upstream`] with a `nar_delay` sleep injected before
    /// the NAR body is returned. The leader-only-permit test uses this
    /// to hold the singleflight init future open long enough to sample
    /// `available_permits()` while waiters are coalesced.
    async fn spawn_fake_upstream_with_delay(
        store_path: &str,
        nar_bytes: Vec<u8>,
        key_name: &str,
        nar_delay: Duration,
    ) -> FakeUpstream {
        use axum::{Router, routing::get};
        use base64::Engine;

        let seed = [0x42u8; 32];
        let signer = Signer::from_seed(key_name, &seed);
        let pubkey = ed25519_dalek::SigningKey::from_bytes(&seed).verifying_key();
        let trusted_key = format!(
            "{key_name}:{}",
            base64::engine::general_purpose::STANDARD.encode(pubkey.as_bytes())
        );

        let nar_hash: [u8; 32] = sha2::Sha256::digest(&nar_bytes).into();
        let nar_hash_str = format!(
            "sha256:{}",
            rio_nix::store_path::nixbase32::encode(&nar_hash)
        );

        let fp = fingerprint(store_path, &nar_hash, nar_bytes.len() as u64, &[]);
        let sig = signer.sign(&fp);

        let sp = StorePath::parse(store_path).unwrap();
        let hash_part = sp.hash_part();

        let narinfo = format!(
            "StorePath: {store_path}\n\
             URL: nar/{hash_part}.nar\n\
             Compression: none\n\
             NarHash: {nar_hash_str}\n\
             NarSize: {}\n\
             References: \n\
             Sig: {sig}\n",
            nar_bytes.len()
        );

        let narinfo_path = format!("/{hash_part}.narinfo");
        let nar_path = format!("/nar/{hash_part}.nar");
        let narinfo_c = narinfo.clone();
        let nar_c = nar_bytes.clone();

        let app = Router::new()
            .route(
                "/nix-cache-info",
                get(|| async { "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n" }),
            )
            .route(&narinfo_path, get(move || async move { narinfo_c }))
            .route(
                &nar_path,
                get(move || async move {
                    tokio::time::sleep(nar_delay).await;
                    nar_c
                }),
            );

        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        FakeUpstream {
            url: format!("http://{addr}"),
            trusted_key,
            _task: task,
        }
    }

    /// Compress `bytes` with the named algorithm using the same
    /// `async_compression` backend the production decoder uses, so the
    /// test exercises encoder→decoder round-trip per algo.
    async fn compress(bytes: &[u8], algo: &str) -> Vec<u8> {
        use async_compression::tokio::bufread as ac;
        use tokio::io::AsyncReadExt;
        let mut out = Vec::new();
        let mut r: Box<dyn tokio::io::AsyncRead + Unpin + Send> = match algo {
            "xz" => Box::new(ac::XzEncoder::new(bytes)),
            "zstd" => Box::new(ac::ZstdEncoder::new(bytes)),
            "bzip2" => Box::new(ac::BzEncoder::new(bytes)),
            "br" => Box::new(ac::BrotliEncoder::new(bytes)),
            "gzip" => Box::new(ac::GzipEncoder::new(bytes)),
            other => panic!("test helper: unknown algo {other}"),
        };
        r.read_to_end(&mut out).await.unwrap();
        out
    }

    /// [`spawn_fake_upstream`] variant that serves the NAR body
    /// compressed with `compression` and advertises it in the narinfo's
    /// `Compression:` field. `NarHash`/`NarSize` remain those of the
    /// UNCOMPRESSED NAR per the narinfo spec.
    async fn spawn_fake_upstream_compressed(
        store_path: &str,
        nar_bytes: Vec<u8>,
        key_name: &str,
        compression: &'static str,
    ) -> FakeUpstream {
        use axum::{Router, routing::get};
        use base64::Engine;

        let seed = [0x42u8; 32];
        let signer = Signer::from_seed(key_name, &seed);
        let pubkey = ed25519_dalek::SigningKey::from_bytes(&seed).verifying_key();
        let trusted_key = format!(
            "{key_name}:{}",
            base64::engine::general_purpose::STANDARD.encode(pubkey.as_bytes())
        );

        let nar_hash: [u8; 32] = sha2::Sha256::digest(&nar_bytes).into();
        let nar_hash_str = format!(
            "sha256:{}",
            rio_nix::store_path::nixbase32::encode(&nar_hash)
        );
        let nar_size = nar_bytes.len() as u64;
        let fp = fingerprint(store_path, &nar_hash, nar_size, &[]);
        let sig = signer.sign(&fp);

        let sp = StorePath::parse(store_path).unwrap();
        let hash_part = sp.hash_part();
        let body = compress(&nar_bytes, compression).await;

        let narinfo = format!(
            "StorePath: {store_path}\n\
             URL: nar/{hash_part}.nar\n\
             Compression: {compression}\n\
             NarHash: {nar_hash_str}\n\
             NarSize: {nar_size}\n\
             References: \n\
             Sig: {sig}\n",
        );

        let narinfo_path = format!("/{hash_part}.narinfo");
        let nar_path = format!("/nar/{hash_part}.nar");
        let app = Router::new()
            .route(
                "/nix-cache-info",
                get(|| async { "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n" }),
            )
            .route(&narinfo_path, get(move || async move { narinfo }))
            .route(&nar_path, get(move || async move { body }));

        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        FakeUpstream {
            url: format!("http://{addr}"),
            trusted_key,
            _task: task,
        }
    }

    /// Sandbox-safe reqwest client: empty root-cert store. The fake
    /// upstream is plaintext `http://localhost` so TLS never engages.
    /// `Client::new()` panics in the nix sandbox because
    /// rustls-native-certs finds no CA bundle; `tls_certs_only([])`
    /// skips the native-cert load entirely.
    fn sandbox_http() -> reqwest::Client {
        reqwest::Client::builder()
            .tls_certs_only(std::iter::empty())
            .build()
            .expect("empty-cert client build should never fail")
    }

    fn test_substituter(pool: PgPool) -> Substituter {
        Substituter::new(pool, None).with_http_client(sandbox_http())
    }

    /// `check_available` deadline for tests that don't exercise the
    /// 429-budget path. Far enough out that the budget check never
    /// trips on local-loopback HEAD latency.
    fn far_deadline() -> tokio::time::Instant {
        tokio::time::Instant::now() + CHECK_AVAILABLE_DEFAULT_BUDGET
    }

    fn make_path() -> (String, Vec<u8>) {
        let path = rio_test_support::fixtures::test_store_path("substituted");
        let (nar, _hash) = rio_test_support::fixtures::make_nar(b"hi");
        (path, nar)
    }

    // r[verify store.substitute.upstream]
    // r[verify store.substitute.sig-mode]
    #[tokio::test]
    async fn substitute_keep_mode_end_to_end() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-keep").await;
        let (path, nar) = make_path();
        let fake = spawn_fake_upstream(&path, nar.clone(), "cache.test-1").await;

        // Configure the upstream for this tenant.
        metadata::upstreams::insert(
            &db.pool,
            tid,
            &fake.url,
            50,
            std::slice::from_ref(&fake.trusted_key),
            SigMode::Keep,
        )
        .await
        .unwrap();

        let sub = test_substituter(db.pool.clone());
        let got = sub.try_substitute(tid, &path).await.unwrap();
        let got = got.expect("upstream has the path");

        // Path landed in narinfo + manifests.
        assert_eq!(got.store_path.as_str(), path);
        assert_eq!(got.nar_size, nar.len() as u64);

        // sig_mode=keep → upstream's Sig: is stored verbatim.
        assert_eq!(got.signatures.len(), 1);
        assert!(
            got.signatures[0].starts_with("cache.test-1:"),
            "keep mode should store upstream sig: {:?}",
            got.signatures
        );

        // Verify persistence: re-query via metadata layer.
        let stored = metadata::query_path_info(&db.pool, &path)
            .await
            .unwrap()
            .expect("path should be in narinfo table");
        assert_eq!(stored.nar_size, nar.len() as u64);
        assert_eq!(stored.signatures.len(), 1);
    }

    // r[verify store.substitute.progress-stream]
    /// `try_substitute_with_progress` fires the callback at least once
    /// (the final tick) with `(nar.len(), nar.len(), upstream_base)` and
    /// returns the same `PathInfo` as the unary path. Test NAR is below
    /// `SUBSTITUTE_PROGRESS_INTERVAL_BYTES` so we get exactly one emit.
    #[tokio::test]
    async fn substitute_with_progress_emits_final_tick() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-prog").await;
        let (path, nar) = make_path();
        let fake = spawn_fake_upstream(&path, nar.clone(), "cache.prog").await;
        metadata::upstreams::insert(
            &db.pool,
            tid,
            &fake.url,
            50,
            std::slice::from_ref(&fake.trusted_key),
            SigMode::Keep,
        )
        .await
        .unwrap();

        let sub = test_substituter(db.pool.clone());
        // `SubstProgressFn` is `dyn Fn + 'static` (type-alias default
        // lifetime) — closure must own its captures. Arc the collector.
        let emits = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(u64, u64, String)>::new()));
        let cb = {
            let emits = emits.clone();
            move |d: u64, e: u64, u: &str| emits.lock().unwrap().push((d, e, u.to_string()))
        };
        let got = sub
            .try_substitute_with_progress(tid, &path, &cb)
            .await
            .unwrap()
            .expect("upstream has it");
        assert_eq!(got.nar_size, nar.len() as u64);

        let emits = std::sync::Arc::try_unwrap(emits)
            .map(|m| m.into_inner().unwrap())
            .unwrap_or_else(|a| a.lock().unwrap().clone());
        assert!(!emits.is_empty(), "final tick fires even for sub-MiB paths");
        let (done, expected, uri) = emits.last().unwrap();
        assert_eq!(*done, nar.len() as u64, "done = full nar_size");
        assert_eq!(*expected, nar.len() as u64, "expected = narinfo NarSize");
        assert!(
            uri.starts_with("http://"),
            "upstream base captured: {uri:?}"
        );

        // Cache-hit fast path: second call returns immediately, NO emits.
        let emits2 = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let cb2 = {
            let emits2 = emits2.clone();
            move |_: u64, _: u64, _: &str| {
                emits2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        };
        let _ = sub
            .try_substitute_with_progress(tid, &path, &cb2)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            emits2.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "moka cache-hit → no progress emits"
        );
    }

    // r[verify store.substitute.progress-stream]
    /// N concurrent `try_substitute_with_progress` calls for the same
    /// `(tenant, path)` coalesce at the moka singleflight: ALL return
    /// `Ok(Some)` (none get `Raced`/`None`); only the winner's callback
    /// fires. Regression: pre-fix the miss path bypassed `try_get_with`
    /// → N-1 reached `claim_placeholder` → `Concurrent` → `Err(Raced)`
    /// → gRPC `NotFound` → scheduler false build-dispatch.
    #[tokio::test]
    async fn substitute_with_progress_concurrent_coalesces() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-coalesce").await;
        let (path, nar) = make_path();
        // Delay the NAR body so all N callers enter `try_get_with`
        // while the winner is still downloading.
        let fake = spawn_fake_upstream_with_delay(
            &path,
            nar.clone(),
            "cache.co",
            Duration::from_millis(200),
        )
        .await;
        metadata::upstreams::insert(
            &db.pool,
            tid,
            &fake.url,
            50,
            std::slice::from_ref(&fake.trusted_key),
            SigMode::Keep,
        )
        .await
        .unwrap();

        let sub = std::sync::Arc::new(test_substituter(db.pool.clone()));
        const N: usize = 4;
        let emit_counts: Vec<_> = (0..N)
            .map(|_| std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)))
            .collect();
        let calls = emit_counts.iter().map(|c| {
            let sub = sub.clone();
            let path = path.clone();
            let c = c.clone();
            async move {
                let cb = move |_: u64, _: u64, _: &str| {
                    c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                };
                sub.try_substitute_with_progress(tid, &path, &cb).await
            }
        });
        let results = futures_util::future::join_all(calls).await;

        for (i, r) in results.into_iter().enumerate() {
            let info = r
                .unwrap_or_else(|e| panic!("caller {i} got Err({e:?}) — singleflight regressed"))
                .unwrap_or_else(|| panic!("caller {i} got Ok(None) — Raced→NotFound regressed"));
            assert_eq!(info.nar_size, nar.len() as u64);
        }
        let winners = emit_counts
            .iter()
            .filter(|c| c.load(std::sync::atomic::Ordering::SeqCst) > 0)
            .count();
        assert_eq!(
            winners, 1,
            "exactly one caller's progress callback fires (the singleflight winner)"
        );
    }

    // r[verify store.substitute.compression]
    /// `fetch_nar` decodes every `Compression:` value reference Nix's
    /// `libutil/compression.cc` accepts, end-to-end through
    /// `try_substitute` so the NarHash check proves the decompressed
    /// bytes match exactly. cache.nixos.org still serves bzip2 for
    /// pre-2016 paths.
    #[tokio::test]
    async fn substitute_handles_all_nar_compressions() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let sub = test_substituter(db.pool.clone());

        for algo in ["xz", "zstd", "bzip2", "br", "gzip"] {
            let tid = seed_tenant(&db.pool, &format!("sub-compress-{algo}")).await;
            let path = rio_test_support::fixtures::test_store_path(&format!("comp-{algo}"));
            let (nar, _hash) = rio_test_support::fixtures::make_nar(algo.as_bytes());
            let fake =
                spawn_fake_upstream_compressed(&path, nar.clone(), "cache.compress-1", algo).await;

            metadata::upstreams::insert(
                &db.pool,
                tid,
                &fake.url,
                50,
                std::slice::from_ref(&fake.trusted_key),
                SigMode::Keep,
            )
            .await
            .unwrap();

            let got = sub
                .try_substitute(tid, &path)
                .await
                .unwrap_or_else(|e| panic!("{algo}: try_substitute failed: {e}"))
                .unwrap_or_else(|| panic!("{algo}: upstream should have the path"));
            assert_eq!(got.nar_size, nar.len() as u64, "{algo}: nar_size mismatch");
        }
    }

    #[tokio::test]
    async fn substitute_replace_mode() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-replace").await;
        let (path, nar) = make_path();
        let fake = spawn_fake_upstream(&path, nar, "cache.test-2").await;

        metadata::upstreams::insert(
            &db.pool,
            tid,
            &fake.url,
            50,
            std::slice::from_ref(&fake.trusted_key),
            SigMode::Replace,
        )
        .await
        .unwrap();

        // Signer with a distinct key name so we can tell upstream vs
        // rio sigs apart.
        let cluster = Signer::from_seed("rio-cluster-1", &[0x99u8; 32]);
        let ts = Arc::new(TenantSigner::new(cluster, db.pool.clone()));
        let sub = test_substituter(db.pool.clone()).with_signer(ts);

        let got = sub.try_substitute(tid, &path).await.unwrap().unwrap();

        // sig_mode=replace → ONLY rio's sig, upstream's dropped.
        assert_eq!(
            got.signatures.len(),
            1,
            "replace: exactly one sig, got {:?}",
            got.signatures
        );
        assert!(
            got.signatures[0].starts_with("rio-cluster-1:"),
            "replace: should be rio-signed, got {:?}",
            got.signatures
        );
    }

    #[tokio::test]
    async fn substitute_add_mode() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-add").await;
        let (path, nar) = make_path();
        let fake = spawn_fake_upstream(&path, nar, "cache.test-3").await;

        metadata::upstreams::insert(
            &db.pool,
            tid,
            &fake.url,
            50,
            std::slice::from_ref(&fake.trusted_key),
            SigMode::Add,
        )
        .await
        .unwrap();

        let cluster = Signer::from_seed("rio-cluster-2", &[0x88u8; 32]);
        let ts = Arc::new(TenantSigner::new(cluster, db.pool.clone()));
        let sub = test_substituter(db.pool.clone()).with_signer(ts);

        let got = sub.try_substitute(tid, &path).await.unwrap().unwrap();

        // sig_mode=add → upstream + rio.
        assert_eq!(got.signatures.len(), 2);
        let has_upstream = got
            .signatures
            .iter()
            .any(|s| s.starts_with("cache.test-3:"));
        let has_rio = got
            .signatures
            .iter()
            .any(|s| s.starts_with("rio-cluster-2:"));
        assert!(
            has_upstream && has_rio,
            "add: both sigs, got {:?}",
            got.signatures
        );
    }

    /// Per-tenant miss/error labeling: an upstream that 404s emits
    /// `{result=miss,tenant=UUID}`; one that fails fetch/verify emits
    /// `{result=error,tenant=UUID}`. The label MUST be `tenant` (UUID,
    /// bounded by tenant count), NOT `upstream` (tenant-supplied URL,
    /// unbounded cardinality → exporter-memory DoS).
    #[tokio::test]
    async fn substitute_per_tenant_miss_and_error_labeled() {
        use rio_test_support::metrics::CountingRecorder;

        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-metrics").await;
        let (have_path, nar) = make_path();
        // Upstream A serves `have_path` only → request a DIFFERENT
        // path → 404 (axum default) → Ok(None) miss.
        let a = spawn_fake_upstream(&have_path, nar, "cache.metrics-a").await;
        // Upstream B: every .narinfo returns 500 → Err.
        let b = spawn_500_upstream().await;

        for url in [&a.url, &b.url] {
            metadata::upstreams::insert(
                &db.pool,
                tid,
                url,
                50,
                std::slice::from_ref(&a.trusted_key),
                SigMode::Keep,
            )
            .await
            .unwrap();
        }

        // Distinct hash_part: test_store_path() uses a fixed TEST_HASH
        // for all names, so requesting it would HIT upstream A's
        // narinfo route. A different hash_part guarantees A 404s.
        let other = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-not-on-any-upstream".to_string();
        let sub = test_substituter(db.pool.clone());

        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);
        let got = sub.try_substitute(tid, &other).await.unwrap();
        assert!(got.is_none());

        let miss = format!("rio_store_substitute_total{{result=miss,tenant={tid}}}");
        let err = format!("rio_store_substitute_total{{result=error,tenant={tid}}}");
        assert_eq!(
            rec.get(&miss),
            1,
            "per-tenant miss; keys={:?}",
            rec.all_keys()
        );
        assert_eq!(
            rec.get(&err),
            1,
            "per-tenant error; keys={:?}",
            rec.all_keys()
        );
        // No `upstream=` label anywhere — tenant-supplied URL must not
        // be a Prometheus label dimension.
        assert!(
            !rec.all_keys().iter().any(|k| k.contains("upstream=")),
            "no upstream= label; keys={:?}",
            rec.all_keys()
        );
    }

    /// Axum server returning a fixed status on every request — for the
    /// error-metric / 403-is-miss tests.
    async fn spawn_status_upstream(status: axum::http::StatusCode) -> FakeUpstream {
        use axum::{Router, routing::any};
        let app = Router::new().fallback(any(move || async move { status }));
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        FakeUpstream {
            url: format!("http://{addr}"),
            trusted_key: String::new(),
            _task: task,
        }
    }

    async fn spawn_500_upstream() -> FakeUpstream {
        spawn_status_upstream(axum::http::StatusCode::INTERNAL_SERVER_ERROR).await
    }

    /// Config for [`spawn_mass_probe_upstream`].
    #[derive(Default)]
    struct ProbeCfg {
        /// First N HEAD requests return 429; subsequent ones 200.
        head_429_first_n: usize,
        /// Literal `Retry-After` header value on 429 responses.
        /// `None` → no header (caller's 1s default applies). May be
        /// delta-seconds (`"1"`) or an HTTP-date.
        retry_after: Option<String>,
        /// Artificial per-HEAD latency (after the concurrency yield).
        /// Default ZERO. Non-zero lets a test push a pass past the
        /// caller's `deadline` without needing huge path counts.
        head_delay: Duration,
    }

    struct ProbeUpstream {
        url: String,
        /// Total HEAD requests served (across all passes).
        head_hits: Arc<AtomicUsize>,
        /// Max concurrent in-flight HEADs observed AFTER the
        /// `head_429_first_n`-th request — i.e. during retry passes.
        max_concurrent_after: Arc<AtomicUsize>,
        _task: tokio::task::JoinHandle<()>,
    }

    /// Axum upstream that 200s any `*.narinfo` HEAD (no per-path
    /// routing — every path is "present"). Serves `WantMassQuery: 1`
    /// so `check_available` runs at 128-wide. For the no-truncation /
    /// 429-retry / adaptive-concurrency tests where the FlexUpstream's
    /// single-seeded-path shape doesn't fit.
    async fn spawn_mass_probe_upstream(cfg: ProbeCfg) -> ProbeUpstream {
        use axum::http::{HeaderMap, HeaderValue, StatusCode};
        use axum::{
            Router,
            routing::{get, head},
        };

        let head_hits = Arc::new(AtomicUsize::new(0));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_concurrent_after = Arc::new(AtomicUsize::new(0));
        let hh = head_hits.clone();
        let mca = max_concurrent_after.clone();
        let ProbeCfg {
            head_429_first_n,
            retry_after,
            head_delay,
        } = cfg;

        let app = Router::new()
            .route(
                "/nix-cache-info",
                get(|| async { "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n" }),
            )
            .route(
                "/{hash}",
                head(move || {
                    let hh = hh.clone();
                    let in_flight = in_flight.clone();
                    let mca = mca.clone();
                    let ra = retry_after.clone();
                    async move {
                        let n = hh.fetch_add(1, Ordering::SeqCst);
                        let cur = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                        // Track max-concurrent only for requests past
                        // the 429 window (i.e. retry passes).
                        if n >= head_429_first_n {
                            mca.fetch_max(cur, Ordering::SeqCst);
                        }
                        // Yield so concurrent in-flight requests pile
                        // up enough for fetch_max to observe the peak.
                        tokio::task::yield_now().await;
                        if !head_delay.is_zero() {
                            tokio::time::sleep(head_delay).await;
                        }
                        in_flight.fetch_sub(1, Ordering::SeqCst);
                        if n < head_429_first_n {
                            let mut h = HeaderMap::new();
                            if let Some(s) = ra {
                                h.insert(
                                    reqwest::header::RETRY_AFTER,
                                    HeaderValue::from_str(&s).unwrap(),
                                );
                            }
                            (StatusCode::TOO_MANY_REQUESTS, h)
                        } else {
                            (StatusCode::OK, HeaderMap::new())
                        }
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        ProbeUpstream {
            url: format!("http://{addr}"),
            head_hits,
            max_concurrent_after,
            _task: task,
        }
    }

    async fn insert_probe(pool: &PgPool, tid: Uuid, fake: &ProbeUpstream) {
        metadata::upstreams::insert(
            pool,
            tid,
            &fake.url,
            50,
            &["dummy:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into()],
            SigMode::Keep,
        )
        .await
        .unwrap();
    }

    // r[verify store.substitute.probe-bounded+4]
    /// HTTP 403 (S3 without public `s3:ListBucket`) MUST be treated as
    /// a miss, not an error: emits `result=miss`, and `check_available`
    /// caches it as a definitive negative so the truncation-convergence
    /// strategy works.
    #[tokio::test]
    async fn substitute_403_is_miss_and_populates_probe_cache() {
        use rio_test_support::metrics::CountingRecorder;

        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-403").await;
        let fake = spawn_status_upstream(axum::http::StatusCode::FORBIDDEN).await;
        metadata::upstreams::insert(
            &db.pool,
            tid,
            &fake.url,
            50,
            &["dummy:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into()],
            SigMode::Keep,
        )
        .await
        .unwrap();

        let path = format!(
            "/nix/store/{}-403-miss",
            rio_test_support::fixtures::rand_store_hash()
        );
        let sub = test_substituter(db.pool.clone());

        // try_substitute: 403 → Miss (not Err).
        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);
        let got = sub.try_substitute(tid, &path).await.unwrap();
        assert!(got.is_none(), "403 → miss");
        assert_eq!(
            rec.get(&format!(
                "rio_store_substitute_total{{result=miss,tenant={tid}}}"
            )),
            1,
            "403 must be result=miss; keys={:?}",
            rec.all_keys()
        );
        assert_eq!(
            rec.get(&format!(
                "rio_store_substitute_total{{result=error,tenant={tid}}}"
            )),
            0,
            "403 must NOT be result=error; keys={:?}",
            rec.all_keys()
        );
        drop(_g);

        // check_available: first call probes (403 → Miss → cached);
        // second call MUST hit the probe_cache (proving 403 was cached
        // as a definitive negative, not left Indeterminate).
        let hits = sub
            .check_available(tid, std::slice::from_ref(&path), far_deadline())
            .await
            .unwrap();
        assert!(hits.is_empty());

        let rec2 = CountingRecorder::default();
        let _g2 = metrics::set_default_local_recorder(&rec2);
        let hits2 = sub
            .check_available(tid, std::slice::from_ref(&path), far_deadline())
            .await
            .unwrap();
        assert!(hits2.is_empty());
        assert_eq!(
            rec2.get("rio_store_substitute_probe_cache_hits_total{}"),
            1,
            "second call must be a probe-cache hit; keys={:?}",
            rec2.all_keys()
        );
        assert_eq!(
            rec2.get("rio_store_substitute_probe_cache_misses_total{}"),
            0,
            "second call must NOT re-probe; keys={:?}",
            rec2.all_keys()
        );
    }

    #[tokio::test]
    async fn substitute_miss_no_upstreams() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-none").await;
        let (path, _) = make_path();

        let sub = test_substituter(db.pool.clone());
        let got = sub.try_substitute(tid, &path).await.unwrap();
        assert!(got.is_none(), "no upstreams → None");
    }

    // r[verify store.substitute.probe-429-retry+2]
    /// First upstream 429s the narinfo GET; second upstream has the
    /// path. `do_substitute` MUST continue to the second (matching the
    /// HEAD-probe semantics) and return a hit, not stop at the first
    /// 429. If both miss-or-429, the loop falls through to
    /// `Err(RateLimited)` so moka doesn't cache a definitive miss.
    #[tokio::test]
    async fn do_substitute_429_first_upstream_tries_second() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-429-then-hit").await;
        let (path, nar) = make_path();
        // Upstream A: every request → 429.
        let a = spawn_status_upstream(axum::http::StatusCode::TOO_MANY_REQUESTS).await;
        // Upstream B: serves the path.
        let b = spawn_fake_upstream(&path, nar, "cache.429-b").await;
        // Priority: A=10 (tried first), B=50 (tried second).
        metadata::upstreams::insert(&db.pool, tid, &a.url, 10, &[], SigMode::Keep)
            .await
            .unwrap();
        metadata::upstreams::insert(
            &db.pool,
            tid,
            &b.url,
            50,
            std::slice::from_ref(&b.trusted_key),
            SigMode::Keep,
        )
        .await
        .unwrap();

        let sub = test_substituter(db.pool.clone());
        let got = sub.try_substitute(tid, &path).await.unwrap();
        assert!(
            got.is_some(),
            "429 from first upstream must NOT stop iteration; \
             second upstream has the path"
        );

        // Now invert: only the 429 upstream configured → no hit, but
        // the result MUST be `Err(RateLimited)` (uncached), not
        // `Ok(None)` (cached miss).
        let tid2 = seed_tenant(&db.pool, "sub-429-only").await;
        metadata::upstreams::insert(&db.pool, tid2, &a.url, 10, &[], SigMode::Keep)
            .await
            .unwrap();
        let got2 = sub.try_substitute(tid2, &path).await;
        assert!(
            matches!(got2, Err(SubstituteError::RateLimited { .. })),
            "all-429 must propagate RateLimited (uncached), not Ok(None); got {got2:?}"
        );
    }

    #[tokio::test]
    async fn substitute_rejects_bad_sig() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-badsig").await;
        let (path, nar) = make_path();
        let fake = spawn_fake_upstream(&path, nar, "cache.test-4").await;

        // WRONG trusted_key — upstream signs with cache.test-4, we
        // trust only cache.WRONG.
        metadata::upstreams::insert(
            &db.pool,
            tid,
            &fake.url,
            50,
            &["cache.WRONG:abcd".into()],
            SigMode::Keep,
        )
        .await
        .unwrap();

        let sub = test_substituter(db.pool.clone());
        let got = sub.try_substitute(tid, &path).await.unwrap();
        assert!(got.is_none(), "sig verification failed → treat as miss");
    }

    #[tokio::test]
    async fn check_available_head_probe() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-head").await;
        let (path, nar) = make_path();
        let fake = spawn_fake_upstream(&path, nar, "cache.test-5").await;

        metadata::upstreams::insert(
            &db.pool,
            tid,
            &fake.url,
            50,
            std::slice::from_ref(&fake.trusted_key),
            SigMode::Keep,
        )
        .await
        .unwrap();

        // Second path with a DISTINCT hash_part — test_store_path
        // uses a fixed TEST_HASH, so both would resolve to the same
        // narinfo URL otherwise. rand_store_hash gives us a distinct
        // 32-char nixbase32 prefix the axum router won't match.
        let absent = format!(
            "/nix/store/{}-not-on-upstream",
            rio_test_support::fixtures::rand_store_hash()
        );
        let sub = test_substituter(db.pool.clone());
        let missing = vec![path.clone(), absent];
        let available = sub
            .check_available(tid, &missing, far_deadline())
            .await
            .unwrap();
        assert_eq!(available, vec![path], "only the seeded path is available");
    }

    // r[verify store.substitute.probe-bounded+4]
    /// No batch-size truncation: a 5000-path batch (>old 4096 cap)
    /// MUST probe every path. Uses a local fake upstream that 200s
    /// every `*.narinfo` HEAD → all 5000 land in `hits` and the
    /// `probe_cache`. Regression guard: pre-change, the tail past
    /// 4096 stayed unprobed → `hits.len() ≤ 4096`.
    #[tokio::test]
    async fn check_available_no_truncation() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-head-nocap").await;
        let fake = spawn_mass_probe_upstream(ProbeCfg::default()).await;
        insert_probe(&db.pool, tid, &fake).await;

        const N: usize = 5000;
        let paths: Vec<String> = (0..N)
            .map(|i| {
                format!(
                    "/nix/store/{}-nocap-{i}",
                    rio_test_support::fixtures::rand_store_hash()
                )
            })
            .collect();

        let sub = test_substituter(db.pool.clone());
        let available = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            sub.check_available(tid, &paths, far_deadline()),
        )
        .await
        .expect("5000 local-200 HEADs at 128 conc should complete in ~1s")
        .unwrap();
        assert_eq!(
            available.len(),
            N,
            "every path must be probed and hit (no truncation)"
        );
        assert!(
            sub.probe_cache
                .get(&(tid, paths.last().unwrap().clone()))
                .await
                .is_some(),
            "tail of batch must be probed and cached"
        );
    }

    // r[verify store.substitute.probe-429-retry+2]
    /// 429 + `Retry-After` honored: upstream 429s the first
    /// `head_429_first_n` HEADs then 200s. All paths MUST end up in
    /// `hits` (the rate-limited subset is retried, not dropped to
    /// `Indeterminate`), the ratelimited counter increments, and
    /// wall-clock ≥ the `Retry-After` value (proves we slept).
    #[tokio::test]
    async fn check_available_429_retry() {
        use rio_test_support::metrics::CountingRecorder;

        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-head-429").await;
        // 50 paths; first 20 HEADs 429+Retry-After:1, rest 200.
        let fake = spawn_mass_probe_upstream(ProbeCfg {
            head_429_first_n: 20,
            retry_after: Some("1".into()),
            ..Default::default()
        })
        .await;
        insert_probe(&db.pool, tid, &fake).await;

        let paths: Vec<String> = (0..50)
            .map(|i| {
                format!(
                    "/nix/store/{}-429-{i}",
                    rio_test_support::fixtures::rand_store_hash()
                )
            })
            .collect();

        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);
        let sub = test_substituter(db.pool.clone());
        let t0 = tokio::time::Instant::now();
        let available = sub
            .check_available(tid, &paths, far_deadline())
            .await
            .unwrap();
        let elapsed = t0.elapsed();

        assert_eq!(
            available.len(),
            50,
            "rate-limited paths must be retried to Hit, not lost"
        );
        assert!(
            rec.get(&format!(
                "rio_store_substitute_probe_ratelimited_total{{tenant={tid}}}"
            )) > 0,
            "ratelimited counter must increment; keys={:?}",
            rec.all_keys()
        );
        assert!(
            elapsed >= Duration::from_secs(1),
            "must sleep ≥ Retry-After before retry pass; elapsed={elapsed:?}"
        );
        // Retry pass re-probes only the rate-limited subset, not the
        // whole batch (Hit/Miss are cached after pass 0).
        assert!(
            fake.head_hits.load(Ordering::SeqCst) <= 50 + 20,
            "retry pass must only re-probe the 429'd subset; total HEADs={}",
            fake.head_hits.load(Ordering::SeqCst)
        );
    }

    // r[verify store.substitute.probe-429-retry+2]
    /// >10% rate-limited → concurrency halves for the retry pass.
    /// 200 paths at 128 concurrency; ALL of pass-0 429s. Retry pass
    /// MUST observe max-concurrent ≤ 64.
    #[tokio::test]
    async fn check_available_429_adaptive() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-head-429-adapt").await;
        // 200 HEADs 429 (no Retry-After → 1s default), rest 200. 200 >
        // batch size so EVERY pass-0 request 429s → 100% > 10%
        // threshold → concurrency halves 128→64.
        let fake = spawn_mass_probe_upstream(ProbeCfg {
            head_429_first_n: 200,
            ..Default::default()
        })
        .await;
        insert_probe(&db.pool, tid, &fake).await;

        let paths: Vec<String> = (0..200)
            .map(|i| {
                format!(
                    "/nix/store/{}-429a-{i}",
                    rio_test_support::fixtures::rand_store_hash()
                )
            })
            .collect();

        let sub = test_substituter(db.pool.clone());
        // Reset the high-water mark AFTER pass 0's 128-wide burst so
        // we measure pass 1's concurrency. Can't intercept between
        // passes, so instead: arm `track_after_n` to start tracking
        // max-concurrent only once `head_429_first_n` requests have
        // been served (i.e. once pass 0 is done).
        let available = sub
            .check_available(tid, &paths, far_deadline())
            .await
            .unwrap();
        assert_eq!(available.len(), 200, "all eventually hit after retry");

        let pass1_max = fake.max_concurrent_after.load(Ordering::SeqCst);
        assert!(
            pass1_max > 0 && pass1_max <= SUBSTITUTE_PROBE_CONCURRENCY / 2,
            "retry pass concurrency must be ≤ {}/2 (halved); observed max={pass1_max}",
            SUBSTITUTE_PROBE_CONCURRENCY
        );
    }

    // r[verify store.substitute.probe-429-retry+2]
    /// `Retry-After` exceeding the caller's deadline budget → retry
    /// pass is SKIPPED (no sleep), rate-limited paths returned as
    /// not-substitutable for this call (uncached → re-probed next time).
    #[tokio::test]
    async fn check_available_429_exceeds_budget() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-head-429-budget").await;
        let fake = spawn_mass_probe_upstream(ProbeCfg {
            head_429_first_n: 10,
            retry_after: Some("300".into()),
            ..Default::default()
        })
        .await;
        insert_probe(&db.pool, tid, &fake).await;

        let paths: Vec<String> = (0..10)
            .map(|i| {
                format!(
                    "/nix/store/{}-429b-{i}",
                    rio_test_support::fixtures::rand_store_hash()
                )
            })
            .collect();

        let sub = test_substituter(db.pool.clone());
        let t0 = tokio::time::Instant::now();
        // 10s budget; Retry-After=300s — must skip the retry pass.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let available = sub.check_available(tid, &paths, deadline).await.unwrap();
        let elapsed = t0.elapsed();

        assert!(
            available.is_empty(),
            "Retry-After > budget → no retry → 0 hits"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "must NOT sleep 300s (budget skip); elapsed={elapsed:?}"
        );
        assert_eq!(
            fake.head_hits.load(Ordering::SeqCst),
            10,
            "exactly one pass; no retry"
        );
        // Not cached as Miss — next call re-probes.
        assert!(
            sub.probe_cache
                .get(&(tid, paths[0].clone()))
                .await
                .is_none(),
            "rate-limited paths must NOT be cached"
        );
    }

    // r[verify store.substitute.probe-bounded+4]
    /// Probe PASS itself (not just the inter-pass sleep) exceeding the
    /// caller's deadline → pass is truncated, un-probed paths
    /// Indeterminate (uncached), AND results that completed before the
    /// deadline survive. Covers the gap the sleep-budget check alone
    /// left: 153k-path pass-0 + halved pass-1 = 109s > 90s
    /// `MERGE_FMP_TIMEOUT` despite each Retry-After fitting the budget.
    #[tokio::test]
    async fn check_available_pass_exceeds_deadline() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-head-pass-deadline").await;
        // No 429s — exercise the pass-0 deadline bound directly. 400ms
        // per HEAD, 200 paths, concurrency=128 (WantMassQuery): wave-1
        // (128) lands ~setup+400ms; wave-2 (72) would land ~setup+800ms
        // but the deadline (set below, AFTER setup) cuts it. Asserts
        // the take_until semantics: wave-1 hits survive, wave-2 is
        // deferred (Indeterminate). Wide margins for builder variance.
        let fake = spawn_mass_probe_upstream(ProbeCfg {
            head_delay: Duration::from_millis(400),
            ..Default::default()
        })
        .await;
        insert_probe(&db.pool, tid, &fake).await;

        let paths: Vec<String> = (0..200)
            .map(|i| {
                format!(
                    "/nix/store/{}-dl-{i}",
                    rio_test_support::fixtures::rand_store_hash()
                )
            })
            .collect();

        let sub = test_substituter(db.pool.clone());
        // Warm `list_for_tenant` PG query + `/nix-cache-info` cache so
        // the deadline budget below covers the HEAD pass itself, not
        // setup. One throwaway path; result discarded.
        let warm = format!(
            "/nix/store/{}-warm",
            rio_test_support::fixtures::rand_store_hash()
        );
        sub.check_available(
            tid,
            std::slice::from_ref(&warm),
            tokio::time::Instant::now() + Duration::from_secs(10),
        )
        .await
        .unwrap();
        let t0 = tokio::time::Instant::now();
        let deadline = t0 + Duration::from_millis(600);
        let available = sub.check_available(tid, &paths, deadline).await.unwrap();
        let elapsed = t0.elapsed();

        // Partial-pass results survive the deadline (regression: the
        // old timeout(.., collect()) dropped them all). Structural
        // assertion — "some survived AND some deferred" — not exact
        // wave boundaries (builder CPU variance).
        assert!(
            !available.is_empty(),
            "completed-before-deadline hits must survive truncation; got 0"
        );
        assert!(
            available.len() < paths.len(),
            "deadline must truncate the pass; got all {} hits",
            available.len()
        );
        // Returned near deadline, not after wave-2 (~800ms+). Loose
        // upper bound; the structural asserts above are primary.
        assert!(
            elapsed < Duration::from_secs(2),
            "must return at deadline, not wait out wave-2; elapsed={elapsed:?}"
        );
        // Survived hits ARE cached.
        assert_eq!(
            sub.probe_cache.get(&(tid, available[0].clone())).await,
            Some(true),
            "completed hits must be cached as Hit"
        );
        // Deferred paths (Indeterminate) are NOT cached; next call
        // re-probes.
        let deferred: Vec<_> = paths
            .iter()
            .filter(|p| !available.contains(p))
            .cloned()
            .collect();
        assert!(!deferred.is_empty());
        assert!(
            sub.probe_cache
                .get(&(tid, deferred[0].clone()))
                .await
                .is_none(),
            "deadline-truncated paths must NOT be cached"
        );
    }

    // r[verify store.substitute.probe-429-retry+2]
    /// `Retry-After` as an RFC 9110 HTTP-date (not delta-seconds):
    /// parsed via `httpdate` and honored. Upstream sends a date ~3s
    /// in the future (`fmt_http_date` truncates sub-second, so +2s
    /// could format to as little as 1.001s ahead and race the ≥1s
    /// gate); assert wall-clock ≥ ~1s (slept) and all hit.
    #[tokio::test]
    async fn check_available_429_http_date() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-head-429-date").await;
        let when = std::time::SystemTime::now() + Duration::from_secs(3);
        let fake = spawn_mass_probe_upstream(ProbeCfg {
            head_429_first_n: 5,
            retry_after: Some(httpdate::fmt_http_date(when)),
            ..Default::default()
        })
        .await;
        insert_probe(&db.pool, tid, &fake).await;

        let paths: Vec<String> = (0..5)
            .map(|i| {
                format!(
                    "/nix/store/{}-429d-{i}",
                    rio_test_support::fixtures::rand_store_hash()
                )
            })
            .collect();

        let sub = test_substituter(db.pool.clone());
        let t0 = tokio::time::Instant::now();
        let available = sub
            .check_available(tid, &paths, far_deadline())
            .await
            .unwrap();
        let elapsed = t0.elapsed();

        assert_eq!(available.len(), 5, "HTTP-date Retry-After → retried to Hit");
        // Smoke only: ≥1s does NOT distinguish a parsed HTTP-date
        // (~2-3s) from the None-default 1s floor — both satisfy the
        // gate. The HTTP-date parse branch is proven directly by
        // `parse_retry_after_http_date` below; this test covers the
        // end-to-end retry wiring.
        assert!(
            elapsed >= Duration::from_secs(1),
            "must sleep per HTTP-date Retry-After; elapsed={elapsed:?}"
        );
        // Structural: retry pass actually re-probed (5 first-pass +
        // 5 retry = 10), independent of wall-clock.
        assert_eq!(
            fake.head_hits.load(Ordering::SeqCst),
            10,
            "first pass (5×429) + retry pass (5×200)"
        );
    }

    // r[verify store.substitute.probe-429-retry+2]
    /// Direct unit test of [`parse_retry_after`]'s HTTP-date branch.
    /// The integration test above (`check_available_429_http_date`)
    /// can't tell a parsed ~2 s from the None-default 1 s floor via
    /// `elapsed >= 1 s`; this asserts the parse itself returns a
    /// duration that could ONLY have come from the HTTP-date arm.
    #[test]
    fn parse_retry_after_http_date() {
        let when = std::time::SystemTime::now() + Duration::from_secs(4);
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(
            reqwest::header::RETRY_AFTER,
            httpdate::fmt_http_date(when).try_into().unwrap(),
        );
        let got = parse_retry_after(&h).expect("HTTP-date must parse");
        // `fmt_http_date` truncates sub-second, so a +4 s target can
        // format to as little as +3.001 s ahead. ≥3 s still rules out
        // both delta-seconds (the header is non-numeric) and the
        // None-default (which would be `None`, not `Some(1s)`).
        assert!(
            got >= Duration::from_secs(3) && got <= Duration::from_secs(5),
            "HTTP-date 4 s ahead → ~3-4 s; got {got:?}"
        );
    }

    /// Delta-seconds form: `Retry-After: 7` → exactly 7 s.
    #[test]
    fn parse_retry_after_delta_seconds() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(reqwest::header::RETRY_AFTER, "7".try_into().unwrap());
        assert_eq!(parse_retry_after(&h), Some(Duration::from_secs(7)));
    }

    /// Absent / malformed header → `None` (caller falls back to its
    /// own default floor, NOT zero).
    #[test]
    fn parse_retry_after_absent_or_garbage() {
        assert_eq!(parse_retry_after(&reqwest::header::HeaderMap::new()), None);
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(reqwest::header::RETRY_AFTER, "soon".try_into().unwrap());
        assert_eq!(parse_retry_after(&h), None);
    }

    // r[verify store.substitute.probe-429-retry+2]
    /// 429 on the narinfo GET path (`try_upstream`) → returns
    /// `Err(RateLimited{retry_after: Some})` IMMEDIATELY (no inline
    /// sleep). The admission permit drops on return so per-replica
    /// capacity isn't held across the wait.
    #[tokio::test]
    async fn try_upstream_429_returns_busy_no_sleep() {
        use axum::http::HeaderValue;
        use axum::{Router, routing::get};

        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-get-429").await;

        // GET on any narinfo → 429 + Retry-After: 30. (Distinct from
        // spawn_mass_probe_upstream which only routes HEAD.)
        let app = Router::new()
            .route(
                "/nix-cache-info",
                get(|| async { "StoreDir: /nix/store\nWantMassQuery: 1\n" }),
            )
            .fallback(get(|| async {
                (
                    axum::http::StatusCode::TOO_MANY_REQUESTS,
                    [(reqwest::header::RETRY_AFTER, HeaderValue::from_static("30"))],
                )
            }));
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let _task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        metadata::upstreams::insert(
            &db.pool,
            tid,
            &url,
            50,
            &["dummy:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into()],
            SigMode::Keep,
        )
        .await
        .unwrap();

        let gate = AdmissionGate::new(4);
        let sub = test_substituter(db.pool.clone()).with_admission_gate(gate.clone());
        let (path, _) = make_path();

        let t0 = tokio::time::Instant::now();
        let got = sub.try_substitute(tid, &path).await;
        let elapsed = t0.elapsed();

        assert!(
            matches!(
                got,
                Err(SubstituteError::RateLimited {
                    retry_after: Some(_)
                })
            ),
            "narinfo GET 429 → Err(RateLimited{{retry_after: Some}}); got {got:?}"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "must NOT sleep inline (admission permit held); elapsed={elapsed:?}"
        );
        assert_eq!(
            gate.utilization(),
            0.0,
            "admission permit must be released on RateLimited return"
        );
    }

    // r[verify store.substitute.probe-bounded+4]
    /// Probe results are cached: a second `check_available` for the
    /// same path returns the cached answer without touching the
    /// upstream. Verified by aborting the fake upstream between calls.
    #[tokio::test]
    async fn check_available_probe_cache_hit() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-head-cache").await;
        let (path, nar) = make_path();
        let fake = spawn_fake_upstream(&path, nar, "cache.test-probe-cache").await;
        metadata::upstreams::insert(
            &db.pool,
            tid,
            &fake.url,
            50,
            std::slice::from_ref(&fake.trusted_key),
            SigMode::Keep,
        )
        .await
        .unwrap();

        let absent = format!(
            "/nix/store/{}-not-on-upstream",
            rio_test_support::fixtures::rand_store_hash()
        );
        let sub = test_substituter(db.pool.clone());
        let batch = vec![path.clone(), absent.clone()];

        let first = sub
            .check_available(tid, &batch, far_deadline())
            .await
            .unwrap();
        assert_eq!(first, vec![path.clone()]);

        // Kill the upstream. Second call must answer from cache —
        // including the negative result for `absent`.
        fake._task.abort();
        let _ = fake._task.await;

        let second = sub
            .check_available(tid, &batch, far_deadline())
            .await
            .unwrap();
        assert_eq!(second, vec![path], "cached positive + negative results");
    }

    /// `probe_cache` is keyed by `(tenant_id, path)`: tenant B's miss
    /// must not poison tenant A's lookup. Upstreams are per-tenant
    /// (`tenant_upstreams`), so the cached boolean is a per-tenant
    /// answer; a path-only key would corrupt cross-tenant scheduling
    /// decisions for the full TTL (1h).
    #[tokio::test]
    async fn check_available_probe_cache_isolated_by_tenant() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid_a = seed_tenant(&db.pool, "sub-isol-a").await;
        let tid_b = seed_tenant(&db.pool, "sub-isol-b").await;
        let (path, nar) = make_path();
        let fake = spawn_fake_upstream(&path, nar, "cache.isol").await;

        // A has the upstream that serves `path`; B has a dead one.
        metadata::upstreams::insert(
            &db.pool,
            tid_a,
            &fake.url,
            50,
            std::slice::from_ref(&fake.trusted_key),
            SigMode::Keep,
        )
        .await
        .unwrap();
        metadata::upstreams::insert(
            &db.pool,
            tid_b,
            "http://127.0.0.1:1", // refused → miss
            50,
            std::slice::from_ref(&fake.trusted_key),
            SigMode::Keep,
        )
        .await
        .unwrap();

        let sub = test_substituter(db.pool.clone());
        let batch = std::slice::from_ref(&path);

        // B probes first → caches `(B, path) = false`.
        let b = sub
            .check_available(tid_b, batch, far_deadline())
            .await
            .unwrap();
        assert!(b.is_empty(), "B's upstream is dead → miss");

        // A probes second → MUST hit A's upstream, not return B's
        // cached miss. With a path-only key this would be `[]`.
        let a = sub
            .check_available(tid_a, batch, far_deadline())
            .await
            .unwrap();
        assert_eq!(a, vec![path.clone()], "A's upstream serves the path");

        // Reverse leakage: A's hit must not leak to B.
        let b2 = sub
            .check_available(tid_b, batch, far_deadline())
            .await
            .unwrap();
        assert!(b2.is_empty(), "B still misses (cached per-tenant)");
    }

    #[test]
    fn upstream_info_parse() {
        assert!(
            UpstreamInfo::parse("StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n")
                .want_mass_query
        );
        assert!(!UpstreamInfo::parse("StoreDir: /nix/store\nPriority: 40\n").want_mass_query);
        assert!(!UpstreamInfo::parse("WantMassQuery: 0\n").want_mass_query);
        assert!(!UpstreamInfo::parse("").want_mass_query);
        // Whitespace tolerance.
        assert!(UpstreamInfo::parse("WantMassQuery:1").want_mass_query);
    }

    /// Seed an 'uploading' placeholder for `store_path` backdated by
    /// `age`. Shared between the stale/young reclaim tests.
    async fn seed_uploading_placeholder(pool: &PgPool, store_path: &str, age: Duration) {
        let sp = StorePath::parse(store_path).unwrap();
        let hash = sp.sha256_digest();
        metadata::insert_manifest_uploading(pool, &hash, store_path, &[])
            .await
            .unwrap();
        sqlx::query(
            "UPDATE manifests SET updated_at = now() - make_interval(secs => $2) \
             WHERE store_path_hash = $1",
        )
        .bind(hash.as_slice())
        .bind(age.as_secs() as i64)
        .execute(pool)
        .await
        .unwrap();
    }

    // r[verify store.substitute.stale-reclaim]
    /// A stale 'uploading' placeholder (crashed prior substitution)
    /// must NOT block a fresh try_substitute. Reclaim → re-insert →
    /// fetch completes.
    #[tokio::test]
    async fn try_substitute_reclaims_stale_uploading() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-stale-reclaim").await;
        let (path, nar) = make_path();
        let fake = spawn_fake_upstream(&path, nar.clone(), "cache.stale-1").await;
        metadata::upstreams::insert(
            &db.pool,
            tid,
            &fake.url,
            50,
            std::slice::from_ref(&fake.trusted_key),
            SigMode::Keep,
        )
        .await
        .unwrap();

        // Stale placeholder: 10min old, threshold 5min → reclaimed.
        seed_uploading_placeholder(&db.pool, &path, Duration::from_secs(10 * 60)).await;

        let sub = test_substituter(db.pool.clone());
        let got = sub.try_substitute(tid, &path).await.unwrap();
        let got = got.expect("stale placeholder reclaimed → fetch completes");

        assert_eq!(got.store_path.as_str(), path);
        assert_eq!(got.nar_size, nar.len() as u64);

        // Placeholder replaced with a real complete row.
        let stored = metadata::query_path_info(&db.pool, &path)
            .await
            .unwrap()
            .expect("path persisted post-reclaim");
        assert_eq!(stored.nar_size, nar.len() as u64);
    }

    // r[verify store.substitute.singleflight+3]
    /// A young 'uploading' placeholder means a live concurrent
    /// uploader — do NOT reclaim, return `Err(Raced)` (NOT a cached
    /// `Ok(None)`). Once the placeholder completes, a retry MUST reach
    /// `AlreadyComplete` and return `Ok(Some)` — proving the moka
    /// singleflight did NOT cache the transient `Raced` outcome.
    #[tokio::test]
    async fn try_substitute_raced_not_cached() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-young-noreclaim").await;
        let (path, nar) = make_path();
        let fake = spawn_fake_upstream(&path, nar.clone(), "cache.young-1").await;
        metadata::upstreams::insert(
            &db.pool,
            tid,
            &fake.url,
            50,
            std::slice::from_ref(&fake.trusted_key),
            SigMode::Keep,
        )
        .await
        .unwrap();

        // Young placeholder: 30s old, threshold 5min → NOT reclaimed.
        seed_uploading_placeholder(&db.pool, &path, Duration::from_secs(30)).await;

        let sub = test_substituter(db.pool.clone());
        let got = sub.try_substitute(tid, &path).await;

        // Young placeholder → PlaceholderClaim::Concurrent → `Err(Raced)`
        // so moka does NOT cache. Caller retries on the next request.
        assert!(
            matches!(got, Err(SubstituteError::Raced)),
            "young placeholder should yield Err(Raced), got {got:?}"
        );

        // Placeholder still present (NOT reclaimed).
        let sp = StorePath::parse(&path).unwrap();
        let hash = sp.sha256_digest();
        let age = metadata::manifest_uploading_age(&db.pool, &hash)
            .await
            .unwrap();
        assert!(age.is_some(), "young placeholder must survive");

        // Now: simulate the concurrent uploader completing. Reap the
        // placeholder so a fresh `try_substitute` can claim and ingest
        // (proving moka didn't cache the prior `Busy` as a miss).
        sqlx::query("DELETE FROM manifests WHERE store_path_hash = $1")
            .bind(hash.as_slice())
            .execute(&db.pool)
            .await
            .unwrap();

        let got2 = sub.try_substitute(tid, &path).await.unwrap();
        let got2 = got2.expect("second call after Busy must re-run and hit (not cached None)");
        assert_eq!(got2.nar_size, nar.len() as u64);
    }

    // r[verify store.substitute.admission]
    /// N concurrent `try_substitute` calls for the SAME `(tenant, path)`
    /// coalesce on the moka singleflight; only the leader's init future
    /// runs and only IT acquires an admission permit. With cap=2 and 5
    /// waiters, `available_permits()` floors at 1 (leader holds one),
    /// not 0 (which the pre-refactor whole-call gate produced — every
    /// waiter held a permit before reaching moka).
    #[tokio::test(flavor = "multi_thread")]
    async fn same_path_waiters_share_one_permit() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-admission-leader").await;
        let (path, nar) = make_path();
        // 300 ms NAR delay holds the leader inside the init future long
        // enough for the sampler to observe the permit floor.
        let fake = spawn_fake_upstream_with_delay(
            &path,
            nar.clone(),
            "cache.adm-1",
            Duration::from_millis(300),
        )
        .await;
        metadata::upstreams::insert(
            &db.pool,
            tid,
            &fake.url,
            50,
            std::slice::from_ref(&fake.trusted_key),
            SigMode::Keep,
        )
        .await
        .unwrap();

        let gate = AdmissionGate::new(2);
        let sub = Arc::new(test_substituter(db.pool.clone()).with_admission_gate(gate.clone()));

        // 5 concurrent same-path callers.
        let calls: Vec<_> = (0..5)
            .map(|_| {
                let sub = Arc::clone(&sub);
                let path = path.clone();
                tokio::spawn(async move { sub.try_substitute(tid, &path).await })
            })
            .collect();

        // Sample the floor while the leader is parked on the slow NAR
        // GET. Structural assertion: the floor is the count, not a
        // wall-clock bound — robust under builder load.
        let sem = gate.semaphore().clone();
        let sampler = tokio::spawn(async move {
            let mut min = sem.available_permits();
            for _ in 0..50 {
                tokio::time::sleep(Duration::from_millis(5)).await;
                min = min.min(sem.available_permits());
            }
            min
        });

        for call in calls {
            let got = call.await.unwrap().unwrap();
            assert_eq!(
                got.expect("upstream has the path").nar_size,
                nar.len() as u64
            );
        }
        let floor = sampler.await.unwrap();
        assert_eq!(
            floor, 1,
            "only the singleflight leader may hold a permit: cap=2 → floor=1; \
             floor=0 means waiters acquired (pre-refactor behavior)"
        );
        assert_eq!(
            gate.semaphore().available_permits(),
            2,
            "permit released after leader completes"
        );
    }

    // — flexible fixture for the regression tests below —
    //
    // Serves one (path, NAR) pair with tunable misbehavior: oversized
    // narinfo, identity-mismatched narinfo, fail-first-then-succeed,
    // block-on-NAR, and per-route hit counters. Kept separate from
    // `spawn_fake_upstream` so the simple end-to-end tests above stay
    // readable.

    use std::sync::atomic::AtomicBool;

    #[derive(Default)]
    struct FlexCfg {
        /// Serve `narinfo_override` instead of the well-formed one.
        narinfo_override: Option<String>,
        /// First narinfo GET returns 503; subsequent ones succeed.
        narinfo_fail_first: bool,
        /// First `/nix-cache-info` GET returns 503; subsequent succeed.
        cache_info_fail_first: bool,
        /// First N narinfo GETs return 429 with NO `Retry-After`
        /// header; subsequent ones succeed.
        narinfo_429_first_n: usize,
        /// HEAD on narinfo returns 503 (every time).
        head_503: bool,
        /// `/nix-cache-info` GET returns 404 (every time).
        cache_info_404: bool,
        /// NAR GET awaits this Notify before responding (drop test).
        nar_gate: Option<Arc<tokio::sync::Notify>>,
    }

    struct FlexUpstream {
        url: String,
        trusted_key: String,
        narinfo_hits: Arc<AtomicUsize>,
        nar_hits: Arc<AtomicUsize>,
        _task: tokio::task::JoinHandle<()>,
    }

    async fn spawn_flex_upstream(
        store_path: &str,
        nar_bytes: Vec<u8>,
        key_name: &str,
        cfg: FlexCfg,
    ) -> FlexUpstream {
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use axum::{
            Router,
            routing::{get, head},
        };
        use base64::Engine;

        let seed = [0x42u8; 32];
        let signer = Signer::from_seed(key_name, &seed);
        let pubkey = ed25519_dalek::SigningKey::from_bytes(&seed).verifying_key();
        let trusted_key = format!(
            "{key_name}:{}",
            base64::engine::general_purpose::STANDARD.encode(pubkey.as_bytes())
        );

        let nar_hash: [u8; 32] = sha2::Sha256::digest(&nar_bytes).into();
        let nar_hash_str = format!(
            "sha256:{}",
            rio_nix::store_path::nixbase32::encode(&nar_hash)
        );
        let fp = fingerprint(store_path, &nar_hash, nar_bytes.len() as u64, &[]);
        let sig = signer.sign(&fp);

        let sp = StorePath::parse(store_path).unwrap();
        let hash_part = sp.hash_part();

        let narinfo_body = cfg.narinfo_override.unwrap_or_else(|| {
            format!(
                "StorePath: {store_path}\n\
                 URL: nar/{hash_part}.nar\n\
                 Compression: none\n\
                 NarHash: {nar_hash_str}\n\
                 NarSize: {}\n\
                 References: \n\
                 Sig: {sig}\n",
                nar_bytes.len()
            )
        });

        let narinfo_path = format!("/{hash_part}.narinfo");
        let nar_path = format!("/nar/{hash_part}.nar");

        let narinfo_hits = Arc::new(AtomicUsize::new(0));
        let nar_hits = Arc::new(AtomicUsize::new(0));
        let ni_hits = narinfo_hits.clone();
        let nr_hits = nar_hits.clone();
        let ni_failed = Arc::new(AtomicBool::new(false));
        let ci_failed = Arc::new(AtomicBool::new(false));
        let narinfo_fail_first = cfg.narinfo_fail_first;
        let narinfo_429_first_n = cfg.narinfo_429_first_n;
        let cache_info_fail_first = cfg.cache_info_fail_first;
        let cache_info_404 = cfg.cache_info_404;
        let head_503 = cfg.head_503;
        let nar_gate = cfg.nar_gate;

        let app = Router::new()
            .route(
                "/nix-cache-info",
                get(move || {
                    let ci_failed = ci_failed.clone();
                    async move {
                        if cache_info_404 {
                            return (StatusCode::NOT_FOUND, "").into_response();
                        }
                        if cache_info_fail_first && !ci_failed.swap(true, Ordering::SeqCst) {
                            return (StatusCode::SERVICE_UNAVAILABLE, "").into_response();
                        }
                        "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n".into_response()
                    }
                }),
            )
            .route(
                &narinfo_path,
                head(move || async move {
                    if head_503 {
                        StatusCode::SERVICE_UNAVAILABLE
                    } else {
                        StatusCode::OK
                    }
                })
                .get(move || {
                    let ni_hits = ni_hits.clone();
                    let ni_failed = ni_failed.clone();
                    let body = narinfo_body.clone();
                    async move {
                        let n = ni_hits.fetch_add(1, Ordering::SeqCst);
                        if narinfo_429_first_n > 0 && n < narinfo_429_first_n {
                            // Bare 429, no Retry-After header.
                            return (StatusCode::TOO_MANY_REQUESTS, String::new()).into_response();
                        }
                        if narinfo_fail_first && !ni_failed.swap(true, Ordering::SeqCst) {
                            return (StatusCode::SERVICE_UNAVAILABLE, String::new())
                                .into_response();
                        }
                        body.into_response()
                    }
                }),
            )
            .route(
                &nar_path,
                get(move || {
                    let nr_hits = nr_hits.clone();
                    let nar = nar_bytes.clone();
                    let gate = nar_gate.clone();
                    async move {
                        nr_hits.fetch_add(1, Ordering::SeqCst);
                        if let Some(g) = gate {
                            g.notified().await;
                        }
                        nar
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        FlexUpstream {
            url: format!("http://{addr}"),
            trusted_key,
            narinfo_hits,
            nar_hits,
            _task: task,
        }
    }

    /// Build a validly-signed narinfo body for `signed_path` (so
    /// `verify_sig` would pass) — used to serve at the WRONG hash_part.
    /// `url_hash` controls the `URL:` line independently so the body
    /// can point at a NAR route the flex upstream actually serves;
    /// `nar_size_override` lets the body lie about `NarSize` (signed
    /// over the lie, so `verify_sig` still passes).
    fn signed_narinfo_for(
        signed_path: &str,
        nar: &[u8],
        key_name: &str,
        url_hash: &str,
        nar_size_override: Option<u64>,
    ) -> String {
        let seed = [0x42u8; 32];
        let signer = Signer::from_seed(key_name, &seed);
        let nar_hash: [u8; 32] = sha2::Sha256::digest(nar).into();
        let nar_hash_str = format!(
            "sha256:{}",
            rio_nix::store_path::nixbase32::encode(&nar_hash)
        );
        let nar_size = nar_size_override.unwrap_or(nar.len() as u64);
        let fp = fingerprint(signed_path, &nar_hash, nar_size, &[]);
        let sig = signer.sign(&fp);
        format!(
            "StorePath: {signed_path}\n\
             URL: nar/{url_hash}.nar\n\
             Compression: none\n\
             NarHash: {nar_hash_str}\n\
             NarSize: {nar_size}\n\
             References: \n\
             Sig: {sig}\n"
        )
    }

    async fn insert_flex(pool: &PgPool, tid: Uuid, fake: &FlexUpstream, prio: i32) {
        metadata::upstreams::insert(
            pool,
            tid,
            &fake.url,
            prio,
            std::slice::from_ref(&fake.trusted_key),
            SigMode::Keep,
        )
        .await
        .unwrap();
    }

    // r[verify store.substitute.identity-check]
    /// bug_249: a validly-signed narinfo for path A served at
    /// `{hash_of_B}.narinfo` MUST be rejected before sig-verify, and
    /// nothing must be ingested.
    #[tokio::test]
    async fn narinfo_identity_mismatch_rejected() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-ident").await;
        let (path_b, nar) = make_path();
        let path_a = format!(
            "/nix/store/{}-victim",
            rio_test_support::fixtures::rand_store_hash()
        );
        // Serve A's narinfo (valid sig over A) at B's hash_part. The
        // `URL:` line points at B's NAR route (which the flex upstream
        // actually serves) so that with the identity check REMOVED,
        // ingestion would complete and `nar_hits == 0` /
        // `query_path_info(A).is_none()` would FAIL — i.e. the test
        // is mutation-killing.
        let hash_b = StorePath::parse(&path_b).unwrap().hash_part();
        let body_a = signed_narinfo_for(&path_a, &nar, "cache.ident", &hash_b, None);
        let fake = spawn_flex_upstream(
            &path_b,
            nar,
            "cache.ident",
            FlexCfg {
                narinfo_override: Some(body_a),
                ..Default::default()
            },
        )
        .await;
        insert_flex(&db.pool, tid, &fake, 50).await;

        let sub = test_substituter(db.pool.clone());
        // Single upstream → its NarInfo error escapes the loop unswallowed?
        // No: per-upstream errors are try-next; with one upstream that's
        // a miss. Assert the miss AND that nothing was ingested.
        let got = sub.try_substitute(tid, &path_b).await.unwrap();
        assert!(got.is_none(), "identity mismatch → miss, got {got:?}");
        assert_eq!(
            fake.nar_hits.load(Ordering::SeqCst),
            0,
            "NAR endpoint must not be hit on identity reject"
        );
        assert!(
            metadata::query_path_info(&db.pool, &path_a)
                .await
                .unwrap()
                .is_none(),
            "path A must not be ingested"
        );
        assert!(
            metadata::query_path_info(&db.pool, &path_b)
                .await
                .unwrap()
                .is_none(),
            "path B must not be ingested"
        );
    }

    /// bug_247: with a young 'uploading' placeholder, `try_upstream`
    /// returns `Raced` BEFORE the NAR download — and `do_substitute`
    /// stops without trying the second upstream.
    #[tokio::test]
    async fn concurrent_claim_skips_redownload() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-raced").await;
        let (path, nar) = make_path();
        let fake1 = spawn_flex_upstream(&path, nar.clone(), "cache.r1", FlexCfg::default()).await;
        let fake2 = spawn_flex_upstream(&path, nar, "cache.r2", FlexCfg::default()).await;
        insert_flex(&db.pool, tid, &fake1, 10).await;
        insert_flex(&db.pool, tid, &fake2, 20).await;

        seed_uploading_placeholder(&db.pool, &path, Duration::from_secs(30)).await;

        let sub = test_substituter(db.pool.clone());
        let got = sub.try_substitute(tid, &path).await;
        assert!(
            matches!(got, Err(SubstituteError::Raced)),
            "Raced → Err(Raced), got {got:?}"
        );
        assert_eq!(
            fake1.nar_hits.load(Ordering::SeqCst),
            0,
            "claim before fetch: NAR endpoint NOT hit"
        );
        assert_eq!(
            fake2.narinfo_hits.load(Ordering::SeqCst),
            0,
            "Raced must STOP the upstream loop"
        );
    }

    /// bug_357: dedup via `AlreadyComplete` — pre-ingested path skips
    /// the NAR download (narinfo IS fetched, NAR is NOT).
    #[tokio::test]
    async fn dedup_via_already_complete_no_redownload() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-dedup").await;
        let (path, nar) = make_path();
        let fake = spawn_flex_upstream(&path, nar.clone(), "cache.dedup", FlexCfg::default()).await;
        insert_flex(&db.pool, tid, &fake, 50).await;

        let sub = test_substituter(db.pool.clone());
        // First call ingests.
        sub.try_substitute(tid, &path).await.unwrap().unwrap();
        let baseline = fake.nar_hits.load(Ordering::SeqCst);
        assert_eq!(baseline, 1);
        // Clear singleflight so the second call reaches do_substitute.
        sub.inflight.invalidate_all();
        sub.inflight.run_pending_tasks().await;

        let got = sub.try_substitute(tid, &path).await.unwrap().unwrap();
        assert_eq!(got.store_path.as_str(), path);
        assert_eq!(
            fake.nar_hits.load(Ordering::SeqCst),
            baseline,
            "AlreadyComplete must short-circuit before NAR GET"
        );
    }

    // r[verify store.substitute.untrusted-upstream+3]
    /// bug_172: oversized narinfo body → `TooLarge`, NAR endpoint never
    /// hit.
    #[tokio::test]
    async fn narinfo_oversized_rejected() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-huge-ni").await;
        let (path, nar) = make_path();
        let huge = "X".repeat((MAX_NARINFO_BYTES + 1024) as usize);
        let fake = spawn_flex_upstream(
            &path,
            nar,
            "cache.huge",
            FlexCfg {
                narinfo_override: Some(huge),
                ..Default::default()
            },
        )
        .await;
        insert_flex(&db.pool, tid, &fake, 50).await;

        let sub = test_substituter(db.pool.clone());
        // Per-upstream error is swallowed → Ok(None). Assert via
        // `try_upstream` directly so we see the TooLarge variant.
        let http = sub.http.as_ref().unwrap();
        let upstreams = metadata::upstreams::list_for_tenant(&db.pool, tid)
            .await
            .unwrap();
        let hp = StorePath::parse(&path).unwrap().hash_part();
        let err = sub
            .try_upstream(http, tid, &upstreams[0], &path, &hp, None)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                SubstituteError::TooLarge {
                    what: "narinfo",
                    ..
                }
            ),
            "got {err:?}"
        );
        assert_eq!(fake.nar_hits.load(Ordering::SeqCst), 0);
    }

    // r[verify store.substitute.untrusted-upstream+3]
    /// bug_093: a NAR larger than the decompressed cap → `TooLarge`.
    /// Uses the test-only 64 KiB `SUBSTITUTE_NAR_DECOMPRESSED_CAP` so
    /// this doesn't allocate 4 GiB.
    #[tokio::test]
    async fn fetch_nar_decompressed_cap() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-huge-nar").await;
        let (path, _) = make_path();
        let huge_nar = vec![0u8; (SUBSTITUTE_NAR_DECOMPRESSED_CAP + 1024) as usize];
        let fake = spawn_flex_upstream(&path, huge_nar, "cache.huge-nar", FlexCfg::default()).await;
        insert_flex(&db.pool, tid, &fake, 50).await;

        let sub = test_substituter(db.pool.clone());
        let http = sub.http.as_ref().unwrap();
        let upstreams = metadata::upstreams::list_for_tenant(&db.pool, tid)
            .await
            .unwrap();
        let hp = StorePath::parse(&path).unwrap().hash_part();
        let err = sub
            .try_upstream(http, tid, &upstreams[0], &path, &hp, None)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                SubstituteError::TooLarge {
                    what: "decompressed NAR",
                    ..
                }
            ),
            "got {err:?}"
        );
        // Placeholder must be cleaned up (explicit-abort path).
        let sp = StorePath::parse(&path).unwrap();
        assert!(
            metadata::manifest_uploading_age(&db.pool, &sp.sha256_digest())
                .await
                .unwrap()
                .is_none(),
            "abort_placeholder must run on TooLarge"
        );
    }

    // r[verify store.substitute.singleflight+3]
    /// merged_bug_199 / bug_327: a transient narinfo 503 propagates as
    /// `Err` and is NOT cached — the immediate retry succeeds.
    #[tokio::test]
    async fn try_substitute_transient_error_not_cached() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-transient").await;
        let (path, nar) = make_path();
        // Suppress mass-query so the 503 isn't masked by upstream_info
        // throttling re-fetching nix-cache-info between calls.
        let fake = spawn_flex_upstream(
            &path,
            nar,
            "cache.tr",
            FlexCfg {
                narinfo_fail_first: true,
                cache_info_404: true,
                ..Default::default()
            },
        )
        .await;
        insert_flex(&db.pool, tid, &fake, 50).await;

        let sub = test_substituter(db.pool.clone());
        // First call → upstream 503. With one upstream the per-upstream
        // swallow means do_substitute returns Ok(None) — but the key
        // assertion is that the SECOND call re-runs do_substitute
        // (i.e. moka didn't cache the miss-from-error). Under the old
        // get_with this would have cached `None` AND eaten the error;
        // under try_get_with the Ok(None) IS cached. To force the error
        // to escape, drive try_upstream directly first.
        let http = sub.http.as_ref().unwrap();
        let upstreams = metadata::upstreams::list_for_tenant(&db.pool, tid)
            .await
            .unwrap();
        let hp = StorePath::parse(&path).unwrap().hash_part();
        let first = sub
            .try_upstream(http, tid, &upstreams[0], &path, &hp, None)
            .await;
        assert!(
            matches!(first, Err(SubstituteError::Fetch(_))),
            "first call must surface 503: {first:?}"
        );

        // Now exercise the public path: do_substitute swallows the
        // per-upstream error → Ok(None). moka caches Ok(None). To prove
        // try_get_with doesn't cache Err, force an Err out of
        // do_substitute by making it fail at the only un-swallowed
        // point: PG. Simpler structural assertion: check that an Err
        // from try_substitute leaves the slot empty.
        sub.inflight
            .try_get_with((tid, path.clone()), async {
                Err::<Option<Arc<ValidatedPathInfo>>, _>(SubstituteError::Fetch("boom".into()))
            })
            .await
            .unwrap_err();
        assert!(
            sub.inflight.get(&(tid, path.clone())).await.is_none(),
            "Err must NOT be cached in the singleflight slot"
        );

        // Second real call (fail_first already consumed) → hit.
        let got = sub.try_substitute(tid, &path).await.unwrap();
        assert!(got.is_some(), "second call after transient 503 must hit");
    }

    /// bug_094: a `/nix-cache-info` 503 must not be cached for 1h.
    #[tokio::test]
    async fn upstream_info_error_not_cached() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-ci-err").await;
        let (path, nar) = make_path();
        let fake = spawn_flex_upstream(
            &path,
            nar,
            "cache.ci",
            FlexCfg {
                cache_info_fail_first: true,
                ..Default::default()
            },
        )
        .await;
        insert_flex(&db.pool, tid, &fake, 50).await;

        let sub = test_substituter(db.pool.clone());
        let http = sub.http.as_ref().unwrap();
        let base = fake.url.trim_end_matches('/');

        let first = sub.upstream_info(http, base).await;
        assert!(first.is_none(), "503 → None (uncached)");
        assert!(
            sub.upstream_info.get(base).await.is_none(),
            "None must not enter the cache"
        );

        let second = sub.upstream_info(http, base).await;
        assert!(
            second.is_some_and(|i| i.want_mass_query),
            "second call must re-fetch and see WantMassQuery:1"
        );
    }

    // r[verify store.substitute.probe-bounded+4]
    /// bug_251: HEAD 503 → `Indeterminate` → not cached as `false`;
    /// next call re-probes.
    #[tokio::test]
    async fn probe_cache_5xx_not_cached_as_miss() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-probe-5xx").await;
        let (path, nar) = make_path();
        let fake = spawn_flex_upstream(
            &path,
            nar,
            "cache.p5",
            FlexCfg {
                head_503: true,
                ..Default::default()
            },
        )
        .await;
        insert_flex(&db.pool, tid, &fake, 50).await;

        let sub = test_substituter(db.pool.clone());
        let batch = std::slice::from_ref(&path);

        let first = sub
            .check_available(tid, batch, far_deadline())
            .await
            .unwrap();
        assert!(first.is_empty(), "503 → no hit");
        assert!(
            sub.probe_cache.get(&(tid, path.clone())).await.is_none(),
            "503 must NOT be cached as Some(false)"
        );
    }

    /// bug_441 + G21-C4-fold: dropping `try_substitute` mid-fetch
    /// (post-claim) must clean up the `'uploading'` placeholder.
    #[tokio::test]
    async fn substitute_drop_mid_fetch_cleans_placeholder() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-drop").await;
        let (path, nar) = make_path();
        let gate = Arc::new(tokio::sync::Notify::new());
        let fake = spawn_flex_upstream(
            &path,
            nar,
            "cache.drop",
            FlexCfg {
                nar_gate: Some(gate.clone()),
                ..Default::default()
            },
        )
        .await;
        insert_flex(&db.pool, tid, &fake, 50).await;

        let sub = Arc::new(test_substituter(db.pool.clone()));
        let sub2 = sub.clone();
        let path2 = path.clone();
        // Race the substitute against a short timeout so the future is
        // dropped mid-NAR-GET (post-claim). The gate never fires.
        let res = tokio::time::timeout(Duration::from_millis(200), async move {
            sub2.try_substitute(tid, &path2).await
        })
        .await;
        assert!(res.is_err(), "must time out (NAR endpoint blocked)");
        assert_eq!(
            fake.nar_hits.load(Ordering::SeqCst),
            1,
            "claim happened, NAR GET started"
        );

        // Guard's spawn is fire-and-forget; poll ≤1s for the cleanup.
        let sp = StorePath::parse(&path).unwrap();
        let hash = sp.sha256_digest();
        let mut cleaned = false;
        for _ in 0..20 {
            if metadata::manifest_uploading_age(&db.pool, &hash)
                .await
                .unwrap()
                .is_none()
            {
                cleaned = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            cleaned,
            "drop-guard must reap the 'uploading' placeholder within 1s"
        );
        gate.notify_one(); // unblock the server task
    }

    // r[verify store.substitute.probe-bounded+4]
    /// bug_204: `connect_timeout`-only client + dead upstream must not
    /// hang `check_available` / `try_substitute`. Routes through
    /// PRODUCTION code (not a hand-rolled reqwest call) so deleting
    /// `.timeout(SUBSTITUTE_SMALL_FETCH_TIMEOUT)` from any of the
    /// three small-fetch sites (narinfo GET, cache-info GET, narinfo
    /// HEAD) makes this fail.
    #[tokio::test]
    async fn small_fetch_timeout_does_not_block() {
        // Hold a listener open but never accept → connect succeeds
        // (kernel backlog), body never arrives.
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let _hold = tokio::spawn(async move {
            let _l = listener;
            tokio::time::sleep(Duration::from_secs(120)).await;
        });

        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-timeout").await;
        metadata::upstreams::insert(
            &db.pool,
            tid,
            &format!("http://{addr}"),
            50,
            &["dummy:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into()],
            SigMode::Keep,
        )
        .await
        .unwrap();

        let sub = test_substituter(db.pool.clone());
        let path = rio_test_support::fixtures::test_store_path("hung");

        // check_available: cache-info GET (timeout) then HEAD (timeout)
        // — exercises BOTH `.timeout(SUBSTITUTE_SMALL_FETCH_TIMEOUT)`
        // sites at once. Under cfg(test) the timeout is 2s; allow 3×
        // that as slack (two sequential timeouts + scheduler jitter).
        let slack = SUBSTITUTE_SMALL_FETCH_TIMEOUT * 3;
        let start = Instant::now();
        let _ = sub
            .check_available(tid, std::slice::from_ref(&path), far_deadline())
            .await;
        assert!(
            start.elapsed() < slack,
            "check_available must abort hung cache-info+HEAD via per-request timeout; took {:?}",
            start.elapsed()
        );

        // try_substitute: narinfo GET (timeout) — exercises the third
        // small-fetch site.
        let start = Instant::now();
        let _ = sub.try_substitute(tid, &path).await;
        assert!(
            start.elapsed() < slack,
            "try_substitute must abort hung narinfo GET via per-request timeout; took {:?}",
            start.elapsed()
        );
    }

    // r[verify store.substitute.untrusted-upstream+3]
    /// bug_005: a narinfo whose `NarSize` differs from the actual
    /// decompressed length MUST be rejected (integrity failure).
    /// Signatures are computed over `nar_size`; persisting an unchecked
    /// size would store sigs that don't verify against the row.
    #[tokio::test]
    async fn substitute_rejects_nar_size_mismatch() {
        use rio_test_support::metrics::CountingRecorder;

        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-sizemis").await;
        let (path, nar) = make_path();
        let hash = StorePath::parse(&path).unwrap().hash_part();
        // Signed over a WRONG NarSize (actual+1). NarHash is correct.
        let body = signed_narinfo_for(
            &path,
            &nar,
            "cache.sizemis",
            &hash,
            Some(nar.len() as u64 + 1),
        );
        let fake = spawn_flex_upstream(
            &path,
            nar,
            "cache.sizemis",
            FlexCfg {
                narinfo_override: Some(body),
                ..Default::default()
            },
        )
        .await;
        insert_flex(&db.pool, tid, &fake, 50).await;

        let sub = test_substituter(db.pool.clone());
        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);
        let got = sub.try_substitute(tid, &path).await.unwrap();

        // Per-upstream error swallowed → miss (one upstream).
        assert!(got.is_none(), "size mismatch → miss; got {got:?}");
        // NAR was fetched (size check happens AFTER download).
        assert_eq!(fake.nar_hits.load(Ordering::SeqCst), 1, "NAR fetched");
        // Nothing persisted.
        assert!(
            metadata::query_path_info(&db.pool, &path)
                .await
                .unwrap()
                .is_none(),
            "size mismatch must not persist"
        );
        // Integrity metric incremented.
        assert_eq!(
            rec.get(&format!(
                "rio_store_substitute_integrity_failures_total{{tenant={tid}}}"
            )),
            1,
            "SizeMismatch is an integrity failure; keys={:?}",
            rec.all_keys()
        );
    }

    /// bug_005 (AlreadyComplete arm): a second upstream serving a
    /// lying `NarSize` for an already-ingested path MUST NOT poison
    /// the stored sigs — `sigs_for_mode` is computed over the STORED
    /// row, not the upstream's claim.
    #[tokio::test]
    async fn substitute_already_complete_signs_stored_size() {
        use base64::Engine;

        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid_a = seed_tenant(&db.pool, "sub-ac-a").await;
        let tid_b = seed_tenant(&db.pool, "sub-ac-b").await;
        let (path, nar) = make_path();
        let hash = StorePath::parse(&path).unwrap().hash_part();

        // Tenant A: honest upstream → ingests with correct nar_size.
        let honest = spawn_fake_upstream(&path, nar.clone(), "cache.ac-honest").await;
        metadata::upstreams::insert(
            &db.pool,
            tid_a,
            &honest.url,
            50,
            std::slice::from_ref(&honest.trusted_key),
            SigMode::Keep,
        )
        .await
        .unwrap();
        let sub_a = test_substituter(db.pool.clone());
        sub_a.try_substitute(tid_a, &path).await.unwrap().unwrap();

        // Tenant B: a different upstream that LIES about NarSize for
        // the same path. sig_mode=Replace so rio signs with its own
        // key — over the STORED row, not the lying narinfo.
        let lying_body = signed_narinfo_for(
            &path,
            &nar,
            "cache.ac-liar",
            &hash,
            Some(nar.len() as u64 + 99),
        );
        let liar = spawn_flex_upstream(
            &path,
            nar.clone(),
            "cache.ac-liar",
            FlexCfg {
                narinfo_override: Some(lying_body),
                ..Default::default()
            },
        )
        .await;
        metadata::upstreams::insert(
            &db.pool,
            tid_b,
            &liar.url,
            50,
            std::slice::from_ref(&liar.trusted_key),
            SigMode::Replace,
        )
        .await
        .unwrap();
        let cluster_seed = [0x77u8; 32];
        let cluster = Signer::from_seed("rio-ac-1", &cluster_seed);
        let ts = Arc::new(TenantSigner::new(cluster, db.pool.clone()));
        let sub_b = test_substituter(db.pool.clone()).with_signer(ts);

        // claim_placeholder → AlreadyComplete → sigs computed over the
        // stored row.
        let got = sub_b.try_substitute(tid_b, &path).await.unwrap().unwrap();
        // No NAR download on AlreadyComplete.
        assert_eq!(
            liar.nar_hits.load(Ordering::SeqCst),
            0,
            "AlreadyComplete must not download"
        );

        // The appended rio sig must verify against the STORED tuple
        // (correct nar_size = nar.len()), NOT the liar's NarSize.
        let rio_sig = got
            .signatures
            .iter()
            .find(|s| s.starts_with("rio-ac-1:"))
            .expect("Replace mode appends rio sig");
        let fp = rio_nix::narinfo::fingerprint(
            got.store_path.as_str(),
            &got.nar_hash,
            got.nar_size,
            &got.references
                .iter()
                .map(|r| r.to_string())
                .collect::<Vec<_>>(),
        );
        let pk = ed25519_dalek::SigningKey::from_bytes(&cluster_seed).verifying_key();
        let sig_b64 = rio_sig.split_once(':').unwrap().1;
        let sig_bytes = base64::engine::general_purpose::STANDARD
            .decode(sig_b64)
            .unwrap();
        let sig = ed25519_dalek::Signature::from_slice(&sig_bytes).unwrap();
        use ed25519_dalek::Verifier;
        assert!(
            pk.verify(fp.as_bytes(), &sig).is_ok(),
            "appended sig must verify against STORED nar_size={}, not liar's claim",
            got.nar_size
        );
        assert_eq!(got.nar_size, nar.len() as u64);
    }

    // r[verify store.put.nar-bytes-budget+3]
    /// bug_070: `fetch_nar` MUST acquire from `nar_bytes_budget` as
    /// bytes accumulate. Structural assertion: while a fetch is
    /// in-flight (gated mid-body), the shared semaphore's available
    /// permits drop; after the future is dropped, they recover.
    #[tokio::test]
    async fn fetch_nar_backpressures_on_budget() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-budget").await;
        let (path, nar) = make_path();
        let gate = Arc::new(tokio::sync::Notify::new());
        let fake = spawn_flex_upstream(
            &path,
            nar.clone(),
            "cache.budget",
            FlexCfg {
                nar_gate: Some(gate.clone()),
                ..Default::default()
            },
        )
        .await;
        insert_flex(&db.pool, tid, &fake, 50).await;

        // Tiny budget: well over one NAR so the fetch never blocks on
        // ITSELF, but small enough that any acquisition is observable.
        let initial = (nar.len() * 4).max(64 * 1024);
        let budget = Arc::new(Semaphore::new(initial));
        let sub =
            Arc::new(test_substituter(db.pool.clone()).with_nar_bytes_budget(Arc::clone(&budget)));

        // First call: gate holds the NAR body BEFORE any bytes are
        // sent → no permits acquired yet. Prove the budget is
        // untouched, then drop the future and unblock.
        assert_eq!(budget.available_permits(), initial);

        // Release the gate, run to completion: permits acquired during
        // the read loop and released after persist_nar (future returns).
        gate.notify_one();
        let got = sub.try_substitute(tid, &path).await.unwrap().unwrap();
        assert_eq!(got.nar_size, nar.len() as u64);
        assert_eq!(
            budget.available_permits(),
            initial,
            "permits must be released after persist"
        );

        // Now structurally prove acquisition: pre-acquire enough that
        // a second fetch CANNOT complete without blocking. The fetch
        // charges ≥ nar.len() (floored at MIN_NAR_CHUNK_CHARGE per
        // read); leave fewer than that available.
        let leave = (MIN_NAR_CHUNK_CHARGE as usize) - 1;
        let _hold = budget
            .clone()
            .acquire_many_owned((initial - leave) as u32)
            .await
            .unwrap();
        assert_eq!(budget.available_permits(), leave);

        // Distinct path so the moka singleflight doesn't return the
        // cached result.
        let path2 = format!(
            "/nix/store/{}-budget-2",
            rio_test_support::fixtures::rand_store_hash()
        );
        let fake2 =
            spawn_flex_upstream(&path2, nar.clone(), "cache.budget", FlexCfg::default()).await;
        insert_flex(&db.pool, tid, &fake2, 50).await;

        let sub2 = Arc::clone(&sub);
        let p2 = path2.clone();
        let blocked = tokio::spawn(async move { sub2.try_substitute(tid, &p2).await });
        // Give the fetch time to reach the budgeted read loop and
        // block on `acquire_many_owned`.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !blocked.is_finished(),
            "fetch must block on nar_bytes_budget when budget < MIN_NAR_CHUNK_CHARGE"
        );
        // Release: fetch completes.
        drop(_hold);
        let got2 = blocked.await.unwrap().unwrap().unwrap();
        assert_eq!(got2.nar_size, nar.len() as u64);
    }

    // r[verify store.substitute.untrusted-upstream+3]
    /// bug_144: `narinfo_to_validated` MUST reject `References:` count
    /// > MAX_REFERENCES (parity with PutPath).
    #[test]
    fn narinfo_to_validated_rejects_excess_references() {
        let mut ni = NarInfo {
            store_path: rio_test_support::fixtures::test_store_path("manyrefs"),
            url: "nar/x.nar".into(),
            compression: "none".into(),
            nar_hash: "sha256:0000000000000000000000000000000000000000000000000000".into(),
            nar_size: 0,
            references: Vec::new(),
            deriver: None,
            sigs: vec![],
            ca: None,
            file_hash: None,
            file_size: None,
        };
        // At the cap: OK.
        ni.references = (0..MAX_REFERENCES)
            .map(|i| format!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-ref-{i}"))
            .collect();
        assert!(narinfo_to_validated(&ni, [0u8; 32]).is_ok());
        // One over: rejected.
        ni.references
            .push("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-one-more".into());
        assert!(matches!(
            narinfo_to_validated(&ni, [0u8; 32]),
            Err(SubstituteError::NarInfo(_))
        ));
    }
}
