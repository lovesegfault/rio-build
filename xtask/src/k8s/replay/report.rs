//! Run report for `xtask k8s replay` — the JSON summary artifact, the
//! console summary block, and the exit-code policy.
//!
//! [`Summary`] is the single source for both renderings: it is written as
//! `summary.json` into the report directory ([`write_summary_json`]) and
//! flattened into the human block printed at the end of the run
//! ([`render_console`]). [`exit_code`] maps the final [`VerdictCounts`] onto
//! the `--fail-on` policy; the orchestration turns a nonzero code into its
//! error exit AFTER the summary has been printed and written.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::FailOn;
use super::compare::VerdictCounts;
use super::prewarm::PrewarmReport;

/// Everything the run report carries. Serialized verbatim as
/// `summary.json`; the console block is a lossy view of the same data (full
/// path lists live only in the JSON).
#[derive(serde::Serialize)]
pub struct Summary {
    /// The replay archive that was driven.
    pub archive: String,
    /// Recorded window length (manifest `to` − `from`) in seconds.
    pub window_secs: f64,
    /// Wall-clock duration of the timeline phase in seconds (0 for a dry
    /// run — no timeline runs).
    pub wall_clock_secs: f64,
    /// The `--speedup` factor the schedule was built with.
    pub speedup: f64,
    /// Requests in the schedule (after `--limit`).
    pub requests_total: usize,
    /// Requests that produced an outcome (0 for a dry run).
    pub requests_replayed: usize,
    /// Derivations demoted because of recorded `impureEnvVars`
    /// (`impure-env.json` entries) — supplied instead of rebuilt.
    pub demoted_impure_drvs: usize,
    /// Whether the archive carries `builds.jsonl` (without it every build
    /// classifies as a skip).
    pub validation_enabled: bool,
    /// How long building the run-wide supply context took, in seconds.
    pub context_build_secs: f64,
    /// What the prewarm phase did; `None` when prewarm did not run
    /// (`--no-prewarm` or `--dry-run`).
    pub prewarm: Option<PrewarmReport>,
    /// Final verdict tally.
    pub counts: VerdictCounts,
    /// Worst dispatch lateness across all replayed requests, in
    /// milliseconds.
    pub max_dispatch_lateness_ms: u64,
    /// The `--fail-on` policy the exit code was computed under.
    pub fail_on: String,
    /// True for `--dry-run` (nothing was replayed).
    pub dry_run: bool,
    /// Where the run artifacts (this summary, `divergences.jsonl`) were
    /// written.
    pub report_dir: String,
}

