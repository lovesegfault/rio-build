//! State recovery on leader acquisition.
//!
//! When a standby scheduler acquires the lease (previous leader died
//! or was demoted), it calls `recover_from_pg()` to rebuild its
//! in-mem DAG from PG. Without this, the new leader starts with an
//! EMPTY DAG — all in-flight builds are lost, clients see "unknown
//! build" on WatchBuild.
//!
//! # Design: non-blocking lease renewal
//!
//! Recovery for a large DAG (10k derivations) may take several
//! seconds (PG roundtrips + critical-path sweep). If the lease loop
//! WAITED for recovery, the lease could expire (15s TTL) → another
//! replica acquires → dual-leader. Instead:
//!
//! 1. Lease loop on acquire: `fetch_add(1)` + `is_leader.store(true)`
//!    IMMEDIATELY, then fire-and-forget `ActorCommand::LeaderAcquired`
//!    (no reply). Lease loop continues renewing normally.
//! 2. Actor handles `LeaderAcquired`: calls `recover_from_pg().await`
//!    → on success `recovery_complete.store(true)`.
//! 3. `dispatch_ready` gates on BOTH `is_leader` AND `recovery_complete`.
//!    Standby merges DAGs (state warm) but doesn't dispatch until
//!    recovery is done.
//!
//! # Failure mode: degrade, don't block
//!
//! If recovery fails (PG down mid-recovery), log error + metric but
//! still set `recovery_complete=true` — with an EMPTY DAG. Same as
//! Phase 3a behavior (lost builds, not blocked cluster). A panicked
//! recovery with `is_leader=true` + `recovery_complete=false` would
//! block dispatch FOREVER while holding the lease — standby can
//! never take over. Degrading is better.
//!
//! # Generation seeding
//!
//! Seed from `MAX(generation) FROM assignments` + 1 via `fetch_max`
//! (not `store`) — the lease loop already did `fetch_add(1)` on the
//! SAME `Arc<AtomicU64>`. `store` would clobber that. `fetch_max`
//! takes the larger of (lease's post-increment value, PG high-water
//! mark + 1). Defensive against Lease reset (`kubectl delete lease`
//! resets the annotation; PG's high-water mark persists).

use super::*;
use crate::state::DerivationState;

