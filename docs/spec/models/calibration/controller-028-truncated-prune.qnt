// bug_028 falsify twin (bughunt-5 S2): the as-built AD2 gate truncated
// the wanted set with take(headroom) BEFORE the partition, so the
// streak fold's evaluated set was the spawn WINDOW — and the prune
// read "absent from gated" as observed recovery. Under priority churn
// on a ceiling'd pool a genuinely exhausted intent oscillates across
// the cutoff: each completing fold (an exhausted intent IS in the
// window) resets the out-of-window intent's live streak, so the
// firing law (count >= 3 AND the wall floor) never holds and the
// poison report that would un-wedge the scheduler is livelocked.
//
// This twin binds WINDOW_COUPLED_FOLD=true (the as-built law: window
// cut first, fold domain = window, gated intents burn slots, reports
// in-window only, out-of-window live streaks reset by a completing
// fold) over the exhaust-persist alphabet at CEILING=1. The wipe
// trace: i2 alone gates twice (streak 2); i1 arrives exhausted and
// outranks i2 for the single window slot; the fold completes on i1
// and RESETS i2's live streak — VForeignStreakWipe latches (the
// reset-without-observation family the m117 wipe shares) and
// persistentExhaustionEventuallyReports is expected [violation].
//
// The HOLD half is NOT a new regime: the fixed evaluated-relative law
// is the law the existing exhaust-persist / multipool-hold /
// streak-cadence regimes already verify (the model's gate order was
// always pre-truncation); they stay green and unmodified with the
// axis bound false (round-4 SS4.F16 byte-identity discipline,
// bughunt4-fix-specs.md).
//
module controller028TruncatedPrune {
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
    ENABLE_STREAK_CADENCE = false,
    STREAK_FLOOR_ENFORCED = false,
    STREAK_FLOOR = 2,
    WINDOW_COUPLED_FOLD = true,
    ENABLE_SKIP_SPAWN_FAILOPEN = false,
    SKIP_SPAWN_WITHHOLD = false,
    ENABLE_HELD_MACHINERY = false,
    ENABLE_HELD_VISIBILITY = false,
    ENABLE_UNREPORTED_DEATH = false,
    ENABLE_TERMINAL_ABSENT_REAP = false
  ).* from "../spawnCoherence"
}
