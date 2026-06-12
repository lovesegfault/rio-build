// -*- mode: Bluespec; -*-
//
// Gateway build-watch display under event loss (bughunt fix wave,
// bugs 144/150/153/323; bughunt-2 wave, merged_bug_056; spec rules
// gw.display.single-map, gw.resync.loss-signal,
// gw.resync.reattach-budget, gw.resync.snapshot-owed,
// sched.pull.kinded-running-surface,
// sched.watch.terminal-from-durable-row).
//
// WHAT IS MODELED
//   Two derivations, one build, one watching gateway. The scheduler
//   runs each drv as a build or a store-side materialization and
//   reaches a build terminal; the broadcast ring between scheduler and
//   gateway is lossy (every emission nondeterministically delivers or
//   skips). The KNOWLEDGE of loss is the model's pivot:
//     - post-fix, a skip sets pendingResync (the bridge's in-stream
//       ResyncRequired signal — one per streak); the gateway's heal is
//       SPLIT into signal → failable re-attach → snapshot apply
//       (merged_bug_056): consuming the signal kills the old stream
//       (signalled), a re-attach can fail and is charged against a
//       budget, and only the owed snapshot's apply reconciles the
//       display and re-enters the live regime;
//     - the re-attach budget (resyncStreak) resets ONLY on organic
//       delivered events — never on the snapshot apply. (The code
//       adds one further lawful reset OUTSIDE this model's frame:
//       evidenced recovery, a wall-clock Live tenure exceeding the
//       backoff cap at the next death — bug_068, pinned by unit
//       tests at build.rs. This model has no wall clock; within its
//       frame the only-organic claim stands. Charging offset,
//       relocated from the retired gw-resync map and re-verified
//       against the as-built budget: the model charges ONE budget
//       cycle per heal cycle (reattachOk / reattachFailed); the
//       implementation also charges the cycle that CONSUMES the
//       loss signal (note_reattach on the stream error) — same
//       monotone accounting, off by a constant; the only-organic
//       property is unaffected. Post-bug_160/merged_bug_083
//       amendment: the as-built budget is now TWO-AXIS — this
//       model's resyncStreak is the streak axis; the wall-clock
//       rate-axis token bucket (organic-immune refill, every death
//       arm charged through the single next_backoff(cause)
//       chokepoint) is outside the untimed frame, pinned by the
//       build.rs ladder unit tests.) The oracle
//       seat cyclesSinceOrganic counts the same cycles from the live
//       actions; the invariant pins the gateway's counter to it, so a
//       "reset on snapshot" implementation diverges immediately;
//     - while signalled or snapshotOwed, the gateway is NOT consuming
//       a stream: delivered emissions in that window have no gateway
//       effect (the dead stream cannot carry them; the owed snapshot
//       covers them). The frozenDisplay/frozenTails checkpoint pins
//       the display to its signal-time value through the window;
//     - the gateway display is ONE map keyed by drv whose value is the
//       display family (build activity vs substitute pair), and the
//       reconcile sweeps THE key set;
//     - a terminal that outlives the build's residency is answered
//       from the durable builds row (migration 087), never fabricated.
//
// THE PROPERTIES (all state invariants; the resync signal is what
// makes them non-vacuous — without it the gateway holds stale display
// state while believing the stream is whole):
//   kindedSurfaceAgrees (144) — when the stream is whole, every display
//     entry's family matches the scheduler's running kind.
//   noStuckDisplay (150) — when the stream is whole, every display
//     entry's drv is actually running.
//   tailCoverage (153) — when the stream is whole, the tail set is
//     exactly the running BUILD set (materializations never tail).
//   terminalVerdictNeverFabricated (323) — the gateway never reports a
//     fabricated outcome: terminal answers come from the live event,
//     the resident snapshot, or the durable row.
//   boundedResyncStreak (merged_bug_056) — the gateway's re-attach
//     counter equals the oracle's cycles-since-organic count and stays
//     within MAX_STREAK; resetting it on the snapshot apply (the
//     shipped defect) diverges from the oracle on the first cycle.
//   snapshotOwedNoConsume (merged_bug_056) — while a re-attach or its
//     snapshot is owed, the display and tails are frozen at their
//     signal-time checkpoint: consuming events from the dead stream
//     cannot mutate them.
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
//   gwresync-reset-on-snapshot (056) — the snapshot apply resets the
//                                 re-attach counter; boundedResyncStreak
//                                 diverges from the oracle.
//   gwresync-consume-while-owed (056) — a delivered event mutates the
//                                 display while the snapshot is owed;
//                                 snapshotOwedNoConsume falsifies.
//
// Tier-1 budget: 2 drvs * sched(4) * display(3) * tail(2) per drv,
// build(3) * pg(2) * outcome(3) * pendingResync(2) * signalled(2) *
// snapshotOwed(2) * streak/oracle (lockstepped, <= MAX_STREAK+1) *
// frozen checkpoints (display-valued) — tens of thousands of states;
// TLC exhausts in seconds.

