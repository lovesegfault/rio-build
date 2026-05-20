//! Build-event emission: per-build broadcast channel + PG persist
//! sidechannel + log/phase forwarding from worker streams.
//!
//! [`BuildEventBus`] owns the per-build channel/sequence/debounce maps
//! and the persister/flusher wires. `DagActor` methods that need DAG
//! lookups (`emit_progress`, `handle_forward_*`, `trigger_log_flush`)
//! stay on `DagActor` and call into the bus.

use std::collections::HashMap;
use std::time::Instant;

use tokio::sync::{broadcast, mpsc};
use tracing::warn;
use uuid::Uuid;

use crate::state::{DerivationStatus, DrvHash, ExecutorId};

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

/// Per-build event broadcast + sequencing state. Sub-struct of
/// [`DagActor`] — single-owner actor, so no locking. Fields are
/// `pub(super)` for the handful of callers that need raw map access
/// (recovery seq seed, watch_build terminal-resend); everything else
/// goes through the methods below.
pub(super) struct BuildEventBus {
    /// State-event broadcast channels (everything but `Event::Log`).
    /// Orphan-watcher checks `receiver_count()` on this sender.
    pub(super) channels: HashMap<Uuid, broadcast::Sender<rio_proto::types::BuildEvent>>,
    /// `Event::Log` broadcast channels — separate so log volume cannot
    /// lag the state ring and drop completions. See
    /// [`LOG_EVENT_BUFFER_SIZE`].
    pub(super) log_channels: HashMap<Uuid, broadcast::Sender<rio_proto::types::BuildEvent>>,
    /// Per-build sequence counters.
    pub(super) sequences: HashMap<Uuid, u64>,
    /// Per-build last-BuildProgress emit time. `emit_progress` debounces
    /// against this — Progress is dashboard-only and `build_summary` is
    /// O(dag_nodes), so emitting on every assign/complete/disconnect at
    /// large-DAG × ephemeral-churn scale head-of-line blocks the actor
    /// (I-140). Cleared on build terminal/cleanup with the other maps.
    progress_at: HashMap<Uuid, Instant>,
    /// Channel to the event-log persister task. [`emit`](Self::emit)
    /// try_sends (build_id, seq, prost-encoded BuildEvent) here AFTER
    /// the broadcast. Event::Log is filtered out — those flood PG
    /// (~20/sec chatty rustc) and S3 already durables them via
    /// `flush_tx`. None in tests without PG.
    persist_tx: Option<mpsc::Sender<crate::event_log::EventLogEntry>>,
    /// Channel to the LogFlusher task. Completion handlers `try_send` a
    /// FlushRequest here so the S3 upload is ordered AFTER the state
    /// transition (hybrid model: buffer outside actor, flush triggered by
    /// actor). `None` in tests/environments without S3.
    ///
    /// `try_send` (not `send`): if the flusher is backed up, drop the
    /// request. The 30s periodic tick will still catch the buffer (it
    /// snapshots, doesn't drain) until CleanupTerminalBuild removes it.
    /// A dropped final-flush is a downgrade to "periodic snapshot only"
    /// for that one derivation, not a hang.
    flush_tx: Option<mpsc::Sender<crate::logs::FlushRequest>>,
}

impl BuildEventBus {
    pub(super) fn new(
        persist_tx: Option<mpsc::Sender<crate::event_log::EventLogEntry>>,
        flush_tx: Option<mpsc::Sender<crate::logs::FlushRequest>>,
    ) -> Self {
        Self {
            channels: HashMap::new(),
            log_channels: HashMap::new(),
            sequences: HashMap::new(),
            progress_at: HashMap::new(),
            persist_tx,
            flush_tx,
        }
    }

