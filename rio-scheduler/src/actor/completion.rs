//! Completion handling: worker reports build done → update DAG, cascade, emit events.
/// merged_bug_013 keystone: record one flagged sighting and return the
/// number of DISTINCT CONTROLLER-AUTHORITATIVE node bindings inside
/// the corroboration window. The sightings map is keyed by NODE
/// (non-optional `String`), so an unattributed report — a pull
/// attempt with no controller binding yet — STRUCTURALLY cannot mint
/// a distinct-node count: there is no `None` bucket to pair with the
/// same node's later attributed sighting (the pre-fix
/// `HashMap<Option<String>, _>` counted exactly that mixed
/// `None`+`Some` pair as 2 "distinct nodes" and self-corroborated
/// into the uncharged Paced lane). Binding-less evidence rides ONLY
/// the scheduler's own store-health leg, never the node count.
// r[impl sched.retry.store-degraded-uncharged+4]
pub(super) fn note_store_degraded_sighting(
    sightings: &mut std::collections::HashMap<String, std::time::Instant>,
    node: Option<String>,
    now: std::time::Instant,
    window: std::time::Duration,
) -> usize {
    if let Some(node) = node {
        sightings.insert(node, now);
    }
    sightings.retain(|_, t| now.duration_since(*t) <= window);
    sightings.len()
}
// r[impl sched.completion.idempotent]
// r[impl sched.critical-path.incremental]

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use tracing::{debug, error, info, instrument, warn};
use uuid::Uuid;

use crate::db::attempts::AttemptRow;
use crate::state::{
    BuildStateExt, DerivationStatus, DrvHash, ExecutorId, OutcomeClass, ReportingParty,
};

use super::DagActor;

/// The identity witness of a late report's OWN execution (bug_098):
/// minted ONLY where the fold resolved its evidence — the pull report
/// intake's find key, the shim's admission-lookup exec, the in-body
/// Cancelled arm's own carrier (a Cancelled node's `exec_id` is the
/// cancelled attempt's own — the carrier clears on terminal EXIT,
/// never entry). The fill stamps THIS execution; it is never
/// re-resolved through mutable node state (cancel → resubmit →
/// re-dispatch moves the node's carrier to the successor attempt; a
/// fill that re-resolves mints a first 'cancelled' verdict on the
/// RUNNING successor and blocks its real verdict forever).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ReportingExec(pub(super) Uuid);

/// The disposition of the derivation a late report names, as the
/// classifier consumes it (round-9 WO-S1-1): the SECOND classification
/// axis beside the report's own status. `Cancelled` = the node is
/// DAG-resident in the cancelled state (the within-grace face — the
/// node itself is the warm cancel context; no separate grace constant
/// exists, `TERMINAL_CLEANUP_DELAY` bounds the residency). `Unknown` =
/// the node is gone (evicted by `handle_cleanup_terminal_build`, or
/// never known) — the beyond-grace face; identity cold-resolves from
/// the durable rows. `Other` = resident in any non-cancelled state
/// (e.g. a duplicate report on a Completed node, whose registration
/// already rode the success epilogue — no late registration applies).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LateNodeContext {
    /// DAG-resident, status == Cancelled.
    Cancelled,
    /// Not in the DAG (evicted or never merged).
    Unknown,
    /// DAG-resident in any other status.
    Other,
}

/// The late-report (AckIgnore) lane's typed effect alphabet (bug_077,
/// the closure set): what a report that FAILED kernel admission may
/// still do. A future late-report side effect adds a variant here —
/// never an ad-hoc statement in an AckIgnore arm. Computed by
/// [`late_report_effect`] from the payload plus the fold's resolved
/// identity; applied by [`DagActor::apply_late_report_effect`].
#[derive(Debug, PartialEq, Eq)]
pub(super) enum LateReportEffect {
    /// merged_bug_294 relocated to where late reports actually arrive
    /// (bug_077): the cancelled execution's `final_line_count`
    /// gap-fill, addressed by the reporting execution's own identity
    /// (bug_098). Safety = identity + monotone COALESCE: the stamp
    /// lands on the exec the fold resolved — the identity is INSIDE
    /// the effect, so a foreign exec is unrepresentable — and the SQL
    /// qual (`status IS NULL OR status = $2`) admits a FIRST verdict
    /// only on the reporting exec's OWN row, which is CORRECT (the
    /// report IS that exec's terminal: the degraded-window cell
    /// honestly delivers the count while the cancel persist is still
    /// outbox-latched and the row unstamped). "Never mint a first
    /// verdict" is true as "never mint a first verdict on a FOREIGN
    /// exec", and the identity enforces it, not the qual.
    FillCancelledCount {
        /// The reporting execution — the row the fill addresses.
        exec: ReportingExec,
        /// The report's post-footer line count (> 0).
        count: i64,
    },
    /// Round-9 WO-S1-1 (the signed §5-S Q1 invariant: *completed
    /// uploads survive cancellation as registered evidence*): a late
    /// SUCCESS-class report (Built/Substituted/AlreadyValid) for a
    /// cancelled or evicted derivation carries everything registration
    /// needs — the validated output paths ride IN the letter; the
    /// historically-interested tenants cold-resolve from the durable
    /// builds/bd rows at apply time (cancellation strips in-memory
    /// interest synchronously, so warm attribution does not exist for
    /// this class on ANY face). Applying stamps `path_tenants` through
    /// the censused writer funnel ([`DagActor::stamp_path_tenants`] —
    /// the same family `handle_success_completion` rides; no second
    /// stamp path) and routes CA-shaped reports through the gated
    /// realisation insert ([`DagActor::ca_insert_realisations`]).
    /// Cancellation MAY stop future work; it MUST NOT discard
    /// registered or registrable completed work.
    Register {
        /// The report's validated outputs (store-path shape checked;
        /// declared-NAME membership checked when the node is
        /// resident; the expected-set PATH membership law — bug_138 —
        /// is enforced at apply against the durable row, both faces).
        outputs: Vec<crate::domain::BuiltOutput>,
    },
    /// Acknowledge and write nothing (every other late report).
    Nothing,
}

// r[impl sched.trust.evidence-scope]
/// bug_155 — the NON-OPTIONAL evidence witness
/// [`DagActor::ca_insert_realisations`] demands. The `realisations`
/// table is GLOBALLY keyed (PK `(drv_hash = modular_hash,
/// output_name)`; its consumers — the resolve/merge `query_batch`
/// consults and the cutoff-compare `query_prior_realisation` — are
/// tenant-unscoped), so the evidence requirement derives from THAT
/// consumer's scope, not from the reporting cohort: every floating-CA
/// face carries its store-evidence set, and an EMPTY cohort yields
/// the EMPTY set (refuse all paths) — never an absence the insert
/// could read as "no boundary to guard". That vacuity argument is
/// valid only for the tenant-keyed `path_tenants` stamp reader; the
/// pre-fix `Option` encoding let the untenanted face (NULL-tenant
/// anon/dev builds; late lanes whose tenant rows aged out) skip the
/// membership law entirely, minting forged first-writer-wins
/// `modular_hash → victim_path` rows with zero corroboration.
///
/// `Evidenced` is constructible only from a
/// [`DagActor::ca_production_evidence`] consult (both lanes construct
/// it on every `is_ca && !is_fixed_output` path); a lane that cannot
/// present it takes `ExpectedSetBounded`, which the insert refuses
/// fail-closed on the floating-CA face.
pub(super) enum CaRealisationEvidence<'a> {
    /// Floating-CA (`is_ca && !is_fixed_output`): the store-evidenced
    /// `sha256(path)` set. Empty set ⇒ every output refuses — the
    /// untenanted face lands here (no cohort, no consultable
    /// evidence; bytes stay durable, the heal lane is lawful
    /// re-registration/re-stamp).
    Evidenced(&'a std::collections::HashSet<Vec<u8>>),
    /// The bounded faces ONLY — quantifier: census(ca_evidence_reader_census) —: deferred-IA and fixed-output reports
    /// passed the dispatch-minted expected-set retain
    /// ([`DagActor::retain_expected_members`]) — the path-value law
    /// already ran upstream. Presented on a floating-CA face, the
    /// insert refuses every output (the fail-closed belt).
    ExpectedSetBounded,
}

/// The one computation `(reporting identity, node context, payload
/// status, final_line_count, validated outputs)` →
/// [`LateReportEffect`]. Pure; all three lanes (the pull report
/// intake, the un-admitted `ProcessCompletion` shim, the in-body
/// degraded-window Cancelled arm) route through it — and since
/// round-9 WO-S1-1 the unknown-derivation early-returns classify here
/// too (a Register-or-censused-sibling classification, never a
/// pre-classifier discard). A `None` identity returns
/// [`LateReportEffect::Nothing`] for the fill law: a ghost exec is
/// never stamped (conservative — the count's only carrier is dropped
/// exactly when no execution can be named for it; disclosed). The
/// Register law needs no exec identity: it registers PATH evidence,
/// addressed by drv + tenant, not an execution row.
///
/// Law order: the fill is the Cancelled-status report's lane and the
/// Register is the success-status report's lane — the two cannot
/// collide on one report (disjoint on `status`).
// r[impl store.registration.cancel-survives]
pub(super) fn late_report_effect(
    reporting: Option<ReportingExec>,
    ctx: LateNodeContext,
    status: rio_proto::types::BuildResultStatus,
    final_line_count: u64,
    outputs: Vec<crate::domain::BuiltOutput>,
) -> LateReportEffect {
    use rio_proto::types::BuildResultStatus as S;
    let success = matches!(status, S::Built | S::Substituted | S::AlreadyValid);
    // The registration arm (round-9 WO-S1-1): success-class report ×
    // cancelled-or-evicted derivation × at least one validated output.
    // `Other` (resident, non-cancelled) never registers late — a
    // duplicate on a Completed node already registered through the
    // success epilogue, and a report on a live node is not LATE
    // evidence (the admitted lane owns it).
    if success && !outputs.is_empty() {
        match ctx {
            LateNodeContext::Cancelled | LateNodeContext::Unknown => {
                return LateReportEffect::Register { outputs };
            }
            LateNodeContext::Other => {}
        }
    }
    let cancelled = status == S::Cancelled;
    match (
        reporting,
        cancelled,
        i64::try_from(final_line_count).ok().filter(|n| *n > 0),
    ) {
        (Some(exec), true, Some(count)) => LateReportEffect::FillCancelledCount { exec, count },
        (None, _, _) | (_, true, None) | (_, false, _) => LateReportEffect::Nothing,
    }
}

/// Timeout for the CA cutoff-compare realisation lookup.
///
/// `query_prior_realisation` is an indexed point-lookup on
/// `realisations(output_path)` — sub-10ms when PG is healthy.
/// `DEFAULT_GRPC_TIMEOUT` (30s) is the "unary RPC over an unreliable
/// link" budget; a PG point-lookup doesn't need it. 2s is generous
/// for the lookup + one retry-worth of PG jitter.
///
/// This is a module constant (NOT plumbed through `grpc_timeout`)
/// because the CA compare runs INSIDE the single-threaded actor event
/// loop and its worst-case latency is the gating concern — callers
/// adjusting `grpc_timeout` for other reasons (tests, degraded links)
/// shouldn't accidentally widen this budget.
const CA_CUTOFF_LOOKUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Per-candidate prior realisation discovered during the CA cutoff
/// cascade walk. Carries everything needed to (a) verify the output
/// exists in the store and (b) stamp the skipped node with its
/// output_path + insert a realisation for the gateway's
/// QueryRealisation.
#[derive(Debug, Clone)]
struct CaCutoffVerified {
    output_name: String,
    output_path: String,
    output_hash: [u8; 32],
}

pub(super) use super::report_ctx::FailureReportCtx;

/// merged_bug_032: how long a flagged sighting (or a scheduler-side
/// store RPC failure) counts as corroborating evidence.
const STORE_DEGRADED_CORROBORATION_WINDOW: std::time::Duration =
    std::time::Duration::from_secs(600);

/// The gated disposition of a worker-supplied `store_degraded` flag —
/// see `DagActor::store_degraded_disposition`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StoreDegradedDisposition {
    /// The report did not carry the flag.
    NotDegraded,
    /// Flagged, corroborated, inside the kernel free run: uncharged
    /// paced requeue (the bug_408 class).
    Paced,
    /// Flagged but uncorroborated — charged as plain infrastructure.
    Uncorroborated,
    /// Flagged and corroborated, but the store-degraded count within
    /// the trailing bounded-uncharged run hit
    /// `STORE_DEGRADED_FREE_RUN`: charged fallthrough into the counted
    /// infra budget (merged_bug_032's operator-visible poison path).
    RunBound,
}

impl StoreDegradedDisposition {
    /// The counter label for the settled emission site
    /// (merged_bug_200); `NotDegraded` reports never tick.
    fn as_label(self) -> Option<&'static str> {
        match self {
            StoreDegradedDisposition::NotDegraded => None,
            StoreDegradedDisposition::Paced => Some("paced"),
            StoreDegradedDisposition::Uncorroborated => Some("uncorroborated"),
            StoreDegradedDisposition::RunBound => Some("run_bound"),
        }
    }
}

/// Total status→report-context classifier: the SOLE non-test producer
/// of [`FailureReportCtx`]. Every failure arm of `handle_completion`'s
/// routing match calls this instead of constructing the ctx inline, so
/// the status→evidence mapping lives in exactly one table — and only
/// the `InfrastructureFailure` row can reach the degraded-carrying
/// constructor (merged_bug_072 / bug_096: the permanent family used to
/// forward the raw wire bit).
///
/// `Cancelled` synthesizes its message (the worker sent a status the
/// scheduler never initiated — the wire message is untrusted noise on
/// that arm); every other arm borrows the worker's `error_msg`.
/// bug_090: the metric-label form of the sizing-class alphabet.
pub(super) fn sizing_class_label(class: rio_proto::types::FailureClass) -> &'static str {
    match class {
        rio_proto::types::FailureClass::Unspecified => "unspecified",
        rio_proto::types::FailureClass::CgroupOom => "cgroup_oom",
        rio_proto::types::FailureClass::DiskFull => "disk_full",
    }
}

pub(super) fn failure_ctx_for<'a>(
    status: rio_proto::types::BuildResultStatus,
    result: &'a crate::domain::BuildResult,
    report_line_count: Option<i64>,
    peak_memory_bytes: u64,
) -> FailureReportCtx<'a> {
    use rio_proto::types::BuildResultStatus as S;
    match status {
        S::InfrastructureFailure => FailureReportCtx::infra(
            report_line_count,
            &result.error_msg,
            result.store_degraded,
            result.failure_classification.as_ref(),
            peak_memory_bytes,
        ),
        S::Cancelled => FailureReportCtx::non_infra(
            report_line_count,
            "worker reported Cancelled without scheduler-initiated cancel",
        ),
        // The permanent family, transient, timeout, unspecified, and
        // the (unreachable here) success statuses: structurally no
        // store evidence and no sizing claim — `non_infra` has
        // neither parameter.
        _ => FailureReportCtx::non_infra(report_line_count, &result.error_msg),
    }
}

/// Whether a Phase-1b-collapsed failure handler completed its appending
/// transaction. [`Self::RecordFailed`] means nothing was recorded and no
/// state changed — the derivation is still in its pre-report state and
/// the completion event must be re-delivered (bounded) by the caller,
/// because the worker never re-sends a `CompletionReport`.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FailureHandling {
    /// The event was fully processed (or legitimately dropped by a
    /// status/precondition guard) — nothing to re-deliver.
    Handled,
    /// The appending transaction failed; re-deliver the completion
    /// event via the bounded re-enqueue helper.
    RecordFailed,
}

impl DagActor {
    // -----------------------------------------------------------------------
    // Attempt-ledger appends (Phase 1a): one row per observed attempt,
    // written in a site-owned transaction together with the site's
    // status persist. Still best-effort in 1a — nothing reads the rows
    // for decisions, so a PG failure degrades exactly as the old
    // best-effort status persist did (log and proceed with the
    // in-memory transition).
    // -----------------------------------------------------------------------

    /// Build the ledger row for an attempt observed on `drv_hash`,
    /// capturing `db_id` / `exec_id` / the assigned executor / the
    /// resubmit cycle BEFORE any state transition clears them. Returns
    /// `None` when the node is unknown or its merge has not committed
    /// (`db_id` is `None`) — there is nothing to key a row on; callers
    /// fall back to the plain status persist.
    ///
    /// Pull-mode attempts (the in-flight executor identity is the
    /// attested intent itself) additionally carry `source_node` from
    /// the controller-authoritative spawn-ack binding (AD2c) so the
    /// exclusion fold keys them by node; stream attempts never get the
    /// stamp here, keeping their pod-name exclusion key untouched.
    pub(super) fn attempt_row_for(
        &self,
        drv_hash: &DrvHash,
        outcome_class: OutcomeClass,
        reporting_party: ReportingParty,
    ) -> Option<AttemptRow> {
        let state = self.dag.node(drv_hash)?;
        let db_id = state.db_id?;
        let mut row = AttemptRow::new(
            db_id,
            outcome_class,
            reporting_party,
            crate::state::AttemptKind::Build,
        );
        row.exec_id = state.exec_id;
        row.executor_id = state.assigned_executor.clone();
        row.source_node = self.pull_attempt_source_node(drv_hash);
        row.resubmit_cycle = i32::try_from(state.retry.resubmit_cycles).unwrap_or(i32::MAX);
        Some(row)
    }

    /// The controller-authoritative node binding for `drv_hash`, but
    /// only when the in-flight attempt is pull-mode — i.e. the assigned
    /// executor identity IS the attested intent (the pull path's
    /// identity convention; stream executors are pod names). `None` for
    /// stream attempts and for pull attempts whose binding the
    /// controller has not reported yet (AD2c: never derived from
    /// worker-supplied identity).
    pub(super) fn pull_attempt_source_node(&self, drv_hash: &DrvHash) -> Option<String> {
        let state = self.dag.node(drv_hash)?;
        let assigned = state.assigned_executor.as_ref()?;
        if assigned.as_str() != drv_hash.as_str() {
            return None;
        }
        self.authoritative_binding
            .get(drv_hash)
            .map(|b| b.node.clone())
    }

    /// The 1a appending transaction for a poison persist: append `row`
    /// (when there is one) and run
    /// `SchedulerDb::persist_poisoned_in_tx` on the same connection,
    /// then push the in-memory mirror after the commit. `row = None`
    /// degrades to the plain best-effort poison persist (the reportless
    /// poison triggers whose own rows land at their observation sites).
    ///
    /// Claims-floor fenced at the transaction start
    /// (`sched.evidence.durability`): a deposed replica's transaction
    /// rolls back having written nothing — no attempt row, no poison —
    /// and pushes no in-memory mirror.
    // r[impl sched.evidence.durability+4]
    pub(super) async fn record_attempt_with_poison(
        &mut self,
        drv_hash: &DrvHash,
        row: Option<AttemptRow>,
    ) {
        let Some(row) = row else {
            self.persist_poisoned(drv_hash).await;
            return;
        };
        let result: Result<Option<bool>, sqlx::Error> = async {
            let mut tx = match self.db.begin_fenced(self.serving_generation()).await? {
                crate::db::FencedBegin::Fenced { .. } => {
                    return Ok(None);
                }
                crate::db::FencedBegin::Open(ftx) => ftx,
            };
            let inserted = crate::db::SchedulerDb::append_attempt(tx.conn(), &row).await?;
            crate::db::SchedulerDb::persist_poisoned_in_tx(tx.conn(), drv_hash).await?;
            tx.commit().await?;
            Ok(Some(inserted))
        }
        .await;
        match result {
            Ok(Some(inserted)) => {
                if inserted {
                    if let Some(state) = self.dag.node_mut(drv_hash) {
                        state.push_attempt_record(row.to_record());
                    }
                    self.refresh_retry_view(drv_hash);
                }
            }
            Ok(None) => {
                self.note_fenced_evidence_write("attempt+poison appending transaction");
            }
            Err(e) => {
                error!(drv_hash = %drv_hash, error = %e,
                       "failed to persist attempt row + poisoned status+timestamp");
            }
        }
    }

    // -----------------------------------------------------------------------
    // Phase 1b: the appending transaction is the decision point. A
    // collapsed site appends its row, reads the post-reset suffix (plus
    // the transitional legacy mirror-column seed) on the same
    // connection, folds it through `decide()`, persists the verdict's
    // status via the `_in_tx` variants, and only then — after the
    // commit — performs the in-memory transition and epilogue. A failed
    // transaction leaves the derivation in its pre-report state and the
    // event is re-delivered with bounded retry
    // (`requeue_failure_completion`).
    // -----------------------------------------------------------------------

    /// The fold's budget view of the actor's live retry/poison
    /// configuration (plus the two compile-time poison constants).
    pub(super) fn decision_budget(&self) -> crate::retry_policy::Budget {
        crate::retry_policy::Budget {
            max_retries: self.retry_policy.max_retries,
            max_infra_retries: self.retry_policy.max_infra_retries,
            max_timeout_retries: self.retry_policy.max_timeout_retries,
            max_exempt_infra_retries: self.retry_policy.max_exempt_infra_retries,
            backoff_base_secs: self.retry_policy.backoff_base_secs as u64,
            backoff_multiplier: self.retry_policy.backoff_multiplier as u64,
            backoff_max_secs: self.retry_policy.backoff_max_secs as u64,
            poison_threshold: self.poison_config.threshold,
            require_distinct_workers: self.poison_config.require_distinct_workers,
            poison_resubmit_retry_limit: crate::state::POISON_RESUBMIT_RETRY_LIMIT,
            poison_ttl_secs: crate::state::POISON_TTL.as_secs(),
        }
    }

    /// Recompute `drv_hash`'s cached dispatch view (`state.retry`) from
    /// the seeded fold over its in-memory attempt history. Call after
    /// every committed change to that history (an append's
    /// `push_attempt_record`, a two-installment
    /// `classify_attempt_record`), so the dispatch-time readers
    /// (`hard_filter`'s exclusion, the resubmit-bound check, the
    /// diagnostics surfaces) see the same counters the appending
    /// transactions decide from. The `backoff_until` / `poisoned_at`
    /// carve-outs stay actor-managed (see
    /// `DerivationState::refresh_retry_view_from_ledger`).
    pub(super) fn refresh_retry_view(&mut self, drv_hash: &DrvHash) {
        let budget = self.decision_budget();
        let now_epoch = crate::db::attempts::epoch_now() as crate::retry_policy::AbsTime;
        if let Some(state) = self.dag.node_mut(drv_hash) {
            state.refresh_retry_view_from_ledger(&budget, now_epoch);
        }
    }

    /// The read half of a Phase-1b appending transaction: append `row`,
    /// load the post-reset suffix it now belongs to on the same
    /// connection, and fold it through `decide()`. The caller persists
    /// the verdict's status via the `_in_tx` variants on the same
    /// connection and commits.
    ///
    /// Returns whether the append actually inserted (an exec_id-bearing
    /// duplicate is rejected by the partial-unique index and reads back
    /// as the existing row) so callers that mirror the row onto the
    /// in-memory history can skip the push for duplicates.
    pub(super) async fn append_and_decide_in_tx(
        &self,
        tx: &mut sqlx::PgConnection,
        row: &AttemptRow,
    ) -> Result<(bool, crate::retry_policy::Decision), sqlx::Error> {
        let inserted = crate::db::SchedulerDb::append_attempt(tx, row).await?;
        let suffix =
            crate::db::SchedulerDb::load_attempt_suffix_one_in_tx(tx, row.derivation_id).await?;
        let history: Vec<crate::state::AttemptRecord> =
            suffix.iter().map(AttemptRow::to_record).collect();
        Ok((
            inserted,
            crate::retry_policy::decide(
                &history,
                &self.decision_budget(),
                crate::db::attempts::epoch_now() as crate::retry_policy::AbsTime,
            ),
        ))
    }

    /// Build a reset-event ledger row for `drv_hash`. `resubmit_cycle`
    /// is read from the node's CURRENT in-memory value (already
    /// incremented for a resubmit reset; already cleared for a
    /// cache-hit clear). The admin `ClearPoison` site BUMPS it to
    /// `cycles + 1` (bug_058 — the demand-epoch floor the post-clear
    /// re-insert presents, so the gave-up latch's change-keyed decay
    /// fires on the first post-clear observation); the TTL-expiry
    /// site still overrides it to 0 — a rewound face whose 0 == 0
    /// presentation a cycle-0 latch holds on (anti-replay equality),
    /// with the verdict-free band's explicit-resubmission mint as the
    /// documented escape. `None` when the node is unknown or its
    /// merge has not committed.
    pub(super) fn reset_row_for(
        &self,
        drv_hash: &DrvHash,
        outcome_class: OutcomeClass,
        reporting_party: ReportingParty,
    ) -> Option<AttemptRow> {
        let state = self.dag.node(drv_hash)?;
        let db_id = state.db_id?;
        Some(AttemptRow::new_reset(
            db_id,
            outcome_class,
            reporting_party,
            i32::try_from(state.retry.resubmit_cycles).unwrap_or(i32::MAX),
            crate::state::AttemptKind::Build,
        ))
    }

    /// 1a appending transaction for a single-derivation poison-clear /
    /// cache-hit reset: append the reset row (when there is one) and
    /// run `SchedulerDb::clear_poison_in_tx` on the same connection.
    /// Returns the transaction's result so call sites keep their
    /// existing PG-first error contracts (admin clear returns false on
    /// failure; the TTL/cache-hit sites log and continue). The
    /// in-memory mirror is pushed only after a successful commit.
    ///
    /// Claims-floor fenced at the transaction start
    /// (`sched.evidence.durability`): a deposed replica's reset rolls
    /// back having written nothing and returns
    /// [`crate::db::FencedOutcome::Fenced`] — callers treat it like the
    /// PG-failure arm of their existing contract (nothing was cleared),
    /// minus the error log.
    // r[impl sched.evidence.durability+4]
    pub(super) async fn record_reset_with_clear_poison(
        &mut self,
        drv_hash: &DrvHash,
        row: Option<AttemptRow>,
    ) -> Result<crate::db::FencedOutcome, sqlx::Error> {
        let mut tx = match self.db.begin_fenced(self.serving_generation()).await? {
            crate::db::FencedBegin::Fenced { .. } => {
                self.note_fenced_evidence_write("poison-clear reset appending transaction");
                return Ok(crate::db::FencedOutcome::Fenced);
            }
            crate::db::FencedBegin::Open(ftx) => ftx,
        };
        if let Some(row) = &row {
            crate::db::SchedulerDb::append_attempt(tx.conn(), row).await?;
        }
        crate::db::SchedulerDb::clear_poison_in_tx(tx.conn(), drv_hash).await?;
        tx.commit().await?;
        if let Some(row) = row {
            if let Some(state) = self.dag.node_mut(drv_hash) {
                state.push_attempt_record(row.to_record());
            }
            self.refresh_retry_view(drv_hash);
        }
        Ok(crate::db::FencedOutcome::Applied(1))
    }

    /// 1a appending transaction for the resubmit reset: one
    /// `resubmit_reset` row per reset node (carrying its NEW cycle
    /// index, already incremented in `dag.merge`) plus the existing
    /// batched poison clear, committed together. Returns the
    /// transaction's result so the merge persist keeps its existing
    /// best-effort warn.
    ///
    /// Claims-floor fenced at the transaction start
    /// (`sched.evidence.durability`): a deposed replica's reset rolls
    /// back having written nothing and pushes no in-memory mirror.
    // r[impl sched.evidence.durability+4]
    pub(super) async fn record_resubmit_resets(
        &mut self,
        reset_on_resubmit: &[DrvHash],
    ) -> Result<crate::db::FencedOutcome, sqlx::Error> {
        let rows: Vec<(DrvHash, AttemptRow)> = reset_on_resubmit
            .iter()
            .filter_map(|h| {
                Some((
                    h.clone(),
                    self.reset_row_for(h, OutcomeClass::ResubmitReset, ReportingParty::Scheduler)?,
                ))
            })
            .collect();
        let batch: Vec<AttemptRow> = rows.iter().map(|(_, r)| r.clone()).collect();
        let mut tx = match self.db.begin_fenced(self.serving_generation()).await? {
            crate::db::FencedBegin::Fenced { .. } => {
                self.note_fenced_evidence_write("resubmit-reset appending transaction");
                return Ok(crate::db::FencedOutcome::Fenced);
            }
            crate::db::FencedBegin::Open(ftx) => ftx,
        };
        crate::db::SchedulerDb::append_attempts_batch(tx.conn(), &batch).await?;
        crate::db::SchedulerDb::clear_poison_batch_in_tx(tx.conn(), reset_on_resubmit).await?;
        tx.commit().await?;
        let applied = rows.len() as u64;
        for (hash, row) in rows {
            if let Some(state) = self.dag.node_mut(&hash) {
                state.push_attempt_record(row.to_record());
            }
            self.refresh_retry_view(&hash);
        }
        Ok(crate::db::FencedOutcome::Applied(applied))
    }

    // -----------------------------------------------------------------------
    // Best-effort persist helpers (13 call sites across actor/*)
    // -----------------------------------------------------------------------

    /// Best-effort PG persist of derivation status. Logs error!,
    /// never returns it — PG blips shouldn't abort in-mem
    /// transitions (the scheduler is authoritative for live state;
    /// PG is recovery-only). Claims-floor fenced
    /// (`sched.evidence.durability`): a Fenced outcome is the fence
    /// refusing a deposed replica's write — warn + count + continue.
    // r[impl sched.evidence.durability+4]
    pub(super) async fn persist_status(
        &self,
        drv_hash: &DrvHash,
        status: DerivationStatus,
        executor_id: Option<&ExecutorId>,
    ) {
        #[cfg(test)]
        self.test_counters
            .persist_status_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match self
            .db
            .update_derivation_status(drv_hash, status, executor_id, self.serving_generation())
            .await
        {
            Ok(crate::db::FencedOutcome::Fenced) => {
                self.note_fenced_evidence_write("derivation status persist");
            }
            Ok(_) => {}
            Err(e) => {
                error!(drv_hash = %drv_hash, ?status, error = %e,
                       "failed to persist derivation status");
            }
        }
    }

