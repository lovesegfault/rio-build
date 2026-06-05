// -*- mode: Bluespec; -*-
//
// Calibration override / regression pin (bughunt fix wave): the
// signal-less lossy bridge (bug_153).
//
// PRE-FIX behavior modeled: a broadcast-lag skip is logged
// scheduler-side only; pendingResync is never set, so the snapshot
// reconcile is never driven — the gateway's surface rots while it
// believes the stream is whole.
//
// Live-import action-only form (bughunt-2 slot 11, §4-R4): the module
// imports the live gwBuildResync and perturbs ONE decision at the four
// skip sites — the signal arming. The heal machinery stays live (and
// stays unreachable on the perturbed paths: nothing arms it).
//
// PREDICTED falsification: tailCoverage — a skipped build start leaves
// a running build with no tail while the gateway believes the stream
// is whole.
//
// Run (falsification):
//   quint verify --backend=tlc --main=gwResyncNoSignal \
//     --step=calibStep --invariant=tailCoverage \
//     docs/spec/models/calibration/gwresync-no-signal.qnt
// Baseline (as-built step, must HOLD): discharged by P3 pairing with
// the live exhaustive check (quint-gw-build-resync).
//
module gwResyncNoSignal {
  import gwBuildResync.* from "../gwBuildResync"

  /// PRE-FIX: the skipped build start never signals.
  action calibStartBuildSkippedNoSignal(d: Drv): bool = all {
    sched.get(d) == SNotStarted, build == BActive, outcome == GWatching,
    sched' = sched.set(d, SRunningBuild),
    display' = display, tails' = tails,
    build' = build, pgRow' = pgRow,
    pendingResync' = pendingResync,
    outcome' = outcome,
    signalled' = signalled, snapshotOwed' = snapshotOwed,
    resyncStreak' = resyncStreak,
    cyclesSinceOrganic' = cyclesSinceOrganic,
    frozenDisplay' = frozenDisplay, frozenTails' = frozenTails,
  }

  /// PRE-FIX: the skipped materialization start never signals.
  action calibStartMatSkippedNoSignal(d: Drv): bool = all {
    sched.get(d) == SNotStarted, build == BActive, outcome == GWatching,
    sched' = sched.set(d, SRunningMat),
    display' = display, tails' = tails,
    build' = build, pgRow' = pgRow,
    pendingResync' = pendingResync,
    outcome' = outcome,
    signalled' = signalled, snapshotOwed' = snapshotOwed,
    resyncStreak' = resyncStreak,
    cyclesSinceOrganic' = cyclesSinceOrganic,
    frozenDisplay' = frozenDisplay, frozenTails' = frozenTails,
  }

  /// PRE-FIX: the skipped finish never signals.
  action calibFinishSkippedNoSignal(d: Drv): bool = all {
    or { sched.get(d) == SRunningBuild, sched.get(d) == SRunningMat },
    build == BActive, outcome == GWatching,
    sched' = sched.set(d, SDone),
    display' = display, tails' = tails,
    build' = build, pgRow' = pgRow,
    pendingResync' = pendingResync,
    outcome' = outcome,
    signalled' = signalled, snapshotOwed' = snapshotOwed,
    resyncStreak' = resyncStreak,
    cyclesSinceOrganic' = cyclesSinceOrganic,
    frozenDisplay' = frozenDisplay, frozenTails' = frozenTails,
  }

  /// PRE-FIX: the skipped terminal never signals.
  action calibBuildTerminalSkippedNoSignal = all {
    drvs.forall(d => sched.get(d) == SDone), build == BActive, outcome == GWatching,
    build' = BTerminalResident, pgRow' = true,
    sched' = sched, display' = display, tails' = tails,
    pendingResync' = pendingResync,
    outcome' = outcome,
    signalled' = signalled, snapshotOwed' = snapshotOwed,
    resyncStreak' = resyncStreak,
    cyclesSinceOrganic' = cyclesSinceOrganic,
    frozenDisplay' = frozenDisplay, frozenTails' = frozenTails,
  }

  action calibStep = {
    nondet d = drvs.oneOf()
    any {
      step,
      calibStartBuildSkippedNoSignal(d),
      calibStartMatSkippedNoSignal(d),
      calibFinishSkippedNoSignal(d),
      calibBuildTerminalSkippedNoSignal,
    }
  }
}
