//! Completion handling: worker reports build done → update DAG, cascade, emit events.
// r[impl sched.completion.idempotent]
// r[impl sched.critical-path.incremental]

use super::*;

impl DagActor {
    // -----------------------------------------------------------------------
    // Best-effort persist helpers (13 call sites across actor/*)
    // -----------------------------------------------------------------------

    /// Best-effort PG persist of derivation status. Logs error!,
    /// never returns it — PG blips shouldn't abort in-mem
    /// transitions (the scheduler is authoritative for live state;
    /// PG is recovery-only).
    pub(super) async fn persist_status(
        &self,
        drv_hash: &DrvHash,
        status: DerivationStatus,
        worker_id: Option<&WorkerId>,
    ) {
        if let Err(e) = self
            .db
            .update_derivation_status(drv_hash, status, worker_id)
            .await
        {
            error!(drv_hash = %drv_hash, ?status, error = %e,
                   "failed to persist derivation status");
        }
    }

    /// Best-effort atomic persist of `status='poisoned'` + `poisoned_at=now()`.
    /// Single SQL UPDATE — no crash window between the two columns.
    /// Logs error!, never returns it (same semantics as `persist_status`).
    pub(super) async fn persist_poisoned(&self, drv_hash: &DrvHash) {
        if let Err(e) = self.db.persist_poisoned(drv_hash).await {
            error!(drv_hash = %drv_hash, error = %e,
                   "failed to persist poisoned status+timestamp");
        }
    }

    /// Best-effort unpin of `scheduler_live_pins` rows for a
    /// terminal derivation. Called at every terminal transition
    /// (Completed/Poisoned/Cancelled; DependencyFailed is never
    /// dispatched so never pinned). `sweep_stale_live_pins` on
    /// recovery is the crash safety net for missed unpins.
    pub(super) async fn unpin_best_effort(&self, drv_hash: &DrvHash) {
        if let Err(e) = self.db.unpin_live_inputs(drv_hash).await {
            debug!(drv_hash = %drv_hash, error = %e,
                   "failed to unpin live inputs (best-effort)");
        }
    }

    /// Record a worker failure for `drv_hash` (in-mem + PG
    /// best-effort) and return whether the poison threshold is
    /// reached. Caller decides: poison_and_cascade if true,
    /// reset_to_ready + retry if false.
    ///
    /// Both `failed_workers` (HashSet — for distinct-mode + best_worker
    /// exclusion) and `failure_count` (flat counter — for non-distinct
    /// mode) are updated; [`PoisonConfig::is_poisoned`] picks the
    /// right one. The in-mem insert comes first (scheduler-
    /// authoritative); PG is recovery-only. A PG blip degrades to
    /// "might retry on the same worker once post-recovery."
    pub(super) async fn record_failure_and_check_poison(
        &mut self,
        drv_hash: &DrvHash,
        worker_id: &WorkerId,
    ) -> bool {
        if let Some(state) = self.dag.node_mut(drv_hash) {
            state.failed_workers.insert(worker_id.clone());
            // Unconditional (doesn't check HashSet::insert's bool) —
            // same worker counts twice. Only used when
            // require_distinct_workers=false.
            state.failure_count += 1;
        }
        if let Err(e) = self.db.append_failed_worker(drv_hash, worker_id).await {
            error!(drv_hash = %drv_hash, worker_id = %worker_id, error = %e,
                   "failed to persist failed_worker");
        }
        self.dag
            .node(drv_hash)
            .map(|s| self.poison_config.is_poisoned(s))
            .unwrap_or(false)
    }

    // -----------------------------------------------------------------------
    // ProcessCompletion
    // -----------------------------------------------------------------------

