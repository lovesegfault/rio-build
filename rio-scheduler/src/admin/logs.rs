//! `AdminService.GetBuildLogs` implementation.
//!
//! Two data sources (per `observability.md:44-50`):
//!
//! | Build State | Source |
//! |---|---|
//! | Active | Ring buffer (in-memory, most recent) |
//! | Completed | S3 (gzipped blob, seekable via PG `build_logs.s3_key`) |
//!
//! We check the ring buffer FIRST: if the derivation is still active,
//! the ring buffer has the freshest lines (the S3 blob, if any, is a
//! 30s-stale periodic snapshot). Only if the ring buffer is empty do
//! we fall back to S3 — which means the derivation finished and the
//! flusher drained it.

use std::io::Read;

use aws_sdk_s3::Client as S3Client;
use flate2::read::GzDecoder;
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Status;

use rio_common::grpc::StatusExt;
use tracing::debug;

use rio_proto::types::{BuildLogChunk, GetBuildLogsRequest};

use crate::logs::LogBuffers;

/// Full `GetBuildLogs` handler body: validate → ring buffer → S3.
///
/// `try_ring_buffer` and `try_s3` are separately testable; this
/// function just sequences them with the right fallback logic.
///
/// Errors are yielded IN-STREAM via [`err_stream`] rather than returned
/// as `Err(Status)` — returning `Err` from a server-streaming handler
/// makes tonic emit Trailers-Only, which the grpc-web dashboard can't
/// read (browser fetch API can't access HTTP trailers).
pub(super) async fn get_build_logs(
    log_buffers: &LogBuffers,
    s3: &Option<(S3Client, String)>,
    pool: &PgPool,
    req: GetBuildLogsRequest,
) -> ReceiverStream<Result<BuildLogChunk, Status>> {
    // Validate: need at least derivation_path. build_id is needed only
    // for the S3 path (PG lookup is keyed on it); ring buffer is keyed
    // on drv_path alone.
    if req.derivation_path.is_empty() {
        return err_stream(Status::invalid_argument(
            "derivation_path is required (build_id is optional if the \
             derivation is still active)",
        ));
    }

    // Step 1: Ring buffer (active or just-completed-not-yet-drained).
    if let Some(chunks) = try_ring_buffer(log_buffers, &req.derivation_path, req.since_line) {
        debug!(
            drv_path = %req.derivation_path,
            chunks = chunks.len(),
            "serving from ring buffer"
        );
        return chunks_to_stream(chunks);
    }

    // Step 2: S3 (completed). Need build_id for the PG lookup.
    if req.build_id.is_empty() {
        return err_stream(Status::not_found(format!(
            "derivation {:?} has no active ring buffer and build_id was \
             not provided for S3 lookup. If the build completed, retry \
             with build_id.",
            req.derivation_path
        )));
    }
    let build_id: uuid::Uuid = match req.build_id.parse() {
        Ok(id) => id,
        Err(e) => {
            return err_stream(Status::invalid_argument(format!(
                "invalid build_id UUID: {e}"
            )));
        }
    };

    // For the S3 path, we need drv_hash, not drv_path. The spec's
    // S3 key format is `logs/{build_id}/{drv_hash}.log.gz`. The client
    // typically has drv_path (that's what the gateway speaks). We
    // could resolve drv_path→drv_hash via the actor, but the DAG entry
    // is likely gone by now (CleanupTerminalBuild removes it ~30s after
    // completion). Instead: accept EITHER in derivation_path. If it
    // parses as a store path, use the hash-name part; otherwise assume
    // it's already a hash.
    //
    // This is a soft interface contract. The phase4 dashboard will know
    // both and pass drv_hash explicitly.
    let drv_hash = extract_drv_hash(&req.derivation_path);

    match try_s3(s3, pool, &build_id, &drv_hash, req.since_line).await {
        Ok(Some(chunks)) => {
            debug!(
                drv_hash = %drv_hash,
                chunks = chunks.len(),
                "serving from S3"
            );
            chunks_to_stream(chunks)
        }
        Ok(None) => err_stream(Status::not_found(format!(
            "no log found for build {build_id} derivation {drv_hash:?} \
             (not in ring buffer or S3). Either the derivation produced \
             no output, or the flusher hasn't uploaded yet."
        ))),
        Err(status) => err_stream(status),
    }
}

