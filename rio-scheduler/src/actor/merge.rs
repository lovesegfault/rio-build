//! DAG merge handling: SubmitBuild → merge client DAG into global DAG.
// r[impl sched.merge.dedup]
// r[impl sched.merge.shared-priority-max]
// r[impl sched.merge.toctou-serial]

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use tracing::{debug, error, info, instrument, warn};
use uuid::Uuid;

use rio_proto::types::FindMissingPathsRequest;

use crate::state::{
    BuildInfo, BuildState, BuildStateExt, DerivationStatus, DrvHash, effective_wanted,
    union_wanted_saturating, verifiable_wanted_paths,
};

use super::{ActorError, DagActor, MergeDagRequest};

/// Cross-phase carrier from [`DagActor::validate_and_ingest`] to
/// [`DagActor::reconcile_merged_state`].
///
/// `handle_merge_dag` was a ~550-line monolith with 6 locals threaded
/// across 8 phases and 3 early-return error paths. Splitting at the
/// "build is now Active" boundary (step 5 → step 6) gives a clean
/// `Result<_, ActorError>` for the validate/persist half; everything
/// after that point is log-and-continue (build is committed). This
/// struct carries the locals that cross that boundary.
///
/// `node_index` is NOT carried (it borrows from `nodes`, which would
/// make this self-referential) — `reconcile_merged_state` rebuilds it
/// in one pass over `nodes`.
pub(super) struct MergeIngest {
    pub build_id: Uuid,
    /// Post-topdown-prune node set (may be smaller than the request's).
    pub nodes: Vec<crate::domain::DerivationNode>,
    /// Only `edges.len()` survives past step 5 (for the total-time log);
    /// the edges themselves are consumed by `dag.merge` + persist.
    pub edges_len: usize,
    /// Deduped parent hashes of the post-prune submission `edges`,
    /// resolved while the edges were still alive in
    /// `validate_and_ingest`. Candidate set for the caller's
    /// post-reconciliation `topdown_pruned` clear pass — empty for a
    /// pruned merge (its edges were dropped with the closure).
    pub edge_parent_hashes: Vec<DrvHash>,
    pub merge_result: crate::dag::MergeResult,
    pub event_rx: super::BuildEventReceivers,
    /// Pre-existing not-done nodes that were re-probed in step 4.
    pub existing_reprobe: HashSet<DrvHash>,
    pub cached_hits: HashMap<DrvHash, Vec<String>>,
    /// Derivations whose outputs are upstream-substitutable but not
    /// yet locally present. Their materialization jobs ride the merge
    /// transaction (the new_sub/reprobe lanes); `reconcile_merged_state`
    /// applies the matching in-memory corrections after
    /// `seed_initial_states`.
    pub pending_substitute: Vec<(DrvHash, Vec<String>)>,
    /// Threaded for `verify_preexisting_completed`'s store call.
    pub jwt_token: Option<String>,
}

/// Output of [`DagActor::reconcile_merged_state`].
pub(super) struct MergeReconcile {
    pub cached_count: u32,
    /// First Poisoned/DependencyFailed hash (newly-seeded OR
    /// pre-existing). Caller fires `handle_derivation_failure` on it.
    pub first_dep_failed: Option<DrvHash>,
    /// Other builds (≠ this merge's build_id) interested in a node
    /// that a re-probe transitioned →Completed. Caller fans out
    /// `update_build_counts` + `check_build_completion` so a
    /// shared-node completion doesn't leave the earlier build hung
    /// Active. r[sched.merge.dedup]
    pub other_builds: HashSet<Uuid>,
}

impl DagActor {
    // -----------------------------------------------------------------------
    // MergeDag
    // -----------------------------------------------------------------------

    #[instrument(skip(self, req), fields(build_id = %req.build_id))]
    pub(super) async fn handle_merge_dag(
        &mut self,
        req: MergeDagRequest,
    ) -> Result<super::BuildEventReceivers, ActorError> {
        // I-139: per-phase timing. handle_merge_dag was >300s for a
        // 153k-node / 837k-edge DAG with only ~22s in the batched DB
        // phase; the rest had no logging. Each phase now self-reports
        // so future regressions localize immediately. The two extracted
        // phases keep their own per-step `phase!` macros; this scope
        // tracks the inter-phase boundaries + total.
        let t_total = Instant::now();
        // rio_scheduler_builds_total is incremented at terminal transition
        // (complete_build/transition_build_to_failed/handle_cancel_build)
        // with an outcome label, so SLI queries can compute success rate.

        // Phase 1: validate, merge into DAG, cache-check, persist, → Active.
        // All early-return error paths (cycle, breaker open, persist fail,
        // transition reject) live here. After this returns Ok the build is
        // committed; later DB errors are log-and-continue.
        let ingest = self.validate_and_ingest(req).await?;
        let build_id = ingest.build_id;
        let total_derivations = ingest.nodes.len() as u32;

        // Phase 2: cached-hit transitions, critical-path, stale-reset,
        // initial-state seed, pre-existing reconciliation.
        let MergeReconcile {
            cached_count,
            first_dep_failed,
            other_builds,
        } = self.reconcile_merged_state(&ingest).await;

        // Update build's cached count + persist initial denorm columns.
        if let Some(build) = self.builds.get_mut(&build_id) {
            build.cached_count = cached_count;
        }
        // I-103: sets completed_count from DAG ground truth + persists
        // (total, completed, cached) to PG so list_builds is O(LIMIT).
        self.update_build_counts(build_id).await;

        // r[impl sched.merge.substitute-topdown+12]
        // Post-reconciliation `topdown_pruned` clear pass. A node may
        // only lose the mark once its children (in the post-merge DAG)
        // are all already produced — its closure is then in the store,
        // so a from-source dispatch is no longer doomed. The decision
        // is taken HERE, after `reconcile_merged_state`, and not at
        // merge/persist time, so it sees the post-verification view:
        // phase 6c (`verify_preexisting_completed`) demotes a stale
        // pre-existing Completed child (output GC'd, not substitutable)
        // back to Ready in the same merge, and a clear computed before
        // that demotion would be laundered by the stale status —
        // dropping the guard while the closure is NOT in the store.
        // Candidates are this submission's edge parents plus the DAG
        // parents of this merge's cache hits (a hit is a child that can
        // turn a parent all-produced); each unique candidate is
        // evaluated once and gated on the flag first, so a merge with
        // no marked parents pays one node lookup per unique parent
        // instead of a children scan per edge. The PG clear is
        // best-effort outside the edge-insert transaction (warn and
        // continue, same posture as the lazy and fail-fast clears): a
        // crash between the merge commit and this pass leaves the mark
        // set, which is the safe direction — at worst a bounded
        // wrongful fail-fast that a resubmit recovers, never the
        // unbounded doomed from-source dispatch. A merge that adds only
        // unbuilt children keeps the mark (they can be reaped unbuilt
        // later — the classifier judges them Pending, not Vouched; see
        // `closure_vouched`); once those children are produced, the
        // completion-time `clear_topdown_pruned_for_produced_parents`
        // drops it (with the lazy clear in `handle_substitute_complete`
        // as the walk-failure backstop).
        //
        // Closure-hole healing comes FIRST: a full merge that
        // re-declares a node's edges re-supplies its inputDrvs, so its
        // child set is representative of its closure again — drop the
        // `closure_hole` breadcrumb for every edge parent of this
        // submission before judging the clear. A pruned merge has
        // no edges (`edge_parent_hashes` is empty), so it never heals a
        // hole. Candidates that are NOT edge parents of this submission
        // (e.g. pre-existing DAG parents of a cache hit) keep their
        // breadcrumb and are skipped below: the classifier judges their
        // reap-truncated child set Broken, never Vouched.
        //
        // The persisted breadcrumb (`migrations/064`) is cleared here
        // too, best-effort, for EVERY edge parent of this submission:
        // the upsert above never clears the column (OR-on-conflict,
        // merge binds false), and the heal must also fire when the
        // `topdown_pruned` mark stays (unbuilt children), so it cannot
        // ride the mark-clear statement below. The clear is total —
        // NOT keyed on the pre-clear in-memory value — because the
        // persisted bit can be stale when the in-memory copy was
        // cleared elsewhere or lost: the node may have been reaped out
        // and re-inserted fresh (or restored by a poisoned-stub
        // recovery) with the field at its `false` default while the
        // OR-combined column still carries the old truncation, and an
        // earlier heal's best-effort write may simply have failed. The
        // helper's `AND closure_hole` WHERE keeps the statement a
        // no-op for rows that never carried the hole.
        // r[impl sched.evidence.closure-hole]
        for hash in &ingest.edge_parent_hashes {
            if let Some(s) = self.dag.node_mut(hash) {
                s.closure_hole = false;
            }
        }
        let heal_parents: Vec<String> = ingest
            .edge_parent_hashes
            .iter()
            .map(|h| h.to_string())
            .collect();
        if !heal_parents.is_empty() {
            match self
                .db
                .clear_closure_hole_by_hashes(&heal_parents, self.serving_generation())
                .await
            {
                Ok(crate::db::FencedWrite::Fenced) => {
                    self.note_fenced_evidence_write("merge-heal closure_hole clear");
                }
                Ok(_) => {}
                Err(e) => {
                    warn!(build_id = %build_id, count = heal_parents.len(), error = %e,
                          "failed to clear persisted closure_hole after merge heal (continuing)");
                }
            }
        }
        let mut clear_candidates: HashSet<DrvHash> =
            ingest.edge_parent_hashes.iter().cloned().collect();
        for child in ingest.cached_hits.keys() {
            clear_candidates.extend(self.dag.get_parents(child));
        }
        let mut cleared: Vec<String> = Vec::new();
        for hash in clear_candidates {
            if self.dag.node(&hash).is_some_and(|s| s.topdown_pruned)
                && self.closure_vouched(&hash)
                && let Some(s) = self.dag.node_mut(&hash)
            {
                s.topdown_pruned = false;
                cleared.push(hash.to_string());
            }
        }
        if !cleared.is_empty() {
            debug!(
                build_id = %build_id,
                cleared = cleared.len(),
                "cleared topdown_pruned for parents whose children are all produced after reconciliation"
            );
            match self
                .db
                .clear_topdown_pruned_by_hashes(&cleared, self.serving_generation())
                .await
            {
                Ok(crate::db::FencedWrite::Fenced) => {
                    self.note_fenced_evidence_write("merge-reconciliation topdown_pruned clear");
                }
                Ok(_) => {}
                Err(e) => {
                    warn!(build_id = %build_id, error = %e,
                          "failed to clear persisted topdown_pruned after merge reconciliation (continuing)");
                }
            }
        }

        // r[impl sched.merge.dedup]
        // Re-probe completed a node that other builds were waiting on:
        // fan out the count-update + completion-check to them. Mirrors
        // complete_ready_from_store / release_downstream.
        for b in other_builds {
            self.update_build_counts(b).await;
            self.check_build_completion(b).await;
        }

        // Send BuildStarted event
        self.events.emit(
            build_id,
            rio_proto::types::build_event::Event::Started(rio_proto::types::BuildStarted {
                total_derivations,
                cached_derivations: cached_count,
            }),
        );

        // If a newly merged node depends on an already-poisoned existing
        // node, handle the failure now (fail build if !keepGoing, or sync
        // counts + check completion if keepGoing).
        // r[impl sched.merge.reconcile-order]
        // Defense-in-depth: re-read the node's CURRENT status before
        // fail-fasting. reconcile_merged_state's phase ordering
        // guarantees all dep-state corrections (cache-hit, stale-reset,
        // reprobe-Poisoned→Substituting) ran before any verdict was
        // computed, but a future reordering could regress this; the
        // re-check is cheap and also suppresses the phantom
        // error_summary side-effect under keep_going=true if the node
        // has since transitioned out of a failed state.
        if let Some(failed_hash) = first_dep_failed
            && self.dag.node(&failed_hash).is_some_and(|s| {
                matches!(
                    s.status(),
                    DerivationStatus::Poisoned | DerivationStatus::DependencyFailed
                )
            })
        {
            self.handle_derivation_failure(build_id, &failed_hash).await;
            // handle_derivation_failure may have transitioned the build to
            // Failed. If so, don't dispatch.
            if self
                .builds
                .get(&build_id)
                .is_some_and(|b| b.state().is_terminal())
            {
                return Ok(ingest.event_rx);
            }
        }

        // Store cache-check is done (step 4 above, long since); signal
        // the boundary between cache-resolution and dispatch. Gateways
        // can surface this ("inputs resolved, N to build"). Fires even
        // on the all-cached path — "resolved to zero work" is still
        // resolved. P0294 ripped the Build CRD that originally wanted
        // this as a condition; kept for gateway STDERR_NEXT.
        self.events.emit(
            build_id,
            rio_proto::types::build_event::Event::InputsResolved(
                rio_proto::types::BuildInputsResolved {},
            ),
        );

        // Check if the build is already complete (all cache hits).
        // Log-and-continue on DB error: build is Active+committed in PG;
        // returning Err here would surface a failure for a build that
        // already succeeded, with no BuildCompleted event and no
        // schedule_terminal_cleanup. Mirrors check_build_completion at
        // build.rs.
        if cached_count == total_derivations {
            if let Err(e) = self.complete_build(build_id).await {
                error!(build_id = %build_id, error = %e,
                       "failed to persist build completion (build is Active; continuing)");
            }
        } else {
            // Complete/substitute any Ready node whose outputs already
            // exist (the store short-circuit; delivery itself is pull).
            self.sweep_ready_cached().await;
        }
        debug!(
            elapsed = ?t_total.elapsed(),
            nodes = ingest.nodes.len(),
            edges = ingest.edges_len,
            newly_inserted = ingest.merge_result.newly_inserted.len(),
            "handle_merge_dag total"
        );

        Ok(ingest.event_rx)
    }

