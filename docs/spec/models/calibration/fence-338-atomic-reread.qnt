// -*- mode: Bluespec; -*-
//
// Fence calibration, merged_bug_338 (tenure stamped from a fresh
// lease-atomic read).
//
// PRE-FIX behavior modeled: a fenced write stamps the LIVE LEASE
// ATOMIC instead of the claim-stamped tenure (the typed
// ServingGeneration). The defect window: a mid-tenure
// re-acquire/rebound raises the atomic ABOVE the claim stamp
// (atomicBumpReacquire), and the next mint relabels its row with a
// generation the claims ledger never vouched for. The pause trace:
// r1 beginTx → r1 atomicBumpReacquire (atomic 1→2; claim stamp stays
// 1) → r1 calibMintFromAtomic — the shared apply oracle
// (guardedUpsertCommitAs) latches staleWriteCommitted because the
// committed stamp (2) is not r1's claim-stamped tenure (1).
//
// The DECISION is the only perturbation: the calibration passes
// `atomicGen` where the fixed system passes `replicaGen` — the apply
// sub-action and its live oracle are shared (formal-policy P2/P5: no
// frozen copies, no calibration latch writes).
//
// PREDICTED: writesCarryClaimedTenure falsifies.
//
// Run (falsification):
//   quint verify --backend=tlc --main=fenceCalib338AtomicReread \
//     --step=calibStep --invariant=writesCarryClaimedTenure \
//     docs/spec/models/calibration/fence-338-atomic-reread.qnt
// Baseline (as-built step, must HOLD): discharged by the wired
// exhaustive check (quint-fenced-writes, all planes live).
module fenceCalib338AtomicReread {
  import fencedWrites(
    MAX_GEN = 3,
    MAX_EXECS = 4,
    REPLICAS = Set("r1", "r2"),
    // Plane 1 LIVE (the bump event is the defect's window); the other
    // planes dormant.
    ENABLE_ATOMIC_REREAD = true,
    ENABLE_OUTBOX = false,
    ENABLE_TENURE_LIFECYCLE = false
  ).* from "../fencedWrites"

  /// PRE-FIX: the mint stamps the fresh atomic read.
  action calibMintFromAtomic(r: str): bool =
    guardedUpsertCommitAs(r, atomicGen.get(r))

  action calibStep = {
    nondet r = REPLICAS.oneOf()
    any {
      step,
      calibMintFromAtomic(r),
    }
  }
}
