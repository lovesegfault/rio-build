//! Executor lifecycle: connect/disconnect/register, heartbeat
//! reconcile, phantom-drain. Periodic `tick_*` housekeeping lives in
//! [`super::housekeeping`].
// r[impl sched.executor.dual-register]
// r[impl sched.executor.deregister-reassign]

use std::collections::HashMap;
use std::time::Instant;

use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::dag::DerivationDag;
use crate::state::{DerivationStatus, DrvHash, ExecutorId, ExecutorState};

/// Initial-hint budget: max store paths to send in the registration-
/// time `PrefetchHint`. A broad common-set (glibc, stdenv, etc.) is the
/// intent, not the entire queue's closure. See [`on_worker_registered`].
///
/// [`on_worker_registered`]: DagActor::on_worker_registered
use super::{DagActor, DrainResult, HeartbeatPayload, MAX_PREFETCH_PATHS};

/// How long a `recently_disconnected` entry waits for the controller's
/// `ReportExecutorTermination` before being swept. The controller
/// observes Pod-status + reconciles ~1-3s after the gRPC stream drops;
/// `JOB_TTL_SECS=600` keeps the Pod around that long. 60s covers
/// apiserver lag + one missed reconcile tick (~10s) with margin. Past
/// this, a missed report degrades to "one OOM doesn't promote" — the
/// next OOM on the same drv will (the floor is sticky).
pub(super) const TERMINATION_REPORT_TTL: std::time::Duration = std::time::Duration::from_secs(60);

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
        // 2. HMAC-attested intent doesn't match the stored one →
        //    drop. The stored `intent_id` was set by a token-gated
        //    heartbeat or a prior connect; a token for a different
        //    intent cannot take over. (1) alone leaves a window
        //    between disconnect and reconnect; (2) closes it.
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
                && worker.intent_id.is_some()
                && worker.intent_id != auth_intent
            {
                warn!(
                    executor_id = %executor_id,
                    stored = ?worker.intent_id,
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
        if auth_intent.is_some() {
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
        worker.stream_tx = Some(stream_tx);

        if !was_registered && worker.is_registered() {
            info!(executor_id = %executor_id, "worker fully registered (stream + heartbeat)");
            metrics::gauge!("rio_scheduler_workers_active").increment(1.0);
            self.on_worker_registered(executor_id);
        }
        Ok(())
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

    pub(super) async fn handle_executor_disconnected(
        &mut self,
        executor_id: &ExecutorId,
        stream_epoch: u64,
        seen_drvs: Vec<String>,
    ) {
        let Some(worker) = self.executors.get(executor_id) else {
            return; // unknown worker, no-op (and no gauge decrement)
        };
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

        // Only decrement if worker was fully registered (stream + heartbeat).
        // Otherwise the gauge goes negative for workers that connected a stream
        // but never sent a heartbeat (increment fires on full registration only).
        let was_registered = worker.is_registered();

        // Reassign whatever was on this worker. The worker is gone;
        // whether it was draining or not doesn't matter now.
        //
        // Disconnect does NOT bump `resource_floor` — the
        // controller is authoritative on termination reason
        // (`ReportExecutorTermination` with k8s OOMKilled/DiskPressure
        // → promote; anything else → no-op). A bare disconnect is
        // ambiguous: pod-kill, store-replica-restart, node failure are
        // all NOT sizing signals. Live QA: cmake went medium→large→
        // xlarge from a pod-kill + store-replica-restart with zero
        // builds run; floor is sticky (M_032). I-197's
        // `last_completed` discriminator is kept ONLY to decide
        // whether to record a `recently_disconnected` entry (expected
        // one-shot exit → no entry; the controller will report
        // `Completed` and we'd ignore it anyway, so skip the map
        // churn).
        let lost_last_completed = worker.last_completed.clone();
        let to_reassign: Vec<DrvHash> = worker.running_build.into_iter().collect();
        // Record for the controller's follow-up report. Only when the
        // worker died MID-BUILD (last_completed != running_build) — an
        // expected one-shot exit needs no entry.
        if let Some(drv) = to_reassign.first()
            && lost_last_completed.as_ref() != Some(drv)
        {
            self.recently_disconnected
                .insert(executor_id.clone(), (drv.clone(), Instant::now()));
        }
        self.reassign_derivations(&to_reassign, Some(executor_id))
            .await;

        if was_registered {
            metrics::gauge!("rio_scheduler_workers_active").decrement(1.0);
        }
        metrics::counter!("rio_scheduler_worker_disconnects_total").increment(1);

        // Log-buffer cleanup, AFTER the epoch check above (a stale
        // reader's `seen_drvs` reaches here only when the epoch
        // matched) and with ownership awareness. Discard ONLY paths
        // the DAG has never heard of — fabricated by an untrusted
        // worker (the `MAX_DRVS_PER_STREAM` cap bounds count, not
        // which paths) or post-cleanup. Real drvs are reaped by the
        // existing machinery: `seal()` on completion, `discard()` on
        // next `assign_to_worker`, `discard()` in
        // `handle_cleanup_terminal_build`. This was previously in the
        // reader task, branching on `is_sealed`; that raced the
        // actor's `seal()` (TOCTOU under load → completed build's
        // buffer discarded before flusher drained it) and had no
        // ownership check (compromised worker → discard a victim's
        // buffer by sending one `LogBatch{derivation_path=D_victim}`
        // then disconnecting).
        if let Some(bufs) = &self.log_buffers {
            for drv in seen_drvs {
                if self.dag.hash_for_path(&drv).is_none() {
                    bufs.discard(&drv);
                }
            }
        }
    }

    /// Reset a set of derivations to Ready and re-enqueue.
    ///
    /// Extracted from `handle_executor_disconnected` so `handle_drain_executor`
    /// (force=true) can reuse it. Both callers have already decided these
    /// derivations should be retried elsewhere — this is the mechanism.
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
        let mut affected: std::collections::HashSet<Uuid> = Default::default();
        for drv_hash in drv_hashes {
            // Re-read existing poison state so 3 prior REAL failures
            // (recorded by handle_transient_failure) + this disconnect
            // → poison instead of dispatching a 4th time. Disconnect
            // itself never increments the count.
            let should_poison = self
                .dag
                .node(drv_hash)
                .map(|s| self.poison_config.is_poisoned(s))
                .unwrap_or(false);
            if should_poison {
                info!(drv_hash = %drv_hash, lost_worker = ?lost_worker,
                      "reassign: poison threshold reached, poisoning instead of retry");
                self.poison_and_cascade(
                    drv_hash,
                    "poison threshold reached on worker disconnect after prior failures",
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

    // r[impl sched.sla.reactive-floor+2]
    /// Controller-reported Pod termination reason. `OomKilled` /
    /// `EvictedDiskPressure` / `DeadlineExceeded` → bump the relevant
    /// `resource_floor` dimension (D4). Other reasons → no-op (log
    /// only).
    ///
    /// Resolves `executor_id → drv_hash` via `recently_disconnected`
    /// (populated by `handle_executor_disconnected` when the worker died
    /// mid-build). The entry is `remove()`d on first report — the
    /// controller's reconcile loop re-reports the same Pod every ~10s
    /// for `JOB_TTL_SECS=600`; subsequent reports miss the map and
    /// no-op. If the report races AHEAD of the disconnect (controller
    /// observed Pod-status before the gRPC stream broke at the
    /// scheduler — rare), fall back to the still-live executor's
    /// `running_build` and set its `last_completed` so the imminent
    /// `handle_executor_disconnected` skips the `recently_disconnected`
    /// insert (otherwise the controller's ~10s re-report would find the
    /// entry and double-bump).
    pub(super) async fn handle_executor_termination(
        &mut self,
        executor_id: &ExecutorId,
        reason: rio_proto::types::TerminationReason,
    ) -> bool {
        use rio_proto::types::TerminationReason as R;

        // r[impl sched.termination.deadline-exceeded+2]
        // DeadlineExceeded is reported by JOB name (the Job controller
        // deletes the Pod when activeDeadlineSeconds fires, so the
        // controller never sees a terminated container). Prefix-match
        // recently_disconnected: pod name = `{job}-{5char}`.
        if reason == R::DeadlineExceeded {
            return self.handle_deadline_exceeded(executor_id).await;
        }

        // Non-promoting reason → return BEFORE touching
        // `recently_disconnected` or the race-ahead `executors`
        // lookup. Without this gate a `Completed`/`Error`/
        // `EvictedOther` report consumes the dedup entry then no-ops
        // — the same-tick `report_deadline_exceeded_jobs` prefix-match
        // (or a later promoting report) finds nothing, so the
        // `r[ctrl.terminated.deadline-exceeded]` backstop is
        // structurally defeated and a deterministically-wedging drv
        // loops at `activeDeadlineSeconds` instead of climbing the
        // floor ladder. Same hazard for the race-ahead arm: a non-
        // promoting report would set `last_completed` and suppress
        // the imminent disconnect's `recently_disconnected` insert.
        let Some(label) = super::floor::reason_label(reason) else {
            debug!(executor_id = %executor_id, ?reason,
                   "termination report: non-resource reason, no promotion");
            return false;
        };

        // Resolve drv. remove() = first-report-wins dedup.
        let drv_hash = match self.recently_disconnected.remove(executor_id) {
            Some((drv, _)) => drv,
            // Race-ahead: report landed before ExecutorDisconnected.
            // Look at the still-live executor.
            None => match self.executors.get_mut(executor_id) {
                Some(w) => match w.running_build.clone() {
                    Some(drv) => {
                        // Mark as termination-handled so the imminent
                        // ExecutorDisconnected's `last_completed !=
                        // running_build` guard skips the
                        // recently_disconnected insert. Otherwise the
                        // controller's ~10s re-report finds the entry
                        // and double-bumps (4× provisioning, or
                        // infra_count+=2 at ceiling → premature poison).
                        w.last_completed = Some(drv.clone());
                        drv
                    }
                    None => {
                        debug!(executor_id = %executor_id, ?reason,
                               "termination report: executor live but idle, ignoring");
                        return false;
                    }
                },
                None => {
                    debug!(executor_id = %executor_id, ?reason,
                           "termination report: no recently_disconnected entry \
                            (already handled, swept, or expected one-shot exit)");
                    return false;
                }
            },
        };

        let outcome = self.bump_resource_floor(&drv_hash, reason, label).await;

        // m044: at the ceiling (`at_cap=true`), this path owns the cap
        // check + increment + poison. Kubelet-level OOMKilled /
        // EvictedDiskPressure means the pod died — there is no
        // worker-reported `CompletionReport`, so
        // `handle_infrastructure_failure`'s cap check never fires and
        // the build loops at the ceiling forever otherwise. Mirrors
        // completion.rs `handle_infrastructure_failure`: cap-check
        // BEFORE increment, so at-cap poisons at the same attempt
        // number as non-floor infra. Guard on `at_cap` (resource
        // reasons only — non-resource returned early via
        // reason_label=None) AND a poison-able status: the disconnect
        // already re-queued (Ready), but a dispatch tick may have
        // raced (Assigned/Running); any other state (Completed
        // elsewhere, already Poisoned) → skip.
        let max = self.retry_policy.max_infra_retries;
        if outcome.at_cap
            && let Some(state) = self.dag.node_mut(&drv_hash)
            && matches!(
                state.status(),
                DerivationStatus::Ready | DerivationStatus::Assigned | DerivationStatus::Running
            )
        {
            if state.retry.infra_count >= max {
                warn!(
                    drv_hash = %drv_hash, executor_id = %executor_id, ?reason,
                    infra_retry_count = state.retry.infra_count, max,
                    resource_floor = ?state.sched.resource_floor,
                    "controller-reported termination: max_infra_retries \
                     exhausted at ceiling, poisoning"
                );
                self.poison_and_cascade(
                    &drv_hash,
                    &format!("max_infra_retries={max} exhausted at resource ceiling ({reason:?})"),
                )
                .await;
                return false;
            }
            state.retry.infra_count += 1;
        }

        outcome.promoted
    }

    /// `activeDeadlineSeconds` backstop fired — worker was too wedged
    /// to fire its own `daemon_timeout`. Bump `resource_floor.
    /// deadline_secs` (D4; `bump_floor_or_count` handles the
    /// `timeout_count` increment at the cap — same bounded-ladder
    /// semantics as `handle_timeout_failure`,
    /// `r[sched.timeout.promote-on-exceed+2]`).
    ///
    /// `job_name` is the JOB name; the Pod is already deleted.
    /// Prefix-match `recently_disconnected` (pod name = `{job}-{5}`).
    /// remove() = first-report-wins dedup, same as the exact-match
    /// path. Unlike `handle_timeout_failure`, this does NOT
    /// `reset_to_ready` — `handle_executor_disconnected` already re-
    /// queued, and the drv may already be re-dispatched.
    ///
    /// At the 24h cap (`counted=true`), `timeout_count >=
    /// max_timeout_retries` poisons HERE — the backstop fires
    /// precisely when the worker is too wedged (FUSE/kernel hang) to
    /// send `CompletionReport{TimedOut}`, so `handle_timeout_failure`'s
    /// cap-check never runs and a deterministically-wedging drv would
    /// otherwise re-queue at 24h forever (`is_poisoned()` reads
    /// `failure_count`/`failed_builders`, never `timeout_count`).
    /// Mirrors the m044 OOM/DiskPressure cap-check above.
    async fn handle_deadline_exceeded(&mut self, job_name: &ExecutorId) -> bool {
        use rio_proto::types::TerminationReason as R;
        let prefix = format!("{job_name}-");
        let Some(pod_name) = self
            .recently_disconnected
            .keys()
            .find(|k| k.starts_with(&prefix))
            .cloned()
        else {
            debug!(job_name = %job_name,
                   "DeadlineExceeded report: no recently_disconnected entry \
                    with prefix (already handled, swept, or worker reported \
                    TimedOut first)");
            return false;
        };
        let (drv_hash, _) = self
            .recently_disconnected
            .remove(&pod_name)
            .expect("key found above");

        let outcome = self
            .bump_resource_floor(&drv_hash, R::DeadlineExceeded, "deadline_exceeded")
            .await;

        let max = self.retry_policy.max_timeout_retries;
        if outcome.at_cap
            && let Some(state) = self.dag.node_mut(&drv_hash)
            && matches!(
                state.status(),
                DerivationStatus::Ready | DerivationStatus::Assigned | DerivationStatus::Running
            )
        {
            if state.retry.timeout_count >= max {
                warn!(
                    drv_hash = %drv_hash, job_name = %job_name,
                    timeout_retry_count = state.retry.timeout_count, max,
                    resource_floor = ?state.sched.resource_floor,
                    "DeadlineExceeded backstop: max_timeout_retries exhausted \
                     at cap, poisoning"
                );
                self.poison_and_cascade(
                    &drv_hash,
                    &format!("max_timeout_retries={max} exhausted at 24h deadline cap"),
                )
                .await;
                return false;
            }
            state.retry.timeout_count += 1;
        }

        if let Some(state) = self.dag.node(&drv_hash) {
            info!(
                drv_hash = %drv_hash, job_name = %job_name,
                pod_name = %pod_name, promoted = outcome.promoted,
                timeout_retry_count = state.retry.timeout_count, max,
                "DeadlineExceeded backstop fired"
            );
        }
        outcome.promoted
    }

    /// Sweep `recently_disconnected` entries older than
    /// [`TERMINATION_REPORT_TTL`]. Called from `handle_tick`. A swept
    /// entry means the controller's report never arrived (controller
    /// down, Pod deleted before reconcile observed it) — degrades to
    /// "this one OOM didn't promote".
    pub(super) fn tick_sweep_recently_disconnected(&mut self, now: Instant) {
        self.recently_disconnected
            .retain(|_, (_, at)| now.duration_since(*at) < TERMINATION_REPORT_TTL);
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
        // r[impl sec.executor.identity-token+2]
        // Defence-in-depth: once `executor_id` is bound to an attested
        // `intent_id` (by `handle_worker_connected` on accept, or a
        // prior token-gated heartbeat), a heartbeat for that
        // `executor_id` with a DIFFERENT non-empty `intent_id` is a
        // spoof — the gRPC bind only proves the body matches the
        // CALLER's token, not that `executor_id` belongs to that
        // caller. Without this, a worker holding a token for `I_self`
        // can heartbeat `{executor_id: E_victim, intent_id: I_self}`,
        // pass the gRPC bind, and overwrite
        // `executors[E_victim].intent_id` / `kind` — poisoning
        // E_victim's dispatch match. Mirrors the reconnect
        // intent-mismatch guard in `handle_worker_connected` (both-Some
        // and differ; `hb.intent_id = None` is unreachable in HMAC mode
        // for a bound executor — the gRPC bind rejects "" ≠ stored).
        if let Some(w) = self.executors.get(executor_id.as_str())
            && let Some(stored) = w.intent_id.as_ref()
            && let Some(presented) = hb.intent_id.as_ref()
            && presented != stored
        {
            warn!(
                executor_id = %executor_id,
                stored = %stored,
                presented = %presented,
                "heartbeat intent_id differs from bound intent; dropping (spoof?)"
            );
            metrics::counter!("rio_scheduler_executor_reconnect_rejected_total",
                              "reason" => "heartbeat_intent_mismatch")
            .increment(1);
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

        // ADR-023 §2.8: pod made it past Pending → clear the
        // Pending-watch entry so housekeeping doesn't mark its
        // (band, cap) ICE-infeasible. Idempotent (DashMap remove on
        // miss is a no-op); fires on the FIRST heartbeat from a
        // band-targeted pod regardless of whether the intent_id is
        // still Ready (the capacity signal is "pod scheduled", not
        // "drv still wants it"). Runs BEFORE the not-Ready→None
        // downgrade below for that reason.
        if let Some(id) = &hb.intent_id {
            self.pending_intents.remove(id.as_str());
            // Pod scheduled → ladder reset (r[sched.sla.ice-ladder-cap]).
            self.ice_attempts.remove(id.as_str());
        }
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
    // r[impl sched.heartbeat.phantom-drain+2]
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

        // Keep the scheduler-assigned build if still in-flight ON THIS
        // EXECUTOR. The ownership check matters for the adopt-conflict
        // path (W1 reports D, DAG has D Assigned→W2): without it, W1's
        // `running_build=D` survives as `prev_kept=Some(D)`, two empty
        // W1 heartbeats turn D into a confirmed phantom, and
        // `drain_phantoms` resets D to Ready while W2 is mid-build —
        // W2's eventual completion is then dropped on the floor.
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
        //
        // Whenever the worker reports `Some(hb)` and we have nothing
        // else for it, `running_build = Some(hb)` — matching the
        // worker's authoritative claim of being busy. The
        // `adopt_heartbeat_build` SIDE-EFFECT (DAG adoption + log/
        // metric) is gated separately on `prev_running != Some(hb)` so
        // it fires once on the first transition, not every heartbeat.
        // Without the split, an adopt-conflict worker whose DAG[D] is
        // terminal (W2 completed first) would oscillate None↔Some(D)
        // every heartbeat: prev_kept=None, guard false → fall through
        // to kept.clone()=None → spurious became_idle → inline
        // dispatch_ready to a worker that's actually busy.
        let mut reconciled: Option<DrvHash> = match (&prev_kept, &heartbeat_hash) {
            (None, Some(hb)) => {
                if prev_running.as_ref() != Some(hb) {
                    self.adopt_heartbeat_build(executor_id, hb);
                }
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
    ///
    /// `executor_id`: the heartbeating worker. Guarded so a phantom
    /// assigned to a DIFFERENT worker is never reset — defense-in-
    /// depth for the adopt-conflict case (`prev_kept`'s ownership
    /// check is the primary guard; this makes the clobber impossible
    /// even if that regresses).
    pub(super) async fn drain_phantoms(
        &mut self,
        executor_id: &ExecutorId,
        phantoms: Vec<DrvHash>,
    ) {
        let mut affected: std::collections::HashSet<Uuid> = Default::default();
        for phantom in phantoms {
            let Some(state) = self.dag.node_mut(&phantom) else {
                continue;
            };
            if state
                .assigned_executor
                .as_ref()
                .is_some_and(|a| a != executor_id)
            {
                // Adopt-conflict residue: W1's stale running_build
                // entry pointed at a drv now owned by W2. prev_kept's
                // ownership check should have dropped it before it
                // became a phantom; if it didn't, refuse the reset
                // here so W2's in-flight build isn't yanked.
                debug!(drv_hash = %phantom, owner = ?state.assigned_executor,
                       reporter = %executor_id,
                       "phantom drain: assigned to a different executor; skipping");
                continue;
            }
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
            affected.extend(self.get_interested_builds(&phantom));
            self.push_ready(phantom);
        }
        // Dashboard: running count dropped; assigned_executors lost
        // this worker. Emit so a quiet build doesn't show stale
        // Running until the next unrelated dispatch/completion. Same
        // chokepoint rationale as `reassign_derivations`.
        for build_id in affected {
            self.emit_progress(build_id);
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
