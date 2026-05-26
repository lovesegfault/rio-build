//! Proto-agnostic gRPC helpers shared across rio binaries.
//!
//! **Layering rule:** anything that depends on `tonic` types but NOT on
//! generated proto types belongs here — timeout wrappers, [`StatusExt`],
//! [`check_bound`], [`max_message_size`], h2
//! tuning, the shared transient-status predicate, `x-rio-*` metadata
//! key constants. Anything that names a generated client or message type
//! (`connect_single`, `BalancedChannel`, NAR stream chunk/collect) belongs
//! in `rio-proto::client`. `rio-proto` depends on this crate, not the
//! other way round, so `rio-controller` can take a backoff helper without
//! pulling in the whole proto crate.
// r[impl common.helpers]
//!
//! Missing timeouts on gRPC calls are a systemic footgun in a distributed
//! system: a hung store/scheduler causes cascading hangs in gateway sessions,
//! worker FUSE mounts, and the scheduler actor's event loop. This module
//! provides consistent timeout bounds and a helper to wrap calls.

use std::fmt::Display;
use std::future::Future;
use std::time::Duration;

use tonic::Status;
use tonic::metadata::MetadataMap;

// ---------------------------------------------------------------------------
// `x-rio-*` gRPC metadata header keys
//
// Single home so the gateway, scheduler, store, and builder all reference
// the SAME constant — a typo in a string literal at one site silently
// breaks header propagation with no compile-time signal. All values are
// lowercase: tonic normalizes metadata keys per HTTP/2 header rules.
// ---------------------------------------------------------------------------

/// gRPC initial-metadata key carrying the scheduler-assigned build_id
/// on `SubmitBuild` responses. Server-streaming RPCs send initial
/// metadata (headers) BEFORE any stream message, so the client has
/// `build_id` even if the stream delivers zero events (scheduler
/// SIGTERM between MergeDag commit and first BuildEvent send).
///
/// Value: UUID v7 stringified (always ASCII, always a valid
/// `MetadataValue<Ascii>`). Always set by the scheduler.
pub const BUILD_ID_HEADER: &str = "x-rio-build-id";

/// gRPC initial-metadata key carrying the scheduler handler span's
/// trace_id on `SubmitBuild` responses.
///
/// Set by the scheduler AFTER `link_parent()` so it reflects the actual
/// trace the handler is in — which, due to the `#[instrument]` +
/// `set_parent` ordering, is a NEW trace LINKED to the gateway's, not a
/// child of it. Jaeger shows two traces connected by an OTel span link.
///
/// The gateway emits THIS id as the `(trace <32-hex>)` suffix on the
/// `rio: build <id>` `STDERR_NEXT` preamble so operators grep the trace
/// that actually spans scheduler→builder (via the
/// `WorkAssignment.traceparent` data-carry). The gateway's own trace_id
/// only reaches gateway spans.
///
/// Value: 32 lowercase-hex characters (128-bit W3C trace_id). Always
/// ASCII. Empty/absent → legacy scheduler; gateway falls back to its
/// own `current_trace_id_hex()`.
pub const TRACE_ID_HEADER: &str = "x-rio-trace-id";

/// gRPC metadata key for HMAC-signed assignment tokens.
///
/// Scheduler signs at dispatch (executor_id + drv_hash + expiry);
/// store verifies on PutPath to gate which executor can upload which
/// path. See `rio_auth::hmac` for the token format. Value is
/// base64-encoded bytes (always ASCII).
pub const ASSIGNMENT_TOKEN_HEADER: &str = "x-rio-assignment-token";

/// Service-identity HMAC token. Minted by trusted control-plane callers
/// (gateway) on `PutPath`; the store verifies it as the HMAC-bypass
/// condition. See `rio_auth::hmac::ServiceClaims`.
pub const SERVICE_TOKEN_HEADER: &str = "x-rio-service-token";

/// gRPC metadata key the gateway sets on every outbound call in JWT mode.
///
/// Lowercase: tonic normalizes metadata keys (HTTP/2 header rules).
/// Matches `rio-gateway/src/handler/build.rs` — if the gateway ever
/// renames this, `header_name_matches_gateway_literal` in
/// `rio_auth::jwt_interceptor` tests fails.
pub const TENANT_TOKEN_HEADER: &str = "x-rio-tenant-token";

