// -*- mode: Bluespec; -*-
//
// walkFanout calibration, spawn-after-abort (live_047/R-C —
// noSpawnAfterLatch's falsifier).
//
// PRE-LAW behavior modeled: the abort latch is ignored at spawn — a
// walk keeps widening after an abort-grade completion, so the
// cancelled-sibling window is no longer bounded by F−1 and a failing
// job keeps opening upstream work it is about to throw away (the
// law-4 latch exists so the first abort-grade completion stops the
// spend immediately). The width guard stays intact — this twin kills
// exactly the latch clause (the width clause's falsifier is the
// spawn-unbounded twin).
//
// DECISION-ONLY perturbation: `calibSpawnIgnoreLatch =
// spawnSeat(true)` — the live seat with the latch decision flipped;
// the ghost `spawnedPostLatch` (written by the SEAT, reachable only
// under this perturbation) is noSpawnAfterLatch's kill surface.
//
// PREDICTED: noSpawnAfterLatch falsifies via spawn(3) -> complete(3
//   charge, latch) -> calibSpawnIgnoreLatch (any pending path —
//   spawnedPostLatch flips).
//
// Run (falsification):
//   quint run --invariant=noSpawnAfterLatch --step=calibStep \
//     --main=walkFanoutSpawnAfterAbort \
//     docs/spec/models/calibration/walkfanout-spawn-after-abort.qnt
module walkFanoutSpawnAfterAbort {
  import walkFanout.* from "../walkFanout"

  /// PRE-LAW: the latch is invisible to the spawn decision.
  action calibSpawnIgnoreLatch = spawnSeat(true)

  action calibStep = any {
    spawn,
    calibSpawnIgnoreLatch,
    complete,
    cancelSibling,
    reResolve,
    compileOutcome,
    idle,
  }
}
