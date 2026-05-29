//! Report rendering: summary.md, per-bucket JSONL, and progress.json.
//!
//! Rendering is a pure function of its inputs (including the
//! `generated_at` timestamp the caller supplies), so re-rendering
//! identical state produces byte-identical output — the committed golden
//! summary fixture relies on that.
//!
//! The arithmetic is not re-derived here: [`headline`] and
//! [`job_nar_verdict`] (from the classifier module) stay the single
//! sources of the headline ratio and the per-job NAR verdict, and bucket
//! names are only ever read via [`Bucket::as_str`].

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use super::classify::{Headline, NAR_DIFFERS, NAR_EQUAL, headline, job_nar_verdict};
use super::model::{Bucket, JobRecord, is_terminal_bucket};
use super::spec::{
    CampaignRecord, ComparabilityBlock, PLAN_COUNT_ATTEMPTABLE, PLAN_COUNT_IN_SCOPE,
};
use super::state::StateDir;
use super::watchdog::SuspensionSummary;

/// Aggregates derived from the latest-per-job records.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Aggregates {
    pub bucket_counts: BTreeMap<String, usize>,
    pub cascaded_counts: BTreeMap<String, usize>,
    pub signature_counts: BTreeMap<String, usize>,
    pub nar_equal: usize,
    pub nar_differs: usize,
    pub nar_compared_jobs: usize,
    pub nar_divergent_jobs: Vec<String>,
    pub first_attempt_successes: usize,
    pub multi_attempt_successes: usize,
    pub infra_rate_pct: Option<f64>,
    pub hydra_unknown_rate_pct: Option<f64>,
    pub attempted: usize,
}

/// Tally bucket/cascade/signature counts, NAR agreement, retry splits, and
/// the attempted-job rates from the latest record per job.
pub fn aggregate(records: &BTreeMap<String, JobRecord>) -> Aggregates {
    let mut agg = Aggregates::default();
    for rec in records.values() {
        *agg.bucket_counts.entry(rec.bucket.clone()).or_default() += 1;
        if rec.cascaded {
            *agg.cascaded_counts.entry(rec.bucket.clone()).or_default() += 1;
        }
        if let Some(sig) = &rec.signature {
            *agg.signature_counts.entry(sig.clone()).or_default() += 1;
        }
        if rec.bucket == Bucket::MatchBuilt.as_str() {
            if rec.attempts <= 1 {
                agg.first_attempt_successes += 1;
            } else {
                agg.multi_attempt_successes += 1;
            }
            // The per-job verdict comes from the classifier's
            // job_nar_verdict (single source of the any-differs/any-equal
            // rule); a job with no comparable output stays out of the
            // compared tally.
            match job_nar_verdict(&rec.nar_compare) {
                NAR_DIFFERS => {
                    agg.nar_differs += 1;
                    agg.nar_divergent_jobs.push(rec.job.clone());
                    agg.nar_compared_jobs += 1;
                }
                NAR_EQUAL => {
                    agg.nar_equal += 1;
                    agg.nar_compared_jobs += 1;
                }
                _ => {}
            }
        }
    }
    agg.nar_divergent_jobs.sort();
    let get = |b: Bucket| agg.bucket_counts.get(b.as_str()).copied().unwrap_or(0);
    // Attempted = jobs that produced a rio observation via submission
    // (everything except plan-time exclusions and not-attempted).
    let excluded_from_attempt = get(Bucket::Skipped)
        + get(Bucket::EvalError)
        + get(Bucket::NotAttemptable)
        + get(Bucket::NotAttempted)
        + get(Bucket::CachedPrior);
    agg.attempted = records.len().saturating_sub(excluded_from_attempt);
    if agg.attempted > 0 {
        agg.infra_rate_pct =
            Some(100.0 * get(Bucket::RioInfraFailure) as f64 / agg.attempted as f64);
    }
    let in_comparison = agg.attempted;
    if in_comparison > 0 {
        agg.hydra_unknown_rate_pct =
            Some(100.0 * get(Bucket::HydraUnknown) as f64 / in_comparison as f64);
    }
    agg
}

/// Everything the renderer needs (pure inputs → deterministic output).
#[derive(Debug, Clone)]
pub struct ReportInput<'a> {
    pub campaign: &'a CampaignRecord,
    pub records: &'a BTreeMap<String, JobRecord>,
    pub suspension: &'a SuspensionSummary,
    pub generated_at: String,
    pub partial: bool,
    pub top_n: usize,
}

