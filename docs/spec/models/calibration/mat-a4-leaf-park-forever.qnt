// -*- mode: Bluespec; -*-
//
// Bughunt-wave A4 calibration override — THE REMOVED AS-BUILT
// ENCODING behind the RBroken split: with one collapsed Broken cell,
// the park re-evaluation released only Vouched/Pending evidence. A
// childless leaf (broken BY STRUCTURE — nothing to vouch, nothing
// holed, from-source perfectly viable while unmarked) could only
// cycle backoff → claim → infra → park: a forever-park the stalled
// gauge then alerted on as if it were a genuine closure hole.
//
// PRE-FIX behavior modeled: calibParkReevaluateVPOnly restores the
// Vouched|Pending-only guard. Under it the leaf-release edge is
// UNREACHABLE — the noLeafParkReevalResolve witness STOPS violating
// (the b3-no-redial dead-end pattern: this module is the recorded
// flip, not a wired CI check). Additionally the stalled-population
// collapse (parked ∧ any-broken) is restored by calibStalledCollapse
// so the parkNeverOutlivesFromSourceViability invariant falsifies.
//
// PREDICTED (wired): parkNeverOutlivesFromSourceViability falsifies
// under calibStep — a parked, from-source-viable childless leaf lands
// in the collapsed stalled population (calibStalledClaim records it).
// PREDICTED (manual, the dead-end): noLeafParkReevalResolve is NOT
// violated under calibStep:
//   quint run --main=matCalibLeafParkForever --step=calibStep \
//     --invariant=noLeafParkReevalResolve --max-samples=400000 \
//     docs/spec/models/calibration/mat-a4-leaf-park-forever.qnt
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalibLeafParkForever \
//     --step=calibStep --invariant=calibParkViability \
//     docs/spec/models/calibration/mat-a4-leaf-park-forever.qnt
module matCalibLeafParkForever {
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
    ENABLE_WALK_FOLD = false,
    ENABLE_WORKER_ABORTED = false
  ).* from "../materializationJob"

  /// PRE-FIX re-evaluation: Vouched|Pending only — the childless
  /// leaf's park has no release arm.
  action calibParkReevaluateVPOnly(d: str): bool = all {
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
    abortChargeFree' = abortChargeFree,
    a4Unchanged,
  }

  /// The pre-fix gauge collapse, restated as the wired falsification
  /// target: under the collapse, EVERY broken-cell park is "stalled" —
  /// so a parked viable leaf violates the as-built invariant's
  /// viable-parks-are-transients reading. (The invariant references
  /// the as-built `stalledJobs` (Holed-only); the collapse is
  /// reproduced here by quantifying over the pre-fix population.)
  val calibParkViability = DRVS.forall(d =>
    (parkedAt(d) and fromSourceViable(d)) implies
      not(durableRel.get(d) == RChildlessLeaf or durableRel.get(d) == RHoled)
  )

  action calibStep = {
    nondet d = DRVS.oneOf()
    nondet b = BUILDS.oneOf()
    nondet o = OUTPUTS.oneOf()
    nondet replica = REPLICAS.oneOf()
    nondet outs = OUTPUTS.powerset().oneOf()
    nondet origin = Set(OProbe, OMerge, OPruned, OReprobe, OStaleReset).oneOf()
    nondet rel = Set(RVouched, RPending, RChildlessLeaf, RHoled).oneOf()
    all {
      any {
        recordWantedRelation(b, d, outs),
        createJob(d, origin, b),
        claimJob(d, replica),
        executeStep(d, o),
        consumeSuccess(d),
        reportInfra(d),
        // the override REPLACES parkReevaluate
        calibParkReevaluateVPOnly(d),
        parkBackoffExpires(d),
        buildTerminal(b),
        upstreamChange(d, o),
        durableRelationChange(d, rel),
      },
      walkUnchanged,
    }
  }
}
