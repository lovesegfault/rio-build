// -*- mode: Bluespec; -*-
//
// Calibration override / regression pin, bughunt-4 S2 merged_bug_085
// (rio-lease believing-409 spurious failover, signed Q3): re-introduces
// the IMMEDIATE-LOSE world — the pre-fix production code in which the
// first renew 409 while believing ran the full lose edge (DAG/outbox
// wipe, marks bounce) with no holder re-check, although the 409 proves
// only rv movement and both non-lose rv-movers (an own dropped-then-
// committed write; a foreign metadata patch) are live in this regime.
//
// PRE-FIX behavior modeled (one const, no step surgery):
// CONFLICT_IMMEDIATE_LOSE = true restores the lose-on-first-409 arm
// inside the holder-evidence world; the blindConflictLose latch
// (judged on the deferral ground truth — a believing FIRST 409 losing
// — independent of the toggle's own behavior) fires on exactly that
// arm.
//
// PREDICTED falsification: loseRequiresHolderEvidence. Schedule shape:
// the holder renews steadily; any rv movement between its GET and PUT
// (a foreign bumpRv, or its own renewSendDropped commit surfacing)
// bounces the CAS; the believing first 409 takes the restored
// immediate-lose arm and the latch fires.
//
// This pin is the falsify twin for leaderElectionHolderEvidence's
// loseRequiresHolderEvidence: if it ever stops falsifying, the model
// has drifted away from the blind lose the deferral law exists to
// prevent.
//
// Run (the wired pin — the immediate-lose world falsifies):
//   quint verify --backend=tlc --main=leaseCalib085BlindConflictLose \
//     --step=calibStep --invariant=loseRequiresHolderEvidence \
//     docs/spec/models/calibration/lease-085-blind-conflict-lose.qnt
module leaseCalib085BlindConflictLose {
  import leaderElection(
    NODES = Set("n1", "n2"),
    FENCE_AFTER = 2,
    STEAL_AFTER = 5,
    RENEW = 1,
    MAX_SKEW = 1,
    MAX_TIME = 10,
    MAX_GEN = 4,
    MAX_RV = 8,
    MAX_DELETES = 0,
    MAX_CLAIM_FAILURES = 0,
    MAX_RESTORES = 0,
    MAX_SUSPENDS = 0,
    MAX_SHUTDOWNS = 0,
    MAX_DROPS = 1,
    MAX_FOREIGN_BUMPS = 2,
    OBS_KEY_RV = false,
    BLIND_HOLDER_BOUND = 8,
    HOLDER_EVIDENCE_LOSE = true,
    CONFLICT_IMMEDIATE_LOSE = true,
    MAX_STEP_DOWNS = 0,
    STEP_DOWN_COUNT_KEYED = false,
    CONFLICT_CLEARS_HOLD = false
  ).* from "../leaderElection"

  /// Same fault economy as the live regime's holderEvidenceStep — the
  /// only delta in this world is the immediate-lose toggle above.
  action calibStep = any {
    tickAny,
    apiGetAny,
    stealAny,
    renewLeaseAny,
    conflictAny,
    loseObservedAny,
    selfFenceAny,
    renewSendDroppedAny,
    renewSendLostAny,
    fetchObservesOwnCommitAny,
    bumpRv,
  }
}
