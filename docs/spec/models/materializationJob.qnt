// -*- mode: Bluespec; -*-
// quint-policy-latches: covLiveOnlyAll, failoverDroppedJobs, staleJobWriteApplied, dupJob, requeueReadyJustifiedAll, abortChargeFree, reprobeCongruentAll, probeUnchargedOn429All, anyTenantHitServesAll, localServeVisibleAll, walkFoldSeen, stampLawAll, stampFoldSeen
//
// Substitution-replacement model (Phase C-prime, re-targeted to the
// post-deletion system at Phase D-prime T-D7.2): the store-owned
// materialization-job lifecycle AS BUILT at the D-prime tip — the ONLY
// substitution mechanism (the walk machinery, the Substituting status,
// the coexistence flag and both evidence columns were deleted in Waves
// D3-D6; migration 080). The six Phase-B behavior deltas are encoded
// here (the C-prime handoff delta list, encoded per-delta below),
// with the D-prime
// adjustment recorded per affected delta:
//
//   Delta 1 — the pruned-origin discriminator (c952e5a51,
//     sched.materialize.routing; post-D-prime form): the 063 mark
//     column is DROPPED (migration 080) — the durable fact is
//     `materialization_jobs.origin = 'pruned'` on the unresolved row
//     (Wave D2.1 keyed the settlement fail-fast on it, with the PD-D1
//     pruned-wins dedup upgrade). The per-drv `topdownPruned` ghost is
//     the model image of "an unresolved-or-parked pruned-origin job
//     exists": set by pruned-origin creation AND by the dedup upgrade
//     (`dedupPrunedUpgrade` — an OPruned creation landing on a live
//     job upgrades its origin in place, armament preserved); ended by
//     RESOLUTION ITSELF (there is no column to clear: success,
//     from-source, the fail-fast, cancellation and obsolescence all
//     resolve the row, and the pruned fact dies with the row's
//     unresolved state — the §4 clear-mirror is structural now).
//     arm-3 FailFast requires the FOUR-conjunct corner (Broken
//     evidence AND pruned-origin AND a live-wanted path missing AND
//     confirmed-missing-or-spent-one-shot); an UNMARKED node's arm 3
//     resolves from source (materialize.rs route_unobtainable).
//   Delta 2a — the finding-18 transport (ce17c6445,
//     store.materialize.executor+2): per-replica channel-staleness
//     state; failover pins every executor channel to the deposed
//     replica (the kube-proxy ClusterIP shape) and claims/reports are
//     blocked until `redialChannel` — the model of the
//     abandon-and-redial-on-UNAVAILABLE behavior
//     (rio-store/src/materialize/client.rs:338-365). The pre-fix
//     dead-end (no redial: claims blocked forever) is the
//     witness-flip calibration (canReachPostFailoverClaim).
//   Delta 2b — the finding-18 wanted backfill (056bfc9b6): legacy
//     interest is representable (`legacyInterest`: in-memory interest
//     with NO wanted rows — post-D-prime this is the PRE-RELATION-era
//     build population, the DQ-2 wanted-width transition residual,
//     not a flag state; the coexistence flag is gone); probe-partition
//     and stale-reset creation BACKFILL the relation for every live
//     legacy-interested build (materialize.rs, the
//     create_materialization_job live_interested loop); the invariant
//     `creationLeavesTenantResolvable` pins it.
//   Delta 3 — the park re-evaluation split (642fe9fb7, T-6.1/PD-20):
//     `parkReevaluate` resolves Vouched/Pending-evidence parked jobs
//     from source per tick (materialize.rs
//     tick_reevaluate_parked_materialization_jobs); Broken-evidence
//     jobs STAY parked (the stalled-gauge population — the
//     `stalledJobs` derived val) until `parkBackoffExpires`. The
//     budget counter is NOT reset at unpark (as built:
//     park_materialization_job derives backoff from the surviving
//     count); the park itself rides the charge chokepoint
//     (charge_materialization_infra fuses charge + kernel verdict —
//     materialize.rs), not a separate transition, and parking is
//     PARTY-BLIND (the bughunt-wave Q5 reversal of counter-signed
//     residual (a), owner-signed 2026-06-03): worker InfraFailure and
//     establishment charges count toward the SAME per-job budget
//     window and both park at MAT_BUDGET (config.rs
//     conversion_requires_worker_charge doc — "worker-reported AND
//     establishment-written, both channels charge the same budget").
//     The per-job window is the 085 job-creation reset row's image:
//     freshJob's zeroed counts ARE the window (each job's ledger
//     suffix starts at its creation reset).
//   Delta 4 — five origins, split posture (T-1.5/T-1.6/T-5.2,
//     finding 20): `OReprobe` added (carries the AS-5 6d status
//     reset, merge.rs:934 apply_reprobe_reset_in_memory);
//     `OStaleReset` resets FROM Completed in the same action
//     (merge.rs verify_preexisting_completed — the NCompleted
//     precondition the draft carried is REVERSED for this origin);
//     the 3-in-tx/2-standalone-fenced posture is below this model's
//     atomicity granularity (every creation is one fenced atomic
//     action either way) — recorded, not encoded; the in-tx origins
//     require the creating tenant's wanted rows present (the same-tx
//     batch), the standalone sites backfill instead.
//   Delta 5 — recovery rebuilds the job view (4e57180fd, T-4.3): an
//     explicit in-memory `view` layer (the kernel admission's input —
//     recovery.rs:197 rebuild_materialization_job_view); failover
//     rebuilds it EAGERLY and faithfully (claim holders, park
//     expiries); the dispatch-probe dedup re-feed (`dedupRefeed`)
//     preserves armament state (the or_insert discipline,
//     materialize.rs:160-176); `unresolvedJobAlwaysArmed` is
//     re-encoded as view-faithfulness (view == projection of the
//     durable state), which the F10/L1 view-drop and B5a
//     refeed-overwrite calibrations falsify.
//   Delta 6 — Queued claims (8d5caee2d + 7c9b9f949/B1): `claimJob`
//     admits NQueued|NReady and carries NO mark conjunct (the claim IS
//     the substitution — the B1 upgrade). D-prime adjustment: the
//     builderPull must_substitute refusal arm is DELETED with the
//     kernel arm it modeled (Wave D4.1 collapsed the kinded admission;
//     D5.2 reduced the kernel to the durable child relation). The
//     dual-predicate window is EMPTY post-D-prime — the equivalence:
//     the pruned fact lives only on an unresolved-or-parked job row
//     (delta 1), any unresolved job projects a non-None JobView
//     (VPending carries parked too), and builderPull already requires
//     JobView::None — so the dropped conjunct was vacuous. The
//     equivalence holds in-model because EVERY resolution arm (incl.
//     cancellation and obsolescence) ends the ghost with the row; the
//     B1 regression guard is the canReachMarkedClaim witness (claims
//     on pruned-origin nodes stay reachable).
//
// WHAT THIS MODELS (design §2, §9.1) — unchanged from the draft:
// job creation at the scheduler's classification sites; the durable
// per-(build, derivation) wanted relation (§6) and the interest
// derived from it; store-replica claim through the pull protocol
// (kind=MATERIALIZATION, the one-winner admission arm); in-store
// per-path ingest + pin-at-ingest; outcome reporting and the
// consumption transactions (Success coverage, the four-arm
// Unobtainable routing); the establishment sweep
// (materialization_infra, never executor_crash, never adopt, never
// park); the charge-free cancellation closer; obsolete-on-produced;
// the materialization budget with park-not-fail; the §5.3 pin release
// (job resolved AND all interest terminal) and store GC vs pins; the
// build-side environment (builder pulls, from-source dispatch, the
// dispatch-time store short-circuit); leader failover; upstream
// availability drift.
//
// WHAT IS NOT MODELED (priced omissions; each cites its out-of-model
// owner):
//   - Walk machinery and the Substituting status: DELETED from
//     production (D-prime Waves D3-D6; the predecessor record is
//     closureEvidence.qnt and its survivors core). The F14 calibration
//     family is by-construction here.
//   - DAG edges / multi-node closures: the closure walk is "ingest
//     the wanted set path by path" (closure incompleteness surfaces
//     as a wanted path still missing); parent/child soundness stays
//     with the closureEvidence survivors (B9/B10 kept guards).
//   - Poison: the reprobe origin's poison_cleared row is recorded as
//     out-of-model (no poison state exists here; the retryPolicy
//     coexistence regime owns the poison-partition invariants).
//   - The probe's advisory verdict at creation, byte progress,
//     gateway events, metrics (display-only).
//   - The backoff clock: park/expiry are states, not timers.
//   - The probe-site backfill's per-write fence failure (a failed
//     standalone wanted write degrades to the MD-D1 park-alert
//     posture as built; the model folds the backfill into the
//     creation atom).
//   - The build-failure fan-out of the fail-fast: as built the NODE
//     goes Queued+interest-stripped while the BUILDS fail
//     (dispatch.rs:1102); the model's NFailed carries the verdict on
//     the node — the per-build projection is out of scope.
//   - The resubmit-after-fail-fast lane (merge resetNodes): out of
//     the §9.1 alphabet; consequence: the drv-level one-shot ledger
//     never resets in-model, which matches the as-built
//     count_materialization_rows_in_history read (drv history, NOT
//     per-job — the draft's per-job read was a wrong-verdict
//     encoding; see the C-prime stage record).
//
// THE BOUNDED REPRESENTATION (SC-1): bounded per-drv counters for the
// kind-partitioned ledger suffix; ceilings as action preconditions
// (MAX_EXECS, MAX_GEN, MAX_UPSTREAM_FLIPS, MAX_LEGACY, the
// MAT_BUDGET+2 ledger ceilings). Ghost latches (the at-decision-time
// justification family) are monotone and constant-true under the
// as-built actions — they add zero reachable states and exist for the
// calibration overrides to falsify.
//
// PROPERTIES: the §9.1 successor table (17 named vals) + the three
// C-prime re-encodes (routingRequiresDurableVouchOrFailFast /
// noWrongfulTerminalFailure / noWrongfulFromSourceRouting are
// at-decision-time ghost latches now, not current-state reads) + the
// delta-derived invariants (creationLeavesTenantResolvable,
// materializationCrashChargedOnce, crossBuildWantedIsolation).
// Witnesses: one canReach* per contended scenario (see section 6b).
// The calibration-transfer table (which override falsifies which
// property, and the by-construction rows) lives in
// calibration/README.md (the materializationJob verdict section);
// the override modules are docs/spec/models/calibration/mat-*.qnt.
// Measured state counts / depths / wall-clocks for the WIRED checks
// live in the introducing commits and the check transcripts, never
// here; the unconverged MANUAL-TARGET coordinates (the budget
// record) are the MEASUREMENT block below.
//
// MEASUREMENT BLOCK (the exhaustive budget record, relocated from
// the retired substitution-replacement invariant map, C-prime stage
// record; A3/A4 update: the manual sweep now spans the 36-invariant
// post-A4 conjunction — the bounded-simulation holds checks plus the
// calibration pins are the gate-wired deliverable, unchanged in
// shape from C-prime):
// ### Exhaustive measurement table (the budget record)
//
// Coordinates: TLC BFS, 60 workers (the house-budget worker count),
// 10-min caps for the Ex measurements (the 15-min-hard rule's
// determination form: every run below was still mid-frontier with a
// growing queue at its cap — none is within reach of a 30-min
// convergence either, per the B1-flip 24.5-min/192-worker corroboration
// at the BaseEx scope). Manual-target command shape:
//
//     quint verify --backend=tlc --main=materializationJob<Scope>Ex \
//       --invariant=<the 21-name §9.1 conjunction> \
//       docs/spec/models/materializationJob.qnt
//
// | Scope (as-built step, full conjunction) | Verdict @ cap | Bounded coverage |
// |---|---|---|
// | materializationJobBaseEx | unconverged @ 10 min | zero violation through 435,251 distinct / depth 14 |
// | materializationJobFailoverEx | unconverged @ 10 min | zero violation through 500,327 distinct / depth 14 |
// | materializationJobAdversarialStoreEx | unconverged @ 10 min | zero violation through 406,878 distinct / depth 16 |
// | materializationJobStaleTenureEx | unconverged @ 10 min | zero violation through 475,795 distinct / depth 14 |
// | materializationJobCrashLoopEx | unconverged @ 10 min | zero violation through 400,001 distinct / depth 14 |
// | (corroboration) matCalibB1ClaimRefusesMarked @ calibStep — a strict SUBSET of the as-built BaseEx space | unconverged @ 24.5 min, auto(192) workers | zero violation through 2,338,887 distinct / depth 18, queue still growing — the superset (as-built BaseEx) therefore cannot converge at 30 min either |
//
// Design-scale (2-drv) regimes are strict supersets of their Ex scopes
// and are not separately measured (the executor/closure precedent: a
// scope whose reduction does not converge is recorded once). The
// `longChecks` Tier-2 mechanism stays empty — no conjunction fits the
// 15-30-min window; pressure to change shard knobs remains
// stop-and-report per the standing rule.
//
// Width posture (the orchestrator resource directive, applied): the
// 60-worker rows above are the reference-budget coordinates (comparable
// to the house budgets and the Wave-4 floor); the directive's
// wide-instance allowance was additionally exploited for a deepening
// wave on the two most-cited bounded-coverage rows — scale-annotated
// below their reference entries:
//
// | Deepened row | @ 90 workers (15-min cap) |
// |---|---|
// | matCalibF1NoPresenceRecheck × the §9.1 conjunction (calibStep) | zero violation through 818,905 distinct / depth 15 (unconverged @ 15-min cap) |
// | materializationJobBaseEx × the §9.1 conjunction (as-built) | zero violation through 919,952 distinct / depth 16 (unconverged @ 15-min cap) |
//
module materializationJob {
  // --- 1. types -----------------------------------------------------------

  /// Job lifecycle states (§2.4, §6). Parked is NOT a state: it is a
  /// flag on a pending job (state stays pending; the §2.5 park).
  type JobState =
    | JPending
    | JClaimed
    | JResolvedSuccess
    | JResolvedFromSource
    | JResolvedUnobtainable
    | JObsolete
    | JCancelled

  /// Where the job came from — the FIVE §2.1 creation sites (delta 4).
  /// OProbe and OMerge are both `cache_opportunity` rows as built (the
  /// dispatch-probe standalone-fenced site vs the merge new_sub in-tx
  /// site); the model keeps them distinct because their POSTURE
  /// differs (the standalone sites backfill legacy interest, delta 2b).
  type JobOrigin = | OProbe | OMerge | OPruned | OReprobe | OStaleReset

  /// One job record (state x origin x park x resolution-exec-id), plus
  /// the §6 columns the properties read (tenant, the budget counters).
  type Job = {
    state: JobState,
    origin: JobOrigin,
    parked: bool,
    resolutionExec: int,
    tenant: str,
    matInfraCount: int,
    matUnobtainableCount: int,
  }
  type JobOpt = | NoJob | SomeJob(Job)

  /// Node (derivation) status. NFailed carries the fail-fast verdict
  /// (see the priced-omission note on the build fan-out).
  type NodeStatus =
    | NQueued
    | NReady
    | NRunning
    | NCompleted
    | NFailed
    | NDepFailed

  /// Attempt kind (drv_executions.attempt_kind / dispatch_mode).
  type AttemptKind = | KBuild | KMaterialization

  /// The open attempt on a derivation (closed attempts live on as
  /// ledger counters).
  type Attempt = {
    kind: AttemptKind,
    instance: str,
    execId: int,
  }
  type AttemptOpt = | NoAttempt | OpenAttempt(Attempt)

  /// Pin state for one (derivation, output) store path (§5).
  type PinKind = | NoPin | PinBuildInput | PinMaterialization

  /// The durable declared-relation classification the routing arms read
  /// (§2.4 arms 1/2: Vouched / Pending; Broken = childless or holed).
  type DurableRelation = | RVouched | RPending | RChildlessLeaf | RHoled
  // A4 (bughunt wave): RBroken split into the two broken cells the
  // 4-cell classifier persists — RChildlessLeaf (broken by structure:
  // no declared children; from-source viable while unmarked) and
  // RHoled (missing produced children: the genuine closure hole, the
  // stalled population's cell, never from-source viable by itself).

  /// The in-memory job view per derivation (delta 5) — the kernel
  /// admission's JobView input. VPending carries the parked flag;
  /// VClaimed carries the holder identity.
  type ViewState = | VNone | VPending(bool) | VClaimed(str)

  // --- 2. constants (const = overridable per regime) -----------------------

  /// Builds (the interest universe).
  const BUILDS: Set[str]
  /// Derivations.
  const DRVS: Set[str]
  /// Output names per derivation.
  const OUTPUTS: Set[str]
  /// Store replicas (materialization executors).
  const REPLICAS: Set[str]
  /// Builder-pod identities (build-kind pulls).
  const BUILDERS: Set[str]
  /// The materialization budget (max_materialization_attempts).
  const MAT_BUDGET: int
  /// Ceiling on exec ids minted (every claim/dispatch consumes one).
  const MAX_EXECS: int
  /// Ceiling on lease generations (failovers).
  const MAX_GEN: int
  /// Ceiling on upstream-availability flips.
  const MAX_UPSTREAM_FLIPS: int
  /// Ceiling on legacy (pre-relation-era) interest arrivals (delta 2b).
  const MAX_LEGACY: int
  /// Regime switches.
  const ENABLE_FAILOVER: bool
  const ENABLE_GC: bool
  const ENABLE_CRASH: bool
  const ENABLE_STALE_TENURE: bool
  /// A1 fenced-write-discipline (bughunt wave): expose the durable
  /// resolve's FAULT arms (fenced/errored) as separate steps so the
  /// view-settlement gate is checkable. Default false — every
  /// pre-existing regime is state-space-identical.
  const ENABLE_RESOLVE_FAULTS: bool
  /// B1 bounded-await-transport (bughunt wave, merged_bug_189 / owner
  /// Q3): expose the SIGTERM-aborted walk's charge-free close
  /// (MaterializationOutcome::Aborted -> release_claim) as a step.
  /// Default false — every pre-existing regime is
  /// state-space-identical.
  const ENABLE_WORKER_ABORTED: bool
  /// Slot-10 walk-fold plane (bug_299/295/133/115): the interested
  /// tenants of one store-walk fold evaluation (two suffice for the
  /// complementary-coverage matrix), and the step gate. Default
  /// false — every pre-existing regime is state-space-identical (the
  /// plane vars are constant after init when disabled).
  const TENANTS: Set[str]
  const ENABLE_WALK_FOLD: bool

  // --- 3. pure defs ---------------------------------------------------------

  pure val freshJob: Job = {
    state: JPending, origin: OProbe, parked: false, resolutionExec: 0,
    tenant: "", matInfraCount: 0, matUnobtainableCount: 0,
  }

  pure def jobUnresolved(j: JobOpt): bool = match j {
    | NoJob => false
    | SomeJob(job) => job.state == JPending or job.state == JClaimed
  }

  pure def jobResolved(j: JobOpt): bool = match j {
    | NoJob => false
    | SomeJob(job) => or {
        job.state == JResolvedSuccess,
        job.state == JResolvedFromSource,
        job.state == JResolvedUnobtainable,
        job.state == JObsolete,
        job.state == JCancelled,
      }
  }

  /// The faithful view projection of the durable state (delta 5): what
  /// the T-4.3 recovery rebuild produces and what every as-built view
  /// write maintains. unresolvedJobAlwaysArmed asserts view == this.
  pure def projView(j: JobOpt, a: AttemptOpt): ViewState = match j {
    | NoJob => VNone
    | SomeJob(job) =>
      if (job.state == JClaimed)
        match a {
          | OpenAttempt(at) => VClaimed(at.instance)
          | NoAttempt => VPending(job.parked)
        }
      else if (job.state == JPending) VPending(job.parked)
      else VNone
  }

  // (The as-built A11 mustSubstitute judgment was deleted here at
  // T-D7.2 with the kernel arm it modeled — see delta 6 in the header:
  // the JobView::None requirement subsumes it post-D-prime.)

  // --- 4. state variables ----------------------------------------------------

  /// One job record per derivation (the partial-unique index carried
  /// structurally as a single slot; `dupJob` is the widened-slot ghost
  /// — see atMostOneUnresolvedJobPerDrv).
  var jobs: str -> JobOpt
  /// GHOST (the C-prime slot widening): a second unresolved job row
  /// was created for the drv. Unreachable as built (the creation guard
  /// is the partial-unique index). R10: computed live by the
  /// dedupSlotApply seat; the dedup-removed calibration perturbs the
  /// decision only.
  var dupJob: str -> bool
  /// The in-memory job view (delta 5) — the kernel admission's input.
  var view: str -> ViewState

  /// The durable per-(build, derivation) wanted relation (§6). Empty
  /// set = no relation row.
  var wanted: (str, str) -> Set[str]
  /// GHOST (F5 cross-build isolation): the last wanted set THIS build
  /// wrote for the pair (its own write history). As built it always
  /// equals `wanted` (the PK makes cross-build overwrites structurally
  /// impossible); the F5 overwrite override diverges them.
  var lastWritten: (str, str) -> Set[str]
  /// Delta 2b: build b holds in-memory interest in d WITHOUT wanted
  /// rows (the pre-relation-era build, the DQ-2 residual population).
  /// Cleared by the build's own merge
  /// (recordWantedRelation) or by a creation-site backfill.
  var legacyInterest: (str, str) -> bool
  /// Delta 2b ceiling: legacy arrivals left.
  var legacyBudget: int

  /// The open attempt per derivation (at most one; the §2.2 arbiter).
  var attempts: str -> AttemptOpt

  /// Node status per derivation.
  var nodeStatus: str -> NodeStatus

