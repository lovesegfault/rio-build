// -*- mode: Bluespec; -*-
//
// Calibration override / regression pin, bughunt2-wave slot 8
// merged_bug_212 (rio-scheduler cost latch): re-introduces the
// ACQUIRE-ONLY rebound delivery — the pre-fix world in which a
// lose→re-acquire the lease loop observed as one transition was
// delivered as a bare LeaderAcquired, so the LEADER_EDGES cost-latch
// lose cell never ran (rio-scheduler pre-fix: the rebound arm called
// on_acquire; cost_was_leader stayed true across the unobserved
// holder change — the recorded red).
//
// PRE-FIX behavior modeled (the lose cell skipped):
//   - calibReboundAcquireOnly replaces reboundCompound: the latch is
//     UNTOUCHED — the acquire cell does not write the cost latch, and
//     no lose cell runs.
//
// PREDICTED falsification: noStalePersist. A foreign tenure persists
// inside the rebound gap (the interim leader's evolved EMA); the
// still-true latch makes the next leading tick skip the reload and
// run the body — the deposed tenure's table, reloaded a tenure ago,
// overwrites the evolved world (tableEpoch < worldEpoch at the
// persist).
//
// This pin is the non-vacuity evidence for quint-cost-latch: if it
// ever stops falsifying, the model has drifted away from the skip
// the Compound delivery exists to prevent.
//
// Run (the wired pin — the acquire-only world falsifies):
//   quint verify --backend=tlc --main=costlatchCalib212AcquireOnly \
//     --step=calibStep --invariant=noStalePersist \
//     docs/spec/models/calibration/costlatch-212-acquire-only.qnt
module costlatchCalib212AcquireOnly {
  import costLatch(
    MAX_EDGES = 4,
    MAX_TICKS = 6,
    MAX_FOREIGN = 1
  ).* from "../costLatch"

  /// PRE-FIX rebound: delivered as a bare LeaderAcquired — no lose
  /// cell, latch untouched. The foreign-tenure nondet pick is the
  /// live model's.
  action calibReboundAcquireOnly = {
    nondet during = Set(true, false).oneOf()
    all {
      edges < MAX_EDGES,
      leading,
      during implies foreignPersists < MAX_FOREIGN,
      leading' = true,
      latch' = latch,
      worldEpoch' = if (during) worldEpoch + 1 else worldEpoch,
      foreignPersists' = if (during) foreignPersists + 1 else foreignPersists,
      tableEpoch' = tableEpoch,
      stalePersisted' = stalePersisted, persists' = persists,
      edges' = edges + 1, ticks' = ticks,
    }
  }

  /// The live step with the Compound rebound swapped for the
  /// acquire-only delivery (one cell skipped; everything else the
  /// live model's).
  action calibStep = any {
    acquireEdge,
    loseEdge,
    calibReboundAcquireOnly,
    foreignTenurePersists,
    pollerTickStandby,
    pollerTickReload,
    pollerTickBody,
  }
}