fn fmt_pct(v: Option<f64>) -> String {
    match v {
        Some(p) => format!("{p:.2}%"),
        None => "n/a".to_string(),
    }
}

/// Number of jobs whose latest record sits in a terminal bucket — the one
/// "terminal" definition the report path uses (completeness, progress
/// remaining-work). Delegates to [`is_terminal_bucket`] so it can never
/// drift from the run loop's notion of terminal.
fn terminal_job_count(bucket_counts: &BTreeMap<String, usize>) -> usize {
    bucket_counts
        .iter()
        .filter(|(bucket, _)| is_terminal_bucket(bucket))
        .map(|(_, count)| *count)
        .sum()
}

/// Refresh the comparability block with final counts.
pub fn comparability_with_counts(
    base: &ComparabilityBlock,
    agg: &Aggregates,
    plan_counts: &BTreeMap<String, usize>,
) -> ComparabilityBlock {
    let mut block = base.clone();
    block.in_scope = plan_counts.get(PLAN_COUNT_IN_SCOPE).copied().unwrap_or(0);
    block.attemptable = plan_counts
        .get(PLAN_COUNT_ATTEMPTABLE)
        .copied()
        .unwrap_or(0);
    block.attempted = agg.attempted;
    let mut excluded = BTreeMap::new();
    for b in [
        Bucket::Skipped,
        Bucket::EvalError,
        Bucket::NotAttemptable,
        Bucket::NotAttempted,
        Bucket::CachedPrior,
        Bucket::RioInfraFailure,
        Bucket::UpstreamSourceUnavailable,
        Bucket::TargetSubstituted,
        Bucket::HydraUnknown,
        Bucket::EvalDivergence,
    ] {
        if let Some(n) = agg.bucket_counts.get(b.as_str()) {
            excluded.insert(b.as_str().to_string(), *n);
        }
    }
    // Merge, never overwrite: the base block carries exclusion counts the
    // job records cannot reproduce (the archive's recorder-side eval errors
    // and aggregates never become workload units), so any reason already
    // recorded there and not re-derived from bucket counts above survives
    // the refresh.
    for (reason, count) in &base.excluded {
        excluded.entry(reason.clone()).or_insert(*count);
    }
    block.excluded = excluded;
    let terminal = terminal_job_count(&agg.bucket_counts);
    block.completeness_pct = if block.in_scope > 0 {
        100.0 * terminal as f64 / block.in_scope as f64
    } else {
        0.0
    };
    block
}

