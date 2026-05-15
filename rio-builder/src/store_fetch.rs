//! Store-fetch primitives shared by the old FUSE (`fuse::fetch`) and
//! the castore-FUSE (`castore_fuse`, P0559).
//!
//! Anything that talks gRPC to rio-store and isn't FUSE-typed lives
//! here so the castore-FUSE doesn't have to import from `fuse::fetch`.
//! The FUSE-typed callers (`fetch_extract_insert`,
//! `prefetch_path_blocking`, the `Errno`-returning streamers) stay in
//! `fuse::fetch` until P0560 deletes that module wholesale.

use std::time::Duration;

use tonic::transport::Channel;

use rio_proto::StoreServiceClient;
use rio_proto::store::log_service_client::LogServiceClient;

/// gRPC client bundle for store fetches.
///
/// Wraps `StoreServiceClient` over a (typically p2c-balanced)
/// `tonic::transport::Channel`. Clone is cheap — the channel is
/// `Arc`-internal.
///
/// Kept as a struct (not a bare type alias) so future client additions
/// (P0573 `DirectoryService`, P0577 `BlobService`) thread through every
/// call site as one parameter.
#[derive(Clone)]
pub struct StoreClients {
    pub store: StoreServiceClient<Channel>,
    /// `LogService` over the same channel. Cloned into each build's
    /// [`crate::log_upload::LogUploader`] for the `AppendLog` stream.
    pub log: LogServiceClient<Channel>,
}

impl StoreClients {
    /// Wrap the store clients over a single `Channel` with the standard
    /// max-message-size headroom (matches `connect_single`'s convention).
    pub fn from_channel(ch: Channel) -> Self {
        let max = rio_common::grpc::max_message_size();
        Self {
            store: StoreServiceClient::new(ch.clone())
                .max_decoding_message_size(max)
                .max_encoding_message_size(max),
            log: LogServiceClient::new(ch)
                .max_decoding_message_size(max)
                .max_encoding_message_size(max),
        }
    }
}

/// Minimum expected store→builder throughput for JIT fetch-timeout
/// sizing. I-178: 15 MiB/s is a conservative floor — half the ~30 MB/s
/// observed in cluster (`rio_builder_fuse_fetch_bytes_total` ÷
/// `rio_builder_fuse_fetch_duration_seconds`). A 1.9 GB NAR at this
/// floor needs ≈127 s; the previous flat 60 s timeout aborted the fetch
/// mid-stream → daemon ENOENT → PermanentFailure poison.
///
/// Tune DOWN if `rio_builder_input_materialization_failures_total` is
/// sustained nonzero (means real throughput is below this floor —
/// cross-AZ builders, S3 throttle).
pub const JIT_MIN_THROUGHPUT_BPS: u64 = 15 * 1024 * 1024;

/// Per-path JIT fetch timeout: `max(base, nar_size / MIN_THROUGHPUT)`.
///
/// `base` is `fuse_fetch_timeout` (60 s) so small paths are unchanged
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

/// Backoff schedule for retrying transient store-gRPC errors
/// (`Unavailable` / `Unknown` — server restarting, transport disconnect).
/// Five delays = six attempts. Total wait ~17.6s (× [`jitter`] per step
/// → ~[8.8s, 26.4s)), sized to survive a `replicas: 1` store rolling
/// restart (~10s old-pod-SIGTERM → new-pod-Ready) without surfacing
/// `EIO` to the build sandbox. I-039: a deploy mid-LLVM-build was
/// killing 40min of work with an opaque `Input/output error` on
/// `stat()`.
///
/// I-189: schedule extended `[…, 5s]` → `[…, 5s, 10s]` and jittered at
/// the call site (NOT baked into this const — the const stays
/// deterministic for tests/docs; jitter is applied where the delay is
/// consumed). Under `hello-deep-256x` (~38000 drvs), hundreds of
/// builders `GetPath` the same 164 MB gcc within seconds; every builder
/// hits the same h2 reset and then retries at the SAME instant — the
/// retry IS the herd. Per-attempt jitter breaks lockstep; the extra
/// 10 s step buys one more drain window.
///
/// Sits BELOW the circuit breaker: callers check the breaker before
/// calling here, so if the store has been down long enough to trip it
/// we never reach this loop. The retry handles the transition
/// window (was-up → briefly-down → up-again); the breaker handles
/// the steady-state (down-for-a-while → fail-fast).
///
/// Short in tests so the permanent-failure path stays sub-second.
#[cfg(not(test))]
pub(crate) const RETRY_BACKOFF: &[Duration] = &[
    Duration::from_millis(100),
    Duration::from_millis(500),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
];
#[cfg(test)]
pub(crate) const RETRY_BACKOFF: &[Duration] = &[
    Duration::from_millis(10),
    Duration::from_millis(50),
    Duration::from_millis(200),
    Duration::from_millis(500),
];

/// Jitter a backoff delay: `delay × U(0.5, 1.5)`.
///
/// I-189: under thundering-herd, every builder that hit the same
/// transient error retries at the same instant — the retry IS the herd.
/// ±50% spread breaks lockstep while keeping the expected delay equal
/// to the schedule entry. Applied at the `tokio::time::sleep` call
/// sites that consume [`RETRY_BACKOFF`], not baked into the const, so
/// the schedule stays inspectable and the test-cfg short schedule
/// stays deterministic in sum.
// r[impl builder.fuse.retry-jitter]
pub(crate) fn jitter(delay: Duration) -> Duration {
    rio_common::backoff::Jitter::Proportional(0.5).apply(delay)
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

    #[test]
    fn jitter_stays_within_band() {
        let d = Duration::from_secs(10);
        for _ in 0..100 {
            let j = jitter(d);
            assert!(j >= d / 2 && j <= d * 3 / 2, "jitter out of band: {j:?}");
        }
    }
}
