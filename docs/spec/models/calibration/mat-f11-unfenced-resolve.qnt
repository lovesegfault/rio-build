// -*- mode: Bluespec; -*-
//
// C-prime calibration override, family F11 (CE-50 -> A18, extended).
// Transfer row: §9.3 "kept guard, extended: the a17-unfenced pin's
// analogue for job-table + wanted-relation writes (override: unfenced
// job resolve)".
//
// PRE-FIX behavior modeled: a deposed tenure's queued job-resolve
// APPLIES after the successor's recovery instead of being discarded by
// the claims-floor fence (the closure-a17-unfenced shape, re-pointed
// at the job table).
//
// PREDICTED: fencedJobWritesOnly falsifies — the staleJobWriteApplied
// latch records the below-floor apply.
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalibF11UnfencedResolve \
//     --step=calibStep --invariant=fencedJobWritesOnly \
//     docs/spec/models/calibration/mat-f11-unfenced-resolve.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalibF11UnfencedResolve \
//     --invariant=fencedJobWritesOnly \
//     docs/spec/models/calibration/mat-f11-unfenced-resolve.qnt
module matCalibF11UnfencedResolve {
  import materializationJob(
    BUILDS = Set("b1", "b2"),
    DRVS = Set("d1"),
    OUTPUTS = Set("o1", "o2"),
    REPLICAS = Set("r1", "r2"),
    BUILDERS = Set("w1"),
    MAT_BUDGET = 2,
    MAX_EXECS = 4,
    MAX_GEN = 2,
    MAX_UPSTREAM_FLIPS = 2,
    MAX_LEGACY = 0,
    ENABLE_FAILOVER = true,
    ENABLE_GC = false,
    ENABLE_CRASH = false,
    ENABLE_STALE_TENURE = true,
    ENABLE_RESOLVE_FAULTS = false,
    ENABLE_WORKER_ABORTED = false
  ).* from "../materializationJob"

  /// PRE-FIX stale-tenure write: the apply (no fence discard).
  action calibStaleTenureWriteApplies(d: str): bool = all {
    ENABLE_STALE_TENURE,
    leaseGen > 0,
    hasUnresolvedJob(d),
    jobs' = jobs.put(d, match jobs.get(d) {
      | NoJob => NoJob
      | SomeJob(j) => SomeJob({ ...j, state: JResolvedFromSource, parked: false })
    }),
    view' = view.put(d, VNone),
    staleJobWriteApplied' = true,
    staleWriteDiscarded' = staleWriteDiscarded,
    attempts' = attempts, nodeStatus' = nodeStatus,
    dupJob' = dupJob, topdownPruned' = topdownPruned, execsMinted' = execsMinted,
    leaseGen' = leaseGen, channelStale' = channelStale,
    failedOver' = failedOver, claimAfterFailover' = claimAfterFailover,
    failoverDroppedJobs' = failoverDroppedJobs,
    relUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
    ghostsUnchanged,
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
      calibStaleTenureWriteApplies(d),
      releasePins(d),
      storeGc(d, o),
      upstreamChange(d, o),
      durableRelationChange(d, rel),
    }
  }
}
