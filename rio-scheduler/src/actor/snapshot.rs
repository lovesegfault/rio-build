//! Read-only snapshot/inspect handlers on [`DagActor`]. All methods
//! here are `&self` over the in-memory DAG/executors and back the admin
//! RPCs (ClusterStatus, GetSpawnIntents, InspectBuildDag,
//! DebugListExecutors).

use std::collections::HashMap;

use uuid::Uuid;

use crate::state::{BuildState, DerivationStatus, SolvedIntent};

use super::{
    AdminQuery, ClusterSnapshot, DagActor, DebugExecutorInfo, SpawnIntentsRequest,
    SpawnIntentsSnapshot, command,
};

impl DagActor {
    /// Dispatch a read-only [`AdminQuery`].
    pub(super) fn handle_admin(&self, q: AdminQuery) {
        match q {
            AdminQuery::GetSpawnIntents { req, reply } => {
                let _ = reply.send(self.compute_spawn_intents(&req));
            }
            AdminQuery::GcRoots { reply } => {
                let _ = reply.send(self.handle_gc_roots());
            }
            AdminQuery::ListExecutors { reply } => {
                let _ = reply.send(self.handle_list_executors());
            }
            AdminQuery::InspectBuildDag { build_id, reply } => {
                let _ = reply.send(self.handle_inspect_build_dag(build_id));
            }
            AdminQuery::DebugQueryWorkers { reply } => {
                let _ = reply.send(self.handle_debug_query_workers());
            }
            AdminQuery::SlaStatus { key, reply } => {
                // active_override: surface the matching ROW (not the
                // projected ResolvedTarget) so the CLI can show
                // id/created_by/expires_at. `resolve_row` is the SAME
                // filter+rank `resolve()` uses for dispatch — the
                // previous inline reimplementation omitted `cluster`
                // from the specificity rank and disagreed with dispatch
                // when a cluster-scoped and a newer global row both
                // matched.
                let rows = self.sla_estimator.overrides();
                let active = crate::sla::r#override::resolve_row(&key, &rows).cloned();
                let _ = reply.send((self.sla_estimator.cached(&key), active));
            }
            AdminQuery::SlaEvict { key, reply } => {
                let _ = reply.send(self.sla_estimator.evict(&key));
            }
            AdminQuery::SlaExplain { key, reply } => {
                let fit = self.sla_estimator.cached(&key);
                let override_ = self.sla_estimator.resolved_override(&key);
                let _ = reply.send(crate::sla::explain::explain(
                    &key,
                    fit.as_ref(),
                    &self.sla_tiers,
                    &self.sla_ceilings,
                    override_.as_ref(),
                ));
            }
            AdminQuery::SlaExportCorpus {
                tenant,
                min_n,
                reply,
            } => {
                let _ = reply.send(self.sla_estimator.export_corpus(tenant.as_deref(), min_n));
            }
            AdminQuery::SlaImportCorpus { corpus, reply } => {
                let _ = reply.send(self.sla_estimator.import_seed(corpus));
            }
        }
    }

