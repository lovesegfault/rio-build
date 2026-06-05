// -*- mode: Bluespec; -*-
// quint-policy-latches: viol
//
// Model S of the executor-lifecycle campaign, RE-TARGETED to the pull
// protocol as landed by Phase-1 slices 1a–1c' (the "the assignment IS
// the pod" replacement; design §4, OA6 = option (a)). The stream
// session machinery the Stage-B as-built encoding modeled —
// registration, heartbeat, the bidi dispatch stream, disconnect/reap,
// the recently_disconnected correlation — was deleted by the 1c'
// deletion commits; the as-built encoding (executorSessionAsBuilt.qnt,
// P14) was retired on 2026-05-29 — see git history and the invariant
// map's retirement record. This file is the live Model S:
// the scheduler side of `PullAssignment` / `ReportOutcome` /
// `ReportAttemptOutcome` plus the establishment sweep over the durable
// open-attempt view, and the pod's own bounded lifecycle around them.
//
// Every action cites the production function it encodes (post-deletion
// code; function names are the anchors, never line numbers). The
// invariant ↔ spec-rule linkage lives in
// docs/spec/models/executor-invariant-map.md (the Phase-1c' re-target
// record); check wiring and r[verify] markers live in nix/quint.nix,
// never here.
//
// WHAT IS MODELED
//   - One derivation (the intent) per DRVS element, with its DAG-side
//     readiness lifecycle: wanted-but-not-Ready (deps unbuilt — the
//     OA6(a) pre-attempt wait state), Ready, open (an attempt is
//     in flight), Completed, no-longer-wanted (cancelled / swept).
//   - The pod the controller spawns for that intent: born knowing its
//     derivation, it pulls (`PullAssignment`), receives exactly one of
//     Deliver / Gone / NotYetReady{retry_after} (admit_pull), retries
//     NotYetReady up to a bounded budget, idle-exits charge-free if the
//     drv never becomes deliverable, builds and reports
//     (`ReportOutcome`), or dies. A replacement pod for the same intent
//     carries the same executor identity (the attested intent id), so a
//     re-pull converges on the same open attempt instead of minting a
//     second one.
//   - The durable open-attempt row: minted only by the fenced pull
//     transaction (`mint_pull_attempt_fenced`), carrying the serving
//     generation; the worker-report first installment
//     (fold_report → handle_completion), the controller's idempotent
//     `ReportAttemptOutcome` second installment / synthesized verdict /
//     no-attempt no-op, and the establishment sweep
//     (tick_sweep_open_pull_attempts: store-probe adopt arm or the C2
//     executor-crash charge arm) — every closer of that row.
//   - The claims floor (GREATEST over assignments.generation and
//     leader_generation_claims.generation) and the generation fence
//     every authority-exercising transaction carries; leader failover
//     bumps the cluster generation and writes the new claim row before
//     serving (claim-before-serve), and a deposed believer's
//     below-floor pull/establishment writes are fenced to no-ops.
//   - Bucketed time only: the establishment window (deadline +
//     report-slack) is a per-attempt closed/not-closed bucket, the
//     NotYetReady retry budget and the pod-respawn budget are bounded
//     counters. No numeric clocks.
//
// WHAT IS NOT MODELED (deliberate scope boundaries, each priced)
//   - Budget/charge arithmetic and verdicts: every charge site appends
//     to the durable ledger and folds via decide() — that is
//     retryPolicy.qnt's subject (attemptsChargedOnce, verdictMatchesFold,
//     failoverPreservesHistory and the pull-mode environment regime
//     retryPolicyPull are imported, not re-stated). This model tracks
//     only the attempt-row dedup lifecycle those charges key on: at most
//     one open row per intent, at most one classification per attempt,
//     charge-free closes stay charge-free.
//   - The spawn decision itself (which intents get Jobs, ceilings,
//     anti-affinity rendering of the AD2 exclusion set) and the Job/
//     NodeClaim lifecycle: spawnCoherence.qnt (Model J) and
//     nodeclaimLifecycle.qnt (Model N) own them; the spawn here is an
//     environment action that may legitimately arrive before the drv is
//     Ready (the OA6 forecast-spawn case — Model J's no-ready-filter
//     publish stands per the OA6(a) decision).
//   - The builder's internal delivery choreography (the stream-era
//     Model D, executorDelivery.qnt) — retired with the 1d builder
//     collapse; the pull-mode pod's report loop is a single
//     bounded-retry unary and is folded into the report actions here
//     (the retirement record names the unit/VM carriers).
//   - The token↔intent HMAC check (admit_pull's RejectToken arm) and
//     the gRPC-layer leader guard: input hardening, pre-registered
//     NOT-ENCODED (F8 posture unchanged from the as-built model).
//   - NoEligibleSource / spawn-gate fleet exhaustion: a controller
//     verdict about the source universe, not about an attempt; it is
//     the retry model's spawnGateExhaust environment.
//   - Multi-replica in-memory state: the model state is the durable
//     state plus the current leader's view; a deposed-but-unaware
//     replica appears only through the fault-leader regime's
//     stale-generation write attempts, which the fence must make inert
//     (StaleAuthorityWritesAreInert). Dual belief itself is bounded by
//     the lease model (boundedDualLeadership), as before.
//
// ASSUME-GUARANTEE CHECKLIST (the foreign invariants this model relies
// on, named exactly as exported by their models — verified in-repo;
// re-derived against the pull-only world at 1c', T-1c'.7)
//   - leaderElection.qnt: atMostOneCASWinner (single-writer
//     acquisition), boundedDualLeadership (dual belief IS reachable in
//     degraded regimes, bounded with discovery armed),
//     staleLeaderHasStaleGeneration (concurrent believers are
//     generation-ordered — what makes the claims-floor comparison
//     meaningful). neverDual is NOT imported: the fault-leader regime
//     here keeps the stale-believer window reachable (witness
//     canReachStaleFenced). The consumer of these exports is no longer
//     the worker-side B5 latch (deleted with the stream); it is the
//     transaction-side fence carried by the pull mint, the uncharged
//     close and the establishment charge (the claims-floor GREATEST
//     read inside each transaction). claim-before-advertise's successor
//     is claim-before-serve: the new leader's claim row is durable
//     before it serves pulls — encoded here as failover bumping the
//     floor in the same action that hands authority over. The
//     acquire-epoch-vs-generation distinction is imported from the log
//     campaign's record + the recovery code, unchanged.
//   - retryPolicy.qnt (the retryPolicyPull regime): attemptsChargedOnce,
//     verdictMatchesFold, failoverPreservesHistory over the pull-mode
//     event-arrival environment. This model owes the retry model the
//     environment facts its pull regime states: the attempt-row dedup
//     lifecycle (one open row, one classification), identity freshness
//     (a fresh pod per attempt; the identity is the intent, so a
//     replacement pod converges on the same attempt rather than minting
//     a parallel one), and the no-attempt no-op rule.
//   - spawnCoherence.qnt (Model J): ceilingRespected, reapSafety,
//     orphanRemoved, gateFailClosed, ackSoundness, ackCoversPending,
//     degradedPolarity. What this model GUARANTEES back to Model J/N:
//     busy-accuracy of the durable open-attempt view (an in-flight
//     intent always has its open row — unresolvedClaimHasRepairArmed's
//     first conjunct — and a closed/never-pulled intent has none), the
//     ICE-clear edge at first successful pull (mint-time
//     dispatched_cells removal), and termination-report idempotency
//     (the second-installment fill and the no-attempt no-op).
//   - nodeclaimLifecycle.qnt (Model N): nothing imported as a
//     precondition; the dead_nodes signal this subsystem used to
//     produce is retired — the OA2 controller-side clustering is the
//     successor (out of model, recorded in the invariant map).
//
// ENCODING DECISIONS
//   - One state record (`st`) holding the per-intent records plus the
//     generation/floor and fault budgets, a latch record (`viol`) for
//     cross-action "never happens" invariants, and a reachability
//     record (`reach`) feeding the expect-violation witnesses — the
//     same house pattern as the frozen as-built encoding. Latches the
//     pull encoding can never set (the guard exists at every site) are
//     kept so the Stage-C-style overrides (the re-encoded F4
//     calibration check) have a stable oracle; the invariant-map
//     re-target record lists which invariants are by-construction vs
//     checked.
//   - The establishment sweep is per-attempt here (one expired open
//     attempt resolved per action) rather than the batched per-tick
//     pass; the batch is an implementation detail with no protocol
//     content, and per-attempt actions keep the interleavings with the
//     report paths fully explorable.
//   - admit_pull's decision order (token → fence → wantedness →
//     deliverability) is collapsed to its observable outcomes; the
//     token arm is out of model (F8) and the fence arm is the
//     stale-authority action of the fault-leader regime.
//   - Ceilings are action preconditions (NotYetReady retries, pod
//     respawns, attempts per intent, pod deaths, cancels, failovers,
//     stale-write attempts), never saturating clamps.
//
// The invariants, witnesses, regimes and their check wiring are
// documented at their definitions and in nix/quint.nix. Measured state
// counts, depths and wall-clocks live in the introducing commits'
// messages and the CI checks' transcripts, never in this file.
module executorSession {
  // --- 1. types ---------------------------------------------------------

