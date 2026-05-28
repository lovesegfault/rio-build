//! Ready-set store short-circuit and substitution machinery, plus the
//! shared `WorkAssignment` payload constructor the pull path uses.
//!
//! The stream-era placement/assign pass (`dispatch_ready` and the
//! 4-phase assign path) was deleted with the placement layer; work
//! delivery is pull-only (`actor/pull.rs`). What remains here is
//! dispatch-mode-independent: completing/substituting Ready
//! derivations whose outputs already exist, the detached substitute
//! fetch machinery, and `build_assignment_proto`/`emit_assignment_started`
//! (shared with the pull mint).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use rio_common::limits::MAX_SUBSTITUTE_CLOSURE;

use uuid::Uuid;

use tracing::{debug, error, info, warn};

use rio_proto::types::FindMissingPathsRequest;

use crate::dag::ClosureEvidence;
use crate::state::{
    DerivationStatus, DrvHash, ExecutorId, effective_wanted, verifiable_wanted_paths, wanted_subset,
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
    /// Iterates the full DAG, not just `ready_queue` — `ready_queue` is
    /// a heap (no peek-iter without drain) and stale entries in it are
    /// harmless (the inner-loop status guard drops them after this
    /// completes them). Full-DAG scan is O(nodes) but the actor is
    /// single-threaded so there's no contention; for a 1085-node merge
    /// the scan is sub-ms vs. ~25s of sequential RPCs it replaces.
    ///
    /// Returns the set of hashes the drain loop must skip
    /// `ready_check_or_spawn` for (I-163). On success this is the
    /// batch-probed head (completed here or definitively found-missing
    /// one RPC ago) plus the truncated tail (deferred to next pass's
    /// batch). On RPC error/timeout this is the tail only — the
    /// stamped head is protected via `probed_generation`, so neither
    /// hits the per-drv fallback.
    // r[impl sched.dispatch.fod-substitute+2]
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
        let auth = self.probe_substitute_auth(candidates.iter().map(|(h, _)| h));
        let probe = auth.mint();
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

        // r[impl sched.substitute.detached+5]
        // Partition: locally-present (not in missing_paths) → complete
        // inline; substitutable → spawn detached fetch; truly-missing →
        // leave Ready (dispatches normally). The detached fetch runs
        // OUTSIDE the actor loop — before this, the awaited
        // eager_substitute_fetch blocked MergeDag/dispatch for >100s
        // when the closure walk pulled ghc-sized NARs.
        let missing: HashSet<String> = resp.missing_paths.into_iter().collect();
        let substitutable: HashSet<String> = resp.substitutable_paths.into_iter().collect();
        let indeterminate: HashSet<String> = resp.indeterminate_paths.into_iter().collect();
        // I-139: collect-then-batch. The locally-present branch awaited
        // `complete_ready_from_store` per item (≥3 sequential PG RTTs
        // each); on warm-restart of a large closure ~all 2048 candidates
        // hit it → 12-30s actor stall → heartbeats missed → live workers
        // reaped. The Substituting branch already batched.
        let mut locally_present = Vec::new();
        let mut to_spawn = Vec::new();
        // Topdown-pruned roots with broken closure evidence (childless
        // or closure-holed) whose wanted set can neither complete
        // inline nor route to substitution: fail fast instead of
        // leaving them Ready (see the arm below).
        let mut to_fail_fast: Vec<DrvHash> = Vec::new();
        for (drv_hash, paths) in candidates {
            checked.insert(drv_hash.clone());
            let substitute_tried = self.dag.node(&drv_hash).is_some_and(|s| s.substitute_tried);
            // r[impl sched.merge.wanted-outputs+2]
            // Demand-driven completeness: only the WANTED outputs must
            // be present (→ complete inline) or present-or-
            // substitutable (→ detached fetch). A missing output
            // nothing consumes must not force a from-source dispatch.
            // The wanted slice is the LIVE effective wanted set
            // (`effective_wanted` over live interested builds'
            // contributions; a terminal build's wants stop counting),
            // falling back to the stored node-level union when it is
            // unavailable. `verifiable_wanted_paths` returns None for a
            // wanted set that resolves to no verifiable path; degrade
            // to all of `paths` then (and for a node that vanished from
            // the DAG mid-probe). The probe set and the `to_spawn` walk
            // seeds stay ALL expected paths (opportunistic completeness
            // — fetch the unwanted output too if the upstream has it).
            let wanted: Vec<String> = self
                .dag
                .node(&drv_hash)
                .and_then(|s| {
                    let eff = effective_wanted(s, &self.builds);
                    verifiable_wanted_paths(
                        &s.output_names,
                        &s.expected_output_paths,
                        eff.as_deref().unwrap_or(&s.wanted_output_names),
                    )
                    .map(|w| w.into_iter().map(str::to_owned).collect::<Vec<String>>())
                })
                .unwrap_or_else(|| paths.clone());
            if wanted.iter().all(|p| !missing.contains(p)) {
                // `substitute_tried` ⇒ the closure walk ingested the
                // seed (output) then failed on a ref — output-present
                // in PG does NOT imply closure-complete. FMP probes
                // output paths only, so "present" here can hide a
                // hole. Fall through to dispatch (build re-derives the
                // full closure) instead of marking Completed.
                if !substitute_tried {
                    locally_present.push(drv_hash);
                }
            } else if !substitute_tried
                && wanted.iter().all(|p| {
                    !missing.contains(p) || substitutable.contains(p) || indeterminate.contains(p)
                })
            {
                // r[impl sched.merge.substitute-probe-indeterminate]
                // Indeterminate treated optimistically — same as
                // merge.rs. The closure walk's failure path falls
                // through to build via `substitute_tried`.
                to_spawn.push((drv_hash, paths));
            } else if self.must_substitute(&drv_hash) {
                // r[impl sched.merge.substitute-topdown+10]
                // Truly missing (a wanted output is missing upstream and
                // not substitutable): every other node is left Ready and
                // dispatches from source. A topdown-pruned root whose
                // closure evidence is Broken (childless or closure-holed)
                // must not — its dep closure was never merged, so a
                // from-source dispatch is doomed (worker ENOENTs on
                // inputDrvs). This is the post-failover shape: the
                // recovered wanted union ('{}' = all declared) is wider
                // than the prune-time criterion, so an output the prune
                // never vouched for can be definitively missing here.
                // Fail fast with the resubmit-directing error instead.
                to_fail_fast.push(drv_hash);
            }
        }
        self.complete_ready_from_store_batch(&locally_present).await;
        self.spawn_substitute_fetches(to_spawn, auth).await;
        for drv_hash in &to_fail_fast {
            self.fail_fast_topdown_pruned_root(
                drv_hash,
                "wanted output(s) missing upstream and not substitutable at dispatch \
                 after deps were pruned",
            )
            .await;
        }
        checked
    }

    /// Resolve auth for dispatch-time store calls. `Jwt(vec![])`
    /// (no-auth) when no `service_signer` (dev mode) or no candidate
    /// has a known tenant (single-tenant mode / recovered orphan);
    /// otherwise `Service{signer, tenant_id}` so the detached
    /// closure-walk task can re-mint tokens past the original 60s.
    #[allow(clippy::extra_unused_lifetimes)] // 'a only in impl-Trait arg
    fn probe_substitute_auth<'a>(
        &self,
        drv_hashes: impl Iterator<Item = &'a DrvHash>,
    ) -> SubstituteAuth {
        let tid = drv_hashes
            .filter_map(|h| self.dag.node(h))
            .flat_map(|s| s.interested_builds.iter())
            .filter_map(|bid| self.builds.get(bid))
            .find_map(|b| b.tenant_id);
        self.substitute_auth_for_tenant(tid)
    }

    /// Build a [`SubstituteAuth`] for a known `tenant_id`. `Service`
    /// when both `service_signer` and `tenant_id` are present (so
    /// `walk_substitute_closure` can re-mint past the original
    /// expiry); `Jwt(vec![])` (no-auth) otherwise — dev mode or
    /// single-tenant. Used by both `probe_substitute_auth`
    /// (dispatch-time, derives `tenant_id` from the DAG) and
    /// merge-time (`MergeDagRequest.tenant_id` is already in hand).
    pub(super) fn substitute_auth_for_tenant(&self, tenant_id: Option<Uuid>) -> SubstituteAuth {
        match (&self.service_signer, tenant_id) {
            (Some(signer), Some(tid)) => SubstituteAuth::Service {
                signer: signer.clone(),
                tenant_id: tid,
            },
            _ => SubstituteAuth::Jwt(Vec::new()),
        }
    }

    fn inject_probe_meta(md: &mut tonic::metadata::MetadataMap, meta: &[(&'static str, &str)]) {
        for (k, v) in meta {
            if let Ok(mv) = tonic::metadata::MetadataValue::try_from(*v) {
                md.insert(*k, mv);
            }
        }
    }

    // r[impl sched.substitute.detached+5]
    /// Transition each candidate to `Substituting` and spawn a
    /// background task that triggers store-side `try_substitute` (via
    /// `QueryPathInfo`) for its output paths AND their transitive
    /// reference closure, then posts
    /// [`ActorCommand::SubstituteComplete`] back into the mailbox.
    ///
    /// Detaches the upstream NAR fetch from the actor event loop:
    /// before this, `eager_substitute_fetch` was awaited inline and a
    /// single ghc-sized closure walk blocked `MergeDag` for >100s
    /// (`"actor command exceeded 1s","cmd":"MergeDag","elapsed":"135s"`).
    ///
    /// Candidates whose transition is rejected (vanished, wrong status)
    /// are skipped — they fall through to normal scheduling.
    /// `auth` is the `(signer, tenant_id)` pair (both merge- and
    /// dispatch-time, via `substitute_auth_for_tenant`) so the spawned
    /// task can re-mint a fresh service token AFTER acquiring
    /// `substitute_sem`, once per BFS layer, and every
    /// `SUBSTITUTE_REMINT_PATHS` / `SUBSTITUTE_REMINT_INTERVAL` inside
    /// the per-path loop: a token minted at spawn-time would expire
    /// while parked on the semaphore or mid-way through a wide cold
    /// closure walk (later QPIs → `NotFound` → spurious `ok=false`).
    pub(super) async fn spawn_substitute_fetches(
        &mut self,
        candidates: Vec<(DrvHash, Vec<String>)>,
        auth: SubstituteAuth,
    ) {
        if candidates.is_empty() {
            return;
        }
        let Some(store) = self.store_client.clone() else {
            return;
        };
        let Some(weak_tx) = self.self_tx.clone() else {
            return;
        };
        struct Spawned {
            hash: DrvHash,
            drv_path: String,
            output_paths: Vec<String>,
            interested: HashSet<Uuid>,
        }
        let mut spawned: Vec<Spawned> = Vec::with_capacity(candidates.len());
        for (drv_hash, paths) in candidates {
            // Live effective wanted set for the forgivable complement
            // below (terminal builds' contributions excluded; stored-
            // union fallback on None) — computed before the `node_mut`
            // borrow since it needs `self.builds`.
            let eff = self
                .dag
                .node(&drv_hash)
                .and_then(|s| effective_wanted(s, &self.builds));
            let Some(state) = self.dag.node_mut(&drv_hash) else {
                continue;
            };
            let from = match state.transition(DerivationStatus::Substituting) {
                Ok(f) => f,
                Err(e) => {
                    debug!(%drv_hash, %e, "spawn_substitute: transition rejected; falling through");
                    continue;
                }
            };
            // Stash the realized paths now so SubstituteComplete →
            // complete_ready_from_store_batch doesn't have to recompute
            // them. Dispatch-time callers pass `expected_output_paths`
            // (no-op semantically); the verify_preexisting_completed
            // reprobe lane passes the REALIZED floating-CA path which
            // would otherwise be lost (cleared at merge.rs reset, then
            // clobbered with `[""]` at the IA-only assignment below).
            state.output_paths = paths.clone();
            // I-094 reprobe substitutable lane: failure history is moot
            // — we're fetching the upstream-built output. The
            // `cache_hit_clear` reset row appended below (in
            // `record_reset_with_clear_poison`, same shape as
            // apply_cached_hits) is what clears the retry state: the
            // fold's reset arm zeroes the counters and the cached view
            // is refreshed when the row lands. Clearing at this point
            // in the chain (not on SubstituteComplete{ok=true}) means a
            // later fetch failure demotes via `revert_target_for`
            // (Ready/Queued/DependencyFailed) and may get one more
            // dispatch attempt — acceptable, since substitutability is
            // evidence the world changed (Hydra/another tenant built
            // it).
            // r[impl sched.merge.wanted-outputs+2]
            // The forgivable seed subset: declared output paths whose
            // name is OUTSIDE the (non-empty) wanted set. The walk
            // still attempts them (opportunistic completeness) but
            // their failure must not demote the derivation to a
            // from-source build. Computed from the LIVE effective
            // wanted set (`effective_wanted` over live interested
            // builds' contributions, stored-union fallback) — the same
            // source the cache-hit classification uses — so a path is
            // only forgiven when NO LIVE build wants it; a terminal
            // build's wants stop pinning seeds as unforgivable. Empty
            // wanted set = all wanted = nothing forgivable (today's
            // behaviour). Only paths positively identifiable as
            // unwanted (declared in `expected_output_paths` but outside
            // the wanted subset) qualify; a seed that matches no
            // declared path (the realized floating-CA path from the
            // reprobe lane) is never forgiven, and neither is a path
            // that already triggered a forgiven-seed-became-wanted
            // downgrade (`never_forgive_paths` — see
            // `handle_substitute_complete`'s termination argument).
            //
            // The complement MUST be taken against the *verifiable*
            // wanted subset. A wanted set that resolves to no
            // verifiable path (`drv^bogus`) yields an EMPTY wanted set
            // here — and the complement of nothing is every declared
            // path, which forgives every seed failure and lets the
            // walk return ok=true having fetched NOTHING. On `None`
            // nothing is positively identifiable as unwanted, so
            // nothing is forgivable: every seed failure fails the walk
            // (the conservative pre-feature behaviour).
            let forgivable: HashSet<String> = match verifiable_wanted_paths(
                &state.output_names,
                &state.expected_output_paths,
                eff.as_deref().unwrap_or(&state.wanted_output_names),
            ) {
                Some(wanted) => {
                    let wanted: HashSet<&str> = wanted.into_iter().collect();
                    state
                        .expected_output_paths
                        .iter()
                        .filter(|p| {
                            !p.is_empty()
                                && !wanted.contains(p.as_str())
                                && paths.contains(*p)
                                && !state.never_forgive_paths.contains(p.as_str())
                        })
                        .cloned()
                        .collect()
                }
                None => HashSet::new(),
            };
            let drv_path = state.drv_path().to_string();
            let interested = state.interested_builds.clone();
            // Best-effort PG clear so recovery doesn't resurrect the
            // poison. After last use of `state` so the &mut self.dag
            // borrow ends before &self.db.
            if matches!(
                from,
                DerivationStatus::Poisoned | DerivationStatus::DependencyFailed
            ) {
                // 1a: `cache_hit_clear` reset row + poison clear in one
                // transaction (same shape as the merge-time cache-hit
                // clears).
                let reset_row = self.reset_row_for(
                    &drv_hash,
                    crate::state::OutcomeClass::CacheHitClear,
                    crate::state::ReportingParty::Scheduler,
                );
                if let Err(e) = self
                    .record_reset_with_clear_poison(&drv_hash, reset_row)
                    .await
                {
                    warn!(%drv_hash, error = %e,
                          "failed to clear poison in PG after re-probe substitutable hit");
                }
            }
            let output_paths = paths.clone();
            let store = store.clone();
            let weak_tx = weak_tx.clone();
            let auth = auth.clone();
            let h = drv_hash.clone();
            let sem = self.substitute_sem.clone();
            let shutdown = self.shutdown.clone();
            rio_common::task::spawn_monitored("substitute-fetch", async move {
                // Bound in-flight closure walks across ALL spawned
                // tasks. The task is already spawned (so the actor
                // returned), but it parks here until a slot is free —
                // Substituting status keeps dependents gated meanwhile.
                // `auth.mint()` is deferred to inside the walk (per
                // layer) so time parked here doesn't eat token expiry.
                let _permit = sem.acquire_owned().await;
                // r[impl gw.activity.subst-progress]
                // Progress emits post back into the actor mailbox.
                // `try_send` (via the weak upgrade) — if the actor is
                // gone or its mailbox full, drop the emit (display-
                // only; SubstituteComplete below uses `send` so the
                // state transition is never lost to backpressure).
                let progress_tx = weak_tx.clone();
                let progress_hash = h.clone();
                let on_progress = move |done: u64, expected: u64, upstream: &str| {
                    if let Some(tx) = progress_tx.upgrade() {
                        let _ = tx.try_send(super::ActorCommand::SubstituteProgress {
                            drv_hash: progress_hash.clone(),
                            bytes_done: done,
                            bytes_expected: expected,
                            upstream_uri: upstream.to_string(),
                        });
                    }
                };
                let (ok, forgiven) = walk_substitute_closure(
                    &store,
                    paths,
                    &forgivable,
                    &auth,
                    &shutdown,
                    on_progress,
                )
                .await;
                if let Some(tx) = weak_tx.upgrade() {
                    let _ = tx
                        .send(super::ActorCommand::SubstituteComplete {
                            drv_hash: h,
                            ok,
                            forgiven,
                        })
                        .await;
                }
            });
            spawned.push(Spawned {
                hash: drv_hash,
                drv_path,
                output_paths,
                interested,
            });
        }
        if !spawned.is_empty() {
            debug!(
                count = spawned.len(),
                "detached upstream substitute fetch spawned"
            );
            metrics::counter!("rio_scheduler_substitute_spawned_total")
                .increment(spawned.len() as u64);
            let hashes: Vec<&str> = spawned.iter().map(|s| s.hash.as_str()).collect();
            self.persist_status_batch(&hashes, DerivationStatus::Substituting)
                .await;
            for s in &spawned {
                let event = rio_proto::types::build_event::Event::Derivation(
                    rio_proto::types::DerivationEvent::substituting(
                        s.drv_path.clone(),
                        s.output_paths.clone(),
                    ),
                );
                for &build_id in &s.interested {
                    self.events.emit(build_id, event.clone());
                }
            }
            // build_summary counts Substituting as running — emit a
            // progress snapshot so the queued/running flip is visible
            // (matches `emit_assignment_started`). Dedup builds across
            // all spawned drvs; emit once per build.
            let interested_builds: HashSet<Uuid> = spawned
                .iter()
                .flat_map(|s| s.interested.iter().copied())
                .collect();
            for build_id in interested_builds {
                self.emit_progress(build_id);
            }
        }
    }

    // r[impl sched.substitute.detached+5]
    /// Handle a [`ActorCommand::SubstituteComplete`] posted by a
    /// detached fetch task. `ok=true` → output now in rio-store with
    /// its full reference closure ([`walk_substitute_closure`] walked
    /// it), so `Substituting → Completed` is safe even if inputDrvs
    /// aren't yet Completed in the DAG. `ok=false` → revert to
    /// `Ready`/`Queued` for normal scheduling.
    ///
    /// `forgiven` is the set of seeds the walk forgave against the
    /// wanted set *as snapshotted at spawn time*; see the re-check
    /// below for why it must be re-evaluated here.
    pub(super) async fn handle_substitute_complete(
        &mut self,
        drv_hash: &DrvHash,
        ok: bool,
        forgiven: &[String],
    ) {
        // r[impl sched.substitute.leader-gate]
        // `on_lose` only flips atomics; the detached `substitute-fetch`
        // task survives lease loss and posts here on the standby. The
        // ok=true branch writes PG (`persist_status(Completed)` /
        // `complete_ready_from_store`) → split-brain on
        // `derivations.status`. Same gate as `dispatch_ready` — the new
        // leader's recovery owns this drv (resets Substituting via the
        // dep-walk).
        if !self.leader.is_leader() {
            debug!(%drv_hash, ok,
                   "SubstituteComplete on standby (lease lost mid-fetch); dropping");
            return;
        }
        let Some(state) = self.dag.node(drv_hash) else {
            return;
        };
        if state.status() != DerivationStatus::Substituting {
            debug!(%drv_hash, status = ?state.status(),
                   "SubstituteComplete: not Substituting (cancelled/re-merged); dropping");
            return;
        }
        let topdown_pruned = state.topdown_pruned;
        // r[impl sched.merge.wanted-outputs+2]
        // The walk's forgiveness verdict was computed against the
        // wanted set as of SPAWN time. A build that merged during the
        // (potentially minutes-long) detached fetch can have made a
        // seed the walk forgave wanted by a live build — completing
        // now would hand that build a node missing an output it wants.
        // The re-check is evaluated against the LIVE effective wanted
        // set (`effective_wanted`, computed at this call site because
        // liveness needs `self.builds`, which the node cannot see),
        // falling back to the stored node-level union — same source as
        // the spawn-time forgivable complement. Downgrade to a revert
        // WITHOUT setting `substitute_tried`: the next dispatch pass
        // re-probes and re-spawns the walk with the corrected
        // forgivable set, so the delta is re-substituted (and a genuine
        // miss then fails the walk → `substitute_tried` → from-source
        // build). Two revert targets cannot wait for "the next pass" —
        // a topdown-pruned root with broken closure evidence (childless
        // or closure-holed) and a node whose dep is terminally failed —
        // so for those the walk is re-spawned immediately below instead
        // (see the downgrade re-spawn ahead of the generic revert).
        //
        // The trigger paths (forgiven at spawn time AND wanted now) are
        // collected — not just detected — so the downgrade can record
        // them in `never_forgive_paths` below.
        let forgiven_now_wanted_paths: Vec<String> = if ok && !forgiven.is_empty() {
            let eff = effective_wanted(state, &self.builds);
            wanted_subset(
                &state.output_names,
                &state.expected_output_paths,
                eff.as_deref().unwrap_or(&state.wanted_output_names),
            )
            .filter(|p| forgiven.contains(p))
            .cloned()
            .collect()
        } else {
            Vec::new()
        };
        let forgiven_now_wanted = !forgiven_now_wanted_paths.is_empty();
        let ok = ok && !forgiven_now_wanted;
        if forgiven_now_wanted {
            // Spend the trigger paths' forgiveness BEFORE any re-spawn
            // (immediate or next-pass): a path that has triggered a
            // downgrade once is never forgivable again for this node,
            // no matter how the live effective wanted set later shrinks
            // (the wanting build goes terminal) or re-grows. This is
            // the monotone step the walk chain's termination argument
            // rests on — see the downgrade re-spawn comment below.
            if let Some(s) = self.dag.node_mut(drv_hash) {
                s.never_forgive_paths
                    .extend(forgiven_now_wanted_paths.iter().cloned());
            }
            info!(%drv_hash,
                  "substitute walk forgave a seed that became wanted \
                   mid-fetch; reverting for re-substitution of the delta");
        }
        if ok {
            // The substitution chain ends in success — the spent-
            // forgiveness bookkeeping is scoped to the chain, so clear
            // it. Safe: no re-spawn follows the ok=true arm, and a NEW
            // chain only starts after an external reset (re-merge of a
            // failed node, stale-Completed verify, recovery), each of
            // which is bounded by the same |declared outputs| argument
            // again — clearing here cannot re-open an unbounded
            // downgrade loop.
            if let Some(s) = self.dag.node_mut(drv_hash) {
                s.never_forgive_paths.clear();
            }
            // complete_ready_from_store_batch does Substituting→
            // Completed (valid transition) + the full post-completion
            // machinery (output_paths, persist, upsert_path_tenants,
            // promote_newly_ready, per-build events + completion check).
            self.complete_ready_from_store_batch(std::slice::from_ref(drv_hash))
                .await;
            // r[impl sched.dispatch.substitute-complete-inline+2]
            // promote_newly_ready pushed dependents to ready_queue at
            // probed_generation=0. Probe inline so a fully-substitutable
            // cascade doesn't wait one Tick per layer.
            // `probed_generation` stamping bounds the cost of a
            // fresh-cluster substitution burst: nodes already probed
            // this Tick are skipped, so repeated inline sweeps only ever
            // probe newly-promoted dependents.
            // `r[sched.admin.spawn-intents.probed-gate]` still
            // suppresses spurious intents for the not-yet-probed tail.
            self.sweep_ready_cached().await;
            return;
        }
        // r[impl sched.merge.substitute-topdown+10]
        // Topdown-pruned root: the dep subgraph was dropped from this
        // submission, so a build dispatch cannot succeed (worker
        // ENOENTs on inputDrvs). Fail every interested build with a
        // resubmit-directing error instead of demoting to Ready
        // (vacuously all_deps_completed → would dispatch). keep_going
        // is irrelevant: a topdown-pruned graph is roots-only by
        // construction; there is no other work to "keep going" with,
        // and leaving the build Active would hang it.
        //
        // The marked node's routing is keyed on its closure evidence
        // (`DerivationDag::closure_evidence`):
        //
        //  - Vouched (≥1 child, all produced, no closure hole): the
        //    "deps were dropped" invariant is moot — lazily clear the
        //    flag (memory + best-effort PG) and fall through to the
        //    normal revert. This backstops the stamp being conditional
        //    and the other clear sites (post-reconciliation,
        //    completion-time, recovery-time) having missed it.
        //  - Pending (children present but not all produced): a live
        //    flag alongside unbuilt children is normal (kept by
        //    design — they can still be reaped unbuilt). Suppress the
        //    fail-fast, keep the mark, and fall through to the normal
        //    Ready/Queued handling instead of collaterally failing a
        //    build whose node IS buildable once those children land.
        //  - Broken (childless or closure-holed): the child set must
        //    not vouch for a from-source dispatch — a closure-holed
        //    node's surviving children (left by a reap, a poison-clear
        //    removal, or a recovery edge-drop) are not representative
        //    of the pruned input closure, so neither the lazy clear nor
        //    the suppression may trust them. Take the fail-fast arm below
        //    (the bounded resubmit-directing outcome, never the doomed
        //    from-source dispatch a Ready revert would produce).
        //
        // The fail-fast arm additionally gates on
        // `!forgiven_now_wanted`: that downgrade means the fetch did
        // NOT definitively fail — it forgave a seed that has since
        // become wanted. Failing the build here would be premature;
        // fall through to the downgrade re-spawn below, which re-walks
        // immediately with the corrected forgivable set. A genuine
        // failure on that second walk lands back here with
        // `forgiven_now_wanted = false` and fails the build then.
        let evidence = self.dag.closure_evidence(drv_hash);
        if topdown_pruned && evidence == ClosureEvidence::Vouched {
            debug!(%drv_hash,
                   "topdown-pruned root's children are all produced; \
                    invariant moot, clearing flag (memory + PG)");
            if let Some(s) = self.dag.node_mut(drv_hash) {
                s.topdown_pruned = false;
            }
            // Best-effort PG counterpart of the in-memory clear:
            // this lazy clear backstops the children-produced-later
            // case (the completion-time clear in
            // `clear_topdown_pruned_for_produced_parents` is the
            // primary site), and the column is what a failover
            // restores — left set, a new leader would resurrect the
            // mark onto a node whose closure IS produced and a
            // later walk failure would wrongly fail-fast. Same
            // error posture as the fail-fast's clear: warn and
            // continue, never fail the handler.
            if let Err(e) = self.db.clear_topdown_pruned_by_hash(drv_hash).await {
                warn!(%drv_hash, error = %e,
                      "failed to clear persisted topdown_pruned after lazy clear (continuing)");
            }
        } else if topdown_pruned && evidence == ClosureEvidence::Pending {
            debug!(%drv_hash,
                   "topdown-pruned root has unbuilt DAG children; \
                    suppressing the fail-fast but keeping the mark \
                    (children can still be reaped unbuilt)");
        } else if topdown_pruned && !forgiven_now_wanted {
            self.fail_fast_topdown_pruned_root(
                drv_hash,
                "upstream substitute fetch failed after deps were pruned",
            )
            .await;
            return;
        }
        // 3-way revert (NOT 2-way Ready|Queued): the I-094 reprobe lane
        // can transition a node whose dep is Poisoned directly →
        // Substituting; on fetch-fail it must go DependencyFailed, not
        // Queued (Queued with a Poisoned dep is stuck forever — see
        // `revert_target_for`).
        let to = self.dag.revert_target_for(drv_hash);
        // Must-substitute judgment for the downgrade re-spawn's topdown
        // leg below: the flag now survives merges that add unbuilt
        // children, so "flagged" no longer implies "childless" — the
        // doomed-from-source shape this leg must catch is a marked node
        // whose closure evidence is Broken (childless OR closure-holed,
        // same predicate as the dispatch guards). A genuine failure on
        // such a node takes the fail-fast arm before reaching here;
        // this leg only matters for the forgiven-now-wanted downgrade.
        // Evaluated before `node_mut` because the `&mut` borrow is held
        // at the use site.
        let must_sub = self.must_substitute(drv_hash);
        let Some(state) = self.dag.node_mut(drv_hash) else {
            return;
        };
        // r[impl sched.merge.wanted-outputs+2]
        // Downgrade re-spawn: two revert targets must not wait for
        // "the next pass" to re-substitute the delta.
        //
        //  - `DependencyFailed` (a dep is Poisoned — the I-094 lane):
        //    the arm below runs `terminal_failure_epilogue`, terminally
        //    failing every interested build — including the build whose
        //    wanted subset WAS fully fetched and the build whose newly
        //    wanted seed never had a single real attempt (forgivable
        //    seeds are forgiven on their first failure). There is no
        //    "next pass" out of DependencyFailed.
        //  - a topdown-pruned root with broken closure evidence
        //    (childless or closure-holed — `must_substitute`): its dep
        //    closure was dropped from the submission and the current
        //    child set cannot vouch for it, so plain Ready without
        //    `substitute_tried` is a doomed from-source dispatch
        //    (worker ENOENTs on inputDrvs) if the next probe finds the
        //    now-wanted path definitively missing — the exact hazard
        //    the fail-fast arm above exists to prevent.
        //
        // For both, re-spawn the walk NOW: `spawn_substitute_fetches`
        // recomputes the forgivable set from the CURRENT effective
        // wanted set, so the newly-wanted delta gets a real,
        // retry-laddered attempt before any terminal verdict. If that
        // walk genuinely fails it lands back here with
        // `forgiven_now_wanted = false` and takes the normal arms
        // (fail-fast / epilogue). Terminates: every downgrade first
        // adds its trigger paths to `never_forgive_paths` (above), and
        // a path in that set is excluded from every later walk's
        // forgivable set within this chain — so it can never be
        // reported forgiven (and trigger another downgrade) again,
        // regardless of how the live effective wanted set shrinks (a
        // build goes terminal) and re-grows (a new build merges)
        // between walks. The trigger paths were forgivable, hence not
        // yet in the set, so within one chain the set only grows
        // between re-spawns and the chain takes at most |declared
        // outputs| downgrades. The set does NOT persist past the
        // chain: it is cleared at every chain ending (the ok=true
        // completion, the genuine-failure revert below, the topdown
        // fail-fast above, a worker-build verdict after a from-source
        // routing, any non-substitution completion — inline
        // store-batch, merge-time cached-hit, CA-cutoff Skip — or the
        // node being reset/removed, including the stale-Completed
        // reset that opens the next chain) — only this re-spawn and
        // the deferred next-pass delta re-walk retain it. A NEW chain
        // requires an external event (a later dispatch-pass or
        // merge-time classification spawning a fresh walk), which
        // re-establishes the same per-chain bound rather than
        // re-opening this one.
        //
        // The in-memory revert to `to` only satisfies the transition
        // table (there is no Substituting→Substituting); PG keeps
        // `Substituting`, which the re-spawn re-persists (one nuance:
        // for the DependencyFailed/Poisoned-origin arm the re-spawn's
        // best-effort `clear_poison` transiently writes status
        // 'created' to PG before that re-persist lands). The ordinary
        // Ready/Queued downgrade keeps today's behaviour (revert; the
        // next pass re-probes and re-substitutes — and a from-source
        // dispatch there is safe because the node's deps ARE in the
        // DAG and Completed). Falls through to that plain revert if the
        // store/self handles are gone (shutdown) — pre-fix behaviour.
        if forgiven_now_wanted
            && (to == DerivationStatus::DependencyFailed || must_sub)
            && !state.output_paths.is_empty()
            && self.store_client.is_some()
            && self.self_tx.is_some()
        {
            if let Err(e) = state.transition(to) {
                warn!(%drv_hash, %e, "SubstituteComplete downgrade: revert-for-respawn rejected");
                return;
            }
            let paths = state.output_paths.clone();
            info!(%drv_hash, revert_target = ?to,
                  "downgraded substitute completion would land in a \
                   terminal/fail-fast arm; re-spawning the walk with the \
                   corrected forgivable set");
            let auth = self.probe_substitute_auth(std::iter::once(drv_hash));
            self.spawn_substitute_fetches(vec![(drv_hash.clone(), paths)], auth)
                .await;
            return;
        }
        // One-shot fall-through: FMP said substitutable, QPI said no.
        // Next dispatch pass skips substitution and routes to a worker
        // — without this the partition re-includes it every Tick (~1/s
        // livelock; never reaches `find_executor`).
        //
        // EXCEPT for the forgiven-seed-became-wanted downgrade: there
        // the fetch did not definitively fail a wanted path (it never
        // tried the now-wanted one as wanted), so the next pass MUST
        // re-substitute with the corrected forgivable set. Bounded:
        // the trigger path is in `never_forgive_paths`, so that second
        // walk either succeeds or fails the now-unforgivable seed →
        // lands back here with `forgiven_now_wanted = false` → sets the
        // one-shot flag. (The hazardous DependencyFailed /
        // topdown-must-substitute targets never reach here — they
        // re-spawned above.)
        if !forgiven_now_wanted {
            state.substitute_tried = true;
        }
        // Chain-scope bookkeeping: the only way this chain continues
        // past this arm is that deferred delta re-substitution — the
        // downgrade reverting to Ready/Queued WITHOUT the one-shot
        // flag, so the next pass re-walks and `never_forgive_paths`
        // must survive into that walk. Every other way out ends the
        // chain (a genuine failure routes to a from-source build via
        // the one-shot flag; DependencyFailed is terminal), so the
        // spent-forgiveness set is cleared: leaking it into a LATER
        // substitution chain for this node (a stale-Completed reset, a
        // resubmit re-probe) would veto forgiving a path no live build
        // wants any more and turn a fully-substitutable node into a
        // from-source dispatch.
        if !(forgiven_now_wanted
            && matches!(to, DerivationStatus::Ready | DerivationStatus::Queued))
        {
            state.never_forgive_paths.clear();
        }
        if let Err(e) = state.transition(to) {
            warn!(%drv_hash, %e, "SubstituteComplete fail: revert rejected");
            return;
        }
        self.persist_status(drv_hash, to, None).await;
        match to {
            DerivationStatus::Ready => {
                // Demoted to from-source: the controller's next
                // GetSpawnIntents poll sees the Ready node and spawns a
                // pull-mode pod for it (no store re-probe needed — the
                // substitute fetch just told us it is not available).
                self.push_ready(drv_hash.clone());
            }
            DerivationStatus::DependencyFailed => {
                // Cascade + per-build completion-check so the interested
                // build terminates instead of hanging Active.
                self.terminal_failure_epilogue(
                    drv_hash,
                    "substitute fetch failed and a dependency is terminally failed",
                    rio_proto::types::BuildResultStatus::DependencyFailed,
                    None,
                )
                .await;
            }
            _ => {}
        }
        // build_summary counts Substituting as running; revert flips
        // running→queued. Mirror `rollback_assignment` /
        // `emit_assignment_started` so the dashboard sees the demote.
        for build_id in self.get_interested_builds(drv_hash) {
            self.emit_progress(build_id);
        }
    }

    // r[impl sched.merge.substitute-topdown+10]
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
    /// Callers (all gate on `topdown_pruned` plus closure evidence the
    /// classifier judges `Broken` — childless or closure-holed, the
    /// `must_substitute` predicate — so a reap-truncated child set is
    /// treated exactly like an empty one at every site; unbuilt
    /// children (`Pending`) suppress the fail-fast but no longer shed
    /// the mark; only a `Vouched` child set — all produced, no hole —
    /// clears it):
    ///  - `handle_substitute_complete` on a failed/downgraded detached
    ///    fetch (`SubstituteComplete{ok=false}` after the
    ///    closure-evidence gate), the original home of this block;
    ///  - the dispatch-time probes (`batch_probe_cached_ready`,
    ///    `ready_check_or_spawn`) when a marked node with Broken
    ///    evidence can neither complete inline nor be routed to
    ///    substitution — the post-failover shape, where the recovered
    ///    (wider) wanted union contains an output that is genuinely
    ///    missing upstream. Pre-fix that outcome left the node Ready
    ///    and dispatched it from source — the doomed dispatch this arm
    ///    exists to prevent;
    ///  - the reap-time survivor re-evaluation in
    ///    `handle_cleanup_terminal_build` (leader-gated), which
    ///    additionally requires `substitute_tried` (the node's own walk
    ///    already failed) and a non-`Substituting` status (a walk in
    ///    flight keeps its chance) for a survivor the reap left
    ///    childless or closure-holed — nothing children-keyed would
    ///    ever re-evaluate it otherwise (`find_newly_ready` only fires
    ///    on completions).
    pub(super) async fn fail_fast_topdown_pruned_root(&mut self, drv_hash: &DrvHash, cause: &str) {
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
        // SubstituteComplete{ok=false} caller never trips this: it only
        // reaches the helper for a node it just observed Substituting
        // with live interest.)
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
        let parked = if let Some(s) = self.dag.node_mut(drv_hash) {
            s.substitute_tried = true;
            // The chain ends here (terminal fail-fast): drop the
            // chain-scoped spent-forgiveness bookkeeping so it
            // cannot leak into a later substitution chain for this
            // node (e.g. after a resubmit re-probes it).
            s.never_forgive_paths.clear();
            // The fail-fast CONSUMES the pruned marker (here and, best-
            // effort, in PG below). The flag can be stale: a merge
            // transaction that committed but whose build activation
            // failed leaves it on shared rows; a node stamped while its
            // existing children were invisible (recovery drops edges to
            // completed children) reads as "childless" again after the
            // next failover; and a genuinely pruned leaf is never
            // cleared by a children-adding merge. Left in place, a
            // stale flag re-arms this fail-fast after EVERY failover
            // and wrongfully terminal-fails builds for a node that
            // could build from source. Clearing loses nothing: after
            // the park there is no surviving interest, and a
            // resubmitted genuinely-pruned root either re-prunes
            // (re-stamped — the retained breadcrumb below keeps a
            // reap-truncated survivor set from vouching) or full-merges
            // (children all produced ⇒ cleared).
            s.topdown_pruned = false;
            // Deliberately do NOT clear `closure_hole`: the directed
            // resubmit this fail-fast solicits goes through the
            // resubmit-reset, which keeps the (possibly truncated)
            // child edges and carries the breadcrumb, and the
            // re-pruning merge's stamp gates need it to avoid reading
            // produced survivors as Vouched — erasing it here would
            // launder that resubmit into the doomed from-source
            // dispatch one lifecycle step later (round-23 bug_006). An
            // unmarked node's hole has no consumer that can mis-fire
            // (every consumer also requires the mark); a later full
            // merge that re-declares the edges heals it.
            if let Err(e) = s.transition(DerivationStatus::Queued) {
                warn!(%drv_hash, %e, "topdown fail-fast: transition to Queued rejected");
            }
            true
        } else {
            false
        };
        // Persist + PG flag clear only for a node the block above
        // actually parked — never for one the early return skipped or
        // that vanished between the check and the mutation.
        if parked {
            self.persist_status(drv_hash, DerivationStatus::Queued, None)
                .await;
            // Best-effort PG counterpart of the in-memory mark clear
            // above (mark-only: the persisted closure_hole breadcrumb
            // is deliberately left set, mirroring the retention above).
            // A failure costs at most one more wrongful fail-fast cycle
            // after a later failover — never fail the actor command
            // over it.
            if let Err(e) = self.db.clear_topdown_pruned_by_hash(drv_hash).await {
                warn!(%drv_hash, error = %e,
                      "failed to clear persisted topdown_pruned after fail-fast (continuing)");
            }
        }
        for build_id in self.get_interested_builds(drv_hash) {
            if let Some(build) = self.builds.get_mut(&build_id) {
                build.error_summary.get_or_insert_with(|| msg.clone());
                build
                    .failed_derivation
                    .get_or_insert_with(|| drv_hash.to_string());
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

    // r[impl gw.activity.subst-progress]
    /// Relay byte-progress from a detached substitute fetch to every
    /// interested build via [`Event::SubstituteProgress`]. Display-only
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
    async fn complete_ready_from_store_batch(&mut self, hashes: &[DrvHash]) {
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
            // An inline store completion ends any substitution chain
            // that left this node Ready (the forgiven-now-wanted
            // downgrade reverts to Ready for a delta re-walk; if the
            // wanting build goes terminal first, the next pass lands
            // here instead of re-walking). The spent-forgiveness set is
            // chain-scoped — clear it so a LATER chain (e.g. a stale-
            // Completed reset after GC) starts with a clean slate
            // instead of vetoing forgiveness of a path no live build
            // wants any more.
            state.never_forgive_paths.clear();
            // IA-only convenience: `expected_output_paths` IS the
            // realised path. Non-destructive when a path is already
            // known — the floating-CA reprobe→re-substitute lane
            // arrives here with `output_paths` set to the realized
            // path by `spawn_substitute_fetches`; clobbering it with
            // `expected_output_paths == [""]` would drop GC retention
            // and emit `[""]` to clients.
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
        // hashes, transition + push_ready in-mem, then one
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
                    newly_ready.push(ready_hash.clone());
                    self.push_ready(ready_hash);
                }
            }
        }
        let ready_refs: Vec<&str> = newly_ready.iter().map(|h| h.as_str()).collect();
        self.persist_status_batch(&ready_refs, DerivationStatus::Ready)
            .await;

        // Same completion-time topdown_pruned re-evaluation as the
        // worker path (`promote_newly_ready_batch`): substitution
        // success and dispatch-time store hits also turn parents'
        // children produced, and this path promotes through its own
        // inline loop above instead of that helper.
        self.clear_topdown_pruned_for_produced_parents(&ok_hashes)
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
            if let Some(b) = self.builds.get_mut(&build_id) {
                b.cached_count += n;
            }
            // I-140: one build_summary scan shared, not two.
            let summary = self.dag.build_summary(build_id);
            self.update_build_counts_with(build_id, &summary).await;
            self.events.emit_progress_with(build_id, &summary);
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

    // -----------------------------------------------------------------------
    // Queue priority helpers
    // -----------------------------------------------------------------------
    //
    // Pure DAG lookup helpers (`path_for_hash`, `hash_for_path`,
    // `path_or_hash_fallback`, `db_id_for_path`) live on
    // [`crate::dag::DerivationDag`]; the helpers below stay on
    // `DagActor` because they cross-reference `self.builds`.

    /// Compute the effective queue priority for a derivation: its
    /// critical-path priority + interactive boost if applicable.
    ///
    /// All queue pushes go through this. Replaces the old `push_front`/
    /// `push_back` split — interactive is now a number, not a position.
    ///
    /// Returns 0.0 if the node isn't in the DAG (stale hash). The
    /// caller probably shouldn't be pushing it, but 0.0 = lowest
    /// priority = harmless (stale entries get skipped on pop anyway
    /// if status != Ready).
    pub(super) fn queue_priority(&self, drv_hash: &DrvHash) -> f64 {
        let base = self
            .dag
            .node(drv_hash)
            .map(|n| n.sched.priority)
            .unwrap_or(0.0);
        // Any interested build is interactive (IFD) → priority boost
        // dwarfing any critical-path value.
        let interactive = self.get_interested_builds(drv_hash).iter().any(|id| {
            self.builds
                .get(id)
                .is_some_and(|b| b.priority_class.is_interactive())
        });
        if interactive {
            base + crate::queue::INTERACTIVE_BOOST
        } else {
            base
        }
    }

    /// Push a derivation onto the ready queue with its computed priority.
    /// Centralizes the priority lookup so call sites are simple.
    pub(super) fn push_ready(&mut self, drv_hash: DrvHash) {
        let prio = self.queue_priority(&drv_hash);
        self.ready_queue.push(drv_hash, prio);
    }
}

// r[impl sched.substitute.detached+5]
/// Auth source for the detached substitute closure walk.
///
/// `Service` holds `(signer, tenant_id)` and re-mints a fresh
/// `SUBSTITUTE_FETCH_TIMEOUT`-expiry token on every `mint()` so a long
/// closure walk or time parked on `substitute_sem` can't outlive the
/// token (a 60s spawn-time token expired mid-walk → later QPIs
/// `NotFound` → spurious `ok=false`). Both merge-time and dispatch-time
/// callers now use `Service` (via `substitute_auth_for_tenant`): the
/// gateway JWT is single-shot and a wide cold closure can outlive its
/// ~65 min expiry. `Jwt` remains only for the dev/no-signer/no-tenant
/// fallback (`vec![]` — no-auth) where re-minting is meaningless.
#[derive(Clone)]
pub(super) enum SubstituteAuth {
    Jwt(Vec<(&'static str, String)>),
    Service {
        signer: Arc<rio_auth::hmac::HmacSigner>,
        tenant_id: Uuid,
    },
}

impl SubstituteAuth {
    pub(super) fn mint(&self) -> Vec<(&'static str, String)> {
        match self {
            SubstituteAuth::Jwt(m) => m.clone(),
            SubstituteAuth::Service { signer, tenant_id } => {
                let claims = rio_auth::hmac::ServiceClaims {
                    caller: "rio-scheduler".to_string(),
                    expiry_unix: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0)
                        + super::SUBSTITUTE_FETCH_TIMEOUT.as_secs(),
                };
                vec![
                    (rio_proto::SERVICE_TOKEN_HEADER, signer.sign(&claims)),
                    (rio_proto::PROBE_TENANT_ID_HEADER, tenant_id.to_string()),
                ]
            }
        }
    }
}

/// `reason` label values for `rio_scheduler_substitute_demotions_total`,
/// derived from the FINAL error that demoted a non-forgivable path
/// (attempts within one path's retry ladder can fail with different
/// codes; the last one is what the walk gave up on). A small fixed set
/// — never the raw store message or the path.
///
/// - `not_found`: the final attempt was a plain path-not-found — no
///   configured upstream produced the path, or the store skipped
///   substitution entirely (no upstreams configured for the tenant /
///   the replica's HTTP client failed at boot). The skip cases are
///   counted store-side in `rio_store_substitute_skipped_total`; the
///   bare message gives the scheduler no way to tell them apart from a
///   genuine all-upstreams miss.
/// - `not_found_infra`: the final attempt's NotFound message indicates
///   the request never reached an upstream (auth chain / substituter
///   config) — fix the infrastructure, the path is probably fine. The
///   substrings are pinned against rio-store's `substitute_path_impl`
///   refusal messages by `demotion_reason_classifies_store_messages`.
/// - `error`: a non-transient, non-NotFound gRPC error (no retry).
/// - `exhausted`: the retry budget ran out on a transient error.
fn demotion_reason(e: &tonic::Status) -> &'static str {
    match e.code() {
        tonic::Code::NotFound => {
            let m = e.message();
            if m.contains("no tenant context") || m.contains("substituter not configured") {
                "not_found_infra"
            } else {
                "not_found"
            }
        }
        c if rio_common::grpc::is_transient(c) => "exhausted",
        _ => "error",
    }
}

/// Bump the demotion counters for a non-forgivable path whose failure
/// is about to set `ok = false` — the derivation and its build-time
/// closure are about to be compiled from source because a download
/// failed. The caller emits the `error!` (the message differs per
/// arm); this owns the two counters so no demotion site can increment
/// one and forget the other.
fn record_demotion(e: &tonic::Status) {
    metrics::counter!("rio_scheduler_substitute_fetch_failures_total").increment(1);
    metrics::counter!(
        "rio_scheduler_substitute_demotions_total",
        "reason" => demotion_reason(e)
    )
    .increment(1);
}

/// `MAX_SUBSTITUTE_CLOSURE` overshoot guard for [`walk_substitute_closure`].
/// Re-checked after every per-path `references` push so a hostile
/// upstream can overshoot by at most one path's `MAX_REFERENCES` (10k),
/// not `layer_width × MAX_REFERENCES` (~100M strings before the next
/// top-of-loop check).
fn closure_cap_exceeded(visited: usize) -> bool {
    if visited > MAX_SUBSTITUTE_CLOSURE {
        warn!(
            visited,
            cap = MAX_SUBSTITUTE_CLOSURE,
            "substitute closure walk exceeded MAX_SUBSTITUTE_CLOSURE; \
             demoting to cache-miss (hostile upstream reference chain?)"
        );
        metrics::counter!("rio_scheduler_substitute_fetch_failures_total").increment(1);
        true
    } else {
        false
    }
}

/// BFS over `info.references` from `seeds`, issuing `QueryPathInfo`
/// (which triggers store-side `try_substitute`) for every node. Returns
/// `true` iff every node in the closure is present in the store on
/// return — the contract `handle_substitute_complete{ok=true}` relies
/// on for `Substituting → Completed`.
///
/// The store substitutes ONE path per call (no closure walk), so this
/// is the only place the runtime closure can be completed. Without it,
/// `compute_input_closure`'s `BatchQueryPathInfo` (local-only) drops a
/// transitive ref from the JIT allowlist → ENOENT at exec time (e.g.
/// `rustc-wrapper` execs `rustc-1.94.0`).
///
/// **Layer-batched fast-path:** each BFS layer is first probed via
/// `BatchQueryPathInfo` (local-only, one PG `= ANY()` round-trip — the
/// I-110 pattern from `compute_input_closure`). Refs already in PG
/// contribute their `references` to the next layer without a per-path
/// RPC; only the absent subset gets a substituting `QueryPathInfo`. For
/// an 800-path closure that's mostly warm, this is O(depth) ≈ 10 batch
/// RPCs instead of 800 unary ones. A `BatchQueryPathInfo` error falls
/// through to per-path QPI for the whole layer (correctness over
/// throughput).
///
/// A non-transient `Err`, a retry-exhausted path (NotFound and
/// transient errors share the backoff ladder — a NotFound inside the
/// walk contradicts the HEAD probe / narinfo that named the path, so
/// it is retried rather than believed on first occurrence), or the
/// `MAX_SUBSTITUTE_CLOSURE` cap → `false`. Self-references and
/// diamonds dedup via `visited`.
///
/// **`forgivable`** is the subset of `seeds` whose failure must NOT
/// fail the walk: the declared-but-unwanted output paths (no consumer's
/// `inputDrvs` names them and the root didn't ask for them). The seeds
/// stay ALL expected output paths — opportunistic completeness: fetch
/// the `-debug` output if the upstream has it — but when the upstream
/// definitively misses one, condemning the derivation (and its whole
/// build-time closure) to a from-source rebuild over an output nothing
/// consumes is the incident this gate exists to prevent. A failed
/// WANTED seed and a failed reference-BFS-discovered path (a runtime
/// reference of something already fetched — its absence is a hole in a
/// closure we are about to declare complete) keep failing the walk;
/// only unwanted seeds are in `forgivable` by construction. A
/// forgivable seed is forgiven on its FIRST failure of any kind — it
/// does not consume the retry budget (the per-path loop is serial, so
/// ~32 s of backoff on an output nobody consumes delays every path
/// behind it).
///
/// **Residual hole risk.** If a WANTED output runtime-references an
/// unwanted sibling output, forgiving that sibling can leave a hole in
/// the wanted output's runtime closure even though the walk returns
/// `ok=true`. The no-hole guarantee only holds when the forgiven
/// error is a **NotFound**, and only against an upstream that
/// maintains its closure invariant: an upstream that served the wanted
/// output has every path in that output's closure, so a true miss on
/// the sibling proves the wanted output does not reference it. A
/// forgiven **transient or non-transient error** carries no such proof
/// — the upstream may well have the sibling (a 500, a timeout, a flaky
/// connection) and the walk still completes with the reference
/// unsatisfied; first-failure forgiveness widens this slightly (a
/// transient blip that one retry would have cleared is now forgiven
/// instead of retried). Accepted
/// because (a) it requires the rare wanted→unwanted-sibling reference
/// direction (`-debug`/`-doc` outputs reference their `out`, not the
/// reverse), and (b) the alternative — failing the walk — is a
/// GUARANTEED from-source rebuild of the derivation and its entire
/// build-time closure, versus a POSSIBLE FUSE ENOENT on one path in
/// one dependent's build, whose retry re-queries the path and
/// re-triggers substitution.
///
/// Returns `(ok, forgiven)`: `forgiven` is the subset of `forgivable`
/// seeds that actually FAILED and were forgiven (not the ones that
/// substituted fine). `forgivable` is a snapshot of the wanted set at
/// spawn time; a build that merges during the (potentially minutes-
/// long) walk can grow the node's wanted set to include a forgiven
/// seed. `handle_substitute_complete` re-checks `forgiven` against the
/// node's CURRENT wanted set and downgrades a stale `ok=true` to a
/// revert.
pub(super) async fn walk_substitute_closure(
    store: &rio_proto::store::store_service_client::StoreServiceClient<tonic::transport::Channel>,
    seeds: Vec<String>,
    forgivable: &HashSet<String>,
    auth: &SubstituteAuth,
    shutdown: &rio_common::signal::Token,
    mut on_progress: impl FnMut(u64, u64, &str),
) -> (bool, Vec<String>) {
    let mut visited: HashSet<String> = seeds.iter().cloned().collect();
    let mut frontier: VecDeque<String> = seeds.into_iter().collect();
    let mut ok = true;
    let mut forgiven: Vec<String> = Vec::new();
    // Aggregate across the closure for `r[gw.activity.subst-progress]`.
    // `done_base` accumulates completed-path nar_sizes; the per-path
    // callback adds its in-flight `done` on top. `expected` grows as
    // each path's narinfo is read (first progress emit OR Ok(Some)).
    // `seen_expected` tracks which paths have already contributed to
    // `expected` so a retry after a transient error doesn't double-count.
    //
    // `expected_total` grows as the BFS discovers references — nom's
    // denominator increases mid-stream. Inherent to walk-and-discover
    // (we don't pre-fetch all narinfos), not a bug; the invariants
    // below keep it from rendering as >100%.
    let mut done_base: u64 = 0;
    let mut expected_total: u64 = 0;
    let mut seen_expected: HashSet<String> = HashSet::new();
    while !frontier.is_empty() {
        if shutdown.is_cancelled() {
            return (false, forgiven);
        }
        if closure_cap_exceeded(visited.len()) {
            return (false, forgiven);
        }
        // Re-mint per layer: dispatch-time `Service` tokens are
        // short-lived; minting once at spawn meant a ghc-sized walk or
        // time on `substitute_sem` outlived the token. `Jwt` is a
        // cheap clone. Only the per-path QPIs need tenant context (for
        // store-side `try_substitute_on_miss → tenant_upstreams`).
        let mut meta_owned = auth.mint();
        let mut paths_since_mint = 0usize;
        let mut mint_at = Instant::now();
        // Layer-batched fast-path: drain the current frontier into one
        // BatchQueryPathInfo. Present refs (warm in PG) push their
        // references; absent refs go to per-path QPI to trigger
        // substitution. Batch error → treat the whole layer as absent
        // (per-path QPI then handles each, including retry).
        //
        // `&[]` metadata: BatchQPI is local-only and rejects end-user
        // tenant JWTs (`reject_end_user_tenant`); the merge-time `Jwt`
        // path carries one, so passing `meta` here →
        // `PermissionDenied` → debug! + per-path fallback (fast-path
        // never fired). The call needs no tenant.
        let layer: Vec<String> = frontier.drain(..).collect();
        let mut absent: Vec<String> = Vec::new();
        let mut c = store.clone();
        match rio_proto::client::batch_query_path_info(
            &mut c,
            layer.clone(),
            super::SUBSTITUTE_FETCH_TIMEOUT,
            &[],
        )
        .await
        {
            Ok(entries) => {
                for (path, info) in entries {
                    match info {
                        Some(info) => {
                            for r in &info.references {
                                if visited.insert(r.to_string()) {
                                    frontier.push_back(r.to_string());
                                }
                            }
                            if closure_cap_exceeded(visited.len()) {
                                return (false, forgiven);
                            }
                        }
                        None => absent.push(path),
                    }
                }
            }
            Err(e) => {
                debug!(error = %e, layer = layer.len(),
                       "substitute closure: batch probe failed; \
                        falling through to per-path QPI");
                absent = layer;
            }
        }
        // Per-path QPI for the absent subset — triggers store-side
        // try_substitute. Same retry/error handling as before; on
        // success, push references for the next layer.
        'paths: for p in absent {
            // Per-path high-water mark for the in-flight `done`.
            // `do_substitute` may iterate upstreams (or the outer
            // attempt loop may retry), each restarting at `done=0`;
            // emitting raw `done` would make the bar jump backward.
            // The hwm makes the per-path contribution monotone.
            let mut in_flight_hwm: u64 = 0;
            // Final error of the retry ladder, for the post-loop
            // exhaustion arm: the demotion log and the
            // `substitute_demotions_total{reason}` label need to know
            // whether the budget was burned by NotFounds (the upstream
            // kept contradicting the probe) or by transient errors
            // (the store never answered).
            let mut last_err: Option<tonic::Status> = None;
            // Per-path re-mint cadence: the per-layer mint above is
            // not enough — this serial loop can run >30 min on a wide
            // cold layer (hundreds of paths × admission-wait + retry
            // backoff each), outliving a `Service` token. Re-mint
            // every `SUBSTITUTE_REMINT_PATHS` paths OR
            // `SUBSTITUTE_REMINT_INTERVAL` elapsed, whichever first.
            // Expired-token QPI surfaces as `NotFound`/
            // `Unauthenticated` (NON-transient) → spurious `ok=false`
            // → demote to build-from-source.
            if paths_since_mint >= super::SUBSTITUTE_REMINT_PATHS
                || mint_at.elapsed() >= super::SUBSTITUTE_REMINT_INTERVAL
            {
                meta_owned = auth.mint();
                paths_since_mint = 0;
                mint_at = Instant::now();
            }
            paths_since_mint += 1;
            let meta: Vec<(&'static str, &str)> =
                meta_owned.iter().map(|(k, v)| (*k, v.as_str())).collect();
            for attempt in 0..super::SUBSTITUTE_FETCH_MAX_ATTEMPTS {
                if shutdown.is_cancelled() {
                    return (false, forgiven);
                }
                let mut c = store.clone();
                // r[impl gw.activity.subst-progress]
                // Per-path progress: store streams (done, expected,
                // upstream) per ~1 MiB. First emit for `p` adds its
                // `expected` to the closure aggregate; subsequent emits
                // (and retries) only update the in-flight hwm on top of
                // `done_base`. `seen_expected` keys on path so a retry
                // after a transient error doesn't re-add `expected`.
                let path_progress = |done: u64, expected: u64, upstream: &str| {
                    if seen_expected.insert(p.clone()) {
                        expected_total = expected_total.saturating_add(expected);
                    }
                    in_flight_hwm = in_flight_hwm.max(done);
                    on_progress(
                        done_base.saturating_add(in_flight_hwm),
                        expected_total,
                        upstream,
                    );
                };
                match rio_proto::client::substitute_path_with_progress(
                    &mut c,
                    &p,
                    super::SUBSTITUTE_FETCH_TIMEOUT,
                    &meta,
                    path_progress,
                )
                .await
                {
                    Ok(info) => {
                        // Store-side cache-hit / AlreadyComplete returns
                        // here without ever calling `path_progress`, so
                        // `expected_total` must learn `nar_size` here too
                        // — otherwise `done_base` outgrows it and the
                        // next path's emit shows >100%.
                        if seen_expected.insert(p.clone()) {
                            expected_total = expected_total.saturating_add(info.nar_size);
                        }
                        done_base = done_base.saturating_add(info.nar_size);
                        for r in &info.references {
                            if visited.insert(r.to_string()) {
                                frontier.push_back(r.to_string());
                            }
                        }
                        if closure_cap_exceeded(visited.len()) {
                            return (false, forgiven);
                        }
                        continue 'paths;
                    }
                    // An unwanted seed is forgiven on its FIRST
                    // failure of ANY kind: not a failure (no metric),
                    // the walk continues, the closure is still
                    // complete for every output anything consumes.
                    // Checked before the retry ladder — burning ~32 s
                    // of serialized backoff on an output nobody
                    // consumes delays every path behind it in the
                    // layer. The path was still ATTEMPTED once
                    // (opportunistic completeness — it stays in the
                    // seed list). `store_msg` for the same reason as
                    // the fatal arms below: "no tenant context" /
                    // "substituter not configured" mean the request
                    // never reached the upstream — a forgiven skip
                    // that should have been a fetch.
                    Err(e) if forgivable.contains(&p) => {
                        info!(path = %p, code = ?e.code(), store_msg = e.message(),
                              "unwanted output not substituted; continuing without it");
                        forgiven.push(p.clone());
                        continue 'paths;
                    }
                    // A NotFound inside the walk is always a
                    // contradiction: every path here was either
                    // HEAD-probed as available minutes earlier (a
                    // seed) or named in a narinfo the upstream just
                    // served (a reference), so "the upstream doesn't
                    // have it" disagrees with an observation the
                    // store/upstream made moments ago. The genuinely-
                    // not-on-any-upstream case never enters the walk.
                    // Treat it like a transient error: retry through
                    // the same backoff ladder. The 2026-05 incident
                    // demoted 235 paths to from-source builds on
                    // first-occurrence NotFounds that the store never
                    // actually checked against the upstream; all 235
                    // substituted fine 80 seconds later.
                    Err(e)
                        if e.code() == tonic::Code::NotFound
                            || rio_common::grpc::is_transient(e.code()) =>
                    {
                        debug!(path = %p, attempt, code = ?e.code(), store_msg = e.message(),
                               "substitute fetch retryable error; retrying");
                        metrics::counter!("rio_scheduler_substitute_fetch_retries_total")
                            .increment(1);
                        let exhausted = attempt + 1 == super::SUBSTITUTE_FETCH_MAX_ATTEMPTS;
                        last_err = Some(e);
                        if exhausted {
                            break;
                        }
                        tokio::select! {
                            _ = shutdown.cancelled() => return (false, forgiven),
                            _ = tokio::time::sleep(
                                super::SUBSTITUTE_FETCH_BACKOFF.duration(attempt)
                            ) => {}
                        }
                    }
                    Err(e) => {
                        // Non-transient, non-NotFound: retrying won't
                        // change the answer. error! (not warn!) — this
                        // event means a derivation and its build-time
                        // closure are about to be compiled from source
                        // because a download failed; it should page.
                        error!(path = %p, error = %e, store_msg = e.message(),
                               reason = demotion_reason(&e),
                               "detached substitute fetch failed; demoting to cache-miss");
                        record_demotion(&e);
                        ok = false;
                        continue 'paths;
                    }
                }
            }
            // Retry ladder exhausted: every attempt failed with a
            // retryable error (NotFound or transient) on a
            // non-forgivable path — every other arm `continue 'paths`
            // out of the attempt loop, so `last_err` is always the
            // final attempt's error here. The consequence is "compile
            // the derivation (and its build closure) from source", so
            // the WHY must survive into the log: `store_msg` is
            // rio-store's own reason for the FINAL attempt — "no
            // tenant context on request" / "substituter not
            // configured" mean that request never reached
            // cache.nixos.org (fix the auth chain / config); a bare
            // "path not found" means no configured upstream produced
            // it — or the store skipped substitution entirely (no
            // upstreams configured for the tenant / no HTTP client on
            // the replica; rio_store_substitute_skipped_total carries
            // the store-side cause). Indistinguishable before
            // 2026-05-23.
            let store_msg = last_err
                .as_ref()
                .map(|e| e.message().to_owned())
                .unwrap_or_default();
            let reason = last_err.as_ref().map_or("exhausted", demotion_reason);
            error!(path = %p, attempts = super::SUBSTITUTE_FETCH_MAX_ATTEMPTS,
                   store_msg, reason,
                   "detached substitute fetch exhausted retries; demoting to cache-miss");
            match last_err {
                Some(e) => record_demotion(&e),
                // Unreachable in practice; keep the failure counted
                // rather than silently losing the demotion.
                None => {
                    metrics::counter!("rio_scheduler_substitute_fetch_failures_total").increment(1);
                    metrics::counter!(
                        "rio_scheduler_substitute_demotions_total",
                        "reason" => "exhausted"
                    )
                    .increment(1);
                }
            }
            ok = false;
        }
    }
    (ok, forgiven)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin `demotion_reason`'s message-substring classifier against the
    /// THREE NotFound shapes rio-store's `substitute_path_impl`
    /// actually produces (rio-store/src/grpc/queries.rs). The reason
    /// label is the only alertable signal that distinguishes "the
    /// upstream really missed" from "the request never reached the
    /// upstream"; if the store re-words a refusal message and the
    /// substring no longer matches, the infra case silently collapses
    /// into `not_found` — this test breaks instead.
    #[test]
    fn demotion_reason_classifies_store_messages() {
        let p = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x";
        for (status, want) in [
            // queries.rs: the no-substituter refusal.
            (
                tonic::Status::not_found(format!(
                    "path not found (substituter not configured on this store replica): {p}"
                )),
                "not_found_infra",
            ),
            // queries.rs: the no-tenant refusal.
            (
                tonic::Status::not_found(format!(
                    "path not found (no tenant context on request — substitution \
                     requires x-rio-tenant-token or x-rio-probe-tenant-id + \
                     x-rio-service-token): {p}"
                )),
                "not_found_infra",
            ),
            // queries.rs: the bare terminal. Reached on a genuine
            // all-upstreams miss, but ALSO when the store skipped
            // substitution entirely (no upstreams configured for the
            // tenant / no HTTP client on the replica) — same message
            // either way; the store-side cause is only visible in
            // rio_store_substitute_skipped_total.
            (
                tonic::Status::not_found(format!("path not found: {p}")),
                "not_found",
            ),
            // Transient code → the ladder ran out of budget.
            (
                tonic::Status::unavailable("connection refused"),
                "exhausted",
            ),
            // Non-transient, non-NotFound → no retry.
            (tonic::Status::internal("boom"), "error"),
        ] {
            assert_eq!(
                demotion_reason(&status),
                want,
                "code={:?} msg={:?}",
                status.code(),
                status.message()
            );
        }
    }
}
