// -*- mode: Bluespec; -*-
//
// DRAFT (substitution-replacement Phase A deliverable, T-5.3).
//
// Wired as CI checks at Phase C-prime — the design §8 go/no-go gate.
// Until then this file MUST typecheck and its `run` smoke MUST execute,
// but NO property here is claimed verified: the properties are the §9.1
// successor table drafted as named predicates so Phase C-prime completes
// and verifies them against this skeleton instead of designing them from
// scratch. Draft-stage property violations found by simulation are
// expected and are NOT defects of the shipped system (the shipped Phase-A
// code is dormant; this model describes the flag-on end-state).
//
// WHAT THIS MODELS (design §2, §9.1): the store-owned materialization-job
// lifecycle that replaces the scheduler-side substitution walk —
//
//   - job creation at the scheduler's classification sites (durable rows,
//     fenced writes, one unresolved job per derivation);
//   - the durable per-(build, derivation) wanted relation (§6) and the
//     interest DERIVED from it by a live-build join (never registered
//     separately — finding AS-1);
//   - store-replica claim through the pull protocol (kind=MATERIALIZATION,
//     per-replica executor identity, the admission arm as the one-winner
//     arbiter — finding AS-3);
//   - in-store execution: per-path ingest + pin-at-ingest, tenant-scoped
//     (finding AS-4);
//   - outcome reporting (Success / Unobtainable / InfraFailure) and the
//     consumption transactions: the Success coverage check (the CE-17
//     class) and the four-arm Unobtainable routing (the C3-settlement
//     successor — findings AS-2/PP-1/BC-5);
//   - the establishment sweep for crashed replicas (materialization_infra,
//     never executor_crash — OQ1 amendment 1 / finding PP-4), the
//     charge-free cancellation closer (finding BC-2), obsolete-on-produced;
//   - the materialization budget with park-not-fail exhaustion (§2.5) and
//     the housekeeping park re-evaluation;
//   - the build-side environment that makes the anchor properties
//     non-vacuous (finding BC-6): builder pulls (NotYetReady while a job
//     is unresolved), from-source dispatch, build terminality;
//   - leader failover (jobs/attempts/relations are durable; the in-memory
//     view is droppable), store GC vs pins, upstream availability drift.
//
// WHAT IS NOT MODELED (deliberate draft scope; Phase C-prime revisits)
//   - Multiple outputs per derivation beyond the OUTPUTS constant and
//     per-output reference closures: the closure walk is abstracted to
//     "ingest the wanted set path by path" — closure incompleteness
//     surfaces as a wanted path still missing.
//   - The probe's advisory verdict at creation (§2.1/OQ4): creation is
//     enabled whenever wanted outputs are missing locally; the probe's
//     wrong-answer cost (one wasted round trip) is priced by the routing
//     arms, not by a creation guard.
//   - Byte progress, gateway events, metrics (display-only, droppable).
//   - The backoff clock: parking and re-claim eligibility are modeled as
//     states, not timers (the park re-evaluation action covers both the
//     backoff expiry and the housekeeping sweep).
//   - Per-path singleflight and concurrent ingest of the same path by two
//     jobs (the store's per-(tenant,path) singleflight is assumed).
//
// THE BOUNDED REPRESENTATION (the SC-1 no-unbounded-variables rule): the
// kind-partitioned ledger suffix is carried as bounded per-derivation
// counters (matInfra / matUnobtainable / buildCrash), exactly as
// retryPolicy.qnt carries the build ledger as its fold; exec ids and
// lease generations are ceilinged consts; every counter's ceiling is an
// action-precondition guard.
//
// PHASE C-PRIME COMPLETION CONTRACT (what "wired and green" requires):
//   1. Every property below either HOLDS exhaustively (TLC) in every
//      regime it names, or is re-encoded with the violation documented
//      and adjudicated (the §9.3 calibration-transfer discipline).
//   2. Every property has at least one expect-violation witness proving
//      its contended state reachable (the non-vacuity discipline of
//      .claude/rules/quint.md), wired via mkQuintWitnessCheck.
//   3. The §9.3 calibration-transfer table gets a verdict per family
//      (closure-evidence F1-F14, C1-C5, the executor-campaign rows).
//   4. The regimes below are each the --main of at least one wired check
//      in nix/quint.nix.
//   5. The budget: per-check minutes-class (the state space is dominated
//      by 2 builds x 2 drvs x job lifecycle — well under the
//      closureEvidence slice that needed the reduction ladder).
//
// Budget note (§9.1): 2 builds x 2 derivations; one job record per
// derivation; 2 store replicas; 2 outputs per derivation.
module materializationJob {
  // --- 1. types -----------------------------------------------------------

  /// Job lifecycle states (§2.4, §6). Parked is NOT a state: it is a flag
  /// on a pending job (state stays pending; the §2.5 park).
  type JobState =
    | JPending
    | JClaimed
    | JResolvedSuccess
    | JResolvedFromSource
    | JResolvedUnobtainable
    | JObsolete
    | JCancelled

  /// Where the job came from (§2.1's creation sites).
  type JobOrigin = | OProbe | OMerge | OPruned | OStaleReset

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

  /// Node (derivation) status — the §2.3 table's status machine with
  /// Substituting deleted (adjudication OQ6).
  type NodeStatus =
    | NQueued
    | NReady
    | NRunning
    | NCompleted
    | NFailed
    | NDepFailed

  /// Attempt kind (drv_executions.attempt_kind / dispatch_mode).
  type AttemptKind = | KBuild | KMaterialization

  /// The open attempt on a derivation (closed attempts live on as ledger
  /// counters). instance is the per-replica executor identity for
  /// materialization attempts (finding BC-1) and the builder-pod intent
  /// identity for build attempts.
  type Attempt = {
    kind: AttemptKind,
    instance: str,
    execId: int,
  }
  type AttemptOpt = | NoAttempt | OpenAttempt(Attempt)

  /// Pin state for one (derivation, output) store path (§5): unpinned, or
  /// pinned with a kind (build_input / materialization + the owning job's
  /// resolution identity is abstracted away — the release rule reads the
  /// interest join, not the id).
  type PinKind = | NoPin | PinBuildInput | PinMaterialization

  /// The durable declared-relation classification the routing arms read
  /// (§2.4 arms 1/2: Vouched = declared closure produced, Pending = deps
  /// declared but not all produced, Broken = neither).
  type DurableRelation = | RVouched | RPending | RBroken

