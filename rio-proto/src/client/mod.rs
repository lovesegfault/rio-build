//! gRPC client connection and streaming helpers.
//!
//! - Connection: `connect_*` build `"http(s)://{addr}"` from a `host:port`
//!   string, apply the process-global TLS config (see
//!   [`rio_common::grpc::init_client_tls`]), connect, and apply
//!   [`max_message_size`].
//! - NAR streaming: [`collect_nar_stream`] drains `GetPath` responses;
//!   [`chunk_nar_for_put`] builds a lazy `PutPath` request stream.

pub mod balance;
pub use balance::BalancedChannel;

pub mod retry;
pub use retry::{RetryError, connect_with_retry};

use std::sync::Arc;
use std::time::Duration;

use rio_common::grpc::{client_tls, max_message_size};
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

/// Connect to a gRPC endpoint at `host:port` and return a raw [`Channel`].
///
/// Scheme and TLS wiring depend on the process-global
/// [`rio_common::grpc::client_tls`]: set → `https://` + `.tls_config()`;
/// unset → `http://` (plaintext). The global is initialized by
/// [`rio_common::grpc::init_client_tls`] (via `bootstrap()` in each
/// binary's main); if never called (or called with `None`), plaintext.
///
/// 10s connect timeout: tonic's default is UNBOUNDED. A stale address
/// (e.g., scheduler pod killed → replacement has new IP, but DNS TTL /
/// caller's cached addr hasn't updated) hangs forever on TCP SYN.
/// Observed in lifecycle test: controller's cleanup() never logged
/// "starting drain" — stuck in connect_admin after the scheduler
/// leader was killed mid-run. 10s is enough for a real connect
/// (even cross-AZ) and bounds the failure mode.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Initial h2 per-stream flow-control window (1 MiB). h2's default is
/// 65 535 bytes — at 2-3 ms cross-AZ RTT that's a ~20-30 MB/s ceiling
/// (each 256 KiB NAR chunk needs ~4 WINDOW_UPDATE round-trips before
/// the next can flow). 1 MiB lifts the floor; `http2_adaptive_window`
/// (BDP probing) auto-tunes upward from there. I-180: this was the
/// 30 MB/s wall on builder NAR fetch, not S3 prefetch or proto decode.
const H2_INITIAL_STREAM_WINDOW: u32 = 1024 * 1024;

/// Initial h2 connection-level window (16 MiB). Shared across all
/// streams on the connection; sized so a handful of concurrent
/// GB-scale `GetPath` streams don't head-of-line block each other.
const H2_INITIAL_CONN_WINDOW: u32 = 16 * 1024 * 1024;

/// Apply h2 keepalive + flow-control window tuning.
///
/// **Keepalive** (30s PING interval, 10s PONG timeout, while-idle):
/// detects half-open connections in ~40s (next PING + PONG timeout)
/// instead of falling through to kernel TCP keepalive --- Linux default
/// `tcp_keepalive_time` is 7200s (2h). Without this, an ungracefully
/// dead peer (SIGKILL, netsplit, OOM-kill --- anything that skips FIN)
/// leaves the connection silently stuck until the kernel notices.
///
/// `while_idle`: send PINGs even with no in-flight requests. Without
/// it, an idle channel never probes --- the peer can vanish and the
/// next RPC blocks until kernel TCP timeout. With it, the h2 layer
/// fires `GoAway` proactively, all streams error, callers reconnect.
///
/// **Flow-control windows** (1 MiB stream / 16 MiB conn / adaptive):
/// see [`H2_INITIAL_STREAM_WINDOW`]. Mirrored server-side in each
/// component's `Server::builder()` — h2 windows are per-direction.
///
/// Factored out after I-048c: the balanced channel diverged from
/// `connect_store_lazy` and went 7 minutes dark on scheduler SIGKILL.
pub(crate) fn with_h2_keepalive(ep: tonic::transport::Endpoint) -> tonic::transport::Endpoint {
    with_h2_throughput(ep)
        .http2_keep_alive_interval(Duration::from_secs(30))
        .keep_alive_timeout(Duration::from_secs(10))
        .keep_alive_while_idle(true)
}

/// Apply h2 flow-control window tuning only (no keepalive). See
/// [`H2_INITIAL_STREAM_WINDOW`].
///
/// Separate from [`with_h2_keepalive`] so the eager [`connect_channel`]
/// path can take the throughput fix without keepalive: under heavy
/// parallel-process load (workspace nextest), `keep_alive_while_idle`
/// PING/PONG on a freshly-eager-connected channel raced and produced
/// spurious GoAway → EIO in fetch tests. The lazy/balanced paths
/// (long-lived channels, the production builder path) take both via
/// [`with_h2_keepalive`].
// r[impl proto.h2.adaptive-window]
pub(crate) fn with_h2_throughput(ep: tonic::transport::Endpoint) -> tonic::transport::Endpoint {
    ep.http2_adaptive_window(true)
        .initial_stream_window_size(Some(H2_INITIAL_STREAM_WINDOW))
        .initial_connection_window_size(Some(H2_INITIAL_CONN_WINDOW))
}