impl DagActor {
    /// Rebuild DAG + build state from PG. Called by LeaderAcquired.
    ///
    /// Clears in-mem state first (standby may have merged DAGs
    /// live), then loads from PG as the single source of truth.
    /// Priorities recomputed via critical_path::full_sweep.
    ///
    /// Returns Err on PG failure; caller sets `recovery_complete=
    /// true` anyway (see module doc — degrade not block).
    pub(super) async fn recover_from_pg(&mut self) -> Result<(), ActorError> {
        let start = Instant::now();
        info!("starting state recovery from PG");

        // --- Clear in-mem state ---
        // Standby schedulers merge DAGs live (dispatch.rs:12-20
        // early-return on !is_leader means they don't dispatch, but
        // they DO process MergeDag commands — state warm for fast
        // takeover). On acquire, PG is the single source of truth
        // — clear the standby's partial in-mem view, reload.
        //
        // DON'T clear self.workers: those are live connections
        // (WorkerConnected + Heartbeat), not persisted state. A
        // worker connected to the standby is still connected.
        self.dag = DerivationDag::new();
        self.ready_queue.clear();
        self.builds.clear();
        self.build_events.clear();
        self.build_sequences.clear();

        // --- Load builds ---
        let build_rows = self.db.load_nonterminal_builds().await?;
        let build_ids: Vec<Uuid> = build_rows.iter().map(|r| r.build_id).collect();
        info!(count = build_rows.len(), "loaded non-terminal builds");

        // --- Load derivations ---
        let drv_rows = self.db.load_nonterminal_derivations().await?;
        info!(count = drv_rows.len(), "loaded non-terminal derivations");

        // Build db_id → drv_hash map for edge + build_derivation
        // resolution below. Also build DerivationState nodes.
        let mut id_to_hash: HashMap<Uuid, DrvHash> = HashMap::with_capacity(drv_rows.len());
        for row in &drv_rows {
            let Ok(status) = row.status.parse::<DerivationStatus>() else {
                warn!(drv_hash = %row.drv_hash, status = %row.status,
                      "unknown derivation status in PG, skipping row");
                continue;
            };
            let Ok(state) = DerivationState::from_recovery_row(row, status) else {
                warn!(drv_hash = %row.drv_hash, "invalid drv_path in PG, skipping row");
                continue;
            };
            let hash = state.drv_hash.clone();
            id_to_hash.insert(row.derivation_id, hash.clone());
            self.dag.insert_recovered_node(state);
        }

        // --- Load edges + add to DAG ---
        let drv_ids: Vec<Uuid> = id_to_hash.keys().copied().collect();
        let edge_rows = self.db.load_edges_for_derivations(&drv_ids).await?;
        for (parent_id, child_id) in &edge_rows {
            // Both must be in id_to_hash (query filters on ANY($1)
            // for both endpoints) but be defensive.
            if let (Some(parent), Some(child)) =
                (id_to_hash.get(parent_id), id_to_hash.get(child_id))
            {
                self.dag
                    .insert_recovered_edge(parent.clone(), child.clone());
            }
        }
        info!(count = edge_rows.len(), "loaded edges");

        // --- Load build_derivations + rebuild interested_builds ---
        let bd_rows = self.db.load_build_derivations(&build_ids).await?;
        // Also accumulate derivation_hashes per build (for BuildInfo).
        let mut build_drv_hashes: HashMap<Uuid, HashSet<DrvHash>> = HashMap::new();
        for (build_id, drv_id) in &bd_rows {
            let Some(hash) = id_to_hash.get(drv_id) else {
                // Derivation is terminal (not in our non-terminal
                // load). That's fine — it doesn't need interested_
                // builds tracking (it's done). BuildInfo's
                // derivation_hashes set will be smaller than it was,
                // but check_build_completion only counts non-
                // terminal derivations anyway.
                continue;
            };
            if let Some(state) = self.dag.node_mut(hash) {
                state.interested_builds.insert(*build_id);
            }
            build_drv_hashes
                .entry(*build_id)
                .or_default()
                .insert(hash.clone());
        }

        // --- Build BuildInfo + broadcast channels ---
        for row in build_rows {
            let Ok(state) = row.status.parse::<BuildState>() else {
                warn!(build_id = %row.build_id, status = %row.status,
                      "unknown build status in PG, skipping");
                continue;
            };
            let priority_class = row.priority_class.parse().unwrap_or_default();
            let options = row.options_json.map(|j| j.0).unwrap_or_default();
            let hashes = build_drv_hashes.remove(&row.build_id).unwrap_or_default();

            // BuildInfo::new_pending then transition. Lossy:
            // submitted_at resets to now() (can't restore Instant).
            // completed_count/cached_count/failed_count reset to 0
            // — check_build_completion recomputes from DAG status
            // on the next completion, so this is self-healing.
            let mut info = BuildInfo::new_pending(
                row.build_id,
                row.tenant_id,
                priority_class,
                row.keep_going,
                options,
                hashes,
            );
            // Transition to current state (Pending → Active if the
            // row says active). new_pending starts at Pending.
            if state == BuildState::Active
                && let Err(e) = info.transition(BuildState::Active)
            {
                // Shouldn't happen (Pending → Active is valid) but
                // log + continue.
                warn!(build_id = %row.build_id, error = %e,
                      "recovered build transition failed");
            }

            // Fresh broadcast channel. Late WatchBuild subscribers
            // get new events from here; events before recovery are
            // served from build_event_log replay (WatchBuild with
            // since_sequence reads from PG).
            let (tx, _) = broadcast::channel(BUILD_EVENT_BUFFER_SIZE);
            self.build_events.insert(row.build_id, tx);
            self.builds.insert(row.build_id, info);
        }

        // --- Seed build_sequences from event_log high-water marks ---
        let seq_rows = self.db.max_sequence_per_build(&build_ids).await?;
        for (build_id, max_seq) in seq_rows {
            // i64 → u64: sequences are always positive.
            self.build_sequences.insert(build_id, max_seq as u64);
        }

        // --- Seed generation from assignments high-water mark ---
        // fetch_max not store: the lease loop already did
        // fetch_add(1) on this SAME Arc. store would clobber it.
        // fetch_max takes the larger of (lease's value, PG + 1).
        // Defensive against Lease annotation reset.
        if let Some(max_gen) = self.db.max_assignment_generation().await? {
            let target = (max_gen as u64).saturating_add(1);
            let prev = self
                .generation
                .fetch_max(target, std::sync::atomic::Ordering::Release);
            if target > prev {
                info!(
                    prev_gen = prev,
                    pg_high_water = max_gen,
                    new_gen = target,
                    "seeded generation from PG high-water mark (defensive monotonicity)"
                );
            }
        }

        // --- Recompute priorities (critical-path sweep) ---
        // est_duration is recomputed from the estimator (build_
        // history was already loaded on first tick). full_sweep
        // does a bottom-up pass: leaves priority=est; parents
        // priority=est+max(children).
        crate::critical_path::full_sweep(&mut self.dag, &self.estimator);

        // --- Populate ready queue ---
        // Push all Ready-status derivations. push_ready computes
        // the priority-heap key from state.priority (just set by
        // full_sweep above).
        let ready: Vec<DrvHash> = self
            .dag
            .iter_nodes()
            .filter(|(_, s)| s.status() == DerivationStatus::Ready)
            .map(|(h, _)| h.into())
            .collect();
        for hash in ready {
            self.push_ready(hash);
        }

        // --- Check for all-complete builds (X5 fix) ---
        // A crash between "last drv → Completed" and "build →
        // Succeeded" leaves the build Active in PG with all its
        // derivations terminal. Recovery loads the build (Active)
        // but loads 0 non-terminal derivations for it (all filtered
        // by db.rs:537). Without this sweep, check_build_completion
        // never fires → build stays Active forever.
        //
        // update_build_counts recomputes completed/failed from DAG.
        // For a build with 0 recovered derivations: total=0,
        // completed=0, failed=0 → all_completed (0>=0), failed==0
        // → complete_build(). The build goes Succeeded and the
        // terminal-cleanup timer is scheduled as normal.
        // Round 4 Z16: track which builds have ZERO build_derivations
        // rows in PG — those are orphans (Z1 pre-fix victims or crash
        // during merge BEFORE persist). The X5 case (all derivations
        // terminal) has non-empty build_derivations; we just filter
        // them out at :122. bd_rows is the flat list from PG; count
        // per-build to distinguish.
        let mut bd_counts: HashMap<Uuid, usize> = HashMap::new();
        for (build_id, _) in &bd_rows {
            *bd_counts.entry(*build_id).or_insert(0) += 1;
        }

        let build_ids_to_check: Vec<Uuid> = self.builds.keys().copied().collect();
        for build_id in build_ids_to_check {
            // Z16: zero PG links → orphan. Skip completion check.
            // Z4 (TransitionOutcome::Rejected) also guards against
            // spurious events for already-terminal builds, but this
            // catches Active orphans that would emit a spurious
            // BuildCompleted with empty output_paths.
            if bd_counts.get(&build_id).copied().unwrap_or(0) == 0 {
                warn!(
                    build_id = %build_id,
                    "recovery: Active build with ZERO build_derivations rows in PG — orphan, skipping"
                );
                continue;
            }
            self.update_build_counts(build_id);
            self.check_build_completion(build_id).await;
        }

        let elapsed = start.elapsed();
        info!(
            builds = self.builds.len(),
            derivations = self.dag.iter_nodes().count(),
            ready_queue = self.ready_queue.len(),
            elapsed_ms = elapsed.as_millis(),
            "state recovery complete"
        );
        metrics::histogram!("rio_scheduler_recovery_duration_seconds")
            .record(elapsed.as_secs_f64());
        metrics::counter!("rio_scheduler_recovery_total", "outcome" => "success").increment(1);

        Ok(())
    }

