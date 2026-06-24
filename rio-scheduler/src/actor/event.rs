//! Build-event emission: per-build broadcast channels.
//!
//! [`BuildEventBus`] owns the per-build channel/debounce maps. `DagActor`
//! methods that need DAG lookups (`emit_progress`, `handle_forward_*`)
//! stay on `DagActor` and call into the bus.

use std::collections::HashMap;
use std::time::Instant;

use tokio::sync::broadcast;
use tracing::warn;
use uuid::Uuid;

use crate::state::DrvHash;

use super::{BUILD_EVENT_BUFFER_SIZE, DagActor, LOG_EVENT_BUFFER_SIZE};

/// Paired broadcast receivers for one build: state-transition events
/// (Derivation/Progress/Started/Completed/...) on `state`, build-log
/// batches on `log`. Split so log volume cannot evict state events from
/// the broadcast ring (`r[gw.activity.stop-parity]`).
///
/// `bridge_build_events` holds both for the lifetime of the gRPC stream;
/// orphan-watcher checks `receiver_count()` on the *state* sender only.
#[derive(Debug)]
pub struct BuildEventReceivers {
    pub state: broadcast::Receiver<rio_proto::types::BuildEvent>,
    pub log: broadcast::Receiver<rio_proto::types::BuildEvent>,
}

/// Minimum interval between `BuildProgress` emits for one build (I-140).
/// `emit_progress` → `build_summary` is O(dag_nodes); on a 153k-node DAG
/// that's ~15-60ms per call. Calling per-assign + per-complete +
/// per-disconnect under ephemeral-builder churn compounds to >100% actor
/// utilization. 250ms ≈ 4/s caps the scan rate well below the ~1s
/// dashboard poll cadence. Callers that already hold a fresh summary use
/// `emit_progress_with` directly (bypasses debounce — scan cost paid).
const PROGRESS_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(250);

/// Per-build event broadcast state. Sub-struct of [`DagActor`] —
/// single-owner actor, so no locking. Fields are `pub(super)` for the
/// handful of callers that need raw map access (watch_build subscribe,
/// orphan-watcher receiver counts); everything else goes through the
/// methods below.
pub(super) struct BuildEventBus {
    /// State-event broadcast channels (everything but the display-only
    /// kinds). Orphan-watcher checks `receiver_count()` on this sender.
    pub(super) channels: HashMap<Uuid, broadcast::Sender<rio_proto::types::BuildEvent>>,
    /// Display-only (`Event::SubstituteProgress`) broadcast channels —
    /// separate so display volume cannot lag the state ring and drop
    /// completions. See [`LOG_EVENT_BUFFER_SIZE`].
    pub(super) log_channels: HashMap<Uuid, broadcast::Sender<rio_proto::types::BuildEvent>>,
    /// Per-build last-BuildProgress emit time. `emit_progress` debounces
    /// against this — Progress is dashboard-only and `build_summary` is
    /// O(dag_nodes), so emitting on every assign/complete/disconnect at
    /// large-DAG × ephemeral-churn scale head-of-line blocks the actor
    /// (I-140). Cleared on build terminal/cleanup with the other maps.
    progress_at: HashMap<Uuid, Instant>,
}

impl BuildEventBus {
    pub(super) fn new() -> Self {
        Self {
            channels: HashMap::new(),
            log_channels: HashMap::new(),
            progress_at: HashMap::new(),
        }
    }

    /// Create fresh state + log broadcast channels for `build_id`.
    /// Returns both receivers (merge step 3 hands them to the
    /// SubmitBuild bridge; recovery drops them).
    pub(super) fn register(&mut self, build_id: Uuid) -> BuildEventReceivers {
        let (tx, state) = broadcast::channel(BUILD_EVENT_BUFFER_SIZE);
        let (log_tx, log) = broadcast::channel(LOG_EVENT_BUFFER_SIZE);
        self.channels.insert(build_id, tx);
        self.log_channels.insert(build_id, log_tx);
        BuildEventReceivers { state, log }
    }

