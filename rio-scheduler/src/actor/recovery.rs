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
//! WAITED for recovery, a standby would steal after `STEAL_AFTER`
//! (19s) of observed staleness while this replica still believes it
//! leads → dual-leader. Instead:
//!
//! 1. Lease loop on acquire: derive the generation from the Lease's
//!    transition count (`fetch_max`) + `is_leader.store(true)`
//!    IMMEDIATELY, then fire-and-forget `ActorCommand::LeaderAcquired`
//!    (no reply). Lease loop continues renewing normally.
//! 2. Actor handles `LeaderAcquired`: calls `recover_from_pg().await`
//!    (the DAG load), then reads the PG generation floor as an
//!    independent step, claims the resulting target, and records the
//!    completion for the acquire-epoch (transition count) the
//!    recovery was computed under
//!    (`LeaderState::set_recovery_complete`).
//! 3. `dispatch_ready` gates on BOTH `is_leader` AND `recovery_complete`.
//!    Standby merges DAGs (state warm) but doesn't dispatch until
//!    recovery is done.
// r[impl sched.recovery.gate-dispatch]
//!
//! # Failure mode: degrade, don't block
//!
//! If the DAG load fails (PG hiccup mid-recovery), log error + metric
//! and continue with an EMPTY DAG — but the term is still floored,
//! claimed, and (when required) confirmed before `recovery_complete`
//! is set, so its dispatches stay inside what the durable floor
//! covers (only the in-flight builds are lost). Only when the PG
//! floor itself cannot be read does the term skip the claim attempt
//! entirely and complete — after the post-claim leadership
//! confirmation, which needs no PG — at the recovery-entry generation
//! (the floor-unreadable fallback — see the disposition comment in
//! `handle_leader_acquired`). A panicked
//! recovery with `is_leader=true` + `recovery_complete=false` would
//! block dispatch FOREVER while holding the lease — standby can
//! never take over. Degrading is better.
//!
//! # Generation seeding and the write-ahead claim
//!
//! The PRIMARY generation source is the Lease's transition count (the
//! lease loop's `fetch_max` in `on_acquire` — the apiserver bumps
//! `leaseTransitions` atomically with the holder change). The PG floor
//! read here, `GREATEST(MAX(assignments.generation), MAX(claims))` + 1
//! via `fetch_max` (not `store` — both writers only ever raise the
//! SAME `Arc<AtomicU64>`), is the durable backstop that survives the
//! Lease object being deleted and recreated at `leaseTransitions = 0`.
//!
//! Before `recovery_complete` ungates dispatch, the target generation
//! is durably CLAIMED in `leader_generation_claims` on the
//! non-degraded path. The exceptions: the floor-unreadable fallback
//! (no claim is possible when PG cannot answer even the single-row
//! floor query) and the claim-INSERT-failure / conflict-exhaustion
//! arms, which proceed unclaimed — see the bump-confirm pricing and
//! the claim-target disposition in `handle_leader_acquired`.
//! Without the claim, a leader deposed before persisting a single
//! assignment leaves no trace in PG, and its post-deletion successor
//! seeds from the same stale floor and reuses its generation. The
//! claim bounds the post-deletion damage; it cannot prevent it under
//! a PG point-in-time restore (which regresses both floor arms
//! together).
//!
//! A claim target the durable floor cannot vouch for — one ABOVE the
//! entry generation, a retained entry generation more than one above
//! the floor, or a retained entry generation whose floor could not be
//! read at all — is additionally seeded (and the recovery completed)
//! only after a post-claim apiserver round-trip that ended with this
//! replica holding the Lease (`sched.recovery.bump-confirm`): the PG floor
//! cannot distinguish a dead predecessor's claim from a live
//! successor's, nor can it rule out a live successor inside a
//! never-claimed gap, so a deposed-but-unaware leader whose recovery
//! outlives its deposal would otherwise complete above the live leader
//! and invert the executor fence. Unconfirmed such recoveries are
//! discarded and re-run by the next acquire edge.

use std::time::Duration;

use super::*;
use crate::db::RecoveryBuildRow;
use crate::state::{DerivationState, verifiable_wanted_paths};

/// How long a recovery whose claim target the durable floor cannot
/// vouch for (a bump target, or a retained entry generation over a
/// non-adjacent floor) waits for the lease loop to complete
/// a post-claim Leading round before discarding
/// (`sched.recovery.bump-confirm`). Derived from the lease constants:
/// the backstop only has to outlive one self-fence detection (at most
/// [`crate::lease::SELF_FENCE_AFTER`] plus one
/// [`crate::lease::RENEW_INTERVAL`] of fence-check latency); the second
/// `RENEW_INTERVAL` is slack. This cap is a pure backstop for a
/// wedged-but-believing lease loop — the normal exits are a
/// confirmation within about one renew interval, or a lose/self-fence
/// within the fence deadline.
const BUMP_CONFIRMATION_CAP: Duration = Duration::from_secs(
    crate::lease::SELF_FENCE_AFTER.as_secs() + 2 * crate::lease::RENEW_INTERVAL.as_secs(),
);

/// Poll cadence of
/// [`DagActor::await_post_claim_leadership_confirmation`]: the lease
/// loop publishes confirmations at renew-tick granularity (seconds), so
/// a 100ms poll adds negligible latency while keeping the actor task
/// responsive to the early-exit conditions.
const CONFIRMATION_POLL_INTERVAL: Duration = Duration::from_millis(100);

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
    /// `cancelled` child in PG that a live co-owning build vouches
    /// for. `seed_ready_queue` short-circuits these to
    /// `DependencyFailed` BEFORE `compute_initial_states` (which would
    /// otherwise see no edge → `all_deps_completed` → wrong Ready).
    /// r[sched.recovery.failed-dep-cascade+2]
    failed_dep_parents: HashSet<DrvHash>,
}

impl DagActor {
    /// Rebuild DAG + build state from PG. Called by LeaderAcquired.
    ///
    /// Clears in-mem state first (standby may have merged DAGs
    /// live), then loads from PG as the single source of truth.
    /// Priorities recomputed via critical_path::full_sweep.
    ///
    /// The PG generation floor is NOT read here: the caller
    /// (`handle_leader_acquired`) reads it as an independent step, so
    /// a DAG-load failure cannot take the floor — and the claim and
    /// confirmation built on it — down with it. Keeping this function
    /// free of writes to `self.leader` (generation,
    /// acquired_transitions, is_leader) is load-bearing for the
    /// snapshot check in `handle_leader_acquired` (any change to the
    /// generation OR to the recorded acquire-transitions, or a cleared
    /// is_leader, during recovery then unambiguously means the lease
    /// loop wrote it, i.e. a flap onto a new epoch or a lapsed lease).
    ///
    /// Returns Err on PG failure; the caller still floors, claims, and
    /// (when required) confirms before completing (see module doc —
    /// degrade not block; only the in-flight builds are lost).
    pub(super) async fn recover_from_pg(&mut self) -> Result<(), ActorError> {
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

        // Test-only: deterministic DAG-load failure. Scoped to the load
        // phase — the caller's independent PG-floor read must stay
        // unaffected so the load-failure tests can exercise the floored
        // degraded path.
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_recovery_load) {
            return Err(ActorError::Database(sqlx::Error::PoolClosed));
        }

        let RecoveryLoad {
            build_rows,
            build_ids,
            bd_rows,
            build_drv_hashes,
            failed_dep_parents,
        } = self.load_dag_from_rows().await?;

        self.restore_builds(build_rows, &build_ids, build_drv_hashes)
            .await?;

        self.seed_ready_queue(&failed_dep_parents).await;

        self.finalize_recovered_builds(&bd_rows).await;

        info!(
            builds = self.builds.len(),
            derivations = self.dag.iter_nodes().count(),
            ready_queue = self.ready_queue.len(),
            "state recovery complete"
        );

