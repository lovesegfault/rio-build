//! Periodic Tick housekeeping: orphan-watcher sweep, poison-TTL
//! expiry, per-build timeouts, event-log GC, derivation-row GC, gauge
//! publish, SLA estimator refresh, and the open pull-attempt
//! establishment sweep — the single scheduler-side time-based repair
//! the pull path keeps.
//!
//! Split from `executor.rs` — that module is the executor lifecycle
//! (connect/disconnect/heartbeat); the `tick_*` fns here are periodic
//! maintenance that happens to run from the same actor loop.

use std::sync::Arc;
use std::time::Instant;

use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::state::{
    BuildState, DerivationStatus, DrvHash, ExecutorId, OutcomeClass, POISON_TTL, ReportingParty,
    verifiable_wanted_paths,
};

use super::DagActor;

/// Grace period for an Active build with zero `build_events` receivers
/// before the orphan-watcher sweep auto-cancels it. The gateway's
/// SubmitBuild stream is the primary receiver; on client disconnect
/// the gateway-side P0331 fix sends CancelBuild explicitly, but a
/// gateway crash (or gateway→scheduler timeout during disconnect
/// cleanup) leaves zero receivers and no cancel. The gateway's
/// WatchBuild reconnect path retries for ~111s (10 attempts, backoff
/// capped at 16s — see `gw.reconnect.backoff`), so 5min gives ample
/// room for a gateway blip without false-cancelling. Same cfg(test)
/// shadow pattern as POISON_TTL: tests backdate `orphaned_since`
/// instead of waiting 5 minutes.
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
    /// Refresh the SLA estimator from build_samples. Runs every ~6
    /// ticks (60s at the default 10s interval). Separated from
    /// handle_tick so the every-tick housekeeping stays readable.
    ///
    /// `pub(super)` for the `sla_contract` actor-boundary tests, which
    /// drive this directly to assert the derived-`inputs_gen` stability
    /// across no-op refreshes without going through `handle_tick`'s
    /// leader check and orphan-cancel side-effects.
    pub(super) async fn maybe_refresh_estimator(&mut self) {
        self.tick_count = self.tick_count.wrapping_add(1);

        // Every 6th tick (≈60s with 10s interval). Not configurable:
        // the SLA cache is a snapshot, not live; 60s is plenty fresh
        // for critical-path priorities. Making this tunable is YAGNI
        // until someone asks.
        const ESTIMATOR_REFRESH_EVERY: u64 = 6;
        if !self.tick_count.is_multiple_of(ESTIMATOR_REFRESH_EVERY) {
            return;
        }

        // VM-test sync barrier: increments on every refresh tick
        // (counter increments per tick so fixtures can poll it as
        // "≥2 ticks since INSERT").
        metrics::counter!("rio_scheduler_sla_refit_total").increment(1);

        // ADR-023 SLA estimator: incremental refit of touched
        // (pname, system, tenant) keys. Log-and-keep-stale on PG
        // failure; the cache holds the previous fit. The tier ladder
        // feeds the Schmitt-trigger reassignment inside refit.
        // `on_evict` is the shared `on_fit_evicted` so the memo's
        // bound (|live keys| × |overrides|) actually holds. ADR-023
        // L616: `HwTable` is a shared solve input; `inputs_gen` is
        // derived from it at poll time — nobody bumps.
        match self
            .sla_estimator
            .refresh(&self.db, &self.sla_tiers, |k| self.on_fit_evicted(k))
            .await
        {
            Ok(n) => debug!(keys_refit = n, "sla estimator refreshed"),
            Err(e) => {
                warn!(error = %e, "sla estimator refresh failed; keeping previous fits");
            }
        }

        // Full critical-path sweep (same 60s cadence). Belt-and-
        // suspenders over the incremental update_ancestors calls: any
        // drift (float accumulation, missed edge case) corrects here.
        // O(V+E); ~1ms for a 10k-node DAG.
        crate::critical_path::full_sweep(&mut self.dag, &self.sla_estimator, &self.builds);

        // Compact the ready queue (remove lazy-invalidated garbage).
        // No-op if garbage <50% of heap. Without this, a long-running
        // scheduler with lots of cancellations leaks heap memory.
        self.ready_queue.compact();
    }

    pub(super) async fn handle_tick(&mut self) {
        // r[impl sched.lease.standby-tick-noop+2]
        // Standby keeps stale self.builds/dag until LeaderLost lands;
        // every tick_* below either writes PG (orphan-cancel, build-
        // timeout, establishment, poison-clear, derivations-gc,
        // event-log sweep) or reads stale state. dispatch_ready (:108)
        // and gRPC (r[sched.grpc.leader-guard]) already gate; this
        // closes the Tick path. A 2-replica deploy with one lease flap
        // would otherwise let the ex-leader cancel every Active build
        // 5min later (orphan grace) — db.update_build_status has no
        // fence in its WHERE clause.
        if !self.leader.is_leader() {
            return;
        }
        self.maybe_refresh_estimator().await;

        let now = Instant::now();
        // Ordering is load-bearing: per-build-timeout cancels whole
        // builds (permanent failure) before poison-expire removes DAG
        // nodes.
        let expired_poisons = self.tick_scan_dag(now);
        self.tick_check_build_timeouts().await;
        self.tick_recheck_stuck_completions().await;
        self.tick_check_orphaned_builds().await;
        self.tick_process_expired_poisons(expired_poisons).await;

        self.tick_sweep_event_log();
        self.tick_gc_orphan_derivations().await;
        self.tick_sweep_dispatched_cells();
        self.tick_publish_gauges();
        self.tick_sweep_open_pull_attempts().await;

        // Advance probe_generation here (1/s) — NOT per
        // `sweep_ready_cached` call — so a Ready node is FMP-probed at
        // most once per Tick regardless of how many inline sweeps fire
        // between Ticks (after merges and completion cascades).
        self.probe_generation = self.probe_generation.wrapping_add(1);
        // Tick-cadence ready-set store short-circuit. The stream fleet
        // got this cadence implicitly (heartbeats marked the dispatch
        // pass dirty every tick, and the pass began with the batch
        // probe); running the surviving probe here keeps "outputs
        // appeared in the store after merge" bounded by one Tick
        // instead of waiting for the next merge/completion, on any
        // fleet. `probed_generation` stamping makes it one batched FMP
        // per Tick at most.
        self.sweep_ready_cached().await;

        // r[impl sched.admin.snapshot-cached]
        // send_replace: single-slot overwrite, never blocks, returns
        // the previous Arc (dropped). No-receiver is fine —
        // watch::Sender holds the value regardless.
        self.snapshot_tx
            .send_replace(Arc::new(self.compute_cluster_snapshot()));
    }

    // -----------------------------------------------------------------------
    // handle_tick helpers — one per periodic check
    // -----------------------------------------------------------------------

    /// Single DAG pass collecting poison-TTL expiries. The stream-era
    /// backstop-timeout scan that used to share this pass is gone with
    /// the session machinery — a stuck pull-mode attempt is bounded by
    /// the Job's `activeDeadlineSeconds` and resolved by the
    /// establishment sweep ([`Self::tick_sweep_open_pull_attempts`]).
    fn tick_scan_dag(&self, now: Instant) -> Vec<DrvHash> {
        let mut expired_poisons: Vec<DrvHash> = Vec::new();
        for (drv_hash, state) in self.dag.iter_nodes() {
            if state.status() == DerivationStatus::Poisoned
                && let Some(poisoned_at) = state.retry.poisoned_at
                && now.duration_since(poisoned_at) > POISON_TTL
            {
                expired_poisons.push(drv_hash.into());
            }
        }
        expired_poisons
    }

    /// Wall-clock limit on the ENTIRE build from submission. Distinct from:
    ///   - the per-attempt establishment window (deadline + report
    ///     slack) owned by [`Self::tick_sweep_open_pull_attempts`]
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

    /// Retry-driver for `transition_build` DB errors swallowed by
    /// `check_build_completion`. The DB-first ordering in
    /// `transition_build` makes retry POSSIBLE (in-mem stays Active on
    /// PG error), but every `check_build_completion` caller is
    /// event-driven (per-drv completion, dispatch cache-hit, merge,
    /// recovery) — after the LAST derivation completes there are no
    /// more events. Without a periodic re-check, the build sticks
    /// Active and `WatchBuild` hangs until the user disconnects (→
    /// orphan-watcher wrongly Cancels) or the scheduler restarts.
    ///
    /// O(builds) per tick, no DAG walk; idempotent
    /// (`check_build_completion` early-returns on `is_terminal()`).
    /// Runs BEFORE `tick_check_orphaned_builds` so completion-retry is
    /// attempted before orphan-cancel.
    async fn tick_recheck_stuck_completions(&mut self) {
        let candidates: Vec<Uuid> = self
            .builds
            .iter()
            .filter(|(_, b)| {
                b.state() == BuildState::Active
                    && (b.completed_count + b.failed_count) >= b.derivation_hashes.len() as u32
            })
            .map(|(id, _)| *id)
            .collect();
        for id in candidates {
            self.check_build_completion(id).await;
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
                .handle_cancel_build(build_id, None, "orphan_watcher_no_client")
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
        // r[impl sched.merge.substitute-topdown+10]
        // Surviving parents of the removed children, collected across
        // the loop for one batched closure-hole stamp below — the
        // TTL-sweep twin of the admin ClearPoison stamp and of the
        // terminal-build reap's holed-parents hook: a Poisoned child is
        // by definition un-produced, so its removal truncates each
        // surviving parent's child set relative to the parent's declared
        // closure, and children-keyed `topdown_pruned` verdicts must not
        // trust the truncated set (see `DerivationState::closure_hole`).
        let mut holed_parents: Vec<DrvHash> = Vec::new();
        for drv_hash in expired_poisons {
            info!(drv_hash = %drv_hash, "poison TTL expired, removing from DAG");
            // 1a: the TTL expiry's `poison_cleared` reset row joins the
            // PG-first clear in one transaction (the clear ordering —
            // pinned by `clearedPoisonClearsDurably` in the model — is
            // unchanged; this only adds the row). `resubmit_cycle = 0`
            // mirrors the full PG reset.
            let reset_row = self
                .reset_row_for(
                    &drv_hash,
                    crate::state::OutcomeClass::PoisonCleared,
                    crate::state::ReportingParty::Scheduler,
                )
                .map(|mut r| {
                    r.resubmit_cycle = 0;
                    r
                });
            if let Err(e) = self
                .record_reset_with_clear_poison(&drv_hash, reset_row)
                .await
            {
                error!(drv_hash = %drv_hash, error = %e, "failed to clear poison in PG");
                continue;
            }
            // Capture the parents AFTER the PG clear succeeded (only
            // then is the child actually removed below) and BEFORE
            // `remove_node` scrubs the edge maps.
            holed_parents.extend(self.dag.get_parents(&drv_hash));
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
        // Stamp the surviving parents (skipping any that were themselves
        // removed above): in memory first, then one best-effort PG write.
        holed_parents.sort();
        holed_parents.dedup();
        let mut holed: Vec<String> = Vec::new();
        for parent in &holed_parents {
            if let Some(state) = self.dag.node_mut(parent) {
                state.closure_hole = true;
                holed.push(parent.to_string());
            }
        }
        if !holed.is_empty() {
            // Best-effort PG counterpart (`migrations/064`). Leader-only
            // by construction — `handle_tick` is a no-op on standby
            // (`r[sched.lease.standby-tick-noop]`), the same posture the
            // `clear_poison` PG write above already relies on — so no
            // redundant gate here. A lost write costs only the
            // breadcrumb's durability across a failover; the in-memory
            // stamp covers this tenure.
            if let Err(e) = self.db.set_closure_hole_by_hashes(&holed).await {
                warn!(count = holed.len(), error = %e,
                      "failed to persist closure_hole after poison-TTL sweep (continuing)");
            }
        }
    }

    /// DAG-state sweep for `dispatched_cells`. The arm-on-ack write
    /// (`handle_ack_spawned_intents`) can't fire for a drv that was
    /// acked then cancelled / substituted / dependency-failed before
    /// its pod heartbeated, so the heartbeat-edge / disconnect remove
    /// paths never run for it. Retain only entries whose DAG node is
    /// still in a pre-terminal state where a heartbeat is plausible.
    /// Cheap: `dispatched_cells` is bounded by acked-but-not-yet-
    /// heartbeated drvs (≪ DAG size).
    fn tick_sweep_dispatched_cells(&self) {
        use DerivationStatus::{Assigned, Ready, Running};
        self.dispatched_cells.retain(|k, _| {
            self.dag
                .node(k)
                .is_some_and(|s| matches!(s.status(), Ready | Assigned | Running))
        });
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

    // r[impl sched.db.derivations-gc+2]
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
    /// Leader-only via the `handle_tick` early-return above. A fresh
    /// standby never reaches here so it exports no series (see
    /// `test_not_leader_does_not_set_gauges`). A was-leader-now-standby
    /// has its gauges zeroed once by `handle_leader_lost` so its frozen
    /// last-tick values don't sit in Prometheus indefinitely —
    /// EXCEPT `workers_active`, which is connection-state (not
    /// leader-state) and is maintained by the inc/dec path on standby
    /// as workers rebalance away. Net: queries see one non-zero series
    /// for the leader-state gauges, no max() wrapper needed.
    // r[impl obs.metric.scheduler-leader-gate+2]
    fn tick_publish_gauges(&self) {
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

    /// Establishment sweep for open pull-mode attempts — the single
    /// scheduler-side time-based repair the pull path keeps. Every open
    /// pull-mode attempt (the durable view, `dispatch_mode = 'pull'`
    /// only) is visited every sweep; one whose age exceeds its intent
    /// deadline plus `establishment_report_slack` with no terminal row
    /// is resolved by the store-probe arm (all verifiable wanted
    /// outputs present → adopted as completed, never charged) or
    /// established exactly once as an unreported executor crash
    /// (charged through the same append+decide discipline as every
    /// other establishment vehicle) and requeued. Stream-mode attempts
    /// are never visited — the as-built correlation machinery remains
    /// their only establishment vehicle during coexistence. Also
    /// refreshes `rio_scheduler_open_attempts` (one query serves both;
    /// the gauge counts pull-mode attempts only and is durable-backed
    /// so it survives failover exactly like the rows it counts).
    /// Leader-only via the `handle_tick` early-return; the establishing
    /// transaction additionally carries the same generation-floor fence
    /// as the pull transaction.
    // r[impl sched.attempt.establishment-window+2]
    pub(super) async fn tick_sweep_open_pull_attempts(&mut self) {
        let opens = match self.db.list_open_pull_attempts().await {
            Ok(rows) => rows,
            Err(e) => {
                debug!(error = %e, "open pull-attempt sweep: view query failed; retrying next tick");
                return;
            }
        };
        metrics::gauge!("rio_scheduler_open_attempts").set(opens.len() as f64);
        if opens.is_empty() {
            return;
        }
        let slack_secs = self.establishment_report_slack.as_secs_f64();
        // The window: anchored to the deadline the attempt was actually
        // dispatched under (persisted by the pull mint — the same solve
        // activeDeadlineSeconds is rendered from), plus the configured
        // report slack. The sweep-time re-solve may only WIDEN the
        // window (estimate grew, floor bump): a fitted estimate or
        // hw-table change that shrinks between dispatch and sweep must
        // never establish a healthy attempt that is still inside the
        // deadline its pod is really running under. Rows minted before
        // the column existed (and a node evicted from the DAG) fall
        // back to whichever anchor is available.
        let (hw, cost, inputs_gen) = self.solve_inputs();
        let expired: Vec<crate::db::open_attempts::OpenAttemptRow> = opens
            .into_iter()
            .filter(|attempt| {
                let dispatched_deadline = attempt.deadline_secs.unwrap_or(0.0);
                let resolved_deadline = self
                    .dag
                    .node(attempt.drv_hash.as_str())
                    .map(|state| {
                        f64::from(
                            self.solve_intent_for(state, &hw, &cost, inputs_gen)
                                .deadline_secs,
                        )
                    })
                    .unwrap_or(0.0);
                let deadline_secs = dispatched_deadline.max(resolved_deadline);
                attempt.age_secs > deadline_secs + slack_secs
            })
            .collect();
        if expired.is_empty() {
            return;
        }
        // One store probe for the whole batch (the same recovery probe
        // the post-failover reconcile uses).
        let probe_paths: Vec<String> = expired
            .iter()
            .filter_map(|a| self.dag.node(a.drv_hash.as_str()))
            .flat_map(|s| s.expected_output_paths.iter())
            .filter(|p| !p.is_empty())
            .cloned()
            .collect();
        let missing = self.batch_probe_orphan_outputs(probe_paths).await;
        for attempt in expired {
            self.establish_open_pull_attempt(&attempt, missing.as_ref())
                .await;
        }
    }

    /// Resolve one expired open pull-mode attempt: adopt (store-probe
    /// arm) or establish + requeue (C2 charge arm). See
    /// [`Self::tick_sweep_open_pull_attempts`].
    async fn establish_open_pull_attempt(
        &mut self,
        attempt: &crate::db::open_attempts::OpenAttemptRow,
        missing: Option<&std::collections::HashSet<String>>,
    ) {
        // Standby replicas must neither write attempt rows nor decide
        // from them (the same gate every establishment vehicle carries).
        if !self.leader.is_leader() {
            return;
        }
        let drv_hash = DrvHash::from(attempt.drv_hash.as_str());
        let executor = ExecutorId::from(attempt.executor_id.as_str());

        // Store-probe arm: every verifiable wanted output present →
        // adopt as completed; the attempt is closed and never charged.
        if let Some(state) = self.dag.node(&drv_hash) {
            let adopt = verifiable_wanted_paths(
                &state.output_names,
                &state.expected_output_paths,
                &state.wanted_output_names,
            )
            .is_some_and(|verifiable| {
                missing.is_some_and(|m| verifiable.iter().all(|p| !m.contains(*p)))
            });
            if adopt {
                let expected = state.expected_output_paths.clone();
                self.adopt_orphan_completion(&drv_hash, &Some(executor.clone()), expected)
                    .await;
                if let Err(e) = self
                    .db
                    .update_assignment_status(
                        attempt.derivation_id,
                        crate::db::AssignmentStatus::Completed,
                    )
                    .await
                {
                    warn!(drv_hash = %drv_hash, error = %e,
                          "establishment adopt: failed to close the assignment row");
                }
                info!(drv_hash = %drv_hash, exec_id = %attempt.exec_id,
                      "establishment sweep: outputs present in store, adopted as completed (no charge)");
                return;
            }
        }

        // C2 charge arm: exactly one executor-crash/unreported
        // establishment per attempt (the exec_id partial-unique index
        // is the arbiter), fenced by the claims floor, closing the
        // assignments row in the same transaction.
        let verdict_eligible = self.dag.node(&drv_hash).is_some_and(|s| {
            matches!(
                s.status(),
                DerivationStatus::Ready | DerivationStatus::Assigned | DerivationStatus::Running
            )
        });
        let serving_generation = self.leader.generation() as i64;
        let mut row = crate::db::attempts::AttemptRow::new(
            attempt.derivation_id,
            OutcomeClass::ExecutorCrash,
            ReportingParty::Scheduler,
        );
        row.exec_id = Some(attempt.exec_id);
        row.executor_id = Some(executor.clone());
        // AD2c: the establishment charge carries the
        // controller-authoritative node attribution — the value the
        // mint persisted, the controller's later ReportAttemptOutcome
        // backfill, or (belt-and-braces) the in-memory spawn-ack
        // binding that arrived after this sweep's row was read — so
        // the re-keyed exclusion is independent of winning the
        // mint-time race and survives failover off the ledger row.
        row.source_node = attempt
            .source_node
            .clone()
            .or_else(|| self.pull_attempt_source_node(&drv_hash));
        row.termination_reason = Some("unreported".into());
        type ChargeOutcome = Option<(bool, crate::retry_policy::Decision)>;
        let result: Result<ChargeOutcome, sqlx::Error> = async {
            let mut tx = self.db.pool().begin().await?;
            // The same generation fence the pull transaction applies:
            // a below-floor serving generation writes nothing.
            let floor: Option<i64> = sqlx::query_scalar(
                "SELECT GREATEST( \
                     (SELECT MAX(generation) FROM assignments), \
                     (SELECT MAX(generation) FROM leader_generation_claims))",
            )
            .fetch_one(&mut *tx)
            .await?;
            if floor.is_some_and(|f| serving_generation < f) {
                tx.rollback().await?;
                return Ok(None);
            }
            let (won, decision) = self.append_and_decide_in_tx(&mut tx, &row).await?;
            if won
                && verdict_eligible
                && matches!(decision.verdict, crate::retry_policy::Verdict::Poison(_))
            {
                crate::db::SchedulerDb::persist_poisoned_in_tx(&mut tx, &drv_hash).await?;
            }
            sqlx::query(
                "UPDATE assignments SET status = 'failed', completed_at = now() \
                 WHERE exec_id = $1 AND status IN ('pending', 'acknowledged')",
            )
            .bind(attempt.exec_id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok(Some((won, decision)))
        }
        .await;
        let (won, decision) = match result {
            Ok(Some(pair)) => pair,
            Ok(None) => {
                info!(drv_hash = %drv_hash, serving_generation,
                      "establishment sweep: serving generation below the claims floor; nothing written");
                return;
            }
            Err(e) => {
                warn!(drv_hash = %drv_hash, exec_id = %attempt.exec_id, error = %e,
                      "establishment sweep: appending transaction failed; the attempt stays open \
                       for this pass (no charge, no verdict)");
                return;
            }
        };
        if !won {
            // Another classifier landed concurrently (its row holds the
            // verdict); this pass records and changes nothing.
            return;
        }
        // OA1 interval, establishment cause: attempt opened → established.
        metrics::histogram!(
            "rio_scheduler_attempt_requeue_seconds",
            "cause" => "establishment"
        )
        .record(attempt.age_secs.max(0.0));
        // Dedicated pull-sweep establishment count for the OA2 interim
        // alert: the requeue histogram's establishment cause is shared
        // with the stream-mode correlation-TTL sweep, so the per-node
        // clustering tripwire keys on this counter instead.
        metrics::counter!("rio_scheduler_pull_establishments_total").increment(1);
        if let Some(state) = self.dag.node_mut(&drv_hash) {
            state.push_attempt_record(row.to_record());
        }
        self.refresh_retry_view(&drv_hash);
        info!(
            drv_hash = %drv_hash,
            exec_id = %attempt.exec_id,
            executor_id = %executor,
            age_secs = attempt.age_secs,
            "establishment sweep: open pull-mode attempt established as unreported executor crash"
        );
        if !verdict_eligible {
            return;
        }
        match decision.verdict {
            crate::retry_policy::Verdict::Poison(reason) => {
                if !matches!(reason, crate::retry_policy::PoisonReason::Threshold) {
                    error!(drv_hash = %drv_hash, ?reason,
                           "decide() returned an unexpected poison reason for an established \
                            pull attempt; poisoning with the threshold message (investigate)");
                }
                self.poison_already_recorded(
                    &drv_hash,
                    "poison threshold reached after unreported executor crashes",
                    None,
                )
                .await;
            }
            _ => {
                // The C2 requeue: the pod is gone, the charge is
                // recorded — return the derivation to the queue through
                // the same chokepoint every other no-report observation
                // uses.
                self.reassign_derivations(std::slice::from_ref(&drv_hash), Some(&executor))
                    .await;
            }
        }
    }
}
