// -*- mode: Bluespec; -*-
//
// Slot-10 walk-fold calibration override, the merged_bug_133 in-loop tenant return:
// The executor's per-tenant loop returned the job-level disposition
// from INSIDE the loop body on the first charging failure — the
// deterministic resolve order (creating-build hint first) starved
// every later tenant of its chance to serve: a dead first tenant
// turned an obtainable path into InfraFailure. The production fix
// accumulates evidence cells only; ALL failure dispositions exit at
// the post-loop fold after every tenant has been consulted
// (fold_tenant_attempts; owner-Q2 restored).
//
// PREDICTED: hitUnderAnyTenantOrderSucceeds falsifies when a charging tenant precedes a serving one.
//
// Run (falsification):
//   quint run --main=matCalibInloopReturn --init=init --step=calibStep \
//     --invariant=hitUnderAnyTenantOrderSucceeds --max-samples=400000 --max-steps=12 \
//     docs/spec/models/calibration/mat-133-inloop-return.qnt
// Baseline (the lawful fold, must HOLD):
//   quint run --main=matCalibInloopReturn --init=init --step=step \
//     --invariant=hitUnderAnyTenantOrderSucceeds --max-samples=100000 --max-steps=12 \
//     docs/spec/models/calibration/mat-133-inloop-return.qnt
module matCalibInloopReturn {
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
  action calibInloopReturn = all {
    walkFoldApply(
      // THE BUG: the in-loop return — a charging tenant earlier in
      // the deterministic resolve order aborts the walk before the
      // serving tenant is consulted: the fold serves only when NO
      // tenant charged.
      and { walkCells.served != Set(), walkCells.charging == Set() },
      armedLocalVisible,
      armedConfirmedMissing,
      and { walkCells.charging != Set(), walkCells.served == Set() }
    ),
  }

  action calibStep = any {
    step,
    all { calibInloopReturn, machineFrame },
  }
}
