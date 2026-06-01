//! Materialization-job actor logic (substitution-replacement Phase A).
//!
//! Everything here is reachable ONLY when materialization dispatch is
//! enabled (`materialization_cfg.enabled`); flag-off, the only
//! reachable code is the empty-list answer and the kind=BUILD
//! passthrough in pull admission. Design: substitution-replacement-
//! design.md §2; spec: sched.materialize.{job,routing,pinning}.
// r[impl sched.materialize.job]

use tokio::sync::oneshot;
use tracing::warn;
use uuid::Uuid;

use crate::db::materialization::FencedJobCreate;
use crate::state::{DerivationStatus, DrvHash, ExecutorId, JobOrigin};

use super::DagActor;

/// What `ListMaterializationJobs` returns per job (the proto
/// descriptor's actor-side source).
#[derive(Debug, Clone)]
pub struct JobDescriptor {
    /// `materialization_jobs.job_id`.
    pub job_id: Uuid,
    /// The derivation hash (the DAG key / claim intent).
    pub drv_hash: String,
    /// Creating build's tenant; `None` = no tenant context (the
    /// executor re-resolves at execution time — design §2.2 item 3).
    pub tenant_id: Option<Uuid>,
    /// Which classification demanded the job (observability).
    pub origin: crate::state::JobOrigin,
}

impl JobDescriptor {
    fn from_row(row: crate::db::materialization::MaterializationJobRow) -> Self {
        Self {
            job_id: row.job_id,
            drv_hash: row.drv_hash,
            tenant_id: row.tenant_id,
            origin: row.origin,
        }
    }
}

/// The in-memory job view entry (droppable, never written back —
/// design handoff input 1's "derived droppable view"). Authority lives
/// in PG: creation dedup is the partial-unique index; consumption is
/// the fenced exec_id-keyed transaction. The view exists so pull
/// admission can answer from memory inside the actor turn; it is
/// populated only by the flag-gated creation paths (so flag-off it is
/// always empty) and rebuilt by query at recovery (Phase B).
#[derive(Debug, Clone)]
pub(crate) struct JobViewEntry {
    /// `materialization_jobs.job_id`.
    pub job_id: Uuid,
    /// Backoff expiry while parked; `None` = not parked.
    pub parked_until: Option<std::time::Instant>,
    /// `Some(identity)` while an open materialization attempt exists.
    pub claimed_by: Option<ExecutorId>,
}

/// One job the merge transaction created (or found via the dedup) —
/// what `persist_merge_to_db` returns to the post-commit phase so the
/// in-memory view is fed OUTSIDE the transaction (a rolled-back merge
/// must leave no view entry; the view is a cache, not an authority).
#[derive(Debug, Clone)]
pub(crate) struct CreatedJob {
    pub drv_hash: DrvHash,
    pub job_id: Uuid,
    /// False when the dedup found a pre-existing unresolved job.
    pub created: bool,
    pub origin: JobOrigin,
}

impl DagActor {
    /// Leader-served job listing (the store's poll). Flag-off, standby,
    /// or no jobs → empty vec (never an error — the AS-6 mixed-flag
    /// posture: a flag-on store polling a flag-off scheduler hangs
    /// harmlessly on empty lists).
    // r[impl sched.materialize.job]
    pub(super) async fn handle_list_materialization_jobs(
        &mut self,
        limit: u32,
        reply: oneshot::Sender<Vec<JobDescriptor>>,
    ) {
        let jobs = if !self.materialization_cfg.enabled || !self.leader.is_leader() {
            Vec::new()
        } else {
            match self
                .db
                .list_claimable_materialization_jobs(i64::from(limit.min(256)))
                .await
            {
                Ok(rows) => rows.into_iter().map(JobDescriptor::from_row).collect(),
                Err(e) => {
                    warn!(error = %e, "ListMaterializationJobs query failed; answering empty");
                    Vec::new()
                }
            }
        };
        let _ = reply.send(jobs);
    }