    #[instrument(skip(self, result), fields(worker_id = %worker_id, drv_key = %drv_key))]
    pub(super) async fn handle_completion(
        &mut self,
        worker_id: &WorkerId,
        // gRPC layer passes CompletionReport.drv_path; may be a drv_hash in tests.
        drv_key: &str,
        result: rio_proto::types::BuildResult,
        // CompletionReport resource fields. 0 = worker had no signal
        // (build failed before cgroup populated). Converted to None
        // before the DB write so the EMA isn't dragged toward zero.
        //
        // Tuple to stay under clippy's 7-arg limit. All three are
        // "resource measurements from the cgroup" with identical
        // zero-means-no-signal semantics; unpacked immediately.
        (peak_memory_bytes, output_size_bytes, peak_cpu_cores): (u64, u64, f64),
    ) {
        let status =
            rio_proto::types::BuildResultStatus::try_from(result.status).unwrap_or_else(|_| {
                tracing::warn!(
                    raw_status = result.status,
                    "unknown BuildResultStatus from worker, treating as Unspecified"
                );
                rio_proto::types::BuildResultStatus::Unspecified
            });

        // Resolve drv_key (which may be a drv_path or a drv_hash) to drv_hash.
        // Boundary: construct the typed DrvHash ONCE here, then pass &DrvHash
        // to all internal handlers. The gRPC layer sends raw &str; internal
        // functions after this point use the newtype.
        let drv_hash: DrvHash = if self.dag.contains(drv_key) {
            drv_key.into()
        } else if let Some(h) = self.drv_path_to_hash(drv_key) {
            h
        } else {
            warn!(key = drv_key, "completion for unknown derivation, ignoring");
            return;
        };
        let drv_hash = &drv_hash;

        // Find the derivation in the DAG
        let Some(state) = self.dag.node(drv_hash) else {
            warn!(drv_hash = %drv_hash, "completion for unknown derivation, ignoring");
            return;
        };
        let current_status = state.status();

        // Idempotency: completed -> completed is a no-op
        if current_status == DerivationStatus::Completed {
            debug!(drv_hash = %drv_hash, "duplicate completion report, ignoring");
            return;
        }

        // Only process completions for assigned/running derivations
        if !matches!(
            current_status,
            DerivationStatus::Assigned | DerivationStatus::Running
        ) {
            warn!(
                drv_hash = %drv_hash,
                current_status = %current_status,
                "completion for derivation not in assigned/running state, ignoring"
            );
            return;
        }

        match status {
            rio_proto::types::BuildResultStatus::Built
            | rio_proto::types::BuildResultStatus::Substituted
            | rio_proto::types::BuildResultStatus::AlreadyValid => {
                self.handle_success_completion(
                    drv_hash,
                    &result,
                    worker_id,
                    (peak_memory_bytes, output_size_bytes, peak_cpu_cores),
                )
                .await;
            }
            rio_proto::types::BuildResultStatus::TransientFailure => {
                // Build ran, exited non-zero. Counts toward poison — 3
                // workers all seeing this means it's not actually transient.
                self.handle_transient_failure(drv_hash, worker_id).await;
            }
            // r[impl sched.retry.per-worker-budget]
            rio_proto::types::BuildResultStatus::InfrastructureFailure => {
                // Worker-local problem (FUSE EIO, cgroup setup fail, OOM-
                // kill of the build process). Not the build's fault. Retry
                // WITHOUT inserting into failed_workers.
                self.handle_infrastructure_failure(drv_hash, worker_id)
                    .await;
            }
            rio_proto::types::BuildResultStatus::PermanentFailure
            | rio_proto::types::BuildResultStatus::CachedFailure
            | rio_proto::types::BuildResultStatus::DependencyFailed
            | rio_proto::types::BuildResultStatus::LogLimitExceeded
            | rio_proto::types::BuildResultStatus::OutputRejected
            // TimedOut: same inputs → same timeout. Reassigning is a
            // storm. The operator fix is "raise spec.timeoutSeconds,"
            // not "try another worker."
            | rio_proto::types::BuildResultStatus::TimedOut
            // NotDeterministic: nix --check failed. Retrying doesn't
            // help — the nondeterminism is in the build itself.
            | rio_proto::types::BuildResultStatus::NotDeterministic
            // InputRejected: corrupt/invalid .drv. Same .drv on another
            // worker is still corrupt.
            | rio_proto::types::BuildResultStatus::InputRejected => {
                self.handle_permanent_failure(drv_hash, &result.error_msg, worker_id)
                    .await;
            }
            rio_proto::types::BuildResultStatus::Cancelled => {
                // Worker reports Cancelled after cgroup.kill. The
                // scheduler already transitioned the DerivationState
                // when it SENT the CancelSignal (see handle_cancel_
                // build / handle_drain_worker), so this report is
                // expected but needs no further action on the
                // derivation itself. Just the worker-capacity cleanup
                // below. Log at debug: every CancelBuild generates
                // one of these per running drv, and it's the happy
                // path for cancel.
                debug!(drv_hash = %drv_hash, worker_id = %worker_id,
                       "cancelled completion report (expected after CancelSignal)");
            }
            rio_proto::types::BuildResultStatus::Unspecified => {
                warn!(
                    drv_hash = %drv_hash,
                    status = result.status,
                    "unknown build result status, treating as transient failure"
                );
                self.handle_transient_failure(drv_hash, worker_id).await;
            }
        }

        // Free worker capacity
        if let Some(worker) = self.workers.get_mut(worker_id) {
            worker.running_builds.remove(drv_hash);
        }

        // Dispatch newly ready derivations
        self.dispatch_ready().await;
    }

