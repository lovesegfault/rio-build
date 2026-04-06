//! DAG merge handling: SubmitBuild → merge client DAG into global DAG.
// r[impl sched.merge.dedup]
// r[impl sched.merge.shared-priority-max]
// r[impl sched.merge.toctou-serial]

use super::*;

impl DagActor {
    // -----------------------------------------------------------------------
    // MergeDag
    // -----------------------------------------------------------------------

    #[instrument(skip(self, req), fields(build_id = %req.build_id))]
    pub(super) async fn handle_merge_dag(
        &mut self,
        req: MergeDagRequest,
    ) -> Result<broadcast::Receiver<rio_proto::types::BuildEvent>, ActorError> {
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
        // I-139: per-phase timing. handle_merge_dag was >300s for a
        // 153k-node / 837k-edge DAG with only ~22s in the batched DB
        // phase; the rest had no logging. Each phase now self-reports
        // so future regressions localize immediately.
        let t_total = Instant::now();
        let mut t_phase = Instant::now();
        macro_rules! phase {
            ($name:literal) => {
                debug!(elapsed = ?t_phase.elapsed(), phase = $name, "merge phase");
                t_phase = Instant::now();
            };
        }
        // rio_scheduler_builds_total is incremented at terminal transition
        // (complete_build/transition_build_to_failed/handle_cancel_build)
        // with an outcome label, so SLI queries can compute success rate.

        // === Step 0: Top-down root substitution check ===============
        // r[impl sched.merge.substitute-topdown]
        // Before merging the full DAG, check if the ROOT derivations'
        // outputs are already available. If ALL roots are cached, the
        // deps are transitively unnecessary — prune the submission to
        // roots-only before merge.
        //
        // This short-circuits the common case `rsb -L p#hello` where
        // hello is in cache.nixos.org: instead of eager-fetching ~700
        // dependency NARs (stdenv bootstrap chain), fetch just hello
        // and complete. Deps never enter the global DAG, so a later
        // build that needs them triggers its own cache-check.
        //
        // Falls through to the full DAG on any uncertainty (store
        // unreachable, partial root cache, CA roots, fetch failure).
        // The existing check_cached_outputs at step 4 handles those
        // correctly — this is a fast-path, not a replacement.
        let (nodes, edges) = match self
            .check_roots_topdown(&nodes, &edges, jwt_token.as_deref())
            .await
        {
            Some(roots_only) => {
                debug!(
                    original_nodes = nodes.len(),
                    root_nodes = roots_only.len(),
                    pruned = nodes.len() - roots_only.len(),
                    "top-down: all roots substitutable; pruning deps from submission"
                );
                metrics::counter!("rio_scheduler_topdown_prune_total").increment(1);
                (roots_only, Vec::new())
            }
            None => (nodes, edges),
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
        // in build_events/build_sequences/builds with no cleanup scheduled.
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
        let (event_tx, event_rx) = broadcast::channel(BUILD_EVENT_BUFFER_SIZE);
        self.build_events.insert(build_id, event_tx);
        self.build_sequences.insert(build_id, 0);

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
        let node_index: HashMap<&str, &rio_proto::dag::DerivationNode> =
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
        let cached_hits = match self
            .check_cached_outputs(&probe_set, &node_index, jwt_token.as_deref())
            .await
        {
            Ok(hits) => hits,
            Err(e) => {
                error!(build_id = %build_id, error = %e, "cache-check circuit breaker open; rolling back merge");
                self.cleanup_failed_merge(build_id, &merge_result).await;
                return Err(e);
            }
        };
        phase!("4-check-cached-outputs");

        // === Step 5: DB persistence with rollback on error ============
        // If any of these fail, roll back the merge AND the map inserts
        // AND delete the DB build row, so in-memory and DB state stay
        // consistent. After this block the build is committed (Active).
        if let Err(e) = self
            .persist_merge_to_db(build_id, &nodes, &edges, newly_inserted)
            .await
        {
            error!(build_id = %build_id, error = %e, "merge DB persistence failed; rolling back");
            self.cleanup_failed_merge(build_id, &merge_result).await;
            return Err(e);
        }
        phase!("5-persist-merge-db");

        // Transition build to active. If DB write fails, roll back everything.
        // Pending→Active is always a valid transition for a fresh build; we
        // debug_assert the outcome but don't branch on it (Rejected here
        // would be a bug in BuildInfo::transition, not a recoverable error).
        match self.transition_build(build_id, BuildState::Active).await {
            Ok(outcome) => {
                debug_assert_eq!(
                    outcome,
                    super::build::TransitionOutcome::Applied,
                    "Pending→Active rejected on fresh build (BuildInfo::transition bug)"
                );
            }
            Err(e) => {
                error!(build_id = %build_id, error = %e, "transition to Active failed; rolling back");
                self.cleanup_failed_merge(build_id, &merge_result).await;
                return Err(e);
            }
        }

        // === Step 6: Post-Active processing ==========================
        // From here on, DB write failures are log-and-continue (build is
        // Active and in a valid state; DB sync will catch up on next status
        // update or heartbeat reconciliation).

        let total_derivations = nodes.len() as u32;
        let mut cached_count = 0u32;

        let mut reprobe_unlocked: Vec<DrvHash> = Vec::new();
        // I-139: collect for one batched Completed update after the
        // loop instead of N sequential round-trips. clear_poison and
        // upsert_path_tenants_for stay per-hit — both are gated
        // (Poisoned-only / tenant_id-present-only) and rare relative
        // to cached_hits.len().
        let mut completed_batch: Vec<DrvHash> = Vec::with_capacity(cached_hits.len());
        for (drv_hash, output_paths) in &cached_hits {
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
                // I-099/I-094: re-probe hit on a previously-failed node
                // — failure history is moot now we have the output.
                if matches!(
                    from,
                    DerivationStatus::Poisoned
                        | DerivationStatus::DependencyFailed
                        | DerivationStatus::Failed
                ) {
                    state.clear_failure_history();
                }
                from
            };
            cached_count += 1;
            let source = if is_reprobe { "reprobe" } else { "scheduler" };
            metrics::counter!("rio_scheduler_cache_hits_total", "source" => source).increment(1);

            // I-094: PG-side poison clear (status='created', NULLs
            // poisoned_at/retry_count/failed_builders) so recovery
            // doesn't resurrect the poison. Best-effort like the
            // status update below.
            if matches!(
                from_status,
                DerivationStatus::Poisoned | DerivationStatus::DependencyFailed
            ) && let Err(e) = self.db.clear_poison(drv_hash).await
            {
                warn!(drv_hash = %drv_hash, error = %e,
                      "failed to clear poison in PG after re-probe cache hit");
            }
            completed_batch.push(drv_hash.clone());

            if is_reprobe {
                info!(drv_hash = %drv_hash, from = ?from_status,
                      "re-probe: existing not-done node found in upstream cache");
                // Existing dependents in Queued can now advance.
                // Newly-inserted dependents are handled by
                // compute_initial_states below; this catches the
                // pre-existing ones.
                reprobe_unlocked.extend(self.dag.find_newly_ready(drv_hash));
            }

            self.emit_build_event(
                build_id,
                rio_proto::types::build_event::Event::Derivation(rio_proto::dag::DerivationEvent {
                    derivation_path: node.drv_path.clone(),
                    status: Some(rio_proto::dag::derivation_event::Status::Cached(
                        rio_proto::dag::DerivationCached {
                            output_paths: output_paths.clone(),
                        },
                    )),
                }),
            );
            // r[impl sched.gc.path-tenants-upsert]
            // Cache hit during merge: path already in store, new
            // tenant needs attribution so GC retains under their
            // policy. output_paths were just set above.
            self.upsert_path_tenants_for(drv_hash).await;
        }
        if !completed_batch.is_empty() {
            let hashes: Vec<&str> = completed_batch.iter().map(DrvHash::as_str).collect();
            if let Err(e) = self
                .db
                .update_derivation_status_batch(&hashes, DerivationStatus::Completed)
                .await
            {
                warn!(count = hashes.len(), error = %e,
                      "failed to persist cache-hit Completed status batch");
            }
        }
        // I-099: advance pre-existing Queued dependents of re-probe
        // hits. Newly-inserted dependents are handled by
        // compute_initial_states; this is the pre-existing-DAG side.
        for ready_hash in reprobe_unlocked {
            if let Some(s) = self.dag.node_mut(&ready_hash)
                && s.transition(DerivationStatus::Ready).is_ok()
            {
                if let Err(e) = self
                    .db
                    .update_derivation_status(&ready_hash, DerivationStatus::Ready, None)
                    .await
                {
                    warn!(drv_hash = %ready_hash, error = %e,
                          "failed to persist re-probe-unlocked Ready status");
                }
                self.push_ready(ready_hash);
            }
        }
        phase!("6a-cached-hits-loop");

        // Compute critical-path priorities for newly-inserted nodes.
        // Done AFTER cache-hit transitions so completed derivations
        // are correctly excluded from their parents' max-child (a
        // cached dep doesn't block anything — it's done).
        //
        // This sets est_duration (from estimator) + priority (bottom-up)
        // for new nodes, and propagates to existing nodes if the new
        // subgraph raises their priority. The ready queue reads these
        // for BinaryHeap ordering.
        crate::critical_path::compute_initial(&mut self.dag, &self.estimator, newly_inserted);
        phase!("6b-critical-path");

        // I-047: pre-existing Completed nodes may have stale output_paths
        // (GC deleted the output between the node's original completion and
        // this merge). Verify outputs exist BEFORE compute_initial_states —
        // otherwise newly-inserted dependents would be unlocked against a
        // dep whose output is gone, and the worker fails on isValidPath.
        // Reset stale nodes to Ready; they re-dispatch and re-complete.
        // r[impl sched.merge.stale-completed-verify]
        let stale_reset = self
            .verify_preexisting_completed(&nodes, newly_inserted, &cached_hits)
            .await;
        phase!("6c-verify-preexisting");

        // Compute initial states for the remaining (non-cached) newly-inserted
        // derivations. Cached derivations above are now Completed, so their
        // dependents will correctly be computed as Ready here.
        let remaining_new: HashSet<DrvHash> = newly_inserted
            .iter()
            .filter(|h| !cached_hits.contains_key(h.as_str()))
            .cloned()
            .collect();
        let initial_states = self.dag.compute_initial_states(&remaining_new);
        phase!("6d-compute-initial-states");

        // Track whether any newly inserted node was immediately marked
        // DependencyFailed (because a dep is already poisoned). If so, the
        // build may need to fail (!keepGoing) or terminate early (keepGoing).
        let mut first_dep_failed: Option<DrvHash> = None;

        // I-139: this loop previously did one update_derivation_status
        // round-trip PER NODE. For a 153k-node fresh DAG that's 153k
        // sequential PG awaits inside the single-threaded actor — ~278s
        // at ~1.8ms RTT. Split into an in-memory transition pass +
        // three batched ANY($1::text[]) updates (one per target status).
        // Same log-and-continue semantics as before: persist failures
        // don't abort (build is already Active).
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
            if let Err(e) = self
                .db
                .update_derivation_status_batch(&hashes, status)
                .await
            {
                error!(
                    status = ?status, count = hashes.len(), error = %e,
                    "failed to persist initial-state status batch \
                     (build is Active; continuing)"
                );
            }
        }
        phase!("6e-initial-states-persist");

