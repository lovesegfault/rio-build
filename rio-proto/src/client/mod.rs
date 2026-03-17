//! gRPC client connection and streaming helpers.
//!
//! - Connection: `connect_*` build `"http(s)://{addr}"` from a `host:port`
//!   string, apply global TLS config if [`init_client_tls`] was called,
//!   connect, and apply [`max_message_size`](crate::max_message_size).
//! - NAR streaming: [`collect_nar_stream`] drains `GetPath` responses;
//!   [`chunk_nar_for_put`] builds a lazy `PutPath` request stream.

pub mod balance;
pub use balance::BalancedChannel;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio_stream::{Stream, StreamExt};
use tonic::Streaming;
use tonic::transport::{Channel, ClientTlsConfig};

use crate::StoreServiceClient;
use crate::types::{
    GetPathRequest, GetPathResponse, PathInfo, PutPathMetadata, PutPathRequest, PutPathTrailer,
    QueryPathInfoRequest, get_path_response, put_path_request,
};
use crate::validated::ValidatedPathInfo;

/// Unified chunk size for NAR streaming (256 KiB).
///
/// 256 KiB reduces per-chunk overhead vs. smaller chunk sizes, with
/// negligible latency impact.
pub const NAR_CHUNK_SIZE: usize = 256 * 1024;

/// Process-global client TLS config. Set once via [`init_client_tls`] in
/// each binary's `main()` AFTER config load but BEFORE any `connect_*`.
///
/// Why a global instead of threading `ClientTlsConfig` through every
/// connect call: the controller's reconcilers connect lazily per-reconcile
/// (`Ctx` holds only `String` addrs — see `rio-controller/src/reconcilers/
/// mod.rs`). Threading TLS config through ~11 call sites + 4 wrapper fns
/// is invasive. A OnceLock initialized once in main() is the minimal
/// change — and TLS config IS process-global (same cert for all outgoing
/// connections; we don't vary it per target).
///
/// `None` in the OnceLock = plaintext (init_client_tls called with None,
/// or never called at all). Both mean "TLS not configured."
static CLIENT_TLS: OnceLock<Option<ClientTlsConfig>> = OnceLock::new();

/// Set the process-wide client TLS config. Call ONCE in each binary's
/// main(), after loading TlsConfig but before any `connect_*`.
///
/// `None` → plaintext (http://). `Some` → TLS (https:// + the given
/// config). Calling twice is a silent no-op (OnceLock semantics) — the
/// first call wins. Tests that need to re-init should use a fresh
/// process (nextest's default) or accept the first-wins behavior.
pub fn init_client_tls(cfg: Option<ClientTlsConfig>) {
    // `let _`: set() returns Err if already set. Not an error —
    // just means another call raced us (main-only → shouldn't
    // happen) or tests re-init (first wins, fine).
    let _ = CLIENT_TLS.set(cfg);
}

/// Crate-internal accessor for the global TLS config. Used by
/// `balance.rs` to build per-endpoint channels with a domain_name
/// override (connect to pod IP, verify against service SAN).
pub(crate) fn client_tls() -> Option<ClientTlsConfig> {
    CLIENT_TLS.get().and_then(|o| o.as_ref()).cloned()
}

/// Connect to a gRPC endpoint at `host:port` and return a raw [`Channel`].
///
/// Scheme and TLS wiring depend on the process-global [`CLIENT_TLS`]:
/// set → `https://` + `.tls_config()`; unset → `http://` (plaintext).
/// The global is initialized by [`init_client_tls`] in each binary's
/// main; if main doesn't call it (or calls it with `None`), plaintext.
///
/// 10s connect timeout: tonic's default is UNBOUNDED. A stale address
/// (e.g., scheduler pod killed → replacement has new IP, but DNS TTL /
/// caller's cached addr hasn't updated) hangs forever on TCP SYN.
/// Observed in lifecycle test: controller's cleanup() never logged
/// "starting drain" — stuck in connect_admin after the scheduler
/// leader was killed mid-run. 10s is enough for a real connect
/// (even cross-AZ) and bounds the failure mode.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

