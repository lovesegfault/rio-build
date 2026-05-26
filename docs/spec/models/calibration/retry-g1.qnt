// -*- mode: Bluespec; -*-
//
// Stage-C calibration overrides, family G1: "a counter was incremented on
// the wrong path, or not incremented on a path that needed it"
// (retry-invariant-map.md, calibration table; inventory section 5).
//
// Each module instantiates the frozen as-built retryPolicyAsBuilt model and swaps ONE
// entry point's action for a local PRE-FIX variant (the behavior the named
// historical fix removed), exposed through a `calibStep` that restricts the
// alphabet to what the scenario family needs -- the same extend-and-override
// calibration pattern the log campaign used, except the override is a local
// action + step (the retry model has no per-fix const switches) and the
// verify run selects it with `--step=calibStep`. The reference-fold ghost
// is still advanced by the as-built `specApply` at every observed event:
// the fold is the specification oracle and stays post-fix, which is what
// makes the refinement invariants the falsification surface.
//
// Verdicts, depths and state counts live in the calibration table in
// docs/spec/models/retry-invariant-map.md and in the introducing commit's
// message, never in this file.

// =========================================================================
// CALIBRATION -- pre-8283d4362 (half a): the I-127 window reset is not
// gated on the current event's at-cap floor outcome, so a stale-window
// at-cap OOM wipes the infra_count the at-cap accounting had accumulated
// (the m044 loop-at-ceiling shape).
// PREDICTED: countersRefineHistory falsifies (the fold does not reset on
// at-cap events) on a counted-infra-then-stale-at-cap-OOM history, in a
// worker-channel-only alphabet where the as-built encoding holds it.
// =========================================================================
module retryCalibG1WindowResetUngated {
  import retryPolicyAsBuilt(
    EXECUTORS = Set("w1", "w2"),
    MAX_TIME = 3,
    MAX_ATTEMPTS = 4,
    ATTEMPT_BOUND = 4,
    MAX_FAILOVERS = 0,
    PG_FAULTS = 0,
    ENABLE_WORKER = true,
    ENABLE_PODDEATH = false,
    ENABLE_CRASH = false,
    ENABLE_WEDGE = false,
    ENABLE_RESETS = false
  ).* from "../retryPolicyAsBuilt"

  /// PRE-FIX E2 (worker-reported infra classes only): identical to the
  /// as-built infraArm except the window-reset guard drops the
  /// `not(atCap)` conjunct -- the reset fires on any stale window, even
  /// when the current event is an at-cap one.
  action processInfraWindowUngated(w: str, class: ReportClass): bool = all {
    attempt == Reported({ exec: w, class: class, dual: false }),
    dStatus == DRunning,
    or { class == RcInfraOther, class == RcInfraOom, class == RcInfraPutPath },
    val exempt = class == RcInfraPutPath
    val atCap = class == RcInfraOom
    val execAfter = { ...execs.get(w), runningOurs: false, lastCompletedOurs: true, draining: true }
    val fleetAfter = EXECUTORS.filter(x => and {
      execs.get(x).registered,
      not(execs.get(x).draining),
      x != w,
    })
    val applied = specApply(spec, FInfra({ at: now, exempt: exempt, atCap: atCap }), fleetAfter)
    val cExempt = if (exempt) { ...live, exemptInfraCount: live.exemptInfraCount + 1 } else live
    val exemptCapHit = exempt and cExempt.exemptInfraCount >= MAX_EXEMPT
    // PRE-FIX: no `not(atCap)` gate on the stale-window reset.
    val windowElapsed =
      match cExempt.lastInfraAt { | NoTime => false | At(t) => now - t > INFRA_WINDOW }
    val cReset = if (windowElapsed and not(exemptCapHit)) { ...cExempt, infraCount: 0 } else cExempt
    val infraCapHit = not(exempt) and cReset.infraCount >= MAX_INFRA
    all {
      spec' = applied.c,
      specVerdict' = applied.v,
      execs' = execs.put(w, execAfter),
      attempt' = NoAttempt,
      promotionsLeft' = promotionsLeft,
      if (exemptCapHit) all {
        dStatus' = DPoisoned,
        poisonSrc' = EventPoison,
        live' = { ...cExempt, poisonedAt: At(now) },
        lastPoisonReason' = RExemptInfraBudget,
        pg' = { ...pg, status: PgPoisoned, poisonedAt: At(now) },
        depStatus' = DepFailed,
        everFailureTerminal' = true,
        last' = { ...noObs, kind: OE2, chargedAny: true, terminal: true, atCap: atCap },
      } else if (infraCapHit) all {
        dStatus' = DPoisoned,
        poisonSrc' = EventPoison,
        live' = { ...cReset, poisonedAt: At(now) },
        lastPoisonReason' = RInfraBudget,
        pg' = { ...pg, status: PgPoisoned, poisonedAt: At(now) },
        depStatus' = DepFailed,
        everFailureTerminal' = true,
        last' = { ...noObs, kind: OE2, terminal: true, windowReset: windowElapsed, atCap: atCap },
      } else all {
        dStatus' = DReady,
        poisonSrc' = poisonSrc,
        live' = if (exempt) cReset
          else { ...cReset, infraCount: cReset.infraCount + 1, lastInfraAt: At(now) },
        lastPoisonReason' = lastPoisonReason,
        pg' = pg,
        depStatus' = depStatus,
        everFailureTerminal' = everFailureTerminal,
        last' = { ...noObs, kind: OE2, chargedAny: true, windowReset: windowElapsed, atCap: atCap },
      },
    },
    pgFaultsLeft' = pgFaultsLeft,
    everFailedBuilders' = everFailedBuilders,
    now' = now, physAttempts' = physAttempts, failoversLeft' = failoversLeft,
    justFailedOver' = false,
  }

