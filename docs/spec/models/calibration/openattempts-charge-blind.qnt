// -*- mode: Bluespec; -*-
//
// openAttempts calibration, charge-blind establishment (bug_347 —
// cancelledNeverChargedAsCrash's falsifier; converted from a frozen
// copy to live-import action-only form per §4-R2 / quint-policy P2).
//
// PRE-FIX behavior modeled (bug_347): the failed cancel persist is
// dropped (no outbox latch) AND the establishment sweep charges
// every expired open attempt without consulting the node — a
// cancelled node's attempt is charged executor_crash. The trace:
// mintDelivered -> calibCancelPersistErrDropped ->
// calibSweepChargeBlind (the closeApply seat applies the charge over
// node == NCancelled).
//
// DECISION-ONLY perturbations: the outbox decision flips at the
// cancelFailApply seat; the sweep's kernel node-consult is dropped at
// the closeApply seat. No calibration action assigns a latch (P5).
//
// PREDICTED: cancelledNeverChargedAsCrash falsifies (charged while
// node != NWantedLive).
//
// Run (falsification):
//   quint verify --backend=tlc --main=openAttemptsChargeBlind \
//     --step=calibStep --invariant=cancelledNeverChargedAsCrash \
//     docs/spec/models/calibration/openattempts-charge-blind.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=openAttemptsChargeBlind \
//     --invariant=cancelledNeverChargedAsCrash \
//     docs/spec/models/calibration/openattempts-charge-blind.qnt
module openAttemptsChargeBlind {
  import openAttempts.* from "../openAttempts"

  /// PRE-FIX: the failed persist is dropped (best-effort error log) —
  /// nothing latches, nothing re-drives.
  action calibCancelPersistErrDropped = all {
    node == NWantedLive,
    attempt == AOpen,
    cancelFailApply(false),
    cancelDurable' = cancelDurable,
    viewClaimed' = viewClaimed,
    clientHoldsResume' = clientHoldsResume,
    responseLost' = responseLost,
  }

  /// PRE-FIX: the sweep never consults the node — every expired open
  /// attempt is charged executor_crash, cancelled or not.
  action calibSweepChargeBlind = all {
    attempt == AOpen,
    not(responseLost and clientHoldsResume),
    closeSettledApply(true, true),
    node' = node,
    outbox' = outbox,
    cancelDurable' = cancelDurable,
    responseLost' = responseLost,
    clientHoldsResume' = false,
  }

  action calibStep = any {
    step,
    calibCancelPersistErrDropped,
    calibSweepChargeBlind,
  }
}