    /// Best-effort atomic persist of `status='poisoned'` + `poisoned_at=now()`.
    /// Single SQL UPDATE — no crash window between the two columns.
    /// Logs error!, never returns it (same semantics as `persist_status`).
    /// Claims-floor fenced (`sched.evidence.durability`), same posture.
    // r[impl sched.evidence.durability+4]
    pub(super) async fn persist_poisoned(&self, drv_hash: &DrvHash) {
        match self
            .db
            .persist_poisoned(drv_hash, self.serving_generation())
            .await
        {
            Ok(crate::db::FencedOutcome::Fenced) => {
                self.note_fenced_evidence_write("poison persist");
            }
            Ok(_) => {}
            Err(e) => {
                error!(drv_hash = %drv_hash, error = %e,
                       "failed to persist poisoned status+timestamp");
            }
        }
    }

    /// Best-effort unpin of `scheduler_live_pins` rows for a
    /// terminal derivation. Called at every terminal transition
    /// (Completed/Poisoned/Cancelled; DependencyFailed is never
    /// dispatched so never pinned). `sweep_stale_live_pins` on
    /// recovery is the crash safety net for missed unpins.
    pub(super) async fn unpin_best_effort(&self, drv_hash: &DrvHash) {
        if let Err(e) = self.db.unpin_live_inputs(drv_hash).await {
            debug!(drv_hash = %drv_hash, error = %e,
                   "failed to unpin live inputs (best-effort)");
        }
    }

    /// Batch variant of [`persist_status`]: one PG round-trip for N
    /// derivations. Used by `cancel_build_derivations` where the
    /// per-item loop caused N+1 actor stall (500-drv cancel = ~1000
    /// sequential awaits blocking heartbeats). Same best-effort
    /// semantics: logs error!, never propagates. Claims-floor fenced
    /// (`sched.evidence.durability`), same posture as
    /// [`persist_status`].
    ///
    /// Composed from [`Self::persist_status_batch_db`] (the
    /// `&SchedulerDb`-only PG await) +
    /// [`Self::handle_persist_status_batch_result`] (the `&mut self`
    /// Fenced note / Err-arm latch). The split exists so phase-17's
    /// 3-way `try_join!` can hold THREE `&self`/`&self.db` borrows
    /// (Completed ∥ tenants ∥ Ready) and apply BOTH latches serially
    /// after — `&mut self` across the await would otherwise force
    /// serial.
    ///
    /// [`persist_status`]: Self::persist_status
    // r[impl sched.evidence.durability+4]
    pub(super) async fn persist_status_batch(
        &mut self,
        drv_hashes: &[&str],
        status: DerivationStatus,
    ) {
        let r =
            Self::persist_status_batch_db(&self.db, drv_hashes, status, self.serving_generation())
                .await;
        self.handle_persist_status_batch_result(drv_hashes, status, r);
    }

    /// PG-only half of [`Self::persist_status_batch`]: the fenced
    /// `update_derivation_status_batch` round-trip, taking only
    /// `&SchedulerDb` so a caller can join it against another `&self`
    /// borrow. The `&mut self` post-processing (Fenced note + Err-arm
    /// outbox latch) lives in
    /// [`Self::handle_persist_status_batch_result`] and runs AFTER the
    /// join. sh-007b S3-lite split-borrow.
    pub(super) async fn persist_status_batch_db(
        db: &crate::db::SchedulerDb,
        drv_hashes: &[&str],
        status: DerivationStatus,
        generation: crate::db::ServingGeneration,
    ) -> Result<crate::db::FencedOutcome, sqlx::Error> {
        db.update_derivation_status_batch(drv_hashes, status, generation)
            .await
    }

    /// `&mut self` post-processing for [`Self::persist_status_batch_db`]:
    /// Fenced → note, Ok → no-op, Err → outbox latch. Factored so the
    /// phase-17 join can hold `&self` across both PG awaits and apply
    /// the latch after.
    pub(super) fn handle_persist_status_batch_result(
        &mut self,
        drv_hashes: &[&str],
        status: DerivationStatus,
        result: Result<crate::db::FencedOutcome, sqlx::Error>,
    ) {
        match result {
            Ok(crate::db::FencedOutcome::Fenced) => {
                // Deposed: the successor owns these rows. NOT outboxed —
                // re-driving a fenced write only re-fences.
                self.note_fenced_evidence_write("derivation status batch persist");
            }
            Ok(_) => {}
            Err(e) => {
                error!(count = drv_hashes.len(), ?status, error = %e,
                       "failed to batch-persist derivation status; latched in the outbox");
                // r[impl sched.attempt.cancel-close-driven+3]
                // The persist is what closes the batch's assignment
                // rows: latch the owned batch for the housekeeping
                // tick's flusher, dropped only on a later Ok or when
                // the flush-time re-derivation finds the node
                // advanced past the latch. The active exec_ids are
                // latched NOW (memory is the only source — PG is
                // down): the replay's close is scoped to exactly
                // these, so a successor attempt minted between latch
                // and flush is untouchable (merged_bug_011 — the
                // absolute derivation-scoped close cancelled a
                // resubmitted build's fresh attempt).
                let exec_ids: Vec<uuid::Uuid> = drv_hashes
                    .iter()
                    .filter_map(|h| self.dag.node(h).and_then(|s| s.exec_id))
                    .collect();
                // Latched through the per-drv supersession chokepoint
                // (merged_bug_004): a newer latch for the same drv is
                // the only truth worth replaying queue-wide.
                self.latch_status_batch(super::StatusBatch {
                    drv_hashes: drv_hashes.iter().map(|s| s.to_string()).collect(),
                    status,
                    exec_ids,
                    enqueued_at: std::time::Instant::now(),
                    latched_at_epoch: crate::db::attempts::epoch_now(),
                });
            }
        }
    }

    /// Batch variant of [`unpin_best_effort`]. Same best-effort
    /// semantics (debug-log, never propagate); `sweep_stale_live_pins`
    /// on recovery backstops any missed unpins.
    ///
    /// [`unpin_best_effort`]: Self::unpin_best_effort
    pub(super) async fn unpin_best_effort_batch(&self, drv_hashes: &[&str]) {
        if let Err(e) = self.db.unpin_live_inputs_batch(drv_hashes).await {
            debug!(count = drv_hashes.len(), error = %e,
                   "failed to batch-unpin live inputs (best-effort)");
        }
    }

    /// Walk parents of a just-completed/skipped derivation: any
    /// `Queued` parent whose deps are now all completed-equivalent
    /// transitions to `Ready`, gets persisted, and becomes
    /// pull-claimable. Shared by every completion-like path
    /// (`release_downstream`, `complete_ready_from_store`,
    /// `ca_cutoff_cascade`, recovery's `adopt_orphan_completion`).
    pub(super) async fn promote_newly_ready(&mut self, completed: &DrvHash) {
        self.promote_newly_ready_batch(std::slice::from_ref(completed))
            .await;
    }

    // r[impl sched.db.batch-unnest]
    /// Slice-taking variant of [`promote_newly_ready`]: walk parents of
    /// EVERY hash in `completed`, dedup the union (a parent may be
    /// returned by several completed children), transition in-mem,
    /// then ONE `persist_status_batch(Ready)`.
    ///
    /// `find_newly_ready` returns ALL Queued parents whose deps are now
    /// satisfied — for stdenv/glibc-class nodes that's hundreds. The
    /// previous per-item `persist_status().await` inside the actor was
    /// N×PG-RTT of head-of-line blocking on heartbeats and dispatch
    /// (`r[sched.actor.single-owner]`).
    ///
    /// [`promote_newly_ready`]: Self::promote_newly_ready
    pub(super) async fn promote_newly_ready_batch(&mut self, completed: &[DrvHash]) {
        let mut newly_ready: Vec<DrvHash> = Vec::new();
        let mut seen: HashSet<DrvHash> = HashSet::new();
        for c in completed {
            for ready_hash in self.dag.find_newly_ready(c) {
                if !seen.insert(ready_hash.clone()) {
                    continue;
                }
                if let Some(s) = self.dag.node_mut(&ready_hash)
                    && s.transition(DerivationStatus::Ready).is_ok()
                {
                    newly_ready.push(ready_hash);
                }
            }
        }
        if newly_ready.is_empty() {
            return;
        }
        let refs: Vec<&str> = newly_ready.iter().map(DrvHash::as_str).collect();
        self.persist_status_batch(&refs, DerivationStatus::Ready)
            .await;
    }

    // r[impl sched.gc.path-tenants-upsert]
    /// Best-effort `path_tenants` upsert for a derivation that just
    /// reached a completed-equivalent state (Completed/Skipped/
    /// cache-hit). Resolves `interested_builds → tenant_ids` via the
    /// builds map, collects the node's `output_paths`, calls
    /// `db.upsert_path_tenants`. Logs warn! on failure — GC may
    /// under-retain, but never blocks the completion flow. The 24h
    /// global grace is the fallback.
    ///
    /// Called at every path that marks a derivation's outputs as
    /// "this tenant wants them": handle_success_completion, CA-cutoff
    /// skipped nodes, merge-time cache hits, merge-time pre-existing
    /// Completed, recovery orphan-completion. Missing any of these =
    /// paths GC'd prematurely under that tenant's retention policy.
    ///
    /// Single-node only — for N>1 inside an actor loop use
    /// [`upsert_path_tenants_for_batch`](Self::upsert_path_tenants_for_batch)
    /// (per-node sequential awaits here are an I-139 actor-stall).
    pub(super) async fn upsert_path_tenants_for(
        &self,
        drv_hash: &DrvHash,
        provenance: &crate::db::live_pins::StampProvenance,
    ) {
        let Some(state) = self.dag.node(drv_hash) else {
            return;
        };
        if state.output_paths.is_empty() {
            return;
        }
        // Only builds with a resolved tenant contribute.
        let tenant_ids: Vec<Uuid> = state.attributed_tenants(&self.builds).collect();
        if tenant_ids.is_empty() {
            return;
        }
        self.stamp_path_tenants(drv_hash, &state.output_paths, &tenant_ids, provenance)
            .await;
    }

    /// THE single-drv registration-stamp chokepoint (round-9 WO-S1-1;
    /// census-pinned with [`Self::upsert_path_tenants_for_batch`] —
    /// the two actor-side callers of the db writer family): every
    /// non-batched `path_tenants` ownership write in the scheduler
    /// routes through here, whether the (paths, tenants) pair resolved
    /// warm (the success epilogue via
    /// [`Self::upsert_path_tenants_for`]) or cold (the late-report
    /// Register arm). Best-effort: warn on Err, never block the
    /// caller — the tenant-retention GC seed under-retains until a
    /// re-stamp; the 24h global grace is the fallback.
    pub(super) async fn stamp_path_tenants(
        &self,
        drv_hash: &DrvHash,
        output_paths: &[String],
        tenant_ids: &[Uuid],
        provenance: &crate::db::live_pins::StampProvenance,
    ) {
        if let Err(e) = self
            .db
            .upsert_path_tenants(output_paths, tenant_ids, provenance)
            .await
        {
            warn!(
                drv_hash = %drv_hash, ?e,
                output_paths = output_paths.len(),
                tenants = tenant_ids.len(),
                "path_tenants upsert failed; GC retention may under-retain"
            );
        }
        // Round-9 WO-S1-3 (the identity half): the registration writer
        // owns the (path ↔ deriver) linkage — fill it where the
        // uploader did not declare it (monotone; wire-declared deriver
        // wins). The deriver is the drv_path: resident nodes carry it;
        // the late Register arm cold-resolves it with the tenants.
        let drv_path = self.dag.node(drv_hash).map(|s| s.drv_path().to_string());
        if let Some(drv_path) = drv_path {
            self.fill_deriver_for(drv_hash, output_paths, &drv_path)
                .await;
        }
    }

    /// The deriver-linkage fill half of the registration funnel
    /// (round-9 WO-S1-3): pairs every output path with `drv_path` and
    /// fills absent narinfo deriver cells. Split out so the late
    /// Register arm (whose node may be evicted) can call it with the
    /// COLD-resolved drv_path.
    pub(super) async fn fill_deriver_for(
        &self,
        drv_hash: &DrvHash,
        output_paths: &[String],
        drv_path: &str,
    ) {
        use sha2::Digest;
        let hashes: Vec<Vec<u8>> = output_paths
            .iter()
            .map(|p| sha2::Sha256::digest(p.as_bytes()).to_vec())
            .collect();
        let drv_paths: Vec<String> = vec![drv_path.to_string(); hashes.len()];
        match self.db.fill_deriver_linkage(&hashes, &drv_paths).await {
            Ok(filled) if filled > 0 => {
                debug!(
                    drv_hash = %drv_hash,
                    filled,
                    "deriver linkage filled at registration (identity half)"
                );
            }
            Ok(_) => {}
            Err(e) => {
                warn!(
                    drv_hash = %drv_hash, error = %e,
                    "deriver-linkage fill failed (best-effort; identity \
                     re-association degrades until a re-stamp)"
                );
            }
        }
    }

    /// Batched [`upsert_path_tenants_for`]: collect `(path_hash,
    /// tenant_id)` pairs across many derivations, then one UNNEST
    /// insert. Same lookup logic per-drv (output_paths from DAG state,
    /// tenant_ids via interested_builds → builds map); same best-effort
    /// semantics (warn on Err, never block). I-139 recurring: the per-
    /// hit call in `apply_cached_hits` was 5281 sequential PG awaits
    /// for tfc's 5298-node merge → ~20s of head-of-line blocking.
    ///
    /// [`upsert_path_tenants_for`]: Self::upsert_path_tenants_for
    pub(super) async fn upsert_path_tenants_for_batch(
        &self,
        items: &[(DrvHash, crate::db::live_pins::StampProvenance)],
    ) {
        use crate::db::live_pins::StampProvenance;
        use sha2::Digest;
        let mut hashes: Vec<Vec<u8>> = Vec::new();
        let mut tids: Vec<Uuid> = Vec::new();
        // Round-9 WO-S1-3: the identity half rides the same batch —
        // (output path hash, drv_path) pairs, one fill round-trip.
        let mut deriver_hashes: Vec<Vec<u8>> = Vec::new();
        let mut deriver_paths: Vec<String> = Vec::new();
        for (drv_hash, provenance) in items {
            let Some(state) = self.dag.node(drv_hash) else {
                continue;
            };
            if state.output_paths.is_empty() {
                continue;
            }
            let tenant_ids: Vec<Uuid> = state
                .interested_builds
                .iter()
                .filter_map(|id| self.builds.get(id)?.tenant_id)
                .collect();
            if tenant_ids.is_empty() {
                continue;
            }
            // Signed Q2: the lawful pairs derive from the per-drv
            // witness — THE one body shared with the single-drv
            // wrapper. The belt-and-braces consistency checks
            // (formerly at `upsert_path_tenants_raw`) sit HERE,
            // per-drv, where each item's own provenance is in scope:
            // a coalesced batch carries N independent `WalkVerified`
            // maps and merging them would over-stamp under
            // floating-CA path collision (the `lawful_pairs`
            // intersection at live_pins.rs is per-map).
            let drv_mark = hashes.len();
            provenance.lawful_pairs(&state.output_paths, &tenant_ids, &mut hashes, &mut tids);
            if let StampProvenance::ProbedBy(probe_tenant) = provenance {
                debug_assert!(
                    tids[drv_mark..].iter().all(|t| t == probe_tenant),
                    "ProbedBy stamps carry exactly the probing tenant"
                );
            }
            if let StampProvenance::WalkVerified(verified) = provenance {
                debug_assert!(
                    hashes[drv_mark..]
                        .iter()
                        .zip(tids[drv_mark..].iter())
                        .all(|(h, t)| verified.get(h.as_slice()).is_some_and(|v| v.contains(t))),
                    "WalkVerified stamps are within the wire-carried sets"
                );
            }
            for p in &state.output_paths {
                deriver_hashes.push(sha2::Sha256::digest(p.as_bytes()).to_vec());
                deriver_paths.push(state.drv_path().to_string());
            }
        }
        if hashes.is_empty() {
            return;
        }
        if let Err(e) = self.db.upsert_path_tenants_raw(&hashes, &tids).await {
            warn!(
                ?e,
                drvs = items.len(),
                pairs = hashes.len(),
                "batched path_tenants upsert failed; GC retention may under-retain"
            );
        }
        if let Err(e) = self
            .db
            .fill_deriver_linkage(&deriver_hashes, &deriver_paths)
            .await
        {
            warn!(
                ?e,
                drvs = items.len(),
                "batched deriver-linkage fill failed (best-effort; identity \
                 re-association degrades until a re-stamp)"
            );
        }
    }

    /// Walk downstream from `trigger` (a CA-unchanged completion) and
    /// discover prior output_paths for each cascade candidate via the
    /// `realisation_deps` reverse walk. Returns candidates whose
    /// prior outputs ALL exist in the store, along with the output
    /// metadata needed to stamp the skipped node + insert its
    /// realisation for the gateway.
    ///
    /// The walk: `trigger`'s prior modular_hash → `realisation_deps`
    /// reverse → prior downstream modular_hashes + output_paths.
    /// Match prior outputs to current DAG candidates by exact store-
    /// path name segment (`StorePath::parse(p).name() == {pname}` or
    /// `{pname}-{output}` for non-out). Verify existence via
    /// FindMissingPaths.
    ///
    /// The realisation-based trigger check (`query_prior_realisation`)
    /// already ensures a prior build existed — first-ever builds
    /// return `None` there, so `ca_output_unchanged` stays false and
    /// this verify never runs. That replaces the old
    /// `ContentLookup(exclude_store_path)` defense which was broken
    /// for CA (same content → same path → self-exclusion filtered
    /// out the only matching row).
    ///
    /// Batch: single FindMissingPaths RPC covering all discovered
    /// prior outputs. Bounded by MAX_CASCADE_NODES on both the in-mem
    /// DAG walk AND the PG realisation_deps walk.
    async fn verify_cutoff_candidates(
        &mut self,
        trigger: &DrvHash,
        prior_seeds: &[(Vec<u8>, String)],
    ) -> HashMap<DrvHash, Vec<CaCutoffVerified>> {
        use rio_proto::types::FindMissingPathsRequest;
        let Some(store_client) = &self.store_client else {
            return HashMap::new();
        };

        // In-mem speculative BFS: collect current DAG's cascade
        // candidates. Same over-approximation as before — the actual
        // cascade re-checks eligibility.
        let (candidates, _cap) = crate::dag::DerivationDag::speculative_cascade_reachable(
            trigger,
            crate::dag::MAX_CASCADE_NODES,
            |current, provisional| {
                self.dag
                    .find_cutoff_eligible_speculative(current, provisional)
            },
        );
        if candidates.is_empty() {
            return HashMap::new();
        }

        // PG realisation_deps reverse walk: from the trigger's PRIOR
        // modular_hash(es), find all previously-built dependents.
        // Uses realisation_deps_reverse_idx (migration 015, indexed
        // explicitly "for cutoff cascade").
        let prior_outputs = match tokio::time::timeout(
            CA_CUTOFF_LOOKUP_TIMEOUT,
            crate::ca::walk_dependent_realisations(
                self.db.pool(),
                prior_seeds,
                crate::dag::MAX_CASCADE_NODES,
            ),
        )
        .await
        {
            Ok(Ok(m)) => m,
            Ok(Err(e)) => {
                debug!(error = %e, "CA cutoff verify: realisation_deps walk failed; skipping cascade");
                return HashMap::new();
            }
            Err(_elapsed) => {
                debug!("CA cutoff verify: realisation_deps walk timed out; skipping cascade");
                return HashMap::new();
            }
        };
        if prior_outputs.is_empty() {
            debug!(
                trigger = %trigger,
                n_candidates = candidates.len(),
                "CA cutoff verify: no prior dependents in realisation_deps; skipping cascade"
            );
            return HashMap::new();
        }

        // Match current DAG candidates to prior outputs by EXACT
        // store-path name segment. A CA output_path is
        // `/nix/store/{32-char-hash}-{name}` (or `…-{name}-{outName}`
        // for non-out). `pname` is the most reliable invariant across
        // builds with different drv hashes but identical content.
        //
        // The match MUST be on the parsed name segment, not a string
        // suffix: `…-python-requests".ends_with("-requests")` is true,
        // so a suffix match cross-matches different packages (`tools`
        // vs `bootstrap-tools`, `foo` vs `lib-foo`, …) and writes the
        // WRONG path into `state.output_paths` + a poisoned
        // realisation row to PG.
        //
        // Ambiguity (>1 prior output with the exact same name segment
        // — e.g. two versions of one pname in one prior build, or two
        // nodes that fell through to the same `name=` fallback)
        // degrades to no-skip for that candidate: we cannot know
        // which prior path is the right one, so build it.
        let mut cand_to_prior: HashMap<DrvHash, Vec<CaCutoffVerified>> = HashMap::new();
        let mut check_paths: Vec<String> = Vec::new();
        for hash in &candidates {
            let Some(state) = self.dag.node(hash) else {
                continue;
            };
            // Match on the derivation's `name` (encoded in
            // `drv_path`), NOT `pname`: stdenv constructs `name =
            // "${pname}-${version}"` and store-path name segments
            // are built from `name`, so a `pname`-based comparison
            // (`"hello" == "hello-2.10"`) never matches for ~all of
            // nixpkgs. `drv_name()` is infallible (no recovered-node
            // `version=None` regression that a `${pname}-${version}`
            // reconstruction would have).
            let drv_name = state.drv_name();
            let mut matched: Vec<CaCutoffVerified> = Vec::new();
            for out_name in &state.output_names {
                let expected_name = rio_nix::store_path::output_path_name(drv_name, out_name);
                // Linear scan over prior_outputs — bounded by
                // MAX_CASCADE_NODES, so small-N. Count hits so the
                // ambiguity guard below can fire.
                let mut hits = prior_outputs.iter().filter(|((_, on), (p, _))| {
                    on == out_name
                        && rio_nix::store_path::StorePath::parse(p)
                            .ok()
                            .is_some_and(|sp| sp.name() == expected_name)
                });
                match (hits.next(), hits.next()) {
                    (Some(((_, prior_out), (path, oh))), None) => {
                        matched.push(CaCutoffVerified {
                            output_name: prior_out.clone(),
                            output_path: path.clone(),
                            output_hash: *oh,
                        });
                        check_paths.push(path.clone());
                    }
                    (Some(_), Some(_)) => {
                        // >1 prior output with this exact name →
                        // ambiguous. Conservative: drop the whole
                        // candidate (clear matched so the len()
                        // check below rejects it).
                        debug!(drv_hash = %hash, %expected_name,
                               "CA cutoff verify: ambiguous prior match; excluding candidate");
                        matched.clear();
                        break;
                    }
                    (None, _) => {} // no match → len() check excludes
                }
            }
            // All outputs must have a prior match. Partial match →
            // exclude (conservative: can't skip if we don't know
            // where ALL outputs are).
            if matched.len() == state.output_names.len() && !matched.is_empty() {
                cand_to_prior.insert(hash.clone(), matched);
            }
        }
        if check_paths.is_empty() {
            return HashMap::new();
        }

        // r[impl sched.breaker.cache-check+3]
        // Same store, same unavailability signal as merge.rs's
        // `find_missing_with_breaker`. The breaker exists to protect the
        // single-threaded actor loop from blocking on a known-down
        // store; without this gate, the timeout below blocks for up to
        // `grpc_timeout` (30s) per CA completion during an outage even
        // when the breaker is already open. Read-only on the breaker:
        // completion.rs trusts the merge-time probe rather than
        // re-feeding `record_*` (merge.rs half-open-probe model already
        // covers recovery). `&mut self` is for the cascade bookkeeping
        // below, not the breaker.
        if self.cache_breaker.is_open() {
            debug!("CA cutoff verify: cache breaker open; skipping cascade");
            return HashMap::new();
        }

        // Batch FindMissingPaths: verify all discovered prior outputs
        // still exist in the store (not GC'd). Failure → empty
        // verified set (safe fallback; downstream runs normally).
        let mut req = tonic::Request::new(FindMissingPathsRequest {
            store_paths: check_paths,
        });
        rio_proto::interceptor::inject_current(req.metadata_mut());
        let missing: HashSet<String> = match tokio::time::timeout(
            self.grpc_timeout,
            store_client.clone().find_missing_paths(req),
        )
        .await
        {
            Ok(Ok(r)) => r.into_inner().missing_paths.into_iter().collect(),
            Ok(Err(e)) => {
                debug!(error = %e, "CA cutoff verify: FindMissingPaths failed; skipping cascade");
                // merged_bug_179: an issued FMP failure is
                // store-health evidence on every surface.
                self.note_issued_store_rpc_failure("ca-cutoff-verify");
                return HashMap::new();
            }
            Err(_elapsed) => {
                debug!("CA cutoff verify: FindMissingPaths timed out; skipping cascade");
                self.note_issued_store_rpc_failure("ca-cutoff-verify");
                return HashMap::new();
            }
        };

        // Verified = candidates where ALL prior outputs are present.
        cand_to_prior
            .into_iter()
            .filter(|(_, outs)| outs.iter().all(|o| !missing.contains(&o.output_path)))
            .collect()
    }

    // r[impl sched.sla.reactive-floor+5]
    /// Double the relevant `resource_floor` dimension for `drv_hash`
    /// (D4). Thin wrapper around `floor::bump_floor_or_count` that
    /// handles the dag-node lookup, metric, log, and best-effort PG
    /// persist.
    ///
    /// Called ONLY from explicit per-attempt resource-exhaustion
    /// signals, every one presenting a typed
    /// [`super::floor::CorroborationWitness`] (bug_102 — the demand
    /// sits inside the mutation; the witness constructors are the
    /// only mints) — the caller set is MACHINE-PINNED by the
    /// `bump_resource_floor_caller_census` ([GEN-SET], db/live_pins.rs;
    /// a new caller reds the census until it files its row AND the
    /// lib.rs HELP):
    /// - `bump_floor_on_corroborated_claim` (worker-reported typed
    ///   `CgroupOom`/`DiskFull` claims via
    ///   `handle_infrastructure_failure`; band-corroborated by
    ///   `CorroborationWitness::corroborated_sizing`) — labels
    ///   `cgroup_oom` / `disk_full` (the live_057 prjquota-attributed
    ///   classification, `apply_disk_override`)
    /// - `handle_timeout_failure` (worker-reported `TimedOut`,
    ///   corroborated on attempt-open-duration >= assigned_deadline/2
    ///   by `CorroborationWitness::corroborated_timeout` — the
    ///   scheduler's own `running_since` anchor) — label `timeout`
    /// - the establishment sweep's witnessed-OomKilled disposition row
    ///   (live_058-b, housekeeping.rs — the controller-witnessed
    ///   per-container kubelet attribution via
    ///   `CorroborationWitness::witnessed`, promoted exactly once per
    ///   attempt via the establishment transaction's append+decide
    ///   `won` flag) — label `witnessed_oom`
    ///
    /// The stream-era controller-reported arm
    /// (`ReportExecutorTermination` → OOMKilled / DiskPressure /
    /// DeadlineExceeded) retired with that RPC. The pod-terminal
    /// `ReportAttemptOutcome` second installment still never promotes
    /// (it fills a worker-classified row); the witnessed-terminal
    /// ESTABLISHMENT lane is the one controller-witnessed promoter,
    /// for exactly the OomKilled letter, with the durable
    /// once-per-attempt dedup the retired arm lacked — every other
    /// witnessed letter is classify-only
    /// (`floor::witnessed_disposition`).
    ///
    /// NOT called from bare disconnect / `TransientFailure` /
    /// non-OOM `InfrastructureFailure`. The previous
    /// I-170/I-173/I-177/I-199 over-broad heuristic (promote on ANY
    /// failure path) over-fired: live QA showed cmake going medium→
    /// large→xlarge from a pod-kill + store-replica-restart with zero
    /// builds run, and floor is sticky (M_044) so the next submitter
    /// paid for an oversized pod.
    ///
    /// `reason_label`: emitted as a label on the metric + the log
    /// line so operators can tell the producers apart in dashboards.
    /// The label alphabet is {`cgroup_oom`, `disk_full`, `timeout`,
    /// `witnessed_oom`} — pinned by the caller census above (the
    /// `oom_killed`/`disk_pressure` arms were retired with the
    /// over-broad heuristic; `timeout` covers `DeadlineExceeded`
    /// too; `disk_full` is the live_057 worker quota lane) — keep
    /// the lib.rs HELP in lockstep when the census grows.
    /// bug_090 — the corroboration gate: a worker-reported TYPED
    /// sizing claim moves a persisted floor ONLY when its telemetry is — quantifier: census(typed_classification_bumps_only_with_corroboration) —
    /// consistent with the shape THIS scheduler assigned at dispatch
    /// (`state.sched.last_intent` — the corroboration anchor a forger
    /// cannot choose: each ladder step must claim exhaustion AT the
    /// previously-assigned size, from a real scheduled attempt).
    ///
    /// Acceptance bands: the band law lives WITH the witness mint —
    /// [`super::floor::CorroborationWitness::corroborated_sizing`]
    /// (bug_102: one site for the bands and the only constructor that
    /// can authorize the sizing axes; this fn keeps the refusal
    /// letters). No assigned shape (cold start, never dispatched by
    /// this leader) ⇒ refuse: there is nothing to corroborate
    /// against, and `bump_floor_or_count` would no-op on a zero base
    /// anyway. Refusals are TYPED letters: counted
    /// (`rio_scheduler_uncorroborated_sizing_claim_total`),
    /// attributed, classify-only (the report's retry/charge flow is
    /// unaffected).
    // r[impl sched.trust.report-corroboration+4]
    pub(super) async fn bump_floor_on_corroborated_claim(
        &mut self,
        drv_hash: &DrvHash,
        claim: super::report_ctx::SizingClaim,
    ) -> super::floor::FloorOutcome {
        let Some(intent) = self
            .dag
            .node(drv_hash)
            .and_then(|state| state.sched.last_intent.as_ref())
            .map(|i| (i.mem_bytes, i.disk_bytes))
        else {
            warn!(
                drv_hash = %drv_hash, class = ?claim.class,
                "refusing sizing claim: no dispatch intent to corroborate \
                 against (cold/never-dispatched)"
            );
            metrics::counter!(
                "rio_scheduler_uncorroborated_sizing_claim_total",
                "class" => sizing_class_label(claim.class)
            )
            .increment(1);
            return super::floor::FloorOutcome::default();
        };
        let (assigned_mem, assigned_disk) = intent;
        // bug_102: the bands live in the witness constructor — the
        // mint and the band law are ONE site (actor/floor.rs); this
        // fn keeps the refusal letters (count + warn, classify-only).
        let Some(witness) = super::floor::CorroborationWitness::corroborated_sizing(
            &claim,
            assigned_mem,
            assigned_disk,
        ) else {
            warn!(
                drv_hash = %drv_hash, class = ?claim.class,
                peak_memory = claim.peak_memory_bytes,
                quota = ?claim.quota,
                assigned_mem, assigned_disk,
                "refusing uncorroborated sizing claim (telemetry \
                 inconsistent with the assigned shape); classify-only"
            );
            metrics::counter!(
                "rio_scheduler_uncorroborated_sizing_claim_total",
                "class" => sizing_class_label(claim.class)
            )
            .increment(1);
            return super::floor::FloorOutcome::default();
        };
        // Unspecified cannot mint a witness (the constructor's
        // false arm), so the wire-default structurally cannot bump.
        self.bump_resource_floor(drv_hash, witness).await
    }