    /// Steps 0–5 of merge: top-down root prune, DB build row, in-mem DAG
    /// merge, in-mem map inserts, cache-check, DB persist, → Active.
    ///
    /// All `?`-returnable error paths live here; on error any partial
    /// in-mem/DB state is rolled back (`cleanup_failed_merge`). On Ok
    /// the build is Active and committed — the caller's later DB writes
    /// are log-and-continue.
    async fn validate_and_ingest(
        &mut self,
        req: MergeDagRequest,
    ) -> Result<MergeIngest, ActorError> {
        let MergeDagRequest {
            build_id,
            tenant_id,
            priority_class,
            nodes,
            edges,
            options,
            keep_going,
            traceparent,
            jti,
            jwt_token,
        } = req;
        // Arch#13: proto→domain at the actor boundary. `MergeDagRequest`
        // keeps proto-typed `nodes`/`edges` so `actor/tests/` and
        // `rio-test-support` (b03 territory) can keep constructing it
        // unchanged; everything downstream of this line is wire-agnostic.
        let nodes = crate::domain::nodes_from_proto(nodes);
        let edges = crate::domain::edges_from_proto(edges);
        let mut t_phase = Instant::now();
        macro_rules! phase {
            ($name:literal) => {
                let elapsed = t_phase.elapsed();
                metrics::histogram!("rio_scheduler_merge_phase_seconds", "phase" => $name)
                    .record(elapsed.as_secs_f64());
                debug!(?elapsed, phase = $name, "merge phase");
                t_phase = Instant::now();
            };
        }

        // === Step 0: Top-down demand-set substitution check =========
        // r[impl sched.merge.substitute-topdown+12]
        // Before merging the full DAG, check if the DEMANDED
        // derivations' outputs are already available. The demand set is
        // the structural roots ∪ every node the gateway marked
        // explicitly_requested (a client-named target folded inside
        // another target's closure, hence not a structural root of the
        // combined submission). If ALL demanded nodes are cached, the
        // remaining deps are transitively unnecessary — prune the
        // submission to the demand set before merge. "Available" is
        // judged against the wanted outputs of this submission UNIONed,
        // for demanded nodes already in the DAG, with the live
        // effective wanted set of the pre-existing node — a
        // conservative superset of (at least as demanding as) what
        // post-merge classification will use for the shared node — and
        // the submission's own selector must itself resolve to a
        // declared output (see check_roots_topdown for both criteria),
        // so the prune cannot be more permissive than that
        // classification.
        //
        // This short-circuits the common case `rsb -L p#hello` where
        // hello is in cache.nixos.org: instead of eager-fetching ~700
        // dependency NARs (stdenv bootstrap chain), fetch just hello
        // and complete. Deps never enter the global DAG, so a later
        // build that needs them triggers its own cache-check.
        //
        // Falls through to the full DAG on any uncertainty (store
        // unreachable, partial demand-set cache, CA demanded nodes).
        // The fetch itself is deferred
        // (`r[sched.substitute.detached+5]`); on fetch failure the
        // build fails fast (`r[sched.merge.substitute-topdown+12]` —
        // resubmit re-probes). The existing check_cached_outputs at
        // step 4 handles fall-through correctly — this is a fast-path,
        // not a replacement.
        let (nodes, edges, topdown_fired, pruned_closure_parents) = match self
            .check_roots_topdown(&nodes, &edges, jwt_token.as_deref())
            .await
        {
            Some(demanded) => {
                debug!(
                    original_nodes = nodes.len(),
                    kept_nodes = demanded.len(),
                    pruned = nodes.len() - demanded.len(),
                    "top-down: all demanded nodes substitutable; pruning deps from submission"
                );
                metrics::counter!("rio_scheduler_topdown_prune_total").increment(1);
                // Parents of the ORIGINAL submission's edges: the nodes
                // whose dependency closure this prune is about to drop.
                // Only these (∩ the kept set, and only when the closure
                // classifier does not vouch for their existing children
                // — see closure_vouched) get the topdown_pruned marker
                // — a dep-less demanded leaf never had a closure to
                // drop, a from-source dispatch of it would succeed, and
                // marking it would only turn a routine substitute
                // failure into a wrongful terminal fail-fast. Resolved
                // path→hash via the original node list (edges carry
                // drv_paths on the wire).
                let path_to_hash: HashMap<&str, &str> = nodes
                    .iter()
                    .map(|n| (n.drv_path.as_str(), n.drv_hash.as_str()))
                    .collect();
                let parents: HashSet<String> = edges
                    .iter()
                    .filter_map(|e| path_to_hash.get(e.parent_drv_path.as_str()))
                    .map(|h| (*h).to_string())
                    .collect();
                (demanded, Vec::new(), true, parents)
            }
            None => (nodes, edges, false, HashSet::new()),
        };
        phase!("0-topdown-roots");

        // === Step 1: DB build row ==================================
        // If this fails, nothing is in memory; caller gets a clean error.
        self.db
            .insert_build(
                build_id,
                tenant_id,
                priority_class,
                keep_going,
                &options,
                jti.as_deref(),
            )
            .await?;
        phase!("1-db-build-row");

        // === Step 2: DAG merge (BEFORE in-memory map inserts) ========
        // If merge fails (cycle), nothing is in the actor's maps; only the
        // DB build row exists, which we best-effort delete. This ordering
        // prevents the leak where a cyclic submission left permanent entries
        // in build_events/builds with no cleanup scheduled.
        let merge_result = match self.dag.merge(build_id, &nodes, &edges, &traceparent) {
            Ok(r) => r,
            Err(e) => {
                // Best-effort: clean up the orphan build row.
                if let Err(db_e) = self.db.delete_build(build_id).await {
                    warn!(build_id = %build_id, error = %db_e,
                          "failed to delete orphan build row after DAG merge failure");
                }
                return Err(ActorError::Dag(e));
            }
        };
        let newly_inserted = &merge_result.newly_inserted;
        phase!("2-dag-merge-inmem");

        // === Step 3: In-memory map inserts ============================
        let event_rx = self.events.register(build_id);

        let build_info = BuildInfo::new_pending(
            build_id,
            tenant_id,
            priority_class,
            keep_going,
            options,
            nodes.iter().map(|n| n.drv_hash.as_str().into()).collect(),
        );
        self.builds.insert(build_id, build_info);

        // Index proto nodes by hash for efficient lookup during cache-check + transitions.
        let node_index: HashMap<&str, &crate::domain::DerivationNode> =
            nodes.iter().map(|n| (n.drv_hash.as_str(), n)).collect();

        // I-099/I-094: existing nodes (inserted by an earlier build, not
        // yet built) referenced by THIS submission. Re-probe them too —
        // upstream cache config may have changed since first insert.
        // Includes Poisoned/DependencyFailed (I-094): if the output now
        // exists upstream, the prior failure is moot. Excludes in-flight
        // (Assigned/Running) and terminal-done (Completed/Skipped/
        // Cancelled).
        let existing_reprobe: HashSet<DrvHash> = nodes
            .iter()
            .filter(|n| !newly_inserted.contains(n.drv_hash.as_str()))
            .filter_map(|n| {
                use DerivationStatus::*;
                let st = self.dag.node(&n.drv_hash)?.status();
                // Failed/DependencyFailed are reset by dag.merge →
                // newly_inserted today; listed for symmetry with the
                // I-094 transition arms (defense-in-depth if
                // is_retriable_on_resubmit ever bounds Failed).
                matches!(
                    st,
                    Created | Queued | Ready | Failed | Poisoned | DependencyFailed
                )
                .then(|| n.drv_hash.as_str().into())
            })
            .collect();
        let probe_set: HashSet<DrvHash> = newly_inserted
            .iter()
            .cloned()
            .chain(existing_reprobe.iter().cloned())
            .collect();
        phase!("3-inmem-maps");

        // === Step 4: Scheduler-side cache check (BEFORE DB persist) ====
        // Query the store for expected_output_paths of probe_set
        // derivations (newly-inserted + existing not-done re-probe).
        // If all outputs exist, skip straight to Completed.
        // This closes the TOCTOU window between the gateway's
        // FindMissingPaths and our merge (another build may have completed
        // the derivation in between).
        //
        // Err(StoreUnavailable) → circuit breaker is open (sustained store
        // outage). Roll back the merge and reject the whole SubmitBuild.
        // The alternative — queueing with 100% cache miss — causes a rebuild
        // avalanche once the store recovers.
        //
        // ORDERING: this check runs BEFORE persist_merge_to_db so the
        // rollback is in-memory only (no build_derivations FK rows to
        // cascade-delete). If it ran AFTER persist, delete_build would
        // silently fail the FK constraint, leaving orphan build rows
        // that recovery would resurrect.
        let (cached_hits, pending_substitute) = match self
            .check_cached_outputs(&probe_set, &node_index, jwt_token.as_deref())
            .await
        {
            Ok(r) => r,
            Err(e) => {
                error!(build_id = %build_id, error = %e, "cache-check circuit breaker open; rolling back merge");
                self.cleanup_failed_merge(build_id, merge_result).await;
                return Err(e);
            }
        };
        phase!("4-check-cached-outputs");

        // === Step 5: PG persist + → Active ============================
        // All remaining error-returning PG writes. On any error, roll
        // back the merge AND the map inserts AND delete the DB build row
        // so in-memory and DB state stay consistent. After this returns
        // Ok the build is committed (Active); later DB writes are
        // log-and-continue.
        // Substitution-replacement: the new_sub lane (newly-inserted
        // pending_substitute nodes). Computed BEFORE persist so the
        // in-tx job creation (adjudication PDQ-9) can ride the merge
        // transaction. Flag-off the lane is passed but the in-tx block
        // never reads it.
        let new_sub_lane: Vec<DrvHash> = pending_substitute
            .iter()
            .filter(|(h, _)| !existing_reprobe.contains(h))
            .map(|(h, _)| h.clone())
            .collect();
        // Phase B (PD-17): the REPROBE lane subset (pre-existing nodes
        // the probe re-classified substitutable), hoisted pre-tx — the
        // reconcile phase derives the same partition at its 6d slot, but
        // by then this transaction has already committed (the DF-7
        // ordering reality), so the flag-on job rows (origin=reprobe) +
        // the AS-5 durable reset must be computed HERE to ride batch 5.
        // Flag-off the lane is passed but never read in-tx, and the 6d
        // slot stays byte-identical (it spawns the as-built walks).
        let reprobe_sub_lane: Vec<DrvHash> = pending_substitute
            .iter()
            .filter(|(h, _)| existing_reprobe.contains(h))
            .map(|(h, _)| h.clone())
            .collect();
        let created_jobs = match self
            .persist_and_activate(
                build_id,
                &nodes,
                &edges,
                &merge_result,
                &pruned_closure_parents,
                tenant_id,
                &new_sub_lane,
                &reprobe_sub_lane,
            )
            .await
        {
            Ok(jobs) => jobs,
            Err(e) => {
                self.cleanup_failed_merge(build_id, merge_result).await;
                return Err(e);
            }
        };
        phase!("5-persist-and-activate");
        // Post-commit: feed the in-memory job view from the merge
        // transaction's created jobs (never inside the tx — the view is
        // a droppable cache; a rolled-back merge must leave no entry).
        self.note_created_materialization_jobs(&created_jobs);
        let _ = &mut t_phase; // last phase! write is intentionally unread

        // r[impl sched.merge.substitute-topdown+12]
        // r[impl sched.evidence.durability+2]
        // Stamp topdown_pruned on the kept (demanded) nodes only now
        // that the merge is committed (steps 4–5 can no longer fail).
        // The stamp is a cross-build-visible mutation of possibly
        // PRE-EXISTING nodes that rollback_merge does not revert (it is
        // not tracked in MergeResult, and no clearing site runs between
        // a rejected merge's rollback and a later fetch failure) —
        // stamping before the fallible cache-check / persist steps
        // would leak a rejected build's prune verdict onto a shared
        // childless node, and a later routine SubstituteComplete{ok=
        // false} for that node would take the fail-fast arm and
        // terminally fail innocent builds. Nothing reads the flag
        // between dag.merge and this point: its consumers are
        // handle_substitute_complete and the dispatch-time probes,
        // neither of which can run inside this handler — detached
        // fetches are spawned in reconcile_merged_state and dispatch
        // passes run on later commands, both after this returns.
        //
        // Only kept nodes whose dependency closure this prune actually
        // dropped — parents of the ORIGINAL submission's edges
        // (`pruned_closure_parents`) — are stamped, and only when the
        // closure classifier does NOT vouch for their existing DAG
        // children. A dep-less demanded leaf never had a closure to
        // drop, and a kept node whose child set is Vouched (≥1 child,
        // all Completed/Skipped, no closure hole) has its closure in
        // the store; neither needs the "must complete via substitution"
        // guard, and stamping them would only manufacture the stale
        // flag the fail-fast clear has to mop up. Children that are
        // present but UNBUILT do not exempt the node, and neither do
        // produced survivors of a closure-holed reap (their child set
        // is a truncated view of the pruned closure) — see
        // `closure_vouched` for the reap hazard and the deliberate
        // trade. Mirrors the row-level bind in persist_merge_to_db.
        if topdown_fired {
            for n in &nodes {
                if pruned_closure_parents.contains(n.drv_hash.as_str())
                    && !self.closure_vouched(&n.drv_hash)
                    && let Some(s) = self.dag.node_mut(&n.drv_hash)
                {
                    s.topdown_pruned = true;
                }
            }
        }
        // Parents of this submission's (post-prune) edges, deduped and
        // resolved while `edges` is still alive — only `edges_len`
        // otherwise survives into MergeIngest. These are the candidate
        // set for the caller's post-reconciliation `topdown_pruned`
        // clear pass. The clear DECISION is deliberately NOT taken
        // here: `verify_preexisting_completed` (reconcile phase 6c) has
        // not run yet, so a stale pre-existing Completed child that 6c
        // is about to demote would launder a clear computed against the
        // pre-verification view. A pruned merge has no edges, so this
        // is empty there.
        let edge_parent_hashes: Vec<DrvHash> = edges
            .iter()
            .filter_map(|e| self.dag.hash_for_path(&e.parent_drv_path).cloned())
            .collect::<HashSet<DrvHash>>()
            .into_iter()
            .collect();

        Ok(MergeIngest {
            build_id,
            edges_len: edges.len(),
            edge_parent_hashes,
            nodes,
            merge_result,
            event_rx,
            existing_reprobe,
            cached_hits,
            pending_substitute,
            jwt_token,
        })
    }

    /// Step-5 PG-persist tail: derivation/edge upsert, resubmit-poison
    /// clear, and Pending→Active. All `?`-returnable PG writes after
    /// the in-memory merge live here so `validate_and_ingest` has a
    /// single rollback point instead of three repeated
    /// `cleanup_failed_merge` arms. The caller does the rollback on
    /// `Err`; this fn does NOT touch in-memory state on failure.
    ///
    /// `topdown_pruned_parents`: on a pruned merge, the kept nodes
    /// whose dependency closure the prune dropped (parents of the
    /// ORIGINAL submission's edges); empty for a non-pruned merge.
    /// Their rows get `topdown_pruned = true` inside the persist
    /// transaction (the failover-safe counterpart of the post-commit
    /// in-memory stamp in `validate_and_ingest`), additionally gated on
    /// the closure classifier not vouching for the node's existing
    /// children (`closure_vouched`).
    ///
    /// The PG side of Pending→Active rides the SAME transaction
    /// (`activate_build_tx`, the last statement before commit), so a
    /// committed merge implies an Active build and an activation
    /// failure rolls back every side effect of the merge — including
    /// the `topdown_pruned` stamps — rather than leaving them persisted
    /// for a build the caller is rejecting. Only the in-memory
    /// `BuildInfo` transition remains here, applied after the commit
    /// succeeds. (The topdown fail-fast still clears the marker it
    /// consumes, as defense-in-depth for stale-flag shapes that do not
    /// involve this path at all.)
    /// `tenant_id` / `new_sub_lane` / `reprobe_sub_lane`:
    /// substitution-replacement inputs for the flag-gated in-tx job
    /// creation (adjudication PDQ-9; PD-17 for the reprobe lane).
    /// Returns the jobs that transaction created so the caller can feed
    /// the in-memory view POST-commit (empty flag-off).
    // The argument list mirrors the persist transaction's write set
    // (same precedent as persist_merge_to_db / the other multi-column
    // writers).
    #[allow(clippy::too_many_arguments)]
    async fn persist_and_activate(
        &mut self,
        build_id: Uuid,
        nodes: &[crate::domain::DerivationNode],
        edges: &[crate::domain::DerivationEdge],
        merge_result: &crate::dag::MergeResult,
        topdown_pruned_parents: &HashSet<String>,
        tenant_id: Option<Uuid>,
        new_sub_lane: &[DrvHash],
        reprobe_sub_lane: &[DrvHash],
    ) -> Result<Vec<super::materialize::CreatedJob>, ActorError> {
        let created_jobs = self.persist_merge_to_db(
            build_id,
            nodes,
            edges,
            &merge_result.newly_inserted,
            topdown_pruned_parents,
            tenant_id,
            new_sub_lane,
            reprobe_sub_lane,
        )
        .await
        .inspect_err(
            |e| error!(build_id = %build_id, error = %e, "merge DB persistence failed; rolling back"),
        )?;

        // I-169: PG-side poison clear for nodes that were reset by the
        // resubmit-retry path (Poisoned/Cancelled/Failed/DependencyFailed
        // → fresh state in `dag.merge`). `batch_upsert_derivations`' ON
        // CONFLICT does NOT touch poisoned_at, so without this PG keeps a
        // stale poison stamp. The status itself
        // is overwritten by `update_derivation_status_batch` below
        // (→ ready/queued), so recovery's `WHERE status='poisoned'` won't
        // resurrect it; this is about keeping poisoned_at
        // consistent for the NEXT poison cycle. The new cycle index is
        // carried by each reset node's `resubmit_reset` ledger row
        // (appended in the same transaction as this batched clear, so
        // the suffix cut and the per-cycle reset commit together) — that
        // row is what makes the resubmit bound survive leader failover.
        // Best-effort.
        // r[impl sched.db.clear-poison-batch+3]
        if !merge_result.reset_on_resubmit.is_empty()
            && let Err(e) = self
                .record_resubmit_resets(&merge_result.reset_on_resubmit)
                .await
        {
            warn!(
                count = merge_result.reset_on_resubmit.len(),
                error = %e,
                "failed to clear poison in PG for resubmit-reset nodes"
            );
        }

        // The PG half of Pending→Active already committed inside the
        // merge transaction (activate_build_tx); apply the in-memory
        // half now that the commit is durable. Pending→Active is always
        // a valid transition for a fresh build; debug_assert rather
        // than branch (a rejection here would be a bug in
        // BuildInfo::transition, not a recoverable error). Other build
        // transitions keep going through `transition_build` (DB-first
        // via the pool) — only the merge path is transactional.
        let applied = self
            .builds
            .get_mut(&build_id)
            .is_some_and(|b| b.transition(BuildState::Active).is_ok());
        debug_assert!(
            applied,
            "Pending→Active rejected on fresh build (BuildInfo::transition bug)"
        );
        let _ = applied;
        Ok(created_jobs)
    }