    pub(super) async fn handle_success_completion(
        &mut self,
        drv_hash: &DrvHash,
        result: &rio_proto::types::BuildResult,
        worker_id: &WorkerId,
        // Same tuple pattern as handle_completion — clippy 7-arg limit.
        (peak_memory_bytes, output_size_bytes, peak_cpu_cores): (u64, u64, f64),
    ) {
        // Transition to completed
        if let Some(state) = self.dag.node_mut(drv_hash) {
            state.ensure_running();
            if let Err(e) = state.transition(DerivationStatus::Completed) {
                // Worker reported success but the in-memory state machine rejected
                // the transition (e.g., derivation was cascaded to DependencyFailed
                // or reset by heartbeat reconciliation in a race). The build result
                // is lost; downstream derivations will never be released.
                error!(
                    drv_hash = %drv_hash,
                    worker_id = %worker_id,
                    current_state = ?state.status(),
                    error = %e,
                    "worker reported success but Running->Completed transition rejected; build will hang"
                );
                metrics::counter!("rio_scheduler_transition_rejected_total", "to" => "completed")
                    .increment(1);
                return;
            }

            // Store output paths from built_outputs
            state.output_paths = result
                .built_outputs
                .iter()
                .map(|o| o.output_path.clone())
                .collect();
        }

        self.persist_status(drv_hash, DerivationStatus::Completed, None)
            .await;
        self.unpin_best_effort(drv_hash).await;

        // Update assignment (non-terminal progress write: log failure, don't block)
        if let Some(state) = self.dag.node(drv_hash)
            && let Some(db_id) = state.db_id
            && let Err(e) = self
                .db
                .update_assignment_status(db_id, crate::db::AssignmentStatus::Completed)
                .await
        {
            error!(drv_hash = %drv_hash, error = %e, "failed to persist assignment completion");
        }

        // Async: update build history EMA (best-effort statistics)
        if let Some(state) = self.dag.node(drv_hash)
            && let Some(pname) = &state.pname
            && let (Some(start), Some(stop)) = (&result.start_time, &result.stop_time)
        {
            // Convert to seconds FIRST, then subtract. Subtracting
            // nanos separately underflows when stop.nanos < start.nanos
            // (e.g., start=10.900s, stop=11.100s → real diff 0.2s, but
            // separate-subtract gives 1s+0ns = 1.0s). Computing each
            // timestamp as a single f64 in seconds is correct.
            let start_f = start.seconds as f64 + start.nanos as f64 / 1_000_000_000.0;
            let stop_f = stop.seconds as f64 + stop.nanos as f64 / 1_000_000_000.0;
            let duration_secs = stop_f - start_f;
            // Sanity bound: reject durations > 30 days (bogus worker timestamps)
            if duration_secs > 0.0 && duration_secs < 30.0 * 86400.0 {
                // 0 → None: "no signal" must not drag the EMA toward
                // zero. Worker uses 0 for build-failed-before-cgroup-
                // populated paths. A real build using 0 bytes doesn't
                // exist (nix-daemon alone is ~10MB cgroup peak); 0.0
                // CPU cores likewise means "no samples taken" (build
                // exited in <1s before the 1Hz poller fired).
                //
                // Phase2c correction: prior values in build_history
                // are the WRONG memory (~10MB for every build, from
                // daemon-PID VmHWM). cgroup memory.peak is correct.
                // EMA alpha=0.3 → 0.7^10 ≈ 2.8% old value after 10
                // completions. No migration; time heals.
                let peak_mem = (peak_memory_bytes > 0).then_some(peak_memory_bytes);
                let out_size = (output_size_bytes > 0).then_some(output_size_bytes);
                let peak_cpu = (peak_cpu_cores > 0.0).then_some(peak_cpu_cores);
                if let Err(e) = self
                    .db
                    .update_build_history(
                        pname,
                        &state.system,
                        duration_secs,
                        peak_mem,
                        out_size,
                        peak_cpu,
                    )
                    .await
                {
                    error!(drv_hash = %drv_hash, error = %e, "failed to update build history EMA");
                }

                // Raw sample for the CutoffRebalancer (P0229). Unlike
                // the EMA above (one smoothed scalar per pname), this
                // appends every completion — the rebalancer needs the
                // full distribution. Best-effort: warn, never fail
                // completion on sample-write error.
                //
                // peak_memory_bytes passed raw (as i64), not the 0→None
                // filtered Option — the rebalancer wants the full
                // distribution including zeros; 0 is a legitimate
                // sample point ("sub-second build, poller didn't fire").
                // The EMA needs the filter to avoid dragging toward 0;
                // the percentile computation doesn't.
                if let Err(e) = self
                    .db
                    .insert_build_sample(
                        pname,
                        &state.system,
                        duration_secs,
                        peak_memory_bytes as i64,
                    )
                    .await
                {
                    warn!(?e, %pname, system = %state.system, "insert_build_sample failed");
                }

                // Misclassification: routed to "small" but ran like
                // "large"? Threshold is 2× the assigned class's cutoff —
                // a build routed to a 30s class that took 61s triggers.
                // 2× is generous enough to avoid noise from normal
                // variance (build caches, CPU contention) while still
                // catching the "this is clearly not a small build" case.
                //
                // Penalty: overwrite EMA with actual (not blend). Next
                // classify() sees the real duration and picks right.
                // Harsh but self-correcting — a fluke gets blended back
                // down by the next normal completion.
                // r[impl sched.classify.penalty-overwrite]
                if let Some(assigned_class) = &state.assigned_size_class {
                    // Cutoff-drift: would we classify differently post-hoc?
                    // Distinct from the misclassification/penalty check
                    // below — a build can drift (assigned "small"
                    // cutoff=60s, actual=65s, next cutoff=120s) without
                    // tripping the 2× penalty. Drift measures "the
                    // cutoffs we dispatched with were wrong"; penalty
                    // measures "this was so wrong we overwrite the EMA."
                    //
                    // classify() takes the static size_classes slice
                    // (RwLock wires in P0230). Drift is measured
                    // against config-at-dispatch, which is correct:
                    // "given the cutoffs we had, would post-hoc
                    // classification pick differently?"
                    //
                    // peak_mem is the 0→None Option from :309 — memory
                    // bump logic in classify() should see the same
                    // filtered signal the original dispatch-time
                    // classify() saw (which also reads from the EMA's
                    // 0→None filtered history).
                    if let Some(actual_class) = crate::assignment::classify(
                        duration_secs,
                        peak_mem.map(|m| m as f64),
                        &self.size_classes,
                    ) && &actual_class != assigned_class
                    {
                        metrics::counter!(
                            "rio_scheduler_class_drift_total",
                            "assigned_class" => assigned_class.clone(),
                            "actual_class" => actual_class,
                        )
                        .increment(1);
                    }

                    if let Some(cutoff) =
                        crate::assignment::cutoff_for(assigned_class, &self.size_classes)
                        && duration_secs > 2.0 * cutoff
                    {
                        warn!(
                            drv_hash = %drv_hash,
                            pname = %pname,
                            assigned_class = %assigned_class,
                            cutoff_secs = cutoff,
                            actual_secs = duration_secs,
                            "misclassification: actual > 2× cutoff; penalty-writing EMA"
                        );
                        metrics::counter!("rio_scheduler_misclassifications_total").increment(1);
                        if let Err(e) = self
                            .db
                            .update_build_history_misclassified(pname, &state.system, duration_secs)
                            .await
                        {
                            error!(drv_hash = %drv_hash, error = %e, "failed to write misclassification penalty");
                        }
                    }
                }
            }
        }

        // Emit derivation completed event
        let output_paths: Vec<String> = self
            .dag
            .node(drv_hash)
            .map(|s| s.output_paths.clone())
            .unwrap_or_default();

        let interested_builds = self.get_interested_builds(drv_hash);
        for build_id in &interested_builds {
            self.emit_build_event(
                *build_id,
                rio_proto::types::build_event::Event::Derivation(
                    rio_proto::types::DerivationEvent {
                        derivation_path: self.drv_path_or_hash_fallback(drv_hash),
                        status: Some(rio_proto::types::derivation_event::Status::Completed(
                            rio_proto::types::DerivationCompleted {
                                output_paths: output_paths.clone(),
                            },
                        )),
                    },
                ),
            );
        }

        // Trigger log flush AFTER the Completed event has gone out. By the
        // time the gateway sees Completed, the ring buffer still has the full
        // log (flusher hasn't drained yet — it's async on a separate task).
        // So AdminService.GetBuildLogs can serve from the ring buffer in the
        // gap between Completed and the S3 upload landing.
        self.trigger_log_flush(drv_hash, interested_builds.clone());

        // r[impl sched.gc.path-tenants-upsert]
        // Best-effort: GC may under-retain on failure, but never fail
        // completion. The 24h global grace is the fallback.
        //
        // tenant_id: Option<Uuid> — filter_map drops None (single-tenant
        // mode; empty SSH-key comment → gateway sends "" → scheduler
        // stores None). Only builds with a resolved tenant contribute.
        let tenant_ids: Vec<Uuid> = interested_builds
            .iter()
            .filter_map(|id| self.builds.get(id)?.tenant_id)
            .collect();
        if !tenant_ids.is_empty()
            && !output_paths.is_empty()
            && let Err(e) = self
                .db
                .upsert_path_tenants(&output_paths, &tenant_ids)
                .await
        {
            warn!(
                ?e,
                output_paths = output_paths.len(),
                tenants = tenant_ids.len(),
                "path_tenants upsert failed; GC retention may under-retain"
            );
        }

        // Update ancestor priorities: this node is now terminal, so it
        // no longer contributes to its parents' max-child-priority.
        // Parents' priorities DROP. Propagates up until unchanged
        // (dirty-flag stop).
        //
        // Done BEFORE releasing downstream because the newly-ready
        // nodes will be pushed to the queue by priority, and their
        // priority is already correct from compute_initial at merge
        // time. The ancestors that change are NOT newly-ready (they
        // were already in the queue or beyond) — the queue's lazy
        // invalidation handles re-pushing them if needed.
        //
        // Also emit the accuracy metric: how close was our estimate?
        // Histogram of actual/estimated. 1.0 = perfect, >1.0 = took
        // longer than expected. Helps tune the estimator.
        if let Some(state) = self.dag.node(drv_hash)
            && state.est_duration > 0.0
            && let (Some(start), Some(stop)) = (&result.start_time, &result.stop_time)
        {
            let actual_secs = stop.seconds.saturating_sub(start.seconds) as f64;
            if actual_secs > 0.0 {
                metrics::histogram!("rio_scheduler_critical_path_accuracy")
                    .record(actual_secs / state.est_duration);
            }
        }
        crate::critical_path::update_ancestors(&mut self.dag, drv_hash);

        // Release downstream: find newly ready derivations.
        // push_ready handles interactive boost via priority.
        let newly_ready = self.dag.find_newly_ready(drv_hash);
        for ready_hash in newly_ready {
            if let Some(state) = self.dag.node_mut(&ready_hash)
                && state.transition(DerivationStatus::Ready).is_ok()
            {
                self.persist_status(&ready_hash, DerivationStatus::Ready, None)
                    .await;
                self.push_ready(ready_hash);
            }
        }

        // Update build completion status
        for build_id in &interested_builds {
            self.update_build_counts(*build_id);
            // Progress snapshot AFTER update_ancestors (critpath is
            // fresh — root priority dropped when this drv went
            // terminal) and BEFORE check_build_completion (which may
            // emit BuildCompleted; a final Progress showing 0
            // remaining is still useful right before that).
            self.emit_progress(*build_id);
            self.check_build_completion(*build_id).await;
        }
    }