    /// Handle `LeaderAcquired`: run recovery, then set the
    /// completion flag (even on error — see module doc).
    ///
    /// The lease loop fire-and-forgets this; no reply channel.
    /// Recovery runs in the actor's command loop — blocks other
    /// commands until done. That's CORRECT: we don't want to
    /// dispatch a build while half-recovered (the DAG would be
    /// inconsistent). MergeDag from a standby-period SubmitBuild
    /// would queue in the mpsc channel and get processed after.
    pub(super) async fn handle_leader_acquired(&mut self) {
        match self.recover_from_pg().await {
            Ok(()) => {
                // X9 stale-pin cleanup: crash-between-pin-and-unpin
                // (scheduler crashed after dispatch pin but before
                // completion unpin) leaves rows in scheduler_live_
                // pins for terminal drvs. Sweep them — they're
                // safe to remove (drv is terminal, inputs no longer
                // in-use). Best-effort; grace period is fallback.
                match self.db.sweep_stale_live_pins().await {
                    Ok(n) if n > 0 => {
                        info!(
                            swept = n,
                            "swept stale scheduler_live_pins (crash recovery)"
                        );
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!(error = %e, "failed to sweep stale live pins (best-effort)");
                    }
                }
                self.recovery_complete
                    .store(true, std::sync::atomic::Ordering::Release);
            }
            Err(e) => {
                // PG failure mid-recovery. Set complete=true
                // ANYWAY with EMPTY DAG. Same as Phase 3a behavior
                // (builds lost, cluster keeps working). Better than
                // blocking dispatch forever while holding the lease.
                error!(
                    error = %e,
                    "state recovery FAILED — continuing with empty DAG \
                     (Phase 3a behavior: in-flight builds lost)"
                );
                metrics::counter!("rio_scheduler_recovery_total", "outcome" => "failure")
                    .increment(1);
                // Explicitly re-clear: recovery may have partially
                // populated before failing.
                self.dag = DerivationDag::new();
                self.ready_queue.clear();
                self.builds.clear();
                self.build_events.clear();
                self.build_sequences.clear();
                self.recovery_complete
                    .store(true, std::sync::atomic::Ordering::Release);
            }
        }
    }

