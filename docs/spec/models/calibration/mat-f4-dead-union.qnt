// -*- mode: Bluespec; -*-
//
// C-prime calibration override, family F4 B4-half (CE-13 -> B4) and
// the C5/CE-7 closure evidence. Transfer rows: §9.3 "B4: successor
// (interest-union override + the Success-coverage override)" — this
// is the interest-union override (the Success-coverage one is
// mat-ce17-skip-coverage) — and the C5 by-construction record's
// falsifiability half: C5/CE-7 closes by construction (no stored
// union exists; the §6 join is live-only and a terminal build's rows
// drop out atomically), and THIS override demonstrates the latch
// catches exactly the stored-union behavior if it were re-introduced.
//
// PRE-FIX behavior modeled: the success consumption's coverage check
// ranges over the union of ALL builds' wanted rows — live AND dead
// (the stored-union read the live-join replaced): a dead build's
// stale wants drive the completion decision.
//
// PREDICTED: interestUnionLiveOnly falsifies — a consumption decision
// ranged over a union strictly wider than the live-only join.
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalibF4DeadUnion \
//     --step=calibStep --invariant=interestUnionLiveOnly \
//     docs/spec/models/calibration/mat-f4-dead-union.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalibF4DeadUnion \
//     --invariant=interestUnionLiveOnly \
//     docs/spec/models/calibration/mat-f4-dead-union.qnt
module matCalibF4DeadUnion {
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
    ENABLE_WORKER_ABORTED = false
  ).* from "../materializationJob"

  /// The dead-inclusive union (the stored-union read this override
  /// re-introduces).
  def storedUnion(d: str): Set[str] =
    OUTPUTS.filter(o => BUILDS.exists(b => wanted.get((b, d)).contains(o)))

  /// PRE-FIX success consumption: coverage over the stored
  /// (dead-inclusive) union; the covLiveOnlyAll latch keeps the
  /// production oracle (the decision set must equal the live join).
  action calibConsumeSuccessDeadUnion(d: str): bool = all {
    match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) => a.kind == KMaterialization and not(channelStale.get(a.instance))
    },
    match jobs.get(d) { | NoJob => false | SomeJob(j) => j.state == JClaimed },
    val usedUnion = storedUnion(d)
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
      topdownPruned' = if (covered) topdownPruned.put(d, false) else topdownPruned,
      covAtResolveAll' = and { covAtResolveAll, covered implies liveWantedCovered(d) },
      // production oracle: the decision set must be the live join.
      covLiveOnlyAll' = and { covLiveOnlyAll, usedUnion == liveWanted(d) },
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
    carried' = carried,
    budgetSoundAll' = budgetSoundAll,
    abortChargeFree' = abortChargeFree,
    requeueReadyJustifiedAll' = requeueReadyJustifiedAll,
    a4Unchanged,
  }

  action calibStep = {
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
      recordWantedRelation(b, d, outs),
      legacyBuildArrives(b, d),
      createJob(d, origin, b),
      dedupRefeed(d),
      claimJob(d, replica),
      executeStep(d, o),
      calibConsumeSuccessDeadUnion(d),
      consumeUnobtainable(d, missing, refsMissing),
      reportInfra(d),
      establishCrashedAttempt(d),
      cancelOnZeroInterest(d),
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
}
