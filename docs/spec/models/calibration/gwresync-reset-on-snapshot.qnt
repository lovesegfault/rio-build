// -*- mode: Bluespec; -*-
//
// Gateway-resync calibration, merged_bug_056 (storm half): the
// re-attach counter reset on the snapshot apply.
//
// PRE-FIX behavior modeled: the watch loop reset reconnect_attempts on
// ANY event from the stream — the snapshot a re-attach delivers
// included. A scheduler stuck serving snapshot → ResyncRequired every
// cycle refreshed MAX_RECONNECT forever at zero backoff (counter
// 0 → 1 → snapshot resets → 0 → …): the cap was vacuous for exactly
// the storm it existed to bound.
//
// PREDICTED: boundedResyncStreak falsifies — the gateway counter
// diverges from the cyclesSinceOrganic oracle on the first
// reset-on-snapshot cycle.
//
// Run (falsification):
//   quint verify --backend=tlc --main=gwResyncCalibResetOnSnapshot \
//     --step=calibStep --invariant=boundedResyncStreak \
//     docs/spec/models/calibration/gwresync-reset-on-snapshot.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=gwResyncCalibResetOnSnapshot \
//     --invariant=boundedResyncStreak \
//     docs/spec/models/calibration/gwresync-reset-on-snapshot.qnt
module gwResyncCalibResetOnSnapshot {
  import gwBuildResync.* from "../gwBuildResync"

  /// PRE-FIX: the snapshot apply ALSO resets the gateway's re-attach
  /// counter (reset-on-any-event). The oracle keeps counting — the
  /// honest cycles-since-organic — so the divergence is immediate.
  action calibSnapshotApplyWithReset = all {
    snapshotOwed, outcome == GWatching,
    if (build != BActive) all {
      outcome' = GGotTerminal,
      display' = drvs.mapBy(d => DispNone),
      tails' = Set(),
      frozenDisplay' = drvs.mapBy(d => DispNone),
      frozenTails' = Set(),
    } else all {
      outcome' = outcome,
      display' = drvs.mapBy(d =>
        if (sched.get(d) == SRunningBuild) DispBuild
        else if (sched.get(d) == SRunningMat) DispSubst
        else DispNone),
      tails' = drvs.filter(d => sched.get(d) == SRunningBuild),
      frozenDisplay' = drvs.mapBy(d =>
        if (sched.get(d) == SRunningBuild) DispBuild
        else if (sched.get(d) == SRunningMat) DispSubst
        else DispNone),
      frozenTails' = drvs.filter(d => sched.get(d) == SRunningBuild),
    },
    signalled' = false,
    snapshotOwed' = false,
    resyncStreak' = 0,
    cyclesSinceOrganic' = cyclesSinceOrganic,
    sched' = sched, build' = build, pgRow' = pgRow,
    pendingResync' = pendingResync,
  }

  action calibStep = any {
    step,
    calibSnapshotApplyWithReset,
  }
}