/// Render summary.md. Deterministic for identical inputs.
pub fn render_summary(input: &ReportInput<'_>) -> String {
    let agg = aggregate(input.records);
    let head: Headline = headline(&agg.bucket_counts, agg.nar_equal, agg.nar_compared_jobs);
    let empty_counts = BTreeMap::new();
    let plan_counts = input
        .campaign
        .plan
        .as_ref()
        .map(|p| &p.counts)
        .unwrap_or(&empty_counts);
    let block = comparability_with_counts(&input.campaign.comparability, &agg, plan_counts);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# Parity campaign {} — summary",
        input.campaign.campaign_id
    );
    if input.partial {
        let _ = writeln!(
            out,
            "\n> **PARTIAL REPORT** — the campaign hit its deadline or was aborted before draining."
        );
    }
    let _ = writeln!(out, "\nGenerated: {}\n", input.generated_at);
    let _ = writeln!(out, "## Comparability");
    let _ = writeln!(out, "| field | value |");
    let _ = writeln!(out, "|---|---|");
    let _ = writeln!(out, "| eval set | {} |", block.eval_set);
    let _ = writeln!(out, "| manifest sha256 | {} |", block.manifest_sha256);
    let _ = writeln!(out, "| mode | {} |", block.mode);
    let _ = writeln!(out, "| build tenant | {} |", block.build_tenant);
    let _ = writeln!(out, "| systems | {} |", block.filters.systems.join(", "));
    let _ = writeln!(
        out,
        "| exclude features | {} |",
        block.filters.exclude_features.join(", ")
    );
    let _ = writeln!(
        out,
        "| include globs | {} |",
        block.filters.include_globs.join(", ")
    );
    let _ = writeln!(
        out,
        "| limit | {} |",
        block
            .filters
            .limit
            .map(|l| l.to_string())
            .unwrap_or_else(|| "none".into())
    );
    let _ = writeln!(out, "| engine version | {} |", block.engine_version);
    let _ = writeln!(
        out,
        "| signature table | {} |",
        block.signature_table_version
    );
    let _ = writeln!(
        out,
        "| in scope / attemptable / attempted | {} / {} / {} |",
        block.in_scope, block.attemptable, block.attempted
    );
    let _ = writeln!(out, "| completeness | {:.2}% |", block.completeness_pct);
    if !block.low_confidence.is_empty() {
        let _ = writeln!(
            out,
            "| **low confidence** | {} |",
            block.low_confidence.join(", ")
        );
    }
    let _ = writeln!(out, "\n## Headline");
    let _ = writeln!(
        out,
        "- Build-outcome parity: **{}** ({} / {})",
        fmt_pct(head.headline_pct),
        head.numerator,
        head.denominator
    );
    let _ = writeln!(
        out,
        "- NAR-hash agreement (secondary, non-gating): {} ({} / {} compared jobs)",
        fmt_pct(head.nar_agreement_pct),
        head.nar_equal,
        head.nar_compared
    );
    let _ = writeln!(
        out,
        "- Infra-failure rate (excluded from headline): {}",
        fmt_pct(agg.infra_rate_pct)
    );
    let _ = writeln!(
        out,
        "- Hydra-unknown rate: {}",
        fmt_pct(agg.hydra_unknown_rate_pct)
    );
    let _ = writeln!(out, "\n## Buckets");
    let _ = writeln!(out, "| bucket | count | of which cascaded |");
    let _ = writeln!(out, "|---|---:|---:|");
    for bucket in Bucket::ALL {
        let count = agg.bucket_counts.get(bucket.as_str()).copied().unwrap_or(0);
        if count == 0 {
            continue;
        }
        let cascaded = agg
            .cascaded_counts
            .get(bucket.as_str())
            .copied()
            .unwrap_or(0);
        let cascaded = if cascaded > 0 {
            cascaded.to_string()
        } else {
            String::new()
        };
        let _ = writeln!(out, "| {} | {} | {} |", bucket.as_str(), count, cascaded);
    }
    let _ = writeln!(out, "\n## Top failure signatures");
    let _ = writeln!(
        out,
        "Signatures group byte-identical raw evidence (60-character message slugs); the same \
         failure mode worded differently appears as separate rows, so these are NOT \
         failure-mode counts."
    );
    let mut sigs: Vec<(&String, &usize)> = agg.signature_counts.iter().collect();
    sigs.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    if sigs.is_empty() {
        let _ = writeln!(out, "(none)");
    } else if sigs.len() > input.top_n {
        let _ = writeln!(
            out,
            "Showing the top {} of {} distinct signatures.",
            input.top_n,
            sigs.len()
        );
    }
    for (sig, count) in sigs.into_iter().take(input.top_n) {
        let _ = writeln!(out, "- `{sig}`: {count}");
    }
    let _ = writeln!(out, "\n## NAR divergence top offenders");
    if agg.nar_divergent_jobs.is_empty() {
        let _ = writeln!(out, "(none)");
    } else if agg.nar_divergent_jobs.len() > input.top_n {
        let _ = writeln!(
            out,
            "Showing the first {} of {} divergent jobs.",
            input.top_n,
            agg.nar_divergent_jobs.len()
        );
    }
    for job in agg.nar_divergent_jobs.iter().take(input.top_n) {
        let _ = writeln!(out, "- {job}");
    }
    let _ = writeln!(out, "\n## Retries");
    let _ = writeln!(
        out,
        "- match-built on first attempt: {} | after retries: {}",
        agg.first_attempt_successes, agg.multi_attempt_successes
    );
    let _ = writeln!(out, "\n## Suspension windows");
    if input.suspension.windows.is_empty() {
        let _ = writeln!(out, "(none)");
    } else {
        for (component, secs) in &input.suspension.total_secs_by_component {
            let _ = writeln!(out, "- {component}: {secs:.0}s total");
        }
        let _ = writeln!(out, "- windows: {}", input.suspension.windows.len());
    }
    let _ = writeln!(out, "\n## Artifacts");
    let _ = writeln!(
        out,
        "- results.jsonl, hydra.jsonl, supply.jsonl, dispatch.jsonl, batches.jsonl, \
         buckets/<bucket>.jsonl, logs/<job>.log.zst next to this file"
    );
    out
}