    /// Post-recovery worker reconciliation (spec step 6).
    ///
    /// Scheduled via WeakSender delay (~45s = 3× heartbeat + slack)
    /// AFTER recovery. For each Assigned/Running derivation: if
    /// `assigned_worker` NOT in `self.workers` (worker didn't
    /// reconnect after scheduler restart) → query store for output
    /// existence → if all present: Completed (orphan completion
    /// while scheduler was down), else reset_to_ready + retry++.
    ///
    /// Workers ARE in self.workers (survived): they'll send
    /// CompletionReport or heartbeat showing the build still
    /// running — normal handling resumes.
    pub(super) async fn handle_reconcile_assignments(&mut self) {
        // Collect (drv_hash, assigned_worker, expected_outputs)
        // for Assigned/Running with unknown workers. Clone before
        // mutating (node_mut borrow).
        //
        // worker_id is Option: Assigned/Running with assigned_worker
        // =None is inconsistent state (shouldn't happen, but recovery
        // loads from PG which could have drifted). We still reconcile
        // it (check store for outputs → Completed, else Ready) rather
        // than silently skipping and leaving it stuck forever.
        let orphaned: Vec<(DrvHash, Option<WorkerId>, Vec<String>)> = self
            .dag
            .iter_nodes()
            .filter(|(_, s)| {
                matches!(
                    s.status(),
                    DerivationStatus::Assigned | DerivationStatus::Running
                )
            })
            .filter_map(|(h, s)| match s.assigned_worker.as_ref() {
                Some(w) if self.workers.contains_key(w) => {
                    // Worker reconnected. Cross-check running_builds:
                    // if the worker's heartbeat doesn't include this
                    // drv_hash, the assignment is phantom — PG says
                    // Assigned+worker but the worker never got the
                    // message (scheduler crashed between persist_status
                    // and try_send). No running_since → backstop won't
                    // fire → stuck forever without this check.
                    //
                    // Round 4 Z3: before this check, phantom-Assigned
                    // derivations were skipped entirely because the
                    // worker reconnected. Now we reconcile them.
                    let hash: DrvHash = h.into();
                    if self
                        .workers
                        .get(w)
                        .is_some_and(|ws| ws.running_builds.contains(&hash))
                    {
                        // Real assignment — worker has it. Leave it,
                        // completion report will arrive normally.
                        None
                    } else {
                        warn!(drv_hash = %hash, worker_id = %w,
                              "reconcile: worker reconnected but phantom Assigned (not in worker.running_builds) — reconciling");
                        Some((hash, Some(w.clone()), s.expected_output_paths.clone()))
                    }
                }
                Some(w) => Some((h.into(), Some(w.clone()), s.expected_output_paths.clone())),
                None => {
                    warn!(drv_hash = ?h, status = ?s.status(),
                          "reconcile: Assigned/Running drv with NULL worker — reconciling anyway");
                    Some((h.into(), None, s.expected_output_paths.clone()))
                }
            })
            .collect();

        if orphaned.is_empty() {
            debug!("reconcile: all assigned/running derivations have live workers");
            return;
        }
        info!(
            count = orphaned.len(),
            "reconciling orphaned assignments (worker didn't reconnect)"
        );

        for (drv_hash, worker_id, expected_outputs) in orphaned {
            // Query store: are all outputs present? If so, the
            // build completed while the scheduler was down —
            // transition Completed (orphan completion). Else, the
            // worker died mid-build → reset to Ready for retry.
            //
            // FindMissingPaths: store returns paths NOT in its
            // index. Empty response = all present.
            let all_present = if expected_outputs.is_empty() {
                // No expected outputs = can't verify orphan
                // completion. Conservative: treat as incomplete.
                false
            } else if let Some(client) = &mut self.store_client {
                match client
                    .find_missing_paths(FindMissingPathsRequest {
                        store_paths: expected_outputs.clone(),
                    })
                    .await
                {
                    Ok(resp) => resp.into_inner().missing_paths.is_empty(),
                    Err(e) => {
                        warn!(drv_hash = %drv_hash, error = %e,
                              "reconcile: FindMissingPaths failed, assuming incomplete");
                        false
                    }
                }
            } else {
                // No store client (tests). Conservative.
                false
            };

            if all_present {
                // Orphan completion: transition Completed.
                info!(drv_hash = %drv_hash, worker_id = ?worker_id,
                      "reconcile: orphan completion (outputs found in store)");
                // Capture interested_builds BEFORE transitioning —
                // check_build_completion (below) needs to know which
                // builds care. Must read before node_mut (borrow).
                let interested: Vec<Uuid> = self
                    .dag
                    .node(&drv_hash)
                    .map(|s| s.interested_builds.iter().copied().collect())
                    .unwrap_or_default();

                if let Some(state) = self.dag.node_mut(&drv_hash) {
                    // Assigned → Running first if needed (state
                    // machine doesn't allow Assigned → Completed).
                    if state.status() == DerivationStatus::Assigned {
                        let _ = state.transition(DerivationStatus::Running);
                    }
                    if let Err(e) = state.transition(DerivationStatus::Completed) {
                        warn!(drv_hash = %drv_hash, error = %e,
                              "orphan completion transition failed");
                        continue;
                    }
                    state.output_paths = expected_outputs;
                    state.assigned_worker = None;
                }
                self.persist_status(&drv_hash, DerivationStatus::Completed, None)
                    .await;
                // Y2: terminal → unpin. Without this, the pins
                // (written at original dispatch before the crash)
                // leak until next restart's sweep_stale_live_pins.
                // sweep_stale_live_pins ran BEFORE reconcile (the
                // drv was Assigned/Running in PG then — kept), so
                // it won't catch this one.
                self.unpin_best_effort(&drv_hash).await;
                // Release parents (same find_newly_ready pattern as
                // handle_success_completion). Partial re-implementation:
                // DON'T emit BuildEvent (the build is recovered,
                // subscribers are gone), DON'T update build_history
                // (no timing data for an orphan), DON'T update
                // ancestor priorities (full_sweep on next tick does
                // it anyway).
                let newly_ready = self.dag.find_newly_ready(&drv_hash);
                for ready_hash in &newly_ready {
                    if let Some(s) = self.dag.node_mut(ready_hash)
                        && s.transition(DerivationStatus::Ready).is_ok()
                    {
                        self.persist_status(ready_hash, DerivationStatus::Ready, None)
                            .await;
                        self.push_ready(ready_hash.clone());
                    }
                }

                // Fire build completion check (same as
                // handle_success_completion does). Without this,
                // if the orphan-completed drv was the LAST
                // outstanding one, the build stays Active forever
                // — no other completion will trigger the check.
                for build_id in &interested {
                    self.update_build_counts(*build_id);
                    self.check_build_completion(*build_id).await;
                }
            } else {
                // Worker died mid-build. Record the failure (in-mem
                // + PG) + check poison threshold (X6 fix — same
                // helper as reassign_derivations).
                let should_poison = if let Some(w) = &worker_id {
                    self.record_failure_and_check_poison(&drv_hash, w).await
                } else {
                    self.dag
                        .node(&drv_hash)
                        .map(|s| s.failed_workers.len() >= POISON_THRESHOLD)
                        .unwrap_or(false)
                };
                if should_poison {
                    info!(drv_hash = %drv_hash, worker_id = ?worker_id,
                          "reconcile: POISON_THRESHOLD reached, poisoning");
                    self.poison_and_cascade(&drv_hash).await;
                    continue;
                }

                info!(drv_hash = %drv_hash, worker_id = ?worker_id,
                      "reconcile: worker didn't reconnect, resetting to Ready");
                if let Some(state) = self.dag.node_mut(&drv_hash) {
                    if let Err(e) = state.reset_to_ready() {
                        warn!(drv_hash = %drv_hash, error = %e, "reset_to_ready failed");
                        continue;
                    }
                    state.retry_count += 1;
                    self.push_ready(drv_hash.clone());
                }
                if let Err(e) = self.db.increment_retry_count(&drv_hash).await {
                    error!(drv_hash = %drv_hash, error = %e, "failed to persist retry++");
                }
                self.persist_status(&drv_hash, DerivationStatus::Ready, None)
                    .await;
            }
        }

        // After reconcile, dispatch anything newly Ready.
        self.dispatch_ready().await;
    }
}