    /// Subscribe to an existing build's state + log channels. `None` if
    /// no channel registered (build unknown / already cleaned up).
    /// `handle_watch_build` uses this for late-attach gateways.
    pub(super) fn subscribe(&self, build_id: Uuid) -> Option<BuildEventReceivers> {
        let state = self.channels.get(&build_id)?.subscribe();
        // log_channels is created/removed in lockstep with channels;
        // a None here would mean a maps-out-of-sync bug. Treat as
        // build-not-found rather than panicking.
        let log = self.log_channels.get(&build_id)?.subscribe();
        Some(BuildEventReceivers { state, log })
    }

    /// Drop all per-build state for `build_id` (channels + debounce).
    /// Called from terminal-cleanup and merge-rollback.
    pub(super) fn remove(&mut self, build_id: Uuid) {
        self.channels.remove(&build_id);
        self.log_channels.remove(&build_id);
        self.progress_at.remove(&build_id);
    }

    /// Reset to empty. Called from `clear_persisted_state` on leader
    /// transitions.
    pub(super) fn clear(&mut self) {
        self.channels.clear();
        self.log_channels.clear();
        self.progress_at.clear();
    }

    /// `true` if a Progress event for `build_id` was emitted within
    /// [`PROGRESS_DEBOUNCE`]. The `mark_progress` half is folded into
    /// `emit_progress_with`.
    pub(super) fn progress_debounced(&self, build_id: Uuid) -> bool {
        self.progress_at
            .get(&build_id)
            .is_some_and(|t| t.elapsed() < PROGRESS_DEBOUNCE)
    }

    /// Core emit: route to the right broadcast ring.
    ///
    /// Events carry no sequence numbers and are not persisted anywhere:
    /// the live broadcast is the only delivery channel, and a watcher
    /// that attaches later learns the net effect of missed events from
    /// the WatchBuild snapshot (`r[sched.watch.snapshot-first]`), not
    /// from history.
    pub(super) fn emit(&mut self, build_id: Uuid, event: rio_proto::types::build_event::Event) {
        use rio_proto::types::build_event::Event;

        let display_only = matches!(event, Event::SubstituteProgress(_));
        let build_event = rio_proto::types::BuildEvent {
            build_id: build_id.to_string(),
            timestamp: Some(prost_types::Timestamp::from(std::time::SystemTime::now())),
            event: Some(event),
        };

        // r[impl gw.activity.stop-parity]
        // Display-only events (SubstituteProgress) → log ring;
        // state-transition events → state ring. The bridge merges
        // both. This is the load-bearing split: display volume can lag
        // the *log* ring (acceptable — the terminal Cached/Completed
        // covers a dropped progress emit) but never the *state* ring,
        // so `DerivationEvent::Completed` is not evictable.
        let tx = if display_only {
            self.log_channels.get(&build_id)
        } else {
            self.channels.get(&build_id)
        };
        if let Some(tx) = tx {
            // broadcast::send returns Err only if there are no receivers, which is fine
            let _ = tx.send(build_event);
        }
    }

    /// Emit a `BuildProgress` from a precomputed summary, marking the
    /// debounce timestamp. Bypasses `progress_debounced` — the caller
    /// paid for the O(dag_nodes) scan, so emit unconditionally.
    ///
    /// Callers inside [`DagActor`] must go through
    /// [`DagActor::emit_progress_with`], which adds the terminal-build
    /// freeze (`sched.build.terminal-status-settled`); this bus-level
    /// emitter cannot see `DagActor::builds` and emits whatever it is
    /// handed.
    pub(super) fn emit_progress_with(
        &mut self,
        build_id: Uuid,
        summary: &crate::dag::BuildSummary,
        built: u32,
    ) {
        self.progress_at.insert(build_id, Instant::now());
        self.emit(
            build_id,
            rio_proto::types::build_event::Event::Progress(rio_proto::types::BuildProgress {
                completed: summary.completed,
                running: summary.running,
                queued: summary.queued,
                total: summary.total,
                critical_path_remaining_secs: Some(summary.critpath_remaining.round() as u64),
                assigned_executors: summary.assigned_executors.clone(),
                // The same summary.failed the snapshot serves — a live
                // Progress can no longer silently zero a failed count
                // the snapshot just reported (bug_304).
                failed: summary.failed,
                built,
            }),
        );
    }
}

