// -*- mode: Bluespec; -*-
//
// A1 calibration, merged_bug_276 (split cancel leaks the open
// attempt).
//
// PRE-FIX behavior modeled: the zero-interest cancel resolved the job
// row but closed the open attempt only through the in-memory DAG
// node's exec_id — absent node (its own trigger arm's `None => true`
// case), no close: the attempt LEAKS. The establishment sweep later
// converts the leak into a materialization_infra charge (and re-arms
// the cancelled job to pending).
//
// PREDICTED: chargeFreeCancellation falsifies (chargedAfterCancel
// latch — a charge landed on a drv whose job was no longer
// unresolved).
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalib276DagAbsentCancel \
//     --step=calibStep --invariant=chargeFreeCancellation \
//     docs/spec/models/calibration/mat-276-dag-absent-cancel.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalib276DagAbsentCancel \
//     --invariant=chargeFreeCancellation \
//     docs/spec/models/calibration/mat-276-dag-absent-cancel.qnt
module matCalib276DagAbsentCancel {
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

  /// PRE-FIX: the SPLIT cancel — the job resolves cancelled, the view
  /// clears, but the open attempt stays (no DAG node to read the
  /// exec_id from). The main model's establishCrashedAttempt then
  /// fires on the leaked attempt.
  action calibSplitCancel(d: str): bool = all {
    hasUnresolvedJob(d),
    memInterest(d) == Set(),
    jobs' = jobs.put(d, match jobs.get(d) {
      | NoJob => NoJob
      | SomeJob(j) => SomeJob({ ...j, state: JCancelled })
    }),
    view' = view.put(d, VNone),
    attempts' = attempts, // THE LEAK: the open attempt survives
    nodeStatus' = nodeStatus,
    topdownPruned' = topdownPruned.put(d, false),
    dupJob' = dupJob,
    execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
    tenureUnchanged, ghostsUnchanged,
  }

  action calibStep = {
    nondet d = DRVS.oneOf()
    any {
      step,
      calibSplitCancel(d),
    }
  }
}
