//! Pull-mode dispatch: the `PullAssignment` admission kernel and the
//! actor-side pull transaction.
//!
//! A pull-mode pod is born knowing its derivation (the HMAC-attested
//! `intent_id`); its first and only ask is `PullAssignment`, which
//! either delivers the existing `WorkAssignment` payload, says `Gone`
//! (no longer wanted, exit 0 charge-free), or says `NotYetReady`
//! (wanted but not currently deliverable to *this* pod — deps unbuilt,
//! or the drv is open on another executor). The decision is computed by
//! the pure [`admit_pull`] kernel from already-loaded state so a
//! Phase-2 Kani harness needs no refactor; the actor handler executes
//! the decision, and the durable mint runs as one generation-fenced
//! transaction (`SchedulerDb::mint_pull_attempt_fenced`).
//!
//! The attempt's executor identity is the attested intent id itself:
//! the pull request carries no pod name (the token is signed before the
//! controller picks one), so the binding key is the identity the token
//! attests. Pod/node attribution is carried separately by
//! `source_node` (the controller-authoritative binding) and by the
//! controller's `ReportAttemptOutcome`. A re-pull by the pod (or a
//! replacement pod of the same intent) therefore converges on the same
//! open attempt instead of minting a second one, and an attempt held by
//! a *different* identity is never re-delivered or re-pointed.

use tokio::sync::oneshot;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::state::{DerivationStatus, DrvHash, ExecutorId};

use super::DagActor;

/// Server-suggested re-pull delay carried by `NotYetReady` (decision
/// P4: 5 s; the pod adds jitter). The pod-side idle bound reuses the
/// existing builder `idle_timeout`, so no second number exists here.
pub(crate) const NOT_YET_READY_RETRY_AFTER_SECS: u32 = 5;

/// What the scheduler answers a `PullAssignment` with.
#[derive(Debug)]
pub enum PullOutcome {
    /// Deps Ready and the open attempt is bound to the pulling
    /// identity: the dispatch payload (identical on every re-pull
    /// while the attempt stays open).
    Deliver(Box<rio_proto::types::WorkAssignment>),
    /// The derivation is no longer wanted; the pod exits 0 charge-free.
    Gone,
    /// Wanted but not currently deliverable to this pod; re-pull after
    /// the suggested delay.
    NotYetReady { retry_after_secs: u32 },
}

/// Why a `PullAssignment` was refused without an outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullRejection {
    /// The serving replica is not the leader (or lost the lease while
    /// handling the pull). Retryable — same class as `ensure_leader`.
    NotLeader,
    /// The serving generation is below the durable claims floor (the
    /// transaction-side fence). Retryable not-leader class.
    StaleGeneration,
    /// The HMAC-attested intent does not match the requested intent.
    TokenMismatch,
    /// Database failure while admitting or minting.
    Internal(String),
}

impl PullRejection {
    // r[impl sched.grpc.fence-retryable]
    /// The refusal class (the same law as [`ActorError::retry_class`]
    /// — `Retryable ⟺ code ∈ {UNAVAILABLE, RESOURCE_EXHAUSTED}`,
    /// pinned by `retry_class_code_consistency`).
    ///
    /// [`ActorError::retry_class`]: crate::actor::ActorError::retry_class
    pub(crate) fn retry_class(&self) -> super::command::RetryClass {
        use super::command::RetryClass;
        match self {
            // Leadership-class refusals: valid pulls another replica
            // serves.
            Self::NotLeader | Self::StaleGeneration => RetryClass::Retryable,
            // Mis-bound token / internal failure: unservable as posed.
            Self::TokenMismatch | Self::Internal(_) => RetryClass::Terminal,
        }
    }
}

/// The pure admission decision for one pull: the CBMC-verified
/// [`rio_evidence_kernel::pull::PullAdmission`] alphabet, instantiated
/// with the scheduler's exec-id type. The decision logic itself lives
/// in the kernel ([`rio_evidence_kernel::pull::admit_pull`]); the
/// scheduler's [`admit_pull`] is the projection shim over it.
pub(crate) type PullDecision = rio_evidence_kernel::pull::PullAdmission<Uuid>;

/// Everything [`admit_pull`] needs, already loaded by the caller.
pub(crate) struct PullInputs<'a> {
    /// The request's intent id (== drv hash, the DAG key).
    pub intent_id: &'a str,
    /// The HMAC-attested intent binding (`None` = dev mode, no key).
    pub auth_intent: Option<&'a str>,
    /// The derivation's current status; `None` if the DAG has no node.
    pub status: Option<DerivationStatus>,
    /// The open attempt bound to the derivation, if any:
    /// (executor identity, exec_id).
    pub open_attempt: Option<(&'a ExecutorId, Uuid)>,
    /// The identity this pull would bind a fresh attempt to.
    pub pulling_identity: &'a ExecutorId,
    /// The serving replica's lease generation.
    pub serving_generation: u64,
    /// The durable claims floor (`None` = fresh cluster, no rows).
    pub generation_floor: Option<i64>,
    /// The pull's claimed work class.
    pub pull_kind: rio_evidence_kernel::pull::PullKind,
    /// The node's materialization-job state, projected from the actor's
    /// in-memory job view.
    pub job_view: rio_evidence_kernel::pull::JobView,
}

// r[impl sched.executor.pull-gone]
// r[impl sched.executor.pull-not-ready+2]
/// Decide one pull from already-loaded state. Projection shim over the
/// CBMC-verified [`rio_evidence_kernel::pull::admit_pull`] (decision
/// P10 — the decision was kept pure from its introduction precisely so
/// it could be lifted into a kani kernel without refactoring; the
/// closure-evidence campaign's Phase 2 did the lift): this function
/// maps the scheduler vocabulary (`DerivationStatus`, `ExecutorId`,
/// `Uuid`) onto the kernel's mirrored alphabet and returns the kernel's
/// decision unchanged.
///
/// Check order (proven in the kernel): identity first (a mis-bound
/// token never learns anything about the drv), then the generation
/// fence (a deposed believer answers nothing), then
/// wantedness/deliverability — including the advisory generation-fence
/// half (`r[sched.lease.generation-fence+3]`) at the kernel's marked
/// arms.
pub(crate) fn admit_pull(inputs: &PullInputs<'_>) -> PullDecision {
    rio_evidence_kernel::pull::admit_pull(
        rio_evidence_kernel::pull::PullRequest {
            intent_id: inputs.intent_id,
            auth_intent: inputs.auth_intent,
            serving_generation: inputs.serving_generation,
            generation_floor: inputs.generation_floor,
            status: inputs.status.map(pull_node_status),
            open_attempt: inputs.open_attempt,
            pulling_identity: inputs.pulling_identity,
        },
        rio_evidence_kernel::pull::MaterializationInputs {
            kind: inputs.pull_kind,
            job: inputs.job_view,
        },
    )
}

