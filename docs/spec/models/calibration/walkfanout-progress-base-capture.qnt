// -*- mode: Bluespec; -*-
//
// walkFanoutProgress calibration, base-capture (merged_bug_012 — the
// future-side spawn-base arithmetic; noFalseCompletion's falsifier
// and, on its own wired lane, emittedDoneNeverRegresses').
//
// PRE-LAW behavior modeled: each path emits the clamped
// spawn-captured-base pair (max(floor, base+streamed),
// max(base+declared, done')) instead of the driver fold's
// (floor + Σ streamed, floor + Σ declared). Two arithmetic
// consequences at width > 1 (no race needed):
//   - once a sibling commit raises the floor past a still-streaming
//     path's base+declared, every tick from that path clamps to
//     done == expected == floor — the false-100% cell
//     (noFalseCompletion);
//   - interleaved siblings' raw dones differ by their stream gap, so
//     the emitted done sequence regresses on alternating frames —
//     the oscillation cell (emittedDoneNeverRegresses).
//
// DECISION-ONLY perturbation: `calibStreamBaseCapture` is
// `streamSeat(true)`; every other action is the live one.
//
// PREDICTED (noFalseCompletion), two reachable faces:
//   - per-path face: spawnP(1), spawnP(2) (bases 0); stream p1 once:
//     done = max(0, 0+1) = 1, expected = max(0+2, 1) = 2 — fine; a
//     second p1 tick: done = 2 == expected = 2 while p2's 3 declared
//     are outstanding (the per-path pair ignores the sibling's work);
//   - floor-clamp face: spawn both; stream p2 x3; commitP(2)
//     (floor 3 > p1's base+declared = 2); stream p1 once:
//     done = max(3, 1) = 3 == expected = max(2, 3) = 3 with p1's
//     outstanding 1 — the false-100% the Rust red pinned.
//
// PREDICTED (emittedDoneNeverRegresses): spawn both; stream p2 twice
//   (done 2); stream p1 (done = max(0, 1) = 1 < 2) -> regression.
//
// Run (falsification):
//   quint run --invariant=noFalseCompletion --step=calibStep \
//     --main=walkFanoutProgressBaseCapture \
//     docs/spec/models/calibration/walkfanout-progress-base-capture.qnt
//   quint run --invariant=emittedDoneNeverRegresses --step=calibStep \
//     --main=walkFanoutProgressBaseCapture \
//     docs/spec/models/calibration/walkfanout-progress-base-capture.qnt
module walkFanoutProgressBaseCapture {
  import walkFanoutProgress.* from "../walkFanout"

  /// PRE-LAW: the future-side clamped spawn-base emission.
  action calibStreamBaseCapture = streamSeat(true)

  action calibStep = any {
    spawnP,
    calibStreamBaseCapture,
    commitP,
    idleP,
  }
}