    /// THE single job-creation helper for callers with NO enclosing
    /// transaction (every §2.1 probe-partition site calls this one fn —
    /// the "one helper" the design's B7 disposition requires; the merge
    /// sites use the in-tx core inside `persist_merge_to_db` instead).
    /// No-op flag-off and on standby. Creates the job row fenced +
    /// dedup'd, updates the in-memory view, and records the wanted
    /// relation for the creating build when one is named.
    ///
    /// Returns whether an unresolved job exists for the node after the
    /// call (created now or found by the dedup).
    // r[impl sched.materialize.job]
    pub(super) async fn create_materialization_job_if_enabled(
        &mut self,
        drv_hash: &DrvHash,
        origin: JobOrigin,
        creating_build: Option<Uuid>,
    ) -> bool {
        if !self.materialization_cfg.enabled || !self.leader.is_leader() {
            return false;
        }
        let Some(state) = self.dag.node(drv_hash) else {
            return false;
        };
        let Some(db_id) = state.db_id else {
            return false;
        };
        // Skip Substituting nodes (the AS-6 flip-boundary guard: a
        // flag-off-era walk in flight owns the node; creating a job
        // under it would race the walk's completion).
        if state.status() == DerivationStatus::Substituting {
            return false;
        }
        // Tenant: any live interested build's tenant (substitution is
        // content-addressed, so whose upstream config we use is
        // irrelevant to the result — the same derivation
        // probe_substitute_auth uses). NULL = no tenant context; the
        // executor re-resolves at execution time (design §2.2 item 3 /
        // PDQ-8).
        let tenant: Option<Uuid> = state
            .interested_builds
            .iter()
            .filter_map(|bid| self.builds.get(bid))
            .find_map(|b| b.tenant_id);
        let serving_generation = self.serving_generation();
        match self
            .db
            .create_materialization_job_fenced(
                db_id,
                drv_hash.as_str(),
                tenant,
                origin,
                serving_generation,
            )
            .await
        {
            Ok(FencedJobCreate::Applied { job_id, created }) => {
                self.materialization_jobs.insert(
                    drv_hash.clone(),
                    JobViewEntry {
                        job_id,
                        parked_until: None,
                        claimed_by: None,
                    },
                );
                if created {
                    metrics::counter!(
                        "rio_scheduler_materialization_jobs_created_total",
                        "origin" => origin.as_str()
                    )
                    .increment(1);
                }
                // Interest: the creating build's wanted-relation row
                // (merge-tx callers write the relation for ALL nodes in
                // the tx instead — this arm covers the probe-partition
                // path where no merge tx is open).
                if let Some(build_id) = creating_build {
                    self.record_wanted_for_build_node(build_id, drv_hash).await;
                }
                true
            }
            Ok(FencedJobCreate::Fenced) => {
                self.note_fenced_evidence_write("materialization job create");
                false
            }
            Err(e) => {
                warn!(drv_hash = %drv_hash, error = %e, "materialization job create failed");
                false
            }
        }
    }

    /// Standalone fenced wanted-relation write for one (build, node)
    /// pair — the probe-partition path's interest registration (the
    /// merge path writes the relation for all nodes inside its own tx).
    /// Best-effort: a failure leaves the durable relation behind the
    /// in-memory interest, which the next merge of the build repairs.
    pub(super) async fn record_wanted_for_build_node(
        &mut self,
        build_id: Uuid,
        drv_hash: &DrvHash,
    ) {
        let Some((db_id, wanted)) = self
            .dag
            .node(drv_hash)
            .and_then(|s| s.db_id.map(|id| (id, s.wanted_output_names.clone())))
        else {
            return;
        };
        let rows = [crate::db::wanted::WantedRow {
            build_id,
            derivation_id: db_id,
            wanted_output_names: &wanted,
        }];
        match self
            .db
            .record_wanted_fenced(self.serving_generation(), &rows)
            .await
        {
            Ok(crate::db::FencedWrite::Applied(_)) => {}
            Ok(crate::db::FencedWrite::Fenced) => {
                self.note_fenced_evidence_write("wanted relation record");
            }
            Err(e) => {
                warn!(drv_hash = %drv_hash, build_id = %build_id, error = %e,
                      "wanted-relation record failed (best-effort)");
            }
        }
    }

    /// Post-commit feed of the in-memory job view from the merge
    /// transaction's created-jobs list. Called only AFTER the merge tx
    /// committed (never inside it — a rolled-back merge must leave no
    /// view entry), in the same post-commit phase that seeds states and
    /// spawns walks.
    pub(super) fn note_created_materialization_jobs(&mut self, created: &[CreatedJob]) {
        for job in created {
            self.materialization_jobs.insert(
                job.drv_hash.clone(),
                JobViewEntry {
                    job_id: job.job_id,
                    parked_until: None,
                    claimed_by: None,
                },
            );
            if job.created {
                metrics::counter!(
                    "rio_scheduler_materialization_jobs_created_total",
                    "origin" => job.origin.as_str()
                )
                .increment(1);
            }
        }
    }

    /// Project the node's materialization-job state for pull admission
    /// (the kernel's `JobView` input). Flag-off: always `None` — the
    /// view map is never populated, and the projection never even looks
    /// (so the flag-off pull path does zero extra work).
    pub(super) fn materialization_job_view(
        &self,
        drv_hash: &DrvHash,
        pulling_identity: &ExecutorId,
    ) -> rio_evidence_kernel::pull::JobView {
        use rio_evidence_kernel::pull::JobView;
        if !self.materialization_cfg.enabled {
            return JobView::None;
        }
        match self.materialization_jobs.get(drv_hash) {
            None => JobView::None,
            Some(entry) => match &entry.claimed_by {
                Some(holder) => JobView::Claimed {
                    held_by_puller: holder == pulling_identity,
                },
                None => JobView::Pending {
                    parked: entry
                        .parked_until
                        .is_some_and(|until| until > std::time::Instant::now()),
                },
            },
        }
    }

