// merged_bug_023 downgrade falsify twin (bughunt-6 S4): the code's
// engaged-continue arm swapped the stored episode axis with NO
// transition effects, so a breadth (developing, state 2) episode that
// decayed into the dwell arm (state 3) skipped Breadth's
// drain+merge-latch close; at dwell expiry the release keyed Dwell's
// no-op and the RETAINED breadth-phase evidence minted a per-node
// verdict — exactly what noPerNodeFromSuppressedEvidence forbids.
//
// This twin binds ENABLE_DWELL_ARM=true (the code's machine: the
// dwell-engaged value joins the episode alphabet and the release edge
// keys the STORED state) with DOWNGRADE_RUNS_CLOSE=false (the
// as-built continue-without-effects). ENABLE_RELEASE_CLOSE stays true
// — the release law EXISTS and works; the defect is the downgrade
// BYPASSING it into the no-op dwell release (kill-isolation, R16: a
// violation here cannot come from the round-5 missing-release-close
// conjunct, whose own twin is calibration/wedge-023-open-release.qnt
// with RELEASE_CLOSE=false).
//
// MID-TRACE INIT (the controller-113 precedent — recorded seeded
// frame): the violating trajectory needs TWO episodes (a prior
// systemic latch, then a developing phase whose breadth decays inside
// the dwell window with asymmetric anchor ages), which is past the
// cold-init reach at sound bounds (the breadth node's anchor must be
// >= 2 ticks older than the wedged node's — cold init cannot age them
// apart before MAX_TIME). calibInit seeds the frame the cold prefix
// would produce:
//   time=4; watermark=0 (a systemic episode latched at t0);
//   episodeState=2 (a developing phase is engaged);
//   evidence = n1:{d0@4, d1@4} (fresh post-watermark runs, the wedged
//   node), n2:{d0@2} (the breadth node, anchored 2 ticks earlier);
//   expiredSince(n1,*)=3 (> watermark, so n1 re-admits);
//   suppressedAnchors = the developing phase's episode-explained
//   anchors {(n1,d0,4),(n1,d1,4),(n2,d0,2)}.
// From this frame the violation is 3 ticks: t5 — n2@2 still in
// window (5-2 <= WINDOW=3): breadth 2, the developing phase
// continues; t6 — n2@2 ages out (6-2 > 3), breadth 1 < BREADTH_MIN,
// wedged={n1} (n1@4: 6-4 <= 3), dwell active (6-0 <=
// VERDICT_DWELL_TICKS=6) → the 2→3 DOWNGRADE (sawDowngrade latches;
// as-built: no close, anchors retained); t7 — the dwell expires
// (7-0 > 6), the release keys stored state 3 (no-op), survivors =
// wedged = {n1} (n1@4: 7-4 <= 3, still qualifying), and n1's entire
// qualifying evidence {(n1,d0,4),(n1,d1,4)} is suppression-survivors
// → noPerNodeFromSuppressedEvidence is expected [violation]. With
// DOWNGRADE_RUNS_CLOSE=true the t6 downgrade drains + merge-latches
// (watermark=6) and t7 mints nothing — the in-file
// wedgeClusterDwellArm hold.
//
module wedge023DowngradeSkipsClose {
  import wedgeCluster(
    MAX_TIME = 7, WINDOW = 3, MIN_DISTINCT = 2,
    ENABLE_POPULATIONS = true,
    ENABLE_FULL_EPILOGUE = true,
    ENABLE_EPISODE_LATCH = true,
    ENABLE_EVICTION = true,
    TRACK_EPISODE_GHOSTS = true,
    ENABLE_TRAJECTORY = true,
    BREADTH_MIN = 2,
    TRACK_GHOST_NODES = false,
    ENABLE_ADMISSION = true,
    ENABLE_SUPPRESSED_RETENTION = true,
    ENABLE_RELEASE_CLOSE = true,
    ENABLE_DWELL_ARM = true,
    DOWNGRADE_RUNS_CLOSE = false,
    VERDICT_DWELL_TICKS = 6
  ).* from "../wedgeCluster"

