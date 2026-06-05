// -*- mode: Bluespec; -*-
//
// Bughunt-2 R10 calibration override, the delta-5/A10 row-durability
// hypothetical: a failover DROPS unresolved materialization job rows.
// No production defect is transferred here — jobs are PG rows and the
// failover cannot touch them — this is failoverPreservesJobs' first
// falsifier (the invariant was P1-untwinned in all seven regimes that
// check it: no override anywhere could make it fire, so a regression
// in the live failover's row handling would have been invisible).
//
// PRE-FIX-SHAPED behavior modeled: calibFailoverDropsRows is the live
// failover with the durability decision flipped — dropsRows=true
// routed through the SAME failoverRowsApply seat the live action
// uses (R10/P5: the seat computes the drop truth from the pending
// population; this override perturbs the decision only). The eager
// T-4.3 view rebuild then projects the emptied table (every entry
// VNone — projView(NoJob, _)).
//
// PREDICTED: failoverPreservesJobs falsifies — a failover fires while
// some job is unresolved and the latch records the dropped rows.
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalibFailoverDropsRows \
//     --step=calibStep --invariant=failoverPreservesJobs \
//     docs/spec/models/calibration/mat-failover-drops-rows.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalibFailoverDropsRows \
//     --invariant=failoverPreservesJobs \
//     docs/spec/models/calibration/mat-failover-drops-rows.qnt
module matCalibFailoverDropsRows {
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

  /// The row-dropping failover: the live action with the durability
  /// decision flipped at the seat.
  action calibFailoverDropsRows = all {
    ENABLE_FAILOVER,
    leaseGen < MAX_GEN,
    leaseGen' = leaseGen + 1,
    // THE HYPOTHETICAL: rows dropped — the decision routed through
    // the live seat.
    failoverRowsApply(true),
    // T-4.3 eager rebuild over the EMPTIED table: every projection is
    // projView(NoJob, _) = VNone.
    view' = DRVS.mapBy(d => VNone),
    channelStale' = REPLICAS.mapBy(r => true),
    failedOver' = true,
    dupJob' = dupJob, attempts' = attempts,
    nodeStatus' = nodeStatus, topdownPruned' = topdownPruned,
    execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
    staleWriteDiscarded' = staleWriteDiscarded,
    staleJobWriteApplied' = staleJobWriteApplied,
    claimAfterFailover' = claimAfterFailover,
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
      calibFailoverDropsRows,
      redialChannel(replica),
      staleTenureWriteDiscarded(d),
      releasePins(d),
      storeGc(d, o),
      upstreamChange(d, o),
      durableRelationChange(d, rel),
    }
  }
}
