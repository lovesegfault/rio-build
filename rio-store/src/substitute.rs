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
use tracing::{debug, info, instrument, warn};
use uuid::Uuid;

use rio_nix::narinfo::NarInfo;
use rio_nix::store_path::StorePath;
use rio_proto::validated::ValidatedPathInfo;

use rio_common::limits::{
    MAX_CACHE_INFO_BYTES, MAX_NAR_SIZE, MAX_NARINFO_BYTES, MAX_REFERENCES, MAX_SIGNATURES,
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

/// Default owner-side download-stall window
/// (`r[store.substitute.stall-abort]`): a substitution download with no
/// body bytes for this long is aborted by its own owner, the claim
/// released in place. 180 s = 6 missed-progress heartbeats — S3
/// multipart pauses and admission queueing inside a download don't
/// trip it; the persist phase is exempt entirely (the stall rules are
/// scoped `fetched_bytes < nar_size`). Config: `substitute_stall_secs`
/// / env `RIO_SUBSTITUTE_STALL_SECS`.
pub const DEFAULT_SUBSTITUTE_STALL_WINDOW: Duration = Duration::from_secs(180);

/// How old an `'uploading'` placeholder must be before the
/// substitution ingest path reclaims it instead of returning a miss.
///
/// 5 minutes: long enough that a real concurrent substitution (even a
/// multi-GB NAR over a slow link) finishes first; short enough that an
/// rsb retry loop doesn't wait for the orphan scanner's 15-minute sweep.
pub const SUBSTITUTE_STALE_THRESHOLD: Duration = Duration::from_secs(5 * 60);

/// Fallback re-poll interval for a raced waiter parked on the
/// placeholder-event subscription (live_055(b),
/// `r[store.substitute.raced-subscribe]`). NAMED VIOLABLE (R17, time
/// axis): this is the NOTIFY-loss healing bound AND the
/// takeover-latency tax parking adds — every fallback wake re-runs
/// the FULL claim (whose stall/zero-progress arms perform takeover),
/// so a wedged holder is reclaimed at most one interval after its
/// takeover window opens. 15 s = stall window / 12 (≤ 8.3% added to
/// the 180 s reclaim MTTR) while a NOTIFY-dropped release still heals
/// ~24× faster than the 300 s heartbeat reap, at ~4 re-claim
/// round-trips per parked minute worst-case (vs the incident's ~2/s).
/// Tests shrink it via [`Substituter::with_raced_park`]; the
/// violation red proves the knob binds.
pub const RACED_PARK_FALLBACK_POLL: Duration = Duration::from_secs(15);

/// Attempts-axis twin of the park's time budget (R17 all-axes): the
/// park budget is DERIVED, never free-standing —
/// `stall_window + 2 × fallback` (reach the zero-progress takeover
/// boundary, plus one fallback to observe it opening and one for
/// NOTIFY-loss slack). With defaults: 180 + 30 = 210 s, ≈ 14 fallback
/// re-claims worst-case. Past the budget the waiter returns
/// `Err(Raced)` — the executor's RetryLater plane stays the backstop,
/// now at per-budget cadence instead of per-poll.
const RACED_PARK_BUDGET_FALLBACK_SLACK: u32 = 2;

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
/// [`MAX_NAR_SIZE`] decompressed cap bounds its size and the per-read
/// stall watchdog (`r[store.substitute.stall-abort]`) bounds its
/// idleness).
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
///
/// Test override mirrors `SUBSTITUTE_NAR_DECOMPRESSED_CAP`'s: the
/// in-flight tick path (expected leading done — merged_bug_195's pin)
/// must be exercisable under the 64 KiB test NAR cap.
#[cfg(not(test))]
pub const SUBSTITUTE_PROGRESS_INTERVAL_BYTES: u64 = 1024 * 1024;
#[cfg(test)]
pub const SUBSTITUTE_PROGRESS_INTERVAL_BYTES: u64 = 16 * 1024;

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

    /// Owner-side download-stall abort
    /// (`r[store.substitute.stall-abort]`): no NAR body bytes arrived
    /// for the configured stall window, so this owner aborted its own
    /// download and released the placeholder claim **in place** (claim
    /// cleared, progress NULLed, durable `stall_count` incremented).
    /// Returned as `Err` so moka does NOT cache it — every coalesced
    /// singleflight waiter sees the stall (the singleflight-leader
    /// blind spot: no competing `claim_placeholder` would ever observe
    /// it) and the next attempt re-claims the released row
    /// immediately. The materialization executor classifies this as
    /// infrastructure trouble (retryable), never a miss.
    #[error("download stalled: no body bytes for {window:?}; claim released in place")]
    Stalled { window: Duration },

    /// A NAR-budget hold outlived its typed transfer-deadline envelope
    /// (`NarHoldEnvelope`): the leg held budget permits
    /// past `NarHoldEnvelope`'s deadline (grace + byte-basis at the
    /// floor rate) without completing. The per-read stall clock cannot
    /// see this shape — an adversarial trickle (or a black-holed
    /// persist) satisfies every per-read window while pinning the pool
    /// indefinitely. Returned as `Err` so moka does NOT cache it; the
    /// reservation is credited back by drop on the return path, so a
    /// parked sibling's whole-NAR head is granted within the residual
    /// holder bounds. `hold_budget` is the envelope's derived total
    /// (operator-meaningful; the raw deadline instant is not).
    #[error(
        "nar-budget hold exceeded its transfer deadline \
         ({declared} declared bytes, {hold_budget:?} hold budget); aborted"
    )]
    HoldDeadlineExceeded {
        declared: u64,
        hold_budget: Duration,
    },

    /// The tenant's AGGREGATE outstanding reservation charge would
    /// exceed `TENANT_RESERVATION_CAP` — the budget law's COST axis:
    /// substitution charge is declaration-priced (a hostile upstream's
    /// lies cost nothing to make), so per-tenant accounting refuses at
    /// the constructor instead of letting one tenant's declarations
    /// pin the shared pool. Typed REFUSAL, never queueing — returned
    /// as `Err` so moka does NOT cache it; the executor's retry
    /// machinery absorbs it once earlier reservations drain.
    #[error("tenant nar-budget reservation cap reached ({cap} bytes outstanding); retry")]
    TenantBudgetExhausted { cap: u64 },

    /// Transient upstream-429 (`r[store.substitute.probe-429-retry+3]`).
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
    /// complete_manifest, chunked S3 upload / chunk-row upsert).
    #[error("ingest failed: {0}")]
    Ingest(String),

    /// merged_bug_005: every non-serving upstream that HAD the path
    /// answered present-but-untrusted (no sig verified against its
    /// `trusted_keys`) and nothing served, stalled, 429'd, or
    /// errored. Returned as `Err` so moka does NOT cache it — keys
    /// can be fixed; pinning the refusal as a miss would hide the
    /// repair for the TTL window. The materialization executor
    /// settles it as Unobtainable-with-cause, UNCHARGED.
    #[error(
        "upstream narinfo present but no signature verified against \
         trusted_keys (rotated or mistyped key?)"
    )]
    UntrustedPresent,

    /// merged_bug_046: every non-serving upstream that HAD the path
    /// answered with a narinfo whose content DISAGREES with the
    /// locally-stored row (the AlreadyComplete dedup arm's
    /// nar_hash/nar_size/reference-set check) and nothing served,
    /// stalled, 429'd, errored, or trust-refused. Returned as `Err`
    /// so moka does NOT cache it — pinning the disagreement as a
    /// definitive miss would let the content-blind HEAD confirmation
    /// contradict it into a per-retry infrastructure charge (the
    /// merged_bug_005 laundering loop, one axis over). The
    /// materialization executor settles it Unobtainable-with-cause,
    /// UNCHARGED, skipping the HEAD confirmation.
    #[error(
        "upstream narinfo names the path but claims different bytes \
         than the stored row (nar_hash/nar_size/reference disagreement)"
    )]
    ContentMismatch,
}

impl From<metadata::MetadataError> for SubstituteError {
    fn from(e: metadata::MetadataError) -> Self {
        SubstituteError::Ingest(e.to_string())
    }
}

/// The ONE exhaustive `SubstituteError` → (kernel class, duration
/// advice) hop (merged_bug_178 + merged_bug_044): the substitute
/// loop's evidence recording and the materialization executor's
/// failure classification both route through this match — NO
/// catch-all, so adding a `SubstituteError` variant breaks this
/// build together with the kernel's class-keyed routing. The advice
/// channel carries the class's duration evidence (stall window /
/// parsed `Retry-After`); classes without one yield `None`
/// exhaustively, so a future advice-carrying variant must name
/// itself here.
pub(crate) fn substitute_error_evidence(
    e: &SubstituteError,
) -> (
    rio_evidence_kernel::outcome::SubstituteFailureClass,
    Option<Duration>,
) {
    use rio_evidence_kernel::outcome::SubstituteFailureClass as C;
    match e {
        SubstituteError::Raced => (C::Raced, None),
        SubstituteError::RateLimited { retry_after } => (C::RateLimited, *retry_after),
        SubstituteError::Stalled { window } => (C::Stalled, Some(*window)),
        // The hold-envelope abort is a budget-law stall: the leg's
        // integral progress fell below the typed floor (trickle /
        // black-holed persist) — the same operational posture as the
        // per-read stall (claim released/aborted, infrastructure
        // trouble, retryable), with the hold budget as the duration
        // advice. Mapping it onto `C::Stalled` keeps the kernel's
        // class alphabet untouched (no new class is commissioned this
        // wave); the typed Rust variant preserves the distinction at
        // every in-crate consumer.
        SubstituteError::HoldDeadlineExceeded { hold_budget, .. } => {
            (C::Stalled, Some(*hold_budget))
        }
        // The tenant-cap refusal is a LOCAL capacity refusal with the
        // same operational posture as the admission gate's (typed,
        // uncached, retry once in-flight work drains) — mapped onto
        // `C::AdmissionSaturated` so the kernel's class alphabet stays
        // untouched; the typed Rust variant preserves the distinction
        // at every in-crate consumer.
        SubstituteError::TenantBudgetExhausted { .. } => (C::AdmissionSaturated, None),
        SubstituteError::Admission(_) => (C::AdmissionSaturated, None),
        SubstituteError::Fetch(_) | SubstituteError::NarInfo(_) => (C::Fetch, None),
        SubstituteError::HashMismatch { .. }
        | SubstituteError::SizeMismatch { .. }
        | SubstituteError::TooLarge { .. } => (C::Integrity, None),
        SubstituteError::Ingest(_) => (C::Ingest, None),
        SubstituteError::UntrustedPresent => (C::Untrusted, None),
        SubstituteError::ContentMismatch => (C::ContentMismatch, None),
    }
}

/// The owner-side placeholder cleanup posture after a failed
/// substitute leg (bug_108): TOTAL over [`SubstituteError`] — derived
/// from the TYPE, never re-decided positionally at a fold (the
/// `HoldDeadlineExceeded` defect: the variant was minted for exactly
/// the adversarial-trickle population, the in-file classifier mapped
/// it to the same `C::Stalled` posture and CLAIMED "the typed Rust
/// variant preserves the distinction at every in-crate consumer" —
/// but the cleanup fold's generic arm silently absorbed it: row
/// deleted, strike lost, metric unfired, next attempt restarted at
/// `stall_count = 0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CleanupPosture {
    /// Release the claim IN PLACE (claim cleared, progress NULLed,
    /// durable `stall_count += 1`, `stale_reclaimed{stall_abort}`
    /// emitted, row survives for immediate re-claim) — the
    /// stall-family posture: the path is fine, the
    /// infrastructure/transfer stalled, and the evidence must
    /// accumulate across owners.
    StrikeReleaseInPlace,
    /// Delete the placeholder (claim-gated abort) — the default for
    /// integrity/refusal/transient failures with no stall evidence to
    /// preserve.
    Abort,
}

// r[impl store.substitute.stall-abort+2]
/// THE one exhaustive `SubstituteError` → [`CleanupPosture`] hop —
/// the same shape as [`substitute_error_evidence`] one decision over:
/// NO catch-all, so a new variant forces a compile-time posture
/// decision HERE instead of falling silently to whichever arm a fold
/// author wrote last (R22's fold-site axis; the negative census is
/// "no `_ =>` over a closed error alphabet at a posture decision").
// census[test: cleanup_posture_is_total_and_strike_parity]
pub(crate) fn cleanup_posture(e: &SubstituteError) -> CleanupPosture {
    use CleanupPosture as P;
    match e {
        // The per-read stall and the hold-deadline expiry are the SAME
        // operational posture (the classifier's sentence, now true at
        // this consumer too): integral progress fell below the typed
        // floor — strike, release in place, keep the row claimable.
        SubstituteError::Stalled { .. } => P::StrikeReleaseInPlace,
        SubstituteError::HoldDeadlineExceeded { .. } => P::StrikeReleaseInPlace,
        SubstituteError::Raced
        | SubstituteError::RateLimited { .. }
        | SubstituteError::TenantBudgetExhausted { .. }
        | SubstituteError::Admission(_)
        | SubstituteError::Fetch(_)
        | SubstituteError::NarInfo(_)
        | SubstituteError::HashMismatch { .. }
        | SubstituteError::SizeMismatch { .. }
        | SubstituteError::TooLarge { .. }
        | SubstituteError::Ingest(_)
        | SubstituteError::UntrustedPresent
        | SubstituteError::ContentMismatch => P::Abort,
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
    /// `Miss`/`Raced` arms are zero-sized. `ingested_bytes` is the
    /// producer's OWN statement of bytes actually written
    /// (merged_bug_091): the download path states `nar_size`, the
    /// AlreadyComplete dedup arm states 0 — the bytes counter
    /// consumes the fact instead of guessing from the row, so a
    /// zero-ingest dedup hit can never inflate \"bytes ingested\".
    Hit {
        info: Box<ValidatedPathInfo>,
        ingested_bytes: u64,
    },
    /// narinfo 404 — this upstream doesn't have it.
    Miss,
    /// merged_bug_005: narinfo PRESENT and identity-correct, but no
    /// signature verified against this upstream's `trusted_keys`. A
    /// deterministic trust refusal — recorded as its own evidence
    /// cell so neither leg can read it as a miss ("as good as 404"
    /// laundered key-rotation skew into the cacheable miss lane,
    /// where the sig-blind HEAD confirmation re-found the path and
    /// charged infrastructure forever) or as a hit.
    UntrustedPresent,
    /// merged_bug_046: narinfo PRESENT and identity-correct, but its
    /// content claim (nar_hash/nar_size/references) DISAGREES with
    /// the locally-stored row at the AlreadyComplete dedup arm. A
    /// deterministic content refusal — recorded as its own evidence
    /// cell (mirroring [`Self::UntrustedPresent`]) so neither leg can
    /// read it as a miss or as a hit; the loop fails over (another
    /// upstream may carry agreeing bytes).
    ContentMismatch,
    /// `claim_placeholder` returned `Concurrent` — another replica or
    /// closure-walk holds the slot. Mapped to `Err(Raced)` in
    /// `do_substitute` so moka does not cache it; caller's retry
    /// re-runs and reaches `AlreadyComplete`. Trying remaining
    /// upstreams would just race the same slot again.
    Raced,
}

/// The two capabilities a substitution leg needs, gated in ONE place
/// with ONE ordering ([`Substituter::capability_gate`],
/// merged_bug_030): upstream configuration first, HTTP client second.
/// Both legs (`try_substitute_inner`'s singleflight body and
/// `check_available`) consume this — the per-class congruence law
/// (`r[store.materialize.probe-polarity]`) holds at the capability
/// tier by construction because the ordering exists exactly once.
enum CapabilityGate<'a> {
    /// Tenant has no upstreams configured: a clean no-op on both legs
    /// (attempt: definitive miss; probe: empty result).
    NoUpstreams,
    /// Upstreams ARE configured but the replica has no HTTP client:
    /// a capability fault on both legs (attempt: uncached
    /// `Err(Fetch)`; probe: all-indeterminate).
    Clientless,
    /// Both capabilities present.
    Ready {
        http: &'a reqwest::Client,
        upstreams: Vec<Upstream>,
    },
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
/// it (`r[store.substitute.probe-429-retry+3]`).
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
    /// sandbox: no CA bundle → rustls-native-certs errors). The
    /// consequences are per-arm via [`CapabilityGate`] (the ONE
    /// ordering, merged_bug_030/044 — see its enum doc, the
    /// authority): NO upstreams configured → clean no-op (`Ok(None)` /
    /// `[]`); upstreams configured but clientless → a capability
    /// fault, NOT a miss — attempts return an UNCACHED `Err(Fetch)`
    /// counted under `skipped_total{reason=no_http_client}`, probes
    /// answer all-indeterminate. Production always has a CA bundle;
    /// only the sandboxed test run hits this.
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
    // r[impl store.put.nar-bytes-budget+6]
    /// Global budget for in-flight NAR bytes — the SAME sealed
    /// [`crate::budget::NarBudget`] PutPath draws from (wired in
    /// main.rs via [`with_nar_budget`](Self::with_nar_budget); the
    /// raw semaphore is module-private to `crate::budget` —
    /// merged_bug_005's seal — and the budget carries the one
    /// tenant ledger both planes charge). Without this, N concurrent
    /// distinct-path substitutions = N × 4 GiB RSS with zero
    /// backpressure (the moka singleflight only coalesces same
    /// `(tenant, path)`). The substitute leg charges its WHOLE
    /// declared size in ONE [`NarBudgetReservation`] before the NAR GET
    /// (live_047/R-C: a budget waiter holds zero permits — no
    /// hold-and-wait — and the leg's entire cumulative charged demand
    /// is its single reservation `< MAX_NAR_SIZE`); the reservation
    /// rides the bytes through the hash detach and drops after
    /// `persist_nar` returns.
    nar_budget: crate::budget::NarBudget,
    /// Per-replica admission gate on concurrent singleflight LEADERS.
    /// `None` (tests / no-op) skips gating. main.rs wires the SAME
    /// [`AdmissionGate`] clone here and into `StoreAdminServiceImpl`
    /// so `GetLoad` reports the utilization the ComponentScaler
    /// reacts to. Acquired inside the moka init future — coalesced
    /// waiters on the same `(tenant, path)` do NOT consume permits.
    admission: Option<AdmissionGate>,
    /// Owner-side download-stall window
    /// ([`DEFAULT_SUBSTITUTE_STALL_WINDOW`]): both the `fetch_nar`
    /// abort budget and the takeover threshold competing claimants
    /// apply via [`ingest::SubstituteClaimParams`]. main.rs wires
    /// `Config::substitute_stall` here.
    stall_window: Duration,
    /// Floor decompressed-throughput for the NAR-hold envelope
    /// ([`NAR_HOLD_FLOOR_RATE`], bytes/second). The R17 violability
    /// lane: tests/ops override via
    /// [`Self::with_nar_hold_floor_rate`]; each envelope axis has a
    /// violating red in the in-file battery.
    nar_hold_floor_rate: u64,
    /// Cap for the budget's tenant ledger
    /// ([`TENANT_RESERVATION_CAP`]); violable via
    /// [`Self::with_tenant_reservation_cap`] (R17). The LEDGER
    /// instance itself rides [`Self::nar_budget`] (one pool = one
    /// accounting, by construction).
    tenant_reservation_cap: u64,
    /// Owner attribution stamped on substitution-claimed placeholders
    /// (`manifests.claimed_by`): the pod name (`HOSTNAME`), or
    /// `"unknown"` outside k8s.
    claimed_by: String,
    // r[impl store.substitute.raced-subscribe]
    /// Placeholder-event fanout for raced waiters (live_055(b)): the
    /// PG LISTEN task (lazily spawned by
    /// [`Self::ensure_placeholder_listener`]) broadcasts each
    /// announced `store_path_hash`; parked waiters in
    /// [`Self::try_substitute_subscribed`] filter for theirs. One
    /// channel, no per-hash registry to leak: a waiter that finds a
    /// foreign hash just keeps waiting; `Lagged` is a conservative
    /// wake (re-check now). Capacity 1024 ≫ plausible concurrent
    /// terminal events between two polls of a single waiter.
    raced_events: tokio::sync::broadcast::Sender<Vec<u8>>,
    /// At-most-once spawn latch for the PG LISTEN task. Lazy (first
    /// park) so gRPC-only processes and the unparked test majority
    /// never hold a LISTEN connection.
    raced_listener_started: std::sync::atomic::AtomicBool,
    /// [`RACED_PARK_FALLBACK_POLL`], violable via
    /// [`Self::with_raced_park`] (R17 — tests shrink it; the
    /// violation red proves it binds).
    raced_park_fallback: Duration,
    /// Optional override for the DERIVED park budget
    /// (`stall_window + 2 × fallback` when `None`). Tests pin it
    /// (e.g. `Some(ZERO)` = the pre-subscription immediate-Raced
    /// observable) via [`Self::with_raced_park`].
    raced_park_budget: Option<Duration>,
    /// Test-only gate INSIDE the hash `spawn_blocking` closure: lets a
    /// test hold the detached digest task mid-flight so the W-6a
    /// reservation-residency law (budget credited back only after the
    /// blocking task — which owns the NAR bytes — completes) is
    /// assertable deterministically.
    #[cfg(test)]
    hash_gate: Option<HashGate>,
    /// Test-only async gate at the head of `persist_nar`'s span (the
    /// `hash_gate` seam form, one step later): models a black-holed
    /// persist backend — rio-common's S3 client ships NO TimeoutConfig
    /// by design (per-operation deadlines are the caller's duty,
    /// Q-108), so an established-then-black-holed connection awaits
    /// response headers forever. The W8-B′ red parks here and asserts
    /// the call-site tail timeout releases the reservation by the hold
    /// deadline. Async (`Notify`) so the tail `timeout` can CANCEL the
    /// park — unlike the hash gate, whose `spawn_blocking` host tokio
    /// never cancels (that lag is priced, not enforced).
    #[cfg(test)]
    persist_gate: Option<Arc<tokio::sync::Notify>>,
}

/// Test hook: a sync gate the hash `spawn_blocking` closure holds on.
/// `hold()` signals entry (tokio unbounded send — sync, callable from
/// the blocking thread) then parks on a condvar until `release()`.
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct HashGate {
    entered: tokio::sync::mpsc::UnboundedSender<()>,
    release: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
}

#[cfg(test)]
impl HashGate {
    pub(crate) fn new() -> (Self, tokio::sync::mpsc::UnboundedReceiver<()>) {
        let (entered, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            Self {
                entered,
                release: Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new())),
            },
            rx,
        )
    }

    fn hold(&self) {
        let _ = self.entered.send(());
        let (m, c) = &*self.release;
        let mut released = m.lock().unwrap();
        while !*released {
            released = c.wait(released).unwrap();
        }
    }

    pub(crate) fn release(&self) {
        let (m, c) = &*self.release;
        *m.lock().unwrap() = true;
        c.notify_all();
    }
}

/// Default NAR-buffer budget when no shared semaphore is wired:
/// `8 × MAX_NAR_SIZE` (32 GiB) — same as `StoreServiceImpl`'s default.
/// In production main.rs replaces this with the SHARED semaphore so
/// PutPath and substitution draw from one pool.
const DEFAULT_SUBSTITUTE_NAR_BUDGET: usize = (8 * MAX_NAR_SIZE) as usize;

// E-2(ii) backstop, live_047/R-C: the unwired-default constructor must
// admit one whole-NAR reservation (the config-plane premise is the
// validate() floor `nar_buffer_budget_bytes >= MAX_NAR_SIZE`; this
// pins the None-default symbol). And the `>=` declared-size rejection
// makes the max reservation (`MAX_NAR_SIZE − 1`) u32-expressible for
// `acquire_many_owned`.
const _: () = assert!(MAX_NAR_SIZE as usize <= DEFAULT_SUBSTITUTE_NAR_BUDGET);
const _: () = assert!(MAX_NAR_SIZE - 1 <= u32::MAX as u64);

/// Floor decompressed-throughput axis of the NAR-hold envelope
/// (bytes/second). Derivation: a holder's transfer-time allowance is
/// `bytes_basis / NAR_HOLD_FLOOR_RATE` on top of the grace term's
/// fixed grace — at 256 KiB/s a `4 GiB − 1` declaration yields a
/// ≈ 4.55 h ceiling, while an honest mirror sustaining ≥ 1 MiB/s
/// clears with ≥ 4× margin. The ghc-binary rationale (the HTTP client
/// is connect-timeout-only because a multi-GB body legitimately runs
/// long) is PRESERVED: legitimate large transfers have real
/// throughput; only a hold whose integral rate falls below this floor
/// — the shape the per-read stall clock structurally cannot see — is
/// aborted. Violable per R17 via [`Substituter::with_nar_hold_floor_rate`].
pub(crate) const NAR_HOLD_FLOOR_RATE: u64 = 256 * 1024;

/// Grace multiplier over the stall window for the NAR-hold envelope:
/// `NAR_HOLD_GRACE = NAR_HOLD_GRACE_FACTOR × stall_window` (default
/// 5 × 180 s = 15 min). Strictly looser than any single per-read
/// window — the factor is pinned ≥ 2 below (`hold_grace_exceeds_
/// stall_window`) so the hold deadline can never preempt a single
/// healthy read; it only fires on integral slowness across reads.
pub(crate) const NAR_HOLD_GRACE_FACTOR: u32 = 5;

// Const-relation pin `hold_grace_exceeds_stall_window` (R17 ordering
// law between nested budgets): the envelope's fixed grace is ≥ 2×
// any stall window it is derived from, so the per-read clock always
// fires first on a genuinely wedged read and the deadline only ever
// cuts integral slowness.
const _: () = assert!(NAR_HOLD_GRACE_FACTOR >= 2);

// The cost-axis family (TENANT_RESERVATION_CAP, the tenant ledger,
// and the DeclaredCharge constructor) is HOISTED to the shared
// store-crate home `crate::budget` (merged_bug_005): PutPath's
// declared mode is the SAME declaration-priced shape as this plane's
// reserve and consults the SAME law through the one constructor —
// the pre-hoist comparative prose here ("unlike PutPath's
// delivery-priced incremental charge") was falsified by wave-9's
// `reserve_declared`, which acquired by declaration with no ledger
// consult. PutPath's TRAILER mode remains the delivery-priced
// incremental charge, by design outside the declared cap.
pub(crate) use crate::budget::TENANT_RESERVATION_CAP;

// The cap sits below this plane's default pool (so the cap — not the
// pool — is the binding constraint for one tenant). The cap's own
// floor relation (`>= MAX_NAR_SIZE`) is pinned beside its definition
// in `crate::budget`.
const _: () = assert!(TENANT_RESERVATION_CAP as usize <= DEFAULT_SUBSTITUTE_NAR_BUDGET);

// r[impl store.put.nar-hold-envelope+2]
/// The typed, violable transfer-deadline envelope every NAR-budget
/// HOLD must carry (merged_bug_021's hold-TIME axis; R17 all-axes).
/// One discipline: **waiters park free; holders expire** — the
/// zero-holding budget park ([`NarBudgetReservation::reserve`]) stays
/// untimed backpressure (the merged_bug_003 ruling, preserved
/// verbatim on the wait edge), while every span that actually HOLDS
/// permits is bounded by this envelope's deadline.
///
/// Derivation (constants above): `hold_budget = NAR_HOLD_GRACE_FACTOR
/// × stall_window + bytes_basis / floor_rate`. The deadline is ARMED
/// at permit grant ([`Self::arm`]), not at construction — a parked
/// waiter holds nothing, so park time must not consume hold time
/// (the slot invariant binds holders, and the substitute park's
/// boundedness is the FIFO-induction theorem, not a clock).
#[derive(Debug, Clone, Copy)]
pub(crate) struct NarHoldEnvelope {
    /// Total typed hold budget (grace + transfer allowance).
    hold_budget: Duration,
    /// Byte basis the budget was derived from: the declared NarSize
    /// (substitution) or the `MAX_NAR_SIZE` charged-permit cap
    /// (PutPath/PutPathBatch ingest — the handler's own ceiling).
    bytes_basis: u64,
    /// Hold deadline. Provisional at construction; re-stamped by
    /// [`Self::arm`] when the hold actually begins.
    deadline: tokio::time::Instant,
}