  /// DAG-side status of the intent's derivation (the session-relevant
  /// slice of DerivationStatus, post-deletion):
  /// DWaitingDeps = wanted but not yet deliverable (Created/Queued/
  ///                Substituting/Failed-awaiting-requeue — admit_pull
  ///                answers NotYetReady).
  /// DReady       = deliverable now (admit_pull mints on the next pull).
  /// DOpen        = an attempt is in flight (Assigned/Running).
  /// DCompleted   = terminal success (admit_pull answers Gone).
  /// DGone        = no longer wanted: cancelled, skipped, swept
  ///                (admit_pull answers Gone).
  type DStatus = DWaitingDeps | DReady | DOpen | DCompleted | DGone

  /// The pod the controller spawned for this intent (at most one live
  /// pod per intent at a time; a replacement pod reuses the identity).
  /// PNone    = no pod exists (never spawned, or the previous pod's
  ///            exit/death has been absorbed and no replacement yet).
  /// PWaiting = spawned, has not (yet) been delivered work — the OA6(a)
  ///            wait state; its pulls answer NotYetReady until the drv
  ///            is deliverable to it.
  /// PHolding = holds the WorkAssignment payload (its pull Delivered).
  /// PDead    = died (kubelet/Job-level death), report may never come.
  /// PExited  = exited voluntarily: after its report, after Gone, or
  ///            the charge-free NotYetReady idle exit.
  type PodSt = PNone | PWaiting | PHolding | PDead | PExited

