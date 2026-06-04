// -*- mode: Bluespec; -*-
//
// Fence calibration, floor-blind admission (bughunt-2 bug_358 —
// belowFloorTxNeverMutates' FIRST falsifier).
//
// PRE-FIX behavior modeled: the begin_fenced advisory compare is
// DROPPED entirely — a transaction whose snapshot floor exceeds its
// replica's generation (the fence said NO) pushes its mint through
// anyway. The pause trace: r2 successorClaim(2) raises the floor to 2
// -> r1 (generation 1) beginTx snapshots floor 2 -> r1
// calibFloorBlindUpsert mutates decision state with snap 2 > gen 1 ->
// the snapshotExceedsGen oracle latches belowFloorMutation.
//
// DECISION-ONLY perturbation: the admission guard flips; the latch
// truth comes from the live upsertApply oracle.
//
// PREDICTED: belowFloorTxNeverMutates falsifies (belowFloorMutation
// latch).
//
// Run (falsification):
//   quint verify --backend=tlc --main=fenceCalibFloorBlind \
//     --step=calibStep --invariant=belowFloorTxNeverMutates \
//     docs/spec/models/calibration/fence-floor-blind.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=fenceCalibFloorBlind \
//     --invariant=belowFloorTxNeverMutates \
//     docs/spec/models/calibration/fence-floor-blind.qnt
module fenceCalibFloorBlind {
  import fencedWrites(
    MAX_GEN = 3,
    MAX_EXECS = 4,
    REPLICAS = Set("r1", "r2"),
    // bughunt-2 planes DORMANT: this calibration targets the legacy
    // floor-admission decision; the plane actions are disabled and the
    // plane vars are framed by planesUnchanged in the override below.
    ENABLE_ATOMIC_REREAD = false,
    ENABLE_OUTBOX = false,
    ENABLE_TENURE_LIFECYCLE = false
  ).* from "../fencedWrites"

  /// PRE-FIX: the admission compare DROPPED — the fence said no, the
  /// transaction mutates anyway.
  action calibFloorBlindUpsert(r: str): bool = all {
    match txPhase.get(r) {
      | TxIdle => false
      | TxFloorRead(v) => v > replicaGen.get(r) // the fence SAID no
    },
    execsMinted < MAX_EXECS,
    txPhase' = txPhase.set(r, TxIdle),
    val snap = match txPhase.get(r) { | TxIdle => 0 | TxFloorRead(v) => v }
    upsertApply(r, snap, replicaGen.get(r)),
    pendingRefusal' = pendingRefusal,
    claimsUnchanged, floorUnchanged,
    planesUnchanged,
  }

  action calibStep = {
    nondet r = REPLICAS.oneOf()
    any {
      step,
      calibFloorBlindUpsert(r),
    }
  }
}
