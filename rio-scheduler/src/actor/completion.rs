//! Completion handling: worker reports build done → update DAG, cascade, emit events.

use super::*;

impl DagActor {
    // -----------------------------------------------------------------------
    // ProcessCompletion
    // -----------------------------------------------------------------------

    #[instrument(skip(self, result), fields(worker_id = %worker_id, drv_key = %drv_key))]
    pub(super) async fn handle_completion(
        &mut self,
        worker_id: &str,
        // gRPC layer passes CompletionReport.drv_path; may be a drv_hash in tests.
        drv_key: &str,
        result: rio_proto::types::BuildResult,
        // CompletionReport resource fields. 0 = worker had no signal
        // (proc gone, build failed). Converted to None before the DB
        // write so the EMA isn't dragged toward zero by non-data.
        peak_memory_bytes: u64,
        output_size_bytes: u64,
    ) {
        let status =
            rio_proto::types::BuildResultStatus::try_from(result.status).unwrap_or_else(|_| {
                tracing::warn!(
                    raw_status = result.status,
                    "unknown BuildResultStatus from worker, treating as Unspecified"
                );
                rio_proto::types::BuildResultStatus::Unspecified
            });

        // Resolve drv_key (which may be a drv_path or a drv_hash) to drv_hash.
        let resolved_hash: DrvHash;
        let drv_hash: &str = if self.dag.contains(drv_key) {
            drv_key
        } else if let Some(h) = self.drv_path_to_hash(drv_key) {
            resolved_hash = h;
            &resolved_hash
        } else {
            warn!(key = drv_key, "completion for unknown derivation, ignoring");
            return;
        };

        // Find the derivation in the DAG
        let Some(state) = self.dag.node(drv_hash) else {
            warn!(drv_hash, "completion for unknown derivation, ignoring");
            return;
        };
        let current_status = state.status();

        // Idempotency: completed -> completed is a no-op
        if current_status == DerivationStatus::Completed {
            debug!(drv_hash, "duplicate completion report, ignoring");
            return;
        }

        // Only process completions for assigned/running derivations
        if !matches!(
            current_status,
            DerivationStatus::Assigned | DerivationStatus::Running
        ) {
            warn!(
                drv_hash,
                current_status = %current_status,
                "completion for derivation not in assigned/running state, ignoring"
            );
            return;
        }

        match status {
            rio_proto::types::BuildResultStatus::Built
            | rio_proto::types::BuildResultStatus::Substituted
            | rio_proto::types::BuildResultStatus::AlreadyValid => {
                self.handle_success_completion(
                    drv_hash,
                    &result,
                    worker_id,
                    peak_memory_bytes,
                    output_size_bytes,
                )
                .await;
            }
            rio_proto::types::BuildResultStatus::TransientFailure
            | rio_proto::types::BuildResultStatus::InfrastructureFailure => {
                self.handle_transient_failure(drv_hash, worker_id).await;
            }
            rio_proto::types::BuildResultStatus::PermanentFailure
            | rio_proto::types::BuildResultStatus::CachedFailure
            | rio_proto::types::BuildResultStatus::DependencyFailed
            | rio_proto::types::BuildResultStatus::LogLimitExceeded
            | rio_proto::types::BuildResultStatus::OutputRejected => {
                self.handle_permanent_failure(drv_hash, &result.error_msg, worker_id)
                    .await;
            }
            rio_proto::types::BuildResultStatus::Unspecified => {
                warn!(
                    drv_hash,
                    status = result.status,
                    "unknown build result status, treating as transient failure"
                );
                self.handle_transient_failure(drv_hash, worker_id).await;
            }
        }

        // Free worker capacity
        if let Some(worker) = self.workers.get_mut(worker_id) {
            worker.running_builds.remove(drv_hash);
        }

        // Dispatch newly ready derivations
        self.dispatch_ready().await;
    }

