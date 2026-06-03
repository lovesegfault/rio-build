// -*- mode: Bluespec; -*-
//
// Bughunt-wave A3 calibration, family F10 second half (merged_bug_246
// — the swallowed-rebuild shape; model twin of the kani projection
// gap). The as-built failover (delta 5 / T-4.3) rebuilds the view
// EAGERLY and FAITHFULLY — `view' = projView(jobs, attempts)` per drv
// — so the pre-fix behavior is NOT representable in the main model;
// it exists only here (the §2.A3 "INTRODUCED calibration-only"
// polarity, same class as mat-split-rearm-without-reassign).
//
// PRE-FIX behavior modeled: the rebuild RUNS but a per-drv load
// failure is SWALLOWED (recovery.rs:196-210 pre-fix: the job-view
// query's Err arm logged and continued — load_dag_from_rows
// propagated with `?` at :183, the view loader did not). The term
// proceeds as hydrated with a partial view: the failed drv's entry is
// VNone while its durable Pending row lives on — claims answer Gone,
// builder pulls see no job, the armed action is stranded. The
// production fix is the [E] fail-closed posture: the load Err
// propagates (`?` like its :183 sibling) and EVERY degraded path
// serves JobViewState::Unavailable for the whole term — partial
// hydration is unrepresentable.
//
// PREDICTED: unresolvedJobAlwaysArmed (the view-faithfulness form)
// falsifies — a Pending job row whose view entry is VNone while a
// SIBLING drv rebuilt faithfully in the same failover (the partial
// shape that distinguishes this from mat-f10-failover-view-drop's
// no-rebuild-at-all).
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalibF10bSwallowedRebuild \
//     --step=calibStep --invariant=unresolvedJobAlwaysArmed \
//     docs/spec/models/calibration/mat-f10b-swallowed-rebuild.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalibF10bSwallowedRebuild \
//     --invariant=unresolvedJobAlwaysArmed \
//     docs/spec/models/calibration/mat-f10b-swallowed-rebuild.qnt
module matCalibF10bSwallowedRebuild {
  import materializationJob(
    BUILDS = Set("b1", "b2"),
    DRVS = Set("d1", "d2"),
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

  /// PRE-FIX failover: the eager rebuild runs but swallows a nonempty
  /// subset of per-drv load failures — those drvs' entries come back
  /// VNone while the rest rebuild faithfully, and the term proceeds
  /// as hydrated. Everything except the view expression is verbatim
  /// from the as-built `failover`.
  action calibFailoverSwallowedRebuild(swallowed: Set[str]): bool = all {
    ENABLE_FAILOVER,
    leaseGen < MAX_GEN,
    swallowed != Set(),
    leaseGen' = leaseGen + 1,
    // THE BUG: the per-drv rebuild failure is swallowed, not
    // propagated — a partial view is served as hydrated.
    view' = DRVS.mapBy(d =>
      if (swallowed.contains(d)) VNone
      else projView(jobs.get(d), attempts.get(d))),
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
    nondet swallowed = DRVS.powerset().oneOf()
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
      // the override REPLACES the as-built failover
      calibFailoverSwallowedRebuild(swallowed),
      redialChannel(replica),
      staleTenureWriteDiscarded(d),
      releasePins(d),
      storeGc(d, o),
      upstreamChange(d, o),
      durableRelationChange(d, rel),
    }
  }
}