  /// Delta 1 (post-D-prime form): the pruned-origin fact per
  /// derivation — the model image of "an unresolved-or-parked
  /// pruned-origin job exists" (production:
  /// materialization_jobs.origin = 'pruned'; the 063 mark column is
  /// dropped, migration 080). Set by pruned-origin creation and the
  /// PD-D1 dedup upgrade; ended by every resolution arm — the row's
  /// unresolved state is the fact's only carrier.
  var topdownPruned: str -> bool

  /// Store presence per (derivation, output).
  var present: (str, str) -> bool
  /// Pin state per (derivation, output) (§5).
  var pins: (str, str) -> PinKind
  /// GHOST (B2-strong successor): the path was ingested by a
  /// materialization execution. Monotone-true; gives
  /// pinCoversIngestUntilAllInterestTerminal an antecedent the
  /// no-pin-at-ingest override cannot erase.
  var ingested: (str, str) -> bool

  /// The kind-partitioned ledger suffix, bounded counters (SC-1).
  var matInfraLedger: str -> int
  var matUnobtainableLedger: str -> int
  var buildCrashLedger: str -> int
  /// GHOST (PP-4 ii): establishment events observed per drv; the
  /// charge-once invariant compares it to the infra ledger.
  var establishCount: str -> int
  /// GHOST (PP-4 i): a BUILD-kind crash establishment happened for d —
  /// the legitimate writer of buildCrashLedger.
  var buildCrashEstablished: str -> bool
  /// GHOST (F3 i): every materialization_unobtainable charge for d was
  /// evidence-backed (written by a report whose missing paths were
  /// verified absent upstream). Constant-true as built (the report
  /// precondition); the infra-as-unobtainable override breaks it, which
  /// un-justifies the spent-one-shot fail-fast conjunct.
  var unobChargeBacked: str -> bool

  /// The lease generation (fencing token).
  var leaseGen: int
  /// GHOST: a stale-generation write was attempted and discarded (the
  /// stale-tenure regime's probe).
  var staleWriteDiscarded: bool
  /// GHOST (F11): a stale-generation job write APPLIED. Unreachable as
  /// built (the fence discards). R10: computed live by the
  /// staleWriteApply seat; the unfenced-resolve calibration perturbs
  /// the fence decision only.
  var staleJobWriteApplied: bool
  /// GHOST (A1, merged_bug_276): a materialization-infra charge landed
  /// on a drv whose job was ALREADY cancelled (or gone) — the
  /// leaked-attempt establishment conversion. Unreachable as built
  /// post-fix (the single-tx cancel closes the attempt with the row);
  /// the mat-276-dag-absent-cancel override latches it.
  var chargedAfterCancel: bool
  /// Delta 2a: the executor replica's scheduler channel is pinned to a
  /// deposed/standby replica (answers UNAVAILABLE). Set for every
  /// replica by failover; cleared per replica by redialChannel.
  var channelStale: str -> bool
  /// GHOST (delta 2a witness): a failover happened / a claim was
  /// minted after one — the redial liveness probe.
  var failedOver: bool
  var claimAfterFailover: bool
  /// GHOST (delta 5 / A10): a failover dropped durable job rows.
  /// Constant-false as built (jobs are PG rows); the view-drop
  /// override targets the VIEW instead. R10: computed live by the
  /// failoverRowsApply seat; the mat-failover-drops-rows calibration
  /// perturbs the durability decision and is failoverPreservesJobs'
  /// falsifier.
  var failoverDroppedJobs: bool

  /// Build liveness (the interest join's left side).
  var buildLive: str -> bool

  /// Upstream availability per (derivation, output).
  var upstreamAvail: (str, str) -> bool
  var upstreamFlipsLeft: int

  /// The durable declared-relation classification per derivation.
  var durableRel: str -> DurableRelation

  /// Exec-id mint counter (bounded).
  var execsMinted: int

  /// Per-derivation count of fail-fast verdicts delivered.
  var failFastCount: str -> int

  // --- 4b. at-decision-time ghost latches (the three re-encodes) -----------
  // Each is monotone-down (once false, stays false) and constant-true
  // under the as-built actions: the latch conjoins the justification
  // predicate evaluated AT THE DECISION SITE, in the same atomic
  // action — the T-5.3 known-violating current-state reads are gone.

  /// Every fail-fast was the four-conjunct corner (delta 1): marked,
  /// Broken, live-wanted missing (structural at the site) AND
  /// confirmed-missing-or-(spent-one-shot-with-evidence-backed-charges).
  var ffJustifiedAll: bool
  /// Every from-source resolution was a justified disposition (the
  /// 6-row B2 table): arm 1/2 (Vouched/Pending), the unmarked arm-3
  /// disposition, or the park re-evaluation (Vouched/Pending).
  var fsJustifiedAll: bool
  /// Every from-source ROUTING consumed a report whose live-missing
  /// paths were verified absent-and-unavailable at decision time (the
  /// C2 successor).
  var fsReportSoundAll: bool
  /// Every success resolution's coverage held at decision time (the
  /// CE-17 class; survives benign post-resolution GC).
  var covAtResolveAll: bool
  /// Every consumption coverage decision ranged over the LIVE-only
  /// wanted union (the C5/B4 successor). R10: computed live by the
  /// consumeSettleApply seat from the caller's decision set.
  var covLiveOnlyAll: bool
  /// Every job creation left the job tenant-resolvable: >=1 live build
  /// with wanted rows at creation commit (delta 2b — the B4 backfill).
  var creationResolvableAll: bool

  /// GHOST (bughunt wave, A2/318): the node status AT CLAIM TIME — the
  /// model image of `reset_after_attempt`'s deps_completed input (a
  /// Queued-origin claim is the dep-racing window; a release must not
  /// upgrade it). Written only by claimJob; read only by the release
  /// latch below.
  var claimOrigin: str -> NodeStatus
  /// GHOST LATCH (bughunt wave, A2/318): every release edge that set
  /// the node Ready did so on a Ready-origin claim (deps were produced
  /// at claim time). Constant-true under the as-built edges — every
  /// main-model release restores NQueued (the conservative arm of
  /// validate_transition_for_release); the mat-318 calibration's
  /// forced-Ready requeue reaches the violation. R10: computed live
  /// by the releaseStatusApply seat from the caller's status
  /// decision.
  var requeueReadyJustifiedAll: bool
  /// CARRIER IMAGE (bughunt wave, A3/257+055): the stale-reset job
  /// carries the realized output paths captured AT CREATION
  /// (take_realized_paths -> #[must_use] RealizedPathCarrier ->
  /// create_materialization_job — the bare clear is unwritable in
  /// code). Set by createJob's OStaleReset arm, never cleared while
  /// the job exists; completion stamps what the carrier holds, so the
  /// success-resolved stale-reset job must still be carried
  /// (completionRecordsRealizedPaths). The
  /// mat-chain-reset-drops-carrier calibration re-introduces the
  /// pre-fix drop.
  var carried: str -> bool
  /// GHOST LATCH (bughunt wave, A3/067+020): every budget verdict
  /// read the PER-JOB window — the park predicate applied exactly at
  /// newCount >= MAT_BUDGET on BOTH charge channels (party-blind, the
  /// Q5 reversal), and the unobtainable one-shot consulted the job's
  /// OWN count (the 085 reset-row window), never the drv-level
  /// history. Constant-true under the as-built actions; the
  /// mat-establish-never-parks and mat-unscoped-count calibrations
  /// reach the violations.
  var budgetSoundAll: bool
  /// GHOST LATCH (bughunt wave, B1/merged_bug_189 + owner Q3): a
  /// SIGTERM-aborted materialization walk closes CHARGE-FREE — the
  /// Aborted intake routes through release_claim and writes no
  /// materialization_infra row, no budget fold event, nothing in the
  /// 085 window. Constant-true under the as-built workerAborted; the
  /// mat-189-abort-charges calibration re-introduces the pre-fix
  /// class-blind flatten (Err -> infra_failure) and reaches the
  /// violation. R10: computed live by the abortSettleApply seat from
  /// the caller's charge decision.
  var abortChargeFree: bool

  /// A4: the view-only deferral (RetryLater) — pull admission reads it
  /// (claim blocked while armed), the park decision, the park
  /// re-evaluation and the stalled gauge NEVER do (a 429 wave must not
  /// walk a healthy job toward the from-source settlement).
  var deferUntil: str -> bool
  /// A4 latch: every transient (RetryLater) close left both
  /// materialization ledgers, the crash ledgers and the park flag
  /// untouched (charge-free by construction).
  var transientChargeFreeAll: bool
  /// A4 latch (merged_bug_193's cell split): no completed resolution
  /// while a confirmed reference hole was on the table.
  var closureCompleteAll: bool
  /// A4 latch (merged_bug_194's store leg): every covered Success
  /// consumption carried a non-empty live wanted set or actual store
  /// evidence — never a vacuous all-empty completion.
  var noVacuousCoverageAll: bool
  /// A4 ghost: the union of every wanted contribution each build ever
  /// recorded for d — the durable row must never narrow below it (the
  /// union upsert, fork-11 commit 1's surface).
  var everContrib: (str, str) -> Set[str]
  /// A4 ghost (latch-up): the park re-evaluation released a
  /// CHILDLESS-LEAF park at least once — the noLeafParkReevalResolve
  /// witness reads this (a state-shape witness would be polluted by
  /// post-resolution evidence drift).
  var leafReevalReleased: bool

  // --- 4b. slot-10 walk-fold plane (bug_299/295/133/115) ----------------------
  //
  // The store executor's per-(tenant, path) fold semantics. Two-step
  // ORACLE shape (R10 seat discipline + quint-policy P4: a latch's
  // law must read LIVE STATE, never caller params): `walkObserve`
  // arms one evaluation's raw cells into state; the seat
  // (`walkFoldApply`) recomputes each law from the ARMED CELLS
  // against the caller's four bool decisions. The live `walkFold`
  // derives decisions lawfully from the same state; the four mat-*
  // walk calibrations derive ONE decision each the pre-fix way
  // through the SAME seat (P5: perturb a decision, never a latch).

  /// One fold evaluation's raw cells: which tenants' attempt legs
  /// HIT, which paths have complete local rows, which (tenant, path)
  /// cells pass the sig-visibility gate, which are confirmed-miss,
  /// and which tenants produced rate-limited vs charging evidence.
  type WalkCells = {
    served: Set[str],
    localPresent: Set[str],
    localVisible: Set[(str, str)],
    missing: Set[(str, str)],
    rateLimited: Set[str],
    charging: Set[str],
  }

  pure val emptyCells: WalkCells = {
    served: Set(), localPresent: Set(), localVisible: Set(),
    missing: Set(), rateLimited: Set(), charging: Set(),
  }

  /// The armed evaluation's cells (the oracle the seat's laws read).
  var walkCells: WalkCells
  /// An evaluation is armed (observe ran; the fold has not consumed it).
  var cellsArmed: bool
  /// Every fold's ConfirmedMissing decision matched the per-path
  /// quantifier (exists path, forall tenant, missing) over the RAW
  /// armed cells (bug_299 — the whole-set per-tenant projection is
  /// the violation shape).
  var reprobeCongruentAll: bool
  /// Every all-rate-limited, nothing-charging, nothing-served fold
  /// closed UNCHARGED (bug_295 — the probe-leg park burner).
  var probeUnchargedOn429All: bool
  /// Every fold with a serving tenant served (merged_bug_133 — the
  /// in-loop return starving later tenants is the violation shape).
  var anyTenantHitServesAll: bool
  /// Every local-row serve had a gate-visible (tenant, path) cell
  /// (bug_115 — raw physical presence is the violation shape).
  var localServeVisibleAll: bool
  /// Reachability marker (hazard-www vacuity guard): at least one
  /// fold was decided under the walk regime; the witness battery
  /// proves this flips, so the four latches are never vacuous there.
  var walkFoldSeen: bool

  // --- 4b. bughunt-3 S3 stamp sub-plane (bug_139 / signed Q2) ---------------
  //
  // The path_tenants ownership-stamp law: stamped ⊆ verified-per-
  // tenant, derived FROM the producer's evidence witness — the model
  // image of rio-scheduler/src/db/live_pins.rs `StampProvenance::
  // lawful_pairs` (THE one derivation body; signed Q2 2026-06-07).
  // Same two-step oracle shape as the walk sub-plane: `stampObserve`
  // arms one producer's evidence; the seat (`stampApply`) recomputes
  // the law from the ARMED evidence against the caller's decided
  // pair set. The live `stampFold` derives the pairs lawfully; the
  // mat-139 calibration derives them the PRE-FIX way (∃-gate behind a
  // ∀-stamp: the attributed × outputs cartesian) through the SAME
  // seat (P5).

  /// The producer's evidence class — the four `StampProvenance`
  /// arms.
  type StampProv = PWalkVerified | PBuiltLocally | PAllTenantProbe | PProbedBy

  /// One stamp event's evidence: the witness class, the wire-carried
  /// per-(tenant, output) verified pairs (WalkVerified only), the
  /// probing tenant (ProbedBy only), the attributed tenants, and the
  /// derivation's output paths.
  type StampEvidence = {
    provenance: StampProv,
    verified: Set[(str, str)],
    prober: str,
    attributed: Set[str],
    outputs: Set[str],
  }

  pure val emptyStampEv: StampEvidence = {
    provenance: PWalkVerified, verified: Set(),
    prober: "tA", attributed: Set(), outputs: Set(),
  }

  /// The armed stamp evidence (the oracle the seat's law reads).
  var stampEv: StampEvidence
  /// A stamp event is armed (observe ran; the fold has not consumed it).
  var stampArmed: bool
  /// Every stamp decision's pair set was within the lawful pairs
  /// derived from the armed evidence (bug_139 — the ∃-gate/∀-stamp
  /// cartesian is the violation shape).
  var stampLawAll: bool
  /// Reachability marker (hazard-www vacuity guard) for the stamp
  /// sub-plane.
  var stampFoldSeen: bool

  // --- 5. derived state -------------------------------------------------------

  /// In-memory interest (the DAG's interested_builds x liveness):
  /// relation-backed OR legacy (delta 2b).
  def memInterest(d: str): Set[str] =
    BUILDS.filter(b => buildLive.get(b) and
      (wanted.get((b, d)) != Set() or legacyInterest.get((b, d))))

  /// Derived relation-backed interest (finding AS-1): the live builds
  /// holding a non-empty wanted relation for d.
  def interest(d: str): Set[str] =
    BUILDS.filter(b => buildLive.get(b) and wanted.get((b, d)) != Set())

  /// The live effective wanted set for d (the §6 join the executor and
  /// the consumption read): the union of live builds' RELATION rows.
  /// Legacy interest contributes NOTHING here — that gap is exactly
  /// the delta-2b bug class.
  def liveWanted(d: str): Set[str] =
    OUTPUTS.filter(o => BUILDS.exists(b => and {
      buildLive.get(b),
      wanted.get((b, d)).contains(o),
    }))

  /// The in-memory live wanted union the CLASSIFIER reads (legacy
  /// builds want their node-level union — modeled as all OUTPUTS).
  def memLiveWanted(d: str): Set[str] =
    if (BUILDS.exists(b => buildLive.get(b) and legacyInterest.get((b, d))))
      OUTPUTS
    else liveWanted(d)

  /// Every live-wanted output of d is present in the store.
  def liveWantedCovered(d: str): bool =
    liveWanted(d).forall(o => present.get((d, o)))

  /// d has an unresolved materialization job (PG truth).
  def hasUnresolvedJob(d: str): bool = jobUnresolved(jobs.get(d))

  /// The §2.5 stalled population (delta 3): parked jobs whose evidence
  /// is Broken — exactly what the rio_scheduler_materialization_stalled
  /// gauge counts (Vouched/Pending parks are re-evaluation transients).
  def stalledJobs: Set[str] =
    DRVS.filter(d => and {
      match jobs.get(d) {
        | NoJob => false
        | SomeJob(j) => j.state == JPending and j.parked
      },
      // A4 re-derivation: the gauge counts HOLED parks only — a
      // childless leaf's park is a re-evaluation transient exactly
      // like Vouched/Pending (parkNeverOutlivesFromSourceViability).
      durableRel.get(d) == RHoled,
    })

  /// A4: d is parked (job pending + parked flag).
  def parkedAt(d: str): bool =
    match jobs.get(d) {
      | NoJob => false
      | SomeJob(j) => j.state == JPending and j.parked
    }

  /// A4: from-source viability of the durable evidence — Vouched and
  /// Pending always; a childless leaf only while UNMARKED (the arm-3fs
  /// release row); Holed never.
  def fromSourceViable(d: str): bool = or {
    durableRel.get(d) == RVouched,
    durableRel.get(d) == RPending,
    durableRel.get(d) == RChildlessLeaf and not(topdownPruned.get(d)),
  }

  // --- 6. properties (the §9.1 successor table, C-prime re-encodes) ---------

  /// A1/A11 (+ the F8/F13 anchor): a node never runs a from-source
  /// build attempt while its materialization job is unresolved. Reads
  /// PG truth (jobs), so a view-divergence override (F8: pull ignores
  /// the job; F10: dropped view) is caught here.
  val noFromSourceWhileJobUnresolved = DRVS.forall(d =>
    hasUnresolvedJob(d) implies
      match attempts.get(d) {
        | NoAttempt => true
        | OpenAttempt(a) => a.kind != KBuild
      }
  )

  /// A3: resolved_success nodes are Completed (resolution and node
  /// completion ride the same consumption transaction).
  val jobResolutionSound = DRVS.forall(d =>
    match jobs.get(d) {
      | NoJob => true
      | SomeJob(j) => j.state == JResolvedSuccess implies
          nodeStatus.get(d) == NCompleted
    }
  )

  /// A8/C3 (delta-1 re-encode): every from-source disposition and
  /// every fail-fast was justified AT DECISION TIME per the as-built
  /// 6-row routing table — from-source on durable Vouched/Pending OR
  /// the unmarked re-probe dispositions OR the park re-evaluation;
  /// FailFast only on the four-conjunct corner.
  val routingRequiresDurableVouchOrFailFast = and {
    fsJustifiedAll,
    ffJustifiedAll,
  }

  /// L1/L2/settlement (delta-5 re-encode): the in-memory view is a
  /// faithful projection of the durable job + attempt state — which IS
  /// the armed-action totality: pending-unparked projects claimable,
  /// parked projects NotYetReady-until-expiry, claimed projects
  /// re-delivery to the holder, resolved projects VNone (so builder
  /// pulls and new claims see the truth). The F10 view-drop and B5a
  /// refeed-overwrite calibrations falsify exactly this.
  val unresolvedJobAlwaysArmed = DRVS.forall(d =>
    view.get(d) == projView(jobs.get(d), attempts.get(d))
  )

  /// C3 (re-encode): a node is Failed only through the fail-fast door,
  /// and every fail-fast was justified at decision time.
  val noWrongfulTerminalFailure = and {
    DRVS.forall(d =>
      nodeStatus.get(d) == NFailed implies failFastCount.get(d) > 0),
    ffJustifiedAll,
  }

  /// C2/B3 (re-encode): every from-source ROUTING consumed evidence-
  /// backed reports (live-missing paths verified absent-and-unavailable
  /// at the decision).
  val noWrongfulFromSourceRouting = fsReportSoundAll

  /// B1/B4/A12 (the CE-17 class; re-encode): success consumption's
  /// coverage held at decision time.
  val successConsumptionCoversLiveWanted = covAtResolveAll

  /// C5/B4 (re-encode): consumption decisions ranged over the LIVE-only
  /// wanted union (a dead build's rows drive nothing).
  val interestUnionLiveOnly = covLiveOnlyAll

  /// B2-strong (PD-10/§5.3): an ingested path stays present while its
  /// job is unresolved or any live in-memory interest remains — the
  /// pin-at-ingest guarantee, releasable only at
  /// job-resolved-and-all-interest-terminal.
  val pinCoversIngestUntilAllInterestTerminal = DRVS.forall(d =>
    OUTPUTS.forall(o =>
      (ingested.get((d, o)) and (hasUnresolvedJob(d) or memInterest(d) != Set())) implies
        present.get((d, o))
    )
  )

  /// 233 (bughunt wave, D1/migration 093): every materialization pin
  /// is attributable to its job row — production: ClaimedJob.job_id is
  /// a parsed Uuid (the store client refuses unattributable claims)
  /// and the 093 CHECK (pin_kind <> 'materialization' OR job_id IS NOT
  /// NULL) makes the NULL-job pin unrepresentable, so the §5.3 release
  /// rule can always resolve a pin's job — no immortal pins.
  /// Falsifiability pair: quint-materialization-calib-233-unattributed-pin
  /// (an ingest that pins without a job row).
  val materializationPinHasJob = DRVS.forall(d =>
    OUTPUTS.forall(o =>
      (pins.get((d, o)) == PinMaterialization) implies (jobs.get(d) != NoJob)
    )
  )

  /// A10 (delta-5): failover never drops durable job rows. The frame
  /// keeps it constant-false as built; a row-dropping override latches.
  val failoverPreservesJobs = not(failoverDroppedJobs)

  /// A17/A18 (F11): no stale-generation job write ever applies — the
  /// claims-floor fence discards them (staleWriteDiscarded is the
  /// non-vacuity ghost; this latch is the violation).
  val fencedJobWritesOnly = not(staleJobWriteApplied)