    /// Step 6 of merge: post-Active reconciliation. Transitions
    /// cache-hits to Completed, recomputes critical-path priorities,
    /// resets stale pre-existing Completed nodes, seeds initial
    /// Ready/Queued for newly-inserted nodes, and re-walks
    /// pre-existing nodes for cached/failed counting.
    ///
    /// DB write failures are log-and-continue (build is already Active;
    /// DB sync catches up on next status update or heartbeat
    /// reconciliation).
    // r[impl sched.merge.reconcile-order]
    async fn reconcile_merged_state(&mut self, ingest: &MergeIngest) -> MergeReconcile {
        let MergeIngest {
            nodes,
            merge_result,
            existing_reprobe,
            cached_hits,
            pending_substitute,
            jwt_token,
            ..
        } = ingest;
        let newly_inserted = &merge_result.newly_inserted;
        // Rebuild node_index here: it borrows from `nodes`, so it can't
        // live in MergeIngest (self-referential). Cheap: one iter pass.
        let node_index: HashMap<&str, &crate::domain::DerivationNode> =
            nodes.iter().map(|n| (n.drv_hash.as_str(), n)).collect();

        let mut t_phase = Instant::now();
        macro_rules! phase {
            ($name:literal) => {
                let elapsed = t_phase.elapsed();
                metrics::histogram!("rio_scheduler_merge_phase_seconds", "phase" => $name)
                    .record(elapsed.as_secs_f64());
                debug!(?elapsed, phase = $name, "merge phase");
                t_phase = Instant::now();
            };
        }

        // r[sched.merge.reconcile-order]: split pending_substitute by
        // lane. Reprobe-substitutable (pre-existing Poisoned/Failed/
        // DependencyFailed) MUST transition →Substituting BEFORE
        // seed_initial_states reads `any_co_owned_dep_terminally_failed`
        // (6d); newly-inserted substitutable nodes need seed to put them
        // at Created/Queued/Ready first (6g). Partition keys on
        // existing_reprobe (the only set whose members can BE Poisoned
        // here — newly_inserted nodes are at Created).
        let (reprobe_sub, new_sub): (Vec<_>, Vec<_>) = pending_substitute
            .iter()
            .cloned()
            .partition(|(h, _)| existing_reprobe.contains(h));

        // === Phase ordering invariant ==============================
        // All dep-state CORRECTIONS (6a cache-hit→Completed, 6c stale-
        // Completed reset, 6d reprobe-Poisoned→Substituting) MUST
        // complete before any dependent VERDICT that reads dep status
        // (6e seed_initial_states, 6f reprobe_unlocked Queued→Ready).
        // Violations: bug_089 (6a's advance fired before 6c reset),
        // bug_132 (6e seed fired before 6d Poisoned→Substituting).

        let (mut cached_count, deferred_hits, other_builds, reprobe_unlocked) =
            self.apply_cached_hits(ingest, &node_index).await;
        phase!("6a-cached-hits-loop");

        // Compute critical-path priorities for newly-inserted nodes.
        // Done AFTER cache-hit transitions so completed derivations
        // are correctly excluded from their parents' max-child (a
        // cached dep doesn't block anything — it's done).
        //
        // This sets est_duration (SLA T_min, default 60s on miss) +
        // priority (bottom-up) for new nodes, and propagates to existing
        // nodes if the new subgraph raises their priority. The ready
        // queue reads these for BinaryHeap ordering.
        crate::critical_path::compute_initial(
            &mut self.dag,
            &self.sla_estimator,
            &self.builds,
            newly_inserted,
        );
        phase!("6b-critical-path");

        // I-047: pre-existing Completed nodes may have stale output_paths
        // (GC deleted the output between the node's original completion and
        // this merge). Verify outputs exist BEFORE compute_initial_states —
        // otherwise newly-inserted dependents would be unlocked against a
        // dep whose output is gone, and the worker fails on isValidPath.
        // Reset stale nodes to Ready; they re-dispatch and re-complete.
        // r[impl sched.merge.stale-completed-verify+5]
        let stale_reset = self
            .verify_preexisting_completed(nodes, newly_inserted, cached_hits, jwt_token.as_deref())
            .await;
        phase!("6c-verify-preexisting");

        // Reprobe-substitutable lane FIRST: a hard-Poisoned node whose
        // output is now upstream-substitutable must have its status
        // corrected BEFORE seed_initial_states reads
        // `any_co_owned_dep_terminally_failed` for its dependents (the
        // merging build co-owns its whole submission, so the scoped check
        // fires for these). Otherwise a newly-inserted dependent A of a
        // reprobe-Poisoned B is marked DependencyFailed against B's stale
        // Poisoned status, first_dep_failed=Some(A), and !keep_going
        // builds fail-fast while B's substitution is undecided.
        if !reprobe_sub.is_empty() {
            // r[impl sched.materialize.job]
            // PD-17 + AS-5 (Phase B): the reprobe lane's job rows
            // (origin=reprobe) and the durable AS-5 reset already
            // rode the merge transaction (batch 5 — committed before
            // this slot runs; the DF-7 ordering). Apply the matching
            // IN-MEMORY status correction here, in the 6d slot,
            // before seed_initial_states (6e) reads dep statuses —
            // the same phase-ordering invariant the deleted
            // Substituting transition carried (bug_089/bug_132). No
            // walk spawns: this closed the FIRST of the two
            // merge-lane spawn sites (the stale-Completed verify's
            // own site closed at PD-18); the spawner itself is now
            // deleted.
            self.apply_reprobe_reset_in_memory(&reprobe_sub).await;
        }
        phase!("6d-reprobe-reset");

        // Compute initial states for the remaining (non-cached) newly-inserted
        // derivations. Cached derivations above are now Completed, so their
        // dependents will correctly be computed as Ready here.
        let remaining_new: HashSet<DrvHash> = newly_inserted
            .iter()
            .filter(|h| !cached_hits.contains_key(h.as_str()) || deferred_hits.contains(*h))
            .cloned()
            .collect();
        let mut first_dep_failed = self.seed_initial_states(&remaining_new).await;
        phase!("6e-seed-initial-states");

        // I-099: advance pre-existing Queued dependents of re-probe
        // hits (collected in 6a). Runs AFTER 6c so a sibling dep that
        // 6c reset Completed→Ready is no longer counted as Completed:
        // re-check `all_deps_completed` against the post-6c DAG and
        // skip if it's now false (D stays Queued; find_newly_ready
        // picks it up when the reset dep re-completes). The Queued
        // re-check guards the I-099 fixed-point Completed-capture as
        // before.
        for ready_hash in reprobe_unlocked {
            if self
                .dag
                .node(&ready_hash)
                .is_some_and(|s| s.status() == DerivationStatus::Queued)
                && self.dag.all_deps_completed(&ready_hash)
                && self
                    .dag
                    .node_mut(&ready_hash)
                    .is_some_and(|s| s.transition(DerivationStatus::Ready).is_ok())
            {
                match self
                    .db
                    .update_derivation_status(
                        &ready_hash,
                        DerivationStatus::Ready,
                        None,
                        self.serving_generation(),
                    )
                    .await
                {
                    Ok(crate::db::FencedWrite::Fenced) => {
                        self.note_fenced_evidence_write("re-probe-unlocked Ready persist");
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!(drv_hash = %ready_hash, error = %e,
                              "failed to persist re-probe-unlocked Ready status");
                    }
                }
                self.push_ready(ready_hash);
            }
        }
        phase!("6f-reprobe-unlocked");

        // Newly-inserted substitutable lane: nodes are at Created/
        // Queued/Ready (via seed_initial_states above) and stay there.
        if !new_sub.is_empty() {
            // r[impl sched.materialize.job]
            // Substitution-replacement: this lane's jobs were created
            // INSIDE the merge transaction (adjudication PDQ-9); the
            // job row is the in-flight marker and the nodes stay
            // Ready (claimable by store replicas). (The reprobe lane
            // (6d above) and the stale-Completed verify (6c) are
            // likewise job-routed since PD-17/PD-18 — the T-5.3
            // audit's criterion-3 statement made structural: the walk
            // spawner no longer exists.)
            debug!(
                count = new_sub.len(),
                "new_sub lane routed to materialization jobs"
            );
        }
        phase!("6g-new-sub-jobs");

        // Pre-existing Ready nodes whose interest set grew: their
        // critical-path priority may have risen (compute_initial above
        // re-walked over newly_inserted, but a pre-existing Ready node
        // already in the queue under its OLD priority isn't touched by
        // that walk). Re-push so a higher-priority build's shared dep
        // doesn't sit behind lower-priority work. Re-push at lower
        // priority is harmless: ReadyQueue's per-session generation
        // tagging keeps the higher in-session entry winning, and prior-
        // session leftovers are skipped on pop.
        for hash in &merge_result.interest_added {
            if self
                .dag
                .node(hash)
                .is_some_and(|s| s.status() == DerivationStatus::Ready)
            {
                self.push_ready(hash.clone());
            }
        }

        // Pre-existing nodes that weren't newly-inserted, stale-reset,
        // or re-probe-cached: count Completed/Skipped as cached, and
        // surface Poisoned/DependencyFailed for fail-fast.
        let (preexisting_cached, preexisting_failed) =
            self.reconcile_preexisting(ingest, &stale_reset).await;
        cached_count += preexisting_cached;
        if first_dep_failed.is_none() {
            first_dep_failed = preexisting_failed;
        }
        phase!("6h-preexisting-nodes-loop");
        let _ = &mut t_phase; // last phase! write is intentionally unread

        MergeReconcile {
            cached_count,
            first_dep_failed,
            other_builds,
        }
    }

    /// Step-6a body: apply `ingest.cached_hits` — transition each hit
    /// to `Completed`, set `output_paths`, clear retry state for
    /// previously-failed re-probes, batch-persist `Completed`, and
    /// emit `DerivationCached` events. Returns `(cached_count,
    /// deferred_hits, other_builds, reprobe_unlocked)` —
    /// `deferred_hits` are cache-hits whose inputDrvs are incomplete
    /// (closure invariant), so the caller seeds them as Queued
    /// instead; `other_builds` are builds other than `ingest.build_id`
    /// interested in a re-probe completion (caller fans out
    /// count-update + completion-check); `reprobe_unlocked` are
    /// pre-existing Queued dependents of re-probe hits — the CALLER
    /// transitions them Queued→Ready AFTER `verify_preexisting_
    /// completed` (6c) has reset stale-Completed deps, re-checking
    /// `all_deps_completed` against the post-reset DAG
    /// (r[sched.merge.reconcile-order]).
    ///
    /// All I/O here is best-effort log-and-continue (build is Active).
    /// Kept as a `&mut self` method (not a free decision fn) because
    /// the per-hit transition mutates DAG node status, which
    /// `seed_initial_states` reads downstream — the two are
    /// intrinsically sequenced.
    async fn apply_cached_hits(
        &mut self,
        ingest: &MergeIngest,
        node_index: &HashMap<&str, &crate::domain::DerivationNode>,
    ) -> (u32, HashSet<DrvHash>, HashSet<Uuid>, Vec<DrvHash>) {
        let MergeIngest {
            build_id,
            existing_reprobe,
            cached_hits,
            ..
        } = ingest;
        let build_id = *build_id;
        let mut cached_count = 0u32;
        let mut reprobe_unlocked: Vec<DrvHash> = Vec::new();
        // Re-probe completions that other builds were waiting on. The
        // event emission below + caller's update_build_counts are
        // scoped to ingest.build_id; without fanning out, an earlier
        // build B1 with X sitting Ready never sees X complete via
        // B2's re-probe — completed_count stays 0, dispatch_ready
        // silently drops the now-Completed entry,
        // check_build_completion(B1) never fires → B1 hangs Active.
        // r[sched.merge.dedup]: "all interested builds are notified".
        let mut reprobe_completed: Vec<DrvHash> = Vec::new();
        // I-139: collect for batched Completed-status + path_tenants
        // updates after the loop instead of N sequential round-trips.
        // clear_poison stays per-hit (Poisoned-only, rare).
        let mut completed_batch: Vec<DrvHash> = Vec::with_capacity(cached_hits.len());
        // Closure invariant: a cache-hit's output references ⊆ its
        // inputDrv outputs' closure (Nix referential integrity). If A
        // is marked Completed while inputDrv B is still being
        // built/substituted, A's dependents dispatch with B's output
        // absent → builder compute_input_closure drops B → ENOENT
        // (e.g. rustc-wrapper Completed via 2KB substitute, but
        // rustc-1.94.0 timed out at eager_substitute_fetch and is
        // building → git dispatches → rustc not in JIT allowlist).
        // Fixed-point: apply Completed only when all_deps_completed;
        // re-walk until no progress (handles chains of hits). Hits
        // whose deps never complete here are returned in
        // deferred_hits: newly-inserted ones fall through to
        // seed_initial_states as Queued; pre-existing ones in a
        // failed state are reset to Queued in the post-loop stanza
        // below (I-094 deferred lane). Either way they BUILD when
        // deps complete — correctness over the cache-hit shortcut
        // (wrapper drvs are cheap to rebuild).
        let mut worklist: Vec<DrvHash> = cached_hits.keys().cloned().collect();
        let deferred_hits: HashSet<DrvHash> = loop {
            let before = worklist.len();
            let mut still_pending: Vec<DrvHash> = Vec::new();
            let pass = std::mem::take(&mut worklist);
            for drv_hash in &pass {
                if !self.dag.all_deps_completed(drv_hash) {
                    still_pending.push(drv_hash.clone());
                    continue;
                }
                let output_paths = &cached_hits[drv_hash];
                let Some(node) = node_index.get(drv_hash.as_str()) else {
                    continue;
                };
                let is_reprobe = existing_reprobe.contains(drv_hash);
                let from_status = {
                    let Some(state) = self.dag.node_mut(drv_hash) else {
                        continue;
                    };
                    let from = match state.transition(DerivationStatus::Completed) {
                        Ok(f) => f,
                        Err(e) => {
                            warn!(drv_hash = %drv_hash, error = %e, "cache-hit →Completed transition failed");
                            continue;
                        }
                    };
                    // GAP-4 fix: for floating-CA nodes this is the REALIZED path
                    // from the realisations table (not the [""] placeholder from
                    // expected_output_paths). For IA nodes it's expected_output_paths
                    // (same as before). check_cached_outputs populates the right one.
                    state.output_paths = output_paths.clone();
                    // A merge-time cached-hit completion ends any
                    // substitution chain that left this node pre-dispatch
                    // (e.g. a downgraded walk reverted to Ready and the
                    // trigger-wanting build went terminal before the delta
                    // re-walk ran). The spent-forgiveness set is
                    // chain-scoped — clear it like every other completion
                    // site does.
                    state.never_forgive_paths.clear();
                    // I-099/I-094: re-probe hit on a previously-failed
                    // node — failure history is moot now we have the
                    // output. The retry-state reset is the
                    // `cache_hit_clear` ledger row appended below for
                    // the Poisoned/DependencyFailed cases (the fold's
                    // reset arm zeroes the counters and the cached view
                    // refreshes when the row lands); a `Failed`-from hit
                    // appends no reset row, exactly as it never got a PG
                    // poison clear — its (uncut) suffix keeps deciding.
                    from
                };
                cached_count += 1;
                let source = if is_reprobe { "reprobe" } else { "scheduler" };
                metrics::counter!("rio_scheduler_cache_hits_total", "source" => source)
                    .increment(1);

                // I-094: PG-side poison clear (status='created', NULLs
                // poisoned_at/retry_count/failed_builders) so recovery
                // doesn't resurrect the poison. Best-effort like the
                // status update below. 1a: the `cache_hit_clear` reset
                // row joins the clear in one transaction.
                if matches!(
                    from_status,
                    DerivationStatus::Poisoned | DerivationStatus::DependencyFailed
                ) {
                    let reset_row = self.reset_row_for(
                        drv_hash,
                        crate::state::OutcomeClass::CacheHitClear,
                        crate::state::ReportingParty::Scheduler,
                    );
                    if let Err(e) = self
                        .record_reset_with_clear_poison(drv_hash, reset_row)
                        .await
                    {
                        warn!(drv_hash = %drv_hash, error = %e,
                          "failed to clear poison in PG after re-probe cache hit");
                    }
                }
                completed_batch.push(drv_hash.clone());

                if is_reprobe {
                    info!(drv_hash = %drv_hash, from = ?from_status,
                      "re-probe: existing not-done node found in upstream cache");
                    // Existing dependents in Queued may now advance.
                    // Collected here, transitioned by the CALLER after
                    // 6c (stale-reset) so the all_deps_completed
                    // re-check sees post-reset dep status.
                    // Newly-inserted dependents are handled by
                    // compute_initial_states; this catches the
                    // pre-existing ones.
                    reprobe_unlocked.extend(self.dag.find_newly_ready(drv_hash));
                    reprobe_completed.push(drv_hash.clone());
                }

                self.events.emit(
                    build_id,
                    rio_proto::types::build_event::Event::Derivation(
                        rio_proto::types::DerivationEvent::cached(
                            node.drv_path.clone(),
                            output_paths.clone(),
                        ),
                    ),
                );
                // r[impl sched.gc.path-tenants-upsert]
                // Cache hit during merge: path already in store, new
                // tenant needs attribution so GC retains under their
                // policy. Batched after the fixed-point loop —
                // completed_batch already collects exactly these.
            }
            if still_pending.is_empty() || still_pending.len() == before {
                break still_pending.into_iter().collect();
            }
            worklist = still_pending;
        };
        if !deferred_hits.is_empty() {
            debug!(
                count = deferred_hits.len(),
                "cache-hits deferred to build (inputDrvs incomplete; closure invariant)"
            );
            metrics::counter!("rio_scheduler_cache_hit_deferred_total")
                .increment(deferred_hits.len() as u64);
        }
        // I-094 deferred lane: a pre-existing Poisoned/Failed/
        // DependencyFailed node whose output is now locally present
        // but whose inputDrv is in-flight got deferred above (no
        // transition). It is NOT newly_inserted (at-limit ⇒
        // is_retriable_on_resubmit=false) so seed_initial_states
        // skips it; reconcile_preexisting skips ALL cached_hits keys
        // including deferred. When the dep later completes,
        // find_newly_ready only walks Queued parents — so a Poisoned
        // X stays Poisoned forever despite output present. Reset to
        // Queued here (failure history is moot; we have the output);
        // dispatch-time batch_probe_cached_ready re-finds the hit.
        for h in &deferred_hits {
            if !existing_reprobe.contains(h) {
                continue;
            }
            let Some(state) = self.dag.node_mut(h) else {
                continue;
            };
            let from = state.status();
            if !matches!(
                from,
                DerivationStatus::Poisoned
                    | DerivationStatus::Failed
                    | DerivationStatus::DependencyFailed
            ) {
                continue;
            }
            if let Err(e) = state.transition(DerivationStatus::Queued) {
                warn!(drv_hash = %h, error = %e,
                      "deferred re-probe →Queued rejected; node stays {from:?}");
                continue;
            }
            info!(drv_hash = %h, ?from,
                  "deferred re-probe: pre-existing failed node's output present \
                   but inputDrv in-flight; reset to Queued (failure history moot)");
            if matches!(
                from,
                DerivationStatus::Poisoned | DerivationStatus::DependencyFailed
            ) {
                // 1a: `cache_hit_clear` reset row + poison clear in one
                // transaction (same shape as apply_cached_hits above).
                let reset_row = self.reset_row_for(
                    h,
                    crate::state::OutcomeClass::CacheHitClear,
                    crate::state::ReportingParty::Scheduler,
                );
                if let Err(e) = self.record_reset_with_clear_poison(h, reset_row).await {
                    warn!(drv_hash = %h, error = %e,
                          "failed to clear poison in PG after deferred re-probe reset");
                }
            }
            match self
                .db
                .update_derivation_status(
                    h,
                    DerivationStatus::Queued,
                    None,
                    self.serving_generation(),
                )
                .await
            {
                Ok(crate::db::FencedWrite::Fenced) => {
                    self.note_fenced_evidence_write("deferred re-probe Queued reset persist");
                }
                Ok(_) => {}
                Err(e) => {
                    warn!(drv_hash = %h, error = %e,
                          "failed to persist deferred re-probe Queued reset");
                }
            }
        }
        if !completed_batch.is_empty() {
            let hashes: Vec<&str> = completed_batch.iter().map(DrvHash::as_str).collect();
            match self
                .db
                .update_derivation_status_batch(
                    &hashes,
                    DerivationStatus::Completed,
                    self.serving_generation(),
                )
                .await
            {
                Ok(crate::db::FencedWrite::Fenced) => {
                    self.note_fenced_evidence_write("cache-hit Completed batch persist");
                }
                Ok(_) => {}
                Err(e) => {
                    warn!(count = hashes.len(), error = %e,
                          "failed to persist cache-hit Completed status batch");
                }
            }
            self.upsert_path_tenants_for_batch(&completed_batch).await;
        }
        // Fan-out: collect other builds interested in re-probe-
        // completed nodes + emit DerivationCached to each. Caller
        // does update_build_counts + check_build_completion per build.
        let mut other_builds: HashSet<Uuid> = HashSet::new();
        for h in &reprobe_completed {
            let (drv_path, output_paths) = match self.dag.node(h) {
                Some(s) => (s.drv_path().to_string(), s.output_paths.clone()),
                None => continue,
            };
            for b in self.get_interested_builds(h) {
                if b == build_id {
                    continue;
                }
                self.events.emit(
                    b,
                    rio_proto::types::build_event::Event::Derivation(
                        rio_proto::types::DerivationEvent::cached(
                            drv_path.clone(),
                            output_paths.clone(),
                        ),
                    ),
                );
                other_builds.insert(b);
            }
        }
        (cached_count, deferred_hits, other_builds, reprobe_unlocked)
    }

