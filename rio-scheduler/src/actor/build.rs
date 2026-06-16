//! Build lifecycle: cancel, query status, watch events, completion transitions.
// r[impl sched.build.state]
// r[impl sched.build.keep-going]

use tracing::{debug, error, info, instrument, warn};
use uuid::Uuid;

use crate::state::{
    BuildState, BuildStateExt, DerivationStatus, DrvHash, FirstFailure, SettledBuild,
    SettledCounts, TerminalOutcome,
};

use super::{ActorCommand, ActorError, DagActor, TERMINAL_CLEANUP_DELAY};

/// Result of a [`DagActor::transition_build`] call.
///
/// `Applied` = state machine transition + DB update both committed.
/// `Rejected` = state machine rejected (e.g., already terminal).
/// Callers should skip side effects (events, metrics, cleanup) on
/// `Rejected` — the build is already in its final state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TransitionOutcome {
    Applied,
    Rejected,
}

/// Terminal intent for [`DagActor::transition_build`] — the chokepoint
/// captures everything the terminal state must serve. `Cancel` REQUIRES
/// its reason, so the only function able to mark a build Cancelled
/// cannot be called without one (merged_bug_302). Pending→Active is not
/// here: it goes through `BuildInfo::transition` on the merge/recovery
/// paths (transactional / rebuild), which cannot reach a terminal state.
#[derive(Debug, Clone)]
pub(super) enum BuildTransition {
    Succeed,
    Fail,
    Cancel { reason: String },
}

impl DagActor {
    // -----------------------------------------------------------------------
    // CancelBuild
    // -----------------------------------------------------------------------