  action buildEndsInfraAny = {
    nondet w = EXECUTORS.oneOf()
    nondet class = Set(RcInfraOther, RcInfraOom, RcInfraPutPath).oneOf()
    buildEndsReported(w, class, false)
  }
  action processInfraWindowUngatedAny = {
    nondet w = EXECUTORS.oneOf()
    nondet class = Set(RcInfraOther, RcInfraOom, RcInfraPutPath).oneOf()
    processInfraWindowUngated(w, class)
  }

  action calibStep = any {
    tick,
    dispatchToAny,
    buildEndsInfraAny,
    processInfraWindowUngatedAny,
    respawnExecutorAny,
  }
}

// =========================================================================
// CALIBRATION -- pre-8283d4362 (half b): the controller-reported at-cap
// OOM/DiskPressure path has no max_infra_retries check at all -- it keeps
// charging and never poisons, so the build loops at the ceiling forever.
// PREDICTED: boundsOK falsifies (live.infraCount exceeds MAX_INFRA) in a
// pod-death/controller alphabet where the as-built encoding holds it.
// (countersRefineHistory is NOT the prediction here: it already falsifies
// as-built on this alphabet via the documented D2 divergence.)
// =========================================================================
module retryCalibG1ControllerOomUncapped {
  import retryPolicyAsBuilt(
    EXECUTORS = Set("w1"),
    MAX_TIME = 2,
    MAX_ATTEMPTS = 3,
    ATTEMPT_BOUND = 3,
    MAX_FAILOVERS = 0,
    PG_FAULTS = 0,
    ENABLE_WORKER = false,
    ENABLE_PODDEATH = true,
    ENABLE_CRASH = false,
    ENABLE_WEDGE = false,
    ENABLE_RESETS = false
  ).* from "../retryPolicyAsBuilt"

