//! Ready-queue dispatch: assign ready derivations to available workers.
// r[impl sched.overflow.up-only]

use super::*;

/// I-025 freeze detector: state machine that WARNs when derivations are
/// queued but zero streams of the matching kind exist for >60s.
///
/// The scheduler already surfaces this via metrics (`fod_queue_depth` +
/// `fetcher_utilization`), but metrics need a port-forward. A WARN lands
/// in `kubectl logs`. QA I-025: 20-minute freeze with zero ERROR/WARN is
/// operator-hostile — the scheduler knew, it just didn't say.
///
/// Rate-limit: `since` is reset on each WARN so we emit once/minute, not
/// once/dispatch-pass (~once/tick = every 10s would spam).
///
/// Free function (not `&mut self`) so the call site can borrow
/// `&mut self.fod_freeze_since` while also reading `self.executors`.
fn check_freeze(
    since: &mut Option<Instant>,
    frozen: bool,
    kind: &str,
    queue_depth: u64,
    stream_count: usize,
) {
    const WARN_AFTER: std::time::Duration = std::time::Duration::from_secs(60);
    match (frozen, *since) {
        (true, None) => *since = Some(Instant::now()),
        (true, Some(start)) if start.elapsed() > WARN_AFTER => {
            warn!(
                kind,
                queue_depth,
                stream_count,
                frozen_for_secs = start.elapsed().as_secs(),
                "derivations queued but zero {kind} streams — dispatch stuck. \
                 Worker gRPC bidi-streams may have disconnected. \
                 Run `rio-cli derivations --all-active --stuck` to diagnose. \
                 Restart workers: `kubectl rollout restart sts -n rio-builders rio-builders`"
            );
            // Rate-limit: reset so we WARN once/minute, not once/pass.
            *since = Some(Instant::now());
        }
        (false, _) => *since = None,
        _ => {} // frozen but not yet 60s — keep counting
    }
}

impl DagActor {
    // -----------------------------------------------------------------------
    // Dispatch
    // -----------------------------------------------------------------------

    /// Dispatch ready derivations to available workers (FIFO).
    pub(super) async fn dispatch_ready(&mut self) {
        // Standby scheduler: merge DAGs (state warm for fast
        // takeover) but DON'T dispatch. The lease task flips this
        // on acquire/lose via LeaderState::on_acquire/on_lose.
        // SeqCst load: paired with SeqCst stores in LeaderState so
        // the three-field transition (generation, is_leader,
        // recovery_complete) is observably ordered even on ARM.
        // A one-pass lag on a single flag is still harmless (see
        // DagActor.is_leader field doc). In non-K8s mode this is
        // always true — no-op check.
        if !self.is_leader.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        // Also gate on recovery: don't dispatch until recover_from_
        // pg has rebuilt the DAG. Otherwise we'd dispatch from a
        // partial/empty DAG mid-recovery. SeqCst pairs with
        // handle_leader_acquired's SeqCst — sees all recovery
        // writes before proceeding (though actor is single-threaded
        // so this is belt-and-suspenders).
        if !self
            .recovery_complete
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }

        // I-163: any caller reaching here (Tick, MergeDag,
        // PrefetchComplete, ProcessCompletion) is about to do the work
        // the dirty flag represents. Clear it so the NEXT Tick doesn't
        // redundantly re-dispatch when an inline caller already ran.
        // Cleared after the leader/recovery gates — a not-yet-leader
        // standby keeps the flag so the first post-recovery Tick
        // dispatches.
        self.dispatch_dirty = false;

        // I-140: per-phase timing. Same pattern as merge.rs phase!().
        // dispatch_ready is on the hot path (every heartbeat) so the
        // per-phase log is at trace level; only the >1s total is debug.
        let t_total = Instant::now();
        let mut t_phase = Instant::now();
        let mut n_popped = 0u64;
        let mut n_assigned = 0u64;
        macro_rules! phase {
            ($name:literal) => {
                tracing::trace!(elapsed = ?t_phase.elapsed(), phase = $name, "dispatch phase");
                t_phase = Instant::now();
            };
        }

        // I-067/I-070: batched pre-pass — short-circuit Ready FODs
        // whose outputs are already in the store. The per-FOD check
        // inside the dispatch loop below is kept as a fallback for
        // FODs promoted to Ready DURING this pass (from the cascade
        // each completion here triggers). For a fresh-bootstrap merge,
        // ~50 FODs are all Ready at this point — one RPC instead of
        // ~50 sequential ones (~25s of the 49s I-070 merge latency).
        //
        // I-163: returns the set of FOD hashes the batch ALREADY
        // checked (whether or not they completed). The drain loop
        // below skips `fod_outputs_in_store` for these — re-asking
        // the store 200ms later for the same 211 paths is the ~150ms
        // dominant cost of the 169ms/Heartbeat that saturated the
        // actor at medium-mixed-32x scale. The doc-comment on
        // `fod_outputs_in_store` already claimed deferred FODs use
        // the batch; this honors it.
        let batch_checked = self.batch_complete_cached_ready_fods().await;
        phase!("0-batch-fod-precheck");

        // Drain the queue, dispatching eligible derivations and deferring
        // ineligible ones. Deferring (instead of breaking on the first
        // ineligible derivation) prevents head-of-line blocking — an
        // aarch64 drv at queue head must not block all x86_64 dispatch.
        let mut deferred: Vec<DrvHash> = Vec::new();
        let mut dispatched_any = true;
        // Track how many derivations WANTED each class but got deferred.
        // Reported as a gauge at the end — operator signal for "class X
        // is bottlenecked, scale that pool."
        let mut class_deferred: HashMap<String, u64> = HashMap::new();
        // FODs deferred waiting for a fetcher. Separate from
        // class_deferred — fetchers aren't size-classed (ADR-019).
        let mut fod_deferred: u64 = 0;

