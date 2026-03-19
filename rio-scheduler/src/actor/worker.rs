//! Worker lifecycle: connect/disconnect, heartbeat, periodic tick.
// r[impl sched.worker.dual-register]
// r[impl sched.worker.deregister-reassign]

use super::*;

/// Backstop timeout floor: DEFAULT_DAEMON_TIMEOUT (the worker-side
/// timeout). A build can't legitimately run longer than this — the
/// worker would have killed the daemon already. The scheduler-side
/// check at this floor is belt-and-suspenders for "worker heartbeating
/// but not enforcing its own timeout" (worker bug or clock skew).
///
/// Same cfg(test) shadow pattern as POISON_TTL: 7200s in prod (matches
/// worker's daemon_timeout default), short in tests so backstop can be
/// observed without waiting 2h.
#[cfg(not(test))]
const BACKSTOP_DAEMON_TIMEOUT_SECS: u64 = 7200;
#[cfg(test)]
const BACKSTOP_DAEMON_TIMEOUT_SECS: u64 = 0; // tests control via est_duration

/// Slack on top of BACKSTOP_DAEMON_TIMEOUT_SECS. The worker's timeout
/// fires → daemon killed → CompletionReport sent → scheduler receives
/// → completion handler runs. 10 minutes covers that round-trip plus
/// gRPC retry/reconnect slack.
#[cfg(not(test))]
const BACKSTOP_SLACK_SECS: u64 = 600;
#[cfg(test)]
const BACKSTOP_SLACK_SECS: u64 = 0;

impl DagActor {
    // -----------------------------------------------------------------------
    // Worker management
    // -----------------------------------------------------------------------

    pub(super) fn handle_worker_connected(
        &mut self,
        worker_id: &WorkerId,
        stream_tx: mpsc::Sender<rio_proto::types::SchedulerMessage>,
    ) {
        info!(worker_id = %worker_id, "worker stream connected");

        let worker = self
            .workers
            .entry(worker_id.clone())
            .or_insert_with(|| WorkerState::new(worker_id.clone()));

        let was_registered = worker.is_registered();
        worker.stream_tx = Some(stream_tx);

        if !was_registered && worker.is_registered() {
            info!(worker_id = %worker_id, "worker fully registered (stream + heartbeat)");
            metrics::gauge!("rio_scheduler_workers_active").increment(1.0);
        }
    }

    pub(super) async fn handle_worker_disconnected(&mut self, worker_id: &WorkerId) {
        info!(worker_id = %worker_id, "worker disconnected");

        let Some(worker) = self.workers.remove(worker_id) else {
            return; // unknown worker, no-op (and no gauge decrement)
        };

        // Only decrement if worker was fully registered (stream + heartbeat).
        // Otherwise the gauge goes negative for workers that connected a stream
        // but never sent a heartbeat (increment fires on full registration only).
        let was_registered = worker.is_registered();

        // Reassign everything that was on this worker. The worker is
        // gone; whether it was draining or not doesn't matter now.
        // Clone the set so we can call reassign (which borrows self mut).
        let to_reassign: Vec<DrvHash> = worker.running_builds.iter().cloned().collect();
        self.reassign_derivations(&to_reassign, Some(worker_id))
            .await;

        if was_registered {
            metrics::gauge!("rio_scheduler_workers_active").decrement(1.0);
        }
        metrics::counter!("rio_scheduler_worker_disconnects_total").increment(1);
    }