/// `DerivationStatus` → kernel [`PullNodeStatus`]. The exhaustive
/// `match` (no wildcard arm) pins the alphabets in lockstep: adding a
/// scheduler variant the kernel lacks fails this compile.
///
/// [`PullNodeStatus`]: rio_evidence_kernel::pull::PullNodeStatus
fn pull_node_status(status: DerivationStatus) -> rio_evidence_kernel::pull::PullNodeStatus {
    use rio_evidence_kernel::pull::PullNodeStatus as K;
    match status {
        DerivationStatus::Created => K::Created,
        DerivationStatus::Queued => K::Queued,
        DerivationStatus::Ready => K::Ready,
        DerivationStatus::Assigned => K::Assigned,
        DerivationStatus::Running => K::Running,
        DerivationStatus::Completed => K::Completed,
        DerivationStatus::Failed => K::Failed,
        DerivationStatus::Poisoned => K::Poisoned,
        DerivationStatus::DependencyFailed => K::DependencyFailed,
        DerivationStatus::Cancelled => K::Cancelled,
        DerivationStatus::Skipped => K::Skipped,
    }
}

impl DagActor {
    /// Handle one `PullAssignment` (the actor turn). Computes the
    /// admission via [`admit_pull`], executes the decision, and replies.
    pub(super) async fn handle_pull_assignment(
        &mut self,
        intent_id: String,
        auth_intent: Option<String>,
        kind: rio_evidence_kernel::pull::PullKind,
        executor_instance: Option<String>,
        reply: oneshot::Sender<Result<PullOutcome, PullRejection>>,
    ) {
        let result = self
            .pull_assignment_inner(
                &intent_id,
                auth_intent.as_deref(),
                kind,
                executor_instance.as_deref(),
            )
            .await;
        let _ = reply.send(result);
    }

    // r[impl sched.executor.pull-transaction+2]
    async fn pull_assignment_inner(
        &mut self,
        intent_id: &str,
        auth_intent: Option<&str>,
        kind: rio_evidence_kernel::pull::PullKind,
        executor_instance: Option<&str>,
    ) -> Result<PullOutcome, PullRejection> {
        // Standby replicas answer nothing (the gRPC layer already
        // gates; this closes the in-flight-deposed window).
        if !self.leader.is_leader() {
            return Err(PullRejection::NotLeader);
        }
        let serving_generation = self.leader.generation();
        let generation_floor = self
            .db
            .max_known_generation()
            .await
            .map_err(|e| PullRejection::Internal(format!("claims-floor read failed: {e}")))?;

        let drv_hash = DrvHash::from(intent_id);
        // The attempt's executor identity (substitution-replacement
        // BC-1): build pulls bind to the attested intent itself,
        // exactly as-built (the request carries no pod name the token
        // could attest). Materialization pulls bind to the
        // `(intent, replica)` pair — distinct per store replica, so the
        // kernel's open-attempt arm (same-identity re-delivery,
        // different-identity NotYetReady) is the one-winner arbiter.
        // The kind is NEVER parsed back out of this string: it is
        // carried by the request and persisted as
        // `drv_executions.attempt_kind`.
        let pulling_identity = match (kind, executor_instance) {
            (rio_evidence_kernel::pull::PullKind::Materialization, Some(instance))
                if !instance.is_empty() =>
            {
                ExecutorId::from(format!("{intent_id}@{instance}"))
            }
            // Build pulls: the attested intent, exactly as-built.
            _ => ExecutorId::from(intent_id),
        };

        let (status, open_attempt) = match self.dag.node(&drv_hash) {
            None => (None, None),
            Some(state) => (
                Some(state.status()),
                state
                    .assigned_executor
                    .as_ref()
                    .zip(state.exec_id)
                    .map(|(executor, exec_id)| (executor.clone(), exec_id)),
            ),
        };
        let decision = admit_pull(&PullInputs {
            intent_id,
            auth_intent,
            status,
            open_attempt: open_attempt.as_ref().map(|(e, x)| (e, *x)),
            pulling_identity: &pulling_identity,
            serving_generation,
            generation_floor,
            pull_kind: kind,
            // The job view: projected from the actor's in-memory job map.
            job_view: self.materialization_job_view(&drv_hash, &pulling_identity),
        });

        match decision {
            PullDecision::RejectToken => {
                // r[impl sec.executor.identity-token+3]
                warn!(
                    intent_id = %intent_id,
                    "pull rejected: executor token bound to a different intent"
                );
                Err(PullRejection::TokenMismatch)
            }
            PullDecision::RejectStaleGeneration => {
                info!(
                    intent_id = %intent_id,
                    serving_generation,
                    ?generation_floor,
                    "pull rejected: serving generation below the durable claims floor"
                );
                Err(PullRejection::StaleGeneration)
            }
            PullDecision::Gone => {
                debug!(intent_id = %intent_id, ?status, "pull answered Gone");
                Ok(PullOutcome::Gone)
            }
            PullDecision::NotYetReady => {
                debug!(intent_id = %intent_id, ?status, "pull answered NotYetReady");
                Ok(PullOutcome::NotYetReady {
                    retry_after_secs: NOT_YET_READY_RETRY_AFTER_SECS,
                })
            }
            PullDecision::DeliverExisting { exec_id } => {
                // Idempotent re-pull: read, never write. The payload is
                // rebuilt from the same inputs (drv, identity), so the
                // load-bearing fields — drv path, ATerm, exec_id,
                // resources — are identical.
                debug_assert_eq!(
                    self.dag.node(&drv_hash).and_then(|s| s.exec_id),
                    Some(exec_id)
                );
                let assignment = self
                    .build_assignment_proto(&drv_hash, &pulling_identity)
                    .await
                    .ok_or_else(|| {
                        PullRejection::Internal("derivation vanished during re-pull".into())
                    })?;
                Ok(PullOutcome::Deliver(Box::new(assignment)))
            }
            PullDecision::DeliverNew => {
                self.mint_and_deliver(&drv_hash, &pulling_identity, serving_generation, kind)
                    .await
            }
        }
    }