        // Also handle nodes that already existed. A pre-existing Completed
        // node counts as cached; a pre-existing Poisoned/DependencyFailed
        // node must set first_dep_failed so handle_derivation_failure
        // fires below. Without the failure arm, a single-node resubmit of
        // a still-poisoned derivation (within TTL, no ClearPoison yet)
        // leaves the build Active with completed=0, failed=0, total=1 —
        // check_build_completion never fires.
        for node in &nodes {
            if newly_inserted.contains(node.drv_hash.as_str()) {
                continue;
            }
            // Stale-reset nodes are now Ready (re-queued above); skip
            // here so they don't double-count as cached or trigger
            // upsert_path_tenants on the GC'd path.
            if stale_reset.contains(node.drv_hash.as_str()) {
                continue;
            }
            // I-099: re-probe hits were already counted + emitted in
            // the cached_hits loop above. Don't double-count.
            if cached_hits.contains_key(node.drv_hash.as_str()) {
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
                    cached_count += 1;
                    metrics::counter!("rio_scheduler_cache_hits_total", "source" => "existing")
                        .increment(1);
                    // r[impl sched.gc.path-tenants-upsert]
                    // Pre-existing Completed/Skipped merged from
                    // another build: this build's tenant now also
                    // wants those output_paths retained. The node's
                    // interested_builds already includes this build
                    // (merge() added it); output_paths was set when
                    // the node originally completed.
                    self.upsert_path_tenants_for(&node.drv_hash.as_str().into())
                        .await;
                }
                DerivationStatus::Poisoned | DerivationStatus::DependencyFailed => {
                    first_dep_failed.get_or_insert_with(|| node.drv_hash.as_str().into());
                    debug!(
                        drv_hash = %node.drv_hash,
                        status = ?state.status(),
                        "pre-existing node already failed; build will fail fast"
                    );
                }
                _ => {}
            }
        }
        phase!("6f-preexisting-nodes-loop");

        // Update build's cached count + persist initial denorm columns.
        if let Some(build) = self.builds.get_mut(&build_id) {
            build.cached_count = cached_count;
        }
        // I-103: sets completed_count from DAG ground truth + persists
        // (total, completed, cached) to PG so list_builds is O(LIMIT).
        self.update_build_counts(build_id).await;

        // Send BuildStarted event
        self.emit_build_event(
            build_id,
            rio_proto::types::build_event::Event::Started(rio_proto::types::BuildStarted {
                total_derivations,
                cached_derivations: cached_count,
            }),
        );

        // If a newly merged node depends on an already-poisoned existing
        // node, handle the failure now (fail build if !keepGoing, or sync
        // counts + check completion if keepGoing).
        if let Some(failed_hash) = first_dep_failed {
            self.handle_derivation_failure(build_id, &failed_hash).await;
            // handle_derivation_failure may have transitioned the build to
            // Failed. If so, don't dispatch.
            if self
                .builds
                .get(&build_id)
                .is_some_and(|b| b.state().is_terminal())
            {
                return Ok(event_rx);
            }
        }

        // Store cache-check is done (step 4 above, long since); signal
        // the boundary between cache-resolution and dispatch. Gateways
        // can surface this ("inputs resolved, N to build"). Fires even
        // on the all-cached path — "resolved to zero work" is still
        // resolved. P0294 ripped the Build CRD that originally wanted
        // this as a condition; kept for gateway STDERR_NEXT.
        self.emit_build_event(
            build_id,
            rio_proto::types::build_event::Event::InputsResolved(
                rio_proto::types::BuildInputsResolved {},
            ),
        );

        // Check if the build is already complete (all cache hits)
        if cached_count == total_derivations {
            self.complete_build(build_id).await?;
        } else {
            // Dispatch ready derivations to workers
            self.dispatch_ready().await;
        }
        phase!("7-dispatch");
        let _ = &mut t_phase; // last write is intentionally unread
        debug!(
            elapsed = ?t_total.elapsed(),
            nodes = nodes.len(),
            edges = edges.len(),
            newly_inserted = newly_inserted.len(),
            "handle_merge_dag total"
        );

        Ok(event_rx)
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
    /// regression than the original bug. No circuit-breaker
    /// interaction — this is a best-effort correctness check, not an
    /// availability gate like `check_cached_outputs`.
    async fn verify_preexisting_completed(
        &mut self,
        nodes: &[rio_proto::dag::DerivationNode],
        newly_inserted: &HashSet<DrvHash>,
        cached_hits: &HashMap<DrvHash, Vec<String>>,
    ) -> HashSet<String> {
        // Collect (drv_hash, output_paths) for pre-existing Completed
        // nodes in this merge. Skip nodes with empty output_paths —
        // nothing to verify (shouldn't happen for a real Completed
        // node, but defensive against test fixtures).
        let mut candidates: Vec<(String, Vec<String>)> = Vec::new();
        for node in nodes {
            if newly_inserted.contains(node.drv_hash.as_str()) {
                continue;
            }
            // I-099: re-probe hits were just verified + eager-fetched
            // by check_cached_outputs. Re-checking here (without JWT,
            // so substitutable doesn't apply) would reset them.
            if cached_hits.contains_key(node.drv_hash.as_str()) {
                continue;
            }
            let Some(state) = self.dag.node(&node.drv_hash) else {
                continue;
            };
            if state.status() != DerivationStatus::Completed {
                continue;
            }
            if state.output_paths.is_empty() {
                continue;
            }
            candidates.push((node.drv_hash.clone(), state.output_paths.clone()));
        }

        if candidates.is_empty() {
            return HashSet::new();
        }

        let Some(store_client) = &self.store_client else {
            return HashSet::new();
        };

        // Batch all output paths into one FindMissingPaths call.
        let check_paths: Vec<String> = candidates
            .iter()
            .flat_map(|(_, paths)| paths.iter().cloned())
            .collect();

        let mut req = tonic::Request::new(FindMissingPathsRequest {
            store_paths: check_paths,
        });
        rio_proto::interceptor::inject_current(req.metadata_mut());

        let missing: HashSet<String> = match tokio::time::timeout(
            self.grpc_timeout,
            store_client.clone().find_missing_paths(req),
        )
        .await
        {
            Ok(Ok(r)) => r.into_inner().missing_paths.into_iter().collect(),
            Ok(Err(e)) => {
                warn!(error = %e, "stale-completed verify: store FindMissingPaths failed; \
                       treating pre-existing Completed as valid (fail-open)");
                return HashSet::new();
            }
            Err(_) => {
                warn!(timeout = ?self.grpc_timeout,
                      "stale-completed verify: store FindMissingPaths timed out; \
                       treating pre-existing Completed as valid (fail-open)");
                return HashSet::new();
            }
        };

        if missing.is_empty() {
            return HashSet::new();
        }

        // Reset each node that has ANY missing output. Partial-missing
        // (multi-output drv with some outputs GC'd) is the same as
        // all-missing from the dependent's perspective: the build
        // needs to re-run to repopulate everything.
        let mut reset = HashSet::new();
        for (drv_hash, output_paths) in candidates {
            let Some(gone) = output_paths.iter().find(|p| missing.contains(p.as_str())) else {
                continue;
            };
            let drv_hash_k: DrvHash = drv_hash.as_str().into();
            let Some(state) = self.dag.node_mut(&drv_hash_k) else {
                continue;
            };
            if let Err(e) = state.transition(DerivationStatus::Ready) {
                warn!(drv_hash = %drv_hash, error = %e,
                      "stale-completed verify: Completed→Ready rejected; skipping reset");
                continue;
            }
            state.output_paths.clear();

            warn!(
                drv_hash = %drv_hash,
                missing_path = %gone,
                "pre-existing Completed node's output is gone from store \
                 (GC'd?); resetting to Ready for re-dispatch"
            );
            metrics::counter!("rio_scheduler_stale_completed_reset_total").increment(1);

            if let Err(e) = self
                .db
                .update_derivation_status(&drv_hash_k, DerivationStatus::Ready, None)
                .await
            {
                error!(drv_hash = %drv_hash, error = %e,
                       "failed to persist stale-completed Ready reset \
                        (build is Active; continuing)");
            }
            self.push_ready(drv_hash_k);
            reset.insert(drv_hash);
        }

        reset
    }

    /// Persist nodes and edges to the DB after a successful DAG merge.
    /// Extracted from handle_merge_dag so failures can be caught and
    /// rolled back via cleanup_failed_merge.
    async fn persist_merge_to_db(
        &mut self,
        build_id: Uuid,
        nodes: &[rio_proto::dag::DerivationNode],
        edges: &[rio_proto::dag::DerivationEdge],
        newly_inserted: &HashSet<DrvHash>,
    ) -> Result<(), ActorError> {
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

        // Batch 1: upsert all derivations, get back drv_hash -> db_id map.
        let id_map = crate::db::SchedulerDb::batch_upsert_derivations(&mut tx, &node_rows).await?;

        // Batch 2: link all nodes to this build.
        let db_ids: Vec<Uuid> = id_map.values().copied().collect();
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
                .and_then(|h| id_map.get(*h).copied())
                .or_else(|| self.find_db_id_by_path(drv_path))
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

        tx.commit().await?;

        // I-102: a large merge (e.g. 5800 rows for hello-mixed-32x) leaves
        // planner stats stale until autovacuum's analyze cycle (~1-10min on
        // RDS). In that window, list_builds chose a bad plan and went
        // 16ms → 2.3s. ANALYZE post-commit refreshes stats immediately;
        // cost is ~100ms on tables this size, paid once per large merge.
        // Threshold 500 ≈ PG's default autovacuum_analyze_threshold +
        // 10% of a few-thousand-row table.
        if node_rows.len() >= 500
            && let Err(e) = sqlx::query("ANALYZE derivations, build_derivations, derivation_edges")
                .execute(self.db.pool())
                .await
        {
            warn!(error = %e, rows = node_rows.len(),
                  "post-merge ANALYZE failed (non-fatal; autovacuum will catch up)");
        }

        // r[impl sched.db.tx-commit-before-mutate]
        // In-mem db_id write ONLY after the tx is durable. If anything
        // above returned Err, the tx rolled back and we never reach
        // here — cleanup_failed_merge sees nodes with db_id = None.
        for (hash, db_id) in &id_map {
            if let Some(state) = self.dag.node_mut(hash) {
                state.db_id = Some(*db_id);
            }
        }
        Ok(())
    }

    /// Undo all in-memory state from a failed handle_merge_dag AFTER the
    /// merge succeeded but DB persistence or transition_build failed.
    /// Rolls back the DAG merge, removes map entries, and best-effort
    /// deletes the orphan DB build row.
    async fn cleanup_failed_merge(
        &mut self,
        build_id: Uuid,
        merge_result: &crate::dag::MergeResult,
    ) {
        self.dag.rollback_merge(
            &merge_result.newly_inserted,
            &merge_result.new_edges,
            &merge_result.interest_added,
            build_id,
        );
        self.build_events.remove(&build_id);
        self.build_sequences.remove(&build_id);
        self.build_progress_at.remove(&build_id);
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
    async fn check_cached_outputs(
        &mut self,
        probe_set: &HashSet<DrvHash>,
        node_index: &HashMap<&str, &rio_proto::dag::DerivationNode>,
        jwt_token: Option<&str>,
    ) -> Result<HashMap<DrvHash, Vec<String>>, ActorError> {
        let mut hits: HashMap<DrvHash, Vec<String>> = HashMap::new();

        // --- Floating-CA: realisations-table lookup --------------------
        // GAP-3 fix: CA nodes can't use FindMissingPaths (expected path is
        // ""). Query realisations by (modular_hash, output_name) instead —
        // same lookup resolve_ca_inputs uses at dispatch time. A hit here
        // means a prior build (via gateway wopRegisterDrvOutput or scheduler
        // insert_realisation on completion) already produced this output.
        for h in probe_set {
            let Some(n) = node_index.get(h.as_str()) else {
                continue;
            };
            let Ok(modular_hash): Result<[u8; 32], _> = n.ca_modular_hash.as_slice().try_into()
            else {
                continue; // IA or malformed — handled by FindMissingPaths below
            };
            // All output_names must resolve for a full cache-hit. Collect
            // realized paths index-paired with output_names (same layout
            // as DerivationState.output_paths).
            let mut realized = Vec::with_capacity(n.output_names.len());
            let mut all_found = true;
            for out_name in &n.output_names {
                match crate::ca::resolve::query_realisation(self.db.pool(), &modular_hash, out_name)
                    .await
                {
                    Ok(Some(path)) => realized.push(path),
                    Ok(None) => {
                        all_found = false;
                        break;
                    }
                    Err(e) => {
                        warn!(drv_hash = %h, output = %out_name, error = %e,
                              "CA realisation lookup failed; treating as cache-miss");
                        all_found = false;
                        break;
                    }
                }
            }
            if all_found && !realized.is_empty() {
                hits.insert(h.clone(), realized);
            }
        }

        // --- CA store-existence verify (I-048) -------------------------
        // r[impl sched.merge.stale-completed-verify]
        // The realisations table is scheduler-local PG; store GC doesn't
        // touch it. A realisation can point to a path that's been GC'd.
        // Without this verify, such a node flips to Completed here and
        // then I-047's pre-existing-completed reset undoes it on the
        // NEXT merge — ping-pong, with the dependent dispatching against
        // a missing output in between.
        //
        // Fail-open like I-047: store unreachable → keep `hits` as-is.
        // No breaker interaction — the IA path's FindMissingPaths below
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

        // --- IA / fixed-CA: FindMissingPaths by expected path ----------
        let Some(store_client) = &self.store_client else {
            return Ok(hits);
        };

        // Collect expected output paths for IA probe-set derivations
        // (newly-inserted + existing not-done re-probe per I-099).
        // Skip CA nodes (handled above) and nodes without expected_output_paths.
        // Also skip empty-string paths defensively (a stray "" would be
        // reported missing, defeating the cache-hit for that node anyway,
        // but no reason to send it over the wire).
        let check_paths: Vec<String> = probe_set
            .iter()
            .filter_map(|h| node_index.get(h.as_str()))
            .filter(|n| n.ca_modular_hash.len() != 32)
            .flat_map(|n| n.expected_output_paths.iter())
            .filter(|p| !p.is_empty())
            .cloned()
            .collect();

        if check_paths.is_empty() {
            // Didn't probe the store — no signal for the breaker. If it's
            // open, it stays open; if closed, stays closed. Returning
            // Ok here (not checking breaker.is_open()) is deliberate: a
            // submission with no IA expected_output_paths is unaffected by
            // store availability (there's nothing to path-check), so
            // rejecting it with StoreUnavailable would be a false positive.
            return Ok(hits);
        }

        // Wrap in a timeout: this is a synchronous call inside the
        // single-threaded actor event loop. If the store hangs, NO heartbeats,
        // completions, or dispatches are processed until this returns.
        //
        // This call is ALSO the half-open probe: if the breaker is open, we
        // still make the call. Success → close; failure → stay open + reject.
        let mut fmp_req = tonic::Request::new(FindMissingPathsRequest {
            store_paths: check_paths.clone(),
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
                        .insert(rio_common::jwt_interceptor::TENANT_TOKEN_HEADER, v);
                }
                Err(e) => {
                    warn!(error = %e, "jwt_token not ASCII-encodable; \
                          FindMissingPaths sent without tenant header \
                          (substitution probe will not fire)");
                }
            }
        }
        let grpc_timeout = self.grpc_timeout;
        let resp = match tokio::time::timeout(
            grpc_timeout,
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
                // Under threshold: proceed with CA-only hit set.
                // IA nodes run with 100% miss — wasteful, but N<5 of
                // these is tolerable. The breaker catches sustained outages.
                return Ok(hits);
            }
            Err(_) => {
                warn!(
                    timeout = ?grpc_timeout,
                    "store FindMissingPaths timed out"
                );
                metrics::counter!("rio_scheduler_cache_check_failures_total").increment(1);
                if self.cache_breaker.record_failure() {
                    return Err(ActorError::StoreUnavailable);
                }
                return Ok(hits);
            }
        };

        // r[impl sched.merge.substitute-probe]
        // r[impl sched.merge.substitute-fetch]
        // Substitutable paths count as present: the store's HEAD probe
        // confirmed the tenant's upstream has them. But the probe is
        // HEAD-only — the NAR is not yet in the store. The builder's
        // later FUSE GetPath calls carry no JWT (&[] metadata), so the
        // store's try_substitute_on_miss short-circuits → ENOENT.
        //
        // Fix: fire QueryPathInfo with JWT for each substitutable path
        // NOW. That RPC's miss-handler (r[store.substitute.upstream])
        // does the actual fetch. We only care about the side effect;
        // a path whose fetch fails gets dropped from the substitutable
        // set so the derivation falls through to normal dispatch.
        //
        // Concurrency: buffer_unordered(N). A DAG can have hundreds of
        // substitutable paths; serial fetch would block the actor for
        // minutes, but UNBOUNDED concurrency (~1k concurrent QPI →
        // store → S3 PutObject) saturates the aws-sdk connection pool
        // (~10-20 default) → "dispatch failure" → ~20% false demotes.
        // self.substitute_max_concurrent (default 16, configurable via
        // RIO_SUBSTITUTE_MAX_CONCURRENT) keeps throughput without the
        // load spike. Each future is independently bounded by
        // grpc_timeout (same as FindMissingPaths above — still inside
        // the actor loop).
        let substitutable: HashSet<String> = if resp.substitutable_paths.is_empty() {
            HashSet::new()
        } else {
            use futures_util::stream::{self, StreamExt};
            let jwt_pair;
            let jwt_meta: &[(&'static str, &str)] = match jwt_token {
                Some(t) => {
                    jwt_pair = [(rio_common::jwt_interceptor::TENANT_TOKEN_HEADER, t)];
                    &jwt_pair
                }
                None => &[],
            };
            let mut fetches = stream::iter(resp.substitutable_paths)
                .map(|p| {
                    let mut c = store_client.clone();
                    async move {
                        let res = rio_proto::client::query_path_info_opt(
                            &mut c,
                            &p,
                            grpc_timeout,
                            jwt_meta,
                        )
                        .await;
                        (p, res)
                    }
                })
                .buffer_unordered(self.substitute_max_concurrent);
            let mut fetched = HashSet::new();
            while let Some((p, res)) = fetches.next().await {
                match res {
                    Ok(Some(_)) => {
                        fetched.insert(p);
                    }
                    Ok(None) => {
                        warn!(
                            path = %p,
                            "substitutable path NotFound on eager fetch \
                             (upstream HEAD probe lied?); demoting to cache-miss"
                        );
                        metrics::counter!("rio_scheduler_substitute_fetch_failures_total")
                            .increment(1);
                    }
                    Err(e) => {
                        warn!(
                            path = %p,
                            error = %e,
                            "substitutable path failed eager fetch; \
                             demoting to cache-miss"
                        );
                        metrics::counter!("rio_scheduler_substitute_fetch_failures_total")
                            .increment(1);
                    }
                }
            }
            fetched
        };
        if !substitutable.is_empty() {
            debug!(
                count = substitutable.len(),
                "treating upstream-substitutable paths as cache hits"
            );
            metrics::counter!("rio_scheduler_cache_hits_total", "source" => "substitute")
                .increment(substitutable.len() as u64);
        }
        let missing: HashSet<String> = resp
            .missing_paths
            .into_iter()
            .filter(|p| !substitutable.contains(p))
            .collect();
        let present: HashSet<String> = check_paths
            .into_iter()
            .filter(|p| !missing.contains(p))
            .collect();

        // An IA derivation is cached if it has at least one expected output
        // path AND all of them are present (locally or upstream-substitutable).
        for h in probe_set {
            let Some(n) = node_index.get(h.as_str()) else {
                continue;
            };
            if n.ca_modular_hash.len() == 32 {
                continue; // CA — already handled above
            }
            if !n.expected_output_paths.is_empty()
                && n.expected_output_paths.iter().all(|p| present.contains(p))
            {
                hits.insert(h.clone(), n.expected_output_paths.clone());
            }
        }

        Ok(hits)
    }

    // r[impl sched.merge.substitute-topdown]
    /// Top-down root substitution pre-check (step 0 of `handle_merge_dag`).
    ///
    /// Returns `Some(roots)` if ALL root derivations' IA outputs are
    /// available (present in store or upstream-substitutable AND
    /// successfully eager-fetched). The caller prunes the submission
    /// to roots-only, dropping the dep subgraph before merge.
    ///
    /// Returns `None` to fall through to the full bottom-up
    /// `check_cached_outputs`. Reasons: no store client, any root
    /// is floating-CA (no expected path), any root output missing
    /// and not substitutable, any eager-fetch fails, store error.
    ///
    /// Roots are nodes with no parent edge IN THIS SUBMISSION.
    /// Local computation — doesn't consult the global DAG (parents
    /// from other builds are irrelevant to what THIS build needs).
    ///
    /// # Circuit breaker interaction
    ///
    /// Success closes the breaker; error records a failure (but
    /// returns `None`, not `Err` — step 4's check will reject if
    /// the breaker tripped). This probe is advisory; the
    /// authoritative gate is at step 4.
    async fn check_roots_topdown(
        &mut self,
        nodes: &[rio_proto::dag::DerivationNode],
        edges: &[rio_proto::dag::DerivationEdge],
        jwt_token: Option<&str>,
    ) -> Option<Vec<rio_proto::dag::DerivationNode>> {
        let store_client = self.store_client.as_ref()?;

        // Skip if there's nothing to prune. Single-node and roots-only
        // submissions get no benefit from the pre-check (step 4 handles
        // them with the same RPC count). The threshold also avoids a
        // redundant FindMissingPaths round-trip on tiny DAGs where
        // step 4's single batch call is already optimal.
        if edges.is_empty() || nodes.len() <= 1 {
            return None;
        }

        // --- Compute roots from submission edges -------------------
        // A root is a node that appears as no edge's child. Edges key
        // by drv_path (proto-level), so collect child paths and filter.
        let children: HashSet<&str> = edges.iter().map(|e| e.child_drv_path.as_str()).collect();
        let roots: Vec<&rio_proto::dag::DerivationNode> = nodes
            .iter()
            .filter(|n| !children.contains(n.drv_path.as_str()))
            .collect();

        if roots.is_empty() || roots.len() == nodes.len() {
            // No roots (malformed — cycle?) or all roots (no deps to
            // prune). Either way, nothing to short-circuit.
            return None;
        }

        // --- Bail on floating-CA roots ------------------------------
        // CA roots have no expected_output_paths (path isn't known
        // until build time). The realisations-table lookup in
        // check_cached_outputs handles them; we can't pre-check here.
        // Conservative: ANY CA root → fall through entirely.
        if roots.iter().any(|n| n.ca_modular_hash.len() == 32) {
            return None;
        }

        // --- Collect root output paths ------------------------------
        let root_paths: Vec<String> = roots
            .iter()
            .flat_map(|n| n.expected_output_paths.iter())
            .filter(|p| !p.is_empty())
            .cloned()
            .collect();

        if root_paths.is_empty() {
            // Roots have no expected outputs — nothing to check.
            return None;
        }

        // --- FindMissingPaths for roots only ------------------------
        let mut fmp_req = tonic::Request::new(FindMissingPathsRequest {
            store_paths: root_paths.clone(),
        });
        rio_proto::interceptor::inject_current(fmp_req.metadata_mut());
        // JWT propagation — same as r[sched.merge.substitute-probe].
        if let Some(t) = jwt_token
            && let Ok(v) = tonic::metadata::MetadataValue::try_from(t)
        {
            fmp_req
                .metadata_mut()
                .insert(rio_common::jwt_interceptor::TENANT_TOKEN_HEADER, v);
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
            roots = root_paths.len(),
            missing = resp.missing_paths.len(),
            substitutable = resp.substitutable_paths.len(),
            "top-down: FindMissingPaths response"
        );

        // --- All-or-nothing: every root output available? -----------
        // "Available" = present in store (NOT in missing_paths) OR
        // substitutable upstream. A single unavailable root → fall
        // through to the full merge.
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
        if root_paths
            .iter()
            .any(|p| truly_missing.contains(p.as_str()))
        {
            debug!(
                missing = truly_missing.len(),
                "top-down: root output(s) unavailable; falling through to full merge"
            );
            return None;
        }

        // --- Eager-fetch substitutable root NARs --------------------
        // Same pattern as r[sched.merge.substitute-fetch]: QPI with
        // JWT triggers the store's NAR fetch. Bounded concurrency;
        // root count is small (usually 1) so this is fast.
        //
        // Reborrow store_client: the breaker calls above took &mut
        // self, invalidating the earlier shared borrow.
        let store_client = self.store_client.as_ref()?;
        if !resp.substitutable_paths.is_empty() {
            use futures_util::stream::{self, StreamExt};
            let jwt_pair;
            let jwt_meta: &[(&'static str, &str)] = match jwt_token {
                Some(t) => {
                    jwt_pair = [(rio_common::jwt_interceptor::TENANT_TOKEN_HEADER, t)];
                    &jwt_pair
                }
                None => &[],
            };
            let mut fetches = stream::iter(resp.substitutable_paths)
                .map(|p| {
                    let mut c = store_client.clone();
                    async move {
                        let res = rio_proto::client::query_path_info_opt(
                            &mut c,
                            &p,
                            grpc_timeout,
                            jwt_meta,
                        )
                        .await;
                        (p, res)
                    }
                })
                .buffer_unordered(self.substitute_max_concurrent);
            while let Some((p, res)) = fetches.next().await {
                // Fetch failure aborts the short-circuit. The full
                // merge + check_cached_outputs will retry and demote
                // to cache-miss if it fails again. Split Ok(None) vs
                // Err so the log shows WHICH failure mode fired — a
                // sub-20ms NotFound suggests the store's upstream
                // probe never actually fetched (JWT/metadata issue),
                // vs a transport error which is a different bug.
                match res {
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        debug!(
                            path = %p,
                            "top-down: root NotFound on QPI; falling through"
                        );
                        metrics::counter!("rio_scheduler_substitute_fetch_failures_total")
                            .increment(1);
                        return None;
                    }
                    Err(e) => {
                        debug!(
                            path = %p,
                            error = %e,
                            "top-down: root QPI error; falling through"
                        );
                        metrics::counter!("rio_scheduler_substitute_fetch_failures_total")
                            .increment(1);
                        return None;
                    }
                }
            }
            metrics::counter!("rio_scheduler_cache_hits_total", "source" => "substitute")
                .increment(root_paths.len() as u64);
        }

        // All roots available and fetched. Return owned root nodes
        // for the pruned merge.
        Some(roots.into_iter().cloned().collect())
    }
}
