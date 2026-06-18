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
use crate::stream_util::{self, DrainOutcome, DrainPolicy, MessageStream, MissingSentinel};

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
    // hour); wrap just the initial call in the timeout. Per-message
    // silence is bounded by the drain law below
    // ([`drain_log_chunks`] — the policy site that owns the
    // timeout-regime rationale).
    //
    // `follow: false` — one-shot drain of whatever is durably stored
    // (plus the live in-memory buffer if the execution is still
    // ingesting). A `--follow` tail would set this true and re-open
    // on premature end; not yet exposed.
    let pinned_exec = a.exec_id.is_some();
    // sh-042-r1: clap v4's `env =` populates `Some("")` for an
    // empty-string `RIO_TENANT_TOKEN` (verified against the pinned
    // clap: unset→None, empty→Some(""), set→Some("abc")). Treat
    // empty as absent for both the header insert and the NotFound
    // hint below — an empty `TENANT_TOKEN_HEADER` is a valid http
    // HeaderValue but never a usable JWT.
    let token = a.tenant_token.as_deref().filter(|s| !s.is_empty());
    let mut request = tonic::Request::new(TailLogRequest {
        derivation: a.drv_path,
        exec_id: a.exec_id.unwrap_or_default(),
        since_line: 0,
        follow: false,
    });
    if let Some(token) = token {
        request.metadata_mut().insert(
            rio_proto::TENANT_TOKEN_HEADER,
            token
                .parse()
                .map_err(|e| anyhow!("--tenant-token is not a valid header value: {e}"))?,
        );
    }
    let mut stream = match rio_common::grpc::with_timeout_status(
        "TailLog",
        RPC_TIMEOUT,
        client.tail_log(request),
    )
    .await
    {
        Ok(r) => r.into_inner(),
        // sh-042 side-finding: `xtask k8s cli` (`with_cli_tunnel`)
        // does NOT thread `RIO_TENANT_TOKEN`, so a pinned-exec read
        // against a JWT-gated store returns the absence-shaped
        // `not_found` (tail.rs `lookup_exec_id` — foreign and
        // nonexistent are deliberately indistinguishable) instead of
        // the log. Surface the missing-token hint so the (a)-tier
        // `rio-cli logs <drv> --exec-id <old>` pointer is actionable.
        Err(e) if pinned_exec && token.is_none() && e.code() == tonic::Code::NotFound => {
            return Err(anyhow::Error::from(e).context(
                "the store reports no log for this --exec-id, but no \
                 --tenant-token / RIO_TENANT_TOKEN was supplied — a \
                 JWT-gated store returns the same not_found for an \
                 execution your tenant does not own. Re-run with \
                 --tenant-token (the gateway-issued session JWT).",
            ));
        }
        Err(e) => return Err(anyhow::Error::from(e).context("TailLog")),
    };

    // Drain. `lines` is `repeated bytes` — may be non-UTF-8
    // (build output can be arbitrary). Write raw bytes to
    // stdout, newline-terminated (the proto `lines` field
    // strips trailing newlines; re-add for human readability).
    // Lock stdout once — per-line `println!` would flush each
    // line through the global lock, and a verbose build can
    // emit thousands.
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let (last_complete, gap_seen) = drain_log_chunks(&mut out, &mut stream).await?;
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

