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
use rio_proto::validated::ValidatedPathInfo;

/// Maximum number of upload retry attempts.
const MAX_UPLOAD_RETRIES: u32 = 3;

/// Base delay for exponential backoff between retries.
const RETRY_BASE_DELAY: Duration = Duration::from_secs(1);

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
    let read_dir = match std::fs::read_dir(&store_dir) {
        Ok(iter) => iter,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };

    let mut outputs = Vec::new();
    for entry in read_dir {
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
async fn upload_output(
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
    let nar_hash: [u8; 32] = Sha256::digest(&nar_data).into();
    let nar_size = nar_data.len() as u64;
    let nar_data: std::sync::Arc<[u8]> = nar_data.into();

    tracing::info!(
        store_path = %store_path,
        nar_size,
        nar_hash = %hex::encode(nar_hash),
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

        match do_upload(
            store_client,
            &store_path,
            nar_hash,
            nar_size,
            nar_data.clone(),
        )
        .await
        {
            Ok(()) => {
                metrics::counter!("rio_worker_uploads_total", "status" => "success").increment(1);
                return Ok(UploadResult {
                    store_path,
                    nar_hash: nar_hash.to_vec(),
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
        source: last_error.expect("retry loop ran ≥1 times; each failure sets last_error"),
    })
}

/// Perform a single upload attempt via PutPath streaming RPC.
///
/// `store_path` comes from `scan_new_outputs` which reads directory entries
/// under `{upper}/nix/store/` — these are nix-daemon-generated paths, but
/// we parse defensively: a bug in our overlay setup could leave garbage here.
async fn do_upload(
    store_client: &mut StoreServiceClient<Channel>,
    store_path: &str,
    nar_hash: [u8; 32],
    nar_size: u64,
    nar_data: std::sync::Arc<[u8]>,
) -> Result<(), tonic::Status> {
    let parsed_path = rio_nix::store_path::StorePath::parse(store_path).map_err(|e| {
        tonic::Status::invalid_argument(format!(
            "output store path {store_path:?} from overlay upper is malformed: {e}"
        ))
    })?;
    let info = ValidatedPathInfo {
        store_path: parsed_path,
        nar_hash,
        nar_size,
        // Other fields populated by the store server.
        store_path_hash: Vec::new(),
        deriver: None,
        references: Vec::new(),
        registration_time: 0,
        ultimate: false,
        signatures: Vec::new(),
        content_address: None,
    };
    let stream = rio_proto::client::chunk_nar_for_put(info, nar_data);
    rio_common::grpc::with_timeout_status(
        "PutPath",
        rio_common::grpc::GRPC_STREAM_TIMEOUT,
        store_client.put_path(stream),
    )
    .await?;
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
        assert_eq!(rio_proto::client::NAR_CHUNK_SIZE, 256 * 1024);
    }

    // -----------------------------------------------------------------------
    // gRPC upload tests via MockStore
    // -----------------------------------------------------------------------

    use rio_test_support::fixtures::test_store_basename;
    use rio_test_support::grpc::{MockStore, spawn_mock_store};
    use std::sync::atomic::Ordering;

    async fn spawn_and_connect() -> (
        MockStore,
        StoreServiceClient<Channel>,
        tokio::task::JoinHandle<()>,
    ) {
        let (store, addr, handle) = spawn_mock_store().await.unwrap();
        let client = rio_proto::client::connect_store(&addr.to_string())
            .await
            .expect("connect to mock store");
        (store, client, handle)
    }

    /// Write a file at `{tmp}/nix/store/{basename}` and return the tempdir.
    fn make_output_file(basename: &str, contents: &[u8]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let store_dir = tmp.path().join("nix/store");
        fs::create_dir_all(&store_dir).unwrap();
        fs::write(store_dir.join(basename), contents).unwrap();
        tmp
    }

    #[tokio::test]
    async fn test_upload_output_success() {
        let (store, mut client, _h) = spawn_and_connect().await;
        let basename = test_store_basename("hello");
        let tmp = make_output_file(&basename, b"hello world");

        let result = upload_output(&mut client, tmp.path(), &basename)
            .await
            .expect("upload should succeed");

        assert_eq!(result.store_path, format!("/nix/store/{basename}"));
        // Hash must match SHA-256 of the NAR serialization.
        let expected_nar = nar::dump_path(&tmp.path().join("nix/store").join(&basename)).unwrap();
        let expected_hash: [u8; 32] = Sha256::digest(&expected_nar).into();
        assert_eq!(result.nar_hash, expected_hash.to_vec());
        assert_eq!(result.nar_size, expected_nar.len() as u64);

        // MockStore should have recorded exactly one PutPath call.
        let puts = store.put_calls.read().unwrap();
        assert_eq!(puts.len(), 1);
        assert_eq!(puts[0].store_path, format!("/nix/store/{basename}"));
    }

    /// Retry with exponential backoff — first 2 attempts fail, 3rd succeeds.
    /// start_paused auto-advances the clock during sleep() so the 1s+2s
    /// backoff delays don't wall-clock-block the test.
    #[tokio::test(start_paused = true)]
    async fn test_upload_output_retries_then_succeeds() {
        let (store, mut client, _h) = spawn_and_connect().await;
        store.fail_next_puts.store(2, Ordering::SeqCst);
        let basename = test_store_basename("retry");
        let tmp = make_output_file(&basename, b"retry me");

        let result = upload_output(&mut client, tmp.path(), &basename)
            .await
            .expect("upload should succeed on 3rd attempt");

        assert_eq!(result.store_path, format!("/nix/store/{basename}"));
        // Only the successful attempt records the put.
        assert_eq!(store.put_calls.read().unwrap().len(), 1);
        // All injected failures should have been consumed.
        assert_eq!(store.fail_next_puts.load(Ordering::SeqCst), 0);
    }

    /// More failures than MAX_UPLOAD_RETRIES → UploadExhausted.
    #[tokio::test(start_paused = true)]
    async fn test_upload_output_exhausts_retries() {
        let (store, mut client, _h) = spawn_and_connect().await;
        store
            .fail_next_puts
            .store(MAX_UPLOAD_RETRIES + 1, Ordering::SeqCst);
        let basename = test_store_basename("exhaust");
        let tmp = make_output_file(&basename, b"never uploads");

        let err = upload_output(&mut client, tmp.path(), &basename)
            .await
            .expect_err("upload should exhaust retries");

        assert!(
            matches!(err, UploadError::UploadExhausted { .. }),
            "expected UploadExhausted, got {err:?}"
        );
        // No successful PutPath recorded.
        assert_eq!(store.put_calls.read().unwrap().len(), 0);
    }

    /// upload_all_outputs runs concurrently; all outputs land in MockStore.
    #[tokio::test]
    async fn test_upload_all_outputs_multiple() {
        let (store, client, _h) = spawn_and_connect().await;
        let tmp = tempfile::tempdir().unwrap();
        let store_dir = tmp.path().join("nix/store");
        fs::create_dir_all(&store_dir).unwrap();
        let (b1, b2, b3) = (
            test_store_basename("one"),
            test_store_basename("two"),
            test_store_basename("three"),
        );
        fs::write(store_dir.join(&b1), b"one").unwrap();
        fs::write(store_dir.join(&b2), b"two").unwrap();
        fs::write(store_dir.join(&b3), b"three").unwrap();

        let results = upload_all_outputs(&client, tmp.path())
            .await
            .expect("all uploads succeed");

        assert_eq!(results.len(), 3);
        // Result order is NOT guaranteed (buffer_unordered). Collect to set.
        let paths: std::collections::HashSet<_> =
            results.iter().map(|r| r.store_path.clone()).collect();
        assert!(paths.contains(&format!("/nix/store/{b1}")));
        assert!(paths.contains(&format!("/nix/store/{b2}")));
        assert!(paths.contains(&format!("/nix/store/{b3}")));
        assert_eq!(store.put_calls.read().unwrap().len(), 3);
    }

    /// NAR serialization fails on ENOENT → UploadError::NarSerialize, no gRPC.
    #[tokio::test]
    async fn test_upload_output_nar_serialize_error() {
        let (store, mut client, _h) = spawn_and_connect().await;
        let tmp = tempfile::tempdir().unwrap();
        // Create nix/store/ dir but NOT the output file.
        fs::create_dir_all(tmp.path().join("nix/store")).unwrap();

        let err = upload_output(&mut client, tmp.path(), "does-not-exist")
            .await
            .expect_err("should fail NAR serialization");

        assert!(
            matches!(err, UploadError::NarSerialize { .. }),
            "expected NarSerialize, got {err:?}"
        );
        // No gRPC call attempted.
        assert_eq!(store.put_calls.read().unwrap().len(), 0);
    }
}