impl DagActor {
    /// Emit a BuildProgress snapshot for a build.
    ///
    /// Computes fresh counts + critpath + workers via `build_summary()`
    /// (one O(nodes) pass). Call after state changes that affect the
    /// aggregate view — dispatch (running count + worker set changed)
    /// and completion (completed count changed + critpath dropped via
    /// `update_ancestors`). NOT called from recovery (recovery rebuilds
    /// state with no watchers attached; a reattaching watcher gets its
    /// counts from the WatchBuild snapshot).
    ///
    /// Why a separate event (not folding into DerivationEvent): the
    /// dashboard wants a single ETA number it can display without
    /// tracking state. Pushing the aggregate means the client stays
    /// dumb — no stateful reconstruction from the DerivationEvent
    /// stream.
    pub(super) fn emit_progress(&mut self, build_id: Uuid) {
        // r[impl sched.build.terminal-status-settled+3]
        // A terminal build's served progress is settled at its terminal
        // transition: shared-node fan-outs (release_downstream,
        // dispatch-time store hits, failure cascades) can still reach a
        // resident terminal build during the cleanup window, and a
        // post-`BuildCompleted` Progress recomputed from the mutating
        // DAG would reach live watchers (and the WatchBuild snapshot's
        // counts) with totals shrunk by whatever mutated the DAG since.
        if self.build_progress_frozen(build_id) {
            return;
        }
        // I-140: debounce. Progress is dashboard-only; `build_summary`
        // is O(dag_nodes). At 153k nodes that's ~60ms (debug) / ~15ms
        // (release) per call. Calling per-assign + per-complete +
        // per-disconnect under ephemeral-builder churn compounds to
        // >100% actor utilization → mailbox grows unboundedly → admin
        // RPCs timeout → builders idle-timeout with no assignment.
        // 250ms ≈ 4/s max; the dashboard's poll cadence is ~1s anyway.
        // The Tick-driven snapshot-sourced gauge re-emit provides the
        // floor for metrics; this is the per-watcher event stream.
        if self.events.progress_debounced(build_id) {
            return;
        }
        let summary = self.dag.build_summary(build_id);
        let built = self.builds.get(&build_id).map_or(0, |b| b.built_count);
        self.events.emit_progress_with(build_id, &summary, built);
    }

    /// Whether `BuildProgress` emission for this build is frozen because
    /// the build already settled into its terminal payload
    /// (`sched.build.terminal-status-settled`). Terminal builds stay
    /// resident — and in shared nodes' `interested_builds` — for the
    /// terminal-cleanup window, so aggregate-progress emitters must skip
    /// them. Derived from `settled()` — the same capture every other
    /// terminal consumer reads.
    pub(super) fn build_progress_frozen(&self, build_id: Uuid) -> bool {
        self.builds
            .get(&build_id)
            .is_some_and(|b| b.settled().is_some())
    }

    /// [`BuildEventBus::emit_progress_with`] behind the terminal-build
    /// freeze: the caller paid for the `build_summary` scan, but a
    /// resident terminal build's progress must not be re-emitted from
    /// the still-mutating DAG — live watchers (and any later WatchBuild
    /// snapshot) would see totals shrunk by mutations the finished build
    /// no longer describes.
    // r[impl sched.build.terminal-status-settled+3]
    pub(super) fn emit_progress_with(
        &mut self,
        build_id: Uuid,
        summary: &crate::dag::BuildSummary,
    ) {
        if self.build_progress_frozen(build_id) {
            return;
        }
        let built = self.builds.get(&build_id).map_or(0, |b| b.built_count);
        self.events.emit_progress_with(build_id, summary, built);
    }