    /// Create fresh state + log broadcast channels for `build_id` and
    /// seed `sequences[build_id] = 0`. Returns both receivers (merge
    /// step 3 hands them to the SubmitBuild bridge; recovery drops them).
    pub(super) fn register(&mut self, build_id: Uuid) -> BuildEventReceivers {
        let (tx, state) = broadcast::channel(BUILD_EVENT_BUFFER_SIZE);
        let (log_tx, log) = broadcast::channel(LOG_EVENT_BUFFER_SIZE);
        self.channels.insert(build_id, tx);
        self.log_channels.insert(build_id, log_tx);
        self.sequences.insert(build_id, 0);
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

    /// Drop all per-build state for `build_id` (channels + seq +
    /// debounce). Called from terminal-cleanup and merge-rollback.
    pub(super) fn remove(&mut self, build_id: Uuid) {
        self.channels.remove(&build_id);
        self.log_channels.remove(&build_id);
        self.sequences.remove(&build_id);
        self.progress_at.remove(&build_id);
    }

    /// Reset to empty. Called from `clear_persisted_state` on leader
    /// transitions. The `persist_tx`/`flush_tx` wires survive — they're
    /// task channels, not per-build state.
    pub(super) fn clear(&mut self) {
        self.channels.clear();
        self.log_channels.clear();
        self.sequences.clear();
        self.progress_at.clear();
    }

    /// Last-emitted sequence for `build_id`, or 0 if unknown.
    pub(super) fn last_seq(&self, build_id: Uuid) -> u64 {
        self.sequences.get(&build_id).copied().unwrap_or(0)
    }

    /// Whether a PG event-log persister is wired. Gates the
    /// per-build/periodic event-log GC sweeps (no persister → no rows
    /// to sweep).
    pub(super) fn has_persister(&self) -> bool {
        self.persist_tx.is_some()
    }

    /// `true` if a Progress event for `build_id` was emitted within
    /// [`PROGRESS_DEBOUNCE`]. The `mark_progress` half is folded into
    /// [`emit_progress_with`].
    pub(super) fn progress_debounced(&self, build_id: Uuid) -> bool {
        self.progress_at
            .get(&build_id)
            .is_some_and(|t| t.elapsed() < PROGRESS_DEBOUNCE)
    }

    /// Core emit: bump sequence, persist (if wired + non-Log), broadcast.
    pub(super) fn emit(&mut self, build_id: Uuid, event: rio_proto::types::build_event::Event) {
        use rio_proto::types::build_event::Event;

        // Log + SubstituteProgress aren't persisted (see below) —
        // assigning a fresh seq would diverge the in-memory counter
        // from PG `MAX(sequence)`, breaking the `since_sequence <
        // last_seq` replay guard after failover (gateway saw seq=100
        // via broadcast, recovery seeds last_seq=41 from PG → no
        // replay → events 42..100 lost). Reuse the last persisted seq
        // for these; the gateway tracker (build.rs) overwrites, so
        // monotonicity is preserved.
        let display_only = matches!(event, Event::Log(_) | Event::SubstituteProgress(_));
        let seq = if display_only {
            self.sequences.get(&build_id).copied().unwrap_or(0)
        } else {
            let s = self.sequences.entry(build_id).or_insert(0);
            *s += 1;
            *s
        };

        let build_event = rio_proto::types::BuildEvent {
            build_id: build_id.to_string(),
            sequence: seq,
            timestamp: Some(prost_types::Timestamp::from(std::time::SystemTime::now())),
            event: Some(event),
        };

        // Persist to PG for since_sequence replay. BEFORE the
        // broadcast: the prost encode borrows build_event, and the
        // broadcast consumes it. Ordering doesn't matter for
        // correctness (the persister is a separate FIFO task; a
        // watcher that subscribes between try_send and tx.send below
        // still sees this event via broadcast).
        //
        // Event::Log filtered: ~20/sec under a chatty rustc would
        // flood PG. Log lines are already durable via S3 (the
        // LogFlusher, same pattern). Gateway reconnect cares about
        // state-machine events (Started/Completed/Derivation*), not
        // log lines — those it re-fetches from S3.
        if let Some(tx) = &self.persist_tx
            && !display_only
        {
            use prost::Message;
            let bytes = build_event.encode_to_vec();
            if let Err(mpsc::error::TrySendError::Full(_)) = tx.try_send((build_id, seq, bytes)) {
                // Persister backed up (PG slow/down). The broadcast
                // below still carries the event to live watchers;
                // only a mid-backlog reconnect loses it. 1000 events
                // of backlog = ~200s at steady-state — if we're
                // here, PG is probably unreachable anyway.
                metrics::counter!("rio_scheduler_event_persist_dropped_total").increment(1);
            }
            // Closed variant: persister task died. Don't spam the
            // metric — spawn_monitored already logged the panic.
        }

        // r[impl gw.activity.stop-parity]
        // Display-only events (Log, SubstituteProgress) → log ring;
        // state-transition events → state ring. The bridge merges
        // both. This is the load-bearing split: a chatty build's log
        // volume can lag the *log* ring (acceptable — S3 is
        // authoritative for logs; the terminal Cached/Completed
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
    /// debounce timestamp. Bypasses [`progress_debounced`] — the caller
    /// paid for the O(dag_nodes) scan, so emit unconditionally.
    pub(super) fn emit_progress_with(
        &mut self,
        build_id: Uuid,
        summary: &crate::dag::BuildSummary,
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
            }),
        );
    }

    /// Fire a log-flush request. No-op if the flusher isn't configured
    /// (tests, or `RIO_LOG_S3_BUCKET` unset).
    ///
    /// `try_send`: if the flusher channel is full (shouldn't happen — 1000
    /// cap and the flusher's S3 PUT latency is sub-second), drop. The 30s
    /// periodic tick still snapshots the (sealed) buffer to
    /// `logs/{drv_hash}/{exec_id}.partial.log.zst` and UPSERTs the
    /// `drv_logs` row with `is_complete=false` until
    /// `CleanupTerminalBuild` reaps the DAG node and discards the buffer
    /// (after `TERMINAL_CLEANUP_DELAY`, ~60s); the row stays incomplete
    /// and the dashboard sees only the last periodic snapshot.
    pub(super) fn try_log_flush(&self, req: crate::logs::FlushRequest) {
        let Some(tx) = &self.flush_tx else {
            return;
        };
        if let Err(mpsc::error::TrySendError::Full(_)) = tx.try_send(req) {
            warn!("log flush channel full, dropped; periodic tick will snapshot");
            metrics::counter!("rio_scheduler_log_flush_dropped_total").increment(1);
        }
        // Closed: flusher task died — spawn_monitored already logged the
        // panic. Don't spam the warn/metric (which would say "full" and
        // imply the periodic tick recovers — it can't, it lives in the
        // dead task).
    }
}