  // r[A1 view-settlement — sched.materialize.view-settlement]
  /// The in-memory view mirrors the durable unresolved relation under
  /// a stable leader: an entry exists iff an unresolved job row does.
  /// Scope: stable-leader (failover view reconstruction is
  /// failoverPreservesJobs' subject; the resolve-faults regime runs
  /// failover-disabled so this binds everywhere there). The
  /// mat-133-discarded-outcome override (a fenced/errored resolve
  /// that still discards the view entry) is its falsifiability pair.
  val viewMatchesDurableUnresolved = DRVS.forall(d => or {
    failedOver,
    (view.get(d) != VNone) == hasUnresolvedJob(d),
  })

  /// A cancelled job's history is FROZEN: no materialization-infra
  /// charge ever lands after (or without) an unresolved job — the
  /// leaked-attempt establishment conversion is unreachable
  /// (merged_bug_276). The mat-276-dag-absent-cancel override (the
  /// pre-fix split cancel) is its falsifiability pair.
  val chargeFreeCancellation = not(chargedAfterCancel)

  /// bug_299 (slot 10): every fold's ConfirmedMissing matched the
  /// per-path quantifier over raw (tenant, path) cells.
  val reprobeCongruentWithCompletability = reprobeCongruentAll
  /// bug_295 (slot 10): a rate-limit-only fold never drew budget.
  val rateLimitedProbeDrawsNoBudget = probeUnchargedOn429All
  /// merged_bug_133 (slot 10): a serving tenant is never starved by
  /// the fold's order of consultation.
  val hitUnderAnyTenantOrderSucceeds = anyTenantHitServesAll
  /// bug_115 (slot 10): every local-row serve was visibility-gated.
  val verifiedPathsVisibleToInterest = localServeVisibleAll
  /// bug_139 (bughunt-3 S3, signed Q2): every path_tenants ownership
  /// stamp stayed within the pairs its evidence witness lawfully
  /// derives — the quantifier travels WITH the evidence.
  val stampedOnlyVerifiedTenants = stampLawAll

  /// A2 kind-partition (bughunt wave, merged_bug_146 / the 307 wedge
  /// mirror): a Claimed job view always has the open MATERIALIZATION
  /// attempt that drives it, held by the claimed holder. projView is
  /// deliberately kind-blind (the production join was); this invariant
  /// adds the kind conjunct the AttemptRef witness enforces at intake —
  /// a kind-blind controller-verdict close (closing the mat attempt
  /// while the view stays Claimed) breaks it. Falsifiability pair:
  /// mat-146-controller-cross-kind-close. SINGLE OWNER: A2 lands this
  /// definition; A3 preserves it through the contract re-derivation and
  /// extends only its calibration set (§4.F13).
  val claimedImpliesOpenAttempt = DRVS.forall(d =>
    match view.get(d) {
      | VClaimed(h) => match attempts.get(d) {
          | OpenAttempt(a) => a.kind == KMaterialization and a.instance == h
          | NoAttempt => false
        }
      | _ => true
    }
  )

  /// A2 kind-partition (bughunt wave, bug_266): every Claimed-view
  /// holder is a store replica — the recovery view rebuild derives
  /// holders only from materialization-kind attempts (the kind-scoped
  /// claimed_by join). The kind-blind rebuild could attach a BUILD
  /// assignment's builder as the holder of a pending job's view.
  /// Falsifiability pair: mat-266-kindblind-recovery-holder.
  val claimedByOnlyMatHolders = DRVS.forall(d =>
    match view.get(d) {
      | VClaimed(h) => REPLICAS.contains(h)
      | _ => true
    }
  )

  /// A2 kinded release (bughunt wave, merged_bug_318): no release edge
  /// upgrades a Queued-origin (dep-racing) claim to Ready — the
  /// validate_transition_for_release / reset_after_attempt(kind,
  /// deps_completed) contract. Latch-backed: the as-built edges all
  /// restore NQueued; the forced-Ready requeue (the pre-fix
  /// Assigned->Ready release) is the calibration arm. Falsifiability
  /// pair: mat-318-requeue-forces-ready.
  val readyImpliesDepsProducedOnRequeue = requeueReadyJustifiedAll

  /// A3/067+020 (bughunt wave, the contract re-derivation): every
  /// budget verdict read the PER-JOB window — the park predicate
  /// applied exactly at newCount >= MAT_BUDGET on BOTH charge
  /// channels (party-blind, the owner-signed Q5 reversal of
  /// counter-signed residual (a)), and the unobtainable one-shot
  /// consulted the job's own count (the 085 reset-row window). The
  /// mat-establish-never-parks and mat-unscoped-count calibrations
  /// preserve the removed as-built encodings and reach the
  /// violations.
  val budgetSoundness = budgetSoundAll

  /// B1 (merged_bug_189 / owner Q3): the SIGTERM-aborted walk's close
  /// is charge-free — the abort is a routine rollout artifact, not
  /// evidence about the derivation or the replica. The
  /// mat-189-abort-charges calibration (the pre-fix Err->infra flatten)
  /// must violate this.
  val abortNeverCharges = abortChargeFree

  /// A3/015+307 (bughunt wave): a pending unparked job with no open
  /// attempt never strands its node Running — release_claim is
  /// atomic (view rearm + reassign in ONE step; consumeUnobtainable
  /// at qnt is the code twin). The
  /// mat-split-rearm-without-reassign calibration splits the step
  /// and reaches the violation.
  val pendingUnclaimedImpliesClaimableNode = DRVS.forall(d =>
    match jobs.get(d) {
      | NoJob => true
      | SomeJob(j) =>
        (j.state == JPending and not(j.parked) and attempts.get(d) == NoAttempt)
          implies nodeStatus.get(d) != NRunning
    }
  )

  /// A3/257 (bughunt wave): an unresolved stale-reset job keeps its
  /// carrier — capture-at-creation (#[must_use] RealizedPathCarrier
  /// makes the bare clear unwritable in code). The
  /// mat-chain-reset-drops-carrier calibration re-introduces the
  /// pre-fix drop.
  val carrierConservation = DRVS.forall(d =>
    match jobs.get(d) {
      | NoJob => true
      | SomeJob(j) =>
        (j.origin == OStaleReset and (j.state == JPending or j.state == JClaimed))
          implies carried.get(d)
    }
  )

  /// A3/055 (bughunt wave): a success-resolved stale-reset job
  /// recorded the realized paths — the completion chokepoint
  /// (complete_materialization_for_live_interest) cannot skip the
  /// stamp; a [""]-stamped carried completion is the violation image
  /// (the calibration's resolution-discard arm).
  val completionRecordsRealizedPaths = DRVS.forall(d =>
    match jobs.get(d) {
      | NoJob => true
      | SomeJob(j) =>
        (j.origin == OStaleReset and j.state == JResolvedSuccess)
          implies carried.get(d)
    }
  )

  /// §3.6: the attempt kind matches the worker class.
  val kindMatchesWorker = DRVS.forall(d =>
    match attempts.get(d) {
      | NoAttempt => true
      | OpenAttempt(a) => and {
          a.kind == KMaterialization implies REPLICAS.contains(a.instance),
          a.kind == KBuild implies BUILDERS.contains(a.instance),
        }
    }
  )

  /// §2.5/§3.5: materialization charges never fail a node except
  /// through the (justified) fail-fast door — budget exhaustion parks,
  /// the establishment re-arms; neither fails. The park-exhaustion-
  /// fail-fasts override falsifies this.
  val materializationNeverPoisons = DRVS.forall(d =>
    nodeStatus.get(d) == NFailed implies failFastCount.get(d) > 0
  )

  /// §2.5/§3.5 (PP-4 i): the build-kind crash ledger moves only
  /// through the build-kind establishment channel.
  val materializationInvisibleToBuildBudgets = DRVS.forall(d =>
    buildCrashLedger.get(d) > 0 implies buildCrashEstablished.get(d)
  )

  /// The dedup (partial-unique index), slot-widened per the C-prime
  /// handoff: the second-slot ghost is never latched.
  val atMostOneUnresolvedJobPerDrv = DRVS.forall(d => not(dupJob.get(d)))

  /// The §2.2 arbiter: at most one open attempt per drv (structural —
  /// the slot) carrying a worker-class identity; the claim guard
  /// (attempts == NoAttempt + the view holder) is the one-winner arm.
  val atMostOneClaimWinner = DRVS.forall(d =>
    match attempts.get(d) {
      | NoAttempt => true
      | OpenAttempt(a) => a.kind == KMaterialization implies REPLICAS.contains(a.instance)
    }
  )

  /// C1 + C1-strict: at most one fail-fast per drv lifetime (the
  /// drv-level one-shot ledger — see the priced-omission note on
  /// resubmits), every one justified.
  val wrongfulFailFastBoundedPerJob = DRVS.forall(d => and {
    failFastCount.get(d) <= 1,
    failFastCount.get(d) == 1 implies ffJustifiedAll,
  })

  /// Delta 2b (B4): every creation left the job tenant-resolvable.
  val creationLeavesTenantResolvable = creationResolvableAll

  /// PP-4 (ii): every establishment charged the materialization budget
  /// (establishments never outrun the infra ledger).
  val materializationCrashChargedOnce = DRVS.forall(d =>
    establishCount.get(d) <= matInfraLedger.get(d)
  )

  /// A4 (merged_bug_193): the latch — no completed resolution over a
  /// confirmed reference hole. Falsifiability: mat-a4-refs-folded.
  val closureCompleteResolution = closureCompleteAll

  /// A4 (merged_bug_194 store leg): the latch — no vacuous Success.
  /// Falsifiability: mat-a4-vacuous-success.
  val noVacuousCoverage = noVacuousCoverageAll

  /// A4 (merged_bug_178/195 transient axis): the latch — RetryLater
  /// closes are charge-free and park-blind. Falsifiability:
  /// mat-a4-transient-as-infra.
  val transientOutcomesNeverCharge = transientChargeFreeAll

  /// A4 (fork-11 commit 1's union upsert): the durable wanted row per
  /// (build, drv) covers every contribution that build ever recorded —
  /// the upsert may only widen. Falsifiability: mat-a4-union-dropped.
  val durableUnionWidensOrEqualsLive = BUILDS.forall(b => DRVS.forall(d =>
    everContrib.get((b, d)).subseteq(wanted.get((b, d)))
  ))

  /// A4 (the RBroken split): a parked job whose evidence is
  /// from-source viable is a re-evaluation TRANSIENT — never part of
  /// the stalled population the MD-D1 gauge alerts on. The pre-fix
  /// collapse (stalled = parked ∧ Broken, leaves included) is the
  /// recorded flip in mat-a4-leaf-park-forever (with the re-derived
  /// parkReevaluate guard's enabledness as the liveness half — the
  /// noLeafParkReevalResolve witness).
  val parkNeverOutlivesFromSourceViability = DRVS.forall(d =>
    (parkedAt(d) and fromSourceViable(d)) implies not(stalledJobs.contains(d))
  )

  /// F5 (PP-5 i): a build's wanted row is exactly its own last write —
  /// no other build's write ever replaced it (the PK isolation).
  val crossBuildWantedIsolation = BUILDS.forall(b => DRVS.forall(d =>
    wanted.get((b, d)) == lastWritten.get((b, d))
  ))

  /// Ceiling tripwire (house style): every bounded counter is within
  /// its ceiling — a guard regression surfaces here, not as a silent
  /// state-space change.
  val boundsOK = and {
    execsMinted <= MAX_EXECS,
    leaseGen <= MAX_GEN,
    upstreamFlipsLeft >= 0,
    legacyBudget >= 0,
    DRVS.forall(d => and {
      matInfraLedger.get(d) <= MAT_BUDGET + 2,
      matUnobtainableLedger.get(d) <= MAT_BUDGET + 2,
      buildCrashLedger.get(d) <= MAT_BUDGET,
      failFastCount.get(d) <= 2,
    }),
  }

  // --- 6b. witnesses (non-vacuity probes; expect-violation) ------------------

  /// Slot-10 vacuity guard (hazard www): a fold gets decided under
  /// the walk regime — expect VIOLATION there; vacuously holds in
  /// regimes with ENABLE_WALK_FOLD = false.
  val noWalkFoldEvaluated = not(walkFoldSeen)
  /// Bughunt-3 S3 vacuity guard (hazard www): a stamp gets decided
  /// under the walk regime — expect VIOLATION there; vacuously holds
  /// in regimes with ENABLE_WALK_FOLD = false.
  val noStampFoldEvaluated = not(stampFoldSeen)
  // Each canReach* is encoded as no* = not(<interesting state>); the
  // check passes only when the checker VIOLATES it.

  /// The happy path is reachable.
  val noSuccessResolution = DRVS.forall(d =>
    match jobs.get(d) { | NoJob => true | SomeJob(j) => j.state != JResolvedSuccess }
  )
  /// The arm-3b fail-fast is reachable (needs a pruned-origin trace —
  /// delta 1 keeps this non-vacuous through the mark conjunct).
  val noFailFast = DRVS.forall(d => failFastCount.get(d) == 0)
  /// A job parks on budget exhaustion.
  val noPark = DRVS.forall(d =>
    match jobs.get(d) { | NoJob => true | SomeJob(j) => not(j.parked) }
  )
  /// A crashed replica's attempt is established.
  val noCrashEstablishment = DRVS.forall(d => matInfraLedger.get(d) == 0)
  /// A build-kind attempt opens (kindMatchesWorker non-vacuity).
  val noBuildAttempt = DRVS.forall(d =>
    match attempts.get(d) {
      | NoAttempt => true
      | OpenAttempt(a) => a.kind != KBuild
    }
  )
  /// Delta 6 / B1 regression guard: a MARKED node's job is claimed
  /// (the must_substitute upgrade — pre-B1 this was unreachable; if
  /// this witness stops violating, the B1 admission gap is back).
  val noMarkedClaim = DRVS.forall(d =>
    not(and {
      topdownPruned.get(d),
      match attempts.get(d) {
        | NoAttempt => false
        | OpenAttempt(a) => a.kind == KMaterialization
      },
    })
  )
  /// Delta 1: the unmarked arm-3 from-source disposition is reachable
  /// (the B2-fix row of the 6-row table).
  val noUnmarkedArm3FromSource = DRVS.forall(d =>
    not(and {
      not(topdownPruned.get(d)),
      durableRel.get(d) == RChildlessLeaf or durableRel.get(d) == RHoled,
      match jobs.get(d) {
        | NoJob => false
        | SomeJob(j) => j.state == JResolvedFromSource and j.resolutionExec > 0
      },
    })
  )
  /// Delta 4: a stale-reset job is created (reset-from-Completed).
  val noStaleResetCreation = DRVS.forall(d =>
    match jobs.get(d) {
      | NoJob => true
      | SomeJob(j) => j.origin != OStaleReset
    }
  )
  /// Delta 4: a reprobe-origin job is created (the AS-5 reset lane).
  val noReprobeCreation = DRVS.forall(d =>
    match jobs.get(d) {
      | NoJob => true
      | SomeJob(j) => j.origin != OReprobe
    }
  )
  /// Delta 2b: a legacy build's relation is backfilled at creation
  /// (legacy interest existed and the creation converted it to rows).
  val noLegacyBackfill = BUILDS.forall(b => DRVS.forall(d =>
    not(and {
      wanted.get((b, d)) == OUTPUTS,
      not(legacyInterest.get((b, d))),
      legacyBudget < MAX_LEGACY,
      hasUnresolvedJob(d),
    })
  ))
  /// Delta 2a: a claim is minted AFTER a failover staled every
  /// channel — the redial is what makes this reachable; the no-redial
  /// (pre-B3) override makes it unreachable (the dead-end).
  val noPostFailoverClaim = not(claimAfterFailover)
  /// Delta 3: a parked Vouched/Pending job is resolved by the
  /// re-evaluation arm.
  val noParkReevalResolve = DRVS.forall(d =>
    not(and {
      match jobs.get(d) {
        | NoJob => false
        | SomeJob(j) => j.state == JResolvedFromSource and j.resolutionExec == 0
      },
    })
  )
  /// Delta 3: a Holed-evidence parked job (the stalled population) is
  /// reachable — the MD-D1 alert constrains a real state.
  val noStalledBrokenPark = stalledJobs == Set()
  /// A4: a transient (RetryLater) close happens — the charge-free
  /// invariant constrains a reachable edge.
  val noTransientClose = DRVS.forall(d => not(deferUntil.get(d)))
  /// A4: a CHILDLESS-LEAF park is resolved by the re-evaluation arm
  /// (resolutionExec == 0 marks the re-eval resolution; the leaf cell
  /// distinguishes it from the Vouched/Pending transients). Pre-fix
  /// (Vouched|Pending-only guard) this is UNREACHABLE — the recorded
  /// flip in mat-a4-leaf-park-forever.
  val noLeafParkReevalResolve = not(leafReevalReleased)
  /// §5.3: the pin release fires (job resolved, all interest terminal)
  /// — observed at the released-path footprint: a still-present,
  /// now-unpinned output of a resolved-job derivation (presence only
  /// ever arises pinned, so this state is reachable only through the
  /// release).
  val noPinRelease = DRVS.forall(d => OUTPUTS.forall(o =>
    not(and {
      present.get((d, o)),
      pins.get((d, o)) == NoPin,
      jobResolved(jobs.get(d)),
    })
  ))
  /// F5 (PP-5 ii): a same-build wanted REWRITE happens (the documented
  /// intended narrowing — recorded as reachable, not a violation).
  val noWantedRewrite = BUILDS.forall(b => DRVS.forall(d =>
    not(and {
      wanted.get((b, d)) != Set(),
      wanted.get((b, d)) != OUTPUTS,
      lastWritten.get((b, d)) == wanted.get((b, d)),
      hasUnresolvedJob(d),
    })
  ))
  /// The store short-circuit completes a node by other means while its
  /// materialization job is still open (the obsoleteOnProduced
  /// pre-state — produced-by-other-means is reachable).
  val noStoreShortCircuit = DRVS.forall(d =>
    not(and {
      nodeStatus.get(d) == NCompleted,
      hasUnresolvedJob(d),
    })
  )
  /// The zero-interest cancellation closes a job.
  val noCancelOnZeroInterest = DRVS.forall(d =>
    match jobs.get(d) { | NoJob => true | SomeJob(j) => j.state != JCancelled }
  )
  /// Obsolete-on-produced fires.
  val noObsolete = DRVS.forall(d =>
    match jobs.get(d) { | NoJob => true | SomeJob(j) => j.state != JObsolete }
  )
  /// The stale-tenure discard fires (fence non-vacuity).
  val noStaleDiscard = not(staleWriteDiscarded)

  // --- 7. frame helpers --------------------------------------------------------

  /// Relation-layer vars unchanged.
  action relUnchanged = all {
    wanted' = wanted, lastWritten' = lastWritten,
    legacyInterest' = legacyInterest, legacyBudget' = legacyBudget,
  }
  /// Store-layer vars unchanged.
  action storeUnchanged = all {
    present' = present, pins' = pins, ingested' = ingested,
  }
  /// Ledger vars unchanged.
  action ledgerUnchanged = all {
    matInfraLedger' = matInfraLedger,
    matUnobtainableLedger' = matUnobtainableLedger,
    buildCrashLedger' = buildCrashLedger,
    establishCount' = establishCount,
    buildCrashEstablished' = buildCrashEstablished,
    unobChargeBacked' = unobChargeBacked,
  }
  /// Environment vars unchanged.
  action envUnchanged = all {
    buildLive' = buildLive, upstreamAvail' = upstreamAvail,
    upstreamFlipsLeft' = upstreamFlipsLeft, durableRel' = durableRel,
  }
  /// Tenure/transport vars unchanged.
  action tenureUnchanged = all {
    leaseGen' = leaseGen, staleWriteDiscarded' = staleWriteDiscarded,
    staleJobWriteApplied' = staleJobWriteApplied,
    channelStale' = channelStale, failedOver' = failedOver,
    claimAfterFailover' = claimAfterFailover,
    failoverDroppedJobs' = failoverDroppedJobs,
  }
  /// Decision-ghost latches unchanged.
  action ghostsUnchanged = all {
    failFastCount' = failFastCount,
    ffJustifiedAll' = ffJustifiedAll, fsJustifiedAll' = fsJustifiedAll,
    fsReportSoundAll' = fsReportSoundAll, covAtResolveAll' = covAtResolveAll,
    covLiveOnlyAll' = covLiveOnlyAll,
    creationResolvableAll' = creationResolvableAll,
    chargedAfterCancel' = chargedAfterCancel,
    claimOrigin' = claimOrigin,
    requeueReadyJustifiedAll' = requeueReadyJustifiedAll,
    carried' = carried,
    budgetSoundAll' = budgetSoundAll,
    abortChargeFree' = abortChargeFree,
  }
  /// A4 vars unchanged (the deferral flag, the three A4 latches and
  /// the contribution ghost).
  action a4Unchanged = all {
    deferUntil' = deferUntil,
    transientChargeFreeAll' = transientChargeFreeAll,
    closureCompleteAll' = closureCompleteAll,
    noVacuousCoverageAll' = noVacuousCoverageAll,
    everContrib' = everContrib,
    leafReevalReleased' = leafReevalReleased,
  }
  /// Slot-10 walk sub-plane vars unchanged (cells + latches): the
  /// stamp sub-plane's actions frame their sibling through this.
  action walkSubPlaneUnchanged = all {
    walkCells' = walkCells,
    cellsArmed' = cellsArmed,
    reprobeCongruentAll' = reprobeCongruentAll,
    probeUnchargedOn429All' = probeUnchargedOn429All,
    anyTenantHitServesAll' = anyTenantHitServesAll,
    localServeVisibleAll' = localServeVisibleAll,
    walkFoldSeen' = walkFoldSeen,
  }
  /// Bughunt-3 stamp sub-plane vars unchanged: the walk sub-plane's
  /// actions (and the machine branches, via `walkUnchanged`) frame
  /// their sibling through this.
  action stampSubPlaneUnchanged = all {
    stampEv' = stampEv,
    stampArmed' = stampArmed,
    stampLawAll' = stampLawAll,
    stampFoldSeen' = stampFoldSeen,
  }
  /// The WHOLE walk plane (both sub-planes) unchanged — the machine
  /// branches' frame; every deterministic run steps the machine under
  /// this.
  action walkUnchanged = all {
    walkSubPlaneUnchanged,
    stampSubPlaneUnchanged,
  }
  /// Walk latches unchanged (the observe step's frame — observing
  /// cells decides nothing; the stamp sub-plane is framed too).
  action walkLatchesUnchanged = all {
    reprobeCongruentAll' = reprobeCongruentAll,
    probeUnchargedOn429All' = probeUnchargedOn429All,
    anyTenantHitServesAll' = anyTenantHitServesAll,
    localServeVisibleAll' = localServeVisibleAll,
    walkFoldSeen' = walkFoldSeen,
    stampSubPlaneUnchanged,
  }
  /// Job-core vars unchanged.
  action jobCoreUnchanged = all {
    jobs' = jobs, dupJob' = dupJob, view' = view, attempts' = attempts,
    nodeStatus' = nodeStatus, topdownPruned' = topdownPruned,
    execsMinted' = execsMinted,
  }

