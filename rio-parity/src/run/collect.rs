//! Collect stage: turn per-(build, drv) observations, captured stderr
//! reasons, and scheduler poison evidence into terminal per-job records.
//!
//! For every settled batch the stage reads the per-drv observations of the
//! batch's build, decides one [`Verdict`] per member job (terminal,
//! re-queue, or still running), captures evidence — NAR hashes via the
//! store for successes, a compressed log tail for failures — and appends
//! terminal [`JobRecord`]s to results.jsonl.
//!
//! Failure attribution combines two independent signals: the relayed
//! stderr reason captured at submission time (Signal 1) and the
//! scheduler's failed-builder poison evidence (Signal 2). Only their
//! agreement counts a failure as infrastructure; ambiguous, contradictory,
//! or decayed evidence defaults to a genuine target failure so rio is
//! never given the benefit of the doubt. A `dependency_failed` job is
//! re-attributed through its own dependency closure: a failing drv inside
//! the closure is a real blocked dependency, while a failing drv outside
//! it means the job was merely a fail-fast batch-mate and is re-queued.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Result;

use super::artifact::ArtifactStore;
use super::classify::{AuxFlags, OutputHashes, classify, compare_output};
use super::grpc::{AdminApi, StoreApi};
use super::model::{
    Bucket, FailureKind, HydraOutcome, HydraSide, JobRecord, RioOutcome, RioSide, RootCauseKind,
    STATUS_CANCELLED, STATUS_COMPLETED, STATUS_DEPENDENCY_FAILED, STATUS_POISONED, STATUS_SKIPPED,
    now_rfc3339,
};
use super::reader::{DrvObservation, ResultReader};
use super::spec::Knobs;
use super::state::{StateDir, StateFile};
use super::stderrparse::{ReasonClass, classify_reason, signature_for};
use super::submitter::repro_command;

/// Static per-job context assembled from the eval set + plan output.
#[derive(Debug, Clone)]
pub struct JobContext {
    pub job: String,
    pub system: String,
    pub drv_path: String,
    /// Output name → store path (from the eval-set manifest).
    pub outputs: BTreeMap<String, String>,
    /// Dependency drv closure (from dep-closure.jsonl) — used for the
    /// fail-fast re-attribution rule.
    pub dep_drvs: HashSet<String>,
    pub hydra_outcome: HydraOutcome,
    pub hydra_outputs: BTreeMap<String, super::model::HydraOutput>,
    pub hydra_buildstatus: Option<i64>,
    pub plan_not_attemptable: bool,
    pub plan_snapshot_valid: bool,
}

/// What collect decided for one job after looking at one settled batch.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Terminal outcome — write the results.jsonl record.
    Terminal {
        rio: RioOutcome,
        evidence: Option<String>,
    },
    /// Non-terminal: re-offer to the submit loop (fail-fast batch-mate,
    /// engine-cancelled batch, infra auto-retry, no-rows first attempt).
    Requeue { why: &'static str },
    /// Still running — only update activity timestamps.
    StillRunning,
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
            // scheduler's poison-evidence TTL, mirrored by
            // `knobs.evidence_ttl_hours`): only the log tail is left, so the
            // record carries the "log-tail-only" evidence-quality flag.
            None => (FailureKind::Genuine, Some("log-tail-only".to_string())),
        },
    }
}

/// Inputs about the batch the job rode in (from its [`super::model::BatchRecord`]).
#[derive(Debug, Clone, Default)]
pub struct BatchView {
    pub build_id: Option<String>,
    /// Child exit code recorded for the batch (None = killed by a signal or
    /// never ran). Together with `build_id` this distinguishes an
    /// engine-side submission failure from a lost `rio: build` line.
    pub exit_code: Option<i32>,
    /// drv → relayed reason (Signal 1).
    pub reasons: BTreeMap<String, String>,
    pub engine_cancelled: bool,
    pub submitted_at: Option<String>,
}