pub async fn connect_channel(addr: &str) -> anyhow::Result<Channel> {
    // `client_tls()` collapses both "OnceLock not initialized" and
    // "initialized with None" to plaintext. Tests that never call
    // init_client_tls stay plaintext.
    let (scheme, tls) = match client_tls() {
        Some(tls) => ("https", Some(tls)),
        None => ("http", None),
    };
    let mut ep = with_h2_throughput(
        Channel::from_shared(format!("{scheme}://{addr}"))?.connect_timeout(CONNECT_TIMEOUT),
    );
    if let Some(tls) = tls {
        ep = ep.tls_config(tls)?;
    }
    ep.connect().await.map_err(Into::into)
}

/// Connect to the store service.
pub async fn connect_store(addr: &str) -> anyhow::Result<StoreServiceClient<Channel>> {
    let ch = connect_channel(addr).await?;
    Ok(StoreServiceClient::new(ch)
        .max_decoding_message_size(max_message_size())
        .max_encoding_message_size(max_message_size()))
}

/// Lazy-connect store client with HTTP/2 keepalive.
///
/// Unlike [`connect_store`], this does NOT establish a TCP connection at
/// call time — the channel connects on first RPC and RE-RESOLVES DNS on
/// each reconnect. This is the difference between "connection pinned to
/// the pod IP that DNS resolved to at startup" (eager) and "connection
/// follows the Service's current endpoint" (lazy).
///
/// The eager variant breaks on store rollout: old pod terminates → TCP
/// connection drops → eager Channel's cached IP is stale → RPCs fail
/// with `Unavailable` forever. Lazy re-resolves and reconnects on the
/// next RPC.
///
/// Keepalive (30s interval, 10s timeout, while-idle) detects half-open
/// connections within ~40s instead of waiting for kernel TCP timeout
/// (minutes). Without while-idle, an idle channel wouldn't notice the
/// peer vanished until the next RPC — keepalive surfaces it proactively.
///
/// Returns `Err` only on malformed `addr` (scheme parse) or bad TLS
/// config — never on connection failure (that's deferred to first RPC).
/// So callers can drop their retry loop: the channel ALWAYS constructs.
pub fn connect_store_lazy(addr: &str) -> anyhow::Result<StoreServiceClient<Channel>> {
    let (scheme, tls) = match client_tls() {
        Some(tls) => ("https", Some(tls)),
        None => ("http", None),
    };
    let mut ep = with_h2_keepalive(
        tonic::transport::Endpoint::from_shared(format!("{scheme}://{addr}"))?
            .connect_timeout(CONNECT_TIMEOUT),
    );
    if let Some(tls) = tls {
        ep = ep.tls_config(tls)?;
    }
    let ch = ep.connect_lazy();
    Ok(StoreServiceClient::new(ch)
        .max_decoding_message_size(max_message_size())
        .max_encoding_message_size(max_message_size()))
}

/// Connect to the scheduler service (gateway-facing).
pub async fn connect_scheduler(
    addr: &str,
) -> anyhow::Result<crate::SchedulerServiceClient<Channel>> {
    let ch = connect_channel(addr).await?;
    Ok(crate::SchedulerServiceClient::new(ch)
        .max_decoding_message_size(max_message_size())
        .max_encoding_message_size(max_message_size()))
}

/// Connect to the executor service (builder/fetcher-facing scheduler RPCs).
pub async fn connect_executor(addr: &str) -> anyhow::Result<crate::ExecutorServiceClient<Channel>> {
    let ch = connect_channel(addr).await?;
    Ok(crate::ExecutorServiceClient::new(ch)
        .max_decoding_message_size(max_message_size())
        .max_encoding_message_size(max_message_size()))
}