    /// The one pull transaction: mint `exec_id`, write the fenced
    /// `assignments` + `drv_executions` rows, transition the
    /// derivation out of Ready, pin GC live-inputs, and build the
    /// payload. The durable half commits only at-or-above the claims
    /// floor; on a fence abort nothing is written and nothing in
    /// memory changes.
    async fn mint_and_deliver(
        &mut self,
        drv_hash: &DrvHash,
        pulling_identity: &ExecutorId,
        serving_generation: u64,
        kind: rio_evidence_kernel::pull::PullKind,
    ) -> Result<PullOutcome, PullRejection> {
        let Some(db_id) = self.dag.node(drv_hash).and_then(|s| s.db_id) else {
            // Merged but not yet persisted — deliverable on a later
            // pull once the merge commit lands.
            return Ok(PullOutcome::NotYetReady {
                retry_after_secs: NOT_YET_READY_RETRY_AFTER_SECS,
            });
        };
        let exec_id = Uuid::now_v7();
        let log_hash = self
            .dag
            .path_for_hash(drv_hash)
            .map(rio_nix::store_path::drv_log_hash)
            .unwrap_or_default();
        // Controller-authoritative pod→node binding, when known.
        let source_node = self
            .authoritative_binding
            .get(drv_hash)
            .map(|b| b.node.clone());
        // The deadline this attempt is dispatched under (the same solve
        // that sized the spawn intent / activeDeadlineSeconds),
        // persisted so the establishment window is anchored to it and
        // can never shrink below it while the attempt is open. The
        // solve is NOT stamped onto the node: with the stream dispatch
        // pass gone there is no dispatch-time intent writer, and the
        // pull-path conventions (recorded with the unit-corpus
        // re-point) treat the explicit per-test/operator seed as the
        // only source for `last_intent`-derived baselines.
        let deadline_secs = {
            let (hw, cost, inputs_gen) = self.solve_inputs();
            self.dag.node(drv_hash).map(|state| {
                f64::from(
                    self.solve_intent_for(state, &hw, &cost, inputs_gen)
                        .deadline_secs,
                )
            })
        };

        // Substitution-replacement: the work class is persisted on the
        // execution row (`drv_executions.attempt_kind`) — keyed on the
        // request's claimed kind, never derived from an identity prefix.
        // Build pulls write 'build' (the column default — value-identical
        // to the as-built rows).
        let attempt_kind = match kind {
            rio_evidence_kernel::pull::PullKind::Build => crate::state::AttemptKind::Build,
            rio_evidence_kernel::pull::PullKind::Materialization => {
                crate::state::AttemptKind::Materialization
            }
        };
        let minted = self
            .db
            .mint_pull_attempt_fenced(
                db_id,
                pulling_identity,
                serving_generation as i64,
                exec_id,
                &log_hash,
                source_node.as_deref(),
                deadline_secs,
                attempt_kind,
            )
            .await
            .map_err(|e| PullRejection::Internal(format!("pull mint transaction failed: {e}")))?;
        if !minted.settled() {
            info!(
                drv_hash = %drv_hash,
                serving_generation,
                "pull mint aborted by the generation fence; no row written"
            );
            return Err(PullRejection::StaleGeneration);
        }

        // Substitution-replacement: a committed materialization mint is
        // the claim — note it in the in-memory job view so the kernel's
        // one-winner arbitration (Claimed{held_by_puller}) answers
        // re-pulls and competing claims correctly. Reachable only
        // flag-on (no materialization pull is delivered flag-off).
        if attempt_kind == crate::state::AttemptKind::Materialization {
            self.note_materialization_claimed(drv_hash, pulling_identity);
        }

        // r[impl sched.sla.hw-class.ice-mask]
        // Mechanism #22's clear half, pull-mode trigger: the first
        // successful pull is the success edge for the pull path — the
        // pod is scheduled and has taken the work — exactly what the
        // stream path's registration edge signals. Same |A'| = 1
        // discipline as that edge (the pod's affinity is OR-of-A', so a
        // multi-cell intent identifies no single cell; over-clearing
        // would defeat `ice_step_doubles`); `registered_cells` (A18)
        // remains the per-cell signal. NotYetReady / Gone / rejected
        // pulls never reach here, so they never clear. Arming
        // (`AckSpawnedIntents`) and the DAG-state sweep are untouched.
        if let Some((_, cells)) = self.dispatched_cells.remove(drv_hash.as_str())
            && let [cell] = cells.as_slice()
        {
            self.ice.clear(cell);
        }

        // Durable mint committed — now the in-memory bookkeeping, the
        // same shape the stream path's record phase keeps (transition,
        // exec_id, assigned executor, status persist, GC pins).
        // r[impl sched.state.machine+2]
        // The transition uses the KINDED validation (PD-6): build mints
        // take the as-built Ready→Assigned edge byte-identically;
        // materialization mints may additionally take Queued→Assigned
        // (the kernel's Queued admission and this edge are two halves of
        // one decision — a rejection here for an admitted claim would
        // re-open the PDQ-6 stranded-mint window).
        if let Some(state) = self.dag.node_mut(drv_hash) {
            if let Err(e) = state.transition_for_mint(DerivationStatus::Assigned, attempt_kind) {
                // TOCTOU vs a concurrent cancel between the admit and
                // the commit: the durable rows exist but the node left
                // Ready/Queued. The attempt resolves via the normal
                // terminal paths (report or establishment); never deliver.
                warn!(drv_hash = %drv_hash, error = %e,
                      "pull minted but the mint transition was rejected; answering NotYetReady");
                return Ok(PullOutcome::NotYetReady {
                    retry_after_secs: NOT_YET_READY_RETRY_AFTER_SECS,
                });
            }
            state.retry.backoff_until = None;
            state.assigned_executor = Some(pulling_identity.clone());
            state.exec_id = Some(exec_id);
            // r[impl sched.pull.kinded-running-surface]
            // Captured at the single mint site for BOTH work classes,
            // cleared in lockstep with exec_id (bug_144).
            state.open_attempt_kind = Some(attempt_kind);
            if let Err(e) = state.transition(DerivationStatus::Running) {
                warn!(drv_hash = %drv_hash, error = %e,
                      "pull-minted attempt could not enter Running (left Assigned)");
            }
        }
        let new_status = self
            .dag
            .node(drv_hash)
            .map(|s| s.status())
            .unwrap_or(DerivationStatus::Running);
        self.persist_status(drv_hash, new_status, Some(pulling_identity))
            .await;

        // GC live-input pins — same best-effort discipline as the
        // stream path's record phase. Materialization mints skip this:
        // build-input pins are not materialization's lifecycle (design
        // §5 — pin-at-ingest is store-side, and a materialization
        // attempt never reads build inputs).
        if attempt_kind == crate::state::AttemptKind::Build {
            let input_paths = crate::assignment::approx_input_closure(&self.dag, drv_hash);
            if !input_paths.is_empty()
                && let Err(e) = self.db.pin_live_inputs(drv_hash, &input_paths).await
            {
                debug!(drv_hash = %drv_hash, error = %e,
                       "failed to pin live inputs for pull attempt (best-effort)");
            }
        }

        let assignment = self
            .build_assignment_proto(drv_hash, pulling_identity)
            .await
            .ok_or_else(|| {
                PullRejection::Internal("derivation vanished while building the payload".into())
            })?;
        // Display events, per work class: build mints emit STARTED (the
        // as-built path, byte-identical); materialization mints emit
        // SUBSTITUTING (BC-4 — the wire-retained kind whose emission
        // moves from walk-spawn to claim intake; STARTED would stop the
        // gateway's actSubstitute/actCopyPath pair the instant it opened).
        match rio_evidence_kernel::pull::display_class(match attempt_kind {
            crate::state::AttemptKind::Build => rio_evidence_kernel::pull::PullKind::Build,
            crate::state::AttemptKind::Materialization => {
                rio_evidence_kernel::pull::PullKind::Materialization
            }
        }) {
            rio_evidence_kernel::pull::DisplaySurface::Build => {
                self.emit_assignment_started(drv_hash, pulling_identity);
            }
            // r[impl sched.materialize.job+2]
            rio_evidence_kernel::pull::DisplaySurface::Substitution => {
                self.emit_materialization_claimed(drv_hash);
            }
        }
        info!(
            drv_hash = %drv_hash,
            exec_id = %exec_id,
            executor_id = %pulling_identity,
            "pull-mode attempt opened"
        );
        metrics::counter!("rio_scheduler_assignments_total").increment(1);
        Ok(PullOutcome::Deliver(Box::new(assignment)))
    }
}

