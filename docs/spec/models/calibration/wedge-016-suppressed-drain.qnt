// merged_bug_016 falsify twin (round-5): the as-built drain on
// suppressed ticks --- the trajectory-suppressed arm warned "evidence
// is retained (no drain)" yet fed an EMPTY survivor set into the
// shared `marked.retain` tail, draining the transition-memory on
// every breadth/dwell-suppressed tick. A node continuously past the
// cluster threshold across a suppressed phase then re-inserted on the
// first post-suppression tick, re-incrementing
// rio_controller_node_wedge_marked_total and re-firing the
// Dead-equivalent warn for ONE continuous wedge (the
// once-per-transition contract, with no not-wedged interval).
//
// Demonstrated [violation] against suppressedTickRetainsMarked at the
// trajectory regime's exact bounds: per-node mark at t1 (breadth
// under the bar), developing episode at t2 (breadth at the bar,
// ratio not fired) --- the as-built tail drops the marked entry on
// the suppressed tick itself (lastShrankMarked).
//
// ENABLE_SUPPRESSED_RETENTION=false is the entire difference from
// wedgeClusterTrajectory --- same bounds, same axes otherwise.
module wedgeCalib016SuppressedDrain {
  import wedgeCluster(
    MAX_TIME = 4, WINDOW = 2, MIN_DISTINCT = 2,
    ENABLE_POPULATIONS = true,
    ENABLE_FULL_EPILOGUE = true,
    ENABLE_EPISODE_LATCH = true,
    ENABLE_EVICTION = true,
    TRACK_EPISODE_GHOSTS = false,
    ENABLE_TRAJECTORY = true,
    DWELL_TICKS = 1,
    BREADTH_MIN = 2,
    TRACK_GHOST_NODES = false,
    ENABLE_ADMISSION = true,
    ENABLE_SUPPRESSED_RETENTION = false,
    ENABLE_RELEASE_CLOSE = true
  ).* from "../wedgeCluster"
}
