//! Store-query helpers using gRPC.

use super::*;

/// Query PathInfo from store via gRPC. Returns None if NOT_FOUND.
pub(super) async fn grpc_query_path_info(
    store_client: &mut StoreServiceClient<Channel>,
    store_path: &str,
) -> anyhow::Result<Option<types::PathInfo>> {
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
    rio_proto::client::get_path_nar(store_client, store_path, GRPC_STREAM_TIMEOUT, MAX_NAR_SIZE)
        .await
        .map_err(|e| anyhow::anyhow!("gRPC GetPath for {store_path}: {e}"))
}