        Ok(())
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
        // Rows restored with `topdown_pruned = true` and NO restored
        // closure-hole breadcrumb: candidates for the produced-children
        // gate run after the edge load below (the recovered in-memory
        // DAG alone cannot decide it — see that block's comment). Holed
        // rows are never enrolled — see the push site.
        let mut flagged: Vec<Uuid> = Vec::new();
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
            // r[impl sched.merge.substitute-topdown+10]
            // The recovery-time consult of the persisted closure-hole
            // breadcrumb (`migrations/064`): a holed flagged parent is
            // never enrolled as a clear candidate, however produced its
            // surviving persisted children look — an un-produced child
            // was reaped out from under it (and its row may since have
            // been GC'd), so that evidence is a truncated view of the
            // pruned closure. Mirrors the in-memory predicate every
            // other clear site judges through the closure classifier
            // (a holed node is never Vouched). The kept mark routes the
            // node to the dispatch-time carve-out / resubmit-directing
            // fail-fast instead.
            if state.topdown_pruned && !state.closure_hole {
                flagged.push(derivation_id);
            }
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
        // r[impl sched.recovery.failed-dep-cascade+2]
        // Parents with a terminal-FAILURE dep: edge_rows above drops
        // edges to expired-at-load poisoned / dependency_failed /
        // cancelled children (within-TTL poisoned children were
        // re-inserted above and keep their edges), so for the dropped
        // ones compute_initial_states would see
        // all_deps_completed()=true and wrongly promote these to
        // Ready. Load the set here (uses id_to_hash, internal to this
        // fn) and pass through RecoveryLoad for seed_ready_queue to
        // short-circuit → DependencyFailed.
        //
        // Evidence rule (shared with the produced-children gate below,
        // in the failing direction here): a child's terminal failure
        // counts against a recovered parent only when a LIVE build that
        // also owns the parent vouches for that child — another build's
        // dead/cancelled, never-wanted child must not condemn a healthy
        // build's parent (bug_009). Parents excluded by the rule are not
        // condemned: they keep whatever non-terminal children survive
        // the edge load (possibly none) and are re-discovered at
        // dispatch time; the closure-hole stamp below records the
        // dropped un-produced children so later children-keyed verdicts
        // never trust the truncated set.
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

        // r[impl sched.merge.substitute-topdown+10]
        // Produced-children gate on restored `topdown_pruned` marks:
        // drop the mark from any flagged row whose persisted children
        // are ALL produced (`completed`/`skipped`) and vouched for by
        // a still-live build. The check MUST be
        // PG-side: produced children are excluded from
        // load_nonterminal_derivations and their edges were dropped
        // above, so in the recovered in-memory DAG such a parent is
        // indistinguishable from a genuine childless pruned root —
        // which must KEEP its flag (its closure was never merged; a
        // from-source dispatch would ENOENT). Unbuilt / failed /
        // cancelled / poisoned / dependency_failed children fail the
        // query's bool_and, so the must-substitute guard is also kept
        // for them. Running here — before seed_ready_queue and before
        // any dispatch (dispatch gates on recovery_complete) — means no
        // dispatch-time probe can ever observe the stale flag and
        // wrongly fail-fast a build whose closure IS produced. This
        // also absorbs the PG-true/memory-false skew left behind when a
        // best-effort clear (merge-, completion-, or fail-fast-time)
        // lost its PG write: the next failover lands here and the row
        // is re-evaluated against the same produced-children criterion.
        //
        // The produced evidence is additionally scoped to children
        // vouched for by a LIVE ('pending'/'active') build that also
        // owns the parent (the same evidence rule the failed-dep
        // cascade above applies in the failing direction) — the
        // recovery mirror of the merge-time decision to clear only
        // after verify_preexisting_completed: a PG 'completed' row
        // vouched for only by long-terminal builds is stale evidence
        // (the previous-generation re-request shape — the store may
        // have GC'd those outputs long ago), a row vouched for only by
        // builds that never owned the parent is cross-build evidence
        // (a pruning build links only its kept roots, so only a
        // full-merge owner can vouch), and neither must launder a
        // clear. The gate's purpose is absorbing live-flow skew (the
        // lost best-effort clears above), and in those flows the
        // produced children are linked to the still-live build that
        // owns the parent too; parents whose only produced evidence is
        // historical or cross-build keep the restored mark and the
        // bounded fail-fast handles them instead. Rows restored with
        // the closure-hole breadcrumb never reach this gate at all
        // (vetoed at candidate collection above): their persisted child
        // set is reap-truncated, so even live-vouched produced
        // survivors must not clear the mark.
        if !flagged.is_empty() {
            let produced_parents = self
                .db
                .load_parents_with_all_children_produced(&flagged)
                .await?;
            let mut cleared: Vec<String> = Vec::new();
            for parent_id in produced_parents {
                let Some(hash) = id_to_hash.get(&parent_id) else {
                    continue;
                };
                if let Some(state) = self.dag.node_mut(hash) {
                    state.topdown_pruned = false;
                    cleared.push(hash.to_string());
                }
            }
            if !cleared.is_empty() {
                info!(
                    count = cleared.len(),
                    "recovery: dropped restored topdown_pruned marks (persisted children all produced under live builds)"
                );
                // Best-effort persisted clear so later failovers don't
                // re-evaluate the same rows; the in-memory clear above
                // is what this tenure's correctness relies on.
                if let Err(e) = self.db.clear_topdown_pruned_by_hashes(&cleared).await {
                    warn!(count = cleared.len(), error = %e,
                          "failed to clear persisted topdown_pruned at recovery (best-effort; \
                           next failover re-evaluates)");
                }
            }
        }

        // r[impl sched.merge.substitute-topdown+10]
        // Closure-hole stamp for the edges this load dropped to
        // UN-PRODUCED terminal children (`poisoned`/`dependency_failed`/
        // `cancelled`): the recovery-side analogue of the reap, which
        // breadcrumbs every removal of an un-produced child from a
        // parent's child set. The edge load above dropped those edges,
        // so the parent recovers with a silently truncated child set
        // (NOT necessarily childless — non-terminal siblings keep their
        // edges). The produced-children gate's refusal above is not
        // enough on its own: its evidence never reaches the in-memory
        // model, so when a surviving sibling later completes, the
        // completion-time clear would judge the truncated set Vouched
        // and launder the restored mark into a doomed from-source
        // dispatch (the un-produced child's subtree was never built).
        // Setting the breadcrumb keeps every children-keyed verdict at
        // Broken instead — inert for unmarked nodes, exactly like the
        // reap-time stamp. Disjoint from the gate's clear set by
        // construction: a parent with ≥1 non-produced terminal child can
        // never satisfy the gate's bool_and.
        let unproduced_dropped = self
            .db
            .load_parents_with_unproduced_terminal_children(&drv_ids)
            .await?;
        let mut holed: Vec<String> = Vec::new();
        for parent_id in unproduced_dropped {
            let Some(hash) = id_to_hash.get(&parent_id) else {
                continue;
            };
            if let Some(state) = self.dag.node_mut(hash) {
                state.closure_hole = true;
                holed.push(hash.to_string());
            }
        }
        if !holed.is_empty() {
            info!(
                count = holed.len(),
                "recovery: recorded closure holes for parents whose un-produced terminal children were dropped"
            );
            // Best-effort durable counterpart (this fn runs only on the
            // just-acquired leader) so the breadcrumb survives later
            // failovers even after the orphan-terminal GC deletes the
            // dropped child's row; the in-memory stamp above is what
            // this tenure's correctness relies on. A lost write means
            // the next failover re-derives the hole from the same
            // persisted children — or misses it if those rows are GC'd
            // first, the already-accepted best-effort window.
            if let Err(e) = self.db.set_closure_hole_by_hashes(&holed).await {
                warn!(count = holed.len(), error = %e,
                      "failed to persist closure holes at recovery (best-effort; \
                       in-memory breadcrumb covers this tenure)");
            }
        }

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
                // r[impl sched.substitute.detached+5]
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
        // r[impl sched.recovery.failed-dep-cascade+2]
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