    /// Cancel all non-terminal derivations for a build and remove the
    /// build's interest from the DAG.
    ///
    /// Transitions sole-interest in-flight derivations to Cancelled,
    /// removes build interest from the DAG, and revokes orphaned
    /// `Ready` claimability. Does NOT transition the build state itself
    /// — caller decides Cancelled vs Failed. Extracted from
    /// [`handle_cancel_build`] so the per-build-timeout check in
    /// `handle_tick` (`sched.timeout.per-build` spec) can reuse the
    /// derivation-cancellation path but end in Failed.
    ///
    /// The pod side of a pull-mode cancel is the controller's job: it
    /// observes the closed attempt and deletes the owning Job, whose
    /// SIGTERM aborts the build inside the AD5 grace — there is no
    /// scheduler→executor signal path. What closes the attempt is the
    /// terminal status persist below (`persist_status_batch`: the same
    /// statement closes the assignment rows), re-driven by the status
    /// outbox until durable if the first persist fails
    /// (`sched.attempt.cancel-close-driven`). `signal_reason` is
    /// recorded for diagnostics only.
    ///
    /// [`handle_cancel_build`]: Self::handle_cancel_build
    pub(super) async fn cancel_build_derivations(&mut self, build_id: Uuid, signal_reason: &str) {
        // Classify every sole-interest node in ONE iter_nodes() pass via
        // an exhaustive match on status(). Exhaustive (not `matches!`
        // allowlists) so adding a DerivationStatus variant is a compile
        // error here.
        //
        // Shared derivations (another build still cares) are left alone
        // — the other build drives them.
        let mut to_cancel: Vec<(DrvHash, String)> = Vec::new();
        let mut to_depfail: Vec<DrvHash> = Vec::new();
        for (h, s) in self.dag.iter_nodes() {
            if !(s.interested_builds.len() == 1 && s.interested_builds.contains(&build_id)) {
                continue;
            }
            match s.status() {
                // In-flight on a worker → transition Cancelled (the
                // controller's Job deletion aborts the pod).
                DerivationStatus::Assigned | DerivationStatus::Running => {
                    to_cancel.push((h.into(), s.drv_path().to_string()));
                }
                // Not yet dispatched → DependencyFailed.
                DerivationStatus::Queued | DerivationStatus::Ready | DerivationStatus::Created => {
                    to_depfail.push(h.into());
                }
                // Transient sub-second intermediate inside one
                // handle_transient_failure call (Running → Failed →
                // Ready). The single-threaded actor cannot observe it
                // here — no CancelBuild can interleave mid-handler.
                DerivationStatus::Failed => {}
                // Terminal — nothing to transition. Reap handles them.
                DerivationStatus::Completed
                | DerivationStatus::Poisoned
                | DerivationStatus::DependencyFailed
                | DerivationStatus::Cancelled
                | DerivationStatus::Skipped => {}
            }
        }

        // Transition Cancelled + stamp the cancelled execution. The pod
        // is stopped by the controller deleting its Job (the terminal
        // status persist below closes the open attempt — assignment
        // rows close in the same statement, outbox-backed on failure;
        // the deletion's SIGTERM cgroup-kills the build inside the AD5
        // grace) — the scheduler has no executor channel to signal.
        //
        // PG writes are batched AFTER the loop (persist_status_batch
        // + unpin_best_effort_batch). The per-item variant caused an
        // N+1 actor stall: a 500-derivation cancel = ~1000 sequential
        // PG round-trips inside the single-threaded actor, blocking
        // pulls/completions for the duration. Batching collapses that
        // to 2 round-trips regardless of N.
        let mut transitioned: Vec<&str> = Vec::with_capacity(to_cancel.len());
        for (drv_hash, _drv_path) in &to_cancel {
            // Transition FIRST. If it fails (state changed under
            // us — completion arrived between the collect above and
            // here), skip — the build finished naturally.
            if let Some(state) = self.dag.node_mut(drv_hash) {
                if let Err(e) = state.transition(DerivationStatus::Cancelled) {
                    debug!(drv_hash = %drv_hash, error = %e,
                           "cancel transition failed (completion raced us), skipping");
                    continue;
                }
                state.assigned_executor = None;
            }
            // Stamp the cancelled execution's drv_executions row +
            // bd.exec_id correlation. The worker's eventual report (if
            // any arrives before the pod dies) folds through the
            // late-report chokepoint at the completion intake
            // (`LateReportEffect`): a late Cancelled report gap-fills
            // this stamp's NULL count, and a late SUCCESS-class report
            // classifies `LateReportEffect::Register` — the completed
            // upload survives this cancel as registered evidence
            // (round-9 WO-S1-1, the signed Q1 invariant; cancellation
            // stops future work, it does not discard registrable
            // completed work). This remains the only place that can
            // stamp the cancelled exec's TERMINAL row — for any of
            // cancel_build_derivations' callers (user cancel, per-build
            // timeout, fail-fast, top-down substitute fail).
            // Sole-interest filter at collect time means `&[build_id]`
            // is the full interested set. Without this, every cancel of
            // a running drv leaves the `drv_executions` row at
            // status=NULL until the TTL sweep and bd.exec_id NULL
            // (dashboard shows the "approximate" banner for a log that
            // was streamed).
            // r[impl sched.merge.exec-correlation+8]
            // No CompletionReport exists YET on the cancel path →
            // final_line_count stamps NULL here; the executor's late
            // Cancelled report fills it through the COALESCE gap-fill
            // (merged_bug_294) so a fully-stored log can still seal.
            self.terminal_log_epilogue(drv_hash, "cancelled", &[build_id], None);
            transitioned.push(drv_hash.as_str());
        }
        if !to_cancel.is_empty() {
            info!(
                build_id = %build_id,
                count = to_cancel.len(),
                reason = signal_reason,
                "cancelled sole-interest in-flight derivations \
                 (the controller's Job deletion stops the pods)"
            );
        }

        // Batch persist + unpin. fire-and-forget via db; completion
        // handler's no-op for Cancelled means no double-write.
        if !transitioned.is_empty() {
            self.persist_status_batch(&transitioned, DerivationStatus::Cancelled)
                .await;
            self.unpin_best_effort_batch(&transitioned).await;
        }
        // Sole-interest Queued/Ready/Created → DependencyFailed.
        // Without this, remove_build_interest orphans them (no
        // interested build → never dispatched → never terminal) but
        // they linger in the DAG with no accounting path.
        let mut depfailed: Vec<&str> = Vec::with_capacity(to_depfail.len());
        for drv_hash in &to_depfail {
            if let Some(state) = self.dag.node_mut(drv_hash)
                && state.transition(DerivationStatus::DependencyFailed).is_ok()
            {
                depfailed.push(drv_hash.as_str());
            }
        }
        if !depfailed.is_empty() {
            self.persist_status_batch(&depfailed, DerivationStatus::DependencyFailed)
                .await;
        }

        // Remove build interest from derivations
        self.dag.remove_build_interest(build_id);
    }