    /// Reset a set of derivations to Ready and re-enqueue.
    ///
    /// Extracted from `handle_worker_disconnected` so `handle_drain_worker`
    /// (force=true) can reuse it. Both callers have already decided these
    /// derivations should be retried elsewhere — this is the mechanism.
    ///
    /// `reset_to_ready()` handles both Assigned → Ready and Running →
    /// Failed → Ready (the latter increments retry_count because Running
    /// means the build actually started — that's a failed attempt). A
    /// derivation in any other state (Completed, Poisoned, DepFailed) is
    /// skipped with a warn — it shouldn't be in `running_builds` but
    /// split-brain or delayed heartbeat reconcile can produce it.
    ///
    /// `lost_worker`: if Some, record it in each derivation's
    /// `failed_workers` set. This feeds:
    /// - `best_worker()` exclusion (don't retry on the SAME broken
    ///   worker — it just disconnected, clearly something is wrong
    ///   there)
    /// - Poison detection (3 distinct failed workers → poisoned).
    ///   If worker disconnect didn't feed poison detection, a
    ///   derivation that crashed 3 workers in a row would loop
    ///   forever (each crash = reassign = fresh attempt).
    ///
    /// `None` for callers that don't have a specific lost worker
    /// (none currently, but keeps the signature extensible).
    async fn reassign_derivations(
        &mut self,
        drv_hashes: &[DrvHash],
        lost_worker: Option<&WorkerId>,
    ) {
        for drv_hash in drv_hashes {
            // Track the lost worker (in-mem + PG) + check poison
            // threshold BEFORE reset_to_ready — poison_and_cascade
            // expects Assigned/Running, not Ready. Without the
            // threshold check, 3 sequential disconnects leave
            // failed_workers={w1,w2,w3} with status=Ready →
            // best_worker excludes all 3 → deferred forever.
            let should_poison = if let Some(worker_id) = lost_worker {
                self.record_failure_and_check_poison(drv_hash, worker_id)
                    .await
            } else {
                self.dag
                    .node(drv_hash)
                    .map(|s| self.poison_config.is_poisoned(s))
                    .unwrap_or(false)
            };
            if should_poison {
                info!(drv_hash = %drv_hash, lost_worker = ?lost_worker,
                      "reassign: poison threshold reached, poisoning instead of retry");
                self.poison_and_cascade(drv_hash).await;
                continue;
            }

            if let Some(state) = self.dag.node_mut(drv_hash) {
                if let Err(e) = state.reset_to_ready() {
                    warn!(
                        drv_hash = %drv_hash, error = %e,
                        "invalid state for reassignment, skipping"
                    );
                    continue;
                }
                // Worker-loss mid-build is a failed attempt: count it.
                state.retry_count += 1;
                if let Err(e) = self.db.increment_retry_count(drv_hash).await {
                    error!(drv_hash = %drv_hash, error = %e, "failed to persist retry increment");
                }
                self.persist_status(drv_hash, DerivationStatus::Ready, None)
                    .await;
                self.push_ready(drv_hash.clone());
            }
        }
    }

