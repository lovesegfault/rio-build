// -*- mode: Bluespec; -*-
//
// openAttempts calibration, rotation-skew (bughunt-4 merged_bug_074
// -- noFaultNeverCharged's second falsifier, through the auth seat).
//
// PRE-FIX behavior modeled: is_fatal_rejection folded the auth-layer
// codes (PermissionDenied/Unauthenticated -- the scheduler's HMAC
// rotation-skew trace, emitted without consulting attempt state)
// into the resolving Rejected arm, so the resume pass DESTROYED the
// rule-4b ledger entry on a skewed presentation. The entry exists
// precisely because the original unanswered pull may have committed
// a mint: with the credential gone and the response still lost, the
// attempt can no longer redeliver and settles through the CHARGED
// establishment window -- charged with no fault but a lost response,
// exactly during fleet rotations.
//
// DECISION-ONLY perturbation: `resolvesEntry` flips to true at the
// auth-refusal seat; the harm latches through the EXISTING machinery
// (sweepEstablish's charged arm: charged and responseLost ->
// chargedNoFault). responseLost stays untouched by the seat in both
// polarities -- the auth answer judges the presentation, never the
// response fate.
//
// PREDICTED: noFaultNeverCharged falsifies (chargedNoFault) via
//   mintResponseLost -> calibAuthRefusalResolves -> sweepEstablish.
//
// Run (falsification):
//   quint verify --backend=tlc --main=openAttemptsRotationSkew \
//     --step=calibStep --invariant=noFaultNeverCharged \
//     docs/spec/models/calibration/openattempts-rotation-skew.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=openAttemptsRotationSkew \
//     --invariant=noFaultNeverCharged \
//     docs/spec/models/calibration/openattempts-rotation-skew.qnt
module openAttemptsRotationSkew {
  import openAttempts.* from "../openAttempts"

  /// PRE-FIX: the auth-layer refusal resolves the entry (the
  /// credential evaporates during rotation skew).
  // Environment: calibration override -- the pre-fix posture this twin re-encodes (the file header names the bug); no production transition fires this arm
  action calibAuthRefusalResolves = all {
    clientHoldsResume,
    authRefusalSeat(true),
    node' = node,
    outbox' = outbox,
    cancelDurable' = cancelDurable,
  }

  action calibStep = any {
    step,
    calibAuthRefusalResolves,
  }
}
