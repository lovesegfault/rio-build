// merged_bug_024 falsify twin (round-5): the as-built admission gate
// never consulted fleet membership --- the absence sweep derived its
// tombstones from evidence keys only, so a node deleted BEFORE its
// first observation (a ghost) was admitted on its first post-expiry
// tick and counted in wedged, breadth and the population. Both
// admission laws are demonstrated [violation] at the admission
// regime's exact bounds:
//
//   evidenceWithinRegistered --- a ghost's expiry anchors land in the
//     evidence map (one observed tick suffices).
//
//   systemicOfWithinRegisteredUniverse --- admitted ghosts both wedge
//     and pad the denominator past the registered universe (the
//     false-Systemic shape: ghosts + a registered minority trip the
//     ratio bar over a population the fleet does not contain).
//
// ENABLE_ADMISSION=false is the entire difference from
// wedgeClusterAdmission --- same bounds, same axes otherwise.
module wedgeCalib024GhostAdmission {
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
    TRACK_GHOST_NODES = true,
    ENABLE_ADMISSION = false,
    ENABLE_SUPPRESSED_RETENTION = true,
    ENABLE_RELEASE_CLOSE = true
  ).* from "../wedgeCluster"
}
