// -*- mode: Bluespec; -*-
//
// Calibration override / regression pin (bughunt fix wave): the
// kind-blind running surface (bug_144).
//
// PRE-FIX behavior modeled: RunningDerivation carries no kind — a live
// materialization mint gets the uniform build treatment (build display
// AND a log tail against an execution that never logs).
//
// Live-import action-only form (bughunt-2 slot 11, §4-R4): the module
// imports the live gwBuildResync and perturbs ONE decision — the
// kinded-mint rendering. Everything else (heal split, budget oracle,
// checkpoint freeze) is the live model's.
//
// PREDICTED falsification: kindedSurfaceAgrees — the wired
// expect-violation check pins this falsification permanently: if it
// ever stops falsifying, the model has drifted away from the defect
// class it documents.
//
// Run (falsification):
//   quint verify --backend=tlc --main=gwResyncKindBlind \
//     --step=calibStep --invariant=kindedSurfaceAgrees \
//     docs/spec/models/calibration/gwresync-kind-blind.qnt
// Baseline (as-built step, must HOLD): discharged by P3 pairing with
// the live exhaustive check (quint-gw-build-resync).
//
module gwResyncKindBlind {
  import gwBuildResync.* from "../gwBuildResync"

  /// PRE-FIX: a delivered materialization start rendered kind-blind —
  /// build display + tail. Organic delivered receipt: the budget and
  /// its oracle reset in lockstep, exactly like the live action; only
  /// the rendering decision differs.
  action calibStartMatKindBlind(d: Drv): bool = all {
    sched.get(d) == SNotStarted, build == BActive, outcome == GWatching,
    consuming,
    sched' = sched.set(d, SRunningMat),
    display' = display.set(d, DispBuild),
    tails' = tails.union(Set(d)),
    build' = build, pgRow' = pgRow,
    pendingResync' = pendingResync, outcome' = outcome,
    signalled' = signalled, snapshotOwed' = snapshotOwed,
    resyncStreak' = 0,
    cyclesSinceOrganic' = 0,
    frozenDisplay' = frozenDisplay, frozenTails' = frozenTails,
  }

  action calibStep = {
    nondet d = drvs.oneOf()
    any {
      step,
      calibStartMatKindBlind(d),
    }
  }
}