    pub(super) fn get_interested_builds(&self, drv_hash: &DrvHash) -> Vec<Uuid> {
        // Sorted: `interested_builds` is a HashSet (RandomState), so raw
        // iteration order is process-dependent. The flusher's S3 key is
        // now `(drv_hash, exec_id)` and no longer derives from a build_id
        // — but `record_exec_correlation` and the per-build `BuildEvent`
        // emitters still iterate this set, and a deterministic order
        // keeps test assertions and PG-side trace ordering stable.
        let mut v: Vec<Uuid> = self
            .dag
            .node(drv_hash)
            .map(|s| s.interested_builds.iter().copied().collect())
            .unwrap_or_default();
        v.sort_unstable();
        v
    }

    /// Resolve the exec_id to correlate/stamp at a terminal transition.
    ///
    /// Reads `state.exec_id` (the actor's carrier, set by
    /// `assign_to_worker`). See [`crate::state::DerivationState::exec_id`]
    /// and `reset_to_ready` for the carrier-divergence rationale.
    ///
    /// Returns `None` when the carrier has no exec_id — the derivation
    /// never reached a worker (cached terminal, never-dispatched poison,
    /// a never-dispatched cascaded `DependencyFailed`), the assignment
    /// was rolled back, the prior execution was already finalized at an
    /// earlier terminal and the node was reset out of it (I-094 reprobe /
    /// I-047 stale-output reset — `transition()` drops the carrier on
    /// terminal-exit), or `reset_to_ready()` cleared it on a
    /// disconnect→re-dispatch and a poison path reached the terminal
    /// before the next `assign_to_worker` re-stamped it.
    ///
    /// Called from [`Self::terminal_log_epilogue`] (once, at the top —
    /// the resolved value is threaded to the correlate/stamp steps).
    pub(super) fn exec_id_for_terminal(
        &self,
        state: &crate::state::DerivationState,
    ) -> Option<Uuid> {
        state.exec_id
    }