  // --- 2. constants (const = overridable per regime) -----------------------

  /// Builds (the interest universe). §9.1 budget: 2.
  const BUILDS: Set[str]
  /// Derivations. §9.1 budget: 2.
  const DRVS: Set[str]
  /// Output names per derivation (every drv declares the same names at
  /// this scale; the wanted relation selects subsets).
  const OUTPUTS: Set[str]
  /// Store replicas (materialization executors).
  const REPLICAS: Set[str]
  /// Builder-pod identities (build-kind pulls; kindMatchesWorker's
  /// non-vacuity needs at least one).
  const BUILDERS: Set[str]
  /// The materialization budget (max_materialization_attempts).
  const MAT_BUDGET: int
  /// Ceiling on exec ids minted (every claim/dispatch consumes one).
  const MAX_EXECS: int
  /// Ceiling on lease generations (failovers).
  const MAX_GEN: int
  /// Ceiling on upstream-availability flips (keeps the environment
  /// finite).
  const MAX_UPSTREAM_FLIPS: int
  /// Regime switches.
  const ENABLE_FAILOVER: bool
  const ENABLE_GC: bool
  const ENABLE_CRASH: bool
  const ENABLE_STALE_TENURE: bool

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

  // --- 4. state variables ----------------------------------------------------

  /// One job record per derivation (atMostOneUnresolvedJobPerDrv is the
  /// partial-unique index; the model carries it structurally as a single
  /// slot whose creation action requires the slot resolved-or-empty).
  var jobs: str -> JobOpt

  /// The durable per-(build, derivation) wanted relation (§6): which
  /// output names each live build wants from each derivation. Empty set =
  /// no relation row.
  var wanted: (str, str) -> Set[str]

  /// The open attempt per derivation (at most one; the §2.2 arbiter).
  var attempts: str -> AttemptOpt

  /// Node status per derivation.
  var nodeStatus: str -> NodeStatus

  /// Store presence per (derivation, output).
  var present: (str, str) -> bool

  /// Pin state per (derivation, output) (§5).
  var pins: (str, str) -> PinKind

  /// The kind-partitioned ledger suffix, carried as bounded counters
  /// (the SC-1 rule): materialization-kind charges per drv...
  var matInfraLedger: str -> int
  var matUnobtainableLedger: str -> int
  /// ...and build-kind charges per drv (the partition's other side;
  /// materializationInvisibleToBuildBudgets reads this).
  var buildCrashLedger: str -> int

  /// The lease generation (fencing token). Job/relation writes carry it;
  /// stale writes are discards (fencedJobWritesOnly).
  var leaseGen: int
  /// GHOST: a stale-generation write was attempted and discarded (the
  /// stale-tenure regime's probe).
  var staleWriteDiscarded: bool

  /// Build liveness (the interest join's left side).
  var buildLive: str -> bool

  /// Upstream availability per (derivation, output) — the world the
  /// executor probes.
  var upstreamAvail: (str, str) -> bool
  var upstreamFlipsLeft: int

  /// The durable declared-relation classification per derivation (the
  /// routing arms' input; driven by the environment, abstracting the
  /// build DAG's progress).
  var durableRel: str -> DurableRelation

  /// Exec-id mint counter (bounded).
  var execsMinted: int

  /// GHOST: per-derivation count of fail-fast verdicts delivered
  /// (wrongfulFailFastBoundedPerJob).
  var failFastCount: str -> int

  /// GHOST: the last consumption's routing took the fail-fast arm only
  /// after a re-probe confirmed a live-wanted path missing or the
  /// one-shot was spent (noWrongfulTerminalFailure's evidence).
  var lastFailFastJustified: bool

  // --- 5. derived state -------------------------------------------------------

  /// Derived interest (finding AS-1): the live builds holding a non-empty
  /// wanted relation for d. Never registered separately.
  def interest(d: str): Set[str] =
    BUILDS.filter(b => buildLive.get(b) and wanted.get((b, d)) != Set())

  /// The live effective wanted set for d (the §6 join): the union of
  /// live interested builds' wanted output names.
  def liveWanted(d: str): Set[str] =
    OUTPUTS.filter(o => BUILDS.exists(b => and {
      buildLive.get(b),
      wanted.get((b, d)).contains(o),
    }))

  /// Every live-wanted output of d is present in the store.
  def liveWantedCovered(d: str): bool =
    liveWanted(d).forall(o => present.get((d, o)))

  /// d has an unresolved materialization job.
  def hasUnresolvedJob(d: str): bool = jobUnresolved(jobs.get(d))

  // --- 6. properties (the §9.1 successor table, drafted) ---------------------
  // Each carries the §3 rows it succeeds. DRAFT: encodings are first
  // approximations for Phase C-prime to complete; none is claimed
  // verified at Phase A.

  /// A1/A11 (+ the F8/F13 anchor): a node never runs a from-source build
  /// attempt while its materialization job is unresolved.
  val noFromSourceWhileJobUnresolved = DRVS.forall(d =>
    hasUnresolvedJob(d) implies
      match attempts.get(d) {
        | NoAttempt => true
        | OpenAttempt(a) => a.kind != KBuild
      }
  )

  /// A3: a resolved job's verdict is sound — resolved_success means the
  /// live wanted set (at resolution) was covered; the model checks the
  /// weaker always-form "resolved_success nodes are Completed".
  val jobResolutionSound = DRVS.forall(d =>
    match jobs.get(d) {
      | NoJob => true
      | SomeJob(j) => j.state == JResolvedSuccess implies
          nodeStatus.get(d) == NCompleted
    }
  )

  /// A8/C3: from-source routing requires the durable relation to say
  /// Vouched or Pending at the routing decision (never from a Broken
  /// declared closure without a fail-fast).
  val routingRequiresDurableVouchOrFailFast = DRVS.forall(d =>
    match jobs.get(d) {
      | NoJob => true
      | SomeJob(j) => j.state == JResolvedFromSource implies
          (durableRel.get(d) == RVouched or durableRel.get(d) == RPending)
    }
  )

  /// L1/L2/settlement: an unresolved job always has an armed action —
  /// pending and unparked (claimable), claimed (attempt open, report or
  /// establishment will arrive), or parked (the housekeeping
  /// re-evaluation owns it). Drafted structurally: an unresolved job is
  /// pending or claimed (park keeps state pending).
  val unresolvedJobAlwaysArmed = DRVS.forall(d =>
    hasUnresolvedJob(d) implies
      match jobs.get(d) {
        | NoJob => false
        | SomeJob(j) => or {
            j.state == JPending,
            j.state == JClaimed and attempts.get(d) != NoAttempt,
          }
      }
  )

