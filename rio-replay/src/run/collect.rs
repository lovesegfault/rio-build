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
use super::ledger::{measured_attempt_requeues, stamped_attempts};
use super::model::{
    BATCH_KIND_TIMED, ExpectedOutcome, ExpectedSide, FailureKind, JobRecord, PathOutcome,
    RequeueReason, RioOutcome, RioSide, RootCauseKind, UnifiedClass, Verdict,
    build_status_from_name, now_rfc3339,
};
use super::spec::Knobs;
use super::state::{StateDir, StateFile};
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
    /// Fixed-output derivations (outputHash present in the drv ATerm)
    /// among the campaign's targets and dependencies, derived once from
    /// the archive at plan time and shared across contexts. Membership is
    /// a plain fact, never an Option whose disabling default every caller
    /// passes: it feeds the source-rot attribution for both the target
    /// itself and a failing dependency trigger.
    pub fixed_output_drvs: std::sync::Arc<HashSet<String>>,
}

/// Consumed budgets a deciding member arrives with, read from the
/// tracker's counters (live) or the journal fold (resume) at the
/// chokepoint that calls [`decide`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PriorBudgets {
    /// TOTAL engine resubmissions so far — every requeue reason counts
    /// (see [`decide`]'s conservative-budget contract).
    pub requeues: u32,
    /// Engine-cancelled carve-out re-offers already granted (the
    /// `RequeueReason::EngineCancelled` why-slice of the same journal).
    pub cancel_cycles: u32,
}

/// Proof token that a requeue decision consulted its bound.
///
/// [`CollectDecision::Requeue`] cannot be constructed without a
/// [`RequeueBudget`], and the witness's field is private to this module —
/// the three minting methods below are the ONLY ways to obtain one. A new
/// failure arm therefore cannot re-offer jobs while forgetting the retry
/// budget: it must either pass one of the budget checks or explicitly name
/// the carve-out (and inherit the carve-out's documented backstop).
mod requeue_budget {
    use super::super::spec::Knobs;

    /// What the requeue charges. Private to the witness module so the
    /// only way to obtain an exempt witness is the probe constructor.
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Charge {
        /// Normal re-offer: the resubmission is journaled and counted.
        Counted,
        /// Canary-probe re-offer: neither journaled nor counted.
        ProbeExempt,
    }

    /// See the module docs. The field is private on purpose: outside
    /// this module (including the rest of collect.rs) the type can be
    /// moved and matched but never built.
    #[derive(Debug, Clone, PartialEq)]
    pub struct RequeueBudget(Charge);

    impl RequeueBudget {
        /// The single auto-retry budget for transport-shaped defects
        /// (missing in-band result, engine-side submission failure,
        /// positively-identified infra failure): `prior_requeues <
        /// max_auto_retries`. Any prior requeue of any reason consumes it
        /// — see [`super::decide`]'s conservative-budget contract.
        pub fn auto_retry(prior_requeues: u32, knobs: &Knobs) -> Option<Self> {
            (prior_requeues < knobs.max_auto_retries).then_some(Self(Charge::Counted))
        }

        /// The bound for jobs denied a fair attempt by their batch (a
        /// fail-fast cancellation whose trigger is outside the job's own
        /// closure, or a dependency failure with no identifiable trigger):
        /// `prior_requeues < failfast_singleton_after + max_auto_retries`.
        ///
        /// Wider than [`Self::auto_retry`] by design: these re-offers are
        /// how a healthy job escapes a failing batch-mate, and fail-fast
        /// singleton isolation — the mechanism that guarantees the escape —
        /// only engages after `failfast_singleton_after` resubmissions. The
        /// budget therefore covers isolation plus the standard auto-retry
        /// slack; a job still hitting these arms past that point (singled
        /// out, yet its "dependency" trigger keeps falling outside its
        /// recorded closure) is wedged on defective closure data or
        /// degraded scheduler triggers, and another re-offer cannot fix it.
        pub fn unfair_attempt(prior_requeues: u32, knobs: &Knobs) -> Option<Self> {
            let limit = knobs
                .failfast_singleton_after
                .saturating_add(knobs.max_auto_retries);
            (prior_requeues < limit).then_some(Self(Charge::Counted))
        }

        /// The engine-cancelled carve-out: a batch the ENGINE itself
        /// cancelled (batch deadline, abort) re-offers its members without
        /// consuming the auto-retry budget — the cancellation is the
        /// engine's own act, not evidence about the job. The carve-out
        /// carries its OWN explicit bound: `prior_cancel_cycles <
        /// max_engine_cancel_cycles`, each granted cycle journaled (the
        /// `RequeueReason::EngineCancelled` why-slice) so the budget survives
        /// restarts. Exhaustion terminalizes — a job whose batches the
        /// engine keeps cancelling has consumed cycles x batch_timeout of
        /// cluster time without producing a result, and another re-offer
        /// cannot converge. Do NOT reach for this constructor from a new
        /// arm without consulting the cycle budget.
        pub fn engine_cancelled(prior_cancel_cycles: u32, knobs: &Knobs) -> Option<Self> {
            (prior_cancel_cycles < knobs.max_engine_cancel_cycles).then_some(Self(Charge::Counted))
        }

        /// The canary-probe carve-out: an infra-shaped failure of a probe
        /// batch (released by the paused submit loop to test whether the
        /// infrastructure recovered) re-offers the member WITHOUT
        /// journaling or counting the resubmission — the failure is
        /// evidence about the outage, not about the job, and charging it
        /// would convert a transient outage into per-job budget exhaustion
        /// and mass terminal retirement. Bounded by the probe ladder
        /// itself: the poller releases at most one single-job probe per
        /// cycle and escalates to the operator PAUSE file after
        /// `INFRA_PROBE_PAUSE_AFTER` consecutive failed cycles, so an
        /// outage costs at most that many exempt re-offers before an
        /// operator must intervene.
        pub fn probe_carveout() -> Self {
            Self(Charge::ProbeExempt)
        }

        /// True when this witness was minted by the probe carve-out: the
        /// decision consumer applies the re-offer without journaling or
        /// counting (`fold(requeues.jsonl) == live counters` holds because
        /// neither side moves).
        pub fn probe_exempt(&self) -> bool {
            self.0 == Charge::ProbeExempt
        }
    }
}

pub use requeue_budget::RequeueBudget;

/// What collect decided for one job after looking at one settled batch.
///
/// In-band per-root results are terminal by construction (the build call
/// does not return until every requested root has an outcome), so there is
/// no still-running decision.
///
/// TOTAL over batch members: [`process_settled_batch`] returns exactly one
/// decision per member — "skip" is unrepresentable. A member the pass
/// deliberately leaves to another owner (the timed dispatcher's retries,
/// the end-of-run backfill) is an explicit [`CollectDecision::Defer`], and
/// a duplicate dropped by the already-terminal belt is an explicit
/// [`CollectDecision::AlreadyTerminal`] — both are lifecycle decisions the
/// caller maps to a ledger exit, so no arm can leave a watchdog clock
/// accruing toward a spurious stall on a member nothing is working on.
/// Every job-state transition a settled batch causes flows through this
/// enum, so a new failure arm cannot re-offer jobs through a side channel
/// that forgets the retry budget — there is no raw job-list return to
/// reach for, and the `Requeue` variant itself demands a [`RequeueBudget`]
/// witness that only the budget checks (or the named carve-outs) can mint.
/// The reason is a typed [`RequeueReason`], not a string: the journaled
/// reason carries measurement semantics (`counts_as_cluster_attempt`), so
/// a new arm cannot invent a reason the measurement has not classified.
#[derive(Debug, Clone, PartialEq)]
pub enum CollectDecision {
    /// Terminal outcome — the results.jsonl record has been written; the
    /// caller retires the job.
    Terminal {
        rio: RioOutcome,
        evidence: Option<String>,
    },
    /// Non-terminal: re-offer to the submit loop (fail-fast batch-mate,
    /// engine-cancelled batch, infra auto-retry, missing in-band result).
    Requeue {
        why: RequeueReason,
        budget: RequeueBudget,
    },
    /// Deliberately left unresolved by this pass — another owner holds the
    /// member's resolution (the timed dispatcher's confirmation retries,
    /// the end-of-run not-attempted backfill, or no job context exists to
    /// record against). The caller releases the member's stall clock: a
    /// deferred member has nothing in flight for the ladder to measure,
    /// and its bound is the deferral target, not the watchdog.
    Defer { reason: &'static str },
    /// The member already holds a terminal record and this batch's result
    /// is not a sanctioned superseding write: dropped by the belt. The
    /// caller retires the job — a recorded member must never keep a live
    /// clock for the stall ladder to overwrite its real verdict with.
    AlreadyTerminal,
}

impl CollectDecision {
    /// The requeue reason's wire string when this decision is a re-offer,
    /// `None` for terminals — the assertable view of a decision whose
    /// budget witness callers cannot construct.
    pub fn requeue_why(&self) -> Option<&'static str> {
        match self {
            CollectDecision::Requeue { why, .. } => Some(why.as_str()),
            CollectDecision::Terminal { .. }
            | CollectDecision::Defer { .. }
            | CollectDecision::AlreadyTerminal => None,
        }
    }
}

/// Fetch-error signatures for the fixed-output source-rot arm: text a
/// failed FOD's evidence carries when the upstream origin (not rio)
/// refused the fetch. Sourced from real fetcher output — curl/wget lines
/// the daemon's last-N-log-lines block embeds, plus the nixpkgs fetchurl
/// builder's mirror-exhaustion line ("error: cannot download X from any
/// mirror") and modern curl's "(22) The requested URL returned error:
/// NNN" shape.
///
/// Needles are heuristics and several ("TLS", "SSL", "timed out") overlap
/// rio's own infrastructure vocabulary — the scheduler's infra relay
/// routinely embeds `'PutPathChunked' timed out after 30s` from a
/// store-upload failure. They are therefore consulted ONLY for evidence
/// whose Signal 1 carries no positive structured classification (see
/// [`resolve_failure_kind`]); the cross-product test
/// `needle_scan_never_shadows_positively_classified_reasons` pins that no
/// positively-classified scheduler reason can be shadowed by a needle.
const FETCH_NEEDLES: &[&str] = &[
    "unable to download",
    "cannot download",
    "couldn't resolve host",
    "Couldn't resolve host",
    "error 404",
    "404 Not Found",
    "The requested URL returned error:",
    "TLS",
    "SSL",
    "timed out",
];

/// Whether `text` carries a fetch-error signature ([`FETCH_NEEDLES`]).
fn fetch_signature_present(text: &str) -> bool {
    FETCH_NEEDLES.iter().any(|n| text.contains(n))
}

