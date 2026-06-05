//! `rio-cli logs` — stream build logs for a derivation.
//!
//! Calls `rio.store.LogService/TailLog` (server-streaming) on the
//! STORE — build logs live in rio-store as immutable chunks with a PG
//! manifest, not on the scheduler. Storage is keyed on
//! `(drv_hash, exec_id)`; the positional arg is the drv path.
//! `--exec-id` is optional and defaults to the latest execution.
//!
//! `--store-addr` / `RIO_STORE_ADDR` must reach the store (same
//! requirement as `upstream` / `verify-chunks`); the scheduler can be
//! down.

use std::io::Write;

use anyhow::anyhow;
use rio_log_kernel::{ChunkVisit, visit_chunk};
use rio_proto::store::{TailLogChunk, TailLogRequest};

use crate::LogsClient;
use crate::RPC_TIMEOUT;

#[derive(clap::Args, Clone)]
pub(crate) struct Args {
    /// Full derivation store path (e.g. `/nix/store/abc-foo.drv`),
    /// basename, or bare 32-char hash.
    drv_path: String,
    /// Specific execution to fetch. Defaults to the latest. UUIDs come
    /// from the worker's `rio: exec` log header line (the first line of
    /// every build log) or the dashboard's per-derivation log view.
    #[arg(long)]
    exec_id: Option<String>,
    /// Tenant session token (JWT) presented to the store. Required
    /// when the store has a JWT pubkey configured: `TailLog` is
    /// tenant-authenticated and ownership-checked (the gateway relays
    /// the watching caller's token automatically; the dashboard is
    /// registry-declared keyless and shows a sign-in-required notice
    /// instead; direct CLI reads supply one here).
    #[arg(long, env = "RIO_TENANT_TOKEN")]
    tenant_token: Option<String>,
}

/// Run the `logs` subcommand.
///
/// `as_json` is NOT threaded here — logs are raw bytes (may be
/// non-UTF-8 build output), and there's no structured JSON shape for
/// an arbitrary byte stream. The global `--json` flag is ignored,
/// same as `gc`.
pub(crate) async fn run(client: &mut LogsClient, a: Args) -> anyhow::Result<()> {
    // STREAMING — NOT via rpc() helper. The helper's whole-call
    // deadline is wrong for log tails (a build can run for an
    // hour). Wrap just the initial call in the timeout — once the
    // stream is open, per-message receives have no deadline (an
    // active build may go minutes between log lines; that's not a
    // hang, that's a slow build).
    //
    // `follow: false` — one-shot drain of whatever is durably stored
    // (plus the live in-memory buffer if the execution is still
    // ingesting). A `--follow` tail would set this true and re-open
    // on premature end; not yet exposed.
    let mut request = tonic::Request::new(TailLogRequest {
        derivation: a.drv_path,
        exec_id: a.exec_id.unwrap_or_default(),
        since_line: 0,
        follow: false,
    });
    if let Some(token) = a.tenant_token.as_deref() {
        request.metadata_mut().insert(
            rio_proto::TENANT_TOKEN_HEADER,
            token
                .parse()
                .map_err(|e| anyhow!("--tenant-token is not a valid header value: {e}"))?,
        );
    }
    let mut stream =
        rio_common::grpc::with_timeout("TailLog", RPC_TIMEOUT, client.tail_log(request))
            .await?
            .into_inner();

    // Drain. `lines` is `repeated bytes` — may be non-UTF-8
    // (build output can be arbitrary). Write raw bytes to
    // stdout, newline-terminated (the proto `lines` field
    // strips trailing newlines; re-add for human readability).
    // Lock stdout once — per-line `println!` would flush each
    // line through the global lock, and a verbose build can
    // emit thousands.
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    // Track the terminal chunk's completeness. The store computes
    // `is_complete` from the manifest (terminal execution +
    // `final_line_count` reported + a contiguous [0, n) chunk range)
    // and stamps it on the last chunk. A still-running build, a
    // cancelled execution, or a log whose final lines never reached
    // the store closes the stream with `is_complete=false`. The lines
    // are still worth printing, but the missing tail is usually the
    // build error itself — say so on stderr rather than letting a
    // truncated log read as the whole thing. Stderr keeps stdout pure
    // log bytes; exit stays 0 (an incomplete log is not a command
    // failure). Initialized `true` so a zero-chunk clean close
    // (unreachable today — every server path emits ≥1 chunk or an
    // error) stays silent.
    // r[impl obs.log.incomplete-surfaced+2]
    let mut last_complete = true;
    // The shared kernel cursor: dedups chunk-granularity resends (the
    // store legally re-serves whole containing chunks) and names
    // forward jumps instead of printing a seamless splice
    // (merged_bug_306 CLI half).
    let mut cursor = 0u64;
    let mut gap_seen = false;
    while let Some(chunk) = stream
        .message()
        .await
        .map_err(|s| anyhow!("TailLog: stream: {} ({:?})", s.message(), s.code()))?
    {
        last_complete = chunk.is_complete;
        emit_chunk(&mut out, &mut cursor, &mut gap_seen, &chunk)?;
    }
    out.flush()?;
    // DOCUMENTED EXCEPTION to stream_util::drain_until_done (bug_141):
    // an incomplete LOG is a rendered state with user-facing semantics
    // (build still running / cancelled / flush pending), not a command
    // failure — `rio-cli logs` stays exit-0 with the disclosure on
    // stderr so pipelines get whatever lines exist.
    // Interior gaps and a truncated tail are different failure shapes:
    // the marker lines above name the former; the note here says which
    // applies so a clean-looking log is never silently partial.
    if !last_complete && gap_seen {
        eprintln!(
            "(log incomplete — interior lines missing from the stored log (marked inline); \
             the tail may also be truncated)"
        );
    } else if !last_complete {
        eprintln!("(log incomplete — build still running, cancelled, or flush pending)");
    } else if gap_seen {
        eprintln!("(stored log has interior gaps — marked inline)");
    }
    Ok(())
}

