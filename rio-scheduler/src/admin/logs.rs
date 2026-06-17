//! `AdminService.GetDerivationLogs` implementation.
//!
//! Two data sources (per `observability.typ`):
//!
//! | Build State | Source |
//! |---|---|
//! | Active | Ring buffer (in-memory, most recent) |
//! | Completed | S3 (zstd blob, seekable via PG `drv_logs.s3_key`) |
//!
//! We check the ring buffer FIRST: if the derivation is still active,
//! the ring buffer has the freshest lines (the S3 blob, if any, is a
//! 30s-stale periodic snapshot). We fall back to S3 when no ring entry
//! exists — the derivation finished and the flusher drained it (or it
//! was never active on this replica). An entry that exists but holds
//! ZERO lines also falls through, keyed to the entry's stamped
//! execution: the stored-coverage reconcile can empty a retained entry
//! whose `.partial` already has content (an empty ring can never offer
//! more than the stored row), and when nothing is stored yet the
//! handler answers with the same empty re-poll chunk the ring would
//! have produced.
//!
//! Storage is keyed by `(drv_hash, exec_id)` — one row/blob per
//! *derivation execution*, not per build (the scheduler dedups
//! derivations across all concurrent builds, so one execution serves N
//! interested builds). The request's `exec_id` is optional: empty means
//! "the latest execution for this derivation", resolved server-side via
//! the `drv_logs_drv_latest` index (`UUIDv7` is time-sortable, so
//! `MAX(exec_id)` = newest dispatch). Every chunk carries the
//! resolved/provided `exec_id` so the client knows exactly which
//! execution it got even when it asked for "latest".

use aws_sdk_s3::Client as S3Client;
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Status;

use rio_common::grpc::StatusExt;
use tracing::{debug, warn};

use rio_proto::types::{DerivationLogChunk, GetDerivationLogsRequest};

use crate::logs::LogBuffers;

