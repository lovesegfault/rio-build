//! Build lifecycle: cancel, query status, watch events, completion transitions.
// r[impl sched.build.state]
// r[impl sched.build.keep-going]

use super::*;

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

impl DagActor {
    // -----------------------------------------------------------------------
    // CancelBuild
    // -----------------------------------------------------------------------

    /// Cancel all non-terminal derivations for a build and remove the
    /// build's interest from the DAG.
    ///
    /// Sends CancelSignal to workers for sole-interest Running/Assigned
    /// derivations, transitions them to Cancelled, removes build interest
    /// from the DAG, and prunes orphaned ready-queue entries. Does NOT
    /// transition the build state itself — caller decides Cancelled vs
    /// Failed. Extracted from [`handle_cancel_build`] so the per-build-
    /// timeout check in `handle_tick` (`sched.timeout.per-build` spec) can
    /// reuse the derivation-cancellation path but end in Failed.
    ///
    /// `signal_reason` is the CancelSignal.reason sent to workers.
    ///
    /// [`handle_cancel_build`]: Self::handle_cancel_build
    pub(super) async fn cancel_build_derivations(&mut self, build_id: Uuid, signal_reason: &str) {
        // Before removing interest, find derivations that are Running/
        // Assigned AND have THIS build as their ONLY interested build.
        // These need an active cancel — the worker is burning CPU on
        // them right now. Other derivations (sole-interest but not
        // yet dispatched, or shared with another build) are handled
        // by the existing orphan-removal / keep-running paths.
        //
        // Collect (drv_path, executor_id) for each such derivation.
        // drv_path (not drv_hash) because that's what CancelSignal
        // and the worker's cancel registry are keyed on.
        let to_cancel: Vec<(DrvHash, String, ExecutorId)> = self
            .dag
            .iter_nodes()
            .filter(|(_, s)| {
                matches!(
                    s.status(),
                    DerivationStatus::Assigned | DerivationStatus::Running
                ) && s.interested_builds.len() == 1
                    && s.interested_builds.contains(&build_id)
            })
            .filter_map(|(h, s)| {
                // assigned_executor should always be Some for Assigned/
                // Running, but be defensive. h is &str (iter_nodes
                // returns HashMap's &str keys) — .into() to DrvHash.
                s.assigned_executor
                    .as_ref()
                    .map(|w| (h.into(), s.drv_path().to_string(), w.clone()))
            })
            .collect();

        // Send CancelSignal + transition Cancelled. The worker's
        // cgroup.kill SIGKILLs the daemon tree → run_daemon_build
        // Errs → worker reports BuildResultStatus::Cancelled →
        // completion handler is a no-op (we already transitioned).
        //
        // try_send (not send): fire-and-forget. If the worker's
        // stream is full/closed, it's about to disconnect anyway
        // and reassign_derivations will handle it. The transition
        // to Cancelled still happens — scheduler-authoritative.
        //
        // PG writes are batched AFTER the loop (persist_status_batch
        // + unpin_best_effort_batch). The per-item variant caused an
        // N+1 actor stall: a 500-derivation cancel = ~1000 sequential
        // PG round-trips inside the single-threaded actor, blocking
        // heartbeats/completions/dispatch for the duration. Batching
        // collapses that to 2 round-trips regardless of N.
        let mut transitioned: Vec<&str> = Vec::with_capacity(to_cancel.len());
        for (drv_hash, drv_path, executor_id) in &to_cancel {
            // Transition FIRST. If it fails (state changed under
            // us — completion arrived between the collect above and
            // here), skip the signal — the build finished naturally.
            if let Some(state) = self.dag.node_mut(drv_hash) {
                if let Err(e) = state.transition(DerivationStatus::Cancelled) {
                    debug!(drv_hash = %drv_hash, error = %e,
                           "cancel transition failed (completion raced us), skipping signal");
                    continue;
                }
                state.assigned_executor = None;
            }
            transitioned.push(drv_hash.as_str());
            if let Some(worker) = self.executors.get(executor_id)
                && let Some(tx) = &worker.stream_tx
                && let Err(e) = tx.try_send(rio_proto::types::SchedulerMessage {
                    msg: Some(rio_proto::types::scheduler_message::Msg::Cancel(
                        rio_proto::types::CancelSignal {
                            drv_path: drv_path.clone(),
                            reason: signal_reason.to_string(),
                        },
                    )),
                })
            {
                debug!(executor_id = %executor_id, drv_hash = %drv_hash, error = %e,
                       "cancel signal dropped (stream full/closed)");
                metrics::counter!("rio_scheduler_cancel_signal_dropped_total").increment(1);
            }
            // Clear worker's running build — no longer counted against
            // capacity. They'll re-report it on next heartbeat but our
            // reconcile logic keeps scheduler-authoritative.
            if let Some(worker) = self.executors.get_mut(executor_id)
                && worker.running_build.as_ref() == Some(drv_hash)
            {
                worker.running_build = None;
            }
        }
        // Batch persist + unpin. fire-and-forget via db; completion
        // handler's no-op for Cancelled means no double-write.
        if !transitioned.is_empty() {
            self.persist_status_batch(&transitioned, DerivationStatus::Cancelled)
                .await;
            self.unpin_best_effort_batch(&transitioned).await;
        }
        if !to_cancel.is_empty() {
            info!(
                build_id = %build_id,
                count = to_cancel.len(),
                "sent CancelSignal to workers for sole-interest in-flight derivations"
            );
            metrics::counter!("rio_scheduler_cancel_signals_total")
                .increment(to_cancel.len() as u64);
        }

        // Sole-interest Queued/Ready/Created → DependencyFailed.
        // Without this, remove_build_interest orphans them (no
        // interested build → never dispatched → never terminal) but
        // they linger in the DAG with no accounting path. Shared
        // derivations (another build still cares) are left alone —
        // the other build will drive them.
        let to_depfail: Vec<DrvHash> = self
            .dag
            .iter_nodes()
            .filter(|(_, s)| {
                matches!(
                    s.status(),
                    DerivationStatus::Queued | DerivationStatus::Ready | DerivationStatus::Created
                ) && s.interested_builds.len() == 1
                    && s.interested_builds.contains(&build_id)
            })
            .map(|(h, _)| h.into())
            .collect();
        let mut depfailed: Vec<&str> = Vec::with_capacity(to_depfail.len());
        for drv_hash in &to_depfail {
            if let Some(state) = self.dag.node_mut(drv_hash)
                && state.transition(DerivationStatus::DependencyFailed).is_ok()
            {
                self.ready_queue.remove(drv_hash);
                depfailed.push(drv_hash.as_str());
            }
        }
        if !depfailed.is_empty() {
            self.persist_status_batch(&depfailed, DerivationStatus::DependencyFailed)
                .await;
        }

        // Remove build interest from derivations
        let orphaned = self.dag.remove_build_interest(build_id);

        // Remove orphaned derivations from the ready queue
        for hash in &orphaned {
            self.ready_queue.remove(hash);
        }
    }