    /// Transition a derivation to Poisoned, persist, cascade
    /// DependencyFailed to ancestors, and propagate to interested
    /// builds. Called when the poison threshold is reached (see
    /// [`PoisonConfig::is_poisoned`]) or max_retries is hit.
    ///
    /// **Precondition:** status must be Assigned or Running.
    /// Enforced via debug_assert! (tests catch violations) +
    /// early-return on transition failure (release builds don't
    /// cascade spuriously). All 3 current callers guarantee this;
    /// actor is single-threaded so no race between filter and call.
    /// Handles the Assigned→Running intermediate (state machine
    /// requires Running before Poisoned). For the reassign path
    /// (worker disconnect), the caller checks the threshold BEFORE
    /// reset_to_ready — the drv is still Assigned/Running then.
    pub(super) async fn poison_and_cascade(&mut self, drv_hash: &DrvHash) {
        let Some(state) = self.dag.node_mut(drv_hash) else {
            return;
        };
        debug_assert!(
            matches!(
                state.status(),
                DerivationStatus::Assigned | DerivationStatus::Running
            ),
            "poison_and_cascade precondition violated: got {:?}",
            state.status()
        );
        state.ensure_running();
        if let Err(e) = state.transition(DerivationStatus::Poisoned) {
            // Unexpected state (not Assigned/Running). debug_assert!
            // above fires in tests; in release, DON'T write Poisoned
            // to PG or cascade — in-mem ≠ PG + spurious cascade is
            // worse than a missed poison (which the next tick/
            // completion will re-evaluate).
            warn!(drv_hash = %drv_hash, error = %e, current = ?state.status(),
                  "poison_and_cascade: ->Poisoned transition rejected, skipping PG write + cascade");
            return;
        }
        state.poisoned_at = Some(Instant::now());

        self.persist_poisoned(drv_hash).await;
        self.unpin_best_effort(drv_hash).await;

        // Cascade: parents of a poisoned derivation can never complete.
        // Transition them to DependencyFailed so keepGoing builds terminate.
        self.cascade_dependency_failure(drv_hash).await;

        // Propagate failure to interested builds.
        let interested_builds = self.get_interested_builds(drv_hash);
        for build_id in interested_builds {
            self.handle_derivation_failure(build_id, drv_hash).await;
        }
    }