  /// PRE-FIX E6 (the recently_disconnected-correlated arm): identical to
  /// the as-built ctrlDeliverOomLate except there is NO cap check -- the
  /// at-cap report always charges and never poisons.
  action ctrlDeliverOomLateUncapped(w: str, atCap: bool): bool = all {
    execs.get(w).ctrl == CtrlOom,
    execs.get(w).rd != NoTime,
    execs.get(w).rdForCtrl,
    atCap,
    val e = execs.get(w)
    val statusOk = dStatus == DReady or dStatus == DRunning
    val charges = statusOk and atCap
    val applied = specApply(spec,
      FControllerTermination({ at: now, promoted: false, atCap: atCap }), eligibleFleet)
    all {
      execs' = execs.put(w, { ...e, rd: NoTime, ctrl: NoCtrl,
        deathCharges: if (charges) e.deathCharges + 1 else e.deathCharges }),
      spec' = if (statusOk) applied.c else spec,
      specVerdict' = if (statusOk) applied.v else specVerdict,
      promotionsLeft' = promotionsLeft,
      if (charges) all {
        // PRE-FIX: charge without ever checking max_infra_retries.
        dStatus' = dStatus,
        poisonSrc' = poisonSrc,
        live' = { ...live, infraCount: live.infraCount + 1 },
        lastPoisonReason' = lastPoisonReason,
        pg' = pg,
        depStatus' = depStatus,
        everFailureTerminal' = everFailureTerminal,
        last' = { ...noObs, kind: OE6, chargedAny: true, atCap: atCap, lateInstallment: true },
      } else all {
        dStatus' = dStatus,
        poisonSrc' = poisonSrc,
        live' = live,
        lastPoisonReason' = lastPoisonReason,
        pg' = pg,
        depStatus' = depStatus,
        everFailureTerminal' = everFailureTerminal,
        last' = { ...noObs, kind: OE6, atCap: atCap, lateInstallment: true },
      },
    },
    attempt' = attempt,
    now' = now, physAttempts' = physAttempts, failoversLeft' = failoversLeft,
    pgFaultsLeft' = pgFaultsLeft, justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  action buildEndsOomAny = { nondet w = EXECUTORS.oneOf() buildEndsPodDeath(w, DkOom) }
  action ctrlDeliverOomLateUncappedAny = {
    nondet w = EXECUTORS.oneOf()
    ctrlDeliverOomLateUncapped(w, true)
  }

  action calibStep = any {
    dispatchToAny,
    buildEndsOomAny,
    processDisconnectAny,
    ctrlDeliverOomLateUncappedAny,
    respawnExecutorAny,
  }
}

// =========================================================================
// CALIBRATION -- pre-172776b1b: the controller-reported DeadlineExceeded
// path has no cap action -- at max_timeout_retries it keeps charging and
// the derivation loops at the cap forever (the fix introduced the
// poison-at-cap that is today's documented D1 divergence).
// PREDICTED: boundsOK falsifies (live.timeoutCount exceeds MAX_TIMEOUT) in
// a deadline-death alphabet where the as-built encoding holds it.
// =========================================================================
module retryCalibG1DeadlineUncapped {
  import retryPolicyAsBuilt(
    EXECUTORS = Set("w1"),
    MAX_TIME = 2,
    MAX_ATTEMPTS = 4,
    ATTEMPT_BOUND = 4,
    MAX_FAILOVERS = 0,
    PG_FAULTS = 0,
    ENABLE_WORKER = false,
    ENABLE_PODDEATH = true,
    ENABLE_CRASH = false,
    ENABLE_WEDGE = false,
    ENABLE_RESETS = false
  ).* from "../retryPolicyAsBuilt"

  /// PRE-FIX E7: identical to the as-built ctrlDeliverDeadline except the
  /// cap arm only logs (here: does nothing) -- the charge is unconditional
  /// and nothing ever goes terminal on this path.
  action ctrlDeliverDeadlineUncapped(w: str): bool = all {
    execs.get(w).ctrl == CtrlDeadline,
    execs.get(w).rd != NoTime,
    execs.get(w).rdForCtrl,
    val e = execs.get(w)
    val statusOk = dStatus == DReady or dStatus == DRunning
    val applied = specApply(spec, FControllerDeadlineExceeded(now), eligibleFleet)
    all {
      execs' = execs.put(w, { ...e, rd: NoTime, ctrl: NoCtrl,
        deathCharges: if (statusOk) e.deathCharges + 1 else e.deathCharges }),
      spec' = if (statusOk) applied.c else spec,
      specVerdict' = if (statusOk) applied.v else specVerdict,
      if (statusOk) all {
        // PRE-FIX: charge regardless of the cap; never poison, never cancel.
        dStatus' = dStatus,
        poisonSrc' = poisonSrc,
        live' = { ...live, timeoutCount: live.timeoutCount + 1 },
        lastPoisonReason' = lastPoisonReason,
        pg' = pg,
        depStatus' = depStatus,
        everFailureTerminal' = everFailureTerminal,
        last' = { ...noObs, kind: OE7, chargedAny: true, lateInstallment: true },
      } else all {
        dStatus' = dStatus,
        poisonSrc' = poisonSrc,
        live' = live,
        lastPoisonReason' = lastPoisonReason,
        pg' = pg,
        depStatus' = depStatus,
        everFailureTerminal' = everFailureTerminal,
        last' = { ...noObs, kind: OE7, lateInstallment: true },
      },
    },
    attempt' = attempt,
    now' = now, promotionsLeft' = promotionsLeft, physAttempts' = physAttempts,
    failoversLeft' = failoversLeft, pgFaultsLeft' = pgFaultsLeft,
    justFailedOver' = false, everFailedBuilders' = everFailedBuilders,
  }

  action buildEndsDeadlineAny = { nondet w = EXECUTORS.oneOf() buildEndsPodDeath(w, DkDeadline) }
  action ctrlDeliverDeadlineUncappedAny = { nondet w = EXECUTORS.oneOf() ctrlDeliverDeadlineUncapped(w) }

  action calibStep = any {
    dispatchToAny,
    buildEndsDeadlineAny,
    processDisconnectAny,
    ctrlDeliverDeadlineUncappedAny,
    respawnExecutorAny,
  }
}