    #[instrument(skip(self), fields(build_id = %build_id))]
    pub(super) async fn handle_cancel_build(
        &mut self,
        build_id: Uuid,
        caller_tenant: Option<Uuid>,
        reason: &str,
    ) -> Result<bool, ActorError> {
        let build = self
            .builds
            .get(&build_id)
            .ok_or(ActorError::BuildNotFound(build_id))?;
        // r[impl sched.tenant.authz+3]
        if caller_tenant.is_some() && build.tenant_id != caller_tenant {
            return Err(ActorError::PermissionDenied { build_id });
        }

        if build.state().is_terminal() {
            return Ok(false);
        }

        self.cancel_build_derivations(build_id, &format!("build {build_id} cancelled: {reason}"))
            .await;

        // Route through transition_build (the single chokepoint) so this
        // path picks up the build_duration_seconds histogram and the
        // DB-first ordering. Previously this open-coded transition + DB
        // write, which skipped the histogram (builds_total{cancelled}
        // diverged from histogram_count) and had the same in-mem-before-
        // DB ordering bug. Rejected is unreachable here (checked
        // !is_terminal above and we're the single-owner actor) but
        // handled for defence-in-depth.
        if self
            .transition_build(
                build_id,
                BuildTransition::Cancel {
                    reason: reason.to_string(),
                },
            )
            .await?
            == TransitionOutcome::Rejected
        {
            error!(build_id = %build_id,
                   "cancel transition rejected despite !is_terminal check");
            return Ok(false);
        }

        // Emit FROM the settled payload (single-emitter discipline) —
        // the snapshot serves the same reason string.
        let settled_reason = match self.builds.get(&build_id).and_then(|b| b.settled()) {
            Some(SettledBuild {
                outcome: TerminalOutcome::Cancelled { reason },
                ..
            }) => reason.clone(),
            _ => reason.to_string(),
        };
        self.events.emit(
            build_id,
            rio_proto::types::build_event::Event::Cancelled(rio_proto::types::BuildCancelled {
                reason: settled_reason,
            }),
        );

        info!(build_id = %build_id, reason, "build cancelled");
        metrics::counter!("rio_scheduler_builds_total", "outcome" => "cancelled").increment(1);
        self.schedule_terminal_cleanup(build_id);
        Ok(true)
    }

    // -----------------------------------------------------------------------
    // Query handlers
    // -----------------------------------------------------------------------

    pub(super) fn handle_query_build_status(
        &self,
        build_id: Uuid,
        caller_tenant: Option<Uuid>,
    ) -> Result<rio_proto::types::BuildStatus, ActorError> {
        let build = self
            .builds
            .get(&build_id)
            .ok_or(ActorError::BuildNotFound(build_id))?;
        // r[impl sched.tenant.authz+3]
        if caller_tenant.is_some() && build.tenant_id != caller_tenant {
            return Err(ActorError::PermissionDenied { build_id });
        }

        // Settled builds serve their captured payload — the live DAG is
        // shared and mutable, and a finished build's numbers must not
        // track it (merged_bug_097).
        if let Some(settled) = build.settled() {
            let error_summary = match &settled.outcome {
                TerminalOutcome::Failed(ff) => ff.summary.clone(),
                TerminalOutcome::Succeeded { .. } | TerminalOutcome::Cancelled { .. } => {
                    String::new()
                }
            };
            return Ok(rio_proto::types::BuildStatus {
                build_id: build_id.to_string(),
                state: build.state().into(),
                total_derivations: settled.counts.total,
                completed_derivations: settled.counts.completed,
                cached_derivations: settled.counts.cached,
                running_derivations: 0,
                failed_derivations: settled.counts.failed,
                queued_derivations: 0,
                submitted_at: None,
                started_at: None,
                finished_at: None,
                error_summary,
                critical_path_remaining_secs: Some(0),
                assigned_executors: Vec::new(),
            });
        }

        let summary = self.dag.build_summary(build_id);

        Ok(rio_proto::types::BuildStatus {
            build_id: build_id.to_string(),
            state: build.state().into(),
            // I-111: summary.{total,completed} are DAG-relative; after
            // recovery the DAG only holds non-terminal-at-recovery drvs.
            total_derivations: build.total_count,
            completed_derivations: build.recovered_completed + summary.completed,
            cached_derivations: build.cached_count,
            running_derivations: summary.running,
            failed_derivations: summary.failed,
            queued_derivations: summary.queued,
            submitted_at: None,
            started_at: None,
            finished_at: None,
            error_summary: build.error_summary().unwrap_or_default().to_string(),
            critical_path_remaining_secs: Some(summary.critpath_remaining.round() as u64),
            assigned_executors: summary.assigned_executors,
        })
    }

    // r[impl sched.watch.snapshot-first]
    pub(super) fn handle_watch_build(
        &self,
        build_id: Uuid,
        caller_tenant: Option<Uuid>,
    ) -> Result<
        (
            super::BuildEventReceivers,
            Box<rio_proto::types::BuildEvent>,
        ),
        ActorError,
    > {
        let build = self
            .builds
            .get(&build_id)
            .ok_or(ActorError::BuildNotFound(build_id))?;
        // r[impl sched.tenant.authz+3]
        if caller_tenant.is_some() && build.tenant_id != caller_tenant {
            return Err(ActorError::PermissionDenied { build_id });
        }

        // Subscribe FIRST, then compute the snapshot. The actor is
        // single-threaded and this fn is synchronous (no .await between
        // subscribe and snapshot), so no event can land in between: the
        // snapshot folds in every event emitted before this point, and
        // the receivers carry exactly the events emitted after it. This
        // adjacency is what makes snapshot-first attach gap-free without
        // sequence numbers, replay, or dedup.
        //
        // builds and events.channels are removed together
        // (handle_cleanup_terminal_build); subscribe() returning None
        // is defense-in-depth against maps drift.
        let rx = self
            .events
            .subscribe(build_id)
            .ok_or(ActorError::BuildNotFound(build_id))?;
        let snapshot = self.watch_snapshot(build_id, build);
        Ok((rx, Box::new(snapshot)))
    }