    // r[impl sched.admin.clear-poison]
    /// Clear poison state for a derivation (admin-initiated via
    /// `AdminService.ClearPoison`). Returns `true` if cleared.
    ///
    /// In-mem: node removed from the DAG entirely — next submit re-inserts
    /// it fresh with full proto fields and runs it through
    /// `compute_initial_states`. PG: `db.clear_poison()` sets
    /// status='created', NULLs `poisoned_at`/`retry_count`/`failed_workers`.
    ///
    /// PG first, in-mem second — if PG fails the operator's retry
    /// finds in-mem still Poisoned and can proceed. The previous
    /// order (in-mem first) meant a PG blip left status=Created,
    /// so retry hit the not-poisoned guard → permanent no-op until
    /// scheduler restart.
    pub(super) async fn handle_clear_poison(&mut self, drv_hash: &DrvHash) -> bool {
        match self.dag.node(drv_hash).map(|s| s.status()) {
            None => return false, // not found
            Some(s) if s != DerivationStatus::Poisoned => return false,
            Some(_) => {}
        }
        if let Err(e) = self.db.clear_poison(drv_hash).await {
            error!(drv_hash = %drv_hash, error = %e,
                   "ClearPoison: PG clear failed (in-mem untouched; retry-safe)");
            return false;
        }
        // Remove from DAG so next merge treats it as newly-inserted.
        // Resetting status in-place would strand stub fields from
        // `from_poisoned_row` and `compute_initial_states` only iterates
        // `newly_inserted` — node would sit in Created forever. Poisoned
        // nodes have no interested builds (build already terminated) so
        // removal here orphans no live build accounting.
        self.dag.remove_node(drv_hash);
        info!(drv_hash = %drv_hash, "poison cleared by admin; node removed from DAG");
        true
    }