    /// Note a materialization claim in the in-memory view (called by
    /// the pull mint after the fenced transaction committed for a
    /// materialization-kind delivery). Reachable only flag-on.
    pub(super) fn note_materialization_claimed(&mut self, drv_hash: &DrvHash, holder: &ExecutorId) {
        if let Some(entry) = self.materialization_jobs.get_mut(drv_hash) {
            entry.claimed_by = Some(holder.clone());
        }
    }

    // r[impl sched.materialize.job]
    /// BC-4 (Phase B): emit the SUBSTITUTING DerivationEvent at
    /// materialization-claim intake. The event KIND is wire-retained;
    /// only its emission site moves — from walk-spawn (which never runs
    /// for fresh flag-on work) to the claim. The gateway's
    /// actSubstitute/actCopyPath pair creation keys on this kind
    /// (rio-gateway handler/build.rs `relay_derivation_status`,
    /// Substituting arm) and is untouched (BC-4's contract). Mirrors the
    /// walk-spawn emission shape (`spawn_substitute_fetches`): one event
    /// per interested build + a progress snapshot so the queued/running
    /// flip is visible.
    ///
    /// Called by the pull mint INSTEAD of `emit_assignment_started` for
    /// materialization-kind mints — STARTED is one of the gateway's
    /// pair-STOP triggers, so emitting it here would close the pair the
    /// instant it opened (and misrepresent substitution work as a
    /// builder dispatch).
    ///
    /// Reachable only flag-on (no materialization mint exists flag-off),
    /// so flag-off event streams are byte-identical to as-built.
    pub(super) fn emit_materialization_claimed(&mut self, drv_hash: &DrvHash) {
        let Some(state) = self.dag.node(drv_hash) else {
            return;
        };
        let drv_path = state.drv_path().to_string();
        // The same payload the walk-spawn site sends: the paths the
        // store will fetch. `output_paths` is set by completion only;
        // pre-completion the expected paths are the fetch targets.
        let output_paths = state.expected_output_paths.clone();
        let event = rio_proto::types::build_event::Event::Derivation(
            rio_proto::types::DerivationEvent::substituting(drv_path, output_paths),
        );
        for build_id in self.get_interested_builds(drv_hash) {
            self.events.emit(build_id, event.clone());
            // build_summary counts the now-Assigned/Running node as
            // running — emit a progress snapshot so the queued/running
            // flip is visible (matches the walk site and
            // `emit_assignment_started`).
            self.emit_progress(build_id);
        }
    }

    /// Whether the node carries an unresolved, UNCLAIMED materialization
    /// job — the §2.6 "substitution backlog" predicate read by the
    /// snapshot bucket re-sourcing and the spawn-intent filter.
    ///
    /// Pending AND parked jobs both count: the consumers' question is
    /// "does store-side substitution work exist for this node", and a
    /// parked job is exactly that (work the store will resume once its
    /// backoff expires). Claimed jobs do NOT count — their nodes are
    /// Assigned/Running and surface through `running_derivations`.
    ///
    /// Reads only the in-memory view, which is populated exclusively by
    /// the flag-gated creation paths — flag-off it is permanently empty,
    /// so this is always false there. Callers still gate on
    /// `materialization_cfg.enabled` (defense in depth: criterion 2 /
    /// stop condition 8 — flag-off snapshot values must be byte-identical
    /// to baseline regardless of view contents).
    pub(super) fn has_pending_unclaimed_job(&self, drv_hash: &str) -> bool {
        self.materialization_jobs
            .get(drv_hash)
            .is_some_and(|entry| entry.claimed_by.is_none())
    }
}

// ──────────────────────────────────────────────────────────────────────
// The consumption transaction (design §2.4): Success coverage + the
// four-arm Unobtainable routing. The routing core is PURE (no IO, no
// clocks — kani-liftable per design §9.4); the consumption handler
// wires it to the fenced db operations.
// ──────────────────────────────────────────────────────────────────────

/// What the Unobtainable routing decided (design §2.4's four arms).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnobtainableRouting {
    /// Arm 0, covered: consume as success-for-live-interest.
    CompleteForLiveInterest,
    /// Arms 0 (uncovered) / 3a: job returns to pending.
    ReArm,
    /// Arms 1/2: node becomes from-source dispatchable.
    ResolveFromSource,
    /// Arm 3b: fail-fast every live DAG-interested build.
    FailFast,
}

/// The durable declared-relation classification (computed by the caller
/// from the dependency relation + statuses; the routing core never
/// touches the DAG).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DurableEvidence {
    /// Children all produced, no closure hole: from-source is viable.
    Vouched,
    /// Children exist but not all produced yet: normal dep gating.
    Pending,
    /// Absent/childless/holed: from-source is doomed.
    Broken,
}

/// The same-transaction FMP re-probe answer over the live wanted paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReprobeAnswer {
    /// Every live-wanted path present, substitutable, or indeterminate.
    Obtainable,
    /// Some live-wanted path confirmed missing-and-unsubstitutable.
    ConfirmedMissing,
}