    // r[impl sched.materialize.job]
    /// PD-17 + AS-5 (Phase B), the 6d slot's flag-on body: the IN-MEMORY
    /// half of the reprobe-lane reset. The durable half — the job rows
    /// (origin=reprobe), the poison clear, and the `poison_cleared`
    /// budget-reset ledger row — rode the merge transaction's batch 5
    /// and is already committed when this runs.
    ///
    /// Failed-status reprobe nodes (Poisoned/DependencyFailed/Failed —
    /// their prior failure is moot now the output is substitutable
    /// again) take the I-094 deferred-reprobe edge to `Queued`, then
    /// promote to `Ready` when every dep is complete (the dep-derived
    /// non-failed status AS-5 prescribes), and the corrected statuses
    /// are batch-persisted (the post-commit half — the same pattern as
    /// the as-built walk's `persist_status_batch(Substituting)`).
    /// Benign-status reprobe nodes (Created/Queued/Ready) need no
    /// correction: the committed job row is their in-flight marker.
    ///
    /// MUST run before 6e (`seed_initial_states` reads dep statuses) —
    /// the bug_089/bug_132 phase-ordering invariant the as-built
    /// Substituting transition carried.
    async fn apply_reprobe_reset_in_memory(&mut self, reprobe_sub: &[(DrvHash, Vec<String>)]) {
        let mut corrected_queued: Vec<DrvHash> = Vec::new();
        let mut corrected_ready: Vec<DrvHash> = Vec::new();
        for (drv_hash, _) in reprobe_sub {
            let Some(state) = self.dag.node_mut(drv_hash) else {
                continue;
            };
            let from = state.status();
            if !matches!(
                from,
                DerivationStatus::Poisoned
                    | DerivationStatus::DependencyFailed
                    | DerivationStatus::Failed
            ) {
                // Benign statuses: the job row is the in-flight marker;
                // nothing to correct.
                continue;
            }
            // The I-094 deferred-reprobe edge (failed → Queued). The
            // durable budget reset (poison_cleared row) already rode
            // batch 5; the in-memory retry view follows the as-built
            // I-094 deferred-lane posture (refreshed from the durable
            // ledger on the next reload).
            if let Err(e) = state.transition(DerivationStatus::Queued) {
                warn!(drv_hash = %drv_hash, error = %e,
                      "reprobe AS-5 reset to Queued rejected; node stays {from:?}");
                continue;
            }
            info!(drv_hash = %drv_hash, ?from,
                  "reprobe lane (flag-on): AS-5 reset applied; the origin=reprobe job is the \
                   in-flight marker (no walk spawned)");
            // Promote to the dep-derived status: Ready when every dep
            // is complete (a dep-less node is vacuously complete).
            if self.dag.all_deps_completed(drv_hash)
                && self
                    .dag
                    .node_mut(drv_hash)
                    .is_some_and(|s| s.transition(DerivationStatus::Ready).is_ok())
            {
                self.push_ready(drv_hash.clone());
                corrected_ready.push(drv_hash.clone());
            } else {
                corrected_queued.push(drv_hash.clone());
            }
        }
        // Persist the corrected statuses (post-commit, batched per
        // status — the durable rows from batch 5 left these at
        // 'created'; this is the same persist the as-built walk did
        // with Substituting).
        if !corrected_queued.is_empty() {
            let refs: Vec<&str> = corrected_queued.iter().map(DrvHash::as_str).collect();
            self.persist_status_batch(&refs, DerivationStatus::Queued)
                .await;
        }
        if !corrected_ready.is_empty() {
            let refs: Vec<&str> = corrected_ready.iter().map(DrvHash::as_str).collect();
            self.persist_status_batch(&refs, DerivationStatus::Ready)
                .await;
        }
    }

    /// Step-6h body: walk pre-existing nodes (not newly-inserted, not
    /// stale-reset, not already counted as a cache-hit) and classify.
    /// `Completed`/`Skipped` → bump cached count + path-tenants upsert;
    /// `Poisoned`/`DependencyFailed` → surface first failed hash so the
    /// caller can `handle_derivation_failure` (fail-fast). Other
    /// statuses are in-flight from another build and need no action.
    ///
    /// Without the failure arm, a single-node resubmit of a
    /// still-poisoned derivation (within TTL, no ClearPoison yet)
    /// leaves the build Active with completed=0, failed=0, total=1 —
    /// `check_build_completion` never fires.
    async fn reconcile_preexisting(
        &mut self,
        ingest: &MergeIngest,
        stale_reset: &HashSet<String>,
    ) -> (u32, Option<DrvHash>) {
        let MergeIngest {
            nodes,
            merge_result,
            cached_hits,
            pending_substitute,
            ..
        } = ingest;
        let newly_inserted = &merge_result.newly_inserted;
        let pending_sub: HashSet<&str> =
            pending_substitute.iter().map(|(h, _)| h.as_str()).collect();
        let mut first_failed: Option<DrvHash> = None;
        let mut preexisting_completed: Vec<DrvHash> = Vec::new();
        for node in nodes {
            if newly_inserted.contains(node.drv_hash.as_str()) {
                continue;
            }
            // Stale-reset nodes are now Ready (re-queued above); skip
            // here so they don't double-count as cached or trigger
            // upsert_path_tenants on the GC'd path.
            if stale_reset.contains(node.drv_hash.as_str()) {
                continue;
            }
            // I-099: re-probe hits were handled in apply_cached_hits
            // (either counted + emitted, or deferred-and-reset to
            // Queued). Don't double-count or fail-fast on them.
            if cached_hits.contains_key(node.drv_hash.as_str()) {
                continue;
            }
            // I-094 substitutable lane: re-probe hits carry pending
            // materialization jobs (their reset puts them at
            // Queued/Ready); don't fail-fast on them. Parallel to the
            // cached_hits skip above.
            if pending_sub.contains(node.drv_hash.as_str()) {
                continue;
            }
            let Some(state) = self.dag.node(&node.drv_hash) else {
                continue;
            };
            match state.status() {
                // Skipped counts as cached: output already in store
                // from a prior CA-cutoff. Same semantics as a cache
                // hit from the build's perspective.
                DerivationStatus::Completed | DerivationStatus::Skipped => {
                    // r[impl sched.event.derivation-terminal]
                    // Pre-existing Completed/Skipped is a terminal
                    // cached-equivalent for THIS build's WatchBuild —
                    // emit so the client doesn't see it stuck Queued.
                    // `apply_cached_hits` (the upstream-cache analogue)
                    // already does this; this arm covers the
                    // already-in-DAG case.
                    self.events.emit(
                        ingest.build_id,
                        rio_proto::types::build_event::Event::Derivation(
                            rio_proto::types::DerivationEvent::cached(
                                node.drv_path.clone(),
                                state.output_paths.clone(),
                            ),
                        ),
                    );
                    // r[impl sched.gc.path-tenants-upsert]
                    // Pre-existing Completed/Skipped merged from
                    // another build: this build's tenant now also
                    // wants those output_paths retained. The node's
                    // interested_builds already includes this build
                    // (merge() added it); output_paths was set when
                    // the node originally completed. Batched after
                    // the loop — I-139 shape: a 5k-node resubmit
                    // with 4800 prior-Completed nodes was ~8.6s of
                    // sequential PG awaits in the actor loop here.
                    preexisting_completed.push(node.drv_hash.as_str().into());
                }
                DerivationStatus::Poisoned | DerivationStatus::DependencyFailed => {
                    first_failed.get_or_insert_with(|| node.drv_hash.as_str().into());
                    debug!(
                        drv_hash = %node.drv_hash,
                        status = ?state.status(),
                        "pre-existing node already failed; build will fail fast"
                    );
                }
                _ => {}
            }
        }
        let cached = preexisting_completed.len() as u32;
        if cached > 0 {
            metrics::counter!("rio_scheduler_cache_hits_total", "source" => "existing")
                .increment(u64::from(cached));
            self.upsert_path_tenants_for_batch(&preexisting_completed)
                .await;
        }
        (cached, first_failed)
    }

    /// Compute initial Ready/Queued/DependencyFailed for `remaining_new`
    /// (newly-inserted minus cache-hits), transition + push_ready in
    /// memory, then persist as three batched ANY($1::text[]) updates.
    /// Returns the first DependencyFailed hash (if any) so the caller
    /// can fail-fast / handle_derivation_failure.
    ///
    /// I-139: this used to be one update_derivation_status round-trip
    /// PER NODE — for a 153k-node fresh DAG that's 153k sequential PG
    /// awaits inside the single-threaded actor (~278s at ~1.8ms RTT).
    /// Persist failures don't abort (build is already Active).
    async fn seed_initial_states(&mut self, remaining_new: &HashSet<DrvHash>) -> Option<DrvHash> {
        let initial_states = self.dag.compute_initial_states(remaining_new);
        let mut first_dep_failed: Option<DrvHash> = None;
        let mut by_status: [Vec<DrvHash>; 3] = Default::default();
        let bucket = |s: DerivationStatus| match s {
            DerivationStatus::Ready => 0,
            DerivationStatus::Queued => 1,
            DerivationStatus::DependencyFailed => 2,
            _ => unreachable!("compute_initial_states only emits Ready/Queued/DependencyFailed"),
        };

        for (drv_hash, target_status) in &initial_states {
            let Some(state) = self.dag.node_mut(drv_hash) else {
                continue;
            };
            match target_status {
                DerivationStatus::Ready => {
                    // Transition created -> queued -> ready
                    if let Err(e) = state.transition(DerivationStatus::Queued) {
                        warn!(drv_hash = %drv_hash, error = %e, "Created->Queued transition failed");
                    }
                    if let Err(e) = state.transition(DerivationStatus::Ready) {
                        warn!(drv_hash = %drv_hash, error = %e, "Queued->Ready transition failed");
                    }
                    // push_ready computes critical-path priority +
                    // interactive boost. Old push_front/push_back
                    // split is now a number, not a position.
                    self.push_ready(drv_hash.clone());
                }
                DerivationStatus::Queued => {
                    if let Err(e) = state.transition(DerivationStatus::Queued) {
                        warn!(drv_hash = %drv_hash, error = %e, "Created->Queued transition failed");
                    }
                }
                DerivationStatus::DependencyFailed => {
                    if let Err(e) = state.transition(DerivationStatus::DependencyFailed) {
                        warn!(drv_hash = %drv_hash, error = %e, "Created->DependencyFailed transition failed");
                    }
                    first_dep_failed.get_or_insert_with(|| drv_hash.clone());
                    debug!(
                        drv_hash = %drv_hash,
                        "dep already poisoned at merge; marking DependencyFailed"
                    );
                }
                _ => continue,
            }
            by_status[bucket(*target_status)].push(drv_hash.clone());
        }
        for (i, status) in [
            DerivationStatus::Ready,
            DerivationStatus::Queued,
            DerivationStatus::DependencyFailed,
        ]
        .into_iter()
        .enumerate()
        {
            let hashes: Vec<&str> = by_status[i].iter().map(DrvHash::as_str).collect();
            if hashes.is_empty() {
                continue;
            }
            match self
                .db
                .update_derivation_status_batch(&hashes, status, self.serving_generation())
                .await
            {
                Ok(crate::db::FencedWrite::Fenced) => {
                    self.note_fenced_evidence_write("initial-state status batch persist");
                }
                Ok(_) => {}
                Err(e) => {
                    error!(
                        status = ?status, count = hashes.len(), error = %e,
                        "failed to persist initial-state status batch \
                         (build is Active; continuing)"
                    );
                }
            }
        }
        first_dep_failed
    }

