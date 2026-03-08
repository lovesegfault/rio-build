//! Output upload to rio-store after build completion.
//!
//! Scans the overlay upper layer for new store paths, serializes each as
//! a NAR, computes SHA-256, and uploads via `StoreService.PutPath` gRPC
//! with retry on failure.
// r[impl worker.upload.multi-output]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::stream::{self, StreamExt};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tonic::transport::Channel;
use tracing::instrument;

use rio_nix::nar;
use rio_proto::StoreServiceClient;
use rio_proto::types::{
    PathInfo, PutPathMetadata, PutPathRequest, PutPathTrailer, put_path_request,
};

/// Maximum number of upload retry attempts.
const MAX_UPLOAD_RETRIES: u32 = 3;

/// Base delay for exponential backoff between retries.
const RETRY_BASE_DELAY: Duration = Duration::from_secs(1);

/// Maximum concurrent output uploads. Phase2a held one full NAR in memory
/// per in-flight upload (8GiB peak for a 4GiB NAR × 4 = 32GiB). Phase2b's
/// streaming tee bounds each in-flight upload to `STREAM_CHANNEL_BUF × 256KiB`
/// (~1MiB buffered), so 4 parallel is now ~4MiB peak. We keep 4 as the
/// default — disk read bandwidth is the new bottleneck, and 4 concurrent
/// reads saturate most NVMe queues.
const MAX_PARALLEL_UPLOADS: usize = 4;

/// Channel buffer between the sync `dump_path_streaming` (in spawn_blocking)
/// and the async gRPC send. At 256KiB/chunk, this is 1MiB of backpressure
/// headroom — enough to absorb jitter between disk read and network send
/// without blocking either side for long.
const STREAM_CHANNEL_BUF: usize = 4;

/// Result of uploading a single output path.
#[derive(Debug)]
pub struct UploadResult {
    /// The store path that was uploaded.
    pub store_path: String,
    /// SHA-256 digest of the NAR. Always 32 bytes — `[u8; 32]` instead of
    /// `Vec<u8>` so the type system enforces it (no `hash.len() == 32` check
    /// at every consumer). The source (`do_upload_streaming`) already returns
    /// `[u8; 32]`; the old `.to_vec()` was a gratuitous heap allocation.
    pub nar_hash: [u8; 32],
    /// Size of the NAR in bytes.
    pub nar_size: u64,
}

