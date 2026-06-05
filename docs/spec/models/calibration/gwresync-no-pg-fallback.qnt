// -*- mode: Bluespec; -*-
//
// Calibration override / regression pin (bughunt fix wave): the
// pre-durable-terminal watch attach (merged_bug_323).
//
// PRE-FIX behavior modeled, BOTH halves of the defect: (a) reaching
// the build terminal persisted NO durable row (pre-migration-087 — no
// capture-at-transition payload), and (b) a post-cleanup re-attach,
// finding neither the resident build nor a row, burned MAX_RECONNECT
// and FABRICATED a failure trace instead of answering from durable
// state.
//
// Live-import action-only form (bughunt-2 slot 11, §4-R4): the module
// imports the live gwBuildResync and perturbs exactly those two
// decisions — the terminal's durable write and the heal's post-cleanup
// arm. The signal/re-attach machinery is the live model's.
//
// PREDICTED falsification: terminalVerdictNeverFabricated.
//
// Run (falsification):
//   quint verify --backend=tlc --main=gwResyncNoPgFallback \
//     --step=calibStep --invariant=terminalVerdictNeverFabricated \
//     docs/spec/models/calibration/gwresync-no-pg-fallback.qnt
// Baseline (as-built step, must HOLD): discharged by P3 pairing with
// the live exhaustive check (quint-gw-build-resync).
//
module gwResyncNoPgFallback {
  import gwBuildResync.* from "../gwBuildResync"

  /// PRE-FIX (no migration-087 payload): the delivered terminal
  /// persists nothing durable.
  action calibBuildTerminalDeliveredNoRow = all {
    drvs.forall(d => sched.get(d) == SDone), build == BActive, outcome == GWatching,
    consuming,
    build' = BTerminalResident, pgRow' = false,
    sched' = sched, display' = drvs.mapBy(d => DispNone), tails' = Set(),
    pendingResync' = pendingResync,
    outcome' = GGotTerminal,
    signalled' = signalled, snapshotOwed' = snapshotOwed,
    resyncStreak' = 0,
    cyclesSinceOrganic' = 0,
    frozenDisplay' = frozenDisplay, frozenTails' = frozenTails,
  }

  /// PRE-FIX (no migration-087 payload): the skipped terminal
  /// persists nothing durable either; the bridge still signals.
  action calibBuildTerminalSkippedNoRow = all {
    drvs.forall(d => sched.get(d) == SDone), build == BActive, outcome == GWatching,
    build' = BTerminalResident, pgRow' = false,
    sched' = sched, display' = display, tails' = tails,
    pendingResync' = or { pendingResync, consuming },
    outcome' = outcome,
    signalled' = signalled, snapshotOwed' = snapshotOwed,
    resyncStreak' = resyncStreak,
    cyclesSinceOrganic' = cyclesSinceOrganic,
    frozenDisplay' = frozenDisplay, frozenTails' = frozenTails,
  }

  /// PRE-FIX: the owed snapshot's terminal arm finds a cleaned build
  /// and NO durable row — BuildNotFound; the gateway fabricates a
  /// failure trace instead of consulting durable state.
  action calibSnapshotApplyFabricates = all {
    snapshotOwed, outcome == GWatching,
    build != BActive,
    outcome' = if (build == BTerminalCleaned and not(pgRow)) GFabricated
               else GGotTerminal,
    display' = drvs.mapBy(d => DispNone),
    tails' = Set(),
    frozenDisplay' = drvs.mapBy(d => DispNone),
    frozenTails' = Set(),
    signalled' = false,
    snapshotOwed' = false,
    resyncStreak' = resyncStreak,
    cyclesSinceOrganic' = cyclesSinceOrganic,
    sched' = sched, build' = build, pgRow' = pgRow,
    pendingResync' = pendingResync,
  }

  action calibStep = any {
    step,
    calibBuildTerminalDeliveredNoRow,
    calibBuildTerminalSkippedNoRow,
    calibSnapshotApplyFabricates,
  }
}