#[cfg(test)]
mod kernel_tests {
    use super::*;

    fn base_inputs<'a>(
        status: Option<DerivationStatus>,
        pulling: &'a ExecutorId,
        open: Option<(&'a ExecutorId, Uuid)>,
    ) -> PullInputs<'a> {
        PullInputs {
            intent_id: "drv-x",
            auth_intent: Some("drv-x"),
            status,
            open_attempt: open,
            pulling_identity: pulling,
            serving_generation: 3,
            generation_floor: Some(3),
            pull_kind: rio_evidence_kernel::pull::PullKind::Build,
            job_view: rio_evidence_kernel::pull::JobView::None,
        }
    }

    /// Exhaustive status → decision table for the no-open-attempt case.
    #[test]
    fn admit_pull_status_table() {
        use DerivationStatus as S;
        let me = ExecutorId::from("drv-x");
        for (status, want) in [
            (None, PullDecision::Gone),
            (Some(S::Created), PullDecision::NotYetReady),
            (Some(S::Queued), PullDecision::NotYetReady),
            (Some(S::Failed), PullDecision::NotYetReady),
            (Some(S::Ready), PullDecision::DeliverNew),
            (Some(S::Completed), PullDecision::Gone),
            (Some(S::Cancelled), PullDecision::Gone),
            (Some(S::Skipped), PullDecision::Gone),
            (Some(S::Poisoned), PullDecision::Gone),
            (Some(S::DependencyFailed), PullDecision::Gone),
        ] {
            let got = admit_pull(&base_inputs(status, &me, None));
            assert_eq!(got, want, "status {status:?}");
        }
        // Assigned/Running with no recorded open attempt: never deliver.
        for status in [S::Assigned, S::Running] {
            assert_eq!(
                admit_pull(&base_inputs(Some(status), &me, None)),
                PullDecision::NotYetReady,
                "in-flight without exec bookkeeping must wait"
            );
        }
    }

    /// Open-attempt identity match decides re-delivery vs wait.
    #[test]
    fn admit_pull_open_attempt_identity() {
        use DerivationStatus as S;
        let me = ExecutorId::from("drv-x");
        let other = ExecutorId::from("pool-pod-7");
        let exec = Uuid::now_v7();
        for status in [S::Assigned, S::Running] {
            assert_eq!(
                admit_pull(&base_inputs(Some(status), &me, Some((&me, exec)))),
                PullDecision::DeliverExisting { exec_id: exec },
            );
            assert_eq!(
                admit_pull(&base_inputs(Some(status), &me, Some((&other, exec)))),
                PullDecision::NotYetReady,
                "an attempt open on another executor is never re-delivered or re-pointed"
            );
        }
    }

    /// The token binding and the generation fence dominate everything.
    #[test]
    fn admit_pull_rejections_dominate() {
        let me = ExecutorId::from("drv-x");
        // Token mismatch wins even for a Ready drv.
        let mut inputs = base_inputs(Some(DerivationStatus::Ready), &me, None);
        inputs.auth_intent = Some("drv-other");
        assert_eq!(admit_pull(&inputs), PullDecision::RejectToken);
        // Below-floor serving generation answers nothing.
        let mut inputs = base_inputs(Some(DerivationStatus::Ready), &me, None);
        inputs.serving_generation = 2;
        inputs.generation_floor = Some(3);
        assert_eq!(admit_pull(&inputs), PullDecision::RejectStaleGeneration);
        // Dev mode (no token) and fresh cluster (no floor) both admit.
        let mut inputs = base_inputs(Some(DerivationStatus::Ready), &me, None);
        inputs.auth_intent = None;
        inputs.generation_floor = None;
        assert_eq!(admit_pull(&inputs), PullDecision::DeliverNew);
    }
}

/// What to do with one `ReportOutcome` (the pure half of the report
/// intake — terminal-row-wins / no-transition-out-of-terminal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReportAdmission {
    /// First report for an open attempt: run the existing
    /// classification path (the same entry point the stream arm uses).
    Process,
    /// Duplicate, post-establishment, superseded, or never-pulled:
    /// acknowledge and write nothing.
    AckIgnore,
}

// r[impl sched.executor.report-idempotent]
/// Decide one report from the attempt's durable state. Pure (decision
/// P10): the attempt is processed only when it is still open
/// (assignment active) and no classification row exists for its exec —
/// a terminal or already-classified row always wins and is never
/// overwritten or re-charged.
pub(crate) fn fold_report(
    assignment_active: bool,
    attempt_already_classified: bool,
) -> ReportAdmission {
    if assignment_active && !attempt_already_classified {
        ReportAdmission::Process
    } else {
        ReportAdmission::AckIgnore
    }
}

