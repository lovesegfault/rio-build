// -*- mode: Bluespec; -*-
//
// Stage-C calibration overrides, family G4: "poison state desynced between
// memory and PG, or never cleared" (retry-invariant-map.md, calibration
// table; inventory section 5). Same extend-and-override pattern as
// retry-g1.qnt.

// =========================================================================
// CALIBRATION -- pre-b874e5120: the admin ClearPoison runs in-memory
// FIRST and the PG clear second and best-effort, so a PG blip leaves the
// two stores permanently disagreeing (the in-memory state is cleared, the
// durable row still says Poisoned, and the operator's retry hits the
// status-is-not-Poisoned guard and can never repair it).
// PREDICTED: clearedPoisonClearsDurably falsifies in the state right
// after a clear whose PG write was lost, in a worker-channel alphabet
// with one allowed lost mirror write. (The downstream consequence -- the
// poison resurrecting at the next failover -- is the documented
// projection of whatever PG holds, so the recovery invariants themselves
// stay green; the defect is the clear's durability discipline, which is
// exactly what the new invariant pins.)
// =========================================================================
module retryCalibG4ClearInMemFirst {
  import retryPolicy(
    EXECUTORS = Set("w1"),
    MAX_TIME = 2,
    MAX_ATTEMPTS = 2,
    ATTEMPT_BOUND = 2,
    MAX_FAILOVERS = 0,
    PG_FAULTS = 1,
    ENABLE_WORKER = true,
    ENABLE_PODDEATH = false,
    ENABLE_CRASH = false,
    ENABLE_WEDGE = false,
    ENABLE_RESETS = true
  ).* from "../retryPolicy"

  /// PRE-FIX admin ClearPoison: in-memory reset first (the node leaves the
  /// poison set immediately), PG clear second and best-effort -- a lost
  /// write leaves the durable row poisoned behind a cleared derivation.
  action adminClearInMemFirst(pgLost: bool): bool = all {
    ENABLE_RESETS,
    dStatus == DPoisoned,
    pgLost implies pgFaultsLeft > 0,
    val applied = specApply(spec, FPoisonCleared(now), eligibleFleet)
    all {
      dStatus' = DAbsent,
      poisonSrc' = NoPoison,
      live' = zeroCounters,
      spec' = applied.c,
      specVerdict' = applied.v,
      // PRE-FIX: the PG clear is best-effort; a lost write leaves the row.
      pg' = if (pgLost) pg else freshPg,
      last' = { ...noObs, kind: OPoisonClear },
    },
    depStatus' = depStatus,
    everFailureTerminal' = everFailureTerminal,
    attempt' = attempt, execs' = execs,
    now' = now, promotionsLeft' = promotionsLeft, physAttempts' = physAttempts,
    failoversLeft' = failoversLeft,
    pgFaultsLeft' = if (pgLost) pgFaultsLeft - 1 else pgFaultsLeft,
    lastPoisonReason' = lastPoisonReason, justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  action adminClearInMemFirstAny = { nondet pgLost = Set(true, false).oneOf() adminClearInMemFirst(pgLost) }
  action buildEndsPermanentAny = { nondet w = EXECUTORS.oneOf() buildEndsReported(w, RcPermanent, false) }
  action processPermanentAny = {
    nondet w = EXECUTORS.oneOf()
    processReport(w, RcPermanent, false, false, false, false)
  }

  action calibStep = any {
    dispatchToAny,
    buildEndsPermanentAny,
    processPermanentAny,
    adminClearInMemFirstAny,
  }
}

// =========================================================================
// CALIBRATION -- pre-f9adf3c76: a poison whose TTL elapsed while no leader
// was running is reloaded at recovery anyway, and the monotonic-clock
// clamp re-stamps it with a fresh poisoned_at -- a derivation that should
// have been free gets a fresh full TTL.
// PREDICTED: recoveryIsTheDocumentedProjection falsifies in the
// post-failover state (the recovered poisoned_at is the recovery time,
// not the row's), failover alphabet with enough clock to let the TTL
// elapse before the failover.
// =========================================================================
module retryCalibG4ReloadExpiredPoison {
  import retryPolicy(
    EXECUTORS = Set("w1"),
    MAX_TIME = 4,
    MAX_ATTEMPTS = 2,
    ATTEMPT_BOUND = 2,
    MAX_FAILOVERS = 1,
    PG_FAULTS = 0,
    ENABLE_WORKER = true,
    ENABLE_PODDEATH = false,
    ENABLE_CRASH = false,
    ENABLE_WEDGE = false,
    ENABLE_RESETS = false
  ).* from "../retryPolicy"