// =========================================================================
// CALIBRATION -- pre-9c20d04e3: handle_timeout_failure's charge was gated
// on the floor having been promoted, so the I-200 cold-start (no SLA
// estimate, nothing to promote) never incremented timeout_count and the
// worker-reported timeout retried forever. The override models the
// harmful sub-case: the charge simply never happens.
// PREDICTED: countersRefineHistory falsifies at the first worker-reported
// timeout (the fold charges unconditionally under the cap), worker-channel
// alphabet. Also covers, by analogy, the deadline-side half of
// db457374f-C3 (the same charge-gated-on-floor-outcome shape on E7).
// =========================================================================
module retryCalibG1TimeoutChargeSkipped {
  import retryPolicyAsBuilt(
    EXECUTORS = Set("w1"),
    MAX_TIME = 2,
    MAX_ATTEMPTS = 3,
    ATTEMPT_BOUND = 3,
    MAX_FAILOVERS = 0,
    PG_FAULTS = 0,
    ENABLE_WORKER = true,
    ENABLE_PODDEATH = false,
    ENABLE_CRASH = false,
    ENABLE_WEDGE = false,
    ENABLE_RESETS = false
  ).* from "../retryPolicyAsBuilt"

  /// PRE-FIX E4: the report is consumed and the derivation requeued, but
  /// the cold-start (never-promoted) gate skips the timeout_count charge.
  action processTimeoutSkipped(w: str): bool = all {
    attempt == Reported({ exec: w, class: RcTimeout, dual: false }),
    dStatus == DRunning,
    val applied = specApply(spec, FWorkerTimeout(now), eligibleFleet)
    all {
      spec' = applied.c,
      specVerdict' = applied.v,
      attempt' = NoAttempt,
      dStatus' = DReady,
      poisonSrc' = poisonSrc,
      live' = live,
      lastPoisonReason' = lastPoisonReason,
      pg' = pg,
      depStatus' = depStatus,
      everFailureTerminal' = everFailureTerminal,
      execs' = execs.put(w, { ...execs.get(w), runningOurs: false, lastCompletedOurs: true, draining: true }),
      last' = { ...noObs, kind: OE4 },
    },
    promotionsLeft' = promotionsLeft,
    pgFaultsLeft' = pgFaultsLeft,
    everFailedBuilders' = everFailedBuilders,
    now' = now, physAttempts' = physAttempts, failoversLeft' = failoversLeft,
    justFailedOver' = false,
  }

  action buildEndsTimeoutAny = { nondet w = EXECUTORS.oneOf() buildEndsReported(w, RcTimeout, false) }
  action processTimeoutSkippedAny = { nondet w = EXECUTORS.oneOf() processTimeoutSkipped(w) }

  action calibStep = any {
    dispatchToAny,
    buildEndsTimeoutAny,
    processTimeoutSkippedAny,
    respawnExecutorAny,
  }
}

// =========================================================================
// CALIBRATION -- pre-db457374f (the backstop half of its C3): the
// scheduler-side backstop for a wedged attempt recorded nothing into
// failed_builders/failure_count and did not quarantine the slot, so the
// no-report wedge loop was bounded by nothing at all.
// PREDICTED: countersRefineHistory falsifies at the first backstop firing
// (the fold charges failed_builders/failure_count), and
// attemptsBoundedGlobal falsifies once the uncounted wedge loop exceeds
// the threshold-justified bound -- in a wedge-only alphabet where the
// as-built encoding holds both.
// =========================================================================
module retryCalibG1BackstopUncounted {
  import retryPolicyAsBuilt(
    EXECUTORS = Set("w1", "w2"),
    MAX_TIME = 2,
    MAX_ATTEMPTS = 4,
    // What the distinct-worker threshold justifies for a wedge-only
    // alphabet under the as-built accounting: one attempt per slot, then
    // the threshold poisons.
    ATTEMPT_BOUND = 2,
    MAX_FAILOVERS = 0,
    PG_FAULTS = 0,
    ENABLE_WORKER = false,
    ENABLE_PODDEATH = false,
    ENABLE_CRASH = false,
    ENABLE_WEDGE = true,
    ENABLE_RESETS = false
  ).* from "../retryPolicyAsBuilt"

