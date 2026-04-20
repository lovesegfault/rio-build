//! `AdminService.GetBuildLogs` implementation.
//!
//! Two data sources (per `observability.md:44-50`):
//!
//! | Build State | Source |
//! |---|---|
//! | Active | Ring buffer (in-memory, most recent) |
//! | Completed | S3 (zstd blob, seekable via PG `build_logs.s3_key`) |
//!
//! We check the ring buffer FIRST: if the derivation is still active,
//! the ring buffer has the freshest lines (the S3 blob, if any, is a
//! 30s-stale periodic snapshot). Only if the ring buffer is empty do
//! we fall back to S3 — which means the derivation finished and the
//! flusher drained it.

use aws_sdk_s3::Client as S3Client;
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
    // S3 key format is `logs/{build_id}/{drv_hash}.log.zst`. The client
    // typically has drv_path (that's what the gateway speaks). We
    // could resolve drv_path→drv_hash via the actor, but the DAG entry
    // is likely gone by now (CleanupTerminalBuild removes it ~30s after
    // completion). Instead: accept EITHER in derivation_path —
    // `drv_log_hash` normalizes full path / basename / bare hash to
    // the 32-char hash. Same helper the flusher uses for the PG row,
    // so the lookup key can't drift from what was written.
    let drv_hash = crate::logs::drv_log_hash(&req.derivation_path);

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
/// The whole log is decompressed into memory first (we need to do line
/// splitting on decompressed data), then re-chunked for the gRPC stream.
/// 256 lines/chunk balances message count vs. per-message size.
const CHUNK_LINES: usize = 256;

