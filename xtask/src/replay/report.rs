//! `cargo xtask replay report` — download the rendered campaign report
//! (summary.md + progress.json) and print it.
//!
//! No campaign logic lives here: the engine renders summary.md (and
//! keeps progress.json fresh) under the campaign's S3 prefix, and this
//! subcommand only downloads the documents into a local output
//! directory and prints the summary verbatim. The campaign Job may
//! already be gone (TTL, abort, cleanup) — the S3 artifacts are the
//! source of truth, so the cluster is never consulted. Whether the
//! campaign drained completely or stopped at its deadline is the
//! `partial` banner inside summary.md itself.
//!
//! `--check` is the single CI consumption point for the regression gate:
//! the engine records the gate result as data (`report/gate.json`, never
//! its own exit code), and this subcommand maps a tripped gate to a
//! non-zero exit.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;

#[derive(Args)]
pub struct ReportArgs {
    /// Campaign id.
    pub campaign: String,
    /// Directory the artifacts are written into
    /// (`<out>/<campaign>/report/summary.md`, …).
    #[arg(long, default_value = ".replay-reports")]
    pub out: PathBuf,
    /// Exit non-zero when the campaign's recorded regression gate tripped
    /// (requires the campaign to have been launched with
    /// `--report-policy regression-gate`).
    #[arg(long)]
    pub check: bool,
}

/// The campaign-relative S3 path of the regression-gate result document.
const GATE_DOC: &str = "report/gate.json";

/// Local path for one downloaded artifact: the campaign id then the
/// artifact's campaign-relative S3 path, so several campaigns (and
/// several artifacts per campaign) can share one `--out` directory.
pub fn local_path(out: &Path, campaign: &str, rel: &str) -> PathBuf {
    out.join(campaign).join(rel)
}

/// Decide the `--check` exit from the downloaded gate.json bytes.
///
/// `None` means the campaign never recorded a gate (the artifact does not
/// exist in S3) — an error, because a CI caller asking for `--check`
/// against a gate-less campaign would otherwise always "pass". A recorded
/// gate prints one summary line and turns `tripped: true` into a non-zero
/// exit through xtask's normal error path. Malformed gate.json is an
/// error, never silently treated as untripped.
#[allow(clippy::print_stdout)]
fn check_gate(gate_json: Option<&[u8]>) -> Result<()> {
    let Some(bytes) = gate_json else {
        anyhow::bail!(
            "--check requested but the campaign recorded no regression gate \
             (launch with --report-policy regression-gate)"
        );
    };
    let gate: serde_json::Value =
        serde_json::from_slice(bytes).context("parse report/gate.json")?;
    let fail_on = gate["fail_on"]
        .as_str()
        .context("report/gate.json has no \"fail_on\" string")?;
    let tripped = gate["tripped"]
        .as_bool()
        .context("report/gate.json has no \"tripped\" boolean")?;
    println!("gate: fail_on={fail_on} tripped={tripped}");
    if tripped {
        anyhow::bail!("regression gate tripped");
    }
    Ok(())
}

#[allow(clippy::print_stdout)]
pub async fn run(a: ReportArgs) -> Result<()> {
    let store = super::s3::CampaignStore::discover()?;

    // summary.md is the deliverable; progress.json gives the
    // comparability/partial-run context next to it; gate.json exists only
    // when the campaign requested the regression-gate report policy.
    let mut downloaded = Vec::new();
    for rel in ["report/summary.md", "progress.json", GATE_DOC] {
        match store.fetch_campaign_doc(&a.campaign, rel).await? {
            Some(body) => {
                let path = local_path(&a.out, &a.campaign, rel);
                let parent = path
                    .parent()
                    .expect("local_path nests under <out>/<campaign>");
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
                std::fs::write(&path, &body)
                    .with_context(|| format!("write {}", path.display()))?;
                downloaded.push((rel, path, body));
            }
            None if rel == "report/summary.md" => anyhow::bail!(
                "{} not found — the campaign has not rendered a report yet \
                 (check `cargo xtask replay status {}`)",
                store.uri(&a.campaign, rel),
                a.campaign
            ),
            None => tracing::warn!("{} not found; skipping", store.uri(&a.campaign, rel)),
        }
    }

    for (rel, path, body) in &downloaded {
        tracing::info!("downloaded {rel} -> {}", path.display());
        if *rel == "report/summary.md" {
            println!("{body}");
        }
    }

    if a.check {
        let gate_body = downloaded
            .iter()
            .find(|(rel, _, _)| *rel == GATE_DOC)
            .map(|(_, _, body)| body.as_bytes());
        check_gate(gate_body)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_path_nests_campaign_and_relpath() {
        assert_eq!(
            local_path(Path::new(".replay-reports"), "c1", "report/summary.md"),
            PathBuf::from(".replay-reports/c1/report/summary.md")
        );
    }

    #[test]
    fn check_gate_maps_the_recorded_gate_to_the_exit_decision() {
        // No gate.json recorded: an error naming the launch flag that would
        // have produced one — never a silent pass.
        let err = check_gate(None).unwrap_err().to_string();
        assert!(err.contains("--report-policy regression-gate"), "{err}");

        // Untripped gate: success.
        check_gate(Some(
            br#"{"policy":"regression-gate","fail_on":"regression","tripped":false,"counts":{}}"#,
        ))
        .unwrap();

        // Tripped gate: non-zero exit through xtask's normal error path.
        let err = check_gate(Some(
            br#"{"policy":"regression-gate","fail_on":"regression","tripped":true,"counts":{"unexpected-failure":3}}"#,
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("regression gate tripped"), "{err}");

        // Malformed documents are errors, never treated as untripped.
        assert!(check_gate(Some(b"not json")).is_err());
        assert!(check_gate(Some(br#"{"fail_on":"regression"}"#)).is_err());
        assert!(check_gate(Some(br#"{"tripped":true}"#)).is_err());
    }
}