/// Errors from upload operations.
///
/// Phase2a had a `NarSerialize` variant — removed in phase2b. NAR
/// serialization now happens INSIDE the retry loop (each retry is a fresh
/// disk read), so NAR errors surface as `UploadExhausted` wrapping a
/// `tonic::Status::internal("NAR serialization failed...")`. Same fidelity
/// (error message names path + cause), one fewer match arm for callers.
#[derive(Debug, thiserror::Error)]
pub enum UploadError {
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

/// Upload a single output path to the store via single-pass streaming tee.
///
/// ## Single-pass architecture (phase2b)
///
/// Phase2a: `dump_path()` → full NAR in memory → `Sha256::digest()` →
/// `chunk_nar_for_put()`. For a 4GiB output: 4GiB for `NarNode::contents`
/// + 4GiB for the serialized Vec = 8GiB peak.
///
/// Phase2b: `dump_path_streaming()` (inside `spawn_blocking`) writes to a
/// `HashingChannelWriter` that (a) hashes every byte through SHA-256 AND
/// (b) buffers into 256KiB chunks → `blocking_send()` to an mpsc channel.
/// Tonic polls the channel as `PutPathRequest` stream. Hash finalized
/// after the last byte; sent as `PutPathTrailer`. Peak memory: ~1MiB
/// (`STREAM_CHANNEL_BUF` × 256KiB) regardless of output size.
///
/// ## Retry cost
///
/// Each retry re-reads the file from disk (`spawn_blocking` + channel are
/// consumed on each attempt; can't rewind them). At `MAX_UPLOAD_RETRIES=3`
/// that's worst-case 3× disk reads. Retries are rare (transient S3/gRPC
/// blips); the extra reads cost seconds on NVMe for a 4GiB file — trivial
/// vs. the 32GiB memory saving.
#[instrument(skip_all, fields(store_path = %format!("/nix/store/{output_basename}")))]
async fn upload_output(
    store_client: &mut StoreServiceClient<Channel>,
    upper_dir: &Path,
    output_basename: &str,
) -> Result<UploadResult, UploadError> {
    let output_path = upper_dir.join("nix/store").join(output_basename);
    let store_path = format!("/nix/store/{output_basename}");

    // Validate the store path ONCE, before the retry loop. A malformed
    // path (overlay setup bug) won't fix itself on retry. Discard the
    // parsed value — do_upload_streaming sends the string form raw; the
    // store re-parses server-side.
    let _ = rio_nix::store_path::StorePath::parse(&store_path).map_err(|e| {
        UploadError::UploadExhausted {
            path: store_path.clone(),
            source: tonic::Status::invalid_argument(format!(
                "output store path {store_path:?} from overlay upper is malformed: {e}"
            )),
        }
    })?;

    tracing::info!(store_path = %store_path, "uploading output (streaming tee)");

    let mut last_error = None;
    for attempt in 0..MAX_UPLOAD_RETRIES {
        if attempt > 0 {
            let delay = RETRY_BASE_DELAY * 2u32.pow(attempt - 1);
            tracing::warn!(
                store_path = %store_path,
                attempt,
                delay_ms = delay.as_millis() as u64,
                "retrying upload (fresh disk read)"
            );
            tokio::time::sleep(delay).await;
        }

        match do_upload_streaming(store_client, &store_path, output_path.clone()).await {
            Ok((nar_hash, nar_size)) => {
                metrics::counter!("rio_worker_uploads_total", "status" => "success").increment(1);
                tracing::info!(
                    store_path = %store_path,
                    nar_size,
                    nar_hash = %hex::encode(nar_hash),
                    "upload complete"
                );
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
        source: last_error.expect("retry loop ran ≥1 times; each failure sets last_error"),
    })
}

/// One upload attempt: spawn_blocking(dump_streaming → HashingChannelWriter)
/// → mpsc → tonic gRPC. Returns (hash, size) on success.
#[instrument(skip_all)]
async fn do_upload_streaming(
    store_client: &mut StoreServiceClient<Channel>,
    store_path: &str,
    output_path: PathBuf,
) -> Result<([u8; 32], u64), tonic::Status> {
    // Channel bridges sync `dump_path_streaming` (spawn_blocking) to async
    // gRPC. Backpressure: when full, `blocking_send` inside the writer
    // blocks the spawn_blocking thread until tonic pulls a chunk.
    let (tx, rx) = mpsc::channel::<PutPathRequest>(STREAM_CHANNEL_BUF);

    // First message: metadata with EMPTY hash/size → trailer mode. Send
    // this from the async side BEFORE spawning the blocking task, so the
    // message order is guaranteed (metadata must be first; chunks follow).
    let info = PathInfo {
        store_path: store_path.to_string(),
        nar_hash: Vec::new(), // EMPTY → triggers trailer mode on the store
        nar_size: 0,          //         (real values arrive in trailer)
        store_path_hash: Vec::new(),
        deriver: String::new(),
        references: Vec::new(),
        registration_time: 0,
        ultimate: false,
        signatures: Vec::new(),
        content_address: String::new(),
    };
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::Metadata(PutPathMetadata {
            info: Some(info),
        })),
    })
    .await
    .map_err(|_| tonic::Status::internal("upload channel closed before metadata send"))?;

    // Spawn the streaming dump on the blocking pool. It hashes + forwards
    // chunks + sends trailer, then returns (hash, size). MUST be
    // spawn_blocking: `HashingChannelWriter::write` calls `blocking_send`
    // which PANICS if invoked from a runtime thread.
    let dump_task = tokio::task::spawn_blocking(move || {
        let mut sink = HashingChannelWriter::new(tx);
        let written = nar::dump_path_streaming(&output_path, &mut sink)
            .map_err(|e| nar_err_to_status(&output_path, e))?;
        // finalize() sends the trailer and drops tx (which closes the channel,
        // telling tonic the stream is done).
        let (hash, size) = sink.finalize();
        // Sanity: dump_path_streaming's CountingWriter count should match
        // the tee's total. A mismatch means the CountingWriter or the tee
        // has a bug; fail loud instead of uploading a corrupted NAR.
        debug_assert_eq!(written, size, "CountingWriter vs tee size mismatch");
        Ok::<_, tonic::Status>((hash, size))
    });

    // Drive the gRPC stream. Chunks + trailer arrive on `rx` as the
    // blocking task produces them. `with_timeout_status` bounds the whole
    // thing (initial call + stream drain) so a stuck disk read can't hang
    // the worker forever.
    let outbound = tokio_stream::wrappers::ReceiverStream::new(rx);
    let put_result = rio_common::grpc::with_timeout_status(
        "PutPath",
        rio_common::grpc::GRPC_STREAM_TIMEOUT,
        store_client.put_path(outbound),
    )
    .await;

    // Join the blocking task. Two cases:
    //   (a) put_path succeeded → dump_task has definitely finished (it
    //       closed the channel before put_path could return). Await is
    //       non-blocking; just collect the result.
    //   (b) put_path failed → dump_task might still be running (blocked on
    //       blocking_send to a channel whose rx was dropped). blocking_send
    //       returns Err when rx is dropped; the task will exit with a
    //       BrokenPipe. Await that.
    let dump_result = dump_task
        .await
        .map_err(|e| tonic::Status::internal(format!("dump task panicked: {e}")))?;

    // Error priority: if BOTH failed, surface the gRPC error (it's the
    // one the operator cares about — "store unreachable" is more useful
    // than "BrokenPipe on a channel"). If only one failed, surface it.
    put_result?;
    dump_result
}

