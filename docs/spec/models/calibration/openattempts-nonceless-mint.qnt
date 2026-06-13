// -*- mode: Bluespec; -*-
//
// openAttempts calibration, nonceless-mint (bughunt-2 bug_251 —
// noFaultNeverCharged's falsifier).
//
// PRE-FIX behavior modeled: the mint persisted NO redelivery
// credential — a committed claim whose response was lost could never
// be re-delivered (the kernel's colliding-identity refusal answered
// NotYetReady to the same store forever), so the attempt settled
// through the CHARGED establishment window despite the worker having
// done nothing wrong. The trace: calibMintNonceless ->
// sweepEstablish (the sweep's no-credential guard passes; the
// closeApply seat observes the charge against responseLost and
// latches chargedNoFault).
//
// DECISION-ONLY perturbation: `credentialPersisted` flips to false at
// the mint seat; the latch truth comes from the live closeApply
// oracle inside the UNMODIFIED live sweepEstablish.
//
// PREDICTED: noFaultNeverCharged falsifies (chargedNoFault latch).
//
// Run (falsification):
//   quint verify --backend=tlc --main=openAttemptsNoncelessMint \
//     --step=calibStep --invariant=noFaultNeverCharged \
//     docs/spec/models/calibration/openattempts-nonceless-mint.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=openAttemptsNoncelessMint \
//     --invariant=noFaultNeverCharged \
//     docs/spec/models/calibration/openattempts-nonceless-mint.qnt
module openAttemptsNoncelessMint {
  import openAttempts.* from "../openAttempts"

  /// PRE-FIX: the mint's response is lost and NO credential was
  /// persisted — redelivery is impossible by construction.
  action calibMintNonceless = all {
    node == NWantedLive,
    attempt != AOpen,
    not(viewClaimed),
    mintApply(false, true),
    node' = node,
    outbox' = outbox,
    cancelDurable' = cancelDurable,
  }

  action calibStep = any {
    step,
    calibMintNonceless,
  }

  /// The bug_251 acceptance trace, PRE-FIX half (OQ-12; the named-run
  /// form pins the violation the twin's TLC search finds): the
  /// nonceless mint loses its response with NOTHING persisted to
  /// redeliver against, so the establishment sweep is enabled
  /// immediately (`not(responseLost and clientHoldsResume)` holds
  /// vacuously -- no credential) and the charge lands on a no-fault
  /// attempt: `chargedNoFault` latches. rio-scheduler's
  /// `mbt_openattempts` red-holders carry the implementation half:
  /// the production mint PERSISTS the credential
  /// (`assignments.claim_nonce`), so the same trace diverges at the
  /// mint step's projected credential, and a nonceless mint's
  /// credential presentation is REFUSED where the fix redelivers.
  run oa251AcceptanceRun =
    init
      .then(calibMintNonceless)
      .then(sweepEstablish)
      .expect(and {
        attempt == AClosedCharged,
        chargedNoFault,
        not(clientHoldsResume),
        responseLost,
        not(viewClaimed),
      })
}
