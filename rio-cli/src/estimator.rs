//! `rio-cli estimator` — per-drv-name EMA dump (I-124).
//!
//! Calls `AdminService.GetEstimatorStats` and prints a table of
//! `name | samples | dur | mem | class`. Shows what the scheduler's
//! in-memory `build_history` snapshot says for each `(pname, system)`
//! and which size-class `classify()` would pick under the current
//! effective cutoffs. Operator question: "why is X routed to large?"
//! / "is the EMA for X plausible?".
//!
//! `--filter` is a substring match on pname.

use rio_proto::AdminServiceClient;
use rio_proto::types::GetEstimatorStatsRequest;
use tonic::transport::Channel;

// r[impl cli.cmd.estimator]
/// Run the `estimator` subcommand.
pub(crate) async fn run(
    as_json: bool,
    filter: Option<String>,
    client: &mut AdminServiceClient<Channel>,
) -> anyhow::Result<()> {
    let req = GetEstimatorStatsRequest {
        drv_name_filter: filter,
    };
    let resp = crate::rpc("GetEstimatorStats", async || {
        client.get_estimator_stats(req.clone()).await
    })
    .await?;

    if as_json {
        return crate::json(&resp);
    }

    if resp.entries.is_empty() {
        // Either no build_history rows yet (cold scheduler) or the
        // filter matched nothing. Distinct from a table with rows.
        println!("(no estimator entries)");
        return Ok(());
    }

    // Fixed-width table. Duration to whole seconds (EMA precision
    // beyond that is noise); memory in GiB to one decimal. `name`
    // shows pname + system since the estimator keys on the pair —
    // gcc/x86_64 and gcc/aarch64 are separate EMAs.
    println!(
        "{:<40} {:>8} {:>8} {:>10} {:<10}",
        "NAME", "SAMPLES", "DUR", "MEM", "CLASS"
    );
    for e in &resp.entries {
        println!(
            "{:<40} {:>8} {:>7}s {:>8.1}Gi {:<10}",
            format!("{} ({})", e.drv_name, e.system),
            e.sample_count,
            e.ema_duration_secs.round() as u64,
            e.ema_peak_memory_bytes / (1024.0 * 1024.0 * 1024.0),
            if e.size_class.is_empty() {
                "-"
            } else {
                &e.size_class
            },
        );
    }
    Ok(())
}
