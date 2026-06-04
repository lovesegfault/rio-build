// -*- mode: Bluespec; -*-
//
// A1 calibration, merged_bug_133 (discarded fence outcome → view
// removal).
//
// PRE-FIX behavior modeled: the consumption/cancel sites discarded the
// fenced resolve's outcome and removed the in-memory view entry
// UNCONDITIONALLY — a Fenced (deposed) or errored durable write left
// the job row pending while the view said None: the armed action
// strands behind an empty view (admission answers Gone; the store
// never re-claims; the cancel tick never re-attempts).
//
// PREDICTED: viewMatchesDurableUnresolved falsifies (view VNone with
// an unresolved durable row).
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalib133DiscardedOutcome \
//     --step=calibStep --invariant=viewMatchesDurableUnresolved \
//     docs/spec/models/calibration/mat-133-discarded-outcome.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalib133DiscardedOutcome \
//     --invariant=viewMatchesDurableUnresolved \
//     docs/spec/models/calibration/mat-133-discarded-outcome.qnt
module matCalib133DiscardedOutcome {
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
    ENABLE_RESOLVE_FAULTS = true,
    ENABLE_WORKER_ABORTED = false
  ).* from "../materializationJob"

  /// PRE-FIX: the fenced/errored resolve's view discard — the durable
  /// row is untouched (the write was refused) but the view entry is
  /// removed anyway.
  action calibResolveFencedDiscardsView(d: str): bool = all {
    hasUnresolvedJob(d),
    jobs' = jobs,
    view' = view.put(d, VNone),
    attempts' = attempts,
    nodeStatus' = nodeStatus,
    topdownPruned' = topdownPruned,
    dupJob' = dupJob,
    execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
    tenureUnchanged, ghostsUnchanged, a4Unchanged,
  }

  action calibStep = {
    nondet d = DRVS.oneOf()
    any {
      step,
      calibResolveFencedDiscardsView(d),
    }
  }
}
