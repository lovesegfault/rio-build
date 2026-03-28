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

/// Query PathInfo from store via gRPC. Returns None if NOT_FOUND.
pub(crate) async fn grpc_query_path_info(
    store_client: &mut StoreServiceClient<Channel>,
    jwt_token: Option<&str>,
    store_path: &str,
) -> anyhow::Result<Option<ValidatedPathInfo>> {
    let req = with_jwt(
        types::QueryPathInfoRequest {
            store_path: store_path.to_string(),
        },
        jwt_token,
    )?;
    match tokio::time::timeout(DEFAULT_GRPC_TIMEOUT, store_client.query_path_info(req)).await {
        Ok(Ok(resp)) => {
            let validated = ValidatedPathInfo::try_from(resp.into_inner()).map_err(|e| {
                GatewayError::Store(format!("store returned malformed PathInfo: {e}"))
            })?;
            Ok(Some(validated))
        }
        Ok(Err(status)) if status.code() == tonic::Code::NotFound => Ok(None),
        Ok(Err(status)) => {
            Err(GatewayError::Store(format!("QueryPathInfo failed: {status}")).into())
        }
        Err(_) => Err(GatewayError::Store(format!(
            "QueryPathInfo timed out after {DEFAULT_GRPC_TIMEOUT:?}"
        ))
        .into()),
    }
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

/// Upload a path to the store via gRPC PutPath (metadata + NAR chunks).
pub(super) async fn grpc_put_path(
    store_client: &mut StoreServiceClient<Channel>,
    jwt_token: Option<&str>,
    info: ValidatedPathInfo,
    nar_data: Vec<u8>,
) -> anyhow::Result<bool> {
    let nar: std::sync::Arc<[u8]> = nar_data.into();
    let stream = rio_proto::client::chunk_nar_for_put(info, nar);
    let resp = rio_common::grpc::with_timeout(
        "PutPath",
        GRPC_STREAM_TIMEOUT,
        store_client.put_path(with_jwt(stream, jwt_token)?),
    )
    .await?;

    Ok(resp.into_inner().created)
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
pub(super) async fn grpc_get_path(
    store_client: &mut StoreServiceClient<Channel>,
    jwt_token: Option<&str>,
    store_path: &str,
) -> anyhow::Result<Option<(ValidatedPathInfo, Vec<u8>)>> {
    let req = with_jwt(
        types::GetPathRequest {
            store_path: store_path.to_string(),
        },
        jwt_token,
    )?;
    let fut = async {
        let mut stream = match store_client.get_path(req).await {
            Ok(resp) => resp.into_inner(),
            Err(status) if status.code() == tonic::Code::NotFound => return Ok(None),
            Err(status) => {
                return Err(
                    GatewayError::Store(format!("GetPath for {store_path}: {status}")).into(),
                );
            }
        };
        let (info, nar) = rio_proto::client::collect_nar_stream(&mut stream, MAX_NAR_SIZE)
            .await
            .map_err(|e| GatewayError::Store(format!("GetPath for {store_path}: {e}")))?;
        match info {
            Some(raw) => {
                let validated = ValidatedPathInfo::try_from(raw)
                    .map_err(|e| GatewayError::Store(format!("GetPath for {store_path}: {e}")))?;
                Ok(Some((validated, nar)))
            }
            None => Ok(None),
        }
    };
    match tokio::time::timeout(GRPC_STREAM_TIMEOUT, fut).await {
        Ok(r) => r,
        Err(_) => Err(GatewayError::Store(format!(
            "GetPath for {store_path} timed out after {GRPC_STREAM_TIMEOUT:?}"
        ))
        .into()),
    }
}