  /// PRE-FIX E8: the backstop frees the slot and requeues the derivation
  /// but records nothing and quarantines nothing.
  action backstopFiresUncounted(w: str): bool = all {
    ENABLE_WEDGE,
    attempt == Wedged(w),
    dStatus == DRunning,
    val applied = specApply(spec, FBackstopTimeout({ at: now, exec: w }), eligibleFleet)
    all {
      execs' = execs.put(w, { ...execs.get(w), runningOurs: false }),
      attempt' = NoAttempt,
      spec' = applied.c,
      specVerdict' = applied.v,
      dStatus' = DReady,
      poisonSrc' = poisonSrc,
      live' = live,
      lastPoisonReason' = lastPoisonReason,
      pg' = pg,
      depStatus' = depStatus,
      everFailureTerminal' = everFailureTerminal,
      last' = { ...noObs, kind: OE8 },
    },
    now' = now, promotionsLeft' = promotionsLeft, physAttempts' = physAttempts,
    failoversLeft' = failoversLeft, pgFaultsLeft' = pgFaultsLeft,
    justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  action backstopFiresUncountedAny = { nondet w = EXECUTORS.oneOf() backstopFiresUncounted(w) }

  action calibStep = any {
    dispatchToAny,
    buildWedgesAny,
    backstopFiresUncountedAny,
    respawnExecutorAny,
  }

  /// The as-built restriction of the same alphabet (used to confirm the
  /// ATTEMPT_BOUND baseline really is what the as-built accounting
  /// enforces before the override is blamed for breaking it).
  action baselineStep = any {
    dispatchToAny,
    buildWedgesAny,
    backstopFiresAny,
    respawnExecutorAny,
  }
}

// =========================================================================
// CALIBRATION -- pre-8a016a393: an at-cap cgroup-OOM was double-counted
// into infra_count (the floor helper's at-cap arm incremented it and the
// infrastructure-failure handler incremented it again).
// PREDICTED: countersRefineHistory falsifies at the first at-cap OOM
// report (live jumps by 2 where the fold charges 1), worker-channel
// alphabet.
// =========================================================================
module retryCalibG1AtCapOomDoubleCount {
  import retryPolicyAsBuilt(
    EXECUTORS = Set("w1"),
    MAX_TIME = 2,
    MAX_ATTEMPTS = 3,
    ATTEMPT_BOUND = 3,
    MAX_FAILOVERS = 0,
    PG_FAULTS = 0,
    ENABLE_WORKER = true,
    ENABLE_PODDEATH = false,
    ENABLE_CRASH = false,
    ENABLE_WEDGE = false,
    ENABLE_RESETS = false
  ).* from "../retryPolicyAsBuilt"

