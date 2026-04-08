//! `rio-cli logs` — stream build logs for a derivation.
//!
//! Calls `AdminService.GetBuildLogs` (server-streaming). The server
//! keys its ring buffer on `derivation_path`, not `build_id` — the
//! positional arg is the drv path. `--build-id` is only needed for
//! completed builds (S3 archive lookup); active builds serve from
//! the ring buffer by drv path alone.
//!
//! Separate module (not inline in `main.rs`) — same convention as
//! `cutoffs.rs`/`wps.rs`: keep `main.rs` deltas to enum variant +
//! match arm + mod decl only.

use std::io::Write;

use anyhow::anyhow;
use rio_proto::AdminServiceClient;
use rio_proto::types::GetBuildLogsRequest;
use tonic::transport::Channel;

use crate::RPC_TIMEOUT;

/// Run the `logs` subcommand.
///
/// `as_json` is NOT threaded here — logs are raw bytes (may be
/// non-UTF-8 build output), and there's no structured JSON shape for
/// an arbitrary byte stream. The global `--json` flag is ignored,
/// same as `gc`.
pub(crate) async fn run(
    client: &mut AdminServiceClient<Channel>,
    drv_path: String,
    build_id: Option<String>,
) -> anyhow::Result<()> {
    // STREAMING — NOT via rpc() helper. The helper's 30s whole-
    // call deadline is wrong for log tails (a build can run for
    // an hour). Wrap just the initial call in the timeout —
    // once the stream is open, per-message receives have no
    // deadline (an active build may go minutes between log
    // lines; that's not a hang, that's a slow build).
    let mut stream = rio_common::grpc::with_timeout(
        "GetBuildLogs",
        RPC_TIMEOUT,
        client.get_build_logs(GetBuildLogsRequest {
            build_id: build_id.unwrap_or_default(),
            derivation_path: drv_path,
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
    while let Some(chunk) = stream
        .message()
        .await
        .map_err(|s| anyhow!("GetBuildLogs: stream: {} ({:?})", s.message(), s.code()))?
    {
        for line in &chunk.lines {
            out.write_all(line)?;
            out.write_all(b"\n")?;
        }
    }
    out.flush()?;
    Ok(())
}
