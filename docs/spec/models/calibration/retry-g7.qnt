// -*- mode: Bluespec; -*-
//
// Stage-C calibration overrides, family G7: "fleet-exhaust / placement
// interaction" (retry-invariant-map.md, calibration table; inventory
// section 5). Same extend-and-override pattern as retry-g1.qnt.

// =========================================================================
// CALIBRATION -- pre-699ad52e1 (the draining-exclusion root cause): the
// fleet-exhaust check counts draining workers as eligible, so a one-shot
// (poolSize=1) fleet whose only worker just failed the derivation and is
// draining out reads as "exhausted" and the derivation is poisoned on its
// first failure -- instead of deferring until a replacement registers
// (today's empty-fleet defer, sched.dispatch.fleet-exhaust+3).
// PREDICTED: the fleet-exhaust poison becomes reachable where the
// as-built encoding cannot reach it -- checked as the noFleetExhaustPoison
// probe (an invariant here, not a witness): HOLDS on baselineStep,
// falsifies on calibStep. The respawn action is deliberately excluded
// from both steps: the model's slot identity makes a respawned slot
// inherit the exclusion, which is the separate (as-built, witnessed)
// fleet-exhaust path and would mask the draining-specific regression.
// =========================================================================
module retryCalibG7ExhaustCountsDraining {
  import retryPolicyAsBuilt(
    EXECUTORS = Set("w1"),
    MAX_TIME = 2,
    MAX_ATTEMPTS = 2,
    ATTEMPT_BOUND = 2,
    MAX_FAILOVERS = 0,
    PG_FAULTS = 0,
    ENABLE_WORKER = true,
    ENABLE_PODDEATH = false,
    ENABLE_CRASH = false,
    ENABLE_WEDGE = false,
    ENABLE_RESETS = false
  ).* from "../retryPolicyAsBuilt"

  /// PRE-FIX E9: the exhaust predicate is evaluated over every REGISTERED
  /// slot, draining or not -- the draining filter the fix added is gone.
  action dispatchFleetExhaustCountsDraining = all {
    dStatus == DReady,
    backoffElapsed,
    placeable == Set(),
    val eligibleIncludingDraining = EXECUTORS.filter(w => execs.get(w).registered)
    exhaustsFleet(live.failedBuilders, eligibleIncludingDraining),
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
    pgFaultsLeft' = pgFaultsLeft,
    last' = { ...noObs, kind: OE9Dispatch, terminal: true },
    justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  action buildEndsTransientAny = { nondet w = EXECUTORS.oneOf() buildEndsReported(w, RcTransient, false) }
  action processTransientAny = {
    nondet w = EXECUTORS.oneOf()
    processReport(w, RcTransient, false, false, false, false)
  }

  /// The pre-fix step (no respawn -- see the module header).
  action calibStep = any {
    tick,
    dispatchToAny,
    buildEndsTransientAny,
    processTransientAny,
    dispatchFleetExhaustCountsDraining,
  }

  /// The as-built restriction of the same alphabet: the real exhaust
  /// check (draining excluded, empty fleet defers).
  action baselineStep = any {
    tick,
    dispatchToAny,
    buildEndsTransientAny,
    processTransientAny,
    dispatchFleetExhaust,
  }
}
