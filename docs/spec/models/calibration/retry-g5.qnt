// -*- mode: Bluespec; -*-
//
// Stage-C calibration overrides, family G5: "the same dead executor was
// counted twice -- signal-channel dedup" (retry-invariant-map.md,
// calibration table; inventory section 5). Same extend-and-override
// pattern as retry-g1.qnt.

// =========================================================================
// CALIBRATION -- pre-ee9302b86: the race-ahead controller termination
// report (the report that lands before the dead pod's stream close)
// resolves the derivation through the still-live executor entry but does
// NOT mark the completion -- so the subsequent disconnect inserts a
// recently_disconnected entry for the same death, and the controller's
// re-report finds it and charges again: one pod death, two accounting
// events (double floor bump / infra_count += 2 at the ceiling).
// PREDICTED: noDoubleCount falsifies in a pod-death/controller alphabet
// where the as-built dedup holds it (the Stage-B dual-regime headline).
// =========================================================================
module retryCalibG5RaceAheadKeepsPending {
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

  /// PRE-FIX E6 race-ahead arm: the charge lands, but last_completed is
  /// not set (the disconnect that follows will insert a
  /// recently_disconnected entry for the same death) and the controller's
  /// re-report stream is still pending (the as-built model consumes the
  /// single effective delivery here only because the dedup makes any
  /// further delivery a no-op -- pre-fix it is not).
  action ctrlOomRaceAheadKeepsPending(w: str, atCap: bool): bool = all {
    match attempt { | PodDead(p) => p.exec == w and p.kind == DkOom | _ => false },
    execs.get(w).ctrl == CtrlOom,
    not(execs.get(w).ctrlStale),
    execs.get(w).rd == NoTime,
    atCap,
    val e = execs.get(w)
    val statusOk = dStatus == DReady or dStatus == DRunning
    val charges = statusOk and atCap
    val capHit = charges and live.infraCount >= MAX_INFRA
    val applied = specApply(spec,
      FControllerTermination({ at: now, promoted: false, atCap: atCap }), eligibleFleet)
    all {
      // PRE-FIX: neither last_completed nor the pending report is touched.
      execs' = execs.put(w, { ...e,
        deathCharges: if (charges) e.deathCharges + 1 else e.deathCharges }),
      spec' = if (statusOk) applied.c else spec,
      specVerdict' = if (statusOk) applied.v else specVerdict,
      promotionsLeft' = promotionsLeft,
      if (capHit) all {
        dStatus' = DPoisoned,
        poisonSrc' = EventPoison,
        live' = { ...live, poisonedAt: At(now) },
        lastPoisonReason' = RInfraBudget,
        pg' = { ...pg, status: PgPoisoned, poisonedAt: At(now) },
        depStatus' = DepFailed,
        everFailureTerminal' = true,
        last' = { ...noObs, kind: OE6, terminal: true, atCap: atCap, raceAhead: true },
      } else if (charges) all {
        dStatus' = dStatus,
        poisonSrc' = poisonSrc,
        live' = { ...live, infraCount: live.infraCount + 1 },
        lastPoisonReason' = lastPoisonReason,
        pg' = pg,
        depStatus' = depStatus,
        everFailureTerminal' = everFailureTerminal,
        last' = { ...noObs, kind: OE6, chargedAny: true, atCap: atCap, raceAhead: true },
      } else all {
        dStatus' = dStatus,
        poisonSrc' = poisonSrc,
        live' = live,
        lastPoisonReason' = lastPoisonReason,
        pg' = pg,
        depStatus' = depStatus,
        everFailureTerminal' = everFailureTerminal,
        last' = { ...noObs, kind: OE6, atCap: atCap, raceAhead: true },
      },
    },
    attempt' = attempt,
    now' = now, physAttempts' = physAttempts, failoversLeft' = failoversLeft,
    pgFaultsLeft' = pgFaultsLeft, justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  action buildEndsOomAny = { nondet w = EXECUTORS.oneOf() buildEndsPodDeath(w, DkOom) }
  action ctrlOomRaceAheadKeepsPendingAny = {
    nondet w = EXECUTORS.oneOf()
    ctrlOomRaceAheadKeepsPending(w, true)
  }
  action ctrlDeliverOomLateAtCapAny = {
    nondet w = EXECUTORS.oneOf()
    ctrlDeliverOomLate(w, false, true)
  }

  /// The pre-fix step: the race-ahead arm is the override; the
  /// post-disconnect (recently_disconnected-correlated) arm is the
  /// as-built one -- exactly the second delivery the missing
  /// last_completed mark lets through.
  action calibStep = any {
    dispatchToAny,
    buildEndsOomAny,
    ctrlOomRaceAheadKeepsPendingAny,
    processDisconnectAny,
    ctrlDeliverOomLateAtCapAny,
    respawnExecutorAny,
  }

  /// The documented incident shape, pinned step by step: race-ahead
  /// report charges, the disconnect (no last_completed mark) inserts the
  /// dedup entry, the re-report consumes it and charges the same death a
  /// second time.
  run g5RaceAheadDoubleCountRun =
    init
      .then(dispatchTo("w1"))
      .then(buildEndsPodDeath("w1", DkOom))
      .then(ctrlOomRaceAheadKeepsPending("w1", true))
      .expect(all { live.infraCount == 1, execs.get("w1").deathCharges == 1, noDoubleCount })
      .then(processDisconnect("w1"))
      .expect(all { execs.get("w1").rd != NoTime, noDoubleCount })
      .then(ctrlDeliverOomLate("w1", false, true))
      .expect(all {
        live.infraCount == 2,
        execs.get("w1").deathCharges == 2,
        not(noDoubleCount),
      })
}

