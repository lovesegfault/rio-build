//! Store-fetch primitives for the castore-FUSE (`castore_fuse`).
//!
//! Everything here is FUSE-agnostic plumbing shared by the castore
//! fetch paths: the per-build gRPC client bundle ([`StoreClients`]),
//! the crate-internal assignment-token request wrapper
//! (`authed_request`), the in-budget transient-retry policy for
//! JIT/streaming fetches, the per-build closure-scope presenter
//! ([`ScopePresenter`], ADR-022 P0591 Phase 2), and the
//! size-aware JIT fetch-timeout helper ([`jit_fetch_timeout`]). The
//! FUSE-typed callers (the `open()` whole-file fetch, the streaming
//! fill task, the DAG prefetch) live in `castore_fuse::{open,stream,
//! tree}` and call into here for every rio-store RPC.

use std::time::Duration;

use tonic::transport::Channel;

use rio_proto::store::chunk_service_client::ChunkServiceClient;
use rio_proto::types::PresentClosureRequest;
use rio_proto::{DirectoryServiceClient, StoreServiceClient};

/// gRPC client bundle for store fetches.
///
/// Wraps `StoreServiceClient`, `ChunkServiceClient`, and
/// `DirectoryServiceClient` over a single (typically p2c-balanced)
/// `tonic::transport::Channel`. Clone is cheap — the channel is
/// `Arc`-internal.
///
/// Kept as a struct (not a bare type alias) so future client additions
/// thread through every call site as one parameter. `chunk` is the
/// P0568 addition (the streaming fill task pipelines local-cache misses
/// to rio-store's batched fan-out); `directory` is the P0559 addition
/// (the castore-FUSE prefetches the Directory DAG via `GetDirectory`
/// and fetches whole files via `ReadBlob`).
#[derive(Clone)]
pub struct StoreClients {
    pub store: StoreServiceClient<Channel>,
    pub chunk: ChunkServiceClient<Channel>,
    pub directory: DirectoryServiceClient<Channel>,
}

impl StoreClients {
    /// Wrap the store, chunk, and directory clients over a single
    /// `Channel` with the standard max-message-size headroom (matches
    /// `connect_single`'s convention). One channel: all three RPC
    /// services run on the same rio-store endpoint and share the p2c
    /// balancer.
    pub fn from_channel(ch: Channel) -> Self {
        let max = rio_common::grpc::max_message_size();
        Self {
            store: StoreServiceClient::new(ch.clone())
                .max_decoding_message_size(max)
                .max_encoding_message_size(max),
            chunk: ChunkServiceClient::new(ch.clone())
                .max_decoding_message_size(max)
                .max_encoding_message_size(max),
            directory: DirectoryServiceClient::new(ch)
                .max_decoding_message_size(max)
                .max_encoding_message_size(max),
        }
    }
}

/// Wrap a request body in [`tonic::Request`] carrying the build's
/// assignment token (`x-rio-assignment-token`) plus the current trace
/// context — the same metadata the upload path attaches
/// ([`crate::upload::common::attach_assignment_token`]). rio-store's
/// castore surface (`GetDirectory`/`ReadBlob`/`StatBlob`/`GetChunks`)
/// derives the caller's tenant from this token
/// (`r[store.castore.tenant-scope+2]`); a request without it is rejected
/// as `UNAUTHENTICATED`, so every castore-FUSE RPC goes through here.
pub(crate) fn authed_request<T>(
    msg: T,
    assignment_token: &str,
) -> Result<tonic::Request<T>, tonic::Status> {
    let mut req = tonic::Request::new(msg);
    crate::upload::common::attach_assignment_token(&mut req, assignment_token)?;
    Ok(req)
}

