//! Periodic Tick housekeeping: orphan-watcher sweep, poison-TTL
//! expiry, per-build timeouts, derivation-row GC, gauge publish, SLA
//! estimator refresh, and the open pull-attempt establishment sweep —
//! the single scheduler-side time-based repair the pull path keeps.
//!
//! Split from `executor.rs` — that module is now the requeue
//! chokepoint for derivations whose executor is gone (the stream-era
//! lifecycle it once held was deleted); the `tick_*` fns here are
//! periodic maintenance that happens to run from the same actor loop.

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
    }

    pub(super) async fn handle_tick(&mut self) {
        // r[impl sched.lease.standby-tick-noop+2]
        // Standby keeps stale self.builds/dag until LeaderLost lands;
        // every tick_* below either writes PG (orphan-cancel, build-
        // timeout, establishment, poison-clear, derivations-gc) or
        // reads stale state. dispatch_ready (:108)
        // and gRPC (r[sched.grpc.leader-guard]) already gate; this
        // closes the Tick path. A 2-replica deploy with one lease flap
        // would otherwise let the ex-leader cancel every Active build
        // 5min later (orphan grace) — db.update_build_status has no
        // fence in its WHERE clause.
        if !self.leader.is_leader() {
            return;
        }
        self.maybe_refresh_estimator().await;

        // Ordering is load-bearing: per-build-timeout cancels whole
        // builds (permanent failure) before poison-expire removes DAG
        // nodes.
        let expired_poisons = self.tick_scan_dag();

        // r[impl sched.attempt.establishment-window+5]
        // The destructive block (merged_bug_210): every tick below
        // either writes PG or decides from "not in / stale in the
        // DAG" inferences. All of them take the DagAuthority witness,
        // minted HERE and only here — an un-authoritative DAG
        // (pre-recovery, failed recovery) repairs nothing: closing
        // attempts, cancelling builds, or GC'ing rows from an empty
        // non-authoritative DAG destroys a healthy predecessor's
        // state. Observe-only work (estimator refresh, the poison
        // scan) stays above; the snapshot/gauge tail below is also
        // skipped — publishing zeros computed from a DAG that is not
        // ground truth is fabricated telemetry.
        let Some(authority) = self.dag_authority() else {
            debug!(
                "DAG not authoritative (pre-recovery or failed recovery); \
                 destructive housekeeping skipped this tick"
            );
            return;
        };
        self.tick_check_build_timeouts(&authority).await;
        self.tick_recheck_stuck_completions(&authority).await;
        self.tick_check_orphaned_builds(&authority).await;
        self.tick_process_expired_poisons(expired_poisons, &authority)
            .await;

        self.tick_gc_orphan_derivations(&authority).await;
        self.tick_gc_attempt_ledger(&authority).await;
        self.tick_gc_materialization_jobs(&authority).await;
        self.tick_gc_build_wanted_outputs(&authority).await;
        self.tick_sweep_dispatched_cells(&authority);
        self.tick_flush_status_outbox(&authority).await;
        self.tick_sweep_open_pull_attempts(&authority).await;
        // Materialization sweeps: cancel jobs whose derivation has no
        // live interest left, closing open attempts charge-free; then
        // the PD-20 parked-job arm — re-evaluate parked jobs whose
        // nodes have buildable dependency closures (resolve
        // from-source) and publish the stalled gauge from ground truth.
        self.tick_backstop_materialization_jobs(&authority).await;
        self.tick_cancel_zero_interest_materialization(&authority)
            .await;
        self.tick_reevaluate_parked_materialization_jobs(&authority)
            .await;
        self.tick_retry_pending_carriers(&authority).await;

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
        let snapshot = Arc::new(self.compute_cluster_snapshot());
        // r[impl obs.metric.scheduler-substituting]
        // The materialization backlog, scrapeable: the SAME quantity
        // `ClusterStatus.substituting_derivations` reports
        // (sched.admin.snapshot-substituting), published from the
        // snapshot just computed so gauge and proto field can never
        // disagree. This is the leading KEDA store-scaling signal —
        // pending-job backlog is visible to Prometheus before any
        // store replica claims the work. Leader-gated like every
        // state gauge here (the handle_tick early-return above);
        // zeroed once by handle_leader_lost.
        crate::observability::LeaderGauge::SubstitutingDerivations
            .set(f64::from(snapshot.substituting_derivations));
        // A2.4 (bug_217): the state gauges are single-sourced from the
        // SAME snapshot the proto fields serve — gauge and
        // `ClusterStatus` cannot diverge (the pre-fix standalone gauge
        // publisher recomputed them independently and counted pending-job Ready
        // nodes as builder-queue depth, the has_pending_unclaimed_job
        // omission). Leader-gated by the handle_tick early-return;
        // zeroed by handle_leader_lost.
        crate::observability::LeaderGauge::DerivationsQueued
            .set(f64::from(snapshot.queued_derivations));
        crate::observability::LeaderGauge::BuildsActive.set(f64::from(snapshot.active_builds));
        crate::observability::LeaderGauge::DerivationsRunning
            .set(f64::from(snapshot.running_derivations));
        self.snapshot_tx.send_replace(snapshot);
    }

    // -----------------------------------------------------------------------
    // handle_tick helpers — one per periodic check
    // -----------------------------------------------------------------------

    /// Single DAG pass collecting poison-TTL expiries. The stream-era
    /// backstop-timeout scan that used to share this pass is gone with
    /// the session machinery — a stuck pull-mode attempt is bounded by
    /// the Job's `activeDeadlineSeconds` and resolved by the
    /// establishment sweep ([`Self::tick_sweep_open_pull_attempts`]).
    fn tick_scan_dag(&self) -> Vec<DrvHash> {
        let mut expired_poisons: Vec<DrvHash> = Vec::new();
        for (drv_hash, state) in self.dag.iter_nodes() {
            if state.status() == DerivationStatus::Poisoned
                && let Some(poisoned_at) = state.retry.poisoned_at
                && poisoned_at.elapsed() > POISON_TTL
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
    async fn tick_check_build_timeouts(&mut self, _authority: &super::DagAuthority) {
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

            // Record the BUILD-level failure FIRST so the terminal
            // capture in transition_build picks it up for the
            // BuildFailed event + persisted row. Whole-struct override:
            // a per-build timeout is about the BUILD, so the culprit
            // derivation is structurally None — the previous
            // independent-field overwrite kept a stale failed_derivation
            // from an earlier per-drv failure spliced next to the
            // timeout summary (merged_bug_036).
            if let Some(build) = self.builds.get_mut(&build_id) {
                build.override_failure_build_level(
                    reason.clone(),
                    rio_proto::types::BuildResultStatus::TimedOut,
                );
            }
            // Reuse the CancelBuild derivation-cancellation path (transitions
            // drvs to Cancelled — the controller's Job deletion aborts
            // in-flight pods — removes build interest, revokes Ready
            // claimability). Then fail the BUILD instead
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
    async fn tick_recheck_stuck_completions(&mut self, _authority: &super::DagAuthority) {
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
    async fn tick_check_orphaned_builds(&mut self, _authority: &super::DagAuthority) {
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
    async fn tick_process_expired_poisons(
        &mut self,
        expired_poisons: Vec<DrvHash>,
        _authority: &super::DagAuthority,
    ) {
        // Surviving parents of the removed children, collected across
        // the loop for the survivor re-evaluation below (the TTL-sweep
        // twin of the admin ClearPoison wake).
        let mut surviving_parents: Vec<DrvHash> = Vec::new();
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
            match self
                .record_reset_with_clear_poison(&drv_hash, reset_row)
                .await
            {
                Ok(
                    crate::db::FencedOutcome::Applied(_)
                    | crate::db::FencedOutcome::AlreadyResolved,
                ) => {}
                // r[impl sched.evidence.durability+4]
                // Fenced: deposed replica. The PG clear did not happen,
                // so the PG-first contract skips the in-memory removal
                // exactly like the PG-failure arm — the successor owns
                // this poison's lifecycle now.
                Ok(crate::db::FencedOutcome::Fenced) => {
                    continue;
                }
                Err(e) => {
                    error!(drv_hash = %drv_hash, error = %e, "failed to clear poison in PG");
                    continue;
                }
            }
            // Capture the parents AFTER the PG clear succeeded (only
            // then is the child actually removed below) and BEFORE
            // `remove_node` scrubs the edge maps.
            surviving_parents.extend(self.dag.get_parents(&drv_hash));
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
        surviving_parents.sort();
        surviving_parents.dedup();
        // r[impl sched.poison.clear-survivor-reevaluation+2]
        // Wake the surviving parents (the TTL-sweep twin of the admin
        // ClearPoison hook): settle marked-Broken survivors, promote
        // Queued ones whose deps are now (vacuously) satisfied. A parent
        // the recovery condemnation spared on co-ownership grounds waits
        // Queued above its non-co-owned poisoned child until exactly this
        // sweep fires — without the re-evaluation it would sit there
        // forever (`find_newly_ready` fires only on completions) and its
        // build would hang. Leader-only: `handle_tick` no-ops on standby.
        self.reevaluate_removal_survivors(&surviving_parents).await;
    }

    /// Retry the leader-scoped realized-path carrier stash
    /// (merged_bug_257): re-attempt the stale-reset job creation until
    /// the row applies; drop — counted and warned — once the node is
    /// terminal/gone (the carrier has no consumer left).
    async fn tick_retry_pending_carriers(&mut self, _authority: &super::DagAuthority) {
        if self.pending_carriers.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending_carriers);
        for (drv_hash, carried) in pending {
            let alive = self
                .dag
                .node(&drv_hash)
                .is_some_and(|s| !s.status().is_terminal());
            if !alive {
                metrics::counter!("rio_scheduler_materialization_carrier_dropped_total")
                    .increment(1);
                warn!(
                    drv_hash = %drv_hash,
                    "carried realized paths dropped: node terminal/gone before \
                     the stale-reset job row applied"
                );
                continue;
            }
            let carried_opt = (!carried.is_empty()).then(|| carried.clone());
            if !self
                .create_materialization_job(
                    &drv_hash,
                    crate::state::JobOrigin::StaleReset,
                    None,
                    carried_opt,
                )
                .await
            {
                self.pending_carriers.push((drv_hash, carried));
            }
        }
    }

    /// DAG-state sweep for `dispatched_cells`. The arm-on-ack write
    /// (`handle_ack_spawned_intents`) can't fire for a drv that was
    /// acked then cancelled / substituted / dependency-failed before
    /// its pod's first pull, so the pull-mint remove path (the §13a
    /// ICE-clear in `actor/pull.rs`) never runs for it. Retain only
    /// entries whose DAG node is still in a pre-terminal state where
    /// a pull is plausible. Cheap: `dispatched_cells` is bounded by
    /// acked-but-not-yet-pulled drvs (≪ DAG size).
    fn tick_sweep_dispatched_cells(&self, _authority: &super::DagAuthority) {
        use DerivationStatus::{Assigned, Ready, Running};
        self.dispatched_cells.retain(|k, _| {
            self.dag
                .node(k)
                .is_some_and(|s| matches!(s.status(), Ready | Assigned | Running))
        });
    }

    // r[impl sched.db.derivations-gc+4]
    /// I-169.2: periodic sweep of orphan-terminal `derivations` rows.
    /// Every 30th tick (~5min at the default 10s interval) → delete
    /// ≤1000. A 1.16M backlog drains in ~4 days; steady-state churn
    /// (terminal nodes per 5min from failed closures) is well under the
    /// batch cap. Best-effort: PG error logs and retries next interval.
    async fn tick_gc_orphan_derivations(&self, _authority: &super::DagAuthority) {
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

    // r[impl sched.db.attempts-gc]
    /// Attempt-ledger retention sweep (decision P8, phase 2): every
    /// 30th tick (~5min at the default 10s interval) → delete ≤1000
    /// rows per arm. Suffix-complement only — the kernel-proved
    /// eligibility (attempt-kind, strictly pre-reset, past the horizon,
    /// no active assignment) plus orphaned histories; the decision
    /// suffix every loader returns is bit-identical before/after.
    /// Leader-only via `handle_tick`'s standby early-return. Best-
    /// effort: PG error logs and retries next interval.
    ///
    /// Module consts, not config knobs (the derivations-GC precedent:
    /// "Making this tunable is YAGNI until someone asks"). The one
    /// value that MUST track live configuration — the infra retry
    /// window — flows through `decision_budget()` into the horizon.
    ///
    /// `pub(super)` for the actor-boundary smoke test, which drives
    /// this directly (the `maybe_refresh_estimator` precedent).
    pub(super) async fn tick_gc_attempt_ledger(&self, _authority: &super::DagAuthority) {
        const ATTEMPTS_GC_EVERY: u64 = 30;
        const ATTEMPTS_GC_BATCH: i64 = 1000;
        if !self.tick_count.is_multiple_of(ATTEMPTS_GC_EVERY) {
            return;
        }
        let horizon = crate::retry_policy::sweep_horizon_secs(
            &self.decision_budget(),
            crate::db::attempts::LEDGER_RETENTION_FLOOR.as_secs(),
        );
        #[allow(clippy::cast_precision_loss)] // horizon ≪ 2^52 s
        match self
            .db
            .gc_attempt_ledger(horizon as f64, ATTEMPTS_GC_BATCH)
            .await
        {
            Ok(0) => {}
            Ok(n) => {
                debug!(
                    deleted = n,
                    "GC'd attempt-ledger rows past the retention horizon"
                );
                metrics::counter!("rio_scheduler_attempts_gc_deleted_total").increment(n);
            }
            Err(e) => {
                warn!(error = %e, "attempt-ledger GC sweep failed; retrying next interval");
            }
        }

        // The execution-row GC rides the same cadence, AFTER the ledger
        // pass: it deletes only rows the ledger no longer references,
        // so one tick can collect an exec row whose last ledger row
        // aged out in that same pass. Conjunction documented at
        // rio_retry_kernel::exec_row_sweep_eligible; the SQL twin is
        // SchedulerDb::gc_exec_rows.
        // r[impl store.log.sweep-ownership+1]
        let retention_secs = f64::from(self.exec_retention_days) * 86_400.0;
        match self
            .db
            .gc_exec_rows(retention_secs, ATTEMPTS_GC_BATCH)
            .await
        {
            Ok(0) => {}
            Ok(n) => {
                debug!(
                    deleted = n,
                    "GC'd terminal unreferenced drv_executions rows past retention"
                );
                metrics::counter!("rio_scheduler_exec_rows_gc_deleted_total").increment(n);
            }
            Err(e) => {
                warn!(error = %e, "execution-row GC sweep failed; retrying next interval");
            }
        }
    }

    /// D1/A6 (merged_bug_163): reap RESOLVED materialization jobs past the
    /// forensic horizon once unpinned and interest-free. Leader-only via
    /// handle_tick; same cadence/batch class as the attempt-ledger sweep.
    pub(super) async fn tick_gc_materialization_jobs(&self, _authority: &super::DagAuthority) {
        const MAT_JOBS_GC_EVERY: u64 = 30;
        const MAT_JOBS_GC_BATCH: i64 = 1000;
        if !self.tick_count.is_multiple_of(MAT_JOBS_GC_EVERY) {
            return;
        }
        // The same forensic window as the attempt ledger: resolved jobs
        // are debugging evidence, not decision inputs.
        let horizon = crate::db::attempts::LEDGER_RETENTION_FLOOR.as_secs();
        #[allow(clippy::cast_precision_loss)] // horizon ≪ 2^52 s
        match self
            .db
            .gc_resolved_materialization_jobs(
                horizon as f64,
                MAT_JOBS_GC_BATCH,
                self.serving_generation(),
            )
            .await
        {
            Ok(0) => {}
            Ok(n) => {
                debug!(
                    deleted = n,
                    "GC'd resolved materialization jobs past the retention horizon"
                );
                metrics::counter!("rio_scheduler_materialization_jobs_gc_total").increment(n);
            }
            Err(e) => {
                warn!(error = %e, "materialization-jobs GC sweep failed; retrying next interval");
            }
        }
    }

    /// D1/A6 (merged_bug_163): reap build_wanted_outputs rows whose build
    /// is long-terminal (or gone). Leader-only via handle_tick.
    pub(super) async fn tick_gc_build_wanted_outputs(&self, _authority: &super::DagAuthority) {
        const WANTED_GC_EVERY: u64 = 30;
        const WANTED_GC_BATCH: i64 = 1000;
        if !self.tick_count.is_multiple_of(WANTED_GC_EVERY) {
            return;
        }
        let horizon = crate::db::attempts::LEDGER_RETENTION_FLOOR.as_secs();
        #[allow(clippy::cast_precision_loss)] // horizon ≪ 2^52 s
        match self
            .db
            .gc_dead_build_wanted_outputs(
                horizon as f64,
                WANTED_GC_BATCH,
                self.serving_generation(),
            )
            .await
        {
            Ok(0) => {}
            Ok(n) => {
                debug!(deleted = n, "GC'd wanted-output rows for dead builds");
                metrics::counter!("rio_scheduler_wanted_outputs_gc_total").increment(n);
            }
            Err(e) => {
                warn!(error = %e, "build-wanted-outputs GC sweep failed; retrying next interval");
            }
        }
    }

    // r[impl sched.attempt.cancel-close-driven+1]
    /// Re-drive failed status-batch persists. FIFO; drains the WHOLE
    /// queue on Ok (a healed PG clears the backlog in one tick) and
    /// fail-fasts on the first Err/Fenced (a dead PG costs one attempt
    /// per tick — the throttle rationale applies to failures only;
    /// pre-fix it throttled the success path to 6 batches/minute,
    /// scaling the stale-replay window linearly with depth).
    ///
    /// Each batch is re-derived against the authoritative in-memory
    /// DAG before replay (we hold the `DagAuthority` witness):
    /// - KEEP a derivation whose node still carries the latched
    ///   status (present-equal: the latch is still the truth), or
    ///   whose node left the DAG (absent: terminal cleanup reaps only
    ///   in-memory-terminal nodes, so the latched terminal status IS
    ///   the node's last truth and the close must still land);
    /// - DROP a derivation whose node is present with a DIFFERENT
    ///   status (resubmit reset or advanced): replaying the latch
    ///   would regress newer state (merged_bug_011 — a stale
    ///   Cancelled batch rewrote a resubmitted build to cancelled and
    ///   force-closed its fresh attempt).
    ///
    /// The replay goes through `replay_status_batch_guarded`, whose
    /// assignment close is scoped to the LATCHED exec_ids — never the
    /// derivation — so a successor attempt is untouchable by
    /// construction. Leader-gated by `handle_tick`; the outbox is
    /// cleared on leadership loss in `clear_persisted_state`.
    // r[impl sched.attempt.cancel-close-driven+1]
    async fn tick_flush_status_outbox(&mut self, _authority: &super::DagAuthority) {
        while let Some(front) = self.status_outbox.front() {
            let age = front.enqueued_at.elapsed();
            if age > std::time::Duration::from_secs(300) {
                warn!(
                    depth = self.status_outbox.len(),
                    age_secs = age.as_secs(),
                    "status outbox head is old; PG persists keep failing \
                     (the latched batches' attempt rows stay open until this drains)"
                );
            }
            let batch = self
                .status_outbox
                .pop_front()
                .expect("front() was Some above");
            // Flush-time re-derivation vs the live DAG.
            let (kept, dropped): (Vec<&str>, Vec<&str>) = batch
                .drv_hashes
                .iter()
                .map(String::as_str)
                .partition(|h| match self.dag.node(h) {
                    None => true,
                    Some(s) => s.status() == batch.status,
                });
            if !dropped.is_empty() {
                info!(
                    dropped = dropped.len(),
                    status = ?batch.status,
                    drvs = ?dropped,
                    "status outbox: dropped stale entries (nodes advanced past the latch)"
                );
            }
            if kept.is_empty() {
                continue;
            }
            match self
                .db
                .replay_status_batch_guarded(
                    &kept,
                    batch.status,
                    &batch.exec_ids,
                    self.serving_generation(),
                )
                .await
            {
                Ok(crate::db::FencedOutcome::Fenced) => {
                    // Deposed mid-tick: keep the entry for visibility;
                    // LeaderLost clears the whole outbox momentarily.
                    self.note_fenced_evidence_write("status outbox flush");
                    self.status_outbox.push_front(batch);
                    break;
                }
                Ok(_) => {
                    info!(
                        count = kept.len(),
                        status = ?batch.status,
                        remaining = self.status_outbox.len(),
                        "status outbox: batch flushed (latched attempt rows closed)"
                    );
                }
                Err(e) => {
                    warn!(count = kept.len(), error = %e,
                          "status outbox: flush failed; retrying next tick");
                    self.status_outbox.push_front(batch);
                    break;
                }
            }
        }
        metrics::gauge!("rio_scheduler_status_outbox_depth").set(self.status_outbox.len() as f64);
    }

    /// Establishment sweep for open pull-mode attempts — the single
    /// scheduler-side time-based repair the pull path keeps
    /// (merged_bug_004: this doc anchors HERE, on the sweep itself,
    /// not on the outbox flusher above it). Every open attempt (the
    /// durable view) is visited every sweep; one whose age exceeds its
    /// intent deadline plus `establishment_report_slack` with no
    /// terminal row is resolved by the store-probe arm (all verifiable
    /// wanted outputs present → adopted as completed, never charged)
    /// or established exactly once as an unreported executor crash
    /// (charged through the same append+decide discipline as every
    /// other establishment vehicle) and requeued. Also refreshes
    /// `rio_scheduler_open_attempts` (one query serves both; the gauge
    /// is durable-backed so it survives failover exactly like the rows
    /// it counts). Leader-only via the `handle_tick` early-return plus
    /// the `DagAuthority` witness; the establishing transaction
    /// additionally carries the same generation-floor fence as the
    /// pull transaction.
    // r[impl sched.attempt.establishment-window+5]
    pub(super) async fn tick_sweep_open_pull_attempts(&mut self, _authority: &super::DagAuthority) {
        let opens = match self.db.list_open_pull_attempts().await {
            Ok(rows) => rows,
            Err(e) => {
                debug!(error = %e, "open pull-attempt sweep: view query failed; retrying next tick");
                return;
            }
        };
        // A2.4 (bug_217): the busy-fleet gauge is the BUILD lane only —
        // one open build attempt = one builder pod slot (the
        // workers_active successor and the one-attempt-per-pod
        // ScaledObject contract). Store materialization claims get
        // their own series; counting them here inflated every consumer
        // sized off the builder fleet during substitution waves.
        crate::observability::LeaderGauge::OpenAttempts.set(opens.build.len() as f64);
        crate::observability::LeaderGauge::OpenMaterializationAttempts
            .set(opens.materialization.len() as f64);
        // Skew tripwires over the SAME snapshot, both polarities
        // (merged_bug_307 rider + merged_bug_055 C — the
        // `claimed_by.is_some() ⇒ continue` blinder is gone):
        //
        // polarity=split_release — a pending-unclaimed view entry
        // whose node is still Assigned/Running with NO open
        // materialization assignment: `release_claim` should have
        // requeued the node in the same step that dropped the claim.
        // Counted (and fatal under debug builds) so a re-introduced
        // split release is observable instead of a silent
        // NotYetReady-forever.
        //
        // polarity=claimed_no_attempt — the INVERSE ghost: a view
        // entry still CLAIMED while no open materialization assignment
        // exists (the attempt closed without its companion clearing
        // the holder — any future bypass of the sealed witness
        // pipeline, a crash between close-commit and view update, a
        // failed companion before the release fallback existed). The
        // listing's claimed-filter hides such a job from every replica
        // forever. LEVEL-TRIGGERED REPAIR, not just a counter: release
        // the claim uncharged through the same atomic
        // release+requeue path the consumption companions use — the
        // whole ghost family self-heals here regardless of producer.
        // Two-strike structural guard (see `claimed_unbacked_strike`):
        // a claim minted between the rows snapshot and this iteration
        // gets one full sweep to appear before it can be called a
        // ghost. No debug_assert on this polarity BY DECISION: the
        // ghost is a repairable runtime condition with live producers
        // (crash windows), not a pure code-regression shape — a fatal
        // assert would make the self-healing lane untestable and turn
        // recoverable skew into debug-build crashes.
        let ghosts: Vec<(crate::state::DrvHash, crate::state::ExecutorId)> = {
            use crate::state::DerivationStatus::{Assigned, Running};
            let mat_open: std::collections::HashSet<&str> = opens
                .materialization
                .iter()
                .map(|a| a.drv_hash.as_str())
                .collect();
            let mut ghosts = Vec::new();
            let mut first_strikes: Vec<crate::state::DrvHash> = Vec::new();
            let mut cleared_strikes: Vec<crate::state::DrvHash> = Vec::new();
            for (drv_hash, entry) in self.materialization_jobs.iter() {
                match entry.holder() {
                    Some(holder) => {
                        if mat_open.contains(drv_hash.as_str()) {
                            // Backed claim: reset any stale strike.
                            if entry.strike_armed() {
                                cleared_strikes.push(drv_hash.clone());
                            }
                        } else if entry.strike_armed() {
                            // Second consecutive unbacked observation:
                            // the claimed-no-attempt ghost.
                            ghosts.push((drv_hash.clone(), holder.clone()));
                        } else {
                            first_strikes.push(drv_hash.clone());
                        }
                    }
                    None => {
                        let wedged = self
                            .dag
                            .node(drv_hash.as_str())
                            .is_some_and(|s| matches!(s.status(), Assigned | Running))
                            && !mat_open.contains(drv_hash.as_str());
                        if wedged {
                            metrics::counter!(
                                "rio_scheduler_materialization_view_node_skew_total",
                                "polarity" => "split_release"
                            )
                            .increment(1);
                            debug_assert!(
                                false,
                                "pending-unclaimed materialization job for {drv_hash} with \
                                 node Assigned/Running and no open assignment \
                                 (split-release wedge)"
                            );
                        }
                    }
                }
            }
            for drv_hash in first_strikes {
                if let Some(entry) = self.materialization_jobs.get_mut(&drv_hash) {
                    entry.set_strike(true);
                }
            }
            for drv_hash in cleared_strikes {
                if let Some(entry) = self.materialization_jobs.get_mut(&drv_hash) {
                    entry.set_strike(false);
                }
            }
            ghosts
        };
        // r[impl sched.materialize.claim-coherence]
        for (drv_hash, holder) in ghosts {
            metrics::counter!(
                "rio_scheduler_materialization_view_node_skew_total",
                "polarity" => "claimed_no_attempt"
            )
            .increment(1);
            warn!(
                drv_hash = %drv_hash, holder = %holder,
                "claimed-no-attempt ghost (two sweeps unbacked): releasing the claim \
                 uncharged — the job returns to the listing"
            );
            self.release_claim(&drv_hash, &holder).await;
        }
        if opens.build.is_empty() && opens.materialization.is_empty() {
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
        // Per-kind windows (A2.4): the BUILD window is
        // max(persisted, sweep-time re-solve) — widen-only, as before.
        // The MATERIALIZATION window is persisted-only: a store walk
        // runs under `materialization.attempt_deadline_secs`, never a
        // build pod's solve — re-solving a BUILD deadline for a store
        // claim either widened the window by a minutes-scale solve or
        // (post-A2.3) compared apples to the store anchor.
        let mut expired: Vec<crate::db::open_attempts::OpenAttemptRow> = opens
            .build
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
        expired.extend(opens.materialization.into_iter().filter(|attempt| {
            attempt.age_secs > attempt.deadline_secs.unwrap_or(0.0) + slack_secs
        }));
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
        let probe = self.batch_probe_orphan_outputs(probe_paths).await;
        for attempt in expired {
            self.establish_open_pull_attempt(&attempt, &probe).await;
        }
    }

    /// Resolve one expired open pull-mode attempt: adopt (store-probe
    /// arm) or establish + requeue (C2 charge arm). See
    /// [`Self::tick_sweep_open_pull_attempts`].
    async fn establish_open_pull_attempt(
        &mut self,
        attempt: &crate::db::open_attempts::OpenAttemptRow,
        probe: &super::recovery::StoreProbe,
    ) {
        // Standby replicas must neither write attempt rows nor decide
        // from them (the same gate every establishment vehicle carries).
        if !self.leader.is_leader() {
            return;
        }
        let drv_hash = DrvHash::from(attempt.drv_hash.as_str());
        let executor = ExecutorId::from(attempt.executor_id.as_str());

        // ONE kernel call dispositions the attempt
        // (sched.attempt.establishment-window+5: every expired attempt
        // routes through the total establishment kernel; §4.R2: this is
        // the kernel's single scheduler call site).
        use rio_evidence_kernel::establish::{
            EstablishmentAction, NodeProjection, NodeStatusClass, ProbeEvidence,
            establish_expired_attempt, project_node,
        };
        let kind = if attempt.attempt_kind == crate::state::AttemptKind::Materialization.as_str() {
            rio_evidence_kernel::pull::PullKind::Materialization
        } else {
            rio_evidence_kernel::pull::PullKind::Build
        };
        // Node axis: a charge needs a LIVE wanting node
        // (merged_bug_210). The status map is a no-wildcard exhaustive
        // match — a new DerivationStatus must name its cell. Failed is
        // Live (retriable per is_terminal: the node re-enters
        // dispatch); the settled terminals close charge-free in the
        // kernel; Cancelled covers the failed-persist window (the
        // cancel transitioned in-memory but the closing persist is
        // still outboxed); not-in-DAG projects to Absent ONLY under
        // DAG authority (the sweep runs under `repair`'s DagAuthority
        // witness, but the projection re-checks — defense in depth
        // against a future caller outside the gate).
        let status_class = self.dag.node(&drv_hash).map(|s| match s.status() {
            DerivationStatus::Created
            | DerivationStatus::Queued
            | DerivationStatus::Ready
            | DerivationStatus::Assigned
            | DerivationStatus::Running
            | DerivationStatus::Failed => NodeStatusClass::Live,
            DerivationStatus::Cancelled => NodeStatusClass::Cancelled,
            DerivationStatus::Completed
            | DerivationStatus::Poisoned
            | DerivationStatus::DependencyFailed
            | DerivationStatus::Skipped => NodeStatusClass::Settled,
        });
        let node = match project_node(status_class, self.dag_authoritative) {
            NodeProjection::Node(node) => node,
            NodeProjection::NoAuthority => {
                // Un-authoritative DAG: no node-axis inference, no
                // destructive establishment. The attempt stays open
                // for an authoritative pass.
                debug!(drv_hash = %attempt.drv_hash, exec_id = %attempt.exec_id,
                       "establishment sweep: DAG not authoritative; attempt left open");
                return;
            }
        };
        // Probe axis (merged_bug_232, the §4.R2 split this workstream
        // owns): a failed probe is Unavailable — the kernel DEFERS a
        // build establishment (absence of evidence is not evidence of
        // absence; the attempt stays open for a pass with a working
        // probe). Only the no-client deployment shape charges without
        // a probe.
        let probe = match probe {
            super::recovery::StoreProbe::Verified(m) => ProbeEvidence::Verified(m),
            super::recovery::StoreProbe::Unavailable => ProbeEvidence::Unavailable,
            super::recovery::StoreProbe::NoClient => ProbeEvidence::NoStoreConfigured,
        };
        // The wanted set is the LIVE effective resolution (T-D2.3: the
        // rebuilt in-memory union over live builds' durable
        // contributions, saturating-absent). Owned so the kernel call
        // borrows nothing from the DAG.
        let verifiable_owned: Option<Vec<String>> = self.dag.node(&drv_hash).and_then(|state| {
            let eff = crate::state::effective_wanted(state, &self.builds);
            verifiable_wanted_paths(
                &state.output_names,
                &state.expected_output_paths,
                eff.as_deref().unwrap_or(&[]),
            )
            .map(|v| v.iter().map(|p| p.to_string()).collect())
        });
        let verifiable_refs: Option<Vec<&str>> = verifiable_owned
            .as_ref()
            .map(|v| v.iter().map(String::as_str).collect());

        match establish_expired_attempt(kind, node, probe, verifiable_refs.as_deref()) {
            EstablishmentAction::CloseChargeFree => {
                // r[impl sched.attempt.cancel-close-driven+1]
                // Nobody wants this work any more: close the assignment
                // row and write NOTHING else — no AttemptRow (no
                // exclusion seed), no pull_establishments_total (the
                // OA2 clustering and the alert population stay clean),
                // no push_attempt_record. Fenced + exec_id-scoped (the
                // FencedTx capability): a deposed leader closes
                // nothing.
                // The close CAUSE is truthful per cell: settled work
                // closes with its settled polarity (a completed node's
                // stale row reads `completed`, a poisoned/dep-failed
                // node's reads `failed`); cancelled/absent close as
                // `cancelled` (work nobody wants). Consumers of
                // recently_closed select on cause.
                let close_status = match node {
                    rio_evidence_kernel::establish::NodeDisposition::TerminalSettled => {
                        match status_class {
                            Some(NodeStatusClass::Settled) => {
                                match self.dag.node(&drv_hash).map(|s| s.status()) {
                                    Some(
                                        DerivationStatus::Completed | DerivationStatus::Skipped,
                                    ) => crate::db::AssignmentCloseStatus::Completed,
                                    _ => crate::db::AssignmentCloseStatus::Failed,
                                }
                            }
                            _ => crate::db::AssignmentCloseStatus::Cancelled,
                        }
                    }
                    _ => crate::db::AssignmentCloseStatus::Cancelled,
                };
                match self
                    .db
                    .close_assignment_fenced(
                        attempt.exec_id,
                        close_status,
                        self.serving_generation(),
                    )
                    .await
                {
                    Ok(o) if o.settled() => {
                        info!(
                            drv_hash = %attempt.drv_hash, exec_id = %attempt.exec_id,
                            node = ?node,
                            "establishment sweep: cancelled/settled/absent node; assignment closed charge-free"
                        );
                        // merged_bug_055 C, producer 3 fixed directly:
                        // a settled charge-free close of a
                        // MATERIALIZATION-kind attempt must clear the
                        // view holder in the same step — otherwise the
                        // entry stays claimed with its attempt closed
                        // (the claimed-no-attempt ghost the two-strike
                        // tripwire above would otherwise repair two
                        // sweeps later).
                        if kind == rio_evidence_kernel::pull::PullKind::Materialization
                            && let Some(entry) =
                                self.materialization_jobs.get_mut(attempt.drv_hash.as_str())
                        {
                            // Compare-and-clear on the CLOSED attempt's
                            // executor (bug_170 rider): if a fresh
                            // claim was already minted to someone
                            // else, this late charge-free close must
                            // not strip it.
                            let holder =
                                crate::state::ExecutorId::from(attempt.executor_id.as_str());
                            let _ = entry.release_claim_if_held(&holder);
                        }
                    }
                    Ok(_) => {
                        self.note_fenced_evidence_write("establishment charge-free close");
                    }
                    Err(e) => warn!(
                        drv_hash = %attempt.drv_hash, error = %e,
                        "establishment sweep: charge-free close failed; the attempt stays open for this pass"
                    ),
                }
                return;
            }
            EstablishmentAction::ChargeMaterializationInfra => {
                // Substitution-replacement (design §2.4 / findings
                // BC-2, BC-3): the materialization kind has NO adopt
                // arm (a mid-walk crash leaves outputs present but the
                // closure incomplete — adopting would fabricate a
                // closure-incomplete completion), and its charge class
                // is materialization_infra (counts toward the
                // materialization budget and toward NOTHING else),
                // never executor_crash.
                // r[impl sched.materialize.routing+5]
                self.establish_materialization_attempt(attempt).await;
                return;
            }
            EstablishmentAction::AdoptCompleted(verified) => {
                // Store-probe arm: every verifiable wanted output
                // present → adopt as completed; the attempt is closed
                // and never charged. The adopt stamps EXACTLY the
                // kernel's VerifiedPresent witness (bug_148: the
                // expected_output_paths superset contained paths the
                // same probe had just reported absent).
                if self.dag.node(&drv_hash).is_some() {
                    self.adopt_orphan_completion(&drv_hash, &Some(executor.clone()), verified)
                        .await;
                }
                // r[impl sched.evidence.durability+4]
                // Fenced + exec_id-scoped: a deposed replica's sweep
                // can no longer close a successor's re-minted
                // assignment row.
                match self
                    .db
                    .close_assignment_fenced(
                        attempt.exec_id,
                        crate::db::AssignmentCloseStatus::Completed,
                        self.serving_generation(),
                    )
                    .await
                {
                    Ok(o) if o.settled() => {}
                    Ok(_) => {
                        self.note_fenced_evidence_write("establishment adopt assignment close");
                    }
                    Err(e) => {
                        warn!(drv_hash = %drv_hash, error = %e,
                              "establishment adopt: failed to close the assignment row");
                    }
                }
                info!(drv_hash = %drv_hash, exec_id = %attempt.exec_id,
                      "establishment sweep: outputs present in store, adopted as completed (no charge)");
                return;
            }
            EstablishmentAction::Defer => {
                // No probe evidence in either direction: the attempt
                // stays open for a pass with a working probe. Produced
                // when the batch FindMissingPaths probe fails or times
                // out (StoreProbe::Unavailable → the kernel's
                // Unavailable axis) for a live-wanted BUILD attempt —
                // the merged_bug_232 fix; reachability is pinned
                // executably by
                // `probe_unavailable_defers_build_establishment`
                // (merged_bug_005: this arm IS producible — the old
                // "cannot produce this arm" parenthetical predated the
                // probe-axis wiring).
                debug!(drv_hash = %attempt.drv_hash, exec_id = %attempt.exec_id,
                       "establishment sweep: probe evidence unavailable; attempt stays open");
                return;
            }
            EstablishmentAction::ChargeExecutorCrash => {
                // Fall through to the C2 charge arm below.
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
        let serving_generation = self.serving_generation;
        let mut row = crate::db::attempts::AttemptRow::new(
            attempt.derivation_id,
            OutcomeClass::ExecutorCrash,
            ReportingParty::Scheduler,
            crate::state::AttemptKind::Build,
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
            // r[impl sched.lease.generation-fence+3]
            // The same generation fence the pull transaction applies:
            // a below-floor serving generation writes nothing.
            let mut tx = match self.db.begin_fenced(serving_generation).await? {
                crate::db::FencedBegin::Fenced { .. } => return Ok(None),
                crate::db::FencedBegin::Open(ftx) => ftx,
            };
            let (won, decision) = self.append_and_decide_in_tx(tx.conn(), &row).await?;
            if won
                && verdict_eligible
                && matches!(decision.verdict, crate::retry_policy::Verdict::Poison(_))
            {
                crate::db::SchedulerDb::persist_poisoned_in_tx(tx.conn(), &drv_hash).await?;
            }
            tx.close_assignment(attempt.exec_id, crate::db::AssignmentCloseStatus::Failed)
                .await?;
            tx.commit().await?;
            Ok(Some((won, decision)))
        }
        .await;
        let (won, decision) = match result {
            Ok(Some(pair)) => pair,
            Ok(None) => {
                info!(drv_hash = %drv_hash, serving_generation = serving_generation.as_i64(),
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