  /// C3 (the wired-check headline): a node is never failed while every
  /// live-wanted output is actually obtainable (present, or available
  /// upstream) — the wrongful-failure class.
  val noWrongfulTerminalFailure = DRVS.forall(d =>
    nodeStatus.get(d) == NFailed implies
      not(liveWanted(d).forall(o => or {
        present.get((d, o)),
        upstreamAvail.get((d, o)),
      }))
  )

  /// C2/B3: from-source resolution never happens while the live wanted
  /// set is fully present (that would be a wasted build) — the inverse
  /// permissiveness bound. DRAFT: weak form.
  val noWrongfulFromSourceRouting = DRVS.forall(d =>
    match jobs.get(d) {
      | NoJob => true
      | SomeJob(j) => (j.state == JResolvedFromSource and j.resolutionExec > 0) implies
          not(liveWantedCovered(d))
    }
  )

  /// B1/B4/A12 (the CE-17 class): success consumption covers the live
  /// wanted set re-read at consumption.
  val successConsumptionCoversLiveWanted = DRVS.forall(d =>
    match jobs.get(d) {
      | NoJob => true
      | SomeJob(j) => j.state == JResolvedSuccess implies liveWantedCovered(d)
    }
  )

  /// C5/B4: interest is derived from live builds only — a dead build
  /// contributes nothing to any wanted set.
  val interestUnionLiveOnly = DRVS.forall(d =>
    liveWanted(d).forall(o => BUILDS.exists(b => and {
      buildLive.get(b),
      wanted.get((b, d)).contains(o),
    }))
  )

  /// B2-strong (finding PP-2): every output a materialization job
  /// ingested stays pinned until every interested build is terminal.
  val pinCoversIngestUntilAllInterestTerminal = DRVS.forall(d =>
    OUTPUTS.forall(o =>
      (pins.get((d, o)) == PinMaterialization and interest(d) != Set()) implies
        present.get((d, o))
    )
  )

  /// A10: failover preserves jobs (they are durable rows; the in-memory
  /// view is droppable). Drafted as: jobs never silently disappear —
  /// every derivation that had a job before a failover has the same job
  /// after it. (The draft encodes the stronger structural form: failover
  /// does not write jobs at all; C-prime refines with a pre/post ghost.)
  val failoverPreservesJobs = true

  /// A17/A18: every job write is fenced — a stale-generation write is a
  /// discard, never an apply. The stale-tenure regime drives
  /// staleWriteDiscarded; the property says discards never mutate state
  /// (drafted via the ghost: a discard happened and the job slot it
  /// targeted is unchanged — structural in this draft).
  val fencedJobWritesOnly = true

  /// §3.6: the attempt kind matches the worker class — materialization
  /// attempts carry store-replica identities, build attempts carry
  /// builder identities (non-vacuous via builderPull).
  val kindMatchesWorker = DRVS.forall(d =>
    match attempts.get(d) {
      | NoAttempt => true
      | OpenAttempt(a) => and {
          a.kind == KMaterialization implies REPLICAS.contains(a.instance),
          a.kind == KBuild implies BUILDERS.contains(a.instance),
        }
    }
  )

  /// §2.5/§3.5: no materialization-kind charge sequence ever produces a
  /// node failure attributable to it — the budget exhaustion parks the
  /// job, never fails the node. (The retryPolicy.qnt twin is the wired
  /// Phase-A form; this one covers the job-lifecycle side.)
  val materializationNeverPoisons = DRVS.forall(d =>
    (matInfraLedger.get(d) > 0 or matUnobtainableLedger.get(d) > 0) implies
      (nodeStatus.get(d) == NFailed implies failFastCount.get(d) > 0)
  )

  /// §2.5/§3.5: materialization-kind ledger rows are invisible to the
  /// build-kind ledger — the partition is structural (separate counters);
  /// the invariant pins that no action couples them (drafted as: build
  /// crash charges only come from build-kind establishments, which the
  /// crash-loop regime exercises).
  val materializationInvisibleToBuildBudgets = DRVS.forall(d =>
    (matInfraLedger.get(d) + matUnobtainableLedger.get(d) > 0 and buildCrashLedger.get(d) == 0) implies
      match attempts.get(d) {
        | NoAttempt => true
        | OpenAttempt(a) => a.kind == KMaterialization implies buildCrashLedger.get(d) == 0
      }
  )

  /// The dedup / C3-class guard: at most one unresolved job per
  /// derivation (the partial-unique index; structural in this model — the
  /// job slot is a single record — kept as a property so the C-prime
  /// completion can widen the state to a job multiset and re-check).
  val atMostOneUnresolvedJobPerDrv = true

  /// The §2.2 arbiter (finding AS-3): at most one open materialization
  /// attempt per derivation, and a second claim by a different replica is
  /// refused (encoded structurally: the attempt slot is single; the claim
  /// action's guard refuses on an open attempt with a different
  /// instance).
  val atMostOneClaimWinner = DRVS.forall(d =>
    match attempts.get(d) {
      | NoAttempt => true
      | OpenAttempt(a) => a.kind == KMaterialization implies REPLICAS.contains(a.instance)
    }
  )

  /// C1 + C1-strict: fail-fast verdicts per job are bounded (at most one
  /// re-probe-driven re-arm, then at most one fail-fast), and every
  /// fail-fast was justified by a confirmed-missing re-probe or a spent
  /// one-shot.
  val wrongfulFailFastBoundedPerJob = DRVS.forall(d => and {
    failFastCount.get(d) <= 1,
    failFastCount.get(d) == 1 implies lastFailFastJustified,
  })

  // --- 6b. draft witnesses (non-vacuity probes; wired at C-prime) ------------

