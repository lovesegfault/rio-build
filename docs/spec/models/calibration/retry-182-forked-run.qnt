// -*- mode: Bluespec; -*-
//
// Bughunt-4 S5b calibration override, the forked-run shape (bug_182's
// pre-fix fold): decide()'s pacing-run reset was gated on
// `row_to_event().is_some()`, so a build-lane MARKER row that folds to
// no event — the E9 fleet-exhaust verdict, the cascade rows — broke
// the ADMISSION scan (is_bounded_uncharged_row answers false → the
// trailing-run walk stops) but never reset the FOLD's pacing curve.
// The signed union-run law (bughunt-3 §5-S Q1) named ONE run; the code
// had two: a post-break store-degraded report was admitted fresh while
// paced at the dead run's cap.
//
// PRE-FIX behavior modeled: calibSpawnGateExhaustKeepsRuns replaces
// spawnGateExhaust — same trigger, same poison verdict, same E9
// marker observation, but BOTH run counters survive the marker row
// (`storeDegradedRun' = storeDegradedRun`, `freeCloseRun' =
// freeCloseRun`), exactly the shape the live model carried before the
// fix ("no fold event — the marker row is a fold no-op"). The fixed
// model resets both (the marker row is a build-lane row outside the
// bounded-uncharged union: run_step answers Break for BOTH consumers).
//
// PREDICTED: markerRowBreaksRuns falsifies — a pacing prefix
// (uncharged store-degraded closes) followed by the spawn-gate
// exhaust reaches `last.kind == OE9Dispatch` with a nonzero run: the
// fold's curve survives a row the admission scan breaks on.
//
// Run (falsification — the gating wiring is
// quint-retry-policy-calib-182-forked-run, expect-violation):
//   quint verify --backend=tlc --main=retryCalibForkedRun \
//     --step=calibStep --invariant=markerRowBreaksRuns \
//     docs/spec/models/calibration/retry-182-forked-run.qnt
// Baseline (the as-shipped step, must HOLD):
//   quint verify --backend=tlc --main=retryCalibForkedRun \
//     --step=pullStep --invariant=markerRowBreaksRuns \
//     docs/spec/models/calibration/retry-182-forked-run.qnt
module retryCalibForkedRun {
  import retryPolicy(
    EXECUTORS = Set("n1", "n2"),
    MAX_TIME = 3,
    MAX_ATTEMPTS = 6,
    ATTEMPT_BOUND = 6,
    MAX_FAILOVERS = 1,
    TX_FAULTS = 0,
    ENABLE_WORKER = true,
    ENABLE_PODDEATH = true,
    ENABLE_CRASH = true,
    ENABLE_WEDGE = false,
    ENABLE_RESETS = true,
    ENABLE_MATERIALIZATION = false,
    ENABLE_STORE_DEGRADED = true
  ).* from "../retryPolicy"

  /// PRE-FIX spawn-gate exhaust: the as-shipped action minus the run
  /// resets — the E9 marker row leaves both pacing counters standing
  /// (the fold's `row_to_event().is_some()` gate skipped them). Body
  /// otherwise verbatim.
  action calibSpawnGateExhaustKeepsRuns = all {
    freeCloseRun' = freeCloseRun,
    storeDegradedRun' = storeDegradedRun,
    dStatus == DReady,
    backoffElapsed,
    attempt == NoAttempt,
    exhaustsFleet(live.failedBuilders, spawnableSources),
    dStatus' = DPoisoned,
    poisonSrc' = DispatchPoison,
    live' = { ...live, poisonedAt: At(now) },
    lastPoisonReason' = RFleetExhausted,
    pg' = { ...pg, status: PgPoisoned, poisonedAt: At(now) },
    depStatus' = DepFailed,
    everFailureTerminal' = true,
    spec' = spec, specVerdict' = specVerdict,
    attempt' = attempt, execs' = execs,
    now' = now, promotionsLeft' = promotionsLeft,
    physAttempts' = physAttempts, failoversLeft' = failoversLeft,
    txFaultsLeft' = txFaultsLeft,
    last' = { ...noObs, kind: OE9Dispatch, terminal: true },
    justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  /// pullStep with the pre-fix spawn-gate arm swapped in; every other
  /// disjunct is the as-shipped action.
  action calibStep = any {
    all { tick, buildLeavesMatUntouched },
    all { pullOpensAttemptAny, buildLeavesMatUntouched },
    all { calibSpawnGateExhaustKeepsRuns, buildLeavesMatUntouched },
    all { buildEndsReportedAny, buildLeavesMatUntouched },
    all { buildEndsPodDeathAny, buildLeavesMatUntouched },
    all { pullReportOutcomeAny, buildLeavesMatUntouched },
    all { pullAttemptOutcomeOomAny, buildLeavesMatUntouched },
    all { pullAttemptOutcomeDeadlineAny, buildLeavesMatUntouched },
    all { pullAttemptOutcomeNoOpAny, buildLeavesMatUntouched },
    all { pullWorkerAbortAny, buildLeavesMatUntouched },
    all { pullStoreDegradedAny, buildLeavesMatUntouched },
    all { pullStoreDegradedRunBoundAny, buildLeavesMatUntouched },
    all { ctrlReportLostAny, buildLeavesMatUntouched },
    all { pullEstablishAny, buildLeavesMatUntouched },
    all { sourceLeavesPoolAny, buildLeavesMatUntouched },
    all { resubmitReset, buildLeavesMatUntouched },
    all { cacheHitClear, buildLeavesMatUntouched },
    all { poisonTtlExpires, buildLeavesMatUntouched },
    all { pullLeaderFailover, buildLeavesMatUntouched },
    materializationReportUnobtainable,
    materializationReportInfra,
    establishMaterializationCrash,
  }

  /// Warmup alias (see the calibration file's note in retryPolicy):
  /// lets the Apalache-server warm-up convert this module under
  /// quint's default step name.
  action step = calibStep
}