module gwBuildResync {
  type Drv = D1 | D2
  type SchedState = SNotStarted | SRunningBuild | SRunningMat | SDone
  type Display = DispNone | DispBuild | DispSubst
  type BuildState = BActive | BTerminalResident | BTerminalCleaned
  type GwOutcome = GWatching | GGotTerminal | GFabricated

  val drvs: Set[Drv] = Set(D1, D2)

  /// Re-attach cycles without an organic receipt before the gateway
  /// gives up (MAX_RECONNECT's model-scale stand-in; small to keep the
  /// state space Tier-1).
  val MAX_STREAK: int = 3

  var sched: Drv -> SchedState
  var build: BuildState
  var pgRow: bool
  var display: Drv -> Display
  var tails: Set[Drv]
  var pendingResync: bool
  var outcome: GwOutcome
  /// The gateway consumed the in-stream loss signal: the old stream is
  /// dead from this point until the owed snapshot applies.
  var signalled: bool
  /// A WatchBuild re-attach succeeded; the snapshot is owed as the new
  /// stream's first message and nothing else may be consumed first.
  var snapshotOwed: bool
  /// The GATEWAY's re-attach counter (the implementation artifact under
  /// test): incremented per cycle, reset only on organic receipts.
  var resyncStreak: int
  /// ORACLE SEAT: cycles since the last organic receipt, written by the
  /// same live actions. boundedResyncStreak pins resyncStreak to it.
  var cyclesSinceOrganic: int
  /// Display/tails checkpoint taken when the loss signal is consumed:
  /// while signalled or snapshotOwed the live display must equal it.
  var frozenDisplay: Drv -> Display
  var frozenTails: Set[Drv]

  /// The gateway is consuming a live stream (not mid-heal).
  val consuming: bool = and { not(signalled), not(snapshotOwed) }

  action init = all {
    sched' = drvs.mapBy(d => SNotStarted),
    build' = BActive,
    pgRow' = false,
    display' = drvs.mapBy(d => DispNone),
    tails' = Set(),
    pendingResync' = false,
    outcome' = GWatching,
    signalled' = false,
    snapshotOwed' = false,
    resyncStreak' = 0,
    cyclesSinceOrganic' = 0,
    frozenDisplay' = drvs.mapBy(d => DispNone),
    frozenTails' = Set(),
  }

  // ---- scheduler-side emissions, each delivered or skipped ----
  //
  // Delivered variants require `consuming` — the gateway only consumes
  // a LIVE stream. While signalled/snapshotOwed an emission can only
  // take the Skipped shape (the dead stream cannot carry it; the owed
  // snapshot reconcile covers it, so the bridge's pendingResync is not
  // re-armed in that window either). Organic delivered receipts reset
  // BOTH the gateway streak and the oracle (lockstep).

