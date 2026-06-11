//! Read-only snapshot/inspect handlers on [`DagActor`]. All methods
//! here are `&self` over the in-memory DAG and back the admin RPCs
//! (ClusterStatus, GetSpawnIntents, InspectBuildDag).

use std::collections::HashMap;

use uuid::Uuid;

use crate::state::{BuildState, DerivationStatus, DrvHash, SolvedIntent};

/// Which autoscaler bucket a Ready derivation belongs to — see
/// [`DagActor::classify_ready_node`] (bug_129: the ONE classifier both
/// aggregate surfaces read).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReadyClass {
    /// Unresolved CLAIMABLE materialization job: substitution backlog
    /// (the store trigger's demand), never builder demand.
    Substituting,
    /// Unresolved unclaimed job that is not claimable right now
    /// (parked or deferred): store pacing — counted in NEITHER bucket
    /// (bug_252). Only the PARKED half is visible via the stalled
    /// gauge; a DEFERRED job (defer_until, bounded <=300s) is counted
    /// in no gauge for that window (m032 — the accepted blind spot,
    /// stated in the substituting_derivations HELP).
    ParkedPacing,
    /// Builder-queue demand: counted in `queued_by_system` on both
    /// surfaces, whatever the retry-backoff state.
    Queued,
}

use super::{
    AdminQuery, AuthBinding, ClusterSnapshot, DagActor, SpawnIntentsRequest, SpawnIntentsSnapshot,
};

/// §13e + r35: thin accessor for the drv's stored
/// `DerivationState::effective_features` field. The derivation moved
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

/// §13b substitution-face ETA prior (seconds): the typed contribution
/// of a dependency whose unresolved materialization job is
/// store-ACTIVE (live_049 lever 3 / WO-S7-R F1 — the "blind
/// materialization minute": pre-F1, a substituting dep yielded NO
/// forecast contribution, so a warm-cache run's parents emitted their
/// first intents only at readiness and paid the full node-provisioning
/// lead cold).
///
/// R17 envelope: VIOLABLE + testable, derivation recorded. Value = the
/// live_049 blind-minute evidence scale (substitutions complete on the
/// ~60 s scale; the binding pre-fix error was the EXCLUSION — an
/// effectively infinite eta — never estimate precision). STATIC by
/// design: the scheduler retains neither claim timestamps nor byte
/// progress for materialization jobs (`ReportMaterializationProgress`
/// is a display-only relay through the event ring —
/// `handle_substitute_progress` — not retained state), so the prior
/// cannot decay in-flight. Error model — the same `eta_error`
/// absorption family as [`running_dep_eta`]'s ref↔wall skew:
///
/// - short substitutions: over-estimate bounded by the prior; the
///   forecast intent still emits the whole blind window earlier than
///   pre-fix (never later), and the controller's per-cell `a_open`
///   re-gates every poll;
/// - wedged claims (live_055(a)): under-estimate, self-healing — the
///   intent re-emits each poll until the job resolves or parks (park ⇒
///   the typed pacing exclusion takes over);
/// - cells with `lead_time < prior`: the lead-horizon gate keeps
///   dropping the intent (correct by the gate law); the un-recovered
///   notice is bounded by that cell's own lead. In-flight decay needs
///   progress retention — a cross-plane change deliberately not taken
///   this round (see the §13b lead-time comment in
///   [`DagActor::compute_spawn_intents`]).
pub(super) const SUBSTITUTING_DEP_ETA_PRIOR_SECS: f64 = 60.0;

/// Which active face contributed the substitution prior — provenance
/// for tests/diagnostics. The wire carries only `eta_seconds`
/// (`SpawnIntent` is unchanged); the controller's §13c consumption is
/// source-agnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SubstActiveFace {
    /// A store replica holds the claim (fetch executing).
    Claimed,
    /// Unclaimed but admittable right now: the next worker beat
    /// (~1 s poll, ≤1.2 s jittered — `LISTING_STEAL_HORIZON`'s
    /// calibration note) claims it; claim latency is noise against
    /// the prior.
    ClaimableNow,
}

/// Why a pacing job blocks forecastability — the typed exclusion's
/// provenance axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SubstPacingFace {
    /// Durable park backoff (scale: minutes — at/above lead horizons).
    Parked,
    /// View-only transient deferral (≤300 s, uncharged 429/raced —
    /// `RETRY_LATER_MAX_DEFER_SECS`).
    Deferred,
}

