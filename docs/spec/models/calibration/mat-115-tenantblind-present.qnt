// -*- mode: Bluespec; -*-
//
// Slot-10 walk-fold calibration override, the bug_115 tenant-blind local presence:
// The walk's local-presence probe consulted only
// manifests.status='complete': raw physical presence was sufficient
// to pin, count verified, and extend the frontier — a local row the
// sig-visibility gate hides from every interested tenant
// (substitution-only under untrusted keys, or another tenant's built
// output per I-217) was laundered into every interested build's
// durable per-tenant ownership at consumption. The production fix
// requires the TenantVisible witness on the Present arm; the verdict
// is the kernel's I-217 table, shared with the gRPC read gates.
//
// PREDICTED: verifiedPathsVisibleToInterest falsifies on a present-but-invisible row.
//
// Run (falsification):
//   quint run --main=matCalibTenantblindPresent --init=init --step=calibStep \
//     --invariant=verifiedPathsVisibleToInterest --max-samples=400000 --max-steps=12 \
//     docs/spec/models/calibration/mat-115-tenantblind-present.qnt
// Baseline (the lawful fold, must HOLD):
//   quint run --main=matCalibTenantblindPresent --init=init --step=step \
//     --invariant=verifiedPathsVisibleToInterest --max-samples=100000 --max-steps=12 \
//     docs/spec/models/calibration/mat-115-tenantblind-present.qnt
module matCalibTenantblindPresent {
  import materializationJob(
    BUILDS = Set("b1", "b2"),
    DRVS = Set("d1"),
    OUTPUTS = Set("o1", "o2"),
    REPLICAS = Set("r1", "r2"),
    BUILDERS = Set("w1"),
    MAT_BUDGET = 2,
    MAX_EXECS = 4,
    MAX_GEN = 0,
    MAX_UPSTREAM_FLIPS = 2,
    MAX_LEGACY = 0,
    ENABLE_FAILOVER = false,
    ENABLE_GC = false,
    ENABLE_CRASH = false,
    ENABLE_STALE_TENURE = false,
    ENABLE_RESOLVE_FAULTS = false,
    TENANTS = Set("tA", "tB"),
    ENABLE_WALK_FOLD = true,
    ENABLE_WORKER_ABORTED = false
  ).* from "../materializationJob"

  /// PRE-FIX fold: the SAME armed cells, ONE decision derived the
  /// pre-fix way, routed through the live seat (P5: a calibration
  /// perturbs a decision, never a latch).
  action calibTenantblindPresent = all {
    walkFoldApply(
      or { walkCells.served != Set(), armedLocalVisible },
      // THE BUG: raw physical presence is sufficient — the local
      // serve ignores the sig-visibility cells entirely (the
      // gate-hidden row is pinned, counted verified, and laundered
      // into per-tenant ownership at consumption).
      walkCells.localPresent != Set(),
      armedConfirmedMissing,
      and { walkCells.charging != Set(), walkCells.served == Set() }
    ),
  }

  action calibStep = any {
    step,
    all { calibTenantblindPresent, machineFrame },
  }
}
