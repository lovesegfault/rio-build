// -*- mode: Bluespec; -*-
//
// openAttempts calibration, refusal-as-lost (bughunt-3 bug_119 --
// noRefusalFiledAsLost's falsifier).
//
// PRE-FIX behavior modeled: pull_once mapped every resolved RPC error
// to Unanswered -- including answered permanent rejections
// (unauthenticated / permission-denied) where no mint can have
// committed -- so the ledger entry became immortal: every pass burned
// a doomed resume pull plus a doomed fresh claim while the warn
// mis-narrated a credential refusal as a resumable lost response.
//
// DECISION-ONLY perturbation: `resolvesEntry` flips to false at the
// answered-refusal seat; the latch truth comes from the seat's own
// oracle (a refusal that keeps the credential is the filing).
//
// PREDICTED: noRefusalFiledAsLost falsifies (refusalFiledAsLost).
//
// Run (falsification):
//   quint verify --backend=tlc --main=openAttemptsRefusalAsLost \
//     --step=calibStep --invariant=noRefusalFiledAsLost \
//     docs/spec/models/calibration/openattempts-refusal-as-lost.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=openAttemptsRefusalAsLost \
//     --invariant=noRefusalFiledAsLost \
//     docs/spec/models/calibration/openattempts-refusal-as-lost.qnt
module openAttemptsRefusalAsLost {
  import openAttempts.* from "../openAttempts"

  /// PRE-FIX: the answered refusal keeps the entry (filed as lost).
  // Environment: calibration override -- the pre-fix posture this twin re-encodes (the file header names the bug); no production transition fires this arm
  action calibRefusalKeepsEntry = all {
    clientHoldsResume,
    answeredRefusalSeat(false),
    node' = node,
    outbox' = outbox,
    cancelDurable' = cancelDurable,
  }

  action calibStep = any {
    step,
    calibRefusalKeepsEntry,
  }
}
