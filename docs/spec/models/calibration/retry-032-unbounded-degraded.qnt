// -*- mode: Bluespec; -*-
//
// Bughunt-2 slot 3 calibration override, the unbounded-degraded shape
// (merged_bug_032's pre-fix intake): before admit_store_degraded
// existed, EVERY worker-flagged store-degraded report took the
// uncharged paced close — worker-trusted, unbounded. A degraded (or
// lying) worker could mint uncharged requeues forever: no budget ever
// drawn, no exclusion ever minted, the derivation cycling free while
// the scheduler believed the store.
//
// PRE-FIX behavior modeled: calibUnboundedStoreDegraded replaces
// pullStoreDegraded — same trigger and same uncharged close, but with
// NO run-bound admission guard (and no charged fallthrough in the
// alphabet: the pre-fix intake had no such arm). The trailing-run
// counter still advances — it is the OBSERVER of consecutive uncharged
// closes, not part of the pre-fix mechanism.
//
// PREDICTED: boundedStoreDegradedRun falsifies on the
// (STORE_DEGRADED_FREE_RUN+1)-th consecutive flagged report (the run
// passes the kernel bound with nothing charged). The uncharged
// contract triple (storeDegradedNeverPoisons / DrawsNoBudget /
// MintsNoExclusion) still HOLDS under the override — the pre-fix bug
// is unboundedness, not mischarging; that direction is exactly what
// the triple alone could never falsify (its vacuous direction, closed
// here).
//
// Run (falsification):
//   quint verify --backend=tlc --main=retryCalibUnboundedDegraded \
//     --step=calibStep --invariant=boundedStoreDegradedRun \
//     docs/spec/models/calibration/retry-032-unbounded-degraded.qnt
// Baseline (the as-shipped step, must HOLD):
//   quint verify --backend=tlc --main=retryCalibUnboundedDegraded \
//     --step=pullStep --invariant=boundedStoreDegradedRun \
//     docs/spec/models/calibration/retry-032-unbounded-degraded.qnt
module retryCalibUnboundedDegraded {
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

  /// PRE-FIX intake: the as-shipped pullStoreDegraded minus the
  /// admission guard — the uncharged paced close with no run bound.
  /// Body otherwise verbatim (pacing, not charges: no fold event,
  /// nothing moves; the SD row still breaks the worker-abort run and
  /// extends its own).
  action calibUnboundedStoreDegraded(n: str): bool = all {
    ENABLE_STORE_DEGRADED,
    attempt == Running(n),
    dStatus == DRunning,
    attempt' = NoAttempt,
    dStatus' = DReady,
    execs' = execs.put(n, { ...execs.get(n), runningOurs: false }),
    freeCloseRun' = 0,
    storeDegradedRun' = storeDegradedRun + 1,
    spec' = spec, specVerdict' = specVerdict, live' = live,
    pg' = pg,
    last' = { ...noObs, kind: OStoreDegraded },
    now' = now, poisonSrc' = poisonSrc,
    depStatus' = depStatus, everFailureTerminal' = everFailureTerminal,
    promotionsLeft' = promotionsLeft, physAttempts' = physAttempts,
    failoversLeft' = failoversLeft, txFaultsLeft' = txFaultsLeft,
    lastPoisonReason' = lastPoisonReason, justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  action calibUnboundedStoreDegradedAny = {
    nondet n = EXECUTORS.oneOf() calibUnboundedStoreDegraded(n)
  }

  /// pullStep with the override replacing the as-shipped
  /// pullStoreDegraded disjunct and the run-bound fallthrough REMOVED
  /// (the pre-fix intake had no charged arm; everything else verbatim).
  action calibStep = any {
    all { tick, buildLeavesMatUntouched },
    all { pullOpensAttemptAny, buildLeavesMatUntouched },
    all { spawnGateExhaust, buildLeavesMatUntouched },
    all { buildEndsReportedAny, buildLeavesMatUntouched },
    all { buildEndsPodDeathAny, buildLeavesMatUntouched },
    all { pullReportOutcomeAny, buildLeavesMatUntouched },
    all { pullAttemptOutcomeOomAny, buildLeavesMatUntouched },
    all { pullAttemptOutcomeDeadlineAny, buildLeavesMatUntouched },
    all { pullAttemptOutcomeNoOpAny, buildLeavesMatUntouched },
    all { pullWorkerAbortAny, buildLeavesMatUntouched },
    all { calibUnboundedStoreDegradedAny, buildLeavesMatUntouched },
    all { ctrlReportLostAny, buildLeavesMatUntouched },
    all { pullEstablishAny, buildLeavesMatUntouched },
    all { sourceLeavesPoolAny, buildLeavesMatUntouched },
    all { resubmitReset, buildLeavesMatUntouched },
    all { cacheHitClear, buildLeavesMatUntouched },
    all { poisonTtlExpires, buildLeavesMatUntouched },
    all { pullLeaderFailover, buildLeavesMatUntouched },
  }

  /// Warmup alias: the Apalache-server prelude's warm-up compile uses
  /// quint's default step name; without a `step` in scope the warm-up
  /// QNT404s silently and the verify races the server's cold-conversion
  /// window (the empty-details INTERNAL shape). The alias makes the
  /// warm-up convert THIS module deterministically.
  action step = calibStep
}