/// The report payload fields forwarded from the gRPC layer — the same
/// set the stream `ProcessCompletion` arm carries, so the intake can
/// funnel into the identical internal entry point.
#[derive(Debug)]
pub struct PullReportPayload {
    pub result: rio_proto::types::BuildResult,
    pub peak_memory_bytes: u64,
    pub peak_cpu_cores: f64,
    pub node_name: Option<String>,
    pub hw_class: Option<String>,
    pub final_resources: Option<rio_proto::types::ResourceUsage>,
    pub final_line_count: u64,
    /// Substitution-replacement: set INSTEAD of `result` for
    /// materialization attempts (the gRPC layer rejects requests
    /// carrying both). `None` for every build report — the as-built
    /// shape, bit-identical.
    pub materialization_outcome: Option<rio_proto::types::MaterializationOutcome>,
}

impl DagActor {
    /// Handle one `ReportOutcome` (the actor turn): resolve the exec_id
    /// to its attempt, decide via [`fold_report`], and on Process run
    /// the existing completion path — the same `handle_completion`
    /// entry point the stream arm calls, so the worker-report→fold feed
    /// is identical in classification terms. The reply is sent only
    /// after that call returns (its appending transaction has
    /// committed by then), which is what the pod's exit-0 waits for.
    // r[impl sched.executor.report-idempotent]
    pub(super) async fn handle_report_outcome(
        &mut self,
        exec_id: Uuid,
        auth_intent: Option<String>,
        payload: PullReportPayload,
        reply: oneshot::Sender<Result<(), PullRejection>>,
    ) {
        let result = self
            .report_outcome_inner(exec_id, auth_intent.as_deref(), payload)
            .await;
        let _ = reply.send(result);
    }

    async fn report_outcome_inner(
        &mut self,
        exec_id: Uuid,
        auth_intent: Option<&str>,
        payload: PullReportPayload,
    ) -> Result<(), PullRejection> {
        if !self.leader.is_leader() {
            return Err(PullRejection::NotLeader);
        }
        let attempt = self
            .db
            .find_attempt_by_exec_id(exec_id)
            .await
            .map_err(|e| PullRejection::Internal(format!("attempt lookup failed: {e}")))?;

        let Some(attempt) = attempt else {
            // Never-pulled (or superseded) exec: acknowledged, nothing
            // written — the no-open-attempt arm of report idempotency.
            debug!(%exec_id, "ReportOutcome for unknown/superseded exec acknowledged-and-ignored");
            return Ok(());
        };
        // Token↔intent binding (per-unary, same as the pull): the
        // attested intent must be the attempt's derivation.
        if let Some(auth) = auth_intent
            && auth != attempt.core().drv_hash
        {
            // r[impl sec.executor.identity-token+3]
            warn!(%exec_id, "ReportOutcome rejected: executor token bound to a different intent");
            return Err(PullRejection::TokenMismatch);
        }
        // The kind witness (A2): materialization attempts route to
        // their own consumption transaction; the build arms below bind
        // `&BuildAttempt`, so a cross-kind close no longer typechecks.
        // Kind mismatch between witness and payload is acknowledged-
        // and-ignored (the kindMatchesWorker rule at the report
        // intake). Both arms are reachable FLAG-OFF from a
        // buggy/hostile reporter, so the warn+ack posture is a dormancy
        // guarantee, not just a flag-on routing rule.
        let b = match &attempt {
            crate::db::open_attempts::AttemptRef::Materialization(m) => {
                return match payload.materialization_outcome {
                    Some(outcome) => {
                        if m.core.attempt_terminal || m.core.attempt_recorded {
                            // Terminal-row-wins: a duplicate/late report for
                            // an already-consumed attempt is acknowledged.
                            debug!(%exec_id, "duplicate materialization report acknowledged-and-ignored");
                            return Ok(());
                        }
                        self.consume_materialization_outcome(exec_id, m, outcome)
                            .await
                    }
                    None => {
                        warn!(%exec_id, "build-report payload for a materialization attempt; ignoring");
                        Ok(())
                    }
                };
            }
            crate::db::open_attempts::AttemptRef::Build(b) => b,
        };
        if payload.materialization_outcome.is_some() {
            warn!(%exec_id, "materialization payload for a build attempt; acknowledged-and-ignored");
            return Ok(());
        }
        match fold_report(
            b.core.assignment_active,
            b.core.attempt_recorded || b.core.attempt_terminal,
        ) {
            ReportAdmission::AckIgnore => {
                debug!(
                    %exec_id,
                    drv_hash = %b.core.drv_hash,
                    assignment_active = b.core.assignment_active,
                    "duplicate/late ReportOutcome acknowledged-and-ignored"
                );
                Ok(())
            }
            ReportAdmission::Process => {
                let executor_id = ExecutorId::from(b.core.executor_id.as_str());
                // r[impl sched.attempt.synthesized-verdict+2]
                // AD5 abort charge class: a pod reporting `Cancelled`
                // for a derivation the scheduler still wants is the
                // SIGTERM-abort report (preemption, scale-down,
                // controller delete) — a platform termination, not a
                // worker fault. It closes the attempt charge-free and
                // requeues at this fold; it is never charged as an
                // infrastructure failure. A genuinely-cancelled
                // (no-longer-wanted) derivation falls through to the
                // completion path's Cancelled early-return and stays
                // exactly as the cancel arm leaves it (no row, no
                // requeue).
                let drv_hash = DrvHash::from(b.core.drv_hash.as_str());
                let abort_of_still_wanted = payload.result.status()
                    == rio_proto::types::BuildResultStatus::Cancelled
                    && self.dag.node(&drv_hash).is_some_and(|s| {
                        matches!(
                            s.status(),
                            DerivationStatus::Assigned | DerivationStatus::Running
                        ) && s.exec_id == Some(exec_id)
                    });
                if abort_of_still_wanted {
                    // r[impl sched.attempt.worker-abort-bounded]
                    // bug_279: the charge-free admission is LEDGER-
                    // BOUNDED — the worker-supplied Cancelled
                    // discriminator is trusted only while the trailing
                    // run of build-lane worker-abort closures is below
                    // the kernel bound. At the bound the report falls
                    // through to the charged unsolicited-Cancelled→
                    // infrastructure arm of the completion path,
                    // consuming the attempt WITH budget (exclusion and
                    // poison advance; the loop is finite).
                    let admission = self
                        .dag
                        .node(&drv_hash)
                        .map(|s| crate::retry_policy::admit_worker_abort(s.attempt_history()))
                        .unwrap_or(rio_retry_kernel::WorkerAbortAdmission::Uncharged);
                    if admission == rio_retry_kernel::WorkerAbortAdmission::Uncharged {
                        info!(
                            %exec_id,
                            drv_hash = %b.core.drv_hash,
                            "pull-mode SIGTERM-abort report for still-wanted work: closing the \
                             attempt charge-free and requeueing (AD5)"
                        );
                        // AD2c: node attribution comes from the
                        // controller-authoritative binding only — never the
                        // worker-supplied report fields.
                        let source_node = self.pull_attempt_source_node(&drv_hash);
                        self.close_pull_attempt_uncharged(
                            b,
                            exec_id,
                            "worker_abort",
                            crate::state::ReportingParty::Worker,
                            source_node,
                            "worker-abort",
                        )
                        .await;
                        return Ok(());
                    }
                    warn!(
                        %exec_id,
                        drv_hash = %b.core.drv_hash,
                        bound = rio_retry_kernel::WORKER_ABORT_FREE_CLOSES,
                        "worker-abort free-close run at the bound; consuming the report as a \
                         charged infrastructure failure (worker-protocol loop)"
                    );
                }
                // Same internal entry point as the stream Completion
                // arm — classification, verdict, attempt-row append,
                // status persist, realisations, SLA samples all happen
                // exactly as they do for stream-reported outcomes. The
                // drv is addressed by the attempt's own derivation (the
                // exec_id is the key; the report's drv_path is not
                // trusted to re-route it).
                self.handle_completion(
                    &executor_id,
                    &b.core.drv_path,
                    payload.result,
                    (payload.peak_memory_bytes, payload.peak_cpu_cores),
                    (payload.node_name, payload.hw_class),
                    (payload.final_resources, payload.final_line_count),
                )
                .await;
                Ok(())
            }
        }
    }

