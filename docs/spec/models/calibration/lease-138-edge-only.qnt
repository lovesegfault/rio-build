// -*- mode: Bluespec; -*-
//
// Calibration override / regression pin, bughunt-wave F1
// merged_bug_138 (rio-lease leader marks): re-introduces the
// EDGE-ONLY marks machinery — the pre-fix world in which marks_dirty
// had an enumerated writer set (init, acquire, lose, fence, task
// re-dirty) and NOTHING re-examined the stored marks between edges
// (rio-lease/src/lib.rs pre-fix: "Steady state spawns nothing").
//
// PRE-FIX behavior modeled (the verify pass removed):
//   - calibStep drops the `verify` action entirely;
//   - calibTickEdgeOnly replaces tickRound WITHOUT the verify-cadence
//     cap (no cadence obligation exists in the edge-only world).
//
// Updated for the bughunt2-wave slot-8 DirtyGen rework (bug_181): the
// parent model's dirty flag is now the marked/cleared generation pair,
// the completion is split into reconcilePatched/reconcileClear, and
// the divergence stamp is cause-tagged. This pin keeps the FIXED
// generation arithmetic and removes ONLY the verify pass — the two
// fixes are independent mechanisms and each twin reverts exactly one.
//
// PREDICTED falsification: marksDivergenceBounded. An external strip
// (foreign sweep racing a re-acquire, kubectl, any future actor)
// diverges OUR pod's marks with no edge writer firing; the clock
// advances past VERIFY_EVERY with the generations clean and no task —
// the ByStrip divergence ages unboundedly, which production paid as a
// leader-only Service outage until the next leadership transition.
//
// This pin is the non-vacuity evidence for quint-leader-marks: if it
// ever stops falsifying, the model has drifted away from the
// falsifier class the verify cadence exists to bound.
//
// Run (the wired pin — the edge-only world falsifies):
//   quint verify --backend=tlc --main=leaseCalib138EdgeOnly \
//     --step=calibStep --invariant=marksDivergenceBounded \
//     docs/spec/models/calibration/lease-138-edge-only.qnt
module leaseCalib138EdgeOnly {
  import leaderMarks(
    VERIFY_EVERY = 3,
    MAX_TIME = 10,
    MAX_STRIPS = 1,
    MAX_ADDS = 0,
    MAX_HOLDER_CHANGES = 2,
    MAX_REBOUNDS = 1,
    TASK_TIMEOUT = 4
  ).* from "../leaderMarks"

  /// PRE-FIX tick: no verify-cadence cap — the edge-only loop owes
  /// nothing between edges.
  action calibTickEdgeOnly = all {
    clock < MAX_TIME,
    // The task-residency cap is KEPT: API deadlines existed pre-fix
    // too — the deleted mechanism is the verify cadence only.
    (task != NoTask) implies (clock + 1 - taskSince <= TASK_TIMEOUT),
    clock' = clock + 1,
    holder' = holder, marks' = marks, marked' = marked,
    cleared' = cleared, task' = task,
    lastVerify' = lastVerify, strips' = strips, adds' = adds,
    holderChanges' = holderChanges, rebounds' = rebounds,
    clobberErased' = clobberErased,
    wrongSince' = stampWrong(marks, holder, wrongSince, clock + 1),
    wrongCause' = stampCause(marks, holder, wrongSince, wrongCause, NoCause),
  }

  /// The live step with the verify pass REMOVED and the tick cap
  /// dropped (one mechanism deleted; everything else the live
  /// model's — including the fixed clear-through).
  action calibStep = any {
    calibTickEdgeOnly,
    holderChangeAny,
    reboundRedirty,
    externalStripAny,
    reconcileSpawn,
    reconcilePatched,
    reconcileClear,
    reconcileFail,
  }
}
