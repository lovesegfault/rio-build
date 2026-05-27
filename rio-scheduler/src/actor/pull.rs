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
//! a *different* identity (a stream-mode executor during coexistence)
//! is never re-delivered or re-pointed.

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

/// The pure admission decision for one pull.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PullDecision {
    /// No open attempt for this identity: run the fenced mint
    /// transaction and deliver a fresh payload.
    DeliverNew,
    /// The open attempt already belongs to the pulling identity:
    /// re-deliver the identical payload/exec_id, write nothing.
    DeliverExisting { exec_id: Uuid },
    /// No longer wanted: cancelled, substituted/completed, skipped,
    /// permanently failed/poisoned, or absent from the DAG.
    Gone,
    /// Still wanted but not deliverable to this pod right now.
    NotYetReady,
    /// Token↔intent binding failed.
    RejectToken,
    /// Serving generation below the durable claims floor.
    RejectStaleGeneration,
}

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
}

// r[impl sched.executor.pull-gone]
// r[impl sched.executor.pull-not-ready]
/// Decide one pull from already-loaded state. Pure — no clocks, no IO,
/// no `&self` — so it can be unit-tested exhaustively and lifted into a
/// Kani harness without refactoring (decision P10).
///
/// Check order is load-bearing: identity first (a mis-bound token never
/// learns anything about the drv), then the generation fence (a deposed
/// believer answers nothing), then wantedness/deliverability.
pub(crate) fn admit_pull(inputs: &PullInputs<'_>) -> PullDecision {
    // Token↔intent binding (mechanism #6, applied per-unary).
    if let Some(auth) = inputs.auth_intent
        && auth != inputs.intent_id
    {
        return PullDecision::RejectToken;
    }

    // Transaction-side generation fence, advisory half: a serving
    // generation below the durable claims floor answers nothing. The
    // authoritative check re-runs inside the mint transaction.
    if let Some(floor) = inputs.generation_floor
        && floor >= 0
        && inputs.serving_generation < floor as u64
    {
        return PullDecision::RejectStaleGeneration;
    }

    let Some(status) = inputs.status else {
        // Not in the DAG: nothing wants it (never submitted, already
        // reaped after completion, or cancelled and swept).
        return PullDecision::Gone;
    };

    use DerivationStatus as S;
    match status {
        // No longer wanted: terminal or permanently failed states.
        S::Completed | S::Cancelled | S::Skipped | S::Poisoned | S::DependencyFailed => {
            PullDecision::Gone
        }
        // Wanted but not deliverable yet: deps unbuilt, substitution in
        // flight, or a retry waiting to requeue. Never `Gone` (the
        // reap→respawn churn loop), never a write.
        S::Created | S::Queued | S::Substituting | S::Failed => PullDecision::NotYetReady,
        // Ready: deliverable now — mint a fresh attempt.
        S::Ready => PullDecision::DeliverNew,
        // Already open on some executor: idempotent re-delivery only
        // for the same identity; anyone else waits (a stream-mode
        // assignment during coexistence, or another pod's attempt).
        S::Assigned | S::Running => match inputs.open_attempt {
            Some((executor, exec_id)) if executor == inputs.pulling_identity => {
                PullDecision::DeliverExisting { exec_id }
            }
            // Open elsewhere — or in-flight bookkeeping is missing its
            // exec_id (never deliverable without an attempt to share).
            _ => PullDecision::NotYetReady,
        },
    }
}

impl DagActor {
    /// Handle one `PullAssignment` (the actor turn). Computes the
    /// admission via [`admit_pull`], executes the decision, and replies.
    pub(super) async fn handle_pull_assignment(
        &mut self,
        intent_id: String,
        auth_intent: Option<String>,
        reply: oneshot::Sender<Result<PullOutcome, PullRejection>>,
    ) {
        let result = self
            .pull_assignment_inner(&intent_id, auth_intent.as_deref())
            .await;
        let _ = reply.send(result);
    }