  // --- 8a. oracle-seated apply sub-actions (bughunt-2 R10) -------------------
  //
  // The six declared latches (header directive) are computed HERE, in
  // shared parameterized seats the live actions and the calibration
  // overrides BOTH route through — only the caller's DECISION differs
  // (the fencedWrites 0f1ddf484 repair pattern). A regression in a
  // live action now trips its own invariant instead of leaving a
  // writerless ghost; a calibration perturbs a decision, never a
  // latch (quint-policy P5).

  /// Release-status seat (A2/318): the re-arm after a closed
  /// materialization attempt. `toReady` is the CALLER's status
  /// decision (production: reset_after_attempt's deps_completed
  /// input); the justification truth — a Ready-setting release had a
  /// Ready-origin claim — is computed here from the claimOrigin
  /// witness. Live callers: reportInfra (always false — the
  /// conservative NQueued restore) and workerAborted (origin-restore:
  /// true exactly when the claim originated Ready). The mat-318
  /// calibration passes the forced true.
  action releaseStatusApply(d: str, toReady: bool): bool = all {
    nodeStatus' = nodeStatus.put(d, if (toReady) NReady else NQueued),
    requeueReadyJustifiedAll' = and {
      requeueReadyJustifiedAll,
      toReady implies (claimOrigin.get(d) == NReady),
    },
  }

  /// Abort-settlement seat (B1/189): the SIGTERM-aborted close.
  /// `charges` is the CALLER's intake decision (production: the
  /// Aborted wire variant routes through release_claim charge-free;
  /// the pre-fix class-blind Err arm charged). The charge shape — the
  /// per-job window bump, the park verdict, the ledger row — and the
  /// abort-charge-free truth are computed here. Live caller:
  /// workerAborted (false). The mat-189 calibration passes true.
  action abortSettleApply(d: str, charges: bool): bool = all {
    val newCount = match jobs.get(d) {
      | NoJob => 0
      | SomeJob(j) => if (charges) j.matInfraCount + 1 else j.matInfraCount
    }
    val parks = and { charges, newCount >= MAT_BUDGET }
    all {
      jobs' = jobs.put(d, match jobs.get(d) {
        | NoJob => NoJob
        | SomeJob(j) => SomeJob({ ...j,
            state: JPending,
            parked: if (charges) parks else j.parked,
            matInfraCount: newCount })
      }),
      view' = view.put(d, VPending(if (charges) parks else false)),
      matInfraLedger' = if (charges)
        matInfraLedger.put(d, matInfraLedger.get(d) + 1)
      else matInfraLedger,
      budgetSoundAll' = and {
        budgetSoundAll,
        charges implies (parks == (newCount >= MAT_BUDGET)),
      },
      abortChargeFree' = and { abortChargeFree, not(charges) },
    },
  }

  /// Success-consumption settlement seat (C5/B4 + CE-17): the
  /// coverage decision's input set is the CALLER's choice —
  /// production reads the live §6 join; the pre-fix read was the
  /// dead-inclusive stored union (the mat-f4 calibration's decision).
  /// Both coverage latches and the vacuity latch are computed here
  /// from the pre-state.
  action consumeSettleApply(d: str, usedUnion: Set[str]): bool = all {
    val covered = usedUnion.forall(o => present.get((d, o)))
    val execId = match attempts.get(d) { | NoAttempt => 0 | OpenAttempt(a) => a.execId }
    all {
      jobs' = jobs.put(d, match jobs.get(d) {
        | NoJob => NoJob
        | SomeJob(j) => SomeJob({ ...j,
            state: if (covered) JResolvedSuccess else JPending,
            resolutionExec: if (covered) execId else j.resolutionExec })
      }),
      view' = if (covered) view.put(d, VNone) else view.put(d, VPending(false)),
      nodeStatus' = if (covered) nodeStatus.put(d, NCompleted) else nodeStatus.put(d, NQueued),
      attempts' = attempts.put(d, NoAttempt),
      // delta 1 clear-mirror: success resolution consumes the mark.
      topdownPruned' = if (covered) topdownPruned.put(d, false) else topdownPruned,
      // CE-17 latch: coverage held over the LIVE union at the decision
      // (identical to the decision itself when the caller passes the
      // live join; the dead-union decision diverges exactly when a
      // dead build's row covers what the live join does not).
      covAtResolveAll' = and {
        covAtResolveAll,
        covered implies liveWantedCovered(d),
      },
      // C5/B4 latch: the decision set WAS the live join.
      covLiveOnlyAll' = and {
        covLiveOnlyAll,
        usedUnion == liveWanted(d),
      },
      // A4 latch: the covered completion carried real width or real
      // evidence.
      noVacuousCoverageAll' = and {
        noVacuousCoverageAll,
        covered implies or {
          liveWanted(d) != Set(),
          OUTPUTS.exists(o => ingested.get((d, o)) or present.get((d, o))),
        },
      },
    },
  }

  /// Stale-tenure job-write seat (F11): `fenceHonored` is the
  /// CALLER's fence decision (production: the claims-floor fence
  /// covers job-table writes uniformly). Honored => the write is
  /// DISCARDED (the staleWriteDiscarded probe records the visit);
  /// dishonored => the write APPLIES and the stale-apply truth is
  /// computed here from the tenure pre-state. Live caller:
  /// staleTenureWriteDiscarded (true). The mat-f11 calibration passes
  /// false.
  action staleWriteApply(d: str, fenceHonored: bool): bool = all {
    jobs' = if (fenceHonored) jobs else jobs.put(d, match jobs.get(d) {
      | NoJob => NoJob
      | SomeJob(j) => SomeJob({ ...j, state: JResolvedFromSource, parked: false })
    }),
    view' = if (fenceHonored) view else view.put(d, VNone),
    staleWriteDiscarded' = if (fenceHonored) true else staleWriteDiscarded,
    staleJobWriteApplied' = or {
      staleJobWriteApplied,
      and { not(fenceHonored), leaseGen > 0 },
    },
  }

  /// Creation dedup-slot seat (C-prime slot widening): `dedupHonored`
  /// is the CALLER's index decision (production: the partial-unique
  /// index refuses a second unresolved row). The widened-slot truth is
  /// computed here from the pre-state. Live caller: createJob (true —
  /// and its no-unresolved-job guard keeps the oracle quiet). The
  /// mat-dedup-removed calibration passes false with the guard
  /// dropped.
  action dedupSlotApply(d: str, dedupHonored: bool): bool = all {
    dupJob' = if (and { not(dedupHonored), hasUnresolvedJob(d) })
      dupJob.put(d, true)
    else dupJob,
  }

  /// Failover row-durability seat (delta 5 / A10): `dropsRows` is the
  /// CALLER's durability decision (production: jobs are PG rows — a
  /// failover cannot drop them). The drop truth is computed here from
  /// the pre-state's pending population. Live caller: failover
  /// (false). The mat-failover-drops-rows calibration passes true —
  /// failoverPreservesJobs' first falsifier.
  action failoverRowsApply(dropsRows: bool): bool = all {
    jobs' = if (dropsRows) DRVS.mapBy(d => NoJob) else jobs,
    failoverDroppedJobs' = or {
      failoverDroppedJobs,
      and { dropsRows, DRVS.exists(d => hasUnresolvedJob(d)) },
    },
  }

  // --- 8. actions ----------------------------------------------------------------

  /// §6: a build's merge writes (or REWRITES — the as-built upsert,
  /// PP-5 ii) its wanted relation rows for d, clearing any legacy
  /// marker for the pair (the merge is what backfills a re-merging
  /// build). Both `wanted` and the per-build write history move
  /// together (crossBuildWantedIsolation's two sides).
  action recordWantedRelation(b: str, d: str, outs: Set[str]): bool = all {
    buildLive.get(b),
    outs != Set(),
    outs.subseteq(OUTPUTS),
    // A4 re-derivation (fork-11 commit 1): the upsert UNIONS — a
    // re-merge can only widen its own row (the F5 overwrite is the
    // mat-a4-union-dropped calibration). Guard: the union must move.
    not(outs.subseteq(wanted.get((b, d)))),
    val applied = wanted.get((b, d)).union(outs)
    all {
      wanted' = wanted.put((b, d), applied),
      lastWritten' = lastWritten.put((b, d), applied),
      everContrib' = everContrib.put((b, d), everContrib.get((b, d)).union(outs)),
    },
    legacyInterest' = legacyInterest.put((b, d), false),
    legacyBudget' = legacyBudget,
    deferUntil' = deferUntil, transientChargeFreeAll' = transientChargeFreeAll,
    closureCompleteAll' = closureCompleteAll,
    noVacuousCoverageAll' = noVacuousCoverageAll,
    leafReevalReleased' = leafReevalReleased,
    jobCoreUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
    tenureUnchanged, ghostsUnchanged,
  }

  /// Delta 2b: a pre-relation-era build's in-memory interest surfaces —
  /// live interest with NO wanted rows (the FP-4(b) transition-window
  /// shape). Bounded by MAX_LEGACY.
  action legacyBuildArrives(b: str, d: str): bool = all {
    legacyBudget > 0,
    buildLive.get(b),
    wanted.get((b, d)) == Set(),
    not(legacyInterest.get((b, d))),
    legacyInterest' = legacyInterest.put((b, d), true),
    legacyBudget' = legacyBudget - 1,
    wanted' = wanted, lastWritten' = lastWritten,
    jobCoreUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
    tenureUnchanged, ghostsUnchanged, a4Unchanged,
  }

  /// §2.1: a scheduler classification site creates a job for d — FIVE
  /// origins (delta 4). Per-origin status guards and effects:
  ///   - OProbe / OMerge / OPruned / OReprobe: node Queued or Ready;
  ///     OPruned additionally stamps the topdown mark (the merge-tx
  ///     stamp, delta 1); OReprobe applies the AS-5 status reset
  ///     (-> Queued, merge.rs:934).
  ///   - OStaleReset: node COMPLETED with a live-wanted output absent
  ///     (the GC'd-outputs re-merge); resets the node to Queued in the
  ///     same action (delta 4's relaxed precondition).
  /// The standalone-fenced sites (OProbe, OStaleReset) BACKFILL the
  /// wanted relation for every live legacy-interested build (delta 2b,
  /// the B4 fix); the in-tx sites require the creating tenant's rows
  /// present (the same-tx merge batch wrote them).
  /// The creation-time presence re-check (`not covered`) is the §2.1
  /// classification's local store re-check (the F1-permissiveness kept
  /// guard, data source re-pointed to the relation/mem union).
  action createJob(d: str, origin: JobOrigin, tenant: str): bool = all {
    BUILDS.contains(tenant),
    buildLive.get(tenant),
    or {
      wanted.get((tenant, d)) != Set(),
      legacyInterest.get((tenant, d)),
    },
    not(memLiveWanted(d).forall(o => present.get((d, o)))),
    not(hasUnresolvedJob(d)),
    nodeStatus.get(d) != NFailed,
    nodeStatus.get(d) != NDepFailed,
    nodeStatus.get(d) != NRunning,
    if (origin == OStaleReset)
      nodeStatus.get(d) == NCompleted
    else
      nodeStatus.get(d) == NQueued or nodeStatus.get(d) == NReady,
    // in-tx origins ride the merge tx whose batch wrote the tenant's
    // rows; the standalone sites accept legacy tenants and backfill.
    (origin == OMerge or origin == OPruned or origin == OReprobe) implies
      wanted.get((tenant, d)) != Set(),
    val standalone = origin == OProbe or origin == OStaleReset
    val backfills = BUILDS.filter(b => and {
      standalone,
      buildLive.get(b),
      legacyInterest.get((b, d)),
    })
    val wantedNew = backfills.fold(wanted, (acc, b) => acc.put((b, d), OUTPUTS))
    all {
      jobs' = jobs.put(d, SomeJob({ ...freshJob, origin: origin, tenant: tenant })),
      view' = view.put(d, VPending(false)),
      // R10: the widened-slot oracle routes through the dedup seat —
      // the as-built decision honors the partial-unique index (and
      // the no-unresolved-job guard above keeps the oracle quiet);
      // the mat-dedup-removed calibration passes dedupHonored=false
      // with the guard dropped.
      dedupSlotApply(d, true),
      topdownPruned' = if (origin == OPruned) topdownPruned.put(d, true) else topdownPruned,
      nodeStatus' = if (origin == OStaleReset or origin == OReprobe)
        nodeStatus.put(d, NQueued)
      else nodeStatus,
      wanted' = wantedNew,
      lastWritten' = backfills.fold(lastWritten, (acc, b) => acc.put((b, d), OUTPUTS)),
      legacyInterest' = backfills.fold(legacyInterest, (acc, b) => acc.put((b, d), false)),
      // delta 2b invariant latch: resolvable at creation commit —
      // some live build holds wanted rows AFTER the backfill.
      creationResolvableAll' = and {
        creationResolvableAll,
        BUILDS.exists(b => buildLive.get(b) and wantedNew.get((b, d)) != Set()),
      },
    },
    legacyBudget' = legacyBudget,
    attempts' = attempts, execsMinted' = execsMinted,
    storeUnchanged, ledgerUnchanged, envUnchanged, tenureUnchanged,
    failFastCount' = failFastCount,
    ffJustifiedAll' = ffJustifiedAll, fsJustifiedAll' = fsJustifiedAll,
    fsReportSoundAll' = fsReportSoundAll, covAtResolveAll' = covAtResolveAll,
    covLiveOnlyAll' = covLiveOnlyAll,
    chargedAfterCancel' = chargedAfterCancel,
    claimOrigin' = claimOrigin,
    requeueReadyJustifiedAll' = requeueReadyJustifiedAll,
    // A3/257 carrier capture-at-creation: the stale-reset origin
    // creates WITH the realized paths (take_realized_paths ->
    // create_materialization_job in one pass; the !deps_ok exit
    // creates too — PD-6 Queued claims). The
    // mat-chain-reset-drops-carrier calibration re-introduces the
    // pre-fix drop.
    carried' = if (origin == OStaleReset) carried.put(d, true) else carried,
    budgetSoundAll' = budgetSoundAll,
    abortChargeFree' = abortChargeFree,
    // A4: a fresh job admits immediately — any stale deferral from a
    // predecessor job is reset at creation.
    deferUntil' = deferUntil.put(d, false),
    transientChargeFreeAll' = transientChargeFreeAll,
    closureCompleteAll' = closureCompleteAll,
    noVacuousCoverageAll' = noVacuousCoverageAll,
    everContrib' = everContrib,
    leafReevalReleased' = leafReevalReleased,
  }

  /// Delta 5: the dispatch-probe dedup re-encounters an existing
  /// unresolved job (created==false). As built the view feed is
  /// entry().or_insert() — armament state (park, holder) is PRESERVED
  /// — and the live_interested backfill loop still runs (the lazy heal
  /// for legacy interest that arrived after creation). The view write
  /// is a no-op here BECAUSE the view is already faithful; the B5a
  /// override replaces this action with the pre-fix overwrite.
  action dedupRefeed(d: str): bool = all {
    hasUnresolvedJob(d),
    // fire only when the backfill half has work (economy: the pure
    // no-op self-loop adds nothing; the or_insert half is exercised
    // by the override's calibration).
    BUILDS.exists(b => buildLive.get(b) and legacyInterest.get((b, d))),
    val backfills = BUILDS.filter(b => buildLive.get(b) and legacyInterest.get((b, d)))
    all {
      wanted' = backfills.fold(wanted, (acc, b) => acc.put((b, d), OUTPUTS)),
      lastWritten' = backfills.fold(lastWritten, (acc, b) => acc.put((b, d), OUTPUTS)),
      legacyInterest' = backfills.fold(legacyInterest, (acc, b) => acc.put((b, d), false)),
    },
    legacyBudget' = legacyBudget,
    jobCoreUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
    tenureUnchanged, ghostsUnchanged, a4Unchanged,
  }

  /// PD-D1 (Wave D2.1, encoded at T-D7.2): a pruned merge's creation
  /// dedups onto d's existing unresolved job — the fenced in-place
  /// pruned-wins origin upgrade (`ON CONFLICT` on the partial-unique
  /// index; the C3 index forecloses a second unresolved row, so the
  /// upgrade is the only way the pruned fact can land on a node that
  /// already has a job). Armament is PRESERVED (the DQ-1 adjudication:
  /// the upgrade touches origin only — never state, parked, the claim
  /// holder, or the view), and production observes it via
  /// rio_scheduler_materialization_origin_upgraded_total. Guards
  /// mirror createJob's OPruned arm (live tenant whose same-tx merge
  /// batch wrote its wanted rows; node not locally covered) minus the
  /// no-unresolved-job guard this action inverts; fires only when it
  /// changes state (an already-pruned job needs no upgrade — economy).
  action dedupPrunedUpgrade(d: str, tenant: str): bool = all {
    BUILDS.contains(tenant),
    buildLive.get(tenant),
    wanted.get((tenant, d)) != Set(),
    hasUnresolvedJob(d),
    not(topdownPruned.get(d)),
    not(memLiveWanted(d).forall(o => present.get((d, o)))),
    jobs' = jobs.put(d, match jobs.get(d) {
      | NoJob => NoJob
      | SomeJob(j) => SomeJob({ ...j, origin: OPruned })
    }),
    topdownPruned' = topdownPruned.put(d, true),
    dupJob' = dupJob, view' = view, attempts' = attempts,
    nodeStatus' = nodeStatus, execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
    tenureUnchanged, ghostsUnchanged, a4Unchanged,
  }

  /// §2.2/§2.3: a store replica claims d's pending job — the kinded
  /// admission arm reading the VIEW (delta 5) + the fenced mint.
  /// Delta 6: legal from Queued or Ready (PD-6) and carries NO
  /// must_substitute conjunct — the claim IS the substitution (B1);
  /// the marked-node claim is ENABLED (canReachMarkedClaim).
  /// Delta 2a: blocked while the replica's channel is stale.
  action claimJob(d: str, replica: str): bool = all {
    REPLICAS.contains(replica),
    execsMinted < MAX_EXECS,
    not(channelStale.get(replica)),
    // A4: pull admission honors the view-only deferral (RetryLater) —
    // the ONLY reader of deferUntil (park/PD-20/stalled never read it).
    not(deferUntil.get(d)),
    view.get(d) == VPending(false),
    match jobs.get(d) {
      | NoJob => false
      | SomeJob(j) => j.state == JPending and not(j.parked)
    },
    attempts.get(d) == NoAttempt,
    nodeStatus.get(d) == NQueued or nodeStatus.get(d) == NReady,
    val newExec = execsMinted + 1
    all {
      attempts' = attempts.put(d, OpenAttempt({ kind: KMaterialization, instance: replica, execId: newExec })),
      jobs' = jobs.put(d, match jobs.get(d) {
        | NoJob => NoJob
        | SomeJob(j) => SomeJob({ ...j, state: JClaimed })
      }),
      view' = view.put(d, VClaimed(replica)),
      nodeStatus' = nodeStatus.put(d, NRunning),
      execsMinted' = newExec,
      claimAfterFailover' = or { claimAfterFailover, failedOver },
      // A2/318 ghost: record the claim-origin status (the
      // deps_completed input of the release contract) BEFORE the
      // NRunning overwrite above.
      claimOrigin' = claimOrigin.put(d, nodeStatus.get(d)),
    },
    a4Unchanged,
    dupJob' = dupJob, topdownPruned' = topdownPruned,
    relUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
    leaseGen' = leaseGen, staleWriteDiscarded' = staleWriteDiscarded,
    staleJobWriteApplied' = staleJobWriteApplied,
    channelStale' = channelStale, failedOver' = failedOver,
    failoverDroppedJobs' = failoverDroppedJobs,
    failFastCount' = failFastCount,
    ffJustifiedAll' = ffJustifiedAll, fsJustifiedAll' = fsJustifiedAll,
    fsReportSoundAll' = fsReportSoundAll, covAtResolveAll' = covAtResolveAll,
    covLiveOnlyAll' = covLiveOnlyAll,
    creationResolvableAll' = creationResolvableAll,
    chargedAfterCancel' = chargedAfterCancel,
    requeueReadyJustifiedAll' = requeueReadyJustifiedAll,
    carried' = carried,
    budgetSoundAll' = budgetSoundAll,
    abortChargeFree' = abortChargeFree,
  }

  /// §2.2 item 3 / §5: the claiming replica ingests one
  /// upstream-available live-wanted path and PINS IT AT INGEST
  /// (store-internal — no scheduler channel involved).
  action executeStep(d: str, o: str): bool = all {
    OUTPUTS.contains(o),
    match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) => a.kind == KMaterialization
    },
    liveWanted(d).contains(o),
    not(present.get((d, o))),
    upstreamAvail.get((d, o)),
    present' = present.put((d, o), true),
    pins' = pins.put((d, o), PinMaterialization),
    ingested' = ingested.put((d, o), true),
    jobCoreUnchanged, relUnchanged, ledgerUnchanged, envUnchanged,
    tenureUnchanged, ghostsUnchanged, a4Unchanged,
  }

  /// §2.4: the executor's Success report + the consumption
  /// transaction's coverage check (collapsed; the report travels the
  /// channel — delta 2a gates on the holder's channel). Covered =>
  /// resolved_success + node Completed + the §4 clear-mirror (delta
  /// 1); else (interest grew) the job re-arms.
  action consumeSuccess(d: str): bool = all {
    match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) => a.kind == KMaterialization and not(channelStale.get(a.instance))
    },
    match jobs.get(d) { | NoJob => false | SomeJob(j) => j.state == JClaimed },
    // A4 (merged_bug_194 store leg): the executor never REPORTS a
    // Success with nothing behind it — a zero-wanted zero-evidence
    // walk is InfraFailure at the store. The report carries a
    // non-empty live wanted set or actual store evidence.
    or {
      liveWanted(d) != Set(),
      OUTPUTS.exists(o => ingested.get((d, o)) or present.get((d, o))),
    },
    // R10: the settlement (job/view/status/mark + all three coverage
    // latches) is the seat's; the as-built decision set is the live
    // §6 join. The mat-f4 calibration routes the dead-inclusive
    // stored union through the same seat (its decision is the only
    // difference); mat-a4-vacuous-success keeps its own guard-dropped
    // replica (its subject latch has this live writer).
    consumeSettleApply(d, liveWanted(d)),
    deferUntil' = deferUntil, transientChargeFreeAll' = transientChargeFreeAll,
    closureCompleteAll' = closureCompleteAll, everContrib' = everContrib,
    leafReevalReleased' = leafReevalReleased,
    dupJob' = dupJob, execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
    tenureUnchanged,
    failFastCount' = failFastCount,
    ffJustifiedAll' = ffJustifiedAll, fsJustifiedAll' = fsJustifiedAll,
    fsReportSoundAll' = fsReportSoundAll,
    creationResolvableAll' = creationResolvableAll,
    chargedAfterCancel' = chargedAfterCancel,
    claimOrigin' = claimOrigin,
    requeueReadyJustifiedAll' = requeueReadyJustifiedAll,
    carried' = carried,
    budgetSoundAll' = budgetSoundAll,
    abortChargeFree' = abortChargeFree,
  }

  /// §2.4: the executor's Unobtainable report + the four-arm
  /// consumption routing (collapsed), AS BUILT at the Phase B tip
  /// (delta 1 — materialize.rs route_unobtainable):
  ///   arm 0 (moot): no live-wanted path missing -> covered ?
  ///     success-for-live-interest (clears the mark) : re-arm;
  ///   arm 1/2 (durable Vouched/Pending): resolved_from_source
  ///     (clears the mark);
  ///   arm 3 (Broken): the re-probe gate — Obtainable + unspent
  ///     one-shot -> re-arm; else MARKED -> FailFast (the
  ///     four-conjunct corner; consumes the mark, dispatch.rs:1126);
  ///     UNMARKED -> resolved_from_source (the B2 walk-equivalent
  ///     disposition).
  /// The one-shot reads the DRV-level unobtainable ledger (as built:
  /// count_materialization_rows_in_history — NOT the per-job count).
  /// The report precondition (missing paths verified absent AND
  /// unavailable) is the evidence the fsReportSound latch records.
  action consumeUnobtainable(d: str, missing: Set[str], refsMissing: bool): bool = all {
    // A4 (merged_bug_193): the report splits the miss cells — wanted
    // misses (`missing`, ⊆ OUTPUTS) and confirmed reference-closure
    // misses (`refsMissing` — modeled as the non-empty bit of the
    // missing_reference_paths field). A reference hole alone is a
    // reportable Unobtainable (pre-fix it was UNREPRESENTABLE here:
    // missing ⊆ OUTPUTS forced every report into the wanted cell).
    or { missing != Set(), refsMissing },
    missing.subseteq(OUTPUTS),
    match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) => a.kind == KMaterialization and not(channelStale.get(a.instance))
    },
    match jobs.get(d) { | NoJob => false | SomeJob(j) => j.state == JClaimed },
    // the report's missing paths really are missing-and-unavailable
    // (the executor verified them; B3's evidence-backed direction).
    missing.forall(o => not(present.get((d, o))) and not(upstreamAvail.get((d, o)))),
    matUnobtainableLedger.get(d) < MAT_BUDGET + 2,
    val w = liveWanted(d)
    val missingLive = missing.intersect(w)
    val execId = match attempts.get(d) { | NoAttempt => 0 | OpenAttempt(a) => a.execId }
    val marked = topdownPruned.get(d)
    // PER-JOB one-shot (the 085 reset-row window — the job's own
    // count, A3/020): the drv-level history read is preserved ONLY as
    // the mat-unscoped-count calibration.
    val oneShotSpent = match jobs.get(d) { | NoJob => false | SomeJob(j) => j.matUnobtainableCount > 0 }
    val ownShotSpent = oneShotSpent
    val arm0 = missingLive == Set()
    // A4 (merged_bug_193): the moot arm completes ONLY when BOTH cells
    // are clean — no live-wanted miss AND no confirmed reference hole.
    // A reference hole over a covered wanted set re-arms (the closure
    // is incomplete; completing would launder the hole).
    val arm0covered = arm0 and liveWantedCovered(d) and not(refsMissing)
    val arm12 = not(arm0) and (durableRel.get(d) == RVouched or durableRel.get(d) == RPending)
    val reprobeAllObtainable = w.forall(o => present.get((d, o)) or upstreamAvail.get((d, o)))
    val arm3rearm = not(arm0) and not(arm12) and reprobeAllObtainable and not(oneShotSpent)
    val arm3 = not(arm0) and not(arm12) and not(arm3rearm)
    val arm3fail = arm3 and marked
    val arm3fs = arm3 and not(marked)
    all {
      jobs' = jobs.put(d, match jobs.get(d) {
        | NoJob => NoJob
        | SomeJob(j) => SomeJob({ ...j,
            state: if (arm0covered) JResolvedSuccess
              else if (arm0) JPending
              else if (arm12 or arm3fs) JResolvedFromSource
              else if (arm3rearm) JPending
              else JResolvedUnobtainable,
            resolutionExec: if (arm0covered or arm12 or arm3fs or arm3fail) execId
              else j.resolutionExec,
            matUnobtainableCount: j.matUnobtainableCount + 1 })
      }),
      view' = if (arm0 and not(arm0covered)) view.put(d, VPending(false))
        else if (arm3rearm) view.put(d, VPending(false))
        else view.put(d, VNone),
      nodeStatus' = if (arm0covered)
        nodeStatus.put(d, NCompleted)
      else if (arm3fail)
        nodeStatus.put(d, NFailed)
      else if (arm12 or arm3fs)
        nodeStatus.put(d, NReady)
      else
        nodeStatus.put(d, NQueued),
      attempts' = attempts.put(d, NoAttempt),
      // delta 1: the mark is consumed by success (arm0covered),
      // from-source (arm12; arm3fs is unmarked already) AND the
      // fail-fast itself (dispatch.rs:1126).
      topdownPruned' = if (arm0covered or arm12 or arm3fail)
        topdownPruned.put(d, false)
      else topdownPruned,
      matUnobtainableLedger' = matUnobtainableLedger.put(d, matUnobtainableLedger.get(d) + 1),
      // A3/020: the one-shot routing consulted the job's OWN window —
      // the mat-unscoped-count override records false on divergence.
      budgetSoundAll' = and { budgetSoundAll, oneShotSpent == ownShotSpent },
      abortChargeFree' = abortChargeFree,
      failFastCount' = if (arm3fail) failFastCount.put(d, failFastCount.get(d) + 1) else failFastCount,
      // delta 1: the four-conjunct justification, at decision time.
      // marked/Broken/missing-live are structural at the arm3fail
      // site; the latch records the fourth conjunct, with the spent
      // one-shot only counting when its charges were evidence-backed.
      ffJustifiedAll' = and {
        ffJustifiedAll,
        arm3fail implies or {
          not(reprobeAllObtainable),
          oneShotSpent and unobChargeBacked.get(d),
        },
      },
      // the 6-row from-source justification, at decision time.
      fsJustifiedAll' = and {
        fsJustifiedAll,
        arm12 implies (durableRel.get(d) == RVouched or durableRel.get(d) == RPending),
        arm3fs implies (and {
          durableRel.get(d) == RChildlessLeaf or durableRel.get(d) == RHoled,
          not(marked),
        }),
      },
      // C2: the from-source routing consumed evidence-backed missing
      // paths (verified absent-and-unavailable at this decision).
      fsReportSoundAll' = and {
        fsReportSoundAll,
        (arm12 or arm3fs) implies
          missingLive.forall(o => not(present.get((d, o))) and not(upstreamAvail.get((d, o)))),
      },
      // CE-17 latch: the moot-covered arm is a success resolution.
      covAtResolveAll' = and { covAtResolveAll, arm0covered implies liveWantedCovered(d) },
      covLiveOnlyAll' = covLiveOnlyAll,
      // A4 latch (193): no completed resolution over a confirmed
      // reference hole — structurally true as-built (arm0covered
      // carries not(refsMissing)); the mat-a4-refs-folded override
      // re-introduces the fold and this latch records it.
      closureCompleteAll' = and {
        closureCompleteAll,
        arm0covered implies not(refsMissing),
      },
    },
    deferUntil' = deferUntil, transientChargeFreeAll' = transientChargeFreeAll,
    noVacuousCoverageAll' = noVacuousCoverageAll, everContrib' = everContrib,
    leafReevalReleased' = leafReevalReleased,
    dupJob' = dupJob, execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, envUnchanged,
    matInfraLedger' = matInfraLedger, buildCrashLedger' = buildCrashLedger,
    establishCount' = establishCount, buildCrashEstablished' = buildCrashEstablished,
    unobChargeBacked' = unobChargeBacked,
    tenureUnchanged,
    creationResolvableAll' = creationResolvableAll,
    chargedAfterCancel' = chargedAfterCancel,
    claimOrigin' = claimOrigin,
    requeueReadyJustifiedAll' = requeueReadyJustifiedAll,
    carried' = carried,
  }

  /// §2.5: the executor's InfraFailure report — the charge rides the
  /// materialization budget; AT the budget the job PARKS in the same
  /// consumption (delta 3, materialize.rs:826 — park is not a separate
  /// transition), else it re-arms. Never fail-fasts, never routes
  /// from-source (B3). The budget counter survives the park cycle
  /// (ceiling MAT_BUDGET+2 bounds the model).
  action reportInfra(d: str): bool = all {
    match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) => a.kind == KMaterialization and not(channelStale.get(a.instance))
    },
    match jobs.get(d) {
      | NoJob => false
      | SomeJob(j) => j.state == JClaimed and j.matInfraCount < MAT_BUDGET + 2
    },
    val newCount = match jobs.get(d) { | NoJob => 0 | SomeJob(j) => j.matInfraCount + 1 }
    val parks = newCount >= MAT_BUDGET
    all {
      jobs' = jobs.put(d, match jobs.get(d) {
        | NoJob => NoJob
        | SomeJob(j) => SomeJob({ ...j, state: JPending, parked: parks, matInfraCount: newCount })
      }),
      view' = view.put(d, VPending(parks)),
      // A3/067: the budget-soundness contract at this charge edge.
      budgetSoundAll' = and { budgetSoundAll, parks == (newCount >= MAT_BUDGET) },
      abortChargeFree' = abortChargeFree,
    },
    attempts' = attempts.put(d, NoAttempt),
    // R10: the re-arm status + the A2/318 release latch route through
    // the seat — the as-built decision is the conservative NQueued
    // restore (validate_transition_for_release's no-upgrade arm). The
    // mat-318 calibration routes the forced Ready through the same
    // seat.
    releaseStatusApply(d, false),
    matInfraLedger' = matInfraLedger.put(d, matInfraLedger.get(d) + 1),
    dupJob' = dupJob, topdownPruned' = topdownPruned, execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, envUnchanged,
    matUnobtainableLedger' = matUnobtainableLedger,
    buildCrashLedger' = buildCrashLedger,
    establishCount' = establishCount, buildCrashEstablished' = buildCrashEstablished,
    unobChargeBacked' = unobChargeBacked,
    tenureUnchanged,
    // ghostsUnchanged expanded: this charge edge writes the
    // budget-soundness latch (A3/067 — park applied iff the per-job
    // window reached the budget) and seats the release latch above.
    failFastCount' = failFastCount,
    ffJustifiedAll' = ffJustifiedAll, fsJustifiedAll' = fsJustifiedAll,
    fsReportSoundAll' = fsReportSoundAll, covAtResolveAll' = covAtResolveAll,
    covLiveOnlyAll' = covLiveOnlyAll,
    creationResolvableAll' = creationResolvableAll,
    chargedAfterCancel' = chargedAfterCancel,
    claimOrigin' = claimOrigin,
    carried' = carried,
    a4Unchanged,
  }

  /// A4 (merged_bug_178/195): the executor's RetryLater report — a
  /// transient substitution failure (raced placeholder slot, upstream
  /// 429) closes the attempt CHARGE-FREE: no ledger row of any class,
  /// no park-counter movement, and the only durable footprint is the
  /// VIEW-ONLY deferral that pull admission reads (claimJob's guard).
  /// The park decision, the park re-evaluation and the stalled gauge
  /// never read it — a 429 wave cannot walk a healthy job toward the
  /// from-source settlement. (A failover rebuilds the view without the
  /// deferral — the job loses at most one uncharged wait window.)
  action reportTransient(d: str): bool = all {
    match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) => a.kind == KMaterialization and not(channelStale.get(a.instance))
    },
    match jobs.get(d) { | NoJob => false | SomeJob(j) => j.state == JClaimed },
    all {
      jobs' = jobs.put(d, match jobs.get(d) {
        | NoJob => NoJob
        | SomeJob(j) => SomeJob({ ...j, state: JPending })
      }),
      view' = view.put(d, VPending(false)),
      nodeStatus' = nodeStatus.put(d, NQueued),
      attempts' = attempts.put(d, NoAttempt),
      deferUntil' = deferUntil.put(d, true),
      // The charge-free latch: both materialization ledgers, the crash
      // ledgers and the park flag are untouched at this edge — the
      // facts are recorded HERE (the override charges and records the
      // divergence).
      transientChargeFreeAll' = and {
        transientChargeFreeAll,
        match jobs.get(d) { | NoJob => true | SomeJob(j) => not(j.parked) },
      },
    },
    dupJob' = dupJob, topdownPruned' = topdownPruned, execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
    tenureUnchanged, ghostsUnchanged,
    closureCompleteAll' = closureCompleteAll,
    noVacuousCoverageAll' = noVacuousCoverageAll,
    everContrib' = everContrib,
    leafReevalReleased' = leafReevalReleased,
  }

  /// A4: the deferral window lapses — the job is claimable again.
  /// (Production: defer_until is a timestamp the admission read
  /// compares to now(); the model's bool flips back.)
  action deferExpires(d: str): bool = all {
    deferUntil.get(d),
    deferUntil' = deferUntil.put(d, false),
    transientChargeFreeAll' = transientChargeFreeAll,
    closureCompleteAll' = closureCompleteAll,
    noVacuousCoverageAll' = noVacuousCoverageAll,
    everContrib' = everContrib,
    leafReevalReleased' = leafReevalReleased,
    jobCoreUnchanged, relUnchanged, storeUnchanged, ledgerUnchanged,
    envUnchanged, tenureUnchanged, ghostsUnchanged,
  }

  /// §2.4 establishment (BC-2/BC-3/PP-4): a crashed worker's open
  /// attempt is established — BOTH kinds modeled. Materialization
  /// kind: materialization_infra charge THROUGH the kernel park
  /// verdict (the bughunt-wave Q5 reversal, owner-signed 2026-06-03:
  /// charge_materialization_infra fuses charge + decision; parking is
  /// party-blind, so establishment-only crash-loops park at
  /// MAT_BUDGET and join the stalled population instead of re-listing
  /// forever), NO adopt arm; build kind: executor_crash into the
  /// build ledger (+ the legitimate-channel ghost).
  /// Scheduler-internal — no channel gate. The pre-reversal
  /// never-parks behavior is preserved ONLY as the
  /// mat-establish-never-parks calibration.
  action establishCrashedAttempt(d: str): bool = all {
    ENABLE_CRASH,
    attempts.get(d) != NoAttempt,
    match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) =>
        if (a.kind == KMaterialization)
          matInfraLedger.get(d) < MAT_BUDGET + 2
        else
          buildCrashLedger.get(d) < MAT_BUDGET
    },
    val isMat = match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) => a.kind == KMaterialization
    }
    // The Q5 reversal: the establishment charge runs the SAME park
    // predicate as the worker consumption (per-job window — the 085
    // reset-row image; party-blind per config.rs's
    // conversion_requires_worker_charge doc).
    val newCount = match jobs.get(d) { | NoJob => 0 | SomeJob(j) => j.matInfraCount + 1 }
    val parks = isMat and newCount >= MAT_BUDGET
    all {
      attempts' = attempts.put(d, NoAttempt),
      jobs' = if (isMat) jobs.put(d, match jobs.get(d) {
        | NoJob => NoJob
        | SomeJob(j) => SomeJob({ ...j, state: JPending, parked: parks, matInfraCount: newCount })
      }) else jobs,
      view' = if (isMat) view.put(d, match jobs.get(d) {
        | NoJob => VNone
        | SomeJob(j) => VPending(parks)
      }) else view,
      nodeStatus' = nodeStatus.put(d, NQueued),
      matInfraLedger' = if (isMat) matInfraLedger.put(d, matInfraLedger.get(d) + 1) else matInfraLedger,
      establishCount' = if (isMat) establishCount.put(d, establishCount.get(d) + 1) else establishCount,
      buildCrashLedger' = if (isMat) buildCrashLedger else buildCrashLedger.put(d, buildCrashLedger.get(d) + 1),
      buildCrashEstablished' = if (isMat) buildCrashEstablished else buildCrashEstablished.put(d, true),
      // A3/067: the park applied iff the per-job window reached the
      // budget — the mat-establish-never-parks override records false.
      budgetSoundAll' = and {
        budgetSoundAll,
        isMat implies (parks == (newCount >= MAT_BUDGET)),
      },
      abortChargeFree' = abortChargeFree,
    },
    dupJob' = dupJob, topdownPruned' = topdownPruned, execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, envUnchanged,
    matUnobtainableLedger' = matUnobtainableLedger,
    unobChargeBacked' = unobChargeBacked,
    tenureUnchanged,
    failFastCount' = failFastCount,
    ffJustifiedAll' = ffJustifiedAll, fsJustifiedAll' = fsJustifiedAll,
    fsReportSoundAll' = fsReportSoundAll, covAtResolveAll' = covAtResolveAll,
    covLiveOnlyAll' = covLiveOnlyAll,
    creationResolvableAll' = creationResolvableAll,
    // The 276 ghost: a mat charge against a job that is no longer
    // unresolved (cancelled or gone) is the leaked-attempt
    // conversion. Unreachable as built (cancel closes the attempt in
    // the same tx); the dag-absent-cancel override reaches it.
    chargedAfterCancel' = or {
      chargedAfterCancel,
      match attempts.get(d) {
        | NoAttempt => false
        | OpenAttempt(a) => a.kind == KMaterialization and not(hasUnresolvedJob(d))
      },
    },
    claimOrigin' = claimOrigin,
    // A2/318: establishment releases restore NQueued (the line above
    // sets nodeStatus NQueued) — the latch formula is preserved-true
    // structurally, not merely framed.
    requeueReadyJustifiedAll' = requeueReadyJustifiedAll,
    carried' = carried,
    a4Unchanged,
  }

  /// §2.4 / BC-2: zero live in-memory interest cancels the job and
  /// closes any open attempt charge-free. Cancellation RESOLVES the
  /// row, and post-D-prime the pruned fact has no other carrier — the
  /// ghost ends with the row's unresolved state (delta 1: production
  /// has no mark column to outlive the row, migration 080).
  ///
  /// A1 (merged_bug_276): job + attempt close in ONE step — the
  /// production mirror is cancel_job_and_close_attempt_fenced, a
  /// single fenced transaction keyed on durable state. The action
  /// reads NO node status (total over the DAG-absent arm); the
  /// chargeFreeCancellation invariant pins that no materialization
  /// charge ever follows the cancellation (the calibration's split
  /// cancel leaves the attempt open and the establishment converts
  /// the leak into a charge).
  action cancelOnZeroInterest(d: str): bool = all {
    hasUnresolvedJob(d),
    memInterest(d) == Set(),
    jobs' = jobs.put(d, match jobs.get(d) {
      | NoJob => NoJob
      | SomeJob(j) => SomeJob({ ...j, state: JCancelled })
    }),
    view' = view.put(d, VNone),
    attempts' = attempts.put(d, NoAttempt),
    nodeStatus' = nodeStatus,
    topdownPruned' = topdownPruned.put(d, false),
    dupJob' = dupJob, execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
    tenureUnchanged, ghostsUnchanged, a4Unchanged,
  }

  /// A1 resolve-fault arms (ENABLE_RESOLVE_FAULTS; the view-settlement
  /// gate, sched.materialize.view-settlement). The FIXED system:
  ///   - resolveApplied: the settled durable resolution — modeled as
  ///     the single-tx zero-interest cancel (the one resolution arm
  ///     with no other justification input).
  ///   - resolveFenced / resolveErr: the durable write was REFUSED
  ///     (claims-floor fence) or ERRORED — the fixed system keeps the
  ///     job row, the VIEW ENTRY, and the attempt untouched (the
  ///     disposition gates every companion); both are observational
  ///     stutters here. The mat-133-discarded-outcome override
  ///     replaces them with the pre-fix view discard.
  action resolveApplied(d: str): bool = all {
    ENABLE_RESOLVE_FAULTS,
    cancelOnZeroInterest(d),
  }

  action resolveFenced(d: str): bool = all {
    ENABLE_RESOLVE_FAULTS,
    hasUnresolvedJob(d),
    jobCoreUnchanged,
    relUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
    tenureUnchanged, ghostsUnchanged, a4Unchanged,
  }

  action resolveErr(d: str): bool = all {
    ENABLE_RESOLVE_FAULTS,
    hasUnresolvedJob(d),
    jobCoreUnchanged,
    relUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
    tenureUnchanged, ghostsUnchanged, a4Unchanged,
  }

  /// §2.4: the node was Produced by other means: the job goes
  /// obsolete. Obsolescence resolves the row, so the pruned fact ends
  /// with it (delta 1 — the same structural clear as every other
  /// resolution arm).
  action obsoleteOnProduced(d: str): bool = all {
    hasUnresolvedJob(d),
    nodeStatus.get(d) == NCompleted,
    jobs' = jobs.put(d, match jobs.get(d) {
      | NoJob => NoJob
      | SomeJob(j) => SomeJob({ ...j, state: JObsolete })
    }),
    view' = view.put(d, VNone),
    attempts' = attempts.put(d, NoAttempt),
    nodeStatus' = nodeStatus,
    topdownPruned' = topdownPruned.put(d, false),
    dupJob' = dupJob, execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
    tenureUnchanged, ghostsUnchanged, a4Unchanged,
  }

  /// B1 bounded-await-transport (merged_bug_189 / owner Q3): the store
  /// replica's SIGTERM-aborted walk reports
  /// MaterializationOutcome::Aborted and the intake closes the attempt
  /// CHARGE-FREE through release_claim -- claim dropped, job back to
  /// JPending (claimable; the abort is a routine rollout artifact, not
  /// evidence), the node re-armed exactly as the claim found it (the
  /// A2/318 release contract over the claimOrigin witness), and the
  /// LEDGER UNTOUCHED: no materialization_infra row, no budget fold
  /// event, the 085 window unmoved.
  // Mirrors: scheduler Aborted intake (charge-free close + rearm,
  //          landed B1-s1) -> rio-scheduler release_claim (A3).
  action workerAborted(d: str): bool = all {
    ENABLE_WORKER_ABORTED,
    match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) => a.kind == KMaterialization
    },
    match jobs.get(d) { | NoJob => false | SomeJob(j) => j.state == JClaimed },
    // R10: the charge-free close routes through the abort-settlement
    // seat (charges=false — the Aborted intake's release_claim path;
    // the mat-189 calibration routes charges=true through the same
    // seat) and the origin-restore routes through the release seat
    // (toReady exactly when the claim originated Ready — the A2/318
    // contract over the claimOrigin witness).
    abortSettleApply(d, false),
    releaseStatusApply(d, claimOrigin.get(d) == NReady),
    attempts' = attempts.put(d, NoAttempt),
    topdownPruned' = topdownPruned,
    dupJob' = dupJob, execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, envUnchanged,
    // ledgerUnchanged expanded: the abort seat owns matInfraLedger
    // (identity at charges=false).
    matUnobtainableLedger' = matUnobtainableLedger,
    buildCrashLedger' = buildCrashLedger,
    establishCount' = establishCount, buildCrashEstablished' = buildCrashEstablished,
    unobChargeBacked' = unobChargeBacked,
    tenureUnchanged,
    // ghostsUnchanged expanded: the seats own the release latch, the
    // budget latch and the abort latch.
    failFastCount' = failFastCount,
    ffJustifiedAll' = ffJustifiedAll, fsJustifiedAll' = fsJustifiedAll,
    fsReportSoundAll' = fsReportSoundAll, covAtResolveAll' = covAtResolveAll,
    covLiveOnlyAll' = covLiveOnlyAll,
    creationResolvableAll' = creationResolvableAll,
    chargedAfterCancel' = chargedAfterCancel,
    claimOrigin' = claimOrigin,
    carried' = carried,
    a4Unchanged,
  }

  /// Delta 3 arm 1: the per-tick park RE-EVALUATION
  /// (tick_reevaluate_parked_materialization_jobs): a parked job whose
  /// durable evidence is Vouched/Pending resolves from_source NOW (no
  /// exec — resolutionExec stays 0), clears any stale mark, and
  /// requeues the node. Broken-evidence jobs are NOT touched (they are
  /// the stalled population; their armed action is the backoff expiry).
  action parkReevaluate(d: str): bool = all {
    match jobs.get(d) {
      | NoJob => false
      | SomeJob(j) => j.state == JPending and j.parked
    },
    // A4 re-derivation (the RBroken split): the re-evaluation releases
    // every from-source-VIABLE park — Vouched, Pending, and the
    // childless leaf while unmarked (pre-fix the leaf was collapsed
    // into Broken and could only cycle backoff→claim→infra→park; the
    // recorded flip is mat-a4-leaf-park-forever). Holed parks have
    // only the backoff arm — they ARE the stalled population.
    fromSourceViable(d),
    jobs' = jobs.put(d, match jobs.get(d) {
      | NoJob => NoJob
      | SomeJob(j) => SomeJob({ ...j, state: JResolvedFromSource, parked: false })
    }),
    view' = view.put(d, VNone),
    topdownPruned' = topdownPruned.put(d, false),
    nodeStatus' = nodeStatus.put(d, NReady),
    attempts' = attempts,
    dupJob' = dupJob, execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
    tenureUnchanged,
    failFastCount' = failFastCount,
    ffJustifiedAll' = ffJustifiedAll,
    fsJustifiedAll' = and {
      fsJustifiedAll,
      fromSourceViable(d),
    },
    fsReportSoundAll' = fsReportSoundAll, covAtResolveAll' = covAtResolveAll,
    covLiveOnlyAll' = covLiveOnlyAll,
    creationResolvableAll' = creationResolvableAll,
    chargedAfterCancel' = chargedAfterCancel,
    claimOrigin' = claimOrigin,
    requeueReadyJustifiedAll' = requeueReadyJustifiedAll,
    carried' = carried,
    budgetSoundAll' = budgetSoundAll,
    abortChargeFree' = abortChargeFree,
    deferUntil' = deferUntil,
    transientChargeFreeAll' = transientChargeFreeAll,
    closureCompleteAll' = closureCompleteAll,
    noVacuousCoverageAll' = noVacuousCoverageAll,
    everContrib' = everContrib,
    // the leaf-release reachability latch (the noLeafParkReevalResolve
    // witness; a Vouched/Pending release leaves it unchanged).
    leafReevalReleased' = or {
      leafReevalReleased,
      durableRel.get(d) == RChildlessLeaf,
    },
  }

  /// Delta 3 arm 2: the durable backoff lapses — the job un-parks for
  /// another claim cycle. The budget counter is NOT reset (as built:
  /// the surviving count drives the next park's exponential backoff).
  /// Enabled for any evidence (as built the expiry races the
  /// re-evaluation tick for Vouched/Pending parks; Broken parks have
  /// ONLY this arm).
  action parkBackoffExpires(d: str): bool = all {
    match jobs.get(d) {
      | NoJob => false
      | SomeJob(j) => j.state == JPending and j.parked
    },
    jobs' = jobs.put(d, match jobs.get(d) {
      | NoJob => NoJob
      | SomeJob(j) => SomeJob({ ...j, parked: false })
    }),
    view' = view.put(d, VPending(false)),
    attempts' = attempts, nodeStatus' = nodeStatus,
    dupJob' = dupJob, topdownPruned' = topdownPruned, execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
    tenureUnchanged, ghostsUnchanged, a4Unchanged,
  }

  /// A build reaches a terminal state: it leaves the live set; its
  /// wanted rows drop out of the §6 join atomically (the live-join —
  /// no stored-union residue, the C5 by-construction).
  action buildTerminal(b: str): bool = all {
    buildLive.get(b),
    buildLive' = buildLive.put(b, false),
    upstreamAvail' = upstreamAvail, upstreamFlipsLeft' = upstreamFlipsLeft,
    durableRel' = durableRel,
    jobCoreUnchanged, relUnchanged, storeUnchanged, ledgerUnchanged,
    tenureUnchanged, ghostsUnchanged, a4Unchanged,
  }

  /// §2.3 / delta 6 (post-D-prime): a builder pull for d — the BUILD
  /// column of the kinded table. The kernel reads the VIEW:
  /// JobView::None is required for DeliverNew, and that requirement
  /// SUBSUMES the deleted must_substitute refusal (the pruned fact
  /// implies an unresolved-or-parked job implies a non-None view —
  /// the delta-6 equivalence in the header; the kernel arm died in
  /// Waves D4.1/D5.2).
  action builderPull(d: str, builder: str): bool = all {
    BUILDERS.contains(builder),
    execsMinted < MAX_EXECS,
    view.get(d) == VNone,
    nodeStatus.get(d) == NReady,
    attempts.get(d) == NoAttempt,
    val newExec = execsMinted + 1
    all {
      attempts' = attempts.put(d, OpenAttempt({ kind: KBuild, instance: builder, execId: newExec })),
      nodeStatus' = nodeStatus.put(d, NRunning),
      execsMinted' = newExec,
    },
    jobs' = jobs, dupJob' = dupJob, view' = view, topdownPruned' = topdownPruned,
    relUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
    tenureUnchanged, ghostsUnchanged, a4Unchanged,
  }

  /// The from-source dispatch completes: the node completes, outputs
  /// become present with build pins.
  action dispatchFromSource(d: str): bool = all {
    match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) => a.kind == KBuild
    },
    nodeStatus' = nodeStatus.put(d, NCompleted),
    attempts' = attempts.put(d, NoAttempt),
    present' = OUTPUTS.fold(present, (acc, o) =>
      if (liveWanted(d).contains(o)) acc.put((d, o), true) else acc),
    pins' = OUTPUTS.fold(pins, (acc, o) =>
      if (liveWanted(d).contains(o)) acc.put((d, o), PinBuildInput) else acc),
    ingested' = ingested,
    jobs' = jobs, dupJob' = dupJob, view' = view, topdownPruned' = topdownPruned,
    execsMinted' = execsMinted,
    relUnchanged, ledgerUnchanged, envUnchanged, tenureUnchanged,
    ghostsUnchanged, a4Unchanged,
  }

  /// The dispatch-time store short-circuit (the D16 probe partition's
  /// present cell / complete_ready_from_store) AND the merge-time
  /// cache-hit completion (apply_cached_hits — Queued nodes complete
  /// from store presence regardless of dep state): a node whose
  /// live-wanted outputs are all present completes without any
  /// attempt. Job-carrying nodes that complete this way are closed by
  /// obsoleteOnProduced (the produced-by-other-means closer).
  action completeReadyFromStore(d: str): bool = all {
    nodeStatus.get(d) == NQueued or nodeStatus.get(d) == NReady,
    attempts.get(d) == NoAttempt,
    liveWanted(d) != Set(),
    liveWantedCovered(d),
    nodeStatus' = nodeStatus.put(d, NCompleted),
    jobs' = jobs, dupJob' = dupJob, view' = view, topdownPruned' = topdownPruned,
    attempts' = attempts, execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
    tenureUnchanged, ghostsUnchanged, a4Unchanged,
  }

  /// Leader failover (delta 5 + delta 2a): the lease generation bumps;
  /// jobs/attempts/relations/ledgers/pins are durable; the in-memory
  /// view is dropped and EAGERLY rebuilt from PG (T-4.3) — claim
  /// holders and park expiries mirrored, so the rebuilt view equals
  /// the faithful projection. Every executor channel goes STALE (the
  /// kube-proxy ClusterIP pinned to the deposed replica — finding 18).
  action failover = all {
    ENABLE_FAILOVER,
    leaseGen < MAX_GEN,
    leaseGen' = leaseGen + 1,
    view' = DRVS.mapBy(d => projView(jobs.get(d), attempts.get(d))),
    channelStale' = REPLICAS.mapBy(r => true),
    failedOver' = true,
    // R10: row durability routes through the seat — the as-built
    // decision is dropsRows=false (jobs are PG rows; the failover
    // cannot touch them). The mat-failover-drops-rows calibration
    // passes true and is failoverPreservesJobs' falsifier.
    failoverRowsApply(false),
    dupJob' = dupJob, attempts' = attempts,
    nodeStatus' = nodeStatus, topdownPruned' = topdownPruned,
    execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
    staleWriteDiscarded' = staleWriteDiscarded,
    staleJobWriteApplied' = staleJobWriteApplied,
    claimAfterFailover' = claimAfterFailover,
    ghostsUnchanged, a4Unchanged,
  }

  /// Delta 2a: the executor abandons its standby-pinned channel and
  /// re-dials (the UNAVAILABLE answer's inspect-outcome hook,
  /// client.rs:348 — the trigger RPC is collapsed into this action).
  /// Without this action (the pre-B3 calibration) every post-failover
  /// claim/report is blocked forever — canReachPostFailoverClaim goes
  /// unreachable.
  action redialChannel(replica: str): bool = all {
    REPLICAS.contains(replica),
    channelStale.get(replica),
    channelStale' = channelStale.put(replica, false),
    leaseGen' = leaseGen, staleWriteDiscarded' = staleWriteDiscarded,
    staleJobWriteApplied' = staleJobWriteApplied,
    failedOver' = failedOver, claimAfterFailover' = claimAfterFailover,
    failoverDroppedJobs' = failoverDroppedJobs,
    jobCoreUnchanged, relUnchanged, storeUnchanged, ledgerUnchanged,
    envUnchanged, ghostsUnchanged, a4Unchanged,
  }

  /// A job write from a previous lease generation arrives (stale
  /// tenure): the claims-floor fence DISCARDS it — no state changes
  /// except the ghost (F11: the same fence that covers every evidence
  /// write covers job-table and wanted-relation writes; the unfenced
  /// override applies the write instead and latches
  /// staleJobWriteApplied).
  action staleTenureWriteDiscarded(d: str): bool = all {
    ENABLE_STALE_TENURE,
    leaseGen > 0,
    hasUnresolvedJob(d),
    not(staleWriteDiscarded),
    // R10: the fence decision routes through the seat — the as-built
    // decision honors the claims-floor fence (the write is discarded;
    // only the probe ghost records the visit). The mat-f11
    // calibration routes fenceHonored=false through the same seat and
    // the write applies.
    staleWriteApply(d, true),
    leaseGen' = leaseGen,
    channelStale' = channelStale, failedOver' = failedOver,
    claimAfterFailover' = claimAfterFailover,
    failoverDroppedJobs' = failoverDroppedJobs,
    // jobCoreUnchanged expanded: the seat owns jobs and view
    // (identity at fenceHonored=true).
    dupJob' = dupJob, attempts' = attempts, nodeStatus' = nodeStatus,
    topdownPruned' = topdownPruned, execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, ledgerUnchanged,
    envUnchanged, ghostsUnchanged, a4Unchanged,
  }

  /// §5.3: the pin release — job resolved (or none) AND all in-memory
  /// interest terminal releases the materialization pins (PD-10; the
  /// all-interest-terminal release site). Build pins release through
  /// the build-input paths (out of model). The release CLOSES the
  /// ingest-guarantee window: the `ingested` ghost is cleared with the
  /// pin, so the B2-strong invariant ranges over the CURRENT window
  /// only — interest arriving after a legitimate release+GC is the
  /// stale_reset re-materialization lane (T-1.6), not a pin hole (the
  /// baseline-protocol catch recorded in the C-prime stage record).
  action releasePins(d: str): bool = all {
    not(hasUnresolvedJob(d)),
    memInterest(d) == Set(),
    OUTPUTS.exists(o => pins.get((d, o)) == PinMaterialization),
    pins' = OUTPUTS.fold(pins, (acc, o) =>
      if (acc.get((d, o)) == PinMaterialization) acc.put((d, o), NoPin) else acc),
    ingested' = OUTPUTS.fold(ingested, (acc, o) =>
      if (pins.get((d, o)) == PinMaterialization) acc.put((d, o), false) else acc),
    present' = present,
    jobCoreUnchanged, relUnchanged, ledgerUnchanged, envUnchanged,
    tenureUnchanged, ghostsUnchanged, a4Unchanged,
  }

  /// Store GC visits a (derivation, output) path: deletion is blocked
  /// by pins of either kind (§5).
  action storeGc(d: str, o: str): bool = all {
    ENABLE_GC,
    OUTPUTS.contains(o),
    present.get((d, o)),
    pins.get((d, o)) == NoPin,
    present' = present.put((d, o), false),
    pins' = pins, ingested' = ingested,
    jobCoreUnchanged, relUnchanged, ledgerUnchanged, envUnchanged,
    tenureUnchanged, ghostsUnchanged, a4Unchanged,
  }

  /// The upstream world changes (bounded).
  action upstreamChange(d: str, o: str): bool = all {
    upstreamFlipsLeft > 0,
    OUTPUTS.contains(o),
    upstreamAvail' = upstreamAvail.put((d, o), not(upstreamAvail.get((d, o)))),
    upstreamFlipsLeft' = upstreamFlipsLeft - 1,
    buildLive' = buildLive, durableRel' = durableRel,
    jobCoreUnchanged, relUnchanged, storeUnchanged, ledgerUnchanged,
    tenureUnchanged, ghostsUnchanged, a4Unchanged,
  }

  /// The durable declared-relation classification changes (deps
  /// produced by other builds, children declared).
  action durableRelationChange(d: str, r: DurableRelation): bool = all {
    durableRel.get(d) != r,
    durableRel' = durableRel.put(d, r),
    buildLive' = buildLive, upstreamAvail' = upstreamAvail,
    upstreamFlipsLeft' = upstreamFlipsLeft,
    jobCoreUnchanged, relUnchanged, storeUnchanged, ledgerUnchanged,
    tenureUnchanged, ghostsUnchanged, a4Unchanged,
  }

  // --- 9. init ------------------------------------------------------------------

  action init = all {
    jobs' = DRVS.mapBy(d => NoJob),
    dupJob' = DRVS.mapBy(d => false),
    view' = DRVS.mapBy(d => VNone),
    wanted' = tuples(BUILDS, DRVS).mapBy(p => Set()),
    lastWritten' = tuples(BUILDS, DRVS).mapBy(p => Set()),
    legacyInterest' = tuples(BUILDS, DRVS).mapBy(p => false),
    legacyBudget' = MAX_LEGACY,
    attempts' = DRVS.mapBy(d => NoAttempt),
    nodeStatus' = DRVS.mapBy(d => NQueued),
    topdownPruned' = DRVS.mapBy(d => false),
    present' = tuples(DRVS, OUTPUTS).mapBy(p => false),
    pins' = tuples(DRVS, OUTPUTS).mapBy(p => NoPin),
    ingested' = tuples(DRVS, OUTPUTS).mapBy(p => false),
    matInfraLedger' = DRVS.mapBy(d => 0),
    matUnobtainableLedger' = DRVS.mapBy(d => 0),
    buildCrashLedger' = DRVS.mapBy(d => 0),
    establishCount' = DRVS.mapBy(d => 0),
    buildCrashEstablished' = DRVS.mapBy(d => false),
    unobChargeBacked' = DRVS.mapBy(d => true),
    leaseGen' = 0,
    staleWriteDiscarded' = false,
    staleJobWriteApplied' = false,
    chargedAfterCancel' = false,
    carried' = DRVS.mapBy(d => false),
    budgetSoundAll' = true,
    abortChargeFree' = true,
    channelStale' = REPLICAS.mapBy(r => false),
    failedOver' = false,
    claimAfterFailover' = false,
    failoverDroppedJobs' = false,
    buildLive' = BUILDS.mapBy(b => true),
    upstreamAvail' = tuples(DRVS, OUTPUTS).mapBy(p => true),
    upstreamFlipsLeft' = MAX_UPSTREAM_FLIPS,
    durableRel' = DRVS.mapBy(d => RHoled),
    execsMinted' = 0,
    failFastCount' = DRVS.mapBy(d => 0),
    ffJustifiedAll' = true,
    fsJustifiedAll' = true,
    fsReportSoundAll' = true,
    covAtResolveAll' = true,
    covLiveOnlyAll' = true,
    creationResolvableAll' = true,
    claimOrigin' = DRVS.mapBy(d => NQueued),
    requeueReadyJustifiedAll' = true,
    deferUntil' = DRVS.mapBy(d => false),
    transientChargeFreeAll' = true,
    closureCompleteAll' = true,
    noVacuousCoverageAll' = true,
    everContrib' = tuples(BUILDS, DRVS).mapBy(p => Set()),
    leafReevalReleased' = false,
    walkCells' = emptyCells,
    cellsArmed' = false,
    reprobeCongruentAll' = true,
    probeUnchargedOn429All' = true,
    anyTenantHitServesAll' = true,
    localServeVisibleAll' = true,
    walkFoldSeen' = false,
    stampEv' = emptyStampEv,
    stampArmed' = false,
    stampLawAll' = true,
    stampFoldSeen' = false,
  }

  // --- 8b. slot-10 walk-fold seat + callers ----------------------------------

  /// The full machine frozen (the plane branches' frame): the plane
  /// reads nothing from and writes nothing to the job machine.
  action machineFrame = all {
    jobCoreUnchanged,
    relUnchanged,
    storeUnchanged,
    ledgerUnchanged,
    envUnchanged,
    tenureUnchanged,
    ghostsUnchanged,
    a4Unchanged,
  }

  /// The visible-local-row predicate over the ARMED cells (bug_115's
  /// lawful serve source).
  val armedLocalVisible: bool = OUTPUTS.exists(p => and {
    walkCells.localPresent.contains(p),
    TENANTS.exists(t => walkCells.localVisible.contains((t, p))),
  })

  /// fold_path_reprobes' quantifier over the ARMED cells: exists path
  /// missing under EVERY tenant (bug_299's lawful ConfirmedMissing).
  val armedConfirmedMissing: bool = and {
    walkCells.served == Set(),
    OUTPUTS.exists(p => TENANTS.forall(t => walkCells.missing.contains((t, p)))),
  }

  /// The curated evaluation matrices: the four laws' full decision
  /// partition (every partition class of every law appears, including
  /// every calibration twin's violation cell). Curated rather than
  /// drawn from six powersets: the powerset draw ran ~32x slower per
  /// simulator trace (40 ms vs 1.3 ms measured — the slot-7
  /// eternal-gate class), and the laws' evidence scales with
  /// partition coverage, not raw matrix count; the TLC calibration
  /// pins enumerate this set exhaustively at shallow depth.
  pure val WALK_MATRICES: Set[WalkCells] = Set(
    emptyCells,
    // bug_299 defining cell: complementary coverage.
    { ...emptyCells, missing: Set(("tA", "o1"), ("tB", "o2")) },
    // common missing path (ConfirmedMissing truth).
    { ...emptyCells, missing: Set(("tA", "o1"), ("tB", "o1")) },
    // everything missing everywhere.
    { ...emptyCells, missing: Set(("tA", "o1"), ("tA", "o2"), ("tB", "o1"), ("tB", "o2")) },
    // one tenant missing both paths, the other clean.
    { ...emptyCells, missing: Set(("tA", "o1"), ("tA", "o2")) },
    // a serving tenant, nothing else.
    { ...emptyCells, served: Set("tA") },
    // merged_bug_133 cell: a charging tenant beside a serving one.
    { ...emptyCells, served: Set("tB"), charging: Set("tA") },
    // bug_295 cell: rate-limited only.
    { ...emptyCells, rateLimited: Set("tA", "tB") },
    // rate-limited beside charging (charge lawfully wins).
    { ...emptyCells, rateLimited: Set("tA"), charging: Set("tB") },
    // charging only.
    { ...emptyCells, charging: Set("tA") },
    // bug_115 cell: a present row with NO visible cell.
    { ...emptyCells, localPresent: Set("o1") },
    // present + visible (the lawful local serve).
    { ...emptyCells, localPresent: Set("o1"), localVisible: Set(("tA", "o1")) },
    // visible cell exists but NOT for a present path (alignment).
    { ...emptyCells, localPresent: Set("o1"), localVisible: Set(("tB", "o2")) },
    // mixed: a hit beside a visible row, a miss, and a 429.
    { served: Set("tA"), localPresent: Set("o2"),
      localVisible: Set(("tB", "o2")), missing: Set(("tB", "o1")),
      rateLimited: Set("tB"), charging: Set() },
    // complementary coverage beside a 429 (299 x 295 interaction).
    { ...emptyCells, missing: Set(("tA", "o1"), ("tB", "o2")), rateLimited: Set("tA") },
    // common missing beside a 429.
    { ...emptyCells, missing: Set(("tA", "o2"), ("tB", "o2")), rateLimited: Set("tB") }
  )

  /// Record one evaluation's raw cells (parameterized so the run pins
  /// and the twins can arm literal matrices).
  action walkObserve(cells: WalkCells): bool = all {
    ENABLE_WALK_FOLD,
    not(cellsArmed),
    walkCells' = cells,
    cellsArmed' = true,
    walkLatchesUnchanged,
  }

  /// One curated evaluation.
  action walkObserveAny = {
    nondet cells = WALK_MATRICES.oneOf()
    walkObserve(cells)
  }

  /// Walk-fold seat (§8a): the caller passes its four FOLD DECISIONS
  /// (bool-only); the seat recomputes each law from the ARMED CELLS
  /// (live state, P4) and accumulates the latches. Consumes the armed
  /// evaluation.
  action walkFoldApply(
    decideServe: bool,
    decideLocalServe: bool,
    decideConfirmedMissing: bool,
    decideCharge: bool
  ): bool = all {
    cellsArmed,
    walkFoldSeen' = true,
    cellsArmed' = false,
    walkCells' = emptyCells,
    stampSubPlaneUnchanged,
    // merged_bug_133: a serving tenant is never starved by the fold.
    anyTenantHitServesAll' = and {
      anyTenantHitServesAll,
      (walkCells.served != Set()) implies decideServe,
    },
    // bug_115: a local serve requires a gate-visible cell.
    localServeVisibleAll' = and {
      localServeVisibleAll,
      decideLocalServe implies armedLocalVisible,
    },
    // bug_299: ConfirmedMissing iff the per-path quantifier over the
    // raw cells — never a per-tenant pre-fold.
    reprobeCongruentAll' = and {
      reprobeCongruentAll,
      decideConfirmedMissing iff armedConfirmedMissing,
    },
    // bug_295: all-429, nothing charging, nothing served closes
    // uncharged (congruence per CLASS through the one truth table).
    probeUnchargedOn429All' = and {
      probeUnchargedOn429All,
      and {
        walkCells.rateLimited != Set(),
        walkCells.charging == Set(),
        walkCells.served == Set(),
      } implies not(decideCharge),
    },
  }

  /// The LIVE fold: decisions derived lawfully from the armed cells.
  action walkFold = all {
    walkFoldApply(
      or { walkCells.served != Set(), armedLocalVisible },
      armedLocalVisible,
      armedConfirmedMissing,
      and { walkCells.charging != Set(), walkCells.served == Set() }
    ),
  }

  // --- 8c. bughunt-3 S3 stamp seat + callers ---------------------------------

  /// The lawful (tenant, output) ownership pairs derived from the
  /// ARMED evidence — the model image of `StampProvenance::
  /// lawful_pairs` (live_pins.rs, THE one body, signed Q2):
  /// WalkVerified intersects the wire pairs with the attributed
  /// tenants; BuiltLocally / AllTenantProbe lawfully cover the
  /// attributed cartesian (locally produced bytes / every tenant's
  /// own probe answered); ProbedBy stamps only the probing tenant,
  /// and only if attributed.
  def lawfulStampPairs: Set[(str, str)] =
    match stampEv.provenance {
      | PWalkVerified => stampEv.verified.filter(p =>
          and { stampEv.attributed.contains(p._1), stampEv.outputs.contains(p._2) })
      | PBuiltLocally => tuples(stampEv.attributed, stampEv.outputs)
      | PAllTenantProbe => tuples(stampEv.attributed, stampEv.outputs)
      | PProbedBy =>
          if (stampEv.attributed.contains(stampEv.prober))
            tuples(Set(stampEv.prober), stampEv.outputs)
          else Set()
    }

  /// Curated producer-evidence matrices (the discriminating shapes;
  /// mirrors WALK_MATRICES). The first is the bug_139 defining case:
  /// the wire verified ONE (tenant, output) pair while two tenants
  /// are attributed over two outputs — the pre-fix cartesian stamp
  /// exceeds the lawful set by three pairs.
  pure val STAMP_MATRICES: Set[StampEvidence] = Set(
    // ∃-gate / ∀-stamp discriminator: verified ⊊ attributed × outputs.
    { provenance: PWalkVerified, verified: Set(("tA", "o1")),
      prober: "tA", attributed: Set("tA", "tB"), outputs: Set("o1", "o2") },
    // Everything verified — lawful == cartesian (boundary case).
    { provenance: PWalkVerified,
      verified: Set(("tA", "o1"), ("tA", "o2"), ("tB", "o1"), ("tB", "o2")),
      prober: "tA", attributed: Set("tA", "tB"), outputs: Set("o1", "o2") },
    // Conservative-under-skew: no wire sets — stamp NOTHING.
    { provenance: PWalkVerified, verified: Set(),
      prober: "tA", attributed: Set("tA", "tB"), outputs: Set("o1") },
    // Locally produced bytes: the attributed cartesian is lawful.
    { provenance: PBuiltLocally, verified: Set(),
      prober: "tA", attributed: Set("tA", "tB"), outputs: Set("o1") },
    // Every stamped tenant's own probe answered present.
    { provenance: PAllTenantProbe, verified: Set(),
      prober: "tB", attributed: Set("tB"), outputs: Set("o2") },
    // Single-evidence probe: only the prober, only if attributed.
    { provenance: PProbedBy, verified: Set(),
      prober: "tA", attributed: Set("tA", "tB"), outputs: Set("o1", "o2") },
    // Prober NOT attributed: nothing is lawful.
    { provenance: PProbedBy, verified: Set(),
      prober: "tB", attributed: Set("tA"), outputs: Set("o1") }
  )

  /// Arm one producer's stamp evidence (parameterized so the run pins
  /// and the calibration can arm literal evidence).
  action stampObserve(ev: StampEvidence): bool = all {
    ENABLE_WALK_FOLD,
    not(stampArmed),
    stampEv' = ev,
    stampArmed' = true,
    stampLawAll' = stampLawAll,
    stampFoldSeen' = stampFoldSeen,
    walkSubPlaneUnchanged,
  }

  /// One curated evidence arming.
  action stampObserveAny = {
    nondet ev = STAMP_MATRICES.oneOf()
    stampObserve(ev)
  }

  /// Stamp seat (§8a discipline): the caller passes its DECIDED pair
  /// set; the seat recomputes the law from the ARMED evidence (live
  /// state, P4) and accumulates the latch. Consumes the armed event.
  action stampApply(decidedPairs: Set[(str, str)]): bool = all {
    stampArmed,
    stampFoldSeen' = true,
    stampArmed' = false,
    stampEv' = emptyStampEv,
    // bug_139: every stamped pair is within the witness's lawful set.
    stampLawAll' = and {
      stampLawAll,
      decidedPairs.subseteq(lawfulStampPairs),
    },
    walkSubPlaneUnchanged,
  }

  /// The LIVE stamp: pairs derived lawfully from the armed evidence
  /// (the production funnel — `upsert_path_tenants` derives the pair
  /// set FROM the provenance witness).
  action stampFold = all {
    stampApply(lawfulStampPairs),
  }

  // --- 10. step --------------------------------------------------------------------

  action step = {
    nondet d = DRVS.oneOf()
    nondet b = BUILDS.oneOf()
    nondet o = OUTPUTS.oneOf()
    nondet replica = REPLICAS.oneOf()
    nondet builder = BUILDERS.oneOf()
    nondet origin = Set(OProbe, OMerge, OPruned, OReprobe, OStaleReset).oneOf()
    nondet outs = OUTPUTS.powerset().oneOf()
    nondet missing = OUTPUTS.powerset().oneOf()
    nondet rel = Set(RVouched, RPending, RChildlessLeaf, RHoled).oneOf()
    nondet refsMissing = Set(true, false).oneOf()
    any {
      // the machine moves, the walk plane frozen (slot 10: the plane
      // composes at the STEP level so no machine action is edited).
      all {
        any {
      recordWantedRelation(b, d, outs),
      legacyBuildArrives(b, d),
      createJob(d, origin, b),
      dedupRefeed(d),
      dedupPrunedUpgrade(d, b),
      claimJob(d, replica),
      executeStep(d, o),
      consumeSuccess(d),
      consumeUnobtainable(d, missing, refsMissing),
      workerAborted(d),
      reportTransient(d),
      deferExpires(d),
      reportInfra(d),
      establishCrashedAttempt(d),
      cancelOnZeroInterest(d),
      resolveApplied(d),
      resolveFenced(d),
      resolveErr(d),
      obsoleteOnProduced(d),
      parkReevaluate(d),
      parkBackoffExpires(d),
      buildTerminal(b),
      builderPull(d, builder),
      dispatchFromSource(d),
      completeReadyFromStore(d),
      failover,
      redialChannel(replica),
      staleTenureWriteDiscarded(d),
      releasePins(d),
      storeGc(d, o),
      upstreamChange(d, o),
      durableRelationChange(d, rel),
        },
        walkUnchanged,
      },
      all { walkObserveAny, machineFrame },
      all { walkFold, machineFrame },
      all { stampObserveAny, machineFrame },
      all { stampFold, machineFrame },
    }
  }

  // --- 11. runs (deterministic scenario pins) -----------------------------------

  /// Slot-10: the bug_299 defining cell — complementary coverage (tA
  /// missing o1 only, tB missing o2 only: each tenant misses
  /// SOMETHING, no path is missing under BOTH). The lawful fold
  /// answers Obtainable and every walk latch holds; the pre-fix
  /// whole-set projection folded ConfirmedMissing here (the mat-299
  /// calibration re-finds it). Runs under the walk regime.
  run walkFoldComplementaryCoverageRun = {
    init
      .then(all {
        walkObserve({
          served: Set(), localPresent: Set(), localVisible: Set(),
          missing: Set(("tA", "o1"), ("tB", "o2")),
          rateLimited: Set(), charging: Set(),
        }),
        machineFrame,
      })
      .then(all { walkFold, machineFrame })
      .expect(and {
        walkFoldSeen,
        not(cellsArmed),
        reprobeCongruentAll,
        probeUnchargedOn429All,
        anyTenantHitServesAll,
        localServeVisibleAll,
      })
  }

  /// Bughunt-3 S3: the bug_139 defining evidence — the wire verified
  /// exactly (tA, o1) while two tenants are attributed over two
  /// outputs. The LIVE stamp derives the lawful singleton and the
  /// law holds; the pre-fix ∃-gate/∀-stamp cartesian (the mat-139
  /// calibration re-finds it) stamps all four pairs and falsifies.
  /// Runs under the walk regime.
  run stampLawWalkVerifiedRun = {
    init
      .then(all {
        stampObserve({
          provenance: PWalkVerified, verified: Set(("tA", "o1")),
          prober: "tA", attributed: Set("tA", "tB"),
          outputs: Set("o1", "o2"),
        }),
        machineFrame,
      })
      .then(all { stampFold, machineFrame })
      .expect(and {
        stampFoldSeen,
        not(stampArmed),
        stampLawAll,
      })
  }

  /// The §9.1 happy path: relation, probe-origin job, claim, ingest,
  /// success consumption -> resolved_success, node Completed, pinned.
  run happyPathRun = {
    init
      .then(all { recordWantedRelation("b1", "d1", Set("o1")), walkUnchanged })
      .then(all { createJob("d1", OProbe, "b1"), walkUnchanged })
      .then(all { claimJob("d1", "r1"), walkUnchanged })
      .then(all { executeStep("d1", "o1"), walkUnchanged })
      .then(all { consumeSuccess("d1"), walkUnchanged })
      .expect(and {
        match jobs.get("d1") {
          | NoJob => false
          | SomeJob(j) => j.state == JResolvedSuccess
        },
        nodeStatus.get("d1") == NCompleted,
        present.get(("d1", "o1")),
        pins.get(("d1", "o1")) == PinMaterialization,
        view.get("d1") == VNone,
      })
  }

  /// live_061 (the matmodel-gap repair): the by-other-means trace,
  /// production-shaped. The executor ingests the wanted output and
  /// then closes its attempt with an infra failure (one charge, under
  /// budget — unparked, node released NQueued); the store
  /// short-circuit then completes the node from local presence with
  /// NO open attempt, and the still-pending job goes OBSOLETE — the
  /// enum's exact letter, the resolution arm that had zero production
  /// writers for the system's entire life. The mbt-rio-materialization
  /// conformance check replays this run against the real scheduler:
  /// at the pre-fold tree the moot sweep resolves the row `cancelled`
  /// and the replay reds at the final step (spec JObsolete vs
  /// implementation JCancelled — the live_061 conflation, W12-S9B's
  /// twin observable).
  run obsoleteOnProducedRun = {
    init
      .then(all { recordWantedRelation("b1", "d1", Set("o1")), walkUnchanged })
      .then(all { createJob("d1", OProbe, "b1"), walkUnchanged })
      .then(all { claimJob("d1", "r1"), walkUnchanged })
      .then(all { executeStep("d1", "o1"), walkUnchanged })
      .then(all { reportInfra("d1"), walkUnchanged })
      .expect(and {
        match jobs.get("d1") {
          | NoJob => false
          | SomeJob(j) => j.state == JPending and not(j.parked) and j.matInfraCount == 1
        },
        nodeStatus.get("d1") == NQueued,
        attempts.get("d1") == NoAttempt,
        present.get(("d1", "o1")),
      })
      .then(all { completeReadyFromStore("d1"), walkUnchanged })
      .expect(and {
        nodeStatus.get("d1") == NCompleted,
        match jobs.get("d1") { | NoJob => false | SomeJob(j) => j.state == JPending },
      })
      .then(all { obsoleteOnProduced("d1"), walkUnchanged })
      .expect(and {
        match jobs.get("d1") {
          | NoJob => false
          | SomeJob(j) => j.state == JObsolete
        },
        nodeStatus.get("d1") == NCompleted,
        view.get("d1") == VNone,
        attempts.get("d1") == NoAttempt,
        not(topdownPruned.get("d1")),
      })
  }

  /// Delta 3: infra failures park IN the consumption at the budget
  /// (no separate park transition); the Broken-evidence park is the
  /// stalled population; the backoff expiry un-parks WITHOUT resetting
  /// the count; the re-evaluation resolves a Vouched park from source.
  run parkSplitRun = {
    init
      .then(all { recordWantedRelation("b1", "d1", Set("o1")), walkUnchanged })
      .then(all { createJob("d1", OProbe, "b1"), walkUnchanged })
      .then(all { claimJob("d1", "r1"), walkUnchanged })
      .then(all { reportInfra("d1"), walkUnchanged })
      .then(all { claimJob("d1", "r1"), walkUnchanged })
      .then(all { reportInfra("d1"), walkUnchanged })
      .expect(and {
        match jobs.get("d1") {
          | NoJob => false
          | SomeJob(j) => j.parked and j.state == JPending and j.matInfraCount == 2
        },
        nodeStatus.get("d1") != NFailed,
        stalledJobs == Set("d1"),
        view.get("d1") == VPending(true),
      })
      .then(all { parkBackoffExpires("d1"), walkUnchanged })
      .expect(match jobs.get("d1") {
        | NoJob => false
        | SomeJob(j) => not(j.parked) and j.matInfraCount == 2
      })
      .then(all { claimJob("d1", "r2"), walkUnchanged })
      .then(all { reportInfra("d1"), walkUnchanged })
      .expect(match jobs.get("d1") {
        | NoJob => false
        | SomeJob(j) => j.parked and j.matInfraCount == 3
      })
      .then(all { durableRelationChange("d1", RVouched), walkUnchanged })
      .then(all { parkReevaluate("d1"), walkUnchanged })
      .expect(and {
        match jobs.get("d1") {
          | NoJob => false
          | SomeJob(j) => j.state == JResolvedFromSource and j.resolutionExec == 0
        },
        nodeStatus.get("d1") == NReady,
        view.get("d1") == VNone,
      })
  }

  /// Delta 1: the four-conjunct fail-fast — pruned-origin (marked),
  /// Broken evidence, live-wanted path confirmed missing upstream.
  /// The fail-fast consumes the mark.
  run prunedFailFastRun = {
    init
      .then(all { recordWantedRelation("b1", "d1", Set("o1")), walkUnchanged })
      .then(all { createJob("d1", OPruned, "b1"), walkUnchanged })
      .expect(topdownPruned.get("d1"))
      .then(all { upstreamChange("d1", "o1"), walkUnchanged })
      .then(all { claimJob("d1", "r1"), walkUnchanged })
      .then(all { consumeUnobtainable("d1", Set("o1"), false), walkUnchanged })
      .expect(and {
        nodeStatus.get("d1") == NFailed,
        failFastCount.get("d1") == 1,
        ffJustifiedAll,
        not(topdownPruned.get("d1")),
        match jobs.get("d1") {
          | NoJob => false
          | SomeJob(j) => j.state == JResolvedUnobtainable
        },
      })
  }

  /// Delta 1 (the B2 fix row): the SAME shape UNMARKED resolves from
  /// source — the walk-equivalent disposition — and the build then
  /// dispatches from source.
  run unmarkedArm3FromSourceRun = {
    init
      .then(all { recordWantedRelation("b1", "d1", Set("o1")), walkUnchanged })
      .then(all { createJob("d1", OProbe, "b1"), walkUnchanged })
      .then(all { upstreamChange("d1", "o1"), walkUnchanged })
      .then(all { claimJob("d1", "r1"), walkUnchanged })
      .then(all { consumeUnobtainable("d1", Set("o1"), false), walkUnchanged })
      .expect(and {
        nodeStatus.get("d1") == NReady,
        failFastCount.get("d1") == 0,
        fsJustifiedAll,
        match jobs.get("d1") {
          | NoJob => false
          | SomeJob(j) => j.state == JResolvedFromSource
        },
      })
      .then(all { builderPull("d1", "w1"), walkUnchanged })
      .then(all { dispatchFromSource("d1"), walkUnchanged })
      .expect(nodeStatus.get("d1") == NCompleted)
  }

  /// Delta 6 / B1: a MARKED (pruned-origin) Ready node with an
  /// unparked pending job is claimable (the B1 claim upgrade); the
  /// builder pull of the same node is blocked by the JobView arm
  /// (VPending != VNone — post-D-prime the view IS the refusal; the
  /// kernel's A11 arm is deleted).
  run markedClaimUpgradeRun = {
    init
      .then(all { recordWantedRelation("b1", "d1", Set("o1")), walkUnchanged })
      .then(all { createJob("d1", OPruned, "b1"), walkUnchanged })
      .then(all { claimJob("d1", "r1"), walkUnchanged })
      .expect(and {
        topdownPruned.get("d1"),
        match attempts.get("d1") {
          | NoAttempt => false
          | OpenAttempt(a) => a.kind == KMaterialization
        },
      })
  }

  /// Delta 2b: a legacy (pre-relation-era) build's interest is backfilled
  /// into the relation by the probe-partition creation; the job is
  /// tenant-resolvable at creation commit.
  run legacyBackfillRun = {
    init
      .then(all { legacyBuildArrives("b1", "d1"), walkUnchanged })
      .expect(liveWanted("d1") == Set())
      .then(all { createJob("d1", OProbe, "b1"), walkUnchanged })
      .expect(and {
        wanted.get(("b1", "d1")) == OUTPUTS,
        not(legacyInterest.get(("b1", "d1"))),
        liveWanted("d1") != Set(),
        creationResolvableAll,
      })
  }

  /// Delta 4: the stale-Completed verify — a Completed node whose
  /// live-wanted output vanished gets a stale_reset job and the node
  /// resets to Queued in the same action.
  run staleResetRun = {
    init
      .then(all { recordWantedRelation("b1", "d1", Set("o1")), walkUnchanged })
      .then(all { createJob("d1", OProbe, "b1"), walkUnchanged })
      .then(all { claimJob("d1", "r1"), walkUnchanged })
      .then(all { executeStep("d1", "o1"), walkUnchanged })
      .then(all { consumeSuccess("d1"), walkUnchanged })
      .expect(nodeStatus.get("d1") == NCompleted)
      .then(all { buildTerminal("b1"), walkUnchanged })
      .then(all { releasePins("d1"), walkUnchanged })
      .then(all { storeGc("d1", "o1"), walkUnchanged })
      .expect(not(present.get(("d1", "o1"))))
      .then(all { recordWantedRelation("b2", "d1", Set("o1")), walkUnchanged })
      .then(all { createJob("d1", OStaleReset, "b2"), walkUnchanged })
      .expect(and {
        nodeStatus.get("d1") == NQueued,
        match jobs.get("d1") {
          | NoJob => false
          | SomeJob(j) => j.state == JPending and j.origin == OStaleReset
        },
      })
  }

  /// Delta 4: the reprobe lane — an existing Ready node re-merged
  /// creates a reprobe-origin job WITH the AS-5 status reset.
  /// A3/020 (the per-job budget window — migration 085's image):
  /// job 1 accrues budget-1 charges and resolves from source; job 2
  /// is created (the creation reset starts a FRESH window) and its
  /// first infra charge re-arms instead of parking. The pre-fix flat
  /// history count would have parked job 2 immediately
  /// (mat-unscoped-count preserves that encoding).
  run twoJobFreshWindowRun = {
    init
      .then(all { recordWantedRelation("b1", "d1", Set("o1")), walkUnchanged })
      .then(all { createJob("d1", OProbe, "b1"), walkUnchanged })
      .then(all { claimJob("d1", "r1"), walkUnchanged })
      .then(all { reportInfra("d1"), walkUnchanged })
      .expect(match jobs.get("d1") {
        | NoJob => false
        | SomeJob(j) => j.state == JPending and not(j.parked) and j.matInfraCount == 1
      })
      // kill the upstream so the next report's missing path is
      // evidence-backed, then resolve job 1 from source (arm 3,
      // RBroken + unmarked).
      .then(all { upstreamChange("d1", "o1"), walkUnchanged })
      .then(all { claimJob("d1", "r1"), walkUnchanged })
      .then(all { consumeUnobtainable("d1", Set("o1"), false), walkUnchanged })
      .expect(match jobs.get("d1") {
        | NoJob => false
        | SomeJob(j) => j.state == JResolvedFromSource
      })
      // job 2: the creation reset starts a FRESH per-job window — its
      // first infra charge re-arms (count 1 < MAT_BUDGET 2), where the
      // pre-fix flat history count (already 2 by job 1) would park.
      .then(all { createJob("d1", OProbe, "b1"), walkUnchanged })
      .then(all { claimJob("d1", "r2"), walkUnchanged })
      .then(all { reportInfra("d1"), walkUnchanged })
      .expect(and {
        match jobs.get("d1") {
          | NoJob => false
          | SomeJob(j) => j.state == JPending and not(j.parked) and j.matInfraCount == 1
        },
        budgetSoundAll,
      })
  }

  /// A3/067 (the owner-signed Q5 reversal): a pure-establishment
  /// crash-loop parks at the budget — the job joins the stalled
  /// population instead of re-listing forever.
  run establishmentParkRun = {
    init
      .then(all { recordWantedRelation("b1", "d1", Set("o1")), walkUnchanged })
      .then(all { createJob("d1", OProbe, "b1"), walkUnchanged })
      .then(all { claimJob("d1", "r1"), walkUnchanged })
      .then(all { establishCrashedAttempt("d1"), walkUnchanged })
      .expect(match jobs.get("d1") {
        | NoJob => false
        | SomeJob(j) => j.state == JPending and not(j.parked) and j.matInfraCount == 1
      })
      .then(all { claimJob("d1", "r1"), walkUnchanged })
      .then(all { establishCrashedAttempt("d1"), walkUnchanged })
      .expect(and {
        match jobs.get("d1") {
          // MAT_BUDGET = 2 in every regime: the second establishment
          // charge parks, party-blind (zero worker reports).
          | NoJob => false
          | SomeJob(j) => j.state == JPending and j.parked and j.matInfraCount == 2
        },
        budgetSoundAll,
      })
  }

  run reprobeResetRun = {
    init
      .then(all { recordWantedRelation("b1", "d1", Set("o1")), walkUnchanged })
      .then(all { durableRelationChange("d1", RVouched), walkUnchanged })
      .then(all { createJob("d1", OReprobe, "b1"), walkUnchanged })
      .expect(and {
        nodeStatus.get("d1") == NQueued,
        match jobs.get("d1") {
          | NoJob => false
          | SomeJob(j) => j.origin == OReprobe
        },
      })
  }

  /// Delta 2a: failover stales every executor channel; the claim is
  /// blocked until the redial; after the redial the claim lands (the
  /// post-failover liveness the B3 fix restored).
  run failoverRedialRun = {
    init
      .then(all { recordWantedRelation("b1", "d1", Set("o1")), walkUnchanged })
      .then(all { createJob("d1", OProbe, "b1"), walkUnchanged })
      .then(all { failover, walkUnchanged })
      .expect(and {
        channelStale.get("r1"),
        view.get("d1") == VPending(false),
      })
      .then(all { redialChannel("r1"), walkUnchanged })
      .then(all { claimJob("d1", "r1"), walkUnchanged })
      .expect(and {
        claimAfterFailover,
        match attempts.get("d1") {
          | NoAttempt => false
          | OpenAttempt(a) => a.instance == "r1"
        },
      })
  }

  /// Delta 5: the view survives failover faithfully — a parked job
  /// stays parked (NotYetReady) across the rebuild; a claimed job
  /// re-delivers to its holder.
  run failoverViewRebuildRun = {
    init
      .then(all { recordWantedRelation("b1", "d1", Set("o1")), walkUnchanged })
      .then(all { createJob("d1", OProbe, "b1"), walkUnchanged })
      .then(all { claimJob("d1", "r1"), walkUnchanged })
      .then(all { reportInfra("d1"), walkUnchanged })
      .then(all { claimJob("d1", "r1"), walkUnchanged })
      .then(all { reportInfra("d1"), walkUnchanged })
      .expect(view.get("d1") == VPending(true))
      .then(all { failover, walkUnchanged })
      .expect(and {
        view.get("d1") == VPending(true),
        unresolvedJobAlwaysArmed,
      })
  }
}