    pub(super) async fn handle_transient_failure(
        &mut self,
        drv_hash: &DrvHash,
        worker_id: &WorkerId,
    ) {
        // Record failure (in-mem HashSet insert + PG append,
        // best-effort) + get poison verdict in one call — same
        // helper as reassign_derivations (worker.rs) and
        // handle_reconcile_assignments (recovery.rs).
        let reached_poison = self
            .record_failure_and_check_poison(drv_hash, worker_id)
            .await;

        let should_retry = if let Some(state) = self.dag.node_mut(drv_hash) {
            if reached_poison {
                false // poison_and_cascade below does the transition
            } else if state.retry_count < self.retry_policy.max_retries {
                state.ensure_running();
                if let Err(e) = state.transition(DerivationStatus::Failed) {
                    warn!(drv_hash = %drv_hash, error = %e, "Running->Failed transition failed");
                }
                true
            } else {
                false // poison_and_cascade below does the transition
            }
        } else {
            return;
        };

        if should_retry {
            let retry_count = self.dag.node(drv_hash).map_or(0, |s| s.retry_count);

            // Schedule retry with backoff
            let backoff = self.retry_policy.backoff_duration(retry_count);
            let drv_hash_owned: DrvHash = drv_hash.clone();

            if let Some(state) = self.dag.node_mut(&drv_hash_owned) {
                state.retry_count += 1;
                state.assigned_worker = None;
            }

            if let Err(e) = self.db.increment_retry_count(drv_hash).await {
                error!(drv_hash = %drv_hash, error = %e, "failed to persist retry increment");
            }

            debug!(
                drv_hash = %drv_hash,
                retry_count = retry_count + 1,
                backoff_secs = backoff.as_secs_f64(),
                "scheduling retry after transient failure"
            );

            // Delayed re-queue: set backoff_until on the state, then
            // Failed → Ready + push. dispatch_ready checks
            // backoff_until and defers if not yet elapsed. Stateless
            // — no timer tasks, no cleanup if the derivation is
            // cancelled meanwhile (backoff_until is just an Option
            // on the state, ignored for non-Ready). Cleared on
            // successful dispatch in assign_to_worker.
            if let Some(state) = self.dag.node_mut(&drv_hash_owned) {
                state.backoff_until = Some(Instant::now() + backoff);
                if let Err(e) = state.transition(DerivationStatus::Ready) {
                    warn!(drv_hash = %drv_hash, error = %e, "Failed->Ready transition failed");
                } else {
                    self.push_ready(drv_hash_owned);
                }
            }
            // PG status: Ready, NOT Failed. The in-mem state machine
            // goes Failed→Ready (Failed is an intermediate); PG must
            // match the FINAL in-mem state. Crash in the backoff
            // window (up to 300s) with PG=Failed → recovery loads it
            // (Failed not in db.rs:537 terminal filter) but only
            // pushes Ready-status drvs to the queue (recovery.rs:225)
            // → Failed drv sits in DAG forever, never dispatched.
            self.persist_status(drv_hash, DerivationStatus::Ready, None)
                .await;
        } else {
            self.poison_and_cascade(drv_hash).await;
        }
    }