async fn connect_channel(addr: &str) -> anyhow::Result<Channel> {
    // `get().and_then(|o| o.as_ref())` collapses both "OnceLock not
    // initialized" and "initialized with None" to plaintext. Tests
    // that never call init_client_tls stay plaintext.
    match CLIENT_TLS.get().and_then(|o| o.as_ref()) {
        Some(tls) => {
            let endpoint = format!("https://{addr}");
            Channel::from_shared(endpoint)?
                .connect_timeout(CONNECT_TIMEOUT)
                .tls_config(tls.clone())?
                .connect()
                .await
                .map_err(Into::into)
        }
        None => {
            let endpoint = format!("http://{addr}");
            Channel::from_shared(endpoint)?
                .connect_timeout(CONNECT_TIMEOUT)
                .connect()
                .await
                .map_err(Into::into)
        }
    }
}

/// Connect to the store service.
pub async fn connect_store(addr: &str) -> anyhow::Result<StoreServiceClient<Channel>> {
    let ch = connect_channel(addr).await?;
    Ok(StoreServiceClient::new(ch)
        .max_decoding_message_size(crate::max_message_size())
        .max_encoding_message_size(crate::max_message_size()))
}

/// Connect to the scheduler service (gateway-facing).
pub async fn connect_scheduler(
    addr: &str,
) -> anyhow::Result<crate::SchedulerServiceClient<Channel>> {
    let ch = connect_channel(addr).await?;
    Ok(crate::SchedulerServiceClient::new(ch)
        .max_decoding_message_size(crate::max_message_size())
        .max_encoding_message_size(crate::max_message_size()))
}

/// Connect to the worker service (worker-facing scheduler RPCs).
pub async fn connect_worker(addr: &str) -> anyhow::Result<crate::WorkerServiceClient<Channel>> {
    let ch = connect_channel(addr).await?;
    Ok(crate::WorkerServiceClient::new(ch)
        .max_decoding_message_size(crate::max_message_size())
        .max_encoding_message_size(crate::max_message_size()))
}

/// Connect to the admin service (controller + worker preStop).
///
/// Same address as `connect_worker` — AdminService is hosted on the
/// scheduler's gRPC port alongside SchedulerService/WorkerService.
/// The worker's SIGTERM handler uses this for `DrainWorker` (step 1
/// of preStop); the controller uses it for `ClusterStatus` autoscaling.
pub async fn connect_admin(addr: &str) -> anyhow::Result<crate::AdminServiceClient<Channel>> {
    let ch = connect_channel(addr).await?;
    Ok(crate::AdminServiceClient::new(ch)
        .max_decoding_message_size(crate::max_message_size())
        .max_encoding_message_size(crate::max_message_size()))
}

/// Connect to the store admin service (scheduler's TriggerGC proxy).
///
/// Same address as `connect_store` — StoreAdminService is hosted on
/// the store's gRPC port alongside StoreService/ChunkService. The
/// scheduler's `AdminService.TriggerGC` populates extra_roots from
/// GcRoots and proxies here.
pub async fn connect_store_admin(
    addr: &str,
) -> anyhow::Result<crate::StoreAdminServiceClient<Channel>> {
    let ch = connect_channel(addr).await?;
    Ok(crate::StoreAdminServiceClient::new(ch)
        .max_decoding_message_size(crate::max_message_size())
        .max_encoding_message_size(crate::max_message_size()))
}

// ===========================================================================
// NAR stream helpers
// ===========================================================================

