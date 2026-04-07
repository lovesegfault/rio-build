//! Completion handling: worker reports build done → update DAG, cascade, emit events.
// r[impl sched.completion.idempotent]
// r[impl sched.critical-path.incremental]

use super::*;

/// Timeout for the CA cutoff-compare realisation lookup.
///
/// `query_prior_realisation` is an indexed point-lookup on
/// `realisations(output_path)` — sub-10ms when PG is healthy.
/// `DEFAULT_GRPC_TIMEOUT` (30s) is the "unary RPC over an unreliable
/// link" budget; a PG point-lookup doesn't need it. 2s is generous
/// for the lookup + one retry-worth of PG jitter.
///
/// This is a module constant (NOT plumbed through `grpc_timeout`)
/// because the CA compare runs INSIDE the single-threaded actor event
/// loop and its worst-case latency is the gating concern — callers
/// adjusting `grpc_timeout` for other reasons (tests, degraded links)
/// shouldn't accidentally widen this budget.
const CA_CUTOFF_LOOKUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Per-candidate prior realisation discovered during the CA cutoff
/// cascade walk. Carries everything needed to (a) verify the output
/// exists in the store and (b) stamp the skipped node with its
/// output_path + insert a realisation for the gateway's
/// QueryRealisation.
#[derive(Debug, Clone)]
struct CaCutoffVerified {
    output_name: String,
    output_path: String,
    output_hash: [u8; 32],
}

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
        executor_id: Option<&ExecutorId>,
    ) {
        if let Err(e) = self
            .db
            .update_derivation_status(drv_hash, status, executor_id)
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

    /// Batch variant of [`persist_status`]: one PG round-trip for N
    /// derivations. Used by `cancel_build_derivations` where the
    /// per-item loop caused N+1 actor stall (500-drv cancel = ~1000
    /// sequential awaits blocking heartbeats). Same best-effort
    /// semantics: logs error!, never propagates.
    ///
    /// [`persist_status`]: Self::persist_status
    pub(super) async fn persist_status_batch(&self, drv_hashes: &[&str], status: DerivationStatus) {
        if let Err(e) = self
            .db
            .update_derivation_status_batch(drv_hashes, status)
            .await
        {
            error!(count = drv_hashes.len(), ?status, error = %e,
                   "failed to batch-persist derivation status");
        }
    }

    /// Batch variant of [`unpin_best_effort`]. Same best-effort
    /// semantics (debug-log, never propagate); `sweep_stale_live_pins`
    /// on recovery backstops any missed unpins.
    ///
    /// [`unpin_best_effort`]: Self::unpin_best_effort
    pub(super) async fn unpin_best_effort_batch(&self, drv_hashes: &[&str]) {
        if let Err(e) = self.db.unpin_live_inputs_batch(drv_hashes).await {
            debug!(count = drv_hashes.len(), error = %e,
                   "failed to batch-unpin live inputs (best-effort)");
        }
    }

    // r[impl sched.gc.path-tenants-upsert]
    /// Best-effort `path_tenants` upsert for a derivation that just
    /// reached a completed-equivalent state (Completed/Skipped/
    /// cache-hit). Resolves `interested_builds → tenant_ids` via the
    /// builds map, collects the node's `output_paths`, calls
    /// `db.upsert_path_tenants`. Logs warn! on failure — GC may
    /// under-retain, but never blocks the completion flow. The 24h
    /// global grace is the fallback.
    ///
    /// Called at every path that marks a derivation's outputs as
    /// "this tenant wants them": handle_success_completion, CA-cutoff
    /// skipped nodes, merge-time cache hits, merge-time pre-existing
    /// Completed, recovery orphan-completion. Missing any of these =
    /// paths GC'd prematurely under that tenant's retention policy.
    pub(super) async fn upsert_path_tenants_for(&self, drv_hash: &DrvHash) {
        let Some(state) = self.dag.node(drv_hash) else {
            return;
        };
        if state.output_paths.is_empty() {
            return;
        }
        // tenant_id: Option<Uuid> — filter_map drops None
        // (single-tenant mode; empty SSH-key comment → gateway sends
        // "" → scheduler stores None). Only builds with a resolved
        // tenant contribute.
        let tenant_ids: Vec<Uuid> = state
            .interested_builds
            .iter()
            .filter_map(|id| self.builds.get(id)?.tenant_id)
            .collect();
        if tenant_ids.is_empty() {
            return;
        }
        if let Err(e) = self
            .db
            .upsert_path_tenants(&state.output_paths, &tenant_ids)
            .await
        {
            warn!(
                drv_hash = %drv_hash, ?e,
                output_paths = state.output_paths.len(),
                tenants = tenant_ids.len(),
                "path_tenants upsert failed; GC retention may under-retain"
            );
        }
    }

    /// Walk downstream from `trigger` (a CA-unchanged completion) and
    /// discover prior output_paths for each cascade candidate via the
    /// `realisation_deps` reverse walk. Returns candidates whose
    /// prior outputs ALL exist in the store, along with the output
    /// metadata needed to stamp the skipped node + insert its
    /// realisation for the gateway.
    ///
    /// The walk: `trigger`'s prior modular_hash → `realisation_deps`
    /// reverse → prior downstream modular_hashes + output_paths.
    /// Match prior outputs to current DAG candidates by name-suffix
    /// (`output_path` ends with `-{pname}` or `-{pname}-{output}` for
    /// non-out outputs). Verify existence via FindMissingPaths.
    ///
    /// The realisation-based trigger check (`query_prior_realisation`)
    /// already ensures a prior build existed — first-ever builds
    /// return `None` there, so `ca_output_unchanged` stays false and
    /// this verify never runs. That replaces the old
    /// `ContentLookup(exclude_store_path)` defense which was broken
    /// for CA (same content → same path → self-exclusion filtered
    /// out the only matching row).
    ///
    /// Batch: single FindMissingPaths RPC covering all discovered
    /// prior outputs. Bounded by MAX_CASCADE_NODES on both the in-mem
    /// DAG walk AND the PG realisation_deps walk.
    async fn verify_cutoff_candidates(
        &self,
        trigger: &DrvHash,
        prior_seeds: &[(Vec<u8>, String)],
    ) -> HashMap<DrvHash, Vec<CaCutoffVerified>> {
        use rio_proto::types::FindMissingPathsRequest;
        let Some(store_client) = &self.store_client else {
            return HashMap::new();
        };

        // In-mem speculative BFS: collect current DAG's cascade
        // candidates. Same over-approximation as before — the actual
        // cascade re-checks eligibility.
        let (candidates, _cap) = crate::dag::DerivationDag::speculative_cascade_reachable(
            trigger,
            crate::dag::MAX_CASCADE_NODES,
            |current, provisional| {
                self.dag
                    .find_cutoff_eligible_speculative(current, provisional)
            },
        );
        if candidates.is_empty() {
            return HashMap::new();
        }

        // PG realisation_deps reverse walk: from the trigger's PRIOR
        // modular_hash(es), find all previously-built dependents.
        // Uses realisation_deps_reverse_idx (migration 015, indexed
        // explicitly "for cutoff cascade").
        let prior_outputs = match tokio::time::timeout(
            CA_CUTOFF_LOOKUP_TIMEOUT,
            crate::ca::walk_dependent_realisations(
                self.db.pool(),
                prior_seeds,
                crate::dag::MAX_CASCADE_NODES,
            ),
        )
        .await
        {
            Ok(Ok(m)) => m,
            Ok(Err(e)) => {
                debug!(error = %e, "CA cutoff verify: realisation_deps walk failed; skipping cascade");
                return HashMap::new();
            }
            Err(_elapsed) => {
                debug!("CA cutoff verify: realisation_deps walk timed out; skipping cascade");
                return HashMap::new();
            }
        };
        if prior_outputs.is_empty() {
            debug!(
                trigger = %trigger,
                n_candidates = candidates.len(),
                "CA cutoff verify: no prior dependents in realisation_deps; skipping cascade"
            );
            return HashMap::new();
        }

        // Match current DAG candidates to prior outputs by name
        // suffix. A CA output_path is `/nix/store/HASH-{name}` (or
        // `-{name}-{outputName}` for non-out). `pname` is the most
        // reliable invariant across builds with different drv hashes
        // but identical content. Multiple prior outputs may share a
        // name suffix (pathological: two versions of the same pname
        // in one prior build) — first match wins; ambiguity degrades
        // to no-skip for that candidate (conservative).
        let mut cand_to_prior: HashMap<DrvHash, Vec<CaCutoffVerified>> = HashMap::new();
        let mut check_paths: Vec<String> = Vec::new();
        for hash in &candidates {
            let Some(state) = self.dag.node(hash) else {
                continue;
            };
            let Some(pname) = &state.pname else {
                // No pname → can't match by suffix → conservative exclude.
                continue;
            };
            let mut matched: Vec<CaCutoffVerified> = Vec::new();
            for out_name in &state.output_names {
                let suffix = if out_name == "out" {
                    format!("-{pname}")
                } else {
                    format!("-{pname}-{out_name}")
                };
                // Find a prior output whose path ends with this
                // suffix. Linear scan over prior_outputs — bounded
                // by MAX_CASCADE_NODES, so small-N.
                if let Some(((_, prior_out), (path, oh))) = prior_outputs
                    .iter()
                    .find(|((_, on), (p, _))| on == out_name && p.ends_with(&suffix))
                {
                    matched.push(CaCutoffVerified {
                        output_name: prior_out.clone(),
                        output_path: path.clone(),
                        output_hash: *oh,
                    });
                    check_paths.push(path.clone());
                }
            }
            // All outputs must have a prior match. Partial match →
            // exclude (conservative: can't skip if we don't know
            // where ALL outputs are).
            if matched.len() == state.output_names.len() && !matched.is_empty() {
                cand_to_prior.insert(hash.clone(), matched);
            }
        }
        if check_paths.is_empty() {
            return HashMap::new();
        }

        // Batch FindMissingPaths: verify all discovered prior outputs
        // still exist in the store (not GC'd). Failure → empty
        // verified set (safe fallback; downstream runs normally).
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
                debug!(error = %e, "CA cutoff verify: FindMissingPaths failed; skipping cascade");
                return HashMap::new();
            }
            Err(_elapsed) => {
                debug!("CA cutoff verify: FindMissingPaths timed out; skipping cascade");
                return HashMap::new();
            }
        };

        // Verified = candidates where ALL prior outputs are present.
        cand_to_prior
            .into_iter()
            .filter(|(_, outs)| outs.iter().all(|o| !missing.contains(&o.output_path)))
            .collect()
    }

    // r[impl sched.fod.size-class-reactive]
    // r[impl sched.builder.size-class-reactive]
    /// I-170/I-177: bump a derivation's `size_class_floor` to one
    /// above `failed_class` so the next dispatch skips already-tried
    /// sizes. No clean OOM signal reaches the scheduler — pod death
    /// is a disconnect — so this fires on ANY failure path; over-
    /// promoting is cheap (one larger pod), retry-storm on tiny is
    /// not (poison-loop).
    ///
    /// I-177: generalized from FOD-only. 103 tiny builders OOMKilled
    /// on bootstrap-stage0-glibc / gnu-config / llvm-src; the
    /// `is_fixed_output` guard meant non-FOD OOMs left floor=NULL,
    /// and the EMA classifier (success-only samples, see
    /// `update_build_history`) kept routing to tiny → poison.
    /// Branches on `is_fixed_output` to pick which class list to
    /// walk: FOD → `fetcher_size_classes` (config-order); non-FOD →
    /// `size_classes` (cutoff-order).
    ///
    /// I-173: extracted from [`Self::record_failure_and_check_poison`]
    /// so the Assigned-disconnect path (`reassign_derivations`, I-097
    /// guard) can promote WITHOUT recording a failure. An OOM'd
    /// executor disconnects with the drv often still Assigned (Running
    /// ack not yet processed); coupling promotion to poison-record
    /// meant 14 live OOMs left `size_class_floor=NULL` and retry-
    /// stormed on tiny. Takes the resolved class string (NOT
    /// executor_id) because `handle_executor_disconnected` removes
    /// the executor from `self.executors` before reassign — the
    /// caller threads the captured class through.
    ///
    /// No-op for `failed_class=None`, largest class, unknown class,
    /// feature off (relevant class list empty), or floor already at
    /// target. Idempotent.
    pub(super) async fn promote_size_class_floor(
        &mut self,
        drv_hash: &DrvHash,
        failed_class: Option<&str>,
    ) {
        let Some(from) = failed_class else { return };
        let mut promoted_to: Option<String> = None;
        if let Some(state) = self.dag.node_mut(drv_hash) {
            let (to, kind) = if state.is_fixed_output {
                let to = crate::assignment::next_fetcher_class(from, &self.fetcher_size_classes);
                (to, "fod")
            } else {
                // parking_lot guard scope: ends here, before the
                // persist `.await` below (guard is !Send).
                let classes = self.size_classes.read();
                let to = crate::assignment::next_builder_class(from, &classes);
                (to, "builder")
            };
            let Some(to) = to else { return };
            // Change-detector, NOT an ordinal compare — fires on any
            // `to != current floor`, not strictly `to > floor`. The
            // "only ever bumps UP" property is structural: dispatch
            // clamps assignments at floor (FOD + non-FOD as of I-177),
            // so `from` ≥ floor, so `next_*_class(from)` is the class
            // above floor (or None at top). A floor=large drv can't be
            // assigned to tiny, so the demote case is unreachable in
            // steady state. The `!=` form just skips the no-op write +
            // metric when floor is already at `to` (e.g. floor=small,
            // overflow-placed on tiny, `next(tiny)=small` — equal, no
            // log spam).
            if state.size_class_floor.as_deref() != Some(to.as_str()) {
                info!(
                    drv_hash = %drv_hash, kind, from = %from, to = %to,
                    "transient failure: promoting size_class_floor"
                );
                metrics::counter!(
                    "rio_scheduler_size_class_promotions_total",
                    "kind" => kind, "from" => from.to_owned(), "to" => to.clone()
                )
                .increment(1);
                if state.is_fixed_output {
                    // Back-compat alias for existing dashboards;
                    // prefer the kind-labeled metric above.
                    metrics::counter!(
                        "rio_scheduler_fod_size_class_promotions_total",
                        "from" => from.to_owned(), "to" => to.clone()
                    )
                    .increment(1);
                }
                state.size_class_floor = Some(to.clone());
                promoted_to = Some(to);
            }
        }
        // P0556: persist the floor so failover doesn't reset it →
        // re-OOM on tiny. Outside the node_mut borrow (await point);
        // best-effort — a lost write degrades to one wasted retry,
        // same as pre-P0556 behavior.
        if let Some(to) = promoted_to
            && let Err(e) = self.db.update_size_class_floor(drv_hash, &to).await
        {
            error!(drv_hash = %drv_hash, to = %to, error = %e,
                   "failed to persist size_class_floor");
        }
    }

    /// Record a worker failure for `drv_hash` (in-mem + PG
    /// best-effort) and return whether the poison threshold is
    /// reached. Caller decides: poison_and_cascade if true,
    /// reset_to_ready + retry if false.
    ///
    /// Both `failed_builders` (HashSet — for distinct-mode + best_executor
    /// exclusion) and `failure_count` (flat counter — for non-distinct
    /// mode) are updated; [`PoisonConfig::is_poisoned`] picks the
    /// right one. The in-mem insert comes first (scheduler-
    /// authoritative); PG is recovery-only. A PG blip degrades to
    /// "might retry on the same worker once post-recovery."
    pub(super) async fn record_failure_and_check_poison(
        &mut self,
        drv_hash: &DrvHash,
        executor_id: &ExecutorId,
    ) -> bool {
        // I-170/I-173/I-177: promote size_class_floor on any recorded
        // failure (FOD or builder). The executor lookup here covers
        // handle_transient_failure and recovery-reconcile (executor
        // still in self.executors). The reassign_derivations caller
        // promotes separately with the class captured BEFORE executor
        // removal — on the disconnect path this lookup returns None,
        // and the second promote is the one that fires; on drain/
        // backstop paths both fire, idempotent.
        let failed_class = self
            .executors
            .get(executor_id)
            .and_then(|e| e.size_class.clone());
        self.promote_size_class_floor(drv_hash, failed_class.as_deref())
            .await;
        if let Some(state) = self.dag.node_mut(drv_hash) {
            state.failed_builders.insert(executor_id.clone());
            // Unconditional (doesn't check HashSet::insert's bool) —
            // same worker counts twice. Only used when
            // require_distinct_workers=false.
            state.failure_count += 1;
        }
        if let Err(e) = self.db.append_failed_worker(drv_hash, executor_id).await {
            error!(drv_hash = %drv_hash, executor_id = %executor_id, error = %e,
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

    #[instrument(skip(self, result), fields(executor_id = %executor_id, drv_key = %drv_key))]
    pub(super) async fn handle_completion(
        &mut self,
        executor_id: &ExecutorId,
        // gRPC layer passes CompletionReport.drv_path; may be a drv_hash in tests.
        drv_key: &str,
        result: rio_proto::build_types::BuildResult,
        // CompletionReport resource fields. 0 = worker had no signal
        // (build failed before cgroup populated). Converted to None
        // before the DB write so the EMA isn't dragged toward zero.
        //
        // Tuple to stay under clippy's 7-arg limit. All three are
        // "resource measurements from the cgroup" with identical
        // zero-means-no-signal semantics; unpacked immediately.
        (peak_memory_bytes, output_size_bytes, peak_cpu_cores): (u64, u64, f64),
    ) {
        let status = rio_proto::build_types::BuildResultStatus::try_from(result.status)
            .unwrap_or_else(|_| {
                tracing::warn!(
                    raw_status = result.status,
                    "unknown BuildResultStatus from worker, treating as Unspecified"
                );
                rio_proto::build_types::BuildResultStatus::Unspecified
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
            // Drv not in DAG (reaped after build-terminal, or truly
            // unknown). running_builds is keyed by drv_hash which we
            // can't recover from drv_key here — but the heartbeat
            // reconcile drops entries whose DAG node is gone (executor.rs
            // still_inflight check). The next heartbeat (~10s) frees
            // any such phantom. The WARN includes executor_id so the
            // race is traceable in logs.
            warn!(
                executor_id = %executor_id,
                key = drv_key,
                "completion for unknown derivation, ignoring \
                 (running_build entry, if any, freed by next heartbeat reconcile)"
            );
            return;
        };
        let drv_hash = &drv_hash;

        // Free worker capacity NOW, before any early-return below. The
        // early-return guards (already-Completed, already-terminal,
        // stale-executor) all mean "this completion's per-derivation
        // work is moot" — but the executor's slot must still free.
        // I-042: before this hoist, a completion arriving for an
        // already-Poisoned derivation (parallel-retry race: drv assigned
        // to executor-B, executor-A's prior completion poisoned it,
        // executor-B's completion arrives) hit the not-Assigned/Running
        // early-return and leaked executor-B's slot. The heartbeat
        // reconcile (executor.rs:608) eventually frees it via the
        // still_inflight filter, but that's a ~10s delay (one
        // heartbeat) of dead capacity per leaked slot.
        //
        // Idempotent: clearing None or a different drv is a no-op.
        // The later clear below is now redundant for the happy path
        // but kept for clarity (and harmless).
        if let Some(worker) = self.executors.get_mut(executor_id)
            && worker.running_build.as_ref() == Some(drv_hash)
        {
            worker.running_build = None;
            // I-197: record that THIS drv terminated on THIS executor.
            // `reassign_derivations` reads it on disconnect to tell
            // OOMKilled-mid-build (last_completed != running) from the
            // I-188 post-completion race (last_completed == running).
            // Any terminal report counts — success, failure, infra,
            // cancelled — the build is no longer "in flight" either way.
            worker.last_completed = Some(drv_hash.clone());
            // r[impl sched.ephemeral.no-redispatch-after-completion]
            // I-188: every executor is one-shot — it exits after this
            // completion. Mark it draining NOW, before dispatch_ready
            // below, so the freed slot isn't re-assigned a dependent
            // the executor will never start. Without this, the
            // dependent goes Assigned → ExecutorDisconnected →
            // reassign, and (pre-I-188) walked the size ladder via
            // spurious floor promotion.
            if !worker.draining {
                worker.draining = true;
                debug!(executor_id = %executor_id,
                       "executor completed its build; marking draining (one-shot exit)");
            }
        }

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

        // r[impl sched.completion.idempotent]
        // Stale-report guard: if this completion is from a worker that no
        // longer owns the derivation (reassigned after disconnect/timeout),
        // drop it. The current assigned_executor's report is authoritative.
        // running_builds was already freed above — for the stale executor
        // that's correct (it doesn't own the drv); for the current owner
        // this branch doesn't fire.
        if let Some(assigned) = &state.assigned_executor
            && assigned != executor_id
        {
            debug!(
                drv_hash = %drv_hash,
                stale_worker = %executor_id,
                current_worker = %assigned,
                "dropping stale completion report"
            );
            return;
        }

        match status {
            rio_proto::build_types::BuildResultStatus::Built
            | rio_proto::build_types::BuildResultStatus::Substituted
            | rio_proto::build_types::BuildResultStatus::AlreadyValid => {
                self.handle_success_completion(
                    drv_hash,
                    &result,
                    executor_id,
                    (peak_memory_bytes, output_size_bytes, peak_cpu_cores),
                )
                .await;
            }
            rio_proto::build_types::BuildResultStatus::TransientFailure => {
                // Build ran, exited non-zero. Counts toward poison — 3
                // workers all seeing this means it's not actually transient.
                self.handle_transient_failure(drv_hash, executor_id).await;
            }
            // r[impl sched.retry.per-worker-budget]
            rio_proto::build_types::BuildResultStatus::InfrastructureFailure => {
                // Worker-local problem (FUSE EIO, cgroup setup fail, OOM-
                // kill of the build process). Not the build's fault. Retry
                // WITHOUT inserting into failed_builders.
                self.handle_infrastructure_failure(drv_hash, executor_id, &result.error_msg)
                    .await;
            }
            rio_proto::build_types::BuildResultStatus::PermanentFailure
            | rio_proto::build_types::BuildResultStatus::CachedFailure
            | rio_proto::build_types::BuildResultStatus::DependencyFailed
            | rio_proto::build_types::BuildResultStatus::LogLimitExceeded
            | rio_proto::build_types::BuildResultStatus::OutputRejected
            // NotDeterministic: nix --check failed. Retrying doesn't
            // help — the nondeterminism is in the build itself.
            | rio_proto::build_types::BuildResultStatus::NotDeterministic
            // InputRejected: corrupt/invalid .drv. Same .drv on another
            // worker is still corrupt.
            | rio_proto::build_types::BuildResultStatus::InputRejected => {
                self.handle_permanent_failure(drv_hash, &result.error_msg, executor_id)
                    .await;
            }
            // TimedOut: I-200 promotes size_class_floor and retries on
            // a larger class (longer activeDeadlineSeconds), bounded by
            // max_timeout_retries. After the cap → Cancelled (terminal,
            // retriable-on-resubmit) — same inputs on the LARGEST class
            // → same timeout, so further auto-retry is a storm.
            // Poisoned's 24h TTL is way too aggressive for a timeout.
            rio_proto::build_types::BuildResultStatus::TimedOut => {
                self.handle_timeout_failure(drv_hash, &result.error_msg, executor_id)
                    .await;
            }
            rio_proto::build_types::BuildResultStatus::Cancelled => {
                // Worker reports Cancelled after cgroup.kill. The
                // scheduler already transitioned the DerivationState
                // when it SENT the CancelSignal (see handle_cancel_
                // build / handle_drain_executor), so this report is
                // expected but needs no further action on the
                // derivation itself. Just the worker-capacity cleanup
                // below. Log at debug: every CancelBuild generates
                // one of these per running drv, and it's the happy
                // path for cancel.
                debug!(drv_hash = %drv_hash, executor_id = %executor_id,
                       "cancelled completion report (expected after CancelSignal)");
            }
            rio_proto::build_types::BuildResultStatus::Unspecified => {
                warn!(
                    drv_hash = %drv_hash,
                    status = result.status,
                    "unknown build result status, treating as transient failure"
                );
                self.handle_transient_failure(drv_hash, executor_id).await;
            }
        }

        // Free worker capacity (redundant with the hoist above; kept
        // for clarity, harmless when already None or different).
        if let Some(worker) = self.executors.get_mut(executor_id)
            && worker.running_build.as_ref() == Some(drv_hash)
        {
            worker.running_build = None;
        }

        // Dispatch newly ready derivations
        self.dispatch_ready().await;
    }

    pub(super) async fn handle_success_completion(
        &mut self,
        drv_hash: &DrvHash,
        result: &rio_proto::build_types::BuildResult,
        executor_id: &ExecutorId,
        // Same tuple pattern as handle_completion — clippy 7-arg limit.
        (peak_memory_bytes, output_size_bytes, peak_cpu_cores): (u64, u64, f64),
    ) {
        // I-140: per-phase timing. Same pattern as merge.rs phase!().
        let t_total = std::time::Instant::now();
        let mut t_phase = std::time::Instant::now();
        macro_rules! phase {
            ($name:literal) => {
                tracing::trace!(elapsed = ?t_phase.elapsed(), phase = $name, "completion phase");
                t_phase = std::time::Instant::now();
            };
        }
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
                    executor_id = %executor_id,
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

        // r[impl sched.ca.resolve+2]
        // Realisation insert: for a CA derivation, write
        // `(modular_hash, output_name) → (output_path, output_hash)`
        // to PG NOW, before `find_newly_ready` below queues the
        // dependents for dispatch. The dependents' `maybe_resolve_ca`
        // → `query_realisation` reads this row; without it, resolve
        // fails with `RealisationMissing` and the scheduler dispatches
        // unresolved content → worker fetches the floating-CA input's
        // `.drv` → reads `out.path() == ""` → `invalid store path ""`.
        //
        // The gateway's `wopRegisterDrvOutput` handler inserts for the
        // Nix wire-protocol path, but rio-builders don't speak wire
        // protocol — their upload is `PutPath → CompletionReport`
        // only. This insert closes the gap.
        //
        // Best-effort: PG blip → warn, don't abort completion. The
        // dependent's dispatch-unresolved → worker-fail →
        // `handle_infrastructure_failure` path retries with backoff
        // (after the companion fix), giving PG time to recover. The
        // in-mem transition to Completed already happened; a missing
        // realisation degrades CA-on-CA resolve, not the build itself.
        if let Some(state) = self.dag.node(drv_hash)
            && state.is_ca
            && let Some(modular_hash) = state.ca_modular_hash
        {
            // Log the hash in the same hex-encoding the gateway's
            // wopQueryRealisation handler uses — if nix-build's later
            // QueryRealisation finds nothing, grep both logs for
            // `drv_hash=` and compare. A mismatch = our
            // hash_derivation_modulo diverges from CppNix (the
            // maskOutputs env-masking gap was one such divergence).
            info!(
                drv_hash = %hex::encode(modular_hash),
                outputs = result.built_outputs.len(),
                "insert_realisation: CA build complete, writing realisations"
            );
            for output in &result.built_outputs {
                let Ok(output_hash): Result<[u8; 32], _> = output.output_hash.as_slice().try_into()
                else {
                    debug!(
                        drv_hash = %drv_hash,
                        output_name = %output.output_name,
                        hash_len = output.output_hash.len(),
                        "realisation insert: output_hash not 32 bytes, skipping"
                    );
                    continue;
                };
                if let Err(e) = crate::ca::insert_realisation(
                    self.db.pool(),
                    &modular_hash,
                    &output.output_name,
                    &output.output_path,
                    &output_hash,
                )
                .await
                {
                    warn!(
                        drv_hash = %drv_hash,
                        output_name = %output.output_name,
                        error = %e,
                        "realisation insert failed (best-effort; dependent resolve will retry)"
                    );
                }
            }
        }

        // r[impl sched.ca.cutoff-compare]
        // CA early-cutoff compare: if this was a CA derivation, check
        // each output_path against the realisations table for a PRIOR
        // build (different modular_hash, same path). All-match →
        // P0252's cutoff-propagate cascade (cascade_cutoff) will skip
        // downstream builds. This hook only records the compare result
        // + the prior realisation seeds; propagation happens below.
        //
        // WHY realisation-based, not ContentLookup-based: CA
        // derivations produce IDENTICAL output_paths for identical
        // content (that's the point of CA). The previous
        // `ContentLookup(nar_hash, exclude=output_path)` approach
        // always excluded the only matching row — for CA, the self-
        // exclusion filters out the very evidence of a prior build.
        // Querying realisations by output_path with modular_hash
        // exclusion instead: two builds with different drv envs
        // (hence different modular_hash) but identical content get
        // the same path, so the exclusion leaves the prior build's
        // row visible.
        //
        // First-ever build: no prior realisation → all_matched=false
        // → no cascade. This REPLACES the verify_cutoff_candidates
        // self-match defense (bughunt-mc196) at the source.
        //
        // Best-effort: PG blip → degrade to "no cutoff" (safe —
        // downstream builds run when they could've been skipped).
        //
        // AND-fold: `all_matched` starts true and is &= each lookup.
        // A single miss means the derivation's outputs aren't
        // byte-identical to a prior build as a WHOLE, so downstream
        // can't be skipped. Per-output granularity is a later
        // refinement.
        let mut prior_seeds: Vec<(Vec<u8>, String)> = Vec::new();
        if let Some(state) = self.dag.node(drv_hash)
            && state.is_ca
            && let Some(modular_hash) = state.ca_modular_hash
        {
            let mut all_matched = !result.built_outputs.is_empty();
            for (i, output) in result.built_outputs.iter().enumerate() {
                if output.output_path.is_empty() {
                    debug!(
                        drv_hash = %drv_hash,
                        output_name = %output.output_name,
                        "CA cutoff-compare: empty output_path, counting as malformed"
                    );
                    metrics::counter!(
                        "rio_scheduler_ca_hash_compares_total",
                        "outcome" => "malformed"
                    )
                    .increment(1);
                    all_matched = false;
                    continue;
                }
                // Outcome labels: match/miss distinguish "prior build
                // found" vs "novel content" (both healthy). error
                // distinguishes "PG blip/timeout" (infra problem —
                // alert if rate>0). malformed catches worker-sent
                // garbage. High miss-rate is normal; high error-rate
                // means investigate PG. Cutoff semantics unchanged:
                // all non-match fold to all_matched=false → safe
                // don't-skip.
                let (matched, outcome) = match tokio::time::timeout(
                    CA_CUTOFF_LOOKUP_TIMEOUT,
                    crate::ca::query_prior_realisation(
                        self.db.pool(),
                        &output.output_path,
                        &modular_hash,
                    ),
                )
                .await
                {
                    Ok(Ok(Some(prior))) => {
                        // Found a prior build's realisation for the
                        // same path. Seed the realisation_deps walk
                        // with its (modular_hash, output_name) so
                        // the cascade can discover what was built
                        // downstream of it.
                        prior_seeds.push((prior.drv_hash.to_vec(), prior.output_name));
                        (true, "match")
                    }
                    Ok(Ok(None)) => (false, "miss"),
                    Ok(Err(e)) => {
                        debug!(drv_hash = %drv_hash, error = %e,
                               "CA cutoff-compare: prior-realisation lookup failed");
                        (false, "error")
                    }
                    Err(_elapsed) => {
                        debug!(drv_hash = %drv_hash, timeout = ?CA_CUTOFF_LOOKUP_TIMEOUT,
                               "CA cutoff-compare: prior-realisation lookup timed out");
                        (false, "error")
                    }
                };
                all_matched &= matched;
                metrics::counter!(
                    "rio_scheduler_ca_hash_compares_total",
                    "outcome" => outcome
                )
                .increment(1);
                // Short-circuit: one miss means the derivation's
                // outputs aren't byte-identical AS A WHOLE (AND-
                // fold semantics). Remaining lookups can't flip
                // `all_matched` back to true.
                if !matched {
                    let skipped = result.built_outputs.len() - i - 1;
                    if skipped > 0 {
                        metrics::counter!(
                            "rio_scheduler_ca_hash_compares_total",
                            "outcome" => "skipped_after_miss"
                        )
                        .increment(skipped as u64);
                    }
                    break;
                }
            }
            if let Some(state) = self.dag.node_mut(drv_hash) {
                state.ca_output_unchanged = all_matched;
            }
        }

        // r[impl sched.ca.cutoff-propagate]
        // Cascade: if the compare set ca_output_unchanged=true,
        // transitively skip downstream Queued derivations whose only
        // incomplete dep was this one.
        //
        // First-ever-build defense (bughunt-mc196): now handled at
        // the SOURCE — `query_prior_realisation` excludes by
        // modular_hash (different across builds), not output_path
        // (identical for CA). A first-ever build finds no prior
        // realisation → `ca_output_unchanged=false` → cascade never
        // fires. `verify_cutoff_candidates` adds GC-defense: prior
        // outputs must still exist in the store (FindMissingPaths).
        //
        // Placement BEFORE find_newly_ready: skipped nodes don't get
        // promoted to Ready + pushed to the dispatch queue.
        //
        // `skipped_interested`: union of interested_builds over all
        // skipped nodes. A skipped node may belong to a merged build
        // that the trigger does NOT — without unioning this into the
        // completion-check loop below, that merged build never sees
        // check_build_completion fire and hangs Active forever.
        let mut skipped_interested: HashSet<Uuid> = HashSet::new();
        if self
            .dag
            .node(drv_hash)
            .is_some_and(|s| s.ca_output_unchanged)
        {
            let verified = self.verify_cutoff_candidates(drv_hash, &prior_seeds).await;
            let (skipped, cap_hit) = self
                .dag
                .cascade_cutoff(drv_hash, |h| verified.contains_key(h));
            metrics::counter!("rio_scheduler_ca_cutoff_saves_total")
                .increment(skipped.len() as u64);
            // Sum of estimated durations — lower-bound on wall-clock
            // saved. est_duration is the Estimator's EMA (set at
            // merge time from build_history); for a derivation that
            // has never run, it's the fallback (closure-size proxy
            // or 30s default). Counter not gauge: cumulative across
            // all cascades, like saves_total.
            let seconds_saved: f64 = skipped
                .iter()
                .filter_map(|h| self.dag.node(h).map(|s| s.est_duration))
                .sum();
            metrics::counter!("rio_scheduler_ca_cutoff_seconds_saved")
                .increment(seconds_saved.max(0.0) as u64);
            for hash in &skipped {
                // Stamp the skipped node with the prior build's
                // output_paths AND insert realisations keyed on THIS
                // build's modular_hash. Without the realisation, the
                // gateway's QueryRealisation for (M2_b, out) returns
                // NotFound → client gets empty outPath → assert at
                // nix-build.cc:722. The prior build's realisation is
                // keyed on M1_b (different modular_hash) so the
                // gateway can't find it without this bridge row.
                if let Some(prior_outs) = verified.get(hash) {
                    if let Some(state) = self.dag.node_mut(hash) {
                        state.output_paths =
                            prior_outs.iter().map(|o| o.output_path.clone()).collect();
                    }
                    if let Some(state) = self.dag.node(hash)
                        && let Some(modular) = state.ca_modular_hash
                    {
                        for o in prior_outs {
                            if let Err(e) = crate::ca::insert_realisation(
                                self.db.pool(),
                                &modular,
                                &o.output_name,
                                &o.output_path,
                                &o.output_hash,
                            )
                            .await
                            {
                                warn!(
                                    drv_hash = %hash,
                                    output = %o.output_name,
                                    error = %e,
                                    "CA cutoff: realisation insert for skipped node failed \
                                     (best-effort; gateway QueryRealisation may return empty)"
                                );
                            }
                        }
                    }
                }
                // Collect interested_builds BEFORE persist (though
                // persist doesn't clear them — just for locality with
                // the node-mut block above).
                if let Some(state) = self.dag.node(hash) {
                    skipped_interested.extend(state.interested_builds.iter().copied());
                }
                // r[impl sched.gc.path-tenants-upsert]
                // Skipped node's output_paths (from the prior build's
                // realization, stamped above) need tenant attribution
                // — the build's tenant wants them retained, same as
                // if the node had actually built them.
                self.upsert_path_tenants_for(hash).await;
                self.persist_status(hash, DerivationStatus::Skipped, None)
                    .await;
                debug!(drv_hash = %hash, trigger = %drv_hash,
                       "CA cutoff: skipped (output already in store)");
            }
            // H1 fix (P0399): each newly-Skipped node may have Queued
            // parents whose all-deps are now Completed|Skipped. The
            // find_newly_ready(drv_hash) below at :732 only walks
            // parents of the ORIGINAL trigger — without this loop,
            // verify-rejected parents-of-Skipped hang Queued forever.
            // Example: A→B→C chain, A completes unchanged, verify
            // accepts B (Skipped) but rejects C (Queued). C's only
            // dep is B (now Skipped). find_newly_ready(A) returns []
            // (B is Skipped, not Queued); find_newly_ready(B) returns
            // [C] (C is Queued and all_deps_completed — Skipped now
            // accepted there).
            for s in &skipped {
                for ready_hash in self.dag.find_newly_ready(s) {
                    if let Some(state) = self.dag.node_mut(&ready_hash)
                        && state.transition(DerivationStatus::Ready).is_ok()
                    {
                        self.persist_status(&ready_hash, DerivationStatus::Ready, None)
                            .await;
                        self.push_ready(ready_hash);
                    }
                }
            }
            if cap_hit {
                tracing::warn!(
                    trigger = %drv_hash,
                    node_count = crate::dag::MAX_CASCADE_NODES,
                    skipped = skipped.len(),
                    "CA cutoff cascade hit depth cap; remaining downstream will run normally"
                );
                metrics::counter!("rio_scheduler_ca_cutoff_depth_cap_hits_total").increment(1);
            }
        }

        // r[impl sched.ca.resolve+2]
        // realisation_deps insert: the CA-on-CA resolve at dispatch
        // time recorded every `(dep_modular_hash, dep_output_name)`
        // lookup into `pending_realisation_deps`. The FK ordering
        // means those rows can only land AFTER this derivation's
        // own realisation exists — which `wopRegisterDrvOutput`
        // wrote before BuildComplete arrived (the worker's upload
        // flow: PutPath → RegisterDrvOutput → BuildComplete). So
        // this is the correct point: parent's realisation row is
        // present, dep rows were present at resolve time (we
        // queried them), both FK halves satisfied.
        //
        // Drained via `mem::take`: consumed once. A retry-after-
        // failure that re-dispatches re-runs resolve → fresh
        // `pending_realisation_deps`; the INSERT is ON CONFLICT
        // DO NOTHING so a duplicate attempt is harmless.
        //
        // Best-effort: PG blip → warn, don't abort completion.
        // `realisation_deps` is rio's derived-build-trace cache
        // (ADR-018:45), not correctness-critical for the build.
        if let Some(state) = self.dag.node_mut(drv_hash)
            && let Some(modular_hash) = state.ca_modular_hash
            && !state.pending_realisation_deps.is_empty()
        {
            let lookups = std::mem::take(&mut state.pending_realisation_deps);
            let output_names: Vec<String> = result
                .built_outputs
                .iter()
                .map(|o| o.output_name.clone())
                .collect();
            if let Err(e) = crate::ca::insert_realisation_deps(
                self.db.pool(),
                &modular_hash,
                &output_names,
                &lookups,
            )
            .await
            {
                warn!(
                    drv_hash = %drv_hash,
                    n_lookups = lookups.len(),
                    error = %e,
                    "insert_realisation_deps failed (best-effort cache; completion proceeds)"
                );
            }
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
                // FODs excluded (ADR-019): the rebalancer partitions
                // BUILDER size classes by CPU-bound duration. Fetch
                // durations are network-bound noise; mixing them into
                // the SITA-E partition drags cutoffs toward whatever
                // the upstream mirror's bandwidth happens to be.
                // Fetchers aren't size-classed, so there's nothing to
                // rebalance on their side anyway.
                //
                // peak_memory_bytes passed raw (as i64), not the 0→None
                // filtered Option — the rebalancer wants the full
                // distribution including zeros; 0 is a legitimate
                // sample point ("sub-second build, poller didn't fire").
                // The EMA needs the filter to avoid dragging toward 0;
                // the percentile computation doesn't.
                if !state.is_fixed_output
                    && let Err(e) = self
                        .db
                        .insert_build_sample(
                            pname,
                            &state.system,
                            duration_secs,
                            // Clamp before i64 cast. u64 > i64::MAX wraps
                            // negative → build_samples row with negative
                            // memory → CutoffRebalancer percentiles poisoned.
                            // Physical RAM is well below 2^63 bytes, so this
                            // only fires on a misbehaving worker — but it
                            // costs nothing and prevents silent corruption.
                            peak_memory_bytes.min(i64::MAX as u64) as i64,
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
                    // Drift measured against current cutoffs. Read
                    // guard held for the two sync calls (classify +
                    // cutoff_for) INSIDE a block scope so it drops
                    // BEFORE the `.await` below. parking_lot guards
                    // aren't Send — the borrow checker enforces this
                    // (compile error if the guard crosses .await).
                    //
                    // peak_mem is the 0→None Option from :309 — memory
                    // bump logic in classify() should see the same
                    // filtered signal the original dispatch-time
                    // classify() saw (which also reads from the EMA's
                    // 0→None filtered history).
                    let cutoff_opt = {
                        let classes = self.size_classes.read();
                        if let Some(actual_class) = crate::assignment::classify(
                            duration_secs,
                            peak_mem.map(|m| m as f64),
                            peak_cpu,
                            &classes,
                        ) && &actual_class != assigned_class
                        {
                            metrics::counter!(
                                "rio_scheduler_class_drift_total",
                                "assigned_class" => assigned_class.clone(),
                                "actual_class" => actual_class,
                            )
                            .increment(1);
                        }
                        crate::assignment::cutoff_for(assigned_class, &classes)
                    };

                    if let Some(cutoff) = cutoff_opt
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
                rio_proto::types::build_event::Event::Derivation(rio_proto::dag::DerivationEvent {
                    derivation_path: self.drv_path_or_hash_fallback(drv_hash),
                    status: Some(rio_proto::dag::derivation_event::Status::Completed(
                        rio_proto::dag::DerivationCompleted {
                            output_paths: output_paths.clone(),
                        },
                    )),
                }),
            );
        }

        // Trigger log flush AFTER the Completed event has gone out. By the
        // time the gateway sees Completed, the ring buffer still has the full
        // log (flusher hasn't drained yet — it's async on a separate task).
        // So AdminService.GetBuildLogs can serve from the ring buffer in the
        // gap between Completed and the S3 upload landing.
        self.trigger_log_flush(drv_hash, interested_builds.clone());

        // r[impl sched.gc.path-tenants-upsert]
        self.upsert_path_tenants_for(drv_hash).await;

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
        phase!("4-update-ancestors");

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
        phase!("5-newly-ready");

        // Update build completion status. Union the trigger's
        // interested_builds with skipped nodes' — a CA-cutoff-skipped
        // node may belong to a merged build the trigger does not, and
        // that build needs check_build_completion too.
        let mut check_builds: HashSet<Uuid> = interested_builds.iter().copied().collect();
        check_builds.extend(skipped_interested);
        for build_id in check_builds {
            // I-140: build_summary is O(dag_nodes). Compute ONCE per
            // build, share between counts-persist and progress-emit.
            // Previously each fn ran its own scan → 2× per completion.
            let summary = self.dag.build_summary(build_id);
            self.update_build_counts_with(build_id, &summary).await;
            // Progress snapshot AFTER update_ancestors (critpath is
            // fresh — root priority dropped when this drv went
            // terminal) and BEFORE check_build_completion (which may
            // emit BuildCompleted; a final Progress showing 0
            // remaining is still useful right before that).
            // _with bypasses debounce: completion always carries
            // user-visible state change, and the scan is already paid.
            self.emit_progress_with(build_id, &summary);
            self.check_build_completion(build_id).await;
        }
        phase!("6-per-build-counts");
        let _ = &mut t_phase;
        let total = t_total.elapsed();
        if total >= std::time::Duration::from_secs(1) {
            debug!(elapsed = ?total, drv_hash = %drv_hash, "handle_success_completion total");
        }
    }

    /// Transition a derivation to Poisoned, persist, cascade
    /// DependencyFailed to ancestors, and propagate to interested
    /// builds. Called when the poison threshold is reached (see
    /// [`PoisonConfig::is_poisoned`]) or max_retries is hit.
    ///
    /// **Precondition:** status must be Ready, Assigned, or Running.
    /// Enforced via debug_assert! (tests catch violations) +
    /// early-return on transition failure (release builds don't
    /// cascade spuriously). Actor is single-threaded so no race
    /// between filter and call. Handles the Assigned→Running
    /// intermediate (state machine requires Running before Poisoned
    /// from those states). Ready→Poisoned is direct (I-065:
    /// `failed_builders` exhausts the fleet — never assigned). For
    /// the reassign path (worker disconnect), the caller checks the
    /// threshold BEFORE reset_to_ready — the drv is still
    /// Assigned/Running then.
    pub(super) async fn poison_and_cascade(&mut self, drv_hash: &DrvHash) {
        let Some(state) = self.dag.node_mut(drv_hash) else {
            return;
        };
        debug_assert!(
            matches!(
                state.status(),
                DerivationStatus::Ready | DerivationStatus::Assigned | DerivationStatus::Running
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
        let cascaded = self.cascade_dependency_failure(drv_hash).await;

        // Propagate failure to interested builds — union of trigger's
        // AND cascaded nodes'. A cascaded parent may belong to a
        // merged build the trigger does not; that build must also get
        // handle_derivation_failure or it hangs Active forever.
        for build_id in self.union_interested_with_cascaded(drv_hash, &cascaded) {
            self.handle_derivation_failure(build_id, drv_hash).await;
        }
    }

    // r[impl sched.build.keep-going]
    /// Prune `drv_hash` from `derivation_hashes` of every interested
    /// `keep_going=true` build. Call BEFORE `dag.remove_node` (reads
    /// `interested_builds` from the node).
    ///
    /// Both poison-removal paths (`tick_process_expired_poisons`,
    /// `handle_clear_poison`) drop the node from the DAG. A
    /// `keep_going=true` build that's still Active (other derivations
    /// running) keeps the hash in `derivation_hashes` → `total` stays
    /// at the original count but `completed+failed` (from
    /// `build_summary`, which counts DAG nodes) can never reach it →
    /// build hangs Active forever. `keep_going=false` builds are
    /// already terminal (poison → `handle_derivation_failure` →
    /// `transition_build_to_failed` in the same turn) so the prune is
    /// a no-op for them — but filtering keeps the intent explicit.
    pub(super) fn prune_interested_keep_going(&mut self, drv_hash: &DrvHash) {
        for build_id in self.get_interested_builds(drv_hash) {
            if let Some(build) = self.builds.get_mut(&build_id)
                && build.keep_going
            {
                build.derivation_hashes.remove(drv_hash);
            }
        }
    }

    // r[impl sched.admin.clear-poison]
    /// Clear poison state for a derivation (admin-initiated via
    /// `AdminService.ClearPoison`). Returns `true` if cleared.
    ///
    /// In-mem: node removed from the DAG entirely — next submit re-inserts
    /// it fresh with full proto fields and runs it through
    /// `compute_initial_states`. PG: `db.clear_poison()` sets
    /// status='created', NULLs `poisoned_at`/`retry_count`/`failed_builders`.
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
        // nodes have no interested keep_going=false builds (build already
        // terminated); keep_going=true builds are pruned from
        // derivation_hashes here so removal orphans no live accounting.
        self.prune_interested_keep_going(drv_hash);
        self.dag.remove_node(drv_hash);
        info!(drv_hash = %drv_hash, "poison cleared by admin; node removed from DAG");
        true
    }

    pub(super) async fn handle_transient_failure(
        &mut self,
        drv_hash: &DrvHash,
        executor_id: &ExecutorId,
    ) {
        // Record failure (in-mem HashSet insert + PG append,
        // best-effort) + get poison verdict in one call — same
        // helper as reassign_derivations (worker.rs) and
        // handle_reconcile_assignments (recovery.rs).
        let reached_poison = self
            .record_failure_and_check_poison(drv_hash, executor_id)
            .await;

        // r[impl sched.retry.per-worker-budget]
        // Starvation guard: clamp effective threshold to the
        // kind-matching fleet. If ALL registered workers of the
        // matching kind are in failed_builders, best_executor returns
        // None forever (hard_filter's failed_builders exclusion
        // rejects everyone). Poison now so the operator gets a signal
        // instead of a silent stuck derivation. The dispatch-time
        // backstop (`failed_builders_exhausts_fleet`) catches the same
        // condition for paths that bypass this handler
        // (reassign_derivations, recovery reconcile); doing it here
        // saves one dispatch cycle for the common explicit-failure
        // path.
        //
        // I-065: the previous version checked `self.executors.keys()`
        // (ALL kinds). With 2 builders + 2 fetchers, a drv that failed
        // on both builders never tripped it — fetchers aren't in
        // failed_builders. The kind-aware predicate is shared with the
        // dispatch-time check.
        let is_fod = self.dag.node(drv_hash).is_some_and(|s| s.is_fixed_output);
        let reached_poison =
            reached_poison || self.failed_builders_exhausts_fleet(drv_hash, is_fod);

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
                state.assigned_executor = None;
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
            // (Failed not in TERMINAL_STATUS_SQL filter) but only
            // pushes Ready-status drvs to the queue (recovery.rs:225)
            // → Failed drv sits in DAG forever, never dispatched.
            self.persist_status(drv_hash, DerivationStatus::Ready, None)
                .await;
        } else {
            self.poison_and_cascade(drv_hash).await;
        }
    }

    /// InfrastructureFailure: worker-local problem, not the build's fault.
    /// Reset to Ready and retry WITHOUT inserting into `failed_builders`.
    ///
    /// InfrastructureFailure is the worker saying "I can't right now."
    /// If it's still broken, it'll fail again. If it's recovered
    /// (circuit closed), it'll succeed. P0211's `store_degraded`
    /// heartbeat → `has_capacity()` false already excludes persistently-
    /// broken workers from assignment upstream, so per-worker capping
    /// isn't needed.
    ///
    /// BUT: `infra_retry_count` IS incremented and checked against
    /// `max_infra_retries` (a separate, higher bound than `max_retries`).
    /// Without this, a scheduler-side bug that misclassifies a
    /// deterministic failure as infra (e.g., empty CA input path →
    /// worker's MetadataFetch error → InfrastructureFailure) hot-loops
    /// forever at ~100ms intervals. Observed: 9748 re-dispatches in one
    /// session before the CA-path-propagation fix. The bound converts a
    /// livelock into a visible poison.
    ///
    /// Separate counter from `retry_count` so infra failures don't eat
    /// into the transient-failure budget: 3× infra + 1× transient
    /// should NOT poison on the transient (the transient is the first
    /// REAL failure; the 3 infra were worker-side noise).
    pub(super) async fn handle_infrastructure_failure(
        &mut self,
        drv_hash: &DrvHash,
        executor_id: &ExecutorId,
        error_msg: &str,
    ) {
        // I-199: promote size_class_floor on InfrastructureFailure too.
        // The dominant infra failure mode is `CgroupOom` (I-196 OOM
        // watcher) — exactly when promotion is the right answer. Other
        // infra failures (FUSE EIO, PutPath race) aren't size-related,
        // but over-promoting them is bounded by `max_infra_retries`
        // and the cost is one larger pod, not correctness. Without
        // this, the OOM-watcher path retries on the same class until
        // the infra cap poisons — defeating the watcher's purpose.
        let failed_class = self
            .executors
            .get(executor_id)
            .and_then(|e| e.size_class.clone());
        self.promote_size_class_floor(drv_hash, failed_class.as_deref())
            .await;

        let Some(state) = self.dag.node_mut(drv_hash) else {
            return;
        };

        // I-127: "concurrent PutPath" is NEVER counted toward the
        // infra cap. It means another builder is uploading the SAME
        // output — the drv almost certainly succeeded elsewhere; this
        // worker just lost the upload race. I-125b makes the builder
        // wait-then-adopt on this error, so reaching here means the
        // wait timed out (other uploader stuck/slow). Re-dispatch
        // freely: either the path appears (next attempt → AlreadyValid)
        // or the lock clears and the upload retries. Poisoning on this
        // is exactly the wrong outcome — observed under shallow-1024x
        // when a leaked lock (I-125a) made 4 builders hit this in a
        // row → poison at 99.7%.
        //
        // Substring match mirrors `is_concurrent_put_path` in
        // rio-builder/src/upload.rs (the store emits this exact
        // phrase in its Aborted status).
        let exempt_from_cap = error_msg.contains("concurrent PutPath");

        // I-127 time-window reset: if the last counted infra failure
        // was longer ago than `infra_retry_window_secs`, treat this as
        // a fresh incident — reset the counter before the cap check.
        // Sparse failures over a long build (4 fails over an hour)
        // are independent; only a tight burst (4 fails in 2min)
        // suggests a misclassified permanent error.
        if let Some(last) = state.last_infra_failure_at
            && last.elapsed().as_secs_f64() > self.retry_policy.infra_retry_window_secs
        {
            debug!(
                drv_hash = %drv_hash,
                prev_count = state.infra_retry_count,
                window_secs = self.retry_policy.infra_retry_window_secs,
                "infra-retry window elapsed — resetting counter"
            );
            state.infra_retry_count = 0;
        }

        // Bound check BEFORE reset_to_ready: if we've already exhausted
        // the infra budget, poison instead. The derivation is likely
        // hitting a deterministic failure the worker misclassifies as
        // infra — more retries won't help, and the operator needs the
        // poison signal to investigate. ensure_running() first:
        // poison_and_cascade expects Running (not Assigned).
        // Exempt errors skip the cap entirely (see above).
        if !exempt_from_cap && state.infra_retry_count >= self.retry_policy.max_infra_retries {
            state.ensure_running();
            warn!(
                drv_hash = %drv_hash,
                executor_id = %executor_id,
                infra_retry_count = state.infra_retry_count,
                max = self.retry_policy.max_infra_retries,
                "infrastructure failure: max_infra_retries exhausted, poisoning"
            );
            self.poison_and_cascade(drv_hash).await;
            return;
        }

        if let Err(e) = state.reset_to_ready() {
            warn!(drv_hash = %drv_hash, error = %e,
                  "infrastructure failure: reset_to_ready failed, skipping");
            return;
        }
        // NO insert into failed_builders (infra is worker-local, not
        // build-local). NO retry_count++ (infra doesn't eat transient
        // budget). NO backoff (re-dispatch immediately; P0211's
        // store_degraded check excludes still-broken workers). But DO
        // count against the SEPARATE infra bound — livelock prevention.
        // Exempt errors (concurrent PutPath) don't increment — they
        // can't indicate a misclassified permanent failure, so the
        // hot-loop guard isn't needed for them.
        if !exempt_from_cap {
            state.infra_retry_count += 1;
            state.last_infra_failure_at = Some(Instant::now());
        }
        info!(
            drv_hash = %drv_hash,
            executor_id = %executor_id,
            infra_retry_count = state.infra_retry_count,
            exempt_from_cap,
            "infrastructure failure — retry without poison count"
        );
        self.persist_status(drv_hash, DerivationStatus::Ready, None)
            .await;
        self.push_ready(drv_hash.clone());

        // Dashboard: Running → Ready state change. emit_progress so
        // the dashboard sees the retry requeue.
        for build_id in self.get_interested_builds(drv_hash) {
            self.emit_progress(build_id);
        }
    }

    pub(super) async fn handle_permanent_failure(
        &mut self,
        drv_hash: &DrvHash,
        error_msg: &str,
        _executor_id: &ExecutorId,
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
        let cascaded = self.cascade_dependency_failure(drv_hash).await;

        // Propagate failure to interested builds — union of trigger's
        // AND cascaded nodes' (a cascaded parent may belong to a
        // merged build the trigger does not). Log-flush + failure
        // event use the trigger's set (those builds saw THIS drv
        // fail); handle_derivation_failure uses the union (those
        // builds saw SOME drv fail, possibly a cascaded one).
        let trigger_builds = self.get_interested_builds(drv_hash);

        // Flush logs for failed builds too — the failure's log is often the
        // most useful log (compile errors, test output). Do this BEFORE
        // handle_derivation_failure below, which may transition builds to
        // terminal and schedule cleanup.
        self.trigger_log_flush(drv_hash, trigger_builds.clone());

        for build_id in &trigger_builds {
            // Emit failure event
            self.emit_build_event(
                *build_id,
                rio_proto::types::build_event::Event::Derivation(rio_proto::dag::DerivationEvent {
                    derivation_path: self.drv_path_or_hash_fallback(drv_hash),
                    status: Some(rio_proto::dag::derivation_event::Status::Failed(
                        rio_proto::dag::DerivationFailed {
                            error_message: error_msg.to_string(),
                            status: rio_proto::build_types::BuildResultStatus::PermanentFailure
                                .into(),
                        },
                    )),
                }),
            );
        }

        for build_id in self.union_interested_with_cascaded(drv_hash, &cascaded) {
            self.handle_derivation_failure(build_id, drv_hash).await;
        }
    }

    /// Worker-side timeout (`BuildResultStatus::TimedOut`): promote
    /// `size_class_floor` and reset to Ready for re-dispatch on a
    /// larger class — bounded by `max_timeout_retries`, after which
    /// terminal `Cancelled` (retriable on EXPLICIT resubmit only).
    ///
    /// I-200: with per-class `activeDeadlineSeconds = cutoff×5`
    /// (`r[ctrl.ephemeral.per-class-deadline]`) the next dispatch
    /// lands on a larger class with a proportionally longer deadline,
    /// so "same inputs → same timeout → storm" no longer holds for
    /// the first N retries. The cap (default 4: tiny→xlarge) ensures
    /// a genuinely-infinite build still goes terminal.
    ///
    /// Separate `timeout_retry_count` (NOT `retry_count` /
    /// `infra_retry_count`): timeouts neither consume the transient
    /// budget nor get the infra time-window reset (sparse timeouts
    /// over hours are still the same hung build).
    ///
    /// Terminal path transitions to `Cancelled` (not `Poisoned`):
    /// `Cancelled` is in `is_retriable_on_resubmit`, `Poisoned` has a
    /// 24h TTL that's way too aggressive for "ran out of time". Same
    /// cascade/events/build-fail side-effects as
    /// `handle_permanent_failure` — the build still fails THIS time,
    /// just without the 24h resubmit lockout.
    // r[impl sched.timeout.promote-on-exceed]
    pub(super) async fn handle_timeout_failure(
        &mut self,
        drv_hash: &DrvHash,
        error_msg: &str,
        executor_id: &ExecutorId,
    ) {
        // Promote BEFORE the cap check / state transition: even the
        // terminal-path attempt should record that this class was
        // inadequate, so an explicit resubmit starts on the next-
        // larger class instead of replaying the timeout. Same shape
        // as I-199's handle_infrastructure_failure prologue.
        let failed_class = self
            .executors
            .get(executor_id)
            .and_then(|e| e.size_class.clone());
        self.promote_size_class_floor(drv_hash, failed_class.as_deref())
            .await;

        let Some(state) = self.dag.node_mut(drv_hash) else {
            return;
        };

        // Cap check BEFORE reset_to_ready. At the cap, fall through
        // to terminal Cancelled — a build that timed out on every
        // class (or on a class where promote_size_class_floor
        // returned None: already at largest) is genuinely stuck.
        if state.timeout_retry_count < self.retry_policy.max_timeout_retries {
            state.ensure_running();
            if let Err(e) = state.reset_to_ready() {
                warn!(drv_hash = %drv_hash, error = %e,
                      "timeout failure: reset_to_ready failed, skipping");
                return;
            }
            // NO insert into failed_builders (timeout is not a per-
            // worker problem — the SAME worker on a larger class
            // would succeed). NO retry_count++ (separate counter).
            // NO backoff (next class has a longer deadline; that IS
            // the backoff).
            state.timeout_retry_count += 1;
            info!(
                drv_hash = %drv_hash,
                executor_id = %executor_id,
                timeout_retry_count = state.timeout_retry_count,
                max = self.retry_policy.max_timeout_retries,
                failed_class = ?failed_class,
                "timeout — promoted size_class_floor, retrying on larger class"
            );
            self.persist_status(drv_hash, DerivationStatus::Ready, None)
                .await;
            self.push_ready(drv_hash.clone());
            for build_id in self.get_interested_builds(drv_hash) {
                self.emit_progress(build_id);
            }
            return;
        }

        // ── Terminal path (cap exhausted) ───────────────────────────
        warn!(
            drv_hash = %drv_hash,
            executor_id = %executor_id,
            timeout_retry_count = state.timeout_retry_count,
            max = self.retry_policy.max_timeout_retries,
            "timeout: max_timeout_retries exhausted, transitioning to Cancelled"
        );
        state.ensure_running();
        if let Err(e) = state.transition(DerivationStatus::Cancelled) {
            warn!(drv_hash = %drv_hash, error = %e, current = ?state.status(),
                  "handle_timeout_failure: ->Cancelled transition rejected, skipping");
            return;
        }

        self.persist_status(drv_hash, DerivationStatus::Cancelled, None)
            .await;
        self.unpin_best_effort(drv_hash).await;

        // Cascade: parents of a timed-out derivation can't complete THIS
        // time (resubmit-reset handles the next attempt).
        let cascaded = self.cascade_dependency_failure(drv_hash).await;

        let trigger_builds = self.get_interested_builds(drv_hash);
        self.trigger_log_flush(drv_hash, trigger_builds.clone());

        for build_id in &trigger_builds {
            self.emit_build_event(
                *build_id,
                rio_proto::types::build_event::Event::Derivation(rio_proto::dag::DerivationEvent {
                    derivation_path: self.drv_path_or_hash_fallback(drv_hash),
                    status: Some(rio_proto::dag::derivation_event::Status::Failed(
                        rio_proto::dag::DerivationFailed {
                            error_message: error_msg.to_string(),
                            status: rio_proto::build_types::BuildResultStatus::TimedOut.into(),
                        },
                    )),
                }),
            );
        }

        for build_id in self.union_interested_with_cascaded(drv_hash, &cascaded) {
            self.handle_derivation_failure(build_id, drv_hash).await;
        }
    }

    /// Transitively walk parents of a poisoned derivation and transition all
    /// Queued/Ready/Created ancestors to DependencyFailed.
    ///
    /// Returns the set of derivations actually transitioned. Callers
    /// union the `interested_builds` of each transitioned node with the
    /// trigger's — a cascaded node may belong to a merged build that
    /// the trigger does NOT (shared dep, different upstream). Without
    /// the union, that merged build hangs Active forever.
    ///
    /// Without the cascade itself, keepGoing builds with a poisoned
    /// leaf hang forever: parents stay Queued, so completed+failed
    /// never reaches total.
    //
    // Same BFS-frontier shape as speculative_cascade_reachable but
    // async (per-step persist_status().await) + walks get_parents()
    // unconditionally rather than eligibility-gated. Not migrated —
    // see P0405-T3 route-(a). If a 4th async walker appears, consider
    // route-(b): collect-then-batch-persist (safe because recovery
    // re-cascades from the original poisoned leaf on partial persist).
    pub(super) async fn cascade_dependency_failure(
        &mut self,
        poisoned_hash: &DrvHash,
    ) -> HashSet<DrvHash> {
        let mut to_visit: Vec<DrvHash> = self.dag.get_parents(poisoned_hash);
        let mut visited: HashSet<DrvHash> = HashSet::new();
        let mut transitioned: HashSet<DrvHash> = HashSet::new();

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
            transitioned.insert(parent_hash);
        }
        transitioned
    }

    /// Collect the union of `interested_builds` over the trigger
    /// derivation AND all cascaded derivations. A cascaded node may
    /// belong to a merged build that the trigger does not — without
    /// this union, `handle_derivation_failure` is never called for
    /// that build and it hangs Active forever.
    fn union_interested_with_cascaded(
        &self,
        trigger: &DrvHash,
        cascaded: &HashSet<DrvHash>,
    ) -> HashSet<Uuid> {
        let mut builds: HashSet<Uuid> = self.get_interested_builds(trigger).into_iter().collect();
        for h in cascaded {
            if let Some(s) = self.dag.node(h) {
                builds.extend(s.interested_builds.iter().copied());
            }
        }
        builds
    }

    pub(super) async fn handle_derivation_failure(&mut self, build_id: Uuid, drv_hash: &DrvHash) {
        // Sync counts from DAG ground truth. The cascade may have transitioned
        // additional parents to DependencyFailed; those must be counted here.
        self.update_build_counts(build_id).await;

        let Some(build) = self.builds.get_mut(&build_id) else {
            return;
        };

        if !build.keep_going {
            // r[impl sched.build.keep-going]
            // Fail the entire build immediately. Cancel remaining
            // derivations first — without this, sole-interest Queued/
            // Ready/Assigned derivations for this build linger:
            // Assigned ones keep burning worker CPU, Queued/Ready
            // ones occupy the ready queue. cancel_build_derivations
            // sends CancelSignal + transitions DependencyFailed/
            // Cancelled + removes build interest.
            build.error_summary = Some(format!("derivation {drv_hash} failed"));
            build.failed_derivation = Some(drv_hash.to_string());
            self.cancel_build_derivations(
                build_id,
                &format!("build {build_id} failed fast (keep_going=false)"),
            )
            .await;
            if let Err(e) = self.transition_build_to_failed(build_id).await {
                error!(build_id = %build_id, error = %e, "failed to persist build-failed transition");
            }
        } else {
            // keepGoing: check if all derivations are resolved
            self.check_build_completion(build_id).await;
        }
    }
}
