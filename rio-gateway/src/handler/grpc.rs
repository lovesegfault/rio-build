//! Store-query helpers using gRPC.
//!
//! All helpers take `jwt_token: Option<&str>` and attach it as
//! `x-rio-tenant-token` via [`with_jwt`]. Without the JWT, store-side
//! tenant-scoped operations (substitution, narinfo visibility gate)
//! short-circuit — see `r[gw.jwt.issue]`.

use super::*;
use rio_proto::client::NAR_CHUNK_SIZE;
use rio_proto::validated::ValidatedPathInfo;
use tokio::io::AsyncReadExt;

/// Build the `x-rio-tenant-token` metadata pair for rio-proto helpers.
///
/// Returns a borrowed-slice-compatible array so callers can pass
/// `&jwt_metadata(token)` directly. Empty slice when `jwt_token` is
/// `None` (dual-mode fallback — store's interceptor treats absent
/// header as pass-through per `r[gw.jwt.dual-mode]`).
fn jwt_metadata(jwt_token: Option<&str>) -> Vec<(&'static str, &str)> {
    match jwt_token {
        Some(t) => vec![(rio_common::jwt_interceptor::TENANT_TOKEN_HEADER, t)],
        None => vec![],
    }
}

/// Query PathInfo from store via gRPC. Returns None if NOT_FOUND.
pub(crate) async fn grpc_query_path_info(
    store_client: &mut StoreServiceClient<Channel>,
    jwt_token: Option<&str>,
    store_path: &str,
) -> anyhow::Result<Option<ValidatedPathInfo>> {
    rio_proto::client::query_path_info_opt(
        store_client,
        store_path,
        DEFAULT_GRPC_TIMEOUT,
        &jwt_metadata(jwt_token),
    )
    .await
    .map_err(|e| GatewayError::Store(format!("QueryPathInfo failed: {e}")).into())
}

/// Check validity via QueryPathInfo -- returns true if path exists.
pub(super) async fn grpc_is_valid_path(
    store_client: &mut StoreServiceClient<Channel>,
    jwt_token: Option<&str>,
    path: &StorePath,
) -> anyhow::Result<bool> {
    Ok(grpc_query_path_info(store_client, jwt_token, path.as_str())
        .await?
        .is_some())
}

/// Max attempts for `Code::Aborted` retry in [`grpc_put_path`]. The
/// store returns Aborted when another upload holds the placeholder for
/// this path (I-068) or on PG serialization/deadlock conflicts — both
/// clear in one round-trip (.drv NARs are KB). GC no longer blocks
/// PutPath at all (I-192).
///
/// 50 ms base, ×2, full jitter, 2 s cap. 8 attempts → ≤~6 s budget —
/// generous for the remaining (fast-clearing) cases; kept as a safety
/// margin rather than tightened. Shared with rio-builder's PutPath
/// retry (`upload.rs`): both hit the same store-side placeholder
/// contention, so they use the same curve+budget.
const PUT_PATH_ABORTED_MAX_ATTEMPTS: u32 = 8;
const PUT_PATH_BACKOFF: rio_common::backoff::Backoff = rio_common::backoff::Backoff {
    base: std::time::Duration::from_millis(50),
    mult: 2.0,
    cap: std::time::Duration::from_secs(2),
    jitter: rio_common::backoff::Jitter::Full,
};

