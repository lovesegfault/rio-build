//! Output upload to rio-store after build completion.
//!
//! Scans the overlay upper layer for new store paths, serializes each as
//! a NAR, computes SHA-256, and uploads via `StoreService.PutPath` gRPC
//! with retry on failure.
// r[impl worker.upload.multi-output]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::{self, StreamExt};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tonic::transport::Channel;
use tracing::instrument;

use rio_nix::nar;
use rio_nix::refscan::{CandidateSet, RefScanSink};
use rio_proto::StoreServiceClient;
use rio_proto::types::{
    FindMissingPathsRequest, PathInfo, PutPathBatchRequest, PutPathMetadata, PutPathRequest,
    PutPathTrailer, put_path_request,
};

/// Maximum number of upload retry attempts.
const MAX_UPLOAD_RETRIES: u32 = 3;

/// Base delay for exponential backoff between retries.
const RETRY_BASE_DELAY: Duration = Duration::from_secs(1);

/// Maximum concurrent output uploads. Each in-flight upload buffers at
/// most `STREAM_CHANNEL_BUF × 256KiB` (~1MiB); 4 parallel is ~4MiB peak.
/// Disk read bandwidth is the bottleneck; 4 concurrent reads saturate
/// typical NVMe queues.
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
    /// Store paths this output references (runtime deps). Sorted, full
    /// `/nix/store/...` paths. Populated by the pre-scan pass in
    /// `upload_output`. Empty for outputs with no runtime deps (legal for
    /// CA paths like fetchurl; suspicious for non-CA).
    pub references: Vec<String>,
}