  /// Per-intent record: the DAG node, the pod, and the durable
  /// attempt-row bookkeeping the pull/report/establishment transactions
  /// maintain.
  type DrvRec = {
    dag: DStatus,
    pod: PodSt,
    // Current pod's interaction history (reset when a replacement pod
    // spawns).
    podDelivered: bool,
    podSawNyr: bool,
    pullsLeft: int,
    // Intent-lifetime monotone facts.
    everDelivered: bool,
    built: bool,            // ground truth: wanted outputs exist in the store
    // The open attempt row (the durable open-attempt view's slice for
    // this intent). openRows is an int so the at-most-one invariant is
    // a checkable state predicate, not a typing artifact.
    openRows: int,
    openRecorded: bool,     // worker classification row exists for the open attempt
    openWindowClosed: bool, // deadline + report-slack elapsed for the open attempt
    // The most recently closed attempt's classification state (what the
    // controller's second-installment fill targets).
    closedRecorded: bool,
    closedReasonFilled: bool,
    // Counters (bounded by the regime constants).
    attemptsMinted: int,
    charged: int,           // classification rows that consumed budget (worker fault or establishment)
    podsSpawned: int
  }

  /// Global scheduler/authority state.
  type GlobalSt = {
    drvs: str -> DrvRec,
    clusterGen: int,        // the lease's current generation (the actual authority)
    floor: int,             // the durable claims floor (GREATEST over claim + assignment rows)
    deathsLeft: int,
    cancelsLeft: int,
    failoversLeft: int,
    staleLeft: int
  }

  /// Cross-action "never happens" latches. Each is set only by the
  /// behavior the production guard forbids; the Stage-C-style override
  /// modules (calibration/) revert one guard and expect the matching
  /// invariant to falsify.
  type Viol = {
    staleWrite: bool,            // a below-floor transaction mutated durable state
    secondOpenMint: bool,        // a mint fired while an open row existed
    nyrMutation: bool,           // a NotYetReady answer (or NYR idle exit) mutated anything durable
    establishedBeforeWindow: bool, // the establishment charge fired inside the window
    doubleClassification: bool,  // a second classification landed on the same attempt
    fabricatedCompletion: bool   // a completion was recorded with no outputs behind it
  }

  /// Reachability flags feeding the expect-violation witnesses.
  type Reach = {
    warmStart: bool,             // NotYetReady first, Deliver later (OA6 witness i)
    idleExitNeverDelivered: bool, // charge-free idle exit of a never-delivered pod (OA6 witness ii)
    established: bool,           // the establishment charge arm fired
    adopted: bool,               // the establishment store-probe adopt arm fired
    synthesized: bool,           // a controller-synthesized verdict closed an open attempt
    secondInstallment: bool,     // the reason-only fill landed on a recorded row
    redelivered: bool,           // an idempotent re-pull re-delivered an open attempt
    lateReportIgnored: bool,     // a worker report for an already-classified attempt was acked-and-ignored
    noAttemptNoop: bool,         // a pod-terminal report found no attempt and charged nothing
    staleFenced: bool,           // a below-floor authority attempted a write and was fenced
    deliveredAfterFailover: bool // a pull was served by the post-failover authority
  }

  // --- 2. constants (const = overridable per regime) ---------------------

  const DRVS: Set[str]
  const PULL_CEIL: int      // NotYetReady answers a single pod will retry through
  const ATTEMPTS_CEIL: int  // attempts minted per intent
  const PODS_CEIL: int      // pods spawned per intent (initial + replacements)
  const POD_DEATHS: int     // pod deaths across the run
  const CANCELS: int        // cancellation events across the run
  const FAILOVERS: int      // leader failovers (fault-leader regime)
  const STALE_TRIES: int    // below-floor write attempts (fault-leader regime)

  // --- 3. pure defs -----------------------------------------------------

  pure def freshDrv: DrvRec = {
    dag: DWaitingDeps,
    pod: PNone,
    podDelivered: false,
    podSawNyr: false,
    pullsLeft: 0,
    everDelivered: false,
    built: false,
    openRows: 0,
    openRecorded: false,
    openWindowClosed: false,
    closedRecorded: false,
    closedReasonFilled: false,
    attemptsMinted: 0,
    charged: 0,
    podsSpawned: 0
  }

  pure def noViol: Viol = {
    staleWrite: false,
    secondOpenMint: false,
    nyrMutation: false,
    establishedBeforeWindow: false,
    doubleClassification: false,
    fabricatedCompletion: false
  }

  pure def noReach: Reach = {
    warmStart: false,
    idleExitNeverDelivered: false,
    established: false,
    adopted: false,
    synthesized: false,
    secondInstallment: false,
    redelivered: false,
    lateReportIgnored: false,
    noAttemptNoop: false,
    staleFenced: false,
    deliveredAfterFailover: false
  }

  /// Close the open attempt row of `r` (the assignments-row update every
  /// closer performs), recording whether the closing classification was
  /// a worker-recorded one.
  pure def closeOpen(r: DrvRec, recorded: bool): DrvRec = {
    ...r,
    openRows: 0,
    openRecorded: false,
    openWindowClosed: false,
    closedRecorded: recorded,
    closedReasonFilled: false
  }

  // --- 4. state variables -----------------------------------------------

  var st: GlobalSt
  var viol: Viol
  var reach: Reach

  // --- 5. derived state ---------------------------------------------------

  def drv(d: str): DrvRec = st.drvs.get(d)

  /// The serving generation of the live leader (the only authority that
  /// answers pulls in this model; stale believers appear only through
  /// the explicit stale-write actions).
  def servingGen: int = st.clusterGen

  // --- 6. invariants ------------------------------------------------------

