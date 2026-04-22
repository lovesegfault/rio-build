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
use crate::db::RecoveryBuildRow;
use crate::state::DerivationState;

/// Cross-phase carrier for [`DagActor::recover_from_pg`]: PG row sets
/// loaded by [`DagActor::load_dag_from_rows`] that the later phases
/// (`restore_builds`, `finalize_recovered_builds`) need.
///
/// `id_to_hash` is internal to `load_dag_from_rows` (resolves edge +
/// bd_row UUIDs to hashes) and doesn't cross.
struct RecoveryLoad {
    build_rows: Vec<RecoveryBuildRow>,
    build_ids: Vec<Uuid>,
    /// Flat (build_id, derivation_id) link rows. `restore_builds` only
    /// needs the per-build hash sets (`build_drv_hashes` below);
    /// `finalize_recovered_builds` needs the raw rows again to count
    /// per-build links for the orphan guard.
    bd_rows: Vec<(Uuid, Uuid)>,
    build_drv_hashes: HashMap<Uuid, HashSet<DrvHash>>,
    /// Recovered parents with ≥1 `poisoned`/`dependency_failed`/
    /// `cancelled` child in PG. `seed_ready_queue` short-circuits
    /// these to `DependencyFailed` BEFORE `compute_initial_states`
    /// (which would otherwise see no edge → `all_deps_completed`
    /// → wrong Ready). r[sched.recovery.failed-dep-cascade]
    failed_dep_parents: HashSet<DrvHash>,
}

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
    /// writes to `self.leader`'s generation is load-bearing for the
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
        self.clear_persisted_state();

        let RecoveryLoad {
            build_rows,
            build_ids,
            bd_rows,
            build_drv_hashes,
            failed_dep_parents,
        } = self.load_dag_from_rows().await?;

        self.restore_builds(build_rows, &build_ids, build_drv_hashes)
            .await?;

        // --- Fetch PG generation high-water mark (caller seeds) ---
        // NOT applied here: the caller (handle_leader_acquired) does
        // fetch_max AFTER the TOCTOU gen-snapshot check. Writing to
        // self.leader's generation here would false-positive that check.
        let pg_max_gen = self.db.max_assignment_generation().await?;

        self.seed_ready_queue(&failed_dep_parents).await;

        self.finalize_recovered_builds(&bd_rows).await;

        info!(
            builds = self.builds.len(),
            derivations = self.dag.iter_nodes().count(),
            ready_queue = self.ready_queue.len(),
            "state recovery complete"
        );

        Ok(pg_max_gen)
    }

    /// Load builds + derivations + poisoned + edges + build_derivations
    /// from PG into `self.dag`. Returns the row sets the later phases
    /// need (`restore_builds` for BuildInfo construction;
    /// `finalize_recovered_builds` for the orphan guard).
    async fn load_dag_from_rows(&mut self) -> Result<RecoveryLoad, ActorError> {
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
        // r[impl sched.recovery.failed-dep-cascade]
        // Parents with a terminal-FAILURE dep: edge_rows above drops
        // edges to ALL terminal children, so compute_initial_states
        // would see all_deps_completed()=true and wrongly promote
        // these to Ready. Load the set here (uses id_to_hash, internal
        // to this fn) and pass through RecoveryLoad for seed_ready_
        // queue to short-circuit → DependencyFailed.
        let failed_dep_parents: HashSet<DrvHash> = self
            .db
            .load_parents_with_failed_deps(&drv_ids)
            .await?
            .into_iter()
            .filter_map(|id| id_to_hash.get(&id).cloned())
            .collect();
        if !failed_dep_parents.is_empty() {
            info!(
                count = failed_dep_parents.len(),
                "loaded parents with terminal-failed deps (crash mid-cascade)"
            );
        }
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

        Ok(RecoveryLoad {
            build_rows,
            build_ids,
            bd_rows,
            build_drv_hashes,
            failed_dep_parents,
        })
    }

    /// Reconstruct `BuildInfo` + broadcast channels + `build_sequences`
    /// from the loaded rows. `submitted_at` is reconstructed from PG's
    /// `now() - submitted_at` (so `r[sched.timeout.per-build]` survives
    /// failover); total/completed/cached counts are seeded from PG
    /// denorm columns (I-111).
    async fn restore_builds(
        &mut self,
        build_rows: Vec<RecoveryBuildRow>,
        build_ids: &[Uuid],
        mut build_drv_hashes: HashMap<Uuid, HashSet<DrvHash>>,
    ) -> Result<(), ActorError> {
        // --- Build BuildInfo + broadcast channels ---
        for row in build_rows {
            let Ok(state) = BuildState::parse_db(&row.status) else {
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

            // BuildInfo::new_pending then transition.
            // completed_count/failed_count reset to 0 —
            // check_build_completion recomputes from DAG status on the
            // next completion (relative to derivation_hashes, which is
            // also DAG-relative here), so the completion check is
            // self-healing.
            //
            // I-111: total_count/recovered_completed/cached_count are
            // SEEDED from PG below — `hashes` only contains drvs that
            // were non-terminal at recovery, so new_pending's
            // `total_count = hashes.len()` would be the *remaining*
            // count, and update_build_counts would persist that back to
            // builds.total_drvs (1111/1555 → 0/443 on restart). The DB
            // is authoritative for these denorm columns.
            let mut info = BuildInfo::new_pending(
                row.build_id,
                row.tenant_id,
                priority_class,
                row.keep_going,
                options,
                hashes,
            );
            info.total_count = row.total_drvs as u32;
            info.recovered_completed = row.completed_drvs as u32;
            info.cached_count = row.cached_drvs as u32;
            // Seed submitted_at from PG so r[sched.timeout.per-build]
            // and rio_scheduler_build_duration_seconds survive failover
            // (otherwise each failover grants a fresh full
            // build_timeout window, contradicting "wall-clock since
            // submission"). PG-side elapsed → Instant reconstruction,
            // same as PoisonedDerivationRow.
            info.submitted_at = Instant::now()
                .checked_sub(std::time::Duration::from_secs_f64(
                    row.submitted_age_secs.max(0.0),
                ))
                .unwrap_or_else(Instant::now);
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
            // since_sequence reads from PG). The returned
            // `broadcast::Receiver` is intentionally dropped:
            // recovery itself doesn't subscribe, it only needs the
            // channel to EXIST so emit_build_event has a sender and
            // late WatchBuild calls can `events.subscribe(id)`.
            drop(self.events.register(row.build_id));
            self.builds.insert(row.build_id, info);
        }

        // --- Seed event-bus sequences from event_log high-water marks ---
        let seq_rows = self.db.max_sequence_per_build(build_ids).await?;
        for (build_id, max_seq) in seq_rows {
            // i64 → u64: sequences are always positive.
            self.events.sequences.insert(build_id, max_seq as u64);
        }

        Ok(())
    }

    /// Critical-path sweep + I-058 Created/Queued recompute, then push
    /// every Ready node into `self.ready_queue`. Reads only from
    /// `self.dag` (already populated by `load_dag_from_rows`).
    ///
    /// `failed_dep_parents`: short-circuited to `DependencyFailed`
    /// BEFORE `compute_initial_states` — see
    /// `r[sched.recovery.failed-dep-cascade]`.
    async fn seed_ready_queue(&mut self, failed_dep_parents: &HashSet<DrvHash>) {
        // --- Recompute priorities (critical-path sweep) ---
        // est_duration is recomputed from the SLA cache (refreshed on
        // first tick). full_sweep does a bottom-up pass: leaves
        // priority=est; parents priority=est+max(children).
        crate::critical_path::full_sweep(&mut self.dag, &self.sla_estimator, &self.builds);

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
                // Substituting included: the spawned task is gone after
                // restart, so re-derive Ready/Queued via the same dep-
                // walk and let the next dispatch-time batch re-probe.
                // r[impl sched.substitute.detached]
                let status_matches = matches!(
                    s.status(),
                    DerivationStatus::Created
                        | DerivationStatus::Queued
                        | DerivationStatus::Substituting
                );
                if status_matches && s.interested_builds.is_empty() {
                    orphans_skipped += 1;
                    return false;
                }
                status_matches
            })
            .map(|(h, _)| h.into())
            .collect();
        // r[impl sched.recovery.failed-dep-cascade]
        // Partition: parents whose dep is poisoned/dependency_failed/
        // cancelled in PG go directly to DependencyFailed and are
        // EXCLUDED from compute_initial_states. Without this, the
        // edge to the failed child was dropped by
        // load_edges_for_derivations → all_deps_completed()=true →
        // wrong Ready → dispatch against missing input →
        // InfrastructureFailure → wasted retries → wrong-reason Poisoned.
        // Realistic trigger: crash mid-cascade_dependency_failure
        // (sequential per-parent persist_status awaits).
        //
        // I-059 orphan gate also applies here: a parent with no
        // active interested build is left at PG status (it's not in
        // `to_recompute`, so the intersection below skips it).
        let mut cascade_failed: Vec<DrvHash> = Vec::new();
        let to_recompute: HashSet<DrvHash> = to_recompute
            .into_iter()
            .filter(|h| {
                if failed_dep_parents.contains(h) {
                    cascade_failed.push(h.clone());
                    false
                } else {
                    true
                }
            })
            .collect();
        for hash in &cascade_failed {
            let Some(state) = self.dag.node_mut(hash) else {
                continue;
            };
            // Created/Queued → DependencyFailed direct;
            // Substituting → Queued → DependencyFailed (no direct arm
            // in validate_transition).
            if state.status() == DerivationStatus::Substituting
                && let Err(e) = state.transition(DerivationStatus::Queued)
            {
                warn!(drv_hash = %hash, error = %e,
                      "recovery: Substituting→Queued (failed-dep) failed");
                continue;
            }
            if let Err(e) = state.transition(DerivationStatus::DependencyFailed) {
                warn!(drv_hash = %hash, error = %e,
                      "recovery: →DependencyFailed (failed-dep) failed");
                continue;
            }
            // Persist: otherwise PG stays 'queued', the build goes
            // terminal, the build_derivations link is GC'd, and the
            // derivation row leaks (status non-terminal, no link).
            self.persist_status(hash, DerivationStatus::DependencyFailed, None)
                .await;
        }
        if !cascade_failed.is_empty() {
            info!(
                count = cascade_failed.len(),
                "recovery: →DependencyFailed (dep was terminal-failed in PG; crash mid-cascade)"
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
            // Created and Substituting need the two-step (both have a
            // valid →Queued edge but no direct →DependencyFailed).
            // Queued goes direct.
            if matches!(
                from,
                DerivationStatus::Created | DerivationStatus::Substituting
            ) && target != DerivationStatus::Queued
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
            if target == DerivationStatus::DependencyFailed {
                // Same as cascade_failed (500-504): without this, PG
                // stays 'queued', gc_orphan_terminal_derivations
                // (filters status IN TERMINAL) never reaps the row
                // after the build link is GC'd → permanent leak.
                // Reaches here for depth-≥2 ancestors (immediate
                // parents handled above by cascade_failed).
                self.persist_status(&drv_hash, DerivationStatus::DependencyFailed, None)
                    .await;
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
        // Push all Ready-status derivations with at least one active
        // interested build. push_ready computes the priority-heap key
        // from state.priority (just set by full_sweep above).
        // I-059 also applies here: an already-Ready node whose every
        // build is terminal (orphan-Ready, produced e.g. by
        // check_build_completion's recovery !keep_going branch which
        // does not cancel sibling derivations) bypasses the recompute-
        // set guard above and would otherwise dispatch into GC'd
        // inputs → infra-fail → spurious poison.
        let ready: Vec<DrvHash> = self
            .dag
            .iter_nodes()
            .filter(|(_, s)| {
                if s.status() != DerivationStatus::Ready {
                    return false;
                }
                if s.interested_builds.is_empty() {
                    orphans_skipped += 1;
                    return false;
                }
                true
            })
            .map(|(h, _)| h.into())
            .collect();
        if orphans_skipped > 0 {
            debug!(
                count = orphans_skipped,
                "recovery: skipping orphan Created/Queued/Ready nodes (no active build interested)"
            );
        }
        for hash in ready {
            self.push_ready(hash);
        }
    }

    /// Per-build completion sweep + orphan guard. A crash between
    /// "last drv → Completed" and "build → Succeeded" leaves the
    /// build Active with all derivations terminal; this fires
    /// `check_build_completion` for it. A crash mid-merge BEFORE
    /// `persist_merge_to_db` leaves an Active build with ZERO
    /// `build_derivations` rows; this skips it (orphan guard) so it
    /// doesn't emit a spurious BuildCompleted with empty outputs.
    async fn finalize_recovered_builds(&mut self, bd_rows: &[(Uuid, Uuid)]) {
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
        for (build_id, _) in bd_rows {
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
            // Reconstruct sticky had_failure (build.rs:461).
            // update_build_counts just set failed_count from the DAG
            // (which includes Poisoned/DepFailed per
            // r[sched.recovery.poisoned-failed-count]). Without this,
            // a later ClearPoison/TTL removes the node →
            // failed_count=0 → keep_going build spuriously Succeeds.
            // error_summary is the sticky; failed_count is not.
            if let Some(b) = self.builds.get_mut(&build_id)
                && b.failed_count > 0
                && b.error_summary.is_none()
            {
                b.error_summary = Some(format!(
                    "recovered with {} failed derivation(s)",
                    b.failed_count
                ));
            }
            self.check_build_completion(build_id).await;
        }
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
    /// Lose-transition counterpart to [`Self::handle_leader_acquired`].
    /// The lease loop has already flipped `is_leader=false` via
    /// `on_lose()`; this clears the actor's stale in-memory view so a
    /// long-lived standby doesn't hold the previous leadership's DAG.
    /// `handle_tick` early-returns on `!is_leader`, so housekeeping
    /// can't act on the stale state in the gap before this command
    /// lands — but holding it indefinitely is wasted memory and would
    /// be wrong if any future code path forgets the gate.
    ///
    /// Also zeros the leader-only state gauges (one-shot). A fresh
    /// standby never sets them; a was-leader-now-standby would
    /// otherwise export its frozen last-tick values forever (the
    /// `tick_publish_gauges` call is unreachable on standby via the
    /// `handle_tick` gate). Prometheus then sees two series per
    /// gauge until this pod restarts.
    // r[impl sched.lease.standby-tick-noop]
    // r[impl obs.metric.scheduler-leader-gate+2]
    pub(super) fn handle_leader_lost(&mut self) {
        info!("leader lost: clearing persisted actor state");
        self.clear_persisted_state();
        // workers_active is NOT zeroed: `executors` is retained
        // (clear_persisted_state keeps live connections), and
        // ExecutorDisconnected is not leader-gated — the inc/dec path
        // (executor.rs) maintains the gauge on standby. Zeroing it
        // here desyncs from N retained entries; each subsequent
        // disconnect would decrement to −1…−N. Workers rebalancing to
        // the new leader drain it naturally.
        for g in [
            "rio_scheduler_derivations_queued",
            "rio_scheduler_builds_active",
            "rio_scheduler_derivations_running",
        ] {
            metrics::gauge!(g).set(0.0);
        }
    }

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
        // recover_from_pg() does NOT write to self.leader's generation (it
        // returns the PG high-water for us to seed below). So any
        // change between gen_at_entry and gen_now is unambiguously
        // a lease-loop fetch_add — no false positives from PG seeding.
        let gen_at_entry = self.leader.generation();

        let start = Instant::now();
        let result = self.recover_from_pg().await;

        // Stale-pin cleanup: crash-between-pin-and-unpin (scheduler
        // crashed after dispatch pin but before completion unpin)
        // leaves rows in scheduler_live_pins for terminal drvs.
        // Sweep them — safe to remove (drv is terminal, inputs no
        // longer in-use). Best-effort; grace period is fallback.
        // Runs BEFORE the gen re-check so a lease flap during this
        // await is caught at the TOCTOU gate below — this is the
        // second async PG round-trip in handle_leader_acquired and
        // must be inside the gen_at_entry window. The DELETE is
        // DB-side terminal-status based, independent of `result`.
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
        // Stale-assignment cleanup: crash-between-derivations-UPDATE-
        // and-assignments-UPDATE (pre-tx-wrap binaries) left rows with
        // derivation terminal but assignment pending → permanently
        // un-GC-able (I-209 leak class). Best-effort backstop; the
        // tx-wrap chokepoint is the structural fix going forward.
        // DB-side terminal-status based, independent of `result` —
        // same TOCTOU-window placement as sweep_stale_live_pins above.
        // r[impl sched.db.assignment-stale-sweep]
        match self.db.sweep_stale_assignments().await {
            Ok(n) if n > 0 => {
                info!(
                    swept = n,
                    "swept stale assignments (terminal derivation, pending assignment)"
                );
            }
            Ok(_) => {}
            Err(e) => {
                warn!(error = %e, "failed to sweep stale assignments (best-effort)");
            }
        }

        // Test-only interleave gate: lets a test bump `generation`
        // between the awaits above and the gen re-check below,
        // deterministically proving the TOCTOU fix covers BOTH
        // recover_from_pg AND the sweep_stale_* calls. Signal "reached"
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
        // recovery? recover_from_pg doesn't touch self.leader's
        // generation, so gen_now != gen_at_entry ⇒ lease flap.
        // The re-acquire's LeaderAcquired is already queued in our
        // mpsc — discard this recovery, let the next one re-run.
        let gen_now = self.leader.generation();
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
            self.clear_persisted_state();
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
                    let prev = self.leader.seed_generation_from(target);
                    if target > prev {
                        info!(
                            prev_gen = prev,
                            pg_high_water = max_gen,
                            new_gen = target,
                            "seeded generation from PG high-water mark (defensive monotonicity)"
                        );
                    }
                }
                self.leader.set_recovery_complete();
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
                self.clear_persisted_state();
                self.leader.set_recovery_complete();
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
        // r[impl sched.reconcile.leader-gate]
        // Mirror dispatch_ready: the 45s timer is fire-and-forget and
        // on_lose doesn't cancel it or clear self.dag, so an ex-leader's
        // timer would otherwise write to PG against a stale DAG.
        if !self.leader.is_leader() {
            debug!("reconcile: not leader, skipping");
            return;
        }
        let orphaned = self.collect_orphaned_assignments();

        if orphaned.is_empty() {
            debug!("reconcile: all assigned/running derivations have live workers");
            return;
        }
        info!(
            count = orphaned.len(),
            "reconciling orphaned assignments (worker didn't reconnect)"
        );

        for (drv_hash, executor_id, expected_outputs) in orphaned {
            // Query store: did the build complete while the scheduler
            // was down (orphan completion)? Else the worker died
            // mid-build → reset to Ready for retry.
            if self
                .outputs_present_in_store(&drv_hash, &expected_outputs)
                .await
            {
                self.adopt_orphan_completion(&drv_hash, &executor_id, expected_outputs)
                    .await;
            } else {
                self.reset_orphan_to_ready(&drv_hash, &executor_id).await;
            }
        }

        // After reconcile, dispatch anything newly Ready.
        self.dispatch_ready().await;
    }

    /// Collect `(drv_hash, assigned_executor, expected_outputs)` for
    /// every Assigned/Running derivation whose worker is no longer
    /// live — the liveness-check input set for
    /// [`handle_reconcile_assignments`](Self::handle_reconcile_assignments).
    ///
    /// Cloned out of the DAG before any mutation (the per-row
    /// reset/adopt path takes `node_mut`).
    ///
    /// `executor_id` is `Option`: Assigned/Running with
    /// `assigned_executor=None` is inconsistent state (shouldn't
    /// happen, but recovery loads from PG which could have drifted).
    /// We still reconcile it (check store for outputs → Completed,
    /// else Ready) rather than silently skipping and leaving it stuck
    /// forever.
    fn collect_orphaned_assignments(&self) -> Vec<(DrvHash, Option<ExecutorId>, Vec<String>)> {
        self.dag
            .iter_nodes()
            .filter(|(_, s)| {
                matches!(
                    s.status(),
                    DerivationStatus::Assigned | DerivationStatus::Running
                )
            })
            .filter_map(|(h, s)| match s.assigned_executor.as_ref() {
                Some(w) if self.executors.get(w).is_some_and(|ws| ws.is_registered()) => {
                    // Worker reconnected AND heartbeated — running_build
                    // is authoritative. Cross-check: if the worker's
                    // heartbeat doesn't include this drv_hash, the
                    // assignment is phantom — PG says Assigned+worker
                    // but the worker never got the message (scheduler
                    // crashed between persist_status and try_send). No
                    // running_since → backstop won't fire → stuck
                    // forever without this check.
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
                // Stream connected but no heartbeat yet — running_build
                // is NOT authoritative (None until executor.rs writes it
                // from the first accepted heartbeat; I-048b drops
                // pre-stream heartbeats). Defer: the heartbeat path's
                // two-strike confirmed_phantoms will reconcile real
                // phantoms once heartbeats flow. A worker that stream-
                // connects but never heartbeats has is_registered()=false
                // → has_capacity()=false → never dispatched-to; if its
                // stream later drops, handle_worker_disconnected
                // reassigns. No worse than spurious failure_count++.
                Some(w) if self.executors.contains_key(w) => {
                    debug!(drv_hash = ?h, executor_id = %w,
                           "reconcile: worker stream-connected but not yet heartbeated — deferring");
                    None
                }
                Some(w) => Some((h.into(), Some(w.clone()), s.expected_output_paths.clone())),
                None => {
                    warn!(drv_hash = ?h, status = ?s.status(),
                          "reconcile: Assigned/Running drv with NULL worker — reconciling anyway");
                    Some((h.into(), None, s.expected_output_paths.clone()))
                }
            })
            .collect()
    }

    /// `FindMissingPaths` against `expected_outputs`: store returns
    /// paths NOT in its index, so empty response = all present.
    /// Conservative `false` for empty `expected_outputs` (can't verify
    /// orphan completion), no store client (tests), or RPC error.
    async fn outputs_present_in_store(
        &mut self,
        drv_hash: &DrvHash,
        expected_outputs: &[String],
    ) -> bool {
        // Floating-CA carries `[""]` pre-completion (translate.rs
        // convention) — filter so a CA orphan doesn't send `""` to
        // FMP and get a misleading InvalidArgument warn. Empty after
        // filter ⇒ nothing verifiable ⇒ conservative `false`.
        let store_paths: Vec<String> = expected_outputs
            .iter()
            .filter(|p| !p.is_empty())
            .cloned()
            .collect();
        if store_paths.is_empty() {
            return false;
        }
        let Some(client) = &mut self.store_client else {
            return false;
        };
        let mut fmp_req = tonic::Request::new(FindMissingPathsRequest { store_paths });
        rio_proto::interceptor::inject_current(fmp_req.metadata_mut());
        // Per-orphan in `handle_reconcile_assignments`' loop — without
        // the timeout, a dead store stalls reconcile unbounded × N
        // orphans; without the credit, even a bounded 30s × N reaps
        // every executor that DID reconnect.
        let grpc_timeout = self.grpc_timeout;
        let fmp_start = Instant::now();
        let r = match tokio::time::timeout(grpc_timeout, client.find_missing_paths(fmp_req)).await {
            Ok(Ok(resp)) => resp.into_inner().missing_paths.is_empty(),
            Ok(Err(e)) => {
                warn!(drv_hash = %drv_hash, error = %e,
                      "reconcile: FindMissingPaths failed, assuming incomplete");
                false
            }
            Err(_) => {
                warn!(drv_hash = %drv_hash, timeout = ?grpc_timeout,
                      "reconcile: FindMissingPaths timed out, assuming incomplete");
                false
            }
        };
        self.credit_heartbeats_for_stall(fmp_start.elapsed());
        r
    }

    /// Reconcile path for an orphaned assignment whose outputs ARE in
    /// the store: the build completed while the scheduler was down.
    /// Transition Completed, persist, attribute tenants, unpin, then
    /// reuse [`release_downstream`](Self::release_downstream) for the
    /// newly-ready cascade + per-build completion check. Skips the
    /// `handle_success_completion` steps that need worker-result data
    /// (build_samples, CA bookkeeping, ancestor priorities — full_sweep
    /// on next tick handles the latter).
    async fn adopt_orphan_completion(
        &mut self,
        drv_hash: &DrvHash,
        executor_id: &Option<ExecutorId>,
        expected_outputs: Vec<String>,
    ) {
        info!(drv_hash = %drv_hash, executor_id = ?executor_id,
              "reconcile: orphan completion (outputs found in store)");
        let interested = self.get_interested_builds(drv_hash);

        if let Some(state) = self.dag.node_mut(drv_hash) {
            state.ensure_running();
            if let Err(e) = state.transition(DerivationStatus::Completed) {
                warn!(drv_hash = %drv_hash, error = %e,
                      "orphan completion transition failed");
                return;
            }
            state.output_paths = expected_outputs;
            state.assigned_executor = None;
        }
        self.persist_status(drv_hash, DerivationStatus::Completed, None)
            .await;
        // r[impl sched.event.derivation-terminal]
        // Orphan completion is worker-built (not cached) — emit
        // DerivationCompleted so WatchBuild clients see it finish.
        // `release_downstream` below does NOT emit per-drv events
        // (only progress + build-level); the analogue in
        // `handle_success_completion` emits before calling it.
        let output_paths = self
            .dag
            .node(drv_hash)
            .map(|s| s.output_paths.clone())
            .unwrap_or_default();
        let drv_path = self.dag.path_or_hash_fallback(drv_hash);
        for build_id in &interested {
            self.events.emit(
                *build_id,
                rio_proto::types::build_event::Event::Derivation(
                    rio_proto::types::DerivationEvent::completed(
                        drv_path.clone(),
                        output_paths.clone(),
                    ),
                ),
            );
        }
        // r[impl sched.gc.path-tenants-upsert]
        // Orphan completion during recovery: derivation was
        // Running at crash, completed during downtime. The
        // normal completion path (handle_success_completion)
        // never fired → no tenant attribution → GC
        // under-retains. output_paths was just set above
        // (= expected_outputs, verified present in store).
        self.upsert_path_tenants_for(drv_hash).await;
        // Terminal → unpin. sweep_stale_live_pins ran BEFORE
        // reconcile (the drv was Assigned/Running in PG then —
        // kept), so it won't catch this one.
        self.unpin_best_effort(drv_hash).await;
        self.release_downstream(drv_hash, &interested, HashSet::new())
            .await;
    }

    /// Reconcile path for an orphaned assignment whose outputs are NOT
    /// in the store: worker died mid-build (or phantom: never received
    /// the assignment). Same semantics as
    /// [`reassign_derivations`](Self::reassign_derivations): an
    /// orphaned assignment is an infrastructure event, not a derivation
    /// failure — re-read existing poison state only, do NOT record a
    /// failure or bump `retry.count`. `executor_id` is kept for logging
    /// parity with `reassign_derivations(.., lost_worker)`.
    // r[impl sched.reassign.no-promote-on-ephemeral-disconnect+4]
    async fn reset_orphan_to_ready(
        &mut self,
        drv_hash: &DrvHash,
        executor_id: &Option<ExecutorId>,
    ) {
        // Re-read existing poison state so 3 prior REAL failures
        // (recorded by handle_transient_failure) + this disconnect
        // → poison instead of dispatching a 4th time. The orphan
        // event itself never increments the count.
        let should_poison = self
            .dag
            .node(drv_hash)
            .map(|s| self.poison_config.is_poisoned(s))
            .unwrap_or(false);
        if should_poison {
            info!(drv_hash = %drv_hash, executor_id = ?executor_id,
                  "reconcile: poison threshold reached, poisoning");
            self.poison_and_cascade(
                drv_hash,
                "poison threshold reached on recovery (orphan worker did not reconnect)",
            )
            .await;
            return;
        }

        info!(drv_hash = %drv_hash, executor_id = ?executor_id,
              "reconcile: worker didn't reconnect, resetting to Ready");
        if let Some(state) = self.dag.node_mut(drv_hash) {
            if let Err(e) = state.reset_to_ready() {
                warn!(drv_hash = %drv_hash, error = %e, "reset_to_ready failed");
                return;
            }
            self.push_ready(drv_hash.clone());
        }
        self.persist_status(drv_hash, DerivationStatus::Ready, None)
            .await;
    }

    /// Schedule reconciliation ~45s out via WeakSender. Same pattern as
    /// `schedule_terminal_cleanup`. Workers have ~45s (3× heartbeat +
    /// slack) to reconnect after scheduler restart. Any
    /// Assigned/Running derivation whose worker DIDN'T reconnect by
    /// then gets reconciled (Completed if outputs in store, else reset).
    pub(super) fn schedule_reconcile_timer(&self) {
        let Some(weak_tx) = self.self_tx.clone() else {
            return;
        };
        rio_common::task::spawn_monitored("reconcile-timer", async move {
            tokio::time::sleep(RECONCILE_DELAY).await;
            if let Some(tx) = weak_tx.upgrade()
                && tx.try_send(ActorCommand::ReconcileAssignments).is_err()
            {
                tracing::warn!("reconcile command dropped (channel full)");
                metrics::counter!("rio_scheduler_reconcile_dropped_total").increment(1);
            }
        });
    }
}

/// Delay before post-recovery worker reconciliation. Workers have
/// this long to reconnect after scheduler restart; after that, any
/// Assigned/Running derivation with an unknown worker is reconciled
/// (Completed if outputs in store, else reset to Ready).
///
/// 45s = 3× HEARTBEAT_INTERVAL (10s) + 15s slack. A worker that's
/// alive should reconnect within one heartbeat; 3× covers network
/// blips. Same cfg(test) shadow pattern as POISON_TTL.
#[cfg(not(test))]
const RECONCILE_DELAY: std::time::Duration = std::time::Duration::from_secs(45);
#[cfg(test)]
const RECONCILE_DELAY: std::time::Duration = std::time::Duration::from_millis(100);