fn nar_err_to_status(path: &Path, e: nar::NarError) -> tonic::Status {
    tonic::Status::internal(format!(
        "NAR serialization failed for {}: {e}",
        path.display()
    ))
}

// ---------------------------------------------------------------------------
// HashingChannelWriter — the tee sink
// ---------------------------------------------------------------------------

/// Sync `Write` sink that tees every byte through SHA-256 AND forwards
/// 256KiB chunks to an mpsc channel as `PutPathRequest::NarChunk` messages.
///
/// `write()` calls `Sender::blocking_send()` — **MUST** run inside
/// `spawn_blocking`. Calling from a tokio runtime thread would panic
/// ("Cannot block the current thread from within a runtime").
///
/// Backpressure: when the channel is full (`STREAM_CHANNEL_BUF` chunks
/// in flight), `blocking_send` blocks until tonic pulls one. This ties
/// disk read speed to network send speed without unbounded buffering.
struct HashingChannelWriter {
    hasher: Sha256,
    /// Current partial chunk. Flushed to the channel when it reaches
    /// `NAR_CHUNK_SIZE`.
    buf: Vec<u8>,
    /// Total bytes ever written (what goes in the trailer's `nar_size`).
    total: u64,
    tx: mpsc::Sender<PutPathRequest>,
}

impl HashingChannelWriter {
    fn new(tx: mpsc::Sender<PutPathRequest>) -> Self {
        Self {
            hasher: Sha256::new(),
            buf: Vec::with_capacity(rio_proto::client::NAR_CHUNK_SIZE),
            total: 0,
            tx,
        }
    }

    /// Send the final partial chunk (if any), then the trailer, then drop
    /// the sender (closing the channel, signaling stream-end to tonic).
    ///
    /// Returns `(sha256, total_bytes)` — what goes in the trailer.
    fn finalize(mut self) -> ([u8; 32], u64) {
        // Final partial chunk. May be empty if total was a multiple of
        // NAR_CHUNK_SIZE — in that case we skip the send (store would
        // accept an empty chunk but it's wasteful).
        if !self.buf.is_empty() {
            let chunk = std::mem::take(&mut self.buf);
            // Best-effort: if the receiver is gone (gRPC already failed),
            // blocking_send returns Err. We can't propagate here (finalize
            // returns infallibly), but the gRPC error will surface via
            // the put_result in do_upload_streaming.
            let _ = self.tx.blocking_send(PutPathRequest {
                msg: Some(put_path_request::Msg::NarChunk(chunk)),
            });
        }

        let hash: [u8; 32] = self.hasher.finalize().into();
        let _ = self.tx.blocking_send(PutPathRequest {
            msg: Some(put_path_request::Msg::Trailer(PutPathTrailer {
                nar_hash: hash.to_vec(),
                nar_size: self.total,
            })),
        });
        // tx drops here → channel closes → tonic's ReceiverStream yields None
        // → server sees stream end.
        (hash, self.total)
    }
}

