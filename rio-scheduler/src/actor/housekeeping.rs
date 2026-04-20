//! Periodic Tick housekeeping: heartbeat-timeout reap, backstop
//! timeouts, orphan-watcher sweep, poison-TTL expiry, event-log GC,
//! derivation-row GC, gauge publish, SLA estimator refresh.
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
    BuildState, DerivationStatus, DrvHash, ExecutorId, HEARTBEAT_TIMEOUT_SECS, POISON_TTL,
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
    /// Refresh the SLA estimator from build_samples. Runs every ~6
    /// ticks (60s at the default 10s interval). Separated from
    /// handle_tick so the every-tick housekeeping stays readable.
    async fn maybe_refresh_estimator(&mut self) {
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
        match self.sla_estimator.refresh(&self.db, &self.sla_tiers).await {
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
        // r[impl sched.lease.standby-tick-noop]
        // Standby keeps stale self.builds/dag until LeaderLost lands;
        // every tick_* below either writes PG (orphan-cancel, build-
        // timeout, backstop-reassign, poison-clear, derivations-gc,
        // event-log sweep) or reads stale state. dispatch_ready (:108)
        // and gRPC (r[sched.grpc.leader-guard]) already gate; this
        // closes the Tick path. A 2-replica deploy with one lease flap
        // would otherwise let the ex-leader cancel every Active build
        // 5min later (orphan grace) — db.update_build_status has no
        // fence in its WHERE clause.
        if !self.leader.is_leader() {
            return;
        }
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
        self.tick_sweep_pending_intents(now);

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
        self.tick_recheck_stuck_completions().await;
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

    /// ADR-023 §2.8 Pending-watch: sweep `pending_intents` entries
    /// older than `hw_fallback_after_secs` (jittered ±20% per-entry,
    /// derived from drv_hash so the threshold is stable across ticks)
    /// → mark `(band, cap)` ICE-infeasible for 60s and drop the entry.
    /// Next `compute_spawn_intents` re-runs `solve_full` for that
    /// drv with the cell excluded.
    ///
    /// Also drops entries for drvs that have left Ready (dispatched
    /// elsewhere, cancelled, completed-via-substitute) — those will
    /// never see a heartbeat and shouldn't mark ICE.
    ///
    /// Scheduler-side approximation of the controller seeing
    /// `phase=Pending`: less precise (cannot tell Pending from
    /// "controller hasn't spawned yet" or "container crashed before
    /// first heartbeat") but all three mean the `(band, cap)` failed to
    /// produce capacity within the window, and the 60s ICE TTL bounds
    /// the false-positive cost. See [`crate::sla::cost::IceBackoff`].
    // r[impl sched.sla.ice-ladder-cap]
    pub(super) fn tick_sweep_pending_intents(&self, now: Instant) {
        let fallback = self.sla_config.hw_fallback_after_secs;
        let ladder_cap = self.ice_ladder_cap();
        // Reap `ice_attempts` for drvs that left Ready. Keyed on dag
        // state directly — NOT parasitic on `pending_intents` membership:
        // a ladder-exhausted drv dispatches band-agnostic, so its acks
        // don't re-enter `pending_intents` (parse_selector → None) and
        // a parasitic cleanup would orphan the entry forever. A re-queue
        // (e.g. resubmit) must not inherit a prior generation's exhaust.
        self.ice_attempts.retain(|drv_hash, _| {
            self.dag
                .node(drv_hash)
                .is_some_and(|s| s.status() == crate::state::DerivationStatus::Ready)
        });
        self.pending_intents.retain(|drv_hash, (band, cap, since)| {
            // Drop entries whose drv left Ready: they won't heartbeat
            // and the timeout isn't a capacity signal.
            if self
                .dag
                .node(drv_hash)
                .is_none_or(|s| s.status() != crate::state::DerivationStatus::Ready)
            {
                return false;
            }
            // Stable per-drv jitter ∈ [0.8, 1.2): hash low byte → factor.
            // Stable so a single entry doesn't flip in/out across ticks.
            let h = drv_hash.bytes().fold(0u8, |a, b| a.wrapping_add(b));
            let jitter = 0.8 + 0.4 * f64::from(h) / 256.0;
            if now.duration_since(*since).as_secs_f64() <= fallback * jitter {
                return true;
            }
            self.ice.mark(*band, *cap);
            // Per-build ladder cap: bound retries so a tier with a 1h
            // p90 doesn't burn ~30 fallback rounds before demoting. The
            // fleet-wide `ice.exhausted()` (all 6 cells live) is
            // unreachable from one build's serial probing because
            // `ICE_TTL=60s < hw_fallback_after≈120s`. `ice_attempts`
            // is separate from `pending_intents` so it survives this
            // drop + the controller's reap/respawn/ack `or_insert`.
            let mut attempted = self.ice_attempts.entry(drv_hash.clone()).or_default();
            attempted.push((*band, *cap));
            metrics::counter!(
                "rio_scheduler_sla_ice_backoff_total",
                "band" => band.label(),
                "cap" => cap.label(),
            )
            .increment(1);
            debug!(
                drv_hash = %drv_hash, band = band.label(), cap = cap.label(),
                pending_secs = now.duration_since(*since).as_secs(),
                attempted = attempted.len(), ladder_cap,
                "pending-watch: no heartbeat within hw_fallback_after; \
                 marking (band, cap) ICE-infeasible for 60s"
            );
            if attempted.len() as u32 >= ladder_cap {
                crate::sla::cost::capacity_exhausted();
                debug!(
                    drv_hash = %drv_hash,
                    attempted = ?crate::sla::cost::IceBackoff::encode_attempted(&attempted),
                    "pending-watch: ICE ladder cap reached; forcing band-agnostic dispatch"
                );
            }
            false
        });
    }

    /// `IceBackoff::ladder_cap` evaluated against the configured tier
    /// ladder + `hw_fallback_after_secs`. Shared between the sweep and
    /// `solve_intent_for`'s ladder-exhausted gate so both agree on the
    /// same threshold.
    pub(crate) fn ice_ladder_cap(&self) -> u32 {
        let max_tier_bound = self
            .sla_tiers
            .iter()
            .filter_map(crate::sla::solve::Tier::binding_bound)
            .fold(0.0_f64, f64::max);
        crate::sla::cost::IceBackoff::ladder_cap(
            if max_tier_bound > 0.0 {
                max_tier_bound
            } else {
                3600.0
            },
            self.sla_config.hw_fallback_after_secs,
        )
    }

    /// Scan workers for heartbeat timeouts; disconnect any that have
    /// been silent past `HEARTBEAT_TIMEOUT_SECS`. The constant is
    /// `MAX_MISSED_HEARTBEATS × HEARTBEAT_INTERVAL_SECS` (limits.rs:63)
    /// — the ×3 is already baked in. The previous implementation used
    /// 30s as a per-tick increment gate AND required a counter to reach
    /// 3, applying the ×3 twice → reap at ~60s not 30s.
    async fn tick_check_heartbeats(&mut self, now: Instant) {
        let timeout = std::time::Duration::from_secs(HEARTBEAT_TIMEOUT_SECS);
        let timed_out: Vec<_> = self
            .executors
            .iter()
            .filter(|(_, w)| now.duration_since(w.last_heartbeat) > timeout)
            .map(|(id, w)| (id.clone(), w.stream_epoch))
            .collect();
        for (executor_id, stream_epoch) in timed_out {
            warn!(executor_id = %executor_id, silence_secs = HEARTBEAT_TIMEOUT_SECS,
                  "worker heartbeat timeout; disconnecting");
            // Current epoch — this is the actor itself deciding the
            // worker is dead, not a late reader-task signal. No
            // `seen_drvs`: heartbeat-timeout has no reader-task
            // context; any buffer leak is bounded by
            // `handle_cleanup_terminal_build`'s discard on DAG reap.
            self.handle_executor_disconnected(&executor_id, stream_epoch, Vec::new())
                .await;
        }
    }

    /// Single DAG pass collecting both poison-TTL expiries and backstop-
    /// timeout candidates. Coupled because the two checks share the
    /// per-node iteration; splitting would double `iter_nodes()` passes
    /// for no behavioral gain.
    ///
    /// Returns `(expired_poisons, backstop_timeouts)` — backstop tuple is
    /// `(drv_hash, drv_path, executor_id)`.
    // r[impl sched.backstop.timeout+3]
    fn tick_scan_dag(&self, now: Instant) -> (Vec<DrvHash>, Vec<(DrvHash, String, ExecutorId)>) {
        let mut expired_poisons: Vec<DrvHash> = Vec::new();
        // (drv_hash, drv_path, executor_id) for backstop-timed-out builds
        let mut backstop_timeouts: Vec<(DrvHash, String, ExecutorId)> = Vec::new();

        // r[impl sched.sla.hw-ref-seconds]
        // est_duration is REF-seconds (sla.ref_estimate → t_min); elapsed
        // is wall. Worker hw_class is unknown here (only CompletionReport
        // carries it) → divide by slowest factor so the backstop never
        // fires before worst-case wall completion. Same pattern as
        // snapshot.rs deadline derivation. min_factor() is clamped at
        // HW_FACTOR_SANITY_FLOOR (sla/hw.rs) so division is safe; one
        // read-lock per tick (hoisted out of the per-node loop).
        let min_hw = self.sla_estimator.hw_table().min_factor();

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
                // est_duration is REF-seconds (f64) — denormalized to
                // wall above the loop via `/ min_hw`. 0.0 = no estimate
                // (fresh derivation, estimator had no history) → floor
                // applies.
                //
                // is_finite() guard: NaN/inf propagate through max()
                // (NaN.max(x)=NaN, inf.max(x)=inf) → `elapsed > NaN`
                // is always false → backstop never fires. Treat
                // non-finite est as "no estimate" (0.0 → floor wins).
                let est_3x_secs =
                    if state.sched.est_duration.is_finite() && state.sched.est_duration > 0.0 {
                        (state.sched.est_duration / min_hw) * 3.0
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

    /// Process backstop timeouts: send CancelSignal, quarantine the
    /// wedged worker, record into `failed_builders`/`failure_count`,
    /// reset to Ready for retry. This is a TRANSIENT failure (the
    /// build may work fine on another worker) so we go through retry
    /// not poison — but the attempt IS accounted, so
    /// `reassign_derivations`'s poison check bounds the loop at
    /// `threshold` iterations.
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
                match tx.try_send(rio_proto::types::SchedulerMessage {
                    msg: Some(rio_proto::types::scheduler_message::Msg::Cancel(
                        rio_proto::types::CancelSignal {
                            drv_path: drv_path.clone(),
                            reason: "backstop timeout (stuck build)".into(),
                        },
                    )),
                }) {
                    Ok(()) => {
                        metrics::counter!("rio_scheduler_cancel_signals_total").increment(1);
                    }
                    Err(_) => {
                        metrics::counter!("rio_scheduler_cancel_signal_dropped_total").increment(1);
                    }
                }
            }
            // Clear worker's running build (we're taking it back). With
            // P0537's single-slot model the equality check is belt-and-
            // suspenders — `drv_hash` came from this worker's slot.
            if let Some(worker) = self.executors.get_mut(executor_id)
                && worker.running_build.as_ref() == Some(drv_hash)
            {
                worker.running_build = None;
                // Backstop fired while heartbeats continue → executor
                // task is wedged but pod alive. Mark draining so
                // dispatch doesn't feed it new work that would sit
                // Assigned forever (`tick_scan_dag` only scans
                // Running). Same shape as completion.rs's one-shot
                // post-completion drain.
                worker.draining = true;
            }
            // r[impl sched.backstop.timeout+3]
            // Backstop is the no-CompletionReport path — completion.rs
            // will never account this attempt. Record it here so
            // `is_poisoned()` in `reassign_derivations` caps the loop
            // at `threshold`. NOT a sizing signal —
            // `handle_timeout_failure` (worker-reported TimedOut) does
            // the floor promotion; this scheduler-side backstop is
            // "worker hung, never reported."
            if let Some(state) = self.dag.node_mut(drv_hash) {
                state.retry.failed_builders.insert(executor_id.clone());
                state.retry.failure_count += 1;
            }
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
            // Discard any log buffer BEFORE remove_node drops the
            // hash→path mapping. Reachable via 24h-TTL on a drv that
            // never re-dispatched (so the reassign-path discard never
            // fired) — defense-in-depth against a slow leak.
            if let Some(bufs) = &self.log_buffers
                && let Some(drv_path) = self.dag.path_for_hash(&drv_hash)
            {
                bufs.discard(drv_path);
            }
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
}
