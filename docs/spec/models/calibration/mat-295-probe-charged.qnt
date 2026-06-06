// -*- mode: Bluespec; -*-
//
// Slot-10 walk-fold calibration override, the bug_295 probe-leg 429 charge:
// The miss-confirmation HEAD probe laundered terminal 429s into
// `indeterminate`, which the executor charged as infrastructure: a
// rate-limit wave on the PROBE leg burned the park budget the
// attempt leg's transient lane was already shielding. The production
// fix routes the probe leg through the SAME class truth table
// (classify_substitute_failure): 429 -> RetryUncharged, 5xx/timeout
// stays charged (congruence per CLASS, owner Q23).
//
// PREDICTED: rateLimitedProbeDrawsNoBudget falsifies on an all-429 fold.
//
// Run (falsification):
//   quint run --main=matCalibProbeCharged --init=init --step=calibStep \
//     --invariant=rateLimitedProbeDrawsNoBudget --max-samples=400000 --max-steps=12 \
//     docs/spec/models/calibration/mat-295-probe-charged.qnt
// Baseline (the lawful fold, must HOLD):
//   quint run --main=matCalibProbeCharged --init=init --step=step \
//     --invariant=rateLimitedProbeDrawsNoBudget --max-samples=100000 --max-steps=12 \
//     docs/spec/models/calibration/mat-295-probe-charged.qnt
module matCalibProbeCharged {
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
  action calibProbeCharged = all {
    walkFoldApply(
      or { walkCells.served != Set(), armedLocalVisible },
      armedLocalVisible,
      armedConfirmedMissing,
      // THE BUG: the probe leg laundered terminal 429s into the
      // charging class — rate-limited evidence draws the budget
      // exactly like a 5xx (the park-burning harm case).
      and {
        or { walkCells.charging != Set(), walkCells.rateLimited != Set() },
        walkCells.served == Set(),
      }
    ),
  }

  action calibStep = any {
    step,
    all { calibProbeCharged, machineFrame },
  }
}