impl Write for HashingChannelWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.hasher.update(data);
        self.total += data.len() as u64;
        self.buf.extend_from_slice(data);

        // Chunk-size flush. `>=` not `==` because `data` might be more than
        // one chunk's worth (dump_path_streaming's 256KiB STREAM_CHUNK
        // matches NAR_CHUNK_SIZE, but the NAR framing bytes in between
        // file contents can push a write call over the edge).
        while self.buf.len() >= rio_proto::client::NAR_CHUNK_SIZE {
            let chunk: Vec<u8> = self
                .buf
                .drain(..rio_proto::client::NAR_CHUNK_SIZE)
                .collect();
            self.tx
                .blocking_send(PutPathRequest {
                    msg: Some(put_path_request::Msg::NarChunk(chunk)),
                })
                .map_err(|_| {
                    // Receiver dropped = gRPC stream already failed.
                    // Surface as BrokenPipe so dump_path_streaming bails
                    // cleanly instead of writing more bytes to /dev/null.
                    std::io::Error::from(std::io::ErrorKind::BrokenPipe)
                })?;
        }

        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // We don't flush-to-channel here — chunks go out on their own
        // schedule (size-based). A Write::flush() call from inside
        // dump_path_streaming would produce an undersized chunk and waste
        // a gRPC roundtrip. finalize() handles the final partial chunk.
        Ok(())
    }
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
#[instrument(skip_all)]
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

