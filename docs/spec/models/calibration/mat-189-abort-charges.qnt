// -*- mode: Bluespec; -*-
//
// Bughunt-wave B1 calibration override, the abort-charges shape
// (merged_bug_189's pre-fix transport half): before
// MaterializationOutcome::Aborted existed, a SIGTERM-aborted store
// walk surfaced through the class-blind `Err(e) => infra_failure(...)`
// arm — every routine rollout charged the materialization budget
// exactly like a dead upstream, and a rolling restart could march a
// healthy job into the park. The production fix is the Aborted wire
// variant + the charge-free intake through release_claim (owner Q3,
// charge-free settlement recorded in the T-0e.6 extension block).
//
// PRE-FIX behavior modeled: calibAbortCharges replaces workerAborted —
// same trigger (SIGTERM mid-walk on a claimed materialization
// attempt), but the close charges the per-job infra window (the
// reportInfra edge) and latches abortChargeFree false.
//
// PREDICTED: abortNeverCharges falsifies on the first override step.
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalibAbortCharges \
//     --step=calibStep --invariant=abortNeverCharges \
//     docs/spec/models/calibration/mat-189-abort-charges.qnt
// Baseline (the as-shipped step, must HOLD):
//   quint verify --backend=tlc --main=matCalibAbortCharges \
//     --step=step --invariant=abortNeverCharges \
//     docs/spec/models/calibration/mat-189-abort-charges.qnt
module matCalibAbortCharges {
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
    ENABLE_WORKER_ABORTED = true
  ).* from "../materializationJob"

  /// PRE-FIX abort intake: the SIGTERM-aborted walk charges the
  /// per-job infra window (reportInfra's edge, verbatim) and the
  /// abortChargeFree latch records the violation.
  action calibAbortCharges(d: str): bool = all {
    match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) => a.kind == KMaterialization
    },
    match jobs.get(d) {
      | NoJob => false
      | SomeJob(j) => j.state == JClaimed and j.matInfraCount < MAT_BUDGET + 2
    },
    // THE BUG: the abort drew the budget — the charging decision,
    // routed through the LIVE abort-settlement seat (R10/P5: the
    // seat computes the charge shape and the abort latch; this
    // override perturbs the decision only) and the conservative
    // release through the live release seat.
    abortSettleApply(d, true),
    releaseStatusApply(d, false),
    attempts' = attempts.put(d, NoAttempt),
    dupJob' = dupJob, topdownPruned' = topdownPruned, execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, envUnchanged,
    matUnobtainableLedger' = matUnobtainableLedger,
    buildCrashLedger' = buildCrashLedger,
    establishCount' = establishCount, buildCrashEstablished' = buildCrashEstablished,
    unobChargeBacked' = unobChargeBacked,
    tenureUnchanged,
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

  action calibStep = {
    nondet d = DRVS.oneOf()
    nondet b = BUILDS.oneOf()
    nondet o = OUTPUTS.oneOf()
    nondet replica = REPLICAS.oneOf()
    nondet builder = BUILDERS.oneOf()
    nondet origin = Set(OProbe, OMerge, OPruned, OReprobe, OStaleReset).oneOf()
    nondet outs = OUTPUTS.powerset().oneOf()
    nondet missing = OUTPUTS.powerset().oneOf()
    nondet refsMissing = Set(true, false).oneOf()
    nondet rel = Set(RVouched, RPending, RChildlessLeaf, RHoled).oneOf()
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
      calibAbortCharges(d),
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
}
