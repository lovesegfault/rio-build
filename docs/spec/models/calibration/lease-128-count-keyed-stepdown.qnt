// -*- mode: Bluespec; -*-
//
// Calibration override / regression pin, bughunt-4 S2 merged_bug_128
// (rio-lease stale step-down demotion): re-introduces the COUNT-KEYED
// world — the pre-fix production code in which the cooperative
// step-down stamp was the lease transition count (which a false-alarm
// fence + same-epoch re-acquire legitimately REPEATS) and nothing
// cleared a pending request on the acquire edge.
//
// PRE-FIX behavior modeled (one const, no step surgery):
// STEP_DOWN_COUNT_KEYED = true stamps requests with the generation-
// derived count (gen is fetch_max-derived from leaseTransitions, so
// the same-epoch re-acquire repeats it exactly as production
// acquired_transitions did) and skips the acquire-edge clear in
// steal/renewLease.
//
// PREDICTED falsification: noStaleStepDownServed. Schedule shape:
// acquire (instance 1) -> requestStepDown (stamp = count, inst 1) ->
// selfFence (false alarm; belief drops, the request survives) ->
// apiGet -> renewLease re-acquire at the SAME count (instance 2; the
// COUNT_KEYED world does not clear) -> serveStepDown: the stamp
// compares equal against the repeated count, the serve demotes
// instance 2, and the latch (judged on the TRUE filing instance,
// independent of the keying) fires — recovery #1's demotion firing
// against a successor whose recovery #2 succeeded.
//
// This pin is the falsify twin for leaderElectionStepDown's
// noStaleStepDownServed: if it ever stops falsifying, the model has
// drifted away from the ABA the instance stamp exists to prevent.
//
// Run (the wired pin — the count-keyed world falsifies):
//   quint verify --backend=tlc --main=leaseCalib128CountKeyedStepDown \
//     --step=calibStep --invariant=noStaleStepDownServed \
//     docs/spec/models/calibration/lease-128-count-keyed-stepdown.qnt
module leaseCalib128CountKeyedStepDown {
  import leaderElection(
    NODES = Set("n1", "n2"),
    FENCE_AFTER = 3,
    STEAL_AFTER = 3,
    RENEW = 1,
    MAX_SKEW = 1,
    MAX_TIME = 8,
    MAX_GEN = 4,
    MAX_RV = 8,
    MAX_DELETES = 0,
    MAX_CLAIM_FAILURES = 0,
    MAX_RESTORES = 0,
    MAX_SUSPENDS = 0,
    MAX_SHUTDOWNS = 0,
    MAX_DROPS = 0,
    MAX_FOREIGN_BUMPS = 0,
    OBS_KEY_RV = false,
    BLIND_HOLDER_BOUND = 0,
    HOLDER_EVIDENCE_LOSE = false,
    CONFLICT_IMMEDIATE_LOSE = false,
    MAX_STEP_DOWNS = 2,
    STEP_DOWN_COUNT_KEYED = true
  ).* from "../leaderElection"

  /// Same fault economy as the live regime's stepDownStep — the only
  /// delta in this world is the count keying + missing clear above.
  action calibStep = any {
    tickAny,
    apiGetAny,
    stealAny,
    renewLeaseAny,
    conflictAny,
    selfFenceAny,
    crashAny,
    recoverAny,
    requestStepDownAny,
    serveStepDownAny,
  }
}