/// The inputs of one Unobtainable routing decision.
pub(crate) struct RoutingInputs<'a> {
    /// Paths the executor confirmed absent upstream.
    pub missing_paths: &'a [String],
    /// Paths the executor verified present (and pinned).
    pub verified_paths: &'a [String],
    /// The live effective wanted PATHS (the §6 join, resolved to store
    /// paths by the caller inside the consumption transaction).
    pub live_wanted_paths: &'a [String],
    pub durable_evidence: DurableEvidence,
    /// Prior materialization_unobtainable rows for THIS job (the
    /// re-probe one-shot; design §2.4 arm 3).
    pub prior_unobtainable_count: u32,
    /// The same-transaction FMP re-probe answer over live_wanted_paths.
    /// `None` = not fetched (arms 0–2 decided without it); the caller
    /// fetches it only when arms 0–2 do not apply (purity by
    /// parameterization — design §9.4).
    pub reprobe: Option<ReprobeAnswer>,
}

// r[impl sched.materialize.routing]
/// The four-arm routing core. PURE (no IO, no clocks) — kani-liftable
/// per design §9.4; the FMP re-probe answer is an input.
///
/// The probe-failure case: the consumption HANDLER maps "the re-probe
/// RPC itself failed/timed out" to ReArm before calling this core (B3:
/// an indeterminate answer never fail-fasts) — the core's `None`
/// reprobe arm is therefore only reachable as one-shot-spent.
pub(crate) fn route_unobtainable(inputs: &RoutingInputs<'_>) -> UnobtainableRouting {
    let missing_live: Vec<&String> = inputs
        .missing_paths
        .iter()
        .filter(|p| inputs.live_wanted_paths.contains(p))
        .collect();
    // Arm 0 — moot-failure (the C3 arm).
    if missing_live.is_empty() {
        let covered = inputs
            .live_wanted_paths
            .iter()
            .all(|w| inputs.verified_paths.contains(w));
        return if covered {
            UnobtainableRouting::CompleteForLiveInterest
        } else {
            UnobtainableRouting::ReArm
        };
    }
    // Arms 1/2 — durable Vouched / Pending: from-source.
    match inputs.durable_evidence {
        DurableEvidence::Vouched | DurableEvidence::Pending => {
            return UnobtainableRouting::ResolveFromSource;
        }
        DurableEvidence::Broken => {}
    }
    // Arm 3 — Broken + live-wanted missing: the re-probe gate.
    match inputs.reprobe {
        Some(ReprobeAnswer::Obtainable) if inputs.prior_unobtainable_count == 0 => {
            UnobtainableRouting::ReArm
        }
        // Re-probe confirms missing, or the one-shot is spent. (A
        // missing probe is mapped to ReArm by the caller before this
        // core runs — see the doc above.)
        _ => UnobtainableRouting::FailFast,
    }
}

/// Success-consumption coverage check (the CE-17 closer): the live
/// wanted set is covered by what the execution ingested or verified.
// r[impl sched.materialize.routing]
pub(crate) fn success_covers_live_wanted(
    ingested: &[String],
    verified: &[String],
    live_wanted: &[String],
) -> bool {
    live_wanted
        .iter()
        .all(|w| ingested.contains(w) || verified.contains(w))
}