/// Re-presentations of the input closure allowed per castore read
/// operation when the store keeps answering `FAILED_PRECONDITION` +
/// `CASTORE_SCOPE_REQUIRED` (ADR-022 P0591). Each re-present is one
/// cheap idempotent unary plus one retry of the read, all inside the
/// caller's existing per-operation budget — the bound only exists so a
/// pathological store (or an L7 balancer ping-ponging every retry to a
/// replica that immediately evicts the scope) cannot spin the loop for
/// the whole budget. Normal multi-replica churn needs exactly one.
pub const SCOPE_PRESENT_ATTEMPTS: u32 = 3;

/// Per-build closure-scope presenter (ADR-022 P0591 Phase 2,
/// `r[builder.castore.scope-present]`).
///
/// rio-store scopes a build's assignment-token castore reads to the
/// input closure the scheduler signed for it (`castore_read_scope.mode
/// = "enforce"`, the shipped default). The store learns that closure
/// when the builder presents `WorkAssignment.input_closure` via
/// `DirectoryService.PresentClosure`; this type owns that presentation
/// for one build:
///
/// - **once per store channel** — [`Self::present_on_new_channel`] runs
///   at mount time before the DAG prefetch (the build's first castore
///   read), so the common case never sees a scope miss;
/// - **on demand** — [`Self::run_scoped`] wraps every castore read of
///   the JIT/streaming/prefetch paths and, when the store answers
///   `FAILED_PRECONDITION` carrying
///   [`rio_proto::CASTORE_SCOPE_REQUIRED_MSG`] (a replica that never
///   saw, or evicted, the scope), re-presents and retries within the
///   caller's existing budget — bounded by [`SCOPE_PRESENT_ATTEMPTS`]
///   and **excluded from circuit-breaker accounting** (the callers skip
///   their breaker record for this failure class; it is a coordination
///   signal, not a store-health signal);
/// - **singleflight** — concurrent fetches that all hit a scope miss
///   serialize on one presentation: each caller snapshots the
///   presentation generation before its read, and only the first one
///   through re-presents; the rest see the bumped generation and just
///   retry.
///
/// Tolerates `UNIMPLEMENTED` from a store that predates the RPC (skip
/// presentation, log once, proceed — such a store never emits the
/// scope-required reason either). When the assignment carries no
/// `input_closure` (legacy/degraded dispatch) the presenter is inert:
/// no RPC is ever sent and behavior is unchanged.
// r[impl builder.castore.scope-present]
pub struct ScopePresenter {
    /// Dedicated client clone for `PresentClosure` (same shared channel
    /// as every other castore RPC).
    directory: DirectoryServiceClient<Channel>,
    /// `WorkAssignment.input_closure`, exactly as received. Empty =
    /// nothing to present (the token carries no closure attestation
    /// either, so the store never asks).
    closure: Vec<String>,
    assignment_token: String,
    state: tokio::sync::Mutex<PresenterState>,
}

#[derive(Default)]
struct PresenterState {
    /// Number of successful presentations on this channel. 0 = never
    /// presented; callers snapshot this before a read and pass it back
    /// so only one of several concurrent scope-miss victims re-presents.
    generation: u64,
    /// The store answered `UNIMPLEMENTED`: it predates PresentClosure.
    /// Never present again on this channel (logged once).
    unsupported: bool,
}

/// How [`ScopePresenter::run_scoped`] inspects and produces the
/// caller's error type: `tonic::Status` for the prefetch/streaming
/// paths, the whole-file path's `FetchError` in `castore_fuse::open`.
pub trait ScopeRetryError: Sized {
    /// The wrapped gRPC status, if this error is `FAILED_PRECONDITION`
    /// carrying the `CASTORE_SCOPE_REQUIRED` reason.
    fn scope_required(&self) -> Option<&tonic::Status>;
    /// Wrap a failed `PresentClosure` status (surfaced instead of the
    /// opaque scope-required error — e.g. an `INVALID_ARGUMENT` closure
    /// mismatch is the actionable root cause).
    fn from_present_failure(status: tonic::Status) -> Self;
}

impl ScopeRetryError for tonic::Status {
    fn scope_required(&self) -> Option<&tonic::Status> {
        ScopePresenter::is_scope_required(self).then_some(self)
    }
    fn from_present_failure(status: tonic::Status) -> Self {
        status
    }
}

