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
use rio_proto::store::TailLogRequest;

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
    let mut stream = rio_common::grpc::with_timeout(
        "TailLog",
        RPC_TIMEOUT,
        client.tail_log(TailLogRequest {
            derivation: a.drv_path,
            exec_id: a.exec_id.unwrap_or_default(),
            since_line: 0,
            follow: false,
        }),
    )
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
    while let Some(chunk) = stream
        .message()
        .await
        .map_err(|s| anyhow!("TailLog: stream: {} ({:?})", s.message(), s.code()))?
    {
        last_complete = chunk.is_complete;
        for line in &chunk.lines {
            out.write_all(line)?;
            out.write_all(b"\n")?;
        }
    }
    out.flush()?;
    if !last_complete {
        eprintln!("(log incomplete — build still running, cancelled, or flush pending)");
    }
    Ok(())
}
