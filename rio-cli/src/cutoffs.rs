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
use rio_proto::types::{GetSizeClassStatusRequest, SizeClassStatus};
use serde::Serialize;
use tonic::transport::Channel;

/// Run the `cutoffs` subcommand.
///
/// `as_json` comes from the global `--json` flag, not a per-subcommand
/// arg — `CliArgs` already parses it globally.
pub(crate) async fn run(
    as_json: bool,
    client: &mut AdminServiceClient<Channel>,
) -> anyhow::Result<()> {
    let resp = crate::rpc(
        "GetSizeClassStatus",
        client.get_size_class_status(GetSizeClassStatusRequest::default()),
    )
    .await?;

    if as_json {
        // Named key (not a bare array) — same future-proofing as
        // `workers --json` / `builds --json`: room for snapshot
        // metadata without a breaking change.
        return crate::json(&CutoffsJson {
            classes: resp.classes.iter().map(ClassJson::from).collect(),
        });
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

// ---------------------------------------------------------------------------
// JSON projection
//
// Prost types don't derive `Serialize` (see the module comment in
// `main.rs`'s JSON block). Thin wrapper like every other subcommand's.
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct CutoffsJson<'a> {
    classes: Vec<ClassJson<'a>>,
}

#[derive(Serialize)]
struct ClassJson<'a> {
    name: &'a str,
    configured_cutoff_secs: f64,
    effective_cutoff_secs: f64,
    queued: u64,
    running: u64,
    sample_count: u64,
}

impl<'a> From<&'a SizeClassStatus> for ClassJson<'a> {
    fn from(c: &'a SizeClassStatus) -> Self {
        Self {
            name: &c.name,
            configured_cutoff_secs: c.configured_cutoff_secs,
            effective_cutoff_secs: c.effective_cutoff_secs,
            queued: c.queued,
            running: c.running,
            sample_count: c.sample_count,
        }
    }
}