// r[verify worker.upload.multi-output]
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_scan_new_outputs_empty() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let outputs = scan_new_outputs(dir.path())?;
        assert!(outputs.is_empty());
        Ok(())
    }

    #[test]
    fn test_scan_new_outputs_with_paths() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store_dir = dir.path().join("nix/store");
        fs::create_dir_all(&store_dir)?;

        // Create in reverse alphabetical order to verify internal sort.
        fs::create_dir(store_dir.join("def-world"))?;
        fs::create_dir(store_dir.join("abc-hello"))?;
        // Hidden files should be skipped
        fs::write(store_dir.join(".links"), "")?;

        // scan_new_outputs sorts internally for deterministic output.
        let outputs = scan_new_outputs(dir.path())?;
        assert_eq!(outputs, vec!["abc-hello", "def-world"]);
        Ok(())
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
    use rio_test_support::grpc::spawn_mock_store_with_client;
    use std::sync::atomic::Ordering;

    /// Write a file at `{tmp}/nix/store/{basename}` and return the tempdir.
    fn make_output_file(basename: &str, contents: &[u8]) -> anyhow::Result<tempfile::TempDir> {
        let tmp = tempfile::tempdir()?;
        let store_dir = tmp.path().join("nix/store");
        fs::create_dir_all(&store_dir)?;
        fs::write(store_dir.join(basename), contents)?;
        Ok(tmp)
    }

    #[tokio::test]
    async fn test_upload_output_success() -> anyhow::Result<()> {
        let (store, mut client, _h) = spawn_mock_store_with_client().await?;
        let basename = test_store_basename("hello");
        let tmp = make_output_file(&basename, b"hello world")?;

        let result = upload_output(&mut client, tmp.path(), &basename)
            .await
            .expect("upload should succeed");

        assert_eq!(result.store_path, format!("/nix/store/{basename}"));
        // Hash must match SHA-256 of the NAR serialization.
        let expected_nar = nar::dump_path(&tmp.path().join("nix/store").join(&basename))?;
        let expected_hash: [u8; 32] = Sha256::digest(&expected_nar).into();
        assert_eq!(result.nar_hash, expected_hash);
        assert_eq!(result.nar_size, expected_nar.len() as u64);

        // MockStore should have recorded exactly one PutPath call.
        let puts = store.put_calls.read().unwrap();
        assert_eq!(puts.len(), 1);
        assert_eq!(puts[0].store_path, format!("/nix/store/{basename}"));
        Ok(())
    }

    /// Retry with exponential backoff — first 2 attempts fail, 3rd succeeds.
    /// start_paused auto-advances the clock during sleep() so the 1s+2s
    /// backoff delays don't wall-clock-block the test.
    #[tokio::test(start_paused = true)]
    async fn test_upload_output_retries_then_succeeds() -> anyhow::Result<()> {
        let (store, mut client, _h) = spawn_mock_store_with_client().await?;
        store.fail_next_puts.store(2, Ordering::SeqCst);
        let basename = test_store_basename("retry");
        let tmp = make_output_file(&basename, b"retry me")?;

        let result = upload_output(&mut client, tmp.path(), &basename)
            .await
            .expect("upload should succeed on 3rd attempt");

        assert_eq!(result.store_path, format!("/nix/store/{basename}"));
        // Only the successful attempt records the put.
        assert_eq!(store.put_calls.read().unwrap().len(), 1);
        // All injected failures should have been consumed.
        assert_eq!(store.fail_next_puts.load(Ordering::SeqCst), 0);
        Ok(())
    }

    /// More failures than MAX_UPLOAD_RETRIES → UploadExhausted.
    #[tokio::test(start_paused = true)]
    async fn test_upload_output_exhausts_retries() -> anyhow::Result<()> {
        let (store, mut client, _h) = spawn_mock_store_with_client().await?;
        store
            .fail_next_puts
            .store(MAX_UPLOAD_RETRIES + 1, Ordering::SeqCst);
        let basename = test_store_basename("exhaust");
        let tmp = make_output_file(&basename, b"never uploads")?;

        let err = upload_output(&mut client, tmp.path(), &basename)
            .await
            .expect_err("upload should exhaust retries");

        assert!(
            matches!(err, UploadError::UploadExhausted { .. }),
            "expected UploadExhausted, got {err:?}"
        );
        // No successful PutPath recorded.
        assert_eq!(store.put_calls.read().unwrap().len(), 0);
        Ok(())
    }

    /// upload_all_outputs runs concurrently; all outputs land in MockStore.
    #[tokio::test]
    async fn test_upload_all_outputs_multiple() -> anyhow::Result<()> {
        let (store, client, _h) = spawn_mock_store_with_client().await?;
        let tmp = tempfile::tempdir()?;
        let store_dir = tmp.path().join("nix/store");
        fs::create_dir_all(&store_dir)?;
        let (b1, b2, b3) = (
            test_store_basename("one"),
            test_store_basename("two"),
            test_store_basename("three"),
        );
        fs::write(store_dir.join(&b1), b"one")?;
        fs::write(store_dir.join(&b2), b"two")?;
        fs::write(store_dir.join(&b3), b"three")?;

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
        Ok(())
    }

    /// ENOENT during streaming dump → UploadExhausted (wraps the NAR error).
    /// The dump happens inside spawn_blocking AFTER the gRPC stream opens,
    /// so the PutPath call is attempted — the store sees metadata then the
    /// channel closes without a trailer, and fails. We just verify the
    /// worker surfaces a useful error.
    #[tokio::test(start_paused = true)]
    async fn test_upload_output_nar_serialize_error() -> anyhow::Result<()> {
        let (_store, mut client, _h) = spawn_mock_store_with_client().await?;
        let tmp = tempfile::tempdir()?;
        // Create nix/store/ dir but NOT the output file.
        fs::create_dir_all(tmp.path().join("nix/store"))?;

        // Use a VALID basename (32-char hash) so we get past the path
        // validation and into the dump that actually ENOENTs.
        let basename = test_store_basename("nonexistent");
        let err = upload_output(&mut client, tmp.path(), &basename)
            .await
            .expect_err("should fail NAR serialization");

        // Behavior change: phase2a surfaced NarSerialize (a specific
        // variant); phase2b surfaces UploadExhausted because the NAR
        // error happens inside the retry loop (each retry re-reads disk,
        // each gets the same ENOENT). Same fidelity — error message names
        // the path and the cause.
        assert!(
            matches!(err, UploadError::UploadExhausted { .. }),
            "expected UploadExhausted, got {err:?}"
        );
        Ok(())
    }

    // -----------------------------------------------------------------------
    // HashingChannelWriter — the tee sink
    // -----------------------------------------------------------------------

    /// HashingChannelWriter produces the same SHA-256 as a direct digest.
    /// Runs in spawn_blocking because `blocking_send` panics otherwise.
    #[tokio::test]
    async fn test_hashing_channel_writer_hash_correct() -> anyhow::Result<()> {
        let data = b"hello world, this is tee test data";
        let expected_hash: [u8; 32] = Sha256::digest(data).into();

        let (tx, mut rx) = mpsc::channel(8);
        let data = data.to_vec();
        let (hash, size) = tokio::task::spawn_blocking(move || {
            let mut sink = HashingChannelWriter::new(tx);
            sink.write_all(&data).unwrap();
            sink.finalize()
        })
        .await?;

        assert_eq!(hash, expected_hash, "tee hash should match direct digest");
        assert_eq!(size, 34);

        // Trailer should have arrived.
        let mut saw_trailer = false;
        while let Some(msg) = rx.recv().await {
            if let Some(put_path_request::Msg::Trailer(t)) = msg.msg {
                assert_eq!(t.nar_hash, expected_hash.to_vec());
                assert_eq!(t.nar_size, 34);
                saw_trailer = true;
            }
        }
        assert!(saw_trailer, "trailer should be sent");
        Ok(())
    }

    /// The recorded PutPath call in MockStore has the REAL hash from the
    /// trailer — proves the MockStore trailer-apply logic and the upload
    /// tee produce the same result as phase2a's eager dump+digest.
    #[tokio::test]
    async fn test_upload_streaming_mockstore_has_trailer_hash() -> anyhow::Result<()> {
        let (store, mut client, _h) = spawn_mock_store_with_client().await?;
        let basename = test_store_basename("tee-hash");
        let tmp = make_output_file(&basename, b"tee upload test data")?;

        let result = upload_output(&mut client, tmp.path(), &basename).await?;

        // The hash returned by upload_output == the hash MockStore recorded
        // == SHA-256 of dump_path(). Three-way consistency.
        let expected_nar = nar::dump_path(&tmp.path().join("nix/store").join(&basename))?;
        let expected_hash: [u8; 32] = Sha256::digest(&expected_nar).into();
        assert_eq!(result.nar_hash, expected_hash, "worker's hash");

        let puts = store.put_calls.read().unwrap();
        assert_eq!(puts.len(), 1);
        assert_eq!(
            puts[0].nar_hash,
            expected_hash.to_vec(),
            "MockStore should have the TRAILER's hash, not the empty metadata hash"
        );
        assert_eq!(puts[0].nar_size, expected_nar.len() as u64);
        Ok(())
    }

    /// Write > 256KiB through the tee → multiple chunks produced + correct hash.
    #[tokio::test]
    async fn test_hashing_channel_writer_multi_chunk() -> anyhow::Result<()> {
        // 600 KiB of predictable bytes.
        let data: Vec<u8> = (0..600 * 1024).map(|i| (i % 256) as u8).collect();
        let expected_hash: [u8; 32] = Sha256::digest(&data).into();

        let (tx, mut rx) = mpsc::channel(8);
        let data_owned = data.clone();
        let (hash, size) = tokio::task::spawn_blocking(move || {
            let mut sink = HashingChannelWriter::new(tx);
            sink.write_all(&data_owned).unwrap();
            sink.finalize()
        })
        .await?;

        assert_eq!(hash, expected_hash);
        assert_eq!(size, 600 * 1024);

        // Count chunks + reassemble + verify.
        let mut chunk_count = 0;
        let mut reassembled = Vec::new();
        while let Some(msg) = rx.recv().await {
            match msg.msg {
                Some(put_path_request::Msg::NarChunk(c)) => {
                    chunk_count += 1;
                    reassembled.extend_from_slice(&c);
                }
                Some(put_path_request::Msg::Trailer(_)) => break,
                _ => {}
            }
        }
        assert!(chunk_count >= 2, "600KiB at 256KiB/chunk → ≥2 chunks");
        assert_eq!(reassembled, data, "reassembled chunks should == input");
        Ok(())
    }
}