    /// InfrastructureFailure: worker-local problem, not the build's fault.
    /// Reset to Ready and retry WITHOUT inserting into `failed_workers`.
    ///
    /// This CAN loop if the infrastructure problem is widespread (all
    /// workers have it) — but that's a cluster problem, not a per-
    /// derivation one. Without inserting, this worker is immediately
    /// re-eligible. That's fine: InfrastructureFailure is the worker
    /// saying "I can't right now." If it's still broken, it'll fail
    /// again. If it's recovered (circuit closed), it'll succeed.
    ///
    /// The original P0219 plan had a `per_worker_failures` HashMap to
    /// cap retries on a persistently-broken worker. Not needed:
    /// P0211's `store_degraded` heartbeat → `has_capacity()` false
    /// already excludes broken workers from assignment upstream.
    pub(super) async fn handle_infrastructure_failure(
        &mut self,
        drv_hash: &DrvHash,
        worker_id: &WorkerId,
    ) {
        info!(drv_hash = %drv_hash, worker_id = %worker_id,
              "infrastructure failure — retry without poison count");
        let Some(state) = self.dag.node_mut(drv_hash) else {
            return;
        };
        if let Err(e) = state.reset_to_ready() {
            warn!(drv_hash = %drv_hash, error = %e,
                  "infrastructure failure: reset_to_ready failed, skipping");
            return;
        }
        // NO insert into failed_workers. NO retry_count++. NO backoff.
        // Infrastructure failures are the worker's problem, not the
        // build's — the build itself never ran far enough to earn a
        // retry penalty. Re-dispatch immediately (P0211's store_degraded
        // check will exclude the worker if it's still broken).
        self.persist_status(drv_hash, DerivationStatus::Ready, None)
            .await;
        self.push_ready(drv_hash.clone());
    }

