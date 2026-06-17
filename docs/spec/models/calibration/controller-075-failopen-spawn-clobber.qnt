// bug_075 falsify twin (bughunt-6 S4): the as-built jobs.rs ListFailed
// arm spawned every wanted intent fail-open — including a mid-streak
// gated intent. The spawned Job's deterministic name enters the
// existing-name skip set, the intent leaves the fold's evaluated set,
// and its LIVE exhaustion streak dies while job-held (in the code: the
// 120 s orphan expiry vs the >=180 s Job-alive floor; in the model:
// the next completed fold's job-held reset). One transient node-LIST
// blip destroys the evidence the retain law preserved and livelocks
// the NoEligibleSource poison report that would un-wedge the
// scheduler.
//
// This twin binds ENABLE_SKIP_SPAWN_FAILOPEN=true (the REAL fail-open
// skip arm + the clobber ghost/latch) with SKIP_SPAWN_WITHHOLD=false
// (the as-built law: no evidence consultation on skip ticks).
// Trajectory: streak builds on completed folds -> a fold-skip tick
// spawns the live-streak intent (ESkipFailopenSpawn in the trace —
// the skip-tick spawn is trace-visible, not inferred) -> the next
// completed fold zeroes the still-exhausted, still-wanted, job-held
// intent's streak -> VSpawnClobberedStreak latches and
// persistentExhaustionEventuallyReports is expected [violation].
//
// Witness-strength / kill-isolation (R16): in THIS lane the witness's
// only other defeater, VForeignStreakWipe, is structurally
// unreachable — WINDOW_COUPLED_FOLD=false (the 028 window wipe, whose
// own twin is calibration/controller-028-truncated-prune.qnt) and
// ENABLE_MULTI_POOL=false (the m117 foreign wipe) — so a violation
// here certifies the bug_075 skip-spawn clobber path and nothing
// else. The HOLD half (SKIP_SPAWN_WITHHOLD=true) rides the in-file
// spawnCoherenceFailopenWithhold regime: the withhold conjunct is the
// SAME evidence predicate the latch's ghost records, so enforcement
// empties the latch by construction.
//
// The floor-blind cadence frame (ENABLE_STREAK_CADENCE=true,
// STREAK_FLOOR_ENFORCED=false) is load-bearing: foldSkip — the
// model's fold-skip tick — is only exercised there (the m073 nondet
// gate).
//
module controller075FailopenSpawnClobber {
  import spawnCoherence(
    IS_BUILDER = true,
    CRD_PRESENT = true,
    CEILING = 1,
    ENABLE_RPC_FAULT = false,
    ENABLE_LEASE_FAULT = false,
    ENABLE_STALE_POLL = false,
    ENABLE_MAT_JOBS = false,
    ENABLE_RENDERED_DEADLINE = false,
    ENABLE_EXHAUST_POISON = true,
    ENABLE_EXHAUST_PERSIST = true,
    ENABLE_CANCEL_ASBUILT = false,
    ENABLE_CANCEL_CAUSE = false,
    ENABLE_YOUNG_LEADER_REAP = false,
    ENABLE_SYNTH_CLOSE = false,
    ENABLE_EXEC_PINNED = false,
    ENABLE_MULTI_POOL = false,
    ENABLE_STREAK_POOL_KEY = false,
    ENABLE_CANCEL_GENERATION = false,
    ENABLE_RETRY_BACKOFF = false,
    ENABLE_STREAK_CADENCE = true,
    STREAK_FLOOR_ENFORCED = false,
    STREAK_FLOOR = 2,
    WINDOW_COUPLED_FOLD = false,
    ENABLE_SKIP_SPAWN_FAILOPEN = true,
    SKIP_SPAWN_WITHHOLD = false,
    ENABLE_HELD_MACHINERY = false,
    ENABLE_HELD_VISIBILITY = false,
    ENABLE_UNREPORTED_DEATH = false,
    ENABLE_TERMINAL_ABSENT_REAP = false
  ).* from "../spawnCoherence"
}