/// Executor-identity HMAC token. Minted by the scheduler per
/// `SpawnIntent`, threaded via the controller as `RIO_EXECUTOR_TOKEN`,
/// presented by builders on `BuildExecution` / `Heartbeat`. The
/// scheduler verifies it to bind the stream to the intent the pod was
/// spawned for. See `rio_auth::hmac::ExecutorClaims` and
/// `r[sec.executor.identity-token]`.
pub const EXECUTOR_TOKEN_HEADER: &str = "x-rio-executor-token";

/// Tenant UUID asserted by a trusted internal caller (scheduler) that
/// has no JWT to forward. The store only honours this when the request
/// ALSO carries a valid `x-rio-service-token` whose `caller` is in the
/// allowlist — an unauthenticated request cannot self-select a tenant.
/// See `r[sched.dispatch.fod-substitute]`.
pub const PROBE_TENANT_ID_HEADER: &str = "x-rio-probe-tenant-id";

/// Default timeout for metadata gRPC calls (QueryPathInfo, FindMissingPaths, etc.).
///
/// Should be long enough for a round trip under load, short enough that a
/// stuck server doesn't hang callers indefinitely.
///
/// Tests that arm a hung MockStore to prove a timeout-wrapper exists
/// override this to ~3s via per-component plumbing (e.g.
/// `DagActor::with_grpc_timeout`) — NOT `cfg(test)` on this constant.
/// `cfg(test)` is per-crate; a cross-crate caller's test build still
/// links against rio-common built without `cfg(test)`, so a test-gated
/// constant here would be invisible to it.
pub const DEFAULT_GRPC_TIMEOUT: Duration = Duration::from_secs(30);

/// Timeout for NAR streaming calls (GetPath, PutPath).
///
/// At `MAX_NAR_SIZE` = 4 GiB and ~15 MB/s, a full transfer is ~270s. 300s
/// gives headroom without being unbounded.
pub const GRPC_STREAM_TIMEOUT: Duration = Duration::from_secs(300);

/// Initial h2 per-stream flow-control window (1 MiB). h2's default is
/// 65 535 bytes — at 2-3 ms cross-AZ RTT that's a ~20-30 MB/s ceiling
/// (each 256 KiB NAR chunk needs ~4 WINDOW_UPDATE round-trips before
/// the next can flow). A fixed 1 MiB gives ≥100 MB/s at any RTT
/// ≤10 ms (in-cluster + cross-AZ).
///
/// Do NOT pair this with `http2_adaptive_window(true)`: hyper's
/// `adaptive_window(true)` *resets* both initial windows to
/// `SPEC_WINDOW_SIZE = 65 535` and BDP-probes upward from there, and
/// tonic's builder applies it AFTER `initial_stream_window_size` —
/// silently overriding this constant. Measured live: with both set,
/// builder↔store wire moved ~1 MB/s while store-only could push 55+.
///
/// I-180: the original 30 MB/s wall on builder NAR fetch (not S3
/// prefetch or proto decode).
pub const H2_INITIAL_STREAM_WINDOW: u32 = 1024 * 1024;

/// Initial h2 connection-level window (64 MiB). Shared across all
/// streams on the connection; must exceed peak-concurrent-streams ×
/// [`H2_INITIAL_STREAM_WINDOW`] or the conn window depletes and ALL
/// streams (including small unary RPCs) throttle to the rate of conn
/// `WINDOW_UPDATE` releases. Builder peak is `MAX_PARALLEL_FETCHES`
/// (16) + prefetch sem (8) = 24 streams × 1 MiB = 24 MiB; the previous
/// 16 MiB cap caused 29 s conn-update gaps under nix-bench
/// large-shallow → `BatchQueryPathInfo` 30 s timeouts (h2 frame trace:
/// stream 91 response dribbled at ~10 KB/s). 64 MiB gives ~2.5×
/// headroom over current peak. Goes away with ADR-022.
pub const H2_INITIAL_CONN_WINDOW: u32 = 64 * 1024 * 1024;

