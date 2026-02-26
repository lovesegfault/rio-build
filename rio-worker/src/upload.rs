//! Output upload to rio-store after build completion.
//!
//! Scans the overlay upper layer for new store paths, serializes each as
//! a NAR, computes SHA-256, and uploads via `StoreService.PutPath` gRPC
//! with retry on failure.

use std::path::Path;
use std::time::Duration;

use futures_util::stream::{self, StreamExt};
use sha2::{Digest, Sha256};
use tonic::transport::Channel;

use rio_nix::nar;
use rio_proto::store::store_service_client::StoreServiceClient;
use rio_proto::types::{PathInfo, PutPathMetadata, PutPathRequest, put_path_request};

/// Maximum number of upload retry attempts.
const MAX_UPLOAD_RETRIES: u32 = 3;

/// Base delay for exponential backoff between retries.
const RETRY_BASE_DELAY: Duration = Duration::from_secs(1);

/// Maximum NAR chunk size for streaming upload (256 KB).
const NAR_CHUNK_SIZE: usize = 256 * 1024;

/// Maximum concurrent output uploads. Bounds peak memory: each in-flight
/// upload holds one full NAR in memory during the PutPath stream.
const MAX_PARALLEL_UPLOADS: usize = 4;

/// Result of uploading a single output path.
#[derive(Debug)]
pub struct UploadResult {
    /// The store path that was uploaded.
    pub store_path: String,
    /// SHA-256 digest of the NAR.
    pub nar_hash: Vec<u8>,
    /// Size of the NAR in bytes.
    pub nar_size: u64,
}

/// Errors from upload operations.
#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    #[error("NAR serialization failed for {path}: {source}")]
    NarSerialize { path: String, source: nar::NarError },
    #[error("upload failed after {MAX_UPLOAD_RETRIES} retries for {path}: {source}")]
    UploadExhausted { path: String, source: tonic::Status },
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Scan the overlay upper layer for new store paths.
///
/// Returns basenames of paths under `/nix/store/` in the upper layer
/// that represent build outputs.
pub fn scan_new_outputs(upper_dir: &Path) -> std::io::Result<Vec<String>> {
    let store_dir = upper_dir.join("nix/store");
    if !store_dir.exists() {
        return Ok(Vec::new());
    }

    let mut outputs = Vec::new();
    for entry in std::fs::read_dir(&store_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        // Skip hidden files and the .links directory
        if !name.starts_with('.') {
            outputs.push(name);
        }
    }

    // read_dir order is filesystem-dependent; sort for deterministic behavior.
    outputs.sort();
    Ok(outputs)
}

/// Upload a single output path to the store.
///
/// Serializes the path as a NAR, computes SHA-256, and streams via PutPath.
/// Retries with exponential backoff up to `MAX_UPLOAD_RETRIES` times.
pub async fn upload_output(
    store_client: &mut StoreServiceClient<Channel>,
    upper_dir: &Path,
    output_basename: &str,
) -> Result<UploadResult, UploadError> {
    let output_path = upper_dir.join("nix/store").join(output_basename);
    let store_path = format!("/nix/store/{output_basename}");

    // Serialize to NAR
    let nar_data = nar::dump_path(&output_path).map_err(|e| UploadError::NarSerialize {
        path: store_path.clone(),
        source: e,
    })?;

    // Compute SHA-256
    let nar_hash = Sha256::digest(&nar_data).to_vec();
    let nar_size = nar_data.len() as u64;

    tracing::info!(
        store_path = %store_path,
        nar_size,
        nar_hash = %hex::encode(&nar_hash),
        "uploading output"
    );

    // Retry loop with exponential backoff
    let mut last_error = None;
    for attempt in 0..MAX_UPLOAD_RETRIES {
        if attempt > 0 {
            let delay = RETRY_BASE_DELAY * 2u32.pow(attempt - 1);
            tracing::warn!(
                store_path = %store_path,
                attempt,
                delay_ms = delay.as_millis() as u64,
                "retrying upload"
            );
            tokio::time::sleep(delay).await;
        }

        match do_upload(store_client, &store_path, &nar_hash, nar_size, &nar_data).await {
            Ok(()) => {
                metrics::counter!("rio_worker_uploads_total", "status" => "success").increment(1);
                return Ok(UploadResult {
                    store_path,
                    nar_hash,
                    nar_size,
                });
            }
            Err(e) => {
                tracing::warn!(
                    store_path = %store_path,
                    attempt,
                    error = %e,
                    "upload attempt failed"
                );
                last_error = Some(e);
            }
        }
    }

    metrics::counter!("rio_worker_uploads_total", "status" => "exhausted").increment(1);
    Err(UploadError::UploadExhausted {
        path: store_path,
        source: last_error.unwrap(),
    })
}