    /// Close one open pull-mode attempt charge-free (the AD5 abort /
    /// synthesized-verdict closure): append exactly one uncharged
    /// terminal row (`disconnected`, `termination_reason` filled — the
    /// fold treats it as a no-charge event and the open-attempt view
    /// drops it) and close the assignment row in ONE transaction
    /// carrying the same claims-floor fence as the pull mint and the
    /// establishment sweep, then mirror the row in memory and requeue
    /// the derivation if this attempt was still the in-flight one.
    /// Idempotent: a row already present for the exec (any classifier
    /// won first) makes the append a no-op and nothing else changes.
    // r[impl sched.attempt.synthesized-verdict+2]
    /// Takes the BUILD witness: a materialization attempt cannot be
    /// closed by this path — the cross-kind call no longer typechecks
    /// (merged_bug_146's structural half).
    async fn close_pull_attempt_uncharged(
        &mut self,
        attempt: &crate::db::open_attempts::BuildAttempt,
        exec_id: Uuid,
        termination_reason: &str,
        reporting_party: crate::state::ReportingParty,
        source_node: Option<String>,
        requeue_cause: &'static str,
    ) {
        if !self.leader.is_leader() {
            return;
        }
        let drv_hash = &DrvHash::from(attempt.core.drv_hash.as_str());
        let executor_id = &ExecutorId::from(attempt.core.executor_id.as_str());
        let serving_generation = self.leader.generation() as i64;
        let mut row = crate::db::attempts::AttemptRow::new(
            attempt.core.derivation_id,
            crate::state::OutcomeClass::Disconnected,
            reporting_party,
            crate::state::AttemptKind::Build,
        );
        row.exec_id = Some(exec_id);
        row.executor_id = Some(executor_id.clone());
        row.source_node = source_node;
        row.termination_reason = Some(termination_reason.to_owned());
        if let Some(state) = self.dag.node(drv_hash) {
            row.resubmit_cycle = i32::try_from(state.retry.resubmit_cycles).unwrap_or(i32::MAX);
        }
        let result: Result<Option<bool>, sqlx::Error> = async {
            // The same generation fence the pull mint and the
            // establishment sweep apply: a below-floor serving
            // generation writes nothing.
            let mut tx = match self.db.begin_fenced(serving_generation).await? {
                crate::db::FencedBegin::Fenced { .. } => return Ok(None),
                crate::db::FencedBegin::Open(ftx) => ftx,
            };
            let inserted = crate::db::SchedulerDb::append_attempt(tx.conn(), &row).await?;
            tx.close_assignment(exec_id, crate::db::AssignmentCloseStatus::Failed)
                .await?;
            tx.commit().await?;
            Ok(Some(inserted))
        }
        .await;
        let inserted = match result {
            Ok(Some(inserted)) => inserted,
            Ok(None) => {
                info!(drv_hash = %drv_hash, serving_generation,
                      "uncharged close: serving generation below the claims floor; nothing written");
                return;
            }
            Err(e) => {
                warn!(drv_hash = %drv_hash, %exec_id, error = %e,
                      "uncharged close failed; the attempt stays open (the establishment \
                       sweep remains the backstop)");
                return;
            }
        };
        if !inserted {
            // Another classifier won the row; its verdict stands.
            return;
        }
        if let Some(state) = self.dag.node_mut(drv_hash) {
            state.push_attempt_record(row.to_record());
        }
        self.refresh_retry_view(drv_hash);
        // OA1 interval: the closing observation and the requeue happen
        // in this same actor turn, so the recorded latency is the
        // in-turn processing time (the same shape as the worker-report
        // cause).
        metrics::histogram!(
            "rio_scheduler_attempt_requeue_seconds",
            "cause" => requeue_cause
        )
        .record((crate::db::attempts::epoch_now() - row.occurred_at_epoch_secs).max(0.0));
        let still_in_flight = self.dag.node(drv_hash).is_some_and(|s| {
            matches!(
                s.status(),
                DerivationStatus::Assigned | DerivationStatus::Running
            ) && s.exec_id == Some(exec_id)
        });
        if still_in_flight {
            self.reassign_derivations(std::slice::from_ref(drv_hash), Some(executor_id))
                .await;
        }
    }
}

/// The attempt identity carried by `ReportAttemptOutcome` (exec_id
/// preferred; intent id accepted; the Job name is recorded for
/// diagnostics — its full resolution arrives with the controller-side
/// re-point, which always knows the exec/intent from the open-attempt
/// view).
#[derive(Debug, Default)]
pub struct AttemptIdentity {
    pub intent_id: Option<String>,
    pub job_name: Option<String>,
    pub exec_id: Option<Uuid>,
}

