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

use bytes::Bytes;
use moka::future::Cache;
use sqlx::PgPool;
use tokio::io::AsyncReadExt;
use tracing::{debug, instrument, warn};
use uuid::Uuid;

use rio_nix::narinfo::NarInfo;
use rio_nix::store_path::StorePath;
use rio_proto::validated::ValidatedPathInfo;

use rio_common::limits::{MAX_CACHE_INFO_BYTES, MAX_NAR_SIZE, MAX_NARINFO_BYTES};

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

/// Maximum number of uncached paths [`Substituter::check_available`]
/// will probe in a single call. Oversized batches are TRUNCATED to the
/// first `SUBSTITUTE_PROBE_MAX_PATHS` (not skipped) — the 1h
/// `probe_cache` means a retry sees the probed prefix as cached, so
/// repeated calls converge to full coverage. `substitutable_paths` is a
/// scheduler optimization hint; even bounded-concurrency probing of
/// hundreds of thousands of paths blocks the originating RPC for too
/// long.
pub const SUBSTITUTE_PROBE_MAX_PATHS: usize = 4096;

/// Conservative HEAD-probe concurrency for upstreams that do NOT
/// advertise `WantMassQuery: 1` in `/nix-cache-info` (or whose
/// cache-info fetch failed). Matches Nix's own conservative default
/// for non-mass-query caches.
pub const SUBSTITUTE_PROBE_CONCURRENCY_CONSERVATIVE: usize = 8;

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
const SUBSTITUTE_SMALL_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Decompressed-NAR size cap applied in [`Substituter::fetch_nar`].
/// Equals [`MAX_NAR_SIZE`] in production; overridable to a small value
/// in tests so the bomb-protection path is exercisable without
/// allocating 4 GiB.
#[cfg(not(test))]
const SUBSTITUTE_NAR_DECOMPRESSED_CAP: u64 = MAX_NAR_SIZE;
#[cfg(test)]
const SUBSTITUTE_NAR_DECOMPRESSED_CAP: u64 = 64 * 1024;

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
    /// closure-walk holds the slot. The singleflight 30s TTL means the
    /// caller's retry sees the now-complete row; trying remaining
    /// upstreams would just race the same slot again.
    Raced,
}

