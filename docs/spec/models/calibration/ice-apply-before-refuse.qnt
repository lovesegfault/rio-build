// -*- mode: Bluespec; -*-
//
// Bughunt-5 slot-5 calibration override, the merged_bug_008 headline
// axis (+ bug_094's silent-partial-apply ordering): the as-built
// handle_ack_spawned_intents applied the cell planes FIRST and only
// then erred CostGateClosed (snapshot.rs:1032-1075 pre-fix order) —
// an erring Ack did NOT imply "no plane landed", so the controller's
// retained-buffer redelivery re-applied the planes every ~10s tick.
// The production fix is AckApplyPlan::validate computing EVERY
// refusal before the first mutation (commit infallible by
// signature).
//
// PRE-FIX behavior modeled: calibRefuseAfterApply applies the
// in-flight request through the live apply law and THEN answers the
// refusal (buffer retained, nothing drained) — the errApplied ghost
// latches when the pre-refusal apply mutated ladder state.
//
// PREDICTED: errImpliesNoMutation falsifies.
//
// Run (falsification):
//   quint run --backend=rust --main=iceCalibApplyBeforeRefuse \
//     --step=calibStep --invariant=errImpliesNoMutation \
//     docs/spec/models/calibration/ice-apply-before-refuse.qnt
module iceCalibApplyBeforeRefuse {
  import iceEvidenceAck(MAX_EVENTS = 4).* from "../iceEvidenceAck"

  /// THE BUG: the refusal answers AFTER the planes applied. The
  /// ladder/gate writes are the live apply law verbatim; the buffer
  /// is retained (refusals never drain) and the ghost records
  /// whether the refused delivery mutated scheduler state.
  action calibRefuseAfterApply: bool = all {
    inflight != RNone,
    val r = reqOf(inflight)
    val res = CELLS.mapBy(c => applyCellG(true, lad.get(c), la.get(c), r.get(c), MAX_EVENTS))
    all {
      lad' = CELLS.mapBy(c => res.get(c).lad),
      la' = CELLS.mapBy(c => res.get(c).la),
      errApplied' = (errApplied or CELLS.exists(c =>
        (res.get(c).lad != lad.get(c)) or (res.get(c).la != la.get(c)))),
      // The defective order still realizes chronology bookkeeping
      // exactly like a delivery (the planes DID land).
      ctmViolated' = (ctmViolated or CELLS.exists(c => ctmViolCell(pendingCtm.get(c), r.get(c), res.get(c)))),
      pendingCtm' = CELLS.mapBy(c => if (r.get(c).clear != ONone) false else pendingCtm.get(c)),
      staleChanged' = (staleChanged or CELLS.exists(c => res.get(c).stale)),
      remaskedByStale' = (remaskedByStale or CELLS.exists(c => res.get(c).remask)),
      buf' = buf,
      inflight' = RNone,
      stash' = stash,
      mint' = mint,
    }
  }

  action calibStep = {
    nondet c = CELLS.oneOf()
    any {
      obsClear(c),
      obsMark(c),
      ship,
      deliverOk,
      deliverOkLost,
      // the override REPLACES refuse
      calibRefuseAfterApply,
      lateDuplicate(true),
      localClear(c),
      maskExpire(c),
    }
  }
}
