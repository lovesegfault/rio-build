//! Worker lifecycle: connect/disconnect, heartbeat, periodic tick.

use super::*;

impl DagActor {
    // -----------------------------------------------------------------------
    // Worker management
    // -----------------------------------------------------------------------

    pub(super) fn handle_worker_connected(
        &mut self,
        worker_id: &WorkerId,
        stream_tx: mpsc::Sender<rio_proto::types::SchedulerMessage>,
    ) {
        info!(worker_id = %worker_id, "worker stream connected");

        let worker = self
            .workers
            .entry(worker_id.clone())
            .or_insert_with(|| WorkerState::new(worker_id.clone()));

        let was_registered = worker.is_registered();
        worker.stream_tx = Some(stream_tx);

        if !was_registered && worker.is_registered() {
            info!(worker_id = %worker_id, "worker fully registered (stream + heartbeat)");
        }
    }

    pub(super) async fn handle_worker_disconnected(&mut self, worker_id: &str) {
        info!(worker_id, "worker disconnected");

        let Some(worker) = self.workers.remove(worker_id) else {
            return; // unknown worker, no-op (and no gauge decrement)
        };

        // Only decrement if worker was fully registered (stream + heartbeat).
        // Otherwise the gauge goes negative for workers that connected a stream
        // but never sent a heartbeat (increment fires on full registration only).
        let was_registered = worker.is_registered();

        // Reassign all derivations that were assigned to this worker.
        // reset_to_ready() handles Assigned -> Ready and Running -> Failed -> Ready,
        // maintaining state-machine invariants.
        for drv_hash in &worker.running_builds {
            if let Some(state) = self.dag.node_mut(drv_hash.as_str()) {
                if let Err(e) = state.reset_to_ready() {
                    warn!(
                        drv_hash = %drv_hash, error = %e,
                        "invalid state for worker-lost recovery, skipping"
                    );
                    continue;
                }
                // Worker disconnect during build is a failed attempt: count it.
                state.retry_count += 1;
                if let Err(e) = self.db.increment_retry_count(drv_hash).await {
                    error!(drv_hash = %drv_hash, error = %e, "failed to persist retry increment");
                }
                if let Err(e) = self
                    .db
                    .update_derivation_status(drv_hash, DerivationStatus::Ready, None)
                    .await
                {
                    error!(drv_hash = %drv_hash, error = %e, "failed to persist Ready status");
                }
                self.push_ready(drv_hash.clone());
            }
        }

        if was_registered {
            metrics::gauge!("rio_scheduler_workers_active").decrement(1.0);
        }
    }