impl ScopePresenter {
    pub fn new(
        directory: DirectoryServiceClient<Channel>,
        closure: Vec<String>,
        assignment_token: String,
    ) -> Self {
        Self {
            directory,
            closure,
            assignment_token,
            state: tokio::sync::Mutex::new(PresenterState::default()),
        }
    }

    /// True iff `status` is the store's "present the closure and retry"
    /// answer (`r[builder.castore.scope-present]`). Deliberately narrow:
    /// other `FAILED_PRECONDITION` reasons (inline manifest on StatBlob,
    /// chunked-upload preconditions) MUST NOT trigger the
    /// present-and-retry loop — mirrors `is_chunked_unsupported`'s
    /// single-constant match.
    pub fn is_scope_required(status: &tonic::Status) -> bool {
        status.code() == tonic::Code::FailedPrecondition
            && status
                .message()
                .contains(rio_proto::CASTORE_SCOPE_REQUIRED_MSG)
    }

    /// Whether this assignment carries a closure to present.
    pub fn has_closure(&self) -> bool {
        !self.closure.is_empty()
    }

    /// Proactive presentation for a freshly established store channel —
    /// called at mount time, before the DAG prefetch, inside the
    /// caller's `dag_prefetch_timeout`. Best-effort: a failure here is
    /// logged and the build proceeds (a `log`-mode or pre-P0591 store
    /// serves without it; an `enforce`-mode store re-asks via
    /// `CASTORE_SCOPE_REQUIRED` and [`Self::run_scoped`] re-presents).
    /// No-op when the closure is empty or a previous attempt already
    /// presented / learned the store doesn't support it.
    pub async fn present_on_new_channel(&self) {
        if !self.has_closure() {
            return;
        }
        {
            let state = self.state.lock().await;
            if state.generation > 0 || state.unsupported {
                return;
            }
        }
        if let Err(status) = self.present(0, "mount").await {
            tracing::warn!(
                error = %status,
                paths = self.closure.len(),
                "PresentClosure failed at mount; proceeding — an enforce-mode store will \
                 ask again via CASTORE_SCOPE_REQUIRED and the fetch path re-presents"
            );
        }
    }

    /// Snapshot of the presentation generation, taken by
    /// [`Self::run_scoped`] before each attempt so concurrent scope-miss
    /// victims don't stampede `PresentClosure`.
    async fn generation(&self) -> u64 {
        self.state.lock().await.generation
    }

    /// Present the closure (idempotent). Returns `Ok(true)` when the
    /// caller should retry its read: either this call presented, or a
    /// concurrent caller already advanced the generation past
    /// `observed_generation`. Returns `Ok(false)` when presentation is
    /// impossible and retrying is pointless (no closure to present, or
    /// the store doesn't implement the RPC). `Err` carries the
    /// `PresentClosure` failure itself.
    async fn present(
        &self,
        observed_generation: u64,
        trigger: &'static str,
    ) -> Result<bool, tonic::Status> {
        if !self.has_closure() {
            return Ok(false);
        }
        let mut state = self.state.lock().await;
        if state.unsupported {
            return Ok(false);
        }
        if state.generation > observed_generation {
            // Another fetch already (re-)presented since the caller
            // observed its failure — its presentation covers us.
            return Ok(true);
        }
        let req = authed_request(
            PresentClosureRequest {
                closure: self.closure.clone(),
            },
            &self.assignment_token,
        )?;
        let mut client = self.directory.clone();
        match client.present_closure(req).await {
            Ok(_) => {
                state.generation += 1;
                metrics::counter!(
                    "rio_builder_castore_scope_present_total",
                    "trigger" => trigger,
                    "outcome" => "ok"
                )
                .increment(1);
                tracing::debug!(
                    trigger,
                    paths = self.closure.len(),
                    generation = state.generation,
                    "presented the input closure to rio-store"
                );
                Ok(true)
            }
            Err(status) if status.code() == tonic::Code::Unimplemented => {
                // Old store: presentation (and scope enforcement) does
                // not exist there. Log once, never try again.
                state.unsupported = true;
                metrics::counter!(
                    "rio_builder_castore_scope_present_total",
                    "trigger" => trigger,
                    "outcome" => "unsupported"
                )
                .increment(1);
                tracing::info!(
                    "rio-store does not implement PresentClosure (pre-P0591 store); \
                     skipping closure presentation for this build"
                );
                Ok(false)
            }
            Err(status) => {
                metrics::counter!(
                    "rio_builder_castore_scope_present_total",
                    "trigger" => trigger,
                    "outcome" => "error"
                )
                .increment(1);
                Err(status)
            }
        }
    }

