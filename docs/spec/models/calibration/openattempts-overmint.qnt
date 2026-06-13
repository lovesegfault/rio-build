// -*- mode: Bluespec; -*-
//
// openAttemptsBudget calibration, overmint (bughunt-4 merged_bug_072
// -- outstandingBounded's falsifier).
//
// PRE-FIX behavior modeled: the fresh-claim gate consulted a
// PER-PASS counter that reset to 0 at every pass boundary, so
// prior-pass unanswered mints (the surviving ledger population) were
// invisible to the budget: in a list-ok/pull-lost brownout a 1-slot
// worker minted one fresh nonce per pass, accumulating to the ledger
// cap (32x its capacity), each at-cap mint then evicting a live
// rule-4b credential.
//
// DECISION-ONLY perturbation: the mint gate's budget input flips
// from the ledger population to the per-pass counter
// (`mintBudgetSeat(passMints < SLOTS)`); one pass boundary later the
// population exceeds the slot budget.
//
// PREDICTED: outstandingBounded falsifies (ledgerPop = 2 > SLOTS = 1)
//   via budgetMintPerPass -> budgetPassEnd -> budgetMintPerPass.
//
// Run (falsification):
//   quint verify --backend=tlc --main=openAttemptsOvermint \
//     --step=calibStep --invariant=outstandingBounded \
//     docs/spec/models/calibration/openattempts-overmint.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=openAttemptsOvermint \
//     --invariant=outstandingBounded \
//     docs/spec/models/calibration/openattempts-overmint.qnt
module openAttemptsOvermint {
  import openAttemptsBudget.* from "../openAttemptsBudget"

  /// PRE-FIX: the gate consults the per-pass counter -- prior-pass
  /// unanswered mints are invisible.
  // Environment: calibration override -- the pre-fix posture this twin re-encodes (the file header names the bug); no production transition fires this arm
  action calibMintPerPassGated = mintBudgetSeat(passMints < SLOTS)

  action calibStep = any {
    step,
    calibMintPerPassGated,
  }
}