    /// Compute the [`rio_proto::types::BuildSnapshot`] first-message for a
    /// `WatchBuild` attach: the build's current state, absolute aggregate
    /// counts (same arithmetic as [`Self::handle_query_build_status`]),
    /// the per-drv running set (so the gateway re-attaches log tails),
    /// and — for terminal builds — the outcome payload that replaces the
    /// old terminal-event re-send.
    fn watch_snapshot(
        &self,
        build_id: Uuid,
        build: &crate::state::BuildInfo,
    ) -> rio_proto::types::BuildEvent {
        use rio_proto::types;

        // Settled builds serve their captured payload, byte-equal to
        // the live terminal emit and the persisted row. No live DAG
        // read happens on this branch — the running-set filter below is
        // unreachable for terminal builds, so a stale-Completed reset +
        // re-dispatch under a LATER build can neither resurrect entries
        // in this build's running set nor shrink its counts
        // (merged_bug_097).
        if let Some(settled) = build.settled() {
            // EXHAUSTIVE match — adding a TerminalOutcome variant is a
            // compile error here, not a silently-empty payload.
            let (output_paths, error_message, failed_derivation, failure_status, cancel_reason) =
                match &settled.outcome {
                    TerminalOutcome::Succeeded { output_paths } => (
                        output_paths.clone(),
                        String::new(),
                        String::new(),
                        0i32,
                        String::new(),
                    ),
                    TerminalOutcome::Failed(ff) => (
                        Vec::new(),
                        ff.summary.clone(),
                        ff.failed_drv.clone().unwrap_or_default(),
                        ff.status.map_or(0, |s| s as i32),
                        String::new(),
                    ),
                    TerminalOutcome::Cancelled { reason } => (
                        Vec::new(),
                        String::new(),
                        String::new(),
                        0i32,
                        reason.clone(),
                    ),
                };
            let snapshot = types::BuildSnapshot {
                state: build.state().into(),
                total_derivations: settled.counts.total,
                completed_derivations: settled.counts.completed,
                cached_derivations: settled.counts.cached,
                running_derivations: 0,
                failed_derivations: settled.counts.failed,
                queued_derivations: 0,
                critical_path_remaining_secs: Some(0),
                assigned_executors: Vec::new(),
                running: Vec::new(),
                output_paths,
                error_message,
                failed_derivation,
                failure_status,
                cancel_reason,
            };
            return rio_proto::types::BuildEvent {
                build_id: build_id.to_string(),
                timestamp: Some(prost_types::Timestamp::from(std::time::SystemTime::now())),
                event: Some(types::build_event::Event::Snapshot(snapshot)),
            };
        }

        let summary = self.dag.build_summary(build_id);

        // Per-drv running set: derivations currently executing for this
        // build, with the exec_id minted at dispatch. The gateway uses
        // this to re-create per-drv activities and re-attach log tails
        // for executions whose Started event it missed while detached.
        let mut running: Vec<types::RunningDerivation> = self
            .dag
            .iter_nodes()
            .filter(|(_, s)| s.interested_builds.contains(&build_id))
            .filter(|(_, s)| {
                matches!(
                    s.status(),
                    DerivationStatus::Assigned | DerivationStatus::Running
                )
            })
            .map(|(_, s)| types::RunningDerivation {
                derivation_path: s.drv_path().to_string(),
                exec_id: s.exec_id.map(|e| e.to_string()).unwrap_or_default(),
                // r[impl sched.pull.kinded-running-surface]
                // Wire projection of the open attempt's work class.
                // None (no open attempt — Assigned without a mint yet)
                // degrades to UNSPECIFIED == the build display, exactly
                // the pre-kind behavior.
                kind: match s.open_attempt_kind {
                    Some(crate::state::AttemptKind::Build) => types::AttemptKind::Build as i32,
                    Some(crate::state::AttemptKind::Materialization) => {
                        types::AttemptKind::Materialization as i32
                    }
                    None => types::AttemptKind::Unspecified as i32,
                },
            })
            .collect();
        // Deterministic wire order (iter_nodes is HashMap-ordered).
        running.sort_by(|a, b| a.derivation_path.cmp(&b.derivation_path));

        let snapshot = types::BuildSnapshot {
            state: build.state().into(),
            // I-111: summary.{total,completed} are DAG-relative; after
            // recovery the DAG only holds non-terminal-at-recovery drvs.
            // Absolute counts come from BuildInfo (same as
            // handle_query_build_status).
            total_derivations: build.total_count,
            completed_derivations: build.recovered_completed + summary.completed,
            cached_derivations: build.cached_count,
            running_derivations: summary.running,
            failed_derivations: summary.failed,
            queued_derivations: summary.queued,
            critical_path_remaining_secs: Some(summary.critpath_remaining.round() as u64),
            assigned_executors: summary.assigned_executors,
            running,
            // Active build: no terminal payload arms.
            output_paths: Vec::new(),
            error_message: String::new(),
            failed_derivation: String::new(),
            failure_status: 0,
            cancel_reason: String::new(),
        };

        rio_proto::types::BuildEvent {
            build_id: build_id.to_string(),
            timestamp: Some(prost_types::Timestamp::from(std::time::SystemTime::now())),
            event: Some(types::build_event::Event::Snapshot(snapshot)),
        }
    }