    // r[impl sched.trust.report-corroboration+4]
    /// bug_102 — the corroboration chokepoint: the demand sits INSIDE
    /// the floor mutation. Every caller presents a typed
    /// [`super::floor::CorroborationWitness`] minted by a verifying
    /// constructor (the worker cannot mint one), so an ungated axis is
    /// UNREPRESENTABLE — the pre-fix shape (the status-borne TimedOut
    /// lane bumping unconditionally while the typed-claim gate covered
    /// only the FailureClass carriers) cannot compile. The
    /// `(reason, label)` pair derives from the witness — one producer.
    pub(super) async fn bump_resource_floor(
        &mut self,
        drv_hash: &DrvHash,
        witness: super::floor::CorroborationWitness,
    ) -> super::floor::FloorOutcome {
        let reason = witness.reason();
        let reason_label = witness.label();
        let mut new_floor = None;
        let outcome = if let Some(state) = self.dag.node_mut(drv_hash) {
            let o = super::floor::bump_floor_or_count(state, reason, &self.sla_ceilings);
            if o.promoted {
                info!(
                    drv_hash = %drv_hash, reason = reason_label,
                    floor = ?state.sched.resource_floor,
                    "resource_floor bumped"
                );
                metrics::counter!(
                    "rio_scheduler_resource_floor_bumps_total",
                    "reason" => reason_label
                )
                .increment(1);
                new_floor = Some(state.sched.resource_floor);
            }
            o
        } else {
            super::floor::FloorOutcome::default()
        };
        // M_044: persist the floor so failover doesn't reset it →
        // re-OOM at probe defaults. Outside the node_mut borrow
        // (await point); best-effort — a lost write degrades to one
        // wasted retry.
        // Fenced + server-side GREATEST ratchet: a deposed replica's
        // late OOM report writes nothing, and a same-tenure stale base
        // can never lower a promoted dimension.
        if let Some(floor) = &new_floor {
            match self
                .db
                .update_resource_floor(drv_hash, floor, self.serving_generation())
                .await
            {
                Ok(o) if o.settled() => {}
                Ok(_) => self.note_fenced_evidence_write("resource-floor persist"),
                Err(e) => {
                    error!(drv_hash = %drv_hash, ?floor, error = %e,
                           "failed to persist resource_floor");
                }
            }
        }
        outcome
    }

    // -----------------------------------------------------------------------
    // ProcessCompletion
    // -----------------------------------------------------------------------

    /// Apply one [`LateReportEffect`] (the AckIgnore lane's only
    /// state-touching arm). The fill stamps the effect's OWN exec
    /// directly (bug_098) — a count gap-fill on the reporting
    /// execution's row, not a correlation event (the cancelled
    /// attempt's correlation landed at cancel time) — so there is no
    /// epilogue re-resolve through mutable node state: after a
    /// resubmit the node's carrier names the SUCCESSOR attempt, and
    /// the pre-fix re-resolve minted a first 'cancelled' verdict on
    /// the running successor via the stamp SQL's `status IS NULL`
    /// arm. The monotone COALESCE qual still bounds the write (an
    /// already-stamped equal-status row only fills NULL gaps; a
    /// different verdict matches zero rows). Delta from the epilogue
    /// route, disclosed: a drv already reaped from the DAG now still
    /// fills (the stamp needs no node), where the old route skipped —
    /// strictly less count loss, same monotone bound.
    ///
    /// The Register arm (round-9 WO-S1-1, the signed Q1 invariant):
    /// `drv_key` may be a drv_hash OR a drv_path (the evicted face
    /// arrives with whatever key the report carried); the canonical
    /// hash and the historically-interested tenants cold-resolve in
    /// ONE indexed query over the durable rows (derivations ⋈ bd ⋈
    /// builds — cancellation strips in-memory interest synchronously,
    /// so this is the lawful attribution on EVERY face). The stamp
    /// rides the censused writer funnel ([`Self::stamp_path_tenants`],
    /// `StampProvenance::BuiltLocally` — locally produced bytes; all
    /// historically-interested tenants lawful under the signed Q2
    /// witness law) and CA-shaped reports reach the gated realisation
    /// insert ([`Self::ca_insert_realisations`] — resident CA nodes;
    /// the general identity writes are WO-S1-3's). Best-effort like
    /// every registration write: a PG blip degrades retention, never
    /// the intake.
    // r[impl store.registration.cancel-survives]
    pub(super) async fn apply_late_report_effect(
        &mut self,
        executor_id: &str,
        drv_key: &str,
        effect: LateReportEffect,
    ) {
        let drv_hash = DrvHash::from(drv_key);
        match effect {
            LateReportEffect::FillCancelledCount {
                exec: ReportingExec(exec_id),
                count,
            } => {
                self.stamp_drv_execution_terminal(&drv_hash, exec_id, "cancelled", Some(count));
            }
            LateReportEffect::Register { mut outputs } => {
                // Cold identity resolve: canonical drv_hash + the
                // tenants whose builds were interested (LEFT JOIN so a
                // known drv with zero tenanted builds still resolves
                // its hash — the stamp is then vacuous, logged) + the
                // durable expected set and CA flags (bug_138: the
                // membership inputs — the evicted face has no resident
                // node, so the dispatch-minted truth is read from the
                // SAME durable row identity resolves from).
                type ColdRow = (String, String, Option<Uuid>, Vec<String>, bool, bool);
                let rows: Vec<ColdRow> = match sqlx::query_as(
                    r#"
                    SELECT d.drv_hash, d.drv_path, b.tenant_id,
                           d.expected_output_paths, d.is_fixed_output, d.is_ca
                      FROM derivations d
                      LEFT JOIN build_derivations bd USING (derivation_id)
                      LEFT JOIN builds b
                        ON b.build_id = bd.build_id AND b.tenant_id IS NOT NULL
                     WHERE d.drv_hash = $1 OR d.drv_path = $1
                    "#,
                )
                .bind(drv_key)
                .fetch_all(self.db.pool())
                .await
                {
                    Ok(rows) => rows,
                    Err(e) => {
                        warn!(
                            drv_key,
                            error = %e,
                            "late-report registration: identity resolve failed; \
                             registration evidence lost for this delivery \
                             (best-effort — the report redelivery may retry)"
                        );
                        return;
                    }
                };
                let Some((canonical_hash, canonical_drv_path, _, expected, is_fo, is_ca)) =
                    rows.first()
                else {
                    // The censused no-evidence sibling: no durable drv
                    // row — nothing to address registration to. Loud:
                    // this is the only arm that still drops a
                    // registrable report, and it requires the drv to
                    // have never been merged.
                    warn!(
                        drv_key,
                        outputs = outputs.len(),
                        "late success report for a derivation with no durable row; \
                         registration has no addressable identity — dropped"
                    );
                    return;
                };
                let canonical = DrvHash::from(canonical_hash.as_str());
                let canonical_drv_path = canonical_drv_path.clone();
                // bug_138 — the membership law on the late lane (the
                // weaker-checked wave-9 Register face): worker paths
                // verify against the durable expected set, mirroring
                // the admitted lane's resident check and the store's
                // PutPath enforcement. CA exemption under the durable
                // claims-mint predicate (`is_ca AND NOT
                // is_fixed_output` — the same face the store's
                // content recompute authorizes). The durable set is
                // the MERGE-time truth: a deferred-IA drv (CA-chain
                // input, is_ca=false) whose row still carries the
                // unresolved "" placeholders refuses here — priced:
                // that cell's bytes were claims-checked at upload
                // (PutPath ran against the post-resolve claims), the
                // refusal degrades exactly to the pre-registration
                // posture (bytes durable, tenant-invisible until
                // re-stamp), and "" never matches a parsed store
                // path, so the placeholder can never be forged.
                // r[impl sched.trust.report-membership+4]
                // (De Morgan of NOT(is_ca AND NOT is_fo) — the
                // non-exempt face.)
                if !*is_ca || *is_fo {
                    Self::retain_expected_members(
                        executor_id,
                        drv_key,
                        "late-register",
                        expected,
                        &mut outputs,
                    );
                    if outputs.is_empty() {
                        // Every output refused — each one counted and
                        // attributed above (the typed letter); the
                        // Register effect degrades to no stamp at
                        // all, never a partial one.
                        return;
                    }
                }
                let tenants: Vec<Uuid> = {
                    let mut t: Vec<Uuid> = rows.iter().filter_map(|(_, _, t, ..)| *t).collect();
                    t.sort();
                    t.dedup();
                    t
                };
                let paths: Vec<String> = outputs.iter().map(|o| o.output_path.clone()).collect();
                // bug_132/bug_155 — the late lane's CA face takes the
                // same production-evidence gate as the admitted lane
                // (the wave-9 Register face was the weaker-checked
                // replica once already; the durable `is_ca AND NOT
                // is_fo` is the same claims-mint predicate the skip
                // above keyed). The consult also bounds the
                // realisation insert below, and it runs on EVERY — quantifier: census(ca_evidence_reader_census) —
                // floating-CA path: an aged-out or untenanted cohort
                // consults empty and gets the EMPTY evidence set —
                // the realisation insert refuses all (its consumers
                // are tenant-unscoped; only the tenant-keyed STAMP is
                // vacuous on the empty cohort).
                let ca_evidence: Option<std::collections::HashSet<Vec<u8>>> = if *is_ca && !*is_fo {
                    Some(
                        self.ca_production_evidence(
                            executor_id,
                            drv_key,
                            "late-register",
                            &paths,
                            &tenants,
                        )
                        .await,
                    )
                } else {
                    None
                };
                if tenants.is_empty() {
                    info!(
                        drv_hash = %canonical,
                        paths = paths.len(),
                        "late success report registered no tenants (no tenanted \
                         build ever held interest); path stamp vacuous"
                    );
                } else {
                    info!(
                        drv_hash = %canonical,
                        paths = paths.len(),
                        tenants = tenants.len(),
                        "registering late built outputs for cancelled/evicted \
                         derivation (completed uploads survive cancellation)"
                    );
                    let stamp_provenance = match &ca_evidence {
                        Some(evidenced) => {
                            crate::db::live_pins::StampProvenance::BuiltLocallyEvidenced(
                                evidenced.clone(),
                            )
                        }
                        None => crate::db::live_pins::StampProvenance::BuiltLocally,
                    };
                    self.stamp_path_tenants(&canonical, &paths, &tenants, &stamp_provenance)
                        .await;
                }
                // The identity half (round-9 WO-S1-3): the deriver
                // linkage rides the COLD-resolved drv_path — the
                // evicted face has no resident node, so the funnel's
                // warm fill cannot fire; this is the lane that makes
                // the linkage exist for the full late population.
                self.fill_deriver_for(&canonical, &paths, &canonical_drv_path)
                    .await;
                // CA-shaped late reports reach the SAME gated
                // realisation insert the success epilogue uses (the
                // is_ca/needs_resolve gate reads the resident node;
                // an EVICTED CA node's modular hash is not durably
                // resolvable — the priced residual recorded in the
                // owning commit), evidence-bounded on the CA face
                // (empty cohort ⇒ empty set ⇒ refuse all — bug_155).
                self.ca_insert_realisations(
                    &canonical,
                    &outputs,
                    match &ca_evidence {
                        Some(set) => CaRealisationEvidence::Evidenced(set),
                        None => CaRealisationEvidence::ExpectedSetBounded,
                    },
                )
                .await;
            }
            LateReportEffect::Nothing => {}
        }
    }

    /// Validate a late report's raw wire outputs into the typed
    /// Register payload: the SAME name/shape boundary filters the
    /// admitted success path applies (store-path SHAPE at the trust
    /// boundary, then declared-NAME membership + dedup when the
    /// resident node's `output_names` are available — the evicted
    /// face has no declared set). The PATH membership law (bug_138 —
    /// path ∈ the dispatch-minted expected set) runs at APPLY time
    /// for this lane ([`DagActor::apply_late_report_effect`]'s
    /// Register arm, against the durable row both faces cold-resolve
    /// from), so the evicted face is exactly as path-checked as the
    /// resident one. Same counters as the admitted path — one metric
    /// surface for malformed/undeclared/unexpected outputs.
    pub(super) fn validated_late_outputs(
        executor_id: &str,
        drv_key: &str,
        declared: Option<&[String]>,
        raw: &[rio_proto::types::BuiltOutput],
    ) -> Vec<crate::domain::BuiltOutput> {
        let mut seen: HashSet<String> = HashSet::with_capacity(raw.len());
        raw.iter()
            .filter(|o| {
                if rio_nix::store_path::StorePath::parse(&o.output_path).is_err() {
                    warn!(
                        executor_id,
                        drv_key,
                        output_name = %o.output_name,
                        output_path = %o.output_path,
                        "dropping malformed worker-supplied output_path (late lane)"
                    );
                    metrics::counter!("rio_scheduler_malformed_built_output_total").increment(1);
                    return false;
                }
                if let Some(declared) = declared
                    && !declared.contains(&o.output_name)
                {
                    warn!(
                        executor_id,
                        drv_key,
                        output_name = %o.output_name,
                        "dropping worker-supplied output not declared by \
                         derivation (late lane)"
                    );
                    metrics::counter!("rio_scheduler_undeclared_built_output_total").increment(1);
                    return false;
                }
                seen.insert(o.output_name.clone())
            })
            .map(|o| crate::domain::BuiltOutput {
                output_name: o.output_name.clone(),
                output_path: o.output_path.clone(),
                output_hash: o.output_hash.clone(),
            })
            .collect()
    }

    /// bug_138 — the trust-boundary MEMBERSHIP law, the one filter
    /// both worker-report lanes share: a worker-supplied output path
    /// is retained ONLY if it is a member of the
    /// scheduler-authoritative expected set for the assignment — the
    /// SAME set the AssignmentClaims mint signs at dispatch
    /// (dispatch.rs `expected_outputs`) and the store's PutPath check
    /// enforces on upload (`put_path/common.rs`: `path ∈
    /// claims.expected_outputs`). This is the scheduler-side mirror:
    /// a report that triggers NO upload (the path already exists —
    /// another tenant's bytes) never meets the store's check, so the
    /// scheduler must enforce the same law before any registration
    /// stamp.
    ///
    /// Each refused output is a TYPED letter, not a shadow drop:
    /// counted (`rio_scheduler_unexpected_built_output_total`),
    /// attributed (executor, drv, lane, output), non-poisoning (the
    /// caller keeps processing the lawful subset; the report itself
    /// is never failed for it — same family as the malformed/
    /// undeclared siblings above).
    ///
    /// Callers gate the CA exemption with EXACTLY the claims-mint
    /// predicate (`state.ca.is_ca && !state.is_fixed_output`, or the
    /// durable `is_ca AND NOT is_fixed_output` on the cold face):
    /// floating-CA paths are computed post-build from the NAR hash —
    /// no sign-time expected set exists. That face's path-VALUE bound
    /// is [`Self::ca_production_evidence`] (bug_132): the store's
    /// content recompute (`verify_ca_store_path`) authorizes the
    /// UPLOAD, but an attack that uploads nothing never meets it —
    /// so the stamp additionally demands the store-recorded
    /// registration evidence the upload leaves behind (no upload, no
    /// stamp; machine-bound by the
    /// `ca_stamp_lanes_consult_production_evidence` census).
    // r[impl sched.trust.report-membership+4]
    pub(super) fn retain_expected_members(
        executor_id: &str,
        drv_key: &str,
        lane: &'static str,
        expected: &[String],
        outputs: &mut Vec<crate::domain::BuiltOutput>,
    ) {
        outputs.retain(|o| {
            if expected.iter().any(|e| e == &o.output_path) {
                return true;
            }
            warn!(
                executor_id,
                drv_key,
                lane,
                output_name = %o.output_name,
                output_path = %o.output_path,
                "refusing worker-supplied output_path outside the \
                 assignment's expected set (no registration stamp; \
                 the report's other effects are unaffected)"
            );
            metrics::counter!("rio_scheduler_unexpected_built_output_total").increment(1);
            false
        });
    }

    /// bug_132 — the floating-CA face's production-evidence consult
    /// (the membership law's CA face): the De-Morgan complement of
    /// [`Self::retain_expected_members`]'s gate. Floating-CA paths
    /// have no dispatch-minted expected set (computed post-build from
    /// the NAR hash), so the path-VALUE bound is the store-recorded
    /// registration evidence instead: a path is lawful iff the store
    /// stamped it for the reporting build's attributed cohort at
    /// upload ([`SchedulerDb::paths_with_production_evidence`] — the
    /// SAME `path_tenants` rows the visibility verdict reads — quantifier: census(ca_no_upload_report_never_flips_visibility_on_any_lane). **No
    /// upload, no stamp:** the returned set bounds BOTH the
    /// tenant-visibility stamp
    /// ([`StampProvenance::BuiltLocallyEvidenced`]) and the
    /// realisation insert ([`Self::ca_insert_realisations`] — without
    /// the latter, a forged mapping would resurrect the flip one lane
    /// over: the CA-cutoff cascade stamps skipped nodes' paths FROM
    /// `realisations` under plain `BuiltLocally`).
    ///
    /// Each refused output is a TYPED letter, not a shadow drop:
    /// counted (`rio_scheduler_unevidenced_ca_output_total`),
    /// attributed (executor, drv, lane, path), non-poisoning (the
    /// stamp is withheld; the report's other effects — completion,
    /// retry accounting, events — are unaffected; the refusal
    /// degrades exactly to the pre-registration posture: bytes
    /// durable, tenant-invisible until a lawful re-stamp).
    ///
    /// A consult ERROR fails CLOSED (empty evidence set): unknowable
    /// evidence must never grant cross-tenant visibility; the
    /// degraded posture and heal lane are the same as a refused path.
    ///
    /// An EMPTY cohort (bug_155 — untenanted anon/dev builds; late
    /// lanes whose tenant rows aged out) ALSO fails closed: evidence
    /// is structurally unrepresentable there (`path_tenants` is
    /// tenant-keyed, so no row can speak for an absent cohort), and
    /// the GLOBAL `realisations` consumers this set bounds are
    /// tenant-unscoped — the consult returns the empty set and every
    /// path takes the typed refusal letter. Degraded posture and heal
    /// lane unchanged: bytes durable, lawful re-registration/re-stamp
    /// recovers.
    ///
    /// [`SchedulerDb::paths_with_production_evidence`]: crate::db::SchedulerDb::paths_with_production_evidence
    /// [`StampProvenance::BuiltLocallyEvidenced`]: crate::db::live_pins::StampProvenance::BuiltLocallyEvidenced
    // r[impl sched.trust.report-corroboration+4]
    pub(super) async fn ca_production_evidence(
        &self,
        executor_id: &str,
        drv_key: &str,
        lane: &'static str,
        output_paths: &[String],
        tenant_ids: &[Uuid],
    ) -> std::collections::HashSet<Vec<u8>> {
        use sha2::Digest;
        let evidenced = match self
            .db
            .paths_with_production_evidence(output_paths, tenant_ids)
            .await
        {
            Ok(set) => set,
            Err(e) => {
                warn!(
                    executor_id,
                    drv_key,
                    lane,
                    error = %e,
                    "production-evidence consult failed; failing CLOSED \
                     (no stamp this report — the registration grace and \
                     a lawful re-stamp cover the honest case)"
                );
                std::collections::HashSet::new()
            }
        };
        for p in output_paths {
            let h = sha2::Sha256::digest(p.as_bytes());
            if !evidenced.contains(h.as_slice()) {
                warn!(
                    executor_id,
                    drv_key,
                    lane,
                    output_path = %p,
                    "refusing tenant-visibility stamp for a CA-reported \
                     output without store-recorded production evidence \
                     (no upload, no stamp; the report's other effects \
                     are unaffected)"
                );
                metrics::counter!("rio_scheduler_unevidenced_ca_output_total").increment(1);
            }
        }
        evidenced
    }

    /// The late-report classification context for `drv_key`: resident
    /// node status → Cancelled/Other; absent → Unknown. Also returns
    /// the declared output names when resident (the membership filter
    /// input for [`Self::validated_late_outputs`]).
    pub(super) fn late_node_context(
        &self,
        drv_key: &str,
    ) -> (LateNodeContext, Option<Vec<String>>) {
        let resolved: Option<DrvHash> = if self.dag.contains(drv_key) {
            Some(drv_key.into())
        } else {
            self.dag.hash_for_path(drv_key).cloned()
        };
        match resolved.as_ref().and_then(|h| self.dag.node(h)) {
            Some(state) => {
                let ctx = if state.status() == DerivationStatus::Cancelled {
                    LateNodeContext::Cancelled
                } else {
                    LateNodeContext::Other
                };
                (ctx, Some(state.output_names.to_vec()))
            }
            None => (LateNodeContext::Unknown, None),
        }
    }

    /// The un-admitted completion intake (bug_077): every sender that
    /// is NOT already behind a [`rio_evidence_kernel::pull::fold_report`]
    /// admission routes here —
    /// the `ProcessCompletion` command arm (production: the
    /// append-failure redelivery echo; tests: the stream-era helper
    /// senders). The shim re-runs the SAME kernel admission law the
    /// pull report intake uses, then dispatches: `Process` →
    /// [`Self::handle_admitted_completion`] (the witness-taking body),
    /// `AckIgnore` → the late-report lane ([`LateReportEffect`]).
    ///
    /// Admission inputs, by what the world provides:
    /// - the DAG node names an open attempt (`exec_id`): the DURABLE
    ///   attempt row carries the facts (`assignment_active`,
    ///   recorded/terminal) — identical to the pull intake's fold; a
    ///   resolvable-but-absent attempt (never-pulled/superseded exec)
    ///   folds over honest `(false, false)` inputs;
    /// - no attempt named (`exec_id == None`): the in-memory
    ///   assignment view IS the assignment ledger (the stream-era
    ///   `ProcessCompletion` worlds have no durable assignments) —
    ///   `assignment_active := status ∈ {Assigned, Running}`, nothing
    ///   classified.
    ///
    /// A DB read failure drops the redelivery (warn): the echo's
    /// retry cap and the backstop sweep remain the safety net — the
    /// same family as the give-up cell in
    /// [`Self::requeue_failure_completion`].
    #[instrument(skip(self, result), fields(executor_id = %executor_id, drv_key = %drv_key))]
    pub(super) async fn handle_completion(
        &mut self,
        executor_id: &ExecutorId,
        // gRPC layer passes CompletionReport.drv_path; may be a drv_hash in tests.
        drv_key: &str,
        result: rio_proto::types::BuildResult,
        (peak_memory_bytes, peak_cpu_cores): (u64, f64),
        (node_name, hw_class): (Option<String>, Option<String>),
        (final_resources, final_line_count): (Option<rio_proto::types::ResourceUsage>, u64),
    ) {
        use rio_evidence_kernel::pull::{ReportAdmission, fold_report};
        // Two-key resolve (hash or path), read-only — the body keeps
        // its own resolve for its internal flow.
        let drv_hash: Option<DrvHash> = if self.dag.contains(drv_key) {
            Some(drv_key.into())
        } else {
            self.dag.hash_for_path(drv_key).cloned()
        };
        let Some(drv_hash) = drv_hash else {
            // Round-9 WO-S1-1 (W9-C): the unknown-derivation report
            // folds through the late-report chokepoint instead of a
            // pre-classifier discard — a BUILT report for an EVICTED
            // cancelled drv (the beyond-grace face) is registrable
            // completed work; identity cold-resolves from the durable
            // rows inside the Register applier. Non-success unknowns
            // still classify Nothing (the censused sibling) and land
            // exactly where the old early-return left them.
            let status = rio_proto::types::BuildResultStatus::try_from(result.status)
                .unwrap_or(rio_proto::types::BuildResultStatus::Unspecified);
            let outputs = Self::validated_late_outputs(
                executor_id.as_str(),
                drv_key,
                None,
                &result.built_outputs,
            );
            let effect = late_report_effect(
                None,
                LateNodeContext::Unknown,
                status,
                final_line_count,
                outputs,
            );
            if effect == LateReportEffect::Nothing {
                warn!(
                    executor_id = %executor_id,
                    key = drv_key,
                    "completion for unknown derivation, ignoring"
                );
                return;
            }
            self.apply_late_report_effect(executor_id.as_str(), drv_key, effect)
                .await;
            return;
        };
        let (admission, reporting) = match self.dag.node(&drv_hash).and_then(|s| s.exec_id) {
            Some(exec_id) => match self.db.find_attempt_by_exec_id(exec_id).await {
                Ok(Some(attempt)) => {
                    let core = attempt.core();
                    (
                        fold_report(
                            core.assignment_active,
                            core.attempt_recorded || core.attempt_terminal,
                        ),
                        // The admission exec (bug_098): the durable
                        // row whose facts the fold consumed — the
                        // identity a late-report fill may stamp.
                        Some(ReportingExec(exec_id)),
                    )
                }
                // Never-pulled or superseded exec: no active
                // assignment exists — honest fold inputs, AckIgnore
                // by the kernel law; a ghost exec is never stamped.
                Ok(None) => (fold_report(false, false), None),
                Err(e) => {
                    warn!(
                        drv_hash = %drv_hash, %exec_id, error = %e,
                        "completion admission lookup failed; dropping the delivery \
                         (the redelivery cap and the backstop sweep remain)"
                    );
                    return;
                }
            },
            // No durable attempt named by the node: fold over the
            // in-memory assignment view (see the method doc). No
            // exec, no fill identity.
            None => {
                let in_memory_active = self.dag.node(&drv_hash).is_some_and(|s| {
                    matches!(
                        s.status(),
                        DerivationStatus::Assigned | DerivationStatus::Running
                    )
                });
                (fold_report(in_memory_active, false), None)
            }
        };
        match admission {
            ReportAdmission::Process(admission) => {
                self.handle_admitted_completion(
                    admission,
                    executor_id,
                    drv_key,
                    result,
                    (peak_memory_bytes, peak_cpu_cores),
                    (node_name, hw_class),
                    (final_resources, final_line_count),
                )
                .await;
            }
            ReportAdmission::AckIgnore => {
                debug!(
                    drv_hash = %drv_hash,
                    executor_id = %executor_id,
                    "duplicate/late completion acknowledged-and-ignored (un-admitted intake)"
                );
                let status = rio_proto::types::BuildResultStatus::try_from(result.status)
                    .unwrap_or(rio_proto::types::BuildResultStatus::Unspecified);
                let (ctx, declared) = self.late_node_context(drv_hash.as_str());
                let outputs = Self::validated_late_outputs(
                    executor_id.as_str(),
                    drv_hash.as_str(),
                    declared.as_deref(),
                    &result.built_outputs,
                );
                let effect = late_report_effect(reporting, ctx, status, final_line_count, outputs);
                self.apply_late_report_effect(executor_id.as_str(), drv_hash.as_str(), effect)
                    .await;
            }
        }
    }

