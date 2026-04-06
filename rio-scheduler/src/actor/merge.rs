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
        } = req;
        // rio_scheduler_builds_total is incremented at terminal transition
        // (complete_build/transition_build_to_failed/handle_cancel_build)
        // with an outcome label, so SLI queries can compute success rate.

        // === Step 1: DB build row ==================================
        // If this fails, nothing is in memory; caller gets a clean error.
        self.db
            .insert_build(build_id, tenant_id, priority_class, keep_going, &options)
            .await?;

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

        // === Step 4: Scheduler-side cache check (BEFORE DB persist) ====
        // Query the store for expected_output_paths of newly-inserted
        // derivations. If all outputs exist, skip straight to Completed.
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
        let cached_hits = match self.check_cached_outputs(newly_inserted, &node_index).await {
            Ok(hits) => hits,
            Err(e) => {
                error!(build_id = %build_id, error = %e, "cache-check circuit breaker open; rolling back merge");
                self.cleanup_failed_merge(build_id, &merge_result).await;
                return Err(e);
            }
        };

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

        for (drv_hash, output_paths) in &cached_hits {
            let Some(node) = node_index.get(drv_hash.as_str()) else {
                continue;
            };
            if let Some(state) = self.dag.node_mut(drv_hash) {
                if let Err(e) = state.transition(DerivationStatus::Completed) {
                    warn!(drv_hash = %drv_hash, error = %e, "cache-hit Created->Completed transition failed");
                    continue;
                }
                // GAP-4 fix: for floating-CA nodes this is the REALIZED path
                // from the realisations table (not the [""] placeholder from
                // expected_output_paths). For IA nodes it's expected_output_paths
                // (same as before). check_cached_outputs populates the right one.
                state.output_paths = output_paths.clone();
                cached_count += 1;
                metrics::counter!("rio_scheduler_cache_hits_total", "source" => "scheduler")
                    .increment(1);

                if let Err(e) = self
                    .db
                    .update_derivation_status(drv_hash, DerivationStatus::Completed, None)
                    .await
                {
                    warn!(drv_hash = %drv_hash, error = %e, "failed to persist cache-hit status");
                }

                self.emit_build_event(
                    build_id,
                    rio_proto::types::build_event::Event::Derivation(
                        rio_proto::dag::DerivationEvent {
                            derivation_path: node.drv_path.clone(),
                            status: Some(rio_proto::dag::derivation_event::Status::Cached(
                                rio_proto::dag::DerivationCached {
                                    output_paths: output_paths.clone(),
                                },
                            )),
                        },
                    ),
                );
            }
        }

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

        // Compute initial states for the remaining (non-cached) newly-inserted
        // derivations. Cached derivations above are now Completed, so their
        // dependents will correctly be computed as Ready here.
        let remaining_new: HashSet<DrvHash> = newly_inserted
            .iter()
            .filter(|h| !cached_hits.contains_key(h.as_str()))
            .cloned()
            .collect();
        let initial_states = self.dag.compute_initial_states(&remaining_new);

        // Track whether any newly inserted node was immediately marked
        // DependencyFailed (because a dep is already poisoned). If so, the
        // build may need to fail (!keepGoing) or terminate early (keepGoing).
        let mut first_dep_failed: Option<DrvHash> = None;

        for (drv_hash, target_status) in &initial_states {
            if let Some(state) = self.dag.node_mut(drv_hash) {
                match target_status {
                    DerivationStatus::Ready => {
                        // Transition created -> queued -> ready
                        if let Err(e) = state.transition(DerivationStatus::Queued) {
                            warn!(drv_hash = %drv_hash, error = %e, "Created->Queued transition failed");
                        }
                        if let Err(e) = state.transition(DerivationStatus::Ready) {
                            warn!(drv_hash = %drv_hash, error = %e, "Queued->Ready transition failed");
                        }
                        if let Err(e) = self
                            .db
                            .update_derivation_status(drv_hash, DerivationStatus::Ready, None)
                            .await
                        {
                            error!(drv_hash = %drv_hash, error = %e, "failed to persist Ready status (build is Active; continuing)");
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
                        if let Err(e) = self
                            .db
                            .update_derivation_status(drv_hash, DerivationStatus::Queued, None)
                            .await
                        {
                            error!(drv_hash = %drv_hash, error = %e, "failed to persist Queued status (build is Active; continuing)");
                        }
                    }
                    DerivationStatus::DependencyFailed => {
                        if let Err(e) = state.transition(DerivationStatus::DependencyFailed) {
                            warn!(drv_hash = %drv_hash, error = %e, "Created->DependencyFailed transition failed");
                        }
                        if let Err(e) = self
                            .db
                            .update_derivation_status(
                                drv_hash,
                                DerivationStatus::DependencyFailed,
                                None,
                            )
                            .await
                        {
                            error!(drv_hash = %drv_hash, error = %e, "failed to persist DependencyFailed status (build is Active; continuing)");
                        }
                        first_dep_failed.get_or_insert_with(|| drv_hash.clone());
                        debug!(
                            drv_hash = %drv_hash,
                            "dep already poisoned at merge; marking DependencyFailed"
                        );
                    }
                    _ => {}
                }
            }
        }

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

        // Update build's cached count
        if let Some(build) = self.builds.get_mut(&build_id) {
            build.cached_count = cached_count;
            build.completed_count = cached_count;
        }

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

        Ok(event_rx)
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
    async fn check_cached_outputs(
        &mut self,
        newly_inserted: &HashSet<DrvHash>,
        node_index: &HashMap<&str, &rio_proto::dag::DerivationNode>,
    ) -> Result<HashMap<DrvHash, Vec<String>>, ActorError> {
        let mut hits: HashMap<DrvHash, Vec<String>> = HashMap::new();

        // --- Floating-CA: realisations-table lookup --------------------
        // GAP-3 fix: CA nodes can't use FindMissingPaths (expected path is
        // ""). Query realisations by (modular_hash, output_name) instead —
        // same lookup resolve_ca_inputs uses at dispatch time. A hit here
        // means a prior build (via gateway wopRegisterDrvOutput or scheduler
        // insert_realisation on completion) already produced this output.
        for h in newly_inserted {
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

        // --- IA / fixed-CA: FindMissingPaths by expected path ----------
        let Some(store_client) = &self.store_client else {
            return Ok(hits);
        };

        // Collect expected output paths for IA newly-inserted derivations.
        // Skip CA nodes (handled above) and nodes without expected_output_paths.
        // Also skip empty-string paths defensively (a stray "" would be
        // reported missing, defeating the cache-hit for that node anyway,
        // but no reason to send it over the wire).
        let check_paths: Vec<String> = newly_inserted
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

        let missing: HashSet<String> = resp.missing_paths.into_iter().collect();
        let present: HashSet<String> = check_paths
            .into_iter()
            .filter(|p| !missing.contains(p))
            .collect();

        // An IA derivation is cached if it has at least one expected output
        // path AND all of them are present.
        for h in newly_inserted {
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
}