        // Keep cycling until a full pass with no dispatches AND no stale removals.
        // In practice this terminates quickly: each derivation is either
        // dispatched, deferred, or removed (stale) exactly once per pass.
        while dispatched_any {
            dispatched_any = false;

            while let Some(drv_hash) = self.ready_queue.pop() {
                n_popped += 1;
                // Stale-entry guards: drop if not in DAG or not Ready.
                let Some(state) = self.dag.node(&drv_hash) else {
                    continue;
                };
                if state.status() != DerivationStatus::Ready {
                    continue;
                }
                // Retry backoff: if set and not yet elapsed, defer.
                // The derivation stays Ready + in queue (re-pushed
                // at the end of the pass with the other deferred).
                // Next dispatch pass re-checks — convergent without
                // timers. Cheap: one Instant::now() only for
                // derivations that failed transiently (backoff_until
                // is None for fresh ones).
                if let Some(deadline) = state.backoff_until
                    && Instant::now() < deadline
                {
                    deferred.push(drv_hash);
                    continue;
                }

                // Classify by estimated duration + memory + CPU. None
                // if size_classes unconfigured (optional feature off —
                // no filter, all workers candidates).
                //
                // Also compute the ADR-020 bucketed memory estimate for
                // the resource-fit filter. Same estimator lookup path
                // as compute_capacity_manifest (lookup_entry +
                // bucketed_estimate + self.headroom_mult) so the
                // placement filter and the manifest RPC agree.
                //
                // `is_fixed_output` is captured into a local so the
                // `state` borrow ends here — `node_mut` below needs
                // exclusive access to `self.dag`.
                //
                // Read guard is dropped at the end of this block —
                // BEFORE `assign_to_worker().await`. parking_lot
                // guards aren't `Send`; the await point would be a
                // compile error anyway, but keeping the scope tight
                // is defensive.
                let (target_class, est_memory_bytes, is_fixed_output) = {
                    let pname = state.pname.as_deref();
                    let system = &state.system;
                    let classes = self.size_classes.read();
                    let target_class = crate::assignment::classify(
                        state.est_duration,
                        self.estimator.peak_memory(pname, system),
                        self.estimator.peak_cpu(pname, system),
                        &classes,
                    );
                    // None-chain: no pname → no lookup key → None.
                    // No history entry → None. No memory sample →
                    // bucketed_estimate returns None. All three mean
                    // "cold start" to the filter (any worker fits).
                    //
                    // Skip for FODs: MEMORY_BUCKET_BYTES (4 GiB, ADR-020
                    // compile-workload pod sizing) rounds a 22 MB fetch
                    // up to 4 GiB → resource-fit rejects 2 GiB fetchers.
                    // I-062: 5 recurrences of fod_queue=2 + fetcher_util=0
                    // before the per-clause diagnostic exposed this. FOD
                    // memory is the download buffer, not a compile heap;
                    // resource-fit is the wrong gate.
                    let est_memory_bytes = if state.is_fixed_output {
                        None
                    } else {
                        pname
                            .and_then(|p| self.estimator.lookup_entry(p, system))
                            .and_then(|e| Estimator::bucketed_estimate(&e, self.headroom_mult))
                            .map(|b| b.memory_bytes)
                    };
                    (target_class, est_memory_bytes, state.is_fixed_output)
                };

                // Write the estimate onto the state BEFORE placement
                // so `hard_filter` (via find_executor_with_overflow →
                // best_executor) reads the fresh value. Refreshed each
                // dispatch pass — picks up estimator Tick updates.
                if let Some(state) = self.dag.node_mut(&drv_hash) {
                    state.est_memory_bytes = est_memory_bytes;
                }

                // I-067: a Ready FOD whose output already exists in
                // rio-store should not dispatch — re-fetching is a
                // wasted round-trip at best, and a hash-mismatch
                // poison if upstream changed since the cached output
                // was produced (I-041). The merge-time
                // check_cached_outputs only checks newly_inserted, so
                // a FOD that was already in-DAG (e.g. stuck Ready via
                // I-062, or Completed→Ready via verify_preexisting_
                // completed) is never re-checked there. Re-check here.
                // FOD-only: non-FOD outputs are compile-dependent and
                // the merge-time check covers their fresh-insert case.
                // Best-effort: store unreachable → dispatch as before.
                //
                // I-163: skip the per-FOD RPC if the batch pre-pass
                // already checked this hash. A FOD in `batch_checked`
                // that's still Ready here was found NOT-in-store by
                // the batch (otherwise it would have completed and the
                // status guard above would have dropped it) — no need
                // to ask again. Only cascade-promoted FODs (Ready
                // AFTER the batch ran) hit the per-FOD path.
                if is_fixed_output
                    && !batch_checked.contains(&drv_hash)
                    && self.fod_outputs_in_store(&drv_hash).await
                {
                    self.complete_ready_fod_from_store(&drv_hash).await;
                    dispatched_any = true;
                    continue;
                }

                // Try target class first, then overflow to larger
                // classes if no worker in target has capacity. A
                // "small" build CAN go to a "large" worker (just
                // wasteful); a "large" build CANNOT go to "small"
                // (would under-provision). So overflow walks UP only.
                let (eligible_worker, chosen_class) =
                    self.find_executor_with_overflow(&drv_hash, target_class.as_deref());

                match eligible_worker {
                    Some(executor_id) => {
                        // Record what class we ACTUALLY routed to (may
                        // be larger than target if we overflowed).
                        // Misclassification detector reads this at
                        // completion time.
                        if let Some(state) = self.dag.node_mut(&drv_hash) {
                            state.assigned_size_class = chosen_class.clone();
                        }
                        if let Some(class) = &chosen_class {
                            metrics::counter!(
                                "rio_scheduler_size_class_assignments_total",
                                "class" => class.clone()
                            )
                            .increment(1);
                        }

                        if self.assign_to_worker(&drv_hash, &executor_id).await {
                            dispatched_any = true;
                            n_assigned += 1;
                        } else {
                            // Assignment send failed (worker stream full or
                            // disconnected). Defer — retrying immediately in
                            // the same pass would spin: the channel won't
                            // drain until we yield to the runtime.
                            deferred.push(drv_hash);
                        }
                    }
                    None => {
                        // No eligible worker (even with overflow).
                        //
                        // I-065: if EVERY currently-registered worker of
                        // the matching kind is in failed_builders, this
                        // derivation can never dispatch on this fleet —
                        // it would defer forever (poison threshold
                        // counts failures, but with N workers you can't
                        // exceed N). Poison now so the build fails
                        // visibly instead of hanging silently.
                        //
                        // The "every registered worker" check (not
                        // `failed_builders.len() >= total`) handles
                        // worker replacement: failed_builders may hold
                        // stale IDs that don't count against the
                        // current fleet.
                        if self.failed_builders_exhausts_fleet(&drv_hash, is_fixed_output) {
                            self.poison_and_cascade(&drv_hash).await;
                            continue;
                        }
                        // Defer and track by TARGET class (not chosen —
                        // chosen is None when there's no eligible).
                        // FODs tracked separately: they have no class,
                        // and the operator action is "scale fetchers"
                        // not "scale class X builders".
                        if is_fixed_output {
                            fod_deferred += 1;
                        } else if let Some(class) = target_class {
                            *class_deferred.entry(class).or_insert(0) += 1;
                        }
                        deferred.push(drv_hash);
                    }
                }
            }

            // Re-queue deferred derivations. push_ready recomputes their
            // priority (unchanged since we just popped them), so they
            // slot back into the same position. The old "push_front to
            // preserve order" doesn't apply — priority IS the order.
            for hash in std::mem::take(&mut deferred) {
                self.push_ready(hash);
            }
        }
        phase!("1-drain-loop");

        // Gauge: per-class deferral count. Snapshot from this dispatch
        // pass. Fire-and-forget — next dispatch overwrites. An operator
        // seeing rio_scheduler_class_queue_depth{class="large"}=50 knows
        // large workers are the bottleneck.
        //
        // Zero out ALL configured classes first. Gauges PERSIST in
        // Prometheus until overwritten — a class that was backed up
        // (gauge=50) then cleared (no entries in class_deferred this
        // pass) would STAY at 50 forever without this zeroing.
        // Operators would see a phantom bottleneck.
        for sc in self.size_classes.read().iter() {
            metrics::gauge!("rio_scheduler_class_queue_depth", "class" => sc.name.clone()).set(0.0);
        }
        // Sum before class_deferred is consumed by the gauge loop below.
        // Feeds the I-025 builder-freeze check at the end of this fn.
        let class_total: u64 = class_deferred.values().sum();
        // Now overwrite for classes that actually have deferrals.
        for (class, count) in class_deferred {
            metrics::gauge!("rio_scheduler_class_queue_depth", "class" => class).set(count as f64);
        }

        // FOD queue depth + fetcher utilization (ADR-019 observability).
        // Same snapshot semantics as class_queue_depth: per-dispatch-
        // pass gauge, next pass overwrites. Zero is a legitimate value
        // (no FODs queued), emitted explicitly so Prometheus doesn't
        // persist stale nonzero.
        metrics::gauge!("rio_scheduler_fod_queue_depth").set(fod_deferred as f64);
        // I-048b: count only is_registered() fetchers. A heartbeat-only
        // zombie (stream_tx: None — race after scheduler restart, fixed
        // at the create-side in handle_heartbeat) would inflate `total`
        // here, hiding the freeze: fod_queue>0 + util=0 + total>0 looks
        // like "fetchers busy on something else" when really nothing
        // can dispatch. Filtering by is_registered() makes the freeze
        // detector below fire on genuine no-stream-connected.
        let (busy, total) = self.executors.values().fold((0u32, 0u32), |(b, t), e| {
            if e.kind == rio_proto::types::ExecutorKind::Fetcher && e.is_registered() {
                (b + u32::from(e.running_build.is_some()), t + 1)
            } else {
                (b, t)
            }
        });
        // No fetchers → emit 0.0 (not NaN). An operator seeing
        // fod_queue_depth > 0 AND fetcher_utilization == 0 with no
        // fetchers registered knows the FetcherPool isn't deployed.
        let util = if total > 0 {
            f64::from(busy) / f64::from(total)
        } else {
            0.0
        };
        metrics::gauge!("rio_scheduler_fetcher_utilization").set(util);

