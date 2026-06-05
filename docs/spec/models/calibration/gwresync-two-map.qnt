// -*- mode: Bluespec; -*-
//
// Calibration override / regression pin (bughunt fix wave): the
// pre-single-map gateway display (bug_150).
//
// PRE-FIX behavior modeled: the snapshot reconcile rebuilds only the
// build-family entries; substitute entries are never swept — a subst
// display whose drv finished during the detach SURVIVES the reconcile.
//
// Live-import action-only form (bughunt-2 slot 11, §4-R4): the module
// imports the live gwBuildResync and perturbs ONE decision — the owed
// snapshot's reconcile map. The signal/re-attach machinery and the
// checkpoint freeze are the live model's (the perturbed apply
// checkpoints the same surviving-subst surface it renders, faithful to
// one map of two never being iterated).
//
// PREDICTED falsification: noStuckDisplay.
//
// Run (falsification):
//   quint verify --backend=tlc --main=gwResyncTwoMap \
//     --step=calibStep --invariant=noStuckDisplay \
//     docs/spec/models/calibration/gwresync-two-map.qnt
// Baseline (as-built step, must HOLD): discharged by P3 pairing with
// the live exhaustive check (quint-gw-build-resync).
//
module gwResyncTwoMap {
  import gwBuildResync.* from "../gwBuildResync"

  /// PRE-FIX (two independent maps): the gone-reconcile iterates only
  /// the build map — substitute entries survive verbatim.
  action calibSnapshotApplyTwoMap = all {
    snapshotOwed, outcome == GWatching,
    build == BActive,
    outcome' = outcome,
    display' = drvs.mapBy(d =>
      if (display.get(d) == DispSubst) DispSubst
      else if (sched.get(d) == SRunningBuild) DispBuild
      else if (sched.get(d) == SRunningMat) DispSubst
      else DispNone),
    tails' = drvs.filter(d => sched.get(d) == SRunningBuild),
    frozenDisplay' = drvs.mapBy(d =>
      if (display.get(d) == DispSubst) DispSubst
      else if (sched.get(d) == SRunningBuild) DispBuild
      else if (sched.get(d) == SRunningMat) DispSubst
      else DispNone),
    frozenTails' = drvs.filter(d => sched.get(d) == SRunningBuild),
    signalled' = false,
    snapshotOwed' = false,
    resyncStreak' = resyncStreak,
    cyclesSinceOrganic' = cyclesSinceOrganic,
    sched' = sched, build' = build, pgRow' = pgRow,
    pendingResync' = pendingResync,
  }

  action calibStep = any {
    step,
    calibSnapshotApplyTwoMap,
  }
}