  /// Violated when a job resolves successfully (the happy path is
  /// reachable).
  val noSuccessResolution = DRVS.forall(d =>
    match jobs.get(d) { | NoJob => true | SomeJob(j) => j.state != JResolvedSuccess }
  )
  /// Violated when the four-arm routing fail-fasts (the arm-3 terminal is
  /// reachable).
  val noFailFast = DRVS.forall(d => failFastCount.get(d) == 0)
  /// Violated when a job parks on budget exhaustion.
  val noPark = DRVS.forall(d =>
    match jobs.get(d) { | NoJob => true | SomeJob(j) => not(j.parked) }
  )
  /// Violated when a crashed replica's attempt is established.
  val noCrashEstablishment = DRVS.forall(d => matInfraLedger.get(d) == 0)
  /// Violated when a build-kind attempt opens (the build environment is
  /// live — kindMatchesWorker non-vacuity).
  val noBuildAttempt = DRVS.forall(d =>
    match attempts.get(d) {
      | NoAttempt => true
      | OpenAttempt(a) => a.kind != KBuild
    }
  )

  // --- 7. actions -------------------------------------------------------------

  /// §2.1: a scheduler classification site creates a job for d (probe /
  /// merge / prune / stale-Completed origins; reprobe-lane origins reset
  /// the node status to its dep-derived value in the same transaction —
  /// modeled as Queued). Creation requires: some live-wanted output
  /// missing locally, no unresolved job already (the partial-unique
  /// upsert), and the node not terminal.
  action createJob(d: str, origin: JobOrigin, tenant: str): bool = all {
    liveWanted(d) != Set(),
    not(liveWantedCovered(d)),
    not(hasUnresolvedJob(d)),
    nodeStatus.get(d) != NCompleted,
    nodeStatus.get(d) != NFailed,
    BUILDS.contains(tenant),
    buildLive.get(tenant),
    jobs' = jobs.put(d, SomeJob({ ...freshJob, origin: origin, tenant: tenant })),
    // reprobe-lane origins (stale reset) recompute the status in the same
    // transaction (finding AS-5); other origins leave it.
    nodeStatus' = if (origin == OStaleReset)
      nodeStatus.put(d, NQueued)
    else nodeStatus,
    wanted' = wanted, attempts' = attempts, present' = present, pins' = pins,
    matInfraLedger' = matInfraLedger, matUnobtainableLedger' = matUnobtainableLedger,
    buildCrashLedger' = buildCrashLedger, leaseGen' = leaseGen,
    staleWriteDiscarded' = staleWriteDiscarded, buildLive' = buildLive,
    upstreamAvail' = upstreamAvail, upstreamFlipsLeft' = upstreamFlipsLeft,
    durableRel' = durableRel, execsMinted' = execsMinted,
    failFastCount' = failFastCount, lastFailFastJustified' = lastFailFastJustified,
  }

  /// §6: a build's merge writes its wanted relation rows for d (every
  /// (build, node) pair, regardless of routing — finding AS-1).
  action recordWantedRelation(b: str, d: str, outs: Set[str]): bool = all {
    buildLive.get(b),
    outs != Set(),
    outs.subseteq(OUTPUTS),
    wanted.get((b, d)) == Set(),
    wanted' = wanted.put((b, d), outs),
    jobs' = jobs, attempts' = attempts, nodeStatus' = nodeStatus,
    present' = present, pins' = pins,
    matInfraLedger' = matInfraLedger, matUnobtainableLedger' = matUnobtainableLedger,
    buildCrashLedger' = buildCrashLedger, leaseGen' = leaseGen,
    staleWriteDiscarded' = staleWriteDiscarded, buildLive' = buildLive,
    upstreamAvail' = upstreamAvail, upstreamFlipsLeft' = upstreamFlipsLeft,
    durableRel' = durableRel, execsMinted' = execsMinted,
    failFastCount' = failFastCount, lastFailFastJustified' = lastFailFastJustified,
  }