  // A drv starts as a BUILD. Delivered: the gateway opens the build
  // display + tail. Skipped: the bridge signals (pendingResync).
  action startBuildDelivered(d: Drv): bool = all {
    sched.get(d) == SNotStarted, build == BActive, outcome == GWatching,
    consuming,
    sched' = sched.set(d, SRunningBuild),
    display' = display.set(d, DispBuild),
    tails' = tails.union(Set(d)),
    build' = build, pgRow' = pgRow,
    pendingResync' = pendingResync, outcome' = outcome,
    signalled' = signalled, snapshotOwed' = snapshotOwed,
    resyncStreak' = 0,
    cyclesSinceOrganic' = 0,
    frozenDisplay' = frozenDisplay, frozenTails' = frozenTails,
  }
  action startBuildSkipped(d: Drv): bool = all {
    sched.get(d) == SNotStarted, build == BActive, outcome == GWatching,
    sched' = sched.set(d, SRunningBuild),
    display' = display, tails' = tails,
    build' = build, pgRow' = pgRow,
    pendingResync' = or { pendingResync, consuming },
    outcome' = outcome,
    signalled' = signalled, snapshotOwed' = snapshotOwed,
    resyncStreak' = resyncStreak,
    cyclesSinceOrganic' = cyclesSinceOrganic,
    frozenDisplay' = frozenDisplay, frozenTails' = frozenTails,
  }

  // A drv starts as a MATERIALIZATION (kinded event path: substitute
  // display, NO tail).
  action startMatDelivered(d: Drv): bool = all {
    sched.get(d) == SNotStarted, build == BActive, outcome == GWatching,
    consuming,
    sched' = sched.set(d, SRunningMat),
    display' = display.set(d, DispSubst),
    tails' = tails,
    build' = build, pgRow' = pgRow,
    pendingResync' = pendingResync, outcome' = outcome,
    signalled' = signalled, snapshotOwed' = snapshotOwed,
    resyncStreak' = 0,
    cyclesSinceOrganic' = 0,
    frozenDisplay' = frozenDisplay, frozenTails' = frozenTails,
  }
  action startMatSkipped(d: Drv): bool = all {
    sched.get(d) == SNotStarted, build == BActive, outcome == GWatching,
    sched' = sched.set(d, SRunningMat),
    display' = display, tails' = tails,
    build' = build, pgRow' = pgRow,
    pendingResync' = or { pendingResync, consuming },
    outcome' = outcome,
    signalled' = signalled, snapshotOwed' = snapshotOwed,
    resyncStreak' = resyncStreak,
    cyclesSinceOrganic' = cyclesSinceOrganic,
    frozenDisplay' = frozenDisplay, frozenTails' = frozenTails,
  }

  // A running drv finishes. Delivered: the terminal arm closes
  // whichever display family is open and the tail.
  action finishDelivered(d: Drv): bool = all {
    or { sched.get(d) == SRunningBuild, sched.get(d) == SRunningMat },
    build == BActive, outcome == GWatching,
    consuming,
    sched' = sched.set(d, SDone),
    display' = display.set(d, DispNone),
    tails' = tails.exclude(Set(d)),
    build' = build, pgRow' = pgRow,
    pendingResync' = pendingResync, outcome' = outcome,
    signalled' = signalled, snapshotOwed' = snapshotOwed,
    resyncStreak' = 0,
    cyclesSinceOrganic' = 0,
    frozenDisplay' = frozenDisplay, frozenTails' = frozenTails,
  }
  action finishSkipped(d: Drv): bool = all {
    or { sched.get(d) == SRunningBuild, sched.get(d) == SRunningMat },
    build == BActive, outcome == GWatching,
    sched' = sched.set(d, SDone),
    display' = display, tails' = tails,
    build' = build, pgRow' = pgRow,
    pendingResync' = or { pendingResync, consuming },
    outcome' = outcome,
    signalled' = signalled, snapshotOwed' = snapshotOwed,
    resyncStreak' = resyncStreak,
    cyclesSinceOrganic' = cyclesSinceOrganic,
    frozenDisplay' = frozenDisplay, frozenTails' = frozenTails,
  }