    /// Mark a worker draining. In-flight builds continue; no new
    /// assignments. `force=true` additionally reassigns in-flight.
    ///
    /// Returns `accepted=false` only for unknown worker_id. That's not
    /// an error — the worker's preStop calls this AFTER receiving
    /// SIGTERM, which may race with the BuildExecution stream closing
    /// (SIGTERM → select! break → stream drop → WorkerDisconnected →
    /// entry removed). In that race, drain is a no-op: the disconnect
    /// already reassigned everything.
    ///
    /// Idempotent on `draining=true`: setting the flag again is a
    /// no-op, same running count returned. The worker's preStop may
    /// retry; the controller's finalizer may ALSO call this for the
    /// same worker. Both succeed.
    ///
    /// `force=true` with draining already set: DOES reassign. Use case:
    /// operator first drains gracefully, builds take too long, operator
    /// force-drains. The builds on the worker will complete (wasted)
    /// but the scheduler stops waiting and redispatches — fresh workers
    /// may finish faster anyway.
    pub(super) async fn handle_drain_worker(
        &mut self,
        worker_id: &WorkerId,
        force: bool,
    ) -> DrainResult {
        let Some(worker) = self.workers.get_mut(worker_id.as_str()) else {
            // Unknown. Not an error — worker may have disconnected first.
            // running=0: caller proceeds immediately (nothing to wait for).
            debug!(worker_id = %worker_id, "drain request for unknown worker");
            return DrainResult {
                accepted: false,
                running_builds: 0,
            };
        };

        let was_draining = worker.draining;
        worker.draining = true;

        // Log the transition once. Repeat calls at debug.
        if !was_draining {
            info!(
                worker_id = %worker_id,
                running = worker.running_builds.len(),
                force,
                "worker draining"
            );
            // ClusterStatus.active_workers counts `is_registered() && !draining` —
            // but the gauge tracks is_registered() only (drain doesn't
            // decrement it; disconnect does). That's intentional: a
            // draining worker is still connected, still heartbeating,
            // still "active" in the "pod is alive" sense. The controller
            // cares about the DISTINCTION (active vs draining) which
            // ClusterStatus provides separately.
        } else {
            debug!(worker_id = %worker_id, force, "drain request for already-draining worker");
        }

        if force {
            // Clone the set + capture stream_tx before reassign
            // (borrows self mut). We'll send CancelSignal for each
            // AFTER we know the drv_paths (which needs DAG lookup).
            let to_reassign: Vec<DrvHash> = worker.running_builds.drain().collect();
            let stream_tx = worker.stream_tx.clone();

            // Send CancelSignal for each in-flight build BEFORE
            // reassigning. This is the preemption hook: when the
            // controller sees DisruptionTarget condition on a pod,
            // it calls DrainWorker(force=true). The CancelSignal
            // makes the worker SIGKILL its builds immediately (via
            // cgroup.kill) instead of letting them run for the
            // full terminationGracePeriodSeconds (2h — wasted if
            // the pod is evicting anyway).
            //
            // try_send: best-effort. If the stream is full/closed,
            // the worker is about to disconnect anyway. reassign_
            // derivations below still redispatches regardless.
            //
            // Look up drv_paths from the DAG (CancelSignal is keyed
            // on drv_path, not drv_hash). Skip derivations that
            // aren't in the DAG (shouldn't happen but be defensive).
            if let Some(tx) = &stream_tx {
                for drv_hash in &to_reassign {
                    let Some(drv_path) = self.dag.node(drv_hash).map(|s| s.drv_path().to_string())
                    else {
                        continue;
                    };
                    let _ = tx.try_send(rio_proto::types::SchedulerMessage {
                        msg: Some(rio_proto::types::scheduler_message::Msg::Cancel(
                            rio_proto::types::CancelSignal {
                                drv_path,
                                reason: "worker draining (forced)".into(),
                            },
                        )),
                    });
                }
                if !to_reassign.is_empty() {
                    info!(
                        worker_id = %worker_id,
                        count = to_reassign.len(),
                        "sent CancelSignal for force-drain (preemption)"
                    );
                    metrics::counter!("rio_scheduler_cancel_signals_total")
                        .increment(to_reassign.len() as u64);
                }
            }

            // Reassign. Worker later sends CompletionReport{Cancelled};
            // completion handler's Cancelled arm is a no-op (status
            // already Ready after reassign, not Assigned/Running — the
            // "not in assigned/running state, ignoring" warn fires.
            // That's fine — the warn documents the expected behavior).
            //
            // Pass the drained worker's ID so reassigned derivations
            // track it in failed_workers. A force-drained worker is
            // "failed" for these builds in the sense that it didn't
            // finish them — exclude it from retry consideration
            // (moot since it's draining anyway, but consistent with
            // the disconnect path and feeds poison detection).
            self.reassign_derivations(&to_reassign, Some(worker_id))
                .await;

            return DrainResult {
                accepted: true,
                running_builds: 0, // reassigned: caller doesn't wait
            };
        }

        DrainResult {
            accepted: true,
            running_builds: worker.running_builds.len() as u32,
        }
    }

