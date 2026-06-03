// -*- mode: Bluespec; -*-
//
// Bughunt-wave A3 calibration override, the charge-after-cancel shape
// (the A3 leg of merged_bug_276's class — extends A1's
// chargeFreeCancellation latch per §4.F13): a cancellation that
// resolves the job but LEAVES THE OPEN ATTEMPT (the pre-fix split —
// cancel and attempt-close were separate steps) lets the
// establishment sweep charge a job that no longer exists. The
// production fix is the single-tx cancel (A1's 276 closer) plus
// charge_materialization_infra's view-settlement gate.
//
// PRE-FIX behavior modeled: calibCancelLeavesAttemptOpen is
// cancelOnZeroInterest WITHOUT the attempt close; the main model's
// establishCrashedAttempt then fires on the orphaned attempt and its
// chargedAfterCancel ghost latches the leaked-attempt conversion.
//
// PREDICTED: chargeFreeCancellation falsifies — a materialization
// charge landed after the job's cancellation.
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalibChargeOnCancel \
//     --step=calibStep --invariant=chargeFreeCancellation \
//     docs/spec/models/calibration/mat-charge-on-cancel.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalibChargeOnCancel \
//     --invariant=chargeFreeCancellation \
//     docs/spec/models/calibration/mat-charge-on-cancel.qnt
module matCalibChargeOnCancel {
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
    ENABLE_CRASH = true,
    ENABLE_STALE_TENURE = false,
    ENABLE_RESOLVE_FAULTS = false
  ).* from "../materializationJob"

  /// PRE-FIX cancel: the job resolves cancelled, the view clears, but
  /// the open attempt SURVIVES (everything else verbatim from
  /// cancelOnZeroInterest).
  action calibCancelLeavesAttemptOpen(d: str): bool = all {
    hasUnresolvedJob(d),
    memInterest(d) == Set(),
    jobs' = jobs.put(d, match jobs.get(d) {
      | NoJob => NoJob
      | SomeJob(j) => SomeJob({ ...j, state: JCancelled })
    }),
    view' = view.put(d, VNone),
    // THE BUG: the attempt is NOT closed.
    attempts' = attempts,
    nodeStatus' = nodeStatus,
    topdownPruned' = topdownPruned.put(d, false),
    dupJob' = dupJob, execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
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
      // the as-built establishment (post-reversal: charges through
      // the park verdict) — its chargedAfterCancel ghost is the
      // detector.
      establishCrashedAttempt(d),
      // the override REPLACES cancelOnZeroInterest
      calibCancelLeavesAttemptOpen(d),
      obsoleteOnProduced(d),
      parkReevaluate(d),
      parkBackoffExpires(d),
      buildTerminal(b),
      builderPull(d, builder),
      dispatchFromSource(d),
      completeReadyFromStore(d),
      releasePins(d),
      upstreamChange(d, o),
      durableRelationChange(d, rel),
    }
  }
}
