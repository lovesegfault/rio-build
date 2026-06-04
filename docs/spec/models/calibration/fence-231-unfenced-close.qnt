// -*- mode: Bluespec; -*-
//
// Fence calibration, merged_bug_231 (derivation-keyed unfenced close).
//
// PRE-FIX behavior modeled: the establishment adopt arm closed the
// assignments row via a derivation_id-keyed UPDATE with no fence and
// no exec scope — a deposed replica's stale sweep could close the
// SUCCESSOR's active row (the open attempt vanishes under the live
// leader).
//
// PREDICTED: openAttemptViewStableUnderDeposedClose falsifies
// (deposedCloseApplied latch).
//
// Run (falsification):
//   quint verify --backend=tlc --main=fenceCalib231UnfencedClose \
//     --step=calibStep --invariant=openAttemptViewStableUnderDeposedClose \
//     docs/spec/models/calibration/fence-231-unfenced-close.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=fenceCalib231UnfencedClose \
//     --invariant=openAttemptViewStableUnderDeposedClose \
//     docs/spec/models/calibration/fence-231-unfenced-close.qnt
module fenceCalib231UnfencedClose {
  import fencedWrites(
    MAX_GEN = 3,
    MAX_EXECS = 4,
    REPLICAS = Set("r1", "r2"),
    // bughunt-2 planes DORMANT: this legacy calibration's state space
    // stays identical (the plane actions are disabled; the plane vars
    // are framed by planesUnchanged in the override action below).
    ENABLE_ATOMIC_REREAD = false,
    ENABLE_OUTBOX = false,
    ENABLE_TENURE_LIFECYCLE = false
  ).* from "../fencedWrites"

  /// PRE-FIX: derivation-keyed close — no begin_fenced, no exec
  /// scope: closes WHATEVER row is active for the derivation.
  /// DECISION-ONLY perturbation (bughunt-2 bug_358): the owner/fence
  /// guards are SKIPPED; the deposed-foreign-close truth comes from
  /// the live closeApply oracle (fenceless writers believe
  /// currentFloor).
  action calibUnfencedDerivationClose(r: str): bool = all {
    replicaGen.get(r) > 0,
    activeGen > 0,
    closeApply(r, currentFloor),
    txPhase' = txPhase,
    pendingRefusal' = pendingRefusal,
    claimsUnchanged, floorUnchanged,
    planesUnchanged,
  }

  action calibStep = {
    nondet r = REPLICAS.oneOf()
    any {
      step,
      calibUnfencedDerivationClose(r),
    }
  }
}
