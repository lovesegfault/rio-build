// -*- mode: Bluespec; -*-
//
// Stage-C calibration overrides, family G3: "the cascade didn't fire,
// fired twice, or left a build hanging" (retry-invariant-map.md,
// calibration table; inventory section 5). Same extend-and-override
// pattern as retry-g1.qnt.

// =========================================================================
// CALIBRATION -- pre-af0eb62c6: poisoning a derivation does not cascade
// DependencyFailed onto its dependents at all (the original
// keep_going-build-hangs-forever defect the cascade was introduced for).
// PREDICTED: cascadeReachesExactlyTheDependents falsifies at the first
// poison (the building derivation is failure-terminal, the dependent is
// still DepReady), worker-channel alphabet restricted to the permanent-
// failure class (the simplest poison).
// =========================================================================
module retryCalibG3PoisonWithoutCascade {
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

  /// PRE-FIX E3: the permanent failure poisons the derivation but no
  /// cascade reaches the dependent.
  action processPermanentNoCascade(w: str): bool = all {
    attempt == Reported({ exec: w, class: RcPermanent, dual: false }),
    dStatus == DRunning,
    val execAfter = { ...execs.get(w), runningOurs: false, lastCompletedOurs: true, draining: true }
    val fleetAfter = EXECUTORS.filter(x => and {
      execs.get(x).registered,
      not(execs.get(x).draining),
      x != w,
    })
    val applied = specApply(spec, FPermanent({ at: now, exec: w }), fleetAfter)
    all {
      execs' = execs.put(w, execAfter),
      attempt' = NoAttempt,
      spec' = applied.c,
      specVerdict' = applied.v,
      dStatus' = DPoisoned,
      poisonSrc' = EventPoison,
      live' = { ...live,
        poisonedAt: At(now),
        failedBuilders: live.failedBuilders.union(Set(w)) },
      lastPoisonReason' = RPermanent,
      pg' = { ...pg,
        status: PgPoisoned, poisonedAt: At(now),
        failedBuilders: pg.failedBuilders.union(Set(w)) },
      // PRE-FIX: no cascade.
      depStatus' = depStatus,
      everFailureTerminal' = true,
      last' = { ...noObs, kind: OE3, chargedAny: true, terminal: true },
      promotionsLeft' = promotionsLeft,
    },
    pgFaultsLeft' = pgFaultsLeft,
    everFailedBuilders' = everFailedBuilders.union(Set(w)),
    now' = now, physAttempts' = physAttempts, failoversLeft' = failoversLeft,
    justFailedOver' = false,
  }

  action buildEndsPermanentAny = { nondet w = EXECUTORS.oneOf() buildEndsReported(w, RcPermanent, false) }
  action processPermanentNoCascadeAny = { nondet w = EXECUTORS.oneOf() processPermanentNoCascade(w) }

  action calibStep = any {
    dispatchToAny,
    buildEndsPermanentAny,
    processPermanentNoCascadeAny,
  }
}

// =========================================================================
// CALIBRATION -- pre-3973a4f54 / pre-5b4543c3a (the recovery-cascade
// halves): post-failover recovery rebuilds the poisoned derivation from
// its durable row but does not re-seed the dependent's DependencyFailed,
// so a crash that interleaves with a resubmit whose poison-clear write was
// lost leaves a Poisoned parent with a Ready dependent -- the
// wrongly-promoted-to-Ready recovery shape.
// PREDICTED: cascadeReachesExactlyTheDependents falsifies in the
// post-failover state, fault-persist/failover alphabet (one failover, one
// lost mirror write) where the as-built encoding holds it.
// =========================================================================
module retryCalibG3RecoveryNoRecascade {
  import retryPolicyAsBuilt(
    EXECUTORS = Set("w1"),
    MAX_TIME = 2,
    MAX_ATTEMPTS = 3,
    ATTEMPT_BOUND = 3,
    MAX_FAILOVERS = 1,
    PG_FAULTS = 1,
    ENABLE_WORKER = true,
    ENABLE_PODDEATH = false,
    ENABLE_CRASH = false,
    ENABLE_WEDGE = false,
    ENABLE_RESETS = true
  ).* from "../retryPolicyAsBuilt"

