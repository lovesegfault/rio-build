// -*- mode: Bluespec; -*-
//
// C-prime calibration override, family F10 L1-half (CE-48(i) -> L1).
// Transfer row: §9.3 "override 'pending jobs not listed after failover'
// falsifies unresolvedJobAlwaysArmed".
//
// PRE-FIX behavior modeled: the pre-T-4.3 recovery — the failover drops
// the in-memory job view WITHOUT the eager rebuild. A pending job's
// view entry becomes VNone: materialization claims answer Gone (the
// store skips the job forever) and builder pulls see no job — the
// armed action is stranded until a lazy dispatch-probe re-feed.
//
// PREDICTED: unresolvedJobAlwaysArmed (the view-faithfulness form)
// falsifies — a Pending job row with a VNone view entry.
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalibF10FailoverViewDrop \
//     --step=calibStep --invariant=unresolvedJobAlwaysArmed \
//     docs/spec/models/calibration/mat-f10-failover-view-drop.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalibF10FailoverViewDrop \
//     --invariant=unresolvedJobAlwaysArmed \
//     docs/spec/models/calibration/mat-f10-failover-view-drop.qnt
module matCalibF10FailoverViewDrop {
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
    ENABLE_STALE_TENURE = false,
    ENABLE_RESOLVE_FAULTS = false,
    ENABLE_WORKER_ABORTED = false
  ).* from "../materializationJob"

  /// PRE-FIX failover: the view is dropped, not rebuilt.
  action calibFailoverDropsView = all {
    ENABLE_FAILOVER,
    leaseGen < MAX_GEN,
    leaseGen' = leaseGen + 1,
    view' = DRVS.mapBy(d => VNone),
    channelStale' = REPLICAS.mapBy(r => true),
    failedOver' = true,
    failoverDroppedJobs' = failoverDroppedJobs,
    jobs' = jobs, dupJob' = dupJob, attempts' = attempts,
    nodeStatus' = nodeStatus, topdownPruned' = topdownPruned,
    execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
    staleWriteDiscarded' = staleWriteDiscarded,
    staleJobWriteApplied' = staleJobWriteApplied,
    claimAfterFailover' = claimAfterFailover,
    ghostsUnchanged,
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
      establishCrashedAttempt(d),
      cancelOnZeroInterest(d),
      obsoleteOnProduced(d),
      parkReevaluate(d),
      parkBackoffExpires(d),
      buildTerminal(b),
      builderPull(d, builder),
      dispatchFromSource(d),
      completeReadyFromStore(d),
      calibFailoverDropsView,
      redialChannel(replica),
      staleTenureWriteDiscarded(d),
      releasePins(d),
      storeGc(d, o),
      upstreamChange(d, o),
      durableRelationChange(d, rel),
    }
  }
}
