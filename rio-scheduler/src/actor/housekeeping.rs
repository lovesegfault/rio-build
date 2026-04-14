//! Periodic Tick housekeeping: heartbeat-timeout reap, backstop
//! timeouts, orphan-watcher sweep, poison-TTL expiry, event-log GC,
//! derivation-row GC, gauge publish, estimator refresh.
//!
//! Split from `executor.rs` — that module is the executor lifecycle
//! (connect/disconnect/heartbeat); the eight `tick_*` fns here are
//! periodic maintenance that happens to run from the same actor loop.
//! Keeping them separate makes "what runs every Tick" discoverable
//! without scrolling past 1000 lines of heartbeat-reconcile.

use std::sync::Arc;
use std::time::Instant;

use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::state::{
    BuildState, DerivationStatus, DrvHash, ExecutorId, HEARTBEAT_TIMEOUT_SECS,
    MAX_MISSED_HEARTBEATS, POISON_TTL,
};

use super::DagActor;

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

/// Grace period for an Active build with zero `build_events` receivers
/// before the orphan-watcher sweep auto-cancels it. The gateway's
/// SubmitBuild stream is the primary receiver; on client disconnect
/// the gateway-side P0331 fix sends CancelBuild explicitly, but a
/// gateway crash (or gateway→scheduler timeout during disconnect
/// cleanup) leaves zero receivers and no cancel. The gateway's
/// WatchBuild reconnect path retries for ~111s (10 attempts, backoff
/// capped at 16s — see `gw.reconnect.backoff`), so 5min gives ample
/// room for a gateway blip without false-cancelling. Same cfg(test)
/// shadow as BACKSTOP_*: tests backdate `orphaned_since` instead of
/// waiting 5 minutes.
#[cfg(not(test))]
const ORPHAN_BUILD_GRACE: std::time::Duration = std::time::Duration::from_secs(300);
// CAUTION: with zero grace, the orphan sweep cancels ANY Active build whose
// `build_events` receiver count is 0 on the SECOND Tick after the drop. A
// test that drops/ignores the `event_rx` returned from MergeDag and then
// sends `Tick` ≥2× will silently lose its derivations to auto-cancel. If a
// test's build vanishes mid-sequence, check whether `event_rx` is held.
#[cfg(test)]
const ORPHAN_BUILD_GRACE: std::time::Duration = std::time::Duration::ZERO;

impl DagActor {
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

