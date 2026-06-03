//! Report rendering: summary.md, gate.json, per-class JSONL files, progress.json.
//!
//! Each distinct verdict or disposition gets its own JSONL file under
//! `buckets/`, named by the class's wire string.
//!
//! Rendering is a pure function of its inputs (including the
//! `generated_at` timestamp the caller supplies), so re-rendering
//! identical state produces byte-identical output — the committed golden
//! summary fixture relies on that.
//!
//! The arithmetic is not re-derived here: [`headline`] and
//! [`job_nar_verdict`] (from the classifier module) stay the single
//! sources of the headline ratio and the per-job NAR verdict, and
//! verdict/disposition names are only ever read via [`Verdict::as_str`] /
//! [`Disposition::as_str`].

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use super::classify::{Headline, NAR_DIFFERS, NAR_EQUAL, headline, job_nar_verdict};
use super::model::{Disposition, GateAccounting, JobRecord, Verdict};
use super::spec::{
    CampaignRecord, ComparabilityBlock, FailOn, Knobs, PLAN_COUNT_ATTEMPTABLE, PLAN_COUNT_IN_SCOPE,
    ReportPolicy,
};
use super::state::StateDir;
use super::supply::exec::SupplyStageReport;
use super::timeline::TimedRunStats;
use super::watchdog::SuspensionSummary;

/// Aggregates derived from the latest-per-job records.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Aggregates {
    pub verdict_counts: BTreeMap<String, usize>,
    pub disposition_counts: BTreeMap<String, usize>,
    /// Cascaded-dependent counts, keyed by the verdict string (only
    /// verdict-carrying records can be cascaded).
    pub cascaded_counts: BTreeMap<String, usize>,
    pub signature_counts: BTreeMap<String, usize>,
    pub nar_equal: usize,
    pub nar_differs: usize,
    pub nar_compared_jobs: usize,
    pub nar_divergent_jobs: Vec<String>,
    pub first_attempt_successes: usize,
    pub multi_attempt_successes: usize,
    pub infra_rate_pct: Option<f64>,
    pub no_truth_rate_pct: Option<f64>,
    pub attempted: usize,
}

