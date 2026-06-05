// -*- mode: Bluespec; -*-
//
// openAttempts calibration, no-fallback-release (bughunt-2
// merged_bug_055 — claimedImpliesOpenAttempt's falsifier).
//
// PRE-FIX behavior modeled: a settled consumption close whose
// COMPANION write failed kept the view holder — the claim stranded
// with its attempt closed (the claimed-no-attempt ghost: the
// listing's claimed filter hid the job from every replica forever).
// The trace: mintDelivered -> calibCloseNoRelease (the closeApply
// seat observes releasesClaim=false with a closed result and latches
// ghostClaim).
//
// DECISION-ONLY perturbation: the companion-failure release fallback
// flips off; the latch truth comes from the live closeApply oracle.
//
// PREDICTED: claimedImpliesOpenAttempt falsifies (ghostClaim latch +
// the state form).
//
// Run (falsification):
//   quint verify --backend=tlc --main=openAttemptsNoFallbackRelease \
//     --step=calibStep --invariant=claimedImpliesOpenAttempt \
//     docs/spec/models/calibration/openattempts-no-fallback-release.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=openAttemptsNoFallbackRelease \
//     --invariant=claimedImpliesOpenAttempt \
//     docs/spec/models/calibration/openattempts-no-fallback-release.qnt
module openAttemptsNoFallbackRelease {
  import openAttempts.* from "../openAttempts"

  /// PRE-FIX: the close settles but the failed companion's release
  /// fallback never runs — the claim is stranded (releasesClaim=false
  /// at the settled-close seat).
  action calibCloseNoRelease = all {
    attempt == AOpen,
    node == NWantedLive,
    closeSettledApply(false, false),
    responseLost' = false,
    clientHoldsResume' = false,
    node' = node,
    outbox' = outbox,
    cancelDurable' = cancelDurable,
  }

  action calibStep = any {
    step,
    calibCloseNoRelease,
  }
}