/// Connect to the admin service (controller + executor preStop).
///
/// Same address as `connect_executor` — AdminService is hosted on the
/// scheduler's gRPC port alongside SchedulerService/ExecutorService.
/// The executor's SIGTERM handler uses this for `DrainExecutor` (step 1
/// of preStop); the controller uses it for `ClusterStatus` autoscaling.
pub async fn connect_admin(addr: &str) -> anyhow::Result<crate::AdminServiceClient<Channel>> {
    let ch = connect_channel(addr).await?;
    Ok(crate::AdminServiceClient::new(ch)
        .max_decoding_message_size(max_message_size())
        .max_encoding_message_size(max_message_size()))
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
        .max_decoding_message_size(max_message_size())
        .max_encoding_message_size(max_message_size()))
}

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
    /// own timeout firing — the store hung past `fetch_timeout`.
    /// Retrying with the same timeout won't help, and the next retry
    /// would compound the wait on a FUSE-thread caller.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            NarCollectError::Stream(s)
                // I-122: ResourceExhausted = store's PG pool full. With
                // ~400 ephemeral builders synchronously transitioning
                // (warm→build, build→collect), output-path GetPath
                // bursts can briefly saturate even 8×200=1600 conns.
                // The pool drains in <1s — retry is the right answer.
                //
                // I-189: Aborted = store's retryable PG conflict
                // (Serialization, Deadlock — see `rio-store::metadata`).
                // The store says "retry" via Aborted; without it here
                // the builder's
                // no-manifest-hint fallback path EIOs immediately on PG
                // contention instead of backing off.
                if matches!(
                    s.code(),
                    tonic::Code::Unavailable
                        | tonic::Code::Unknown
                        | tonic::Code::ResourceExhausted
                        | tonic::Code::Aborted
                )
        )
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
pub async fn collect_nar_stream(
    stream: &mut Streaming<GetPathResponse>,
    max_size: u64,
) -> Result<(Option<PathInfo>, Vec<u8>), NarCollectError> {
    let mut info = None;
    let mut nar = Vec::new();

    while let Some(msg) = stream.message().await? {
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
/// small-payload callers (.drv files, gateway `wopNarFromPath`).
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
        // r[impl builder.fuse.fetch-progress-timeout]
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
    for (k, v) in extra_metadata {
        req.metadata_mut().insert(
            *k,
            v.parse()
                .map_err(|e| tonic::Status::internal(format!("metadata {k}: {e}")))?,
        );
    }
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
    for (k, v) in extra_metadata {
        req.metadata_mut().insert(
            *k,
            v.parse()
                .map_err(|e| tonic::Status::internal(format!("metadata {k}: {e}")))?,
        );
    }
    let resp = tokio::time::timeout(timeout, client.batch_query_path_info(req))
        .await
        .map_err(|_| {
            tonic::Status::deadline_exceeded(format!(
                "BatchQueryPathInfo timed out after {timeout:?}"
            ))
        })??;
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
    let resp = tokio::time::timeout(timeout, client.batch_get_manifest(req))
        .await
        .map_err(|_| {
            tonic::Status::deadline_exceeded(format!(
                "BatchGetManifest timed out after {timeout:?}"
            ))
        })??;
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
/// The whole operation (initial call + stream drain) is bounded by `timeout`.
/// Returns `None` if the path doesn't exist or the stream contains no PathInfo.
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
    for (k, v) in extra_metadata {
        req.metadata_mut().insert(
            *k,
            v.parse().map_err(|e| {
                NarCollectError::Stream(tonic::Status::internal(format!("metadata {k}: {e}")))
            })?,
        );
    }
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
    for (k, v) in extra_metadata {
        req.metadata_mut().insert(
            *k,
            v.parse().map_err(|e| {
                NarCollectError::Stream(tonic::Status::internal(format!("metadata {k}: {e}")))
            })?,
        );
    }
    // I-211: `timeout` is now an IDLE bound, not a wall-clock bound on the
    // whole fetch. It applies twice: (1) the initial RPC (covers connect +
    // server-side first-response — a hung store still trips at 60s); (2)
    // each subsequent `stream.message()` inside the collector. A 2.9 GB
    // NAR completes as long as the store yields a chunk every <`timeout`.
    let mut stream = match tokio::time::timeout(timeout, client.get_path(req)).await {
        Ok(Ok(resp)) => resp.into_inner(),
        Ok(Err(status)) if status.code() == tonic::Code::NotFound => return Ok(None),
        Ok(Err(status)) => return Err(NarCollectError::Stream(status)),
        Err(_) => {
            return Err(NarCollectError::Stream(tonic::Status::deadline_exceeded(
                format!("GetPath({store_path}) initial response timed out after {timeout:?}"),
            )));
        }
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

#[cfg(test)]
mod retry_tests {
    use super::*;
    use std::time::Duration;

    /// Regression guard: h2 flow-control windows must stay above the
    /// 64 KiB default. A revert to defaults reintroduces the ~30 MB/s
    /// cross-AZ ceiling on `GetPath` (I-180) — silently, since nothing
    /// fails, throughput just drops. There's no public accessor on
    /// `Endpoint` to inspect the configured window, so this asserts the
    /// constants directly; `with_h2_keepalive` is the single application
    /// site (covered by `connect_closed_port_fails_fast_then_succeeds`
    /// going through it via `connect_channel`).
    // r[verify proto.h2.adaptive-window]
    #[test]
    #[allow(
        clippy::assertions_on_constants,
        reason = "the constants ARE the contract — this guards against a \
                  silent revert to h2 defaults"
    )]
    fn h2_window_floor_not_regressed() {
        const H2_DEFAULT: u32 = 65_535;
        assert!(
            H2_INITIAL_STREAM_WINDOW > H2_DEFAULT,
            "stream window {} must exceed h2 default {} (I-180 30 MB/s ceiling)",
            H2_INITIAL_STREAM_WINDOW,
            H2_DEFAULT
        );
        assert!(
            H2_INITIAL_CONN_WINDOW >= H2_INITIAL_STREAM_WINDOW,
            "connection window must be at least the stream window"
        );
    }

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
