//! Collect stage: turn in-band per-root build results, captured stderr
//! reasons, and scheduler poison evidence into terminal per-job records.
//!
//! For every settled batch the stage reads the per-root [`PathOutcome`]s the
//! submission returned in band, decides one [`CollectDecision`] per member
//! job (terminal or re-queue), captures evidence — NAR hashes via the store
//! for successes, a compressed log tail for failures — and appends terminal
//! [`JobRecord`]s to results.jsonl.
//!
//! Failure attribution combines two independent signals: the failing root's
//! in-band error message, falling back to the relayed stderr reason captured
//! at submission time (Signal 1), and the scheduler's failed-builder poison
//! evidence from `ListPoisoned` (Signal 2). Only their agreement counts a
//! failure as infrastructure; ambiguous, contradictory, or decayed evidence
//! defaults to a genuine target failure so rio is never given the benefit of
//! the doubt. A dependency-failed root is re-attributed through its own
//! dependency closure: a failing drv inside the closure is a real blocked
//! dependency, while a failing drv outside it means the job was merely a
//! fail-fast batch-mate and is re-queued.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Result;
use rio_nix::protocol::build::BuildStatus;

use super::artifact::ArtifactStore;
use super::classify::{
    AuxFlags, OutputHashes, TimedInterruption, classify, compare_output, job_nar_verdict,
    project_output_divergence,
};
use super::grpc::{AdminApi, StoreApi};
use super::model::{
    BATCH_KIND_TIMED, ExpectedOutcome, ExpectedSide, FailureKind, JobRecord, PathOutcome,
    RioOutcome, RioSide, RootCauseKind, UnifiedClass, Verdict, build_status_from_name,
    is_terminal_class, now_rfc3339,
};
use super::spec::Knobs;
use super::state::{StateDir, StateFile, latest_per_job};
use super::stderrparse::{ReasonClass, classify_reason, signature_for};
use super::submitter::repro_command;

/// Static per-job context assembled from the archive's workload units,
/// recorded truth, and plan output.
#[derive(Debug, Clone)]
pub struct JobContext {
    pub job: String,
    pub system: String,
    pub drv_path: String,
    /// Output name → store path (from the archive's unit record).
    pub outputs: BTreeMap<String, String>,
    /// Dependency drv closure (from the archive's closure records) — used for the
    /// fail-fast re-attribution rule.
    pub dep_drvs: HashSet<String>,
    pub expected_outcome: ExpectedOutcome,
    pub expected_outputs: BTreeMap<String, super::model::ExpectedOutput>,
    pub plan_not_attemptable: bool,
    pub plan_snapshot_valid: bool,
}