    pub(super) async fn update_build_counts(&mut self, build_id: Uuid) {
        let summary = self.dag.build_summary(build_id);
        self.update_build_counts_with(build_id, &summary).await;
    }

    /// `update_build_counts` with a precomputed summary — for callers
    /// that also `emit_progress_with` so the O(dag_nodes) `build_summary`
    /// scan runs once, not twice (I-140).
    pub(super) async fn update_build_counts_with(
        &mut self,
        build_id: Uuid,
        summary: &crate::dag::BuildSummary,
    ) {
        let Some(build) = self.builds.get_mut(&build_id) else {
            return;
        };
        // r[impl sched.build.terminal-status-settled+3]
        // A settled build's accounting is frozen at the terminal
        // transition — in memory AND in PG. Without this gate, a
        // dispatch-time store hit on a shared node (stale-Completed
        // reset under a later build) re-persists the terminal build's
        // counts from the mutated DAG: `builds.completed_drvs` shrinks
        // below total on a Succeeded row (merged_bug_097's PG leg).
        if build.settled().is_some() {
            return;
        }
        build.completed_count = summary.completed;
        build.failed_count = summary.failed;
        // I-103: persist denormalized counts so list_builds is O(LIMIT).
        // Best-effort — these are display columns; recovery re-runs this
        // for active builds, so a missed write self-heals on failover.
        // I-111: persist ABSOLUTE counts. After recovery the DAG only
        // holds drvs that were non-terminal at recovery, so
        // summary.completed and derivation_hashes.len() are relative to
        // that subset. total_count + recovered_completed are seeded from
        // PG at recovery (0-offset for fresh builds).
        let total = build.total_count;
        let completed = build.recovered_completed + summary.completed;
        let cached = build.cached_count;
        #[cfg(test)]
        self.test_counters
            .persist_build_counts_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Err(e) = self
            .db
            .persist_build_counts(build_id, total, completed, cached)
            .await
        {
            debug!(build_id = %build_id, error = %e,
                   "failed to persist build counts (best-effort)");
        }
    }

    pub(super) async fn check_build_completion(&mut self, build_id: Uuid) {
        let Some(b) = self.builds.get(&build_id) else {
            return;
        };
        if b.state().is_terminal() {
            return;
        }
        let keep_going = b.keep_going;
        let total = b.derivation_hashes.len() as u32;
        let completed = b.completed_count;
        let failed = b.failed_count;
        // Sticky had-failure flag: failed_count is recomputed from the live
        // DAG and drops to 0 if a poisoned node is removed (ClearPoison/TTL).
        // The first failure is recorded once and never cleared, so it is
        // the authoritative "this build had a failure" gate for keep_going.
        let had_failure = b.first_failure().is_some();

        let all_completed = completed >= total;
        let all_resolved = (completed + failed) >= total;

        if all_completed && failed == 0 && !had_failure {
            if let Err(e) = self.complete_build(build_id).await {
                error!(build_id = %build_id, error = %e, "failed to persist build completion");
            }
        } else if (failed > 0 || had_failure) && (all_resolved || !keep_going) {
            // keep_going=true: all derivations resolved but some failed.
            //
            // keep_going=false: live failures go through handle_derivation_failure
            // immediately (which calls transition_build_to_failed directly, never
            // this function). This branch catches RECOVERY: the post-recovery
            // sweep calls check_build_completion on a build whose only drv is
            // Poisoned in PG — failed=1, but handle_derivation_failure never
            // fires because recovery doesn't replay completion events. Without
            // this, keep_going=false (the default!) falls through and the build
            // hangs Active forever. Live operation never reaches this branch
            // for !keep_going: the build is already terminal by the time
            // check_build_completion is called (early return above).
            if let Err(e) = self.transition_build_to_failed(build_id).await {
                error!(build_id = %build_id, error = %e, "failed to persist build-failed transition");
            }
        }
    }

