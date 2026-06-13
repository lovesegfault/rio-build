// -*- mode: Bluespec; -*-
//
// Falsify twin, controller wedge family (bughunt-2 slot 5,
// merged_bug_009). Live-import form: the AS-BUILT defect is an axis of
// the live wedgeCluster model (ENABLE_POPULATIONS = false), not a
// frozen copy — the twin breaks loudly if the live transition relation
// drifts.
//
// PRE-FIX behavior modeled: the systemic guard compares
// retained-evidence wedged nodes against the THIS-TICK fleet only
// (incommensurable populations), escapes entirely on an empty fleet,
// and an RPC-failed tick runs the full verdict over an empty view.
//
// PREDICTED: affectedLeOf falsifies within 2 ticks — tick 1 anchors
// two nodes' evidence inside a wide fleet (per-node verdict), tick 2
// observes a 1-node fleet while both retained nodes still qualify:
// Systemic{affected: 2, of: 1}. (The empty-fleet escape variant
// surfaces as noPerNodeFromSuppressedEvidence in the
// wedge-176-early-return twin, where the undrained episode supplies
// the retained anchors.)
//
// Recorded violation seed (quint run, --backend=rust):
//   0xa0fd4de6691f8839 — see the slot-5 wave-log entry.
//
// Run:
//   quint run --main=wedgeCalib009SplitPopulation \
//     --invariant=affectedLeOf \
//     docs/spec/models/calibration/wedge-009-split-population.qnt
module wedgeCalib009SplitPopulation {
  import wedgeCluster(
    MAX_TIME = 5,
    WINDOW = 3,
    MIN_DISTINCT = 2,
    ENABLE_POPULATIONS = false,
    ENABLE_FULL_EPILOGUE = true,
    ENABLE_EVICTION = true,
  ENABLE_EPISODE_LATCH = false,
    TRACK_EPISODE_GHOSTS = false,    ENABLE_TRAJECTORY = false,    BREADTH_MIN = 2,
    TRACK_GHOST_NODES = false,
    ENABLE_ADMISSION = true,
    ENABLE_SUPPRESSED_RETENTION = true,
    ENABLE_RELEASE_CLOSE = true,
    ENABLE_DWELL_ARM = false,
    DOWNGRADE_RUNS_CLOSE = false,
    VERDICT_DWELL_TICKS = 1
  ).* from "../wedgeCluster"

  /// Deterministic tick mirror for the OQ-12 acceptance run below
  /// (named inputs instead of `tick`'s nondet picks; rpcFail false).
  /// Runs-only addition: the module's step alphabet (the imported
  /// `step` = any { tick, reapMarked }) is untouched — the falsify
  /// twin's violation search and the P8 walk see the same actions as
  /// before.
  // Mirrors: rio-controller/src/reconcilers/nodeclaim_pool/wedge.rs:update
  action calib009Tick(expired: Set[(str, str)], healthy: Set[str]): bool = {
    val r = foldTick(expired, healthy, false)
    all {
      time < MAX_TIME,
      time' = time + 1,
      evidence' = r.evidence,
      marked' = r.marked,
      markedIncrements' = markedIncrements + r.delta,
      pendingEvictions' = pendingEvictions.exclude(r.consumed),
      reapHistory' = reapHistory,
      lastSystemic' = r.systemic,
      lastAffected' = r.affected,
      lastOf' = r.of_,
      lastWedged' = r.wedged,
      lastDelta' = r.delta,
      suppressedAnchors' = suppressedAnchors
        .union(r.episodeAnchors)
        .filter(a => and {
          r.evidence.keys().contains(a._1),
          nodeEvidence(r.evidence, a._1).keys().contains(a._2),
          nodeEvidence(r.evidence, a._1).get(a._2) == a._3,
        }),
      watermark' = r.watermark,
      expiredSince' = r.expiredSince,
      sawSystemic' = (sawSystemic or r.systemic),
      sawPerNode' = (sawPerNode or (not(r.systemic) and r.wedged.size() > 0)),
      sawSuppressed' = (sawSuppressed or r.episodeAnchors.size() > 0),
      lastSkip' = r.skip,
      lastSuppressedNoDrain' = r.suppressedNoDrain,
      lastShrankMarked' = r.shrankMarked,
      episodeState' = r.episodeState,
      sawRelease' = (sawRelease or r.releaseEdge),
      sawDowngrade' = (sawDowngrade or r.downgradeEdge),
    }
  }

  /// The merged_bug_009 acceptance trace, PRE-FIX half (OQ-12; the
  /// named-run form REPLACES the historical proptest seed
  /// 0xa0fd4de6691f8839 as the violation's pinned record): tick 1
  /// anchors n0's pair inside fleet {n0, n1} (per-node [n0]); tick 2
  /// observes ONLY n1's fresh pair — the retained n0 still qualifies,
  /// the this-tick fleet is {n1}, and the split-population guard
  /// mints the incommensurable Systemic{affected: 2, of: 1}.
  /// wedge.rs `mbt_replay_009_acceptance_pre_fix_trace_reds` replays
  /// this trace against the LIVE tracker (registered-fleet
  /// denominator: Systemic{2, of: 3}) and asserts the per-step diff
  /// REDS at exactly step 2; wedgeClusterReplay's
  /// `wedge009CommensurableRun` carries the green half.
  run wedge009AcceptanceRun =
    init
      .then(calib009Tick(Set(("n0", "d0"), ("n0", "d1")), Set("n1")))
      .then(calib009Tick(Set(("n1", "d0"), ("n1", "d1")), Set()))
      .expect(and {
        lastSystemic,
        lastAffected == 2,
        lastOf == 1,
        lastWedged == Set(),
      })
}