// ===========================================================================
// REGIME MODULES (the §9.1 regime vocabulary, mirroring closureEvidence so
// the §9.3 calibration transfer reads 1:1). The design-scale modules are
// the §9.1 budget (2 builds x 2 drvs); the reduced "Ex" modules are the
// exhaustive-TLC scopes (the closure-evidence reduction-ladder precedent:
// 1 drv keeps every per-drv lifecycle interleaving — no §9.1 property
// couples two drvs — while the 2-build interest dynamics stay).
// Measured budgets per scope live in the C-prime stage record.
// ===========================================================================

/// Base regime: single tenure, no faults, no GC; one legacy arrival
/// (delta 2b non-vacuity).
module materializationJobBase {
  import materializationJob(
    BUILDS = Set("b1", "b2"),
    DRVS = Set("d1", "d2"),
    OUTPUTS = Set("o1", "o2"),
    REPLICAS = Set("r1", "r2"),
    BUILDERS = Set("w1"),
    MAT_BUDGET = 2,
    MAX_EXECS = 4,
    MAX_GEN = 0,
    MAX_UPSTREAM_FLIPS = 2,
    MAX_LEGACY = 1,
    ENABLE_FAILOVER = false,
    ENABLE_GC = false,
    ENABLE_CRASH = false,
    ENABLE_STALE_TENURE = false,
    ENABLE_RESOLVE_FAULTS = false,
    TENANTS = Set("tA", "tB"),
    ENABLE_WALK_FOLD = false,
    ENABLE_WORKER_ABORTED = false
  ).*
}