    /// Verify that pre-existing `Completed` nodes' outputs still exist
    /// in the store. Reset stale ones to `Ready` for re-dispatch.
    ///
    /// Returns the set of `drv_hash` values that were reset (so the
    /// caller can skip them in the cached-count loop).
    ///
    /// I-047: a FOD output is content-addressed and shared across
    /// builds. GC may delete it under one tenant's retention policy
    /// while another tenant's DAG still has the node marked
    /// `Completed`. When a new build merges, `compute_initial_states`
    /// would unlock the new build's dependents against the stale dep,
    /// and the worker fails on `isValidPath`.
    ///
    /// **Fail-open:** if the store is unreachable, skip verification
    /// and treat existing `Completed` as valid. The bug is rare (GC
    /// race); blocking merge on store availability would be a worse
    /// regression than the original bug. Feeds the circuit-breaker so
    /// a stall here counts toward opening, but the fail-open RETURN
    /// is unconditional per `r[sched.breaker.cache-check]` — this is
    /// a best-effort correctness check, not an availability gate like
    /// `check_cached_outputs`.
    async fn verify_preexisting_completed(
        &mut self,
        nodes: &[crate::domain::DerivationNode],
        newly_inserted: &HashSet<DrvHash>,
        cached_hits: &HashMap<DrvHash, Vec<String>>,
        jwt_token: Option<&str>,
    ) -> HashSet<String> {
        // Collect (drv_hash, output_paths, unwanted_paths) for
        // pre-existing Completed OR Skipped nodes in this merge.
        // Skipped carries real output_paths (stamped at completion) and
        // unlocks dependents via all_deps_completed; a GC'd Skipped
        // output bypasses I-047 and dispatches dependents against a
        // missing path. Skip nodes with empty output_paths — nothing to
        // verify (shouldn't happen for a real done node, but defensive
        // against test fixtures).
        //
        // `unwanted_paths` is the set of declared output paths that NO
        // LIVE interested build wants: a recorded output whose absence
        // is forgiven by the demand-driven criterion. The wanted source
        // is the live effective set (`effective_wanted` over the node's
        // interested builds' contributions), falling back to the stored
        // node-level union (`wanted_output_names`) when that set is
        // unavailable (no live interested builds, or a live build's
        // contribution is unknown — post-failover). The submitting
        // build's wants are already included: `dag.merge`
        // (validate_and_ingest step 2) recorded this submission's
        // contribution + interest and step 3 inserted its live
        // BuildInfo, both before reconcile_merged_state calls this
        // verifier — so reading the contribution map alone suffices. A
        // Completed node whose only missing recorded output is one no
        // live build wants must NOT reset (it was legitimately never
        // substituted — resetting it on every merge would ping-pong
        // Completed↔Ready forever). A build that wants a previously-
        // unwanted output finds it missing → one reset → re-classifies
        // under the new wanted set → substitutes the delta; conversely
        // a terminal build's wide wanted set stops pinning the node —
        // its missing extras are forgiven as soon as no live build
        // wants them. Only paths positively identifiable as unwanted
        // (declared in expected_output_paths but outside the wanted
        // subset) are forgiven; a missing recorded path NOT in expected
        // (a realized floating-CA path) keeps triggering the reset as
        // today.
        let mut candidates: Vec<(String, Vec<String>, HashSet<String>)> = Vec::new();
        for node in nodes {
            if newly_inserted.contains(node.drv_hash.as_str()) {
                continue;
            }
            // I-099: re-probe hits were just verified + eager-fetched
            // by check_cached_outputs. Re-checking here is wasted RPCs.
            if cached_hits.contains_key(node.drv_hash.as_str()) {
                continue;
            }
            let Some(state) = self.dag.node(&node.drv_hash) else {
                continue;
            };
            if !matches!(
                state.status(),
                DerivationStatus::Completed | DerivationStatus::Skipped
            ) {
                continue;
            }
            if state.output_paths.is_empty() {
                continue;
            }
            // The complement MUST be taken against the *verifiable*
            // wanted subset. A wanted set that resolves to no
            // verifiable path (`drv^bogus`) yields an EMPTY wanted set
            // — and the complement of nothing is every declared path,
            // which forgives every missing recorded output and a GC'd
            // node is never reset. On `None` nothing is positively
            // identifiable as unwanted → forgive nothing → every
            // missing recorded path triggers the reset (the
            // conservative pre-feature behaviour).
            let eff = effective_wanted(state, &self.builds);
            let unwanted: HashSet<String> = match verifiable_wanted_paths(
                &node.output_names,
                &node.expected_output_paths,
                // Zero-live-interest degrades to all-declared (the
                // conservative-absent branch, T-D2.3 — the stored-union
                // fallback is gone).
                eff.as_deref().unwrap_or(&[]),
            ) {
                Some(wanted) => {
                    let wanted: HashSet<&str> = wanted.into_iter().collect();
                    node.expected_output_paths
                        .iter()
                        .filter(|p| !p.is_empty() && !wanted.contains(p.as_str()))
                        .cloned()
                        .collect()
                }
                None => HashSet::new(),
            };
            candidates.push((node.drv_hash.clone(), state.output_paths.clone(), unwanted));
        }

        if candidates.is_empty() {
            return HashSet::new();
        }

        let Some(store_client) = &self.store_client else {
            return HashSet::new();
        };

        // Batch all output paths into one FindMissingPaths call. The
        // probe set stays ALL recorded paths (probing an unwanted path
        // is harmless; only the reset DECISION below filters by wanted).
        let check_paths: Vec<String> = candidates
            .iter()
            .flat_map(|(_, paths, _)| paths.iter().cloned())
            .collect();

        let mut req = tonic::Request::new(FindMissingPathsRequest {
            store_paths: check_paths,
        });
        rio_proto::interceptor::inject_current(req.metadata_mut());
        // I-202: same JWT propagation as check_cached_outputs. Without
        // it, the store sees tenant_id=None → substitutable_paths stays
        // empty → a GC'd output that cache.nixos.org has is treated as
        // truly-missing → reset to Ready → re-dispatch the whole subtree
        // (FOD sources hit origin URLs; some are dead upstream).
        if let Some(t) = jwt_token
            && let Ok(v) = tonic::metadata::MetadataValue::try_from(t)
        {
            req.metadata_mut().insert(rio_proto::TENANT_TOKEN_HEADER, v);
        }

        // Conditional timeout — same pattern as
        // `find_missing_with_breaker`: this is a submission-sized
        // batch (every pre-existing Completed/Skipped output) so it
        // needs MERGE_FMP_TIMEOUT when the store is healthy, but the
        // short grpc_timeout during an outage so a fail-open path
        // doesn't pin the actor 90s. The breaker is fed but not gated
        // on: record_failure/success so a 90s dead stall here counts
        // toward opening, and a success here closes it; but the
        // fail-open RETURN (HashSet::new()) is unconditional —
        // `r[sched.breaker.cache-check]` reserves fail-CLOSED
        // rejection for new submissions, never for an already-admitted
        // DAG's re-verify.
        let fmp_timeout = if self.cache_breaker.is_open() {
            self.grpc_timeout
        } else {
            super::MERGE_FMP_TIMEOUT
        };
        let resp =
            match tokio::time::timeout(fmp_timeout, store_client.clone().find_missing_paths(req))
                .await
            {
                Ok(Ok(r)) => {
                    self.cache_breaker.record_success();
                    r.into_inner()
                }
                Ok(Err(e)) => {
                    self.cache_breaker.record_failure();
                    warn!(error = %e, "stale-completed verify: store FindMissingPaths failed; \
                       treating pre-existing Completed as valid (fail-open)");
                    return HashSet::new();
                }
                Err(_) => {
                    self.cache_breaker.record_failure();
                    warn!(timeout = ?fmp_timeout,
                      "stale-completed verify: store FindMissingPaths timed out; \
                       treating pre-existing Completed as valid (fail-open)");
                    return HashSet::new();
                }
            };

        if resp.missing_paths.is_empty() {
            return HashSet::new();
        }

        // r[impl sched.merge.stale-substitutable]
        // r[impl sched.substitute.detached+5]
        // Missing-but-substitutable: instead of awaiting eager-fetch
        // (which blocked the actor), reset Completed→Ready and spawn
        // the detached fetch (Ready→Substituting). SubstituteComplete
        // brings the node back to Completed. Dependents stay gated
        // during the window — same outcome, no actor stall.
        let substitutable: HashSet<String> = resp.substitutable_paths.into_iter().collect();
        let missing: HashSet<String> = resp.missing_paths.into_iter().collect();

        if missing.is_empty() {
            return HashSet::new();
        }

        // Two-pass: when GC sweeps a chain {A→B}, both must reset; A
        // gates on B, so A goes to Queued (not Ready) and is NOT
        // pushed — find_newly_ready picks A up when B re-completes.
        // Single-pass would push_ready A while B is still
        // Ready/Substituting → worker ENOENT on B's output → burned
        // retry, can cascade to Poisoned. "Ready ⟹ all deps' outputs
        // available in store" is the invariant.
        //
        // The same invariant requires demoting any pre-existing READY
        // parent of a reset node to Queued (the parent's prior Ready
        // verdict was computed against the now-stale Completed dep).
        // Collected in `demote_parents` and applied after pass 2.
        //
        // Pass 1: collect reset_set (no mutation). A missing recorded
        // output that no LIVE interested build wants does not trigger
        // the reset — see the candidate-collection comment above.
        let reset_set: HashSet<DrvHash> = candidates
            .iter()
            .filter(|(_, paths, unwanted)| {
                paths
                    .iter()
                    .any(|p| missing.contains(p.as_str()) && !unwanted.contains(p.as_str()))
            })
            .map(|(h, _, _)| DrvHash::from(h.as_str()))
            .collect();

        // Pass 2: per-node, compute deps_ok against reset_set + DAG
        // state, then transition to Ready (deps ok) or Queued (a dep
        // also reset / not done).
        let mut reset = HashSet::new();
        let mut to_spawn: Vec<(DrvHash, Vec<String>)> = Vec::new();
        let mut demote_parents: HashSet<DrvHash> = HashSet::new();
        for (drv_hash, output_paths, unwanted) in candidates {
            let Some(gone) = output_paths
                .iter()
                .find(|p| missing.contains(p.as_str()) && !unwanted.contains(p.as_str()))
            else {
                continue;
            };
            let drv_hash_k: DrvHash = drv_hash.as_str().into();
            // 3-way revert via the canonical helper (NOT 2-way
            // Ready|Queued — a Poisoned dep would leave this node
            // stuck Queued forever). reset_set overlay: a child being
            // reset THIS pass is currently still Completed/Skipped in
            // DAG state (so revert_target_for would say Ready) but is
            // about to become Ready/Queued, so cap at Queued. The
            // substitutable→to_spawn lane is gated on deps_ok too —
            // strictly safe and simpler; Queued falls through to
            // normal dispatch which re-probes substitutability.
            let mut target = self.dag.revert_target_for(&drv_hash_k);
            if target == DerivationStatus::Ready
                && self
                    .dag
                    .get_children(&drv_hash)
                    .into_iter()
                    .any(|d| reset_set.contains(&d))
            {
                target = DerivationStatus::Queued;
            }
            let deps_ok = target == DerivationStatus::Ready;
            let Some(state) = self.dag.node_mut(&drv_hash_k) else {
                continue;
            };
            if let Err(e) = state.transition(target) {
                warn!(drv_hash = %drv_hash, error = %e, ?target,
                      "stale-completed verify: reset transition rejected; skipping");
                continue;
            }
            state.output_paths.clear();
            // This reset is the moment a NEW substitution chain begins
            // for the node (the to_spawn lane below or the next
            // dispatch pass walks it afresh). However the previous
            // chain ended — including completion paths with no chain
            // bookkeeping of their own — the chain-scoped
            // spent-forgiveness set must not leak into this one: a
            // stale entry would veto forgiving a path no live build
            // wants any more and demote a fully-substitutable node to
            // a from-source build.
            state.never_forgive_paths.clear();
            // r[impl sched.merge.stale-completed-verify+5]
            // Pre-existing Ready parents of this reset node were Ready
            // against its now-gone output. Collect for demotion to
            // Queued after pass 2 (try_dispatch_one's `!= Ready` guard
            // drops the stale ready_queue entry).
            for p in self.dag.get_parents(&drv_hash) {
                if self
                    .dag
                    .node(&p)
                    .is_some_and(|s| s.status() == DerivationStatus::Ready)
                {
                    demote_parents.insert(p);
                }
            }

            warn!(
                drv_hash = %drv_hash,
                missing_path = %gone,
                ?target,
                "pre-existing Completed/Skipped node's output is gone from store \
                 (GC'd?); resetting for re-dispatch"
            );
            metrics::counter!("rio_scheduler_stale_completed_reset_total").increment(1);

            match self
                .db
                .update_derivation_status(&drv_hash_k, target, None, self.serving_generation())
                .await
            {
                Ok(crate::db::FencedWrite::Fenced) => {
                    self.note_fenced_evidence_write("stale-completed reset persist");
                }
                Ok(_) => {}
                Err(e) => {
                    error!(drv_hash = %drv_hash, error = %e, ?target,
                           "failed to persist stale-completed reset \
                            (build is Active; continuing)");
                }
            }
            if !deps_ok {
                // Queued: no push_ready, no spawn — find_newly_ready
                // promotes it when its reset dep re-completes.
                // DependencyFailed: terminal — the build's completion
                // check (later in handle_merge) handles it.
                reset.insert(drv_hash);
                continue;
            }
            // Substitutable subset: route to a materialization job
            // instead of pushing to ready_queue. The metric above
            // stays — output WAS gone, even if upstream can re-provide
            // it.
            //
            // r[impl sched.merge.wanted-outputs+2]
            // Like the reset decision above, the routing forgives
            // `unwanted` paths: a recorded output no LIVE interested
            // build wants (typically never present and not
            // substitutable — the steady state the demand-driven
            // criterion leaves behind) must not push the node onto the
            // ready queue when every WANTED missing path is
            // substitutable. The routing must never be stricter than
            // the reset that put the node here, otherwise the detached
            // re-substitution lane is skipped and only the
            // dispatch-time batch probe (cap-truncated, fail-open)
            // stands between the node and a from-source re-dispatch.
            if output_paths.iter().all(|p| {
                !missing.contains(p.as_str())
                    || substitutable.contains(p.as_str())
                    || unwanted.contains(p.as_str())
            }) {
                metrics::counter!("rio_scheduler_stale_completed_substituted_total").increment(1);
                to_spawn.push((drv_hash_k, output_paths));
            } else {
                self.push_ready(drv_hash_k);
            }
            reset.insert(drv_hash);
        }

        // Pass 3: demote Ready parents of reset nodes → Queued. The
        // parent's prior Ready verdict was computed against a dep
        // whose output is now gone; "Ready ⟹ all deps' outputs
        // available" no longer holds. find_newly_ready re-promotes
        // when the reset dep re-completes. Skip parents that pass 2
        // itself reset (already at Ready/Queued via the reset, not a
        // stale-Ready). The (Ready, Queued) transition arm is the
        // I-047 parent-side analog of (Completed, Ready/Queued).
        for p in demote_parents {
            if reset.contains(p.as_str()) {
                continue;
            }
            let Some(state) = self.dag.node_mut(&p) else {
                continue;
            };
            if state.status() != DerivationStatus::Ready {
                continue;
            }
            if let Err(e) = state.transition(DerivationStatus::Queued) {
                warn!(drv_hash = %p, error = %e,
                      "stale-completed verify: Ready-parent demotion rejected");
                continue;
            }
            warn!(drv_hash = %p,
                  "stale-completed verify: demoting Ready parent of reset dep → Queued");
            match self
                .db
                .update_derivation_status(
                    &p,
                    DerivationStatus::Queued,
                    None,
                    self.serving_generation(),
                )
                .await
            {
                Ok(crate::db::FencedWrite::Fenced) => {
                    self.note_fenced_evidence_write("Ready-parent demotion persist");
                }
                Ok(_) => {}
                Err(e) => {
                    error!(drv_hash = %p, error = %e,
                           "failed to persist Ready-parent demotion (continuing)");
                }
            }
        }

        if !to_spawn.is_empty() {
            // r[impl sched.materialize.job]
            // PD-18 (Phase B, design §2.1 row 4): the substitutable
            // stale-reset subset routes to materialization jobs
            // (origin=stale_reset) — this closed the SECOND of the two
            // merge-lane walk-spawn sites, and the spawner itself is
            // now deleted (a node Completed via materialization whose
            // outputs are GC'd re-merges through exactly this lane).
            // The pass-2 demote already put the node at Ready and
            // persisted it; the job row is the in-flight marker a
            // store replica claims.
            //
            // Posture (PDQ-9's per-§2.1-row split, recorded for the
            // T-5.2 table): this site runs POST-tx (reconcile 6c —
            // persist_merge_to_db committed long before), so the
            // job INSERT uses the standalone fenced helper (the
            // probe-origin posture), not batch 5. The wanted
            // relation for this (build, node) pair already rode the
            // merge transaction.
            for (drv_hash, _) in to_spawn {
                self.create_materialization_job_if_enabled(
                    &drv_hash,
                    crate::state::JobOrigin::StaleReset,
                    None,
                )
                .await;
            }
        }

        reset
    }

