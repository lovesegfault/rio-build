//! `cargo xtask replay report` — download the rendered campaign report
//! (summary.md, plus progress.json and gate.json) and print it.
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
    /// Additionally exit non-zero when the gate's coverage witness is zero
    /// (`checked: 0`): an untripped gate that classified no evidence-bearing
    /// units passed vacuously. Off by default because empty-scope campaigns
    /// legitimately record a clean gate; pipelines whose campaigns must have
    /// exercised real work opt in.
    #[arg(long, requires = "check")]
    pub require_coverage: bool,
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
///
/// `checked` is the gate's coverage witness (the engine's
/// `GateResult::checked`: how many evidence-bearing classified units the
/// trip sets were evaluated over — see the design doc §7.3). This is the
/// design-named single CI consumption point for the gate, so the witness
/// is DEMANDED here, across the JSON boundary the producer-side type
/// cannot cross: `checked` is parsed, printed in the summary line, and an
/// untripped gate with `checked: 0` is called out as a vacuous pass.
///
/// Vacuous-pass policy (deliberately staged):
/// - default: a LOUD warning, exit 0 — an empty-scope campaign
///   legitimately records a clean gate (the engine's `GateCoverage`
///   contract: `NothingInScope` is clean by design), and smoke pipelines
///   run such campaigns today;
/// - `--require-coverage`: exit non-zero — for pipelines whose campaigns
///   must have exercised real work, a vacuous pass is a failure.
///
/// Revisit flipping the default to fail once the recurring smoke
/// pipelines either assert a non-empty scope themselves or adopt the
/// flag; until then a hard default would turn every legitimately-empty
/// campaign red. A gate.json without the `checked` key (written before
/// the witness existed) reads as zero coverage — the honest reading of a
/// record that never carried one — while a present-but-non-numeric value
/// is malformed and errors like any other malformed field.
#[allow(clippy::print_stdout)]
fn check_gate(gate_json: Option<&[u8]>, require_coverage: bool) -> Result<()> {
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
    let checked = match gate.get("checked") {
        None => 0,
        Some(value) => value
            .as_u64()
            .context("report/gate.json \"checked\" is not an unsigned integer")?,
    };
    println!("gate: fail_on={fail_on} tripped={tripped} checked={checked}");
    if tripped {
        anyhow::bail!("regression gate tripped");
    }
    if checked == 0 {
        let vacuous = "the regression gate passed VACUOUSLY: it was evaluated over zero \
                       evidence-bearing classified units (checked: 0), so this pass verified \
                       nothing about the target";
        if require_coverage {
            anyhow::bail!("{vacuous} (--require-coverage)");
        }
        println!("WARNING: {vacuous}; pass --require-coverage to fail on this");
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
        check_gate(gate_body, a.require_coverage)?;
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
        let err = check_gate(None, false).unwrap_err().to_string();
        assert!(err.contains("--report-policy regression-gate"), "{err}");

        // Untripped gate with real coverage: success in both modes.
        let covered =
            br#"{"policy":"regression-gate","fail_on":"regression","tripped":false,"checked":12,"counts":{}}"#;
        check_gate(Some(covered), false).unwrap();
        check_gate(Some(covered), true).unwrap();

        // Tripped gate: non-zero exit through xtask's normal error path.
        let err = check_gate(
            Some(
                br#"{"policy":"regression-gate","fail_on":"regression","tripped":true,"checked":3,"counts":{"unexpected-failure":3}}"#,
            ),
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("regression gate tripped"), "{err}");

        // Malformed documents are errors, never treated as untripped. A
        // present-but-non-numeric `checked` is malformed too — only a fully
        // ABSENT key gets the legacy zero-coverage reading.
        assert!(check_gate(Some(b"not json"), false).is_err());
        assert!(check_gate(Some(br#"{"fail_on":"regression"}"#), false).is_err());
        assert!(check_gate(Some(br#"{"tripped":true}"#), false).is_err());
        assert!(
            check_gate(
                Some(br#"{"fail_on":"regression","tripped":false,"checked":"three"}"#),
                false
            )
            .is_err()
        );
    }

    /// The consumer-side contract for the gate's coverage witness, pinned
    /// over canned gate.json BYTES because this is the wire boundary the
    /// producer-side type witness (`rio_replay::run::report::GateCoverage`,
    /// a `NonZeroUsize` wrapper) cannot cross: serde flattens it to a plain
    /// JSON number, so only this consumer demanding the field makes it
    /// load-bearing. Contract: design doc §7.3 names `report --check` "the
    /// single CI consumption point" for the gate, and `GateResult::checked`
    /// defines `checked: 0` on an untripped gate as a vacuous pass.
    ///
    /// Both directions are pinned: a vacuous pass must be called out (warn
    /// by default, non-zero exit under --require-coverage), AND a covered
    /// pass must stay clean in both modes — so the discriminator is the
    /// witness, not the flag.
    #[test]
    fn check_gate_demands_the_coverage_witness_across_the_wire() {
        // The exact attempted-nothing artifact an engine writes for a
        // campaign whose classification was empty (or all backfill):
        // untripped, checked: 0.
        let vacuous =
            br#"{"policy":"regression-gate","fail_on":"regression","tripped":false,"checked":0,"counts":{}}"#;
        // Default: loud warning, exit 0 — empty-scope campaigns are
        // legitimately clean (GateCoverage::NothingInScope is CLEAN by
        // design; smoke pipelines run such campaigns today).
        check_gate(Some(vacuous), false).unwrap();
        // --require-coverage: the vacuous pass is a failure naming itself.
        let err = check_gate(Some(vacuous), true).unwrap_err().to_string();
        assert!(err.contains("VACUOUSLY"), "{err}");
        assert!(err.contains("checked: 0"), "{err}");

        // A pre-witness gate.json (no `checked` key at all) reads as zero
        // coverage — the honest reading of a record that never carried the
        // witness — and follows the same vacuous-pass policy.
        let legacy =
            br#"{"policy":"regression-gate","fail_on":"regression","tripped":false,"counts":{}}"#;
        check_gate(Some(legacy), false).unwrap();
        assert!(check_gate(Some(legacy), true).is_err());

        // A tripped gate fails REGARDLESS of coverage mode or witness value:
        // the trip decision is never weakened by the coverage check.
        let tripped =
            br#"{"policy":"regression-gate","fail_on":"regression","tripped":true,"checked":0,"counts":{"upload-rejected":1}}"#;
        assert!(check_gate(Some(tripped), false).is_err());
        assert!(check_gate(Some(tripped), true).is_err());
    }
}