/// live_049 lever 3 (WO-S7-R F1): the TOTAL typed disposition of a
/// dependency's materialization job for the §13b forecast dep walk —
/// the substitution face's sibling alphabet to [`CellEmission`] (R21:
/// the dep-admission chokepoint's terminal dispositions are typed and
/// censused; the pacing drop joins the censused
/// `rio_scheduler_sla_forecast_dropped_total{reason}` value set).
///
/// Deliberately NOT a `CellEmission` arm: the dep eta is poll-time
/// data and `solve_intent_for`'s emission is memoized per `inputs_gen`
/// (hw+cost only) — an eta embedded in the emission alphabet would
/// freeze in the memo exactly like the merged_bug_002 overlay class.
/// Poll-time facts attach ABOVE the memo, where the dep walk computes.
///
/// Derived from the one armament source (`claimability`, bug_170) plus
/// the view-hydration axis; consumed by exactly one fold (the dep
/// walk) with zero wildcard arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SubstDepEta {
    /// Store-active job: the dep resolves on the store plane within
    /// [`SUBSTITUTING_DEP_ETA_PRIOR_SECS`] — contribute the typed
    /// prior.
    Active(SubstActiveFace),
    /// Pacing job (parked/deferred): not active store work within any
    /// lead horizon — the parent is not forecastable through this dep;
    /// the drop is typed + counted (`reason="substituting_pacing"`).
    Pacing(SubstPacingFace),
    /// No unresolved job entry for this dep.
    NoJob,
    /// Job view unhydrated (post-failover recovery window): job
    /// knowledge unavailable — fail closed to the pre-F1 status
    /// disposition, uncounted (counting would assert job knowledge
    /// this arm exists to deny having).
    NoView,
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
    /// One Ready-node bucket classification (bug_129): consumed by
    /// BOTH [`Self::compute_cluster_snapshot`] and
    /// [`Self::compute_spawn_intents`], so the two RPCs' per-system
    /// `queued_by_system` aggregates are equal BY CONSTRUCTION. The
    /// old discipline was paired comments (the PD-7 exclusion was
    /// mirrored by hand; the bug_282 backoff continue got no snapshot
    /// twin and the aggregates diverged by exactly the in-backoff
    /// Ready set). Retry backoff is deliberately NOT a class: it
    /// suppresses spawn-intent EMISSION only, never demand accounting
    /// — an in-backoff Ready node is still builder-queue demand on
    /// both surfaces.
    // r[impl sched.admin.snapshot-substituting+4]
    pub(super) fn classify_ready_node(
        &self,
        drv_hash: &str,
        now: std::time::Instant,
    ) -> ReadyClass {
        if self.has_claimable_job(drv_hash, now) {
            ReadyClass::Substituting
        } else if self.has_pending_unclaimed_job(drv_hash) {
            ReadyClass::ParkedPacing
        } else {
            ReadyClass::Queued
        }
    }

    /// live_049 lever 3 (F1): classify a dependency's materialization
    /// job for the §13b forecast dep walk. One armament source —
    /// `JobViewEntry::claimability` (bug_170) — so this cannot
    /// disagree with pull admission / the KEDA gauge / the listing
    /// about what "active" means; the only added axis is view
    /// hydration (fail-closed [`SubstDepEta::NoView`], the
    /// `has_pending_unclaimed_job` posture). Total over
    /// `Option<view> × Option<entry> × Claimability` — zero wildcard
    /// arms.
    // r[impl sched.sla.forecast.substituting-dep-eta]
    pub(super) fn subst_dep_eta(&self, drv_hash: &str, now: std::time::Instant) -> SubstDepEta {
        use super::materialize::Claimability;
        match self.materialization_jobs.hydrated() {
            None => SubstDepEta::NoView,
            Some(view) => match view.get(drv_hash) {
                None => SubstDepEta::NoJob,
                Some(entry) => match entry.claimability(now) {
                    Claimability::Claimed => SubstDepEta::Active(SubstActiveFace::Claimed),
                    Claimability::ClaimableNow => {
                        SubstDepEta::Active(SubstActiveFace::ClaimableNow)
                    }
                    Claimability::Parked => SubstDepEta::Pacing(SubstPacingFace::Parked),
                    Claimability::Deferred => SubstDepEta::Pacing(SubstPacingFace::Deferred),
                },
            },
        }
    }

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
        // r[impl sched.admin.snapshot-substituting+4]
        // Exhaustive over DerivationStatus so a future variant addition
        // is a compile-time break here, not a silently-zero autoscaler
        // input.
        //
        // The substituting bucket is job-derived (§2.6): a node with a
        // CLAIMABLE materialization job (unclaimed, not parked, not
        // deferred — claimability()'s three axes) is substitution
        // backlog whatever its DerivationStatus; parked/deferred jobs
        // are pacing, not demand (ReadyClass::ParkedPacing — neither
        // gauge).
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
                    // §2.6 via THE shared classifier (bug_129): a Ready
                    // node carrying a CLAIMABLE materialization job
                    // (unclaimed, unparked, undeferred) is substitution
                    // backlog, not builder-queue backlog; a parked/deferred job is
                    // pacing (bug_252 — NEITHER bucket; parked stays
                    // visible via rio_scheduler_materialization_stalled,
                    // deferred is gauge-invisible for its <=300s window
                    // per the HELP); everything else is builder demand.
                    // `compute_spawn_intents` reads the SAME
                    // classification, so the two `queued_by_system`
                    // aggregates cannot diverge.
                    match self.classify_ready_node(drv_hash, bucket_now) {
                        ReadyClass::Substituting => substituting_derivations += 1,
                        ReadyClass::ParkedPacing => {}
                        ReadyClass::Queued => {
                            // The scalar and the I-107 per-system breakdown
                            // are counted in the same arm so the sum across
                            // keys equals the scalar by construction (the
                            // removed ready-queue membership the scalar once
                            // read was not dequeued by pull mints — the
                            // recorded over-count).
                            queued_derivations += 1;
                            *queued_by_system.entry(s.system.clone()).or_default() += 1;
                        }
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
    /// `queued_by_system` is populated regardless of the kind/feature
    /// filters and the backoff gate, from the SAME [`ReadyClass`]
    /// classification as `ClusterSnapshot.queued_by_system` — equal by
    /// construction (bug_129). The ComponentScaler reads ClusterStatus
    /// (rio-controller componentscaler/decide.rs), NOT this RPC; the
    /// field here exists so any consumer of either RPC sees one
    /// answer.
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

        let spawn_now = std::time::Instant::now();
        for (drv_hash, state) in self.dag.iter_nodes() {
            if state.status() != DerivationStatus::Ready {
                continue;
            }
            // r[impl sched.materialize.job+2]
            // PD-7 (Phase B, design §2.3) via THE shared classifier
            // (bug_129): nodes with an unresolved materialization job
            // are never spawn-intent candidates — the controller must
            // not spawn builder pods for work that will be
            // materialized — and they stay out of the per-system
            // aggregate exactly as `compute_cluster_snapshot` keeps
            // them out of its Ready arm (Substituting and ParkedPacing
            // are not builder demand on either surface). Retires the
            // CE-59 spawn-intent churn class as a side effect. Claimed
            // jobs' nodes are Assigned/Running and already excluded by
            // the status check above. The node itself is never builder
            // demand while its job is unresolved — but its PARENTS
            // are: the §13b forecast pass below contributes the typed
            // substitution prior for deps in exactly this class (F1,
            // `subst_dep_eta`), so the exclusion here no longer makes
            // the dependents invisible to provisioning.
            match self.classify_ready_node(drv_hash, spawn_now) {
                ReadyClass::Substituting | ReadyClass::ParkedPacing => continue,
                ReadyClass::Queued => {}
            }
            // Per-system aggregate: counted BEFORE the kind/feature
            // filters AND before the backoff gate, from the SAME
            // classification as `ClusterSnapshot.queued_by_system`, so
            // the two aggregates are equal by construction whichever
            // RPC a consumer reads.
            *queued_by_system.entry(state.system.clone()).or_default() += 1;
            // bug_282: a Ready node inside its transient-retry backoff
            // window emits no spawn intent — the kernel's pull
            // admission would refuse the mint anyway (the
            // build_backoff_expired conjunct), so spawning a pod for
            // it just burns a pod-start against a guaranteed
            // NotYetReady loop until the window lapses. BELOW the
            // aggregate (bug_129): backoff suppresses intent EMISSION
            // only — the node is still queued demand on both surfaces.
            if state.retry.backoff_until.is_some_and(|t| t > spawn_now) {
                continue;
            }

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

        // r[impl sched.sla.forecast.one-layer+2]
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
        // F1 (WO-S7-R, round-9) re-records that absence as a DECIDED
        // non-goal, not an oversight: the substituting-dep prior
        // (`SUBSTITUTING_DEP_ETA_PRIOR_SECS`) rides the SAME seed-
        // based gates, and the channel was evaluated and not taken —
        // the F1 invariant (active job ⇒ typed contribution) is
        // independent of the gate's rhs source, while the field would
        // be a fifth wire change whose producer is controller-side
        // code outside this round's plane. The seed approximation's
        // honesty caveat above remains the operative statement for
        // BOTH eta sources.
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
                // Running with a fitted-curve ETA, OR substitution-
                // active with the typed prior (F1 below). Any other
                // Queued/Ready/Created/unfitted dep → not
                // forecastable. `had_incomplete` guards the
                // (degenerate) all-deps-satisfied case — that drv
                // belongs to the Ready loop, not here.
                //
                // r[impl sched.sla.forecast.substituting-dep-eta]
                // F1 (live_049 lever 3 / WO-S7-R): a dep whose
                // unresolved materialization job is store-ACTIVE
                // (claimed, or claimable on the next worker beat)
                // resolves on the STORE plane within the substitution
                // prior — it contributes
                // `SUBSTITUTING_DEP_ETA_PRIOR_SECS` instead of the
                // pre-F1 exclusion (the blind materialization minute:
                // warm-cache parents emitted nothing until readiness,
                // paying the full node-provisioning lead cold). The
                // contribution is JOB-grounded, not layer-propagated:
                // the one-layer law's σ_resid-compounding argument
                // does not apply — substitution resolves the dep
                // directly, independent of the dep's own subtree, so
                // a Queued dep with an active job contributes the
                // same prior (and stays one fold, no recursion).
                // Disposition law per dep (total over status × job):
                //   Completed|Skipped            → satisfied;
                //   Running|Assigned + Claimed   → the prior (a held
                //     claim IS the executing resolution path; cache
                //     hits never builder-dispatched have no fitted
                //     curve at all — pull.rs `DispatchShape::Unsized`
                //     stamps no `last_intent` — and a leftover curve
                //     from a pre-substitution dispatch is stale);
                //   Running|Assigned + other     → the fitted curve
                //     (an UNCLAIMED job never displaces a live build
                //     attempt's progress-grounded estimate; pacing
                //     does not block a live attempt — the PD-20
                //     park→re-evaluation→from-source family);
                //   Queued|Ready + Active        → the prior;
                //   Queued|Ready + Pacing        → typed counted drop
                //     (park scale is minutes, deferral ≤300 s — not
                //     active store work within any lead horizon);
                //   Queued|Ready + NoJob|NoView  → not forecastable
                //     (the pre-F1 shape; NoView is the fail-closed
                //     recovery window, uncounted — counting would
                //     assert job knowledge the arm exists to deny);
                //   terminal/pre-merge statuses  → not forecastable
                //     regardless of job state (a Failed/Poisoned/
                //     Cancelled dep is a dead end, not progressing
                //     work; Created precedes the materialization
                //     plane's service surface).
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
                            match self.subst_dep_eta(&dep_hash, now) {
                                SubstDepEta::Active(SubstActiveFace::Claimed) => {
                                    eta = eta.max(SUBSTITUTING_DEP_ETA_PRIOR_SECS);
                                }
                                SubstDepEta::Active(SubstActiveFace::ClaimableNow)
                                | SubstDepEta::Pacing(_)
                                | SubstDepEta::NoJob
                                | SubstDepEta::NoView => {
                                    let Some(d) = running_dep_eta(dep, now) else {
                                        continue 'q;
                                    };
                                    eta = eta.max(d);
                                }
                            }
                        }
                        DerivationStatus::Queued | DerivationStatus::Ready => {
                            match self.subst_dep_eta(&dep_hash, now) {
                                SubstDepEta::Active(_) => {
                                    had_incomplete = true;
                                    eta = eta.max(SUBSTITUTING_DEP_ETA_PRIOR_SECS);
                                }
                                SubstDepEta::Pacing(_) => {
                                    if forecast_dropped_first(self, drv_hash, "substituting_pacing")
                                    {
                                        ::metrics::counter!(
                                            "rio_scheduler_sla_forecast_dropped_total",
                                            "reason" => "substituting_pacing",
                                        )
                                        .increment(1);
                                    }
                                    continue 'q;
                                }
                                SubstDepEta::NoJob | SubstDepEta::NoView => continue 'q,
                            }
                        }
                        DerivationStatus::Created
                        | DerivationStatus::Failed
                        | DerivationStatus::Poisoned
                        | DerivationStatus::DependencyFailed
                        | DerivationStatus::Cancelled => continue 'q,
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
    /// the two calls — the two-RPC read-read race) are omitted from
    /// the map; bug_121: the controller SKIPS an omitted intent this
    /// tick (a token-less Job under HMAC is unauthenticatable BY
    /// CONSTRUCTION — the retired posture spawned it into a
    /// fast-fail + NameCollision + backoff-tax detour) and the intent
    /// re-presents next tick, minted.
    ///
    /// The second tuple element is the bug_121 keyless DISCRIMINATOR:
    /// `true` iff `hmac_signer` is None (keyless dev mode — no tokens
    /// EXIST anywhere; the controller spawns token-less), so an
    /// `Ok(empty)` response is no longer ambiguous between dev mode
    /// and whole-batch omission.
    ///
    /// [`compute_spawn_intents`]: Self::compute_spawn_intents
    // r[impl sec.executor.identity-token+3]
    pub(crate) fn mint_executor_tokens(
        &self,
        intent_ids: &[String],
    ) -> (HashMap<String, String>, bool) {
        let Some(signer) = &self.hmac_signer else {
            return (HashMap::new(), true);
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
        let tokens = intent_ids
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
            .collect();
        (tokens, false)
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
    /// merged_bug_005 + bug_094: returns the apply outcome the drain
    /// relays to the gRPC layer — `Ok` only when EVERY plane landed,
    /// and `Err` only when NO plane landed (validate-then-commit).
    /// Every refusal — undecodable plane entry, skewed arm echo — is
    /// computed by [`AckApplyPlan::validate`] before the first state
    /// mutation; [`AckApplyPlan::commit`] is infallible, so
    /// error-after-mutate is unrepresentable by signature. The
    /// controller's commit-on-Ack buffer survives an erring Ack and
    /// redelivers the WHOLE buffer — safe, because an erring Ack
    /// applied nothing. Pre-fix unparseable entries were silently
    /// dropped while the Ack answered Ok — destroying the
    /// controller's consume-once evidence ("the ONLY clear" is
    /// Ack-Ok). merged_bug_046: the cost edge-reload gate is GONE
    /// from this path — `carry_catalog` merges pre-reload menu
    /// observations into the fresh load, so the clobber hazard the
    /// gate refused evidence over no longer exists and a refusal
    /// class died with it.
    ///
    /// merged_bug_008 — redelivery after a successful-but-unobserved
    /// Ack (routine: client timeout after server apply) is a no-op by
    /// construction, NOT by per-plane idempotence prose: cell events
    /// carry the producer's evidence epoch (`"h:cap@epoch"`, the
    /// shared `cell_wire` grammar) and the ladder applies an event
    /// iff `epoch > last_applied[cell]` (`IceBackoff::apply_*_event`)
    /// — `==` (redelivery) and `<` (reorder) are total no-ops
    /// answered Ok, so the controller's buffer clears. Epoch-less
    /// entries take the pre-epoch semantics exactly (decode-totality
    /// lane; binding snapshots rebuild and observed types upsert,
    /// idempotent as before).
    // One parameter per wire evidence plane — the positional shape IS
    // the validate-then-commit contract (a new plane forces every
    // caller through this seam).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn handle_ack_spawned_intents(
        &mut self,
        spawned: &[rio_proto::types::SpawnIntent],
        unfulfillable_cells: &[String],
        registered_cells: &[String],
        observed_instance_types: &[rio_proto::types::ObservedInstanceType],
        bound_intents: &[rio_proto::types::BoundIntent],
        binding_snapshot: Option<&[rio_proto::types::BoundIntent]>,
        rejected: &[rio_proto::types::IntentVerdict],
    ) -> Result<Vec<NoHostPoison>, super::command::AckApplyError> {
        let plan = AckApplyPlan::validate(
            spawned,
            unfulfillable_cells,
            registered_cells,
            observed_instance_types,
            bound_intents,
            binding_snapshot,
            rejected,
        )?;
        Ok(plan.commit(self))
    }

    // r[impl scheduler.sla.ceiling.stale-solve-revalidation+2]
    /// live_051(c): poison every drv whose consecutive no-hosting-class
    /// verdict budget crossed in [`AckApplyPlan::commit`]. Runs AFTER
    /// the atomic apply (commit is infallible, so a poison here can
    /// never follow a half-applied ack) on the actor command arm's
    /// async context; the poison rides the EXISTING machinery
    /// (`poison_and_cascade` — Ready is a legal precondition there)
    /// with the controller's `detail` as the operator-actionable
    /// message, so the operator reads which classes exist and what
    /// failed to match — never a bare "no hosting class".
    pub(super) async fn apply_no_host_poisons(&mut self, poisons: Vec<NoHostPoison>) {
        for p in poisons {
            tracing::warn!(
                drv_hash = %p.drv,
                budget = NO_HOST_VERDICTS_TO_POISON,
                detail = %p.detail,
                "consecutive no-hosting-class verdict budget exhausted — \
                 poisoning the drv with the controller's verdict detail",
            );
            self.poison_and_cascade(
                &p.drv,
                &format!(
                    "no hosting class after {NO_HOST_VERDICTS_TO_POISON} \
                     consecutive controller verdicts: {}",
                    p.detail
                ),
                None,
                None,
                // live_051(c): a verdict poison has no pod and no
                // attempt — the drv looped Ready, the controller
                // structurally refused to host it; any name-resolved
                // log would be a PRIOR attempt's (the spawn-gate
                // NoExecution lane, bug_080's second caller).
                rio_proto::VerdictBacking::NoExecution,
            )
            .await;
        }
    }
}

// r[impl scheduler.sla.ceiling.stale-solve-revalidation+2]
/// live_051(c): the verdict budget — consecutive `NO_HOSTING_CLASS`
/// verdicts (one per controller cover pass) a Ready drv may accumulate
/// before it poisons with the verdict's detail. The TIME envelope is
/// `N x the controller ack cadence` (the nodeclaim-pool tick, ~10s
/// shipped): 30 passes ~= 5 minutes operator-visible budget — long
/// enough to ride out a config rollout window (a reload that CHANGES
/// the hosting-class set resets the counter via the config-census
/// reset key in [`step_no_host_counter`] — merged_bug_043(1): demand
/// jitter in the verdict detail does NOT reset), short enough that
/// the measured live
/// loop (hours of Ready-forever churn, operator cancellation as the
/// only exit) dies promptly. Violable by test loop count; the budget
/// IS the envelope (R17 time axis).
pub(super) const NO_HOST_VERDICTS_TO_POISON: u32 = 30;

/// One budget-crossing from [`AckApplyPlan::commit`]: the drv to
/// poison and the controller's last verdict `detail` (the
/// operator-actionable message).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NoHostPoison {
    pub(super) drv: DrvHash,
    pub(super) detail: String,
}