impl DagActor {
    // r[impl sched.materialize.routing]
    /// Consume one materialization outcome (the §2.4 consumption
    /// transaction). Reachable only flag-on in practice (no
    /// materialization attempt can exist otherwise) — but ALWAYS wired
    /// (design §4 "always-on regardless of flags": reports for existing
    /// attempts must drain after an ON→OFF flip).
    pub(super) async fn consume_materialization_outcome(
        &mut self,
        exec_id: Uuid,
        attempt: &crate::db::open_attempts::AttemptByExecRow,
        outcome: rio_proto::types::MaterializationOutcome,
    ) -> Result<(), super::pull::PullRejection> {
        use rio_proto::types::materialization_outcome::Outcome;
        let drv_hash = DrvHash::from(attempt.drv_hash.as_str());
        let serving_generation = self.serving_generation();
        let executor = ExecutorId::from(attempt.executor_id.as_str());

        // The unresolved job this attempt executes (PG is the
        // authority; the in-memory view is a cache).
        let job_id = self
            .db
            .unresolved_job_for_derivation(attempt.derivation_id)
            .await
            .map_err(|e| {
                super::pull::PullRejection::Internal(format!("materialization job lookup: {e}"))
            })?;

        // 1. The live effective wanted set (the §6 join), resolved to
        //    store paths — the presence-re-check half of D7's closure.
        let live_wanted_paths = self
            .live_wanted_paths_for(attempt.derivation_id, &drv_hash)
            .await
            .map_err(|e| super::pull::PullRejection::Internal(format!("wanted-union read: {e}")))?;

        match outcome.outcome {
            Some(Outcome::Success(s)) => {
                // Success appends NOTHING to the ledger (design §2.4 —
                // success is not a fold event). Coverage decides
                // Complete vs ReArm (the CE-17 class).
                self.close_materialization_attempt(exec_id, &drv_hash, None, serving_generation)
                    .await;
                if success_covers_live_wanted(
                    &s.ingested_paths,
                    &s.verified_paths,
                    &live_wanted_paths,
                ) {
                    if let Some(job_id) = job_id {
                        self.resolve_materialization_job(
                            job_id,
                            Some(exec_id),
                            crate::state::JobState::ResolvedSuccess,
                            serving_generation,
                        )
                        .await;
                    }
                    self.materialization_jobs.remove(&drv_hash);
                    // The build-success path: outputs are present and
                    // verified in the store; complete the node for live
                    // interest through the same chokepoint the
                    // dispatch-time store short-circuit uses.
                    self.complete_ready_from_store_batch(std::slice::from_ref(&drv_hash))
                        .await;
                } else {
                    // Interest grew between execution and consumption:
                    // the job stays pending; the next claim covers it.
                    self.rearm_materialization_job(&drv_hash, &executor).await;
                }
                Ok(())
            }
            Some(Outcome::Unobtainable(u)) => {
                // The charge row: kind=materialization — visible only to
                // the materialization budget, never to build budgets.
                let mut row = crate::db::attempts::AttemptRow::new(
                    attempt.derivation_id,
                    crate::state::OutcomeClass::MaterializationUnobtainable,
                    crate::state::ReportingParty::Worker,
                );
                row.exec_id = Some(exec_id);
                row.executor_id = Some(executor.clone());
                row.attempt_kind = crate::state::AttemptKind::Materialization;
                row.error_msg = (!u.cause.is_empty()).then(|| u.cause.clone());
                let prior_unobtainable = self.count_materialization_rows_in_history(
                    &drv_hash,
                    crate::state::OutcomeClass::MaterializationUnobtainable,
                );
                self.close_materialization_attempt(
                    exec_id,
                    &drv_hash,
                    Some(row),
                    serving_generation,
                )
                .await;

                // 2. The four-arm routing. Arms 0–2 decide without the
                //    re-probe; the probe is fetched only for arm 3.
                let durable_evidence = match self.dag.closure_evidence(drv_hash.as_str()) {
                    rio_evidence_kernel::ClosureEvidence::Vouched => DurableEvidence::Vouched,
                    rio_evidence_kernel::ClosureEvidence::Pending => DurableEvidence::Pending,
                    rio_evidence_kernel::ClosureEvidence::Broken => DurableEvidence::Broken,
                };
                let needs_probe = u
                    .missing_paths
                    .iter()
                    .any(|p| live_wanted_paths.contains(p))
                    && durable_evidence == DurableEvidence::Broken;
                let reprobe = if needs_probe {
                    match self
                        .reprobe_live_wanted_paths(&drv_hash, &live_wanted_paths)
                        .await
                    {
                        Some(answer) => Some(answer),
                        None => {
                            // B3: the re-probe RPC itself failed — an
                            // indeterminate answer never fail-fasts.
                            self.rearm_materialization_job(&drv_hash, &executor).await;
                            return Ok(());
                        }
                    }
                } else {
                    None
                };
                let routing = route_unobtainable(&RoutingInputs {
                    missing_paths: &u.missing_paths,
                    verified_paths: &u.verified_paths,
                    live_wanted_paths: &live_wanted_paths,
                    durable_evidence,
                    prior_unobtainable_count: prior_unobtainable,
                    reprobe,
                });
                // 3. Execute the routing.
                match routing {
                    UnobtainableRouting::CompleteForLiveInterest => {
                        if let Some(job_id) = job_id {
                            self.resolve_materialization_job(
                                job_id,
                                Some(exec_id),
                                crate::state::JobState::ResolvedSuccess,
                                serving_generation,
                            )
                            .await;
                        }
                        self.materialization_jobs.remove(&drv_hash);
                        self.complete_ready_from_store_batch(std::slice::from_ref(&drv_hash))
                            .await;
                    }
                    UnobtainableRouting::ReArm => {
                        self.rearm_materialization_job(&drv_hash, &executor).await;
                    }
                    UnobtainableRouting::ResolveFromSource => {
                        if let Some(job_id) = job_id {
                            self.resolve_materialization_job(
                                job_id,
                                Some(exec_id),
                                crate::state::JobState::ResolvedFromSource,
                                serving_generation,
                            )
                            .await;
                        }
                        self.materialization_jobs.remove(&drv_hash);
                        // The node returns to its dep-derived status
                        // (the normal Ready path) — requeue it.
                        self.reassign_derivations(std::slice::from_ref(&drv_hash), Some(&executor))
                            .await;
                    }
                    UnobtainableRouting::FailFast => {
                        if let Some(job_id) = job_id {
                            self.resolve_materialization_job(
                                job_id,
                                Some(exec_id),
                                crate::state::JobState::ResolvedUnobtainable,
                                serving_generation,
                            )
                            .await;
                        }
                        self.materialization_jobs.remove(&drv_hash);
                        self.fail_fast_topdown_pruned_root(
                            &drv_hash,
                            "materialization confirmed a live-wanted output missing upstream \
                             and not substitutable",
                        )
                        .await;
                    }
                }
                Ok(())
            }
            Some(Outcome::InfraFailure(f)) => {
                // The infra charge: counts toward the materialization
                // budget and toward NOTHING else. Never fail-fasts,
                // never routes from source (B3).
                let mut row = crate::db::attempts::AttemptRow::new(
                    attempt.derivation_id,
                    crate::state::OutcomeClass::MaterializationInfra,
                    crate::state::ReportingParty::Worker,
                );
                row.exec_id = Some(exec_id);
                row.executor_id = Some(executor.clone());
                row.attempt_kind = crate::state::AttemptKind::Materialization;
                row.error_msg = (!f.detail.is_empty()).then(|| f.detail.clone());
                self.close_materialization_attempt(
                    exec_id,
                    &drv_hash,
                    Some(row),
                    serving_generation,
                )
                .await;
                // Budget: at max_attempts the job parks (durable
                // park_until + the view), else it re-arms claimable.
                let infra_count = self.count_materialization_rows_in_history(
                    &drv_hash,
                    crate::state::OutcomeClass::MaterializationInfra,
                );
                if infra_count >= self.materialization_cfg.max_attempts {
                    self.park_materialization_job(
                        &drv_hash,
                        job_id,
                        infra_count,
                        serving_generation,
                    )
                    .await;
                } else {
                    self.rearm_materialization_job(&drv_hash, &executor).await;
                }
                // Either way the node itself returns to the queue
                // (claimable again / from-source dispatchable per the
                // admission table).
                self.reassign_derivations(std::slice::from_ref(&drv_hash), Some(&executor))
                    .await;
                Ok(())
            }
            None => {
                warn!(%exec_id, "materialization outcome with no payload; acknowledged-and-ignored");
                Ok(())
            }
        }
    }