/// Chunk size for streaming S3-fetched log lines back to the client.
/// The whole log is gunzipped into memory first (we need to do line
/// splitting on decompressed data), then re-chunked for the gRPC stream.
/// 256 lines/chunk balances message count vs. per-message size.
const CHUNK_LINES: usize = 256;

/// Try the ring buffer. Returns `Some` if the derivation has any lines
/// buffered (i.e., it's active or just-completed-not-yet-drained).
fn try_ring_buffer(
    log_buffers: &LogBuffers,
    drv_path: &str,
    since: u64,
) -> Option<Vec<BuildLogChunk>> {
    let lines = log_buffers.read_since(drv_path, since);
    if lines.is_empty() {
        return None;
    }
    // Group into CHUNK_LINES-sized chunks. Each chunk carries the
    // first_line_number of its first line for client-side ordering.
    let mut chunks = Vec::new();
    for group in lines.chunks(CHUNK_LINES) {
        let first_line = group[0].0;
        chunks.push(BuildLogChunk {
            derivation_path: drv_path.to_string(),
            lines: group.iter().map(|(_n, bytes)| bytes.clone()).collect(),
            first_line_number: first_line,
            is_complete: false, // ring buffer = still active
        });
    }
    // Mark the LAST chunk is_complete=false too — the derivation is
    // still running. The client should re-poll for more.
    Some(chunks)
}

/// Fetch from S3, gunzip, split lines, chunk. The whole blob comes
/// into memory during gunzip — acceptable for build logs (bounded
/// by the worker's `log_size_limit` of 100 MiB, which gzips to
/// ~10 MiB). True streaming gunzip would need an async GzDecoder
/// that yields lines, which flate2 doesn't have natively.
async fn try_s3(
    s3: &Option<(S3Client, String)>,
    pool: &PgPool,
    build_id: &uuid::Uuid,
    drv_hash: &str,
    since: u64,
) -> Result<Option<Vec<BuildLogChunk>>, Status> {
    let Some((s3, bucket)) = s3 else {
        // No S3 configured. Can't serve completed logs.
        return Ok(None);
    };

    // PG lookup: find the s3_key. The flusher wrote this row with
    // is_complete=true on the final flush.
    let row: Option<(String, i64)> = sqlx::query_as(
        "SELECT s3_key, line_count FROM build_logs
         WHERE build_id = $1 AND drv_hash = $2 AND is_complete = true",
    )
    .bind(build_id)
    .bind(drv_hash)
    .fetch_optional(pool)
    .await
    .status_internal("PG query failed")?;

    let Some((s3_key, _line_count)) = row else {
        return Ok(None); // not in S3 either → truly not found
    };

    debug!(s3_key = %s3_key, "serving build log from S3");

    // S3 GET + full-body drain.
    let resp = s3
        .get_object()
        .bucket(bucket)
        .key(&s3_key)
        .send()
        .await
        .map_err(|e| Status::unavailable(format!("S3 GetObject failed: {e}")))?;
    let gzipped = resp
        .body
        .collect()
        .await
        .map_err(|e| Status::unavailable(format!("S3 body read failed: {e}")))?
        .into_bytes();

    // Gunzip in spawn_blocking — same rationale as the flusher's gzip.
    let drv_hash_owned = drv_hash.to_string();
    let chunks =
        tokio::task::spawn_blocking(move || gunzip_and_chunk(&gzipped, &drv_hash_owned, since))
            .await
            .status_internal("gunzip task panicked")?
            .status_internal("gunzip failed")?;

    Ok(Some(chunks))
}