/// One drv's consecutive-verdict track (merged_bug_043: the typed
/// transition state — count, reset key, display detail, pass stamp —
/// replacing the `(u32, String)` pair whose String was BOTH the reset
/// key and the display message).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NoHostTrack {
    /// Consecutive verdict-carrying passes with identical census.
    pub(super) count: u32,
    /// The TYPED reset key: [`crate::sla::config::SlaConfig::
    /// hosting_census`] at fold time — a hosting-class config change
    /// re-opens the heal window (the law's axis); demand jitter in
    /// the controller's formatted detail does NOT (merged_bug_043(1)).
    pub(super) census: u64,
    /// Latest verdict detail — DISPLAY-ONLY (the operator-actionable
    /// poison message), never compared.
    pub(super) detail: String,
    /// The verdict-pass stamp this track last counted
    /// (merged_bug_043(3)): "consecutive" means ADJACENT
    /// verdict-carrying passes — a pass gap (the drv got no verdict
    /// while others did: spawned, masked `UnplaceableAllMasked`, or
    /// reaped) breaks the streak STRUCTURALLY, so a frozen track can
    /// never claim false consecutiveness when verdicts resume (the
    /// 29+1 false-"30 consecutive" shape). Lazy expiry — no curated
    /// sweep position to bypass (the merged_bug_033 lesson, T3).
    pub(super) pass: u64,
}

/// live_051(c): one consecutive-verdict counter step — pure, so the
/// counter-lifecycle census walks it as a law table. The count
/// CONTINUES iff the hosting-config census is unchanged AND the pass
/// is adjacent to the last counted one; everything else — census
/// change (config reload re-opens the heal window), pass gap (the
/// streak broke: no verdict for this drv on an evidence-carrying
/// pass), or no prior track — RESTARTS at 1. The budget counts
/// CONSECUTIVE IDENTICAL-CENSUS rejection evidence only; the
/// controller's formatted detail (which embeds per-solve demand
/// jitter) is carried for display, never compared
/// (merged_bug_043(1)/(3)).
pub(super) fn step_no_host_counter(
    prev: Option<&NoHostTrack>,
    census: u64,
    detail: &str,
    pass: u64,
) -> NoHostTrack {
    let count = match prev {
        Some(t) if t.census == census && t.pass.saturating_add(1) == pass => {
            t.count.saturating_add(1)
        }
        // Defensive idempotency: a same-pass re-step (unreachable
        // through the in-request dedup) keeps the count.
        Some(t) if t.census == census && t.pass == pass => t.count,
        _ => 1,
    };
    NoHostTrack {
        count,
        census,
        detail: detail.to_owned(),
        pass,
    }
}

// r[impl scheduler.sla.ceiling.stale-solve-revalidation+2]
/// live_050(e)/live_051(b): the TOTAL typed outcome of the
/// cell-emission chokepoint — BOTH solve arms since merged_bug_002:
/// the no-memo classify and the memo arm's post-overlay
/// re-classification fold through the same alphabet (R14 closed
/// alphabet, zero wildcard arms at the folds in `solve_intent_for`).
/// Pre-fix the chokepoint's only
/// vocabulary was `Vec<Cell>` — emptiness conflated "genuinely
/// hw-agnostic" (correct, quiet), "demand solved under a ceiling that
/// no longer exists" (the live_050(e) starvation channel), "demand
/// infeasible at every class" (live_051(b)), and "operator pin
/// refused" — and the controller dropped the non-agnostic ones as a
/// metric-only tally forever. The totality claim is SCOPED to the
/// typed segment (merged_bug_037): within the scheduler the alphabet
/// is total — `StaleSolve` re-routes (clamped cells, disclosed),
/// while `Unhostable` serializes as typed-empty `hw_class_names` BY
/// DESIGN (the forced-demand law mandates the empty emission so the
/// controller's `fallback_cell` reaches its own `None`), making it
/// the designed feeder of the controller's `no_hosting_class` arm —
/// distinguished controller-side by the `IntentVerdict` plane
/// (answered by the live_051(c) verdict loop — composition with
/// WO-S7-3's `PlacementOutcome`). The wire DELIBERATELY erases the
/// variant: the verdict plane already carries the controller-side
/// answer, so a second wire axis would duplicate it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CellEmission {
    /// hw-routed cells — the unchanged §13d arms (reference-class
    /// expansion, pin honored) plus disclosed re-solves.
    Cells(Vec<crate::sla::config::Cell>),
    /// Genuinely hw-agnostic: ∅ features, no infeasibility evidence,
    /// no pin (or arch-unmappable featureless). The §13e cold-start
    /// quiet edge — quiet BY TYPE, not by shared emptiness.
    HwAgnostic,
    /// Demand no class hosts at size, with live routing candidates —
    /// re-solved under the LIVE ceilings (clamped into the largest
    /// hosting class) and disclosed. Carries the WHY: the stale
    /// solved dims, the live best-class ceilings, and the re-solved
    /// emission.
    StaleSolve {
        solved: (u32, u64),
        live_max: (u32, u64),
        class: String,
        resolved: (u32, u64),
        cells: Vec<crate::sla::config::Cell>,
    },
    /// No class can host even re-solved: zero routing candidates
    /// (feature/arch gap) or the clamped floor exceeds the best
    /// candidate's ceiling. Carries the WHY by construction (T5):
    /// the demand and the best class + its ceilings, so every
    /// consumer derives the delta.
    Unhostable {
        demand: (u32, u64),
        best_class: Option<(String, (u32, u64))>,
    },
    /// Operator `--capacity` pin refused by a size-hosting class —
    /// the r31 A3 lane (its own debounced warn lives at the
    /// `bypass_cells` arm); emission stays empty, the pin is never
    /// silently rewritten.
    PinGated,
}

/// live_050(e)/live_051(c): actor-held supply-revalidation state, one
/// typed home for the two pieces the stale-solve law mints — the
/// per-drv consecutive-verdict counters and the emission-disclosure
/// debounce. The r35 STRIKE-3 tripwire (mod.rs LRU-cap consts) asks
/// any further `Mutex<LruCache>` debounce accretion to carry a
/// type-enforced gate: [`Self::disclose_once`] IS that gate — the LRU
/// is private to this struct, so an emit site cannot bypass it.
pub(super) struct SupplyRevalidation {
    /// `drv -> NoHostTrack` for `IntentVerdict::NO_HOSTING_CLASS`
    /// entries. Tenure-scoped in-memory state (the IceBackoff
    /// precedent: ADR-023 "the controller reports, the scheduler
    /// decides") — wiped on leader transition; the controller
    /// re-mints fresh verdicts every tick, so a successor re-burns
    /// its own budget.
    pub(super) no_host_verdicts: std::collections::HashMap<DrvHash, NoHostTrack>,
    /// The verdict-pass ordinal (merged_bug_043(3)): incremented once
    /// per APPLIED ack that carries ≥1 verdict (the cover-pass
    /// signature — per-pool acks without a verdict plane do not
    /// advance it, so they cannot fake a gap). Tracks stamp it at
    /// step time; adjacency IS consecutiveness.
    pub(super) verdict_pass: u64,
    /// `(tenant, pname, kind)` emission-revalidation disclosures
    /// already made (kind ∈ {stale_resolved, unhostable} — the
    /// `exit` label values), each carrying the DISCLOSING drv.
    /// Once-per-EPISODE (bug_119): the heal edge — a healthy
    /// classified emission for the same `(tenant, pname)` BY THE SAME
    /// drv — pops both kinds, so a relapse discloses again. The
    /// drv-matched pop keeps the pname-level spam bound sound when
    /// same-pname drvs have mixed health (per-drv floors): a healthy
    /// sibling cannot oscillate a latch a sick sibling holds.
    /// Eviction and leader transition also re-arm (fail-safe
    /// over-emit).
    disclosed: parking_lot::Mutex<lru::LruCache<(String, String, &'static str), DrvHash>>,
}

impl Default for SupplyRevalidation {
    fn default() -> Self {
        Self {
            no_host_verdicts: std::collections::HashMap::new(),
            verdict_pass: 0,
            disclosed: parking_lot::Mutex::new(lru::LruCache::new(
                std::num::NonZeroUsize::new(1024).unwrap(),
            )),
        }
    }
}

impl SupplyRevalidation {
    /// True exactly once per `(tenant, pname, kind)` EPISODE — the
    /// type-level debounce gate. An episode opens at the first
    /// disclosure (recording the disclosing drv) and closes at
    /// [`Self::heal`]. A different sick drv under a live latch
    /// re-points the latch at itself without re-disclosing (the
    /// pname-level spam bound).
    pub(super) fn disclose_once(
        &self,
        tenant: &str,
        pname: &str,
        kind: &'static str,
        drv: &DrvHash,
    ) -> bool {
        self.disclosed
            .lock()
            .put((tenant.to_owned(), pname.to_owned(), kind), drv.clone())
            .is_none()
    }