    pub(super) async fn handle_permanent_failure(
        &mut self,
        drv_hash: &DrvHash,
        error_msg: &str,
        _worker_id: &WorkerId,
    ) {
        let Some(state) = self.dag.node_mut(drv_hash) else {
            return;
        };
        state.ensure_running();
        if let Err(e) = state.transition(DerivationStatus::Poisoned) {
            // Stale PermanentFailure (e.g., drv already Completed on
            // another worker after reassignment). Don't write Poisoned
            // to PG or cascade — in-mem/PG drift + spurious cascade is
            // worse than a missed failure event.
            warn!(drv_hash = %drv_hash, error = %e, current = ?state.status(),
                  "handle_permanent_failure: ->Poisoned transition rejected, skipping");
            return;
        }
        state.poisoned_at = Some(Instant::now());

        self.persist_poisoned(drv_hash).await;
        self.unpin_best_effort(drv_hash).await;

        // Cascade: parents of a poisoned derivation can never complete.
        self.cascade_dependency_failure(drv_hash).await;

        // Propagate failure to interested builds
        let interested_builds = self.get_interested_builds(drv_hash);

        // Flush logs for failed builds too — the failure's log is often the
        // most useful log (compile errors, test output). Do this BEFORE
        // handle_derivation_failure below, which may transition builds to
        // terminal and schedule cleanup.
        self.trigger_log_flush(drv_hash, interested_builds.clone());

        for build_id in interested_builds {
            // Emit failure event
            self.emit_build_event(
                build_id,
                rio_proto::types::build_event::Event::Derivation(
                    rio_proto::types::DerivationEvent {
                        derivation_path: self.drv_path_or_hash_fallback(drv_hash),
                        status: Some(rio_proto::types::derivation_event::Status::Failed(
                            rio_proto::types::DerivationFailed {
                                error_message: error_msg.to_string(),
                                status: rio_proto::types::BuildResultStatus::PermanentFailure
                                    .into(),
                            },
                        )),
                    },
                ),
            );

            self.handle_derivation_failure(build_id, drv_hash).await;
        }
    }

    /// Transitively walk parents of a poisoned derivation and transition all
    /// Queued/Ready/Created ancestors to DependencyFailed.
    ///
    /// Without this, keepGoing builds with a poisoned leaf hang forever:
    /// parents stay Queued, so completed+failed never reaches total.
    pub(super) async fn cascade_dependency_failure(&mut self, poisoned_hash: &DrvHash) {
        let mut to_visit: Vec<DrvHash> = self.dag.get_parents(poisoned_hash);
        let mut visited: HashSet<DrvHash> = HashSet::new();

        while let Some(parent_hash) = to_visit.pop() {
            if !visited.insert(parent_hash.clone()) {
                continue; // already processed
            }

            let Some(state) = self.dag.node_mut(&parent_hash) else {
                continue;
            };

            // Only cascade to derivations that haven't started yet.
            // Assigned/Running derivations will complete or fail on their own
            // (and their completion handler will re-cascade if they succeed
            // but a sibling dep is dead — but actually they'd never become
            // Ready in the first place since all_deps_completed is false).
            // r[impl sched.preempt.never-running]
            if !matches!(
                state.status(),
                DerivationStatus::Queued | DerivationStatus::Ready | DerivationStatus::Created
            ) {
                continue;
            }

            if let Err(e) = state.transition(DerivationStatus::DependencyFailed) {
                warn!(drv_hash = %parent_hash, error = %e, "cascade ->DependencyFailed transition failed");
                continue;
            }

            debug!(
                drv_hash = %parent_hash,
                poisoned_dep = %poisoned_hash,
                "cascaded DependencyFailed from poisoned dependency"
            );

            // Remove from ready queue if present (Ready -> DependencyFailed).
            self.ready_queue.remove(&parent_hash);

            self.persist_status(&parent_hash, DerivationStatus::DependencyFailed, None)
                .await;

            // Continue cascade: this parent's parents also cannot complete.
            to_visit.extend(self.dag.get_parents(&parent_hash));
        }
    }

    pub(super) async fn handle_derivation_failure(&mut self, build_id: Uuid, drv_hash: &DrvHash) {
        // Sync counts from DAG ground truth. The cascade may have transitioned
        // additional parents to DependencyFailed; those must be counted here.
        self.update_build_counts(build_id);

        let Some(build) = self.builds.get_mut(&build_id) else {
            return;
        };

        if !build.keep_going {
            // Fail the entire build immediately
            build.error_summary = Some(format!("derivation {drv_hash} failed"));
            build.failed_derivation = Some(drv_hash.to_string());
            if let Err(e) = self.transition_build_to_failed(build_id).await {
                error!(build_id = %build_id, error = %e, "failed to persist build-failed transition");
            }
        } else {
            // keepGoing: check if all derivations are resolved
            self.check_build_completion(build_id).await;
        }
    }
}
