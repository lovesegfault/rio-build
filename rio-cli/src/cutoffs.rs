//! `rio-cli cutoffs` — size-class cutoff table.
//!
//! Calls `AdminService.GetSizeClassStatus` and prints a table of
//! `name | configured | effective | queued | running | samples`.
//! Shows how far the SITA-E rebalancer's EMA has drifted from the
//! static TOML config, and how much data backs each class.
//!
//! Separate module (not inline in `main.rs`) to keep P0236/P0237's
//! `main.rs` deltas to the enum variant + match arm + mod decl only.

use rio_proto::AdminServiceClient;
use rio_proto::types::GetSizeClassStatusRequest;
use tonic::transport::Channel;

/// Run the `cutoffs` subcommand.
///
/// `as_json` comes from the global `--json` flag, not a per-subcommand
/// arg — `CliArgs` already parses it globally.
pub(crate) async fn run(
    as_json: bool,
    client: &mut AdminServiceClient<Channel>,
) -> anyhow::Result<()> {
    let resp = crate::rpc("GetSizeClassStatus", async || {
        client
            .get_size_class_status(GetSizeClassStatusRequest::default())
            .await
    })
    .await?;

    if as_json {
        return crate::json(&resp);
    }

    if resp.classes.is_empty() {
        // `repeated` proto field — empty when the scheduler has no
        // `[[size_classes]]` configured (optional feature). Distinct
        // from a table with zero rows.
        println!("(no size classes configured)");
        return Ok(());
    }

    // Fixed-width table. Floats to one decimal — cutoffs are seconds
    // and the rebalancer's EMA smoothing means sub-second precision
    // is noise. Counts are whole integers.
    println!(
        "{:<12} {:>10} {:>10} {:>8} {:>8} {:>8}",
        "CLASS", "CONFIGURED", "EFFECTIVE", "QUEUED", "RUNNING", "SAMPLES"
    );
    for c in &resp.classes {
        println!(
            "{:<12} {:>10.1} {:>10.1} {:>8} {:>8} {:>8}",
            c.name,
            c.configured_cutoff_secs,
            c.effective_cutoff_secs,
            c.queued,
            c.running,
            c.sample_count
        );
    }
    Ok(())
}