  /// PRE-FIX E2, at-cap OOM only: the charge is applied twice (the
  /// pre-FloorOutcome world where both the bump helper and the handler
  /// incremented), the cap check runs against the pre-event count.
  action processAtCapOomDoubleCounted(w: str): bool = all {
    attempt == Reported({ exec: w, class: RcInfraOom, dual: false }),
    dStatus == DRunning,
    val execAfter = { ...execs.get(w), runningOurs: false, lastCompletedOurs: true, draining: true }
    val fleetAfter = EXECUTORS.filter(x => and {
      execs.get(x).registered,
      not(execs.get(x).draining),
      x != w,
    })
    val applied = specApply(spec, FInfra({ at: now, exempt: false, atCap: true }), fleetAfter)
    val infraCapHit = live.infraCount >= MAX_INFRA
    all {
      spec' = applied.c,
      specVerdict' = applied.v,
      execs' = execs.put(w, execAfter),
      attempt' = NoAttempt,
      promotionsLeft' = promotionsLeft,
      if (infraCapHit) all {
        dStatus' = DPoisoned,
        poisonSrc' = EventPoison,
        live' = { ...live, poisonedAt: At(now) },
        lastPoisonReason' = RInfraBudget,
        pg' = { ...pg, status: PgPoisoned, poisonedAt: At(now) },
        depStatus' = DepFailed,
        everFailureTerminal' = true,
        last' = { ...noObs, kind: OE2, terminal: true, atCap: true },
      } else all {
        // PRE-FIX: the same physical at-cap OOM lands twice in infra_count.
        dStatus' = DReady,
        poisonSrc' = poisonSrc,
        live' = { ...live, infraCount: live.infraCount + 2, lastInfraAt: At(now) },
        lastPoisonReason' = lastPoisonReason,
        pg' = pg,
        depStatus' = depStatus,
        everFailureTerminal' = everFailureTerminal,
        last' = { ...noObs, kind: OE2, chargedAny: true, doubleCharge: true, atCap: true },
      },
    },
    pgFaultsLeft' = pgFaultsLeft,
    everFailedBuilders' = everFailedBuilders,
    now' = now, physAttempts' = physAttempts, failoversLeft' = failoversLeft,
    justFailedOver' = false,
  }

  action buildEndsOomReportAny = { nondet w = EXECUTORS.oneOf() buildEndsReported(w, RcInfraOom, false) }
  action processAtCapOomDoubleCountedAny = { nondet w = EXECUTORS.oneOf() processAtCapOomDoubleCounted(w) }

  action calibStep = any {
    dispatchToAny,
    buildEndsOomReportAny,
    processAtCapOomDoubleCountedAny,
    respawnExecutorAny,
  }
}

// =========================================================================
// CALIBRATION -- pre-a60d58a32: no CONCURRENT_PUTPATH exemption and no
// 300 s window -- a store-side PutPath contention failure charges the
// non-exempt infra budget like any other infrastructure failure, and a
// long-running mostly-healthy build accumulates toward the cap with no
// sliding-window forgiveness (the I-127 99.7%-complete poison).
// PREDICTED: countersRefineHistory falsifies at the first PutPath report
// (the fold charges the exemption budget, the pre-fix code charges
// infra_count); the premature RInfraBudget poison at the cap is the
// downstream consequence (verdictMatchesFold, secondary).
// =========================================================================
module retryCalibG1PutPathNotExempt {
  import retryPolicyAsBuilt(
    EXECUTORS = Set("w1"),
    MAX_TIME = 2,
    MAX_ATTEMPTS = 4,
    ATTEMPT_BOUND = 4,
    MAX_FAILOVERS = 0,
    PG_FAULTS = 0,
    ENABLE_WORKER = true,
    ENABLE_PODDEATH = false,
    ENABLE_CRASH = false,
    ENABLE_WEDGE = false,
    ENABLE_RESETS = false
  ).* from "../retryPolicyAsBuilt"

  /// PRE-FIX E2, PutPath class only: treated as a plain (non-exempt)
  /// infrastructure failure, with no window reset (the window did not
  /// exist yet).
  action processPutPathCharged(w: str): bool = all {
    attempt == Reported({ exec: w, class: RcInfraPutPath, dual: false }),
    dStatus == DRunning,
    val execAfter = { ...execs.get(w), runningOurs: false, lastCompletedOurs: true, draining: true }
    val fleetAfter = EXECUTORS.filter(x => and {
      execs.get(x).registered,
      not(execs.get(x).draining),
      x != w,
    })
    val applied = specApply(spec, FInfra({ at: now, exempt: true, atCap: false }), fleetAfter)
    val infraCapHit = live.infraCount >= MAX_INFRA
    all {
      spec' = applied.c,
      specVerdict' = applied.v,
      execs' = execs.put(w, execAfter),
      attempt' = NoAttempt,
      promotionsLeft' = promotionsLeft,
      if (infraCapHit) all {
        dStatus' = DPoisoned,
        poisonSrc' = EventPoison,
        live' = { ...live, poisonedAt: At(now) },
        lastPoisonReason' = RInfraBudget,
        pg' = { ...pg, status: PgPoisoned, poisonedAt: At(now) },
        depStatus' = DepFailed,
        everFailureTerminal' = true,
        last' = { ...noObs, kind: OE2, terminal: true },
      } else all {
        // PRE-FIX: the exempt-class event charges the non-exempt budget.
        dStatus' = DReady,
        poisonSrc' = poisonSrc,
        live' = { ...live, infraCount: live.infraCount + 1, lastInfraAt: At(now) },
        lastPoisonReason' = lastPoisonReason,
        pg' = pg,
        depStatus' = depStatus,
        everFailureTerminal' = everFailureTerminal,
        last' = { ...noObs, kind: OE2, chargedAny: true },
      },
    },
    pgFaultsLeft' = pgFaultsLeft,
    everFailedBuilders' = everFailedBuilders,
    now' = now, physAttempts' = physAttempts, failoversLeft' = failoversLeft,
    justFailedOver' = false,
  }

