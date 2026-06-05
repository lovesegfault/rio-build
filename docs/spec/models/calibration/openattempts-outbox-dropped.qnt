// -*- mode: Bluespec; -*-
//
// openAttempts calibration, outbox-dropped (bughunt-2 bug_357's
// prescribed twin — notDriverLost's falsifier).
//
// PRE-FIX behavior modeled: the cancel's failed terminal persist is
// dropped on a best-effort error log — nothing latches in the status
// outbox, nothing re-drives the close. The trace: mintDelivered ->
// calibCancelPersistErrDropped (the driver is never armed; the
// cancelFailApply seat latches driverLost).
//
// DECISION-ONLY perturbation: `latchesOutbox` flips to false; the
// latch truth comes from the live cancelFailApply oracle. This is the
// twin the old tautological openAttemptHasDriver could never have:
// its consequent was constant-true, so no transition system falsified
// it (bug_357).
//
// PREDICTED: notDriverLost falsifies (driverLost latch).
//
// Run (falsification):
//   quint verify --backend=tlc --main=openAttemptsOutboxDropped \
//     --step=calibStep --invariant=notDriverLost \
//     docs/spec/models/calibration/openattempts-outbox-dropped.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=openAttemptsOutboxDropped \
//     --invariant=notDriverLost \
//     docs/spec/models/calibration/openattempts-outbox-dropped.qnt
module openAttemptsOutboxDropped {
  import openAttempts.* from "../openAttempts"

  /// PRE-FIX: the failed persist drops the close on the floor — the
  /// driver is never armed.
  action calibCancelPersistErrDropped = all {
    node == NWantedLive,
    attempt == AOpen,
    cancelFailApply(false),
    cancelDurable' = cancelDurable,
    viewClaimed' = viewClaimed,
    clientHoldsResume' = clientHoldsResume,
    responseLost' = responseLost,
  }

  action calibStep = any {
    step,
    calibCancelPersistErrDropped,
  }
}
