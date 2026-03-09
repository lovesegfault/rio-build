//! Ready-queue dispatch: assign ready derivations to available workers.
// r[impl sched.overflow.up-only]

use super::*;

impl DagActor {
    // -----------------------------------------------------------------------
    // Dispatch
    // -----------------------------------------------------------------------

    /// Dispatch ready derivations to available workers (FIFO).
    pub(super) async fn dispatch_ready(&mut self) {
        // Standby scheduler: merge DAGs (state warm for fast
        // takeover) but DON'T dispatch. The lease task flips this
        // on acquire/lose. Relaxed load: a one-pass lag either way
        // is harmless (see DagActor.is_leader field doc). In non-
        // K8s mode this is always true — no-op check.
        if !self.is_leader.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        // Also gate on recovery: don't dispatch until recover_from_
        // pg has rebuilt the DAG. Otherwise we'd dispatch from a
        // partial/empty DAG mid-recovery. Acquire pairs with
        // handle_leader_acquired's Release — sees all recovery
        // writes before proceeding (though actor is single-threaded
        // so this is belt-and-suspenders).
        if !self
            .recovery_complete
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }

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
                // Retry backoff: if set and not yet elapsed, defer.
                // The derivation stays Ready + in queue (re-pushed
                // at the end of the pass with the other deferred).
                // Next dispatch pass re-checks — convergent without
                // timers. Cheap: one Instant::now() only for
                // derivations that failed transiently (backoff_until
                // is None for fresh ones).
                if let Some(deadline) = state.backoff_until
                    && Instant::now() < deadline
                {
                    deferred.push(drv_hash);
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
        // Round 4 Z14: zero out ALL configured classes first. The
        // previous comment claimed "absence of the label means not
        // bottlenecked" — but gauges PERSIST in Prometheus until
        // overwritten. A class that was backed up (gauge=50) then
        // cleared (no entries in class_deferred this pass) would STAY
        // at 50 forever. Operators would see a phantom bottleneck.
        for sc in &self.size_classes {
            metrics::gauge!("rio_scheduler_class_queue_depth", "class" => sc.name.clone()).set(0.0);
        }
        // Now overwrite for classes that actually have deferrals.
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
    /// a large worker is just slightly wasteful. scheduler.md:178.
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
            // Clear retry-backoff deadline: we're dispatching, the
            // backoff has been honored (dispatch_ready wouldn't
            // have let us here otherwise). Next failure gets a
            // fresh computed backoff from the (incremented)
            // retry_count.
            state.backoff_until = None;
            state.assigned_worker = Some(worker_id.clone());
        }

        // Single atomic load. The lease task (C2) may fetch_add the
        // generation between the DB insert and the WorkAssignment send
        // below (there's an await in between). Without this snapshot,
        // dispatch.rs:229 and :258 would read DIFFERENT generations —
        // the PG row says "assigned under gen N" but the worker
        // receives "gen N+1." The worker then rejects its own
        // assignment as stale. Loading once and reusing closes the tear.
        //
        // Acquire pairs with the lease task's Release fetch_add. Sees
        // the generation AND any writes the lease task did before it
        // (is_leader=true, which dispatch_ready checked at loop top).
        let generation = self.generation.load(std::sync::atomic::Ordering::Acquire);

        // Update DB (non-terminal: log failure, don't block dispatch)
        self.persist_status(drv_hash, DerivationStatus::Assigned, Some(worker_id))
            .await;

        // Create assignment in DB. PG BIGINT is signed; cast at THIS
        // boundary, not at the proto-encode sites below. One cast
        // instead of two, and the proto sites are hotter (this PG
        // write is best-effort anyway — log+continue on error).
        if let Some(state) = self.dag.node(drv_hash)
            && let Some(db_id) = state.db_id
            && let Err(e) = self
                .db
                .insert_assignment(db_id, worker_id, generation as i64)
                .await
        {
            error!(drv_hash = %drv_hash, worker_id = %worker_id, error = %e, "failed to insert assignment record");
        }

        // Track on worker
        if let Some(worker) = self.workers.get_mut(worker_id) {
            worker.running_builds.insert(drv_hash.clone());
        }

        // X9 auto-pin: write input-closure paths to scheduler_live_
        // pins so GC's mark CTE protects them. Same closure
        // approximation as send_prefetch_hint and best_worker
        // scoring (approx_input_closure). Best-effort: PG failure
        // logs + continues; 24h grace period is the fallback.
        // Empty for leaf derivations → no-op.
        {
            let input_paths = crate::assignment::approx_input_closure(&self.dag, drv_hash);
            if !input_paths.is_empty()
                && let Err(e) = self.db.pin_live_inputs(drv_hash, &input_paths).await
            {
                debug!(drv_hash = %drv_hash, error = %e,
                       "failed to pin live inputs (best-effort; grace period is fallback)");
            }
        }