/// Error from [`collect_nar_stream`].
#[derive(Debug, thiserror::Error)]
pub enum NarCollectError {
    #[error("gRPC stream error: {0}")]
    Stream(#[from] tonic::Status),
    #[error("NAR exceeds maximum size: {got} > {limit}")]
    SizeExceeded { got: u64, limit: u64 },
    #[error("store returned malformed PathInfo: {0}")]
    Validation(#[from] crate::validated::PathInfoValidationError),
}

impl NarCollectError {
    /// True if this error represents a server-side NotFound (path doesn't exist).
    pub fn is_not_found(&self) -> bool {
        matches!(self, NarCollectError::Stream(s) if s.code() == tonic::Code::NotFound)
    }
}

/// Drain a `GetPath` response stream into `(Option<PathInfo>, nar_bytes)`.
///
/// Enforces `max_size` (callers typically pass `rio_common::limits::MAX_NAR_SIZE`).
/// Ignores empty messages (proto-mismatch safety).
pub async fn collect_nar_stream(
    stream: &mut Streaming<GetPathResponse>,
    max_size: u64,
) -> Result<(Option<PathInfo>, Vec<u8>), NarCollectError> {
    let mut info = None;
    let mut nar = Vec::new();

    while let Some(msg) = stream.message().await? {
        match msg.msg {
            Some(get_path_response::Msg::Info(i)) => info = Some(i),
            Some(get_path_response::Msg::NarChunk(chunk)) => {
                let new_len = (nar.len() as u64).saturating_add(chunk.len() as u64);
                if new_len > max_size {
                    return Err(NarCollectError::SizeExceeded {
                        got: new_len,
                        limit: max_size,
                    });
                }
                nar.extend_from_slice(&chunk);
            }
            None => {} // empty oneof — ignore
        }
    }
    Ok((info, nar))
}

/// Build a `PutPath` request stream: metadata first, then [`NAR_CHUNK_SIZE`]
/// chunks of the NAR, then a `PutPathTrailer` with the hash/size.
///
/// Trailer-mode only: `nar_hash`/`nar_size` are zeroed in the metadata
/// PathInfo and sent in the trailer. This is the ONLY PutPath mode the
/// store accepts.
///
/// Takes `Arc<[u8]>` so chunks borrow without eagerly materializing
/// a `Vec<PutPathRequest>`. The returned stream is lazy — chunks are
/// sliced on demand as tonic polls, keeping at most a few chunks in
/// flight at once.
pub fn chunk_nar_for_put(
    info: ValidatedPathInfo,
    nar: Arc<[u8]>,
) -> impl Stream<Item = PutPathRequest> + Send + 'static {
    // Extract hash/size for the trailer, then zero them in the metadata.
    // The store fills a placeholder, reads all chunks, then validates
    // against the trailer (same security property as before — server-
    // computed digest must match client-declared).
    let mut raw: PathInfo = info.into();
    let trailer = PutPathTrailer {
        nar_hash: std::mem::take(&mut raw.nar_hash),
        nar_size: std::mem::take(&mut raw.nar_size),
    };

    let metadata = PutPathRequest {
        msg: Some(put_path_request::Msg::Metadata(PutPathMetadata {
            info: Some(raw),
        })),
    };

    let total = nar.len();
    let chunks = tokio_stream::iter((0..total).step_by(NAR_CHUNK_SIZE)).map(move |start| {
        let end = (start + NAR_CHUNK_SIZE).min(total);
        PutPathRequest {
            msg: Some(put_path_request::Msg::NarChunk(nar[start..end].to_vec())),
        }
    });

    let trailer = PutPathRequest {
        msg: Some(put_path_request::Msg::Trailer(trailer)),
    };

    tokio_stream::once(metadata)
        .chain(chunks)
        .chain(tokio_stream::once(trailer))
}

// ===========================================================================
// High-level store operations with timeout + NotFound handling
// ===========================================================================

/// QueryPathInfo with timeout. Returns `None` if the store reports NotFound.
///
/// Validates the returned `PathInfo` — a malformed response (bad store
/// path, wrong-length nar_hash, bad reference) is propagated as an error,
/// not silently passed through. Collapses the common
/// `match { Ok => Some, NotFound => None, Err => Err }` pattern.
pub async fn query_path_info_opt(
    client: &mut StoreServiceClient<Channel>,
    store_path: &str,
    timeout: Duration,
) -> Result<Option<ValidatedPathInfo>, tonic::Status> {
    let mut req = tonic::Request::new(QueryPathInfoRequest {
        store_path: store_path.to_string(),
    });
    crate::interceptor::inject_current(req.metadata_mut());
    match tokio::time::timeout(timeout, client.query_path_info(req)).await {
        Ok(Ok(resp)) => {
            let validated = ValidatedPathInfo::try_from(resp.into_inner()).map_err(|e| {
                tonic::Status::internal(format!("store returned malformed PathInfo: {e}"))
            })?;
            Ok(Some(validated))
        }
        Ok(Err(status)) if status.code() == tonic::Code::NotFound => Ok(None),
        Ok(Err(status)) => Err(status),
        Err(_) => Err(tonic::Status::deadline_exceeded(format!(
            "QueryPathInfo timed out after {timeout:?}"
        ))),
    }
}

/// GetPath with timeout, full NAR collection, and NotFound handling.
///
/// Combines the `GetPath → collect_nar_stream → NotFound-branch` pattern.
/// The whole operation (initial call + stream drain) is bounded by `timeout`.
/// Returns `None` if the path doesn't exist or the stream contains no PathInfo.
pub async fn get_path_nar(
    client: &mut StoreServiceClient<Channel>,
    store_path: &str,
    timeout: Duration,
    max_nar_size: u64,
) -> Result<Option<(ValidatedPathInfo, Vec<u8>)>, NarCollectError> {
    let mut req = tonic::Request::new(GetPathRequest {
        store_path: store_path.to_string(),
    });
    crate::interceptor::inject_current(req.metadata_mut());
    let fut = async {
        let mut stream = match client.get_path(req).await {
            Ok(resp) => resp.into_inner(),
            Err(status) if status.code() == tonic::Code::NotFound => return Ok(None),
            Err(status) => return Err(NarCollectError::Stream(status)),
        };
        let (info, nar) = collect_nar_stream(&mut stream, max_nar_size).await?;
        match info {
            Some(raw) => {
                let validated = ValidatedPathInfo::try_from(raw)?;
                Ok(Some((validated, nar)))
            }
            None => Ok(None),
        }
    };
    match tokio::time::timeout(timeout, fut).await {
        Ok(r) => r,
        Err(_) => Err(NarCollectError::Stream(tonic::Status::deadline_exceeded(
            format!("GetPath({store_path}) timed out after {timeout:?}"),
        ))),
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use std::time::Duration;

    /// Retry loop pattern: connect to a closed port, assert it fails
    /// fast (not hang), bind the port, assert next attempt succeeds.
    /// This is the contract the main.rs retry loops depend on:
    /// closed port = fast Err, not 10s CONNECT_TIMEOUT hang.
    ///
    /// NOT start_paused: real TCP sockets + auto-advancing mock clock
    /// fires CONNECT_TIMEOUT spuriously while the kernel does real work.
    #[tokio::test]
    async fn connect_closed_port_fails_fast_then_succeeds() {
        // Reserve a port, then close the listener — port is now free
        // but nothing's listening. connect() should get ECONNREFUSED
        // in <100ms (kernel fast-path, no SYN retry).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        // First attempt: refused, fast.
        let t0 = std::time::Instant::now();
        let err = connect_channel(&addr.to_string()).await.unwrap_err();
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "closed port should fail fast (ECONNREFUSED), got {err:?} after {:?}",
            t0.elapsed()
        );

        // Bind a real gRPC server on that port. connect_channel only
        // needs the transport (HTTP/2 handshake) to come up — a
        // tonic-health service suffices (already a non-dev dep for
        // balance.rs, same pattern as balance.rs:426-439).
        let (_reporter, health_svc) = tonic_health::server::health_reporter();
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        let server = tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(health_svc)
                .serve_with_incoming(incoming),
        );

        // Simulate the retry loop: poll until Ok. Bounded at 10 tries
        // (= 20s with the real 2s sleep; here 50ms so the test is fast).
        let mut ch = None;
        for _ in 0..10 {
            match connect_channel(&addr.to_string()).await {
                Ok(c) => {
                    ch = Some(c);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
        assert!(ch.is_some(), "connect never succeeded after port opened");

        server.abort();
    }
}