  /// A pull / establishment / uncharged-close transaction whose serving
  /// generation is below the durable claims floor mutates nothing — the
  /// transaction-side generation fence (admit_pull's
  /// RejectStaleGeneration arm and the GREATEST(...) floor read inside
  /// mint_pull_attempt_fenced, close_pull_attempt_uncharged and
  /// establish_open_pull_attempt). The checked successor of the
  /// dual-belief residual the as-built model priced (T-0e.2).
  val staleAuthorityWritesAreInert = not(viol.staleWrite)

  /// At most one open attempt row per intent/Job: the partial-unique
  /// open-attempt index plus admit_pull's Assigned/Running arm
  /// (idempotent re-delivery for the holder, NotYetReady for anyone
  /// else) — a re-pull converges on the existing attempt instead of
  /// minting a second one.
  val atMostOneOpenAttemptPerJob = and {
    DRVS.forall(d => drv(d).openRows <= 1),
    not(viol.secondOpenMint)
  }

  /// The OA6(a) charge-free clause: a NotYetReady answer and a
  /// NotYetReady-idle exit create no attempt row, consume no retry
  /// budget / produce no charge, bump no claims floor, and never
  /// trigger establishment (admit_pull's NotYetReady arm writes
  /// nothing; the pod-side bounded retry and idle exit are reads).
  val notYetReadyIsInert = and {
    not(viol.nyrMutation),
    DRVS.forall(d => not(drv(d).everDelivered) implies and {
      drv(d).attemptsMinted == 0,
      drv(d).openRows == 0,
      drv(d).charged == 0
    })
  }

  /// Re-based unresolvedClaimHasRepairArmed: an in-flight derivation
  /// always has its durable open-attempt row, so at least one repair
  /// path can resolve it — the holding pod's report loop while the pod
  /// is alive, the controller's pod-terminal ReportAttemptOutcome once
  /// the pod is gone, and the establishment sweep once the window
  /// closes (the sweep visits every open row, so an open row is never
  /// unrepairable). Conversely an open row's derivation is still the
  /// in-flight one — or a cancelled one whose synthesized-verdict close
  /// is the armed repair — so the sweep never charges a derivation that
  /// already moved on.
  val unresolvedClaimHasRepairArmed = DRVS.forall(d => and {
    (drv(d).dag == DOpen) implies (drv(d).openRows == 1),
    (drv(d).openRows == 1) implies (drv(d).dag == DOpen or drv(d).dag == DGone)
  })

  /// A completion is only ever recorded for outputs that exist: the
  /// worker success report carries the outputs it built, and the
  /// establishment sweep's adopt arm requires every verifiable wanted
  /// output present in the store (batch_probe_orphan_outputs) before it
  /// adopts. Nothing else completes a derivation.
  val noFabricatedCompletion = and {
    not(viol.fabricatedCompletion),
    DRVS.forall(d => (drv(d).dag == DCompleted) implies drv(d).built)
  }

  /// Re-keyed establishment-window discipline: the C2 charge arm of the
  /// establishment sweep fires only on an attempt whose window
  /// (dispatch deadline + report slack, anchored at mint time) has
  /// closed — never racing a report that is still inside its window.
  val establishmentOnlyAfterWindowCloses = not(viol.establishedBeforeWindow)

  /// The dedup-state half of one-charge-per-death (the budget
  /// arithmetic itself is retryPolicy.qnt's): each minted attempt is
  /// classified at most once — the first classifier wins the row
  /// (fold_report's AckIgnore arm, the establishment's exec_id
  /// partial-unique arbiter, the synthesized close's
  /// already-present no-op) — and charged at most once.
  val attemptResolvesAtMostOnce = and {
    not(viol.doubleClassification),
    DRVS.forall(d => drv(d).charged <= drv(d).attemptsMinted)
  }

  /// Bounds discipline: every counter stays inside its ceiling (the
  /// ceilings are action preconditions, so this doubles as a check that
  /// no action forgot its precondition).
  val boundsOK = DRVS.forall(d => and {
    drv(d).attemptsMinted <= ATTEMPTS_CEIL,
    drv(d).podsSpawned <= PODS_CEIL,
    drv(d).pullsLeft >= 0,
    drv(d).openRows <= 1,
    drv(d).charged <= ATTEMPTS_CEIL
  })

  val allInvariants = and {
    staleAuthorityWritesAreInert,
    atMostOneOpenAttemptPerJob,
    notYetReadyIsInert,
    unresolvedClaimHasRepairArmed,
    noFabricatedCompletion,
    establishmentOnlyAfterWindowCloses,
    attemptResolvesAtMostOnce,
    boundsOK
  }

  // --- 6b. expect-violation witnesses (non-vacuity probes) ----------------
  // Each is "the interesting state never happens"; the wired checks pass
  // only when the checker violates it. The OA6 reachability pair is the
  // first two.

  /// OA6 (i): the NotYetReady → drv-becomes-Ready → Deliver warm-start
  /// path is reachable (a forecast-spawned pod waits and then builds).
  val canReachWarmStartDeliver = not(reach.warmStart)

  /// OA6 (ii): the never-Ready → charge-free idle-exit path is
  /// reachable (a forecast-spawned pod that never sees Ready exits 0).
  val canReachNeverReadyIdleExit = not(reach.idleExitNeverDelivered)

  /// The establishment charge arm fires somewhere in the explored space
  /// (establishmentOnlyAfterWindowCloses and the charge dedup are not
  /// vacuous).
  val canReachEstablishmentCharge = not(reach.established)

  /// The store-probe adopt arm fires (an unreported-but-built attempt
  /// is adopted, never charged).
  val canReachStoreProbeAdopt = not(reach.adopted)

