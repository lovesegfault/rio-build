// -*- mode: Bluespec; -*-
//
// Fence calibration, bug_155 (a failed-recovery tenure completes and
// serves).
//
// PRE-FIX behavior modeled: the retired "degrade, don't block"
// doctrine — a tenure whose recovery never succeeded completes
// anyway (set_recovery_complete over an empty, non-authoritative
// DAG: the zombie leader). The pause trace: r2 successorClaim (a new
// tenure, recoveredOk = false) → r2 calibCompleteUnwitnessed — the
// shared completion oracle (completeTenureApply) latches
// servedWithoutRecovery because the caller holds no recovered
// witness. The fixed system's completeTenure is precondition-gated on
// recoveredOk (the RecoveredDag witness: minted only by a successful
// load, consumed only by complete_tenure) and a failed tenure takes
// stepDownFailedTenure instead — this state was previously
// MODEL-UNREPRESENTABLE; the plane makes the zombie expressible so
// its impossibility in the fixed system is a checked invariant, not
// an assumption.
//
// The DECISION is the only perturbation: the calibration calls the
// apply without the witness precondition — the apply sub-action and
// its live oracle are shared (formal-policy P2/P5).
//
// PREDICTED: failedRecoveryNeverServes falsifies.
//
// Run (falsification):
//   quint verify --backend=tlc --main=fenceCalib155ServeAfterFailedRecovery \
//     --step=calibStep --invariant=failedRecoveryNeverServes \
//     docs/spec/models/calibration/fence-155-serve-after-failed-recovery.qnt
// Baseline (as-built step, must HOLD): discharged by the wired
// exhaustive check (quint-fenced-writes, all planes live).
module fenceCalib155ServeAfterFailedRecovery {
  import fencedWrites(
    MAX_GEN = 3,
    MAX_EXECS = 4,
    REPLICAS = Set("r1", "r2"),
    ENABLE_ATOMIC_REREAD = false,
    ENABLE_OUTBOX = false,
    // Plane 3 LIVE; the other planes dormant.
    ENABLE_TENURE_LIFECYCLE = true
  ).* from "../fencedWrites"

  /// PRE-FIX: completion without the recovered witness (the caller
  /// passes its actual — possibly absent — recovery state straight
  /// through, no gate).
  action calibCompleteUnwitnessed(r: str): bool =
    completeTenureApply(r, recoveredOk.get(r))

  action calibStep = {
    nondet r = REPLICAS.oneOf()
    any {
      step,
      calibCompleteUnwitnessed(r),
    }
  }
}