    /// Handle `LeaderLost`: invalidate any kept recovery completion,
    /// then wipe the actor's persisted in-memory state and zero the
    /// leader-only gauges. Lose-transition counterpart to
    /// [`Self::handle_leader_acquired`].
    ///
    /// The lease loop fire-and-forgets this; no reply channel. On a
    /// real loss it has already flipped `is_leader=false` via
    /// `on_lose()`; on a same-tick false alarm (lose then re-acquire)
    /// `is_leader` is already true again by the time this lands —
    /// either way the completion stamp must not outlive the DAG it
    /// certified, so it is cleared here before the wipe. Clearing the
    /// stale in-memory view also means a long-lived standby doesn't
    /// hold the previous leadership's DAG. `handle_tick` early-returns
    /// on `!is_leader`, so housekeeping can't act on the stale state
    /// in the gap before this command lands — but holding it
    /// indefinitely is wasted memory and would be wrong if any future
    /// code path forgets the gate.
    ///
    /// Also zeros the leader-only state gauges (one-shot). A fresh
    /// standby never sets them; a was-leader-now-standby would
    /// otherwise export its frozen last-tick values forever (the
    /// `tick_publish_gauges` call is unreachable on standby via the
    /// `handle_tick` gate). Prometheus then sees two series per
    /// gauge until this pod restarts.
    // r[impl sched.lease.standby-tick-noop+2]
    // r[impl obs.metric.scheduler-leader-gate+2]
    pub(super) fn handle_leader_lost(&mut self) {
        // A same-count re-acquire's kept recovery may have re-stamped
        // the completion after `on_lose` cleared it (the same-epoch
        // keep). The state that completion certified is about to be
        // wiped, so the stamp goes with it — `dispatch_ready` and
        // `advertised()` must not treat the wiped DAG as recovered. The
        // `LeaderAcquired` queued behind this command (hook order is
        // preserved by the forwarder) re-runs recovery and re-stamps;
        // on a real loss `is_leader` is already false and the next
        // acquire re-runs recovery anyway — so this can never gate
        // dispatch permanently.
        self.leader.invalidate_recovery_completion();
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

    /// Handle `LeaderAcquired`: snapshot the flap-detection signals,
    /// run state recovery from PG, then run the claim /
    /// bump-confirmation loop and the recovery TOCTOU gate, recording
    /// the epoch-keyed completion (`set_recovery_complete` with the
    /// transition count snapshotted at recovery entry) only when the
    /// gate passes — completion is deliberately withheld on the
    /// discard paths (lose-/rebound-flap, lapsed leadership, discarded
    /// unconfirmed bump). See the inline comments for the details.
    ///
    /// The lease loop fire-and-forgets this; no reply channel.
    /// Recovery runs in the actor's command loop — it blocks other
    /// commands until done. That's CORRECT: we don't want to dispatch
    /// a build while half-recovered (the DAG would be inconsistent).
    /// MergeDag from a standby-period SubmitBuild would queue in the
    /// mpsc channel and get processed after.
    pub(super) async fn handle_leader_acquired(&mut self) {
        // This tenure has not (re)proven its DAG against PG yet.
        // Redundant on the single-threaded actor — recover_from_pg's
        // first statement is clear_persisted_state(), which clears the
        // bit — but it makes the acquisition handler's own contract
        // explicit: only the Ok arm below may set it. (The
        // pre-LeaderAcquired gap is closed by the PREVIOUS tenure's
        // LeaderLost-time clear, not by anything in this fn.)
        self.dag_authoritative = false;

        // Nudge `interrupt_housekeeping` so its lease-acquire
        // edge-reload of `cost_table` (and the `cost_was_leader`
        // false→true store) runs promptly. Without this, the gate on
        // `handle_ack_spawned_intents`' `observe_instance_types` write
        // stays closed for up to 600s (the housekeeping tick) and the
        // controller's edge-detected instance-type observations during
        // that window are dropped. `Notify::notified()` is
        // permit-based, so this fire-and-forget won't be lost if
        // housekeeping isn't yet at its select.
        self.cost_reload_notify.notify_one();

        // Snapshot the flap-detection signals BEFORE recovery. If the
        // lease flaps (lose→reacquire WITH a holder change in between)
        // while recover_from_pg() is running, the lease loop will have
        // cleared the recovery completion (`LeaderState::on_lose` in
        // rio-lease; `on_rebound` is the second mid-recovery clearer,
        // for holder changes observed late on a still-leading round).
        // The completion at the end of this function is keyed to
        // `transitions_at_entry` (recovery_complete() compares that
        // stamp against the CURRENTLY recorded acquired_transitions),
        // so a holder change that lands at ANY point — before the gate
        // below, or in the window between the gate's loads and the
        // completion call (the lease loop runs on its own thread; no
        // actor-side await is needed for its writes to interleave) —
        // leaves the stamp referring to an epoch that is no longer the
        // recorded one, and dispatch stays gated. What the stamp cannot
        // distinguish is an epoch that left and came back to the SAME
        // count: the count-coincidence ABA, priced in the residual note
        // below — the same residual the gate itself accepts. The gate
        // is still what discards this recovery's WORK (the loaded DAG,
        // the claim) when a flap is detected, so the next
        // LeaderAcquired re-runs it instead of leaving a stale-epoch
        // DAG behind a permanently-false completion.
        //
        // Three signals, re-checked at the gate below:
        //
        // - `acquired_transitions`: the holder-change signal. The lease
        //   loop records the Lease's transition count at every acquire
        //   edge and at every rebound, and the apiserver bumps that
        //   count atomically with every holder change, so a foreign
        //   term that lands inside our recovery window moves the
        //   recorded value the gate compares — whether the loop saw it
        //   through a lose/acquire edge pair or only late, as a rebound
        //   (the one exception is the count-coincidence ABA priced in
        //   the residual note below). The generation alone cannot serve
        //   here: once the PG-floor seed has saturated the generation
        //   above `leaseTransitions + 1` (the permanent state after a
        //   `kubectl delete lease`), on_acquire's fetch_max is a
        //   generation no-op on every subsequent holder change.
        // - `is_leader` (re-read at the gate, no snapshot needed): a
        //   false there means a lose landed mid-recovery and we have
        //   not re-acquired — the foreign term may still be in
        //   progress.
        // - `generation`: belt-and-suspenders. recover_from_pg() does
        //   NOT write to self.leader's generation (the caller reads
        //   the PG floor as an independent step; the seed is applied
        //   only at the shared post-gate tail), so any change here is
        //   unambiguously a lease-loop write; keeping the comparison
        //   also catches any future writer or seed-ordering mistake.
        //
        // Same-epoch re-acquire (self-fence false alarm followed by a
        // successful renew — no holder change, transition count
        // unchanged): none of the signals move and this recovery is
        // KEPT. Its result is valid (no foreign PG writes happened) and
        // is used by the inline post-recovery dispatch and by any
        // commands queued ahead of the already-queued LeaderLost.
        // Processing that LeaderLost invalidates the kept completion
        // together with the wipe (handle_leader_lost calls
        // invalidate_recovery_completion before clear_persisted_state),
        // and the second LeaderAcquired's recovery re-establishes it —
        // so the stretch between the wipe and the re-run stays
        // dispatch-gated.
        //
        // Documented residual: a foreign term inside the recovery
        // window whose observed transition count lands back exactly on
        // the recorded entry value (the count-coincidence ABA). The
        // edge-ful shape: a Lease deletion resets the count, the
        // recreated object's holder changes bring it back to the entry
        // value by the time our re-steal's on_acquire records it.
        // Cheapest version: the entry steal was the Lease's first
        // holder change (transitions_at_entry=1), then one
        // `kubectl delete lease`, a peer wins the recreate race and
        // runs a full term (acquire, recover, dispatch, vacate), and
        // our re-steal of the recreated Lease lands at transitions=1.
        // A creator entry (transitions_at_entry=0) needs a second
        // deletion; later-failover entries need correspondingly more
        // in-window holder changes. The no-edge shapes (the foreign
        // term ends in a graceful vacate, or is erased by a further
        // deletion, entirely inside our observation gap) are detected
        // by the lease loop's rebound — it re-records the moved count,
        // clears recovery_complete, and re-fires LeaderAcquired — so
        // they reduce to the same coincidence pricing: only an observed
        // count that lands back exactly on the recorded value defeats
        // the gate. The generation cannot flag any of these
        // (gen_at_entry >= transitions_at_entry + 1 by construction, so
        // the re-derivation's fetch_max is a no-op — no PG-floor
        // saturation needed); and we hold the lease again by gate time,
        // so all three signals reproduce. Accepted: it takes an
        // operator deletion plus a complete foreign term inside one
        // recovery window AND the count landing back on its entry
        // value; the gate cannot tell that from the same-epoch
        // re-acquire it deliberately keeps (above) without a signal no
        // apiserver-side reset can replay (e.g. the Lease object's
        // identity); the post-claim confirmation does not close it
        // either (when the foreign term raised the floor we do bump,
        // but we hold the re-stolen Lease at confirmation time, so the
        // confirmation legitimately succeeds). Exposure: in the
        // edge-ful shape the already-queued LeaderLost + LeaderAcquired
        // clear and re-run recovery — until then the stale DAG feeds
        // the inline post-recovery dispatch and any commands queued
        // ahead of them; in a detected rebound the synthesized
        // LeaderAcquired re-runs it with the same bounded window (plus
        // whatever gates only on is_leader — dispatch itself stays
        // recovery-gated); in the undetected coincidence-on-a-rebound
        // shape no command is queued at all, so the stale recovery
        // persists until the next real leadership change or rebound —
        // that narrower shape is the accepted exposure.
        let gen_at_entry = self.leader.generation();
        let transitions_at_entry = self.leader.acquired_transitions();

        let start = Instant::now();
        let result = self.recover_from_pg().await;

        // --- Fetch PG generation high-water mark (independent step) ---
        // Read OUTSIDE recover_from_pg so a DAG-load failure cannot
        // take the floor — and the claim/confirmation built on it —
        // down with it; reading after the load preserves the previous
        // freshness ordering (the claim loop's PK-conflict retry
        // tolerates a stale read either way). NOT applied here: the
        // fetch_max happens at the shared post-gate tail below, AFTER
        // the TOCTOU gen-snapshot check — writing self.leader's
        // generation here would false-positive that check. The floor
        // spans assignments ∪ leader_generation_claims — see
        // max_known_generation's doc for why neither arm alone is
        // reliable.
        // Test-only: deterministic floor-read failure (same `ActorError`
        // shape a sqlx error maps to) without touching the DB; scoped to
        // the floor read only — the DAG load above is unaffected.
        #[cfg(test)]
        let fail_floor_read = std::mem::take(&mut self.fail_next_floor_read);
        #[cfg(not(test))]
        let fail_floor_read = false;
        let pg_floor_read: Result<Option<i64>, ActorError> = if fail_floor_read {
            Err(ActorError::Database(sqlx::Error::PoolClosed))
        } else {
            self.db
                .max_known_generation()
                .await
                .map_err(ActorError::from)
        };
        // For the post-gate "seeded generation" log line: the floor
        // value when readable (None when unreadable or empty — the
        // field keeps its historical name).
        let pg_high_water: Option<i64> = match &pg_floor_read {
            Ok(v) => *v,
            Err(_) => None,
        };

        // Stale-pin cleanup: crash-between-pin-and-unpin (scheduler
        // crashed after dispatch pin but before completion unpin)
        // leaves rows in scheduler_live_pins for terminal drvs.
        // Sweep them — safe to remove (drv is terminal, inputs no
        // longer in-use). Best-effort; grace period is fallback.
        // Runs BEFORE the gen re-check so a lease flap during this
        // await is caught at the TOCTOU gate below — like every other
        // PG round-trip in handle_leader_acquired it must run inside
        // the gen_at_entry window. The DELETE is DB-side
        // terminal-status based, independent of `result`.
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

        // --- Durably claim the generation this term will dispatch at ---
        // r[impl sched.recovery.fetch-max-seed+4]
        //
        // The PRIMARY generation source is the Lease's transition count
        // (the lease loop's fetch_max in on_acquire — the apiserver
        // bumps leaseTransitions atomically with the holder change, so
        // two holders can never share a generation). The PG floor
        // (`pg_max_gen`, GREATEST over assignments ∪ claims) is the
        // DURABLE backstop that survives the Lease object being deleted
        // and recreated at transitions=0: it bounds how far back a
        // post-deletion leader can land.
        //
        // The claim INSERT is what makes that floor cover generations
        // that never persisted on an assignment row: a leader deposed
        // before its first dispatch would otherwise leave no trace in
        // PG, and its post-deletion successor would seed from the same
        // stale floor and reuse its generation. Claiming BEFORE
        // set_recovery_complete() means no assignment is ever stamped
        // with an unclaimed generation (dispatch_ready gates on
        // recovery_complete).
        //
        // This bounds the post-deletion damage; it cannot prevent it
        // under a PG point-in-time restore (which regresses the claims
        // table and the assignment history together).
        //
        // Placement: BEFORE the gen re-check below, like the
        // sweep_stale_* calls above — these are the async PG
        // round-trips of handle_leader_acquired and every one of them
        // must sit inside the gen_at_entry window so a lease flap
        // during any of these awaits is caught at the single TOCTOU
        // gate. The claim does not write self.leader's generation, so
        // it cannot false-positive that gate. (Only the
        // seed_generation_from ATOMIC WRITE must come after the gate —
        // it would look like a flap otherwise.) A claim row left
        // behind by a recovery the gate then discards is a harmless
        // over-claim: the next term's floor reads a value ≥ anything
        // it would have read anyway. When the floor cannot vouch for
        // the target this block lands on (a target above the entry
        // generation, or a retained entry generation more than one
        // above the floor), an additional wait between here and the
        // gate requires a post-claim Leading round before the seed
        // (sched.recovery.bump-confirm, below) — the floor cannot
        // distinguish a dead predecessor's claim from a live
        // successor's, nor rule out a live successor inside a
        // never-claimed gap; unconfirmed such recoveries are discarded
        // and re-run by the next acquire edge.
        //
        // Same-epoch re-acquire (a self-fence false alarm followed by
        // a successful renew, or a discarded-and-requeued recovery):
        // the floor now CONTAINS our own claim row from the previous
        // run. The rule is "exceed every floor generation the claims
        // ledger does not prove is our own current epoch's; retain
        // only on our own claim row" — bumping past our own row would
        // burn a generation per connectivity blip and fence our own
        // in-flight assignments, contradicting the lease-side
        // semantics (same epoch ⇒ same generation ⇒ in-flight work
        // stays valid). `holder_id` is the replica's pod identity:
        // no two LIVE processes ever share one — the predecessor is
        // dead before its successor starts. It IS reused when the
        // kubelet restarts the container in place (same pod, same
        // `HOSTNAME`): while the Lease still names the pod, that
        // successor renews at the same transition count. With no
        // prior lease deletion it re-derives the same generation,
        // ties the floor, and retains through this very own-claim-row
        // match — like the same-process blip. In the saturated
        // post-deletion regime its restarted in-memory counter
        // re-derives transitions+1 BELOW the floor, so the floor-above
        // arm bumps and waits for the post-claim confirmation instead
        // (the modeled renewLease post-deletion crash/recover
        // re-acquire, kept reachable in CI by the deletion-regime
        // floor-bump witness check) — safe, just not a retain. Only a
        // REPLACED pod gets a new name, steals, bumps
        // leaseTransitions, and derives a fresh generation that needs
        // no self re-claim. (See `SchedulerDb::claim_generation`'s doc
        // for the load-bearing premise.)
        //
        // Does the (max) claims-ledger row sit at exactly `gen` and
        // belong to `holder`? The read-back guards the i64→u64 edge:
        // a negative generation in the table is a hand-edited anomaly
        // and matches nothing.
        let claim_row_matches = |row: &Option<(i64, String)>, at_gen: u64, holder: &str| {
            row.as_ref()
                .is_some_and(|(g, h)| u64::try_from(*g).ok() == Some(at_gen) && h == holder)
        };
        // r[impl sched.lease.generation-claim+2]
        let (claim_target, floor_vouches_entry) = match &pg_floor_read {
            // Floor unreadable: PG could not answer even the
            // single-row floor query, so it cannot vouch for the entry
            // generation — the same conservative posture as a failed
            // claims-ledger read ("counts as no proof"). No claim
            // INSERT is attempted (it would fail the same way); the
            // post-claim Leading-round confirmation below needs no PG
            // and still runs, so a deposed-but-unaware replica is
            // discarded instead of completing above a live successor.
            // The shared post-gate tail still runs the (no-op) seed; a
            // confirmed term completes unclaimed at the entry
            // generation — see the floor-unreadable pricing at the
            // disposition below.
            Err(e) => {
                warn!(
                    error = %e,
                    gen_at_entry,
                    "PG generation floor unreadable; proceeding unclaimed at the \
                     entry generation after the post-claim leadership confirmation"
                );
                metrics::counter!("rio_scheduler_generation_floor_read_failed_total").increment(1);
                (gen_at_entry, false)
            }
            Ok(pg_max_gen) => {
                // u64 view of the PG floor. A negative BIGINT is a
                // hand-edited or corrupt row — clamp to 0 (below every
                // real generation, so it demands nothing) and warn;
                // same trust boundary as the negative-leaseTransitions
                // clamp on the lease side.
                let pg_floor = pg_max_gen.map(|g| {
                    u64::try_from(g).unwrap_or_else(|_| {
                        warn!(
                            pg_floor = g,
                            "negative generation in the PG floor; treating as no floor"
                        );
                        0
                    })
                });
                // Can the durable floor vouch for the entry generation?
                // It can when it reaches to within one of it (an
                // ordinary failover over a contiguous history, or a tie
                // — whose ownership the claim-target match below
                // resolves), and a fresh cluster's entry generation 1
                // needs no history at all. A floor more than one below
                // the entry (or no floor while the entry exceeds 1)
                // means at least one generation in between was derived
                // from the Lease but never claimed and never left an
                // assignment row — a predecessor that died between its
                // acquire edge and its claim INSERT — and a
                // post-deletion successor may be live inside that gap,
                // below us. Such targets wait for the post-claim
                // confirmation below even though they retain the entry
                // generation.
                let floor_vouches_entry = match pg_floor {
                    Some(f) => f.saturating_add(1) >= gen_at_entry,
                    None => gen_at_entry <= 1,
                };
                // The holder at the floor only matters when the floor
                // lands EXACTLY on our epoch: below it nothing demands
                // a bump, above it we must exceed regardless of whose
                // it is. One extra single-row PK scan on the
                // re-acquire and collision paths only.
                let max_claim = if pg_floor == Some(gen_at_entry) {
                    self.db.max_claimed_generation().await.ok().flatten()
                } else {
                    None
                };
                let own_claim_at_our_gen =
                    claim_row_matches(&max_claim, gen_at_entry, &self.holder_id);
                let initial = match pg_floor {
                    // Someone's generation exceeds our epoch (the
                    // post-deletion case: PG remembers a higher world
                    // than the recreated Lease derives to). Exceed it.
                    Some(f) if f > gen_at_entry => f.saturating_add(1),
                    // The floor lands exactly on our epoch but the
                    // ledger cannot affirm it is ours: another holder's
                    // claim row, an assignments-only floor with no
                    // claim row at all (pre-claim-ledger assignment
                    // history from before migration 063, or a
                    // predecessor that proceeded unclaimed), or a
                    // failed ledger read. Treat it as foreign and
                    // exceed it.
                    Some(f) if f == gen_at_entry && !own_claim_at_our_gen => f.saturating_add(1),
                    // Everything in PG is at or below our epoch, or the
                    // floor ties it and the ledger shows OUR OWN claim
                    // row there (the same-epoch re-acquire): retain the
                    // entry generation. The rule is "exceed every floor
                    // generation the claims ledger does not prove is
                    // our own current epoch's; retain only on our own
                    // claim row". Assignment rows carry no scheduler-
                    // holder identity (only builder_id), so the ledger
                    // is the only acceptable witness — and a failed
                    // ledger read counts as no proof. Bumping past our
                    // own row would burn a generation per connectivity
                    // blip and fence our own in-flight assignments.
                    _ => gen_at_entry,
                };
                if claim_row_matches(&max_claim, initial, &self.holder_id) {
                    // Same-epoch re-acquire: our own claim row from
                    // the previous run is already durable. Nothing to
                    // INSERT.
                    debug!(
                        generation = initial,
                        "same-epoch re-acquire; generation claim already durable"
                    );
                    (initial, floor_vouches_entry)
                } else {
                    // The PK conflict is the CAS: another holder
                    // already claimed exactly this generation
                    // (reachable only when the Lease was deleted and
                    // two replicas raced through fresh acquisitions) →
                    // re-target past the claims high-water and retry,
                    // bounded. Whatever happens, the seed below uses
                    // the LAST ATTEMPTED value — never one that was
                    // not offered to the ledger.
                    let mut target = initial;
                    for attempt in 0..3u32 {
                        // Write-direction guard: a generation above
                        // i64::MAX is unreachable (2^63 leadership
                        // epochs), but a wrapping `as` would insert a
                        // negative row and silently break the ledger's
                        // monotonicity. Saturate instead.
                        let target_db = i64::try_from(target).unwrap_or(i64::MAX);
                        match self.db.claim_generation(target_db, &self.holder_id).await {
                            Ok(true) => break,
                            Ok(false) => {
                                // Read back the winning row. If it is
                                // OURS at this exact generation, the
                                // "conflict" is an idempotent re-claim
                                // of our own previous row — success,
                                // not a collision. (Unreachable given
                                // the pre-INSERT check above, but it
                                // keeps the CAS semantics
                                // self-consistent.)
                                let winner = self.db.max_claimed_generation().await.ok().flatten();
                                if claim_row_matches(&winner, target, &self.holder_id) {
                                    break;
                                }
                                if attempt == 2 {
                                    warn!(
                                        target,
                                        "generation claim still conflicting after retries; \
                                         proceeding unclaimed at the last attempted target \
                                         (collision window re-opens for this term)"
                                    );
                                    break;
                                }
                                let bumped = winner.map_or(target.saturating_add(1), |(g, _)| {
                                    u64::try_from(g).unwrap_or(0).saturating_add(1)
                                });
                                let next = bumped.max(target.saturating_add(1));
                                warn!(
                                    target,
                                    bumped = next,
                                    attempt,
                                    "generation claim conflict; re-targeting"
                                );
                                target = next;
                            }
                            Err(e) => {
                                // PG died between the seed read and
                                // the claim write. Proceed: blocking
                                // recovery here would convert a PG
                                // blip at failover time into a leader
                                // that holds the lease but never
                                // dispatches, while the standby cannot
                                // take over. The cost of proceeding is
                                // that THIS term's generation is not
                                // in the claims ledger — exactly the
                                // pre-claim window, for one term.
                                error!(
                                    error = %e,
                                    target,
                                    "generation claim failed; proceeding unclaimed"
                                );
                                metrics::counter!("rio_scheduler_generation_claim_failed_total")
                                    .increment(1);
                                break;
                            }
                        }
                    }
                    (target, floor_vouches_entry)
                }
            }
        };
        // Snapshot the lease loop's round counter AFTER the claim
        // attempt: the claim-attempt→snapshot ordering is load-bearing
        // for the bump confirmation below — a Leading round whose id
        // exceeds this snapshot began after the claim row (when there
        // is one) became durable, so any replica that acquires later
        // reads a floor that covers our claim and exceeds it.
        let rounds_at_claim = self.leader.renew_rounds_started();

        // Test-only interleave gate: lets a test bump `generation`
        // between the awaits above and the gen re-check below,
        // deterministically proving the TOCTOU fix covers
        // recover_from_pg, the sweep_stale_* calls, AND the
        // generation-claim loop. Signal "reached" then wait for
        // "release".
        #[cfg(test)]
        if let Some((reached_tx, release_rx)) = self.recovery_toctou_gate.take() {
            let _ = reached_tx.send(());
            let _ = release_rx.await;
        }

        // The duration histogram is recorded for EVERY attempt — even
        // ones the gate below discards — because it measures the
        // PG-load outcome/duration, and the error arm's partial-state
        // clear (.dag = new(), etc.) doesn't touch `start`. Label by
        // outcome: a 30s failure (PG timeout) and a 30s success (huge
        // DAG) are very different signals; without the label, one
        // washes out the other. The attempt COUNTER is recorded after
        // the gate (one increment per attempt, final disposition).
        let outcome = if result.is_ok() { "success" } else { "failure" };
        let elapsed = start.elapsed();
        metrics::histogram!("rio_scheduler_recovery_duration_seconds", "outcome" => outcome)
            .record(elapsed.as_secs_f64());
        info!(elapsed_ms = elapsed.as_millis(), outcome, "recovery timing");

        // r[impl sched.recovery.bump-confirm+3]
        // A claim target the durable floor cannot vouch for is only
        // seeded — and the recovery only completed — after positive,
        // post-claim evidence from the apiserver that this replica
        // holds the Lease. Three trigger shapes:
        //
        // - A target ABOVE the entry generation (the floor-above,
        //   foreign-row-at-our-gen, conflict-retry, and
        //   proceed-unclaimed paths): PG alone cannot distinguish "the
        //   floor's excess belongs to a dead previous term" (the
        //   routine post-deletion case — must bump) from "the excess
        //   belongs to a live successor" (must not leapfrog); only the
        //   Lease can.
        // - A RETAINED entry generation the floor does not reach to
        //   within one (a derived-but-never-claimed predecessor epoch
        //   sits in between): a post-deletion successor can have seeded
        //   inside that gap, below us, and completing above it would
        //   invert the fence the same way.
        //
        // - A RETAINED entry generation under an UNREADABLE floor: a
        //   floor that cannot be read cannot vouch for anything, so the
        //   fallback waits for the same confirmation even though its
        //   target degenerates to the entry generation.
        //
        // Ordinary failovers (floor + 1 == entry), same-epoch
        // re-acquires (floor ties the entry on our own claim row), and
        // fresh-cluster entries (no floor, entry == 1) skip this
        // entirely. A floor exactly one below the entry is exempt
        // because a successor over that floor either collides with our
        // claim at the entry generation (the PK CAS) or seeds above us
        // — except in the named adjacent-floor-race residual (see the
        // confirmation fn's doc). The floor-unreadable fallback
        // proceeds unclaimed but is confirmation-gated like every
        // other non-vouched path, and the claim-failure/
        // proceed-unclaimed degradation still never blocks beyond the
        // existing cap.
        let needs_confirmation = claim_target > gen_at_entry || !floor_vouches_entry;
        let confirm_started = Instant::now();
        let confirmed = if needs_confirmation {
            self.await_post_claim_leadership_confirmation(
                rounds_at_claim,
                gen_at_entry,
                transitions_at_entry,
            )
            .await
        } else {
            true
        };

        // TOCTOU re-check: did leadership change hands (or lapse)
        // during recovery? Trips on any of:
        // - the generation moved (a lease-loop write — recover_from_pg
        //   doesn't touch self.leader's generation),
        // - the recorded acquire-transitions moved (a holder change
        //   whose generation fetch_max was a no-op in the saturated
        //   regime — observed through a lose/acquire edge pair or
        //   through a rebound),
        // - is_leader is false (a lose landed and we have not
        //   re-acquired),
        // - a recovery whose claim target the floor cannot vouch for
        //   never got its post-claim leadership confirmation (above).
        // In every discard case the lease loop has already queued the
        // follow-up commands in our mpsc — discard this recovery, let
        // the next LeaderAcquired re-run it.
        //
        // INVARIANT: no await may be introduced between this check and
        // set_recovery_complete() (its single call site, at the end of
        // the shared tail below) — and new PG
        // round-trips or waits (like the bump confirmation) belong
        // ABOVE, with recover_from_pg, the sweep_stale_* calls, and the
        // generation-claim loop. The completion below is keyed to
        // transitions_at_entry, so a concurrent lease transition can no
        // longer turn it into a false "complete" (see the entry-
        // snapshot comment) — but an await here would still widen the
        // window in which this attempt's already-checked WORK (the
        // loaded DAG, the claim, the seed below) goes stale before it
        // is applied, for no benefit. (The reads below are atomic
        // loads, not awaits — the invariant holds as written.)
        let gen_now = self.leader.generation();
        let transitions_now = self.leader.acquired_transitions();
        let still_leader = self.leader.is_leader();
        if gen_now != gen_at_entry
            || transitions_now != transitions_at_entry
            || !still_leader
            // r[impl sched.recovery.bump-confirm+3]
            || !confirmed
        {
            // Distinguish "the lease state moved" from "the lease state
            // never moved but the bump confirmation did not arrive" —
            // operationally very different signals.
            let discard_outcome = if !confirmed
                && gen_now == gen_at_entry
                && transitions_now == transitions_at_entry
                && still_leader
            {
                "discarded_unconfirmed"
            } else {
                "discarded_flap"
            };
            warn!(
                gen_at_entry,
                gen_now,
                transitions_at_entry,
                transitions_now,
                still_leader,
                confirmed,
                confirm_wait_ms = confirm_started.elapsed().as_millis(),
                recovery_ok = result.is_ok(),
                outcome = discard_outcome,
                "leadership changed during recovery \u{2014} lease flapped, lapsed, rebounded, \
                 or the post-claim confirmation never arrived; DISCARDING this recovery \
                 (next LeaderAcquired will retry)"
            );
            metrics::counter!("rio_scheduler_recovery_total", "outcome" => discard_outcome)
                .increment(1);
            // Clear the partial state we loaded. The next
            // LeaderAcquired's recover_from_pg() will do this
            // again but do it here too so any Tick that sneaks in
            // before the next LeaderAcquired sees a consistent
            // (empty) DAG.
            self.clear_persisted_state();
            // DON'T set recovery_complete — no completion was ever
            // recorded for this entry epoch, and that absence is what
            // keeps dispatch gated on every discard path: a lose-flap
            // leaves on_lose's clear in effect; a rebound-flap leaves
            // on_rebound's clear and new count (no lose ever fires on
            // a still-leading round); a !still_leader lapse is also
            // covered by dispatch_ready's is_leader check; and
            // discarded_unconfirmed has no lease-loop clear at all —
            // the never-recorded completion alone keeps it gated.
            // dispatch_ready re-checks is_leader and
            // recovery_complete(); early-return here makes the
            // post-LeaderAcquired dispatch a no-op.
            return;
        }

        // A loss observed while recovery was running (a bare lose, not a
        // re-acquire — the TOCTOU check above only catches the latter)
        // still reaches the set_recovery_complete() below, but the stamp
        // it records is keyed to THIS acquire's transition count: the
        // next acquire moves `acquired_transitions`, the stale stamp no
        // longer matches, and `recovery_complete()` reads false again —
        // so the next tenure's pre-recovery gap stays gated for
        // dispatch_ready without any is_leader() special-casing here.
        //
        // Final disposition of this attempt: exactly one
        // rio_scheduler_recovery_total increment per attempt. The
        // discard branch above counted discarded_*; every attempt that
        // survives the gate counts success|failure here, so discard
        // outcomes take precedence over the load result (a
        // failed-then-discarded attempt counts only as discarded_*; the
        // load failure stays visible in the duration histogram). The
        // increment is synchronous — the no-awaits-before-
        // set_recovery_complete() INVARIANT above is untouched.
        metrics::counter!("rio_scheduler_recovery_total", "outcome" => outcome).increment(1);

        if let Err(e) = &result {
            // DAG load failed: continue with an EMPTY DAG (in-flight
            // builds are lost for this term), but the generation
            // handling is NOT skipped — the floor was read
            // independently above, so this term is floored, claimed,
            // and (when required) confirmed like any other; its
            // dispatches stay inside what the durable floor covers.
            // Only when the floor itself was unreadable
            // (`floored = false` below) does the term proceed —
            // confirmation-gated, unclaimed — at the entry generation:
            // in the saturated post-deletion regime that generation
            // sits below the executors' fetch_max latch, so long-lived
            // builders silently reject its dispatches until the next
            // leadership transition. The floor-unreadable operator
            // signal is the warn plus the floor-read-failure counter
            // at the claim-target match (they fire whether or not the
            // load also failed); this error line remains the
            // load-failure signal. Better than blocking dispatch
            // forever while holding the lease.
            error!(
                error = %e,
                floored = pg_floor_read.is_ok(),
                "state recovery FAILED — continuing with empty DAG \
                 (in-flight builds lost for this term)"
            );
            // Explicitly re-clear: recovery may have partially
            // populated before failing. dag_authoritative stays false
            // (clear_persisted_state keeps it cleared): destructive
            // consumers must not treat "not in the (empty) DAG" as
            // "stale".
            self.clear_persisted_state();
        }
        // Only on a successful load: the DAG was rebuilt from PG by
        // this tenure's recover_from_pg, so "not in the DAG" means
        // "stale" again. On the Err path the clear_persisted_state
        // above keeps the bit false.
        if result.is_ok() {
            self.dag_authoritative = true;
        }

        // --- Seed the generation, then ungate dispatch ---
        //
        // Shared tail for every non-discard outcome. `claim_target`
        // was computed AND durably claimed in the claim loop above,
        // inside the gen_at_entry window. Only the ATOMIC WRITE
        // happens here, after the TOCTOU check — writing the atomic
        // before the check would make the seed itself look like a
        // lease flap. fetch_max not store: both writers (the lease
        // loop and this seed) only ever raise the same Arc. In the
        // floor-unreadable fallback `claim_target` equals the entry
        // generation, so the seed is a no-op — kept unconditional so
        // there is exactly one seed call site (the gate-pass tail).
        // No awaits from here to set_recovery_complete() — see the
        // INVARIANT comment at the gen re-check.
        let prev = self.leader.seed_generation_from(claim_target);
        if claim_target > prev {
            info!(
                prev_gen = prev,
                pg_high_water = ?pg_high_water,
                new_gen = claim_target,
                confirm_wait_ms = confirm_started.elapsed().as_millis(),
                "seeded generation from PG floor (assignments ∪ claims)"
            );
        }
        self.leader.set_recovery_complete(transitions_at_entry);
    }

    /// Wait for a post-claim apiserver round-trip that ended with this
    /// replica as the Lease holder, before a claim target the durable
    /// PG floor cannot vouch for — one above the recovery-entry
    /// generation, a retained entry generation more than one above
    /// the floor, or a retained entry generation whose floor could not
    /// be read at all — is seeded and the recovery completed
    /// (`sched.recovery.bump-confirm`).
    ///
    /// What a confirming round establishes — and what it does not:
    ///
    /// - leg (i): a round that resolved `Leading` after
    ///   `rounds_at_claim` was snapshotted shows that at that
    ///   observation no other replica held the Lease, so a successor
    ///   that acquired after our acquire edge is no longer the holder —
    ///   we are not completing recovery above a *live* successor.
    /// - leg (ii): on the claimed path our claim row was durable before
    ///   that round began, so any replica that acquires later reads a
    ///   floor at or above our claim and exceeds it — its generation
    ///   lands above ours, the correct direction. (On the
    ///   proceed-unclaimed and floor-unreadable arms leg (ii) does not
    ///   hold; that is the pre-existing claim-failure residual,
    ///   unchanged here.)
    ///
    /// Residuals that remain: the count-coincidence ABA documented at
    /// the gate's entry-snapshot comment (an edge-ful re-steal, or a
    /// rebound, whose observed transition count lands back exactly on
    /// the recorded value — in the rebound sub-case no command is
    /// queued, so the stale recovery persists until the next real
    /// leadership change or rebound); the claim-failure conjunction (a
    /// term that proceeded unclaimed leaves no durable trace of its
    /// generation, so a Lease deletion after that term's confirmation
    /// still lets a successor seed below it); and the adjacent-floor
    /// race (the never-claimed gap is exactly one generation wide and
    /// the post-deletion successor's claim at our entry−1 lands before
    /// our floor read, so the floor looks contiguous and no wait is
    /// required — completing above the live successor then additionally
    /// needs our renew rounds to fail from the deletion through the
    /// TOCTOU gate while staying under the self-fence deadline).
    ///
    /// Exits early (`false`) when the lease loop observed a loss or a
    /// holder change — the TOCTOU gate would discard the recovery
    /// anyway. (A rebound moves `acquired_transitions` on a
    /// still-leading round, so an in-flight wait exits on that signal
    /// and the gate discards: deliberately treated as a flap — discard
    /// and re-run — not as a confirmation.) The cap is a pure backstop
    /// for a wedged-but-believing
    /// loop; such a loop also cannot renew, so the lease is lost
    /// shortly after. The wait runs on the actor task — a bounded stall
    /// on non-vouched paths only (dispatch is gated during recovery
    /// anyway, and heartbeat replies read the shared atomics in the
    /// gRPC layer, not through the actor). The cap path is
    /// intentionally untested: it requires a wedged-but-believing loop,
    /// and the loop-level wiring that matters is covered by rio-lease's
    /// round tests.
    // r[impl sched.recovery.bump-confirm+3]
    async fn await_post_claim_leadership_confirmation(
        &self,
        rounds_at_claim: u64,
        gen_at_entry: u64,
        transitions_at_entry: u64,
    ) -> bool {
        let deadline = Instant::now() + BUMP_CONFIRMATION_CAP;
        loop {
            if self.leader.last_leading_round() > rounds_at_claim {
                return true;
            }
            if !self.leader.is_leader()
                || self.leader.generation() != gen_at_entry
                || self.leader.acquired_transitions() != transitions_at_entry
            {
                // The lease loop observed a loss or a holder change —
                // the TOCTOU gate below will discard this recovery.
                return false;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(CONFIRMATION_POLL_INTERVAL).await;
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

        // Batch-probe ALL orphans' outputs in ONE FindMissingPaths
        // before the loop. Per-orphan calls were N×grpc_timeout against
        // a dead store (every orphan paid the full 30s); the per-orphan
        // decision below is then a hashset lookup. Floating-CA `[""]`
        // placeholders filtered (translate.rs convention) so they don't
        // trip FMP's InvalidArgument.
        let all_outputs: Vec<String> = orphaned
            .iter()
            .flat_map(|o| o.expected_output_paths.iter())
            .filter(|p| !p.is_empty())
            .cloned()
            .collect();
        let missing = self.batch_probe_orphan_outputs(all_outputs).await;

        for o in orphaned {
            // r[impl sched.merge.wanted-outputs+2]
            // Did the build complete while the scheduler was down
            // (orphan completion)? All WANTED outputs present = none in
            // `missing` (the probe set stays all expected paths; an
            // absent output nothing wants must not force the orphan
            // back to a from-source re-dispatch). Conservative reset
            // for: no verifiable wanted path (CA pre-completion, or a
            // wanted set that resolves to nothing), or no missing-set
            // (RPC failed/timed out).
            let present = verifiable_wanted_paths(
                &o.output_names,
                &o.expected_output_paths,
                &o.wanted_output_names,
            )
            .is_some_and(|verifiable| {
                missing
                    .as_ref()
                    .is_some_and(|m| verifiable.iter().all(|p| !m.contains(*p)))
            });
            if present {
                self.adopt_orphan_completion(&o.drv_hash, &o.executor, o.expected_output_paths)
                    .await;
            } else {
                self.reset_orphan_to_ready(&o.drv_hash, &o.executor).await;
            }
        }

        // After reconcile, dispatch anything newly Ready.
        self.dispatch_ready().await;
    }

    /// Collect an [`OrphanedAssignment`] for every Assigned/Running
    /// derivation whose worker is no longer live — the liveness-check
    /// input set for
    /// [`handle_reconcile_assignments`](Self::handle_reconcile_assignments).
    ///
    /// Cloned out of the DAG before any mutation (the per-row
    /// reset/adopt path takes `node_mut`).
    ///
    /// `executor` is `Option`: Assigned/Running with
    /// `assigned_executor=None` is inconsistent state (shouldn't
    /// happen, but recovery loads from PG which could have drifted).
    /// We still reconcile it (check store for outputs → Completed,
    /// else Ready) rather than silently skipping and leaving it stuck
    /// forever.
    fn collect_orphaned_assignments(&self) -> Vec<OrphanedAssignment> {
        let orphan = |h: &str, s: &DerivationState, w: Option<&ExecutorId>| OrphanedAssignment {
            drv_hash: h.into(),
            executor: w.cloned(),
            expected_output_paths: s.expected_output_paths.clone(),
            output_names: s.output_names.clone(),
            wanted_output_names: s.wanted_output_names.clone(),
        };
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
                        Some(orphan(h, s, Some(w)))
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
                Some(w) => Some(orphan(h, s, Some(w))),
                None => {
                    warn!(drv_hash = ?h, status = ?s.status(),
                          "reconcile: Assigned/Running drv with NULL worker — reconciling anyway");
                    Some(orphan(h, s, None))
                }
            })
            .collect()
    }

    /// One `FindMissingPaths` over the union of all orphans' expected
    /// outputs. `Some(missing_set)` on success; `None` on no-client
    /// (tests) / RPC error / timeout — caller treats `None` as "all
    /// orphans incomplete" (conservative reset_to_ready). Wrapped in
    /// `grpc_timeout` + `credit_heartbeats_for_stall` so a dead store
    /// stalls reconcile at most ONCE (not N×) and doesn't reap
    /// executors that DID reconnect; feeds `cache_breaker` like the
    /// merge-time FMP path so a 30s stall here counts toward opening.
    async fn batch_probe_orphan_outputs(
        &mut self,
        store_paths: Vec<String>,
    ) -> Option<HashSet<String>> {
        if store_paths.is_empty() {
            return Some(HashSet::new());
        }
        let Some(client) = &mut self.store_client else {
            return None;
        };
        let mut req = tonic::Request::new(FindMissingPathsRequest { store_paths });
        rio_proto::interceptor::inject_current(req.metadata_mut());
        let grpc_timeout = self.grpc_timeout;
        let fmp_start = Instant::now();
        let r = match tokio::time::timeout(grpc_timeout, client.find_missing_paths(req)).await {
            Ok(Ok(resp)) => {
                self.cache_breaker.record_success();
                Some(resp.into_inner().missing_paths.into_iter().collect())
            }
            Ok(Err(e)) => {
                self.cache_breaker.record_failure();
                warn!(error = %e,
                      "reconcile: FindMissingPaths failed, assuming all orphans incomplete");
                None
            }
            Err(_) => {
                self.cache_breaker.record_failure();
                warn!(timeout = ?grpc_timeout,
                      "reconcile: FindMissingPaths timed out, assuming all orphans incomplete");
                None
            }
        };
        self.credit_heartbeats_for_stall(fmp_start.elapsed());
        r
    }

    /// Reconcile path for an orphaned assignment whose outputs ARE in
    /// the store: the build completed while the scheduler was down.
    /// Transition Completed, persist, attribute tenants, run the
    /// terminal log epilogue (correlate + stamp — see its
    /// caller-list entry),
    /// unpin, then reuse [`release_downstream`](Self::release_downstream)
    /// for the newly-ready cascade + per-build completion check. Skips
    /// the `handle_success_completion` steps that need worker-result
    /// data (build_samples, CA bookkeeping, ancestor priorities —
    /// full_sweep on next tick handles the latter).
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
        // Passed to `release_downstream` so it lands AFTER Progress
        // (nom ordering — r[impl gw.activity.progress-before-stop]).
        let output_paths = self
            .dag
            .node(drv_hash)
            .map(|s| s.output_paths.clone())
            .unwrap_or_default();
        let completed_event = rio_proto::types::build_event::Event::Derivation(
            rio_proto::types::DerivationEvent::completed(
                self.dag.path_or_hash_fallback(drv_hash),
                output_paths,
            ),
        );
        // r[impl sched.gc.path-tenants-upsert]
        // Orphan completion during recovery: derivation was
        // Running at crash, completed during downtime. The
        // normal completion path (handle_success_completion)
        // never fired → no tenant attribution → GC
        // under-retains. output_paths was just set above
        // (= expected_outputs, verified present in store).
        self.upsert_path_tenants_for(drv_hash).await;
        // r[impl sched.merge.exec-correlation+7]
        // Same gap as path-tenants above: `handle_success_completion`
        // never fired for this drv, so the terminal chokepoint
        // (correlate + stamp) must run here.
        // The adopted completion's CompletionReport (and its
        // final_line_count) belonged to the interim leader's stream —
        // this leader never saw it. NULL → the row reads as incomplete,
        // which a recovered-across-failover log generally is.
        self.terminal_log_epilogue(drv_hash, "succeeded", &interested, None);
        // Terminal → unpin. sweep_stale_live_pins ran BEFORE
        // reconcile (the drv was Assigned/Running in PG then —
        // kept), so it won't catch this one.
        self.unpin_best_effort(drv_hash).await;
        self.release_downstream(drv_hash, &interested, HashSet::new(), Some(completed_event))
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

/// One orphaned (worker-gone) Assigned/Running derivation collected by
/// [`DagActor::collect_orphaned_assignments`] for the reconcile pass.
///
/// A named struct rather than a tuple: the output-path/output-name
/// fields are adjacent same-typed `Vec<String>`s, and a positional
/// swap would silently flip the adoption criterion from "all wanted
/// outputs present" to something nonsensical without a type error.
struct OrphanedAssignment {
    drv_hash: DrvHash,
    /// `None` = Assigned/Running with no recorded worker (PG drift) —
    /// still reconciled rather than left stuck forever.
    executor: Option<ExecutorId>,
    /// ALL declared output paths: the `FindMissingPaths` probe set and
    /// the adopted node's recorded `output_paths`. Probing an unwanted
    /// path is harmless; only the adopt-vs-reset DECISION filters by
    /// wanted.
    expected_output_paths: Vec<String>,
    /// Declared output names, index-paired with `expected_output_paths`.
    /// Together with `wanted_output_names` these feed
    /// [`crate::state::verifiable_wanted_paths`] for the adopt-vs-reset
    /// decision.
    output_names: Vec<String>,
    /// The recovered row's wanted set (empty = all declared wanted).
    wanted_output_names: Vec<String>,
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