  action buildEndsPutPathAny = { nondet w = EXECUTORS.oneOf() buildEndsReported(w, RcInfraPutPath, false) }
  action processPutPathChargedAny = { nondet w = EXECUTORS.oneOf() processPutPathCharged(w) }

  action calibStep = any {
    dispatchToAny,
    buildEndsPutPathAny,
    processPutPathChargedAny,
    respawnExecutorAny,
  }
}

// =========================================================================
// CALIBRATION -- pre-699ad52e1 (the exempt-infra-cap root cause): the
// exempt path had no budget of its own -- an exempt failure charged
// nothing and could recur forever (the leaked-store-lock livelock the
// exempt_infra_count cap was added to bound).
// PREDICTED: countersRefineHistory falsifies at the first exempt report
// (the fold charges exempt_infra_count), worker-channel alphabet.
// =========================================================================
module retryCalibG1ExemptPathUncapped {
  import retryPolicyAsBuilt(
    EXECUTORS = Set("w1"),
    MAX_TIME = 2,
    MAX_ATTEMPTS = 4,
    ATTEMPT_BOUND = 4,
    MAX_FAILOVERS = 0,
    PG_FAULTS = 0,
    ENABLE_WORKER = true,
    ENABLE_PODDEATH = false,
    ENABLE_CRASH = false,
    ENABLE_WEDGE = false,
    ENABLE_RESETS = false
  ).* from "../retryPolicyAsBuilt"

  /// PRE-FIX E2, exempt classes only: exempt from the infra cap AND from
  /// any budget at all -- no exempt_infra_count, no exempt cap.
  action processExemptUncharged(w: str): bool = all {
    attempt == Reported({ exec: w, class: RcInfraPutPath, dual: false }),
    dStatus == DRunning,
    val execAfter = { ...execs.get(w), runningOurs: false, lastCompletedOurs: true, draining: true }
    val fleetAfter = EXECUTORS.filter(x => and {
      execs.get(x).registered,
      not(execs.get(x).draining),
      x != w,
    })
    val applied = specApply(spec, FInfra({ at: now, exempt: true, atCap: false }), fleetAfter)
    all {
      spec' = applied.c,
      specVerdict' = applied.v,
      execs' = execs.put(w, execAfter),
      attempt' = NoAttempt,
      promotionsLeft' = promotionsLeft,
      // PRE-FIX: nothing is charged, nothing can ever cap out.
      dStatus' = DReady,
      poisonSrc' = poisonSrc,
      live' = live,
      lastPoisonReason' = lastPoisonReason,
      pg' = pg,
      depStatus' = depStatus,
      everFailureTerminal' = everFailureTerminal,
      last' = { ...noObs, kind: OE2 },
    },
    pgFaultsLeft' = pgFaultsLeft,
    everFailedBuilders' = everFailedBuilders,
    now' = now, physAttempts' = physAttempts, failoversLeft' = failoversLeft,
    justFailedOver' = false,
  }

  action buildEndsPutPathAny = { nondet w = EXECUTORS.oneOf() buildEndsReported(w, RcInfraPutPath, false) }
  action processExemptUnchargedAny = { nondet w = EXECUTORS.oneOf() processExemptUncharged(w) }