    /// The live effective wanted PATHS for a node: the §6 wanted-union
    /// (joined over live builds' contributions), resolved to store
    /// paths against the node's declared outputs. Falls back to the
    /// node-level union when no live contribution exists.
    async fn live_wanted_paths_for(
        &self,
        derivation_id: Uuid,
        drv_hash: &DrvHash,
    ) -> Result<Vec<String>, sqlx::Error> {
        let union = self.db.effective_wanted_union(derivation_id).await?;
        let Some(state) = self.dag.node(drv_hash) else {
            return Ok(Vec::new());
        };
        let wanted_names: Vec<String> = match union {
            // No live contribution: fall back to the node-level union.
            None => state.wanted_output_names.clone(),
            // '{}' saturation = all declared outputs.
            Some(v) if v.is_empty() => Vec::new(),
            Some(v) => v,
        };
        let paths: Vec<String> = if wanted_names.is_empty() {
            state.expected_output_paths.clone()
        } else {
            state
                .output_names
                .iter()
                .zip(state.expected_output_paths.iter())
                .filter(|(name, _)| wanted_names.iter().any(|w| w == *name))
                .map(|(_, path)| path.clone())
                .collect()
        };
        Ok(paths.into_iter().filter(|p| !p.is_empty()).collect())
    }

    /// Count this node's in-memory ledger rows of one materialization
    /// outcome class (the budget/one-shot inputs). The in-memory
    /// history mirrors committed rows (read-through cache).
    fn count_materialization_rows_in_history(
        &self,
        drv_hash: &DrvHash,
        class: crate::state::OutcomeClass,
    ) -> u32 {
        self.dag
            .node(drv_hash)
            .map(|s| {
                s.attempt_history()
                    .iter()
                    .filter(|r| {
                        r.attempt_kind == crate::state::AttemptKind::Materialization
                            && r.outcome_class == class
                    })
                    .count() as u32
            })
            .unwrap_or(0)
    }

