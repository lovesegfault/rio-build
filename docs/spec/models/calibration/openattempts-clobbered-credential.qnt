// -*- mode: Bluespec; -*-
//
// openAttempts calibration, clobbered-credential (bughunt-3
// merged_bug_096 -- noCredentialClobber's falsifier).
//
// PRE-FIX behavior modeled: the fresh-claim pass never consulted the
// ledger before note_pull, whose by-job upsert silently REPLACED a
// live entry's nonce with a fresh one -- destroying the only proof of
// a possibly-committed server-side mint (the listing is a live PG
// anti-join, so a job with a live entry re-lists exactly in the
// brownout regime the credential exists for).
//
// DECISION-ONLY perturbation: `mintsOverLive` flips to true at the
// fresh-claim seat while a credential is live; the latch truth comes
// from the seat's own oracle.
//
// PREDICTED: noCredentialClobber falsifies (credentialClobbered).
//
// Run (falsification):
//   quint verify --backend=tlc --main=openAttemptsClobberedCredential \
//     --step=calibStep --invariant=noCredentialClobber \
//     docs/spec/models/calibration/openattempts-clobbered-credential.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=openAttemptsClobberedCredential \
//     --invariant=noCredentialClobber \
//     docs/spec/models/calibration/openattempts-clobbered-credential.qnt
module openAttemptsClobberedCredential {
  import openAttempts.* from "../openAttempts"

  /// PRE-FIX: the fresh pass mints over the live credential.
  // Environment: calibration override -- the pre-fix posture this twin re-encodes (the file header names the bug); no production transition fires this arm
  action calibFreshMintClobbers = all {
    clientHoldsResume,
    freshClaimSeat(true),
    node' = node,
    outbox' = outbox,
    cancelDurable' = cancelDurable,
  }

  action calibStep = any {
    step,
    calibFreshMintClobbers,
  }
}