    /// bug_119: the HEAL EDGE — a healthy classified emission
    /// (`Cells`/`HwAgnostic`/memo-survives) closes any disclosure
    /// episode THE SAME drv opened (or last re-pointed), so a relapse
    /// discloses again. The latch lives where the heal is visible
    /// (the emission classifier — the `ice_exhausted` rising-edge
    /// pattern on the SAME metric family is the in-tree precedent);
    /// pre-fix the LRU was insert-only and the per-episode law
    /// silently became per-tenure. Drv-matched: a healthy same-pname
    /// sibling never pops a latch a sick drv holds (per-drv floors
    /// make mixed health within a pname real — an unmatched pop would
    /// oscillate the latch into per-poll re-disclosure).
    pub(super) fn heal(&self, tenant: &str, pname: &str, drv: &DrvHash) {
        let mut d = self.disclosed.lock();
        for kind in ["stale_resolved", "unhostable"] {
            let k = (tenant.to_owned(), pname.to_owned(), kind);
            if d.peek(&k) == Some(drv) {
                d.pop(&k);
            }
        }
    }
}

/// One validated `AckSpawnedIntents` application (bug_094 —
/// validate-then-commit). [`Self::validate`] computes EVERY refusal
/// over the RAW wire planes; [`Self::commit`] applies the typed plan
/// and is infallible by signature. The wire types stop here:
/// `commit` receives only decoded cells, hashes, and rows, so a
/// silent per-plane parse skip — or any new refusal arm landing
/// after a mutation — is unwritable at this seam.
///
/// Banner-(a) witness: `commit` exhaustively destructures the plan
/// (`let AckApplyPlan { .. } = self` with every field named), so a
/// wire plane added to `validate` without a `commit` handler does
/// not compile.
pub(super) struct AckApplyPlan {
    /// 124(d) spawn-ack witnesses (every spawned `intent_id`).
    acked_spawned: Vec<DrvHash>,
    /// Kube-authoritative binding rows; `None` = "this Ack carries no
    /// snapshot" (per-pool reconcilers; the legacy field-5 arm).
    binding: Option<Vec<PlannedBinding>>,
    /// Arm-on-ack decodes of the spawned echo (merged_bug_134: the
    /// typed pairing law, including the no-arm legacy lanes).
    armed: Vec<(DrvHash, ArmDecode)>,
    /// ICE-clear cell events (`registered_cells`, wire field 3) with
    /// the producer's evidence epoch (merged_bug_008; `None` =
    /// legacy epoch-less entry).
    clears: Vec<(
        crate::sla::config::Cell,
        Option<rio_common::cell_wire::EvidenceEpoch>,
    )>,
    /// ICE-mark cell events (`unfulfillable_cells`, wire field 2),
    /// epoch'd like `clears`.
    marks: Vec<(
        crate::sla::config::Cell,
        Option<rio_common::cell_wire::EvidenceEpoch>,
    )>,
    /// Cost-table observations (decoded `observed_instance_types`).
    observed: Vec<(crate::sla::config::Cell, String, u32, u64)>,
    /// live_051(c): decoded `rejected` verdicts (wire field 7) —
    /// `(drv, detail)` rows whose reason decoded to the closed
    /// alphabet's `NO_HOSTING_CLASS`. Folded by `commit` into the
    /// consecutive-verdict counters.
    verdicts: Vec<(DrvHash, String)>,
}

/// One decoded `BoundIntent` row — the typed remnant of the wire
/// type that crosses into [`AckApplyPlan::commit`].
struct PlannedBinding {
    intent: DrvHash,
    node: String,
    /// Wire `0` = absent (pre-upgrade controller): the mint falls
    /// back to its re-solve alone.
    deadline_secs: Option<u32>,
}

/// merged_bug_134: total decode of one spawned intent's echoed
/// parallel `(hw_class_names, node_affinity)` arrays. The pre-fix
/// `Iterator::zip` silently truncated to the shorter array — a 2-cell
/// arm truncated to 1 forges the exactly-one-cell proof the §13a
/// first-pull ICE clear gates on (`let [cell] = cells.as_slice()`,
/// actor/pull.rs). The law:
///
/// | names     | terms     | decode                                  |
/// |-----------|-----------|-----------------------------------------|
/// | empty     | empty     | `Empty` (hw-agnostic intent, no arm)    |
/// | exactly one side empty | `LegacyUnarmed` (no arm, NO refusal)   |
/// | non-empty, equal len   | `Armed(cells)` or a typed refusal      |
/// | non-empty, unequal len | `ArmEchoSkewed` refusal                |
///
/// Only skew shapes that could forge a DIFFERENT cell set refuse.
/// `LegacyUnarmed` is the pre-field-14 echo shape (one array absent);
/// it cannot arm anything — pre-fix it already zip-truncated to
/// no-arm — so it stays a typed no-arm TOTALITY lane, not a refusal
/// (its rolling-skew rationale is MOOT per SIGNED Q6, --wipe rollout;
/// the lane survives as decode totality over the echo shapes). In the
/// `Armed` lane every aligned term is decoded against the PRODUCER'S
/// shape (merged_bug_039: parse-don't-validate —
/// `cells_to_selector_terms` emits EXACTLY ONE
/// `karpenter.sh/capacity-type` requirement per term, operator `In`,
/// single-valued, the capacity LAST after the label requirements; the
/// pre-fix `find().and_then(values.first())` peek read one cell out
/// of `In[spot,on-demand]`, decoded `NotIn[spot]` to its inverse, and
/// was order-sensitive across duplicate requirements — a
/// `karpenter.sh/capacity-type` entry in `hw_classes.labels` made the
/// producer emit a LABEL COPY first, which the peek decoded instead
/// of the authoritative one). The axes product
/// `{0,1,≥2} requirement multiplicity × {In, other} operator ×
/// {0,1,≥2} values` is total (the generator is rustc — the decoder's
/// match has no wildcard):
///
/// | axis cell                     | decode                          |
/// |-------------------------------|---------------------------------|
/// | 0 capacity requirements       | `ArmEchoSkewed` (pairing: the   |
/// |                               | pair cannot name a cell)        |
/// | ≥2 capacity requirements      | `PlaneEntryUndecodable` (NEW —  |
/// |                               | present-but-not-producer-shaped)|
/// | operator ≠ `In`               | `PlaneEntryUndecodable` (NEW)   |
/// | 0 values                      | `ArmEchoSkewed` (existing       |
/// |                               | empty-values law)               |
/// | ≥2 values                     | `PlaneEntryUndecodable` (NEW)   |
/// | value outside the alphabet    | `PlaneEntryUndecodable`         |
/// |                               | (existing)                      |
///
/// Partition rationale: `ArmEchoSkewed` = absence/pairing structure;
/// `PlaneEntryUndecodable{SpawnedArming}` = present-but-undecodable
/// (both variants pre-exist — zero refusal-alphabet change). The
/// strict decode's SAME-COMMIT precondition: `validate_shape`
/// reserves `LABEL_CAPACITY_TYPE` out of `hw_classes.labels`, so a
/// colliding config refuses at BOOT instead of converting into a
/// permanent whole-request Ack refusal loop.
pub(super) enum ArmDecode {
    /// Both arrays empty — hw-agnostic intent, nothing to arm.
    Empty,
    /// Exactly one array empty — the legacy echo shape; no arm.
    LegacyUnarmed,
    /// Paired echo — arm `dispatched_cells` with the FULL set.
    Armed(smallvec::SmallVec<[crate::sla::config::Cell; 4]>),
}

impl ArmDecode {
    /// Total decode per the table above; refusals are typed, never
    /// truncations.
    fn decode(i: &rio_proto::types::SpawnIntent) -> Result<Self, super::command::AckApplyError> {
        use super::command::{AckApplyError, AckPlane};
        let (names, terms) = (i.hw_class_names.len(), i.node_affinity.len());
        match (names, terms) {
            (0, 0) => Ok(Self::Empty),
            (0, _) | (_, 0) => Ok(Self::LegacyUnarmed),
            (n, t) if n != t => Err(AckApplyError::ArmEchoSkewed {
                intent_id: i.intent_id.clone(),
                names: n,
                terms: t,
            }),
            _ => {
                let mut cells: smallvec::SmallVec<[crate::sla::config::Cell; 4]> =
                    smallvec::SmallVec::with_capacity(names);
                for (h, term) in i.hw_class_names.iter().zip(&i.node_affinity) {
                    let cap = match decode_capacity_requirement(term) {
                        Ok(cap) => cap,
                        // Absence/pairing structure: the pair cannot
                        // name a cell, so the echo could forge a
                        // different cell set.
                        Err(CapacityReqDefect::Pairing) => {
                            return Err(AckApplyError::ArmEchoSkewed {
                                intent_id: i.intent_id.clone(),
                                names,
                                terms,
                            });
                        }
                        // Present but not producer-shaped (duplicate
                        // requirement, non-In operator, multi-value,
                        // out-of-alphabet value).
                        Err(CapacityReqDefect::Undecodable(entry)) => {
                            return Err(AckApplyError::PlaneEntryUndecodable {
                                plane: AckPlane::SpawnedArming,
                                entry,
                            });
                        }
                    };
                    cells.push((h.clone(), cap));
                }
                Ok(Self::Armed(cells))
            }
        }
    }
}

/// Why one aligned term's capacity requirement failed the typed parse
/// — the [`decode_capacity_requirement`] refusal partition (the two
/// existing [`super::command::AckApplyError`] classes; no new wire
/// alphabet).
enum CapacityReqDefect {
    /// No capacity requirement at all, or empty values: structural
    /// pairing skew (the existing `ArmEchoSkewed` lanes).
    Pairing,
    /// Present but not the producer's shape: the rendered offending
    /// requirement(s) for the `PlaneEntryUndecodable` entry field.
    Undecodable(String),
}

/// merged_bug_039: parse one aligned term's capacity requirement
/// against the PRODUCER'S shape (`cells_to_selector_terms` emits
/// exactly one `LABEL_CAPACITY_TYPE` requirement per term, operator
/// `In`, single-valued) — a total match over the
/// multiplicity × operator × arity product, replacing the
/// `find().and_then(values.first())` peek that read one cell out of
/// `In[spot,on-demand]`, decoded `NotIn[spot]` to its inverse, and
/// resolved duplicate requirements order-sensitively.
fn decode_capacity_requirement(
    term: &rio_proto::types::NodeSelectorTerm,
) -> Result<crate::sla::config::CapacityType, CapacityReqDefect> {
    let mut matches = term
        .match_expressions
        .iter()
        .filter(|r| r.key == crate::sla::config::LABEL_CAPACITY_TYPE);
    let Some(req) = matches.next() else {
        return Err(CapacityReqDefect::Pairing);
    };
    let dupes = matches.count();
    if dupes > 0 {
        return Err(CapacityReqDefect::Undecodable(format!(
            "{} appears {} times in one term (the producer emits exactly one)",
            crate::sla::config::LABEL_CAPACITY_TYPE,
            dupes + 1,
        )));
    }
    if req.operator != "In" {
        return Err(CapacityReqDefect::Undecodable(format!(
            "{} {} [{}]",
            req.key,
            req.operator,
            req.values.join(", "),
        )));
    }
    match req.values.as_slice() {
        // The existing empty-values law: structural skew.
        [] => Err(CapacityReqDefect::Pairing),
        [value] => crate::sla::config::CapacityType::parse(value)
            .ok_or_else(|| CapacityReqDefect::Undecodable(value.clone())),
        more => Err(CapacityReqDefect::Undecodable(format!(
            "{} In [{}] (multi-valued: names {} cells, not one)",
            req.key,
            req.values.join(", "),
            more.len(),
        ))),
    }
}

impl AckApplyPlan {
    // r[impl sched.sla.ack-validate-then-commit+1]
    /// Decode and refuse BEFORE any mutation exists. Planes validate
    /// in wire-field order (`spawned` arming = 1,
    /// `unfulfillable_cells` = 2, `registered_cells` = 3,
    /// `observed_instance_types` = 4); the first failure refuses the
    /// WHOLE request. Whole-request refusal is safe controller-side:
    /// the buffer is retained on Ack-Err and buffered marks keep
    /// masking `cover_deficit` locally until acked — the refusal is a
    /// loud, logged skew signal where the pre-fix behavior was silent
    /// evidence destruction.
    pub(super) fn validate(
        spawned: &[rio_proto::types::SpawnIntent],
        unfulfillable_cells: &[String],
        registered_cells: &[String],
        observed_instance_types: &[rio_proto::types::ObservedInstanceType],
        bound_intents: &[rio_proto::types::BoundIntent],
        binding_snapshot: Option<&[rio_proto::types::BoundIntent]>,
        rejected: &[rio_proto::types::IntentVerdict],
    ) -> Result<Self, super::command::AckApplyError> {
        use super::command::{AckApplyError, AckPlane};
        // 124(d): record the spawn-ack witness for EVERY spawned
        // intent — a NoEligibleSource verdict landing within the
        // defer window raced its own spawn.
        let acked_spawned: Vec<DrvHash> = spawned
            .iter()
            .map(|i| DrvHash::from(i.intent_id.as_str()))
            .collect();
        // Arm-on-ack: recover the FULL `cells` vec from the parallel
        // `(hw_class_names, node_affinity)` wire form
        // (`cells_to_selector_terms` emits one term per cell) through
        // the total [`ArmDecode`] law — merged_bug_134: the rolling
        // skew the comment block in `commit` names is in-scope, so
        // silent `zip` truncation of the CONTROLLER'S ECHO is a
        // forgery lane, not a tolerance. Recording only `cells[0]`
        // (bug_030) is the §1-of-N approximation: the pod's affinity
        // is OR-of-A', so the first-pull consumer needs the whole
        // set.
        let mut armed: Vec<(DrvHash, ArmDecode)> = Vec::with_capacity(spawned.len());
        for i in spawned {
            armed.push((DrvHash::from(i.intent_id.as_str()), ArmDecode::decode(i)?));
        }
        let marks = Self::decode_cell_plane(unfulfillable_cells, AckPlane::UnfulfillableCells)?;
        let clears = Self::decode_cell_plane(registered_cells, AckPlane::RegisteredCells)?;
        let mut observed = Vec::with_capacity(observed_instance_types.len());
        for o in observed_instance_types {
            match rio_common::cell_wire::decode_cell_event(&o.cell) {
                Ok(p) => observed.push((
                    (p.hw_class, p.capacity.into()),
                    o.instance_type.clone(),
                    o.cores,
                    o.mem_bytes,
                )),
                Err(_) => {
                    return Err(AckApplyError::PlaneEntryUndecodable {
                        plane: AckPlane::ObservedTypes,
                        entry: o.cell.clone(),
                    });
                }
            }
        }
        // live_051(c): decode the verdict plane through the CLOSED
        // reason alphabet — rustc-exhaustive over the prost enum, zero
        // wildcard arms, so a future `IntentVerdictReason` variant
        // stops compiling here until this consumer decides its fold.
        // `UNSPECIFIED` and unknown discriminants refuse the WHOLE
        // request (validate-then-commit: an erring ack applied
        // nothing; the controller re-mints next tick).
        let mut verdicts = Vec::with_capacity(rejected.len());
        for v in rejected {
            use rio_proto::types::IntentVerdictReason;
            match IntentVerdictReason::try_from(v.reason) {
                Ok(IntentVerdictReason::NoHostingClass) => {
                    verdicts.push((DrvHash::from(v.intent_id.as_str()), v.detail.clone()));
                }
                Ok(IntentVerdictReason::OverCap) => {
                    // ADVISORY acknowledge-WITHOUT-poison (the typed
                    // non-poisoning arm). Over-cap is a TRANSIENT,
                    // self-healing disposition — the controller's
                    // sizing backstop fires on ≤300s GetHwClassConfig
                    // version skew — while this fold's sibling lane
                    // feeds the terminal poison budget
                    // (NO_HOST_VERDICTS_TO_POISON = 30 × the ~10s ack
                    // cadence ≈ the SAME window): stepping ANY terminal
                    // budget here would poison self-healing drvs at
                    // exactly the skew threshold. The drv stays Ready;
                    // the controller re-mints once the skew clears or
                    // the demand re-solves. The wire reason is DISTINCT
                    // from NO_HOSTING_CLASS by type; conflating the two
                    // (the laundering form) is forbidden — see the
                    // IntentVerdictReason proto doc.
                    tracing::debug!(
                        intent_id = %v.intent_id,
                        detail = %v.detail,
                        "over-cap verdict acknowledged without poison",
                    );
                }
                Ok(IntentVerdictReason::Unspecified) | Err(_) => {
                    return Err(AckApplyError::PlaneEntryUndecodable {
                        plane: AckPlane::Rejected,
                        entry: format!("{} reason={}", v.intent_id, v.reason),
                    });
                }
            }
        }
        // r[impl sched.snapshot.binding-presence]
        // Plane selection: the nodeclaim_pool reconciler ships the
        // FULL set every tick as an EXPLICIT snapshot
        // (`binding_snapshot`, C2/285): `Some(set)` — even empty —
        // wholesale-rebuilds (present-and-empty correctly CLEARS the
        // map: the scale-to-zero tick has zero bound pods and says
        // so); `None` = "this Ack carries no snapshot" (per-pool
        // reconcilers, and pre-upgrade controllers on the legacy
        // field-5 arm) = no-op (mb_012/⛔2: an unconditional
        // `mem::take` would discard every captured `tenant` on every
        // per-pool reconcile). The legacy arm keeps the OLD semantics
        // — non-empty `bound_intents` rebuilds — for rolling skew
        // (R9: read-side back-compat only, never dual-written).
        let binding = match binding_snapshot {
            Some(snap) => Some(snap),
            None if !bound_intents.is_empty() => Some(bound_intents),
            None => None,
        }
        .map(|snap| {
            snap.iter()
                .map(|b| PlannedBinding {
                    intent: DrvHash::from(b.intent_id.as_str()),
                    node: b.node_name.clone(),
                    deadline_secs: (b.deadline_secs > 0).then_some(b.deadline_secs),
                })
                .collect()
        });
        Ok(Self {
            acked_spawned,
            binding,
            armed,
            clears,
            marks,
            observed,
            verdicts,
        })
    }