    /// True when [`crate::dag::DerivationDag::closure_evidence`] judges
    /// `drv_hash`'s child set [`ClosureEvidence::Vouched`](crate::dag::ClosureEvidence::Vouched): at least
    /// one DAG child, every one of them already produced
    /// (Completed/Skipped), and no `closure_hole` breadcrumb — the only
    /// children shape that says the node's dependency closure exists in
    /// the store, so a from-source dispatch is not doomed.
    ///
    /// Used by the stamp gates (the in-memory loop in
    /// `validate_and_ingest` and the row-level bind in
    /// `persist_merge_to_db` — a kept closure-dropped node is exempted
    /// from the `topdown_pruned` stamp only when its closure is
    /// vouched) and by the in-process clear sites (the
    /// post-reconciliation pass in `handle_merge_dag` and the
    /// completion-time `clear_topdown_pruned_for_produced_parents`;
    /// the lazy clear in `handle_substitute_complete` reads
    /// [`ClosureEvidence::Vouched`](crate::dag::ClosureEvidence::Vouched) off
    /// [`crate::dag::DerivationDag::closure_evidence`] directly rather
    /// than through this helper), so the stamps and the clears always
    /// judge the same criterion. Children that
    /// are merely *present* but unbuilt do NOT vouch
    /// ([`ClosureEvidence::Pending`](crate::dag::ClosureEvidence::Pending)) — they can belong to another
    /// build and be reaped unbuilt later (cancel → cascade terminal →
    /// reap → `children` scrubbed), leaving the node childless or
    /// closure-holed with a never-produced closure; and produced
    /// SURVIVORS of such a reap do not vouch either
    /// ([`ClosureEvidence::Broken`](crate::dag::ClosureEvidence::Broken) via the hole) — they are a
    /// truncated view of the pruned closure. Deliberate trade: a node
    /// stamped alongside unbuilt children carries the mark only until
    /// those children are produced (the completion-time clear drops it
    /// then); in the residual missed-clear window (e.g. a failover
    /// restoring a persisted mark whose best-effort PG clear was lost)
    /// one substitute failure can still fail-fast where a from-source
    /// dispatch would have worked — bounded (the fail-fast clears the
    /// marker it consumes; a resubmit recovers) and strictly preferable
    /// to the unbounded doomed-dispatch corner. The recovery-time gate
    /// evaluates a stricter SQL variant of the Vouched criterion over
    /// *persisted* children (`load_parents_with_all_children_produced`:
    /// every child produced AND vouched for by a still-live build that
    /// also owns the parent); the dispatch-time and reap-time guards
    /// consume the inverse judgment via [`Self::must_substitute`].
    pub(super) fn closure_vouched(&self, drv_hash: &str) -> bool {
        rio_evidence_kernel::closure_vouched(self.dag.closure_evidence(drv_hash))
    }

    // r[impl sched.merge.substitute-topdown+12]
    /// True when `drv_hash` carries the `topdown_pruned` mark AND its
    /// closure evidence is [`ClosureEvidence::Broken`](crate::dag::ClosureEvidence::Broken) (childless or
    /// closure-holed): its dependency closure was dropped from the
    /// submission and the current child set cannot vouch for it, so the
    /// node must complete via substitution — a from-source dispatch is
    /// doomed (the worker ENOENTs on inputDrvs that were never merged).
    ///
    /// This is the single predicate behind the dispatch-time carve-out
    /// and fail-fast arms (`batch_probe_cached_ready`, the
    /// pull-admission refusal in `admit_pull`), the
    /// downgrade re-spawn key in `handle_substitute_complete`, and the
    /// reap-hook fail-fast in `handle_cleanup_terminal_build` — a
    /// closure-holed survivor is treated exactly like a childless node
    /// at every guard. Unmarked nodes are never affected, whatever
    /// their evidence.
    pub(super) fn must_substitute(&self, drv_hash: &str) -> bool {
        rio_evidence_kernel::must_substitute(
            self.dag.node(drv_hash).is_some_and(|s| s.topdown_pruned),
            self.dag.closure_evidence(drv_hash),
        )
    }

    /// Persist nodes and edges to the DB after a successful DAG merge,
    /// and flip the build to Active as the transaction's last statement.
    /// Extracted from handle_merge_dag so failures can be caught and
    /// rolled back via cleanup_failed_merge.
    ///
    /// `topdown_pruned_parents`: kept nodes whose dependency closure a
    /// fired prune dropped (empty otherwise) — only their rows are
    /// upserted with `topdown_pruned = true` (OR-on-conflict, see
    /// `db/batch.rs`), additionally gated on the closure classifier not
    /// vouching for their existing children (`closure_vouched`).
    /// Inside the same transaction so a rejected merge can never
    /// leak the marker into PG, and a committed one can never lose it
    /// to a failover that races the in-memory stamp.
    ///
    /// Substitution-replacement (adjudication PDQ-9 / design A13/B6):
    /// flag-on, the same transaction also writes the wanted relation
    /// for every (build, node) pair and creates materialization jobs
    /// for the pruned roots and the new_sub lane — riding the same
    /// claims-floor fence (no extra floor read). The created jobs are
    /// returned so the caller can feed the in-memory view POST-commit.
    /// Flag-off the block is not entered and the transaction executes
    /// byte-identically to the as-built form.
    // r[impl sched.evidence.durability+2]
    #[allow(clippy::too_many_arguments)]
    async fn persist_merge_to_db(
        &mut self,
        build_id: Uuid,
        nodes: &[crate::domain::DerivationNode],
        edges: &[crate::domain::DerivationEdge],
        newly_inserted: &HashSet<DrvHash>,
        topdown_pruned_parents: &HashSet<String>,
        tenant_id: Option<Uuid>,
        new_sub_lane: &[DrvHash],
        reprobe_sub_lane: &[DrvHash],
    ) -> Result<Vec<super::materialize::CreatedJob>, ActorError> {
        // Build input rows for batch upsert.
        let node_rows: Vec<_> = nodes
            .iter()
            .map(|node| {
                let status = if newly_inserted.contains(node.drv_hash.as_str()) {
                    DerivationStatus::Created
                } else if let Some(state) = self.dag.node(&node.drv_hash) {
                    state.status()
                } else {
                    DerivationStatus::Created
                };
                crate::db::DerivationRow {
                    drv_hash: node.drv_hash.clone(),
                    drv_path: node.drv_path.clone(),
                    pname: (!node.pname.is_empty()).then(|| node.pname.clone()),
                    system: node.system.clone(),
                    status,
                    required_features: node.required_features.clone(),
                    // Phase 3b recovery columns: persist what we
                    // need to fully reconstruct DerivationState on
                    // leader failover. The proto node has all this.
                    expected_output_paths: node.expected_output_paths.clone(),
                    output_names: node.output_names.clone(),
                    is_fixed_output: node.is_fixed_output,
                    is_ca: node.is_content_addressed,
                    // THIS submission's wanted set. The upsert UNIONs
                    // it into the existing row (empty saturates to
                    // empty = "all wanted"), so the persisted set is
                    // the union across every build that ever merged
                    // this drv — same monotonic-growth semantics as
                    // the in-memory `DerivationState::union_wanted`.
                    wanted_output_names: node.wanted_output_names.clone(),
                    // Mark only kept nodes whose dependency closure the
                    // prune dropped (parents of the ORIGINAL
                    // submission's edges) so the guard survives leader
                    // failover, and only when the closure classifier
                    // does not vouch for their existing DAG children: a
                    // dep-less demanded leaf never had a closure to
                    // drop, and a node whose child set is Vouched (all
                    // produced, no closure hole) has its closure in the
                    // store — neither must carry the marker. Unbuilt
                    // children do NOT exempt the node (they can be
                    // reaped unbuilt), and neither do the produced
                    // survivors of a closure-holed reap (see
                    // `closure_vouched`). Mirrors the in-memory stamp
                    // gate in validate_and_ingest. OR-on-conflict
                    // upsert: a later non-pruned merge of the same drv
                    // (false here) never clears it.
                    topdown_pruned: topdown_pruned_parents.contains(node.drv_hash.as_str())
                        && !self.closure_vouched(&node.drv_hash),
                    // Always false: a merge never creates a closure hole
                    // (holes are stamped in PG via
                    // `set_closure_hole_by_hashes` by the leader's reap
                    // hook, the recovery-time stamp in
                    // `load_dag_from_rows`, and the poison-clear paths —
                    // admin ClearPoison and the poison-TTL sweep), and
                    // binding the in-memory value here would turn the
                    // upsert into a second stamping site. The
                    // OR-on-conflict SET keeps any existing persisted
                    // hole; the merge-side clears are the explicit heal
                    // in `handle_merge_dag` and the both-bits batched
                    // mark clear it runs for Vouched parents.
                    closure_hole: false,
                }
            })
            .collect();

        // drv_path → drv_hash lookup for edge resolution below. Edges
        // carry paths (proto wire format); id_map keys by hash.
        let path_to_hash: HashMap<&str, &str> = node_rows
            .iter()
            .map(|r| (r.drv_path.as_str(), r.drv_hash.as_str()))
            .collect();

        // Transaction: 3 batched roundtrips instead of 2N+E serial.
        let mut tx = self.db.pool().begin().await?;

        // r[impl sched.evidence.durability+2]
        // Claims-floor fence for the whole merge transaction (stamps,
        // links, edges, activation). Checked twice: once here at
        // begin() — a cheap early abort so a large merge from a deposed
        // believer never runs its multi-batch body — and once
        // immediately before commit (the authoritative check; see the
        // comment there). This is the deposed-believer MergeDag window:
        // leadership is checked at SubmitBuild enqueue time only, so a
        // replica deposed after the enqueue (or across this handler's
        // awaits) still executes its merge transaction unless the
        // transaction itself refuses to commit below the floor.
        let serving_generation = self.serving_generation();
        let floor = crate::db::SchedulerDb::claims_floor(&mut tx).await?;
        if !crate::db::SchedulerDb::at_or_above_floor(floor, serving_generation) {
            tx.rollback().await?;
            self.note_fenced_evidence_write("merge transaction (begin-time check)");
            return Err(ActorError::StaleGeneration {
                serving: serving_generation,
                floor: floor.unwrap_or(0),
            });
        }

        // Batch 1: upsert all derivations, get back drv_hash -> db_id map.
        let id_map = crate::db::SchedulerDb::batch_upsert_derivations(&mut tx, &node_rows).await?;

        // Batch 2: link all nodes to this build.
        let db_ids: Vec<Uuid> = id_map.values().map(|(id, _)| *id).collect();
        crate::db::SchedulerDb::batch_insert_build_derivations(&mut tx, build_id, &db_ids).await?;

        // Batch 3: insert edges. Resolve drv_path -> db_id via:
        //   1. this tx's id_map (covers newly-inserted + re-upserted
        //      nodes from this batch — ON CONFLICT RETURNING gives
        //      back existing ids for the latter),
        //   2. fall back to self.dag (covers cross-batch edges to
        //      nodes merged by a PRIOR SubmitBuild that aren't in
        //      this request's `nodes` list at all — rare but legal
        //      when gateway deduplicates against live DAG).
        // Does NOT read self.dag.node().db_id for nodes in THIS
        // batch — that field isn't set until after commit() below.
        let resolve = |drv_path: &str| -> Option<Uuid> {
            path_to_hash
                .get(drv_path)
                .and_then(|h| id_map.get(*h).map(|(id, _)| *id))
                .or_else(|| self.dag.db_id_for_path(drv_path))
        };
        let edge_rows: Result<Vec<(Uuid, Uuid)>, ActorError> = edges
            .iter()
            .map(|e| {
                let parent =
                    resolve(&e.parent_drv_path).ok_or_else(|| ActorError::MissingDbId {
                        drv_path: e.parent_drv_path.clone(),
                    })?;
                let child = resolve(&e.child_drv_path).ok_or_else(|| ActorError::MissingDbId {
                    drv_path: e.child_drv_path.clone(),
                })?;
                Ok((parent, child))
            })
            .collect();
        let edge_rows = edge_rows?;
        crate::db::SchedulerDb::batch_insert_edges(&mut tx, &edge_rows).await?;

        // No PG-side topdown_pruned clear in this transaction: the
        // clear decision moved to the post-reconciliation pass in
        // `handle_merge_dag` (clear_topdown_pruned_by_hashes), where it
        // can see stale-Completed children that
        // `verify_preexisting_completed` demotes after this commit.

        // Batch 4: Pending→Active for the build, in the SAME transaction.
        // A committed merge therefore implies an Active builds row; a
        // failure here aborts the whole merge (derivation upserts incl.
        // the topdown_pruned stamps, links, edges) instead of leaving
        // committed side effects behind for a build the caller is about
        // to reject and roll back in memory. The in-memory BuildInfo
        // transition happens in `persist_and_activate` only after this
        // commit succeeds.
        crate::db::SchedulerDb::activate_build_tx(&mut tx, build_id).await?;

        // Batch 5 (substitution-replacement Phase A, flag-gated): the
        // wanted relation + materialization-job creation, INSIDE this
        // transaction (adjudication PDQ-9 / design §2.1 rows 1–2 /
        // A13 / B6): a rolled-back merge creates no jobs; a committed
        // one cannot lose them to a failover racing the post-commit
        // phase. The writes ride this transaction's claims-floor fence
        // (begin-time check above, authoritative re-check below) — one
        // fence per transaction, no extra floor read. Flag-off the
        // whole block is skipped and the transaction is byte-identical
        // to the as-built form.
        let created_jobs: Vec<super::materialize::CreatedJob> = if self.materialization_cfg.enabled
        {
            // (a) The durable wanted relation — one row per
            // (build, node) pair, written by every merge regardless of
            // routing (design §6 / AS-1).
            // r[impl sched.materialize.job]
            let wanted_rows: Vec<crate::db::wanted::WantedRow<'_>> = nodes
                .iter()
                .filter_map(|node| {
                    let (db_id, _) = id_map.get(node.drv_hash.as_str())?;
                    Some(crate::db::wanted::WantedRow {
                        build_id,
                        derivation_id: *db_id,
                        wanted_output_names: &node.wanted_output_names,
                    })
                })
                .collect();
            crate::db::SchedulerDb::record_wanted_in_tx(&mut tx, &wanted_rows).await?;

            // (b) Job creation: pruned-origin rows first (kept roots
            // whose dependency closure this prune dropped — the same
            // nodes the topdown_pruned row stamp covers; the as-built
            // comment on that stamp states exactly the crash-atomicity
            // property the in-tx job INSERT preserves), then the
            // new_sub lane (origin=cache_opportunity — the nodes the
            // post-commit phase 6g would have routed to the walk).
            // A node in both sets is queued ONCE with the pruned
            // origin (the more specific classification).
            let mut job_rows: Vec<crate::db::materialization::NewJobRow<'_>> = Vec::new();
            for node in nodes {
                if topdown_pruned_parents.contains(node.drv_hash.as_str())
                    && !self.closure_vouched(&node.drv_hash)
                    && let Some((db_id, _)) = id_map.get(node.drv_hash.as_str())
                {
                    job_rows.push(crate::db::materialization::NewJobRow {
                        derivation_id: *db_id,
                        drv_hash: node.drv_hash.as_str(),
                        tenant_id,
                        origin: crate::state::JobOrigin::Pruned,
                    });
                }
            }
            let pruned_set: std::collections::HashSet<&str> =
                job_rows.iter().map(|r| r.drv_hash).collect();
            for drv_hash in new_sub_lane {
                if !pruned_set.contains(drv_hash.as_str())
                    && let Some((db_id, _)) = id_map.get(drv_hash.as_str())
                {
                    job_rows.push(crate::db::materialization::NewJobRow {
                        derivation_id: *db_id,
                        drv_hash: drv_hash.as_str(),
                        tenant_id,
                        origin: crate::state::JobOrigin::CacheOpportunity,
                    });
                }
            }

            // (c) PD-17 (Phase B): reprobe-lane jobs (origin=reprobe) +
            // the AS-5 durable reset, riding the same transaction. The
            // reprobe nodes are pre-existing rows referenced by this
            // submission, so they are in id_map; the partial-unique
            // dedup makes a re-merge onto an existing pending job
            // idempotent (created=false — exactly the orphan-former the
            // walk used to complete around). The AS-5 reset applies to
            // the FAILED-status subset (Poisoned/DependencyFailed/
            // Failed — their prior failure is moot now the output is
            // substitutable again): the durable poison clear + the
            // poison_cleared budget-reset ledger row (I-094 semantics,
            // resubmit_cycle zeroed like the admin clear). The matching
            // in-memory status correction applies post-commit at the 6d
            // slot, BEFORE seed_initial_states reads dep statuses
            // (the bug_089/bug_132 phase-ordering invariant).
            // r[impl sched.materialize.job]
            let already_queued: std::collections::HashSet<&str> =
                job_rows.iter().map(|r| r.drv_hash).collect();
            let mut reset_hashes: Vec<DrvHash> = Vec::new();
            let mut reset_rows: Vec<crate::db::attempts::AttemptRow> = Vec::new();
            for drv_hash in reprobe_sub_lane {
                if !already_queued.contains(drv_hash.as_str())
                    && let Some((db_id, _)) = id_map.get(drv_hash.as_str())
                {
                    job_rows.push(crate::db::materialization::NewJobRow {
                        derivation_id: *db_id,
                        drv_hash: drv_hash.as_str(),
                        tenant_id,
                        origin: crate::state::JobOrigin::Reprobe,
                    });
                }
                if self.dag.node(drv_hash).is_some_and(|s| {
                    matches!(
                        s.status(),
                        DerivationStatus::Poisoned
                            | DerivationStatus::DependencyFailed
                            | DerivationStatus::Failed
                    )
                }) && let Some(mut row) = self.reset_row_for(
                    drv_hash,
                    crate::state::OutcomeClass::PoisonCleared,
                    crate::state::ReportingParty::Scheduler,
                ) {
                    // Full reset: mirrors the admin clear's PG-side zero.
                    row.resubmit_cycle = 0;
                    reset_rows.push(row);
                    reset_hashes.push(drv_hash.clone());
                }
            }

            let created = crate::db::SchedulerDb::create_materialization_jobs_in_tx(
                &mut tx,
                &job_rows,
                serving_generation,
            )
            .await?;

            // The AS-5 durable reset: poison clear (poisoned_at NULL,
            // status reset) + the budget-reset rows, same transaction.
            if !reset_hashes.is_empty() {
                crate::db::SchedulerDb::clear_poison_batch_in_tx(&mut tx, &reset_hashes).await?;
                crate::db::SchedulerDb::append_attempts_batch(&mut tx, &reset_rows).await?;
            }

            job_rows
                .iter()
                .zip(created)
                .map(|(row, result)| super::materialize::CreatedJob {
                    drv_hash: DrvHash::from(row.drv_hash),
                    job_id: result.job_id,
                    created: result.created,
                    upgraded: result.upgraded,
                    origin: row.origin,
                })
                .collect()
        } else {
            Vec::new()
        };

        // r[verify sched.db.tx-commit-before-mutate]
        // In-mem mutation ordering invariant: no newly-inserted node has
        // db_id set BEFORE commit. If this fires, someone re-introduced
        // the in-tx write (the P0191 bug). Fires in every existing merge
        // test's happy path — zero new test scaffolding.
        // (rem-12 option b, endorsed at 12-pg-transaction-safety.md:1107)
        #[cfg(debug_assertions)]
        for hash in newly_inserted {
            if let Some(state) = self.dag.node(hash) {
                debug_assert!(
                    state.db_id.is_none(),
                    "newly-inserted node {hash} has db_id set before commit — \
                     in-mem mutation leaked into tx scope"
                );
            }
        }

        // r[impl sched.evidence.durability+2]
        // The AUTHORITATIVE claims-floor re-read, as the last statement
        // before commit. Under READ COMMITTED a successor's claim
        // INSERT can commit while the multi-batch tx body above runs
        // (hundreds of ms to seconds on large merges), and the
        // begin-time check would not see it — a begin-time-only fence
        // leaves a window equal to the whole tx duration. Re-reading
        // here narrows the residual to one floor-read-to-commit round
        // trip; this is a window-narrowing fence, not a serializability
        // proof.
        let floor = crate::db::SchedulerDb::claims_floor(&mut tx).await?;
        if !crate::db::SchedulerDb::at_or_above_floor(floor, serving_generation) {
            tx.rollback().await?;
            self.note_fenced_evidence_write("merge transaction (commit-time check)");
            return Err(ActorError::StaleGeneration {
                serving: serving_generation,
                floor: floor.unwrap_or(0),
            });
        }

        tx.commit().await?;

        // I-102: a large merge (e.g. 5800 rows for hello-mixed-32x) leaves
        // planner stats stale until autovacuum's analyze cycle (~1-10min on
        // RDS). In that window, list_builds chose a bad plan and went
        // 16ms → 2.3s. ANALYZE post-commit refreshes stats immediately;
        // cost is ~100ms on tables this size, paid once per large merge.
        // Threshold 500 ≈ PG's default autovacuum_analyze_threshold +
        // 10% of a few-thousand-row table.
        if node_rows.len() >= 500 {
            // Detached: ANALYZE on a grown derivations table is seconds
            // (the "~100ms" above dates from when these tables were
            // tiny). Best-effort/log-on-error already; spawning loses
            // nothing and keeps the actor's mailbox draining.
            let pool = self.db.pool().clone();
            let n = node_rows.len();
            tokio::spawn(async move {
                if let Err(e) =
                    sqlx::query("ANALYZE derivations, build_derivations, derivation_edges")
                        .execute(&pool)
                        .await
                {
                    warn!(error = %e, rows = n,
                          "post-merge ANALYZE failed (non-fatal; autovacuum will catch up)");
                }
            });
        }

        // r[impl sched.db.tx-commit-before-mutate]
        // In-mem db_id write ONLY after the tx is durable. If anything
        // above returned Err, the tx rolled back and we never reach
        // here — cleanup_failed_merge sees nodes with db_id = None.
        //
        // I-208: hydrate `resource_floor` for newly-inserted nodes
        // from the DB row. `try_from_node` sets `floor=zeros`, but the
        // row may pre-exist (ON CONFLICT) with a floor promoted by a
        // prior run's failures. Only newly_inserted: nodes already in
        // memory have a floor ≥ DB (recovery loaded it; any promotion
        // since then wrote both in-mem and DB), so overwriting would
        // downgrade. Per-dimension `.max()` only RAISES so a stale DB
        // row never demotes a higher in-memory floor.
        for (hash, (db_id, floor)) in &id_map {
            if let Some(state) = self.dag.node_mut(hash) {
                state.db_id = Some(*db_id);
                if newly_inserted.contains(hash.as_str()) {
                    let f = &mut state.sched.resource_floor;
                    f.mem_bytes = f.mem_bytes.max(floor.mem_bytes);
                    f.disk_bytes = f.disk_bytes.max(floor.disk_bytes);
                    f.deadline_secs = f.deadline_secs.max(floor.deadline_secs);
                }
            }
        }
        Ok(created_jobs)
    }

