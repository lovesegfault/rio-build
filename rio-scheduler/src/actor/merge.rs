//! DAG merge handling: SubmitBuild → merge client DAG into global DAG.

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
        } = req;
        // rio_scheduler_builds_total is incremented at terminal transition
        // (complete_build/transition_build_to_failed/handle_cancel_build)
        // with an outcome label, so SLI queries can compute success rate.

        // === Step 1: DB build row ==================================
        // If this fails, nothing is in memory; caller gets a clean error.
        self.db
            .insert_build(build_id, tenant_id.as_deref(), priority_class)
            .await?;

        // === Step 2: DAG merge (BEFORE in-memory map inserts) ========
        // If merge fails (cycle), nothing is in the actor's maps; only the
        // DB build row exists, which we best-effort delete. This ordering
        // prevents the leak where a cyclic submission left permanent entries
        // in build_events/build_sequences/builds with no cleanup scheduled.
        let merge_result = match self.dag.merge(build_id, &nodes, &edges) {
            Ok(r) => r,
            Err(e) => {
                // Best-effort: clean up the orphan build row.
                if let Err(db_e) = self.db.delete_build(build_id).await {
                    warn!(build_id = %build_id, error = %db_e,
                          "failed to delete orphan build row after DAG merge failure");
                }
                return Err(ActorError::Internal(format!("DAG merge failed: {e}")));
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

        // === Step 4: DB persistence with rollback on error ============
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

        // Transition build to active. If this fails, roll back everything.
        if let Err(e) = self.transition_build(build_id, BuildState::Active).await {
            error!(build_id = %build_id, error = %e, "transition to Active failed; rolling back");
            self.cleanup_failed_merge(build_id, &merge_result).await;
            return Err(e);
        }

        // === Step 5: Post-Active processing ==========================
        // From here on, DB write failures are log-and-continue (build is
        // Active and in a valid state; DB sync will catch up on next status
        // update or heartbeat reconciliation).

        // Index proto nodes by hash for efficient lookup during the transition loop.
        let node_index: HashMap<&str, &rio_proto::types::DerivationNode> =
            nodes.iter().map(|n| (n.drv_hash.as_str(), n)).collect();

        let total_derivations = nodes.len() as u32;
        let mut cached_count = 0u32;

        // Scheduler-side cache check: query the store for expected_output_paths
        // of newly-inserted derivations. If all outputs exist, skip straight to
        // Completed. This closes the TOCTOU window between the gateway's
        // FindMissingPaths and our merge (another build may have completed the
        // derivation in between).
        //
        // Err(StoreUnavailable) → circuit breaker is open (sustained store
        // outage). Roll back the merge and reject the whole SubmitBuild. The
        // alternative — queueing with 100% cache miss — causes a rebuild
        // avalanche once the store recovers.
        let cached_hashes = match self.check_cached_outputs(newly_inserted, &node_index).await {
            Ok(hashes) => hashes,
            Err(e) => {
                // Same rollback as persist_merge_to_db failure: undo the DAG
                // merge, the map inserts, and the DB build row. Without this,
                // the build would be half-committed (Active in the DB but
                // rejected to the client).
                error!(build_id = %build_id, error = %e, "cache-check circuit breaker open; rolling back merge");
                self.cleanup_failed_merge(build_id, &merge_result).await;
                return Err(e);
            }
        };

        for drv_hash in &cached_hashes {
            let Some(node) = node_index.get(drv_hash.as_str()) else {
                continue;
            };
            if let Some(state) = self.dag.node_mut(drv_hash) {
                if let Err(e) = state.transition(DerivationStatus::Completed) {
                    warn!(drv_hash = %drv_hash, error = %e, "cache-hit Created->Completed transition failed");
                    continue;
                }
                state.output_paths = node.expected_output_paths.clone();
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
                        rio_proto::types::DerivationEvent {
                            derivation_path: node.drv_path.clone(),
                            status: Some(rio_proto::types::derivation_event::Status::Cached(
                                rio_proto::types::DerivationCached {
                                    output_paths: node.expected_output_paths.clone(),
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
        // subgraph raises their priority. D5 reads these for BinaryHeap
        // ordering.
        crate::critical_path::compute_initial(&mut self.dag, &self.estimator, newly_inserted);

        // Compute initial states for the remaining (non-cached) newly-inserted
        // derivations. Cached derivations above are now Completed, so their
        // dependents will correctly be computed as Ready here.
        let remaining_new: HashSet<DrvHash> = newly_inserted
            .iter()
            .filter(|h| !cached_hashes.contains(h.as_str()))
            .cloned()
            .collect();
        let initial_states = self.dag.compute_initial_states(&remaining_new);

        // Interactive builds (IFD) get priority: push_front instead of push_back
        let is_interactive = self
            .builds
            .get(&build_id)
            .is_some_and(|b| b.priority_class.is_interactive());

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
                        if is_interactive {
                            self.ready_queue.push_front(drv_hash.clone());
                        } else {
                            self.ready_queue.push_back(drv_hash.clone());
                        }
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

        // Also handle nodes that already existed and are already completed
        for node in &nodes {
            if !newly_inserted.contains(node.drv_hash.as_str())
                && let Some(state) = self.dag.node(&node.drv_hash)
                && state.status() == DerivationStatus::Completed
            {
                cached_count += 1;
                metrics::counter!("rio_scheduler_cache_hits_total", "source" => "existing")
                    .increment(1);
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
        nodes: &[rio_proto::types::DerivationNode],
        edges: &[rio_proto::types::DerivationEdge],
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
                }
            })
            .collect();

        // Transaction: 3 batched roundtrips instead of 2N+E serial.
        let mut tx = self.db.pool().begin().await?;

        // Batch 1: upsert all derivations, get back drv_hash -> db_id map.
        let id_map = crate::db::SchedulerDb::batch_upsert_derivations(&mut tx, &node_rows).await?;

        // Update in-memory db_id from returned map.
        for (hash, db_id) in &id_map {
            if let Some(state) = self.dag.node_mut(hash) {
                state.db_id = Some(*db_id);
            }
        }

        // Batch 2: link all nodes to this build.
        let db_ids: Vec<Uuid> = id_map.values().copied().collect();
        crate::db::SchedulerDb::batch_insert_build_derivations(&mut tx, build_id, &db_ids).await?;

        // Batch 3: insert edges (resolve drv_path -> db_id via find_db_id_by_path).
        let edge_rows: Result<Vec<(Uuid, Uuid)>, ActorError> = edges
            .iter()
            .map(|e| {
                let parent = self.find_db_id_by_path(&e.parent_drv_path).ok_or_else(|| {
                    ActorError::Internal(format!(
                        "edge {} -> {} references unpersisted derivation (db_id missing)",
                        e.parent_drv_path, e.child_drv_path
                    ))
                })?;
                let child = self.find_db_id_by_path(&e.child_drv_path).ok_or_else(|| {
                    ActorError::Internal(format!(
                        "edge {} -> {} references unpersisted derivation (db_id missing)",
                        e.parent_drv_path, e.child_drv_path
                    ))
                })?;
                Ok((parent, child))
            })
            .collect();
        let edge_rows = edge_rows?;
        crate::db::SchedulerDb::batch_insert_edges(&mut tx, &edge_rows).await?;

        tx.commit().await?;
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

    /// Query the store for newly-inserted derivations' expected outputs and
    /// return the set of drv_hashes whose outputs are all already present.
    ///
    /// # Circuit breaker interaction
    ///
    /// - Store unreachable + breaker closed + under threshold → `Ok(empty set)`.
    ///   Build proceeds as 100% cache miss. Slightly wasteful but recoverable.
    /// - Store unreachable + breaker trips open (or was already open) →
    ///   `Err(StoreUnavailable)`. SubmitBuild is rejected entirely.
    /// - Store reachable → `Ok(cached set)`. Breaker closes (if it was open,
    ///   this was a successful half-open probe).
    /// - No store client (tests) → `Ok(empty set)`. Breaker not touched.
    /// - No expected_output_paths → `Ok(empty set)`. Breaker not touched
    ///   (we didn't actually probe the store, so no signal either way).
    ///
    /// `&mut self` because the breaker is actor-owned state.
    async fn check_cached_outputs(
        &mut self,
        newly_inserted: &HashSet<DrvHash>,
        node_index: &HashMap<&str, &rio_proto::types::DerivationNode>,
    ) -> Result<HashSet<DrvHash>, ActorError> {
        let Some(store_client) = &self.store_client else {
            return Ok(HashSet::new());
        };

        // Collect all expected output paths for newly-inserted derivations.
        // Skip nodes without expected_output_paths (old gateways, unresolvable leaf nodes).
        let check_paths: Vec<String> = newly_inserted
            .iter()
            .filter_map(|h| node_index.get(h.as_str()))
            .flat_map(|n| n.expected_output_paths.iter().cloned())
            .collect();

        if check_paths.is_empty() {
            // Didn't probe the store — no signal for the breaker. If it's
            // open, it stays open; if closed, stays closed. Returning
            // Ok here (not checking breaker.is_open()) is deliberate: a
            // submission with no expected_output_paths is unaffected by
            // store availability (there's nothing to cache-check), so
            // rejecting it with StoreUnavailable would be a false positive.
            return Ok(HashSet::new());
        }

        // Wrap in a timeout: this is a synchronous call inside the
        // single-threaded actor event loop. If the store hangs, NO heartbeats,
        // completions, or dispatches are processed until this returns.
        //
        // This call is ALSO the half-open probe: if the breaker is open, we
        // still make the call. Success → close; failure → stay open + reject.
        let resp = match tokio::time::timeout(
            rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
            store_client
                .clone()
                .find_missing_paths(FindMissingPathsRequest {
                    store_paths: check_paths.clone(),
                }),
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
                // Under threshold: proceed with empty cache-hit set.
                // The build runs with 100% miss — wasteful, but N<5 of
                // these is tolerable. The breaker catches sustained outages.
                return Ok(HashSet::new());
            }
            Err(_) => {
                warn!(
                    timeout = ?rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
                    "store FindMissingPaths timed out"
                );
                metrics::counter!("rio_scheduler_cache_check_failures_total").increment(1);
                if self.cache_breaker.record_failure() {
                    return Err(ActorError::StoreUnavailable);
                }
                return Ok(HashSet::new());
            }
        };

        let missing: HashSet<String> = resp.missing_paths.into_iter().collect();
        let present: HashSet<String> = check_paths
            .into_iter()
            .filter(|p| !missing.contains(p))
            .collect();

        // A derivation is cached if it has at least one expected output path
        // AND all of them are present.
        Ok(newly_inserted
            .iter()
            .filter(|h| {
                node_index.get(h.as_str()).is_some_and(|n| {
                    !n.expected_output_paths.is_empty()
                        && n.expected_output_paths.iter().all(|p| present.contains(p))
                })
            })
            .cloned()
            .collect())
    }
}