    /// Close the open materialization attempt (assignment row) and
    /// append the charge row when one is given, in ONE transaction
    /// carrying the same claims-floor fence as every other attempt
    /// closer. Mirrors `close_pull_attempt_uncharged`'s shape WITH an
    /// optional charge. Idempotent: a row already present for the exec
    /// makes the append a no-op (terminal-row-wins).
    async fn close_materialization_attempt(
        &mut self,
        exec_id: Uuid,
        drv_hash: &DrvHash,
        charge_row: Option<crate::db::attempts::AttemptRow>,
        serving_generation: i64,
    ) {
        let result: Result<Option<bool>, sqlx::Error> = async {
            let mut tx = self.db.pool().begin().await?;
            let floor = crate::db::SchedulerDb::claims_floor(&mut tx).await?;
            if !crate::db::SchedulerDb::at_or_above_floor(floor, serving_generation) {
                tx.rollback().await?;
                return Ok(None);
            }
            let mut inserted = false;
            if let Some(row) = &charge_row {
                inserted = crate::db::SchedulerDb::append_attempt(&mut tx, row).await?;
            }
            sqlx::query(
                "UPDATE assignments SET status = 'completed', completed_at = now() \
                 WHERE exec_id = $1 AND status IN ('pending', 'acknowledged')",
            )
            .bind(exec_id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok(Some(inserted))
        }
        .await;
        match result {
            Ok(Some(inserted)) => {
                if inserted && let Some(row) = charge_row {
                    if let Some(state) = self.dag.node_mut(drv_hash) {
                        state.push_attempt_record(row.to_record());
                    }
                    self.refresh_retry_view(drv_hash);
                }
            }
            Ok(None) => {
                self.note_fenced_evidence_write("materialization attempt close");
            }
            Err(e) => {
                warn!(drv_hash = %drv_hash, %exec_id, error = %e,
                      "materialization attempt close failed; the establishment sweep remains \
                       the backstop");
            }
        }
    }

    /// Resolve the job terminally (fenced, exec_id-keyed, at-most-once)
    /// and note a fence refusal.
    async fn resolve_materialization_job(
        &mut self,
        job_id: Uuid,
        exec_id: Option<Uuid>,
        to_state: crate::state::JobState,
        serving_generation: i64,
    ) {
        match self
            .db
            .resolve_materialization_job_fenced(job_id, exec_id, to_state, serving_generation)
            .await
        {
            Ok(crate::db::FencedWrite::Applied(_)) => {}
            Ok(crate::db::FencedWrite::Fenced) => {
                self.note_fenced_evidence_write("materialization job resolve");
            }
            Err(e) => {
                warn!(%job_id, error = %e, "materialization job resolve failed");
            }
        }
    }

    /// Re-arm the job: it stays pending (claimable); the in-memory view
    /// drops the claim so the next pull's one-winner arbitration sees
    /// Pending again.
    async fn rearm_materialization_job(&mut self, drv_hash: &DrvHash, _executor: &ExecutorId) {
        if let Some(entry) = self.materialization_jobs.get_mut(drv_hash) {
            entry.claimed_by = None;
        }
    }

    /// Park the job (infra-budget exhaustion, design §2.5): durable
    /// `park_until` + the in-memory view. Never a fail-fast.
    async fn park_materialization_job(
        &mut self,
        drv_hash: &DrvHash,
        job_id: Option<Uuid>,
        infra_count: u32,
        serving_generation: i64,
    ) {
        let base = self.materialization_cfg.park_backoff_base_secs;
        let cap = self.materialization_cfg.park_backoff_cap_secs;
        let exp = infra_count.saturating_sub(self.materialization_cfg.max_attempts);
        let backoff_secs = base.saturating_mul(2u64.saturating_pow(exp)).min(cap);
        if let Some(job_id) = job_id {
            let park_until_epoch = crate::db::attempts::epoch_now() + backoff_secs as f64;
            match self
                .db
                .park_materialization_job_fenced(job_id, park_until_epoch, serving_generation)
                .await
            {
                Ok(crate::db::FencedWrite::Applied(_)) => {}
                Ok(crate::db::FencedWrite::Fenced) => {
                    self.note_fenced_evidence_write("materialization job park");
                }
                Err(e) => warn!(%job_id, error = %e, "materialization job park failed"),
            }
        }
        if let Some(entry) = self.materialization_jobs.get_mut(drv_hash) {
            entry.claimed_by = None;
            entry.parked_until =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(backoff_secs));
        }
    }

    /// The arm-3 FMP re-probe over the live wanted paths. `None` = the
    /// probe could not answer (no store client / RPC failure / timeout)
    /// — the caller maps that to ReArm (B3).
    ///
    /// Without a service signer the store cannot run its upstream
    /// substitution check (no `x-rio-probe-tenant-id`), so a missing
    /// path is indeterminate, never confirmed-missing — the probe then
    /// cannot produce the fail-fast conjunct (B3's conservative
    /// direction).
    async fn reprobe_live_wanted_paths(
        &mut self,
        drv_hash: &DrvHash,
        live_wanted: &[String],
    ) -> Option<ReprobeAnswer> {
        if live_wanted.is_empty() {
            return Some(ReprobeAnswer::Obtainable);
        }
        let store = self.store_client.clone()?;
        let tenant = self
            .dag
            .node(drv_hash)
            .into_iter()
            .flat_map(|s| s.interested_builds.iter())
            .filter_map(|bid| self.builds.get(bid))
            .find_map(|b| b.tenant_id);
        let auth = self.substitute_auth_for_tenant(tenant);
        let can_confirm = matches!(auth, super::dispatch::SubstituteAuth::Service { .. });
        let mut req = tonic::Request::new(rio_proto::types::FindMissingPathsRequest {
            store_paths: live_wanted.to_vec(),
        });
        for (k, v) in auth.mint() {
            if let Ok(mv) = tonic::metadata::MetadataValue::try_from(v.as_str()) {
                req.metadata_mut().insert(k, mv);
            }
        }
        let resp = tokio::time::timeout(self.grpc_timeout, store.clone().find_missing_paths(req))
            .await
            .ok()?
            .ok()?
            .into_inner();
        let missing: std::collections::HashSet<String> = resp.missing_paths.into_iter().collect();
        let substitutable: std::collections::HashSet<String> =
            resp.substitutable_paths.into_iter().collect();
        let indeterminate: std::collections::HashSet<String> =
            resp.indeterminate_paths.into_iter().collect();
        let obtainable = live_wanted.iter().all(|p| {
            !missing.contains(p) || substitutable.contains(p) || indeterminate.contains(p)
        });
        Some(if obtainable || !can_confirm {
            ReprobeAnswer::Obtainable
        } else {
            ReprobeAnswer::ConfirmedMissing
        })
    }
}