  action calibStep = any {
    dispatchToAny,
    buildEndsPutPathAny,
    processExemptUnchargedAny,
    respawnExecutorAny,
  }
}

// =========================================================================
// CALIBRATION -- the disconnect/eviction half of pre-I-213 (8d38cb999 /
// dc094dd0c): the disconnect path recorded the lost worker into
// failed_builders / failure_count unconditionally -- a kubelet-eviction
// (floor-promotion) ladder or an assigned-only disconnect was charged as
// poison-threshold progress, so a resizing build was poisoned by its own
// promotions. (The E1 half of the same incident -- c13f6a277,
// floor-promoted transient failures consuming max_retries -- is NOT
// covered by this override: the floor outcome exists only on OOM-class
// events in the model and the fold, so that commit is dispositioned
// NOT-ENC in the calibration table and its coverage stays with the
// promotion-exempt unit tests.)
// PREDICTED: countersRefineHistory falsifies at the first charging
// disconnect (today's E5 -- and the fold -- charge nothing), and the
// distinct-worker threshold poison the fold does not sanction follows
// (verdictMatchesFold, secondary). Pod-death alphabet with the
// controller channel silenced (so the as-built baseline holds both
// invariants on this alphabet -- confirmed with baselineStep).
// =========================================================================
module retryCalibG1DisconnectCharges {
  import retryPolicyAsBuilt(
    EXECUTORS = Set("w1", "w2"),
    MAX_TIME = 2,
    MAX_ATTEMPTS = 4,
    ATTEMPT_BOUND = 4,
    MAX_FAILOVERS = 0,
    PG_FAULTS = 0,
    ENABLE_WORKER = false,
    ENABLE_PODDEATH = true,
    ENABLE_CRASH = true,
    ENABLE_WEDGE = false,
    ENABLE_RESETS = false
  ).* from "../retryPolicyAsBuilt"

  /// PRE-FIX E5: the mid-build disconnect records the lost worker
  /// (failed_builders + failure_count + the PG mirror append) and then
  /// re-checks the threshold, exactly like a worker-reported transient
  /// failure would -- the accounting today's reassign_derivations no
  /// longer performs.
  action processDisconnectCharging(w: str): bool = all {
    match attempt { | PodDead(p) => p.exec == w | _ => false },
    dStatus == DRunning,
    val e = execs.get(w)
    val insertRd = not(e.lastCompletedOurs)
    val fb1 = live.failedBuilders.union(Set(w))
    val c1 = { ...live, failedBuilders: fb1, failureCount: live.failureCount + 1 }
    val poisonNow = c1.failedBuilders.size() >= THRESHOLD
    val applied = specApply(spec, FDisconnect(now), eligibleFleet)
    all {
      execs' = execs.put(w, { ...e,
        registered: false, draining: false, runningOurs: false,
        rd: if (insertRd) At(now) else e.rd,
        rdForCtrl: if (insertRd) true else e.rdForCtrl }),
      attempt' = NoAttempt,
      spec' = applied.c,
      specVerdict' = applied.v,
      if (poisonNow) all {
        dStatus' = DPoisoned,
        poisonSrc' = EventPoison,
        live' = { ...c1, poisonedAt: At(now) },
        lastPoisonReason' = RThreshold,
        pg' = { ...pg, status: PgPoisoned, poisonedAt: At(now),
          failedBuilders: pg.failedBuilders.union(Set(w)) },
        depStatus' = DepFailed,
        everFailureTerminal' = true,
        last' = { ...noObs, kind: OE5, chargedAny: true, terminal: true },
      } else all {
        dStatus' = DReady,
        poisonSrc' = poisonSrc,
        live' = c1,
        lastPoisonReason' = lastPoisonReason,
        pg' = { ...pg, failedBuilders: pg.failedBuilders.union(Set(w)) },
        depStatus' = depStatus,
        everFailureTerminal' = everFailureTerminal,
        last' = { ...noObs, kind: OE5, chargedAny: true },
      },
    },
    now' = now, promotionsLeft' = promotionsLeft, physAttempts' = physAttempts,
    failoversLeft' = failoversLeft, pgFaultsLeft' = pgFaultsLeft,
    justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders.union(Set(w)),
  }

  action processDisconnectChargingAny = { nondet w = EXECUTORS.oneOf() processDisconnectCharging(w) }

  /// The pre-fix step: pod deaths observed only through the disconnect
  /// (the controller channel never delivers, so the as-built D2/D3
  /// divergences cannot muddy the comparison).
  action calibStep = any {
    dispatchToAny,
    buildEndsPodDeathAny,
    processDisconnectChargingAny,
    respawnExecutorAny,
  }

  /// The as-built restriction of the same alphabet (the baseline that
  /// holds countersRefineHistory and verdictMatchesFold).
  action baselineStep = any {
    dispatchToAny,
    buildEndsPodDeathAny,
    processDisconnectAny,
    respawnExecutorAny,
  }
}