  // The build reaches terminal: capture-at-transition persists the
  // durable row (087) ATOMICALLY with the terminal state. Delivered:
  // the gateway short-circuits to the outcome.
  action buildTerminalDelivered = all {
    drvs.forall(d => sched.get(d) == SDone), build == BActive, outcome == GWatching,
    consuming,
    build' = BTerminalResident, pgRow' = true,
    sched' = sched, display' = drvs.mapBy(d => DispNone), tails' = Set(),
    pendingResync' = pendingResync,
    outcome' = GGotTerminal,
    signalled' = signalled, snapshotOwed' = snapshotOwed,
    resyncStreak' = 0,
    cyclesSinceOrganic' = 0,
    frozenDisplay' = frozenDisplay, frozenTails' = frozenTails,
  }
  action buildTerminalSkipped = all {
    drvs.forall(d => sched.get(d) == SDone), build == BActive, outcome == GWatching,
    build' = BTerminalResident, pgRow' = true,
    sched' = sched, display' = display, tails' = tails,
    pendingResync' = or { pendingResync, consuming },
    outcome' = outcome,
    signalled' = signalled, snapshotOwed' = snapshotOwed,
    resyncStreak' = resyncStreak,
    cyclesSinceOrganic' = cyclesSinceOrganic,
    frozenDisplay' = frozenDisplay, frozenTails' = frozenTails,
  }

  // Terminal cleanup evicts the resident build; the durable row stays.
  action cleanup = all {
    build == BTerminalResident,
    build' = BTerminalCleaned,
    sched' = sched, pgRow' = pgRow, display' = display, tails' = tails,
    pendingResync' = pendingResync, outcome' = outcome,
    signalled' = signalled, snapshotOwed' = snapshotOwed,
    resyncStreak' = resyncStreak,
    cyclesSinceOrganic' = cyclesSinceOrganic,
    frozenDisplay' = frozenDisplay, frozenTails' = frozenTails,
  }

  // ---- the gateway's heal, SPLIT (merged_bug_056) ----
  //
  // gw.resync.loss-signal+1 / gw.resync.snapshot-owed /
  // gw.resync.reattach-budget: the signal (pendingResync) is consumed
  // from the stream (signalDelivered — the old stream dies HERE and the
  // display freezes at its checkpoint), a fresh WatchBuild can fail
  // (reattachFailed) or succeed and owe the snapshot (reattachOk), and
  // only the snapshot apply reconciles and re-enters the live regime.
  // The budget charges per cycle and resets ONLY on organic receipts —
  // snapshotApply keeps it (the fix pivot). The code's second lawful
  // reset (evidenced recovery via wall-clock Live tenure, bug_068) is
  // outside this model's frame — see the header note.

  /// The gateway reads ResyncRequired off the live stream: the stream
  /// is dead from here, the display checkpoint is taken.
  action signalDelivered = all {
    pendingResync, consuming, outcome == GWatching,
    signalled' = true,
    pendingResync' = false,
    frozenDisplay' = display,
    frozenTails' = tails,
    sched' = sched, build' = build, pgRow' = pgRow,
    display' = display, tails' = tails, outcome' = outcome,
    snapshotOwed' = snapshotOwed,
    resyncStreak' = resyncStreak,
    cyclesSinceOrganic' = cyclesSinceOrganic,
  }

  /// A WatchBuild re-attach attempt succeeds: the snapshot is now owed.
  /// The cycle is charged (gateway counter AND oracle, lockstep).
  action reattachOk = all {
    signalled, not(snapshotOwed), outcome == GWatching,
    resyncStreak < MAX_STREAK,
    snapshotOwed' = true,
    resyncStreak' = resyncStreak + 1,
    cyclesSinceOrganic' = cyclesSinceOrganic + 1,
    sched' = sched, build' = build, pgRow' = pgRow,
    display' = display, tails' = tails,
    pendingResync' = pendingResync, outcome' = outcome,
    signalled' = signalled,
    frozenDisplay' = frozenDisplay, frozenTails' = frozenTails,
  }

