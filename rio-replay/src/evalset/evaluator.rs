//! nix-eval-jobs runner and JSONL output parsing.
//!
//! nix-eval-jobs prints one JSON object per evaluated attribute; this
//! module classifies each line into a manifest record (a buildable
//! job), an eval error, or an aggregate, and runs the evaluator as a
//! subprocess collecting all three.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

/// One line of nix-eval-jobs JSONL output (tolerant: unknown fields are
/// ignored, all fields optional so version drift surfaces as a typed
/// "neither error nor drvPath" error instead of a serde panic).
#[derive(Debug, Clone, Deserialize)]
struct RawEvalLine {
    /// Human-display rendering of the attribute path. nix-eval-jobs
    /// quotes any path component that is not a bare Nix identifier, so
    /// for the scoped selection's flat dotted attribute names this
    /// arrives wrapped in literal `"` characters — never use it as a
    /// join key; it is only a fallback when `attrPath` is absent.
    attr: Option<String>,
    /// Machine-readable attribute path, one element per component, free
    /// of display escaping.
    #[serde(default, rename = "attrPath")]
    attr_path: Vec<String>,
    #[serde(rename = "drvPath")]
    drv_path: Option<String>,
    #[serde(default)]
    outputs: BTreeMap<String, String>,
    system: Option<String>,
    error: Option<String>,
    #[serde(default)]
    constituents: Vec<String>,
}

impl RawEvalLine {
    /// Canonical attribute/job name for this line: the dot-join of the
    /// structured `attrPath` — which is exactly Hydra's job-name
    /// convention, so a scoped selection's flat `"a.b.c"` attribute and
    /// a full evaluation's nested `a.b.c` path both derive the name the
    /// recorder's ground truth, buildstatus truth, and unit labels are
    /// keyed by. Falls back to the display `attr` rendering only when
    /// `attrPath` is missing entirely.
    fn canonical_attr(&self) -> String {
        if self.attr_path.is_empty() {
            self.attr.clone().unwrap_or_default()
        } else {
            self.attr_path.join(".")
        }
    }
}

/// One in-scope job as the evaluator produced it:
/// `{job, system, attr, drvPath, outputs, requiredFeatures?}` (camelCase on
/// the wire). These records become the archive's `units.jsonl` workload
/// units when the eval pipeline stages a replay archive.
///
/// `requiredFeatures` is not part of the nix-eval-jobs output; it is
/// backfilled later from the derivation's `requiredSystemFeatures` before
/// the records are handed to the archive staging step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestRecord {
    pub job: String,
    pub system: String,
    pub attr: String,
    #[serde(rename = "drvPath")]
    pub drv_path: String,
    pub outputs: BTreeMap<String, String>,
    #[serde(rename = "requiredFeatures", skip_serializing_if = "Option::is_none")]
    pub required_features: Option<Vec<String>>,
}

/// One attribute that failed to evaluate. These records become the
/// archive's `exclusions.jsonl` eval-error entries when the eval pipeline
/// stages a replay archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalErrorRecord {
    pub attr: String,
    pub error: String,
}

/// Classification of one nix-eval-jobs output line.
#[derive(Debug, Clone)]
pub enum ParsedEvalLine {
    Job(ManifestRecord),
    /// `_hydraAggregate` jobs (they carry a `constituents` list when
    /// the evaluator runs with `--constituents`). Excluded from build
    /// scope and from the fidelity gate: Hydra rewrites aggregate
    /// derivations after evaluation, so their drvPaths can never match
    /// a local run.
    Aggregate {
        attr: String,
        drv_path: String,
    },
    Error(EvalErrorRecord),
}

/// Clip an evaluator output line to a short prefix for error messages.
/// nix-eval-jobs lines can run to tens of kilobytes (large `outputs`
/// maps), and serde_json errors already report the offending byte
/// offset, so a 200-character prefix is enough to identify the line
/// without flooding the error chain.
fn line_snippet(line: &str) -> String {
    const MAX_CHARS: usize = 200;
    let mut chars = line.chars();
    let snippet: String = chars.by_ref().take(MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{snippet}…")
    } else {
        snippet
    }
}

