// -*- mode: Bluespec; -*-
//
// Bughunt-wave A3 calibration override, the bug_282 shape: the
// transient-retry backoff was PRODUCED (handle_transient_failure set
// backoff_until) but enforced NOWHERE in the pull architecture — the
// spawn-intent pass emitted in-backoff Ready nodes and the kernel
// delivered to an immediate re-pull, so a transiently-failing
// derivation thrashed executors with zero pacing. The production fix
// is the spawn-edge conjunct in compute_spawn_intents plus
// PullRequest.build_backoff_expired consumed in admit_pull's
// (Build, None) arm.
//
// PRE-FIX behavior modeled: calibIntentArrivesIgnoringBackoff is
// intentArrives WITHOUT the backoff conjunct — an in-backoff intent
// re-enters the queue.
//
// PREDICTED: noSpawnIntentInsideBackoff falsifies — a queued intent
// inside its backoff window.
//
// Run (falsification):
//   quint verify --backend=tlc --main=ctrlCalib282BackoffIgnored \
//     --step=calibStep --invariant=noSpawnIntentInsideBackoff \
//     docs/spec/models/calibration/controller-282-backoff-ignored.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=ctrlCalib282BackoffIgnored \
//     --invariant=noSpawnIntentInsideBackoff \
//     docs/spec/models/calibration/controller-282-backoff-ignored.qnt
module ctrlCalib282BackoffIgnored {
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
    ENABLE_RETRY_BACKOFF = true,
    ENABLE_STREAK_CADENCE = false,
    STREAK_FLOOR_ENFORCED = false,
    STREAK_FLOOR = 2,
    WINDOW_COUPLED_FOLD = false,
    ENABLE_SKIP_SPAWN_FAILOPEN = false,
    SKIP_SPAWN_WITHHOLD = false
  ).* from "../spawnCoherence"

  /// PRE-FIX spawn edge: the backoff conjunct dropped (everything
  /// else verbatim from intentArrives).
  action calibIntentArrivesIgnoringBackoff(i: IntentId): bool = all {
    not(queued.get(i)),
    // THE BUG: no backoff check.
    queued' = queued.set(i, true),
    intentFp' = intentFp, job' = job, execr' = execr,
    leaderOld' = leaderOld, gate' = gate,
    obsLive' = obsLive,
    bookkeepingUnchanged,
  }

  action calibStep = {
    nondet i = INTENTS.oneOf()
    any {
      tick,
      // the override REPLACES intentArrives
      calibIntentArrivesIgnoringBackoff(i),
      intentLeaves(i),
      intentResolves(i),
      podStartsLagged(i),
      jobControllerSyncs(i),
      jobCompletes(i),
      jobFailsAtStart(i),
      ttlDeletes(i),
      finalizerCompletes(i),
      jobAges(i),
      execRegisters(i),
      execGetsWork(i),
      execGoesIdle(i),
      execVanishes(i),
      leaderAges,
      schedulerRestarts,
      dispatchedCleared(i),
      gatePublishes,
      gateUnarms,
      backoffStarts(i),
      backoffExpires(i),
    }
  }
}