/// Map the wire reason to the `termination_reason` label the second
/// installment records.
pub(crate) fn attempt_terminal_reason_label(
    reason: rio_proto::types::AttemptTerminalReason,
) -> &'static str {
    use rio_proto::types::AttemptTerminalReason as R;
    match reason {
        R::Unspecified => "unspecified",
        R::OomKilled => "oom_killed",
        R::EvictedDiskPressure => "evicted_disk_pressure",
        R::EvictedOther => "evicted_other",
        R::Completed => "pod_completed",
        R::Error => "pod_error",
        R::DeadlineExceeded => "deadline_exceeded",
        R::Cancelled => "cancelled",
        R::Preempted => "preempted",
        R::Reaped => "reaped",
        R::NoEligibleSource => "no_eligible_source",
    }
}

impl DagActor {
    /// Handle one `ReportAttemptOutcome` (the unified pod-terminal
    /// intake, scheduler half). Idempotent: the only write it ever
    /// performs is the reason-only second-installment fill on an
    /// existing, still-unfilled classification row; it never inserts a
    /// row, never consumes budget, and never bumps a floor.
    ///
    /// The no-attempt arm (a pod that died without ever completing a
    /// pull) acknowledges and charges nothing; its only permitted side
    /// effects are clearing the intent's ICE cell (the
    /// `dispatched_cells` arm for that intent) and re-arming the spawn
    /// intent — which, for a still-wanted drv, simply means leaving it
    /// Ready so the next `GetSpawnIntents` re-emits it.
    // r[impl ctrl.report.attempt-outcome]
    // r[impl sched.attempt.no-attempt-no-op]
    pub(super) async fn handle_report_attempt_outcome(
        &mut self,
        identity: AttemptIdentity,
        reason: rio_proto::types::AttemptTerminalReason,
        node_name: Option<String>,
        reply: oneshot::Sender<Result<(), PullRejection>>,
    ) {
        let result = self
            .report_attempt_outcome_inner(identity, reason, node_name)
            .await;
        let _ = reply.send(result);
    }

    async fn report_attempt_outcome_inner(
        &mut self,
        identity: AttemptIdentity,
        reason: rio_proto::types::AttemptTerminalReason,
        node_name: Option<String>,
    ) -> Result<(), PullRejection> {
        if !self.leader.is_leader() {
            return Err(PullRejection::NotLeader);
        }
        // AD2(a): the spawn-gate exhaustion verdict. Not a pod-terminal
        // classification — there is no pod and no attempt — so it is
        // handled before attempt resolution: the controller (which owns
        // the spawnable-source universe) detected excluded ⊇ spawnable
        // for this intent and the fold maps that to the fleet-exhaust
        // poison arm, exactly like the dispatch-time E9 backstop.
        if reason == rio_proto::types::AttemptTerminalReason::NoEligibleSource {
            return self.handle_no_eligible_source(&identity).await;
        }
        // Resolve the attempt: exec_id first, then the intent's open
        // pull-mode attempt. A Job-name-only report cannot be resolved
        // here yet (the deterministic name embeds only a derived
        // suffix); the controller-side callers always know the
        // exec/intent from ListOpenAttempts.
        let resolved = if let Some(exec_id) = identity.exec_id {
            self.db
                .find_attempt_by_exec_id(exec_id)
                .await
                .map_err(|e| PullRejection::Internal(format!("attempt lookup failed: {e}")))?
                .map(|row| (exec_id, row))
        } else if let Some(intent) = identity.intent_id.as_deref().filter(|s| !s.is_empty()) {
            self.db
                .find_open_pull_attempt_by_drv_hash(intent)
                .await
                .map_err(|e| PullRejection::Internal(format!("attempt lookup failed: {e}")))?
        } else {
            None
        };

        let Some((exec_id, attempt)) = resolved else {
            // Pull-side no-attempt side effect first (the never-pulled
            // pod-death case): drop the intent's ICE-clear arm so a
            // death before the first pull cannot leave a stale
            // Pending-watch entry behind, regardless of whether the
            // report also carries a job name that routes below.
            if let Some(intent) = identity.intent_id.as_deref().filter(|s| !s.is_empty()) {
                self.dispatched_cells.remove(intent);
            }
            // The no-attempt no-op arm: acknowledge, charge nothing,
            // and leave the (still-wanted) drv exactly as it is so the
            // spawn intent re-arms on the next controller poll.
            debug!(
                intent_id = ?identity.intent_id,
                job_name = ?identity.job_name,
                exec_id = ?identity.exec_id,
                ?reason,
                "ReportAttemptOutcome with no matching attempt acknowledged charge-free"
            );
            return Ok(());
        };

        // The kind witness FIRST (merged_bug_146): a controller verdict
        // names a BUILD-lifecycle event — the controller deleting a
        // builder Job — and never consumes a store replica's open
        // materialization attempt. Acknowledged charge-free BEFORE the
        // AD2c fill below, so the fill can never stamp a builder node
        // onto a materialization exec row (the 084 CHECK makes that
        // unrepresentable; this arm makes it unreachable) and the
        // store's own outcome report stays the attempt's only consumer.
        let b = match &attempt {
            crate::db::open_attempts::AttemptRef::Materialization(_) => {
                debug!(
                    %exec_id,
                    drv_hash = %attempt.core().drv_hash,
                    ?reason,
                    "ReportAttemptOutcome for a materialization attempt acknowledged \
                     charge-free (controller verdicts never consume store attempts)"
                );
                return Ok(());
            }
            crate::db::open_attempts::AttemptRef::Build(b) => b,
        };

        if b.core.attempt_terminal {
            // Duplicate / already established: idempotent no-op.
            return Ok(());
        }

        // AD2c: persist the controller-reported node onto the open
        // execution row when the mint lost the binding-ack race
        // (NULL-only fill, best-effort), so a later establishment
        // charge carries the node key even when this report itself
        // classifies nothing. Never a worker-supplied value — the
        // node here comes from the controller's informer view.
        if let Some(node) = node_name.as_deref().filter(|s| !s.is_empty())
            && let Err(e) = self.db.fill_open_execution_source_node(exec_id, node).await
        {
            debug!(%exec_id, error = %e,
                   "open-execution source_node fill failed (best-effort)");
        }

        let label = attempt_terminal_reason_label(reason);
        if b.core.attempt_recorded {
            // Second installment on the worker-reported row: fill the
            // termination reason only — never a reclassification, never
            // a new row, never a budget or floor change.
            let derivation_id = b.core.derivation_id;
            let won = self
                .db
                .fill_termination_reason_only(
                    derivation_id,
                    exec_id,
                    label,
                    node_name.as_deref(),
                    self.serving_generation(),
                )
                .await
                .map_err(|e| PullRejection::Internal(format!("installment fill failed: {e}")))?
                .applied();
            let drv_hash = DrvHash::from(b.core.drv_hash.as_str());
            if won {
                if let Some(state) = self.dag.node_mut(&drv_hash) {
                    state.fill_attempt_termination_reason(exec_id, label, node_name.as_deref());
                }
                // A won fill on an attempt whose derivation is still
                // in-flight on this attempt means no other observer has
                // resolved it yet — requeue now (the pod is gone), and
                // record the pod-terminal requeue interval.
                let still_in_flight = self.dag.node(&drv_hash).is_some_and(|s| {
                    matches!(
                        s.status(),
                        DerivationStatus::Assigned | DerivationStatus::Running
                    ) && s.exec_id == Some(exec_id)
                });
                if still_in_flight {
                    // OA1 interval, pod-terminal cause: the attempt's
                    // classifying observation -> this requeue.
                    let observed_at = self
                        .dag
                        .node(&drv_hash)
                        .and_then(|s| {
                            s.attempt_history()
                                .iter()
                                .rev()
                                .find(|r| r.exec_id == Some(exec_id))
                                .map(|r| r.occurred_at_epoch_secs)
                        })
                        .unwrap_or_else(crate::db::attempts::epoch_now);
                    metrics::histogram!(
                        "rio_scheduler_attempt_requeue_seconds",
                        "cause" => "pod-terminal"
                    )
                    .record((crate::db::attempts::epoch_now() - observed_at).max(0.0));
                    let executor = ExecutorId::from(b.core.executor_id.as_str());
                    self.reassign_derivations(std::slice::from_ref(&drv_hash), Some(&executor))
                        .await;
                }
            }
            return Ok(());
        }

        // Open attempt with no classification row yet (pulled, never
        // worker-reported, pod now terminal). The split:
        //
        //   - Controller-synthesized verdicts (cancelled / preempted /
        //     reaped — the AD5/C5/C6 synthesize-on-delete and
        //     DisruptionTarget arms) classify HERE: the controller is
        //     deleting (or has deleted) the Job, so no other observer
        //     will ever report this attempt; the close is charge-free
        //     and the still-wanted derivation requeues at this fold,
        //     never at the establishment sweep.
        //   - Pod-terminal reasons without a worker row (OOM, eviction,
        //     deadline, plain error) keep waiting: the establishment
        //     sweep stays their classifier (the 1b gate text), because
        //     the worker's own classifying report may still arrive.
        // r[impl sched.attempt.synthesized-verdict+2]
        use rio_proto::types::AttemptTerminalReason as R;
        if b.core.assignment_active && matches!(reason, R::Cancelled | R::Preempted | R::Reaped) {
            let drv_hash = DrvHash::from(b.core.drv_hash.as_str());
            // AD2c: prefer the controller-reported node from this very
            // report, fall back to the spawn-ack binding; never a
            // worker-supplied value.
            let source_node = node_name
                .clone()
                .or_else(|| self.pull_attempt_source_node(&drv_hash));
            info!(
                %exec_id,
                drv_hash = %b.core.drv_hash,
                ?reason,
                "controller-synthesized verdict for an open pull attempt: closing charge-free"
            );
            self.close_pull_attempt_uncharged(
                b,
                exec_id,
                label,
                crate::state::ReportingParty::Controller,
                source_node,
                "synthesized",
            )
            .await;
            return Ok(());
        }
        debug!(
            %exec_id,
            drv_hash = %b.core.drv_hash,
            ?reason,
            "ReportAttemptOutcome for an unclassified open attempt acknowledged (no fill \
             target; the establishment sweep remains its classifier)"
        );
        Ok(())
    }

