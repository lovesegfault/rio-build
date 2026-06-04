// -*- mode: Bluespec; -*-
//
// Fence calibration, merged_bug_011 (outbox replays stale absolute
// batches).
//
// PRE-FIX behavior modeled: the outbox flush re-drives a latched
// batch VERBATIM — no flush-time re-derivation (stale entries are
// applied, not dropped) and a DERIVATION-scoped assignment close (the
// active row is closed whatever exec it carries). The pause trace:
// r1 latchFailedPersist (latch = exec E1) → r1 advanceDerivation (the
// resubmit re-mints the active row to E2; the latch is stale) → r1
// calibFlushAbsolute — the shared replay oracle (flushReplayApply)
// latches BOTH defects: staleReplayApplied (a stale batch landed) and
// successorExecClosed (the close hit E2 ≠ the latched E1).
//
// The DECISIONS are the only perturbation: the calibration passes
// (dropStale = false, execScoped = false) where the fixed system
// passes (true, true) — the apply sub-action and its live oracles are
// shared (formal-policy P2/P5).
//
// PREDICTED: outboxReplayNeverRegresses falsifies AND
// outboxClosesOnlyLatchedExecs falsifies (two wired witness checks,
// one module).
//
// Run (falsification):
//   quint verify --backend=tlc --main=fenceCalib011AbsoluteReplay \
//     --step=calibStep --invariant=outboxReplayNeverRegresses \
//     docs/spec/models/calibration/fence-011-absolute-replay.qnt
//   quint verify --backend=tlc --main=fenceCalib011AbsoluteReplay \
//     --step=calibStep --invariant=outboxClosesOnlyLatchedExecs \
//     docs/spec/models/calibration/fence-011-absolute-replay.qnt
// Baseline (as-built step, must HOLD): discharged by the wired
// exhaustive check (quint-fenced-writes, all planes live).
module fenceCalib011AbsoluteReplay {
  import fencedWrites(
    MAX_GEN = 3,
    MAX_EXECS = 4,
    REPLICAS = Set("r1", "r2"),
    ENABLE_ATOMIC_REREAD = false,
    // Plane 2 LIVE (latch/advance are the defect's setup); the other
    // planes dormant.
    ENABLE_OUTBOX = true,
    ENABLE_TENURE_LIFECYCLE = false
  ).* from "../fencedWrites"

  /// PRE-FIX: the verbatim absolute replay — no re-derivation, close
  /// scoped to the derivation (whatever exec is active).
  action calibFlushAbsolute(r: str): bool =
    flushReplayApply(r, false, false)

  action calibStep = {
    nondet r = REPLICAS.oneOf()
    any {
      step,
      calibFlushAbsolute(r),
    }
  }
}
