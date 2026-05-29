//! Golden report test: a canned results.jsonl must aggregate to exact
//! bucket counts and render to a stable summary.md, and re-rendering the
//! same input must be byte-identical. Regenerate the golden summary with:
//!   BLESS=1 nix develop -c cargo nextest run -p rio-parity -E 'test(golden_summary_matches)'

use std::collections::BTreeMap;
use std::path::PathBuf;

use rio_parity::run::model::JobRecord;
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
    let expect = |b: &str| agg.bucket_counts.get(b).copied().unwrap_or(0);
    assert_eq!(records.len(), 12);
    assert_eq!(expect("match-built"), 2);
    assert_eq!(expect("rio-only-failure"), 1);
    assert_eq!(expect("rio-dependency-failure"), 1);
    assert_eq!(expect("rio-infra-failure"), 2);
    assert_eq!(
        agg.cascaded_counts
            .get("rio-infra-failure")
            .copied()
            .unwrap_or(0),
        1
    );
    assert_eq!(expect("hydra-only-failure"), 1);
    assert_eq!(expect("cached-prior"), 1);
    assert_eq!(expect("not-attemptable"), 1);
    assert_eq!(expect("hydra-unknown"), 1);
    // The timed-only interruption verdicts are counted like any other
    // bucket and stay out of the headline.
    assert_eq!(expect("interruption-replayed"), 1);
    assert_eq!(expect("interruption-not-reproduced"), 1);
    // Headline: 2 match-built / (2 + 1 + 1) = 50%.
    let head = rio_parity::run::classify::headline(
        &agg.bucket_counts,
        agg.nar_equal,
        agg.nar_compared_jobs,
    );
    assert_eq!(head.denominator, 4);
    assert_eq!(head.numerator, 2);
    // NAR: one equal, one differs among compared match-built jobs.
    assert_eq!(
        (agg.nar_equal, agg.nar_differs, agg.nar_compared_jobs),
        (1, 1, 2)
    );
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
