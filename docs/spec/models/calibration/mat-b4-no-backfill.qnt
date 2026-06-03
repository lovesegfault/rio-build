// -*- mode: Bluespec; -*-
//
// C-prime calibration override, Phase-B bug B4 (056bfc9b6, finding 18
// gap 2 / delta 2b): flag-off-era builds have no build_wanted_outputs
// rows; pre-fix, the probe-partition creation did NOT backfill the
// relation — every execution of the job then failed instantly as
// InfraFailure("no tenant context") and the build never completed.
//
// PRE-FIX behavior modeled: createJob without the legacy-interest
// backfill folds (the wanted relation is untouched at the standalone
// sites).
//
// PREDICTED: creationLeavesTenantResolvable falsifies — a creation
// commits with live in-memory interest but zero live builds holding
// wanted rows.
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalibB4NoBackfill \
//     --step=calibStep --invariant=creationLeavesTenantResolvable \
//     docs/spec/models/calibration/mat-b4-no-backfill.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalibB4NoBackfill \
//     --invariant=creationLeavesTenantResolvable \
//     docs/spec/models/calibration/mat-b4-no-backfill.qnt
module matCalibB4NoBackfill {
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
  ).* from "../materializationJob"

  /// PRE-FIX creation: no backfill. The resolvability latch keeps the
  /// production oracle (computed against the UNBACKFILLED relation).
  action calibCreateJobNoBackfill(d: str, origin: JobOrigin, tenant: str): bool = all {
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
    (origin == OMerge or origin == OPruned or origin == OReprobe) implies
      wanted.get((tenant, d)) != Set(),
    all {
      jobs' = jobs.put(d, SomeJob({ ...freshJob, origin: origin, tenant: tenant })),
      view' = view.put(d, VPending(false)),
      dupJob' = dupJob,
      topdownPruned' = if (origin == OPruned) topdownPruned.put(d, true) else topdownPruned,
      nodeStatus' = if (origin == OStaleReset or origin == OReprobe)
        nodeStatus.put(d, NQueued)
      else nodeStatus,
      // pre-fix: NO backfill.
      wanted' = wanted,
      lastWritten' = lastWritten,
      legacyInterest' = legacyInterest,
      creationResolvableAll' = and {
        creationResolvableAll,
        BUILDS.exists(b => buildLive.get(b) and wanted.get((b, d)) != Set()),
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
    carried' = carried,
    budgetSoundAll' = budgetSoundAll,
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
      calibCreateJobNoBackfill(d, origin, b),
      dedupRefeed(d),
      claimJob(d, replica),
      executeStep(d, o),
      consumeSuccess(d),
      consumeUnobtainable(d, missing),
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