/// Default max gRPC message size: 256 MiB.
///
/// Sized for `MAX_DAG_NODES`-scale SubmitBuild requests: hello-deep-1024x at
/// 153,821 nodes serializes to ~120 MB (I-138). At the 1M-node cap, ~400 MB
/// — operators submitting near that scale should raise
/// `RIO_GRPC_MAX_MESSAGE_SIZE`. A streaming SubmitBuild would remove this
/// coupling entirely (followup).
pub const DEFAULT_MAX_MESSAGE_SIZE: usize = 256 * 1024 * 1024;

/// Read the max message size from the `RIO_GRPC_MAX_MESSAGE_SIZE` environment
/// variable, falling back to [`DEFAULT_MAX_MESSAGE_SIZE`] if not set or invalid.
///
/// Single underscore (not `__`): this is a direct env read, not the config
/// loader. The double underscore is the RIO_ env layer's nesting separator
/// — misleading here.
pub fn max_message_size() -> usize {
    crate::config::env_or("RIO_GRPC_MAX_MESSAGE_SIZE", DEFAULT_MAX_MESSAGE_SIZE)
}

/// Timeout for `SubmitBuild`.
///
/// I-070: scheduler `handle_merge_dag` for a 1085-node fresh-bootstrap
/// closure is ~49s (PG batch inserts ~20s + store cache-checks + first
/// dispatch). Subsequent merges of overlapping DAGs are ~10s (mostly
/// `ON CONFLICT`). 30s default fires mid-merge → reply receiver dropped
/// → build cancelled `client_disconnect_during_merge`. The gateway-side
/// translate (~210s for 1085 nodes) happens BEFORE this timeout starts.
/// 300s covers ~6k-node closures at the observed per-node rate.
pub const SUBMIT_BUILD_TIMEOUT: Duration = Duration::from_secs(300);

/// Wrap a gRPC call (or any fallible async op) with a timeout.
///
/// On timeout, returns `anyhow::Error` mentioning the operation name and
/// duration. On inner error, converts via `Into<anyhow::Error>`.
///
/// # Example
/// ```ignore
/// let info = with_timeout(
///     "QueryPathInfo",
///     DEFAULT_GRPC_TIMEOUT,
///     store_client.query_path_info(req),
/// ).await?;
/// ```
pub async fn with_timeout<T, E>(
    name: &'static str,
    timeout: Duration,
    fut: impl Future<Output = Result<T, E>>,
) -> anyhow::Result<T>
where
    E: Into<anyhow::Error>,
{
    tokio::time::timeout(timeout, fut)
        .await
        .map_err(|_| anyhow::anyhow!("gRPC call '{name}' timed out after {timeout:?}"))?
        .map_err(Into::into)
}

/// Like [`with_timeout`] but preserves `tonic::Status` for NotFound branching.
///
/// On timeout, returns `Status::deadline_exceeded(name)`. On inner error,
/// passes the Status through unchanged — callers can still match
/// `e.code() == Code::NotFound`.
///
/// # Example
/// ```ignore
/// match with_timeout_status(
///     "QueryPathInfo",
///     DEFAULT_GRPC_TIMEOUT,
///     store_client.query_path_info(req),
/// ).await {
///     Ok(resp) => ...,
///     Err(e) if e.code() == tonic::Code::NotFound => ...,
///     Err(e) => return Err(e.into()),
/// }
/// ```
pub async fn with_timeout_status<T>(
    name: &'static str,
    timeout: Duration,
    fut: impl Future<Output = Result<T, tonic::Status>>,
) -> Result<T, tonic::Status> {
    tokio::time::timeout(timeout, fut).await.map_err(|_| {
        tonic::Status::deadline_exceeded(format!("'{name}' timed out after {timeout:?}"))
    })?
}

