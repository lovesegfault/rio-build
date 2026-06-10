// -*- mode: Bluespec; -*-
//
// walkFanout calibration, spawn-unbounded (live_047/R-C —
// windowWithinF's falsifier).
//
// PRE-LAW behavior modeled: the spawn phase ignores the F window —
// every frontier path spawns immediately (the unbounded fan-out the
// `path_fanout` lever and the slot pool exist to forbid: per-pod
// demand becomes n×closure-width on the admission gate and
// n×closure-width whole-NAR reservations on the byte budget).
//
// Perturbation: `calibSpawnUnbounded` is the live seat's body with
// the width conjunct deleted (the latch guard stays intact — this
// twin kills exactly the width clause; the latch clause's falsifier
// is the spawn-after-abort twin).
//
// PREDICTED: windowWithinF falsifies via spawn -> spawn -> spawn
//   (inflight reaches 3 > F = 2).
//
// Run (falsification):
//   quint run --invariant=windowWithinF --step=calibStep \
//     --main=walkFanoutSpawnUnbounded \
//     docs/spec/models/calibration/walkfanout-spawn-unbounded.qnt
module walkFanoutSpawnUnbounded {
  import walkFanout.* from "../walkFanout"

  /// PRE-LAW: width-blind spawn (the live `spawn` minus the
  /// `inflight.size() < F` conjunct; latch guard intact).
  action calibSpawnUnbounded = {
    nondet p = oneOf(pending)
    all {
      verdict == "",
      not(latched),
      pending' = pending.exclude(Set(p)),
      inflight' = inflight.union(Set(p)),
      spawnedPostLatch' = spawnedPostLatch,
      visitedM' = visitedM,
      dupQueued' = dupQueued,
      served' = served,
      settledCells' = settledCells,
      abortsDone' = abortsDone,
      cancelled' = cancelled,
      latched' = latched,
      floor' = floor,
      gen' = gen,
      verdict' = verdict,
      firstAbortVerdict' = firstAbortVerdict,
    }
  }

  action calibStep = any {
    calibSpawnUnbounded,
    complete,
    cancelSibling,
    reResolve,
    compileOutcome,
    idle,
  }
}
