// -*- mode: Bluespec; -*-
//
// Gateway-resync calibration, merged_bug_056 (stale-consume half): the
// dead stream stays bound after a failed re-attach.
//
// PRE-FIX behavior modeled: a failed WatchBuild left the old
// event_stream binding in place, and the next loop iteration
// re-entered process_build_events on it — a dead-but-buffered stream
// served events recorded before the loss signal as if the display
// were whole.
//
// PREDICTED: snapshotOwedNoConsume falsifies — a delivered event
// mutates the display while the snapshot is owed, diverging from the
// signal-time checkpoint.
//
// Run (falsification):
//   quint verify --backend=tlc --main=gwResyncCalibConsumeWhileOwed \
//     --step=calibStep --invariant=snapshotOwedNoConsume \
//     docs/spec/models/calibration/gwresync-consume-while-owed.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=gwResyncCalibConsumeWhileOwed \
//     --invariant=snapshotOwedNoConsume \
//     docs/spec/models/calibration/gwresync-consume-while-owed.qnt
module gwResyncCalibConsumeWhileOwed {
  import gwBuildResync.* from "../gwBuildResync"

  /// PRE-FIX: a start-build emission is consumed off the DEAD stream
  /// while the gateway is mid-heal (signalled or snapshot owed): the
  /// display mutates away from its signal-time checkpoint.
  action calibConsumeStaleStart(d: Drv): bool = all {
    sched.get(d) == SNotStarted, build == BActive, outcome == GWatching,
    or { signalled, snapshotOwed },
    sched' = sched.set(d, SRunningBuild),
    display' = display.set(d, DispBuild),
    tails' = tails.union(Set(d)),
    build' = build, pgRow' = pgRow,
    pendingResync' = pendingResync, outcome' = outcome,
    signalled' = signalled, snapshotOwed' = snapshotOwed,
    resyncStreak' = resyncStreak,
    cyclesSinceOrganic' = cyclesSinceOrganic,
    frozenDisplay' = frozenDisplay, frozenTails' = frozenTails,
  }

  action calibStep = {
    nondet d = drvs.oneOf()
    any {
      step,
      calibConsumeStaleStart(d),
    }
  }
}