        // ADR-023 SLA estimator: incremental refit of touched
        // (pname, system, tenant) keys. Same cadence + same
        // log-and-keep-stale semantics on PG failure as the EMA
        // estimator above; the cache holds the previous fit. The tier
        // ladder feeds the Schmitt-trigger reassignment inside refit.
        //
        // Gated on `[sla]` configured: in Static mode the cache is
        // never read (snapshot.rs / dispatch.rs both gate on
        // `sla_config.is_some()`), so the per-key PG round-trips +
        // NNLS refit are pure overhead. Keeps Static-mode tick latency
        // identical to pre-ADR-023.
        if self.sla_config.is_some() {
            match self.sla_estimator.refresh(&self.db, &self.sla_tiers).await {
                Ok(n) => debug!(keys_refit = n, "sla estimator refreshed"),
                Err(e) => {
                    warn!(error = %e, "sla estimator refresh failed; keeping previous fits");
                }
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
        // Reset the "worker newly available" inline-dispatch budget.
        // See `BECAME_IDLE_INLINE_CAP` — past the cap, became_idle
        // heartbeats and PrefetchComplete cold→warm edges since the
        // previous Tick set dispatch_dirty instead; the dispatch
        // below drains it.
        self.became_idle_inline_this_tick = 0;

        self.maybe_refresh_estimator().await;

        let now = Instant::now();
        self.tick_check_heartbeats(now).await;
        self.tick_sweep_recently_disconnected(now);

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
        self.tick_check_orphaned_builds().await;
        self.tick_process_expired_poisons(expired_poisons).await;

        self.tick_sweep_event_log();
        self.tick_gc_orphan_derivations().await;
        self.tick_publish_gauges();

        // r[impl sched.actor.dispatch-decoupled]
        // I-163: coalesced dispatch. Heartbeat sets the flag; we drain
        // it at Tick cadence (≤1/s) instead of per-heartbeat (29/s at
        // 290 workers). dispatch_ready clears the flag itself (after
        // its leader/recovery gates) so inline callers (MergeDag,
        // ProcessCompletion, the capped became_idle/PrefetchComplete
        // carve-out) also satisfy it.
        //
        // Advance probe_generation here (1/s) — NOT per dispatch_ready
        // call — so a Ready node is FMP-probed at most once per Tick
        // regardless of how many inline dispatches fire between Ticks.
        self.probe_generation = self.probe_generation.wrapping_add(1);
        if self.dispatch_dirty {
            self.dispatch_ready().await;
        }

        // r[impl sched.admin.snapshot-cached]
        // Publish AFTER dispatch so the snapshot reflects this Tick's
        // assignments. send_replace: single-slot overwrite, never blocks,
        // returns the previous Arc (dropped). No-receiver is fine —
        // watch::Sender holds the value regardless.
        self.snapshot_tx
            .send_replace(Arc::new(self.compute_cluster_snapshot()));
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
                && let Some(poisoned_at) = state.retry.poisoned_at
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
                let est_3x_secs =
                    if state.sched.est_duration.is_finite() && state.sched.est_duration > 0.0 {
                        state.sched.est_duration * 3.0
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
            // Clear worker's running build (we're taking it back). With
            // P0537's single-slot model the equality check is belt-and-
            // suspenders — `drv_hash` came from this worker's slot.
            if let Some(worker) = self.executors.get_mut(executor_id)
                && worker.running_build.as_ref() == Some(drv_hash)
            {
                worker.running_build = None;
            }
            // Reassign (same path as worker disconnect): reset_to_
            // ready + PG status + push_ready. Backstop-timeout is NOT
            // a sizing signal — handle_timeout_failure (worker-reported
            // TimedOut) does the floor promotion; this scheduler-side
            // backstop is "worker hung, never reported."
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

    /// Auto-cancel Active builds whose event-broadcast channel has had
    /// zero receivers for longer than [`ORPHAN_BUILD_GRACE`].
    ///
    /// `receiver_count() == 0` means no gateway is watching: the
    /// SubmitBuild response stream and any WatchBuild streams have all
    /// dropped. The gateway's P0331 fix sends an explicit CancelBuild on
    /// client disconnect, so this sweep is the backstop for the cases
    /// that path can't cover:
    ///
    ///   - Gateway crash mid-build (no process left to send CancelBuild)
    ///   - Gateway→scheduler timeout during the disconnect-cleanup loop
    ///     (session.rs `cancel_active_builds` wraps in DEFAULT_GRPC_TIMEOUT
    ///     and warns-but-continues on timeout — the build leaks)
    ///   - Post-recovery: recovered builds start with zero receivers
    ///     until the gateway WatchBuild-reconnects. Gateway reconnect
    ///     retries for ~111s; the 5min grace covers it.
    ///
    /// The grace timer resets if a watcher reattaches (`handle_watch_build`
    /// → `tx.subscribe()` → `receiver_count() > 0` on the next tick) —
    /// a transient gateway blip doesn't cancel.
    ///
    /// TODO(I-112): `detached` builds (fire-and-forget — survive client
    /// disconnect by design) should skip this check. The opt-out knob
    /// can't be plumbed via `nix build --option` today: ssh-ng SetOptions
    /// is unreachable (WONTFIX P0310, ssh-store.cc empty override). Wire
    /// `SubmitBuildRequest.detached` once a non-SetOptions channel exists
    /// (per-tenant config, or rio-gateway advertising
    /// `set-options-map-only`).
    // r[impl sched.backstop.orphan-watcher]
    async fn tick_check_orphaned_builds(&mut self) {
        let now = Instant::now();
        let mut to_cancel: Vec<Uuid> = Vec::new();
        for (build_id, build) in self.builds.iter_mut() {
            if build.state() != BuildState::Active {
                // Pending: hasn't started dispatching — the SubmitBuild
                // handler is still running and holds a receiver (or the
                // MergeDag-reply-dropped path already cancelled it).
                // Terminal: nothing to cancel.
                continue;
            }
            let watched = self
                .events
                .channels
                .get(build_id)
                .is_some_and(|tx| tx.receiver_count() > 0);
            if watched {
                // Watcher (re)attached — reset the timer. Covers the
                // gateway WatchBuild-reconnect path: a 30s gateway blip
                // sets orphaned_since, the reconnect clears it.
                build.orphaned_since = None;
            } else {
                match build.orphaned_since {
                    None => build.orphaned_since = Some(now),
                    Some(since) if now.duration_since(since) > ORPHAN_BUILD_GRACE => {
                        to_cancel.push(*build_id);
                    }
                    Some(_) => {} // within grace — keep waiting
                }
            }
        }
        for build_id in to_cancel {
            warn!(
                build_id = %build_id,
                grace_secs = ORPHAN_BUILD_GRACE.as_secs(),
                "orphan-watcher: build has no watchers past grace; auto-cancelling"
            );
            metrics::counter!("rio_scheduler_orphan_builds_cancelled_total").increment(1);
            if let Err(e) = self
                .handle_cancel_build(build_id, "orphan_watcher_no_client")
                .await
            {
                error!(build_id = %build_id, error = %e,
                       "orphan-watcher: cancel failed");
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
        if self.tick_count.is_multiple_of(EVENT_LOG_SWEEP_EVERY) && self.events.has_persister() {
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

    // r[impl sched.db.derivations-gc]
    /// I-169.2: periodic sweep of orphan-terminal `derivations` rows.
    /// Every 30th tick (~5min at the default 10s interval) → delete
    /// ≤1000. A 1.16M backlog drains in ~4 days; steady-state churn
    /// (terminal nodes per 5min from failed closures) is well under the
    /// batch cap. Best-effort: PG error logs and retries next interval.
    async fn tick_gc_orphan_derivations(&self) {
        const DERIVATIONS_GC_EVERY: u64 = 30;
        const DERIVATIONS_GC_BATCH: i64 = 1000;
        if !self.tick_count.is_multiple_of(DERIVATIONS_GC_EVERY) {
            return;
        }
        match self
            .db
            .gc_orphan_terminal_derivations(DERIVATIONS_GC_BATCH)
            .await
        {
            Ok(0) => {}
            Ok(n) => {
                debug!(deleted = n, "GC'd orphan-terminal derivation rows");
                metrics::counter!("rio_scheduler_derivations_gc_deleted_total").increment(n);
            }
            Err(e) => warn!(error = %e, "derivations GC sweep failed; retrying next interval"),
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
        if self.leader.is_leader() {
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