/// Perform a single upload attempt via PutPath streaming RPC.
async fn do_upload(
    store_client: &mut StoreServiceClient<Channel>,
    store_path: &str,
    nar_hash: &[u8],
    nar_size: u64,
    nar_data: &[u8],
) -> Result<(), tonic::Status> {
    // Build the stream of PutPathRequest messages
    let metadata_msg = PutPathRequest {
        msg: Some(put_path_request::Msg::Metadata(PutPathMetadata {
            info: Some(PathInfo {
                store_path: store_path.to_string(),
                nar_hash: nar_hash.to_vec(),
                nar_size,
                // Other fields will be populated by the store
                ..Default::default()
            }),
        })),
    };

    // Chunk the NAR data
    let mut messages = vec![metadata_msg];
    for chunk in nar_data.chunks(NAR_CHUNK_SIZE) {
        messages.push(PutPathRequest {
            msg: Some(put_path_request::Msg::NarChunk(chunk.to_vec())),
        });
    }

    let stream = tokio_stream::iter(messages);
    store_client.put_path(stream).await?;

    Ok(())
}

/// Upload all new outputs from the overlay upper layer.
///
/// Uploads run concurrently (bounded by `MAX_PARALLEL_UPLOADS`). On exhausted
/// retries for any output, returns an error — partial uploads are safe since
/// `PutPath` is idempotent and the caller will report `InfrastructureFailure`.
///
/// Result order is **not** guaranteed. Callers must not assume results
/// correspond positionally to any input list; use `UploadResult.store_path`
/// to identify outputs.
pub async fn upload_all_outputs(
    store_client: &StoreServiceClient<Channel>,
    upper_dir: &Path,
) -> Result<Vec<UploadResult>, UploadError> {
    let outputs = scan_new_outputs(upper_dir)?;

    tracing::info!(
        count = outputs.len(),
        max_parallel = MAX_PARALLEL_UPLOADS,
        "uploading build outputs"
    );

    let upper_dir = upper_dir.to_path_buf();
    let results: Vec<Result<UploadResult, UploadError>> = stream::iter(outputs)
        .map(|output| {
            let mut client = store_client.clone();
            let upper_dir = upper_dir.clone();
            async move { upload_output(&mut client, &upper_dir, &output).await }
        })
        .buffer_unordered(MAX_PARALLEL_UPLOADS)
        .collect()
        .await;

    results.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_scan_new_outputs_empty() {
        let dir = tempfile::tempdir().unwrap();
        let outputs = scan_new_outputs(dir.path()).unwrap();
        assert!(outputs.is_empty());
    }

    #[test]
    fn test_scan_new_outputs_with_paths() {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = dir.path().join("nix/store");
        fs::create_dir_all(&store_dir).unwrap();

        // Create in reverse alphabetical order to verify internal sort.
        fs::create_dir(store_dir.join("def-world")).unwrap();
        fs::create_dir(store_dir.join("abc-hello")).unwrap();
        // Hidden files should be skipped
        fs::write(store_dir.join(".links"), "").unwrap();

        // scan_new_outputs sorts internally for deterministic output.
        let outputs = scan_new_outputs(dir.path()).unwrap();
        assert_eq!(outputs, vec!["abc-hello", "def-world"]);
    }

    #[test]
    fn test_nar_chunk_size() {
        // Verify chunk size is reasonable
        assert_eq!(NAR_CHUNK_SIZE, 256 * 1024);
    }
}