// r[impl obs.log.incomplete-surfaced+2]
/// Drain the non-follow `TailLog` stream through the shared drain law
/// (bug_163) and return `(complete, gap_seen)`.
///
/// Completeness: the store computes `is_complete` from the manifest
/// (terminal execution + `final_line_count` reported + a contiguous
/// [0, n) chunk range) and stamps it ONLY in `send_final`, immediately
/// followed by return — the stamp is terminal BY CONSTRUCTION, so the
/// drain law's sentinel seal applies: the loop breaks on it and never
/// polls again. Pre-fix, one more `message().await` ran past the
/// sentinel, and a post-seal transport error (replica rolled, LB RST
/// before trailers) exited nonzero with the provably complete log
/// already on stdout — inverting the documented exit-0 pipeline
/// contract.
///
/// Posture: `DiscloseExitZero` — a clean unsealed end means a
/// still-running build, a cancelled execution, or a never-flushed
/// tail; the lines are worth printing and the caller's stderr notes
/// carry the disclosure (an incomplete LOG is a rendered state, not a
/// command failure). The silence bound still applies: a non-follow
/// replay streams stored chunks back-to-back, so minutes of silence
/// is a dead peer (half-open store death), not a slow build.
async fn drain_log_chunks<W: Write, S: MessageStream<TailLogChunk>>(
    out: &mut W,
    stream: &mut S,
) -> anyhow::Result<(bool, bool)> {
    // The shared kernel cursor: dedups chunk-granularity resends (the
    // store legally re-serves whole containing chunks) and names
    // forward jumps instead of printing a seamless splice
    // (merged_bug_306 CLI half).
    let mut cursor = 0u64;
    let mut gap_seen = false;
    let outcome = stream_util::drain_with(
        &DrainPolicy {
            what: "TailLog",
            inactivity: stream_util::STREAM_INACTIVITY_TIMEOUT,
            missing_sentinel: MissingSentinel::DiscloseExitZero,
        },
        stream,
        |chunk| emit_chunk(out, &mut cursor, &mut gap_seen, chunk).map_err(Into::into),
        |chunk| chunk.is_complete,
    )
    .await?;
    Ok((matches!(outcome, DrainOutcome::Sealed), gap_seen))
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

    // ── The drain-law routing (bug_163) ────────────────────────────

    struct Scripted(std::collections::VecDeque<Result<Option<TailLogChunk>, tonic::Status>>);

    impl MessageStream<TailLogChunk> for Scripted {
        async fn next_message(&mut self) -> Result<Option<TailLogChunk>, tonic::Status> {
            self.0.pop_front().unwrap_or(Ok(None))
        }
    }

    fn sealed(mut c: TailLogChunk) -> TailLogChunk {
        c.is_complete = true;
        c
    }

    // r[verify obs.log.incomplete-surfaced+2]
    /// RED (bug_163): the non-follow drain polled once more past the
    /// terminal-by-construction `is_complete` sentinel, so a post-seal
    /// transport error (replica rolled, LB RST before trailers) exited
    /// nonzero with the provably complete log already on stdout —
    /// inverting the documented exit-0 pipeline contract. The drain
    /// law's sentinel seal breaks on the stamp; the error is never
    /// polled. Pre-fix: `Err("TailLog: stream: LB reset ...")`.
    #[tokio::test]
    async fn sentinel_then_transport_error_is_complete_and_ok() {
        let mut out = Vec::new();
        let mut s = Scripted(
            [
                Ok(Some(chunk(0, &["a"]))),
                Ok(Some(sealed(chunk(1, &["b"])))),
                Err(tonic::Status::unavailable("LB reset before trailers")),
            ]
            .into(),
        );
        let (complete, gap) = drain_log_chunks(&mut out, &mut s)
            .await
            .expect("post-seal transport noise is not a command failure");
        assert!(complete, "the sentinel sealed the log as complete");
        assert!(!gap);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "a\nb\n",
            "the complete log reached stdout"
        );
    }

    /// The exit-0 disclosure posture is preserved: a clean end without
    /// the sentinel drains Ok (the caller's stderr note carries the
    /// disclosure), and a PRE-sentinel transport error is still an
    /// error — only the clean-unsealed-end case is posture-dependent.
    #[tokio::test]
    async fn unsealed_end_is_ok_and_incomplete() {
        let mut out = Vec::new();
        let mut s = Scripted([Ok(Some(chunk(0, &["a"])))].into());
        let (complete, gap) = drain_log_chunks(&mut out, &mut s)
            .await
            .expect("an incomplete log is a rendered state, not a failure");
        assert!(!complete);
        assert!(!gap);

        let mut s = Scripted(
            [
                Ok(Some(chunk(0, &["a"]))),
                Err(tonic::Status::unavailable("mid-stream death")),
            ]
            .into(),
        );
        drain_log_chunks(&mut Vec::new(), &mut s)
            .await
            .expect_err("a pre-sentinel transport error still fails the drain");
    }

    /// sh-042 side-finding (the logs.rs:172 candidate): the kernel
    /// guarantees `[yield_from, yield_until)` lies inside `[first,
    /// first+len)` for both `Serve` and `GapThenServe`, so the
    /// `chunk.lines[n - first]` index can neither underflow nor
    /// out-of-bounds. Pin the boundary cells (overlapping resend with
    /// `cursor > first`; gap-then-serve with `first > cursor`) so a
    /// future kernel refactor that breaks the bound is caught here,
    /// not by a live-cluster panic.
    #[test]
    fn emit_chunk_index_is_kernel_bounded() {
        // Serve with cursor inside the chunk (yield_from > first).
        let (out, _) = drain(&[chunk(0, &["a", "b", "c"]), chunk(0, &["a", "b", "c", "d"])]);
        assert_eq!(out, "a\nb\nc\nd\n");
        // GapThenServe with first > cursor (yield_from == first).
        let (out, gap) = drain(&[chunk(5, &["x", "y"])]);
        assert_eq!(out, "≡ rio: lines 0-4 missing from stored log ≡\nx\ny\n");
        assert!(gap);
        // High first_line_number (the --exec-id replay shape: a stored
        // execution's chunks start past zero only on a re-served
        // tail) — never panics, the gap is named.
        let (out, gap) = drain(&[chunk(u32::MAX as u64, &["z"])]);
        assert!(gap);
        assert!(
            out.starts_with("≡ rio: lines 0-4294967294 missing from stored log ≡\nz\n"),
            "high-boundary gap is named: {out}"
        );
    }
}