    pub(super) async fn complete_build(&mut self, build_id: Uuid) -> Result<(), ActorError> {
        // Skip all side effects if transition was rejected (already
        // terminal). Otherwise a double-complete would emit a spurious
        // BuildCompleted event + metric + cleanup schedule.
        if self
            .transition_build(build_id, BuildTransition::Succeed)
            .await?
            == TransitionOutcome::Rejected
        {
            debug!(build_id = %build_id, "complete_build: transition rejected (already terminal), skipping side effects");
            return Ok(());
        }

        // Emit FROM the settled payload — the same paths the snapshot
        // and the persisted row serve (single-emitter discipline).
        let output_paths = match self.builds.get(&build_id).and_then(|b| b.settled()) {
            Some(SettledBuild {
                outcome: TerminalOutcome::Succeeded { output_paths },
                ..
            }) => output_paths.clone(),
            _ => Vec::new(),
        };
        self.events.emit(
            build_id,
            rio_proto::types::build_event::Event::Completed(rio_proto::types::BuildCompleted {
                output_paths,
            }),
        );

        info!(build_id = %build_id, "build completed successfully");
        metrics::counter!("rio_scheduler_builds_total", "outcome" => "success").increment(1);
        self.schedule_terminal_cleanup(build_id);
        Ok(())
    }

    pub(super) async fn transition_build_to_failed(
        &mut self,
        build_id: Uuid,
    ) -> Result<(), ActorError> {
        // Skip side effects on Rejected (already terminal).
        if self
            .transition_build(build_id, BuildTransition::Fail)
            .await?
            == TransitionOutcome::Rejected
        {
            debug!(build_id = %build_id, "transition_build_to_failed: rejected (already terminal), skipping side effects");
            return Ok(());
        }

        // Emit FROM the settled payload (single-emitter discipline) —
        // byte-equal to what the snapshot and persisted row serve.
        let (error_message, failed_derivation, status) =
            match self.builds.get(&build_id).and_then(|b| b.settled()) {
                Some(SettledBuild {
                    outcome: TerminalOutcome::Failed(ff),
                    ..
                }) => (
                    ff.summary.clone(),
                    ff.failed_drv.clone().unwrap_or_default(),
                    ff.status.map_or(0, |s| s as i32),
                ),
                _ => Default::default(),
            };
        self.events.emit(
            build_id,
            rio_proto::types::build_event::Event::Failed(rio_proto::types::BuildFailed {
                error_message,
                failed_derivation,
                status,
            }),
        );
        metrics::counter!("rio_scheduler_builds_total", "outcome" => "failure").increment(1);

        self.schedule_terminal_cleanup(build_id);
        Ok(())
    }