/// Emit one chunk through the shared cursor: skip the already-printed
/// prefix, print the new lines, and disclose a forward jump with one
/// inline marker line before the chunk that revealed it.
fn emit_chunk<W: Write>(
    out: &mut W,
    cursor: &mut u64,
    gap_seen: &mut bool,
    chunk: &TailLogChunk,
) -> std::io::Result<()> {
    let write_range = |out: &mut W, from: u64, until: u64| -> std::io::Result<()> {
        let first = chunk.first_line_number;
        for n in from..until {
            // The kernel guarantees [from, until) lies inside the
            // chunk; the index arithmetic cannot wrap.
            let line = &chunk.lines[usize::try_from(n - first).expect("kernel-bounded index")];
            out.write_all(line)?;
            out.write_all(b"\n")?;
        }
        Ok(())
    };
    match visit_chunk(*cursor, chunk.first_line_number, chunk.lines.len() as u64) {
        ChunkVisit::Skip { .. } => {}
        ChunkVisit::Serve {
            yield_from,
            yield_until,
            next_line,
        } => {
            write_range(out, yield_from, yield_until)?;
            *cursor = next_line;
        }
        ChunkVisit::GapThenServe {
            gap_from,
            gap_until,
            yield_from,
            yield_until,
            next_line,
        } => {
            *gap_seen = true;
            writeln!(
                out,
                "≡ rio: lines {}-{} missing from stored log ≡",
                gap_from,
                gap_until.saturating_sub(1)
            )?;
            write_range(out, yield_from, yield_until)?;
            *cursor = next_line;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(first: u64, lines: &[&str]) -> TailLogChunk {
        TailLogChunk {
            exec_id: String::new(),
            lines: lines.iter().map(|l| l.as_bytes().to_vec()).collect(),
            first_line_number: first,
            is_complete: false,
        }
    }

    fn drain(chunks: &[TailLogChunk]) -> (String, bool) {
        let mut out = Vec::new();
        let mut cursor = 0u64;
        let mut gap_seen = false;
        for c in chunks {
            emit_chunk(&mut out, &mut cursor, &mut gap_seen, c).unwrap();
        }
        (String::from_utf8(out).unwrap(), gap_seen)
    }

    /// The store's chunk granularity legally re-serves whole containing
    /// chunks; the cursor prints each line exactly once.
    #[test]
    fn resent_prefix_is_not_double_printed() {
        let (out, gap) = drain(&[chunk(0, &["a", "b"]), chunk(0, &["a", "b", "c"])]);
        assert_eq!(out, "a\nb\nc\n", "no double-printed prefix");
        assert!(!gap);
    }

    // r[verify store.log.tail-reconnect]
    /// A wholly-stale chunk (everything below the cursor) prints
    /// nothing and does not move the cursor.
    #[test]
    fn stale_chunk_is_skipped() {
        let (out, gap) = drain(&[chunk(0, &["a", "b", "c"]), chunk(0, &["a", "b"])]);
        assert_eq!(out, "a\nb\nc\n");
        assert!(!gap);
    }

    /// A forward jump prints one marker naming the missing span, then
    /// the chunk — never a seamless splice (merged_bug_306).
    #[test]
    fn gap_prints_inline_marker() {
        let (out, gap) = drain(&[chunk(0, &["a"]), chunk(100, &["z"])]);
        assert_eq!(
            out, "a\n≡ rio: lines 1-99 missing from stored log ≡\nz\n",
            "the marker sits between line 0 and line 100"
        );
        assert!(gap, "the gap flag drives the stderr note");
    }

    /// A zero-line (final-state) chunk is invisible to the cursor.
    #[test]
    fn empty_final_chunk_prints_nothing() {
        let (out, gap) = drain(&[chunk(0, &["a"]), chunk(1, &[])]);
        assert_eq!(out, "a\n");
        assert!(!gap);
    }
}
