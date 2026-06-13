// -*- mode: Bluespec; -*-
//
// Calibration override / regression pin, bughunt2-wave slot 8
// bug_181 (rio-lease marks_dirty): re-introduces the BOOL dirty flag's
// clear semantics — the pre-fix world in which the reconcile loop's
// success path ran store(false), erasing EVERY dirtying event
// regardless of when it landed (rio-lease/src/lib.rs pre-fix; the
// is_leader-ordering premise and the polarity re-check were the
// partial patches the DirtyGen arithmetic deleted).
//
// PRE-FIX behavior modeled (clear-all instead of clear-through):
//   - calibClearAll replaces reconcileClear: `cleared' = marked` — the
//     bool's store(false) collapses the generation pair to "clean now"
//     no matter what marked after the spawn snapshot.
//
// PREDICTED falsification: notClobbered. A dirtying site that fires
// between the spawn snapshot and the clear — a leadership edge, or
// the merged_bug_212 rebound hook — is erased by the clear-all, which
// is exactly the parked-PATCH clobber of the bug_181 red
// (rio-lease/src/lib.rs:3304 schedule). marksDivergenceBounded falls
// with it on the stale-write shapes (ByWrite divergence left
// undiscovered), but notClobbered is the wired witness: it pins the
// ARITHMETIC claim independently of any age window.
//
// This pin is the non-vacuity evidence for quint-leader-marks's
// notClobbered verdict: if it ever stops falsifying, the model has
// drifted away from the race the generation counters exist to absorb.
//
// Run (the wired pin — the bool-clear world falsifies):
//   quint verify --backend=tlc --main=leaseCalib181BoolClear \
//     --step=calibStep --invariant=notClobbered \
//     docs/spec/models/calibration/lease-181-bool-clear.qnt
module leaseCalib181BoolClear {
  import leaderMarks(
    VERIFY_EVERY = 3,
    MAX_TIME = 10,
    MAX_STRIPS = 1,
    MAX_ADDS = 0,
    MAX_VERIFY_FAILS = 0,
    MAX_RECONCILE_FAILS = 2,
    MAX_HOLDER_CHANGES = 2,
    MAX_REBOUNDS = 1,
    TASK_TIMEOUT = 4
  ).* from "../leaderMarks"

  /// PRE-FIX clear: store(false) — clear EVERYTHING, including marks
  /// that landed after the spawn snapshot. The clobber detector is the
  /// live model's, unchanged: it fires when this clear goes clean over
  /// a post-snapshot mark.
  action calibClearAll = {
    val newCleared = marked
    all {
      match task { | Patched(_) => true | _ => false },
      cleared' = newCleared,
      clobberErased' = (clobberErased or
        (newCleared >= marked and marked > taskSnap)),
      task' = NoTask,
      holder' = holder, marks' = marks, marked' = marked,
      clock' = clock, lastVerify' = lastVerify,
      strips' = strips, adds' = adds, verifyFails' = verifyFails, reconcileFails' = reconcileFails, holderChanges' = holderChanges,
      rebounds' = rebounds,
      wrongSince' = stampWrong(marks, holder, wrongSince, clock),
      wrongCause' = stampCause(marks, holder, wrongSince, wrongCause, NoCause),
    }
  }

  /// The live step with reconcileClear swapped for the bool clear-all
  /// (one mechanism reverted; everything else the live model's).
  action calibStep = any {
    tickRound,
    holderChangeAny,
    reboundRedirty,
    externalStripAny,
    verify,
    reconcileSpawn,
    reconcilePatched,
    calibClearAll,
    reconcileFail,
  }
}
