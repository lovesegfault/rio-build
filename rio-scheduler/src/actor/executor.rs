//! Stream-era executor session plumbing: connect/disconnect/register,
//! heartbeat reconcile, drain. Production traffic no longer reaches it
//! (the `BuildExecution`/`Heartbeat` RPCs are unconditional error
//! stubs) and the placement layer it used to feed is deleted; it
//! remains only because the admin/operator surfaces — owned by the
//! next deletion commit — still read the `executors` map and their
//! tests still drive these arms. The warm-gate/prefetch half retired
//! with the placement layer. Periodic `tick_*` housekeeping lives in
//! [`super::housekeeping`].
// r[impl sched.executor.dual-register]

use std::time::Instant;

use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::state::{DerivationStatus, DrvHash, ExecutorId, ExecutorState};

use super::{DagActor, DrainResult, HeartbeatPayload};

impl DagActor {
    // -----------------------------------------------------------------------
    // Worker management
    // -----------------------------------------------------------------------

    /// Returns `Ok(())` when the stream is accepted, `Err(reason)` when
    /// the live-stream / intent-mismatch hijack guards reject. The gRPC
    /// handler awaits this via the `ExecutorConnected.reply` oneshot
    /// BEFORE spawning the `worker-stream-reader` task — on `Err`, the
    /// reader is never spawned with a body-supplied `executor_id`, so a
    /// spoofed `Register{executor_id=E_victim}` cannot forward
    /// `ProcessCompletion{E_victim}`.
    pub(super) fn handle_worker_connected(
        &mut self,
        executor_id: &ExecutorId,
        stream_tx: mpsc::Sender<rio_proto::types::SchedulerMessage>,
        stream_epoch: u64,
        auth_intent: Option<String>,
    ) -> Result<(), &'static str> {
        info!(executor_id = %executor_id, stream_epoch, "worker stream connected");

        let entry = self.executors.entry(executor_id.clone());
        let is_reconnect = matches!(entry, std::collections::hash_map::Entry::Occupied(_));
        let worker = entry.or_insert_with(|| ExecutorState::new(executor_id.clone()));

        // r[impl sec.executor.identity-token+2]
        // Two reconnect rejections that together prevent stream hijack:
        //
        // 1. Existing stream still live → drop the NEW one. A
        //    compromised builder opening a stream as another
        //    executor's id would otherwise replace `stream_tx` and
        //    receive its next `WorkAssignment.assignment_token`. A
        //    legitimate same-process reconnect only happens after the
        //    OLD stream's bridge task exited (output_rx dropped →
        //    actor_rx dropped → `is_closed()`). Builder retries with
        //    1s backoff; the I-056a race below is sub-ms.
        //
        // 2. HMAC-attested intent doesn't match the stored
        //    `auth_intent` → drop. `auth_intent` was set by a prior
        //    token-gated connect and is NEVER mutated by heartbeat
        //    (unlike `intent_id`, which is dispatch-downgradeable);
        //    a token for a different intent cannot take over. (1)
        //    alone leaves a window between disconnect and reconnect;
        //    (2) closes it.
        if is_reconnect {
            if worker.stream_tx.as_ref().is_some_and(|tx| !tx.is_closed()) {
                warn!(
                    executor_id = %executor_id,
                    "reconnect with existing live stream; rejecting new stream \
                     (hijack guard — legitimate reconnect retries after the \
                     old bridge task exits)"
                );
                metrics::counter!("rio_scheduler_executor_reconnect_rejected_total",
                                  "reason" => "live_stream")
                .increment(1);
                return Err("live stream");
            }
            if auth_intent.is_some()
                && worker.auth_intent.is_some()
                && worker.auth_intent != auth_intent
            {
                warn!(
                    executor_id = %executor_id,
                    stored = ?worker.auth_intent,
                    presented = ?auth_intent,
                    "reconnect with mismatched executor-token intent; rejecting"
                );
                metrics::counter!("rio_scheduler_executor_reconnect_rejected_total",
                                  "reason" => "intent_mismatch")
                .increment(1);
                return Err("intent mismatch");
            }
        }
        // Accept path: stamp `stream_epoch` HERE, not above the reject
        // guards. A rejected (spoofed) reconnect would otherwise
        // overwrite the legit stream's epoch, causing its eventual
        // `ExecutorDisconnected` to be dropped as stale (I-056a check
        // in `handle_executor_disconnected`) — entry never cleaned,
        // `running_build` never reassigned.
        worker.stream_epoch = stream_epoch;
        // Stamp the attested intent immediately so dispatch can match
        // before the first heartbeat lands, and so a later reconnect
        // attempt with a different intent fails check (2) above.
        // `auth_intent` is the immutable identity key (read by the
        // reconnect guard and `handle_heartbeat`'s spoof check);
        // `intent_id` is the dispatch-reservation hint (heartbeat may
        // downgrade it to None when the drv leaves Ready).
        if auth_intent.is_some() {
            worker.auth_intent.clone_from(&auth_intent);
            worker.intent_id = auth_intent;
        }