    // r[impl sched.executor.pull-transaction]
    async fn pull_assignment_inner(
        &mut self,
        intent_id: &str,
        auth_intent: Option<&str>,
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
        // The attempt's executor identity is the attested intent (see
        // the module doc) — not a pod name the request cannot carry.
        let pulling_identity = ExecutorId::from(intent_id);

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
        });

        match decision {
            PullDecision::RejectToken => {
                // r[impl sec.executor.identity-token+2]
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
                // rebuilt from the same inputs (drv, identity,
                // generation), so the load-bearing fields — drv path,
                // ATerm, exec_id, resources — are identical.
                debug_assert_eq!(
                    self.dag.node(&drv_hash).and_then(|s| s.exec_id),
                    Some(exec_id)
                );
                let assignment = self
                    .build_assignment_proto(&drv_hash, &pulling_identity, serving_generation)
                    .await
                    .ok_or_else(|| {
                        PullRejection::Internal("derivation vanished during re-pull".into())
                    })?;
                Ok(PullOutcome::Deliver(Box::new(assignment)))
            }
            PullDecision::DeliverNew => {
                self.mint_and_deliver(&drv_hash, &pulling_identity, serving_generation)
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

        let committed = self
            .db
            .mint_pull_attempt_fenced(
                db_id,
                pulling_identity,
                serving_generation as i64,
                exec_id,
                &log_hash,
                source_node.as_deref(),
            )
            .await
            .map_err(|e| PullRejection::Internal(format!("pull mint transaction failed: {e}")))?;
        if !committed {
            info!(
                drv_hash = %drv_hash,
                serving_generation,
                "pull mint aborted by the generation fence; no row written"
            );
            return Err(PullRejection::StaleGeneration);
        }

        // Durable mint committed — now the in-memory bookkeeping, the
        // same shape the stream path's record phase keeps (transition,
        // exec_id, assigned executor, status persist, GC pins).
        if let Some(state) = self.dag.node_mut(drv_hash) {
            if let Err(e) = state.transition(DerivationStatus::Assigned) {
                // TOCTOU vs a concurrent cancel between the admit and
                // the commit: the durable rows exist but the node left
                // Ready. The attempt resolves via the normal terminal
                // paths (report or establishment); never deliver.
                warn!(drv_hash = %drv_hash, error = %e,
                      "pull minted but Ready→Assigned rejected; answering NotYetReady");
                return Ok(PullOutcome::NotYetReady {
                    retry_after_secs: NOT_YET_READY_RETRY_AFTER_SECS,
                });
            }
            state.retry.backoff_until = None;
            state.assigned_executor = Some(pulling_identity.clone());
            state.exec_id = Some(exec_id);
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
        // stream path's record phase.
        let input_paths = crate::assignment::approx_input_closure(&self.dag, drv_hash);
        if !input_paths.is_empty()
            && let Err(e) = self.db.pin_live_inputs(drv_hash, &input_paths).await
        {
            debug!(drv_hash = %drv_hash, error = %e,
                   "failed to pin live inputs for pull attempt (best-effort)");
        }

        let assignment = self
            .build_assignment_proto(drv_hash, pulling_identity, serving_generation)
            .await
            .ok_or_else(|| {
                PullRejection::Internal("derivation vanished while building the payload".into())
            })?;
        self.emit_assignment_started(drv_hash, pulling_identity);
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
            (Some(S::Substituting), PullDecision::NotYetReady),
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
pub(crate) struct PullReportPayload {
    pub result: rio_proto::types::BuildResult,
    pub peak_memory_bytes: u64,
    pub peak_cpu_cores: f64,
    pub node_name: Option<String>,
    pub hw_class: Option<String>,
    pub final_resources: Option<rio_proto::types::ResourceUsage>,
    pub final_line_count: u64,
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
            && auth != attempt.drv_hash
        {
            // r[impl sec.executor.identity-token+2]
            warn!(%exec_id, "ReportOutcome rejected: executor token bound to a different intent");
            return Err(PullRejection::TokenMismatch);
        }
        match fold_report(
            attempt.assignment_active,
            attempt.attempt_recorded || attempt.attempt_terminal,
        ) {
            ReportAdmission::AckIgnore => {
                debug!(
                    %exec_id,
                    drv_hash = %attempt.drv_hash,
                    assignment_active = attempt.assignment_active,
                    "duplicate/late ReportOutcome acknowledged-and-ignored"
                );
                Ok(())
            }
            ReportAdmission::Process => {
                let executor_id = ExecutorId::from(attempt.executor_id.as_str());
                // Same internal entry point as the stream Completion
                // arm — classification, verdict, attempt-row append,
                // status persist, realisations, SLA samples all happen
                // exactly as they do for stream-reported outcomes. The
                // drv is addressed by the attempt's own derivation (the
                // exec_id is the key; the report's drv_path is not
                // trusted to re-route it).
                self.handle_completion(
                    &executor_id,
                    &attempt.drv_path,
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
}