  /// §2.2/§2.3: a store replica claims d's pending job — the admission
  /// arm (the one-winner arbiter under the actor's serialized loop,
  /// finding AS-3) + the fenced mint. Legal from Queued or Ready;
  /// refused (not enabled) when an attempt by a different identity is
  /// open or the job is parked.
  action claimJob(d: str, replica: str): bool = all {
    REPLICAS.contains(replica),
    execsMinted < MAX_EXECS,
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
      nodeStatus' = nodeStatus.put(d, NRunning),
      execsMinted' = newExec,
    },
    wanted' = wanted, present' = present, pins' = pins,
    matInfraLedger' = matInfraLedger, matUnobtainableLedger' = matUnobtainableLedger,
    buildCrashLedger' = buildCrashLedger, leaseGen' = leaseGen,
    staleWriteDiscarded' = staleWriteDiscarded, buildLive' = buildLive,
    upstreamAvail' = upstreamAvail, upstreamFlipsLeft' = upstreamFlipsLeft,
    durableRel' = durableRel,
    failFastCount' = failFastCount, lastFailFastJustified' = lastFailFastJustified,
  }

  /// §2.2 item 3 / §5: the claiming replica ingests one upstream-available
  /// live-wanted path and pins it at ingest (tenant-scoped; the tenant
  /// re-resolution is abstracted — the job's tenant is live or any live
  /// interested build's).
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
    jobs' = jobs, wanted' = wanted, attempts' = attempts, nodeStatus' = nodeStatus,
    matInfraLedger' = matInfraLedger, matUnobtainableLedger' = matUnobtainableLedger,
    buildCrashLedger' = buildCrashLedger, leaseGen' = leaseGen,
    staleWriteDiscarded' = staleWriteDiscarded, buildLive' = buildLive,
    upstreamAvail' = upstreamAvail, upstreamFlipsLeft' = upstreamFlipsLeft,
    durableRel' = durableRel, execsMinted' = execsMinted,
    failFastCount' = failFastCount, lastFailFastJustified' = lastFailFastJustified,
  }

  /// §2.4: the executor's Success report + the consumption transaction's
  /// coverage check, collapsed to one action at this draft's resolution
  /// (the report-then-consume split is C-prime work; the COVERAGE CHECK
  /// is the load-bearing part and is modeled): if the live wanted set
  /// re-read at consumption is covered, the node completes and the job
  /// resolves; else (interest grew) the job re-arms.
  action consumeSuccess(d: str): bool = all {
    match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) => a.kind == KMaterialization
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
      nodeStatus' = if (covered) nodeStatus.put(d, NCompleted) else nodeStatus.put(d, NQueued),
      attempts' = attempts.put(d, NoAttempt),
    },
    wanted' = wanted, present' = present, pins' = pins,
    matInfraLedger' = matInfraLedger, matUnobtainableLedger' = matUnobtainableLedger,
    buildCrashLedger' = buildCrashLedger, leaseGen' = leaseGen,
    staleWriteDiscarded' = staleWriteDiscarded, buildLive' = buildLive,
    upstreamAvail' = upstreamAvail, upstreamFlipsLeft' = upstreamFlipsLeft,
    durableRel' = durableRel, execsMinted' = execsMinted,
    failFastCount' = failFastCount, lastFailFastJustified' = lastFailFastJustified,
  }

  /// §2.4: the executor's Unobtainable report + the four-arm consumption
  /// routing, collapsed to one action (the routing core is what the
  /// properties constrain):
  ///   arm 0 (moot): no live-wanted path is missing -> success-for-live-
  ///     interest or re-arm; never fail-fast;
  ///   arm 1/2 (durable Vouched/Pending): resolved_from_source;
  ///   arm 3: re-probe (the upstreamAvail map is the probe oracle);
  ///     all obtainable -> re-arm once (the one-shot), else fail-fast.
  action consumeUnobtainable(d: str, missing: Set[str]): bool = all {
    missing != Set(),
    missing.subseteq(OUTPUTS),
    match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) => a.kind == KMaterialization
    },
    match jobs.get(d) { | NoJob => false | SomeJob(j) => j.state == JClaimed },
    // the report's missing paths really are missing-and-unavailable
    missing.forall(o => not(present.get((d, o))) and not(upstreamAvail.get((d, o)))),
    matUnobtainableLedger.get(d) < MAT_BUDGET + 2,
    val w = liveWanted(d)
    val missingLive = missing.intersect(w)
    val execId = match attempts.get(d) { | NoAttempt => 0 | OpenAttempt(a) => a.execId }
    val oneShotSpent = match jobs.get(d) { | NoJob => false | SomeJob(j) => j.matUnobtainableCount > 0 }
    // arm selection
    val arm0 = missingLive == Set()
    val arm12 = not(arm0) and (durableRel.get(d) == RVouched or durableRel.get(d) == RPending)
    val reprobeAllObtainable = w.forall(o => present.get((d, o)) or upstreamAvail.get((d, o)))
    val arm3rearm = not(arm0) and not(arm12) and reprobeAllObtainable and not(oneShotSpent)
    val arm3fail = not(arm0) and not(arm12) and not(arm3rearm)
    all {
      jobs' = jobs.put(d, match jobs.get(d) {
        | NoJob => NoJob
        | SomeJob(j) => SomeJob({ ...j,
            state: if (arm0 and liveWantedCovered(d)) JResolvedSuccess
              else if (arm0) JPending
              else if (arm12) JResolvedFromSource
              else if (arm3rearm) JPending
              else JResolvedUnobtainable,
            resolutionExec: if (arm0 and liveWantedCovered(d)) execId
              else if (arm12 or arm3fail) execId
              else j.resolutionExec,
            matUnobtainableCount: j.matUnobtainableCount + 1 })
      }),
      nodeStatus' = if (arm0 and liveWantedCovered(d))
        nodeStatus.put(d, NCompleted)
      else if (arm3fail)
        nodeStatus.put(d, NFailed)
      else if (arm12)
        nodeStatus.put(d, NReady)
      else
        nodeStatus.put(d, NQueued),
      attempts' = attempts.put(d, NoAttempt),
      matUnobtainableLedger' = matUnobtainableLedger.put(d, matUnobtainableLedger.get(d) + 1),
      failFastCount' = if (arm3fail) failFastCount.put(d, failFastCount.get(d) + 1) else failFastCount,
      lastFailFastJustified' = if (arm3fail) and {
        // the fail-fast is justified: a live-wanted path is confirmed
        // missing at the re-probe, or the one-shot was spent
        or { not(reprobeAllObtainable), oneShotSpent }
      } else lastFailFastJustified,
    },
    wanted' = wanted, present' = present, pins' = pins,
    matInfraLedger' = matInfraLedger,
    buildCrashLedger' = buildCrashLedger, leaseGen' = leaseGen,
    staleWriteDiscarded' = staleWriteDiscarded, buildLive' = buildLive,
    upstreamAvail' = upstreamAvail, upstreamFlipsLeft' = upstreamFlipsLeft,
    durableRel' = durableRel, execsMinted' = execsMinted,
  }

  /// §2.5: the executor's InfraFailure report — the job re-arms within
  /// the materialization budget (the charge); never routes from-source,
  /// never fail-fasts (B3).
  action reportInfra(d: str): bool = all {
    match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) => a.kind == KMaterialization
    },
    match jobs.get(d) {
      | NoJob => false
      | SomeJob(j) => j.state == JClaimed and j.matInfraCount < MAT_BUDGET
    },
    jobs' = jobs.put(d, match jobs.get(d) {
      | NoJob => NoJob
      | SomeJob(j) => SomeJob({ ...j, state: JPending, matInfraCount: j.matInfraCount + 1 })
    }),
    attempts' = attempts.put(d, NoAttempt),
    nodeStatus' = nodeStatus.put(d, NQueued),
    matInfraLedger' = matInfraLedger.put(d, matInfraLedger.get(d) + 1),
    wanted' = wanted, present' = present, pins' = pins,
    matUnobtainableLedger' = matUnobtainableLedger,
    buildCrashLedger' = buildCrashLedger, leaseGen' = leaseGen,
    staleWriteDiscarded' = staleWriteDiscarded, buildLive' = buildLive,
    upstreamAvail' = upstreamAvail, upstreamFlipsLeft' = upstreamFlipsLeft,
    durableRel' = durableRel, execsMinted' = execsMinted,
    failFastCount' = failFastCount, lastFailFastJustified' = lastFailFastJustified,
  }

  /// §2.4 establishment (findings BC-2/BC-3/PP-4, OQ1 amendment 1): a
  /// crashed worker's open attempt is established — BOTH kinds modeled so
  /// the partition invariants are non-vacuous. Materialization kind: the
  /// charge is materialization_infra (no adopt arm); build kind: the
  /// charge is executor_crash into the build ledger.
  action establishCrashedAttempt(d: str): bool = all {
    ENABLE_CRASH,
    attempts.get(d) != NoAttempt,
    match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) =>
        if (a.kind == KMaterialization)
          // materialization arm: infra charge within budget
          match jobs.get(d) { | NoJob => false | SomeJob(j) => j.matInfraCount < MAT_BUDGET }
        else
          // build arm: crash-charge ceiling
          buildCrashLedger.get(d) < MAT_BUDGET
    },
    val isMat = match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) => a.kind == KMaterialization
    }
    all {
      attempts' = attempts.put(d, NoAttempt),
      jobs' = if (isMat) jobs.put(d, match jobs.get(d) {
        | NoJob => NoJob
        | SomeJob(j) => SomeJob({ ...j, state: JPending, matInfraCount: j.matInfraCount + 1 })
      }) else jobs,
      nodeStatus' = nodeStatus.put(d, NQueued),
      matInfraLedger' = if (isMat) matInfraLedger.put(d, matInfraLedger.get(d) + 1) else matInfraLedger,
      buildCrashLedger' = if (isMat) buildCrashLedger else buildCrashLedger.put(d, buildCrashLedger.get(d) + 1),
    },
    wanted' = wanted, present' = present, pins' = pins,
    matUnobtainableLedger' = matUnobtainableLedger, leaseGen' = leaseGen,
    staleWriteDiscarded' = staleWriteDiscarded, buildLive' = buildLive,
    upstreamAvail' = upstreamAvail, upstreamFlipsLeft' = upstreamFlipsLeft,
    durableRel' = durableRel, execsMinted' = execsMinted,
    failFastCount' = failFastCount, lastFailFastJustified' = lastFailFastJustified,
  }

  /// §2.4 / finding BC-2: zero live interest cancels the job and closes
  /// any open attempt charge-free in the same transaction.
  action cancelOnZeroInterest(d: str): bool = all {
    hasUnresolvedJob(d),
    interest(d) == Set(),
    jobs' = jobs.put(d, match jobs.get(d) {
      | NoJob => NoJob
      | SomeJob(j) => SomeJob({ ...j, state: JCancelled })
    }),
    attempts' = attempts.put(d, NoAttempt),
    // charge-free: no ledger counter moves
    nodeStatus' = nodeStatus,
    jobsUnchangedRest,
  }

  /// §2.4: the node was Produced by other means (deps built it from
  /// source, another tenant's build): the report's consumption is a no-op
  /// and the job goes obsolete.
  action obsoleteOnProduced(d: str): bool = all {
    hasUnresolvedJob(d),
    nodeStatus.get(d) == NCompleted,
    jobs' = jobs.put(d, match jobs.get(d) {
      | NoJob => NoJob
      | SomeJob(j) => SomeJob({ ...j, state: JObsolete })
    }),
    attempts' = attempts.put(d, NoAttempt),
    nodeStatus' = nodeStatus,
    jobsUnchangedRest,
  }

  /// §2.5: budget exhaustion parks the job (state stays pending; the park
  /// flag arms the housekeeping re-evaluation). Never a failure.
  action parkOnBudgetExhaustion(d: str): bool = all {
    match jobs.get(d) {
      | NoJob => false
      | SomeJob(j) => and {
          j.state == JPending,
          not(j.parked),
          j.matInfraCount >= MAT_BUDGET,
        }
    },
    jobs' = jobs.put(d, match jobs.get(d) {
      | NoJob => NoJob
      | SomeJob(j) => SomeJob({ ...j, parked: true })
    }),
    attempts' = attempts, nodeStatus' = nodeStatus,
    jobsUnchangedRest,
  }

  /// §2.5: the housekeeping sweep re-evaluates a parked job — if the
  /// durable relation has become Vouched/Pending (deps produced by other
  /// builds), the job resolves from-source; else it un-parks for another
  /// claim cycle (the backoff-expiry re-claim).
  action housekeepingReevaluatePark(d: str): bool = all {
    match jobs.get(d) {
      | NoJob => false
      | SomeJob(j) => j.state == JPending and j.parked
    },
    val vouchedOrPending = durableRel.get(d) == RVouched or durableRel.get(d) == RPending
    jobs' = jobs.put(d, match jobs.get(d) {
      | NoJob => NoJob
      | SomeJob(j) =>
        if (vouchedOrPending)
          SomeJob({ ...j, state: JResolvedFromSource, parked: false })
        else
          SomeJob({ ...j, parked: false, matInfraCount: 0 })
    }),
    attempts' = attempts,
    nodeStatus' = if (durableRel.get(d) == RVouched or durableRel.get(d) == RPending)
      nodeStatus.put(d, NReady) else nodeStatus,
    jobsUnchangedRest,
  }

  /// A build reaches a terminal state (completes or is cancelled): it
  /// leaves the live set; its wanted rows drop out of the interest join.
  action buildTerminal(b: str): bool = all {
    buildLive.get(b),
    buildLive' = buildLive.put(b, false),
    jobs' = jobs, attempts' = attempts, nodeStatus' = nodeStatus,
    wanted' = wanted, present' = present, pins' = pins,
    matInfraLedger' = matInfraLedger, matUnobtainableLedger' = matUnobtainableLedger,
    buildCrashLedger' = buildCrashLedger, leaseGen' = leaseGen,
    staleWriteDiscarded' = staleWriteDiscarded,
    upstreamAvail' = upstreamAvail, upstreamFlipsLeft' = upstreamFlipsLeft,
    durableRel' = durableRel, execsMinted' = execsMinted,
    failFastCount' = failFastCount, lastFailFastJustified' = lastFailFastJustified,
  }

  /// §2.3 (finding BC-6): a builder pull for d — the BUILD column of the
  /// kind table. NotYetReady while a materialization job is unresolved
  /// (the must-substitute refusal's successor — the action is disabled);
  /// DeliverNew when the job is resolved from_source (or no job exists)
  /// and the node is Ready.
  action builderPull(d: str, builder: str): bool = all {
    BUILDERS.contains(builder),
    execsMinted < MAX_EXECS,
    not(hasUnresolvedJob(d)),
    nodeStatus.get(d) == NReady,
    attempts.get(d) == NoAttempt,
    val newExec = execsMinted + 1
    all {
      attempts' = attempts.put(d, OpenAttempt({ kind: KBuild, instance: builder, execId: newExec })),
      nodeStatus' = nodeStatus.put(d, NRunning),
      execsMinted' = newExec,
    },
    jobs' = jobs, wanted' = wanted, present' = present, pins' = pins,
    matInfraLedger' = matInfraLedger, matUnobtainableLedger' = matUnobtainableLedger,
    buildCrashLedger' = buildCrashLedger, leaseGen' = leaseGen,
    staleWriteDiscarded' = staleWriteDiscarded, buildLive' = buildLive,
    upstreamAvail' = upstreamAvail, upstreamFlipsLeft' = upstreamFlipsLeft,
    durableRel' = durableRel,
    failFastCount' = failFastCount, lastFailFastJustified' = lastFailFastJustified,
  }

  /// The from-source dispatch completes (the build attempt produces the
  /// outputs): the node completes, outputs become present with build
  /// pins.
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
    jobs' = jobs, wanted' = wanted,
    matInfraLedger' = matInfraLedger, matUnobtainableLedger' = matUnobtainableLedger,
    buildCrashLedger' = buildCrashLedger, leaseGen' = leaseGen,
    staleWriteDiscarded' = staleWriteDiscarded, buildLive' = buildLive,
    upstreamAvail' = upstreamAvail, upstreamFlipsLeft' = upstreamFlipsLeft,
    durableRel' = durableRel, execsMinted' = execsMinted,
    failFastCount' = failFastCount, lastFailFastJustified' = lastFailFastJustified,
  }

  /// Leader failover: the lease generation bumps; jobs, attempts,
  /// relations, ledger counters and pins are durable and survive
  /// untouched (failoverPreservesJobs); the in-memory job view (not
  /// modeled separately — it is a cache) is dropped and rebuilt.
  action failover = all {
    ENABLE_FAILOVER,
    leaseGen < MAX_GEN,
    leaseGen' = leaseGen + 1,
    jobs' = jobs, attempts' = attempts, nodeStatus' = nodeStatus,
    wanted' = wanted, present' = present, pins' = pins,
    matInfraLedger' = matInfraLedger, matUnobtainableLedger' = matUnobtainableLedger,
    buildCrashLedger' = buildCrashLedger,
    staleWriteDiscarded' = staleWriteDiscarded, buildLive' = buildLive,
    upstreamAvail' = upstreamAvail, upstreamFlipsLeft' = upstreamFlipsLeft,
    durableRel' = durableRel, execsMinted' = execsMinted,
    failFastCount' = failFastCount, lastFailFastJustified' = lastFailFastJustified,
  }

  /// A write from a previous lease generation arrives (stale tenure): it
  /// is discarded — no state changes except the ghost recording that the
  /// discard happened (fencedJobWritesOnly's probe).
  action staleTenureWriteDiscarded = all {
    ENABLE_STALE_TENURE,
    leaseGen > 0,
    not(staleWriteDiscarded),
    staleWriteDiscarded' = true,
    jobs' = jobs, attempts' = attempts, nodeStatus' = nodeStatus,
    wanted' = wanted, present' = present, pins' = pins,
    matInfraLedger' = matInfraLedger, matUnobtainableLedger' = matUnobtainableLedger,
    buildCrashLedger' = buildCrashLedger, leaseGen' = leaseGen,
    buildLive' = buildLive,
    upstreamAvail' = upstreamAvail, upstreamFlipsLeft' = upstreamFlipsLeft,
    durableRel' = durableRel, execsMinted' = execsMinted,
    failFastCount' = failFastCount, lastFailFastJustified' = lastFailFastJustified,
  }

  /// Store GC visits a (derivation, output) path: deletion is blocked by
  /// pins of either kind (§5); an unpinned, un-wanted path can be
  /// collected.
  action storeGc(d: str, o: str): bool = all {
    ENABLE_GC,
    OUTPUTS.contains(o),
    present.get((d, o)),
    pins.get((d, o)) == NoPin,
    present' = present.put((d, o), false),
    jobs' = jobs, attempts' = attempts, nodeStatus' = nodeStatus,
    wanted' = wanted, pins' = pins,
    matInfraLedger' = matInfraLedger, matUnobtainableLedger' = matUnobtainableLedger,
    buildCrashLedger' = buildCrashLedger, leaseGen' = leaseGen,
    staleWriteDiscarded' = staleWriteDiscarded, buildLive' = buildLive,
    upstreamAvail' = upstreamAvail, upstreamFlipsLeft' = upstreamFlipsLeft,
    durableRel' = durableRel, execsMinted' = execsMinted,
    failFastCount' = failFastCount, lastFailFastJustified' = lastFailFastJustified,
  }

  /// The upstream world changes: one (derivation, output)'s availability
  /// flips (bounded by MAX_UPSTREAM_FLIPS).
  action upstreamChange(d: str, o: str): bool = all {
    upstreamFlipsLeft > 0,
    OUTPUTS.contains(o),
    upstreamAvail' = upstreamAvail.put((d, o), not(upstreamAvail.get((d, o)))),
    upstreamFlipsLeft' = upstreamFlipsLeft - 1,
    jobs' = jobs, attempts' = attempts, nodeStatus' = nodeStatus,
    wanted' = wanted, present' = present, pins' = pins,
    matInfraLedger' = matInfraLedger, matUnobtainableLedger' = matUnobtainableLedger,
    buildCrashLedger' = buildCrashLedger, leaseGen' = leaseGen,
    staleWriteDiscarded' = staleWriteDiscarded, buildLive' = buildLive,
    durableRel' = durableRel, execsMinted' = execsMinted,
    failFastCount' = failFastCount, lastFailFastJustified' = lastFailFastJustified,
  }

  /// The build DAG's durable declared-relation classification for d
  /// changes (deps produced by other builds, children declared): the
  /// environment input the routing arms 1/2 and the park re-evaluation
  /// read.
  action durableRelationChange(d: str, r: DurableRelation): bool = all {
    durableRel.get(d) != r,
    durableRel' = durableRel.put(d, r),
    jobs' = jobs, attempts' = attempts, nodeStatus' = nodeStatus,
    wanted' = wanted, present' = present, pins' = pins,
    matInfraLedger' = matInfraLedger, matUnobtainableLedger' = matUnobtainableLedger,
    buildCrashLedger' = buildCrashLedger, leaseGen' = leaseGen,
    staleWriteDiscarded' = staleWriteDiscarded, buildLive' = buildLive,
    upstreamAvail' = upstreamAvail, upstreamFlipsLeft' = upstreamFlipsLeft,
    execsMinted' = execsMinted,
    failFastCount' = failFastCount, lastFailFastJustified' = lastFailFastJustified,
  }

  // --- 7b. shared frame helper ------------------------------------------------

  /// The variables the small job-lifecycle actions never touch (frame
  /// helper — keeps those actions readable; every action still assigns
  /// every variable exactly once through this).
  action jobsUnchangedRest = all {
    wanted' = wanted, present' = present, pins' = pins,
    matInfraLedger' = matInfraLedger, matUnobtainableLedger' = matUnobtainableLedger,
    buildCrashLedger' = buildCrashLedger, leaseGen' = leaseGen,
    staleWriteDiscarded' = staleWriteDiscarded, buildLive' = buildLive,
    upstreamAvail' = upstreamAvail, upstreamFlipsLeft' = upstreamFlipsLeft,
    durableRel' = durableRel, execsMinted' = execsMinted,
    failFastCount' = failFastCount, lastFailFastJustified' = lastFailFastJustified,
  }

  // --- 8. init ------------------------------------------------------------------

  action init = all {
    jobs' = DRVS.mapBy(d => NoJob),
    wanted' = tuples(BUILDS, DRVS).mapBy(p => Set()),
    attempts' = DRVS.mapBy(d => NoAttempt),
    nodeStatus' = DRVS.mapBy(d => NQueued),
    present' = tuples(DRVS, OUTPUTS).mapBy(p => false),
    pins' = tuples(DRVS, OUTPUTS).mapBy(p => NoPin),
    matInfraLedger' = DRVS.mapBy(d => 0),
    matUnobtainableLedger' = DRVS.mapBy(d => 0),
    buildCrashLedger' = DRVS.mapBy(d => 0),
    leaseGen' = 0,
    staleWriteDiscarded' = false,
    buildLive' = BUILDS.mapBy(b => true),
    upstreamAvail' = tuples(DRVS, OUTPUTS).mapBy(p => true),
    upstreamFlipsLeft' = MAX_UPSTREAM_FLIPS,
    durableRel' = DRVS.mapBy(d => RBroken),
    execsMinted' = 0,
    failFastCount' = DRVS.mapBy(d => 0),
    lastFailFastJustified' = true,
  }

  // --- 9. step --------------------------------------------------------------------

  action step = {
    nondet d = DRVS.oneOf()
    nondet b = BUILDS.oneOf()
    nondet o = OUTPUTS.oneOf()
    nondet replica = REPLICAS.oneOf()
    nondet builder = BUILDERS.oneOf()
    nondet origin = Set(OProbe, OMerge, OPruned, OStaleReset).oneOf()
    nondet outs = OUTPUTS.powerset().oneOf()
    nondet missing = OUTPUTS.powerset().oneOf()
    nondet rel = Set(RVouched, RPending, RBroken).oneOf()
    any {
      recordWantedRelation(b, d, outs),
      createJob(d, origin, b),
      claimJob(d, replica),
      executeStep(d, o),
      consumeSuccess(d),
      consumeUnobtainable(d, missing),
      reportInfra(d),
      establishCrashedAttempt(d),
      cancelOnZeroInterest(d),
      obsoleteOnProduced(d),
      parkOnBudgetExhaustion(d),
      housekeepingReevaluatePark(d),
      buildTerminal(b),
      builderPull(d, builder),
      dispatchFromSource(d),
      failover,
      staleTenureWriteDiscarded,
      storeGc(d, o),
      upstreamChange(d, o),
      durableRelationChange(d, rel),
    }
  }

  // --- 10. runs (the smoke scenario) -----------------------------------------------

  /// The §9.1 happy path: a build records its wanted relation, a job is
  /// created at the probe gate, a store replica claims it, ingests the
  /// wanted output, and the success consumption completes the node and
  /// resolves the job. Proves the action skeleton composes.
  run happyPathTest = {
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
      })
  }

  /// The park path: infra failures exhaust the budget, the job parks
  /// (never fails the node), and the housekeeping re-evaluation un-parks
  /// it for another cycle.
  run parkPathTest = {
    init
      .then(recordWantedRelation("b1", "d1", Set("o1")))
      .then(createJob("d1", OProbe, "b1"))
      .then(claimJob("d1", "r1"))
      .then(reportInfra("d1"))
      .then(claimJob("d1", "r1"))
      .then(reportInfra("d1"))
      .then(parkOnBudgetExhaustion("d1"))
      .expect(and {
        match jobs.get("d1") {
          | NoJob => false
          | SomeJob(j) => j.parked and j.state == JPending
        },
        // park is not failure: the node is not failed
        nodeStatus.get("d1") != NFailed,
      })
      .then(housekeepingReevaluatePark("d1"))
      .expect(match jobs.get("d1") {
        | NoJob => false
        | SomeJob(j) => not(j.parked)
      })
  }
}