    /// Run `op`, intercepting the store's `FAILED_PRECONDITION` +
    /// `CASTORE_SCOPE_REQUIRED` answer: present the closure and retry,
    /// at most [`SCOPE_PRESENT_ATTEMPTS`] presentations per operation.
    /// Every other outcome (success, NotFound, Unavailable, …) passes
    /// through untouched, so the existing transient-retry, circuit
    /// breaker, and EIO classification see exactly what they saw before
    /// — callers exclude only this scope-required class from their
    /// breaker record.
    ///
    /// The retries run inside whatever budget the caller already wraps
    /// around `op` (`jit_fetch_timeout`, the streaming fill deadline,
    /// `dag_prefetch_timeout`) — this loop never sleeps and never
    /// extends a fetch past it.
    pub async fn run_scoped<T, E: ScopeRetryError>(
        &self,
        op_name: &'static str,
        mut op: impl AsyncFnMut() -> Result<T, E>,
    ) -> Result<T, E> {
        let mut presents = 0u32;
        loop {
            let observed = self.generation().await;
            let err = match op().await {
                Err(err) if err.scope_required().is_some() => err,
                other => return other,
            };
            if presents >= SCOPE_PRESENT_ATTEMPTS {
                tracing::warn!(
                    op = op_name,
                    presents,
                    "castore read still reports CASTORE_SCOPE_REQUIRED after re-presenting; \
                     giving up within this operation's budget"
                );
                return Err(err);
            }
            presents += 1;
            match self.present(observed, "scope_required").await {
                // Presented (or a concurrent fetch did) — retry the read.
                Ok(true) => continue,
                // Nothing to present / store can't accept it: the
                // original scope-required error stands.
                Ok(false) => return Err(err),
                Err(present_err) => {
                    // The presentation itself failed — that status is the
                    // actionable root cause (e.g. INVALID_ARGUMENT when
                    // the presented closure doesn't hash to the token's
                    // signed digest), so surface it instead of the
                    // deliberately generic scope-required answer.
                    tracing::warn!(
                        op = op_name,
                        error = %present_err,
                        "PresentClosure failed while handling CASTORE_SCOPE_REQUIRED"
                    );
                    return Err(E::from_present_failure(present_err));
                }
            }
        }
    }
}

/// Attempts (first try included) for the in-budget transient retry on
/// the castore JIT/streaming fetch path (the I-039 class: a single
/// connection reset or rolling-restart blip must not surface as `EIO`
/// to the build).
///
/// Budget interaction (load-bearing):
/// - Every caller already bounds the whole fetch with its own budget
///   (`jit_fetch_timeout` for whole-file fetches, the streaming fill's
///   size-aware deadline, the connect/request timeouts), and the retry
///   sleeps run INSIDE that same `tokio::time::timeout`/deadline — the
///   retry can never extend a fetch past the budget the daemon's
///   `request_wait_answer` is already parked on. Worst-case added
///   latency ≈ 0.6 s of sleeps (100 + 200 + 400 ms ceilings, full
///   jitter halves the expectation), small against the 60 s flat JIT
///   floor.
/// - The fetch circuit breaker stays ABOVE this retry: callers
///   `check()` once before the first attempt and `record()` once for
///   the overall outcome, so inner retries neither reset nor
///   double-count breaker state — sustained unreachability still trips
///   it after the same number of failed *fetches*, and a fetch that
///   succeeded on attempt 2 records exactly one success.
pub(crate) const TRANSIENT_FETCH_ATTEMPTS: u32 = 3;