impl NarHoldEnvelope {
    /// Envelope for a substitute leg's whole-NAR reservation: basis is
    /// the verified narinfo's declared `NarSize`.
    pub(crate) fn for_declared(declared: u64, stall_window: Duration, floor_rate: u64) -> Self {
        Self::derive(declared, stall_window, floor_rate)
    }

    /// Envelope for an incremental ingest holder (PutPath/PutPathBatch):
    /// basis is the `MAX_NAR_SIZE` charged-permit cap — the most any
    /// single handler may ever hold (`charged >= MAX_NAR_SIZE` is
    /// rejected before each acquire), so the deadline is sufficient
    /// for the largest lawful upload at the floor rate.
    pub(crate) fn for_ingest_cap(stall_window: Duration, floor_rate: u64) -> Self {
        Self::derive(MAX_NAR_SIZE, stall_window, floor_rate)
    }

    fn derive(bytes_basis: u64, stall_window: Duration, floor_rate: u64) -> Self {
        let grace = stall_window.saturating_mul(NAR_HOLD_GRACE_FACTOR);
        // `max(1)`: a zero floor rate would be division by zero; the
        // production const is 256 KiB/s, and a 1-byte/s override is
        // the loosest expressible envelope (≈ 136 years for 4 GiB —
        // within tokio Instant range), not an unbounded one. Integer
        // floor division (exact for the production pair: 4 GiB at
        // 256 KiB/s = 16384 s); the sub-second remainder is noise
        // against the ≥ 2×-stall grace, and `from_secs_f64` is
        // banned (merged_bug_262 — panics on +inf/NaN).
        let transfer = Duration::from_secs(bytes_basis / floor_rate.max(1));
        let hold_budget = grace.saturating_add(transfer);
        Self {
            hold_budget,
            bytes_basis,
            deadline: tokio::time::Instant::now() + hold_budget,
        }
    }

    /// Stamp the deadline from NOW — called exactly when the hold
    /// begins (permit grant). Construction time (which may precede a
    /// long zero-holding park) does not count against the hold.
    pub(crate) fn arm(mut self) -> Self {
        self.deadline = tokio::time::Instant::now() + self.hold_budget;
        self
    }

    /// Monotone knowledge-improvement TIGHTEN (bug_114, the tiling
    /// law's ingest form): once the stream has drained, the bytes the
    /// hold still has to move are KNOWN (`buffered`), so the tail's
    /// bound can shrink from the arming-time `MAX_NAR_SIZE` cap basis
    /// to `derive(buffered)`-from-now — but NEVER grow. Returns
    /// whichever envelope's deadline is sooner, so the one clock armed
    /// at first grant stays the upper bound on the whole hold (a
    /// stream that already spent most of its cap budget cannot buy a
    /// fresh allowance at drain time; that fresh-clock shape is
    /// exactly the per-span gap this close kills).
    pub(crate) fn tightened_for_remaining(
        self,
        buffered: u64,
        stall_window: Duration,
        floor_rate: u64,
    ) -> Self {
        let candidate = Self::derive(buffered, stall_window, floor_rate);
        if candidate.deadline < self.deadline {
            candidate
        } else {
            self
        }
    }

    /// Time left before the deadline (zero once expired).
    pub(crate) fn remaining(&self) -> Duration {
        self.deadline
            .saturating_duration_since(tokio::time::Instant::now())
    }

    /// Whether the hold deadline has passed.
    pub(crate) fn expired(&self) -> bool {
        self.remaining() == Duration::ZERO
    }

    /// The derived total hold budget (for typed errors/diagnostics).
    pub(crate) fn hold_budget(&self) -> Duration {
        self.hold_budget
    }

    /// The byte basis the budget was derived from.
    pub(crate) fn bytes_basis(&self) -> u64 {
        self.bytes_basis
    }
}

// r[impl store.put.nar-bytes-budget+6]
/// One substitute leg's WHOLE-NAR budget reservation — the single-shot
/// charge that replaced the incremental per-read acquire (live_047/R-C
/// WO-R7-1). THE invariants it carries:
///
/// - **No hold-and-wait:** the one [`Self::reserve`] call is the ONLY
///   acquire site on the substitute leg; a waiter holds zero budget
///   permits, so the multi-holder wedge (Σ holds > budget with every
///   holder incomplete) is structurally unwritable.
/// - **Charged demand == reservation:** the leg's entire cumulative
///   charged demand is `declared.max(MIN_NAR_CHUNK_CHARGE)` `<
///   MAX_NAR_SIZE` (the `>=` declared-size rejection runs first), so
///   the spec's single-handler no-self-deadlock premise holds on this
///   leg for every validate()-passing budget (floor `>= MAX_NAR_SIZE`).
/// - **Lifetime covers byte residency:** the reservation moves WITH
///   `nar_bytes` into the hash `spawn_blocking` closure (§3.2 step 4) —
///   cancelling the leg at the hash detach point cannot credit the
///   budget back while the detached blocking task still owns the
///   buffer — and drops after `persist_nar` returns.
/// - **Every hold expires:** the reservation cannot be constructed
///   without a [`NarHoldEnvelope`] (merged_bug_021 — an unenveloped
///   permit-hold does not typecheck). The envelope is armed at grant
///   and enforced over the read span ([`read_nar_capped`]'s per-read
///   clamp + after-read expiry check) AND the post-read tail (the
///   call-site `timeout(envelope.remaining(), …)` spanning hash →
///   sigs → persist), so neither an adversarial trickle nor a
///   black-holed persist can pin the pool past the deadline.
///
/// The post-fix read loop ([`read_nar_capped`]) takes
/// `&NarBudgetReservation` and neither `self` nor any semaphore — an
/// incremental re-acquire inside it does not typecheck (the §5.1
/// laws-as-types census; the committed site census below is the drift
/// guard).
#[derive(Debug)]
pub(crate) struct NarBudgetReservation {
    /// The fused declaration-priced acquisition: permits + the
    /// tenant's cost-axis charge in ONE RAII value (the shared
    /// [`crate::budget::DeclaredCharge`] constructor — merged_bug_005:
    /// both reservation modes construct it, so a declaration-priced
    /// consumer without the cost axis does not typecheck). Dropping
    /// credits the semaphore back AND restores the tenant's headroom
    /// on every abort path — completion, deadline expiry,
    /// cancellation.
    _charge: crate::budget::DeclaredCharge,
    /// The typed hold envelope, armed at permit grant. Carried so the
    /// read loop and the post-read tail derive every clock from ONE
    /// deadline (`remaining()`), and so the type system makes an
    /// unenveloped hold unwritable.
    envelope: NarHoldEnvelope,
}

impl NarBudgetReservation {
    /// THE reservation site: acquire `declared.max(MIN_NAR_CHUNK_CHARGE)`
    /// permits in one shot. Stamps `ClaimPhase::BudgetParked` for the
    /// wait (merged_bug_003: parking is backpressure, never a strike,
    /// and the takeover predicate exempts a parked owner AS DATA) —
    /// the park now happens BEFORE the upstream connection opens, so a
    /// long park no longer rides an open socket the upstream may drop.
    /// Deliberately OUTSIDE the stall watchdog for the same reason.
    /// The park is also OUTSIDE the hold envelope: `envelope` is armed
    /// only at grant (a waiter holds nothing — its boundedness is the
    /// FIFO-induction theorem over the holders, not a clock). The
    /// tenant ledger is consulted BEFORE the park (cost axis,
    /// WO-S1-2b): an over-cap tenant is REFUSED typed, never queued —
    /// the wait edge gains no new population.
    async fn reserve(
        budget: &crate::budget::NarBudget,
        tenant_id: Uuid,
        tenant_cap: u64,
        declared: u64,
        envelope: NarHoldEnvelope,
        progress_handle: Option<&ingest::ProgressHandle>,
    ) -> Result<Self, SubstituteError> {
        // ONE constructor for both declaration-priced modes
        // (merged_bug_005): size bound, charge floor, ledger consult
        // (BEFORE the park — a refused tenant never queues; the
        // ledger is the sealed budget's own), and the single-shot
        // acquire all live in `DeclaredCharge::new`; this plane only
        // maps the typed refusals onto its own alphabet and stamps
        // the progress phases around the park.
        let charge =
            crate::budget::DeclaredCharge::new(budget, tenant_id, tenant_cap, declared, || {
                if let Some(h) = progress_handle {
                    h.set_phase(ingest::ClaimPhase::BudgetParked);
                }
            })
            .await
            .map_err(|e| match e {
                crate::budget::DeclaredRefusal::TooLarge { limit } => SubstituteError::TooLarge {
                    what: "NarSize",
                    limit,
                },
                crate::budget::DeclaredRefusal::TenantBudgetExhausted { cap } => {
                    SubstituteError::TenantBudgetExhausted { cap }
                }
                crate::budget::DeclaredRefusal::BudgetClosed => {
                    SubstituteError::Fetch("NAR buffer budget closed".into())
                }
            })?;
        if let Some(h) = progress_handle {
            h.set_phase(ingest::ClaimPhase::Downloading);
        }
        Ok(Self {
            _charge: charge,
            envelope: envelope.arm(),
        })
    }

    /// The armed hold envelope (deadline stamped at permit grant).
    pub(crate) fn envelope(&self) -> &NarHoldEnvelope {
        &self.envelope
    }
}

// r[impl store.put.nar-bytes-budget+6]
// r[impl store.put.nar-hold-envelope+2]
/// The substitute leg's read loop, budget-free by construction (§5.1
/// laws-as-types census): its parameter set is the stream, the caps,
/// the watchdog inputs, the progress sinks, and the PROOF a whole-NAR
/// reservation is already held (`&NarBudgetReservation`) — no `self`,
/// no semaphore, so an incremental per-read acquire does not
/// typecheck here.
///
/// Caps, in precedence order per read: a stream exceeding its DECLARED
/// size fails as `SizeMismatch` during the read (the reservation
/// covers exactly `declared`, so buffered bytes stay ≤ reservation by
/// construction); the `cap` arm (`declared.min(SUBSTITUTE_NAR_
/// DECOMPRESSED_CAP)`, enforced via the caller's `.take(cap+1)`) is
/// reachable only in the test-cap regime where the global cap sits
/// below `declared`.
#[allow(clippy::too_many_arguments)]
async fn read_nar_capped(
    capped: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
    declared: u64,
    cap: u64,
    stall: Duration,
    nar_url: &str,
    progress: Option<&SubstProgressFn>,
    progress_handle: Option<&ingest::ProgressHandle>,
    upstream_base: &str,
    reservation: &NarBudgetReservation,
) -> Result<Vec<u8>, SubstituteError> {
    let stalled = || SubstituteError::Stalled { window: stall };
    let envelope = reservation.envelope();
    let deadline_exceeded = || SubstituteError::HoldDeadlineExceeded {
        declared,
        hold_budget: envelope.hold_budget(),
    };
    // Exact-capacity allocation: appends never reallocate, so resident
    // heap for the buffer is bounded by the reservation (+1 sentinel
    // byte) at every instant — no doubling overshoot.
    let mut out = Vec::with_capacity((declared.min(cap) as usize).saturating_add(1));
    let mut buf = vec![0u8; 64 * 1024];
    let mut last_progress = 0u64;
    loop {
        // r[impl store.substitute.stall-abort+2]
        // Per-read stall clock: each successful read restarts it, so a
        // slow-but-advancing stream never trips — only a wedged one
        // (no bytes for the whole window) does. UNCHANGED as a rule —
        // the envelope clamp below is a budget-law clock, not a new
        // stall semantics: `min(stall, remaining)` only shortens the
        // in-read wait when the HOLD deadline lands first, and the
        // grace pin (`NAR_HOLD_GRACE_FACTOR ≥ 2`) guarantees the
        // deadline can never preempt a single healthy read's window
        // until integral slowness has already burned the grace.
        let per_read = stall.min(envelope.remaining());
        let n = match tokio::time::timeout(per_read, capped.read(&mut buf)).await {
            Ok(r) => r.map_err(|e| SubstituteError::Fetch(format!("{nar_url} body: {e}")))?,
            // Which clock fired? If the hold deadline has passed, this
            // is the envelope abort (typed; credits the budget back by
            // drop on the return path); otherwise the full per-read
            // window elapsed byte-free — the classic stall.
            Err(_) if envelope.expired() => return Err(deadline_exceeded()),
            Err(_) => return Err(stalled()),
        };
        if n == 0 {
            break;
        }
        // After-read expiry check: an adversarial trickle satisfies
        // every per-read window (each read delivers ≥ 1 byte inside
        // `stall`) while the integral rate stays below the floor —
        // the merged_bug_021 evasion shape. The hold deadline is the
        // clock that sees it.
        if envelope.expired() {
            return Err(deadline_exceeded());
        }
        out.extend_from_slice(&buf[..n]);
        if out.len() as u64 > declared {
            // Over-delivery against the declaration: fail DURING the
            // read (pre-fix this was caught only after full buffering).
            return Err(SubstituteError::SizeMismatch {
                declared,
                actual: out.len() as u64,
            });
        }
        // r[impl store.substitute.untrusted-upstream+3]
        if out.len() as u64 > cap {
            return Err(SubstituteError::TooLarge {
                what: "decompressed NAR",
                limit: cap,
            });
        }
        // r[impl store.substitute.progress-heartbeat]
        // Advance the durable-progress handle per read; the
        // placeholder guard's heartbeat samples it every tick.
        // Relaxed: single writer, freshness-tolerant reader.
        if let Some(h) = progress_handle {
            h.store_bytes(out.len() as u64);
        }
        // r[impl store.substitute.progress-stream]
        if let Some(cb) = progress {
            let done = out.len() as u64;
            if done - last_progress >= SUBSTITUTE_PROGRESS_INTERVAL_BYTES {
                cb(done, declared, upstream_base);
                last_progress = done;
            }
        }
    }
    Ok(out)
}

/// The placeholder-listener retry budget (bug_172): the
/// success-latch placement doctrine AS A TYPE. The budget resets on
/// DELIVERED work ([`Self::on_delivered`] — the first received
/// notification), NEVER on connection-establishment acks: connect-ok
/// and LISTEN-ok are handshakes, not deliveries. Pre-fix the reset
/// sat on LISTEN-ok, so the connect-ok/LISTEN-ok/recv-fails shape
/// re-reset every cycle and the documented 30s cap never engaged —
/// one fresh pool connection + LISTEN per second per replica for the
/// whole degradation window, against the budgeted PG connection
/// ceiling. The reset is structurally unwritable from the error
/// arms: only the delivery path can call [`Self::on_delivered`].
///
/// "Capped backoff on ANY listener error" (the doc contract below)
/// binds to the per-error-class census (R23′): connect-err,
/// listen-err, and recv-err all fall through to the ONE
/// [`Self::on_error`] arm — the three-class test battery pins each
/// to the same geometric-to-cap contract.
#[derive(Debug)]
struct ListenerBackoff {
    current: Duration,
    /// Error cycles since the last delivery — the recovery
    /// disclosure's input.
    degraded_cycles: u32,
}

impl ListenerBackoff {
    const FLOOR: Duration = Duration::from_secs(1);
    const CAP: Duration = Duration::from_secs(30);

    fn new() -> Self {
        Self {
            current: Self::FLOOR,
            degraded_cycles: 0,
        }
    }

    /// ANY listener error (connect-err / listen-err / recv-err): the
    /// delay to sleep NOW; the budget advances geometrically to the
    /// cap behind it.
    fn on_error(&mut self) -> Duration {
        let d = self.current;
        self.current = (self.current * 2).min(Self::CAP);
        self.degraded_cycles = self.degraded_cycles.saturating_add(1);
        d
    }

    /// Delivered work (a received notification): reset the budget.
    /// Returns the error-cycle count this delivery recovered from
    /// (0 = healthy steady state) for the warn-with-recovery
    /// disclosure.
    fn on_delivered(&mut self) -> u32 {
        let degraded = self.degraded_cycles;
        self.current = Self::FLOOR;
        self.degraded_cycles = 0;
        degraded
    }
}