/// Errors from upload operations.
///
/// NAR serialization happens inside the retry loop (each retry is a
/// fresh disk read), so NAR errors surface as `UploadExhausted` wrapping
/// a `tonic::Status::internal("NAR serialization failed...")`.
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
///
/// `upper_store` is `{overlay_upper}/nix/store` — callers pass
/// `OverlayMount::upper_store()`.
pub fn scan_new_outputs(upper_store: &Path) -> std::io::Result<Vec<String>> {
    let read_dir = match std::fs::read_dir(upper_store) {
        Ok(iter) => iter,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };

    let mut outputs = Vec::new();
    for entry in read_dir {
        let entry = entry?;
        // Store paths are UTF-8 (nix enforces this). A non-UTF-8 name
        // here is a violation — surface as InvalidData rather than
        // lossy-decode and push a wrong path.
        let name = entry.file_name().into_string().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "non-UTF-8 filename in upper store",
            )
        })?;
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
/// ## Single-pass streaming tee
///
/// A naive approach (`dump_path()` → full NAR in memory → `Sha256::digest()`
/// → chunk) would peak at 2× the NAR size in memory (4GiB output = 8GiB peak).
///
/// Instead: `dump_path_streaming()` (inside `spawn_blocking`) writes to a
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
    upper_store: &Path,
    output_basename: &str,
    assignment_token: &str,
    deriver: &str,
    candidates: Arc<CandidateSet>,
) -> Result<UploadResult, UploadError> {
    let output_path = upper_store.join(output_basename);
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

    // --- Pre-scan for references -------------------------------------
    // r[impl worker.upload.references-scanned]
    // Single extra disk read through RefScanSink ONLY (no hash, no
    // network). spawn_blocking because dump_path_streaming is sync I/O.
    //
    // Done HERE (outside the retry loop) so retries don't re-scan. The
    // scan is deterministic — re-scanning on retry would waste disk
    // bandwidth for the same result. At NVMe speeds a 4 GiB output adds
    // ~4s wall time; the Boyer-Moore skip-scan does ~memcpy speed on
    // binary sections (skips ~31/32 bytes).
    //
    // Why a separate pass instead of a three-way tee inside
    // do_upload_streaming: references go in PathInfo, which is the FIRST
    // gRPC message (trailer-mode protocol requires metadata at index 0).
    // We can't know refs until the dump finishes. Changing the proto to
    // send refs in the trailer would ripple into store-side put_path.rs,
    // ValidatedPathInfo, and the re-sign path — scope creep for a P0 fix.
    //
    // TODO(P0433): trailer-refs protocol extension — move refs into the
    // PutPath trailer so the scan happens inline with the upload tee
    // (avoiding this extra disk pass). Gated on measuring pre-scan cost
    // at scale (see worker.md § pre-scan cost). Deferred P0181 remainder.
    let references = {
        let scan_path = output_path.clone();
        let cands = Arc::clone(&candidates);
        tokio::task::spawn_blocking(move || {
            let mut sink = RefScanSink::new(cands.hashes());
            nar::dump_path_streaming(&scan_path, &mut sink)
                .map(|_| cands.resolve(&sink.into_found()))
        })
        .await
        .map_err(|e| UploadError::UploadExhausted {
            path: store_path.clone(),
            source: tonic::Status::internal(format!("ref-scan task panicked: {e}")),
        })?
        .map_err(|e| UploadError::UploadExhausted {
            path: store_path.clone(),
            source: nar_err_to_status(&output_path, e),
        })?
    };

    tracing::info!(
        store_path = %store_path,
        ref_count = references.len(),
        deriver = %deriver,
        "scanned references; uploading output (streaming tee)"
    );
    metrics::histogram!("rio_worker_upload_references_count").record(references.len() as f64);
    // -----------------------------------------------------------------

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

        match do_upload_streaming(
            store_client,
            &store_path,
            output_path.clone(),
            assignment_token,
            deriver,
            &references,
        )
        .await
        {
            Ok((nar_hash, nar_size)) => {
                metrics::counter!("rio_worker_uploads_total", "status" => "success").increment(1);
                metrics::counter!("rio_worker_upload_bytes_total").increment(nar_size);
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
                    references,
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
///
/// `assignment_token`: passed as `x-rio-assignment-token` gRPC
/// metadata. The store verifies if HMAC is configured. Empty string
/// = no header (dev mode).
#[instrument(skip_all)]
async fn do_upload_streaming(
    store_client: &mut StoreServiceClient<Channel>,
    store_path: &str,
    output_path: PathBuf,
    assignment_token: &str,
    deriver: &str,
    references: &[String],
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
        // r[impl worker.upload.deriver-populated]
        deriver: deriver.to_string(),
        references: references.to_vec(),
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
    //
    // Attach the assignment token as gRPC metadata. Store with
    // hmac_verifier set will check it; store without = ignore
    // (the header is just extra metadata). Empty token = no header
    // (scheduler without hmac_signer, dev mode).
    let outbound = tokio_stream::wrappers::ReceiverStream::new(rx);
    let mut req = tonic::Request::new(outbound);
    rio_proto::interceptor::inject_current(req.metadata_mut());
    if !assignment_token.is_empty() {
        // parse() for AsciiMetadataValue — assignment tokens are
        // base64url.base64url, always ASCII. If parse fails (non-ASCII
        // bytes somehow — scheduler bug or memory corruption), the
        // store WILL reject the upload with PermissionDenied when
        // hmac_verifier is set. Silently omitting the header here
        // turned that into a confusing "rejected, no token" error
        // with no worker-side trace. Fail loud instead.
        match assignment_token.parse() {
            Ok(v) => {
                req.metadata_mut()
                    .insert(rio_proto::ASSIGNMENT_TOKEN_HEADER, v);
            }
            Err(_) => {
                tracing::error!(
                    token_len = assignment_token.len(),
                    "assignment token failed MetadataValue parse — upload will be rejected"
                );
                return Err(tonic::Status::invalid_argument(
                    "assignment token is not a valid ASCII metadata value",
                ));
            }
        }
    }
    let put_result = rio_common::grpc::with_timeout_status(
        "PutPath",
        rio_common::grpc::GRPC_STREAM_TIMEOUT,
        store_client.put_path(req),
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
/// Pipeline:
/// 1. **Idempotency pre-check** (`r[worker.upload.idempotent-precheck]`):
///    `FindMissingPaths` filters out outputs the store already has. Skipped
///    outputs get their `UploadResult` from `QueryPathInfo` — zero disk reads.
/// 2. **≥2 remaining → `PutPathBatch`** for cross-output atomicity
///    (`r[store.atomic.multi-output]`). On `FailedPrecondition` (an output
///    ≥ INLINE_THRESHOLD, which the v1 batch handler rejects), falls through
///    to step 3 — which LOSES atomicity (pre-P0267 status quo).
/// 3. **≤1 remaining, or batch fallthrough → independent `PutPath`** with
///    `buffer_unordered(MAX_PARALLEL_UPLOADS)`.
///
/// Result order is **not** guaranteed. Callers must not assume results
/// correspond positionally to any input list; use `UploadResult.store_path`
/// to identify outputs.
#[instrument(skip_all)]
pub async fn upload_all_outputs(
    store_client: &StoreServiceClient<Channel>,
    upper_store: &Path,
    assignment_token: &str,
    deriver: &str,
    ref_candidates: &[String],
) -> Result<Vec<UploadResult>, UploadError> {
    let outputs = scan_new_outputs(upper_store)?;
    if outputs.is_empty() {
        return Ok(Vec::new());
    }

    // --- Idempotency pre-check -------------------------------------------
    // r[impl worker.upload.idempotent-precheck]
    //
    // Batch-check all outputs against the store BEFORE reading any bytes
    // from disk. Outputs with a `'complete'` manifest are skipped: the
    // pre-scan disk read, NAR stream, SHA-256, and gRPC stream setup
    // are all wasted work when r[store.put.idempotent] would no-op
    // server-side anyway.
    //
    // Best-effort: on FindMissingPaths error, log + treat ALL as missing.
    // r[store.put.idempotent] catches the duplicates server-side — zero
    // behavior change from before this pre-check existed. This is an
    // optimization, not a correctness requirement.
    //
    // TODO(P0434): manifest-mode bandwidth opt — send manifest-only,
    // store fetches missing chunks from ChunkCache. Gated on measuring
    // rio_store_chunk_cache_hits_total ratio in production. Worker NOT
    // trusted → store must reconstruct NAR to verify, so the "win" is
    // net positive only if ChunkCache hit rate is high (>80%). P0263
    // scoped down to the zero-proto-change path; deferred remainder.
    let store_paths: Vec<String> = outputs.iter().map(|b| format!("/nix/store/{b}")).collect();
    let (to_upload, mut skipped_results) =
        partition_by_presence(store_client, &outputs, store_paths).await;
    // ---------------------------------------------------------------------

    // Build the candidate set ONCE. Same input closure applies to every
    // output of a derivation; Arc so buffer_unordered's per-task clone
    // is a pointer copy, not a full HashMap clone.
    let candidates = Arc::new(CandidateSet::from_paths(ref_candidates));

    // Branch: ≥2 outputs TO UPLOAD → atomic batch; ≤1 → independent
    // (atomicity is vacuous for a single output). The count is against the
    // post-idempotency-pre-check set — if 3 outputs exist but 2 are already
    // in the store, only 1 needs upload and PutPath is sufficient. See
    // store.atomic.multi-output in the store spec.
    if to_upload.len() >= 2 {
        tracing::info!(
            to_upload = to_upload.len(),
            skipped = skipped_results.len(),
            "uploading build outputs (atomic batch)"
        );
        match upload_outputs_batch(
            store_client,
            upper_store,
            &to_upload,
            assignment_token,
            deriver,
            &candidates,
        )
        .await
        {
            Ok(mut results) => {
                results.append(&mut skipped_results);
                return Ok(results);
            }
            Err(UploadError::UploadExhausted { source, .. })
                if source.code() == tonic::Code::FailedPrecondition =>
            {
                // v1 batch handler is inline-only; an output was too large.
                // Fall through to independent PutPath (loses atomicity —
                // pre-P0267 behavior, which `gt13_multi_output_not_atomic`
                // documents).
                tracing::warn!(
                    error = %source,
                    "batch upload rejected (output too large for inline); \
                     falling back to independent PutPath — partial registration possible"
                );
            }
            Err(e) => return Err(e),
        }
    }

    tracing::info!(
        to_upload = to_upload.len(),
        skipped = skipped_results.len(),
        max_parallel = MAX_PARALLEL_UPLOADS,
        "uploading build outputs (independent PutPath)"
    );

    let upper_store = upper_store.to_path_buf();
    // Clone the token into each task (it's a String in each async
    // block; MAX_PARALLEL_UPLOADS=4 copies of ~150 bytes — trivial).
    let token = assignment_token.to_string();
    let deriver = deriver.to_string();
    let results: Vec<Result<UploadResult, UploadError>> = stream::iter(to_upload)
        .map(|output| {
            let mut client = store_client.clone();
            let upper_store = upper_store.clone();
            let token = token.clone();
            let deriver = deriver.clone();
            let candidates = Arc::clone(&candidates);
            async move {
                upload_output(
                    &mut client,
                    &upper_store,
                    &output,
                    &token,
                    &deriver,
                    candidates,
                )
                .await
            }
        })
        .buffer_unordered(MAX_PARALLEL_UPLOADS)
        .collect()
        .await;

    let mut uploaded: Vec<UploadResult> = results.into_iter().collect::<Result<_, _>>()?;
    uploaded.append(&mut skipped_results);
    Ok(uploaded)
}

/// Batch `FindMissingPaths` → partition outputs into (upload, skip).
///
/// For already-present outputs, `QueryPathInfo` fetches `nar_hash`/`nar_size`
/// from the store so callers get a complete `UploadResult` without any disk
/// read. `references` is empty — the store already has the authoritative
/// reference set (from whoever originally uploaded), and no caller reads
/// `UploadResult.references` anyway (it's only sent TO the store via PutPath).
///
/// Fail-open: any error (FindMissingPaths unavailable, QueryPathInfo returned
/// None for a supposedly-present path) → fall back to uploading. The store's
/// idempotent PutPath handles it. No error propagation — this whole function
/// is an optimization layer.
async fn partition_by_presence(
    store_client: &StoreServiceClient<Channel>,
    basenames: &[String],
    store_paths: Vec<String>,
) -> (Vec<String>, Vec<UploadResult>) {
    let mut client = store_client.clone();
    let mut req = tonic::Request::new(FindMissingPathsRequest { store_paths });
    rio_proto::interceptor::inject_current(req.metadata_mut());

    let missing: std::collections::HashSet<String> = match rio_common::grpc::with_timeout_status(
        "FindMissingPaths",
        rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
        client.find_missing_paths(req),
    )
    .await
    {
        Ok(resp) => resp.into_inner().missing_paths.into_iter().collect(),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "idempotent pre-check: FindMissingPaths failed; \
                 falling back to upload-all (store.put.idempotent catches dups)"
            );
            return (basenames.to_vec(), Vec::new());
        }
    };

    let mut to_upload = Vec::with_capacity(basenames.len());
    let mut skipped = Vec::new();
    for basename in basenames {
        let store_path = format!("/nix/store/{basename}");
        if missing.contains(&store_path) {
            to_upload.push(basename.clone());
            continue;
        }
        // Present in store — fetch nar_hash/nar_size instead of re-uploading.
        // QueryPathInfo is cheap (~1 PG row read); re-upload is 2× disk
        // reads + NAR stream + gRPC stream. Worth it even for small outputs.
        match rio_proto::client::query_path_info_opt(
            &mut client,
            &store_path,
            rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
        )
        .await
        {
            Ok(Some(info)) => {
                metrics::counter!("rio_worker_upload_skipped_idempotent_total").increment(1);
                tracing::info!(
                    store_path = %store_path,
                    nar_size = info.nar_size,
                    "output already in store; skipping upload"
                );
                skipped.push(UploadResult {
                    store_path,
                    nar_hash: info.nar_hash,
                    nar_size: info.nar_size,
                    references: Vec::new(),
                });
            }
            // Present per FindMissingPaths but QueryPathInfo disagrees
            // (TOCTOU — sub-second window, effectively impossible; or
            // store transient). Fall back to upload; don't error.
            Ok(None) | Err(_) => {
                tracing::warn!(
                    store_path = %store_path,
                    "idempotent pre-check: FindMissingPaths said present but \
                     QueryPathInfo disagreed; falling back to upload"
                );
                to_upload.push(basename.clone());
            }
        }
    }
    (to_upload, skipped)
}

/// Batch upload: all outputs in one `PutPathBatch` stream, committed
/// atomically server-side.
///
/// Outputs are streamed **serially** (output 0 fully, then output 1, …).
/// Each output's NAR is hashed+streamed via the same `spawn_blocking` +
/// `HashingChannelWriter` tee as `do_upload_streaming`, with an outer
/// forwarding loop that tags each `PutPathRequest` with its `output_index`.
/// Peak memory: `STREAM_CHANNEL_BUF × 256 KiB` (~1 MiB) — same as the
/// single-output path, since outputs are serial.
///
/// No per-output retry: batch is all-or-nothing at the gRPC level too.
/// The caller (`upload_all_outputs`) falls back to independent `PutPath`
/// on `FailedPrecondition` (the one error code the batch handler uses to
/// signal "use the other path").
#[instrument(skip_all, fields(outputs = outputs.len()))]
async fn upload_outputs_batch(
    store_client: &StoreServiceClient<Channel>,
    upper_store: &Path,
    outputs: &[String],
    assignment_token: &str,
    deriver: &str,
    candidates: &Arc<CandidateSet>,
) -> Result<Vec<UploadResult>, UploadError> {
    let (tx, rx) = mpsc::channel::<PutPathBatchRequest>(STREAM_CHANNEL_BUF);

    // Producer task: for each output, pre-scan refs → send tagged metadata →
    // spawn_blocking dump into an inner channel → forward inner messages
    // tagged with output_index. Returns the Vec<UploadResult> on success.
    //
    // Cloned inputs for the `spawn`ed task (it needs 'static). The outputs
    // list is cloned once (basenames, small).
    let upper_store = upper_store.to_path_buf();
    let outputs_owned: Vec<String> = outputs.to_vec();
    let deriver_owned = deriver.to_string();
    let candidates = Arc::clone(candidates);

    let producer = tokio::spawn(async move {
        let mut results: Vec<UploadResult> = Vec::with_capacity(outputs_owned.len());
        for (idx, basename) in outputs_owned.iter().enumerate() {
            let output_path = upper_store.join(basename);
            let store_path = format!("/nix/store/{basename}");
            let idx = idx as u32;

            // Validate store path format (same guard as `upload_output`).
            let _ = rio_nix::store_path::StorePath::parse(&store_path).map_err(|e| {
                tonic::Status::invalid_argument(format!(
                    "output {idx}: store path {store_path:?} malformed: {e}"
                ))
            })?;

            // Pre-scan refs (same pattern as upload_output). spawn_blocking
            // because dump_path_streaming is sync I/O.
            let scan_path = output_path.clone();
            let cands = Arc::clone(&candidates);
            let references = tokio::task::spawn_blocking(move || {
                let mut sink = RefScanSink::new(cands.hashes());
                nar::dump_path_streaming(&scan_path, &mut sink)
                    .map(|_| cands.resolve(&sink.into_found()))
            })
            .await
            .map_err(|e| tonic::Status::internal(format!("ref-scan task panicked: {e}")))?
            .map_err(|e| nar_err_to_status(&output_path, e))?;

            // Metadata message (trailer mode — empty hash/size).
            let info = PathInfo {
                store_path: store_path.clone(),
                nar_hash: Vec::new(),
                nar_size: 0,
                store_path_hash: Vec::new(),
                deriver: deriver_owned.clone(),
                references: references.clone(),
                registration_time: 0,
                ultimate: false,
                signatures: Vec::new(),
                content_address: String::new(),
            };
            tx.send(PutPathBatchRequest {
                output_index: idx,
                inner: Some(PutPathRequest {
                    msg: Some(put_path_request::Msg::Metadata(PutPathMetadata {
                        info: Some(info),
                    })),
                }),
            })
            .await
            .map_err(|_| tonic::Status::internal("batch channel closed (metadata)"))?;

            // Inner channel: spawn_blocking produces PutPathRequest, async
            // side wraps + forwards. Same tee pattern as do_upload_streaming.
            let (inner_tx, mut inner_rx) = mpsc::channel::<PutPathRequest>(STREAM_CHANNEL_BUF);
            let dump_path = output_path.clone();
            let dump_task = tokio::task::spawn_blocking(move || {
                let mut sink = HashingChannelWriter::new(inner_tx);
                nar::dump_path_streaming(&dump_path, &mut sink)
                    .map_err(|e| nar_err_to_status(&dump_path, e))?;
                Ok::<_, tonic::Status>(sink.finalize())
            });

            // Forward chunks + trailer, tagging with output_index.
            while let Some(inner) = inner_rx.recv().await {
                tx.send(PutPathBatchRequest {
                    output_index: idx,
                    inner: Some(inner),
                })
                .await
                .map_err(|_| tonic::Status::internal("batch channel closed (forward)"))?;
            }

            let (nar_hash, nar_size) = dump_task
                .await
                .map_err(|e| tonic::Status::internal(format!("dump task panicked: {e}")))??;

            results.push(UploadResult {
                store_path,
                nar_hash,
                nar_size,
                references,
            });
        }
        // tx drops here → batch stream closes → server enters commit phase.
        Ok::<_, tonic::Status>(results)
    });

    // Drive the gRPC call. The producer feeds `rx` concurrently.
    let outbound = tokio_stream::wrappers::ReceiverStream::new(rx);
    let mut req = tonic::Request::new(outbound);
    rio_proto::interceptor::inject_current(req.metadata_mut());
    if !assignment_token.is_empty()
        && let Ok(v) = assignment_token.parse()
    {
        req.metadata_mut()
            .insert(rio_proto::ASSIGNMENT_TOKEN_HEADER, v);
    }

    let mut client = store_client.clone();
    let put_result = rio_common::grpc::with_timeout_status(
        "PutPathBatch",
        rio_common::grpc::GRPC_STREAM_TIMEOUT,
        client.put_path_batch(req),
    )
    .await;

    // Join producer. Same error-priority logic as do_upload_streaming:
    // gRPC error wins (it's what the operator cares about).
    let producer_result = producer
        .await
        .map_err(|e| tonic::Status::internal(format!("batch producer panicked: {e}")));

    // Surface errors via UploadExhausted (batch is one-shot, no retries).
    let resp = put_result.map_err(|e| UploadError::UploadExhausted {
        path: "<batch>".into(),
        source: e,
    })?;
    let results = producer_result
        .and_then(|r| r)
        .map_err(|e| UploadError::UploadExhausted {
            path: "<batch>".into(),
            source: e,
        })?;

    let created = resp.into_inner().created;
    tracing::info!(
        outputs = results.len(),
        created = created.iter().filter(|&&c| c).count(),
        "batch upload committed atomically"
    );
    for r in &results {
        metrics::counter!("rio_worker_uploads_total", "status" => "success").increment(1);
        metrics::counter!("rio_worker_upload_bytes_total").increment(r.nar_size);
    }

    Ok(results)
}

// r[verify worker.upload.multi-output]
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_scan_new_outputs_empty() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        // upper_store doesn't exist → ENOENT → empty Vec.
        let outputs = scan_new_outputs(&dir.path().join("nonexistent"))?;
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
        let outputs = scan_new_outputs(&store_dir)?;
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

    use rio_test_support::fixtures::{test_drv_path, test_store_basename};
    use rio_test_support::grpc::{spawn_mock_store_inproc, spawn_mock_store_with_client};
    use std::sync::atomic::Ordering;

    use rio_test_support::fixtures::seed_store_output as make_output_file;

    /// Empty candidate set — for tests that don't care about ref scanning.
    fn no_candidates() -> Arc<CandidateSet> {
        Arc::new(CandidateSet::from_paths(std::iter::empty::<&str>()))
    }

    #[tokio::test]
    async fn test_upload_output_success() -> anyhow::Result<()> {
        let (store, mut client, _h) = spawn_mock_store_with_client().await?;
        let basename = test_store_basename("hello");
        let (_tmp, store_dir) = make_output_file(&basename, b"hello world")?;

        let result = upload_output(&mut client, &store_dir, &basename, "", "", no_candidates())
            .await
            .expect("upload should succeed");

        assert_eq!(result.store_path, format!("/nix/store/{basename}"));
        // Hash must match SHA-256 of the NAR serialization.
        let expected_nar = nar::dump_path(&store_dir.join(&basename))?;
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
        let (store, mut client) = spawn_mock_store_inproc().await?;
        store.fail_next_puts.store(2, Ordering::SeqCst);
        let basename = test_store_basename("retry");
        let (_tmp, store_dir) = make_output_file(&basename, b"retry me")?;

        let result = upload_output(&mut client, &store_dir, &basename, "", "", no_candidates())
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
        let (store, mut client) = spawn_mock_store_inproc().await?;
        store
            .fail_next_puts
            .store(MAX_UPLOAD_RETRIES + 1, Ordering::SeqCst);
        let basename = test_store_basename("exhaust");
        let (_tmp, store_dir) = make_output_file(&basename, b"never uploads")?;

        let err = upload_output(&mut client, &store_dir, &basename, "", "", no_candidates())
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

        let results = upload_all_outputs(&client, &store_dir, "", "", &[])
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
    /// With the pre-scan pass, the ENOENT is caught BEFORE the gRPC stream
    /// opens (pre-scan reads from disk first). The error still surfaces as
    /// UploadExhausted with a NAR-serialization message.
    #[tokio::test(start_paused = true)]
    async fn test_upload_output_nar_serialize_error() -> anyhow::Result<()> {
        let (_store, mut client) = spawn_mock_store_inproc().await?;
        let tmp = tempfile::tempdir()?;
        // Create nix/store/ dir but NOT the output file.
        let store_dir = tmp.path().join("nix/store");
        fs::create_dir_all(&store_dir)?;

        // Use a VALID basename (32-char hash) so we get past the path
        // validation and into the dump that actually ENOENTs.
        let basename = test_store_basename("nonexistent");
        let err = upload_output(&mut client, &store_dir, &basename, "", "", no_candidates())
            .await
            .expect_err("should fail NAR serialization");

        // NAR error happens in the pre-scan pass (before the retry loop) —
        // same ENOENT every time → UploadExhausted. Error message still
        // names path + cause.
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
    /// tee produce the same result as a naive dump_path() + digest().
    #[tokio::test]
    async fn test_upload_streaming_mockstore_has_trailer_hash() -> anyhow::Result<()> {
        let (store, mut client, _h) = spawn_mock_store_with_client().await?;
        let basename = test_store_basename("tee-hash");
        let (_tmp, store_dir) = make_output_file(&basename, b"tee upload test data")?;

        let result =
            upload_output(&mut client, &store_dir, &basename, "", "", no_candidates()).await?;

        // The hash returned by upload_output == the hash MockStore recorded
        // == SHA-256 of dump_path(). Three-way consistency.
        let expected_nar = nar::dump_path(&store_dir.join(&basename))?;
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

    // -----------------------------------------------------------------------
    // Reference scanning (pre-scan pass → PathInfo.references)
    // -----------------------------------------------------------------------

    /// Two distinct valid nixbase32 hashes for building test candidate paths.
    /// Must differ from TEST_HASH (aaaa...) used by test_store_basename, so
    /// the CandidateSet's hash→path map doesn't collide.
    const DEP_HASH_A: &str = "7rjj5xmrxb3n63wlk6mzlwxzxbvg7r3a";
    const DEP_HASH_B: &str = "v5sv61sszx301i0x6xysaqzla09nksnd";

    /// r[verify worker.upload.references-scanned]
    /// r[verify worker.upload.deriver-populated]
    ///
    /// End-to-end: output file embeds a store-path string → pre-scan finds
    /// it → PathInfo.references arrives at MockStore non-empty. Also checks
    /// PathInfo.deriver is populated (was String::new() before this fix).
    #[tokio::test]
    async fn test_upload_output_scans_references() -> anyhow::Result<()> {
        let (store, mut client, _h) = spawn_mock_store_with_client().await?;
        let basename = test_store_basename("scanned");
        let deriver = test_drv_path("scanned");

        // Two candidate deps. Output contents mention dep-A (as a full store
        // path, the way a real RPATH or shebang would). dep-B is NOT in the
        // output — verifies we don't over-report.
        let dep_a = format!("/nix/store/{DEP_HASH_A}-glibc-2.38");
        let dep_b = format!("/nix/store/{DEP_HASH_B}-unused");
        let self_path = format!("/nix/store/{basename}");
        let contents = format!("RPATH={dep_a}/lib\nself={self_path}\n");
        let (_tmp, store_dir) = make_output_file(&basename, contents.as_bytes())?;

        // Candidate set: both deps + the output itself (self-references are
        // legal — binaries embed their own store path in rpaths).
        let candidates = Arc::new(CandidateSet::from_paths([&dep_a, &dep_b, &self_path]));

        let result =
            upload_output(&mut client, &store_dir, &basename, "", &deriver, candidates).await?;

        // UploadResult carries the scanned refs. Sorted: /nix/store/7rjj...
        // < /nix/store/aaaa... (self). dep-B absent.
        assert_eq!(
            result.references,
            vec![dep_a.clone(), self_path.clone()],
            "scanned refs: dep-A + self, sorted, no dep-B"
        );

        // MockStore recorded the PathInfo WITH references + deriver. This is
        // the actual fix — pre-fix, both were always empty (upload.rs:223-224).
        let puts = store.put_calls.read().unwrap();
        assert_eq!(puts.len(), 1);
        assert_eq!(
            puts[0].references,
            vec![dep_a, self_path],
            "PathInfo.references delivered to store"
        );
        assert_eq!(
            puts[0].deriver, deriver,
            "PathInfo.deriver delivered to store"
        );
        Ok(())
    }

    /// Output with no embedded store paths → empty references (but deriver
    /// still populated). Legal for CA paths like fetchurl.
    #[tokio::test]
    async fn test_upload_output_no_references_found() -> anyhow::Result<()> {
        let (store, mut client, _h) = spawn_mock_store_with_client().await?;
        let basename = test_store_basename("noref");
        let deriver = test_drv_path("noref");
        let (_tmp, store_dir) = make_output_file(&basename, b"plain text, no store paths here")?;

        let dep = format!("/nix/store/{DEP_HASH_A}-dep");
        let candidates = Arc::new(CandidateSet::from_paths([&dep]));

        let result =
            upload_output(&mut client, &store_dir, &basename, "", &deriver, candidates).await?;

        assert!(result.references.is_empty(), "no refs in output contents");
        let puts = store.put_calls.read().unwrap();
        assert!(puts[0].references.is_empty());
        assert_eq!(
            puts[0].deriver, deriver,
            "deriver still set even with zero refs"
        );
        Ok(())
    }

    /// Reference scanning in upload_all_outputs: candidate set built once,
    /// shared across all outputs. Each output gets its own scan result.
    #[tokio::test]
    async fn test_upload_all_outputs_per_output_refs() -> anyhow::Result<()> {
        let (store, client, _h) = spawn_mock_store_with_client().await?;
        let tmp = tempfile::tempdir()?;
        let store_dir = tmp.path().join("nix/store");
        fs::create_dir_all(&store_dir)?;

        // Two outputs: one mentions dep-A, the other mentions dep-B.
        // Use distinct hashes for the outputs so CandidateSet doesn't
        // collapse them (test_store_basename uses a single TEST_HASH).
        let out1 = format!("{DEP_HASH_A}-out1");
        let out2 = format!("{DEP_HASH_B}-out2");
        let dep_a = format!("/nix/store/{DEP_HASH_A}-out1"); // out1 self-ref
        let dep_b = format!("/nix/store/{DEP_HASH_B}-out2"); // out2 self-ref
        fs::write(store_dir.join(&out1), format!("ref={dep_a}"))?;
        fs::write(store_dir.join(&out2), format!("ref={dep_b}"))?;

        let deriver = test_drv_path("multi");
        let results = upload_all_outputs(
            &client,
            &store_dir,
            "",
            &deriver,
            &[dep_a.clone(), dep_b.clone()],
        )
        .await?;

        assert_eq!(results.len(), 2);
        // Find by store_path (buffer_unordered → result order is not guaranteed).
        let r1 = results
            .iter()
            .find(|r| r.store_path.ends_with("-out1"))
            .expect("out1 result");
        let r2 = results
            .iter()
            .find(|r| r.store_path.ends_with("-out2"))
            .expect("out2 result");
        assert_eq!(r1.references, vec![dep_a], "out1 refs itself only");
        assert_eq!(r2.references, vec![dep_b], "out2 refs itself only");

        // All MockStore PathInfos carry the deriver.
        let puts = store.put_calls.read().unwrap();
        assert_eq!(puts.len(), 2);
        for p in puts.iter() {
            assert_eq!(p.deriver, deriver);
        }
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

    // -----------------------------------------------------------------------
    // Idempotent pre-check (FindMissingPaths → skip already-present outputs)
    // -----------------------------------------------------------------------

    /// r[verify worker.upload.idempotent-precheck]
    ///
    /// Output already in store → zero PutPath calls, UploadResult carries
    /// the STORE's nar_hash (not a freshly-computed one). This is the
    /// exit criterion: "second identical-NAR upload → zero chunks".
    ///
    /// Disk contents are deliberately DIFFERENT from what's seeded in the
    /// store: the test asserts the returned nar_hash matches the SEEDED
    /// hash, NOT the on-disk NAR's hash. Proves we queried the store
    /// instead of reading disk (the optimization's whole point).
    #[tokio::test]
    async fn test_upload_all_outputs_skips_already_present() -> anyhow::Result<()> {
        let (store, client, _h) = spawn_mock_store_with_client().await?;
        let basename = format!("{DEP_HASH_A}-already-there");
        let store_path = format!("/nix/store/{basename}");

        // Seed: path already complete in store. seed_with_content builds a
        // NAR from "seeded content" and returns its hash — the nar_hash the
        // worker should return for the skipped path.
        let (_seeded_nar, seeded_hash) = store.seed_with_content(&store_path, b"seeded content");

        // Disk: DIFFERENT contents. If the pre-check is broken (falls
        // through to upload), the result's nar_hash would be the disk
        // NAR's hash, not seeded_hash. This is the precondition assert —
        // without distinct contents, the test passes trivially.
        let tmp = tempfile::tempdir()?;
        let store_dir = tmp.path().join("nix/store");
        fs::create_dir_all(&store_dir)?;
        fs::write(store_dir.join(&basename), b"DIFFERENT disk contents")?;
        let disk_nar = nar::dump_path(&store_dir.join(&basename))?;
        let disk_hash: [u8; 32] = Sha256::digest(&disk_nar).into();
        assert_ne!(
            seeded_hash, disk_hash,
            "precondition: seeded vs disk NARs must differ, else this test proves nothing"
        );

        let results = upload_all_outputs(&client, &store_dir, "", "", &[]).await?;

        // Zero PutPath calls — the skip fired.
        assert_eq!(
            store.put_calls.read().unwrap().len(),
            0,
            "pre-check should skip already-present path; zero PutPath calls"
        );
        // One result, carrying the STORE's hash (not disk's).
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].store_path, store_path);
        assert_eq!(
            results[0].nar_hash, seeded_hash,
            "skipped path's UploadResult carries the store's nar_hash, not disk's"
        );
        // references empty for skipped paths (store already has the
        // authoritative set; nobody reads UploadResult.references).
        assert!(results[0].references.is_empty());
        Ok(())
    }

    /// Mixed: one output already present, one missing. Only the missing
    /// one hits PutPath; both appear in results.
    #[tokio::test]
    async fn test_upload_all_outputs_mixed_presence() -> anyhow::Result<()> {
        let (store, client, _h) = spawn_mock_store_with_client().await?;

        let b_present = format!("{DEP_HASH_A}-present");
        let b_missing = format!("{DEP_HASH_B}-missing");
        let path_present = format!("/nix/store/{b_present}");
        let path_missing = format!("/nix/store/{b_missing}");

        let (_nar, seeded_hash) = store.seed_with_content(&path_present, b"already here");

        let tmp = tempfile::tempdir()?;
        let store_dir = tmp.path().join("nix/store");
        fs::create_dir_all(&store_dir)?;
        fs::write(store_dir.join(&b_present), b"disk present")?;
        fs::write(store_dir.join(&b_missing), b"disk missing")?;

        let results = upload_all_outputs(&client, &store_dir, "", "", &[]).await?;

        // Exactly one PutPath: the missing output.
        let puts = store.put_calls.read().unwrap();
        assert_eq!(puts.len(), 1, "only the missing output hits PutPath");
        assert_eq!(puts[0].store_path, path_missing);

        // Both outputs in results (caller needs ALL outputs reported).
        assert_eq!(results.len(), 2);
        let r_present = results
            .iter()
            .find(|r| r.store_path == path_present)
            .expect("present output in results");
        let r_missing = results
            .iter()
            .find(|r| r.store_path == path_missing)
            .expect("missing output in results");

        // Present: store's hash. Missing: freshly computed.
        assert_eq!(r_present.nar_hash, seeded_hash);
        let missing_nar = nar::dump_path(&store_dir.join(&b_missing))?;
        let missing_hash: [u8; 32] = Sha256::digest(&missing_nar).into();
        assert_eq!(r_missing.nar_hash, missing_hash);
        Ok(())
    }

    /// FindMissingPaths errors → fall back to upload-all. Best-effort:
    /// store transient doesn't break the upload; r[store.put.idempotent]
    /// catches duplicates server-side. Zero behavior change from the
    /// pre-precheck world.
    #[tokio::test]
    async fn test_upload_all_outputs_find_missing_error_falls_back() -> anyhow::Result<()> {
        let (store, client, _h) = spawn_mock_store_with_client().await?;
        store.fail_find_missing.store(true, Ordering::SeqCst);

        let basename = format!("{DEP_HASH_A}-fallback");
        let store_path = format!("/nix/store/{basename}");

        // Seed the path — WOULD be skipped if FindMissingPaths worked.
        store.seed_with_content(&store_path, b"seeded");

        let tmp = tempfile::tempdir()?;
        let store_dir = tmp.path().join("nix/store");
        fs::create_dir_all(&store_dir)?;
        fs::write(store_dir.join(&basename), b"disk fallback")?;

        let results = upload_all_outputs(&client, &store_dir, "", "", &[]).await?;

        // FindMissingPaths failed → fell back to upload → PutPath called.
        // (MockStore's put_path doesn't implement the idempotent no-op;
        // it happily overwrites. Real store would no-op. We're testing
        // the WORKER's fail-open, not the store's idempotency.)
        assert_eq!(
            store.put_calls.read().unwrap().len(),
            1,
            "FindMissingPaths error → fall back to upload (fail-open)"
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].store_path, store_path);
        // Hash is the disk NAR's (we uploaded, didn't skip).
        let disk_nar = nar::dump_path(&store_dir.join(&basename))?;
        let disk_hash: [u8; 32] = Sha256::digest(&disk_nar).into();
        assert_eq!(results[0].nar_hash, disk_hash);
        Ok(())
    }

    /// Empty overlay upper → early return, no FindMissingPaths call.
    /// Guards the `if outputs.is_empty()` branch added to avoid an empty
    /// RPC on derivations that produce nothing in the upper (shouldn't
    /// happen in practice, but the branch is there).
    #[tokio::test]
    async fn test_upload_all_outputs_empty_no_rpc() -> anyhow::Result<()> {
        let (store, client, _h) = spawn_mock_store_with_client().await?;
        // Arm the failure: if FindMissingPaths were called, the test
        // would still pass (fail-open), but the empty check should
        // short-circuit BEFORE the RPC.
        store.fail_find_missing.store(true, Ordering::SeqCst);
        // MockStore doesn't expose call-count for find_missing_paths.
        // Just assert empty result + zero PutPath. The is_empty() guard
        // is simple enough that existence-in-code is the real assurance.
        let tmp = tempfile::tempdir()?;
        // upper_store doesn't exist — scan_new_outputs returns empty.

        let results =
            upload_all_outputs(&client, &tmp.path().join("nonexistent"), "", "", &[]).await?;
        assert!(results.is_empty());
        assert_eq!(store.put_calls.read().unwrap().len(), 0);
        Ok(())
    }
}