impl DagActor {
    /// Emit a BuildProgress snapshot for a build.
    ///
    /// Computes fresh counts + critpath + workers via `build_summary()`
    /// (one O(nodes) pass). Call after state changes that affect the
    /// aggregate view — dispatch (running count + worker set changed)
    /// and completion (completed count changed + critpath dropped via
    /// `update_ancestors`). NOT called from recovery (recovery
    /// rebuilds state but watchers replay from PG event log; emitting
    /// here would inject a spurious event into the sequence).
    ///
    /// Why a separate event (not folding into DerivationEvent): the
    /// dashboard wants a single ETA number it can display without
    /// tracking state. Pushing the aggregate means the client stays
    /// dumb — no stateful reconstruction from the DerivationEvent
    /// stream.
    pub(super) fn emit_progress(&mut self, build_id: Uuid) {
        // I-140: debounce. Progress is dashboard-only; `build_summary`
        // is O(dag_nodes). At 153k nodes that's ~60ms (debug) / ~15ms
        // (release) per call. Calling per-assign + per-complete +
        // per-disconnect under ephemeral-builder churn compounds to
        // >100% actor utilization → mailbox grows unboundedly → admin
        // RPCs timeout → builders idle-timeout with no assignment.
        // 250ms ≈ 4/s max; the dashboard's poll cadence is ~1s anyway.
        // The Tick-driven `tick_publish_gauges` provides the floor for
        // metrics; this is the per-watcher event stream.
        if self.events.progress_debounced(build_id) {
            return;
        }
        let summary = self.dag.build_summary(build_id);
        self.events.emit_progress_with(build_id, &summary);
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

    /// Resolve drv_path → drv_hash → interested_builds, then emit
    /// `BuildEvent::Log` on each build's broadcast channel. The gateway
    /// already handles Event::Log (handler/build.rs:27-32) — it
    /// translates to STDERR_NEXT for the Nix client.
    ///
    /// Unknown drv_path → drop silently. Two legitimate cases:
    /// (a) batch arrived after CleanupTerminalBuild removed the DAG
    ///     entry (race between worker stream and actor loop — the
    ///     build is done, gateway already saw Completed, late log
    ///     lines are irrelevant);
    /// (b) malformed batch from a buggy worker. Neither warrants a
    ///     `warn!()` — (a) is expected, (b) would spam.
    pub(super) fn handle_forward_log_batch(
        &mut self,
        drv_path: &str,
        batch: rio_proto::types::BuildLogBatch,
    ) {
        let Some(hash) = self.dag.hash_for_path(drv_path).cloned() else {
            return;
        };
        let lines = batch.lines.len() as u64;
        for build_id in self.get_interested_builds(&hash) {
            // batch.clone(): BuildLogBatch has Vec<Vec<u8>> so this is
            // a deep copy. For 64 lines × 100 bytes that's ~6.5KB × N
            // interested builds. Typically N=1 (one gateway per build).
            // If profiling ever shows this hot, Arc<BuildLogBatch> in
            // BuildEvent.
            self.events.emit(
                build_id,
                rio_proto::types::build_event::Event::Log(batch.clone()),
            );
        }
        // Metric: proves worker → scheduler → actor pipeline works.
        // The observability VM scenario asserts this > 0. The gateway
        // → client leg (STDERR_NEXT rendering) depends on the Nix
        // client's verbosity and activity-context handling — not
        // something we control, so not asserted on in the VM test.
        // The ring buffer + AdminService give the authoritative
        // log-serving path; STDERR_NEXT is a convenience tail that
        // may or may not render.
        metrics::counter!("rio_scheduler_log_lines_forwarded_total").increment(lines);
    }

    /// Relay a build-phase change to interested gateways. Mirror of
    /// [`handle_forward_log_batch`] without the ring-buffer side: phase
    /// is a state edge, not log content. Unknown drv_path → drop
    /// silently (same rationale: late-arrival race or buggy worker).
    ///
    /// `executor_id` is the calling `BuildExecution` stream's identity.
    /// The gate against `(status, assigned_executor)` is the actor-side
    /// counterpart of `LogBuffers::push_for` (`r[sched.log.batch-binding]`):
    /// both close the same hole — a worker-supplied `derivation_path`
    /// consumed without checking the calling executor actually owns the
    /// assignment. Without it, a compromised builder spoofs
    /// `BuildPhase{derivation_path: <victim>, phase: <text>}` and the
    /// gateway renders attacker-controlled `phase` text as `SetPhase`
    /// into another tenant's `nix build -L` progress display.
    ///
    /// Two-part gate, mirroring `ProcessCompletion`'s stale-report guard
    /// (`r[sched.completion.idempotent]`): an `Assigned|Running` status
    /// precondition, then an exact-match `assigned_executor` comparison.
    /// Both checks are load-bearing — see `sched.log.phase-binding` in
    /// `scheduler.typ` for why each matters and what fails without it.
    /// Unlike that guard, this one also fails closed on
    /// `assigned_executor == None` (defense-in-depth; unreachable when
    /// the precondition passed).
    pub(super) fn handle_forward_phase(
        &mut self,
        phase: rio_proto::types::BuildPhase,
        executor_id: &ExecutorId,
    ) {
        let Some(hash) = self.dag.hash_for_path(&phase.derivation_path).cloned() else {
            return;
        };
        // r[impl sched.log.phase-binding]
        // Status precondition: `transition()` never clears
        // `assigned_executor`, and the worker-completion terminal handlers
        // leave it set, so a bare executor match would accept a late phase
        // from the just-finished executor for ~60s until
        // `CleanupTerminalBuild` reaps the DAG node. Rationale in spec
        // (`sched.log.phase-binding`).
        let Some(node) = self.dag.node(&hash) else {
            // hash_for_path() and dag.node() are kept in lockstep —
            // unreachable, but fail closed (consistent with the
            // hash_for_path None-arm above).
            return;
        };
        let status = node.status();
        if !matches!(
            status,
            DerivationStatus::Assigned | DerivationStatus::Running
        ) {
            tracing::debug!(
                drv = %phase.derivation_path,
                sender = %executor_id,
                status = ?status,
                reason = "not_active",
                "dropping phase update for non-active derivation"
            );
            metrics::counter!(
                "rio_scheduler_phases_rejected_total",
                "reason" => "not_active"
            )
            .increment(1);
            return;
        }
        let assigned = node.assigned_executor.as_ref();
        if assigned != Some(executor_id) {
            // debug!, not warn!: the common cause is a benign late
            // phase from a heartbeat-timed-out executor whose drv was
            // re-dispatched, same as ProcessCompletion's stale-report
            // guard (completion.rs). The metric covers the
            // attack-detection use case without log noise.
            let reason = if assigned.is_none() {
                "no_assignment"
            } else {
                "executor_mismatch"
            };
            tracing::debug!(
                drv = %phase.derivation_path,
                sender = %executor_id,
                assigned = ?assigned,
                reason,
                "dropping phase update from non-assigned executor"
            );
            metrics::counter!(
                "rio_scheduler_phases_rejected_total",
                "reason" => reason
            )
            .increment(1);
            return;
        }
        for build_id in self.get_interested_builds(&hash) {
            self.events.emit(
                build_id,
                rio_proto::types::build_event::Event::Phase(phase.clone()),
            );
        }
    }

    /// Resolve the exec_id to flush/correlate at a terminal transition.
    ///
    /// Reads `state.exec_id` (the actor's carrier, set by
    /// `assign_to_worker`), falling back to the `LogBuffers` ring-buffer
    /// entry's stamped exec_id. The fallback covers poison-while-Ready:
    /// `reset_to_ready()` clears `state.exec_id` WITHOUT discarding the
    /// buffer entry — the entry's lines are still needed by the periodic
    /// flusher in the disconnect→re-dispatch window. If a poison path
    /// (I-065 fleet exhaustion `dispatch.rs`, max_infra/timeout_retries
    /// cap `executor.rs`) reaches `terminal_failure_epilogue` BEFORE the
    /// next `assign_to_worker` re-stamps, `state.exec_id` is `None` while
    /// the buffer holds the disconnected execution's lines + exec_id.
    /// See [`crate::state::DerivationState::exec_id`] and
    /// `reset_to_ready` for the carrier-divergence rationale.
    ///
    /// Returns `None` only when neither carrier has an exec_id — the
    /// derivation never reached a worker (cached terminal,
    /// never-dispatched poison, cascaded `DependencyFailed`, or the
    /// assignment never landed: `rollback_assignment` discarded the
    /// buffer before any push).
    pub(super) fn exec_id_for_terminal(
        &self,
        state: &crate::state::DerivationState,
    ) -> Option<Uuid> {
        state.exec_id.or_else(|| {
            self.log_buffers
                .as_ref()
                .and_then(|b| b.exec_id(state.drv_path().as_str()))
        })
    }

    /// Fire a log-flush request for the given derivation. No-op if the
    /// flusher isn't configured (tests, or `RIO_LOG_S3_BUCKET` unset).
    ///
    /// Called from [`Self::terminal_log_epilogue`] — see its doc for
    /// the call-site enumeration. NOT called from
    /// `handle_transient_failure`: the derivation gets re-queued, a new
    /// worker builds it from scratch, and the next `assign_to_worker`
    /// calls [`Self::discard_log_buffer`] so the old partial buffer is
    /// dropped before the new worker's first push.
    ///
    /// `status` lands in `drv_logs.status` (no production read path
    /// consumes it yet — see `FlushRequest::status`). The request pins
    /// the resolved `exec_id` ([`Self::exec_id_for_terminal`]) so a
    /// re-dispatch racing the flusher's mpsc can't be drained by a stale
    /// request — see `FlushRequest::exec_id`.
    pub(super) fn trigger_log_flush(&self, drv_hash: &DrvHash, status: &'static str) {
        let Some(state) = self.dag.node(drv_hash) else {
            // Should be impossible at this call site (completion handlers
            // already validated the hash exists in the DAG), but defensive.
            warn!(drv_hash = %drv_hash, "trigger_log_flush: hash not in DAG, skipping");
            return;
        };
        let Some(exec_id) = self.exec_id_for_terminal(state) else {
            // No execution ran (cached terminal, or a poison/cascade path
            // that never dispatched). There is nothing to key a `drv_logs`
            // row on; the flusher would skip anyway. Don't queue a request.
            // debug!, not warn!: every cause of a missing exec_id here is a
            // documented expected no-op — same choice as handle_forward_phase's
            // binding gate and push_for's reject arms. Pre-refactor this fell
            // through to flush_final's debug!-level "no buffer" arm.
            tracing::debug!(
                drv_hash = %drv_hash,
                status,
                "trigger_log_flush: no exec_id (never dispatched)"
            );
            return;
        };
        let drv_path = state.drv_path().to_string();
        self.events.try_log_flush(crate::logs::FlushRequest {
            drv_path,
            exec_id,
            status: Some(status.to_string()),
        });
    }

    /// Record which execution each interested build observed for
    /// `drv_hash`, so the dashboard's build view (`GraphNode.exec_id` →
    /// `GetDerivationLogs`) can fetch the *exact* `drv_logs` row instead
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
    /// Called from [`Self::terminal_log_epilogue`] (the seal/flush/
    /// correlate chokepoint — success, permanent failure, and
    /// build-level cancellation) and from recovery's
    /// `adopt_orphan_completion` directly. The chokepoint doc
    /// enumerates the call sites and the never-dispatched carve-outs
    /// (cascaded `DependencyFailed`, `Skipped`, cache-hit
    /// `Completed`) — those drvs have no `exec_id`, so this helper
    /// no-ops anyway and `build_derivations.exec_id` stays `NULL` for
    /// them, falling back to latest-exec resolution.
    ///
    /// No-op (silent) when both `state.exec_id` and the `LogBuffers`
    /// entry's stamp are `None` (never-dispatched drvs — see
    /// [`Self::exec_id_for_terminal`]), or when `state.db_id` is `None`
    /// (nodes whose merge tx hasn't committed — impossible here; merge
    /// commits before any dispatch — but cheap to guard).
    ///
    /// r[impl sched.merge.exec-correlation+4]
    pub(super) fn record_exec_correlation(&self, drv_hash: &DrvHash, interested_builds: &[Uuid]) {
        let Some(state) = self.dag.node(drv_hash) else {
            return;
        };
        let Some(derivation_id) = state.db_id else {
            return;
        };
        // Same fallback as `trigger_log_flush` — poison-while-Ready paths
        // run after `reset_to_ready` cleared `state.exec_id` but before
        // `assign_to_worker` re-stamps it. See `exec_id_for_terminal`.
        let Some(exec_id) = self.exec_id_for_terminal(state) else {
            return;
        };
        if interested_builds.is_empty() {
            return;
        }
        let builds: Vec<Uuid> = interested_builds.to_vec();
        let pool = self.db.pool().clone();
        rio_common::task::spawn_monitored("exec-correlation", async move {
            if let Err(e) = sqlx::query(
                "UPDATE build_derivations SET exec_id = $1 \
                 WHERE derivation_id = $2 AND build_id = ANY($3)",
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

    /// Run the log-finalization sequence for a derivation execution
    /// that just reached a terminal state on a *connected* worker.
    ///
    /// This is the single chokepoint for [`Self::seal_log_buffer`] →
    /// [`Self::trigger_log_flush`] → [`Self::record_exec_correlation`].
    /// A terminal path that ran an execution MUST call it; any terminal
    /// that forgets leaves the `drv_logs` row stuck at
    /// `is_complete=false`/`status=NULL` for the 30-day TTL, the
    /// `.partial` S3 blob never replaced or best-effort-deleted (only
    /// `flush_final` does that), and `build_derivations.exec_id` NULL —
    /// the dashboard then shows the "approximate" banner for a log that
    /// was actually streamed.
    ///
    /// Callers and their `status` argument:
    /// - `handle_success_completion` — `"succeeded"`
    /// - `terminal_failure_epilogue` — `"failed"` (covers `Poisoned` via
    ///   `poison_and_cascade`/`handle_permanent_failure`, timeout-exhausted
    ///   `Cancelled` via `handle_timeout_failure`, and `DependencyFailed`
    ///   via `handle_substitute_complete`'s revert arm — a no-op for
    ///   never-dispatched drvs, but a *reset* drv re-probed into
    ///   `Substituting` retains the prior execution's buffer stamp and
    ///   gets correctly finalized here)
    /// - `cancel_build_derivations` (`to_cancel` arm) — `"cancelled"`
    ///   (build-level cancellation/failure of an `Assigned`/`Running`
    ///   drv: `handle_cancel_build`, per-build wall-clock timeout,
    ///   fail-fast, top-down substitute fail)
    /// - `cancel_build_derivations` (`to_cancel_substituting` and
    ///   `to_depfail` arms) — `"cancelled"`, gated on
    ///   [`Self::exec_id_for_terminal`]: a `Ready`/`Substituting` drv
    ///   that went through `reset_to_ready()` retains a `LogBuffers`
    ///   entry stamped with the prior (reset) execution's `exec_id` —
    ///   that execution's `.partial` row must be finalized here or it
    ///   lingers at `is_complete=false` for the 30-day TTL as the
    ///   latest exec for the drv. The never-dispatched majority of
    ///   those arms (`Queued`/`Created`, first-attempt
    ///   `Ready`/`Substituting`) have no exec_id from either carrier
    ///   and skip the call — nothing to finalize, and routing them
    ///   through `trigger_log_flush` would warn-spam its no-exec_id
    ///   arm on every cancel of a not-yet-started build.
    ///
    /// NOT called from:
    /// - `adopt_orphan_completion` (recovery) — the worker disconnected
    ///   before this leader started, the buffer is empty (or absent),
    ///   the log is already lost. It calls `record_exec_correlation`
    ///   directly (so the dashboard knows which exec produced the
    ///   output) and `discard_log_buffer` (to remove recovery's empty
    ///   placeholder — `flush_final` would no-op on it but the
    ///   `GetDerivationLogs` read path would serve an empty re-poll
    ///   chunk until `CleanupTerminalBuild` reaps it).
    /// - cascaded `DependencyFailed` and `Skipped`: structurally never
    ///   dispatched — a cascaded ancestor's deps were never all
    ///   `Completed`, so it was never `Ready`, so it has no exec_id
    ///   from either carrier and no buffer. Calling would be a
    ///   harmless warn-spamming no-op.
    /// - cache-hit / substitute-success `Completed`
    ///   (`complete_ready_from_store_batch`): usually never dispatched
    ///   (no buffer), but a *reset* drv completed via store hit or
    ///   substitute retains the prior execution's stamped buffer.
    ///   That exec's row stays `is_complete=false` for the GC TTL as
    ///   the drv's latest exec and the unsealed buffer is served as
    ///   "still active" until `CleanupTerminalBuild`. Known gap,
    ///   GC-bounded; closing it needs a status-vocabulary decision
    ///   (`"succeeded"` would attribute the substitute's success to
    ///   the aborted execution), so it is deliberately not wired here.
    ///
    /// Sequencing: seal first so late `LogBatch` pushes between now and
    /// the flusher's drain are dropped instead of recreating an orphan
    /// entry; the buffer present NOW survives for drain. For the
    /// completion-triggered callers (success, permanent failure) the
    /// worker's final output and its `rio: result` footer precede the
    /// `CompletionReport` on the same ordered stream, so they are
    /// already in the buffer when this runs — the seal costs nothing.
    /// The cancel caller inverts that: it seals BEFORE the
    /// `CancelSignal` is even sent, so the worker's in-flight output
    /// and its eventual `rio: result cancelled` footer arrive after the
    /// seal and are dropped by `push_for`. Accepted: the cancel path
    /// must finalize the log without depending on the worker responding
    /// (the signal is a best-effort `try_send`), and the authoritative
    /// outcome is the `drv_logs.status` this same call writes — a
    /// stored cancelled log ends at whatever output had arrived when
    /// the cancel was processed, with no footer. Flush before correlate
    /// — both fire-and-forget but the flush request pins the `exec_id`
    /// it resolves, so it should resolve from the same snapshot of
    /// `state` as the correlate.
    ///
    /// r[impl sched.merge.exec-correlation+4]
    pub(super) fn terminal_log_epilogue(
        &self,
        drv_hash: &DrvHash,
        status: &'static str,
        interested_builds: &[Uuid],
    ) {
        self.seal_log_buffer(drv_hash);
        self.trigger_log_flush(drv_hash, status);
        self.record_exec_correlation(drv_hash, interested_builds);
    }

    /// Seal the log ring buffer for `drv_hash` so late `LogBatch`
    /// pushes (still in flight on the BuildExecution stream after the
    /// worker sent CompletionReport) cannot recreate an entry the
    /// flusher already drained. Called from
    /// [`Self::terminal_log_epilogue`] BEFORE
    /// [`Self::trigger_log_flush`] — the flusher's drain still returns
    /// the pre-seal contents; sealing only blocks post-drain
    /// recreation. No-op if `log_buffers` unwired (tests).
    pub(super) fn seal_log_buffer(&self, drv_hash: &DrvHash) {
        let Some(bufs) = &self.log_buffers else {
            return;
        };
        let Some(drv_path) = self.dag.path_for_hash(drv_hash) else {
            return;
        };
        bufs.seal(drv_path);
    }

    /// Drop any stale log buffer (and seal) for `drv_hash`. Called from
    /// [`super::dispatch`]'s `assign_to_worker` immediately after the
    /// `Ready→Assigned` transition so every fresh attempt starts with a
    /// clean buffer. Covers transient-retry (old worker's partial lines
    /// would otherwise prefix the new worker's), poison-clear-resubmit
    /// (stale seal would silently drop the retry's pushes), and any
    /// dropped-FlushRequest leak that survived to re-dispatch. Also
    /// called from `rollback_assignment` (failed `try_send` rollback —
    /// reaps the empty `set_log_exec`-stamped entry the failed dispatch
    /// just created) and `adopt_orphan_completion` (post-terminal during
    /// recovery — keeps the empty recovery-stamped entry from shadowing
    /// the ex-leader's S3 `.partial` blob in `GetDerivationLogs`).
    /// Idempotent: first-ever dispatch finds no entry. No-op if
    /// `log_buffers` unwired (tests).
    pub(super) fn discard_log_buffer(&self, drv_hash: &DrvHash) {
        let Some(bufs) = &self.log_buffers else {
            return;
        };
        let Some(drv_path) = self.dag.path_for_hash(drv_hash) else {
            return;
        };
        bufs.discard(drv_path);
    }

    /// Stamp a fresh ring-buffer entry with `(exec_id, executor_id)` for
    /// `drv_hash`. Called from [`super::dispatch`]'s `assign_to_worker`
    /// immediately after [`Self::discard_log_buffer`], and from recovery
    /// for each active assignment loaded from PG (the new leader's
    /// `LogBuffers` is empty and `set_exec` is the only carrier the
    /// flusher reads — see `logs/mod.rs::LogBuffers::set_exec`).
    /// No-op if `log_buffers` unwired (tests) or the drv vanished.
    pub(super) fn set_log_exec(&self, drv_hash: &DrvHash, exec_id: Uuid, executor_id: &ExecutorId) {
        let Some(bufs) = &self.log_buffers else {
            return;
        };
        let Some(drv_path) = self.dag.path_for_hash(drv_hash) else {
            return;
        };
        bufs.set_exec(drv_path, exec_id, executor_id.as_str());
    }
}
