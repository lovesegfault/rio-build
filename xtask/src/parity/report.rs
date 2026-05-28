//! `cargo xtask parity report` — download the rendered campaign report
//! (summary.md + progress.json) and print it.
//!
//! No campaign logic lives here: the engine renders summary.md (and
//! keeps progress.json fresh) under the campaign's S3 prefix, and this
//! subcommand only downloads the two documents into a local output
//! directory and prints the summary verbatim. The campaign Job may
//! already be gone (TTL, abort, cleanup) — the S3 artifacts are the
//! source of truth, so the cluster is never consulted. Whether the
//! campaign drained completely or stopped at its deadline is the
//! `partial` banner inside summary.md itself.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;

#[derive(Args)]
pub struct ReportArgs {
    /// Campaign id.
    pub campaign: String,
    /// Directory the artifacts are written into
    /// (<out>/<campaign>/report/summary.md, …).
    #[arg(long, default_value = ".parity-reports")]
    pub out: PathBuf,
}

/// Local path for one downloaded artifact: the campaign id then the
/// artifact's campaign-relative S3 path, so several campaigns (and
/// several artifacts per campaign) can share one `--out` directory.
pub fn local_path(out: &Path, campaign: &str, rel: &str) -> PathBuf {
    out.join(campaign).join(rel)
}

#[allow(clippy::print_stdout)]
pub async fn run(a: ReportArgs) -> Result<()> {
    let store = super::s3::CampaignStore::discover()?;

    // summary.md is the deliverable; progress.json gives the
    // comparability/partial-run context next to it.
    let mut downloaded = Vec::new();
    for rel in ["report/summary.md", "progress.json"] {
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
                 (check `cargo xtask parity status {}`)",
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_path_nests_campaign_and_relpath() {
        assert_eq!(
            local_path(Path::new(".parity-reports"), "c1", "report/summary.md"),
            PathBuf::from(".parity-reports/c1/report/summary.md")
        );
    }
}
