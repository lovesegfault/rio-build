// -*- mode: Bluespec; -*-
//
// C-prime calibration override, the CE-17 anchor row (wanted growth vs
// consumption — finding PP-3) carrying THREE §9.3 rows: CE-17 itself
// ("override 'consumeSuccess without coverage check' must falsify
// successConsumptionCoversLiveWanted"), F6's hazard half (the F6 latch
// is by-construction gone; the hazard class is wanted growth racing
// outcome consumption), and F2(a) (the seed-only walk's model-level
// shape: closure incompleteness surfaces as a wanted path still
// missing at consumption — the §9.1 abstraction note).
//
// PRE-FIX behavior modeled: success consumption resolves the job and
// completes the node WITHOUT re-reading the live wanted set (trusts
// the execution's view; interest that grew between execution and
// consumption is silently dropped).
//
// PREDICTED: successConsumptionCoversLiveWanted falsifies — a
// resolved_success whose at-decision coverage was false.
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalibCe17SkipCoverage \
//     --step=calibStep --invariant=successConsumptionCoversLiveWanted \
//     docs/spec/models/calibration/mat-ce17-skip-coverage.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalibCe17SkipCoverage \
//     --invariant=successConsumptionCoversLiveWanted \
//     docs/spec/models/calibration/mat-ce17-skip-coverage.qnt
module matCalibCe17SkipCoverage {
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
  ).* from "../materializationJob"

  /// PRE-FIX success consumption: no coverage re-check — always
  /// resolves success and completes. The covAtResolveAll latch keeps
  /// the production oracle (coverage AT the decision).
  action calibConsumeSuccessNoCoverage(d: str): bool = all {
    match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) => a.kind == KMaterialization and not(channelStale.get(a.instance))
    },
    match jobs.get(d) { | NoJob => false | SomeJob(j) => j.state == JClaimed },
    val execId = match attempts.get(d) { | NoAttempt => 0 | OpenAttempt(a) => a.execId }
    all {
      jobs' = jobs.put(d, match jobs.get(d) {
        | NoJob => NoJob
        | SomeJob(j) => SomeJob({ ...j, state: JResolvedSuccess, resolutionExec: execId })
      }),
      view' = view.put(d, VNone),
      nodeStatus' = nodeStatus.put(d, NCompleted),
      attempts' = attempts.put(d, NoAttempt),
      topdownPruned' = topdownPruned.put(d, false),
      covAtResolveAll' = and { covAtResolveAll, liveWantedCovered(d) },
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
    all {
      any {
        recordWantedRelation(b, d, outs),
        legacyBuildArrives(b, d),
        createJob(d, origin, b),
        dedupRefeed(d),
        claimJob(d, replica),
        executeStep(d, o),
        calibConsumeSuccessNoCoverage(d),
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
      },
      walkUnchanged,
    }
  }
}