// =========================================================================
// CALIBRATION -- pre-e872b2b49: a non-promoting controller termination
// report (Error / Completed / EvictedOther) consumed the
// recently_disconnected entry before the promotion-reason gate rejected
// it, so the same-tick DeadlineExceeded report's prefix-match found
// nothing and the controller-side timeout backstop was structurally
// defeated for that death.
// PREDICTED: the local correlation-conservation invariant
// (pendingReportKeepsItsEntry) falsifies -- a pending, non-stale
// controller report is left with no recently_disconnected entry to be
// correlated through. The as-built encoding holds it on this alphabet
// (baselineStep), because as built nothing but delivery or the TTL sweep
// consumes the entry. The downstream consequence -- the deadline death
// goes uncounted and the wedge ladder loses its bound -- is a
// fairness-dependent liveness harm the safety invariants cannot state.
// =========================================================================
module retryCalibG5NonPromotingConsumesEntry {
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

  /// PRE-FIX: the non-promoting report consumes the dedup entry and then
  /// no-ops (the as-built code returns before touching it, which is why
  /// the action is absent from the as-built alphabet).
  action ctrlNonPromotingConsumesEntry(w: str): bool = all {
    execs.get(w).rd != NoTime,
    execs' = execs.put(w, { ...execs.get(w), rd: NoTime, rdForCtrl: false }),
    now' = now, dStatus' = dStatus, live' = live, poisonSrc' = poisonSrc,
    spec' = spec, specVerdict' = specVerdict, pg' = pg,
    depStatus' = depStatus, everFailureTerminal' = everFailureTerminal,
    attempt' = attempt, promotionsLeft' = promotionsLeft,
    physAttempts' = physAttempts, failoversLeft' = failoversLeft,
    pgFaultsLeft' = pgFaultsLeft, last' = last,
    lastPoisonReason' = lastPoisonReason, justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  action ctrlNonPromotingConsumesEntryAny = { nondet w = EXECUTORS.oneOf() ctrlNonPromotingConsumesEntry(w) }
  action buildEndsDeadlineAny = { nondet w = EXECUTORS.oneOf() buildEndsPodDeath(w, DkDeadline) }

  /// Local calibration invariant: a pending, non-stale controller report
  /// whose death has already been fully disconnected (no attempt in
  /// flight) still has the recently_disconnected entry it will be
  /// correlated through. This is the structural statement of "the
  /// non-promoting report preserves recently_disconnected"; in this
  /// no-tick alphabet nothing as-built can consume the entry before the
  /// report does.
  val pendingReportKeepsItsEntry = EXECUTORS.forall(w =>
    and {
      execs.get(w).ctrl != NoCtrl,
      not(execs.get(w).ctrlStale),
      attempt == NoAttempt,
    } implies execs.get(w).rd != NoTime)

  /// The pre-fix step.
  action calibStep = any {
    dispatchToAny,
    buildEndsDeadlineAny,
    processDisconnectAny,
    ctrlDeliverDeadlineAny,
    ctrlNonPromotingConsumesEntryAny,
    respawnExecutorAny,
  }

  /// The as-built restriction of the same alphabet (the baseline that
  /// holds pendingReportKeepsItsEntry).
  action baselineStep = any {
    dispatchToAny,
    buildEndsDeadlineAny,
    processDisconnectAny,
    ctrlDeliverDeadlineAny,
    respawnExecutorAny,
  }
}