    /// Record which execution each interested build observed for
    /// `drv_hash`, so the dashboard's build view (`GraphNode.exec_id` →
    /// rio-store's `LogService.TailLog`) can fetch the *exact*
    /// execution's log instead
    /// of falling back to "latest exec for this drv" — which can be
    /// wrong after a retry or a later build's rebuild of the same drv.
    /// (`rio-cli derivations <build-id>` does NOT surface `exec_id` —
    /// `DerivationDiagnostic` has no field for it; the CLI half of this
    /// wiring is unfinished. The CLI fallback is `rio-cli logs <drv>`,
    /// which resolves the latest execution.)
    ///
    /// Best-effort, fire-and-forget — a failed write degrades the
    /// dashboard's build view to the latest-exec fallback, not a hard
    /// error. The same shape as the event-log GC and `build_samples`
    /// inserts: spawned (not awaited in the actor loop) so a slow PG
    /// can't stall the next command.
    ///
    /// Called from [`Self::terminal_log_epilogue`] (the correlate/stamp
    /// chokepoint — success, orphan-adopted recovery
    /// completion, permanent failure, and build-level cancellation).
    /// The chokepoint doc is
    /// authoritative for which terminal paths route through it and
    /// which are carved out; a drv that never reaches this helper
    /// (or reaches it without a resolvable exec_id — see the no-op
    /// conditions below) keeps `build_derivations.exec_id` `NULL`,
    /// falling back to latest-exec resolution.
    ///
    /// No-op (silent) when `state.db_id` is `None` (nodes whose merge
    /// tx hasn't committed — impossible here; merge commits before any
    /// dispatch — but cheap to guard). The never-dispatched skip (both
    /// exec_id carriers `None`) now belongs to the caller:
    /// [`Self::terminal_log_epilogue`] resolves
    /// [`Self::exec_id_for_terminal`] before calling and passes the
    /// resolved value in.
    ///
    /// The UPDATE carries `AND exec_id IS NULL`: a (build, drv)
    /// observation is written exactly once. A build that already
    /// recorded an observation — at its own completion — keeps it; a
    /// post-completion re-execution of the same derivation (I-047/
    /// I-094 reset inside the TERMINAL_CLEANUP_DELAY window, while the
    /// finished build is still in `interested_builds`) does not revise
    /// it.
    ///
    /// r[impl sched.merge.exec-correlation+8]
    pub(super) fn record_exec_correlation(
        &self,
        drv_hash: &DrvHash,
        exec_id: Uuid,
        interested_builds: &[Uuid],
    ) {
        let Some(state) = self.dag.node(drv_hash) else {
            return;
        };
        let Some(derivation_id) = state.db_id else {
            return;
        };
        if interested_builds.is_empty() {
            return;
        }
        let builds: Vec<Uuid> = interested_builds.to_vec();
        let pool = self.db.pool().clone();
        // `AND exec_id IS NULL` — a (build, drv) observation is written
        // exactly once, by the first terminal that build is interested
        // in for that drv. A build's membership in `interested_builds`
        // outlives its completion by TERMINAL_CLEANUP_DELAY (~60s) —
        // `complete_build` only *schedules* the interest removal — so a
        // drv reset out of a terminal state (I-047 GC'd-output reset,
        // I-094 reprobe) and re-completed inside that window would
        // otherwise stomp the already-recorded `bd.exec_id` with an
        // execution the finished build never observed, and the
        // dashboard would present the wrong log as exact (the
        // "approximate" banner gates on `execId === ''`, not on
        // correctness). The guard is SQL-side rather than an
        // actor-side is_terminal() filter because the build that this
        // very completion finishes is ALREADY terminal in
        // `self.builds` by the time the epilogue runs
        // (`release_downstream` → `complete_build` precedes
        // `terminal_log_epilogue` in `handle_success_completion`) —
        // an actor-side filter cannot distinguish "finished just now
        // because of this drv" from "finished 60s ago". Transient
        // retries never reach this function (only terminal transitions
        // do), so first-write-wins == the-only-write-wins everywhere
        // except a post-terminal reset, which is exactly the case
        // being guarded.
        rio_common::task::spawn_monitored("exec-correlation", async move {
            if let Err(e) = sqlx::query(
                "UPDATE build_derivations SET exec_id = $1 \
                 WHERE derivation_id = $2 AND build_id = ANY($3) \
                 AND exec_id IS NULL",
            )
            .bind(exec_id)
            .bind(derivation_id)
            .bind(&builds)
            .execute(&pool)
            .await
            {
                warn!(
                    exec_id = %exec_id,
                    derivation_id = %derivation_id,
                    error = %e,
                    "exec-correlation update failed (best-effort; dashboard \
                     falls back to latest-exec)"
                );
            }
        });
    }