/// Tally verdict/disposition/cascade/signature counts, NAR agreement,
/// retry splits, and the attempted-job rates from the latest record per
/// job.
pub fn aggregate(records: &BTreeMap<String, JobRecord>) -> Aggregates {
    let mut agg = Aggregates::default();
    for rec in records.values() {
        // A classified record carries exactly one of verdict/disposition;
        // an unclassified record (neither set) enters neither count map.
        if let Some(verdict) = &rec.verdict {
            *agg.verdict_counts.entry(verdict.clone()).or_default() += 1;
            if rec.cascaded {
                *agg.cascaded_counts.entry(verdict.clone()).or_default() += 1;
            }
        } else if let Some(disposition) = &rec.disposition {
            *agg.disposition_counts
                .entry(disposition.clone())
                .or_default() += 1;
        }
        if let Some(sig) = &rec.signature {
            *agg.signature_counts.entry(sig.clone()).or_default() += 1;
        }
        let verdict_is = |v: Verdict| rec.verdict.as_deref() == Some(v.as_str());
        if verdict_is(Verdict::MatchBuilt) || verdict_is(Verdict::OutputDivergence) {
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
    let verdict = |v: Verdict| agg.verdict_counts.get(v.as_str()).copied().unwrap_or(0);
    let disposition = |d: Disposition| agg.disposition_counts.get(d.as_str()).copied().unwrap_or(0);
    // Attempted = jobs that produced a rio observation via submission. The
    // not-attempted property is the enum's own exhaustive method, never a
    // hand-enumerated subset here: a new disposition cannot ship without
    // deciding its attempted-ness, and this sum picks the decision up
    // automatically.
    let excluded_from_attempt: usize = Disposition::ALL
        .iter()
        .filter(|d| !d.attempted())
        .map(|d| disposition(*d))
        .sum();
    agg.attempted = records.len().saturating_sub(excluded_from_attempt);
    if agg.attempted > 0 {
        agg.infra_rate_pct =
            Some(100.0 * verdict(Verdict::InfraIndeterminate) as f64 / agg.attempted as f64);
        agg.no_truth_rate_pct =
            Some(100.0 * verdict(Verdict::NoTruth) as f64 / agg.attempted as f64);
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
    /// Supply-stage summary read back from `supply-report.json`; `None` for
    /// campaign states that never ran the supply stage.
    pub supply: Option<&'a SupplyStageReport>,
    /// Timed-dispatch summary read back from `timed-stats.json`; `None` for
    /// timeless campaigns.
    pub timed: Option<&'a TimedRunStats>,
    /// Latched abort recommendation (timed mode only: the infra-failure rate
    /// crossed the pause threshold but pausing would distort the recorded
    /// cadence, so the operator is asked to decide instead).
    pub abort_recommended: bool,
    /// Plan-stage resident-set size before the closure-graph load, in MiB.
    pub plan_rss_mib: Option<u64>,
    /// Process peak resident-set size after the closure/overlap computation,
    /// in MiB.
    pub plan_rss_peak_mib: Option<u64>,
    /// Recorder-side exclusion counts, re-derived from the ARCHIVE by the
    /// caller (`archive_input::exclusion_counts`) — never read back from a
    /// previously persisted comparability block, so a stale derived count
    /// in campaign.json cannot masquerade as recorder truth (see
    /// [`comparability_with_counts`]).
    pub recorder_excluded: &'a BTreeMap<String, usize>,
}

fn fmt_pct(v: Option<f64>) -> String {
    match v {
        Some(p) => format!("{p:.2}%"),
        None => "n/a".to_string(),
    }
}

/// Number of IN-SCOPE jobs whose latest record carries a terminal class —
/// the one "terminal" definition the report path uses (completeness,
/// progress remaining-work). Every verdict counts (verdicts only ever come
/// from submitted, in-scope jobs); a disposition counts when it is
/// terminal (`not-attempted` may still be attempted by a resume) AND its
/// records describe in-scope jobs ([`Disposition::in_scope_population`] —
/// `filtered` records are plan-time bookkeeping for jobs the in-scope
/// denominator excludes, so counting them yields >100% completeness on
/// any filtered campaign). A record with neither field set entered
/// neither count map and so never counts; a disposition string outside
/// the engine's vocabulary is counted defensively (it cannot be proven
/// out of population).
fn terminal_job_count(
    verdict_counts: &BTreeMap<String, usize>,
    disposition_counts: &BTreeMap<String, usize>,
) -> usize {
    verdict_counts.values().sum::<usize>()
        + disposition_counts
            .iter()
            .filter(|(class, _)| {
                match Disposition::ALL
                    .iter()
                    .find(|d| d.as_str() == class.as_str())
                {
                    Some(d) => *d != Disposition::NotAttempted && d.in_scope_population(),
                    None => true,
                }
            })
            .map(|(_, count)| *count)
            .sum::<usize>()
}

/// The completeness ratio: in-scope terminal jobs over the in-scope
/// denominator, as a percentage.
///
/// Population invariant: [`terminal_job_count`] counts only records whose
/// class belongs to the in-scope population, so on engine-written state
/// the numerator can never exceed the denominator. The clamp makes a
/// figure above 100% unrepresentable in every emitted artifact even over
/// state this engine did not write (foreign or hand-edited results.jsonl,
/// records for jobs a re-plan no longer scopes); a violation is loud, not
/// silent, because it means some record class leaked across populations.
fn completeness_pct(terminal: usize, in_scope: usize) -> f64 {
    if in_scope == 0 {
        return 0.0;
    }
    if terminal > in_scope {
        tracing::warn!(
            terminal,
            in_scope,
            "more in-scope terminal records than in-scope jobs; a record class has leaked \
             across the scope populations — clamping completeness to 100%"
        );
        return 100.0;
    }
    100.0 * terminal as f64 / in_scope as f64
}

/// Low-confidence flag: the infra-indeterminate rate exceeded
/// `knobs.infra_low_confidence_pct`.
pub const FLAG_INFRA_INDETERMINATE_RATE: &str = "infra-indeterminate-rate";
/// Low-confidence flag: the no-truth rate exceeded
/// `knobs.no_truth_threshold_pct`.
pub const FLAG_NO_TRUTH_RATE: &str = "no-truth-rate";
/// Low-confidence flag: the supply stage recorded a nonzero prefetch
/// shortfall (missing or unavailable prefetch-wanted paths), so the
/// headline measured something other than what was planned.
pub const FLAG_PREFETCH_SHORTFALL: &str = "prefetch-shortfall";
/// Low-confidence flag: a timed run's recorded cadence was not honored
/// (resume re-anchoring or a suspension window during timed execution).
pub const FLAG_TIMING_DEGRADED: &str = "timing-degraded";
/// Low-confidence flag (set at plan time): tenant upstreams unverified — `replay dev`
/// runs, and `replay repro` of campaigns whose original record had unverified tenants.
pub const FLAG_TENANT_UPSTREAMS_UNVERIFIED: &str = "tenant-upstreams-unverified";
/// Low-confidence flag (set at bootstrap): interruption replay was requested
/// but the archive records no cancellations or client disconnects, so the
/// knob was forced off for this campaign.
pub const FLAG_REPLAY_INTERRUPTIONS_DISABLED: &str = "replay-interruptions-disabled";
/// Low-confidence flag: at least one unit retired `supply-failed` — the
/// engine could not obtain or deliver required supply for reasons not
/// attributable to the target. This is where §7.2's "counted against run
/// confidence" for supply-failed lives: the disposition never trips the
/// regression gate (its [`GateAccounting`] is `Excluded`) and is outside
/// every rate denominator, so without this flag a mid-run supply collapse
/// would raise completeness while every automated confidence signal stayed
/// green. Any-nonzero semantics, mirroring [`FLAG_PREFETCH_SHORTFALL`]:
/// both mean the headline measured something other than what was planned.
pub const FLAG_SUPPLY_FAILED_UNITS: &str = "supply-failed-units";

/// The full low-confidence flag vocabulary, partitioned by refresh
/// provenance. Every flag the engine can mint appears in exactly one of
/// the two lists (the registry test pins the partition against the
/// `FLAG_` constants above), and [`comparability_with_counts`] reads the
/// partition as data:
///
/// - [`STICKY_BASE_FLAGS`]: minted once at plan/bootstrap time from facts
///   the report-time refresh cannot re-derive (tenant verification ran in
///   the plan stage; the interruption-replay knob was forced off when the
///   campaign record was first written). These survive from the persisted
///   base block — the merge is the persistence mechanism.
/// - [`REFRESH_DERIVED_FLAGS`]: predicates over the CURRENT campaign
///   truth (final cumulative rates, sticky context fields, terminal
///   counts), re-derived by [`low_confidence_flags`] on every refresh and
///   NEVER carried over from the base — carrying them is how a
///   deadline-partial run's tiny denominator used to latch a rate flag
///   onto a campaign whose final rates are clean.
pub const STICKY_BASE_FLAGS: &[&str] = &[
    FLAG_TENANT_UPSTREAMS_UNVERIFIED,
    FLAG_REPLAY_INTERRUPTIONS_DISABLED,
];

/// See [`STICKY_BASE_FLAGS`]. The re-derivation sources, per flag: the
/// two rate flags from the campaign-cumulative aggregates; prefetch
/// shortfall and timing degradation from the block's own sticky context
/// fields (`prefetch_shortfall_pct`, `timing_degraded` — those FIELDS
/// persist; the flags are recomputed from them); supply-failed-units from
/// the terminal disposition counts.
pub const REFRESH_DERIVED_FLAGS: &[&str] = &[
    FLAG_INFRA_INDETERMINATE_RATE,
    FLAG_NO_TRUTH_RATE,
    FLAG_PREFETCH_SHORTFALL,
    FLAG_TIMING_DEGRADED,
    FLAG_SUPPLY_FAILED_UNITS,
];

/// Derive the report-time low-confidence flags, in their fixed order:
/// infra-indeterminate rate, no-truth rate, prefetch shortfall, timing
/// degradation, supply-failed units. Flags set at plan/bootstrap time
/// (tenant verification, scheduling degradations) are not re-derived here
/// — they arrive through the base block and are merged by
/// [`comparability_with_counts`].
pub fn low_confidence_flags(
    agg: &Aggregates,
    knobs: &Knobs,
    block: &ComparabilityBlock,
) -> Vec<String> {
    let mut flags = Vec::new();
    if agg
        .infra_rate_pct
        .is_some_and(|pct| pct > knobs.infra_low_confidence_pct)
    {
        flags.push(FLAG_INFRA_INDETERMINATE_RATE.to_string());
    }
    if agg
        .no_truth_rate_pct
        .is_some_and(|pct| pct > knobs.no_truth_threshold_pct)
    {
        flags.push(FLAG_NO_TRUTH_RATE.to_string());
    }
    if block.prefetch_shortfall_pct.is_some_and(|pct| pct > 0.0) {
        flags.push(FLAG_PREFETCH_SHORTFALL.to_string());
    }
    if block.timing_degraded {
        flags.push(FLAG_TIMING_DEGRADED.to_string());
    }
    if agg
        .disposition_counts
        .get(Disposition::SupplyFailed.as_str())
        .copied()
        .unwrap_or(0)
        > 0
    {
        flags.push(FLAG_SUPPLY_FAILED_UNITS.to_string());
    }
    flags
}

/// Refresh the comparability block with final counts, the supply/timing
/// context, and the re-derived low-confidence flags.
///
/// Provenance discipline: the refreshed block is written back to
/// campaign.json at the end of every run and reloaded as `base` on resume,
/// so anything copied from `base` that the refresh ALSO derives becomes a
/// permanent latch across resume cycles (a deadline-partial run's stale
/// derivation indistinguishable from a source-authoritative fact). The
/// refresh therefore re-derives every derivable entry from current truth
/// and takes from its inputs only what no derivation can reproduce:
///
/// - excluded counts: re-derived from the class counts, plus
///   `recorder_excluded` — the archive's recorder-side exclusions
///   (re-read from the archive by the caller each run, never from the
///   previously persisted block; recorder-excluded units never become
///   workload units, so the two count disjoint populations and a shared
///   reason key SUMS — the engine count never shadows the recorder's).
///   `base.excluded` is deliberately not read at all.
/// - low-confidence flags: re-derived by [`low_confidence_flags`]; from
///   `base.low_confidence` only the [`STICKY_BASE_FLAGS`] (plan/bootstrap
///   facts with no report-time source) survive, plus — conservatively —
///   any flag outside the engine's vocabulary (a foreign or newer-engine
///   artifact: provably re-derivable it is not, and dropping it could
///   only upgrade confidence). A [`REFRESH_DERIVED_FLAGS`] member in the
///   base that the current truth no longer supports is GONE after the
///   refresh — flags can clear, counts can shrink.
pub fn comparability_with_counts(
    base: &ComparabilityBlock,
    agg: &Aggregates,
    plan_counts: &BTreeMap<String, usize>,
    knobs: &Knobs,
    supply: Option<&SupplyStageReport>,
    timed: Option<&TimedRunStats>,
    recorder_excluded: &BTreeMap<String, usize>,
) -> ComparabilityBlock {
    let mut block = base.clone();
    block.in_scope = plan_counts.get(PLAN_COUNT_IN_SCOPE).copied().unwrap_or(0);
    block.attemptable = plan_counts
        .get(PLAN_COUNT_ATTEMPTABLE)
        .copied()
        .unwrap_or(0);
    block.attempted = agg.attempted;
    // Excluded-but-reported: every verdict outside the headline denominator
    // (everything except match-built, output-divergence, unexpected-failure,
    // unexpected-dependency-failure) and every disposition, keyed by its
    // wire string, with its nonzero count.
    let headline_verdicts = [
        Verdict::MatchBuilt.as_str(),
        Verdict::OutputDivergence.as_str(),
        Verdict::UnexpectedFailure.as_str(),
        Verdict::UnexpectedDependencyFailure.as_str(),
    ];
    let mut excluded = BTreeMap::new();
    for (verdict, count) in &agg.verdict_counts {
        if !headline_verdicts.contains(&verdict.as_str()) && *count > 0 {
            excluded.insert(verdict.clone(), *count);
        }
    }
    for (disposition, count) in &agg.disposition_counts {
        if *count > 0 {
            excluded.insert(disposition.clone(), *count);
        }
    }
    // Recorder-side exclusions overlay the engine-derived counts by
    // SUMMING: the populations are disjoint (a recorder-excluded unit
    // never became a workload unit, so no engine record can describe it),
    // and the previous or_insert-from-base policy both resurrected stale
    // engine counts and silently dropped the recorder's count whenever
    // the engine re-derived the same reason key (e.g. `eval-error`, which
    // is a Disposition wire string AND a recorder reason).
    for (reason, count) in recorder_excluded {
        if *count > 0 {
            *excluded.entry(reason.clone()).or_insert(0) += *count;
        }
    }
    block.excluded = excluded;
    let terminal = terminal_job_count(&agg.verdict_counts, &agg.disposition_counts);
    block.completeness_pct = completeness_pct(terminal, block.in_scope);
    // Supply/timing context: copied into the block so the low-confidence
    // derivation below (and anyone reading campaign.json or progress.json)
    // sees them without chasing the per-stage reports. A missing stage
    // report keeps whatever the base block already recorded — these FIELDS
    // are the sticky carriers (campaign-lifetime events: a shortfall
    // happened, a cadence was not honored); the FLAGS over them re-derive.
    if let Some(pct) = supply.and_then(|report| report.shortfall_pct) {
        block.prefetch_shortfall_pct = Some(pct);
    }
    if timed.is_some_and(|stats| stats.timing_degraded) {
        block.timing_degraded = true;
    }
    // Low-confidence flags: the report-time derivations first (in their
    // fixed order), then the sticky plan/bootstrap flags (and unknown-
    // vocabulary flags, kept conservatively) from the base that the
    // derivation did not re-emit. Derived flags in the base are NOT
    // carried: a rate flag latched by an earlier partial run clears here
    // when the final cumulative rates no longer support it.
    let mut low_confidence = low_confidence_flags(agg, knobs, &block);
    for flag in &base.low_confidence {
        if low_confidence.contains(flag) {
            continue;
        }
        let known_derived = REFRESH_DERIVED_FLAGS.contains(&flag.as_str());
        if known_derived {
            continue;
        }
        if !STICKY_BASE_FLAGS.contains(&flag.as_str()) {
            tracing::warn!(
                flag,
                "comparability flag outside the engine vocabulary; keeping it \
                 (cannot prove it re-derivable — confidence only ever degrades)"
            );
        }
        low_confidence.push(flag.clone());
    }
    block.low_confidence = low_confidence;
    block
}

/// The supply summary as reported: a copy of the stage report with the
/// upload throughput derived from bytes / seconds when the stage did not
/// record it itself (the formula is the same one the stage uses).
fn supply_block(report: &SupplyStageReport) -> SupplyStageReport {
    let mut block = report.clone();
    if block.upload_mib_per_s.is_none() && block.upload_secs > 0.0 && block.uploaded_bytes > 0 {
        block.upload_mib_per_s =
            Some(block.uploaded_bytes as f64 / (1024.0 * 1024.0) / block.upload_secs);
    }
    block
}

/// The timed summary as reported: the dispatcher's run statistics with the
/// interruption counts re-derived from the classified verdict counts.
/// results.jsonl is the source of truth for what each armed unit ultimately
/// classified as (e.g. an armed request abandoned by the build deadline
/// rather than the disconnect deadline still classifies
/// `interruption-replayed`), so the report never disagrees with the verdict
/// counts it prints next to.
fn timed_block(stats: &TimedRunStats, verdict_counts: &BTreeMap<String, usize>) -> TimedRunStats {
    let count = |verdict: Verdict| verdict_counts.get(verdict.as_str()).copied().unwrap_or(0);
    TimedRunStats {
        interruptions_replayed: count(Verdict::InterruptionReplayed),
        interruptions_not_reproduced: count(Verdict::InterruptionNotReproduced),
        ..stats.clone()
    }
}

/// Render summary.md. Deterministic for identical inputs.
pub fn render_summary(input: &ReportInput<'_>) -> String {
    let agg = aggregate(input.records);
    let head: Headline = headline(&agg.verdict_counts, agg.nar_equal, agg.nar_compared_jobs);
    let empty_counts = BTreeMap::new();
    let plan_counts = input
        .campaign
        .plan
        .as_ref()
        .map(|p| &p.counts)
        .unwrap_or(&empty_counts);
    let block = comparability_with_counts(
        &input.campaign.comparability,
        &agg,
        plan_counts,
        &input.campaign.spec.knobs,
        input.supply,
        input.timed,
        input.recorder_excluded,
    );
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# Replay campaign {} — summary",
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
    // Archive provenance, campaign identity, and confidence context: each
    // row renders only when recorded, so reports over older campaign states
    // (and smoke runs) stay compact.
    if let Some(created_at) = &block.archive_created_at {
        let _ = writeln!(out, "| archive created_at | {created_at} |");
    }
    if let Some(age_days) = block.archive_age_days {
        let _ = writeln!(out, "| archive age (days) | {age_days:.1} |");
    }
    if let Some(mode) = &block.scheduling_mode {
        let _ = writeln!(out, "| scheduling mode | {mode} |");
    }
    if let Some(policy) = &block.supply_policy {
        let _ = writeln!(out, "| supply policy | {policy} |");
    }
    if block.prefetch_shortfall_pct.is_some() {
        let _ = writeln!(
            out,
            "| prefetch shortfall | {} |",
            fmt_pct(block.prefetch_shortfall_pct)
        );
    }
    if block.timing_degraded {
        let _ = writeln!(out, "| timing degraded | true |");
    }
    if let Some(count) = block.exclusions_recorded {
        let _ = writeln!(out, "| exclusions recorded | {count} |");
    }
    // Conflict-resolved truth is comparability context like the exclusion
    // counts: nonzero means some units' expected outcomes were chosen by
    // the collapse rank, not plainly recorded. `Some(0)` renders nothing
    // (no conflicts to disclose); `None` is a campaign from before the
    // count existed.
    if let Some(count) = block.truth_collapse_conflicts
        && count > 0
    {
        let _ = writeln!(out, "| truth collapse conflicts | {count} |");
    }
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
        "- Output divergence (within headline): {} jobs",
        agg.verdict_counts
            .get(Verdict::OutputDivergence.as_str())
            .copied()
            .unwrap_or(0)
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
        "- Infra-indeterminate rate (excluded from headline): {}",
        fmt_pct(agg.infra_rate_pct)
    );
    let _ = writeln!(out, "- No-truth rate: {}", fmt_pct(agg.no_truth_rate_pct));
    let _ = writeln!(out, "\n## Verdicts");
    let _ = writeln!(out, "| verdict | count | of which cascaded |");
    let _ = writeln!(out, "|---|---:|---:|");
    for verdict in Verdict::ALL {
        let count = agg
            .verdict_counts
            .get(verdict.as_str())
            .copied()
            .unwrap_or(0);
        if count == 0 {
            continue;
        }
        let cascaded = agg
            .cascaded_counts
            .get(verdict.as_str())
            .copied()
            .unwrap_or(0);
        let cascaded = if cascaded > 0 {
            cascaded.to_string()
        } else {
            String::new()
        };
        let _ = writeln!(out, "| {} | {} | {} |", verdict.as_str(), count, cascaded);
    }
    let _ = writeln!(out, "\n## Dispositions");
    let _ = writeln!(out, "| disposition | count |");
    let _ = writeln!(out, "|---|---:|");
    for disposition in Disposition::ALL {
        let count = agg
            .disposition_counts
            .get(disposition.as_str())
            .copied()
            .unwrap_or(0);
        if count == 0 {
            continue;
        }
        let _ = writeln!(out, "| {} | {} |", disposition.as_str(), count);
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
        "- match-built/output-divergence on first attempt: {} | after retries: {}",
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
    // Supply summary: always rendered (every campaign runs the supply
    // stage); the placeholder covers states written before the stage existed
    // or whose report was lost.
    let _ = writeln!(out, "\n## Supply");
    match input.supply {
        Some(report) => {
            let supply = supply_block(report);
            let _ = writeln!(
                out,
                "- delivered: {} | delegated: {} | already-present: {} | refused: {} | \
                 unavailable: {} | failed: {} | skipped: {}",
                supply.delivered,
                supply.delegated,
                supply.already_present,
                supply.refused,
                supply.unavailable,
                supply.failed,
                supply.skipped
            );
            let _ = writeln!(
                out,
                "- uploaded: {:.1} MiB at {}",
                supply.uploaded_bytes as f64 / (1024.0 * 1024.0),
                supply
                    .upload_mib_per_s
                    .map(|rate| format!("{rate:.2} MiB/s"))
                    .unwrap_or_else(|| "n/a".to_string())
            );
            let _ = writeln!(
                out,
                "- prefetch shortfall: {} (planned {}, missing {}, unavailable {})",
                fmt_pct(supply.shortfall_pct),
                supply.planned_prefetch,
                supply.prefetch_missing,
                supply.prefetch_unavailable
            );
        }
        None => {
            let _ = writeln!(out, "(not recorded)");
        }
    }
    if input.plan_rss_mib.is_some() || input.plan_rss_peak_mib.is_some() {
        let fmt_mib = |v: Option<u64>| {
            v.map(|mib| format!("{mib} MiB"))
                .unwrap_or_else(|| "n/a".to_string())
        };
        let _ = writeln!(
            out,
            "- plan-stage RSS: {} before, {} peak",
            fmt_mib(input.plan_rss_mib),
            fmt_mib(input.plan_rss_peak_mib)
        );
    }
    // Timed dispatch summary: only timed campaigns have one.
    if let Some(stats) = input.timed {
        let timed = timed_block(stats, &agg.verdict_counts);
        let _ = writeln!(out, "\n## Timed dispatch");
        let _ = writeln!(
            out,
            "- requests: {} dispatched of {} scheduled",
            timed.dispatched, timed.requests_total
        );
        let _ = writeln!(
            out,
            "- dispatch lateness: max {} ms, p50 {} ms, p95 {} ms",
            timed.max_dispatch_lateness_ms, timed.lateness_p50_ms, timed.lateness_p95_ms
        );
        let _ = writeln!(
            out,
            "- interruptions: {} replayed, {} not reproduced",
            timed.interruptions_replayed, timed.interruptions_not_reproduced
        );
        let _ = writeln!(
            out,
            "- engine-side submission failures: {}",
            timed.submission_failures
        );
        let _ = writeln!(
            out,
            "- resumes: {} | timing degraded: {}",
            timed.resume_count, timed.timing_degraded
        );
        let _ = writeln!(out, "- abort recommended: {}", input.abort_recommended);
    }
    let _ = writeln!(out, "\n## Artifacts");
    let _ = writeln!(
        out,
        "- results.jsonl, supply.jsonl, dispatch.jsonl, batches.jsonl, \
         buckets/<verdict-or-disposition>.jsonl, report/gate.json (when a regression gate was \
         requested), logs/<job>.log.zst next to this file"
    );
    out
}

/// Coverage witness of one gate evaluation: what an untripped gate's
/// "pass" is actually worth.
///
/// Absence of counter-evidence is not positive verification — a gate
/// evaluated over zero classified units observed nothing, so its pass is
/// vacuous. The witness makes that distinction structural: a meaningful
/// pass carries the non-zero count of classified units the trip sets were
/// evaluated over, and the zero-coverage case is its own variant that no
/// consumer can conflate with verified coverage. `NothingInScope` is
/// CLEAN here by design — an empty-scope campaign legitimately reports an
/// untripped gate — but a consumer that requires coverage (CI wiring, a
/// publish step) can demand `Checked(_)` instead of trusting
/// `tripped == false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateCoverage {
    /// The gate was evaluated over zero classified units: nothing was
    /// checked, so the pass asserts nothing (and a trip is impossible).
    NothingInScope,
    /// The gate was evaluated over this many classified units.
    Checked(std::num::NonZeroUsize),
}

/// Regression-gate result, written to `report/gate.json` and mirrored in
/// progress.json when the campaign requested the regression-gate report
/// policy. The gate is data for the operator CLI (`report --check` maps it
/// to an exit code); the engine's own exit code never depends on it. The
/// field names are the wire keys verbatim (snake_case), so the JSON reads
/// `{"policy":…,"fail_on":…,"tripped":…,"checked":…,"counts":{…}}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GateResult {
    /// Always the regression-gate policy name.
    pub policy: String,
    /// The trip condition the gate was evaluated under ([`FailOn`] wire string).
    pub fail_on: String,
    /// Whether any contributing class had a nonzero count.
    pub tripped: bool,
    /// Coverage witness: how many EVIDENCE-BEARING classified units the
    /// trip sets were evaluated over — every class whose
    /// [`GateAccounting`] is not `Excluded`, so verdicts plus the
    /// upload-rejected and target-substituted dispositions count, while
    /// plan-time exclusions, supply-failed, and the not-attempted deadline
    /// backfill do not (the backfill records every untouched unit when a
    /// deadline fires, and a witness it could inflate would certify
    /// attempted-nothing campaigns). An untripped gate with `checked: 0`
    /// passed vacuously — see [`GateResult::coverage`]. Defaulted on
    /// reload so gate.json written before the witness existed still parses
    /// (as zero coverage, the honest reading of a record that never
    /// carried one).
    #[serde(default)]
    pub checked: usize,
    /// The contributing classes with nonzero counts (empty when nothing in
    /// the trip set was observed).
    pub counts: BTreeMap<String, usize>,
}

impl GateResult {
    /// The typed coverage witness for this evaluation.
    pub fn coverage(&self) -> GateCoverage {
        match std::num::NonZeroUsize::new(self.checked) {
            None => GateCoverage::NothingInScope,
            Some(checked) => GateCoverage::Checked(checked),
        }
    }
}

/// Whether a class with the given accounting trips under `fail_on`.
fn accounting_trips(accounting: GateAccounting, fail_on: FailOn) -> bool {
    match fail_on {
        FailOn::None => false,
        FailOn::Regression => accounting == GateAccounting::TripsRegression,
        FailOn::Divergence => matches!(
            accounting,
            GateAccounting::TripsRegression | GateAccounting::TripsDivergence
        ),
    }
}

/// Evaluate the regression gate over the final per-class counts.
///
/// Both the trip sets and the coverage witness derive from ONE per-class
/// classification, [`GateAccounting`] (an exhaustive method on `Verdict`
/// and `Disposition`, so a new class cannot ship without a gate
/// decision): `regression` trips on the classes accounted
/// `TripsRegression` — exactly the design doc §7.3 regression row
/// (unexpected-failure, unexpected-dependency-failure, upload-rejected,
/// infra-indeterminate); `divergence` adds the `TripsDivergence` classes
/// (output-divergence, unexpected-success, interruption-not-reproduced);
/// `none` never trips. `supply-failed` deliberately trips NOTHING: per
/// §7.2 it is counted against run confidence, and that accounting lives
/// in the supply-failed low-confidence flag
/// ([`FLAG_SUPPLY_FAILED_UNITS`]), never in the gate.
///
/// The result always carries its coverage witness: the number of
/// EVIDENCE-BEARING classified units the trip sets were evaluated over —
/// every class except the `Excluded` accounting (plan-time exclusions,
/// supply-failed, the not-attempted deadline backfill). The exclusion
/// matters: the backfill writes a record for every untouched unit when a
/// deadline fires, so a total-classification count would mint a non-zero
/// witness for a campaign that attempted nothing. The denominator is an
/// explicit part of the evaluation, not an incidentally-incremented
/// counter, so a pass over an empty (or all-backfill) classification
/// cannot masquerade as a verified one.
pub fn evaluate_gate(
    fail_on: FailOn,
    verdict_counts: &BTreeMap<String, usize>,
    disposition_counts: &BTreeMap<String, usize>,
) -> GateResult {
    let mut counts = BTreeMap::new();
    let mut checked = 0usize;
    let mut account = |class: &str, accounting: GateAccounting, count: usize| {
        if count == 0 {
            return;
        }
        if accounting.counts_as_checked() {
            checked += count;
        }
        if accounting_trips(accounting, fail_on) {
            counts.insert(class.to_string(), count);
        }
    };
    for verdict in Verdict::ALL {
        let count = verdict_counts.get(verdict.as_str()).copied().unwrap_or(0);
        account(verdict.as_str(), verdict.gate_accounting(), count);
    }
    for disposition in Disposition::ALL {
        let count = disposition_counts
            .get(disposition.as_str())
            .copied()
            .unwrap_or(0);
        account(disposition.as_str(), disposition.gate_accounting(), count);
    }
    GateResult {
        policy: ReportPolicy::RegressionGate.as_str().to_string(),
        fail_on: fail_on.as_str().to_string(),
        tripped: !counts.is_empty(),
        checked,
        counts,
    }
}

/// The campaign's gate result when (and only when) the regression-gate
/// report policy was requested in the spec; `None` otherwise.
fn gate_for_spec(campaign: &CampaignRecord, agg: &Aggregates) -> Option<GateResult> {
    campaign
        .spec
        .report
        .policies
        .contains(&ReportPolicy::RegressionGate)
        .then(|| {
            evaluate_gate(
                campaign.spec.report.fail_on,
                &agg.verdict_counts,
                &agg.disposition_counts,
            )
        })
}

/// progress.json: stage, per-verdict/disposition counts, rates, suspension
/// windows, ETA.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub campaign_id: String,
    pub stage: String,
    pub updated_at: String,
    pub verdict_counts: BTreeMap<String, usize>,
    pub disposition_counts: BTreeMap<String, usize>,
    pub attempted: usize,
    pub infra_rate_pct: Option<f64>,
    pub no_truth_rate_pct: Option<f64>,
    pub jobs_per_hour: Option<f64>,
    pub eta_hours: Option<f64>,
    pub suspension: SuspensionSummary,
    pub comparability: ComparabilityBlock,
    /// Supply-stage summary (delivered/delegated/… counts, upload
    /// throughput, prefetch shortfall); absent until the supply stage has
    /// run.
    #[serde(default)]
    pub supply: Option<SupplyStageReport>,
    /// Timed-dispatch summary (lateness distribution, interruption counts,
    /// resume/degradation flags); absent for timeless campaigns.
    #[serde(default)]
    pub timed: Option<TimedRunStats>,
    /// True when the infra-failure rate crossed the pause threshold in timed
    /// mode, where pausing would distort the recorded cadence — the operator
    /// is asked to abort instead of the engine pausing itself.
    #[serde(default)]
    pub abort_recommended: bool,
    /// Regression-gate result over the counts above, mirroring
    /// `report/gate.json`; absent when the campaign did not request the
    /// regression-gate report policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<GateResult>,
}