    /// The admitted completion body (bug_077): takes the kernel's
    /// [`rio_evidence_kernel::pull::ProcessAdmission`] witness BY
    /// VALUE (`#[must_use]`, non-Clone, fold-mint-only), so every arm
    /// inside is provably behind the Process gate — logic intended
    /// for late/closed-assignment reports cannot compile here and
    /// lives on the [`LateReportEffect`] lane instead. The in-body
    /// `Cancelled` arm STAYS: it is the degraded-window cell honestly
    /// reachable via Process (durable close not yet landed, in-memory
    /// already Cancelled); both arms call the same epilogue
    /// chokepoint.
    #[instrument(skip(self, result, _admission), fields(executor_id = %executor_id, drv_key = %drv_key))]
    #[expect(
        clippy::too_many_arguments,
        reason = "the admission witness rides ahead of the ProcessCompletion wire surface"
    )]
    pub(super) async fn handle_admitted_completion(
        &mut self,
        // Consumed by value: one admission admits one processing pass.
        _admission: rio_evidence_kernel::pull::ProcessAdmission,
        executor_id: &ExecutorId,
        // gRPC layer passes CompletionReport.drv_path; may be a drv_hash in tests.
        drv_key: &str,
        result: rio_proto::types::BuildResult,
        // CompletionReport resource fields. 0 = worker had no signal
        // (build failed before cgroup populated). Converted to None
        // before the DB write so the EMA isn't dragged toward zero.
        //
        // Tuple to stay under clippy's 7-arg limit. Both are
        // "resource measurements from the cgroup" with identical
        // zero-means-no-signal semantics; unpacked immediately.
        (peak_memory_bytes, peak_cpu_cores): (u64, f64),
        // CompletionReport.{node_name, hw_class} (downward API). Tupled
        // — both are pod-identity stamps that flow straight to
        // build_samples; bundling stays under clippy's 7-arg limit.
        (node_name, hw_class): (Option<String>, Option<String>),
        // CompletionReport's final-state metadata, tupled to stay under
        // clippy's 7-arg limit. `final_resources` — builder's last
        // cgroup-poll snapshot, feeds build_samples ADR-023 columns,
        // None = old executor. `final_line_count` — total log lines
        // emitted, feeds `drv_executions.final_line_count` at terminal,
        // 0 = not reported.
        (final_resources, final_line_count): (Option<rio_proto::types::ResourceUsage>, u64),
    ) {
        // Arch#13: proto→domain at the actor boundary. Status
        // normalization (raw i32 → enum, unknown → Unspecified) happens
        // inside `domain::BuildResult::from`; everything downstream
        // matches on the typed enum and reads `SystemTime` instead of
        // `prost_types::Timestamp`. `ActorCommand::ProcessCompletion`
        // stays proto-typed so `actor/tests/` keeps constructing it
        // unchanged (b03 reconciles post-integration).
        // merged_bug_003: capture the pristine wire payload for possible
        // re-delivery BEFORE the domain conversion consumes it.
        // Failure-gated: the Built/Substituted/AlreadyValid hot path
        // never clones (status peek mirrors the domain normalization —
        // unknown discriminants are Unspecified, a failure route).
        let wire_status = rio_proto::types::BuildResultStatus::try_from(result.status)
            .unwrap_or(rio_proto::types::BuildResultStatus::Unspecified);
        // sh-012 D4 cores axis: the compute-bound corroborant.
        // Extracted before `final_resources` is moved into the echo
        // (failure paths) — `Option<f64>` is Copy.
        let cpu_seconds_total = final_resources.as_ref().and_then(|r| r.cpu_seconds_total);
        let mut echo = (!matches!(
            wire_status,
            rio_proto::types::BuildResultStatus::Built
                | rio_proto::types::BuildResultStatus::Substituted
                | rio_proto::types::BuildResultStatus::AlreadyValid
        ))
        .then(|| super::CompletionEcho {
            result: result.clone(),
            peak_memory_bytes,
            peak_cpu_cores,
            node_name: node_name.clone(),
            hw_class: hw_class.clone(),
            final_resources,
            final_line_count,
        });
        let mut result: crate::domain::BuildResult = result.into();
        // Threat model: builders are untrusted. `built_outputs.
        // output_path` reaches PG `realisations` (ca_insert_
        // realisations) and `path_tenants` (state.output_paths →
        // upsert_path_tenants_for) and the gateway reads both for
        // client-facing responses. Filter to valid store paths HERE,
        // at the boundary, so every consumer sees only well-formed
        // data. `ca_cutoff_compare`'s `is_empty()` guard becomes
        // defense-in-depth.
        result.built_outputs.retain(|o| {
            if rio_nix::store_path::StorePath::parse(&o.output_path).is_ok() {
                true
            } else {
                warn!(
                    executor_id = %executor_id,
                    output_name = %o.output_name,
                    output_path = %o.output_path,
                    "dropping malformed worker-supplied output_path"
                );
                metrics::counter!("rio_scheduler_malformed_built_output_total").increment(1);
                false
            }
        });
        let status = result.status;

        // Resolve drv_key (which may be a drv_path or a drv_hash) to drv_hash.
        // Boundary: construct the typed DrvHash ONCE here, then pass &DrvHash
        // to all internal handlers. The gRPC layer sends raw &str; internal
        // functions after this point use the newtype.
        let drv_hash: DrvHash = if self.dag.contains(drv_key) {
            drv_key.into()
        } else if let Some(h) = self.dag.hash_for_path(drv_key).cloned() {
            h
        } else {
            // Drv not in DAG (reaped after build-terminal, or truly
            // unknown). Nothing to free here in pull mode: there is no
            // per-executor in-memory entry, and the durable open
            // attempt was already consumed on the ReportOutcome path.
            // The WARN includes executor_id so the race is traceable
            // in logs.
            //
            // Round-9 WO-S1-1 (W9-C): a SUCCESS-class report still
            // classifies through the late-report chokepoint (the
            // beyond-grace registration face) — the shape filter above
            // already ran; identity cold-resolves in the applier.
            let effect = late_report_effect(
                None,
                LateNodeContext::Unknown,
                wire_status,
                final_line_count,
                result.built_outputs.clone(),
            );
            if effect == LateReportEffect::Nothing {
                warn!(
                    executor_id = %executor_id,
                    key = drv_key,
                    "completion for unknown derivation, ignoring"
                );
                return;
            }
            self.apply_late_report_effect(executor_id.as_str(), drv_key, effect)
                .await;
            return;
        };
        let drv_hash = &drv_hash;

        // The stream-era post-completion capacity bookkeeping that
        // lived here (clear `worker.running_build` before any
        // early-return, mark the one-shot executor draining so the
        // freed slot was not re-assigned — I-042 / I-188) retired with
        // the placement layer: there is no dispatch decision left to
        // protect. Pull-mode executors are never in the executors map;
        // the pod exits after its report and the Job is reaped by the
        // controller.

        // Find the derivation in the DAG
        let Some(state) = self.dag.node(drv_hash) else {
            // Same W9-C fold as the resolve-failure arm above: the
            // hash resolved but the node is gone — a success-class
            // report is still registrable completed work.
            let effect = late_report_effect(
                None,
                LateNodeContext::Unknown,
                wire_status,
                final_line_count,
                result.built_outputs.clone(),
            );
            if effect == LateReportEffect::Nothing {
                warn!(drv_hash = %drv_hash, "completion for unknown derivation, ignoring");
                return;
            }
            self.apply_late_report_effect(executor_id.as_str(), drv_hash.as_str(), effect)
                .await;
            return;
        };
        // r[impl sched.completion.output-membership]
        // Threat-model boundary, part 2: now that `state` is available,
        // bound `built_outputs` to the scheduler-trusted `output_names`
        // (parsed from the .drv at DAG-merge time). The format-filter
        // above runs before `state` is resolved and validates path
        // SHAPE only — it does not check membership or cardinality. A
        // compromised worker reporting on its own assigned drv could
        // otherwise stuff ~30k fabricated entries (4MB tonic limit ÷
        // ~130B/entry). Consequence pricing is PER-CONSUMER (bug_138,
        // RC-2(iii)) and lives in the taint-to-consumer census
        // (db/live_pins.rs `worker_report_taint_sinks_pinned`): the
        // sinks are GC retention (`path_tenants` pinning), the
        // VISIBILITY axis (`own_built_projection` → the I-217 verdict
        // — the consequence this comment's pre-bug_138 form omitted),
        // the gateway-facing realisations, the client-facing
        // completed-event paths, and the sequential
        // `insert_realisation` loop (~150s actor stall — same I-139
        // shape called out at the cascade-dispatch comment). After
        // this retain, `built_outputs.len() ≤ output_names.len()`;
        // the part-3 retain below bounds the path VALUES.
        let declared = &state.output_names;
        let mut seen: HashSet<String> = HashSet::with_capacity(declared.len());
        result.built_outputs.retain(|o| {
            if !declared.contains(&o.output_name) {
                warn!(
                    executor_id = %executor_id,
                    drv_hash = %drv_hash,
                    output_name = %o.output_name,
                    "dropping worker-supplied output not declared by derivation"
                );
                metrics::counter!("rio_scheduler_undeclared_built_output_total").increment(1);
                return false;
            }
            // Dedup by output_name (keep first). `insert` returns
            // false on dup → drop.
            seen.insert(o.output_name.clone())
        });
        // bug_138, threat-model boundary part 3 — the MEMBERSHIP law
        // (the pre-campaign admitted half, closed with the Register
        // half in the same commit): the name retain above bounds
        // cardinality; this bounds the VALUE. Without it, a worker on
        // its own assigned drv reports another tenant's EXISTING path
        // as its output — no upload occurs, so the store's PutPath
        // `path ∈ claims.expected_outputs` check never runs — and the
        // path lands in `state.output_paths` → `path_tenants`
        // (BuiltLocally) → the store's `own_built_projection` reads
        // owned=true → I-217 flips the victim path Visible for this
        // report's tenants. The expected set is dispatch-minted and
        // claims-signed (dispatch.rs); deferred-IA nodes carry their
        // post-resolve real paths here by report time (the dispatch
        // overwrite runs before any assignment exists). Floating-CA
        // is exempt under EXACTLY the claims-mint predicate — see
        // [`Self::retain_expected_members`].
        // r[impl sched.trust.report-membership+4]
        // (De Morgan of NOT(is_ca AND NOT is_fixed_output) — the
        // non-exempt face.)
        if !state.ca.is_ca || state.is_fixed_output {
            Self::retain_expected_members(
                executor_id.as_str(),
                drv_key,
                "admitted",
                &state.expected_output_paths,
                &mut result.built_outputs,
            );
        }
        let current_status = state.status();

        // r[impl sched.completion.idempotent]
        // Idempotency: completed -> completed is a no-op
        if current_status == DerivationStatus::Completed {
            debug!(drv_hash = %drv_hash, "duplicate completion report, ignoring");
            return;
        }

        // Cancelled: scheduler transitioned BEFORE the pod observed the
        // cancel (build.rs:cancel_build_derivations), so the executor's
        // Cancelled report finds the derivation already in this state.
        // Expected; capacity was freed above. The drv_executions row was
        // stamped by terminal_log_epilogue at cancel time -- with
        // final_line_count = None, because no report existed yet.
        //
        // merged_bug_294 / bug_077: this in-body arm is the
        // DEGRADED-WINDOW cell -- honestly reachable via Process only
        // while the durable close has not yet landed but the
        // in-memory state is already Cancelled (an outbox-latched
        // cancel persist). The common production cell (durable close
        // landed, report folds AckIgnore) rides the late-report lane
        // instead; BOTH arms route the same typed effect through the
        // same epilogue chokepoint, and the stamp SQL's equal-status
        // COALESCE guard makes the double coverage idempotent.
        if current_status == DerivationStatus::Cancelled {
            // The Cancelled node's carrier is the cancelled attempt's
            // OWN exec (the carrier clears on terminal EXIT, never
            // entry) — the degraded-window fill stamps its own row
            // (bug_098). A success-class report in this window is the
            // within-grace REGISTRATION face (round-9 WO-S1-1): the
            // shape AND declared-membership filters above already ran,
            // so `result.built_outputs` is the validated payload.
            let reporting = self
                .dag
                .node(drv_hash)
                .and_then(|s| s.exec_id)
                .map(ReportingExec);
            let effect = late_report_effect(
                reporting,
                LateNodeContext::Cancelled,
                wire_status,
                final_line_count,
                result.built_outputs.clone(),
            );
            self.apply_late_report_effect(executor_id.as_str(), drv_hash.as_str(), effect)
                .await;
            debug!(drv_hash = %drv_hash, executor_id = %executor_id,
                   "cancelled completion report (expected after a cancel)");
            return;
        }

        // Only process completions for assigned/running derivations
        if !matches!(
            current_status,
            DerivationStatus::Assigned | DerivationStatus::Running
        ) {
            warn!(
                drv_hash = %drv_hash,
                current_status = %current_status,
                "completion for derivation not in assigned/running state, ignoring"
            );
            return;
        }

        // r[impl sched.completion.idempotent]
        // Stale-report guard: if this completion is from a worker that no
        // longer owns the derivation (reassigned after executor loss),
        // drop it. The current assigned_executor's report is authoritative.
        if let Some(assigned) = &state.assigned_executor
            && assigned != executor_id
        {
            debug!(
                drv_hash = %drv_hash,
                stale_worker = %executor_id,
                current_worker = %assigned,
                "dropping stale completion report"
            );
            return;
        }

        // Report-carried context for the failure handlers. The
        // line-count sentinel conversion matches the success path's
        // stamp below: 0 ("not reported") and out-of-range worker
        // values become None, never a literal/negative count.
        let report_line_count = i64::try_from(final_line_count).ok().filter(|n| *n > 0);
        match status {
            rio_proto::types::BuildResultStatus::Built
            | rio_proto::types::BuildResultStatus::Substituted
            | rio_proto::types::BuildResultStatus::AlreadyValid => {
                self.handle_success_completion(
                    drv_hash,
                    &result,
                    executor_id,
                    (peak_memory_bytes, peak_cpu_cores),
                    (node_name, hw_class),
                    (final_resources, final_line_count),
                )
                .await;
            }
            rio_proto::types::BuildResultStatus::TransientFailure => {
                // Build ran, exited non-zero. Counts toward poison — 3
                // workers all seeing this means it's not actually transient.
                let handling = self
                    .handle_transient_failure(
                        drv_hash,
                        executor_id,
                        failure_ctx_for(status, &result, report_line_count, peak_memory_bytes),
                    )
                    .await;
                match handling {
                    FailureHandling::Handled => {
                        self.attempt_record_retries.remove(drv_hash);
                    }
                    FailureHandling::RecordFailed => {
                        if let Some(echo) = echo.take() {
                            self.requeue_failure_completion(executor_id, drv_hash, echo);
                        }
                    }
                }
            }
            // r[impl sched.retry.per-executor-budget+4]
            rio_proto::types::BuildResultStatus::InfrastructureFailure => {
                // Worker-local problem (FUSE EIO, cgroup setup fail, OOM-
                // kill of the build process). Not the build's fault. Retry
                // WITHOUT inserting into failed_builders.
                let handling = self
                    .handle_infrastructure_failure(
                        drv_hash,
                        executor_id,
                        failure_ctx_for(status, &result, report_line_count, peak_memory_bytes),
                    )
                    .await;
                match handling {
                    FailureHandling::Handled => {
                        self.attempt_record_retries.remove(drv_hash);
                    }
                    FailureHandling::RecordFailed => {
                        if let Some(echo) = echo.take() {
                            self.requeue_failure_completion(executor_id, drv_hash, echo);
                        }
                    }
                }
            }
            // sh-012 (E3a): the daemon's heuristic exit≠0 / unclassified
            // — verdict CAN vary by executor. Threshold-gated requeue
            // (mirrors E1); only poisons when ≥N distinct executors
            // agree.
            rio_proto::types::BuildResultStatus::ExecutorVariantFailure => {
                let handling = self
                    .handle_executor_variant_failure(
                        drv_hash,
                        executor_id,
                        failure_ctx_for(status, &result, report_line_count, peak_memory_bytes),
                        cpu_seconds_total,
                    )
                    .await;
                match handling {
                    FailureHandling::Handled => {
                        self.attempt_record_retries.remove(drv_hash);
                    }
                    FailureHandling::RecordFailed => {
                        if let Some(echo) = echo.take() {
                            self.requeue_failure_completion(executor_id, drv_hash, echo);
                        }
                    }
                }
            }
            // E3b: derivation-INTRINSIC permanent statuses — the verdict
            // CANNOT vary by executor. First-observation poison.
            // PermanentFailure stays here for synthetically-stamped
            // statuses (NoSubstituters → PermanentFailure; the
            // builder→scheduler path now sends ExecutorVariantFailure
            // for the daemon's PermanentFailure/MiscFailure).
            rio_proto::types::BuildResultStatus::PermanentFailure
            | rio_proto::types::BuildResultStatus::CachedFailure
            | rio_proto::types::BuildResultStatus::DependencyFailed
            | rio_proto::types::BuildResultStatus::LogLimitExceeded
            | rio_proto::types::BuildResultStatus::OutputRejected
            // NotDeterministic: nix --check failed. Retrying doesn't
            // help — the nondeterminism is in the build itself.
            | rio_proto::types::BuildResultStatus::NotDeterministic
            // InputRejected: corrupt/invalid .drv OR wrong-kind misroute
            // (deterministic re-dispatch on persisted is_fixed_output).
            // Retry can't help.
            | rio_proto::types::BuildResultStatus::InputRejected => {
                let handling = self
                    .handle_permanent_failure(
                        drv_hash,
                        executor_id,
                        failure_ctx_for(status, &result, report_line_count, peak_memory_bytes),
                    )
                    .await;
                match handling {
                    FailureHandling::Handled => {
                        self.attempt_record_retries.remove(drv_hash);
                    }
                    FailureHandling::RecordFailed => {
                        if let Some(echo) = echo.take() {
                            self.requeue_failure_completion(executor_id, drv_hash, echo);
                        }
                    }
                }
            }
            // TimedOut: I-200 bumps resource_floor.deadline and retries on
            // a larger class (longer activeDeadlineSeconds), bounded by
            // max_timeout_retries. After the cap → Cancelled (terminal,
            // retriable-on-resubmit) — same inputs on the LARGEST class
            // → same timeout, so further auto-retry is a storm.
            // Poisoned's 24h TTL is way too aggressive for a timeout.
            rio_proto::types::BuildResultStatus::TimedOut => {
                let handling = self
                    .handle_timeout_failure(
                        drv_hash,
                        executor_id,
                        failure_ctx_for(status, &result, report_line_count, peak_memory_bytes),
                    )
                    .await;
                match handling {
                    FailureHandling::Handled => {
                        self.attempt_record_retries.remove(drv_hash);
                    }
                    FailureHandling::RecordFailed => {
                        if let Some(echo) = echo.take() {
                            self.requeue_failure_completion(executor_id, drv_hash, echo);
                        }
                    }
                }
            }
            rio_proto::types::BuildResultStatus::Cancelled => {
                // The early-return at :661 checks the DAG status, NOT
                // the worker-reported result.status. An untrusted
                // worker sending status=Cancelled while the DAG is
                // Assigned/Running falls through here; this report
                // consumes the attempt, so nothing else would recover
                // the derivation. Treat as infra (worker-protocol
                // violation, bounded by infra_count) — not transient
                // (NOT a build-determinism signal).
                warn!(
                    drv_hash = %drv_hash, executor_id = %executor_id,
                    "unsolicited Cancelled from worker (DAG not Cancelled) \
                     — treating as infrastructure failure"
                );
                let handling = self
                    .handle_infrastructure_failure(
                        drv_hash,
                        executor_id,
                        failure_ctx_for(status, &result, report_line_count, peak_memory_bytes),
                    )
                    .await;
                match handling {
                    FailureHandling::Handled => {
                        self.attempt_record_retries.remove(drv_hash);
                    }
                    FailureHandling::RecordFailed => {
                        // Re-deliver with the original worker-reported
                        // status so the routing arm re-runs this same
                        // treat-as-infra path (and re-synthesizes the
                        // message).
                        if let Some(echo) = echo.take() {
                            self.requeue_failure_completion(executor_id, drv_hash, echo);
                        }
                    }
                }
            }
            rio_proto::types::BuildResultStatus::Unspecified => {
                warn!(
                    drv_hash = %drv_hash,
                    status = ?result.status,
                    "unknown build result status, treating as transient failure"
                );
                let handling = self
                    .handle_transient_failure(
                        drv_hash,
                        executor_id,
                        failure_ctx_for(status, &result, report_line_count, peak_memory_bytes),
                    )
                    .await;
                match handling {
                    FailureHandling::Handled => {
                        self.attempt_record_retries.remove(drv_hash);
                    }
                    FailureHandling::RecordFailed => {
                        if let Some(echo) = echo.take() {
                            self.requeue_failure_completion(executor_id, drv_hash, echo);
                        }
                    }
                }
            }
        }

        // Newly-ready dependents: complete/substitute any whose outputs
        // already exist (the store short-circuit; delivery itself is
        // pull — the controller observes Ready via GetSpawnIntents).
        self.sweep_ready_cached().await;
    }

    pub(super) async fn handle_success_completion(
        &mut self,
        drv_hash: &DrvHash,
        result: &crate::domain::BuildResult,
        executor_id: &ExecutorId,
        // Same tuple pattern as handle_completion — clippy 7-arg limit.
        (peak_memory_bytes, peak_cpu_cores): (u64, f64),
        (node_name, hw_class): (Option<String>, Option<String>),
        (final_resources, final_line_count): (Option<rio_proto::types::ResourceUsage>, u64),
    ) {
        // I-140: per-phase timing. Same pattern as merge.rs phase!().
        let t_total = std::time::Instant::now();
        let mut t_phase = std::time::Instant::now();
        macro_rules! phase {
            ($name:literal) => {
                tracing::trace!(elapsed = ?t_phase.elapsed(), phase = $name, "completion phase");
                t_phase = std::time::Instant::now();
            };
        }
        // Transition to completed
        if let Some(state) = self.dag.node_mut(drv_hash) {
            state.ensure_running();
            if let Err(e) = state.transition(DerivationStatus::Completed) {
                // Worker reported success but the in-memory state machine rejected
                // the transition (e.g., derivation was cascaded to DependencyFailed
                // or reset by the establishment sweep in a race). The build result
                // is lost; downstream derivations will never be released.
                error!(
                    drv_hash = %drv_hash,
                    executor_id = %executor_id,
                    current_state = ?state.status(),
                    error = %e,
                    "worker reported success but Running->Completed transition rejected; build will hang"
                );
                metrics::counter!("rio_scheduler_transition_rejected_total", "to" => "completed")
                    .increment(1);
                return;
            }

            // Store output paths from built_outputs
            state.output_paths = result
                .built_outputs
                .iter()
                .map(|o| o.output_path.clone())
                .collect();
        }

        // bug_132/bug_155 — the floating-CA face's production-evidence
        // gate (the membership law's CA face, the De-Morgan complement
        // of the `retain_expected_members` arm above): consult ONCE
        // here, BEFORE the CA bookkeeping, so the same evidenced set
        // bounds the realisation insert AND the tenant-visibility
        // stamp below. The consult runs on EVERY floating-CA path — quantifier: census(ca_evidence_reader_census) —
        // an untenanted report (no attributed cohort) consults with
        // the empty cohort and gets the EMPTY evidence set, which
        // refuses every realisation insert (the GLOBAL table's
        // consumers are tenant-unscoped, so "no cohort" means
        // "refuse", never "stand down"; only the tenant-keyed STAMP
        // is vacuous there). `None` = the bounded faces only
        // (IA/fixed-output: the expected-set retain already ran).
        let ca_evidence: Option<std::collections::HashSet<Vec<u8>>> = match self.dag.node(drv_hash)
        {
            Some(state) if state.ca.is_ca && !state.is_fixed_output => {
                let tenant_ids: Vec<Uuid> = state.attributed_tenants(&self.builds).collect();
                let paths = state.output_paths.clone();
                Some(
                    self.ca_production_evidence(
                        executor_id.as_str(),
                        drv_hash.as_str(),
                        "admitted",
                        &paths,
                        &tenant_ids,
                    )
                    .await,
                )
            }
            _ => None,
        };

        let skipped_interested = self
            .complete_ca_bookkeeping(
                drv_hash,
                &result.built_outputs,
                match &ca_evidence {
                    Some(set) => CaRealisationEvidence::Evidenced(set),
                    None => CaRealisationEvidence::ExpectedSetBounded,
                },
            )
            .await;

        // I-209: persist_status(.., Completed, ..) now also closes the
        // active assignments row (db-layer fold) — the explicit
        // update_assignment_status that lived here is redundant.
        self.persist_status(drv_hash, DerivationStatus::Completed, None)
            .await;
        self.unpin_best_effort(drv_hash).await;

        // r[impl sched.sla.hw-ref-seconds]
        // For the critical_path_accuracy metric below: est_duration is
        // REF-seconds, result.duration() is wall. Normalize wall→ref via
        // the reporting worker's `α·factor[h]` so the ratio is
        // dimensionally consistent (same normalization
        // record_build_sample applies to its T sample). Computed before
        // the move into record_build_sample. Per-key α from the cache;
        // unfitted keys (and keys whose state is gone) get UNIFORM.
        let alpha = self
            .dag
            .node(drv_hash)
            .and_then(|s| Some((s.pname.clone()?, s.system.clone(), s)))
            .map(|(pname, system, s)| crate::sla::types::ModelKey {
                pname,
                system,
                tenant: s
                    .attributed_tenant(&self.builds)
                    .map(|u| u.to_string())
                    .unwrap_or_default(),
            })
            .map_or(crate::sla::alpha::UNIFORM, |k| {
                self.sla_estimator.cached_alpha(&k)
            });
        let hw_factor = self.sla_estimator.hw_factor(hw_class.as_deref(), alpha);

        self.record_build_sample(
            drv_hash,
            result,
            (peak_memory_bytes, peak_cpu_cores),
            (node_name, hw_class),
            final_resources,
        )
        .await;

        // Build the per-drv terminal event ONCE; release_downstream
        // emits it per interested build AFTER Progress (nom ordering —
        // r[impl gw.activity.progress-before-stop]).
        let output_paths: Vec<String> = self
            .dag
            .node(drv_hash)
            .map(|s| s.output_paths.clone())
            .unwrap_or_default();
        let interested_builds = self.get_interested_builds(drv_hash);
        let completed_event = rio_proto::types::build_event::Event::Derivation(
            rio_proto::types::DerivationEvent::completed(
                self.dag.path_or_hash_fallback(drv_hash),
                output_paths,
            ),
        );

        // r[impl sched.gc.path-tenants-upsert]
        // Signed Q2: worker-built — locally produced bytes; all
        // interested tenants lawful. bug_132: on the floating-CA face
        // the lawful paths are additionally bounded by the evidence
        // consult above (no upload, no stamp) — the typed witness
        // carries the evidenced subset into the Q2 funnel.
        let stamp_provenance = match &ca_evidence {
            Some(evidenced) => {
                crate::db::live_pins::StampProvenance::BuiltLocallyEvidenced(evidenced.clone())
            }
            None => crate::db::live_pins::StampProvenance::BuiltLocally,
        };
        self.upsert_path_tenants_for(drv_hash, &stamp_provenance)
            .await;

        // Update ancestor priorities: this node is now terminal, so it
        // no longer contributes to its parents' max-child-priority.
        // Done BEFORE releasing downstream because the newly-ready
        // nodes will be pushed to the queue by priority, and their
        // priority is already correct from compute_initial at merge
        // time. The ancestors that change are NOT newly-ready (they
        // were already in the queue or beyond) — the queue's lazy
        // invalidation handles re-pushing them if needed.
        //
        // Also emit the accuracy metric: how close was our estimate?
        // r[impl sched.sla.hw-ref-seconds]
        // `est_duration` is REFERENCE-seconds (sla::wall_estimate
        // returns t_min().0); `actual` is wall-clock. Normalize the
        // wall-clock by this completion's hw_factor so the ratio is
        // ref/ref and "1.0=perfect" holds across a heterogeneous
        // fleet (same as `score_completion` at :1500-1506).
        if let Some(state) = self.dag.node(drv_hash)
            && state.sched.est_duration > 0.0
            && let Some(actual) = result.duration()
        {
            let actual_ref = actual.as_secs_f64() * hw_factor;
            if actual_ref > 0.0 {
                metrics::histogram!("rio_scheduler_critical_path_accuracy")
                    .record(actual_ref / state.sched.est_duration);
            }
        }
        crate::critical_path::update_ancestors(&mut self.dag, drv_hash);
        phase!("4-update-ancestors");

        self.release_downstream(
            drv_hash,
            &interested_builds,
            skipped_interested,
            Some(completed_event),
        )
        .await;
        phase!("5-newly-ready+per-build-counts");

        // Stamp the drv_executions row AFTER the Completed event has
        // gone out (now emitted inside release_downstream, after
        // Progress).
        // r[impl sched.merge.exec-correlation+8]
        // 0 → None: the proto's "not reported" sentinel must become SQL
        // NULL, not a literal 0 — the store's completeness predicate
        // treats NULL as "can't judge yet" but 0 as "a zero-line log is
        // complete". try_from, NOT `as`: the count is a worker-supplied
        // u64, and a wrapping cast of a value > i64::MAX writes a
        // NEGATIVE count — which the store's contiguity fold (`covered
        // starts at 0; complete ⇔ covered >= count`) reads as vacuously
        // complete with an EMPTY manifest, sealing the log against any
        // further append. Out of range degrades to None ("not
        // reported"), the same as every other unusable report field.
        self.terminal_log_epilogue(
            drv_hash,
            "succeeded",
            &interested_builds,
            i64::try_from(final_line_count).ok().filter(|n| *n > 0),
        );
        let _ = &mut t_phase;
        let total = t_total.elapsed();
        // IA-branch parity with the CA `info!` in `ca_insert_realisations`:
        // without this, an IA dependency's INFO-level trace is
        // `worker registered → assignment ACKed → worker disconnected`
        // with no success marker — indistinguishable from a crash.
        info!(drv_hash = %drv_hash, elapsed = ?total, "derivation built");
        if total >= std::time::Duration::from_secs(1) {
            debug!(elapsed = ?total, drv_hash = %drv_hash, "handle_success_completion total");
        }
    }

    /// Phases 2-5 of success completion for content-addressed
    /// derivations: realisation insert, cutoff-compare, cutoff-
    /// propagate cascade, realisation_deps insert. All four are
    /// no-ops for IA derivations (each phase gates on `state.ca.is_ca`
    /// / `state.ca.modular_hash` internally). Returns the union of
    /// `interested_builds` over all cascade-skipped nodes — the caller
    /// folds these into its `check_build_completion` loop so a merged
    /// build the trigger does NOT belong to still terminates.
    async fn complete_ca_bookkeeping(
        &mut self,
        drv_hash: &DrvHash,
        built_outputs: &[crate::domain::BuiltOutput],
        ca_evidence: CaRealisationEvidence<'_>,
    ) -> HashSet<Uuid> {
        self.ca_insert_realisations(drv_hash, built_outputs, ca_evidence)
            .await;
        let prior_seeds = self.ca_cutoff_compare(drv_hash, built_outputs).await;
        let skipped_interested = self.ca_cutoff_cascade(drv_hash, &prior_seeds).await;
        self.ca_insert_realisation_deps(drv_hash, built_outputs)
            .await;
        skipped_interested
    }

    /// Phase 2: realisation insert. Best-effort — PG blip degrades
    /// CA-on-CA resolve, not the build itself.
    // r[impl sched.ca.resolve+3]
    async fn ca_insert_realisations(
        &self,
        drv_hash: &DrvHash,
        built_outputs: &[crate::domain::BuiltOutput],
        // bug_155: the NON-OPTIONAL typed witness — see
        // [`CaRealisationEvidence`]. On the floating-CA face the
        // variant is `Evidenced` with the store-evidenced
        // sha256(path) set from [`Self::ca_production_evidence`]; a
        // non-member output is SKIPPED (already counted/attributed by
        // the consult's typed letter), and an EMPTY set — the
        // untenanted face — skips every output. Without this bound a
        // forged (modular_hash → victim_path) mapping becomes durable
        // truth the CA-cutoff cascade later stamps under plain
        // BuiltLocally and the tenant-unscoped consumers
        // (`query_batch` / `query_prior_realisation`) serve globally
        // — the flip one lane over. `ExpectedSetBounded` = the
        // bounded faces ONLY — quantifier: census(ca_evidence_reader_census) — (deferred-IA / fixed-output passed the
        // dispatch-minted expected-set retain); a floating-CA report
        // presenting it is refused fail-closed below.
        ca_evidence: CaRealisationEvidence<'_>,
    ) {
        // Realisation insert: for a CA derivation, write
        // `(modular_hash, output_name) → (output_path, output_hash)`
        // to PG NOW, before `find_newly_ready` below queues the
        // dependents for dispatch. The dependents' `maybe_resolve_ca`
        // → `resolve_ca_inputs` → `realisations::query_batch` reads
        // this row; without it, resolve fails with
        // `RealisationMissing` and the scheduler dispatches
        // unresolved content → worker fetches the floating-CA input's
        // `.drv` → reads `out.path() == ""` → `invalid store path ""`.
        //
        // The gateway's `wopRegisterDrvOutput` handler inserts for the
        // Nix wire-protocol path, but rio-builders don't speak wire
        // protocol — their upload is `PutPath → CompletionReport`
        // only. This insert closes the gap.
        //
        // Best-effort: PG blip → warn, don't abort completion. The
        // dependent's dispatch-unresolved → worker-fail →
        // `handle_infrastructure_failure` path retries with backoff
        // (after the companion fix), giving PG time to recover. The
        // in-mem transition to Completed already happened; a missing
        // realisation degrades CA-on-CA resolve, not the build itself.
        //
        // Gate on `is_ca || needs_resolve`: deferred-IA (`is_ca=false`,
        // `needs_resolve=true`) ALSO needs a realisation row — the
        // .drv on disk has `path=""`, so the gateway's
        // `wopQueryDerivationOutputMap` realisation-lookup is the only
        // way the client learns the post-resolve output path.
        if let Some(state) = self.dag.node(drv_hash)
            && (state.ca.is_ca || state.ca.needs_resolve)
            && let Some(modular_hash) = state.ca.modular_hash
        {
            // Log the hash in the same hex-encoding the gateway's
            // wopQueryRealisation handler uses — if nix-build's later
            // QueryRealisation finds nothing, grep both logs for
            // `drv_hash=` and compare. A mismatch = our
            // hash_derivation_modulo diverges from CppNix (the
            // maskOutputs env-masking gap was one such divergence).
            info!(
                drv_hash = %hex::encode(modular_hash),
                outputs = built_outputs.len(),
                "insert_realisation: CA build complete, writing realisations"
            );
            // bug_155: the floating-CA predicate, re-derived HERE from
            // the same node state the insert reads — the fail-closed
            // belt below never trusts the caller's claimed face.
            let floating_ca = state.ca.is_ca && !state.is_fixed_output;
            for output in built_outputs {
                // bug_132/bug_155: the evidence bound (see the
                // parameter doc) — a worker-supplied mapping for a
                // path the build's cohort never uploaded is refused
                // here exactly as at the stamp; the consult already
                // counted the letter. The EMPTY set (untenanted face:
                // no cohort ⇒ no consultable evidence — `path_tenants`
                // is tenant-keyed) refuses every path: the GLOBAL
                // table's scope demands evidence on every floating-CA
                // face, so the empty cohort REFUSES ALL — quantifier: census(untenanted_floating_ca_report_never_mints_global_realisations) — rather than
                // standing down (the cohort-keyed vacuity argument is
                // the stamp reader's; it never discharged this
                // reader).
                match ca_evidence {
                    CaRealisationEvidence::Evidenced(evidenced) => {
                        use sha2::Digest;
                        let h = sha2::Sha256::digest(output.output_path.as_bytes());
                        if !evidenced.contains(h.as_slice()) {
                            continue;
                        }
                    }
                    CaRealisationEvidence::ExpectedSetBounded => {
                        // Fail-closed belt: a floating-CA report can
                        // never ride the expected-set face (no
                        // dispatch-minted set exists for it) — a
                        // mis-constructed future lane refuses here
                        // with the same typed letter the consult
                        // mints, instead of inserting unguarded.
                        if floating_ca {
                            warn!(
                                drv_hash = %drv_hash,
                                output_name = %output.output_name,
                                output_path = %output.output_path,
                                "refusing floating-CA realisation presented without \
                                 its evidence set (fail-closed; the lane must consult \
                                 ca_production_evidence)"
                            );
                            metrics::counter!("rio_scheduler_unevidenced_ca_output_total")
                                .increment(1);
                            continue;
                        }
                    }
                }
                let Ok(output_hash): Result<[u8; 32], _> = output.output_hash.as_slice().try_into()
                else {
                    debug!(
                        drv_hash = %drv_hash,
                        output_name = %output.output_name,
                        hash_len = output.output_hash.len(),
                        "realisation insert: output_hash not 32 bytes, skipping"
                    );
                    continue;
                };
                if let Err(e) = crate::ca::insert_realisation(
                    self.db.pool(),
                    &modular_hash,
                    &output.output_name,
                    &output.output_path,
                    &output_hash,
                )
                .await
                {
                    warn!(
                        drv_hash = %drv_hash,
                        output_name = %output.output_name,
                        error = %e,
                        "realisation insert failed (best-effort; dependent resolve will retry)"
                    );
                }
            }
        }
    }

    /// Phase 3: per-output cutoff-compare. Queries `realisations` for a
    /// PRIOR build (different `modular_hash`, same `output_path`) for
    /// each output; AND-folds into `state.ca.output_unchanged`. Returns
    /// the discovered prior `(modular_hash, output_name)` seeds for
    /// [`Self::ca_cutoff_cascade`]. No-op (empty seeds, `output_unchanged`
    /// untouched) for IA / no-modular-hash.
    // r[impl sched.ca.cutoff-compare]
    async fn ca_cutoff_compare(
        &mut self,
        drv_hash: &DrvHash,
        built_outputs: &[crate::domain::BuiltOutput],
    ) -> Vec<(Vec<u8>, String)> {
        // CA early-cutoff compare: if this was a CA derivation, check
        // each output_path against the realisations table for a PRIOR
        // build (different modular_hash, same path). All-match →
        // P0252's cutoff-propagate cascade (cascade_cutoff) will skip
        // downstream builds. This hook only records the compare result
        // + the prior realisation seeds; propagation happens below.
        //
        // WHY realisation-based, not ContentLookup-based: CA
        // derivations produce IDENTICAL output_paths for identical
        // content (that's the point of CA). The previous
        // `ContentLookup(nar_hash, exclude=output_path)` approach
        // always excluded the only matching row — for CA, the self-
        // exclusion filters out the very evidence of a prior build.
        // Querying realisations by output_path with modular_hash
        // exclusion instead: two builds with different drv envs
        // (hence different modular_hash) but identical content get
        // the same path, so the exclusion leaves the prior build's
        // row visible.
        //
        // First-ever build: no prior realisation → all_matched=false
        // → no cascade. This REPLACES the verify_cutoff_candidates
        // self-match defense (bughunt-mc196) at the source.
        //
        // Best-effort: PG blip → degrade to "no cutoff" (safe —
        // downstream builds run when they could've been skipped).
        //
        // AND-fold: `all_matched` starts true and is &= each lookup.
        // A single miss means the derivation's outputs aren't
        // byte-identical to a prior build as a WHOLE, so downstream
        // can't be skipped. Per-output granularity is a later
        // refinement.
        let mut prior_seeds: Vec<(Vec<u8>, String)> = Vec::new();
        if let Some(state) = self.dag.node(drv_hash)
            && state.ca.is_ca
            && let Some(modular_hash) = state.ca.modular_hash
        {
            let mut all_matched = !built_outputs.is_empty();
            for (i, output) in built_outputs.iter().enumerate() {
                // Defense-in-depth — handle_completion already filters
                // malformed paths at the proto→domain boundary.
                if output.output_path.is_empty() {
                    debug!(
                        drv_hash = %drv_hash,
                        output_name = %output.output_name,
                        "CA cutoff-compare: empty output_path, counting as malformed"
                    );
                    metrics::counter!(
                        "rio_scheduler_ca_hash_compares_total",
                        "outcome" => "malformed"
                    )
                    .increment(1);
                    all_matched = false;
                    continue;
                }
                // Outcome labels: match/miss distinguish "prior build
                // found" vs "novel content" (both healthy). error
                // distinguishes "PG blip/timeout" (infra problem —
                // alert if rate>0). malformed catches worker-sent
                // garbage. High miss-rate is normal; high error-rate
                // means investigate PG. Cutoff semantics unchanged:
                // all non-match fold to all_matched=false → safe
                // don't-skip.
                let (matched, outcome) = match tokio::time::timeout(
                    CA_CUTOFF_LOOKUP_TIMEOUT,
                    crate::ca::query_prior_realisation(
                        self.db.pool(),
                        &output.output_path,
                        &modular_hash,
                    ),
                )
                .await
                {
                    Ok(Ok(Some(prior))) => {
                        // Found a prior build's realisation for the
                        // same path. Seed the realisation_deps walk
                        // with its (modular_hash, output_name) so
                        // the cascade can discover what was built
                        // downstream of it.
                        prior_seeds.push((prior.drv_hash.to_vec(), prior.output_name));
                        (true, "match")
                    }
                    Ok(Ok(None)) => (false, "miss"),
                    Ok(Err(e)) => {
                        debug!(drv_hash = %drv_hash, error = %e,
                               "CA cutoff-compare: prior-realisation lookup failed");
                        (false, "error")
                    }
                    Err(_elapsed) => {
                        debug!(drv_hash = %drv_hash, timeout = ?CA_CUTOFF_LOOKUP_TIMEOUT,
                               "CA cutoff-compare: prior-realisation lookup timed out");
                        (false, "error")
                    }
                };
                all_matched &= matched;
                metrics::counter!(
                    "rio_scheduler_ca_hash_compares_total",
                    "outcome" => outcome
                )
                .increment(1);
                // Short-circuit: one miss means the derivation's
                // outputs aren't byte-identical AS A WHOLE (AND-
                // fold semantics). Remaining lookups can't flip
                // `all_matched` back to true.
                if !matched {
                    let skipped = built_outputs.len() - i - 1;
                    if skipped > 0 {
                        metrics::counter!(
                            "rio_scheduler_ca_hash_compares_total",
                            "outcome" => "skipped_after_miss"
                        )
                        .increment(skipped as u64);
                    }
                    break;
                }
            }
            if let Some(state) = self.dag.node_mut(drv_hash) {
                state.ca.output_unchanged = all_matched;
            }
        }
        prior_seeds
    }

    /// Phase 4: cutoff-propagate cascade. If [`Self::ca_cutoff_compare`]
    /// set `output_unchanged=true`, verify the prior outputs still exist
    /// (GC-defense), transitively skip downstream Queued derivations,
    /// stamp/persist each skipped node, and promote any newly-Ready
    /// parents-of-Skipped. Returns the union of `interested_builds`
    /// across skipped nodes — folded into the caller's
    /// `check_build_completion` loop.
    // r[impl sched.ca.cutoff-propagate+2]
    async fn ca_cutoff_cascade(
        &mut self,
        drv_hash: &DrvHash,
        prior_seeds: &[(Vec<u8>, String)],
    ) -> HashSet<Uuid> {
        // Cascade: if the compare set ca_output_unchanged=true,
        // transitively skip downstream Queued derivations whose only
        // incomplete dep was this one.
        //
        // First-ever-build defense (bughunt-mc196): now handled at
        // the SOURCE — `query_prior_realisation` excludes by
        // modular_hash (different across builds), not output_path
        // (identical for CA). A first-ever build finds no prior
        // realisation → `ca_output_unchanged=false` → cascade never
        // fires. `verify_cutoff_candidates` adds GC-defense: prior
        // outputs must still exist in the store (FindMissingPaths).
        //
        // Placement BEFORE find_newly_ready: skipped nodes don't get
        // promoted to Ready + pushed to the dispatch queue.
        //
        // `skipped_interested`: union of interested_builds over all
        // skipped nodes. A skipped node may belong to a merged build
        // that the trigger does NOT — without unioning this into the
        // completion-check loop below, that merged build never sees
        // check_build_completion fire and hangs Active forever.
        let mut skipped_interested: HashSet<Uuid> = HashSet::new();
        if self
            .dag
            .node(drv_hash)
            .is_some_and(|s| s.ca.output_unchanged)
        {
            let verified = self.verify_cutoff_candidates(drv_hash, prior_seeds).await;
            let (skipped, cap_hit) = self
                .dag
                .cascade_cutoff(drv_hash, |h| verified.contains_key(h));
            metrics::counter!("rio_scheduler_ca_cutoff_saves_total")
                .increment(skipped.len() as u64);
            // Sum of est_duration in hw-normalized ref-seconds
            // (r[sched.sla.hw-ref-seconds]; NOT wall-clock — skipped
            // builds were never assigned, so no hw_factor exists to
            // denormalize). est_duration is the SLA estimator's
            // prediction (set at merge time from build_samples); for
            // a derivation that has never run, it's the cold-start
            // probe default. Counter not gauge: cumulative across
            // all cascades, like saves_total.
            let seconds_saved: f64 = skipped
                .iter()
                .filter_map(|h| self.dag.node(h).map(|s| s.sched.est_duration))
                .sum();
            metrics::counter!("rio_scheduler_ca_cutoff_seconds_saved")
                .increment(seconds_saved.max(0.0) as u64);
            // First pass: in-memory only, NO .await. Stamp output_
            // paths, collect realisation tuples + interested_builds.
            // The per-item PG awaits previously here (insert_
            // realisation / upsert_path_tenants_for / persist_status
            // per node) were ~3×N sequential round-trips inside the
            // single-threaded actor — at the MAX_CASCADE_NODES=1000
            // cap, ~15-20s of head-of-line blocking on heartbeats and
            // dispatch (same class as I-139 / `apply_cached_hits`).
            //
            // Stamp each skipped node with the prior build's output_
            // paths AND collect realisations keyed on THIS build's
            // modular_hash. Without the realisation, the gateway's
            // QueryRealisation for (M2_b, out) returns NotFound →
            // client gets empty outPath → assert at nix-build.cc:722.
            // The prior build's realisation is keyed on M1_b
            // (different modular_hash) so the gateway can't find it
            // without this bridge row.
            let mut realisations: Vec<([u8; 32], String, String, [u8; 32])> = Vec::new();
            // Per-skipped (drv_path, output_paths, interested_builds)
            // for the DerivationCached emission below.
            let mut cached_emit: Vec<(String, Vec<String>, Vec<Uuid>)> = Vec::new();
            for hash in &skipped {
                if let Some(prior_outs) = verified.get(hash) {
                    if let Some(state) = self.dag.node_mut(hash) {
                        state.output_paths =
                            prior_outs.iter().map(|o| o.output_path.clone()).collect();
                    }
                    if let Some(state) = self.dag.node(hash)
                        && let Some(modular) = state.ca.modular_hash
                    {
                        for o in prior_outs {
                            realisations.push((
                                modular,
                                o.output_name.clone(),
                                o.output_path.clone(),
                                o.output_hash,
                            ));
                        }
                    }
                }
                if let Some(state) = self.dag.node(hash) {
                    skipped_interested.extend(state.interested_builds.iter().copied());
                    cached_emit.push((
                        self.dag.path_or_hash_fallback(hash),
                        state.output_paths.clone(),
                        state.interested_builds.iter().copied().collect(),
                    ));
                }
                debug!(drv_hash = %hash, trigger = %drv_hash,
                       "CA cutoff: skipped (output already in store)");
            }
            // Batch the PG writes — three round-trips total instead
            // of 3×N. All best-effort (log on Err, never block the
            // in-mem transitions).
            if let Err(e) = crate::ca::insert_realisation_batch(self.db.pool(), &realisations).await
            {
                warn!(
                    n_rows = realisations.len(),
                    error = %e,
                    "CA cutoff: batch realisation insert for skipped nodes failed \
                     (best-effort; gateway QueryRealisation may return empty)"
                );
            }
            // r[impl sched.gc.path-tenants-upsert]
            // Skipped nodes' output_paths (from the prior build's
            // realization, stamped above) need tenant attribution —
            // the build's tenant wants them retained, same as if the
            // node had actually built them. Signed Q2 provenance:
            // BuiltLocally — the realisation row proves the bytes were
            // produced by a prior build in THIS cluster; the cutoff's
            // FindMissingPaths verify is ANONYMOUS (no tenant header;
            // only missing_paths is consumed — physical presence, not
            // a per-tenant view), so it could never authorize a
            // ProbedBy stamp.
            self.upsert_path_tenants_for_batch(
                &skipped
                    .iter()
                    .map(|h| {
                        (
                            h.clone(),
                            crate::db::live_pins::StampProvenance::BuiltLocally,
                        )
                    })
                    .collect::<Vec<_>>(),
            )
            .await;
            let skipped_refs: Vec<&str> = skipped.iter().map(|h| h.as_str()).collect();
            self.persist_status_batch(&skipped_refs, DerivationStatus::Skipped)
                .await;
            // r[impl sched.event.derivation-terminal]
            // Skipped is a terminal cached-equivalent transition: emit
            // DerivationCached + bump cached_count for each interested
            // build (matches `apply_cached_hits` / `complete_ready_
            // from_store`). Without this, WatchBuild clients see
            // skipped nodes frozen at Queued and the nix client's
            // SetExpected count doesn't match the Cached events
            // received.
            for (drv_path, output_paths, interested) in cached_emit {
                let event = rio_proto::types::build_event::Event::Derivation(
                    rio_proto::types::DerivationEvent::cached(drv_path, output_paths),
                );
                for build_id in interested {
                    self.events.emit(build_id, event.clone());
                    // r[impl sched.build.terminal-status-settled+3]
                    // A resident terminal build's served accounting is
                    // frozen — the per-drv DerivationCached event above
                    // still flows, but its cached_derivations count must
                    // not drift after the terminal transition.
                    if let Some(b) = self.builds.get_mut(&build_id)
                        && !b.state().is_terminal()
                    {
                        b.cached_count += 1;
                    }
                }
            }
            // H1 fix (P0399): each newly-Skipped node may have Queued
            // parents whose all-deps are now Completed|Skipped. The
            // find_newly_ready(drv_hash) below at :732 only walks
            // parents of the ORIGINAL trigger — without this loop,
            // verify-rejected parents-of-Skipped hang Queued forever.
            // Example: A→B→C chain, A completes unchanged, verify
            // accepts B (Skipped) but rejects C (Queued). C's only
            // dep is B (now Skipped). find_newly_ready(A) returns []
            // (B is Skipped, not Queued); find_newly_ready(B) returns
            // [C] (C is Queued and all_deps_completed — Skipped now
            // accepted there).
            self.promote_newly_ready_batch(&skipped).await;
            if cap_hit {
                tracing::warn!(
                    trigger = %drv_hash,
                    node_count = crate::dag::MAX_CASCADE_NODES,
                    skipped = skipped.len(),
                    "CA cutoff cascade hit depth cap; remaining downstream will run normally"
                );
                metrics::counter!("rio_scheduler_ca_cutoff_depth_cap_hits_total").increment(1);
            }
        }
        skipped_interested
    }

    /// Phase 5: realisation_deps insert. Drains
    /// `pending_realisation_deps` (recorded at dispatch-time CA-on-CA
    /// resolve) into PG. Best-effort — `realisation_deps` is rio's
    /// derived-build-trace cache, not correctness-critical.
    // r[impl sched.ca.resolve+3]
    async fn ca_insert_realisation_deps(
        &mut self,
        drv_hash: &DrvHash,
        built_outputs: &[crate::domain::BuiltOutput],
    ) {
        // realisation_deps insert: the CA-on-CA resolve at dispatch
        // time recorded every `(dep_modular_hash, dep_output_name)`
        // lookup into `pending_realisation_deps`. The FK ordering
        // means those rows can only land AFTER this derivation's
        // own realisation exists — which `wopRegisterDrvOutput`
        // wrote before BuildComplete arrived (the worker's upload
        // flow: PutPath → RegisterDrvOutput → BuildComplete). So
        // this is the correct point: parent's realisation row is
        // present, dep rows were present at resolve time (we
        // queried them), both FK halves satisfied.
        //
        // Drained via `mem::take`: consumed once. A retry-after-
        // failure that re-dispatches re-runs resolve → fresh
        // `pending_realisation_deps`; the INSERT is ON CONFLICT
        // DO NOTHING so a duplicate attempt is harmless.
        //
        // Best-effort: PG blip → warn, don't abort completion.
        // `realisation_deps` is rio's derived-build-trace cache
        // (ADR-018:45), not correctness-critical for the build.
        if let Some(state) = self.dag.node_mut(drv_hash)
            && let Some(modular_hash) = state.ca.modular_hash
            && !state.ca.pending_realisation_deps.is_empty()
        {
            let lookups = std::mem::take(&mut state.ca.pending_realisation_deps);
            let output_names: Vec<String> = built_outputs
                .iter()
                .map(|o| o.output_name.clone())
                .collect();
            if let Err(e) = crate::ca::insert_realisation_deps(
                self.db.pool(),
                &modular_hash,
                &output_names,
                &lookups,
            )
            .await
            {
                warn!(
                    drv_hash = %drv_hash,
                    n_lookups = lookups.len(),
                    error = %e,
                    "insert_realisation_deps failed (best-effort cache; completion proceeds)"
                );
            }
        }
    }

    /// Phase 7 of success completion: build_samples insert (SLA-fit
    /// feed) and actual-vs-predicted scoring. All best-effort
    /// statistics — never fails completion.
    async fn record_build_sample(
        &mut self,
        drv_hash: &DrvHash,
        result: &crate::domain::BuildResult,
        (peak_memory_bytes, peak_cpu_cores): (u64, f64),
        (node_name, hw_class): (Option<String>, Option<String>),
        final_resources: Option<rio_proto::types::ResourceUsage>,
    ) {
        let final_res = final_resources.as_ref();
        if let Some(state) = self.dag.node(drv_hash)
            && let Some(pname) = &state.pname
            && let Some(actual) = result.duration()
        {
            // `domain::BuildResult::duration()` returns
            // `stop.duration_since(start)` — `None` on out-of-order
            // timestamps (matches the `> 0.0` gate below).
            let duration_secs = actual.as_secs_f64();
            // Sanity bound: reject durations > 30 days (bogus worker timestamps)
            if duration_secs > 0.0 && duration_secs < 30.0 * 86400.0 {
                // 0.0 CPU cores → "no samples taken" (build exited in
                // <1s before the 1Hz poller fired). Filtered to None so
                // the SLA fit's saturation detector doesn't see a
                // spurious 0. Worker floats are untrusted: non-finite
                // readings (±Inf, NaN) and magnitudes above the
                // structural cores ceiling (sla::config::MAX_CORES_HARD)
                // are likewise recorded as not-reported rather than
                // poisoning the fit's saturation check.
                // r[impl sched.executor.input-bounds+2]
                let peak_cpu = (peak_cpu_cores.is_finite()
                    && peak_cpu_cores > 0.0
                    && peak_cpu_cores <= crate::sla::config::MAX_CORES_HARD)
                    .then_some(peak_cpu_cores);
                // Raw sample for the SLA fit. Appends every completion
                // — the fit needs the full distribution, not a smoothed
                // scalar. Best-effort: warn, never fail completion on
                // sample-write error.
                //
                // D2: FODs included. They go through the identical SLA
                // pipeline as builds; "network-bound" is not a safe
                // assumption (large source unpacks, vendored builds,
                // git submodule recursion). If FOD telemetry later
                // shows the T(c) model is a poor fit for genuinely
                // network-dominated keys, the fix is a per-key
                // adjustment in the fitter, not a parallel codepath.
                //
                // peak_memory_bytes passed raw (as i64), not 0→None
                // filtered — 0 is a legitimate sample point
                // ("sub-second build, poller didn't fire"); the
                // percentile computation doesn't drag.
                {
                    // Same `attributed_tenant()` as `solve_intent_for`
                    // so the sample lands under the key the estimator
                    // was queried with — modulo the rare cancel/late-
                    // merge mid-build that shifts the min (per-tenant
                    // key is a grouping dimension, not a ledger; one
                    // mislabeled row in a P-percentile fitter is
                    // tolerated). None → "" matches the column's NOT
                    // NULL DEFAULT ''.
                    let tenant = state
                        .attributed_tenant(&self.builds)
                        .map(|u| u.to_string())
                        .unwrap_or_default();
                    let row = crate::db::BuildSampleRow {
                        pname: pname.clone(),
                        system: state.system.clone(),
                        tenant,
                        duration_secs,
                        // Clamp before i64 cast. u64 > i64::MAX wraps
                        // negative → build_samples row with negative
                        // memory → CutoffRebalancer percentiles poisoned.
                        // Physical RAM is well below 2^63 bytes, so this
                        // only fires on a misbehaving worker — but it
                        // costs nothing and prevents silent corruption.
                        peak_memory_bytes: peak_memory_bytes.min(i64::MAX as u64) as i64,
                        peak_cpu_cores: peak_cpu,
                        // The fit's independent variable is the
                        // parallelism the build RAN at — the builder
                        // sets `NIX_BUILD_CORES = min(assigned_cores,
                        // cgroup cpu.max)`. Intent-miss fallback can
                        // land a 2-core solve on a 16-core wildcard pod
                        // (cgroup > assigned); intent-match with a
                        // dispatch-time re-solve can pick more cores
                        // than the spawn-time pod's cgroup (assigned >
                        // cgroup). Either direction places a point off
                        // the true curve and inflates the fitted serial
                        // fraction, so record `min(assigned, cgroup)`.
                        // Fallback to whichever is present when one is
                        // missing (recovery → no intent; old executor →
                        // no cgroup).
                        cpu_limit_cores: {
                            let assigned =
                                state.sched.last_intent.as_ref().map(|i| f64::from(i.cores));
                            // Worker-supplied cgroup reading: non-finite,
                            // non-positive, or above MAX_CORES_HARD →
                            // treat as not-reported, so it can neither
                            // win the min() against the scheduler's own
                            // assigned-cores figure nor land raw on the
                            // no-intent (recovery) arm.
                            // r[impl sched.executor.input-bounds+2]
                            let cgroup = final_res.and_then(|r| r.cpu_limit_cores).filter(|c| {
                                c.is_finite()
                                    && *c > 0.0
                                    && *c <= crate::sla::config::MAX_CORES_HARD
                            });
                            match (assigned, cgroup) {
                                (Some(a), Some(c)) => Some(a.min(c)),
                                (a, c) => a.or(c),
                            }
                        },
                        // Worker-supplied floats below: kept only when
                        // finite and inside their physical domain
                        // (cumulative CPU-seconds ≥ 0, PSI pct ∈ [0,100]
                        // per the proto contract), else None ("not
                        // reported") — same convention as the 0-cores
                        // filter above. Integer magnitudes keep their
                        // i64::MAX clamps. r[impl sched.executor.input-bounds+2]
                        cpu_seconds_total: final_res
                            .and_then(|r| r.cpu_seconds_total)
                            .filter(|s| s.is_finite() && *s >= 0.0),
                        peak_disk_bytes: final_res
                            .and_then(|r| r.peak_disk_bytes)
                            .map(|b| b.min(i64::MAX as u64) as i64),
                        peak_io_pressure_pct: final_res
                            .and_then(|r| r.peak_io_pressure_pct)
                            .filter(|p| p.is_finite() && (0.0..=100.0).contains(p)),
                        // drv-declared sizing inputs — on the dag node.
                        version: state.version.clone(),
                        enable_parallel_building: state.enable_parallel_building,
                        enable_parallel_checking: state.enable_parallel_checking,
                        prefer_local_build: state.prefer_local_build,
                        // §A17: FOD fleet-prior exclusion keyed on
                        // output-spec, not pname-absence.
                        is_fixed_output: Some(state.is_fixed_output),
                        node_name,
                        // r[impl sched.sla.hw-ref-seconds]
                        // From CompletionReport.hw_class (builder reads
                        // RIO_HW_CLASS from the controller-stamped pod
                        // annotation). The scheduler has no Node
                        // informer, so this is the only path; None →
                        // factor=1.0 (old executor / non-k8s /
                        // annotator race).
                        hw_class: hw_class.clone(),
                        // Read-side only; write_build_sample uses now().
                        completed_at: 0.0,
                        id: 0,
                    };
                    if let Err(e) = self.db.write_build_sample(&row).await {
                        warn!(?e, %pname, system = %state.system, "write_build_sample failed");
                    }
                    // ADR-023 phase-7: actual-vs-predicted scoring.
                    // `last_intent.predicted` was snapshotted at
                    // dispatch time, so the ratio reflects the curve
                    // we sized against, not whatever the estimator
                    // has refit to since. None on cold-start /
                    // recovery — the histogram only sees model-backed
                    // dispatches.
                    if let Some(pred) = state
                        .sched
                        .last_intent
                        .as_ref()
                        .and_then(|i| i.predicted.as_ref())
                    {
                        // r[impl sched.sla.hw-ref-seconds]
                        // pred.wall_secs is reference-seconds (t_at()
                        // evaluates the ref-second-denominated fit);
                        // normalize the actual wall-clock by this
                        // completion's hw_class so the ratio is
                        // ref/ref. Envelope check (tier_target) stays
                        // wall-vs-wall — handled inside.
                        let key = crate::sla::types::ModelKey {
                            pname: pname.clone(),
                            system: state.system.clone(),
                            tenant: state
                                .attributed_tenant(&self.builds)
                                .map(|u| u.to_string())
                                .unwrap_or_default(),
                        };
                        let alpha = self.sla_estimator.cached_alpha(&key);
                        let hw_factor = self.sla_estimator.hw_factor(hw_class.as_deref(), alpha);
                        let score = crate::sla::metrics::score_completion(
                            duration_secs,
                            hw_factor,
                            peak_memory_bytes,
                            pred,
                        );
                        self.sla_estimator.record_misprediction(&key, &score);
                        if let Some(r) = score.ratio_wall {
                            ::metrics::histogram!(
                                "rio_scheduler_sla_prediction_ratio",
                                "dim" => "wall"
                            )
                            .record(r);
                        }
                        if let Some(r) = score.ratio_mem {
                            ::metrics::histogram!(
                                "rio_scheduler_sla_prediction_ratio",
                                "dim" => "mem"
                            )
                            .record(r);
                        }
                        if let Some((tier, result, constraint)) = score.envelope {
                            ::metrics::counter!(
                                "rio_scheduler_sla_envelope_result_total",
                                "tier" => tier,
                                "result" => result,
                                "constraint" => constraint,
                            )
                            .increment(1);
                        }
                    }
                }
            }
        }
    }

    /// Final phases of success completion: release newly-ready
    /// dependents (find_newly_ready) and per-build
    /// completion check. `interested_builds` is the trigger's set;
    /// `skipped_interested` (from CA cutoff cascade) is unioned in so a
    /// merged build the trigger does NOT belong to still terminates.
    pub(super) async fn release_downstream(
        &mut self,
        drv_hash: &DrvHash,
        interested_builds: &[Uuid],
        skipped_interested: HashSet<Uuid>,
        terminal_event: Option<rio_proto::types::build_event::Event>,
    ) {
        self.promote_newly_ready(drv_hash).await;

        // Update build completion status. Union the trigger's
        // interested_builds with skipped nodes' — a CA-cutoff-skipped
        // node may belong to a merged build the trigger does not, and
        // that build needs check_build_completion too.
        let trigger_set: HashSet<Uuid> = interested_builds.iter().copied().collect();
        let mut check_builds: HashSet<Uuid> = trigger_set.clone();
        check_builds.extend(skipped_interested);
        // sh-007c S5: collect-then-batch — same shape as the
        // dispatch.rs per-build tail. Per-build ordering preserved:
        // each build sees Progress (loop 1) before its terminal_event
        // and BuildCompleted (loop 2).
        let check_builds: Vec<Uuid> = check_builds.into_iter().collect();
        let mut counts: Vec<(Uuid, u32, u32, u32)> = Vec::with_capacity(check_builds.len());
        for &build_id in &check_builds {
            // I-140: build_summary is O(dag_nodes). Compute ONCE per
            // build, share between counts-persist and progress-emit.
            // Previously each fn ran its own scan → 2× per completion.
            let summary = self.dag.build_summary(build_id);
            if let Some((t, c, h)) = self.update_build_counts_with(build_id, &summary) {
                counts.push((build_id, t, c, h));
            }
            // Progress snapshot AFTER update_ancestors (critpath is
            // fresh — root priority dropped when this drv went
            // terminal) and BEFORE check_build_completion (which may
            // emit BuildCompleted; a final Progress showing 0
            // remaining is still useful right before that) — so a build
            // completing on THIS fan-out is still non-terminal here and
            // gets its final Progress; only builds that were ALREADY
            // terminal (resident shared-node interest) are skipped by
            // the wrapper's freeze.
            // _with bypasses debounce: completion always carries
            // user-visible state change, and the scan is already paid.
            self.emit_progress_with(build_id, &summary);
        }
        self.persist_build_counts_batch(&counts).await;
        for &build_id in &check_builds {
            // r[impl gw.activity.progress-before-stop]
            // Per-drv terminal event AFTER Progress: nom marks an
            // actBuild ✔ only when Progress.done increments while the
            // activity is still open (native nix Goal::done() updates
            // the parent counter before the Activity destructor).
            // Emitting here — between Progress and check_build_
            // completion — gives gateway progress→stop_activity→
            // BuildCompleted in that order. Only to the trigger's
            // interested builds; skipped-only builds don't have this
            // drv in their DAG.
            if let Some(ref ev) = terminal_event
                && trigger_set.contains(&build_id)
            {
                self.events.emit(build_id, ev.clone());
            }
            self.check_build_completion(build_id).await;
        }
    }

    /// Transition a derivation to Poisoned, persist, cascade
    /// DependencyFailed to ancestors, and propagate to interested
    /// builds. Called when the poison threshold is reached (see
    /// `PoisonConfig::is_poisoned`) or max_retries is hit.
    ///
    /// **Precondition:** status must be Ready, Assigned, or Running.
    /// Enforced via debug_assert! (tests catch violations) +
    /// early-return on transition failure (release builds don't
    /// cascade spuriously). Actor is single-threaded so no race
    /// between filter and call. Handles the Assigned→Running
    /// intermediate (state machine requires Running before Poisoned
    /// from those states). Ready→Poisoned is direct (I-065:
    /// `failed_builders` exhausts the fleet — never assigned). For
    /// Phase-1b sibling of [`Self::poison_and_cascade`] for collapsed
    /// sites whose appending transaction already wrote the attempt row
    /// AND persisted the Poisoned status: performs only the in-memory
    /// transition (with the same precondition guard), the unpin, and the
    /// shared terminal epilogue — no further PG writes. The epilogue's
    /// event status stays `TransientFailure`, matching what
    /// `poison_and_cascade` has always emitted for budget-exhaustion
    /// poisons.
    pub(super) async fn poison_already_recorded(
        &mut self,
        drv_hash: &DrvHash,
        error_msg: &str,
        final_line_count: Option<i64>,
        // Whether a fresh execution backs the poison (bug_080): every
        // caller is a collapsed worker-report site or the establishment
        // sweep — each states its value with a one-line justification.
        backing: rio_proto::VerdictBacking,
    ) {
        let Some(state) = self.dag.node_mut(drv_hash) else {
            return;
        };
        debug_assert!(
            matches!(
                state.status(),
                DerivationStatus::Ready | DerivationStatus::Assigned | DerivationStatus::Running
            ),
            "poison_already_recorded precondition violated: got {:?}",
            state.status()
        );
        state.ensure_running();
        if let Err(e) = state.transition(DerivationStatus::Poisoned) {
            warn!(drv_hash = %drv_hash, error = %e, current = ?state.status(),
                  "poison_already_recorded: ->Poisoned transition rejected, skipping cascade");
            return;
        }
        state.retry.poisoned_at = Some(crate::state::RecoveredInstant::fresh_now());
        self.unpin_best_effort(drv_hash).await;
        self.terminal_failure_epilogue(
            drv_hash,
            error_msg,
            rio_proto::types::BuildResultStatus::TransientFailure,
            final_line_count,
            backing,
        )
        .await;
    }

    /// the reassign path (worker disconnect), the caller checks the
    /// threshold BEFORE reset_to_ready — the drv is still
    /// Assigned/Running then.
    pub(super) async fn poison_and_cascade(
        &mut self,
        drv_hash: &DrvHash,
        error_msg: &str,
        // `CompletionReport.final_line_count` when the poisoning event
        // is a worker report (E1–E2); `None` for the reportless
        // triggers (disconnect re-check, controller terminations,
        // backstop, recovery) — the conservative stamp.
        final_line_count: Option<i64>,
        // The trigger's attempt-ledger row, when this poison IS the
        // observation being recorded (worker-reported E1/E2, the E9
        // fleet-exhaust marker). `None` when the trigger's row was (or
        // will be) appended at its own observation site (disconnect,
        // backstop, controller reports) or no row applies (recovery
        // re-check) — the poison persist then runs alone, as today.
        attempt_row: Option<AttemptRow>,
        // Whether a fresh execution backs the poison (bug_080): each
        // caller states its value with a one-line justification — the
        // no-eligible-source lane ("no pod and no attempt") is the one
        // NoExecution caller.
        backing: rio_proto::VerdictBacking,
    ) {
        let Some(state) = self.dag.node_mut(drv_hash) else {
            return;
        };
        debug_assert!(
            matches!(
                state.status(),
                DerivationStatus::Ready | DerivationStatus::Assigned | DerivationStatus::Running
            ),
            "poison_and_cascade precondition violated: got {:?}",
            state.status()
        );
        state.ensure_running();
        if let Err(e) = state.transition(DerivationStatus::Poisoned) {
            // Unexpected state (not Assigned/Running). debug_assert!
            // above fires in tests; in release, DON'T write Poisoned
            // to PG or cascade — in-mem ≠ PG + spurious cascade is
            // worse than a missed poison (which the next tick/
            // completion will re-evaluate).
            warn!(drv_hash = %drv_hash, error = %e, current = ?state.status(),
                  "poison_and_cascade: ->Poisoned transition rejected, skipping PG write + cascade");
            return;
        }
        state.retry.poisoned_at = Some(crate::state::RecoveredInstant::fresh_now());

        self.record_attempt_with_poison(drv_hash, attempt_row).await;
        self.unpin_best_effort(drv_hash).await;

        self.terminal_failure_epilogue(
            drv_hash,
            error_msg,
            rio_proto::types::BuildResultStatus::TransientFailure,
            final_line_count,
            backing,
        )
        .await;
    }

    // r[impl sched.event.derivation-terminal]
    /// Shared tail of every terminal-failure path
    /// ([`poison_and_cascade`], `handle_permanent_failure`,
    /// `handle_timeout_failure` cap-exhausted): stamp the execution
    /// terminal, emit
    /// `DerivationFailed` for the trigger to its interested builds,
    /// cascade `DependencyFailed` to ancestors, emit `DerivationFailed
    /// {DependencyFailed}` for EACH cascaded node to ITS interested
    /// builds, then run `handle_derivation_failure` for the union.
    ///
    /// The terminal stamp and the trigger's `DerivationFailed` event are
    /// unconditional. The previous `Option` form let
    /// `poison_and_cascade` skip both with a wrong rationale ("retry
    /// paths already emitted per-attempt events" — they don't; this is
    /// the only `DerivationEvent::failed` emission in the actor), so
    /// poison-via-exhaustion produced no client-visible event.
    ///
    /// This was the I-213 bug-class area — three near-identical ~30L
    /// blocks that drifted on which side-effects ran. Keeping the
    /// cascade/propagate sequencing in ONE place means a future
    /// "permanent failures forgot to unpin" can't recur per-handler.
    ///
    /// [`poison_and_cascade`]: Self::poison_and_cascade
    // r[impl sched.poison.cascade-dependents]
    pub(super) async fn terminal_failure_epilogue(
        &mut self,
        drv_hash: &DrvHash,
        error_msg: &str,
        status: rio_proto::types::BuildResultStatus,
        // The triggering report's `final_line_count` where the failure
        // path has one (worker-reported E1–E4); `None` on the
        // reportless paths (disconnect, controller reports, backstop,
        // substitute revert, recovery), which stays the correct
        // conservative value — the row reads as incomplete rather than
        // falsely complete.
        final_line_count: Option<i64>,
        // Whether a fresh execution of the current attempt cycle backs
        // the TRIGGER's failure event (bug_080) — stated on the wire as
        // `DerivationEvent.has_execution` so the gateway's log-hint
        // gate reads a fact instead of inferring from failure_status.
        // Cascaded ancestors always emit NoExecution: they are
        // bystanders swept from non-executing states (the note below).
        backing: rio_proto::VerdictBacking,
    ) {
        // Stamp + emit use the trigger's interested set (those builds
        // saw THIS drv fail); handle_derivation_failure below uses the
        // union (those builds saw SOME drv fail, possibly a cascaded
        // one). Stamp BEFORE handle_derivation_failure (which
        // may transition builds to terminal and schedule cleanup).
        // Cascaded ancestors are bystanders swept from non-executing
        // states — see terminal_log_epilogue's NOT-called-for note.
        // r[impl sched.merge.exec-correlation+8]
        let trigger_builds = self.get_interested_builds(drv_hash);
        self.terminal_log_epilogue(drv_hash, "failed", &trigger_builds, final_line_count);
        let trigger_path = self.dag.path_or_hash_fallback(drv_hash);
        for build_id in &trigger_builds {
            // r[impl gw.activity.progress-before-stop]
            // Progress BEFORE the per-drv terminal event so nom sees
            // failed++ while the actBuild is still open. Failure path
            // doesn't go through release_downstream, so this is
            // emitted inline (cost: one extra build_summary scan per
            // failure — rare, and handle_derivation_failure recomputes
            // it anyway after cascade mutates the DAG). This runs
            // BEFORE handle_derivation_failure flips a !keep_going
            // build terminal, so a build failing on THIS event still
            // gets its final Progress; already-terminal resident builds
            // are skipped by the wrapper's freeze.
            let summary = self.dag.build_summary(*build_id);
            self.emit_progress_with(*build_id, &summary);
            self.events.emit(
                *build_id,
                rio_proto::types::build_event::Event::Derivation(
                    rio_proto::types::DerivationEvent::failed(
                        trigger_path.clone(),
                        error_msg.to_string(),
                        status,
                        backing,
                    ),
                ),
            );
        }

        // Cascade: parents of a terminally-failed derivation can never
        // complete. Transition them to DependencyFailed so keepGoing
        // builds terminate.
        let cascaded = self.cascade_dependency_failure(drv_hash).await;

        // r[impl sched.event.derivation-terminal]
        // Emit a DependencyFailed event per cascaded node to THAT
        // node's interested builds (a cascaded node may belong to a
        // merged build the trigger does NOT). Without this, WatchBuild
        // clients see cascaded derivations frozen at Queued under
        // keep_going while the build moves on.
        let dep_msg = format!("dependency '{trigger_path}' failed: {error_msg}");
        for cascaded_hash in &cascaded {
            let interested = self.get_interested_builds(cascaded_hash);
            let cascaded_path = self.dag.path_or_hash_fallback(cascaded_hash);
            for build_id in interested {
                self.events.emit(
                    build_id,
                    rio_proto::types::build_event::Event::Derivation(
                        rio_proto::types::DerivationEvent::failed(
                            cascaded_path.clone(),
                            dep_msg.clone(),
                            rio_proto::types::BuildResultStatus::DependencyFailed,
                            // Bystanders swept from non-executing
                            // states: no execution of THEIR cycle
                            // exists — stated, not narrated (bug_080).
                            rio_proto::VerdictBacking::NoExecution,
                        ),
                    ),
                );
            }
        }

        // Propagate to builds — union of trigger's AND cascaded nodes'.
        // A cascaded parent may belong to a merged build the trigger
        // does not; that build must also get handle_derivation_failure
        // or it hangs Active forever.
        for build_id in self.union_interested_with_cascaded(drv_hash, &cascaded) {
            self.handle_derivation_failure(build_id, drv_hash, status)
                .await;
        }
    }

    /// Shared tail of the collapsed non-terminal retry paths
    /// (`handle_infrastructure_failure`, `handle_timeout_failure`
    /// under-cap): the `Ready` persist already happened inside the
    /// site's appending transaction, so only the dashboard progress
    /// emit remains (the `Ready` status itself re-arms dispatch via
    /// the spawn-intent pass). The caller has already done the
    /// `reset_to_ready` transition + counter bookkeeping.
    ///
    /// `handle_transient_failure` does NOT use this — it goes through
    /// the `Failed→Ready` intermediate with backoff and its own
    /// progress emit.
    fn requeue_after_recorded_retry(&mut self, drv_hash: &DrvHash) {
        for build_id in self.get_interested_builds(drv_hash) {
            self.emit_progress(build_id);
        }
    }

    // r[impl sched.build.keep-going]
    /// Prune `drv_hash` from `derivation_hashes` of every interested
    /// `keep_going=true` build. Call BEFORE `dag.remove_node` (reads
    /// `interested_builds` from the node).
    ///
    /// Both poison-removal paths (`tick_process_expired_poisons`,
    /// `handle_clear_poison`) drop the node from the DAG. A
    /// `keep_going=true` build that's still Active (other derivations
    /// running) keeps the hash in `derivation_hashes` → `total` stays
    /// at the original count but `completed+failed` (from
    /// `build_summary`, which counts DAG nodes) can never reach it →
    /// build hangs Active forever. `keep_going=false` builds are
    /// already terminal (poison → `handle_derivation_failure` →
    /// `transition_build_to_failed` in the same turn) so the prune is
    /// a no-op for them — but filtering keeps the intent explicit.
    pub(super) fn prune_interested_keep_going(&mut self, drv_hash: &DrvHash) {
        for build_id in self.get_interested_builds(drv_hash) {
            if let Some(build) = self.builds.get_mut(&build_id)
                && build.keep_going
            {
                build.derivation_hashes.remove(drv_hash);
            }
        }
    }

    // r[impl sched.admin.clear-poison+3]
    /// Clear poison state for a derivation (admin-initiated via
    /// `AdminService.ClearPoison`). Returns `true` if cleared.
    ///
    /// In-mem: node removed from the DAG entirely — next submit re-inserts
    /// it fresh with full proto fields and runs it through
    /// `compute_initial_states`; the re-insert's `resubmit_cycles`
    /// starts at the parked demand-epoch floor (`cycles + 1` — bug_058,
    /// see the reset-row comment below). PG: `db.clear_poison()` sets
    /// status='created', NULLs `poisoned_at`/`retry_count`/`failed_builders`.
    /// Surviving parents are closure-hole stamped and then re-evaluated
    /// (`reevaluate_removal_survivors` — settlement / promotion), so a
    /// parent that was waiting Queued above this child makes progress
    /// (`sched.poison.clear-survivor-reevaluation`).
    ///
    /// PG first, in-mem second — if PG fails the operator's retry
    /// finds in-mem still Poisoned and can proceed. The previous
    /// order (in-mem first) meant a PG blip left status=Created,
    /// so retry hit the not-poisoned guard → permanent no-op until
    /// scheduler restart.
    pub(super) async fn handle_clear_poison(&mut self, drv_hash: &DrvHash) -> bool {
        match self.dag.node(drv_hash).map(|s| s.status()) {
            None => return false, // not found
            Some(s) if s != DerivationStatus::Poisoned => return false,
            Some(_) => {}
        }
        // 1a: the admin clear's `poison_cleared` reset row joins the
        // PG-first clear in one transaction. The clear discipline
        // itself (PG-first, scrubs the durable exclusion set) is pinned
        // by `clearedPoisonClearsDurably` / `clearedPoisonScrubsExclusions`
        // in the model — this only ADDS a row; it must not reorder the
        // clear.
        //
        // bug_058: the row carries `cycles + 1` — a BUMP from the live
        // value, never the old rewind-to-0. The controller's gave-up
        // latch decays on a CHANGED observed `SpawnIntent
        // .resubmit_cycle`; the rewind made the documented
        // ClearPoison-then-resubmit recovery an equality FIXED POINT at
        // the common cycle-0 latch (cleared row 0 → fresh re-insert at
        // 0 → observed 0 == latched 0 → held forever). The bump is
        // monotone over everything the controller can have observed
        // for this node, so the FIRST post-clear observation decays the
        // latch. The same value is parked as a DAG floor below so the
        // post-clear re-insert actually presents it.
        // r[impl sched.resubmit.epoch-total]
        let reset_row = self
            .reset_row_for(drv_hash, OutcomeClass::PoisonCleared, ReportingParty::Admin)
            .map(|mut r| {
                r.resubmit_cycle = r.resubmit_cycle.saturating_add(1);
                r
            });
        let epoch_floor = reset_row
            .as_ref()
            .map(|r| u32::try_from(r.resubmit_cycle).unwrap_or(u32::MAX));
        match self
            .record_reset_with_clear_poison(drv_hash, reset_row)
            .await
        {
            Ok(
                crate::db::FencedOutcome::Applied(_) | crate::db::FencedOutcome::AlreadyResolved,
            ) => {}
            // r[impl sched.evidence.durability+4]
            // Fenced: this replica is deposed (a successor's claim is
            // the floor). The PG clear did NOT happen, so the admin
            // contract is the same as the PG-failure arm: report
            // cleared=false, leave the in-memory state untouched. The
            // operator retries against the live leader.
            Ok(crate::db::FencedOutcome::Fenced) => {
                return false;
            }
            Err(e) => {
                error!(drv_hash = %drv_hash, error = %e,
                       "ClearPoison: PG clear failed (in-mem untouched; retry-safe)");
                return false;
            }
        }
        // Remove from DAG so next merge treats it as newly-inserted.
        // Resetting status in-place would strand stub fields from
        // `from_poisoned_row` and `compute_initial_states` only iterates
        // `newly_inserted` — node would sit in Created forever. Poisoned
        // nodes have no interested keep_going=false builds (build already
        // terminated); keep_going=true builds are pruned from
        // derivation_hashes here. The sticky `error_summary` set by
        // `handle_derivation_failure` keeps such builds on track to Fail
        // even after the node (and its `failed_count` contribution) is gone.
        //
        // Capture the parents BEFORE `remove_node` scrubs the edge maps
        // — they are the survivor set the re-evaluation below wakes.
        let surviving_parents = self.dag.get_parents(drv_hash);
        self.prune_interested_keep_going(drv_hash);
        self.dag.remove_node(drv_hash);
        // bug_058: park the bumped demand epoch so the next merge's
        // fresh insert of this drv starts at it (in-memory floor; the
        // committed `poison_cleared` row above carries the same value
        // — the re-insert's own `resubmit_reset` row then makes it
        // durable; a failover inside the clear→resubmit window
        // degrades to the verdict-free band's explicit-resubmission
        // mint, never to a fixed point).
        if let Some(floor) = epoch_floor {
            self.dag.note_resubmit_floor(drv_hash, floor);
        }
        // r[impl sched.poison.clear-survivor-reevaluation+2]
        // Wake the surviving parents: promote Queued ones whose deps
        // are now (vacuously) satisfied; survivors with an unresolved
        // materialization job are armed by the job itself. Without
        // this, a parent the recovery condemnation spared on
        // co-ownership grounds (it recovered Queued above this
        // non-co-owned poisoned child) waits forever — this removal is
        // its only wake-up edge, since `find_newly_ready` fires only on
        // completions. Leader-only by construction — the only
        // production caller is the admin ClearPoison RPC, which is
        // leader-guarded at the gRPC layer (`ensure_leader`).
        self.reevaluate_removal_survivors(&surviving_parents).await;
        info!(drv_hash = %drv_hash, "poison cleared by admin; node removed from DAG");
        true
    }

    // r[impl sched.retry.transient-budget+2]
    /// E1, collapsed onto `decide()` (Phase 1b): the threshold /
    /// per-cycle-cap / retry verdict comes from the fold over the
    /// appended attempt suffix inside the appending transaction. (The
    /// stream-era completion-time fleet-exhaust arm — `placeable()`
    /// over the in-memory executors map — retired with that map; the
    /// AD2 spawn-gate `NoEligibleSource` report is the source-exhaust
    /// path.) The
    /// failure is charged once, by the appended row (no in-place RAM
    /// counter writes and no legacy mirror-column writes remain since
    /// T-1b.13 — the cached view is refreshed from the fold after the
    /// commit); the synthesized poison-reason strings stay as-is (A8 is
    /// out of scope); transients are never floor-promoted and never
    /// exempt (P4) — there is deliberately no floor or exemption logic
    /// on this arm. No resource_floor bump either: `TransientFailure`
    /// (build script exited nonzero) is a build-determinism signal, not
    /// a sizing signal — the previous I-170/I-173/I-177 over-broad
    /// promote here meant a flaky build climbed the ladder and the next
    /// submitter paid for an oversized pod.
    pub(super) async fn handle_transient_failure(
        &mut self,
        drv_hash: &DrvHash,
        executor_id: &ExecutorId,
        report: FailureReportCtx<'_>,
    ) -> FailureHandling {
        // OA1 interval (ii), worker-report cause: the worker's own
        // report is the terminal observation; the requeue (if the
        // verdict is a retry) happens in this same actor turn.
        let observed_at = Instant::now();
        // Ledger row for this observed attempt (1a): captured before
        // any transition clears the exec_id carrier; appended together
        // with whichever status persist this handler ends in.
        let mut attempt_row =
            self.attempt_row_for(drv_hash, OutcomeClass::Transient, ReportingParty::Worker);
        if let Some(row) = attempt_row.as_mut() {
            row.executor_id = Some(executor_id.clone());
            row.error_msg = (!report.error_msg.is_empty()).then(|| report.error_msg.to_string());
            row.final_line_count = report.final_line_count;
        }

        if self.dag.node(drv_hash).is_none() {
            return FailureHandling::Handled;
        }

        // The verdict: decide() over the appended suffix, inside the
        // appending transaction; the verdict's status is persisted on
        // the same connection (threshold / cap poison -> Poisoned,
        // retry -> Ready). The stream-era completion-time fleet-exhaust
        // arm (placeable() over the in-memory executors map) retired
        // with that map: source exhaustion is now observed where the
        // fleet actually lives — the controller's AD2 spawn gate
        // reports `NoEligibleSource`, and `handle_no_eligible_source`
        // poisons with the same metric. The uncommitted-merge edge (no
        // db row) folds the in-memory history plus a synthetic record
        // for this observation and persists nothing.
        let (decision, recorded_row) = if let Some(row) = attempt_row {
            let result: Result<Option<crate::retry_policy::Decision>, sqlx::Error> = async {
                // r[impl sched.evidence.durability+4]
                // Claims-floor fence at the appending-transaction start:
                // a deposed replica records nothing and persists nothing.
                let mut tx = match self.db.begin_fenced(self.serving_generation()).await? {
                    crate::db::FencedBegin::Fenced { .. } => {
                        return Ok(None);
                    }
                    crate::db::FencedBegin::Open(ftx) => ftx,
                };
                let (_, decision) = self.append_and_decide_in_tx(tx.conn(), &row).await?;
                if matches!(decision.verdict, crate::retry_policy::Verdict::Poison(_)) {
                    crate::db::SchedulerDb::persist_poisoned_in_tx(tx.conn(), drv_hash).await?;
                } else {
                    crate::db::SchedulerDb::update_derivation_status_in_tx(
                        tx.conn(),
                        drv_hash,
                        DerivationStatus::Ready,
                        None,
                    )
                    .await?;
                }
                tx.commit().await?;
                Ok(Some(decision))
            }
            .await;
            match result {
                Ok(Some(decision)) => (decision, Some(row)),
                Ok(None) => {
                    // Fenced: this replica is deposed. Drop the report
                    // (re-delivery would keep hitting the fence); the
                    // successor re-derives the failure from the open
                    // attempt row via its establishment sweep.
                    self.note_fenced_evidence_write("transient-failure appending transaction");
                    return FailureHandling::Handled;
                }
                Err(e) => {
                    warn!(drv_hash = %drv_hash, executor_id = %executor_id, error = %e,
                          "transient failure: appending transaction failed; derivation stays in \
                           its pre-report state pending re-delivery");
                    return FailureHandling::RecordFailed;
                }
            }
        } else {
            let mut history = self
                .dag
                .node(drv_hash)
                .map(|s| s.attempt_history().to_vec())
                .unwrap_or_default();
            // Include the current (unrecordable) observation so the
            // threshold sees it, exactly as the RAM path did.
            history.push(crate::state::AttemptRecord {
                attempt_id: Uuid::now_v7(),
                event_kind: crate::state::AttemptEventKind::Attempt,
                outcome_class: OutcomeClass::Transient,
                exec_id: None,
                executor_id: Some(executor_id.clone()),
                // The transient-failure path is a build observation by
                // construction (kind partition identity value).
                attempt_kind: crate::state::AttemptKind::Build,
                source_node: self.pull_attempt_source_node(drv_hash),
                termination_reason: None,
                reporting_party: ReportingParty::Worker,
                exempt: false,
                floor_promoted: false,
                floor_at_cap: false,
                error_msg: None,
                final_line_count: None,
                resubmit_cycle: 0,
                occurred_at_epoch_secs: crate::db::attempts::epoch_now(),
                recorded_at_epoch_secs: 0.0,
            });
            let decision = crate::retry_policy::decide(
                &history,
                &self.decision_budget(),
                crate::db::attempts::epoch_now() as crate::retry_policy::AbsTime,
            );
            (decision, None)
        };

        // Push the committed row onto the in-memory history and refresh
        // the cached dispatch view before acting on the verdict, so the
        // re-dispatch that may follow the requeue arm already sees this
        // failure's exclusion in `hard_filter`.
        if let Some(row) = recorded_row {
            if let Some(state) = self.dag.node_mut(drv_hash) {
                state.push_attempt_record(row.to_record());
            }
            self.refresh_retry_view(drv_hash);
        }

        // Act on the verdict with the as-built reason priority:
        // threshold > per-cycle cap. The poison reason stays the
        // synthesized string (the worker's error_msg is diagnostics the
        // E1 arm has never surfaced here); the report's line count
        // rides along so the final execution's log reads complete.
        // Source exhaustion (every spawnable source excluded) is the
        // controller's AD2 spawn-gate verdict, reported via
        // `ReportAttemptOutcome(NoEligibleSource)` and acted on in
        // `handle_no_eligible_source` — there is no in-memory fleet
        // here to check against.
        match decision.verdict {
            crate::retry_policy::Verdict::Poison(
                crate::retry_policy::PoisonReason::TransientBudget,
            ) => {
                let n = decision.counters.count;
                if let Some(state) = self.dag.node(drv_hash) {
                    warn!(
                        drv_hash = %drv_hash,
                        retry_count = n,
                        max = self.retry_policy.max_retries,
                        resource_floor = ?state.sched.resource_floor,
                        "transient failure: max_retries exhausted, poisoning"
                    );
                }
                self.poison_already_recorded(
                    drv_hash,
                    &format!("max_retries={n} exhausted after transient failures"),
                    report.final_line_count,
                    // Collapsed worker-report poison: this report IS a
                    // fresh execution's outcome (bug_080).
                    rio_proto::VerdictBacking::FreshExecution,
                )
                .await;
            }
            crate::retry_policy::Verdict::Poison(_) => {
                // The distinct-worker / flat-count poison threshold (or,
                // defensively, any other reason the fold could produce
                // for a transient last event).
                self.poison_already_recorded(
                    drv_hash,
                    &format!(
                        "poison threshold reached after {} distinct-worker failures",
                        decision.counters.failed_builders.len()
                    ),
                    report.final_line_count,
                    // Collapsed worker-report poison: fresh execution
                    // (bug_080).
                    rio_proto::VerdictBacking::FreshExecution,
                )
                .await;
            }
            _ => {
                // Delayed re-queue: set backoff_until on the state, then
                // Failed → Ready + push. dispatch_ready checks
                // backoff_until and defers if not yet elapsed. Stateless
                // — no timer tasks, no cleanup if the derivation is
                // cancelled meanwhile (backoff_until is just an Option
                // on the state, ignored for non-Ready). Cleared on
                // successful dispatch in assign_to_worker. The count
                // itself is fold-derived (the refresh above); only the
                // jittered pacing deadline is written here at the site
                // (the backoff_until carve-out). The curve index is the
                // attempt count BEFORE this failure — the fold's count
                // already includes this event, hence the -1.
                let backoff = self
                    .retry_policy
                    .backoff_duration(decision.counters.count.saturating_sub(1));
                if let Some(state) = self.dag.node_mut(drv_hash) {
                    state.ensure_running();
                    if let Err(e) = state.transition(DerivationStatus::Failed) {
                        warn!(drv_hash = %drv_hash, error = %e, "Running->Failed transition failed");
                    }
                    state.assigned_executor = None;
                    state.retry.backoff_until = Some(Instant::now() + backoff);
                    debug!(
                        drv_hash = %drv_hash,
                        retry_count = state.retry.count,
                        backoff_secs = backoff.as_secs_f64(),
                        "scheduling retry after transient failure"
                    );
                    if let Err(e) = state.transition(DerivationStatus::Ready) {
                        warn!(drv_hash = %drv_hash, error = %e, "Failed->Ready transition failed");
                    } else {
                        metrics::histogram!(
                            "rio_scheduler_attempt_requeue_seconds",
                            "cause" => "worker-report"
                        )
                        .record(observed_at.elapsed().as_secs_f64());
                    }
                }
                // C2c: dashboard's WatchBuild showed stale running_count
                // through the backoff window (up to 300s) without the
                // progress emit.
                for build_id in self.get_interested_builds(drv_hash) {
                    self.emit_progress(build_id);
                }
            }
        }
        FailureHandling::Handled
    }

    /// bughunt-2 slot 3 C2 (merged_bug_032): compute the gated
    /// disposition of a worker-supplied `store_degraded` flag. Called
    /// ONCE at the top of `handle_infrastructure_failure`; after that
    /// line the raw ctx bit is read nowhere else in this file (pinned
    /// by the policy test) -- floor-skip, event selection, and the
    /// paced write all consume this disposition.
    ///
    /// Corroboration (the flag is WORKER evidence -- one node's word is
    /// never store evidence): >= 2 distinct CONTROLLER-AUTHORITATIVE
    /// node bindings flagging within
    /// [`STORE_DEGRADED_CORROBORATION_WINDOW`], OR the scheduler's own
    /// store RPCs failing inside the window (the fleet-of-one leg).
    /// Corroborated reports are then admitted against the kernel run
    /// bound ([`rio_retry_kernel::STORE_DEGRADED_FREE_RUN`]) -- at the
    /// bound, charged fallthrough.
    fn store_degraded_disposition(
        &mut self,
        drv_hash: &DrvHash,
        flagged: bool,
    ) -> StoreDegradedDisposition {
        if !flagged {
            return StoreDegradedDisposition::NotDegraded;
        }
        let now = Instant::now();
        let node = self.pull_attempt_source_node(drv_hash);
        let distinct_nodes = note_store_degraded_sighting(
            &mut self.store_degraded_sightings,
            node,
            now,
            STORE_DEGRADED_CORROBORATION_WINDOW,
        );
        let store_health_leg = self
            .last_store_rpc_failure
            .is_some_and(|t| now.duration_since(t) <= STORE_DEGRADED_CORROBORATION_WINDOW);
        let disposition = if distinct_nodes < 2 && !store_health_leg {
            StoreDegradedDisposition::Uncorroborated
        } else {
            let admission = self
                .dag
                .node(drv_hash)
                .map(|s| crate::retry_policy::admit_store_degraded(s.attempt_history()))
                .unwrap_or(rio_retry_kernel::WorkerAbortAdmission::Uncharged);
            if admission == rio_retry_kernel::WorkerAbortAdmission::ChargedFallthrough {
                StoreDegradedDisposition::RunBound
            } else {
                StoreDegradedDisposition::Paced
            }
        };
        // merged_bug_200: NO tick here. The disposition is computed at
        // classification time but the counter advertises SETTLED
        // outcomes ("uncharged requeue" per the HELP) — the single
        // emission site is the post-commit block in
        // `handle_infrastructure_failure` (the settled-close witness
        // pattern, same as the bug_086 width-event close): fenced
        // drops, failed appending transactions (one report = N
        // deliveries), and DAG-absent races never tick.
        disposition
    }

    /// InfrastructureFailure: worker-local problem, not the build's fault.
    /// Reset to Ready and retry WITHOUT inserting into `failed_builders`.
    ///
    /// InfrastructureFailure is the worker saying "I can't right now."
    /// If it's still broken, it'll fail again. If it's recovered
    /// (circuit closed), it'll succeed. Pull-mode executors are
    /// one-shot pods — a worker with a broken store fails its single
    /// build and exits; there is no persistent worker to cap, so
    /// per-worker capping isn't needed.
    ///
    /// BUT: `infra_retry_count` IS incremented and checked against
    /// `max_infra_retries` (a separate, higher bound than `max_retries`).
    /// Without this, a scheduler-side bug that misclassifies a
    /// deterministic failure as infra (e.g., empty CA input path →
    /// worker's MetadataFetch error → InfrastructureFailure) hot-loops
    /// forever at ~100ms intervals. Observed: 9748 re-dispatches in one
    /// session before the CA-path-propagation fix. The bound converts a
    /// livelock into a visible poison.
    ///
    /// Separate counter from `retry_count` so infra failures don't eat
    /// into the transient-failure budget: 3× infra + 1× transient
    /// should NOT poison on the transient (the transient is the first
    /// REAL failure; the 3 infra were worker-side noise).
    /// E2, collapsed onto `decide()`/`classify()` (Phase 1b): the
    /// exempt-vs-counted classification comes from `classify()` over the
    /// site's floor-outcome locals, and the requeue / exempt-cap /
    /// infra-cap verdict comes from the fold over the appended attempt
    /// suffix inside the appending transaction. The charge is carried by
    /// the appended row alone (the cached view is refreshed from the
    /// fold; no in-place counter writes remain since T-1b.13); a failed
    /// transaction leaves the derivation in its pre-report state for
    /// re-delivery.
    pub(super) async fn handle_infrastructure_failure(
        &mut self,
        drv_hash: &DrvHash,
        executor_id: &ExecutorId,
        report: FailureReportCtx<'_>,
    ) -> FailureHandling {
        // OA1 interval (ii), worker-report cause (see
        // handle_transient_failure).
        let observed_at = Instant::now();
        let error_msg = report.error_msg;
        // merged_bug_032: the SINGLE raw-bit read site — everything
        // below consumes the gated disposition, never the wire flag.
        let degraded = self.store_degraded_disposition(drv_hash, report.store_degraded());
        // Ledger row for this observed attempt (1a): captured before
        // the floor bump / reset clears the exec_id carrier. The class
        // and exemption flags are refined below once the floor outcome
        // and the exempt predicate are known.
        let mut attempt_row =
            self.attempt_row_for(drv_hash, OutcomeClass::Infra, ReportingParty::Worker);
        if let Some(row) = attempt_row.as_mut() {
            row.executor_id = Some(executor_id.clone());
            row.error_msg = (!error_msg.is_empty()).then(|| error_msg.to_string());
            row.final_line_count = report.final_line_count;
        }
        // I-199: bump resource_floor ONLY on the worker-reported
        // sizing signals — `CgroupOom` (I-196 OOM watcher: build child
        // hit cgroup memory.max while the pod itself survived) and
        // `DiskFull` (live_057: overlay prjquota exhausted with node
        // headroom — the disk twin). Other infra failures (FUSE EIO,
        // PutPath race, store-replica-restart) are NOT size-related —
        // the previous over-broad promote here is what made cmake go
        // medium→large→xlarge in live QA from a store-replica-restart
        // with zero builds run.
        // Pod-level OOMKilled (whole pod died) arrives as the
        // controller's `ReportAttemptOutcome` classification fill,
        // which deliberately never promotes (sla-sizing.typ accepted
        // residual).
        //
        // bug_408: a store-degraded failure is never a sizing signal —
        // the breaker fired on store fetches, not on the build's
        // memory — so the floor bump is skipped even if the message
        // happens to contain the OOM marker.
        // merged_bug_032: only a BELIEVED store attribution (corroborated
        // — Paced or RunBound) suppresses the sizing signal; an
        // uncorroborated flag is treated as plain infra INCLUDING the
        // OOM floor bump.
        let believed_store = matches!(
            degraded,
            StoreDegradedDisposition::Paced | StoreDegradedDisposition::RunBound
        );
        // bug_090 (live_057-b rebuilt): the floor moves ONLY on the — quantifier: census(forged_free_text_never_moves_resource_floors)
        // TYPED classification field, corroborated against the shape
        // this scheduler itself assigned — worker-supplied free text
        // (error_msg) is display/narration and drives nothing. The
        // bug_408 believed-store suppression applies IDENTICALLY: a
        // store-degraded failure is never a sizing signal, on either
        // axis.
        //
        // ENVELOPE (R29, enrolled in the wave's duration/envelope
        // census; consumer: THIS gate): at most ONE doubling per
        // corroborated incident IDENTITY — per unique
        // (drv_hash, exec_id) — POPULATION-denominated, never
        // wall-windowed (a wall window lets a paced forger ladder up
        // anyway; N corroboration-free reports over any timespan move
        // nothing). The identity law's durable half is the report
        // admission fold: a terminal report is consumed once per exec
        // (`fold_report` over the durable attempt row + the
        // assignment-token dedup), so this gate runs at most once per
        // exec and each ladder step burns a REAL scheduled attempt of
        // the forger's own drv at the previously-assigned size — the
        // honest path's cost, no amplification.
        let floor_outcome = match report.sizing() {
            Some(claim) if !believed_store => {
                self.bump_floor_on_corroborated_claim(drv_hash, claim).await
            }
            _ => super::floor::FloorOutcome::default(),
        };

        if self.dag.node(drv_hash).is_none() {
            return FailureHandling::Handled;
        }

        // D4: a floor bump that returned `promoted=true` is a sizing
        // signal — the next dispatch goes larger; the THIS-attempt
        // failure is not the build's fault. Exempt from the infra cap
        // alongside the I-127 PutPath case below. At-cap (`at_cap=
        // true`) is NOT exempt — the counted charge is what bounds it.
        //
        // I-127: "concurrent PutPath" is NEVER counted toward the
        // infra cap. It means another builder is uploading the SAME
        // output — the drv almost certainly succeeded elsewhere; this
        // worker just lost the upload race. I-125b makes the builder
        // wait-then-adopt on this error, so reaching here means the
        // wait timed out (other uploader stuck/slow). Re-dispatch
        // freely: either the path appears (next attempt → AlreadyValid)
        // or the lock clears and the upload retries. Poisoning on this
        // is exactly the wrong outcome — observed under shallow-1024x
        // when a leaked lock (I-125a) made 4 builders hit this in a
        // row → poison at 99.7%.
        //
        // The exemption predicate (promoted-or-CONCURRENT_PUTPATH) lives
        // in `classify()` — the single append-time classifier — so the
        // row's class is what the fold charges.
        // r[impl sched.retry.store-degraded-uncharged+4]
        // bug_408: the flagged report classifies as the dedicated
        // pacing class — the kernel fold advances only the derivation
        // backoff (no count budget, no exclusion, never poison), so
        // the verdict below is a paced Requeue for as long as the
        // outage lasts.
        let event = if degraded == StoreDegradedDisposition::Paced {
            crate::retry_policy::ObservedFailure::WorkerStoreDegraded
        } else {
            // NotDegraded, Uncorroborated, and RunBound all charge the
            // counted infra budget (merged_bug_032).
            crate::retry_policy::ObservedFailure::WorkerInfra { error_msg }
        };
        let class = crate::retry_policy::classify(
            &event,
            crate::retry_policy::FloorOutcomeView {
                promoted: floor_outcome.promoted,
                at_cap: floor_outcome.at_cap,
            },
        );
        let exempt_from_cap = class == OutcomeClass::ExemptInfra;

        // Refine the ledger row with the site's classification: the
        // exempt class plus the floor-outcome discriminators the fold
        // reads instead of re-deriving the floor at decision time.
        if let Some(row) = attempt_row.as_mut() {
            row.exempt = exempt_from_cap;
            row.floor_promoted = floor_outcome.promoted;
            row.floor_at_cap = floor_outcome.at_cap;
            row.outcome_class = class;
        }

        // The verdict: decide() over the appended suffix inside the
        // appending transaction (poison verdicts persist Poisoned,
        // requeue persists Ready — both on the same connection). The
        // uncommitted-merge edge (no db row) folds the in-memory
        // attempt history instead and persists nothing (there is no
        // derivations row to update either).
        #[cfg(test)]
        let inject_append_failure = std::mem::take(&mut self.fail_next_attempt_append);
        let (decision, recorded_row) = if let Some(row) = attempt_row {
            let result: Result<Option<crate::retry_policy::Decision>, sqlx::Error> = async {
                #[cfg(test)]
                if inject_append_failure {
                    return Err(sqlx::Error::WorkerCrashed);
                }
                // r[impl sched.evidence.durability+4]
                // Claims-floor fence at the appending-transaction start.
                let mut tx = match self.db.begin_fenced(self.serving_generation()).await? {
                    crate::db::FencedBegin::Fenced { .. } => {
                        return Ok(None);
                    }
                    crate::db::FencedBegin::Open(ftx) => ftx,
                };
                let (_, decision) = self.append_and_decide_in_tx(tx.conn(), &row).await?;
                if matches!(decision.verdict, crate::retry_policy::Verdict::Poison(_)) {
                    crate::db::SchedulerDb::persist_poisoned_in_tx(tx.conn(), drv_hash).await?;
                } else {
                    crate::db::SchedulerDb::update_derivation_status_in_tx(
                        tx.conn(),
                        drv_hash,
                        DerivationStatus::Ready,
                        None,
                    )
                    .await?;
                }
                tx.commit().await?;
                Ok(Some(decision))
            }
            .await;
            match result {
                Ok(Some(decision)) => (decision, Some(row)),
                Ok(None) => {
                    // Fenced: deposed replica — drop the report (see the
                    // transient handler's fence comment).
                    self.note_fenced_evidence_write("infrastructure-failure appending transaction");
                    return FailureHandling::Handled;
                }
                Err(e) => {
                    warn!(drv_hash = %drv_hash, executor_id = %executor_id, error = %e,
                          "infrastructure failure: appending transaction failed; derivation \
                           stays in its pre-report state pending re-delivery");
                    return FailureHandling::RecordFailed;
                }
            }
        } else {
            let history = self
                .dag
                .node(drv_hash)
                .map(|s| s.attempt_history().to_vec())
                .unwrap_or_default();
            (
                crate::retry_policy::decide(
                    &history,
                    &self.decision_budget(),
                    crate::db::attempts::epoch_now() as crate::retry_policy::AbsTime,
                ),
                None,
            )
        };

        // Push the committed row onto the in-memory history and refresh
        // the cached dispatch view before acting on the verdict.
        if let Some(row) = recorded_row {
            if let Some(state) = self.dag.node_mut(drv_hash) {
                state.push_attempt_record(row.to_record());
            }
            self.refresh_retry_view(drv_hash);
            // merged_bug_200: THE single emission site — the row
            // committed, so the disposition settled. Fenced drops,
            // failed appending transactions, and DAG-absent races all
            // returned above without reaching this block.
            if let Some(label) = degraded.as_label() {
                metrics::counter!(
                    "rio_scheduler_store_degraded_requeues_total",
                    "disposition" => label
                )
                .increment(1);
            }
        }

        match decision.verdict {
            crate::retry_policy::Verdict::Poison(
                crate::retry_policy::PoisonReason::ExemptInfraBudget,
            ) => {
                // High-water terminal for the cap-exemption
                // (`sched.retry.exempt-infra-cap`, decided by the fold):
                // a leaked store-side placeholder lock or a stuck
                // floor-promote becomes an actionable poison instead of a
                // silent livelock.
                let max = self.retry_policy.max_exempt_infra_retries;
                warn!(
                    drv_hash = %drv_hash,
                    executor_id = %executor_id,
                    exempt_infra_count = decision.counters.exempt_infra_count,
                    max,
                    "exempt infra-retry cap exceeded; poisoning \
                     (likely leaked store lock / stuck floor-promote)"
                );
                self.poison_already_recorded(
                    drv_hash,
                    &format!("max_exempt_infra_retries={max} exceeded: {error_msg}"),
                    report.final_line_count,
                    // Collapsed worker-report poison: fresh execution
                    // (bug_080).
                    rio_proto::VerdictBacking::FreshExecution,
                )
                .await;
            }
            crate::retry_policy::Verdict::Poison(reason) => {
                // The counted infra budget (or, defensively, any other
                // poison reason the fold could ever produce here): the
                // derivation is likely hitting a deterministic failure
                // the worker misclassifies as infra — more retries won't
                // help, and the operator needs the poison signal. The
                // as-built arm performed no RAM increment on this path.
                let max = self.retry_policy.max_infra_retries;
                if !matches!(reason, crate::retry_policy::PoisonReason::InfraBudget) {
                    error!(drv_hash = %drv_hash, ?reason,
                           "decide() returned an unexpected poison reason for an infra failure; \
                            poisoning with the infra-budget message (investigate)");
                }
                warn!(
                    drv_hash = %drv_hash,
                    executor_id = %executor_id,
                    infra_retry_count = decision.counters.infra_count,
                    max,
                    "infrastructure failure: max_infra_retries exhausted, poisoning"
                );
                self.poison_already_recorded(
                    drv_hash,
                    &format!(
                        "max_infra_retries={max} exhausted after infrastructure failures: {error_msg}"
                    ),
                    report.final_line_count,
                    // Collapsed worker-report poison: fresh execution
                    // (bug_080).
                    rio_proto::VerdictBacking::FreshExecution,
                )
                .await;
            }
            _ => {
                // Requeue. The exempt charge and the counted-infra
                // increment + diagnostic anchor are all carried by the
                // appended row and the fold (the refresh above;
                // live059-c: the streak forgiveness lives on the
                // different-class arms, never on elapsed time) —
                // nothing is mutated in place here. NO
                // failed_builders insert, NO retry_count++, NO backoff —
                // infra failures are worker-local and requeue
                // immediately.
                //
                // live059-d: every infra requeue is COUNTED with its
                // charge disposition — the incident's 520 requeues in
                // 23 minutes were observable only as INFO log lines
                // (no counter, no SLI, no alarm on the carousel
                // signature). fund==spend (W12-LD3): `counted`
                // increments exactly when the fold charged
                // `infra_count` this event; `exempt` exactly when it
                // charged `exempt_infra_count` (the uncharged-by-
                // design lane). The rate alarm/SLI on this counter is
                // post-wave ops wiring (the §4 live-ops line).
                metrics::counter!(
                    "rio_scheduler_infra_requeues_total",
                    "charge" => if exempt_from_cap { "exempt" } else { "counted" }
                )
                .increment(1);
                let Some(state) = self.dag.node_mut(drv_hash) else {
                    return FailureHandling::Handled;
                };
                if let Err(e) = state.reset_to_ready() {
                    warn!(drv_hash = %drv_hash, error = %e,
                          "infrastructure failure: reset_to_ready failed, skipping");
                    return FailureHandling::Handled;
                }
                // r[impl sched.retry.store-degraded-uncharged+4]
                // bug_408: the flagged class is the ONE infra shape
                // that does NOT requeue immediately — the fold computed
                // the pacing deadline from the count within the trailing
                // bounded-uncharged run; write
                // it through the live backoff carve-out (the refresh
                // above deliberately preserves the actor-managed value,
                // same convention as the transient site). Epoch→Instant
                // mirrors `rebuild_retry_view_from_ledger`.
                if degraded == StoreDegradedDisposition::Paced
                    && let Some(t) = decision.counters.backoff_until
                {
                    let now_epoch =
                        crate::db::attempts::epoch_now() as crate::retry_policy::AbsTime;
                    let deadline = if t > now_epoch {
                        Instant::now() + std::time::Duration::from_secs(t - now_epoch)
                    } else {
                        Instant::now()
                    };
                    state.retry.backoff_until = Some(deadline);
                    info!(
                        drv_hash = %drv_hash,
                        executor_id = %executor_id,
                        backoff_secs = t.saturating_sub(now_epoch),
                        "store-degraded failure — uncharged requeue, paced backoff"
                    );
                } else {
                    info!(
                        drv_hash = %drv_hash,
                        executor_id = %executor_id,
                        infra_retry_count = state.retry.infra_count,
                        exempt_from_cap,
                        error_msg,
                        "infrastructure failure — retry without poison count"
                    );
                }
                self.requeue_after_recorded_retry(drv_hash);
                metrics::histogram!(
                    "rio_scheduler_attempt_requeue_seconds",
                    "cause" => "worker-report"
                )
                .record(observed_at.elapsed().as_secs_f64());
            }
        }
        FailureHandling::Handled
    }

    // r[impl sched.retry.executor-variant-threshold]
    /// E3a (sh-012): an `ExecutorVariantFailure` — the daemon's
    /// heuristic exit≠0 (`PermanentFailure`) or unclassified
    /// (`MiscFailure`). The verdict CAN vary by executor (a
    /// compute-bound build on a small node and a genuine compile error
    /// are indistinguishable to nix-daemon), so the distinct-executor
    /// poison threshold gates the conclusion: this handler mirrors E1
    /// ([`Self::handle_transient_failure`]) — append the
    /// `executor_variant` row, fold, and act on the verdict — and only
    /// poisons `Permanent` when ≥N distinct executors agree; below
    /// threshold the attempt charges as transient and requeues with
    /// backoff and executor exclusion.
    ///
    /// The inverse cost — a derivation-intrinsic exit≠0 the daemon
    /// classifies as `PermanentFailure` consumes up to `max_retries+1`
    /// attempts before poison — is accepted: bounded by
    /// [`crate::retry_policy::Budget`], converges via executor
    /// exclusion, and never escalates cores (the compute-bound
    /// corroboration gate refuses on low cpu_util).
    pub(super) async fn handle_executor_variant_failure(
        &mut self,
        drv_hash: &DrvHash,
        executor_id: &ExecutorId,
        report: FailureReportCtx<'_>,
        cpu_seconds_total: Option<f64>,
    ) -> FailureHandling {
        let observed_at = Instant::now();
        let mut attempt_row = self.attempt_row_for(
            drv_hash,
            OutcomeClass::ExecutorVariant,
            ReportingParty::Worker,
        );
        if let Some(row) = attempt_row.as_mut() {
            row.executor_id = Some(executor_id.clone());
            row.error_msg = (!report.error_msg.is_empty()).then(|| report.error_msg.to_string());
            row.final_line_count = report.final_line_count;
        }
        // r[impl sched.sla.reactive-floor+5]
        // sh-012 D4 cores axis: BEFORE the verdict, mint the
        // compute-bound witness. cpu_util = cpu_seconds_total /
        // (assigned_deadline × assigned_cores); when ≥ threshold the
        // attempt demonstrably exhausted its parallelism budget and
        // floor.cores doubles. A genuine compile-error exit (cpu_util
        // ≪ threshold) refuses — the inverse-cost bound: 3 attempts at
        // the same shape, never 3× resource. Same prologue shape as
        // `handle_timeout_failure`'s timeout-witness mint.
        let compute_witness = {
            let intent = self
                .dag
                .node(drv_hash)
                .and_then(|s| s.sched.last_intent.as_ref())
                .map(|i| (i.cores, i.deadline_secs))
                .unwrap_or((0, 0));
            super::floor::CorroborationWitness::corroborated_compute_bound(
                cpu_seconds_total,
                intent.0,
                intent.1,
                self.sla_config.compute_bound_threshold,
            )
        };
        let floor_outcome = match compute_witness {
            Some(witness) => self.bump_resource_floor(drv_hash, witness).await,
            None => super::floor::FloorOutcome::default(),
        };
        if let Some(row) = attempt_row.as_mut() {
            row.floor_promoted = floor_outcome.promoted;
            row.floor_at_cap = floor_outcome.at_cap;
        }

        if self.dag.node(drv_hash).is_none() {
            return FailureHandling::Handled;
        }

        // The verdict: decide() over the appended suffix, inside the
        // appending transaction; the verdict's status is persisted on
        // the same connection (threshold poison → Poisoned, retry →
        // Ready). Same shape as E1's appending transaction.
        let (decision, recorded_row) = if let Some(row) = attempt_row {
            let result: Result<Option<crate::retry_policy::Decision>, sqlx::Error> = async {
                // r[impl sched.evidence.durability+4]
                let mut tx = match self.db.begin_fenced(self.serving_generation()).await? {
                    crate::db::FencedBegin::Fenced { .. } => {
                        return Ok(None);
                    }
                    crate::db::FencedBegin::Open(ftx) => ftx,
                };
                let (_, decision) = self.append_and_decide_in_tx(tx.conn(), &row).await?;
                if matches!(decision.verdict, crate::retry_policy::Verdict::Poison(_)) {
                    crate::db::SchedulerDb::persist_poisoned_in_tx(tx.conn(), drv_hash).await?;
                } else {
                    crate::db::SchedulerDb::update_derivation_status_in_tx(
                        tx.conn(),
                        drv_hash,
                        DerivationStatus::Ready,
                        None,
                    )
                    .await?;
                }
                tx.commit().await?;
                Ok(Some(decision))
            }
            .await;
            match result {
                Ok(Some(decision)) => (decision, Some(row)),
                Ok(None) => {
                    self.note_fenced_evidence_write(
                        "executor-variant-failure appending transaction",
                    );
                    return FailureHandling::Handled;
                }
                Err(e) => {
                    warn!(drv_hash = %drv_hash, executor_id = %executor_id, error = %e,
                          "executor-variant failure: appending transaction failed; \
                           derivation stays in its pre-report state pending re-delivery");
                    return FailureHandling::RecordFailed;
                }
            }
        } else {
            // No db_id yet (merge not committed): fold the in-memory
            // history plus a synthetic record for this observation
            // (same shape as E1's uncommitted-merge edge).
            let mut history = self
                .dag
                .node(drv_hash)
                .map(|s| s.attempt_history().to_vec())
                .unwrap_or_default();
            history.push(crate::state::AttemptRecord {
                attempt_id: Uuid::now_v7(),
                event_kind: crate::state::AttemptEventKind::Attempt,
                outcome_class: OutcomeClass::ExecutorVariant,
                exec_id: None,
                executor_id: Some(executor_id.clone()),
                attempt_kind: crate::state::AttemptKind::Build,
                source_node: self.pull_attempt_source_node(drv_hash),
                termination_reason: None,
                reporting_party: ReportingParty::Worker,
                exempt: false,
                floor_promoted: false,
                floor_at_cap: false,
                error_msg: None,
                final_line_count: None,
                resubmit_cycle: 0,
                occurred_at_epoch_secs: crate::db::attempts::epoch_now(),
                recorded_at_epoch_secs: 0.0,
            });
            let decision = crate::retry_policy::decide(
                &history,
                &self.decision_budget(),
                crate::db::attempts::epoch_now() as crate::retry_policy::AbsTime,
            );
            (decision, None)
        };

        if let Some(row) = recorded_row {
            if let Some(state) = self.dag.node_mut(drv_hash) {
                state.push_attempt_record(row.to_record());
            }
            self.refresh_retry_view(drv_hash);
        }

        match decision.verdict {
            crate::retry_policy::Verdict::Poison(
                crate::retry_policy::PoisonReason::TransientBudget,
            ) => {
                let n = decision.counters.count;
                self.poison_already_recorded(
                    drv_hash,
                    &format!("max_retries={n} exhausted after executor-variant failures"),
                    report.final_line_count,
                    rio_proto::VerdictBacking::FreshExecution,
                )
                .await;
            }
            crate::retry_policy::Verdict::Poison(_) => {
                // PoisonReason::Permanent — ≥N distinct executors agreed.
                // The worker's error_msg IS the user-facing reason here
                // (unlike E1's synthesized string): the daemon's exit≠0
                // message is what the operator wants to see.
                self.poison_already_recorded(
                    drv_hash,
                    report.error_msg,
                    report.final_line_count,
                    rio_proto::VerdictBacking::FreshExecution,
                )
                .await;
            }
            _ => {
                // Below threshold and below the per-cycle cap: requeue
                // with backoff (E1's structure). The fold's count
                // already includes this event, hence the -1.
                let backoff = self
                    .retry_policy
                    .backoff_duration(decision.counters.count.saturating_sub(1));
                if let Some(state) = self.dag.node_mut(drv_hash) {
                    state.ensure_running();
                    if let Err(e) = state.transition(DerivationStatus::Failed) {
                        warn!(drv_hash = %drv_hash, error = %e, "Running->Failed transition failed");
                    }
                    state.assigned_executor = None;
                    state.retry.backoff_until = Some(Instant::now() + backoff);
                    info!(
                        drv_hash = %drv_hash,
                        executor_id = %executor_id,
                        retry_count = state.retry.count,
                        distinct_failed = decision.counters.failed_builders.len(),
                        backoff_secs = backoff.as_secs_f64(),
                        error_msg = report.error_msg,
                        "executor-variant failure — requeue (below distinct-executor threshold)"
                    );
                    if let Err(e) = state.transition(DerivationStatus::Ready) {
                        warn!(drv_hash = %drv_hash, error = %e, "Failed->Ready transition failed");
                    } else {
                        metrics::histogram!(
                            "rio_scheduler_attempt_requeue_seconds",
                            "cause" => "worker-report"
                        )
                        .record(observed_at.elapsed().as_secs_f64());
                    }
                }
                for build_id in self.get_interested_builds(drv_hash) {
                    self.emit_progress(build_id);
                }
            }
        }
        FailureHandling::Handled
    }

    /// E3b, collapsed onto `decide()` (Phase 1b): the verdict for a
    /// derivation-intrinsic permanent failure is computed by the fold over the appended
    /// attempt suffix inside the appending transaction; the in-memory
    /// transition, the cached-view refresh, and the terminal epilogue
    /// run only after the commit. A failed transaction leaves the
    /// derivation in its pre-report state and the caller re-delivers
    /// the event.
    pub(super) async fn handle_permanent_failure(
        &mut self,
        drv_hash: &DrvHash,
        executor_id: &ExecutorId,
        report: FailureReportCtx<'_>,
    ) -> FailureHandling {
        let error_msg = report.error_msg;
        // Pre-report precondition (the same one the as-built in-memory
        // transition enforced first): the node exists and is still in a
        // poison-able state. handle_completion's status guards already
        // ensure Assigned/Running; this is defense against a stale
        // report racing a terminal transition.
        match self.dag.node(drv_hash).map(|s| s.status()) {
            Some(
                DerivationStatus::Ready | DerivationStatus::Assigned | DerivationStatus::Running,
            ) => {}
            Some(status) => {
                warn!(drv_hash = %drv_hash, current = ?status,
                      "handle_permanent_failure: not in a poison-able state, skipping");
                return FailureHandling::Handled;
            }
            None => return FailureHandling::Handled,
        }
        // Ledger row for this observed attempt (1a): captured before
        // any transition clears the exec_id carrier.
        let mut attempt_row =
            self.attempt_row_for(drv_hash, OutcomeClass::Permanent, ReportingParty::Worker);
        if let Some(row) = attempt_row.as_mut() {
            row.executor_id = Some(executor_id.clone());
            row.error_msg = (!error_msg.is_empty()).then(|| error_msg.to_string());
            row.final_line_count = report.final_line_count;
        }

        let recorded_row = if let Some(row) = attempt_row {
            // The appending transaction is the decision point: append the
            // row, fold the suffix through decide(), persist Poisoned —
            // commit or leave the derivation untouched.
            let result: Result<Option<crate::retry_policy::Decision>, sqlx::Error> = async {
                // r[impl sched.evidence.durability+4]
                // Claims-floor fence at the appending-transaction start.
                let mut tx = match self.db.begin_fenced(self.serving_generation()).await? {
                    crate::db::FencedBegin::Fenced { .. } => {
                        return Ok(None);
                    }
                    crate::db::FencedBegin::Open(ftx) => ftx,
                };
                let (_, decision) = self.append_and_decide_in_tx(tx.conn(), &row).await?;
                crate::db::SchedulerDb::persist_poisoned_in_tx(tx.conn(), drv_hash).await?;
                tx.commit().await?;
                Ok(Some(decision))
            }
            .await;
            match result {
                Ok(None) => {
                    // Fenced: deposed replica — drop the report (see the
                    // transient handler's fence comment).
                    self.note_fenced_evidence_write("permanent-failure appending transaction");
                    return FailureHandling::Handled;
                }
                Ok(Some(decision)) => {
                    // Permanent statuses poison unconditionally — the fold's
                    // Permanent arm has no other verdict, so anything else
                    // here is a decide()/mapping bug. Log it loudly but keep
                    // the as-built terminal outcome (the equivalence the
                    // battery pins).
                    if !matches!(decision.verdict, crate::retry_policy::Verdict::Poison(_)) {
                        error!(drv_hash = %drv_hash, verdict = ?decision.verdict,
                               "decide() returned a non-poison verdict for a permanent failure; \
                                poisoning anyway (fold bug — investigate)");
                    }
                    Some(row)
                }
                Err(e) => {
                    warn!(drv_hash = %drv_hash, executor_id = %executor_id, error = %e,
                          "permanent failure: appending transaction failed; derivation stays in \
                           its pre-report state pending re-delivery");
                    return FailureHandling::RecordFailed;
                }
            }
        } else {
            // No db_id yet (merge not committed): nothing to append or
            // fold. Degrade to the as-built best-effort poison persist so
            // the terminal outcome is preserved.
            self.persist_poisoned(drv_hash).await;
            None
        };

        let Some(state) = self.dag.node_mut(drv_hash) else {
            return FailureHandling::Handled;
        };
        state.ensure_running();
        if let Err(e) = state.transition(DerivationStatus::Poisoned) {
            // Should be unreachable given the precondition above (the
            // actor is single-threaded, so nothing ran between the check
            // and here besides our own awaits). Keep the as-built guard:
            // don't cascade on a rejected transition.
            warn!(drv_hash = %drv_hash, error = %e, current = ?state.status(),
                  "handle_permanent_failure: ->Poisoned transition rejected, skipping");
            return FailureHandling::Handled;
        }
        state.retry.poisoned_at = Some(crate::state::RecoveredInstant::fresh_now());
        // I-209: which builder produced the permanent failure is carried
        // by the appended `permanent` row (the fold's diagnostics-only
        // `failed_builders` insert), so the refreshed view and the
        // ListPoisoned aggregate both show it — rio-cli/kubectl would
        // otherwise show an empty array as "never ran".
        if let Some(row) = recorded_row {
            state.push_attempt_record(row.to_record());
        }
        self.refresh_retry_view(drv_hash);
        self.unpin_best_effort(drv_hash).await;

        self.terminal_failure_epilogue(
            drv_hash,
            error_msg,
            rio_proto::types::BuildResultStatus::PermanentFailure,
            report.final_line_count,
            // Worker-reported permanent failure: the report carries the
            // execution's own line count — fresh execution (bug_080).
            rio_proto::VerdictBacking::FreshExecution,
        )
        .await;
        FailureHandling::Handled
    }

    /// Worker-side timeout (`BuildResultStatus::TimedOut`): promote
    /// `resource_floor.deadline_secs` and reset to Ready for re-dispatch on a
    /// larger class — bounded by `max_timeout_retries`, after which
    /// terminal `Cancelled` (retriable on EXPLICIT resubmit only).
    ///
    /// D7: with per-intent `activeDeadlineSeconds`
    /// (`r[ctrl.ephemeral.intent-deadline]`) the next dispatch gets a
    /// doubled `floor.deadline_secs` and a proportionally longer deadline,
    /// so "same inputs → same timeout → storm" no longer holds for
    /// the first N retries. The cap (default 4: tiny→xlarge) ensures
    /// a genuinely-infinite build still goes terminal.
    ///
    /// Separate `timeout_retry_count` (NOT `retry_count` /
    /// `infra_retry_count`): timeouts neither consume the transient
    /// budget nor get the infra time-window reset (sparse timeouts
    /// over hours are still the same hung build).
    ///
    /// Terminal path transitions to `Cancelled` (not `Poisoned`):
    /// `Cancelled` is in `is_retriable_on_resubmit`, `Poisoned` has a
    /// 24h TTL that's way too aggressive for "ran out of time". Same
    /// cascade/events/build-fail side-effects as
    /// `handle_permanent_failure` — the build still fails THIS time,
    /// just without the 24h resubmit lockout.
    // r[impl sched.timeout.promote-on-exceed+3]
    /// E4, collapsed onto `decide()` (Phase 1b): the under-cap requeue
    /// vs at-cap terminal-`Cancelled` verdict comes from the fold over
    /// the appended attempt suffix inside the appending transaction.
    /// The deadline-floor doubling stays at this site (it is a floor
    /// action, not a budget decision), the timeout count is carried by
    /// the appended row alone (the cached view is refreshed from the
    /// fold), and a failed transaction leaves the derivation in its
    /// pre-report state for re-delivery.
    pub(super) async fn handle_timeout_failure(
        &mut self,
        drv_hash: &DrvHash,
        executor_id: &ExecutorId,
        report: FailureReportCtx<'_>,
    ) -> FailureHandling {
        // OA1 interval (ii), worker-report cause (see
        // handle_transient_failure).
        let observed_at = Instant::now();
        let error_msg = report.error_msg;
        // Ledger row for this observed attempt (1a): captured before
        // the floor bump / reset clears the exec_id carrier; the floor
        // flags are filled in once the bump's outcome is known.
        let mut attempt_row =
            self.attempt_row_for(drv_hash, OutcomeClass::Timeout, ReportingParty::Worker);
        if let Some(row) = attempt_row.as_mut() {
            row.executor_id = Some(executor_id.clone());
            row.error_msg = (!error_msg.is_empty()).then(|| error_msg.to_string());
            row.final_line_count = report.final_line_count;
        }
        // Bump BEFORE the verdict / state transition: even the
        // terminal-path attempt should record that this deadline was
        // inadequate, so an explicit resubmit starts at the doubled
        // floor instead of replaying the timeout. Same shape as
        // I-199's handle_infrastructure_failure prologue.
        //
        // bug_102 — the status-borne axis joins the corroboration law:
        // TimedOut rides BuildResultStatus (no FailureClassification),
        // so the wave-11 typed-claim gate never saw it and this bump
        // was unconditional — a hostile builder ratcheted the
        // cross-tenant 24h deadline floor in ~5 cheap zero-age
        // reports. The witness anchors on the scheduler's OWN clock:
        // the attempt demonstrably RAN at least half its assigned
        // deadline (`running_since` elapsed vs the reconciled
        // `last_intent.deadline_secs` — neither mintable by the
        // worker). Uncorroborated => classify-only: the refusal is a
        // typed counted letter and the verdict flow below is
        // untouched (timeouts still charge `timeout_count`).
        let timeout_witness = {
            let state = self.dag.node(drv_hash);
            let attempt_open = state
                .and_then(|s| s.running_since)
                .map(|since| since.elapsed());
            let assigned_deadline = state
                .and_then(|s| s.sched.last_intent.as_ref())
                .map(|i| i.deadline_secs)
                .unwrap_or(0);
            super::floor::CorroborationWitness::corroborated_timeout(
                attempt_open,
                assigned_deadline,
            )
        };
        let floor_outcome = match timeout_witness {
            Some(witness) => self.bump_resource_floor(drv_hash, witness).await,
            None => {
                warn!(
                    drv_hash = %drv_hash, executor_id = %executor_id,
                    "refusing uncorroborated timeout claim (attempt-open \
                     duration below half the assigned deadline, or no \
                     scheduler-side anchor); classify-only"
                );
                metrics::counter!(
                    "rio_scheduler_uncorroborated_sizing_claim_total",
                    "class" => "timed_out"
                )
                .increment(1);
                super::floor::FloorOutcome::default()
            }
        };
        if let Some(row) = attempt_row.as_mut() {
            row.floor_promoted = floor_outcome.promoted;
            row.floor_at_cap = floor_outcome.at_cap;
        }

        if self.dag.node(drv_hash).is_none() {
            return FailureHandling::Handled;
        }

        // The verdict: decide() over the appended suffix, inside the
        // appending transaction, with the verdict's status persisted on
        // the same connection (Requeue → Ready, Cancel → Cancelled).
        // The uncommitted-merge edge (no db row) keeps the as-built
        // RAM-checked verdict — there is nothing to append or persist.
        let (verdict, recorded_row) = if let Some(row) = attempt_row {
            let result: Result<Option<crate::retry_policy::Decision>, sqlx::Error> = async {
                // r[impl sched.evidence.durability+4]
                // Claims-floor fence at the appending-transaction start.
                let mut tx = match self.db.begin_fenced(self.serving_generation()).await? {
                    crate::db::FencedBegin::Fenced { .. } => {
                        return Ok(None);
                    }
                    crate::db::FencedBegin::Open(ftx) => ftx,
                };
                let (_, decision) = self.append_and_decide_in_tx(tx.conn(), &row).await?;
                let status = match decision.verdict {
                    crate::retry_policy::Verdict::Cancel => DerivationStatus::Cancelled,
                    _ => DerivationStatus::Ready,
                };
                crate::db::SchedulerDb::update_derivation_status_in_tx(
                    tx.conn(),
                    drv_hash,
                    status,
                    None,
                )
                .await?;
                tx.commit().await?;
                Ok(Some(decision))
            }
            .await;
            match result {
                Ok(Some(decision)) => (decision.verdict, Some(row)),
                Ok(None) => {
                    // Fenced: deposed replica — drop the report (see the
                    // transient handler's fence comment).
                    self.note_fenced_evidence_write("timeout-failure appending transaction");
                    return FailureHandling::Handled;
                }
                Err(e) => {
                    warn!(drv_hash = %drv_hash, executor_id = %executor_id, error = %e,
                          "timeout failure: appending transaction failed; derivation stays in \
                           its pre-report state pending re-delivery");
                    return FailureHandling::RecordFailed;
                }
            }
        } else {
            let under_cap = self
                .dag
                .node(drv_hash)
                .is_some_and(|s| s.retry.timeout_count < self.retry_policy.max_timeout_retries);
            (
                if under_cap {
                    crate::retry_policy::Verdict::Requeue
                } else {
                    crate::retry_policy::Verdict::Cancel
                },
                None,
            )
        };

        let Some(state) = self.dag.node_mut(drv_hash) else {
            return FailureHandling::Handled;
        };

        if verdict == crate::retry_policy::Verdict::Cancel {
            // ── Terminal path (cap exhausted) ────────────────────────
            warn!(
                drv_hash = %drv_hash,
                executor_id = %executor_id,
                timeout_retry_count = state.retry.timeout_count,
                max = self.retry_policy.max_timeout_retries,
                "timeout: max_timeout_retries exhausted, transitioning to Cancelled"
            );
            state.ensure_running();
            if let Err(e) = state.transition(DerivationStatus::Cancelled) {
                warn!(drv_hash = %drv_hash, error = %e, current = ?state.status(),
                      "handle_timeout_failure: ->Cancelled transition rejected, skipping");
                return FailureHandling::Handled;
            }
            if let Some(row) = recorded_row {
                state.push_attempt_record(row.to_record());
            }
            self.refresh_retry_view(drv_hash);
            self.unpin_best_effort(drv_hash).await;

            self.terminal_failure_epilogue(
                drv_hash,
                error_msg,
                rio_proto::types::BuildResultStatus::TimedOut,
                report.final_line_count,
                // Worker-reported timeout at the retry cap: the timed-
                // out execution exists and its partial log is the right
                // pointer — fresh execution (bug_080).
                rio_proto::VerdictBacking::FreshExecution,
            )
            .await;
            return FailureHandling::Handled;
        }

        // ── Retry path (under cap; Requeue verdict) ──────────────────
        state.ensure_running();
        if let Err(e) = state.reset_to_ready() {
            warn!(drv_hash = %drv_hash, error = %e,
                  "timeout failure: reset_to_ready failed, skipping");
            return FailureHandling::Handled;
        }
        // NO insert into failed_builders (timeout is not a per-
        // worker problem — the SAME worker with a longer deadline
        // would succeed). NO retry_count++ (separate counter).
        // NO backoff (next dispatch's longer deadline IS the
        // backoff). I-200: every TimedOut consumes budget. NOT
        // symmetric with `handle_infrastructure_failure` — infra
        // has an outer `!promoted` exemption (via `exempt_from_
        // cap`); timeout deliberately does not, so
        // `max_timeout_retries` bounds TOTAL attempts. Covers
        // cold-start (no [sla], est=None, floor=0 → {false,false})
        // where `if promoted` would never increment → infinite
        // retry until globalTimeout. The charge is the appended
        // `timeout` row; the cached view picks it up at the refresh
        // below.
        if let Some(row) = recorded_row {
            state.push_attempt_record(row.to_record());
        }
        self.refresh_retry_view(drv_hash);
        if let Some(state) = self.dag.node(drv_hash) {
            info!(
                drv_hash = %drv_hash,
                executor_id = %executor_id,
                timeout_retry_count = state.retry.timeout_count,
                max = self.retry_policy.max_timeout_retries,
                promoted = floor_outcome.promoted,
                "timeout — bumped deadline floor, retrying"
            );
        }
        self.requeue_after_recorded_retry(drv_hash);
        metrics::histogram!(
            "rio_scheduler_attempt_requeue_seconds",
            "cause" => "worker-report"
        )
        .record(observed_at.elapsed().as_secs_f64());
        FailureHandling::Handled
    }

    /// Transitively walk parents of a poisoned derivation and transition all
    /// Queued/Ready/Created ancestors to DependencyFailed.
    ///
    /// Returns the set of derivations actually transitioned. Callers
    /// union the `interested_builds` of each transitioned node with the
    /// trigger's — a cascaded node may belong to a merged build that
    /// the trigger does NOT (shared dep, different upstream). Without
    /// the union, that merged build hangs Active forever.
    ///
    /// Without the cascade itself, keepGoing builds with a poisoned
    /// leaf hang forever: parents stay Queued, so completed+failed
    /// never reaches total.
    //
    // r[impl sched.poison.cascade-dependents]
    // r[impl sched.db.batch-unnest]
    // Collect-then-batch-persist: the BFS runs entirely in-memory
    // (transitions), then ONE persist_status_batch
    // for all transitioned ancestors. Safe because recovery re-cascades
    // from the original poisoned leaf on partial persist. The previous
    // per-step await was unbounded N×PG-RTT inside the actor (no
    // MAX_CASCADE_NODES cap here).
    pub(super) async fn cascade_dependency_failure(
        &mut self,
        poisoned_hash: &DrvHash,
    ) -> HashSet<DrvHash> {
        let mut to_visit: Vec<DrvHash> = self.dag.get_parents(poisoned_hash);
        let mut visited: HashSet<DrvHash> = HashSet::new();
        let mut transitioned: HashSet<DrvHash> = HashSet::new();

        while let Some(parent_hash) = to_visit.pop() {
            if !visited.insert(parent_hash.clone()) {
                continue; // already processed
            }

            let Some(state) = self.dag.node_mut(&parent_hash) else {
                continue;
            };

            // Only cascade to derivations that haven't started yet.
            // Assigned/Running derivations will complete or fail on their own
            // (and their completion handler will re-cascade if they succeed
            // but a sibling dep is dead — but actually they'd never become
            // Ready in the first place since all_deps_completed is false).
            // r[impl sched.preempt.never-running]
            if !matches!(
                state.status(),
                DerivationStatus::Queued | DerivationStatus::Ready | DerivationStatus::Created
            ) {
                continue;
            }

            if let Err(e) = state.transition(DerivationStatus::DependencyFailed) {
                warn!(drv_hash = %parent_hash, error = %e, "cascade ->DependencyFailed transition failed");
                continue;
            }

            debug!(
                drv_hash = %parent_hash,
                poisoned_dep = %poisoned_hash,
                "cascaded DependencyFailed from poisoned dependency"
            );

            // Continue cascade: this parent's parents also cannot complete.
            to_visit.extend(self.dag.get_parents(&parent_hash));
            transitioned.insert(parent_hash);
        }
        if !transitioned.is_empty() {
            self.record_cascade_attempts_and_status(&transitioned).await;
        }
        transitioned
    }

    /// 1a appending transaction for the dependency-failure cascade: one
    /// `outcome_class='cascade'` ledger row per newly-DependencyFailed
    /// dependent (no exec_id, no executor — these nodes never ran) plus
    /// the batched DependencyFailed status persist, committed together
    /// in one actor-owned transaction (mirroring how the status persist
    /// already batched). Best-effort like every 1a append: on failure,
    /// log and proceed with the in-memory transitions — recovery
    /// re-cascades from the poisoned leaf on partial persist, as today.
    async fn record_cascade_attempts_and_status(&mut self, transitioned: &HashSet<DrvHash>) {
        let refs: Vec<&str> = transitioned.iter().map(DrvHash::as_str).collect();
        // One row per dependent whose merge has committed (db_id
        // present); the status batch still covers every hash either way.
        let rows: Vec<(DrvHash, AttemptRow)> = transitioned
            .iter()
            .filter_map(|h| {
                let mut row =
                    self.attempt_row_for(h, OutcomeClass::Cascade, ReportingParty::Scheduler)?;
                row.exec_id = None;
                row.executor_id = None;
                Some((h.clone(), row))
            })
            .collect();
        let batch: Vec<AttemptRow> = rows.iter().map(|(_, r)| r.clone()).collect();
        let result: Result<bool, sqlx::Error> = async {
            // r[impl sched.evidence.durability+4]
            // Claims-floor fence at the appending-transaction start: a
            // deposed replica's cascade persists nothing (the successor's
            // recovery recomputes the cascade from the poisoned row).
            let mut tx = match self.db.begin_fenced(self.serving_generation()).await? {
                crate::db::FencedBegin::Fenced { .. } => {
                    return Ok(false);
                }
                crate::db::FencedBegin::Open(ftx) => ftx,
            };
            crate::db::SchedulerDb::append_attempts_batch(tx.conn(), &batch).await?;
            crate::db::SchedulerDb::update_derivation_status_batch_in_tx(
                tx.conn(),
                &refs,
                DerivationStatus::DependencyFailed,
            )
            .await?;
            tx.commit().await?;
            Ok(true)
        }
        .await;
        match result {
            Ok(true) => {
                for (hash, row) in rows {
                    if let Some(state) = self.dag.node_mut(&hash) {
                        state.push_attempt_record(row.to_record());
                    }
                    self.refresh_retry_view(&hash);
                }
            }
            Ok(false) => {
                self.note_fenced_evidence_write("cascade appending transaction");
            }
            Err(e) => {
                error!(count = refs.len(), error = %e,
                       "failed to batch-persist cascade attempt rows + DependencyFailed status");
            }
        }
    }

    /// Collect the union of `interested_builds` over the trigger
    /// derivation AND all cascaded derivations. A cascaded node may
    /// belong to a merged build that the trigger does not — without
    /// this union, `handle_derivation_failure` is never called for
    /// that build and it hangs Active forever.
    fn union_interested_with_cascaded(
        &self,
        trigger: &DrvHash,
        cascaded: &HashSet<DrvHash>,
    ) -> HashSet<Uuid> {
        let mut builds: HashSet<Uuid> = self.get_interested_builds(trigger).into_iter().collect();
        for h in cascaded {
            if let Some(s) = self.dag.node(h) {
                builds.extend(s.interested_builds.iter().copied());
            }
        }
        builds
    }

    /// `status` is the failing derivation's wire classification — every
    /// caller must decide it at the call site (the parameter is the
    /// forcing function: a future failure path cannot forget the
    /// classification without failing to compile).
    pub(super) async fn handle_derivation_failure(
        &mut self,
        build_id: Uuid,
        drv_hash: &DrvHash,
        status: rio_proto::types::BuildResultStatus,
    ) {
        // r[impl sched.build.terminal-status-settled+3]
        // A build that already reached a terminal state keeps its
        // settled outcome: a shared node failing later (re-dispatched by
        // another build during the cleanup window) must not rewrite this
        // build's error_summary/failed_derivation, re-persist its counts
        // from the mutated DAG, or re-run its failure handling. Interest
        // cleanup for resident terminal builds is owned by
        // handle_cleanup_terminal_build.
        if self
            .builds
            .get(&build_id)
            .is_some_and(|b| b.state().is_terminal())
        {
            return;
        }
        // Sync counts from DAG ground truth. The cascade may have transitioned
        // additional parents to DependencyFailed; those must be counted here.
        self.update_build_counts(build_id).await;

        let Some(build) = self.builds.get_mut(&build_id) else {
            return;
        };

        // r[impl sched.build.keep-going]
        // Record the FIRST failure regardless of keep_going. The
        // previous `!keep_going`-only assignment meant a keep_going
        // build's eventual `BuildFailed` had `error_message=""` and
        // `failed_derivation=""`. `note_first_failure` keeps the first
        // failure across multiple calls under keep_going, and the trio
        // (summary/culprit/classification) travels as ONE struct so the
        // fields cannot name different failures (union-only builds
        // record the TRIGGER's status, matching the trigger hash the
        // culprit field records for them).
        build.note_first_failure(crate::state::FirstFailure {
            summary: format!("derivation {drv_hash} failed"),
            failed_drv: Some(drv_hash.to_string()),
            status: Some(status),
        });

        if !build.keep_going {
            // Fail the entire build immediately. Cancel remaining
            // derivations first — without this, sole-interest Queued/
            // Ready/Assigned derivations for this build linger:
            // Assigned ones keep burning worker CPU, Queued/Ready
            // ones stay pull-claimable. cancel_build_derivations
            // transitions DependencyFailed/Cancelled (the controller's
            // Job deletion aborts in-flight pods) + removes build
            // interest.
            self.cancel_build_derivations(
                build_id,
                &format!("build {build_id} failed fast (keep_going=false)"),
            )
            .await;
            if let Err(e) = self.transition_build_to_failed(build_id).await {
                error!(build_id = %build_id, error = %e, "failed to persist build-failed transition");
            }
        } else {
            // keepGoing: sticky failure already recorded above; failed_count
            // is DAG-derived and resets to 0 if the poisoned node is later
            // removed via ClearPoison/TTL. Check if all derivations resolved.
            self.check_build_completion(build_id).await;
        }
    }
}

