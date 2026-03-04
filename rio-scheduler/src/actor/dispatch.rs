//! Ready-queue dispatch: assign ready derivations to available workers.

use super::*;

impl DagActor {
    // -----------------------------------------------------------------------
    // Dispatch
    // -----------------------------------------------------------------------

    /// Dispatch ready derivations to available workers (FIFO).
    pub(super) async fn dispatch_ready(&mut self) {
        // Drain the queue, dispatching eligible derivations and deferring
        // ineligible ones. Previously, `None => break` on the first ineligible
        // derivation blocked all subsequent work (e.g., an aarch64 drv at
        // queue head blocked all x86_64 dispatch).
        let mut deferred: Vec<DrvHash> = Vec::new();
        let mut dispatched_any = true;
        // Track how many derivations WANTED each class but got deferred.
        // Reported as a gauge at the end — operator signal for "class X
        // is bottlenecked, scale that pool."
        let mut class_deferred: HashMap<String, u64> = HashMap::new();

        // Keep cycling until a full pass with no dispatches AND no stale removals.
        // In practice this terminates quickly: each derivation is either
        // dispatched, deferred, or removed (stale) exactly once per pass.
        while dispatched_any {
            dispatched_any = false;

            while let Some(drv_hash) = self.ready_queue.pop() {
                // Stale-entry guards: drop if not in DAG or not Ready.
                let Some(state) = self.dag.node(&drv_hash) else {
                    continue;
                };
                if state.status() != DerivationStatus::Ready {
                    continue;
                }

                // Classify by estimated duration + memory. None if
                // size_classes unconfigured (optional feature off —
                // no filter, all workers candidates).
                let target_class = crate::assignment::classify(
                    state.est_duration,
                    self.estimator
                        .peak_memory(state.pname.as_deref(), &state.system),
                    &self.size_classes,
                );

                // Try target class first, then overflow to larger
                // classes if no worker in target has capacity. A
                // "small" build CAN go to a "large" worker (just
                // wasteful); a "large" build CANNOT go to "small"
                // (would under-provision). So overflow walks UP only.
                let (eligible_worker, chosen_class) =
                    self.find_worker_with_overflow(&drv_hash, target_class.as_deref());

                match eligible_worker {
                    Some(worker_id) => {
                        // Record what class we ACTUALLY routed to (may
                        // be larger than target if we overflowed).
                        // Misclassification detector reads this at
                        // completion time.
                        if let Some(state) = self.dag.node_mut(&drv_hash) {
                            state.assigned_size_class = chosen_class.clone();
                        }
                        if let Some(class) = &chosen_class {
                            metrics::counter!(
                                "rio_scheduler_size_class_assignments_total",
                                "class" => class.clone()
                            )
                            .increment(1);
                        }

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
                        // No eligible worker (even with overflow).
                        // Defer and track by TARGET class (not chosen —
                        // chosen is None when there's no eligible).
                        if let Some(class) = target_class {
                            *class_deferred.entry(class).or_insert(0) += 1;
                        }
                        deferred.push(drv_hash);
                    }
                }
            }

            // Re-queue deferred derivations. push_ready recomputes their
            // priority (unchanged since we just popped them), so they
            // slot back into the same position. The old "push_front to
            // preserve order" doesn't apply — priority IS the order.
            for hash in std::mem::take(&mut deferred) {
                self.push_ready(hash);
            }
        }

        // Gauge: per-class deferral count. Snapshot from this dispatch
        // pass. Fire-and-forget — next dispatch overwrites. An operator
        // seeing rio_scheduler_class_queue_depth{class="large"}=50 knows
        // large workers are the bottleneck.
        //
        // Classes with zero deferred aren't reported (the map only has
        // entries we incremented). That's fine — absence of the metric
        // label means "not bottlenecked," which is the right signal.
        for (class, count) in class_deferred {
            metrics::gauge!("rio_scheduler_class_queue_depth", "class" => class).set(count as f64);
        }
    }