    /// Run the terminal bookkeeping for a derivation execution that just
    /// reached a terminal state: record which execution each interested
    /// build observed (`build_derivations.exec_id`) and stamp the
    /// `drv_executions` lifecycle row terminal.
    ///
    /// This is the single chokepoint for
    /// [`Self::record_exec_correlation`] →
    /// [`Self::stamp_drv_execution_terminal`]. A terminal path that ran
    /// an execution MUST call it; any terminal that forgets leaves the
    /// `drv_executions` row at `status=NULL` (the store's completeness
    /// predicate reads the execution as still-running until the TTL
    /// sweep) and `build_derivations.exec_id` NULL — the dashboard then
    /// shows the "approximate" banner for a log that was actually
    /// streamed.
    ///
    /// Callers and their `status` argument:
    /// - `handle_success_completion` — `"succeeded"`
    /// - `adopt_orphan_completion` (recovery) — `"succeeded"` (outputs
    ///   found in the store for a drv whose worker never reconnected:
    ///   the execution ran to completion while the scheduler was down)
    /// - `terminal_failure_epilogue` — `"failed"` (covers `Poisoned` via
    ///   `poison_and_cascade`/`handle_permanent_failure` and
    ///   timeout-exhausted `Cancelled` via `handle_timeout_failure`; the
    ///   walk-era `DependencyFailed` revert caller died with Phase
    ///   D-prime — the epilogue self-gates on
    ///   [`Self::exec_id_for_terminal`] and skips never-dispatched drvs
    ///   entirely)
    /// - `cancel_build_derivations` (`to_cancel` arm) — `"cancelled"`
    ///   (build-level cancellation/failure of an `Assigned`/`Running`
    ///   drv: `handle_cancel_build`, per-build wall-clock timeout,
    ///   fail-fast, top-down substitute fail)
    ///
    /// NOT called for bystander drvs swept to a terminal from a
    /// non-executing state (`Created|Queued|Ready`) by
    /// someone else's event — a build cancellation sweeping
    /// not-yet-dispatched members, a dependency-failure cascade sweeping
    /// ancestors. The only exec_id those nodes can carry is a stale
    /// restamp from a reset-out execution; firing the epilogue on it
    /// would record `bd.exec_id` for an execution the build never
    /// observed. (The reset execution's `drv_executions` row stays
    /// `status=NULL` until the store's TTL sweep — truthful: that
    /// execution was abandoned, not terminated.)
    ///
    /// Returns the join handle of the spawned `drv_executions` stamp
    /// write, `None` when nothing was stamped (hash not in DAG, no
    /// exec_id, status outside the vocabulary). Production callers sit
    /// in the actor loop and ignore it — awaiting it there would block
    /// the actor on PG, exactly what the fire-and-forget design avoids.
    /// Tests await it to order assertions after the stamp has
    /// committed: the monotone-guard test needs a happens-before edge
    /// on a write whose correct outcome is "zero rows changed", which
    /// no row poll can observe.
    ///
    /// r[impl sched.merge.exec-correlation+8]
    pub(super) fn terminal_log_epilogue(
        &self,
        drv_hash: &DrvHash,
        status: &'static str,
        interested_builds: &[Uuid],
        // `CompletionReport.final_line_count` for the terminal paths
        // that have a report (success, report-bearing failures, and
        // the cancelled LATE-report arm — merged_bug_294; the
        // cancel-transition itself and recovery pass `None`). `None` ⇒
        // `drv_executions.final_line_count` stays NULL ⇒ the store's
        // completeness predicate reads the execution as incomplete —
        // the conservative direction (never falsely claim a log is
        // complete). The stamp's COALESCE accepts a later equal-status
        // write, which is exactly how the cancelled report fills the
        // cancel-time NULL.
        final_line_count: Option<i64>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let Some(state) = self.dag.node(drv_hash) else {
            // Should be impossible at this call site (terminal handlers
            // already validated the hash exists in the DAG), but defensive.
            warn!(drv_hash = %drv_hash, "terminal_log_epilogue: hash not in DAG, skipping");
            return None;
        };
        // Resolve the execution to finalize ONCE and thread it through
        // both steps so the `bd.exec_id` write and the lifecycle stamp
        // cannot name different executions.
        let Some(exec_id) = self.exec_id_for_terminal(state) else {
            // Never dispatched: nothing to correlate or stamp.
            // debug!, not warn!: every cause is a documented expected
            // no-op (cached terminal, never-dispatched poison,
            // first-attempt substitute revert).
            tracing::debug!(
                drv_hash = %drv_hash,
                status,
                "terminal_log_epilogue: no exec_id (never dispatched), skipping"
            );
            return None;
        };
        self.record_exec_correlation(drv_hash, exec_id, interested_builds);
        self.stamp_drv_execution_terminal(drv_hash, exec_id, status, final_line_count)
    }

