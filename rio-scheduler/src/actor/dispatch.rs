//! Ready-queue dispatch: assign ready derivations to available workers.

use super::*;

impl DagActor {
    // -----------------------------------------------------------------------
    // Dispatch
    // -----------------------------------------------------------------------

    /// Dispatch ready derivations to available workers (FIFO).
    #[allow(clippy::while_let_loop)]
    pub(super) async fn dispatch_ready(&mut self) {
        // Drain the queue, dispatching eligible derivations and deferring
        // ineligible ones. Previously, `None => break` on the first ineligible
        // derivation blocked all subsequent work (e.g., an aarch64 drv at
        // queue head blocked all x86_64 dispatch).
        let mut deferred: Vec<String> = Vec::new();
        let mut dispatched_any = true;

        // Keep cycling until a full pass with no dispatches AND no stale removals.
        // In practice this terminates quickly: each derivation is either
        // dispatched, deferred, or removed (stale) exactly once per pass.
        while dispatched_any {
            dispatched_any = false;

            while let Some(drv_hash) = self.ready_queue.pop_front() {
                // Find the derivation's requirements
                let (system, required_features) = match self.dag.node(&drv_hash) {
                    Some(state) => (state.system.clone(), state.required_features.clone()),
                    None => continue, // stale entry, drop
                };

                // Only dispatch if derivation is actually in Ready state
                if self
                    .dag
                    .node(&drv_hash)
                    .is_none_or(|s| s.status() != DerivationStatus::Ready)
                {
                    continue; // stale state, drop
                }

                // Find first eligible worker with capacity (FIFO - no scoring)
                let eligible_worker = self
                    .workers
                    .values()
                    .find(|w| w.has_capacity() && w.can_build(&system, &required_features))
                    .map(|w| w.worker_id.clone());

                match eligible_worker {
                    Some(worker_id) => {
                        if self.assign_to_worker(&drv_hash, &worker_id).await {
                            dispatched_any = true;
                        } else {
                            // Assignment send failed (worker stream full or
                            // disconnected). Defer — retrying immediately in
                            // the same pass would spin: the channel won't
                            // drain until we yield to the runtime.
                            deferred.push(drv_hash);
                        }
                    }
                    None => {
                        // No eligible worker for this derivation. Defer and
                        // continue scanning for others we CAN dispatch.
                        deferred.push(drv_hash);
                    }
                }
            }

            // Re-queue deferred derivations at the front, preserving order.
            for hash in deferred.drain(..).rev() {
                self.ready_queue.push_front(hash);
            }
        }
    }