/// Decide the verdict for one job given its target-drv observation and the
/// observations of every drv in the batch (for root-cause lookup).
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
    target: &DrvObservation,
    all_observations: &HashMap<String, DrvObservation>,
    batch: &BatchView,
    prior_requeues: u32,
    knobs: &Knobs,
    log_tail: Option<&str>,
) -> Verdict {
    let reason = batch.reasons.get(&ctx.drv_path).map(String::as_str);
    match target.status.as_str() {
        // A build-scoped exec_id decides built-vs-substituted; the
        // completed-without-execution discriminator (cached-prior vs
        // target-substituted) is the classifier's job, driven by the
        // plan-snapshot flag.
        STATUS_COMPLETED | STATUS_SKIPPED => Verdict::Terminal {
            rio: RioOutcome::Built {
                executed: target.exec_id.is_some(),
            },
            evidence: None,
        },
        STATUS_POISONED => {
            let (kind, evidence) = resolve_failure_kind(
                reason,
                target.failed_builders.as_deref(),
                target.is_fixed_output,
                log_tail,
            );
            if kind == FailureKind::Infra && prior_requeues < knobs.max_auto_retries {
                return Verdict::Requeue {
                    why: "infra-auto-retry",
                };
            }
            Verdict::Terminal {
                rio: RioOutcome::TargetFailed { kind },
                evidence,
            }
        }
        STATUS_DEPENDENCY_FAILED => {
            // Find the trigger drv: prefer the relayed reason, fall back to a
            // poisoned node among this job's dependency closure.
            let trigger = reason
                .map(classify_reason)
                .and_then(|c| match c {
                    ReasonClass::Dependency { failing_drv } => Some(failing_drv),
                    _ => None,
                })
                .or_else(|| {
                    // Fallback closure scan when no relayed reason names the
                    // trigger. Tie-break: with several poisoned deps in the
                    // closure, the lexicographically smallest drv path wins —
                    // an arbitrary but deterministic rule, so re-running
                    // collect over the same observations always attributes
                    // the same trigger (set iteration order must never pick
                    // it).
                    ctx.dep_drvs
                        .iter()
                        .filter(|d| {
                            all_observations
                                .get(*d)
                                .is_some_and(|o| o.status == STATUS_POISONED)
                        })
                        .min()
                        .cloned()
                });
            let Some(trigger) = trigger else {
                // No identifiable trigger: treat as a fail-fast batch-mate.
                return Verdict::Requeue {
                    why: "dependency-failed-no-trigger",
                };
            };
            if !ctx.dep_drvs.contains(&trigger) && trigger != ctx.drv_path {
                // Fail-fast marks unrelated batch-mates dependency_failed —
                // the trigger is not in this job's own closure, so the job
                // never got a fair attempt and is re-queued instead of being
                // charged with a dependency failure.
                return Verdict::Requeue {
                    why: "failfast-batch-mate",
                };
            }
            // Root-cause classification of the trigger, so dependents of an
            // infra-poisoned or source-rotted dependency cascade out of the
            // headline instead of being charged as rio failures.
            let trigger_obs = all_observations.get(&trigger);
            let trigger_reason = batch.reasons.get(&trigger).map(String::as_str);
            let (kind, evidence) = resolve_failure_kind(
                trigger_reason,
                trigger_obs.and_then(|o| o.failed_builders.as_deref()),
                trigger_obs.and_then(|o| o.is_fixed_output),
                None,
            );
            let root = match kind {
                FailureKind::Infra => RootCauseKind::Infra,
                FailureKind::SourceRot => RootCauseKind::SourceRot,
                _ => RootCauseKind::Genuine,
            };
            Verdict::Terminal {
                rio: RioOutcome::DependencyFailed {
                    root,
                    failing_drv: trigger,
                },
                evidence,
            }
        }
        STATUS_CANCELLED => {
            if batch.engine_cancelled {
                Verdict::Requeue {
                    why: "engine-cancelled",
                }
            } else {
                // A cancellation the engine did not request is the
                // scheduler/operator cutting the build short — counted as a
                // timeout against rio rather than excused.
                Verdict::Terminal {
                    rio: RioOutcome::TargetFailed {
                        kind: FailureKind::Timeout,
                    },
                    evidence: None,
                }
            }
        }
        // No derivation rows for this drv in the build (empty status from
        // the reader): one auto-retry, then an infra failure — the build
        // never even recorded the drv, which is not the target's fault.
        "" => {
            if prior_requeues < knobs.max_auto_retries {
                Verdict::Requeue {
                    why: "no-derivation-rows",
                }
            } else {
                Verdict::Terminal {
                    rio: RioOutcome::TargetFailed {
                        kind: FailureKind::Infra,
                    },
                    evidence: Some("no-derivation-rows".to_string()),
                }
            }
        }
        // created/queued/ready/assigned/running/substituting/failed (the
        // scheduler retries `failed` internally) — not settled yet.
        _ => Verdict::StillRunning,
    }
}