pub fn parse_eval_jobs_line(line: &str) -> anyhow::Result<ParsedEvalLine> {
    let raw: RawEvalLine = serde_json::from_str(line)
        .with_context(|| format!("parse nix-eval-jobs output line: {}", line_snippet(line)))?;
    let attr = raw.canonical_attr();
    if let Some(error) = raw.error {
        return Ok(ParsedEvalLine::Error(EvalErrorRecord { attr, error }));
    }
    let Some(drv_path) = raw.drv_path else {
        anyhow::bail!(
            "nix-eval-jobs line for attr {attr:?} has neither `error` nor `drvPath`: {}",
            line_snippet(line)
        );
    };
    if !raw.constituents.is_empty() {
        return Ok(ParsedEvalLine::Aggregate { attr, drv_path });
    }
    Ok(ParsedEvalLine::Job(ManifestRecord {
        job: attr.clone(),
        system: raw.system.unwrap_or_default(),
        attr,
        drv_path,
        outputs: raw.outputs,
        required_features: None,
    }))
}

/// Aggregated result of one evaluator run.
#[derive(Debug, Default)]
pub struct EvalOutput {
    pub manifest: Vec<ManifestRecord>,
    pub errors: Vec<EvalErrorRecord>,
    /// `(attr, drvPath)` of every aggregate line, kept for the eval-set
    /// stats and excluded from the manifest.
    pub aggregates: Vec<(String, String)>,
}

