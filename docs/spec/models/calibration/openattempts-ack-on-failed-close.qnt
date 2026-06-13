// -*- mode: Bluespec; -*-
//
// openAttempts calibration, ack-on-failed-close (bughunt-2 bug_182 —
// ackImpliesSettledOrArmed's falsifier).
//
// PRE-FIX behavior modeled: every consumption arm acked
// unconditionally — a close whose write FAILED still answered the
// store OK, killing the 600s report redelivery; the attempt settled
// ~an hour later through the CHARGED 'unreported' establishment. The
// trace: mintDelivered -> calibAckOnFailedClose (the closeApply seat
// observes acks=true with settled=false and latches ackedUnsettled).
//
// DECISION-ONLY perturbation: the NACK decision flips to an ack; the
// latch truth comes from the live closeApply oracle.
//
// PREDICTED: ackImpliesSettledOrArmed falsifies (ackedUnsettled
// latch).
//
// Run (falsification):
//   quint verify --backend=tlc --main=openAttemptsAckOnFailedClose \
//     --step=calibStep --invariant=ackImpliesSettledOrArmed \
//     docs/spec/models/calibration/openattempts-ack-on-failed-close.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=openAttemptsAckOnFailedClose \
//     --invariant=ackImpliesSettledOrArmed \
//     docs/spec/models/calibration/openattempts-ack-on-failed-close.qnt
module openAttemptsAckOnFailedClose {
  import openAttempts.* from "../openAttempts"

  /// PRE-FIX: the failed close write is ACKED anyway — the
  /// unconditional-ack arm routes the flipped decision through the
  /// failed-close seat.
  // Environment: calibration override -- the pre-fix posture this twin re-encodes (the file header names the bug); no production transition fires this arm
  action calibAckOnFailedClose = all {
    attempt == AOpen,
    node == NWantedLive,
    closeFailedApply(true),
    responseLost' = responseLost,
    clientHoldsResume' = clientHoldsResume,
    node' = node,
    outbox' = outbox,
    cancelDurable' = cancelDurable,
  }

  action calibStep = any {
    step,
    calibAckOnFailedClose,
  }
}