  /// The seeded mid-trajectory frame (derivation in the header).
  action calibInit = all {
    time' = 4,
    evidence' = Map(
      "n1" -> Map("d0" -> 4, "d1" -> 4),
      "n2" -> Map("d0" -> 2)
    ),
    marked' = Set(),
    markedIncrements' = 0,
    pendingEvictions' = Set(),
    reapHistory' = Set(),
    lastSystemic' = false,
    lastAffected' = 0,
    lastOf' = 0,
    lastWedged' = Set(),
    lastDelta' = 0,
    suppressedAnchors' = Set(("n1", "d0", 4), ("n1", "d1", 4), ("n2", "d0", 2)),
    watermark' = 0,
    expiredSince' = Map(("n1", "d0") -> 3, ("n1", "d1") -> 3),
    sawSystemic' = true,
    sawPerNode' = false,
    sawSuppressed' = true,
    lastSkip' = false,
    lastSuppressedNoDrain' = true,
    lastShrankMarked' = false,
    episodeState' = 2,
    sawRelease' = false,
    sawDowngrade' = false,
  }

  /// Tick-only step (reapMarked is irrelevant: marked stays empty on
  /// the violating path).
  action calibStep = tick
}

// HOLD half from the SAME mid-trace init (the controller-113-hold
// precedent): DOWNGRADE_RUNS_CLOSE=true — the t6 downgrade RUNS the
// breadth close (drain + merge-latch, sawDowngrade still latches) and
// t7 mints nothing: noPerNodeFromSuppressedEvidence holds over the
// seeded frontier, and downgradeReachableW is expected [violation]
// (the downgrade edge IS exercised — the hold is not vacuous).
module wedge023DowngradeSkipsCloseHold {
  import wedgeCluster(
    MAX_TIME = 7, WINDOW = 3, MIN_DISTINCT = 2,
    ENABLE_POPULATIONS = true,
    ENABLE_FULL_EPILOGUE = true,
    ENABLE_EPISODE_LATCH = true,
    ENABLE_EVICTION = true,
    TRACK_EPISODE_GHOSTS = true,
    ENABLE_TRAJECTORY = true,
    BREADTH_MIN = 2,
    TRACK_GHOST_NODES = false,
    ENABLE_ADMISSION = true,
    ENABLE_SUPPRESSED_RETENTION = true,
    ENABLE_RELEASE_CLOSE = true,
    ENABLE_DWELL_ARM = true,
    DOWNGRADE_RUNS_CLOSE = true,
    VERDICT_DWELL_TICKS = 6
  ).* from "../wedgeCluster"

  /// The seeded mid-trajectory frame (same as the twin's; see the
  /// module header above).
  action calibInit = all {
    time' = 4,
    evidence' = Map(
      "n1" -> Map("d0" -> 4, "d1" -> 4),
      "n2" -> Map("d0" -> 2)
    ),
    marked' = Set(),
    markedIncrements' = 0,
    pendingEvictions' = Set(),
    reapHistory' = Set(),
    lastSystemic' = false,
    lastAffected' = 0,
    lastOf' = 0,
    lastWedged' = Set(),
    lastDelta' = 0,
    suppressedAnchors' = Set(("n1", "d0", 4), ("n1", "d1", 4), ("n2", "d0", 2)),
    watermark' = 0,
    expiredSince' = Map(("n1", "d0") -> 3, ("n1", "d1") -> 3),
    sawSystemic' = true,
    sawPerNode' = false,
    sawSuppressed' = true,
    lastSkip' = false,
    lastSuppressedNoDrain' = true,
    lastShrankMarked' = false,
    episodeState' = 2,
    sawRelease' = false,
    sawDowngrade' = false,
  }

  /// Tick-only step.
  action calibStep = tick
}