    /// Compute counts for `AdminService.ClusterStatus`.
    ///
    /// O(workers + builds + dag_nodes) per call. The autoscaler polls
    /// every 30s; even with 10k active derivations that's ~300μs/call —
    /// not worth maintaining incremental counters. Revisit if dashboards
    /// start polling at 1Hz.
    ///
    /// `as u32` casts: if any collection exceeds 4B entries, truncation
    /// is the LEAST of our problems. The `ready_queue.len()` is bounded
    /// by `ACTOR_CHANNEL_CAPACITY × derivations_per_submit` anyway (you
    /// can't enqueue what you can't merge).
    pub(super) fn compute_cluster_snapshot(&self) -> ClusterSnapshot {
        let mut active_executors = 0u32;
        let mut draining_executors = 0u32;
        // Single pass: registered ∧ ¬draining → active. draining →
        // draining (regardless of registered — a draining worker that
        // lost its stream mid-drain is still "draining" for the
        // controller's "how many pods are shutting down" question).
        for w in self.executors.values() {
            if w.is_draining() {
                draining_executors += 1;
            } else if w.is_registered() {
                active_executors += 1;
            }
        }

        let mut pending_builds = 0u32;
        let mut active_builds = 0u32;
        for b in self.builds.values() {
            match b.state() {
                BuildState::Pending => pending_builds += 1,
                BuildState::Active => active_builds += 1,
                // Terminal builds stay in the map until CleanupTerminalBuild
                // (delayed ~30s). Don't count them — they're not "active"
                // in any autoscaling sense. Unspecified never appears
                // (proto3 default-0; scheduler always sets a real state).
                BuildState::Succeeded
                | BuildState::Failed
                | BuildState::Cancelled
                | BuildState::Unspecified => {}
            }
        }

        // Running = Assigned | Running. Both mean "a worker slot is taken."
        // Assigned hasn't acked yet but the slot is reserved; for "how
        // busy are workers" they're equivalent.
        let mut running_derivations = 0u32;
        let mut substituting_derivations = 0u32;
        let mut queued_by_system: HashMap<String, u32> = HashMap::new();
        // r[impl sched.admin.snapshot-substituting]
        // Exhaustive over DerivationStatus so a future variant addition
        // is a compile-time break here, not a silently-zero autoscaler
        // input. The `_ => {}` this replaces dropped Substituting on
        // the floor — substitution cascades read as `builders=0` to
        // the ComponentScaler and the store scaled DOWN exactly when
        // it was the bottleneck.
        for (_, s) in self.dag.iter_nodes() {
            match s.status() {
                DerivationStatus::Assigned | DerivationStatus::Running => {
                    running_derivations += 1;
                }
                DerivationStatus::Ready => {
                    // I-107: per-system queued breakdown so per-arch
                    // Pools scale on their own backlog. Ready-only
                    // to match `queued_derivations` (= ready_queue.len())
                    // semantics — sum across keys equals the scalar.
                    *queued_by_system.entry(s.system.clone()).or_default() += 1;
                }
                // r[impl ctrl.scaler.signal-substituting]
                DerivationStatus::Substituting => substituting_derivations += 1,
                // Pre-ready: not yet store/builder load. Created has no
                // deps probed; Queued has unmet deps. Neither drives
                // any RPC traffic.
                DerivationStatus::Created | DerivationStatus::Queued => {}
                // Terminal (or transient-mid-retry for Failed): no
                // ongoing load.
                DerivationStatus::Completed
                | DerivationStatus::Failed
                | DerivationStatus::Poisoned
                | DerivationStatus::DependencyFailed
                | DerivationStatus::Cancelled
                | DerivationStatus::Skipped => {}
            }
        }

        ClusterSnapshot {
            total_executors: self.executors.len() as u32,
            active_executors,
            draining_executors,
            pending_builds,
            active_builds,
            queued_derivations: self.ready_queue.len() as u32,
            running_derivations,
            substituting_derivations,
            queued_by_system,
        }
    }