    // r[impl sched.dispatch.fleet-exhaust+4]
    /// The spawn-gate exhaustion arm of `ReportAttemptOutcome`
    /// (`reason = NoEligibleSource`, AD2a): the controller holds the
    /// node informers, so it is the party that can observe "every
    /// source this intent could be scheduled onto is already excluded".
    /// The scheduler maps that observation to the same fleet-exhaust
    /// poison the dispatch-time E9 backstop produces: a `fleet_exhaust`
    /// marker row (no charge — the fold treats it as a no-op event)
    /// appended in the same transaction as the poison persist, then the
    /// cascade. Idempotent: only a currently-Ready derivation is acted
    /// on — an already-poisoned (or in-flight, or terminal, or unknown)
    /// drv acknowledges and changes nothing, so controller re-ticks and
    /// duplicate reports are no-ops.
    async fn handle_no_eligible_source(
        &mut self,
        identity: &AttemptIdentity,
    ) -> Result<(), PullRejection> {
        let Some(intent) = identity.intent_id.as_deref().filter(|s| !s.is_empty()) else {
            debug!(
                job_name = ?identity.job_name,
                "NoEligibleSource report without an intent id acknowledged (nothing to act on)"
            );
            return Ok(());
        };
        let drv_hash = DrvHash::from(intent);
        let status = self.dag.node(&drv_hash).map(|s| s.status());
        if status != Some(DerivationStatus::Ready) {
            debug!(
                intent_id = %intent,
                ?status,
                "NoEligibleSource for a non-Ready derivation acknowledged (already resolved, \
                 in flight, or unknown)"
            );
            return Ok(());
        }
        if let Some(state) = self.dag.node(&drv_hash) {
            warn!(
                intent_id = %intent,
                system = %state.system,
                excluded = state.retry.failed_builders.len(),
                "controller reported NoEligibleSource: every spawnable source for this \
                 derivation is excluded; poisoning (AD2 spawn-gate fleet exhaust)"
            );
        }
        metrics::counter!("rio_scheduler_poison_fleet_exhausted_total").increment(1);
        // Same marker-row discipline as the dispatch-time arm: a verdict
        // marker is not an execution, so the execution/executor/node
        // attribution is cleared before the append.
        let marker = self
            .attempt_row_for(
                &drv_hash,
                crate::state::OutcomeClass::FleetExhaust,
                crate::state::ReportingParty::Controller,
            )
            .map(|mut row| {
                row.exec_id = None;
                row.executor_id = None;
                row.source_node = None;
                row
            });
        self.poison_and_cascade(
            &drv_hash,
            "no eligible source: every spawnable node is excluded for this derivation \
             (controller spawn gate)",
            None,
            marker,
        )
        .await;
        Ok(())
    }
}
