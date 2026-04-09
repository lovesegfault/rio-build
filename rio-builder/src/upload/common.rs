//! Mechanics shared by [`single`](super::single) and [`batch`](super::batch):
//! the streaming-tee sink, ref-scan pre-pass, trailer-mode `PathInfo`
//! construction, assignment-token header, and retry/backpressure constants.
//!
//! Everything here is `pub(super)` — the public surface is
//! [`upload_all_outputs`](super::upload_all_outputs).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use rio_common::backoff::{Backoff, Jitter};
use rio_nix::nar;
use rio_nix::refscan::{CandidateSet, RefScanSink};
use rio_nix::store_path::StorePath;
use rio_proto::types::{PathInfo, PutPathRequest, PutPathTrailer, put_path_request};
use rio_proto::validated::ValidatedPathInfo;

use super::UploadError;

/// Maximum number of upload retry attempts. Aligned with the
/// gateway's PutPath retry (`rio-gateway/src/handler/grpc.rs`): both
/// hit the same store-side placeholder contention (I-068/I-125b), so
/// they share curve+budget. 8 attempts × full-jitter ≤~6 s — was 3
/// attempts × no-jitter, which thundering-herded under deep-256x.
pub(super) const MAX_UPLOAD_RETRIES: u32 = 8;

/// PutPath retry curve. See [`MAX_UPLOAD_RETRIES`] for the
/// gateway-alignment rationale.
pub(super) const UPLOAD_BACKOFF: Backoff = Backoff {
    base: Duration::from_millis(50),
    mult: 2.0,
    cap: Duration::from_secs(2),
    jitter: Jitter::Full,
};

/// Maximum concurrent output uploads. Each in-flight upload buffers at
/// most `STREAM_CHANNEL_BUF × 256KiB` (~1MiB); 4 parallel is ~4MiB peak.
/// Disk read bandwidth is the bottleneck; 4 concurrent reads saturate
/// typical NVMe queues.
pub(super) const MAX_PARALLEL_UPLOADS: usize = 4;

/// Channel buffer between the sync `dump_path_streaming` (in spawn_blocking)
/// and the async gRPC send. At 256KiB/chunk, this is 1MiB of backpressure
/// headroom — enough to absorb jitter between disk read and network send
/// without blocking either side for long.
pub(super) const STREAM_CHANNEL_BUF: usize = 4;

/// Construct a `ValidatedPathInfo` for a freshly-uploaded output.
///
/// `references` are full `/nix/store/...` paths from the ref-scan
/// candidate set, which was built from already-validated input-closure
/// paths plus declared output paths — `StorePath::parse` cannot fail on
/// them. A parse failure here is an invariant violation (CandidateSet
/// returned a path it didn't validate) and is surfaced as
/// [`UploadError::InvalidReference`] rather than silently dropped:
/// dropping would corrupt the output's reference graph and break GC
/// reachability. `deriver` may be empty (dev mode), which maps to
/// `None`. Fields not known at upload time (`registration_time`,
/// `signatures`, `content_address`, …) are left default; the store
/// fills them server-side.
pub(super) fn uploaded_info(
    store_path: StorePath,
    nar_hash: [u8; 32],
    nar_size: u64,
    references: Vec<String>,
    deriver: &str,
) -> Result<ValidatedPathInfo, UploadError> {
    let references = references
        .into_iter()
        .map(|r| StorePath::parse(&r).map_err(|_| UploadError::InvalidReference { path: r }))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ValidatedPathInfo {
        store_path,
        store_path_hash: Vec::new(),
        deriver: StorePath::parse(deriver).ok(),
        nar_hash,
        nar_size,
        references,
        registration_time: 0,
        ultimate: false,
        signatures: Vec::new(),
        content_address: None,
    })
}

