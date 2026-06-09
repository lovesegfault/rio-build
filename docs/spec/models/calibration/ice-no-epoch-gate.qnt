// -*- mode: Bluespec; -*-
//
// Bughunt-5 slot-5 calibration override, merged_bug_008's three
// redelivery axes: pre-fix the scheduler had NO per-cell evidence
// epoch gate — every delivered mark unconditionally re-stamped
// `until = now + ttl` (axis 1: masks pinned across a gate-closed
// redelivery window), a redelivery landing after mask expiry climbed
// the ladder though it was the SAME observation (axis 2:
// next_mark_step(Some((s,false))) = s+1), and a redelivered retained
// mark after the §13a local clear minted a fresh mask over the
// just-proven-healthy cell (axis 3 — self-sustaining, because masked
// cells are excluded from dispatch so no superseding clear can
// arrive). The production fix is IceBackoff::last_applied +
// apply_{mark,clear}_event gating `epoch > last_applied[cell]`.
//
// PRE-FIX behavior modeled: every delivery path applies with the
// gate OFF (`gate = false` through the live applyCellG — stale
// events apply; the stale/remask ghost bits latch what they
// mutated).
//
// PREDICTED: redeliveryIdempotent falsifies (axes 1/2/4 — the stale
// re-apply), and healthyCellNeedsNewEvidence falsifies (axis 3 —
// the post-local-clear re-mask). Each is wired as its own witness
// check (quint-policy P1: one twin run per holds invariant).
//
// Run (falsification):
//   quint run --backend=rust --main=iceCalibNoEpochGate \
//     --step=calibStep --invariant=redeliveryIdempotent \
//     docs/spec/models/calibration/ice-no-epoch-gate.qnt
//   quint run --backend=rust --main=iceCalibNoEpochGate \
//     --step=calibStep --invariant=healthyCellNeedsNewEvidence \
//     docs/spec/models/calibration/ice-no-epoch-gate.qnt
module iceCalibNoEpochGate {
  import iceEvidenceAck(MAX_EVENTS = 4).* from "../iceEvidenceAck"

  /// THE BUG: deliveries apply without the epoch gate.
  action calibDeliverOkNoGate = deliverApply(false, true, false)
  action calibDeliverOkLostNoGate = deliverApply(false, false, true)
  action calibLateDuplicateNoGate = lateDuplicate(false)

  action calibStep = {
    nondet c = CELLS.oneOf()
    any {
      obsClear(c),
      obsMark(c),
      ship,
      // the overrides REPLACE the gated delivery lanes
      calibDeliverOkNoGate,
      calibDeliverOkLostNoGate,
      refuse,
      calibLateDuplicateNoGate,
      localClear(c),
      maskExpire(c),
    }
  }
}