        // I-025 freeze detector: WARN if queue pressure + zero streams >60s.
        // `total` is the fetcher-stream count from the fold above.
        check_freeze(
            &mut self.fod_freeze_since,
            fod_deferred > 0 && total == 0,
            "fetcher",
            fod_deferred,
            total as usize,
        );
        let builder_stream_count = self
            .executors
            .values()
            .filter(|e| e.kind == rio_proto::types::ExecutorKind::Builder && e.is_registered())
            .count();
        check_freeze(
            &mut self.builder_freeze_since,
            class_total > 0 && builder_stream_count == 0,
            "builder",
            class_total,
            builder_stream_count,
        );
        phase!("2-gauges");
        let _ = &mut t_phase;
        let total = t_total.elapsed();
        if total >= std::time::Duration::from_secs(1) {
            debug!(
                elapsed = ?total,
                popped = n_popped,
                assigned = n_assigned,
                ready_queue = self.ready_queue.len(),
                "dispatch_ready total"
            );
        }
    }

    /// I-067: best-effort store check for a Ready FOD's outputs.
    ///
    /// I-070: batched form of [`Self::fod_outputs_in_store`] — collect
    /// every Ready FOD's expected outputs, ONE `FindMissingPaths`, then
    /// [`Self::complete_ready_fod_from_store`] each whose outputs are
    /// all present. Fail-open: store unreachable → no-op (per-FOD
    /// fallback in the dispatch loop covers it next pass).
    ///
    /// Iterates the full DAG, not just `ready_queue` — `ready_queue` is
    /// a heap (no peek-iter without drain) and stale entries in it are
    /// harmless (the inner-loop status guard drops them after this
    /// completes them). Full-DAG scan is O(nodes) but the actor is
    /// single-threaded so there's no contention; for a 1085-node merge
    /// the scan is sub-ms vs. ~25s of sequential RPCs it replaces.
    ///
    /// Returns the set of FOD hashes that were CHECKED (regardless of
    /// outcome). The drain loop skips `fod_outputs_in_store` for these
    /// (I-163) — they were either completed here or definitively
    /// found-missing one RPC ago. Empty set on fail-open paths (no
    /// store / RPC error / timeout): the per-FOD fallback then runs as
    /// before, so the fail-open semantics are unchanged.
    async fn batch_complete_cached_ready_fods(&mut self) -> HashSet<DrvHash> {
        let Some(store) = &self.store_client else {
            return HashSet::new();
        };
        // Candidate set: (drv_hash, output_paths). Collected up-front
        // so the FindMissingPaths borrow doesn't hold &self.dag across
        // the .await (and so the completion loop can take &mut self).
        let candidates: Vec<(DrvHash, Vec<String>)> = self
            .dag
            .iter_nodes()
            .filter(|(_, s)| {
                s.status() == DerivationStatus::Ready
                    && s.is_fixed_output
                    && !s.expected_output_paths.is_empty()
            })
            .map(|(h, s)| (DrvHash::from(h), s.expected_output_paths.clone()))
            .collect();
        if candidates.is_empty() {
            return HashSet::new();
        }

        let store_paths: Vec<String> = candidates
            .iter()
            .flat_map(|(_, p)| p.iter().cloned())
            .collect();
        let req = tonic::Request::new(FindMissingPathsRequest { store_paths });
        let missing: HashSet<String> =
            match tokio::time::timeout(self.grpc_timeout, store.clone().find_missing_paths(req))
                .await
            {
                Ok(Ok(r)) => r.into_inner().missing_paths.into_iter().collect(),
                Ok(Err(e)) => {
                    debug!(
                        candidates = candidates.len(),
                        error = %e,
                        "batched FOD store-check FindMissingPaths failed; \
                         per-FOD fallback will retry"
                    );
                    return HashSet::new();
                }
                Err(_) => {
                    debug!(
                        candidates = candidates.len(),
                        timeout = ?self.grpc_timeout,
                        "batched FOD store-check timed out; per-FOD fallback will retry"
                    );
                    return HashSet::new();
                }
            };

        let mut checked = HashSet::with_capacity(candidates.len());
        for (drv_hash, paths) in candidates {
            if paths.iter().all(|p| !missing.contains(p)) {
                self.complete_ready_fod_from_store(&drv_hash).await;
            }
            checked.insert(drv_hash);
        }
        checked
    }

    /// Returns `true` only when `FindMissingPaths` definitively says all
    /// `expected_output_paths` are present. Any uncertainty (no paths to
    /// check, no store_client, RPC error, timeout) returns `false` so the
    /// caller proceeds to dispatch as before — fail-open.
    ///
    /// Fallback for the cascade tail: [`Self::batch_complete_cached_ready_fods`]
    /// at the top of `dispatch_ready` covers every FOD that was Ready
    /// at pass start (one RPC). This per-FOD check fires only for FODs
    /// promoted to Ready DURING the pass (via `find_newly_ready` from a
    /// completion above) — typically zero, occasionally a handful.
    /// Deferred FODs (no fetcher capacity) re-check each tick via the
    /// batch, not here; the answer can flip to `true` mid-queue (an
    /// earlier dispatch on another scheduler/build uploaded it).
    async fn fod_outputs_in_store(&self, drv_hash: &DrvHash) -> bool {
        let Some(state) = self.dag.node(drv_hash) else {
            return false;
        };
        // Floating-CA FODs don't exist (FOD ⇒ fixed hash ⇒ IA path
        // known); guard anyway so an empty-paths edge case can't
        // fall through to "all present".
        if state.expected_output_paths.is_empty() {
            return false;
        }
        let Some(store) = &self.store_client else {
            return false;
        };
        let req = tonic::Request::new(FindMissingPathsRequest {
            store_paths: state.expected_output_paths.clone(),
        });
        match tokio::time::timeout(self.grpc_timeout, store.clone().find_missing_paths(req)).await {
            Ok(Ok(r)) => r.into_inner().missing_paths.is_empty(),
            Ok(Err(e)) => {
                debug!(drv_hash = %drv_hash, error = %e,
                       "FOD store-check FindMissingPaths failed; will dispatch");
                false
            }
            Err(_) => {
                debug!(drv_hash = %drv_hash, timeout = ?self.grpc_timeout,
                       "FOD store-check FindMissingPaths timed out; will dispatch");
                false
            }
        }
    }

    /// I-067: complete a Ready FOD whose output is already in store,
    /// without dispatching to a fetcher.
    ///
    /// Dispatch-time analogue of the merge-time `cached_hits` block in
    /// `handle_merge`, with the post-completion machinery from
    /// `handle_success_completion` (newly-ready cascade + per-build
    /// progress + completion check) since dependents are already in
    /// the DAG. Skips worker-result-only steps: no executor running-
    /// build clear, no `record_durations`, no critical-path accuracy
    /// metric, no CA realisation insert (FOD outputs are
    /// input-addressed; `expected_output_paths` is the realised path).
    async fn complete_ready_fod_from_store(&mut self, drv_hash: &DrvHash) {
        let (drv_path, output_paths, interested) = {
            let Some(state) = self.dag.node_mut(drv_hash) else {
                return;
            };
            if let Err(e) = state.transition(DerivationStatus::Completed) {
                warn!(drv_hash = %drv_hash, error = %e,
                      "FOD store-hit Ready→Completed rejected; dispatching instead");
                return;
            }
            state.output_paths = state.expected_output_paths.clone();
            (
                state.drv_path().to_string(),
                state.output_paths.clone(),
                state.interested_builds.clone(),
            )
        };

        info!(drv_hash = %drv_hash, "FOD output already in store; skipping fetch");
        metrics::counter!("rio_scheduler_cache_hits_total", "source" => "dispatch_fod")
            .increment(1);
        self.persist_status(drv_hash, DerivationStatus::Completed, None)
            .await;
        self.upsert_path_tenants_for(drv_hash).await;

        for ready_hash in self.dag.find_newly_ready(drv_hash) {
            if let Some(s) = self.dag.node_mut(&ready_hash)
                && s.transition(DerivationStatus::Ready).is_ok()
            {
                self.persist_status(&ready_hash, DerivationStatus::Ready, None)
                    .await;
                self.push_ready(ready_hash);
            }
        }

        let event =
            rio_proto::types::build_event::Event::Derivation(rio_proto::dag::DerivationEvent {
                derivation_path: drv_path,
                status: Some(rio_proto::dag::derivation_event::Status::Cached(
                    rio_proto::dag::DerivationCached { output_paths },
                )),
            });
        for build_id in interested {
            self.emit_build_event(build_id, event.clone());
            // I-103: dispatch_fod short-circuit is "completed without
            // assignment" → counts as cached (matches the original
            // LIST_BUILDS_SELECT NOT EXISTS heuristic).
            if let Some(b) = self.builds.get_mut(&build_id) {
                b.cached_count += 1;
            }
            // I-140: one build_summary scan shared, not two.
            let summary = self.dag.build_summary(build_id);
            self.update_build_counts_with(build_id, &summary).await;
            self.emit_progress_with(build_id, &summary);
            self.check_build_completion(build_id).await;
        }
    }

    /// Find a worker for this derivation, starting at `target_class` and
    /// overflowing to progressively larger classes if needed.
    /// I-065: has `failed_builders` excluded EVERY currently-registered
    /// worker of the matching kind?
    ///
    /// Live example: 2-builder cluster, `diffutils.drv` accumulates
    /// `failed_builders=[b0,b1]`. `hard_filter`'s `!contains()` rejects
    /// both → defer forever. `PoisonConfig.threshold=3` never reached.
    /// The build hangs `[Active]` with no log signal.
    ///
    /// Predicate is "every kind-matching registered worker is in the
    /// failed set", not `failed_builders.len() >= total`. The latter
    /// over-counts stale IDs: b0 fails, b0 is replaced by b2, b1 fails
    /// → set={b0,b1} len=2, total=2 → would poison, but b2 was never
    /// tried.
    ///
    /// Returns false (don't poison) when zero workers of that kind are
    /// registered — that's "no workers connected", a transient that the
    /// freeze detector + autoscaler handle. Poisoning then would brick
    /// builds during a deployment rollout.
    pub(super) fn failed_builders_exhausts_fleet(
        &self,
        drv_hash: &DrvHash,
        is_fixed_output: bool,
    ) -> bool {
        let Some(state) = self.dag.node(drv_hash) else {
            return false;
        };
        if state.failed_builders.is_empty() {
            return false;
        }
        let want_kind = if is_fixed_output {
            rio_proto::types::ExecutorKind::Fetcher
        } else {
            rio_proto::types::ExecutorKind::Builder
        };
        let mut fleet = self
            .executors
            .values()
            .filter(|w| w.kind == want_kind && w.is_registered());
        // `all()` on an empty iterator is vacuously true — peek first.
        let Some(first) = fleet.next() else {
            return false;
        };
        let exhausted = std::iter::once(first)
            .chain(fleet)
            .all(|w| state.failed_builders.contains(&w.executor_id));
        if exhausted {
            warn!(
                drv_hash = %drv_hash,
                kind = ?want_kind,
                failed_on = state.failed_builders.len(),
                "failed_builders excludes every registered worker; poisoning \
                 (would otherwise defer forever — see I-065)"
            );
            metrics::counter!("rio_scheduler_poison_fleet_exhausted_total").increment(1);
        }
        exhausted
    }

    ///
    /// Returns `(executor_id, class_actually_used)`. Both None if nobody
    /// can take it (wrong system, all full, no workers).
    ///
    /// Overflow direction: small → large only. A slow build on a small
    /// worker would dominate that worker's single slot; a fast build on
    /// a large worker is just slightly wasteful. scheduler.md:178.
    fn find_executor_with_overflow(
        &self,
        drv_hash: &DrvHash,
        target_class: Option<&str>,
    ) -> (Option<ExecutorId>, Option<String>) {
        let Some(drv_state) = self.dag.node(drv_hash) else {
            return (None, None);
        };

        // r[impl sched.dispatch.no-fod-fallback]
        // r[impl sched.fod.size-class-reactive]
        // FOD overflow walks FETCHER classes only (I-170) — never the
        // builder size_classes chain below. If no fetcher class has a
        // free executor the FOD queues; the scheduler NEVER sends a
        // FOD to a builder under pressure (kind-mismatch in
        // hard_filter is the absolute boundary). A queued FOD is
        // preferable to a builder with internet access.
        //
        // Chain start: `size_class_floor` (set by reactive promotion
        // on prior failure) or `fetcher_size_classes[0]` if never
        // failed. Empty config = no class filter (original behavior).
        if drv_state.is_fixed_output {
            if self.fetcher_size_classes.is_empty() {
                let w =
                    crate::assignment::best_executor(&self.executors, drv_state, &self.dag, None);
                return (w, None);
            }
            // Walk fetcher classes from `floor` upward. Config order is
            // authoritative (smallest→largest); a floor not in the
            // config (stale after a config change) degrades to "start
            // from smallest" via `position().unwrap_or(0)`.
            let floor_idx = drv_state
                .size_class_floor
                .as_deref()
                .and_then(|f| self.fetcher_size_classes.iter().position(|c| c.name == f))
                .unwrap_or(0);
            for class in &self.fetcher_size_classes[floor_idx..] {
                if let Some(w) = crate::assignment::best_executor(
                    &self.executors,
                    drv_state,
                    &self.dag,
                    Some(&class.name),
                ) {
                    return (Some(w), Some(class.name.clone()));
                }
            }
            return (None, None);
        }

        // No classification configured → single best_executor call with
        // no filter. Fast path for deployments without size-classes.
        let Some(target) = target_class else {
            let w = crate::assignment::best_executor(&self.executors, drv_state, &self.dag, None);
            return (w, None);
        };

        // Build the overflow chain: target class, then all classes with
        // cutoff > target's cutoff, sorted ascending. If target cutoff
        // is 30s, chain is [small(30), medium(300), large(3600)].
        //
        // We don't cache this chain because classify() is called fresh
        // per-dispatch anyway (est_duration can change between ticks
        // via estimator refresh) and the sort is 2-4 elements.
        //
        // Read guard lives through the chain walk — no `.await` in
        // this fn, so it's safe. best_executor is sync.
        let classes = self.size_classes.read();
        let target_cutoff = crate::assignment::cutoff_for(target, &classes);
        let mut chain: Vec<(&str, f64)> = classes
            .iter()
            .filter(|c| {
                // Target itself (== cutoff) or larger.
                // target_cutoff=None shouldn't happen (target came
                // FROM classify which reads the same config) but be
                // defensive: if None, include everything.
                target_cutoff.is_none_or(|t| c.cutoff_secs >= t)
            })
            .map(|c| (c.name.as_str(), c.cutoff_secs))
            .collect();
        chain.sort_by(|a, b| a.1.total_cmp(&b.1));

        // Walk the chain: first class with an available worker wins.
        for (class, _) in chain {
            if let Some(w) =
                crate::assignment::best_executor(&self.executors, drv_state, &self.dag, Some(class))
            {
                return (Some(w), Some(class.to_string()));
            }
        }

        (None, None)
    }

    /// Transition a derivation to Assigned and send it to the worker.
    /// Returns `true` if the assignment was sent, `false` if it failed
    /// (caller should defer the derivation, not retry immediately).
    pub(super) async fn assign_to_worker(
        &mut self,
        drv_hash: &DrvHash,
        executor_id: &ExecutorId,
    ) -> bool {
        // Transition ready -> assigned. Do this FIRST (before recording latency
        // or clearing ready_at) so a rejected transition doesn't pollute metrics.
        if let Some(state) = self.dag.node_mut(drv_hash) {
            if let Err(e) = state.transition(DerivationStatus::Assigned) {
                // Not in Ready state (TOCTOU vs. the dispatch_ready pre-check).
                // Caller will defer; the next dispatch pass drops it via the
                // status != Ready guard. Log so operators can spot races.
                warn!(
                    drv_hash = %drv_hash,
                    executor_id = %executor_id,
                    current = ?state.status(),
                    error = %e,
                    "Ready->Assigned transition rejected in assign_to_worker (TOCTOU)"
                );
                metrics::counter!("rio_scheduler_transition_rejected_total", "to" => "assigned")
                    .increment(1);
                return false;
            }
            // Record assignment latency (Ready -> Assigned time) after transitioning
            if let Some(ready_at) = state.ready_at.take() {
                let latency = ready_at.elapsed().as_secs_f64();
                metrics::histogram!("rio_scheduler_assignment_latency_seconds").record(latency);
                // P0539c: same measurement, dashboard-facing name. Kept
                // distinct from `assignment_latency_seconds` (legacy
                // alias) so part-B Grafana JSON can reference a stable
                // name without coupling to the older one's eventual
                // deprecation. Both fed from the same `ready_at`
                // (set on transition→Ready in DerivationState).
                metrics::histogram!("rio_scheduler_dispatch_wait_seconds").record(latency);
            }
            // Clear retry-backoff deadline: we're dispatching, the
            // backoff has been honored (dispatch_ready wouldn't
            // have let us here otherwise). Next failure gets a
            // fresh computed backoff from the (incremented)
            // retry_count.
            state.backoff_until = None;
            state.assigned_executor = Some(executor_id.clone());
        }

        // Single atomic load. The lease task may fetch_add the
        // generation between the DB insert and the WorkAssignment send
        // below (there's an await in between). Without this snapshot,
        // the two reads could see DIFFERENT generations — the PG row
        // says "assigned under gen N" but the worker receives "gen
        // N+1." The worker then rejects its own assignment as stale.
        // Loading once and reusing closes the tear.
        //
        // Acquire pairs with the lease task's Release fetch_add. Sees
        // the generation AND any writes the lease task did before it
        // (is_leader=true, which dispatch_ready checked at loop top).
        let generation = self.generation.load(std::sync::atomic::Ordering::Acquire);

        // Update DB (non-terminal: log failure, don't block dispatch)
        self.persist_status(drv_hash, DerivationStatus::Assigned, Some(executor_id))
            .await;

        // Create assignment in DB. PG BIGINT is signed; cast at THIS
        // boundary, not at the proto-encode sites below. One cast
        // instead of two, and the proto sites are hotter (this PG
        // write is best-effort anyway — log+continue on error).
        if let Some(state) = self.dag.node(drv_hash)
            && let Some(db_id) = state.db_id
            && let Err(e) = self
                .db
                .insert_assignment(db_id, executor_id, generation as i64)
                .await
        {
            error!(drv_hash = %drv_hash, executor_id = %executor_id, error = %e, "failed to insert assignment record");
        }

        // Track on worker. has_capacity() (running_build.is_none()) was
        // checked by hard_filter before we got here, so this never
        // overwrites a live assignment.
        if let Some(worker) = self.executors.get_mut(executor_id) {
            debug_assert!(
                worker.running_build.is_none(),
                "assign_to_worker called for busy executor (hard_filter gap?)"
            );
            worker.running_build = Some(drv_hash.clone());
        }

        // Auto-pin: write input-closure paths to scheduler_live_pins
        // so GC's mark CTE protects them. Same closure
        // approximation as send_prefetch_hint and best_executor
        // scoring (approx_input_closure). Best-effort: PG failure
        // logs + continues; 24h grace period is the fallback.
        // Empty for leaf derivations → no-op.
        {
            let input_paths = crate::assignment::approx_input_closure(&self.dag, drv_hash);
            if !input_paths.is_empty()
                && let Err(e) = self.db.pin_live_inputs(drv_hash, &input_paths).await
            {
                debug!(drv_hash = %drv_hash, error = %e,
                       "failed to pin live inputs (best-effort; grace period is fallback)");
            }
        }

        // PrefetchHint BEFORE WorkAssignment: the worker starts
        // warming its FUSE cache while still parsing the .drv
        // (which it fetches or extracts from drv_content below).
        // A few seconds of head-start on a multi-minute fetch
        // is the win.
        //
        // Same closure approximation as best_executor's bloom
        // scoring (approx_input_closure) — if scoring said "w1
        // has most of these," the hint should be "the few w1
        // DOESN'T have." Consistent approximation = consistent
        // filtering.
        //
        // Best-effort: try_send, failure logs debug not warn
        // (hint not contract). If the channel is full (worker's
        // recv loop busy), the assignment that follows will
        // likely also fail → the reset_to_ready cleanup below
        // handles it. If only the HINT fails, the build still
        // works, just fetches on-demand via FUSE.
        self.send_prefetch_hint(executor_id, drv_hash);

        // CA input resolution: rewrite placeholder paths in
        // env/args/builder to realized output paths before
        // dispatch. Fires when gateway set needs_resolve (ADR-018
        // Appendix B: floating-CA self OR ia.deferred — IA drv
        // with a floating-CA input).
        //
        // `maybe_resolve_ca` returns the (possibly rewritten)
        // drv_content PLUS the realisation lookups performed. On
        // resolve error (missing realisation, PG blip) it logs and
        // returns the original unresolved bytes + empty lookups —
        // the worker's build fails on the placeholder path, which
        // is the correct signal (retry after the realisation lands).
        //
        // The resolve runs in its OWN scoped borrow of `self.dag`
        // (node() + collect_ca_inputs both &-borrow) so the lookups
        // can be stashed via node_mut() below before the main
        // WorkAssignment construction takes its own & borrow.
        let (drv_content_to_send, resolve_lookups) = {
            let Some(state) = self.dag.node(drv_hash) else {
                return false;
            };
            self.maybe_resolve_ca(drv_hash, state).await
        };

        // Stash lookups for handle_success_completion's
        // insert_realisation_deps (the FK needs the parent's own
        // realisation row to exist, which only happens post-build).
        // Empty vec → no-op; non-empty only for CA-on-CA chains
        // that actually resolved.
        if !resolve_lookups.is_empty()
            && let Some(state) = self.dag.node_mut(drv_hash)
        {
            state.pending_realisation_deps = resolve_lookups;
        }

        // Send WorkAssignment to worker via stream
        if let Some(state) = self.dag.node(drv_hash) {
            let build_opts = self.build_options_for_derivation(drv_hash);

            // Assignment token: HMAC-signed if configured, else
            // legacy format-string. The store verifies signed
            // tokens on PutPath (prevents arbitrary-path upload
            // from a compromised worker). Unsigned tokens are
            // accepted by a store with hmac_verifier=None (dev).
            //
            // Expiry: 2× build_timeout (or 2× daemon_timeout
            // default if timeout=0). A worker legitimately
            // uploading after completion is well within that
            // window. Prevents replay from a leaked token later.
            let assignment_token = if let Some(signer) = &self.hmac_signer {
                let timeout_secs = if build_opts.build_timeout > 0 {
                    build_opts.build_timeout
                } else {
                    // Match rio-builder's DEFAULT_DAEMON_TIMEOUT.
                    // Can't reference the const cross-crate, so
                    // duplicate the value. 7200s = 2h.
                    7200
                };
                // Clamp BEFORE saturating_mul: a client sending
                // build_timeout=u64::MAX would get saturating_mul
                // → u64::MAX → expiry_unix = u64::MAX = immortal
                // token. A leaked immortal token defeats the
                // replay-prevention purpose of expiry entirely.
                // 7 days max: well above any real build duration.
                const MAX_HMAC_TIMEOUT_SECS: u64 = 7 * 86400;
                let timeout_secs = timeout_secs.min(MAX_HMAC_TIMEOUT_SECS);
                let expiry_unix = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
                    .saturating_add(timeout_secs.saturating_mul(2));
                signer.sign(&rio_common::hmac::AssignmentClaims {
                    executor_id: executor_id.to_string(),
                    drv_hash: drv_hash.to_string(),
                    expected_outputs: state.expected_output_paths.clone(),
                    // Floating-CA: output path is computed post-build
                    // from the NAR hash, so expected_output_paths is
                    // [""] here. Store skips the path-in-claims check
                    // when is_ca is set (verify-on-put still hashes
                    // the NAR independently; threat model holds).
                    // Fixed-output CA (FOD) has a known path → treat
                    // as IA for the membership check.
                    is_ca: state.is_ca && !state.is_fixed_output,
                    expiry_unix,
                })
            } else {
                // Legacy unsigned: format-string. Store with
                // hmac_verifier=None accepts this.
                format!("{executor_id}-{drv_hash}-{generation}")
            };

            let assignment = rio_proto::types::WorkAssignment {
                drv_path: state.drv_path().to_string(),
                // Forward what the gateway inlined (or empty → worker
                // fetches from store). Gateway only inlines for nodes
                // whose outputs are MISSING (will-dispatch), so cache
                // hits don't bloat this. Worker already handles both
                // paths (executor/mod.rs:241 branches on is_empty).
                // For CA-depends-on-CA derivations, this is the
                // RESOLVED ATerm (placeholders replaced by realized
                // paths) — see maybe_resolve_ca above.
                drv_content: drv_content_to_send,
                output_names: state.output_names.clone(),
                build_options: Some(build_opts),
                assignment_token,
                generation,
                is_fixed_output: state.is_fixed_output,
                traceparent: state.traceparent.clone(),
            };

            let msg = rio_proto::types::SchedulerMessage {
                msg: Some(rio_proto::types::scheduler_message::Msg::Assignment(
                    assignment,
                )),
            };

            if let Some(worker) = self.executors.get(executor_id)
                && let Some(tx) = &worker.stream_tx
                && let Err(e) = tx.try_send(msg)
            {
                warn!(
                    executor_id = %executor_id,
                    drv_hash = %drv_hash,
                    error = %e,
                    "failed to send assignment to worker"
                );
                // Clean up worker tracking (we set drv_hash above;
                // without this, the worker appears busy, causing a
                // phantom capacity leak).
                if let Some(worker) = self.executors.get_mut(executor_id)
                    && worker.running_build.as_ref() == Some(drv_hash)
                {
                    worker.running_build = None;
                }
                // Reset state: Assigned -> Ready. Caller (dispatch_ready)
                // will defer the derivation; next dispatch pass retries.
                // Do NOT push_front here — that would cause the inner
                // dispatch loop to spin (channel is still full).
                if let Some(state) = self.dag.node_mut(drv_hash)
                    && let Err(e) = state.reset_to_ready()
                {
                    // We already transitioned to Assigned, cleared running_builds,
                    // and now can't reset. Derivation is orphaned in Assigned
                    // with no worker actually building. Heartbeat reconciliation
                    // may eventually catch this, but it's a visible hang until then.
                    error!(
                        drv_hash = %drv_hash,
                        executor_id = %executor_id,
                        current = ?state.status(),
                        error = %e,
                        "reset_to_ready failed after assignment send failure; derivation orphaned in Assigned"
                    );
                    metrics::counter!("rio_scheduler_transition_rejected_total", "to" => "ready_reset")
                        .increment(1);
                }

                // Also clean up PG state written above.
                // unpin: pin_live_inputs wrote scheduler_live_pins
                // rows. Without unpinning, they leak until terminal
                // cleanup (which never runs if this drv stays stuck).
                self.unpin_best_effort(drv_hash).await;
                // delete assignment: insert_assignment wrote a
                // 'pending' row. On recovery, this row is misleading
                // (the worker never got the assignment). log-on-fail:
                // this is best-effort cleanup.
                if let Some(state) = self.dag.node(drv_hash)
                    && let Some(db_id) = state.db_id
                    && let Err(e) = self.db.delete_latest_assignment(db_id).await
                {
                    warn!(drv_hash = %drv_hash, error = %e,
                          "delete_latest_assignment failed during try_send rollback");
                }

                // This derivation WAS Assigned (counted in running), now
                // Ready (queued). emit_progress so the dashboard sees
                // the rollback instead of stale state until re-dispatch.
                for build_id in self.get_interested_builds(drv_hash) {
                    self.emit_progress(build_id);
                }

                return false;
            }
        }

        // Emit derivation started event
        let interested_builds = self.get_interested_builds(drv_hash);
        for build_id in &interested_builds {
            self.emit_build_event(
                *build_id,
                rio_proto::types::build_event::Event::Derivation(rio_proto::dag::DerivationEvent {
                    derivation_path: self.drv_path_or_hash_fallback(drv_hash),
                    status: Some(rio_proto::dag::derivation_event::Status::Started(
                        rio_proto::dag::DerivationStarted {
                            executor_id: executor_id.to_string(),
                        },
                    )),
                }),
            );
            // Progress snapshot: running count +1, worker set changed.
            // Critpath unchanged on dispatch (no completion, no
            // update_ancestors) — but the dashboard also uses
            // Progress for the running/queued columns, so emit.
            self.emit_progress(*build_id);
        }

        debug!(drv_hash = %drv_hash, executor_id = %executor_id, "assigned derivation to worker");
        metrics::counter!("rio_scheduler_assignments_total").increment(1);
        true
    }

    /// Send a PrefetchHint for the chosen worker to warm its FUSE
    /// cache. Best-effort: `try_send`, failure logs at debug.
    ///
    /// Same closure approximation as [`best_executor`]'s scoring
    /// (both call `approx_input_closure`). Bloom-filtered against
    /// the worker's last heartbeat filter: skip paths the worker
    /// PROBABLY has. False positives (bloom says yes, worker
    /// doesn't have it) mean we skip a hint we should have sent
    /// — the build's FUSE op fetches it on-demand. False negatives
    /// are impossible (bloom never false-negative on a positive
    /// insert). So we only ever UNDER-hint, never OVER-hint.
    /// Under-hinting is a missed optimization; over-hinting would
    /// waste worker bandwidth on paths it already has.
    ///
    /// No bloom = send everything (pessimistic, same as scoring).
    /// Empty hint (everything filtered) = don't send the message
    /// at all (saves one try_send for the common case where
    /// best_executor picked a warm worker — that's the point of
    /// bloom-locality scoring).
    ///
    /// [`best_executor`]: crate::assignment::best_executor
    fn send_prefetch_hint(&self, executor_id: &ExecutorId, drv_hash: &DrvHash) {
        let input_paths = crate::assignment::approx_input_closure(&self.dag, drv_hash);
        if input_paths.is_empty() {
            // Leaf derivation (no DAG children). Nothing to prefetch.
            // Common for .drv fetches and source tarballs.
            return;
        }

        // Bloom filter. Same logic as count_missing: skip paths the
        // worker claims to have. The filter is up to 10s stale
        // (heartbeat interval) but that's fine — a stale positive
        // means we skip hinting for a path the worker evicted
        // since last heartbeat; the build fetches it on-demand.
        let Some(worker) = self.executors.get(executor_id.as_str()) else {
            // Worker gone between best_executor and here? Actor is
            // single-threaded so this shouldn't happen, but be
            // defensive (the if-let Some(state) at the top of the
            // caller is the same kind of guard).
            return;
        };
        let to_prefetch: Vec<String> = match &worker.bloom {
            Some(bloom) => input_paths
                .into_iter()
                .filter(|p| !bloom.maybe_contains(p))
                .collect(),
            // No bloom = worker didn't send one. Pessimistic: send
            // all. Same "incentivize sending the filter" policy as
            // count_missing.
            None => input_paths,
        };

        // Cap: bound message size. A derivation with 200 deps ×
        // 3 outputs = 600 paths × ~80 bytes = 48 KB. Fine for gRPC
        // but let's not surprise anyone with a 1 MB hint for a
        // pathological case. 100 covers the 95th percentile; the
        // rest fetch on-demand (we cap by truncating, not by
        // "pick the best 100" — that would need per-path nar_size
        // which we don't have. Arbitrary 100 is better than
        // nothing).
        let mut to_prefetch = to_prefetch;
        if to_prefetch.len() > super::MAX_PREFETCH_PATHS {
            to_prefetch.truncate(super::MAX_PREFETCH_PATHS);
        }

        if to_prefetch.is_empty() {
            // Everything filtered = best_executor picked a warm worker.
            // Exactly what bloom-locality scoring is FOR. No hint
            // message needed.
            return;
        }

        let hint_len = to_prefetch.len();
        let hint = rio_proto::types::PrefetchHint {
            store_paths: to_prefetch,
        };
        let msg = rio_proto::types::SchedulerMessage {
            msg: Some(rio_proto::types::scheduler_message::Msg::Prefetch(hint)),
        };

        // try_send: if the channel is full, drop the hint. The
        // assignment that follows uses the SAME channel — if it's
        // full, that assignment also fails and reset_to_ready cleans
        // up. If only this fails (race: channel had 1 slot, hint
        // lost, assignment fit), the build works without prefetch.
        // debug not warn: this is a hint, not a contract.
        if let Some(tx) = &worker.stream_tx {
            match tx.try_send(msg) {
                Ok(()) => {
                    metrics::counter!("rio_scheduler_prefetch_hints_sent_total").increment(1);
                    metrics::counter!("rio_scheduler_prefetch_paths_sent_total")
                        .increment(hint_len as u64);
                }
                Err(e) => {
                    debug!(
                        executor_id = %executor_id,
                        drv_hash = %drv_hash,
                        error = %e,
                        "prefetch hint dropped (channel full; assignment may also fail)"
                    );
                }
            }
        }
    }

    /// If this derivation is CA-floating with CA inputs, resolve
    /// placeholder paths to realized output paths before dispatch.
    /// Returns `(drv_content_bytes, realisation_lookups)`: the
    /// (possibly rewritten) ATerm plus every
    /// `(dep_modular_hash, dep_output_name) → realized_path` lookup
    /// the resolve performed. Caller stashes lookups on
    /// `DerivationState.pending_realisation_deps` for the
    /// completion-time `insert_realisation_deps` call (the FK needs
    /// the parent's OWN realisation row to exist first).
    ///
    /// ADR-018 Appendix B: resolve fires when `needs_resolve` is set
    /// by the gateway — floating-CA self (`has_ca_floating_outputs`)
    /// OR any inputDrv is floating-CA (`ia.deferred`: an IA drv
    /// depending on a CA input has the CA placeholder embedded in
    /// its env/args). Fixed-output CA with no CA inputs doesn't need
    /// resolve — its output path AND its inputs' paths are all
    /// eval-time known.
    ///
    /// The resolve step queries the `realisations` table for each CA
    /// input's `(modular_hash, output_name)` → `output_path`, then
    /// string-replaces placeholders through the ATerm. Each lookup
    /// is staged for `realisation_deps` INSERT (rio's derived build
    /// trace, per ADR-018:45) — though the actual INSERT is deferred
    /// to completion time (the FK needs the parent's OWN realisation
    /// to exist, which only happens post-build).
    ///
    /// Error handling: resolve failure (missing realisation, PG blip)
    /// logs and returns the original unresolved bytes + empty lookups.
    /// The worker's build will then fail on the placeholder path not
    /// existing (`/1ril1qzj...` is not a real store path), triggering
    /// the normal retry-with-backoff. This is correct: a missing
    /// realisation means the input's `wopRegisterDrvOutput` hasn't
    /// landed yet (race), and retry-after-backoff gives it time to.
    async fn maybe_resolve_ca(
        &self,
        drv_hash: &DrvHash,
        state: &crate::state::DerivationState,
    ) -> (Vec<u8>, Vec<crate::ca::RealisationLookup>) {
        // Gate: ADR-018 Appendix B `shouldResolve`. Gateway computes
        // `needs_resolve = has_ca_floating_outputs() || any inputDrv
        // is floating-CA` at translate time. Covers both floating-CA
        // self AND ia.deferred (IA with CA inputs — the CA input's
        // placeholder is embedded in this drv's env/args and needs
        // rewriting to the realized path).
        if !state.needs_resolve {
            return (state.drv_content.clone(), Vec::new());
        }

        // Build the input lists: walk DAG children, split into CA
        // and IA. For CA children we need the MODULAR hash (the
        // `realisations` table key, plumbed by the gateway via
        // `DerivationNode.ca_modular_hash`). For IA children we need
        // the `expected_output_paths` (deterministic, computed at
        // gateway submit time from the parsed `.drv`).
        //
        // Nix's `tryResolve` (derivations.cc:1206-1234) iterates ALL
        // inputDrvs regardless of addressing mode, adding each output
        // path to `inputSrcs`. CA outputs come from realisations; IA
        // outputs are concrete and already in the DAG.
        //
        // Floating-CA with NO inputs at all (rare: a leaf CA drv with
        // only fixed srcs) doesn't need resolve — nothing to collapse
        // into inputSrcs. Short-circuit before the ATerm parse.
        let ca_inputs = self.collect_ca_inputs(drv_hash);
        let ia_inputs = self.collect_ia_inputs(drv_hash);
        if ca_inputs.is_empty() && ia_inputs.is_empty() {
            return (state.drv_content.clone(), Vec::new());
        }

        // No drv_content → recovered derivation (scheduler restart,
        // DAG reloaded from PG, drv_content not persisted). The store
        // has the ATerm — fetch it. Workers do the same when the
        // inline is empty (build_types.proto:231: "Empty = fallback;
        // worker fetches via GetPath"). ~10-50ms round-trip, once
        // per recovered floating-CA dispatch.
        //
        // Checked AFTER the both-empty short-circuit: a recovered
        // floating-CA with no DAG inputs doesn't need resolve and
        // doesn't need the fetch — worker fetches the unresolved
        // `.drv` from the store itself (same path it always does
        // when `drv_content` is empty). Any floating-CA WITH inputs
        // (CA or IA) needs the scheduler-side fetch so
        // `resolve_ca_inputs` can parse `inputDrvs` and serialize
        // the resolved `BasicDerivation` form.
        //
        // The same lossy-on-recovery pattern still applies to
        // `ca_modular_hash` (see `collect_ca_inputs`'s skip-on-None)
        // and `pending_realisation_deps` (best-effort cache,
        // reconstituted here on each resolve).
        //
        // r[impl sched.ca.resolve+2]
        let drv_content = if state.drv_content.is_empty() {
            match self.fetch_drv_content_from_store(drv_hash, state).await {
                Some(bytes) => bytes,
                None => {
                    // Store unreachable or .drv not found — dispatch
                    // unresolved (worker fails on placeholder,
                    // self-heals via retry after a fresh SubmitBuild
                    // re-merges with inline drv_content). Same
                    // degrade as before P0408.
                    warn!(
                        drv_hash = %drv_hash,
                        "recovered CA-on-CA dispatch: drv_content empty + store fetch failed; \
                         dispatching unresolved (worker will fail on placeholder)"
                    );
                    return (state.drv_content.clone(), Vec::new());
                }
            }
        } else {
            state.drv_content.clone()
        };

        match crate::ca::resolve_ca_inputs(&drv_content, &ca_inputs, &ia_inputs, self.db.pool())
            .await
        {
            Ok(resolved) => {
                debug!(
                    drv_hash = %drv_hash,
                    n_ca_inputs = ca_inputs.len(),
                    n_ia_inputs = ia_inputs.len(),
                    n_lookups = resolved.lookups.len(),
                    "CA resolve: rewrote placeholders + collapsed inputSrcs for dispatch"
                );
                (resolved.drv_content, resolved.lookups)
            }
            Err(e) => {
                // Swallow-to-warn for ALL ResolveError variants,
                // including `Db` (transient PG blip). Rationale:
                // the unresolved dispatch → worker fails on the
                // placeholder path → retry-with-backoff fires →
                // next dispatch re-runs resolve. For
                // `RealisationMissing`, the backoff gives the
                // input's `wopRegisterDrvOutput` time to land
                // (race). For `Db`, the backoff IS the retry-PG
                // mechanism — the wasted worker cycle (~seconds
                // to fail on ENOENT) is acceptable vs adding a
                // defer-and-requeue path here (would need a timer
                // to re-dispatch, which `backoff_until` already
                // provides on the FAILURE path). Slot-wasteful
                // but correct; profiling can drive a `Db → defer`
                // split if the waste proves measurable.
                warn!(
                    drv_hash = %drv_hash,
                    error = %e,
                    "CA resolve failed; dispatching unresolved (worker will fail on placeholder)"
                );
                // Return the (possibly fetched-from-store) bytes
                // unresolved. If the fetch succeeded but resolve
                // failed, the worker at least skips its own GetPath.
                (drv_content, Vec::new())
            }
        }
    }

    /// Fetch a derivation's ATerm bytes from the store via `GetPath`.
    ///
    /// The store returns NAR-framed bytes; a `.drv` is a single
    /// regular file, so [`rio_nix::nar::extract_single_file`] unwraps
    /// it to the raw ATerm. This is the same path the worker takes
    /// when `WorkAssignment.drv_content` is empty
    /// ([`rio-builder/src/executor/inputs.rs::fetch_drv_from_store`]).
    ///
    /// Returns `None` on any failure: store unconfigured
    /// (`store_client = None`, test mode), `GetPath` error, timeout,
    /// not-found, or NAR unwrap failure. Callers treat `None` as
    /// "degrade to the pre-P0408 behavior" — dispatch unresolved,
    /// worker fails on placeholder, retry-with-backoff self-heals.
    ///
    /// Hard 2s timeout + 1 MiB NAR cap: a `.drv` is ~1-50 KB ASCII.
    /// A larger-than-1-MiB blob means something is badly wrong (the
    /// path isn't a `.drv`, or the store returned a closure NAR).
    /// Either way, bail — resolve can't parse a non-ATerm.
    async fn fetch_drv_content_from_store(
        &self,
        drv_hash: &DrvHash,
        state: &crate::state::DerivationState,
    ) -> Option<Vec<u8>> {
        /// `.drv` NAR cap. ~1-50 KB typical; 1 MiB is ~20× any
        /// real-world `.drv`. Avoids pulling a multi-GB closure if
        /// the store path was mis-resolved.
        const MAX_DRV_NAR_SIZE: u64 = 1024 * 1024;
        /// End-to-end `GetPath` + stream-drain timeout. ~10-50 ms
        /// typical; 2 s covers a slow store without blocking
        /// dispatch for long. On timeout we degrade to unresolved
        /// dispatch (same as store-unconfigured).
        const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

        let mut client = self.store_client.as_ref()?.clone();
        let drv_path = state.drv_path().to_string();

        let result = rio_proto::client::get_path_nar(
            &mut client,
            &drv_path,
            FETCH_TIMEOUT,
            MAX_DRV_NAR_SIZE,
            None,
            &[],
        )
        .await;

        let nar = match result {
            Ok(Some((_info, nar))) => nar,
            Ok(None) => {
                debug!(
                    drv_hash = %drv_hash,
                    drv_path = %drv_path,
                    "recovered CA resolve: .drv not found in store"
                );
                return None;
            }
            Err(e) => {
                debug!(
                    drv_hash = %drv_hash,
                    drv_path = %drv_path,
                    error = %e,
                    "recovered CA resolve: GetPath failed"
                );
                return None;
            }
        };

        // NAR unwrap: .drv is a single regular file. Anything else
        // (directory, symlink, corrupt NAR) → None.
        match rio_nix::nar::extract_single_file(&nar) {
            Ok(bytes) => Some(bytes),
            Err(e) => {
                debug!(
                    drv_hash = %drv_hash,
                    error = %e,
                    "recovered CA resolve: NAR unwrap failed (not a single regular file)"
                );
                None
            }
        }
    }

    /// Collect CA inputs for resolve. Walks the DAG children (deps)
    /// and returns a `CaResolveInput` for each child with
    /// `is_ca = true` AND a populated `ca_modular_hash`.
    ///
    /// Children with `is_ca && ca_modular_hash.is_none()` are
    /// skipped — the gateway couldn't compute the modular hash
    /// (BasicDerivation fallback, or recovered state where the
    /// hash wasn't persisted). The parent's resolve is incomplete
    /// for that input → worker fails on the placeholder path →
    /// retry-with-backoff. The next SubmitBuild referencing this
    /// child re-merges the proto with a fresh `ca_modular_hash`.
    fn collect_ca_inputs(&self, drv_hash: &DrvHash) -> Vec<crate::ca::CaResolveInput> {
        // DAG children = dependencies (must complete before this drv).
        let children = self.dag.get_children(drv_hash);
        let mut inputs = Vec::new();
        for child_hash in children {
            let Some(child) = self.dag.node(&child_hash) else {
                continue;
            };
            if !child.is_ca {
                continue;
            }
            let Some(modular_hash) = child.ca_modular_hash else {
                // Gateway didn't populate (BasicDerivation fallback
                // OR recovered state). Skip — resolve is incomplete
                // for this input, worker fails on placeholder,
                // retry-with-backoff handles it. debug not warn:
                // recovered chains hit this legitimately; the
                // scheduler-restart-mid-CA-chain case is expected
                // to degrade to worker-retry, not spam logs.
                debug!(
                    drv_hash = %drv_hash,
                    child = %child_hash,
                    "collect_ca_inputs: child is CA but ca_modular_hash unset; \
                     resolve incomplete, worker will fail on placeholder"
                );
                continue;
            };
            inputs.push(crate::ca::CaResolveInput {
                drv_path: child.drv_path().to_string(),
                modular_hash,
                output_names: child.output_names.clone(),
            });
        }
        inputs
    }

    /// Collect IA (input-addressed) inputs for resolve. Walks the
    /// DAG children and returns an [`IaResolveInput`] for each child
    /// with `is_ca = false` AND non-empty `expected_output_paths`.
    ///
    /// IA output paths are deterministic — the gateway computed them
    /// at submit time from the parsed `.drv` and plumbed them via
    /// `DerivationNode.expected_output_paths`. No store RPC needed.
    /// This is the same field [`approx_input_closure`] reads for
    /// prefetch scoring, so the data is already live.
    ///
    /// Children with empty `expected_output_paths` (recovered state
    /// where the paths weren't persisted, or a proto without the
    /// field) are skipped — `resolve_ca_inputs` will log and skip
    /// the `inputSrcs` add for that input. The worker's FUSE layer
    /// on-demand-fetches regardless, so builds don't break; only
    /// resolved-drv-hash compat with Nix is affected.
    ///
    /// [`IaResolveInput`]: crate::ca::IaResolveInput
    /// [`approx_input_closure`]: crate::assignment::approx_input_closure
    fn collect_ia_inputs(&self, drv_hash: &DrvHash) -> Vec<crate::ca::IaResolveInput> {
        let children = self.dag.get_children(drv_hash);
        let mut inputs = Vec::new();
        for child_hash in children {
            let Some(child) = self.dag.node(&child_hash) else {
                continue;
            };
            if child.is_ca {
                // CA child with a modular hash — handled by
                // collect_ca_inputs via realisation lookup. But a CA
                // child WITHOUT a modular hash (recovered state,
                // BasicDerivation fallback) that HAS completed can
                // still contribute its realized output_paths here:
                // the resolve doesn't need the realisation table when
                // we already have the concrete path in-memory.
                if child.ca_modular_hash.is_some() || child.output_paths.is_empty() {
                    continue;
                }
                // Fall through: CA child, no modular hash, but
                // output_paths is populated (completed). Treat as IA
                // for the purpose of inputSrcs collection — the
                // realized path is just as concrete as an IA
                // expected_output_path.
            }
            // Prefer realized output_paths (filled on completion) over
            // expected_output_paths (filled at merge). For IA children
            // the two are equivalent; for the CA-no-hash fallthrough
            // above, only output_paths is usable (expected is [""]
            // for floating-CA).
            let paths = if !child.output_paths.is_empty() {
                &child.output_paths
            } else {
                &child.expected_output_paths
            };
            if paths.is_empty() {
                // Recovered node or proto without the field. Skip;
                // resolve_ca_inputs logs and skips the inputSrcs add.
                continue;
            }
            inputs.push(crate::ca::IaResolveInput {
                drv_path: child.drv_path().to_string(),
                output_names: child.output_names.clone(),
                output_paths: paths.clone(),
            });
        }
        inputs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // check_freeze state machine. `backdate` (from actor/mod.rs) lets us
    // construct Instants in the past without waiting or mocking the clock.

    #[test]
    fn check_freeze_starts_timer_on_first_freeze() {
        let mut since = None;
        check_freeze(&mut since, true, "fetcher", 41, 0);
        assert!(since.is_some(), "frozen=true with None → timer started");
    }

    #[test]
    fn check_freeze_thaw_resets_to_none() {
        let mut since = Some(backdate(30));
        check_freeze(&mut since, false, "fetcher", 0, 5);
        assert!(since.is_none(), "frozen=false → reset to None");

        // Also resets even if we were past the WARN threshold.
        let mut since = Some(backdate(120));
        check_freeze(&mut since, false, "fetcher", 0, 5);
        assert!(since.is_none(), "thaw wins regardless of elapsed");
    }

    #[test]
    fn check_freeze_keeps_counting_before_threshold() {
        let start = backdate(30);
        let mut since = Some(start);
        check_freeze(&mut since, true, "fetcher", 41, 0);
        assert_eq!(
            since,
            Some(start),
            "frozen but under 60s → unchanged (keep counting)"
        );
    }

    #[test]
    fn check_freeze_resets_timer_after_warn() {
        // Past the 60s threshold: the WARN fires and `since` is reset
        // to ~now for rate-limiting (once/minute, not once/pass).
        let start = backdate(61);
        let mut since = Some(start);
        check_freeze(&mut since, true, "fetcher", 41, 0);
        // Timer was reset: new Instant, strictly after the old one.
        let new = since.expect("still frozen → still Some");
        assert!(new > start, "rate-limit reset: new timer > old start");
        // And the reset is recent (within the last second — the call just happened).
        assert!(
            new.elapsed() < std::time::Duration::from_secs(1),
            "reset to ~now"
        );
    }

    #[test]
    fn check_freeze_noop_when_never_frozen() {
        let mut since = None;
        check_freeze(&mut since, false, "fetcher", 0, 5);
        assert!(since.is_none(), "never frozen → stays None");
    }
}