    /// Strict decode of one string cell-event plane via the shared
    /// grammar ([`rio_common::cell_wire`]). Any undecodable entry
    /// refuses the plane's WHOLE request — there is no drop lane.
    fn decode_cell_plane(
        entries: &[String],
        plane: super::command::AckPlane,
    ) -> Result<
        Vec<(
            crate::sla::config::Cell,
            Option<rio_common::cell_wire::EvidenceEpoch>,
        )>,
        super::command::AckApplyError,
    > {
        entries
            .iter()
            .map(|s| match rio_common::cell_wire::decode_cell_event(s) {
                Ok(p) => Ok(((p.hw_class, p.capacity.into()), p.epoch)),
                Err(_) => Err(super::command::AckApplyError::PlaneEntryUndecodable {
                    plane,
                    entry: s.clone(),
                }),
            })
            .collect()
    }

    // r[impl sched.sla.ack-validate-then-commit+1]
    /// Apply the validated plan. Infallible by signature — every
    /// refusal was computed in [`Self::validate`], so no arm here can
    /// err after a sibling plane mutated. The destructure names every
    /// field: a plane added to the plan without a commit handler is a
    /// compile error, and `commit` has no access to the raw request
    /// types (the wire stopped at `validate`).
    pub(super) fn commit(self, actor: &mut DagActor) -> Vec<NoHostPoison> {
        let AckApplyPlan {
            acked_spawned,
            binding,
            armed,
            clears,
            marks,
            observed,
            verdicts,
        } = self;
        // 124(d): opportunistic prune keeps the map bounded (entries
        // older than 2× the window are dead: the defer read only
        // consults the window).
        if !acked_spawned.is_empty() {
            let now = crate::db::attempts::epoch_now();
            for h in acked_spawned {
                // live_051(c) + merged_bug_043(2): a spawned ack heals
                // the consecutive-verdict budget only on the FRESH
                // spawn edge. The pool reconciler re-acks
                // already-Pending Jobs in `spawned` every tick — a
                // Job-EXISTS echo, not a hosting witness — and the
                // pre-fix unconditional reset let a Pending-forever
                // Job keep a genuinely-unhostable drv looping Ready.
                // Freshness is structural: the `acked_spawned` entry
                // is refreshed per ack and pruned at 2× the defer
                // window, so a still-Pending Job's re-acks always find
                // a live entry (echo ⇒ no reset), while a genuinely
                // new spawn after a quiet gap finds none (fresh ⇒
                // reset — the heal).
                if actor.acked_spawned.insert(h.clone(), now).is_none() {
                    actor.supply_reval.no_host_verdicts.remove(&h);
                }
            }
            actor
                .acked_spawned
                .retain(|_, t| now - *t < 2.0 * crate::actor::pull::ACKED_SPAWNED_DEFER_SECS);
        }
        // Kube-authoritative `intent_id (== drv_hash) →
        // (spec.nodeName, tenant)`. `tenant` is captured from the DAG
        // when present, else carried forward from the existing entry
        // — once DAG-absent the last DAG-present value sticks. A
        // fall-through executor's spawn-drv leaves the DAG but the
        // Ack keeps shipping its `intent_id` while the pod lives.
        if let Some(snap) = binding {
            let prev = std::mem::take(&mut actor.authoritative_binding);
            for b in snap {
                let tenant = actor
                    .dag
                    .node(&b.intent)
                    .and_then(|n| n.attributed_tenant(&actor.builds))
                    .or_else(|| prev.get(&b.intent).and_then(|p| p.tenant));
                actor.authoritative_binding.insert(
                    b.intent,
                    AuthBinding {
                        node: b.node,
                        tenant,
                        deadline_secs: b.deadline_secs,
                    },
                );
            }
        }
        // Exhaustive over the closed [`ArmDecode`] alphabet — a new
        // echo-shape variant cannot fall through to a silent no-arm.
        for (id, arm) in armed {
            match arm {
                ArmDecode::Empty | ArmDecode::LegacyUnarmed => {}
                ArmDecode::Armed(cells) => {
                    actor.dispatched_cells.insert(id, cells);
                }
            }
        }
        // merged_bug_003 (supersedes the merged_bug_005 two-set law):
        // the controller buffers per-cell ORDERED evidence, so a
        // request MAY carry one cell in BOTH planes — exactly the
        // ClearThenMark chronology (clear epoch < mark epoch). This
        // fixed clears-then-marks order is now LOAD-BEARING again: it
        // realizes that chronology as reset-then-step-0. A stale
        // buffered mark can no longer resurrect over a strictly newer
        // registration (the buffer's clear-supersedes-mark direction
        // is unchanged), and out-of-order/duplicate arrivals are
        // killed by the epoch gate below, not by plane arithmetic.
        // merged_bug_008: each event applies through the per-cell
        // evidence-epoch gate — redelivery (`==`) and reorder (`<`)
        // are TOTAL no-ops, epoch-less entries take the legacy lane.
        for (cell, epoch) in clears {
            actor.ice.apply_clear_event(&cell, epoch);
        }
        for (cell, epoch) in marks {
            actor.ice.apply_mark_event(&cell, epoch);
        }
        // Third writer to `cost_table` (after `fold_spot_poll`→price
        // and `interrupt_housekeeping`→λ/node_count); field-disjoint —
        // this arm writes only `cells`. Applies UNCONDITIONALLY
        // (merged_bug_046): a write landing before the lease-acquire
        // edge reload is preserved BY the reload — `carry_catalog`
        // merges the outgoing menus into the fresh load (union-only
        // monotone store, lossless reload law), so the gate that
        // refused whole requests here protected against a clobber
        // lane that no longer exists. Priced residual at the one
        // surviving cross-task seam (bug_068): a leadership loss
        // between this request's leader gate and this write leaves at
        // most one observation batch on a table whose NEXT tenure's
        // reload merges it forward — the batch survives (pre-merge,
        // the same interleaving lost it).
        if !observed.is_empty() {
            actor.cost_table.write().observe_instance_types(observed);
        }
        // r[impl scheduler.sla.ceiling.stale-solve-revalidation+2]
        // live_051(c): fold the verdict plane into the consecutive
        // counters. The budget keys on EMISSION PASS, not ack count —
        // realized as a composition: the producer mints at most one
        // verdict per drv per cover pass and never redelivers them
        // (`CoverResult::rejected` is NOT buffered across acks; the
        // controller's `admin_call` is single-shot per tick), and this
        // fold counts at most once per drv per APPLIED request (the
        // `seen` dedup below kills duplicate entries within one ack).
        // A FRESH spawned entry for a drv is the success signal — its
        // track resets (the heal edge; a Pending re-ack echo does
        // not, merged_bug_043(2)); a hosting-config census change
        // restarts at 1 (the config-reload reset) and a pass gap
        // restarts at 1 (the typed non-event) — see
        // `step_no_host_counter`; the verdict detail is display-only.
        let mut poisons = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        // merged_bug_043(3): the verdict-pass ordinal advances once
        // per APPLIED ack carrying ≥1 verdict — the cover-pass
        // signature. Tracks stamp it; a track whose stamp is not
        // adjacent to the current pass restarts at 1 in
        // `step_no_host_counter` (the typed no-verdict-this-pass
        // non-event: spawned/masked/reaped drvs break their streak
        // structurally instead of freezing). Residual (recorded): a
        // cover pass yielding ZERO verdicts fleet-wide does not
        // advance the ordinal — such a pass is indistinguishable from
        // no pass on the current wire; any drv-visible gap (the
        // frozen-29-track shape) requires other drvs' verdicts, which
        // DO advance it.
        if !verdicts.is_empty() {
            actor.supply_reval.verdict_pass += 1;
        }
        let pass = actor.supply_reval.verdict_pass;
        // merged_bug_043(1): the typed reset key — computed once per
        // verdict-carrying ack, never per verdict.
        let census = if verdicts.is_empty() {
            0
        } else {
            actor.sla_config.hosting_census()
        };
        for (drv, detail) in verdicts {
            if !seen.insert(drv.clone()) {
                continue;
            }
            let next = step_no_host_counter(
                actor.supply_reval.no_host_verdicts.get(&drv),
                census,
                &detail,
                pass,
            );
            if next.count >= NO_HOST_VERDICTS_TO_POISON {
                // Only the Ready loop population poisons — a drv that
                // already left Ready (cancelled, substituted,
                // completed) gets its track dropped instead.
                if actor
                    .dag
                    .node(&drv)
                    .is_some_and(|s| s.status() == DerivationStatus::Ready)
                {
                    poisons.push(NoHostPoison {
                        drv: drv.clone(),
                        detail,
                    });
                }
                actor.supply_reval.no_host_verdicts.remove(&drv);
            } else {
                actor.supply_reval.no_host_verdicts.insert(drv, next);
            }
        }
        // Opportunistic prune: tracks for drvs that left Ready are
        // dead (the controller stops minting their verdicts; bounded
        // map hygiene, the acked_spawned-retain precedent above).
        let dag = &actor.dag;
        actor.supply_reval.no_host_verdicts.retain(|h, _| {
            dag.node(h)
                .is_some_and(|s| s.status() == DerivationStatus::Ready)
        });
        poisons
    }
}

impl DagActor {
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
    /// routes through the memoized `solve::solve_full`
    /// (admissible-set), draws ε_h, applies the read-time ICE mask,
    /// and returns `nodeAffinity` over `A' \ masked`. Otherwise — or
    /// for override/probe/explore branches — it routes through
    /// `solve::intent_for` (hw-agnostic `solve_tier`) and returns an
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
        // merged_bug_057: the BestEffort lane's typed reason SURVIVES
        // to the emission classifier instead of flattening into the
        // `hw_emitted || infeasible.is_some()` bool — size- and
        // time-caused evidence dispatch differently at the agnostic
        // gate (`InfeasibleReason::is_size_infeasibility`).
        let mut hw_reason: Option<solve::InfeasibleReason> = None;
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
                    hw_reason = Some(why);
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
                //
                // r[impl scheduler.sla.ceiling.stale-solve-revalidation+2]
                // merged_bug_002 (R21): the law quantifies over
                // EMISSIONS, not over no-memo emissions — memo hits
                // re-classify after the overlays. The shared chokepoint
                // below raises mem to the clamped floor AFTER this arm
                // returns, and the floor is NOT in `inputs_gen` BY
                // DESIGN (a floor bump must not thrash the memo) — so a
                // post-memo floor bump into the (class-ceiling, global]
                // band killed every memoized cell at
                // `retain_hosting_cells` and emitted
                // `hw_class_names=[]` silently (only the misattributed
                // producer-regression strip warn fired). Predict the
                // post-overlay demand with the SAME formulas the
                // chokepoint applies; when no memoized cell survives
                // the live class ceilings, route through the
                // stale-demand walk and the shared disclosure fold —
                // the emission is then CLASSIFIED exactly as a no-memo
                // one would be.
                let fclamped =
                    super::floor::ClampedFloor::of(&state.sched.resource_floor, &self.sla_ceilings);
                let eff_cores = memo.a.c_star.min(self.sla_ceilings.max_cores as u32).max(1);
                let eff_mem = memo
                    .a
                    .mem_bytes
                    .max(fclamped.mem_bytes)
                    .min(self.sla_ceilings.max_mem);
                let survives = cells.iter().any(|(h, _)| {
                    let (cc, cm) = self.sla_config.class_ceilings(
                        h,
                        cost.catalog_ceilings(),
                        cost.resolved_global(),
                    );
                    eff_cores <= cc && eff_mem <= cm
                });
                if survives {
                    // bug_119: a memo emission whose cells survive the
                    // live ceilings is a HEALTHY letter too — the heal
                    // edge closes any open disclosure episode here
                    // exactly as the fold's Cells arm does (the memo
                    // path bypasses the fold by design when nothing
                    // re-classifies).
                    self.supply_reval.heal(
                        &tenant,
                        state.pname.as_deref().unwrap_or(""),
                        &state.drv_hash,
                    );
                    (
                        memo.a.c_star,
                        memo.a.mem_bytes,
                        memo.a.disk_bytes,
                        cells,
                        Some(memo.tier),
                    )
                } else {
                    let forced_demand = override_
                        .as_ref()
                        .is_some_and(|o| o.forced_cores.is_some() || o.forced_mem.is_some());
                    let emission = self.resolve_stale_demand(
                        state,
                        override_.as_ref().and_then(|o| o.capacity),
                        eff_cores,
                        eff_mem,
                        cost,
                        &feat,
                        forced_demand,
                    );
                    let (c2, m2, cells2) =
                        self.fold_cell_emission(emission, eff_cores, eff_mem, &tenant, state);
                    (c2, m2, memo.a.disk_bytes, cells2, Some(memo.tier))
                }
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
                // `retain_hosting_cells` filters-then-expands (it
                // closes over RETAINED classes' declared ladder
                // rungs; merged_bug_004) but never RECOVERS a
                // producer-rejected cell, so an over-cap override
                // that makes the producer reject every class yields
                // `node_affinity=[]`, and the chokepoint can't
                // recover. With `[]` affinity, a featured intent's pod
                // lands without the feature affinity and crashloops
                // (no `RioNodeclaimPoolNoHostingClass` alert: the
                // controller's `fallback_cell` reads the POST-clamp
                // cores and succeeds). The shared chokepoint clamp at
                // the end of `solve_intent_for` re-applies the same
                // bounds — this pre-clamp is idempotent under it.
                let c = c.min(self.sla_ceilings.max_cores as u32).max(1);
                // live_051(d): the floor max consumes the CLAMPED
                // projection — a stale persisted floor (minted under a
                // larger old global) can never re-raise demand past
                // the live ceiling at this seam.
                let m = m
                    .max(
                        super::floor::ClampedFloor::of(
                            &state.sched.resource_floor,
                            &self.sla_ceilings,
                        )
                        .mem_bytes,
                    )
                    .min(self.sla_ceilings.max_mem);
                // r[impl scheduler.sla.ceiling.stale-solve-revalidation+2]
                // live_050(e)/live_051(b): the emission chokepoint
                // mints a TOTAL typed outcome and folds it with zero
                // wildcard arms — the pre-fix shape emitted `[]` for
                // every unroutable case (stale demand, infeasible-
                // everywhere, feature gaps, genuine agnosticism alike)
                // and the controller churned the non-agnostic ones as
                // `no_hosting_class` forever (the measured live loop).
                // merged_bug_057: the reason crosses the classifier
                // boundary TYPED — `hw_reason` (the BestEffort lane)
                // or `infeasible` (intent_for's fallthrough); at most
                // one is Some (`memo_entry.is_some() ⟹ hw_emitted`
                // gates intent_for's emit, and a BestEffort solve
                // short-circuits `full` to None before intent_for
                // runs), and the classifier's agnostic gate keys on
                // the SIZE axis only.
                let infeasible_evidence = hw_reason.or(infeasible);
                // Operator-forced dims are pins, never stale solver
                // evidence — the classifier refuses to clamp them.
                let forced_demand = override_
                    .as_ref()
                    .is_some_and(|o| o.forced_cores.is_some() || o.forced_mem.is_some());
                let emission = self.classify_cell_emission(
                    state,
                    override_.as_ref().and_then(|o| o.capacity),
                    c,
                    m,
                    cost,
                    &tenant,
                    &feat,
                    infeasible_evidence,
                    forced_demand,
                );
                let (c, m, cells) = self.fold_cell_emission(emission, c, m, &tenant, state);
                (c, m, d, cells, None)
            }
        };
        // r[impl sched.sla.reactive-floor+4]
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
        // live_051(d): mem/disk floor dimensions consume the CLAMPED
        // projection (stale persisted floors are grounded at the live
        // ceilings on every read — see `actor::floor::ClampedFloor`);
        // the deadline dimension keeps the raw floor, capped by
        // DEADLINE_CAP_SECS below (Ceilings has no time axis).
        let floor = &state.sched.resource_floor;
        let fclamped = super::floor::ClampedFloor::of(floor, &self.sla_ceilings);
        let cores = cores.min(self.sla_ceilings.max_cores as u32).max(1);
        let mem = mem.max(fclamped.mem_bytes).min(self.sla_ceilings.max_mem);
        let disk = disk
            .max(fclamped.disk_bytes)
            .min(self.sla_ceilings.max_disk);
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
            // merged_bug_004: the chokepoint inherits the operator
            // pin — the ladder expansion may only close over
            // pin-capacity rungs, and an off-pin producer cell strips
            // loud (the axis every producer arm enforces upstream).
            override_.as_ref().and_then(|o| o.capacity),
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
        // live_051(d): THE load-bearing floor read — pre-fix this max
        // consumed the RAW floor with no ceiling bound, so a stale
        // persisted floor re-raised mem PAST the live global AFTER the
        // caller's pre-clamp, drove `reference_hw_class_for_system` to
        // None, and fed the silent empty-cells channel. The clamped
        // projection makes the bypass max ≤ the live global by type.
        let mem = mem.max(
            super::floor::ClampedFloor::of(&state.sched.resource_floor, &self.sla_ceilings)
                .mem_bytes,
        );
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
            // post-finalize `retain_hosting_cells` chokepoint
            // filters-then-expands (the ladder closure over RETAINED
            // classes — merged_bug_004) but never RECOVERS a cell the
            // producer rejected on a pre-clamp `cores > ceiling`, so
            // the producer-side size filter and the chokepoint must
            // agree on the demand they evaluate. `solve_intent_for`'s
            // `None` arm pre-clamps before calling here. (Pre-r40 this
            // comment claimed "the post-finalize chokepoint catches
            // the post-clamp delta" — that claim IS the bug.)
            // merged_bug_067: the resolver consumes the pin as a TYPED
            // axis (mb_003's caller-side `capacity_types_for` gate
            // folded into it) — a pin the REFERENCE class refuses but a
            // SIBLING hosts now routes to the sibling at the pinned
            // capacity instead of dropping empty (where the controller
            // `fallback_cell`'s first-cap silently INVERTED the pin).
            Some(cap) => match self.sla_config.reference_hw_class_for_system(
                &state.system,
                cores,
                mem,
                &feat,
                cost.catalog_ceilings(),
                cost.resolved_global(),
                Some(cap),
            ) {
                Some(h) => vec![(h.to_owned(), cap)],
                // r31 A3 (re-keyed at merged_bug_067): NO configured
                // class hosts the pin at this size. When the pin is
                // the BINDING axis (a size-hosting class exists
                // ignoring it), the debounced WARN keeps the operator
                // signal (the pin cannot be honored as configured) and
                // the classifier mints the typed `PinGated`; when even
                // the cap-blind resolve fails, the size/feature axes
                // bind and the classifier walk discloses
                // (StaleSolve/Unhostable) — no warn here, the pin is
                // not the differentiator.
                None => {
                    let cap_is_binding = self
                        .sla_config
                        .reference_hw_class_for_system(
                            &state.system,
                            cores,
                            mem,
                            &feat,
                            cost.catalog_ceilings(),
                            cost.resolved_global(),
                            None,
                        )
                        .is_some();
                    if cap_is_binding {
                        let pname = state.pname.clone().unwrap_or_default();
                        if self
                            .cap_mismatch_warned
                            .lock()
                            .put((tenant.to_owned(), pname, cap), ())
                            .is_none()
                        {
                            tracing::warn!(
                                %tenant,
                                pname = state.pname.as_deref().unwrap_or(""),
                                ?cap,
                                "`--capacity` override pin hosted by NO \
                                 configured hwClass at this size (size-hosting \
                                 classes exist without the pin) — cells \
                                 emitted empty, the emission classifies as \
                                 PinGated; change the pin to a hosted cap, or \
                                 add the cap to a routing class's \
                                 `[sla.hw_classes.<h>].capacity_types`",
                            );
                        }
                    }
                    Vec::new()
                }
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
            // §13e: `feat = [fetcher]` for FODs, so cold-start FODs
            // land here and route to `fetcher-*` — the same
            // chicken-egg fix as kvm-cold-start.
            //
            // live_050(e): an EMPTY return from these arms is no
            // longer the end state — `classify_cell_emission` wraps
            // this fn and types every empty exit through the
            // `CellEmission` alphabet (HwAgnostic stays quiet BY
            // TYPE; stale/over-ceiling demand RE-SOLVES into the
            // largest live hosting class; a genuine feature/arch gap
            // is `Unhostable` + loud). Pre-fix, `[]` here was
            // indistinguishable on the wire from genuine hw-agnostic
            // demand — the measured silent-starvation channel.
            None if !feat.is_empty() => self
                .sla_config
                .reference_hw_class_for_system(
                    &state.system,
                    cores,
                    mem,
                    &feat,
                    cost.catalog_ceilings(),
                    cost.resolved_global(),
                    None,
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

    // r[impl scheduler.sla.ceiling.stale-solve-revalidation+2]
    /// live_050(e)/live_051(b): classify one no-memo cell emission into
    /// the TOTAL [`CellEmission`] alphabet (the memo arm enters the
    /// same alphabet through [`Self::resolve_stale_demand`] when its
    /// post-overlay survival check fails — merged_bug_002). Wraps
    /// [`Self::bypass_cells`] (whose operator-pin lane keeps its own
    /// r31 A3 disclosure machinery untouched) and types every EMPTY
    /// exit — the pre-fix silent population:
    ///
    /// - non-empty routing → `Cells` (the unchanged §13d arms);
    /// - ∅ features + no SIZE-infeasibility evidence (+ no pin) →
    ///   `HwAgnostic` — the genuinely quiet edge, preserved by type;
    ///   time-only evidence (SerialFloor/InterruptRunaway) KEEPS this
    ///   lane (merged_bug_057: the typed reason crosses the boundary,
    ///   `InfeasibleReason::is_size_infeasibility` keys the gate);
    /// - a `--capacity` pin a size-hosting class refuses → `PinGated`
    ///   (bypass already warned, debounced);
    /// - demand no class hosts at SIZE, with routing candidates →
    ///   `StaleSolve` — the revalidation arm: re-solve under the LIVE
    ///   ceilings by clamping into the largest hosting class (the
    ///   demand was authorized by a ceiling vector that no longer
    ///   exists — stale floors, shrunk catalogs, or an over-global
    ///   solve all land here); the premise is `resolved != solved` —
    ///   evidence-carrying demand that FITS the best class routes as
    ///   plain `Cells` with no stale disclosure;
    /// - no routing candidate at all, or a clamped floor above the
    ///   best candidate's ceiling → `Unhostable` with the WHY (demand
    ///   + best class) so every consumer can derive the delta.
    ///
    /// The re-solve is per-emission revalidation (T2: the envelope
    /// consumes the ceiling-witness that authorized it) — boots,
    /// reloads, and mid-run shrinks are all observed at the next
    /// emission pass, durable rows included, because the validation is
    /// read-time against `cost.catalog_ceilings()`/`resolved_global()`.
    /// Zero new per-pass complexity class: the pre-fix arm already
    /// called `reference_hw_class_for_system` per intent; the
    /// candidate walk below runs only for the EMPTY population.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn classify_cell_emission(
        &self,
        state: &crate::state::DerivationState,
        cap: Option<crate::sla::config::CapacityType>,
        cores: u32,
        mem: u64,
        cost: &crate::sla::cost::CostTable,
        tenant: &str,
        feat: &[String],
        infeasible: Option<crate::sla::solve::InfeasibleReason>,
        forced_demand: bool,
    ) -> CellEmission {
        let routed = self.bypass_cells(state, cap, cores, mem, cost, tenant);
        if !routed.is_empty() {
            return CellEmission::Cells(routed);
        }
        let arch = rio_common::k8s::system_to_k8s_arch(&state.system);
        if feat.is_empty() {
            // Arch-unmappable featureless demand can never route
            // (the r35 B1 guard in `reference_hw_class_for_system`);
            // and featureless demand with no SIZE-infeasibility
            // evidence and no pin is the designed agnostic lane —
            // the controller's `fallback_cell` arch-matches it.
            // merged_bug_057: the gate keys on the TYPED reason, not
            // "any infeasibility" — time-only evidence (SerialFloor,
            // InterruptRunaway) leaves the demand hostable by every
            // class, and routing it into the mem-largest class
            // concentrated demand on the most expensive class exactly
            // under capacity/interrupt pressure.
            if arch.is_none()
                || (cap.is_none() && !infeasible.is_some_and(|r| r.is_size_infeasibility()))
            {
                return CellEmission::HwAgnostic;
            }
        }
        if let Some(pinned) = cap
            && self
                .sla_config
                .reference_hw_class_for_system(
                    &state.system,
                    cores,
                    mem,
                    feat,
                    cost.catalog_ceilings(),
                    cost.resolved_global(),
                    None,
                )
                .is_some()
        {
            // merged_bug_067 (the letter's premise): PinGated mints
            // only when the PIN is the binding axis — a size-hosting
            // class exists IGNORING the pin (the cap-blind probe
            // above) while no class hosts it WITH the pin (asserted
            // below: `bypass_cells`' Some-arm resolver is pin-aware,
            // so any pin-honoring class would have routed before this
            // point). The pre-fix capacity-blind probe pre-empted the
            // pin-aware walk whenever ANY size-hosting class existed,
            // silently inverting the pin via the controller fallback
            // while a pin-honoring sibling route existed.
            debug_assert!(
                self.sla_config
                    .reference_hw_class_for_system(
                        &state.system,
                        cores,
                        mem,
                        feat,
                        cost.catalog_ceilings(),
                        cost.resolved_global(),
                        Some(pinned),
                    )
                    .is_none(),
                "PinGated premise: no pin-honoring class exists (bypass \
                 routes pin-honoring siblings before classification)"
            );
            return CellEmission::PinGated;
        }
        self.resolve_stale_demand(state, cap, cores, mem, cost, feat, forced_demand)
    }

    /// The stale-demand re-solve walk — the classifier's tail, shared
    /// with the memo arm's post-overlay re-classification
    /// (merged_bug_002): given demand no routed/bypass cell hosts,
    /// walk the routing candidates IGNORING size, pick the largest
    /// live hosting class, and mint the typed letter. The agnostic
    /// and pin gates do NOT apply here: the memo arm's population was
    /// hw-routed at solve time (never agnostic), and its capacity pin
    /// was already honored by the in-arm `all_candidates ∩ {cap}`
    /// filter — `cap` here only constrains the candidate walk.
    #[allow(clippy::too_many_arguments)]
    fn resolve_stale_demand(
        &self,
        state: &crate::state::DerivationState,
        cap: Option<crate::sla::config::CapacityType>,
        cores: u32,
        mem: u64,
        cost: &crate::sla::cost::CostTable,
        feat: &[String],
        forced_demand: bool,
    ) -> CellEmission {
        let arch = rio_common::k8s::system_to_k8s_arch(&state.system);
        // Routing candidates IGNORING size — the re-solve universe.
        let catalog = cost.catalog_ceilings();
        let global = cost.resolved_global();
        let mut cands: Vec<&str> = self
            .sla_config
            .hw_classes
            .keys()
            .map(String::as_str)
            .filter(|h| self.sla_config.class_routes(h, arch, feat))
            .filter(|h| cap.is_none_or(|c| self.sla_config.capacity_types_for(h).contains(&c)))
            .collect();
        cands.sort_unstable();
        // Largest by (mem ceiling, cores ceiling) — mem-major because
        // the floor (the un-clampable demand component) is a mem/disk
        // axis; ties resolve to the lexicographically last (sorted +
        // max_by keeps the last maximum) for determinism.
        let best = cands
            .into_iter()
            .map(|h| (h, self.sla_config.class_ceilings(h, catalog, global)))
            .max_by(|a, b| (a.1.1, a.1.0).cmp(&(b.1.1, b.1.0)));
        let Some((best_h, (bcc, bcm))) = best else {
            return CellEmission::Unhostable {
                demand: (cores, mem),
                best_class: None,
            };
        };
        // OPERATOR-FORCED demand is a pin, not stale solver evidence —
        // the re-solve clamp would silently rewrite `--cores`/`--mem`
        // (the bug_019 (a)-inverse law: oversized forced demand MUST
        // emit empty so the controller's `fallback_cell` reaches its
        // own None — now ANSWERED by the live_051(c) verdict loop,
        // which poisons the drv with an actionable detail instead of
        // looping silently). Typed loud, never clamped.
        if forced_demand {
            return CellEmission::Unhostable {
                demand: (cores, mem),
                best_class: Some((best_h.to_owned(), (bcc, bcm))),
            };
        }
        // live_051(d): the floor is the demand component a re-solve
        // cannot clamp away — consume it CLAMPED (the projection law)
        // and refuse when even the best candidate sits below it.
        let fclamped =
            super::floor::ClampedFloor::of(&state.sched.resource_floor, &self.sla_ceilings);
        if fclamped.mem_bytes > bcm {
            return CellEmission::Unhostable {
                demand: (cores, mem),
                best_class: Some((best_h.to_owned(), (bcc, bcm))),
            };
        }
        let resolved = (cores.min(bcc).max(1), mem.min(bcm).max(fclamped.mem_bytes));
        let cells: Vec<crate::sla::config::Cell> = match cap {
            // The pin survives the re-solve: one cell at the pinned
            // capacity (candidates were pre-filtered to pin hosts).
            Some(c) => vec![(best_h.to_owned(), c)],
            // Mirror the §13d cold-start arm: every configured cap of
            // the chosen class, so the controller mints the matching
            // cell and the ladder closure can extend it.
            None => self
                .sla_config
                .capacity_types_for(best_h)
                .iter()
                .map(|c| (best_h.to_owned(), *c))
                .collect(),
        };
        // merged_bug_057: the StaleSolve premise is *the re-solve
        // CHANGED the demand* — `resolved != solved`. Demand that fits
        // the best hosting class as-is has nothing stale to disclose
        // (the live ceilings host it); it routes as plain `Cells` at
        // that class. Pre-fix this arm minted StaleSolve with
        // `resolved == solved`, a false "no longer hostable" WARN, and
        // an `exit=stale_resolved` increment for every
        // evidence-carrying fitting emission.
        if resolved == (cores, mem) {
            return CellEmission::Cells(cells);
        }
        // T1's premise assert at the mint site (kept even though the
        // branch above makes it trivially true today — it survives
        // refactors that delete the branch).
        debug_assert_ne!(
            resolved,
            (cores, mem),
            "StaleSolve premise: the re-solve must CHANGE demand"
        );
        CellEmission::StaleSolve {
            solved: (cores, mem),
            live_max: (bcc, bcm),
            class: best_h.to_owned(),
            resolved,
            cells,
        }
    }

    /// Fold one [`CellEmission`] into the `(cores, mem, cells)` triple
    /// the post-finalize chokepoint consumes, applying each letter's
    /// disclosure side-effects (the `exit`-labeled
    /// `rio_scheduler_sla_hw_ladder_exhausted_total` increments + the
    /// WARNs, debounced per `(tenant, pname, kind)` by
    /// [`SupplyRevalidation::disclose_once`]).
    ///
    /// ONE fold for both producer arms (merged_bug_002 / R21): the
    /// no-memo classify path and the memo arm's post-overlay
    /// re-classification route through the same machinery, so a letter
    /// minted on either arm is observably identical — a second fold
    /// would be a sibling disclosure surface that drifts.
    fn fold_cell_emission(
        &self,
        emission: CellEmission,
        cores: u32,
        mem: u64,
        tenant: &str,
        state: &crate::state::DerivationState,
    ) -> (u32, u64, Vec<crate::sla::config::Cell>) {
        match emission {
            // Healthy letters close any open disclosure episode
            // (bug_119: the heal edge, visible exactly here — a
            // routed/agnostic emission means the ceilings host the
            // demand again, so a relapse must disclose anew).
            CellEmission::Cells(cells) => {
                self.supply_reval.heal(
                    tenant,
                    state.pname.as_deref().unwrap_or(""),
                    &state.drv_hash,
                );
                (cores, mem, cells)
            }
            // Genuinely hw-agnostic (∅ features, no
            // infeasibility evidence) — the §13e cold-start
            // quiet edge survives BY TYPE, not by shared
            // emptiness (R18's regression pin).
            CellEmission::HwAgnostic => {
                self.supply_reval.heal(
                    tenant,
                    state.pname.as_deref().unwrap_or(""),
                    &state.drv_hash,
                );
                (cores, mem, Vec::new())
            }
            // Operator `--capacity` pin not hosted — the r31
            // A3 lane already disclosed (debounced warn in
            // `bypass_cells`); emission stays empty so the
            // pin is never silently rewritten.
            CellEmission::PinGated => (cores, mem, Vec::new()),
            CellEmission::StaleSolve {
                solved,
                live_max,
                class,
                resolved,
                cells,
            } => {
                // Re-solve disclosed: demand authorized under
                // a stale/over-global ceiling is clamped into
                // the largest live hosting class instead of
                // emitting empty cells (clamp-with-disclosure
                // — §5-S: "the system shouldn't hang").
                if self.supply_reval.disclose_once(
                    tenant,
                    state.pname.as_deref().unwrap_or(""),
                    "stale_resolved",
                    &state.drv_hash,
                ) {
                    ::metrics::counter!(
                        "rio_scheduler_sla_hw_ladder_exhausted_total",
                        "tenant" => tenant.to_owned(),
                        "exit" => "stale_resolved",
                    )
                    .increment(1);
                    tracing::warn!(
                        %tenant,
                        pname = state.pname.as_deref().unwrap_or(""),
                        solved_cores = solved.0,
                        solved_mem = solved.1,
                        live_max_cores = live_max.0,
                        live_max_mem = live_max.1,
                        class = %class,
                        resolved_cores = resolved.0,
                        resolved_mem = resolved.1,
                        "demand envelope no longer hostable under the \
                         live ceilings — re-solved (clamped) into the \
                         largest hosting class instead of emitting \
                         empty cells",
                    );
                }
                (resolved.0, resolved.1, cells)
            }
            CellEmission::Unhostable { demand, best_class } => {
                // No class can host even re-solved — typed +
                // loud, never empty-silent. The controller
                // answers its own config gaps with the
                // IntentVerdict loop (live_051(c)).
                if self.supply_reval.disclose_once(
                    tenant,
                    state.pname.as_deref().unwrap_or(""),
                    "unhostable",
                    &state.drv_hash,
                ) {
                    ::metrics::counter!(
                        "rio_scheduler_sla_hw_ladder_exhausted_total",
                        "tenant" => tenant.to_owned(),
                        "exit" => "unhostable",
                    )
                    .increment(1);
                    tracing::warn!(
                        %tenant,
                        pname = state.pname.as_deref().unwrap_or(""),
                        demand_cores = demand.0,
                        demand_mem = demand.1,
                        best_class = ?best_class,
                        "demand is unhostable by every configured \
                         hw class (feature/arch-constrained or floor \
                         above the best class ceiling) — emitting \
                         typed-empty; fix the class config or the \
                         demand",
                    );
                }
                (cores, mem, Vec::new())
            }
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