    // 10 args — all independent heartbeat fields. A struct would add
    // boilerplate at every call site for an internal-only method.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn handle_heartbeat(
        &mut self,
        worker_id: &WorkerId,
        systems: Vec<String>,
        supported_features: Vec<String>,
        max_builds: u32,
        running_builds: Vec<String>, // drv_paths from worker proto
        bloom: Option<rio_common::bloom::BloomFilter>,
        size_class: Option<String>,
        resources: Option<rio_proto::types::ResourceUsage>,
        store_degraded: bool,
    ) {
        // TOCTOU fix: a stale heartbeat must not clobber fresh assignments.
        // The scheduler is authoritative for what it assigned. We reconcile:
        //   - Keep scheduler-known builds that are still Assigned/Running
        //     in the DAG (heartbeat may predate the assignment).
        //   - Accept heartbeat-reported builds we don't know about, but warn
        //     (shouldn't happen; indicates split-brain or restart).
        //   - Remove builds absent from heartbeat only if DAG state is no
        //     longer Assigned/Running (completion already processed).
        // Worker reports drv_paths; resolve to drv_hashes via the DAG index.
        // Unknown paths (not in DAG) are silently dropped — they indicate
        // split-brain or a stale heartbeat after scheduler restart.
        let heartbeat_set: HashSet<DrvHash> = running_builds
            .into_iter()
            .filter_map(|path| self.dag.hash_for_path(&path).cloned())
            .collect();

        // Compute the reconciled running set before borrowing `worker` mutably,
        // so we can read self.dag for derivation state checks.
        let prev_running: HashSet<DrvHash> = self
            .workers
            .get(worker_id.as_str())
            .map(|w| w.running_builds.clone())
            .unwrap_or_default();

        let mut reconciled: HashSet<DrvHash> = HashSet::new();
        // Keep scheduler-assigned builds that are still in-flight.
        for drv_hash in &prev_running {
            let still_inflight = self.dag.node(drv_hash).is_some_and(|s| {
                matches!(
                    s.status(),
                    DerivationStatus::Assigned | DerivationStatus::Running
                )
            });
            if still_inflight {
                reconciled.insert(drv_hash.clone());
            }
        }
        // Add heartbeat-reported builds we don't know about (with warning).
        for drv_hash in &heartbeat_set {
            if !reconciled.contains(drv_hash) && !prev_running.contains(drv_hash) {
                warn!(
                    worker_id = %worker_id,
                    drv_hash = %drv_hash,
                    "heartbeat reports running build scheduler did not assign"
                );
                reconciled.insert(drv_hash.clone());
            }
        }

        let worker = self
            .workers
            .entry(worker_id.clone())
            .or_insert_with(|| WorkerState::new(worker_id.clone()));

        let was_registered = worker.is_registered();

        worker.systems = systems;
        worker.supported_features = supported_features;
        worker.max_builds = max_builds;
        worker.last_heartbeat = Instant::now();
        worker.missed_heartbeats = 0;
        worker.running_builds = reconciled;
        // Bloom: overwrite unconditionally. A heartbeat without a
        // filter (None) clears the old one — the worker stopped
        // sending it, maybe FUSE unmounted. Better to score it as
        // "unknown locality" than use a stale filter that claims
        // paths are cached when they might have been evicted.
        worker.bloom = bloom;
        // size_class: also overwrite unconditionally. None means the
        // worker didn't declare one (empty string in proto) — it
        // becomes a wildcard worker that accepts any class. Same
        // don't-trust-stale reasoning as bloom.
        worker.size_class = size_class;
        // resources: DON'T clobber with None. Prost makes message
        // fields Option<T>; worker always populates, but if a future
        // proto version omits it, keep the last-known reading for
        // ListWorkers rather than flashing None.
        if resources.is_some() {
            worker.last_resources = resources;
        }
        // store_degraded: overwrite unconditionally (bool, no Option
        // ambiguity). false→true transition logged at info — a worker
        // dropping out of the assignment pool mid-run is operationally
        // interesting. true→false (recovery) also logged: symmetry.
        // Steady-state (same value both sides) is silent.
        let was_degraded = worker.store_degraded;
        worker.store_degraded = store_degraded;
        if !was_degraded && store_degraded {
            info!(worker_id = %worker_id, "marked store-degraded; removing from assignment pool");
        } else if was_degraded && !store_degraded {
            info!(worker_id = %worker_id, "store-degraded cleared; returning to assignment pool");
        }

        if !was_registered && worker.is_registered() {
            info!(worker_id = %worker_id, "worker fully registered (heartbeat + stream)");
            metrics::gauge!("rio_scheduler_workers_active").increment(1.0);
        }
    }

    // -----------------------------------------------------------------------
    // Tick (periodic housekeeping)
    // -----------------------------------------------------------------------

    /// Refresh the estimator from build_history. Runs every ~6 ticks
    /// (60s at the default 10s interval). Separated from handle_tick
    /// so the every-tick housekeeping stays readable.
    async fn maybe_refresh_estimator(&mut self) {
        self.tick_count = self.tick_count.wrapping_add(1);

        // Every 6th tick (≈60s with 10s interval). Not configurable:
        // the estimator is a snapshot, not live; 60s is plenty fresh
        // for critical-path priorities. Making this tunable is YAGNI
        // until someone asks.
        const ESTIMATOR_REFRESH_EVERY: u64 = 6;
        if !self.tick_count.is_multiple_of(ESTIMATOR_REFRESH_EVERY) {
            return;
        }

        // PG read can fail (connection blip). Log and keep the OLD
        // estimator — stale estimates are better than no estimates.
        // The next successful refresh catches up.
        match self.db.read_build_history().await {
            Ok(rows) => {
                let n = rows.len();
                self.estimator.refresh(rows);
                debug!(entries = n, "estimator refreshed from build_history");
                // Counter for VM test observability: vm-phase2c
                // previously sleep(15)'d waiting for this refresh
                // to pick up a pre-seeded build_history row. Now
                // it can poll this metric instead — ≥2 increments
                // after the INSERT = refresh has seen the seed
                // (first tick may have raced, second is certain).
                metrics::counter!("rio_scheduler_estimator_refresh_total").increment(1);
            }
            Err(e) => {
                warn!(error = %e, "estimator refresh failed; keeping previous snapshot");
            }
        }

        // Full critical-path sweep (same 60s cadence). Belt-and-
        // suspenders over the incremental update_ancestors calls: any
        // drift (float accumulation, missed edge case) corrects here.
        // O(V+E); ~1ms for a 10k-node DAG.
        crate::critical_path::full_sweep(&mut self.dag, &self.estimator);

        // Compact the ready queue (remove lazy-invalidated garbage).
        // No-op if garbage <50% of heap. Without this, a long-running
        // scheduler with lots of cancellations leaks heap memory.
        self.ready_queue.compact();
    }

    pub(super) async fn handle_tick(&mut self) {
        self.maybe_refresh_estimator().await;

        let now = Instant::now();
        self.tick_check_heartbeats(now).await;

        // Check for poisoned derivations that should expire (24h TTL)
        // + backstop timeout for stuck-Running derivations.
        // r[impl sched.backstop.timeout]
        let mut expired_poisons: Vec<DrvHash> = Vec::new();
        // (drv_hash, drv_path, worker_id) for backstop-timed-out builds
        let mut backstop_timeouts: Vec<(DrvHash, String, WorkerId)> = Vec::new();

        for (drv_hash, state) in self.dag.iter_nodes() {
            if state.status() == DerivationStatus::Poisoned
                && let Some(poisoned_at) = state.poisoned_at
                && now.duration_since(poisoned_at) > POISON_TTL
            {
                expired_poisons.push(drv_hash.into());
            }

            // Backstop timeout: a build that's been Running far
            // longer than expected is likely stuck (worker still
            // heartbeating but daemon wedged, or the worker's
            // clock jumped). Send CancelSignal + reset to Ready.
            //
            // Threshold: max(est_duration × 3, 7200s + 600s). The
            // first term catches builds that exceed their estimate
            // by 3×; the second is a floor at daemon_timeout + 10
            // minutes slack (even with no estimate, a build can't
            // legitimately run longer than the daemon timeout
            // plus some grace for reporting). 7200 = DEFAULT_
            // DAEMON_TIMEOUT; 600 = arbitrary slack.
            if state.status() == DerivationStatus::Running
                && let Some(running_since) = state.running_since
            {
                let elapsed = now.duration_since(running_since);
                // est_duration is in seconds (f64). 0.0 = no
                // estimate (fresh derivation, estimator had no
                // history) → floor applies.
                //
                // is_finite() guard: NaN/inf propagate through max()
                // (NaN.max(x)=NaN, inf.max(x)=inf) → `elapsed > NaN`
                // is always false → backstop never fires. Treat
                // non-finite est as "no estimate" (0.0 → floor wins).
                let est_3x_secs = if state.est_duration.is_finite() && state.est_duration > 0.0 {
                    state.est_duration * 3.0
                } else {
                    0.0
                };
                let floor_secs = (BACKSTOP_DAEMON_TIMEOUT_SECS + BACKSTOP_SLACK_SECS) as f64;
                let backstop_secs = est_3x_secs.max(floor_secs);

                if elapsed.as_secs_f64() > backstop_secs
                    && let Some(worker_id) = &state.assigned_worker
                {
                    backstop_timeouts.push((
                        drv_hash.into(),
                        state.drv_path().to_string(),
                        worker_id.clone(),
                    ));
                }
            }
        }

        // Process backstop timeouts: send CancelSignal, reset to
        // Ready for retry. This is a TRANSIENT failure (the build
        // may work fine on another worker or even the same worker
        // after a restart) so we go through retry not poison.
        for (drv_hash, drv_path, worker_id) in &backstop_timeouts {
            warn!(
                drv_hash = %drv_hash,
                worker_id = %worker_id,
                "backstop timeout: build running far longer than expected, cancelling + retrying"
            );
            metrics::counter!("rio_scheduler_backstop_timeouts_total").increment(1);

            // CancelSignal: worker's cgroup.kill. Best-effort
            // try_send — if the worker is truly wedged its stream
            // may be full; reset_to_ready below still progresses.
            if let Some(worker) = self.workers.get(worker_id)
                && let Some(tx) = &worker.stream_tx
            {
                // Only count the signal if it actually landed on the stream.
                // try_send Err = channel full or closed — no signal was sent.
                // This keeps cancel_signals_total semantically "signals delivered",
                // which makes the gap vs backstop_timeouts_total explainable:
                // backstop fires for silent workers; silent workers often have
                // no stream_tx (disconnected) → no increment here. That's correct.
                if tx
                    .try_send(rio_proto::types::SchedulerMessage {
                        msg: Some(rio_proto::types::scheduler_message::Msg::Cancel(
                            rio_proto::types::CancelSignal {
                                drv_path: drv_path.clone(),
                                reason: "backstop timeout (stuck build)".into(),
                            },
                        )),
                    })
                    .is_ok()
                {
                    metrics::counter!("rio_scheduler_cancel_signals_total").increment(1);
                }
            }
            // Remove from worker's running set (we're taking it back).
            if let Some(worker) = self.workers.get_mut(worker_id) {
                worker.running_builds.remove(drv_hash);
            }
            // Reassign (same path as worker disconnect): reset_to_
            // ready + retry++ + failed_workers.insert (in-mem AND
            // PG via append_failed_worker) + PG status + push_ready.
            self.reassign_derivations(std::slice::from_ref(drv_hash), Some(worker_id))
                .await;
        }

        // r[impl sched.timeout.per-build]
        //
        // Wall-clock limit on the ENTIRE build from submission. Distinct from:
        //   - r[sched.backstop.timeout] above (per-derivation heuristic: est×3)
        //   - worker-side daemon floor at actor/build.rs build_options_for_
        //     derivation (also receives build_timeout as min_nonzero per-
        //     derivation — defense-in-depth, NOT the primary semantics)
        //
        // Zero = no overall timeout. Only Active builds are checked: Pending
        // hasn't started dispatching (validate_transition rejects Pending →
        // Failed anyway); terminal builds are already done.
        let mut timed_out_builds: Vec<(Uuid, u64)> = Vec::new();
        for (build_id, build) in &self.builds {
            if build.state() == BuildState::Active
                && build.options.build_timeout > 0
                && build.submitted_at.elapsed().as_secs() > build.options.build_timeout
            {
                timed_out_builds.push((*build_id, build.options.build_timeout));
            }
        }
        for (build_id, timeout) in timed_out_builds {
            let reason = format!("build_timeout {timeout}s exceeded (wall-clock since submission)");
            warn!(build_id = %build_id, timeout_secs = timeout, "per-build timeout exceeded; cancelling derivations and failing build");
            metrics::counter!("rio_scheduler_build_timeouts_total").increment(1);

            // Set error_summary FIRST so transition_build_to_failed picks it
            // up for the BuildFailed event + DB error_summary column.
            if let Some(build) = self.builds.get_mut(&build_id) {
                build.error_summary = Some(reason.clone());
            }
            // Reuse the CancelBuild derivation-cancellation path (sends
            // CancelSignal, transitions drvs to Cancelled, removes build
            // interest, prunes ready queue). Then fail the BUILD instead
            // of cancelling it — TimedOut is semantically "permanent-no-
            // reassign" (types.proto:278), same as a build failure.
            self.cancel_build_derivations(build_id, &reason).await;
            if let Err(e) = self.transition_build_to_failed(build_id).await {
                error!(build_id = %build_id, error = %e, "failed to persist per-build-timeout failure");
            }
        }

        for drv_hash in expired_poisons {
            info!(drv_hash = %drv_hash, "poison TTL expired, removing from DAG");
            // PG first, in-mem second (same ordering as handle_clear_poison):
            // a PG blip here leaves in-mem still Poisoned, so the next
            // tick's expired_poisons scan retries. Previous order meant
            // a blip left in-mem gone → scan never finds it again
            // → PG clear deferred to next scheduler restart.
            if let Err(e) = self.db.clear_poison(&drv_hash).await {
                error!(drv_hash = %drv_hash, error = %e, "failed to clear poison in PG");
                continue;
            }
            // Remove (not reset) — same rationale as handle_clear_poison.
            self.dag.remove_node(&drv_hash);
        }

        // build_event_log time-based sweep. Every 360 ticks (~1h at
        // 10s interval). Safety net for terminal-cleanup delete —
        // if that failed (PG blip), rows would leak. Also catches
        // rows from builds that never hit terminal-cleanup (actor
        // restart mid-build, PG restored before recovery).
        //
        // spawn_monitored (not bare spawn): a PG panic in the sweep
        // logs with task=event-log-sweep + component=scheduler instead
        // of vanishing. Still fire-and-forget — handle_tick doesn't block.
        // 24h retention is plenty for WatchBuild replay (gateway
        // reconnects are within minutes of disconnect).
        const EVENT_LOG_SWEEP_EVERY: u64 = 360;
        if self.tick_count.is_multiple_of(EVENT_LOG_SWEEP_EVERY) && self.event_persist_tx.is_some()
        {
            let pool = self.db.pool().clone();
            rio_common::task::spawn_monitored("event-log-sweep", async move {
                match sqlx::query(
                    "DELETE FROM build_event_log WHERE created_at < now() - interval '24 hours'",
                )
                .execute(&pool)
                .await
                {
                    Ok(r) => {
                        if r.rows_affected() > 0 {
                            debug!(
                                rows = r.rows_affected(),
                                "event-log sweep: deleted rows older than 24h"
                            );
                        }
                    }
                    Err(e) => {
                        debug!(error = %e, "event-log sweep failed (will retry next hour)");
                    }
                }
            });
        }

        // Update metrics. All gauges are set from ground-truth state on each
        // Tick — this is self-healing against any counting bugs elsewhere.
        // The inc/dec calls at connect/disconnect/heartbeat (worker.rs:52/
        // :76/:384) stay — they give sub-tick responsiveness. This block
        // corrects any drift every tick.
        //
        // r[impl obs.metric.scheduler-leader-gate]
        // Leader-only: standby's actor is warm (DAGs merge for takeover) but
        // workers don't connect to it (leader-guarded gRPC), so its counts are
        // stale-or-zero. With replicas:2, Prometheus scrapes both; a naked
        // gauge query returns two series. Stat-panel lastNotNull picks one
        // nondeterministically. Gate here so the standby simply doesn't export
        // the series — queries see one series, no max() wrapper needed.
        if self.is_leader.load(std::sync::atomic::Ordering::Relaxed) {
            metrics::gauge!("rio_scheduler_derivations_queued").set(self.ready_queue.len() as f64);
            metrics::gauge!("rio_scheduler_workers_active")
                .set(self.workers.values().filter(|w| w.is_registered()).count() as f64);
            metrics::gauge!("rio_scheduler_builds_active").set(
                self.builds
                    .values()
                    .filter(|b| b.state() == BuildState::Active)
                    .count() as f64,
            );
            metrics::gauge!("rio_scheduler_derivations_running").set(
                self.dag
                    .iter_values()
                    .filter(|s| {
                        matches!(
                            s.status(),
                            DerivationStatus::Running | DerivationStatus::Assigned
                        )
                    })
                    .count() as f64,
            );
        }
    }

    // -----------------------------------------------------------------------
    // handle_tick helpers — one per periodic check
    // -----------------------------------------------------------------------

    /// Scan workers for heartbeat timeouts; disconnect any that have
    /// missed MAX_MISSED_HEARTBEATS consecutive checks.
    async fn tick_check_heartbeats(&mut self, now: Instant) {
        let timeout = std::time::Duration::from_secs(HEARTBEAT_TIMEOUT_SECS);
        let mut timed_out_workers = Vec::new();

        for (worker_id, worker) in &mut self.workers {
            if now.duration_since(worker.last_heartbeat) > timeout {
                worker.missed_heartbeats += 1;
                if worker.missed_heartbeats >= MAX_MISSED_HEARTBEATS {
                    timed_out_workers.push(worker_id.clone());
                }
            }
        }

        for worker_id in timed_out_workers {
            warn!(worker_id = %worker_id, "worker timed out (missed heartbeats)");
            self.handle_worker_disconnected(&worker_id).await;
        }
    }
}
