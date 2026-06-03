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
//! evidence from `ListPoisoned` (Signal 2). Per the design's two-signal
//! rule (§6.6): a positive Signal-1 infra classification stands unless
//! Signal 2 contradicts it with recorded on-worker failures — decayed
//! Signal-2 evidence does not contradict; with Signal 1 lost, only a
//! positive poison row with an empty builder list identifies infra; double
//! losses default to a genuine target failure carrying the log-tail-only
//! evidence flag, so rio is never excused on absence of evidence alone.
//! A dependency-failed root is re-attributed through its own
//! dependency closure: a failing drv inside the closure is a real blocked
//! dependency, while a failing drv outside it means the job was merely a
//! fail-fast batch-mate and is re-queued. A failed root whose row carries
//! only the scheduler's DAG-level fail-fast summary (no per-root event was
//! ever emitted for it) recovers its trigger from OUTSIDE the row: the
//! poison snapshot intersected with the job's dependency closure — a
//! poisoned fixed-output member with recorded worker failures reroutes the
//! row through the cascaded source-rot classification instead of charging
//! the rotted dependency's fan-out to the parity headline.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use anyhow::Result;
use rio_nix::protocol::build::{BuildResult, BuildStatus};

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
    /// Dependency drv closure (from the archive's closure records) — used
    /// for the fail-fast re-attribution rule and for the out-of-row
    /// trigger recovery of DAG-fallback blanket rows.
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
    /// engine-cancel why-slice of the same journal —
    /// [`RequeueReason::is_engine_cancel_cycle`], both the announced and
    /// the fully cancelled variants).
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
        /// engine-cancel why-slice,
        /// `RequeueReason::is_engine_cancel_cycle` — announced and fully
        /// cancelled variants alike) so the budget survives restarts.
        /// Exhaustion terminalizes — a job whose batches the engine keeps
        /// cancelling has consumed cycles x batch_timeout of cluster time
        /// without producing a result, and another re-offer cannot
        /// converge. Do NOT reach for this constructor from a new arm
        /// without consulting the cycle budget.
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

/// THE door predicate for the source-rot log-tail fetches: does this
/// in-band/relayed channel ALREADY satisfy the source-rot evidence bar
/// (design §6.6: a fetch-error needle), so the trigger-keyed (or
/// own-row) tail fetch can be skipped?
///
/// All three fetch doors — the direct fixed-output arm, the
/// dependency-cascade trigger fetch, and the DAG-fallback blanket fetch
/// — must consult THIS predicate, never line PRESENCE: the dominant
/// producer shapes put a needle-FREE line in the captured channel by
/// construction (the relay capture keeps only the first line of a
/// multi-line failure payload, and the scheduler's poison-threshold
/// terminalization emits the bare `poison threshold reached after N
/// distinct-worker failures` summary as the trigger's whole reason —
/// both pinned by the door tests), so suppressing the fetch on mere
/// presence makes the bar unsatisfiable for exactly those rows and the
/// same rotted fixed-output derivation classifies Genuine same-batch
/// but SourceRot cross-batch — the batch-timing classification flip
/// §6.6's one-bar rule exists to prevent. Presence-vs-content is the
/// door's whole decision: one owner here, so no door can re-derive it.
///
/// The caller passes the channel(s) `resolve_failure_kind` will
/// actually scan for that row shape — skipping on a needle the scan
/// would never see would excuse nothing and fetch nothing.
///
/// Fleet cost of the needle-aware doors: one bounded `log_tail` fetch
/// (`knobs.log_tail_bytes`) per dependent row whose trigger channel is
/// needle-free — the same per-row cost class the cross-batch door has
/// always paid; same-batch fan-outs of one rotted FOD now pay it too,
/// in exchange for classifying like their cross-batch siblings.
fn channel_satisfies_rot_bar(line: Option<&str>) -> bool {
    line.is_some_and(fetch_signature_present)
}

/// Whether `text` is the scheduler's build-level fail-fast summary — the
/// DAG-fallback blanket a root's in-band result carries when NO per-root
/// terminal event was ever emitted for it.
///
/// Producer shape, matched exactly. FORMAT: the scheduler records the
/// build's first failure via `rio_proto::dag_first_failure_summary` —
/// `derivation {key} failed` (`rio-scheduler/src/actor/completion.rs`,
/// `handle_derivation_failure`) — and emits it as the `BuildFailed`
/// message; the gateway clones that DAG-level result onto every requested
/// root without a recorded terminal of its own
/// (`rio-gateway/src/handler/build.rs`, `root_evidence` /
/// `per_root_verdict` — "Unverified blanket failure stands"). CONTENT:
/// the interpolated key is the scheduler's DAG key, which the gateway
/// mints as the FULL drv store path (`build_node` in
/// rio-gateway/src/translate.rs sets `drv_hash: drv_path`, content-pinned
/// by `build_node_drv_hash_is_the_full_store_path`), so the production
/// token is `/nix/store/{hash}-{name}.drv`. The bare `{hash}-{name}.drv`
/// basename — the key shape the scheduler's `DrvHash` was historically
/// documented as — is tolerated too, so a future ingress normalization to
/// the documented shape cannot silently kill this recovery arm again.
///
/// Either way the key is a 32-char lowercase nix hash, `-`, the name,
/// `.drv` — never quoted and never followed by a `: <reason>` tail —
/// which distinguishes the blanket from the gateway's per-derivation
/// relay lines (`derivation '<path>' failed: <reason>`), the cascade
/// shape (`dependency '<drv>' failed: …`), and every worker/scheduler
/// reason in the relayed vocabulary.
fn is_dag_fallback_blanket(text: &str) -> bool {
    let Some(node) = text
        .trim()
        .strip_prefix("derivation ")
        .and_then(|rest| rest.strip_suffix(" failed"))
    else {
        return false;
    };
    if node.contains([' ', '\'', ':']) {
        return false;
    }
    // Production keys are full store paths (see CONTENT above); the bare
    // hash-name shape is the documented historical form. Both reduce to
    // the same `{32-char-hash}-{name}.drv` basename (store-path names
    // cannot contain `/`, so a residual slash disqualifies).
    let basename = node.strip_prefix("/nix/store/").unwrap_or(node);
    let bytes = basename.as_bytes();
    bytes.len() > 33
        && bytes[..32]
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && bytes[32] == b'-'
        && basename.ends_with(".drv")
        && !basename.contains('/')
}

/// A cascade trigger recovered from OUTSIDE a failed row: the
/// intersection of the batch's poison snapshot with the job's own
/// dependency closure (target included), restricted to fixed-output
/// derivations with recorded worker failures. See the DAG-fallback
/// blanket arm in [`decide`] for why the row itself cannot name the
/// trigger.
#[derive(Debug)]
struct RecoveredPoisonTrigger {
    /// The deterministically picked trigger: the target itself when its
    /// own poison row qualifies (its rot fully explains the fail-fast
    /// and is no cascade), otherwise the first qualifying closure member
    /// in path order. The archive's closure set carries no topological
    /// order, so lexicographic path order is the deterministic stand-in.
    trigger: String,
    /// EVERY qualifying (drv, failed executors) pair, sorted by path —
    /// all of them are recorded as evidence so a closure with several
    /// rotted fixed-output members stays auditable whichever one the
    /// pick names.
    candidates: Vec<(String, Vec<String>)>,
}

impl RecoveredPoisonTrigger {
    /// The terminal record's evidence: the recovery method and the full
    /// poison rows it matched, so the record explains both WHY the row
    /// was rerouted (its in-band text was the scheduler's DAG-level
    /// fail-fast summary, which proves nothing about this root) and on
    /// WHAT evidence (the poison rows, executors included).
    fn evidence(&self) -> String {
        let rows = self
            .candidates
            .iter()
            .map(|(drv, executors)| format!("'{drv}' (failed executors: {})", executors.join(", ")))
            .collect::<Vec<_>>()
            .join("; ");
        format!(
            "trigger recovered outside the row: the in-band reason is the scheduler's \
             DAG-level fail-fast summary, and the poison snapshot intersected with this \
             job's dependency closure names poisoned fixed-output {rows}; attributed to \
             '{}'",
            self.trigger
        )
    }
}

/// Recover the cascade trigger for a DAG-fallback blanket row from the
/// batch's poison snapshot: the poisoned fixed-output members of the
/// job's own dependency closure (or the target itself), each with at
/// least one recorded worker failure.
///
/// Each gate is load-bearing for the conservative direction — the
/// recovery may only ever EXCLUDE a root that provably never got an
/// attributable attempt, never hide a failure rio executed:
///
/// - **fixed-output membership**: a poisoned NON-fixed-output closure
///   member proves a dependency kept failing, but not that an upstream
///   origin rotted — rerouting on it could excuse a real rio regression
///   in that dependency, so such rows keep the genuine headline charge.
/// - **worker-failure evidence** (non-empty executor list): an empty
///   list is the scheduler's infra-poisoning shape (poison recorded
///   without any worker failing the node — infra never inserts into
///   `failed_builders`), which says nothing about the upstream origin.
/// - **closure membership** (the job's own `dep_drvs`, target included):
///   the DAG-level summary is build-wide and may name a batch-mate's
///   trigger, so only this job's own dependency facts can justify
///   rerouting THIS row.
/// - **contradictory-evidence refusal**: a poisoned NON-fixed-output
///   closure member with recorded worker failures is an executed,
///   retry-exhausted dependency regression blocking this same job
///   (design §6.6: non-empty `failed_executors` is charged to the
///   workload). §7.1's source-unavailable covers a unit that failed
///   ONLY because a fixed-output input could not be fetched — with such
///   a member present that clause cannot hold, so the recovery refuses
///   outright (no candidate is returned, the genuine charge stands)
///   rather than merely filtering the member out of the candidate set:
///   a filter would let a rotted FOD bystander launder a real
///   regression's whole late-batch fan-out off the headline. Cascade
///   intermediates cannot pollute this check — they settle
///   dependency-failed, never poisoned, and ListPoisoned returns only
///   poisoned rows.
fn recover_poisoned_fod_trigger(
    ctx: &JobContext,
    poisoned: &HashMap<String, Vec<String>>,
) -> Option<RecoveredPoisonTrigger> {
    let in_closure = |drv: &str| ctx.dep_drvs.contains(drv) || drv == ctx.drv_path;
    let mut contradicting: Vec<&str> = poisoned
        .iter()
        .filter(|(drv, executors)| {
            !executors.is_empty()
                && !ctx.fixed_output_drvs.contains(drv.as_str())
                && in_closure(drv)
        })
        .map(|(drv, _)| drv.as_str())
        .collect();
    if !contradicting.is_empty() {
        contradicting.sort_unstable();
        tracing::info!(
            target_drv = %ctx.drv_path,
            contradicting = ?contradicting,
            "blanket recovery refused: the poison snapshot also names executed \
             non-fixed-output closure members — a real dependency regression blocks this \
             job, so the genuine charge stands"
        );
        return None;
    }
    let mut candidates: Vec<(String, Vec<String>)> = poisoned
        .iter()
        .filter(|(drv, executors)| {
            !executors.is_empty() && ctx.fixed_output_drvs.contains(drv.as_str()) && in_closure(drv)
        })
        .map(|(drv, executors)| (drv.clone(), executors.clone()))
        .collect();
    candidates.sort();
    let trigger = candidates
        .iter()
        .find(|(drv, _)| *drv == ctx.drv_path)
        .or_else(|| candidates.first())?
        .0
        .clone();
    Some(RecoveredPoisonTrigger {
        trigger,
        candidates,
    })
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
            // recorded) ⇒ NOT infra: per §6.6, Signal 2 must not
            // contradict a positive Signal-1 infra classification.
            // Charged to rio — the infra-vocabulary reason is not
            // upstream-fetch evidence, so the needle scan does not get a
            // say here either.
            Some(builders) if !builders.is_empty() => (FailureKind::Genuine, None),
            // Empty poison row corroborates; decayed evidence (`None`)
            // does not contradict (§6.6: decay is "evidence
            // unavailable", not corroboration of either side) — the
            // positive Signal-1 classification stands in both cells.
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
    /// Roots whose lost-terminal relay marker the gateway emitted on this
    /// submission's stderr (from the batch record): the disambiguator
    /// [`decide`]'s success arm reads — a `Substituted` row for a marked
    /// root stands on a lost evidence channel, not a recorded
    /// substitution event, and classifies as evidence loss instead of
    /// `target-substituted`.
    pub lost_terminals: BTreeSet<String>,
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
    /// Root drv → input derivations the batch's import walk skipped from
    /// THAT root's text closure because the archive does not embed them
    /// (from the batch record): the import-gap breadcrumb the collect
    /// pass consumes — a failed root with an entry here retires as
    /// supply-failed instead of being charged a regression.
    pub import_skipped_by_root: BTreeMap<String, Vec<String>>,
}

impl BatchView {
    /// Whether the cluster acknowledged the settled submission this
    /// batch describes: a build id (the gateway's `rio: build <uuid>`
    /// announcement) or in-band per-root results prove the cluster saw
    /// it; their joint absence is the engine-side shape (submission
    /// failure, fully cancelled engine-cancel cycle) that never reached
    /// the cluster. THE single batch-evidence predicate behind the
    /// attempts measurement: `process_settled_batch` derives
    /// `current_is_cluster_attempt` from it for every record writer, and
    /// decide()'s engine-cancelled arm keys the journaled requeue
    /// vocabulary (announced vs fully cancelled) on it — one predicate,
    /// so the current-event judgment and the journaled history cannot
    /// drift on what "the cluster saw it" means.
    pub fn cluster_acknowledged(&self) -> bool {
        self.build_id.is_some() || !self.results.is_empty()
    }

