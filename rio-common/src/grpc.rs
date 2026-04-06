//! Shared helpers for gRPC client calls.
//!
//! Missing timeouts on gRPC calls are a systemic footgun in a distributed
//! system: a hung store/scheduler causes cascading hangs in gateway sessions,
//! worker FUSE mounts, and the scheduler actor's event loop. This module
//! provides consistent timeout bounds and a helper to wrap calls.

use std::future::Future;
use std::time::Duration;

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
pub fn check_bound(field: &str, got: usize, max: usize) -> Result<(), tonic::Status> {
    if got > max {
        return Err(tonic::Status::invalid_argument(format!(
            "too many {field}: {got} (max {max})"
        )));
    }
    Ok(())
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
    fn test_timeout_constants_ordering() {
        assert!(
            DEFAULT_GRPC_TIMEOUT < GRPC_STREAM_TIMEOUT,
            "metadata timeout should be shorter than stream timeout"
        );
        // Stream timeout (300s) is shorter than any sane daemon build
        // timeout (rio-worker Config.daemon_timeout_secs, default
        // 7200s). The ordering invariant is enforced at
        // rio-worker/src/executor/daemon.rs test_timeout_ordering.
    }
}