    /// Find a worker for this derivation, starting at `target_class` and
    /// overflowing to progressively larger classes if needed.
    ///
    /// Returns `(worker_id, class_actually_used)`. Both None if nobody
    /// can take it (wrong system, all full, no workers).
    ///
    /// Overflow direction: small → large only. A slow build on a small
    /// worker would dominate that worker's single slot; a fast build on
    /// a large worker is just slightly wasteful. scheduler.md:158.
    fn find_worker_with_overflow(
        &self,
        drv_hash: &DrvHash,
        target_class: Option<&str>,
    ) -> (Option<WorkerId>, Option<String>) {
        let Some(drv_state) = self.dag.node(drv_hash) else {
            return (None, None);
        };

        // No classification configured → single best_worker call with
        // no filter. The fast path for pre-D7 deployments.
        let Some(target) = target_class else {
            let w = crate::assignment::best_worker(&self.workers, drv_state, &self.dag, None);
            return (w, None);
        };

        // Build the overflow chain: target class, then all classes with
        // cutoff > target's cutoff, sorted ascending. If target cutoff
        // is 30s, chain is [small(30), medium(300), large(3600)].
        //
        // We don't cache this chain because classify() is called fresh
        // per-dispatch anyway (est_duration can change between ticks
        // via estimator refresh) and the sort is 2-4 elements.
        let target_cutoff = crate::assignment::cutoff_for(target, &self.size_classes);
        let mut chain: Vec<&str> = self
            .size_classes
            .iter()
            .filter(|c| {
                // Target itself (== cutoff) or larger.
                // target_cutoff=None shouldn't happen (target came
                // FROM classify which reads the same config) but be
                // defensive: if None, include everything.
                target_cutoff.is_none_or(|t| c.cutoff_secs >= t)
            })
            .map(|c| c.name.as_str())
            .collect();
        chain.sort_by(|a, b| {
            let ca = crate::assignment::cutoff_for(a, &self.size_classes).unwrap_or(f64::MAX);
            let cb = crate::assignment::cutoff_for(b, &self.size_classes).unwrap_or(f64::MAX);
            ca.total_cmp(&cb)
        });

        // Walk the chain: first class with an available worker wins.
        for class in chain {
            if let Some(w) =
                crate::assignment::best_worker(&self.workers, drv_state, &self.dag, Some(class))
            {
                return (Some(w), Some(class.to_string()));
            }
        }

        (None, None)
    }

    /// Transition a derivation to Assigned and send it to the worker.
    /// Returns `true` if the assignment was sent, `false` if it failed
    /// (caller should defer the derivation, not retry immediately).
    pub(super) async fn assign_to_worker(
        &mut self,
        drv_hash: &DrvHash,
        worker_id: &WorkerId,
    ) -> bool {
        // Transition ready -> assigned. Do this FIRST (before recording latency
        // or clearing ready_at) so a rejected transition doesn't pollute metrics.
        if let Some(state) = self.dag.node_mut(drv_hash) {
            if let Err(e) = state.transition(DerivationStatus::Assigned) {
                // Not in Ready state (TOCTOU vs. the dispatch_ready pre-check).
                // Caller will defer; the next dispatch pass drops it via the
                // status != Ready guard. Log so operators can spot races.
                warn!(
                    drv_hash = %drv_hash,
                    worker_id = %worker_id,
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
            state.assigned_worker = Some(worker_id.clone());
        }

        // Update DB (non-terminal: log failure, don't block dispatch)
        if let Err(e) = self
            .db
            .update_derivation_status(drv_hash, DerivationStatus::Assigned, Some(worker_id))
            .await
        {
            error!(drv_hash = %drv_hash, worker_id = %worker_id, error = %e, "failed to persist Assigned status");
        }

        // Create assignment in DB
        if let Some(state) = self.dag.node(drv_hash)
            && let Some(db_id) = state.db_id
            && let Err(e) = self
                .db
                .insert_assignment(db_id, worker_id, self.generation)
                .await
        {
            error!(drv_hash = %drv_hash, worker_id = %worker_id, error = %e, "failed to insert assignment record");
        }

        // Track on worker
        if let Some(worker) = self.workers.get_mut(worker_id) {
            worker.running_builds.insert(drv_hash.clone());
        }

        // Send WorkAssignment to worker via stream
        if let Some(state) = self.dag.node(drv_hash) {
            let assignment = rio_proto::types::WorkAssignment {
                drv_path: state.drv_path().to_string(),
                // Forward what the gateway inlined (or empty → worker
                // fetches from store). Gateway only inlines for nodes
                // whose outputs are MISSING (will-dispatch), so cache
                // hits don't bloat this. Worker already handles both
                // paths (executor/mod.rs:241 branches on is_empty).
                drv_content: state.drv_content.clone(),
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
                    worker_id = %worker_id,
                    drv_hash = %drv_hash,
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
                        drv_hash = %drv_hash,
                        worker_id = %worker_id,
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
                                drv_hash = %drv_hash,
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

        debug!(drv_hash = %drv_hash, worker_id = %worker_id, "assigned derivation to worker");
        metrics::counter!("rio_scheduler_assignments_total").increment(1);
        true
    }
}
