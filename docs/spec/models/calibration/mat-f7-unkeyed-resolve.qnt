// -*- mode: Bluespec; -*-
//
// C-prime calibration override, family F7 (CE-30/CE-28 -> A3).
// Transfer row: §9.3 "successor falsification: override 'resolve job
// without exec_id identity' falsifies jobResolutionSound".
//
// PRE-FIX behavior modeled: a job resolution that is NOT keyed to one
// of its own attempts' outcomes — any unresolved job resolves
// spontaneously (no open attempt, no consumption transaction, no node
// completion), the model analogue of the clear-keyed-on-the-wrong-
// predicate class.
//
// PREDICTED: jobResolutionSound falsifies — a resolved_success job on
// a node that is not Completed.
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalibF7UnkeyedResolve \
//     --step=calibStep --invariant=jobResolutionSound \
//     docs/spec/models/calibration/mat-f7-unkeyed-resolve.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalibF7UnkeyedResolve \
//     --invariant=jobResolutionSound \
//     docs/spec/models/calibration/mat-f7-unkeyed-resolve.qnt
module matCalibF7UnkeyedResolve {
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

  /// PRE-FIX resolution: un-keyed — resolves without any attempt.
  action calibResolveUnkeyed(d: str): bool = all {
    hasUnresolvedJob(d),
    jobs' = jobs.put(d, match jobs.get(d) {
      | NoJob => NoJob
      | SomeJob(j) => SomeJob({ ...j, state: JResolvedSuccess, parked: false })
    }),
    view' = view.put(d, VNone),
    attempts' = attempts, nodeStatus' = nodeStatus,
    dupJob' = dupJob, topdownPruned' = topdownPruned, execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
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
      calibResolveUnkeyed(d),
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