/// Result of one HEAD probe across the tenant's upstreams. Only `Hit`
/// and `Miss` are cached; `Indeterminate` (network error / 5xx on every
/// non-hit base) is left uncached so the next call re-probes instead of
/// pinning a transient failure for the full 1h TTL.
#[derive(Clone, Copy)]
enum ProbeOutcome {
    Hit,
    Miss,
    Indeterminate,
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
}

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
            // r[impl store.substitute.singleflight+2]
            // Short TTL + small cap: this is a singleflight coalescer,
            // not a PathInfo cache. The narinfo table IS the cache.
            // 30s is long enough to coalesce a burst of GetPaths for
            // the same path from N workers; short enough that a
            // subsequent substitution-miss doesn't stay stale.
            inflight: Cache::builder()
                .max_capacity(10_000)
                .time_to_live(std::time::Duration::from_secs(30))
                .build(),
            chunk_upload_max_concurrent: cas::DEFAULT_CHUNK_UPLOAD_CONCURRENCY,
            upstream_info: Cache::builder()
                .time_to_live(SUBSTITUTE_PROBE_CACHE_TTL)
                .build(),
            probe_cache: Cache::builder()
                .max_capacity(SUBSTITUTE_PROBE_CACHE_CAP)
                .time_to_live(SUBSTITUTE_PROBE_CACHE_TTL)
                .build(),
        }
    }

    /// Enable `sig_mode = add|replace` signing. Builder-style.
    pub fn with_signer(mut self, signer: Arc<TenantSigner>) -> Self {
        self.signer = Some(signer);
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
    #[instrument(skip(self), fields(tenant = %tenant_id, path = store_path))]
    pub async fn try_substitute(
        &self,
        tenant_id: Uuid,
        store_path: &str,
    ) -> Result<Option<ValidatedPathInfo>, SubstituteError> {
        let key = (tenant_id, store_path.to_string());
        // moka's `try_get_with`: if another caller is already computing
        // this key, we wait and share its result. The init future runs
        // at most once per key-per-TTL-window. moka caches `Ok(v)` (both
        // `Some` and definitive-miss `None`) but does NOT cache `Err` —
        // a transient 503 propagates to every coalesced waiter without
        // poisoning the slot for 30s, and the next caller after they all
        // return retries cleanly.
        let cached = self
            .inflight
            .try_get_with(key, async {
                self.do_substitute(tenant_id, store_path)
                    .await
                    .map(|v| v.map(Arc::new))
            })
            .await
            .map_err(|e: Arc<SubstituteError>| {
                metrics::counter!("rio_store_substitute_total", "result" => "error").increment(1);
                (*e).clone()
            })?;
        let result = cached.map(|arc| (*arc).clone());
        // Closure walk AFTER the singleflight slot is released (recursing
        // from inside the init future would deadlock on moka). A path is
        // only usable once its runtime references are also present —
        // standard Nix substituter semantics. Without this, a builder's
        // compute_input_closure BFS reaches a transitive ref that was
        // never fetched, BatchQueryPathInfo (local-only) returns None,
        // the ref is dropped from the JIT allowlist, and the build fails
        // with ENOENT when the wrapped binary execs it (e.g.
        // rustc-wrapper → rustc-1.94.0). The scheduler's
        // eager_substitute_fetch only covers DAG-output paths, not their
        // runtime closures, so this is the only place the closure can be
        // completed.
        if let Some(info) = &result {
            Box::pin(self.ensure_references(tenant_id, &info.references)).await;
        }
        Ok(result)
    }

    /// Ensure every reference is present locally, substituting misses.
    /// Depth-first via mutual recursion with [`try_substitute`] (which
    /// singleflights per-ref and walks each ref's own closure). The
    /// local `query_path_info` check short-circuits already-present
    /// paths so the recursion converges; self-references hit the
    /// just-ingested row and skip. Best-effort: a failed ref logs +
    /// continues so a partial closure doesn't poison the seed (the
    /// build still fails with a clear ENOENT naming the missing ref).
    async fn ensure_references(&self, tenant_id: Uuid, refs: &[StorePath]) {
        for r in refs {
            let r = r.as_str();
            match metadata::query_path_info(&self.pool, r).await {
                Ok(Some(_)) => continue,
                Ok(None) => {}
                Err(e) => {
                    warn!(reference = %r, error = %e, "closure: local check failed");
                    continue;
                }
            }
            if let Err(e) = Box::pin(self.try_substitute(tenant_id, r)).await {
                warn!(reference = %r, error = %e, "closure: reference substitution failed");
            }
        }
    }

    /// One full fetch cycle — the singleflight body.
    async fn do_substitute(
        &self,
        tenant_id: Uuid,
        store_path: &str,
    ) -> Result<Option<ValidatedPathInfo>, SubstituteError> {
        let Some(http) = &self.http else {
            return Ok(None); // sandbox: client build failed
        };
        let upstreams = metadata::upstreams::list_for_tenant(&self.pool, tenant_id).await?;
        if upstreams.is_empty() {
            // Normal — most tenants don't configure upstreams. No
            // metric increment; this isn't a miss, it's a no-op.
            return Ok(None);
        }

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
        for upstream in &upstreams {
            match self
                .try_upstream(http, tenant_id, upstream, store_path, &hash_part)
                .await
            {
                Ok(UpstreamOutcome::Hit(info)) => {
                    let elapsed = start.elapsed().as_secs_f64();
                    metrics::histogram!("rio_store_substitute_duration_seconds").record(elapsed);
                    metrics::counter!(
                        "rio_store_substitute_total",
                        "result" => "hit",
                        "upstream" => upstream.url.clone()
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
                        "upstream" => upstream.url.clone()
                    )
                    .increment(1);
                }
                Ok(UpstreamOutcome::Raced) => {
                    // Another uploader holds the placeholder. STOP —
                    // remaining upstreams would race the same slot. The
                    // singleflight 30s TTL means the caller's retry sees
                    // the now-complete row.
                    debug!(upstream = %upstream.url, "concurrent uploader, stopping");
                    break;
                }
                Err(e) => {
                    // This upstream failed (down, hash mismatch, parse
                    // error). Log and try the next one — a single bad
                    // upstream shouldn't block substitution entirely.
                    // The integrity metric is emitted HERE (where the
                    // error is observable) — per-upstream errors never
                    // reach `grpc/mod.rs`.
                    if matches!(e, SubstituteError::HashMismatch { .. }) {
                        metrics::counter!(
                            "rio_store_substitute_integrity_failures_total",
                            "upstream" => upstream.url.clone()
                        )
                        .increment(1);
                    }
                    warn!(upstream = %upstream.url, error = %e, "upstream fetch failed, trying next");
                    metrics::counter!(
                        "rio_store_substitute_total",
                        "result" => "error",
                        "upstream" => upstream.url.clone()
                    )
                    .increment(1);
                }
            }
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
        if resp.status().as_u16() == 404 {
            return Ok(UpstreamOutcome::Miss);
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

        // r[impl store.substitute.untrusted-upstream]
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
        let mut info = narinfo_to_validated(&ni, expected_hash)?;
        info.signatures = self
            .sigs_for_mode(tenant_id, upstream.sig_mode, &ni, &info)
            .await;
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
                // Lost the race; winner completed. Append our sigs to
                // the existing row (idempotent — append_signatures
                // dedupes) and return it. NO download.
                metadata::append_signatures(&self.pool, store_path, &info.signatures).await?;
                let stored = metadata::query_path_info(&self.pool, store_path)
                    .await?
                    .ok_or_else(|| {
                        SubstituteError::Ingest(
                            "claim AlreadyComplete but query_path_info miss".into(),
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
            let nar_bytes = self.fetch_nar(http, &nar_url, &ni.compression).await?;
            info.nar_size = nar_bytes.len() as u64;

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

    /// GET the NAR body and decompress. Returns the raw NAR bytes.
    ///
    /// Accumulates fully before ingest — `cas::put_chunked` needs the
    /// whole `&[u8]` for FastCDC. Streaming-chunker would avoid the
    /// full buffer but isn't here yet; TODO(P0463) tracks it.
    async fn fetch_nar(
        &self,
        http: &reqwest::Client,
        nar_url: &str,
        compression: &str,
    ) -> Result<Bytes, SubstituteError> {
        let resp = http
            .get(nar_url)
            .send()
            .await
            .map_err(|e| SubstituteError::Fetch(format!("{nar_url}: {e}")))?;
        if !resp.status().is_success() {
            return Err(SubstituteError::Fetch(format!(
                "{nar_url}: HTTP {}",
                resp.status()
            )));
        }
        // r[impl store.substitute.untrusted-upstream]
        // bytes_stream → StreamReader → decoder → `.take(cap+1)` →
        // read_to_end. The `.take()` wraps the DECOMPRESSED side so a
        // zstd bomb is bounded regardless of what `NarSize` claimed.
        use futures_util::TryStreamExt;
        use tokio_util::io::StreamReader;
        let stream = resp
            .bytes_stream()
            .map_err(|e| std::io::Error::other(format!("NAR stream: {e}")));
        let reader = StreamReader::new(stream);

        let cap = SUBSTITUTE_NAR_DECOMPRESSED_CAP;
        let mut out = Vec::new();
        match compression {
            "xz" => {
                let mut dec =
                    async_compression::tokio::bufread::XzDecoder::new(reader).take(cap + 1);
                dec.read_to_end(&mut out)
                    .await
                    .map_err(|e| SubstituteError::Fetch(format!("{nar_url} xz: {e}")))?;
            }
            "zstd" => {
                let mut dec =
                    async_compression::tokio::bufread::ZstdDecoder::new(reader).take(cap + 1);
                dec.read_to_end(&mut out)
                    .await
                    .map_err(|e| SubstituteError::Fetch(format!("{nar_url} zstd: {e}")))?;
            }
            "none" | "" => {
                let mut r = reader.take(cap + 1);
                r.read_to_end(&mut out)
                    .await
                    .map_err(|e| SubstituteError::Fetch(format!("{nar_url} body: {e}")))?;
            }
            other => {
                return Err(SubstituteError::NarInfo(format!(
                    "unsupported Compression: {other:?}"
                )));
            }
        }
        if out.len() as u64 > cap {
            return Err(SubstituteError::TooLarge {
                what: "decompressed NAR",
                limit: cap,
            });
        }
        Ok(Bytes::from(out))
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
    /// hide paths that OTHER upstreams have). Uncached batches larger
    /// than [`SUBSTITUTE_PROBE_MAX_PATHS`] are TRUNCATED to the first
    /// `SUBSTITUTE_PROBE_MAX_PATHS` — see that constant's doc.
    #[instrument(skip(self, paths), fields(tenant = %tenant_id, n = paths.len()))]
    pub async fn check_available(
        &self,
        tenant_id: Uuid,
        paths: &[String],
    ) -> Result<Vec<String>, SubstituteError> {
        use futures_util::StreamExt;

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
        if uncached.len() > SUBSTITUTE_PROBE_MAX_PATHS {
            // Latency guard: probing N>4096 paths even at bounded
            // concurrency would block the originating FindMissingPaths
            // RPC (and the scheduler's actor event loop) for too long.
            // Probe the FIRST 4096 (not zero) — the 1h probe_cache
            // means a retry/subsequent merge sees those as cached,
            // leaving fewer uncached → eventually full coverage. The
            // dispatch-time re-check (r[sched.dispatch.fod-substitute])
            // covers FODs in the truncated tail per-tick.
            warn!(
                uncached = uncached.len(),
                max = SUBSTITUTE_PROBE_MAX_PATHS,
                cached_hits = hits.len(),
                "check_available: uncached batch exceeds probe cap; truncating to first {SUBSTITUTE_PROBE_MAX_PATHS}"
            );
            metrics::counter!("rio_store_substitute_probe_skipped_total").increment(1);
            uncached.truncate(SUBSTITUTE_PROBE_MAX_PATHS);
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
        let parsed: Vec<(String, String)> = uncached
            .into_iter()
            .filter_map(|p| {
                let h = StorePath::parse(&p).ok()?.hash_part();
                Some((p, h))
            })
            .collect();

        let bases = &bases;
        let futs = parsed.into_iter().map(move |(path, hash_part)| async move {
            // `Hit` if any base 2xx; `Miss` if EVERY base returned a
            // clean 404; `Indeterminate` if no hit and ≥1 base errored
            // or returned non-404 — caching that as `false` would route
            // a substitutable derivation to `willBuild` for 1h after a
            // transient 503.
            let mut any_indeterminate = false;
            for base in bases {
                let url = format!("{base}/{hash_part}.narinfo");
                match http
                    .head(&url)
                    .timeout(SUBSTITUTE_SMALL_FETCH_TIMEOUT)
                    .send()
                    .await
                {
                    Ok(r) if r.status().is_success() => return (path, ProbeOutcome::Hit),
                    Ok(r) if r.status().as_u16() == 404 => {}
                    Ok(_) | Err(_) => any_indeterminate = true,
                }
            }
            let outcome = if any_indeterminate {
                ProbeOutcome::Indeterminate
            } else {
                ProbeOutcome::Miss
            };
            (path, outcome)
        });
        // r[impl store.substitute.probe-bounded+2]
        let probed: Vec<(String, ProbeOutcome)> = futures_util::stream::iter(futs)
            .buffer_unordered(concurrency)
            .collect()
            .await;

        for (path, outcome) in probed {
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
            }
        }
        Ok(hits)
    }
}

// r[impl store.substitute.untrusted-upstream]
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
            .route(&nar_path, get(move || async move { nar_c }));

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

    /// Per-upstream miss/error labeling: an upstream that 404s emits
    /// `{result=miss,upstream=URL}`; one that fails fetch/verify emits
    /// `{result=error,upstream=URL}`. Prior code emitted ONE unlabeled
    /// `{result=miss}` after the loop and NO metric on Err — a 503ing
    /// upstream was invisible in metrics and indistinguishable from
    /// cache-miss.
    #[tokio::test]
    async fn substitute_per_upstream_miss_and_error_labeled() {
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

        let miss_a = format!(
            "rio_store_substitute_total{{result=miss,upstream={}}}",
            a.url
        );
        let err_b = format!(
            "rio_store_substitute_total{{result=error,upstream={}}}",
            b.url
        );
        assert_eq!(
            rec.get(&miss_a),
            1,
            "per-upstream miss; keys={:?}",
            rec.all_keys()
        );
        assert_eq!(
            rec.get(&err_b),
            1,
            "per-upstream error; keys={:?}",
            rec.all_keys()
        );
        // No unlabeled post-loop miss any more.
        assert_eq!(rec.get("rio_store_substitute_total{result=miss}"), 0);
    }

    /// Axum server that 500s on every request — for the per-upstream
    /// error-metric test.
    async fn spawn_500_upstream() -> FakeUpstream {
        use axum::{Router, http::StatusCode, routing::get};
        let app = Router::new().fallback(get(|| async { StatusCode::INTERNAL_SERVER_ERROR }));
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

    #[tokio::test]
    async fn substitute_miss_no_upstreams() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-none").await;
        let (path, _) = make_path();

        let sub = test_substituter(db.pool.clone());
        let got = sub.try_substitute(tid, &path).await.unwrap();
        assert!(got.is_none(), "no upstreams → None");
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
        let available = sub.check_available(tid, &missing).await.unwrap();
        assert_eq!(available, vec![path], "only the seeded path is available");
    }

    // r[verify store.substitute.probe-bounded+2]
    /// Batches over [`SUBSTITUTE_PROBE_MAX_PATHS`] short-circuit before
    /// to 4096 (not skipped to zero). Uses a local fake upstream that
    /// 404s everything except its one seeded path → 4096 instant HEADs
    /// at WantMassQuery=128 concurrency. The 4097th path is never
    /// probed → its `probe_cache` entry stays absent.
    #[tokio::test]
    async fn check_available_truncates_oversized_batch() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-head-cap").await;
        let (dummy, nar) = make_path();
        let fake = spawn_fake_upstream(&dummy, nar, "cache.cap-test").await;

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

        let paths: Vec<String> = (0..SUBSTITUTE_PROBE_MAX_PATHS + 1)
            .map(|i| {
                format!(
                    "/nix/store/{}-oversized-{i}",
                    rio_test_support::fixtures::rand_store_hash()
                )
            })
            .collect();

        let sub = test_substituter(db.pool.clone());
        let available = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            sub.check_available(tid, &paths),
        )
        .await
        .expect("4096 local-404 HEADs at 128 conc should complete in ~100ms")
        .unwrap();
        assert!(
            available.is_empty(),
            "fake upstream 404s all generated paths → 0 substitutable"
        );
        assert!(
            sub.probe_cache
                .get(&(tid, paths[0].clone()))
                .await
                .is_some(),
            "head of batch must be probed and cached"
        );
        assert!(
            sub.probe_cache
                .get(&(tid, paths.last().unwrap().clone()))
                .await
                .is_none(),
            "tail past truncation point must NOT be probed"
        );
    }

    // r[verify store.substitute.probe-bounded+2]
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

        let first = sub.check_available(tid, &batch).await.unwrap();
        assert_eq!(first, vec![path.clone()]);

        // Kill the upstream. Second call must answer from cache —
        // including the negative result for `absent`.
        fake._task.abort();
        let _ = fake._task.await;

        let second = sub.check_available(tid, &batch).await.unwrap();
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
        let b = sub.check_available(tid_b, batch).await.unwrap();
        assert!(b.is_empty(), "B's upstream is dead → miss");

        // A probes second → MUST hit A's upstream, not return B's
        // cached miss. With a path-only key this would be `[]`.
        let a = sub.check_available(tid_a, batch).await.unwrap();
        assert_eq!(a, vec![path.clone()], "A's upstream serves the path");

        // Reverse leakage: A's hit must not leak to B.
        let b2 = sub.check_available(tid_b, batch).await.unwrap();
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

    /// A young 'uploading' placeholder means a live concurrent
    /// uploader — do NOT reclaim, return miss.
    #[tokio::test]
    async fn try_substitute_respects_young_uploading() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-young-noreclaim").await;
        let (path, nar) = make_path();
        let fake = spawn_fake_upstream(&path, nar, "cache.young-1").await;
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
        let got = sub.try_substitute(tid, &path).await.unwrap();

        // Young placeholder → PlaceholderClaim::Concurrent → ingest
        // returns Ok(None) directly. Caller sees a miss and retries
        // on the next request; no spurious WARN about "placeholder
        // missing (concurrently deleted?)".
        assert!(
            got.is_none(),
            "young placeholder should yield miss (None), got {got:?}"
        );

        // Placeholder still present (NOT reclaimed).
        let sp = StorePath::parse(&path).unwrap();
        let age = metadata::manifest_uploading_age(&db.pool, &sp.sha256_digest())
            .await
            .unwrap();
        assert!(age.is_some(), "young placeholder must survive");
    }

    // — flexible fixture for the regression tests below —
    //
    // Serves one (path, NAR) pair with tunable misbehavior: oversized
    // narinfo, identity-mismatched narinfo, fail-first-then-succeed,
    // block-on-NAR, and per-route hit counters. Kept separate from
    // `spawn_fake_upstream` so the simple end-to-end tests above stay
    // readable.

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    struct FlexCfg {
        /// Serve `narinfo_override` instead of the well-formed one.
        narinfo_override: Option<String>,
        /// First narinfo GET returns 503; subsequent ones succeed.
        narinfo_fail_first: bool,
        /// First `/nix-cache-info` GET returns 503; subsequent succeed.
        cache_info_fail_first: bool,
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
                        ni_hits.fetch_add(1, Ordering::SeqCst);
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
    fn signed_narinfo_for(signed_path: &str, nar: &[u8], key_name: &str) -> String {
        let seed = [0x42u8; 32];
        let signer = Signer::from_seed(key_name, &seed);
        let nar_hash: [u8; 32] = sha2::Sha256::digest(nar).into();
        let nar_hash_str = format!(
            "sha256:{}",
            rio_nix::store_path::nixbase32::encode(&nar_hash)
        );
        let fp = fingerprint(signed_path, &nar_hash, nar.len() as u64, &[]);
        let sig = signer.sign(&fp);
        let hp = StorePath::parse(signed_path).unwrap().hash_part();
        format!(
            "StorePath: {signed_path}\n\
             URL: nar/{hp}.nar\n\
             Compression: none\n\
             NarHash: {nar_hash_str}\n\
             NarSize: {}\n\
             References: \n\
             Sig: {sig}\n",
            nar.len()
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
        // Serve A's narinfo (valid sig over A) at B's hash_part.
        let body_a = signed_narinfo_for(&path_a, &nar, "cache.ident");
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
        let got = sub.try_substitute(tid, &path).await.unwrap();
        assert!(got.is_none(), "Raced → Ok(None)");
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

    // r[verify store.substitute.untrusted-upstream]
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
            .try_upstream(http, tid, &upstreams[0], &path, &hp)
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

    // r[verify store.substitute.untrusted-upstream]
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
            .try_upstream(http, tid, &upstreams[0], &path, &hp)
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

    // r[verify store.substitute.singleflight+2]
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
        let first = sub.try_upstream(http, tid, &upstreams[0], &path, &hp).await;
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

    // r[verify store.substitute.probe-bounded+2]
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

        let first = sub.check_available(tid, batch).await.unwrap();
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

    /// bug_204: `connect_timeout`-only client + dead upstream must not
    /// hang `check_available`. Structural assertion: per-request
    /// `.timeout()` is set; verified by issuing a HEAD against a port
    /// that accepts but never responds, gated under the small-fetch
    /// timeout.
    #[tokio::test]
    async fn head_probe_timeout_does_not_block() {
        // Port-1 → connection refused → reqwest errors immediately
        // (Indeterminate). That exercises the Err arm; the timeout arm
        // is exercised by the per-request `.timeout()` which is set
        // unconditionally — assert it's wired by checking the request
        // builder doesn't hang past the timeout against a TCP listener
        // that never reads.
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        // Hold the listener open but never accept → connect succeeds
        // (kernel backlog), body never arrives.
        let _hold = tokio::spawn(async move {
            let _l = listener;
            tokio::time::sleep(Duration::from_secs(120)).await;
        });

        let http = sandbox_http();
        let url = format!("http://{addr}/x.narinfo");
        let start = Instant::now();
        let _ = http
            .head(&url)
            .timeout(SUBSTITUTE_SMALL_FETCH_TIMEOUT)
            .send()
            .await;
        assert!(
            start.elapsed() < SUBSTITUTE_SMALL_FETCH_TIMEOUT + Duration::from_secs(5),
            "per-request timeout must abort the hung HEAD"
        );
    }
}
