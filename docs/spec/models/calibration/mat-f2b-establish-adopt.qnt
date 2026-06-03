// -*- mode: Bluespec; -*-
//
// C-prime calibration override, family F2 establishment channel
// (finding BC-3). Transfer row: §9.3 "override 'sweep adopts
// materialization attempt on output-presence only' must falsify
// successConsumptionCoversLiveWanted/B1-successor (the no-adopt rule
// is what blocks it)".
//
// PRE-FIX behavior modeled: the establishment sweep ADOPTS a crashed
// materialization attempt when ANY wanted output is present (the
// mid-walk crash leaves outputs present but the set incomplete —
// output-presence is not closure-completeness).
//
// PREDICTED: successConsumptionCoversLiveWanted falsifies — an adopted
// resolution whose live-wanted coverage was partial.
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalibF2bEstablishAdopt \
//     --step=calibStep --invariant=successConsumptionCoversLiveWanted \
//     docs/spec/models/calibration/mat-f2b-establish-adopt.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalibF2bEstablishAdopt \
//     --invariant=successConsumptionCoversLiveWanted \
//     docs/spec/models/calibration/mat-f2b-establish-adopt.qnt
module matCalibF2bEstablishAdopt {
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
    ENABLE_WORKER_ABORTED = false
  ).* from "../materializationJob"

  /// PRE-FIX establishment: adopt-on-presence for the materialization
  /// kind (build kind unchanged). The covAtResolveAll latch keeps the
  /// production oracle.
  action calibEstablishAdoptsOnPresence(d: str): bool = all {
    ENABLE_CRASH,
    attempts.get(d) != NoAttempt,
    match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) =>
        if (a.kind == KMaterialization)
          OUTPUTS.exists(o => present.get((d, o)))
        else
          buildCrashLedger.get(d) < MAT_BUDGET
    },
    val isMat = match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) => a.kind == KMaterialization
    }
    val execId = match attempts.get(d) { | NoAttempt => 0 | OpenAttempt(a) => a.execId }
    all {
      attempts' = attempts.put(d, NoAttempt),
      jobs' = if (isMat) jobs.put(d, match jobs.get(d) {
        | NoJob => NoJob
        | SomeJob(j) => SomeJob({ ...j, state: JResolvedSuccess, resolutionExec: execId })
      }) else jobs,
      view' = if (isMat) view.put(d, VNone) else view,
      nodeStatus' = if (isMat) nodeStatus.put(d, NCompleted) else nodeStatus.put(d, NQueued),
      topdownPruned' = if (isMat) topdownPruned.put(d, false) else topdownPruned,
      buildCrashLedger' = if (isMat) buildCrashLedger else buildCrashLedger.put(d, buildCrashLedger.get(d) + 1),
      buildCrashEstablished' = if (isMat) buildCrashEstablished else buildCrashEstablished.put(d, true),
      covAtResolveAll' = and { covAtResolveAll, isMat implies liveWantedCovered(d) },
    },
    dupJob' = dupJob, execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, envUnchanged,
    matInfraLedger' = matInfraLedger, matUnobtainableLedger' = matUnobtainableLedger,
    establishCount' = establishCount, unobChargeBacked' = unobChargeBacked,
    tenureUnchanged,
    failFastCount' = failFastCount,
    ffJustifiedAll' = ffJustifiedAll, fsJustifiedAll' = fsJustifiedAll,
    fsReportSoundAll' = fsReportSoundAll, covLiveOnlyAll' = covLiveOnlyAll,
    creationResolvableAll' = creationResolvableAll,
    chargedAfterCancel' = chargedAfterCancel,
    claimOrigin' = claimOrigin,
    carried' = carried,
    budgetSoundAll' = budgetSoundAll,
    abortChargeFree' = abortChargeFree,
    requeueReadyJustifiedAll' = requeueReadyJustifiedAll,
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
      calibEstablishAdoptsOnPresence(d),
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