/// Render the multi-line console summary block (no colors, no per-request
/// detail — the JSON artifact has the full lists).
pub fn render_console(summary: &Summary) -> String {
    let mut out = String::new();
    let mut line = |text: String| {
        out.push_str(&text);
        out.push('\n');
    };

    line("replay summary".to_string());
    line(format!("  archive               {}", summary.archive));
    if summary.dry_run {
        line("  mode                  dry run (nothing was sent to a cluster)".to_string());
    }
    line(format!(
        "  requests              {} replayed of {} scheduled",
        summary.requests_replayed, summary.requests_total
    ));
    line(format!(
        "  validation            {}",
        if summary.validation_enabled {
            "enabled (recorded outcomes compared)"
        } else {
            "disabled (no builds.jsonl in the archive)"
        }
    ));
    line(format!(
        "  demoted impure drvs   {}",
        summary.demoted_impure_drvs
    ));

    // Verdict buckets, one per line: matches and regressions always, the
    // rest only when non-zero. Request errors get their own line below.
    let counts = &summary.counts;
    line(format!(
        "  verdicts              {} derived paths",
        counts.total()
    ));
    let buckets: [(&str, u64, bool); 8] = [
        ("matches", counts.matches, true),
        ("regressions", counts.regressions, true),
        ("skips", counts.skips, false),
        ("non-reproducible", counts.non_reproducible, false),
        (
            "failure not reproduced",
            counts.failure_not_reproduced,
            false,
        ),
        (
            "cancellation not reproduced",
            counts.cancellation_not_reproduced,
            false,
        ),
        ("disconnect replayed", counts.disconnect_replayed, false),
        ("upload rejected", counts.upload_rejected, false),
    ];
    for (label, value, always) in buckets {
        if always || value > 0 {
            line(format!("    {label:<28}{value}"));
        }
    }
    line(format!("  divergences           {}", counts.divergences()));
    line(format!("  flaky requests        {}", counts.flaky));
    line(format!("  request errors        {}", counts.request_errors));

    if let Some(prewarm) = &summary.prewarm {
        line(format!(
            "  prewarm               uploaded {} paths ({:.1} MiB) in {:.1}s; {} upload failures, {} relay failures",
            prewarm.uploaded_paths,
            prewarm.uploaded_bytes as f64 / (1024.0 * 1024.0),
            prewarm.elapsed_secs,
            prewarm.upload_failures.len(),
            prewarm.relay_failures.len(),
        ));
        if !prewarm.skipped.is_empty() {
            line(format!(
                "    skipped             {}{}",
                prewarm.skipped.len(),
                examples(&prewarm.skipped)
            ));
        }
        if !prewarm.upload_failures.is_empty() {
            line(format!(
                "    upload failures     {}{}",
                prewarm.upload_failures.len(),
                examples(&prewarm.upload_failures)
            ));
        }
    }

    line(format!(
        "  timing                window {:.1}s, wall clock {:.1}s, speedup {}x, max dispatch lateness {}ms",
        summary.window_secs,
        summary.wall_clock_secs,
        summary.speedup,
        summary.max_dispatch_lateness_ms
    ));
    line(format!("  fail-on               {}", summary.fail_on));
    line(format!("  artifacts             {}", summary.report_dir));
    out
}

/// Up to three example paths from a `(path, reason)` list, as a ` (e.g. …)`
/// suffix; the full list lives in the JSON summary.
fn examples(pairs: &[(String, String)]) -> String {
    if pairs.is_empty() {
        return String::new();
    }
    let sample: Vec<&str> = pairs
        .iter()
        .take(3)
        .map(|(path, _)| path.as_str())
        .collect();
    format!(" (e.g. {})", sample.join(", "))
}

/// Write `summary.json` (pretty-printed) into `dir`, creating the directory
/// if needed. Returns the file's path.
pub fn write_summary_json(dir: &Path, summary: &Summary) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create report directory {}", dir.display()))?;
    let path = dir.join("summary.json");
    let json = serde_json::to_vec_pretty(summary).context("serialize the replay summary")?;
    std::fs::write(&path, json).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

