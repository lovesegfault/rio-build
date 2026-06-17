// -*- mode: Bluespec; -*-
//
// sh-037 calibration twin (ctrl.ephemeral.reap-excess-pending+4): the
// excess-Pending thrash on a wide pull burst. Live-import form over
// the LIVE spawnCoherence relation — only INIT is overridden, to the
// mid-trace state the iter8 wide quint Ready-front reaches (the
// EXACT sh-037 §mechanism step-3/4 shape: queued has dropped to 0
// the instant pulled work left the Ready set; i1's Job is
// `JobStatus.ready==0`-Pending past the grace; i1's pod has already
// pulled — `execr=ExecBusy` — but the informer-cached `ready` still
// lags; the live pod-phase re-check is one-shot and `m0gz0cn814dw`
// was reaped 104 ms after a live-Running skip).
//
// CALIB INIT = "the pull burst just drained the Ready set; i1 is
// covered but its Job still reads Pending":
//   - i1's Job is JPending, AgePastPending, podRunning=false (the
//     informer-cached `ready==0` lag);
//   - i1's `execr` is ExecBusy — the open pull-mode attempt the mint
//     wrote (`covered_by_open_pull_attempt` would match);
//   - i1 is NOT queued (pulled work left the Ready set → `queued=0`);
//   - gate is GArmed and leaderOld is true (so `queuedKnown` holds and
//     the orphan/leader gates don't suppress);
//   - i2 cold (so the bound is `pending=1 − queued=0 = 1` → i1 is the
//     oldest excess).
//
// PRE-FIX behavior modeled (ENABLE_EXCESS_ATTEMPT_FILTER = false): the
// next `tick` walks `excessCandidates` without the
// `execr.get(i) != ExecBusy` conjunct, selects i1 (Pending, past
// grace, queued=0), `podRunning=false` (the lag) → `excessReaped`
// contains i1 with `execr(i1)=ExecBusy` → `vExcessCovered` →
// `VExcessReapCovered` latched.
//
// PREDICTED: noExcessReapOfCoveredJob FALSIFIES at depth 1 (one `tick`).
//
// Run:
//   quint verify --backend=tlc --main=spawnCoherenceCalibSh037ExcessThrash \
//     --init=calibInit --step=calibStep \
//     --invariant=noExcessReapOfCoveredJob \
//     docs/spec/models/calibration/controller-sh037-excess-thrash.qnt
module spawnCoherenceCalibSh037ExcessThrash {
  import spawnCoherence(
    IS_BUILDER = true,
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
    ENABLE_UNREPORTED_DEATH = false,
    ENABLE_TERMINAL_ABSENT_REAP = false,
    ENABLE_EXCESS_ATTEMPT_FILTER = false
  ).* from "../spawnCoherence"

  /// The post-pull-burst mid-trace state (every var assigned; deltas
  /// from cold init are the four commented lines).
  action calibInit = all {
    queued' = INTENTS.mapBy(i => false),               // pulled work left Ready
    intentFp' = INTENTS.mapBy(i => FpA),
    job' = INTENTS.mapBy(i => if (i == "i1")           // ready==0 lag
      { phase: JPending, age: AgePastPending, fp: FpA, podRunning: false }
      else { phase: JAbsent, age: AgeFresh, fp: FpA, podRunning: false }),
    execr' = INTENTS.mapBy(i =>                        // open pull-mode attempt
      if (i == "i1") ExecBusy else ExecAbsent),
    leaderOld' = true,
    gate' = GArmed((Set(), Set())),                    // queuedKnown holds
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
/// ENABLE_EXCESS_ATTEMPT_FILTER the `excessCandidates` filter
/// excludes i1 (`execr(i1)=ExecBusy`) → `excessReaped` is empty →
/// noExcessReapOfCoveredJob HOLDS. canReachExcessReap stays violable
/// in base via a Pending Job with `execr=ExecAbsent` (the witness is
/// re-verified there).
module spawnCoherenceCalibSh037ExcessThrashHold {
  import spawnCoherence(
    IS_BUILDER = true,
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
    ENABLE_UNREPORTED_DEATH = false,
    ENABLE_TERMINAL_ABSENT_REAP = false,
    ENABLE_EXCESS_ATTEMPT_FILTER = true
  ).* from "../spawnCoherence"

  action calibInit = all {
    queued' = INTENTS.mapBy(i => false),
    intentFp' = INTENTS.mapBy(i => FpA),
    job' = INTENTS.mapBy(i => if (i == "i1")
      { phase: JPending, age: AgePastPending, fp: FpA, podRunning: false }
      else { phase: JAbsent, age: AgeFresh, fp: FpA, podRunning: false }),
    execr' = INTENTS.mapBy(i =>
      if (i == "i1") ExecBusy else ExecAbsent),
    leaderOld' = true,
    gate' = GArmed((Set(), Set())),
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