    pub(super) async fn handle_success_completion(
        &mut self,
        drv_hash: &str,
        result: &rio_proto::types::BuildResult,
        worker_id: &str,
        peak_memory_bytes: u64,
        output_size_bytes: u64,
    ) {
        // Transition to completed
        if let Some(state) = self.dag.node_mut(drv_hash) {
            // Ensure we're in running state first
            if state.status() == DerivationStatus::Assigned
                && let Err(e) = state.transition(DerivationStatus::Running)
            {
                warn!(drv_hash, error = %e, "Assigned->Running transition failed");
            }
            if let Err(e) = state.transition(DerivationStatus::Completed) {
                // Worker reported success but the in-memory state machine rejected
                // the transition (e.g., derivation was cascaded to DependencyFailed
                // or reset by heartbeat reconciliation in a race). The build result
                // is lost; downstream derivations will never be released.
                error!(
                    drv_hash,
                    worker_id,
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

        // Update DB
        if let Err(e) = self
            .db
            .update_derivation_status(drv_hash, DerivationStatus::Completed, None)
            .await
        {
            error!(drv_hash, error = %e, "failed to update derivation status in DB");
        }

        // Update assignment (non-terminal progress write: log failure, don't block)
        if let Some(state) = self.dag.node(drv_hash)
            && let Some(db_id) = state.db_id
            && let Err(e) = self
                .db
                .update_assignment_status(db_id, crate::db::AssignmentStatus::Completed)
                .await
        {
            error!(drv_hash, error = %e, "failed to persist assignment completion");
        }

        // Async: update build history EMA (best-effort statistics)
        if let Some(state) = self.dag.node(drv_hash)
            && let Some(pname) = &state.pname
            && let (Some(start), Some(stop)) = (&result.start_time, &result.stop_time)
        {
            let duration_secs = stop.seconds.saturating_sub(start.seconds) as f64
                + stop.nanos.saturating_sub(start.nanos) as f64 / 1_000_000_000.0;
            // Sanity bound: reject durations > 30 days (bogus worker timestamps)
            if duration_secs > 0.0 && duration_secs < 30.0 * 86400.0 {
                // 0 → None: "no signal" must not drag the EMA toward
                // zero. The worker explicitly uses 0 for proc-gone /
                // early-fail paths. A real build that uses literally
                // zero bytes of RSS doesn't exist (nix-daemon alone
                // is ~10MB), so this mapping loses nothing.
                let peak_mem = (peak_memory_bytes > 0).then_some(peak_memory_bytes);
                let out_size = (output_size_bytes > 0).then_some(output_size_bytes);
                if let Err(e) = self
                    .db
                    .update_build_history(pname, &state.system, duration_secs, peak_mem, out_size)
                    .await
                {
                    error!(drv_hash, error = %e, "failed to update build history EMA");
                }

                // Misclassification: routed to "small" but ran like
                // "large"? Threshold is 2× the assigned class's cutoff —
                // a build routed to a 30s class that took 61s triggers.
                // 2× is generous enough to avoid noise from normal
                // variance (build caches, CPU contention) while still
                // catching the "this is clearly not a small build" case.
                //
                // Penalty: overwrite EMA with actual (not blend). Next
                // classify() sees the real duration and picks right.
                // Harsh but self-correcting — a fluke gets blended back
                // down by the next normal completion.
                if let Some(assigned_class) = &state.assigned_size_class
                    && let Some(cutoff) =
                        crate::assignment::cutoff_for(assigned_class, &self.size_classes)
                    && duration_secs > 2.0 * cutoff
                {
                    warn!(
                        drv_hash,
                        pname = %pname,
                        assigned_class = %assigned_class,
                        cutoff_secs = cutoff,
                        actual_secs = duration_secs,
                        "misclassification: actual > 2× cutoff; penalty-writing EMA"
                    );
                    metrics::counter!("rio_scheduler_misclassifications_total").increment(1);
                    if let Err(e) = self
                        .db
                        .update_build_history_misclassified(pname, &state.system, duration_secs)
                        .await
                    {
                        error!(drv_hash, error = %e, "failed to write misclassification penalty");
                    }
                }
            }
        }

        // Emit derivation completed event
        let output_paths: Vec<String> = self
            .dag
            .node(drv_hash)
            .map(|s| s.output_paths.clone())
            .unwrap_or_default();

        let interested_builds = self.get_interested_builds(drv_hash);
        for build_id in &interested_builds {
            self.emit_build_event(
                *build_id,
                rio_proto::types::build_event::Event::Derivation(
                    rio_proto::types::DerivationEvent {
                        derivation_path: self.drv_hash_to_path(drv_hash).unwrap_or_else(|| {
                            warn!(
                                drv_hash,
                                "drv_hash_to_path returned None; using hash as fallback"
                            );
                            drv_hash.to_string()
                        }),
                        status: Some(rio_proto::types::derivation_event::Status::Completed(
                            rio_proto::types::DerivationCompleted {
                                output_paths: output_paths.clone(),
                            },
                        )),
                    },
                ),
            );
        }

        // Trigger log flush AFTER the Completed event has gone out. By the
        // time the gateway sees Completed, the ring buffer still has the full
        // log (flusher hasn't drained yet — it's async on a separate task).
        // So AdminService.GetBuildLogs can serve from the ring buffer in the
        // gap between Completed and the S3 upload landing.
        self.trigger_log_flush(drv_hash, interested_builds.clone());

        // Update ancestor priorities: this node is now terminal, so it
        // no longer contributes to its parents' max-child-priority.
        // Parents' priorities DROP. Propagates up until unchanged
        // (dirty-flag stop).
        //
        // Done BEFORE releasing downstream because the newly-ready
        // nodes will be pushed to the queue (D5: by priority), and
        // their priority is already correct from compute_initial at
        // merge time. The ancestors that change are NOT newly-ready
        // (they were already in the queue or beyond) — D5's lazy
        // invalidation handles re-pushing them if needed.
        //
        // Also emit the accuracy metric: how close was our estimate?
        // Histogram of actual/estimated. 1.0 = perfect, >1.0 = took
        // longer than expected. Helps tune the estimator.
        if let Some(state) = self.dag.node(drv_hash)
            && state.est_duration > 0.0
            && let (Some(start), Some(stop)) = (&result.start_time, &result.stop_time)
        {
            let actual_secs = stop.seconds.saturating_sub(start.seconds) as f64;
            if actual_secs > 0.0 {
                metrics::histogram!("rio_scheduler_critical_path_accuracy")
                    .record(actual_secs / state.est_duration);
            }
        }
        crate::critical_path::update_ancestors(&mut self.dag, drv_hash);

        // Release downstream: find newly ready derivations.
        // push_ready handles interactive boost via priority.
        let newly_ready = self.dag.find_newly_ready(drv_hash);
        for ready_hash in &newly_ready {
            if let Some(state) = self.dag.node_mut(ready_hash)
                && state.transition(DerivationStatus::Ready).is_ok()
            {
                if let Err(e) = self
                    .db
                    .update_derivation_status(ready_hash, DerivationStatus::Ready, None)
                    .await
                {
                    error!(drv_hash = %ready_hash, error = %e, "failed to update status");
                }
                self.push_ready(ready_hash.clone());
            }
        }

        // Update build completion status
        for build_id in &interested_builds {
            self.update_build_counts(*build_id);
            self.check_build_completion(*build_id).await;
        }
    }

    pub(super) async fn handle_transient_failure(&mut self, drv_hash: &str, worker_id: &str) {
        let should_retry = if let Some(state) = self.dag.node_mut(drv_hash) {
            state.failed_workers.insert(worker_id.into());

            // Check poison threshold
            if state.failed_workers.len() >= POISON_THRESHOLD {
                // Transition to poisoned
                if state.status() == DerivationStatus::Assigned
                    && let Err(e) = state.transition(DerivationStatus::Running)
                {
                    warn!(drv_hash, error = %e, "Assigned->Running transition failed");
                }
                if let Err(e) = state.transition(DerivationStatus::Poisoned) {
                    warn!(drv_hash, error = %e, "->Poisoned transition failed");
                }
                state.poisoned_at = Some(Instant::now());
                false
            } else if state.retry_count < self.retry_policy.max_retries {
                // Transition running -> failed
                if state.status() == DerivationStatus::Assigned
                    && let Err(e) = state.transition(DerivationStatus::Running)
                {
                    warn!(drv_hash, error = %e, "Assigned->Running transition failed");
                }
                if let Err(e) = state.transition(DerivationStatus::Failed) {
                    warn!(drv_hash, error = %e, "Running->Failed transition failed");
                }
                true
            } else {
                // Max retries exceeded: treat as permanent
                if state.status() == DerivationStatus::Assigned
                    && let Err(e) = state.transition(DerivationStatus::Running)
                {
                    warn!(drv_hash, error = %e, "Assigned->Running transition failed");
                }
                if let Err(e) = state.transition(DerivationStatus::Poisoned) {
                    warn!(drv_hash, error = %e, "->Poisoned transition failed");
                }
                state.poisoned_at = Some(Instant::now());
                false
            }
        } else {
            false
        };

        if should_retry {
            let retry_count = self.dag.node(drv_hash).map_or(0, |s| s.retry_count);

            // Schedule retry with backoff
            let backoff = self.retry_policy.backoff_duration(retry_count);
            let drv_hash_owned: DrvHash = drv_hash.into();

            if let Some(state) = self.dag.node_mut(&drv_hash_owned) {
                state.retry_count += 1;
                state.assigned_worker = None;
            }

            if let Err(e) = self
                .db
                .update_derivation_status(drv_hash, DerivationStatus::Failed, None)
                .await
            {
                error!(drv_hash, error = %e, "failed to persist Failed status");
            }
            if let Err(e) = self.db.increment_retry_count(drv_hash).await {
                error!(drv_hash, error = %e, "failed to persist retry increment");
            }

            // After backoff, transition failed -> ready
            // For simplicity in Phase 2a, we re-queue immediately with the backoff
            // duration logged. A full implementation would use a timer.
            debug!(
                drv_hash,
                retry_count = retry_count + 1,
                backoff_secs = backoff.as_secs_f64(),
                "scheduling retry after transient failure"
            );

            // TODO(phase3b): delayed re-queue using the computed backoff duration
            // (K8s-aware retry extends Phase 2a basic retry; see phase3b.md task 12).
            // Phase 2a re-queues immediately (see docs/src/phases/phase2a.md).
            if let Some(state) = self.dag.node_mut(&drv_hash_owned) {
                if let Err(e) = state.transition(DerivationStatus::Ready) {
                    warn!(drv_hash, error = %e, "Failed->Ready transition failed");
                } else {
                    self.push_ready(drv_hash_owned);
                }
            }
        } else {
            if let Err(e) = self
                .db
                .update_derivation_status(drv_hash, DerivationStatus::Poisoned, None)
                .await
            {
                error!(drv_hash, error = %e, "failed to persist Poisoned status");
            }

            // Cascade: parents of a poisoned derivation can never complete.
            // Transition them to DependencyFailed so keepGoing builds terminate.
            self.cascade_dependency_failure(drv_hash).await;

            // Propagate failure to interested builds
            let interested_builds = self.get_interested_builds(drv_hash);
            for build_id in interested_builds {
                self.handle_derivation_failure(build_id, drv_hash).await;
            }
        }
    }

    pub(super) async fn handle_permanent_failure(
        &mut self,
        drv_hash: &str,
        error_msg: &str,
        _worker_id: &str,
    ) {
        if let Some(state) = self.dag.node_mut(drv_hash) {
            if state.status() == DerivationStatus::Assigned
                && let Err(e) = state.transition(DerivationStatus::Running)
            {
                warn!(drv_hash, error = %e, "Assigned->Running transition failed");
            }
            // Permanent failure -> poisoned (no retry)
            if let Err(e) = state.transition(DerivationStatus::Poisoned) {
                warn!(drv_hash, error = %e, "->Poisoned transition failed");
            }
            state.poisoned_at = Some(Instant::now());
        }

        if let Err(e) = self
            .db
            .update_derivation_status(drv_hash, DerivationStatus::Poisoned, None)
            .await
        {
            error!(drv_hash, error = %e, "failed to persist Poisoned status");
        }

        // Cascade: parents of a poisoned derivation can never complete.
        self.cascade_dependency_failure(drv_hash).await;

        // Propagate failure to interested builds
        let interested_builds = self.get_interested_builds(drv_hash);

        // Flush logs for failed builds too — the failure's log is often the
        // most useful log (compile errors, test output). Do this BEFORE
        // handle_derivation_failure below, which may transition builds to
        // terminal and schedule cleanup.
        self.trigger_log_flush(drv_hash, interested_builds.clone());

        for build_id in interested_builds {
            // Emit failure event
            self.emit_build_event(
                build_id,
                rio_proto::types::build_event::Event::Derivation(
                    rio_proto::types::DerivationEvent {
                        derivation_path: self.drv_hash_to_path(drv_hash).unwrap_or_else(|| {
                            warn!(
                                drv_hash,
                                "drv_hash_to_path returned None; using hash as fallback"
                            );
                            drv_hash.to_string()
                        }),
                        status: Some(rio_proto::types::derivation_event::Status::Failed(
                            rio_proto::types::DerivationFailed {
                                error_message: error_msg.to_string(),
                                status: rio_proto::types::BuildResultStatus::PermanentFailure
                                    .into(),
                            },
                        )),
                    },
                ),
            );

            self.handle_derivation_failure(build_id, drv_hash).await;
        }
    }

    /// Transitively walk parents of a poisoned derivation and transition all
    /// Queued/Ready/Created ancestors to DependencyFailed.
    ///
    /// Without this, keepGoing builds with a poisoned leaf hang forever:
    /// parents stay Queued, so completed+failed never reaches total.
    pub(super) async fn cascade_dependency_failure(&mut self, poisoned_hash: &str) {
        let mut to_visit: Vec<DrvHash> = self.dag.get_parents(poisoned_hash);
        let mut visited: HashSet<DrvHash> = HashSet::new();

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

            // Remove from ready queue if present (Ready -> DependencyFailed).
            self.ready_queue.remove(&parent_hash);

            if let Err(e) = self
                .db
                .update_derivation_status(&parent_hash, DerivationStatus::DependencyFailed, None)
                .await
            {
                error!(drv_hash = %parent_hash, error = %e, "failed to persist DependencyFailed");
            }

            // Continue cascade: this parent's parents also cannot complete.
            to_visit.extend(self.dag.get_parents(&parent_hash));
        }
    }

    pub(super) async fn handle_derivation_failure(&mut self, build_id: Uuid, drv_hash: &str) {
        // Sync counts from DAG ground truth. The cascade may have transitioned
        // additional parents to DependencyFailed; those must be counted here.
        self.update_build_counts(build_id);

        let Some(build) = self.builds.get_mut(&build_id) else {
            return;
        };

        if !build.keep_going {
            // Fail the entire build immediately
            build.error_summary = Some(format!("derivation {drv_hash} failed"));
            build.failed_derivation = Some(drv_hash.to_string());
            if let Err(e) = self.transition_build_to_failed(build_id).await {
                error!(build_id = %build_id, error = %e, "failed to persist build-failed transition");
            }
        } else {
            // keepGoing: check if all derivations are resolved
            self.check_build_completion(build_id).await;
        }
    }
}
