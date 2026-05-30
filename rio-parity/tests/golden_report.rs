//! Golden report test: a canned results.jsonl must aggregate to exact
//! verdict/disposition counts and render to a stable summary.md, and
//! re-rendering the same input must be byte-identical. The frozen
//! pre-cutover fixture (legacy-results.jsonl) additionally proves the
//! legacy-bucket → verdict/disposition rename count-preserving.
//! Regenerate the golden summary with:
//!   BLESS=1 nix develop -c cargo nextest run -p rio-parity -E 'test(golden_summary_matches)'

use std::collections::BTreeMap;
use std::path::PathBuf;

use rio_parity::run::model::{JobRecord, unified_from_legacy_bucket};
use rio_parity::run::report::{ReportInput, aggregate, render_summary};
use rio_parity::run::spec::CampaignRecord;
use rio_parity::run::watchdog::SuspensionSummary;

/// Crate directory at *runtime* (not `env!()`): under nextest
/// `--workspace-remap` the compile-time path is a per-crate build sandbox
/// that no longer exists when the test binary runs.
fn manifest_dir() -> PathBuf {
    PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo/nextest"),
    )
}

fn fixtures() -> (CampaignRecord, BTreeMap<String, JobRecord>) {
    let dir = manifest_dir().join("tests/fixtures/golden");
    let campaign: CampaignRecord =
        serde_json::from_str(&std::fs::read_to_string(dir.join("campaign.json")).unwrap()).unwrap();
    let mut records = BTreeMap::new();
    for line in std::fs::read_to_string(dir.join("results.jsonl"))
        .unwrap()
        .lines()
    {
        if line.trim().is_empty() {
            continue;
        }
        let rec: JobRecord = serde_json::from_str(line).unwrap();
        records.insert(rec.job.clone(), rec);
    }
    (campaign, records)
}

#[test]
fn golden_bucket_counts() {
    let (_campaign, records) = fixtures();
    let agg = aggregate(&records);
    let verdict = |v: &str| agg.verdict_counts.get(v).copied().unwrap_or(0);
    let disposition = |d: &str| agg.disposition_counts.get(d).copied().unwrap_or(0);
    assert_eq!(records.len(), 12);
    assert_eq!(verdict("match-built"), 1);
    assert_eq!(verdict("output-divergence"), 1);
    assert_eq!(verdict("unexpected-failure"), 1);
    assert_eq!(verdict("unexpected-dependency-failure"), 1);
    assert_eq!(verdict("infra-indeterminate"), 2);
    assert_eq!(
        agg.cascaded_counts
            .get("infra-indeterminate")
            .copied()
            .unwrap_or(0),
        1
    );
    assert_eq!(verdict("unexpected-success"), 1);
    assert_eq!(verdict("no-truth"), 1);
    assert_eq!(disposition("cached-prior"), 1);
    assert_eq!(disposition("not-attemptable"), 1);
    // The timed-only interruption verdicts are counted like any other
    // verdict and stay out of the headline.
    assert_eq!(verdict("interruption-replayed"), 1);
    assert_eq!(verdict("interruption-not-reproduced"), 1);
    // Headline: (1 match-built + 1 output-divergence) / (2 + 1 + 1) = 50%.
    let head = rio_parity::run::classify::headline(
        &agg.verdict_counts,
        agg.nar_equal,
        agg.nar_compared_jobs,
    );
    assert_eq!(head.denominator, 4);
    assert_eq!(head.numerator, 2);
    // NAR: one equal, one differs among the compared match-class jobs.
    assert_eq!(
        (agg.nar_equal, agg.nar_differs, agg.nar_compared_jobs),
        (1, 1, 2)
    );
}

/// The frozen pre-cutover results file maps onto the unified vocabulary
/// count-preservingly: every legacy record lands in exactly one verdict or
/// disposition, totals are preserved, and each legacy bucket's count
/// reappears under its mapped name (with the one data-dependent split:
/// `match-built` records whose narCompare carries a `differs` entry become
/// `output-divergence`). The legacy fixture stays byte-identical to the
/// pre-cutover artifact forever, old tenant/prefix strings included.
#[test]
fn legacy_results_render_count_preserving() {
    let path = manifest_dir().join("tests/fixtures/golden/legacy-results.jsonl");
    let mut legacy_total = 0usize;
    let mut legacy_by_bucket: BTreeMap<String, usize> = BTreeMap::new();
    let mut unified: BTreeMap<String, usize> = BTreeMap::new();
    for line in std::fs::read_to_string(&path).unwrap().lines() {
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        let bucket = value["bucket"]
            .as_str()
            .expect("legacy record has a bucket");
        let nar_differs = value["narCompare"]
            .as_object()
            .is_some_and(|outputs| outputs.values().any(|v| v == "differs"));
        let class = unified_from_legacy_bucket(bucket, nar_differs)
            .expect("every legacy bucket maps onto the unified vocabulary");
        legacy_total += 1;
        *legacy_by_bucket.entry(bucket.to_string()).or_default() += 1;
        *unified.entry(class.as_str().to_string()).or_default() += 1;
    }
    // Count preservation: nothing dropped, nothing double-counted.
    assert_eq!(legacy_total, 12);
    assert_eq!(unified.values().sum::<usize>(), legacy_total);
    // Name-for-name: each legacy bucket's count reappears under its mapped
    // name. The two match-built records split 1/1 because exactly one of
    // them carries a differs narCompare entry.
    assert_eq!(legacy_by_bucket["match-built"], 2);
    let expected: BTreeMap<&str, usize> = BTreeMap::from([
        ("match-built", 1),
        ("output-divergence", 1),
        ("unexpected-failure", 1),
        ("unexpected-dependency-failure", 1),
        ("infra-indeterminate", 2),
        ("unexpected-success", 1),
        ("no-truth", 1),
        ("cached-prior", 1),
        ("not-attemptable", 1),
        ("interruption-replayed", 1),
        ("interruption-not-reproduced", 1),
    ]);
    let unified_ref: BTreeMap<&str, usize> =
        unified.iter().map(|(k, v)| (k.as_str(), *v)).collect();
    assert_eq!(unified_ref, expected);
}

#[test]
fn golden_summary_matches_and_rerenders_identically() {
    let (campaign, records) = fixtures();
    let suspension = SuspensionSummary::default();
    let input = ReportInput {
        campaign: &campaign,
        records: &records,
        suspension: &suspension,
        generated_at: "2026-05-26T12:00:00Z".to_string(),
        partial: false,
        top_n: 20,
        supply: None,
        timed: None,
        abort_recommended: false,
        plan_rss_mib: None,
        plan_rss_peak_mib: None,
    };
    let rendered = render_summary(&input);
    let rendered_again = render_summary(&input);
    assert_eq!(rendered, rendered_again, "byte-identical re-render");

    let golden_path = manifest_dir().join("tests/fixtures/golden/summary.md");
    if std::env::var("BLESS").is_ok() {
        std::fs::write(&golden_path, &rendered).unwrap();
    }
    let golden = std::fs::read_to_string(&golden_path)
        .expect("golden summary.md missing — run once with BLESS=1 and commit it");
    assert_eq!(
        rendered, golden,
        "summary.md drifted from the golden fixture (BLESS=1 to regenerate)"
    );
}
