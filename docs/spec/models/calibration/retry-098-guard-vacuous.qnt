// -*- mode: Bluespec; -*-
//
// Bughunt-3 S6a calibration, the composed-mint shape (bug_098's
// pre-fix admissions): admit_worker_abort and admit_store_degraded
// each counted only their OWN class's trailing run and broke on any
// other build-lane row — so each class's uncharged close appended
// exactly the row that reset the other class's run. Under strict
// alternation both runs idled at <= 1, neither signed bound ever
// tripped, and the uncharged-requeue mint was unbounded (the kernel
// regression red: "strict alternation minted 28 uncharged closes
// without ever reaching ChargedFallthrough").
//
// PRE-FIX behavior modeled — with one DISCLOSED abstraction: along
// every composed (alternating) schedule the pre-fix per-class guards
// are identically true (the mutual reset pins both runs <= 1), i.e.
// the guards are VACUOUS on the composed axis. They are therefore
// REMOVED here, which frees the base run vars to serve as UNION
// OBSERVERS of the mint (the pre-fix vars themselves cannot observe
// the mint they fail to bound — that blindness IS the bug). The
// abstraction over-approximates exactly one corner: PURE same-class
// runs, which the pre-fix guards DID bound — that corner is owned by
// the per-class twins (retry-032-unbounded-degraded for the degraded
// class; the kernel decision-table tests for the abort class). THIS
// twin isolates the composed axis the round-3 close adds.
//
// PREDICTED: unionMintBounded falsifies once the observed union mint
// exceeds WORKER_ABORT_FREE_CLOSES + STORE_DEGRADED_FREE_RUN (3+3=6
// at model scale) — seven guard-free uncharged closes in one
// bounded-uncharged run. canReachComposedRuns also violates here
// (and on the FIXED base step — it is the union law's reachability
// witness, not a defect marker).
//
// Run (falsification):
//   quint verify --backend=tlc --main=retryCalibGuardVacuous \
//     --step=calibStep --invariant=unionMintBounded \
//     docs/spec/models/calibration/retry-098-guard-vacuous.qnt
// Baseline (the as-shipped step, must HOLD):
//   quint verify --backend=tlc --main=retryCalibGuardVacuous \
//     --step=pullStep --invariant=unionMintBounded \
//     docs/spec/models/calibration/retry-098-guard-vacuous.qnt
module retryCalibGuardVacuous {
  import retryPolicy(
    EXECUTORS = Set("n1", "n2"),
    MAX_TIME = 3,
    MAX_ATTEMPTS = 8,
    ATTEMPT_BOUND = 8,
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

  /// PRE-FIX worker-abort close, guard vacuous on the composed axis:
  /// the as-shipped pullWorkerAbort minus the admission guard; the
  /// run vars advance as UNION observers (sibling preserved) so the
  /// mint is observable. Body otherwise verbatim (charge-free: no
  /// fold event, no counter movement, no backoff).
  action calibWorkerAbortVacuous(n: str): bool = all {
    attempt == Running(n),
    dStatus == DRunning,
    attempt' = NoAttempt,
    dStatus' = DReady,
    execs' = execs.put(n, { ...execs.get(n), runningOurs: false }),
    freeCloseRun' = freeCloseRun + 1,
    storeDegradedRun' = storeDegradedRun,
    spec' = spec, specVerdict' = specVerdict, live' = live,
    pg' = pg, last' = noObs,
    now' = now, poisonSrc' = poisonSrc,
    depStatus' = depStatus, everFailureTerminal' = everFailureTerminal,
    promotionsLeft' = promotionsLeft, physAttempts' = physAttempts,
    failoversLeft' = failoversLeft, txFaultsLeft' = txFaultsLeft,
    lastPoisonReason' = lastPoisonReason, justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  action calibWorkerAbortVacuousAny = {
    nondet n = EXECUTORS.oneOf() calibWorkerAbortVacuous(n)
  }

  /// PRE-FIX store-degraded paced close, guard vacuous on the
  /// composed axis: the as-shipped pullStoreDegraded minus the
  /// admission guard; union-observing run updates. Body otherwise
  /// verbatim (pacing, not charges: no fold event, nothing moves).
  action calibStoreDegradedVacuous(n: str): bool = all {
    ENABLE_STORE_DEGRADED,
    attempt == Running(n),
    dStatus == DRunning,
    attempt' = NoAttempt,
    dStatus' = DReady,
    execs' = execs.put(n, { ...execs.get(n), runningOurs: false }),
    freeCloseRun' = freeCloseRun,
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

  action calibStoreDegradedVacuousAny = {
    nondet n = EXECUTORS.oneOf() calibStoreDegradedVacuous(n)
  }

  /// pullStep with BOTH uncharged closes replaced by their
  /// guard-vacuous pre-fix forms (alphabet otherwise the
  /// retry-032 calibration's: the run-bound fallthrough is omitted —
  /// with the guards vacuous it is never the forced arm, and omitting
  /// it only removes escapes from the violating trace).
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
    all { calibWorkerAbortVacuousAny, buildLeavesMatUntouched },
    all { calibStoreDegradedVacuousAny, buildLeavesMatUntouched },
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
