// -*- mode: Bluespec; -*-
//
// C-prime calibration override, the PP-4 crash-loop family, direction
// (i). Transfer row: §9.3 "override 'establishment writes build-class
// executor_crash for materialization kind' must falsify
// materializationInvisibleToBuildBudgets".
//
// PRE-FIX behavior modeled: the establishment sweep writes an
// executor_crash row into the BUILD ledger for a crashed
// materialization attempt (the kind partition broken at the
// establishment channel — exactly the OQ1-amendment-1 site).
//
// PREDICTED: materializationInvisibleToBuildBudgets falsifies — the
// build-crash ledger moves without a build-kind establishment.
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalibPp4EstablishBuildCharge \
//     --step=calibStep --invariant=materializationInvisibleToBuildBudgets \
//     docs/spec/models/calibration/mat-pp4-establish-build-charge.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalibPp4EstablishBuildCharge \
//     --invariant=materializationInvisibleToBuildBudgets \
//     docs/spec/models/calibration/mat-pp4-establish-build-charge.qnt
module matCalibPp4EstablishBuildCharge {
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
    ENABLE_STALE_TENURE = false
  ).* from "../materializationJob"

  /// PRE-FIX establishment: the materialization arm ALSO charges the
  /// build-crash ledger (without the legitimate-channel ghost).
  action calibEstablishWritesBuildCrash(d: str): bool = all {
    ENABLE_CRASH,
    attempts.get(d) != NoAttempt,
    match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) =>
        if (a.kind == KMaterialization)
          matInfraLedger.get(d) < MAT_BUDGET + 2 and buildCrashLedger.get(d) < MAT_BUDGET
        else
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
      view' = if (isMat) view.put(d, match jobs.get(d) {
        | NoJob => VNone
        | SomeJob(j) => VPending(j.parked)
      }) else view,
      nodeStatus' = nodeStatus.put(d, NQueued),
      matInfraLedger' = if (isMat) matInfraLedger.put(d, matInfraLedger.get(d) + 1) else matInfraLedger,
      establishCount' = if (isMat) establishCount.put(d, establishCount.get(d) + 1) else establishCount,
      // pre-fix: the build ledger is charged for BOTH kinds; the
      // legitimate-channel ghost moves only for the build kind.
      buildCrashLedger' = buildCrashLedger.put(d, buildCrashLedger.get(d) + 1),
      buildCrashEstablished' = if (isMat) buildCrashEstablished else buildCrashEstablished.put(d, true),
    },
    dupJob' = dupJob, topdownPruned' = topdownPruned, execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, envUnchanged,
    matUnobtainableLedger' = matUnobtainableLedger,
    unobChargeBacked' = unobChargeBacked,
    tenureUnchanged, ghostsUnchanged,
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
    nondet rel = Set(RVouched, RPending, RBroken).oneOf()
    any {
      recordWantedRelation(b, d, outs),
      legacyBuildArrives(b, d),
      createJob(d, origin, b),
      dedupRefeed(d),
      claimJob(d, replica),
      executeStep(d, o),
      consumeSuccess(d),
      consumeUnobtainable(d, missing),
      reportInfra(d),
      calibEstablishWritesBuildCrash(d),
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