/// Failure-kind resolution from the two signals (+ fixed-output knowledge
/// and a log tail). Ambiguous or contradictory evidence defaults to
/// [`FailureKind::Genuine`] — an unexplained failure is charged to rio,
/// never excused as infrastructure. `is_fixed_output` is a plain bool on
/// purpose: the source-rot arm is reachable exactly when the caller's
/// derivation facts say so, not gated behind an optional default that
/// silently disables it.
///
/// Precedence: positive structured classification beats the source-rot
/// needle heuristic. The design's two-signal rule gives infrastructure
/// attribution precedence over every comparison verdict (§7.1:
/// "infra-indeterminate takes precedence over every comparison verdict"),
/// and source-unavailable is defined upstream-origin-only (§7.1: "failed
/// only because a fixed-output input could not be fetched from its
/// upstream origin") — so a reason the scheduler positively classifies as
/// Infra/Timeout/ResourceCeiling is never excused as source rot, however
/// many needle words its text happens to contain. The needle scan runs
/// only in the arms with no positive Signal-1 classification, and inside
/// the lost-Signal-1 arm only when Signal 2 does not positively identify
/// infrastructure (a poison row with an empty executor list).
pub fn resolve_failure_kind(
    reason: Option<&str>,
    failed_builders: Option<&[String]>,
    is_fixed_output: bool,
    log_tail: Option<&str>,
) -> (FailureKind, Option<String>) {
    let signal1 = reason.map(classify_reason);
    // Source rot: a fixed-output derivation whose evidence text carries a
    // fetch-error signature failed because the upstream source is gone,
    // not because rio mis-built it. Only consulted from the
    // non-positively-classified arms below.
    let source_rot = || -> Option<(FailureKind, Option<String>)> {
        if !is_fixed_output {
            return None;
        }
        let evidence_text = format!(
            "{} {}",
            reason.unwrap_or_default(),
            log_tail.unwrap_or_default()
        );
        fetch_signature_present(&evidence_text).then_some((FailureKind::SourceRot, None))
    };
    match signal1 {
        Some(ReasonClass::Timeout) => (FailureKind::Timeout, None),
        Some(ReasonClass::ResourceCeiling) => (FailureKind::ResourceCeiling, None),
        Some(ReasonClass::Infra) => match failed_builders {
            // Contradicting target evidence (real on-worker failures
            // recorded) ⇒ NOT infra: both signals must agree before a
            // failure is excused as infrastructure. Charged to rio — the
            // infra-vocabulary reason is not upstream-fetch evidence, so
            // the needle scan does not get a say here either.
            Some(builders) if !builders.is_empty() => (FailureKind::Genuine, None),
            _ => (FailureKind::Infra, None),
        },
        Some(ReasonClass::Target) | Some(ReasonClass::Dependency { .. }) => {
            source_rot().unwrap_or((FailureKind::Genuine, None))
        }
        None => match failed_builders {
            // Signal 2 positively identifies infrastructure (a poison row
            // whose executor list is empty — infra never inserts into
            // failed_builders): same precedence as a positive Signal 1.
            Some([]) => (FailureKind::Infra, None),
            Some(_) => source_rot().unwrap_or((FailureKind::Genuine, None)),
            // Signal 1 lost AND Signal 2 decayed (the failure outlived the
            // scheduler's poison-evidence TTL): only the log tail is left,
            // so the record carries the "log-tail-only" evidence-quality
            // flag.
            None => {
                source_rot().unwrap_or((FailureKind::Genuine, Some("log-tail-only".to_string())))
            }
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
    /// True when the cancellation was the armed disconnect-replay deadline
    /// firing (from the batch record's bit, set by the submission
    /// chokepoint) — the only evidence that distinguishes a reproduced
    /// recorded interruption from the engine's own build-budget cut.
    pub disconnect_deadline_fired: bool,
    /// Root drvs for which the timed dispatcher armed a recorded
    /// interruption (empty for every non-timed batch).
    pub interruption_drvs: Vec<String>,
    pub submitted_at: Option<String>,
    /// True for a canary-probe batch (from the batch record's bit, set by
    /// the submission chokepoint): its infra-shaped failures take the
    /// budget-exempt probe carve-out instead of consuming the auto-retry
    /// budget.
    pub probe: bool,
    /// 1-based confirmation-retry index for the timed dispatcher's
    /// re-confirmation batches, 0 otherwise (from the batch record): a
    /// SUCCESS result from such a batch passes the already-terminal belt
    /// as a sanctioned superseding write.
    pub confirmation_attempt: u32,
}

/// Derive the timed-interruption flag for one root of a settled batch:
/// `Some(Replayed)` when an interruption was armed for the drv and the
/// armed disconnect-replay deadline fired (the channel was abandoned at the
/// recorded offset), `Some(NotReproduced)` when an interruption was armed
/// but the root completed successfully in band before the abandon deadline,
/// `None` otherwise — including for an engine cancellation by the BUILD
/// deadline (the engine cut the request short before the recorded offset;
/// nothing about the recording was reproduced or refuted), and for every
/// batch that is not a timed-dispatcher submission, so the flag can never
/// leak into timeless classification.
pub fn timed_interruption_for(
    batch: &BatchView,
    drv: &str,
    in_band_success: Option<bool>,
) -> Option<TimedInterruption> {
    let armed = batch.kind == BATCH_KIND_TIMED && batch.interruption_drvs.iter().any(|d| d == drv);
    if !armed {
        return None;
    }
    if batch.disconnect_deadline_fired {
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
///
/// Every requeue arm is bounded through a [`RequeueBudget`] witness: the
/// transport-shaped arms (no in-band result, infra) by the auto-retry
/// budget, the unfair-attempt arms (fail-fast batch-mate, dependency
/// failure with no identifiable trigger) by the wider
/// `failfast_singleton_after + max_auto_retries` bound that covers
/// singleton isolation, and the engine-cancelled arm by the named
/// carve-out whose backstop (the active-stall watchdog) spec validation
/// guarantees via the `batch_timeout_hours > active_stall_hours` ordering.
/// Exhausted bounds terminalize — a deterministic condition can delay a
/// campaign by its budget, never wedge it.
///
/// `log_tail` is third-signal evidence the caller fetched: the TARGET's
/// log tail for evidence-aged failures (the `needs_log_signal` gate), or
/// the TRIGGER's log tail for a dependency-failed row whose fixed-output
/// trigger failed in an earlier batch (cross-batch cascade attribution) —
/// the two gates are disjoint, so the channel is unambiguous per row.
pub fn decide(
    ctx: &JobContext,
    target: Option<&PathOutcome>,
    batch: &BatchView,
    poisoned: &HashMap<String, Vec<String>>,
    prior: PriorBudgets,
    knobs: &Knobs,
    log_tail: Option<&str>,
) -> CollectDecision {
    let prior_requeues = prior.requeues;
    let relayed = batch.reasons.get(&ctx.drv_path).map(String::as_str);
    let Some(target) = target else {
        // No in-band result for this root. An engine-cancelled batch
        // (deadline/abort: the channel was abandoned before results
        // arrived) re-offers within the explicit cycle budget — the
        // cancellation is the engine's own act, but a job whose batches
        // the engine keeps cancelling cannot converge by re-offering
        // (see [`RequeueBudget::engine_cancelled`]); otherwise a missing
        // result is a transport defect — one auto-retry, then an infra
        // failure. A canary probe's missing result (the very outage the
        // probe was sent to test) is exempt from the budget: see
        // [`RequeueBudget::probe_carveout`].
        if batch.engine_cancelled {
            if let Some(budget) = RequeueBudget::engine_cancelled(prior.cancel_cycles, knobs) {
                return CollectDecision::Requeue {
                    why: RequeueReason::EngineCancelled,
                    budget,
                };
            }
            return CollectDecision::Terminal {
                rio: RioOutcome::TargetFailed {
                    kind: FailureKind::Infra,
                },
                evidence: Some(format!(
                    "engine-cancel cycle budget exhausted after {} cycles (each granted the \
                     full batch timeout)",
                    prior.cancel_cycles
                )),
            };
        }
        if batch.probe {
            return CollectDecision::Requeue {
                why: RequeueReason::InfraProbe,
                budget: RequeueBudget::probe_carveout(),
            };
        }
        if let Some(budget) = RequeueBudget::auto_retry(prior_requeues, knobs) {
            return CollectDecision::Requeue {
                why: RequeueReason::NoInbandResult,
                budget,
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
            ctx.fixed_output_drvs.contains(&ctx.drv_path),
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
            // No identifiable trigger: treat as a fail-fast batch-mate —
            // re-offered within the unfair-attempt budget. Exhaustion
            // terminalizes as an infra-indeterminate target failure: the
            // engine could not obtain an attributable attempt for this job
            // within budget, and recording a dependency failure would
            // charge rio with a trigger nobody identified.
            return match RequeueBudget::unfair_attempt(prior_requeues, knobs) {
                Some(budget) => CollectDecision::Requeue {
                    why: RequeueReason::DependencyFailedNoTrigger,
                    budget,
                },
                None => CollectDecision::Terminal {
                    rio: RioOutcome::TargetFailed {
                        kind: FailureKind::Infra,
                    },
                    evidence: Some(format!(
                        "dependency-failed-no-trigger: requeue budget exhausted after \
                         {prior_requeues} re-offers"
                    )),
                },
            };
        };
        if !ctx.dep_drvs.contains(&trigger) && trigger != ctx.drv_path {
            // Fail-fast marks unrelated batch-mates dependency-failed — the
            // trigger is not in this job's own closure, so the job never got
            // a fair attempt and is re-queued instead of being charged with
            // a dependency failure. Bounded by the unfair-attempt budget:
            // past it the job has been singled out (no batch-mates left to
            // blame) and STILL reports an outside-closure trigger, which
            // means defective closure data or a degraded scheduler trigger
            // — re-offering cannot converge, so it terminalizes as
            // infra-indeterminate with the trigger as evidence instead of
            // cycling submit→fail→requeue forever.
            return match RequeueBudget::unfair_attempt(prior_requeues, knobs) {
                Some(budget) => CollectDecision::Requeue {
                    why: RequeueReason::FailfastBatchMate,
                    budget,
                },
                None => CollectDecision::Terminal {
                    rio: RioOutcome::TargetFailed {
                        kind: FailureKind::Infra,
                    },
                    evidence: Some(format!(
                        "failfast-batch-mate: requeue budget exhausted after {prior_requeues} \
                         re-offers; last trigger '{trigger}' is outside the job's recorded \
                         dependency closure"
                    )),
                },
            };
        }
        // Root-cause classification of the trigger, so dependents of an
        // infra-poisoned or source-rotted dependency cascade out of the
        // headline instead of being charged as rio failures.
        let trigger_signal1 = batch.reasons.get(&trigger).map(String::as_str);
        // Evidence-channel completeness for the source-rot scan: the
        // relayed-line capture splits the gateway's multi-line failure
        // payload, so `batch.reasons[trigger]` is the trigger's
        // needle-free FIRST line ("builder for '…' failed with exit code
        // 1;") — the fetch-error signature lives in the lines below it.
        // The dependent's own in-band message is the channel that carries
        // the trigger's COMPLETE text: the scheduler formats the cascade
        // as "dependency '<drv>' failed: <full text>" (embedding the
        // daemon's last-N-log-lines block), and that is exactly this
        // arm's `signal1`. For cross-batch cascades — the trigger failed
        // in an earlier batch, so neither in-band channel carries its
        // text — the caller fetched the TRIGGER's log tail into
        // `log_tail`. Both feed the scan channel; classification of the
        // trigger's reason stays on the relayed line.
        let scan_evidence = (signal1.is_some() || log_tail.is_some()).then(|| {
            format!(
                "{} {}",
                signal1.unwrap_or_default(),
                log_tail.unwrap_or_default()
            )
        });
        let (kind, evidence) = resolve_failure_kind(
            trigger_signal1,
            poisoned.get(&trigger).map(Vec::as_slice),
            // The trigger may be the target itself or any closure member;
            // the shared fixed-output set covers both, so a 404'd
            // fixed-output dependency cascades as source rot instead of a
            // genuine dependency failure.
            ctx.fixed_output_drvs.contains(&trigger),
            scan_evidence.as_deref(),
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
    // single auto-retry while budget remains. On a canary probe an infra
    // failure is the probed outage answering — exempt from the budget
    // ([`RequeueBudget::probe_carveout`]); every NON-infra probe outcome
    // (genuine, timeout, source-rot, …) is evidence the cluster executed
    // the build and classifies normally.
    let (kind, evidence) = resolve_failure_kind(
        signal1,
        poisoned.get(&ctx.drv_path).map(Vec::as_slice),
        ctx.fixed_output_drvs.contains(&ctx.drv_path),
        log_tail,
    );
    if kind == FailureKind::Infra {
        if batch.probe {
            return CollectDecision::Requeue {
                why: RequeueReason::InfraProbe,
                budget: RequeueBudget::probe_carveout(),
            };
        }
        if let Some(budget) = RequeueBudget::auto_retry(prior_requeues, knobs) {
            return CollectDecision::Requeue {
                why: RequeueReason::InfraAutoRetry,
                budget,
            };
        }
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
        plan_not_attemptable: ctx.plan_not_attemptable,
        plan_snapshot_valid: ctx.plan_snapshot_valid,
        timed_interruption: timed_interruption_for(batch, &ctx.drv_path, in_band_success),
        // Filtered/eval-error/identity-divergent/demoted/supply facts never
        // apply HERE: plan-time exclusions are retired before submission,
        // and a member whose required supply settled refused/failed during
        // execution (the per-submission top-up's journal rows) is retired
        // by the collect pass's batch-settle supply rollup BEFORE this
        // classification runs — the already-terminal belt drops it. The
        // resolve-unknown pass runs outside collect.
        ..AuxFlags::default()
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
/// for successes), append terminal records, and return the decision made
/// for every member that transitioned.
///
/// The returned map is the function's whole contract: a
/// [`CollectDecision::Terminal`] entry means the job's results.jsonl record
/// has been appended (the caller retires the job), a
/// [`CollectDecision::Requeue`] entry means the job must be re-offered to
/// the timeless pending pool (the caller counts the resubmission). Members
/// that made no transition — timed-batch members left to the timed
/// dispatcher, members with no job context, members already terminal — have
/// no entry.
///
/// `prior_budgets` carries each job's consumed budgets so far: the TOTAL
/// engine resubmission count (every requeue reason counts, not just infra
/// retries — see [`decide`]) and the engine-cancel cycles already granted
/// through the carve-out.
///
/// Duplicate-batch belt: `already_terminal` is the ledger's in-memory view
/// of jobs whose latest record is terminal. The submit loop can re-submit a
/// job whose terminal record landed between settle and collect (the
/// cool-down damper narrows that window but does not close it), and a batch
/// that outlived its job's stall retirement settles late; either way a
/// member already terminal is dropped here — neither re-offered nor
/// re-recorded — so a duplicate can never overwrite the job's real verdict
/// under latest-record-per-job semantics.
#[allow(clippy::too_many_arguments)]
pub async fn process_settled_batch(
    state: &StateDir,
    admin: &dyn AdminApi,
    store: &dyn StoreApi,
    artifacts: Option<(&dyn ArtifactStore, String)>,
    contexts: &HashMap<String, JobContext>,
    batch_jobs: &[String],
    batch: &BatchView,
    prior_budgets: &HashMap<String, PriorBudgets>,
    knobs: &Knobs,
    mode: &str,
    campaign_id: &str,
    first_active: &HashMap<String, String>,
    already_terminal: &HashSet<String>,
) -> Result<BTreeMap<String, CollectDecision>> {
    let mut decisions: BTreeMap<String, CollectDecision> = BTreeMap::new();
    // Records stamp MEASURED attempts: the journal's cluster-attempt fold,
    // not the budget counter the decisions consult. One journal, two
    // projections — see `ledger::measured_attempt_requeues`. Loaded once
    // per settled batch so every record this pass writes shares one view.
    let prior_attempts = measured_attempt_requeues(state)?;
    // Members of timed batches are never re-offered to the timeless pending
    // pool: the timed dispatcher owns its own retries (confirmation
    // re-submissions), and whatever stays unresolved is covered by the
    // end-of-run not-attempted backfill.
    let timed = batch.kind == BATCH_KIND_TIMED;
    if batch.results.is_empty() && batch.build_id.is_none() && !(timed && batch.engine_cancelled) {
        // Neither in-band results nor a build id: an engine-side submission
        // failure (channel open, drv import, the build op erroring before
        // any result arrived) — or the engine's own cancellation of the
        // whole submission. Members of a timed batch are simply left to
        // the timed dispatcher, which owns its own retries. (A timed batch
        // the engine cancelled with no results falls through instead, so
        // armed interruptions whose disconnect-replay deadline fired are
        // still recorded below; a build-deadline cut leaves its members to
        // the end-of-run backfill like any other unsettled timed member.)
        if timed {
            tracing::info!(
                jobs = batch_jobs.len(),
                "timed batch has no in-band results and no build id (engine-side submission \
                 failure); deferring its members to the timed dispatcher / end-of-run backfill"
            );
            for job in batch_jobs {
                decisions.insert(
                    job.clone(),
                    if already_terminal.contains(job) {
                        CollectDecision::AlreadyTerminal
                    } else {
                        CollectDecision::Defer {
                            reason: "engine-submission-failure-timed",
                        }
                    },
                );
            }
            return Ok(decisions);
        }
        // The per-member decision is [`decide`]'s no-result rule — the one
        // budget site: an engine-cancelled batch (deadline, abort — the
        // engine's own act, not a transport defect) re-offers every member
        // without consuming budget; otherwise the failure consumes the same
        // bounded auto-retry budget as a missing in-band result, then
        // terminalizes as an infrastructure failure carrying the recorded
        // submission error as evidence. Without the bound, a deterministic
        // submission failure (gateway unreachable, host-key mismatch, drv
        // import error) would re-offer its members on every wave and the
        // campaign would never drain.
        // No poison snapshot or log tail is fetched: the failure pre-dates
        // any build, so the scheduler holds no evidence for it.
        let evidence = batch
            .stderr_tail
            .clone()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "engine-submission-failure".to_string());
        for job in batch_jobs {
            let Some(ctx) = contexts.get(job) else {
                tracing::warn!(job, "batch member has no job context; deferring");
                decisions.insert(
                    job.clone(),
                    CollectDecision::Defer {
                        reason: "no-job-context",
                    },
                );
                continue;
            };
            if already_terminal.contains(job) {
                tracing::info!(
                    job,
                    "member of a failed submission already has a terminal record; dropping"
                );
                decisions.insert(job.clone(), CollectDecision::AlreadyTerminal);
                continue;
            }
            let prior = prior_budgets.get(job).copied().unwrap_or_default();
            match decide(ctx, None, batch, &HashMap::new(), prior, knobs, None) {
                // decide() never defers or belt-drops — those are
                // membership decisions made here, not outcome decisions.
                CollectDecision::Defer { .. } | CollectDecision::AlreadyTerminal => {
                    unreachable!("decide() only returns Requeue or Terminal")
                }
                CollectDecision::Requeue { why, budget } => {
                    // The whole submission failed, so "no in-band result"
                    // understates the cause; the engine-cancelled reason
                    // passes through unchanged. The budget witness rides
                    // along — relabeling the reason never re-mints it.
                    let why = if batch.engine_cancelled {
                        why
                    } else {
                        RequeueReason::EngineSubmissionFailure
                    };
                    tracing::info!(job, why = why.as_str(), "re-queueing");
                    decisions.insert(job.clone(), CollectDecision::Requeue { why, budget });
                }
                CollectDecision::Terminal { rio, .. } => {
                    tracing::info!(
                        job,
                        prior_requeues = prior.requeues,
                        "engine-side submission failure with no retry budget left; recording an \
                         infrastructure failure"
                    );
                    let record = build_record(
                        ctx,
                        &rio,
                        Some(evidence.clone()),
                        None,
                        batch,
                        &HashMap::new(),
                        &HashMap::new(),
                        mode,
                        campaign_id,
                        stamped_attempts(prior_attempts.get(job).copied().unwrap_or(0), true),
                        None,
                        first_active.get(job).cloned(),
                        None,
                    );
                    state.append_jsonl(StateFile::Results, &record)?;
                    decisions.insert(
                        job.clone(),
                        CollectDecision::Terminal {
                            rio,
                            evidence: Some(evidence.clone()),
                        },
                    );
                }
            }
        }
        return Ok(decisions);
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
            // No context, no possible record — but the commitment created
            // a watchdog clock (drv-keyed unmapped targets are committed
            // for visibility), so the exit must be explicit or the ladder
            // measures a member nothing will ever resolve.
            tracing::warn!(job, "batch member has no job context; deferring");
            decisions.insert(
                job.clone(),
                CollectDecision::Defer {
                    reason: "no-job-context",
                },
            );
            continue;
        };
        let target = results_by_drv.get(ctx.drv_path.as_str()).copied();
        if already_terminal.contains(job) {
            // Duplicate-batch belt with a writer-legitimacy carve-out.
            // Legitimacy is a property of the WRITING batch, not of the
            // job's state: a confirmation-retry batch is the timed
            // dispatcher's DESIGNED post-terminal superseding writer (an
            // expected-built unit whose first replayed result failed is
            // re-confirmed before unexpected-failure may stand), so its
            // SUCCESS results pass the belt and supersede the initial
            // failure under latest-record-per-job semantics. Everything
            // else — duplicate plain submissions, and retry results that
            // merely re-confirm the failure — is dropped, so a duplicate
            // can never overwrite the job's real verdict.
            let confirmation_supersede = batch.confirmation_attempt > 0
                && target
                    .and_then(|t| build_status_from_name(&t.status))
                    .is_some_and(|status| status.is_success());
            if !confirmation_supersede {
                tracing::info!(
                    job,
                    "settled-batch member already has a terminal record; dropping"
                );
                decisions.insert(job.clone(), CollectDecision::AlreadyTerminal);
                continue;
            }
            tracing::info!(
                job,
                attempt = batch.confirmation_attempt,
                "sanctioned confirmation retry succeeded; superseding the terminal record"
            );
        }
        let prior = prior_budgets.get(job).copied().unwrap_or_default();
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
        // Trigger-keyed evidence for fixed-output cascade attribution
        // (the cross-batch shape): a dependency-failed root whose trigger
        // failed in an EARLIER batch carries neither the trigger's
        // relayed reason (this batch's stderr never saw the trigger fail)
        // nor its full failure text (the dependent's own message wraps a
        // needle-free poison/DAG fallback). When that trigger is a
        // fixed-output derivation and no in-band channel already carries
        // a fetch signature, fetch the TRIGGER's log tail so the
        // source-rot scan sees the fetcher's own output — without it,
        // one rotted FOD charges its whole dependent fan-out to the
        // parity headline as rio regressions. The fetch is keyed by the
        // trigger and feeds only the scan (via decide's log_tail
        // channel, unused for dependency-failed rows otherwise); the
        // dependent's own evidence capture below stays keyed by the
        // dependent.
        let trigger_log_text = if target_status == Some(BuildStatus::DependencyFailed) {
            let signal1 = target
                .map(|t| t.error_msg.as_str())
                .filter(|m| !m.is_empty())
                .or_else(|| batch.reasons.get(&ctx.drv_path).map(String::as_str));
            let trigger = signal1.map(classify_reason).and_then(|class| match class {
                ReasonClass::Dependency { failing_drv } => Some(failing_drv),
                _ => None,
            });
            match trigger {
                Some(trigger)
                    if ctx.fixed_output_drvs.contains(&trigger)
                        && !batch.reasons.contains_key(&trigger)
                        && !signal1.is_some_and(fetch_signature_present) =>
                {
                    admin
                        .log_tail(&trigger, None, knobs.log_tail_bytes)
                        .await
                        .ok()
                        .filter(|bytes| !bytes.is_empty())
                        .map(|bytes| lossy_log_text(&bytes))
                }
                _ => None,
            }
        } else {
            None
        };
        match decide(
            ctx,
            target,
            batch,
            &poisoned,
            prior,
            knobs,
            // At most one is Some: the Signal-3 fetch excludes
            // dependency-failed rows, the trigger fetch covers only them.
            log_signal_text.as_deref().or(trigger_log_text.as_deref()),
        ) {
            // decide() never defers or belt-drops — membership decisions
            // are made above, outcome decisions here.
            CollectDecision::Defer { .. } | CollectDecision::AlreadyTerminal => {
                unreachable!("decide() only returns Requeue or Terminal")
            }
            CollectDecision::Requeue { why, budget } => {
                if timed {
                    // Never re-offered (the timed dispatcher owns retries).
                    // An armed interruption whose disconnect-replay
                    // deadline fired is the recorded outcome reproduced,
                    // so it still gets its terminal record here; every
                    // other requeue-shaped member — including one the
                    // engine's own build deadline cut — stays outstanding
                    // for a later confirmation-retry batch or the
                    // end-of-run backfill (no transition, so no decision
                    // entry).
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
                            stamped_attempts(prior_attempts.get(job).copied().unwrap_or(0), true),
                            None,
                            first_active.get(job).cloned(),
                            None,
                        );
                        state.append_jsonl(StateFile::Results, &record)?;
                        decisions.insert(
                            job.clone(),
                            CollectDecision::Terminal {
                                rio: RioOutcome::NotAttempted,
                                evidence: None,
                            },
                        );
                    } else {
                        tracing::info!(
                            job,
                            why = why.as_str(),
                            "timed batch member is deferred, not re-offered"
                        );
                        decisions.insert(
                            job.clone(),
                            CollectDecision::Defer {
                                reason: why.as_str(),
                            },
                        );
                    }
                } else {
                    tracing::info!(job, why = why.as_str(), "re-queueing");
                    decisions.insert(job.clone(), CollectDecision::Requeue { why, budget });
                }
            }
            CollectDecision::Terminal { rio, evidence } => {
                // Attempts accounting: a confirmation-retry batch carries
                // its 1-based attempt index, so the surviving record
                // reports initial + retries (flakiness then surfaces on
                // the verdict); every other batch derives attempts from
                // the journal's cluster-attempt projection
                // ([`stamped_attempts`] over [`measured_attempt_requeues`]).
                let attempts = if batch.confirmation_attempt > 0 {
                    batch.confirmation_attempt + 1
                } else {
                    stamped_attempts(prior_attempts.get(job).copied().unwrap_or(0), true)
                };
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
                    evidence.clone(),
                    target,
                    batch,
                    &poisoned,
                    &rio_paths,
                    mode,
                    campaign_id,
                    attempts,
                    log_key,
                    first_active.get(job).cloned(),
                    captured_tail.as_deref(),
                );
                state.append_jsonl(StateFile::Results, &record)?;
                decisions.insert(job.clone(), CollectDecision::Terminal { rio, evidence });
            }
        }
    }
    Ok(decisions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::artifact::LocalDirArtifactStore;
    use crate::run::grpc::test_support::FakeStoreApi;
    use crate::run::grpc::{GraphSnapshot, PoisonedView};
    use crate::run::model::{BATCH_KIND_SUBMIT, Disposition, build_status_name};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The re-offer view of a decision map: the jobs the caller must
    /// re-queue, in map (deterministic) order.
    fn requeued(decisions: &BTreeMap<String, CollectDecision>) -> Vec<String> {
        decisions
            .iter()
            .filter(|(_, decision)| matches!(decision, CollectDecision::Requeue { .. }))
            .map(|(job, _)| job.clone())
            .collect()
    }

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
            fixed_output_drvs: std::sync::Arc::new(HashSet::new()),
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

    /// Consumed-budget fixture: `n` prior requeues, no cancel cycles.
    fn prior(requeues: u32) -> PriorBudgets {
        PriorBudgets {
            requeues,
            cancel_cycles: 0,
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
        /// Drv paths `log_tail` was asked for, in call order — so tests
        /// can assert WHICH derivation's evidence was fetched (the
        /// cascade-attribution fetch must be keyed by the trigger, not
        /// the dependent).
        log_drvs: std::sync::Mutex<Vec<String>>,
        poisoned_calls: AtomicUsize,
        tail: Vec<u8>,
        poisoned: Vec<PoisonedView>,
        fail_poisoned: bool,
    }
    impl Default for LogAdmin {
        fn default() -> Self {
            Self {
                log_calls: AtomicUsize::new(0),
                log_drvs: std::sync::Mutex::new(Vec::new()),
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
        async fn log_tail(&self, d: &str, _e: Option<&str>, _m: usize) -> Result<Vec<u8>> {
            self.log_calls.fetch_add(1, Ordering::SeqCst);
            self.log_drvs.lock().unwrap().push(d.to_string());
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
        async fn get_to_file(
            &self,
            _key: &str,
            _dest: &std::path::Path,
        ) -> Result<Option<(String, u64)>> {
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
                false,
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
                false,
                None
            )
            .0,
            FailureKind::Genuine
        );
        // No signal1, poisoned with empty failed_builders → Infra.
        assert_eq!(
            resolve_failure_kind(None, Some(&[]), false, None).0,
            FailureKind::Infra
        );
        // No signal1, failed_builders present → Genuine.
        assert_eq!(
            resolve_failure_kind(None, Some(&["b1".to_string()]), false, None).0,
            FailureKind::Genuine
        );
        // Both signals lost (evidence decayed) → Genuine + log-tail-only flag.
        let (kind, flag) = resolve_failure_kind(None, None, false, Some("error: whatever"));
        assert_eq!(kind, FailureKind::Genuine);
        assert_eq!(flag.as_deref(), Some("log-tail-only"));
        // Timeout / resource ceiling get their own kinds.
        assert_eq!(
            resolve_failure_kind(
                Some("max_timeout_retries=2 exhausted (DeadlineExceeded backstop)"),
                Some(&[]),
                false,
                None
            )
            .0,
            FailureKind::Timeout
        );
        assert_eq!(
            resolve_failure_kind(
                Some("max_infra_retries=3 exhausted at resource ceiling (OomKilled)"),
                Some(&[]),
                false,
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
                true,
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
                true,
                None
            )
            .0,
            FailureKind::Genuine
        );
        // Positive structured classification beats the needle scan, even
        // for a fixed-output drv whose reason text contains a needle: an
        // agreed-infra reason embedding rio's own "timed out" transport
        // text is infrastructure (design §7.1 gives the two-signal infra
        // attribution precedence over every verdict; source-unavailable
        // is upstream-origin-only). Pre-empting it as SourceRot would
        // excuse a rio infra incident from the headline.
        assert_eq!(
            resolve_failure_kind(
                Some(
                    "max_infra_retries=3 exhausted after infrastructure failures: output \
                     upload failed: 'PutPathChunked' timed out after 30s"
                ),
                Some(&[]),
                true,
                None
            )
            .0,
            FailureKind::Infra
        );
        // Same collision under contradicting signal 2: charged to rio as
        // Genuine (the two signals disagree), still never SourceRot.
        assert_eq!(
            resolve_failure_kind(
                Some(
                    "max_infra_retries=3 exhausted after infrastructure failures: output \
                     upload failed: 'PutPathChunked' timed out after 30s"
                ),
                Some(&["b1".to_string()]),
                true,
                None
            )
            .0,
            FailureKind::Genuine
        );
        // Signal-2-only infra (signal 1 lost, poison row with empty
        // executor list) keeps the same precedence: a needle-bearing log
        // tail on a FOD must not reclassify a positively-identified
        // infrastructure failure.
        assert_eq!(
            resolve_failure_kind(None, Some(&[]), true, Some("fetch timed out")).0,
            FailureKind::Infra
        );
        // Without any positive signal, the same needle-bearing log tail
        // IS source rot for a FOD — the must-admit direction.
        assert_eq!(
            resolve_failure_kind(
                None,
                Some(&["b1".to_string()]),
                true,
                Some("curl: (22) The requested URL returned error: 404")
            )
            .0,
            FailureKind::SourceRot
        );
        assert_eq!(
            resolve_failure_kind(
                None,
                None,
                true,
                Some("error: cannot download src.tar.gz from any mirror")
            )
            .0,
            FailureKind::SourceRot
        );
    }

    /// Vocabulary-collision cross-product: the scheduler's relayed-reason
    /// corpus × the fixed-output fetch-needle list. Quantification domain:
    /// every reason string the scheduler can relay
    /// ([`scheduler_reason_corpus`] — the same corpus that pins
    /// `classify_reason`'s totality) crossed with every needle in
    /// [`FETCH_NEEDLES`].
    ///
    /// Must-block: no positively-classified reason (Infra / Timeout /
    /// ResourceCeiling — the scheduler's own structured vocabulary) may be
    /// shadowed into SourceRot by a needle match, under any signal-2 state,
    /// even with fixed-output knowledge asserted; the resolved kind must
    /// stay the one the structured classification dictates. Contract:
    /// design §7.1 — infra attribution by the two-signal rule "takes
    /// precedence over every comparison verdict", and source-unavailable
    /// is defined upstream-origin-only.
    ///
    /// Must-admit: a non-positively-classified corpus reason carrying a
    /// needle still resolves SourceRot for a fixed-output drv (the scan
    /// stays reachable where it belongs).
    ///
    /// Non-vacuity: the corpus must contain at least one
    /// positively-classified reason that textually collides with a needle
    /// (rio's own infra relay embeds "timed out" transport text), so the
    /// must-block direction is exercised by a real collision, not an empty
    /// intersection — and future needle additions that newly shadow a
    /// scheduler vocabulary entry fail here the day they land.
    #[test]
    fn needle_scan_never_shadows_positively_classified_reasons() {
        use crate::run::stderrparse::scheduler_reason_corpus;
        let builder = ["b1".to_string()];
        let signal2_states: [Option<&[String]>; 3] = [Some(&[]), Some(&builder), None];
        let mut positive_collisions = 0usize;
        let mut admitted = 0usize;
        for (reason, class) in scheduler_reason_corpus() {
            let collides = FETCH_NEEDLES.iter().any(|n| reason.contains(n));
            let positive = matches!(
                class,
                ReasonClass::Infra | ReasonClass::Timeout | ReasonClass::ResourceCeiling
            );
            for builders in signal2_states {
                let (kind, _) = resolve_failure_kind(Some(reason), builders, true, None);
                if positive {
                    positive_collisions += usize::from(collides);
                    assert_ne!(
                        kind,
                        FailureKind::SourceRot,
                        "needle scan shadowed a positively-classified scheduler reason \
                         (signal2={builders:?}): {reason}"
                    );
                    let expected = match (&class, builders) {
                        (ReasonClass::Timeout, _) => FailureKind::Timeout,
                        (ReasonClass::ResourceCeiling, _) => FailureKind::ResourceCeiling,
                        (ReasonClass::Infra, Some(b)) if !b.is_empty() => FailureKind::Genuine,
                        (ReasonClass::Infra, _) => FailureKind::Infra,
                        _ => unreachable!(),
                    };
                    assert_eq!(kind, expected, "structured kind for: {reason}");
                } else if collides {
                    admitted += 1;
                    assert_eq!(
                        kind,
                        FailureKind::SourceRot,
                        "non-positively-classified needle-bearing reason must stay \
                         source rot for a FOD (signal2={builders:?}): {reason}"
                    );
                }
            }
        }
        assert!(
            positive_collisions > 0,
            "vacuous cross-product: the corpus must contain a positively-classified \
             reason that collides with a needle"
        );
        assert!(
            admitted > 0,
            "vacuous must-admit direction: the corpus must contain a needle-bearing \
             reason the scan still classifies as source rot"
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
                prior(0),
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
                    prior(0),
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
                prior(0),
                &knobs,
                None
            )
            .requeue_why(),
            Some("infra-auto-retry")
        );
        assert!(matches!(
            decide(
                &c,
                Some(&infra),
                &batch,
                &poisoned_no_builders,
                prior(1),
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
                prior(0),
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
        // A FIXED-OUTPUT target with the same agreed-infra shape, whose
        // reason embeds rio's own "timed out" transport text: the positive
        // infra classification wins over the source-rot needle scan
        // (design §7.1 precedence; source-unavailable is upstream-origin
        // only), so the failure keeps the infra auto-retry instead of
        // being excused from the headline as source rot.
        let mut fod_ctx = ctx("app.x86_64-linux", T, &[DEP], ExpectedOutcome::Built);
        fod_ctx.fixed_output_drvs = std::sync::Arc::new([T.to_string()].into_iter().collect());
        let infra_needle = po(
            T,
            BuildStatus::TransientFailure,
            "max_infra_retries=3 exhausted after infrastructure failures: output upload \
             failed: 'PutPathChunked' timed out after 30s",
        );
        assert_eq!(
            decide(
                &fod_ctx,
                Some(&infra_needle),
                &batch,
                &poisoned_no_builders,
                prior(0),
                &knobs,
                None
            )
            .requeue_why(),
            Some("infra-auto-retry"),
            "agreed infra on a fixed-output drv must keep the infra auto-retry, \
             not be reclassified source rot by the 'timed out' needle"
        );
        assert!(matches!(
            decide(
                &fod_ctx,
                Some(&infra_needle),
                &batch,
                &poisoned_no_builders,
                prior(1),
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
                prior(0),
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
                prior(0),
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
        match decide(&c, Some(&bogus), &batch, &no_poison, prior(0), &knobs, None) {
            CollectDecision::Terminal {
                rio: RioOutcome::TargetFailed { .. },
                evidence,
            } => assert!(evidence.unwrap().contains("bogus-status")),
            other => panic!("expected a terminal failure, got {other:?}"),
        }
        // Missing in-band result: engine-cancelled re-offers within its
        // OWN cycle budget — auto-retry exhaustion (prior requeues 5) does
        // not block it, but consumed cancel cycles do: at
        // max_engine_cancel_cycles the next cancellation terminalizes
        // (each granted cycle cost the full batch timeout of cluster
        // time, so re-offering cannot converge).
        let cancelled_batch = BatchView {
            engine_cancelled: true,
            ..BatchView::default()
        };
        assert_eq!(
            decide(
                &c,
                None,
                &cancelled_batch,
                &no_poison,
                prior(5),
                &knobs,
                None
            )
            .requeue_why(),
            Some("engine-cancelled")
        );
        let one_cycle_left = PriorBudgets {
            requeues: 5,
            cancel_cycles: knobs.max_engine_cancel_cycles - 1,
        };
        assert_eq!(
            decide(
                &c,
                None,
                &cancelled_batch,
                &no_poison,
                one_cycle_left,
                &knobs,
                None
            )
            .requeue_why(),
            Some("engine-cancelled"),
            "the last budgeted cycle still re-offers"
        );
        let spent_cycles = PriorBudgets {
            requeues: 5,
            cancel_cycles: knobs.max_engine_cancel_cycles,
        };
        match decide(
            &c,
            None,
            &cancelled_batch,
            &no_poison,
            spent_cycles,
            &knobs,
            None,
        ) {
            CollectDecision::Terminal {
                rio:
                    RioOutcome::TargetFailed {
                        kind: FailureKind::Infra,
                    },
                evidence,
            } => assert!(
                evidence
                    .unwrap()
                    .contains("engine-cancel cycle budget exhausted"),
                "the terminal evidence names the exhausted bound"
            ),
            other => panic!("expected terminal infra at the cycle bound, got {other:?}"),
        }
        assert_eq!(
            decide(&c, None, &batch, &no_poison, prior(0), &knobs, None).requeue_why(),
            Some("no-inband-result")
        );
        match decide(&c, None, &batch, &no_poison, prior(1), &knobs, None) {
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

    /// The canary-probe carve-out, both directions. Must-exempt: a probe
    /// batch's infra-shaped failures (missing in-band result, two-signal
    /// infra) re-offer with the budget-exempt witness even with the
    /// auto-retry budget long exhausted — a probe failure is evidence about
    /// the outage, never charged to the job. Must-still-classify: a probe
    /// whose build actually executed (genuine failure, success) produces
    /// its normal terminal decision — the probe carve-out can only catch
    /// infra shapes, so a recovered cluster's verdicts land as evidence.
    #[test]
    fn probe_batch_exempts_infra_shapes_but_real_verdicts_still_land() {
        let knobs = Knobs::default();
        let c = ctx("app.x86_64-linux", T, &[], ExpectedOutcome::Built);
        let no_poison: HashMap<String, Vec<String>> = HashMap::new();
        let probe_batch = BatchView {
            probe: true,
            ..BatchView::default()
        };

        // Missing in-band result on a probe, budget exhausted (prior 5):
        // exempt requeue, not terminal infra.
        match decide(&c, None, &probe_batch, &no_poison, prior(5), &knobs, None) {
            CollectDecision::Requeue { why, budget } => {
                assert_eq!(why, RequeueReason::InfraProbe);
                assert!(
                    budget.probe_exempt(),
                    "the witness must carry the exemption"
                );
            }
            other => panic!("expected an exempt probe requeue, got {other:?}"),
        }

        // Two-signal infra failure on a probe (infra reason, empty poison
        // entry), budget exhausted: exempt requeue.
        let infra_probe = BatchView {
            probe: true,
            reasons: BTreeMap::from([(
                T.to_string(),
                "max_infra_retries=3 exhausted after infrastructure failures: x".to_string(),
            )]),
            ..BatchView::default()
        };
        let empty_poison: HashMap<String, Vec<String>> = HashMap::from([(T.to_string(), vec![])]);
        match decide(
            &c,
            Some(&po(T, BuildStatus::TransientFailure, "")),
            &infra_probe,
            &empty_poison,
            prior(5),
            &knobs,
            None,
        ) {
            CollectDecision::Requeue { why, budget } => {
                assert_eq!(why, RequeueReason::InfraProbe);
                assert!(budget.probe_exempt());
            }
            other => panic!("expected an exempt probe requeue, got {other:?}"),
        }

        // Genuine failure on a probe: the cluster executed the build — a
        // real verdict, recorded exactly as on a normal batch.
        assert!(matches!(
            decide(
                &c,
                Some(&po(
                    T,
                    BuildStatus::PermanentFailure,
                    "builder failed with exit code 2"
                )),
                &probe_batch,
                &no_poison,
                prior(0),
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

        // Success on a probe: terminal Built, as on a normal batch.
        assert!(matches!(
            decide(
                &c,
                Some(&po(T, BuildStatus::Built, "")),
                &probe_batch,
                &no_poison,
                prior(0),
                &knobs,
                None
            ),
            CollectDecision::Terminal {
                rio: RioOutcome::Built { executed: true },
                ..
            }
        ));

        // The non-probe charged witness is the contrast: same infra shape,
        // same exhausted budget, NO probe flag → terminal infra (the
        // exemption is minted by the probe carve-out alone).
        let non_probe = BatchView::default();
        assert!(matches!(
            decide(&c, None, &non_probe, &no_poison, prior(5), &knobs, None),
            CollectDecision::Terminal {
                rio: RioOutcome::TargetFailed {
                    kind: FailureKind::Infra
                },
                ..
            }
        ));
        // And a within-budget non-probe requeue's witness is NOT exempt.
        match decide(&c, None, &non_probe, &no_poison, prior(0), &knobs, None) {
            CollectDecision::Requeue { budget, .. } => assert!(!budget.probe_exempt()),
            other => panic!("expected a charged requeue, got {other:?}"),
        }
    }

    /// Source-rot reachability through the production decide() path: a
    /// fixed-output derivation (member of the campaign's fixed-output set)
    /// whose failure evidence carries a fetch-error signature classifies
    /// SourceRot — terminal immediately, never the infra auto-retry — and
    /// the same failure on a non-fixed-output drv stays Genuine. The
    /// classifier then keys the verdict on the recorded expectation: an
    /// expected build becomes source-unavailable (excluded from the
    /// headline as ambient decay), an expected failure matches as
    /// match-failed.
    #[test]
    fn fixed_output_fetch_failures_classify_source_rot_end_to_end() {
        let knobs = Knobs::default();
        let no_poison: HashMap<String, Vec<String>> = HashMap::new();
        let fetch_failure = po(
            T,
            BuildStatus::PermanentFailure,
            "builder failed: unable to download 'https://example.com/src.tar.gz'",
        );
        let fixed: std::sync::Arc<HashSet<String>> =
            std::sync::Arc::new([T.to_string()].into_iter().collect());

        let mut fod_ctx = ctx("src.x86_64-linux", T, &[], ExpectedOutcome::Built);
        fod_ctx.fixed_output_drvs = fixed.clone();
        let decision = decide(
            &fod_ctx,
            Some(&fetch_failure),
            &BatchView::default(),
            &no_poison,
            prior(0),
            &knobs,
            None,
        );
        assert_eq!(
            decision,
            CollectDecision::Terminal {
                rio: RioOutcome::TargetFailed {
                    kind: FailureKind::SourceRot
                },
                evidence: None,
            }
        );
        // Expected built → the record carries the source-unavailable
        // verdict (excluded from the parity headline).
        let record = build_record(
            &fod_ctx,
            &RioOutcome::TargetFailed {
                kind: FailureKind::SourceRot,
            },
            None,
            Some(&fetch_failure),
            &BatchView::default(),
            &no_poison,
            &HashMap::new(),
            "leaf",
            "c1",
            1,
            None,
            None,
            None,
        );
        assert_eq!(
            record.verdict.as_deref(),
            Some(Verdict::SourceUnavailable.as_str())
        );
        assert_eq!(record.failure_cause.as_deref(), Some("source-rot"));

        // Expected FAILED + the same source-rot failure agrees with the
        // recording: match-failed, not source-unavailable (design 7.1).
        let mut expected_failed_ctx = ctx("src.x86_64-linux", T, &[], ExpectedOutcome::Failed);
        expected_failed_ctx.fixed_output_drvs = fixed;
        let record = build_record(
            &expected_failed_ctx,
            &RioOutcome::TargetFailed {
                kind: FailureKind::SourceRot,
            },
            None,
            Some(&fetch_failure),
            &BatchView::default(),
            &no_poison,
            &HashMap::new(),
            "leaf",
            "c1",
            1,
            None,
            None,
            None,
        );
        assert_eq!(
            record.verdict.as_deref(),
            Some(Verdict::MatchFailed.as_str())
        );

        // The same fetch-shaped failure on a NON-fixed-output drv stays a
        // genuine rio failure: the set membership is the only thing that
        // can excuse it, and absence is conservative.
        let plain_ctx = ctx("app.x86_64-linux", T, &[], ExpectedOutcome::Built);
        assert!(matches!(
            decide(
                &plain_ctx,
                Some(&fetch_failure),
                &BatchView::default(),
                &no_poison,
                prior(0),
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
    }

    /// A failing fixed-output trigger cascades as source rot through the
    /// evidence channels production actually fills — producer-verbatim
    /// fixtures, not hand-planted ones:
    ///
    /// - The captured relayed reason for the trigger is built by running
    ///   the engine's own line-split capture (`parse_stderr`) over the
    ///   gateway's multi-line relay payload ("derivation '<drv>' failed:
    ///   <full text>" + the rio-cli hint line). That capture keeps only
    ///   the FIRST line of the failure text — needle-free, asserted as
    ///   such — so `batch.reasons[trigger]` structurally cannot carry the
    ///   fetch signature.
    /// - The dependent's own in-band message is the scheduler's cascade
    ///   shape ("dependency '<drv>' failed: <full text>", completion.rs),
    ///   embedding the trigger's complete failure text including the
    ///   daemon's last-N-log-lines block where the curl error lives.
    ///
    /// The cascade arm must classify SourceRot from the dependent's full
    /// text; with only the first-line relay channel (the pre-fix scan
    /// input) the needle is invisible and one rotted FOD charges its
    /// whole dependent fan-out to the headline.
    #[test]
    fn fixed_output_dependency_trigger_cascades_source_rot() {
        use crate::run::stderrparse::parse_stderr;
        let knobs = Knobs::default();
        let mut c = ctx("app.x86_64-linux", T, &[DEP], ExpectedOutcome::Built);
        c.fixed_output_drvs = std::sync::Arc::new([DEP.to_string()].into_iter().collect());

        // The trigger's full terminal message as the daemon produces it:
        // builder-failure line, then the embedded last-N-log-lines block
        // carrying the fetcher's own output.
        let trigger_full_msg = format!(
            "builder for '{DEP}' failed with exit code 1;\n\
             last 10 log lines:\n\
             > trying https://example.com/dep-1.0.tar.gz\n\
             > curl: (22) The requested URL returned error: 404\n\
             For full logs, run 'nix log {DEP}'."
        );
        // The gateway relays it as one multi-line stderr payload with the
        // rio-cli hint appended; the engine's capture splits lines.
        let relay_payload =
            format!("derivation '{DEP}' failed: {trigger_full_msg}\n  ↳ rio-cli logs '{DEP}'");
        let parsed = parse_stderr(&relay_payload);
        // Producer parity: the captured channel is the needle-free first
        // line. If this assertion ever fails, the relay capture changed
        // and the fixture must be re-derived from the new producer.
        assert_eq!(
            parsed.reasons[DEP],
            format!("builder for '{DEP}' failed with exit code 1;")
        );
        assert!(
            !fetch_signature_present(&parsed.reasons[DEP]),
            "the relayed first line must not carry the fetch signature — \
             otherwise this test no longer proves the dependent-text channel"
        );

        let batch = BatchView {
            reasons: parsed.reasons,
            ..BatchView::default()
        };
        let target = po(
            T,
            BuildStatus::DependencyFailed,
            &format!("dependency '{DEP}' failed: {trigger_full_msg}"),
        );
        match decide(
            &c,
            Some(&target),
            &batch,
            &HashMap::new(),
            prior(0),
            &knobs,
            None,
        ) {
            CollectDecision::Terminal {
                rio: RioOutcome::DependencyFailed { root, failing_drv },
                ..
            } => {
                assert_eq!(root, RootCauseKind::SourceRot);
                assert_eq!(failing_drv, DEP);
            }
            other => panic!("expected a source-rot dependency failure, got {other:?}"),
        }
    }

    /// Cross-batch cascade attribution: the fixed-output trigger failed in
    /// an EARLIER batch, so neither in-band channel of THIS batch carries
    /// its failure text — the dependent's message wraps the scheduler's
    /// needle-free poison reason, and `batch.reasons` has no entry for the
    /// trigger. The collector must fetch the TRIGGER's log tail (keyed by
    /// the trigger drv, not the dependent) and classify the cascade as
    /// source rot from the fetcher output found there; the record then
    /// carries the cascaded source-unavailable verdict instead of charging
    /// the dependent to the headline.
    #[tokio::test]
    async fn cross_batch_fixed_output_cascade_fetches_trigger_log() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();

        let mut app = ctx("app.x86_64-linux", T, &[DEP], ExpectedOutcome::Built);
        app.fixed_output_drvs = std::sync::Arc::new([DEP.to_string()].into_iter().collect());
        let admin = LogAdmin {
            // The trigger's log: the fetcher's own output (what the
            // scheduler's log retention still holds for the poisoned
            // FOD).
            tail: b"trying https://example.com/dep-1.0.tar.gz\n\
                    curl: (22) The requested URL returned error: 404\n"
                .to_vec(),
            // The trigger's poison row survives with real worker rows —
            // a 404'd FOD fails on every worker that tries it.
            poisoned: vec![PoisonedView {
                drv_path: DEP.to_string(),
                failed_executors: vec!["b1".into()],
                poisoned_secs_ago: 60,
            }],
            ..LogAdmin::default()
        };
        let store = FakeStoreApi::default();
        let contexts: HashMap<String, JobContext> = [("app.x86_64-linux".to_string(), app)].into();
        // The dependent's row: merged onto the already-poisoned trigger,
        // fail-fasted with the scheduler's cascade message wrapping the
        // POISON reason — needle-free, like production emits for a node
        // poisoned in a previous build.
        let dep_msg = format!(
            "dependency '{DEP}' failed: poison threshold reached after 3 distinct-worker \
             failures"
        );
        let batch = BatchView {
            kind: BATCH_KIND_SUBMIT.to_string(),
            build_id: Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".to_string()),
            results: vec![po(T, BuildStatus::DependencyFailed, &dep_msg)],
            // No entry for the trigger: its failure happened in an
            // earlier batch, so this batch's stderr never relayed it.
            reasons: BTreeMap::from([(T.to_string(), dep_msg.clone())]),
            ..BatchView::default()
        };
        let decisions = process_settled_batch(
            &state,
            &admin,
            &store,
            None,
            &contexts,
            &["app.x86_64-linux".into()],
            &batch,
            &HashMap::new(),
            &Knobs::default(),
            "leaf",
            "c1",
            &HashMap::new(),
            &HashSet::new(),
        )
        .await
        .unwrap();

        assert_eq!(
            decisions["app.x86_64-linux"],
            CollectDecision::Terminal {
                rio: RioOutcome::DependencyFailed {
                    root: RootCauseKind::SourceRot,
                    failing_drv: DEP.to_string(),
                },
                evidence: None,
            },
            "the trigger's log evidence must drive source-rot attribution"
        );
        // The attribution fetch is keyed by the TRIGGER.
        assert!(
            admin.log_drvs.lock().unwrap().iter().any(|d| d == DEP),
            "expected a log fetch keyed by the trigger {DEP}, got {:?}",
            admin.log_drvs.lock().unwrap()
        );
        // And the record carries the cascaded exclusion, not a headline
        // charge.
        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].verdict.as_deref(), Some("source-unavailable"));
        assert!(records[0].cascaded, "cascaded dependent must be flagged");
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
                prior(0),
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
                prior(0),
                &knobs,
                None
            )
            .requeue_why(),
            Some("infra-auto-retry")
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
            prior(0),
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
                prior(0),
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
                prior(0),
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
                prior(0),
                &knobs,
                None
            )
            .requeue_why(),
            Some("failfast-batch-mate")
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
                prior(0),
                &knobs,
                None
            )
            .requeue_why(),
            Some("dependency-failed-no-trigger")
        );
    }

    /// The two unfair-attempt arms (fail-fast batch-mate, dependency
    /// failure with no identifiable trigger) are bounded: re-offered while
    /// `prior_requeues < failfast_singleton_after + max_auto_retries` —
    /// wide enough that singleton isolation engages and gets its fair
    /// shot — and TERMINAL past it, as an infra-indeterminate target
    /// failure carrying the arm and (for the batch-mate arm) the
    /// outside-closure trigger as evidence. A deterministic condition
    /// (truncated closure data, degraded scheduler triggers) can therefore
    /// no longer cycle submit→fail→requeue forever: the watchdog's stall
    /// clocks reset on every phase flip, so without this bound nothing
    /// else terminates the loop.
    #[test]
    fn dependency_failed_arms_exhaust_their_budget_and_terminalize() {
        let knobs = Knobs::default();
        // Defaults: failfast_singleton_after = 3, max_auto_retries = 1 →
        // the unfair-attempt bound is 4 re-offers.
        let limit = knobs.failfast_singleton_after + knobs.max_auto_retries;
        let c = ctx("app.x86_64-linux", T, &[DEP], ExpectedOutcome::Built);
        let no_poison: HashMap<String, Vec<String>> = HashMap::new();
        let mate = po(
            T,
            BuildStatus::DependencyFailed,
            &format!("dependency '{OTHER}' failed: poison threshold reached"),
        );
        let no_trigger = po(T, BuildStatus::DependencyFailed, "");

        // One under the bound: still re-offered.
        for (target, why) in [
            (&mate, "failfast-batch-mate"),
            (&no_trigger, "dependency-failed-no-trigger"),
        ] {
            assert_eq!(
                decide(
                    &c,
                    Some(target),
                    &BatchView::default(),
                    &no_poison,
                    prior(limit - 1),
                    &knobs,
                    None
                )
                .requeue_why(),
                Some(why),
                "one re-offer under the bound must still requeue ({why})"
            );
        }

        // At the bound: terminal infra-indeterminate with arm-named
        // evidence; the batch-mate arm names the outside-closure trigger.
        match decide(
            &c,
            Some(&mate),
            &BatchView::default(),
            &no_poison,
            prior(limit),
            &knobs,
            None,
        ) {
            CollectDecision::Terminal {
                rio:
                    RioOutcome::TargetFailed {
                        kind: FailureKind::Infra,
                    },
                evidence,
            } => {
                let evidence = evidence.unwrap();
                assert!(evidence.contains("failfast-batch-mate"), "{evidence}");
                assert!(evidence.contains(OTHER), "{evidence}");
            }
            other => panic!("expected terminal infra at the bound, got {other:?}"),
        }
        match decide(
            &c,
            Some(&no_trigger),
            &BatchView::default(),
            &no_poison,
            prior(limit),
            &knobs,
            None,
        ) {
            CollectDecision::Terminal {
                rio:
                    RioOutcome::TargetFailed {
                        kind: FailureKind::Infra,
                    },
                evidence,
            } => {
                assert!(
                    evidence.unwrap().contains("dependency-failed-no-trigger"),
                    "evidence names the exhausted arm"
                );
            }
            other => panic!("expected terminal infra at the bound, got {other:?}"),
        }

        // The exhausted record classifies infra-indeterminate (excluded
        // from the genuine-failure headline: the job never got a fair,
        // attributable attempt).
        let record = build_record(
            &c,
            &RioOutcome::TargetFailed {
                kind: FailureKind::Infra,
            },
            Some("failfast-batch-mate: requeue budget exhausted".to_string()),
            Some(&mate),
            &BatchView::default(),
            &no_poison,
            &HashMap::new(),
            "leaf",
            "c1",
            limit + 1,
            None,
            None,
            None,
        );
        assert_eq!(
            record.verdict.as_deref(),
            Some(Verdict::InfraIndeterminate.as_str())
        );
        assert_eq!(record.failure_cause.as_deref(), Some("infra"));
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
            decide(
                &job1,
                target_for(T),
                &batch,
                &poisoned,
                prior(0),
                &knobs,
                None
            ),
            CollectDecision::Terminal {
                rio: RioOutcome::Built { executed: true },
                evidence: None
            }
        );
        assert_eq!(
            decide(
                &job2,
                target_for(DEP),
                &batch,
                &poisoned,
                prior(0),
                &knobs,
                None
            )
            .requeue_why(),
            Some("infra-auto-retry")
        );
        assert_eq!(
            decide(
                &job3,
                target_for(OTHER),
                &batch,
                &poisoned,
                prior(0),
                &knobs,
                None
            )
            .requeue_why(),
            Some("no-inband-result")
        );

        // Second pass (one prior requeue each): both budget-consuming rows go
        // terminal infra; the missing root carries the missing-result
        // evidence.
        assert!(matches!(
            decide(
                &job2,
                target_for(DEP),
                &batch,
                &poisoned,
                prior(1),
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
        match decide(
            &job3,
            target_for(OTHER),
            &batch,
            &poisoned,
            prior(1),
            &knobs,
            None,
        ) {
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

        // Armed + the disconnect-replay deadline fired (the channel was
        // abandoned at the recorded offset): Replayed, and the record
        // classifies interruption-replayed.
        let cancelled = BatchView {
            kind: BATCH_KIND_TIMED.to_string(),
            interruption_drvs: vec![T.to_string()],
            engine_cancelled: true,
            disconnect_deadline_fired: true,
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

        // Armed + engine-cancelled by the BUILD deadline (the engine cut
        // the request short before the recorded offset): no flag — the
        // recorded interruption was neither reproduced nor out-raced, so
        // claiming Replayed would fabricate a fidelity signal.
        let build_cut = BatchView {
            kind: BATCH_KIND_TIMED.to_string(),
            interruption_drvs: vec![T.to_string()],
            engine_cancelled: true,
            disconnect_deadline_fired: false,
            ..BatchView::default()
        };
        assert_eq!(timed_interruption_for(&build_cut, T, None), None);

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
        // results, no observed build id, the disconnect-replay deadline
        // fired.
        let batch = BatchView {
            kind: BATCH_KIND_TIMED.to_string(),
            engine_cancelled: true,
            disconnect_deadline_fired: true,
            interruption_drvs: vec![T.to_string()],
            ..BatchView::default()
        };
        let decisions = process_settled_batch(
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
            &HashSet::new(),
        )
        .await
        .unwrap();
        assert!(requeued(&decisions).is_empty(), "{decisions:?}");
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
        let decisions = process_settled_batch(
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
            &HashSet::new(),
        )
        .await
        .unwrap();
        assert!(requeued(&decisions).is_empty(), "{decisions:?}");
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
                decide(
                    &job,
                    None,
                    &batch,
                    &no_poison,
                    prior(prior_requeues),
                    &knobs,
                    None
                )
                .requeue_why(),
                Some("engine-cancelled"),
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
        // Knobs::default() grants one auto-retry: "spent" already burned it
        // on a real transport retry. The budget map drives the decision; the
        // journal is what the record's attempts derive from (production
        // keeps the two in lockstep via journal-then-increment).
        let prior: HashMap<String, PriorBudgets> =
            [("spent.x86_64-linux".to_string(), prior(1))].into();
        state
            .append_jsonl(
                StateFile::Requeues,
                &crate::run::model::RequeueRecord {
                    job: "spent.x86_64-linux".to_string(),
                    source: crate::run::model::REQUEUE_SOURCE_COLLECT.to_string(),
                    why: RequeueReason::NoInbandResult.as_str().to_string(),
                    at: "2026-05-26T00:00:00Z".to_string(),
                },
            )
            .unwrap();
        let decisions = process_settled_batch(
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
            &HashSet::new(),
        )
        .await
        .unwrap();
        assert_eq!(requeued(&decisions), vec!["fresh.x86_64-linux".to_string()]);
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
        // attempts = the journaled cluster-attempt requeue + the final
        // failed submission this record is about.
        assert_eq!(rec.attempts, 2);
        assert!(rec.build_ids.is_empty());
        // The failure pre-dates any build: nothing to fetch poison evidence
        // or a log tail for.
        assert_eq!(admin.poisoned_calls.load(Ordering::SeqCst), 0);
        assert_eq!(admin.log_calls.load(Ordering::SeqCst), 0);
    }

    /// Both directions of the cluster-attempt measurement on success
    /// records, derived from the journal: a success after an
    /// engine-cancelled wave is a FIRST-attempt success — the cancellation
    /// was the engine's own scheduling act, not evidence about the job
    /// (the carve-out's documented contract) — while a success after a
    /// real infra retry IS flaky: more than one cluster attempt was needed
    /// (the `flaky` field's documented meaning, model::JobRecord).
    #[tokio::test]
    async fn engine_side_requeues_do_not_mark_successes_flaky() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let contexts: HashMap<String, JobContext> = [
            (
                "cut.x86_64-linux".to_string(),
                ctx("cut.x86_64-linux", T, &[], ExpectedOutcome::Built),
            ),
            (
                "retried.x86_64-linux".to_string(),
                ctx("retried.x86_64-linux", DEP, &[], ExpectedOutcome::Built),
            ),
        ]
        .into();
        // History in the journal: "cut" was re-offered by an engine
        // deadline cut, "retried" by a positively-identified infra failure.
        for (job, why) in [
            ("cut.x86_64-linux", RequeueReason::EngineCancelled),
            ("retried.x86_64-linux", RequeueReason::InfraAutoRetry),
        ] {
            state
                .append_jsonl(
                    StateFile::Requeues,
                    &crate::run::model::RequeueRecord {
                        job: job.to_string(),
                        source: crate::run::model::REQUEUE_SOURCE_COLLECT.to_string(),
                        why: why.as_str().to_string(),
                        at: "2026-05-26T00:00:00Z".to_string(),
                    },
                )
                .unwrap();
        }
        // Both succeed on the next wave. The budget map mirrors the journal
        // (one prior requeue each) — and must NOT leak into the records.
        let prior: HashMap<String, PriorBudgets> = [
            ("cut.x86_64-linux".to_string(), prior(1)),
            ("retried.x86_64-linux".to_string(), prior(1)),
        ]
        .into();
        let batch = BatchView {
            kind: BATCH_KIND_SUBMIT.to_string(),
            build_id: Some("0193e4a2-7c1b-7d20-9b3a-3f2e3d4c5b6c".into()),
            results: vec![
                po(T, BuildStatus::Built, ""),
                po(DEP, BuildStatus::Built, ""),
            ],
            ..BatchView::default()
        };
        let decisions = process_settled_batch(
            &state,
            &LogAdmin::default(),
            &FakeStoreApi::default(),
            None,
            &contexts,
            &["cut.x86_64-linux".into(), "retried.x86_64-linux".into()],
            &batch,
            &prior,
            &Knobs::default(),
            "leaf",
            "c1",
            &HashMap::new(),
            &HashSet::new(),
        )
        .await
        .unwrap();
        assert!(requeued(&decisions).is_empty(), "{decisions:?}");
        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        let by_job = |job: &str| records.iter().find(|r| r.job == job).unwrap();
        let cut = by_job("cut.x86_64-linux");
        assert_eq!(cut.verdict.as_deref(), Some("match-built"));
        assert_eq!(
            cut.attempts, 1,
            "an engine-cancelled re-offer is not a cluster attempt"
        );
        assert!(!cut.flaky, "first-real-attempt success is not flaky");
        let retried = by_job("retried.x86_64-linux");
        assert_eq!(retried.verdict.as_deref(), Some("match-built"));
        assert_eq!(retried.attempts, 2, "the infra-failed attempt counts");
        assert!(retried.flaky, "a needed retry IS flaky");
    }

    /// A duplicate batch for a job that already settled terminally (the
    /// submit loop can re-submit inside the settle-to-collect window) is
    /// dropped via the caller-maintained already-terminal view — exactly
    /// like the production collect pass, which seeds the view from the
    /// in-memory results map and extends it with each batch's terminal
    /// decisions. The duplicate must neither overwrite the real verdict
    /// under latest-record-per-job semantics nor re-offer the finished job,
    /// whether it settled as an engine-side failure or with a real result.
    #[tokio::test]
    async fn duplicate_submission_never_clobbers_a_terminal_record() {
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
        // the job's whole retry budget already spent; a second duplicate
        // settles with an in-band FAILURE result (the main-loop shape).
        let duplicate_failure = BatchView {
            kind: BATCH_KIND_SUBMIT.to_string(),
            stderr_tail: Some("engine submission error: channel open failed".into()),
            ..BatchView::default()
        };
        let duplicate_result = BatchView {
            kind: BATCH_KIND_SUBMIT.to_string(),
            build_id: Some("0193e4a2-7c1b-7d20-9b3a-2f2e3d4c5b6b".to_string()),
            results: vec![po(T, BuildStatus::PermanentFailure, "exit code 2")],
            ..BatchView::default()
        };
        let prior: HashMap<String, PriorBudgets> =
            [("ok.x86_64-linux".to_string(), prior(5))].into();
        // Mirror collect_pass_with's bookkeeping: extend the terminal view
        // with each batch's terminal decisions before the next batch.
        let mut already_terminal: HashSet<String> = HashSet::new();
        for (batch, prior) in [
            (&settled, &HashMap::new()),
            (&duplicate_failure, &prior),
            (&duplicate_result, &prior),
        ] {
            let decisions = process_settled_batch(
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
                &already_terminal,
            )
            .await
            .unwrap();
            assert!(requeued(&decisions).is_empty(), "{decisions:?}");
            for (job, decision) in &decisions {
                if matches!(decision, CollectDecision::Terminal { .. }) {
                    already_terminal.insert(job.clone());
                }
            }
        }
        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].verdict.as_deref(), Some("match-built"));
    }

    /// The belt's two directions, pinned together against the design
    /// contract for timed confirmation retries (design doc section 9.2:
    /// an expected-built unit whose replayed result is a failure is
    /// re-submitted up to confirm_attempts before unexpected-failure is
    /// recorded, with attempts and flakiness carried on the verdict).
    ///
    /// Must-admit: the initial timed batch records the genuine-classified
    /// failure; the dispatcher's confirmation retry (confirmation_attempt
    /// > 0) succeeds, passes the already-terminal belt as the sanctioned
    /// superseding writer, and the surviving record is flaky match-built
    /// with attempts = confirmation_attempt + 1. Must-block: a plain
    /// duplicate success (confirmation_attempt == 0) is still dropped, and
    /// a confirmation retry whose result is another FAILURE adds nothing —
    /// the initial record stands.
    #[tokio::test]
    async fn confirmation_retry_success_supersedes_but_duplicates_still_cannot() {
        let job = "flaky.x86_64-linux";
        let contexts: HashMap<String, JobContext> =
            [(job.to_string(), ctx(job, T, &[], ExpectedOutcome::Built))].into();
        let initial_failure = BatchView {
            kind: BATCH_KIND_TIMED.to_string(),
            build_id: Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".to_string()),
            results: vec![po(T, BuildStatus::PermanentFailure, "exit code 2")],
            ..BatchView::default()
        };
        let confirmation_success = BatchView {
            kind: BATCH_KIND_TIMED.to_string(),
            build_id: Some("0193e4a2-7c1b-7d20-9b3a-2f2e3d4c5b6b".to_string()),
            results: vec![po(T, BuildStatus::Built, "")],
            confirmation_attempt: 1,
            ..BatchView::default()
        };
        let confirmation_failure = BatchView {
            kind: BATCH_KIND_TIMED.to_string(),
            build_id: Some("0193e4a2-7c1b-7d20-9b3a-3f2e3d4c5b6c".to_string()),
            results: vec![po(T, BuildStatus::PermanentFailure, "exit code 2")],
            confirmation_attempt: 2,
            ..BatchView::default()
        };
        let plain_duplicate_success = BatchView {
            kind: BATCH_KIND_TIMED.to_string(),
            build_id: Some("0193e4a2-7c1b-7d20-9b3a-4f2e3d4c5b6d".to_string()),
            results: vec![po(T, BuildStatus::Built, "")],
            ..BatchView::default()
        };
        let run_batch =
            async |state: &StateDir, batch: &BatchView, already_terminal: &HashSet<String>| {
                process_settled_batch(
                    state,
                    &LogAdmin::default(),
                    &FakeStoreApi::default(),
                    None,
                    &contexts,
                    &[job.to_string()],
                    batch,
                    &HashMap::new(),
                    &Knobs::default(),
                    "leaf",
                    "c-confirm",
                    &HashMap::new(),
                    already_terminal,
                )
                .await
                .unwrap()
            };

        // Must-admit leg: initial failure, then confirmation success.
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let mut already_terminal: HashSet<String> = HashSet::new();
        let decisions = run_batch(&state, &initial_failure, &already_terminal).await;
        assert!(matches!(decisions[job], CollectDecision::Terminal { .. }));
        already_terminal.insert(job.to_string());
        let decisions = run_batch(&state, &confirmation_success, &already_terminal).await;
        assert!(
            matches!(decisions[job], CollectDecision::Terminal { .. }),
            "the sanctioned retry's success must land: {decisions:?}"
        );
        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(records.len(), 2, "initial + superseding: {records:?}");
        let surviving = &records[1];
        assert_eq!(
            surviving.verdict.as_deref(),
            Some("match-built"),
            "latest-record-per-job semantics flip the verdict"
        );
        assert_eq!(
            surviving.attempts, 2,
            "attempts = confirmation_attempt + 1 (initial + one retry)"
        );
        assert!(
            surviving.flaky,
            "a success on attempt 2 carries flakiness on the verdict"
        );

        // Must-block leg 1: a plain duplicate success cannot clobber.
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let mut already_terminal: HashSet<String> = HashSet::new();
        run_batch(&state, &initial_failure, &already_terminal).await;
        already_terminal.insert(job.to_string());
        let decisions = run_batch(&state, &plain_duplicate_success, &already_terminal).await;
        assert_eq!(
            decisions.get(job),
            Some(&CollectDecision::AlreadyTerminal),
            "an unsanctioned duplicate is dropped (and its clock retired): {decisions:?}"
        );
        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(records.len(), 1, "{records:?}");

        // Must-block leg 2: a confirmation retry that FAILS again adds
        // nothing — the initial record stands (the failure is confirmed,
        // not superseded).
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let mut already_terminal: HashSet<String> = HashSet::new();
        run_batch(&state, &initial_failure, &already_terminal).await;
        already_terminal.insert(job.to_string());
        let decisions = run_batch(&state, &confirmation_failure, &already_terminal).await;
        assert_eq!(
            decisions.get(job),
            Some(&CollectDecision::AlreadyTerminal),
            "a re-confirmed failure is dropped (and its clock retired): {decisions:?}"
        );
        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(records.len(), 1, "{records:?}");
    }

    /// Exit totality over the deliberate skip arms, enumerated as data:
    /// every arm that resolves nothing must still produce an explicit
    /// decision (Defer / AlreadyTerminal) the consumer maps to a ledger
    /// exit — "skip" is unrepresentable, so no settled member can silently
    /// keep a stall clock accruing toward a spurious stalled verdict. The
    /// table IS the skip-arm class definition: a new deliberate-skip arm
    /// joins it with its expected decision, and the per-row totality
    /// assertion (exactly one decision per member) catches an arm that
    /// forgets to decide.
    #[tokio::test]
    async fn every_skip_arm_makes_an_explicit_decision() {
        let job = "skippy.x86_64-linux";
        let no_ctx_job = "ghost.x86_64-linux";
        let contexts: HashMap<String, JobContext> =
            [(job.to_string(), ctx(job, T, &[], ExpectedOutcome::Built))].into();
        let terminal_set: HashSet<String> = [job.to_string()].into();
        let empty_set: HashSet<String> = HashSet::new();

        // (arm name, batch fixture, member, already_terminal view,
        //  expected decision)
        let arms: Vec<(&str, BatchView, &str, &HashSet<String>, CollectDecision)> = vec![
            (
                "timed engine-side submission failure",
                BatchView {
                    kind: BATCH_KIND_TIMED.to_string(),
                    ..BatchView::default()
                },
                job,
                &empty_set,
                CollectDecision::Defer {
                    reason: "engine-submission-failure-timed",
                },
            ),
            (
                "timed engine-cancelled member (requeue-shaped, no replay)",
                BatchView {
                    kind: BATCH_KIND_TIMED.to_string(),
                    build_id: Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".to_string()),
                    results: vec![],
                    engine_cancelled: true,
                    ..BatchView::default()
                },
                job,
                &empty_set,
                CollectDecision::Defer {
                    reason: "engine-cancelled",
                },
            ),
            (
                "timed member with no in-band result",
                BatchView {
                    kind: BATCH_KIND_TIMED.to_string(),
                    build_id: Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".to_string()),
                    results: vec![po(OTHER, BuildStatus::Built, "")],
                    ..BatchView::default()
                },
                job,
                &empty_set,
                CollectDecision::Defer {
                    reason: "no-inband-result",
                },
            ),
            (
                "no-context member of a settled batch",
                BatchView {
                    kind: BATCH_KIND_SUBMIT.to_string(),
                    build_id: Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".to_string()),
                    results: vec![po(T, BuildStatus::Built, "")],
                    ..BatchView::default()
                },
                no_ctx_job,
                &empty_set,
                CollectDecision::Defer {
                    reason: "no-job-context",
                },
            ),
            (
                "no-context member of a failed submission",
                BatchView {
                    kind: BATCH_KIND_SUBMIT.to_string(),
                    ..BatchView::default()
                },
                no_ctx_job,
                &empty_set,
                CollectDecision::Defer {
                    reason: "no-job-context",
                },
            ),
            (
                "already-terminal duplicate of a settled batch",
                BatchView {
                    kind: BATCH_KIND_SUBMIT.to_string(),
                    build_id: Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".to_string()),
                    results: vec![po(T, BuildStatus::Built, "")],
                    ..BatchView::default()
                },
                job,
                &terminal_set,
                CollectDecision::AlreadyTerminal,
            ),
            (
                "already-terminal member of a failed submission",
                BatchView {
                    kind: BATCH_KIND_SUBMIT.to_string(),
                    ..BatchView::default()
                },
                job,
                &terminal_set,
                CollectDecision::AlreadyTerminal,
            ),
            (
                "already-terminal member of a timed submission failure",
                BatchView {
                    kind: BATCH_KIND_TIMED.to_string(),
                    ..BatchView::default()
                },
                job,
                &terminal_set,
                CollectDecision::AlreadyTerminal,
            ),
        ];

        for (name, batch, member, already_terminal, expected) in arms {
            let dir = tempfile::tempdir().unwrap();
            let state = StateDir::new(dir.path()).unwrap();
            let decisions = process_settled_batch(
                &state,
                &LogAdmin::default(),
                &FakeStoreApi::default(),
                None,
                &contexts,
                &[member.to_string()],
                &batch,
                &HashMap::new(),
                &Knobs::default(),
                "leaf",
                "c-skip",
                &HashMap::new(),
                already_terminal,
            )
            .await
            .unwrap();
            assert_eq!(
                decisions.len(),
                1,
                "{name}: exactly one decision per member, never a silent skip: {decisions:?}"
            );
            assert_eq!(
                decisions.get(member),
                Some(&expected),
                "{name}: wrong decision"
            );
            let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
            assert!(
                records.is_empty(),
                "{name}: skip arms write no records: {records:?}"
            );
        }
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
        let prior: HashMap<String, PriorBudgets> =
            [("spent.x86_64-linux".to_string(), prior(5))].into();
        let decisions = process_settled_batch(
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
            &HashSet::new(),
        )
        .await
        .unwrap();
        assert_eq!(requeued(&decisions), vec!["spent.x86_64-linux".to_string()]);
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
            disconnect_deadline_fired: false,
            interruption_drvs: Vec::new(),
            submitted_at: Some("2026-05-26T01:00:00Z".into()),
            probe: false,
            confirmation_attempt: 0,
        };
        let decisions = process_settled_batch(
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
            &HashSet::new(),
        )
        .await
        .unwrap();

        // mate's trigger (T) is not in its dep closure → requeued.
        assert_eq!(requeued(&decisions), vec!["mate.x86_64-linux".to_string()]);
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
        let decisions = process_settled_batch(
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
            &HashSet::new(),
        )
        .await
        .unwrap();
        assert_eq!(requeued(&decisions), vec!["mate.x86_64-linux".to_string()]);
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
        let decisions = process_settled_batch(
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
            &HashSet::new(),
        )
        .await
        .unwrap();
        assert!(requeued(&decisions).is_empty(), "{decisions:?}");
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
        let prior: HashMap<String, PriorBudgets> =
            [("bad.x86_64-linux".to_string(), prior(1))].into();
        let decisions = process_settled_batch(
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
            &HashSet::new(),
        )
        .await
        .unwrap();
        assert!(requeued(&decisions).is_empty(), "{decisions:?}");
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
        let decisions = process_settled_batch(
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
            &HashSet::new(),
        )
        .await
        .unwrap();
        assert!(requeued(&decisions).is_empty(), "{decisions:?}");
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
        let decisions = process_settled_batch(
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
            &HashSet::new(),
        )
        .await
        .unwrap();
        assert!(requeued(&decisions).is_empty(), "{decisions:?}");
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
            &HashSet::new(),
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
            &HashSet::new(),
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