    /// Undo all in-memory state from a failed handle_merge_dag AFTER the
    /// merge succeeded but DB persistence or transition_build failed.
    /// Rolls back the DAG merge, removes map entries, and best-effort
    /// deletes the orphan DB build row.
    ///
    /// Takes `merge_result` by value: `removed_retriable` carries owned
    /// `DerivationState`s that `rollback_merge` re-inserts into the DAG.
    async fn cleanup_failed_merge(
        &mut self,
        build_id: Uuid,
        merge_result: crate::dag::MergeResult,
    ) {
        self.dag.rollback_merge(
            &merge_result.newly_inserted,
            &merge_result.new_edges,
            &merge_result.interest_added,
            &merge_result.traceparent_upgraded,
            &merge_result.wanted_grown,
            &merge_result.contributions_recorded,
            build_id,
            merge_result.removed_retriable,
        );
        self.events.remove(build_id);
        self.builds.remove(&build_id);
        if let Err(db_e) = self.db.delete_build(build_id).await {
            warn!(
                build_id = %build_id,
                error = %db_e,
                "failed to delete orphan build row during merge rollback"
            );
        }
    }

    /// Query for newly-inserted derivations' outputs and return the map of
    /// `drv_hash → output_paths` for nodes whose outputs are already available.
    ///
    /// **IA / fixed-CA nodes:** query the store via `FindMissingPaths` using
    /// `expected_output_paths`. The returned paths are `expected_output_paths`
    /// (deterministic — gateway computed them from the `.drv`).
    ///
    /// **Floating-CA nodes** (non-empty `ca_modular_hash`): `expected_output_paths`
    /// is `[""]` — the path isn't known until build time. Instead, query the
    /// `realisations` table by `(modular_hash, output_name)`. If a prior build
    /// registered the realisation, return the REALIZED path. This is the GAP-3
    /// fix: before this, CA nodes were passed to `FindMissingPaths` with `""`,
    /// which the store treats as missing → CA never cache-hit at merge time.
    ///
    /// # Circuit breaker interaction
    ///
    /// - Store unreachable + breaker closed + under threshold → `Ok(ca_hits only)`.
    ///   IA nodes proceed as cache miss. Slightly wasteful but recoverable.
    /// - Store unreachable + breaker trips open (or was already open) →
    ///   `Err(StoreUnavailable)`. SubmitBuild is rejected entirely.
    /// - Store reachable → `Ok(full hit map)`. Breaker closes.
    /// - No store client (tests) → `Ok(ca_hits only)`. Breaker not touched.
    /// - No IA expected_output_paths → `Ok(ca_hits only)`. Breaker not touched.
    ///
    /// The CA realisation lookup hits PG directly (same pool as the actor's
    /// own DB writes) and does NOT interact with the circuit breaker — a PG
    /// blip here degrades to cache-miss for that node (warn + skip), not a
    /// full SubmitBuild rejection.
    ///
    /// `&mut self` because the breaker is actor-owned state.
    ///
    /// `jwt_token` is forwarded as `x-rio-tenant-token` metadata so the
    /// store's per-tenant upstream probe fires and populates
    /// `substitutable_paths` — see r[sched.merge.substitute-probe].
    #[allow(clippy::type_complexity)]
    async fn check_cached_outputs(
        &mut self,
        probe_set: &HashSet<DrvHash>,
        node_index: &HashMap<&str, &crate::domain::DerivationNode>,
        jwt_token: Option<&str>,
    ) -> Result<(HashMap<DrvHash, Vec<String>>, Vec<(DrvHash, Vec<String>)>), ActorError> {
        // Floating-CA lane: PG realisations lookup + store-existence verify.
        let mut hits = self.check_ca_realisation_hits(probe_set, node_index).await;

        // Path-based lane: batch FindMissingPaths (breaker-gated) +
        // eager substitute fetch. `None` = no store client or no
        // path-based candidates → return CA-only hits.
        // r[impl sched.merge.ca-fod-substitute]
        let check_paths: Vec<String> = probe_set
            .iter()
            .filter_map(|h| node_index.get(h.as_str()))
            .flat_map(|n| n.expected_output_paths.iter())
            .filter(|p| !p.is_empty())
            .cloned()
            .collect();
        let Some(resp) = self
            .find_missing_with_breaker(check_paths.clone(), jwt_token)
            .await?
        else {
            return Ok((hits, Vec::new()));
        };

        // r[impl sched.merge.substitute-probe]
        // r[impl sched.merge.substitute-fetch]
        // r[impl sched.substitute.detached+5]
        // Locally-present → cached_hits (Created→Completed inline).
        // Upstream-substitutable → pending_substitute (caller spawns
        // the detached fetch after seed_initial_states; the actor loop
        // is NOT blocked on the NAR download).
        let missing: HashSet<String> = resp.missing_paths.into_iter().collect();
        let substitutable: HashSet<String> = resp.substitutable_paths.into_iter().collect();
        let indeterminate: HashSet<String> = resp.indeterminate_paths.into_iter().collect();
        let present: HashSet<String> = check_paths
            .into_iter()
            .filter(|p| !missing.contains(p))
            .collect();

        // A derivation is cached if it has at least one non-empty
        // expected output path AND all of its WANTED outputs are
        // LOCALLY present. Skip nodes the floating-CA lane already
        // resolved.
        // r[impl sched.merge.substitute-probe-indeterminate]
        // Indeterminate (probe got 429/5xx/deadline-cut) is treated the
        // SAME as substitutable here: optimistically try the detached
        // fetch. The closure walk's failure path
        // (`SubstituteComplete{ok=false}`) sets `substitute_tried` and
        // falls through to build — so a true miss costs one extra fetch
        // attempt, not a wrong build dispatch.
        //
        // r[impl sched.merge.wanted-outputs+2]
        // Demand-driven completeness: only the WANTED outputs (the ones
        // some consumer's inputDrvs names, or the root asked for) must
        // be present for a hit / present-or-substitutable for
        // pending_substitute. A missing output nothing consumes
        // (glibc-debug) must not condemn the derivation — and its
        // entire build-time closure — to a from-source rebuild. The
        // wanted subset yields ALL declared paths when the wanted set
        // is empty (pre-migration rows, the BasicDerivation fallback,
        // ^* roots). The wanted set is read from the DAG state, not the
        // submission node: dag.merge already ran (step 2), so the DAG
        // carries the per-build contributions of EVERY interested build
        // — using only this submission's narrower set could complete a
        // shared re-probed node while another build still wants a
        // missing output. The slice fed to the predicate is the LIVE
        // effective wanted set (`effective_wanted`: the saturating
        // union of live interested builds' contributions), so a
        // terminal build's wants stop pinning the verdict; when that
        // set is unavailable (no live builds, post-failover unknown
        // contributions) the stored node-level union is the
        // conservative fallback. The hit/pending VALUES stay
        // `expected_output_paths` (the realized-output stamp, the GC
        // pin set, and the client output report keep covering every
        // declared output).
        let mut pending_substitute: Vec<(DrvHash, Vec<String>)> = Vec::new();
        for h in probe_set {
            if hits.contains_key(h) {
                continue; // floating-CA — already resolved above
            }
            let Some(n) = node_index.get(h.as_str()) else {
                continue;
            };
            if !n.expected_output_paths.iter().any(|p| !p.is_empty()) {
                continue;
            }
            // A wanted set that resolves to no verifiable path cannot
            // be classified — fall through to build, same as the
            // all-declared guard above.
            let dag_state = self.dag.node(h);
            let eff = dag_state.and_then(|s| effective_wanted(s, &self.builds));
            let Some(wanted) = verifiable_wanted_paths(
                &n.output_names,
                &n.expected_output_paths,
                // Pre-existing node with zero live interest degrades to
                // all-declared (conservative-absent, T-D2.3); a node
                // not in the DAG keeps the submission's own wanted set.
                eff.as_deref().unwrap_or_else(|| {
                    if dag_state.is_some() {
                        &[]
                    } else {
                        &n.wanted_output_names
                    }
                }),
            ) else {
                continue;
            };
            if wanted.iter().all(|p| present.contains(*p)) {
                hits.insert(h.clone(), n.expected_output_paths.clone());
            } else if wanted.iter().all(|p| {
                present.contains(*p) || substitutable.contains(*p) || indeterminate.contains(*p)
            }) {
                pending_substitute.push((h.clone(), n.expected_output_paths.clone()));
            }
        }

        Ok((hits, pending_substitute))
    }

    /// Floating-CA cache-check lane: query the `realisations` table by
    /// `(modular_hash, output_name)` for each floating-CA node in
    /// `probe_set`, then verify the realized paths still exist in the
    /// store (GC-race defense). Fixed-CA FODs and IA nodes pass
    /// through (handled by the path-based lane). Best-effort — PG
    /// blip degrades to cache-miss; store unreachable degrades to
    /// fail-open (keep realisation hits).
    ///
    /// The store-existence verify can block the actor up to
    /// `grpc_timeout`.
    async fn check_ca_realisation_hits(
        &mut self,
        probe_set: &HashSet<DrvHash>,
        node_index: &HashMap<&str, &crate::domain::DerivationNode>,
    ) -> HashMap<DrvHash, Vec<String>> {
        let mut hits: HashMap<DrvHash, Vec<String>> = HashMap::new();

        // Floating-CA nodes have expected_output_paths == [""] (path
        // unknown until built). Query realisations by (modular_hash,
        // output_name) instead — same lookup resolve_ca_inputs uses at
        // dispatch time. A hit means a prior build (via gateway
        // wopRegisterDrvOutput or scheduler insert_realisation on
        // completion) already produced this output.
        //
        // Fixed-CA FODs have ca_modular_hash set (translate.rs:343) but
        // ALSO have a known expected_output_path — those go through the
        // path-based lane below, which checks upstream substitutability.
        // Latent I-139 shape: was per-(node × output) serial PG awaits.
        // Not hit by IA-heavy builds (tfc's 5298 had zero floating-CA),
        // but a CA-heavy DAG would stall the actor here. Collect pairs,
        // one batched lookup, then per-node assemble.
        let mut pairs: Vec<([u8; 32], String)> = Vec::new();
        let mut floating: Vec<(&DrvHash, [u8; 32], &Vec<String>)> = Vec::new();
        for h in probe_set {
            let Some(n) = node_index.get(h.as_str()) else {
                continue;
            };
            let Some(modular_hash) = n.ca_modular_hash else {
                continue; // IA or malformed — handled by path-based lane below
            };
            if n.expected_output_paths.iter().any(|p| !p.is_empty()) {
                // Fixed-CA FOD: output path known at eval time. The
                // path-based lane handles it (incl. substitutability).
                continue;
            }
            for out_name in &n.output_names {
                pairs.push((modular_hash, out_name.clone()));
            }
            floating.push((h, modular_hash, &n.output_names));
        }
        let realised: HashMap<([u8; 32], String), String> =
            match rio_store::realisations::query_batch(self.db.pool(), &pairs).await {
                Ok(rs) => rs
                    .into_iter()
                    .map(|r| ((r.drv_hash, r.output_name), r.output_path))
                    .collect(),
                Err(e) => {
                    warn!(error = %e, n = pairs.len(),
                          "CA realisation batch lookup failed; treating all as cache-miss");
                    HashMap::new()
                }
            };
        for (h, modular_hash, output_names) in floating {
            // All output_names must resolve for a full cache-hit. Collect
            // realized paths index-paired with output_names (same layout
            // as DerivationState.output_paths).
            let mut realized = Vec::with_capacity(output_names.len());
            let mut all_found = true;
            for out_name in output_names {
                match realised.get(&(modular_hash, out_name.clone())) {
                    Some(path) => realized.push(path.clone()),
                    None => {
                        all_found = false;
                        break;
                    }
                }
            }
            if all_found && !realized.is_empty() {
                hits.insert(h.clone(), realized);
            }
        }

        // --- Floating-CA store-existence verify (I-048) ----------------
        // r[impl sched.merge.stale-completed-verify+5]
        // The realisations table is scheduler-local PG; store GC doesn't
        // touch it. A realisation can point to a path that's been GC'd.
        // Without this verify, such a node flips to Completed here and
        // then I-047's pre-existing-completed reset undoes it on the
        // NEXT merge — ping-pong, with the dependent dispatching against
        // a missing output in between.
        //
        // No upstream substitution here: floating-CA paths can't be
        // probed against cache.nixos.org by path (the path is local-
        // realisation-derived, not a globally-known FOD output), and
        // rio-store has no upstream realisations lookup.
        //
        // Fail-open like I-047: store unreachable → keep `hits` as-is.
        // No breaker interaction — the path-based FindMissingPaths below
        // is the breaker-gated availability check; this is best-effort
        // correctness on the rare GC-race window.
        if !hits.is_empty()
            && let Some(store_client) = &self.store_client
        {
            let ca_check_paths: Vec<String> = hits.values().flatten().cloned().collect();
            let mut req = tonic::Request::new(FindMissingPathsRequest {
                store_paths: ca_check_paths,
            });
            rio_proto::interceptor::inject_current(req.metadata_mut());
            let missing: Option<HashSet<String>> = match tokio::time::timeout(
                self.grpc_timeout,
                store_client.clone().find_missing_paths(req),
            )
            .await
            {
                Ok(Ok(r)) => Some(r.into_inner().missing_paths.into_iter().collect()),
                Ok(Err(e)) => {
                    warn!(error = %e, "CA realisation store-verify: FindMissingPaths failed; \
                          treating realisations as valid (fail-open)");
                    None
                }
                Err(_) => {
                    warn!(timeout = ?self.grpc_timeout,
                          "CA realisation store-verify: FindMissingPaths timed out; \
                           treating realisations as valid (fail-open)");
                    None
                }
            };
            if let Some(missing) = missing
                && !missing.is_empty()
            {
                hits.retain(|h, paths| {
                    let Some(gone) = paths.iter().find(|p| missing.contains(p.as_str())) else {
                        return true;
                    };
                    warn!(
                        drv_hash = %h,
                        missing_path = %gone,
                        "CA realisation points to GC'd output; treating as cache-miss"
                    );
                    metrics::counter!("rio_scheduler_stale_realisation_filtered_total")
                        .increment(1);
                    false
                });
            }
        }
        hits
    }

