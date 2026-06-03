//! Read-only snapshot/inspect handlers on [`DagActor`]. All methods
//! here are `&self` over the in-memory DAG and back the admin RPCs
//! (ClusterStatus, GetSpawnIntents, InspectBuildDag).

use std::collections::HashMap;

use uuid::Uuid;

use crate::state::{BuildState, DerivationStatus, DrvHash, SolvedIntent};

use super::{
    AdminQuery, AuthBinding, ClusterSnapshot, DagActor, SpawnIntentsRequest, SpawnIntentsSnapshot,
};

/// §13e + r35: thin accessor for the drv's stored
/// [`DerivationState::effective_features`] field. The derivation moved
/// from this free fn (called at 5 spawn-intent-path sites) to a
/// constructor invariant on `DerivationState` (§nth-strike STRIKE-3:
/// each round of "the chokepoint isn't total" added another caller of
/// this fn; each missed the next site — `assignment.rs` dispatch,
/// `pool_covers`, the recovery row constructors). The biconditional
/// `is_fixed_output ⟺ ∋ fetcher` is now enforced at construction +
/// `set_required_features` write-gate, so this accessor cannot
/// observe a stale or unstrut value.
///
/// Kept as a free fn so the 5 spawn-intent call sites stay textually
/// unchanged. Returns an owned `Vec<String>` (the call sites assign it
/// to `SpawnIntent.required_features` / `DrvHints.required_features`
/// or pass `&feat` to `features_compatible`).
///
/// The TWO intentional bypasses read the in-memory normalized set via
/// `state.required_features()`: `handle_inspect_build_dag`'s
/// `required_features` field and `actor/dispatch.rs`'s `failed_builders`
/// warn. Operator-facing diagnostic echo — but NOT the verbatim
/// declared set: it is post I-204 soft-strip (the verbatim declaration
/// lives only in the `derivations.required_features` PG column). What
/// it omits relative to this fn is the §13e FOD↔fetcher derivation,
/// which IS what the operator needs to see when triaging a routing
/// surprise (`is_fixed_output ⟹ [fetcher]` regardless of what was
/// declared).
fn effective_features(state: &crate::state::DerivationState) -> Vec<String> {
    state.effective_features().as_slice().to_vec()
}