        // I-056a: clear scheduler-side `draining` and `store_degraded`
        // on reconnect. We're here because the disconnect signal
        // didn't fire (old stream's task still in TCP/h2 close
        // handshake when the new stream's connect arrived). Both flags
        // reflect prior-session state — stale. Live: fetchers stuck
        // 22 min after deploy churn drained them; only restart
        // cleared it. The late-disconnect half of that race is the
        // `stream_epoch` check in `handle_executor_disconnected`: the
        // old reader's `ExecutorDisconnected` carries the prior epoch
        // and is ignored once this assignment overwrote it.
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
        // `stream_epoch` and `stream_tx` are paired: the epoch
        // identifies the stream stored in `stream_tx`. Written
        // together AFTER the rejection guards above so a rejected
        // reconnect cannot leave `{stream_tx=TX_legit, stream_epoch=
        // E_rejected}` — the gRPC handler unconditionally spawns a
        // reader that fires `ExecutorDisconnected{stream_epoch}` on
        // close, and a corrupted epoch would let the rejected
        // stream's disconnect evict the legitimate worker.
        worker.stream_epoch = stream_epoch;
        worker.stream_tx = Some(stream_tx);

        if !was_registered && worker.is_registered() {
            info!(executor_id = %executor_id, "worker fully registered (stream + heartbeat)");
            metrics::gauge!("rio_scheduler_workers_active").increment(1.0);
        }
        Ok(())
    }

    pub(super) async fn handle_executor_disconnected(
        &mut self,
        executor_id: &ExecutorId,
        stream_epoch: u64,
    ) {
        let Some(worker) = self.executors.get(executor_id) else {
            return; // unknown worker, no-op (and no gauge decrement)
        };
        // r[impl sched.executor.session-epoch]
        // I-056a late-disconnect half: connect-before-disconnect
        // ordering happens in production (old reader task still in
        // TCP/h2 close handshake when the new stream's connect
        // arrived). Without this guard the late `ExecutorDisconnected`
        // from the OLD reader removes the freshly-reconnected entry —
        // `tx_NEW` is dropped (worker churns through another
        // reconnect), `running_build` is spuriously reassigned, and
        // `rio_scheduler_worker_disconnects_total` over-counts.
        if worker.stream_epoch != stream_epoch {
            debug!(
                executor_id = %executor_id,
                stale = stream_epoch,
                current = worker.stream_epoch,
                "stale ExecutorDisconnected from prior stream — ignoring (I-056a late-half)"
            );
            return;
        }
        info!(executor_id = %executor_id, stream_epoch, "worker disconnected");

        let worker = self
            .executors
            .remove(executor_id)
            .expect("checked get() above");

        // §13a interim ICE-clear bookkeeping: drop the entry so a
        // never-registered pod's stale cell can't be cleared by a
        // later heartbeat for the same drv on a different cell.
        // Disconnect itself is NOT a success signal — no `ice.clear`.
        if let Some(intent) = &worker.auth_intent {
            self.dispatched_cells.remove(intent.as_str());
        }

        // Only decrement if worker was fully registered (stream + heartbeat).
        // Otherwise the gauge goes negative for workers that connected a stream
        // but never sent a heartbeat (increment fires on full registration only).
        let was_registered = worker.is_registered();

        // Reassign whatever was on this worker. The worker is gone;
        // whether it was draining or not doesn't matter now.
        //
        // Disconnect does NOT bump `resource_floor` and does not append
        // an attempt row: the stream-era disconnect/termination
        // correlation (the `recently_disconnected` second-installment
        // path) is gone with the session machinery. Pull-mode attempts
        // are durable rows owned by the establishment sweep; this arm
        // only services the (production-unreachable) stream test
        // plumbing that remains until the placement layer retires.
        let to_reassign: Vec<DrvHash> = worker.running_build.into_iter().collect();
        self.reassign_derivations(&to_reassign, Some(executor_id))
            .await;

        if was_registered {
            metrics::gauge!("rio_scheduler_workers_active").decrement(1.0);
        }
        metrics::counter!("rio_scheduler_worker_disconnects_total").increment(1);
    }

    /// E5's poison-threshold re-check as a `decide()` caller (Phase 1b,
    /// T-1b.8): fold the derivation's durable attempt suffix plus the
    /// transitional legacy mirror-column seed (P5) and report whether
    /// the verdict is a threshold poison. Read-only — this site appends
    /// nothing and charges nothing for the disconnect itself (the
    /// no-report establishment charge lands at the TTL sweep / backstop,
    /// T-1b.11, not here); the disconnect / force-drain / backstop rows
    /// it folds over were appended by their observation sites before
    /// this runs.
    ///
    /// Acts only on the threshold reason — exactly the check
    /// `PoisonConfig::is_poisoned` performed over the RAM counters —
    /// so the re-check stays a *threshold* re-check; every other budget
    /// is owned by its observation site's own collapsed verdict.
    ///
    /// On a read failure (or an uncommitted merge with no derivations
    /// row) the in-memory attempt history — the committed-suffix
    /// mirror — is the fallback fold input, so the re-check never goes
    /// silent on a PG blip (the as-built check was RAM-only and never
    /// touched PG).
    async fn reassign_threshold_recheck(&self, drv_hash: &DrvHash) -> bool {
        let Some(state) = self.dag.node(drv_hash) else {
            return false;
        };
        let budget = self.decision_budget();
        let now = crate::db::attempts::epoch_now() as crate::retry_policy::AbsTime;
        let decision = match state.db_id {
            Some(derivation_id) => {
                let read: Result<crate::retry_policy::Decision, sqlx::Error> = async {
                    let mut conn = self.db.pool().acquire().await?;
                    let suffix = crate::db::SchedulerDb::load_attempt_suffix_one_in_tx(
                        &mut conn,
                        derivation_id,
                    )
                    .await?;
                    let seed =
                        crate::db::SchedulerDb::load_retry_seed_in_tx(&mut conn, derivation_id)
                            .await?;
                    let history: Vec<crate::state::AttemptRecord> = suffix
                        .iter()
                        .map(crate::db::attempts::AttemptRow::to_record)
                        .collect();
                    Ok(crate::retry_policy::decide(
                        &history,
                        &budget,
                        now,
                        seed.as_ref(),
                    ))
                }
                .await;
                match read {
                    Ok(decision) => decision,
                    Err(e) => {
                        warn!(drv_hash = %drv_hash, error = %e,
                              "reassign re-check: suffix read failed; folding the in-memory \
                               attempt history instead");
                        crate::retry_policy::decide(state.attempt_history(), &budget, now, None)
                    }
                }
            }
            None => crate::retry_policy::decide(state.attempt_history(), &budget, now, None),
        };
        matches!(
            decision.verdict,
            crate::retry_policy::Verdict::Poison(crate::retry_policy::PoisonReason::Threshold)
        )
    }

    /// Reset a set of derivations to Ready and re-enqueue.
    ///
    /// Extracted from `handle_executor_disconnected` so `handle_drain_executor`
    /// (force=true) can reuse it. Both callers have already decided these
    /// derivations should be retried elsewhere — this is the mechanism.
    ///
    /// Leader-gated at the top: the routes in — worker disconnect (stream
    /// loss via the ungated ExecutorDisconnected arm, heartbeat timeout via
    /// handle_tick), force-drain (ungated DrainExecutor arm), and the tick
    /// backstop — all converge on this function, so it is the single
    /// chokepoint keeping a deposed leader from writing
    /// poison/Ready/terminal-log state from a stale DAG
    /// (r[sched.lease.standby-drops-writes]).
    ///
    /// `reset_to_ready()` handles both Assigned → Ready and Running →
    /// Failed → Ready. A derivation in any other state (Completed,
    /// Poisoned, DepFailed) is skipped with a warn — it shouldn't be in
    /// `running_build` but split-brain or delayed heartbeat reconcile
    /// can produce it.
    ///
    /// Disconnect does NOT bump `resource_floor` and does NOT
    /// record into `failed_builders`/`failure_count`/`retry_count`. The
    /// controller is authoritative on termination reason: it calls
    /// `ReportExecutorTermination` with the k8s OOMKilled/Evicted
    /// signal ~1-3s later, and ONLY OomKilled/EvictedDiskPressure
    /// promote. A bare disconnect is ambiguous (pod-kill, node failure,
    /// store-replica-restart, operator delete) — none are the build's
    /// fault, none are sizing signals. The previous I-173/I-177/I-197
    /// disconnect-promote heuristic over-fired: live QA showed cmake
    /// going medium→large→xlarge from a pod-kill + store-replica-
    /// restart with zero builds run.
    ///
    /// Builds that genuinely fail send a `CompletionReport` BEFORE
    /// disconnecting (worker catches the failure) → `handle_transient_
    /// failure` / `handle_permanent_failure` records + poison-checks
    /// there. The "drv that crashes 3 workers should poison" property
    /// is preserved on those paths.
    ///
    /// `lost_worker`: kept for the existing-poison-state check (3 prior
    /// REAL failures + 1 disconnect → poison instead of dispatching a
    /// 4th time) and for logging.
    // r[impl sched.reassign.no-promote-on-ephemeral-disconnect+4]
    pub(super) async fn reassign_derivations(
        &mut self,
        drv_hashes: &[DrvHash],
        lost_worker: Option<&ExecutorId>,
    ) {
        // r[impl sched.lease.standby-drops-writes]
        // Same defense-in-depth as the ProcessCompletion/CancelBuild arm
        // gates (mod.rs), placed HERE because the ExecutorDisconnected /
        // DrainExecutor arms must stay ungated (executors-map + gauge
        // bookkeeping runs on standby). A deposed leader processing a
        // disconnect against its stale DAG would otherwise:
        //   - poison branch: persist_poisoned + terminal_failure_epilogue
        //     → terminal_log_epilogue, which stamps the drv_executions row and
        //     pins the write-once build_derivations.exec_id for an execution
        //     the new leader is about to re-run (and, keep_going=false,
        //     fails/cancels the whole build from stale state);
        //   - reset branch: persist_status(Ready), racing the new leader's
        //     recovery.
        // The new leader's recovery + reconcile/orphan sweeps own the lost
        // worker's derivations; in-memory state here is cleared by LeaderLost.
        if !self.leader.is_leader() {
            if !drv_hashes.is_empty() {
                warn!(
                    drvs = drv_hashes.len(),
                    lost_worker = ?lost_worker,
                    "dropping reassign_derivations: not leader \
                     (new leader's recovery owns these derivations)"
                );
            }
            return;
        }
        let mut affected: std::collections::HashSet<Uuid> = Default::default();
        for drv_hash in drv_hashes {
            // Re-read existing poison state so 3 prior REAL failures
            // (recorded by handle_transient_failure) + this disconnect
            // → poison instead of dispatching a 4th time. Disconnect
            // itself never increments the count.
            //
            // E5, collapsed onto decide() (Phase 1b, T-1b.8): the
            // threshold re-check folds the durable attempt suffix (plus
            // the transitional legacy seed) instead of reading the RAM
            // counters; verdict-identical on every single-tenure history
            // reachable today. Kept rather than deleted (decision P2,
            // the narrowed b09c5b312-X6 disposition): the backstop's
            // poison verdict no longer depends on this check since the
            // E8 collapse decides at its own site, but it remains the
            // disconnect-time and force-drain-time re-poison path and
            // the post-failover backstop for a lost persist_poisoned
            // write.
            let should_poison = self.reassign_threshold_recheck(drv_hash).await;
            if should_poison {
                info!(drv_hash = %drv_hash, lost_worker = ?lost_worker,
                      "reassign: poison threshold reached, poisoning instead of retry");
                self.poison_and_cascade(
                    drv_hash,
                    "poison threshold reached on worker disconnect after prior failures",
                    None,
                    None,
                )
                .await;
                continue;
            }

            // Disconnect does NOT bump `resource_floor` — the
            // controller's follow-up `ReportExecutorTermination` is
            // authoritative on whether the cause was a sizing signal.
            if let Some(state) = self.dag.node_mut(drv_hash) {
                if let Err(e) = state.reset_to_ready() {
                    warn!(
                        drv_hash = %drv_hash, error = %e,
                        "invalid state for reassignment, skipping"
                    );
                    continue;
                }
                self.persist_status(drv_hash, DerivationStatus::Ready, None)
                    .await;
                affected.extend(self.get_interested_builds(drv_hash));
                self.push_ready(drv_hash.clone());
            }
        }
        // Dashboard: running count dropped; assigned_executors lost
        // this worker. Without emit_progress here, a quiet build shows
        // stale state until the next unrelated dispatch/completion.
        // Done at the chokepoint so every caller (disconnect, force-
        // drain, backstop-timeout) gets it for free and future callers
        // can't repeat the omission. `poison_and_cascade` emits its
        // own events; only the reset-to-Ready arm needs the explicit
        // emit.
        for build_id in affected {
            self.emit_progress(build_id);
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
                let mut sent: u64 = 0;
                for drv_hash in &to_reassign {
                    let Some(drv_path) = self.dag.node(drv_hash).map(|s| s.drv_path().to_string())
                    else {
                        continue;
                    };
                    match tx.try_send(rio_proto::types::SchedulerMessage {
                        msg: Some(rio_proto::types::scheduler_message::Msg::Cancel(
                            rio_proto::types::CancelSignal {
                                drv_path,
                                reason: "worker draining (forced)".into(),
                            },
                        )),
                    }) {
                        Ok(()) => sent += 1,
                        Err(e) => {
                            debug!(executor_id = %executor_id, drv_hash = %drv_hash, error = %e,
                                   "cancel signal dropped (stream full/closed)");
                            metrics::counter!("rio_scheduler_cancel_signal_dropped_total")
                                .increment(1);
                        }
                    }
                }
                // cancel_signals_total counts signals DELIVERED — see
                // build.rs / housekeeping.rs.
                if sent > 0 {
                    info!(
                        executor_id = %executor_id,
                        count = sent,
                        "sent CancelSignal for force-drain (preemption)"
                    );
                    metrics::counter!("rio_scheduler_cancel_signals_total").increment(sent);
                }
            }

            // 1a: the taken in-flight execution's ledger row, complete
            // in ONE installment (`termination_reason='force_drain'`):
            // a force-drain has no follow-up classification path, so
            // it must never sit unclassified, and nothing later charges
            // it (as-built force-drain charges nothing). Appended
            // before the reassign clears the exec_id carrier; the
            // worker's subsequent real disconnect finds running_build
            // already taken and appends nothing further.
            for drv in &to_reassign {
                let row = self
                    .attempt_row_for(
                        drv,
                        crate::state::OutcomeClass::Disconnected,
                        crate::state::ReportingParty::Scheduler,
                    )
                    .map(|mut r| {
                        r.executor_id = Some(executor_id.clone());
                        r.termination_reason = Some("force_drain".to_string());
                        r
                    });
                self.append_attempt_standalone(drv, row).await;
            }

            // Reassign. Worker later sends CompletionReport{Cancelled};
            // completion handler's Cancelled arm is a no-op (status
            // already Ready after reassign, not Assigned/Running — the
            // "not in assigned/running state, ignoring" warn fires.
            // That's fine — the warn documents the expected behavior).
            //
            // Pass the drained worker's ID so reassigned derivations
            // Force-drain is operator-initiated (or controller-driven
            // preemption), NOT a sizing signal — re-queue at current
            // floor only, same as bare disconnect.
            self.reassign_derivations(&to_reassign, Some(executor_id))
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

    /// Reconcile one heartbeat into the (production-unreachable)
    /// executors map. Kept only as the admin surfaces' test plumbing
    /// until commit C re-points them; the became-idle inline-dispatch
    /// edge this used to report retired with the placement layer.
    pub(super) fn handle_heartbeat(&mut self, hb: HeartbeatPayload) {
        let executor_id = &hb.executor_id;
        // r[impl sched.executor.session-epoch]
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
            return;
        }
        // r[impl sec.executor.identity-token+2]
        // Bind the heartbeat to the executor entry: `hb.intent_id` is
        // token-attested (gRPC bound it to the caller's token at
        // executor_service.rs); the stored `auth_intent` was token-
        // attested at connect. Mismatch = cross-executor spoof
        // (compromised pod A heartbeating as B with A's own intent).
        // None on either side = dev-mode / Static-sized → permissive.
        // Runs BEFORE any mutation (reconcile_running_build, intent_id
        // overwrite, draining_hb/store_degraded) so a spoof cannot
        // clear the victim's slot or flag-flip it.
        if let Some(worker) = self.executors.get(executor_id.as_str())
            && let Some(stored) = &worker.auth_intent
            && let Some(presented) = &hb.intent_id
            && stored != presented
        {
            warn!(
                executor_id = %executor_id, stored = %stored, presented = %presented,
                "heartbeat intent mismatch vs stored auth_intent; dropping \
                 (cross-executor spoof?)"
            );
            metrics::counter!("rio_scheduler_heartbeat_rejected_total",
                              "reason" => "intent_mismatch")
            .increment(1);
            return;
        }

        // TOCTOU fix: a stale heartbeat must not clobber a fresh assignment.
        // The scheduler is authoritative for what it assigned. We reconcile:
        //   - Keep the scheduler-known build if it is still Assigned/Running
        //     in the DAG (heartbeat may predate the assignment).
        //   - Accept a heartbeat-reported build we don't know about, but warn
        //     (shouldn't happen; indicates split-brain or restart).
        //   - Clear if absent from heartbeat AND DAG state is no longer
        //     Assigned/Running (completion already processed).
        let reconciled = self.reconcile_running_build(executor_id, hb.running_build);

        // intent_id: DOWNGRADE to None if it doesn't point at a
        // currently-Ready drv. Computed here (before `get_mut`) so the
        // dag read doesn't overlap the executors borrow. See the
        // assignment site below for rationale.
        let intent_id = hb.intent_id.filter(|id| {
            self.dag
                .node(id)
                .is_some_and(|s| s.status() == DerivationStatus::Ready)
        });

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
        worker.running_build = reconciled;
        // intent_id: the pod annotation is immutable post-create, but
        // the scheduler may re-plan (drv completed elsewhere, scheduler
        // restarted) before this pod heartbeats. `rejection_reason()`
        // treats `Some(X)` as an exclusive reservation for X; without
        // the not-Ready→None downgrade above, a stale-intent worker
        // would be rejected for everything and idle until
        // activeDeadlineSeconds. After the downgrade it falls through
        // to pick-from-queue like a Static-sized pod.
        worker.intent_id = intent_id;
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

        if !was_registered && worker.is_registered() {
            let auth_intent = worker.auth_intent.clone();
            info!(executor_id = %executor_id, "worker fully registered (heartbeat + stream)");
            metrics::gauge!("rio_scheduler_workers_active").increment(1.0);
            // r[impl sched.sla.hw-class.ice-mask]
            // §13a interim ICE clear: heartbeat ⇒ pod scheduled ⇒
            // ∃ cell ∈ A' with capacity. |A'|=1 ⇒ that cell — clear
            // it. |A'|>1 ⇒ heartbeat identifies none (the pod's
            // affinity is OR-of-A'; kube-scheduler picked SOME term);
            // over-clear defeats `ice_step_doubles` (bug_030).
            // `registered_cells` (A18 NodeClaim watcher) is the
            // per-cell signal. `auth_intent` is the token-attested
            // drv hash (immutable, NOT the dispatch-downgradeable
            // `intent_id`); `dispatched_cells` was armed at
            // `handle_ack_spawned_intents` (arm-on-ack — emit path is
            // read-only). **DELETE this block at A18**
            // (`registered_cells` covers |A'|=1 too).
            if let Some(intent) = auth_intent
                && let Some((_, cells)) = self.dispatched_cells.remove(intent.as_str())
                && let [cell] = cells.as_slice()
            {
                self.ice.clear(cell);
            }
        }
    }

    /// TOCTOU reconcile: a stale heartbeat must not clobber a fresh
    /// assignment. The scheduler is authoritative for what it assigned.
    ///   - Keep the scheduler-known build if it is still
    ///     Assigned/Running in the DAG on this executor (the heartbeat
    ///     may predate the assignment).
    ///   - Otherwise mirror the worker's own claim of being busy
    ///     (worker-side capacity bookkeeping only — no DAG mutation).
    ///   - Clear if absent from heartbeat AND DAG state is no longer
    ///     Assigned/Running (completion already processed).
    ///
    /// The stream-era heartbeat repair arms that used to live here
    /// (DAG-side adoption of unknown builds, the two-strike phantom
    /// drain) are gone with the session machinery: pull-mode attempts
    /// are durable rows owned by the report intake and the
    /// establishment sweep, not by heartbeat reconciliation.
    fn reconcile_running_build(
        &mut self,
        executor_id: &ExecutorId,
        running_build: Option<String>,
    ) -> Option<DrvHash> {
        // Worker reports a drv_path; resolve to a drv_hash via the DAG
        // index. The gRPC layer already rejected >1 entry (P0537).
        let heartbeat_hash: Option<DrvHash> =
            running_build.and_then(|path| self.dag.hash_for_path(&path).cloned());

        let prev_running = self
            .executors
            .get(executor_id.as_str())
            .and_then(|w| w.running_build.clone());

        // Keep the scheduler-assigned build if still in-flight ON THIS
        // EXECUTOR. The ownership check keeps a stale entry from
        // surviving after the DAG re-assigned the derivation elsewhere.
        let prev_kept: Option<DrvHash> = prev_running.as_ref().and_then(|h| {
            self.dag
                .node(h)
                .is_some_and(|s| {
                    matches!(
                        s.status(),
                        DerivationStatus::Assigned | DerivationStatus::Running
                    ) && s.assigned_executor.as_ref() == Some(executor_id)
                })
                .then(|| h.clone())
        });
        // No scheduler-side record: mirror the worker's claim so
        // dispatch sees the slot as occupied. Worker-side bookkeeping
        // only — the DAG is never mutated from a heartbeat.
        match (&prev_kept, &heartbeat_hash) {
            (None, Some(hb)) => Some(hb.clone()),
            (kept, _) => kept.clone(),
        }
    }
}
