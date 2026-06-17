// -*- mode: Bluespec; -*-
//
// sh-021 calibration twin (ctrl.ephemeral.reap-terminal-absent): the
// never-pulled FOD ghost. Live-import form over the LIVE
// spawnCoherence relation — only INIT is overridden, to the mid-trace
// state the `jobDiesUnreported` action reaches (the EXACT sh-021
// shape: open Build attempt still busy on the scheduler ledger,
// `source_node` never reported, Job `Failed/BackoffLimitExceeded`,
// intent left the demand set when status→Running).
//
// CALIB INIT = "the unreported death just happened, intent already
// left `want`":
//   - i1's Job is JTerminal (the BackoffLimitExceeded shape);
//   - i1's `execr` is ExecBusy — the open attempt the mint wrote, the
//     controller never reported a binding (`source_node IS NULL`);
//   - i1 is NOT queued (status→Running emptied the fetcher pool's
//     `want` — the `wantEmpty` early-return arm of the bug);
//   - i2 cold (so the `AbsentFromDemand => continue` arm at the
//     non-empty-want sibling is exercised by the model's own
//     `intentArrives(i2)` from this init at depth 2).
//
// PRE-FIX behavior modeled (ENABLE_TERMINAL_ABSENT_REAP = false): the
// next `tick` walks the loop, classifies i1 as `AbsentFromDemand` on
// a complete view (or hits the `wantEmpty` early-return — both encoded
// as `terminalReaps` filtering `postGate` only and
// `terminalAbsentReaps = Set()`), and `continue`s. `ttlDeletes(i1)`
// then removes the Job silently with `execr` STILL ExecBusy — the
// 62min establishment-sweep ghost.
//
// PREDICTED: terminalAbsentReaped FALSIFIES at depth 1 (one `tick`).
//
// Run:
//   quint verify --backend=tlc --main=spawnCoherenceCalibSh021GhostedFod \
//     --init=calibInit --step=calibStep \
//     --invariant=terminalAbsentReaped \
//     docs/spec/models/calibration/spawnCoherence-calib-sh021-ghosted-fod.qnt
module spawnCoherenceCalibSh021GhostedFod {
  import spawnCoherence(
    IS_BUILDER = false,
    CRD_PRESENT = true,
    CEILING = 1,
    ENABLE_RPC_FAULT = false,
    ENABLE_LEASE_FAULT = false,
    ENABLE_STALE_POLL = false,
    ENABLE_MAT_JOBS = false,
    ENABLE_RENDERED_DEADLINE = false,
    ENABLE_EXHAUST_POISON = false,
    ENABLE_EXHAUST_PERSIST = false,
    ENABLE_CANCEL_ASBUILT = false,
    ENABLE_CANCEL_CAUSE = false,
    ENABLE_YOUNG_LEADER_REAP = false,
    ENABLE_SYNTH_CLOSE = false,
    ENABLE_EXEC_PINNED = false,
    ENABLE_MULTI_POOL = false,
    ENABLE_STREAK_POOL_KEY = false,
    ENABLE_CANCEL_GENERATION = false,
    ENABLE_RETRY_BACKOFF = false,
    ENABLE_STREAK_CADENCE = false,
    STREAK_FLOOR_ENFORCED = false,
    STREAK_FLOOR = 2,
    WINDOW_COUPLED_FOLD = false,
    ENABLE_SKIP_SPAWN_FAILOPEN = false,
    SKIP_SPAWN_WITHHOLD = false,
    ENABLE_HELD_MACHINERY = false,
    ENABLE_HELD_VISIBILITY = false,
    ENABLE_UNREPORTED_DEATH = true,
    ENABLE_TERMINAL_ABSENT_REAP = false
  ).* from "../spawnCoherence"