/// progress.json: stage, per-bucket counts, rates, suspension windows, ETA.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub campaign_id: String,
    pub stage: String,
    pub updated_at: String,
    pub bucket_counts: BTreeMap<String, usize>,
    pub attempted: usize,
    pub infra_rate_pct: Option<f64>,
    pub hydra_unknown_rate_pct: Option<f64>,
    pub jobs_per_hour: Option<f64>,
    pub eta_hours: Option<f64>,
    pub suspension: SuspensionSummary,
    pub comparability: ComparabilityBlock,
}

/// Build the progress.json document from the current records and stage.
pub fn build_progress(
    campaign: &CampaignRecord,
    records: &BTreeMap<String, JobRecord>,
    suspension: &SuspensionSummary,
    stage: &str,
    updated_at: String,
    jobs_per_hour: Option<f64>,
) -> Progress {
    let agg = aggregate(records);
    let empty_counts = BTreeMap::new();
    let plan_counts = campaign
        .plan
        .as_ref()
        .map(|p| &p.counts)
        .unwrap_or(&empty_counts);
    let block = comparability_with_counts(&campaign.comparability, &agg, plan_counts);
    let terminal = terminal_job_count(&agg.bucket_counts);
    let remaining = block.in_scope.saturating_sub(terminal);
    Progress {
        campaign_id: campaign.campaign_id.clone(),
        stage: stage.to_string(),
        updated_at,
        bucket_counts: agg.bucket_counts.clone(),
        attempted: agg.attempted,
        infra_rate_pct: agg.infra_rate_pct,
        hydra_unknown_rate_pct: agg.hydra_unknown_rate_pct,
        jobs_per_hour,
        // ETA is undefined when the plan has no in-scope work at all — a
        // "0h remaining" figure for an empty campaign would be misleading.
        eta_hours: jobs_per_hour
            .filter(|jph| *jph > 0.0 && block.in_scope > 0)
            .map(|jph| remaining as f64 / jph),
        suspension: suspension.clone(),
        comparability: block,
    }
}