/// Backoff between transient-fetch attempts: 100 ms → 200 ms → 400 ms
/// ceilings with full jitter (many fuser threads can hit the same store
/// blip at once — desynchronize their retries).
pub(crate) const TRANSIENT_FETCH_BACKOFF: rio_common::backoff::Backoff =
    rio_common::backoff::Backoff {
        base: Duration::from_millis(100),
        mult: 2.0,
        cap: Duration::from_millis(400),
        jitter: rio_common::backoff::Jitter::Full,
    };

/// Whether a gRPC status is a transport-level failure worth one of the
/// short in-budget retries: the endpoint is briefly unreachable
/// (rolling restart, LB reshuffle — `Unavailable`) or the HTTP/2
/// channel died mid-call (tonic surfaces transport errors as
/// `Unknown`). Application-level statuses (NotFound,
/// FailedPrecondition, Unauthenticated, ...) are NOT transient —
/// retrying them only burns the fetch budget.
pub(crate) fn is_transient_fetch_status(status: &tonic::Status) -> bool {
    matches!(
        status.code(),
        tonic::Code::Unavailable | tonic::Code::Unknown
    )
}

/// Run `op` under the transient-fetch retry policy: up to
/// [`TRANSIENT_FETCH_ATTEMPTS`] attempts, retrying only
/// [`is_transient_fetch_status`] failures, sleeping
/// [`TRANSIENT_FETCH_BACKOFF`] between attempts. Non-transient errors
/// (and the last error once attempts are exhausted) return immediately.
///
/// Callers stay responsible for the outer budget and the circuit
/// breaker — see [`TRANSIENT_FETCH_ATTEMPTS`] for that contract.
pub(crate) async fn retry_transient<T>(
    op_name: &'static str,
    mut op: impl AsyncFnMut() -> Result<T, tonic::Status>,
) -> Result<T, tonic::Status> {
    let mut attempt = 0u32;
    loop {
        match op().await {
            Err(status)
                if attempt + 1 < TRANSIENT_FETCH_ATTEMPTS && is_transient_fetch_status(&status) =>
            {
                metrics::counter!("rio_builder_castore_fuse_fetch_retries_total", "op" => op_name)
                    .increment(1);
                tracing::debug!(
                    op = op_name,
                    attempt,
                    error = %status,
                    "transient castore fetch failure; retrying within the fetch budget"
                );
                tokio::time::sleep(TRANSIENT_FETCH_BACKOFF.duration(attempt)).await;
                attempt += 1;
            }
            other => return other,
        }
    }
}

/// Minimum expected store→builder throughput for JIT fetch-timeout
/// sizing. I-178: 15 MiB/s is a conservative floor — half the ~30 MB/s
/// observed in cluster on the pre-castore JIT fetch path. A 1.9 GB NAR at this
/// floor needs ≈127 s; the previous flat 60 s timeout aborted the fetch
/// mid-stream → daemon ENOENT → PermanentFailure poison.
///
/// Tune DOWN if `rio_builder_input_materialization_failures_total` is
/// sustained nonzero (means real throughput is below this floor —
/// cross-AZ builders, S3 throttle).
pub const JIT_MIN_THROUGHPUT_BPS: u64 = 15 * 1024 * 1024;