/// Upload a path to the store via gRPC PutPath (metadata + NAR chunks).
///
/// Retries on `Code::Aborted` (concurrent same-path upload — store's
/// `put_path.rs` returns this when another writer holds the placeholder
/// row). I-068: with the I-052 32-way pipeline × N clients × shared
/// closure, collisions are guaranteed; before this retry the gateway
/// surfaced Aborted as a hard wopAddMultipleToStore failure and the
/// client died mid-push.
///
/// `nar_data` is held as `Arc<[u8]>` so each retry rebuilds the request
/// stream without copying the buffer. `info` is `Clone` (cheap — strings
/// and Vecs already heap-allocated).
pub(super) async fn grpc_put_path(
    store_client: &mut StoreServiceClient<Channel>,
    jwt_token: Option<&str>,
    info: ValidatedPathInfo,
    nar_data: Vec<u8>,
) -> anyhow::Result<bool> {
    let nar: std::sync::Arc<[u8]> = nar_data.into();
    let mut attempt = 0u32;
    loop {
        let stream =
            rio_proto::client::chunk_nar_for_put(info.clone(), std::sync::Arc::clone(&nar));
        let result = rio_common::grpc::with_timeout_status(
            "PutPath",
            GRPC_STREAM_TIMEOUT,
            store_client.put_path(with_jwt(stream, jwt_token)?),
        )
        .await;
        match result {
            Ok(resp) => return Ok(resp.into_inner().created),
            Err(status) if status.code() == tonic::Code::Aborted => {
                attempt += 1;
                // I-168: dashboard-visible retry budget (was log-only).
                metrics::counter!(
                    "rio_gateway_putpath_aborted_retries_total",
                    "attempt" => attempt.to_string(),
                )
                .increment(1);
                if attempt >= PUT_PATH_ABORTED_MAX_ATTEMPTS {
                    tracing::warn!(
                        store_path = %info.store_path,
                        attempts = attempt,
                        "PutPath: store still Aborted after retry budget; surfacing"
                    );
                    return Err(status.into());
                }
                // FULL jitter (`U(0, capᵃ]`): N clients retrying the
                // SAME path don't re-collide in lockstep, and the
                // I-068 placeholder case stays fast (first retry
                // ≤50 ms) while the I-168 mark-busy case gets a
                // multi-second window. `attempt-1` so attempt=1 uses
                // mult⁰ = base.
                let delay = PUT_PATH_BACKOFF.duration(attempt - 1);
                tracing::debug!(
                    store_path = %info.store_path,
                    attempt,
                    backoff = ?delay,
                    msg = %status.message(),
                    "PutPath: store Aborted; retrying with exponential backoff"
                );
                tokio::time::sleep(delay).await;
            }
            Err(status) => return Err(status.into()),
        }
    }
}

