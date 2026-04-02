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
// r[impl sched.recovery.gate-dispatch]
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
    /// Returns the PG high-water generation on success (for the
    /// caller's monotonicity seed). The caller does `fetch_max`
    /// AFTER the TOCTOU check — keeping this function free of
    /// writes to `self.generation` is load-bearing for the
    /// gen-snapshot check in `handle_leader_acquired` (any change
    /// to `generation` during recovery then unambiguously means
    /// the lease loop fetch_add'd, i.e. a flap).
    ///
    /// Returns Err on PG failure; caller sets `recovery_complete=
    /// true` anyway (see module doc — degrade not block).
    pub(super) async fn recover_from_pg(&mut self) -> Result<Option<i64>, ActorError> {
        info!("starting state recovery from PG");

        // --- Clear in-mem state ---
        // Standby schedulers merge DAGs live (dispatch.rs:12-20
        // early-return on !is_leader means they don't dispatch, but
        // they DO process MergeDag commands — state warm for fast
        // takeover). On acquire, PG is the single source of truth
        // — clear the standby's partial in-mem view, reload.
        //
        // DON'T clear self.executors: those are live connections
        // (ExecutorConnected + Heartbeat), not persisted state. A
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
        for row in drv_rows {
            let derivation_id = row.derivation_id;
            let Ok(status) = row.status.parse::<DerivationStatus>() else {
                warn!(drv_hash = %row.drv_hash, status = %row.status,
                      "unknown derivation status in PG, skipping row");
                continue;
            };
            let state = match DerivationState::from_recovery_row(row, status) {
                Ok(s) => s,
                Err((drv_hash, _)) => {
                    warn!(drv_hash = %drv_hash, "invalid drv_path in PG, skipping row");
                    continue;
                }
            };
            let hash = state.drv_hash.clone();
            id_to_hash.insert(derivation_id, hash.clone());
            self.dag.insert_recovered_node(state);
        }

        // --- Load poisoned derivations (separate query) ---
        // TERMINAL_STATUSES includes "poisoned" so load_nonterminal_
        // derivations skips them. But the TTL check in handle_tick
        // needs them in the DAG with their poisoned_at set. Without
        // this, poison TTL resets on scheduler restart.
        let poisoned_rows = self.db.load_poisoned_derivations().await?;
        if !poisoned_rows.is_empty() {
            info!(
                count = poisoned_rows.len(),
                "loaded poisoned derivations for TTL tracking"
            );
        }
        let ttl_secs = crate::state::POISON_TTL.as_secs_f64();
        for row in poisoned_rows {
            let derivation_id = row.derivation_id;
            // Expired-at-load: clear in PG, don't insert. Avoids the
            // from_poisoned_row Instant-arithmetic trap on fresh
            // nodes — checked_sub(30h) on a node booted 1h ago returns
            // None → unwrap_or(now) → poisoned_at=now → FRESH 24h TTL
            // instead of immediate expiry. PG's elapsed_secs is wall-
            // clock so the comparison here is correct regardless of
            // node uptime.
            //
            // KNOWN GAP (narrow): skipping the id_to_hash insert means
            // an Active build referencing this drv via bd_rows will
            // fall through on the `continue` at the bd_rows join below
            // → build.derivation_hashes shrinks by 1. If other drvs
            // complete: spurious Succeeded with this drv's output
            // missing. Window: scheduler must crash in the poison-
            // before-transition gap AND stay down > POISON_TTL (24h).
            // Normal operation transitions the build to Failed within
            // the same actor turn. Not worth the re-load complexity
            // for a 24h-outage + crash-window intersection.
            if row.elapsed_secs > ttl_secs {
                info!(drv_hash = %row.drv_hash, elapsed_secs = row.elapsed_secs,
                      "poison already past TTL at recovery — clearing");
                let hash: crate::state::DrvHash = row.drv_hash.into();
                if let Err(e) = self.db.clear_poison(&hash).await {
                    warn!(drv_hash = %hash, error = %e,
                          "clear_poison for expired-at-load failed; next recovery will retry");
                }
                continue;
            }
            let state = match DerivationState::from_poisoned_row(row) {
                Ok(s) => s,
                Err((drv_hash, _)) => {
                    warn!(drv_hash = %drv_hash, "invalid poisoned drv_path in PG, skipping");
                    continue;
                }
            };
            let hash = state.drv_hash.clone();
            // r[impl sched.recovery.poisoned-failed-count]
            // Keystone: without this, the bd_rows join below falls through
            // on `continue` for poisoned derivations → build.derivation_hashes
            // stays empty → check_build_completion sees 0/0 → Succeeded.
            id_to_hash.insert(derivation_id, hash);
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
                // Derivation is success-terminal (Completed) OR in a
                // terminal state we don't load (Cancelled,
                // DependencyFailed). Poisoned IS loaded (separate query
                // above) and IS in id_to_hash — if we hit this branch
                // for a poisoned drv, the keystone insert is broken.
                //
                // check_build_completion uses build.derivation_hashes.len()
                // as the denominator. Every drv that falls through here
                // shrinks that denominator. For a build whose ONLY
                // remaining drv is here: total=0, completed=0, failed=0
                // → 0>=0 && 0==0 → spurious Succeeded. The orphan-guard
                // below catches total=0; but a build with ONE Completed
                // drv + ONE fall-through has total=1, completed=1 →
                // Succeeded, and the guard doesn't fire. That case is
                // correct iff the fall-through was genuinely Completed.
                // It is WRONG if the fall-through was Cancelled (Route 2
                // in remediation 01 — needs its own fix).
                warn!(build_id = %build_id, derivation_id = %drv_id,
                      "bd_row derivation not in id_to_hash — success-terminal or unloaded-terminal");
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
            let priority_class = row.priority_class.parse().unwrap_or_else(|_| {
                warn!(build_id = ?row.build_id, priority_class = %row.priority_class,
                      "unknown priority_class, defaulting");
                Default::default()
            });
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

        // --- Fetch PG generation high-water mark (caller seeds) ---
        // NOT applied here: the caller (handle_leader_acquired) does
        // fetch_max AFTER the TOCTOU gen-snapshot check. Writing to
        // self.generation here would false-positive that check.
        let pg_max_gen = self.db.max_assignment_generation().await?;

        // --- Recompute priorities (critical-path sweep) ---
        // est_duration is recomputed from the estimator (build_
        // history was already loaded on first tick). full_sweep
        // does a bottom-up pass: leaves priority=est; parents
        // priority=est+max(children).
        crate::critical_path::full_sweep(&mut self.dag, &self.estimator);

        // --- I-058: recompute Created/Queued initial states ---
        // load_edges_for_derivations only loads edges where BOTH
        // endpoints are non-terminal — an edge to a Completed dep is
        // dropped (correct: completed dep IS satisfied). But a node
        // that was Queued in PG (waiting on that dep) STAYS Queued —
        // nothing transitions it. The push_ready loop below filters
        // on `status() == Ready`, so Queued nodes are never pushed.
        // Any restart with active builds = permanent freeze.
        //
        // compute_initial_states does the same dep-state walk MergeDag
        // uses for fresh nodes: all_deps_completed() → Ready,
        // any_dep_terminally_failed() → DependencyFailed, else Queued.
        // Only Created/Queued are recomputed — Ready was already
        // correct, Assigned/Running are reconcile-assignments' job.
        //
        // I-059: gate on interested_builds. load_nonterminal_derivations
        // has no JOIN to builds — a derivation whose own status is
        // queued/created loads even if every interested build is
        // terminal (failed/cancelled weeks ago). Pre-I-058 those orphans
        // were inert (frozen at Queued). Post-I-058 they'd transition to
        // Ready, dispatch, hit GC'd inputs, infrastructure-fail, poison.
        // The build_derivations join above only populates
        // interested_builds for builds that load_nonterminal_builds
        // returned (status IN pending/active) — empty set = orphan.
        let mut orphans_skipped = 0usize;
        let to_recompute: HashSet<DrvHash> = self
            .dag
            .iter_nodes()
            .filter(|(_, s)| {
                let status_matches = matches!(
                    s.status(),
                    DerivationStatus::Created | DerivationStatus::Queued
                );
                if status_matches && s.interested_builds.is_empty() {
                    orphans_skipped += 1;
                    return false;
                }
                status_matches
            })
            .map(|(h, _)| h.into())
            .collect();
        if orphans_skipped > 0 {
            debug!(
                count = orphans_skipped,
                "recovery: skipping orphan Created/Queued nodes (no active build interested)"
            );
        }
        let initial_states = self.dag.compute_initial_states(&to_recompute);
        let mut transitioned_ready = 0usize;
        for (drv_hash, target) in initial_states {
            let Some(state) = self.dag.node_mut(&drv_hash) else {
                continue;
            };
            let from = state.status();
            // Skip same-status: Queued → Queued is the "still
            // waiting" case (deps also recovered as non-terminal).
            // Non-terminal self-transitions are Err in
            // validate_transition, so the warn! below would noise.
            if from == target {
                continue;
            }
            // Created needs the two-step. Queued goes direct.
            if from == DerivationStatus::Created
                && target != DerivationStatus::Queued
                && let Err(e) = state.transition(DerivationStatus::Queued)
            {
                warn!(drv_hash = %drv_hash, error = %e,
                      "recovery: Created→Queued failed");
                continue;
            }
            if let Err(e) = state.transition(target) {
                warn!(drv_hash = %drv_hash, from = ?from, to = ?target, error = %e,
                      "recovery: initial-state transition failed");
                continue;
            }
            if target == DerivationStatus::Ready {
                transitioned_ready += 1;
            }
        }
        if transitioned_ready > 0 {
            info!(
                count = transitioned_ready,
                "recovery: Queued→Ready transitions (deps completed pre-crash)"
            );
        }

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

        // --- Check for all-complete builds ---
        // A crash between "last drv → Completed" and "build →
        // Succeeded" leaves the build Active in PG with all its
        // derivations terminal. Recovery loads the build (Active)
        // but loads 0 non-terminal derivations for it (all filtered
        // by TERMINAL_STATUSES). Without this sweep,
        // check_build_completion never fires → build stays Active
        // forever.
        //
        // update_build_counts recomputes completed/failed from DAG.
        // For a build with 0 recovered derivations: total=0,
        // completed=0, failed=0 → all_completed (0>=0), failed==0
        // → complete_build(). The build goes Succeeded and the
        // terminal-cleanup timer is scheduled as normal.
        // Track which builds have ZERO build_derivations rows in PG
        // — those are orphans (crash during merge BEFORE persist, or
        // stale rows from a failed rollback). The all-terminal case
        // above has non-empty build_derivations; we just filter them
        // out in load_nonterminal_derivations. bd_rows is the flat
        // list from PG; count per-build to distinguish.
        let mut bd_counts: HashMap<Uuid, usize> = HashMap::new();
        for (build_id, _) in &bd_rows {
            *bd_counts.entry(*build_id).or_insert(0) += 1;
        }

        let build_ids_to_check: Vec<Uuid> = self.builds.keys().copied().collect();
        for build_id in build_ids_to_check {
            // Zero PG links → orphan. Skip completion check.
            // TransitionOutcome::Rejected also guards against
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
            self.update_build_counts(build_id).await;
            self.check_build_completion(build_id).await;
        }

        info!(
            builds = self.builds.len(),
            derivations = self.dag.iter_nodes().count(),
            ready_queue = self.ready_queue.len(),
            "state recovery complete"
        );

        Ok(pg_max_gen)
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
        // Snapshot generation BEFORE recovery. If the lease flaps
        // (lose→reacquire) while recover_from_pg() is running, the
        // lease loop will fetch_add again AND will have cleared
        // recovery_complete (lease/mod.rs lose-transition). An
        // unconditional store(true) here would clobber that clear
        // and let dispatch_ready fire with a DAG loaded under the
        // OLD generation but stamped with the NEW one — worker-side
        // gen fence can't catch that.
        //
        // recover_from_pg() does NOT write to self.generation (it
        // returns the PG high-water for us to seed below). So any
        // change between gen_at_entry and gen_now is unambiguously
        // a lease-loop fetch_add — no false positives from PG seeding.
        let gen_at_entry = self.generation.load(std::sync::atomic::Ordering::Acquire);

        let start = Instant::now();
        let result = self.recover_from_pg().await;

        // Test-only interleave gate: lets a test bump `generation`
        // between recovery completion and the gen re-check below,
        // deterministically proving the TOCTOU fix. Signal "reached"
        // then wait for "release".
        #[cfg(test)]
        if let Some((reached_tx, release_rx)) = self.recovery_toctou_gate.take() {
            let _ = reached_tx.send(());
            let _ = release_rx.await;
        }

        // Record BEFORE the match — both arms need it, and the error
        // arm's partial-state clear (.dag = new(), etc.) doesn't touch
        // `start`. Label by outcome: a 30s failure (PG timeout) and a
        // 30s success (huge DAG) are very different signals; without the
        // label, one washes out the other.
        let outcome = if result.is_ok() { "success" } else { "failure" };
        let elapsed = start.elapsed();
        metrics::histogram!("rio_scheduler_recovery_duration_seconds", "outcome" => outcome)
            .record(elapsed.as_secs_f64());
        metrics::counter!("rio_scheduler_recovery_total", "outcome" => outcome).increment(1);
        info!(elapsed_ms = elapsed.as_millis(), outcome, "recovery timing");

        // TOCTOU re-check: did the lease task fetch_add during
        // recovery? recover_from_pg doesn't touch self.generation,
        // so gen_now != gen_at_entry ⇒ lease loop fetch_add'd ⇒ flap.
        // The re-acquire's LeaderAcquired is already queued in our
        // mpsc — discard this recovery, let the next one re-run.
        let gen_now = self.generation.load(std::sync::atomic::Ordering::Acquire);
        if gen_now != gen_at_entry {
            warn!(
                gen_at_entry,
                gen_now,
                recovery_ok = result.is_ok(),
                "generation changed during recovery \u{2014} lease flapped; \
                 DISCARDING this recovery (next LeaderAcquired will retry)"
            );
            metrics::counter!("rio_scheduler_recovery_total", "outcome" => "discarded_flap")
                .increment(1);
            // Clear the partial state we loaded. The next
            // LeaderAcquired's recover_from_pg() will do this
            // again but do it here too so any Tick that sneaks in
            // before the next LeaderAcquired sees a consistent
            // (empty) DAG.
            self.dag = DerivationDag::new();
            self.ready_queue.clear();
            self.builds.clear();
            self.build_events.clear();
            self.build_sequences.clear();
            // DON'T set recovery_complete — the lease loop's clear
            // at the lose-transition stays in effect. dispatch_ready
            // gates on this (dispatch.rs:27); early-return here
            // makes the post-LeaderAcquired dispatch a no-op.
            return;
        }

        match result {
            Ok(pg_max_gen) => {
                // r[impl sched.recovery.fetch-max-seed]
                // --- Seed generation from PG high-water mark ---
                // fetch_max not store: the lease loop already did
                // fetch_add(1) on this SAME Arc. store would clobber
                // it. fetch_max takes the larger of (lease's value,
                // PG + 1). Defensive against Lease annotation reset
                // (`kubectl delete lease` zeros the annotation; PG's
                // high-water persists). Applied HERE (not inside
                // recover_from_pg) so it happens AFTER the TOCTOU
                // check — this write is deliberate, not a flap.
                if let Some(max_gen) = pg_max_gen {
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

                // Stale-pin cleanup: crash-between-pin-and-unpin
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
                    .store(true, std::sync::atomic::Ordering::SeqCst);
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
                // Explicitly re-clear: recovery may have partially
                // populated before failing.
                self.dag = DerivationDag::new();
                self.ready_queue.clear();
                self.builds.clear();
                self.build_events.clear();
                self.build_sequences.clear();
                self.recovery_complete
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }

    /// Post-recovery worker reconciliation (spec step 6).
    ///
    /// Scheduled via WeakSender delay (~45s = 3× heartbeat + slack)
    /// AFTER recovery. For each Assigned/Running derivation: if
    /// `assigned_executor` NOT in `self.executors` (worker didn't
    /// reconnect after scheduler restart) → query store for output
    /// existence → if all present: Completed (orphan completion
    /// while scheduler was down), else reset_to_ready + retry++.
    ///
    /// Workers ARE in self.executors (survived): they'll send
    /// CompletionReport or heartbeat showing the build still
    /// running — normal handling resumes.
    #[instrument(skip(self))]
    pub(super) async fn handle_reconcile_assignments(&mut self) {
        // Collect (drv_hash, assigned_executor, expected_outputs)
        // for Assigned/Running with unknown workers. Clone before
        // mutating (node_mut borrow).
        //
        // executor_id is Option: Assigned/Running with assigned_executor
        // =None is inconsistent state (shouldn't happen, but recovery
        // loads from PG which could have drifted). We still reconcile
        // it (check store for outputs → Completed, else Ready) rather
        // than silently skipping and leaving it stuck forever.
        let orphaned: Vec<(DrvHash, Option<ExecutorId>, Vec<String>)> = self
            .dag
            .iter_nodes()
            .filter(|(_, s)| {
                matches!(
                    s.status(),
                    DerivationStatus::Assigned | DerivationStatus::Running
                )
            })
            .filter_map(|(h, s)| match s.assigned_executor.as_ref() {
                Some(w) if self.executors.contains_key(w) => {
                    // Worker reconnected. Cross-check running_builds:
                    // if the worker's heartbeat doesn't include this
                    // drv_hash, the assignment is phantom — PG says
                    // Assigned+worker but the worker never got the
                    // message (scheduler crashed between persist_status
                    // and try_send). No running_since → backstop won't
                    // fire → stuck forever without this check.
                    //
                    // Without this check, phantom-Assigned derivations
                    // would be skipped entirely because the worker
                    // reconnected. This cross-check reconciles them.
                    let hash: DrvHash = h.into();
                    if self
                        .executors
                        .get(w)
                        .is_some_and(|ws| ws.running_build.as_ref() == Some(&hash))
                    {
                        // Real assignment — worker has it. Leave it,
                        // completion report will arrive normally.
                        None
                    } else {
                        warn!(drv_hash = %hash, executor_id = %w,
                              "reconcile: worker reconnected but phantom Assigned (not in worker.running_build) — reconciling");
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

        for (drv_hash, executor_id, expected_outputs) in orphaned {
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
                let mut fmp_req = tonic::Request::new(FindMissingPathsRequest {
                    store_paths: expected_outputs.clone(),
                });
                rio_proto::interceptor::inject_current(fmp_req.metadata_mut());
                match client.find_missing_paths(fmp_req).await {
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
                info!(drv_hash = %drv_hash, executor_id = ?executor_id,
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
                    if state.status() == DerivationStatus::Assigned
                        && let Err(e) = state.transition(DerivationStatus::Running)
                    {
                        warn!(drv_hash = %drv_hash, error = %e,
                              "orphan completion Assigned→Running transition failed");
                        continue;
                    }
                    if let Err(e) = state.transition(DerivationStatus::Completed) {
                        warn!(drv_hash = %drv_hash, error = %e,
                              "orphan completion transition failed");
                        continue;
                    }
                    state.output_paths = expected_outputs;
                    state.assigned_executor = None;
                }
                self.persist_status(&drv_hash, DerivationStatus::Completed, None)
                    .await;
                // r[impl sched.gc.path-tenants-upsert]
                // Orphan completion during recovery: derivation was
                // Running at crash, completed during downtime. The
                // normal completion path (handle_success_completion)
                // never fired → no tenant attribution → GC
                // under-retains. output_paths was just set above
                // (= expected_outputs, verified present in store).
                self.upsert_path_tenants_for(&drv_hash).await;
                // Terminal → unpin. Without this, the pins
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
                for ready_hash in newly_ready {
                    if let Some(s) = self.dag.node_mut(&ready_hash)
                        && s.transition(DerivationStatus::Ready).is_ok()
                    {
                        self.persist_status(&ready_hash, DerivationStatus::Ready, None)
                            .await;
                        self.push_ready(ready_hash);
                    }
                }

                // Fire build completion check (same as
                // handle_success_completion does). Without this,
                // if the orphan-completed drv was the LAST
                // outstanding one, the build stays Active forever
                // — no other completion will trigger the check.
                for build_id in &interested {
                    self.update_build_counts(*build_id).await;
                    self.check_build_completion(*build_id).await;
                }
            } else {
                // Worker died mid-build. Record the failure (in-mem
                // + PG) + check poison threshold (same helper as
                // reassign_derivations).
                let should_poison = if let Some(w) = &executor_id {
                    self.record_failure_and_check_poison(&drv_hash, w).await
                } else {
                    self.dag
                        .node(&drv_hash)
                        .map(|s| self.poison_config.is_poisoned(s))
                        .unwrap_or(false)
                };
                if should_poison {
                    info!(drv_hash = %drv_hash, executor_id = ?executor_id,
                          "reconcile: poison threshold reached, poisoning");
                    self.poison_and_cascade(&drv_hash).await;
                    continue;
                }

                info!(drv_hash = %drv_hash, executor_id = ?executor_id,
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