    /// Compute the flat per-derivation spawn-intent stream for
    /// `AdminService.GetSpawnIntents` (D5).
    ///
    /// Single `iter_nodes()` pass: for each Ready derivation that
    /// passes the request filter, run `solve_intent_for` and push one
    /// `SpawnIntent`. FODs and non-FODs go through the SAME path (D2)
    /// — `intent.kind` carries the ADR-019 boundary so the controller
    /// can filter per-pool.
    ///
    /// O(dag_nodes) per call. Same cost order as
    /// [`compute_cluster_snapshot`]; the autoscaler polls every ~10s so
    /// even 10k Ready derivations is sub-ms.
    ///
    /// `queued_by_system` is populated regardless of the
    /// kind/feature filters (it's the same population as
    /// `ClusterSnapshot.queued_by_system`) so the ComponentScaler reads
    /// a coherent snapshot from the same RPC.
    ///
    /// [`compute_cluster_snapshot`]: Self::compute_cluster_snapshot
    // pub(crate) for the feature-filter tests (tests/misc.rs) which
    // exercise it on a bare (unspawned) actor.
    pub(crate) fn compute_spawn_intents(&self, req: &SpawnIntentsRequest) -> SpawnIntentsSnapshot {
        let mut intents = Vec::new();
        let mut queued_by_system: HashMap<String, u64> = HashMap::new();
        let probe_gate = self.store_client.is_some();

        for (drv_hash, state) in self.dag.iter_nodes() {
            if state.status() != DerivationStatus::Ready {
                continue;
            }
            // Per-system aggregate: counted BEFORE the kind/feature
            // filters so it matches `ClusterSnapshot.queued_by_system`
            // (the ComponentScaler reads this independent of which
            // pool asked).
            *queued_by_system.entry(state.system.clone()).or_default() += 1;

            // r[impl sched.admin.spawn-intents.probed-gate+2]
            // SubstituteComplete{ok=true} promotes dependents
            // Queued→Ready then defers their probe to next Tick. A
            // poll in that ≤1s window would spawn pods that get
            // reaped 10s later when the probe finds them
            // substitutable. probed_generation==0 ⇔ "never probed
            // since insert/recovery"; the inline-dispatch carve-out
            // in `handle_substitute_complete` keeps this window
            // narrow (≤BECAME_IDLE_INLINE_CAP per Tick fall-through).
            // The gate is moot when there is no store (test-only;
            // `batch_probe_cached_ready` early-returns without
            // stamping) or when the node is unprobeable (floating-CA
            // / no expected_output_paths — probe never stamps it).
            if probe_gate && state.probed_generation == 0 && state.output_paths_probeable() {
                continue;
            }

            let kind = crate::state::kind_for_drv(state.is_fixed_output);
            // r[impl sched.admin.spawn-intents.feature-filter]
            // ── request filter ────────────────────────────────────
            // kind: Unknown (None) = unfiltered. Otherwise must match
            // — the ADR-019 airgap boundary (FOD ⇔ Fetcher) means a
            // Builder pool never sees a Fetcher intent and vice-versa.
            if req.kind.is_some_and(|k| k != kind) {
                continue;
            }
            // systems: empty = unfiltered. I-107/I-143 per-arch
            // intersection so an x86-64 pool doesn't spawn for an
            // aarch64-only backlog.
            if !req.systems.is_empty() && !req.systems.iter().any(|s| s == &state.system) {
                continue;
            }
            // features: I-176 subset check + I-181 ∅-guard. `None` =
            // unfiltered (CLI, status display). `Some([])` = a
            // featureless pool — only emits intents with empty
            // `required_features`. `Some(pf)` with `pf ≠ ∅` = a
            // feature-gated pool — emits intents whose
            // `required_features ⊆ pf` AND `required_features ≠ ∅`
            // (∅-feature work belongs to the featureless pool;
            // dispatch's overflow walk tries cheapest first, so a
            // kvm builder spawned for ∅-feature work would idle until
            // activeDeadlineSeconds).
            if let Some(pf) = req.features.as_deref() {
                if !pf.is_empty() && state.required_features.is_empty() {
                    continue;
                }
                if !state.required_features.iter().all(|f| pf.contains(f)) {
                    continue;
                }
            }

            // r[impl sched.sla.intent-from-solve]
            // ADR-023: per-derivation SpawnIntent. intent_id is the
            // drv_hash itself — the controller stamps it on the pod
            // annotation, the builder echoes it on heartbeat, dispatch
            // matches `worker.intent_id == drv_hash`. No separate
            // intent→drv map to keep in sync; if the drv leaves Ready
            // before the pod heartbeats, the match misses and dispatch
            // falls through to pick-from-queue.
            let intent = self.solve_intent_for(state);
            // ADR-023 §2.8 selector pin: if a Pending-watch entry
            // already exists for this drv (controller acked a spawn),
            // REUSE its `(band, cap)` for the returned selector instead
            // of the freshly-solved one. `solve_full`'s softmax re-rolls
            // on every poll (default temp=0.3 → ~15% per-tick flip);
            // without the pin the controller's `reap_stale_for_intents`
            // sees fingerprint drift and reap-respawns a still-Pending
            // Job each tick. The ICE-timeout sweep DROPS the entry, so
            // a deliberate re-solve (excluding the marked cell) flows
            // through unpinned and the reaper correctly replaces the
            // stale Pending Job.
            //
            // Read-only: this method NEVER writes `pending_intents`
            // (the timer is armed by `handle_ack_spawned_intents` only
            // for intents the controller actually created Jobs for —
            // see that method for why arm-on-emit was wrong).
            let node_selector = self
                .pending_intents
                .get(drv_hash)
                .map(|e| crate::sla::cost::selector_for(e.0, e.1))
                .unwrap_or(intent.node_selector);
            // r[impl sec.executor.identity-token+2]
            // Sign per-intent so the spawned pod can prove on
            // BuildExecution/Heartbeat that it was spawned for THIS
            // intent. Expiry: deadline + 5-min grace (the pod's
            // `activeDeadlineSeconds` is the upper bound; a token
            // outliving the pod is harmless). Empty when no HMAC key
            // is configured (dev mode → scheduler verify is permissive).
            let executor_token = self
                .hmac_signer
                .as_ref()
                .map(|s| {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    s.sign(&rio_auth::hmac::ExecutorClaims {
                        intent_id: drv_hash.to_string(),
                        kind: kind.into(),
                        expiry_unix: now
                            .saturating_add(u64::from(intent.deadline_secs))
                            .saturating_add(300),
                    })
                })
                .unwrap_or_default();
            intents.push((
                state.sched.priority,
                rio_proto::types::SpawnIntent {
                    intent_id: drv_hash.to_string(),
                    cores: intent.cores,
                    mem_bytes: intent.mem_bytes,
                    disk_bytes: intent.disk_bytes,
                    node_selector: node_selector.into_iter().collect(),
                    kind: kind.into(),
                    system: state.system.clone(),
                    required_features: state.required_features.clone(),
                    deadline_secs: intent.deadline_secs,
                    executor_token,
                },
            ));
        }

        // Priority-sort (critical-path first): `dag.iter_nodes()` is
        // HashMap-order, but the controller truncates to `[..headroom]`
        // under `maxConcurrent`. Unsorted, a high-priority large drv
        // past the prefix gets no pod and fails resource-fit on the
        // small ones spawned for low-priority work (large→small can't
        // overflow; small→large can). Descending so the prefix is the
        // work whose pods matter most.
        intents.sort_unstable_by(|(a, _), (b, _)| {
            b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal)
        });