/// Per-path JIT fetch timeout: `max(base, nar_size / MIN_THROUGHPUT)`.
///
/// `base` is `jit_fetch_timeout` (`RIO_JIT_FETCH_TIMEOUT_SECS`, default
/// 60 s) so small paths are unchanged
/// from pre-I-178 behavior. Large paths get a size-proportional budget
/// — the I-178 1.9 GB input gets ≈127 s instead of the flat 60 s that
/// aborted it mid-stream.
///
/// Under JIT (I-043 redesign) the FUSE callback IS the fetch site —
/// the daemon's `lstat` blocks in `request_wait_answer` for this
/// duration on a cold input. The size-aware budget is therefore
/// load-bearing for correctness (a too-short timeout → EIO →
/// `InfrastructureFailure`), not just an optimization.
pub fn jit_fetch_timeout(base: Duration, nar_size: u64) -> Duration {
    base.max(Duration::from_secs(
        nar_size.div_ceil(JIT_MIN_THROUGHPUT_BPS),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The CASTORE_SCOPE_REQUIRED matcher is deliberately narrow: only
    /// `FAILED_PRECONDITION` carrying the wire constant triggers the
    /// present-and-retry loop — other `FAILED_PRECONDITION` reasons
    /// (StatBlob's inline-manifest answer, upload preconditions) and
    /// other codes carrying the same text must not.
    // r[verify builder.castore.scope-present]
    #[test]
    fn is_scope_required_matches_only_the_wire_constant() {
        assert!(ScopePresenter::is_scope_required(
            &tonic::Status::failed_precondition(rio_proto::CASTORE_SCOPE_REQUIRED_MSG)
        ));
        // Same code, different reason (e.g. the inline-manifest answer).
        assert!(!ScopePresenter::is_scope_required(
            &tonic::Status::failed_precondition("inline manifest: no chunk list, use ReadBlob")
        ));
        // Same reason text under a different code is not the contract.
        assert!(!ScopePresenter::is_scope_required(
            &tonic::Status::unavailable(rio_proto::CASTORE_SCOPE_REQUIRED_MSG)
        ));
    }

    #[test]
    fn jit_fetch_timeout_floors_at_base() {
        let base = Duration::from_secs(60);
        // Small path: timeout = base.
        assert_eq!(jit_fetch_timeout(base, 1024), base);
        // 1.9 GB at 15 MiB/s ≈ 127 s > base.
        let big = 1_900_000_000u64;
        let t = jit_fetch_timeout(base, big);
        assert!(t > base, "big NAR must extend the timeout, got {t:?}");
        assert_eq!(t.as_secs(), big.div_ceil(JIT_MIN_THROUGHPUT_BPS));
    }

    /// The transient-fetch retry helper: retries Unavailable/Unknown
    /// (transport-level blips), surfaces application-level statuses
    /// immediately, and gives up after `TRANSIENT_FETCH_ATTEMPTS` with
    /// the last error. start_paused: the inter-attempt sleeps advance
    /// instantly so the exhausted case doesn't pay real wall-clock.
    #[tokio::test(start_paused = true)]
    async fn retry_transient_retries_only_transient_codes() {
        use std::sync::atomic::{AtomicU32, Ordering};

        // Unavailable then success → retried, value surfaces.
        let calls = AtomicU32::new(0);
        let out = retry_transient("test-ok", async || {
            match calls.fetch_add(1, Ordering::SeqCst) {
                0 => Err(tonic::Status::unavailable("blip")),
                _ => Ok(42u32),
            }
        })
        .await;
        assert_eq!(out.expect("second attempt succeeds"), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        // NotFound is application-level: exactly one call, no retry.
        let calls = AtomicU32::new(0);
        let out: Result<u32, _> = retry_transient("test-notfound", async || {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(tonic::Status::not_found("no such blob"))
        })
        .await;
        assert_eq!(out.expect_err("not retried").code(), tonic::Code::NotFound);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Sustained Unavailable: exactly TRANSIENT_FETCH_ATTEMPTS calls,
        // last error returned (the breaker above this records ONE failure).
        let calls = AtomicU32::new(0);
        let out: Result<u32, _> = retry_transient("test-exhaust", async || {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(tonic::Status::unavailable("still down"))
        })
        .await;
        assert_eq!(out.expect_err("exhausted").code(), tonic::Code::Unavailable);
        assert_eq!(calls.load(Ordering::SeqCst), TRANSIENT_FETCH_ATTEMPTS);
    }
}