    /// Attempt to transition a build per the intent.
    ///
    /// Returns `Applied` if the transition succeeded (in-memory state
    /// machine + DB update both committed). Returns `Rejected` if the
    /// in-memory state machine rejected the transition (e.g., already
    /// terminal → Succeed would double-complete).
    ///
    /// Terminal intents CAPTURE the settled payload here, BEFORE the DB
    /// write: counts from one `dag.build_summary` scan, the outcome arm
    /// from the intent (Succeed collects root output paths; Fail
    /// snapshots the recorded first failure, synthesizing a build-level
    /// one if none was recorded; Cancel takes the required reason).
    /// The payload is persisted, then installed in-memory — every later
    /// consumer (live terminal event, snapshot, query, persisted row)
    /// reads the SAME `SettledBuild`, never live state (merged_bug_097).
    ///
    /// Ordering is dry-run validate → DB write → in-memory mutate.
    /// A transient DB error therefore leaves in-memory unchanged
    /// (`is_terminal()` stays false), so a re-call retries cleanly —
    /// the retry recomputes the payload. `tick_recheck_stuck_completions`
    /// is the retry DRIVER — every other `check_build_completion` caller
    /// is event-driven, and after the last derivation completes there
    /// are no more events. The previous order (in-mem first)
    /// self-defeated retry: in-mem went terminal, the `?` propagated,
    /// every caller swallowed with `error!()`, and re-calling on an
    /// already-terminal build returns `Rejected` — gateway `WatchBuild`
    /// hung forever.
    ///
    /// Callers (complete_build, transition_build_to_failed,
    /// handle_cancel_build) check the outcome and skip side effects on
    /// Rejected — otherwise a double-complete or resurrected orphan
    /// build would emit a spurious BuildCompleted event (with empty
    /// output_paths) to the gateway.
    pub(super) async fn transition_build(
        &mut self,
        build_id: Uuid,
        intent: BuildTransition,
    ) -> Result<TransitionOutcome, ActorError> {
        let new_state = match &intent {
            BuildTransition::Succeed => BuildState::Succeeded,
            BuildTransition::Fail => BuildState::Failed,
            BuildTransition::Cancel { .. } => BuildState::Cancelled,
        };
        // Dry-run validate without mutating. validate_transition is the
        // exact predicate the post-DB install below uses, so it cannot
        // fail there.
        if let Some(b) = self.builds.get(&build_id)
            && let Err(e) = b.state().validate_transition(new_state)
        {
            debug!(
                build_id = %build_id,
                from = ?b.state(),
                to = ?new_state,
                error = %e,
                "build transition rejected; skipping DB update + side effects"
            );
            return Ok(TransitionOutcome::Rejected);
        }

        // Capture the settled payload BEFORE the DB write, from one
        // summary scan + the recorded first failure.
        let Some(build) = self.builds.get(&build_id) else {
            return Err(ActorError::BuildNotFound(build_id));
        };
        let summary = self.dag.build_summary(build_id);
        let counts = SettledCounts {
            total: build.total_count,
            // I-111: summary counts are DAG-relative; absolute =
            // recovered offset + live (same arithmetic the live
            // snapshot/query surfaces use).
            completed: build.recovered_completed + summary.completed,
            cached: build.cached_count,
            failed: summary.failed,
        };
        let outcome = match &intent {
            BuildTransition::Succeed => {
                // Collect output paths from root derivations NOW —
                // the DAG is shared and mutable; watchers must see
                // the paths as of the terminal transition.
                let roots = self.dag.find_roots(build_id);
                let output_paths: Vec<String> = roots
                    .iter()
                    .flat_map(|h| {
                        self.dag
                            .node(h)
                            .map(|s| s.output_paths.clone())
                            .unwrap_or_default()
                    })
                    .collect();
                TerminalOutcome::Succeeded { output_paths }
            }
            BuildTransition::Fail => {
                TerminalOutcome::Failed(build.first_failure().cloned().unwrap_or_else(|| {
                    // No per-drv failure recorded (recovery-
                    // synthesized paths) — a build-level failure
                    // with no spliced derivation.
                    FirstFailure {
                        summary: "build failed".to_string(),
                        failed_drv: None,
                        status: None,
                    }
                }))
            }
            BuildTransition::Cancel { reason } => TerminalOutcome::Cancelled {
                reason: reason.clone(),
            },
        };
        let settled = SettledBuild { counts, outcome };

        // DB first — if this fails, in-memory is unchanged and retry
        // remains possible (the retry recomputes the payload).
        self.db
            .update_build_status(build_id, new_state, Some(&settled))
            .await?;

        if let Some(build) = self.builds.get_mut(&build_id) {
            // Validated above; the actor is single-threaded and there is
            // no `.await` between the dry-run and here that could observe
            // a state change.
            let _ = build.transition_terminal(settled);
            let duration = build.submitted_at.elapsed();
            metrics::histogram!("rio_scheduler_build_duration_seconds")
                .record(duration.as_secs_f64());
        }

        // r[impl sched.materialize.pinning]
        // §5.3 release site (ii): a terminal build departs the live
        // interest join (the durable status above committed BEFORE this
        // runs, so the materialization_interest view already excludes
        // it). If it was the LAST live interested build of a derivation
        // with a resolved materialization job, that job's pins release
        // here. ALWAYS-ON — never flag-gated (PD-B17): flag-on-era pins
        // must release after a rollback to flag-off; flag-off with no
        // materialization pins this is one cheap self-scoping no-op
        // query per build-terminal event.
        if new_state.is_terminal() {
            self.release_materialization_pins_best_effort("build terminal")
                .await;
        }

        Ok(TransitionOutcome::Applied)
    }

    /// Schedule delayed cleanup of terminal build state. After
    /// TERMINAL_CLEANUP_DELAY, the build's entries in builds/build_events
    /// are removed and orphaned+terminal DAG nodes are reaped.
    ///
    /// The delay keeps the build resident so late WatchBuild subscribers
    /// can still attach and learn the outcome from their snapshot.
    ///
    /// No-op if `self_tx` is None (tests that use bare `run()`).
    fn schedule_terminal_cleanup(&self, build_id: Uuid) {
        let Some(weak_tx) = self.self_tx.clone() else {
            return;
        };
        rio_common::task::spawn_monitored("terminal-cleanup-timer", async move {
            tokio::time::sleep(TERMINAL_CLEANUP_DELAY).await;
            // Upgrade weak->strong at send time. If all handles dropped,
            // upgrade fails and cleanup is moot (actor is shutting down).
            // try_send: if channel is full, cleanup is dropped. Log + count so
            // sustained drops are visible (unbounded memory growth under load).
            if let Some(tx) = weak_tx.upgrade()
                && tx
                    .try_send(ActorCommand::CleanupTerminalBuild { build_id })
                    .is_err()
            {
                tracing::warn!(
                    build_id = %build_id,
                    "cleanup command dropped (channel full); build state will leak until next restart"
                );
                metrics::counter!("rio_scheduler_cleanup_dropped_total").increment(1);
            }
        });
    }