        // PrefetchHint BEFORE WorkAssignment: the worker starts
        // warming its FUSE cache while still parsing the .drv
        // (which it fetches or extracts from drv_content below).
        // A few seconds of head-start on a multi-minute fetch
        // is the win.
        //
        // Same closure approximation as best_worker's bloom
        // scoring (approx_input_closure) — if scoring said "w1
        // has most of these," the hint should be "the few w1
        // DOESN'T have." Consistent approximation = consistent
        // filtering.
        //
        // Best-effort: try_send, failure logs debug not warn
        // (hint not contract). If the channel is full (worker's
        // recv loop busy), the assignment that follows will
        // likely also fail → the reset_to_ready cleanup below
        // handles it. If only the HINT fails, the build still
        // works, just fetches on-demand via FUSE.
        self.send_prefetch_hint(worker_id, drv_hash);

        // Send WorkAssignment to worker via stream
        if let Some(state) = self.dag.node(drv_hash) {
            let build_opts = self.build_options_for_derivation(drv_hash);

            // Assignment token: HMAC-signed if configured, else
            // legacy format-string. The store verifies signed
            // tokens on PutPath (prevents arbitrary-path upload
            // from a compromised worker). Unsigned tokens are
            // accepted by a store with hmac_verifier=None (dev).
            //
            // Expiry: 2× build_timeout (or 2× daemon_timeout
            // default if timeout=0). A worker legitimately
            // uploading after completion is well within that
            // window. Prevents replay from a leaked token later.
            let assignment_token = if let Some(signer) = &self.hmac_signer {
                let timeout_secs = if build_opts.build_timeout > 0 {
                    build_opts.build_timeout
                } else {
                    // Match rio-worker's DEFAULT_DAEMON_TIMEOUT.
                    // Can't reference the const cross-crate, so
                    // duplicate the value. 7200s = 2h.
                    7200
                };
                // Clamp BEFORE saturating_mul: a client sending
                // build_timeout=u64::MAX would get saturating_mul
                // → u64::MAX → expiry_unix = u64::MAX = immortal
                // token. A leaked immortal token defeats the
                // replay-prevention purpose of expiry entirely.
                // 7 days max: well above any real build duration.
                const MAX_HMAC_TIMEOUT_SECS: u64 = 7 * 86400;
                let timeout_secs = timeout_secs.min(MAX_HMAC_TIMEOUT_SECS);
                let expiry_unix = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
                    .saturating_add(timeout_secs.saturating_mul(2));
                signer.sign(&rio_common::hmac::Claims {
                    worker_id: worker_id.to_string(),
                    drv_hash: drv_hash.to_string(),
                    expected_outputs: state.expected_output_paths.clone(),
                    expiry_unix,
                })
            } else {
                // Legacy unsigned: format-string. Store with
                // hmac_verifier=None accepts this.
                format!("{worker_id}-{drv_hash}-{generation}")
            };

            let assignment = rio_proto::types::WorkAssignment {
                drv_path: state.drv_path().to_string(),
                // Forward what the gateway inlined (or empty → worker
                // fetches from store). Gateway only inlines for nodes
                // whose outputs are MISSING (will-dispatch), so cache
                // hits don't bloat this. Worker already handles both
                // paths (executor/mod.rs:241 branches on is_empty).
                drv_content: state.drv_content.clone(),
                output_names: state.output_names.clone(),
                build_options: Some(build_opts),
                assignment_token,
                generation,
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

                // Round 4 Z15: also clean up PG state written above.
                // unpin: pin_live_inputs wrote scheduler_live_pins
                // rows. Without unpinning, they leak until terminal
                // cleanup (which never runs if this drv stays stuck).
                self.unpin_best_effort(drv_hash).await;
                // delete assignment: insert_assignment wrote a
                // 'pending' row. On recovery, this row is misleading
                // (the worker never got the assignment). log-on-fail:
                // this is best-effort cleanup.
                if let Some(state) = self.dag.node(drv_hash)
                    && let Some(db_id) = state.db_id
                    && let Err(e) = self.db.delete_latest_assignment(db_id).await
                {
                    warn!(drv_hash = %drv_hash, error = %e,
                          "delete_latest_assignment failed during try_send rollback");
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
                        derivation_path: self.drv_path_or_hash_fallback(drv_hash),
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

    /// Send a PrefetchHint for the chosen worker to warm its FUSE
    /// cache. Best-effort: `try_send`, failure logs at debug.
    ///
    /// Same closure approximation as [`best_worker`]'s scoring
    /// (both call `approx_input_closure`). Bloom-filtered against
    /// the worker's last heartbeat filter: skip paths the worker
    /// PROBABLY has. False positives (bloom says yes, worker
    /// doesn't have it) mean we skip a hint we should have sent
    /// — the build's FUSE op fetches it on-demand. False negatives
    /// are impossible (bloom never false-negative on a positive
    /// insert). So we only ever UNDER-hint, never OVER-hint.
    /// Under-hinting is a missed optimization; over-hinting would
    /// waste worker bandwidth on paths it already has.
    ///
    /// No bloom = send everything (pessimistic, same as scoring).
    /// Empty hint (everything filtered) = don't send the message
    /// at all (saves one try_send for the common case where
    /// best_worker picked a warm worker — that's the point of
    /// bloom-locality scoring).
    ///
    /// [`best_worker`]: crate::assignment::best_worker
    fn send_prefetch_hint(&self, worker_id: &WorkerId, drv_hash: &DrvHash) {
        let input_paths = crate::assignment::approx_input_closure(&self.dag, drv_hash);
        if input_paths.is_empty() {
            // Leaf derivation (no DAG children). Nothing to prefetch.
            // Common for .drv fetches and source tarballs.
            return;
        }

        // Bloom filter. Same logic as count_missing: skip paths the
        // worker claims to have. The filter is up to 10s stale
        // (heartbeat interval) but that's fine — a stale positive
        // means we skip hinting for a path the worker evicted
        // since last heartbeat; the build fetches it on-demand.
        let Some(worker) = self.workers.get(worker_id.as_str()) else {
            // Worker gone between best_worker and here? Actor is
            // single-threaded so this shouldn't happen, but be
            // defensive (the if-let Some(state) at the top of the
            // caller is the same kind of guard).
            return;
        };
        let to_prefetch: Vec<String> = match &worker.bloom {
            Some(bloom) => input_paths
                .into_iter()
                .filter(|p| !bloom.maybe_contains(p))
                .collect(),
            // No bloom = worker didn't send one. Pessimistic: send
            // all. Same "incentivize sending the filter" policy as
            // count_missing.
            None => input_paths,
        };

        // Cap: bound message size. A derivation with 200 deps ×
        // 3 outputs = 600 paths × ~80 bytes = 48 KB. Fine for gRPC
        // but let's not surprise anyone with a 1 MB hint for a
        // pathological case. 100 covers the 95th percentile; the
        // rest fetch on-demand (we cap by truncating, not by
        // "pick the best 100" — that would need per-path nar_size
        // which we don't have. Arbitrary 100 is better than
        // nothing).
        const MAX_PREFETCH_PATHS: usize = 100;
        let mut to_prefetch = to_prefetch;
        if to_prefetch.len() > MAX_PREFETCH_PATHS {
            to_prefetch.truncate(MAX_PREFETCH_PATHS);
        }

        if to_prefetch.is_empty() {
            // Everything filtered = best_worker picked a warm worker.
            // Exactly what bloom-locality scoring is FOR. No hint
            // message needed.
            return;
        }

        let hint_len = to_prefetch.len();
        let hint = rio_proto::types::PrefetchHint {
            store_paths: to_prefetch,
        };
        let msg = rio_proto::types::SchedulerMessage {
            msg: Some(rio_proto::types::scheduler_message::Msg::Prefetch(hint)),
        };

        // try_send: if the channel is full, drop the hint. The
        // assignment that follows uses the SAME channel — if it's
        // full, that assignment also fails and reset_to_ready cleans
        // up. If only this fails (race: channel had 1 slot, hint
        // lost, assignment fit), the build works without prefetch.
        // debug not warn: this is a hint, not a contract.
        if let Some(tx) = &worker.stream_tx {
            match tx.try_send(msg) {
                Ok(()) => {
                    metrics::counter!("rio_scheduler_prefetch_hints_sent_total").increment(1);
                    metrics::counter!("rio_scheduler_prefetch_paths_sent_total")
                        .increment(hint_len as u64);
                }
                Err(e) => {
                    debug!(
                        worker_id = %worker_id,
                        drv_hash = %drv_hash,
                        error = %e,
                        "prefetch hint dropped (channel full; assignment may also fail)"
                    );
                }
            }
        }
    }
}