    /// THE batch-record → batch-view projection. Deliberately an
    /// exhaustive destructuring with no `..` rest pattern: a new
    /// `BatchRecord` field refuses to compile until this constructor
    /// DECIDES whether classification consumes it — the import-skip
    /// breadcrumb was once recorded faithfully and then dropped exactly
    /// here, by a hand-copied view that listed every field except it,
    /// leaving the archive-damage evidence write-only while failed
    /// roots were charged as regressions.
    pub fn of_record(record: &super::model::BatchRecord) -> Self {
        let super::model::BatchRecord {
            // Correlation/bookkeeping keys the caller drives the pass
            // with — not per-job classification evidence.
            batch_id: _,
            jobs: _,
            root_drvs: _,
            est_nodes: _,
            finished_at: _,
            // The operator-facing union view of the import gaps; the
            // per-root attribution below is the consumable form.
            import_skipped_drvs: _,
            // The inline-resume gate's delivery proof (read by the
            // resume path over raw records), not classification's.
            topup_delivered: _,
            kind,
            build_id,
            started_at,
            results,
            reasons,
            lost_terminals,
            stderr_tail,
            engine_cancelled,
            disconnect_deadline_fired,
            interruption_drvs,
            import_skipped_by_root,
            probe,
            confirmation_attempt,
        } = record;
        BatchView {
            kind: kind.clone(),
            build_id: build_id.clone(),
            results: results.clone(),
            reasons: reasons.clone(),
            lost_terminals: lost_terminals.clone(),
            stderr_tail: stderr_tail.clone(),
            engine_cancelled: *engine_cancelled,
            disconnect_deadline_fired: *disconnect_deadline_fired,
            interruption_drvs: interruption_drvs.clone(),
            submitted_at: Some(started_at.clone()),
            probe: *probe,
            confirmation_attempt: *confirmation_attempt,
            import_skipped_by_root: import_skipped_by_root.clone(),
        }
    }
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
/// log tail for evidence-aged failures and for fixed-output targets
/// whose Signal-1 text does not satisfy the source-rot bar (the
/// `needs_log_signal` gate), or the TRIGGER's log tail for a
/// dependency-failed or blanket-shaped row whose fixed-output trigger's
/// needle is not in band (same-batch needle-free captures and
/// cross-batch cascades alike — the doors test channel CONTENT against
/// the bar, never line presence; see `channel_satisfies_rot_bar`) — the
/// gates are pairwise disjoint, so the channel is unambiguous per row.
///
/// A failure row whose Signal-1 text is the scheduler's DAG-level
/// fail-fast summary (`is_dag_fallback_blanket` — the gateway's per-root
/// fallback when no per-root event was ever emitted) recovers its
/// trigger from OUTSIDE the row before the two-signal rule runs: see
/// `recover_poisoned_fod_trigger` and the blanket arm below.
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
        // No in-band result for this root: every branch of this arm is
        // infra-shaped (the engine's own cancellation, or a transport
        // defect), so the probe carve-out gates the ARM'S ENTRY — no
        // infra-shaped budget charge or terminal-mint exit may precede
        // the probe check. A canary probe with no result is the probed
        // outage answering, whatever cut the result off: charging the
        // engine-cancel cycle budget here would grind the conscripted
        // job to a spurious infra terminal within
        // `max_engine_cancel_cycles` hang-shaped probe cycles (the
        // with-result arm orders the same way for the same reason: probe
        // before any budget consult). See
        // [`RequeueBudget::probe_carveout`].
        if batch.probe {
            return CollectDecision::Requeue {
                why: RequeueReason::InfraProbe,
                budget: RequeueBudget::probe_carveout(),
            };
        }
        // An engine-cancelled batch (deadline/abort: the channel was
        // abandoned before results arrived) re-offers within the
        // explicit cycle budget — the cancellation is the engine's own
        // act, but a job whose batches the engine keeps cancelling
        // cannot converge by re-offering (see
        // [`RequeueBudget::engine_cancelled`]); otherwise a missing
        // result is a transport defect — one auto-retry, then an infra
        // failure. The journaled reason carries the batch-evidence bit
        // the record writers stamp attempts from: a cancellation that
        // fired AFTER the cluster acknowledged the submission (a build
        // id, or per-root results for batch-mates —
        // [`BatchView::cluster_acknowledged`], the same predicate
        // `process_settled_batch` derives `current_is_cluster_attempt`
        // from) journals the announced variant, which the measurement
        // counts as a cluster attempt — so the journal fold and the
        // current-event judgment cannot disagree about the same cycle.
        // Both variants charge the same cycle budget
        // ([`RequeueReason::is_engine_cancel_cycle`]).
        if batch.engine_cancelled {
            if let Some(budget) = RequeueBudget::engine_cancelled(prior.cancel_cycles, knobs) {
                let why = if batch.cluster_acknowledged() {
                    RequeueReason::EngineCancelledAnnounced
                } else {
                    RequeueReason::EngineCancelled
                };
                return CollectDecision::Requeue { why, budget };
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
        // An executed Built stands whatever the relay carried: the root's
        // own success terminal is per-root scheduler evidence, strictly
        // stronger than any side-channel marker (the producer never pairs
        // the lost-terminal marker with an own terminal in one batch —
        // the marker exists because no terminal was captured).
        return CollectDecision::Terminal {
            rio: RioOutcome::Built { executed: true },
            evidence: None,
        };
    }
    if status.is_success() {
        // The gateway's lost-terminal relay marker, classified BEFORE the
        // completed-without-execution discriminator: a `Substituted` row
        // for a root the gateway marked stands on a LOST evidence channel
        // — this root's terminal event never reached the relay while the
        // DAG-level word implied it was resolved (completed DAG, failed
        // keep-going DAG, or a gateway-synthesized reconnect-exhausted
        // word) and the store positively confirmed its outputs — not on
        // a recorded substitution event. The wire status stays Substituted for stock
        // clients (presence is real), so the in-band row alone is
        // indistinguishable from a genuine substitution; without the
        // marker it would classify `target-substituted`, which a
        // force-build measurement tenant makes definitionally impossible
        // — a false policy-violation record. The row is the evidence-loss
        // leg of the design's `infra-indeterminate` class (§7.2: "an
        // engine transport failure after the retry budget, or evidence
        // loss — counted against run confidence, never against the
        // target"), routed exactly like its in-band sibling
        // ([`BuildResult::lost_terminal_unverified`]): probe carve-out,
        // then the auto-retry budget (a re-attempt produces fresh
        // evidence — under force-build the root re-executes), then a
        // terminal infra row.
        //
        // TIMED batches terminalize immediately instead of consulting
        // the requeue budget: a timed member is never re-offered (the
        // dispatcher owns retries), and NO dispatcher re-attempt exists
        // for THIS row — the confirmation-retry filter selects
        // expected-built positions whose replayed result is a failure,
        // and a marked row is success-shaped (`Substituted`), while
        // armed requests skip the retry loop entirely — so a requeue
        // decision here would be converted to a record-less Defer and
        // backfilled `not-attempted` (GateAccounting::Excluded): the
        // same evidence-loss event would trip the gate timeless and
        // vanish timed. The terminal is the §7.2 evidence-loss row the
        // budget-exhausted leg mints, with the same evidence; for an
        // ARMED member, classification's step-0 timed-interruption
        // precedence (the in-band success out-raced the abandon
        // deadline) yields `interruption-not-reproduced`, agreeing with
        // the timed-stats bucket the dispatcher counted from the same
        // in-band row.
        //
        // Detector conjuncts, both producer-exact: the marker is read
        // from the RELAY capture only (`batch.lost_terminals`, the same
        // channel that captures the `rio: build <uuid>` announcement —
        // in-band `error_msg` text cannot select this arm), and the
        // status conjunct is `Substituted` exactly (the only status the
        // producer pairs the marker with; an executed `Built` returned
        // above, and `AlreadyValid`-shaped rows never carry the marker).
        if status == BuildStatus::Substituted && batch.lost_terminals.contains(&ctx.drv_path) {
            if batch.probe {
                // Same arm-entry rule as every infra-shaped exit: a
                // marked row on a canary probe is the probed outage
                // answering (the evidence channel is what is being
                // probed), budget-exempt.
                return CollectDecision::Requeue {
                    why: RequeueReason::InfraProbe,
                    budget: RequeueBudget::probe_carveout(),
                };
            }
            if batch.kind != BATCH_KIND_TIMED
                && let Some(budget) = RequeueBudget::auto_retry(prior_requeues, knobs)
            {
                return CollectDecision::Requeue {
                    why: RequeueReason::InfraAutoRetry,
                    budget,
                };
            }
            return CollectDecision::Terminal {
                rio: RioOutcome::TargetFailed {
                    kind: FailureKind::Infra,
                },
                evidence: Some(
                    "gateway lost-terminal relay marker with an in-band Substituted row \
                     (terminal never reached the relay; store presence confirmed, execution \
                     unknown): evidence loss, never a recorded substitution event"
                        .to_string(),
                ),
            };
        }
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
    // DAG-fallback blanket recovery — the trigger comes from OUTSIDE the
    // row. A root whose dependency was already terminally failed when its
    // batch merged (e.g. a fixed-output derivation still poisoned past the
    // scheduler's resubmit-reset budget) is fail-fasted at merge seeding,
    // which emits NO per-root event; the gateway then falls back to the
    // DAG-level result, so the row arrives here as the scheduler's
    // build-level summary "derivation /nix/store/<hash>-<name>.drv
    // failed" (`rio_proto::dag_first_failure_summary` over the gateway's
    // store-path DAG key) — Target-classified, no trigger reference, no
    // fetch needle — and would
    // otherwise land Genuine, charging a rotted dependency's whole
    // late-campaign fan-out to the parity headline through a door that
    // never reaches the dependency-failed cascade arm above. For exactly
    // that row shape, the batch's poison snapshot intersected with this
    // job's own dependency closure recovers the trigger CANDIDATE: a
    // poisoned fixed-output member with recorded worker failures is the
    // sticky-rot signature. Rows carrying any other text — a worker's own
    // builder error, scheduler retry/poison vocabulary, a dependency
    // shape — never take this path: their in-row evidence outranks
    // out-of-row recovery, so a still-live poison row can never excuse a
    // failure rio actually executed.
    //
    // The candidate then passes the SAME evidence bar as the in-row
    // cascade arm above — `resolve_failure_kind` over the trigger's own
    // channels (its relayed line when this batch captured one, plus the
    // trigger-keyed log tail the caller fetched) — because SourceRot has
    // ONE owner and one bar (design §6.6: "SourceRot now requires
    // is_fixed_output … plus a fetch-error needle in error_msg/log
    // tail"). The poison signature alone proves the dependency kept
    // failing on real workers, not WHY: a worker-executed fixed-output
    // failure (a hash mismatch, say) carries no fetch needle and must
    // classify Genuine through this door exactly as it does through the
    // evidenced cascade door, or one root cause classifies opposite ways
    // by batch timing. An unfetchable tail keeps the genuine charge for
    // the same reason — the in-row sibling resolves a lost tail the same
    // way (the module contract: ambiguous evidence is charged, never
    // excused).
    if signal1.is_some_and(is_dag_fallback_blanket)
        && let Some(recovered) = recover_poisoned_fod_trigger(ctx, poisoned)
    {
        let trigger_signal1 = batch.reasons.get(&recovered.trigger).map(String::as_str);
        let (kind, _) = resolve_failure_kind(
            trigger_signal1,
            poisoned.get(&recovered.trigger).map(Vec::as_slice),
            // True by the recovery's own gate; looked up rather than
            // hard-coded so the resolver sees the same derivation facts
            // the cascade arm feeds it.
            ctx.fixed_output_drvs.contains(&recovered.trigger),
            log_tail,
        );
        if kind == FailureKind::SourceRot {
            let evidence = Some(recovered.evidence());
            let rio = if recovered.trigger == ctx.drv_path {
                // The target itself is the sticky-poisoned fixed-output
                // derivation: its own upstream rotted; not a cascade.
                RioOutcome::TargetFailed {
                    kind: FailureKind::SourceRot,
                }
            } else {
                RioOutcome::DependencyFailed {
                    root: RootCauseKind::SourceRot,
                    failing_drv: recovered.trigger,
                }
            };
            return CollectDecision::Terminal { rio, evidence };
        }
        tracing::info!(
            target_drv = %ctx.drv_path,
            trigger = %recovered.trigger,
            resolved = ?kind,
            "blanket recovery candidate failed the fetch-needle evidence bar; the \
             genuine charge stands (poison proves the dependency kept failing, not that \
             its upstream rotted)"
        );
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
    //
    // ORDER PIN — classification BEFORE the probe check is correct here
    // and must stay: a with-result row proves the cluster executed the
    // build, and the probe carve-out's contract ("can only catch infra
    // shapes" — [`RequeueBudget::probe_carveout`], pinned by
    // `probe_batch_exempts_infra_shapes_but_real_verdicts_still_land`)
    // demands a probe's REAL verdicts land as evidence. The arm-entry
    // probe rule covers infra-shaped exits only; its scope here is
    // exact, not arm-wide:
    //
    // - on THIS leg (the two-signal statuses), the probe check precedes
    //   the infra auto-retry's budget consult and the infra terminal
    //   mint — the carve-out's whole jurisdiction;
    // - the DependencyFailed sub-arms ABOVE this pin consult the
    //   UNFAIR-ATTEMPT budget and, at exhaustion, mint
    //   infra-indeterminate terminals with no probe check — deliberate,
    //   not exempt-by-omission: those sub-arms ride with-result rows
    //   (the cluster answered; a probe's dependency-failed verdict is
    //   outage evidence the scorer must see), and the budget they
    //   consume is the unfair-attempt budget, which the carve-out's
    //   contract never covered.
    //
    // Do NOT hoist the probe check above classification — that would
    // turn a recovered cluster's first genuine verdict into an exempt
    // requeue and the probe ladder would never score a real recovery.
    // One row class is classified BEFORE the two-signal rule: the
    // gateway's evidence-loss row
    // ([`BuildResult::lost_terminal_unverified`]) — minted when this
    // root's terminal event was lost under a COMPLETED DAG and the store
    // could not positively confirm the requested outputs. It is an
    // infrastructure claim about the evidence channel, not a build
    // failure: the DAG completed, so nothing is known to have failed on a
    // worker — there is no failure for the two-signal rule to excuse, and
    // poison rows surviving from earlier attempts cannot contradict an
    // evidence-loss report (the design's infra-indeterminate class names
    // "evidence loss" explicitly, §7.2). Detector and producer share the
    // message-prefix constant so the match is producer-exact and cannot
    // drift; the match reads the IN-BAND `error_msg` (the producer's only
    // channel — the relay never carries this row) plus the status
    // conjunct, so no relayed scheduler line can select the arm.
    let lost_terminal_row = status == BuildStatus::TransientFailure
        && target
            .error_msg
            .starts_with(BuildResult::LOST_TERMINAL_UNVERIFIED_PREFIX);
    let (kind, evidence) = if lost_terminal_row {
        (
            FailureKind::Infra,
            Some(format!(
                "gateway evidence-loss row (terminal lost under a completed DAG, store \
                 presence unverified): {}",
                target.error_msg
            )),
        )
    } else {
        resolve_failure_kind(
            signal1,
            poisoned.get(&ctx.drv_path).map(Vec::as_slice),
            ctx.fixed_output_drvs.contains(&ctx.drv_path),
            log_tail,
        )
    };
    if kind == FailureKind::Infra {
        if batch.probe {
            return CollectDecision::Requeue {
                why: RequeueReason::InfraProbe,
                budget: RequeueBudget::probe_carveout(),
            };
        }
        // The in-band evidence-loss row of a TIMED batch terminalizes
        // immediately, like its marker sibling in the success arm above:
        // a timed member is never re-offered, so the requeue would
        // become a record-less Defer — and while THIS row is
        // failure-shaped (the confirmation retry CAN select an
        // expected-built position carrying it, unlike the success-shaped
        // marker row), that retry runs off the batch record's statuses
        // regardless of this decision, and its executed-Built success
        // supersedes the terminal through the already-terminal belt
        // (§9.2). Recording now means an unretried or armed or
        // non-built-expectation member ends as the §7.2
        // infra-indeterminate evidence-loss terminal instead of
        // vanishing into the not-attempted backfill. Ordinary
        // positively-identified infra failures in timed batches keep the
        // requeue shape: their Defer leaves the member to the
        // confirmation retry / end-of-run backfill exactly as designed —
        // only the evidence-loss rows had no completing leg.
        if !(lost_terminal_row && batch.kind == BATCH_KIND_TIMED)
            && let Some(budget) = RequeueBudget::auto_retry(prior_requeues, knobs)
        {
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
/// for successes), append terminal records, and return exactly one decision
/// per batch member.
///
/// The returned map is the function's whole contract, and it is TOTAL over
/// the batch's members — see [`CollectDecision`], which owns the variant
/// semantics. In caller terms: [`CollectDecision::Terminal`] means the
/// job's results.jsonl record has been appended (the caller retires the
/// job); [`CollectDecision::Requeue`] means the job must be re-offered to
/// the timeless pending pool (the caller counts the resubmission);
/// [`CollectDecision::Defer`] means another owner holds the resolution —
/// the timed dispatcher's retries, the end-of-run backfill, or no job
/// context to record against (the caller releases the member's watchdog
/// stall clock); [`CollectDecision::AlreadyTerminal`] is a duplicate
/// dropped by the belt below (the caller retires the job). Absence from
/// the map is not a signal — every member gets an entry.
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
    // Whether the CURRENT settled submission was itself a cluster attempt,
    // derived once from THE batch-evidence predicate
    // ([`BatchView::cluster_acknowledged`]) so no writer arm can
    // hand-tune the +1: a build id or in-band results prove the cluster
    // saw the submission; their joint absence is the engine-side shape
    // (submission failure, fully cancelled engine-cancel cycle) whose
    // requeue class `RequeueReason::counts_as_cluster_attempt` pins as
    // never-reached-the-cluster — the same judgment, asked of the current
    // event instead of journaled history. The ANNOUNCED-cancel cell sits
    // on the true side: an engine cancellation that fires after the
    // cluster acknowledged the submission leaves a build id (or per-root
    // results) behind, and that acknowledgment IS cluster contact — only
    // the fully cancelled cycle, cut before anything was announced, is
    // engine-side. The journal agrees cell-for-cell: decide()'s
    // engine-cancelled arm keys the journaled reason on the SAME
    // predicate (`EngineCancelledAnnounced`, counted, vs
    // `EngineCancelled`, not), so N announced cancels followed by a
    // settle stamp the same attempts whether they are folded from
    // history or judged as the current event. False by construction in
    // the no-result arm below; true by construction in every with-result
    // arm.
    let current_is_cluster_attempt = batch.cluster_acknowledged();
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
                        // No current cluster attempt: this arm IS the
                        // engine-side shape (false by construction —
                        // see `current_is_cluster_attempt`), so the
                        // record reports only the cluster attempts the
                        // journal measured. A job whose every submission
                        // failed at channel open stamps 0, agreeing with
                        // the stalled-queued writer for the same
                        // zero-cluster-contact truth.
                        stamped_attempts(
                            prior_attempts.get(job).copied().unwrap_or(0),
                            current_is_cluster_attempt,
                        ),
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
            // EXECUTED success results pass the belt and supersede the
            // initial failure under latest-record-per-job semantics.
            //
            // Executed means `BuildStatus::executed()` — `Built` only.
            // The presence-shaped successes (Substituted / AlreadyValid /
            // ResolvesToAlreadyValid) prove the outputs exist in the
            // store, possibly landed by a concurrent campaign, a
            // warm-tenant prefetch, or an upstream substitution — never
            // that THIS retry could build the unit (the gateway's own
            // presence-claim honesty invariant), so they cannot refute
            // the recorded failure: the design's §9.2 re-confirmation
            // contract demands the failure be contradicted by a build,
            // and an expected-built unit's outputs always exist upstream
            // by construction, so admitting presence here would let any
            // concurrent landing erase a genuine regression from the
            // gate. Presence retries are dropped like re-confirmed
            // failures — the initial verdict stands — with the
            // observation logged so operators learn the outputs appeared
            // mid-campaign. Everything else — duplicate plain
            // submissions, and retry results that merely re-confirm the
            // failure — is dropped the same way, so a duplicate can never
            // overwrite the job's real verdict.
            let retry_status = (batch.confirmation_attempt > 0)
                .then(|| target.and_then(|t| build_status_from_name(&t.status)))
                .flatten();
            let confirmation_supersede = retry_status.is_some_and(|status| status.executed());
            if !confirmation_supersede {
                if retry_status.is_some_and(|status| status.is_success()) {
                    tracing::info!(
                        job,
                        attempt = batch.confirmation_attempt,
                        status = ?retry_status,
                        "confirmation retry settled with store PRESENCE, not an executed \
                         build; the recorded failure stands (the outputs appeared \
                         mid-campaign through another channel)"
                    );
                } else {
                    tracing::info!(
                        job,
                        "settled-batch member already has a terminal record; dropping"
                    );
                }
                decisions.insert(job.clone(), CollectDecision::AlreadyTerminal);
                continue;
            }
            tracing::info!(
                job,
                attempt = batch.confirmation_attempt,
                "sanctioned confirmation retry succeeded with an executed build; superseding \
                 the terminal record"
            );
        }
        let prior = prior_budgets.get(job).copied().unwrap_or_default();
        let target_status = target.and_then(|t| build_status_from_name(&t.status));
        let target_is_failure = target.is_some() && !target_status.is_some_and(|s| s.is_success());
        // The row's own Signal-1 text, in decide()'s binding source order
        // (in-band error message first, the captured relayed line second)
        // — the channel the resolver will scan for non-dependency rows.
        let row_signal1 = target
            .map(|t| t.error_msg.as_str())
            .filter(|m| !m.is_empty())
            .or_else(|| batch.reasons.get(&ctx.drv_path).map(String::as_str));
        // Direct door — the root's OWN tail, two disjuncts:
        // - Evidence-age gate: a failed root carrying neither an in-band
        //   error message nor a relayed reason (Signal 1) and no
        //   ListPoisoned entry (Signal 2 — the scheduler's poison rows
        //   decay with its evidence TTL) fetches the tail as the third
        //   signal; the record then carries the "log-tail-only" evidence
        //   flag from [`resolve_failure_kind`].
        // - Source-rot completeness for fixed-output TARGETS: when the
        //   target itself is fixed-output and its Signal-1 text does not
        //   already satisfy the rot bar ([`channel_satisfies_rot_bar`] —
        //   content, never presence: the captured relay line is the
        //   needle-free first line of a multi-line payload, and a
        //   poison-terminalized FOD's own message is the bare threshold
        //   summary), the tail is the only channel that can carry the
        //   fetcher's output — without it the same rotted FOD classifies
        //   SourceRot through its dependents' doors but Genuine through
        //   its own row. Poison presence does not suppress this fetch
        //   (a poison row proves the FOD kept failing on workers, not
        //   WHY). Blanket-shaped rows are excluded: their evidence
        //   channel is the recovered TRIGGER's tail (third door below),
        //   which covers the target-is-trigger case.
        let needs_log_signal = target_is_failure
            && target_status != Some(BuildStatus::DependencyFailed)
            && ((target.is_some_and(|t| t.error_msg.is_empty())
                && !batch.reasons.contains_key(&ctx.drv_path)
                && !poisoned.contains_key(&ctx.drv_path))
                || (ctx.fixed_output_drvs.contains(&ctx.drv_path)
                    && !channel_satisfies_rot_bar(row_signal1)
                    && !row_signal1.is_some_and(is_dag_fallback_blanket)));
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
        // Trigger-keyed evidence for fixed-output cascade attribution: a
        // dependency-failed root whose fixed-output trigger's failure text
        // is not in band — either the trigger failed in an EARLIER batch
        // (no relayed line at all) or this batch's capture carries only
        // the needle-free shape the producers emit (the first line of a
        // multi-line payload; the bare poison summary) — fetches the
        // TRIGGER's log tail so the source-rot scan sees the fetcher's
        // own output. Without it, one rotted FOD charges its whole
        // dependent fan-out to the parity headline as rio regressions.
        // The skip test is the shared bar predicate over BOTH channels
        // the resolver scans for this shape (the trigger's relayed line
        // and the dependent's own full text) — content, never presence.
        // The fetch is keyed by the trigger and feeds only the scan (via
        // decide's log_tail channel, unused for dependency-failed rows
        // otherwise); the dependent's own evidence capture below stays
        // keyed by the dependent.
        let trigger_log_text = if target_status == Some(BuildStatus::DependencyFailed) {
            let trigger = row_signal1
                .map(classify_reason)
                .and_then(|class| match class {
                    ReasonClass::Dependency { failing_drv } => Some(failing_drv),
                    _ => None,
                });
            match trigger {
                Some(trigger)
                    if ctx.fixed_output_drvs.contains(&trigger)
                        && !channel_satisfies_rot_bar(
                            batch.reasons.get(&trigger).map(String::as_str),
                        )
                        && !channel_satisfies_rot_bar(row_signal1) =>
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
        // The same trigger-keyed fetch for the DAG-fallback blanket shape
        // (the eventless merge-seeded fail-fast): the row's own text is
        // the scheduler's needle-free build-level summary, so the only
        // channels that can satisfy the source-rot evidence bar are the
        // trigger's captured relay line (when this batch captured a
        // needled one) and the trigger's own log tail. The skip test is
        // the shared bar predicate over the trigger's relayed line —
        // content, never presence: a captured line exists for a
        // same-batch trigger, but the capture keeps only the needle-free
        // first line of a multi-line payload, so suppressing the fetch on
        // presence would make the bar unsatisfiable exactly when the rot
        // and its fan-out share a batch. The candidate is pre-computed
        // with the SAME recovery fn the decide arm uses — same closure,
        // worker-failure, and contradictory-evidence gates — so a row the
        // arm would refuse never costs a fetch, and the arm never sees a
        // tail fetched for a different candidate than it recovers.
        let blanket_trigger_log_text = if target_is_failure
            && target_status != Some(BuildStatus::DependencyFailed)
            && row_signal1.is_some_and(is_dag_fallback_blanket)
        {
            match recover_poisoned_fod_trigger(ctx, &poisoned) {
                Some(recovered)
                    if !channel_satisfies_rot_bar(
                        batch.reasons.get(&recovered.trigger).map(String::as_str),
                    ) =>
                {
                    admin
                        .log_tail(&recovered.trigger, None, knobs.log_tail_bytes)
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
            // At most one is Some: the direct fetch excludes
            // dependency-failed rows and blanket-shaped rows (a
            // blanket row's evidence channel is the recovered
            // trigger's tail, which covers the target-is-trigger
            // case), the dependency-failed trigger fetch covers only
            // that status, and the blanket fetch requires the blanket
            // shape the direct fetch excludes.
            log_signal_text
                .as_deref()
                .or(trigger_log_text.as_deref())
                .or(blanket_trigger_log_text.as_deref()),
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
                    // end-of-run backfill (no transition; its explicit
                    // `Defer` entry below only releases the watchdog
                    // stall clock). The evidence-loss rows (the gateway's
                    // lost-terminal marker and the in-band
                    // `lost_terminal_unverified` shape) never reach this
                    // arm: decide() terminalizes them directly for timed
                    // batches, because no dispatcher re-attempt completes
                    // their leg and a Defer here would convert them to
                    // gate-excluded not-attempted backfill rows.
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
                            // True by construction past the no-result
                            // gate: the batch carries cluster evidence,
                            // and the replayed interruption ran on the
                            // cluster until its recorded offset.
                            stamped_attempts(
                                prior_attempts.get(job).copied().unwrap_or(0),
                                current_is_cluster_attempt,
                            ),
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
                // ([`stamped_attempts`] over [`measured_attempt_requeues`])
                // plus this settled submission — a cluster attempt by
                // construction here (the arm is reachable only past the
                // no-result gate, so `current_is_cluster_attempt` holds).
                let attempts = if batch.confirmation_attempt > 0 {
                    batch.confirmation_attempt + 1
                } else {
                    stamped_attempts(
                        prior_attempts.get(job).copied().unwrap_or(0),
                        current_is_cluster_attempt,
                    )
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
    use crate::run::state::latest_per_job;
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

    /// [`BatchView::of_record`] carries every classification-relevant
    /// batch-record field — pinned over a record whose every field holds
    /// a distinct non-default value, so a projection that quietly drops
    /// one (the import-skip breadcrumb spent a round recorded-but-
    /// dropped exactly here) cannot pass. The constructor's exhaustive
    /// destructuring already compile-forces a DECISION for new fields;
    /// this pins the decisions made for the existing ones.
    #[test]
    fn batch_view_of_record_carries_every_classification_field() {
        let record = super::super::model::BatchRecord {
            batch_id: 11,
            kind: "timed".to_string(),
            jobs: vec!["j.x86_64-linux".into()],
            root_drvs: vec!["/nix/store/r.drv".into()],
            est_nodes: 3,
            build_id: Some("b-77".into()),
            started_at: "2026-06-01T00:00:00Z".into(),
            finished_at: Some("2026-06-01T00:10:00Z".into()),
            results: vec![po("/nix/store/r.drv", BuildStatus::Built, "")],
            reasons: BTreeMap::from([("/nix/store/r.drv".to_string(), "reason".to_string())]),
            lost_terminals: BTreeSet::from(["/nix/store/r.drv".to_string()]),
            stderr_tail: Some("tail".into()),
            engine_cancelled: true,
            disconnect_deadline_fired: true,
            interruption_drvs: vec!["/nix/store/r.drv".into()],
            import_skipped_drvs: vec!["/nix/store/m.drv".into()],
            import_skipped_by_root: BTreeMap::from([(
                "/nix/store/r.drv".to_string(),
                vec!["/nix/store/m.drv".to_string()],
            )]),
            probe: true,
            confirmation_attempt: 2,
            topup_delivered: true,
        };
        let view = BatchView::of_record(&record);
        assert_eq!(view.kind, record.kind);
        assert_eq!(view.build_id, record.build_id);
        assert_eq!(view.results.len(), 1);
        assert_eq!(view.reasons, record.reasons);
        assert_eq!(view.lost_terminals, record.lost_terminals);
        assert_eq!(view.stderr_tail, record.stderr_tail);
        assert!(view.engine_cancelled);
        assert!(view.disconnect_deadline_fired);
        assert_eq!(view.interruption_drvs, record.interruption_drvs);
        assert_eq!(
            view.submitted_at.as_deref(),
            Some(record.started_at.as_str())
        );
        assert!(view.probe);
        assert_eq!(view.confirmation_attempt, 2);
        assert_eq!(view.import_skipped_by_root, record.import_skipped_by_root);
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
            _max_bytes: u64,
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
        // Signal1 infra + contradicting target evidence → Genuine (§6.6:
        // Signal 2's recorded on-worker failures contradict the infra
        // classification).
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
        // Signal1 infra + Signal 2 decayed (None) → Infra (§6.6: "infra
        // only when Signal 1 says infra and Signal 2 does not contradict"
        // — decayed evidence is "evidence unavailable", which does not
        // contradict; agreement is NOT required).
        assert_eq!(
            resolve_failure_kind(
                Some("max_infra_retries=3 exhausted after infrastructure failures: x"),
                None,
                false,
                None
            )
            .0,
            FailureKind::Infra
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
        // for a fixed-output drv whose reason text contains a needle: a
        // corroborated-infra reason embedding rio's own "timed out" transport
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
        // terminal immediately (§6.6: recorded on-worker failures
        // contradict the infra classification).
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
        // A FIXED-OUTPUT target with the same corroborated-infra shape
        // (positive Signal 1, empty-builders poison row), whose
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

    /// Wire contract for the gateway's evidence-loss row
    /// ([`BuildResult::lost_terminal_unverified`]): the one status the
    /// gateway may mint with NEITHER per-root scheduler evidence NOR
    /// store presence evidence must classify as infrastructure evidence
    /// loss — requeue within the auto-retry budget, infra-indeterminate
    /// at exhaustion — never as a substitution event (the row replaced
    /// the old `Substituted` mint exactly because a force-build campaign
    /// makes substitution definitionally impossible) and never as a
    /// genuine target failure (the DAG completed; nothing is known to
    /// have failed).
    ///
    /// Producer-mirrored fixture: the row is CONSTRUCTED via the
    /// producer's own chain — the shared `lost_terminal_unverified()`
    /// constructor (the same fn the gateway arm calls), serialized and
    /// re-parsed through the production wire codec
    /// (`write_build_result`/`read_build_result`), then mapped through
    /// the production transport projection (`path_outcomes_from_keyed`)
    /// — never a hand-written consumer-side string.
    ///
    /// Quantification domain: the detector's two conjuncts crossed with
    /// every sibling input of the with-result arm — BatchView bits the
    /// arm reads (`probe` × `engine_cancelled`), Signal-2 states (no
    /// poison row / poison row WITH builders — the two-signal
    /// contradiction deliberately does NOT defeat an evidence-loss
    /// report), the fixed-output bit (no source-rot shadow), the budget
    /// axis (fresh / exhausted), and the batch-kind axis (a TIMED batch
    /// terminalizes the row immediately whatever the budget — a timed
    /// member is never re-offered, so the requeue shape would drain
    /// record-less into the gate-excluded not-attempted backfill).
    /// Must-NOT-match direction: same status with a different message,
    /// the needle mid-string instead of at byte 0, a different status
    /// carrying the needle, and the needle arriving only on the
    /// relayed-line channel.
    #[tokio::test]
    async fn lost_terminal_unverified_row_is_evidence_loss_not_a_substitution_or_failure()
    -> Result<()> {
        use crate::run::model::path_outcomes_from_keyed;
        use rio_nix::protocol::build::{read_build_result, write_build_result};
        use rio_nix::protocol::client::KeyedBuildResult;
        use rio_nix::protocol::handshake::PROTOCOL_VERSION;

        // ── Producer chain: constructor → wire encode → wire decode →
        // transport projection. ──
        let minted = BuildResult::lost_terminal_unverified();
        let mut buf = Vec::new();
        write_build_result(&mut buf, &minted, PROTOCOL_VERSION).await?;
        let parsed = read_build_result(&mut std::io::Cursor::new(buf), PROTOCOL_VERSION).await?;
        let keyed = KeyedBuildResult {
            derived_path: format!("{T}!*"),
            result: parsed,
        };
        let outcomes = path_outcomes_from_keyed(&[T.to_string()], std::slice::from_ref(&keyed));
        let row = &outcomes[0];
        assert_eq!(row.status, build_status_name(BuildStatus::TransientFailure));
        assert!(
            row.error_msg
                .starts_with(BuildResult::LOST_TERMINAL_UNVERIFIED_PREFIX),
            "the producer chain must deliver the detector prefix intact: {:?}",
            row.error_msg
        );

        let knobs = Knobs::default();
        let c = ctx("app.x86_64-linux", T, &[DEP], ExpectedOutcome::Built);
        let no_poison: HashMap<String, Vec<String>> = HashMap::new();
        // Stale poison rows WITH recorded builders: prior attempts failed
        // on workers before the DAG eventually completed. The two-signal
        // contradiction must NOT reclassify the evidence-loss row as
        // genuine — there is no failure being excused.
        let poisoned_with_builders: HashMap<String, Vec<String>> =
            HashMap::from([(T.to_string(), vec!["b1".to_string()])]);
        // Fixed-output target: the row must not be shadowed into source
        // rot either (the detector bypasses the needle scan entirely).
        let mut fod_ctx = ctx("app.x86_64-linux", T, &[DEP], ExpectedOutcome::Built);
        fod_ctx.fixed_output_drvs = std::sync::Arc::new([T.to_string()].into_iter().collect());

        // ── Must-admit: every (probe × engine_cancelled) cell of the
        // with-result arm, under every Signal-2/ctx state. ──
        for (probe, cancelled) in [(false, false), (false, true), (true, false), (true, true)] {
            let batch = BatchView {
                probe,
                engine_cancelled: cancelled,
                ..BatchView::default()
            };
            for (label, job_ctx, poison) in [
                ("no poison", &c, &no_poison),
                ("contradicting builders", &c, &poisoned_with_builders),
                ("fixed-output target", &fod_ctx, &no_poison),
            ] {
                let cell = format!("probe={probe} cancelled={cancelled} {label}");
                if probe {
                    // Probe batches take the budget-exempt carve-out: the
                    // evidence loss is infra-shaped, so it must not charge
                    // the conscripted unit's budget.
                    assert_eq!(
                        decide(job_ctx, Some(row), &batch, poison, prior(5), &knobs, None)
                            .requeue_why(),
                        Some("infra-probe"),
                        "{cell}"
                    );
                    continue;
                }
                // Fresh budget: one fair re-attempt (fresh evidence) instead
                // of any terminal record.
                assert_eq!(
                    decide(job_ctx, Some(row), &batch, poison, prior(0), &knobs, None)
                        .requeue_why(),
                    Some("infra-auto-retry"),
                    "{cell}"
                );
                // Exhausted budget: terminal infra (→ infra-indeterminate,
                // counted against run confidence, never against the target
                // and never as a substitution) with provenance evidence.
                match decide(job_ctx, Some(row), &batch, poison, prior(1), &knobs, None) {
                    CollectDecision::Terminal {
                        rio:
                            RioOutcome::TargetFailed {
                                kind: FailureKind::Infra,
                            },
                        evidence,
                    } => assert!(
                        evidence.unwrap().contains("gateway evidence-loss row"),
                        "{cell}: terminal evidence must name the row's provenance"
                    ),
                    other => panic!("{cell}: expected terminal infra, got {other:?}"),
                }
            }
        }

        // ── Timed batch: immediate evidence-loss terminal on BOTH sides
        // of the budget axis — no dispatcher leg completes this row for
        // a non-built expectation or an armed request, and the
        // confirmation retry that CAN fire (expected-built positions —
        // the row is failure-shaped) runs off the batch record's
        // statuses regardless of this decision and supersedes through
        // the belt with an executed Built. ──
        let timed_batch = BatchView {
            kind: BATCH_KIND_TIMED.to_string(),
            ..BatchView::default()
        };
        for prior_requeues in [0, 1] {
            match decide(
                &c,
                Some(row),
                &timed_batch,
                &no_poison,
                prior(prior_requeues),
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
                    evidence.unwrap().contains("gateway evidence-loss row"),
                    "timed prior={prior_requeues}: terminal evidence must name the row's \
                     provenance"
                ),
                other => panic!(
                    "timed prior={prior_requeues}: expected immediate terminal infra, got \
                     {other:?}"
                ),
            }
        }

        // ── Must-NOT-match: the detector is producer-exact. ──
        let batch = BatchView::default();
        // Same status, different message (the gateway's post-ack stream
        // synthesis): normal two-signal classification — genuine.
        assert!(matches!(
            decide(
                &c,
                Some(&po(
                    T,
                    BuildStatus::TransientFailure,
                    "build stream error (reconnect exhausted): transport"
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
        // Needle mid-string: the producer puts the prefix at byte 0, so a
        // message merely CONTAINING it is not the producer's row.
        assert!(matches!(
            decide(
                &c,
                Some(&po(
                    T,
                    BuildStatus::TransientFailure,
                    "context: per-root terminal lost under a completed DAG"
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
        // Different status carrying the needle: the status conjunct holds
        // (the producer only mints the row as TransientFailure).
        assert!(matches!(
            decide(
                &c,
                Some(&po(T, BuildStatus::PermanentFailure, &minted.error_msg)),
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
        // Needle only on the relayed-line channel (in-band error_msg
        // empty): the detector reads the in-band message — the producer's
        // only channel — so a relayed line cannot select the arm.
        let relayed_only = BatchView {
            reasons: BTreeMap::from([(T.to_string(), minted.error_msg.clone())]),
            ..BatchView::default()
        };
        assert!(matches!(
            decide(
                &c,
                Some(&po(T, BuildStatus::TransientFailure, "")),
                &relayed_only,
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
        Ok(())
    }

    /// Wire contract for the gateway's lost-terminal RELAY marker — the
    /// side-channel disambiguator for the confirmed-present lost-terminal
    /// cell, where the wire status stays `Substituted` (presence is real;
    /// stock clients keep seeing a plain success) and is therefore
    /// in-band-indistinguishable from a genuine substitution terminal. A
    /// `Substituted` row WITH the marker must classify as evidence loss —
    /// probe carve-out / auto-retry / terminal infra (the
    /// `infra-indeterminate` "evidence loss" leg, design §7.2) — never as
    /// `Built {executed: false}` → `target-substituted`, the recorded
    /// substitution event a force-build measurement tenant makes
    /// definitionally impossible. Without the marker the same row keeps
    /// the substitution-event leg: genuine target substitutions stay
    /// measurable.
    ///
    /// Producer-mirrored fixtures, both channels: the in-band row is
    /// CONSTRUCTED via the producer's own chain — `substituted()` (the
    /// constructor the gateway's confirmed-present arm calls), the
    /// production wire codec (`write_build_result`/`read_build_result`),
    /// the production transport projection (`path_outcomes_from_keyed`) —
    /// and the marker is constructed via the SHARED producer formatter
    /// (`lost_terminal_relay_line`, the fn the gateway emission calls)
    /// fed through the engine's own capture (`parse_stderr`, the channel
    /// that captures the `rio: build <uuid>` announcement). No
    /// hand-written consumer-side strings on either channel.
    ///
    /// Quantification domain: the detector's two conjuncts (relay marker
    /// for THIS drv × in-band `Substituted`) crossed with every sibling
    /// input of the with-result success arm — BatchView bits the arm
    /// reads (`probe` × `engine_cancelled`), Signal-2 states (no poison /
    /// stale poison rows WITH builders — successes never consult poison,
    /// and a poison row must not flip an evidence-loss report), the
    /// fixed-output bit (no source-rot shadow), and the budget axis
    /// (fresh / exhausted). The timed-batch, armed-interruption, and
    /// confirmation-belt crossings live in `process_settled_batch`
    /// (membership decisions and step-0 classification precedence, not
    /// outcome decisions) and are pinned by
    /// `lost_terminal_marker_rows_settle_as_evidence_loss_end_to_end`.
    /// Must-NOT-match: no marker (the genuine-substitution leg, asserted
    /// all the way to the `target-substituted` disposition), marker for a
    /// DIFFERENT drv, marker × executed `Built` (an own terminal is
    /// strictly stronger evidence), marker × `AlreadyValid` /
    /// `ResolvesToAlreadyValid` (the status conjunct — the producer pairs
    /// the marker with `Substituted` only), marker × failure rows (the
    /// failure path is untouched), and the marker text arriving in-band
    /// instead of on the relay channel.
    #[tokio::test]
    async fn lost_terminal_marker_substituted_row_is_evidence_loss_not_target_substituted()
    -> Result<()> {
        use crate::run::model::path_outcomes_from_keyed;
        use crate::run::stderrparse::parse_stderr;
        use rio_nix::protocol::build::{read_build_result, write_build_result};
        use rio_nix::protocol::client::KeyedBuildResult;
        use rio_nix::protocol::handshake::PROTOCOL_VERSION;

        // ── Producer chain, in-band channel: constructor → wire encode →
        // wire decode → transport projection. ──
        let minted = BuildResult::substituted();
        let mut buf = Vec::new();
        write_build_result(&mut buf, &minted, PROTOCOL_VERSION).await?;
        let parsed = read_build_result(&mut std::io::Cursor::new(buf), PROTOCOL_VERSION).await?;
        let keyed = KeyedBuildResult {
            derived_path: format!("{T}!*"),
            result: parsed,
        };
        let outcomes = path_outcomes_from_keyed(&[T.to_string()], std::slice::from_ref(&keyed));
        let row = &outcomes[0];
        assert_eq!(row.status, build_status_name(BuildStatus::Substituted));
        assert!(row.error_msg.is_empty(), "{row:?}");

        // ── Producer chain, relay channel: shared formatter → the
        // gateway's newline framing → the engine's own line-split
        // capture. ──
        let captured = parse_stderr(&format!("{}\n", BuildResult::lost_terminal_relay_line(T)));
        assert_eq!(captured.lost_terminals, BTreeSet::from([T.to_string()]));
        let marked = |probe: bool, cancelled: bool| BatchView {
            probe,
            engine_cancelled: cancelled,
            lost_terminals: captured.lost_terminals.clone(),
            ..BatchView::default()
        };

        let knobs = Knobs::default();
        let c = ctx("app.x86_64-linux", T, &[DEP], ExpectedOutcome::Built);
        let no_poison: HashMap<String, Vec<String>> = HashMap::new();
        // Stale poison rows WITH recorded builders, surviving from prior
        // attempts of the same drv: a success-status row never consults
        // poison, and the contradiction must not flip an evidence-loss
        // report either.
        let poisoned_with_builders: HashMap<String, Vec<String>> =
            HashMap::from([(T.to_string(), vec!["b1".to_string()])]);
        // Fixed-output target: no source-rot shadow on the success arm.
        let mut fod_ctx = ctx("app.x86_64-linux", T, &[DEP], ExpectedOutcome::Built);
        fod_ctx.fixed_output_drvs = std::sync::Arc::new([T.to_string()].into_iter().collect());

        // ── Must-admit: every (probe × engine_cancelled) cell, under
        // every Signal-2/ctx state. ──
        for (probe, cancelled) in [(false, false), (false, true), (true, false), (true, true)] {
            let batch = marked(probe, cancelled);
            for (label, job_ctx, poison) in [
                ("no poison", &c, &no_poison),
                ("contradicting builders", &c, &poisoned_with_builders),
                ("fixed-output target", &fod_ctx, &no_poison),
            ] {
                let cell = format!("probe={probe} cancelled={cancelled} {label}");
                if probe {
                    // Budget-exempt carve-out: the lost evidence channel
                    // is the probed outage answering.
                    assert_eq!(
                        decide(job_ctx, Some(row), &batch, poison, prior(5), &knobs, None)
                            .requeue_why(),
                        Some("infra-probe"),
                        "{cell}"
                    );
                    continue;
                }
                // Fresh budget: one fair re-attempt — fresh evidence
                // (under force-build the root re-executes).
                assert_eq!(
                    decide(job_ctx, Some(row), &batch, poison, prior(0), &knobs, None)
                        .requeue_why(),
                    Some("infra-auto-retry"),
                    "{cell}"
                );
                // Exhausted budget: terminal infra (→ infra-indeterminate,
                // counted against run confidence) with provenance evidence
                // — never a substitution event.
                match decide(job_ctx, Some(row), &batch, poison, prior(1), &knobs, None) {
                    CollectDecision::Terminal {
                        rio:
                            RioOutcome::TargetFailed {
                                kind: FailureKind::Infra,
                            },
                        evidence,
                    } => assert!(
                        evidence
                            .unwrap()
                            .contains("gateway lost-terminal relay marker"),
                        "{cell}: terminal evidence must name the row's provenance"
                    ),
                    other => panic!("{cell}: expected terminal infra, got {other:?}"),
                }
            }
        }

        // ── Must-NOT-match: the detector is producer-exact on both
        // conjuncts. ──
        // No marker: the genuine-substitution leg stands — completed
        // without execution, and the classifier still mints the
        // target-substituted disposition for a plan-absent root. This is
        // the cell that keeps REAL substitution events measurable (the
        // zero-target-substituted smoke criterion needs both directions).
        let unmarked = BatchView::default();
        match decide(&c, Some(row), &unmarked, &no_poison, prior(0), &knobs, None) {
            CollectDecision::Terminal {
                rio: rio @ RioOutcome::Built { executed: false },
                evidence: None,
            } => {
                let class = classify(&c.expected_outcome, &rio, &AuxFlags::default());
                assert_eq!(
                    class.class,
                    UnifiedClass::Disposition(Disposition::TargetSubstituted),
                    "an unmarked Substituted row must keep the substitution-event leg"
                );
            }
            other => panic!("unmarked Substituted row must stay a success: {other:?}"),
        }
        // Marker for a DIFFERENT drv: this root's row is unmarked.
        let other_marked = BatchView {
            lost_terminals: BTreeSet::from([OTHER.to_string()]),
            ..BatchView::default()
        };
        assert!(matches!(
            decide(
                &c,
                Some(row),
                &other_marked,
                &no_poison,
                prior(0),
                &knobs,
                None
            ),
            CollectDecision::Terminal {
                rio: RioOutcome::Built { executed: false },
                ..
            }
        ));
        // Marker × executed Built: the root's own success terminal is
        // strictly stronger evidence than any side-channel marker (the
        // producer never pairs the two in one batch; a stale or spoofed
        // marker must not erase a real execution).
        assert!(matches!(
            decide(
                &c,
                Some(&po(T, BuildStatus::Built, "")),
                &marked(false, false),
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
        // Marker × AlreadyValid / ResolvesToAlreadyValid: the status
        // conjunct holds — the producer pairs the marker with
        // Substituted only; the other completed-without-execution words
        // come from validity paths, not the lost-terminal mint.
        for valid_shaped in [
            BuildStatus::AlreadyValid,
            BuildStatus::ResolvesToAlreadyValid,
        ] {
            assert!(
                matches!(
                    decide(
                        &c,
                        Some(&po(T, valid_shaped, "")),
                        &marked(false, false),
                        &no_poison,
                        prior(0),
                        &knobs,
                        None
                    ),
                    CollectDecision::Terminal {
                        rio: RioOutcome::Built { executed: false },
                        ..
                    }
                ),
                "marker × {valid_shaped:?} must keep the unexecuted-success leg"
            );
        }
        // Marker × a failure row: the failure path is untouched — the
        // marker disambiguates success words only, and the in-band
        // failure (here a genuine worker message) classifies normally.
        assert!(matches!(
            decide(
                &c,
                Some(&po(
                    T,
                    BuildStatus::PermanentFailure,
                    "builder failed with exit code 2"
                )),
                &marked(false, false),
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
        // Marker text arriving IN-BAND (error_msg) instead of on the
        // relay channel: the detector reads the relay capture only — an
        // in-band string, which a worker could influence on failure
        // shapes, cannot select the arm. The row stays on the
        // substitution-event leg.
        let inband_text = BuildResult::lost_terminal_relay_line(T);
        assert!(matches!(
            decide(
                &c,
                Some(&po(T, BuildStatus::Substituted, &inband_text)),
                &BatchView::default(),
                &no_poison,
                prior(0),
                &knobs,
                None
            ),
            CollectDecision::Terminal {
                rio: RioOutcome::Built { executed: false },
                ..
            }
        ));
        Ok(())
    }

    /// The lost-terminal marker through `process_settled_batch` — the
    /// membership crossings the decide()-level contract test cannot
    /// reach, plus the recorded artifact both ways:
    ///
    /// - submit batch, budget exhausted: the record carries verdict
    ///   `infra-indeterminate` (the §7.2 evidence-loss leg) with the
    ///   provenance evidence — and NO disposition;
    /// - the unmarked control of the same row records disposition
    ///   `target-substituted` — the genuine substitution-event leg the
    ///   marker must not erase;
    /// - timed batch (marker × `kind=timed`), BOTH budget states: the
    ///   row terminalizes as the §7.2 evidence-loss record IMMEDIATELY —
    ///   no dispatcher re-attempt exists for a success-shaped row (the
    ///   confirmation-retry filter selects expected-built FAILURES), so
    ///   the former Defer drained record-less into the not-attempted
    ///   backfill (gate-excluded) while the same event tripped the gate
    ///   timeless;
    /// - armed interruption (marker × `kind=timed` ×
    ///   `interruption_drvs`), BOTH budget states: records verdict
    ///   `interruption-not-reproduced` carrying the marker arm's infra
    ///   evidence — classification's step-0 precedence answers the
    ///   armed question from the in-band settle (the build out-raced
    ///   the abandon deadline), which outranks the row's evidence
    ///   QUALITY; the timed-stats `not_reproduced` bucket counts the
    ///   same in-band row, so record and stats agree;
    /// - timed batch × ordinary positively-identified infra (the
    ///   conjunct's other side — NOT an evidence-loss row): still the
    ///   requeue shape, deferred to the dispatcher's confirmation retry
    ///   / end-of-run backfill — the timed terminalization is scoped to
    ///   the rows with no completing leg, never to infra failures whose
    ///   retry story is real;
    /// - confirmation belt (marker × `confirmation_attempt` ×
    ///   already-terminal): a marked `Substituted` retry is
    ///   presence-shaped, not an executed build, so it cannot supersede
    ///   the recorded verdict — `AlreadyTerminal`, no new record.
    #[tokio::test]
    async fn lost_terminal_marker_rows_settle_as_evidence_loss_end_to_end() {
        use crate::run::stderrparse::parse_stderr;

        let job = "app.x86_64-linux";
        let contexts: HashMap<String, JobContext> =
            [(job.to_string(), ctx(job, T, &[], ExpectedOutcome::Built))].into();
        let captured = parse_stderr(&format!("{}\n", BuildResult::lost_terminal_relay_line(T)));
        let marked_batch = |kind: &str, confirmation_attempt: u32| BatchView {
            kind: kind.to_string(),
            build_id: Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".to_string()),
            results: vec![po(T, BuildStatus::Substituted, "")],
            lost_terminals: captured.lost_terminals.clone(),
            confirmation_attempt,
            ..BatchView::default()
        };
        let exhausted: HashMap<String, PriorBudgets> = [(job.to_string(), prior(1))].into();

        // ── Submit batch, budget exhausted: the recorded unit is
        // evidence loss, never a substitution event. ──
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let decisions = process_settled_batch(
            &state,
            &LogAdmin::default(),
            &FakeStoreApi::default(),
            None,
            &contexts,
            &[job.to_string()],
            &marked_batch(BATCH_KIND_SUBMIT, 0),
            &exhausted,
            &Knobs::default(),
            "leaf",
            "c1",
            &HashMap::new(),
            &HashSet::new(),
        )
        .await
        .unwrap();
        assert!(matches!(
            decisions[job],
            CollectDecision::Terminal {
                rio: RioOutcome::TargetFailed {
                    kind: FailureKind::Infra
                },
                ..
            }
        ));
        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].verdict.as_deref(),
            Some("infra-indeterminate"),
            "the marked row's terminal is the evidence-loss leg (design §7.2), \
             counted against run confidence"
        );
        assert_eq!(records[0].disposition, None);
        assert!(
            records[0]
                .evidence
                .as_deref()
                .unwrap()
                .contains("gateway lost-terminal relay marker"),
            "{:?}",
            records[0].evidence
        );

        // ── Unmarked control: the same row without the marker records
        // the substitution-event disposition — the leg force-build smoke
        // criteria alarm on, kept fully measurable. ──
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let unmarked = BatchView {
            lost_terminals: BTreeSet::new(),
            ..marked_batch(BATCH_KIND_SUBMIT, 0)
        };
        let decisions = process_settled_batch(
            &state,
            &LogAdmin::default(),
            &FakeStoreApi::default(),
            None,
            &contexts,
            &[job.to_string()],
            &unmarked,
            &exhausted,
            &Knobs::default(),
            "leaf",
            "c1",
            &HashMap::new(),
            &HashSet::new(),
        )
        .await
        .unwrap();
        assert!(matches!(
            decisions[job],
            CollectDecision::Terminal {
                rio: RioOutcome::Built { executed: false },
                ..
            }
        ));
        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].disposition.as_deref(),
            Some("target-substituted")
        );
        assert_eq!(records[0].verdict, None);

        // ── Timed batch, FRESH budget: the marked row terminalizes as
        // the evidence-loss record immediately. The deferral premise
        // ("the timed dispatcher's confirmation retry is the designed
        // re-attempt") is structurally false for this row — the retry
        // filter selects `expected_built && !is_success` positions and a
        // marked row is success-shaped (`Substituted`), `lost_terminals`
        // is never consulted by the dispatcher, and armed requests skip
        // the retry loop — so a Defer here drained record-less into the
        // not-attempted backfill (GateAccounting::Excluded): the same
        // evidence-loss event tripped the gate timeless and vanished
        // timed. ──
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let decisions = process_settled_batch(
            &state,
            &LogAdmin::default(),
            &FakeStoreApi::default(),
            None,
            &contexts,
            &[job.to_string()],
            &marked_batch(BATCH_KIND_TIMED, 0),
            &HashMap::new(),
            &Knobs::default(),
            "timed",
            "c1",
            &HashMap::new(),
            &HashSet::new(),
        )
        .await
        .unwrap();
        assert!(matches!(
            decisions[job],
            CollectDecision::Terminal {
                rio: RioOutcome::TargetFailed {
                    kind: FailureKind::Infra
                },
                ..
            }
        ));
        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].verdict.as_deref(),
            Some("infra-indeterminate"),
            "the timed marked row is the same §7.2 evidence-loss terminal as the \
             timeless exhausted leg — never a record-less Defer"
        );
        assert!(
            records[0]
                .evidence
                .as_deref()
                .unwrap()
                .contains("gateway lost-terminal relay marker"),
            "{:?}",
            records[0].evidence
        );

        // ── Timed batch × ordinary positively-identified infra: the
        // conjunct's other side. A NON-evidence-loss infra row (the
        // scheduler's infra-retries-exhausted vocabulary) keeps the
        // requeue shape and is deferred to the timed dispatcher /
        // backfill — the timed terminalization is scoped to evidence
        // loss, not to infra failures whose confirmation-retry story is
        // real (the row is failure-shaped, so the retry filter selects
        // it). ──
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let infra_batch = BatchView {
            kind: BATCH_KIND_TIMED.to_string(),
            build_id: Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".to_string()),
            results: vec![po(
                T,
                BuildStatus::TransientFailure,
                "max_infra_retries=3 exhausted after infrastructure failures: store unavailable",
            )],
            ..BatchView::default()
        };
        let decisions = process_settled_batch(
            &state,
            &LogAdmin::default(),
            &FakeStoreApi::default(),
            None,
            &contexts,
            &[job.to_string()],
            &infra_batch,
            &HashMap::new(),
            &Knobs::default(),
            "timed",
            "c1",
            &HashMap::new(),
            &HashSet::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            decisions[job],
            CollectDecision::Defer {
                reason: "infra-auto-retry"
            },
            "ordinary timed infra failures keep the deferred requeue shape"
        );
        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert!(records.is_empty(), "{records:?}");

        // ── Armed interruption × marker, fresh budget: terminalizes like
        // the unarmed timed cell, and classification's step-0
        // timed-interruption precedence answers the armed question from
        // the in-band settle — `timed_interruption_for(batch, drv,
        // Some(true))` derives `NotReproduced` (armed, deadline did not
        // fire, the submission settled in band success-shaped), which
        // outranks the row's evidence QUALITY. The record agrees with
        // the timed-stats `not_reproduced` bucket, which the dispatcher
        // counts from the same in-band row — under the former Defer the
        // stats said `not_reproduced` while the record said
        // not-attempted. ──
        let armed = |confirmation_attempt: u32| {
            let mut batch = marked_batch(BATCH_KIND_TIMED, confirmation_attempt);
            batch.interruption_drvs = vec![T.to_string()];
            batch
        };
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let decisions = process_settled_batch(
            &state,
            &LogAdmin::default(),
            &FakeStoreApi::default(),
            None,
            &contexts,
            &[job.to_string()],
            &armed(0),
            &HashMap::new(),
            &Knobs::default(),
            "timed",
            "c1",
            &HashMap::new(),
            &HashSet::new(),
        )
        .await
        .unwrap();
        assert!(
            matches!(
                decisions[job],
                CollectDecision::Terminal {
                    rio: RioOutcome::TargetFailed {
                        kind: FailureKind::Infra
                    },
                    ..
                }
            ),
            "{:?}",
            decisions[job]
        );
        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].verdict.as_deref(),
            Some("interruption-not-reproduced"),
            "step-0 precedence answers the armed cell from the in-band settle"
        );
        assert!(
            records[0]
                .evidence
                .as_deref()
                .unwrap()
                .contains("gateway lost-terminal relay marker"),
            "the marker arm's provenance evidence must ride the interruption \
             verdict: {:?}",
            records[0].evidence
        );

        // ── Armed interruption × marker, budget exhausted: the marker
        // arm terminalizes TargetFailed{Infra} with its provenance
        // evidence, and classification then applies step-0 precedence —
        // `classify` consults `AuxFlags::timed_interruption` before any
        // other rule, and `timed_interruption_for(batch, drv,
        // in_band_success)` derives `Some(NotReproduced)` from the RAW
        // success-shaped in-band row (armed, deadline did not fire, the
        // submission settled in band). The verdict is therefore
        // interruption-not-reproduced, NOT infra-indeterminate.
        //
        // PINNED DELIBERATELY: the armed-interruption question — "did
        // the recorded interruption replay?" — is answered by the
        // in-band settle itself (the build out-raced the abandon
        // deadline; nothing about the recording was reproduced), and
        // that observation outranks the row's evidence QUALITY, which is
        // what the marker degrades. The marker arm's infra evidence
        // still rides the record, so the lost channel stays auditable
        // under the interruption verdict. Re-routing this cell to
        // infra-indeterminate would be a behavior change to the timed
        // fidelity vocabulary and needs its own justification — this
        // assertion is where that decision would surface. ──
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let decisions = process_settled_batch(
            &state,
            &LogAdmin::default(),
            &FakeStoreApi::default(),
            None,
            &contexts,
            &[job.to_string()],
            &armed(0),
            &exhausted,
            &Knobs::default(),
            "timed",
            "c1",
            &HashMap::new(),
            &HashSet::new(),
        )
        .await
        .unwrap();
        assert!(
            matches!(
                decisions[job],
                CollectDecision::Terminal {
                    rio: RioOutcome::TargetFailed {
                        kind: FailureKind::Infra
                    },
                    ..
                }
            ),
            "{:?}",
            decisions[job]
        );
        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].verdict.as_deref(),
            Some("interruption-not-reproduced"),
            "classify step-0 precedence: the timed-interruption flag outranks the \
             marker arm's infra rio outcome"
        );
        assert_eq!(records[0].disposition, None);
        assert!(
            records[0]
                .evidence
                .as_deref()
                .unwrap()
                .contains("gateway lost-terminal relay marker"),
            "the marker arm's provenance evidence must ride the interruption \
             verdict: {:?}",
            records[0].evidence
        );

        // ── Confirmation belt: a marked Substituted retry is
        // presence-shaped — it cannot supersede the recorded verdict
        // (only an EXECUTED Built may), and the belt drops it before the
        // evidence-loss arm could mint anything. ──
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let decisions = process_settled_batch(
            &state,
            &LogAdmin::default(),
            &FakeStoreApi::default(),
            None,
            &contexts,
            &[job.to_string()],
            &marked_batch(BATCH_KIND_TIMED, 1),
            &HashMap::new(),
            &Knobs::default(),
            "timed",
            "c1",
            &HashMap::new(),
            &HashSet::from([job.to_string()]),
        )
        .await
        .unwrap();
        assert_eq!(decisions[job], CollectDecision::AlreadyTerminal);
        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert!(records.is_empty(), "{records:?}");
    }

    /// The canary-probe carve-out, both directions. Must-exempt: a probe
    /// batch's infra-shaped failures (missing in-band result, two-signal
    /// infra, a marked lost-terminal row — including in a TIMED probe
    /// batch, where the carve-out outranks the timed-mode immediate
    /// terminalization) re-offer with the budget-exempt witness even with
    /// the auto-retry budget long exhausted — a probe failure is evidence
    /// about the outage, never charged to the job. Must-still-classify: a
    /// probe whose build actually executed (genuine failure, success)
    /// produces its normal terminal decision — the probe carve-out can
    /// only catch infra shapes, so a recovered cluster's verdicts land as
    /// evidence.
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

        // Marked lost-terminal row (relay marker × Substituted) in a batch
        // that is BOTH timed AND a probe. Producer-impossible today — the
        // timed dispatcher never sets `probe` on its submissions — pinned
        // anyway per the precedence discipline: the marker arm consults
        // the probe carve-out BEFORE the timed-mode immediate
        // terminalization, so the probed outage answers budget-exempt
        // instead of minting the §7.2 evidence-loss terminal.
        let marked_timed_probe = BatchView {
            probe: true,
            kind: BATCH_KIND_TIMED.to_string(),
            lost_terminals: BTreeSet::from([T.to_string()]),
            ..BatchView::default()
        };
        match decide(
            &c,
            Some(&po(T, BuildStatus::Substituted, "")),
            &marked_timed_probe,
            &no_poison,
            prior(5),
            &knobs,
            None,
        ) {
            CollectDecision::Requeue { why, budget } => {
                assert_eq!(why, RequeueReason::InfraProbe);
                assert!(
                    budget.probe_exempt(),
                    "probe wins over timed for a marked row"
                );
            }
            other => panic!(
                "expected the probe carve-out to outrank the timed terminalization, got {other:?}"
            ),
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

    /// Standing enumeration of every wire-success predicate consumer in
    /// the run module's production code — the audit that decayed when the
    /// belt re-conflated the success grades is now a test instead of a
    /// one-time sweep. Quantification domain: `.is_success()` /
    /// `.executed()` call sites in `run/**/*.rs` production regions (each
    /// file truncated at its first `#[cfg(test)]`; files outside `run/`
    /// use HTTP-status `is_success`, a different type).
    ///
    /// The law: `BuildStatus::is_success()` is completion-shaped and may
    /// only feed POLARITY questions (failure detection, retry-loop stops,
    /// the pre-decide evidence-fetch gates) or the decide() chokepoint
    /// that translates wire status into the graded evidence vocabulary
    /// (`RioOutcome::Built { executed }`). Any consumer whose decision
    /// needs proof a build EXECUTED — today exactly one, the belt's
    /// confirmation supersede — must use `BuildStatus::executed()`. A new
    /// call site fails these counts until it is enumerated here with its
    /// question decided.
    #[test]
    fn wire_success_consumers_are_enumerated_by_question() {
        // (file, expected `.is_success()` sites, expected `.executed()`
        // sites, the questions they answer).
        let expected: &[(&str, usize, usize, &str)] = &[
            (
                "collect.rs",
                5,
                1,
                "is_success: the decide() executed/presence chokepoint, the interruption \
                 rule's in-band-success input, the poison-fetch gate, the belt's \
                 presence-drop log arm, and the log-signal failure gate (all polarity or \
                 pre-decide); executed: the belt's confirmation supersede — the one \
                 evidence-graded gate",
            ),
            (
                "mod.rs",
                2,
                0,
                "polarity only: import_gap_retirements consumes failure-shaped in-band \
                 results (a root that succeeded is judged on its success, never retired \
                 over an archive gap), and the batch-settle supply rollup's in-band \
                 success exemption (in-row evidence outranks journal inference) — both \
                 pre-classification gates, neither asserts execution",
            ),
            (
                "timeline.rs",
                2,
                0,
                "polarity only: the confirmation retry loop stops on any success status \
                 (the belt upstream decides what may supersede), and in_band_success \
                 feeds the interruption rule",
            ),
            (
                "supply/exec.rs",
                1,
                0,
                "polarity only: a prefetch submission's success status",
            ),
        ];
        // Runtime resolution (crate::test_manifest_dir): the compile-time
        // env! path does not exist inside the nextest sandbox.
        let run_root = crate::test_manifest_dir().join("src/run");
        // Built at runtime so this test's own source cannot match.
        let success_needle = format!(".{}{}", "is_success", "()");
        let executed_needle = format!(".{}{}", "executed", "()");
        let mut found: std::collections::BTreeMap<String, (usize, usize)> = Default::default();
        let mut stack = vec![run_root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|ext| ext != "rs") {
                    continue;
                }
                let src = std::fs::read_to_string(&path).unwrap();
                let prod = src
                    .split("#[cfg(test)]")
                    .next()
                    .expect("split always yields at least one piece");
                let success = prod.matches(success_needle.as_str()).count();
                let executed = prod.matches(executed_needle.as_str()).count();
                if success > 0 || executed > 0 {
                    let rel = path
                        .strip_prefix(&run_root)
                        .unwrap()
                        .display()
                        .to_string()
                        .replace('\\', "/");
                    found.insert(rel, (success, executed));
                }
            }
        }
        let expected_map: std::collections::BTreeMap<String, (usize, usize)> = expected
            .iter()
            .map(|(file, success, executed, _q)| ((*file).to_string(), (*success, *executed)))
            .collect();
        assert_eq!(
            found, expected_map,
            "wire-success consumers changed: every `.is_success()` / `.executed()` call \
             site in run/ production code must be enumerated here with its question \
             (polarity / chokepoint / evidence-graded) decided"
        );
    }

    /// The probe bit crossed with EVERY sibling field of [`BatchView`] —
    /// not just the failure shapes a script happens to produce.
    /// Quantification domain: `BatchView`'s full field list, enumerated
    /// as data below and coupled to the struct by an exhaustive
    /// destructuring, so adding a field refuses to compile until its
    /// probe-cross row is decided here.
    ///
    /// The invariant under test is decide()'s arm-entry rule: in the
    /// no-in-band-result arm, NO sibling state may route a probe member
    /// into a budget charge or a terminal mint — the probe exit dominates
    /// the arm. The load-bearing cell is probe × engine_cancelled with
    /// the cancel-cycle budget exhausted: the engine maps a hung probe's
    /// deadline cut to (empty results, engine_cancelled: true)
    /// (`submitter.rs`: Timeout → "collect re-offers the members via the
    /// engine-cancelled rule"), and before the arm-entry rule that cell
    /// charged Counted cycles and, at exhaustion, minted a spurious
    /// regression-tripping infra terminal on the conscripted unit — which
    /// the probe scorer then read as SUCCESS, defeating the operator
    /// escalation. Every other sibling row pins that no future bit can
    /// re-open the same hole.
    #[test]
    fn probe_carveout_dominates_every_sibling_batch_bit_in_the_no_result_arm() {
        let knobs = Knobs::default();
        let c = ctx("app.x86_64-linux", T, &[], ExpectedOutcome::Built);
        let no_poison: HashMap<String, Vec<String>> = HashMap::new();
        // Couple the row list to the struct: a new BatchView field fails
        // this destructuring until its probe-cross row below is decided.
        let BatchView {
            kind: _,
            build_id: _,
            results: _,
            reasons: _,
            lost_terminals: _,
            stderr_tail: _,
            engine_cancelled: _,
            disconnect_deadline_fired: _,
            interruption_drvs: _,
            submitted_at: _,
            probe: _,
            confirmation_attempt: _,
            import_skipped_by_root: _,
        } = BatchView::default();
        let probe_with = |mutate: fn(&mut BatchView)| {
            let mut batch = BatchView {
                probe: true,
                ..BatchView::default()
            };
            mutate(&mut batch);
            batch
        };
        let rows: Vec<(&str, BatchView)> = vec![
            (
                "kind=timed",
                probe_with(|b| b.kind = BATCH_KIND_TIMED.to_string()),
            ),
            // A build id alone (no per-root results) is still the
            // no-result shape for this member.
            (
                "build_id=Some",
                probe_with(|b| b.build_id = Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".into())),
            ),
            // A result for ANOTHER root: this member still has none.
            (
                "results=other-root",
                probe_with(|b| b.results = vec![po(OTHER, BuildStatus::Built, "")]),
            ),
            (
                "reasons=non-empty",
                probe_with(|b| {
                    b.reasons =
                        BTreeMap::from([(T.to_string(), "spurious relayed line".to_string())]);
                }),
            ),
            (
                "stderr_tail=Some",
                probe_with(|b| b.stderr_tail = Some("engine submission error: x".into())),
            ),
            // THE bug cell: hung probe cut by the engine batch deadline.
            (
                "engine_cancelled",
                probe_with(|b| b.engine_cancelled = true),
            ),
            (
                "disconnect_deadline_fired",
                probe_with(|b| {
                    b.engine_cancelled = true;
                    b.disconnect_deadline_fired = true;
                }),
            ),
            (
                "interruption_drvs=non-empty",
                probe_with(|b| b.interruption_drvs = vec![T.to_string()]),
            ),
            (
                "submitted_at=Some",
                probe_with(|b| b.submitted_at = Some("2026-06-01T00:00:00Z".into())),
            ),
            ("probe (base)", probe_with(|_| {})),
            (
                "confirmation_attempt>0",
                probe_with(|b| b.confirmation_attempt = 1),
            ),
            // An import-gap entry for the root: the gap retirement
            // (`import_gap_retirements`) consumes only failure-shaped
            // IN-BAND results, so in this arm's no-result shape the
            // breadcrumb is inert and the carve-out still dominates —
            // the starved canary settles through the gap retirer on a
            // batch that produced a result, never through a budget
            // charge here.
            (
                "import_skipped_by_root=non-empty",
                probe_with(|b| {
                    b.import_skipped_by_root = BTreeMap::from([(
                        T.to_string(),
                        vec!["/nix/store/dddddddddddddddddddddddddddddddd-gap.drv".to_string()],
                    )]);
                }),
            ),
            // A lost-terminal relay marker for the root: its detector
            // conjunct requires an IN-BAND Substituted row, so in this
            // arm's no-result shape the marker is inert and the
            // carve-out still dominates — a marker whose row was lost
            // too settles through the no-result rule on a non-probe
            // batch, never through a budget charge here.
            (
                "lost_terminals=this-root",
                probe_with(|b| b.lost_terminals = BTreeSet::from([T.to_string()])),
            ),
        ];
        // Budgets fully exhausted on BOTH counters: any arm that consults
        // a budget instead of the carve-out terminalizes, failing the row.
        let spent = PriorBudgets {
            requeues: 5,
            cancel_cycles: knobs.max_engine_cancel_cycles,
        };
        for (name, batch) in &rows {
            assert!(batch.probe, "{name}: every row crosses WITH the probe bit");
            match decide(&c, None, batch, &no_poison, spent, &knobs, None) {
                CollectDecision::Requeue { why, budget } => {
                    assert_eq!(why, RequeueReason::InfraProbe, "{name}");
                    assert!(
                        budget.probe_exempt(),
                        "{name}: the carve-out's witness must carry the exemption"
                    );
                }
                other => panic!(
                    "[probe × {name}] no sibling bit may charge a budget or mint a terminal \
                     on a probe with no in-band result; got {other:?}"
                ),
            }
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

    /// SAME-batch cascade attribution — one row past the cross-batch
    /// sibling above: the rotted fixed-output trigger terminalized in
    /// THIS batch, so `batch.reasons` carries a captured line for it.
    /// Production's captured shape for the poison-threshold
    /// terminalization is the bare summary (`poison_and_cascade` emits
    /// "poison threshold reached after N distinct-worker failures" as
    /// the trigger's WHOLE reason and the cascade wrap — the engine's
    /// own `scheduler_reason_corpus` row), needle-free by construction —
    /// and the dependent's own message wraps the same needle-free
    /// summary. Line PRESENCE for the trigger must therefore not
    /// suppress the trigger-keyed tail fetch: the tail is the only
    /// channel that can satisfy the source-rot bar for this row, and
    /// without the fetch the same rot classifies Genuine same-batch but
    /// SourceRot cross-batch — the batch-timing flip §6.6 pins away.
    /// The captured channel is built by the engine's own line-split
    /// capture over the gateway relay shape, never hand-planted.
    #[tokio::test]
    async fn same_batch_poison_cascade_fetches_trigger_log_despite_captured_line() {
        use crate::run::stderrparse::parse_stderr;
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();

        let mut app = ctx("app.x86_64-linux", T, &[DEP], ExpectedOutcome::Built);
        app.fixed_output_drvs = std::sync::Arc::new([DEP.to_string()].into_iter().collect());
        let admin = LogAdmin {
            tail: b"trying https://example.com/dep-1.0.tar.gz\n\
                    curl: (22) The requested URL returned error: 404\n"
                .to_vec(),
            poisoned: vec![PoisonedView {
                drv_path: DEP.to_string(),
                failed_executors: vec!["b1".into()],
                poisoned_secs_ago: 60,
            }],
            ..LogAdmin::default()
        };
        let store = FakeStoreApi::default();
        let contexts: HashMap<String, JobContext> = [("app.x86_64-linux".to_string(), app)].into();
        // Producer-verbatim capture: the gateway relays the trigger's
        // bare poison summary and the dependent's cascade wrap; the
        // engine's own line-split capture yields both reasons.
        let poison_summary = "poison threshold reached after 3 distinct-worker failures";
        let dep_msg = format!("dependency '{DEP}' failed: {poison_summary}");
        let relay = format!(
            "derivation '{DEP}' failed: {poison_summary}\n\
             derivation '{T}' failed: {dep_msg}\n"
        );
        let parsed = parse_stderr(&relay);
        assert_eq!(
            parsed.reasons[DEP], poison_summary,
            "producer parity: the trigger's captured line is the bare summary"
        );
        assert!(
            !fetch_signature_present(&parsed.reasons[DEP]),
            "producer parity: the bare poison summary must be needle-free, or this test \
             no longer exercises the suppressed-fetch cell"
        );
        let batch = BatchView {
            kind: BATCH_KIND_SUBMIT.to_string(),
            build_id: Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".to_string()),
            results: vec![po(T, BuildStatus::DependencyFailed, &dep_msg)],
            reasons: parsed.reasons,
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
            "the trigger's tail evidence must drive source-rot attribution even though a \
             (needle-free) captured line exists for the trigger"
        );
        assert!(
            admin.log_drvs.lock().unwrap().iter().any(|d| d == DEP),
            "the trigger-keyed scan fetch must fire despite the captured line: {:?}",
            admin.log_drvs.lock().unwrap()
        );
        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(records[0].verdict.as_deref(), Some("source-unavailable"));
        assert!(records[0].cascaded);
    }

    /// The other side of the dependency door's content conjunct: a
    /// captured trigger line that ALREADY satisfies the source-rot bar
    /// (the single-line worker shape "builder failed: unable to
    /// download …" — the corpus's must-admit control) skips the
    /// trigger-keyed fetch entirely and still classifies SourceRot from
    /// the in-band channel. Pins that the door is needle-AWARE, not
    /// fetch-always: the skip is the cheap path, and it must not excuse
    /// less than the fetch would.
    #[tokio::test]
    async fn needled_trigger_line_skips_the_fetch_and_satisfies_the_bar_in_band() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();

        let mut app = ctx("app.x86_64-linux", T, &[DEP], ExpectedOutcome::Built);
        app.fixed_output_drvs = std::sync::Arc::new([DEP.to_string()].into_iter().collect());
        // A needle-free tail: if the door (incorrectly) fetched and the
        // classification leaned on it, the bar would fail and the cell
        // would charge Genuine — so the SourceRot assertion below also
        // proves the in-band needle did the work.
        let admin = LogAdmin {
            tail: b"gcc: fatal error\n".to_vec(),
            poisoned: vec![PoisonedView {
                drv_path: DEP.to_string(),
                failed_executors: vec!["b1".into()],
                poisoned_secs_ago: 60,
            }],
            ..LogAdmin::default()
        };
        let store = FakeStoreApi::default();
        let contexts: HashMap<String, JobContext> = [("app.x86_64-linux".to_string(), app)].into();
        // The corpus's needled single-line worker shape, captured as the
        // trigger's relayed reason.
        let needled = "builder failed: unable to download 'https://example.com/src.tar.gz'";
        let dep_msg = format!("dependency '{DEP}' failed: {needled}");
        let batch = BatchView {
            kind: BATCH_KIND_SUBMIT.to_string(),
            build_id: Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".to_string()),
            results: vec![po(T, BuildStatus::DependencyFailed, &dep_msg)],
            reasons: BTreeMap::from([
                (DEP.to_string(), needled.to_string()),
                (T.to_string(), dep_msg.clone()),
            ]),
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
        );
        assert!(
            !admin.log_drvs.lock().unwrap().iter().any(|d| d == DEP),
            "a trigger line that already satisfies the bar must skip the trigger-keyed \
             fetch: {:?}",
            admin.log_drvs.lock().unwrap()
        );
    }

    /// The blanket door's captured-relay axis, both sides. The
    /// round-4 blanket fixtures all fixed `reasons` at empty (the
    /// cross-batch shape); these are the same-batch cells: the recovery
    /// candidate's failure was captured by THIS batch's relay, so a line
    /// exists for the trigger.
    ///
    /// - Needle-free captured line (the engine's own line-split capture
    ///   keeps the first line of the trigger's multi-line payload): the
    ///   fetch must still fire — the captured line cannot satisfy the
    ///   bar it was being allowed to suppress.
    /// - Needled captured line (single-line download-failure shape): the
    ///   fetch is skipped and the bar is satisfied in band.
    #[tokio::test]
    async fn blanket_recovery_fetches_trigger_despite_captured_relay_line() {
        use crate::run::stderrparse::parse_stderr;
        for (case, relay_payload, expect_fetch) in [
            (
                "needle-free first-line capture",
                format!(
                    "derivation '{DEP}' failed: builder for '{DEP}' failed with exit code 1;\n\
                     last 10 log lines:\n\
                     > trying https://example.com/dep-1.0.tar.gz\n\
                     > curl: (22) The requested URL returned error: 404\n\
                     For full logs, run 'nix log {DEP}'.\n"
                ),
                true,
            ),
            (
                "needled single-line capture",
                format!(
                    "derivation '{DEP}' failed: builder failed: unable to download \
                     'https://example.com/src.tar.gz'\n"
                ),
                false,
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let state = StateDir::new(dir.path()).unwrap();
            let mut app = ctx("app.x86_64-linux", T, &[DEP], ExpectedOutcome::Built);
            app.fixed_output_drvs = std::sync::Arc::new([DEP.to_string()].into_iter().collect());
            let admin = LogAdmin {
                poisoned: vec![PoisonedView {
                    drv_path: DEP.to_string(),
                    failed_executors: vec!["b1".into(), "b2".into()],
                    poisoned_secs_ago: 7200,
                }],
                tail: b"trying https://example.com/dep-1.0.tar.gz\n\
                        curl: (22) The requested URL returned error: 404\n"
                    .to_vec(),
                ..LogAdmin::default()
            };
            let store = FakeStoreApi::default();
            let contexts: HashMap<String, JobContext> =
                [("app.x86_64-linux".to_string(), app)].into();
            // The dependent's own row stays the producer-verbatim DAG
            // blanket (the eventless fail-fast shape).
            let blanket = rio_proto::dag_first_failure_summary(DEP);
            let parsed = parse_stderr(&relay_payload);
            assert_eq!(
                fetch_signature_present(&parsed.reasons[DEP]),
                !expect_fetch,
                "[{case}] producer parity: the captured line's needle content must match \
                 the cell"
            );
            let batch = BatchView {
                kind: BATCH_KIND_SUBMIT.to_string(),
                build_id: Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".to_string()),
                results: vec![po(T, BuildStatus::MiscFailure, &blanket)],
                reasons: parsed.reasons,
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
            match &decisions["app.x86_64-linux"] {
                CollectDecision::Terminal {
                    rio: RioOutcome::DependencyFailed { root, failing_drv },
                    ..
                } => {
                    assert_eq!(*root, RootCauseKind::SourceRot, "[{case}]");
                    assert_eq!(failing_drv, DEP, "[{case}]");
                }
                other => panic!("[{case}] expected a recovered source-rot cascade, got {other:?}"),
            }
            assert_eq!(
                admin.log_drvs.lock().unwrap().iter().any(|d| d == DEP),
                expect_fetch,
                "[{case}] trigger-keyed fetch fired-ness must match the captured line's \
                 bar content: {:?}",
                admin.log_drvs.lock().unwrap()
            );
        }
    }

    /// The direct door's fixed-output completeness arm, both sides of
    /// the content conjunct, for the TARGET's own row (the trigger
    /// itself in some batch — e.g. its own poison-threshold
    /// terminalization, whose in-band message is the bare needle-free
    /// summary):
    ///
    /// - Needle-free Signal-1 (the bare poison summary): the root's own
    ///   tail must be fetched — the only channel that can carry the
    ///   fetcher's output — and the row classifies
    ///   `TargetFailed{SourceRot}`. A live poison row must NOT suppress
    ///   the fetch (the old evidence-age gate's poison conjunct): the
    ///   poison row proves the FOD kept failing on workers, not WHY.
    /// - Needled Signal-1 (the single-line download-failure shape) with
    ///   a needle-FREE tail: classifies SourceRot from the in-band
    ///   channel alone — proof the bar is satisfiable without the fetch
    ///   on this side, so the door's skip excuses nothing.
    #[tokio::test]
    async fn fixed_output_target_needle_bar_reaches_its_own_tail() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let mut app = ctx("app.x86_64-linux", T, &[], ExpectedOutcome::Built);
        app.fixed_output_drvs = std::sync::Arc::new([T.to_string()].into_iter().collect());
        let admin = LogAdmin {
            tail: b"trying https://example.com/app-1.0.tar.gz\n\
                    curl: (22) The requested URL returned error: 404\n"
                .to_vec(),
            poisoned: vec![PoisonedView {
                drv_path: T.to_string(),
                failed_executors: vec!["b1".into(), "b2".into()],
                poisoned_secs_ago: 60,
            }],
            ..LogAdmin::default()
        };
        let store = FakeStoreApi::default();
        let contexts: HashMap<String, JobContext> =
            [("app.x86_64-linux".to_string(), app.clone())].into();
        let batch = BatchView {
            kind: BATCH_KIND_SUBMIT.to_string(),
            build_id: Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".to_string()),
            results: vec![po(
                T,
                BuildStatus::TransientFailure,
                "poison threshold reached after 3 distinct-worker failures",
            )],
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
        assert!(
            matches!(
                decisions["app.x86_64-linux"],
                CollectDecision::Terminal {
                    rio: RioOutcome::TargetFailed {
                        kind: FailureKind::SourceRot
                    },
                    ..
                }
            ),
            "the FOD target's own needle-free row must reach its tail: {:?}",
            decisions["app.x86_64-linux"]
        );
        assert!(
            admin.log_drvs.lock().unwrap().iter().any(|d| d == T),
            "{:?}",
            admin.log_drvs.lock().unwrap()
        );
        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(records[0].verdict.as_deref(), Some("source-unavailable"));

        // Needled Signal-1, needle-free tail: in-band satisfies the bar.
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let admin = LogAdmin {
            tail: b"gcc: fatal error\n".to_vec(),
            poisoned: vec![PoisonedView {
                drv_path: T.to_string(),
                failed_executors: vec!["b1".into()],
                poisoned_secs_ago: 60,
            }],
            ..LogAdmin::default()
        };
        let contexts: HashMap<String, JobContext> = [("app.x86_64-linux".to_string(), app)].into();
        let batch = BatchView {
            kind: BATCH_KIND_SUBMIT.to_string(),
            build_id: Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".to_string()),
            results: vec![po(
                T,
                BuildStatus::TransientFailure,
                "builder failed: unable to download 'https://example.com/app-1.0.tar.gz'",
            )],
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
        assert!(
            matches!(
                decisions["app.x86_64-linux"],
                CollectDecision::Terminal {
                    rio: RioOutcome::TargetFailed {
                        kind: FailureKind::SourceRot
                    },
                    ..
                }
            ),
            "a needled in-band channel satisfies the bar with no tail dependence \
             (the scripted tail is needle-free): {:?}",
            decisions["app.x86_64-linux"]
        );
    }

    /// The DAG-fallback blanket detector admits exactly the scheduler's
    /// build-level first-failure summary and nothing else from the
    /// failure vocabulary.
    ///
    /// Producer contract, BOTH halves: the FORMAT is
    /// `rio_proto::dag_first_failure_summary` (the one production
    /// formatter — `rio-scheduler/src/actor/completion.rs`
    /// `handle_derivation_failure` records `error_summary` through it,
    /// captured end-to-end by the scheduler's
    /// `test_build_failed_summary_interpolates_the_full_drv_store_path`),
    /// and the CONTENT of the interpolated key is the FULL drv store path
    /// (`build_node` in rio-gateway/src/translate.rs mints `drv_hash =
    /// drv_path`, content-pinned by
    /// `build_node_drv_hash_is_the_full_store_path`). The gateway's DAG
    /// fallback relays the summary verbatim as the per-root result for
    /// roots without their own terminal
    /// (`rio-gateway/src/handler/build.rs` `root_evidence` /
    /// `per_root_verdict`).
    ///
    /// The positive fixture is therefore CONSTRUCTED VIA the producer's
    /// own format chain — the shared cross-crate formatter applied to a
    /// store path rio-nix validates as parseable — never a hand-written
    /// consumer-side string. The previous revision of this test built its
    /// positive fixture by stripping `/nix/store/` off the path (a
    /// transformation no producer performs) and pinned the real
    /// production string as must-NOT-match: detector and test shared the
    /// same wrong belief about the producer, and CI certified a recovery
    /// arm that could never fire. Fixture-from-producer-chain is the rule
    /// that makes that shape impossible.
    #[test]
    fn dag_fallback_blanket_detector_is_producer_exact() {
        use crate::run::stderrparse::scheduler_reason_corpus;
        // Producer-typed value: the gateway interpolates a parsed-valid
        // drv store path (translate.rs operates on validated paths).
        let _ = rio_nix::store_path::StorePath::parse(DEP)
            .expect("the fixture's drv path must be a valid store path");
        // Producer-verbatim positive: the production formatter applied to
        // the production key shape (the full store path).
        let blanket = rio_proto::dag_first_failure_summary(DEP);
        assert!(is_dag_fallback_blanket(&blanket), "{blanket:?}");
        // Tolerated historical key shape: the bare hash-name basename the
        // scheduler's DrvHash was once documented as. Kept admitted so an
        // ingress normalization to the documented shape cannot silently
        // kill the recovery arm.
        let basename_blanket = rio_proto::dag_first_failure_summary(
            DEP.strip_prefix("/nix/store/")
                .expect("DEP is a full store path"),
        );
        assert!(
            is_dag_fallback_blanket(&basename_blanket),
            "{basename_blanket:?}"
        );
        // Must-block: every other failure shape the engine can see.
        let relay_line = format!("derivation '{DEP}' failed: builder failed with exit code 2");
        let cascade_line = format!(
            "dependency '{DEP}' failed: poison threshold reached after 3 distinct-worker \
             failures"
        );
        let trailing_reason = format!("derivation {DEP} failed: boom");
        for not_blanket in [
            // The gateway's per-derivation relay line: quoted full path
            // plus a `: <reason>` tail.
            relay_line.as_str(),
            // The scheduler's cascade shape.
            cascade_line.as_str(),
            // Worker text that merely mentions a derivation failing.
            "builder failed with exit code 2: derivation x failed",
            // No hash-name key: bare name, or hash without `.drv`.
            "derivation foo.drv failed",
            "derivation bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-dep failed",
            "derivation /nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-dep failed",
            // A residual slash past the store prefix is no store path.
            "derivation /nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-a/b.drv failed",
            // A reason tail disqualifies even a real key.
            trailing_reason.as_str(),
            "",
        ] {
            assert!(!is_dag_fallback_blanket(not_blanket), "{not_blanket:?}");
        }
        // The blanket is not part of the relayed-reason vocabulary: no
        // corpus string may match (the corpus is the same one that pins
        // `classify_reason`'s totality).
        for (reason, _) in scheduler_reason_corpus() {
            assert!(!is_dag_fallback_blanket(reason), "{reason}");
        }
    }

    /// THE failed-row shape lattice against poison evidence: every row
    /// shape the gateway can deliver for one failed root, crossed with
    /// the scheduler's poison snapshot, with the contracted
    /// classification asserted for each cell in both directions.
    ///
    /// QUANTIFICATION DOMAIN (the shapes, enumerated as data below): the
    /// per-root failure rows the gateway's single evidence-derivation
    /// site can produce (`rio-gateway/src/handler/build.rs`
    /// `root_evidence`: the root's own recorded terminal verbatim,
    /// otherwise the DAG-level result — `per_root_verdict`'s "Unverified
    /// blanket failure stands"), fed by the scheduler's terminal emission
    /// sites (`rio-scheduler/src/actor/completion.rs`: the failing node's
    /// own Failed event embedding the daemon's last-N-log-lines text, and
    /// the cascade's `dependency '<drv>' failed: <text>` events;
    /// `rio-scheduler/src/actor/build.rs` `transition_build_to_failed`:
    /// the build-level `derivation /nix/store/<hash>-<name>.drv failed`
    /// summary (`rio_proto::dag_first_failure_summary` over the gateway's
    /// store-path DAG key) — the ONLY text for a root fail-fasted
    /// eventlessly at merge seeding, `rio-scheduler/src/actor/merge.rs`
    /// `seed_initial_states`):
    ///
    /// 1. own-terminal DependencyFailed, same batch — the dependent's own
    ///    message embeds the trigger's complete failure text.
    /// 2. own-terminal DependencyFailed, cross batch — the trigger failed
    ///    in an earlier batch; the dependent's message wraps the
    ///    needle-free poison reason and the collector fetches the
    ///    TRIGGER's log tail (fetch wiring proven by
    ///    `cross_batch_fixed_output_cascade_fetches_trigger_log`).
    /// 3. DAG-fallback blanket — no per-root event exists; the row is the
    ///    scheduler's build-level summary with no in-row trigger
    ///    evidence at all, so the only evidence channel is the recovered
    ///    trigger's own fetched log tail (fetch wiring proven by
    ///    `sticky_poison_blanket_recovers_trigger_from_poison_snapshot`).
    ///    Crossed with the tail axis {needled, needle-free, unfetchable}:
    ///    SourceRot has one bar at `resolve_failure_kind` (design §6.6:
    ///    "SourceRot now requires is_fixed_output … plus a fetch-error
    ///    needle in error_msg/log tail"), so a needle-free tail (a
    ///    worker-executed fixed-output failure — a hash mismatch carries
    ///    no fetch needle) and an unfetchable tail keep the genuine
    ///    charge through this door exactly as they do through doors 1-2
    ///    — otherwise one root cause would classify opposite ways by
    ///    batch timing. The CAPTURED-RELAY axis (does `batch.reasons`
    ///    carry a line for the trigger, and is it needle-free?) is the
    ///    door tests' jurisdiction
    ///    (`same_batch_poison_cascade_fetches_trigger_log_despite_captured_line`
    ///    and the blanket/needled-relay siblings): production's two
    ///    dominant captured shapes — the first-line-only relay capture
    ///    and `poison_and_cascade`'s bare threshold summary (the
    ///    scheduler emits it as the trigger's WHOLE reason and the
    ///    cascade wrap, completion.rs) — are needle-free by
    ///    construction, so the fetch doors test channel content against
    ///    the bar, never line presence, or these rows' bar would be
    ///    unsatisfiable same-batch.
    /// 4. generic failure — the root's OWN builder text: proof the root
    ///    itself executed.
    ///
    /// Cell contracts (design §7.1: source-unavailable is "the unit (or a
    /// dependency, with `cascaded: true`) failed only because a
    /// fixed-output input could not be fetched from its upstream origin";
    /// the genuine default is the module's charged-to-rio stance):
    ///
    /// - Must-admit: shapes 1-3 with the rotted fixed-output trigger
    ///   poisoned in the job's closure classify cascaded source-rot —
    ///   each through the evidence channel production fills for that
    ///   shape (needle-bearing text for 1-2, the trigger-keyed tail for
    ///   3).
    /// - Must-block: shape 4 stays Genuine EVEN WITH the poison row
    ///   present — in-row execution evidence outranks out-of-row
    ///   recovery, so a still-live poison row (the trigger may have been
    ///   substituted since it was poisoned) can never hide a failure rio
    ///   actually executed — shape 3 stays Genuine without the needle
    ///   (the poison signature proves the dependency kept failing on
    ///   real workers, not WHY), and EVERY shape without poison evidence
    ///   keeps its current classification, so the recovery cannot widen
    ///   beyond the poison signal.
    #[test]
    fn failed_row_shape_lattice_against_poison_evidence() {
        use crate::run::stderrparse::parse_stderr;
        let knobs = Knobs::default();
        let mut c = ctx("app.x86_64-linux", T, &[DEP], ExpectedOutcome::Built);
        c.fixed_output_drvs = std::sync::Arc::new([DEP.to_string()].into_iter().collect());

        // Shape-1 evidence, producer-verbatim: the trigger's complete
        // terminal message (daemon shape), relayed by the gateway and
        // captured by the engine's own line-split capture.
        let trigger_full_msg = format!(
            "builder for '{DEP}' failed with exit code 1;\n\
             last 10 log lines:\n\
             > trying https://example.com/dep-1.0.tar.gz\n\
             > curl: (22) The requested URL returned error: 404\n\
             For full logs, run 'nix log {DEP}'."
        );
        let relay_payload =
            format!("derivation '{DEP}' failed: {trigger_full_msg}\n  ↳ rio-cli logs '{DEP}'");
        let same_batch_reasons = parse_stderr(&relay_payload).reasons;
        assert!(
            !fetch_signature_present(&same_batch_reasons[DEP]),
            "producer parity: the captured relay channel must be the needle-free first line"
        );
        let cross_batch_msg = format!(
            "dependency '{DEP}' failed: poison threshold reached after 3 distinct-worker \
             failures"
        );
        // Producer-verbatim: the shared formatter over the store-path DAG
        // key (see dag_fallback_blanket_detector_is_producer_exact).
        let blanket = rio_proto::dag_first_failure_summary(DEP);
        let own_failure = format!(
            "builder for '{T}' failed with exit code 2;\n\
             last 2 log lines:\n\
             > make: *** [Makefile:12: all] Error 2"
        );

        let rot_cascade = || RioOutcome::DependencyFailed {
            root: RootCauseKind::SourceRot,
            failing_drv: DEP.to_string(),
        };
        let genuine = || RioOutcome::TargetFailed {
            kind: FailureKind::Genuine,
        };

        struct ShapeRow {
            name: &'static str,
            target: PathOutcome,
            reasons: BTreeMap<String, String>,
            log_tail: Option<&'static str>,
            with_poison: RioOutcome,
            without_poison: RioOutcome,
        }
        let rows = [
            ShapeRow {
                name: "own-terminal dependency-failed, same batch",
                target: po(
                    T,
                    BuildStatus::DependencyFailed,
                    &format!("dependency '{DEP}' failed: {trigger_full_msg}"),
                ),
                reasons: same_batch_reasons,
                log_tail: None,
                with_poison: rot_cascade(),
                without_poison: rot_cascade(),
            },
            ShapeRow {
                name: "own-terminal dependency-failed, cross batch",
                target: po(T, BuildStatus::DependencyFailed, &cross_batch_msg),
                reasons: BTreeMap::from([(T.to_string(), cross_batch_msg.clone())]),
                // The trigger-keyed log fetch the collector performs for
                // this shape, fed through decide()'s log-tail channel.
                log_tail: Some(
                    "trying https://example.com/dep-1.0.tar.gz\n\
                     curl: (22) The requested URL returned error: 404\n",
                ),
                with_poison: rot_cascade(),
                without_poison: rot_cascade(),
            },
            ShapeRow {
                name: "DAG-fallback blanket, needled trigger tail",
                target: po(T, BuildStatus::MiscFailure, &blanket),
                // Eventless: nothing was relayed for any drv; the
                // collector's blanket fetch supplies the TRIGGER's tail.
                reasons: BTreeMap::new(),
                log_tail: Some(
                    "trying https://example.com/dep-1.0.tar.gz\n\
                     curl: (22) The requested URL returned error: 404\n",
                ),
                with_poison: rot_cascade(),
                without_poison: genuine(),
            },
            ShapeRow {
                name: "DAG-fallback blanket, needle-free trigger tail (worker-executed FOD \
                       failure)",
                target: po(T, BuildStatus::MiscFailure, &blanket),
                reasons: BTreeMap::new(),
                log_tail: Some(
                    "hash mismatch in fixed-output derivation:\n\
                     specified: sha256-AAAA\n\
                     got:       sha256-BBBB\n",
                ),
                with_poison: genuine(),
                without_poison: genuine(),
            },
            ShapeRow {
                name: "DAG-fallback blanket, unfetchable trigger tail",
                target: po(T, BuildStatus::MiscFailure, &blanket),
                reasons: BTreeMap::new(),
                log_tail: None,
                with_poison: genuine(),
                without_poison: genuine(),
            },
            ShapeRow {
                name: "generic failure (the root's own execution)",
                target: po(T, BuildStatus::PermanentFailure, &own_failure),
                reasons: BTreeMap::new(),
                log_tail: None,
                with_poison: genuine(),
                without_poison: genuine(),
            },
        ];

        let trigger_poisoned: HashMap<String, Vec<String>> =
            HashMap::from([(DEP.to_string(), vec!["b1".to_string(), "b2".to_string()])]);
        let no_poison: HashMap<String, Vec<String>> = HashMap::new();
        for row in &rows {
            for (state, poisoned, expected) in [
                (
                    "poisoned-FOD-in-closure",
                    &trigger_poisoned,
                    &row.with_poison,
                ),
                ("no-poison", &no_poison, &row.without_poison),
            ] {
                let cell = format!("[{} × {state}]", row.name);
                let batch = BatchView {
                    reasons: row.reasons.clone(),
                    ..BatchView::default()
                };
                match decide(
                    &c,
                    Some(&row.target),
                    &batch,
                    poisoned,
                    prior(0),
                    &knobs,
                    row.log_tail,
                ) {
                    CollectDecision::Terminal { rio, .. } => {
                        assert_eq!(&rio, expected, "{cell}");
                        // Verdict-layer coupling for every cascade cell:
                        // the classifier must turn the recovered outcome
                        // into the cascaded source-unavailable exclusion.
                        if matches!(
                            rio,
                            RioOutcome::DependencyFailed {
                                root: RootCauseKind::SourceRot,
                                ..
                            }
                        ) {
                            let cls = classify(&ExpectedOutcome::Built, &rio, &AuxFlags::default());
                            assert_eq!(
                                cls.class,
                                UnifiedClass::Verdict(Verdict::SourceUnavailable),
                                "{cell}"
                            );
                            assert!(cls.cascaded, "{cell}: cascaded dependent must be flagged");
                        }
                    }
                    other => panic!("{cell}: expected a terminal decision, got {other:?}"),
                }
            }
        }
    }

    /// Sticky-poison cascade end-to-end through
    /// [`process_settled_batch`]: a dependent merged onto a fixed-output
    /// derivation still poisoned past the scheduler's resubmit-reset
    /// budget is fail-fasted at merge seeding with NO per-root event, so
    /// its in-band row is the DAG-fallback blanket — `MiscFailure`, the
    /// scheduler's build-level summary, no relayed reasons, no trigger
    /// reference. The collector must intersect the batch's poison
    /// snapshot with the job's dependency closure, recover the trigger
    /// candidate, fetch the TRIGGER's log tail (the fetch is keyed by
    /// the trigger — the only channel that can satisfy the source-rot
    /// needle bar for an eventless row), and settle the record as
    /// cascaded source-unavailable carrying the recovery method and the
    /// poison row as evidence — instead of charging the dependent to
    /// the headline as an unexpected failure.
    #[tokio::test]
    async fn sticky_poison_blanket_recovers_trigger_from_poison_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let mut app = ctx("app.x86_64-linux", T, &[DEP], ExpectedOutcome::Built);
        app.fixed_output_drvs = std::sync::Arc::new([DEP.to_string()].into_iter().collect());
        let admin = LogAdmin {
            // The sticky poison row: the rotted FOD kept failing on real
            // workers, well within the scheduler's poison TTL.
            poisoned: vec![PoisonedView {
                drv_path: DEP.to_string(),
                failed_executors: vec!["b1".into(), "b2".into()],
                poisoned_secs_ago: 7200,
            }],
            // The trigger's own log tail carries the fetcher's output —
            // the fetch-error needle the evidence bar requires.
            tail: b"trying https://example.com/dep-1.0.tar.gz\n\
                    curl: (22) The requested URL returned error: 404\n"
                .to_vec(),
            ..LogAdmin::default()
        };
        let store = FakeStoreApi::default();
        let contexts: HashMap<String, JobContext> = [("app.x86_64-linux".to_string(), app)].into();
        // Producer-verbatim: the shared formatter over the store-path DAG
        // key (see dag_fallback_blanket_detector_is_producer_exact).
        let blanket = rio_proto::dag_first_failure_summary(DEP);
        let batch = BatchView {
            kind: BATCH_KIND_SUBMIT.to_string(),
            build_id: Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".to_string()),
            results: vec![po(T, BuildStatus::MiscFailure, &blanket)],
            // Eventless fail-fast: the stderr stream relayed no
            // per-derivation failure line for ANY drv.
            reasons: BTreeMap::new(),
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

        match &decisions["app.x86_64-linux"] {
            CollectDecision::Terminal {
                rio: RioOutcome::DependencyFailed { root, failing_drv },
                evidence,
            } => {
                assert_eq!(*root, RootCauseKind::SourceRot);
                assert_eq!(failing_drv, DEP);
                let evidence = evidence.as_deref().expect("recovery must record evidence");
                assert!(
                    evidence.contains(DEP),
                    "the recovered trigger is named: {evidence}"
                );
                assert!(
                    evidence.contains("b1, b2"),
                    "the poison row's executors are the evidence: {evidence}"
                );
                assert!(
                    evidence.contains("poison snapshot intersected"),
                    "the recovery method is named: {evidence}"
                );
            }
            other => panic!("expected a recovered source-rot cascade, got {other:?}"),
        }
        // The record carries the cascaded exclusion, not a headline charge.
        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].verdict.as_deref(), Some("source-unavailable"));
        assert!(records[0].cascaded, "cascaded dependent must be flagged");
        assert_eq!(records[0].failure_cause.as_deref(), Some("source-rot"));
        assert_eq!(records[0].rio.failing_drv.as_deref(), Some(DEP));
        // The poison snapshot was consulted exactly once for the batch.
        assert_eq!(admin.poisoned_calls.load(Ordering::SeqCst), 1);
        // The scan fetch was keyed by the recovered TRIGGER — the
        // dependent's own tail could never carry the fetcher's output.
        // (The later fetch is the terminal record's own evidence capture,
        // which stays keyed by the dependent.)
        assert_eq!(
            admin.log_drvs.lock().unwrap().first().map(String::as_str),
            Some(DEP),
            "the blanket scan fetch must be trigger-keyed: {:?}",
            admin.log_drvs.lock().unwrap()
        );
    }

    /// The blanket door's evidence bar end-to-end, the must-block
    /// directions of the sibling test above: the poison signature alone
    /// (the dependency kept failing on real workers) does not satisfy
    /// the source-rot bar — the bar wants proof of WHY (a fetch-error
    /// needle in the trigger's own evidence, design §6.6). A trigger
    /// whose fetched tail carries no needle (the worker-executed
    /// hash-mismatch shape) and a trigger whose tail cannot be fetched
    /// at all both keep the genuine headline charge — the same
    /// resolution the evidenced cascade door reaches for those cases,
    /// so one root cause cannot classify opposite ways by batch timing
    /// (the in-row sibling's lost-tail arm resolves Genuine too: the
    /// module contract charges ambiguous evidence, never excuses it).
    #[tokio::test]
    async fn blanket_recovery_without_fetch_needle_keeps_the_genuine_charge() {
        for (case, tail) in [
            (
                "needle-free tail (hash mismatch)",
                b"hash mismatch in fixed-output derivation:\nspecified: sha256-AAAA\n".to_vec(),
            ),
            ("unfetchable tail", Vec::new()),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let state = StateDir::new(dir.path()).unwrap();
            let mut app = ctx("app.x86_64-linux", T, &[DEP], ExpectedOutcome::Built);
            app.fixed_output_drvs = std::sync::Arc::new([DEP.to_string()].into_iter().collect());
            let admin = LogAdmin {
                poisoned: vec![PoisonedView {
                    drv_path: DEP.to_string(),
                    failed_executors: vec!["b1".into(), "b2".into()],
                    poisoned_secs_ago: 7200,
                }],
                tail,
                ..LogAdmin::default()
            };
            let store = FakeStoreApi::default();
            let contexts: HashMap<String, JobContext> =
                [("app.x86_64-linux".to_string(), app)].into();
            let blanket = format!(
                "derivation {} failed",
                DEP.trim_start_matches("/nix/store/")
            );
            let batch = BatchView {
                kind: BATCH_KIND_SUBMIT.to_string(),
                build_id: Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".to_string()),
                results: vec![po(T, BuildStatus::MiscFailure, &blanket)],
                reasons: BTreeMap::new(),
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
            match &decisions["app.x86_64-linux"] {
                CollectDecision::Terminal {
                    rio:
                        RioOutcome::TargetFailed {
                            kind: FailureKind::Genuine,
                        },
                    ..
                } => {}
                other => panic!("[{case}] the genuine charge must stand, got {other:?}"),
            }
            let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
            assert_eq!(records.len(), 1, "[{case}]");
            assert_eq!(
                records[0].verdict.as_deref(),
                Some("unexpected-failure"),
                "[{case}] the row stays on the headline and in the gate's trip set"
            );
        }
    }

    /// The blanket recovery's deterministic pick and its conservative
    /// gates, both directions at the [`decide`] level. Every must-admit
    /// arm feeds the trigger's needled log tail (the evidence bar is
    /// satisfied; what is under test here is the candidate gating).
    ///
    /// Must-admit: several poisoned fixed-output closure members resolve
    /// to ONE deterministic trigger (first in path order — the closure
    /// set carries no topological order) with EVERY candidate recorded in
    /// the evidence; a qualifying poison row on the target itself wins
    /// over closure members (its own rot is no cascade) whatever the path
    /// order says.
    ///
    /// Must-block (each broken gate keeps the genuine headline charge —
    /// the conservative direction): a poisoned closure member that is not
    /// fixed-output; a poisoned fixed-output drv outside this job's
    /// closure (the build-wide blanket may be a batch-mate's failure); a
    /// poison row with no recorded worker failure (the scheduler's
    /// infra-poisoning shape).
    ///
    /// Contradictory-evidence refusal, crossed over its OWN gating bits
    /// (the refusal keys on non-fixed-output × non-empty executors ×
    /// closure membership, so each bit is flipped solo): a poisoned
    /// non-fixed-output closure member WITH worker failures is an
    /// executed, retry-exhausted dependency regression blocking this
    /// job — §7.1's "failed ONLY because a fixed-output input could not
    /// be fetched" cannot hold, so recovery refuses EVEN WHEN a
    /// qualifying rotted FOD coexists with a needled tail (the property
    /// holds for every snapshot containing such a member, not just
    /// solo). Flipping either sibling bit disarms the refusal: the same
    /// member with EMPTY executors is the infra-poisoning shape (not an
    /// executed regression), and the same member OUTSIDE the closure is
    /// a batch-mate's problem — recovery proceeds in both.
    #[test]
    fn blanket_recovery_picks_deterministically_and_gates_on_rot_evidence() {
        let knobs = Knobs::default();
        let no_reasons = BatchView::default();
        // Producer-verbatim: the shared formatter over the store-path DAG
        // key (see dag_fallback_blanket_detector_is_producer_exact).
        let blanket_for = rio_proto::dag_first_failure_summary;
        let execs = |names: &[&str]| names.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let row = po(T, BuildStatus::MiscFailure, &blanket_for(DEP));
        // The recovered trigger's fetched tail: the fetcher's own output,
        // carrying the fetch-error needle the evidence bar requires.
        let needled = "trying https://example.com/dep-1.0.tar.gz\n\
                       curl: (22) The requested URL returned error: 404\n";

        // Two rotted fixed-output deps poisoned at once: deterministic
        // first-in-path-order pick, both rows recorded as evidence in
        // that same order.
        let mut c = ctx("app.x86_64-linux", T, &[DEP, OTHER], ExpectedOutcome::Built);
        c.fixed_output_drvs =
            std::sync::Arc::new([DEP.to_string(), OTHER.to_string()].into_iter().collect());
        let both: HashMap<String, Vec<String>> = HashMap::from([
            (DEP.to_string(), execs(&["b1"])),
            (OTHER.to_string(), execs(&["b2"])),
        ]);
        match decide(
            &c,
            Some(&row),
            &no_reasons,
            &both,
            prior(0),
            &knobs,
            Some(needled),
        ) {
            CollectDecision::Terminal {
                rio: RioOutcome::DependencyFailed { root, failing_drv },
                evidence,
            } => {
                assert_eq!(root, RootCauseKind::SourceRot);
                assert_eq!(failing_drv, DEP, "first poisoned FOD in path order");
                let e = evidence.expect("evidence");
                assert!(
                    e.contains(DEP) && e.contains(OTHER),
                    "ALL candidates recorded: {e}"
                );
                assert!(
                    e.find(DEP).unwrap() < e.find(OTHER).unwrap(),
                    "candidates listed in deterministic path order: {e}"
                );
            }
            other => panic!("expected the deterministic source-rot pick, got {other:?}"),
        }

        // The target ITSELF is the sticky-poisoned FOD: self wins over a
        // closure candidate that sorts earlier, and the outcome is the
        // target's own source rot — no cascade is fabricated.
        let mut self_ctx = ctx("other.x86_64-linux", OTHER, &[DEP], ExpectedOutcome::Built);
        self_ctx.fixed_output_drvs =
            std::sync::Arc::new([DEP.to_string(), OTHER.to_string()].into_iter().collect());
        let self_row = po(OTHER, BuildStatus::MiscFailure, &blanket_for(OTHER));
        let with_self: HashMap<String, Vec<String>> = HashMap::from([
            (DEP.to_string(), execs(&["b1"])),
            (OTHER.to_string(), execs(&["b3"])),
        ]);
        match decide(
            &self_ctx,
            Some(&self_row),
            &no_reasons,
            &with_self,
            prior(0),
            &knobs,
            Some(needled),
        ) {
            CollectDecision::Terminal {
                rio: RioOutcome::TargetFailed { kind },
                evidence,
            } => {
                assert_eq!(kind, FailureKind::SourceRot);
                let e = evidence.expect("evidence");
                assert!(
                    e.contains(OTHER) && e.contains(DEP),
                    "self pick still records every candidate: {e}"
                );
            }
            other => panic!("expected the target's own source rot, got {other:?}"),
        }

        // Must-block gates.
        let genuine_stands = |decision: CollectDecision, gate: &str| match decision {
            CollectDecision::Terminal {
                rio:
                    RioOutcome::TargetFailed {
                        kind: FailureKind::Genuine,
                    },
                ..
            } => {}
            other => panic!("{gate}: expected the genuine charge to stand, got {other:?}"),
        };
        // (a) Poisoned closure member that is NOT fixed-output.
        let non_fod = ctx("app.x86_64-linux", T, &[DEP], ExpectedOutcome::Built);
        genuine_stands(
            decide(
                &non_fod,
                Some(&row),
                &no_reasons,
                &HashMap::from([(DEP.to_string(), execs(&["b1"]))]),
                prior(0),
                &knobs,
                Some(needled),
            ),
            "non-fixed-output trigger",
        );
        // (b) Poisoned fixed-output drv OUTSIDE this job's closure.
        let mut outside = ctx("app.x86_64-linux", T, &[DEP], ExpectedOutcome::Built);
        outside.fixed_output_drvs = std::sync::Arc::new([OTHER.to_string()].into_iter().collect());
        genuine_stands(
            decide(
                &outside,
                Some(&row),
                &no_reasons,
                &HashMap::from([(OTHER.to_string(), execs(&["b1"]))]),
                prior(0),
                &knobs,
                Some(needled),
            ),
            "outside-closure trigger",
        );
        // (c) Poison row with no recorded worker failure (infra shape).
        genuine_stands(
            decide(
                &c,
                Some(&row),
                &no_reasons,
                &HashMap::from([(DEP.to_string(), Vec::new())]),
                prior(0),
                &knobs,
                Some(needled),
            ),
            "no worker-failure evidence",
        );
        // (d) Contradictory-evidence refusal: a poisoned NON-fixed-output
        // closure member with worker failures coexists with the
        // qualifying rotted FOD — every other gate passes (needled tail
        // included), and the genuine charge still stands: a real
        // executed dependency regression blocks this job, so §7.1's
        // "only because" cannot hold.
        let mut mixed = ctx("app.x86_64-linux", T, &[DEP, OTHER], ExpectedOutcome::Built);
        mixed.fixed_output_drvs = std::sync::Arc::new([DEP.to_string()].into_iter().collect());
        let fod_plus_regression: HashMap<String, Vec<String>> = HashMap::from([
            (DEP.to_string(), execs(&["b1", "b2"])),
            (OTHER.to_string(), execs(&["b3"])),
        ]);
        genuine_stands(
            decide(
                &mixed,
                Some(&row),
                &no_reasons,
                &fod_plus_regression,
                prior(0),
                &knobs,
                Some(needled),
            ),
            "coexisting executed non-fixed-output regression",
        );
        // (d-sibling 1) The same non-FOD member with EMPTY executors is
        // the scheduler's infra-poisoning shape, NOT an executed
        // regression: it must not disarm the recovery.
        let infra_shaped: HashMap<String, Vec<String>> = HashMap::from([
            (DEP.to_string(), execs(&["b1", "b2"])),
            (OTHER.to_string(), Vec::new()),
        ]);
        match decide(
            &mixed,
            Some(&row),
            &no_reasons,
            &infra_shaped,
            prior(0),
            &knobs,
            Some(needled),
        ) {
            CollectDecision::Terminal {
                rio: RioOutcome::DependencyFailed { root, failing_drv },
                ..
            } => {
                assert_eq!(root, RootCauseKind::SourceRot);
                assert_eq!(
                    failing_drv, DEP,
                    "an infra-poisoned (executor-less) non-FOD row is not contradictory \
                     evidence"
                );
            }
            other => panic!("infra-shaped sibling must not refuse recovery, got {other:?}"),
        }
        // (d-sibling 2) The same non-FOD member OUTSIDE the closure is a
        // batch-mate's problem: it must not disarm the recovery either.
        let mut narrow = ctx("app.x86_64-linux", T, &[DEP], ExpectedOutcome::Built);
        narrow.fixed_output_drvs = std::sync::Arc::new([DEP.to_string()].into_iter().collect());
        match decide(
            &narrow,
            Some(&row),
            &no_reasons,
            &fod_plus_regression,
            prior(0),
            &knobs,
            Some(needled),
        ) {
            CollectDecision::Terminal {
                rio: RioOutcome::DependencyFailed { root, .. },
                ..
            } => assert_eq!(root, RootCauseKind::SourceRot),
            other => panic!("outside-closure sibling must not refuse recovery, got {other:?}"),
        }
    }

    /// Standing enumeration of the SourceRot minting surface: the verdict
    /// has ONE evidence owner — `resolve_failure_kind`'s needle-gated
    /// closure (design §6.6: "SourceRot now requires is_fixed_output …
    /// plus a fetch-error needle in error_msg/log tail") — and exactly two
    /// doors that MAP its result onto an outcome, the in-row cascade arm
    /// and the blanket-recovery arm, both downstream of the same resolver
    /// call.
    ///
    /// QUANTIFICATION DOMAIN: every non-comment line containing
    /// `SourceRot` in collect.rs above the file-level `#[cfg(test)]`
    /// marker. A new line is a new minting door (or a moved one) and
    /// fails here until it is re-derived against the one-owner rule —
    /// the round that added a second, bar-free door did so precisely
    /// because nothing pinned the minting surface.
    #[test]
    fn source_rot_minting_sites_are_enumerated() {
        const ALLOWED: &[&str] = &[
            // THE owner: the needle-gated mint inside resolve_failure_kind.
            "fetch_signature_present(&evidence_text).then_some((FailureKind::SourceRot, None))",
            // Door 1: the in-row cascade arm mapping the resolved kind.
            "FailureKind::SourceRot => RootCauseKind::SourceRot,",
            // Door 2: the blanket-recovery arm — gate on the resolved kind,
            // then the self-rot and cascade mappings.
            "if kind == FailureKind::SourceRot {",
            "kind: FailureKind::SourceRot,",
            "root: RootCauseKind::SourceRot,",
        ];
        let source = include_str!("collect.rs");
        let non_test = source
            .split("\n#[cfg(test)]")
            .next()
            .expect("collect.rs has a test module");
        let found: Vec<&str> = non_test
            .lines()
            .map(str::trim)
            .filter(|line| !line.starts_with("//") && line.contains("SourceRot"))
            .collect();
        for line in &found {
            assert!(
                ALLOWED.contains(line),
                "unenumerated SourceRot site: {line:?} — route the verdict through \
                 resolve_failure_kind's needle bar (one owner), then enumerate the mapping \
                 line here after review"
            );
        }
        for line in ALLOWED {
            assert!(
                found.contains(line),
                "enumerated SourceRot site no longer present: {line:?} — keep this list in \
                 lockstep with the minting surface"
            );
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
    /// already spent — and the journaled reason carries the
    /// batch-evidence bit on both sides of the announcement conjunct
    /// (`BatchView::cluster_acknowledged`): a batch the cluster
    /// acknowledged before the cut (build id observed) journals the
    /// ANNOUNCED variant (counted by the attempts measurement), a batch
    /// cut before any acknowledgment journals the plain variant (not
    /// counted) — so fold(journal) agrees with the current-event
    /// judgment for the same cycle. Both ride the same cycle budget.
    #[test]
    fn engine_cancelled_batch_requeues_members_without_results() {
        let knobs = Knobs::default();
        let no_poison: HashMap<String, Vec<String>> = HashMap::new();
        for (case, build_id, results, expected_why) in [
            (
                "announced (build id observed before the cut)",
                Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".to_string()),
                vec![],
                "engine-cancelled-announced",
            ),
            (
                // The acknowledgment disjunction's other producer: no
                // build id was captured, but a batch-mate's in-band
                // result proves the cluster saw the submission — the
                // member without a result still rides the announced
                // vocabulary, mirroring `cluster_acknowledged`.
                "announced (batch-mate results, no build id)",
                None,
                vec![po(OTHER, BuildStatus::Built, "")],
                "engine-cancelled-announced",
            ),
            (
                "fully cancelled (cut before any acknowledgment)",
                None,
                vec![],
                "engine-cancelled",
            ),
        ] {
            let batch = BatchView {
                build_id,
                results,
                engine_cancelled: true,
                ..BatchView::default()
            };
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
                    Some(expected_why),
                    "[{case}] prior_requeues = {prior_requeues}"
                );
            }
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
    ///
    /// Attempts on the exhaustion records report what the CLUSTER saw,
    /// per `RequeueReason::counts_as_cluster_attempt`'s contract (the
    /// ONLY path from events to the measurement; an engine-side
    /// submission failure "never reached the cluster at all"): the final
    /// failed submission this record is about is itself that excluded
    /// event class, so it adds nothing — "spent" stamps exactly its one
    /// journaled cluster attempt, and "never" (every submission failed at
    /// channel open) stamps 0, agreeing with the stalled-queued writer's
    /// stamp for the same zero-cluster-contact truth.
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
            (
                "never.x86_64-linux".to_string(),
                ctx("never.x86_64-linux", OTHER, &[], ExpectedOutcome::Built),
            ),
        ]
        .into();
        let batch = BatchView {
            kind: BATCH_KIND_SUBMIT.to_string(),
            stderr_tail: Some("engine submission error: ssh handshake: host key mismatch".into()),
            ..BatchView::default()
        };
        // Knobs::default() grants one auto-retry: "spent" burned it on a
        // real transport retry (a cluster attempt), "never" on an earlier
        // engine-side submission failure (not one). The budget map drives
        // the decision; the journal is what the record's attempts derive
        // from (production keeps the two in lockstep via
        // journal-then-increment).
        let prior: HashMap<String, PriorBudgets> = [
            ("spent.x86_64-linux".to_string(), prior(1)),
            ("never.x86_64-linux".to_string(), prior(1)),
        ]
        .into();
        for (job, why) in [
            ("spent.x86_64-linux", RequeueReason::NoInbandResult),
            ("never.x86_64-linux", RequeueReason::EngineSubmissionFailure),
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
        let decisions = process_settled_batch(
            &state,
            &admin,
            &FakeStoreApi::default(),
            None,
            &contexts,
            &[
                "fresh.x86_64-linux".into(),
                "spent.x86_64-linux".into(),
                "never.x86_64-linux".into(),
            ],
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
        let records = latest_per_job(state.load_jsonl(StateFile::Results).unwrap());
        assert_eq!(records.len(), 2, "{records:?}");
        let rec = &records["spent.x86_64-linux"];
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
        // attempts = the one journaled cluster-attempt requeue; the final
        // failed submission never reached the cluster
        // (counts_as_cluster_attempt pins EngineSubmissionFailure false),
        // so it cannot add a current attempt.
        assert_eq!(rec.attempts, 1);
        assert!(rec.build_ids.is_empty());
        // Zero cluster contact across the job's whole history: the
        // measurement reports zero attempts, not a fabricated 1.
        let never = &records["never.x86_64-linux"];
        assert_eq!(
            never.verdict.as_deref(),
            Some(Verdict::InfraIndeterminate.as_str())
        );
        assert_eq!(never.attempts, 0);
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

    /// The announced-cancel cell of the attempts measurement, both
    /// temporal positions and both sides of the announcement conjunct.
    /// An engine cancellation that fired AFTER the cluster acknowledged
    /// the submission journals `engine-cancelled-announced`, which the
    /// measurement counts — so a cycle scores identically as journaled
    /// history and as the settling event:
    ///
    /// - announced cancel then success: attempts = 2, flaky (the cluster
    ///   saw two submissions of this job);
    /// - two announced cancels then cycle-budget exhaustion: the
    ///   terminal stamps attempts = 3 (two journaled cluster-acknowledged
    ///   cycles + the final acknowledged submission);
    /// - control, fully cancelled cycles (never announced): the
    ///   journaled history counts 0 and the exhaustion terminal rides
    ///   the no-result writer with no current attempt — attempts = 0,
    ///   agreeing with the never-reached-the-cluster truth.
    #[tokio::test]
    async fn announced_cancel_cycles_count_in_the_attempts_measurement() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let contexts: HashMap<String, JobContext> = [
            (
                "ack.x86_64-linux".to_string(),
                ctx("ack.x86_64-linux", T, &[], ExpectedOutcome::Built),
            ),
            (
                "exhausted.x86_64-linux".to_string(),
                ctx("exhausted.x86_64-linux", DEP, &[], ExpectedOutcome::Built),
            ),
            (
                "cold.x86_64-linux".to_string(),
                ctx("cold.x86_64-linux", OTHER, &[], ExpectedOutcome::Built),
            ),
        ]
        .into();
        // Journaled history: one announced cancel for "ack", two for
        // "exhausted" (the full default cycle budget), two FULLY
        // cancelled cycles for "cold".
        for (job, why, n) in [
            (
                "ack.x86_64-linux",
                RequeueReason::EngineCancelledAnnounced,
                1,
            ),
            (
                "exhausted.x86_64-linux",
                RequeueReason::EngineCancelledAnnounced,
                2,
            ),
            ("cold.x86_64-linux", RequeueReason::EngineCancelled, 2),
        ] {
            for _ in 0..n {
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
        }

        // "ack" succeeds on the next wave: the announced cycle counts, so
        // the success is the SECOND cluster attempt — flaky.
        let success_batch = BatchView {
            kind: BATCH_KIND_SUBMIT.to_string(),
            build_id: Some("0193e4a2-7c1b-7d20-9b3a-3f2e3d4c5b6d".into()),
            results: vec![po(T, BuildStatus::Built, "")],
            ..BatchView::default()
        };
        process_settled_batch(
            &state,
            &LogAdmin::default(),
            &FakeStoreApi::default(),
            None,
            &contexts,
            &["ack.x86_64-linux".into()],
            &success_batch,
            &[("ack.x86_64-linux".to_string(), prior(1))].into(),
            &Knobs::default(),
            "leaf",
            "c1",
            &HashMap::new(),
            &HashSet::new(),
        )
        .await
        .unwrap();

        // "exhausted": a third announced cancellation with the cycle
        // budget spent terminalizes; the record stamps the two journaled
        // cycles plus the current acknowledged submission.
        let announced_cancel = BatchView {
            kind: BATCH_KIND_SUBMIT.to_string(),
            build_id: Some("0193e4a2-7c1b-7d20-9b3a-3f2e3d4c5b6e".into()),
            results: vec![],
            engine_cancelled: true,
            ..BatchView::default()
        };
        process_settled_batch(
            &state,
            &LogAdmin::default(),
            &FakeStoreApi::default(),
            None,
            &contexts,
            &["exhausted.x86_64-linux".into()],
            &announced_cancel,
            &[(
                "exhausted.x86_64-linux".to_string(),
                PriorBudgets {
                    requeues: 2,
                    cancel_cycles: 2,
                },
            )]
            .into(),
            &Knobs::default(),
            "leaf",
            "c1",
            &HashMap::new(),
            &HashSet::new(),
        )
        .await
        .unwrap();

        // "cold": the never-announced exhaustion control — no build id,
        // no results, the engine-side shape end to end.
        let cold_cancel = BatchView {
            kind: BATCH_KIND_SUBMIT.to_string(),
            build_id: None,
            results: vec![],
            engine_cancelled: true,
            ..BatchView::default()
        };
        process_settled_batch(
            &state,
            &LogAdmin::default(),
            &FakeStoreApi::default(),
            None,
            &contexts,
            &["cold.x86_64-linux".into()],
            &cold_cancel,
            &[(
                "cold.x86_64-linux".to_string(),
                PriorBudgets {
                    requeues: 2,
                    cancel_cycles: 2,
                },
            )]
            .into(),
            &Knobs::default(),
            "leaf",
            "c1",
            &HashMap::new(),
            &HashSet::new(),
        )
        .await
        .unwrap();

        let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        let by_job = |job: &str| records.iter().find(|r| r.job == job).unwrap();
        let ack = by_job("ack.x86_64-linux");
        assert_eq!(ack.verdict.as_deref(), Some("match-built"));
        assert_eq!(
            ack.attempts, 2,
            "the announced cancel was cluster contact; the success is attempt two"
        );
        assert!(ack.flaky, "two cluster attempts were needed");
        let exhausted = by_job("exhausted.x86_64-linux");
        assert_eq!(exhausted.verdict.as_deref(), Some("infra-indeterminate"));
        assert_eq!(
            exhausted.attempts, 3,
            "two journaled announced cycles + the current acknowledged submission"
        );
        let cold = by_job("cold.x86_64-linux");
        assert_eq!(cold.verdict.as_deref(), Some("infra-indeterminate"));
        assert_eq!(
            cold.attempts, 0,
            "fully cancelled cycles never reached the cluster — both temporal \
             positions agree on zero"
        );
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
    /// > 0) settles an EXECUTED success (`Built`), passes the
    /// already-terminal belt as the sanctioned superseding writer, and
    /// the surviving record is flaky match-built with attempts =
    /// confirmation_attempt + 1.
    ///
    /// Must-block, partitioned at the gating predicate's real granularity
    /// (`BuildStatus` has FOUR success members, not one — the belt gates
    /// on `executed()`, so every non-executed member needs its own row):
    /// a plain duplicate success (confirmation_attempt == 0) is still
    /// dropped; a confirmation retry whose result is another FAILURE adds
    /// nothing; and a confirmation retry that settles each of the THREE
    /// presence statuses (Substituted / AlreadyValid /
    /// ResolvesToAlreadyValid) is dropped too — presence proves the
    /// outputs landed in the store (any concurrent campaign or upstream
    /// substitution can do that for an expected-built unit, whose outputs
    /// exist upstream by construction), never that this retry could BUILD
    /// the unit, so it must not erase the recorded unexpected-failure
    /// from the regression gate.
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

        // Must-block legs 3-5: ALL THREE presence-shaped success statuses
        // on a sanctioned confirmation retry. Each is a wire success
        // (`is_success()`), none is an execution (`executed()`): the belt
        // must drop them and the recorded unexpected-failure must remain
        // the surviving verdict — presence on the retry is consistent
        // with the unit being unbuildable on rio while its outputs land
        // through any other channel.
        for presence_status in [
            BuildStatus::Substituted,
            BuildStatus::AlreadyValid,
            BuildStatus::ResolvesToAlreadyValid,
        ] {
            assert!(
                presence_status.is_success() && !presence_status.executed(),
                "{presence_status:?}: the row only makes sense for non-executed successes"
            );
            let presence_retry = BatchView {
                kind: BATCH_KIND_TIMED.to_string(),
                build_id: Some("0193e4a2-7c1b-7d20-9b3a-5f2e3d4c5b6e".to_string()),
                results: vec![po(T, presence_status, "")],
                confirmation_attempt: 1,
                ..BatchView::default()
            };
            let dir = tempfile::tempdir().unwrap();
            let state = StateDir::new(dir.path()).unwrap();
            let mut already_terminal: HashSet<String> = HashSet::new();
            run_batch(&state, &initial_failure, &already_terminal).await;
            already_terminal.insert(job.to_string());
            let decisions = run_batch(&state, &presence_retry, &already_terminal).await;
            assert_eq!(
                decisions.get(job),
                Some(&CollectDecision::AlreadyTerminal),
                "{presence_status:?}: a presence retry must be dropped, not supersede"
            );
            let records: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
            assert_eq!(records.len(), 1, "{presence_status:?}: {records:?}");
            let surviving = latest_per_job(records);
            assert_eq!(
                surviving[job].verdict.as_deref(),
                Some(Verdict::UnexpectedFailure.as_str()),
                "{presence_status:?}: the genuine unexpected-failure must survive on the gate"
            );
        }
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
                // The build id was observed before the cut, so the
                // requeue-shaped reason is the announced variant.
                CollectDecision::Defer {
                    reason: "engine-cancelled-announced",
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
            import_skipped_by_root: BTreeMap::new(),
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
            lost_terminals: BTreeSet::new(),
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
