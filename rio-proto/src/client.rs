//! gRPC client connection and streaming helpers.
//!
//! - Connection: `connect_*` build `"http://{addr}"` from a `host:port`
//!   string, connect, and apply [`max_message_size`](crate::max_message_size).
//! - NAR streaming: [`collect_nar_stream`] drains `GetPath` responses;
//!   [`chunk_nar_for_put`] builds a lazy `PutPath` request stream.

use std::sync::Arc;
use std::time::Duration;

use tokio_stream::{Stream, StreamExt};
use tonic::Streaming;
use tonic::transport::Channel;

use crate::StoreServiceClient;
use crate::types::{
    GetPathRequest, GetPathResponse, PathInfo, PutPathMetadata, PutPathRequest,
    QueryPathInfoRequest, get_path_response, put_path_request,
};
use crate::validated::ValidatedPathInfo;

/// Unified chunk size for NAR streaming (256 KiB).
///
/// Previously inconsistent: gateway used 64 KiB, worker used 256 KiB.
/// 256 KiB reduces per-chunk overhead with negligible latency impact.
pub const NAR_CHUNK_SIZE: usize = 256 * 1024;

/// Connect to a gRPC endpoint at `host:port` and return a raw [`Channel`].
///
/// The caller wraps in `XServiceClient::new(channel)`. Use the per-service
/// helpers below unless you need to share one channel across multiple clients.
pub async fn connect_channel(addr: &str) -> anyhow::Result<Channel> {
    let endpoint = format!("http://{addr}");
    Channel::from_shared(endpoint)?
        .connect()
        .await
        .map_err(Into::into)
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

/// Build a `PutPath` request stream: metadata message first, then
/// [`NAR_CHUNK_SIZE`] chunks of the NAR.
///
/// Takes `Arc<[u8]>` so chunks borrow without eagerly materializing
/// a `Vec<PutPathRequest>`. The returned stream is lazy — chunks are
/// sliced on demand as tonic polls, keeping at most a few chunks in
/// flight at once.
pub fn chunk_nar_for_put(
    info: ValidatedPathInfo,
    nar: Arc<[u8]>,
) -> impl Stream<Item = PutPathRequest> + Send + 'static {
    let metadata = PutPathRequest {
        msg: Some(put_path_request::Msg::Metadata(PutPathMetadata {
            info: Some(info.into()),
        })),
    };

    let total = nar.len();
    let chunks = tokio_stream::iter((0..total).step_by(NAR_CHUNK_SIZE)).map(move |start| {
        let end = (start + NAR_CHUNK_SIZE).min(total);
        PutPathRequest {
            msg: Some(put_path_request::Msg::NarChunk(nar[start..end].to_vec())),
        }
    });

    tokio_stream::once(metadata).chain(chunks)
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
    let req = QueryPathInfoRequest {
        store_path: store_path.to_string(),
    };
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
    let req = GetPathRequest {
        store_path: store_path.to_string(),
    };
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