    /// Stamp the `drv_executions` lifecycle row terminal. Fire-and-forget
    /// from the sync terminal chokepoint (the [`Self::record_exec_correlation`]
    /// pattern): the actor must not block on PG in the completion path, and
    /// a failed stamp degrades exactly one thing — the store's completeness
    /// predicate keeps reading this execution as still-running until the
    /// 30-day TTL — which is the same observable state as "the stamp hasn't
    /// landed yet".
    ///
    /// The incoming `status` is `terminal_log_epilogue`'s `&'static str`
    /// vocabulary (`"succeeded"` / `"failed"` / `"cancelled"`). It is
    /// re-mapped onto the [`rio_migrations::schema`] `EXEC_STATUS_*`
    /// constants rather than written through verbatim so that the value the
    /// store's `EXEC_STATUS_TERMINAL.contains(..)` test sees can never
    /// drift from the value this function writes — if the epilogue ever
    /// grows a fourth status string, the match below fails loudly instead
    /// of silently writing a vocabulary the predicate doesn't recognize.
    ///
    /// The qual commutes with the assignment-close stamp
    /// (`close_assignments_sql` — sched.db.exec-stamp-on-close):
    /// `status IS NULL` admits the first verdict; `status = $2` admits
    /// a SAME-verdict epilogue arriving after the closer already
    /// stamped, so the closer racing ahead cannot cost the row its
    /// `final_line_count` (filled via COALESCE — first verdict wins on
    /// every column, late equal-status writes only fill gaps). A
    /// DIFFERENT verdict still matches zero rows.
    ///
    /// Returns the spawned write's join handle (`None` on the
    /// vocabulary-error arm). [`Self::terminal_log_epilogue`] forwards
    /// it; production never awaits it (see the caveat there).
    pub(super) fn stamp_drv_execution_terminal(
        &self,
        drv_hash: &DrvHash,
        exec_id: Uuid,
        status: &'static str,
        final_line_count: Option<i64>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        use rio_migrations::schema::{
            EXEC_STATUS_CANCELLED, EXEC_STATUS_FAILED, EXEC_STATUS_SUCCEEDED,
        };
        let exec_status = match status {
            "succeeded" => EXEC_STATUS_SUCCEEDED,
            "failed" => EXEC_STATUS_FAILED,
            "cancelled" => EXEC_STATUS_CANCELLED,
            other => {
                tracing::error!(
                    drv_hash = %drv_hash,
                    exec_id = %exec_id,
                    status = other,
                    "terminal_log_epilogue called with a status outside the \
                     drv_executions vocabulary; lifecycle row left unstamped"
                );
                return None;
            }
        };
        let pool = self.db.pool().clone();
        let handle = rio_common::task::spawn_monitored("drv-execution-terminal", async move {
            // r[impl sched.db.exec-stamp-on-close]
            if let Err(e) = sqlx::query(
                "UPDATE drv_executions \
                 SET status = $2, \
                     finished_at = COALESCE(finished_at, now()), \
                     final_line_count = COALESCE(final_line_count, $3) \
                 WHERE exec_id = $1 AND (status IS NULL OR status = $2)",
            )
            .bind(exec_id)
            .bind(exec_status)
            .bind(final_line_count)
            .execute(&pool)
            .await
            {
                warn!(
                    exec_id = %exec_id,
                    status = exec_status,
                    error = %e,
                    "drv_executions terminal stamp failed (best-effort; the \
                     execution reads as still-running until the TTL sweep)"
                );
            }
        });
        Some(handle)
    }
}
