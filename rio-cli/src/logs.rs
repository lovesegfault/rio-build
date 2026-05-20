//! `rio-cli logs` — stream build logs for a derivation.
//!
//! Calls `AdminService.GetDerivationLogs` (server-streaming). Storage
//! is keyed on `(drv_hash, exec_id)`; the positional arg is the drv
//! path. `--exec-id` is optional and defaults to the latest execution.

use std::io::Write;

use crate::AdminClient;
use anyhow::anyhow;
use rio_proto::types::GetDerivationLogsRequest;

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
pub(crate) async fn run(client: &mut AdminClient, a: Args) -> anyhow::Result<()> {
    // STREAMING — NOT via rpc() helper. The helper's 30s whole-
    // call deadline is wrong for log tails (a build can run for
    // an hour). Wrap just the initial call in the timeout —
    // once the stream is open, per-message receives have no
    // deadline (an active build may go minutes between log
    // lines; that's not a hang, that's a slow build).
    let mut stream = rio_common::grpc::with_timeout(
        "GetDerivationLogs",
        RPC_TIMEOUT,
        client.get_derivation_logs(GetDerivationLogsRequest {
            derivation_path: a.drv_path,
            exec_id: a.exec_id.unwrap_or_default(),
            since_line: 0,
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
    while let Some(chunk) = stream.message().await.map_err(|s| {
        anyhow!(
            "GetDerivationLogs: stream: {} ({:?})",
            s.message(),
            s.code()
        )
    })? {
        for line in &chunk.lines {
            out.write_all(line)?;
            out.write_all(b"\n")?;
        }
    }
    out.flush()?;
    Ok(())
}