/// Assemble the final [`JobRecord`] for a terminal verdict.
///
/// `log_tail` is the captured failure-log text (when any was fetched); it
/// only feeds the failure-signature fallback, so failures whose relayed
/// reason was lost can still be grouped by their log evidence.
#[allow(clippy::too_many_arguments)]
pub fn build_record(
    ctx: &JobContext,
    rio_outcome: &RioOutcome,
    evidence: Option<String>,
    target: &DrvObservation,
    batch: &BatchView,
    rio_paths: &HashMap<String, Option<(String, u64)>>,
    mode: &str,
    store_url: &str,
    attempts: u32,
    log_key: Option<String>,
    first_active_at: Option<String>,
    log_tail: Option<&str>,
) -> JobRecord {
    let aux = AuxFlags {
        skipped: None,
        eval_error: false,
        plan_not_attemptable: ctx.plan_not_attemptable,
        plan_snapshot_valid: ctx.plan_snapshot_valid,
        resolve_unknown_divergent: None,
    };
    let classification = classify(&ctx.hydra_outcome, rio_outcome, &aux);
    let reason = batch.reasons.get(&ctx.drv_path).cloned();

    let mut rio_outputs = BTreeMap::new();
    let mut nar_compare = BTreeMap::new();
    for (name, path) in &ctx.outputs {
        let rio_info = rio_paths.get(path).cloned().flatten();
        rio_outputs.insert(
            name.clone(),
            super::model::RioOutput {
                path: path.clone(),
                nar_hash: rio_info.as_ref().map(|(h, _)| h.clone()),
                nar_size: rio_info.as_ref().map(|(_, s)| *s),
            },
        );
        if classification.bucket == Bucket::MatchBuilt {
            let hydra_hash = ctx.hydra_outputs.get(name).and_then(|h| h.nar_hash.clone());
            nar_compare.insert(
                name.clone(),
                compare_output(&OutputHashes {
                    rio_hex: rio_info.as_ref().map(|(h, _)| h.clone()),
                    hydra_narhash: hydra_hash,
                })
                .to_string(),
            );
        }
    }

    let hydra_side = HydraSide {
        outcome: ctx.hydra_outcome.as_str().to_string(),
        buildstatus: ctx.hydra_buildstatus,
        outputs: ctx.hydra_outputs.clone(),
    };
    let rio_side = RioSide {
        outcome: rio_outcome.outcome_str().to_string(),
        status: (!target.status.is_empty()).then(|| target.status.clone()),
        exec_id: target.exec_id.clone(),
        failing_drv: match rio_outcome {
            RioOutcome::DependencyFailed { failing_drv, .. } => Some(failing_drv.clone()),
            _ => None,
        },
        reason: reason.clone(),
        failed_builders: target.failed_builders.clone().unwrap_or_default(),
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
        hydra: hydra_side,
        nar_compare,
        bucket: classification.bucket.as_str().to_string(),
        cascaded: classification.cascaded,
        signature: match rio_outcome {
            RioOutcome::Built { .. } => None,
            _ => signature_for(reason.as_deref(), log_tail),
        },
        log_key,
        repro: repro_command(store_url, &ctx.drv_path),
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

/// Process one settled batch end-to-end: read observations, decide each job,
/// capture evidence (log tails for failures, NAR hashes for successes),
/// append terminal records, and return the jobs that must be re-queued.
///
/// `prior_requeues` carries each job's TOTAL engine resubmission count so
/// far (every requeue reason counts, not just infra retries) — see
/// [`decide`] for why any prior requeue consumes the infra auto-retry
/// budget.
#[allow(clippy::too_many_arguments)]
pub async fn process_settled_batch(
    state: &StateDir,
    reader: &dyn ResultReader,
    admin: &dyn AdminApi,
    store: &dyn StoreApi,
    artifacts: Option<(&dyn ArtifactStore, String)>,
    contexts: &HashMap<String, JobContext>,
    batch_jobs: &[String],
    batch: &BatchView,
    prior_requeues: &HashMap<String, u32>,
    knobs: &Knobs,
    mode: &str,
    store_url: &str,
    first_active: &HashMap<String, String>,
) -> Result<Vec<String>> {
    let mut requeue = Vec::new();
    let Some(build_id) = &batch.build_id else {
        // No build id was ever observed for this batch. The structured
        // batch fields tell the cases apart (never the stderr text): an
        // engine-side submission failure (ssh/spawn/import) or a child that
        // was killed by a signal (including the engine's own batch deadline)
        // leaves BOTH build_id and exit_code unset, while a lost
        // `rio: build` line still records the child's exit code. Either way
        // the engine cannot read per-drv observations without a build id,
        // so every member job is re-offered to the submit loop.
        if batch.exit_code.is_none() {
            tracing::info!(
                jobs = batch_jobs.len(),
                engine_cancelled = batch.engine_cancelled,
                "batch has no build id and no exit code (engine-side submission failure or a \
                 signal-killed child, e.g. at the engine's batch deadline); re-offering its jobs"
            );
        } else {
            tracing::warn!(
                jobs = batch_jobs.len(),
                exit_code = batch.exit_code,
                "batch exited without an observed `rio: build` line; re-offering its jobs"
            );
        }
        return Ok(batch_jobs.to_vec());
    };
    // Read the union of every member job's target + dep drvs (for root-cause
    // lookup), in one read.
    let mut want: Vec<String> = Vec::new();
    for job in batch_jobs {
        if let Some(ctx) = contexts.get(job) {
            want.push(ctx.drv_path.clone());
            want.extend(ctx.dep_drvs.iter().cloned());
        }
    }
    want.sort();
    want.dedup();
    let observations: HashMap<String, DrvObservation> = reader
        .read_build(build_id, &want)
        .await?
        .into_iter()
        .map(|o| (o.drv_path.clone(), o))
        .collect();

    for job in batch_jobs {
        let Some(ctx) = contexts.get(job) else {
            tracing::warn!(job, "batch member has no job context; skipping");
            continue;
        };
        let target = observations
            .get(&ctx.drv_path)
            .cloned()
            .unwrap_or_else(|| DrvObservation {
                drv_path: ctx.drv_path.clone(),
                ..DrvObservation::default()
            });
        let prior = prior_requeues.get(job).copied().unwrap_or(0);
        // Evidence-age gate: when a poisoned drv has neither a relayed
        // reason (Signal 1) nor failed-builder evidence (Signal 2 — the
        // scheduler's poison rows decay with its evidence TTL, mirrored by
        // `knobs.evidence_ttl_hours`), fetch the log tail as the third
        // signal; the record then carries the "log-tail-only" evidence flag
        // from [`resolve_failure_kind`].
        let needs_log_signal = target.status == STATUS_POISONED
            && !batch.reasons.contains_key(&ctx.drv_path)
            && target.failed_builders.is_none();
        let mut log_signal_bytes = if needs_log_signal {
            admin
                .log_tail(
                    &ctx.drv_path,
                    target.exec_id.as_deref(),
                    knobs.log_tail_bytes,
                )
                .await
                .ok()
        } else {
            None
        };
        let log_signal_text = log_signal_bytes.as_deref().map(lossy_log_text);
        match decide(
            ctx,
            &target,
            &observations,
            batch,
            prior,
            knobs,
            log_signal_text.as_deref(),
        ) {
            Verdict::StillRunning => {}
            Verdict::Requeue { why } => {
                tracing::info!(job, why, "re-queueing");
                requeue.push(job.clone());
            }
            Verdict::Terminal { rio, evidence } => {
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
                            .log_tail(
                                &ctx.drv_path,
                                target.exec_id.as_deref(),
                                knobs.log_tail_bytes,
                            )
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
                    &target,
                    batch,
                    &rio_paths,
                    mode,
                    store_url,
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
    use crate::run::reader::test_support::FakeReader;

    fn ctx(job: &str, drv: &str, deps: &[&str], hydra: HydraOutcome) -> JobContext {
        JobContext {
            job: job.to_string(),
            system: "x86_64-linux".into(),
            drv_path: drv.to_string(),
            outputs: BTreeMap::from([(
                "out".to_string(),
                format!("{}-out", drv.trim_end_matches(".drv")),
            )]),
            dep_drvs: deps.iter().map(|s| s.to_string()).collect(),
            hydra_outcome: hydra,
            hydra_outputs: BTreeMap::new(),
            hydra_buildstatus: None,
            plan_not_attemptable: false,
            plan_snapshot_valid: false,
        }
    }

    fn obs(
        drv: &str,
        status: &str,
        exec: Option<&str>,
        builders: Option<Vec<String>>,
    ) -> DrvObservation {
        DrvObservation {
            drv_path: drv.to_string(),
            status: status.to_string(),
            exec_id: exec.map(String::from),
            assigned_executor: None,
            failed_builders: builders,
            poisoned_secs_ago: None,
            is_fixed_output: None,
        }
    }

    const T: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-app.drv";
    const DEP: &str = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-dep.drv";
    const OTHER: &str = "/nix/store/cccccccccccccccccccccccccccccccc-other.drv";

    /// Scripted AdminApi for evidence capture: the build graph is never read
    /// (the reader is faked), every log tail is the same short text, and the
    /// log_tail calls are counted so the Signal-3 reuse (one fetch serving
    /// both classification and evidence capture) can be asserted.
    #[derive(Default)]
    struct LogAdmin {
        log_calls: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl AdminApi for LogAdmin {
        async fn get_build_graph(&self, _b: &str) -> Result<GraphSnapshot> {
            unreachable!("reader is faked")
        }
        async fn list_poisoned(&self) -> Result<Vec<PoisonedView>> {
            Ok(vec![])
        }
        async fn log_tail(&self, _d: &str, _e: Option<&str>, _m: usize) -> Result<Vec<u8>> {
            self.log_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(b"gcc: fatal error\n".to_vec())
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
    fn decide_covers_status_matrix() {
        let knobs = Knobs::default();
        let c = ctx("app.x86_64-linux", T, &[DEP], HydraOutcome::Built);
        let batch = BatchView::default();
        let none: HashMap<String, DrvObservation> = HashMap::new();

        // Completed with exec → Built{executed}.
        assert_eq!(
            decide(
                &c,
                &obs(T, "completed", Some("e1"), None),
                &none,
                &batch,
                0,
                &knobs,
                None
            ),
            Verdict::Terminal {
                rio: RioOutcome::Built { executed: true },
                evidence: None
            }
        );
        // Completed without exec → Built{executed:false} (classifier
        // discriminates cached-prior vs target-substituted).
        assert_eq!(
            decide(
                &c,
                &obs(T, "completed", None, None),
                &none,
                &batch,
                0,
                &knobs,
                None
            ),
            Verdict::Terminal {
                rio: RioOutcome::Built { executed: false },
                evidence: None
            }
        );
        // Skipped (CA early cutoff) is terminal completed-without-execution.
        assert_eq!(
            decide(
                &c,
                &obs(T, "skipped", None, None),
                &none,
                &batch,
                0,
                &knobs,
                None
            ),
            Verdict::Terminal {
                rio: RioOutcome::Built { executed: false },
                evidence: None
            }
        );
        // Poisoned, infra-positive, first attempt → auto-retry requeue;
        // second → terminal infra.
        let infra_obs = obs(T, "poisoned", None, Some(vec![]));
        assert_eq!(
            decide(&c, &infra_obs, &none, &batch, 0, &knobs, None),
            Verdict::Requeue {
                why: "infra-auto-retry"
            }
        );
        assert!(matches!(
            decide(&c, &infra_obs, &none, &batch, 1, &knobs, None),
            Verdict::Terminal {
                rio: RioOutcome::TargetFailed {
                    kind: FailureKind::Infra
                },
                ..
            }
        ));
        // Poisoned with worker evidence → genuine, terminal immediately.
        assert!(matches!(
            decide(
                &c,
                &obs(T, "poisoned", Some("e2"), Some(vec!["b1".into()])),
                &none,
                &batch,
                0,
                &knobs,
                None
            ),
            Verdict::Terminal {
                rio: RioOutcome::TargetFailed {
                    kind: FailureKind::Genuine
                },
                ..
            }
        ));
        // Cancelled: engine-cancelled → requeue; otherwise timeout.
        let cancelled = obs(T, "cancelled", None, None);
        let engine_batch = BatchView {
            engine_cancelled: true,
            ..BatchView::default()
        };
        assert_eq!(
            decide(&c, &cancelled, &none, &engine_batch, 0, &knobs, None),
            Verdict::Requeue {
                why: "engine-cancelled"
            }
        );
        assert!(matches!(
            decide(&c, &cancelled, &none, &batch, 0, &knobs, None),
            Verdict::Terminal {
                rio: RioOutcome::TargetFailed {
                    kind: FailureKind::Timeout
                },
                ..
            }
        ));
        // No rows: requeue once, then infra.
        let missing = obs(T, "", None, None);
        assert_eq!(
            decide(&c, &missing, &none, &batch, 0, &knobs, None),
            Verdict::Requeue {
                why: "no-derivation-rows"
            }
        );
        assert!(matches!(
            decide(&c, &missing, &none, &batch, 1, &knobs, None),
            Verdict::Terminal {
                rio: RioOutcome::TargetFailed {
                    kind: FailureKind::Infra
                },
                ..
            }
        ));
        // Running → StillRunning.
        assert_eq!(
            decide(
                &c,
                &obs(T, "running", None, None),
                &none,
                &batch,
                0,
                &knobs,
                None
            ),
            Verdict::StillRunning
        );
    }

    #[test]
    fn dependency_failed_reattribution_via_closure_membership() {
        let knobs = Knobs::default();
        let c = ctx("app.x86_64-linux", T, &[DEP], HydraOutcome::Built);
        // Trigger in the closure (from the relayed reason) → terminal
        // dependency failure with a genuine root.
        let batch = BatchView {
            reasons: BTreeMap::from([(
                T.to_string(),
                format!(
                    "dependency '{DEP}' failed: poison threshold reached after 3 distinct-worker failures"
                ),
            )]),
            ..BatchView::default()
        };
        let mut all = HashMap::new();
        all.insert(
            DEP.to_string(),
            obs(DEP, "poisoned", Some("e1"), Some(vec!["b1".into()])),
        );
        let v = decide(
            &c,
            &obs(T, "dependency_failed", None, None),
            &all,
            &batch,
            0,
            &knobs,
            None,
        );
        assert!(matches!(
            v,
            Verdict::Terminal {
                rio: RioOutcome::DependencyFailed {
                    root: RootCauseKind::Genuine,
                    ..
                },
                ..
            }
        ));
        // Infra-poisoned shared dependency → cascaded infra root.
        let mut all_infra = HashMap::new();
        all_infra.insert(DEP.to_string(), obs(DEP, "poisoned", None, Some(vec![])));
        let batch_infra = BatchView {
            reasons: BTreeMap::from([
                (
                    T.to_string(),
                    format!(
                        "dependency '{DEP}' failed: max_infra_retries=3 exhausted after infrastructure failures: x"
                    ),
                ),
                (
                    DEP.to_string(),
                    "max_infra_retries=3 exhausted after infrastructure failures: x".to_string(),
                ),
            ]),
            ..BatchView::default()
        };
        let v = decide(
            &c,
            &obs(T, "dependency_failed", None, None),
            &all_infra,
            &batch_infra,
            0,
            &knobs,
            None,
        );
        assert!(matches!(
            v,
            Verdict::Terminal {
                rio: RioOutcome::DependencyFailed {
                    root: RootCauseKind::Infra,
                    ..
                },
                ..
            }
        ));
        // Trigger NOT in the closure (fail-fast batch-mate) → requeue.
        let batch_other = BatchView {
            reasons: BTreeMap::from([(
                T.to_string(),
                format!(
                    "dependency '{OTHER}' failed: poison threshold reached after 3 distinct-worker failures"
                ),
            )]),
            ..BatchView::default()
        };
        let v = decide(
            &c,
            &obs(T, "dependency_failed", None, None),
            &HashMap::new(),
            &batch_other,
            0,
            &knobs,
            None,
        );
        assert_eq!(
            v,
            Verdict::Requeue {
                why: "failfast-batch-mate"
            }
        );
    }

    /// With no relayed reason the trigger comes from the closure scan; with
    /// several poisoned deps the lexicographically smallest drv path is the
    /// documented deterministic tie-break. With no poisoned dep at all the
    /// job is re-queued instead of being charged a dependency failure.
    #[test]
    fn dependency_failed_fallback_scans_closure_deterministically() {
        let knobs = Knobs::default();
        let c = ctx("app.x86_64-linux", T, &[DEP, OTHER], HydraOutcome::Built);
        let batch = BatchView::default();

        // Two poisoned deps in the closure, no relayed reason: the
        // lexicographically smallest drv path (DEP < OTHER) is the trigger.
        let mut all = HashMap::new();
        all.insert(
            DEP.to_string(),
            obs(DEP, "poisoned", None, Some(vec!["b1".into()])),
        );
        all.insert(
            OTHER.to_string(),
            obs(OTHER, "poisoned", None, Some(vec!["b2".into()])),
        );
        let v = decide(
            &c,
            &obs(T, "dependency_failed", None, None),
            &all,
            &batch,
            0,
            &knobs,
            None,
        );
        match v {
            Verdict::Terminal {
                rio: RioOutcome::DependencyFailed { failing_drv, root },
                ..
            } => {
                assert_eq!(failing_drv, DEP, "smallest candidate drv path wins");
                assert_eq!(root, RootCauseKind::Genuine);
            }
            other => panic!("expected a terminal dependency failure, got {other:?}"),
        }

        // No poisoned dep in the closure and no relayed reason → no
        // identifiable trigger → re-queued.
        let v = decide(
            &c,
            &obs(T, "dependency_failed", None, None),
            &HashMap::new(),
            &batch,
            0,
            &knobs,
            None,
        );
        assert_eq!(
            v,
            Verdict::Requeue {
                why: "dependency-failed-no-trigger"
            }
        );
    }

    /// End-to-end through process_settled_batch with fakes: one success (NAR
    /// hash captured), one genuine failure (log tail uploaded), one batch-mate
    /// requeued.
    #[tokio::test]
    async fn process_settled_batch_writes_records_and_requeues() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let reader = FakeReader::default();
        let build_id = "0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a";

        let ok_job = ctx("ok.x86_64-linux", T, &[], HydraOutcome::Built);
        let bad_job = ctx("bad.x86_64-linux", DEP, &[], HydraOutcome::Built);
        let mate_job = ctx("mate.x86_64-linux", OTHER, &[], HydraOutcome::Built);
        reader.set(build_id, obs(T, "completed", Some("e1"), None));
        reader.set(
            build_id,
            obs(DEP, "poisoned", Some("e2"), Some(vec!["b1".into()])),
        );
        reader.set(build_id, obs(OTHER, "dependency_failed", None, None));

        let mut store = FakeStoreApi::default();
        store
            .valid
            .insert(ok_job.outputs["out"].clone(), ("ab".repeat(32), 7));
        let artifacts_dir = tempfile::tempdir().unwrap();
        let artifacts = LocalDirArtifactStore::new(artifacts_dir.path());

        let contexts: HashMap<String, JobContext> = [
            ("ok.x86_64-linux".to_string(), ok_job),
            ("bad.x86_64-linux".to_string(), bad_job),
            ("mate.x86_64-linux".to_string(), mate_job),
        ]
        .into();
        let batch = BatchView {
            build_id: Some(build_id.to_string()),
            exit_code: Some(1),
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
            engine_cancelled: false,
            submitted_at: Some("2026-05-26T01:00:00Z".into()),
        };
        let requeue = process_settled_batch(
            &state,
            &reader,
            &LogAdmin::default(),
            &store,
            Some((&artifacts, "parity/campaigns/c1".to_string())),
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
            "ssh-ng://rio@gw:22?ssh-key=/k",
            &HashMap::new(),
        )
        .await
        .unwrap();

        // mate's trigger (T) is not in its dep closure → requeued.
        assert_eq!(requeue, vec!["mate.x86_64-linux".to_string()]);
        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(records.len(), 2);
        let ok = records.iter().find(|r| r.job == "ok.x86_64-linux").unwrap();
        assert_eq!(ok.bucket, "match-built");
        assert_eq!(
            ok.rio.outputs["out"].nar_hash.as_deref(),
            Some("ab".repeat(32).as_str())
        );
        assert!(!ok.repro.contains("ssh-key"));
        let bad = records
            .iter()
            .find(|r| r.job == "bad.x86_64-linux")
            .unwrap();
        assert_eq!(bad.bucket, "rio-only-failure");
        assert_eq!(bad.signature.as_deref(), Some("failed-every-worker"));
        assert!(
            bad.log_key
                .as_deref()
                .unwrap()
                .ends_with("bad.x86_64-linux.log.zst")
        );
        assert!(
            artifacts_dir
                .path()
                .join("parity/campaigns/c1/logs/bad.x86_64-linux.log.zst")
                .exists()
        );
        // No build_id → whole batch re-offered.
        let r = process_settled_batch(
            &state,
            &reader,
            &LogAdmin::default(),
            &store,
            None,
            &contexts,
            &["ok.x86_64-linux".into()],
            &BatchView::default(),
            &HashMap::new(),
            &Knobs::default(),
            "leaf",
            "ssh-ng://x",
            &HashMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(r, vec!["ok.x86_64-linux".to_string()]);
    }

    /// A reader failure (RPC error, truncated graph) propagates out of
    /// process_settled_batch without writing any records, so the engine
    /// loop can retry the whole batch on a later collect tick.
    #[tokio::test]
    async fn process_settled_batch_propagates_reader_failures() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let reader = FakeReader::default();
        reader.fail_with("GetBuildGraph(b1) truncated at 7000 nodes");
        let contexts: HashMap<String, JobContext> = [(
            "ok.x86_64-linux".to_string(),
            ctx("ok.x86_64-linux", T, &[], HydraOutcome::Built),
        )]
        .into();
        let batch = BatchView {
            build_id: Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".to_string()),
            ..BatchView::default()
        };
        let err = process_settled_batch(
            &state,
            &reader,
            &LogAdmin::default(),
            &FakeStoreApi::default(),
            None,
            &contexts,
            &["ok.x86_64-linux".into()],
            &batch,
            &HashMap::new(),
            &Knobs::default(),
            "leaf",
            "ssh-ng://x",
            &HashMap::new(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("truncated"), "{err}");
        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert!(records.is_empty(), "no records on a failed read");
    }

    /// A store query failure on a success must not lose the terminal record:
    /// the job is still recorded as built, just without NAR identity, and
    /// the comparison stays not-comparable instead of a false differs.
    #[tokio::test]
    async fn store_failure_records_success_without_nar_identity() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let reader = FakeReader::default();
        let build_id = "0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a";
        reader.set(build_id, obs(T, "completed", Some("e1"), None));
        let store = FakeStoreApi::default();
        store.fail_with("BatchQueryPathInfo: store unavailable");
        let mut ok_job = ctx("ok.x86_64-linux", T, &[], HydraOutcome::Built);
        ok_job.hydra_outputs.insert(
            "out".to_string(),
            super::super::model::HydraOutput {
                narinfo_present: true,
                nar_hash: Some(format!("sha256:{}", "0".repeat(52))),
                nar_size: Some(1),
            },
        );
        let contexts: HashMap<String, JobContext> =
            [("ok.x86_64-linux".to_string(), ok_job)].into();
        let batch = BatchView {
            build_id: Some(build_id.to_string()),
            exit_code: Some(0),
            ..BatchView::default()
        };
        let requeue = process_settled_batch(
            &state,
            &reader,
            &LogAdmin::default(),
            &store,
            None,
            &contexts,
            &["ok.x86_64-linux".into()],
            &batch,
            &HashMap::new(),
            &Knobs::default(),
            "leaf",
            "ssh-ng://x",
            &HashMap::new(),
        )
        .await
        .unwrap();
        assert!(requeue.is_empty());
        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].bucket, "match-built");
        assert_eq!(records[0].rio.outputs["out"].nar_hash, None);
        assert_eq!(records[0].nar_compare["out"], "not-comparable");
    }

    /// A poisoned target with no relayed reason and no failed-builder
    /// evidence (both signals lost) pulls the log tail as the third signal.
    /// The same fetch is reused for evidence capture (one log_tail call, not
    /// two), the record carries the log-tail-only flag, and the signature is
    /// derived from the tail so these failures still group.
    #[tokio::test]
    async fn log_tail_only_failure_reuses_one_fetch_and_groups_by_tail() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let reader = FakeReader::default();
        let build_id = "0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a";
        // failed_builders None = the poison evidence already decayed; the
        // batch carries no relayed reason for this drv either.
        reader.set(build_id, obs(T, "poisoned", None, None));
        let admin = LogAdmin::default();
        let contexts: HashMap<String, JobContext> = [(
            "bad.x86_64-linux".to_string(),
            ctx("bad.x86_64-linux", T, &[], HydraOutcome::Built),
        )]
        .into();
        let batch = BatchView {
            build_id: Some(build_id.to_string()),
            exit_code: Some(1),
            ..BatchView::default()
        };
        let requeue = process_settled_batch(
            &state,
            &reader,
            &admin,
            &FakeStoreApi::default(),
            None,
            &contexts,
            &["bad.x86_64-linux".into()],
            &batch,
            &HashMap::new(),
            &Knobs::default(),
            "leaf",
            "ssh-ng://x",
            &HashMap::new(),
        )
        .await
        .unwrap();
        assert!(requeue.is_empty());
        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(records.len(), 1);
        let rec = &records[0];
        assert_eq!(rec.bucket, "rio-only-failure");
        assert_eq!(rec.evidence.as_deref(), Some("log-tail-only"));
        // No relayed reason: the signature falls back to the captured tail.
        assert_eq!(rec.signature.as_deref(), Some("log:gcc--fatal-error"));
        assert_eq!(
            rec.log_key.as_deref(),
            Some("logs/bad.x86_64-linux.log.zst")
        );
        assert!(state.path("logs/bad.x86_64-linux.log.zst").exists());
        // The Signal-3 fetch doubled as the evidence capture: one call only.
        assert_eq!(admin.log_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// A failed at-capture log upload still records the deterministic S3 key
    /// (the periodic state sync re-enumerates logs/*.log.zst and retries the
    /// upload from the local copy, which is kept).
    #[tokio::test]
    async fn failed_log_upload_still_records_the_log_key() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let reader = FakeReader::default();
        let build_id = "0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a";
        reader.set(
            build_id,
            obs(T, "poisoned", Some("e2"), Some(vec!["b1".into()])),
        );
        let contexts: HashMap<String, JobContext> = [(
            "bad.x86_64-linux".to_string(),
            ctx("bad.x86_64-linux", T, &[], HydraOutcome::Built),
        )]
        .into();
        let batch = BatchView {
            build_id: Some(build_id.to_string()),
            exit_code: Some(1),
            reasons: BTreeMap::from([(
                T.to_string(),
                "failed on every eligible worker".to_string(),
            )]),
            ..BatchView::default()
        };
        process_settled_batch(
            &state,
            &reader,
            &LogAdmin::default(),
            &FakeStoreApi::default(),
            Some((&FailingArtifacts, "parity/campaigns/c1".to_string())),
            &contexts,
            &["bad.x86_64-linux".into()],
            &batch,
            &HashMap::new(),
            &Knobs::default(),
            "leaf",
            "ssh-ng://x",
            &HashMap::new(),
        )
        .await
        .unwrap();
        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].log_key.as_deref(),
            Some("parity/campaigns/c1/logs/bad.x86_64-linux.log.zst")
        );
        assert!(
            state.path("logs/bad.x86_64-linux.log.zst").exists(),
            "local copy stays for the sync retry"
        );
    }
}