    #[instrument(skip(self), fields(build_id = %build_id))]
    pub(super) async fn handle_cancel_build(
        &mut self,
        build_id: Uuid,
        reason: &str,
    ) -> Result<bool, ActorError> {
        let build = self
            .builds
            .get(&build_id)
            .ok_or(ActorError::BuildNotFound(build_id))?;

        if build.state().is_terminal() {
            return Ok(false);
        }

        self.cancel_build_derivations(build_id, &format!("build {build_id} cancelled: {reason}"))
            .await;

        // Transition build to cancelled. Already checked !is_terminal above
        // and we're the actor (single owner), so this should always succeed.
        // If it doesn't, skip the DB write to avoid state drift.
        if let Some(build) = self.builds.get_mut(&build_id)
            && let Err(e) = build.transition(BuildState::Cancelled)
        {
            // Should be unreachable (checked !is_terminal earlier).
            error!(build_id = %build_id, current = ?build.state(), error = %e,
                   "cancel transition rejected despite !is_terminal check; skipping DB write");
            return Ok(false);
        }

        self.db
            .update_build_status(build_id, BuildState::Cancelled, None)
            .await?;

        // Emit cancelled event
        self.emit_build_event(
            build_id,
            rio_proto::types::build_event::Event::Cancelled(rio_proto::types::BuildCancelled {
                reason: reason.to_string(),
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
    ) -> Result<rio_proto::types::BuildStatus, ActorError> {
        let build = self
            .builds
            .get(&build_id)
            .ok_or(ActorError::BuildNotFound(build_id))?;

        let summary = self.dag.build_summary(build_id);

        let proto_state = match build.state() {
            BuildState::Pending => rio_proto::types::BuildState::Pending,
            BuildState::Active => rio_proto::types::BuildState::Active,
            BuildState::Succeeded => rio_proto::types::BuildState::Succeeded,
            BuildState::Failed => rio_proto::types::BuildState::Failed,
            BuildState::Cancelled => rio_proto::types::BuildState::Cancelled,
        };

        Ok(rio_proto::types::BuildStatus {
            build_id: build_id.to_string(),
            state: proto_state.into(),
            total_derivations: summary.total,
            completed_derivations: summary.completed,
            cached_derivations: build.cached_count,
            running_derivations: summary.running,
            failed_derivations: summary.failed,
            queued_derivations: summary.queued,
            submitted_at: None,
            started_at: None,
            finished_at: None,
            error_summary: build.error_summary.clone().unwrap_or_default(),
            critical_path_remaining_secs: Some(summary.critpath_remaining.round() as u64),
            assigned_executors: summary.assigned_executors,
        })
    }

    pub(super) fn handle_watch_build(
        &self,
        build_id: Uuid,
    ) -> Result<(broadcast::Receiver<rio_proto::types::BuildEvent>, u64), ActorError> {
        let tx = self
            .build_events
            .get(&build_id)
            .ok_or(ActorError::BuildNotFound(build_id))?;

        // Subscribe FIRST so we receive anything sent after this point.
        // Then capture last_seq. The actor is single-threaded and
        // this fn is synchronous (no .await between subscribe and
        // sequence read) — no event can be emitted in between. So
        // last_seq is an exact watermark: everything ≤ last_seq was
        // emitted before subscribe (PG replay covers it; may also be
        // in the broadcast ring → gRPC dedups); everything > last_seq
        // was emitted after (guaranteed on broadcast, not in PG yet).
        let rx = tx.subscribe();
        let last_seq = self.build_sequences.get(&build_id).copied().unwrap_or(0);

        // If the build is already terminal, the BuildCompleted/Failed/Cancelled
        // event was already sent (possibly to zero receivers). A late subscriber
        // would never see it and would hang forever. Re-send a terminal event
        // so the new subscriber gets it.
        if let Some(build) = self.builds.get(&build_id)
            && build.state().is_terminal()
        {
            let terminal_event = match build.state() {
                BuildState::Succeeded => {
                    // Reconstruct output_paths from DAG roots (same as complete_build).
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
                    rio_proto::types::build_event::Event::Completed(
                        rio_proto::types::BuildCompleted { output_paths },
                    )
                }
                BuildState::Failed => {
                    rio_proto::types::build_event::Event::Failed(rio_proto::types::BuildFailed {
                        error_message: build.error_summary.clone().unwrap_or_default(),
                        failed_derivation: build.failed_derivation.clone().unwrap_or_default(),
                    })
                }
                BuildState::Cancelled => rio_proto::types::build_event::Event::Cancelled(
                    rio_proto::types::BuildCancelled {
                        reason: "build was cancelled".to_string(),
                    },
                ),
                // Non-terminal states handled by is_terminal() check above.
                _ => unreachable!("is_terminal() returned true for non-terminal state"),
            };

            // Sequence number: reuse the last one (approximate; the subscriber
            // only cares about receiving a terminal event, not exact sequencing).
            let seq = self.build_sequences.get(&build_id).copied().unwrap_or(0);
            let event = rio_proto::types::BuildEvent {
                build_id: build_id.to_string(),
                sequence: seq,
                timestamp: Some(prost_types::Timestamp::from(std::time::SystemTime::now())),
                event: Some(terminal_event),
            };
            // broadcast::send takes &self; Err means no receivers, but we just
            // subscribed so there's at least one.
            //
            // This re-send uses seq == last_seq. With PG replay
            // active, the gRPC bridge dedups seq ≤ last_seq from
            // broadcast — so this event is SKIPPED there. That's
            // fine: PG replay already delivered the real terminal
            // event (emit_build_event persisted it). This re-send is
            // now a safety net for when PG replay FAILS (store down)
            // — the bridge falls through to broadcast-only, and THIS
            // event is what the watcher sees.
            let _ = tx.send(event);
        }

        Ok((rx, last_seq))
    }

    pub(super) async fn update_build_counts(&mut self, build_id: Uuid) {
        let summary = self.dag.build_summary(build_id);
        let Some(build) = self.builds.get_mut(&build_id) else {
            return;
        };
        build.completed_count = summary.completed;
        build.failed_count = summary.failed;
        // I-103: persist denormalized counts so list_builds is O(LIMIT).
        // Best-effort — these are display columns; recovery re-runs this
        // for active builds, so a missed write self-heals on failover.
        let total = build.derivation_hashes.len() as u32;
        let cached = build.cached_count;
        if let Err(e) = self
            .db
            .persist_build_counts(build_id, total, summary.completed, cached)
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

        let all_completed = completed >= total;
        let all_resolved = (completed + failed) >= total;

        if all_completed && failed == 0 {
            if let Err(e) = self.complete_build(build_id).await {
                error!(build_id = %build_id, error = %e, "failed to persist build completion");
            }
        } else if failed > 0 && (all_resolved || !keep_going) {
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
            .transition_build(build_id, BuildState::Succeeded)
            .await?
            == TransitionOutcome::Rejected
        {
            debug!(build_id = %build_id, "complete_build: transition rejected (already terminal), skipping side effects");
            return Ok(());
        }

        // Collect output paths from root derivations
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

        self.emit_build_event(
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
        let (error_summary, failed_derivation) = self
            .builds
            .get(&build_id)
            .map(|b| {
                (
                    b.error_summary.clone().unwrap_or_default(),
                    b.failed_derivation.clone().unwrap_or_default(),
                )
            })
            .unwrap_or_default();

        // Skip side effects on Rejected (already terminal).
        if self.transition_build(build_id, BuildState::Failed).await? == TransitionOutcome::Rejected
        {
            debug!(build_id = %build_id, "transition_build_to_failed: rejected (already terminal), skipping side effects");
            return Ok(());
        }

        self.emit_build_event(
            build_id,
            rio_proto::types::build_event::Event::Failed(rio_proto::types::BuildFailed {
                error_message: error_summary,
                failed_derivation,
            }),
        );
        metrics::counter!("rio_scheduler_builds_total", "outcome" => "failure").increment(1);

        self.schedule_terminal_cleanup(build_id);
        Ok(())
    }

    /// Attempt to transition a build to a new state.
    ///
    /// Returns `Applied` if the transition succeeded (in-memory state
    /// machine + DB update both committed). Returns `Rejected` if the
    /// in-memory state machine rejected the transition (e.g., already
    /// terminal → Succeeded would double-complete).
    ///
    /// Callers (complete_build, transition_build_to_failed) check
    /// the outcome and skip side effects on Rejected — otherwise a
    /// double-complete or resurrected orphan build would emit a
    /// spurious BuildCompleted event (with empty output_paths) to
    /// the gateway.
    pub(super) async fn transition_build(
        &mut self,
        build_id: Uuid,
        new_state: BuildState,
    ) -> Result<TransitionOutcome, ActorError> {
        if let Some(build) = self.builds.get_mut(&build_id) {
            if let Err(e) = build.transition(new_state) {
                // Already terminal or otherwise invalid. Skip the DB write to
                // avoid in-memory/DB drift. Return Rejected so callers skip
                // side effects (events, metrics, cleanup).
                debug!(
                    build_id = %build_id,
                    from = ?build.state(),
                    to = ?new_state,
                    error = %e,
                    "build transition rejected; skipping DB update + side effects"
                );
                return Ok(TransitionOutcome::Rejected);
            }

            // Record build duration on terminal transition.
            if new_state.is_terminal() {
                let duration = build.submitted_at.elapsed();
                metrics::histogram!("rio_scheduler_build_duration_seconds")
                    .record(duration.as_secs_f64());
            }
        }

        let error_summary = self
            .builds
            .get(&build_id)
            .and_then(|b| b.error_summary.as_deref());

        self.db
            .update_build_status(build_id, new_state, error_summary)
            .await?;

        Ok(TransitionOutcome::Applied)
    }

    /// Schedule delayed cleanup of terminal build state. After
    /// TERMINAL_CLEANUP_DELAY, the build's entries in builds/build_events/
    /// build_sequences are removed and orphaned+terminal DAG nodes are reaped.
    ///
    /// The delay allows late WatchBuild subscribers to receive the terminal
    /// event before the broadcast sender is dropped.
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

    /// Handle terminal build cleanup: remove build from in-memory maps and
    /// reap orphaned+terminal DAG nodes.
    pub(super) fn handle_cleanup_terminal_build(&mut self, build_id: Uuid) {
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
        self.build_events.remove(&build_id);
        self.build_sequences.remove(&build_id);

        // Remove build interest from DAG and reap orphaned+terminal nodes.
        let reaped = self.dag.remove_build_interest_and_reap(build_id);
        if reaped > 0 {
            debug!(build_id = %build_id, reaped, "reaped orphaned terminal DAG nodes");
        }

        // GC the persisted event log. Fire-and-forget: this is
        // cleanup of replay state for a build that's already
        // terminal — if PG is down the rows just age out via
        // created_at (handle_tick runs a 24h sweep every 360 ticks).
        // Spawned (not awaited in the actor loop): a slow PG
        // doesn't stall the next command. Unlike emit_build_event's
        // persister (which needs FIFO ordering), DELETE ordering
        // doesn't matter — there are no writes for this build_id
        // anymore (build_sequences just removed above).
        //
        // Skip if no persister configured (tests without PG).
        if self.event_persist_tx.is_some() {
            let pool = self.db.pool().clone();
            tokio::spawn(async move {
                if let Err(e) = sqlx::query("DELETE FROM build_event_log WHERE build_id = $1")
                    .bind(build_id)
                    .execute(&pool)
                    .await
                {
                    debug!(build_id = %build_id, error = %e, "event-log GC failed (rows will age out)");
                }
            });
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

        let mut max_silent_time: u64 = 0;
        let mut build_timeout: u64 = 0;
        let mut build_cores: u64 = 0;

        // Helper: take the minimum of non-zero values; zero means "unset".
        fn min_nonzero(acc: u64, val: u64) -> u64 {
            match (acc, val) {
                (0, v) => v,
                (a, 0) => a,
                (a, v) => a.min(v),
            }
        }

        for build_id in &interested {
            if let Some(build) = self.builds.get(build_id) {
                max_silent_time = min_nonzero(max_silent_time, build.options.max_silent_time);
                build_timeout = min_nonzero(build_timeout, build.options.build_timeout);
                // For cores, take the max (more cores is more permissive).
                build_cores = build_cores.max(build.options.build_cores);
            }
        }

        rio_proto::types::BuildOptions {
            max_silent_time,
            build_timeout,
            build_cores,
        }
    }
}