/// Try the ring buffer. `None` ⇔ no buffer exists for `drv_path`
/// (derivation not active / already drained → caller falls through to
/// S3). `Some(chunks)` ⇔ buffer exists; when the caller is caught up
/// (`since` ≥ newest line) `chunks` is a single empty
/// `is_complete=false` chunk telling the client to re-poll.
///
/// The previous `lines.is_empty() → None` conflated "no buffer" with
/// "caught up" — a fast-polling dashboard on an active build fell
/// through to S3 and got `NotFound`.
pub(super) fn try_ring_buffer(
    log_buffers: &LogBuffers,
    drv_path: &str,
    since: u64,
) -> Option<Vec<BuildLogChunk>> {
    let lines = log_buffers.read_since(drv_path, since)?;
    if lines.is_empty() {
        // Buffer present but caller already has everything. Per the
        // proto contract: empty + is_complete=false → re-poll.
        return Some(vec![BuildLogChunk {
            derivation_path: drv_path.to_string(),
            lines: vec![],
            first_line_number: since,
            is_complete: false,
        }]);
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

/// Fetch from S3, decompress, split lines, chunk. The whole blob comes
/// into memory during decompression — acceptable for build logs (bounded
/// by the worker's `log_size_limit` of 100 MiB, which compresses to
/// ~10 MiB). True streaming decode would need an async line-yielding
/// decoder; we buffer-whole instead and keep the architecture simple.
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
    let row: Option<(String, i64, i64)> = sqlx::query_as(
        "SELECT s3_key, first_line, line_count FROM build_logs
         WHERE build_id = $1 AND drv_hash = $2 AND is_complete = true",
    )
    .bind(build_id)
    .bind(drv_hash)
    .fetch_optional(pool)
    .await
    .status_internal("PG query failed")?;

    let Some((s3_key, first_line, line_count)) = row else {
        return Ok(None); // not in S3 either → truly not found
    };
    let first_line = first_line as u64;

    // Short-circuit: client already has every line. Don't fetch +
    // zstd-decode a (potentially 10 MiB) blob just to produce zero
    // chunks — and `decompress_and_chunk` returning `vec![]` means
    // `chunks.last_mut()` is None, so no `is_complete=true` would ship
    // (client never learns the build finished). One empty terminal
    // chunk satisfies the proto contract.
    if s3_is_caught_up(since, first_line, line_count as u64) {
        return Ok(Some(vec![BuildLogChunk {
            derivation_path: drv_hash.to_string(),
            lines: vec![],
            first_line_number: since,
            is_complete: true,
        }]));
    }

    debug!(s3_key = %s3_key, "serving build log from S3");

    // S3 GET + full-body drain.
    let resp = s3
        .get_object()
        .bucket(bucket)
        .key(&s3_key)
        .send()
        .await
        .status_unavailable("S3 GetObject failed")?;
    let compressed = resp
        .body
        .collect()
        .await
        .status_unavailable("S3 body read failed")?
        .into_bytes();

    // Decompress in spawn_blocking — same rationale as the flusher's encode.
    let drv_hash_owned = drv_hash.to_string();
    let chunks = tokio::task::spawn_blocking(move || {
        decompress_and_chunk(&compressed, &drv_hash_owned, first_line, since)
    })
    .await
    .status_internal("zstd decode task panicked")?
    .status_internal("zstd decode failed")?;

    Ok(Some(chunks))
}

/// Short-circuit predicate for [`try_s3`]: the client's `since` cursor
/// is at or past the last line in the blob (true line number
/// `first_line + line_count - 1`), so fetching + decoding would yield
/// zero lines.
///
/// Extracted as a pure fn so the bug_084 arithmetic
/// (`since >= first_line + line_count`, NOT `since >= line_count`) is
/// directly unit-testable. Before bug_084 the comparison ignored
/// `first_line`: a 150k-line build with the client at `since=120000`
/// short-circuited against `line_count=100000` (ring-capped survivors)
/// → silently dropped the final 30k lines.
pub(super) fn s3_is_caught_up(since: u64, first_line: u64, line_count: u64) -> bool {
    since >= first_line + line_count
}

/// Decompress the zstd blob, split on `\n`, apply `since` filtering, chunk.
/// Standalone so spawn_blocking can take it without `self`.
///
/// `first_line` is the true worker-assigned line number of blob index 0
/// (`build_logs.first_line` — non-zero iff ring eviction happened).
/// `since` is in the same true-line-number space; both are rebased here
/// so the returned `first_line_number` matches what `try_ring_buffer`
/// would have reported.
///
/// `drv_label` goes into `BuildLogChunk.derivation_path`. The S3 path uses
/// `drv_hash` but the proto field is called `derivation_path` — we put the
/// hash there since that's all we have at this point (the ring-buffer path
/// uses the real drv_path, but for completed builds the DAG entry is gone).
pub(super) fn decompress_and_chunk(
    compressed: &[u8],
    drv_label: &str,
    first_line: u64,
    since: u64,
) -> std::io::Result<Vec<BuildLogChunk>> {
    let decoded = zstd::decode_all(compressed)?;
    // The flusher writes `line\nline\nline\n` — strip the trailing
    // delimiter BEFORE splitting so `split('\n')` yields exactly the
    // lines (no trailing `""`). Stripping post-hoc from `buf` is wrong
    // when `(M-since) % CHUNK_LINES == CHUNK_LINES-1`: the `""` is the
    // element that fills `buf` to CHUNK_LINES and gets `mem::take`n
    // into `chunks` first. Stripping at source eliminates the boundary
    // case structurally.
    let raw: &[u8] = decoded.strip_suffix(b"\n").unwrap_or(&decoded);

    // True line numbers are blob-index + first_line offset (the flusher
    // writes survivors in buffer order, which IS line-number order, but
    // ring eviction means blob index 0 may be true line N>0).
    let mut chunks = Vec::new();
    let mut buf: Vec<Vec<u8>> = Vec::with_capacity(CHUNK_LINES);
    let mut chunk_first_line = since.max(first_line);

    #[allow(
        clippy::sliced_string_as_bytes,
        reason = "raw is a zstd-decoded byte slice, not UTF-8; the flusher \
                  writes raw bytes joined by 0x0A — splitting on 0x0A is the \
                  inverse, no Unicode line-break handling wanted"
    )]
    for (i, line) in raw.split(|b| *b == b'\n').enumerate() {
        let n = first_line + i as u64;
        if n < since {
            continue; // client already has this line
        }
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
/// fully materialized (we either read the ring buffer or decompressed S3
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