  /// A controller-synthesized verdict closes an open attempt
  /// charge-free (the AD5/C5/C6 cancel/preempt/reap arm).
  val canReachSynthesizedClose = not(reach.synthesized)

  /// The reason-only second installment lands on a worker-recorded row.
  val canReachSecondInstallment = not(reach.secondInstallment)

  /// An idempotent re-pull re-delivers the open attempt (same identity,
  /// no second row).
  val canReachIdempotentRepull = not(reach.redelivered)

  /// An open attempt is orphaned by a pod death (the contended state
  /// unresolvedClaimHasRepairArmed protects).
  val canReachOrphanedOpenAttempt =
    not(DRVS.exists(d => drv(d).openRows == 1 and drv(d).pod == PDead))

  /// A late worker report for an already-classified attempt is
  /// acknowledged and ignored (fold_report's AckIgnore arm is exercised).
  val canReachLateReportIgnored = not(reach.lateReportIgnored)

  /// A pod-terminal report that finds no attempt is acknowledged
  /// charge-free (the no-attempt no-op arm is exercised).
  val canReachNoAttemptNoop = not(reach.noAttemptNoop)

  /// Fault-leader regime: a below-floor authority actually attempted a
  /// write and was fenced (StaleAuthorityWritesAreInert is not vacuous).
  val canReachStaleFenced = not(reach.staleFenced)

  /// Fault-leader regime: the post-failover authority serves a pull
  /// (the protocol continues across failover off the durable rows).
  val canReachPostFailoverDeliver = not(reach.deliveredAfterFailover)

  // --- 7. actions ----------------------------------------------------------

  // --- 7a. DAG-side environment --------------------------------------------

  /// The derivation becomes deliverable: its dependencies completed (or
  /// its substitution check resolved to "must build"). Mirrors the DAG
  /// status reaching Ready (state/derivation.rs transitions; the
  /// ready-set sweep sweep_ready_cached only short-circuits Ready into
  /// Completed, which is out of scope here).
  action drvBecomesReady(d: str): bool = all {
    drv(d).dag == DWaitingDeps,
    st' = { ...st, drvs: st.drvs.set(d, { ...drv(d), dag: DReady }) },
    viol' = viol,
    reach' = reach
  }

