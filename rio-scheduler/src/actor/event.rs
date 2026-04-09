//! Build-event emission: per-build broadcast channel + PG persist
//! sidechannel + log/phase forwarding from worker streams. Everything
//! here writes to `build_events` / `build_sequences` /
//! `build_progress_at` / `event_persist_tx` / `log_flush_tx`.

use super::*;

/// Minimum interval between `BuildProgress` emits for one build (I-140).
/// `emit_progress` → `build_summary` is O(dag_nodes); on a 153k-node DAG
/// that's ~15-60ms per call. Calling per-assign + per-complete +
/// per-disconnect under ephemeral-builder churn compounds to >100% actor
/// utilization. 250ms ≈ 4/s caps the scan rate well below the ~1s
/// dashboard poll cadence. Callers that already hold a fresh summary use
/// `emit_progress_with` directly (bypasses debounce — scan cost paid).
const PROGRESS_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(250);

impl DagActor {
    pub(super) fn emit_build_event(
        &mut self,
        build_id: Uuid,
        event: rio_proto::types::build_event::Event,
    ) {
        use rio_proto::types::build_event::Event;

        let seq = self.build_sequences.entry(build_id).or_insert(0);
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
        if let Some(tx) = &self.event_persist_tx
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

        if let Some(tx) = self.build_events.get(&build_id) {
            // broadcast::send returns Err only if there are no receivers, which is fine
            let _ = tx.send(build_event);
        }
    }

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
        if self
            .build_progress_at
            .get(&build_id)
            .is_some_and(|t| t.elapsed() < PROGRESS_DEBOUNCE)
        {
            return;
        }
        let summary = self.dag.build_summary(build_id);
        self.emit_progress_with(build_id, &summary);
    }

    /// [`emit_progress`] with a precomputed summary. Callers that
    /// already hold a `build_summary` (e.g. `update_build_counts`
    /// callers) pass it so the O(dag_nodes) scan runs once, not twice.
    /// Bypasses the debounce — the caller paid for the scan, so emit.
    pub(super) fn emit_progress_with(
        &mut self,
        build_id: Uuid,
        summary: &crate::dag::BuildSummary,
    ) {
        self.build_progress_at.insert(build_id, Instant::now());
        self.emit_build_event(
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
        let Some(hash) = self.drv_path_to_hash(drv_path) else {
            return;
        };
        let lines = batch.lines.len() as u64;
        for build_id in self.get_interested_builds(&hash) {
            // batch.clone(): BuildLogBatch has Vec<Vec<u8>> so this is
            // a deep copy. For 64 lines × 100 bytes that's ~6.5KB × N
            // interested builds. Typically N=1 (one gateway per build).
            // If profiling ever shows this hot, Arc<BuildLogBatch> in
            // BuildEvent.
            self.emit_build_event(
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
        let Some(hash) = self.drv_path_to_hash(&phase.derivation_path) else {
            return;
        };
        for build_id in self.get_interested_builds(&hash) {
            self.emit_build_event(
                build_id,
                rio_proto::types::build_event::Event::Phase(phase.clone()),
            );
        }
    }

    /// Fire a log-flush request for the given derivation. No-op if the
    /// flusher isn't configured (tests, or `RIO_LOG_S3_BUCKET` unset).
    ///
    /// `try_send`: if the flusher channel is full (shouldn't happen — 1000
    /// cap and the flusher's S3 PUT latency is sub-second), drop silently.
    /// The 30s periodic tick will still snapshot until CleanupTerminalBuild.
    ///
    /// Called from `handle_completion_success` AND `handle_permanent_failure`
    /// — both paths flush because failed builds still have useful logs.
    /// NOT called from `handle_transient_failure`: the derivation gets
    /// re-queued, a new worker builds it from scratch, and that worker's
    /// logs replace the partial ones. The ring buffer gets `discard()`ed
    /// by the BuildExecution recv task on worker disconnect.
    pub(super) fn trigger_log_flush(&self, drv_hash: &DrvHash, interested_builds: Vec<Uuid>) {
        let Some(tx) = &self.log_flush_tx else {
            return;
        };
        let Some(drv_path) = self.drv_hash_to_path(drv_hash) else {
            // Should be impossible at this call site (completion handlers
            // already validated the hash exists in the DAG), but defensive.
            warn!(drv_hash = %drv_hash, "trigger_log_flush: hash not in DAG, skipping");
            return;
        };
        let req = crate::logs::FlushRequest {
            drv_path,
            drv_hash: drv_hash.clone(),
            interested_builds,
        };
        if tx.try_send(req).is_err() {
            warn!(
                drv_hash = %drv_hash,
                "log flush channel full, dropped; periodic tick will snapshot"
            );
            metrics::counter!("rio_scheduler_log_flush_dropped_total").increment(1);
        }
    }
}
