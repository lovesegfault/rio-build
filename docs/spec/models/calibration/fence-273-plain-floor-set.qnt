// -*- mode: Bluespec; -*-
//
// Fence calibration, bug_273 (plain-SET resource-floor write).
//
// PRE-FIX behavior modeled: update_resource_floor wrote
// `SET floor = $2` — no fence AND no server-side GREATEST ratchet.
// Two interleaved writers (or one writer with a stale base) regress
// the floor even under a single live leader; a deposed replica's
// write regresses it across tenures.
//
// PREDICTED: resourceFloorMonotonic falsifies (floorRegressed latch).
//
// Run (falsification):
//   quint verify --backend=tlc --main=fenceCalib273PlainFloorSet \
//     --step=calibStep --invariant=resourceFloorMonotonic \
//     docs/spec/models/calibration/fence-273-plain-floor-set.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=fenceCalib273PlainFloorSet \
//     --invariant=resourceFloorMonotonic \
//     docs/spec/models/calibration/fence-273-plain-floor-set.qnt
module fenceCalib273PlainFloorSet {
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

  /// PRE-FIX: the unfenced plain SET — no ratchet, no floor check.
  /// DECISION-ONLY perturbation (bughunt-2 bug_358): the GREATEST
  /// ratchet is DROPPED (v passes verbatim); the regression truth
  /// comes from the live floorWriteApply oracle.
  action calibFloorWritePlainSet(r: str, v: int): bool = all {
    replicaGen.get(r) > 0,
    floorWriteApply(r, currentFloor, v),
    txPhase' = txPhase,
    pendingRefusal' = pendingRefusal,
    claimsUnchanged, rowUnchanged,
    planesUnchanged,
  }

  action calibStep = {
    nondet r = REPLICAS.oneOf()
    nondet v = 1.to(MAX_GEN).oneOf()
    any {
      step,
      calibFloorWritePlainSet(r, v),
    }
  }
}
