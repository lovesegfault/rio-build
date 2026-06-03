//! Ready-set store short-circuit, plus the shared `WorkAssignment`
//! payload constructor the pull path uses.
//!
//! The stream-era placement/assign pass (`dispatch_ready` and the
//! 4-phase assign path) was deleted with the placement layer; work
//! delivery is pull-only (`actor/pull.rs`). The detached substitute
//! closure walk this file used to host was deleted with the
//! substitution-replacement cutover (store replicas execute
//! materialization jobs instead). What remains is
//! dispatch-mode-independent: completing Ready derivations whose
//! outputs already exist (or routing them to materialization jobs),
//! and `build_assignment_proto`/`emit_assignment_started` (shared
//! with the pull mint).

use std::collections::{HashMap, HashSet};

use uuid::Uuid;

use tracing::{debug, error, info, warn};

use rio_proto::types::FindMissingPathsRequest;

use crate::state::{
    BuildStateExt, DerivationStatus, DrvHash, ExecutorId, effective_wanted, verifiable_wanted_paths,
};

use super::DagActor;

impl DagActor {
    // -----------------------------------------------------------------------
    // Ready-set store short-circuit
    // -----------------------------------------------------------------------

    /// Complete or substitute Ready derivations whose outputs already
    /// exist (locally or upstream-substitutable) — the I-067/I-070
    /// store short-circuit, the dispatch-mode-independent half of the
    /// former dispatch pass. There is no placement decision left (work
    /// delivery is pull-only), but a Ready derivation whose outputs
    /// appeared in the store since merge time must still be completed
    /// or demoted to substitution instead of having a pod spawned for
    /// it. Called where the old dispatch pass ran inline (after merge,
    /// after each completion cascade, on leader acquire);
    /// `probe_generation` (advanced once per Tick) bounds re-probing.
    pub(super) async fn sweep_ready_cached(&mut self) {
        // Same standby/recovery gates the old dispatch pass carried: a
        // standby or mid-recovery actor must not act on its DAG.
        if !self.leader.is_leader() {
            return;
        }
        if !self.leader.recovery_complete() {
            return;
        }
        let _ = self.batch_probe_cached_ready().await;
    }
    // -----------------------------------------------------------------------
    // Dispatch
    // -----------------------------------------------------------------------

