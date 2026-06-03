// -*- mode: Bluespec; -*-
//
// Fence calibration, bug_393 (terminal fence-refusal answer).
//
// PRE-FIX behavior modeled: the fence refusal surfaced as
// FAILED_PRECONDITION — a code no client retries. The client GIVES UP
// on a request the live leader would have accepted.
//
// PREDICTED: fenceRefusalAlwaysRetryable falsifies (clientGaveUp
// latch).
//
// Run (falsification):
//   quint verify --backend=tlc --main=fenceCalib393TerminalRefusal \
//     --step=calibStep --invariant=fenceRefusalAlwaysRetryable \
//     docs/spec/models/calibration/fence-393-terminal-refusal.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=fenceCalib393TerminalRefusal \
//     --invariant=fenceRefusalAlwaysRetryable \
//     docs/spec/models/calibration/fence-393-terminal-refusal.qnt
module fenceCalib393TerminalRefusal {
  import fencedWrites(
    MAX_GEN = 3,
    MAX_EXECS = 4,
    REPLICAS = Set("r1", "r2")
  ).* from "../fencedWrites"

  /// PRE-FIX: the refusal answered with a terminal code.
  action calibAnswerTerminal(r: str): bool = all {
    pendingRefusal.get(r),
    pendingRefusal' = pendingRefusal.set(r, false),
    clientGaveUp' = true,
    rowRegressed' = rowRegressed,
    belowFloorMutation' = belowFloorMutation,
    floorRegressed' = floorRegressed,
    deposedCloseApplied' = deposedCloseApplied,
    txPhase' = txPhase,
    claimsUnchanged, rowUnchanged, floorUnchanged,
  }

  action calibStep = {
    nondet r = REPLICAS.oneOf()
    any {
      step,
      calibAnswerTerminal(r),
    }
  }
}