    /// Path-based cache-check lane: batch `FindMissingPaths` against
    /// the store with circuit-breaker accounting. `&mut self` for the
    /// breaker. Returns:
    ///
    ///   - `Ok(None)` — no store client OR no paths to check; breaker
    ///     untouched. A submission with no path-based candidates is
    ///     unaffected by store availability — rejecting with
    ///     `StoreUnavailable` would be a false positive.
    ///   - `Ok(Some(resp))` — store reachable; breaker closed.
    ///   - `Err(StoreUnavailable)` — store unreachable AND the breaker
    ///     tripped (or was already open).
    ///
    /// This call is ALSO the half-open probe: when the breaker is open
    /// the call still fires; success closes it.
    // r[impl sched.breaker.cache-check+3]
    async fn find_missing_with_breaker(
        &mut self,
        check_paths: Vec<String>,
        jwt_token: Option<&str>,
    ) -> Result<Option<rio_proto::types::FindMissingPathsResponse>, ActorError> {
        let Some(store_client) = &self.store_client else {
            return Ok(None);
        };
        if check_paths.is_empty() {
            return Ok(None);
        }

        // Wrap in a timeout: this is a synchronous call inside the
        // single-threaded actor event loop. If the store hangs, NO heartbeats,
        // completions, or dispatches are processed until this returns.
        //
        // This call is ALSO the half-open probe: if the breaker is open, we
        // still make the call. Success → close; failure → stay open + reject.
        let mut fmp_req = tonic::Request::new(FindMissingPathsRequest {
            store_paths: check_paths,
        });
        rio_proto::interceptor::inject_current(fmp_req.metadata_mut());
        // r[impl sched.merge.substitute-probe]
        // JWT propagation: x-rio-tenant-token → store's interceptor →
        // tenant_id → check_available HEAD probe. Without this header
        // the store sees tenant_id=None and substitutable_paths stays
        // empty — we'd dispatch builds for paths cache.nixos.org has.
        //
        // try_from on a JWT can't fail (base64url is pure ASCII, well
        // under 8KB MetadataValue limit — same invariant as the
        // gateway's submit_and_process_build). If it somehow does,
        // warn+skip: substitution silently degrades to always-miss,
        // which is the pre-P0472 behavior — not a merge failure.
        if let Some(t) = jwt_token {
            match tonic::metadata::MetadataValue::try_from(t) {
                Ok(v) => {
                    fmp_req
                        .metadata_mut()
                        .insert(rio_proto::TENANT_TOKEN_HEADER, v);
                }
                Err(e) => {
                    warn!(error = %e, "jwt_token not ASCII-encodable; \
                          FindMissingPaths sent without tenant header \
                          (substitution probe will not fire)");
                }
            }
        }
        // r[impl sched.substitute.eager-probe]
        // Conditional timeout: when the breaker is CLOSED (store
        // believed healthy) use MERGE_FMP_TIMEOUT (90s) — the
        // store-side check_available no longer truncates, so a
        // 153k-uncached probe legitimately runs ~36s. When OPEN
        // (sustained outage; this call is the half-open probe) use
        // grpc_timeout (30s) so a tiny submission isn't held 90s
        // against a dead store and the probe stays inside
        // OPEN_DURATION. The other three merge-time FMP callers:
        // `verify_preexisting_completed` (submission-sized; same
        // conditional below), the top-down roots-only FMP
        // (root-count-bounded; stays on grpc_timeout), and
        // `check_ca_realisation_hits`' store-existence verify
        // (CA-hit-count-bounded; stays on grpc_timeout).
        let fmp_timeout = if self.cache_breaker.is_open() {
            self.grpc_timeout
        } else {
            super::MERGE_FMP_TIMEOUT
        };
        let resp = match tokio::time::timeout(
            fmp_timeout,
            store_client.clone().find_missing_paths(fmp_req),
        )
        .await
        {
            Ok(Ok(r)) => {
                // Success — close the breaker (resets counter). Cheap no-op
                // if it was already closed.
                self.cache_breaker.record_success();
                r.into_inner()
            }
            Ok(Err(e)) => {
                warn!(error = %e, "store FindMissingPaths failed");
                metrics::counter!("rio_scheduler_cache_check_failures_total").increment(1);
                // record_failure() returns true if this trips the breaker
                // open (or it was already open from a prior trip).
                if self.cache_breaker.record_failure() {
                    return Err(ActorError::StoreUnavailable);
                }
                // Under threshold: caller proceeds with CA-only hits.
                // IA nodes run with 100% miss — wasteful, but N<5 of
                // these is tolerable. The breaker catches sustained outages.
                return Ok(None);
            }
            Err(_) => {
                warn!(
                    timeout = ?fmp_timeout,
                    "store FindMissingPaths timed out"
                );
                metrics::counter!("rio_scheduler_cache_check_failures_total").increment(1);
                if self.cache_breaker.record_failure() {
                    return Err(ActorError::StoreUnavailable);
                }
                return Ok(None);
            }
        };
        Ok(Some(resp))
    }

    // r[impl sched.merge.substitute-topdown+12]
    /// Top-down demand-set substitution pre-check (step 0 of
    /// `handle_merge_dag`).
    ///
    /// The demand set is the submission's structural roots (no parent
    /// edge IN THIS SUBMISSION — parents from other builds don't change
    /// what THIS build needs) ∪ every node the gateway marked
    /// `explicitly_requested` (a client-named target that another
    /// requested target's closure swallowed, so it is NOT a structural
    /// root of the combined submission).
    ///
    /// Returns `Some(demand set)` if ALL demanded derivations' IA
    /// outputs are available (present in store or
    /// upstream-substitutable). The caller prunes the submission to the
    /// demand set, dropping every other node and all edges before
    /// merge, and stamps `topdown_pruned=true` on the kept nodes whose
    /// dependency closure the prune dropped and whose existing DAG
    /// children (if any) are not already all produced — flagged
    /// non-roots are retained as standalone nodes (like multiple roots
    /// today) so they get classified, routed to substitution, and
    /// reported like any other demanded node. The
    /// actual fetch is deferred (`r[sched.substitute.detached]`) and
    /// runs AFTER the prune commits; on `SubstituteComplete{ok=false}`
    /// for a `topdown_pruned` node, `handle_substitute_complete` fails
    /// every interested build (resubmit re-probes or full-merges) —
    /// building is invalid because the node's `inputDrvs` were dropped.
    ///
    /// Returns `None` to fall through to the full bottom-up
    /// `check_cached_outputs`. Reasons: no store client, any demanded
    /// node is floating-CA (no expected path), any wanted output of a
    /// demanded node missing and not substitutable, a demanded node
    /// whose submission selector resolves to no declared output, store
    /// error.
    ///
    /// The availability criterion is NOT submission-local for a
    /// demanded node that already exists in the DAG: the submission's
    /// wanted set is unioned with the pre-existing node's LIVE
    /// effective wanted set ([`effective_wanted`] over live interested
    /// builds, stored-union fallback) — at least as demanding as the
    /// set post-merge classification (`check_cached_outputs`, the
    /// dispatch-time probes) evaluates the shared node against; with
    /// the stored-union fallback it can be strictly wider than the
    /// post-merge live set, never narrower. If another live build wants
    /// an output that is missing and not substitutable, that
    /// classification keeps the node on the from-source path; pruning
    /// THIS submission's dep closure first would leave this build's
    /// progress hostage to the other build's dep interest staying alive
    /// (its cancel/fail sweep takes the sole-interest deps down and
    /// this build hangs on a Queued node). The submission's OWN wanted
    /// set must also resolve to a verifiable path — that guard covers
    /// the fallback corner where no prior build is live and post-merge
    /// classification degrades to exactly this build's (possibly bogus)
    /// selector. Demanded nodes not yet in the DAG keep the
    /// submission-only criterion (there is nothing else to consult).
    ///
    /// # Circuit breaker interaction
    ///
    /// Success closes the breaker; error records a failure (but
    /// returns `None`, not `Err` — step 4's check will reject if
    /// the breaker tripped). This probe is advisory; the
    /// authoritative gate is at step 4.
    async fn check_roots_topdown(
        &mut self,
        nodes: &[crate::domain::DerivationNode],
        edges: &[crate::domain::DerivationEdge],
        jwt_token: Option<&str>,
    ) -> Option<Vec<crate::domain::DerivationNode>> {
        let store_client = self.store_client.as_ref()?;

        // Skip if there's nothing to prune. Single-node and edge-free
        // submissions get no benefit from the pre-check (step 4 handles
        // them with the same RPC count). The threshold also avoids a
        // redundant FindMissingPaths round-trip on tiny DAGs where
        // step 4's single batch call is already optimal.
        if edges.is_empty() || nodes.len() <= 1 {
            return None;
        }

        // --- Compute the demand set ---------------------------------
        // Structural roots (no edge names the node as a child; edges
        // key by drv_path, proto-level) ∪ gateway-flagged explicitly
        // requested nodes.
        let children: HashSet<&str> = edges.iter().map(|e| e.child_drv_path.as_str()).collect();
        let demanded: Vec<&crate::domain::DerivationNode> = nodes
            .iter()
            .filter(|n| !children.contains(n.drv_path.as_str()) || n.explicitly_requested)
            .collect();

        if demanded.is_empty() || demanded.len() == nodes.len() {
            // No demanded nodes (malformed — cycle?) or every node is
            // demanded (nothing to prune). Either way, nothing to
            // short-circuit.
            return None;
        }

        // --- Bail on floating-CA demanded nodes ----------------------
        // CA nodes have no expected_output_paths (path isn't known
        // until build time). The realisations-table lookup in
        // check_cached_outputs handles them; we can't pre-check here.
        // Conservative: ANY CA demanded node → fall through entirely.
        if demanded.iter().any(|n| n.ca_modular_hash.is_some()) {
            return None;
        }

        // --- Collect demanded nodes' output paths --------------------
        let demanded_paths: Vec<String> = demanded
            .iter()
            .flat_map(|n| n.expected_output_paths.iter())
            .filter(|p| !p.is_empty())
            .cloned()
            .collect();

        if demanded_paths.is_empty() {
            // Demanded nodes have no expected outputs — nothing to check.
            return None;
        }

        // --- FindMissingPaths for the demand set only ----------------
        let mut fmp_req = tonic::Request::new(FindMissingPathsRequest {
            store_paths: demanded_paths.clone(),
        });
        rio_proto::interceptor::inject_current(fmp_req.metadata_mut());
        // JWT propagation — same as r[sched.merge.substitute-probe].
        if let Some(t) = jwt_token
            && let Ok(v) = tonic::metadata::MetadataValue::try_from(t)
        {
            fmp_req
                .metadata_mut()
                .insert(rio_proto::TENANT_TOKEN_HEADER, v);
        }
        let grpc_timeout = self.grpc_timeout;
        let resp = match tokio::time::timeout(
            grpc_timeout,
            store_client.clone().find_missing_paths(fmp_req),
        )
        .await
        {
            Ok(Ok(r)) => {
                self.cache_breaker.record_success();
                r.into_inner()
            }
            Ok(Err(e)) => {
                debug!(error = %e, "top-down FindMissingPaths failed; falling through");
                self.cache_breaker.record_failure();
                return None;
            }
            Err(_) => {
                debug!(timeout = ?grpc_timeout, "top-down FindMissingPaths timed out; falling through");
                self.cache_breaker.record_failure();
                return None;
            }
        };

        debug!(
            demanded = demanded_paths.len(),
            missing = resp.missing_paths.len(),
            substitutable = resp.substitutable_paths.len(),
            "top-down: FindMissingPaths response"
        );

        // --- All-or-nothing: every WANTED demanded output available? -
        // r[impl sched.merge.wanted-outputs+2]
        // "Available" = present in store (NOT in missing_paths) OR
        // substitutable upstream. A single unavailable wanted output of
        // any demanded node → fall through to the full merge. Unwanted
        // outputs (declared but referenced by no consumer and not in
        // the request's OutputsSpec) are excluded from the criterion —
        // probing them is harmless but their absence must not defeat
        // the prune. The wanted subset degrades to all declared paths
        // for an empty wanted set.
        let substitutable: HashSet<&str> = resp
            .substitutable_paths
            .iter()
            .map(String::as_str)
            .collect();
        let truly_missing: HashSet<&str> = resp
            .missing_paths
            .iter()
            .map(String::as_str)
            .filter(|p| !substitutable.contains(p))
            .collect();
        if demanded.iter().any(|n| {
            // The criterion set for a pre-existing demanded node is the
            // submission's wanted set UNIONed (saturating; empty =
            // all) with the node's live effective wanted set — at
            // least as demanding as what step-4/dispatch
            // classification will use (a superset: with the
            // stored-union fallback it can be strictly wider than the
            // post-merge live set, never narrower). A narrower,
            // submission-only criterion can prune this build's dep
            // closure while classification (driven by another live
            // build's wants) still routes the node from-source,
            // leaving this build dependent on deps it never
            // registered interest in. Fresh demanded nodes have
            // nothing else to consult — submission-only criterion.
            let mut criterion_wanted = n.wanted_output_names.clone();
            if let Some(existing) = self.dag.node(&n.drv_hash) {
                // Zero-live-interest saturates the criterion to
                // all-declared (conservative-absent, T-D2.3 — the
                // widening direction; an empty union saturates).
                let eff = effective_wanted(existing, &self.builds).unwrap_or_default();
                union_wanted_saturating(&mut criterion_wanted, &eff);
            }
            // The submission's OWN wanted set must also resolve to a
            // verifiable path: post-merge classification of a
            // pre-existing demanded node can degrade to exactly that
            // contribution (stored-union fallback above, only this
            // build live afterwards), and an unresolvable set is
            // unclassifiable — nothing would substitute the node
            // whose deps this prune just dropped.
            verifiable_wanted_paths(
                &n.output_names,
                &n.expected_output_paths,
                &n.wanted_output_names,
            )
            .is_none()
                // A criterion set that resolves to no verifiable path
                // must never vacuously satisfy the all-available
                // criterion: treat it as "unavailable" so the prune
                // doesn't fire and drop a dependency closure the node
                // actually needs. Falling through to the full merge
                // is always safe.
                || verifiable_wanted_paths(
                    &n.output_names,
                    &n.expected_output_paths,
                    &criterion_wanted,
                )
                .is_none_or(|wanted| wanted.iter().any(|p| truly_missing.contains(*p)))
        }) {
            debug!(
                missing = truly_missing.len(),
                "top-down: wanted output(s) of a demanded node unavailable or \
                 unresolvable; falling through to full merge"
            );
            return None;
        }

        // No inline QPI: awaiting query_path_info_opt here blocked the
        // actor for the duration of the store-side closure walk
        // (ghc-sized roots take minutes; grpc_timeout = 30s) — the
        // very builds the prune helps most timed out and fell through
        // anyway. Instead: return the pruned demand set; the caller
        // continues into the full merge with those nodes only;
        // check_cached_outputs re-probes them, populates
        // pending_substitute, and the fetch happens store-side via the
        // materialization job. Prune benefit preserved; actor never
        // blocks on QPI.
        Some(demanded.into_iter().cloned().collect())
    }
}
