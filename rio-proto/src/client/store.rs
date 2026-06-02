//! `StoreService` NAR-streaming and high-level RPC helpers.
//!
//! Split from `client/mod.rs` (Arch#16): the parent module is generic
//! transport (channel construction, h2 tuning, [`ProtoClient`]); this
//! module is `StoreService`-specific data-plane helpers — NAR
//! collect/chunk loops and the timeout/NotFound/validation wrappers
//! for `QueryPathInfo` / `GetPath` / `BatchQueryPathInfo` /
//! `BatchGetManifest`. Everything here is re-exported at
//! `rio_proto::client::*` so callers are unchanged.
//!
//! [`ProtoClient`]: super::ProtoClient

use std::sync::Arc;
use std::time::Duration;

use rio_common::grpc::{inject_metadata, with_timeout_status};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio_stream::{Stream, StreamExt};
use tonic::Streaming;
use tonic::transport::Channel;

use crate::StoreServiceClient;
use crate::types::{
    BatchGetManifestRequest, BatchQueryPathInfoRequest, GetPathRequest, GetPathResponse, PathInfo,
    PutPathMetadata, PutPathRequest, PutPathTrailer, QueryPathInfoRequest, get_path_response,
    put_path_request,
};
use crate::validated::ValidatedPathInfo;

/// Unified chunk size for NAR streaming (256 KiB).
///
/// 256 KiB reduces per-chunk overhead vs. smaller chunk sizes, with
/// negligible latency impact.
pub const NAR_CHUNK_SIZE: usize = 256 * 1024;

// ===========================================================================
// NAR stream helpers
// ===========================================================================

