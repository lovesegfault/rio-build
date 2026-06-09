// -*- mode: Bluespec; -*-
//
// Falsify twin, controller cancel family (bughunt-2 slot 5, bug_113).
// Live-import form over the LIVE spawnCoherence relation — only the
// INIT is overridden, to the mid-trace state the violation needs
// (random walk from cold init misses it: the prefix — publish, spawn,
// pull, cancel, teardown, finalize — is ~10 ordered steps among ~30
// enabled actions; 60k samples × 20 steps found nothing, while from
// this init the rust simulator violates in milliseconds. Same
// rationale as the closure-family run-check escapes, applied to init
// instead of budget).
//
// CALIB INIT = "the cancel just finished tearing down the old Job":
//   - i1's CANCELLED close sits in the recently_closed window
//     (closedCause = CCCancelled, windowExpires has not fired);
//   - the old Job is fully gone (JAbsent, finalizer done);
//   - the intent is RE-QUEUED (cancel + fast re-submit) and the gate
//     publishes it — the respawn is the next tick's spawn;
//   - no open attempt (ExecAbsent: the fresh pod has not pulled).
//
// PRE-FIX behavior modeled (ENABLE_CANCEL_GENERATION = false): the
// cause-only law — tick 1 respawns the deterministic name
// (jobPostdatesClose flips for i1), tick 2 cancel-selects the fresh
// Job (cause CANCELLED, no covering attempt — both true, both about
// the OLD attempt).
//
// PREDICTED: cancelNeverDeletesPostCloseJob falsifies in 2 ticks.
//
// Recorded violation seed (quint run, --backend=rust):
//   0x1819714b2c47fa51 — see the slot-5 wave-log entry.
//
// Run:
//   quint run --main=controllerCalib113RespawnCancel \
//     --init=calibInit --step=calibStep \
//     --invariant=cancelNeverDeletesPostCloseJob \
//     docs/spec/models/calibration/controller-113-respawn-cancel.qnt
module controllerCalib113RespawnCancel {
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
    ENABLE_CANCEL_CAUSE = true,
    ENABLE_YOUNG_LEADER_REAP = false,
    ENABLE_SYNTH_CLOSE = false,
    ENABLE_EXEC_PINNED = false,
    ENABLE_MULTI_POOL = false,
    ENABLE_STREAK_POOL_KEY = false,
    ENABLE_CANCEL_GENERATION = false,
    ENABLE_RETRY_BACKOFF = false,
    ENABLE_STREAK_CADENCE = false,
    STREAK_FLOOR_ENFORCED = false,
    STREAK_FLOOR = 2
  ).* from "../spawnCoherence"

  /// The post-cancel mid-trace state (every var assigned; deltas from
  /// the base init are the four commented lines).
  action calibInit = all {
    queued' = INTENTS.mapBy(i => i == "i1"),          // re-submitted
    intentFp' = INTENTS.mapBy(i => FpA),
    job' = INTENTS.mapBy(i => { phase: JAbsent, age: AgeFresh, fp: FpA, podRunning: false }),
    execr' = INTENTS.mapBy(i => ExecAbsent),
    leaderOld' = true,
    gate' = GArmed(Set("i1")),                        // republished
    dispatched' = Set(),
    prevQueuedSnap' = Set(),
    effects' = Set(),
    violations' = Set(),
    matJob' = INTENTS.mapBy(i => false),
    renderedFarDl' = INTENTS.mapBy(i => false),
    closedCause' = INTENTS.mapBy(i =>                 // close in window
      if (i == "i1") CCCancelled else CCNone),
    seenOpen' = INTENTS.mapBy(i => false),
    exhausted' = INTENTS.mapBy(i => false),
    streak' = INTENTS.mapBy(i => 0),
    streakWallAge' = INTENTS.mapBy(i => 0),
    ownGatedTicks' = INTENTS.mapBy(i => 0),
    jobPostdatesClose' = INTENTS.mapBy(i => false),   // old job gone
    pendingClose' = INTENTS.mapBy(i => false),
    obsLive' = INTENTS.mapBy(i => false),
    inBackoff' = INTENTS.mapBy(i => false),
  }

  action calibStep = step
}

/// HOLD counterpart from the SAME mid-trace init: with
/// ENABLE_CANCEL_GENERATION the respawned Job postdates the close and
/// is structurally unselectable — the pair (twin violates / hold
/// holds, identical init and step) is the calibration contract.
module controllerCalib113RespawnCancelHold {
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
    ENABLE_CANCEL_CAUSE = true,
    ENABLE_YOUNG_LEADER_REAP = false,
    ENABLE_SYNTH_CLOSE = false,
    ENABLE_EXEC_PINNED = false,
    ENABLE_MULTI_POOL = false,
    ENABLE_STREAK_POOL_KEY = false,
    ENABLE_CANCEL_GENERATION = true,
    ENABLE_RETRY_BACKOFF = false,
    ENABLE_STREAK_CADENCE = false,
    STREAK_FLOOR_ENFORCED = false,
    STREAK_FLOOR = 2
  ).* from "../spawnCoherence"

  action calibInit = all {
    queued' = INTENTS.mapBy(i => i == "i1"),
    intentFp' = INTENTS.mapBy(i => FpA),
    job' = INTENTS.mapBy(i => { phase: JAbsent, age: AgeFresh, fp: FpA, podRunning: false }),
    execr' = INTENTS.mapBy(i => ExecAbsent),
    leaderOld' = true,
    gate' = GArmed(Set("i1")),
    dispatched' = Set(),
    prevQueuedSnap' = Set(),
    effects' = Set(),
    violations' = Set(),
    matJob' = INTENTS.mapBy(i => false),
    renderedFarDl' = INTENTS.mapBy(i => false),
    closedCause' = INTENTS.mapBy(i =>
      if (i == "i1") CCCancelled else CCNone),
    seenOpen' = INTENTS.mapBy(i => false),
    exhausted' = INTENTS.mapBy(i => false),
    streak' = INTENTS.mapBy(i => 0),
    streakWallAge' = INTENTS.mapBy(i => 0),
    ownGatedTicks' = INTENTS.mapBy(i => 0),
    jobPostdatesClose' = INTENTS.mapBy(i => false),
    pendingClose' = INTENTS.mapBy(i => false),
    obsLive' = INTENTS.mapBy(i => false),
    inBackoff' = INTENTS.mapBy(i => false),
  }

  action calibStep = step
}
