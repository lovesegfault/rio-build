//! Shared `aws_sdk_s3::Client` builder.
//!
//! The live consumer set is the store's chunk backend + log planes —
//! the machine-derived census at
//! `rio-store/tests/gensets/s3-op-census.txt` is the consumer list's
//! single source (merged_bug_149: this header previously named
//! rio-scheduler's build-log flush + AdminService log replay as
//! consumers after those ops had left the scheduler; trust the
//! census artifact, not a prose roster here). Historically each
//! consumer built its own client with subtly different config —
//! `aws_config::defaults()` (3 retry attempts, stalled-stream
//! protection ON) vs `from_env()` with raised retries +
//! stalled-stream protection OFF. The battle-tested config lives in
//! [`default_client`] (see its rationale); sharing it here means
//! region/endpoint/credential/retry resolution has one home and
//! consumers can't drift.
//!
//! Feature-gated on `aws` so consumers that don't touch S3 stay
//! aws-sdk-free.

use aws_config::retry::RetryConfig;
use aws_config::stalled_stream_protection::StalledStreamProtectionConfig;

/// Default `max_attempts` for [`default_client`]. The sdk's standard
/// retry default is 3; we raise it because S3-compatible backends
/// (rustfs, MinIO) recycle idle connections more aggressively than
/// AWS S3 — see [`default_client`].
pub const DEFAULT_S3_MAX_ATTEMPTS: u32 = 10;

/// Build an `aws_sdk_s3::Client` with rio-build's standard config:
/// `from_env()` credential/region/endpoint chain, raised retry
/// attempts, stalled-stream protection disabled.
///
/// 1. `max_attempts` raised from 3 → `max_attempts` (default
///    [`DEFAULT_S3_MAX_ATTEMPTS`]). S3-compatible backends (rustfs,
///    MinIO) recycle idle connections more aggressively than AWS S3;
///    a pooled connection that was closed server-side surfaces as
///    `DispatchFailure` on the next request. The sdk's standard
///    retry DOES classify this as transient and retries, but at 3
///    attempts a burst of connection churn (e.g. rustfs restart, or
///    its idle timeout firing mid-ingest) can exhaust retries before
///    the pool reconnects. Observed on kind: 134 dispatch failures
///    at only 8 concurrent puts.
///
/// 2. Stalled-stream protection OFF. The sdk's default grace period
///    can trip on small bodies (≤256 KiB chunks, compressed log
///    batches) against local S3-compatible servers where the upload
///    completes faster than the throughput monitor can establish a
///    baseline. A false-positive stall aborts the request →
///    `DispatchFailure`. We have no untrusted-server streaming here
///    (chunks are tiny, pre-buffered; logs are pre-compressed), so
///    the protection is pure downside.
///
/// 3. NO client-wide `TimeoutConfig` — per-operation deadlines live
///    at each op-class SEAM via `config_override` (D5 closed the
///    historical Q-108 item; the bug-number in older comments refers
///    to that item, not bughunt-9's bug_108). The SDK default is
///    connect-timeout only: an established-then-black-holed
///    connection awaits response headers forever, and the
///    never-completing FIRST attempt defeats the retry layer above.
///    The op-size census that sizing required is COMMITTED at
///    `rio-store/tests/gensets/s3-op-census.txt`; every op class owns
///    a timeout-only override beside its call sites — chunk plane
///    (`CHUNK_OP_ATTEMPT_TIMEOUT`, bodies ≤ 256 KiB), log plane
///    (`LOG_OP_ATTEMPT_TIMEOUT`, zstd bodies ≤ ~8 MiB), VerifyChunks
///    HEAD (`ADMIN_VERIFY_HEAD_ENVELOPE`, the original worked
///    example, which additionally pins its retry shape because the
///    `WaveBudget` above it demands a const-asserted worst case).
///    Timeout-only overrides keep `max_attempts` on the operator's
///    knob (the churn recoveries it was raised for) and the SDK's
///    capped standard backoffs. Streaming GET bodies carry their own
///    collect clocks (`*_GET_BODY_TIMEOUT`) — an attempt timeout
///    covers only up to response headers. A client-wide value
///    remains deliberately unset: the classes' lawful worst cases
///    span 5 s to 60 s, and one number would either cancel lawful
///    log reads or under-defend the chunk plane.
pub async fn default_client(max_attempts: u32) -> aws_sdk_s3::Client {
    let cfg = aws_config::from_env()
        .retry_config(RetryConfig::standard().with_max_attempts(max_attempts))
        .stalled_stream_protection(StalledStreamProtectionConfig::disabled())
        .load()
        .await;
    aws_sdk_s3::Client::new(&cfg)
}
