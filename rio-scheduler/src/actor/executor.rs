//! Executor lifecycle: connect/disconnect/register, heartbeat
//! reconcile, phantom-drain. Periodic `tick_*` housekeeping lives in
//! [`super::housekeeping`].
// r[impl sched.executor.dual-register]
// r[impl sched.executor.deregister-reassign]

use std::collections::HashMap;
use std::time::Instant;

use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::dag::DerivationDag;
use crate::state::{DerivationStatus, DrvHash, ExecutorId, ExecutorState};

/// Initial-hint budget: max store paths to send in the registration-
/// time `PrefetchHint`. A broad common-set (glibc, stdenv, etc.) is the
/// intent, not the entire queue's closure. See [`on_worker_registered`].
///
/// [`on_worker_registered`]: DagActor::on_worker_registered
use super::{DagActor, DrainResult, HeartbeatPayload, MAX_PREFETCH_PATHS};

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
/// Highest fan-in = most likely to be dispatched first. Their closures
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

        let entry = self.executors.entry(executor_id.clone());
        let is_reconnect = matches!(entry, std::collections::hash_map::Entry::Occupied(_));
        let worker = entry.or_insert_with(|| ExecutorState::new(executor_id.clone()));

        // I-056a: clear scheduler-side `draining` and `store_degraded`
        // on reconnect. We're here because the disconnect signal
        // didn't fire (old stream's task still in TCP/h2 close
        // handshake when the new stream's connect arrived). Both flags
        // reflect prior-session state — stale. Live: fetchers stuck
        // 22 min after deploy churn drained them; only restart
        // cleared it.
        //
        // NOT `draining_hb`: I-063 split the worker's OWN drain state
        // (SIGTERM received) into a separate heartbeat-authoritative
        // field. A draining worker reconnecting after a scheduler
        // restart re-asserts `draining_hb=true` on its next heartbeat;
        // in the gap, leaving the prior value intact prevents
        // mis-dispatch. The split is what lets I-056a's clear and
        // I-063's preserve coexist — they touch different fields.
        if is_reconnect && (worker.draining || worker.store_degraded) {
            info!(
                executor_id = %executor_id,
                was_draining = worker.draining,
                was_store_degraded = worker.store_degraded,
                "worker reconnected; clearing stale scheduler-side flags \
                 (draining_hb left to heartbeat per I-063)"
            );
            worker.draining = false;
            worker.store_degraded = false;
        }
        if is_reconnect {
            worker.connected_since = std::time::Instant::now();
        }

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
    ///
    /// Returns `true` on a cold→warm transition (the caller dispatches
    /// inline, capped per Tick); `false` for already-warm re-ACKs
    /// (per-assignment hints) and unknown workers.
    pub(super) fn handle_prefetch_complete(
        &mut self,
        executor_id: &ExecutorId,
        paths_fetched: u32,
    ) -> bool {
        let Some(w) = self.executors.get_mut(executor_id) else {
            debug!(executor_id = %executor_id,
                   "PrefetchComplete for unknown worker (disconnected?)");
            return false;
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
        !was_warm
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
    /// `approx_input_closure` as the per-assignment hint in
    /// dispatch.rs), capped at 100 paths.
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

        // Reassign whatever was on this worker. The worker is gone;
        // whether it was draining or not doesn't matter now.
        // I-173: capture size_class from the removed worker — the
        // reassign path's FOD floor promotion can't look it up via
        // self.executors anymore. I-197: capture last_completed too —
        // disconnect AFTER a completion report for the running drv is
        // the expected one-shot exit (suppress promotion); disconnect
        // WITHOUT one is OOMKilled mid-build (promote).
        let lost_class = worker.size_class.clone();
        let lost_last_completed = worker.last_completed.clone();
        let to_reassign: Vec<DrvHash> = worker.running_build.into_iter().collect();
        self.reassign_derivations(
            &to_reassign,
            Some(executor_id),
            lost_class.as_deref(),
            lost_last_completed.as_ref(),
        )
        .await;

        // Dashboard: running count dropped; assigned_executors lost
        // this worker. Without emit_progress here, a quiet build shows
        // stale state until the next unrelated dispatch/completion.
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
    /// skipped with a warn — it shouldn't be in `running_build` but
    /// split-brain or delayed heartbeat reconcile can produce it.
    ///
    /// `lost_worker`: if Some AND the drv was Running, record it in
    /// the derivation's `failed_builders` set. This feeds:
    /// - `best_executor()` exclusion (don't retry on the SAME broken
    ///   worker)
    /// - Poison detection (3 distinct failed workers → poisoned). A
    ///   derivation that crashed 3 workers should not loop forever.
    ///
    /// I-097: Assigned-but-never-Running disconnects do NOT record —
    /// the drv was never attempted, so it can't have caused the
    /// disconnect. Under ephemeral-fetcher churn this race is common
    /// (assignment lands just before the one-shot Job exits); counting
    /// it would falsely poison drvs that nothing ever tried.
    ///
    /// `lost_worker_class`: the lost worker's `size_class`, captured
    /// by the caller before any `self.executors.remove()`. Threaded
    /// through because `handle_executor_disconnected` removes the
    /// executor before calling here, so a `self.executors.get()`
    /// lookup inside would miss (I-173).
    ///
    /// `lost_worker_last_completed`: the lost worker's
    /// `last_completed`, captured the same way. Floor promotion is
    /// suppressed when the disconnect is the expected one-shot exit:
    /// the worker already reported completion for the drv being
    /// reassigned (`last_completed == Some(drv_hash)` — the I-188
    /// race). A worker that disconnects WITHOUT having completed its
    /// running drv was OOMKilled mid-build (I-197) → promote per
    /// `r[sched.builder.size-class-reactive]`.
    ///
    /// `None` for callers that don't have a specific lost worker
    /// (none currently, but keeps the signature extensible).
    pub(super) async fn reassign_derivations(
        &mut self,
        drv_hashes: &[DrvHash],
        lost_worker: Option<&ExecutorId>,
        lost_worker_class: Option<&str>,
        lost_worker_last_completed: Option<&DrvHash>,
    ) {
        for drv_hash in drv_hashes {
            // I-097: only count toward poison/failed_builders if the
            // drv was RUNNING when the worker disconnected. Assigned-
            // but-never-started is a pure scheduling race (ephemeral
            // fetcher exited between try_send and process) — the drv
            // was never attempted, so it can't have caused the crash.
            // Under ephemeral churn, 3 such races would falsely
            // poison a drv that nothing ever tried to build.
            //
            // Running → worker started then died → drv may have
            // crashed it → record + poison-check (the original
            // rationale: a drv that crashes 3 workers should poison).
            let was_running = self
                .dag
                .node(drv_hash)
                .is_some_and(|s| matches!(s.status(), DerivationStatus::Running));

            // Track the lost worker (in-mem + PG) + check poison
            // threshold BEFORE reset_to_ready — poison_and_cascade
            // expects Assigned/Running, not Ready.
            let mut promoted = false;
            let should_poison = match lost_worker {
                Some(executor_id) => {
                    // r[impl sched.fod.size-class-reactive]
                    // r[impl sched.builder.size-class-reactive]
                    // r[impl sched.reassign.no-promote-on-ephemeral-disconnect+2]
                    // I-173/I-177: promote size_class_floor on
                    // disconnect, FOD or not, REGARDLESS of
                    // was_running — DerivationStatus stays Assigned
                    // for the build's whole lifetime (Running is set
                    // only at completion via ensure_running()), so
                    // gating on was_running would never fire in
                    // production. Uses `lost_worker_class` captured
                    // before executor removal — the self.executors
                    // lookup inside record_failure_and_check_poison
                    // misses on the disconnect path.
                    //
                    // I-197: suppress promotion ONLY when the worker
                    // already reported completion for THIS drv (the
                    // I-188 race: completion → one-shot exit,
                    // disconnect with stale running_build). A worker
                    // that disconnects mid-build (last_completed !=
                    // drv_hash, typically None) was OOMKilled — that
                    // IS a size-adequacy signal. I-188's blanket
                    // suppress made openssl OOM-loop on `tiny` for
                    // hours with size_class_floor empty.
                    let expected_oneshot_exit = lost_worker_last_completed == Some(drv_hash);
                    promoted = if expected_oneshot_exit {
                        false
                    } else {
                        self.promote_size_class_floor(drv_hash, lost_worker_class)
                            .await
                    };
                    if was_running {
                        let outcome = self
                            .record_failure_and_check_poison(drv_hash, executor_id, promoted)
                            .await;
                        promoted = outcome.promoted;
                        outcome.reached_poison
                    } else {
                        // I-097: Assigned-only — don't record failure
                        // (scheduling race, drv never attempted). Re-
                        // read existing poison state so 3 prior REAL
                        // failures + 1 disconnect still poisons
                        // instead of dispatching a 4th time.
                        self.dag
                            .node(drv_hash)
                            .map(|s| self.poison_config.is_poisoned(s))
                            .unwrap_or(false)
                    }
                }
                // No lost_worker: just re-read existing poison state.
                None => self
                    .dag
                    .node(drv_hash)
                    .map(|s| self.poison_config.is_poisoned(s))
                    .unwrap_or(false),
            };
            if should_poison {
                info!(drv_hash = %drv_hash, lost_worker = ?lost_worker,
                      was_running,
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
                // Worker-loss mid-build is a failed attempt: count it
                // unless it promoted the floor (`r[sched.retry.
                // promotion-exempt]` — sizing signal, bounded by
                // ladder length, not the retry budget).
                if was_running && !promoted {
                    state.retry.count += 1;
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
                busy: false,
            };
        };

        let was_draining = worker.draining;
        worker.draining = true;

        // Log the transition once. Repeat calls at debug.
        if !was_draining {
            info!(
                executor_id = %executor_id,
                running = u32::from(worker.running_build.is_some()),
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
            // Take the build + capture stream_tx before reassign
            // (borrows self mut). We'll send CancelSignal AFTER we
            // know the drv_path (which needs DAG lookup).
            let to_reassign: Vec<DrvHash> = worker.running_build.take().into_iter().collect();
            let stream_tx = worker.stream_tx.clone();
            let lost_class = worker.size_class.clone();
            let lost_last_completed = worker.last_completed.clone();

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
            self.reassign_derivations(
                &to_reassign,
                Some(executor_id),
                lost_class.as_deref(),
                lost_last_completed.as_ref(),
            )
            .await;

            return DrainResult {
                accepted: true,
                busy: false, // reassigned: caller doesn't wait
            };
        }

        DrainResult {
            accepted: true,
            busy: worker.running_build.is_some(),
        }
    }

    /// Returns confirmed-phantom drv hashes for the caller to reassign,
    /// and the `became_idle` (capacity 0→1) edge-detect for inline
    /// dispatch. Kept sync — the async PG write for the reassign lives
    /// in the caller (mod.rs Heartbeat arm) which already `.await`s
    /// `dispatch_ready`.
    pub(super) fn handle_heartbeat(&mut self, hb: HeartbeatPayload) -> (Vec<DrvHash>, bool) {
        let executor_id = &hb.executor_id;
        // I-048b: heartbeat for an executor without a stream entry is
        // dropped. Only `handle_worker_connected` (BuildExecution
        // stream open) creates entries. Allowing heartbeat to create
        // produces a zombie with `stream_tx: None` — `is_registered()`
        // is false, `has_capacity()` is false, dispatch is dead-locked
        // until a stream connects (which may not happen for minutes if
        // the worker's old stream is stuck in TCP keepalive timeout
        // after an abrupt scheduler restart). Live: `fod_queue=3
        // fetcher_util=0.00` for 5+ minutes after deploy; fetcher
        // restart unblocks because the fresh process opens a stream.
        //
        // Early-return also skips the reconcile loop below, which
        // would otherwise spuriously WARN "scheduler did not assign"
        // for every running build the unknown worker reports.
        if !self.executors.contains_key(executor_id.as_str()) {
            warn!(
                executor_id = %executor_id,
                "heartbeat for unknown executor; dropping \
                 (stream not yet connected — scheduler restart race?)"
            );
            return (Vec::new(), false);
        }

        // TOCTOU fix: a stale heartbeat must not clobber a fresh assignment.
        // The scheduler is authoritative for what it assigned. We reconcile:
        //   - Keep the scheduler-known build if it is still Assigned/Running
        //     in the DAG (heartbeat may predate the assignment).
        //   - Accept a heartbeat-reported build we don't know about, but warn
        //     (shouldn't happen; indicates split-brain or restart).
        //   - Clear if absent from heartbeat AND DAG state is no longer
        //     Assigned/Running (completion already processed).
        // I-163: capture pre-heartbeat capacity for the 0→1 edge-detect
        // (`r[sched.dispatch.became-idle-immediate]`). Read here, before
        // adopt_heartbeat_build / field updates below — same "before any
        // mutation" snapshot as `prev_running`.
        let had_capacity = self
            .executors
            .get(executor_id.as_str())
            .is_some_and(|w| w.has_capacity());

        let (reconciled, suspect, confirmed_phantoms) =
            self.reconcile_running_build(executor_id, hb.running_build);

        // Existence asserted at top of function (I-048b early-return).
        // get_mut not entry().or_insert: this path never creates.
        let worker = self
            .executors
            .get_mut(executor_id.as_str())
            .expect("checked contains_key at top of fn");

        let was_registered = worker.is_registered();

        // Observability: heartbeat-alive but stream channel closed.
        // The bridge task (executor_service.rs build-exec-bridge) exits
        // when the gRPC ReceiverStream is dropped — but worker-stream-
        // reader keeps running until the inbound half breaks. In the
        // gap, dispatch is gated by rejection_reason()'s stream-closed
        // check (I-095) so the executor is skipped, not picked-then-
        // rolled-back. This WARN is operator signal that the half-dead
        // state was reached. is_registered() stays true (gauge
        // accounting); has_capacity()/hard_filter return false.
        if worker.stream_tx.as_ref().is_some_and(|tx| tx.is_closed()) {
            warn!(
                executor_id = %executor_id,
                "heartbeat-alive but stream_tx closed (bridge task exited); \
                 executor unreachable for dispatch until reconnect"
            );
        }

        worker.systems = hb.systems;
        worker.supported_features = hb.supported_features;
        worker.last_heartbeat = Instant::now();
        worker.missed_heartbeats = 0;
        worker.running_build = reconciled;
        // size_class: overwrite unconditionally. None means the
        // worker didn't declare one (empty string in proto) — it
        // becomes a wildcard worker that accepts any class.
        worker.size_class = hb.size_class;
        // kind: overwrite unconditionally. An executor that flips kind
        // mid-life is a misconfiguration, but the scheduler should
        // reflect the most recent heartbeat (not a stale default).
        // hard_filter reads this for FOD routing (ADR-019).
        worker.kind = hb.kind;
        // resources: DON'T clobber with None. Prost makes message
        // fields Option<T>; worker always populates, but if a future
        // proto version omits it, keep the last-known reading for
        // ListExecutors rather than flashing None.
        if hb.resources.is_some() {
            worker.last_resources = hb.resources;
        }
        // store_degraded: overwrite unconditionally (bool, no Option
        // ambiguity). false→true transition logged at info — a worker
        // dropping out of the assignment pool mid-run is operationally
        // interesting. true→false (recovery) also logged: symmetry.
        // Steady-state (same value both sides) is silent.
        let was_degraded = worker.store_degraded;
        worker.store_degraded = hb.store_degraded;
        if !was_degraded && hb.store_degraded {
            info!(executor_id = %executor_id, "marked store-degraded; removing from assignment pool");
        } else if was_degraded && !hb.store_degraded {
            info!(executor_id = %executor_id, "store-degraded cleared; returning to assignment pool");
        }
        // I-063: `draining_hb` is worker-authoritative — overwrite
        // unconditionally from heartbeat (same shape as store_degraded
        // above). Distinct from `draining` (admin-set via DrainExecutor
        // RPC, cleared on reconnect per I-056a). The split lets a
        // worker that got SIGTERM keep its stream alive across a
        // scheduler restart: it heartbeats `draining=true`, the new
        // leader sees both the in-flight build (running_build) and
        // this flag, so reconcile doesn't reassign. Live: gcc
        // duplicated ~30min CPU when the old loop broke on SIGTERM
        // instead of reconnecting.
        let was_draining_hb = worker.draining_hb;
        worker.draining_hb = hb.draining;
        if !was_draining_hb && hb.draining {
            info!(executor_id = %executor_id,
                  running = u32::from(worker.running_build.is_some()),
                  "worker draining (heartbeat-reported)");
        } else if was_draining_hb && !hb.draining {
            info!(executor_id = %executor_id, "draining cleared (heartbeat-reported)");
        }

        // Missed-once → carry to next heartbeat. Missed-twice (confirmed)
        // → already cleared from `reconciled` above so it's gone from
        // `running_build`; drop the suspect too (re-detection would need
        // a fresh dispatch first).
        worker.phantom_suspect = if confirmed_phantoms.is_empty() {
            suspect
        } else {
            None
        };

        // r[impl sched.dispatch.became-idle-immediate]
        // Capacity 0→1 edge: fresh registration (is_registered flip),
        // store_degraded clear, draining_hb clear, or running_build
        // cleared via reconcile/phantom. Computed AFTER all field
        // writes above (running_build, store_degraded, draining_hb)
        // and BEFORE on_worker_registered borrows &mut self. The
        // caller (mod.rs Heartbeat arm) dispatches inline on this
        // signal instead of deferring to Tick — at most one such
        // transition per executor per spawn/degrade cycle, so this
        // doesn't reintroduce the I-163 heartbeat-storm.
        let became_idle = !had_capacity && worker.has_capacity();

        if !was_registered && worker.is_registered() {
            info!(executor_id = %executor_id, "worker fully registered (heartbeat + stream)");
            metrics::gauge!("rio_scheduler_workers_active").increment(1.0);
            // Same hook as handle_worker_connected: whichever of
            // (stream, heartbeat) arrives SECOND triggers the warm-
            // gate initial prefetch. dual-register step 3.
            self.on_worker_registered(executor_id);
        }

        (confirmed_phantoms, became_idle)
    }

    /// TOCTOU reconcile: a stale heartbeat must not clobber a fresh
    /// assignment. The scheduler is authoritative for what it assigned.
    ///   - Keep the scheduler-known build if it is still
    ///     Assigned/Running in the DAG (heartbeat may predate the
    ///     assignment).
    ///   - Adopt a heartbeat-reported build we don't know about
    ///     (`r[sched.heartbeat.adopt]`).
    ///   - Clear if absent from heartbeat AND DAG state is no longer
    ///     Assigned/Running (completion already processed).
    ///   - Phantom-detect (`r[sched.heartbeat.phantom-drain]`, I-035):
    ///     two consecutive heartbeats where the scheduler-kept build is
    ///     missing from the worker's report → past the ~10s race window
    ///     → lost completion à la I-032, or assignment sent into a
    ///     stream that died right after.
    ///
    /// Returns `(reconciled_running_build, suspect_for_next_hb,
    /// confirmed_phantoms_to_drain)`.
    // r[impl sched.heartbeat.adopt]
    // r[impl sched.heartbeat.phantom-drain]
    fn reconcile_running_build(
        &mut self,
        executor_id: &ExecutorId,
        running_build: Option<String>,
    ) -> (Option<DrvHash>, Option<DrvHash>, Vec<DrvHash>) {
        // Worker reports a drv_path; resolve to a drv_hash via the DAG
        // index. The gRPC layer already rejected >1 entry (P0537).
        let heartbeat_hash: Option<DrvHash> =
            running_build.and_then(|path| self.dag.hash_for_path(&path).cloned());

        // Compute the reconciled value before borrowing worker mutably,
        // so we can read self.dag for derivation state checks.
        let prev_running: Option<DrvHash> = self
            .executors
            .get(executor_id.as_str())
            .and_then(|w| w.running_build.clone());

        // Keep the scheduler-assigned build if still in-flight.
        let prev_kept: Option<DrvHash> = prev_running.as_ref().and_then(|h| {
            self.dag
                .node(h)
                .is_some_and(|s| {
                    matches!(
                        s.status(),
                        DerivationStatus::Assigned | DerivationStatus::Running
                    )
                })
                .then(|| h.clone())
        });
        // Adopt a heartbeat-reported build the scheduler doesn't have on
        // record for this executor. Expected after a scheduler restart:
        // recovery's reconcile may have reset the assignment to Ready
        // (worker not yet reconnected) and re-dispatched, while the
        // worker still has it in-flight (I-063 keeps the stream alive
        // during drain). The worker is authoritative for what it's
        // running — adopt into BOTH `worker.running_build` (so dispatch
        // sees at-capacity) AND the DAG node (so dispatch_ready won't
        // re-pop it; reconcile's cross-check matches). I-066: without
        // the DAG-side adoption, openssl was re-dispatched while two
        // draining workers were already running it → both ended up in
        // failed_builders → I-065 poisoned a passing build.
        let mut reconciled: Option<DrvHash> = match (&prev_kept, &heartbeat_hash) {
            (None, Some(hb)) if prev_running.as_ref() != Some(hb) => {
                self.adopt_heartbeat_build(executor_id, hb);
                Some(hb.clone())
            }
            (kept, _) => kept.clone(),
        };

        // Phantom detection: compute against PRIOR phantom_suspect.
        let suspect: Option<DrvHash> = reconciled
            .as_ref()
            .filter(|r| heartbeat_hash.as_ref() != Some(r))
            .cloned();
        let prior_suspect: Option<DrvHash> = self
            .executors
            .get(executor_id.as_str())
            .and_then(|w| w.phantom_suspect.clone());
        let confirmed_phantoms: Vec<DrvHash> = match (&suspect, &prior_suspect) {
            (Some(s), Some(p)) if s == p => {
                warn!(
                    executor_id = %executor_id,
                    drv_hash = %s,
                    "phantom running_build entry: scheduler tracked this assignment \
                     across two heartbeats but worker reports nothing — draining \
                     (lost completion?)"
                );
                metrics::counter!("rio_scheduler_phantom_assignments_drained_total").increment(1);
                reconciled = None;
                vec![s.clone()]
            }
            _ => Vec::new(),
        };

        (reconciled, suspect, confirmed_phantoms)
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
    /// fix.
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

    /// DAG-side adoption of a heartbeat-reported build the scheduler
    /// has no record of for this executor. The worker is authoritative
    /// for what it's running — recovery's reconcile may have already
    /// reset to Ready and re-dispatched elsewhere by the time the
    /// worker's first post-restart heartbeat arrives.
    ///
    /// In-mem only (no PG persist): `handle_heartbeat` stays sync. The
    /// next status change (completion/failure) persists; if the
    /// scheduler crashes again before that, the next adoption is
    /// idempotent.
    fn adopt_heartbeat_build(&mut self, executor_id: &ExecutorId, hb: &DrvHash) {
        let Some(state) = self.dag.node_mut(hb) else {
            // hash_for_path resolved it, so the node existed a moment
            // ago — concurrent removal would need another command in
            // between, but the actor is single-threaded. Unreachable
            // in practice; warn for visibility.
            warn!(executor_id = %executor_id, drv_hash = %hb,
                  "heartbeat-adopt: node vanished after hash_for_path");
            return;
        };
        match state.status() {
            DerivationStatus::Ready => {
                // Recovery's reconcile reset it. Re-claim for this
                // worker so dispatch_ready won't re-pop and the
                // ~45s-later reconcile cross-check matches.
                if let Err(e) = state.transition(DerivationStatus::Assigned) {
                    warn!(executor_id = %executor_id, drv_hash = %hb, error = %e,
                          "heartbeat-adopt: Ready→Assigned rejected");
                    return;
                }
                state.assigned_executor = Some(executor_id.clone());
                info!(executor_id = %executor_id, drv_hash = %hb,
                      "adopted in-flight build from reconnecting worker (was Ready)");
                metrics::counter!("rio_scheduler_heartbeat_adoptions_total").increment(1);
            }
            DerivationStatus::Assigned | DerivationStatus::Running => {
                match state.assigned_executor.as_ref() {
                    Some(a) if a == executor_id => {
                        // Already correct — recovery loaded it Assigned
                        // to this worker, worker reconnected before
                        // reconcile reset it. Just the worker.running_
                        // build side was stale (handle_worker_connected
                        // creates a fresh entry with running_build=None).
                        info!(executor_id = %executor_id, drv_hash = %hb,
                              "re-adopted in-flight build (DAG already Assigned to this worker)");
                    }
                    other => {
                        // Conflict: reconcile already re-dispatched to
                        // someone else. Both will run; first to complete
                        // uploads, second finds output-already-in-store.
                        // Don't steal the DAG assignment (the other
                        // worker's stream got a real WorkAssignment;
                        // this one's relying on its old local state).
                        // The caller still sets worker.running_build so
                        // this worker is at-capacity.
                        warn!(executor_id = %executor_id, drv_hash = %hb,
                              dag_assigned_to = ?other,
                              "heartbeat-adopt: DAG already Assigned elsewhere; \
                               both will run (first-to-complete wins)");
                        metrics::counter!("rio_scheduler_heartbeat_adopt_conflicts_total")
                            .increment(1);
                    }
                }
            }
            other => {
                // Terminal (Completed/Poisoned/DepFailed/Cancelled/
                // Skipped) or pre-Ready (Created/Queued/Failed). Either
                // way the DAG can't accept the worker's claim — the
                // worker's local build is stale or split-brain. Its
                // eventual completion report will no-op (handle_
                // completion guards on status). Caller still sets
                // worker.running_build = Some(hb) — one heartbeat
                // cycle of false at-capacity, cleared once the worker's
                // local build exits and next heartbeat omits it.
                warn!(executor_id = %executor_id, drv_hash = %hb, status = ?other,
                      "heartbeat-adopt: DAG node not adoptable; \
                       worker's in-flight build is stale");
            }
        }
    }
}
