// -*- mode: Bluespec; -*-
//
// Falsify twin, controller wedge family (bughunt-3 S7,
// merged_bug_163). Live-import form: the AS-BUILT round-2 defect is
// an axis of the live wedgeCluster model (ENABLE_EPISODE_LATCH =
// false, everything else repaired).
//
// PRE-FIX behavior modeled: the Systemic arm drains only the WEDGED
// nodes' anchors and nothing latches the episode — the episode's
// still-open attempts re-anchor fresh the next tick (trailing-edge
// laggard reap) and a sub-threshold participant's surviving anchor
// pairs with any later blip (effective threshold 1 for 30 minutes).
//
// PREDICTED: noRemarkFromLatchedEpisode falsifies within ~4 ticks —
// drive a systemic episode (2 nodes x 2 drvs over a small fleet),
// keep one node's pairs expired (still-open attempts), observe the
// next tick: the laggard re-anchors from expiry runs that began at
// or before the watermark and lands in a per-node verdict.
//
// Run:
//   quint run --main=wedgeCalib163PartialDrain \
//     --invariant=noRemarkFromLatchedEpisode \
//     docs/spec/models/calibration/wedge-163-partial-drain.qnt
module wedgeCalib163PartialDrain {
  import wedgeCluster(
    MAX_TIME = 6,
    WINDOW = 3,
    MIN_DISTINCT = 2,
    ENABLE_POPULATIONS = true,
    ENABLE_FULL_EPILOGUE = true,
    ENABLE_EVICTION = true,
    ENABLE_EPISODE_LATCH = false,
    // The latch twin needs the ghosts LIVE: watermark/expiredSince
    // carry the violation (see TRACK_EPISODE_GHOSTS in the model).
    TRACK_EPISODE_GHOSTS = true
  ).* from "../wedgeCluster"
}