/// Upload a path to the store, streaming NAR bytes from a reader.
///
/// Reads exactly `nar_size` bytes from `nar_reader` in `NAR_CHUNK_SIZE`
/// chunks and forwards each as a NarChunk. Forwards the client-declared
/// hash in the trailer — store re-hashes and validates (same security
/// property as [`grpc_put_path`]; the gateway is a dumb pipe here).
///
/// `nar_reader` must yield exactly `nar_size` bytes; short read = error.
/// Caller is responsible for the `nar_size <= MAX_NAR_SIZE` check.
///
/// NOT retried on `Aborted` (unlike [`grpc_put_path`]): the reader is
/// consumed and the bytes are forwarded as they arrive, so there's
/// nothing to replay. In practice this path only fires for oversize
/// (>DRV_NAR_BUFFER_LIMIT) entries — the I-068 collision case is .drv
/// files, which always go through the buffered path.
pub(super) async fn grpc_put_path_streaming<R: AsyncRead + Unpin>(
    store_client: &StoreServiceClient<Channel>,
    jwt_token: Option<&str>,
    info: ValidatedPathInfo,
    nar_reader: &mut R,
    nar_size: u64,
    client_nar_hash: Vec<u8>,
) -> anyhow::Result<bool> {
    // ~1 MiB in flight at 256 KiB chunks.
    const CHANNEL_BUF: usize = 4;

    let (tx, rx) = tokio::sync::mpsc::channel::<types::PutPathRequest>(CHANNEL_BUF);

    // Metadata first. Zero nar_hash/nar_size → trailer mode.
    let mut raw: types::PathInfo = info.into();
    raw.nar_hash = Vec::new();
    raw.nar_size = 0;
    tx.send(types::PutPathRequest {
        msg: Some(types::put_path_request::Msg::Metadata(
            types::PutPathMetadata { info: Some(raw) },
        )),
    })
    .await
    .map_err(|_| GatewayError::GrpcStream("PutPath channel closed before metadata".into()))?;

    // Drive the gRPC call. Clone: tonic Channel is Arc-backed.
    // JWT wrapped BEFORE the spawn — jwt_token's lifetime doesn't
    // extend into the 'static task.
    let mut client = store_client.clone();
    let outbound = tokio_stream::wrappers::ReceiverStream::new(rx);
    let req = with_jwt(outbound, jwt_token)?;
    let rpc: tokio::task::JoinHandle<anyhow::Result<tonic::Response<types::PutPathResponse>>> =
        tokio::spawn(async move {
            rio_common::grpc::with_timeout("PutPath", GRPC_STREAM_TIMEOUT, client.put_path(req))
                .await
        });

    // Read exactly nar_size bytes in NAR_CHUNK_SIZE chunks, forward each.
    // Backpressure: tx.send blocks when rpc isn't pulling. On any error
    // (short read, channel closed) we still drop tx and await rpc so the
    // spawned task completes before we return.
    let pump_result: anyhow::Result<()> = async {
        let mut remaining = nar_size;
        let mut chunk = vec![0u8; NAR_CHUNK_SIZE];
        while remaining > 0 {
            let n = (remaining.min(NAR_CHUNK_SIZE as u64)) as usize;
            nar_reader
                .read_exact(&mut chunk[..n])
                .await
                .map_err(|e| GatewayError::NarRead {
                    context: format!("at {} of {nar_size}", nar_size - remaining),
                    source: e,
                })?;
            tx.send(types::PutPathRequest {
                msg: Some(types::put_path_request::Msg::NarChunk(chunk[..n].to_vec())),
            })
            .await
            .map_err(|_| {
                GatewayError::GrpcStream("PutPath channel closed mid-stream (store error?)".into())
            })?;
            remaining -= n as u64;
        }

        // Trailer: client-declared hash. Store validates independently.
        tx.send(types::PutPathRequest {
            msg: Some(types::put_path_request::Msg::Trailer(
                types::PutPathTrailer {
                    nar_hash: client_nar_hash,
                    nar_size,
                },
            )),
        })
        .await
        .map_err(|_| GatewayError::GrpcStream("PutPath channel closed before trailer".into()))?;
        Ok(())
    }
    .await;

    drop(tx); // close channel → ReceiverStream yields None → rpc completes

    let rpc_result = rpc
        .await
        .map_err(|e| GatewayError::GrpcStream(format!("PutPath task panicked: {e}")))?;

    // Error priority: pump error > rpc error (pump error is the root cause;
    // a short read causes the rpc to see a truncated stream, but the useful
    // message is "NAR read at X of Y", not "store rejected incomplete stream").
    pump_result?;
    let resp = rpc_result?;
    Ok(resp.into_inner().created)
}

/// Fetch NAR data from store via gRPC GetPath.
/// Returns (PathInfo, NAR bytes) or None if not found.
///
/// Delegates to `rio_proto::client::get_path_nar` — DO NOT inline that
/// helper's await structure here. Under `#[tokio::test(start_paused =
/// true)]`, the exact suspend-point layout determines whether tokio's
/// auto-advance fires the GRPC_STREAM_TIMEOUT before in-process gRPC
/// I/O completes (observed in wire_opcodes::build reconnect tests when
/// P0465 initially inlined this; reverted to delegation).
pub(super) async fn grpc_get_path(
    store_client: &mut StoreServiceClient<Channel>,
    jwt_token: Option<&str>,
    store_path: &str,
) -> anyhow::Result<Option<(ValidatedPathInfo, Vec<u8>)>> {
    rio_proto::client::get_path_nar(
        store_client,
        store_path,
        GRPC_STREAM_TIMEOUT,
        MAX_NAR_SIZE,
        None,
        &jwt_metadata(jwt_token),
    )
    .await
    .map_err(|e| GatewayError::Store(format!("GetPath for {store_path}: {e}")).into())
}
