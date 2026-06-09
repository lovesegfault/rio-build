// -*- mode: Bluespec; -*-
//
// Bughunt-5 slot-5 calibration override, the merged_bug_003 shape:
// the as-built buffer_marks did `registered_cells.remove(&c);
// ice_cells.insert(c)` (evidence.rs:65-70 pre-fix) — a newer mark
// DESTROYED the buffered consume-once Registered=True clear. The
// planes are not symmetric polarities scheduler-side (clear is the
// only ladder reset; mark climbs from the RETAINED step), so the
// eviction was strictly lossy: the request shipped mark-only and the
// scheduler climbed from its stale rung instead of realizing
// reset-then-step-0. The production fix is the per-cell ordered
// CellEvidence with the ClearThenMark variant shipping BOTH planes
// (clear epoch < mark epoch).
//
// PRE-FIX behavior modeled: calibObsMarkEvicting replaces the
// ordered-buffer mark transition with latest-wins eviction — any
// buffered clear is dropped, the cell buffers Mark(new) only. The
// pendingCtm ghost still latches the TRUE chronology (the spec's
// view of what the producer observed); the defective buffer then
// ships a mark-only request and the applied mark lands above step 0.
//
// PREDICTED: clearThenMarkRealizesReset falsifies.
//
// Run (falsification):
//   quint run --backend=rust --main=iceCalibLatestWinsEviction \
//     --step=calibStep --invariant=clearThenMarkRealizesReset \
//     docs/spec/models/calibration/ice-latest-wins-eviction.qnt
module iceCalibLatestWinsEviction {
  import iceEvidenceAck(MAX_EVENTS = 4).* from "../iceEvidenceAck"

  /// THE BUG: a newer mark evicts the buffered clear (latest-wins
  /// set arithmetic) instead of retaining it as clear-then-mark.
  action calibObsMarkEvicting(c: str): bool = all {
    inflight == RNone,
    mint < MAX_EVENTS,
    mint' = mint + 1,
    buf' = buf.set(c, BMark(mint + 1)),
    // The ghost records the chronology the defective buffer destroys
    // (same latch edge as the live obsMark: a buffered clear existed
    // when the mark arrived).
    pendingCtm' = pendingCtm.set(c, match buf.get(c) {
      | BClear(_) => true
      | BCtm(_) => pendingCtm.get(c)
      | BNone => pendingCtm.get(c)
      | BMark(_) => pendingCtm.get(c)
    }),
    inflight' = inflight, stash' = stash, lad' = lad, la' = la,
    errApplied' = errApplied, staleChanged' = staleChanged,
    remaskedByStale' = remaskedByStale, ctmViolated' = ctmViolated,
  }

  action calibStep = {
    nondet c = CELLS.oneOf()
    any {
      obsClear(c),
      // the override REPLACES obsMark
      calibObsMarkEvicting(c),
      ship,
      deliverOk,
      deliverOkLost,
      refuse,
      lateDuplicate(true),
      localClear(c),
      maskExpire(c),
    }
  }
}