/// Full `GetDerivationLogs` handler body: validate → ring buffer → S3.
///
/// Also the byte-serving body behind the tenant-facing
/// `SchedulerService.GetDerivationLog` (`grpc/derivation_log.rs`), which
/// resolves tenancy and the tail cursor first and then delegates here.
///
/// `try_ring_buffer` and `try_s3` are separately testable; this
/// function just sequences them with the right fallback logic.
///
/// Errors are yielded IN-STREAM via [`err_stream`] rather than returned
/// as `Err(Status)` — returning `Err` from a server-streaming handler
/// makes tonic emit Trailers-Only, which the grpc-web dashboard can't
/// read (browser fetch API can't access HTTP trailers).
pub(crate) async fn get_derivation_logs(
    log_buffers: &LogBuffers,
    s3: &Option<(S3Client, String)>,
    pool: &PgPool,
    req: GetDerivationLogsRequest,
) -> ReceiverStream<Result<DerivationLogChunk, Status>> {
    // Validate: `derivation_path` is the only required field. `exec_id`
    // is optional — empty means "latest execution".
    if req.derivation_path.is_empty() {
        return err_stream(Status::invalid_argument(
            "derivation_path is required (exec_id is optional — empty = \
             latest execution)",
        ));
    }

    // Parse the caller's exec_id BEFORE the ring-buffer probe so a
    // pinned request is never satisfied by a different execution.
    // Parse failure (non-empty but malformed) is a caller error; an
    // empty string is the "latest" sentinel, not an error.
    let req_exec_id: Option<uuid::Uuid> = if req.exec_id.is_empty() {
        None
    } else {
        match req.exec_id.parse() {
            Ok(id) => Some(id),
            Err(e) => {
                return err_stream(Status::invalid_argument(format!(
                    "invalid exec_id UUID: {e}"
                )));
            }
        }
    };

    // Step 1: Ring buffer (active or just-completed-not-yet-drained).
    // Carries `chunk.exec_id` from the buffer entry's `set_exec` stamp
    // (empty if the entry is unstamped — a recovery gap or test state).
    //
    // Pinned-exec gate: if the caller pinned an `exec_id` (the
    // dashboard's build view uses `GraphNode.exec_id` from
    // `build_derivations` — the *exact* execution that build observed),
    // serving the live ring buffer is wrong whenever the drv has been
    // re-dispatched since. A retried build, or a later build's rebuild
    // of the same drv, replaces the buffer entry with a new execution.
    // Without this gate the handler would silently serve the new exec's
    // in-progress lines under the pinned chunk.exec_id, and the
    // dashboard's "approximate / latest available" banner — which keys
    // on `execId === ''` — would stay hidden because the request
    // carried a non-empty pin. Fall through to S3, which has the pinned
    // execution's blob.
    //
    // The live entry's stamp is read once and shared by the pin gate and
    // the empty-entry fallthrough below, so both key off the same
    // observation.
    let live_exec = log_buffers.exec_id(&req.derivation_path);
    let pin_matches_live = req_exec_id
        .map(|pin| live_exec == Some(pin))
        .unwrap_or(true); // empty pin = "latest" = whatever's live

    // Empty-entry fallthrough: a present-but-EMPTY stamped entry cannot
    // answer the request — the only thing the ring path could produce is
    // the empty re-poll chunk, even when the very same execution's stored
    // row/blob already has content. That shape is real: the stored-coverage
    // reconcile empties an unsealed retained entry after an A→B→A flap (an
    // interim leader's row covers past the retained tail, `truncate_below`
    // drops every retained line) and no reaper may remove it — the drv was
    // reset to Ready, so the entry can still become the live carrier.
    // Instead of answering from the ring, probe the stored side keyed to
    // the entry's STAMPED exec; if nothing is stored for that execution
    // (the just-dispatched window — no output uploaded yet), fall back to
    // exactly the empty re-poll chunk the ring would have produced. The
    // stamped-exec keying is what keeps a latest-mode request during a
    // re-dispatched drv's no-output window from resolving `MAX(exec_id)`
    // over stored rows and serving the PREVIOUS execution's blob.
    //
    // Emptiness is an entry-level probe (`span(..).line_count == 0`), NOT
    // "`read_since` returned no lines" — the latter is also true for a
    // caught-up poller on a non-empty entry, which must keep its re-poll
    // answer (see `try_ring_buffer`). Only consulted when the pin gate
    // passes: a mismatched pin already falls through with the pin, and its
    // not-found semantics must not change.
    let empty_entry_exec: Option<uuid::Uuid> = if pin_matches_live {
        live_exec.filter(|exec| {
            log_buffers
                .span(&req.derivation_path, *exec)
                .is_some_and(|(_, _, line_count)| line_count == 0)
        })
    } else {
        None
    };

    if !pin_matches_live {
        debug!(
            drv_path = %req.derivation_path,
            requested_exec = %req.exec_id,
            "pinned exec_id does not match live ring buffer; falling through to S3"
        );
    } else if let Some(exec) = empty_entry_exec {
        debug!(
            drv_path = %req.derivation_path,
            exec_id = %exec,
            "ring entry holds zero lines; probing stored logs for its stamped execution"
        );
    } else if let Some(chunks) = try_ring_buffer(log_buffers, &req.derivation_path, req.since_line)
    {
        debug!(
            drv_path = %req.derivation_path,
            chunks = chunks.len(),
            "serving from ring buffer"
        );
        return chunks_to_stream(chunks);
    }

    // Step 2: S3 (completed, pinned to an exec that isn't the live one,
    // or the empty-entry fallthrough above). We need drv_hash, not
    // drv_path. The S3 key is
    // `logs/{drv_hash}/{exec_id}.log.zst`. The client typically has
    // drv_path (that's what the gateway speaks). We could resolve
    // drv_path→drv_hash via the actor, but the DAG entry is likely
    // gone by now (CleanupTerminalBuild removes it after
    // TERMINAL_CLEANUP_DELAY, ~60s). Instead: accept EITHER in
    // derivation_path —
    // `drv_log_hash` normalizes full path / basename / bare hash to
    // the 32-char hash. Same helper the flusher uses for the PG row,
    // so the lookup key can't drift from what was written.
    let drv_hash = crate::logs::drv_log_hash(&req.derivation_path);

    // Stored-lookup key: an explicit pin always wins (when the empty-entry
    // fallthrough fired with a pin, the pin equals the stamp — the pin
    // gate above is upstream); otherwise the empty entry's stamped exec;
    // otherwise "latest", resolved over stored rows.
    let exec_filter = req_exec_id.or(empty_entry_exec);

    // The empty re-poll chunk the ring path would have produced for the
    // empty entry — the byte-identical fallback when the stored side has
    // nothing (or cannot be consulted) for the stamped execution.
    let empty_repoll_chunk = |exec: uuid::Uuid| DerivationLogChunk {
        derivation_path: req.derivation_path.clone(),
        exec_id: exec.to_string(),
        lines: vec![],
        first_line_number: req.since_line,
        is_complete: false,
    };

    match try_s3(s3, pool, exec_filter.as_ref(), &drv_hash, req.since_line).await {
        Ok(Some(chunks)) => {
            debug!(
                drv_hash = %drv_hash,
                chunks = chunks.len(),
                "serving from S3"
            );
            chunks_to_stream(chunks)
        }
        Ok(None) => match empty_entry_exec {
            // The empty-entry fallthrough found nothing stored for the
            // stamped execution: the entry's existence means the execution
            // is dispatched/known but has produced no stored output yet
            // (the just-dispatched window). Answer exactly like the ring
            // path does today — empty, is_complete=false, re-poll — never
            // NotFound, and never another execution's data.
            Some(exec) => chunks_to_stream(vec![empty_repoll_chunk(exec)]),
            // Tailor the not-found message: a pinned exec_id that's missing
            // is a different failure (typo, expired) from "this drv has
            // never been built / all logs expired".
            None => err_stream(Status::not_found(match req_exec_id {
                Some(exec_id) => format!(
                    "no log found for exec {exec_id} derivation {drv_hash:?} \
                     (not in ring buffer or S3). Either the execution produced \
                     no output, the flusher hasn't uploaded yet, or the log \
                     has expired."
                ),
                None => format!(
                    "no log found for derivation {drv_hash:?} \
                     (no execution recorded, or all expired)"
                ),
            })),
        },
        Err(status) => match empty_entry_exec {
            // Same degraded answer when the stored side cannot be consulted
            // (PG/S3 outage): before the fallthrough existed this read never
            // touched PG or S3 at all, so an infra blip must not turn an
            // active build's "nothing yet" poll into an error. The next poll
            // retries; the outage is loud everywhere else.
            Some(exec) => {
                warn!(
                    drv_path = %req.derivation_path,
                    exec_id = %exec,
                    error = %status,
                    "stored-log probe for an empty ring entry failed; \
                     serving the empty re-poll chunk instead"
                );
                chunks_to_stream(vec![empty_repoll_chunk(exec)])
            }
            None => err_stream(status),
        },
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
///
/// `get_derivation_logs` skips this probe entirely for a stamped entry
/// that holds zero lines (the empty-entry fallthrough above), so the
/// empty chunk produced here is only ever the caught-up answer for a
/// non-empty entry — or the answer for an unstamped entry, which only
/// test fixtures construct.
///
/// Every chunk carries `exec_id` from the buffer entry's `set_exec`
/// stamp — so a client polling an active build knows which execution
/// it's watching without a PG round trip. Empty when the entry is
/// unstamped (a `push()` test fixture or a pre-`set_exec` recovery
/// gap); the client treats that as "active, exec unknown yet".
pub(super) fn try_ring_buffer(
    log_buffers: &LogBuffers,
    drv_path: &str,
    since: u64,
) -> Option<Vec<DerivationLogChunk>> {
    let lines = log_buffers.read_since(drv_path, since)?;
    let exec_id = log_buffers
        .exec_id(drv_path)
        .map(|u| u.to_string())
        .unwrap_or_default();
    if lines.is_empty() {
        // Buffer present but caller already has everything. Per the
        // proto contract: empty + is_complete=false → re-poll.
        return Some(vec![DerivationLogChunk {
            derivation_path: drv_path.to_string(),
            exec_id,
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
        chunks.push(DerivationLogChunk {
            derivation_path: drv_path.to_string(),
            exec_id: exec_id.clone(),
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
///
/// `exec_id_filter`:
/// - `Some(id)` — direct PK lookup. `drv_hash` is NOT in the WHERE
///   clause: `exec_id` is the `drv_logs` PK and globally unique, so the
///   row is fully identified by it. (The caller already normalized the
///   request's `derivation_path` to `drv_hash` for the not-found message
///   and the chunk label, but the storage key is `exec_id`.)
/// - `None` — resolve the latest execution for `drv_hash`. UUIDv7 is
///   time-sortable so `ORDER BY exec_id DESC LIMIT 1` = most recent
///   dispatch; uses the `drv_logs_drv_latest (drv_hash, exec_id DESC)`
///   index so it's a single index seek, not a sort.
///
/// Returns the row's `is_complete` propagated to the last chunk: a
/// `.partial` blob (periodic snapshot of a build whose ring buffer was
/// lost — leader failover, eviction) is served with `is_complete=false`
/// so the client can tell the user the log is incomplete
/// (`obs.log.incomplete-surfaced`). That includes executions that reached
/// a terminal with nothing further to upload (an empty final drain after a
/// failover restamp): `finalize_empty_drain` stamps `status`/`finished_at`
/// but leaves `is_complete=false`, because the stored snapshot is missing
/// the post-failover tail. The OLD model filtered `is_complete=true`
/// because periodic snapshots had no PG row; now they do, and serving them
/// is strictly more useful than NotFound.
///
/// Blobs whose physical line count diverges from the row's span (a
/// gap-merged blob — the failover marker stands in for the lost range —
/// or a hole-carrying blob whose ring had an unmarked interior hole,
/// `obs.log.gap-span`) are re-served from the start regardless of
/// `since`: bandwidth over silently skipping lines the client never got.
async fn try_s3(
    s3: &Option<(S3Client, String)>,
    pool: &PgPool,
    exec_id_filter: Option<&uuid::Uuid>,
    drv_hash: &str,
    since: u64,
) -> Result<Option<Vec<DerivationLogChunk>>, Status> {
    let Some((s3, bucket)) = s3 else {
        // No S3 configured. Can't serve completed logs.
        return Ok(None);
    };

    // PG lookup: resolve the exec_id and find the s3_key. One round
    // trip whether the caller pinned an exec or asked for latest — the
    // latest-exec resolution is folded into the same query rather than
    // a SELECT-then-SELECT.
    let row: Option<(uuid::Uuid, String, i64, i64, bool)> = match exec_id_filter {
        Some(exec_id) => sqlx::query_as(
            "SELECT exec_id, s3_key, first_line, line_count, is_complete
             FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_optional(pool)
        .await
        .status_internal("PG query failed")?,
        None => sqlx::query_as(
            "SELECT exec_id, s3_key, first_line, line_count, is_complete
             FROM drv_logs WHERE drv_hash = $1
             ORDER BY exec_id DESC LIMIT 1",
        )
        .bind(drv_hash)
        .fetch_optional(pool)
        .await
        .status_internal("PG query failed")?,
    };

    let Some((exec_id, s3_key, first_line, line_count, is_complete)) = row else {
        return Ok(None); // not in S3 either → truly not found
    };
    let exec_id_str = exec_id.to_string();
    let first_line = first_line as u64;

    // Short-circuit: client already has every line. Don't fetch +
    // zstd-decode a (potentially 10 MiB) blob just to produce zero
    // chunks — and `decompress_and_chunk` returning `vec![]` means
    // `chunks.last_mut()` is None, so no terminal chunk would ship
    // (client never learns the build finished). One empty terminal
    // chunk satisfies the proto contract.
    if s3_is_caught_up(since, first_line, line_count as u64) {
        return Ok(Some(vec![DerivationLogChunk {
            derivation_path: drv_hash.to_string(),
            exec_id: exec_id_str,
            lines: vec![],
            first_line_number: since,
            is_complete,
        }]));
    }

    debug!(s3_key = %s3_key, exec_id = %exec_id_str, "serving build log from S3");

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
    let exec_for_chunk = exec_id_str.clone();
    let row_line_count = line_count as u64;
    let mut chunks = tokio::task::spawn_blocking(move || {
        decompress_and_chunk(
            &compressed,
            &drv_hash_owned,
            &exec_for_chunk,
            first_line,
            since,
            row_line_count,
        )
    })
    .await
    .status_internal("zstd decode task panicked")?
    .status_internal("zstd decode failed")?;

    // `decompress_and_chunk` is a pure split/chunk fn that doesn't know
    // whether the blob is `.partial` or final, so it leaves
    // `is_complete=false` on every chunk. Stamp the last one from the PG
    // row: a final blob → `true` (client stops polling); a `.partial`
    // blob (build still running, ring buffer lost) → `false` (re-poll).
    if let Some(last) = chunks.last_mut() {
        last.is_complete = is_complete;
    }

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
///
/// For gap-merged and hole-carrying rows `line_count` is the true span
/// (the gap and any unmarked interior hole are counted —
/// `obs.log.gap-span`), so this stays in true line-number space.
pub(super) fn s3_is_caught_up(since: u64, first_line: u64, line_count: u64) -> bool {
    since >= first_line + line_count
}

/// Decompress the zstd blob, split on `\n`, apply `since` filtering, chunk.
/// Standalone so spawn_blocking can take it without `self`.
///
/// `first_line` is the true worker-assigned line number of blob index 0
/// (`drv_logs.first_line` — non-zero iff ring eviction happened).
/// `since` is in the same true-line-number space; both are rebased here
/// so the returned `first_line_number` matches what `try_ring_buffer`
/// would have reported.
///
/// `drv_label` goes into `DerivationLogChunk.derivation_path`. The S3 path uses
/// `drv_hash` but the proto field is called `derivation_path` — we put the
/// hash there since that's all we have at this point (the ring-buffer path
/// uses the real drv_path, but for completed builds the DAG entry is gone).
///
/// `exec_id` is the resolved/provided execution UUID, stamped on every
/// chunk so the client knows which execution it got even when the
/// request asked for "latest".
///
/// `row_line_count` is `drv_logs.line_count` — the row's true span; used
/// only to detect blobs whose physical line count diverges from it. When
/// they diverge the blob is served from its start (`since` ignored) — see
/// the in-body comment.
///
/// All chunks are emitted with `is_complete=false` — this is a pure
/// split/chunk fn that doesn't know whether the blob is a `.partial`
/// snapshot or a final flush. The caller stamps the last chunk from the
/// `drv_logs.is_complete` row value.
pub(super) fn decompress_and_chunk(
    compressed: &[u8],
    drv_label: &str,
    exec_id: &str,
    first_line: u64,
    since: u64,
    row_line_count: u64,
) -> std::io::Result<Vec<DerivationLogChunk>> {
    let decoded = zstd::decode_all(compressed)?;
    // The flusher writes `line\nline\nline\n` — strip the trailing
    // delimiter BEFORE splitting so `split('\n')` yields exactly the
    // lines (no trailing `""`). Stripping post-hoc from `buf` is wrong
    // when `(M-since) % CHUNK_LINES == CHUNK_LINES-1`: the `""` is the
    // element that fills `buf` to CHUNK_LINES and gets `mem::take`n
    // into `chunks` first. Stripping at source eliminates the boundary
    // case structurally.
    let raw: &[u8] = decoded.strip_suffix(b"\n").unwrap_or(&decoded);

    // r[impl obs.log.gap-span]
    // A gap-merged blob (a failover marker replaced two or more lost lines —
    // see `upload_and_record`'s effective-metadata block) has fewer physical
    // lines than the row's true span, so the index→line mapping below is
    // unreliable past the marker: slicing at `since` would skip lines the
    // client never received and mislabel the rest. Ignore the cursor and
    // re-serve from the start (the caller's caught-up short-circuit already
    // handled cursors at/past the true end). Contiguous blobs and markers
    // that replaced exactly one line keep physical == row count, so exact
    // resume is preserved. Hole-carrying blobs (a re-acquired ex-leader's
    // ring with an unmarked interior hole — see `upload_and_record`, which
    // still claims the true span) diverge the same way and get the same
    // full re-serve, as does any remaining mismatch (writer bug).
    let physical_lines = raw.split(|b| *b == b'\n').count() as u64;
    let since = if physical_lines != row_line_count {
        0
    } else {
        since
    };

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
            chunks.push(DerivationLogChunk {
                derivation_path: drv_label.to_string(),
                exec_id: exec_id.to_string(),
                lines: std::mem::take(&mut buf),
                first_line_number: chunk_first_line,
                is_complete: false,
            });
            chunk_first_line = n + 1;
        }
    }
    if !buf.is_empty() {
        chunks.push(DerivationLogChunk {
            derivation_path: drv_label.to_string(),
            exec_id: exec_id.to_string(),
            lines: buf,
            first_line_number: chunk_first_line,
            is_complete: false,
        });
    }
    // The last chunk's `is_complete` is the caller's responsibility —
    // see the fn doc. We don't know if the blob was final or `.partial`.
    Ok(chunks)
}

/// Convert a Vec<Chunk> into a ReceiverStream. The chunks are already
/// fully materialized (we either read the ring buffer or decompressed S3
/// into memory), so there's no backpressure benefit to streaming — but
/// the gRPC API is streaming, so we honor it.
fn chunks_to_stream(
    chunks: Vec<DerivationLogChunk>,
) -> ReceiverStream<Result<DerivationLogChunk, Status>> {
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