/// Write summary.md + `buckets/<bucket>.jsonl` into the state dir and
/// return the rendered summary text.
pub fn write_report(state: &StateDir, input: &ReportInput<'_>) -> Result<String> {
    let summary = render_summary(input);
    state.write_bytes("report/summary.md", summary.as_bytes())?;
    // Per-bucket JSONL. The report stage owns buckets/: drop files from a
    // previous render first, so a job that since moved buckets cannot
    // linger in its old file.
    let buckets_dir = state.path("buckets");
    if buckets_dir.exists() {
        for entry in std::fs::read_dir(&buckets_dir)
            .with_context(|| format!("list {}", buckets_dir.display()))?
        {
            let path = entry
                .with_context(|| format!("list {}", buckets_dir.display()))?
                .path();
            if path.extension().is_some_and(|ext| ext == "jsonl") {
                std::fs::remove_file(&path)
                    .with_context(|| format!("remove stale {}", path.display()))?;
            }
        }
    }
    let mut by_bucket: BTreeMap<String, Vec<&JobRecord>> = BTreeMap::new();
    for rec in input.records.values() {
        by_bucket.entry(rec.bucket.clone()).or_default().push(rec);
    }
    for (bucket, recs) in by_bucket {
        let path = state.path(&format!("buckets/{bucket}.jsonl"));
        let file =
            std::fs::File::create(&path).with_context(|| format!("create {}", path.display()))?;
        let mut writer = std::io::BufWriter::new(file);
        for rec in recs {
            serde_json::to_writer(&mut writer, rec)
                .with_context(|| format!("write job record to {}", path.display()))?;
            writer
                .write_all(b"\n")
                .with_context(|| format!("write {}", path.display()))?;
        }
        writer
            .flush()
            .with_context(|| format!("flush {}", path.display()))?;
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::model::{HydraSide, RioSide};

    fn rec(
        job: &str,
        bucket: Bucket,
        attempts: u32,
        signature: Option<&str>,
        cascaded: bool,
    ) -> JobRecord {
        JobRecord {
            job: job.into(),
            system: "x86_64-linux".into(),
            drv_path: format!("/nix/store/{}-{job}.drv", "a".repeat(32)),
            mode: "leaf".into(),
            attempts,
            build_ids: vec![],
            rio: RioSide::default(),
            hydra: HydraSide::default(),
            nar_compare: BTreeMap::new(),
            bucket: bucket.as_str().into(),
            cascaded,
            signature: signature.map(String::from),
            log_key: None,
            repro: String::new(),
            evidence: None,
            updated_at: "2026-05-26T00:00:00Z".into(),
        }
    }

    #[test]
    fn aggregate_counts_buckets_signatures_and_rates() {
        let mut records = BTreeMap::new();
        for (i, b) in [
            Bucket::MatchBuilt,
            Bucket::MatchBuilt,
            Bucket::RioOnlyFailure,
            Bucket::RioInfraFailure,
            Bucket::CachedPrior,
            Bucket::NotAttempted,
        ]
        .into_iter()
        .enumerate()
        {
            let sig = (b == Bucket::RioOnlyFailure).then_some("poison-threshold");
            records.insert(format!("j{i}"), rec(&format!("j{i}"), b, 1, sig, false));
        }
        records.insert(
            "casc".into(),
            rec("casc", Bucket::RioInfraFailure, 1, None, true),
        );
        let agg = aggregate(&records);
        assert_eq!(agg.bucket_counts["match-built"], 2);
        assert_eq!(agg.bucket_counts["rio-infra-failure"], 2);
        assert_eq!(agg.cascaded_counts["rio-infra-failure"], 1);
        assert_eq!(agg.signature_counts["poison-threshold"], 1);
        // attempted = total(7) - cached-prior(1) - not-attempted(1) = 5
        assert_eq!(agg.attempted, 5);
        assert!((agg.infra_rate_pct.unwrap() - 40.0).abs() < 1e-9);
    }

    #[test]
    fn render_is_deterministic_and_marks_partial() {
        let spec: crate::run::spec::CampaignSpec =
            serde_json::from_str(r#"{"mode":"leaf"}"#).unwrap();
        let campaign = CampaignRecord::new(
            "c-test".into(),
            "2026-05-26T00:00:00Z".into(),
            spec,
            crate::run::spec::ArchivePin {
                archive_id: "ab".repeat(32),
                archive_id_short: "ab".repeat(8),
            },
        );
        let mut records = BTreeMap::new();
        records.insert("a".into(), rec("a", Bucket::MatchBuilt, 1, None, false));
        let suspension = SuspensionSummary::default();
        let input = ReportInput {
            campaign: &campaign,
            records: &records,
            suspension: &suspension,
            generated_at: "2026-05-26T12:00:00Z".into(),
            partial: true,
            top_n: 5,
        };
        let one = render_summary(&input);
        let two = render_summary(&input);
        assert_eq!(one, two, "byte-identical re-render");
        assert!(one.contains("PARTIAL REPORT"));
        assert!(one.contains("Build-outcome parity"));
        assert!(one.contains(&format!("| eval set | {} |", "ab".repeat(8))));
    }

    #[test]
    fn progress_includes_eta_and_stage() {
        let spec: crate::run::spec::CampaignSpec =
            serde_json::from_str(r#"{"mode":"leaf"}"#).unwrap();
        let mut campaign = CampaignRecord::new(
            "c-test".into(),
            "2026-05-26T00:00:00Z".into(),
            spec,
            crate::run::spec::ArchivePin {
                archive_id: "ab".repeat(32),
                archive_id_short: "ab".repeat(8),
            },
        );
        campaign.plan = Some(crate::run::spec::PlanOutput {
            counts: BTreeMap::from([
                ("inScope".to_string(), 10usize),
                ("attemptable".to_string(), 8usize),
            ]),
            ..Default::default()
        });
        let mut records = BTreeMap::new();
        records.insert("a".into(), rec("a", Bucket::MatchBuilt, 1, None, false));
        let p = build_progress(
            &campaign,
            &records,
            &SuspensionSummary::default(),
            "submit+collect",
            "2026-05-26T01:00:00Z".into(),
            Some(2.0),
        );
        assert_eq!(p.stage, "submit+collect");
        assert_eq!(p.comparability.in_scope, 10);
        assert!(
            (p.eta_hours.unwrap() - 4.5).abs() < 1e-9,
            "remaining 9 / 2 per hour"
        );
    }

    #[test]
    fn eta_is_none_when_nothing_is_in_scope() {
        // A campaign whose plan has zero in-scope jobs must not render a
        // misleading "0h remaining" ETA even when a throughput figure exists.
        let spec: crate::run::spec::CampaignSpec =
            serde_json::from_str(r#"{"mode":"leaf"}"#).unwrap();
        let campaign = CampaignRecord::new(
            "c-empty".into(),
            "2026-05-26T00:00:00Z".into(),
            spec,
            crate::run::spec::ArchivePin::default(),
        );
        let p = build_progress(
            &campaign,
            &BTreeMap::new(),
            &SuspensionSummary::default(),
            "submit+collect",
            "2026-05-26T01:00:00Z".into(),
            Some(2.0),
        );
        assert_eq!(p.comparability.in_scope, 0);
        assert_eq!(p.eta_hours, None);
    }

    #[test]
    fn truncated_lists_carry_a_showing_n_of_m_note() {
        let spec: crate::run::spec::CampaignSpec =
            serde_json::from_str(r#"{"mode":"leaf"}"#).unwrap();
        let campaign = CampaignRecord::new(
            "c-trunc".into(),
            "2026-05-26T00:00:00Z".into(),
            spec,
            crate::run::spec::ArchivePin::default(),
        );
        // Three distinct signatures and three NAR-divergent jobs, rendered
        // with top_n = 2 → both lists are truncated and must say so.
        let mut records = BTreeMap::new();
        for (i, sig) in ["sig-a", "sig-b", "sig-c"].iter().enumerate() {
            records.insert(
                format!("fail{i}"),
                rec(
                    &format!("fail{i}"),
                    Bucket::RioOnlyFailure,
                    1,
                    Some(sig),
                    false,
                ),
            );
        }
        for i in 0..3 {
            let mut r = rec(&format!("div{i}"), Bucket::MatchBuilt, 1, None, false);
            r.nar_compare
                .insert("out".to_string(), crate::run::classify::NAR_DIFFERS.into());
            records.insert(format!("div{i}"), r);
        }
        let suspension = SuspensionSummary::default();
        let input = ReportInput {
            campaign: &campaign,
            records: &records,
            suspension: &suspension,
            generated_at: "2026-05-26T12:00:00Z".into(),
            partial: false,
            top_n: 2,
        };
        let out = render_summary(&input);
        assert!(
            out.contains("Showing the top 2 of 3 distinct signatures."),
            "{out}"
        );
        assert!(
            out.contains("Showing the first 2 of 3 divergent jobs."),
            "{out}"
        );
        // Untruncated lists (top_n large enough) carry no note.
        let input = ReportInput {
            top_n: 20,
            ..input.clone()
        };
        let out = render_summary(&input);
        assert!(!out.contains("Showing the"), "{out}");
    }

    #[test]
    fn write_report_owns_the_buckets_dir() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let spec: crate::run::spec::CampaignSpec =
            serde_json::from_str(r#"{"mode":"leaf"}"#).unwrap();
        let campaign = CampaignRecord::new(
            "c-test".into(),
            "2026-05-26T00:00:00Z".into(),
            spec,
            crate::run::spec::ArchivePin::default(),
        );
        let suspension = SuspensionSummary::default();

        // First render: the job is still not-attempted.
        let mut records = BTreeMap::new();
        records.insert("a".into(), rec("a", Bucket::NotAttempted, 0, None, false));
        let input = ReportInput {
            campaign: &campaign,
            records: &records,
            suspension: &suspension,
            generated_at: "2026-05-26T12:00:00Z".into(),
            partial: true,
            top_n: 5,
        };
        write_report(&state, &input).unwrap();
        assert!(state.path("buckets/not-attempted.jsonl").exists());
        assert!(state.path("report/summary.md").exists());

        // Second render: the job moved to match-built — its old bucket file
        // must not linger.
        let mut records = BTreeMap::new();
        records.insert("a".into(), rec("a", Bucket::MatchBuilt, 1, None, false));
        let input = ReportInput {
            campaign: &campaign,
            records: &records,
            suspension: &suspension,
            generated_at: "2026-05-26T13:00:00Z".into(),
            partial: false,
            top_n: 5,
        };
        write_report(&state, &input).unwrap();
        assert!(state.path("buckets/match-built.jsonl").exists());
        assert!(
            !state.path("buckets/not-attempted.jsonl").exists(),
            "stale bucket file from the previous render must be removed"
        );
    }
}