/// Reduced exhaustive base scope (the ladder): one drv, two builds,
/// two outputs, two replicas.
module materializationJobBaseEx {
  import materializationJob(
    BUILDS = Set("b1", "b2"),
    DRVS = Set("d1"),
    OUTPUTS = Set("o1", "o2"),
    REPLICAS = Set("r1", "r2"),
    BUILDERS = Set("w1"),
    MAT_BUDGET = 2,
    MAX_EXECS = 4,
    MAX_GEN = 0,
    MAX_UPSTREAM_FLIPS = 2,
    MAX_LEGACY = 1,
    ENABLE_FAILOVER = false,
    ENABLE_GC = false,
    ENABLE_CRASH = false,
    ENABLE_STALE_TENURE = false,
    ENABLE_RESOLVE_FAULTS = false,
    TENANTS = Set("tA", "tB"),
    ENABLE_WALK_FOLD = false,
    ENABLE_WORKER_ABORTED = false
  ).*
}

/// Failover regime: leader changes mid-lifecycle; the view rebuild and
/// the channel staling/redial (deltas 2a/5) are the contended surface.
module materializationJobFailover {
  import materializationJob(
    BUILDS = Set("b1", "b2"),
    DRVS = Set("d1", "d2"),
    OUTPUTS = Set("o1", "o2"),
    REPLICAS = Set("r1", "r2"),
    BUILDERS = Set("w1"),
    MAT_BUDGET = 2,
    MAX_EXECS = 4,
    MAX_GEN = 2,
    MAX_UPSTREAM_FLIPS = 2,
    MAX_LEGACY = 0,
    ENABLE_FAILOVER = true,
    ENABLE_GC = false,
    ENABLE_CRASH = false,
    ENABLE_STALE_TENURE = false,
    ENABLE_RESOLVE_FAULTS = false,
    TENANTS = Set("tA", "tB"),
    ENABLE_WALK_FOLD = false,
    ENABLE_WORKER_ABORTED = false
  ).*
}

