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
        // on acquire/lose via LeaderState::on_acquire/on_lose.
        // SeqCst load: paired with SeqCst stores in LeaderState so
        // the three-field transition (generation, is_leader,
        // recovery_complete) is observably ordered even on ARM.
        // A one-pass lag on a single flag is still harmless (see
        // DagActor.is_leader field doc). In non-K8s mode this is
        // always true — no-op check.
        if !self.is_leader.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        // Also gate on recovery: don't dispatch until recover_from_
        // pg has rebuilt the DAG. Otherwise we'd dispatch from a
        // partial/empty DAG mid-recovery. SeqCst pairs with
        // handle_leader_acquired's SeqCst — sees all recovery
        // writes before proceeding (though actor is single-threaded
        // so this is belt-and-suspenders).
        if !self
            .recovery_complete
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }

        // Drain the queue, dispatching eligible derivations and deferring
        // ineligible ones. Deferring (instead of breaking on the first
        // ineligible derivation) prevents head-of-line blocking — an
        // aarch64 drv at queue head must not block all x86_64 dispatch.
        let mut deferred: Vec<DrvHash> = Vec::new();
        let mut dispatched_any = true;
        // Track how many derivations WANTED each class but got deferred.
        // Reported as a gauge at the end — operator signal for "class X
        // is bottlenecked, scale that pool."
        let mut class_deferred: HashMap<String, u64> = HashMap::new();
        // FODs deferred waiting for a fetcher. Separate from
        // class_deferred — fetchers aren't size-classed (ADR-019).
        let mut fod_deferred: u64 = 0;

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

                // Classify by estimated duration + memory + CPU. None
                // if size_classes unconfigured (optional feature off —
                // no filter, all workers candidates).
                //
                // Read guard is dropped at the end of this block —
                // BEFORE `assign_to_worker().await`. parking_lot
                // guards aren't `Send`; the await point would be a
                // compile error anyway, but keeping the scope tight
                // is defensive.
                let target_class = {
                    let classes = self.size_classes.read();
                    crate::assignment::classify(
                        state.est_duration,
                        self.estimator
                            .peak_memory(state.pname.as_deref(), &state.system),
                        self.estimator
                            .peak_cpu(state.pname.as_deref(), &state.system),
                        &classes,
                    )
                };

                // Try target class first, then overflow to larger
                // classes if no worker in target has capacity. A
                // "small" build CAN go to a "large" worker (just
                // wasteful); a "large" build CANNOT go to "small"
                // (would under-provision). So overflow walks UP only.
                let (eligible_worker, chosen_class) =
                    self.find_executor_with_overflow(&drv_hash, target_class.as_deref());

                match eligible_worker {
                    Some(executor_id) => {
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

                        if self.assign_to_worker(&drv_hash, &executor_id).await {
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
                        // FODs tracked separately: they have no class,
                        // and the operator action is "scale fetchers"
                        // not "scale class X builders".
                        if state.is_fixed_output {
                            fod_deferred += 1;
                        } else if let Some(class) = target_class {
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
        // Zero out ALL configured classes first. Gauges PERSIST in
        // Prometheus until overwritten — a class that was backed up
        // (gauge=50) then cleared (no entries in class_deferred this
        // pass) would STAY at 50 forever without this zeroing.
        // Operators would see a phantom bottleneck.
        for sc in self.size_classes.read().iter() {
            metrics::gauge!("rio_scheduler_class_queue_depth", "class" => sc.name.clone()).set(0.0);
        }
        // Now overwrite for classes that actually have deferrals.
        for (class, count) in class_deferred {
            metrics::gauge!("rio_scheduler_class_queue_depth", "class" => class).set(count as f64);
        }

        // FOD queue depth + fetcher utilization (ADR-019 observability).
        // Same snapshot semantics as class_queue_depth: per-dispatch-
        // pass gauge, next pass overwrites. Zero is a legitimate value
        // (no FODs queued), emitted explicitly so Prometheus doesn't
        // persist stale nonzero.
        metrics::gauge!("rio_scheduler_fod_queue_depth").set(fod_deferred as f64);
        let (busy, total) = self.executors.values().fold((0u32, 0u32), |(b, t), e| {
            if e.kind == rio_proto::types::ExecutorKind::Fetcher {
                (b + u32::from(!e.running_builds.is_empty()), t + 1)
            } else {
                (b, t)
            }
        });
        // No fetchers → emit 0.0 (not NaN). An operator seeing
        // fod_queue_depth > 0 AND fetcher_utilization == 0 with no
        // fetchers registered knows the FetcherPool isn't deployed.
        let util = if total > 0 {
            f64::from(busy) / f64::from(total)
        } else {
            0.0
        };
        metrics::gauge!("rio_scheduler_fetcher_utilization").set(util);
    }

    /// Find a worker for this derivation, starting at `target_class` and
    /// overflowing to progressively larger classes if needed.
    ///
    /// Returns `(executor_id, class_actually_used)`. Both None if nobody
    /// can take it (wrong system, all full, no workers).
    ///
    /// Overflow direction: small → large only. A slow build on a small
    /// worker would dominate that worker's single slot; a fast build on
    /// a large worker is just slightly wasteful. scheduler.md:178.
    fn find_executor_with_overflow(
        &self,
        drv_hash: &DrvHash,
        target_class: Option<&str>,
    ) -> (Option<ExecutorId>, Option<String>) {
        let Some(drv_state) = self.dag.node(drv_hash) else {
            return (None, None);
        };

        // r[impl sched.dispatch.no-fod-fallback]
        // FODs skip the overflow chain entirely. Fetchers have no size
        // classes (per ADR-019: "no size-class because fetches are
        // network-bound") so there's nothing to overflow through. If no
        // fetcher is free the FOD queues — the scheduler NEVER sends a
        // FOD to a builder under pressure. A queued FOD is preferable
        // to a builder with internet access.
        if drv_state.is_fixed_output {
            let w = crate::assignment::best_executor(&self.executors, drv_state, &self.dag, None);
            return (w, None);
        }

        // No classification configured → single best_executor call with
        // no filter. Fast path for deployments without size-classes.
        let Some(target) = target_class else {
            let w = crate::assignment::best_executor(&self.executors, drv_state, &self.dag, None);
            return (w, None);
        };

        // Build the overflow chain: target class, then all classes with
        // cutoff > target's cutoff, sorted ascending. If target cutoff
        // is 30s, chain is [small(30), medium(300), large(3600)].
        //
        // We don't cache this chain because classify() is called fresh
        // per-dispatch anyway (est_duration can change between ticks
        // via estimator refresh) and the sort is 2-4 elements.
        //
        // Read guard lives through the chain walk — no `.await` in
        // this fn, so it's safe. best_executor is sync.
        let classes = self.size_classes.read();
        let target_cutoff = crate::assignment::cutoff_for(target, &classes);
        let mut chain: Vec<(&str, f64)> = classes
            .iter()
            .filter(|c| {
                // Target itself (== cutoff) or larger.
                // target_cutoff=None shouldn't happen (target came
                // FROM classify which reads the same config) but be
                // defensive: if None, include everything.
                target_cutoff.is_none_or(|t| c.cutoff_secs >= t)
            })
            .map(|c| (c.name.as_str(), c.cutoff_secs))
            .collect();
        chain.sort_by(|a, b| a.1.total_cmp(&b.1));

        // Walk the chain: first class with an available worker wins.
        for (class, _) in chain {
            if let Some(w) =
                crate::assignment::best_executor(&self.executors, drv_state, &self.dag, Some(class))
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
        executor_id: &ExecutorId,
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
                    executor_id = %executor_id,
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
            state.assigned_executor = Some(executor_id.clone());
        }

        // Single atomic load. The lease task may fetch_add the
        // generation between the DB insert and the WorkAssignment send
        // below (there's an await in between). Without this snapshot,
        // the two reads could see DIFFERENT generations — the PG row
        // says "assigned under gen N" but the worker receives "gen
        // N+1." The worker then rejects its own assignment as stale.
        // Loading once and reusing closes the tear.
        //
        // Acquire pairs with the lease task's Release fetch_add. Sees
        // the generation AND any writes the lease task did before it
        // (is_leader=true, which dispatch_ready checked at loop top).
        let generation = self.generation.load(std::sync::atomic::Ordering::Acquire);

        // Update DB (non-terminal: log failure, don't block dispatch)
        self.persist_status(drv_hash, DerivationStatus::Assigned, Some(executor_id))
            .await;

        // Create assignment in DB. PG BIGINT is signed; cast at THIS
        // boundary, not at the proto-encode sites below. One cast
        // instead of two, and the proto sites are hotter (this PG
        // write is best-effort anyway — log+continue on error).
        if let Some(state) = self.dag.node(drv_hash)
            && let Some(db_id) = state.db_id
            && let Err(e) = self
                .db
                .insert_assignment(db_id, executor_id, generation as i64)
                .await
        {
            error!(drv_hash = %drv_hash, executor_id = %executor_id, error = %e, "failed to insert assignment record");
        }

        // Track on worker
        if let Some(worker) = self.executors.get_mut(executor_id) {
            worker.running_builds.insert(drv_hash.clone());
        }

        // Auto-pin: write input-closure paths to scheduler_live_pins
        // so GC's mark CTE protects them. Same closure
        // approximation as send_prefetch_hint and best_executor
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
        // Same closure approximation as best_executor's bloom
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
        self.send_prefetch_hint(executor_id, drv_hash);

        // CA input resolution: rewrite placeholder paths in
        // env/args/builder to realized output paths before
        // dispatch. Fires when gateway set needs_resolve (ADR-018
        // Appendix B: floating-CA self OR ia.deferred — IA drv
        // with a floating-CA input).
        //
        // `maybe_resolve_ca` returns the (possibly rewritten)
        // drv_content PLUS the realisation lookups performed. On
        // resolve error (missing realisation, PG blip) it logs and
        // returns the original unresolved bytes + empty lookups —
        // the worker's build fails on the placeholder path, which
        // is the correct signal (retry after the realisation lands).
        //
        // The resolve runs in its OWN scoped borrow of `self.dag`
        // (node() + collect_ca_inputs both &-borrow) so the lookups
        // can be stashed via node_mut() below before the main
        // WorkAssignment construction takes its own & borrow.
        let (drv_content_to_send, resolve_lookups) = {
            let Some(state) = self.dag.node(drv_hash) else {
                return false;
            };
            self.maybe_resolve_ca(drv_hash, state).await
        };

        // Stash lookups for handle_success_completion's
        // insert_realisation_deps (the FK needs the parent's own
        // realisation row to exist, which only happens post-build).
        // Empty vec → no-op; non-empty only for CA-on-CA chains
        // that actually resolved.
        if !resolve_lookups.is_empty()
            && let Some(state) = self.dag.node_mut(drv_hash)
        {
            state.pending_realisation_deps = resolve_lookups;
        }

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
                    // Match rio-builder's DEFAULT_DAEMON_TIMEOUT.
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
                signer.sign(&rio_common::hmac::AssignmentClaims {
                    executor_id: executor_id.to_string(),
                    drv_hash: drv_hash.to_string(),
                    expected_outputs: state.expected_output_paths.clone(),
                    // Floating-CA: output path is computed post-build
                    // from the NAR hash, so expected_output_paths is
                    // [""] here. Store skips the path-in-claims check
                    // when is_ca is set (verify-on-put still hashes
                    // the NAR independently; threat model holds).
                    // Fixed-output CA (FOD) has a known path → treat
                    // as IA for the membership check.
                    is_ca: state.is_ca && !state.is_fixed_output,
                    expiry_unix,
                })
            } else {
                // Legacy unsigned: format-string. Store with
                // hmac_verifier=None accepts this.
                format!("{executor_id}-{drv_hash}-{generation}")
            };

            let assignment = rio_proto::types::WorkAssignment {
                drv_path: state.drv_path().to_string(),
                // Forward what the gateway inlined (or empty → worker
                // fetches from store). Gateway only inlines for nodes
                // whose outputs are MISSING (will-dispatch), so cache
                // hits don't bloat this. Worker already handles both
                // paths (executor/mod.rs:241 branches on is_empty).
                // For CA-depends-on-CA derivations, this is the
                // RESOLVED ATerm (placeholders replaced by realized
                // paths) — see maybe_resolve_ca above.
                drv_content: drv_content_to_send,
                output_names: state.output_names.clone(),
                build_options: Some(build_opts),
                assignment_token,
                generation,
                is_fixed_output: state.is_fixed_output,
                traceparent: state.traceparent.clone(),
            };

            let msg = rio_proto::types::SchedulerMessage {
                msg: Some(rio_proto::types::scheduler_message::Msg::Assignment(
                    assignment,
                )),
            };

            if let Some(worker) = self.executors.get(executor_id)
                && let Some(tx) = &worker.stream_tx
                && let Err(e) = tx.try_send(msg)
            {
                warn!(
                    executor_id = %executor_id,
                    drv_hash = %drv_hash,
                    error = %e,
                    "failed to send assignment to worker"
                );
                // Clean up worker tracking (we added drv_hash above;
                // without this, the worker appears to have this derivation
                // running, causing a phantom capacity leak).
                if let Some(worker) = self.executors.get_mut(executor_id) {
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
                        executor_id = %executor_id,
                        current = ?state.status(),
                        error = %e,
                        "reset_to_ready failed after assignment send failure; derivation orphaned in Assigned"
                    );
                    metrics::counter!("rio_scheduler_transition_rejected_total", "to" => "ready_reset")
                        .increment(1);
                }

                // Also clean up PG state written above.
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

                // This derivation WAS Assigned (counted in running), now
                // Ready (queued). emit_progress so the dashboard sees
                // the rollback instead of stale state until re-dispatch.
                for build_id in self.get_interested_builds(drv_hash) {
                    self.emit_progress(build_id);
                }

                return false;
            }
        }

        // Emit derivation started event
        let interested_builds = self.get_interested_builds(drv_hash);
        for build_id in &interested_builds {
            self.emit_build_event(
                *build_id,
                rio_proto::types::build_event::Event::Derivation(rio_proto::dag::DerivationEvent {
                    derivation_path: self.drv_path_or_hash_fallback(drv_hash),
                    status: Some(rio_proto::dag::derivation_event::Status::Started(
                        rio_proto::dag::DerivationStarted {
                            executor_id: executor_id.to_string(),
                        },
                    )),
                }),
            );
            // Progress snapshot: running count +1, worker set changed.
            // Critpath unchanged on dispatch (no completion, no
            // update_ancestors) — but the dashboard also uses
            // Progress for the running/queued columns, so emit.
            self.emit_progress(*build_id);
        }

        debug!(drv_hash = %drv_hash, executor_id = %executor_id, "assigned derivation to worker");
        metrics::counter!("rio_scheduler_assignments_total").increment(1);
        true
    }

    /// Send a PrefetchHint for the chosen worker to warm its FUSE
    /// cache. Best-effort: `try_send`, failure logs at debug.
    ///
    /// Same closure approximation as [`best_executor`]'s scoring
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
    /// best_executor picked a warm worker — that's the point of
    /// bloom-locality scoring).
    ///
    /// [`best_executor`]: crate::assignment::best_executor
    fn send_prefetch_hint(&self, executor_id: &ExecutorId, drv_hash: &DrvHash) {
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
        let Some(worker) = self.executors.get(executor_id.as_str()) else {
            // Worker gone between best_executor and here? Actor is
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
        let mut to_prefetch = to_prefetch;
        if to_prefetch.len() > super::MAX_PREFETCH_PATHS {
            to_prefetch.truncate(super::MAX_PREFETCH_PATHS);
        }

        if to_prefetch.is_empty() {
            // Everything filtered = best_executor picked a warm worker.
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
                        executor_id = %executor_id,
                        drv_hash = %drv_hash,
                        error = %e,
                        "prefetch hint dropped (channel full; assignment may also fail)"
                    );
                }
            }
        }
    }

    /// If this derivation is CA-floating with CA inputs, resolve
    /// placeholder paths to realized output paths before dispatch.
    /// Returns `(drv_content_bytes, realisation_lookups)`: the
    /// (possibly rewritten) ATerm plus every
    /// `(dep_modular_hash, dep_output_name) → realized_path` lookup
    /// the resolve performed. Caller stashes lookups on
    /// `DerivationState.pending_realisation_deps` for the
    /// completion-time `insert_realisation_deps` call (the FK needs
    /// the parent's OWN realisation row to exist first).
    ///
    /// ADR-018 Appendix B: resolve fires when `needs_resolve` is set
    /// by the gateway — floating-CA self (`has_ca_floating_outputs`)
    /// OR any inputDrv is floating-CA (`ia.deferred`: an IA drv
    /// depending on a CA input has the CA placeholder embedded in
    /// its env/args). Fixed-output CA with no CA inputs doesn't need
    /// resolve — its output path AND its inputs' paths are all
    /// eval-time known.
    ///
    /// The resolve step queries the `realisations` table for each CA
    /// input's `(modular_hash, output_name)` → `output_path`, then
    /// string-replaces placeholders through the ATerm. Each lookup
    /// is staged for `realisation_deps` INSERT (rio's derived build
    /// trace, per ADR-018:45) — though the actual INSERT is deferred
    /// to completion time (the FK needs the parent's OWN realisation
    /// to exist, which only happens post-build).
    ///
    /// Error handling: resolve failure (missing realisation, PG blip)
    /// logs and returns the original unresolved bytes + empty lookups.
    /// The worker's build will then fail on the placeholder path not
    /// existing (`/1ril1qzj...` is not a real store path), triggering
    /// the normal retry-with-backoff. This is correct: a missing
    /// realisation means the input's `wopRegisterDrvOutput` hasn't
    /// landed yet (race), and retry-after-backoff gives it time to.
    async fn maybe_resolve_ca(
        &self,
        drv_hash: &DrvHash,
        state: &crate::state::DerivationState,
    ) -> (Vec<u8>, Vec<crate::ca::RealisationLookup>) {
        // Gate: ADR-018 Appendix B `shouldResolve`. Gateway computes
        // `needs_resolve = has_ca_floating_outputs() || any inputDrv
        // is floating-CA` at translate time. Covers both floating-CA
        // self AND ia.deferred (IA with CA inputs — the CA input's
        // placeholder is embedded in this drv's env/args and needs
        // rewriting to the realized path).
        if !state.needs_resolve {
            return (state.drv_content.clone(), Vec::new());
        }

        // Build the input lists: walk DAG children, split into CA
        // and IA. For CA children we need the MODULAR hash (the
        // `realisations` table key, plumbed by the gateway via
        // `DerivationNode.ca_modular_hash`). For IA children we need
        // the `expected_output_paths` (deterministic, computed at
        // gateway submit time from the parsed `.drv`).
        //
        // Nix's `tryResolve` (derivations.cc:1206-1234) iterates ALL
        // inputDrvs regardless of addressing mode, adding each output
        // path to `inputSrcs`. CA outputs come from realisations; IA
        // outputs are concrete and already in the DAG.
        //
        // Floating-CA with NO inputs at all (rare: a leaf CA drv with
        // only fixed srcs) doesn't need resolve — nothing to collapse
        // into inputSrcs. Short-circuit before the ATerm parse.
        let ca_inputs = self.collect_ca_inputs(drv_hash);
        let ia_inputs = self.collect_ia_inputs(drv_hash);
        if ca_inputs.is_empty() && ia_inputs.is_empty() {
            return (state.drv_content.clone(), Vec::new());
        }

        // No drv_content → recovered derivation (scheduler restart,
        // DAG reloaded from PG, drv_content not persisted). The store
        // has the ATerm — fetch it. Workers do the same when the
        // inline is empty (build_types.proto:231: "Empty = fallback;
        // worker fetches via GetPath"). ~10-50ms round-trip, once
        // per recovered floating-CA dispatch.
        //
        // Checked AFTER the both-empty short-circuit: a recovered
        // floating-CA with no DAG inputs doesn't need resolve and
        // doesn't need the fetch — worker fetches the unresolved
        // `.drv` from the store itself (same path it always does
        // when `drv_content` is empty). Any floating-CA WITH inputs
        // (CA or IA) needs the scheduler-side fetch so
        // `resolve_ca_inputs` can parse `inputDrvs` and serialize
        // the resolved `BasicDerivation` form.
        //
        // The same lossy-on-recovery pattern still applies to
        // `ca_modular_hash` (see `collect_ca_inputs`'s skip-on-None)
        // and `pending_realisation_deps` (best-effort cache,
        // reconstituted here on each resolve).
        //
        // r[impl sched.ca.resolve+2]
        let drv_content = if state.drv_content.is_empty() {
            match self.fetch_drv_content_from_store(drv_hash, state).await {
                Some(bytes) => bytes,
                None => {
                    // Store unreachable or .drv not found — dispatch
                    // unresolved (worker fails on placeholder,
                    // self-heals via retry after a fresh SubmitBuild
                    // re-merges with inline drv_content). Same
                    // degrade as before P0408.
                    warn!(
                        drv_hash = %drv_hash,
                        "recovered CA-on-CA dispatch: drv_content empty + store fetch failed; \
                         dispatching unresolved (worker will fail on placeholder)"
                    );
                    return (state.drv_content.clone(), Vec::new());
                }
            }
        } else {
            state.drv_content.clone()
        };

        match crate::ca::resolve_ca_inputs(&drv_content, &ca_inputs, &ia_inputs, self.db.pool())
            .await
        {
            Ok(resolved) => {
                debug!(
                    drv_hash = %drv_hash,
                    n_ca_inputs = ca_inputs.len(),
                    n_ia_inputs = ia_inputs.len(),
                    n_lookups = resolved.lookups.len(),
                    "CA resolve: rewrote placeholders + collapsed inputSrcs for dispatch"
                );
                (resolved.drv_content, resolved.lookups)
            }
            Err(e) => {
                // Swallow-to-warn for ALL ResolveError variants,
                // including `Db` (transient PG blip). Rationale:
                // the unresolved dispatch → worker fails on the
                // placeholder path → retry-with-backoff fires →
                // next dispatch re-runs resolve. For
                // `RealisationMissing`, the backoff gives the
                // input's `wopRegisterDrvOutput` time to land
                // (race). For `Db`, the backoff IS the retry-PG
                // mechanism — the wasted worker cycle (~seconds
                // to fail on ENOENT) is acceptable vs adding a
                // defer-and-requeue path here (would need a timer
                // to re-dispatch, which `backoff_until` already
                // provides on the FAILURE path). Slot-wasteful
                // but correct; profiling can drive a `Db → defer`
                // split if the waste proves measurable.
                warn!(
                    drv_hash = %drv_hash,
                    error = %e,
                    "CA resolve failed; dispatching unresolved (worker will fail on placeholder)"
                );
                // Return the (possibly fetched-from-store) bytes
                // unresolved. If the fetch succeeded but resolve
                // failed, the worker at least skips its own GetPath.
                (drv_content, Vec::new())
            }
        }
    }

    /// Fetch a derivation's ATerm bytes from the store via `GetPath`.
    ///
    /// The store returns NAR-framed bytes; a `.drv` is a single
    /// regular file, so [`rio_nix::nar::extract_single_file`] unwraps
    /// it to the raw ATerm. This is the same path the worker takes
    /// when `WorkAssignment.drv_content` is empty
    /// ([`rio-builder/src/executor/inputs.rs::fetch_drv_from_store`]).
    ///
    /// Returns `None` on any failure: store unconfigured
    /// (`store_client = None`, test mode), `GetPath` error, timeout,
    /// not-found, or NAR unwrap failure. Callers treat `None` as
    /// "degrade to the pre-P0408 behavior" — dispatch unresolved,
    /// worker fails on placeholder, retry-with-backoff self-heals.
    ///
    /// Hard 2s timeout + 1 MiB NAR cap: a `.drv` is ~1-50 KB ASCII.
    /// A larger-than-1-MiB blob means something is badly wrong (the
    /// path isn't a `.drv`, or the store returned a closure NAR).
    /// Either way, bail — resolve can't parse a non-ATerm.
    async fn fetch_drv_content_from_store(
        &self,
        drv_hash: &DrvHash,
        state: &crate::state::DerivationState,
    ) -> Option<Vec<u8>> {
        /// `.drv` NAR cap. ~1-50 KB typical; 1 MiB is ~20× any
        /// real-world `.drv`. Avoids pulling a multi-GB closure if
        /// the store path was mis-resolved.
        const MAX_DRV_NAR_SIZE: u64 = 1024 * 1024;
        /// End-to-end `GetPath` + stream-drain timeout. ~10-50 ms
        /// typical; 2 s covers a slow store without blocking
        /// dispatch for long. On timeout we degrade to unresolved
        /// dispatch (same as store-unconfigured).
        const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

        let mut client = self.store_client.as_ref()?.clone();
        let drv_path = state.drv_path().to_string();

        let result = rio_proto::client::get_path_nar(
            &mut client,
            &drv_path,
            FETCH_TIMEOUT,
            MAX_DRV_NAR_SIZE,
        )
        .await;

        let nar = match result {
            Ok(Some((_info, nar))) => nar,
            Ok(None) => {
                debug!(
                    drv_hash = %drv_hash,
                    drv_path = %drv_path,
                    "recovered CA resolve: .drv not found in store"
                );
                return None;
            }
            Err(e) => {
                debug!(
                    drv_hash = %drv_hash,
                    drv_path = %drv_path,
                    error = %e,
                    "recovered CA resolve: GetPath failed"
                );
                return None;
            }
        };

        // NAR unwrap: .drv is a single regular file. Anything else
        // (directory, symlink, corrupt NAR) → None.
        match rio_nix::nar::extract_single_file(&nar) {
            Ok(bytes) => Some(bytes),
            Err(e) => {
                debug!(
                    drv_hash = %drv_hash,
                    error = %e,
                    "recovered CA resolve: NAR unwrap failed (not a single regular file)"
                );
                None
            }
        }
    }

    /// Collect CA inputs for resolve. Walks the DAG children (deps)
    /// and returns a `CaResolveInput` for each child with
    /// `is_ca = true` AND a populated `ca_modular_hash`.
    ///
    /// Children with `is_ca && ca_modular_hash.is_none()` are
    /// skipped — the gateway couldn't compute the modular hash
    /// (BasicDerivation fallback, or recovered state where the
    /// hash wasn't persisted). The parent's resolve is incomplete
    /// for that input → worker fails on the placeholder path →
    /// retry-with-backoff. The next SubmitBuild referencing this
    /// child re-merges the proto with a fresh `ca_modular_hash`.
    fn collect_ca_inputs(&self, drv_hash: &DrvHash) -> Vec<crate::ca::CaResolveInput> {
        // DAG children = dependencies (must complete before this drv).
        let children = self.dag.get_children(drv_hash);
        let mut inputs = Vec::new();
        for child_hash in children {
            let Some(child) = self.dag.node(&child_hash) else {
                continue;
            };
            if !child.is_ca {
                continue;
            }
            let Some(modular_hash) = child.ca_modular_hash else {
                // Gateway didn't populate (BasicDerivation fallback
                // OR recovered state). Skip — resolve is incomplete
                // for this input, worker fails on placeholder,
                // retry-with-backoff handles it. debug not warn:
                // recovered chains hit this legitimately; the
                // scheduler-restart-mid-CA-chain case is expected
                // to degrade to worker-retry, not spam logs.
                debug!(
                    drv_hash = %drv_hash,
                    child = %child_hash,
                    "collect_ca_inputs: child is CA but ca_modular_hash unset; \
                     resolve incomplete, worker will fail on placeholder"
                );
                continue;
            };
            inputs.push(crate::ca::CaResolveInput {
                drv_path: child.drv_path().to_string(),
                modular_hash,
                output_names: child.output_names.clone(),
            });
        }
        inputs
    }

    /// Collect IA (input-addressed) inputs for resolve. Walks the
    /// DAG children and returns an [`IaResolveInput`] for each child
    /// with `is_ca = false` AND non-empty `expected_output_paths`.
    ///
    /// IA output paths are deterministic — the gateway computed them
    /// at submit time from the parsed `.drv` and plumbed them via
    /// `DerivationNode.expected_output_paths`. No store RPC needed.
    /// This is the same field [`approx_input_closure`] reads for
    /// prefetch scoring, so the data is already live.
    ///
    /// Children with empty `expected_output_paths` (recovered state
    /// where the paths weren't persisted, or a proto without the
    /// field) are skipped — `resolve_ca_inputs` will log and skip
    /// the `inputSrcs` add for that input. The worker's FUSE layer
    /// on-demand-fetches regardless, so builds don't break; only
    /// resolved-drv-hash compat with Nix is affected.
    ///
    /// [`IaResolveInput`]: crate::ca::IaResolveInput
    /// [`approx_input_closure`]: crate::assignment::approx_input_closure
    fn collect_ia_inputs(&self, drv_hash: &DrvHash) -> Vec<crate::ca::IaResolveInput> {
        let children = self.dag.get_children(drv_hash);
        let mut inputs = Vec::new();
        for child_hash in children {
            let Some(child) = self.dag.node(&child_hash) else {
                continue;
            };
            if child.is_ca {
                // CA child with a modular hash — handled by
                // collect_ca_inputs via realisation lookup. But a CA
                // child WITHOUT a modular hash (recovered state,
                // BasicDerivation fallback) that HAS completed can
                // still contribute its realized output_paths here:
                // the resolve doesn't need the realisation table when
                // we already have the concrete path in-memory.
                if child.ca_modular_hash.is_some() || child.output_paths.is_empty() {
                    continue;
                }
                // Fall through: CA child, no modular hash, but
                // output_paths is populated (completed). Treat as IA
                // for the purpose of inputSrcs collection — the
                // realized path is just as concrete as an IA
                // expected_output_path.
            }
            // Prefer realized output_paths (filled on completion) over
            // expected_output_paths (filled at merge). For IA children
            // the two are equivalent; for the CA-no-hash fallthrough
            // above, only output_paths is usable (expected is [""]
            // for floating-CA).
            let paths = if !child.output_paths.is_empty() {
                &child.output_paths
            } else {
                &child.expected_output_paths
            };
            if paths.is_empty() {
                // Recovered node or proto without the field. Skip;
                // resolve_ca_inputs logs and skips the inputSrcs add.
                continue;
            }
            inputs.push(crate::ca::IaResolveInput {
                drv_path: child.drv_path().to_string(),
                output_names: child.output_names.clone(),
                output_paths: paths.clone(),
            });
        }
        inputs
    }
}
