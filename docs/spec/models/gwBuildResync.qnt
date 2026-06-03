// -*- mode: Bluespec; -*-
//
// Gateway build-watch display under event loss (bughunt fix wave,
// bugs 144/150/153/323; spec rules gw.display.single-map,
// gw.resync.loss-signal, sched.pull.kinded-running-surface,
// sched.watch.terminal-from-durable-row).
//
// WHAT IS MODELED
//   Two derivations, one build, one watching gateway. The scheduler
//   runs each drv as a build or a store-side materialization and
//   reaches a build terminal; the broadcast ring between scheduler and
//   gateway is lossy (every emission nondeterministically delivers or
//   skips). The KNOWLEDGE of loss is the model's pivot:
//     - post-fix, a skip sets pendingResync (the bridge's in-stream
//       ResyncRequired signal — one per streak) and the gateway's
//       resync action (zero-backoff WatchBuild snapshot reconcile)
//       restores the WHOLE display from the kinded running set;
//     - the gateway display is ONE map keyed by drv whose value is the
//       display family (build activity vs substitute pair), and the
//       reconcile sweeps THE key set;
//     - a terminal that outlives the build's residency is answered
//       from the durable builds row (migration 087), never fabricated.
//
// THE PROPERTIES (all state invariants; the resync signal is what
// makes them non-vacuous — without it the gateway holds stale display
// state while believing the stream is whole):
//   kindedSurfaceAgrees (144) — absent a pending resync, every display
//     entry's family matches the scheduler's running kind.
//   noStuckDisplay (150) — absent a pending resync, every display
//     entry's drv is actually running.
//   tailCoverage (153) — absent a pending resync, the tail set is
//     exactly the running BUILD set (materializations never tail).
//   terminalVerdictNeverFabricated (323) — the gateway never reports a
//     fabricated outcome: terminal answers come from the live event,
//     the resident snapshot, or the durable row.
//
// CALIBRATIONS (expect-violation, one per as-shipped design):
//   gwresync-two-map (150)      — the reconcile only sweeps the build
//                                 family; a stale subst entry survives.
//   gwresync-no-signal (153)    — skips are silent (no pendingResync);
//                                 the gateway never learns.
//   gwresync-kind-blind (144)   — materializations get the uniform
//                                 build treatment (display + tail).
//   gwresync-no-pg-fallback (323) — no durable terminal row; a
//                                 post-cleanup attach fabricates.
//
// Tier-1 budget: 2 drvs * sched(4) * display(3) * tail(2) per drv,
// build(3) * pg(2) * outcome(3) * pendingResync(2) — thousands of
// states; TLC exhausts in seconds.

module gwBuildResync {
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
      display' = drvs.mapBy(d =>
        if (sched.get(d) == SRunningBuild) DispBuild
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