/// Reduced exhaustive failover scope.
module materializationJobFailoverEx {
  import materializationJob(
    BUILDS = Set("b1", "b2"),
    DRVS = Set("d1"),
    OUTPUTS = Set("o1", "o2"),
    REPLICAS = Set("r1", "r2"),
    BUILDERS = Set("w1"),
    MAT_BUDGET = 2,
    MAX_EXECS = 4,
    MAX_GEN = 2,
    MAX_UPSTREAM_FLIPS = 2,
    MAX_LEGACY = 0,
    ENABLE_FAILOVER = true,
    ENABLE_GC = false,
    ENABLE_CRASH = false,
    ENABLE_STALE_TENURE = false,
    ENABLE_RESOLVE_FAULTS = false,
    TENANTS = Set("tA", "tB"),
    ENABLE_WALK_FOLD = false,
    ENABLE_WORKER_ABORTED = false
  ).*
}

/// Adversarial-store regime: GC races pins; upstream availability
/// drifts under the executor; the §5.3 release lifecycle is live.
module materializationJobAdversarialStore {
  import materializationJob(
    BUILDS = Set("b1", "b2"),
    DRVS = Set("d1", "d2"),
    OUTPUTS = Set("o1", "o2"),
    REPLICAS = Set("r1", "r2"),
    BUILDERS = Set("w1"),
    MAT_BUDGET = 2,
    MAX_EXECS = 4,
    MAX_GEN = 0,
    MAX_UPSTREAM_FLIPS = 4,
    MAX_LEGACY = 0,
    ENABLE_FAILOVER = false,
    ENABLE_GC = true,
    ENABLE_CRASH = false,
    ENABLE_STALE_TENURE = false,
    ENABLE_RESOLVE_FAULTS = false,
    TENANTS = Set("tA", "tB"),
    ENABLE_WALK_FOLD = false,
    ENABLE_WORKER_ABORTED = false
  ).*
}

