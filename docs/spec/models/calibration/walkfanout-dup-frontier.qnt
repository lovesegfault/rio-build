// -*- mode: Bluespec; -*-
//
// walkFanout calibration, dup-frontier (merged_bug_003 — the
// two-timestamp dedup; frontierSpawnable's falsifier).
//
// PRE-LAW behavior modeled: the frontier admission gate checks
// containment-in-PENDING (the apply push's `!visited.contains` over a
// `visited` that is only inserted at SPAWN) instead of
// membership-in-visitedM (marked at ENQUEUE) — an already-SPAWNED
// path's re-offer (the closure-diamond apply push / cross-chain
// arrival) LANDS as a duplicate frontier entry: a membership lie that
// justifies a blocking pod-wide slot acquire for zero spawnable work
// (the phantom acquire the Rust red pins behind a foreign-held pool).
//
// DECISION-ONLY perturbation: `calibReEnqueueTwoTimestamp` is
// `reEnqueueSeat(false)` — the seat's dedup gate flipped to the
// pending-containment shape; every other action is the live one.
//
// PREDICTED: frontierSpawnable falsifies via spawn(p) (p leaves
//   pending) -> calibReEnqueueTwoTimestamp picking p (p ∉ pending,
//   gate admits) -> dupQueued = 1.
//
// Kill isolation: both window leaves (windowWithinF,
// noSpawnAfterLatch) stay INTACT under calibStep — their own holds
// lane (quint-walkfanout-calib-dup-frontier-isolation).
//
// Run (falsification):
//   quint run --invariant=frontierSpawnable --step=calibStep \
//     --main=walkFanoutDupFrontier \
//     docs/spec/models/calibration/walkfanout-dup-frontier.qnt
module walkFanoutDupFrontier {
  import walkFanout.* from "../walkFanout"

  /// PRE-LAW: the two-timestamp gate — re-offers are screened against
  /// PENDING containment, so a spawned path's duplicate lands.
  action calibReEnqueueTwoTimestamp = reEnqueueSeat(false)

  action calibStep = any {
    spawn,
    complete,
    cancelSibling,
    reResolve,
    calibReEnqueueTwoTimestamp,
    compileOutcome,
    idle,
  }
}
