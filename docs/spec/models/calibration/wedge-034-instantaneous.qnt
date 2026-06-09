// merged_bug_034 falsify twin (Q2 SIGNED, round-4): the RETIRED
// instantaneous systemic guard — per-tick wedged/population ratio over
// whatever the tick happened to observe, no breadth, no dwell, no
// fleet denominator — demonstrated against BOTH trajectory laws at the
// trajectory regime's exact bounds:
//
//   systemicDenominatorIsFleet  — the lull false-Systemic: a traffic
//     lull shrinks the instantaneous population to the wedged set
//     itself, so the ratio bar passes against a denominator smaller
//     than the registered fleet (of_ = 2 over a 3-node fleet).
//
//   noSerialReapInDevelopingEpisode — the staggered serial reap: a
//     shared-cause episode wedging nodes one at a time keeps the
//     instantaneous wedged count below the systemic bar, so each
//     victim takes a per-node Dead verdict while the episode's breadth
//     (>= 2 evidence-bearing nodes in the window) is plainly visible.
//
// Both checks are wired expect-violation (the [violation] IS the
// demonstration); the repaired law holds both in
// wedgeClusterTrajectory. ENABLE_TRAJECTORY=false here is the entire
// difference from that regime — same bounds, same axes otherwise.
module wedgeCalib034Instantaneous {
  import wedgeCluster(
    MAX_TIME = 4, WINDOW = 2, MIN_DISTINCT = 2,
    ENABLE_POPULATIONS = true,
    ENABLE_FULL_EPILOGUE = true,
    ENABLE_EPISODE_LATCH = true,
    ENABLE_EVICTION = true,
    TRACK_EPISODE_GHOSTS = false,
    ENABLE_TRAJECTORY = false,
    DWELL_TICKS = 1,
    BREADTH_MIN = 2,
    TRACK_GHOST_NODES = false,
    ENABLE_ADMISSION = true,
    ENABLE_SUPPRESSED_RETENTION = true
  ).* from "../wedgeCluster"
}
