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
    /// `--report-policy regression-gate --fail-on regression|divergence`).
    #[arg(long)]
    pub check: bool,
    /// Additionally exit non-zero on a VACUOUS pass: an untripped gate
    /// that classified no evidence-bearing units (`checked: 0`), or one
    /// whose trip condition is `fail_on: "none"` and so structurally
    /// could never have tripped. Off by default because empty-scope
    /// campaigns legitimately record a clean gate and acknowledged
    /// accounting-only gates exist; pipelines whose pass must mean
    /// "verified something" opt in.
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
/// This is the design-named single CI consumption point for the gate,
/// so pass-MEANINGFULNESS is demanded here, across the JSON boundary the
/// producer-side types cannot cross. An untripped gate's pass asserts
/// something only when a CONJUNCTION over the document's own fields
/// holds, and each conjunct has its own vacuous shape
/// ([`vacuous_pass_reasons`]):
///
/// - evaluated over something: `checked` is the coverage witness (the
///   engine's `GateResult::checked`: how many evidence-bearing
///   classified units the trip sets were evaluated over — design doc
///   §7.3); `checked: 0` passed over nothing.
/// - able to trip at all: `fail_on` is the trip condition; under
///   `"none"` the engine's trip predicate is the constant false
///   (`accounting_trips`), so `tripped: false` is structural, however
///   regressed the target — and a healthy-looking non-zero `checked`
///   actively masks it.
///
/// Vacuous-pass policy (deliberately staged):
/// - default: a LOUD warning per failed conjunct, exit 0 — an
///   empty-scope campaign legitimately records a clean gate (the
///   engine's `GateCoverage` contract: `NothingInScope` is clean by
///   design), smoke pipelines run such campaigns today, and launch
///   permits an explicitly-acknowledged accounting-only gate
///   (`--fail-on none`);
/// - `--require-coverage`: exit non-zero — for pipelines whose pass must
///   mean "verified something", any vacuous shape is a failure. There is
///   deliberately no acknowledgment that silences this mode: a pipeline
///   demanding a meaningful pass and a gate that cannot trip are
///   contradictory by definition.
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
             (launch with --report-policy regression-gate --fail-on regression|divergence)"
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
    // The trip decision is never weakened by the meaningfulness checks:
    // a tripped gate fails first, whatever its other fields claim.
    if tripped {
        anyhow::bail!("regression gate tripped");
    }
    let reasons = vacuous_pass_reasons(fail_on, checked);
    if !reasons.is_empty() {
        if require_coverage {
            anyhow::bail!("{} (--require-coverage)", reasons.join("; ALSO: "));
        }
        for reason in &reasons {
            println!("WARNING: {reason}; pass --require-coverage to fail on this");
        }
    }
    Ok(())
}