/// Gunzip the blob, split on `\n`, apply `since` filtering, chunk.
/// Standalone so spawn_blocking can take it without `self`.
///
/// `drv_label` goes into `BuildLogChunk.derivation_path`. The S3 path uses
/// `drv_hash` but the proto field is called `derivation_path` — we put the
/// hash there since that's all we have at this point (the ring-buffer path
/// uses the real drv_path, but for completed builds the DAG entry is gone).
pub(super) fn gunzip_and_chunk(
    gzipped: &[u8],
    drv_label: &str,
    since: u64,
) -> std::io::Result<Vec<BuildLogChunk>> {
    let mut decoder = GzDecoder::new(gzipped);
    let mut raw = Vec::new();
    decoder.read_to_end(&mut raw)?;

    // Split on \n. The flusher joins with \n so every line gets a trailing
    // \n — split() gives an empty last element, which we skip.
    // Line numbers are the index into this split, starting from 0 (the
    // flusher writes them in buffer order, which IS line-number order).
    let mut chunks = Vec::new();
    let mut buf: Vec<Vec<u8>> = Vec::with_capacity(CHUNK_LINES);
    let mut chunk_first_line = since;

    for (n, line) in raw.split(|b| *b == b'\n').enumerate() {
        let n = n as u64;
        if n < since {
            continue; // client already has this line
        }
        // Skip the trailing empty element from the final \n. (An ACTUAL
        // empty log line is uncommon but valid — we keep those. The
        // distinguisher is whether it's the very last element.)
        // We don't know we're at the last element until the iterator ends,
        // so: collect all, then strip a trailing empty. Simpler: check if
        // this is the last byte position. But `split()` doesn't give us
        // that. Simplest: if line is empty AND it's past line_count, skip.
        // Actually: the flusher writes `line\nline\nline\n` — split gives
        // [line, line, line, ""]. The "" is always the last element.
        // We handle it after the loop.
        buf.push(line.to_vec());
        if buf.len() >= CHUNK_LINES {
            chunks.push(BuildLogChunk {
                derivation_path: drv_label.to_string(),
                lines: std::mem::take(&mut buf),
                first_line_number: chunk_first_line,
                is_complete: false,
            });
            chunk_first_line = n + 1;
        }
    }
    // Strip the trailing empty from the final-\n split artifact.
    if buf.last().is_some_and(|l| l.is_empty()) {
        buf.pop();
    }
    if !buf.is_empty() {
        chunks.push(BuildLogChunk {
            derivation_path: drv_label.to_string(),
            lines: buf,
            first_line_number: chunk_first_line,
            is_complete: false,
        });
    }
    // Mark the LAST chunk is_complete=true. From S3 = derivation finished.
    if let Some(last) = chunks.last_mut() {
        last.is_complete = true;
    }
    Ok(chunks)
}

/// Convert a Vec<Chunk> into a ReceiverStream. The chunks are already
/// fully materialized (we either read the ring buffer or gunzipped S3
/// into memory), so there's no backpressure benefit to streaming — but
/// the gRPC API is streaming, so we honor it.
fn chunks_to_stream(chunks: Vec<BuildLogChunk>) -> ReceiverStream<Result<BuildLogChunk, Status>> {
    let (tx, rx) = mpsc::channel(chunks.len().max(1));
    tokio::spawn(async move {
        for chunk in chunks {
            if tx.send(Ok(chunk)).await.is_err() {
                break; // client disconnected
            }
        }
    });
    ReceiverStream::new(rx)
}

/// Wrap a Status in a stream that yields a single `Err(status)` then ends.
///
/// For server-streaming RPCs consumed via grpc-web (the dashboard),
/// returning `Err(Status)` directly from the handler makes tonic emit a
/// Trailers-Only response — `grpc-status` lives in the HTTP headers with
/// zero body. Envoy's grpc_web filter passes that through as-is, and the
/// browser fetch API can't read HTTP trailers — the dashboard sees a
/// silent 200.
///
/// Yielding `Err` from the stream instead makes tonic emit a normal
/// HEADERS frame followed by TRAILERS; Envoy encodes the trailers as a
/// length-prefixed body frame with flag `0x80`, which fetch CAN read.
pub(super) fn err_stream<T: Send + 'static>(status: Status) -> ReceiverStream<Result<T, Status>> {
    let (tx, rx) = mpsc::channel(1);
    // try_send: capacity is 1 and we're the sole sender, can't fail.
    let _ = tx.try_send(Err(status));
    ReceiverStream::new(rx)
}

/// Extract a drv_hash-shaped key from a derivation_path-ish input.
///
/// `/nix/store/{32-char-hash}-{name}.drv` → `{32-char-hash}-{name}.drv`
/// Already-hash-shaped input → unchanged.
pub(super) fn extract_drv_hash(s: &str) -> String {
    s.strip_prefix(rio_nix::store_path::STORE_PREFIX)
        .unwrap_or(s)
        .to_string()
}
