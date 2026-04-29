//! Read-only snapshot/inspect handlers on [`DagActor`]. All methods
//! here are `&self` over the in-memory DAG/executors and back the admin
//! RPCs (ClusterStatus, GetSpawnIntents, InspectBuildDag,
//! DebugListExecutors).

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::state::{
    BuildState, DerivationStatus, DrvHash, ExecutorId, ExecutorState, HEARTBEAT_TIMEOUT_SECS,
    SolvedIntent,
};

use super::{
    AdminQuery, ClusterSnapshot, DagActor, DebugExecutorInfo, SpawnIntentsRequest,
    SpawnIntentsSnapshot, command,
};

/// §13b hung-node detector (`r[sched.admin.hung-node-detector]`).
///
/// Groups executors by `node_name`; flags a node when
/// `stale ≥ max(3, ⌈0.5·occ⌉)` AND those stale executors span ≥2
/// tenants. The tenant criterion discriminates a hung NODE (kubelet/
/// EBS/kernel — every tenant's builds stall) from a single tenant's
/// pathological build hanging its own pods. Idle stale executors
/// (`running_build = None`) count toward `stale`/`occ` but not
/// `tenants` — an all-idle hung node fails the tenant gate, which is
/// fine: no work is stuck there, and `consolidate::reap_idle` covers
/// it once Karpenter posts `Empty=True`.
///
/// Free function over the inputs (not `&self`) so the test can supply
/// a `tenant_of` closure without seeding `dag` + `builds`.
// r[impl sched.admin.hung-node-detector]
pub(super) fn detect_hung_nodes(
    executors: &HashMap<ExecutorId, ExecutorState>,
    now: Instant,
    tenant_of: impl Fn(&DrvHash) -> Option<Uuid>,
) -> Vec<String> {
    #[derive(Default)]
    struct Agg {
        occ: u32,
        stale: u32,
        tenants: HashSet<Uuid>,
    }
    let timeout = Duration::from_secs(HEARTBEAT_TIMEOUT_SECS);
    let mut by_node: HashMap<&str, Agg> = HashMap::new();
    for w in executors.values() {
        let Some(node) = w.node_name.as_deref() else {
            continue;
        };
        let agg = by_node.entry(node).or_default();
        agg.occ += 1;
        if now.duration_since(w.last_heartbeat) > timeout {
            agg.stale += 1;
            if let Some(t) = w.running_build.as_ref().and_then(&tenant_of) {
                agg.tenants.insert(t);
            }
        }
    }
    let mut hung: Vec<String> = by_node
        .into_iter()
        .filter(|(_, a)| a.stale >= 3.max(a.occ.div_ceil(2)) && a.tenants.len() >= 2)
        .map(|(n, _)| n.to_string())
        .collect();
    hung.sort();
    hung
}

/// Request-side filter shared by the Ready and forecast passes of
/// [`DagActor::compute_spawn_intents`]: kind (ADR-019 boundary),
/// per-arch systems intersection (I-107/I-143), I-176/I-181 feature
/// subset + ∅-guard.
fn passes_intent_filter(
    state: &crate::state::DerivationState,
    kind: rio_proto::types::ExecutorKind,
    req: &SpawnIntentsRequest,
) -> bool {
    if req.kind.is_some_and(|k| k != kind) {
        return false;
    }
    if !req.systems.is_empty() && !req.systems.iter().any(|s| s == &state.system) {
        return false;
    }
    if let Some(pf) = req.features.as_deref() {
        if !pf.is_empty() && state.required_features.is_empty() {
            return false;
        }
        if !state.required_features.iter().all(|f| pf.contains(f)) {
            return false;
        }
    }
    true
}

/// Progress-grounded ETA (seconds) for a running dependency.
///
/// `T(c) − elapsed`, clamped at 0 (panel-13 S1). `T(c)` is the
/// dispatch-time `SlaPrediction::wall_secs` — *reference-seconds*
/// (the fit ingests hw-normalized samples and `h_placed` is unknown
/// to the scheduler until the builder reports it on completion). The
/// ref↔wall skew (factor ∈ [0.7, 1.4] across hw classes) is the
/// `eta_error` term the §13b lead-time DDSketch closed-loop absorbs;
/// for §13a the controller filters on `ready` so only the
/// `eta < max_lead` gate is sensitive to it.
///
/// `Assigned` (dispatched, not yet acked → no `running_since`) is
/// treated as `elapsed = 0`. `None` for any branch where
/// `solve_intent_for` produced no fitted-curve prediction (probe /
/// override / cold-start) — a dep without `T(c)` has no
/// progress-grounded ETA, same exclusion as §Forecast's "Queued dep
/// has no progress-grounded ETA".
fn running_dep_eta(dep: &crate::state::DerivationState) -> Option<f64> {
    let t = dep
        .sched
        .last_intent
        .as_ref()?
        .predicted
        .as_ref()?
        .wall_secs?;
    let elapsed = dep
        .running_since
        .map(|r| r.elapsed().as_secs_f64())
        .unwrap_or(0.0);
    Some((t - elapsed).max(0.0))
}