  /// The post-unreported-death mid-trace state (every var assigned;
  /// deltas from cold init are the three commented lines).
  action calibInit = all {
    queued' = INTENTS.mapBy(i => false),               // intent left
    intentFp' = INTENTS.mapBy(i => FpA),
    job' = INTENTS.mapBy(i => if (i == "i1")           // unreported death
      { phase: JTerminal, age: AgeFresh, fp: FpA, podRunning: false }
      else { phase: JAbsent, age: AgeFresh, fp: FpA, podRunning: false }),
    execr' = INTENTS.mapBy(i =>                        // ghosted open attempt
      if (i == "i1") ExecBusy else ExecAbsent),
    leaderOld' = true,
    gate' = GUnarmed,
    dispatched' = Set(),
    prevQueuedSnap' = Set(),
    effects' = Set(),
    violations' = Set(),
    matJob' = INTENTS.mapBy(i => false),
    renderedFarDl' = INTENTS.mapBy(i => false),
    closedCause' = INTENTS.mapBy(i => CCNone),
    seenOpen' = INTENTS.mapBy(i => false),
    exhausted' = INTENTS.mapBy(i => false),
    streak' = INTENTS.mapBy(i => 0),
    streakWallAge' = INTENTS.mapBy(i => 0),
    ownGatedTicks' = INTENTS.mapBy(i => 0),
    jobPostdatesClose' = INTENTS.mapBy(i => false),
    pendingClose' = INTENTS.mapBy(i => false),
    skipSpawned' = INTENTS.mapBy(i => false),
    obsLive' = INTENTS.mapBy(i => false),
    inBackoff' = INTENTS.mapBy(i => false),
  }

  action calibStep = step
}

/// HOLD counterpart from the SAME mid-trace init: with
/// ENABLE_TERMINAL_ABSENT_REAP the `terminalAbsentReaps` arm reaps i1
/// at the first `tick` (background delete + the synthesized close —
/// Model J's `reaped` membership IS the close abstraction, same as
/// `orphanPendingReaps`). terminalAbsentReaped HOLDS;
/// canReachTerminalAbsentReap is the non-vacuity witness.
module spawnCoherenceCalibSh021GhostedFodHold {
  import spawnCoherence(
    IS_BUILDER = false,
    CRD_PRESENT = true,
    CEILING = 1,
    ENABLE_RPC_FAULT = false,
    ENABLE_LEASE_FAULT = false,
    ENABLE_STALE_POLL = false,
    ENABLE_MAT_JOBS = false,
    ENABLE_RENDERED_DEADLINE = false,
    ENABLE_EXHAUST_POISON = false,
    ENABLE_EXHAUST_PERSIST = false,
    ENABLE_CANCEL_ASBUILT = false,
    ENABLE_CANCEL_CAUSE = false,
    ENABLE_YOUNG_LEADER_REAP = false,
    ENABLE_SYNTH_CLOSE = false,
    ENABLE_EXEC_PINNED = false,
    ENABLE_MULTI_POOL = false,
    ENABLE_STREAK_POOL_KEY = false,
    ENABLE_CANCEL_GENERATION = false,
    ENABLE_RETRY_BACKOFF = false,
    ENABLE_STREAK_CADENCE = false,
    STREAK_FLOOR_ENFORCED = false,
    STREAK_FLOOR = 2,
    WINDOW_COUPLED_FOLD = false,
    ENABLE_SKIP_SPAWN_FAILOPEN = false,
    SKIP_SPAWN_WITHHOLD = false,
    ENABLE_HELD_MACHINERY = false,
    ENABLE_HELD_VISIBILITY = false,
    ENABLE_UNREPORTED_DEATH = true,
    ENABLE_TERMINAL_ABSENT_REAP = true
  ).* from "../spawnCoherence"

  action calibInit = all {
    queued' = INTENTS.mapBy(i => false),
    intentFp' = INTENTS.mapBy(i => FpA),
    job' = INTENTS.mapBy(i => if (i == "i1")
      { phase: JTerminal, age: AgeFresh, fp: FpA, podRunning: false }
      else { phase: JAbsent, age: AgeFresh, fp: FpA, podRunning: false }),
    execr' = INTENTS.mapBy(i =>
      if (i == "i1") ExecBusy else ExecAbsent),
    leaderOld' = true,
    gate' = GUnarmed,
    dispatched' = Set(),
    prevQueuedSnap' = Set(),
    effects' = Set(),
    violations' = Set(),
    matJob' = INTENTS.mapBy(i => false),
    renderedFarDl' = INTENTS.mapBy(i => false),
    closedCause' = INTENTS.mapBy(i => CCNone),
    seenOpen' = INTENTS.mapBy(i => false),
    exhausted' = INTENTS.mapBy(i => false),
    streak' = INTENTS.mapBy(i => 0),
    streakWallAge' = INTENTS.mapBy(i => 0),
    ownGatedTicks' = INTENTS.mapBy(i => 0),
    jobPostdatesClose' = INTENTS.mapBy(i => false),
    pendingClose' = INTENTS.mapBy(i => false),
    skipSpawned' = INTENTS.mapBy(i => false),
    obsLive' = INTENTS.mapBy(i => false),
    inBackoff' = INTENTS.mapBy(i => false),
  }

  action calibStep = step
}
