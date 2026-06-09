// m073 falsify twin (bughunt-4 S1a): the as-built exhaustion verdict
// tracked no wall clock — PoolStreaks counted reconcile SEQUENCES, so
// a sub-second reconcile burst (informer churn re-queueing the pool)
// stepped the streak to the persistence threshold and fired the
// NoEligibleSource poison verdict inside one wall instant, parking a
// pool that had been "exhausted" for milliseconds.
//
// This twin binds ENABLE_STREAK_CADENCE=true (each streak's wall age
// is tracked from its first gated observation) but
// STREAK_FLOOR_ENFORCED=false (the verdict stays floor-blind — the
// as-built law). A burst trace (wallAdv=0 ticks) reaches the
// threshold with streakWallAge < STREAK_FLOOR and the VPoisonBurst
// latch fires: poisonRespectsWallFloor is expected [violation].
// The HOLD half (STREAK_FLOOR_ENFORCED=true) rides the in-file
// spawnCoherenceStreakCadence regime — the floor conjunct is the SAME
// predicate the latch tests, so enforcement empties the latch by
// construction.
//
// quint-policy-latches: violations
module controller073Burst {
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
    ENABLE_MULTI_POOL = true,
    ENABLE_STREAK_POOL_KEY = true,
    ENABLE_CANCEL_GENERATION = false,
    ENABLE_RETRY_BACKOFF = false,
    ENABLE_STREAK_CADENCE = true,
    STREAK_FLOOR_ENFORCED = false,
    STREAK_FLOOR = 2
  ).* from "../spawnCoherence"
}