// =======================================================================
// The late-report classifier law (round-9 WO-S1-1 — red 2's post-fix
// form + the W9-C product cells). The PRE-FIX transcript (recorded in
// the owning commit body): `late_report_effect(Some(exec), Built, 42)`
// classified `Nothing` — the shipped law discarded the registration
// with the acknowledgment.
// =======================================================================
#[cfg(test)]
mod late_report_classifier_tests {
    use super::*;
    use rio_proto::types::BuildResultStatus as S;

    fn out(name: &str) -> crate::domain::BuiltOutput {
        crate::domain::BuiltOutput {
            output_name: name.into(),
            output_path: format!("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-{name}"),
            output_hash: vec![0u8; 32],
        }
    }

    /// red 2 flipped: (Built, Cancelled-resident) classifies Register
    /// — the pre-fix law classified Nothing (transcript in the commit
    /// body; the signed Q1 invariant reverses the design intent).
    #[test]
    fn built_on_cancelled_classifies_register() {
        let effect = late_report_effect(
            Some(ReportingExec(Uuid::new_v4())),
            LateNodeContext::Cancelled,
            S::Built,
            42,
            vec![out("out")],
        );
        assert_eq!(
            effect,
            LateReportEffect::Register {
                outputs: vec![out("out")]
            },
            "a late BUILT report on a cancelled drv is registrable completed \
             work — the registration arm, not the Nothing arm"
        );
    }