/// Error from [`collect_nar_stream`] / [`collect_nar_stream_to_writer`].
#[derive(Debug, thiserror::Error)]
pub enum NarCollectError {
    #[error("gRPC stream error: {0}")]
    Stream(#[from] tonic::Status),
    #[error("NAR exceeds maximum size: {got} > {limit}")]
    SizeExceeded { got: u64, limit: u64 },
    #[error("store returned malformed PathInfo: {0}")]
    Validation(#[from] crate::validated::PathInfoValidationError),
    /// Spool write failure ([`collect_nar_stream_to_writer`] only).
    /// NOT transient — retrying the gRPC won't fix local disk.
    #[error("NAR spool write failed: {0}")]
    Io(#[from] std::io::Error),
}

impl NarCollectError {
    /// True if this error represents a server-side NotFound (path doesn't exist).
    pub fn is_not_found(&self) -> bool {
        matches!(self, NarCollectError::Stream(s) if s.code() == tonic::Code::NotFound)
    }

    /// True if this is a transient transport-level failure that might
    /// succeed on retry — `Unavailable` (server explicitly down: pod
    /// restarting, follower-reject) or `Unknown` (transport disconnect:
    /// h2 connection reset, TLS close mid-stream, what tonic surfaces
    /// when the peer goes away without a gRPC-level status).
    ///
    /// `DeadlineExceeded` is NOT transient: that's [`get_path_nar`]'s
    /// per-chunk idle timeout firing — the store stalled (no chunk for
    /// `timeout`). Retrying with the same idle bound won't help, and
    /// the next retry would compound the wait on a FUSE-thread caller.
    pub fn is_transient(&self) -> bool {
        matches!(self, NarCollectError::Stream(s) if rio_common::grpc::is_transient(s.code()))
    }

    /// True if the server rejected the request as malformed
    /// (`InvalidArgument`). For `GetPath` this means the store-path
    /// string didn't parse — a per-request verdict, not a "store is
    /// sick" signal. Callers should treat it as definitively absent
    /// (ENOENT), NOT as a retry-worthy or breaker-worthy failure.
    pub fn is_invalid_argument(&self) -> bool {
        matches!(self, NarCollectError::Stream(s) if s.code() == tonic::Code::InvalidArgument)
    }
}

/// Drain a `GetPath` response stream into `(Option<PathInfo>, nar_bytes)`.
///
/// Enforces `max_size` (callers typically pass `rio_common::limits::MAX_NAR_SIZE`).
/// Ignores empty messages (proto-mismatch safety).
///
/// `idle_timeout`: when `Some`, bounds the time between successive stream
/// messages — a stalled store (no chunks arriving) trips at the timeout,
/// but a healthy-but-slow store (steady chunks) completes regardless of
/// total NAR size. `None` = unbounded. Mirrors
/// [`collect_nar_stream_to_writer`].
pub async fn collect_nar_stream(
    stream: &mut Streaming<GetPathResponse>,
    max_size: u64,
    idle_timeout: Option<Duration>,
) -> Result<(Option<PathInfo>, Vec<u8>), NarCollectError> {
    let mut info = None;
    let mut nar = Vec::new();

    loop {
        // Per-message idle timeout — same I-211 pattern as
        // collect_nar_stream_to_writer. Each `stream.message()` is a
        // fresh deadline; receiving any message (Info or NarChunk) is
        // progress. h2 keepalive PINGs are answered transport-side
        // regardless of application progress, so without this a store
        // that stalls mid-stream holds the caller indefinitely.
        let next = match idle_timeout {
            Some(t) => tokio::time::timeout(t, stream.message())
                .await
                .map_err(|_| {
                    NarCollectError::Stream(tonic::Status::deadline_exceeded(format!(
                        "GetPath stream idle for {t:?} (no chunk received)"
                    )))
                })??,
            None => stream.message().await?,
        };
        let Some(msg) = next else { break };
        match msg.msg {
            Some(get_path_response::Msg::Info(i)) => {
                // I-180 fix C: server sends Info first with the final
                // nar_size. Pre-size the Vec so the chunk loop doesn't
                // realloc-memcpy ~log2(size) times (a 1.8 GB NAR
                // doubling from empty is ~31 reallocs ≈ 3.6 GB of
                // memcpy). Clamp to max_size so a hostile/buggy server
                // can't make us OOM on the reserve itself.
                nar.reserve_exact((i.nar_size as usize).min(max_size as usize));
                info = Some(i);
            }
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

/// Drain a `GetPath` response stream into an `AsyncWrite` sink.
///
/// Same loop as [`collect_nar_stream`] but writes each chunk to `w`
/// instead of accumulating into a `Vec<u8>`. Peak heap is one chunk
/// (256 KiB), not the whole NAR. Returns `(Option<PathInfo>, bytes_written)`.
///
/// I-180: the builder's FUSE fetch path uses this to spool GB-scale NARs
/// to a tempfile, then [`rio_nix::nar::restore_path_streaming`] extracts
/// from the spool. The in-memory [`collect_nar_stream`] stays for
/// small-payload callers (.drv files).
///
/// On error, `w` may have been partially written — caller is responsible
/// for truncating/discarding the spool before retry.
///
/// `idle_timeout`: when `Some`, bounds the time between successive stream
/// messages — a stalled store (no chunks arriving) trips at the timeout,
/// but a healthy-but-slow store (steady chunks) completes regardless of
/// total NAR size. `None` = unbounded (caller wraps the whole call).
pub async fn collect_nar_stream_to_writer(
    stream: &mut Streaming<GetPathResponse>,
    max_size: u64,
    idle_timeout: Option<Duration>,
    w: &mut (impl AsyncWrite + Unpin),
) -> Result<(Option<PathInfo>, u64), NarCollectError> {
    let mut info = None;
    let mut written: u64 = 0;

    loop {
        // r[impl builder.fuse.fetch-progress-timeout+2]
        // I-211: per-message idle timeout. Each `stream.message()` is a
        // fresh deadline; receiving any message (Info or NarChunk) is
        // progress. A wall-clock bound on the whole loop would conflate
        // "stuck" with "large" — a 2.9 GB clang-debug NAR cannot complete
        // in 60s even on a healthy 30 MB/s store, but it never goes 60s
        // without yielding a 256 KiB chunk.
        let next = match idle_timeout {
            Some(t) => tokio::time::timeout(t, stream.message())
                .await
                .map_err(|_| {
                    NarCollectError::Stream(tonic::Status::deadline_exceeded(format!(
                        "GetPath stream idle for {t:?} (no chunk received)"
                    )))
                })??,
            None => stream.message().await?,
        };
        let Some(msg) = next else { break };
        match msg.msg {
            Some(get_path_response::Msg::Info(i)) => info = Some(i),
            Some(get_path_response::Msg::NarChunk(chunk)) => {
                let new_len = written.saturating_add(chunk.len() as u64);
                if new_len > max_size {
                    return Err(NarCollectError::SizeExceeded {
                        got: new_len,
                        limit: max_size,
                    });
                }
                w.write_all(&chunk).await?;
                written = new_len;
            }
            None => {} // empty oneof — ignore
        }
    }
    Ok((info, written))
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
///
/// `extra_metadata`: caller-supplied metadata attached after trace-context.
/// The gateway passes `x-rio-tenant-token` here so store-side tenant-scoped
/// operations (substitution, narinfo visibility) see the session identity.
pub async fn query_path_info_opt(
    client: &mut StoreServiceClient<Channel>,
    store_path: &str,
    timeout: Duration,
    extra_metadata: &[(&'static str, &str)],
) -> Result<Option<ValidatedPathInfo>, tonic::Status> {
    let mut req = tonic::Request::new(QueryPathInfoRequest {
        store_path: store_path.to_string(),
    });
    crate::interceptor::inject_current(req.metadata_mut());
    inject_metadata(req.metadata_mut(), extra_metadata)?;
    match with_timeout_status("QueryPathInfo", timeout, client.query_path_info(req)).await {
        Ok(resp) => {
            let validated = ValidatedPathInfo::try_from(resp.into_inner()).map_err(|e| {
                tonic::Status::internal(format!("store returned malformed PathInfo: {e}"))
            })?;
            Ok(Some(validated))
        }
        Err(status) if status.code() == tonic::Code::NotFound => Ok(None),
        Err(status) => Err(status),
    }
}

/// BatchQueryPathInfo with timeout. I-110: builder closure-BFS uses
/// this once per layer instead of N × [`query_path_info_opt`].
///
/// Returns `(path, Option<ValidatedPathInfo>)` per requested path, in
/// request order. `None` = not in store. A malformed entry (bad
/// store_path / nar_hash) fails the whole call.
///
/// `Err(Unimplemented)` means the store doesn't support the batch RPC
/// (older binary) — caller falls back to per-path [`query_path_info_opt`].
pub async fn batch_query_path_info(
    client: &mut StoreServiceClient<Channel>,
    store_paths: Vec<String>,
    timeout: Duration,
    extra_metadata: &[(&'static str, &str)],
) -> Result<Vec<(String, Option<ValidatedPathInfo>)>, tonic::Status> {
    let mut req = tonic::Request::new(BatchQueryPathInfoRequest { store_paths });
    crate::interceptor::inject_current(req.metadata_mut());
    inject_metadata(req.metadata_mut(), extra_metadata)?;
    let resp = with_timeout_status(
        "BatchQueryPathInfo",
        timeout,
        client.batch_query_path_info(req),
    )
    .await?;
    resp.into_inner()
        .entries
        .into_iter()
        .map(|e| {
            let info = e
                .info
                .map(ValidatedPathInfo::try_from)
                .transpose()
                .map_err(|err| {
                    tonic::Status::internal(format!(
                        "store returned malformed PathInfo for {}: {err}",
                        e.store_path
                    ))
                })?;
            Ok((e.store_path, info))
        })
        .collect()
}

/// BatchGetManifest with timeout. I-110c: builder calls this once
/// before the FUSE-warm stat loop and primes its hint cache so each
/// subsequent `GetPath` carries `manifest_hint` and the store skips PG.
///
/// Returns `(path, Option<ManifestHint>)` per requested path, request
/// order. `None` = no complete manifest. `Err(Unimplemented)` means
/// the store predates I-110c — caller skips the prefetch (per-path
/// `GetPath` falls back to PG as before).
pub async fn batch_get_manifest(
    client: &mut StoreServiceClient<Channel>,
    store_paths: Vec<String>,
    timeout: Duration,
) -> Result<Vec<(String, Option<crate::types::ManifestHint>)>, tonic::Status> {
    let mut req = tonic::Request::new(BatchGetManifestRequest { store_paths });
    crate::interceptor::inject_current(req.metadata_mut());
    let resp =
        with_timeout_status("BatchGetManifest", timeout, client.batch_get_manifest(req)).await?;
    Ok(resp
        .into_inner()
        .entries
        .into_iter()
        .map(|e| (e.store_path, e.hint))
        .collect())
}

/// GetPath with timeout, full NAR collection, and NotFound handling.
///
/// Combines the `GetPath → collect_nar_stream → NotFound-branch` pattern.
/// `timeout` is a per-chunk IDLE bound (I-211): it gates the initial RPC
/// and then each `stream.message()` independently — NOT a wall-clock cap
/// on the whole drain. Returns `None` if the path doesn't exist or the
/// stream contains no PathInfo.
///
/// `extra_metadata`: see [`query_path_info_opt`]. DO NOT inline this
/// function's await structure at callsites — under
/// `#[tokio::test(start_paused = true)]`, the exact suspend-point
/// layout determines whether tokio's auto-advance fires the timeout
/// before in-process gRPC I/O completes. See
/// `rio-gateway/tests/wire_opcodes/build.rs` reconnect tests comment.
///
/// `manifest_hint` (I-110c): pre-fetched (PathInfo, manifest) so the
/// store skips its two PG lookups. `None` → store queries PG as before.
pub async fn get_path_nar(
    client: &mut StoreServiceClient<Channel>,
    store_path: &str,
    timeout: Duration,
    max_nar_size: u64,
    manifest_hint: Option<crate::types::ManifestHint>,
    extra_metadata: &[(&'static str, &str)],
) -> Result<Option<(ValidatedPathInfo, Vec<u8>)>, NarCollectError> {
    let mut req = tonic::Request::new(GetPathRequest {
        store_path: store_path.to_string(),
        manifest_hint,
    });
    crate::interceptor::inject_current(req.metadata_mut());
    inject_metadata(req.metadata_mut(), extra_metadata).map_err(NarCollectError::Stream)?;
    // I-211 / merged_014: `timeout` is an IDLE bound, not a wall-clock
    // bound on the whole fetch. It applies twice: (1) the initial RPC
    // (covers connect + server-side first-response — a hung store still
    // trips at `timeout`); (2) each subsequent `stream.message()` inside
    // the collector. A 4 GiB NAR completes as long as the store yields a
    // chunk every <`timeout`. The previous whole-call wrap meant
    // `nar_from_path` on a 4 GiB NAR at <13.7 MB/s hit DeadlineExceeded
    // at 300s. Mirrors [`get_path_nar_to_file`].
    let mut stream = match with_timeout_status("GetPath", timeout, client.get_path(req)).await {
        Ok(resp) => resp.into_inner(),
        Err(status) if status.code() == tonic::Code::NotFound => return Ok(None),
        Err(status) => return Err(NarCollectError::Stream(status)),
    };
    let (info, nar) = collect_nar_stream(&mut stream, max_nar_size, Some(timeout)).await?;
    match info {
        Some(raw) => {
            let validated = ValidatedPathInfo::try_from(raw)?;
            Ok(Some((validated, nar)))
        }
        None => Ok(None),
    }
}

/// GetPath with timeout, NAR spooled to an `AsyncWrite` sink, and NotFound
/// handling.
///
/// Streaming sibling of [`get_path_nar`]: same retry/NotFound/SizeExceeded
/// semantics, but NAR bytes go to `spool` instead of a returned `Vec<u8>`.
/// Returns just the `ValidatedPathInfo` (or `None` if absent). Peak heap
/// is one chunk (256 KiB).
///
/// I-180: the builder's FUSE fetch passes a `tokio::fs::File` in the cache
/// dir. On transient error, the caller truncates+seeks the spool and
/// retries; on success, it `restore_path_streaming`s from the spool.
pub async fn get_path_nar_to_file(
    client: &mut StoreServiceClient<Channel>,
    store_path: &str,
    timeout: Duration,
    max_nar_size: u64,
    manifest_hint: Option<crate::types::ManifestHint>,
    extra_metadata: &[(&'static str, &str)],
    spool: &mut (impl AsyncWrite + Unpin),
) -> Result<Option<ValidatedPathInfo>, NarCollectError> {
    let mut req = tonic::Request::new(GetPathRequest {
        store_path: store_path.to_string(),
        manifest_hint,
    });
    crate::interceptor::inject_current(req.metadata_mut());
    inject_metadata(req.metadata_mut(), extra_metadata).map_err(NarCollectError::Stream)?;
    // I-211: `timeout` is now an IDLE bound, not a wall-clock bound on the
    // whole fetch. It applies twice: (1) the initial RPC (covers connect +
    // server-side first-response — a hung store still trips at 60s); (2)
    // each subsequent `stream.message()` inside the collector. A 2.9 GB
    // NAR completes as long as the store yields a chunk every <`timeout`.
    let mut stream = match with_timeout_status("GetPath", timeout, client.get_path(req)).await {
        Ok(resp) => resp.into_inner(),
        Err(status) if status.code() == tonic::Code::NotFound => return Ok(None),
        Err(status) => return Err(NarCollectError::Stream(status)),
    };
    let (info, _written) =
        collect_nar_stream_to_writer(&mut stream, max_nar_size, Some(timeout), spool).await?;
    match info {
        Some(raw) => {
            let validated = ValidatedPathInfo::try_from(raw)?;
            Ok(Some(validated))
        }
        None => Ok(None),
    }
}