/// Build the progress.json document from the current records and stage.
/// `recorder_excluded` carries the archive's recorder-side exclusion
/// counts, re-derived from the archive by the caller (see
/// [`comparability_with_counts`]'s provenance discipline).
#[allow(clippy::too_many_arguments)]
pub fn build_progress(
    campaign: &CampaignRecord,
    records: &BTreeMap<String, JobRecord>,
    suspension: &SuspensionSummary,
    stage: &str,
    updated_at: String,
    jobs_per_hour: Option<f64>,
    supply: Option<&SupplyStageReport>,
    timed: Option<&TimedRunStats>,
    abort_recommended: bool,
    recorder_excluded: &BTreeMap<String, usize>,
) -> Progress {
    let agg = aggregate(records);
    let empty_counts = BTreeMap::new();
    let plan_counts = campaign
        .plan
        .as_ref()
        .map(|p| &p.counts)
        .unwrap_or(&empty_counts);
    let block = comparability_with_counts(
        &campaign.comparability,
        &agg,
        plan_counts,
        &campaign.spec.knobs,
        supply,
        timed,
        recorder_excluded,
    );
    let terminal = terminal_job_count(&agg.verdict_counts, &agg.disposition_counts);
    let remaining = block.in_scope.saturating_sub(terminal);
    Progress {
        campaign_id: campaign.campaign_id.clone(),
        stage: stage.to_string(),
        updated_at,
        verdict_counts: agg.verdict_counts.clone(),
        disposition_counts: agg.disposition_counts.clone(),
        attempted: agg.attempted,
        infra_rate_pct: agg.infra_rate_pct,
        no_truth_rate_pct: agg.no_truth_rate_pct,
        jobs_per_hour,
        // ETA is an estimate of remaining work, so it exists only while
        // in-scope work remains: an empty campaign and a fully terminal one
        // both report `None` rather than a "0h remaining" figure. With
        // `remaining` derived from the in-scope-only terminal count, a
        // zero-hour ETA is unrepresentable while work is outstanding.
        eta_hours: jobs_per_hour
            .filter(|jph| *jph > 0.0 && block.in_scope > 0 && remaining > 0)
            .map(|jph| remaining as f64 / jph),
        suspension: suspension.clone(),
        comparability: block,
        supply: supply.map(supply_block),
        timed: timed.map(|stats| timed_block(stats, &agg.verdict_counts)),
        abort_recommended,
        gate: gate_for_spec(campaign, &agg),
    }
}