    /// Transition a derivation to Assigned and send it to the worker.
    /// Returns `true` if the assignment was sent, `false` if it failed
    /// (caller should defer the derivation, not retry immediately).
    pub(super) async fn assign_to_worker(&mut self, drv_hash: &str, worker_id: &str) -> bool {
        // Transition ready -> assigned. Do this FIRST (before recording latency
        // or clearing ready_at) so a rejected transition doesn't pollute metrics.
        if let Some(state) = self.dag.node_mut(drv_hash) {
            if let Err(e) = state.transition(DerivationStatus::Assigned) {
                // Not in Ready state (TOCTOU vs. the dispatch_ready pre-check).
                // Caller will defer; the next dispatch pass drops it via the
                // status != Ready guard. Log so operators can spot races.
                warn!(
                    drv_hash,
                    worker_id,
                    current = ?state.status(),
                    error = %e,
                    "Ready->Assigned transition rejected in assign_to_worker (TOCTOU)"
                );
                metrics::counter!("rio_scheduler_transition_rejected_total", "to" => "assigned")
                    .increment(1);
                return false;
            }
            // Record assignment latency (Ready -> Assigned time) after transitioning
            if let Some(ready_at) = state.ready_at.take() {
                let latency = ready_at.elapsed();
                metrics::histogram!("rio_scheduler_assignment_latency_seconds")
                    .record(latency.as_secs_f64());
            }
            state.assigned_worker = Some(worker_id.to_string());
        }

        // Update DB (non-terminal: log failure, don't block dispatch)
        if let Err(e) = self
            .db
            .update_derivation_status(drv_hash, DerivationStatus::Assigned, Some(worker_id))
            .await
        {
            error!(drv_hash, worker_id, error = %e, "failed to persist Assigned status");
        }

        // Create assignment in DB
        if let Some(state) = self.dag.node(drv_hash)
            && let Some(db_id) = state.db_id
            && let Err(e) = self
                .db
                .insert_assignment(db_id, worker_id, self.generation)
                .await
        {
            error!(drv_hash, worker_id, error = %e, "failed to insert assignment record");
        }

        // Track on worker
        if let Some(worker) = self.workers.get_mut(worker_id) {
            worker.running_builds.insert(drv_hash.to_string());
        }

        // Send WorkAssignment to worker via stream
        if let Some(state) = self.dag.node(drv_hash) {
            let assignment = rio_proto::types::WorkAssignment {
                drv_path: state.drv_path().to_string(),
                // TODO(phase2c): inline .drv content to avoid worker->store round-trip.
                // Phase 2a: worker fetches via GetPath (see rio-worker/src/executor.rs).
                drv_content: Vec::new(),
                // TODO(phase3a): compute closure scheduler-side for prefetch hints.
                // Phase 2a: worker computes via QueryPathInfo BFS.
                input_paths: Vec::new(),
                output_names: state.output_names.clone(),
                build_options: Some(self.build_options_for_derivation(drv_hash)),
                assignment_token: format!("{}-{}-{}", worker_id, drv_hash, self.generation),
                generation: self.generation as u64,
                is_fixed_output: state.is_fixed_output,
            };

            let msg = rio_proto::types::SchedulerMessage {
                msg: Some(rio_proto::types::scheduler_message::Msg::Assignment(
                    assignment,
                )),
            };

            if let Some(worker) = self.workers.get(worker_id)
                && let Some(tx) = &worker.stream_tx
                && let Err(e) = tx.try_send(msg)
            {
                warn!(
                    worker_id,
                    drv_hash,
                    error = %e,
                    "failed to send assignment to worker"
                );
                // Clean up worker tracking (we added drv_hash above;
                // without this, the worker appears to have this derivation
                // running, causing a phantom capacity leak).
                if let Some(worker) = self.workers.get_mut(worker_id) {
                    worker.running_builds.remove(drv_hash);
                }
                // Reset state: Assigned -> Ready. Caller (dispatch_ready)
                // will defer the derivation; next dispatch pass retries.
                // Do NOT push_front here — that would cause the inner
                // dispatch loop to spin (channel is still full).
                if let Some(state) = self.dag.node_mut(drv_hash)
                    && let Err(e) = state.reset_to_ready()
                {
                    // We already transitioned to Assigned, cleared running_builds,
                    // and now can't reset. Derivation is orphaned in Assigned
                    // with no worker actually building. Heartbeat reconciliation
                    // may eventually catch this, but it's a visible hang until then.
                    error!(
                        drv_hash,
                        worker_id,
                        current = ?state.status(),
                        error = %e,
                        "reset_to_ready failed after assignment send failure; derivation orphaned in Assigned"
                    );
                    metrics::counter!("rio_scheduler_transition_rejected_total", "to" => "ready_reset")
                        .increment(1);
                }
                return false;
            }
        }

        // Emit derivation started event
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
                        status: Some(rio_proto::types::derivation_event::Status::Started(
                            rio_proto::types::DerivationStarted {
                                worker_id: worker_id.to_string(),
                            },
                        )),
                    },
                ),
            );
        }

        debug!(drv_hash, worker_id, "assigned derivation to worker");
        metrics::counter!("rio_scheduler_assignments_total").increment(1);
        true
    }
}
