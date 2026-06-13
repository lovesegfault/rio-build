// merged_bug_023 falsify twin (round-5): the as-built third exit ---
// breadth-axis suppression had no drain/latch/dwell on its release
// edge (last_suppression was written only in the ratio arm), so a
// breadth episode that decayed below the fraction (staggered age-out
// of the early participants, or fleet growth inflating the
// population) released SILENTLY with its retained evidence intact. A
// late-onset node still holding MIN_DISTINCT in-window anchors then
// minted a per-node verdict from evidence the suppression itself
// attributed to a shared cause --- the destructive Dead-reap of a
// possibly-recovering node.
//
// Demonstrated [violation] against noPerNodeFromSuppressedEvidence at
// the trajectory regime's exact bounds: the developing-episode ticks
// feed their window anchors into the suppression-survivor set; with
// the close disabled those anchors survive disengagement and wholly
// constitute the post-release per-node verdict.
//
// ENABLE_RELEASE_CLOSE=false is the entire difference from
// wedgeClusterTrajectory --- same bounds, same axes otherwise.
module wedgeCalib023OpenRelease {
  import wedgeCluster(
    MAX_TIME = 4, WINDOW = 2, MIN_DISTINCT = 2,
    ENABLE_POPULATIONS = true,
    ENABLE_FULL_EPILOGUE = true,
    ENABLE_EPISODE_LATCH = true,
    ENABLE_EVICTION = true,
    TRACK_EPISODE_GHOSTS = false,
    ENABLE_TRAJECTORY = true,
    BREADTH_MIN = 2,
    TRACK_GHOST_NODES = false,
    ENABLE_ADMISSION = true,
    ENABLE_SUPPRESSED_RETENTION = true,
    ENABLE_RELEASE_CLOSE = false,
    ENABLE_DWELL_ARM = false,
    DOWNGRADE_RUNS_CLOSE = false,
    VERDICT_DWELL_TICKS = 1
  ).* from "../wedgeCluster"
}
