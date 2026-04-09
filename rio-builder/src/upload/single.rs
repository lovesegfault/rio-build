//! Per-output `PutPath` upload with retry + concurrent-put adoption.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tonic::transport::Channel;
use tracing::instrument;

use rio_common::backoff::{Backoff, Jitter};
use rio_nix::refscan::CandidateSet;
use rio_nix::store_path::StorePath;
use rio_proto::StoreServiceClient;
use rio_proto::types::{PutPathMetadata, PutPathRequest, put_path_request};
use rio_proto::validated::ValidatedPathInfo;

use super::UploadError;
use super::common::{
    MAX_UPLOAD_RETRIES, STREAM_CHANNEL_BUF, UPLOAD_BACKOFF, attach_assignment_token,
    scan_references, spawn_dump_tee, trailer_mode_path_info, uploaded_info,
};

/// I-125b poll curve: 1s, 2s, 4s, 8s, 16s ≈ 31s total. No jitter —
/// only one builder polls per path (the contention is on PutPath, not
/// the poll).
const CONCURRENT_PUT_POLL_BACKOFF: Backoff = Backoff {
    base: Duration::from_secs(1),
    mult: 2.0,
    cap: Duration::from_secs(16),
    jitter: Jitter::None,
};

/// I-125b: on `Aborted: concurrent PutPath`, poll QueryPathInfo this
/// many times (1s, 2s, 4s, 8s, 16s ≈ 31s total) before falling back to
/// a fresh upload attempt. Long enough for the store's drop-path
/// cleanup (I-125a) to release a stale placeholder; short enough that
/// a genuinely stuck store doesn't wedge the builder forever.
pub(super) const CONCURRENT_PUT_POLL_ATTEMPTS: u32 = 5;

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
/// consumed on each attempt; can't rewind them). At [`MAX_UPLOAD_RETRIES`]
/// that's worst-case 8× disk reads. Retries are rare (transient S3/gRPC
/// blips); the extra reads cost seconds on NVMe for a 4GiB file — trivial
/// vs. the 32GiB memory saving.
#[instrument(skip_all, fields(store_path = %format!("/nix/store/{output_basename}")))]
pub(super) async fn upload_output(
    store_client: &mut StoreServiceClient<Channel>,
    upper_store: &Path,
    output_basename: &str,
    assignment_token: &str,
    deriver: &str,
    candidates: Arc<CandidateSet>,
) -> Result<ValidatedPathInfo, UploadError> {
    let output_path = upper_store.join(output_basename);
    let store_path = format!("/nix/store/{output_basename}");

    // Validate the store path ONCE, before the retry loop. A malformed
    // path (overlay setup bug) won't fix itself on retry.
    // do_upload_streaming sends the string form raw; the store
    // re-parses server-side.
    let parsed_path = StorePath::parse(&store_path).map_err(|e| UploadError::UploadExhausted {
        path: store_path.clone(),
        source: tonic::Status::invalid_argument(format!(
            "output store path {store_path:?} from overlay upper is malformed: {e}"
        )),
    })?;

    // --- Pre-scan for references -------------------------------------
    // r[impl builder.upload.references-scanned]
    // Done HERE (outside the retry loop) so retries don't re-scan. See
    // common::scan_references for the rationale (references go in
    // PathInfo which is the FIRST gRPC message, so we can't know refs
    // until the dump finishes).
    //
    // TODO(P0433): trailer-refs protocol extension — move refs into the
    // PutPath trailer so the scan happens inline with the upload tee
    // (avoiding this extra disk pass). Gated on measuring pre-scan cost
    // at scale (see worker.md § pre-scan cost). Deferred P0181 remainder.
    let references = scan_references(&output_path, &candidates)
        .await
        .map_err(|source| UploadError::UploadExhausted {
            path: store_path.clone(),
            source,
        })?;

    tracing::info!(
        store_path = %store_path,
        ref_count = references.len(),
        deriver = %deriver,
        "scanned references; uploading output (streaming tee)"
    );
    metrics::histogram!("rio_builder_upload_references_count").record(references.len() as f64);
    // -----------------------------------------------------------------

    let mut last_error = None;
    for attempt in 0..MAX_UPLOAD_RETRIES {
        if attempt > 0 {
            let delay = UPLOAD_BACKOFF.duration(attempt - 1);
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
                metrics::counter!("rio_builder_uploads_total", "status" => "success").increment(1);
                metrics::counter!("rio_builder_upload_bytes_total").increment(nar_size);
                tracing::info!(
                    store_path = %store_path,
                    nar_size,
                    nar_hash = %hex::encode(nar_hash),
                    "upload complete"
                );
                return uploaded_info(parsed_path, nar_hash, nar_size, references, deriver);
            }
            // r[impl builder.upload.aborted-poll]
            Err(e) if is_concurrent_put_path(&e) => {
                // I-125b: another uploader holds the placeholder for
                // this path. Two non-error outcomes are likely:
                //   (a) the other uploader finishes → path appears in
                //       store. Our content is identical (output path is
                //       drv-addressed), so adopt their result.
                //   (b) the other uploader was a phantom-drained builder
                //       whose connection dropped → store releases the
                //       placeholder (I-125a) within seconds → our next
                //       upload attempt succeeds.
                // Poll QueryPathInfo with backoff; if the path appears,
                // we're done. If not, fall through to the retry loop.
                tracing::info!(
                    store_path = %store_path,
                    attempt,
                    "concurrent PutPath in progress; polling for completion"
                );
                if let Some(info) = wait_for_concurrent_put(store_client, &store_path).await {
                    metrics::counter!("rio_builder_uploads_total", "status" => "adopted")
                        .increment(1);
                    tracing::info!(
                        store_path = %store_path,
                        nar_size = info.nar_size,
                        "concurrent uploader won; adopting store result"
                    );
                    return Ok(info);
                }
                tracing::warn!(
                    store_path = %store_path,
                    attempt,
                    waited_s = approx_poll_total_secs(),
                    "concurrent PutPath did not complete; retrying upload"
                );
                last_error = Some(e);
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

    metrics::counter!("rio_builder_uploads_total", "status" => "exhausted").increment(1);
    Err(UploadError::UploadExhausted {
        path: store_path,
        source: last_error.expect("retry loop ran ≥1 times; each failure sets last_error"),
    })
}

/// I-125b: matches the store's placeholder-contention response. Other
/// `Aborted` reasons (GC mark serialization, admin cancel) keep the
/// plain retry — only this specific message gets the wait-then-adopt
/// treatment. Message substring is stable (rio-store/src/grpc/put_path.rs).
pub(super) fn is_concurrent_put_path(status: &tonic::Status) -> bool {
    status.code() == tonic::Code::Aborted && status.message().contains("concurrent PutPath")
}

/// I-125b: poll [`query_path_info_opt`] with exponential backoff until
/// the path appears or [`CONCURRENT_PUT_POLL_ATTEMPTS`] is exhausted.
/// Returns the store's `PathInfo` if found (caller adopts it as the
/// upload result), `None` if the path never appeared (caller retries
/// the upload — the contending placeholder has likely been released
/// by then via I-125a's drop-path cleanup).
///
/// QueryPathInfo errors are treated as not-found (logged, keep
/// polling) — a transient store blip during the wait shouldn't fail
/// the build.
async fn wait_for_concurrent_put(
    store_client: &mut StoreServiceClient<Channel>,
    store_path: &str,
) -> Option<ValidatedPathInfo> {
    for poll in 0..CONCURRENT_PUT_POLL_ATTEMPTS {
        tokio::time::sleep(CONCURRENT_PUT_POLL_BACKOFF.duration(poll)).await;
        match rio_proto::client::query_path_info_opt(
            store_client,
            store_path,
            rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
            &[],
        )
        .await
        {
            Ok(Some(info)) => return Some(info),
            Ok(None) => {}
            Err(e) => tracing::debug!(
                store_path = %store_path,
                error = %e,
                "QueryPathInfo poll failed; treating as not-yet-present"
            ),
        }
    }
    None
}

/// Approximate total wait time across [`CONCURRENT_PUT_POLL_ATTEMPTS`]
/// polls (geometric sum, for logging). Kept as a fn so the constant
/// stays the single source of truth.
fn approx_poll_total_secs() -> u64 {
    (0..CONCURRENT_PUT_POLL_ATTEMPTS)
        .map(|p| CONCURRENT_PUT_POLL_BACKOFF.duration(p).as_secs())
        .sum()
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
    let info = trailer_mode_path_info(store_path, deriver, references);
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::Metadata(PutPathMetadata {
            info: Some(info),
        })),
    })
    .await
    .map_err(|_| tonic::Status::internal("upload channel closed before metadata send"))?;

    let dump_task = spawn_dump_tee(output_path, tx);

    // Drive the gRPC stream. Chunks + trailer arrive on `rx` as the
    // blocking task produces them. `with_timeout_status` bounds the whole
    // thing (initial call + stream drain) so a stuck disk read can't hang
    // the worker forever.
    let outbound = tokio_stream::wrappers::ReceiverStream::new(rx);
    let mut req = tonic::Request::new(outbound);
    attach_assignment_token(&mut req, assignment_token)?;
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