impl Substituter {
    pub fn new(pool: PgPool, chunk_backend: Option<Arc<dyn ChunkBackend>>) -> Self {
        // `Client::new()` panics if `.build()` fails; `.build()` fails
        // in the nix sandbox (rustls-native-certs finds no CA bundle).
        // Use the builder + `.ok()` so sandbox tests degrade to no-op
        // instead of panicking. Tests that exercise HTTP inject a
        // working client via `.with_http_client()`.
        //
        // connect_timeout only — NO request-level body timeout.
        // ghc-binary (2.5GB compressed) legitimately exceeds any sane
        // fixed timeout. A hung mid-body upstream is bounded by the
        // owner-side per-read stall watchdog in fetch_nar
        // (`r[store.substitute.stall-abort]`, stall_window): time
        // BETWEEN bytes is bounded, total transfer time is not.
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
            nar_budget: crate::budget::NarBudget::new(DEFAULT_SUBSTITUTE_NAR_BUDGET),
            admission: None,
            stall_window: DEFAULT_SUBSTITUTE_STALL_WINDOW,
            nar_hold_floor_rate: NAR_HOLD_FLOOR_RATE,
            tenant_reservation_cap: TENANT_RESERVATION_CAP,
            claimed_by: std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".into()),
            raced_events: tokio::sync::broadcast::channel(1024).0,
            raced_listener_started: std::sync::atomic::AtomicBool::new(false),
            raced_park_fallback: RACED_PARK_FALLBACK_POLL,
            raced_park_budget: None,
            #[cfg(test)]
            hash_gate: None,
            #[cfg(test)]
            persist_gate: None,
        }
    }

    /// Override the raced-park knobs (`r[store.substitute.raced-
    /// subscribe]`): the fallback re-poll interval and (when `Some`)
    /// the total park budget — production derives the budget as
    /// `stall_window + 2 × fallback`. Builder-style, the R17
    /// violability lane: tests shrink the pair so the budget-exhaust
    /// red runs in milliseconds, or pin the budget to `ZERO` for the
    /// immediate-`Err(Raced)` observable the singleflight battery is
    /// stated in.
    pub fn with_raced_park(mut self, fallback: Duration, budget: Option<Duration>) -> Self {
        self.raced_park_fallback = fallback;
        self.raced_park_budget = budget;
        self
    }

    /// Test-only: install the hash gate (see [`HashGate`]).
    #[cfg(test)]
    pub(crate) fn with_hash_gate(mut self, gate: HashGate) -> Self {
        self.hash_gate = Some(gate);
        self
    }

    /// Test-only: install the persist gate (see the field doc) — the
    /// W8-B′ black-holed-persist seam.
    #[cfg(test)]
    pub(crate) fn with_persist_gate(mut self, gate: Arc<tokio::sync::Notify>) -> Self {
        self.persist_gate = Some(gate);
        self
    }

    /// Override the NAR-hold envelope's floor decompressed-throughput
    /// (`NAR_HOLD_FLOOR_RATE`, bytes/second). Builder-style — the
    /// R17 violability lane: tests shrink the envelope so the
    /// hold-deadline aborts are exercisable in seconds, and the
    /// violation red (`hold_envelope_floor_rate_binds`) proves the
    /// knob binds by flipping it.
    pub fn with_nar_hold_floor_rate(mut self, bytes_per_sec: u64) -> Self {
        self.nar_hold_floor_rate = bytes_per_sec;
        self
    }

    /// Override the per-tenant aggregate reservation cap
    /// (`TENANT_RESERVATION_CAP`). Builder-style — the R17
    /// violability lane: the violation red (`tenant_cap_binds`)
    /// proves the knob binds by flipping it.
    pub fn with_tenant_reservation_cap(mut self, cap: u64) -> Self {
        self.tenant_reservation_cap = cap;
        self
    }

    /// Override the owner-side download-stall window
    /// (`RIO_SUBSTITUTE_STALL_SECS`). Builder-style. main.rs threads
    /// `Config::substitute_stall` here; tests shrink it so the abort
    /// path is exercisable in seconds.
    pub fn with_stall_window(mut self, window: Duration) -> Self {
        self.stall_window = window;
        self
    }

    /// Enable `sig_mode = add|replace` signing. Builder-style.
    pub fn with_signer(mut self, signer: Arc<TenantSigner>) -> Self {
        self.signer = Some(signer);
        self
    }

    /// The signer, if configured — the materialization walk's
    /// local-visibility probe needs the cluster(+history) key entries
    /// for its trusted-set construction (bug_115), and the substituter
    /// is the executor's only line to the signing context.
    pub(crate) fn tenant_signer(&self) -> Option<&TenantSigner> {
        self.signer.as_deref()
    }

    /// Share the process-global NAR budget. Builder-style. main.rs
    /// wires `StoreServiceImpl::nar_budget()` here so PutPath and
    /// substitution draw from ONE sealed pool under ONE tenant
    /// ledger — the aggregate bound the budget exists to enforce
    /// (`NarBudget` clones share identity).
    pub fn with_nar_budget(mut self, budget: crate::budget::NarBudget) -> Self {
        self.nar_budget = budget;
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
    /// `info.references` here; closure completeness is the caller's
    /// responsibility (the materialization executor's own walk,
    /// `r[store.materialize.executor+5]`; the nix client for the
    /// gateway). Matches upstream
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

    // r[impl store.substitute.raced-subscribe]
    /// [`try_substitute_with_progress`](Self::try_substitute_with_progress)
    /// for the EXECUTOR plane (live_055(b)): a `Raced` answer parks on
    /// the placeholder-event subscription instead of bouncing back to
    /// the scheduler's RetryLater re-dispatch loop (the incident's 633
    /// re-claim round-trips at ~2/s against one blocked placeholder;
    /// post-reclaim completion was <200 ms).
    ///
    /// Structure (each clause load-bearing):
    /// - **the park holds NOTHING** — each attempt runs the ordinary
    ///   singleflight (admission permit acquired and dropped INSIDE
    ///   the leader future), so a parked waiter consumes no permit,
    ///   no budget, no claim, and no moka slot pinning a coalesced
    ///   gRPC caller (the unary `QueryPathInfo`/`GetPath` plane keeps
    ///   its fast-`Raced` → `NotFound` mapping untouched);
    /// - **register, THEN re-check** — the broadcast receiver
    ///   subscribes BEFORE `placeholder_blocked`'s row read, so a
    ///   release landing between the raced attempt and the park is
    ///   seen either by the re-check (loop immediately) or by the
    ///   already-registered receiver (NOTIFY at COMMIT) — the
    ///   lost-wakeup race is structurally dead;
    /// - **bounded fallback poll** ([`RACED_PARK_FALLBACK_POLL`]) —
    ///   NOTIFY is best-effort (listener reconnect gaps drop
    ///   messages); every fallback wake re-runs the FULL claim, whose
    ///   stall/zero-progress arms perform takeover, so a silently
    ///   wedged holder (no terminal event to announce) is still
    ///   reclaimed at most one interval past its takeover window;
    /// - **typed budget** — `stall_window + 2 × fallback` (derived;
    ///   override via [`Self::with_raced_park`]); past it the waiter
    ///   returns `Err(Raced)` and the RetryLater plane resumes as the
    ///   backstop, at per-budget cadence instead of per-poll.
    #[instrument(skip(self, progress), fields(tenant = %tenant_id, path = store_path))]
    pub async fn try_substitute_subscribed(
        &self,
        tenant_id: Uuid,
        store_path: &str,
        progress: &SubstProgressFn,
    ) -> Result<Option<ValidatedPathInfo>, SubstituteError> {
        let budget = self.raced_park_budget.unwrap_or(
            self.stall_window + RACED_PARK_BUDGET_FALLBACK_SLACK * self.raced_park_fallback,
        );
        let parked_since = Instant::now();
        loop {
            let r = self
                .try_substitute_inner(tenant_id, store_path, Some(progress))
                .await;
            if !matches!(r, Err(SubstituteError::Raced)) {
                return r;
            }
            let remaining = budget.saturating_sub(parked_since.elapsed());
            if remaining.is_zero() {
                metrics::counter!(
                    "rio_store_substitute_raced_parks_total",
                    "wake" => "budget_exhausted"
                )
                .increment(1);
                return r;
            }
            // The hash is the event key (announced hex-encoded by the
            // writers). Parse failures route through do_substitute's
            // own NarInfo arm — by the time we're Raced the path
            // parsed, so this expect is unreachable.
            let hash = StorePath::parse(store_path)
                .map_err(|e| SubstituteError::NarInfo(format!("bad store path: {e}")))?
                .sha256_digest();
            self.ensure_placeholder_listener();
            // Register BEFORE the re-check (the lost-wakeup kill).
            let mut rx = self.raced_events.subscribe();
            // A non-blocked answer re-attempts immediately; `Ok(true)`
            // AND a read error both park (the fail-safe direction: the
            // bounded fallback re-poll retries against a recovering PG
            // instead of hot-looping the claim path).
            if let Ok(false) = placeholder_blocked(&self.pool, &hash).await {
                // Released/completed/reaped between the raced attempt
                // and the park — re-attempt immediately.
                metrics::counter!(
                    "rio_store_substitute_raced_parks_total",
                    "wake" => "recheck"
                )
                .increment(1);
                continue;
            }
            let slice = self.raced_park_fallback.min(remaining);
            let notified = wait_for_placeholder_event(&mut rx, &hash, slice).await;
            metrics::counter!(
                "rio_store_substitute_raced_parks_total",
                "wake" => if notified { "notified" } else { "fallback" }
            )
            .increment(1);
        }
    }

    /// Spawn the process-wide PG LISTEN task at most once (lazy — the
    /// first raced park pays it; gRPC-only processes never do). The
    /// task LISTENs on [`metadata::PLACEHOLDER_EVENTS_CHANNEL`],
    /// decodes each hex payload, and fans it out on
    /// [`Self::raced_events`]. Reconnects with capped backoff on ANY
    /// listener error — where ANY is machine-backed (R23′, bug_172):
    /// the connect-err / listen-err / recv-err classes all fall
    /// through to the one [`ListenerBackoff::on_error`] arm, pinned
    /// per class by the `listener_backoff_*` test census; the budget
    /// resets only on DELIVERED work (the success-latch placement
    /// doctrine — pre-fix it reset on the LISTEN handshake and the
    /// cap never engaged under recv-failure). Notifications dropped
    /// during a gap are healed by the waiters' bounded fallback poll
    /// (the subscription is a latency optimization, never a
    /// correctness dependency).
    fn ensure_placeholder_listener(&self) {
        use std::sync::atomic::Ordering;
        if self
            .raced_listener_started
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        let pool = self.pool.clone();
        let tx = self.raced_events.clone();
        tokio::spawn(async move {
            // bug_172: the retry budget latches on DELIVERED work
            // (first recv), never on the LISTEN handshake — the
            // reset is only reachable from the delivery arm by
            // construction (ListenerBackoff::on_delivered).
            let mut backoff = ListenerBackoff::new();
            loop {
                match sqlx::postgres::PgListener::connect_with(&pool).await {
                    Ok(mut listener) => {
                        if let Err(e) = listener.listen(metadata::PLACEHOLDER_EVENTS_CHANNEL).await
                        {
                            warn!(
                                error = %e,
                                "placeholder listener: LISTEN failed; reconnecting \
                                 with capped backoff (parked waiters heal via \
                                 fallback poll)"
                            );
                        } else {
                            loop {
                                match listener.recv().await {
                                    Ok(n) => {
                                        let recovered = backoff.on_delivered();
                                        if recovered > 0 {
                                            info!(
                                                degraded_cycles = recovered,
                                                "placeholder listener: subscription \
                                                 delivering again (recovered)"
                                            );
                                        }
                                        if let Ok(hash) = hex::decode(n.payload()) {
                                            // Send fails only with zero
                                            // receivers — nobody parked,
                                            // nothing to wake.
                                            let _ = tx.send(hash);
                                        }
                                    }
                                    Err(e) => {
                                        warn!(
                                            error = %e,
                                            "placeholder listener: recv failed; reconnecting \
                                             with capped backoff (parked waiters heal via \
                                             fallback poll)",
                                        );
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            "placeholder listener: connect failed; reconnecting \
                             with capped backoff"
                        );
                    }
                }
                tokio::time::sleep(backoff.on_error()).await;
            }
        });
    }

    /// The shared capability gate both substitution legs consult
    /// BEFORE any upstream work (merged_bug_030). The ordering law
    /// lives HERE, exactly once: a tenant with NO upstreams is a clean
    /// no-op on both legs (nothing to consult — "no upstream has it"
    /// is truthful), checked FIRST; a missing HTTP client with
    /// upstreams configured is a capability fault on both legs (the
    /// work cannot run, so nothing may be confirmed — hazard eeee),
    /// checked SECOND. Pre-fix the two legs ordered the checks
    /// oppositely: `try_substitute_inner` was upstreams-first while
    /// `check_available` was client-first, so an upstream-less tenant
    /// on a clientless replica answered CleanMiss on the attempt leg
    /// but all-indeterminate on the probe leg → `Failed{Fetch}` →
    /// ChargeInfra — infra-charging and parking jobs that should
    /// settle confirmed-miss (and converting every status='ready'
    /// materialization job into a park-charging one replica-wide,
    /// since dispatch treats indeterminate optimistically while
    /// every claim can only err).
    async fn capability_gate(
        &self,
        tenant_id: Uuid,
    ) -> Result<CapabilityGate<'_>, SubstituteError> {
        let upstreams = metadata::upstreams::list_for_tenant(&self.pool, tenant_id).await?;
        if upstreams.is_empty() {
            return Ok(CapabilityGate::NoUpstreams);
        }
        match &self.http {
            Some(http) => Ok(CapabilityGate::Ready { http, upstreams }),
            None => Ok(CapabilityGate::Clientless),
        }
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
        let singleflight_start = std::time::Instant::now();
        let was_leader = std::sync::atomic::AtomicBool::new(false);
        let cached = self
            .inflight
            .try_get_with(key, async {
                was_leader.store(true, std::sync::atomic::Ordering::Relaxed);
                // merged_bug_016: EVERY `result` tick happens inside
                // THIS once-per-leader future — the machine witness
                // for "exactly one result tick per singleflight
                // leader" is structural (waiter-side accounting is
                // unrepresentable: the only counter sites are the
                // post-loop fold and the leader-exit arm below; the
                // waiter map_err carries no metrics at all).
                // Box::pin: depth firewall — stable rustc's layout-query
                // depth caps at this batch/leader boundary instead of
                // compounding the substitute-pipeline nesting into every
                // caller's async layout (one boxed allocation per
                // singleflight leader).
                let r = Box::pin(self.substitute_leader(tenant_id, store_path, progress)).await;
                if let Err(e) = &r {
                    // The leader-exit tick for pre-loop/SETUP escapes
                    // (StorePath::parse → NarInfo; capability-gate PG
                    // failure → Ingest). The skip list mirrors the
                    // fold's own-label and pre-counted classes:
                    // transients (Raced/RateLimited/Admission/
                    // Stalled) have their own signals; Fetch is
                    // either the fold's Errored verdict (counted AT
                    // the fold) or the Clientless capability fault
                    // (counted under skipped_total before it
                    // returns); UntrustedPresent/ContentMismatch
                    // carry their own fold labels.
                    if !matches!(
                        e,
                        SubstituteError::Raced
                            | SubstituteError::RateLimited { .. }
                            | SubstituteError::Admission(_)
                            | SubstituteError::Stalled { .. }
                            | SubstituteError::Fetch(_)
                            | SubstituteError::UntrustedPresent
                            | SubstituteError::ContentMismatch
                    ) {
                        metrics::counter!(
                            "rio_store_substitute_total",
                            "result" => "error",
                            "tenant" => tenant_id.to_string()
                        )
                        .increment(1);
                    }
                }
                r
            })
            .await
            // merged_bug_016: NO metrics here — this closure runs once
            // per COALESCED CALLER (moka hands every waiter the same
            // Arc<SubstituteError>), which multiplied the error tick
            // by the coalescing width and mislabeled local-DB trouble
            // as upstream error exactly when coalescing and operator
            // attention peaked. The unwrap is the only job left.
            .map_err(|e: Arc<SubstituteError>| (*e).clone())?;
        let elapsed = singleflight_start.elapsed();
        if elapsed > std::time::Duration::from_secs(5) {
            tracing::warn!(
                store_path,
                tenant_id = %tenant_id,
                was_leader = was_leader.load(std::sync::atomic::Ordering::Relaxed),
                elapsed = ?elapsed,
                "try_substitute: slow singleflight (>5s; was_leader=false means waited on another caller)"
            );
        }
        Ok(cached.map(|arc| (*arc).clone()))
    }

    /// The once-per-leader body of `try_substitute`'s singleflight
    /// (merged_bug_016): capability gate, admission permit, and the
    /// upstream loop. Extracted so the leader-exit metric arm in the
    /// init future observes EVERY escape — pre-loop and loop alike —
    /// in leader context.
    async fn substitute_leader(
        &self,
        tenant_id: Uuid,
        store_path: &str,
        progress: Option<&SubstProgressFn>,
    ) -> Result<Option<Arc<ValidatedPathInfo>>, SubstituteError> {
        // Capability gate BEFORE the admission permit
        // (merged_bug_030: ONE ordering for both legs —
        // upstreams first, client second). A tenant with no
        // upstreams (the common case) must get an immediate
        // `Ok(None)`, not queue behind saturated substituters
        // for up to SUBSTITUTE_ADMISSION_WAIT —
        // `list_for_tenant` is one indexed PG read, far
        // cheaper than the permit's potential 25 s queue.
        let (http, upstreams) = match self.capability_gate(tenant_id).await? {
            CapabilityGate::NoUpstreams => {
                // Correct behaviour for a tenant with no
                // upstreams configured — but it MUST be
                // countable. Every skip in the substitution
                // pipeline degrades to "build it from source"
                // at the scheduler, and a skip that leaves no
                // trace is indistinguishable from "the
                // upstream really doesn't have it"
                // (2026-05-23: hours of builder CPU compiling
                // cache.nixos.org-cached paths because every
                // no-op branch was silent). debug! not warn! —
                // an upstream-less tenant hits this on every
                // cache miss; the counter is the alertable
                // signal. Counted once per singleflight
                // leader, same granularity as
                // `result=hit|miss`.
                debug!(
                    tenant = %tenant_id,
                    path = store_path,
                    "substitute skipped: tenant has no upstreams configured"
                );
                metrics::counter!(
                    "rio_store_substitute_skipped_total",
                    "reason" => "no_upstreams"
                )
                .increment(1);
                return Ok(None);
            }
            CapabilityGate::Clientless => {
                // Upstreams ARE configured but the reqwest
                // client failed to build at startup: the walk
                // cannot run, so it cannot confirm ANYTHING —
                // least of all a miss. `Ok(None)` here
                // laundered the capability fault into a
                // definitive NotFound — the merged_bug_044
                // class at the capability chokepoint. Err is
                // UNCACHED (same law as the all-errored fold:
                // nothing was consulted, nothing may be
                // remembered). The construction site warned
                // once; the counter stays the alertable
                // signal.
                debug!(
                    tenant = %tenant_id,
                    path = store_path,
                    "substitute failed: no HTTP client (reqwest client build failed at startup)"
                );
                metrics::counter!(
                    "rio_store_substitute_skipped_total",
                    "reason" => "no_http_client"
                )
                .increment(1);
                return Err(SubstituteError::Fetch(
                    "upstream substitution unavailable: HTTP client failed to build at startup"
                        .to_string(),
                ));
            }
            CapabilityGate::Ready { http, upstreams } => (http, upstreams),
        };
        // r[impl store.substitute.admission+2]
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
        // Evidence cells for the post-loop fold (bug_081 +
        // merged_bug_044): every `Err` arm routes through the kernel's
        // ONE `record` chokepoint, so a continue that loses failure
        // evidence is no longer writable here. 429s and stalls keep
        // their max-across-upstreams semantics (a tenant with
        // [rate-limited-A, healthy-B] hits B; a stall on A fails over
        // with its strike already durably recorded); generic errors
        // set the error cell so an all-errored iteration can never
        // fold to a cacheable clean miss.
        let mut cells = rio_evidence_kernel::outcome::SubstituteLoopCells::new();
        for upstream in &upstreams {
            match self
                .try_upstream(http, tenant_id, upstream, store_path, &hash_part, progress)
                .await
            {
                Ok(UpstreamOutcome::Hit {
                    info,
                    ingested_bytes,
                }) => {
                    let elapsed = start.elapsed().as_secs_f64();
                    metrics::histogram!("rio_store_substitute_duration_seconds").record(elapsed);
                    // The hit IS the attempt verdict (the loop exits):
                    // per-leader by construction, like every result
                    // label below (merged_bug_091).
                    metrics::counter!(
                        "rio_store_substitute_total",
                        "result" => "hit",
                        "tenant" => tenant_label
                    )
                    .increment(1);
                    // Bytes INGESTED — the producer's own statement:
                    // nar_size on a real download+persist, 0 on an
                    // AlreadyComplete dedup hit (merged_bug_091: the
                    // pre-fix increment added nar_size on every Hit,
                    // so dedup hits inflated \"bytes ingested\" by
                    // bytes that were never written).
                    metrics::counter!("rio_store_substitute_bytes_total").increment(ingested_bytes);
                    return Ok(Some(*info));
                }
                Ok(UpstreamOutcome::Miss) => {
                    // This upstream doesn't have it. Try the next.
                    // merged_bug_091: NO counter here — `result=miss`
                    // is the ATTEMPT verdict, emitted once per leader
                    // at the CleanMiss fold below; the per-upstream
                    // detail is this debug! line.
                    debug!(upstream = %upstream.url, "upstream miss, trying next");
                }
                Ok(UpstreamOutcome::UntrustedPresent) => {
                    // merged_bug_005: present-but-untrusted — record
                    // the trust-refusal cell and FAIL OVER (another
                    // upstream may hold a verifiable copy). The fold
                    // below surfaces the refusal only if nothing
                    // serves and nothing outranks it — and emits the
                    // attempt-level `result=untrusted` there
                    // (merged_bug_091).
                    match cells.record(
                        rio_evidence_kernel::outcome::SubstituteFailureClass::Untrusted,
                        None,
                    ) {
                        rio_evidence_kernel::outcome::LoopControl::AbortRaced => {
                            unreachable!("Untrusted records Continue")
                        }
                        rio_evidence_kernel::outcome::LoopControl::Continue => {}
                    }
                }
                Ok(UpstreamOutcome::ContentMismatch) => {
                    // merged_bug_046: stored-row disagreement — record
                    // the content-refusal cell and FAIL OVER (another
                    // upstream may carry agreeing bytes). The fold
                    // below surfaces the refusal only if nothing
                    // serves and nothing outranks it.
                    match cells.record(
                        rio_evidence_kernel::outcome::SubstituteFailureClass::ContentMismatch,
                        None,
                    ) {
                        rio_evidence_kernel::outcome::LoopControl::AbortRaced => {
                            unreachable!("ContentMismatch records Continue")
                        }
                        rio_evidence_kernel::outcome::LoopControl::Continue => {}
                    }
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
                Err(e) => {
                    // This upstream failed. Class-keyed logging and
                    // metrics are cosmetic; the EVIDENCE goes through
                    // the kernel cells' one `record` chokepoint below,
                    // so this arm cannot continue while losing the
                    // failure (merged_bug_044 — the pre-fix catch-all
                    // recorded nothing, making an all-errored
                    // iteration fold to a CACHED clean miss).
                    match &e {
                        SubstituteError::Stalled { .. } => {
                            // r[impl store.substitute.stall-abort+2]
                            // Owner-side stall abort: the claim was
                            // released in place with the strike durably
                            // recorded — the stall is THIS upstream's
                            // failure, so fail over (bug_081). The fold
                            // surfaces `Stalled` only if nothing serves.
                            warn!(upstream = %upstream.url, "download stalled, trying next");
                        }
                        SubstituteError::RateLimited { retry_after } => {
                            // 429 → fail over; the fold returns
                            // `RateLimited` (uncached) only if no other
                            // upstream had it
                            // (r[store.substitute.probe-429-retry+3]).
                            debug!(upstream = %upstream.url, ?retry_after,
                                   "upstream 429, trying next");
                        }
                        other => {
                            // Down / hash mismatch / parse error. The
                            // integrity metric is emitted HERE (where
                            // the error is observable) — per-upstream
                            // errors never reach `grpc/mod.rs`.
                            // bug_240: class membership by CALLING the
                            // canonical table, never by re-enumerating
                            // variants — the pre-fix
                            // `HashMismatch | SizeMismatch` matches!
                            // silently excluded TooLarge (the
                            // declared-NarSize over-cap lie, the
                            // decompression bomb, oversized narinfo
                            // bodies: the most clearly hostile events
                            // this counter exists for).
                            if substitute_error_evidence(other).0
                                == rio_evidence_kernel::outcome::SubstituteFailureClass::Integrity
                            {
                                metrics::counter!(
                                    "rio_store_substitute_integrity_failures_total",
                                    "tenant" => tenant_label.clone()
                                )
                                .increment(1);
                            }
                            // merged_bug_091: NO result counter here
                            // — `result=error` is the attempt verdict,
                            // emitted once per leader at the Errored
                            // fold below. The integrity counter above
                            // stays per-occurrence (security signal).
                            warn!(upstream = %upstream.url, error = %other,
                                  "upstream fetch failed, trying next");
                        }
                    }
                    let (class, advice) = substitute_error_evidence(&e);
                    // D6 trigger observability (RULED non-goal-unless,
                    // bughunt-9): the poisoned-pool retry-once rider
                    // ships only if ChargeInfra SOURCE instrumentation
                    // shows production hits — this counter makes that
                    // trigger measurable. The charged-or-not split is
                    // the kernel's own total classification table
                    // (classify_substitute_failure — rustc-exhaustive,
                    // never an author-typed subset here); the label
                    // value is derived from the class alphabet's Debug
                    // repr (machine-derived, total).
                    if matches!(
                        rio_evidence_kernel::outcome::classify_substitute_failure(class),
                        rio_evidence_kernel::outcome::FailureDisposition::ChargeInfra
                    ) {
                        metrics::counter!(
                            "rio_store_substitute_infra_charge_total",
                            "class" => format!("{class:?}").to_lowercase()
                        )
                        .increment(1);
                    }
                    match cells.record(class, advice) {
                        rio_evidence_kernel::outcome::LoopControl::AbortRaced => {
                            return Err(SubstituteError::Raced);
                        }
                        rio_evidence_kernel::outcome::LoopControl::Continue => {}
                    }
                }
            }
        }

        // No upstream had it. The pure post-loop fold picks the
        // attempt outcome from the recorded evidence: a stall
        // dominates (its strike must reach the executor's
        // classification), then 429 (uncached so a retry re-asks),
        // then `Errored` (≥1 upstream broke — surfaced as an UNCACHED
        // `Fetch` error so the next attempt re-asks instead of
        // trusting a poisoned miss), then the cacheable clean miss —
        // only when every upstream answered hit-or-404.
        // merged_bug_091: the attempt-level `result` labels are
        // emitted HERE, from the fold verdict — one tick per
        // singleflight leader by construction, which is what the HELP
        // has always claimed. Stalled/RateLimited stay uncounted in
        // this family (typed transients with their own signals), as
        // before.
        // bug_085: ONE driver applies the per-variant effects record —
        // tick if labeled, evict if the verdict contradicts a cached
        // HEAD positive, return the outcome. The decisions live in
        // `verdict_effects`' exhaustive constructor, so a new kernel
        // verdict cannot ship without explicitly deciding its
        // cache-eviction bit (the ContentMismatch arm shipped without
        // one when the effects were copy-paste statements per arm).
        let effects = verdict_effects(
            rio_evidence_kernel::outcome::fold_substitute_loop(cells),
            store_path,
        );
        if let Some(result) = effects.result_label {
            metrics::counter!(
                "rio_store_substitute_total",
                "result" => result,
                "tenant" => tenant_id.to_string()
            )
            .increment(1);
        }
        if effects.evict_probe_positive {
            // merged_bug_016/263/bug_085: the verdict contradicts any
            // cached HEAD positive for this (tenant, path) — evict it,
            // so a charge-gating confirmation probe observes live
            // state instead of a stale `true` for the rest of the 1h
            // TTL. Aggregate granularity is load-bearing: the eviction
            // rides the FOLD verdict (every upstream answered), never
            // one upstream's opinion.
            self.probe_cache
                .invalidate(&(tenant_id, store_path.to_string()))
                .await;
        }
        effects.outcome.map(|()| None)
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
        // r[impl store.substitute.probe-429-retry+3]
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
        // merged_bug_005: a present-but-unverifiable narinfo is a
        // typed TRUST REFUSAL, never a miss — folding it into the
        // cacheable miss lane sent the sig-blind HEAD confirmation
        // chasing a "present but not ingested" charge forever on a
        // rotated/mistyped trusted_keys entry.
        let Some(trusted_key) = ni.verify_sig(&upstream.trusted_keys) else {
            warn!(
                upstream = %upstream.url,
                path = store_path,
                "narinfo signature did not verify against upstream.trusted_keys"
            );
            return Ok(UpstreamOutcome::UntrustedPresent);
        };
        debug!(upstream = %upstream.url, trusted_key, "narinfo signature verified");

        // Parse the nar_hash into raw bytes for the ingest path. The
        // narinfo text has `sha256:nixbase32`; we need `[u8; 32]` for
        // ValidatedPathInfo + the post-decompress hash check.
        let expected_hash = parse_nar_hash(&ni.nar_hash)?;

        // r[impl store.substitute.untrusted-upstream+3]
        // r[impl store.put.nar-bytes-budget+6]
        // Declared-size gate. `trusted_keys` is also tenant-supplied so
        // a verified sig is NOT a trust boundary; gate before download.
        // `>=` (PutPath parity, store.put.nar-bytes-budget+6): an
        // exactly-MAX_NAR_SIZE NAR was never cluster-uploadable (PutPath
        // rejects `>=`), and the rejection makes the whole-NAR budget
        // reservation u32-expressible (max reservation 2^32 − 1).
        // `fetch_nar`'s declared+1 read cap catches a narinfo that lies
        // about `NarSize` DURING the read.
        if ni.nar_size >= MAX_NAR_SIZE {
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
        let refs_str: Vec<String> = info.references.iter().map(ToString::to_string).collect();

        // r[impl store.substitute.stale-reclaim+3]
        // Substitution claims carry the stall params: the verified
        // narinfo's NarSize scopes the takeover predicate to
        // mid-download claims (persist phase exempt), and the window
        // is the same one the owner-side abort applies.
        let stall_params = ingest::SubstituteClaimParams {
            nar_size: ni.nar_size,
            stall_window: self.stall_window,
            claimed_by: &self.claimed_by,
        };
        let claim = match ingest::claim_placeholder(
            &self.pool,
            &store_path_hash,
            info.store_path.as_str(),
            &refs_str,
            SUBSTITUTE_HOOKS,
            Some(&stall_params),
        )
        .await?
        {
            PlaceholderClaim::Owned(claim) => claim,
            PlaceholderClaim::AlreadyComplete => {
                // Lost the race; winner completed. NO download.
                let stored = metadata::query_path_info(&self.pool, store_path)
                    .await?
                    .ok_or_else(|| {
                        SubstituteError::Ingest(
                            "claim AlreadyComplete but query_path_info miss".into(),
                        )
                    })?;
                // r[impl store.substitute.content-binding]
                // CONTENT BINDING (merged_bug_114): the narinfo that
                // got us here names the path and verifies against
                // tenant-supplied `trusted_keys` — which is NOT a
                // trust boundary (see the size-gate comment above) —
                // and this arm runs BEFORE any body fetch. Before the
                // STORED row may be returned as THIS upstream's Hit
                // (or this upstream's sigs appended over it), the
                // upstream's claim must AGREE with the stored content:
                // nar_hash, nar_size, and the reference set. On
                // disagreement this upstream does not have *this*
                // path's bytes — Miss for THIS upstream, no sig
                // append; the winner's row stays untouched. (Pre-fix,
                // a tenant whose upstream self-signed a fabricated
                // narinfo naming a victim path got the stored bytes
                // back as a Hit plus persisted upstream signatures
                // whose fingerprint cannot match the stored row.)
                let stored_refs: std::collections::BTreeSet<String> =
                    stored.references.iter().map(ToString::to_string).collect();
                let claimed_refs: std::collections::BTreeSet<String> =
                    info.references.iter().map(ToString::to_string).collect();
                // The agreement decision is the kernel's ONE body
                // (axis totality kani-proven there — a dropped axis
                // flips the proof red, not just the unit test).
                if !rio_evidence_kernel::content::already_complete_agrees(
                    &rio_evidence_kernel::content::ContentFacts {
                        nar_hash: stored.nar_hash,
                        nar_size: stored.nar_size,
                        references: stored_refs,
                    },
                    &rio_evidence_kernel::content::ContentFacts {
                        nar_hash: expected_hash,
                        nar_size: ni.nar_size,
                        references: claimed_refs,
                    },
                ) {
                    warn!(
                        upstream = %upstream.url,
                        path = store_path,
                        stored_nar_size = stored.nar_size,
                        claimed_nar_size = ni.nar_size,
                        "AlreadyComplete content mismatch: upstream narinfo \
                         claims different bytes than the stored row — typed \
                         content refusal for this upstream (no signature \
                         appended; merged_bug_046: never a miss, so the \
                         content-blind HEAD confirmation cannot contradict \
                         a cached CleanMiss into a per-retry charge)"
                    );
                    metrics::counter!(
                        "rio_store_substitute_integrity_failures_total",
                        "tenant" => tenant_id.to_string()
                    )
                    .increment(1);
                    return Ok(UpstreamOutcome::ContentMismatch);
                }
                // Compute sigs over the STORED row (its `nar_size` is
                // what was actually ingested — now PROVEN equal to the
                // upstream's claim), append (idempotent —
                // append_signatures dedupes), return it.
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
                return Ok(UpstreamOutcome::Hit {
                    info: Box::new(stored),
                    ingested_bytes: 0,
                });
            }
            PlaceholderClaim::Concurrent => {
                // Another replica (or this replica via a different
                // closure-walk) holds the placeholder. NO download,
                // and `do_substitute` stops the upstream loop.
                debug!(%store_path, "concurrent uploader holds placeholder");
                return Ok(UpstreamOutcome::Raced);
            }
        };

        // Owner attribution at claim time: the one log line that ties
        // (path, claim, size, pod) together for stall/takeover triage.
        info!(
            %store_path,
            claim_id = %claim,
            nar_size = ni.nar_size,
            pod = %self.claimed_by,
            "substitute: claimed placeholder"
        );

        // r[impl store.put.drop-cleanup+2]
        // We OWN the placeholder. Guard against future-drop (client
        // RST_STREAM mid-fetch) — the guard's spawn reaps it if any
        // path between here and the defuse below is abandoned.
        //
        // r[impl store.substitute.progress-heartbeat]
        // The progress handle: fetch_nar's read loop advances it with
        // the decompressed-byte count; the guard's heartbeat carries
        // it to `manifests.fetched_bytes`/`last_progress_at` so
        // competing claimants can discriminate stuck ≠ slow.
        let progress_handle = Arc::new(ingest::ProgressHandle::new());
        let placeholder_guard = ingest::spawn_placeholder_guard(
            self.pool.clone(),
            store_path_hash.to_vec(),
            claim,
            Some(Arc::clone(&progress_handle)),
        );

        // The remaining steps are fallible AND we own the placeholder;
        // funnel through one async block so a single error arm handles
        // explicit abort (the drop-guard is for the implicit drop path).
        let persist = async {
            // — Step 4: whole-NAR budget reservation, then GET —
            // r[impl store.put.nar-bytes-budget+6]
            // Single-shot reservation AFTER the claim arms (so
            // AlreadyComplete/Raced cost zero budget) and BEFORE the
            // NAR GET (so a budget park never rides an open upstream
            // socket). The verified narinfo's NarSize is in hand —
            // the narinfo read precedes the body fetch
            // (merged_bug_195) — and `>= MAX_NAR_SIZE` was rejected
            // above, so the charge is u32-expressible and below every
            // validate()-passing budget's floor.
            // The typed hold envelope (merged_bug_021): demanded at
            // the constructor — an unenveloped reservation does not
            // typecheck — derived from the declared size at the floor
            // rate, armed at permit grant (the zero-holding park does
            // not consume hold time).
            let envelope = NarHoldEnvelope::for_declared(
                ni.nar_size,
                self.stall_window,
                self.nar_hold_floor_rate,
            );
            let reservation = NarBudgetReservation::reserve(
                &self.nar_budget,
                tenant_id,
                self.tenant_reservation_cap,
                ni.nar_size,
                envelope,
                Some(&progress_handle),
            )
            .await?;
            let nar_url = format!("{base}/{}", ni.url);
            let (nar_bytes, reservation) = self
                .fetch_nar(
                    http,
                    tenant_id,
                    &nar_url,
                    &ni.compression,
                    ni.nar_size,
                    base,
                    progress,
                    Some(&progress_handle),
                    reservation,
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

            // r[impl store.put.nar-hold-envelope+2]
            // — Post-read tail, ENFORCED under the hold envelope —
            // The reservation provably spans hash → sigs → persist.
            // LAYERING (D5 superseded the Q-108 caller-duty
            // doctrine): black-holed S3 ops now fail typed at the
            // SEAM first (per-op-class attempt timeouts in
            // backend.rs; the chunk-plane ladder worst-cases ~3 min,
            // const-pinned inside this envelope's 15-min grace), and
            // this `timeout(envelope.remaining())` remains the
            // HOLD-law backstop — the budget axis's own clock, which
            // Σ-of-bounded-ops can never replace (slow-but-alive ops
            // can sum past the hold deadline; holders expire by THIS
            // clock so parked waiters get capacity). Expiry returns
            // the typed `HoldDeadlineExceeded`, releasing the
            // reservation by drop (the drop-guard above owns
            // placeholder cleanup on the implicit-drop path;
            // `put_chunked`'s internal rollback contract is
            // unchanged). The one remaining NAMED premise is the hash
            // compute bound (~10 s / 4 GiB): the `spawn_blocking`
            // digest task is non-cancellable, so on timeout its
            // completion still drops the moved reservation — that lag
            // is PRICED slack on top of the enforced deadline, never
            // a replacement for it (W-6a's credit-back-at-free
            // property is preserved).
            let tail_envelope = *reservation.envelope();
            let tail = async {
                // Hash-check the decompressed NAR against the
                // narinfo's claim — off the async runtime (4 GiB ≈
                // 8-10s of pure compute would otherwise stall a tokio
                // worker). `Bytes` is cheap-clone; move it in and back
                // out alongside the digest.
                //
                // r[impl store.put.nar-bytes-budget+6]
                // §3.2 step 4 (live_047/R-C): the RESERVATION rides
                // the bytes through the detach. tokio never cancels
                // blocking tasks, so dropping this future at the
                // JoinHandle await leaves the digest task owning the
                // full buffer — were the reservation parked in this
                // frame, cancellation would credit the budget back
                // while the bytes remain resident. Moving it into the
                // closure ties credit-back to the moment the memory
                // actually frees (the task's output drops). W-6a
                // asserts this.
                #[cfg(test)]
                let hash_gate = self.hash_gate.clone();
                // `_reservation`: re-bound here and held to the end of
                // this block — the budget is credited back only after
                // `persist_nar` returns (today's permit lifetime).
                let (nar_bytes, _reservation, got_hash) = tokio::task::spawn_blocking(
                    move || -> (Bytes, NarBudgetReservation, [u8; 32]) {
                        #[cfg(test)]
                        if let Some(g) = &hash_gate {
                            g.hold();
                        }
                        let h = sha2::Sha256::digest(&nar_bytes).into();
                        (nar_bytes, reservation, h)
                    },
                )
                .await
                .map_err(|e| SubstituteError::Ingest(format!("hash task join: {e}")))?;
                if got_hash != expected_hash {
                    return Err(SubstituteError::HashMismatch {
                        expected: hex::encode(expected_hash),
                        got: hex::encode(got_hash),
                    });
                }

                // Size + hash verified — `info.nar_size` (set in
                // `narinfo_to_validated` from `ni.nar_size`) now
                // provably equals what gets persisted. Compute sigs
                // over the verified `info` so stored `(nar_size,
                // signatures)` are mutually consistent.
                info.signatures = self
                    .sigs_for_mode(tenant_id, upstream.sig_mode, &ni, &info)
                    .await;

                // — Step 5-6: persist via the shared write-ahead core —
                // merged_bug_003: the NAR is fully fetched; the persist
                // can legitimately exceed the stall window (S3
                // multipart, chunk dedup) with fetched_bytes frozen at
                // nar_size — exempt AS DATA from the TAKEOVER
                // predicate, not from the hold envelope: the budget
                // law's deadline (hours at the floor rate) still
                // bounds this span.
                progress_handle.set_phase(ingest::ClaimPhase::Persisting);
                #[cfg(test)]
                if let Some(g) = &self.persist_gate {
                    // The W8-B′ seam: a black-holed persist backend.
                    g.notified().await;
                }
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
            };
            match tokio::time::timeout(tail_envelope.remaining(), tail).await {
                Ok(r) => r,
                Err(_) => Err(SubstituteError::HoldDeadlineExceeded {
                    declared: ni.nar_size,
                    hold_budget: tail_envelope.hold_budget(),
                }),
            }
        }
        .await;

        match persist {
            Ok(()) => {
                placeholder_guard.defuse();
                let ingested_bytes = info.nar_size;
                Ok(UpstreamOutcome::Hit {
                    info: Box::new(info),
                    ingested_bytes,
                })
            }
            Err(e) => {
                placeholder_guard.defuse();
                // bug_108: the posture is derived from the TYPE
                // ([`cleanup_posture`], rustc-exhaustive), never
                // re-decided positionally here — pre-fix this fold
                // matched `Stalled` by name and its generic arm
                // silently absorbed `HoldDeadlineExceeded` (row
                // deleted, strike lost, metric unfired).
                match cleanup_posture(&e) {
                    // r[impl store.substitute.stall-abort+2]
                    // Owner-side stall-family abort: release the claim
                    // IN PLACE — claim cleared, progress NULLed,
                    // durable stall_count incremented — instead of
                    // deleting the row, so the next attempt re-claims
                    // immediately and the stall evidence survives.
                    // Claim-guarded: a competing stall-reclaim that
                    // already took the row over wins, and this release
                    // matches zero rows — one stall event, exactly one
                    // strike.
                    CleanupPosture::StrikeReleaseInPlace => {
                        match metadata::release_placeholder_in_place(
                            &self.pool,
                            &store_path_hash,
                            claim,
                        )
                        .await
                        {
                            Ok(released) => {
                                warn!(
                                    %store_path,
                                    claim_id = %claim,
                                    released,
                                    error = %e,
                                    "substitute: stall-family abort — claim released in place"
                                );
                                if released {
                                    metrics::counter!(
                                        SUBSTITUTE_HOOKS.stale_reclaimed_metric,
                                        "reason" => crate::ingest::STALE_RECLAIM_STALL_ABORT
                                    )
                                    .increment(1);
                                }
                            }
                            Err(release_err) => {
                                // The release failed (DB error): fall back to
                                // the claim-gated delete so the row cannot wedge
                                // the path until heartbeat death. Strike lost —
                                // availability over evidence.
                                warn!(%store_path, error = %release_err,
                                    "substitute: stall release failed; falling back to abort");
                                ingest::abort_placeholder(&self.pool, &store_path_hash, claim)
                                    .await;
                            }
                        }
                    }
                    CleanupPosture::Abort => {
                        // Abort synchronously so the next upstream in
                        // `do_substitute`'s loop sees a clean slate
                        // (the guard's tokio::spawn fires too late for
                        // that). threshold=None: our placeholder.
                        ingest::abort_placeholder(&self.pool, &store_path_hash, claim).await;
                    }
                }
                Err(e)
            }
        }
    }

    /// GET the NAR body and decompress. Takes the caller's whole-NAR
    /// [`NarBudgetReservation`] (acquired BEFORE the GET — the budget
    /// park never rides an open upstream socket) and returns it with
    /// the raw NAR bytes; the caller carries the reservation through
    /// the hash detach and holds it until after `persist_nar`.
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
        progress_handle: Option<&ingest::ProgressHandle>,
        reservation: NarBudgetReservation,
    ) -> Result<(Bytes, NarBudgetReservation), SubstituteError> {
        // r[impl store.substitute.stall-abort+2]
        // Owner-side stall watchdog: the NAR GET deliberately has no
        // request-level timeout (a multi-GB body legitimately runs
        // long), so the only abort clock is THIS one — no response
        // headers, or no body bytes from one read to the next, for
        // `stall_window`. The budget reservation happened BEFORE this
        // function (outside the watchdog): blocking on the local
        // NAR-bytes semaphore is backpressure, not an upstream stall,
        // and must never accrue a strike.
        let stall = self.stall_window;
        let stalled = || SubstituteError::Stalled { window: stall };
        let resp = tokio::time::timeout(stall, http.get(nar_url).send())
            .await
            .map_err(|_| stalled())?
            .map_err(|e| SubstituteError::Fetch(format!("{nar_url}: {e}")))?;
        // r[impl store.substitute.probe-429-retry+3]
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
        // capped read loop. The `.take()` wraps the DECOMPRESSED side
        // so a zstd bomb is bounded regardless of what `NarSize`
        // claimed. The cap tightened from the global MAX to
        // `declared` (live_047/R-C): the held reservation covers
        // exactly the declared size, so a stream running past its
        // declaration fails DURING the read and the leg's buffered
        // bytes stay ≤ reservation by construction. (The
        // `SUBSTITUTE_NAR_DECOMPRESSED_CAP` min keeps the test-cap
        // regime expressible; in production it equals MAX_NAR_SIZE
        // and `declared < MAX_NAR_SIZE` always wins the min.)
        use futures_util::TryStreamExt;
        use tokio::io::AsyncRead;
        use tokio_util::io::StreamReader;
        let stream = resp
            .bytes_stream()
            .map_err(|e| std::io::Error::other(format!("NAR stream: {e}")));
        let reader = StreamReader::new(stream);

        let declared = expected_nar_size;
        let cap = declared.min(SUBSTITUTE_NAR_DECOMPRESSED_CAP);
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

        let out = read_nar_capped(
            &mut capped,
            declared,
            cap,
            stall,
            nar_url,
            progress,
            progress_handle,
            upstream_base,
            &reservation,
        )
        .await?;
        // Final tick so a sub-MiB path (or the trailing partial MiB)
        // still reports done==expected before the terminal PathInfo.
        if let Some(cb) = progress {
            cb(out.len() as u64, expected_nar_size, upstream_base);
        }
        Ok((Bytes::from(out), reservation))
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
    /// `r[store.substitute.probe-429-retry+3]`. No batch-size truncation:
    /// the originating RPC's wall-clock is `⌈N_uncached/128⌉ × RTT`;
    /// the scheduler's merge-time caller carries a wider timeout
    /// (`r[sched.substitute.eager-probe]`).
    ///
    /// `deadline` bounds the 429-retry sleep: if the upstream's
    /// `Retry-After` would push past it (with 2s headroom for the
    /// HEADs themselves), the retry pass is skipped and the
    /// rate-limited paths are returned in `indeterminate` (uncached;
    /// the scheduler optimistically tries the substitute fetch and
    /// falls through to build only on confirmed miss).
    #[instrument(skip(self, paths), fields(tenant = %tenant_id, n = paths.len()))]
    pub async fn check_available(
        &self,
        tenant_id: Uuid,
        paths: &[String],
        deadline: tokio::time::Instant,
    ) -> Result<CheckAvailableResult, SubstituteError> {
        use futures_util::StreamExt;

        let started = std::time::Instant::now();
        // Capability gate — the SAME helper (and therefore the SAME
        // check ordering: upstreams first, client second) as the
        // attempt leg (merged_bug_030; pre-fix this leg tested the
        // client first, so an upstream-less tenant on a clientless
        // replica got all-indeterminate here while the attempt leg
        // answered CleanMiss — Failed{Fetch} → ChargeInfra for jobs
        // that should settle confirmed-miss).
        let (http, upstreams) = match self.capability_gate(tenant_id).await? {
            CapabilityGate::NoUpstreams => {
                // Nothing to consult — the clean no-op, congruent with
                // the attempt leg's Ok(None).
                return Ok(CheckAvailableResult::default());
            }
            CapabilityGate::Clientless => {
                // Upstreams configured, no client: the probe cannot
                // classify ANY path. The old `default()` answer put
                // every path in "confirmed miss" (in neither hits nor
                // indeterminate) — capability-fault laundering.
                // All-indeterminate keeps the scheduler optimistic per
                // the probe-polarity law.
                return Ok(CheckAvailableResult {
                    indeterminate: paths.to_vec(),
                    ..CheckAvailableResult::default()
                });
            }
            CapabilityGate::Ready { http, upstreams } => (http, upstreams),
        };
        debug!(
            n_upstreams = upstreams.len(),
            n_paths = paths.len(),
            "check_available"
        );
        if paths.is_empty() {
            return Ok(CheckAvailableResult::default());
        }

        // Partition into cached / uncached. Cached results (positive
        // and negative) are answered immediately; only uncached paths
        // count against the probe cap and incur HEADs.
        let mut hits = Vec::new();
        let mut indeterminate = Vec::new();
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
            return Ok(CheckAvailableResult {
                hits,
                indeterminate,
                rate_limited: Vec::new(),
            });
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
        // r[impl store.substitute.probe-429-retry+3]
        // Last pass's max Retry-After — rides the rate_limited lane so
        // in-process callers (the executor's miss probe) can surface
        // the advice (bug_295).
        let mut last_retry_after: Option<Duration> = None;
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
            let batch = std::mem::take(&mut pending);
            // Snapshot paths for deadline-dropped recovery. Must
            // `into_iter` (owned) the actual probe inputs — see the
            // HRTB note above; `batch.iter()` + clone trips the same
            // inference failure.
            let batch_paths: Vec<String> = batch.iter().map(|(p, _)| p.clone()).collect();
            let probed: Vec<_> =
                futures_util::stream::iter(batch.into_iter().map(|(p, h)| probe_one(p, h)))
                    .buffer_unordered(concurrency)
                    .take_until(tokio::time::sleep_until(deadline))
                    .collect()
                    .await;
            let completed = probed.len();

            let mut max_retry_after: Option<Duration> = None;
            let mut yielded = std::collections::HashSet::<String>::with_capacity(completed);
            for ((path, hash_part), outcome) in probed {
                yielded.insert(path.clone());
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
                    ProbeOutcome::Indeterminate => indeterminate.push(path),
                    ProbeOutcome::RateLimited { retry_after } => {
                        max_retry_after = max_retry_after
                            .max(Some(retry_after.unwrap_or(Duration::from_secs(1))));
                        pending.push((path, hash_part));
                    }
                }
            }

            // Carry the latest advice across breaks: every exit below
            // (deadline cut, final pass, budget) may leave 429 paths
            // in `pending`, and the rate_limited lane reports the last
            // observed Retry-After for all of them (the 429 is
            // edge-wide, not per-object — same rationale as the
            // per-pass max sleep).
            if max_retry_after.is_some() {
                last_retry_after = max_retry_after;
            }

            if completed < batch_len {
                // `take_until` stopped yielding after the deadline; the
                // un-yielded paths are neither hit nor miss for THIS
                // call. Recover them via the pre-consumption snapshot
                // — without this they were silently dropped and the
                // scheduler treated them as confirmed-miss.
                for path in batch_paths {
                    if !yielded.contains(&path) {
                        indeterminate.push(path);
                    }
                }
                info!(
                    pass,
                    completed,
                    deferred = batch_len - completed,
                    "check_available: probe pass exceeded deadline; \
                     un-probed paths returned indeterminate"
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
                     rate-limited paths returned in the rate_limited lane"
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
        // budget) → the rate_limited lane (bug_295): not cached; the
        // wire surface still reports it indeterminate (optimistic
        // substitute fetch), but in-process callers route the 429
        // class through the truth table so a rate-limit wave closes
        // UNCHARGED instead of burning the park budget.
        let rate_limited: Vec<(String, Option<Duration>)> = pending
            .into_iter()
            .map(|(p, _)| (p, last_retry_after))
            .collect();

        metrics::histogram!("rio_store_check_available_duration_seconds")
            .record(started.elapsed().as_secs_f64());
        Ok(CheckAvailableResult {
            hits,
            indeterminate,
            rate_limited,
        })
    }
}

/// Result of [`Substituter::check_available`].
#[derive(Debug, Default)]
pub struct CheckAvailableResult {
    /// Paths confirmed available (HEAD 2xx) on at least one upstream.
    pub hits: Vec<String>,
    /// Paths the probe could NOT classify (upstream 5xx/timeout/
    /// transport error, or the per-call deadline cut the pass short).
    /// NOT cached; the scheduler MUST treat these optimistically — try
    /// the substitute fetch (its failure path falls through to build)
    /// instead of immediately dispatching a builder. A confirmed
    /// `Miss` is in neither set.
    pub indeterminate: Vec<String>,
    // r[impl store.materialize.probe-polarity]
    /// Paths whose terminal probe state is RATE-LIMITED (429 on every
    /// pass, or the upstream's `Retry-After` exceeded the call
    /// budget), with the last observed advice (bug_295). Split out of
    /// `indeterminate` so in-process callers can route the class
    /// through the substitute-failure truth table (429 closes
    /// UNCHARGED; 5xx/timeout charges). The FindMissingPaths wire
    /// surface merges this back into `indeterminate_paths` — same
    /// optimistic scheduler treatment, no proto change.
    pub rate_limited: Vec<(String, Option<Duration>)>,
}

/// The per-variant effects of one substitute-loop fold verdict
/// (bug_085, effects as data): result-counter label, probe-cache
/// eviction bit, and the attempt outcome — every field MANDATORY, so
/// a new [`SubstituteLoopVerdict`] variant must decide all three at
/// compile time instead of inheriting whatever the copy-paste arm it
/// was cloned from happened to carry (exactly how the ContentMismatch
/// arm shipped without its eviction).
///
/// [`SubstituteLoopVerdict`]: rio_evidence_kernel::outcome::SubstituteLoopVerdict
struct VerdictEffects {
    /// `rio_store_substitute_total{result=...}` tick, once per
    /// singleflight leader (merged_bug_091). `None` = typed transient
    /// with its own signals (Stalled/RateLimited stay uncounted in
    /// this family, as always).
    result_label: Option<&'static str>,
    /// Evict the cached HEAD positive for this (tenant, path): the
    /// verdict CONTRADICTS a cached `true` — either nothing has the
    /// path (CleanMiss, merged_bug_016) or the path is present but
    /// refused (UntrustedPresent, merged_bug_263; ContentMismatch,
    /// bug_085 — presence is exactly the misleading half of a
    /// refusal, and a charge-gating probe consulting the stale `true`
    /// would re-confirm the doomed path for the rest of the 1h TTL).
    /// Transients prove nothing about presence and keep the cache.
    evict_probe_positive: bool,
    /// The attempt outcome the caller returns (`Ok(())` maps to the
    /// miss `Ok(None)` at the single driver).
    outcome: Result<(), SubstituteError>,
}

/// The ONE exhaustive verdict→effects constructor (bug_085): every
/// fold verdict states its full effect record here — no wildcard, no
/// shared-tail defaults — and the single driver in `do_substitute`
/// applies it. The verdict enum itself is kernel-owned
/// (rio-evidence-kernel/src/outcome.rs) and untouched.
fn verdict_effects(
    verdict: rio_evidence_kernel::outcome::SubstituteLoopVerdict,
    store_path: &str,
) -> VerdictEffects {
    use rio_evidence_kernel::outcome::SubstituteLoopVerdict as V;
    match verdict {
        // Typed transients: uncounted in the result family, NO
        // eviction (a stall or a 429 proves nothing about presence —
        // the cached positive may well be the truth); payloads ride
        // the error.
        V::Stalled { window } => VerdictEffects {
            result_label: None,
            evict_probe_positive: false,
            outcome: Err(SubstituteError::Stalled { window }),
        },
        V::RateLimited { retry_after } => VerdictEffects {
            result_label: None,
            evict_probe_positive: false,
            outcome: Err(SubstituteError::RateLimited { retry_after }),
        },
        // ≥1 upstream broke: surfaced as an UNCACHED Fetch error so
        // the next attempt re-asks instead of trusting a poisoned
        // miss; the cached positive is NOT contradicted (the errored
        // upstream answered nothing).
        V::Errored => VerdictEffects {
            result_label: Some("error"),
            evict_probe_positive: false,
            outcome: Err(SubstituteError::Fetch(format!(
                "no upstream served {store_path} and at least one errored — \
                 not a definitive miss"
            ))),
        },
        // merged_bug_046: present upstream with disagreeing bytes — a
        // typed, UNCACHED content refusal, never the cacheable miss
        // the content-blind HEAD confirmation would then contradict
        // into an infrastructure charge. bug_085: and the refusal
        // contradicts any cached HEAD positive's USEFULNESS — evict,
        // like the sibling refusal arms (THE FIX).
        V::ContentMismatch => VerdictEffects {
            result_label: Some("content_mismatch"),
            evict_probe_positive: true,
            outcome: Err(SubstituteError::ContentMismatch),
        },
        // merged_bug_005: present upstream, no verifiable signature —
        // a typed, UNCACHED trust refusal. merged_bug_263: evict the
        // cached positive (presence is the misleading half).
        V::UntrustedPresent => VerdictEffects {
            result_label: Some("untrusted"),
            evict_probe_positive: true,
            outcome: Err(SubstituteError::UntrustedPresent),
        },
        // merged_bug_016: a fresh EVERY-upstream GET-404 contradicts
        // any cached HEAD positive — evict so a charge-gating probe
        // observes live state ("present but not ingested" charged the
        // park budget until the TTL lapsed, pre-fix).
        V::CleanMiss => VerdictEffects {
            result_label: Some("miss"),
            evict_probe_positive: true,
            outcome: Ok(()),
        },
    }
}

/// HTTP statuses an upstream binary cache uses to signal "key not
/// present". 403: S3 without public `s3:ListBucket` returns Forbidden
/// for missing keys. 410: Gone. Matches CppNix `HttpBinaryCacheStore`.
fn is_not_found(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 404 | 403 | 410)
}

// r[impl store.substitute.raced-subscribe]
/// The parked waiter's re-check (the lost-wakeup kill's second half):
/// is the slot still HELD — an `'uploading'` row with a live claim?
/// All three released states answer `false`: row absent (aborted/
/// reaped), `claim_id` NULL (released-in-place, claimable now),
/// `status='complete'` (waiters will hit `AlreadyComplete`).
async fn placeholder_blocked(pool: &PgPool, store_path_hash: &[u8]) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM manifests
             WHERE store_path_hash = $1
               AND status = 'uploading'
               AND claim_id IS NOT NULL
        )
        "#,
    )
    .bind(store_path_hash)
    .fetch_one(pool)
    .await
}

/// Wait up to `slice` for a placeholder event naming `hash`. Returns
/// `true` on a matching (or `Lagged` — conservatively treated as
/// maybe-missed) wake, `false` on the fallback deadline. Foreign
/// hashes keep waiting; a `Closed` channel (listener task gone —
/// unreachable in practice, the task loops forever) degrades to the
/// fallback deadline rather than a hot loop.
async fn wait_for_placeholder_event(
    rx: &mut tokio::sync::broadcast::Receiver<Vec<u8>>,
    hash: &[u8],
    slice: Duration,
) -> bool {
    use tokio::sync::broadcast::error::RecvError;
    let deadline = tokio::time::sleep(slice);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => return false,
            msg = rx.recv() => match msg {
                Ok(h) if h == hash => return true,
                Ok(_) => continue,
                Err(RecvError::Lagged(_)) => return true,
                Err(RecvError::Closed) => {
                    deadline.as_mut().await;
                    return false;
                }
            },
        }
    }
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
    use rio_common::limits::MIN_NAR_CHUNK_CHARGE;
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
    use crate::test_helpers::sandbox_http;

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

    /// [`spawn_fake_upstream`] variant whose NAR endpoint HANGS (sends
    /// headers, then no body bytes, forever) for the first
    /// `hang_requests` requests and serves normally afterwards — the
    /// wedged-owner fixture for the stall-abort battery.
    async fn spawn_fake_upstream_hang_then_serve(
        store_path: &str,
        nar_bytes: Vec<u8>,
        key_name: &str,
        hang_requests: u32,
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
        let requests = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let nar_c = nar_bytes.clone();
        let app = Router::new()
            .route(
                "/nix-cache-info",
                get(|| async { "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n" }),
            )
            .route(&narinfo_path, get(move || async move { narinfo }))
            .route(
                &nar_path,
                get(move || async move {
                    let n = requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if n < hang_requests {
                        // Wedge: never produce body bytes.
                        tokio::time::sleep(Duration::from_secs(3600)).await;
                    }
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

    // r[verify store.substitute.stall-abort+2]
    /// Owner-side stall abort end-to-end: a wedged upstream (headers,
    /// then no body bytes) makes the OWNER abort its own download
    /// after the stall window — effective even with every caller
    /// coalesced behind this owner's singleflight, where no competing
    /// `claim_placeholder` would ever observe the stall. The claim is
    /// released IN PLACE (row survives, claim cleared, stall_count=1)
    /// and the next attempt re-claims immediately and completes.
    #[tokio::test]
    async fn stall_abort_releases_in_place_then_next_attempt_recovers() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let rec = DebuggingRecorder::new();
        let snap = rec.snapshotter();
        let _mguard = metrics::set_default_local_recorder(&rec);

        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-stall-abort").await;
        let (path, nar) = make_path();
        // Hangs the FIRST NAR request only; the retry serves.
        let fake =
            spawn_fake_upstream_hang_then_serve(&path, nar.clone(), "cache.stall-1", 1).await;
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

        let sub = test_substituter(db.pool.clone()).with_stall_window(Duration::from_secs(1));

        // Attempt 1: must ABORT (not hang) within the test budget —
        // the owner-side watchdog is the only thing that can end this
        // download (the request-level timeouts deliberately exclude
        // the NAR body).
        let got = tokio::time::timeout(Duration::from_secs(30), sub.try_substitute(tid, &path))
            .await
            .expect("the owner-side stall abort must fire (download never ends on its own)");
        assert!(
            matches!(got, Err(SubstituteError::Stalled { .. })),
            "a wedged download must surface Err(Stalled), got {got:?}"
        );

        // The claim was released IN PLACE: row survives with the strike.
        let sp = StorePath::parse(&path).unwrap();
        let hash = sp.sha256_digest();
        let (status, claim_id, fetched, stalls): (String, Option<Uuid>, Option<i64>, i16) =
            sqlx::query_as(
                "SELECT status, claim_id, fetched_bytes, stall_count \
                   FROM manifests WHERE store_path_hash = $1",
            )
            .bind(hash.as_slice())
            .fetch_one(&db.pool)
            .await
            .expect("placeholder row survives the abort");
        assert_eq!(status, "uploading");
        assert_eq!(claim_id, None, "claim released (cleared), not deleted");
        assert_eq!(fetched, None, "progress NULLed on release");
        assert_eq!(stalls, 1, "the stall recorded its strike");

        // Attempt 2: re-claims the released row IMMEDIATELY (no
        // staleness threshold) and completes against the now-healthy
        // upstream. The moka singleflight did NOT cache the Err.
        let got2 = sub
            .try_substitute(tid, &path)
            .await
            .expect("second attempt must not error")
            .expect("second attempt re-claims the released row and ingests");
        assert_eq!(got2.nar_size, nar.len() as u64);

        // The abort was counted with its own reason.
        let mut stall_aborts = 0u64;
        for (ck, _, _, v) in snap.snapshot().into_vec() {
            let DebugValue::Counter(c) = v else { continue };
            if ck.key().name() == "rio_store_substitute_stale_reclaimed_total"
                && ck
                    .key()
                    .labels()
                    .any(|l| l.key() == "reason" && l.value() == "stall_abort")
            {
                stall_aborts += c;
            }
        }
        assert_eq!(stall_aborts, 1, "the abort increments reason=stall_abort");
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

    /// merged_bug_091 RED-FIRST: `result=miss` is an ATTEMPT-level
    /// verdict (one per singleflight leader, from the post-loop
    /// CleanMiss fold) — the HELP has claimed per-leader granularity
    /// all along, but the increment lived inside the per-upstream
    /// loop, so a tenant with N upstreams emitted N misses per
    /// attempt and every hit-ratio panel over multi-upstream tenants
    /// was wrong.
    #[tokio::test]
    async fn substitute_miss_counts_once_per_leader_attempt() {
        use rio_test_support::metrics::CountingRecorder;

        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-miss-granularity").await;
        let (have_path, nar) = make_path();
        // TWO upstreams, both 404 the requested path (each serves only
        // `have_path`; we ask for a different hash).
        let a = spawn_fake_upstream(&have_path, nar.clone(), "cache.miss-a").await;
        let b = spawn_fake_upstream(&have_path, nar, "cache.miss-b").await;
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
        let other = "/nix/store/cccccccccccccccccccccccccccccccc-on-no-upstream".to_string();
        let sub = test_substituter(db.pool.clone());

        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);
        let got = sub.try_substitute(tid, &other).await.unwrap();
        assert!(got.is_none(), "clean miss across both upstreams");

        assert_eq!(
            rec.get(&format!(
                "rio_store_substitute_total{{result=miss,tenant={tid}}}"
            )),
            1,
            "one attempt = one miss, regardless of upstream count; keys={:?}",
            rec.all_keys()
        );
    }

    /// Per-tenant result labeling at ATTEMPT granularity
    /// (merged_bug_091): a 404+500 mix is ONE attempt whose fold
    /// verdict is Errored — `{result=error,tenant=UUID}` ticks once
    /// and `result=miss` not at all (the 404 is per-upstream detail
    /// in the debug! log). The label MUST be `tenant` (UUID, bounded
    /// by tenant count), NOT `upstream` (tenant-supplied URL,
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
        // merged_bug_044: B errored, so the iteration is NOT a clean
        // miss — the fold surfaces an uncached Err(Fetch) (pre-fix
        // this was a cacheable Ok(None)). The per-tenant labels below
        // are what this test pins; the verdict flip is pinned by the
        // dedicated all-errored battery.
        let got = sub.try_substitute(tid, &other).await;
        assert!(
            matches!(got, Err(SubstituteError::Fetch(_))),
            "404+500 mix must be Err(Fetch), got {got:?}"
        );

        let miss = format!("rio_store_substitute_total{{result=miss,tenant={tid}}}");
        let err = format!("rio_store_substitute_total{{result=error,tenant={tid}}}");
        assert_eq!(
            rec.get(&miss),
            0,
            "the errored attempt is NOT a miss (attempt-level verdict); keys={:?}",
            rec.all_keys()
        );
        assert_eq!(
            rec.get(&err),
            1,
            "one attempt = one error tick; keys={:?}",
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

    // r[verify store.substitute.loop-evidence-total]
    /// merged_bug_044 red (044-a): ONE upstream, every narinfo GET
    /// 500s. The pre-fix loop's catch-all `Err(e)` arm recorded NO
    /// evidence, so the post-loop fold saw `(None, None)` →
    /// `CleanMiss` → `Ok(None)` — an all-errored iteration was
    /// indistinguishable from a definitive all-404 miss. The kernel
    /// cells make the error axis evidence by construction: the fold
    /// yields `Errored` and the caller returns `Err(Fetch)` (gRPC
    /// `unavailable` — retryable), never a cacheable miss.
    ///
    /// Recorded red (pre-fix): `got Ok(None)` where `Err(Fetch)`
    /// expected.
    #[tokio::test]
    async fn substitute_all_errored_is_err_not_clean_miss() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-all-err").await;
        let b = spawn_500_upstream().await;
        metadata::upstreams::insert(
            &db.pool,
            tid,
            &b.url,
            50,
            &["dummy:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into()],
            SigMode::Keep,
        )
        .await
        .unwrap();

        let path = format!(
            "/nix/store/{}-all-errored",
            rio_test_support::fixtures::rand_store_hash()
        );
        let sub = test_substituter(db.pool.clone());
        let got = sub.try_substitute(tid, &path).await;
        assert!(
            matches!(got, Err(SubstituteError::Fetch(_))),
            "all-errored iteration must be Err(Fetch), got {got:?}"
        );
    }

    /// merged_bug_044 red (044-b, cache poisoning): the all-errored
    /// `Ok(None)` was CACHED by the moka definitive-miss slot — every
    /// `(tenant, path)` probed during a 30 s upstream outage stayed
    /// "missing" for the full TTL even after recovery, silently
    /// degrading to build-from-source. `Err` is never cached: the
    /// next call re-runs the iteration and hits the recovered (here:
    /// newly configured, higher-priority) upstream.
    ///
    /// Recorded red (pre-fix): second call returned the poisoned
    /// cached `None` instead of the served path.
    #[tokio::test]
    async fn substitute_all_errored_not_cached_as_definitive_miss() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-poison").await;
        let (path, nar) = make_path();
        // Phase 1: only a 500ing upstream configured.
        let b = spawn_500_upstream().await;
        metadata::upstreams::insert(
            &db.pool,
            tid,
            &b.url,
            50,
            &["dummy:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into()],
            SigMode::Keep,
        )
        .await
        .unwrap();
        let sub = test_substituter(db.pool.clone());
        let first = sub.try_substitute(tid, &path).await;
        assert!(
            matches!(first, Err(SubstituteError::Fetch(_))),
            "phase-1 all-errored must be Err(Fetch), got {first:?}"
        );

        // Phase 2: a healthy upstream appears at higher priority. The
        // Err above must NOT have populated the (tenant, path) slot —
        // this call re-runs the iteration and serves.
        let a = spawn_fake_upstream(&path, nar, "cache.poison-a").await;
        metadata::upstreams::insert(
            &db.pool,
            tid,
            &a.url,
            10,
            std::slice::from_ref(&a.trusted_key),
            SigMode::Keep,
        )
        .await
        .unwrap();
        let second = sub.try_substitute(tid, &path).await.unwrap();
        assert!(
            second.is_some(),
            "recovered upstream must serve — a cached all-errored miss poisons the slot"
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
        assert!(hits.hits.is_empty());
        assert!(hits.indeterminate.is_empty(), "403 is a definitive miss");

        let rec2 = CountingRecorder::default();
        let _g2 = metrics::set_default_local_recorder(&rec2);
        let hits2 = sub
            .check_available(tid, std::slice::from_ref(&path), far_deadline())
            .await
            .unwrap();
        assert!(hits2.hits.is_empty());
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

    /// A tenant with zero `tenant_upstreams` rows returning `Ok(None)`
    /// is correct behaviour — but it MUST be countable. Every skip
    /// branch in the substitution pipeline degrading silently to
    /// "build it from source" is how the 2026-05-23 incident burned
    /// builder CPU compiling cache.nixos.org-cached paths: the
    /// operator had no signal distinguishing "the upstream really
    /// doesn't have it" from "we never asked the upstream".
    /// `reason=no_upstreams` is the "asked to substitute for a tenant
    /// with no upstreams configured" signal. Counted once per
    /// singleflight leader (same granularity as `result=hit|miss`).
    #[tokio::test]
    async fn substitute_skip_no_upstreams_is_counted() {
        use rio_test_support::metrics::CountingRecorder;

        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-none-counted").await;
        let (path, _) = make_path();
        let sub = test_substituter(db.pool.clone());

        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);
        let got = sub.try_substitute(tid, &path).await.unwrap();
        assert!(got.is_none(), "no upstreams → None");
        assert_eq!(
            rec.get("rio_store_substitute_skipped_total{reason=no_upstreams}"),
            1,
            "a no-upstreams skip must increment \
             rio_store_substitute_skipped_total{{reason=no_upstreams}}, \
             not silently no-op; keys={:?}",
            rec.all_keys()
        );
    }

    /// `http: None` (the reqwest client failed to build at startup —
    /// no CA bundle in the sandbox, or a future builder regression)
    /// USED to degrade every substitution on this replica to
    /// `Ok(None)` = build-from-source — a skip that left no trace,
    /// indistinguishable from "the upstream really doesn't have it".
    /// Now counted AND surfaced per singleflight leader: with
    /// upstreams configured, a clientless substituter is a hard,
    /// uncached error — never a clean miss (the merged_bug_044 law,
    /// ordered by the CapabilityGate; the `no_upstreams` arm is a
    /// DIFFERENT class — a deliberately truthful clean no-op). The `http` field is private and only ever `None`
    /// when `Client::builder().build()` fails, so the test reaches
    /// into the field directly (the test module is a child of the
    /// defining module) rather than adding a test-only constructor to
    /// production code.
    #[tokio::test]
    async fn substitute_no_http_client_with_upstreams_is_an_error() {
        use rio_test_support::metrics::CountingRecorder;

        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-no-http-counted").await;
        let (path, _) = make_path();
        // An upstream IS configured (never contacted — the capability
        // check fires first). Without it the walk lawfully answers
        // Ok(None) under reason=no_upstreams before the client check.
        metadata::upstreams::insert(&db.pool, tid, "http://127.0.0.1:9", 50, &[], SigMode::Keep)
            .await
            .unwrap();
        let mut sub = test_substituter(db.pool.clone());
        sub.http = None;

        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);
        // RED under the pre-fix arm: `Ok(None)` — the capability fault
        // laundered into a definitive NotFound at the caller.
        let got = sub.try_substitute(tid, &path).await;
        assert!(
            matches!(got, Err(SubstituteError::Fetch(_))),
            "no http client + configured upstreams must be a hard \
             error (the walk cannot confirm a miss it never ran), got {got:?}"
        );
        // UNCACHED, same law as the all-errored fold: a second call is
        // a fresh singleflight leader and errs (and counts) again.
        let again = sub.try_substitute(tid, &path).await;
        assert!(matches!(again, Err(SubstituteError::Fetch(_))));
        assert_eq!(
            rec.get("rio_store_substitute_skipped_total{reason=no_http_client}"),
            2,
            "each no-http-client leader must increment \
             rio_store_substitute_skipped_total{{reason=no_http_client}}; \
             keys={:?}",
            rec.all_keys()
        );
    }

    // r[verify store.substitute.content-binding]
    /// merged_bug_114 (HIGH): the `AlreadyComplete` arm must
    /// CONTENT-BIND the upstream's narinfo claim to the stored row
    /// before returning the stored row as this upstream's Hit or
    /// appending the upstream's signatures. The narinfo got here by
    /// naming the path and verifying against tenant-supplied
    /// `trusted_keys` — which the size-gate comment itself says is NOT
    /// a trust boundary — and the arm runs BEFORE any body fetch, so a
    /// tenant whose upstream self-signs a fabricated narinfo (forged
    /// NarHash) for a victim path would otherwise get the stored bytes
    /// back as a Hit (cross-tenant content disclosure via the walk's
    /// pin/stamp lane) plus persisted upstream signatures whose
    /// fingerprint cannot match the stored row.
    #[tokio::test]
    async fn already_complete_requires_content_agreement() {
        use rio_test_support::fixtures::make_path_info;

        let db = TestDb::new(&crate::MIGRATOR).await;

        // 1. The VICTIM row: a legitimately-ingested complete path.
        let path = rio_test_support::fixtures::test_store_path("victim-content");
        let (nar, nar_hash) = rio_test_support::fixtures::make_nar(b"victim-secret");
        let info = make_path_info(&path, &nar, nar_hash);
        let path_hash = info.store_path.sha256_digest();
        let claim = metadata::insert_manifest_uploading(&db.pool, &path_hash, &path, &[])
            .await
            .unwrap()
            .unwrap();
        let mut stored = info.clone();
        stored.store_path_hash = path_hash.to_vec();
        metadata::complete_manifest_inline(&db.pool, &stored, claim, nar.clone().into())
            .await
            .unwrap();

        // 2. The ATTACKER tenant trusts a fake upstream that
        //    self-signs a narinfo naming the victim path with a
        //    DIFFERENT NarHash (forged bytes are never fetched — the
        //    arm runs before any body fetch).
        let upstream =
            spawn_fake_upstream(&path, b"forged-other-bytes".to_vec(), "attacker-key").await;
        let tid = seed_tenant(&db.pool, "attacker-content").await;
        metadata::upstreams::insert(
            &db.pool,
            tid,
            &upstream.url,
            50,
            std::slice::from_ref(&upstream.trusted_key),
            SigMode::Keep,
        )
        .await
        .unwrap();
        let sub = test_substituter(db.pool.clone());

        let before: i32 =
            sqlx::query_scalar("SELECT cardinality(signatures) FROM narinfo WHERE store_path = $1")
                .bind(&path)
                .fetch_one(&db.pool)
                .await
                .unwrap();

        // THE law (merged_bug_046 re-aim): content disagreement is a
        // TYPED, UNCACHED refusal — never a Miss (the pre-fix Ok(None)
        // folded to a cacheable CleanMiss that the content-blind HEAD
        // confirmation then contradicted into a per-retry ChargeInfra
        // loop until the job parked), and still never a cross-tenant
        // Hit. Red captured against the old pin verbatim:
        // "must be a Miss for that upstream (no cross-tenant Hit),
        //  got Err(ContentMismatch)".
        let got = sub.try_substitute(tid, &path).await;
        assert!(
            matches!(got, Err(SubstituteError::ContentMismatch)),
            "a narinfo whose content claim disagrees with the stored row \
             must be a typed content refusal (no cross-tenant Hit, no \
             cacheable miss), got {got:?}"
        );
        let after: i32 =
            sqlx::query_scalar("SELECT cardinality(signatures) FROM narinfo WHERE store_path = $1")
                .bind(&path)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(
            before, after,
            "no upstream signature may be appended over a content-mismatched claim"
        );

        // Positive control: an upstream whose narinfo AGREES with the
        // stored row (same bytes ⇒ same NarHash/NarSize/refs) still
        // gets the AlreadyComplete Hit + its signature appended — the
        // legitimate lost-the-race flow is untouched.
        let honest = spawn_fake_upstream(&path, nar.clone(), "honest-key").await;
        let tid2 = seed_tenant(&db.pool, "honest-content").await;
        metadata::upstreams::insert(
            &db.pool,
            tid2,
            &honest.url,
            50,
            std::slice::from_ref(&honest.trusted_key),
            SigMode::Keep,
        )
        .await
        .unwrap();
        let got2 = sub.try_substitute(tid2, &path).await;
        assert!(
            matches!(got2, Ok(Some(_))),
            "content-agreeing AlreadyComplete must stay a Hit, got {got2:?}"
        );
        let after2: i32 =
            sqlx::query_scalar("SELECT cardinality(signatures) FROM narinfo WHERE store_path = $1")
                .bind(&path)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert!(
            after2 > after,
            "the agreeing upstream's signature must be appended (got {after} -> {after2})"
        );
    }

    /// bug_085: a content-mismatch refusal must EVICT the cached HEAD
    /// positive for its (tenant, path) — the sibling refusal arms
    /// (UntrustedPresent, merged_bug_263; CleanMiss, merged_bug_016)
    /// both evict, and the arm's own rationale already argues it: a
    /// cached positive answers PRESENCE, and presence is exactly the
    /// misleading half of a content refusal — a charge-gating
    /// confirmation probe consulting the stale `true` re-confirms the
    /// doomed path for the rest of the 1h TTL. The cache is seeded by
    /// a REAL prior HEAD-positive probe (`check_available` against
    /// the live upstream) and the verdict is minted by the REAL
    /// substitute loop over the stored-row disagreement — no
    /// hand-rolled verdicts, no cache poking.
    #[tokio::test]
    async fn content_mismatch_evicts_cached_probe_positive() {
        use rio_test_support::fixtures::make_path_info;

        let db = TestDb::new(&crate::MIGRATOR).await;

        // The victim row: a legitimately-ingested complete path.
        let path = rio_test_support::fixtures::test_store_path("victim-evict");
        let (nar, nar_hash) = rio_test_support::fixtures::make_nar(b"victim-evict-secret");
        let info = make_path_info(&path, &nar, nar_hash);
        let path_hash = info.store_path.sha256_digest();
        let claim = metadata::insert_manifest_uploading(&db.pool, &path_hash, &path, &[])
            .await
            .unwrap()
            .unwrap();
        let mut stored = info.clone();
        stored.store_path_hash = path_hash.to_vec();
        metadata::complete_manifest_inline(&db.pool, &stored, claim, nar.clone().into())
            .await
            .unwrap();

        // The tenant trusts a fake upstream that self-signs a narinfo
        // naming the victim path with a DIFFERENT NarHash.
        let upstream =
            spawn_fake_upstream(&path, b"forged-evict-bytes".to_vec(), "evict-key").await;
        let tid = seed_tenant(&db.pool, "content-evict").await;
        metadata::upstreams::insert(
            &db.pool,
            tid,
            &upstream.url,
            50,
            std::slice::from_ref(&upstream.trusted_key),
            SigMode::Keep,
        )
        .await
        .unwrap();
        let sub = test_substituter(db.pool.clone());

        // 1. A real prior HEAD-positive probe seeds the cache: the
        //    upstream serves the (forged) narinfo, so the HEAD answers
        //    present and check_available caches `true`.
        let probed = sub
            .check_available(tid, std::slice::from_ref(&path), far_deadline())
            .await
            .unwrap();
        assert_eq!(
            probed.hits,
            vec![path.clone()],
            "the seeding probe must answer hit (the upstream serves the narinfo)"
        );
        assert_eq!(
            sub.probe_cache.get(&(tid, path.clone())).await,
            Some(true),
            "the HEAD positive must be cached before the refusal"
        );

        // 2. The substitute loop reaches the content-disagreement
        //    verdict through the production cells + fold.
        let got = sub.try_substitute(tid, &path).await;
        assert!(
            matches!(got, Err(SubstituteError::ContentMismatch)),
            "the disagreeing claim must be the typed content refusal, got {got:?}"
        );

        // 3. THE pin: the cached positive is GONE.
        assert_eq!(
            sub.probe_cache.get(&(tid, path.clone())).await,
            None,
            "left: Some(true) / right: None — the stale HEAD positive must \
             not survive the content refusal for the 1h TTL"
        );
    }

    /// merged_bug_030: the capability gate's ordering law is shared by
    /// BOTH legs — upstreams-empty FIRST, clientless SECOND. An
    /// upstream-less tenant on a clientless replica is a clean no-op
    /// on the attempt leg (`Ok(None)`); the probe leg must agree
    /// (empty result — nothing to consult), NOT all-indeterminate
    /// (which the executor's probe fold charges as Fetch). Pre-fix,
    /// `check_available` tested the client before the upstream list —
    /// opposite of `try_substitute_inner` — so the same
    /// (tenant, replica) state answered CleanMiss on one leg and
    /// Failed{Fetch}→ChargeInfra on the other, parking jobs that
    /// should settle confirmed-miss.
    #[tokio::test]
    async fn check_available_clientless_without_upstreams_is_clean_empty() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "probe-no-upstreams-no-http").await;
        // NO upstreams configured; clientless replica.
        let mut sub = test_substituter(db.pool.clone());
        sub.http = None;
        let (path, _) = make_path();

        // Attempt leg: clean no-op (upstreams-first — established).
        let attempt = sub.try_substitute(tid, &path).await;
        assert!(
            matches!(attempt, Ok(None)),
            "attempt leg: upstream-less tenant is a clean miss even \
             clientless, got {attempt:?}"
        );

        // Probe leg MUST agree: empty result, not all-indeterminate.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let probe = sub
            .check_available(tid, std::slice::from_ref(&path), deadline)
            .await
            .unwrap();
        assert!(
            probe.indeterminate.is_empty(),
            "probe leg must agree with the attempt leg's clean no-op \
             for an upstream-less tenant on a clientless replica; got \
             indeterminate={:?}",
            probe.indeterminate
        );
        assert!(probe.hits.is_empty() && probe.rate_limited.is_empty());
    }

    // r[verify store.substitute.probe-429-retry+3]
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

    // r[verify store.substitute.stall-abort+2]
    /// bug_081: a stall on the FIRST upstream is an upstream-local
    /// failure — the loop must CONTINUE to the second upstream
    /// (mirroring the 429 failover) and serve the path. The strike
    /// was already durably recorded by the in-place release; failing
    /// over loses nothing. Stalled surfaces as the attempt outcome
    /// only when NO later upstream serves, and it dominates a
    /// concurrent 429 (charging evidence outranks back-off advice).
    /// RED (pre-fix): the Stalled arm aborted the loop — got
    /// Err(Stalled) with healthy-B never consulted.
    #[tokio::test]
    async fn do_substitute_stall_first_upstream_tries_second() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-stall-then-hit").await;
        let (path, nar) = make_path();
        // Upstream A: narinfo OK, NAR wedges forever (every request).
        let a = spawn_fake_upstream_hang_then_serve(&path, nar.clone(), "cache.stall-a", u32::MAX)
            .await;
        // Upstream B: serves the path.
        let b = spawn_fake_upstream(&path, nar, "cache.stall-b").await;
        // Priority: A=10 (tried first), B=50 (tried second).
        metadata::upstreams::insert(
            &db.pool,
            tid,
            &a.url,
            10,
            std::slice::from_ref(&a.trusted_key),
            SigMode::Keep,
        )
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

        let sub = test_substituter(db.pool.clone()).with_stall_window(Duration::from_secs(1));
        let got = tokio::time::timeout(Duration::from_secs(60), sub.try_substitute(tid, &path))
            .await
            .expect("attempt must end within budget");
        assert!(
            matches!(&got, Ok(Some(_))),
            "a stall on the first upstream must fail over to the second, got {got:?}"
        );

        // Precedence: stall-A + 429-C (no server) → the stall dominates
        // the 429 as the attempt outcome (charging evidence outranks
        // back-off advice; the 429 path would hide the recorded strike
        // from the executor's classification). A FRESH path: leg 1
        // ingested `path` locally, which would short-circuit upstreams.
        let path2 = rio_test_support::fixtures::test_store_path("stall-vs-429");
        let (nar2, _) = rio_test_support::fixtures::make_nar(b"stall-vs-429");
        let a2 =
            spawn_fake_upstream_hang_then_serve(&path2, nar2, "cache.stall-a2", u32::MAX).await;
        let tid2 = seed_tenant(&db.pool, "sub-stall-vs-429").await;
        let c = spawn_status_upstream(axum::http::StatusCode::TOO_MANY_REQUESTS).await;
        metadata::upstreams::insert(
            &db.pool,
            tid2,
            &a2.url,
            10,
            std::slice::from_ref(&a2.trusted_key),
            SigMode::Keep,
        )
        .await
        .unwrap();
        metadata::upstreams::insert(&db.pool, tid2, &c.url, 50, &[], SigMode::Keep)
            .await
            .unwrap();
        let got2 = tokio::time::timeout(Duration::from_secs(60), sub.try_substitute(tid2, &path2))
            .await
            .expect("attempt must end within budget");
        assert!(
            matches!(got2, Err(SubstituteError::Stalled { .. })),
            "stall + 429 with no hit must surface Stalled (dominates RateLimited), got {got2:?}"
        );
    }

    proptest::proptest! {
        /// bug_081 + merged_bug_044 failover-totality: for ANY
        /// recorded evidence sequence the post-loop fold is total and
        /// honors the precedence Stalled > RateLimited > Errored >
        /// CleanMiss — the failover loop can never fall through
        /// without a verdict, recorded charging evidence is never
        /// shadowed by back-off advice, and an iteration with ANY
        /// errored upstream can never fold to the cacheable miss.
        #[test]
        fn prop_substitute_loop_fold_total(
            stall_secs in proptest::option::of(0u64..1_000_000),
            had_429 in proptest::bool::ANY,
            retry_secs in proptest::option::of(0u64..1_000_000),
            errored in proptest::bool::ANY,
        ) {
            use rio_evidence_kernel::outcome::{
                SubstituteFailureClass, SubstituteLoopCells, SubstituteLoopVerdict,
                fold_substitute_loop,
            };
            let any_stall = stall_secs.map(Duration::from_secs);
            let any_429 = had_429.then(|| retry_secs.map(Duration::from_secs));
            let mut cells = SubstituteLoopCells::new();
            if let Some(w) = any_stall {
                let _ = cells.record(SubstituteFailureClass::Stalled, Some(w));
            }
            if let Some(ra) = any_429 {
                let _ = cells.record(SubstituteFailureClass::RateLimited, ra);
            }
            if errored {
                let _ = cells.record(SubstituteFailureClass::Fetch, None);
            }
            let got = fold_substitute_loop(cells);
            match (any_stall, any_429, errored) {
                (Some(w), _, _) => proptest::prop_assert_eq!(
                    got, SubstituteLoopVerdict::Stalled { window: w }),
                // merged_bug_210: charge tier (Errored) outranks the
                // 429 back-off tier — the corrected precedence.
                (None, _, true) => proptest::prop_assert_eq!(
                    got, SubstituteLoopVerdict::Errored),
                (None, Some(ra), false) => proptest::prop_assert_eq!(
                    got, SubstituteLoopVerdict::RateLimited { retry_after: ra }),
                (None, None, false) => proptest::prop_assert_eq!(
                    got, SubstituteLoopVerdict::CleanMiss),
            }
        }
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
        // merged_bug_005: a present-but-unverifiable narinfo is a
        // TYPED TRUST REFUSAL, not a miss. (This test previously
        // asserted `got.is_none()` — "as good as 404" — which was the
        // laundering itself: the cacheable miss sent the sig-blind
        // HEAD confirmation into a permanent infra charge.)
        let got = sub.try_substitute(tid, &path).await;
        assert!(
            matches!(got, Err(SubstituteError::UntrustedPresent)),
            "sig verification failed → typed uncached trust refusal, got {got:?}"
        );
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
        assert_eq!(
            available.hits,
            vec![path],
            "only the seeded path is available"
        );
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
            available.hits.len(),
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

    // r[verify store.substitute.probe-429-retry+3]
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
            available.hits.len(),
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

    // r[verify store.substitute.probe-429-retry+3]
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
        assert_eq!(available.hits.len(), 200, "all eventually hit after retry");

        let pass1_max = fake.max_concurrent_after.load(Ordering::SeqCst);
        assert!(
            pass1_max > 0 && pass1_max <= SUBSTITUTE_PROBE_CONCURRENCY / 2,
            "retry pass concurrency must be ≤ {}/2 (halved); observed max={pass1_max}",
            SUBSTITUTE_PROBE_CONCURRENCY
        );
    }

    // r[verify store.substitute.probe-429-retry+3]
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
            available.hits.is_empty(),
            "Retry-After > budget → no retry → 0 hits"
        );
        // bug_295: terminal 429s ride the rate_limited lane (with the
        // upstream's advice) so in-process callers can defer
        // UNCHARGED; the wire surface merges them back into
        // indeterminate_paths at the FMP handler.
        assert_eq!(
            available.rate_limited.len(),
            paths.len(),
            "rate-limited paths past budget ride the rate_limited lane"
        );
        assert!(
            available
                .rate_limited
                .iter()
                .all(|(_, ra)| *ra == Some(Duration::from_secs(300))),
            "the upstream's Retry-After advice rides each entry: {:?}",
            available.rate_limited
        );
        assert!(
            available.indeterminate.is_empty(),
            "a terminal 429 is NOT indeterminate (5xx/timeout) — the classes split"
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
        // No 429s — exercise the pass-0 deadline bound directly.
        // 200 paths, concurrency=128 (WantMassQuery): wave-1 (128)
        // lands ~head_delay+overhead, wave-2 (72) ~2×(head_delay+overhead).
        // Deadline at 1.5×head_delay is the midpoint when overhead=0.
        // Asserts the take_until semantics: wave-1 hits survive, wave-2
        // is deferred (Indeterminate).
        //
        // Slack budget: under contended CI builders (8 nextest jobs ×
        // postgres × this fake server, all on the same box) the
        // per-wave executor/HTTP overhead can exceed 500ms. The deadline
        // must satisfy `head_delay + overhead < deadline` for wave-1 to
        // land; with deadline = 1.5×head_delay that means
        // `overhead < 0.5×head_delay`. 1500ms gives a 750ms ceiling.
        // The previous 400ms (200ms ceiling) flaked deterministically on
        // overlay-backed builders.
        const HEAD_DELAY: Duration = Duration::from_millis(1500);
        let fake = spawn_mass_probe_upstream(ProbeCfg {
            head_delay: HEAD_DELAY,
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
        let deadline = t0 + HEAD_DELAY.mul_f64(1.5);
        let available = sub.check_available(tid, &paths, deadline).await.unwrap();
        let elapsed = t0.elapsed();

        // Partial-pass results survive the deadline (regression: the
        // old timeout(.., collect()) dropped them all). Structural
        // assertion — "some survived AND some deferred" — not exact
        // wave boundaries (builder CPU variance).
        assert!(
            !available.hits.is_empty(),
            "completed-before-deadline hits must survive truncation; got 0"
        );
        assert!(
            available.hits.len() < paths.len(),
            "deadline must truncate the pass; got all {} hits",
            available.hits.len()
        );
        // r[verify sched.merge.substitute-probe-indeterminate+2]
        // Un-yielded paths are reported indeterminate (not silently
        // dropped). hits ∪ indeterminate covers the full input.
        assert_eq!(
            available.hits.len() + available.indeterminate.len(),
            paths.len(),
            "every path must be in hits or indeterminate"
        );
        // Returned near deadline, not after wave-2 (~2×head_delay).
        // Loose upper bound — 4×head_delay = 6s — the structural asserts
        // above are primary; this just catches "ignored the deadline and
        // waited out the whole stream."
        assert!(
            elapsed < HEAD_DELAY * 4,
            "must return at deadline, not wait out wave-2; elapsed={elapsed:?}"
        );
        // Survived hits ARE cached.
        assert_eq!(
            sub.probe_cache.get(&(tid, available.hits[0].clone())).await,
            Some(true),
            "completed hits must be cached as Hit"
        );
        // Deferred paths (Indeterminate) are NOT cached; next call
        // re-probes.
        let deferred = &available.indeterminate;
        assert!(!deferred.is_empty());
        assert!(
            sub.probe_cache
                .get(&(tid, deferred[0].clone()))
                .await
                .is_none(),
            "deadline-truncated paths must NOT be cached"
        );
    }

    // r[verify store.substitute.probe-429-retry+3]
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

        assert_eq!(
            available.hits.len(),
            5,
            "HTTP-date Retry-After → retried to Hit"
        );
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

    // r[verify store.substitute.probe-429-retry+3]
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

    // r[verify store.substitute.probe-429-retry+3]
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
        assert_eq!(first.hits, vec![path.clone()]);

        // Kill the upstream. Second call must answer from cache —
        // including the negative result for `absent`.
        fake._task.abort();
        let _ = fake._task.await;

        let second = sub
            .check_available(tid, &batch, far_deadline())
            .await
            .unwrap();
        assert_eq!(
            second.hits,
            vec![path],
            "cached positive + negative results"
        );
        assert!(
            second.indeterminate.is_empty(),
            "cached negative is a confirmed miss, not indeterminate"
        );
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
        assert!(b.hits.is_empty(), "B's upstream is dead → no hit");

        // A probes second → MUST hit A's upstream, not return B's
        // cached miss. With a path-only key this would be `[]`.
        let a = sub
            .check_available(tid_a, batch, far_deadline())
            .await
            .unwrap();
        assert_eq!(a.hits, vec![path.clone()], "A's upstream serves the path");

        // Reverse leakage: A's hit must not leak to B.
        let b2 = sub
            .check_available(tid_b, batch, far_deadline())
            .await
            .unwrap();
        assert!(b2.hits.is_empty(), "B still has no hit (per-tenant)");
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

    // r[verify store.substitute.stale-reclaim+3]
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

    /// [`spawn_fake_upstream`] variant that counts narinfo GETs — one
    /// per substitution ATTEMPT (the narinfo fetch precedes the
    /// placeholder claim), so the counter is the round-trip witness
    /// the live_055(b) poll-count law is stated in.
    async fn spawn_fake_upstream_counted(
        store_path: &str,
        nar_bytes: Vec<u8>,
        key_name: &str,
    ) -> (FakeUpstream, Arc<std::sync::atomic::AtomicUsize>) {
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

        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits_c = Arc::clone(&hits);
        let narinfo_path = format!("/{hash_part}.narinfo");
        let nar_path = format!("/nar/{hash_part}.nar");
        let app = Router::new()
            .route(
                "/nix-cache-info",
                get(|| async { "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n" }),
            )
            .route(
                &narinfo_path,
                get(move || {
                    let narinfo = narinfo.clone();
                    let hits = Arc::clone(&hits_c);
                    async move {
                        hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        narinfo
                    }
                }),
            )
            .route(&nar_path, get(move || async move { nar_bytes }));

        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (
            FakeUpstream {
                url: format!("http://{addr}"),
                trusted_key,
                _task: task,
            },
            hits,
        )
    }

    // r[verify store.substitute.raced-subscribe]
    /// live_055(b) / W9-BL — the raced-path round-trip law: a waiter
    /// blocked on a concurrently held placeholder completes within
    /// TWO upstream round-trips of the holder's release (the initial
    /// raced attempt + at most ONE post-release re-attempt). Pre-fix
    /// the executor plane poll-raced (`Err(Raced)` → RetryLater →
    /// re-dispatch ~seconds — 633 round-trips against one blocked
    /// placeholder in the live_055 capture); the close subscribes the
    /// raced path to placeholder release/completion instead.
    ///
    /// Count-witnessed: the narinfo GET counter is the round-trip
    /// oracle (one GET per attempt, fetched before the claim). The
    /// fallback poll is set to 120s so a wake within the 20s harness
    /// timeout is structurally a NOTIFY wake, never the fallback.
    #[tokio::test(flavor = "multi_thread")]
    async fn raced_waiter_completes_within_two_round_trips_of_release() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-raced-subscribe").await;
        let (path, nar) = make_path();
        let (fake, hits) =
            spawn_fake_upstream_counted(&path, nar.clone(), "cache.subscribe-1").await;
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

        // A live concurrent holder: heartbeat-fresh placeholder whose
        // claim survives every takeover arm (young + claimed).
        let sp = StorePath::parse(&path).unwrap();
        let hash = sp.sha256_digest();
        let holder_claim =
            metadata::insert_manifest_uploading_as(&db.pool, &hash, &path, &[], Some("holder-pod"))
                .await
                .unwrap()
                .expect("placeholder inserted");

        // Fallback pinned to 120s: any wake inside the 20s harness
        // timeout is structurally a NOTIFY wake (or the post-register
        // re-check), never the fallback poll.
        let sub = Arc::new(
            test_substituter(db.pool.clone()).with_raced_park(Duration::from_secs(120), None),
        );

        // The executor-plane caller: ONE subscribed call replaces the
        // pre-fix re-claim polling (the captured red: 250ms polls
        // against this fixture = 7 round-trips; the incident = 633).
        let waiter = {
            let sub = Arc::clone(&sub);
            let path = path.clone();
            tokio::spawn(async move {
                let cb: Box<SubstProgressFn> = Box::new(|_, _, _| {});
                sub.try_substitute_subscribed(tid, &path, &cb)
                    .await
                    .expect("subscribed substitution must succeed after release")
                    .expect("blocked placeholder must not read as a miss")
            })
        };

        // Let the waiter park, then release the holder's claim in
        // place (the owner-side stall-abort shape — claim_id NULL,
        // immediately claimable).
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let released = metadata::release_placeholder_in_place(&db.pool, &hash, holder_claim)
            .await
            .unwrap();
        assert!(released, "release must apply to the held placeholder");

        let info = tokio::time::timeout(Duration::from_secs(20), waiter)
            .await
            .expect("waiter must complete promptly after release (notify wake, not fallback)")
            .unwrap();
        assert_eq!(info.nar_size, nar.len() as u64);

        let round_trips = hits.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            round_trips <= 2,
            "raced waiter must complete within TWO upstream round-trips \
             (initial raced attempt + <=1 post-release attempt); \
             observed {round_trips} — the poll-racing shape is back"
        );
    }

    // r[verify store.substitute.raced-subscribe]
    /// The park-budget axis binds (R17 violation red): a holder that
    /// never releases bounds the subscribed waiter at the typed
    /// budget — `Err(Raced)` returns to the RetryLater backstop, and
    /// the fallback poll DROVE re-claims while parked (each fallback
    /// wake is a takeover-evaluating claim attempt, so a silently
    /// wedged holder is still reclaimed without any NOTIFY). Both
    /// axes count-witnessed: >=2 round-trips (the fallback re-claims
    /// ran) and bounded above (the budget stopped them).
    #[tokio::test(flavor = "multi_thread")]
    async fn raced_park_budget_bounds_the_wait_and_fallback_drives_reclaims() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-raced-budget").await;
        let (path, nar) = make_path();
        let (fake, hits) = spawn_fake_upstream_counted(&path, nar, "cache.subscribe-2").await;
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

        let sp = StorePath::parse(&path).unwrap();
        let hash = sp.sha256_digest();
        metadata::insert_manifest_uploading_as(&db.pool, &hash, &path, &[], Some("wedged-pod"))
            .await
            .unwrap()
            .expect("placeholder inserted");

        // fallback 100ms, budget 450ms → 1 initial + <=5 fallback
        // re-claims, then Raced.
        let sub = test_substituter(db.pool.clone())
            .with_raced_park(Duration::from_millis(100), Some(Duration::from_millis(450)));
        let cb: Box<SubstProgressFn> = Box::new(|_, _, _| {});
        let got = tokio::time::timeout(
            Duration::from_secs(10),
            sub.try_substitute_subscribed(tid, &path, &cb),
        )
        .await
        .expect("park must end at the budget, not hang");
        assert!(
            matches!(got, Err(SubstituteError::Raced)),
            "budget exhaustion must surface Raced to the backstop, got {got:?}"
        );
        let round_trips = hits.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            (2..=7).contains(&round_trips),
            "fallback re-claims must run while parked and stop at the \
             budget (expected 2..=7 round-trips for 450ms/100ms), \
             observed {round_trips}"
        );
    }

    // r[verify store.substitute.raced-subscribe]
    /// [`placeholder_blocked`] — the parked waiter's re-check decision
    /// table over the row states: held → blocked; released-in-place
    /// (claim_id NULL) → free; completed → free; absent → free. The
    /// lost-wakeup kill is register-then-re-check (code order) — this
    /// pins the re-check's DECISION so a wrong answer cannot park a
    /// waiter against an already-free slot for a full fallback slice.
    #[tokio::test]
    async fn placeholder_blocked_decision_table() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (path, _nar) = make_path();
        let sp = StorePath::parse(&path).unwrap();
        let hash = sp.sha256_digest();

        // Absent → free.
        assert!(!placeholder_blocked(&db.pool, &hash).await.unwrap());

        // Held ('uploading' + live claim) → blocked.
        let claim = metadata::insert_manifest_uploading_as(&db.pool, &hash, &path, &[], None)
            .await
            .unwrap()
            .expect("placeholder inserted");
        assert!(placeholder_blocked(&db.pool, &hash).await.unwrap());

        // Released-in-place (claim_id NULL, row survives) → free.
        assert!(
            metadata::release_placeholder_in_place(&db.pool, &hash, claim)
                .await
                .unwrap()
        );
        assert!(!placeholder_blocked(&db.pool, &hash).await.unwrap());

        // Completed → free (waiters re-check into AlreadyComplete).
        sqlx::query(
            "UPDATE manifests SET status = 'complete', claim_id = $2 WHERE store_path_hash = $1",
        )
        .bind(hash.as_slice())
        .bind(claim)
        .execute(&db.pool)
        .await
        .unwrap();
        assert!(!placeholder_blocked(&db.pool, &hash).await.unwrap());
    }

    // r[verify store.substitute.admission+2]
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
        /// Stream the NAR body as these frames instead of one buffer.
        /// Each frame after the first awaits `gate` when set (one
        /// `notify_one` per gated frame — Notify stores the permit, so
        /// pre-releasing is race-free), else a 3 ms pacing sleep keeps
        /// frames in separate TCP segments → separate reads (the W-3
        /// dribble/two-holder budget tests).
        nar_frames: Option<NarFrames>,
    }

    /// `(frames, per-frame gate)` for [`FlexCfg::nar_frames`].
    type NarFrames = (Vec<Vec<u8>>, Option<Arc<tokio::sync::Notify>>);

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
        let nar_frames = cfg.nar_frames;

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
                    let frames = nar_frames.clone();
                    async move {
                        nr_hits.fetch_add(1, Ordering::SeqCst);
                        if let Some(g) = gate {
                            g.notified().await;
                        }
                        if let Some((frames, frame_gate)) = frames {
                            let stream = futures_util::stream::unfold(
                                (frames.into_iter(), frame_gate, true),
                                |(mut it, frame_gate, first)| async move {
                                    let f = it.next()?;
                                    if !first {
                                        match &frame_gate {
                                            Some(g) => g.notified().await,
                                            None => {
                                                tokio::time::sleep(Duration::from_millis(3)).await;
                                            }
                                        }
                                    }
                                    Some((
                                        Ok::<_, std::io::Error>(axum::body::Bytes::from(f)),
                                        (it, frame_gate, false),
                                    ))
                                },
                            );
                            return axum::body::Body::from_stream(stream).into_response();
                        }
                        nar.into_response()
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
        // The identity reject is a NarInfo (class=Fetch) error: the
        // upstream served GARBAGE, which since merged_bug_044 is
        // recorded evidence — the iteration folds to `Errored` and
        // surfaces an UNCACHED Err(Fetch), never a cacheable
        // definitive miss. Assert the error AND that nothing was
        // ingested.
        let got = sub.try_substitute(tid, &path_b).await;
        assert!(
            matches!(got, Err(SubstituteError::Fetch(_))),
            "identity mismatch → uncached Err(Fetch), got {got:?}"
        );
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
    /// bug_240: the per-occurrence integrity counter derives class
    /// membership by CALLING the canonical table
    /// (`substitute_error_evidence(_).0 == Integrity`), never by
    /// re-enumerating variants — the TooLarge class (declared-NarSize
    /// over-cap lie, decompression bomb, oversized narinfo) is the
    /// most clearly hostile event here and the pre-fix
    /// `HashMismatch | SizeMismatch` matches! never ticked it.
    #[tokio::test]
    async fn integrity_counter_covers_toolarge_via_table() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "toolarge-tick").await;
        let path = rio_test_support::fixtures::test_store_path("toolarge-victim");
        let (nar, _nar_hash) = rio_test_support::fixtures::make_nar(b"toolarge-bytes");
        let hash_part = StorePath::parse(&path).unwrap().hash_part().to_string();
        // The upstream's narinfo LIES: NarSize over the decompressed
        // cap — try_upstream errors TooLarge before any claim/fetch.
        let lying = signed_narinfo_for(
            &path,
            &nar,
            "cache.toolarge",
            &hash_part,
            Some(MAX_NAR_SIZE + 1),
        );
        let upstream = spawn_flex_upstream(
            &path,
            nar.clone(),
            "cache.toolarge",
            FlexCfg {
                narinfo_override: Some(lying),
                ..Default::default()
            },
        )
        .await;
        metadata::upstreams::insert(
            &db.pool,
            tid,
            &upstream.url,
            50,
            std::slice::from_ref(&upstream.trusted_key),
            SigMode::Keep,
        )
        .await
        .unwrap();
        let sub = test_substituter(db.pool.clone());
        let recorder = rio_test_support::metrics::CountingRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let got = sub.try_substitute(tid, &path).await;
        assert!(
            got.is_err(),
            "an over-cap NarSize lie must error, got {got:?}"
        );
        let ticks: u64 = recorder
            .all_keys()
            .iter()
            .filter(|k| k.starts_with("rio_store_substitute_integrity_failures_total"))
            .map(|k| recorder.get(k))
            .sum();
        assert_eq!(
            ticks,
            1,
            "a TooLarge size-lie must tick the integrity counter (class \
             Integrity per the canonical table); keys={:?}",
            recorder.all_keys()
        );
    }

    /// merged_bug_016: result ticks are emitted INSIDE the
    /// singleflight leader — a pre-loop escape (bad store path →
    /// NarInfo) under coalescing ticks result="error" exactly ONCE,
    /// never once per coalesced caller (moka hands every waiter the
    /// same Arc<SubstituteError>; the pre-fix waiter-side map_err
    /// multiplied the tick by the coalescing width and mislabeled
    /// local trouble as upstream error exactly when operator
    /// attention peaked).
    #[tokio::test]
    async fn result_error_ticks_once_per_leader_under_coalescing() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "coalesce-tick").await;
        // Upstreams configured so the capability gate passes and the
        // leader reaches the parse (the pre-loop escape under test).
        let unused = rio_test_support::fixtures::test_store_path("coalesce-tick-unused");
        let upstream = spawn_fake_upstream(&unused, b"x".to_vec(), "coalesce-key").await;
        metadata::upstreams::insert(
            &db.pool,
            tid,
            &upstream.url,
            50,
            std::slice::from_ref(&upstream.trusted_key),
            SigMode::Keep,
        )
        .await
        .unwrap();
        let sub = test_substituter(db.pool.clone());
        let recorder = rio_test_support::metrics::CountingRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);
        // Invalid store path: StorePath::parse fails inside the
        // leader's do_substitute, before any upstream contact.
        let bad_path = "not-a-store-path";
        let (r1, r2, r3) = tokio::join!(
            sub.try_substitute(tid, bad_path),
            sub.try_substitute(tid, bad_path),
            sub.try_substitute(tid, bad_path)
        );
        assert!(r1.is_err() && r2.is_err() && r3.is_err());
        let error_ticks: u64 = recorder
            .all_keys()
            .iter()
            .filter(|k| k.starts_with("rio_store_substitute_total") && k.contains("result=error"))
            .map(|k| recorder.get(k))
            .sum();
        assert_eq!(
            error_ticks,
            1,
            "exactly one result=error tick per singleflight leader \
             (3 coalesced callers); keys={:?}",
            recorder.all_keys()
        );
    }

    #[tokio::test]
    async fn dedup_via_already_complete_no_redownload() {
        use rio_test_support::metrics::CountingRecorder;

        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-dedup").await;
        let (path, nar) = make_path();
        let nar_len = nar.len() as u64;
        let fake = spawn_flex_upstream(&path, nar.clone(), "cache.dedup", FlexCfg::default()).await;
        insert_flex(&db.pool, tid, &fake, 50).await;

        let sub = test_substituter(db.pool.clone());
        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);
        // First call ingests.
        sub.try_substitute(tid, &path).await.unwrap().unwrap();
        let baseline = fake.nar_hits.load(Ordering::SeqCst);
        assert_eq!(baseline, 1);
        assert_eq!(
            rec.get("rio_store_substitute_bytes_total{}"),
            nar_len,
            "the real ingest counts its bytes"
        );
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
        // merged_bug_091: the dedup hit ingested NOTHING — the bytes
        // counter must not move ("Bytes ingested" cannot be inflated
        // by zero-ingest dedup hits).
        assert_eq!(
            rec.get("rio_store_substitute_bytes_total{}"),
            nar_len,
            "dedup hit adds 0 to bytes ingested"
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
        assert!(first.hits.is_empty(), "503 → no hit");
        // r[verify sched.merge.substitute-probe-indeterminate+2]
        assert_eq!(
            first.indeterminate,
            vec![path.clone()],
            "503 → indeterminate, not silent confirmed-miss"
        );
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
        let got = sub.try_substitute(tid, &path).await;

        // Integrity garbage is recorded evidence (merged_bug_044):
        // the iteration folds to `Errored` → uncached Err(Fetch),
        // never a cacheable definitive miss.
        assert!(
            matches!(got, Err(SubstituteError::Fetch(_))),
            "size mismatch → uncached Err(Fetch); got {got:?}"
        );
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

        // claim_placeholder → AlreadyComplete → CONTENT BINDING
        // (merged_bug_114, r[store.substitute.content-binding]; typed
        // by merged_bug_046): the liar's NarSize disagrees with the
        // stored row, so the law is a TYPED CONTENT REFUSAL for that
        // upstream — no Hit, no signature append, and no cacheable
        // miss (the pre-m046 Ok(None) folded to CleanMiss, which the
        // content-blind HEAD confirmation contradicted into a
        // per-retry infrastructure charge; red captured on this pin:
        // "called `Result::unwrap()` on an `Err` value:
        //  ContentMismatch"). The honest-claim sig path is covered by
        // `already_complete_requires_content_agreement`'s positive
        // control.
        let got = sub_b.try_substitute(tid_b, &path).await;
        assert!(
            matches!(got, Err(SubstituteError::ContentMismatch)),
            "a NarSize-disagreeing AlreadyComplete claim must be a typed \
             content refusal for that upstream (content binding), got {got:?}"
        );
        // No NAR download on AlreadyComplete — binding runs pre-fetch.
        assert_eq!(
            liar.nar_hits.load(Ordering::SeqCst),
            0,
            "AlreadyComplete must not download"
        );
        // And NO signature was appended over the mismatched claim —
        // the stored row keeps exactly the honest ingest's signatures.
        let sigs: Vec<String> =
            sqlx::query_scalar("SELECT unnest(signatures) FROM narinfo WHERE store_path = $1")
                .bind(&path)
                .fetch_all(&db.pool)
                .await
                .unwrap();
        assert!(
            !sigs.iter().any(|s| s.starts_with("rio-ac-1:")),
            "no rio sig may be appended over a content-mismatched claim; got {sigs:?}"
        );
        let _ = cluster_seed;
    }

    // r[verify store.put.nar-bytes-budget+6]
    /// bug_070, re-aimed at the live_047/R-C reservation: the
    /// substitute leg MUST draw its whole-NAR reservation from
    /// `nar_bytes_budget` before the GET and credit it back after
    /// persist. Structural assertion: a leg whose reservation exceeds
    /// the available permits parks (backpressure) and completes once
    /// permits free; a completed leg leaves the budget untouched.
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
        let budget = crate::budget::NarBudget::new(initial);
        let sub = Arc::new(test_substituter(db.pool.clone()).with_nar_budget(budget.clone()));

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
        // a second leg CANNOT reserve without parking. The reservation
        // is `nar.len().max(MIN_NAR_CHUNK_CHARGE)`; leave fewer than
        // the floor available.
        let leave = (MIN_NAR_CHUNK_CHARGE as usize) - 1;
        // Drain through the sealed budget's delivery-priced face
        // (the test plays the role of in-flight trailer bytes; the
        // raw semaphore is module-private post-seal).
        let _hold = budget
            .acquire_chunk((initial - leave) as u32)
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
        // Give the leg time to reach the single-shot reservation and
        // park on `acquire_many_owned`.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !blocked.is_finished(),
            "the leg must park on its whole-NAR reservation when available \
             permits are below the reservation floor"
        );
        // Release: fetch completes.
        drop(_hold);
        let got2 = blocked.await.unwrap().unwrap().unwrap();
        assert_eq!(got2.nar_size, nar.len() as u64);
    }

    /// The census file universe: EVERY `.rs` under `rio-store/src`,
    /// embedded at compile time (the `fence_coverage.rs` hybrid — the
    /// nix gate runs test binaries without the source tree on disk, so
    /// a runtime-only walk fails there with NotFound; observed live at
    /// the bughunt-6 gate). Completeness against the live tree is
    /// pinned by [`census_universe_matches_live_tree`] on every
    /// dev-tree run of the same commit.
    const CENSUS_SOURCES: &[(&str, &str)] = &[
        ("admission.rs", include_str!("admission.rs")),
        ("authz.rs", include_str!("authz.rs")),
        ("backend.rs", include_str!("backend.rs")),
        ("budget.rs", include_str!("budget.rs")),
        ("cas.rs", include_str!("cas.rs")),
        ("chunker.rs", include_str!("chunker.rs")),
        ("config.rs", include_str!("config.rs")),
        ("error.rs", include_str!("error.rs")),
        ("gc/collect.rs", include_str!("gc/collect.rs")),
        ("gc/drain.rs", include_str!("gc/drain.rs")),
        ("gc/lane.rs", include_str!("gc/lane.rs")),
        ("gc/lock.rs", include_str!("gc/lock.rs")),
        ("gc/mark.rs", include_str!("gc/mark.rs")),
        (
            "gc/mark_scan_bench.rs",
            include_str!("gc/mark_scan_bench.rs"),
        ),
        ("gc/mod.rs", include_str!("gc/mod.rs")),
        ("gc/orphan.rs", include_str!("gc/orphan.rs")),
        ("gc/state.rs", include_str!("gc/state.rs")),
        ("gc/sweep.rs", include_str!("gc/sweep.rs")),
        ("gc/tenant.rs", include_str!("gc/tenant.rs")),
        ("grpc/admin.rs", include_str!("grpc/admin.rs")),
        ("grpc/chunk.rs", include_str!("grpc/chunk.rs")),
        ("grpc/get_path.rs", include_str!("grpc/get_path.rs")),
        ("grpc/mod.rs", include_str!("grpc/mod.rs")),
        (
            "grpc/put_path/common.rs",
            include_str!("grpc/put_path/common.rs"),
        ),
        ("grpc/put_path/mod.rs", include_str!("grpc/put_path/mod.rs")),
        (
            "grpc/put_path_batch.rs",
            include_str!("grpc/put_path_batch.rs"),
        ),
        ("grpc/queries.rs", include_str!("grpc/queries.rs")),
        ("grpc/sign.rs", include_str!("grpc/sign.rs")),
        ("ingest.rs", include_str!("ingest.rs")),
        ("lib.rs", include_str!("lib.rs")),
        ("logs/ack_census.rs", include_str!("logs/ack_census.rs")),
        ("logs/chunks.rs", include_str!("logs/chunks.rs")),
        ("logs/gate.rs", include_str!("logs/gate.rs")),
        ("logs/ingest.rs", include_str!("logs/ingest.rs")),
        ("logs/loss.rs", include_str!("logs/loss.rs")),
        ("logs/mbt_tests.rs", include_str!("logs/mbt_tests.rs")),
        ("logs/mod.rs", include_str!("logs/mod.rs")),
        ("logs/service.rs", include_str!("logs/service.rs")),
        ("logs/sessions.rs", include_str!("logs/sessions.rs")),
        ("logs/sweep.rs", include_str!("logs/sweep.rs")),
        ("logs/tail.rs", include_str!("logs/tail.rs")),
        ("main.rs", include_str!("main.rs")),
        ("manifest.rs", include_str!("manifest.rs")),
        (
            "materialize/client.rs",
            include_str!("materialize/client.rs"),
        ),
        (
            "materialize/executor.rs",
            include_str!("materialize/executor.rs"),
        ),
        ("materialize/mod.rs", include_str!("materialize/mod.rs")),
        ("metadata/chunked.rs", include_str!("metadata/chunked.rs")),
        (
            "metadata/cluster_key_history.rs",
            include_str!("metadata/cluster_key_history.rs"),
        ),
        ("metadata/inline.rs", include_str!("metadata/inline.rs")),
        ("metadata/mod.rs", include_str!("metadata/mod.rs")),
        ("metadata/queries.rs", include_str!("metadata/queries.rs")),
        (
            "metadata/tenant_keys.rs",
            include_str!("metadata/tenant_keys.rs"),
        ),
        (
            "metadata/upstreams.rs",
            include_str!("metadata/upstreams.rs"),
        ),
        ("realisations.rs", include_str!("realisations.rs")),
        ("signing.rs", include_str!("signing.rs")),
        ("substitute.rs", include_str!("substitute.rs")),
        ("test_helpers.rs", include_str!("test_helpers.rs")),
        ("visibility.rs", include_str!("visibility.rs")),
    ];

    /// Files whose ENTIRE content is non-production (gated by their
    /// parent module / feature, not by an in-file `mod tests`): their
    /// hits classify as the `test` plane wholesale.
    const CENSUS_WHOLE_FILE_NONPROD: &[(&str, &str)] = &[
        (
            "logs/mbt_tests.rs",
            "wired `#[cfg(test)] mod mbt_tests` (logs/mod.rs)",
        ),
        (
            "gc/mark_scan_bench.rs",
            "wired `#[cfg(test)] mod mark_scan_bench` (gc/mod.rs)",
        ),
        (
            "test_helpers.rs",
            "test-utils feature plane (the dev self-dependency)",
        ),
    ];

    /// Cut a source at its INLINE tests module (`#[cfg(test)]` line
    /// followed by a `mod … {` line with a body) — not at the first
    /// cfg(test) attribute (files carry cfg(test) consts mid-body) and
    /// not at gated module DECLARATIONS (`#[cfg(test)] mod x;`, which
    /// gc/mod.rs and logs/mod.rs carry mid-file; those files' bodies
    /// continue after the declaration). The `gc/lock.rs` census
    /// precedent, sharpened.
    fn census_production_half(src: &str) -> String {
        let lines: Vec<&str> = src.lines().collect();
        for i in 0..lines.len().saturating_sub(1) {
            let attr = lines[i].trim();
            let next = lines[i + 1].trim_start();
            if attr == "#[cfg(test)]" && next.starts_with("mod ") && next.contains('{') {
                return lines[..i].join("\n");
            }
        }
        src.to_string()
    }

    /// One classified needle family per line (first match wins;
    /// `try_` checked first so `try_acquire_many_owned` cannot
    /// double-count as the parking form). Split literals so a future
    /// reorg cannot make production code self-match this table.
    fn census_needle_class(line: &str) -> Option<&'static str> {
        let t = line.trim_start();
        if t.starts_with("//") {
            return None;
        }
        let needles: &[(&str, &'static str)] = &[
            (concat!("try_", "acquire"), "try_acquire"),
            (concat!("acquire_many_", "owned("), "acquire_many_owned"),
            (concat!(".acquire_", "many("), "acquire_many"),
            (concat!(".acquire_", "owned("), "acquire_owned"),
            (concat!(".acquire", "("), "acquire"),
            (concat!("add_", "permits("), "add_permits"),
            (concat!(".for", "get()"), "forget"),
        ];
        needles
            .iter()
            .find(|(n, _)| line.contains(n))
            .map(|(_, c)| *c)
    }

    /// Dev-tree completeness pin for [`CENSUS_SOURCES`]: the embedded
    /// universe equals the live `src/` tree EXACTLY (both directions),
    /// so a new source file fails the census until embedded — the
    /// acquire-site census's quantifier domain is generator-bounded,
    /// never author-bounded. In the nix sandbox (no source dir) the
    /// embedded scan is the same commit's content; the skip is
    /// disclosed, not silent (the `fence_coverage.rs` form).
    #[test]
    fn census_universe_matches_live_tree() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        if !root.exists() {
            eprintln!(
                "src/ not on disk (nix sandbox): universe pinned by the \
                 dev-tree run of this same commit"
            );
            return;
        }
        fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).expect("readable src dir") {
                let path = entry.expect("readable dir entry").path();
                if path.is_dir() {
                    walk(&path, root, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(
                        path.strip_prefix(root)
                            .expect("under root")
                            .to_str()
                            .expect("source paths are UTF-8")
                            .to_string(),
                    );
                }
            }
        }
        let mut live = Vec::new();
        walk(&root, &root, &mut live);
        live.sort();
        let mut embedded: Vec<String> = CENSUS_SOURCES.iter().map(|(f, _)| f.to_string()).collect();
        embedded.sort();
        assert_eq!(
            live, embedded,
            "census universe drifted from the live tree: add/remove the \
             named files in CENSUS_SOURCES so the acquire-site census sees \
             the whole crate in the nix sandbox too"
        );
    }

    // r[verify store.put.nar-bytes-budget+6]
    /// [GEN-SET] Budget acquire-site census — REBUILT crate-wide (bw8
    /// WO-S1-1; the RC-1 kill on the no-deadlock theorem's own
    /// quantifier domain). GENERATOR: this fn's scan over
    /// [`CENSUS_SOURCES`] (the whole-crate embedded universe, pinned
    /// to the live tree by [`census_universe_matches_live_tree`]) with
    /// the needle family in [`census_needle_class`] — never an
    /// author-typed file list (the prior form's four `include_str!`
    /// files would have admitted a single-permit `.acquire_owned()` on
    /// the budget handle, an `add_permits`/`forget` mint-or-leak site,
    /// or any acquire outside the four files — e.g. grpc/mod.rs, where
    /// the semaphore LIVES — into the pool invisibly, silently
    /// re-conditionalizing the UNCONDITIONAL theorem).
    ///
    /// Three censuses ride this one scan:
    /// - **Acquire-site:** every production acquire/mint/leak hit must
    ///   match a classification row below (pool + discipline); the
    ///   UNCLASSIFIED arm fails naming file/class/lines.
    /// - **Wait-discipline:** the NAR-budget pool's parking acquires
    ///   are EXACTLY {`reserve` (the one typed zero-holding park,
    ///   provably holding nothing by the `NarBudgetReservation` type),
    ///   `accumulate_chunk` (BUDGET_WAIT_GRACE-timed)} — pinned by the
    ///   two `nar-budget` rows' counts.
    /// - **Holder:** `held_permits` lives ONLY in the two ingest
    ///   drains (each enveloped at first grant; persist spans
    ///   enveloped at their finalize/stage chokepoints), and
    ///   `NarBudgetReservation` construction lives ONLY in
    ///   substitute.rs (envelope demanded at the constructor AND
    ///   enforced end-of-hold by the call-site tail timeout).
    ///
    /// Test-half hits (everything past the tests-module cut, plus the
    /// CENSUS_WHOLE_FILE_NONPROD planes) classify as the free `test`
    /// plane: R13's sanctioned lanes construct freely there, and the
    /// theorem quantifies over PRODUCTION acquire sites.
    #[test]
    fn nar_budget_acquire_site_census() {
        // (file, needle-class, expected production hits, classification)
        // — machine-derived: filled from this test's own failure output
        // at authoring (the committed-generator-output discipline), so
        // any drift in either direction fails with the live counts.
        const EXPECTED: &[(&str, &str, usize, &str)] = &[
            (
                "budget.rs",
                "acquire_many_owned",
                1,
                "NAR budget — THE declaration-priced acquisition \
                 (DeclaredCharge::new, merged_bug_005): the ONE constructor \
                 both reservation modes (substitute reserve + PutPath \
                 reserve_declared) call; cost axis (tenant, ledger, cap) \
                 REQUIRED by the signature before the zero-holding park",
            ),
            (
                "budget.rs",
                "acquire_many",
                3,
                "NAR budget — the delivery-priced debit face \
                 (NarBudget::acquire_chunk): trailer mode's per-chunk \
                 accrual, BUDGET_WAIT_GRACE-timed grant-or-typed-shed at \
                 its one chokepoint (accumulate_chunk). THREE textual \
                 sites, one face (merged_bug_101): the legacy no-lane \
                 arm plus the two select arms of the chunk-lane fairness \
                 split (declared face / chunk lane — the ordering axis, \
                 store.budget.lane-fairness). Post-seal the raw \
                 semaphores are module-private to crate::budget — BOTH \
                 debit faces live here and ONLY here \
                 (store.put.declared-reserve, store.budget.cost-axis)",
            ),
            (
                "admission.rs",
                "acquire_owned",
                1,
                "admission gate semaphore (sibling pool; SUBSTITUTE_ADMISSION_WAIT-timed)",
            ),
            (
                "logs/service.rs",
                "try_acquire",
                2,
                "logs stream-gate + byte_budget (sibling pools; non-parking \
                 try_ forms, typed shed on None)",
            ),
            (
                "materialize/executor.rs",
                "acquire_owned",
                1,
                "executor slot pool (sibling pool; baseline parking acquire \
                 under the executor's own laws)",
            ),
            (
                "materialize/executor.rs",
                "try_acquire",
                2,
                "executor slot pool — surplus try_ (non-parking): try_widen \
                 + try_admit_claim (the bw8 slot ≺ claim admission gate — \
                 leftover-only per the yield law; zero NAR-budget contact)",
            ),
            (
                "gc/lock.rs",
                "acquire",
                1,
                "sqlx PgPool connection acquire (SessionConn::acquire's one \
                 sanctioned site; not a tokio semaphore)",
            ),
            (
                "gc/lock.rs",
                "try_acquire",
                1,
                "PgSessionLock::try_acquire fn decl (PG advisory lock API; \
                 non-blocking; not a tokio semaphore)",
            ),
            (
                "gc/collect.rs",
                "try_acquire",
                1,
                "GcCycleLease::try_acquire call (PG advisory lease; non-blocking)",
            ),
            (
                "gc/mod.rs",
                "try_acquire",
                1,
                "GcCycleLease::try_acquire call (PG advisory lease; non-blocking)",
            ),
            (
                "gc/state.rs",
                "try_acquire",
                2,
                "GcCycleLease::try_acquire decl + PgSessionLock::try_acquire \
                 call (PG advisory lease plane; non-blocking)",
            ),
        ];

        use std::collections::BTreeMap;
        let mut observed: BTreeMap<(String, &'static str), Vec<usize>> = BTreeMap::new();
        for (file, src) in CENSUS_SOURCES {
            let scan_src = if CENSUS_WHOLE_FILE_NONPROD.iter().any(|(f, _)| f == file) {
                String::new() // whole file is the test/bench plane — free.
            } else {
                census_production_half(src)
            };
            for (idx, line) in scan_src.lines().enumerate() {
                if let Some(class) = census_needle_class(line) {
                    observed
                        .entry((file.to_string(), class))
                        .or_default()
                        .push(idx + 1);
                }
            }
        }

        let mut violations = Vec::new();
        for ((file, class), lines) in &observed {
            match EXPECTED.iter().find(|(f, c, _, _)| f == file && c == class) {
                None => violations.push(format!(
                    "UNCLASSIFIED: {file} [{class}] at lines {lines:?} — a new \
                     acquire/mint/leak site joined the census; classify it \
                     (which pool? parking or try_? enveloped or zero-holding?) \
                     and, if it touches the NAR budget, extend the envelope \
                     enforcement story before re-pinning"
                )),
                Some((_, _, n, _)) if *n != lines.len() => violations.push(format!(
                    "COUNT DRIFT: {file} [{class}] expected {n}, found {} at \
                     lines {lines:?}",
                    lines.len()
                )),
                Some(_) => {}
            }
        }
        for (file, class, n, _) in EXPECTED {
            if !observed.contains_key(&(file.to_string(), *class)) {
                violations.push(format!(
                    "VANISHED: {file} [{class}] expected {n} production hits, \
                     found none — the census row is stale"
                ));
            }
        }
        assert!(
            violations.is_empty(),
            "acquire-site census violations:\n{}",
            violations.join("\n")
        );

        // Holder census riders on the same scan: `held_permits` only in
        // the two ingest drains; `NarBudgetReservation` constructed only
        // in substitute.rs (common.rs/put_path_batch.rs reference the
        // envelope type, never the reservation).
        let held = concat!("held_", "permits");
        let resv = concat!("NarBudget", "Reservation");
        for (file, src) in CENSUS_SOURCES {
            let prod = if CENSUS_WHOLE_FILE_NONPROD.iter().any(|(f, _)| f == file) {
                String::new()
            } else {
                census_production_half(src)
            };
            let held_hits = prod
                .lines()
                .filter(|l| !l.trim_start().starts_with("//") && l.contains(held))
                .count();
            let resv_hits = prod
                .lines()
                .filter(|l| !l.trim_start().starts_with("//") && l.contains(resv))
                .count();
            match *file {
                // The two drains (envelope at first grant) + the
                // single-path handler frame that carries the drain's
                // returned permits through `finalize_single`'s
                // enveloped persist span.
                "grpc/put_path/common.rs" | "grpc/put_path_batch.rs" | "grpc/put_path/mod.rs" => {}
                _ => assert_eq!(
                    held_hits, 0,
                    "{file}: held_permits outside the two ingest drains (+ \
                     the single-path handler frame) — a new budget HOLDER \
                     joined the census; envelope it"
                ),
            }
            if *file != "substitute.rs" {
                assert_eq!(
                    resv_hits, 0,
                    "{file}: NarBudgetReservation outside substitute.rs — a \
                     new reservation site joined the census; the envelope \
                     constructor demand + tail enforcement must follow"
                );
            }
        }
    }

    // r[verify store.budget.cost-axis]
    /// W10-B (R22′ planted red, merged_bug_005): a strawman acquirer
    /// BYPASSING the sealed `DeclaredCharge`/`acquire_chunk` faces is
    /// seen by the SAME scan the production census runs — planted at
    /// the SCAN layer (the raw-source needle pass over the
    /// production-half cut), not post-extraction. A scanned source
    /// carrying this hit with no EXPECTED row is exactly the
    /// UNCLASSIFIED reject `nar_budget_acquire_site_census` emits
    /// (demonstrated live: the commit-1 hoist and the commit-2 seal
    /// each tripped it before their EXPECTED re-pins). Post-seal the
    /// bypass cannot even compile outside `crate::budget` (the
    /// semaphore is module-private); this plant is the census BELT
    /// under the compile seal — an in-module regression or a new raw
    /// pool still surfaces.
    #[test]
    fn nar_budget_acquirer_census_flags_strawman_bypass() {
        // The exact wave-9 bare-sibling shape (ef960dca5): a whole
        // wire-supplied charge acquired with no ledger consult.
        let strawman_src = "\
fn rogue_reserve(budget: &std::sync::Arc<tokio::sync::Semaphore>, declared: u64) {
    let _permit = budget.acquire_many(declared as u32);
}
";
        let prod = census_production_half(strawman_src);
        let hits: Vec<&'static str> = prod.lines().filter_map(census_needle_class).collect();
        assert_eq!(
            hits,
            vec!["acquire_many"],
            "the scan layer must see the strawman's bare acquire"
        );
        // Comment-camouflage control: the same shape disarmed into a
        // comment is NOT a hit (the needle pass runs on code, so the
        // production census's zero-noise property holds), while the
        // code form above IS — the pair pins the scan's polarity.
        let camouflaged = "// let _permit = budget.acquire_many(declared as u32);\n";
        assert_eq!(
            census_production_half(camouflaged)
                .lines()
                .filter_map(census_needle_class)
                .count(),
            0,
            "a commented-out acquire is not an acquirer"
        );
    }

    // r[verify store.put.nar-bytes-budget+6]
    /// W-3 leg (a), live_047/R-C: two concurrent substitute legs whose
    /// sizes sum past the budget MUST both complete — the single-shot
    /// whole-NAR reservation means a budget waiter holds ZERO permits,
    /// so the fair-FIFO head is always admissible once holders drain.
    /// Pre-fix (incremental per-read acquire) both legs held their
    /// frame-1 permits and parked on frame-2 forever: the
    /// hold-and-wait wedge (§1.3 F1 honest regime, scaled down).
    #[tokio::test]
    async fn budget_two_holders_past_budget_complete_without_wedge() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-wedge2").await;

        // One NAR in (512, 1024]: each leg fits the 1024-permit budget
        // alone; two frame-1 holds (400 each) leave 224 free — less
        // than any frame-2 charge (floor 256), so the incremental
        // regime wedges deterministically once both hold frame 1.
        let content = vec![0x5au8; 600];
        let (nar, _h) = rio_test_support::fixtures::make_nar(&content);
        assert!(
            nar.len() > 512 && nar.len() <= 1024,
            "fixture sizing drifted: {}",
            nar.len()
        );
        let budget_size = 1024usize;

        let path_a = rio_test_support::fixtures::test_store_path("wedge-a");
        let path_b = rio_test_support::fixtures::test_store_path("wedge-b");
        let gate_a = Arc::new(tokio::sync::Notify::new());
        let gate_b = Arc::new(tokio::sync::Notify::new());
        let fake_a = spawn_flex_upstream(
            &path_a,
            nar.clone(),
            "cache.wedge",
            FlexCfg {
                nar_frames: Some((
                    vec![nar[..400].to_vec(), nar[400..].to_vec()],
                    Some(gate_a.clone()),
                )),
                ..Default::default()
            },
        )
        .await;
        let fake_b = spawn_flex_upstream(
            &path_b,
            nar.clone(),
            "cache.wedge",
            FlexCfg {
                nar_frames: Some((
                    vec![nar[..400].to_vec(), nar[400..].to_vec()],
                    Some(gate_b.clone()),
                )),
                ..Default::default()
            },
        )
        .await;
        insert_flex(&db.pool, tid, &fake_a, 50).await;
        insert_flex(&db.pool, tid, &fake_b, 60).await;

        let budget = crate::budget::NarBudget::new(budget_size);
        let sub = test_substituter(db.pool.clone()).with_nar_budget(budget.clone());
        let http = sub.http.as_ref().unwrap();
        let ups = metadata::upstreams::list_for_tenant(&db.pool, tid)
            .await
            .unwrap();
        let up_a = ups.iter().find(|u| u.url == fake_a.url).unwrap();
        let up_b = ups.iter().find(|u| u.url == fake_b.url).unwrap();
        let hp_a = StorePath::parse(&path_a).unwrap().hash_part().to_string();
        let hp_b = StorePath::parse(&path_b).unwrap().hash_part().to_string();

        let drive = async {
            tokio::join!(
                sub.try_upstream(http, tid, up_a, &path_a, &hp_a, None),
                sub.try_upstream(http, tid, up_b, &path_b, &hp_b, None),
            )
        };
        tokio::pin!(drive);

        // Drive both legs until the budget shows the hold point:
        // pre-fix = both legs hold frame-1 permits (224 free);
        // post-fix = one whole-NAR reservation held (1024 − len free).
        // A single pre-fix frame-1 hold leaves 624 free > the floor,
        // so the probe cannot fire early.
        let probe_floor = budget_size - nar.len();
        let mut done = None;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if budget.available_permits() <= probe_floor {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "legs never reached the budget hold point; available={}",
                budget.available_permits()
            );
            tokio::select! {
                r = &mut drive => { done = Some(r); break; }
                () = tokio::time::sleep(Duration::from_millis(20)) => {}
            }
        }
        // Release frame 2 on both legs (Notify stores the permit, so
        // releasing the not-yet-started leg is race-free).
        gate_a.notify_one();
        gate_b.notify_one();
        let (ra, rb) = match done {
            Some(r) => r,
            None => match tokio::time::timeout(Duration::from_secs(15), drive).await {
                Ok(r) => r,
                Err(_) => panic!(
                    "NAR-budget wedge: two in-flight substitute legs (sizes {}+{} > \
                     budget {}) parked forever — each holds its accumulated permits \
                     while waiting for the next read's; available_permits={} \
                     (hold-and-wait, no release ever)",
                    nar.len(),
                    nar.len(),
                    budget_size,
                    budget.available_permits()
                ),
            },
        };
        let ha = ra.expect("leg A must complete");
        let hb = rb.expect("leg B must complete");
        assert!(matches!(ha, UpstreamOutcome::Hit { .. }), "got {ha:?}");
        assert!(matches!(hb, UpstreamOutcome::Hit { .. }), "got {hb:?}");
        assert_eq!(
            budget.available_permits(),
            budget_size,
            "all permits credited back after both legs complete"
        );
    }

    // r[verify store.put.nar-bytes-budget+6]
    /// W-3 leg (b) — THE K=1 charge-amplification closure
    /// (live_047/R-C §1.3 F1, adversarial regime): ONE dribbling
    /// stream (1-byte reads, raw bytes ≪ budget) MUST complete against
    /// a budget that admits its declared size, because the leg's
    /// entire cumulative charged demand IS its single reservation
    /// (`declared.max(MIN_NAR_CHUNK_CHARGE)`). Pre-fix each 1-byte
    /// read charged MIN_NAR_CHUNK_CHARGE (256) with NO cumulative
    /// charged tracker — 256× amplification: 8 reads charge the whole
    /// 2048-permit budget and the 9th parks forever (outside the
    /// stall watchdog, BudgetParked / takeover-exempt) — a whole-pod
    /// ingest wedge from one tenant-controlled stream.
    #[tokio::test]
    async fn budget_dribbling_stream_cannot_wedge_alone() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-dribble").await;
        let (nar, _h) = rio_test_support::fixtures::make_nar(b"k1-dribble-amplification");
        let budget_size = 2048usize;
        assert!(
            nar.len() >= 9 && nar.len() < budget_size / 2,
            "fixture sizing drifted: {}",
            nar.len()
        );
        // 1-byte frames with 3 ms pacing: every read observes one byte.
        let frames: Vec<Vec<u8>> = nar.iter().map(|b| vec![*b]).collect();
        let path = rio_test_support::fixtures::test_store_path("dribble");
        let fake = spawn_flex_upstream(
            &path,
            nar.clone(),
            "cache.dribble",
            FlexCfg {
                nar_frames: Some((frames, None)),
                ..Default::default()
            },
        )
        .await;
        insert_flex(&db.pool, tid, &fake, 50).await;

        let budget = crate::budget::NarBudget::new(budget_size);
        let sub = test_substituter(db.pool.clone()).with_nar_budget(budget.clone());
        let http = sub.http.as_ref().unwrap();
        let ups = metadata::upstreams::list_for_tenant(&db.pool, tid)
            .await
            .unwrap();
        let hp = StorePath::parse(&path).unwrap().hash_part().to_string();

        let got = tokio::time::timeout(
            Duration::from_secs(20),
            sub.try_upstream(http, tid, &ups[0], &path, &hp, None),
        )
        .await
        .unwrap_or_else(|_| {
            panic!(
                "K=1 wedge: one dribbling stream (raw NAR {} bytes) charged the \
                 whole {}-permit budget through the per-read floor and parked on \
                 its own next acquire; available_permits={} — charged-vs-raw \
                 amplification up to 256×, RSS far below any OOM threshold",
                nar.len(),
                budget_size,
                budget.available_permits()
            )
        });
        let hit = got.expect("dribbled substitute must complete");
        assert!(matches!(hit, UpstreamOutcome::Hit { .. }), "got {hit:?}");
        assert_eq!(
            budget.available_permits(),
            budget_size,
            "the single reservation is credited back after persist"
        );
    }

    // r[verify store.put.nar-bytes-budget+6]
    /// W-6a reservation-residency: cancelling the substitute leg at
    /// the hash-verify detach point MUST NOT credit the budget back
    /// while the DETACHED blocking task (tokio never cancels blocking
    /// tasks) still owns the NAR bytes — the reservation rides the
    /// bytes INTO the `spawn_blocking` closure and is credited back
    /// only when the hash task's output drops. Without the coupling
    /// the permits lived in the dropped future's frame: budget freed
    /// while the full buffer was still resident in the detached task
    /// (§3.2 step 4 — the regime the abort-latch cancellation makes
    /// steady-state at F>1).
    #[tokio::test]
    async fn budget_reservation_rides_the_hash_bytes_across_cancel() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-hashres").await;
        let (path, nar) = make_path();
        let fake =
            spawn_flex_upstream(&path, nar.clone(), "cache.hashres", FlexCfg::default()).await;
        insert_flex(&db.pool, tid, &fake, 50).await;

        let budget_size = 4096usize;
        let budget = crate::budget::NarBudget::new(budget_size);
        let (gate, mut entered) = HashGate::new();
        let sub = Arc::new(
            test_substituter(db.pool.clone())
                .with_nar_budget(budget.clone())
                .with_hash_gate(gate.clone()),
        );
        let reserve = (nar.len() as u64).max(MIN_NAR_CHUNK_CHARGE as u64) as usize;

        let sub2 = Arc::clone(&sub);
        let p = path.clone();
        let task = tokio::spawn(async move { sub2.try_substitute(tid, &p).await });
        entered.recv().await.expect("hash task entered");
        assert_eq!(
            budget.available_permits(),
            budget_size - reserve,
            "the reservation is held while the hash task owns the bytes"
        );
        // Cancel the leg at the hash-verify detach point.
        task.abort();
        let _ = task.await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Snapshot BEFORE releasing the gate (the release must happen
        // unconditionally or the parked blocking thread outlives the
        // test runtime), then assert on the snapshot.
        let resident = budget.available_permits();
        // Let the detached hash finish: its output (bytes + reservation
        // + digest) drops inside tokio, crediting the budget back.
        gate.release();
        assert_eq!(
            resident,
            budget_size - reserve,
            "budget credited back while the detached hash task still owns the \
             NAR bytes — the reservation must ride the bytes through \
             spawn_blocking, not drop with the cancelled future"
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while budget.available_permits() != budget_size {
            assert!(
                tokio::time::Instant::now() < deadline,
                "reservation never credited back after the hash task completed"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
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

    // =====================================================================
    // NAR-hold envelope battery (bw8 WO-S1-1: merged_bug_021 + the
    // hold edges of merged_bug_001). Shrunk envelopes ride the
    // sanctioned builder overrides (`with_stall_window`,
    // `with_nar_hold_floor_rate`) — the R17 violability lane;
    // assertions are structural with ≥10× slack on shrunk bounds.
    // =====================================================================

    /// Drive ONE production upstream leg (the same `try_upstream` form
    /// as the W-3 battery — `do_substitute`'s fold maps the typed
    /// abort onto the Stalled class for fail-over, so the per-upstream
    /// seam is where the typed variant is observable).
    async fn drive_upstream(
        sub: &Substituter,
        tid: Uuid,
        path: &str,
    ) -> Result<UpstreamOutcome, SubstituteError> {
        let http = sub.http.as_ref().unwrap();
        let ups = metadata::upstreams::list_for_tenant(&sub.pool, tid)
            .await
            .unwrap();
        // Each battery tenant seeds exactly ONE upstream.
        assert_eq!(ups.len(), 1, "battery contract: one upstream per tenant");
        let hp = StorePath::parse(path).unwrap().hash_part().to_string();
        sub.try_upstream(http, tid, &ups[0], path, &hp, None).await
    }

    // r[verify store.put.nar-hold-envelope+2]
    // W8-B (R16 statement): a reservation older than its typed
    // deadline cannot exist — abort + credit-back observed under an
    // adversarial trickle that satisfies every per-read clock, never
    // over-delivers, and never EOFs (the exact merged_bug_021 evasion
    // shape), with a parked sibling then completing.
    /// RED pre-fix (verbatim, run at 83e596f0c): `left: leg still
    /// holding at 3× the would-be deadline (5×stall + declared/
    /// floor-rate ≈ 1.5s): a 150ms 1-byte trickle resets the per-read
    /// stall clock forever, advances store_bytes so the takeover
    /// predicate never strikes, never over-delivers, never EOFs;
    /// available_permits == 0; parked sibling never granted / right:
    /// HoldDeadlineExceeded ≤ deadline+slack; permits restored;
    /// sibling completes`.
    #[tokio::test]
    async fn trickle_hold_aborts_at_the_transfer_deadline() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "bw8s1-r2").await;
        let tid_sib = seed_tenant(&db.pool, "bw8s1-r2-sib").await;
        let content = vec![0x6bu8; 64];
        let (nar, _h) = rio_test_support::fixtures::make_nar(&content);
        let path = rio_test_support::fixtures::test_store_path("bw8s1-trickle");
        let gate = Arc::new(tokio::sync::Notify::new());
        let frames: Vec<Vec<u8>> = nar.iter().map(|b| vec![*b]).collect();
        let n_frames = frames.len();
        let fake = spawn_flex_upstream(
            &path,
            nar.clone(),
            "cache.bw8r2",
            FlexCfg {
                nar_frames: Some((frames, Some(gate.clone()))),
                ..Default::default()
            },
        )
        .await;
        insert_flex(&db.pool, tid, &fake, 50).await;

        // Pool admits exactly one whole-NAR reservation. Envelope
        // (shrunk via the builder lane): stall 300ms → deadline =
        // 5×300ms + len/256KiB/s ≈ 1.5s.
        let stall = Duration::from_millis(300);
        let charge = (nar.len() as u64).max(MIN_NAR_CHUNK_CHARGE as u64) as usize;
        let budget = crate::budget::NarBudget::new(charge);
        let sub = Arc::new(
            test_substituter(db.pool.clone())
                .with_nar_budget(budget.clone())
                .with_stall_window(stall),
        );
        let deadline_bound = stall * NAR_HOLD_GRACE_FACTOR
            + Duration::from_secs(nar.len() as u64 / NAR_HOLD_FLOOR_RATE);

        // Trickle driver: one 1-byte frame per 150ms — inside every
        // per-read stall window; the per-read clock NEVER fires.
        let releaser = {
            let g = gate.clone();
            tokio::spawn(async move {
                for _ in 0..n_frames {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    g.notify_one();
                }
            })
        };
        let leg = {
            let s = Arc::clone(&sub);
            let p = path.clone();
            tokio::spawn(async move { drive_upstream(&s, tid, &p).await })
        };
        // Sibling parks on the exhausted pool (zero-holding head).
        let path2 = rio_test_support::fixtures::test_store_path("bw8s1-sib");
        let (nar2, _h2) = rio_test_support::fixtures::make_nar(b"sib");
        let fake2 =
            spawn_flex_upstream(&path2, nar2.clone(), "cache.bw8r2", FlexCfg::default()).await;
        insert_flex(&db.pool, tid_sib, &fake2, 50).await;
        let wait = tokio::time::Instant::now() + Duration::from_secs(5);
        while budget.available_permits() > 0 {
            assert!(
                tokio::time::Instant::now() < wait,
                "trickle leg never reserved"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let sib = {
            let s = Arc::clone(&sub);
            let p = path2.clone();
            tokio::spawn(async move { drive_upstream(&s, tid_sib, &p).await })
        };

        // The hold MUST abort typed within 10× the derived deadline.
        let got = tokio::time::timeout(deadline_bound * 10, leg)
            .await
            .expect("hold deadline must fire — the trickle satisfies every per-read window")
            .unwrap();
        assert!(
            matches!(
                got,
                Err(SubstituteError::HoldDeadlineExceeded { declared, .. })
                    if declared == nar.len() as u64
            ),
            "typed envelope abort expected, got {got:?}"
        );
        // Credit-back: the parked sibling is granted and completes.
        let sib_got = tokio::time::timeout(Duration::from_secs(15), sib)
            .await
            .expect("sibling must complete once the expired hold credits back")
            .unwrap()
            .expect("sibling leg ok");
        assert!(
            matches!(sib_got, UpstreamOutcome::Hit { .. }),
            "{sib_got:?}"
        );
        releaser.abort();
        // Pool fully restored after both legs settle.
        let restore = tokio::time::Instant::now() + Duration::from_secs(5);
        while budget.available_permits() != charge {
            assert!(
                tokio::time::Instant::now() < restore,
                "permits not restored: {} of {charge}",
                budget.available_permits()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    // r[verify store.put.nar-hold-envelope+2]
    // W8-B′ (R16 statement): a black-holed persist cannot hold a
    // reservation past the deadline — the holder parked in the persist
    // span releases by the hold deadline with permits restored and the
    // parked sibling completing (the post-read tail is ENFORCED, not
    // premised: rio-common's S3 client ships no TimeoutConfig, Q-108).
    /// RED pre-fix (verbatim, run at 83e596f0c; the pre-fix park used
    /// the hash gate — the only pre-fix-expressible post-read seam,
    /// same population: a holder past the read loop): `left:
    /// reservation still held at 3× the would-be deadline — the
    /// post-read span (hash→sigs→persist) has no clock and the claim
    /// phase is exempt from every watchdog; available_permits == 0 of
    /// 256 / right: HoldDeadlineExceeded ≤ deadline+slack; permits
    /// restored; parked sibling completes`.
    #[tokio::test]
    async fn blackholed_persist_releases_by_the_hold_deadline() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "bw8s1-r2b").await;
        let tid_sib = seed_tenant(&db.pool, "bw8s1-r2b-sib").await;
        let (path, nar) = make_path();
        let fake =
            spawn_flex_upstream(&path, nar.clone(), "cache.bw8r2b", FlexCfg::default()).await;
        insert_flex(&db.pool, tid, &fake, 50).await;
        let stall = Duration::from_millis(100);
        let charge = (nar.len() as u64).max(MIN_NAR_CHUNK_CHARGE as u64) as usize;
        let budget = crate::budget::NarBudget::new(charge);
        let persist_gate = Arc::new(tokio::sync::Notify::new());
        let sub = Arc::new(
            test_substituter(db.pool.clone())
                .with_nar_budget(budget.clone())
                .with_stall_window(stall)
                .with_persist_gate(persist_gate.clone()),
        );
        let deadline_bound = stall * NAR_HOLD_GRACE_FACTOR
            + Duration::from_secs(nar.len() as u64 / NAR_HOLD_FLOOR_RATE);

        let leg = {
            let s = Arc::clone(&sub);
            let p = path.clone();
            tokio::spawn(async move { drive_upstream(&s, tid, &p).await })
        };
        // Sibling parks behind the persist-parked holder.
        let path2 = rio_test_support::fixtures::test_store_path("bw8s1-r2b-sib");
        let (nar2, _h2) = rio_test_support::fixtures::make_nar(b"sib2");
        let fake2 =
            spawn_flex_upstream(&path2, nar2.clone(), "cache.bw8r2b", FlexCfg::default()).await;
        insert_flex(&db.pool, tid_sib, &fake2, 50).await;
        let wait = tokio::time::Instant::now() + Duration::from_secs(5);
        while budget.available_permits() > 0 {
            assert!(tokio::time::Instant::now() < wait, "leg never reserved");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // The sibling rides the SAME pool through its own ungated
        // Substituter (the gate models ONE leg's black-holed backend;
        // the pool is the shared object under test).
        let sub_sib = Arc::new(
            test_substituter(db.pool.clone())
                .with_nar_budget(budget.clone())
                .with_stall_window(stall),
        );
        let sib = {
            let s = Arc::clone(&sub_sib);
            let p = path2.clone();
            tokio::spawn(async move { drive_upstream(&s, tid_sib, &p).await })
        };

        // The persist park (never-notified gate) is cancelled by the
        // tail timeout: typed abort within 10× the deadline.
        let got = tokio::time::timeout(deadline_bound * 10, leg)
            .await
            .expect("the tail timeout must cancel the black-holed persist")
            .unwrap();
        assert!(
            matches!(got, Err(SubstituteError::HoldDeadlineExceeded { .. })),
            "typed envelope abort expected, got {got:?}"
        );
        let sib_got = tokio::time::timeout(Duration::from_secs(15), sib)
            .await
            .expect("sibling must complete once the expired hold credits back")
            .unwrap()
            .expect("sibling leg ok");
        assert!(
            matches!(sib_got, UpstreamOutcome::Hit { .. }),
            "{sib_got:?}"
        );
        let restore = tokio::time::Instant::now() + Duration::from_secs(5);
        while budget.available_permits() != charge {
            assert!(
                tokio::time::Instant::now() < restore,
                "permits not restored: {} of {charge}",
                budget.available_permits()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// bug_108 census (R22 fold-site axis): the posture hop is TOTAL
    /// by rustc exhaustiveness (the generator — no `_ =>` arm exists
    /// at the decision site), and the strike-parity cell the defect
    /// silently dropped is pinned: the hold-deadline expiry carries
    /// EXACTLY the per-read stall's posture (the in-file classifier's
    /// own sentence, now true at the cleanup consumer too).
    #[test]
    fn cleanup_posture_is_total_and_strike_parity() {
        use std::time::Duration;
        assert_eq!(
            cleanup_posture(&SubstituteError::HoldDeadlineExceeded {
                declared: 1,
                hold_budget: Duration::from_secs(1),
            }),
            CleanupPosture::StrikeReleaseInPlace,
            "the envelope abort must keep the strike plane (bug_108)"
        );
        assert_eq!(
            cleanup_posture(&SubstituteError::Stalled {
                window: Duration::from_secs(1)
            }),
            CleanupPosture::StrikeReleaseInPlace,
        );
        // Representative Abort cells across the non-stall families
        // (integrity / refusal / transient): the posture fn names
        // every variant — a new variant fails compilation there, not
        // silently here.
        assert_eq!(
            cleanup_posture(&SubstituteError::Raced),
            CleanupPosture::Abort
        );
        assert_eq!(
            cleanup_posture(&SubstituteError::HashMismatch {
                expected: String::new(),
                got: String::new(),
            }),
            CleanupPosture::Abort
        );
        assert_eq!(
            cleanup_posture(&SubstituteError::Ingest(String::new())),
            CleanupPosture::Abort
        );
    }

    // r[verify store.substitute.stall-abort+2]
    // W9-BJ (R16 statement): a hold-deadline abort STRIKES + emits +
    // leaves the row reclaimable — full parity with the per-read
    // stall (the classifier's claim as the oracle). Driven through
    // the production persist-gate black hole (the same machinery as
    // W8-B′), then the row state and the immediate re-claim are
    // asserted durably.
    /// RED pre-fix (verbatim in the landing commit): the deadline
    /// abort fell to the generic delete arm — the manifests row was
    /// GONE (count 0), no strike survived, and the re-claim went
    /// through a fresh insert at `stall_count = 0`.
    #[tokio::test]
    async fn hold_deadline_abort_strikes_and_leaves_the_row_reclaimable() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "bw9s5-bj").await;
        let (path, nar) = make_path();
        let fake = spawn_flex_upstream(&path, nar.clone(), "cache.bw9bj", FlexCfg::default()).await;
        insert_flex(&db.pool, tid, &fake, 50).await;
        let stall = Duration::from_millis(100);
        let charge = (nar.len() as u64).max(MIN_NAR_CHUNK_CHARGE as u64) as usize;
        let budget = crate::budget::NarBudget::new(charge);
        let persist_gate = Arc::new(tokio::sync::Notify::new());
        let sub = Arc::new(
            test_substituter(db.pool.clone())
                .with_nar_budget(budget.clone())
                .with_stall_window(stall)
                .with_persist_gate(persist_gate.clone()),
        );
        let deadline_bound = stall * NAR_HOLD_GRACE_FACTOR
            + Duration::from_secs(nar.len() as u64 / NAR_HOLD_FLOOR_RATE);

        let got = tokio::time::timeout(deadline_bound * 10, {
            let s = Arc::clone(&sub);
            let p = path.clone();
            async move { drive_upstream(&s, tid, &p).await }
        })
        .await
        .expect("the hold deadline must cancel the black-holed persist");
        assert!(
            matches!(got, Err(SubstituteError::HoldDeadlineExceeded { .. })),
            "typed envelope abort expected, got {got:?}"
        );

        // THE bug_108 assertions: the row SURVIVES released-in-place
        // with the strike recorded — not deleted.
        let sp_hash: [u8; 32] = sha2::Sha256::digest(path.as_bytes()).into();
        let row: Option<(String, Option<uuid::Uuid>, i16)> = sqlx::query_as(
            "SELECT status, claim_id, stall_count FROM manifests WHERE store_path_hash = $1",
        )
        .bind(&sp_hash[..])
        .fetch_optional(&db.pool)
        .await
        .expect("row query");
        let (status, claim_id, stall_count) = row.expect(
            "the manifests row must SURVIVE a hold-deadline abort \
             (released in place, not deleted) — bug_108's lost arm",
        );
        assert_eq!(status, "uploading");
        assert!(claim_id.is_none(), "claim released in place");
        assert_eq!(stall_count, 1, "the strike must be durable");

        // Immediate re-claim (the released-row arm — no threshold):
        // the next attempt picks the slot up at once with the stall
        // evidence preserved.
        let reclaim = crate::ingest::claim_placeholder(
            &db.pool,
            &sp_hash,
            &path,
            &[],
            crate::ingest::IngestHooks {
                stale_reclaimed_metric: "rio_store_substitute_stale_reclaimed_total",
                ctx_label: "bw9s5-bj",
            },
            None,
        )
        .await
        .expect("re-claim query");
        assert!(
            matches!(reclaim, crate::ingest::PlaceholderClaim::Owned(_)),
            "released row must be immediately re-claimable"
        );
        let preserved: i16 =
            sqlx::query_scalar("SELECT stall_count FROM manifests WHERE store_path_hash = $1")
                .bind(&sp_hash[..])
                .fetch_one(&db.pool)
                .await
                .expect("stall_count query");
        assert_eq!(preserved, 1, "re-claim preserves the stall evidence");
    }

    // r[verify store.put.nar-bytes-budget+6]
    // W8-E (R16 statement; GREEN-SIDE PIN, disclosed — it certifies a
    // premise the pre-fix tree also satisfies): tokio fair-FIFO over
    // `acquire_many` — three staggered PRODUCTION acquires (the
    // reserve site itself) are granted in arrival order. This converts
    // the theorem's fairness dependency from a documented upstream
    // fact into an observed witness.
    #[tokio::test]
    async fn budget_grants_follow_arrival_order() {
        let budget = crate::budget::NarBudget::new(MIN_NAR_CHUNK_CHARGE as usize);
        let env = || {
            NarHoldEnvelope::for_declared(
                MIN_NAR_CHUNK_CHARGE as u64,
                Duration::from_secs(1),
                NAR_HOLD_FLOOR_RATE,
            )
        };
        let tenant = Uuid::new_v4();
        // A holds the whole pool.
        let a = NarBudgetReservation::reserve(
            &budget,
            tenant,
            TENANT_RESERVATION_CAP,
            MIN_NAR_CHUNK_CHARGE as u64,
            env(),
            None,
        )
        .await
        .unwrap();
        // B parks (signal just before the acquire polls), then C.
        let (b_started_tx, b_started) = tokio::sync::oneshot::channel::<()>();
        let b = {
            let budget = budget.clone();
            tokio::spawn(async move {
                let _ = b_started_tx.send(());
                NarBudgetReservation::reserve(
                    &budget,
                    tenant,
                    TENANT_RESERVATION_CAP,
                    MIN_NAR_CHUNK_CHARGE as u64,
                    env(),
                    None,
                )
                .await
            })
        };
        b_started.await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let c = {
            let budget = budget.clone();
            tokio::spawn(async move {
                NarBudgetReservation::reserve(
                    &budget,
                    tenant,
                    TENANT_RESERVATION_CAP,
                    MIN_NAR_CHUNK_CHARGE as u64,
                    env(),
                    None,
                )
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!b.is_finished() && !c.is_finished(), "both must be parked");
        // Release A: exactly B (the earlier arrival) is granted.
        drop(a);
        let b_res = tokio::time::timeout(Duration::from_secs(5), b)
            .await
            .expect("B granted after A frees")
            .unwrap()
            .unwrap();
        assert!(!c.is_finished(), "C must still be parked behind B's grant");
        // Release B: C granted.
        drop(b_res);
        tokio::time::timeout(Duration::from_secs(5), c)
            .await
            .expect("C granted after B frees")
            .unwrap()
            .unwrap();
    }

    // r[verify store.put.nar-hold-envelope+2]
    // Envelope-violation red (R17, floor-rate axis): the SAME paced
    // stream completes under a loose floor rate and aborts typed under
    // an absurd one — the knob demonstrably BINDS, and the abort fires
    // on a stream that satisfies every per-read window (the shape the
    // stall clock structurally cannot see).
    #[tokio::test]
    async fn hold_envelope_floor_rate_binds() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let stall = Duration::from_millis(500);
        let content = rio_test_support::fixtures::pseudo_random_bytes(21, 48 * 1024);
        let (nar, _h) = rio_test_support::fixtures::make_nar(&content);
        let frames: Vec<Vec<u8>> = nar.chunks(nar.len() / 12 + 1).map(|c| c.to_vec()).collect();
        let n_frames = frames.len();

        // Control arm (loose rate 8 KiB/s): deadline = 5×500ms +
        // 48K/8K ≈ 8.5s; the ~3s paced transfer completes.
        let tid_a = seed_tenant(&db.pool, "bw8s1-rate-a").await;
        let path_a = rio_test_support::fixtures::test_store_path("bw8s1-rate-a");
        let gate_a = Arc::new(tokio::sync::Notify::new());
        let fake_a = spawn_flex_upstream(
            &path_a,
            nar.clone(),
            "cache.bw8rate",
            FlexCfg {
                nar_frames: Some((frames.clone(), Some(gate_a.clone()))),
                ..Default::default()
            },
        )
        .await;
        insert_flex(&db.pool, tid_a, &fake_a, 50).await;
        let sub_a = test_substituter(db.pool.clone())
            .with_stall_window(stall)
            .with_nar_hold_floor_rate(8 * 1024);
        let pace_a = {
            let g = gate_a.clone();
            tokio::spawn(async move {
                for _ in 0..n_frames {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    g.notify_one();
                }
            })
        };
        let got_a = drive_upstream(&sub_a, tid_a, &path_a).await;
        pace_a.abort();
        assert!(
            matches!(got_a, Ok(UpstreamOutcome::Hit { .. })),
            "control arm (loose floor rate) must complete: {got_a:?}"
        );

        // Violation arm (absurd rate u64::MAX): deadline collapses to
        // the grace (2.5s) < the ≥3s paced transfer — typed abort on a
        // per-read-healthy stream.
        let tid_b = seed_tenant(&db.pool, "bw8s1-rate-b").await;
        let path_b = rio_test_support::fixtures::test_store_path("bw8s1-rate-b");
        let gate_b = Arc::new(tokio::sync::Notify::new());
        let fake_b = spawn_flex_upstream(
            &path_b,
            nar.clone(),
            "cache.bw8rate",
            FlexCfg {
                nar_frames: Some((frames, Some(gate_b.clone()))),
                ..Default::default()
            },
        )
        .await;
        insert_flex(&db.pool, tid_b, &fake_b, 50).await;
        let sub_b = test_substituter(db.pool.clone())
            .with_stall_window(stall)
            .with_nar_hold_floor_rate(u64::MAX);
        let pace_b = {
            let g = gate_b.clone();
            tokio::spawn(async move {
                for _ in 0..n_frames {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    g.notify_one();
                }
            })
        };
        let got_b = drive_upstream(&sub_b, tid_b, &path_b).await;
        pace_b.abort();
        assert!(
            matches!(got_b, Err(SubstituteError::HoldDeadlineExceeded { .. })),
            "violation arm (absurd floor rate) must abort typed: {got_b:?}"
        );
    }

    // r[verify store.put.nar-bytes-budget+6]
    // W8-G (R16 statement): the cost bound at the constructor — a
    // third same-tenant large reservation is refused typed while a
    // second tenant admits (cross-tenant isolation observed, not
    // narrated). Production constructors throughout: the ledger, the
    // cap, and `reserve` are the production objects.
    /// RED pre-fix (verbatim, run at the WO-S1-2a tip 9bc598728 — the
    /// 2b-pre tree): `left: three ~MAX-scale declarations from one
    /// tenant all acquired (pool depleted 3x by one tenant's
    /// declarations; no per-tenant accounting exists) / right: third
    /// refused TenantBudgetExhausted; second tenant's reservation
    /// admitted`.
    #[tokio::test]
    async fn tenant_reservation_charge_is_capped() {
        // Permit pool large enough that the SEMAPHORE never refuses —
        // the cap must be the binding constraint (permits are
        // counters, not memory). 4x: the chunk-lane carve
        // (merged_bug_101, pool/8) leaves a declared face of
        // 3.5 x MAX_NAR_SIZE, still above the three ~MAX reservations
        // this schedule grants.
        let budget = crate::budget::NarBudget::new((MAX_NAR_SIZE as usize) * 4);
        let tenant_a = Uuid::new_v4();
        let tenant_b = Uuid::new_v4();
        let big = MAX_NAR_SIZE - 1;
        let env = || {
            NarHoldEnvelope::for_declared(big, DEFAULT_SUBSTITUTE_STALL_WINDOW, NAR_HOLD_FLOOR_RATE)
        };
        // Two near-MAX reservations from tenant A: admitted
        // (2 x (MAX-1) <= TENANT_RESERVATION_CAP).
        let _a1 = NarBudgetReservation::reserve(
            &budget,
            tenant_a,
            TENANT_RESERVATION_CAP,
            big,
            env(),
            None,
        )
        .await
        .expect("first same-tenant reservation admitted");
        let _a2 = NarBudgetReservation::reserve(
            &budget,
            tenant_a,
            TENANT_RESERVATION_CAP,
            big,
            env(),
            None,
        )
        .await
        .expect("second same-tenant reservation admitted (parallel warm preserved)");
        // Third same-tenant reservation — even a MINIMAL one — refused
        // typed at the constructor (aggregate accounting, not
        // per-attempt size).
        let third = NarBudgetReservation::reserve(
            &budget,
            tenant_a,
            TENANT_RESERVATION_CAP,
            1,
            NarHoldEnvelope::for_declared(1, DEFAULT_SUBSTITUTE_STALL_WINDOW, NAR_HOLD_FLOOR_RATE),
            None,
        )
        .await;
        assert!(
            matches!(
                third,
                Err(SubstituteError::TenantBudgetExhausted { cap }) if cap == TENANT_RESERVATION_CAP
            ),
            "third same-tenant reservation must be refused typed: {third:?}"
        );
        // Cross-tenant isolation: tenant B admits at full size.
        let b1 = NarBudgetReservation::reserve(
            &budget,
            tenant_b,
            TENANT_RESERVATION_CAP,
            big,
            env(),
            None,
        )
        .await;
        assert!(b1.is_ok(), "second tenant must admit: {:?}", b1.err());
    }

    // r[verify store.put.nar-bytes-budget+6]
    // W8-H (R16 statement): release — drop restores headroom, no leak
    // across abort paths. The guard rides the reservation, so EVERY
    // abort path (completion, HoldDeadlineExceeded, cancellation)
    // releases through the ONE Drop impl — rustc enforces there is no
    // second release seam; the deadline-abort instance is additionally
    // observed end-to-end by the trickle/blackhole reds' pool-restored
    // assertions (a leaked guard would deadlock their re-reservations).
    /// Pre-fix left disclosed as constructor-dependent (no per-tenant
    /// accounting existed to assert against — green-side half,
    /// R13-clean: all flows through `reserve`).
    #[tokio::test]
    async fn tenant_cap_releases_with_the_reservation() {
        let budget = crate::budget::NarBudget::new((MAX_NAR_SIZE as usize) * 3);
        let tenant = Uuid::new_v4();
        let big = MAX_NAR_SIZE - 1;
        let env = || {
            NarHoldEnvelope::for_declared(big, DEFAULT_SUBSTITUTE_STALL_WINDOW, NAR_HOLD_FLOOR_RATE)
        };
        let r1 = NarBudgetReservation::reserve(
            &budget,
            tenant,
            TENANT_RESERVATION_CAP,
            big,
            env(),
            None,
        )
        .await
        .unwrap();
        let r2 = NarBudgetReservation::reserve(
            &budget,
            tenant,
            TENANT_RESERVATION_CAP,
            big,
            env(),
            None,
        )
        .await
        .unwrap();
        // At cap: refused.
        assert!(
            NarBudgetReservation::reserve(
                &budget,
                tenant,
                TENANT_RESERVATION_CAP,
                big,
                env(),
                None,
            )
            .await
            .is_err()
        );
        // Drop one: headroom restored, the SAME tenant re-admits.
        drop(r1);
        let r3 = NarBudgetReservation::reserve(
            &budget,
            tenant,
            TENANT_RESERVATION_CAP,
            big,
            env(),
            None,
        )
        .await;
        assert!(
            r3.is_ok(),
            "headroom must be restored on drop: {:?}",
            r3.err()
        );
        drop(r2);
        drop(r3);
        // Full release: the ledger entry is gone (bounded map).
        let again = NarBudgetReservation::reserve(
            &budget,
            tenant,
            TENANT_RESERVATION_CAP,
            big,
            env(),
            None,
        )
        .await;
        assert!(again.is_ok(), "fully drained tenant re-admits");
    }

    // r[verify store.put.nar-bytes-budget+6]
    // Envelope-violation red (R17, cost-axis knob): an absurd cap
    // refuses even a minimal reservation — the knob demonstrably
    // BINDS — while the default admits the same charge.
    #[tokio::test]
    async fn tenant_cap_binds() {
        let budget = crate::budget::NarBudget::new(MIN_NAR_CHUNK_CHARGE as usize * 4);
        let tenant = Uuid::new_v4();
        let env = || {
            NarHoldEnvelope::for_declared(1, DEFAULT_SUBSTITUTE_STALL_WINDOW, NAR_HOLD_FLOOR_RATE)
        };
        // Violation arm: cap below the minimum charge floor — refused.
        let refused = NarBudgetReservation::reserve(
            &budget,
            tenant,
            MIN_NAR_CHUNK_CHARGE as u64 - 1,
            1,
            env(),
            None,
        )
        .await;
        assert!(
            matches!(refused, Err(SubstituteError::TenantBudgetExhausted { .. })),
            "absurd cap must refuse: {refused:?}"
        );
        // Control arm: default cap admits the identical charge.
        let admitted =
            NarBudgetReservation::reserve(&budget, tenant, TENANT_RESERVATION_CAP, 1, env(), None)
                .await;
        assert!(admitted.is_ok(), "default cap admits: {:?}", admitted.err());
    }

    // r[verify store.put.nar-hold-envelope+2]
    /// Const-relation pin `hold_grace_exceeds_stall_window` (R17
    /// ordering law; the compile-time half is the
    /// `NAR_HOLD_GRACE_FACTOR >= 2` const assert): the derived
    /// envelope's fixed grace is ≥ 2× the stall window it derives
    /// from, for every window — the deadline can never preempt a
    /// single healthy read.
    #[test]
    fn hold_grace_exceeds_stall_window() {
        for secs in [1u64, 30, 180, 600] {
            let stall = Duration::from_secs(secs);
            let env = NarHoldEnvelope::for_declared(0, stall, NAR_HOLD_FLOOR_RATE);
            assert!(
                env.hold_budget() >= stall * 2,
                "grace {:?} < 2× stall {stall:?}",
                env.hold_budget()
            );
        }
    }

    // r[verify store.put.nar-hold-envelope+2]
    /// Const-relation pin `wait_grace_within_hold_floor` (R17 ordering
    /// law; compile-time half lives beside BUDGET_WAIT_GRACE): the
    /// chunk-acquire wait grace is strictly inside the smallest hold
    /// envelope's fixed grace, so a waiting holder always sheds its
    /// WAIT before its own HOLD deadline can fire.
    #[test]
    fn wait_grace_within_hold_floor() {
        let min_hold_grace = DEFAULT_SUBSTITUTE_STALL_WINDOW * NAR_HOLD_GRACE_FACTOR;
        assert!(
            crate::grpc::NarIngestEnvelopeCfg::default().budget_wait_grace < min_hold_grace,
            "wait grace must sit strictly inside the hold grace"
        );
        // And the ingest envelope's basis is the charged-permit cap.
        let env =
            NarHoldEnvelope::for_ingest_cap(DEFAULT_SUBSTITUTE_STALL_WINDOW, NAR_HOLD_FLOOR_RATE);
        assert_eq!(env.bytes_basis(), MAX_NAR_SIZE);
    }
}

// r[verify store.put.nar-bytes-budget+6]
#[cfg(test)]
mod listener_backoff_tests {
    use super::*;

    /// Drive a cycle sequence through the machine; collect the slept
    /// delays (what the loop's `sleep(backoff.on_error())` would do).
    fn drive(events: &[Event]) -> Vec<u64> {
        let mut m = ListenerBackoff::new();
        let mut delays = Vec::new();
        for e in events {
            match e {
                Event::Error => delays.push(m.on_error().as_secs()),
                Event::Delivered => {
                    m.on_delivered();
                }
            }
        }
        delays
    }

    #[derive(Clone, Copy)]
    enum Event {
        /// Any listener error cycle (connect-err, listen-err, or
        /// recv-err — the three classes share the one arm).
        Error,
        /// A received notification (delivered work).
        Delivered,
    }

    /// W10-L (bug_172): under persistent failure with ZERO deliveries
    /// the inter-connect interval reaches the 30s cap — per ERROR
    /// CLASS, all three pinned to the same contract (the R23′ census
    /// for "capped backoff on ANY listener error"). The classes are
    /// structurally one arm; each test leg drives the event shape its
    /// class produces in the loop.
    ///
    /// THE PRE-FIX RED (strawman-disclosed — the pre-fix wiring
    /// simulated verbatim, since the defect lived in loop plumbing a
    /// unit test cannot host): with the reset on LISTEN-ok, the
    /// recv-err class re-reset every cycle and slept a FLAT 1s
    /// forever. The strawman leg asserts that arithmetic so the
    /// defect stays demonstrable; the load-bearing assertions are the
    /// post-fix per-class legs.
    #[test]
    fn listener_backoff_caps_on_every_error_class() {
        // connect-err: no connection, no LISTEN, no delivery.
        let connect_err = drive(&[Event::Error; 8]);
        // listen-err: connect-ok is not a delivery — same shape.
        let listen_err = drive(&[Event::Error; 8]);
        // recv-err (THE bug class): connect-ok + LISTEN-ok are
        // handshakes, not deliveries — same shape post-fix.
        let recv_err = drive(&[Event::Error; 8]);
        let geometric = vec![1, 2, 4, 8, 16, 30, 30, 30];
        for (class, delays) in [
            ("connect-err", connect_err),
            ("listen-err", listen_err),
            ("recv-err", recv_err),
        ] {
            assert_eq!(
                delays, geometric,
                "{class}: the capped contract is geometric to 30s"
            );
        }

        // The heal-edge mixed sequence through the SAME driver (the
        // alphabet's second letter): errors, then a delivery, then a
        // fresh error restarts at the floor.
        let healed = drive(&[
            Event::Error,
            Event::Error,
            Event::Error,
            Event::Delivered,
            Event::Error,
        ]);
        assert_eq!(
            healed,
            vec![1, 2, 4, 1],
            "a delivery resets the budget mid-sequence"
        );

        // The pre-fix strawman (DISCLOSED): reset-on-LISTEN-ok made
        // the recv-err class flat at the 1s floor — the live defect's
        // arithmetic (one fresh pool connection + LISTEN per second
        // per replica for the whole degradation window).
        let mut strawman = ListenerBackoff::new();
        let mut flat = Vec::new();
        for _ in 0..8 {
            strawman.on_delivered(); // the pre-fix misplaced reset (LISTEN-ok)
            flat.push(strawman.on_error().as_secs());
        }
        assert_eq!(
            flat,
            vec![1; 8],
            "the pre-fix latch placement never engages the cap"
        );
    }

    /// The heal edge: a delivery resets the budget (fresh degradation
    /// restarts at the floor), and the recovery disclosure reports
    /// how many error cycles the delivery ended.
    #[test]
    fn listener_backoff_resets_on_delivered_work_only() {
        let mut m = ListenerBackoff::new();
        for _ in 0..5 {
            let _ = m.on_error();
        }
        assert_eq!(m.on_error().as_secs(), 30, "at the cap");
        let recovered = m.on_delivered();
        assert_eq!(recovered, 6, "the recovery names the degraded cycles");
        assert_eq!(
            m.on_error().as_secs(),
            1,
            "post-delivery degradation restarts at the floor"
        );
        let mut healthy = ListenerBackoff::new();
        assert_eq!(
            healthy.on_delivered(),
            0,
            "steady-state deliveries report zero degraded cycles"
        );
    }
}
