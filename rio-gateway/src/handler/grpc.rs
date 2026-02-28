//! Store-query helpers using gRPC.

use super::*;

/// Query PathInfo from store via gRPC. Returns None if NOT_FOUND.
pub(super) async fn grpc_query_path_info(
    store_client: &mut StoreServiceClient<Channel>,
    store_path: &str,
) -> anyhow::Result<Option<types::PathInfo>> {
    let req = types::QueryPathInfoRequest {
        store_path: store_path.to_string(),
    };
    match rio_common::grpc::with_timeout_status(
        "QueryPathInfo",
        DEFAULT_GRPC_TIMEOUT,
        store_client.query_path_info(req),
    )
    .await
    {
        Ok(resp) => Ok(Some(resp.into_inner())),
        Err(status) if status.code() == tonic::Code::NotFound => Ok(None),
        Err(e) => Err(anyhow::anyhow!("gRPC QueryPathInfo failed: {e}")),
    }
}

/// Check validity via QueryPathInfo -- returns true if path exists.
pub(super) async fn grpc_is_valid_path(
    store_client: &mut StoreServiceClient<Channel>,
    path: &StorePath,
) -> anyhow::Result<bool> {
    Ok(grpc_query_path_info(store_client, &path.to_string())
        .await?
        .is_some())
}

/// Upload a path to the store via gRPC PutPath (metadata + NAR chunks).
pub(super) async fn grpc_put_path(
    store_client: &mut StoreServiceClient<Channel>,
    info: types::PathInfo,
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

/// Fetch NAR data from store via gRPC GetPath.
/// Returns (PathInfo, NAR bytes) or None if not found.
pub(super) async fn grpc_get_path(
    store_client: &mut StoreServiceClient<Channel>,
    store_path: &str,
) -> anyhow::Result<Option<(types::PathInfo, Vec<u8>)>> {
    let req = types::GetPathRequest {
        store_path: store_path.to_string(),
    };
    let mut stream = match rio_common::grpc::with_timeout_status(
        "GetPath",
        DEFAULT_GRPC_TIMEOUT,
        store_client.get_path(req),
    )
    .await
    {
        Ok(resp) => resp.into_inner(),
        Err(status) if status.code() == tonic::Code::NotFound => return Ok(None),
        Err(e) => return Err(anyhow::anyhow!("gRPC GetPath failed: {e}")),
    };

    let (info, nar_data) = rio_proto::client::collect_nar_stream(&mut stream, MAX_NAR_SIZE)
        .await
        .map_err(|e| anyhow::anyhow!("gRPC GetPath for {store_path}: {e}"))?;

    match info {
        Some(i) => Ok(Some((i, nar_data))),
        None => Ok(None),
    }
}

/// Build a proto PathInfo from local data (for PutPath).
#[allow(clippy::too_many_arguments)]
pub(super) fn make_proto_path_info(
    store_path: &str,
    deriver: &str,
    nar_hash: &[u8],
    nar_size: u64,
    references: &[String],
    registration_time: u64,
    ultimate: bool,
    sigs: &[String],
    ca: &str,
) -> types::PathInfo {
    types::PathInfo {
        store_path: store_path.to_string(),
        store_path_hash: Vec::new(),
        deriver: deriver.to_string(),
        nar_hash: nar_hash.to_vec(),
        nar_size,
        references: references.to_vec(),
        registration_time,
        ultimate,
        signatures: sigs.to_vec(),
        content_address: ca.to_string(),
    }
}
