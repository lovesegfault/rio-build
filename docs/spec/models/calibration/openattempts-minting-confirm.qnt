// -*- mode: Bluespec; -*-
//
// openAttempts calibration, minting-confirm (bughunt-3
// merged_bug_083 -- confirmNeverMints's falsifier).
//
// PRE-FIX behavior modeled: the maybe-minted confirm probe was a
// PLAIN pull the kernel answers `Ready => DeliverNew` -- the dying
// (or idle-exiting) pod's own confirmation probe could mint a fresh
// attempt for a pod that is about to stop existing. The wire
// discriminator (PullAssignmentRequest.confirm_only) screens the
// DeliverNew admission to NotYetReady.
//
// DECISION-ONLY perturbation: `mints` flips to true at the
// confirm-probe seat; the latch truth comes from the seat's own
// oracle.
//
// PREDICTED: confirmNeverMints falsifies (confirmMinted).
//
// Run (falsification):
//   quint verify --backend=tlc --main=openAttemptsMintingConfirm \
//     --step=calibStep --invariant=confirmNeverMints \
//     docs/spec/models/calibration/openattempts-minting-confirm.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=openAttemptsMintingConfirm \
//     --invariant=confirmNeverMints \
//     docs/spec/models/calibration/openattempts-minting-confirm.qnt
module openAttemptsMintingConfirm {
  import openAttempts.* from "../openAttempts"

  /// PRE-FIX: the confirm probe mints (the unscreened DeliverNew arm).
  // Environment: calibration override -- the pre-fix posture this twin re-encodes (the file header names the bug); no production transition fires this arm
  action calibConfirmMints = all {
    confirmProbeSeat(true),
    node' = node,
    outbox' = outbox,
    cancelDurable' = cancelDurable,
  }

  action calibStep = any {
    step,
    calibConfirmMints,
  }
}