/// Request-side filter shared by the Ready and forecast passes of
/// [`DagActor::compute_spawn_intents`]: kind, per-arch systems
/// intersection (I-107/I-143), I-176/I-181 feature subset + ∅-guard.
///
/// Axis ownership (post-§13e): the ADR-019 FOD↔Fetcher airgap is
/// enforced here by the **features** clause via the
/// `effective_features` chokepoint (`is_fixed_output ⟹ [fetcher]`).
/// The **kind** clause is NOT the spawn-side airgap — it stays because
/// (a) the CLI/dashboard "all Builder intents" query sets `kind` with
/// `features: None`, and (b) `kind` is the *dispatch-time* airgap
/// (`assignment.rs::hard_filter` reads *declared*, not effective,
/// features). For the controller's `queued_for_pool` poll — which
/// always sets both `kind` and `features = effective_features(spec)` —
/// the kind clause is informationally redundant with the features
/// axis.
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
        // §13c: same canonical predicate as the hwClass routing
        // (T2/T10/D10) so the worker filter and the scheduler's
        // routing agree — drift here would route a kvm intent to a
        // metal cell whose worker then rejects it. §13e: reads the
        // *effective* features (FOD ⟹ [fetcher]) so a fetcher-pool
        // poll (`features: [fetcher]`) passes FODs through and a
        // featureless builder pool rejects them — without this, B2's
        // controller-side `effective_features(spec)` derives `[fetcher]`
        // for the fetcher pool, the FOD's raw `required_features=[]`
        // fails the ∅-guard, and FODs become unroutable.
        if !crate::sla::config::features_compatible(&effective_features(state), pf) {
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
/// `eta_error` term the §13b lead-time-sketch closed-loop absorbs;
/// for §13a the controller filters on `ready` so only the
/// `eta < max_lead` gate is sensitive to it.
///
/// `Assigned` (dispatched, not yet acked → no `running_since`) is
/// treated as `elapsed = 0`. `None` for any branch where
/// `solve_intent_for` produced no fitted-curve prediction (probe /
/// override / cold-start) — a dep without `T(c)` has no
/// progress-grounded ETA, same exclusion as §Forecast's "Queued dep
/// has no progress-grounded ETA".
/// Remaining ETA for a Running/Assigned dep at `now`. `now` is
/// snapshotted ONCE at the top of [`DagActor::compute_spawn_intents`]'s
/// forecast loop so every drv in one poll sees the SAME notion of
/// elapsed — two drvs depping on the same Running node get IDENTICAL
/// `eta`, not microsecond-jittered values that would break the
/// `(prio, c*, eta, hash)` budget sort tiebreak's determinism (r33
/// bug_007 added `eta` to the key; bug_025 requires it deterministic).
fn running_dep_eta(dep: &crate::state::DerivationState, now: std::time::Instant) -> Option<f64> {
    let t = dep
        .sched
        .last_intent
        .as_ref()?
        .predicted
        .as_ref()?
        .wall_secs?;
    let elapsed = dep
        .running_since
        // `saturating_duration_since`: `running_since > now` is
        // impossible in production (set on transition→Running, read
        // later), but `0.0` is a safe clamp under test backdating.
        .map(|r| now.saturating_duration_since(r).as_secs_f64())
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
            AdminQuery::InspectBuildDag { build_id, reply } => {
                let _ = reply.send(self.handle_inspect_build_dag(build_id));
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
    /// O(builds + dag_nodes) per call. The autoscaler polls every 30s;
    /// even with 10k active derivations that's ~300μs/call — not worth
    /// maintaining incremental counters. Revisit if dashboards start
    /// polling at 1Hz.
    ///
    /// Every count is DAG-status / build-state derived. The executor
    /// counts are the busy view: one open pull-mode attempt per
    /// `Assigned|Running` derivation, one attempt per pod, so the
    /// in-flight derivation count IS the busy-executor count. The
    /// scheduler holds no registration state for pull-mode pods
    /// (spawned-but-not-yet-pulled pods are the controller's Job
    /// census), and per-executor drain retired with the stream session,
    /// so `draining_executors` is always 0.
    ///
    /// `as u32` casts: if any collection exceeds 4B entries, truncation
    /// is the LEAST of our problems.
    pub(super) fn compute_cluster_snapshot(&self) -> ClusterSnapshot {
        let mut pending_builds = 0u32;
        let mut active_builds = 0u32;
        for b in self.builds.values() {
            match b.state() {
                BuildState::Pending => pending_builds += 1,
                BuildState::Active => active_builds += 1,
                // Terminal builds stay in the map until CleanupTerminalBuild
                // (after TERMINAL_CLEANUP_DELAY, ~60s). Don't count them —
                // they're not "active"
                // in any autoscaling sense. Unspecified never appears
                // (proto3 default-0; scheduler always sets a real state).
                BuildState::Succeeded
                | BuildState::Failed
                | BuildState::Cancelled
                | BuildState::Unspecified => {}
            }
        }

        // Running = Assigned | Running. Both mean "a worker slot is taken."
        // Assigned is the just-minted pull attempt; for "how busy are
        // workers" they're equivalent.
        let mut running_derivations = 0u32;
        let mut build_executors = 0u32;
        let mut queued_derivations = 0u32;
        let mut substituting_derivations = 0u32;
        let mut queued_by_system: HashMap<String, u32> = HashMap::new();
        // One park-expiry comparison instant for the whole pass (the
        // has_claimable_job "now"); per-node Instant::now() would let
        // a park expire mid-pass and double-count across buckets.
        let bucket_now = std::time::Instant::now();
        // r[impl sched.admin.snapshot-substituting+3]
        // Exhaustive over DerivationStatus so a future variant addition
        // is a compile-time break here, not a silently-zero autoscaler
        // input.
        //
        // The substituting bucket is job-derived (§2.6): a node with an
        // unresolved unclaimed materialization job is substitution
        // backlog whatever its status.
        for (drv_hash, s) in self.dag.iter_nodes() {
            match s.status() {
                DerivationStatus::Assigned | DerivationStatus::Running => {
                    // The running bucket is kind-blind by design (the
                    // derivation IS being worked); the EXECUTOR view
                    // below is builders-only (A2.4, bug_217) — a
                    // materialization claim holds a store replica's
                    // walk slot, not a builder pod. Single
                    // kind-to-surface source: the open_attempt_kind
                    // captured at the mint
                    // (r[sched.pull.kinded-running-surface]).
                    running_derivations += 1;
                    if s.open_attempt_kind != Some(crate::state::AttemptKind::Materialization) {
                        build_executors += 1;
                    }
                }
                DerivationStatus::Ready => {
                    // r[impl sched.materialize.job+2]
                    // §2.6: a Ready node carrying an unresolved,
                    // unclaimed materialization job is substitution
                    // backlog, not builder-queue backlog — count it in
                    // the substituting bucket and keep it OUT of
                    // queued_derivations/queued_by_system so the buckets
                    // stay disjoint and builder autoscalers don't scale
                    // on work that will be materialized, not built.
                    if self.has_claimable_job(drv_hash, bucket_now) {
                        substituting_derivations += 1;
                    } else if self.has_pending_unclaimed_job(drv_hash) {
                        // Parked job (bug_252): pacing, not claimable
                        // demand — counted in NEITHER bucket so the
                        // KEDA store trigger drains while the node
                        // stays out of the builder bucket (it will be
                        // materialized, not built). Visible via
                        // rio_scheduler_materialization_stalled.
                    } else {
                        // The scalar and the I-107 per-system breakdown
                        // are counted in the same arm so the sum across
                        // keys equals the scalar by construction (the
                        // ready-queue membership the scalar used to read
                        // was not dequeued by pull mints — the recorded
                        // over-count).
                        queued_derivations += 1;
                        *queued_by_system.entry(s.system.clone()).or_default() += 1;
                    }
                }
                // Pre-ready: not yet store/builder load. Created has no
                // deps probed; Queued has unmet deps. Neither drives
                // any RPC traffic — EXCEPT a Queued node carrying a
                // pending materialization job: materialization does
                // not wait for deps, so that node is store-side
                // backlog exactly like its Ready sibling above.
                DerivationStatus::Created | DerivationStatus::Queued => {
                    // bug_252: claimable only — a parked Queued node is
                    // neither store demand (KEDA must drain) nor
                    // builder demand (it will be materialized).
                    if s.status() == DerivationStatus::Queued
                        && self.has_claimable_job(drv_hash, bucket_now)
                    {
                        substituting_derivations += 1;
                    }
                }
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
            // The busy view: one open BUILD attempt per builder pod
            // (P0537); materialization claims are excluded (A2.4 —
            // store-side work holds no builder slot).
            total_executors: build_executors,
            active_executors: build_executors,
            draining_executors: 0,
            pending_builds,
            active_builds,
            queued_derivations,
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
                // 124(b): the cycle this intent was computed against.
                // The controller echoes it on NoEligibleSource so a
                // verdict that raced a resubmit reset is detectable.
                resubmit_cycle: u64::from(state.retry.resubmit_cycles),
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
                // §13e: wire form carries the EFFECTIVE features so
                // the controller's spawn-decision query and the
                // scheduler agree on which Pool serves the intent
                // (FOD ⟹ [fetcher] ⟹ fetcher Pool's effective set).
                required_features: effective_features(state),
                deadline_secs: intent.deadline_secs,
                node_affinity: intent.node_affinity.clone(),
                eta_seconds,
                ready: Some(ready),
                hw_class_names: intent.hw_class_names.clone(),
                disk_headroom_factor: Some(intent.disk_headroom),
                // AD2: the node-keyed entries of the exclusion set, so
                // the controller can render anti-affinity and evaluate
                // the spawn-gate exhaustion check. Empty for histories
                // with only legacy (pod-name-keyed) failures.
                // r[impl sched.dispatch.fleet-exhaust+5]
                excluded_nodes: state.excluded_source_nodes(),
            }
        };

        for (drv_hash, state) in self.dag.iter_nodes() {
            if state.status() != DerivationStatus::Ready {
                continue;
            }
            // r[impl sched.materialize.job+2]
            // PD-7 (Phase B, design §2.3): nodes with an unresolved
            // materialization job are never spawn-intent candidates —
            // the controller must not spawn builder pods for work that
            // will be materialized. Excluded BEFORE the per-system
            // aggregate so GetSpawnIntents.queued_by_system stays
            // coherent with ClusterSnapshot.queued_by_system (the §2.6
            // bucket exclusion's controller-facing twin); retires the
            // CE-59 spawn-intent churn class as a side effect. Claimed
            // jobs' nodes are Assigned/Running and already excluded by
            // the status check above.
            if self.has_pending_unclaimed_job(drv_hash) {
                continue;
            }
            // Per-system aggregate: counted BEFORE the kind/feature
            // filters so it matches `ClusterSnapshot.queued_by_system`
            // (the ComponentScaler reads this independent of which
            // pool asked).
            *queued_by_system.entry(state.system.clone()).or_default() += 1;

            // r[impl sched.admin.spawn-intents.probed-gate+3]
            // A materialization success's consumption promotes
            // dependents Queued→Ready with their probe deferred to
            // the next Tick. A poll in that ≤1s window would spawn
            // pods that get reaped 10s later when the probe finds
            // them substitutable. probed_generation==0 ⇔ "never
            // probed since insert/recovery".
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
            // annotation, the builder presents it on `PullAssignment`,
            // and the pull mint resolves `intent_id == drv_hash`. No
            // separate intent→drv map to keep in sync; if the drv
            // leaves Ready before the pod pulls, the mint answers
            // Gone/NotYetReady instead.
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

        // r34 bug_018 STRIKE-3: debounce gate for
        // `forecast_dropped_total` — `true` once per `(drv_hash,
        // reason)` edge per LRU residency. The forecast loop runs per
        // `compute_spawn_intents` call (~3 callers per scheduler
        // tick); without this gate the counter reads
        // `(poll_rate)×(stuck drvs)` not `(drop events)`, violating
        // the `ONCE_PER_MISS` contract. Same `LruCache` shape as
        // `unroutable_features_warned` / `cap_mismatch_warned`. The
        // `counter!` stays at each emit site (not folded into the
        // closure) so the `"reason" => "<literal>"` pairs remain
        // statically scannable by `labeled_metric_values_have_emit_
        // sites`.
        let forecast_dropped_first = |actor: &Self, drv_hash: &str, reason: &'static str| -> bool {
            actor
                .forecast_dropped_warned
                .lock()
                .put((drv_hash.to_owned(), reason), ())
                .is_none()
        };

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
        // §13a/§13b: `lead_time` is the operator-supplied
        // `lead_time_seed[h,cap]`. The controller-side lead-time sketch
        // (`CellSketches`, §13b) IS running, but
        // `AckSpawnedIntentsRequest` has no per-cell `lead_time`
        // return channel — the scheduler stays on the static seed
        // (`max_lead_for`'s "Seed-based approximation" caveat). Empty
        // seed map ⇒ max_lead=0 ⇒ pass disabled (every eta ≥ 0 fails
        // the gate; controller filters on `ready` regardless).
        // (r34 merged_bug_006)
        //
        // r33 bug_007 §Granularity-coupling: `max_lead` is the GLOBAL
        // max — it gates the whole pass on/off. The per-intent
        // admission gate is `max_lead_for(system, features)` (pre-
        // solve, over the `class_routes`-admissible classes) and a
        // post-solve mirror over `intent.hw_class_names` (the cells
        // the controller's `a_open` actually evaluates). Pre-fix, the
        // global max gated each intent: r31's metal `lead_time_seed=
        // 600` raised the forecast horizon 30× for non-metal intents
        // the controller would drop at its per-cell `a_open`.
        let max_lead = self
            .sla_config
            .lead_time_seed
            .values()
            .copied()
            .fold(0.0, f64::max);
        if max_lead > 0.0 {
            let mut forecast = Vec::new();
            // ONE `now` for the whole forecast pass: every dep's ETA
            // is measured against the same instant so siblings sharing
            // a Running dep get IDENTICAL `eta` (sort-key determinism;
            // see [`running_dep_eta`]).
            let now = std::time::Instant::now();
            'q: for (drv_hash, state) in self.dag.iter_nodes() {
                if state.status() != DerivationStatus::Queued {
                    continue;
                }
                // r[impl sched.materialize.job+2]
                // PD-7: the same unresolved-job exclusion as the Ready
                // pass — a Queued node with a pending materialization
                // job will be materialized (the PD-6 dep-racing claim),
                // so forecasting a builder pod for it is exactly the
                // churn the filter exists to prevent.
                if self.has_pending_unclaimed_job(drv_hash) {
                    continue;
                }
                let kind = crate::state::kind_for_drv(state.is_fixed_output);
                if !passes_intent_filter(state, kind, req) {
                    continue;
                }
                // 1-layer check: every incomplete dep is Assigned|
                // Running with a fitted-curve ETA. Any Queued/Ready/
                // Created/unfitted dep → not
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
                            let Some(d) = running_dep_eta(dep, now) else {
                                continue 'q;
                            };
                            eta = eta.max(d);
                        }
                        _ => continue 'q,
                    }
                }
                if !had_incomplete {
                    continue;
                }
                // Pre-solve coarse gate (perf): horizon over the
                // `class_routes`-admissible classes — saves the
                // `solve_intent_for` call for an intent the controller
                // would unconditionally drop. Over-approximates
                // (compatible classes ⊇ actual cells; size ceiling
                // unknown pre-solve); the post-solve gate catches the
                // residual.
                let intent_lead = self
                    .sla_config
                    .max_lead_for(&state.system, &effective_features(state));
                if eta >= intent_lead {
                    if forecast_dropped_first(self, drv_hash, "lead_horizon") {
                        ::metrics::counter!(
                            "rio_scheduler_sla_forecast_dropped_total",
                            "reason" => "lead_horizon",
                        )
                        .increment(1);
                    }
                    continue;
                }
                let intent = self.solve_intent_for(state, &hw, &cost, inputs_gen);
                // Post-solve exact gate: seed-based approximation of
                // the controller's `a_open` per-cell filter
                // (`eta < lead_time(c)`), re-stated scheduler-side
                // over the SOLVED `hw_class_names` (r34 merged_bug_006:
                // the controller reads its learned per-cell sketch
                // quantile, which has no return channel here — see
                // [`crate::sla::config::SlaConfig::max_lead_for`]).
                // The pre-solve gate used the arch+features-routable
                // superset; this is the cells `solve_intent_for`
                // actually emitted (post tier walk, post ceiling).
                // `hw_class_names = []` (hw-agnostic / featureless
                // probe path) skips the gate — there is no per-cell
                // lead to check; the controller's `a_open`
                // short-circuits to `fallback_cell`.
                if !intent.hw_class_names.is_empty() {
                    let cell_lead = self
                        .sla_config
                        .lead_time_seed
                        .iter()
                        .filter(|((h, _), _)| intent.hw_class_names.contains(h))
                        .map(|(_, &v)| v)
                        .fold(0.0, f64::max);
                    if eta >= cell_lead {
                        if forecast_dropped_first(self, drv_hash, "lead_horizon") {
                            ::metrics::counter!(
                                "rio_scheduler_sla_forecast_dropped_total",
                                "reason" => "lead_horizon",
                            )
                            .increment(1);
                        }
                        continue;
                    }
                }
                forecast.push((drv_hash, state, intent, eta));
            }
            // bug_025: collect → sort → gate. The budget check at this
            // point used to run INSIDE the `'q` loop, i.e. greedy
            // first-fit in `HashMap::iter()` order — same DAG state
            // produced a different admitted subset across restarts, and
            // the post-loop sort can't resurrect what was already
            // dropped. Sort key is `(priority, c*) desc` — the same key
            // §13b @alg-pool's FFD pass walks, so the admitted subset
            // is what FFD wanted first — then `eta` asc (r33 bug_007:
            // under budget pressure, near-term actionable intents win
            // over far-term ones the controller may still strip at its
            // per-cell sketch quantile, which can drift below the
            // operator seed) — then `drv_hash` asc as the deterministic
            // tiebreak.
            forecast.sort_unstable_by(|(ha, sa, ia, ea), (hb, sb, ib, eb)| {
                sb.sched
                    .priority
                    .total_cmp(&sa.sched.priority)
                    .then(ib.cores.cmp(&ia.cores))
                    .then(ea.total_cmp(eb))
                    .then(ha.cmp(hb))
            });
            for (drv_hash, state, intent, eta) in forecast {
                let budget = tenant_forecast_budget
                    .entry(state.attributed_tenant(&self.builds))
                    .or_insert(cap);
                if i64::from(intent.cores) > *budget {
                    if forecast_dropped_first(self, drv_hash, "tenant_budget") {
                        ::metrics::counter!(
                            "rio_scheduler_sla_forecast_dropped_total",
                            "reason" => "tenant_budget",
                        )
                        .increment(1);
                    }
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
        // (bug_030). Tiebreak `(cores desc, eta asc, intent_id asc)` —
        // superset of the forecast sort above so its order
        // survives within `ready=false`; per REVIEW.md
        // §HashMap-iteration. `eta` is a no-op for Ready (always 0.0).
        intents.sort_unstable_by(|(pa, ia), (pb, ib)| {
            // `unwrap_or(true)`: a pre-§13a sender omits field 13;
            // pre-§13a only emitted Ready-loop intents (bug_001).
            (ib.ready.unwrap_or(true), *pb)
                .partial_cmp(&(ia.ready.unwrap_or(true), *pa))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(ib.cores.cmp(&ia.cores))
                .then(ia.eta_seconds.total_cmp(&ib.eta_seconds))
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
    // r[impl sec.executor.identity-token+3]
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
    /// arms `dispatched_cells` so the §13a first-pull ICE clear
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
    /// success signal is the first successful pull — see the mint's
    /// ICE-clear in `actor/pull.rs`.
    // r[impl sched.sla.hw-class.ice-mask]
    pub(super) fn handle_ack_spawned_intents(
        &mut self,
        spawned: &[rio_proto::types::SpawnIntent],
        unfulfillable_cells: &[String],
        registered_cells: &[String],
        observed_instance_types: &[rio_proto::types::ObservedInstanceType],
        bound_intents: &[rio_proto::types::BoundIntent],
        binding_snapshot: Option<&[rio_proto::types::BoundIntent]>,
    ) {
        // Kube-authoritative `intent_id (== drv_hash) → (spec.nodeName,
        // tenant)`. The nodeclaim_pool reconciler ships the FULL set
        // every tick as an EXPLICIT snapshot (`binding_snapshot`,
        // C2/285): `Some(set)` — even empty — wholesale-rebuilds
        // (present-and-empty correctly CLEARS the map: the
        // scale-to-zero tick has zero bound pods and says so); `None`
        // = "this Ack carries no snapshot" (per-pool reconcilers, and
        // pre-upgrade controllers on the legacy field-5 arm below) =
        // no-op (mb_012/⛔2: an unconditional `mem::take` here would
        // discard every captured `tenant` on every per-pool
        // reconcile). The legacy arm keeps the OLD semantics —
        // non-empty `bound_intents` rebuilds — for rolling skew (R9:
        // read-side back-compat only, never dual-written).
        //
        // `tenant` is captured from the DAG when present, else carried
        // forward from the existing entry — once DAG-absent the last
        // DAG-present value sticks. A fall-through executor's
        // spawn-drv leaves the DAG but the Ack keeps shipping its
        // `intent_id` while the pod lives.
        // 124(d): record the spawn-ack witness for EVERY spawned
        // intent — a NoEligibleSource verdict landing within the defer
        // window raced its own spawn. Opportunistic prune keeps the
        // map bounded (entries older than 2× the window are dead: the
        // defer read only consults the window).
        if !spawned.is_empty() {
            let now = crate::db::attempts::epoch_now();
            for i in spawned {
                self.acked_spawned
                    .insert(DrvHash::from(i.intent_id.as_str()), now);
            }
            self.acked_spawned
                .retain(|_, t| now - *t < 2.0 * crate::actor::pull::ACKED_SPAWNED_DEFER_SECS);
        }
        // r[impl sched.snapshot.binding-presence]
        let snapshot: Option<&[rio_proto::types::BoundIntent]> = match binding_snapshot {
            Some(snap) => Some(snap),
            None if !bound_intents.is_empty() => Some(bound_intents),
            None => None,
        };
        if let Some(snap) = snapshot {
            let prev = std::mem::take(&mut self.authoritative_binding);
            for b in snap {
                let h: DrvHash = b.intent_id.as_str().into();
                let tenant = self
                    .dag
                    .node(&h)
                    .and_then(|n| n.attributed_tenant(&self.builds))
                    .or_else(|| prev.get(&h).and_then(|p| p.tenant));
                self.authoritative_binding.insert(
                    h,
                    AuthBinding {
                        node: b.node_name.clone(),
                        tenant,
                        // Wire `0` = absent (pre-upgrade controller):
                        // the mint falls back to its re-solve alone.
                        deadline_secs: (b.deadline_secs > 0).then_some(b.deadline_secs),
                    },
                );
            }
        }
        // Arm-on-ack: recover the FULL `cells` vec from the parallel
        // `(hw_class_names, node_affinity)` wire form
        // (`cells_to_selector_terms` emits one term per cell). `cap` is
        // the `karpenter.sh/capacity-type` requirement's value.
        // hw-agnostic intents (empty `node_affinity`) skip — no cell
        // to arm. Recording only `cells[0]` (bug_030) is the §1-of-N
        // approximation: the pod's affinity is OR-of-A', so the
        // first-pull consumer needs the whole set.
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
        // Third writer to `cost_table` (after `fold_spot_poll`→price
        // and `interrupt_housekeeping`→λ/node_count). Gate on the
        // shared edge-reload latch like `spot_price_poller` does:
        // before `interrupt_housekeeping` has run the lease-acquire
        // `*cost.write() = CostTable::load(...)`, writes here would
        // land on the pre-reload table and be clobbered. The
        // controller's `observe_registered` is edge-detected +
        // recency-gated, so a clobbered observation isn't re-sent
        // until another NodeClaim of that type registers.
        // `handle_leader_acquired` notifies `interrupt_housekeeping`
        // so this gate is open within ~0s of lease win, not ≤600s.
        if !observed_instance_types.is_empty()
            && self
                .cost_was_leader
                .load(std::sync::atomic::Ordering::Relaxed)
        {
            self.cost_table.write().observe_instance_types(
                observed_instance_types.iter().filter_map(|o| {
                    Some((
                        crate::sla::config::parse_cell(&o.cell)?,
                        o.instance_type.clone(),
                        o.cores,
                        o.mem_bytes,
                    ))
                }),
            );
        }
    }

    /// One snapshot of the **shared solve inputs** + the derived
    /// `inputs_gen`. Both consumers — [`Self::compute_spawn_intents`]
    /// and `dispatch_ready` — call this ONCE at the top of their pass
    /// and thread `(&hw, &cost, inputs_gen)` to every
    /// [`Self::solve_intent_for`] (r33 bug_013 hoisted dispatch's
    /// per-drv call). See [`crate::sla::solve::SolveInputs`] for the
    /// "derived, not bumped" rationale.
    pub(crate) fn solve_inputs(
        &self,
    ) -> (crate::sla::hw::HwTable, crate::sla::cost::CostTable, u64) {
        #[cfg(test)]
        self.test_counters
            .solve_inputs_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let hw = self.sla_estimator.hw_table();
        let cost = self.cost_table.read().clone();
        // §13c-2 r[impl scheduler.sla.ceiling.uncatalogued-fallback]:
        // per-tick gauge — 1 for any class without a boot-derived
        // catalog ceiling (Static cost source, fetch failure, or
        // requirements that match 0 types). Such a class falls to the
        // global ceiling. Emitted here (the once-per-pass boundary —
        // both callers hoist this above their drain loop) so it
        // tracks the live `cost.catalog_ceilings()` snapshot the solve
        // reads, including across `carry_catalog` lease-acquire
        // reloads.
        for h in self.sla_config.hw_classes.keys() {
            let v = u8::from(!cost.catalog_ceilings().contains_key(h));
            ::metrics::gauge!(
                "rio_scheduler_sla_class_ceiling_uncatalogued",
                "hw_class" => h.clone()
            )
            .set(v);
        }
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
        // §13e: bind the EFFECTIVE feature set once per call. Every
        // routing read below (h_all partition, override_hash memo key,
        // retain_hosting_cells, unroutable warn, DrvHints) uses this,
        // not the raw declaration — see `effective_features`'s doc.
        let feat = effective_features(state);
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
            required_features: feat.clone(),
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
        // Serial drvs MUST stay hw-agnostic: they need `intent_for`'s
        // 1-core pin (`r[sched.sla.intent-from-solve]`); solve_full
        // ignores `hints` and would multi-core a
        // `enableParallelBuilding=false` build.
        //
        // r[impl sched.sla.hwclass.provides]
        // §13c: `required_features` drvs are NO LONGER hw-agnostic —
        // they route to hwClasses with matching `provides_features` via
        // the bidirectional ∅-guard, so kvm intents get full SLA-solve
        // participation on metal cells instead of the static
        // pre-§13c static metal NodePool bypass. `h_all` is partitioned
        // accordingly: a class providing `[kvm]` is excluded for
        // featureless intents (so metal doesn't absorb non-kvm), and a
        // class providing `[]` is excluded for kvm intents.
        //
        // §13e: FODs are no longer hw-agnostic either — `effective_
        // features` projects `is_fixed_output` to `[fetcher]`, which
        // partitions `h_all` to the `fetcher-*` classes via the same
        // ∅-guard. They participate in solve_full like any featured
        // intent (the cost model converges on the floor for the
        // unsaturated CPU profile). The static `rio-fetcher` NodePool
        // is DELETED (§13e); the controller mints `fetcher-*`
        // NodeClaims from the cells emitted here.
        //
        // §13d STRIKE-7 (bug_042, A8): ALSO arch-filter — `h_all`
        // feeds `solve_full`'s candidate set; a wrong-arch class gets a
        // wasted `evaluate_cell` and (worse) leaks into `memo.a.cells`.
        // The post-finalize `retain_hosting_cells` chokepoint also
        // arch-filters, so a strip there is a *producer regression
        // signal* — filtering here keeps the chokepoint a backstop, not
        // a per-intent log-spam source.
        //
        // Sorted: ε_h's seeded `pool.choose()` indexes into this Vec,
        // so HashMap iteration order would otherwise leak into the
        // "pure function of drv_hash" contract.
        let want_arch = rio_common::k8s::system_to_k8s_arch(&state.system);
        let mut h_all: Vec<_> = self
            .sla_config
            .hw_classes
            .iter()
            .filter(|(_, d)| {
                crate::sla::config::features_compatible(&feat, &d.provides_features)
                    && want_arch.is_none_or(|a| {
                        d.labels
                            .iter()
                            .find(|l| l.key == crate::sla::config::ARCH_LABEL)
                            .is_none_or(|l| l.value == a)
                    })
            })
            .map(|(h, _)| h.clone())
            .collect();
        h_all.sort_unstable();
        // §one-step-removed inverse: `required_features` non-empty but
        // NO hwClass provides them → unschedulable forever (silent).
        // Surface it once per `(tenant, required_features)` edge so the
        // operator notices without per-drv per-poll spam (this block
        // sits BEFORE `was_miss` and never reaches the memo when
        // `h_all=[]`, so none of the post-memo debounce gates apply —
        // it has its own set on the actor; mb_031). Counter carries
        // ONLY `tenant` (bounded by `Claims.sub`): the per-feature
        // label was tenant-controlled (verbatim `requiredSystemFeatures`,
        // unclamped) → unbounded Prometheus cardinality on shared
        // monitoring (REVIEW.md §Threat-model). The unroutable feature
        // strings still reach the operator via the structured-log
        // `features` field below, which is rate-bounded by the same
        // debounce edge.
        // Clamp the debounce key to bound per-ENTRY heap growth: the
        // raw strings are tenant-controlled (`requiredSystemFeatures`,
        // already clamped at translate.rs's `strings_clamped` — this
        // is a defense-in-depth second line behind the gateway trust
        // boundary); 64 entries × 32 chars; collisions dedup at
        // ASCII-truncate,
        // which only over-debounces (fail-safe). The entry COUNT is
        // bounded by the LRU cap on the set itself (mb_001) — the
        // clamp alone leaves the doc's own threat case `["x-${uuid}"]`
        // open at the cardinality level.
        // `.put()` returns `Some` iff the key was already present;
        // `.is_none()` is the "first edge" predicate (mirrors
        // `HashSet::insert`'s `true`-on-new).
        const MAX_KEY_FEATURES: usize = 64;
        let key_features: Vec<String> = feat
            .iter()
            .take(MAX_KEY_FEATURES)
            .map(|f| f.chars().filter(|c| c.is_ascii()).take(32).collect())
            .collect();
        if h_all.is_empty()
            && !feat.is_empty()
            && self
                .unroutable_features_warned
                .lock()
                .put((tenant.clone(), key_features), ())
                .is_none()
        {
            ::metrics::counter!(
                "rio_scheduler_unroutable_features_total",
                "tenant" => tenant.clone(),
            )
            .increment(1);
            tracing::warn!(
                %tenant,
                features = ?feat,
                system = %state.system,
                "no hwClass for this system provides required_features — \
                 intent unroutable; add a `provides_features` entry to an \
                 arch-compatible `[sla.hw_classes.$h]`",
            );
        }
        // `was_miss`: first time this `(model_key, inputs_gen)` was
        // solved. Gates the post-memo metric emits whose inputs ARE in
        // `inputs_gen` — `BestEffort.why`, ε_h's `_hw_cost_unknown` — so
        // cache-hits don't re-emit per poll. NOT a valid gate for emits
        // depending on read-time state (`ice.masked_cells()`) or for
        // drvs that never enter the hw-aware path (serial/cold-hw-table/
        // unroutable); those use `memo_entry`'s debounce fields /
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
        // §13e D2: the pre-§13e `&& !state.is_fixed_output` gate is
        // GONE — FODs route to fetcher cells via the `[fetcher]`
        // partition of `h_all`, so they participate in solve_full like
        // any featured intent. The cost model fits a flat elapsed/cores
        // curve and converges on the floor.
        let full = (!hw.is_empty()
            && override_.as_ref().is_none_or(|o| !o.bypasses_solve_full())
            && hints.prefer_local_build != Some(true)
            && hints.enable_parallel_building != Some(false)
            && !h_all.is_empty())
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
            //
            // §13c: `ovr` folds in `required_features` — h_all is now
            // feature-partitioned, so the SAME ModelKey with DIFFERENT
            // features gets a DIFFERENT h_all → different solve result.
            // Without this, a kvm and non-kvm drv sharing pname would
            // cache-hit on each other's (wrong-partition) memo.
            // §13e: hashes the EFFECTIVE features so a FOD and a
            // non-FOD sharing pname (degenerate but possible) memo
            // separately.
            let mkh = solve::model_key_hash(&f.key);
            let ovr = solve::override_hash(override_.as_ref(), &feat);
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

        let (cores, mem, disk, cells, full_tier) = match full {
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
                let now_exh = cells.is_empty()
                    && self
                        .ice
                        .exhausted(&h_all, |h| self.sla_config.capacity_types_for(h).to_vec());
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
                            // r27-A4 / STRIKE-6: `all_candidates` is
                            // every cell whose OWN c* fit its class;
                            // the SHARED `a.c_star` may not. Producer-
                            // side filter via the canonical
                            // `class_ceilings`; the post-finalize
                            // chokepoint below is the backstop.
                            memo.all_candidates
                                .iter()
                                .filter(|c| c.cell.1 == cap)
                                .filter(|c| {
                                    let (cc, cm) = self.sla_config.class_ceilings(
                                        &c.cell.0,
                                        cost.catalog_ceilings(),
                                        cost.resolved_global(),
                                    );
                                    memo.a.c_star <= cc && memo.a.mem_bytes <= cm
                                })
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
                (
                    memo.a.c_star,
                    memo.a.mem_bytes,
                    memo.a.disk_bytes,
                    cells,
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
                    let ovr = solve::override_hash(override_.as_ref(), &feat);
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
                // r40 bug_025: pre-clamp BEFORE `bypass_cells`. The
                // producer-side `reference_hw_class_for_system` size
                // filter and the post-finalize `retain_hosting_cells`
                // chokepoint must agree on `(cores, mem)` —
                // `retain_hosting_cells` is filter-only (can drop,
                // can't add), so an over-cap override that makes the
                // producer reject every class yields
                // `node_affinity=[]`, and the chokepoint can't
                // recover. With `[]` affinity, a featured intent's pod
                // lands without the feature affinity and crashloops
                // (no `RioNodeclaimPoolNoHostingClass` alert: the
                // controller's `fallback_cell` reads the POST-clamp
                // cores and succeeds). The shared chokepoint clamp at
                // the end of `solve_intent_for` re-applies the same
                // bounds — this pre-clamp is idempotent under it.
                let c = c.min(self.sla_ceilings.max_cores as u32).max(1);
                let m = m
                    .max(state.sched.resource_floor.mem_bytes)
                    .min(self.sla_ceilings.max_mem);
                let cells = self.bypass_cells(
                    state,
                    override_.as_ref().and_then(|o| o.capacity),
                    c,
                    m,
                    cost,
                    &tenant,
                );
                (c, m, d, cells, None)
            }
        };
        // r[impl sched.sla.reactive-floor+3]
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
        // §13e (was mb_023): `is_fixed_output ⟺ features ∋ fetcher` —
        // the `effective_features` chokepoint projects the role
        // discriminator onto the feature axis, so `retain_hosting_
        // cells` validates cell hosting AND consumer kind through one
        // predicate (the bidirectional ∅-guard: a `[fetcher]` intent
        // only retains `provides ∋ fetcher` cells, and a featureless
        // intent never sees them). A FOD whose effective features lack
        // `fetcher`, or a non-FOD with `fetcher`, means a producer arm
        // bypassed the chokepoint — that's the gap this tripwire
        // closes (the pre-§13e mb_023 tripwire asserted `cells = []`
        // for FODs; that invariant inverts now that FODs route).
        debug_assert_eq!(
            state.is_fixed_output,
            feat.iter().any(|f| f == rio_common::k8s::FETCHER_FEATURE),
            "FOD ⟺ features ∋ fetcher invariant broken (§13e, was mb_023): \
             is_fixed_output={}, effective_features={feat:?} — a producer \
             arm bypassed `effective_features`",
            state.is_fixed_output,
        );
        // STRIKE-7 (r30 §13d): single post-finalize chokepoint. Both
        // arms above converge here with finalized `(cores, mem)` and a
        // `Vec<Cell>` (BEFORE `cells_to_selector_terms` so the
        // capacity-type axis is still typed input, not a string buried
        // in a `NodeSelectorTerm`). This is downstream of every
        // `hw_class_names` producer (the `solve_full` re-filter, the
        // `all_candidates` capacity-fallback, the no-memo
        // `bypass_cells` reference-class lookup). A new producer-hole
        // would require a SpawnIntent construction site that bypasses
        // `solve_intent_for` entirely.
        let cells = self.sla_config.retain_hosting_cells(
            cells,
            &state.system,
            (cores, mem),
            &feat,
            cost.catalog_ceilings(),
            cost.resolved_global(),
        );
        let (node_affinity, hw_class_names) =
            solve::cells_to_selector_terms(&cells, &self.sla_config.hw_classes);
        // D7: deadline_secs. Fitted ⇒ `wall_p99 × 5` (p99 of the
        // log-normal `T(c)·exp(ε)` at the chosen cores, no retry tail
        // — k8s-kill-then-reactive-floor IS the retry). Unfitted
        // (probe/explore/override-with-no-fit) ⇒ `[sla].probe.
        // deadline_secs` — or the matching `feature_probes` entry, same
        // lookup `explore::next` uses for the cores/mem ladder. The
        // fitted-path `q99×5` is FLOORED at the probe deadline: a
        // sub-second fit (trivial-builders) would otherwise yield
        // `activeDeadlineSeconds≈3`, killing the Job before the pod
        // ever pulls — with no pull there's no attempt row to
        // classify, so `bump_floor_or_count`
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

    /// Bypass-path (`full = None`, no memo) cells for the no-`solve_full`
    /// arm of [`Self::solve_intent_for`]. Reached for serial,
    /// override-bypass (`--cores`/`--mem`/`--tier`), `prefer_local`,
    /// cold-start `fit=None`/`Probe`, and empty-`h_all` drvs.
    ///
    /// Extracted (`pub(super)`) so contract tests can assert the
    /// pre-chokepoint cell set directly: the `cap ∉ capacity_types_for(h)`
    /// gate (mb_003) is observationally indistinguishable from the
    /// post-finalize `retain_hosting_cells` strip on `intent.hw_class_names`
    /// — both produce `[]` — so a `solve_intent_for`-level test cannot
    /// be red-first for the producer fix (r31 A1, §Kani-extract-predicate).
    ///
    /// §13e (was mb_023): the FOD hoist (`is_fixed_output ⟹ []`) is
    /// GONE. FODs reach `bypass_cells` cold-start with
    /// `effective_features = [fetcher]` and route to the `fetcher-*`
    /// classes via `reference_hw_class_for_system` like any featured
    /// intent. The static `rio-fetcher` NodePool is DELETED (§13e);
    /// the controller mints `fetcher-*` NodeClaims from the cells
    /// emitted here.
    pub(super) fn bypass_cells(
        &self,
        state: &crate::state::DerivationState,
        cap: Option<crate::sla::config::CapacityType>,
        cores: u32,
        mem: u64,
        cost: &crate::sla::cost::CostTable,
        tenant: &str,
    ) -> Vec<crate::sla::config::Cell> {
        // §13e: bind the EFFECTIVE feature set once. FOD ⟹ [fetcher].
        let feat = effective_features(state);
        let mem = mem.max(state.sched.resource_floor.mem_bytes);
        // mb_053(a) / V-5: `--capacity` on the bypass path. The
        // `Some(memo)` arm filters `cells` to `cap` post-memo, but ANY
        // bypass field gates `full=None` and lands here with empty
        // cells. Empty `hw_class_names` → controller's `cells_of` is
        // empty → `fallback_cell` returns `(ref_h, Spot)` ignoring the
        // pin → cover provisions spot, the cap-type term refuses spot,
        // pod never schedules. Populate cells from `[(ref_h, cap)]` so
        // the controller derives the pinned cell instead of falling
        // back.
        match cap {
            // bug_039: `reference_hw_class` may have a
            // `kubernetes.io/arch` label that doesn't match
            // `state.system` — emitting it would AND the pod's
            // `nodeSelector.arch=arm64` with `nodeAffinity arch In
            // [amd64]` → permanently Pending. Arch-match like the
            // controller's `fallback_cell` does. On `None` (no class
            // hosts this arch at this size, or unmappable system), emit
            // empty so the controller's `fallback_cell` reaches its OWN
            // `None` → `no_hosting_class` metric.
            //
            // r40 bug_025: `(cores, mem)` MUST be the caller's
            // post-clamp values, not raw `intent_for` output. The
            // post-finalize `retain_hosting_cells` chokepoint is
            // filter-only — it cannot recover cells the producer
            // rejected on a pre-clamp `cores > ceiling`, so the
            // producer-side size filter and the chokepoint must agree
            // on the demand they evaluate. `solve_intent_for`'s `None`
            // arm pre-clamps before calling here. (Pre-r40 this
            // comment claimed "the post-finalize chokepoint catches
            // the post-clamp delta" — that claim IS the bug.)
            Some(cap) => match self.sla_config.reference_hw_class_for_system(
                &state.system,
                cores,
                mem,
                &feat,
                cost.catalog_ceilings(),
                cost.resolved_global(),
            ) {
                // mb_003: gate `cap` on `capacity_types_for(h)`,
                // mirroring the `None` arm. Without it, `--cores=16
                // --capacity=spot` on a kvm pname emits `(metal-x86,
                // Spot)` on an od-only class → `retain_hosting_cells`
                // strips (`cap_ok=false`) → chokepoint `warn!` per drv
                // per poll for the override TTL, defeating the
                // "strip = regression" contract at config.rs.
                Some(h) if self.sla_config.capacity_types_for(h).contains(&cap) => {
                    vec![(h.to_owned(), cap)]
                }
                // r31 A3: the operator's `--capacity` pin names a cap
                // the reference class doesn't host — silent drop is a
                // diagnostic blind spot (the override looks applied but
                // the pin is ignored). Debounced WARN names the fix.
                Some(h) => {
                    let pname = state.pname.clone().unwrap_or_default();
                    if self
                        .cap_mismatch_warned
                        .lock()
                        .put((tenant.to_owned(), pname, cap), ())
                        .is_none()
                    {
                        let hosted = self.sla_config.capacity_types_for(h);
                        tracing::warn!(
                            %tenant,
                            pname = state.pname.as_deref().unwrap_or(""),
                            ?cap,
                            h,
                            ?hosted,
                            "`--capacity` override pin not hosted by the \
                             reference hwClass for this drv — pin ignored, \
                             cells emitted empty; change the pin to a \
                             hosted cap, or add the cap to the class's \
                             `[sla.hw_classes.<h>].capacity_types`",
                        );
                    }
                    Vec::new()
                }
                None => Vec::new(),
            },
            // §13d STRIKE-7 (mb_012, A9): cold-start featured intent
            // (`fit=None`, no override). Pre-fix this arm returned `[]`
            // → controller's `fallback_cell` / FFD `agnostic_arch`
            // picked a non-metal cell → kvm pod CrashLoopBackOff on
            // ENXIO `/dev/kvm` (no metal node minted; pool-static
            // nodeSelector deleted r33 bug_002) → no `build_sample` →
            // `fit` stays `None` → bootstrap deadlock.
            // Emit cells for every configured cap of the reference
            // class so the controller mints the matching cell.
            // Featureless intents stay `[]` (genuinely hw-agnostic;
            // controller's `fallback_cell` arch-matches).
            // §13e: `feat = [fetcher]` for FODs, so cold-start FODs
            // land here and route to `fetcher-*` — the same
            // chicken-egg fix as kvm-cold-start.
            None if !feat.is_empty() => self
                .sla_config
                .reference_hw_class_for_system(
                    &state.system,
                    cores,
                    mem,
                    &feat,
                    cost.catalog_ceilings(),
                    cost.resolved_global(),
                )
                .map_or_else(Vec::new, |h| {
                    self.sla_config
                        .capacity_types_for(h)
                        .iter()
                        .map(|cap| (h.to_owned(), *cap))
                        .collect()
                }),
            None => Vec::new(),
        }
    }

    // r[impl sched.admin.inspect-dag+2]
    /// Actor in-memory snapshot of a build's derivations. The stream-era
    /// cross-reference against the live stream pool (the I-025
    /// `executor_has_stream` diagnostic) retired with the executors map:
    /// a pull-mode in-flight derivation's executor identity is its open
    /// attempt, so `live_executor_ids` is now the set of executor ids
    /// the DAG currently has work assigned to, and `executor_has_stream`
    /// is simply "this derivation has an in-flight assignment" (kept
    /// only for wire-shape compatibility until the 1d proto sweep; a
    /// stuck attempt is bounded by `activeDeadlineSeconds` plus the
    /// establishment sweep, not by stream liveness).
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
                // In-flight = the assignment is live in the DAG. The
                // stream-pool membership check this replaced cannot be
                // asked of a pull-mode pod.
                let executor_has_stream = s.assigned_executor.is_some()
                    && matches!(
                        s.status(),
                        DerivationStatus::Assigned | DerivationStatus::Running
                    );
                let backoff_remaining_secs = s
                    .retry
                    .backoff_until
                    .and_then(|deadline| deadline.checked_duration_since(now))
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                // The per-executor placement-rejection simulation
                // (I-062) retired with the placement layer: there is no
                // dispatch decision to explain — a Ready derivation
                // waits for the controller to spawn its pod and for
                // that pod to pull. The proto field stays empty until
                // the 1d proto sweep.
                let rejections = Vec::new();
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
                    // §13e + r35: intentional bypass — the operator
                    // needs to see what the tenant DECLARED, not what
                    // the chokepoint derived. The derived set is a
                    // routing artifact.
                    required_features: s.required_features().to_vec(),
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
        // The executor identities the DAG currently has work assigned
        // to (the pull-mode "live" set: one open attempt per entry).
        let live_executor_ids = self
            .dag
            .iter_nodes()
            .filter(|(_, s)| {
                matches!(
                    s.status(),
                    DerivationStatus::Assigned | DerivationStatus::Running
                )
            })
            .filter_map(|(_, s)| s.assigned_executor.as_ref().map(|e| e.to_string()))
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        (derivations, live_executor_ids)
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