impl DagActor {
    /// Establish one expired open materialization attempt (the
    /// dead-store-replica case): append the `materialization_infra`
    /// charge (kind=materialization, party=Scheduler, "unreported"),
    /// close the assignment row in the same fenced transaction, clear
    /// the in-memory claim, and leave the job pending (claimable
    /// again). NO adopt arm (BC-3: a mid-walk crash leaves outputs
    /// present but the closure incomplete) and never `executor_crash`
    /// (BC-2: the charge feeds the materialization budget and nothing
    /// else). Mirrors `close_pull_attempt_uncharged`'s transaction
    /// shape WITH the charge row.
    // r[impl sched.materialize.routing]
    pub(super) async fn establish_materialization_attempt(
        &mut self,
        attempt: &crate::db::open_attempts::OpenAttemptRow,
    ) {
        let drv_hash = DrvHash::from(attempt.drv_hash.as_str());
        let executor = ExecutorId::from(attempt.executor_id.as_str());
        let serving_generation = self.serving_generation();
        let mut row = crate::db::attempts::AttemptRow::new(
            attempt.derivation_id,
            crate::state::OutcomeClass::MaterializationInfra,
            crate::state::ReportingParty::Scheduler,
        );
        row.exec_id = Some(attempt.exec_id);
        row.executor_id = Some(executor.clone());
        row.attempt_kind = crate::state::AttemptKind::Materialization;
        row.source_node = attempt.source_node.clone();
        row.termination_reason = Some("unreported".into());
        self.close_materialization_attempt(
            attempt.exec_id,
            &drv_hash,
            Some(row),
            serving_generation,
        )
        .await;
        // The claim is gone; the job stays pending (claimable again).
        self.rearm_materialization_job(&drv_hash, &executor).await;
        // The node returns to its dispatchable status.
        self.reassign_derivations(std::slice::from_ref(&drv_hash), Some(&executor))
            .await;
        tracing::info!(
            drv_hash = %drv_hash,
            exec_id = %attempt.exec_id,
            executor_id = %executor,
            age_secs = attempt.age_secs,
            "establishment sweep: open materialization attempt established as \
             materialization_infra (no adopt arm; the job stays pending)"
        );
    }

    /// The flag-gated housekeeping backstop: cancel jobs for
    /// derivations whose live interest dropped to zero (node gone,
    /// node terminal, or every interested build terminal), closing any
    /// open materialization attempt charge-free. Phase B's
    /// build-terminal hooks will call the closer directly; in Phase A
    /// this tick backstop and the tests are the only callers.
    pub(super) async fn tick_cancel_zero_interest_materialization(&mut self) {
        use crate::state::BuildStateExt;
        let zero_interest: Vec<DrvHash> = self
            .materialization_jobs
            .keys()
            .filter(|h| match self.dag.node(h.as_str()) {
                None => true,
                Some(state) => {
                    state.status().is_terminal()
                        || !state.interested_builds.iter().any(|bid| {
                            self.builds
                                .get(bid)
                                .is_some_and(|b| !b.state().is_terminal())
                        })
                }
            })
            .cloned()
            .collect();
        for drv_hash in zero_interest {
            self.cancel_materialization_for_zero_interest(&drv_hash)
                .await;
        }
    }

    // r[impl sched.materialize.job]
    /// Cancel the job for a derivation whose live interest dropped to
    /// zero, closing any open materialization attempt CHARGE-FREE (no
    /// drv_attempts row at all) — BC-2's no-controller closer. The job
    /// resolution is fenced and pending-only (terminal-row-wins).
    pub(super) async fn cancel_materialization_for_zero_interest(&mut self, drv_hash: &DrvHash) {
        let Some(entry) = self.materialization_jobs.get(drv_hash) else {
            return;
        };
        let job_id = entry.job_id;
        let serving_generation = self.serving_generation();
        // Close any open attempt charge-free (no charge row — a
        // cancellation is not a failure; the budget is untouched).
        if let Some(exec_id) = self.dag.node(drv_hash).and_then(|s| s.exec_id) {
            self.close_materialization_attempt(exec_id, drv_hash, None, serving_generation)
                .await;
        }
        // Cancel the job (fenced, pending-only).
        self.resolve_materialization_job(
            job_id,
            None,
            crate::state::JobState::Cancelled,
            serving_generation,
        )
        .await;
        self.materialization_jobs.remove(drv_hash);
        tracing::info!(
            drv_hash = %drv_hash,
            %job_id,
            "materialization job cancelled: no live interested build remains"
        );
    }
}