impl DagActor {
    /// Dispatch a read-only [`AdminQuery`].
    pub(super) fn handle_admin(&self, q: AdminQuery) {
        match q {
            AdminQuery::GetSpawnIntents { req, reply } => {
                let _ = reply.send(self.compute_spawn_intents(&req));
            }
            AdminQuery::MintExecutorTokens { intent_ids, reply } => {
                let _ = reply.send(self.mint_executor_tokens(&intent_ids));
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
                let evicted = self.sla_estimator.evict(&key);
                if evicted {
                    self.on_fit_evicted(&key);
                }
                let _ = reply.send(evicted);
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
            AdminQuery::SlaMispredictors { top_n, reply } => {
                let n = if top_n == 0 { 10 } else { top_n as usize };
                let _ = reply.send(self.sla_estimator.top_mispredictors(n));
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
            AdminQuery::SlaHwSampled { hw_classes, reply } => {
                let hw = self.sla_estimator.hw_table();
                let _ = reply.send(
                    hw_classes
                        .into_iter()
                        .map(|h| {
                            let n = hw.distinct_tenants_per_dim(&h);
                            (h, n)
                        })
                        .collect(),
                );
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
        // ONE snapshot of the shared solve inputs for the whole poll —
        // every drv sees the SAME `(hw, cost, inputs_gen)`. Per-drv
        // re-read meant two drvs in one poll could see different
        // `cheapest_h` if `spot_price_poller` wrote between them
        // (latent TOCTOU at the same `inputs_gen`).
        let (hw, cost, inputs_gen) = self.solve_inputs();
        // r[impl sched.sla.forecast.tenant-ceiling]
        // §Threat-model gap (d): per-tenant `max_forecast_cores_per_
        // tenant` budget, debited by Ready cores BEFORE the forecast
        // pass runs. Keyed on `attributed_tenant` (Option<Uuid> —
        // `None` for orphaned/recovered nodes; bucketed together so
        // they're capped, not exempt).
        let mut tenant_forecast_budget: HashMap<Option<Uuid>, i64> = HashMap::new();
        let cap = i64::from(self.sla_config.max_forecast_cores_per_tenant);

        // SpawnIntent constructor shared by Ready + forecast passes.
        // `ready` is the explicit Ready/forecast discriminator —
        // `eta_seconds` is purely the §13b horizon (a forecast intent
        // with overdue deps clamps to 0.0, which would otherwise
        // collide with the Ready filter; bug_030).
        //
        // NO `executor_token` here: `SpawnIntent` is plain data
        // (dashboard/CLI also read it). The credential mints via
        // `MintExecutorTokens` (controller-only) — see
        // `r[sched.sla.threat.read-path-auth]`.
        let to_proto = |drv_hash: &str,
                        state: &crate::state::DerivationState,
                        intent: &SolvedIntent,
                        ready: bool,
                        eta_seconds: f64|
         -> rio_proto::types::SpawnIntent {
            let kind = crate::state::kind_for_drv(state.is_fixed_output);
            rio_proto::types::SpawnIntent {
                intent_id: drv_hash.to_string(),
                cores: intent.cores,
                mem_bytes: intent.mem_bytes,
                disk_bytes: intent.disk_bytes,
                // Compat (proto field 5): controller stamps the full
                // `node_affinity` term-list onto `pod.spec.affinity.
                // nodeAffinity.required…` (r[ctrl.pool.node-affinity-
                // from-intent]); scheduler-side stays empty.
                node_selector: HashMap::new(),
                kind: kind.into(),
                system: state.system.clone(),
                required_features: state.required_features.clone(),
                deadline_secs: intent.deadline_secs,
                node_affinity: intent.node_affinity.clone(),
                eta_seconds,
                ready: Some(ready),
                hw_class_names: intent.hw_class_names.clone(),
                disk_headroom_factor: Some(intent.disk_headroom),
            }
        };

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
            // kind: Unknown (None) = unfiltered. Otherwise must match
            // — the ADR-019 airgap boundary (FOD ⇔ Fetcher) means a
            // Builder pool never sees a Fetcher intent and vice-versa.
            // systems: empty = unfiltered. I-107/I-143 per-arch
            // intersection so an x86-64 pool doesn't spawn for an
            // aarch64-only backlog. features: I-176 subset check +
            // I-181 ∅-guard. `None` = unfiltered (CLI, status
            // display). `Some([])` = featureless pool — only emits
            // intents with empty `required_features`. `Some(pf)` with
            // `pf ≠ ∅` = feature-gated pool — emits intents whose
            // `required_features ⊆ pf ∧ required_features ≠ ∅`
            // (∅-feature work belongs to the featureless pool;
            // dispatch's overflow walk tries cheapest first, so a
            // kvm builder spawned for ∅-feature work would idle until
            // activeDeadlineSeconds).
            if !passes_intent_filter(state, kind, req) {
                continue;
            }

            // r[impl sched.sla.intent-from-solve]
            // ADR-023: per-derivation SpawnIntent. intent_id is the
            // drv_hash itself — the controller stamps it on the pod
            // annotation, the builder echoes it on heartbeat, dispatch
            // matches `worker.intent_id == drv_hash`. No separate
            // intent→drv map to keep in sync; if the drv leaves Ready
            // before the pod heartbeats, the match misses and dispatch
            // falls through to pick-from-queue.
            let intent = self.solve_intent_for(state, &hw, &cost, inputs_gen);
            // gap (d): debit Ready cores from the tenant's forecast
            // budget. A negative balance is fine — the forecast pass
            // checks `> cores`, not `>= 0`.
            let tenant = state.attributed_tenant(&self.builds);
            *tenant_forecast_budget.entry(tenant).or_insert(cap) -= i64::from(intent.cores);
            // ADR-023 §13a affinity is deterministic (memoized) — no
            // selector-pin needed; the controller's `reap_stale_for_
            // intents` sees the SAME fingerprint across re-polls until
            // `inputs_gen` bumps or the ICE mask changes. eta=0 ⇔
            // Ready.
            intents.push((
                state.sched.priority,
                to_proto(drv_hash, state, &intent, true, 0.0),
            ));
        }

        // r[impl sched.sla.forecast.one-layer]
        // ── §13b forecast frontier ────────────────────────────────
        // One DAG layer: a Queued drv whose every incomplete dep is
        // Assigned|Running with `ETA < max_h lead_time[h,cap]`. ETA is
        // max-across-deps of `T(c) − elapsed` ([`running_dep_eta`]).
        // The 1-layer cutoff is structural, not perf: a Queued dep has
        // no progress-grounded ETA, propagating `ETA(B)=ETA(A)+T(B)`
        // compounds σ_resid per hop, and trivial-drv chains would fan
        // out to thousands of intents (ADR-023 §Forecast memo).
        //
        // §13a: `lead_time` is the operator-supplied
        // `lead_time_seed[h,cap]` — the controller-side DDSketch (B7)
        // isn't running yet (ADR L675). Empty seed map ⇒ max_lead=0 ⇒
        // pass disabled (every eta ≥ 0 fails the gate; controller
        // filters on `ready` regardless).
        let max_lead = self
            .sla_config
            .lead_time_seed
            .values()
            .copied()
            .fold(0.0, f64::max);
        if max_lead > 0.0 {
            let mut forecast = Vec::new();
            'q: for (drv_hash, state) in self.dag.iter_nodes() {
                if state.status() != DerivationStatus::Queued {
                    continue;
                }
                let kind = crate::state::kind_for_drv(state.is_fixed_output);
                if !passes_intent_filter(state, kind, req) {
                    continue;
                }
                // 1-layer check: every incomplete dep is Assigned|
                // Running with a fitted-curve ETA. Any Queued/Ready/
                // Substituting/Created/unfitted dep → not
                // forecastable. `had_incomplete` guards the
                // (degenerate) all-deps-satisfied case — that drv
                // belongs to the Ready loop, not here.
                let mut eta = 0.0f64;
                let mut had_incomplete = false;
                for dep_hash in self.dag.get_children(drv_hash) {
                    let Some(dep) = self.dag.node(&dep_hash) else {
                        continue 'q;
                    };
                    match dep.status() {
                        DerivationStatus::Completed | DerivationStatus::Skipped => {}
                        DerivationStatus::Running | DerivationStatus::Assigned => {
                            had_incomplete = true;
                            let Some(d) = running_dep_eta(dep) else {
                                continue 'q;
                            };
                            eta = eta.max(d);
                        }
                        _ => continue 'q,
                    }
                }
                if !had_incomplete || eta >= max_lead {
                    continue;
                }
                let intent = self.solve_intent_for(state, &hw, &cost, inputs_gen);
                forecast.push((drv_hash, state, intent, eta));
            }
            // bug_025: collect → sort → gate. The budget check at this
            // point used to run INSIDE the `'q` loop, i.e. greedy
            // first-fit in `HashMap::iter()` order — same DAG state
            // produced a different admitted subset across restarts, and
            // the post-loop sort can't resurrect what was already
            // dropped. Sort key is `(priority, c*) desc` — the same key
            // §13b @alg-pool's FFD pass walks, so the admitted subset
            // is what FFD wanted first — with `drv_hash` asc as the
            // deterministic tiebreak.
            forecast.sort_unstable_by(|(ha, sa, ia, _), (hb, sb, ib, _)| {
                sb.sched
                    .priority
                    .total_cmp(&sa.sched.priority)
                    .then(ib.cores.cmp(&ia.cores))
                    .then(ha.cmp(hb))
            });
            for (drv_hash, state, intent, eta) in forecast {
                let budget = tenant_forecast_budget
                    .entry(state.attributed_tenant(&self.builds))
                    .or_insert(cap);
                if i64::from(intent.cores) > *budget {
                    continue;
                }
                *budget -= i64::from(intent.cores);
                intents.push((
                    state.sched.priority,
                    to_proto(drv_hash, state, &intent, false, eta),
                ));
            }
        }

        // (Ready, priority)-sort, both descending: `dag.iter_nodes()`
        // is HashMap-order, but the controller truncates to
        // `[..headroom]` under `maxConcurrent` and §13b @alg-pool's
        // FFD pass walks Ready-before-forecast. Unsorted, a
        // high-priority large drv past the prefix gets no pod and
        // fails resource-fit on the small ones spawned for
        // low-priority work (large→small can't overflow; small→large
        // can). With forecast intents tail-sorted, a `[..headroom]`
        // truncation drops forecast first — Ready pods matter more.
        // Keys on `ready` (not `eta_seconds == 0.0`): a forecast
        // intent with overdue deps clamps to eta=0.0 but is NOT Ready
        // (bug_030). Tiebreak `(cores desc, intent_id asc)` —
        // superset of the forecast sort at :471-477 so its order
        // survives within `ready=false`; per REVIEW.md
        // §HashMap-iteration.
        intents.sort_unstable_by(|(pa, ia), (pb, ib)| {
            // `unwrap_or(true)`: a pre-§13a sender omits field 13;
            // pre-§13a only emitted Ready-loop intents (bug_001).
            (ib.ready.unwrap_or(true), *pb)
                .partial_cmp(&(ia.ready.unwrap_or(true), *pa))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(ib.cores.cmp(&ia.cores))
                .then_with(|| ia.intent_id.cmp(&ib.intent_id))
        });

        SpawnIntentsSnapshot {
            intents: intents.into_iter().map(|(_, i)| i).collect(),
            queued_by_system,
            ice_masked_cells: self
                .ice
                .masked_cells()
                .iter()
                .map(crate::sla::config::cell_label)
                .collect(),
            dead_nodes: self.hung_nodes.clone(),
        }
    }

    /// Mint per-intent `ExecutorClaims` tokens for
    /// `AdminService.MintExecutorTokens`. Controller-only — the
    /// credential lives on a controller-only surface so
    /// dashboard/CLI/ComponentScaler never hold it
    /// (`r[sched.sla.threat.read-path-auth]`).
    ///
    /// Reads `(kind, deadline_secs, eta_seconds)` from the current
    /// [`compute_spawn_intents`] snapshot — the controller calls this
    /// immediately after `GetSpawnIntents` so the `SolveCache` is warm
    /// and the second pass is O(dag_nodes) HashMap walk + memo hits.
    /// `intent_ids` not in the snapshot (drv left Ready/Queued between
    /// the two calls) are omitted from the map; the controller spawns
    /// those pods without a token and the scheduler's HMAC verifier
    /// rejects the connection — pod idle-exits, next tick re-spawns.
    /// Empty map when `hmac_signer` is None (dev mode).
    ///
    /// [`compute_spawn_intents`]: Self::compute_spawn_intents
    // r[impl sec.executor.identity-token+2]
    pub(crate) fn mint_executor_tokens(&self, intent_ids: &[String]) -> HashMap<String, String> {
        let Some(signer) = &self.hmac_signer else {
            return HashMap::new();
        };
        let now = rio_auth::now_unix().unwrap_or(0);
        // Unfiltered: same population GetSpawnIntents serves. The
        // controller's request may span Builder+Fetcher pools and
        // Ready+forecast; one snapshot covers both.
        let snap = self.compute_spawn_intents(&SpawnIntentsRequest::default());
        let by_id: HashMap<&str, &rio_proto::types::SpawnIntent> = snap
            .intents
            .iter()
            .map(|i| (i.intent_id.as_str(), i))
            .collect();
        intent_ids
            .iter()
            .filter_map(|id| {
                let intent = by_id.get(id.as_str())?;
                let token = signer.sign(&rio_auth::hmac::ExecutorClaims {
                    intent_id: id.clone(),
                    kind: intent.kind,
                    // `deadline + eta + 5min`: a forecast-spawned pod's
                    // token covers its boot horizon. Preserved verbatim
                    // from the pre-split `to_proto` mint.
                    expiry_unix: now
                        .saturating_add(u64::from(intent.deadline_secs))
                        .saturating_add(intent.eta_seconds as u64)
                        .saturating_add(300),
                });
                Some((id.clone(), token))
            })
            .collect()
    }

    /// Process the controller's spawn ack. `registered_cells`
    /// (`"h:cap"` strings — NodeClaim `Registered=True` edges) reset
    /// ICE backoff; `unfulfillable_cells` (NodeClaim `Launched=False`
    /// or `Registered` timeout) are ICE-marked with exponential
    /// backoff. `spawned` ("the controller created a Job for these")
    /// arms `dispatched_cells` so the §13a heartbeat-edge ICE clear
    /// has a cell to clear — this is the **commit** path; the emit
    /// path (`compute_spawn_intents`) stays read-only so dashboard /
    /// CLI / ComponentScaler polls don't mutate scheduler state.
    /// "Pending Job created" is NOT an ICE-clear signal (clearing on
    /// it defeats backoff doubling: the all-masked fallback re-emits
    /// the masked cell at `[0]`, so each tick would `clear(C)` then
    /// `mark(C)` and `step` never climbed past 0). ADR-023 §Capacity
    /// backoff: the *scheduler* owns ICE state (in-memory,
    /// lease-holder only); the controller reports, the scheduler
    /// decides.
    ///
    /// Until §13b A18 populates `registered_cells`, the §13a interim
    /// success signal is first-heartbeat — see `handle_heartbeat`'s
    /// registration edge.
    // r[impl sched.sla.hw-class.ice-mask]
    pub(super) fn handle_ack_spawned_intents(
        &self,
        spawned: &[rio_proto::types::SpawnIntent],
        unfulfillable_cells: &[String],
        registered_cells: &[String],
    ) {
        // Arm-on-ack: recover the FULL `cells` vec from the parallel
        // `(hw_class_names, node_affinity)` wire form
        // (`cells_to_selector_terms` emits one term per cell). `cap` is
        // the `karpenter.sh/capacity-type` requirement's value.
        // hw-agnostic intents (empty `node_affinity`) skip — no cell
        // to arm. Recording only `cells[0]` (bug_030) is the §1-of-N
        // approximation: the pod's affinity is OR-of-A', so the
        // heartbeat-edge consumer needs the whole set.
        for i in spawned {
            let cells: smallvec::SmallVec<[crate::sla::config::Cell; 4]> = i
                .hw_class_names
                .iter()
                .zip(&i.node_affinity)
                .filter_map(|(h, t)| {
                    let cap = t
                        .match_expressions
                        .iter()
                        .find(|r| r.key == "karpenter.sh/capacity-type")?
                        .values
                        .first()?;
                    Some((h.clone(), crate::sla::config::CapacityType::parse(cap)?))
                })
                .collect();
            if !cells.is_empty() {
                self.dispatched_cells
                    .insert(i.intent_id.as_str().into(), cells);
            }
        }
        for s in registered_cells {
            if let Some(cell) = crate::sla::config::parse_cell(s) {
                self.ice.clear(&cell);
            }
        }
        for s in unfulfillable_cells {
            if let Some(cell) = crate::sla::config::parse_cell(s) {
                self.ice.mark(&cell);
            }
        }
    }

    /// One snapshot of the **shared solve inputs** + the derived
    /// `inputs_gen`. [`Self::compute_spawn_intents`] calls this ONCE at
    /// the top and threads `(&hw, &cost, inputs_gen)` to every
    /// [`Self::solve_intent_for`]; `try_dispatch_one` calls it once per
    /// drv. See [`crate::sla::solve::SolveInputs`] for the "derived,
    /// not bumped" rationale.
    pub(crate) fn solve_inputs(
        &self,
    ) -> (crate::sla::hw::HwTable, crate::sla::cost::CostTable, u64) {
        let hw = self.sla_estimator.hw_table();
        let cost = self.cost_table.read().clone();
        let inputs_gen = crate::sla::solve::SolveInputs {
            hw: &hw,
            cost: &cost,
        }
        .inputs_gen();
        (hw, cost, inputs_gen)
    }

    /// Propagate `SlaEstimator` fit eviction to every paired map. Wired
    /// from BOTH the housekeeping LRU `on_evict` hook and the
    /// `AdminQuery::SlaEvict` handler — one body, two callers, so the
    /// memo's `|live keys| × |overrides|` bound holds AND an operator
    /// `rio-cli sla reset` doesn't leave a Schmitt `prev_a` alive.
    ///
    /// One body, two callers; the [`crate::sla::solve::SolveCache`]
    /// nested keying makes this a single O(1) remove that auto-sweeps
    /// the per-override `MemoEntry` debounce bits AND the no-memo
    /// `infeasible_static_fh` row — no parallel maps to register here.
    pub(crate) fn on_fit_evicted(&self, k: &crate::sla::types::ModelKey) {
        self.solve_cache
            .remove_model_key(crate::sla::solve::model_key_hash(k));
    }

    /// [`SolvedIntent`] for one queued derivation via the SLA estimator.
    /// Shared between [`Self::compute_spawn_intents`] (SpawnIntent
    /// population) and dispatch's resource-fit filter so the controller
    /// spawns and the scheduler accepts the SAME shape.
    ///
    /// When the hw-factor table is populated, the fitted-key branch
    /// routes through the memoized [`solve::solve_full`]
    /// (admissible-set), draws ε_h, applies the read-time ICE mask,
    /// and returns `nodeAffinity` over `A' \ masked`. Otherwise — or
    /// for override/probe/explore branches — it routes through
    /// [`solve::intent_for`] (hw-agnostic `solve_tier`) and returns an
    /// empty affinity.
    // r[impl sched.sla.hw-class.epsilon-explore+6]
    // r[impl sched.sla.hw-class.ice-mask]
    #[tracing::instrument(
        level = "debug",
        skip_all,
        fields(
            pname = state.pname.as_deref().unwrap_or(""),
            tier,
            c_star,
            n_candidates_feasible,
            hw_explore
        )
    )]
    pub(crate) fn solve_intent_for(
        &self,
        state: &crate::state::DerivationState,
        hw: &crate::sla::hw::HwTable,
        cost: &crate::sla::cost::CostTable,
        inputs_gen: u64,
    ) -> SolvedIntent {
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
            tenant: tenant.clone(),
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

        // r[impl sched.sla.hw-class.admissible-set]
        // solve_full path: gated on hw-factor table populated (runtime —
        // bench cold = hw-agnostic) ∧ a usable fit (same n_eff/span gate
        // as intent_for's solve branch — probe/explore stay on the
        // hw-agnostic path). ANY override field
        // (`forced_cores`/`forced_mem`/`tier`) also gates it off:
        // solve_full doesn't take `override_`, so those fall through to
        // `intent_for` which honors all three. (bug_033: `forced_mem`
        // was previously overlaid post-solve → affinity menu-checked at
        // fit-mem, request at forced-mem → permanently-Pending pod.)
        //
        // FOD / required_features / serial drvs MUST stay hw-agnostic:
        // the `rio-fetcher` and `rio-builder-metal` NodePools carry no
        // hw-class labels — affinity over `{hw-class:X}` matches zero
        // templates and the pod is permanently Pending. Serial drvs
        // additionally need `intent_for`'s 1-core pin
        // (`r[sched.sla.intent-from-solve]`); solve_full ignores
        // `hints` and would multi-core a `enableParallelBuilding=false`
        // build.
        //
        // Sorted: ε_h's seeded `pool.choose()` indexes into this Vec,
        // so HashMap iteration order would otherwise leak into the
        // "pure function of drv_hash" contract.
        let mut h_all: Vec<_> = self.sla_config.hw_classes.keys().cloned().collect();
        h_all.sort_unstable();
        // `was_miss`: first time this `(model_key, inputs_gen)` was
        // solved. Gates the post-memo metric emits whose inputs ARE in
        // `inputs_gen` — `BestEffort.why`, ε_h's `_hw_cost_unknown` — so
        // cache-hits don't re-emit per poll. NOT a valid gate for emits
        // depending on read-time state (`ice.masked_cells()`) or for
        // drvs that never enter the hw-aware path (FOD/featured/serial/
        // cold-hw-table); those use `memo_entry`'s debounce fields /
        // `infeasible_static_fh` instead. `hw_emitted` tracks "hw-aware
        // arm already emitted" for the double-count suppress.
        let mut was_miss = false;
        let mut hw_emitted = false;
        // `(mkh, ovr, entry-clone)` when the memo was reached. `Some`
        // iff `full`'s closure ran past `get_or_insert_with` — i.e. the
        // hw-aware gate held AND the fit passed the n_eff/span/!Probe
        // gate. Read for the debounce-prev values; written back via
        // `update_entry` on edge.
        let mut memo_entry: Option<(u64, u64, solve::MemoEntry)> = None;
        let full = (!hw.is_empty()
            && override_.as_ref().is_none_or(|o| !o.bypasses_solve_full())
            && hints.prefer_local_build != Some(true)
            && hints.enable_parallel_building != Some(false)
            && !state.is_fixed_output
            && state.required_features.is_empty())
        .then_some(())
        .and_then(|()| {
            let f = fit.as_ref()?;
            // R6B4: `!Probe ⟹ n_eff_ring≥3 ∧ span≥4` (ingest.rs:310
            // sets `Probe` iff either fails) — the dropped `n_eff` /
            // `span||frozen` clauses were redundant pre-r5-R5B8 and
            // WRONG after it (read post-filter `fit_df=2` and rejected
            // a valid Capped fit).
            if matches!(f.fit, crate::sla::types::DurationFit::Probe) {
                return None;
            }
            // Memo: keyed on (model_key_hash, override_hash); hit iff
            // (inputs_gen, fit_content_hash) both match. The ε_h draw
            // and ICE mask are applied AFTER reading the memo — never
            // overwriting `result`. `was_miss` gates the
            // memo-input-dependent emits; `entry`'s debounce fields
            // gate the read-time-state ones.
            let mkh = solve::model_key_hash(&f.key);
            let ovr = solve::override_hash(override_.as_ref());
            let (entry, miss) = self.solve_cache.get_or_insert_with(
                mkh,
                ovr,
                inputs_gen,
                solve::fit_content_hash(f),
                |prev_a| {
                    solve::solve_full(
                        f,
                        &self.sla_tiers,
                        hw,
                        cost,
                        &self.sla_ceilings,
                        &self.sla_config,
                        &h_all,
                        prev_a,
                        true,
                    )
                },
            );
            was_miss = miss;
            let result = entry.result.clone();
            memo_entry = Some((mkh, ovr, entry));
            let memo = match result {
                solve::SolveFullResult::Feasible(m) => Some(m),
                solve::SolveFullResult::BestEffort { why, .. } => {
                    if was_miss {
                        why.emit(&tenant);
                    }
                    hw_emitted = true;
                    None
                }
            };
            // ε_h draw (OUTSIDE memo): pin one h ∉ A (or
            // H \ {argmin price} on miss / A=H), restrict the solve
            // to `(h_explore, *)`, and emit ITS A' if feasible. The
            // cached memo's `A` is read but never overwritten.
            //
            // §Fifth-strike: the pin lifecycle (draw, persist,
            // release) is owned by `explore::resolve_h_explore` — see
            // its doc for the full state machine. The per-drv `rng`
            // here governs ONLY the coin (which drvs explore); the pin
            // VALUE is seeded from `mkh ^ ovr` inside that function so
            // it's iteration-order-independent (bug_004) and shared by
            // every same-key drv.
            use rand::{RngExt as _, SeedableRng};
            let seed = {
                use std::hash::{DefaultHasher, Hash, Hasher};
                let mut h = DefaultHasher::new();
                state.drv_hash.as_str().hash(&mut h);
                h.finish()
            };
            let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
            if h_all.len() > 1 && rng.random::<f64>() < self.sla_config.hw_explore_epsilon {
                use crate::sla::explore::{
                    HExploreCtx, HExploreOutcome, HExplorePin, h_explore_pool, resolve_h_explore,
                };
                let in_a: std::collections::HashSet<_> = memo
                    .as_ref()
                    .map(|m| m.a.cells.iter().map(|(h, _)| h.clone()).collect())
                    .unwrap_or_default();
                let cheapest = cost.cheapest_h(&h_all);
                let pool = h_explore_pool(&h_all, &in_a, cheapest.as_ref());
                let masked = self.ice.masked_cells();
                let ctx = HExploreCtx {
                    pool: &pool,
                    masked: &masked,
                };
                // `(pinned_explore, pinned_explore_a)` → one
                // `HExplorePin`; transition owned by `resolve_h_explore`
                // — see [`HExplorePin`] doc.
                let prev = memo_entry
                    .as_ref()
                    .map(|(_, _, e)| HExplorePin {
                        h: e.pinned_explore.clone(),
                        prev_a: e.pinned_explore_a.clone(),
                    })
                    .unwrap_or_default();
                let outcome = resolve_h_explore(prev, mkh, ovr, &ctx, |h, prev_a| {
                    tracing::Span::current().record("hw_explore", h.as_str());
                    solve::solve_full(
                        f,
                        &self.sla_tiers,
                        hw,
                        cost,
                        &self.sla_ceilings,
                        &self.sla_config,
                        std::slice::from_ref(h),
                        prev_a,
                        // Unmemoized — `_hw_cost_unknown_total` already
                        // emitted for the full `h_all × cap` space by
                        // the unrestricted solve at :809; `{h} ⊆ h_all`
                        // so emitting here double-counts. (`was_miss`
                        // was the previous guard; it correctly
                        // suppressed cache-HIT polls but permitted a 2×
                        // emit on the miss poll itself.)
                        false,
                    )
                });
                // Idempotent memo write — same class as the
                // `update_entry` for `ice_exhausted`. Guard on EITHER
                // half of the pin changing. `memo_entry` is an OWNED
                // clone (solve.rs `.cloned()` drops the DashMap guard)
                // — no re-entrancy.
                let commit = |pin: &HExplorePin| {
                    if let Some((_, _, prev)) = &memo_entry
                        && (prev.pinned_explore != pin.h || prev.pinned_explore_a != pin.prev_a)
                    {
                        self.solve_cache.update_entry(mkh, ovr, |e| {
                            e.pinned_explore = pin.h.clone();
                            e.pinned_explore_a = pin.prev_a.clone();
                        });
                    }
                };
                match outcome {
                    HExploreOutcome::Hit { memo, pin } => {
                        commit(&pin);
                        return Some((memo, h_all.clone()));
                    }
                    HExploreOutcome::Miss { pin } => {
                        commit(&pin);
                        // Fall through to the unrestricted memo. `pin.h`
                        // gets its one solve on the NEXT ε_h hit.
                    }
                }
            }
            memo.map(|m| (m, h_all.clone()))
        });

        let (cores, mem, disk, node_affinity, hw_class_names, full_tier) = match full {
            Some((memo, h_all)) => {
                tracing::Span::current()
                    .record("tier", memo.tier.as_str())
                    .record("c_star", memo.a.c_star)
                    .record("n_candidates_feasible", memo.all_candidates.len());
                // Read-time ICE mask: A \ masked. Never empty — fall
                // back to A if all of A is masked (the controller will
                // see `unfulfillable` again and the backoff doubles;
                // emitting an empty affinity would land hw-agnostic
                // which §Capacity backoff reserves for envelope-
                // infeasibility).
                let masked = self.ice.masked_cells();
                let cells: Vec<_> = memo
                    .a
                    .cells
                    .iter()
                    .filter(|c| !masked.contains(c))
                    .cloned()
                    .collect();
                // R5B2: ICE-edge debounce. `was_miss` is the wrong
                // gate — `ice.masked_cells()` is read-time state,
                // explicitly NOT in `inputs_gen` (see
                // `SolveInputs::inputs_gen` doc). ICE marks accumulate
                // on controller-tick cadence (~5s); under
                // `hwCostSource: static`, `inputs_gen` may never change
                // → metric silent. Track the conjunction `A\masked = ∅
                // ∧ ice.exhausted(H)` per memo-key and emit on its
                // rising edge. The conjunction (not `cells.is_empty()`
                // alone): A-unmask ≠ exhaustion-clear; the original
                // `if was_miss && exhausted` gated the conjunction, the
                // debounce must track the same predicate. `memo_entry`
                // is always Some here (this arm is only reachable past
                // `get_or_insert_with`).
                let (mkh, ovr, prev) = memo_entry
                    .as_ref()
                    .expect("full=Some ⇒ get_or_insert_with ran");
                let now_exh = cells.is_empty() && self.ice.exhausted(&h_all);
                if now_exh && !prev.ice_exhausted {
                    ::metrics::counter!(
                        "rio_scheduler_sla_hw_ladder_exhausted_total",
                        "tenant" => tenant.clone(),
                        "exit" => "all_masked",
                    )
                    .increment(1);
                    solve::InfeasibleReason::CapacityExhausted.emit(&tenant);
                }
                if now_exh != prev.ice_exhausted {
                    self.solve_cache
                        .update_entry(*mkh, *ovr, |e| e.ice_exhausted = now_exh);
                }
                let cells = if cells.is_empty() {
                    memo.a.cells
                } else {
                    cells
                };
                // Capacity-type pin: filter A' to the operator's cap.
                // If A' ∩ {cap} = ∅ (solve admitted only the OTHER cap
                // — e.g. spot-only on cost), fall back to
                // `all_candidates` ∩ {cap}: every (h, cap) solve_full
                // evaluated, feasible or not. Honors the pin even when
                // it conflicts with the cost-optimal set; c*/mem/disk
                // stay at A's argmin (approximate but operator-
                // intentional).
                let cells = match override_.as_ref().and_then(|o| o.capacity) {
                    Some(cap) => {
                        let pinned: Vec<_> = cells.into_iter().filter(|(_, c)| *c == cap).collect();
                        if pinned.is_empty() {
                            memo.all_candidates
                                .iter()
                                .filter(|c| c.cell.1 == cap)
                                .map(|c| c.cell.clone())
                                .collect()
                        } else {
                            pinned
                        }
                    }
                    None => cells,
                };
                // `dispatched_cells` is NOT armed here — that's a state
                // write on the emit path (dashboard/CLI/ComponentScaler
                // also poll this), and budget-reject / cancel /
                // substitute / never-Ready forecast drvs would all leak.
                // Armed on the controller's ack instead
                // (`handle_ack_spawned_intents`); each `cells[i]` round-
                // trips via `(hw_class_names[i], node_affinity[i].cap-type)`.
                let (terms, names) =
                    solve::cells_to_selector_terms(&cells, &self.sla_config.hw_classes);
                (
                    memo.a.c_star,
                    memo.a.mem_bytes,
                    memo.a.disk_bytes,
                    terms,
                    names,
                    Some(memo.tier),
                )
            }
            None => {
                let solve::IntentDecision {
                    cores: c,
                    mem: m,
                    disk: d,
                    infeasible,
                } = solve::intent_for(
                    fit.as_ref(),
                    &hints,
                    override_.as_ref(),
                    &self.sla_config,
                    &self.sla_tiers,
                    &self.sla_ceilings,
                );
                // R5B3/R7B1: `intent_for` fallback's `_infeasible_total`
                // anchor. `intent_for` is pure — `infeasible.is_some()`
                // iff execution reached past every hints/override/
                // explore early-return, so the debounce records iff the
                // emit WOULD fire (bug 035: recording before the call
                // let a serial drv burn the slot then early-return).
                // `hw_emitted` stays load-bearing: with-memo BestEffort
                // emits at :821 and falls through to here, so the
                // `Some(..)` memo_entry arm is structurally unreachable
                // (`memo_entry.is_some() ⟹ hw_emitted`) — with-memo
                // debounce lives at :821 via `was_miss`; no-memo via
                // `infeasible_static_fh`. No-memo anchor is
                // once-per-`(mkh, ovr, fit_content_hash)` via
                // `infeasible_static_fh`: refit re-arms; stable across
                // polls.
                if let Some(reason) = infeasible
                    && !hw_emitted
                {
                    let ovr = solve::override_hash(override_.as_ref());
                    let seen = fit.as_ref().is_some_and(|f| {
                        self.solve_cache.infeasible_static_seen(
                            solve::model_key_hash(&f.key),
                            ovr,
                            solve::fit_content_hash(f),
                        )
                    });
                    if !seen {
                        reason.emit(&tenant);
                    }
                }
                // mb_053(a) / V-5: `--capacity` on the bypass path. The
                // `Some(memo)` arm filters `cells` to `cap` post-memo,
                // but ANY bypass field gates `full=None` and lands here
                // with empty `(terms, names)`. Empty `hw_class_names` →
                // controller's `cells_of` is empty → `fallback_cell`
                // returns `(ref_h, Spot)` ignoring the pin → cover
                // provisions spot, the cap-type term refuses spot, pod
                // never schedules. Populate `(terms, names)` from
                // `[(ref_h, cap)]` via the same `cells_to_selector_terms`
                // so the controller derives the pinned cell instead of
                // falling back. No `o.capacity` → empty as before.
                let (terms, names) = match override_.as_ref().and_then(|o| o.capacity) {
                    Some(cap) => solve::cells_to_selector_terms(
                        &[(self.sla_config.reference_hw_class.clone(), cap)],
                        &self.sla_config.hw_classes,
                    ),
                    None => (Vec::new(), Vec::new()),
                };
                (c, m, d, terms, names, None)
            }
        };
        // r[impl sched.sla.reactive-floor+2]
        // D4: floor AND ceiling at the single post-solve chokepoint.
        // Floor: a derivation that OOM'd at its solved mem had
        // `bump_floor_or_count` double `floor.mem`; the next solve
        // returns at least that. Ceiling: `intent_for`'s early-return
        // branches (forced/serial/local/explore) pass fit-derived /
        // override bytes through unclamped, so the `solve_tier` /
        // `solve_full` BestEffort clamp doesn't cover them — a
        // `disk_p90` (or `--mem` / `--cores`) above a
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
        // ADR-023 §sizing: variance-aware overlay-disk headroom. The
        // curve lives in `sla::fit::headroom` (scheduler-only); the
        // controller is a dumb consumer via
        // `SpawnIntent.disk_headroom_factor`. Unfitted → flat 1.5×.
        let disk_headroom = fit
            .as_ref()
            .map(|f| crate::sla::fit::headroom(f.n_eff_ring))
            .unwrap_or(1.5);
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
                let t = f.fit.t_at(RawCores(f64::from(cores))).0 / hw.min_factor(f.alpha);
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
                // mb_053: project via the SAME `effective_tiers` as
                // `intent_for` so a `--p*` override records the
                // operator's target, not the config-ladder one (which
                // emitted false `envelope_result_total{result=miss}`).
                // `tier_target` reads from `tiers`, NOT `self.sla_tiers`,
                // so the recorded bound is the one dispatch sized for.
                let cow;
                let tiers = match override_.as_ref() {
                    Some(o) => {
                        cow = o.effective_tiers(&self.sla_tiers);
                        &*cow
                    }
                    None => &*self.sla_tiers,
                };
                let target = |name: &str| {
                    tiers
                        .iter()
                        .find(|t| t.name == name)
                        .and_then(solve::Tier::binding_bound)
                };
                let (tier, tier_target) = if no_tier {
                    (None, None)
                } else if let Some(tier) = full_tier.as_deref() {
                    (Some(tier.to_owned()), target(tier))
                } else {
                    // hw-agnostic arm ⇒ re-run `solve_tier` (pure) for
                    // the tier name. The admissible-set arm carries
                    // `full_tier` directly so this only fires when
                    // gates routed away from solve_full.
                    match solve::solve_tier(f, tiers, &self.sla_ceilings) {
                        solve::SolveResult::Feasible { tier, .. } => {
                            let t = target(&tier);
                            (Some(tier.clone()), t)
                        }
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
            node_affinity,
            hw_class_names,
            disk_headroom,
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