/// Reduced exhaustive adversarial-store scope.
module materializationJobAdversarialStoreEx {
  import materializationJob(
    BUILDS = Set("b1", "b2"),
    DRVS = Set("d1"),
    OUTPUTS = Set("o1", "o2"),
    REPLICAS = Set("r1", "r2"),
    BUILDERS = Set("w1"),
    MAT_BUDGET = 2,
    MAX_EXECS = 4,
    MAX_GEN = 0,
    MAX_UPSTREAM_FLIPS = 4,
    MAX_LEGACY = 0,
    ENABLE_FAILOVER = false,
    ENABLE_GC = true,
    ENABLE_CRASH = false,
    ENABLE_STALE_TENURE = false,
    ENABLE_RESOLVE_FAULTS = false,
    TENANTS = Set("tA", "tB"),
    ENABLE_WALK_FOLD = false,
    ENABLE_WORKER_ABORTED = false
  ).*
}

/// Stale-tenure regime: writes from previous lease generations arrive
/// and must be discarded (fencedJobWritesOnly, F11 extended to job
/// writes).
module materializationJobStaleTenure {
  import materializationJob(
    BUILDS = Set("b1", "b2"),
    DRVS = Set("d1", "d2"),
    OUTPUTS = Set("o1", "o2"),
    REPLICAS = Set("r1", "r2"),
    BUILDERS = Set("w1"),
    MAT_BUDGET = 2,
    MAX_EXECS = 4,
    MAX_GEN = 2,
    MAX_UPSTREAM_FLIPS = 2,
    MAX_LEGACY = 0,
    ENABLE_FAILOVER = true,
    ENABLE_GC = false,
    ENABLE_CRASH = false,
    ENABLE_STALE_TENURE = true,
    ENABLE_RESOLVE_FAULTS = false,
    TENANTS = Set("tA", "tB"),
    ENABLE_WALK_FOLD = false,
    ENABLE_WORKER_ABORTED = false
  ).*
}

/// Reduced exhaustive stale-tenure scope.
module materializationJobStaleTenureEx {
  import materializationJob(
    BUILDS = Set("b1", "b2"),
    DRVS = Set("d1"),
    OUTPUTS = Set("o1", "o2"),
    REPLICAS = Set("r1", "r2"),
    BUILDERS = Set("w1"),
    MAT_BUDGET = 2,
    MAX_EXECS = 4,
    MAX_GEN = 2,
    MAX_UPSTREAM_FLIPS = 2,
    MAX_LEGACY = 0,
    ENABLE_FAILOVER = true,
    ENABLE_GC = false,
    ENABLE_CRASH = false,
    ENABLE_STALE_TENURE = true,
    ENABLE_RESOLVE_FAULTS = false,
    TENANTS = Set("tA", "tB"),
    ENABLE_WALK_FOLD = false,
    ENABLE_WORKER_ABORTED = false
  ).*
}

/// Crash-loop regime (the PP-4 channel): repeated establishment of
/// crashed attempts of BOTH kinds — the partition invariants' and the
/// charge-once invariant's contended regime. Post-Q5-reversal the
/// establishment PARKS at the budget (party-blind), so pure
/// crash-loops enter the stalled population and re-arm only through
/// the backoff expiry — the loop is bounded by the park cycle plus
/// the ledger ceilings.
module materializationJobCrashLoop {
  import materializationJob(
    BUILDS = Set("b1", "b2"),
    DRVS = Set("d1", "d2"),
    OUTPUTS = Set("o1", "o2"),
    REPLICAS = Set("r1", "r2"),
    BUILDERS = Set("w1"),
    MAT_BUDGET = 2,
    MAX_EXECS = 6,
    MAX_GEN = 0,
    MAX_UPSTREAM_FLIPS = 2,
    MAX_LEGACY = 0,
    ENABLE_FAILOVER = false,
    ENABLE_GC = false,
    ENABLE_CRASH = true,
    ENABLE_STALE_TENURE = false,
    ENABLE_RESOLVE_FAULTS = false,
    TENANTS = Set("tA", "tB"),
    ENABLE_WALK_FOLD = false,
    ENABLE_WORKER_ABORTED = false
  ).*
}

/// Reduced exhaustive crash-loop scope.
module materializationJobCrashLoopEx {
  import materializationJob(
    BUILDS = Set("b1", "b2"),
    DRVS = Set("d1"),
    OUTPUTS = Set("o1", "o2"),
    REPLICAS = Set("r1", "r2"),
    BUILDERS = Set("w1"),
    MAT_BUDGET = 2,
    MAX_EXECS = 6,
    MAX_GEN = 0,
    MAX_UPSTREAM_FLIPS = 2,
    MAX_LEGACY = 0,
    ENABLE_FAILOVER = false,
    ENABLE_GC = false,
    ENABLE_CRASH = true,
    ENABLE_STALE_TENURE = false,
    ENABLE_RESOLVE_FAULTS = false,
    TENANTS = Set("tA", "tB"),
    ENABLE_WALK_FOLD = false,
    ENABLE_WORKER_ABORTED = false
  ).*
}

/// A1 resolve-faults regime (bughunt wave): the durable-resolve fault
/// arms are live (fenced/errored resolves interleave with every
/// consumption and cancellation), failover disabled so
/// viewMatchesDurableUnresolved binds unconditionally. The two A1
/// invariants join the conjunction here and in every other regime
/// (where the fault arms are simply disabled).
module materializationJobResolveFaults {
  import materializationJob(
    BUILDS = Set("b1", "b2"),
    DRVS = Set("d1", "d2"),
    OUTPUTS = Set("o1", "o2"),
    REPLICAS = Set("r1", "r2"),
    BUILDERS = Set("w1"),
    MAT_BUDGET = 2,
    MAX_EXECS = 4,
    MAX_GEN = 0,
    MAX_UPSTREAM_FLIPS = 2,
    MAX_LEGACY = 1,
    ENABLE_FAILOVER = false,
    ENABLE_GC = false,
    ENABLE_CRASH = true,
    ENABLE_STALE_TENURE = false,
    ENABLE_RESOLVE_FAULTS = true,
    TENANTS = Set("tA", "tB"),
    ENABLE_WALK_FOLD = false,
    ENABLE_WORKER_ABORTED = false
  ).*
}

/// B1 bounded-await-transport (merged_bug_189 / owner Q3): the
/// worker-abort regime — the base alphabet plus the SIGTERM-aborted
/// charge-free close. Holds the full conjunction including
/// abortNeverCharges; the falsifiability pair is the
/// mat-189-abort-charges calibration (the pre-fix Err->infra flatten)
/// and the colliding-identity admission pin is
/// mat-158-colliding-identity (atMostOneClaimWinner).
module materializationJobWorkerAbort {
  import materializationJob(
    BUILDS = Set("b1", "b2"),
    DRVS = Set("d1", "d2"),
    OUTPUTS = Set("o1", "o2"),
    REPLICAS = Set("r1", "r2"),
    BUILDERS = Set("w1"),
    MAT_BUDGET = 2,
    MAX_EXECS = 4,
    MAX_GEN = 0,
    MAX_UPSTREAM_FLIPS = 2,
    MAX_LEGACY = 1,
    ENABLE_FAILOVER = false,
    ENABLE_GC = false,
    ENABLE_CRASH = false,
    ENABLE_STALE_TENURE = false,
    ENABLE_RESOLVE_FAULTS = false,
    TENANTS = Set("tA", "tB"),
    ENABLE_WALK_FOLD = false,
    ENABLE_WORKER_ABORTED = true
  ).*
}

/// Slot-10 walk-fold regime: the base scope with the plane enabled —
/// the four kernel-fold laws' live home (holds + runs + the
/// reachability witness; the four mat-* walk calibrations bind the
/// same scope with one perturbed decision).
module materializationJobWalkFold {
  import materializationJob(
    BUILDS = Set("b1", "b2"),
    DRVS = Set("d1"),
    OUTPUTS = Set("o1", "o2"),
    REPLICAS = Set("r1", "r2"),
    BUILDERS = Set("w1"),
    MAT_BUDGET = 2,
    MAX_EXECS = 4,
    MAX_GEN = 0,
    MAX_UPSTREAM_FLIPS = 2,
    MAX_LEGACY = 0,
    ENABLE_FAILOVER = false,
    ENABLE_GC = false,
    ENABLE_CRASH = false,
    ENABLE_STALE_TENURE = false,
    ENABLE_RESOLVE_FAULTS = false,
    TENANTS = Set("tA", "tB"),
    ENABLE_WALK_FOLD = true,
    ENABLE_WORKER_ABORTED = false
  ).*
}
