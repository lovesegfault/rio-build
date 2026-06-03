// -*- mode: Bluespec; -*-
//
// Calibration override / regression pin (bughunt fix wave): the pre-single-map gateway display (bug_150).
// PRE-FIX behavior modeled (bug_150): the snapshot reconcile rebuilds only the build-family entries; substitute entries are never swept.
// PREDICTED falsification: noStuckDisplay — the wired expect-violation
// check pins this falsification permanently: if it ever stops
// falsifying, the model has drifted away from the defect class it
// documents. Re-run:
//   quint run --invariant=noStuckDisplay docs/spec/models/calibration/<this file>
//
module gwResyncTwoMap {
  type Drv = D1 | D2
  type SchedState = SNotStarted | SRunningBuild | SRunningMat | SDone
  type Display = DispNone | DispBuild | DispSubst
  type BuildState = BActive | BTerminalResident | BTerminalCleaned
  type GwOutcome = GWatching | GGotTerminal | GFabricated

  val drvs: Set[Drv] = Set(D1, D2)

  var sched: Drv -> SchedState
  var build: BuildState
  var pgRow: bool
  var display: Drv -> Display
  var tails: Set[Drv]
  var pendingResync: bool
  var outcome: GwOutcome

  action init = all {
    sched' = drvs.mapBy(d => SNotStarted),
    build' = BActive,
    pgRow' = false,
    display' = drvs.mapBy(d => DispNone),
    tails' = Set(),
    pendingResync' = false,
    outcome' = GWatching,
  }

  // ---- scheduler-side emissions, each delivered or skipped ----

  // A drv starts as a BUILD. Delivered: the gateway opens the build
  // display + tail. Skipped: the bridge signals (pendingResync).
  action startBuildDelivered(d: Drv): bool = all {
    sched.get(d) == SNotStarted, build == BActive, outcome == GWatching,
    sched' = sched.set(d, SRunningBuild),
    display' = display.set(d, DispBuild),
    tails' = tails.union(Set(d)),
    build' = build, pgRow' = pgRow,
    pendingResync' = pendingResync, outcome' = outcome,
  }
  action startBuildSkipped(d: Drv): bool = all {
    sched.get(d) == SNotStarted, build == BActive, outcome == GWatching,
    sched' = sched.set(d, SRunningBuild),
    display' = display, tails' = tails,
    build' = build, pgRow' = pgRow,
    pendingResync' = true, outcome' = outcome,
  }

  // A drv starts as a MATERIALIZATION (kinded event path: substitute
  // display, NO tail).
  action startMatDelivered(d: Drv): bool = all {
    sched.get(d) == SNotStarted, build == BActive, outcome == GWatching,
    sched' = sched.set(d, SRunningMat),
    display' = display.set(d, DispSubst),
    tails' = tails,
    build' = build, pgRow' = pgRow,
    pendingResync' = pendingResync, outcome' = outcome,
  }
  action startMatSkipped(d: Drv): bool = all {
    sched.get(d) == SNotStarted, build == BActive, outcome == GWatching,
    sched' = sched.set(d, SRunningMat),
    display' = display, tails' = tails,
    build' = build, pgRow' = pgRow,
    pendingResync' = true, outcome' = outcome,
  }

  // A running drv finishes. Delivered: the terminal arm closes
  // whichever display family is open and the tail.
  action finishDelivered(d: Drv): bool = all {
    or { sched.get(d) == SRunningBuild, sched.get(d) == SRunningMat },
    build == BActive, outcome == GWatching,
    sched' = sched.set(d, SDone),
    display' = display.set(d, DispNone),
    tails' = tails.exclude(Set(d)),
    build' = build, pgRow' = pgRow,
    pendingResync' = pendingResync, outcome' = outcome,
  }
  action finishSkipped(d: Drv): bool = all {
    or { sched.get(d) == SRunningBuild, sched.get(d) == SRunningMat },
    build == BActive, outcome == GWatching,
    sched' = sched.set(d, SDone),
    display' = display, tails' = tails,
    build' = build, pgRow' = pgRow,
    pendingResync' = true, outcome' = outcome,
  }