/// The vacuous shapes of one UNTRIPPED gate document — the failed
/// conjuncts of pass-meaningfulness, each derived from a parsed field of
/// the document itself (see [`check_gate`]). Returned as operator-facing
/// reasons so every failed conjunct is named: the round-3 close demanded
/// only the coverage conjunct, and its healthy non-zero witness then
/// MASKED the sibling shape — a regression-gate campaign launched with
/// the (then-default) `fail_on: "none"`, whose trip predicate is the
/// constant false.
fn vacuous_pass_reasons(fail_on: &str, checked: u64) -> Vec<String> {
    let mut reasons = Vec::new();
    if fail_on == "none" {
        reasons.push(
            "the regression gate passed VACUOUSLY: its trip condition is fail_on \"none\", \
             so no class can trip it and this pass verified nothing about the target \
             (launch with --fail-on regression|divergence for a gate that can fire)"
                .to_string(),
        );
    }
    if checked == 0 {
        reasons.push(
            "the regression gate passed VACUOUSLY: it was evaluated over zero \
             evidence-bearing classified units (checked: 0), so this pass verified \
             nothing about the target"
                .to_string(),
        );
    }
    reasons
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

    /// The consumer-side contract for the gate's TRIP CONDITION, the
    /// second conjunct of pass-meaningfulness: a gate recorded under
    /// `fail_on: "none"` structurally cannot trip (the engine's
    /// `accounting_trips` is the constant false there — rio-replay
    /// `run/report.rs`), so its untripped pass verifies nothing and the
    /// single CI consumption point must say so. Contract: design doc
    /// §7.3's consumption-point paragraph.
    ///
    /// Fixtures are PRODUCER-BUILT across the crate boundary: the bytes
    /// come from `rio_replay::run::report::evaluate_gate` over
    /// producer-typed verdict counts, serialized with the same serde
    /// chain `write_json_atomic` uses for report/gate.json — never from
    /// a consumer-side guess at the wire shape. The whole `FailOn` axis
    /// is swept via `FailOn::ALL` (totality compile-forced engine-side),
    /// so a new trip condition cannot ship without a row here.
    #[test]
    fn check_gate_demands_a_trippable_gate_across_the_wire() {
        use rio_replay::run::model::{Disposition, Verdict};
        use rio_replay::run::report::evaluate_gate;
        use rio_replay::run::spec::{FailOn, ReportBlock};
        use std::collections::BTreeMap;

        // A campaign that REGRESSED: 50 unexpected-failures, 70 matches
        // — keyed by the producer's own wire strings.
        let regressed: BTreeMap<String, usize> = BTreeMap::from([
            (Verdict::UnexpectedFailure.as_str().to_string(), 50),
            (Verdict::MatchBuilt.as_str().to_string(), 70),
        ]);
        // …and one that diverged without regressing.
        let diverged: BTreeMap<String, usize> = BTreeMap::from([
            (Verdict::UnexpectedSuccess.as_str().to_string(), 3),
            (Verdict::MatchBuilt.as_str().to_string(), 70),
        ]);
        // …and a clean one.
        let clean: BTreeMap<String, usize> =
            BTreeMap::from([(Verdict::MatchBuilt.as_str().to_string(), 70)]);
        let no_dispositions: BTreeMap<String, usize> = BTreeMap::new();
        let bytes = |gate: &rio_replay::run::report::GateResult| -> Vec<u8> {
            // The engine writes gate.json via serde_json::to_vec_pretty
            // (state.rs `write_json_atomic`).
            serde_json::to_vec_pretty(gate).unwrap()
        };

        for fail_on in FailOn::ALL {
            // The regressed campaign: under a trippable condition the
            // gate TRIPS and --check fails in both modes; under "none"
            // the trip predicate is structurally false — the document
            // shows healthy coverage and an untripped gate over a fully
            // regressed target.
            let gate = evaluate_gate(fail_on, &regressed, &no_dispositions);
            match fail_on {
                FailOn::Regression | FailOn::Divergence => {
                    assert!(gate.tripped, "{fail_on:?}");
                    assert!(check_gate(Some(&bytes(&gate)), false).is_err());
                    assert!(check_gate(Some(&bytes(&gate)), true).is_err());
                }
                FailOn::None => {
                    assert!(!gate.tripped, "structural: counts can never populate");
                    assert_eq!(gate.checked, 120, "the witness still accrues — the mask");
                    // Default --check stays exit-0 (the staged policy);
                    // --require-coverage refuses, naming the condition.
                    check_gate(Some(&bytes(&gate)), false).unwrap();
                    let err = check_gate(Some(&bytes(&gate)), true)
                        .unwrap_err()
                        .to_string();
                    assert!(err.contains("VACUOUSLY"), "{err}");
                    assert!(err.contains("fail_on \"none\""), "{err}");
                    assert!(err.contains("--fail-on regression|divergence"), "{err}");
                }
            }
            // The diverged campaign trips only the divergence condition.
            let gate = evaluate_gate(fail_on, &diverged, &no_dispositions);
            assert_eq!(gate.tripped, fail_on == FailOn::Divergence, "{fail_on:?}");
            // The clean campaign: untripped everywhere with real
            // coverage; the pass is meaningful exactly when the gate
            // could have fired.
            let gate = evaluate_gate(fail_on, &clean, &no_dispositions);
            assert!(!gate.tripped);
            check_gate(Some(&bytes(&gate)), false).unwrap();
            assert_eq!(
                check_gate(Some(&bytes(&gate)), true).is_ok(),
                fail_on != FailOn::None,
                "{fail_on:?}"
            );
        }

        // The DEFAULT-CONFIGURATION chain, end to end: an engine spec
        // block left at its default (`ReportBlock::default()`, the same
        // FailOn::None a pre-acknowledgment `launch` wrote when the flag
        // was omitted) produces, over a regressed target, exactly the
        // permanently-green document — pinned against the canned bytes
        // the round-3 fixtures should have included. Semantic equality:
        // the producer serializes pretty, the pin is compact.
        let default_gate =
            evaluate_gate(ReportBlock::default().fail_on, &regressed, &no_dispositions);
        let masked_shape =
            br#"{"policy":"regression-gate","fail_on":"none","tripped":false,"checked":120,"counts":{}}"#;
        assert_eq!(
            serde_json::to_value(&default_gate).unwrap(),
            serde_json::from_slice::<serde_json::Value>(masked_shape).unwrap(),
            "the default-config producer chain writes the masked shape verbatim"
        );
        // The disposition trip classes ride the same axis: upload-rejected
        // trips regression — except under "none".
        let upload_rejected: BTreeMap<String, usize> =
            BTreeMap::from([(Disposition::UploadRejected.as_str().to_string(), 2)]);
        assert!(!evaluate_gate(FailOn::None, &clean, &upload_rejected).tripped);
        assert!(evaluate_gate(FailOn::Regression, &clean, &upload_rejected).tripped);
    }

    /// Pass-meaningfulness is a CONJUNCTION over the wire document, and
    /// the consumer must hold it over ARBITRARY field combinations — the
    /// document is wire data, not a producer-typed value, so shapes the
    /// current engine cannot produce (e.g. `tripped: true` under
    /// `fail_on: "none"`) still get a defined, conservative decision.
    /// Full grid: fail_on × tripped × checked × --require-coverage, with
    /// expectations derived from the policy's first principles, not from
    /// the implementation.
    #[test]
    fn check_gate_meaningfulness_grid_over_the_wire_document() {
        for fail_on in ["none", "regression", "divergence"] {
            for tripped in [false, true] {
                for checked in [None, Some(0u64), Some(120u64)] {
                    for require_coverage in [false, true] {
                        let doc = match checked {
                            None => format!(
                                r#"{{"policy":"regression-gate","fail_on":"{fail_on}","tripped":{tripped},"counts":{{}}}}"#
                            ),
                            Some(n) => format!(
                                r#"{{"policy":"regression-gate","fail_on":"{fail_on}","tripped":{tripped},"checked":{n},"counts":{{}}}}"#
                            ),
                        };
                        let result = check_gate(Some(doc.as_bytes()), require_coverage);
                        // First principles: a tripped gate ALWAYS fails
                        // (trip decision never weakened); an untripped
                        // gate fails only under --require-coverage with a
                        // vacuous shape — no trip condition, or no
                        // coverage (absent `checked` reads as zero).
                        let vacuous = fail_on == "none" || checked.unwrap_or(0) == 0;
                        let expect_err = tripped || (require_coverage && vacuous);
                        assert_eq!(
                            result.is_err(),
                            expect_err,
                            "fail_on={fail_on} tripped={tripped} checked={checked:?} \
                             require_coverage={require_coverage}: {result:?}"
                        );
                        if tripped {
                            // Trip-bail precedence: the error names the
                            // trip, never a vacuous shape.
                            let err = result.unwrap_err().to_string();
                            assert!(err.contains("regression gate tripped"), "{err}");
                        } else if expect_err {
                            let err = result.unwrap_err().to_string();
                            assert!(err.contains("VACUOUSLY"), "{err}");
                            // BOTH failed conjuncts are named when both
                            // fail — one reason cannot mask the other.
                            if fail_on == "none" && checked.unwrap_or(0) == 0 {
                                assert!(err.contains("fail_on \"none\""), "{err}");
                                assert!(err.contains("checked: 0"), "{err}");
                            }
                        }
                    }
                }
            }
        }
    }
}