    pub(super) fn handle_heartbeat(
        &mut self,
        worker_id: &WorkerId,
        system: String,
        supported_features: Vec<String>,
        max_builds: u32,
        running_builds: Vec<String>, // drv_paths from worker proto
        // Grouped to stay under clippy's 7-arg limit. Both are
        // optional heartbeat-carried hints with identical overwrite-
        // unconditionally semantics (None clears stale). They're
        // unpacked immediately below; the tuple is just transport.
        (bloom, size_class): (Option<rio_common::bloom::BloomFilter>, Option<String>),
    ) {
        // TOCTOU fix: a stale heartbeat must not clobber fresh assignments.
        // The scheduler is authoritative for what it assigned. We reconcile:
        //   - Keep scheduler-known builds that are still Assigned/Running
        //     in the DAG (heartbeat may predate the assignment).
        //   - Accept heartbeat-reported builds we don't know about, but warn
        //     (shouldn't happen; indicates split-brain or restart).
        //   - Remove builds absent from heartbeat only if DAG state is no
        //     longer Assigned/Running (completion already processed).
        // Worker reports drv_paths; resolve to drv_hashes via the DAG index.
        // Unknown paths (not in DAG) are silently dropped — they indicate
        // split-brain or a stale heartbeat after scheduler restart.
        let heartbeat_set: HashSet<DrvHash> = running_builds
            .into_iter()
            .filter_map(|path| self.dag.hash_for_path(&path).cloned())
            .collect();

        // Compute the reconciled running set before borrowing `worker` mutably,
        // so we can read self.dag for derivation state checks.
        let prev_running: HashSet<DrvHash> = self
            .workers
            .get(worker_id.as_str())
            .map(|w| w.running_builds.clone())
            .unwrap_or_default();

        let mut reconciled: HashSet<DrvHash> = HashSet::new();
        // Keep scheduler-assigned builds that are still in-flight.
        for drv_hash in &prev_running {
            let still_inflight = self.dag.node(drv_hash).is_some_and(|s| {
                matches!(
                    s.status(),
                    DerivationStatus::Assigned | DerivationStatus::Running
                )
            });
            if still_inflight {
                reconciled.insert(drv_hash.clone());
            }
        }
        // Add heartbeat-reported builds we don't know about (with warning).
        for drv_hash in &heartbeat_set {
            if !reconciled.contains(drv_hash) && !prev_running.contains(drv_hash) {
                warn!(
                    worker_id = %worker_id,
                    drv_hash = %drv_hash,
                    "heartbeat reports running build scheduler did not assign"
                );
                reconciled.insert(drv_hash.clone());
            }
        }

        let worker = self
            .workers
            .entry(worker_id.clone())
            .or_insert_with(|| WorkerState::new(worker_id.clone()));

        let was_registered = worker.is_registered();

        worker.system = Some(system);
        worker.supported_features = supported_features;
        worker.max_builds = max_builds;
        worker.last_heartbeat = Instant::now();
        worker.missed_heartbeats = 0;
        worker.running_builds = reconciled;
        // Bloom: overwrite unconditionally. A heartbeat without a
        // filter (None) clears the old one — the worker stopped
        // sending it, maybe FUSE unmounted. Better to score it as
        // "unknown locality" than use a stale filter that claims
        // paths are cached when they might have been evicted.
        worker.bloom = bloom;
        // size_class: also overwrite unconditionally. None means the
        // worker didn't declare one (empty string in proto) — it
        // becomes a wildcard worker that accepts any class. Same
        // don't-trust-stale reasoning as bloom.
        worker.size_class = size_class;

        if !was_registered && worker.is_registered() {
            info!(worker_id = %worker_id, "worker fully registered (heartbeat + stream)");
            metrics::gauge!("rio_scheduler_workers_active").increment(1.0);
        }
    }

    // -----------------------------------------------------------------------
    // Tick (periodic housekeeping)
    // -----------------------------------------------------------------------

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
            }
            Err(e) => {
                warn!(error = %e, "estimator refresh failed; keeping previous snapshot");
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
        self.maybe_refresh_estimator().await;

        // Check for heartbeat timeouts
        let now = Instant::now();
        let timeout = std::time::Duration::from_secs(HEARTBEAT_TIMEOUT_SECS);
        let mut timed_out_workers = Vec::new();

        for (worker_id, worker) in &mut self.workers {
            if now.duration_since(worker.last_heartbeat) > timeout {
                worker.missed_heartbeats += 1;
                if worker.missed_heartbeats >= MAX_MISSED_HEARTBEATS {
                    timed_out_workers.push(worker_id.clone());
                }
            }
        }

        for worker_id in timed_out_workers {
            warn!(worker_id = %worker_id, "worker timed out (missed heartbeats)");
            self.handle_worker_disconnected(&worker_id).await;
        }

        // Check for poisoned derivations that should expire (24h TTL)
        let mut expired_poisons = Vec::new();
        for (drv_hash, state) in self.dag.iter_nodes() {
            if state.status() == DerivationStatus::Poisoned
                && let Some(poisoned_at) = state.poisoned_at
                && now.duration_since(poisoned_at) > POISON_TTL
            {
                expired_poisons.push(drv_hash.to_string());
            }
        }

        for drv_hash in expired_poisons {
            info!(drv_hash, "poison TTL expired, resetting to created");
            if let Some(state) = self.dag.node_mut(&drv_hash)
                && let Err(e) = state.reset_from_poison()
            {
                warn!(drv_hash, error = %e, "poison reset failed");
                continue;
            }
            if let Err(e) = self
                .db
                .update_derivation_status(&drv_hash, DerivationStatus::Created, None)
                .await
            {
                error!(drv_hash, error = %e, "failed to persist poison reset");
            }
        }

        // Update metrics. All gauges are set from ground-truth state on each
        // Tick — this is self-healing against any counting bugs elsewhere.
        metrics::gauge!("rio_scheduler_derivations_queued").set(self.ready_queue.len() as f64);
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