/// Insert caller-supplied `(key, value)` pairs into a request's metadata
/// map.
///
/// Dedupe for the `for (k, v) in extra { req.metadata_mut().insert(...) }`
/// loop that appears at every client wrapper that threads
/// `x-rio-tenant-token` (or similar) onward. Values are parsed as
/// `MetadataValue<Ascii>`; a non-ASCII value returns
/// `Status::internal` (header values are caller-supplied, not
/// network-supplied — a parse failure here is a bug, not client input).
pub fn inject_metadata(md: &mut MetadataMap, extra: &[(&'static str, &str)]) -> Result<(), Status> {
    for (k, v) in extra {
        md.insert(
            *k,
            v.parse()
                .map_err(|e| Status::internal(format!("metadata {k}: {e}")))?,
        );
    }
    Ok(())
}

/// Return `InvalidArgument` if `got > max`.
///
/// Standard bounds-check for untrusted collection sizes at gRPC boundaries.
/// Dedupe for the `too many X: N (max M)` pattern that appears in every
/// request handler that accepts repeated fields.
pub fn check_bound(field: &str, got: usize, max: usize) -> Result<(), Status> {
    if got > max {
        return Err(Status::invalid_argument(format!(
            "too many {field}: {got} (max {max})"
        )));
    }
    Ok(())
}

/// Truncate `s` in place to at most `max` bytes, backing up to the nearest
/// UTF-8 character boundary, and release the excess allocation. No-op when
/// `s.len() <= max`.
///
/// For worker-supplied string fields that must be **bounded but not
/// rejected** — e.g. `BuildResult.error_msg`, where dropping the whole
/// `CompletionReport` would strand the derivation in `Running`. Reject-style
/// bounds (drop the message / fail the RPC) should use [`check_bound`] or an
/// explicit length comparison instead.
///
/// The `shrink_to_fit` is load-bearing, not a tidy-up: `String::truncate`
/// keeps the original allocation, so a field decoded at the gRPC message cap
/// (256 MiB) would report a 256-byte length while pinning a 256 MiB heap
/// block in the actor mailbox for as long as the containing command lives.
/// Same defense as the per-line `shrink_to_fit` in the LogBatch recv arm
/// (`executor_service.rs`, "Vec::truncate keeps the oversized capacity").
pub fn truncate_utf8(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s.shrink_to_fit();
}

/// Extension trait for mapping `Result<T, E: Display>` to `Result<T, Status>`
/// with a context prefix.
///
/// Dedupe for the `.map_err(|e| Status::X(format!("{ctx}: {e}")))?` pattern
/// that appears at every gRPC boundary that converts a typed error to a
/// client-visible status. The context string is the operator-facing prefix
/// (what failed); the error's `Display` is appended after `": "`.
///
/// # Example
/// ```ignore
/// // before
/// s.parse().map_err(|e| Status::invalid_argument(format!("invalid UUID: {e}")))?
/// // after
/// s.parse().status_invalid("invalid UUID")?
/// ```
pub trait StatusExt<T> {
    /// Log the full error at `error!` and map to `Status::internal(ctx)`.
    /// The runtime error text is NOT included in the returned status —
    /// see [`internal`].
    fn status_internal(self, ctx: &str) -> Result<T, Status>;
    /// Map the error to `Status::invalid_argument("{ctx}: {e}")`.
    fn status_invalid(self, ctx: &str) -> Result<T, Status>;
    /// Map the error to `Status::unavailable("{ctx}: {e}")`. Unlike
    /// [`Self::status_internal`] the error text is included: `Unavailable`
    /// is transient/retryable and the underlying detail (connect refused,
    /// S3 5xx) is the client's retry signal, not a server-fault leak.
    fn status_unavailable(self, ctx: &str) -> Result<T, Status>;
}

impl<T, E: Display> StatusExt<T> for Result<T, E> {
    fn status_internal(self, ctx: &str) -> Result<T, Status> {
        self.map_err(|e| internal(ctx, e))
    }
    fn status_invalid(self, ctx: &str) -> Result<T, Status> {
        self.map_err(|e| Status::invalid_argument(format!("{ctx}: {e}")))
    }
    fn status_unavailable(self, ctx: &str) -> Result<T, Status> {
        self.map_err(|e| Status::unavailable(format!("{ctx}: {e}")))
    }
}

/// Log the full error server-side and return `Status::internal(ctx)` —
/// the developer-authored context string only.
///
/// `Status::internal` is server-fault; the underlying error text (sqlx
/// connection strings, filesystem paths, backend SDK detail) is an
/// operator concern, not a client one. Log it; don't ship it. `ctx` is
/// hand-written at each call site and is safe to expose — it tells the
/// client *which* operation failed without leaking *why*.
///
/// `status_invalid` deliberately does NOT scrub: `InvalidArgument` is
/// client-fault, and the parse error tells the client what they sent
/// wrong.
///
/// Prefer [`StatusExt::status_internal`] (`result.status_internal(ctx)?`)
/// where the value is already a `Result`. This free fn is for match
/// arms / `bail!` sites where the trait form is awkward.
pub fn internal(ctx: &str, e: impl Display) -> Status {
    tracing::error!(context = ctx, error = %e, "internal error");
    Status::internal(ctx)
}

/// True if a gRPC status code represents a transient server-side
/// condition that might succeed on retry.
///
/// - `Unavailable` — server explicitly down (pod restarting,
///   follower-reject, connection refused).
/// - `Unknown` — transport disconnect: h2 connection reset, TLS close
///   mid-stream; what tonic surfaces when the peer goes away without a
///   gRPC-level status.
/// - `ResourceExhausted` — store's PG pool full (I-122). With ~400
///   ephemeral builders synchronously transitioning, output-path bursts
///   can briefly saturate even 8×200=1600 conns. Drains in <1s.
/// - `Aborted` — store's retryable PG conflict (Serialization, Deadlock
///   — see `rio-store::metadata`). The store says "retry" via Aborted
///   (I-189); without it the builder's no-manifest-hint fallback path
///   EIOs immediately on PG contention instead of backing off.
///
/// `DeadlineExceeded` is deliberately NOT transient: that's the caller's
/// own timeout firing — the peer hung past `fetch_timeout`. Retrying
/// with the same timeout won't help, and on a FUSE-thread caller the
/// next retry would compound the wait.
// r[impl builder.fuse.retry-jitter]
pub fn is_transient(code: tonic::Code) -> bool {
    matches!(
        code,
        tonic::Code::Unavailable
            | tonic::Code::Unknown
            | tonic::Code::ResourceExhausted
            | tonic::Code::Aborted
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_with_timeout_passes_through_fast_ok() -> anyhow::Result<()> {
        let result: anyhow::Result<u32> = with_timeout("fast-op", Duration::from_secs(1), async {
            Ok::<_, anyhow::Error>(42)
        })
        .await;
        assert_eq!(result?, 42);
        Ok(())
    }

    #[tokio::test]
    async fn test_with_timeout_passes_through_fast_err() {
        let result: anyhow::Result<()> = with_timeout("err-op", Duration::from_secs(1), async {
            Err::<(), _>(anyhow::anyhow!("inner error"))
        })
        .await;
        let err = result.unwrap_err();
        assert!(err.to_string().contains("inner error"));
    }

    #[tokio::test]
    async fn test_with_timeout_fires_on_slow_future() {
        let result: anyhow::Result<()> =
            with_timeout("slow-op", Duration::from_millis(10), async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                Ok::<_, anyhow::Error>(())
            })
            .await;
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("slow-op") && err.to_string().contains("timed out"),
            "error should mention op name and timeout: {err}"
        );
    }

    #[tokio::test]
    async fn test_with_timeout_status_preserves_not_found() {
        let result = with_timeout_status("test", Duration::from_secs(1), async {
            Err::<(), _>(tonic::Status::not_found("missing"))
        })
        .await;
        assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn test_with_timeout_status_on_timeout() {
        let result = with_timeout_status("slow", Duration::from_millis(10), async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok::<(), tonic::Status>(())
        })
        .await;
        assert_eq!(result.unwrap_err().code(), tonic::Code::DeadlineExceeded);
    }

    #[test]
    fn test_status_ext_formats_context_and_error() {
        let r: Result<(), &str> = Err("parse failed");
        let s = r.status_invalid("bad field").unwrap_err();
        assert_eq!(s.code(), tonic::Code::InvalidArgument);
        assert_eq!(s.message(), "bad field: parse failed");

        // status_internal scrubs: error text logged, NOT in the message.
        let r: Result<(), std::io::Error> = Err(std::io::Error::other("boom"));
        let s = r.status_internal("write").unwrap_err();
        assert_eq!(s.code(), tonic::Code::Internal);
        assert_eq!(s.message(), "write");
        assert!(!s.message().contains("boom"));

        // Ok passes through unchanged.
        let r: Result<u32, &str> = Ok(7);
        assert_eq!(r.status_internal("unused").unwrap(), 7);
    }

    /// I-189: store returns `Aborted` for retryable PG conflicts
    /// (Serialization, Deadlock). Callers must retry, not surface EIO.
    // r[verify builder.fuse.retry-jitter]
    #[test]
    fn test_is_transient_classification() {
        assert!(is_transient(tonic::Code::Aborted));
        assert!(is_transient(tonic::Code::Unavailable));
        assert!(is_transient(tonic::Code::Unknown));
        assert!(is_transient(tonic::Code::ResourceExhausted));
        // Non-transient: DeadlineExceeded is the caller's own timeout;
        // DataLoss is permanent corruption.
        assert!(!is_transient(tonic::Code::DeadlineExceeded));
        assert!(!is_transient(tonic::Code::DataLoss));
        assert!(!is_transient(tonic::Code::NotFound));
    }

    #[test]
    fn test_timeout_constants_ordering() {
        assert!(
            DEFAULT_GRPC_TIMEOUT < GRPC_STREAM_TIMEOUT,
            "metadata timeout should be shorter than stream timeout"
        );
        // Stream timeout (300s) is shorter than any sane build
        // timeout (rio-builder Config.build_timeout_secs, default
        // 7200s).
    }

    #[test]
    fn truncate_utf8_under_max_is_noop() {
        let mut s = "hello".to_string();
        truncate_utf8(&mut s, 16);
        assert_eq!(s, "hello");
        // Exactly at the bound is also a no-op.
        let mut s = "hello".to_string();
        truncate_utf8(&mut s, 5);
        assert_eq!(s, "hello");
    }

    #[test]
    fn truncate_utf8_ascii_over_max_truncates_to_max() {
        let mut s = "a".repeat(100);
        truncate_utf8(&mut s, 64);
        assert_eq!(s.len(), 64);
    }

    #[test]
    fn truncate_utf8_backs_off_to_char_boundary() {
        // A 4-byte char straddling the boundary: max-2 ASCII bytes then
        // a 4-byte crab. Truncating at `max` would land mid-crab;
        // the helper must back off to the boundary and not panic.
        let max = 16;
        let mut s = format!("{}🦀", "a".repeat(max - 2));
        assert!(s.len() > max);
        truncate_utf8(&mut s, max);
        assert_eq!(s.len(), max - 2, "backs off to the char boundary");
        assert!(s.chars().all(|c| c == 'a'));
    }

    #[test]
    fn truncate_utf8_max_zero_empties() {
        let mut s = "abc".to_string();
        truncate_utf8(&mut s, 0);
        assert!(s.is_empty());
    }

    #[test]
    fn truncate_utf8_over_max_releases_capacity() {
        // bug_011: a worker-supplied field decoded at the gRPC message cap
        // arrives as a String whose capacity == its wire length. `truncate()`
        // alone keeps that allocation, so a 256 MiB executor_id/error_msg
        // would report a 256 B / 16 KiB length while pinning 256 MiB in the
        // actor mailbox. Mirror of `push_truncates_oversized_line`'s
        // capacity assertion for the per-line truncation.
        let mut s = "a".repeat(1024 * 1024);
        assert!(s.capacity() >= 1024 * 1024);
        truncate_utf8(&mut s, 64);
        assert_eq!(s.len(), 64);
        assert!(
            s.capacity() <= 64,
            "truncate_utf8 must release the over-allocation, got capacity {} \
             — truncate-without-shrink keeps the original 1 MiB block",
            s.capacity()
        );
    }
}