    /// red 2's sibling flipped: (Built, Unknown/evicted) ALSO
    /// registers — the invariant is unconditional over the grace
    /// (pre-fix this cell was unrepresentable: the pre-chokepoint
    /// early-returns discarded it before classification).
    #[test]
    fn built_on_unknown_classifies_register() {
        let effect = late_report_effect(
            None,
            LateNodeContext::Unknown,
            S::Built,
            0,
            vec![out("out")],
        );
        assert!(
            matches!(effect, LateReportEffect::Register { .. }),
            "the beyond-grace face classifies Register (identity cold-resolves \
             in the applier); got {effect:?}"
        );
    }

    /// The success family closes over all three success statuses (the
    /// epilogue treats them identically; carving Built-only would mint
    /// an artificial sibling — divergence vs the WO's `(Built, …)`
    /// phrasing recorded in the commit body).
    #[test]
    fn success_family_registers_uniformly() {
        for status in [S::Built, S::Substituted, S::AlreadyValid] {
            let effect = late_report_effect(
                None,
                LateNodeContext::Cancelled,
                status,
                0,
                vec![out("out")],
            );
            assert!(
                matches!(effect, LateReportEffect::Register { .. }),
                "{status:?} is success-class: registrable"
            );
        }
    }

    /// The Other-context cell: a resident NON-cancelled node (e.g. a
    /// duplicate report on a Completed drv) never registers late —
    /// its registration rode (or rides) the admitted epilogue.
    #[test]
    fn built_on_other_context_classifies_nothing() {
        let effect =
            late_report_effect(None, LateNodeContext::Other, S::Built, 0, vec![out("out")]);
        assert_eq!(effect, LateReportEffect::Nothing);
    }