// ===========================================================================
// REGIME MODULES (the §9.1 regime vocabulary, mirroring closureEvidence so
// the §9.3 calibration transfer reads 1:1 at Phase C-prime). DRAFT: none is
// wired as a CI check yet; Phase C-prime wires each as the --main of at
// least one check.
// ===========================================================================

/// Base regime: single tenure, no faults, no GC. The §9.1 budget
/// (2 builds x 2 drvs, 2 replicas, 2 outputs).
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
    ENABLE_FAILOVER = false,
    ENABLE_GC = false,
    ENABLE_CRASH = false,
    ENABLE_STALE_TENURE = false
  ).*
}

/// Failover regime: leader changes mid-lifecycle; jobs/attempts/relations
/// are durable and must survive (failoverPreservesJobs, A10).
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
    ENABLE_FAILOVER = true,
    ENABLE_GC = false,
    ENABLE_CRASH = false,
    ENABLE_STALE_TENURE = false
  ).*
}

/// Adversarial-store regime: GC races pins (pinCoversIngest...), upstream
/// availability drifts under the executor.
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
    ENABLE_FAILOVER = false,
    ENABLE_GC = true,
    ENABLE_CRASH = false,
    ENABLE_STALE_TENURE = false
  ).*
}

/// Stale-tenure regime: writes from previous lease generations arrive and
/// must be discarded (fencedJobWritesOnly, A17/A18).
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
    ENABLE_FAILOVER = true,
    ENABLE_GC = false,
    ENABLE_CRASH = false,
    ENABLE_STALE_TENURE = true
  ).*
}

/// Crash-loop regime (the PP-4 channel): repeated establishment of
/// crashed attempts of BOTH kinds — the partition invariants' contended
/// regime (materialization crashes charge materialization_infra and park;
/// build crashes charge executor_crash; neither leaks into the other).
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
    ENABLE_FAILOVER = false,
    ENABLE_GC = false,
    ENABLE_CRASH = true,
    ENABLE_STALE_TENURE = false
  ).*
}
