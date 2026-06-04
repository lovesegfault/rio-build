// -*- mode: Bluespec; -*-
//
// Fence calibration, bug_261 (mint upsert TOCTOU under READ
// COMMITTED).
//
// PRE-FIX behavior modeled: the mint's DO UPDATE arm carries NO
// in-statement generation predicate — it applies whenever the
// ADVISORY floor compare passed at read time, even though a successor
// claimed and re-minted between the read and the write. The pause
// trace: r1 beginTx (sees floor 1) → r2 successorClaim(2) → r2
// beginTx + guardedUpsertCommit (row at gen 2, exec E2) → r1
// calibUnguardedUpsertCommit (snapshot still says 1 ≤ 1) → the row
// REGRESSES to gen 1 and E2 is clobbered.
//
// PREDICTED: activeRowGenMonotonic falsifies (rowRegressed latch).
//
// Run (falsification):
//   quint verify --backend=tlc --main=fenceCalib261UnguardedUpsert \
//     --step=calibStep --invariant=activeRowGenMonotonic \
//     docs/spec/models/calibration/fence-261-unguarded-upsert.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=fenceCalib261UnguardedUpsert \
//     --invariant=activeRowGenMonotonic \
//     docs/spec/models/calibration/fence-261-unguarded-upsert.qnt
module fenceCalib261UnguardedUpsert {
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

  /// PRE-FIX: the predicate-free DO UPDATE — applies on the stale
  /// advisory pass, regressing a newer committed row. DECISION-ONLY
  /// perturbation (bughunt-2 bug_358): the EvalPlanQual re-check is
  /// SKIPPED; the latch truth comes from the live upsertApply oracle.
  action calibUnguardedUpsertCommit(r: str): bool = all {
    match txPhase.get(r) {
      | TxIdle => false
      | TxFloorRead(v) => v <= replicaGen.get(r) // advisory passed (stale)
    },
    execsMinted < MAX_EXECS,
    txPhase' = txPhase.set(r, TxIdle),
    val snap = match txPhase.get(r) { | TxIdle => 0 | TxFloorRead(v) => v }
    // Applies UNCONDITIONALLY (no EvalPlanQual re-check); still stamps
    // the writer's own tenure — the defect is the skipped predicate,
    // not the stamp (that axis is fence-338's).
    upsertApply(r, snap, replicaGen.get(r)),
    pendingRefusal' = pendingRefusal,
    claimsUnchanged, floorUnchanged,
    planesUnchanged,
  }

  action calibStep = {
    nondet r = REPLICAS.oneOf()
    any {
      step,
      calibUnguardedUpsertCommit(r),
    }
  }
}
