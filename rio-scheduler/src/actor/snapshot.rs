//! Read-only snapshot/inspect handlers on [`DagActor`]. All methods
//! here are `&self` over the in-memory DAG/executors and back the admin
//! RPCs (ClusterStatus, GetSizeClassStatus, EstimatorStats,
//! InspectBuildDag, DebugListExecutors).

use std::collections::HashMap;

use uuid::Uuid;

use crate::state::{BuildState, DerivationStatus};

use super::{
    AdminQuery, ClusterSnapshot, DagActor, DebugExecutorInfo, EstimatorStatsEntry,
    SizeClassSnapshot, command,
};

impl DagActor {
    /// Dispatch a read-only [`AdminQuery`].
    pub(super) fn handle_admin(&self, q: AdminQuery) {
        match q {
            AdminQuery::GetSizeClassSnapshot {
                pool_features,
                reply,
            } => {
                let _ = reply.send((
                    self.compute_size_class_snapshot(pool_features.as_deref()),
                    self.compute_fod_size_class_snapshot(),
                ));
            }
            AdminQuery::EstimatorStats { reply } => {
                let _ = reply.send(self.compute_estimator_stats());
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
                // active_override: re-run resolve so the RPC reflects
                // exactly what dispatch would see, then surface the
                // matching ROW (not the projected ResolvedTarget) so
                // the CLI can show id/created_by/expires_at.
                let rows = self.sla_estimator.overrides();
                let active = crate::sla::r#override::resolve(&key, &rows).and_then(|_| {
                    rows.into_iter()
                        .filter(|r| {
                            r.pname == key.pname
                                && r.system.as_deref().is_none_or(|s| s == key.system)
                                && r.tenant.as_deref().is_none_or(|t| t == key.tenant)
                        })
                        .max_by(|a, b| {
                            (u8::from(a.system.is_some()) + u8::from(a.tenant.is_some()))
                                .cmp(&(u8::from(b.system.is_some()) + u8::from(b.tenant.is_some())))
                                .then(a.created_at.total_cmp(&b.created_at))
                        })
                });
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
        //
        // Queued-FOD: total in-flight FOD demand (Ready | Assigned |
        // Running). This is the FetcherPool autoscaler signal. Ready-only
        // (the pre-P0541 definition) undercounts: with N fetchers, the
        // first N FODs go Ready→Assigned within one dispatch tick (~10s),
        // so a 30s controller poll sees Ready=0 and never scales past N.
        // Including Assigned+Running makes the signal match "pods I want"
        // — same shape as `queued_derivations + running_derivations` for
        // the BuilderPool scaler. The DAG iteration is the source —
        // `ready_queue` is hash+priority only, no FOD bit.
        let mut running_derivations = 0u32;
        let mut queued_fod_derivations = 0u32;
        let mut queued_by_system: HashMap<String, u32> = HashMap::new();
        for (_, s) in self.dag.iter_nodes() {
            match s.status() {
                DerivationStatus::Assigned | DerivationStatus::Running => {
                    running_derivations += 1;
                    if s.is_fixed_output {
                        queued_fod_derivations += 1;
                    }
                }
                DerivationStatus::Ready => {
                    // I-107: per-system queued breakdown so per-arch
                    // BuilderPools scale on their own backlog. Ready-only
                    // to match `queued_derivations` (= ready_queue.len())
                    // semantics — sum across keys equals the scalar.
                    *queued_by_system.entry(s.system.clone()).or_default() += 1;
                    if s.is_fixed_output {
                        queued_fod_derivations += 1;
                    }
                }
                _ => {}
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
            queued_fod_derivations,
            queued_by_system,
        }
    }

    /// Compute per-size-class snapshot for `GetSizeClassStatus`.
    ///
    /// Three passes:
    /// 1. `size_classes.read()` → effective cutoffs (post-rebalancer)
    /// 2. `configured_cutoffs` lookup → static TOML cutoffs
    /// 3. Single `iter_nodes()` pass: for each derivation, increment
    ///    the appropriate class's `queued` or `running` counter.
    ///
    /// For **queued**: Ready-status derivations. They haven't been
    /// dispatched yet so `assigned_size_class` is None. We call
    /// `classify()` with the SAME inputs dispatch would use
    /// (est_duration + estimator peaks) — this is a forecast, not a
    /// fact. If the rebalancer shifts cutoffs between this call and
    /// actual dispatch, the class may differ. Acceptable for an
    /// operator view.
    ///
    /// For **running**: Assigned/Running derivations. Use
    /// `assigned_size_class` directly — that's the class we ACTUALLY
    /// routed to (may be larger than classify() would give if we
    /// overflowed due to no capacity in the target class).
    ///
    /// O(dag_nodes + n_classes) per call. The classify() inside the
    /// loop is O(n_classes) but n_classes is ~3-5. Total cost is
    /// dominated by the node iteration, same as `compute_cluster_snapshot`.
    ///
    /// `pool_features`: I-176 feature filter. `None` = unfiltered.
    /// `Some(f)` = only count Ready derivations whose
    /// `required_features ⊆ f` — the same subset check
    /// `rejection_reason()`'s `feature-missing` clause applies. The
    /// controller passes `BuilderPool.spec.features` here so each
    /// pool's spawn decision sees only derivations its workers could
    /// accept.
    // pub(crate) for the configured-vs-effective test (tests/misc.rs)
    // which exercises it on a bare (unspawned) actor so it can mutate
    // size_classes directly to simulate a rebalancer pass.
    pub(crate) fn compute_size_class_snapshot(
        &self,
        pool_features: Option<&[String]>,
    ) -> Vec<SizeClassSnapshot> {
        // Take a read lock for the whole computation. Rebalancer
        // writes hourly; contention is near-zero. Dropped at end of
        // scope (no await in this fn).
        let classes = self.sizing.size_classes.read();
        if classes.is_empty() {
            // Feature off — return empty. Handler maps to empty
            // response which the CLI can render as "size-class
            // routing disabled."
            return Vec::new();
        }

        // name → index into `snapshots`. classify() and
        // assigned_size_class both return names, not indices.
        let mut index: HashMap<String, usize> = HashMap::with_capacity(classes.len());
        // I-146: index of the smallest-cutoff class. Fallback bucket
        // for any Ready non-FOD that classify() somehow doesn't place
        // — every dispatchable derivation MUST count in some class so
        // the controller never sees queued=0 across all pools while
        // ready_queue is non-empty (which would scale every pool to 0
        // and deadlock dispatch). With the current classify() contract
        // (always Some when classes non-empty) the fallback is
        // unreachable; it's defensive against future classify changes
        // and makes the "sum(queued) == Ready non-FOD count" invariant
        // structural rather than incidental.
        let mut smallest_idx = 0usize;
        let mut snapshots: Vec<SizeClassSnapshot> = classes
            .iter()
            .enumerate()
            .map(|(i, c)| {
                index.insert(c.name.clone(), i);
                if c.cutoff_secs < classes[smallest_idx].cutoff_secs {
                    smallest_idx = i;
                }
                // configured_cutoffs lookup: linear scan is fine for
                // ~3-5 classes. Falls back to effective if not found
                // (shouldn't happen — both populated from the same
                // config in DagActor::new, but defensive against a
                // future config-reload path that forgets one).
                let configured = self
                    .sizing
                    .configured_cutoffs
                    .iter()
                    .find(|(n, _)| n == &c.name)
                    .map(|(_, cut)| *cut)
                    .unwrap_or(c.cutoff_secs);
                SizeClassSnapshot {
                    name: c.name.clone(),
                    effective_cutoff_secs: c.cutoff_secs,
                    configured_cutoff_secs: configured,
                    queued: 0,
                    running: 0,
                    queued_by_system: HashMap::new(),
                    running_by_system: HashMap::new(),
                    spawn_intents: Vec::new(),
                }
            })
            .collect();

        // Single pass: classify or look up per derivation.
        for (drv_hash, state) in self.dag.iter_nodes() {
            match state.status() {
                DerivationStatus::Ready => {
                    // I-146: FODs dispatch to fetchers, NOT size-class
                    // builders (find_executor_with_overflow skips the
                    // overflow chain entirely for is_fixed_output —
                    // ADR-019). Counting them here would inflate
                    // builder-pool demand for work those builders will
                    // never receive. The Running arm already excludes
                    // FODs implicitly (assigned_size_class=None for FOD
                    // dispatch); this makes Ready symmetric. FOD demand
                    // is reported via ClusterSnapshot.queued_fod_
                    // derivations → FetcherPool autoscaler.
                    if state.is_fixed_output {
                        continue;
                    }
                    // r[impl sched.sizeclass.feature-filter+2]
                    // I-176: skip derivations a worker with
                    // `pool_features` couldn't build. Mirrors the
                    // `feature-missing` clause in rejection_reason()
                    // — a kvm derivation never counts toward a
                    // featureless pool's spawn decision (it would
                    // spawn a builder that hard_filter rejects), and
                    // a kvm pool sees kvm-required work even when
                    // classify() buckets it into a class no kvm pool
                    // owns. `None` = no filter (CLI, status display).
                    if let Some(pf) = pool_features {
                        // I-181: feature-gated pools (non-empty pf)
                        // don't count featureless work — ∅ ⊆ anything
                        // would over-spawn; the featureless pool owns
                        // it. dispatch_ready's overflow walk tries the
                        // cheapest pool first, so a kvm builder spawned
                        // for ∅-feature work would idle until
                        // activeDeadlineSeconds.
                        if !pf.is_empty() && state.required_features.is_empty() {
                            continue;
                        }
                        // I-176: subset check. Derivation needs a
                        // feature this pool lacks → skip. ∅-pf
                        // (featureless pool) passes only ∅-feature
                        // derivations (all() over empty is vacuously
                        // true, contains() over non-empty fails).
                        if !state.required_features.iter().all(|f| pf.contains(f)) {
                            continue;
                        }
                    }
                    // Forecast: what class WOULD dispatch pick?
                    // Same inputs as dispatch.rs — est_duration stored
                    // on the state at merge time; peak_memory /
                    // peak_cpu from the estimator. classify() is
                    // contractually Some when classes is non-empty
                    // (checked above), so the .and_then chain resolves;
                    // .unwrap_or(smallest_idx) is the I-146 belt: if a
                    // future classify() change introduces a None path,
                    // the derivation lands in the smallest class
                    // (matching dispatch's "any worker" semantics for
                    // target_class=None) rather than vanishing from
                    // every pool's queued count.
                    let classify_idx = crate::assignment::classify(
                        state.sched.est_duration,
                        self.estimator
                            .peak_memory(state.pname.as_deref(), &state.system),
                        self.estimator
                            .peak_cpu(state.pname.as_deref(), &state.system),
                        &classes,
                    )
                    .and_then(|c| index.get(&c).copied())
                    .unwrap_or(smallest_idx);
                    // r[impl sched.sizeclass.snapshot-honors-floor]
                    // I-187: clamp at `size_class_floor` — the same
                    // `max(target_cutoff, floor_cutoff)` dispatch.rs
                    // applies in `find_executor_with_overflow`. A
                    // derivation promoted tiny→small via I-177 still
                    // classifies as tiny (EMA is success-only); without
                    // this clamp the snapshot reports `tiny.queued=1`,
                    // controller spawns tiny, dispatch rejects
                    // (floor>tiny), tiny idles 120s → disconnects →
                    // I-173 bumps floor again → spawn loop. A floor
                    // not in the current config (stale) degrades to
                    // no-clamp via `index.get()=None` — same fallback
                    // as dispatch's `cutoff_for()=None`.
                    let i = state
                        .sched
                        .size_class_floor
                        .as_deref()
                        .and_then(|f| index.get(f).copied())
                        .filter(|&fi| classes[fi].cutoff_secs > classes[classify_idx].cutoff_secs)
                        .unwrap_or(classify_idx);
                    snapshots[i].queued += 1;
                    // I-143: per-system breakdown so per-arch
                    // size-class pools scale on their own backlog.
                    *snapshots[i]
                        .queued_by_system
                        .entry(state.system.clone())
                        .or_default() += 1;
                    // r[impl sched.sla.intent-from-solve]
                    // ADR-023: per-derivation SpawnIntent. intent_id is
                    // the drv_hash itself — the controller stamps it on
                    // the pod annotation, the builder echoes it on
                    // heartbeat, dispatch matches `worker.intent_id ==
                    // drv_hash` (find_executor_with_overflow). No
                    // separate intent→drv map to keep in sync; if the
                    // drv leaves Ready before the pod heartbeats, the
                    // match misses and dispatch falls through to
                    // pick-from-queue.
                    //
                    // Gated on `[sla]` configured: the controller's
                    // `jobs.rs` reconcile branches on
                    // `intents.is_empty()` — non-empty diverts to
                    // `spawn_for_each` (per-drv pod resources from
                    // `solve_intent_for`). Under Static sizing the
                    // controller MUST stay on the `spawn_n` path (fixed
                    // per-class `spec.resources`); emitting intents here
                    // with `sla_config=None` sends 4-core/8-GiB fallback
                    // probes that don't match any class shape, and the
                    // `est_memory_bytes`/intent-reservation dispatch
                    // gates have no symmetric spawn-side. Static mode ⇒
                    // empty intents ⇒ exactly pre-ADR-023 behavior.
                    if self.sla_config.is_some() {
                        let (cores, mem_bytes, disk_bytes, _, node_selector) =
                            self.solve_intent_for(state.pname.as_deref(), state);
                        // ADR-023 §2.8: arm the Pending-watch the FIRST
                        // time a band-targeted selector is emitted for
                        // this drv. or_insert keeps the original
                        // timestamp across re-emits (snapshot runs each
                        // tick). Band-agnostic intents don't enter the
                        // ladder.
                        if let Some((band, cap)) = crate::sla::cost::parse_selector(&node_selector)
                        {
                            self.pending_intents.entry(drv_hash.into()).or_insert((
                                band,
                                cap,
                                std::time::Instant::now(),
                            ));
                        }
                        snapshots[i]
                            .spawn_intents
                            .push(rio_proto::types::SpawnIntent {
                                intent_id: drv_hash.to_string(),
                                cores,
                                mem_bytes,
                                disk_bytes,
                                node_selector: node_selector.into_iter().collect(),
                            });
                    }
                }
                DerivationStatus::Assigned | DerivationStatus::Running => {
                    // Fact: what class DID we dispatch to?
                    // assigned_size_class reflects overflow — if the
                    // target was "small" but only "large" had
                    // capacity, this says "large". That's the
                    // operator-relevant answer for "where are my
                    // workers busy?"
                    if let Some(class) = &state.sched.assigned_size_class
                        && let Some(&i) = index.get(class)
                    {
                        snapshots[i].running += 1;
                        *snapshots[i]
                            .running_by_system
                            .entry(state.system.clone())
                            .or_default() += 1;
                    }
                }
                // Terminal + pre-Ready: neither queued nor running.
                _ => {}
            }
        }

        // Sort by effective cutoff ascending — smallest class first.
        // The proto doc says "sorted by effective_cutoff_secs";
        // consumers (P0236's CLI table, P0234's autoscaler) can rely
        // on this order. total_cmp for NaN-safety (same defense as
        // assignment.rs:106).
        snapshots.sort_by(|a, b| a.effective_cutoff_secs.total_cmp(&b.effective_cutoff_secs));
        snapshots
    }

    /// `(cores, mem, disk, predicted, node_selector)` for one queued
    /// derivation via the SLA estimator. Shared between
    /// [`Self::compute_size_class_snapshot`] (SpawnIntent population)
    /// and dispatch's resource-fit filter so the controller spawns and
    /// the scheduler accepts the SAME shape.
    ///
    /// When `[sla].hw_cost_source` is set AND the hw-factor table is
    /// populated, the fitted-key branch routes through
    /// [`solve::solve_full`] (per-`(band, cap)` envelope + cost
    /// softmax) and returns a `rio.build/hw-band` +
    /// `karpenter.sh/capacity-type` nodeSelector. Otherwise — or for
    /// override/probe/explore branches — it routes through
    /// [`solve::intent_for`] (band-agnostic `solve_mvp`) and returns
    /// an empty selector.
    ///
    /// `pname` is taken separately so the dispatch caller (which has
    /// already destructured `state.pname.as_deref()` for `classify()`)
    /// can pass the borrow it already holds.
    pub(crate) fn solve_intent_for(
        &self,
        pname: Option<&str>,
        state: &crate::state::DerivationState,
    ) -> (
        u32,
        u64,
        u64,
        Option<crate::sla::solve::SlaPrediction>,
        std::collections::BTreeMap<String, String>,
    ) {
        use crate::sla::{
            solve,
            types::{ModelKey, RawCores},
        };
        // Tenant: same attribution as completion.rs's write_build_sample
        // — first interested build's tenant_id, stringified, "" on
        // None. The two MUST agree or the cache key never matches the
        // rows that fed it.
        let tenant = state
            .interested_builds
            .iter()
            .filter_map(|id| self.builds.get(id)?.tenant_id)
            .next()
            .map(|u| u.to_string())
            .unwrap_or_default();
        let key = pname.map(|p| ModelKey {
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
        // intent_for's solve branch — override/probe/explore stay on
        // the band-agnostic path). The hw_table snapshot is one
        // RwLock-read clone (~dozens of entries); cost_table same.
        let hw = self.sla_estimator.hw_table();
        let full = self
            .sla_config
            .as_ref()
            .filter(|c| c.hw_cost_source.is_some())
            .filter(|_| !hw.is_empty())
            .filter(|_| override_.as_ref().is_none_or(|o| o.forced_cores.is_none()))
            .filter(|_| hints.prefer_local_build != Some(true))
            .and_then(|c| {
                let f = fit.as_ref()?;
                if f.n_eff < 3.0
                    || (f.span < 4.0
                        && !crate::sla::explore::frozen(&f.explore, self.sla_ceilings.max_cores))
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
                    c.hw_softmax_temp,
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
                    self.sla_config.as_ref(),
                    &self.sla_tiers,
                    &self.sla_ceilings,
                );
                (c, m, d, Default::default())
            }
        };
        // Dispatch-time prediction snapshot for completion's
        // actual-vs-predicted scoring. Only meaningful when there's a
        // fitted curve to evaluate `T(c)` against — cold-start probes
        // and forced-cores overrides leave `wall_secs=None` so the
        // prediction-ratio histogram isn't poisoned by guesses.
        let predicted = fit.as_ref().map(|f| {
            let (tier, tier_p90) = match full
                .as_ref()
                .map(|r| r as &solve::SolveResult)
                .unwrap_or(&solve::solve_mvp(f, &self.sla_tiers, &self.sla_ceilings))
            {
                solve::SolveResult::Feasible { tier, .. } => (
                    Some(tier.clone()),
                    self.sla_tiers
                        .iter()
                        .find(|t| t.name == *tier)
                        .and_then(|t| t.p90),
                ),
                solve::SolveResult::BestEffort { .. } => (None, None),
            };
            solve::SlaPrediction {
                wall_secs: Some(f.fit.t_at(RawCores(f64::from(cores))).0),
                mem_bytes: mem,
                tier,
                tier_p90,
            }
        });
        (cores, mem, disk, predicted, node_selector)
    }

    /// Per-FOD-class snapshot for `GetSizeClassStatus.fod_classes`
    /// (P0556). Buckets in-flight FODs by `size_class_floor` so the
    /// ephemeral FetcherPool reconciler can spawn per-class Jobs
    /// instead of stamping the smallest class only.
    ///
    /// Unlike [`compute_size_class_snapshot`] there is no `classify()`
    /// forecast: FODs have no a-priori size signal (ADR-019), so the
    /// floor IS the routing decision. `floor=None` (never failed —
    /// the cold-start majority) buckets to `fetcher_classes[0]`.
    /// An unknown floor name (config drift: scheduler restarted with
    /// fewer classes) also buckets to `[0]` — same conservative
    /// fallback as I-146 for builder classes.
    ///
    /// `queued` here is **in-flight demand** (Ready+Assigned+Running),
    /// matching [`ClusterSnapshot::queued_fod_derivations`] semantics
    /// so `Σ fod_classes[i].queued == queued_fod_derivations`. The
    /// controller's `spawn_count(queued, active, headroom)` subtracts
    /// per-class active Jobs, so including Assigned/Running is the
    /// right shape (an Assigned FOD has a Job; spawn_count won't
    /// double-spawn for it). `running` is the Assigned+Running subset
    /// — informational for operators/dashboards.
    ///
    /// Preserves `fetcher_classes` order (smallest→largest by cutoff,
    /// snapshotted at construction).
    ///
    /// [`compute_size_class_snapshot`]: Self::compute_size_class_snapshot
    // r[impl sched.fod.size-class-reactive]
    pub(crate) fn compute_fod_size_class_snapshot(&self) -> Vec<SizeClassSnapshot> {
        if self.sizing.fetcher_classes.is_empty() {
            return Vec::new();
        }
        let mut index: HashMap<String, usize> =
            HashMap::with_capacity(self.sizing.fetcher_classes.len());
        let mut snapshots: Vec<SizeClassSnapshot> = self
            .sizing
            .fetcher_classes
            .iter()
            .enumerate()
            .map(|(i, name)| {
                index.insert(name.clone(), i);
                SizeClassSnapshot {
                    name: name.clone(),
                    // No duration cutoffs for fetcher classes — routing
                    // is reactive-only. Zeroed; proto consumers ignore
                    // these for fod_classes.
                    effective_cutoff_secs: 0.0,
                    configured_cutoff_secs: 0.0,
                    queued: 0,
                    running: 0,
                    queued_by_system: HashMap::new(),
                    running_by_system: HashMap::new(),
                    spawn_intents: Vec::new(),
                }
            })
            .collect();

        for (_, state) in self.dag.iter_nodes() {
            if !state.is_fixed_output {
                continue;
            }
            let in_flight = matches!(
                state.status(),
                DerivationStatus::Ready | DerivationStatus::Assigned | DerivationStatus::Running
            );
            if !in_flight {
                continue;
            }
            // floor=None → smallest (index 0, config-order convention).
            // Unknown floor (config drift) → also smallest.
            let i = state
                .sched
                .size_class_floor
                .as_ref()
                .and_then(|f| index.get(f).copied())
                .unwrap_or(0);
            snapshots[i].queued += 1;
            *snapshots[i]
                .queued_by_system
                .entry(state.system.clone())
                .or_default() += 1;
            if matches!(
                state.status(),
                DerivationStatus::Assigned | DerivationStatus::Running
            ) {
                snapshots[i].running += 1;
                *snapshots[i]
                    .running_by_system
                    .entry(state.system.clone())
                    .or_default() += 1;
            }
        }
        snapshots
    }

    // r[impl sched.admin.estimator-stats]
    /// Per-`(pname, system)` estimator dump for `GetEstimatorStats`
    /// (I-124). Walks the in-memory `build_history` snapshot and
    /// classifies each entry under the CURRENT effective cutoffs —
    /// the same `self.sizing.size_classes.read()` dispatch uses, post-
    /// rebalancer drift. Filtering + sorting happen handler-side
    /// (admin/estimator.rs); this returns the full set.
    pub(crate) fn compute_estimator_stats(&self) -> Vec<EstimatorStatsEntry> {
        let classes = self.sizing.size_classes.read();
        self.estimator
            .iter_history()
            .map(|((pname, system), entry)| EstimatorStatsEntry {
                pname: pname.clone(),
                system: system.clone(),
                sample_count: entry.sample_count,
                ema_duration_secs: entry.ema_duration_secs,
                ema_peak_memory_bytes: entry.ema_peak_memory_bytes,
                size_class: crate::assignment::classify(
                    entry.ema_duration_secs,
                    entry.ema_peak_memory_bytes,
                    entry.ema_peak_cpu_cores,
                    &classes,
                ),
            })
            .collect()
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
                size_class: w.size_class.clone(),
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
                            reason: crate::assignment::rejection_reason(w, s, None)
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
    // r[impl sched.gc.live-pins]
    pub(super) fn handle_gc_roots(&self) -> Vec<String> {
        self.dag
            .iter_nodes()
            .filter(|(_, s)| !s.status().is_terminal())
            .flat_map(|(_, s)| {
                s.expected_output_paths
                    .iter()
                    .chain(s.output_paths.iter())
                    .cloned()
            })
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect()
    }
}
