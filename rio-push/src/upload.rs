//! Upload logic: FindMissingPaths dedup and single-path PutPath.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use sha2::{Digest, Sha256};
use tonic::transport::Channel;
use tracing::debug;

use rio_proto::StoreServiceClient;
use rio_proto::client::chunk_nar_for_put;
use rio_proto::types::FindMissingPathsRequest;
use rio_proto::validated::ValidatedPathInfo;

/// Default timeout for store RPCs (5 minutes).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// Query the store for which paths are missing.
pub async fn find_missing(
    client: &mut StoreServiceClient<Channel>,
    store_paths: Vec<String>,
) -> anyhow::Result<Vec<String>> {
    let req = FindMissingPathsRequest { store_paths };
    let resp = tokio::time::timeout(DEFAULT_TIMEOUT, client.find_missing_paths(req))
        .await
        .context("FindMissingPaths timed out")?
        .context("FindMissingPaths RPC failed")?;
    Ok(resp.into_inner().missing_paths)
}

/// Upload a single store path's NAR to the store.
///
/// Returns `true` if the path was created, `false` if it already existed.
pub async fn push_path(
    client: &mut StoreServiceClient<Channel>,
    info: ValidatedPathInfo,
    nar: Vec<u8>,
    oidc_token: Option<&str>,
) -> anyhow::Result<bool> {
    let store_path = info.store_path.as_str().to_string();

    // Verify NAR hash client-side before uploading.
    let computed_hash: [u8; 32] = Sha256::digest(&nar).into();
    if computed_hash != info.nar_hash {
        anyhow::bail!(
            "NAR hash mismatch for {store_path}: \
             nix reports {}, computed {}",
            hex::encode(info.nar_hash),
            hex::encode(computed_hash),
        );
    }

    let nar_arc: Arc<[u8]> = Arc::from(nar);
    let stream = chunk_nar_for_put(info, nar_arc);

    let mut request = tonic::Request::new(stream);

    if let Some(token) = oidc_token {
        request.metadata_mut().insert(
            "x-rio-oidc-token",
            token
                .parse()
                .context("OIDC token contains invalid header characters")?,
        );
    }

    let resp = tokio::time::timeout(DEFAULT_TIMEOUT, client.put_path(request))
        .await
        .context("PutPath timed out")?
        .context("PutPath RPC failed")?;

    let created = resp.into_inner().created;
    debug!(store_path, created, "PutPath completed");
    Ok(created)
}
