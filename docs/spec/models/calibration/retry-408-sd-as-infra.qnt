// -*- mode: Bluespec; -*-
//
// Bughunt-wave B1 calibration override, the store-degraded-as-Infra
// shape (bug_408's pre-fix fold): before the StoreDegraded outcome
// class existed, a FUSE-breaker-flagged report was flattened into
// InfrastructureFailure — every flagged report drew the infra budget,
// minted the placement-exclusion key (kernel apply() adds
// failed_builders[source] on infra; this model's FInfra abstracts the
// exec away, so the override carries that production fact in the
// last-observation ghost) and the correlated outage marched the
// derivation to the cap and POISONED it (the recorded red: 11 flagged
// reports left `left: Poisoned`).
//
// PRE-FIX behavior modeled: calibStoreDegradedAsInfra replaces
// pullStoreDegraded — same trigger (a running worker's store-degraded
// report), but the intake folds it as plain Infra (FInfra, the
// RcInfraOther arm) instead of the uncharged paced requeue.
//
// PREDICTED: storeDegradedDrawsNoBudget and
// storeDegradedMintsNoExclusion falsify on the first override step;
// storeDegradedNeverPoisons falsifies once the infra cap/threshold
// poisons under the override (deeper trace, same calibration).
//
// Run (falsification):
//   quint verify --backend=tlc --main=retryCalibSdAsInfra \
//     --step=calibStep --invariant=storeDegradedDrawsNoBudget \
//     docs/spec/models/calibration/retry-408-sd-as-infra.qnt
// Baseline (the as-shipped step, must HOLD):
//   quint verify --backend=tlc --main=retryCalibSdAsInfra \
//     --step=pullStep --invariant=storeDegradedDrawsNoBudget \
//     docs/spec/models/calibration/retry-408-sd-as-infra.qnt
module retryCalibSdAsInfra {
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

  /// PRE-FIX intake: the store-degraded report charges as plain Infra
  /// (the merged_bug_178 flatten). Body mirrors the RcInfraOther arm
  /// (infraArm with exempt/atCap/consumesPromotion all false) plus the
  /// report-path environment bookkeeping; the last-observation ghost
  /// carries kind OStoreDegraded so the contract triple judges it.
  action calibStoreDegradedAsInfra(n: str): bool = all {
    attempt == Running(n),
    dStatus == DRunning,
    freeCloseRun' = 0,
    val applied = specApply(spec, FInfra({ at: now, exempt: false, atCap: false }), eligibleFleet)
    all {
      spec' = applied.c,
      specVerdict' = applied.v,
      live' = applied.c,
      execs' = execs.put(n, { ...execs.get(n), runningOurs: false }),
      attempt' = NoAttempt,
      promotionsLeft' = promotionsLeft,
      match applied.v {
        | VPoison(r) => all {
          dStatus' = DPoisoned,
          poisonSrc' = EventPoison,
          lastPoisonReason' = r,
          pg' = { status: PgPoisoned, poisonedAt: At(now), ledger: applied.c },
          depStatus' = DepFailed,
          everFailureTerminal' = true,
          last' = { ...noObs, kind: OStoreDegraded, chargedAny: true,
            exclusionMinted: true, terminal: true },
        }
        | VRequeue => all {
          dStatus' = DReady,
          poisonSrc' = poisonSrc,
          lastPoisonReason' = lastPoisonReason,
          pg' = { ...pg, ledger: applied.c },
          depStatus' = depStatus,
          everFailureTerminal' = everFailureTerminal,
          last' = { ...noObs, kind: OStoreDegraded, chargedAny: true,
            exclusionMinted: true },
        }
        | VCancel => all {
          dStatus' = DReady,
          poisonSrc' = poisonSrc,
          lastPoisonReason' = lastPoisonReason,
          pg' = { ...pg, ledger: applied.c },
          depStatus' = depStatus,
          everFailureTerminal' = everFailureTerminal,
          last' = { ...noObs, kind: OStoreDegraded, chargedAny: true,
            exclusionMinted: true },
        }
      },
    },
    now' = now,
    physAttempts' = physAttempts,
    failoversLeft' = failoversLeft, txFaultsLeft' = txFaultsLeft,
    justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  action calibStoreDegradedAsInfraAny = {
    nondet n = EXECUTORS.oneOf() calibStoreDegradedAsInfra(n)
  }

  /// pullStep with the override replacing the as-shipped
  /// pullStoreDegraded disjunct (everything else verbatim).
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
    all { calibStoreDegradedAsInfraAny, buildLeavesMatUntouched },
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
