// -*- mode: Bluespec; -*-
//
// openAttempts calibration, unfenced-gone (bughunt-5 merged_bug_011
// -- noMintAfterGoneAnswer's falsifier).
//
// PRE-FIX behavior modeled: the confirm-fence write-ahead fired only
// when confirm_only, so the LIVE-loop Gone answered the token's
// exit-0 license with no row on disk. The straggler chain: the
// content-addressed resubmit re-readies the drv (resubmitReready),
// the same token's straggler pull admits Ready => DeliverNew, the
// fence read finds nothing, and the mint opens an attempt no sweep
// can see (mintAfterGone).
//
// DECISION-ONLY perturbation: `writesFence` flips to false at the
// Gone-answer seat; the latch truth comes from the mint seat's own
// oracle (quint-policy P4/P5 -- no calibration override).
//
// PREDICTED: noMintAfterGoneAnswer falsifies (mintAfterGone).
//
// Run (falsification):
//   quint verify --backend=tlc --main=openAttemptsUnfencedGone \
//     --step=calibStep --invariant=noMintAfterGoneAnswer \
//     docs/spec/models/calibration/openattempts-unfenced-gone.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=openAttemptsUnfencedGone \
//     --invariant=noMintAfterGoneAnswer \
//     docs/spec/models/calibration/openattempts-unfenced-gone.qnt
module openAttemptsUnfencedGone {
  import openAttempts.* from "../openAttempts"

  /// PRE-FIX: the live-loop Gone licenses the exit WITHOUT writing
  /// the fence (the as-built confirm_only-gated arm).
  action calibLiveGoneUnfenced = all {
    node != NWantedLive,
    attempt != AOpen,
    goneAnswerSeat(false),
    node' = node,
    outbox' = outbox,
    cancelDurable' = cancelDurable,
  }

  action calibStep = any {
    step,
    calibLiveGoneUnfenced,
  }
}
