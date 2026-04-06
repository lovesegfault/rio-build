//! `rio-cli gc` — trigger a store garbage-collection sweep.
//!
//! Calls `AdminService.TriggerGC` (server-streaming). Scheduler
//! proxies to the store after populating `extra_roots` from live
//! builds, so in-flight outputs aren't swept. Progress frames are
//! printed line-by-line so the operator sees activity during long
//! sweeps (mark phase on a large store can take minutes).
//!
//! Separate module (not inline in `main.rs`) — same convention as
//! `cutoffs.rs`/`wps.rs`: keep `main.rs` deltas to enum variant +
//! match arm + mod decl only.

use anyhow::anyhow;
use rio_proto::AdminServiceClient;
use rio_proto::types::{GcProgress, GcRequest};
use tonic::transport::Channel;

use crate::RPC_TIMEOUT;

/// Run the `gc` subcommand.
///
/// `as_json` is NOT threaded here — gc progress is line-oriented
/// (operator wants to watch it scroll, not parse one big document
/// at the end). The global `--json` flag is ignored, same as `logs`.
pub(crate) async fn run(
    client: &mut AdminServiceClient<Channel>,
    dry_run: bool,
) -> anyhow::Result<()> {
    // STREAMING — same open-timeout-only shape as `logs`. A store
    // sweep can legitimately go silent for minutes between
    // progress messages (mark phase on a large store).
    let mut stream = tokio::time::timeout(
        RPC_TIMEOUT,
        client.trigger_gc(GcRequest {
            dry_run,
            ..Default::default()
        }),
    )
    .await
    .map_err(|_| anyhow!("TriggerGC: open timed out after {RPC_TIMEOUT:?}"))?
    .map_err(|s| anyhow!("TriggerGC: {} ({:?})", s.message(), s.code()))?
    .into_inner();

    // Drain progress. Print each message as it arrives so the
    // operator sees live progress during long sweeps. Terminal
    // message carries `is_complete=true` with final counts.
    let mut last: Option<GcProgress> = None;
    while let Some(p) = stream
        .message()
        .await
        .map_err(|s| anyhow!("TriggerGC: stream: {} ({:?})", s.message(), s.code()))?
    {
        if p.is_complete {
            println!(
                "GC {}: {} scanned, {} collected, {} bytes freed",
                if dry_run {
                    "dry-run complete"
                } else {
                    "complete"
                },
                p.paths_scanned,
                p.paths_collected,
                p.bytes_freed
            );
        } else {
            println!(
                "  scanned={} collected={} freed={}B current={}",
                p.paths_scanned, p.paths_collected, p.bytes_freed, p.current_path
            );
        }
        last = Some(p);
    }
    // If the stream closed without an `is_complete` frame,
    // surface that — scheduler shutdown or store disconnect
    // mid-sweep. Not an error (the sweep may have completed
    // store-side; we just don't know), but worth flagging.
    if !last.map(|p| p.is_complete).unwrap_or(false) {
        eprintln!(
            "warning: GC stream closed without is_complete — \
             scheduler or store disconnected mid-sweep"
        );
    }
    Ok(())
}