  /// A WatchBuild re-attach attempt fails (open error, no snapshot, or
  /// the bounded first-message read timing out): stay signalled, charge
  /// the cycle.
  action reattachFailed = all {
    signalled, not(snapshotOwed), outcome == GWatching,
    resyncStreak < MAX_STREAK,
    resyncStreak' = resyncStreak + 1,
    cyclesSinceOrganic' = cyclesSinceOrganic + 1,
    sched' = sched, build' = build, pgRow' = pgRow,
    display' = display, tails' = tails,
    pendingResync' = pendingResync, outcome' = outcome,
    signalled' = signalled, snapshotOwed' = snapshotOwed,
    frozenDisplay' = frozenDisplay, frozenTails' = frozenTails,
  }

  /// The owed snapshot arrives as the re-attached stream's first
  /// message and reconciles EVERYTHING from the kinded running set; a
  /// terminal build short-circuits (resident snapshot or, post cleanup,
  /// the durable row — never fabrication). The budget is NOT reset —
  /// only organic receipts on the now-live stream do that.
  action snapshotApply = all {
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
    resyncStreak' = resyncStreak,
    cyclesSinceOrganic' = cyclesSinceOrganic,
    sched' = sched, build' = build, pgRow' = pgRow,
    pendingResync' = pendingResync,
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
      signalDelivered,
      reattachOk,
      reattachFailed,
      snapshotApply,
    }
  }

  // ---- invariants ----

  def running(d: Drv): bool =
    or { sched.get(d) == SRunningBuild, sched.get(d) == SRunningMat }

  /// The gateway believes the stream is whole: no pending signal, no
  /// dead stream, no owed snapshot.
  val streamWhole: bool =
    and { outcome == GWatching, not(pendingResync), consuming }

  // 144: display family matches the running kind whenever the gateway
  // believes the stream is whole.
  val kindedSurfaceAgrees =
    streamWhole implies
      drvs.forall(d => and {
        (display.get(d) == DispBuild) implies (sched.get(d) == SRunningBuild),
        (display.get(d) == DispSubst) implies (sched.get(d) == SRunningMat),
      })

  // 150: no display entry survives its drv's terminal whenever the
  // gateway believes the stream is whole.
  val noStuckDisplay =
    streamWhole implies
      drvs.forall(d => (display.get(d) != DispNone) implies running(d))

  // 153: tails are exactly the running builds whenever the gateway
  // believes the stream is whole.
  val tailCoverage =
    streamWhole implies
      drvs.forall(d => and {
        (sched.get(d) == SRunningBuild) implies tails.contains(d),
        (sched.get(d) == SRunningMat) implies not(tails.contains(d)),
      })

  // 323: the gateway never fabricates a terminal verdict (the durable
  // row answers post-cleanup attaches; in this post-fix model the
  // fabricating state is unreachable).
  val terminalVerdictNeverFabricated = outcome != GFabricated

  // merged_bug_056 (storm): the gateway's re-attach counter is honest —
  // it equals the oracle's cycles-since-organic count (so a reset on
  // the snapshot apply diverges on the first cycle) and stays within
  // MAX_STREAK.
  val boundedResyncStreak = and {
    resyncStreak == cyclesSinceOrganic,
    resyncStreak <= MAX_STREAK,
  }

  // merged_bug_056 (stale consume): while a re-attach or its snapshot
  // is owed, the display and tails are frozen at the signal-time
  // checkpoint — events from the dead stream cannot mutate them.
  val snapshotOwedNoConsume =
    (or { signalled, snapshotOwed }) implies
      and { display == frozenDisplay, tails == frozenTails }

  val gwBuildResyncInvariants = all {
    kindedSurfaceAgrees,
    noStuckDisplay,
    tailCoverage,
    terminalVerdictNeverFabricated,
    boundedResyncStreak,
    snapshotOwedNoConsume,
  }
}
