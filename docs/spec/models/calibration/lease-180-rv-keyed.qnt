// -*- mode: Bluespec; -*-
//
// Calibration override / regression pin, bughunt3-wave S6b
// merged_bug_180 (rio-lease foreign rv churn): re-introduces the
// RV-KEYED world — the pre-fix production code in which both the
// standby's observed record and the own-commit evidence cursor keyed
// on metadata.resourceVersion movement (rio-lease pre-fix:
// decide()'s Observed{resource_version}, the loop's last_fetched_rv)
// instead of the holder-authored spec content.
//
// PRE-FIX behavior modeled (one const, no step surgery): OBS_KEY_RV =
// true flips leaseKey/snapKeyOf back to the raw rv, so a foreign
// bumpRv moves the very identity both consumers key on.
//
// PREDICTED falsification: noFabricatedEvidence. Schedule shape: the
// holder's transmitted write drops WITHOUT committing here — no
// protocol write moves the content — while a foreign bumpRv moves the
// rv past the evidence cursor; the rv-keyed fetchObservesOwnCommit
// consumes the ledger and re-anchors the holder's belief from churn
// the protocol never authored. The fabrication latch (judged on
// CONTENT ground truth, independent of the keying) fires.
//
// This pin is the falsify twin for leaderElectionForeignRv's
// noFabricatedEvidence: if it ever stops falsifying, the model has
// drifted away from the laundering the content keying exists to
// prevent.
//
// Run (the wired pin — the rv-keyed world falsifies):
//   quint verify --backend=tlc --main=leaseCalib180RvKeyed \
//     --step=calibStep --invariant=noFabricatedEvidence \
//     docs/spec/models/calibration/lease-180-rv-keyed.qnt
module leaseCalib180RvKeyed {
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
    MAX_DROPS = 2,
    MAX_FOREIGN_BUMPS = 2,
    OBS_KEY_RV = true,
    BLIND_HOLDER_BOUND = 8,
    HOLDER_EVIDENCE_LOSE = false,
    CONFLICT_IMMEDIATE_LOSE = false,
    MAX_STEP_DOWNS = 0,
    STEP_DOWN_COUNT_KEYED = false
  ).* from "../leaderElection"

  /// Same fault economy as the live regime's foreignRvStep — the only
  /// delta in this world is the keying const above.
  action calibStep = any {
    tickAny,
    apiGetAny,
    stealAny,
    conflictAny,
    selfFenceAny,
    renewSendDroppedAny,
    renewSendLostAny,
    fetchObservesOwnCommitAny,
    bumpRv,
  }
}
