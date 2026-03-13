//! Store-query helpers using gRPC.

use super::*;
use anyhow::anyhow;
use rio_proto::client::NAR_CHUNK_SIZE;
use rio_proto::validated::ValidatedPathInfo;
use tokio::io::AsyncReadExt;

/// Query PathInfo from store via gRPC. Returns None if NOT_FOUND.
pub(super) async fn grpc_query_path_info(
    store_client: &mut StoreServiceClient<Channel>,
    store_path: &str,
) -> anyhow::Result<Option<ValidatedPathInfo>> {
    rio_proto::client::query_path_info_opt(store_client, store_path, DEFAULT_GRPC_TIMEOUT)
        .await
        .map_err(|e| anyhow::anyhow!("gRPC QueryPathInfo failed: {e}"))
}

/// Check validity via QueryPathInfo -- returns true if path exists.
pub(super) async fn grpc_is_valid_path(
    store_client: &mut StoreServiceClient<Channel>,
    path: &StorePath,
) -> anyhow::Result<bool> {
    Ok(grpc_query_path_info(store_client, path.as_str())
        .await?
        .is_some())
}

/// Upload a path to the store via gRPC PutPath (metadata + NAR chunks).
pub(super) async fn grpc_put_path(
    store_client: &mut StoreServiceClient<Channel>,
    info: ValidatedPathInfo,
    nar_data: Vec<u8>,
) -> anyhow::Result<bool> {
    let nar: std::sync::Arc<[u8]> = nar_data.into();
    let stream = rio_proto::client::chunk_nar_for_put(info, nar);
    let resp = rio_common::grpc::with_timeout(
        "PutPath",
        GRPC_STREAM_TIMEOUT,
        store_client.put_path(stream),
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
    .map_err(|_| anyhow!("PutPath channel closed before metadata"))?;

    // Drive the gRPC call. Clone: tonic Channel is Arc-backed.
    let mut client = store_client.clone();
    let rpc: tokio::task::JoinHandle<anyhow::Result<tonic::Response<types::PutPathResponse>>> =
        tokio::spawn(async move {
            let outbound = tokio_stream::wrappers::ReceiverStream::new(rx);
            rio_common::grpc::with_timeout(
                "PutPath",
                GRPC_STREAM_TIMEOUT,
                client.put_path(outbound),
            )
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
                .map_err(|e| anyhow!("NAR read at {} of {nar_size}: {e}", nar_size - remaining))?;
            tx.send(types::PutPathRequest {
                msg: Some(types::put_path_request::Msg::NarChunk(chunk[..n].to_vec())),
            })
            .await
            .map_err(|_| anyhow!("PutPath channel closed mid-stream (store error?)"))?;
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
        .map_err(|_| anyhow!("PutPath channel closed before trailer"))?;
        Ok(())
    }
    .await;

    drop(tx); // close channel → ReceiverStream yields None → rpc completes

    let rpc_result = rpc
        .await
        .map_err(|e| anyhow!("PutPath task panicked: {e}"))?;

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
    store_path: &str,
) -> anyhow::Result<Option<(ValidatedPathInfo, Vec<u8>)>> {
    rio_proto::client::get_path_nar(store_client, store_path, GRPC_STREAM_TIMEOUT, MAX_NAR_SIZE)
        .await
        .map_err(|e| anyhow::anyhow!("gRPC GetPath for {store_path}: {e}"))
}
