// -*- mode: Bluespec; -*-
//
// Falsify twin, controller wedge family (bughunt-2 slot 5,
// merged_bug_176). Live-import form: the AS-BUILT defect is an axis of
// the live wedgeCluster model (ENABLE_FULL_EPILOGUE = false).
//
// PRE-FIX behavior modeled: the Systemic arm early-returns past the
// epilogue — the marked set freezes across the episode (a stale entry
// silently swallows the next genuine re-wedge's transition increment)
// and the suppressed nodes' evidence survives, so a later per-node
// verdict is built from anchors a systemic episode already explained.
//
// PREDICTED (depth 4): markedIncrementsOnlyOnEdges falsifies — mark a
// node (tick 1-2), drive a systemic episode (tick 3: the early return
// leaves marked nonempty against a Systemic verdict whose epilogue
// survivor set is empty); the same trace extended one tick falsifies
// noPerNodeFromSuppressedEvidence when the retained episode anchors
// re-mark without fresh observation.
//
// Recorded violation seed (quint run, --backend=rust):
//   0xbd5098b0e5566354 (markedIncrementsOnlyOnEdges), 0x377ca8b3d8349cc8 (noPerNodeFromSuppressedEvidence) — see the slot-5 wave-log entry.
//
// Run:
//   quint run --main=wedgeCalib176EarlyReturn \
//     --invariant=markedIncrementsOnlyOnEdges \
//     docs/spec/models/calibration/wedge-176-early-return.qnt
module wedgeCalib176EarlyReturn {
  import wedgeCluster(
    MAX_TIME = 6,
    WINDOW = 3,
    MIN_DISTINCT = 2,
    ENABLE_POPULATIONS = false,
    ENABLE_FULL_EPILOGUE = false,
    ENABLE_EVICTION = true,
    // Pre-round-3 as-built: no episode latch either (the axis under
    // test here is the epilogue; the latch twin is wedge-163).
    ENABLE_EPISODE_LATCH = false,
    TRACK_EPISODE_GHOSTS = false,    ENABLE_TRAJECTORY = false,    DWELL_TICKS = 1,    BREADTH_MIN = 2,
    TRACK_GHOST_NODES = false,
    ENABLE_ADMISSION = true,
    ENABLE_SUPPRESSED_RETENTION = true
  ).* from "../wedgeCluster"
}
