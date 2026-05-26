//! Store-fetch primitives for the castore-FUSE (`castore_fuse`).
//!
//! Everything here is FUSE-agnostic plumbing shared by the castore
//! fetch paths: the per-build gRPC client bundle ([`StoreClients`]),
//! the crate-internal assignment-token request wrapper
//! (`authed_request`), the in-budget transient-retry policy for
//! JIT/streaming fetches, and the
//! size-aware JIT fetch-timeout helper ([`jit_fetch_timeout`]). The
//! FUSE-typed callers (the `open()` whole-file fetch, the streaming
//! fill task, the DAG prefetch) live in `castore_fuse::{open,stream,
//! tree}` and call into here for every rio-store RPC.

use std::time::Duration;

use tonic::transport::Channel;

use rio_proto::store::chunk_service_client::ChunkServiceClient;
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
/// (`r[store.castore.tenant-scope]`); a request without it is rejected
/// as `UNAUTHENTICATED`, so every castore-FUSE RPC goes through here.
pub(crate) fn authed_request<T>(
    msg: T,
    assignment_token: &str,
) -> Result<tonic::Request<T>, tonic::Status> {
    let mut req = tonic::Request::new(msg);
    crate::upload::common::attach_assignment_token(&mut req, assignment_token)?;
    Ok(req)
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