  /// PRE-FIX leader failover: identical to the as-built recovery except
  /// the poisoned-row arm does not re-seed the dependent's
  /// DependencyFailed (sched.recovery.failed-dep-cascade reverted).
  action leaderFailoverNoRecascade = all {
    failoversLeft > 0,
    val gone = dStatus == DAbsent or dStatus == DCompleted
    val expiredInDowntime = and {
      pg.status == PgPoisoned,
      match pg.poisonedAt { | NoTime => false | At(t) => now - t > POISON_TTL },
    }
    val projNT = recoveryProjectionNonTerminal(pg)
    val projP = recoveryProjectionPoisoned(pg)
    val orphanPoisons = pg.status == PgNonTerminal and projNT.failedBuilders.size() >= THRESHOLD
    all {
      failoversLeft' = failoversLeft - 1,
      execs' = EXECUTORS.mapBy(w => freshExec),
      attempt' = NoAttempt,
      justFailedOver' = true,
      last' = { ...noObs, kind: OFailover },
      if (gone) all {
        dStatus' = dStatus, live' = live, spec' = spec, specVerdict' = specVerdict,
        pg' = pg, poisonSrc' = poisonSrc, depStatus' = depStatus,
        everFailureTerminal' = everFailureTerminal, lastPoisonReason' = lastPoisonReason,
      } else if (pg.status == PgPoisoned and expiredInDowntime) all {
        dStatus' = DAbsent, live' = zeroCounters, spec' = zeroCounters,
        specVerdict' = VRequeue, pg' = freshPg, poisonSrc' = NoPoison,
        depStatus' = depStatus, everFailureTerminal' = everFailureTerminal,
        lastPoisonReason' = lastPoisonReason,
      } else if (pg.status == PgPoisoned) all {
        // PRE-FIX: the poisoned row is reloaded, but the dependent is
        // left exactly as the (wiped) in-memory DAG had it -- no
        // recovery-time re-cascade.
        dStatus' = DPoisoned, live' = projP, spec' = projP,
        specVerdict' = VPoison(RThreshold), pg' = pg,
        poisonSrc' = if (poisonSrc == NoPoison) EventPoison else poisonSrc,
        depStatus' = depStatus,
        everFailureTerminal' = true,
        lastPoisonReason' = lastPoisonReason,
      } else if (pg.status == PgNonTerminal and orphanPoisons) all {
        dStatus' = DPoisoned,
        live' = { ...projNT, poisonedAt: At(now) },
        spec' = projNT,
        specVerdict' = VRequeue,
        pg' = { ...pg, status: PgPoisoned, poisonedAt: At(now) },
        poisonSrc' = RecoveryPoison,
        depStatus' = depStatus,
        everFailureTerminal' = true,
        lastPoisonReason' = RThreshold,
      } else if (pg.status == PgNonTerminal) all {
        dStatus' = DReady, live' = projNT, spec' = projNT,
        specVerdict' = VRequeue, pg' = pg, poisonSrc' = NoPoison,
        depStatus' = depStatus, everFailureTerminal' = everFailureTerminal,
        lastPoisonReason' = lastPoisonReason,
      } else all {
        dStatus' = DAbsent, live' = zeroCounters, spec' = zeroCounters,
        specVerdict' = VRequeue, pg' = pg, poisonSrc' = NoPoison,
        depStatus' = depStatus, everFailureTerminal' = everFailureTerminal,
        lastPoisonReason' = lastPoisonReason,
      },
    },
    now' = now, promotionsLeft' = promotionsLeft, physAttempts' = physAttempts,
    pgFaultsLeft' = pgFaultsLeft, everFailedBuilders' = everFailedBuilders,
  }

  action buildEndsPermanentAny = { nondet w = EXECUTORS.oneOf() buildEndsReported(w, RcPermanent, false) }
  action processPermanentAny = {
    nondet w = EXECUTORS.oneOf()
    nondet pgLost = Set(true, false).oneOf()
    processReport(w, RcPermanent, false, false, false, pgLost)
  }

  action calibStep = any {
    dispatchToAny,
    buildEndsPermanentAny,
    processPermanentAny,
    resubmitResetAny,
    leaderFailoverNoRecascade,
    respawnExecutorAny,
  }
}