  /// PRE-FIX leader failover: no expired-poison filter -- every poisoned
  /// row is reloaded as poisoned, and the clamp stamps poisoned_at with
  /// the recovery time instead of the row's value.
  action leaderFailoverReloadsExpired = all {
    failoversLeft > 0,
    val gone = dStatus == DAbsent or dStatus == DCompleted
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
      } else if (pg.status == PgPoisoned) all {
        // PRE-FIX: reloaded regardless of the TTL, with poisoned_at
        // clamped to "now" (the spec ghost keeps the documented
        // projection -- it is the oracle, not the bug).
        dStatus' = DPoisoned,
        live' = { ...projP, poisonedAt: At(now) },
        spec' = projP,
        specVerdict' = VPoison(RThreshold), pg' = pg,
        poisonSrc' = if (poisonSrc == NoPoison) EventPoison else poisonSrc,
        depStatus' = DepFailed,
        everFailureTerminal' = true,
        lastPoisonReason' = lastPoisonReason,
      } else if (pg.status == PgNonTerminal and orphanPoisons) all {
        dStatus' = DPoisoned,
        live' = { ...projNT, poisonedAt: At(now) },
        spec' = projNT,
        specVerdict' = VRequeue,
        pg' = { ...pg, status: PgPoisoned, poisonedAt: At(now) },
        poisonSrc' = RecoveryPoison,
        depStatus' = DepFailed,
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
    processReport(w, RcPermanent, false, false, false, false)
  }

  action calibStep = any {
    tick,
    dispatchToAny,
    buildEndsPermanentAny,
    processPermanentAny,
    leaderFailoverReloadsExpired,
  }
}

// =========================================================================
// REACHABILITY PROBE (not an override) -- b09c5b312's X6 half added a
// poison-threshold re-check to the disconnect path. In today's code the
// charging sites themselves check the threshold at record time
// (record-then-check) and the dispatch-time fleet-exhaust backstop covers
// the placement-starvation shape, so the E5 re-check should only be
// reachable if some OTHER path can leave a Running derivation with a
// threshold-crossing failed_builders set -- which the as-built model says
// cannot happen. The probe runs the AS-BUILT step over a two-slot
// pod-death alphabet and asserts the E5 threshold poison never fires; a
// "no violation" verdict is the machine-checked evidence for the
// redundancy disposition recorded in the calibration table.
// =========================================================================
module retryCalibG4DisconnectThresholdProbe {
  import retryPolicy(
    EXECUTORS = Set("w1", "w2"),
    MAX_TIME = 3,
    MAX_ATTEMPTS = 4,
    ATTEMPT_BOUND = 4,
    MAX_FAILOVERS = 0,
    PG_FAULTS = 0,
    ENABLE_WORKER = true,
    ENABLE_PODDEATH = true,
    ENABLE_CRASH = true,
    ENABLE_WEDGE = true,
    ENABLE_RESETS = true
  ).* from "../retryPolicy"

  /// "The disconnect path's threshold re-check never produces the poison"
  /// -- checked as an invariant over the as-built step; HOLDS means the
  /// arm is dead code in today's structure (the record-then-check sites
  /// and the fleet-exhaust backstop subsume it).
  val noDisconnectThresholdPoison = not(last.kind == OE5 and last.terminal)
}
