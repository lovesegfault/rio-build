// -*- mode: Bluespec; -*-
//
// C-prime calibration override, family F3 soundness (CE-61 -> B3),
// direction (ii) / the E5 budget-exhaustion class. Transfer row: §9.3
// "override 'park exhaustion fail-fasts' must falsify it too (§2.5's
// park rule is the guard)".
//
// PRE-FIX behavior modeled: the InfraFailure consumption at the budget
// FAILS the node (resolved_unobtainable + NFailed) instead of parking
// — budget exhaustion as a terminal verdict, bypassing the fail-fast
// door entirely.
//
// PREDICTED: materializationNeverPoisons falsifies — an NFailed node
// with zero fail-fast verdicts (the failure came from infra charges).
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalibF3ParkFailFast \
//     --step=calibStep --invariant=materializationNeverPoisons \
//     docs/spec/models/calibration/mat-f3-park-failfast.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalibF3ParkFailFast \
//     --invariant=materializationNeverPoisons \
//     docs/spec/models/calibration/mat-f3-park-failfast.qnt
module matCalibF3ParkFailFast {
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

  /// PRE-FIX InfraFailure consumption: at the budget the node FAILS
  /// (no park, no fail-fast door).
  action calibParkExhaustionFailFasts(d: str): bool = all {
    match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) => a.kind == KMaterialization and not(channelStale.get(a.instance))
    },
    match jobs.get(d) {
      | NoJob => false
      | SomeJob(j) => j.state == JClaimed and j.matInfraCount < MAT_BUDGET + 2
    },
    val newCount = match jobs.get(d) { | NoJob => 0 | SomeJob(j) => j.matInfraCount + 1 }
    val exhausts = newCount >= MAT_BUDGET
    all {
      jobs' = jobs.put(d, match jobs.get(d) {
        | NoJob => NoJob
        | SomeJob(j) => SomeJob({ ...j,
            state: if (exhausts) JResolvedUnobtainable else JPending,
            parked: false, matInfraCount: newCount })
      }),
      view' = if (exhausts) view.put(d, VNone) else view.put(d, VPending(false)),
      nodeStatus' = if (exhausts) nodeStatus.put(d, NFailed) else nodeStatus.put(d, NQueued),
    },
    attempts' = attempts.put(d, NoAttempt),
    matInfraLedger' = matInfraLedger.put(d, matInfraLedger.get(d) + 1),
    dupJob' = dupJob, topdownPruned' = topdownPruned, execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, envUnchanged,
    matUnobtainableLedger' = matUnobtainableLedger,
    buildCrashLedger' = buildCrashLedger,
    establishCount' = establishCount, buildCrashEstablished' = buildCrashEstablished,
    unobChargeBacked' = unobChargeBacked,
    tenureUnchanged, ghostsUnchanged,
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
      consumeSuccess(d),
      consumeUnobtainable(d, missing, refsMissing),
      calibParkExhaustionFailFasts(d),
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