    /// I-067: best-effort store check for a Ready IA derivation's
    /// outputs (was FOD-only; generalised per the >4096 cap-gap).
    ///
    /// I-070: batched form — collect every unprobed Ready node's
    /// expected outputs, ONE `FindMissingPaths`, then
    /// [`Self::complete_ready_from_store`] each whose outputs are all
    /// present. Fail-open: store unreachable → no-op (per-drv
    /// fallback in the dispatch loop covers it next pass).
    ///
    /// Iterates the full DAG. Full-DAG scan is O(nodes) but the actor
    /// is single-threaded so there's no contention; for a 1085-node
    /// merge the scan is sub-ms vs. ~25s of sequential RPCs it
    /// replaces.
    ///
    /// Returns the set of hashes the drain loop must skip
    /// `ready_check_or_spawn` for (I-163). On success this is the
    /// batch-probed head (completed here or definitively found-missing
    /// one RPC ago) plus the truncated tail (deferred to next pass's
    /// batch). On RPC error/timeout this is the tail only — the
    /// stamped head is protected via `probed_generation`, so neither
    /// hits the per-drv fallback.
    // r[impl sched.dispatch.fod-substitute+3]
    async fn batch_probe_cached_ready(&mut self) -> HashSet<DrvHash> {
        let Some(store) = &self.store_client else {
            return HashSet::new();
        };
        let probe_gen = self.probe_generation;
        // Candidate set: (drv_hash, output_paths). Collected up-front
        // so the FindMissingPaths borrow doesn't hold &self.dag across
        // the .await (and so the completion loop can take &mut self).
        // Floating-CA (`expected_output_paths == [""]`) is excluded by
        // the `!is_empty()` + path-known check; the realisations lane
        // at merge-time handles those.
        let mut candidates: Vec<(DrvHash, Vec<String>)> = self
            .dag
            .iter_nodes()
            .filter(|(_, s)| {
                s.status() == DerivationStatus::Ready
                    && s.probed_generation < probe_gen
                    && s.output_paths_probeable()
            })
            .map(|(h, s)| (DrvHash::from(h), s.expected_output_paths.clone()))
            .collect();
        if candidates.is_empty() {
            return HashSet::new();
        }
        // Belt under the store-side 4096 cap. The truncated tail is
        // inserted into `checked` (so the drain loop skips the per-drv
        // `ready_check_or_spawn` fallback) but NOT stamped with
        // `probed_generation` — the next inline `dispatch_ready` (same
        // generation) batch-probes that window. Letting the tail fall
        // through to the per-drv path would be O(N) sequential 30s-
        // timeout RPCs in the actor (24h+ stall with a wide layer and
        // an unreachable store; I-139/I-140 invariant).
        let mut checked = HashSet::with_capacity(candidates.len());
        if candidates.len() > super::DISPATCH_PROBE_BATCH_CAP {
            for (h, _) in &candidates[super::DISPATCH_PROBE_BATCH_CAP..] {
                checked.insert(h.clone());
            }
            candidates.truncate(super::DISPATCH_PROBE_BATCH_CAP);
        }
        for (h, _) in &candidates {
            if let Some(s) = self.dag.node_mut(h) {
                s.probed_generation = probe_gen;
            }
        }

        // Tenant context for the upstream-substitution probe: any
        // tenant that wants any candidate (substitution is content-
        // addressed; whose upstream we use is irrelevant to the
        // result). Without this the store sees tenant_id=None and
        // substitutable_paths stays empty — the pre-fix behaviour
        // that dispatched FODs already in cache.nixos.org.
        let probe = self.probe_service_meta(candidates.iter().map(|(h, _)| h));
        let probe_meta: Vec<(&'static str, &str)> =
            probe.iter().map(|(k, v)| (*k, v.as_str())).collect();

        let store_paths: Vec<String> = candidates
            .iter()
            .flat_map(|(_, p)| p.iter().cloned())
            .collect();
        // Deliberately NOT gated on `cache_breaker`: dispatch-time
        // probe failure degrades to cache-miss (per-drv fallback
        // retries), not StoreUnavailable. The breaker is for merge-time
        // admission only — here the call IS the work.
        let mut req = tonic::Request::new(FindMissingPathsRequest { store_paths });
        Self::inject_probe_meta(req.metadata_mut(), &probe_meta);
        let resp =
            match tokio::time::timeout(self.grpc_timeout, store.clone().find_missing_paths(req))
                .await
            {
                Ok(Ok(r)) => r.into_inner(),
                Ok(Err(e)) => {
                    debug!(
                        candidates = candidates.len(),
                        error = %e,
                        "batched Ready store-check FindMissingPaths failed; \
                         dispatching fail-open (next pass batch-retries)"
                    );
                    // Tail already in `checked`; head protected via the
                    // probed_generation stamp at `ready_check_or_spawn`.
                    return checked;
                }
                Err(_) => {
                    debug!(
                        candidates = candidates.len(),
                        timeout = ?self.grpc_timeout,
                        "batched Ready store-check timed out; \
                         dispatching fail-open (next pass batch-retries)"
                    );
                    return checked;
                }
            };

        // Partition: locally-present (not in missing_paths) → complete
        // inline; missing-but-obtainable (substitutable/indeterminate)
        // → route to a materialization job; truly-missing → leave Ready
        // (dispatches normally from source).
        let missing: HashSet<String> = resp.missing_paths.into_iter().collect();
        let substitutable: HashSet<String> = resp.substitutable_paths.into_iter().collect();
        let indeterminate: HashSet<String> = resp.indeterminate_paths.into_iter().collect();
        // I-139: collect-then-batch. The locally-present branch awaited
        // `complete_ready_from_store` per item (≥3 sequential PG RTTs
        // each); on warm-restart of a large closure ~all 2048 candidates
        // hit it → 12-30s actor stall → heartbeats missed → live workers
        // reaped.
        let mut locally_present = Vec::new();
        // Nodes routed to a materialization job (creation itself is
        // owned by `create_materialization_job` — leader-gated,
        // fenced, and dedup'd there).
        let mut to_create_job: Vec<DrvHash> = Vec::new();
        for (drv_hash, paths) in candidates {
            checked.insert(drv_hash.clone());
            // r[impl sched.merge.wanted-outputs+3]
            // Demand-driven completeness: only the WANTED outputs must
            // be present (→ complete inline) or present-or-
            // substitutable (→ detached fetch). A missing output
            // nothing consumes must not force a from-source dispatch.
            // The wanted slice is the LIVE effective wanted set
            // (`effective_wanted` over live interested builds'
            // contributions; a terminal build's wants stop counting),
            // degrading to ALL DECLARED outputs when no live union
            // resolves (the conservative-absent branch, T-D2.3 — the
            // stored-union fallback is gone; divergence is
            // widening-only). `verifiable_wanted_paths` returns None
            // for a wanted set that resolves to no verifiable path;
            // degrade to all of `paths` then (and for a node that
            // vanished from the DAG mid-probe). The probe set stays
            // ALL expected paths (opportunistic completeness — fetch
            // the unwanted output too if the upstream has it).
            let wanted: Vec<String> = self
                .dag
                .node(&drv_hash)
                .and_then(|s| {
                    let eff = effective_wanted(s, &self.builds);
                    verifiable_wanted_paths(
                        &s.output_names,
                        &s.expected_output_paths,
                        eff.as_deref().unwrap_or(&[]),
                    )
                    .map(|w| w.into_iter().map(str::to_owned).collect::<Vec<String>>())
                })
                .unwrap_or_else(|| paths.clone());
            if wanted.iter().all(|p| !missing.contains(p)) {
                locally_present.push(drv_hash);
            } else if wanted.iter().all(|p| {
                !missing.contains(p) || substitutable.contains(p) || indeterminate.contains(p)
            }) {
                // r[impl sched.materialize.job+2]
                // r[impl sched.merge.substitute-probe-indeterminate+2]
                // Route to a materialization job. The job row is the
                // in-flight marker; the node stays Ready (claimable by
                // a store replica). Indeterminate (probe got
                // 429/5xx/deadline) is treated optimistically — same as
                // merge.rs; the job's own routing settles a genuine
                // miss.
                to_create_job.push(drv_hash);
            }
            // else: a wanted output is confirmed missing upstream and
            // not substitutable — leave Ready (dispatches from source).
        }
        self.complete_ready_from_store_batch(&locally_present).await;
        // The probe-partition creation site — the standalone fenced
        // helper, no enclosing transaction (design §2.1 row 3).
        for drv_hash in &to_create_job {
            self.create_materialization_job(
                drv_hash,
                crate::state::JobOrigin::CacheOpportunity,
                None,
                None,
            )
            .await;
        }
        checked
    }

    /// Service-token metadata for dispatch-time store probes
    /// (`FindMissingPaths`): `(service token, probe tenant id)` when
    /// both `service_signer` and a tenant are resolvable from the
    /// candidates' interested builds; empty (no-auth, dev mode /
    /// single-tenant) otherwise. Tenant context matters because the
    /// store's upstream-substitution probe resolves
    /// `tenant_upstreams` from it — without it `substitutable_paths`
    /// stays empty. One-shot mint: the probe is a single bounded gRPC
    /// call (the re-mintable walk auth died with the walk).
    #[allow(clippy::extra_unused_lifetimes)] // 'a only in impl-Trait arg
    pub(super) fn probe_service_meta<'a>(
        &self,
        drv_hashes: impl Iterator<Item = &'a DrvHash>,
    ) -> Vec<(&'static str, String)> {
        let tid = drv_hashes
            .filter_map(|h| self.dag.node(h))
            .flat_map(|s| s.interested_builds.iter())
            .filter_map(|bid| self.builds.get(bid))
            .find_map(|b| b.tenant_id);
        match (&self.service_signer, tid) {
            (Some(signer), Some(tenant_id)) => {
                let claims = rio_auth::hmac::ServiceClaims {
                    caller: "rio-scheduler".to_string(),
                    expiry_unix: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0)
                        + super::PROBE_TOKEN_EXPIRY.as_secs(),
                    // Probe tokens are not replica-bound (T-5.1: only the
                    // store's materialization client mints Some).
                    instance: None,
                };
                vec![
                    (rio_proto::SERVICE_TOKEN_HEADER, signer.sign(&claims)),
                    (rio_proto::PROBE_TENANT_ID_HEADER, tenant_id.to_string()),
                ]
            }
            _ => Vec::new(),
        }
    }

    fn inject_probe_meta(md: &mut tonic::metadata::MetadataMap, meta: &[(&'static str, &str)]) {
        for (k, v) in meta {
            if let Ok(mv) = tonic::metadata::MetadataValue::try_from(*v) {
                md.insert(*k, mv);
            }
        }
    }

    // r[impl sched.merge.substitute-topdown+13]
    /// Topdown-pruned fail-fast: the node's dep subgraph was dropped
    /// from its submission, so a from-source build dispatch cannot
    /// succeed (the worker ENOENTs on inputDrvs that were never
    /// merged). Fail every interested build with a resubmit-directing
    /// error and park the node instead of leaving it dispatchable.
    ///
    /// `cause` is the user-facing reason spliced into the build's
    /// error summary ("topdown-pruned root <hash>: <cause>; resubmit
    /// to re-probe or full-merge").
    ///
    /// Sole caller: the materialization consumption routing's arm 3
    /// (route_unobtainable in materialize.rs) — an unobtainable outcome
    /// on a pruned-origin job whose live-wanted outputs are confirmed
    /// missing (the four-conjunct settlement; r[sched.materialize.settlement]).
    /// The walk-era callers (the SubstituteComplete Broken arm, the
    /// reap-time survivor settlement, the dispatch-probe fail-fast
    /// cell) died with the walk.
    pub(super) async fn fail_fast_pruned_root(&mut self, drv_hash: &DrvHash, cause: &str) {
        // A prior iteration of the same dispatch pass may already have
        // settled this node: `batch_probe_cached_ready` collects the
        // whole `to_fail_fast` layer up front, and the first node's
        // `cancel_build_derivations` transitions every other
        // sole-interest not-yet-dispatched node of that build —
        // including later entries of the list — to DependencyFailed,
        // persists it, and strips the build's interest. Re-running the
        // park here would resurrect that terminal verdict
        // (DependencyFailed→Queued is a valid reprobe edge) and leave a
        // non-terminal, zero-interest, dep-less orphan behind in memory
        // and PG that no reap collects and every recovery reloads.
        // Equally nothing to do when the node vanished or no build is
        // interested any more — the verdicts are already settled. (The
        // walk-era SubstituteComplete{ok=false} caller never tripped
        // this either: it only reached the helper for a node it had
        // just observed in the walk's in-flight status with live
        // interest.)
        let actionable = self
            .dag
            .node(drv_hash)
            .is_some_and(|s| !s.status().is_terminal() && !s.interested_builds.is_empty());
        if !actionable {
            debug!(%drv_hash,
                   "topdown fail-fast: node already terminal/orphaned (settled by an \
                    earlier iteration of this pass); skipping");
            return;
        }
        warn!(%drv_hash, cause,
              "topdown-pruned root cannot complete via substitution; \
               deps were dropped from DAG — failing build (resubmit \
               will re-probe or full-merge)");
        metrics::counter!("rio_scheduler_topdown_substitute_fail_total").increment(1);
        let msg =
            format!("topdown-pruned root {drv_hash}: {cause}; resubmit to re-probe or full-merge");
        // Queued (not Ready): zero DAG deps → vacuous Ready would
        // re-dispatch on the next Tick. cancel_build_derivations
        // strips interest below; with zero remaining interest the
        // node is reaped on the next sweep.
        // The fail-fast's one-shot is the consumed JOB: the arm-3
        // settlement resolves the pruned-origin job row terminally
        // (resolved_unobtainable) before calling this helper, so a
        // failover cannot re-arm the fail-fast from stale state — a
        // resubmitted genuinely-pruned root re-prunes and gets a fresh
        // pruned-origin job; a full merge re-declares the closure and
        // creates none.
        let parked = if let Some(s) = self.dag.node_mut(drv_hash) {
            if let Err(e) = s.transition(DerivationStatus::Queued) {
                warn!(%drv_hash, %e, "topdown fail-fast: transition to Queued rejected");
            }
            true
        } else {
            false
        };
        // Persist only for a node the block above actually parked —
        // never for one the early return skipped or that vanished
        // between the check and the mutation.
        if parked {
            self.persist_status(drv_hash, DerivationStatus::Queued, None)
                .await;
        }
        for build_id in self.get_interested_builds(drv_hash) {
            if let Some(build) = self.builds.get_mut(&build_id) {
                // The message itself says "resubmit to re-probe or
                // full-merge" — TransientFailure is the wire signal for
                // "might work if retried". One struct, first wins.
                build.note_first_failure(crate::state::FirstFailure {
                    summary: msg.clone(),
                    failed_drv: Some(drv_hash.to_string()),
                    status: Some(rio_proto::types::BuildResultStatus::TransientFailure),
                });
            }
            self.cancel_build_derivations(
                build_id,
                &format!("build {build_id}: topdown-pruned root: {cause}"),
            )
            .await;
            if let Err(e) = self.transition_build_to_failed(build_id).await {
                error!(%build_id, error = %e,
                       "failed to persist build-failed after topdown fail-fast");
            }
        }
    }

    // r[impl sched.poison.clear-survivor-reevaluation+2]
    // r[impl sched.merge.substitute-topdown+13]
    /// Per-survivor verdicts after children were removed from the DAG.
    /// Shared by every leader-side removal path that leaves surviving
    /// parents behind: the terminal-build reap
    /// (`handle_cleanup_terminal_build`), admin `ClearPoison`
    /// (`handle_clear_poison`) and the poison-TTL sweep
    /// (`tick_process_expired_poisons`).
    ///
    /// One stranded shape is closed (the walk-era settlement arm — the
    /// reap-time fail-fast of a marked spent survivor — died with the
    /// walk consumption machinery; survivors carrying an unresolved
    /// materialization job are armed by the job itself, and marked
    /// survivors without one are re-classified by the next dispatch
    /// sweep / settled at consumption):
    ///
    ///  - A `Queued` survivor whose last un-produced children were
    ///    removed: it is now vacuously all-deps-completed, but no
    ///    completion event will ever promote it. Promote it to Ready,
    ///    push it and persist, so the next dispatch pass picks it up.
    ///    This is what un-blocks a parent the recovery condemnation
    ///    spared on co-ownership grounds
    ///    (`sched.recovery.failed-dep-cascade+2`'s MUST NOT clause): it
    ///    recovered Queued above a non-co-owned within-TTL poisoned
    ///    child, and the poison-clear removal of that child is its only
    ///    wake-up edge — without this arm it would sit Queued forever
    ///    and its build would hang (the L3 strand).
    ///
    /// Skipped: vanished nodes, terminal nodes (already settled), and
    /// nodes with no interested builds (no build left to hang).
    ///
    /// Leader-only: every caller is leader-gated — the reap hook runs
    /// inside its `is_leader()` block, `handle_clear_poison`'s only
    /// production caller is the leader-guarded admin RPC, and
    /// `handle_tick` no-ops on standby
    /// (`r[sched.lease.standby-tick-noop]`).
    pub(super) async fn reevaluate_removal_survivors(&mut self, survivors: &[DrvHash]) {
        for parent in survivors {
            let Some(node) = self.dag.node(parent) else {
                continue;
            };
            if node.status().is_terminal() || node.interested_builds.is_empty() {
                continue;
            }
            let status = node.status();
            // r[impl sched.materialize.job+2]
            // Substitution-replacement Phase B (T-4.3): a survivor
            // carrying an unresolved materialization job needs NOTHING
            // from this loop — the job is already the armed action
            // (design §2.1: "survivors with an unresolved job need
            // nothing"), and both arms below would race it: the
            // settlement spends the verification one-shot and spawns a
            // walk (a flag-on walk spawn for fresh work — the
            // criterion-3 violation) or fail-fasts the surviving builds
            // outright; the promotion arm would push a Ready node whose
            // builder pulls the kinded admission refuses anyway. The
            // job's own §2.4 consumption settlement is the survivor's
            // settlement authority.
            if self.has_unresolved_job(parent.as_str()) {
                continue;
            }
            if status == DerivationStatus::Queued
                && self.dag.all_deps_completed(parent)
                && let Some(s) = self.dag.node_mut(parent)
                && s.transition(DerivationStatus::Ready).is_ok()
            {
                // Promotion arm. A promoted marked-Broken survivor is
                // settled by the next dispatch sweep's settlement-aware
                // partition; an unmarked one dispatches normally.
                self.persist_status(parent, DerivationStatus::Ready, None)
                    .await;
            }
        }
    }

    // r[impl gw.activity.subst-progress]
    /// Relay byte-progress from a store replica's materialization
    /// execution to every interested build via
    /// [`Event::SubstituteProgress`] (BC-4: the
    /// `ReportMaterializationProgress` RPC posts the
    /// [`ActorCommand::SubstituteProgress`] this handles — the walk
    /// producer this relay was built for is deleted). Display-only
    /// (routed through the log broadcast ring, not persisted, reuses
    /// last seq); the gateway translates to `actCopyPath` +
    /// `resProgress`. Non-leader emits are fine — this is read-only
    /// (no DAG/PG mutation).
    pub(super) fn handle_substitute_progress(
        &mut self,
        drv_hash: &DrvHash,
        bytes_done: u64,
        bytes_expected: u64,
        upstream_uri: String,
    ) {
        let Some(state) = self.dag.node(drv_hash) else {
            return;
        };
        let drv_path = state.drv_path().to_string();
        let interested = state.interested_builds.clone();
        let event = rio_proto::types::build_event::Event::SubstituteProgress(
            rio_proto::types::SubstituteProgress {
                derivation_path: drv_path,
                bytes_done,
                bytes_expected,
                upstream_uri,
            },
        );
        for build_id in interested {
            self.events.emit(build_id, event.clone());
        }
    }

    /// Batched [`complete_ready_from_store`](Self::complete_ready_from_store):
    /// transition + `output_paths` set in-mem first (no await), then one
    /// `persist_status_batch(Completed)`, one `upsert_path_tenants_for_
    /// batch`, one batched newly-ready promote, then per-BUILD (not
    /// per-drv) summary/counts/completion-check. I-139: the per-item
    /// variant in `batch_probe_cached_ready`'s locally-present branch
    /// was 3 sequential PG awaits × ≤2048 candidates → 12-30s actor
    /// stall on warm-restart of a large closure.
    // pub(super): also called by the materialization consumption handler
    // (the Success/moot-covered arms complete through this same chokepoint).
    pub(super) async fn complete_ready_from_store_batch(&mut self, hashes: &[DrvHash]) {
        if hashes.is_empty() {
            return;
        }
        struct Done {
            hash: DrvHash,
            drv_path: String,
            output_paths: Vec<String>,
            interested: HashSet<Uuid>,
        }
        let mut ok: Vec<Done> = Vec::with_capacity(hashes.len());
        for drv_hash in hashes {
            let Some(state) = self.dag.node_mut(drv_hash) else {
                continue;
            };
            if let Err(e) = state.transition(DerivationStatus::Completed) {
                warn!(drv_hash = %drv_hash, error = %e,
                      "store-hit Ready→Completed rejected; dispatching instead");
                continue;
            }
            // IA-only convenience: `expected_output_paths` IS the
            // realised path. Non-destructive when a path is already
            // known — a caller that resolved the REALIZED floating-CA
            // path (the materialization consumption path carries it on
            // the job report) must not have it clobbered with
            // `expected_output_paths == [""]`, which would drop GC
            // retention and emit `[""]` to clients.
            if state.output_paths.is_empty() {
                state.output_paths = state.expected_output_paths.clone();
            }
            ok.push(Done {
                hash: drv_hash.clone(),
                drv_path: state.drv_path().to_string(),
                output_paths: state.output_paths.clone(),
                interested: state.interested_builds.clone(),
            });
        }
        if ok.is_empty() {
            return;
        }
        info!(
            count = ok.len(),
            "outputs already in store; skipping dispatch"
        );
        metrics::counter!("rio_scheduler_cache_hits_total", "source" => "dispatch")
            .increment(ok.len() as u64);

        let ok_hashes: Vec<DrvHash> = ok.iter().map(|d| d.hash.clone()).collect();
        let ok_refs: Vec<&str> = ok_hashes.iter().map(|h| h.as_str()).collect();
        self.persist_status_batch(&ok_refs, DerivationStatus::Completed)
            .await;
        self.upsert_path_tenants_for_batch(&ok_hashes).await;

        // Batched promote: dedup find_newly_ready across all completed
        // hashes, transition in-mem, then one
        // persist_status_batch(Ready). Same shape as the
        // ca_cutoff_cascade batched-promote.
        let mut newly_ready: Vec<DrvHash> = Vec::new();
        let mut seen_ready: HashSet<DrvHash> = HashSet::new();
        for h in &ok_hashes {
            for ready_hash in self.dag.find_newly_ready(h) {
                if !seen_ready.insert(ready_hash.clone()) {
                    continue;
                }
                if let Some(s) = self.dag.node_mut(&ready_hash)
                    && s.transition(DerivationStatus::Ready).is_ok()
                {
                    newly_ready.push(ready_hash);
                }
            }
        }
        let ready_refs: Vec<&str> = newly_ready.iter().map(|h| h.as_str()).collect();
        self.persist_status_batch(&ready_refs, DerivationStatus::Ready)
            .await;

        // Per-build (not per-drv): emit one cached event per (drv,
        // interested-build), then a single summary scan + counts +
        // completion-check per distinct build. I-103: dispatch-time
        // short-circuit counts as cached.
        let mut cached_per_build: HashMap<Uuid, u32> = HashMap::new();
        for d in &ok {
            let event = rio_proto::types::build_event::Event::Derivation(
                rio_proto::types::DerivationEvent::cached(
                    d.drv_path.clone(),
                    d.output_paths.clone(),
                ),
            );
            for &build_id in &d.interested {
                self.events.emit(build_id, event.clone());
                *cached_per_build.entry(build_id).or_default() += 1;
            }
        }
        for (build_id, n) in cached_per_build {
            // r[impl sched.build.terminal-status-settled+3]
            // Dispatch-time store hits can fan out to resident terminal
            // builds that retained interest on the shared node (a
            // stale-Completed reset under a later build re-dispatched
            // it); their served accounting and progress are frozen at
            // the terminal transition. The per-drv DerivationCached
            // event above still flows.
            if let Some(b) = self.builds.get_mut(&build_id)
                && !b.state().is_terminal()
            {
                b.cached_count += n;
            }
            // I-140: one build_summary scan shared, not two.
            let summary = self.dag.build_summary(build_id);
            self.update_build_counts_with(build_id, &summary).await;
            self.emit_progress_with(build_id, &summary);
            self.check_build_completion(build_id).await;
        }
    }

    /// Phase 4 of [`assign_to_worker`](Self::assign_to_worker): emit
    /// `DerivationStarted` + progress to interested gateways.
    pub(super) fn emit_assignment_started(&mut self, drv_hash: &DrvHash, executor_id: &ExecutorId) {
        let drv_path = self.dag.path_or_hash_fallback(drv_hash);
        // The execution this dispatch minted (`assign_to_worker` set
        // `state.exec_id = Some(..)` before calling here). The gateway
        // keys its per-execution TailLog subscription on this; a
        // duplicate Started carrying a *different* exec_id tells it the
        // derivation was re-dispatched and the old subscription is
        // stale. Empty only on the unreachable node-vanished race
        // (the event is then display-only noise for an already-dead
        // derivation).
        let exec_id = self
            .dag
            .node(drv_hash)
            .and_then(|s| s.exec_id)
            .map(|id| id.to_string())
            .unwrap_or_default();
        for build_id in self.get_interested_builds(drv_hash) {
            self.events.emit(
                build_id,
                rio_proto::types::build_event::Event::Derivation(
                    rio_proto::types::DerivationEvent::started(
                        drv_path.clone(),
                        executor_id.to_string(),
                        exec_id.clone(),
                    ),
                ),
            );
            // Progress snapshot: running count +1, worker set changed.
            // Critpath unchanged on dispatch (no completion) — but the
            // dashboard also uses Progress for running/queued columns.
            self.emit_progress(build_id);
        }
    }

    /// Construct the [`WorkAssignment`] proto for `drv_hash` →
    /// `executor_id`: CA-input resolve, HMAC token sign, build-options
    /// lookup. Side-effect: stashes `pending_realisation_deps` on the
    /// node so `handle_success_completion` can write the realisation FK
    /// rows post-build.
    ///
    /// Returns `None` if the DAG node is gone (TOCTOU vs. concurrent
    /// cancel) — caller treats that as assignment failure.
    ///
    /// [`WorkAssignment`]: rio_proto::types::WorkAssignment
    pub(super) async fn build_assignment_proto(
        &mut self,
        drv_hash: &DrvHash,
        executor_id: &ExecutorId,
    ) -> Option<rio_proto::types::WorkAssignment> {
        // CA input resolution: rewrite placeholder paths in
        // env/args/builder to realized output paths before
        // dispatch. Fires when gateway set needs_resolve (ADR-018
        // Appendix B: floating-CA self OR ia.deferred — IA drv
        // with a floating-CA input).
        //
        // `maybe_resolve_ca` returns the (possibly rewritten)
        // drv_content PLUS the realisation lookups performed. On
        // resolve error (missing realisation, PG blip) it logs and
        // returns the original unresolved bytes + empty lookups —
        // the worker's build fails on the placeholder path, which
        // is the correct signal (retry after the realisation lands).
        //
        // The resolve runs in its OWN scoped borrow of `self.dag`
        // (node() + collect_ca_inputs both &-borrow) so the lookups
        // can be stashed via node_mut() below before the main
        // WorkAssignment construction takes its own & borrow.
        let (drv_content_to_send, resolve_lookups, resolved_output_paths) = {
            let state = self.dag.node(drv_hash)?;
            self.maybe_resolve_ca(drv_hash, state).await
        };

        // Stash lookups for handle_success_completion's
        // insert_realisation_deps (the FK needs the parent's own
        // realisation row to exist, which only happens post-build).
        // Empty vec → no-op; non-empty only for CA-on-CA chains
        // that actually resolved.
        //
        // Deferred-IA: also overwrite expected_output_paths with the
        // post-resolve computed paths (index-aligned with output_names)
        // so the HMAC `expected_outputs` claim below carries the real
        // path, not `""`. Floating-CA leaves resolved_output_paths
        // empty → no overwrite (its HMAC path is `is_ca` instead).
        if (!resolve_lookups.is_empty() || !resolved_output_paths.is_empty())
            && let Some(state) = self.dag.node_mut(drv_hash)
        {
            state.ca.pending_realisation_deps = resolve_lookups;
            for (name, path) in resolved_output_paths {
                if let Some(i) = state.output_names.iter().position(|n| n == &name)
                    && let Some(slot) = state.expected_output_paths.get_mut(i)
                {
                    *slot = path;
                }
            }
        }

        let state = self.dag.node(drv_hash)?;
        let build_opts = self.build_options_for_derivation(drv_hash);

        // Assignment token: HMAC-signed if configured, else
        // legacy format-string. The store verifies signed
        // tokens on PutPath (prevents arbitrary-path upload
        // from a compromised worker). Unsigned tokens are
        // accepted by a store with hmac_verifier=None (dev).
        //
        // Expiry: 2× build_timeout (or 2× daemon_timeout
        // default if timeout=0). A worker legitimately
        // uploading after completion is well within that
        // window. Prevents replay from a leaked token later.
        let assignment_token = if let Some(signer) = &self.hmac_signer {
            let timeout_secs = if build_opts.build_timeout > 0 {
                build_opts.build_timeout
            } else {
                // Match rio-builder's DEFAULT_DAEMON_TIMEOUT.
                // Can't reference the const cross-crate, so
                // duplicate the value. 7200s = 2h.
                7200
            };
            // Clamp BEFORE saturating_mul: a client sending
            // build_timeout=u64::MAX would get saturating_mul
            // → u64::MAX → expiry_unix = u64::MAX = immortal
            // token. A leaked immortal token defeats the
            // replay-prevention purpose of expiry entirely.
            // 7 days max: well above any real build duration.
            const MAX_HMAC_TIMEOUT_SECS: u64 = 7 * 86400;
            let timeout_secs = timeout_secs.min(MAX_HMAC_TIMEOUT_SECS);
            let expiry_unix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
                .saturating_add(timeout_secs.saturating_mul(2));
            signer.sign(&rio_auth::hmac::AssignmentClaims {
                executor_id: executor_id.to_string(),
                drv_hash: drv_hash.to_string(),
                expected_outputs: state.expected_output_paths.clone(),
                // Floating-CA: output path is computed post-build
                // from the NAR hash, so expected_output_paths is
                // [""] here. Store skips the path-in-claims check
                // when is_ca is set (verify-on-put still hashes
                // the NAR independently; threat model holds).
                // Fixed-output CA (FOD) has a known path → treat
                // as IA for the membership check.
                is_ca: state.ca.is_ca && !state.is_fixed_output,
                expiry_unix,
                // Tenant attribution for hw_perf_samples.submitting_tenant (M_054).
                // Phase 2 of the bug_011 two-phase rollout (Phase 1 = fb096e50f);
                // safe to set unconditionally since fb096e50f's `skip_serializing_if`
                // + `#[serde(default)]` cover both rolling-upgrade skew directions.
                tenant: state.attributed_tenant(&self.builds).map(|u| u.to_string()),
            })
        } else {
            // Legacy unsigned: format-string. Store with
            // hmac_verifier=None accepts this (content is not
            // verified; the executor/drv pair keeps it unique enough
            // for log correlation).
            format!("{executor_id}-{drv_hash}")
        };

        Some(rio_proto::types::WorkAssignment {
            drv_path: state.drv_path().to_string(),
            // Forward what the gateway inlined (or empty → worker
            // fetches from store). Gateway only inlines for nodes
            // whose outputs are MISSING (will-dispatch), so cache
            // hits don't bloat this. Worker already handles both
            // paths (executor/mod.rs:241 branches on is_empty).
            // For CA-depends-on-CA derivations, this is the
            // RESOLVED ATerm (placeholders replaced by realized
            // paths) — see maybe_resolve_ca above.
            drv_content: drv_content_to_send,
            output_names: state.output_names.clone(),
            build_options: Some(build_opts),
            assignment_token,
            is_fixed_output: state.is_fixed_output,
            traceparent: state.traceparent.clone(),
            // Intent for this drv (matches the SpawnIntent that spawned the
            // pod). Builder clamps to cgroup cpu.max so a wildcard worker
            // (different intent) still gets ground-truth. Populating this
            // makes resolve_build_opts override client `--cores N`. The
            // telemetry side (build_samples.cpu_limit_cores) records this
            // same value at completion — see record_build_sample.
            assigned_cores: state.sched.last_intent.as_ref().map(|i| i.cores),
            // mem/disk are display-only on the worker side — the `rio:
            // builder` banner renders them; runtime limits come from the
            // pod's cgroup, which the spawn intent already shaped. Populate
            // from the same `SolvedIntent` so the banner shows what was
            // actually fitted instead of `?`. Absent (no intent — wildcard
            // worker, or pre-ADR-023 path) → banner renders `?`.
            assigned_mem_bytes: state.sched.last_intent.as_ref().map(|i| i.mem_bytes),
            assigned_disk_bytes: state.sched.last_intent.as_ref().map(|i| i.disk_bytes),
            // Per-execution identifier minted in `assign_to_worker` and
            // stored on `DerivationState`. The worker echoes this in the
            // log header (display-only). Empty only on the unreachable
            // path where the dag node lost its exec_id between assign and
            // proto-build — the actor is single-threaded so that can't
            // happen, but the field is non-Option in the proto.
            exec_id: state.exec_id.map(|u| u.to_string()).unwrap_or_default(),
        })
    }

    /// If this derivation is CA-floating with CA inputs, resolve
    /// placeholder paths to realized output paths before dispatch.
    /// Returns `(drv_content_bytes, realisation_lookups)`: the
    /// (possibly rewritten) ATerm plus every
    /// `(dep_modular_hash, dep_output_name) → realized_path` lookup
    /// the resolve performed. Caller stashes lookups on
    /// `DerivationState.ca.pending_realisation_deps` for the
    /// completion-time `insert_realisation_deps` call (the FK needs
    /// the parent's OWN realisation row to exist first).
    ///
    /// ADR-018 Appendix B: resolve fires when `needs_resolve` is set
    /// by the gateway — floating-CA self (`has_ca_floating_outputs`)
    /// OR any inputDrv is floating-CA (`ia.deferred`: an IA drv
    /// depending on a CA input has the CA placeholder embedded in
    /// its env/args). Fixed-output CA with no CA inputs doesn't need
    /// resolve — its output path AND its inputs' paths are all
    /// eval-time known.
    ///
    /// The resolve step queries the `realisations` table for each CA
    /// input's `(modular_hash, output_name)` → `output_path`, then
    /// string-replaces placeholders through the ATerm. Each lookup
    /// is staged for `realisation_deps` INSERT (rio's derived build
    /// trace, per ADR-018:45) — though the actual INSERT is deferred
    /// to completion time (the FK needs the parent's OWN realisation
    /// to exist, which only happens post-build).
    ///
    /// Error handling: resolve failure (missing realisation, PG blip)
    /// logs and returns the original unresolved bytes + empty lookups.
    /// The worker's build will then fail on the placeholder path not
    /// existing (`/1ril1qzj...` is not a real store path), triggering
    /// the normal retry-with-backoff. This is correct: a missing
    /// realisation means the input's `wopRegisterDrvOutput` hasn't
    /// landed yet (race), and retry-after-backoff gives it time to.
    async fn maybe_resolve_ca(
        &self,
        drv_hash: &DrvHash,
        state: &crate::state::DerivationState,
    ) -> (
        Vec<u8>,
        Vec<crate::ca::RealisationLookup>,
        Vec<(String, String)>,
    ) {
        // Gate: ADR-018 Appendix B `shouldResolve`. Gateway computes
        // `needs_resolve = has_ca_floating_outputs() || any inputDrv
        // is floating-CA` at translate time. Covers both floating-CA
        // self AND ia.deferred (IA with CA inputs — the CA input's
        // placeholder is embedded in this drv's env/args and needs
        // rewriting to the realized path).
        if !state.ca.needs_resolve {
            return (state.drv_content.clone(), Vec::new(), Vec::new());
        }

        // Build the input lists: walk DAG children, split into CA
        // and IA. For CA children we need the MODULAR hash (the
        // `realisations` table key, plumbed by the gateway via
        // `DerivationNode.ca.modular_hash`). For IA children we need
        // the `expected_output_paths` (deterministic, computed at
        // gateway submit time from the parsed `.drv`).
        //
        // Nix's `tryResolve` (derivations.cc:1206-1234) iterates ALL
        // inputDrvs regardless of addressing mode, adding each output
        // path to `inputSrcs`. CA outputs come from realisations; IA
        // outputs are concrete and already in the DAG.
        //
        // Floating-CA with NO inputs at all (rare: a leaf CA drv with
        // only fixed srcs) doesn't need resolve — nothing to collapse
        // into inputSrcs. Short-circuit before the ATerm parse.
        let ca_inputs = self.collect_ca_inputs(drv_hash);
        let ia_inputs = self.collect_ia_inputs(drv_hash);
        if ca_inputs.is_empty() && ia_inputs.is_empty() {
            return (state.drv_content.clone(), Vec::new(), Vec::new());
        }

        // No drv_content → recovered derivation (scheduler restart,
        // DAG reloaded from PG, drv_content not persisted). The store
        // has the ATerm — fetch it. Workers do the same when the
        // inline is empty (build_types.proto:231: "Empty = fallback;
        // worker fetches via GetPath"). ~10-50ms round-trip, once
        // per recovered floating-CA dispatch.
        //
        // Checked AFTER the both-empty short-circuit: a recovered
        // floating-CA with no DAG inputs doesn't need resolve and
        // doesn't need the fetch — worker fetches the unresolved
        // `.drv` from the store itself (same path it always does
        // when `drv_content` is empty). Any floating-CA WITH inputs
        // (CA or IA) needs the scheduler-side fetch so
        // `resolve_ca_inputs` can parse `inputDrvs` and serialize
        // the resolved `BasicDerivation` form.
        //
        // The same lossy-on-recovery pattern still applies to
        // `ca_modular_hash` (see `collect_ca_inputs`'s skip-on-None)
        // and `pending_realisation_deps` (best-effort cache,
        // reconstituted here on each resolve).
        //
        // r[impl sched.ca.resolve+3]
        let drv_content = if state.drv_content.is_empty() {
            match self.fetch_drv_content_from_store(drv_hash, state).await {
                Some(bytes) => bytes,
                None => {
                    // Store unreachable or .drv not found — dispatch
                    // unresolved (worker fails on placeholder,
                    // self-heals via retry after a fresh SubmitBuild
                    // re-merges with inline drv_content). Same
                    // degrade as before P0408.
                    warn!(
                        drv_hash = %drv_hash,
                        "recovered CA-on-CA dispatch: drv_content empty + store fetch failed; \
                         dispatching unresolved (worker will fail on placeholder)"
                    );
                    return (state.drv_content.clone(), Vec::new(), Vec::new());
                }
            }
        } else {
            state.drv_content.clone()
        };

        match crate::ca::resolve_ca_inputs(&drv_content, &ca_inputs, &ia_inputs, self.db.pool())
            .await
        {
            Ok(resolved) => {
                debug!(
                    drv_hash = %drv_hash,
                    n_ca_inputs = ca_inputs.len(),
                    n_ia_inputs = ia_inputs.len(),
                    n_lookups = resolved.lookups.len(),
                    "CA resolve: rewrote placeholders + collapsed inputSrcs for dispatch"
                );
                (
                    resolved.drv_content,
                    resolved.lookups,
                    resolved.output_paths,
                )
            }
            Err(e) => {
                // Swallow-to-warn for ALL ResolveError variants,
                // including `Db` (transient PG blip). Rationale:
                // the unresolved dispatch → worker fails on the
                // placeholder path → retry-with-backoff fires →
                // next dispatch re-runs resolve. For
                // `RealisationMissing`, the backoff gives the
                // input's `wopRegisterDrvOutput` time to land
                // (race). For `Db`, the backoff IS the retry-PG
                // mechanism — the wasted worker cycle (~seconds
                // to fail on ENOENT) is acceptable vs adding a
                // defer-and-requeue path here (would need a timer
                // to re-dispatch, which `backoff_until` already
                // provides on the FAILURE path). Slot-wasteful
                // but correct; profiling can drive a `Db → defer`
                // split if the waste proves measurable.
                warn!(
                    drv_hash = %drv_hash,
                    error = %e,
                    "CA resolve failed; dispatching unresolved (worker will fail on placeholder)"
                );
                // Return the (possibly fetched-from-store) bytes
                // unresolved. If the fetch succeeded but resolve
                // failed, the worker at least skips its own GetPath.
                (drv_content, Vec::new(), Vec::new())
            }
        }
    }

    /// Fetch a derivation's ATerm bytes from the store via `GetPath`.
    ///
    /// The store returns NAR-framed bytes; a `.drv` is a single
    /// regular file, so [`rio_nix::nar::extract_single_file`] unwraps
    /// it to the raw ATerm. This is the same path the worker takes
    /// when `WorkAssignment.drv_content` is empty
    /// ([`rio-builder/src/executor/inputs.rs::fetch_drv_from_store`]).
    ///
    /// Returns `None` on any failure: store unconfigured
    /// (`store_client = None`, test mode), `GetPath` error, timeout,
    /// not-found, or NAR unwrap failure. Callers treat `None` as
    /// "degrade to the pre-P0408 behavior" — dispatch unresolved,
    /// worker fails on placeholder, retry-with-backoff self-heals.
    ///
    /// Hard 2s timeout + 1 MiB NAR cap: a `.drv` is ~1-50 KB ASCII.
    /// A larger-than-1-MiB blob means something is badly wrong (the
    /// path isn't a `.drv`, or the store returned a closure NAR).
    /// Either way, bail — resolve can't parse a non-ATerm.
    async fn fetch_drv_content_from_store(
        &self,
        drv_hash: &DrvHash,
        state: &crate::state::DerivationState,
    ) -> Option<Vec<u8>> {
        /// `.drv` NAR cap. ~1-50 KB typical; 1 MiB is ~20× any
        /// real-world `.drv`. Avoids pulling a multi-GB closure if
        /// the store path was mis-resolved.
        const MAX_DRV_NAR_SIZE: u64 = 1024 * 1024;
        /// Per-chunk idle bound for `GetPath` (initial RPC + each
        /// stream.message() — I-211, not whole-call). ~10-50 ms
        /// typical; 2 s covers a slow store without blocking
        /// dispatch for long. On timeout we degrade to unresolved
        /// dispatch (same as store-unconfigured).
        const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

        let mut client = self.store_client.as_ref()?.clone();
        let drv_path = state.drv_path().to_string();

        let result = rio_proto::client::get_path_nar(
            &mut client,
            &drv_path,
            FETCH_TIMEOUT,
            MAX_DRV_NAR_SIZE,
            None,
            &[],
        )
        .await;

        let nar = match result {
            Ok(Some((_info, nar))) => nar,
            Ok(None) => {
                debug!(
                    drv_hash = %drv_hash,
                    drv_path = %drv_path,
                    "recovered CA resolve: .drv not found in store"
                );
                return None;
            }
            Err(e) => {
                debug!(
                    drv_hash = %drv_hash,
                    drv_path = %drv_path,
                    error = %e,
                    "recovered CA resolve: GetPath failed"
                );
                return None;
            }
        };

        // NAR unwrap: .drv is a single regular file. Anything else
        // (directory, symlink, corrupt NAR) → None.
        match rio_nix::nar::extract_single_file(&nar) {
            Ok(bytes) => Some(bytes),
            Err(e) => {
                debug!(
                    drv_hash = %drv_hash,
                    error = %e,
                    "recovered CA resolve: NAR unwrap failed (not a single regular file)"
                );
                None
            }
        }
    }

    /// Collect CA inputs for resolve. Walks the DAG children (deps)
    /// and returns a `CaResolveInput` for each child with
    /// `is_ca = true` AND a populated `ca_modular_hash`.
    ///
    /// Children with `is_ca && ca_modular_hash.is_none()` are
    /// skipped — the gateway couldn't compute the modular hash
    /// (BasicDerivation fallback, or recovered state where the
    /// hash wasn't persisted). The parent's resolve is incomplete
    /// for that input → worker fails on the placeholder path →
    /// retry-with-backoff. The next SubmitBuild referencing this
    /// child re-merges the proto with a fresh `ca_modular_hash`.
    fn collect_ca_inputs(&self, drv_hash: &DrvHash) -> Vec<crate::ca::CaResolveInput> {
        // DAG children = dependencies (must complete before this drv).
        let children = self.dag.get_children(drv_hash);
        let mut inputs = Vec::new();
        for child_hash in children {
            let Some(child) = self.dag.node(&child_hash) else {
                continue;
            };
            if !child.ca.is_ca {
                continue;
            }
            let Some(modular_hash) = child.ca.modular_hash else {
                // Gateway didn't populate (BasicDerivation fallback
                // OR recovered state). Skip — resolve is incomplete
                // for this input, worker fails on placeholder,
                // retry-with-backoff handles it. debug not warn:
                // recovered chains hit this legitimately; the
                // scheduler-restart-mid-CA-chain case is expected
                // to degrade to worker-retry, not spam logs.
                debug!(
                    drv_hash = %drv_hash,
                    child = %child_hash,
                    "collect_ca_inputs: child is CA but ca_modular_hash unset; \
                     resolve incomplete, worker will fail on placeholder"
                );
                continue;
            };
            inputs.push(crate::ca::CaResolveInput {
                drv_path: child.drv_path().to_string(),
                modular_hash,
            });
        }
        inputs
    }

    /// Collect IA (input-addressed) inputs for resolve. Walks the
    /// DAG children and returns an [`IaResolveInput`] for each child
    /// with `is_ca = false` AND non-empty `expected_output_paths`.
    ///
    /// IA output paths are deterministic — the gateway computed them
    /// at submit time from the parsed `.drv` and plumbed them via
    /// `DerivationNode.expected_output_paths`. No store RPC needed.
    /// This is the same field [`approx_input_closure`] reads for the
    /// prefetch hint, so the data is already live.
    ///
    /// Children with empty `expected_output_paths` (recovered state
    /// where the paths weren't persisted, or a proto without the
    /// field) are skipped — `resolve_ca_inputs` will log and skip
    /// the `inputSrcs` add for that input. The worker's FUSE layer
    /// on-demand-fetches regardless, so builds don't break; only
    /// resolved-drv-hash compat with Nix is affected.
    ///
    /// [`IaResolveInput`]: crate::ca::IaResolveInput
    /// [`approx_input_closure`]: crate::assignment::approx_input_closure
    fn collect_ia_inputs(&self, drv_hash: &DrvHash) -> Vec<crate::ca::IaResolveInput> {
        let children = self.dag.get_children(drv_hash);
        let mut inputs = Vec::new();
        for child_hash in children {
            let Some(child) = self.dag.node(&child_hash) else {
                continue;
            };
            if child.ca.is_ca {
                // CA child with a modular hash — handled by
                // collect_ca_inputs via realisation lookup. But a CA
                // child WITHOUT a modular hash (recovered state,
                // BasicDerivation fallback) that HAS completed can
                // still contribute its realized output_paths here:
                // the resolve doesn't need the realisation table when
                // we already have the concrete path in-memory.
                if child.ca.modular_hash.is_some() || child.output_paths.is_empty() {
                    continue;
                }
                // Fall through: CA child, no modular hash, but
                // output_paths is populated (completed). Treat as IA
                // for the purpose of inputSrcs collection — the
                // realized path is just as concrete as an IA
                // expected_output_path.
            }
            // Prefer realized output_paths (filled on completion) over
            // expected_output_paths (filled at merge). For IA children
            // the two are equivalent; for the CA-no-hash fallthrough
            // above, only output_paths is usable (expected is [""]
            // for floating-CA).
            let paths = if !child.output_paths.is_empty() {
                &child.output_paths
            } else {
                &child.expected_output_paths
            };
            if paths.is_empty() {
                // Recovered node or proto without the field. Skip;
                // resolve_ca_inputs logs and skips the inputSrcs add.
                continue;
            }
            inputs.push(crate::ca::IaResolveInput {
                drv_path: child.drv_path().to_string(),
                output_names: child.output_names.clone(),
                output_paths: paths.clone(),
            });
        }
        inputs
    }
}
