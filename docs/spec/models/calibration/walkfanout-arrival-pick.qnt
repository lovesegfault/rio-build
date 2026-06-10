// -*- mode: Bluespec; -*-
//
// walkFanout calibration, arrival-pick (live_047/R-C — the R-c
// rejected shape; foldTierLawful's falsifier).
//
// PRE-LAW behavior modeled: the outcome compiles from the FIRST
// abort-grade completion's kind (first-dequeued-wins) instead of the
// tier fold over the completed multiset — charge-vs-defer becomes
// schedule-dependent within simultaneously-completed evidence, the
// exact harm the path-fold law's backlog fold exists to prevent
// (charge evidence is park-budget-bearing; a schedule where the
// transient merely DEQUEUES first must not hide a completed charge).
//
// DECISION-ONLY perturbation: `calibCompileArrivalPick` reads the
// live model's `firstAbortVerdict` ghost instead of folding
// `abortsDone`; every other action is the live one.
//
// PREDICTED: foldTierLawful falsifies via spawn(4) -> spawn(3) ->
//   complete(4 transient, latch, firstAbortVerdict="retry") ->
//   complete(3 charge — the dequeue backlog) ->
//   calibCompileArrivalPick (verdict "retry" with a completed charge).
//
// Kill isolation: windowBounded stays INTACT under calibStep — its
// own holds lane (quint-walkfanout-calib-arrival-pick-isolation).
//
// Run (falsification):
//   quint run --invariant=foldTierLawful --step=calibStep \
//     --main=walkFanoutArrivalPick \
//     docs/spec/models/calibration/walkfanout-arrival-pick.qnt
module walkFanoutArrivalPick {
  import walkFanout.* from "../walkFanout"

  /// PRE-LAW: first-dequeued-wins — the wire verdict is the first
  /// abort-grade completion's kind, not the multiset's tier fold.
  action calibCompileArrivalPick = all {
    latched,
    inflight == Set(),
    verdict == "",
    verdict' = firstAbortVerdict,
    pending' = pending,
    inflight' = inflight,
    served' = served,
    settledCells' = settledCells,
    abortsDone' = abortsDone,
    cancelled' = cancelled,
    latched' = latched,
    floor' = floor,
    gen' = gen,
    firstAbortVerdict' = firstAbortVerdict,
    spawnedPostLatch' = spawnedPostLatch,
  }

  action calibStep = any {
    spawn,
    complete,
    cancelSibling,
    reResolve,
    calibCompileArrivalPick,
    idle,
  }
}