/// Map the final verdict counts onto the `--fail-on` policy:
///
/// - `none`: always 0;
/// - `regression`: nonzero iff any regression, upload rejection, or
///   request-level error occurred;
/// - `divergence`: nonzero iff any divergence at all (or a request-level
///   error) occurred.
pub fn exit_code(fail_on: FailOn, counts: &VerdictCounts) -> i32 {
    let trigger = match fail_on {
        FailOn::None => 0,
        FailOn::Regression => counts.regressions + counts.upload_rejected + counts.request_errors,
        FailOn::Divergence => counts.divergences() + counts.request_errors,
    };
    if trigger > 0 { 1 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `VerdictCounts` with just the buckets the exit-code policy looks at.
    fn counts(
        regressions: u64,
        upload_rejected: u64,
        request_errors: u64,
        non_reproducible: u64,
    ) -> VerdictCounts {
        VerdictCounts {
            regressions,
            upload_rejected,
            request_errors,
            non_reproducible,
            ..VerdictCounts::default()
        }
    }

    #[test]
    fn exit_code_policy() {
        let clean = counts(0, 0, 0, 0);
        let regression = counts(1, 0, 0, 0);
        let upload_rejected = counts(0, 2, 0, 0);
        let request_error = counts(0, 0, 3, 0);
        let non_reproducible = counts(0, 0, 0, 1);

        // none: always 0, whatever happened.
        for c in [
            &clean,
            &regression,
            &upload_rejected,
            &request_error,
            &non_reproducible,
        ] {
            assert_eq!(exit_code(FailOn::None, c), 0);
        }

        // regression: regressions, upload rejections, and request errors
        // trigger; other divergences (non-reproducible) do not.
        assert_eq!(exit_code(FailOn::Regression, &clean), 0);
        assert_ne!(exit_code(FailOn::Regression, &regression), 0);
        assert_ne!(exit_code(FailOn::Regression, &upload_rejected), 0);
        assert_ne!(exit_code(FailOn::Regression, &request_error), 0);
        assert_eq!(exit_code(FailOn::Regression, &non_reproducible), 0);

        // divergence: any divergence bucket or a request error triggers.
        assert_eq!(exit_code(FailOn::Divergence, &clean), 0);
        assert_ne!(exit_code(FailOn::Divergence, &regression), 0);
        assert_ne!(exit_code(FailOn::Divergence, &upload_rejected), 0);
        assert_ne!(exit_code(FailOn::Divergence, &request_error), 0);
        assert_ne!(exit_code(FailOn::Divergence, &non_reproducible), 0);
    }

    #[test]
    fn summary_serializes_and_renders() {
        let summary = Summary {
            archive: "/tmp/replay-archive".into(),
            window_secs: 3600.0,
            wall_clock_secs: 1800.5,
            speedup: 2.0,
            requests_total: 42,
            requests_replayed: 41,
            demoted_impure_drvs: 3,
            validation_enabled: true,
            context_build_secs: 12.25,
            prewarm: Some(PrewarmReport {
                uploaded_paths: 17,
                uploaded_bytes: 5 * 1024 * 1024,
                skipped: vec![("/nix/store/aaa-skipped".into(), "no source".into())],
                upload_failures: vec![("/nix/store/bbb-failed".into(), "refused".into())],
                ..PrewarmReport::default()
            }),
            counts: VerdictCounts {
                matches: 30,
                regressions: 2,
                skips: 4,
                non_reproducible: 1,
                request_errors: 5,
                flaky: 6,
                ..VerdictCounts::default()
            },
            max_dispatch_lateness_ms: 153,
            fail_on: "regression".into(),
            dry_run: false,
            report_dir: "/tmp/replay-report".into(),
        };

        // The JSON artifact carries every field, nested structures included.
        let json = serde_json::to_value(&summary).unwrap();
        assert_eq!(json["requests_total"], 42);
        assert_eq!(json["requests_replayed"], 41);
        assert_eq!(json["counts"]["matches"], 30);
        assert_eq!(json["counts"]["regressions"], 2);
        assert_eq!(json["prewarm"]["uploaded_paths"], 17);
        assert_eq!(json["fail_on"], "regression");
        assert_eq!(json["dry_run"], false);

        // The console block carries the key numbers and locations.
        let rendered = render_console(&summary);
        for needle in [
            "/tmp/replay-archive",
            "41 replayed of 42 scheduled",
            "uploaded 17 paths (5.0 MiB)",
            "/nix/store/aaa-skipped",
            "/nix/store/bbb-failed",
            "window 3600.0s",
            "wall clock 1800.5s",
            "max dispatch lateness 153ms",
            "/tmp/replay-report",
        ] {
            assert!(
                rendered.contains(needle),
                "console block is missing {needle:?}:\n{rendered}"
            );
        }

        // Label/value lines (whitespace-aligned): match on label prefix and
        // trailing value so the assertion does not encode the padding.
        let value_of = |label: &str| -> Option<&str> {
            rendered
                .lines()
                .find(|line| line.trim_start().starts_with(label))
                .and_then(|line| line.split_whitespace().next_back())
        };
        assert_eq!(value_of("matches"), Some("30"));
        assert_eq!(value_of("regressions"), Some("2"));
        assert_eq!(value_of("skips"), Some("4"));
        assert_eq!(value_of("non-reproducible"), Some("1"));
        assert_eq!(value_of("divergences"), Some("3"));
        assert_eq!(value_of("flaky requests"), Some("6"));
        assert_eq!(value_of("request errors"), Some("5"));
        assert_eq!(value_of("demoted impure drvs"), Some("3"));

        // Zero-valued optional buckets stay out of the block.
        assert!(!rendered.contains("cancellation not reproduced"));
        assert!(!rendered.contains("upload rejected"));
    }
}
