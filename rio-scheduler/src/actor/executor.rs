//! Worker lifecycle: connect/disconnect, heartbeat, periodic tick.
// r[impl sched.worker.dual-register]
// r[impl sched.worker.deregister-reassign]

use super::*;

/// Initial-hint budget: max store paths to send in the registration-
/// time `PrefetchHint`. A broad common-set (glibc, stdenv, etc.) is the
/// intent, not the entire queue's closure. See [`on_worker_registered`].
///
/// [`on_worker_registered`]: DagActor::on_worker_registered
use super::MAX_PREFETCH_PATHS;

/// Initial-hint scan budget: max Ready derivations to consider. Don't
/// walk 10k Ready nodes just to send 100 paths. 32 derivations × ~40
/// paths covers `MAX_PREFETCH_PATHS` with typical closure sizes.
const MAX_READY_TO_SCAN: usize = 32;

/// Compute the initial `PrefetchHint` path-set for a freshly registered
/// worker. Pure function over the DAG: no side effects, no actor state.
/// Extracted from `on_worker_registered` so the determinism test can
/// exercise it directly (building 40+ Ready nodes via the full actor
/// loop is prohibitive — each needs a connect/assign/complete round-trip).
///
/// Sort Ready nodes by fan-in (`interested_builds.len()`) descending.
/// Highest fan-in = most likely to be dispatched first (the existing
/// `best_executor` scoring already favors high-fan-in). Their closures
/// are the paths a warm cache wants preloaded. Deterministic: sort key
/// is stable across restarts for the same queue state (same Ready set,
/// same `interested_builds`).
///
/// Tie-break on `drv_hash` ascending so identical-fan-in nodes get a
/// reproducible order — `drv_hash` is the DAG key, always unique.
///
/// Why fan-in not `submitted_at`: `submitted_at` lives on `BuildState`,
/// not on `DerivationState`. A derivation with 5 interested builds was
/// needed by 5 submissions — that's a stronger dispatch-priority signal
/// than which single build submitted first.
///
/// `interested_builds.len()` is O(1) (`HashSet::len`). Sort is
/// O(n log n) over Ready nodes. Acceptable: registration is once per
/// worker per reconnect, not hot-path.
pub(crate) fn compute_initial_prefetch_paths(dag: &DerivationDag) -> Vec<String> {
    let mut ready: Vec<(&str, &crate::state::DerivationState)> = dag
        .iter_nodes()
        .filter(|(_, s)| s.status() == DerivationStatus::Ready)
        .collect();
    ready.sort_by(|(ha, a), (hb, b)| {
        b.interested_builds
            .len()
            .cmp(&a.interested_builds.len())
            .then_with(|| ha.cmp(hb))
    });
    let ready_hashes: Vec<DrvHash> = ready
        .into_iter()
        .take(MAX_READY_TO_SCAN)
        .map(|(h, _)| DrvHash::from(h))
        .collect();

    // Count how many of the scanned derivations want each path.
    // High-count paths are the "broad common-set" — glibc/stdenv
    // appearing in most closures. Sort by count descending (tie-break
    // on path string for reproducibility), take the top 100.
    //
    // Why not BTreeSet (would give deterministic lexicographic
    // iteration): lex order has no correlation with prefetch-value
    // (`/nix/store/0...` first). Frequency-sort is the same
    // O(n log n) and directly serves the "broad common-set" intent
    // — a path that appears in 30/32 closures is one every upcoming
    // assignment will want.
    let mut path_counts: HashMap<String, usize> = HashMap::new();
    for h in &ready_hashes {
        for p in crate::assignment::approx_input_closure(dag, h) {
            *path_counts.entry(p).or_default() += 1;
        }
    }
    let mut paths: Vec<(String, usize)> = path_counts.into_iter().collect();
    paths.sort_by(|(pa, ca), (pb, cb)| cb.cmp(ca).then_with(|| pa.cmp(pb)));
    paths
        .into_iter()
        .take(MAX_PREFETCH_PATHS)
        .map(|(p, _)| p)
        .collect()
}

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
        executor_id: &ExecutorId,
        stream_tx: mpsc::Sender<rio_proto::types::SchedulerMessage>,
    ) {
        info!(executor_id = %executor_id, "worker stream connected");

        let worker = self
            .executors
            .entry(executor_id.clone())
            .or_insert_with(|| ExecutorState::new(executor_id.clone()));

        let was_registered = worker.is_registered();
        worker.stream_tx = Some(stream_tx);

        if !was_registered && worker.is_registered() {
            info!(executor_id = %executor_id, "worker fully registered (stream + heartbeat)");
            metrics::gauge!("rio_scheduler_workers_active").increment(1.0);
            self.on_worker_registered(executor_id);
        }
    }

    /// Flip `warm=true` on PrefetchComplete ACK. See
    /// `r[sched.assign.warm-gate]`. No-op if the worker is unknown
    /// (disconnected between hint and ACK — rare race).
    pub(super) fn handle_prefetch_complete(
        &mut self,
        executor_id: &ExecutorId,
        paths_fetched: u32,
    ) {
        let Some(w) = self.executors.get_mut(executor_id) else {
            debug!(executor_id = %executor_id,
                   "PrefetchComplete for unknown worker (disconnected?)");
            return;
        };
        // Idempotent: a worker sending two ACKs (e.g., pre-dispatch
        // hint + first per-assignment hint both getting ACKed) just
        // re-sets an already-true flag. No warn — this is expected
        // for the per-assignment PrefetchHint path (dispatch.rs:342)
        // which also triggers worker-side PrefetchComplete.
        let was_warm = w.warm;
        w.warm = true;
        if !was_warm {
            info!(executor_id = %executor_id, paths_fetched,
                  "warm-gate open: worker ACKed initial prefetch");
        }
        // Record unconditionally: every PrefetchComplete is a data
        // point about hint effectiveness (fetched vs cached). The
        // histogram serves observability, not gating — the gate is
        // the warm flag flip above, which is idempotent.
        metrics::histogram!("rio_scheduler_warm_prefetch_paths").record(f64::from(paths_fetched));
    }

    /// Hook fired exactly once per worker when it transitions
    /// not-registered → registered (both stream AND heartbeat present).
    ///
    /// r[impl sched.assign.warm-gate]
    /// Sends the INITIAL PrefetchHint so the worker can warm its FUSE
    /// cache before receiving any WorkAssignment. If the ready queue
    /// is empty (nothing to prefetch for), the gate flips open
    /// immediately — spec: "Empty scheduler queue at registration
    /// time → warm flips true immediately."
    ///
    /// Hint contents: see [`compute_initial_prefetch_paths`] — union
    /// of top-fan-in Ready derivations' input closures (same
    /// `approx_input_closure` as best_executor scoring and the
    /// per-assignment hint at dispatch.rs:520), capped at 100 paths.
    fn on_worker_registered(&mut self, executor_id: &ExecutorId) {
        let paths = compute_initial_prefetch_paths(&self.dag);

        let Some(worker) = self.executors.get_mut(executor_id) else {
            return; // can't happen (caller just looked it up) but be defensive
        };

        if paths.is_empty() {
            // Nothing queued → nothing to prefetch → gate open now.
            // Same flip as handle_prefetch_complete, just short-
            // circuited. Tests that register workers BEFORE merging
            // a DAG land here — keeps their dispatch assumptions
            // valid without a synthetic ACK.
            worker.warm = true;
            debug!(executor_id = %executor_id,
                   "warm-gate open on registration: ready queue empty");
            return;
        }

        // Send the hint. try_send: if the freshly-opened stream is
        // somehow already full (256 cap) something is very wrong; log
        // and flip warm anyway (gate is optimization, not correctness).
        let hint_len = paths.len();
        let msg = rio_proto::types::SchedulerMessage {
            msg: Some(rio_proto::types::scheduler_message::Msg::Prefetch(
                rio_proto::types::PrefetchHint { store_paths: paths },
            )),
        };
        let sent = worker
            .stream_tx
            .as_ref()
            .is_some_and(|tx| tx.try_send(msg).is_ok());
        if sent {
            debug!(executor_id = %executor_id, paths = hint_len,
                   "warm-gate: sent initial PrefetchHint on registration");
            metrics::counter!("rio_scheduler_prefetch_hints_sent_total").increment(1);
            metrics::counter!("rio_scheduler_prefetch_paths_sent_total").increment(hint_len as u64);
            // warm stays false until PrefetchComplete arrives.
        } else {
            // stream_tx None or channel full — neither should happen
            // for a just-registered worker. Flip warm so dispatch
            // isn't permanently wedged for this worker.
            warn!(executor_id = %executor_id,
                  "warm-gate: initial hint send failed; flipping warm anyway");
            worker.warm = true;
        }
    }

    pub(super) async fn handle_executor_disconnected(&mut self, executor_id: &ExecutorId) {
        info!(executor_id = %executor_id, "worker disconnected");

        let Some(worker) = self.executors.remove(executor_id) else {
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
        self.reassign_derivations(&to_reassign, Some(executor_id))
            .await;

        // Dashboard: running count dropped by to_reassign.len();
        // assigned_executors set lost this worker. Without emit_progress
        // here, a quiet build shows stale state until the next
        // unrelated dispatch/completion.
        let affected: std::collections::HashSet<Uuid> = to_reassign
            .iter()
            .flat_map(|h| self.get_interested_builds(h))
            .collect();
        for build_id in affected {
            self.emit_progress(build_id);
        }

        if was_registered {
            metrics::gauge!("rio_scheduler_workers_active").decrement(1.0);
        }
        metrics::counter!("rio_scheduler_worker_disconnects_total").increment(1);
    }

    /// Reset a set of derivations to Ready and re-enqueue.
    ///
    /// Extracted from `handle_executor_disconnected` so `handle_drain_executor`
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
    /// `failed_builders` set. This feeds:
    /// - `best_executor()` exclusion (don't retry on the SAME broken
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
        lost_worker: Option<&ExecutorId>,
    ) {
        for drv_hash in drv_hashes {
            // Track the lost worker (in-mem + PG) + check poison
            // threshold BEFORE reset_to_ready — poison_and_cascade
            // expects Assigned/Running, not Ready. Without the
            // threshold check, 3 sequential disconnects leave
            // failed_builders={w1,w2,w3} with status=Ready →
            // best_executor excludes all 3 → deferred forever.
            let should_poison = if let Some(executor_id) = lost_worker {
                self.record_failure_and_check_poison(drv_hash, executor_id)
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
                // Capture BEFORE reset_to_ready clobbers status. An
                // Assigned-but-never-Running derivation didn't consume
                // a retry budget — the worker disconnected before
                // starting it. Only was-Running counts as an attempt.
                let was_running = matches!(state.status(), DerivationStatus::Running);
                if let Err(e) = state.reset_to_ready() {
                    warn!(
                        drv_hash = %drv_hash, error = %e,
                        "invalid state for reassignment, skipping"
                    );
                    continue;
                }
                // Worker-loss mid-build is a failed attempt: count it.
                if was_running {
                    state.retry_count += 1;
                    if let Err(e) = self.db.increment_retry_count(drv_hash).await {
                        error!(drv_hash = %drv_hash, error = %e, "failed to persist retry increment");
                    }
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
    /// Returns `accepted=false` only for unknown executor_id. That's not
    /// an error — the worker's preStop calls this AFTER receiving
    /// SIGTERM, which may race with the BuildExecution stream closing
    /// (SIGTERM → select! break → stream drop → ExecutorDisconnected →
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
    pub(super) async fn handle_drain_executor(
        &mut self,
        executor_id: &ExecutorId,
        force: bool,
    ) -> DrainResult {
        let Some(worker) = self.executors.get_mut(executor_id.as_str()) else {
            // Unknown. Not an error — worker may have disconnected first.
            // running=0: caller proceeds immediately (nothing to wait for).
            debug!(executor_id = %executor_id, "drain request for unknown worker");
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
                executor_id = %executor_id,
                running = worker.running_builds.len(),
                force,
                "worker draining"
            );
            // ClusterStatus.active_executors counts `is_registered() && !draining` —
            // but the gauge tracks is_registered() only (drain doesn't
            // decrement it; disconnect does). That's intentional: a
            // draining worker is still connected, still heartbeating,
            // still "active" in the "pod is alive" sense. The controller
            // cares about the DISTINCTION (active vs draining) which
            // ClusterStatus provides separately.
        } else {
            debug!(executor_id = %executor_id, force, "drain request for already-draining worker");
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
            // it calls DrainExecutor(force=true). The CancelSignal
            // makes the worker SIGKILL its builds immediately (via
            // cgroup.kill) instead of letting them run for the
            // full terminationGracePeriodSeconds (2h — wasted if
            // the pod is evicting anyway).
            // (wired: P0285 rio-controller disruption.rs watcher)
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
                    if let Err(e) = tx.try_send(rio_proto::types::SchedulerMessage {
                        msg: Some(rio_proto::types::scheduler_message::Msg::Cancel(
                            rio_proto::types::CancelSignal {
                                drv_path,
                                reason: "worker draining (forced)".into(),
                            },
                        )),
                    }) {
                        debug!(executor_id = %executor_id, drv_hash = %drv_hash, error = %e,
                               "cancel signal dropped (stream full/closed)");
                        metrics::counter!("rio_scheduler_cancel_signal_dropped_total").increment(1);
                    }
                }
                if !to_reassign.is_empty() {
                    info!(
                        executor_id = %executor_id,
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
            // track it in failed_builders. A force-drained worker is
            // "failed" for these builds in the sense that it didn't
            // finish them — exclude it from retry consideration
            // (moot since it's draining anyway, but consistent with
            // the disconnect path and feeds poison detection).
            self.reassign_derivations(&to_reassign, Some(executor_id))
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
    /// Returns confirmed-phantom drv hashes for the caller to reassign.
    /// Kept sync — the async PG write for the reassign lives in the
    /// caller (mod.rs Heartbeat arm) which already `.await`s
    /// `dispatch_ready`. See the phantom-detection block below for
    /// what "confirmed" means.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn handle_heartbeat(
        &mut self,
        executor_id: &ExecutorId,
        systems: Vec<String>,
        supported_features: Vec<String>,
        max_builds: u32,
        running_builds: Vec<String>, // drv_paths from worker proto
        bloom: Option<rio_common::bloom::BloomFilter>,
        size_class: Option<String>,
        resources: Option<rio_proto::types::ResourceUsage>,
        store_degraded: bool,
        kind: rio_proto::types::ExecutorKind,
    ) -> Vec<DrvHash> {
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
            .executors
            .get(executor_id.as_str())
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
                    executor_id = %executor_id,
                    drv_hash = %drv_hash,
                    "heartbeat reports running build scheduler did not assign"
                );
                reconciled.insert(drv_hash.clone());
            }
        }

        // Phantom detection (I-035): entries the prior reconcile kept
        // (TOCTOU keep-logic) but the worker still doesn't report. Two
        // consecutive misses = past the ~10s race window = phantom slot
        // (lost completion à la I-032, or assignment sent into a stream
        // that died right after). Compute against PRIOR phantom_suspects
        // before the entry().or_insert below borrows worker mutably.
        let suspects: HashSet<DrvHash> = reconciled.difference(&heartbeat_set).cloned().collect();
        let prior_suspects: HashSet<DrvHash> = self
            .executors
            .get(executor_id.as_str())
            .map(|w| w.phantom_suspects.clone())
            .unwrap_or_default();
        let confirmed_phantoms: Vec<DrvHash> =
            suspects.intersection(&prior_suspects).cloned().collect();
        for phantom in &confirmed_phantoms {
            warn!(
                executor_id = %executor_id,
                drv_hash = %phantom,
                "phantom running_builds entry: scheduler tracked this assignment \
                 across two heartbeats but worker reports nothing — draining \
                 (lost completion?)"
            );
            metrics::counter!("rio_scheduler_phantom_assignments_drained_total").increment(1);
            reconciled.remove(phantom);
        }

        let worker = self
            .executors
            .entry(executor_id.clone())
            .or_insert_with(|| ExecutorState::new(executor_id.clone()));

        let was_registered = worker.is_registered();

        // Observability: heartbeat-alive but stream channel closed.
        // The bridge task (executor_service.rs build-exec-bridge) exits
        // when the gRPC ReceiverStream is dropped — but worker-stream-
        // reader keeps running until the inbound half breaks. In the
        // gap, dispatch's try_send Errs with the rollback path. This
        // WARN surfaces the half-dead state in logs before dispatch
        // hits it. Doesn't change `is_registered()` — the rollback path
        // already handles it; this is operator signal.
        if worker.stream_tx.as_ref().is_some_and(|tx| tx.is_closed()) {
            warn!(
                executor_id = %executor_id,
                "heartbeat-alive but stream_tx closed (bridge task exited); \
                 executor unreachable for dispatch until reconnect"
            );
        }

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
        // kind: overwrite unconditionally. An executor that flips kind
        // mid-life is a misconfiguration, but the scheduler should
        // reflect the most recent heartbeat (not a stale default).
        // hard_filter reads this for FOD routing (ADR-019).
        worker.kind = kind;
        // resources: DON'T clobber with None. Prost makes message
        // fields Option<T>; worker always populates, but if a future
        // proto version omits it, keep the last-known reading for
        // ListExecutors rather than flashing None.
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
            info!(executor_id = %executor_id, "marked store-degraded; removing from assignment pool");
        } else if was_degraded && !store_degraded {
            info!(executor_id = %executor_id, "store-degraded cleared; returning to assignment pool");
        }

        // Missed-once → carry to next heartbeat. Missed-twice
        // (confirmed) → already removed from `reconciled` above so
        // they're gone from `running_builds`; drop from suspects too
        // (re-detection would need a fresh dispatch insert first).
        let confirmed_set: HashSet<DrvHash> = confirmed_phantoms.iter().cloned().collect();
        worker.phantom_suspects = suspects.difference(&confirmed_set).cloned().collect();

        if !was_registered && worker.is_registered() {
            info!(executor_id = %executor_id, "worker fully registered (heartbeat + stream)");
            metrics::gauge!("rio_scheduler_workers_active").increment(1.0);
            // Same hook as handle_worker_connected: whichever of
            // (stream, heartbeat) arrives SECOND triggers the warm-
            // gate initial prefetch. dual-register step 3.
            self.on_worker_registered(executor_id);
        }

        confirmed_phantoms
    }

    /// Reset confirmed-phantom derivations to Ready and re-queue.
    /// Called from the Heartbeat arm in mod.rs after `handle_heartbeat`
    /// returns — split out because the PG write is async and
    /// `handle_heartbeat` stays sync.
    ///
    /// NOT via `reassign_derivations`: that path adds to
    /// `failed_builders` + checks poison, but a phantom is NOT a worker
    /// failure. The worker may have completed successfully and we just
    /// never heard about it (I-032). Penalizing the executor_id would
    /// recreate the very dead-capacity problem this drain exists to
    /// fix — `failed_builders` is keyed by executor_id (StatefulSet
    /// pod name), stable across restarts.
    pub(super) async fn drain_phantoms(&mut self, phantoms: Vec<DrvHash>) {
        for phantom in phantoms {
            let Some(state) = self.dag.node_mut(&phantom) else {
                continue;
            };
            if let Err(e) = state.reset_to_ready() {
                // State changed under us (completion arrived between
                // heartbeat and here, or another build cancelled it).
                // The phantom is moot — skip.
                debug!(drv_hash = %phantom, error = %e,
                       "phantom drain: reset_to_ready rejected (state moved)");
                continue;
            }
            self.persist_status(&phantom, DerivationStatus::Ready, None)
                .await;
            self.push_ready(phantom);
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

        // Ordering is load-bearing: backstop-process runs before the
        // per-build-timeout check, poison-expire runs last — matches
        // the pre-refactor code sequence. Backstop reassigns stuck
        // derivations (transient retry); per-build-timeout cancels
        // whole builds (permanent failure); poison-expire removes
        // DAG nodes after both have had their say.
        let (expired_poisons, backstop_timeouts) = self.tick_scan_dag(now);
        self.tick_process_backstop_timeouts(&backstop_timeouts)
            .await;
        self.tick_check_build_timeouts().await;
        self.tick_process_expired_poisons(expired_poisons).await;

        self.tick_sweep_event_log();
        self.tick_publish_gauges();
    }

    // -----------------------------------------------------------------------
    // handle_tick helpers — one per periodic check
    // -----------------------------------------------------------------------

    /// Scan workers for heartbeat timeouts; disconnect any that have
    /// missed MAX_MISSED_HEARTBEATS consecutive checks.
    async fn tick_check_heartbeats(&mut self, now: Instant) {
        let timeout = std::time::Duration::from_secs(HEARTBEAT_TIMEOUT_SECS);
        let mut timed_out_workers = Vec::new();

        for (executor_id, worker) in &mut self.executors {
            if now.duration_since(worker.last_heartbeat) > timeout {
                worker.missed_heartbeats += 1;
                if worker.missed_heartbeats >= MAX_MISSED_HEARTBEATS {
                    timed_out_workers.push(executor_id.clone());
                }
            }
        }

        for executor_id in timed_out_workers {
            warn!(executor_id = %executor_id, "worker timed out (missed heartbeats)");
            self.handle_executor_disconnected(&executor_id).await;
        }
    }

    /// Single DAG pass collecting both poison-TTL expiries and backstop-
    /// timeout candidates. Coupled because the two checks share the
    /// per-node iteration; splitting would double `iter_nodes()` passes
    /// for no behavioral gain.
    ///
    /// Returns `(expired_poisons, backstop_timeouts)` — backstop tuple is
    /// `(drv_hash, drv_path, executor_id)`.
    // r[impl sched.backstop.timeout]
    fn tick_scan_dag(&self, now: Instant) -> (Vec<DrvHash>, Vec<(DrvHash, String, ExecutorId)>) {
        let mut expired_poisons: Vec<DrvHash> = Vec::new();
        // (drv_hash, drv_path, executor_id) for backstop-timed-out builds
        let mut backstop_timeouts: Vec<(DrvHash, String, ExecutorId)> = Vec::new();

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
                    && let Some(executor_id) = &state.assigned_executor
                {
                    backstop_timeouts.push((
                        drv_hash.into(),
                        state.drv_path().to_string(),
                        executor_id.clone(),
                    ));
                }
            }
        }

        (expired_poisons, backstop_timeouts)
    }

    /// Process backstop timeouts: send CancelSignal, reset to
    /// Ready for retry. This is a TRANSIENT failure (the build
    /// may work fine on another worker or even the same worker
    /// after a restart) so we go through retry not poison.
    async fn tick_process_backstop_timeouts(&mut self, timeouts: &[(DrvHash, String, ExecutorId)]) {
        for (drv_hash, drv_path, executor_id) in timeouts {
            warn!(
                drv_hash = %drv_hash,
                executor_id = %executor_id,
                "backstop timeout: build running far longer than expected, cancelling + retrying"
            );
            metrics::counter!("rio_scheduler_backstop_timeouts_total").increment(1);

            // CancelSignal: worker's cgroup.kill. Best-effort
            // try_send — if the worker is truly wedged its stream
            // may be full; reset_to_ready below still progresses.
            if let Some(worker) = self.executors.get(executor_id)
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
            if let Some(worker) = self.executors.get_mut(executor_id) {
                worker.running_builds.remove(drv_hash);
            }
            // Reassign (same path as worker disconnect): reset_to_
            // ready + retry++ + failed_builders.insert (in-mem AND
            // PG via append_failed_worker) + PG status + push_ready.
            self.reassign_derivations(std::slice::from_ref(drv_hash), Some(executor_id))
                .await;
        }
    }

    /// Wall-clock limit on the ENTIRE build from submission. Distinct from:
    ///   - `sched.backstop.timeout` spec marker in [`Self::tick_scan_dag`] (per-derivation
    ///     heuristic: est×3)
    ///   - worker-side daemon floor at `actor/build.rs` `build_options_for_derivation`
    ///     (also receives `build_timeout` as `min_nonzero` per-derivation —
    ///     defense-in-depth, NOT the primary semantics)
    ///
    /// Zero = no overall timeout. Only Active builds are checked: Pending
    /// hasn't started dispatching (`validate_transition` rejects Pending →
    /// Failed anyway); terminal builds are already done.
    // r[impl sched.timeout.per-build]
    async fn tick_check_build_timeouts(&mut self) {
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
    }

    /// Clear expired poison entries (PG first, in-mem second — same
    /// ordering as `handle_clear_poison`: a PG blip here leaves in-mem
    /// still Poisoned, so the next tick's scan retries. Previous order
    /// meant a blip left in-mem gone → scan never finds it again →
    /// PG clear deferred to next scheduler restart).
    async fn tick_process_expired_poisons(&mut self, expired_poisons: Vec<DrvHash>) {
        for drv_hash in expired_poisons {
            info!(drv_hash = %drv_hash, "poison TTL expired, removing from DAG");
            if let Err(e) = self.db.clear_poison(&drv_hash).await {
                error!(drv_hash = %drv_hash, error = %e, "failed to clear poison in PG");
                continue;
            }
            // r[impl sched.poison.ttl-persist]
            // Prune BEFORE remove_node (reads interested_builds from
            // the node). keep_going=true builds still Active would
            // otherwise hang: derivation_hashes keeps the stale hash
            // → total never reached. keep_going=false builds are
            // already terminal (failed fast at poison time).
            self.prune_interested_keep_going(&drv_hash);
            // Remove (not reset) — same rationale as handle_clear_poison.
            self.dag.remove_node(&drv_hash);
        }
    }

    /// `build_event_log` time-based sweep. Every 360 ticks (~1h at
    /// 10s interval). Safety net for terminal-cleanup delete —
    /// if that failed (PG blip), rows would leak. Also catches
    /// rows from builds that never hit terminal-cleanup (actor
    /// restart mid-build, PG restored before recovery).
    ///
    /// `spawn_monitored` (not bare spawn): a PG panic in the sweep
    /// logs with task=event-log-sweep + component=scheduler instead
    /// of vanishing. Still fire-and-forget — `handle_tick` doesn't block.
    /// 24h retention is plenty for WatchBuild replay (gateway
    /// reconnects are within minutes of disconnect).
    fn tick_sweep_event_log(&self) {
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
    }

    /// Update metrics. All gauges are set from ground-truth state on each
    /// Tick — this is self-healing against any counting bugs elsewhere.
    /// The inc/dec calls at connect/disconnect/heartbeat stay — they give
    /// sub-tick responsiveness. This block corrects any drift every tick.
    ///
    /// Leader-only: standby's actor is warm (DAGs merge for takeover) but
    /// workers don't connect to it (leader-guarded gRPC), so its counts are
    /// stale-or-zero. With replicas:2, Prometheus scrapes both; a naked
    /// gauge query returns two series. Stat-panel lastNotNull picks one
    /// nondeterministically. Gate here so the standby simply doesn't export
    /// the series — queries see one series, no max() wrapper needed.
    // r[impl obs.metric.scheduler-leader-gate]
    fn tick_publish_gauges(&self) {
        if self.is_leader.load(std::sync::atomic::Ordering::SeqCst) {
            metrics::gauge!("rio_scheduler_derivations_queued").set(self.ready_queue.len() as f64);
            metrics::gauge!("rio_scheduler_workers_active").set(
                self.executors
                    .values()
                    .filter(|w| w.is_registered())
                    .count() as f64,
            );
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
}