/// Run nix-eval-jobs and parse its stdout. stderr is written to
/// `stderr_log` for debugging (it carries eval warnings and OOM/restart
/// notices). Needs `nix-eval-jobs`, a Nix store, and (for a real
/// jobset) network access, so the offline unit suite never calls it; it
/// is exercised end-to-end when an eval set is actually built.
pub async fn run_evaluator(
    nix_eval_jobs_bin: &str,
    argv: &[String],
    stderr_log: &Path,
) -> anyhow::Result<EvalOutput> {
    use tokio::io::AsyncBufReadExt as _;

    tracing::info!(bin = nix_eval_jobs_bin, ?argv, "running nix-eval-jobs");
    let stderr_file = std::fs::File::create(stderr_log)
        .with_context(|| format!("create {}", stderr_log.display()))?;
    let mut child = tokio::process::Command::new(nix_eval_jobs_bin)
        .args(argv)
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr_file))
        // A parse error mid-stream returns early and drops `child`;
        // kill_on_drop keeps that early return from orphaning a
        // still-running nix-eval-jobs.
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn {nix_eval_jobs_bin}"))?;

    let stdout = child.stdout.take().context("evaluator stdout missing")?;
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let mut out = EvalOutput::default();
    while let Some(line) = lines.next_line().await.context("read evaluator stdout")? {
        if line.trim().is_empty() {
            continue;
        }
        match parse_eval_jobs_line(&line)? {
            ParsedEvalLine::Job(rec) => out.manifest.push(rec),
            ParsedEvalLine::Error(e) => out.errors.push(e),
            ParsedEvalLine::Aggregate { attr, drv_path } => out.aggregates.push((attr, drv_path)),
        }
    }
    let status = child.wait().await.context("wait for nix-eval-jobs")?;
    anyhow::ensure!(
        status.success(),
        "nix-eval-jobs exited with {status}; see {}",
        stderr_log.display()
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Golden captures from the pinned nix-eval-jobs (2.34.1) — shape
    // verbatim, store paths substituted with the recorded eval-1824219
    // values other fixtures pin. The scoped selection exposes each job
    // as a single flat attribute named by its dotted Hydra job name, and
    // nix-eval-jobs renders such names in `attr` wrapped in literal `"`
    // characters (`attrPath` carries the clean name):
    //
    //   nix-eval-jobs --workers 1 --meta <selection.nix>
    //
    // Hand-editing these shapes (e.g. unquoting `attr`) would re-pin the
    // parser to output the real binary never produces.
    const HELLO_LINE: &str = r#"{"attr":"\"nixpkgs.hello.x86_64-linux\"","attrPath":["nixpkgs.hello.x86_64-linux"],"constituents":[],"drvPath":"/nix/store/7mdg60drrnh0wq1j8hmmbhll47czm107-hello-2.12.3.drv","globConstituents":false,"meta":{"available":true},"name":"hello-2.12.3","namedConstituents":[],"outputs":{"out":"/nix/store/10s5j3mfdg22k1597x580qrhprnzcjwb-hello-2.12.3"},"requiredSystemFeatures":[],"storeDir":"/nix/store","system":"x86_64-linux"}"#;
    const ERROR_LINE: &str = r#"{"attr":"\"nixpkgs.broken.x86_64-linux\"","attrPath":["nixpkgs.broken.x86_64-linux"],"error":"attribute 'broken' missing","fatal":false}"#;
    const AGGREGATE_LINE: &str = r#"{"attr":"\"nixos.tested\"","attrPath":["nixos.tested"],"constituents":["/nix/store/bvzyd5iyaaaaaaaaaaaaaaaaaaaaaaaa-nixos-minimal.iso.drv"],"drvPath":"/nix/store/nfxb7045aaaaaaaaaaaaaaaaaaaaaaaa-nixos-26.05pre975402.68d8aa3d661f.drv","globConstituents":false,"name":"nixos-26.05pre975402.68d8aa3d661f","namedConstituents":[],"outputs":{"out":"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-nixos-26.05pre975402.68d8aa3d661f"},"requiredSystemFeatures":[],"storeDir":"/nix/store","system":"x86_64-linux"}"#;
    // A full-evaluation line (`--force-recurse`): the attr path is
    // nested, so `attr` is the clean dot-join and `attrPath` has one
    // element per nesting level. Both shapes must derive the same job
    // name.
    const NESTED_LINE: &str = r#"{"attr":"nixos.tests.login.x86_64-linux","attrPath":["nixos","tests","login","x86_64-linux"],"constituents":[],"drvPath":"/nix/store/lwi798n9vczdqf7a51yfhl70kjp5b3b5-vm-test-run-login.drv","globConstituents":false,"name":"vm-test-run-login","namedConstituents":[],"outputs":{"out":"/nix/store/j94lvxv7sn2129q61km8pi5qvyxphb4j-vm-test-run-login"},"requiredSystemFeatures":[],"storeDir":"/nix/store","system":"x86_64-linux"}"#;

    #[test]
    fn parses_a_job_line_into_a_manifest_record() {
        let parsed = parse_eval_jobs_line(HELLO_LINE).unwrap();
        match parsed {
            ParsedEvalLine::Job(rec) => {
                // The job name comes from the structured `attrPath`, not
                // the quote-wrapped `attr` rendering, so it is exactly
                // the Hydra job name the selection was generated from —
                // the join key for the fidelity gate, the buildstatus
                // truth, and the units.jsonl label.
                assert_eq!(rec.job, "nixpkgs.hello.x86_64-linux");
                assert_eq!(rec.attr, "nixpkgs.hello.x86_64-linux");
                assert_eq!(rec.system, "x86_64-linux");
                assert_eq!(
                    rec.drv_path,
                    "/nix/store/7mdg60drrnh0wq1j8hmmbhll47czm107-hello-2.12.3.drv"
                );
                assert_eq!(
                    rec.outputs["out"],
                    "/nix/store/10s5j3mfdg22k1597x580qrhprnzcjwb-hello-2.12.3"
                );
                assert_eq!(rec.required_features, None);
            }
            other => panic!("expected Job, got {other:?}"),
        }
    }

    /// Round-trip: every job-name shape [`super::super::recipe::selection_expr`]
    /// accepts (dots as separators; `-`, `_`, `+` inside components)
    /// comes back from the evaluator output as exactly the requested
    /// name. The lines are golden captures of the pinned nix-eval-jobs
    /// evaluating a flat selection over these names — each `attr` is
    /// quote-wrapped, so deriving names from `attr` instead of
    /// `attrPath` fails this test.
    #[test]
    fn scoped_job_names_round_trip_through_evaluator_output() {
        const ODD_NAME_LINES: [(&str, &str); 3] = [
            (
                "nixos.tests.systemd-networkd.x86_64-linux",
                r#"{"attr":"\"nixos.tests.systemd-networkd.x86_64-linux\"","attrPath":["nixos.tests.systemd-networkd.x86_64-linux"],"constituents":[],"drvPath":"/nix/store/ik8aihdqgmp671k66rjphs4nyxv50f77-vm-test-run-systemd-networkd.drv","globConstituents":false,"name":"vm-test-run-systemd-networkd","namedConstituents":[],"outputs":{"out":"/nix/store/p3khaykm4r1z4i75py1bc96rz06j50h8-vm-test-run-systemd-networkd"},"requiredSystemFeatures":[],"storeDir":"/nix/store","system":"x86_64-linux"}"#,
            ),
            (
                "nixpkgs.gnupg24+libusb.x86_64-linux",
                r#"{"attr":"\"nixpkgs.gnupg24+libusb.x86_64-linux\"","attrPath":["nixpkgs.gnupg24+libusb.x86_64-linux"],"constituents":[],"drvPath":"/nix/store/wra3s69mm29an6srfnbg7f2q9dgnj8b1-gnupg-2.4.5.drv","globConstituents":false,"name":"gnupg-2.4.5","namedConstituents":[],"outputs":{"out":"/nix/store/7vgq5iqr5kbpbzx664a1mi76znv1pr75-gnupg-2.4.5"},"requiredSystemFeatures":[],"storeDir":"/nix/store","system":"x86_64-linux"}"#,
            ),
            (
                "nixpkgs.haskellPackages.aeson_2_2_3_0.x86_64-linux",
                r#"{"attr":"\"nixpkgs.haskellPackages.aeson_2_2_3_0.x86_64-linux\"","attrPath":["nixpkgs.haskellPackages.aeson_2_2_3_0.x86_64-linux"],"constituents":[],"drvPath":"/nix/store/j8s55vwhljra6kq7q19ykq0pcb6a5g8f-aeson-2.2.3.0.drv","globConstituents":false,"name":"aeson-2.2.3.0","namedConstituents":[],"outputs":{"out":"/nix/store/zf5nl55j6zpb3w4gpcrczhr2y9a1sglj-aeson-2.2.3.0"},"requiredSystemFeatures":[],"storeDir":"/nix/store","system":"x86_64-linux"}"#,
            ),
        ];
        for (requested, line) in ODD_NAME_LINES {
            match parse_eval_jobs_line(line).unwrap() {
                ParsedEvalLine::Job(rec) => {
                    assert_eq!(rec.job, requested, "job name must round-trip");
                    assert_eq!(rec.attr, requested);
                }
                other => panic!("expected Job for {requested}, got {other:?}"),
            }
        }
    }

    #[test]
    fn nested_attr_paths_join_to_the_hydra_job_name() {
        // Full evaluations expose nested attrsets; the dot-join of
        // `attrPath` is Hydra's job-name convention, so nested and flat
        // shapes both derive the canonical name.
        match parse_eval_jobs_line(NESTED_LINE).unwrap() {
            ParsedEvalLine::Job(rec) => {
                assert_eq!(rec.job, "nixos.tests.login.x86_64-linux");
                assert_eq!(rec.attr, "nixos.tests.login.x86_64-linux");
            }
            other => panic!("expected Job, got {other:?}"),
        }
    }

    #[test]
    fn missing_attr_path_falls_back_to_the_attr_field() {
        // Defensive fallback for producers that omit `attrPath`: the
        // display string is better than no name at all.
        let line = r#"{"attr":"hello","drvPath":"/nix/store/7mdg60drrnh0wq1j8hmmbhll47czm107-hello-2.12.3.drv","outputs":{},"system":"x86_64-linux"}"#;
        match parse_eval_jobs_line(line).unwrap() {
            ParsedEvalLine::Job(rec) => assert_eq!(rec.job, "hello"),
            other => panic!("expected Job, got {other:?}"),
        }
    }

    #[test]
    fn parses_an_error_line() {
        match parse_eval_jobs_line(ERROR_LINE).unwrap() {
            ParsedEvalLine::Error(e) => {
                // Eval-error attrs become exclusions.jsonl labels, so
                // they need the same clean name as job records.
                assert_eq!(e.attr, "nixpkgs.broken.x86_64-linux");
                assert!(e.error.contains("missing"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn aggregate_lines_are_classified_not_manifested() {
        match parse_eval_jobs_line(AGGREGATE_LINE).unwrap() {
            ParsedEvalLine::Aggregate { attr, .. } => assert_eq!(attr, "nixos.tested"),
            other => panic!("expected Aggregate, got {other:?}"),
        }
    }

    #[test]
    fn manifest_record_serializes_with_design_field_names() {
        let rec = ManifestRecord {
            job: "nixpkgs.hello.x86_64-linux".into(),
            system: "x86_64-linux".into(),
            attr: "nixpkgs.hello.x86_64-linux".into(),
            drv_path: "/nix/store/7mdg60drrnh0wq1j8hmmbhll47czm107-hello-2.12.3.drv".into(),
            outputs: [(
                "out".to_string(),
                "/nix/store/10s5j3mfdg22k1597x580qrhprnzcjwb-hello-2.12.3".to_string(),
            )]
            .into_iter()
            .collect(),
            required_features: None,
        };
        let json = serde_json::to_value(&rec).unwrap();
        // Manifest-record field names: {job, system, attr, drvPath,
        // outputs, requiredFeatures?} — camelCase on the wire.
        assert!(json.get("drvPath").is_some());
        assert!(json.get("drv_path").is_none());
        assert!(json.get("requiredFeatures").is_none(), "omitted when None");

        let with_features = ManifestRecord {
            required_features: Some(vec!["kvm".into(), "nixos-test".into()]),
            ..rec
        };
        let json = serde_json::to_value(&with_features).unwrap();
        assert_eq!(json["requiredFeatures"][0], "kvm");
    }

    #[test]
    fn malformed_lines_are_errors_naming_the_line() {
        let err = parse_eval_jobs_line("{not json").unwrap_err();
        assert!(format!("{err:#}").contains("nix-eval-jobs"), "got: {err:#}");
        // A line with neither error nor drvPath is malformed too.
        let err = parse_eval_jobs_line(r#"{"attr":"x"}"#).unwrap_err();
        assert!(format!("{err:#}").contains("neither"), "got: {err:#}");
    }

    #[test]
    fn parse_errors_clip_overlong_lines() {
        // Malformed JSON: the error embeds a bounded snippet of the
        // line, not the full line (serde_json names the byte offset).
        let unterminated = format!(r#"{{"attr":"x","junk":"{}"#, "y".repeat(5_000));
        let err = format!("{:#}", parse_eval_jobs_line(&unterminated).unwrap_err());
        assert!(
            err.len() < 600,
            "expected a clipped snippet, got {} chars",
            err.len()
        );
        assert!(err.contains('…'), "got: {err}");

        // Valid JSON missing both `error` and `drvPath`: same clipping.
        let incomplete = format!(r#"{{"attr":"x","name":"{}"}}"#, "y".repeat(5_000));
        let err = format!("{:#}", parse_eval_jobs_line(&incomplete).unwrap_err());
        assert!(err.contains("neither"), "got: {err}");
        assert!(
            err.len() < 600,
            "expected a clipped snippet, got {} chars",
            err.len()
        );
    }
}
