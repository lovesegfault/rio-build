//! Build lifecycle: cancel, query status, watch events, completion transitions.

use super::*;

impl DagActor {
    // -----------------------------------------------------------------------
    // CancelBuild
    // -----------------------------------------------------------------------

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

        // Remove build interest from derivations
        let orphaned = self.dag.remove_build_interest(build_id);

        // Remove orphaned derivations from the ready queue
        for hash in &orphaned {
            self.ready_queue.remove(hash);
        }

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
            // active (C5), the gRPC bridge dedups seq ≤ last_seq
            // from broadcast — so this event is SKIPPED there. That's
            // fine: PG replay already delivered the real terminal
            // event (emit_build_event persisted it). This re-send is
            // now a safety net for when PG replay FAILS (store down)
            // — the bridge falls through to broadcast-only, and THIS
            // event is what the watcher sees.
            let _ = tx.send(event);
        }

        Ok((rx, last_seq))
    }

    pub(super) fn update_build_counts(&mut self, build_id: Uuid) {
        let summary = self.dag.build_summary(build_id);
        if let Some(build) = self.builds.get_mut(&build_id) {
            build.completed_count = summary.completed;
            build.failed_count = summary.failed;
        }
    }

    pub(super) async fn check_build_completion(&mut self, build_id: Uuid) {
        let (state, keep_going, total, completed, failed) = match self.builds.get(&build_id) {
            Some(b) => (
                b.state(),
                b.keep_going,
                b.derivation_hashes.len() as u32,
                b.completed_count,
                b.failed_count,
            ),
            None => return,
        };

        if state.is_terminal() {
            return;
        }

        let all_completed = completed >= total;
        let all_resolved = (completed + failed) >= total;

        if all_completed && failed == 0 {
            if let Err(e) = self.complete_build(build_id).await {
                error!(build_id = %build_id, error = %e, "failed to persist build completion");
            }
        } else if keep_going && all_resolved && failed > 0 {
            // keepGoing: all derivations resolved but some failed
            if let Err(e) = self.transition_build_to_failed(build_id).await {
                error!(build_id = %build_id, error = %e, "failed to persist build-failed transition");
            }
        }
        // !keep_going failures are handled immediately in handle_derivation_failure
    }

    pub(super) async fn complete_build(&mut self, build_id: Uuid) -> Result<(), ActorError> {
        self.transition_build(build_id, BuildState::Succeeded)
            .await?;

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

        self.transition_build(build_id, BuildState::Failed).await?;

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

    pub(super) async fn transition_build(
        &mut self,
        build_id: Uuid,
        new_state: BuildState,
    ) -> Result<(), ActorError> {
        if let Some(build) = self.builds.get_mut(&build_id) {
            if let Err(e) = build.transition(new_state) {
                // Already terminal or otherwise invalid. Skip the DB write to
                // avoid in-memory/DB drift (previously: `let _` discarded the
                // error but DB was still overwritten with the rejected state).
                debug!(
                    build_id = %build_id,
                    from = ?build.state(),
                    to = ?new_state,
                    error = %e,
                    "build transition rejected; skipping DB update (already terminal)"
                );
                return Ok(());
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

        Ok(())
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
        // created_at (TODO(phase3b) time-based sweep on Tick).
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
            keep_going: false, // per-derivation, not per-build
        }
    }
}