        SpawnIntentsSnapshot {
            intents: intents.into_iter().map(|(_, i)| i).collect(),
            queued_by_system,
        }
    }

    /// Arm the Pending-watch for intents the controller just created
    /// Jobs for. Separated from [`Self::compute_spawn_intents`] so that
    /// RPC stays a pure read:
    ///
    ///  - dashboard/CLI/ComponentScaler polls of `GetSpawnIntents` no
    ///    longer mutate scheduler state;
    ///  - intents the controller TRUNCATES under `maxConcurrent` are
    ///    not armed — under sustained saturation those would never
    ///    heartbeat, so arm-on-emit false-marked their `(band, cap)`
    ///    cells ICE every Pending-watch window until the solve
    ///    degraded to band-agnostic.
    ///
    /// `or_insert`: the controller re-acks the FULL still-Pending set
    /// every tick (covers scheduler restart, where in-memory
    /// `pending_intents` is empty and existing Pending Jobs would
    /// otherwise never re-arm under deterministic softmax). A live
    /// timer is NOT reset by re-ack. After ICE-sweep removes a
    /// timed-out entry, the controller's reap-on-selector-drift +
    /// respawn + ack lands the NEW `(band, cap)` here as a fresh
    /// insert. A re-ack of a cell ICE-marked SINCE the controller's
    /// poll is dropped — the next poll returns the freshly-solved
    /// (cell-excluded) selector, so reap-on-selector-drift fires.
    /// Without the guard, a Tick sweep landing between poll and ack
    /// (~1-5s of k8s work) would have the stale ack re-pin the swept
    /// cell with a fresh timer, defeating ICE-backoff failover.
    pub(super) fn handle_ack_spawned_intents(&self, spawned: &[rio_proto::types::SpawnIntent]) {
        let now = std::time::Instant::now();
        for i in spawned {
            let sel: std::collections::BTreeMap<_, _> = i
                .node_selector
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if let Some((band, cap)) = crate::sla::cost::parse_selector(&sel)
                && !self.ice.is_infeasible(band, cap)
            {
                self.pending_intents
                    .entry(i.intent_id.clone().into())
                    .or_insert((band, cap, now));
            }
        }
    }

    /// [`SolvedIntent`] for one queued derivation via the SLA estimator.
    /// Shared between [`Self::compute_spawn_intents`] (SpawnIntent
    /// population) and dispatch's resource-fit filter so the controller
    /// spawns and the scheduler accepts the SAME shape.
    ///
    /// When `[sla].hw_cost_source` is set AND the hw-factor table is
    /// populated, the fitted-key branch routes through
    /// [`solve::solve_full`] (per-`(band, cap)` envelope + cost
    /// softmax) and returns a `rio.build/hw-band` +
    /// `karpenter.sh/capacity-type` nodeSelector. Otherwise — or for
    /// override/probe/explore branches — it routes through
    /// [`solve::intent_for`] (band-agnostic `solve_mvp`) and returns
    /// an empty selector.
    pub(crate) fn solve_intent_for(&self, state: &crate::state::DerivationState) -> SolvedIntent {
        use crate::sla::{
            quantile, solve,
            types::{ModelKey, RawCores},
        };
        let tenant = state
            .attributed_tenant(&self.builds)
            .map(|u| u.to_string())
            .unwrap_or_default();
        let key = state.pname.as_deref().map(|p| ModelKey {
            pname: p.to_string(),
            system: state.system.clone(),
            tenant,
        });
        let fit = key.as_ref().and_then(|k| self.sla_estimator.cached(k));
        // Override resolved from the same tick snapshot the fit cache
        // was refreshed alongside — both are ~60s stale at worst.
        let override_ = key
            .as_ref()
            .and_then(|k| self.sla_estimator.resolved_override(k));
        let hints = solve::DrvHints {
            enable_parallel_building: state.enable_parallel_building,
            prefer_local_build: state.prefer_local_build,
            required_features: state.required_features.clone(),
        };

        // r[impl sched.sla.solve-per-band-cap]
        // solve_full path: gated on hw_cost_source set ∧ hw-factor
        // table populated ∧ a usable fit (same n_eff/span gate as
        // intent_for's solve branch — probe/explore stay on the
        // band-agnostic path). A `forced_cores` OR `tier` override
        // also gates it off: solve_full doesn't take `override_`, so
        // those fall through to `intent_for` which honors both.
        // `forced_mem` is overlaid below regardless of arm.
        //
        // FOD / required_features / serial drvs MUST stay band-
        // agnostic: solve_full emits a `rio.build/hw-band` selector
        // that `apply_intent_resources` merges unconditionally, but the
        // `rio-fetcher` and `rio-builder-metal` NodePools carry no
        // hw-band label — `{node-role:fetcher, hw-band:X}` matches zero
        // templates and the pod is permanently Pending. Serial drvs
        // additionally need `intent_for`'s 1-core pin
        // (`r[sched.sla.intent-from-solve]`); solve_full ignores
        // `hints` and would multi-core a `enableParallelBuilding=false`
        // build. The hw_table snapshot is one RwLock-read clone
        // (~dozens of entries); cost_table same.
        // r[impl sched.sla.ice-ladder-cap]
        // Per-build ICE-ladder exhausted → skip `solve_full` so the
        // band-agnostic `intent_for` arm dispatches under no
        // hw-band/capacity-type selector.
        let ladder_exhausted = self
            .ice_attempts
            .get(&state.drv_hash)
            .is_some_and(|a| a.len() as u32 >= self.ice_ladder_cap());
        let hw = self.sla_estimator.hw_table();
        let full = (!ladder_exhausted
            && self.sla_config.hw_cost_source.is_some()
            && !hw.is_empty()
            && override_
                .as_ref()
                .is_none_or(|o| o.forced_cores.is_none() && o.tier.is_none())
            && hints.prefer_local_build != Some(true)
            && hints.enable_parallel_building != Some(false)
            && !state.is_fixed_output
            && state.required_features.is_empty())
        .then_some(())
        .and_then(|()| {
            let f = fit.as_ref()?;
            if f.n_eff < 3.0
                || (f.span < 4.0
                    && !crate::sla::explore::frozen(&f.explore, self.sla_ceilings.max_cores))
                || matches!(f.fit, crate::sla::types::DurationFit::Probe)
            {
                return None;
            }
            Some(solve::solve_full(
                f,
                &self.sla_tiers,
                &hw,
                &self.cost_table.read(),
                &self.sla_ceilings,
                &self.ice,
                0.3_f64, /* removed: A9 deletes call site */
                &mut rand::rng(),
            ))
        });

        let (cores, mem, disk, node_selector) = match &full {
            Some(r) => (
                (r.cores().0.ceil() as u32).max(1),
                r.mem().0,
                r.disk().0,
                r.node_selector().cloned().unwrap_or_default(),
            ),
            None => {
                let (c, m, d) = solve::intent_for(
                    fit.as_ref(),
                    &hints,
                    override_.as_ref(),
                    &self.sla_config,
                    &self.sla_tiers,
                    &self.sla_ceilings,
                );
                (c, m, d, Default::default())
            }
        };
        // `forced_mem` overlays whichever arm fired — `intent_for`
        // already applies it internally so this is a no-op there;
        // `solve_full` doesn't see `override_`, so without this a
        // `--mem`-only override is dead under `hwCostSource`.
        let mem = override_.as_ref().and_then(|o| o.forced_mem).unwrap_or(mem);
        // r[impl sched.sla.reactive-floor+2]
        // D4: floor AND ceiling at the single post-solve chokepoint.
        // Floor: a derivation that OOM'd at its solved mem had
        // `bump_floor_or_count` double `floor.mem`; the next solve
        // returns at least that. Ceiling: `intent_for`'s early-return
        // branches (forced/serial/local/explore) and the `forced_mem`
        // overlay above pass fit-derived bytes through unclamped, so
        // the `solve_mvp`/`solve_full` BestEffort clamp doesn't cover
        // them — a `disk_p90` (or `--mem` / `--cores`) above a
        // tightened `max_disk`/`max_mem`/`max_cores` would otherwise
        // spawn a permanently-Pending pod. `bump_floor_or_count`
        // already caps `floor` at `ceil` (floor.rs), so
        // `.max(floor).min(ceil)` always yields `≤ ceil`. Cores has no
        // `resource_floor` dimension (OOM/DiskPressure are mem/disk
        // under-provision, per the spec); `.max(1)` is belt-and-braces
        // — every upstream branch already floors at 1.
        let floor = &state.sched.resource_floor;
        let cores = cores.min(self.sla_ceilings.max_cores as u32).max(1);
        let mem = mem.max(floor.mem_bytes).min(self.sla_ceilings.max_mem);
        let disk = disk.max(floor.disk_bytes).min(self.sla_ceilings.max_disk);
        // D7: deadline_secs. Fitted ⇒ `wall_p99 × 5` (p99 of the
        // log-normal `T(c)·exp(ε)` at the chosen cores, no retry tail
        // — k8s-kill-then-reactive-floor IS the retry). Unfitted
        // (probe/explore/override-with-no-fit) ⇒ `[sla].probe.
        // deadline_secs` — or the matching `feature_probes` entry, same
        // lookup `explore::next` uses for the cores/mem ladder. The
        // fitted-path `q99×5` is FLOORED at the probe deadline: a
        // sub-second fit (trivial-builders) would otherwise yield
        // `activeDeadlineSeconds≈3`, killing the Job before the pod
        // ever heartbeats — and with no heartbeat there's no
        // `recently_disconnected` entry, so `bump_floor_or_count`
        // never runs and the next solve emits the same 3s. Clamp
        // order: floor first (D4 — a `bump_floor_or_count
        // (DeadlineExceeded)` doubles `floor.deadline_secs`; the next
        // solve must honor it), then 24h ceiling so a doubled floor
        // cannot run away.
        let probe_deadline = hints
            .required_features
            .iter()
            .find_map(|f| self.sla_config.feature_probes.get(f))
            .unwrap_or(&self.sla_config.probe)
            .deadline_secs;
        let computed = fit
            .as_ref()
            .filter(|f| !matches!(f.fit, crate::sla::types::DurationFit::Probe))
            .map(|f| {
                // r[impl sched.sla.hw-ref-seconds]
                // `t_at` is ref-seconds (fit ingests hw-normalized
                // samples); `activeDeadlineSeconds` is wall-clock.
                // De-normalize by the SLOWEST hw_class so the budget
                // covers worst-case wall regardless of which band the
                // pod lands on — band is unknown when `full` is None,
                // and re-deriving the chosen band's factor when Some
                // would duplicate `cost::h_dagger`. Empty table → 1.0
                // (ref==wall, no normalization in effect).
                let t = f.fit.t_at(RawCores(f64::from(cores))).0 / hw.min_factor();
                (quantile::quantile(0.99, t, f.sigma_resid, 0.0, f.z_q(0.99)) * 5.0) as u32
            })
            .map_or(probe_deadline, |c| c.max(probe_deadline));
        let deadline_secs = computed
            .max(floor.deadline_secs)
            .min(crate::actor::floor::DEADLINE_CAP_SECS);
        // Dispatch-time prediction snapshot for completion's
        // actual-vs-predicted scoring. Only meaningful when there's a
        // fitted curve to evaluate `T(c)` against — cold-start probes
        // leave `wall_secs=None` so the prediction-ratio histogram
        // isn't poisoned by guesses. `Probe` is filtered:
        // `Probe.t_at(_) = ∞` would record `actual/∞ = 0` into
        // `sla_prediction_ratio{dim=wall}`.
        //
        // `(tier, tier_p90)` mirrors `intent_for`'s resolution so the
        // recorded tier matches what dispatch actually sized against:
        // forced-cores / serial / prefer-local short-circuit before
        // any solve (no tier); a `--tier` override solves against
        // ONLY that tier. Re-solving the full ladder here recorded a
        // tighter tier than the build was sized for → false
        // `sla_envelope_result_total{result="miss"}` on a build that
        // ran exactly as sized for its operator-pinned slow tier.
        let predicted = fit
            .as_ref()
            .filter(|f| !matches!(f.fit, crate::sla::types::DurationFit::Probe))
            .map(|f| {
                let no_tier = override_.as_ref().is_some_and(|o| o.forced_cores.is_some())
                    || hints.prefer_local_build == Some(true)
                    || hints.enable_parallel_building == Some(false);
                let pinned = override_.as_ref().and_then(|o| o.tier.as_deref()).map(|n| {
                    self.sla_tiers
                        .iter()
                        .filter(|t| t.name == n)
                        .cloned()
                        .collect::<Vec<_>>()
                });
                let tiers = pinned.as_deref().unwrap_or(&self.sla_tiers);
                let (tier, tier_target) = if no_tier {
                    (None, None)
                } else {
                    // `full` is `None` ⇒ re-run `solve_mvp` (now pure)
                    // for the tier name. Match-on-borrow so the local
                    // `mvp` outlives the `&SolveResult`. Previously
                    // `unwrap_or(&solve_mvp(...))` evaluated EAGERLY,
                    // running `solve_mvp` even when `full` was Some —
                    // wasted compute, and double-counted the
                    // `sla_infeasible_total` metric back when
                    // `solve_mvp` emitted internally.
                    let mvp;
                    let r: &solve::SolveResult = match full.as_ref() {
                        Some(r) => r,
                        None => {
                            mvp = solve::solve_mvp(f, tiers, &self.sla_ceilings);
                            &mvp
                        }
                    };
                    match r {
                        solve::SolveResult::Feasible { tier, .. } => (
                            Some(tier.clone()),
                            self.sla_tiers
                                .iter()
                                .find(|t| t.name == *tier)
                                .and_then(solve::Tier::binding_bound),
                        ),
                        solve::SolveResult::BestEffort { .. } => (None, None),
                    }
                };
                solve::SlaPrediction {
                    wall_secs: Some(f.fit.t_at(RawCores(f64::from(cores))).0),
                    mem_bytes: mem,
                    tier,
                    tier_target,
                }
            });
        SolvedIntent {
            cores,
            mem_bytes: mem,
            disk_bytes: disk,
            deadline_secs,
            predicted,
            node_selector,
        }
    }

    pub(super) fn handle_list_executors(&self) -> Vec<command::ExecutorSnapshot> {
        self.executors
            .values()
            .map(|w| command::ExecutorSnapshot {
                executor_id: w.executor_id.clone(),
                kind: w.kind,
                systems: w.systems.clone(),
                supported_features: w.supported_features.clone(),
                busy: w.running_build.is_some(),
                draining: w.is_draining(),
                store_degraded: w.store_degraded,
                connected_since: w.connected_since,
                last_heartbeat: w.last_heartbeat,
                last_resources: w.last_resources,
            })
            .collect()
    }

    // r[impl sched.admin.inspect-dag]
    /// Actor in-memory snapshot of a build's derivations cross-referenced
    /// with the live stream pool. I-025 diagnostic: `executor_has_stream`
    /// is false when a derivation is Assigned to an executor whose gRPC
    /// bidi stream is gone from `self.executors` — dispatch can never
    /// complete. PG (`rio-cli workers`) may still show the executor as
    /// alive; only the actor's HashMap knows the stream is dead.
    pub(super) fn handle_inspect_build_dag(
        &self,
        build_id: Uuid,
    ) -> (Vec<rio_proto::types::DerivationDiagnostic>, Vec<String>) {
        let now = std::time::Instant::now();
        let derivations = self
            .dag
            .iter_nodes()
            .filter(|(_, s)| s.interested_builds.contains(&build_id))
            .map(|(_, s)| {
                let assigned_executor = s
                    .assigned_executor
                    .as_ref()
                    .map(|e| e.to_string())
                    .unwrap_or_default();
                // THE I-025 signal.
                let executor_has_stream = s
                    .assigned_executor
                    .as_ref()
                    .is_some_and(|e| self.executors.contains_key(e));
                let backoff_remaining_secs = s
                    .retry
                    .backoff_until
                    .and_then(|deadline| deadline.checked_duration_since(now))
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                // I-062: for Ready derivations, simulate hard_filter
                // against every executor and name the first rejecting
                // clause. O(ready × executors) per RPC — fine for a
                // debug call. Non-Ready get an empty vec (the question
                // doesn't apply).
                let rejections = if s.status() == DerivationStatus::Ready {
                    self.executors
                        .values()
                        .map(|w| rio_proto::types::ExecutorRejection {
                            executor_id: w.executor_id.to_string(),
                            reason: crate::assignment::rejection_reason(w, s)
                                .unwrap_or("ACCEPT")
                                .to_string(),
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                rio_proto::types::DerivationDiagnostic {
                    drv_path: s.drv_path().to_string(),
                    drv_hash: s.drv_hash.to_string(),
                    status: format!("{:?}", s.status()),
                    is_fod: s.is_fixed_output,
                    assigned_executor,
                    executor_has_stream,
                    retry_count: s.retry.count,
                    infra_retry_count: s.retry.infra_count,
                    backoff_remaining_secs,
                    interested_build_count: s.interested_builds.len() as u32,
                    system: s.system.clone(),
                    required_features: s.required_features.clone(),
                    failed_builders: s
                        .retry
                        .failed_builders
                        .iter()
                        .map(|e| e.to_string())
                        .collect(),
                    rejections,
                }
            })
            .collect();
        let live_executor_ids = self.executors.keys().map(|e| e.to_string()).collect();
        (derivations, live_executor_ids)
    }

    // r[impl sched.admin.debug-list-executors]
    /// Snapshot the in-memory executor map. Backs both unit-test
    /// assertions and the `DebugListExecutors` RPC (`rio-cli workers
    /// --actor`). The fields beyond the original four are the I-048b/c
    /// post-mortem additions: `has_stream` and `kind` together would
    /// have collapsed that investigation into one look — PG showed
    /// fetchers `[alive]`, but the actor map had zero fetcher-kind
    /// entries with `has_stream=true`.
    pub(super) fn handle_debug_query_workers(&self) -> Vec<DebugExecutorInfo> {
        self.executors
            .values()
            .map(|w| DebugExecutorInfo {
                executor_id: w.executor_id.to_string(),
                has_stream: w.stream_tx.is_some(),
                is_registered: w.is_registered(),
                warm: w.warm,
                kind: w.kind,
                systems: w.systems.clone(),
                last_heartbeat_ago_secs: w.last_heartbeat.elapsed().as_secs(),
                running_build: w.running_build.as_ref().map(|h| h.to_string()),
                draining: w.is_draining(),
                store_degraded: w.store_degraded,
                intent_id: w.intent_id.clone(),
            })
            .collect()
    }

    /// Collect `expected_output_paths ∪ output_paths` from all
    /// non-terminal derivations. These are the live-build roots that
    /// GC must NOT delete — either the worker is about to upload them
    /// (expected) or just did (output). Both cases: don't race the
    /// upload.
    ///
    /// Dedup via HashSet: the same drv can appear in multiple builds
    /// (shared dependency) → same expected_output_paths would be
    /// duplicated N× in the roots list. The store's mark CTE handles
    /// dups correctly, but it's wasted network + CTE work.
    ///
    /// Floating-CA derivations carry `expected_output_paths == [""]`
    /// pre-completion (translate.rs convention) — filter so the
    /// store's `validate_store_path` doesn't reject the whole batch
    /// with `InvalidArgument` whenever any CA build is in flight.
    // r[impl sched.gc.live-pins]
    pub(super) fn handle_gc_roots(&self) -> Vec<String> {
        self.dag
            .iter_nodes()
            .filter(|(_, s)| !s.status().is_terminal())
            .flat_map(|(_, s)| {
                s.expected_output_paths
                    .iter()
                    .chain(s.output_paths.iter())
                    .filter(|p| !p.is_empty())
                    .cloned()
            })
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect()
    }
}