/// Write summary.md + `report/gate.json` (when a regression gate was
/// requested) + `buckets/<verdict-or-disposition>.jsonl` into the state dir
/// and return the rendered summary text.
pub fn write_report(state: &StateDir, input: &ReportInput<'_>) -> Result<String> {
    let summary = render_summary(input);
    state.write_bytes("report/summary.md", summary.as_bytes())?;
    // Regression gate: persisted as data next to the summary, never encoded
    // in the engine's exit code — the gate is consumed by the operator CLI's
    // report --check, and a nonzero pod exit would make the Job controller
    // retry the whole campaign.
    if let Some(gate) = gate_for_spec(input.campaign, &aggregate(input.records)) {
        state.write_json_atomic("report/gate.json", &gate)?;
    }
    // Per-class JSONL (one file per distinct verdict or disposition). The
    // report stage owns buckets/: drop files from a previous render first,
    // so a job that since moved classes cannot linger in its old file.
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
    let mut by_class: BTreeMap<String, Vec<&JobRecord>> = BTreeMap::new();
    for rec in input.records.values() {
        // An unclassified record (neither verdict nor disposition) belongs
        // to no per-class file; the engine never writes one, but a skip is
        // safer than inventing an empty file name for it.
        let Some(class) = rec.verdict.clone().or_else(|| rec.disposition.clone()) else {
            continue;
        };
        by_class.entry(class).or_default().push(rec);
    }
    for (class, recs) in by_class {
        let path = state.path(&format!("buckets/{class}.jsonl"));
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
    use crate::run::model::{ExpectedSide, RioSide, UnifiedClass};

    fn v(verdict: Verdict) -> UnifiedClass {
        UnifiedClass::Verdict(verdict)
    }
    fn d(disposition: Disposition) -> UnifiedClass {
        UnifiedClass::Disposition(disposition)
    }

    fn rec(
        job: &str,
        class: UnifiedClass,
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
            expected: ExpectedSide::default(),
            nar_compare: BTreeMap::new(),
            verdict: class.verdict().map(|v| v.as_str().into()),
            disposition: class.disposition().map(|d| d.as_str().into()),
            cascaded,
            failure_cause: None,
            flaky: false,
            signature: signature.map(String::from),
            log_key: None,
            repro: String::new(),
            evidence: None,
            updated_at: "2026-05-26T00:00:00Z".into(),
        }
    }

    /// The attempted denominator derives from Disposition::attempted() —
    /// the enum's own exhaustive method — so EVERY never-submitted
    /// disposition (plan-time exclusions, the supply retirements
    /// upload-rejected/supply-failed, the deadline backfill) is excluded,
    /// and only the mid-run target substitution counts as a submission
    /// outcome. One record per disposition plus one verdict pins the sum.
    #[test]
    fn attempted_denominator_excludes_every_unsubmitted_disposition() {
        for disposition in Disposition::ALL {
            assert_eq!(
                disposition.attempted(),
                disposition == Disposition::TargetSubstituted,
                "{disposition:?}"
            );
        }
        let mut records = BTreeMap::new();
        for (i, disposition) in Disposition::ALL.into_iter().enumerate() {
            records.insert(
                format!("d{i}"),
                rec(&format!("d{i}"), d(disposition), 0, None, false),
            );
        }
        records.insert(
            "v0".into(),
            rec("v0", v(Verdict::MatchBuilt), 1, None, false),
        );
        let agg = aggregate(&records);
        // 11 records total; only match-built and target-substituted were
        // ever submitted.
        assert_eq!(agg.attempted, 2, "{agg:?}");
    }

    /// Completeness population membership, decided per disposition over the
    /// whole vocabulary. The expected column derives from the producers'
    /// scope contract (run/mod.rs): `plan_time_dispositions` classifies
    /// every `plan.skipped` (out-of-scope) job as `filtered`, and every
    /// other disposition producer filters to `plan.in_scope` first — so
    /// `filtered` is the single class whose records the in-scope
    /// denominator's population cannot contain. `not-attempted` is in
    /// population but NOT terminal (a resume may still attempt it). The
    /// exhaustiveness guard makes a new disposition fail here until its
    /// row is added alongside its `in_scope_population()` decision.
    #[test]
    fn completeness_counts_only_in_scope_terminal_dispositions() {
        let expected: &[(Disposition, bool)] = &[
            (Disposition::Filtered, false),
            (Disposition::EvalError, true),
            (Disposition::IdentityDivergent, true),
            (Disposition::NotAttemptable, true),
            (Disposition::DemotedImpure, true),
            (Disposition::CachedPrior, true),
            (Disposition::UploadRejected, true),
            (Disposition::SupplyFailed, true),
            (Disposition::TargetSubstituted, true),
            (Disposition::NotAttempted, true),
        ];
        assert_eq!(
            expected.len(),
            Disposition::ALL.len(),
            "every disposition needs a population row"
        );
        for disposition in Disposition::ALL {
            let in_population = expected
                .iter()
                .find(|(d, _)| *d == disposition)
                .unwrap_or_else(|| panic!("no population row for {disposition:?}"))
                .1;
            assert_eq!(
                disposition.in_scope_population(),
                in_population,
                "{disposition:?}"
            );
            // One record of this class against a 1-job in-scope plan: it
            // contributes to completeness exactly when it is in population
            // AND terminal.
            let counts = BTreeMap::from([(disposition.as_str().to_string(), 1usize)]);
            let terminal = terminal_job_count(&BTreeMap::new(), &counts);
            let counted = in_population && disposition != Disposition::NotAttempted;
            assert_eq!(terminal, usize::from(counted), "{disposition:?}");
            assert_eq!(
                completeness_pct(terminal, 1),
                if counted { 100.0 } else { 0.0 },
                "{disposition:?}"
            );
        }
    }

    /// The filtered-campaign shape every prior fixture missed (their
    /// skipped sets were empty, making the record population coincide with
    /// the in-scope set): plan-time `filtered` exclusion records for
    /// out-of-scope jobs must not inflate completeness past 100% or melt
    /// the ETA to zero while in-scope work remains.
    #[test]
    fn filtered_records_stay_out_of_completeness_and_eta() {
        let spec: crate::run::spec::CampaignSpec =
            serde_json::from_str(r#"{"mode":"leaf"}"#).unwrap();
        let mut campaign = CampaignRecord::new(
            "c-filtered".into(),
            "2026-05-26T00:00:00Z".into(),
            spec,
            crate::run::spec::ArchivePin::default(),
        );
        // --limit 2 of 5: three jobs leave scope and get plan-time
        // `filtered` records; the in-scope denominator is 2.
        campaign.plan = Some(crate::run::spec::PlanOutput {
            counts: BTreeMap::from([
                ("inScope".to_string(), 2usize),
                ("attemptable".to_string(), 2usize),
            ]),
            ..Default::default()
        });
        let mut records = BTreeMap::new();
        for i in 0..3 {
            records.insert(
                format!("skipped{i}"),
                rec(
                    &format!("skipped{i}"),
                    d(Disposition::Filtered),
                    0,
                    None,
                    false,
                ),
            );
        }
        records.insert(
            "done".into(),
            rec("done", v(Verdict::MatchBuilt), 1, None, false),
        );
        let p = build_progress(
            &campaign,
            &records,
            &SuspensionSummary::default(),
            "submit+collect",
            "2026-05-26T01:00:00Z".into(),
            Some(2.0),
            None,
            None,
            false,
            &BTreeMap::new(),
        );
        // 1 of 2 in-scope jobs is terminal; the 3 filtered records are
        // bookkeeping for jobs outside the denominator's population.
        assert_eq!(p.comparability.in_scope, 2);
        assert!(
            (p.comparability.completeness_pct - 50.0).abs() < 1e-9,
            "completeness must be in-scope only: {}",
            p.comparability.completeness_pct
        );
        // 1 in-scope job remains at 2 jobs/hour.
        assert!(
            (p.eta_hours.unwrap() - 0.5).abs() < 1e-9,
            "{:?}",
            p.eta_hours
        );
        // The filtered records still appear where they belong: the
        // excluded-but-reported map, not the completeness numerator.
        assert_eq!(p.comparability.excluded.get("filtered"), Some(&3));
    }

    /// Both directions of the completeness population invariant: a
    /// consistent state reports the exact (unclamped) ratio, and state this
    /// engine did not write — more in-scope terminal records than in-scope
    /// jobs — clamps to 100% instead of reporting an impossible figure.
    #[test]
    fn completeness_is_exact_in_population_and_clamps_above_it() {
        // Must-admit: a half-terminal campaign reads exactly 50%.
        assert!((completeness_pct(5, 10) - 50.0).abs() < 1e-9);
        assert!((completeness_pct(10, 10) - 100.0).abs() < 1e-9);
        // Must-block: >100% is unrepresentable even over foreign state.
        assert!((completeness_pct(11, 10) - 100.0).abs() < 1e-9);
        // No work planned: no ratio to report.
        assert!((completeness_pct(3, 0) - 0.0).abs() < 1e-9);
    }

    /// A fully terminal campaign reports no ETA (there is nothing left to
    /// estimate), so a "0h remaining" figure cannot appear: the ETA either
    /// covers real remaining work or is absent.
    #[test]
    fn eta_is_none_when_no_in_scope_work_remains() {
        let spec: crate::run::spec::CampaignSpec =
            serde_json::from_str(r#"{"mode":"leaf"}"#).unwrap();
        let mut campaign = CampaignRecord::new(
            "c-done".into(),
            "2026-05-26T00:00:00Z".into(),
            spec,
            crate::run::spec::ArchivePin::default(),
        );
        campaign.plan = Some(crate::run::spec::PlanOutput {
            counts: BTreeMap::from([("inScope".to_string(), 1usize)]),
            ..Default::default()
        });
        let mut records = BTreeMap::new();
        records.insert(
            "done".into(),
            rec("done", v(Verdict::MatchBuilt), 1, None, false),
        );
        let p = build_progress(
            &campaign,
            &records,
            &SuspensionSummary::default(),
            "report",
            "2026-05-26T01:00:00Z".into(),
            Some(2.0),
            None,
            None,
            false,
            &BTreeMap::new(),
        );
        assert!(
            (p.comparability.completeness_pct - 100.0).abs() < 1e-9,
            "{}",
            p.comparability.completeness_pct
        );
        assert_eq!(p.eta_hours, None);
    }

    #[test]
    fn aggregate_counts_classes_signatures_and_rates() {
        let mut records = BTreeMap::new();
        for (i, class) in [
            v(Verdict::MatchBuilt),
            v(Verdict::OutputDivergence),
            v(Verdict::UnexpectedFailure),
            v(Verdict::InfraIndeterminate),
            d(Disposition::CachedPrior),
            d(Disposition::NotAttempted),
        ]
        .into_iter()
        .enumerate()
        {
            let sig = (class == v(Verdict::UnexpectedFailure)).then_some("poison-threshold");
            // Attempts = 2 everywhere so the retry split exercises both
            // match-class verdicts (match-built and output-divergence).
            records.insert(format!("j{i}"), rec(&format!("j{i}"), class, 2, sig, false));
        }
        records.insert(
            "casc".into(),
            rec("casc", v(Verdict::InfraIndeterminate), 1, None, true),
        );
        let agg = aggregate(&records);
        assert_eq!(agg.verdict_counts["match-built"], 1);
        assert_eq!(agg.verdict_counts["output-divergence"], 1);
        assert_eq!(agg.verdict_counts["infra-indeterminate"], 2);
        assert_eq!(agg.disposition_counts["cached-prior"], 1);
        assert_eq!(agg.disposition_counts["not-attempted"], 1);
        assert_eq!(agg.cascaded_counts["infra-indeterminate"], 1);
        assert_eq!(agg.signature_counts["poison-threshold"], 1);
        // Both match-class verdicts (match-built and output-divergence)
        // count toward the retry split; both fixtures took two attempts.
        assert_eq!(
            (agg.first_attempt_successes, agg.multi_attempt_successes),
            (0, 2)
        );
        // attempted = total(7) - cached-prior(1) - not-attempted(1) = 5
        assert_eq!(agg.attempted, 5);
        assert!((agg.infra_rate_pct.unwrap() - 40.0).abs() < 1e-9);
        // no-truth rate is reported (zero here), keyed off the no-truth
        // verdict over the attempted denominator.
        assert!((agg.no_truth_rate_pct.unwrap() - 0.0).abs() < 1e-9);
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
        records.insert("a".into(), rec("a", v(Verdict::MatchBuilt), 1, None, false));
        let suspension = SuspensionSummary::default();
        let input = ReportInput {
            campaign: &campaign,
            records: &records,
            suspension: &suspension,
            generated_at: "2026-05-26T12:00:00Z".into(),
            partial: true,
            top_n: 5,
            supply: None,
            timed: None,
            abort_recommended: false,
            plan_rss_mib: None,
            plan_rss_peak_mib: None,
            recorder_excluded: &BTreeMap::new(),
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
        records.insert("a".into(), rec("a", v(Verdict::MatchBuilt), 1, None, false));
        let p = build_progress(
            &campaign,
            &records,
            &SuspensionSummary::default(),
            "submit+collect",
            "2026-05-26T01:00:00Z".into(),
            Some(2.0),
            None,
            None,
            false,
            &BTreeMap::new(),
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
            None,
            None,
            false,
            &BTreeMap::new(),
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
                    v(Verdict::UnexpectedFailure),
                    1,
                    Some(sig),
                    false,
                ),
            );
        }
        for i in 0..3 {
            let mut r = rec(&format!("div{i}"), v(Verdict::MatchBuilt), 1, None, false);
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
            supply: None,
            timed: None,
            abort_recommended: false,
            plan_rss_mib: None,
            plan_rss_peak_mib: None,
            recorder_excluded: &BTreeMap::new(),
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
    fn progress_carries_supply_and_timed_blocks() {
        let spec: crate::run::spec::CampaignSpec =
            serde_json::from_str(r#"{"mode":"leaf"}"#).unwrap();
        let campaign = CampaignRecord::new(
            "c-test".into(),
            "2026-05-26T00:00:00Z".into(),
            spec,
            crate::run::spec::ArchivePin::default(),
        );
        let mut records = BTreeMap::new();
        records.insert("a".into(), rec("a", v(Verdict::MatchBuilt), 1, None, false));
        records.insert(
            "b".into(),
            rec("b", v(Verdict::InterruptionReplayed), 1, None, false),
        );
        // The stage did not compute the throughput figure itself; the
        // progress builder derives it from bytes / seconds.
        let supply = SupplyStageReport {
            delivered: 3,
            uploaded_bytes: 64 * 1024 * 1024,
            upload_secs: 2.0,
            upload_mib_per_s: None,
            shortfall_pct: Some(5.0),
            ..SupplyStageReport::default()
        };
        // Dispatcher tallies deliberately disagree with the classified
        // buckets to prove the buckets win in the rendered block.
        let timed = TimedRunStats {
            requests_total: 4,
            dispatched: 4,
            interruptions_replayed: 0,
            interruptions_not_reproduced: 5,
            ..TimedRunStats::default()
        };
        let p = build_progress(
            &campaign,
            &records,
            &SuspensionSummary::default(),
            "done",
            "2026-05-26T01:00:00Z".into(),
            None,
            Some(&supply),
            Some(&timed),
            true,
            &BTreeMap::new(),
        );
        assert!(p.abort_recommended);
        let supply_block = p.supply.as_ref().expect("supply block present");
        assert!(
            (supply_block.upload_mib_per_s.unwrap() - 32.0).abs() < 1e-9,
            "{supply_block:?}"
        );
        let timed_block = p.timed.as_ref().expect("timed block present");
        // Interruption counts derive from the classified buckets (the source
        // of truth), not from the dispatcher's own tally.
        assert_eq!(timed_block.interruptions_replayed, 1);
        assert_eq!(timed_block.interruptions_not_reproduced, 0);
        // The interruption buckets are excluded from the headline, so the
        // comparability block reports them under the excluded counts.
        assert_eq!(
            p.comparability.excluded.get("interruption-replayed"),
            Some(&1)
        );
        // Wire keys are camelCase under the prescribed block names.
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["supply"]["uploadMibPerS"], serde_json::json!(32.0));
        assert!(json["timed"]["latenessP95Ms"].is_number());
        assert_eq!(json["abortRecommended"], serde_json::json!(true));
    }

    #[test]
    fn summary_renders_supply_and_timed_sections() {
        let spec: crate::run::spec::CampaignSpec =
            serde_json::from_str(r#"{"mode":"leaf"}"#).unwrap();
        let campaign = CampaignRecord::new(
            "c-test".into(),
            "2026-05-26T00:00:00Z".into(),
            spec,
            crate::run::spec::ArchivePin::default(),
        );
        let mut records = BTreeMap::new();
        records.insert("a".into(), rec("a", v(Verdict::MatchBuilt), 1, None, false));
        records.insert(
            "b".into(),
            rec("b", v(Verdict::InterruptionReplayed), 1, None, false),
        );
        let supply = SupplyStageReport {
            planned_prefetch: 40,
            prefetch_missing: 3,
            delivered: 2,
            delegated: 1,
            already_present: 4,
            refused: 1,
            unavailable: 3,
            failed: 1,
            skipped: 2,
            uploaded_bytes: 10 * 1024 * 1024,
            upload_secs: 5.0,
            upload_mib_per_s: Some(2.0),
            shortfall_pct: Some(7.5),
            ..SupplyStageReport::default()
        };
        let timed = TimedRunStats {
            requests_total: 6,
            dispatched: 5,
            max_dispatch_lateness_ms: 1200,
            lateness_p50_ms: 40,
            lateness_p95_ms: 900,
            // Deliberately wrong: the rendered counts come from the buckets.
            interruptions_replayed: 9,
            interruptions_not_reproduced: 9,
            submission_failures: 1,
            resume_count: 1,
            timing_degraded: true,
            dispatch_started_at_unix: Some(1_700_000_000),
            dispatch_ended_at_unix: Some(1_700_003_600),
        };
        let suspension = SuspensionSummary::default();
        let input = ReportInput {
            campaign: &campaign,
            records: &records,
            suspension: &suspension,
            generated_at: "2026-05-26T12:00:00Z".into(),
            partial: false,
            top_n: 5,
            supply: Some(&supply),
            timed: Some(&timed),
            abort_recommended: true,
            plan_rss_mib: Some(512),
            plan_rss_peak_mib: Some(2048),
            recorder_excluded: &BTreeMap::new(),
        };
        let out = render_summary(&input);
        assert!(out.contains("## Supply"), "{out}");
        assert!(
            out.contains(
                "delivered: 2 | delegated: 1 | already-present: 4 | refused: 1 | unavailable: 3 \
                 | failed: 1 | skipped: 2"
            ),
            "{out}"
        );
        assert!(out.contains("uploaded: 10.0 MiB at 2.00 MiB/s"), "{out}");
        assert!(
            out.contains("prefetch shortfall: 7.50% (planned 40, missing 3, unavailable 0)"),
            "{out}"
        );
        assert!(
            out.contains("plan-stage RSS: 512 MiB before, 2048 MiB peak"),
            "{out}"
        );
        assert!(out.contains("## Timed dispatch"), "{out}");
        assert!(
            out.contains("requests: 5 dispatched of 6 scheduled"),
            "{out}"
        );
        assert!(
            out.contains("dispatch lateness: max 1200 ms, p50 40 ms, p95 900 ms"),
            "{out}"
        );
        // Bucket-derived, not the dispatcher tally of 9/9.
        assert!(
            out.contains("interruptions: 1 replayed, 0 not reproduced"),
            "{out}"
        );
        assert!(out.contains("engine-side submission failures: 1"), "{out}");
        assert!(out.contains("resumes: 1 | timing degraded: true"), "{out}");
        assert!(out.contains("abort recommended: true"), "{out}");

        // A timeless campaign (no timed stats) keeps the Supply section but
        // omits the Timed dispatch section entirely.
        let timeless = ReportInput {
            timed: None,
            ..input.clone()
        };
        let out = render_summary(&timeless);
        assert!(out.contains("## Supply"), "{out}");
        assert!(!out.contains("## Timed dispatch"), "{out}");

        // No supply report at all (e.g. a campaign state predating the
        // supply stage): the section renders a placeholder, never vanishes.
        let bare = ReportInput {
            supply: None,
            timed: None,
            plan_rss_mib: None,
            plan_rss_peak_mib: None,
            ..input.clone()
        };
        let out = render_summary(&bare);
        assert!(out.contains("## Supply"), "{out}");
        assert!(out.contains("(not recorded)"), "{out}");
    }

    #[test]
    fn gate_trips_per_fail_on_policy() {
        let mut counts = BTreeMap::new();
        counts.insert(Verdict::MatchBuilt.as_str().to_string(), 10);
        let dispositions = BTreeMap::new();
        assert!(!evaluate_gate(FailOn::Regression, &counts, &dispositions).tripped);
        counts.insert(Verdict::OutputDivergence.as_str().to_string(), 1);
        assert!(!evaluate_gate(FailOn::Regression, &counts, &dispositions).tripped);
        assert!(evaluate_gate(FailOn::Divergence, &counts, &dispositions).tripped);
        counts.insert(Verdict::UnexpectedFailure.as_str().to_string(), 1);
        let gate = evaluate_gate(FailOn::Regression, &counts, &dispositions).tripped;
        assert!(gate);
        let mut dispositions = BTreeMap::new();
        dispositions.insert(Disposition::UploadRejected.as_str().to_string(), 2);
        let counts = BTreeMap::new();
        assert!(evaluate_gate(FailOn::Regression, &counts, &dispositions).tripped);
        assert!(!evaluate_gate(FailOn::None, &counts, &dispositions).tripped);
    }

    /// Every gate result carries its coverage witness: an untripped gate
    /// over zero classified units is NothingInScope (a vacuous pass no
    /// consumer can mistake for verified coverage — though it is CLEAN
    /// here: an empty-scope campaign legitimately reports an untripped
    /// gate), while a real evaluation carries the non-zero EVIDENCE total.
    /// The witness's axis is [`GateAccounting`] (`checked` counts exactly
    /// the non-`Excluded` classes — the `GateResult::checked` contract,
    /// published in the design doc §7.3): a deadline backfill that
    /// classifies every untouched unit not-attempted, plan-time
    /// exclusions, and supply-failed must NOT mint coverage, while
    /// upload-rejected (which trips) and target-substituted (a real
    /// submission outcome) must. Old gate.json files without the field
    /// reload as zero coverage, the honest reading of a record that never
    /// carried one.
    #[test]
    fn gate_passes_carry_a_coverage_witness() {
        let empty = BTreeMap::new();
        let vacuous = evaluate_gate(FailOn::Regression, &empty, &empty);
        assert!(!vacuous.tripped);
        assert_eq!(vacuous.checked, 0);
        assert_eq!(vacuous.coverage(), GateCoverage::NothingInScope);

        let mut verdicts = BTreeMap::new();
        verdicts.insert(Verdict::MatchBuilt.as_str().to_string(), 7);
        let mut dispositions = BTreeMap::new();
        dispositions.insert(Disposition::CachedPrior.as_str().to_string(), 3);
        let checked = evaluate_gate(FailOn::Regression, &verdicts, &dispositions);
        assert!(!checked.tripped);
        assert_eq!(
            checked.checked, 7,
            "verdicts are evidence; the cached-prior plan-time exclusion is not"
        );
        assert_eq!(
            checked.coverage(),
            GateCoverage::Checked(std::num::NonZeroUsize::new(7).unwrap())
        );
        // The witness rides the wire and reloads.
        let json = serde_json::to_value(&checked).unwrap();
        assert_eq!(json["checked"], 7, "{json}");
        let back: GateResult = serde_json::from_value(json).unwrap();
        assert_eq!(back, checked);
        // A pre-witness gate.json (no "checked" key) reloads as zero
        // coverage instead of failing to parse.
        let legacy: GateResult = serde_json::from_value(serde_json::json!({
            "policy": "regression-gate",
            "fail_on": "regression",
            "tripped": false,
            "counts": {}
        }))
        .unwrap();
        assert_eq!(legacy.coverage(), GateCoverage::NothingInScope);
    }

    /// `checked` is decided per class by [`GateAccounting`], totally: one
    /// record of every verdict and every disposition, and the witness sums
    /// exactly the non-`Excluded` classes. The deadline backfill
    /// (not-attempted), the filtered scope cut, and supply-failed are the
    /// load-bearing exclusions — each was a vacuous-coverage hole when
    /// `checked` summed the whole classification — while upload-rejected
    /// (not attempted, yet it TRIPS) and target-substituted (submitted)
    /// pin the axis as gate evidence, not `Disposition::attempted()`.
    #[test]
    fn gate_coverage_counts_evidence_not_classification_totality() {
        let verdicts: BTreeMap<String, usize> = Verdict::ALL
            .iter()
            .map(|verdict| (verdict.as_str().to_string(), 1))
            .collect();
        let dispositions: BTreeMap<String, usize> = Disposition::ALL
            .iter()
            .map(|disposition| (disposition.as_str().to_string(), 1))
            .collect();
        let gate = evaluate_gate(FailOn::Regression, &verdicts, &dispositions);
        let expected: usize = Verdict::ALL
            .iter()
            .filter(|verdict| verdict.gate_accounting().counts_as_checked())
            .count()
            + Disposition::ALL
                .iter()
                .filter(|disposition| disposition.gate_accounting().counts_as_checked())
                .count();
        assert_eq!(gate.checked, expected);
        // The axis, spelled out: every verdict counts; of the dispositions
        // exactly upload-rejected and target-substituted do.
        assert_eq!(expected, Verdict::ALL.len() + 2);
        for disposition in Disposition::ALL {
            assert_eq!(
                disposition.gate_accounting().counts_as_checked(),
                matches!(
                    disposition,
                    Disposition::UploadRejected | Disposition::TargetSubstituted
                ),
                "{disposition:?}"
            );
        }
        // An all-backfill partial campaign — the deadline fired before
        // anything was attempted — has zero coverage, whatever its size.
        let backfill: BTreeMap<String, usize> =
            BTreeMap::from([(Disposition::NotAttempted.as_str().to_string(), 5000)]);
        let gate = evaluate_gate(FailOn::Regression, &BTreeMap::new(), &backfill);
        assert!(!gate.tripped);
        assert_eq!(gate.checked, 0, "backfill must not mint coverage");
    }

    /// The trip sets derive from [`GateAccounting`] and must match the
    /// design doc §7.3 table — the normative contract for what each
    /// `fail_on` trips on. The doc rows are parsed as data (the backticked
    /// class names), so this pins set equality in both directions: a class
    /// added to the accounting without the doc row, or vice versa, fails
    /// here. Same dev-shell-only doc access as
    /// `capability_table_pins_the_design_doc`.
    #[test]
    fn gate_trip_sets_match_the_design_doc_table() {
        let derived =
            |accounting: &dyn Fn(GateAccounting) -> bool| -> std::collections::BTreeSet<String> {
                Verdict::ALL
                    .iter()
                    .filter(|verdict| accounting(verdict.gate_accounting()))
                    .map(|verdict| verdict.as_str().to_string())
                    .chain(
                        Disposition::ALL
                            .iter()
                            .filter(|disposition| accounting(disposition.gate_accounting()))
                            .map(|disposition| disposition.as_str().to_string()),
                    )
                    .collect()
            };
        let regression = derived(&|acc| acc == GateAccounting::TripsRegression);
        let divergence_extra = derived(&|acc| acc == GateAccounting::TripsDivergence);

        // The derived sets ARE what evaluate_gate trips on (counts keys).
        let ones = |names: &std::collections::BTreeSet<String>| -> BTreeMap<String, usize> {
            names.iter().map(|name| (name.clone(), 1)).collect()
        };
        let all_verdicts: BTreeMap<String, usize> = Verdict::ALL
            .iter()
            .map(|verdict| (verdict.as_str().to_string(), 1))
            .collect();
        let all_dispositions: BTreeMap<String, usize> = Disposition::ALL
            .iter()
            .map(|disposition| (disposition.as_str().to_string(), 1))
            .collect();
        let gate = evaluate_gate(FailOn::Regression, &all_verdicts, &all_dispositions);
        assert_eq!(
            gate.counts
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            regression
        );
        let gate = evaluate_gate(FailOn::Divergence, &all_verdicts, &all_dispositions);
        assert_eq!(
            gate.counts
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            regression.union(&divergence_extra).cloned().collect()
        );
        assert!(!evaluate_gate(FailOn::None, &ones(&regression), &BTreeMap::new()).tripped);

        // Pin against the doc table rows.
        let doc = crate::test_manifest_dir().join("../docs/dev/2026-05-28-build-replay-design.md");
        let Ok(text) = std::fs::read_to_string(&doc) else {
            eprintln!(
                "design doc not present in this build's source tree; §7.3 trip-table pin not \
                 checked here (dev-shell test runs check it)"
            );
            return;
        };
        let row_classes = |row_key: &str| -> std::collections::BTreeSet<String> {
            let row = text
                .lines()
                .find(|line| line.starts_with(&format!("| `{row_key}` |")))
                .unwrap_or_else(|| panic!("§7.3 has a `{row_key}` row"));
            let cell = row
                .trim_start_matches(&format!("| `{row_key}` |"))
                .to_string();
            cell.split('`')
                .skip(1)
                .step_by(2)
                .map(str::to_string)
                .collect()
        };
        assert_eq!(
            row_classes("regression"),
            regression,
            "GateAccounting's regression trip set drifted from the §7.3 table"
        );
        let mut doc_divergence = row_classes("divergence");
        // The divergence row references the regression row by name
        // ("everything in `regression`, plus …").
        assert!(doc_divergence.remove("regression"), "divergence row shape");
        assert_eq!(
            doc_divergence, divergence_extra,
            "GateAccounting's divergence additions drifted from the §7.3 table"
        );
    }

    #[test]
    fn low_confidence_flags_derive_in_fixed_order() {
        let knobs = Knobs::default();
        let mut agg = Aggregates::default();
        let mut block = ComparabilityBlock::default();
        // Nothing over threshold, nothing recorded → no flags.
        assert!(low_confidence_flags(&agg, &knobs, &block).is_empty());
        // Every condition holds → all five flags, in the fixed order.
        agg.infra_rate_pct = Some(knobs.infra_low_confidence_pct + 0.1);
        agg.no_truth_rate_pct = Some(knobs.no_truth_threshold_pct + 0.1);
        agg.disposition_counts
            .insert(Disposition::SupplyFailed.as_str().to_string(), 2);
        block.prefetch_shortfall_pct = Some(0.5);
        block.timing_degraded = true;
        assert_eq!(
            low_confidence_flags(&agg, &knobs, &block),
            vec![
                FLAG_INFRA_INDETERMINATE_RATE,
                FLAG_NO_TRUTH_RATE,
                FLAG_PREFETCH_SHORTFALL,
                FLAG_TIMING_DEGRADED,
                FLAG_SUPPLY_FAILED_UNITS,
            ]
        );
        // Boundaries: rates exactly at their threshold, a zero shortfall,
        // and zero supply-failed units do not flag (the rate rules are
        // strictly-greater-than; the count rules are any-nonzero).
        agg.infra_rate_pct = Some(knobs.infra_low_confidence_pct);
        agg.no_truth_rate_pct = Some(knobs.no_truth_threshold_pct);
        agg.disposition_counts
            .insert(Disposition::SupplyFailed.as_str().to_string(), 0);
        block.prefetch_shortfall_pct = Some(0.0);
        block.timing_degraded = false;
        assert!(low_confidence_flags(&agg, &knobs, &block).is_empty());
    }

    /// A mid-run supply collapse must move an automated confidence signal:
    /// supply-failed retirements raise the supply-failed-units flag — the
    /// §7.2 "counted against run confidence" accounting — even though the
    /// disposition stays out of the gate's trip sets, out of `checked`,
    /// and out of every rate denominator, and even though completeness
    /// counts the retired units as terminal.
    #[test]
    fn supply_failed_units_flag_low_confidence() {
        let mut records = BTreeMap::new();
        records.insert("a".into(), rec("a", v(Verdict::MatchBuilt), 1, None, false));
        records.insert(
            "b".into(),
            rec("b", d(Disposition::SupplyFailed), 0, None, false),
        );
        let agg = aggregate(&records);
        let block = ComparabilityBlock::default();
        let flags = low_confidence_flags(&agg, &Knobs::default(), &block);
        assert_eq!(flags, vec![FLAG_SUPPLY_FAILED_UNITS]);
        // ...while the gate neither trips on it nor counts it as coverage.
        let gate = evaluate_gate(
            FailOn::Divergence,
            &agg.verdict_counts,
            &agg.disposition_counts,
        );
        assert!(!gate.tripped);
        assert_eq!(gate.checked, 1, "only the match-built verdict is evidence");
    }

    #[test]
    fn comparability_flags_low_confidence_and_merges_plan_time_flags() {
        let spec: crate::run::spec::CampaignSpec =
            serde_json::from_str(r#"{"mode":"leaf"}"#).unwrap();
        let mut campaign = CampaignRecord::new(
            "c-flags".into(),
            "2026-05-26T00:00:00Z".into(),
            spec,
            crate::run::spec::ArchivePin::default(),
        );
        // A flag recorded at plan time must survive the refresh, after the
        // report-time derivations.
        campaign.comparability.low_confidence = vec![FLAG_TENANT_UPSTREAMS_UNVERIFIED.to_string()];
        // Three attempted records, one infra-indeterminate → 33% infra rate,
        // far above the 5% default threshold; the no-truth rate stays zero.
        let mut records = BTreeMap::new();
        records.insert("a".into(), rec("a", v(Verdict::MatchBuilt), 1, None, false));
        records.insert("b".into(), rec("b", v(Verdict::MatchBuilt), 1, None, false));
        records.insert(
            "c".into(),
            rec("c", v(Verdict::InfraIndeterminate), 1, None, false),
        );
        let agg = aggregate(&records);
        let plan_counts = BTreeMap::new();
        let block = comparability_with_counts(
            &campaign.comparability,
            &agg,
            &plan_counts,
            &campaign.spec.knobs,
            None,
            None,
            &BTreeMap::new(),
        );
        assert_eq!(
            block.low_confidence,
            vec![
                FLAG_INFRA_INDETERMINATE_RATE,
                FLAG_TENANT_UPSTREAMS_UNVERIFIED
            ]
        );
        // The supply report's shortfall and the timed stats' degradation are
        // copied into the block and flagged, keeping the fixed order.
        let supply = SupplyStageReport {
            shortfall_pct: Some(2.5),
            ..SupplyStageReport::default()
        };
        let timed = TimedRunStats {
            timing_degraded: true,
            ..TimedRunStats::default()
        };
        let block = comparability_with_counts(
            &campaign.comparability,
            &agg,
            &plan_counts,
            &campaign.spec.knobs,
            Some(&supply),
            Some(&timed),
            &BTreeMap::new(),
        );
        assert_eq!(block.prefetch_shortfall_pct, Some(2.5));
        assert!(block.timing_degraded);
        assert_eq!(
            block.low_confidence,
            vec![
                FLAG_INFRA_INDETERMINATE_RATE,
                FLAG_PREFETCH_SHORTFALL,
                FLAG_TIMING_DEGRADED,
                FLAG_TENANT_UPSTREAMS_UNVERIFIED,
            ]
        );
        // Refreshing an already-refreshed block is idempotent: no duplicate
        // flags accumulate across resume cycles, and a context value persists
        // even when its stage report is no longer supplied.
        let again = comparability_with_counts(
            &block,
            &agg,
            &plan_counts,
            &campaign.spec.knobs,
            None,
            None,
            &BTreeMap::new(),
        );
        assert_eq!(again.low_confidence, block.low_confidence);
        assert_eq!(again.prefetch_shortfall_pct, Some(2.5));
        assert!(again.timing_degraded);
    }

    /// The latch-clearing direction the idempotency case above structurally
    /// cannot exercise: a refresh whose BASE carries entries the current
    /// truth no longer supports. The refreshed block is written back to
    /// campaign.json and reloaded as the base on resume, so without
    /// provenance partitioning a deadline-partial run's tiny-denominator
    /// rate flag and its drained `not-attempted` count would latch
    /// permanently. Both directions of every entry class are pinned:
    /// derived flags clear AND appear; derived counts shrink to absence
    /// AND appear; sticky plan/bootstrap flags and recorder-side counts
    /// survive (the latter from the ARCHIVE parameter, never from the
    /// base); unknown-vocabulary flags are kept conservatively.
    #[test]
    fn comparability_refresh_clears_stale_derived_flags_and_counts() {
        let spec: crate::run::spec::CampaignSpec =
            serde_json::from_str(r#"{"mode":"leaf"}"#).unwrap();
        let campaign = CampaignRecord::new(
            "c-resume".into(),
            "2026-05-26T00:00:00Z".into(),
            spec,
            crate::run::spec::ArchivePin::default(),
        );
        // The persisted base, as a deadline-partial run leaves it: a rate
        // flag derived from a 1-attempted denominator, a sticky plan flag,
        // a foreign flag from some newer engine, the recorder's eval-error
        // count, and a thousand not-attempted backfills.
        let base = ComparabilityBlock {
            low_confidence: vec![
                FLAG_INFRA_INDETERMINATE_RATE.to_string(),
                FLAG_TENANT_UPSTREAMS_UNVERIFIED.to_string(),
                "some-future-flag".to_string(),
            ],
            excluded: BTreeMap::from([
                ("eval-error".to_string(), 1),
                ("not-attempted".to_string(), 1000),
            ]),
            ..ComparabilityBlock::default()
        };
        // Current truth after the resumed run drained: ten attempted jobs,
        // all match-built — 0% infra rate, nothing not-attempted.
        let mut records = BTreeMap::new();
        for i in 0..10 {
            let name = format!("job{i}");
            records.insert(
                name.clone(),
                rec(&name, v(Verdict::MatchBuilt), 1, None, false),
            );
        }
        let agg = aggregate(&records);
        let plan_counts = BTreeMap::from([(PLAN_COUNT_IN_SCOPE.to_string(), 10)]);
        let recorder_excluded = BTreeMap::from([("eval-error".to_string(), 1)]);
        let block = comparability_with_counts(
            &base,
            &agg,
            &plan_counts,
            &campaign.spec.knobs,
            None,
            None,
            &recorder_excluded,
        );
        // Flags: the stale derived rate flag CLEARED; the sticky plan flag
        // and the unknown flag survived.
        assert_eq!(
            block.low_confidence,
            vec![FLAG_TENANT_UPSTREAMS_UNVERIFIED, "some-future-flag"],
            "a derived rate flag latched by a partial run must clear once the \
             campaign-cumulative rates no longer support it"
        );
        // Counts: the drained not-attempted count is GONE (next to an
        // honest completeness), the recorder count survives via the
        // archive parameter.
        assert_eq!(
            block.excluded,
            BTreeMap::from([("eval-error".to_string(), 1)]),
            "a drained derived class must not resurrect from the persisted base"
        );
        assert_eq!(block.completeness_pct, 100.0);

        // The appear directions, from the SAME base shapes: an infra-heavy
        // truth re-derives the rate flag, and a fresh not-attempted record
        // re-enters the counts — clearing is re-derivation, not deletion.
        let mut records = BTreeMap::new();
        records.insert("a".into(), rec("a", v(Verdict::MatchBuilt), 1, None, false));
        records.insert(
            "b".into(),
            rec("b", v(Verdict::InfraIndeterminate), 1, None, false),
        );
        records.insert(
            "c".into(),
            rec("c", d(Disposition::NotAttempted), 0, None, false),
        );
        let agg = aggregate(&records);
        let block = comparability_with_counts(
            &block,
            &agg,
            &plan_counts,
            &campaign.spec.knobs,
            None,
            None,
            &recorder_excluded,
        );
        assert!(
            block
                .low_confidence
                .contains(&FLAG_INFRA_INDETERMINATE_RATE.to_string()),
            "{:?}",
            block.low_confidence
        );
        assert_eq!(block.excluded.get("not-attempted"), Some(&1));
        assert_eq!(block.excluded.get("eval-error"), Some(&1));
    }

    /// Shared reason keys between the engine's class counts and the
    /// recorder's exclusion reasons (e.g. `eval-error`, which is both a
    /// Disposition wire string and a recorder-side reason) SUM: the two
    /// count disjoint populations — a recorder-excluded unit never became
    /// a workload unit — so the previous engine-shadows-recorder collision
    /// accident understated the exclusions.
    #[test]
    fn comparability_excluded_sums_disjoint_recorder_and_engine_populations() {
        let spec: crate::run::spec::CampaignSpec =
            serde_json::from_str(r#"{"mode":"leaf"}"#).unwrap();
        let campaign = CampaignRecord::new(
            "c-sum".into(),
            "2026-05-26T00:00:00Z".into(),
            spec,
            crate::run::spec::ArchivePin::default(),
        );
        let mut records = BTreeMap::new();
        records.insert(
            "a".into(),
            rec("a", d(Disposition::EvalError), 0, None, false),
        );
        records.insert(
            "b".into(),
            rec("b", d(Disposition::EvalError), 0, None, false),
        );
        let agg = aggregate(&records);
        let recorder_excluded = BTreeMap::from([("eval-error".to_string(), 1)]);
        let block = comparability_with_counts(
            &ComparabilityBlock::default(),
            &agg,
            &BTreeMap::new(),
            &campaign.spec.knobs,
            None,
            None,
            &recorder_excluded,
        );
        assert_eq!(block.excluded.get("eval-error"), Some(&3));
    }

    /// The flag-vocabulary partition is total: every `pub const FLAG_`
    /// declared in this file (the universe is grepped from the source, not
    /// hand-listed) appears in exactly one of [`STICKY_BASE_FLAGS`] /
    /// [`REFRESH_DERIVED_FLAGS`]. A new flag constant fails this test
    /// until it is classified — unclassified flags would silently take the
    /// conservative unknown-vocabulary path and latch forever.
    #[test]
    fn every_low_confidence_flag_is_provenance_classified() {
        let src = include_str!("report.rs");
        let prod = src
            .split_once("#[cfg(test)]")
            .map(|(prod, _)| prod)
            .unwrap_or(src);
        let declared: std::collections::BTreeSet<&str> = prod
            .lines()
            .filter_map(|line| {
                line.trim()
                    .strip_prefix("pub const FLAG_")
                    .and_then(|rest| rest.split_once(": &str = \""))
                    .and_then(|(_, value)| value.split_once('"'))
                    .map(|(value, _)| value)
            })
            .collect();
        assert!(
            !declared.is_empty(),
            "the FLAG_ constant scrape matched nothing; fix the scrape, not the registry"
        );
        let sticky: std::collections::BTreeSet<&str> = STICKY_BASE_FLAGS.iter().copied().collect();
        let derived: std::collections::BTreeSet<&str> =
            REFRESH_DERIVED_FLAGS.iter().copied().collect();
        assert!(
            sticky.is_disjoint(&derived),
            "a flag cannot be both sticky and re-derived"
        );
        let classified: std::collections::BTreeSet<&str> =
            sticky.union(&derived).copied().collect();
        assert_eq!(
            declared, classified,
            "every declared FLAG_ constant must be classified sticky-or-derived \
             (and every registry entry must be a declared flag)"
        );
    }

    #[test]
    fn summary_renders_comparability_context_rows_only_when_present() {
        let spec: crate::run::spec::CampaignSpec =
            serde_json::from_str(r#"{"mode":"leaf"}"#).unwrap();
        let mut campaign = CampaignRecord::new(
            "c-ctx".into(),
            "2026-06-01T00:00:00Z".into(),
            spec,
            crate::run::spec::ArchivePin::default(),
        );
        let mut records = BTreeMap::new();
        records.insert("a".into(), rec("a", v(Verdict::MatchBuilt), 1, None, false));
        let suspension = SuspensionSummary::default();
        let input = ReportInput {
            campaign: &campaign,
            records: &records,
            suspension: &suspension,
            generated_at: "2026-06-01T12:00:00Z".into(),
            partial: false,
            top_n: 5,
            supply: None,
            timed: None,
            abort_recommended: false,
            plan_rss_mib: None,
            plan_rss_peak_mib: None,
            recorder_excluded: &BTreeMap::new(),
        };
        let out = render_summary(&input);
        // Campaign identity (scheduling mode, supply policy) is seeded by
        // CampaignRecord::new and always renders; the optional archive and
        // confidence context rows stay absent until recorded.
        assert!(out.contains("| scheduling mode | timeless |"), "{out}");
        assert!(out.contains("| supply policy | substituters |"), "{out}");
        assert!(!out.contains("| archive created_at |"), "{out}");
        assert!(!out.contains("| archive age (days) |"), "{out}");
        assert!(!out.contains("| prefetch shortfall |"), "{out}");
        assert!(!out.contains("| timing degraded |"), "{out}");
        assert!(!out.contains("| exclusions recorded |"), "{out}");
        assert!(!out.contains("| truth collapse conflicts |"), "{out}");
        assert!(!out.contains("low confidence"), "{out}");

        // Populate the archive provenance and confidence context: every row
        // renders, and the block-recorded shortfall/degradation also flag
        // the report low-confidence. (A measured-zero conflict count stays
        // unrendered — there is nothing to disclose.)
        drop(input);
        campaign.comparability.record_archive_provenance(
            "2026-05-01T00:00:00Z".parse().unwrap(),
            "2026-06-01T00:00:00Z",
        );
        campaign.comparability.exclusions_recorded = Some(3);
        campaign.comparability.truth_collapse_conflicts = Some(2);
        campaign.comparability.prefetch_shortfall_pct = Some(2.5);
        campaign.comparability.timing_degraded = true;
        let input = ReportInput {
            campaign: &campaign,
            records: &records,
            suspension: &suspension,
            generated_at: "2026-06-01T12:00:00Z".into(),
            partial: false,
            top_n: 5,
            supply: None,
            timed: None,
            abort_recommended: false,
            plan_rss_mib: None,
            plan_rss_peak_mib: None,
            recorder_excluded: &BTreeMap::new(),
        };
        let out = render_summary(&input);
        assert!(
            out.contains("| archive created_at | 2026-05-01T00:00:00Z |"),
            "{out}"
        );
        assert!(out.contains("| archive age (days) | 31.0 |"), "{out}");
        assert!(out.contains("| scheduling mode | timeless |"), "{out}");
        assert!(out.contains("| supply policy | substituters |"), "{out}");
        assert!(out.contains("| prefetch shortfall | 2.50% |"), "{out}");
        assert!(out.contains("| timing degraded | true |"), "{out}");
        assert!(out.contains("| exclusions recorded | 3 |"), "{out}");
        assert!(out.contains("| truth collapse conflicts | 2 |"), "{out}");
        assert!(
            out.contains("| **low confidence** | prefetch-shortfall, timing-degraded |"),
            "{out}"
        );

        // Some(0) means measured-and-clean: no row to disclose.
        drop(input);
        campaign.comparability.truth_collapse_conflicts = Some(0);
        let input = ReportInput {
            campaign: &campaign,
            records: &records,
            suspension: &suspension,
            generated_at: "2026-06-01T12:00:00Z".into(),
            partial: false,
            top_n: 5,
            supply: None,
            timed: None,
            abort_recommended: false,
            plan_rss_mib: None,
            plan_rss_peak_mib: None,
            recorder_excluded: &BTreeMap::new(),
        };
        let out = render_summary(&input);
        assert!(!out.contains("| truth collapse conflicts |"), "{out}");
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
        records.insert(
            "a".into(),
            rec("a", d(Disposition::NotAttempted), 0, None, false),
        );
        let input = ReportInput {
            campaign: &campaign,
            records: &records,
            suspension: &suspension,
            generated_at: "2026-05-26T12:00:00Z".into(),
            partial: true,
            top_n: 5,
            supply: None,
            timed: None,
            abort_recommended: false,
            plan_rss_mib: None,
            plan_rss_peak_mib: None,
            recorder_excluded: &BTreeMap::new(),
        };
        write_report(&state, &input).unwrap();
        assert!(state.path("buckets/not-attempted.jsonl").exists());
        assert!(state.path("report/summary.md").exists());

        // Second render: the job moved to match-built — its old bucket file
        // must not linger.
        let mut records = BTreeMap::new();
        records.insert("a".into(), rec("a", v(Verdict::MatchBuilt), 1, None, false));
        let input = ReportInput {
            campaign: &campaign,
            records: &records,
            suspension: &suspension,
            generated_at: "2026-05-26T13:00:00Z".into(),
            partial: false,
            top_n: 5,
            supply: None,
            timed: None,
            abort_recommended: false,
            plan_rss_mib: None,
            plan_rss_peak_mib: None,
            recorder_excluded: &BTreeMap::new(),
        };
        write_report(&state, &input).unwrap();
        assert!(state.path("buckets/match-built.jsonl").exists());
        assert!(
            !state.path("buckets/not-attempted.jsonl").exists(),
            "stale bucket file from the previous render must be removed"
        );
    }
}
