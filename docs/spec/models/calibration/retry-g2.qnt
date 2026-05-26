// -*- mode: Bluespec; -*-
//
// Stage-C calibration overrides, family G2: "a counter could not serve two
// roles at once" (retry-invariant-map.md, calibration table; inventory
// section 5). Same extend-and-override pattern as retry-g1.qnt.

// =========================================================================
// CALIBRATION -- pre-a4bcb5623: retry.count is overloaded as both the
// per-cycle max_retries gate and the cross-cycle resubmit-bound gate, so
// the dag::merge resubmit reset neither restores the per-cycle budget nor
// advances any cross-cycle counter: the carried count exhausts max_retries
// on the first failure of every new cycle, and POISON_RESUBMIT_RETRY_LIMIT
// (compared against a counter that is reset-bounded below it) never fires.
// PREDICTED: countersRefineHistory falsifies at the first resubmit reset
// (the fold zeroes the per-cycle counters and counts the cycle), in a
// worker-channel alphabet with resets enabled where the as-built encoding
// holds it. The unbounded poison-resubmit loop is the downstream
// consequence (the bounded-resubmit guard never engages).
// =========================================================================
module retryCalibG2ResubmitSharedCounter {
  import retryPolicyAsBuilt(
    EXECUTORS = Set("w1", "w2"),
    MAX_TIME = 2,
    MAX_ATTEMPTS = 4,
    ATTEMPT_BOUND = 4,
    MAX_FAILOVERS = 0,
    PG_FAULTS = 0,
    ENABLE_WORKER = true,
    ENABLE_PODDEATH = false,
    ENABLE_CRASH = false,
    ENABLE_WEDGE = false,
    ENABLE_RESETS = true
  ).* from "../retryPolicyAsBuilt"

  /// PRE-FIX resubmit reset: clears the poison status (and the dependent's
  /// DependencyFailed) but carries the per-cycle counters over and never
  /// advances a cross-cycle counter -- there is no resubmit_cycles, and
  /// the resubmit bound (compared against the overloaded count) never
  /// fires, so the guard admits every retriable terminal state
  /// unconditionally.
  action resubmitResetShared(pgLost: bool): bool = all {
    ENABLE_RESETS,
    or { dStatus == DCancelled, dStatus == DPoisoned },
    pgLost implies pgFaultsLeft > 0,
    val applied = specApply(spec, FResubmitReset(now), eligibleFleet)
    all {
      dStatus' = DReady,
      poisonSrc' = NoPoison,
      // PRE-FIX: the failure history of the previous cycle is carried
      // verbatim into the new cycle (only the poison stamp is lifted);
      // no cycle counter exists to advance.
      live' = { ...live, poisonedAt: NoTime },
      spec' = applied.c,
      specVerdict' = applied.v,
      pg' = if (pgLost) pg else { ...pg, status: PgNonTerminal, poisonedAt: NoTime },
      depStatus' = DepReady,
      last' = { ...noObs, kind: OResubmit },
    },
    everFailureTerminal' = everFailureTerminal,
    attempt' = attempt, execs' = execs,
    now' = now, promotionsLeft' = promotionsLeft, physAttempts' = physAttempts,
    failoversLeft' = failoversLeft,
    pgFaultsLeft' = if (pgLost) pgFaultsLeft - 1 else pgFaultsLeft,
    lastPoisonReason' = lastPoisonReason, justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  action resubmitResetSharedAny = { nondet pgLost = Set(false).oneOf() resubmitResetShared(pgLost) }
  action buildEndsTransientAny = { nondet w = EXECUTORS.oneOf() buildEndsReported(w, RcTransient, false) }
  action processTransientAny = {
    nondet w = EXECUTORS.oneOf()
    processReport(w, RcTransient, false, false, false, false)
  }

  action calibStep = any {
    tick,
    dispatchToAny,
    buildEndsTransientAny,
    processTransientAny,
    resubmitResetSharedAny,
    respawnExecutorAny,
  }
}