    /// Handle terminal build cleanup: remove build from in-memory maps,
    /// reap orphaned+terminal DAG nodes, and re-evaluate the surviving
    /// parents that just lost children to the reap.
    pub(super) async fn handle_cleanup_terminal_build(&mut self, build_id: Uuid) {
        // Only clean up if build is actually terminal (guard against misdirected
        // cleanup, e.g., if build_id was reused, though UUIDs make this unlikely).
        let is_terminal = self
            .builds
            .get(&build_id)
            .map(|b| b.state().is_terminal())
            .unwrap_or(true); // already removed = fine
        if !is_terminal {
            warn!(build_id = %build_id, "cleanup scheduled for non-terminal build, skipping");
            return;
        }

        self.builds.remove(&build_id);
        self.events.remove(build_id);

        // Remove build interest from DAG and reap orphaned+terminal nodes.
        let reap = self.dag.remove_build_interest_and_reap(build_id);
        if !reap.reaped_paths.is_empty() {
            debug!(build_id = %build_id, reaped = reap.reaped_paths.len(), "reaped orphaned terminal DAG nodes");
        }

        // r[impl sched.merge.substitute-topdown+13]
        // Re-evaluate the surviving parents that just lost children to this
        // reap, via the shared removal-survivor loop
        // (`reevaluate_removal_survivors` — promotion for
        // now-vacuously-satisfied Queued survivors; job-armed ones are
        // skipped).
        // The poison-removal paths (admin ClearPoison, the poison-TTL
        // sweep) run the same loop at their own call sites: their removal
        // resets the child for a fresh re-merge AND must wake the
        // surviving parents — a parent the recovery condemnation spared on
        // co-ownership grounds waits Queued above the poisoned child, and
        // that child's removal is its only wake-up edge
        // (`sched.poison.clear-survivor-reevaluation`).
        //
        // r[impl sched.lease.standby-drops-writes+4]
        // Leader-gated (the standby-drops-writes discipline): the
        // rest of this handler stays ungated (in-memory build/
        // event-map removal and the DAG reap run on standby as before),
        // but the survivor re-evaluation performs leader-class writes
        // (`persist_status`) and `CleanupTerminalBuild` can be drained
        // by an ex-leader (the delayed cleanup timer posts it via
        // `self_tx` after lease loss). The new leader's recovery owns
        // these survivors.
        if self.leader.is_leader() {
            self.reevaluate_removal_survivors(&reap.surviving_parents)
                .await;
        }
    }

    /// Compute build options for a derivation from its interested builds.
    ///
    /// When multiple builds share a derivation, use the MOST RESTRICTIVE
    /// timeouts (min of non-zero values) so every interested build's
    /// constraints are satisfied. Zero means "unset" (no timeout).
    pub(super) fn build_options_for_derivation(
        &self,
        drv_hash: &DrvHash,
    ) -> rio_proto::types::BuildOptions {
        let interested = self.get_interested_builds(drv_hash);

        // The min-nonzero fold law lives ON the type now
        // (WireSecs::min_permissive, merged_bug_034): zero means
        // "unset" and loses to any set value. Operands are
        // ceiling-bounded by construction (the tenant-seam mint
        // saturates), so the fold output — min of bounded values, or
        // unset — cannot launder an unclamped value onto the wire.
        let mut max_silent_time = rio_common::clamped::WireSecs::UNSET;
        let mut build_timeout = rio_common::clamped::WireSecs::UNSET;
        // Option distinguishes "unseen" from "saw 0". Per
        // build_types.proto:307, build_cores=0 means "all" — the MOST
        // permissive value. `.max()` would treat it as least-permissive
        // (`max(0,4)=4` → a client requesting 0=all loses to any
        // positive value), inverting the "more permissive wins" intent.
        let mut build_cores: Option<u64> = None;

        for build_id in &interested {
            if let Some(build) = self.builds.get(build_id) {
                max_silent_time = max_silent_time.min_permissive(build.options.max_silent_time);
                build_timeout = build_timeout.min_permissive(build.options.build_timeout);
                // 0 = "all cores" (proto:307) — most permissive, sticky
                // once seen. Otherwise, max of positives.
                build_cores = Some(match (build_cores, build.options.build_cores) {
                    (None, v) => v,
                    (Some(0), _) | (_, 0) => 0,
                    (Some(a), v) => a.max(v),
                });
            }
        }

        rio_proto::types::BuildOptions {
            max_silent_time: max_silent_time.raw(),
            build_timeout: build_timeout.raw(),
            build_cores: build_cores.unwrap_or(0),
        }
    }
}
