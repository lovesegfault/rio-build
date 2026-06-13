// -*- mode: Bluespec; -*-
//
// Fence calibration, OP-5 (bughunt-13: the standby's queued write —
// the sched.lease.standby-drops-writes falsification twin).
//
// PRE-FIX behavior modeled: the begin-time floor compare DROPPED for
// a never-led standby's queued write — the exact exposure the spec
// rule's old standing waved away with an enabledness argument ("the
// pull surfaces are leader-gated"): a standby holds a pooled
// connection and a queued write; if only the gRPC gate stands
// between that write and the tables, a dispatch race or a future
// ungated surface commits it under a stale (zero) generation.
//
// DECISION-ONLY perturbation (bughunt-2 bug_358): the admission
// compare is skipped for the gen-0 replica; the write routes through
// the live upsertApply seat believing currentFloor (the fenceless
// writer convention) and the snapshotExceedsGen oracle latches the
// below-floor mutation by itself.
//
// PREDICTED: nonHolderWriteNeverCommits falsifies
// (belowFloorMutation latch — the seat shared with
// belowFloorTxNeverMutates; see the val doc in fencedWrites.qnt).
//
// Run (falsification):
//   quint verify --backend=tlc --main=fenceCalibOp5StandbyWrite \
//     --step=calibStep --invariant=nonHolderWriteNeverCommits \
//     docs/spec/models/calibration/fence-op5-standby-write.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=fenceCalibOp5StandbyWrite \
//     --invariant=nonHolderWriteNeverCommits \
//     docs/spec/models/calibration/fence-op5-standby-write.qnt
module fenceCalibOp5StandbyWrite {
  import fencedWrites(
    MAX_GEN = 3,
    MAX_EXECS = 4,
    REPLICAS = Set("r1", "r2"),
    // bughunt-2 planes DORMANT: the perturbed surface is the begin
    // fence; the plane actions stay disabled and the plane vars are
    // framed by planesUnchanged in the override below.
    ENABLE_ATOMIC_REREAD = false,
    ENABLE_OUTBOX = false,
    ENABLE_TENURE_LIFECYCLE = false
  ).* from "../fencedWrites"

  /// PRE-FIX: the standby's queued write commits — no begin fence.
  /// The write stamps the standby's own (zero) generation, exactly
  /// what a non-holder's pooled connection would carry.
  // Environment: PRE-FIX strawman — the begin-time floor compare
  // dropped for a never-led standby; no production transition
  // implements it (the law pin's falsifiability pair).
  action calibStandbyQueuedWrite(r: str): bool = all {
    replicaGen.get(r) == 0,
    execsMinted < MAX_EXECS,
    upsertApply(r, currentFloor, replicaGen.get(r)),
    staleWriteCommitted' = staleWriteCommitted,
    txPhase' = txPhase,
    pendingRefusal' = pendingRefusal,
    claimsUnchanged, floorUnchanged,
    atomicUnchanged, outboxUnchanged, lifecycleUnchanged,
  }

  action calibStep = {
    nondet r = REPLICAS.oneOf()
    any {
      step,
      calibStandbyQueuedWrite(r),
    }
  }
}