  // The build reaches terminal: capture-at-transition persists the
  // durable row (087) ATOMICALLY with the terminal state. Delivered:
  // the gateway short-circuits to the outcome.
  action buildTerminalDelivered = all {
    drvs.forall(d => sched.get(d) == SDone), build == BActive, outcome == GWatching,
    build' = BTerminalResident, pgRow' = true,
    sched' = sched, display' = drvs.mapBy(d => DispNone), tails' = Set(),
    pendingResync' = pendingResync,
    outcome' = GGotTerminal,
  }
  action buildTerminalSkipped = all {
    drvs.forall(d => sched.get(d) == SDone), build == BActive, outcome == GWatching,
    build' = BTerminalResident, pgRow' = true,
    sched' = sched, display' = display, tails' = tails,
    pendingResync' = true,
    outcome' = outcome,
  }

  // Terminal cleanup evicts the resident build; the durable row stays.
  action cleanup = all {
    build == BTerminalResident,
    build' = BTerminalCleaned,
    sched' = sched, pgRow' = pgRow, display' = display, tails' = tails,
    pendingResync' = pendingResync, outcome' = outcome,
  }

  // ---- the gateway's heal: ResyncRequired -> snapshot reconcile ----
  //
  // gw.resync.loss-signal: the signal (pendingResync) drives a fresh
  // WatchBuild whose snapshot reconcile recovers EVERYTHING — the
  // kinded running set rebuilds the single display map and the tail
  // set; a terminal build short-circuits (resident snapshot or, post
  // cleanup, the durable row — never fabrication).
  action gatewayResync = all {
    pendingResync, outcome == GWatching,
    if (build != BActive) all {
      outcome' = GGotTerminal,
      display' = drvs.mapBy(d => DispNone),
      tails' = Set(),
    } else all {
      outcome' = outcome,
      // PRE-FIX (two independent maps): the gone-reconcile iterates
      // only the build map — a substitute entry whose drv finished
      // during the detach SURVIVES the reconcile.
      display' = drvs.mapBy(d =>
        if (display.get(d) == DispSubst) DispSubst
        else if (sched.get(d) == SRunningBuild) DispBuild
        else if (sched.get(d) == SRunningMat) DispSubst
        else DispNone),
      tails' = drvs.filter(d => sched.get(d) == SRunningBuild),
    },
    pendingResync' = false,
    sched' = sched, build' = build, pgRow' = pgRow,
  }

  action step = {
    nondet d = drvs.oneOf()
    any {
      startBuildDelivered(d), startBuildSkipped(d),
      startMatDelivered(d), startMatSkipped(d),
      finishDelivered(d), finishSkipped(d),
      buildTerminalDelivered,
      buildTerminalSkipped,
      cleanup,
      gatewayResync,
    }
  }

  // ---- invariants ----

  def running(d: Drv): bool =
    or { sched.get(d) == SRunningBuild, sched.get(d) == SRunningMat }

  // 144: display family matches the running kind whenever the gateway
  // believes the stream is whole.
  val kindedSurfaceAgrees =
    (outcome == GWatching and not(pendingResync)) implies
      drvs.forall(d => and {
        (display.get(d) == DispBuild) implies (sched.get(d) == SRunningBuild),
        (display.get(d) == DispSubst) implies (sched.get(d) == SRunningMat),
      })

  // 150: no display entry survives its drv's terminal whenever the
  // gateway believes the stream is whole.
  val noStuckDisplay =
    (outcome == GWatching and not(pendingResync)) implies
      drvs.forall(d => (display.get(d) != DispNone) implies running(d))

  // 153: tails are exactly the running builds whenever the gateway
  // believes the stream is whole.
  val tailCoverage =
    (outcome == GWatching and not(pendingResync)) implies
      drvs.forall(d => and {
        (sched.get(d) == SRunningBuild) implies tails.contains(d),
        (sched.get(d) == SRunningMat) implies not(tails.contains(d)),
      })

  // 323: the gateway never fabricates a terminal verdict (the durable
  // row answers post-cleanup attaches; in this post-fix model the
  // fabricating state is unreachable).
  val terminalVerdictNeverFabricated = outcome != GFabricated

  val gwBuildResyncInvariants = all {
    kindedSurfaceAgrees,
    noStuckDisplay,
    tailCoverage,
    terminalVerdictNeverFabricated,
  }
}