    /// Empty validated outputs ⇒ nothing to register (a success report
    /// with no surviving outputs after the boundary filters).
    #[test]
    fn empty_outputs_classify_nothing() {
        let effect = late_report_effect(None, LateNodeContext::Cancelled, S::Built, 0, vec![]);
        assert_eq!(effect, LateReportEffect::Nothing);
    }

    /// The fill law is UNCHANGED by the registration arm (disjoint on
    /// status): a late Cancelled report with identity + count still
    /// gap-fills; without identity it still classifies Nothing (the
    /// ghost-exec conservatism, bug_098).
    #[test]
    fn fill_law_unchanged() {
        let exec = ReportingExec(Uuid::new_v4());
        assert_eq!(
            late_report_effect(
                Some(exec),
                LateNodeContext::Cancelled,
                S::Cancelled,
                7,
                vec![]
            ),
            LateReportEffect::FillCancelledCount { exec, count: 7 }
        );
        assert_eq!(
            late_report_effect(None, LateNodeContext::Cancelled, S::Cancelled, 7, vec![]),
            LateReportEffect::Nothing,
            "ghost exec is never stamped"
        );
        assert_eq!(
            late_report_effect(
                Some(exec),
                LateNodeContext::Cancelled,
                S::Cancelled,
                0,
                vec![]
            ),
            LateReportEffect::Nothing,
            "zero count = not reported"
        );
    }

    /// Failure-class late reports never register (TransientFailure /
    /// PermanentFailure / TimedOut / Unspecified carry no completed
    /// upload claim).
    #[test]
    fn failure_class_never_registers() {
        for status in [
            S::TransientFailure,
            S::PermanentFailure,
            S::TimedOut,
            S::Unspecified,
        ] {
            for ctx in [
                LateNodeContext::Cancelled,
                LateNodeContext::Unknown,
                LateNodeContext::Other,
            ] {
                let effect = late_report_effect(None, ctx, status, 0, vec![out("out")]);
                assert_eq!(
                    effect,
                    LateReportEffect::Nothing,
                    "({status:?}, {ctx:?}) must not register"
                );
            }
        }
    }
}