  /// The derivation stops being wanted (CancelBuild verdict, submit
  /// retraction, or the cancel cascade). Mirrors
  /// cancel_build_derivations: the DAG node goes terminal Cancelled
  /// immediately; an open attempt row stays open until its closer (the
  /// pod's abort report or the controller's synthesized verdict) lands.
  action drvCancelled(d: str): bool = all {
    st.cancelsLeft > 0,
    drv(d).dag == DWaitingDeps or drv(d).dag == DReady or drv(d).dag == DOpen,
    st' = { ...st,
      cancelsLeft: st.cancelsLeft - 1,
      drvs: st.drvs.set(d, { ...drv(d), dag: DGone })
    },
    viol' = viol,
    reach' = reach
  }

  // --- 7b. pod lifecycle (controller spawn, pull, idle exit, death) --------

  /// The controller spawns the Job/pod for a wanted intent — possibly
  /// before the drv is Ready (the OA6 forecast-spawn case; Model J's
  /// publish has no ready filter per the OA6(a) decision). The pod is
  /// born knowing its derivation and starts its bounded pull loop.
  action spawnPod(d: str): bool = all {
    drv(d).pod == PNone or drv(d).pod == PDead or drv(d).pod == PExited,
    drv(d).dag == DWaitingDeps or drv(d).dag == DReady or drv(d).dag == DOpen,
    drv(d).podsSpawned < PODS_CEIL,
    st' = { ...st, drvs: st.drvs.set(d, { ...drv(d),
      pod: PWaiting,
      podDelivered: false,
      podSawNyr: false,
      pullsLeft: PULL_CEIL,
      podsSpawned: drv(d).podsSpawned + 1
    }) },
    viol' = viol,
    reach' = reach
  }

  /// PullAssignment answered NotYetReady (admit_pull's NotYetReady arm:
  /// deps unbuilt / substitution in flight / a retry waiting to
  /// requeue, or the drv open on a different identity — the latter
  /// cannot arise here because the identity IS the intent). Writes
  /// nothing, mints nothing, charges nothing; the pod consumes one
  /// retry of its bounded budget (NOT_YET_READY_RETRY_AFTER_SECS pacing
  /// + the builder idle_timeout bound).
  action pullNotYetReady(d: str): bool = all {
    drv(d).pod == PWaiting,
    drv(d).dag == DWaitingDeps,
    drv(d).pullsLeft > 0,
    st' = { ...st, drvs: st.drvs.set(d, { ...drv(d),
      podSawNyr: true,
      pullsLeft: drv(d).pullsLeft - 1
    }) },
    viol' = viol,
    reach' = reach
  }

  /// PullAssignment delivers a fresh attempt (admit_pull's DeliverNew
  /// arm + mint_and_deliver): the fenced mint transaction writes the
  /// assignments/drv_executions rows at the serving generation,
  /// transitions the derivation out of Ready, and the payload is
  /// delivered to the pod. The claims floor ratchets to the serving
  /// generation (the row itself carries it).
  action pullDeliverNew(d: str): bool = all {
    drv(d).pod == PWaiting,
    drv(d).dag == DReady,
    drv(d).openRows == 0,
    drv(d).attemptsMinted < ATTEMPTS_CEIL,
    val r = drv(d)
    st' = { ...st,
      floor: if (servingGen > st.floor) servingGen else st.floor,
      drvs: st.drvs.set(d, { ...r,
        dag: DOpen,
        pod: PHolding,
        podDelivered: true,
        everDelivered: true,
        openRows: r.openRows + 1,
        openRecorded: false,
        openWindowClosed: false,
        attemptsMinted: r.attemptsMinted + 1
      })
    },
    viol' = viol,
    reach' = { ...reach,
      warmStart: reach.warmStart or drv(d).podSawNyr,
      deliveredAfterFailover: reach.deliveredAfterFailover or st.clusterGen > 1
    }
  }

  /// PullAssignment re-delivers the existing open attempt (admit_pull's
  /// DeliverExisting arm): the pulling identity already holds the open
  /// attempt — a retried pull, or a replacement pod of the same intent
  /// converging on the attempt its predecessor opened. Read-only: the
  /// payload is rebuilt from the same row, nothing is written.
  action pullRedeliver(d: str): bool = all {
    drv(d).pod == PWaiting or drv(d).pod == PHolding,
    drv(d).dag == DOpen,
    drv(d).openRows == 1,
    st' = { ...st, drvs: st.drvs.set(d, { ...drv(d),
      pod: PHolding,
      podDelivered: true
    }) },
    viol' = viol,
    reach' = { ...reach, redelivered: true }
  }

  /// PullAssignment answers Gone (admit_pull's Gone arm): the
  /// derivation is terminal or no longer wanted (or absent from the
  /// DAG). The pod exits 0 charge-free; nothing is written.
  action pullGone(d: str): bool = all {
    drv(d).pod == PWaiting,
    drv(d).dag == DCompleted or drv(d).dag == DGone,
    st' = { ...st, drvs: st.drvs.set(d, { ...drv(d), pod: PExited }) },
    viol' = viol,
    reach' = reach
  }

  /// The pod's bounded NotYetReady retry budget runs out (the builder
  /// idle_timeout, OA6's pod-side bound): a pod that has only ever been
  /// answered NotYetReady exits 0 charge-free. The drv may later become
  /// Ready and a later replacement pod's pull Delivers.
  action idleExit(d: str): bool = all {
    drv(d).pod == PWaiting,
    not(drv(d).podDelivered),
    drv(d).pullsLeft == 0,
    st' = { ...st, drvs: st.drvs.set(d, { ...drv(d), pod: PExited }) },
    viol' = viol,
    reach' = { ...reach,
      idleExitNeverDelivered: reach.idleExitNeverDelivered or not(drv(d).everDelivered)
    }
  }

  /// The pod dies (OOM, eviction, node loss, deadline kill — the Job's
  /// lifecycle is the liveness authority now; no scheduler-side
  /// detection exists). If it died mid-build it may or may not have
  /// finished uploading its outputs first — that nondeterminism is what
  /// makes both establishment arms (adopt vs charge) reachable.
  action podDies(d: str, uploadedFirst: bool): bool = all {
    st.deathsLeft > 0,
    drv(d).pod == PWaiting or drv(d).pod == PHolding,
    st' = { ...st,
      deathsLeft: st.deathsLeft - 1,
      drvs: st.drvs.set(d, { ...drv(d),
        pod: PDead,
        built: drv(d).built or (drv(d).pod == PHolding and uploadedFirst)
      })
    },
    viol' = viol,
    reach' = reach
  }

  // --- 7c. the worker report (ReportOutcome) --------------------------------

  /// The pod reports success (handle_report_outcome → fold_report =
  /// Process → handle_completion success arm): the classification row
  /// is written, the attempt closes, the derivation completes. The
  /// charge arithmetic (none for success) is the retry fold's.
  action workerReportSuccess(d: str): bool = all {
    drv(d).pod == PHolding,
    drv(d).dag == DOpen,
    drv(d).openRows == 1,
    not(drv(d).openRecorded),
    val r = drv(d)
    st' = { ...st, drvs: st.drvs.set(d, { ...closeOpen(r, true),
      dag: DCompleted,
      pod: PExited,
      built: true
    }) },
    viol' = viol,
    reach' = reach
  }

  /// The pod reports a failure (handle_completion failure arm): the
  /// classification row is written and consumes budget (the fold's
  /// charge — at most once per attempt), the attempt closes, the
  /// derivation requeues to Ready (poison/threshold verdicts are the
  /// retry model's subject).
  action workerReportFailure(d: str): bool = all {
    drv(d).pod == PHolding,
    drv(d).dag == DOpen,
    drv(d).openRows == 1,
    not(drv(d).openRecorded),
    drv(d).charged < ATTEMPTS_CEIL,
    val r = drv(d)
    st' = { ...st, drvs: st.drvs.set(d, { ...closeOpen(r, true),
      dag: DReady,
      pod: PExited,
      charged: r.charged + 1
    }) },
    viol' = viol,
    reach' = reach
  }

  /// The pod reports Cancelled for a derivation the scheduler no longer
  /// wants, or aborts on SIGTERM while the derivation is still wanted
  /// (report_outcome_inner's abort_of_still_wanted arm +
  /// close_pull_attempt_uncharged): the attempt closes charge-free; a
  /// still-wanted derivation requeues, a cancelled one stays terminal.
  action workerAbortReport(d: str): bool = all {
    drv(d).pod == PHolding,
    drv(d).openRows == 1,
    not(drv(d).openRecorded),
    drv(d).dag == DOpen or drv(d).dag == DGone,
    val r = drv(d)
    val requeued = if (r.dag == DOpen) DReady else DGone
    st' = { ...st, drvs: st.drvs.set(d, { ...closeOpen(r, false),
      dag: requeued,
      pod: PExited
    }) },
    viol' = viol,
    reach' = reach
  }

  /// A late or duplicate worker report whose attempt is already
  /// classified/closed (fold_report's AckIgnore arm: terminal-row-wins,
  /// never reclassified, never re-charged). Read-only.
  action lateWorkerReport(d: str): bool = all {
    drv(d).attemptsMinted > drv(d).openRows, // some attempt has already closed
    st' = st,
    viol' = viol,
    reach' = { ...reach, lateReportIgnored: true }
  }

  // --- 7d. the controller report (ReportAttemptOutcome) ---------------------

  /// Pod-terminal report that resolves to no attempt (a pod that died
  /// before its first successful pull, or a stale report after the
  /// attempt closed and was superseded): acknowledged, nothing written,
  /// nothing charged (the no-attempt no-op arm of
  /// report_attempt_outcome_inner).
  action ctrlReportNoAttempt(d: str): bool = all {
    drv(d).pod == PDead,
    drv(d).openRows == 0,
    st' = st,
    viol' = viol,
    reach' = { ...reach, noAttemptNoop: true }
  }

  /// Pod-terminal report landing on a worker-recorded attempt: the
  /// reason-only second installment (fill_termination_reason_only) —
  /// never a reclassification, never a new row, never a budget or floor
  /// change. (The recorded row has already closed the attempt, so this
  /// targets the closed row's NULL termination_reason.)
  action ctrlSecondInstallment(d: str): bool = all {
    drv(d).closedRecorded,
    not(drv(d).closedReasonFilled),
    st' = { ...st, drvs: st.drvs.set(d, { ...drv(d), closedReasonFilled: true }) },
    viol' = viol,
    reach' = { ...reach, secondInstallment: true }
  }

  /// Controller-synthesized verdict for an open, never-worker-reported
  /// attempt whose pod the controller is deleting or has observed gone
  /// (cancelled / preempted / reaped — the AD5/C5/C6 arms of
  /// report_attempt_outcome_inner → close_pull_attempt_uncharged): the
  /// attempt closes charge-free; a still-wanted derivation requeues in
  /// the same turn, a cancelled one stays terminal. Pod-terminal
  /// reasons that are not synthesized verdicts (OOM, deadline, plain
  /// error) deliberately wait for the establishment sweep — encoded by
  /// NOT having an action for them.
  action ctrlSynthesizedClose(d: str): bool = all {
    drv(d).openRows == 1,
    not(drv(d).openRecorded),
    drv(d).pod == PDead or drv(d).dag == DGone,
    val r = drv(d)
    val requeued = if (r.dag == DGone) DGone else DReady
    st' = { ...st, drvs: st.drvs.set(d, { ...closeOpen(r, false), dag: requeued }) },
    viol' = viol,
    reach' = { ...reach, synthesized: true }
  }

  // --- 7e. the establishment sweep ------------------------------------------

  /// The open attempt's establishment window closes: its dispatch-time
  /// deadline (persisted by the mint) plus the configured report slack
  /// has elapsed (tick_sweep_open_pull_attempts' age filter — the
  /// sweep-time re-solve may only widen the window). Bucketed time.
  action windowExpires(d: str): bool = all {
    drv(d).openRows == 1,
    not(drv(d).openWindowClosed),
    st' = { ...st, drvs: st.drvs.set(d, { ...drv(d), openWindowClosed: true }) },
    viol' = viol,
    reach' = reach
  }

  /// Establishment sweep, store-probe adopt arm
  /// (establish_open_pull_attempt): every verifiable wanted output is
  /// already in the store, so the expired attempt is adopted as
  /// completed — closed, never charged.
  action establishAdopt(d: str): bool = all {
    drv(d).openRows == 1,
    drv(d).openWindowClosed,
    not(drv(d).openRecorded),
    drv(d).built,
    drv(d).dag == DOpen,
    val r = drv(d)
    st' = { ...st, drvs: st.drvs.set(d, { ...closeOpen(r, false), dag: DCompleted }) },
    viol' = viol,
    reach' = { ...reach, adopted: true }
  }

  /// Establishment sweep, C2 charge arm (establish_open_pull_attempt):
  /// the expired attempt's outputs are not in the store and no report
  /// ever arrived — exactly one executor-crash/unreported
  /// classification is appended (the exec_id partial-unique index is
  /// the arbiter), fenced by the claims floor, the assignment row
  /// closes in the same transaction, and the derivation requeues.
  /// ORACLE SEAT (bughunt-2 slot 11, quint-policy P5 — the fencedWrites
  /// hoist pattern): the establishment charge's apply. The window gate
  /// is the CALLER's admission decision; the established-before-window
  /// truth is computed here from the pre-state, whatever the caller
  /// checked. Under the live guard the oracle is identity; the F4
  /// calibration drops the guard and the same seat latches honestly.
  action establishChargeApply(d: str): bool = all {
    val r = drv(d)
    st' = { ...st, drvs: st.drvs.set(d, { ...closeOpen(r, false),
      dag: DReady,
      charged: r.charged + 1
    }) },
    viol' = { ...viol,
      establishedBeforeWindow: (viol.establishedBeforeWindow or not(drv(d).openWindowClosed))
    },
    reach' = { ...reach, established: true }
  }

  action establishCharge(d: str): bool = all {
    drv(d).openRows == 1,
    drv(d).openWindowClosed,
    not(drv(d).openRecorded),
    not(drv(d).built),
    drv(d).dag == DOpen,
    drv(d).charged < ATTEMPTS_CEIL,
    establishChargeApply(d)
  }

  // --- 7f. leader failover and the stale authority (fault-leader regime) ----

  /// Leader failover: the lease moves, the new leader recovers from the
  /// durable rows and writes its generation-claim row before serving
  /// (claim-before-serve — recovery's ordering), so the claims floor
  /// ratchets with the generation in the same step. Every durable fact
  /// (open attempts, classifications, charges) survives untouched —
  /// that is the point of the durable-first protocol.
  action leaderFailover: bool = all {
    st.failoversLeft > 0,
    st' = { ...st,
      failoversLeft: st.failoversLeft - 1,
      clusterGen: st.clusterGen + 1,
      floor: st.clusterGen + 1
    },
    viol' = viol,
    reach' = reach
  }

  /// ORACLE SEAT (bughunt-2 slot 11, quint-policy P4 — the fencedWrites
  /// hoist pattern): the stale-authority transaction's apply. The fence
  /// verdict is the CALLER's decision; the staleWrite truth is computed
  /// here — a durable mutation by a below-floor authority latches,
  /// whatever the caller believed. The live caller passes the
  /// production fence (holds); a calibration perturbs ONLY this
  /// decision.
  action staleAuthorityApply(d: str, fenceHolds: bool): bool = all {
    st' = { ...st, staleLeft: st.staleLeft - 1 },
    viol' = { ...viol, staleWrite: (viol.staleWrite or not(fenceHolds)) },
    reach' = { ...reach, staleFenced: true }
  }

  /// A deposed-but-unaware replica (serving generation strictly below
  /// the durable claims floor) attempts the authority-exercising
  /// transactions — a pull mint or an establishment charge. The
  /// transaction-side fence (the GREATEST floor read inside the
  /// transaction; admit_pull's RejectStaleGeneration advisory arm)
  /// rejects it: nothing is minted, nothing is charged, nothing closes.
  /// The staleWrite oracle is computed in the seat from this caller's
  /// fence verdict (always holds in production), and the fault-leader
  /// witness canReachStaleFenced pins that the attempt itself is
  /// reachable.
  action staleAuthorityAttempt(d: str): bool = all {
    st.staleLeft > 0,
    st.floor > 1, // a failover has happened, so a stale generation exists
    staleAuthorityApply(d, true)
  }

  // --- 8. init ------------------------------------------------------------

  action init = all {
    st' = {
      drvs: DRVS.mapBy(d => freshDrv),
      clusterGen: 1,
      floor: 1,
      deathsLeft: POD_DEATHS,
      cancelsLeft: CANCELS,
      failoversLeft: FAILOVERS,
      staleLeft: STALE_TRIES
    },
    viol' = noViol,
    reach' = noReach
  }

  // --- 9. step --------------------------------------------------------------

  action step = {
    nondet d = DRVS.oneOf()
    nondet uploadedFirst = Set(true, false).oneOf()
    any {
      drvBecomesReady(d),
      drvCancelled(d),
      spawnPod(d),
      pullNotYetReady(d),
      pullDeliverNew(d),
      pullRedeliver(d),
      pullGone(d),
      idleExit(d),
      podDies(d, uploadedFirst),
      workerReportSuccess(d),
      workerReportFailure(d),
      workerAbortReport(d),
      lateWorkerReport(d),
      ctrlReportNoAttempt(d),
      ctrlSecondInstallment(d),
      ctrlSynthesizedClose(d),
      windowExpires(d),
      establishAdopt(d),
      establishCharge(d),
      leaderFailover,
      staleAuthorityAttempt(d)
    }
  }
}

