//! Proto-agnostic gRPC helpers shared across rio binaries.
//!
//! **Layering rule:** anything that depends on `tonic` types but NOT on
//! generated proto types belongs here — timeout wrappers, [`StatusExt`],
//! [`check_bound`], the `CLIENT_TLS` OnceLock, [`max_message_size`], h2
//! tuning, the shared retry predicate, backoff/jitter, `x-rio-*` metadata
//! key constants. Anything that names a generated client or message type
//! (`connect_store`, `BalancedChannel`, NAR stream chunk/collect) belongs
//! in `rio-proto::client`. `rio-proto` depends on this crate, not the
//! other way round, so `rio-controller` can take a backoff helper without
//! pulling in the whole proto crate.
//!
//! Missing timeouts on gRPC calls are a systemic footgun in a distributed
//! system: a hung store/scheduler causes cascading hangs in gateway sessions,
//! worker FUSE mounts, and the scheduler actor's event loop. This module
//! provides consistent timeout bounds and a helper to wrap calls.

use std::fmt::Display;
use std::future::Future;
use std::sync::OnceLock;
use std::time::Duration;

use tonic::Status;
use tonic::transport::ClientTlsConfig;

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
/// Single underscore (not `__`): this is a direct env read, not figment.
/// The double underscore is figment's nesting separator — misleading here.
pub fn max_message_size() -> usize {
    match std::env::var("RIO_GRPC_MAX_MESSAGE_SIZE") {
        Ok(val) => match val.parse::<usize>() {
            Ok(size) => size,
            Err(_) => {
                // Direct env read, pre-tracing-init — eprintln not warn!.
                eprintln!(
                    "warning: invalid RIO_GRPC_MAX_MESSAGE_SIZE={val:?}, expected bytes as a positive integer; defaulting to {DEFAULT_MAX_MESSAGE_SIZE}"
                );
                DEFAULT_MAX_MESSAGE_SIZE
            }
        },
        Err(_) => DEFAULT_MAX_MESSAGE_SIZE,
    }
}

/// Process-global client TLS config. Set once via [`init_client_tls`] in
/// each binary's `main()` (via [`crate::server::bootstrap`]) AFTER config
/// load but BEFORE any `connect_*`.
///
/// Why a global instead of threading `ClientTlsConfig` through every
/// connect call: the controller's reconcilers connect lazily per-reconcile,
/// holding only `String` addrs. Threading TLS config through ~11 call
/// sites + 4 wrapper fns is invasive. A OnceLock initialized once in
/// main() is the minimal change — and TLS config IS process-global (same
/// cert for all outgoing connections; we don't vary it per target).
///
/// `None` in the OnceLock = plaintext (init_client_tls called with None,
/// or never called at all). Both mean "TLS not configured."
static CLIENT_TLS: OnceLock<Option<ClientTlsConfig>> = OnceLock::new();

/// Set the process-wide client TLS config. Call ONCE in each binary's
/// main(), after loading TlsConfig but before any `connect_*`.
///
/// `None` → plaintext (`http://`). `Some` → TLS (`https://` + the given
/// config). Calling twice is a silent no-op (OnceLock semantics) — the
/// first call wins. Tests that need to re-init should use a fresh
/// process (nextest's default) or accept the first-wins behavior.
pub fn init_client_tls(cfg: Option<ClientTlsConfig>) {
    // `let _`: set() returns Err if already set. Not an error —
    // just means another call raced us (main-only → shouldn't
    // happen) or tests re-init (first wins, fine).
    let _ = CLIENT_TLS.set(cfg);
}

/// Read the process-global client TLS config. `None` if [`init_client_tls`]
/// was never called or was called with `None` (plaintext). Used by
/// `rio-proto::client` to wire scheme + `.tls_config()` on each
/// `connect_*`, and by `BalancedChannel` to build per-endpoint channels
/// with a `domain_name` override.
pub fn client_tls() -> Option<ClientTlsConfig> {
    CLIENT_TLS.get().and_then(|o| o.as_ref()).cloned()
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
}

impl<T, E: Display> StatusExt<T> for Result<T, E> {
    fn status_internal(self, ctx: &str) -> Result<T, Status> {
        self.map_err(|e| internal(ctx, e))
    }
    fn status_invalid(self, ctx: &str) -> Result<T, Status> {
        self.map_err(|e| Status::invalid_argument(format!("{ctx}: {e}")))
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

    #[test]
    fn test_timeout_constants_ordering() {
        assert!(
            DEFAULT_GRPC_TIMEOUT < GRPC_STREAM_TIMEOUT,
            "metadata timeout should be shorter than stream timeout"
        );
        // Stream timeout (300s) is shorter than any sane daemon build
        // timeout (rio-builder Config.daemon_timeout_secs, default
        // 7200s). The ordering invariant is enforced at
        // rio-builder/src/executor/daemon.rs test_timeout_ordering.
    }
}
