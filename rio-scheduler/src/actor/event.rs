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

    /// Fire a log-flush request. Returns whether the request was actually
    /// handed to a live flusher: `false` means no flusher is configured
    /// (tests, or `RIO_LOG_S3_BUCKET` unset), the channel is full, or the
    /// flusher task died — in all of these the request will never be
    /// processed, so the flusher's `drain_if_exec` will never remove the
    /// ring-buffer entry and the caller must not rely on it for cleanup.
    ///
    /// `try_send`: if the flusher channel is full (shouldn't happen — 1000
    /// cap and the flusher's S3 PUT latency is sub-second), drop. For an
    /// entry that holds lines the degraded mode depends on why the enqueue
    /// failed: on Full the (live) 30s periodic tick keeps snapshotting it
    /// to `logs/{drv_hash}/{exec_id}.partial.log.zst` with
    /// `is_complete=false`; on Closed or with no flusher configured there
    /// is no periodic tick either, and the lines sit in the ring buffer
    /// until `CleanupTerminalBuild` discards them at build cleanup. Either
    /// way the row stays incomplete and the dashboard sees at most the
    /// last periodic snapshot. A zero-line entry has nothing for any of
    /// those paths to persist — `terminal_log_epilogue` reaps it
    /// immediately when this returns `false`; reads never depended on
    /// that entry anyway (GetDerivationLogs probes the stored side when
    /// the entry it finds holds zero lines), the reap just avoids
    /// retaining a dead carrier.
    #[must_use]
    pub(super) fn try_log_flush(&self, req: crate::logs::FlushRequest) -> bool {
        let Some(tx) = &self.flush_tx else {
            return false;
        };
        match tx.try_send(req) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                warn!("log flush channel full, dropped; periodic tick will snapshot");
                metrics::counter!("rio_scheduler_log_flush_dropped_total").increment(1);
                false
            }
            // Closed: flusher task died — spawn_monitored already logged the
            // panic. Don't spam the warn/metric (which would say "full" and
            // imply the periodic tick recovers — it can't, it lives in the
            // dead task).
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
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
    /// never-dispatched poison, a never-dispatched cascaded
    /// `DependencyFailed`, or the assignment never landed:
    /// `rollback_assignment` discarded the buffer before any push), or
    /// the prior execution was already finalized at an earlier terminal
    /// and the node was reset out of it (I-094 reprobe / I-047
    /// stale-output reset — `transition()` drops the carrier on
    /// terminal-exit).
    ///
    /// Called from [`Self::terminal_log_epilogue`] (once, at the top —
    /// the resolved value is threaded to the seal/flush/correlate
    /// steps). The flush/correlate helpers no longer resolve it
    /// individually.
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

    /// Whether `state` has a retained, un-finalized execution log — a
    /// `LogBuffers` ring-buffer entry stamped by a prior
    /// `assign_to_worker` that no terminal epilogue has drained yet.
    ///
    /// This is the gate for the not-yet-dispatched cancel arms
    /// (`to_cancel_substituting` / `to_depfail`): they run the
    /// finalization epilogue ONLY when a reset drv's prior execution
    /// would otherwise linger at `is_complete=false` for the 30-day TTL.
    /// Deliberately NOT [`Self::exec_id_for_terminal`]: `state.exec_id`
    /// answers "which execution do I attribute this terminal to", not
    /// "is there an un-finalized log" — it can be `Some` long after the
    /// execution's log was finalized (a recovery restamp from a leaked
    /// `assignments` row; historically also the terminal-exit reset
    /// lanes, now cleared by `transition()`). Firing the epilogue on
    /// that stale carrier records `bd.exec_id` for an execution this
    /// build never observed and enqueues a `FlushRequest` that
    /// `flush_final`'s staleness guard immediately drops. The epilogue
    /// now *also* self-gates on `exec_id_for_terminal` internally, but
    /// that inner gate would accept the stale `state.exec_id` carrier
    /// this outer gate exists to reject — which is why both gates
    /// coexist.
    ///
    /// Known residual (conscious choice): in the window between a
    /// terminal's `trigger_log_flush` and the flusher's `drain()`, the
    /// buffer still exists (sealed) and this returns `true` — a
    /// reprobe-then-cancel landing inside that window still over-fires.
    /// Requires two actor commands to be processed before a background
    /// mpsc consumer runs once. An `&& !is_sealed(..)` term would close
    /// it without breaking the legitimate shapes (neither
    /// `reset_to_ready` nor the transient-retry window seals); not
    /// added because no test can deterministically exercise the window.
    ///
    /// A second residual is cross-replica: after an A→B→A lease flap the
    /// retained entry can be stamped with an exec an interim leader
    /// already finalized; this gate cannot see PG, so the epilogue still
    /// fires — the flusher's already-finalized check in `flush_final`
    /// drops that request and reaps the residue, so the durable blob/row
    /// are never regressed (`obs.log.finalize-immutable`). Until that
    /// reap (or sweep hardening at lease acquisition) the retained entry
    /// still shadows GetDerivationLogs reads and re-uploads its
    /// `.partial` each periodic tick; bounded by the reap and the GC TTL.
    /// r[impl sched.merge.exec-correlation+7]
    pub(super) fn has_buffered_exec_log(&self, state: &crate::state::DerivationState) -> bool {
        self.log_buffers
            .as_ref()
            .and_then(|b| b.exec_id(state.drv_path().as_str()))
            .is_some()
    }

    /// Finalize a *retained, prior* execution's log on a derivation
    /// being swept to a terminal from a non-executing state
    /// (`Created|Queued|Ready|Substituting`) as a bystander of someone
    /// else's event (a build cancellation, a dependency's permanent
    /// failure). No-op unless a `LogBuffers` entry stamped by a reset
    /// execution is still held — see [`Self::has_buffered_exec_log`]
    /// for why the gate is the buffer carrier and not `state.exec_id`
    /// (a bare `state.exec_id` on a swept node is a stale restamp the
    /// inner gate would resolve and mis-attribute). Without this, the
    /// reset execution's `drv_logs` row stays `is_complete=false`/
    /// `status=NULL` for the 30-day TTL as the drv's latest exec, the
    /// `.partial` blob is never replaced, and `bd.exec_id` stays NULL.
    ///
    /// `status` is `"cancelled"` at every call site: the column records
    /// the scheduler's disposition of the *log* (the execution was
    /// abandoned, never re-ran), not the drv's terminal enum —
    /// `"failed"` is reserved for permanent build failure.
    ///
    /// Callers: `cancel_build_derivations`' `to_cancel_substituting`
    /// and `to_depfail` arms, `terminal_failure_epilogue`'s
    /// cascaded-ancestor loop. See [`Self::terminal_log_epilogue`] for
    /// which terminal paths use this gated form vs. the unconditional
    /// call.
    /// r[impl sched.merge.exec-correlation+7]
    pub(super) fn finalize_buffered_exec_log(
        &self,
        drv_hash: &DrvHash,
        interested_builds: &[Uuid],
    ) {
        if self
            .dag
            .node(drv_hash)
            .is_some_and(|s| self.has_buffered_exec_log(s))
        {
            self.terminal_log_epilogue(drv_hash, "cancelled", interested_builds);
        }
    }

    /// Fire a log-flush request for the given derivation. No-op if the
    /// flusher isn't configured (tests, or `RIO_LOG_S3_BUCKET` unset).
    ///
    /// Returns whether the request was enqueued — see
    /// [`BuildEventBus::try_log_flush`].
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
    /// the `exec_id` resolved once by [`Self::terminal_log_epilogue`]
    /// so a re-dispatch racing the flusher's mpsc can't be drained by a
    /// stale request — see `FlushRequest::exec_id`. The request also
    /// carries the current lease generation; `flush_final` refuses to
    /// finalize it under any other tenure (see
    /// `FlushRequest::lease_generation`).
    #[must_use]
    pub(super) fn trigger_log_flush(
        &self,
        drv_hash: &DrvHash,
        exec_id: Uuid,
        status: &'static str,
    ) -> bool {
        let Some(state) = self.dag.node(drv_hash) else {
            // Unreachable: the epilogue (sole caller) already early-returned
            // on this condition and the two calls are synchronous in the same
            // actor handler. Kept as a defensive arm — the lookup is needed
            // for `drv_path` regardless.
            warn!(drv_hash = %drv_hash, "trigger_log_flush: hash not in DAG, skipping");
            return false;
        };
        let drv_path = state.drv_path().to_string();
        self.events.try_log_flush(crate::logs::FlushRequest {
            drv_path,
            exec_id,
            status: Some(status.to_string()),
            lease_generation: self.leader.generation(),
            acquired_transitions: self.leader.acquired_transitions(),
        })
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
    /// correlate chokepoint — success, orphan-adopted recovery
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
    /// r[impl sched.merge.exec-correlation+7]
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

    /// Run the log-finalization sequence for a derivation execution
    /// that just reached a terminal state and whose log buffer (if it
    /// produced one) is still held by *this leader*.
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
    /// - `adopt_orphan_completion` (recovery) — `"succeeded"` (outputs
    ///   found in the store for a drv whose worker never reconnected:
    ///   the execution ran to completion while the scheduler was
    ///   down). On a fresh standby the recovery-stamped entry has zero
    ///   lines: the final flush uploads nothing — `upload_and_record`'s
    ///   `line_count == 0` arm routes it to `finalize_empty_drain`,
    ///   which stamps terminal metadata (`status`/`finished_at`) on any
    ///   `.partial` `drv_logs` row the ex-leader's periodic flusher
    ///   wrote while leaving `is_complete = false` (the stored log is
    ///   truncated at the last snapshot, so the incomplete indicator
    ///   stays surfaced) — and `drain_if_exec` still reaps the entry,
    ///   so `GetDerivationLogs` serves the ex-leader's S3 `.partial`.
    ///   On an ex-leader re-acquiring the
    ///   lease, the retained entry still holds the prior leadership's
    ///   unflushed tail for this same execution (`set_exec` keeps
    ///   lines on a same-exec restamp); the final flush preserves that
    ///   tail and finalizes the `drv_logs` row. The
    ///   [`Self::exec_id_for_terminal`] self-gate covers the
    ///   never-dispatched case. If the FlushRequest cannot be enqueued
    ///   (full channel / dead flusher / no flusher), the zero-line entry
    ///   is discarded immediately at the epilogue instead — see the
    ///   enqueue-failure branch below — so the read path still falls
    ///   through to S3.
    /// - `terminal_failure_epilogue` — `"failed"` (covers `Poisoned` via
    ///   `poison_and_cascade`/`handle_permanent_failure`, timeout-exhausted
    ///   `Cancelled` via `handle_timeout_failure`, and `DependencyFailed`
    ///   via `handle_substitute_complete`'s revert arm — the epilogue
    ///   self-gates on [`Self::exec_id_for_terminal`] and skips
    ///   never-dispatched drvs entirely (no seal tombstone), but a
    ///   *reset* drv re-probed into `Substituting` retains the prior
    ///   execution's buffer stamp and gets correctly finalized here)
    /// - `cancel_build_derivations` (`to_cancel` arm) — `"cancelled"`
    ///   (build-level cancellation/failure of an `Assigned`/`Running`
    ///   drv: `handle_cancel_build`, per-build wall-clock timeout,
    ///   fail-fast, top-down substitute fail)
    /// - [`Self::finalize_buffered_exec_log`] — `"cancelled"`, gated on
    ///   [`Self::has_buffered_exec_log`]. Three call sites:
    ///   `cancel_build_derivations`' `to_cancel_substituting` and
    ///   `to_depfail` arms, and `terminal_failure_epilogue`'s
    ///   cascaded-ancestor loop. A `Queued`/`Ready`/`Substituting` drv
    ///   that went through `reset_to_ready()` retains a `LogBuffers`
    ///   entry stamped with the prior (reset) execution's `exec_id` —
    ///   that execution's `.partial` row must be finalized here or it
    ///   lingers at `is_complete=false` for the 30-day TTL as the
    ///   latest exec for the drv. The outer `has_buffered_exec_log`
    ///   gate is retained even though the epilogue now self-gates on
    ///   [`Self::exec_id_for_terminal`]: the outer gate is *stricter*
    ///   — it rejects a stale `state.exec_id` with no retained buffer
    ///   (a recovery restamp from a leaked `assignments` row), which
    ///   the inner gate would accept and mis-attribute. See
    ///   [`Self::has_buffered_exec_log`] for the full rationale.
    ///
    /// Which form a new terminal path uses:
    /// - The drv is the *subject* of the event being processed (its
    ///   own completion, failure, timeout, substitute result, or
    ///   fleet-exhaustion poison), or an execution is live
    ///   (`Assigned`/`Running`) when the terminal is reached: call
    ///   this function directly, unconditionally. The inner
    ///   `exec_id_for_terminal` gate no-ops for never-dispatched
    ///   triggers (the direct `Ready → Poisoned` I-065 arm, a
    ///   first-attempt substitute revert) — see
    ///   `epilogue_skips_never_dispatched_drv`.
    /// - The drv is a *bystander* swept to a terminal by someone
    ///   else's event from a non-executing state
    ///   (`Created|Queued|Ready|Substituting`) — a build cancellation
    ///   sweeping not-yet-dispatched members, a dependency-failure
    ///   cascade sweeping ancestors: call
    ///   [`Self::finalize_buffered_exec_log`] instead. The only
    ///   legitimate carrier for a swept non-executing node is a
    ///   buffer retained across `reset_to_ready()`; a bare
    ///   `state.exec_id` with no buffer is a stale restamp the inner
    ///   gate would resolve and mis-attribute (`bd.exec_id` written
    ///   for an execution the build never observed).
    ///
    /// NOT called from:
    /// - `seed_initial_states`' merged-in-already-failed nodes
    ///   (`Created → DependencyFailed` at merge time): a node new to
    ///   the DAG in this merge was never dispatched in this DAG —
    ///   structurally cannot carry a buffer.
    /// - recovery's crash-mid-cascade `DependencyFailed` sweep: a
    ///   fresh leader's `LogBuffers` is empty (per-process). An
    ///   ex-leader re-acquiring the lease retains its buffers and
    ///   inherits the same GC-bounded residual as the cache-hit
    ///   entry below — known, not wired (recovery's sweep has no
    ///   per-node interested-set loop in hand).
    /// - cache-hit / substitute-success `Completed`
    ///   (`complete_ready_from_store_batch`) and `Skipped` (CA
    ///   early-cutoff): usually never dispatched (no buffer), but a
    ///   *reset* drv completed via store hit / substitute / cutoff
    ///   retains the prior execution's stamped buffer.
    ///   That exec's row stays `is_complete=false` for the GC TTL as
    ///   the drv's latest exec and the unsealed buffer is served as
    ///   "still active" until `CleanupTerminalBuild`. Known gap,
    ///   GC-bounded; closing it needs a status-vocabulary decision
    ///   (`"succeeded"` would attribute the substitute's success to
    ///   the aborted execution, and these are success-equivalent
    ///   outcomes — the output materialized without this execution
    ///   finishing), so it is deliberately not wired here.
    ///
    /// Sequencing: seal first so late `LogBatch` pushes between now and
    /// the flusher's drain are dropped instead of recreating an orphan
    /// entry; the buffer present NOW survives for drain. For the
    /// completion-triggered callers (success, permanent failure) the
    /// worker's final output and its `rio: result` footer precede the
    /// `CompletionReport` on the same ordered stream, so they are
    /// already in the buffer when this runs — the seal costs nothing.
    /// The in-flight cancel caller (`to_cancel`) inverts that: it seals
    /// BEFORE the `CancelSignal` is even sent, so the worker's
    /// in-flight output and its eventual `rio: result cancelled` footer
    /// arrive after the seal and are dropped by `push_for`. Accepted:
    /// the cancel path must finalize the log without depending on the
    /// worker responding (the signal is a best-effort `try_send`), and
    /// the authoritative outcome is the `drv_logs.status` this same
    /// call writes — that log ends at whatever output had arrived when
    /// the cancel was processed, normally with no footer. The reset-arm
    /// callers (`to_cancel_substituting`/`to_depfail`) instead finalize
    /// a buffer retained across `reset_to_ready()`, still stamped to
    /// the lost worker — its parting footer (possibly `ok`, if the
    /// success report was lost to the disconnect) may already be
    /// buffered, and the seal cannot remove buffered lines. A
    /// `status='cancelled'` log can therefore still end with a
    /// `rio: result` line that disagrees with the row; the row is
    /// authoritative. All three steps share the single resolution
    /// performed at the top of this function, so the flush request and
    /// the `bd.exec_id` write cannot name different executions. The
    /// final-pending mark is set only after a successful enqueue, so the
    /// dropped-enqueue degraded mode (periodic snapshots + cleanup
    /// discard) is unchanged.
    ///
    /// r[impl sched.merge.exec-correlation+7]
    pub(super) fn terminal_log_epilogue(
        &self,
        drv_hash: &DrvHash,
        status: &'static str,
        interested_builds: &[Uuid],
    ) {
        let Some(state) = self.dag.node(drv_hash) else {
            // Should be impossible at this call site (terminal handlers
            // already validated the hash exists in the DAG), but defensive
            // — this is the lookup `trigger_log_flush` used to do.
            warn!(drv_hash = %drv_hash, "terminal_log_epilogue: hash not in DAG, skipping");
            return;
        };
        // Resolve the execution to finalize ONCE and thread it through all
        // three steps. A per-step re-resolution diverges two ways: (a) a
        // never-dispatched drv (no exec_id from either carrier) would seal
        // a tombstone that the skipped flush never unseals — it lingers in
        // `LogBuffers::sealed` until CleanupTerminalBuild reaps the build;
        // (b) when the carrier is the LogBuffers entry (state.exec_id
        // cleared by reset_to_ready), the flush's try_send wakes the
        // flusher, which can drain the entry on a sibling thread before a
        // re-resolution in the correlate step re-reads it — bd.exec_id
        // would stay NULL for an execution that was just finalized.
        let Some(exec_id) = self.exec_id_for_terminal(state) else {
            // Never dispatched: nothing to seal, flush, or correlate.
            // debug!, not warn!: every cause is a documented expected
            // no-op (cached terminal, never-dispatched poison,
            // first-attempt substitute revert).
            tracing::debug!(
                drv_hash = %drv_hash,
                status,
                "terminal_log_epilogue: no exec_id from either carrier (never dispatched), skipping"
            );
            return;
        };
        self.seal_log_buffer(drv_hash);
        let enqueued = self.trigger_log_flush(drv_hash, exec_id, status);
        if enqueued {
            // The flusher will resolve this request (drain, refusal, reap,
            // or deferral + retry); mark the entry now so a
            // CleanupTerminalBuild firing while the request is still queued
            // behind earlier flusher work (a slow PG outage serially burns
            // the pool-acquire timeout per attempt) does not discard the
            // only copy of the log out from under it. Marking only at
            // deferral time left queued-but-unattempted finals unprotected.
            // r[impl obs.log.deferred-final-retry+4]
            self.mark_log_final_pending(drv_hash, exec_id);
        } else if self.discard_log_buffer_if_empty(drv_hash) {
            // Enqueue-failure reap: the flusher will never see this request
            // (channel full, flusher dead, or not configured), so its
            // drain_if_exec will never remove the entry — the failed
            // enqueue means no final is pending and an empty entry has
            // lost every justification.
            // r[impl obs.log.entry-justified]
            // A zero-line entry has nothing the
            // periodic snapshot will ever persist (line_count==0 skip) and
            // nothing to serve; reads never depended on it either
            // (GetDerivationLogs probes the stored side — e.g. the
            // ex-leader's `.partial` from recovery's restamp +
            // adopt_orphan_completion — when the entry it finds holds zero
            // lines). Reap it now so the dead carrier does not sit in
            // memory until CleanupTerminalBuild; entries with lines keep
            // the documented degraded mode (periodic `.partial` snapshots
            // while a flusher exists, then CleanupTerminalBuild). A late
            // LogBatch that would have hit the sealed-drop now hits the
            // no-entry reject instead — dropped either way.
            tracing::debug!(
                drv_hash = %drv_hash,
                exec_id = %exec_id,
                status,
                "flush enqueue failed; discarded empty sealed log buffer \
                 (bookkeeping; reads are unaffected)"
            );
        }
        self.record_exec_correlation(drv_hash, exec_id, interested_builds);
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

    /// Mark `drv_hash`'s ring entry as having a final flush pending with the
    /// flusher (exec-guarded). Called from [`Self::terminal_log_epilogue`]
    /// immediately after a successful [`Self::trigger_log_flush`] enqueue so
    /// `handle_cleanup_terminal_build` leaves the sealed buffer for the
    /// flusher to resolve even when the request is still queued (a slow PG
    /// outage can hold the flusher past TERMINAL_CLEANUP_DELAY before the
    /// first attempt — the request's eventual drain/refusal/reap is the
    /// entry's reaper either way). No-op if `log_buffers` is unwired
    /// (tests), the drv vanished from the DAG, or the entry is missing /
    /// restamped to a different execution (the request will then resolve as
    /// stale — nothing to protect).
    pub(super) fn mark_log_final_pending(&self, drv_hash: &DrvHash, exec_id: Uuid) {
        let Some(bufs) = &self.log_buffers else {
            return;
        };
        let Some(drv_path) = self.dag.path_for_hash(drv_hash) else {
            return;
        };
        bufs.mark_final_pending(drv_path, exec_id);
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
    /// just created).
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

    /// Conditional sibling of [`Self::discard_log_buffer`]: drop the entry
    /// for `drv_hash` only if it holds zero lines (also un-seals). Called
    /// from [`Self::terminal_log_epilogue`] when the completion
    /// `FlushRequest` could not be enqueued — see
    /// `LogBuffers::discard_if_empty` for the rationale. Returns whether an
    /// entry was removed. No-op if `log_buffers` unwired (tests) or the drv
    /// vanished from the DAG.
    pub(super) fn discard_log_buffer_if_empty(&self, drv_hash: &DrvHash) -> bool {
        let Some(bufs) = &self.log_buffers else {
            return false;
        };
        let Some(drv_path) = self.dag.path_for_hash(drv_hash) else {
            return false;
        };
        bufs.discard_if_empty(drv_path)
    }

    /// Stamp the ring-buffer entry with `(exec_id, executor_id)` for
    /// `drv_hash`. Called from [`super::dispatch`]'s `assign_to_worker`
    /// immediately after [`Self::discard_log_buffer`], and from recovery
    /// for each active assignment loaded from PG (`set_exec` is the only
    /// carrier the flusher reads; a fresh standby's `LogBuffers` is empty,
    /// an ex-leader's is retained — `set_exec` itself drops retained lines
    /// when the exec_id changed under an interim leader, see
    /// `logs/mod.rs::LogBuffers::set_exec`).
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