/// Pre-scan an output for store-path references via `dump_path_streaming`
/// → `RefScanSink`. spawn_blocking because the dump is sync I/O.
///
/// Single extra disk read through `RefScanSink` ONLY (no hash, no
/// network). Done OUTSIDE the upload retry loop so retries don't
/// re-scan — the scan is deterministic. At NVMe speeds a 4 GiB output
/// adds ~4s wall time; the Boyer-Moore skip-scan does ~memcpy speed on
/// binary sections (skips ~31/32 bytes).
///
/// Why a separate pass instead of a three-way tee inside the upload:
/// references go in `PathInfo`, which is the FIRST gRPC message
/// (trailer-mode protocol requires metadata at index 0). We can't know
/// refs until the dump finishes. Changing the proto to send refs in the
/// trailer would ripple into store-side `put_path.rs`, `ValidatedPathInfo`,
/// and the re-sign path — see TODO(P0433) trailer-refs extension.
pub(super) async fn scan_references(
    output_path: &Path,
    candidates: &Arc<CandidateSet>,
) -> Result<Vec<String>, tonic::Status> {
    let scan_path = output_path.to_path_buf();
    let cands = Arc::clone(candidates);
    tokio::task::spawn_blocking(move || {
        let mut sink = RefScanSink::new(cands.hashes());
        nar::dump_path_streaming(&scan_path, &mut sink).map(|_| cands.resolve(&sink.into_found()))
    })
    .await
    .map_err(|e| tonic::Status::internal(format!("ref-scan task panicked: {e}")))?
    .map_err(|e| nar_err_to_status(output_path, e))
}

/// Construct the trailer-mode `PathInfo` (empty hash/size → store
/// expects a `PutPathTrailer` after the chunks). Shared by single and
/// batch metadata-first messages.
// r[impl builder.upload.deriver-populated]
pub(super) fn trailer_mode_path_info(
    store_path: &str,
    deriver: &str,
    references: &[String],
) -> PathInfo {
    PathInfo {
        store_path: store_path.to_string(),
        nar_hash: Vec::new(), // EMPTY → triggers trailer mode on the store
        nar_size: 0,          //         (real values arrive in trailer)
        store_path_hash: Vec::new(),
        deriver: deriver.to_string(),
        references: references.to_vec(),
        registration_time: 0,
        ultimate: false,
        signatures: Vec::new(),
        content_address: String::new(),
    }
}

/// Spawn the streaming dump on the blocking pool. It hashes + forwards
/// chunks + sends trailer, then returns (hash, size). MUST be
/// spawn_blocking: `HashingChannelWriter::write` calls `blocking_send`
/// which PANICS if invoked from a runtime thread.
pub(super) fn spawn_dump_tee(
    output_path: PathBuf,
    tx: mpsc::Sender<PutPathRequest>,
) -> tokio::task::JoinHandle<Result<([u8; 32], u64), tonic::Status>> {
    tokio::task::spawn_blocking(move || {
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
        Ok((hash, size))
    })
}

/// Attach the assignment token as `x-rio-assignment-token` gRPC
/// metadata. Store with `hmac_verifier` set will check it; store
/// without = ignore (the header is just extra metadata). Empty token =
/// no header (scheduler without `hmac_signer`, dev mode).
///
/// `parse()` for `AsciiMetadataValue` — assignment tokens are
/// base64url.base64url, always ASCII. If parse fails (non-ASCII bytes
/// somehow — scheduler bug or memory corruption), the store WILL reject
/// the upload with `PermissionDenied` when `hmac_verifier` is set.
/// Silently omitting the header would turn that into a confusing
/// "rejected, no token" error with no worker-side trace; fail loud
/// instead.
pub(super) fn attach_assignment_token<T>(
    req: &mut tonic::Request<T>,
    assignment_token: &str,
) -> Result<(), tonic::Status> {
    rio_proto::interceptor::inject_current(req.metadata_mut());
    if assignment_token.is_empty() {
        return Ok(());
    }
    match assignment_token.parse() {
        Ok(v) => {
            req.metadata_mut()
                .insert(rio_proto::ASSIGNMENT_TOKEN_HEADER, v);
            Ok(())
        }
        Err(_) => {
            tracing::error!(
                token_len = assignment_token.len(),
                "assignment token failed MetadataValue parse — upload will be rejected"
            );
            Err(tonic::Status::invalid_argument(
                "assignment token is not a valid ASCII metadata value",
            ))
        }
    }
}

pub(super) fn nar_err_to_status(path: &Path, e: nar::NarError) -> tonic::Status {
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
pub(super) struct HashingChannelWriter {
    hasher: Sha256,
    /// Current partial chunk. Flushed to the channel when it reaches
    /// `NAR_CHUNK_SIZE`.
    buf: Vec<u8>,
    /// Total bytes ever written (what goes in the trailer's `nar_size`).
    total: u64,
    tx: mpsc::Sender<PutPathRequest>,
}

impl HashingChannelWriter {
    pub(super) fn new(tx: mpsc::Sender<PutPathRequest>) -> Self {
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
    pub(super) fn finalize(mut self) -> ([u8; 32], u64) {
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
