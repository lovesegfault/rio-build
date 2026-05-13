//! Shared `aws_sdk_s3::Client` builder.
//!
//! rio-store (chunk backend) and rio-scheduler (build-log flush +
//! AdminService log replay) both talk to S3. Before this module each
//! built its own client with subtly different config — scheduler used
//! `aws_config::defaults()` (3 retry attempts, stalled-stream
//! protection ON), store used `from_env()` with raised retries +
//! stalled-stream protection OFF. The store config is the
//! battle-tested one (see [`default_client`] for the rationale);
//! sharing it here means region/endpoint/credential/retry resolution
//! has one home and the two services can't drift.
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
/// 3. `RIO_S3_FORCE_PATH_STYLE=true` (env, not config) forces
///    path-style addressing (`http://host/bucket/key`) instead of the
///    sdk's virtual-hosted default (`http://bucket.host/key`).
///    Virtual-hosted needs per-bucket DNS — fine for AWS, broken for
///    a loopback minio at `http://127.0.0.1:19000` (no resolver for
///    `rio-bench-chunks.127.0.0.1`). The aws-sdk does NOT read
///    `AWS_S3_FORCE_PATH_STYLE` — that's a CLI/boto3 convention only
///    — so we read our own. Env-only because path-style is a *deploy
///    topology* knob (which S3 implementation am I behind), not
///    application config; it pairs with `AWS_ENDPOINT_URL` which is
///    also env-only.
pub async fn default_client(max_attempts: u32) -> aws_sdk_s3::Client {
    let cfg = aws_config::from_env()
        .retry_config(RetryConfig::standard().with_max_attempts(max_attempts))
        .stalled_stream_protection(StalledStreamProtectionConfig::disabled())
        .load()
        .await;
    let force_path = std::env::var("RIO_S3_FORCE_PATH_STYLE")
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let s3_cfg = aws_sdk_s3::config::Builder::from(&cfg)
        .force_path_style(force_path)
        .build();
    aws_sdk_s3::Client::from_conf(s3_cfg)
}
