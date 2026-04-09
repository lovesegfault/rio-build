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

use crate::state::DrvHash;

use super::{BUILD_EVENT_BUFFER_SIZE, DagActor};

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
    /// Build event broadcast channels.
    pub(super) channels: HashMap<Uuid, broadcast::Sender<rio_proto::types::BuildEvent>>,
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
            sequences: HashMap::new(),
            progress_at: HashMap::new(),
            persist_tx,
            flush_tx,
        }
    }

    /// Create a fresh broadcast channel for `build_id` and seed
    /// `sequences[build_id] = 0`. Returns the receiver (merge step 3
    /// hands it to the SubmitBuild bridge; recovery drops it).
    pub(super) fn register(
        &mut self,
        build_id: Uuid,
    ) -> broadcast::Receiver<rio_proto::types::BuildEvent> {
        let (tx, rx) = broadcast::channel(BUILD_EVENT_BUFFER_SIZE);
        self.channels.insert(build_id, tx);
        self.sequences.insert(build_id, 0);
        rx
    }

    /// Drop all per-build state for `build_id` (channels + seq +
    /// debounce). Called from terminal-cleanup and merge-rollback.
    pub(super) fn remove(&mut self, build_id: Uuid) {
        self.channels.remove(&build_id);
        self.sequences.remove(&build_id);
        self.progress_at.remove(&build_id);
    }

    /// Reset to empty. Called from `clear_persisted_state` on leader
    /// transitions. The `persist_tx`/`flush_tx` wires survive — they're
    /// task channels, not per-build state.
    pub(super) fn clear(&mut self) {
        self.channels.clear();
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

        let seq = self.sequences.entry(build_id).or_insert(0);
        *seq += 1;
        let seq = *seq;

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
            && !matches!(build_event.event, Some(Event::Log(_)))
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

        if let Some(tx) = self.channels.get(&build_id) {
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
    /// cap and the flusher's S3 PUT latency is sub-second), drop silently.
    /// The 30s periodic tick will still snapshot until CleanupTerminalBuild.
    pub(super) fn try_log_flush(&self, req: crate::logs::FlushRequest) {
        let Some(tx) = &self.flush_tx else {
            return;
        };
        if tx.try_send(req).is_err() {
            warn!("log flush channel full, dropped; periodic tick will snapshot");
            metrics::counter!("rio_scheduler_log_flush_dropped_total").increment(1);
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
        self.dag
            .node(drv_hash)
            .map(|s| s.interested_builds.iter().copied().collect())
            .unwrap_or_default()
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
        // vm-phase2b asserts this > 0. The gateway → client leg
        // (STDERR_NEXT rendering) depends on the Nix client's
        // verbosity and activity-context handling — not something we
        // control, so not asserted on in the VM test. The ring buffer
        // + AdminService give the authoritative log-serving path;
        // STDERR_NEXT is a convenience tail that may or may not render.
        metrics::counter!("rio_scheduler_log_lines_forwarded_total").increment(lines);
    }

    /// Relay a build-phase change to interested gateways. Mirror of
    /// [`handle_forward_log_batch`] without the ring-buffer side: phase
    /// is a state edge, not log content. Unknown drv_path → drop
    /// silently (same rationale: late-arrival race or buggy worker).
    pub(super) fn handle_forward_phase(&mut self, phase: rio_proto::types::BuildPhase) {
        let Some(hash) = self.dag.hash_for_path(&phase.derivation_path).cloned() else {
            return;
        };
        for build_id in self.get_interested_builds(&hash) {
            self.events.emit(
                build_id,
                rio_proto::types::build_event::Event::Phase(phase.clone()),
            );
        }
    }

    /// Fire a log-flush request for the given derivation. No-op if the
    /// flusher isn't configured (tests, or `RIO_LOG_S3_BUCKET` unset).
    ///
    /// Called from `handle_completion_success` AND `handle_permanent_failure`
    /// — both paths flush because failed builds still have useful logs.
    /// NOT called from `handle_transient_failure`: the derivation gets
    /// re-queued, a new worker builds it from scratch, and that worker's
    /// logs replace the partial ones. The ring buffer gets `discard()`ed
    /// by the BuildExecution recv task on worker disconnect.
    pub(super) fn trigger_log_flush(&self, drv_hash: &DrvHash, interested_builds: Vec<Uuid>) {
        let Some(drv_path) = self.dag.path_for_hash(drv_hash).map(String::from) else {
            // Should be impossible at this call site (completion handlers
            // already validated the hash exists in the DAG), but defensive.
            warn!(drv_hash = %drv_hash, "trigger_log_flush: hash not in DAG, skipping");
            return;
        };
        self.events.try_log_flush(crate::logs::FlushRequest {
            drv_path,
            drv_hash: drv_hash.clone(),
            interested_builds,
        });
    }

    /// Seal the log ring buffer for `drv_hash` so late `LogBatch`
    /// pushes (still in flight on the BuildExecution stream after the
    /// worker sent CompletionReport) cannot recreate an entry the
    /// flusher already drained. Called from terminal completion
    /// handlers BEFORE [`Self::trigger_log_flush`] — the flusher's
    /// drain still returns the pre-seal contents; sealing only blocks
    /// post-drain recreation. No-op if `log_buffers` unwired (tests).
    pub(super) fn seal_log_buffer(&self, drv_hash: &DrvHash) {
        let Some(bufs) = &self.log_buffers else {
            return;
        };
        let Some(drv_path) = self.dag.path_for_hash(drv_hash) else {
            return;
        };
        bufs.seal(drv_path);
    }
}