/// What collect decided for one job after looking at one settled batch.
///
/// In-band per-root results are terminal by construction (the build call
/// does not return until every requested root has an outcome), so there is
/// no still-running decision: every member of a settled batch is either
/// terminal or re-offered.
#[derive(Debug, Clone, PartialEq)]
pub enum CollectDecision {
    /// Terminal outcome — write the results.jsonl record.
    Terminal {
        rio: RioOutcome,
        evidence: Option<String>,
    },
    /// Non-terminal: re-offer to the submit loop (fail-fast batch-mate,
    /// engine-cancelled batch, infra auto-retry, missing in-band result).
    Requeue { why: &'static str },
}

/// Failure-kind resolution from the two signals (+ optional fixed-output
/// knowledge and a log tail). Ambiguous or contradictory evidence defaults
/// to [`FailureKind::Genuine`] — an unexplained failure is charged to rio,
/// never excused as infrastructure.
pub fn resolve_failure_kind(
    reason: Option<&str>,
    failed_builders: Option<&[String]>,
    is_fixed_output: Option<bool>,
    log_tail: Option<&str>,
) -> (FailureKind, Option<String>) {
    let signal1 = reason.map(classify_reason);
    // Source rot: a fixed-output derivation whose evidence text carries a
    // fetch-error signature (conservative needle list) failed because the
    // upstream source is gone, not because rio mis-built it.
    let fetchish = |text: &str| {
        [
            "unable to download",
            "couldn't resolve host",
            "Couldn't resolve host",
            "error 404",
            "404 Not Found",
            "TLS",
            "SSL",
            "timed out",
        ]
        .iter()
        .any(|n| text.contains(n))
    };
    if is_fixed_output == Some(true) {
        let evidence_text = format!(
            "{} {}",
            reason.unwrap_or_default(),
            log_tail.unwrap_or_default()
        );
        if fetchish(&evidence_text) {
            return (FailureKind::SourceRot, None);
        }
    }
    match signal1 {
        Some(ReasonClass::Timeout) => (FailureKind::Timeout, None),
        Some(ReasonClass::ResourceCeiling) => (FailureKind::ResourceCeiling, None),
        Some(ReasonClass::Infra) => match failed_builders {
            // Contradicting target evidence (real on-worker failures
            // recorded) ⇒ NOT infra: both signals must agree before a
            // failure is excused as infrastructure.
            Some(builders) if !builders.is_empty() => (FailureKind::Genuine, None),
            _ => (FailureKind::Infra, None),
        },
        Some(ReasonClass::Target) | Some(ReasonClass::Dependency { .. }) => {
            (FailureKind::Genuine, None)
        }
        None => match failed_builders {
            Some([]) => (FailureKind::Infra, None),
            Some(_) => (FailureKind::Genuine, None),
            // Signal 1 lost AND Signal 2 decayed (the failure outlived the
            // scheduler's poison-evidence TTL): only the log tail is left,
            // so the record carries the "log-tail-only" evidence-quality
            // flag.
            None => (FailureKind::Genuine, Some("log-tail-only".to_string())),
        },
    }
}

/// Inputs about the batch the job rode in (from its [`super::model::BatchRecord`]).
#[derive(Debug, Clone, Default)]
pub struct BatchView {
    /// The batch record's kind (one of the `BATCH_KIND_*` constants).
    /// Members of timed batches are never re-offered to the timeless
    /// pending pool — the timed dispatcher owns its own retries.
    pub kind: String,
    pub build_id: Option<String>,
    /// In-band per-root results returned by the submission (one entry per
    /// requested root). Together with `build_id` this distinguishes an
    /// engine-side submission failure (neither present) from a settled
    /// build.
    pub results: Vec<PathOutcome>,
    /// drv → relayed reason line (the Signal-1 fallback).
    pub reasons: BTreeMap<String, String>,
    /// Raw stderr evidence captured at submission time — for an engine-side
    /// submission failure this is the recorded `engine submission error: …`
    /// text, the only evidence the failure left behind. It becomes the
    /// terminal record's evidence when such a failure exhausts the retry
    /// budget.
    pub stderr_tail: Option<String>,
    pub engine_cancelled: bool,
    /// Root drvs for which the timed dispatcher armed a recorded
    /// interruption (empty for every non-timed batch).
    pub interruption_drvs: Vec<String>,
    pub submitted_at: Option<String>,
}

/// Derive the timed-interruption flag for one root of a settled batch:
/// `Some(Replayed)` when an interruption was armed for the drv and the
/// engine cancelled the submission (the channel was abandoned at the
/// recorded offset), `Some(NotReproduced)` when an interruption was armed
/// but the root completed successfully in band before the abandon deadline,
/// `None` otherwise — including for every batch that is not a
/// timed-dispatcher submission, so the flag can never leak into timeless
/// classification.
pub fn timed_interruption_for(
    batch: &BatchView,
    drv: &str,
    in_band_success: Option<bool>,
) -> Option<TimedInterruption> {
    let armed = batch.kind == BATCH_KIND_TIMED && batch.interruption_drvs.iter().any(|d| d == drv);
    if !armed {
        return None;
    }
    if batch.engine_cancelled {
        return Some(TimedInterruption::Replayed);
    }
    if in_band_success == Some(true) {
        return Some(TimedInterruption::NotReproduced);
    }
    None
}

/// Decide what collect does with one job given its in-band per-root result
/// and the batch-wide poison snapshot.
///
/// `target` is this root's [`PathOutcome`] (None = the submission returned
/// no result for it). `poisoned` is the batch's `ListPoisoned` snapshot
/// (drv → failed executors) — Signal 2; a drv absent from the map means the
/// evidence is unavailable, not that no builder failed it.
///
/// Signal-1 source order is binding: the root's own in-band `error_msg` is
/// primary and the captured relayed reason line for that drv is the
/// fallback. The wire collapses several scheduler-side terminal causes into
/// coarse statuses (e.g. infrastructure failures and cancellations both
/// arrive as `TransientFailure`), so attribution always goes through the
/// message text + poison evidence, never the status alone.
///
/// `prior_requeues` is the engine's TOTAL resubmission count for this job so
/// far — every requeue reason (fail-fast batch-mate, engine-cancelled batch,
/// stall requeue, …) increments it, not just infra retries. Any prior
/// requeue therefore consumes the single infra auto-retry budget
/// (`knobs.max_auto_retries`). Conservative by design: a job that already
/// burned an engine re-offer is not granted an extra infra retry on top, so
/// the budget can never multiply across requeue reasons.
pub fn decide(
    ctx: &JobContext,
    target: Option<&PathOutcome>,
    batch: &BatchView,
    poisoned: &HashMap<String, Vec<String>>,
    prior_requeues: u32,
    knobs: &Knobs,
    log_tail: Option<&str>,
) -> CollectDecision {
    let relayed = batch.reasons.get(&ctx.drv_path).map(String::as_str);
    let Some(target) = target else {
        // No in-band result for this root. An engine-cancelled batch
        // (deadline/abort: the channel was abandoned before results arrived)
        // is always re-offered; otherwise a missing result is a transport
        // defect — one auto-retry, then an infra failure.
        if batch.engine_cancelled {
            return CollectDecision::Requeue {
                why: "engine-cancelled",
            };
        }
        if prior_requeues < knobs.max_auto_retries {
            return CollectDecision::Requeue {
                why: "no-inband-result",
            };
        }
        return CollectDecision::Terminal {
            rio: RioOutcome::TargetFailed {
                kind: FailureKind::Infra,
            },
            evidence: Some("no-inband-result".to_string()),
        };
    };
    // Signal 1: the root's own error message first, the relayed line second.
    let signal1 = Some(target.error_msg.as_str())
        .filter(|m| !m.is_empty())
        .or(relayed);
    let Some(status) = build_status_from_name(&target.status) else {
        // Defensive: writers go through `build_status_name`, so an
        // unrecognized status string cannot normally appear. Treat it as a
        // failure and surface the raw string as evidence.
        let (kind, _) = resolve_failure_kind(
            signal1,
            poisoned.get(&ctx.drv_path).map(Vec::as_slice),
            None,
            log_tail,
        );
        return CollectDecision::Terminal {
            rio: RioOutcome::TargetFailed { kind },
            evidence: Some(format!("unrecognized in-band status: {}", target.status)),
        };
    };
    if status == BuildStatus::Built {
        return CollectDecision::Terminal {
            rio: RioOutcome::Built { executed: true },
            evidence: None,
        };
    }
    if status.is_success() {
        // Substituted / AlreadyValid / ResolvesToAlreadyValid: completed
        // without execution; the completed-without-execution discriminator
        // (cached-prior vs target-substituted) is the classifier's job,
        // driven by the plan-snapshot flag.
        return CollectDecision::Terminal {
            rio: RioOutcome::Built { executed: false },
            evidence: None,
        };
    }
    if status == BuildStatus::DependencyFailed {
        // The trigger drv comes from the dependency-shaped Signal-1 message
        // (`dependency '<drv>' failed: …`).
        let trigger = signal1.map(classify_reason).and_then(|c| match c {
            ReasonClass::Dependency { failing_drv } => Some(failing_drv),
            _ => None,
        });
        let Some(trigger) = trigger else {
            // No identifiable trigger: treat as a fail-fast batch-mate.
            return CollectDecision::Requeue {
                why: "dependency-failed-no-trigger",
            };
        };
        if !ctx.dep_drvs.contains(&trigger) && trigger != ctx.drv_path {
            // Fail-fast marks unrelated batch-mates dependency-failed — the
            // trigger is not in this job's own closure, so the job never got
            // a fair attempt and is re-queued instead of being charged with
            // a dependency failure.
            return CollectDecision::Requeue {
                why: "failfast-batch-mate",
            };
        }
        // Root-cause classification of the trigger, so dependents of an
        // infra-poisoned or source-rotted dependency cascade out of the
        // headline instead of being charged as rio failures.
        let trigger_signal1 = batch.reasons.get(&trigger).map(String::as_str);
        let (kind, evidence) = resolve_failure_kind(
            trigger_signal1,
            poisoned.get(&trigger).map(Vec::as_slice),
            None,
            None,
        );
        let root = match kind {
            FailureKind::Infra => RootCauseKind::Infra,
            FailureKind::SourceRot => RootCauseKind::SourceRot,
            _ => RootCauseKind::Genuine,
        };
        return CollectDecision::Terminal {
            rio: RioOutcome::DependencyFailed {
                root,
                failing_drv: trigger,
            },
            evidence,
        };
    }
    // Every other failure status (PermanentFailure, TransientFailure,
    // TimedOut, MiscFailure, CachedFailure, LogLimitExceeded,
    // NotDeterministic, OutputRejected, InputRejected, NoSubstituters): the
    // two-signal rule decides the kind; positively-identified infra gets the
    // single auto-retry while budget remains.
    let (kind, evidence) = resolve_failure_kind(
        signal1,
        poisoned.get(&ctx.drv_path).map(Vec::as_slice),
        None,
        log_tail,
    );
    if kind == FailureKind::Infra && prior_requeues < knobs.max_auto_retries {
        return CollectDecision::Requeue {
            why: "infra-auto-retry",
        };
    }
    CollectDecision::Terminal {
        rio: RioOutcome::TargetFailed { kind },
        evidence,
    }
}

/// The kebab-case failure-cause string for a failure-shaped rio outcome:
/// the target's own [`FailureKind`] serde form, or the blocking
/// dependency's [`RootCauseKind`] serde form. `None` for non-failure
/// outcomes.
pub fn failure_cause_for(rio: &RioOutcome) -> Option<String> {
    let value = match rio {
        RioOutcome::TargetFailed { kind } => serde_json::to_value(kind).ok()?,
        RioOutcome::DependencyFailed { root, .. } => serde_json::to_value(root).ok()?,
        RioOutcome::NotAttempted | RioOutcome::Built { .. } => return None,
    };
    value.as_str().map(str::to_string)
}

/// Assemble the final [`JobRecord`] for a terminal collect decision.
///
/// `campaign_id` feeds the record's `repro` field — the engine-native
/// single-unit re-run command ([`repro_command`]) is keyed by campaign id
/// and drv path.
///
/// `log_tail` is the captured failure-log text (when any was fetched); it
/// only feeds the failure-signature fallback, so failures whose Signal-1
/// text was lost can still be grouped by their log evidence. The signature
/// key follows the Signal-1 source order (in-band `error_msg` first, the
/// relayed line second); the relayed line itself is retained verbatim on
/// the record as evidence.
#[allow(clippy::too_many_arguments)]
pub fn build_record(
    ctx: &JobContext,
    rio_outcome: &RioOutcome,
    evidence: Option<String>,
    target: Option<&PathOutcome>,
    batch: &BatchView,
    poisoned: &HashMap<String, Vec<String>>,
    rio_paths: &HashMap<String, Option<(crate::narhash::NarHash, u64)>>,
    mode: &str,
    campaign_id: &str,
    attempts: u32,
    log_key: Option<String>,
    first_active_at: Option<String>,
    log_tail: Option<&str>,
) -> JobRecord {
    // "Success" for the interruption rule means the in-band result settled
    // with any success status (built, substituted, already valid) — the root
    // completed before the armed abandon deadline could fire.
    let in_band_success = target
        .and_then(|t| build_status_from_name(&t.status))
        .map(|s| s.is_success());
    let aux = AuxFlags {
        skipped: None,
        eval_error: false,
        plan_not_attemptable: ctx.plan_not_attemptable,
        plan_snapshot_valid: ctx.plan_snapshot_valid,
        resolve_unknown_divergent: None,
        timed_interruption: timed_interruption_for(batch, &ctx.drv_path, in_band_success),
    };
    let classification = classify(&ctx.expected_outcome, rio_outcome, &aux);
    let reason = batch.reasons.get(&ctx.drv_path).cloned();
    let signal1 = target
        .map(|t| t.error_msg.clone())
        .filter(|m| !m.is_empty())
        .or_else(|| reason.clone());

    let mut rio_outputs = BTreeMap::new();
    let mut nar_compare = BTreeMap::new();
    for (name, path) in &ctx.outputs {
        let rio_info = rio_paths.get(path).cloned().flatten();
        rio_outputs.insert(
            name.clone(),
            super::model::RioOutput {
                path: path.clone(),
                nar_hash: rio_info.as_ref().map(|(h, _)| *h),
                nar_size: rio_info.as_ref().map(|(_, s)| *s),
            },
        );
        if classification.class == UnifiedClass::Verdict(Verdict::MatchBuilt) {
            nar_compare.insert(
                name.clone(),
                compare_output(&OutputHashes {
                    rio: rio_info.as_ref().map(|(h, _)| *h),
                    expected: ctx.expected_outputs.get(name).and_then(|h| h.nar_hash),
                })
                .to_string(),
            );
        }
    }

    // Final per-unit class: a match-built verdict whose recorded output
    // hashes disagree is projected to output-divergence; dispositions pass
    // through untouched. Exactly one of verdict/disposition is set.
    let (verdict, disposition) = match classification.class {
        UnifiedClass::Verdict(v) => (
            Some(project_output_divergence(v, job_nar_verdict(&nar_compare))),
            None,
        ),
        UnifiedClass::Disposition(d) => (None, Some(d)),
    };
    // The surviving failure cause is recorded only for failure-class
    // verdicts (the cause of a requeued or excluded unit is not a final
    // observation).
    let failure_cause = match verdict {
        Some(
            Verdict::UnexpectedFailure
            | Verdict::UnexpectedDependencyFailure
            | Verdict::MatchFailed
            | Verdict::InfraIndeterminate
            | Verdict::SourceUnavailable,
        ) => failure_cause_for(rio_outcome),
        _ => None,
    };
    let flaky = attempts > 1
        && matches!(
            verdict,
            Some(Verdict::MatchBuilt | Verdict::OutputDivergence)
        );

    let expected_side = ExpectedSide {
        outcome: ctx.expected_outcome.as_str().to_string(),
        outputs: ctx.expected_outputs.clone(),
    };
    let rio_side = RioSide {
        outcome: rio_outcome.outcome_str().to_string(),
        status: target.map(|t| t.status.clone()),
        exec_id: None,
        failing_drv: match rio_outcome {
            RioOutcome::DependencyFailed { failing_drv, .. } => Some(failing_drv.clone()),
            _ => None,
        },
        reason: reason.clone(),
        failed_builders: poisoned.get(&ctx.drv_path).cloned().unwrap_or_default(),
        durations: super::model::Durations {
            submitted_at: batch.submitted_at.clone(),
            first_active_at,
            terminal_at: Some(now_rfc3339()),
        },
        outputs: rio_outputs,
    };
    JobRecord {
        job: ctx.job.clone(),
        system: ctx.system.clone(),
        drv_path: ctx.drv_path.clone(),
        mode: mode.to_string(),
        attempts,
        build_ids: batch.build_id.clone().into_iter().collect(),
        rio: rio_side,
        expected: expected_side,
        nar_compare,
        verdict: verdict.map(|v| v.as_str().to_string()),
        disposition: disposition.map(|d| d.as_str().to_string()),
        cascaded: classification.cascaded,
        failure_cause,
        flaky,
        signature: match rio_outcome {
            RioOutcome::Built { .. } => None,
            _ => signature_for(signal1.as_deref(), log_tail),
        },
        log_key,
        repro: repro_command(campaign_id, &ctx.drv_path),
        evidence,
        updated_at: now_rfc3339(),
    }
}

/// Decode captured log-tail bytes for evidence classification. Lossy on
/// purpose: builder logs may contain non-UTF-8 bytes and the text is only
/// scanned for fetch-error needles / kept as display evidence — this is
/// log display, not a wire parse path (the case clippy.toml's
/// `disallowed-methods` rationale carves out).
fn lossy_log_text(bytes: &[u8]) -> String {
    #[allow(clippy::disallowed_methods)]
    String::from_utf8_lossy(bytes).into_owned()
}

/// Process one settled batch end-to-end: take its in-band per-root results,
/// decide each job, capture evidence (log tails for failures, NAR hashes
/// for successes), append terminal records, and return the jobs that must
/// be re-queued.
///
/// `prior_requeues` carries each job's TOTAL engine resubmission count so
/// far (every requeue reason counts, not just infra retries) — see
/// [`decide`] for why any prior requeue consumes the infra auto-retry
/// budget.
#[allow(clippy::too_many_arguments)]
pub async fn process_settled_batch(
    state: &StateDir,
    admin: &dyn AdminApi,
    store: &dyn StoreApi,
    artifacts: Option<(&dyn ArtifactStore, String)>,
    contexts: &HashMap<String, JobContext>,
    batch_jobs: &[String],
    batch: &BatchView,
    prior_requeues: &HashMap<String, u32>,
    knobs: &Knobs,
    mode: &str,
    campaign_id: &str,
    first_active: &HashMap<String, String>,
) -> Result<Vec<String>> {
    let mut requeue = Vec::new();
    // Members of timed batches are never re-offered to the timeless pending
    // pool: the timed dispatcher owns its own retries (confirmation
    // re-submissions), and whatever stays unresolved is covered by the
    // end-of-run not-attempted backfill.
    let timed = batch.kind == BATCH_KIND_TIMED;
    if batch.results.is_empty() && batch.build_id.is_none() && !(timed && batch.engine_cancelled) {
        // Neither in-band results nor a build id: an engine-side submission
        // failure (channel open, drv import, the build op erroring before
        // any result arrived). Members of a timed batch are simply left to
        // the timed dispatcher, which owns its own retries. (A timed batch
        // the engine cancelled with no results falls through instead, so
        // its armed interruptions are still recorded below.)
        if timed {
            tracing::info!(
                jobs = batch_jobs.len(),
                "timed batch has no in-band results and no build id (engine-side submission \
                 failure); leaving its members to the timed dispatcher"
            );
            return Ok(Vec::new());
        }
        // An engine-cancelled submission (batch deadline, abort) is the
        // engine's own act, not a transport defect: every member is
        // re-offered without consuming budget — the engine-cancelled rule
        // in [`decide`].
        if batch.engine_cancelled {
            tracing::info!(
                jobs = batch_jobs.len(),
                "engine-cancelled batch settled with no in-band results and no build id; \
                 re-offering its jobs"
            );
            return Ok(batch_jobs.to_vec());
        }
        // Otherwise the failure consumes the same bounded auto-retry budget
        // as a missing in-band result ([`decide`] with no target): re-offer
        // while budget remains, then terminalize as an infrastructure
        // failure carrying the recorded submission error as evidence.
        // Without the bound, a deterministic submission failure (gateway
        // unreachable, host-key mismatch, drv import error) would re-offer
        // its members on every wave and the campaign would never drain.
        // No poison snapshot or log tail is fetched: the failure pre-dates
        // any build, so the scheduler holds no evidence for it.
        let evidence = batch
            .stderr_tail
            .clone()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "engine-submission-failure".to_string());
        // Duplicate-batch belt: the submit loop can re-submit a job whose
        // terminal record landed between settle and collect (the cool-down
        // damper narrows that window but does not close it). Such a
        // duplicate must never overwrite the job's real verdict under
        // latest-record-per-job semantics, so members already terminal in
        // results.jsonl are dropped here — neither re-offered nor recorded.
        let already_terminal: HashSet<String> =
            latest_per_job(state.load_jsonl(StateFile::Results)?)
                .into_iter()
                .filter(|(_, r)| is_terminal_class(&r.verdict, &r.disposition))
                .map(|(job, _)| job)
                .collect();
        for job in batch_jobs {
            let Some(ctx) = contexts.get(job) else {
                tracing::warn!(job, "batch member has no job context; skipping");
                continue;
            };
            if already_terminal.contains(job) {
                tracing::info!(
                    job,
                    "member of a failed submission already has a terminal record; dropping"
                );
                continue;
            }
            let prior = prior_requeues.get(job).copied().unwrap_or(0);
            if prior < knobs.max_auto_retries {
                tracing::info!(job, why = "engine-submission-failure", "re-queueing");
                requeue.push(job.clone());
                continue;
            }
            tracing::info!(
                job,
                prior_requeues = prior,
                "engine-side submission failure with no retry budget left; recording an \
                 infrastructure failure"
            );
            let record = build_record(
                ctx,
                &RioOutcome::TargetFailed {
                    kind: FailureKind::Infra,
                },
                Some(evidence.clone()),
                None,
                batch,
                &HashMap::new(),
                &HashMap::new(),
                mode,
                campaign_id,
                prior + 1,
                None,
                first_active.get(job).cloned(),
                None,
            );
            state.append_jsonl(StateFile::Results, &record)?;
        }
        return Ok(requeue);
    }
    let results_by_drv: HashMap<&str, &PathOutcome> = batch
        .results
        .iter()
        .map(|r| (r.drv_path.as_str(), r))
        .collect();
    // Signal 2: one ListPoisoned snapshot per batch, fetched only when at
    // least one member's in-band status is a non-success (successes never
    // need poison evidence). An RPC failure degrades to an empty map —
    // evidence unavailable, never fatal to the pass.
    let any_failure = batch_jobs.iter().any(|job| {
        contexts.get(job).is_some_and(|ctx| {
            results_by_drv
                .get(ctx.drv_path.as_str())
                .is_some_and(|r| !build_status_from_name(&r.status).is_some_and(|s| s.is_success()))
        })
    });
    let poisoned: HashMap<String, Vec<String>> = if any_failure {
        match admin.list_poisoned().await {
            Ok(rows) => rows
                .into_iter()
                .map(|p| (p.drv_path, p.failed_executors))
                .collect(),
            Err(e) => {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    "ListPoisoned failed; classifying this batch without poison evidence"
                );
                HashMap::new()
            }
        }
    } else {
        HashMap::new()
    };

    for job in batch_jobs {
        let Some(ctx) = contexts.get(job) else {
            tracing::warn!(job, "batch member has no job context; skipping");
            continue;
        };
        let target = results_by_drv.get(ctx.drv_path.as_str()).copied();
        let prior = prior_requeues.get(job).copied().unwrap_or(0);
        // Evidence-age gate: when a failed root carries neither an in-band
        // error message nor a relayed reason (Signal 1) and has no
        // ListPoisoned entry (Signal 2 — the scheduler's poison rows decay
        // with its evidence TTL), fetch the log tail as the third signal;
        // the record then carries the "log-tail-only" evidence flag from
        // [`resolve_failure_kind`].
        let target_status = target.and_then(|t| build_status_from_name(&t.status));
        let target_is_failure = target.is_some() && !target_status.is_some_and(|s| s.is_success());
        let needs_log_signal = target_is_failure
            && target_status != Some(BuildStatus::DependencyFailed)
            && target.is_some_and(|t| t.error_msg.is_empty())
            && !batch.reasons.contains_key(&ctx.drv_path)
            && !poisoned.contains_key(&ctx.drv_path);
        let mut log_signal_bytes = if needs_log_signal {
            admin
                .log_tail(&ctx.drv_path, None, knobs.log_tail_bytes)
                .await
                .ok()
        } else {
            None
        };
        // An empty fetch carries no evidence: drop it here so an empty tail
        // can never become a meaningless `log:` failure signature (or an
        // empty captured tail) downstream.
        let log_signal_text = log_signal_bytes
            .as_deref()
            .filter(|bytes| !bytes.is_empty())
            .map(lossy_log_text);
        match decide(
            ctx,
            target,
            batch,
            &poisoned,
            prior,
            knobs,
            log_signal_text.as_deref(),
        ) {
            CollectDecision::Requeue { why } => {
                if timed {
                    // Never re-offered (the timed dispatcher owns retries).
                    // An armed interruption the engine did cancel is the
                    // recorded outcome reproduced, so it still gets its
                    // terminal record here; every other requeue-shaped
                    // member stays outstanding for a later
                    // confirmation-retry batch or the end-of-run backfill.
                    if timed_interruption_for(batch, &ctx.drv_path, None)
                        == Some(TimedInterruption::Replayed)
                    {
                        let record = build_record(
                            ctx,
                            &RioOutcome::NotAttempted,
                            None,
                            target,
                            batch,
                            &poisoned,
                            &HashMap::new(),
                            mode,
                            campaign_id,
                            prior + 1,
                            None,
                            first_active.get(job).cloned(),
                            None,
                        );
                        state.append_jsonl(StateFile::Results, &record)?;
                    } else {
                        tracing::info!(job, why, "timed batch member is not re-offered");
                    }
                } else {
                    tracing::info!(job, why, "re-queueing");
                    requeue.push(job.clone());
                }
            }
            CollectDecision::Terminal { rio, evidence } => {
                // Evidence capture: NAR hashes for successes, log tail for
                // failures.
                let mut log_key = None;
                let mut captured_tail = log_signal_text.clone();
                let rio_paths = if matches!(rio, RioOutcome::Built { .. }) {
                    let paths: Vec<String> = ctx.outputs.values().cloned().collect();
                    match store.query_valid(&paths).await {
                        Ok(map) => map,
                        Err(e) => {
                            tracing::warn!(
                                job,
                                error = %format!("{e:#}"),
                                "BatchQueryPathInfo failed; recording the success without NAR \
                                 identity"
                            );
                            HashMap::new()
                        }
                    }
                } else {
                    // Failure: capture the log tail while the scheduler still
                    // has it and upload it next to the campaign artifacts.
                    // Reuse the Signal-3 fetch when it already happened — a
                    // second fetch would race the scheduler's log retention
                    // for the same bytes.
                    let tail = match log_signal_bytes.take() {
                        Some(bytes) => Some(bytes),
                        None => match admin
                            .log_tail(&ctx.drv_path, None, knobs.log_tail_bytes)
                            .await
                        {
                            Ok(tail) => Some(tail),
                            Err(e) => {
                                tracing::warn!(
                                    job,
                                    error = %format!("{e:#}"),
                                    "log tail fetch failed; recording the failure without a log"
                                );
                                None
                            }
                        },
                    };
                    if let Some(tail) = tail.filter(|t| !t.is_empty()) {
                        if captured_tail.is_none() {
                            captured_tail = Some(lossy_log_text(&tail));
                        }
                        let compressed = zstd::encode_all(tail.as_slice(), 3).unwrap_or(tail);
                        let rel = format!("logs/{}.log.zst", ctx.job.replace('/', "_"));
                        state.write_bytes(&rel, &compressed)?;
                        if let Some((art, prefix)) = &artifacts {
                            // The S3 key is deterministic and the periodic
                            // state sync re-enumerates logs/*.log.zst, so the
                            // record carries the key even when this immediate
                            // upload fails — the sync retries it from the
                            // local copy.
                            let key = format!("{prefix}/{rel}");
                            if let Err(e) = art.put_bytes(&key, compressed).await {
                                tracing::warn!(
                                    job,
                                    key,
                                    error = %format!("{e:#}"),
                                    "log tail upload failed; the periodic state sync will retry it"
                                );
                            }
                            log_key = Some(key);
                        } else {
                            log_key = Some(rel);
                        }
                    }
                    HashMap::new()
                };
                let record = build_record(
                    ctx,
                    &rio,
                    evidence,
                    target,
                    batch,
                    &poisoned,
                    &rio_paths,
                    mode,
                    campaign_id,
                    prior + 1,
                    log_key,
                    first_active.get(job).cloned(),
                    captured_tail.as_deref(),
                );
                state.append_jsonl(StateFile::Results, &record)?;
            }
        }
    }
    Ok(requeue)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::artifact::LocalDirArtifactStore;
    use crate::run::grpc::test_support::FakeStoreApi;
    use crate::run::grpc::{GraphSnapshot, PoisonedView};
    use crate::run::model::{BATCH_KIND_SUBMIT, Disposition, build_status_name};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn ctx(job: &str, drv: &str, deps: &[&str], expected: ExpectedOutcome) -> JobContext {
        JobContext {
            job: job.to_string(),
            system: "x86_64-linux".into(),
            drv_path: drv.to_string(),
            outputs: BTreeMap::from([(
                "out".to_string(),
                format!("{}-out", drv.trim_end_matches(".drv")),
            )]),
            dep_drvs: deps.iter().map(|s| s.to_string()).collect(),
            expected_outcome: expected,
            expected_outputs: BTreeMap::new(),
            plan_not_attemptable: false,
            plan_snapshot_valid: false,
        }
    }

    fn po(drv: &str, status: BuildStatus, error: &str) -> PathOutcome {
        PathOutcome {
            drv_path: drv.to_string(),
            status: build_status_name(status).to_string(),
            error_msg: error.to_string(),
            start_time: 0,
            stop_time: 0,
        }
    }

    const T: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-app.drv";
    const DEP: &str = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-dep.drv";
    const OTHER: &str = "/nix/store/cccccccccccccccccccccccccccccccc-other.drv";

    /// Scripted AdminApi for evidence capture: the build graph is never read
    /// (collection is in-band), `list_poisoned` returns a configurable
    /// snapshot (or fails) and counts its calls so the once-per-batch /
    /// only-on-failure gating can be asserted, and every log tail is the
    /// same configurable text (a short gcc error by default) with the calls
    /// counted so the Signal-3 reuse (one fetch serving both classification
    /// and evidence capture) can be asserted.
    struct LogAdmin {
        log_calls: AtomicUsize,
        poisoned_calls: AtomicUsize,
        tail: Vec<u8>,
        poisoned: Vec<PoisonedView>,
        fail_poisoned: bool,
    }
    impl Default for LogAdmin {
        fn default() -> Self {
            Self {
                log_calls: AtomicUsize::new(0),
                poisoned_calls: AtomicUsize::new(0),
                tail: b"gcc: fatal error\n".to_vec(),
                poisoned: Vec::new(),
                fail_poisoned: false,
            }
        }
    }
    #[async_trait::async_trait]
    impl AdminApi for LogAdmin {
        async fn get_build_graph(&self, _b: &str) -> Result<GraphSnapshot> {
            unreachable!("collection is in-band; collect never reads the build graph")
        }
        async fn list_poisoned(&self) -> Result<Vec<PoisonedView>> {
            self.poisoned_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_poisoned {
                anyhow::bail!("ListPoisoned: scheduler unavailable");
            }
            Ok(self.poisoned.clone())
        }
        async fn log_tail(&self, _d: &str, _e: Option<&str>, _m: usize) -> Result<Vec<u8>> {
            self.log_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.tail.clone())
        }
        async fn list_builds(&self, _t: &str, _l: u32) -> Result<Vec<(String, Option<String>)>> {
            Ok(vec![])
        }
    }

    /// Artifact store whose uploads always fail — at-capture upload failures
    /// must not strand the evidence pointer (see
    /// `failed_log_upload_still_records_the_log_key`).
    struct FailingArtifacts;
    #[async_trait::async_trait]
    impl ArtifactStore for FailingArtifacts {
        async fn put_bytes(&self, _key: &str, _bytes: Vec<u8>) -> Result<()> {
            anyhow::bail!("simulated S3 outage")
        }
        async fn get_bytes(&self, _key: &str) -> Result<Option<Vec<u8>>> {
            Ok(None)
        }
        async fn exists(&self, _key: &str) -> Result<bool> {
            Ok(false)
        }
    }

    #[test]
    fn two_signal_rule_matrix() {
        // Signal1 infra + signal2 no-builders → Infra.
        assert_eq!(
            resolve_failure_kind(
                Some("max_infra_retries=3 exhausted after infrastructure failures: x"),
                Some(&[]),
                None,
                None
            )
            .0,
            FailureKind::Infra
        );
        // Signal1 infra + contradicting target evidence → Genuine (the two
        // signals must agree).
        assert_eq!(
            resolve_failure_kind(
                Some("max_infra_retries=3 exhausted after infrastructure failures: x"),
                Some(&["b1".to_string()]),
                None,
                None
            )
            .0,
            FailureKind::Genuine
        );
        // No signal1, poisoned with empty failed_builders → Infra.
        assert_eq!(
            resolve_failure_kind(None, Some(&[]), None, None).0,
            FailureKind::Infra
        );
        // No signal1, failed_builders present → Genuine.
        assert_eq!(
            resolve_failure_kind(None, Some(&["b1".to_string()]), None, None).0,
            FailureKind::Genuine
        );
        // Both signals lost (evidence decayed) → Genuine + log-tail-only flag.
        let (kind, flag) = resolve_failure_kind(None, None, None, Some("error: whatever"));
        assert_eq!(kind, FailureKind::Genuine);
        assert_eq!(flag.as_deref(), Some("log-tail-only"));
        // Timeout / resource ceiling get their own kinds.
        assert_eq!(
            resolve_failure_kind(
                Some("max_timeout_retries=2 exhausted (DeadlineExceeded backstop)"),
                Some(&[]),
                None,
                None
            )
            .0,
            FailureKind::Timeout
        );
        assert_eq!(
            resolve_failure_kind(
                Some("max_infra_retries=3 exhausted at resource ceiling (OomKilled)"),
                Some(&[]),
                None,
                None
            )
            .0,
            FailureKind::ResourceCeiling
        );
        // FOD + fetch-error signature → SourceRot.
        assert_eq!(
            resolve_failure_kind(
                Some("builder failed: unable to download 'https://example.com/src.tar.gz'"),
                Some(&["b1".to_string()]),
                Some(true),
                None
            )
            .0,
            FailureKind::SourceRot
        );
        // FOD without fetch signature stays genuine.
        assert_eq!(
            resolve_failure_kind(
                Some("builder failed: hash mismatch"),
                Some(&["b1".to_string()]),
                Some(true),
                None
            )
            .0,
            FailureKind::Genuine
        );
    }

    #[test]
    fn decide_covers_in_band_status_matrix() {
        let knobs = Knobs::default();
        let c = ctx("app.x86_64-linux", T, &[DEP], ExpectedOutcome::Built);
        let batch = BatchView::default();
        let no_poison: HashMap<String, Vec<String>> = HashMap::new();

        // Built → executed.
        assert_eq!(
            decide(
                &c,
                Some(&po(T, BuildStatus::Built, "")),
                &batch,
                &no_poison,
                0,
                &knobs,
                None
            ),
            CollectDecision::Terminal {
                rio: RioOutcome::Built { executed: true },
                evidence: None
            }
        );
        // Substituted / AlreadyValid / ResolvesToAlreadyValid → completed
        // without execution (classifier discriminates cached-prior vs
        // target-substituted via the plan-snapshot flag).
        for status in [
            BuildStatus::Substituted,
            BuildStatus::AlreadyValid,
            BuildStatus::ResolvesToAlreadyValid,
        ] {
            assert_eq!(
                decide(
                    &c,
                    Some(&po(T, status, "")),
                    &batch,
                    &no_poison,
                    0,
                    &knobs,
                    None
                ),
                CollectDecision::Terminal {
                    rio: RioOutcome::Built { executed: false },
                    evidence: None
                },
                "{status:?}"
            );
        }
        // Failure with an infra reason and corroborating poison evidence
        // (entry with no failed builders): auto-retry once, then terminal
        // infra. The wire collapses infra causes into TransientFailure, so
        // the message text drives the attribution, not the status.
        let infra = po(
            T,
            BuildStatus::TransientFailure,
            "max_infra_retries=3 exhausted after infrastructure failures: x",
        );
        let poisoned_no_builders: HashMap<String, Vec<String>> =
            HashMap::from([(T.to_string(), vec![])]);
        assert_eq!(
            decide(
                &c,
                Some(&infra),
                &batch,
                &poisoned_no_builders,
                0,
                &knobs,
                None
            ),
            CollectDecision::Requeue {
                why: "infra-auto-retry"
            }
        );
        assert!(matches!(
            decide(
                &c,
                Some(&infra),
                &batch,
                &poisoned_no_builders,
                1,
                &knobs,
                None
            ),
            CollectDecision::Terminal {
                rio: RioOutcome::TargetFailed {
                    kind: FailureKind::Infra
                },
                ..
            }
        ));
        // Same infra reason but contradicting worker evidence → genuine,
        // terminal immediately (the two signals must agree).
        let poisoned_with_builders: HashMap<String, Vec<String>> =
            HashMap::from([(T.to_string(), vec!["b1".to_string()])]);
        assert!(matches!(
            decide(
                &c,
                Some(&infra),
                &batch,
                &poisoned_with_builders,
                0,
                &knobs,
                None
            ),
            CollectDecision::Terminal {
                rio: RioOutcome::TargetFailed {
                    kind: FailureKind::Genuine
                },
                ..
            }
        ));
        // A permanent failure with a worker reason is genuine, terminal
        // immediately.
        assert!(matches!(
            decide(
                &c,
                Some(&po(
                    T,
                    BuildStatus::PermanentFailure,
                    "builder failed with exit code 2"
                )),
                &batch,
                &no_poison,
                0,
                &knobs,
                None
            ),
            CollectDecision::Terminal {
                rio: RioOutcome::TargetFailed {
                    kind: FailureKind::Genuine
                },
                ..
            }
        ));
        // TimedOut with the scheduler's timeout reason keeps the timeout
        // kind (status adds texture; the reason text decides).
        assert!(matches!(
            decide(
                &c,
                Some(&po(
                    T,
                    BuildStatus::TimedOut,
                    "max_timeout_retries=2 exhausted (DeadlineExceeded backstop)"
                )),
                &batch,
                &no_poison,
                0,
                &knobs,
                None
            ),
            CollectDecision::Terminal {
                rio: RioOutcome::TargetFailed {
                    kind: FailureKind::Timeout
                },
                ..
            }
        ));
        // Unrecognized status string (defensive): a terminal failure
        // carrying the raw status as evidence.
        let bogus = PathOutcome {
            drv_path: T.to_string(),
            status: "bogus-status".to_string(),
            ..PathOutcome::default()
        };
        match decide(&c, Some(&bogus), &batch, &no_poison, 0, &knobs, None) {
            CollectDecision::Terminal {
                rio: RioOutcome::TargetFailed { .. },
                evidence,
            } => assert!(evidence.unwrap().contains("bogus-status")),
            other => panic!("expected a terminal failure, got {other:?}"),
        }
        // Missing in-band result: engine-cancelled wins regardless of the
        // budget; otherwise one requeue, then terminal infra with the
        // missing-result evidence.
        let cancelled_batch = BatchView {
            engine_cancelled: true,
            ..BatchView::default()
        };
        assert_eq!(
            decide(&c, None, &cancelled_batch, &no_poison, 5, &knobs, None),
            CollectDecision::Requeue {
                why: "engine-cancelled"
            }
        );
        assert_eq!(
            decide(&c, None, &batch, &no_poison, 0, &knobs, None),
            CollectDecision::Requeue {
                why: "no-inband-result"
            }
        );
        match decide(&c, None, &batch, &no_poison, 1, &knobs, None) {
            CollectDecision::Terminal {
                rio:
                    RioOutcome::TargetFailed {
                        kind: FailureKind::Infra,
                    },
                evidence,
            } => assert_eq!(evidence.as_deref(), Some("no-inband-result")),
            other => panic!("expected terminal infra, got {other:?}"),
        }
    }

    /// Signal-1 source order is binding: the root's own in-band error
    /// message wins over the captured relayed line; the relayed line is the
    /// fallback when the in-band message is empty.
    #[test]
    fn signal1_prefers_error_msg_then_relayed_reason() {
        let knobs = Knobs::default();
        let c = ctx("app.x86_64-linux", T, &[], ExpectedOutcome::Built);
        let poisoned: HashMap<String, Vec<String>> = HashMap::from([(T.to_string(), vec![])]);
        let batch = BatchView {
            reasons: BTreeMap::from([(
                T.to_string(),
                "max_infra_retries=3 exhausted after infrastructure failures: x".to_string(),
            )]),
            ..BatchView::default()
        };
        // Relayed line says infra, in-band error says worker failure: the
        // in-band message wins → genuine, no auto-retry.
        assert!(matches!(
            decide(
                &c,
                Some(&po(
                    T,
                    BuildStatus::PermanentFailure,
                    "builder failed with exit code 2"
                )),
                &batch,
                &poisoned,
                0,
                &knobs,
                None
            ),
            CollectDecision::Terminal {
                rio: RioOutcome::TargetFailed {
                    kind: FailureKind::Genuine
                },
                ..
            }
        ));
        // Empty in-band message: the relayed line is the fallback → infra
        // auto-retry.
        assert_eq!(
            decide(
                &c,
                Some(&po(T, BuildStatus::TransientFailure, "")),
                &batch,
                &poisoned,
                0,
                &knobs,
                None
            ),
            CollectDecision::Requeue {
                why: "infra-auto-retry"
            }
        );
    }

    #[test]
    fn dependency_failed_reattribution_via_closure_membership() {
        let knobs = Knobs::default();
        let c = ctx("app.x86_64-linux", T, &[DEP], ExpectedOutcome::Built);
        let no_poison: HashMap<String, Vec<String>> = HashMap::new();

        // Trigger in the closure (named by the in-band error message), with
        // worker evidence on the trigger → terminal dependency failure with
        // a genuine root.
        let batch = BatchView {
            reasons: BTreeMap::from([(
                DEP.to_string(),
                "poison threshold reached after 3 distinct-worker failures".to_string(),
            )]),
            ..BatchView::default()
        };
        let target = po(
            T,
            BuildStatus::DependencyFailed,
            &format!(
                "dependency '{DEP}' failed: poison threshold reached after 3 distinct-worker failures"
            ),
        );
        let poisoned_genuine: HashMap<String, Vec<String>> =
            HashMap::from([(DEP.to_string(), vec!["b1".to_string()])]);
        match decide(
            &c,
            Some(&target),
            &batch,
            &poisoned_genuine,
            0,
            &knobs,
            None,
        ) {
            CollectDecision::Terminal {
                rio: RioOutcome::DependencyFailed { root, failing_drv },
                ..
            } => {
                assert_eq!(failing_drv, DEP);
                assert_eq!(root, RootCauseKind::Genuine);
            }
            other => panic!("expected a terminal dependency failure, got {other:?}"),
        }

        // Infra-poisoned shared dependency (the trigger's relayed reason
        // says infra, its poison entry has no failed builders) → cascaded
        // infra root.
        let batch_infra = BatchView {
            reasons: BTreeMap::from([(
                DEP.to_string(),
                "max_infra_retries=3 exhausted after infrastructure failures: x".to_string(),
            )]),
            ..BatchView::default()
        };
        let target_infra = po(
            T,
            BuildStatus::DependencyFailed,
            &format!(
                "dependency '{DEP}' failed: max_infra_retries=3 exhausted after infrastructure failures: x"
            ),
        );
        let poisoned_infra: HashMap<String, Vec<String>> =
            HashMap::from([(DEP.to_string(), vec![])]);
        assert!(matches!(
            decide(
                &c,
                Some(&target_infra),
                &batch_infra,
                &poisoned_infra,
                0,
                &knobs,
                None
            ),
            CollectDecision::Terminal {
                rio: RioOutcome::DependencyFailed {
                    root: RootCauseKind::Infra,
                    ..
                },
                ..
            }
        ));

        // The relayed line is the fallback trigger source when the in-band
        // message is empty.
        let batch_relayed = BatchView {
            reasons: BTreeMap::from([(
                T.to_string(),
                format!("dependency '{DEP}' failed: poison threshold reached"),
            )]),
            ..BatchView::default()
        };
        assert!(matches!(
            decide(
                &c,
                Some(&po(T, BuildStatus::DependencyFailed, "")),
                &batch_relayed,
                &no_poison,
                0,
                &knobs,
                None
            ),
            CollectDecision::Terminal {
                rio: RioOutcome::DependencyFailed { .. },
                ..
            }
        ));

        // Trigger NOT in the closure (fail-fast batch-mate) → requeue.
        let mate = po(
            T,
            BuildStatus::DependencyFailed,
            &format!("dependency '{OTHER}' failed: poison threshold reached"),
        );
        assert_eq!(
            decide(
                &c,
                Some(&mate),
                &BatchView::default(),
                &no_poison,
                0,
                &knobs,
                None
            ),
            CollectDecision::Requeue {
                why: "failfast-batch-mate"
            }
        );

        // No identifiable trigger (no dependency-shaped message from either
        // Signal-1 source) → re-queued instead of being charged a
        // dependency failure.
        assert_eq!(
            decide(
                &c,
                Some(&po(T, BuildStatus::DependencyFailed, "")),
                &BatchView::default(),
                &no_poison,
                0,
                &knobs,
                None
            ),
            CollectDecision::Requeue {
                why: "dependency-failed-no-trigger"
            }
        );
    }

    /// One settled multi-root batch whose in-band results mix a success, an
    /// infra failure, and a missing entry: each root is classified on its own
    /// result — the completed sibling is never dragged down by the failing
    /// one, the infra root spends the auto-retry budget before going
    /// terminal, and the missing root follows the requeue-then-infra rule.
    #[test]
    fn mixed_multi_root_batch_classifies_each_root_independently() {
        let knobs = Knobs::default();
        let job1 = ctx("ok.x86_64-linux", T, &[], ExpectedOutcome::Built);
        let job2 = ctx("infra.x86_64-linux", DEP, &[], ExpectedOutcome::Built);
        let job3 = ctx("missing.x86_64-linux", OTHER, &[], ExpectedOutcome::Built);
        let batch = BatchView {
            build_id: Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".into()),
            results: vec![
                po(T, BuildStatus::Built, ""),
                po(
                    DEP,
                    BuildStatus::PermanentFailure,
                    "max_infra_retries=3 exhausted after infrastructure failures",
                ),
                // job3's drv (OTHER) deliberately has no entry.
            ],
            ..BatchView::default()
        };
        // Signal 2 corroborates the infra reason: job2's drv is poisoned with
        // no failed builders recorded.
        let poisoned: HashMap<String, Vec<String>> = HashMap::from([(DEP.to_string(), vec![])]);
        let target_for = |drv: &str| batch.results.iter().find(|r| r.drv_path == drv);

        // First pass (no prior requeues): success terminal, infra auto-retry,
        // missing result requeued.
        assert_eq!(
            decide(&job1, target_for(T), &batch, &poisoned, 0, &knobs, None),
            CollectDecision::Terminal {
                rio: RioOutcome::Built { executed: true },
                evidence: None
            }
        );
        assert_eq!(
            decide(&job2, target_for(DEP), &batch, &poisoned, 0, &knobs, None),
            CollectDecision::Requeue {
                why: "infra-auto-retry"
            }
        );
        assert_eq!(
            decide(&job3, target_for(OTHER), &batch, &poisoned, 0, &knobs, None),
            CollectDecision::Requeue {
                why: "no-inband-result"
            }
        );

        // Second pass (one prior requeue each): both budget-consuming rows go
        // terminal infra; the missing root carries the missing-result
        // evidence.
        assert!(matches!(
            decide(&job2, target_for(DEP), &batch, &poisoned, 1, &knobs, None),
            CollectDecision::Terminal {
                rio: RioOutcome::TargetFailed {
                    kind: FailureKind::Infra
                },
                ..
            }
        ));
        match decide(&job3, target_for(OTHER), &batch, &poisoned, 1, &knobs, None) {
            CollectDecision::Terminal {
                rio:
                    RioOutcome::TargetFailed {
                        kind: FailureKind::Infra,
                    },
                evidence,
            } => assert_eq!(evidence.as_deref(), Some("no-inband-result")),
            other => panic!("expected terminal infra for the missing root, got {other:?}"),
        }
    }

    /// Flag derivation for interruption-armed roots of timed batches: armed +
    /// engine-cancelled carries Replayed, armed + in-band success carries
    /// NotReproduced, armed + failure without a cancellation carries no flag,
    /// and the flag is never derived outside timed batches. The derived flag
    /// drives the verdict through the AuxFlags built in [`build_record`].
    #[test]
    fn timed_interruption_flag_derivation() {
        let job_ctx = ctx("app.x86_64-linux", T, &[], ExpectedOutcome::Built);
        let no_poison: HashMap<String, Vec<String>> = HashMap::new();
        let no_paths: HashMap<String, Option<(crate::narhash::NarHash, u64)>> = HashMap::new();
        let record = |batch: &BatchView, rio: &RioOutcome, target: Option<&PathOutcome>| {
            build_record(
                &job_ctx, rio, None, target, batch, &no_poison, &no_paths, "leaf", "c1", 1, None,
                None, None,
            )
        };

        // Armed + engine-cancelled (the channel was abandoned at the recorded
        // offset): Replayed, and the record classifies interruption-replayed.
        let cancelled = BatchView {
            kind: BATCH_KIND_TIMED.to_string(),
            interruption_drvs: vec![T.to_string()],
            engine_cancelled: true,
            ..BatchView::default()
        };
        assert_eq!(
            timed_interruption_for(&cancelled, T, None),
            Some(TimedInterruption::Replayed)
        );
        let rec = record(&cancelled, &RioOutcome::NotAttempted, None);
        assert_eq!(
            rec.verdict.as_deref(),
            Some(Verdict::InterruptionReplayed.as_str())
        );
        assert_eq!(rec.disposition, None);

        // Armed + in-band success (the build out-raced the recorded
        // interruption): NotReproduced.
        let built = BatchView {
            kind: BATCH_KIND_TIMED.to_string(),
            interruption_drvs: vec![T.to_string()],
            results: vec![po(T, BuildStatus::Built, "")],
            ..BatchView::default()
        };
        assert_eq!(
            timed_interruption_for(&built, T, Some(true)),
            Some(TimedInterruption::NotReproduced)
        );
        let rec = record(
            &built,
            &RioOutcome::Built { executed: true },
            Some(&built.results[0]),
        );
        assert_eq!(
            rec.verdict.as_deref(),
            Some(Verdict::InterruptionNotReproduced.as_str())
        );

        // Armed + failure without a cancellation: no flag — the failure
        // classifies on its own evidence.
        let failed = BatchView {
            kind: BATCH_KIND_TIMED.to_string(),
            interruption_drvs: vec![T.to_string()],
            results: vec![po(
                T,
                BuildStatus::PermanentFailure,
                "builder failed with exit code 2",
            )],
            ..BatchView::default()
        };
        assert_eq!(timed_interruption_for(&failed, T, Some(false)), None);
        let rec = record(
            &failed,
            &RioOutcome::TargetFailed {
                kind: FailureKind::Genuine,
            },
            Some(&failed.results[0]),
        );
        assert_eq!(
            rec.verdict.as_deref(),
            Some(Verdict::UnexpectedFailure.as_str())
        );
        assert_eq!(rec.failure_cause.as_deref(), Some("genuine"));

        // An unarmed root of the same cancelled batch carries no flag, and a
        // non-timed batch never carries one.
        assert_eq!(timed_interruption_for(&cancelled, DEP, None), None);
        let submit_batch = BatchView {
            kind: BATCH_KIND_SUBMIT.to_string(),
            interruption_drvs: vec![T.to_string()],
            engine_cancelled: true,
            ..BatchView::default()
        };
        assert_eq!(timed_interruption_for(&submit_batch, T, None), None);
    }

    /// A settled timed batch never re-offers its members to the timeless
    /// pending pool: the armed root the engine cancelled gets its
    /// interruption-replayed record, while the unarmed member with no
    /// in-band result is neither re-queued nor recorded (the timed
    /// dispatcher owns its own retries; the end-of-run backfill covers the
    /// rest).
    #[tokio::test]
    async fn timed_batch_members_are_never_reoffered() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let contexts: HashMap<String, JobContext> = [
            (
                "armed.x86_64-linux".to_string(),
                ctx("armed.x86_64-linux", T, &[], ExpectedOutcome::Built),
            ),
            (
                "mate.x86_64-linux".to_string(),
                ctx("mate.x86_64-linux", DEP, &[], ExpectedOutcome::Built),
            ),
        ]
        .into();
        // The channel was abandoned at the interruption deadline: no in-band
        // results, no observed build id, engine_cancelled set.
        let batch = BatchView {
            kind: BATCH_KIND_TIMED.to_string(),
            engine_cancelled: true,
            interruption_drvs: vec![T.to_string()],
            ..BatchView::default()
        };
        let requeue = process_settled_batch(
            &state,
            &LogAdmin::default(),
            &FakeStoreApi::default(),
            None,
            &contexts,
            &["armed.x86_64-linux".into(), "mate.x86_64-linux".into()],
            &batch,
            &HashMap::new(),
            &Knobs::default(),
            "leaf",
            "c1",
            &HashMap::new(),
        )
        .await
        .unwrap();
        assert!(requeue.is_empty(), "{requeue:?}");
        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].job, "armed.x86_64-linux");
        assert_eq!(
            records[0].verdict.as_deref(),
            Some(Verdict::InterruptionReplayed.as_str())
        );
        assert_eq!(records[0].rio.outcome, "not-attempted");

        // An engine-side submission failure (no results, no build id, not
        // cancelled) on a timed batch is not re-offered either.
        let failed_submission = BatchView {
            kind: BATCH_KIND_TIMED.to_string(),
            ..BatchView::default()
        };
        let requeue = process_settled_batch(
            &state,
            &LogAdmin::default(),
            &FakeStoreApi::default(),
            None,
            &contexts,
            &["mate.x86_64-linux".into()],
            &failed_submission,
            &HashMap::new(),
            &Knobs::default(),
            "leaf",
            "c1",
            &HashMap::new(),
        )
        .await
        .unwrap();
        assert!(requeue.is_empty(), "{requeue:?}");
    }

    /// An engine-cancelled batch (channel abandoned at the batch deadline)
    /// settles with no in-band results: every member is re-offered via the
    /// engine-cancelled rule regardless of how much retry budget it has
    /// already spent.
    #[test]
    fn engine_cancelled_batch_requeues_members_without_results() {
        let knobs = Knobs::default();
        let batch = BatchView {
            build_id: Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".into()),
            results: vec![],
            engine_cancelled: true,
            ..BatchView::default()
        };
        let no_poison: HashMap<String, Vec<String>> = HashMap::new();
        for (job, prior_requeues) in [
            (ctx("a.x86_64-linux", T, &[], ExpectedOutcome::Built), 0),
            (ctx("b.x86_64-linux", DEP, &[], ExpectedOutcome::Built), 5),
        ] {
            assert_eq!(
                decide(&job, None, &batch, &no_poison, prior_requeues, &knobs, None),
                CollectDecision::Requeue {
                    why: "engine-cancelled"
                },
                "prior_requeues = {prior_requeues}"
            );
        }
    }

    /// An engine-side submission failure (no in-band results, no build id)
    /// consumes the same bounded auto-retry budget as a missing in-band
    /// result: a member with budget left is re-offered, a member whose
    /// budget is spent gets a terminal infra-indeterminate record carrying
    /// the recorded submission error as evidence — so a deterministic
    /// transport failure (unreachable gateway, host-key mismatch) drains
    /// the campaign instead of re-offering forever. No scheduler evidence
    /// RPCs fire: the failure happened before any build existed.
    #[tokio::test]
    async fn engine_submission_failure_consumes_budget_then_terminalizes() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let admin = LogAdmin::default();
        let contexts: HashMap<String, JobContext> = [
            (
                "fresh.x86_64-linux".to_string(),
                ctx("fresh.x86_64-linux", T, &[], ExpectedOutcome::Built),
            ),
            (
                "spent.x86_64-linux".to_string(),
                ctx("spent.x86_64-linux", DEP, &[], ExpectedOutcome::Built),
            ),
        ]
        .into();
        let batch = BatchView {
            kind: BATCH_KIND_SUBMIT.to_string(),
            stderr_tail: Some("engine submission error: ssh handshake: host key mismatch".into()),
            ..BatchView::default()
        };
        // Knobs::default() grants one auto-retry: "spent" already burned it.
        let prior: HashMap<String, u32> = [("spent.x86_64-linux".to_string(), 1)].into();
        let requeue = process_settled_batch(
            &state,
            &admin,
            &FakeStoreApi::default(),
            None,
            &contexts,
            &["fresh.x86_64-linux".into(), "spent.x86_64-linux".into()],
            &batch,
            &prior,
            &Knobs::default(),
            "leaf",
            "c1",
            &HashMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(requeue, vec!["fresh.x86_64-linux".to_string()]);
        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(records.len(), 1, "{records:?}");
        let rec = &records[0];
        assert_eq!(rec.job, "spent.x86_64-linux");
        assert_eq!(
            rec.verdict.as_deref(),
            Some(Verdict::InfraIndeterminate.as_str())
        );
        assert_eq!(rec.failure_cause.as_deref(), Some("infra"));
        assert_eq!(rec.rio.outcome, "target-failed");
        assert_eq!(
            rec.evidence.as_deref(),
            Some("engine submission error: ssh handshake: host key mismatch")
        );
        assert_eq!(rec.attempts, 2);
        assert!(rec.build_ids.is_empty());
        // The failure pre-dates any build: nothing to fetch poison evidence
        // or a log tail for.
        assert_eq!(admin.poisoned_calls.load(Ordering::SeqCst), 0);
        assert_eq!(admin.log_calls.load(Ordering::SeqCst), 0);
    }

    /// A duplicate engine-side submission failure for a job that already
    /// settled terminally (the submit loop can re-submit inside the
    /// settle-to-collect window) is dropped: it must neither overwrite the
    /// real verdict under latest-record-per-job semantics nor re-offer the
    /// finished job.
    #[tokio::test]
    async fn duplicate_submission_failure_never_clobbers_a_terminal_record() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let contexts: HashMap<String, JobContext> = [(
            "ok.x86_64-linux".to_string(),
            ctx("ok.x86_64-linux", T, &[], ExpectedOutcome::Built),
        )]
        .into();
        // First batch settles the job successfully (match-built record).
        let settled = BatchView {
            kind: BATCH_KIND_SUBMIT.to_string(),
            build_id: Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".to_string()),
            results: vec![po(T, BuildStatus::Built, "")],
            ..BatchView::default()
        };
        // Duplicate submission of the same job then fails engine-side with
        // the job's whole retry budget already spent.
        let duplicate = BatchView {
            kind: BATCH_KIND_SUBMIT.to_string(),
            stderr_tail: Some("engine submission error: channel open failed".into()),
            ..BatchView::default()
        };
        let prior: HashMap<String, u32> = [("ok.x86_64-linux".to_string(), 5)].into();
        for (batch, prior) in [(&settled, &HashMap::new()), (&duplicate, &prior)] {
            let requeue = process_settled_batch(
                &state,
                &LogAdmin::default(),
                &FakeStoreApi::default(),
                None,
                &contexts,
                &["ok.x86_64-linux".into()],
                batch,
                prior,
                &Knobs::default(),
                "leaf",
                "c1",
                &HashMap::new(),
            )
            .await
            .unwrap();
            assert!(requeue.is_empty(), "{requeue:?}");
        }
        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].verdict.as_deref(), Some("match-built"));
    }

    /// The engine-cancelled carve-out survives the submission-failure
    /// budget: a non-timed batch the engine itself cancelled (deadline,
    /// abort) that settled with no results and no build id re-offers every
    /// member regardless of spent budget — cancellation is the engine's own
    /// act, mirroring the engine-cancelled rule in [`decide`].
    #[tokio::test]
    async fn engine_cancelled_submission_failure_requeues_without_budget() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let contexts: HashMap<String, JobContext> = [(
            "spent.x86_64-linux".to_string(),
            ctx("spent.x86_64-linux", T, &[], ExpectedOutcome::Built),
        )]
        .into();
        let batch = BatchView {
            kind: BATCH_KIND_SUBMIT.to_string(),
            engine_cancelled: true,
            ..BatchView::default()
        };
        let prior: HashMap<String, u32> = [("spent.x86_64-linux".to_string(), 5)].into();
        let requeue = process_settled_batch(
            &state,
            &LogAdmin::default(),
            &FakeStoreApi::default(),
            None,
            &contexts,
            &["spent.x86_64-linux".into()],
            &batch,
            &prior,
            &Knobs::default(),
            "leaf",
            "c1",
            &HashMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(requeue, vec!["spent.x86_64-linux".to_string()]);
        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert!(records.is_empty(), "{records:?}");
    }

    /// End-to-end through process_settled_batch with fakes: one success (NAR
    /// hash captured), one genuine failure (log tail uploaded), one batch-mate
    /// requeued — all driven by the batch's in-band per-root results.
    #[tokio::test]
    async fn process_settled_batch_writes_records_and_requeues() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let build_id = "0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a";

        let ok_job = ctx("ok.x86_64-linux", T, &[], ExpectedOutcome::Built);
        let bad_job = ctx("bad.x86_64-linux", DEP, &[], ExpectedOutcome::Built);
        let mate_job = ctx("mate.x86_64-linux", OTHER, &[], ExpectedOutcome::Built);

        let mut store = FakeStoreApi::default();
        store.valid.insert(
            ok_job.outputs["out"].clone(),
            (crate::narhash::NarHash::parse(&"ab".repeat(32)).unwrap(), 7),
        );
        let artifacts_dir = tempfile::tempdir().unwrap();
        let artifacts = LocalDirArtifactStore::new(artifacts_dir.path());
        let admin = LogAdmin {
            poisoned: vec![PoisonedView {
                drv_path: DEP.to_string(),
                failed_executors: vec!["b1".into()],
                poisoned_secs_ago: 60,
            }],
            ..LogAdmin::default()
        };

        let contexts: HashMap<String, JobContext> = [
            ("ok.x86_64-linux".to_string(), ok_job),
            ("bad.x86_64-linux".to_string(), bad_job),
            ("mate.x86_64-linux".to_string(), mate_job),
        ]
        .into();
        let batch = BatchView {
            kind: BATCH_KIND_SUBMIT.to_string(),
            build_id: Some(build_id.to_string()),
            results: vec![
                po(T, BuildStatus::Built, ""),
                po(
                    DEP,
                    BuildStatus::PermanentFailure,
                    "failed on every eligible worker",
                ),
                po(
                    OTHER,
                    BuildStatus::DependencyFailed,
                    &format!("dependency '{T}' failed: failed on every eligible worker"),
                ),
            ],
            reasons: BTreeMap::from([
                (
                    DEP.to_string(),
                    "failed on every eligible worker".to_string(),
                ),
                (
                    OTHER.to_string(),
                    format!("dependency '{T}' failed: failed on every eligible worker"),
                ),
            ]),
            stderr_tail: None,
            engine_cancelled: false,
            interruption_drvs: Vec::new(),
            submitted_at: Some("2026-05-26T01:00:00Z".into()),
        };
        let requeue = process_settled_batch(
            &state,
            &admin,
            &store,
            Some((&artifacts, "replay/campaigns/c1".to_string())),
            &contexts,
            &[
                "ok.x86_64-linux".into(),
                "bad.x86_64-linux".into(),
                "mate.x86_64-linux".into(),
            ],
            &batch,
            &HashMap::new(),
            &Knobs::default(),
            "leaf",
            "c1",
            &HashMap::new(),
        )
        .await
        .unwrap();

        // mate's trigger (T) is not in its dep closure → requeued.
        assert_eq!(requeue, vec!["mate.x86_64-linux".to_string()]);
        // The poison snapshot is fetched exactly once for the whole batch.
        assert_eq!(admin.poisoned_calls.load(Ordering::SeqCst), 1);
        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(records.len(), 2);
        let ok = records.iter().find(|r| r.job == "ok.x86_64-linux").unwrap();
        assert_eq!(ok.verdict.as_deref(), Some("match-built"));
        assert_eq!(ok.disposition, None);
        assert!(!ok.flaky, "first-attempt success is not flaky");
        assert_eq!(ok.rio.status.as_deref(), Some("Built"));
        assert_eq!(
            ok.rio.exec_id, None,
            "exec id is null under in-band collection"
        );
        assert_eq!(
            ok.rio.outputs["out"].nar_hash,
            Some(crate::narhash::NarHash::parse(&"ab".repeat(32)).unwrap())
        );
        assert_eq!(ok.repro, format!("cargo xtask replay repro c1 {T}"));
        let bad = records
            .iter()
            .find(|r| r.job == "bad.x86_64-linux")
            .unwrap();
        assert_eq!(bad.verdict.as_deref(), Some("unexpected-failure"));
        assert_eq!(bad.failure_cause.as_deref(), Some("genuine"));
        assert_eq!(bad.signature.as_deref(), Some("failed-every-worker"));
        assert_eq!(bad.rio.failed_builders, vec!["b1".to_string()]);
        assert!(
            bad.log_key
                .as_deref()
                .unwrap()
                .ends_with("bad.x86_64-linux.log.zst")
        );
        assert!(
            artifacts_dir
                .path()
                .join("replay/campaigns/c1/logs/bad.x86_64-linux.log.zst")
                .exists()
        );
        // Neither in-band results nor a build id → engine-side submission
        // failure → the still-unsettled member is re-offered (its retry
        // budget is untouched).
        let r = process_settled_batch(
            &state,
            &LogAdmin::default(),
            &store,
            None,
            &contexts,
            &["mate.x86_64-linux".into()],
            &BatchView::default(),
            &HashMap::new(),
            &Knobs::default(),
            "leaf",
            "c1",
            &HashMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(r, vec!["mate.x86_64-linux".to_string()]);
    }

    /// A batch whose in-band results are all successes never fetches the
    /// ListPoisoned snapshot — there is no failure to attribute.
    #[tokio::test]
    async fn all_success_batch_skips_the_poison_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let admin = LogAdmin::default();
        let contexts: HashMap<String, JobContext> = [(
            "ok.x86_64-linux".to_string(),
            ctx("ok.x86_64-linux", T, &[], ExpectedOutcome::Built),
        )]
        .into();
        let batch = BatchView {
            build_id: Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".to_string()),
            results: vec![po(T, BuildStatus::Substituted, "")],
            ..BatchView::default()
        };
        let requeue = process_settled_batch(
            &state,
            &admin,
            &FakeStoreApi::default(),
            None,
            &contexts,
            &["ok.x86_64-linux".into()],
            &batch,
            &HashMap::new(),
            &Knobs::default(),
            "leaf",
            "c1",
            &HashMap::new(),
        )
        .await
        .unwrap();
        assert!(requeue.is_empty());
        assert_eq!(admin.poisoned_calls.load(Ordering::SeqCst), 0);
        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].disposition.as_deref(),
            Some(Disposition::TargetSubstituted.as_str())
        );
        assert_eq!(records[0].verdict, None);
        assert_eq!(records[0].rio.status.as_deref(), Some("Substituted"));
    }

    /// A ListPoisoned RPC failure degrades to "no poison evidence" (warning
    /// only): the batch still classifies and the pass never errors out.
    #[tokio::test]
    async fn list_poisoned_failure_degrades_to_no_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let admin = LogAdmin {
            fail_poisoned: true,
            ..LogAdmin::default()
        };
        let contexts: HashMap<String, JobContext> = [(
            "bad.x86_64-linux".to_string(),
            ctx("bad.x86_64-linux", T, &[], ExpectedOutcome::Built),
        )]
        .into();
        let batch = BatchView {
            build_id: Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".to_string()),
            results: vec![po(
                T,
                BuildStatus::TransientFailure,
                "max_infra_retries=3 exhausted after infrastructure failures: x",
            )],
            ..BatchView::default()
        };
        // Budget already consumed → terminal; with Signal 2 unavailable the
        // infra reason still resolves to infra.
        let prior: HashMap<String, u32> = [("bad.x86_64-linux".to_string(), 1)].into();
        let requeue = process_settled_batch(
            &state,
            &admin,
            &FakeStoreApi::default(),
            None,
            &contexts,
            &["bad.x86_64-linux".into()],
            &batch,
            &prior,
            &Knobs::default(),
            "leaf",
            "c1",
            &HashMap::new(),
        )
        .await
        .unwrap();
        assert!(requeue.is_empty());
        assert_eq!(admin.poisoned_calls.load(Ordering::SeqCst), 1);
        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].verdict.as_deref(), Some("infra-indeterminate"));
        assert_eq!(records[0].failure_cause.as_deref(), Some("infra"));
        assert!(records[0].rio.failed_builders.is_empty());
    }

    /// A store query failure on a success must not lose the terminal record:
    /// the job is still recorded as built, just without NAR identity, and
    /// the comparison stays not-comparable instead of a false differs.
    #[tokio::test]
    async fn store_failure_records_success_without_nar_identity() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let store = FakeStoreApi::default();
        store.fail_with("BatchQueryPathInfo: store unavailable");
        let mut ok_job = ctx("ok.x86_64-linux", T, &[], ExpectedOutcome::Built);
        ok_job.expected_outputs.insert(
            "out".to_string(),
            super::super::model::ExpectedOutput {
                narinfo_present: true,
                nar_hash: Some(
                    crate::narhash::NarHash::parse(&format!("sha256:{}", "0".repeat(52))).unwrap(),
                ),
                nar_size: Some(1),
            },
        );
        let contexts: HashMap<String, JobContext> =
            [("ok.x86_64-linux".to_string(), ok_job)].into();
        let batch = BatchView {
            build_id: Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".to_string()),
            results: vec![po(T, BuildStatus::Built, "")],
            ..BatchView::default()
        };
        let requeue = process_settled_batch(
            &state,
            &LogAdmin::default(),
            &store,
            None,
            &contexts,
            &["ok.x86_64-linux".into()],
            &batch,
            &HashMap::new(),
            &Knobs::default(),
            "leaf",
            "c1",
            &HashMap::new(),
        )
        .await
        .unwrap();
        assert!(requeue.is_empty());
        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].verdict.as_deref(), Some("match-built"));
        assert_eq!(records[0].rio.outputs["out"].nar_hash, None);
        assert_eq!(records[0].nar_compare["out"], "not-comparable");
    }

    /// A failed root with no in-band error message, no relayed reason, and
    /// no poison entry (both signals lost) pulls the log tail as the third
    /// signal. The same fetch is reused for evidence capture (one log_tail
    /// call, not two), the record carries the log-tail-only flag, and the
    /// signature is derived from the tail so these failures still group.
    #[tokio::test]
    async fn log_tail_only_failure_reuses_one_fetch_and_groups_by_tail() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let admin = LogAdmin::default();
        let contexts: HashMap<String, JobContext> = [(
            "bad.x86_64-linux".to_string(),
            ctx("bad.x86_64-linux", T, &[], ExpectedOutcome::Built),
        )]
        .into();
        let batch = BatchView {
            build_id: Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".to_string()),
            results: vec![po(T, BuildStatus::MiscFailure, "")],
            ..BatchView::default()
        };
        let requeue = process_settled_batch(
            &state,
            &admin,
            &FakeStoreApi::default(),
            None,
            &contexts,
            &["bad.x86_64-linux".into()],
            &batch,
            &HashMap::new(),
            &Knobs::default(),
            "leaf",
            "c1",
            &HashMap::new(),
        )
        .await
        .unwrap();
        assert!(requeue.is_empty());
        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(records.len(), 1);
        let rec = &records[0];
        assert_eq!(rec.verdict.as_deref(), Some("unexpected-failure"));
        assert_eq!(rec.evidence.as_deref(), Some("log-tail-only"));
        // No reason from either Signal-1 source: the signature falls back to
        // the captured tail.
        assert_eq!(rec.signature.as_deref(), Some("log:gcc--fatal-error"));
        assert_eq!(
            rec.log_key.as_deref(),
            Some("logs/bad.x86_64-linux.log.zst")
        );
        assert!(state.path("logs/bad.x86_64-linux.log.zst").exists());
        // The Signal-3 fetch doubled as the evidence capture: one call only.
        assert_eq!(admin.log_calls.load(Ordering::SeqCst), 1);
    }

    /// An EMPTY Signal-3 log tail is no evidence: the record must not carry
    /// a meaningless `log:` signature or a log key (nothing was captured),
    /// while the log-tail-only evidence flag still marks the degraded
    /// evidence quality.
    #[tokio::test]
    async fn empty_log_tail_is_not_used_as_a_signature() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        // Both signals lost, exactly like the log-tail-only case above — but
        // the scheduler has no log bytes left either.
        let admin = LogAdmin {
            tail: Vec::new(),
            ..LogAdmin::default()
        };
        let contexts: HashMap<String, JobContext> = [(
            "bad.x86_64-linux".to_string(),
            ctx("bad.x86_64-linux", T, &[], ExpectedOutcome::Built),
        )]
        .into();
        let batch = BatchView {
            build_id: Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".to_string()),
            results: vec![po(T, BuildStatus::MiscFailure, "")],
            ..BatchView::default()
        };
        process_settled_batch(
            &state,
            &admin,
            &FakeStoreApi::default(),
            None,
            &contexts,
            &["bad.x86_64-linux".into()],
            &batch,
            &HashMap::new(),
            &Knobs::default(),
            "leaf",
            "c1",
            &HashMap::new(),
        )
        .await
        .unwrap();
        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(records.len(), 1);
        let rec = &records[0];
        assert_eq!(rec.verdict.as_deref(), Some("unexpected-failure"));
        assert_eq!(rec.evidence.as_deref(), Some("log-tail-only"));
        assert_eq!(rec.signature, None, "empty tail must not become `log:`");
        assert_eq!(rec.log_key, None);
        assert!(!state.path("logs/bad.x86_64-linux.log.zst").exists());
    }

    /// A failed at-capture log upload still records the deterministic S3 key
    /// (the periodic state sync re-enumerates logs/*.log.zst and retries the
    /// upload from the local copy, which is kept).
    #[tokio::test]
    async fn failed_log_upload_still_records_the_log_key() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let contexts: HashMap<String, JobContext> = [(
            "bad.x86_64-linux".to_string(),
            ctx("bad.x86_64-linux", T, &[], ExpectedOutcome::Built),
        )]
        .into();
        let batch = BatchView {
            build_id: Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".to_string()),
            results: vec![po(
                T,
                BuildStatus::PermanentFailure,
                "failed on every eligible worker",
            )],
            reasons: BTreeMap::from([(
                T.to_string(),
                "failed on every eligible worker".to_string(),
            )]),
            ..BatchView::default()
        };
        process_settled_batch(
            &state,
            &LogAdmin::default(),
            &FakeStoreApi::default(),
            Some((&FailingArtifacts, "replay/campaigns/c1".to_string())),
            &contexts,
            &["bad.x86_64-linux".into()],
            &batch,
            &HashMap::new(),
            &Knobs::default(),
            "leaf",
            "c1",
            &HashMap::new(),
        )
        .await
        .unwrap();
        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].log_key.as_deref(),
            Some("replay/campaigns/c1/logs/bad.x86_64-linux.log.zst")
        );
        assert!(
            state.path("logs/bad.x86_64-linux.log.zst").exists(),
            "local copy stays for the sync retry"
        );
    }
}