// ===========================================================================
// Regimes (one thin instantiation module per check; nix/quint.nix wires
// each as its own exhaustive TLC run, plus the witness checks).
// ===========================================================================

/// Base regime: no leader faults. The full pull lifecycle — forecast
/// spawn before Ready, the bounded NotYetReady retry loop and its
/// charge-free idle exit, delivery, idempotent re-pull, the worker
/// report (success / failure / abort), pod death with and without
/// uploaded outputs, the controller's no-attempt no-op, second
/// installment and synthesized verdict, and both establishment arms —
/// must hold every invariant here. Pod death and cancellation are
/// normal protocol events on the pull path (the Job's lifecycle is the
/// liveness authority), so they live in the base regime, not a fault
/// regime.
module executorSessionBase {
  import executorSession(
    DRVS = Set("d1"),
    PULL_CEIL = 2,
    ATTEMPTS_CEIL = 2,
    PODS_CEIL = 3,
    POD_DEATHS = 2,
    CANCELS = 1,
    FAILOVERS = 0,
    STALE_TRIES = 0
  ).*
}

/// Fault-leader regime: one leader failover (claim-before-serve floor
/// ratchet) plus below-floor write attempts by a deposed believer. The
/// durable open-attempt state must survive the failover untouched, the
/// post-failover authority must keep serving pulls off the durable
/// rows, and every stale-authority transaction must be fenced to a
/// no-op (StaleAuthorityWritesAreInert — the checked successor of the
/// dual-belief residual the as-built model priced).
module executorSessionFaultLeader {
  import executorSession(
    DRVS = Set("d1"),
    PULL_CEIL = 2,
    ATTEMPTS_CEIL = 2,
    PODS_CEIL = 3,
    POD_DEATHS = 2,
    CANCELS = 1,
    FAILOVERS = 1,
    STALE_TRIES = 2
  ).*
}
