// -*- mode: Bluespec; -*-
//
// Substitution-replacement model (Phase C-prime, re-targeted to the
// post-deletion system at Phase D-prime T-D7.2): the store-owned
// materialization-job lifecycle AS BUILT at the D-prime tip — the ONLY
// substitution mechanism (the walk machinery, the Substituting status,
// the coexistence flag and both evidence columns were deleted in Waves
// D3-D6; migration 080). The six Phase-B behavior deltas are encoded
// here (the stage record's "C-prime handoff" delta list,
// substitution-replacement-invariant-map.md), with the D-prime
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
// substitution-replacement-invariant-map.md's C-prime stage record;
// the override modules are docs/spec/models/calibration/mat-*.qnt.
// Measured state counts / depths / wall-clocks live in the
// introducing commits and the check transcripts, never here.
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
  type DurableRelation = | RVouched | RPending | RBroken

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
  /// is the partial-unique index); the dedup-removed override latches it.
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
  /// built (the fence discards); the unfenced-resolve override latches.
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
  /// override targets the VIEW instead, and a hypothetical
  /// row-dropping override would latch this.
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
  /// wanted union (the C5/B4 successor).
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
  /// forced-Ready requeue reaches the violation.
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
      durableRel.get(d) == RBroken,
    })

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
      durableRel.get(d) == RBroken,
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
  /// Delta 3: a Broken-evidence parked job (the stalled population) is
  /// reachable — the MD-D1 alert constrains a real state.
  val noStalledBrokenPark = stalledJobs == Set()
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
  }
  /// Job-core vars unchanged.
  action jobCoreUnchanged = all {
    jobs' = jobs, dupJob' = dupJob, view' = view, attempts' = attempts,
    nodeStatus' = nodeStatus, topdownPruned' = topdownPruned,
    execsMinted' = execsMinted,
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
    wanted.get((b, d)) != outs,
    wanted' = wanted.put((b, d), outs),
    lastWritten' = lastWritten.put((b, d), outs),
    legacyInterest' = legacyInterest.put((b, d), false),
    legacyBudget' = legacyBudget,
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
    tenureUnchanged, ghostsUnchanged,
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
      dupJob' = dupJob,
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
    tenureUnchanged, ghostsUnchanged,
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
    tenureUnchanged, ghostsUnchanged,
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
    tenureUnchanged, ghostsUnchanged,
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
    val covered = liveWantedCovered(d)
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
      // CE-17 latch: coverage held at the decision (covered arm), and
      // the decision ranged over the live-only union (structural).
      covAtResolveAll' = and { covAtResolveAll, covered implies covered },
      covLiveOnlyAll' = covLiveOnlyAll,
    },
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
  action consumeUnobtainable(d: str, missing: Set[str]): bool = all {
    missing != Set(),
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
    val arm0covered = arm0 and liveWantedCovered(d)
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
        arm3fs implies (durableRel.get(d) == RBroken and not(marked)),
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
    },
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
    },
    attempts' = attempts.put(d, NoAttempt),
    nodeStatus' = nodeStatus.put(d, NQueued),
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
    // window reached the budget).
    failFastCount' = failFastCount,
    ffJustifiedAll' = ffJustifiedAll, fsJustifiedAll' = fsJustifiedAll,
    fsReportSoundAll' = fsReportSoundAll, covAtResolveAll' = covAtResolveAll,
    covLiveOnlyAll' = covLiveOnlyAll,
    creationResolvableAll' = creationResolvableAll,
    chargedAfterCancel' = chargedAfterCancel,
    claimOrigin' = claimOrigin,
    requeueReadyJustifiedAll' = requeueReadyJustifiedAll,
    carried' = carried,
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
    tenureUnchanged, ghostsUnchanged,
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
    tenureUnchanged, ghostsUnchanged,
  }

  action resolveErr(d: str): bool = all {
    ENABLE_RESOLVE_FAULTS,
    hasUnresolvedJob(d),
    jobCoreUnchanged,
    relUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
    tenureUnchanged, ghostsUnchanged,
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
    tenureUnchanged, ghostsUnchanged,
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
    durableRel.get(d) == RVouched or durableRel.get(d) == RPending,
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
      durableRel.get(d) == RVouched or durableRel.get(d) == RPending,
    },
    fsReportSoundAll' = fsReportSoundAll, covAtResolveAll' = covAtResolveAll,
    covLiveOnlyAll' = covLiveOnlyAll,
    creationResolvableAll' = creationResolvableAll,
    chargedAfterCancel' = chargedAfterCancel,
    claimOrigin' = claimOrigin,
    requeueReadyJustifiedAll' = requeueReadyJustifiedAll,
    carried' = carried,
    budgetSoundAll' = budgetSoundAll,
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
    tenureUnchanged, ghostsUnchanged,
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
    tenureUnchanged, ghostsUnchanged,
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
    tenureUnchanged, ghostsUnchanged,
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
    ghostsUnchanged,
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
    tenureUnchanged, ghostsUnchanged,
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
    // the pre/post ghost: durable rows are untouched (the frame); a
    // row-dropping override would compute and latch a difference here.
    failoverDroppedJobs' = failoverDroppedJobs,
    jobs' = jobs, dupJob' = dupJob, attempts' = attempts,
    nodeStatus' = nodeStatus, topdownPruned' = topdownPruned,
    execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
    staleWriteDiscarded' = staleWriteDiscarded,
    staleJobWriteApplied' = staleJobWriteApplied,
    claimAfterFailover' = claimAfterFailover,
    ghostsUnchanged,
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
    envUnchanged, ghostsUnchanged,
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
    staleWriteDiscarded' = true,
    leaseGen' = leaseGen, staleJobWriteApplied' = staleJobWriteApplied,
    channelStale' = channelStale, failedOver' = failedOver,
    claimAfterFailover' = claimAfterFailover,
    failoverDroppedJobs' = failoverDroppedJobs,
    jobCoreUnchanged, relUnchanged, storeUnchanged, ledgerUnchanged,
    envUnchanged, ghostsUnchanged,
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
    tenureUnchanged, ghostsUnchanged,
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
    tenureUnchanged, ghostsUnchanged,
  }

  /// The upstream world changes (bounded).
  action upstreamChange(d: str, o: str): bool = all {
    upstreamFlipsLeft > 0,
    OUTPUTS.contains(o),
    upstreamAvail' = upstreamAvail.put((d, o), not(upstreamAvail.get((d, o)))),
    upstreamFlipsLeft' = upstreamFlipsLeft - 1,
    buildLive' = buildLive, durableRel' = durableRel,
    jobCoreUnchanged, relUnchanged, storeUnchanged, ledgerUnchanged,
    tenureUnchanged, ghostsUnchanged,
  }

  /// The durable declared-relation classification changes (deps
  /// produced by other builds, children declared).
  action durableRelationChange(d: str, r: DurableRelation): bool = all {
    durableRel.get(d) != r,
    durableRel' = durableRel.put(d, r),
    buildLive' = buildLive, upstreamAvail' = upstreamAvail,
    upstreamFlipsLeft' = upstreamFlipsLeft,
    jobCoreUnchanged, relUnchanged, storeUnchanged, ledgerUnchanged,
    tenureUnchanged, ghostsUnchanged,
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
    channelStale' = REPLICAS.mapBy(r => false),
    failedOver' = false,
    claimAfterFailover' = false,
    failoverDroppedJobs' = false,
    buildLive' = BUILDS.mapBy(b => true),
    upstreamAvail' = tuples(DRVS, OUTPUTS).mapBy(p => true),
    upstreamFlipsLeft' = MAX_UPSTREAM_FLIPS,
    durableRel' = DRVS.mapBy(d => RBroken),
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
    nondet rel = Set(RVouched, RPending, RBroken).oneOf()
    any {
      recordWantedRelation(b, d, outs),
      legacyBuildArrives(b, d),
      createJob(d, origin, b),
      dedupRefeed(d),
      dedupPrunedUpgrade(d, b),
      claimJob(d, replica),
      executeStep(d, o),
      consumeSuccess(d),
      consumeUnobtainable(d, missing),
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
    }
  }

  // --- 11. runs (deterministic scenario pins) -----------------------------------

  /// The §9.1 happy path: relation, probe-origin job, claim, ingest,
  /// success consumption -> resolved_success, node Completed, pinned.
  run happyPathRun = {
    init
      .then(recordWantedRelation("b1", "d1", Set("o1")))
      .then(createJob("d1", OProbe, "b1"))
      .then(claimJob("d1", "r1"))
      .then(executeStep("d1", "o1"))
      .then(consumeSuccess("d1"))
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

  /// Delta 3: infra failures park IN the consumption at the budget
  /// (no separate park transition); the Broken-evidence park is the
  /// stalled population; the backoff expiry un-parks WITHOUT resetting
  /// the count; the re-evaluation resolves a Vouched park from source.
  run parkSplitRun = {
    init
      .then(recordWantedRelation("b1", "d1", Set("o1")))
      .then(createJob("d1", OProbe, "b1"))
      .then(claimJob("d1", "r1"))
      .then(reportInfra("d1"))
      .then(claimJob("d1", "r1"))
      .then(reportInfra("d1"))
      .expect(and {
        match jobs.get("d1") {
          | NoJob => false
          | SomeJob(j) => j.parked and j.state == JPending and j.matInfraCount == 2
        },
        nodeStatus.get("d1") != NFailed,
        stalledJobs == Set("d1"),
        view.get("d1") == VPending(true),
      })
      .then(parkBackoffExpires("d1"))
      .expect(match jobs.get("d1") {
        | NoJob => false
        | SomeJob(j) => not(j.parked) and j.matInfraCount == 2
      })
      .then(claimJob("d1", "r2"))
      .then(reportInfra("d1"))
      .expect(match jobs.get("d1") {
        | NoJob => false
        | SomeJob(j) => j.parked and j.matInfraCount == 3
      })
      .then(durableRelationChange("d1", RVouched))
      .then(parkReevaluate("d1"))
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
      .then(recordWantedRelation("b1", "d1", Set("o1")))
      .then(createJob("d1", OPruned, "b1"))
      .expect(topdownPruned.get("d1"))
      .then(upstreamChange("d1", "o1"))
      .then(claimJob("d1", "r1"))
      .then(consumeUnobtainable("d1", Set("o1")))
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
      .then(recordWantedRelation("b1", "d1", Set("o1")))
      .then(createJob("d1", OProbe, "b1"))
      .then(upstreamChange("d1", "o1"))
      .then(claimJob("d1", "r1"))
      .then(consumeUnobtainable("d1", Set("o1")))
      .expect(and {
        nodeStatus.get("d1") == NReady,
        failFastCount.get("d1") == 0,
        fsJustifiedAll,
        match jobs.get("d1") {
          | NoJob => false
          | SomeJob(j) => j.state == JResolvedFromSource
        },
      })
      .then(builderPull("d1", "w1"))
      .then(dispatchFromSource("d1"))
      .expect(nodeStatus.get("d1") == NCompleted)
  }

  /// Delta 6 / B1: a MARKED (pruned-origin) Ready node with an
  /// unparked pending job is claimable (the B1 claim upgrade); the
  /// builder pull of the same node is blocked by the JobView arm
  /// (VPending != VNone — post-D-prime the view IS the refusal; the
  /// kernel's A11 arm is deleted).
  run markedClaimUpgradeRun = {
    init
      .then(recordWantedRelation("b1", "d1", Set("o1")))
      .then(createJob("d1", OPruned, "b1"))
      .then(claimJob("d1", "r1"))
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
      .then(legacyBuildArrives("b1", "d1"))
      .expect(liveWanted("d1") == Set())
      .then(createJob("d1", OProbe, "b1"))
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
      .then(recordWantedRelation("b1", "d1", Set("o1")))
      .then(createJob("d1", OProbe, "b1"))
      .then(claimJob("d1", "r1"))
      .then(executeStep("d1", "o1"))
      .then(consumeSuccess("d1"))
      .expect(nodeStatus.get("d1") == NCompleted)
      .then(buildTerminal("b1"))
      .then(releasePins("d1"))
      .then(storeGc("d1", "o1"))
      .expect(not(present.get(("d1", "o1"))))
      .then(recordWantedRelation("b2", "d1", Set("o1")))
      .then(createJob("d1", OStaleReset, "b2"))
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
      .then(recordWantedRelation("b1", "d1", Set("o1")))
      .then(createJob("d1", OProbe, "b1"))
      .then(claimJob("d1", "r1"))
      .then(reportInfra("d1"))
      .expect(match jobs.get("d1") {
        | NoJob => false
        | SomeJob(j) => j.state == JPending and not(j.parked) and j.matInfraCount == 1
      })
      // kill the upstream so the next report's missing path is
      // evidence-backed, then resolve job 1 from source (arm 3,
      // RBroken + unmarked).
      .then(upstreamChange("d1", "o1"))
      .then(claimJob("d1", "r1"))
      .then(consumeUnobtainable("d1", Set("o1")))
      .expect(match jobs.get("d1") {
        | NoJob => false
        | SomeJob(j) => j.state == JResolvedFromSource
      })
      // job 2: the creation reset starts a FRESH per-job window — its
      // first infra charge re-arms (count 1 < MAT_BUDGET 2), where the
      // pre-fix flat history count (already 2 by job 1) would park.
      .then(createJob("d1", OProbe, "b1"))
      .then(claimJob("d1", "r2"))
      .then(reportInfra("d1"))
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
      .then(recordWantedRelation("b1", "d1", Set("o1")))
      .then(createJob("d1", OProbe, "b1"))
      .then(claimJob("d1", "r1"))
      .then(establishCrashedAttempt("d1"))
      .expect(match jobs.get("d1") {
        | NoJob => false
        | SomeJob(j) => j.state == JPending and not(j.parked) and j.matInfraCount == 1
      })
      .then(claimJob("d1", "r1"))
      .then(establishCrashedAttempt("d1"))
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
      .then(recordWantedRelation("b1", "d1", Set("o1")))
      .then(durableRelationChange("d1", RVouched))
      .then(createJob("d1", OReprobe, "b1"))
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
      .then(recordWantedRelation("b1", "d1", Set("o1")))
      .then(createJob("d1", OProbe, "b1"))
      .then(failover)
      .expect(and {
        channelStale.get("r1"),
        view.get("d1") == VPending(false),
      })
      .then(redialChannel("r1"))
      .then(claimJob("d1", "r1"))
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
      .then(recordWantedRelation("b1", "d1", Set("o1")))
      .then(createJob("d1", OProbe, "b1"))
      .then(claimJob("d1", "r1"))
      .then(reportInfra("d1"))
      .then(claimJob("d1", "r1"))
      .then(reportInfra("d1"))
      .expect(view.get("d1") == VPending(true))
      .then(failover)
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
    ENABLE_RESOLVE_FAULTS = false
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
    ENABLE_RESOLVE_FAULTS = false
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
    ENABLE_RESOLVE_FAULTS = false
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
    ENABLE_RESOLVE_FAULTS = false
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
    ENABLE_RESOLVE_FAULTS = false
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
    ENABLE_RESOLVE_FAULTS = false
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
    ENABLE_RESOLVE_FAULTS = false
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
    ENABLE_RESOLVE_FAULTS = false
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
    ENABLE_RESOLVE_FAULTS = false
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
    ENABLE_RESOLVE_FAULTS = false
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
    ENABLE_RESOLVE_FAULTS = true
  ).*
}
