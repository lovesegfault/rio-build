// -*- mode: Bluespec; -*-
//
// Slot-10 walk-fold calibration override, the bug_299 whole-set reprobe projection:
// fold_tenant_reprobes consumed per-tenant PRE-FOLDED booleans (each
// tenant: "something I want is missing under my view") and AND'd
// them. Complementary coverage — tenant A has o2 only, tenant B has
// o1 only — made both booleans true and the fold answered
// ConfirmedMissing for a closure that was fully obtainable across
// the tenant set. The production fix moves the quantifier INTO the
// kernel: fold_path_reprobes consumes RAW per-(tenant, path) cells
// and answers ConfirmedMissing iff exists path missing under EVERY
// tenant (rio-evidence-kernel outcome.rs; the old fold is deleted +
// deny_shared'd).
//
// PREDICTED: reprobeCongruentWithCompletability falsifies on the complementary-coverage matrix.
//
// Run (falsification):
//   quint run --main=matCalibWholesetProjection --init=init --step=calibStep \
//     --invariant=reprobeCongruentWithCompletability --max-samples=400000 --max-steps=12 \
//     docs/spec/models/calibration/mat-299-wholeset-projection.qnt
// Baseline (the lawful fold, must HOLD):
//   quint run --main=matCalibWholesetProjection --init=init --step=step \
//     --invariant=reprobeCongruentWithCompletability --max-samples=100000 --max-steps=12 \
//     docs/spec/models/calibration/mat-299-wholeset-projection.qnt
module matCalibWholesetProjection {
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
  action calibWholesetProjection = all {
    walkFoldApply(
      or { walkCells.served != Set(), armedLocalVisible },
      armedLocalVisible,
      // THE BUG: the per-tenant pre-fold — every tenant is missing
      // SOMETHING (exists path per tenant), AND'd across tenants —
      // instead of the per-path quantifier. Complementary coverage
      // (tA missing o1 only, tB missing o2 only) folds
      // ConfirmedMissing here; the truth is Obtainable.
      and {
        walkCells.served == Set(),
        TENANTS.forall(t => OUTPUTS.exists(p => walkCells.missing.contains((t, p)))),
      },
      and { walkCells.charging != Set(), walkCells.served == Set() }
    ),
  }

  action calibStep = any {
    step,
    all { calibWholesetProjection, machineFrame },
  }
}
