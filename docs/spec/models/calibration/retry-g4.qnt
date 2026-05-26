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
  import retryPolicyAsBuilt(
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
  ).* from "../retryPolicyAsBuilt"

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
  import retryPolicyAsBuilt(
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
  ).* from "../retryPolicyAsBuilt"

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
// record-then-check premise holds for the WORKER-REPORTED charging sites
// (they check the threshold at the moment they record), and the
// dispatch-time fleet-exhaust backstop covers the placement-starvation
// shape -- but NOT for the backstop charging site:
// tick_process_backstop_timeouts records failed_builders/failure_count
// WITHOUT checking and explicitly delegates the poison decision to
// reassign_derivations, so the same code-level check is the only
// threshold gate on that path. The model encodes that delegated check
// inside backstopFires (observed as OE8), which is deliberately OUTSIDE
// this probe's scope: the probe asserts only that the
// disconnect/force-drain-triggered arm (observed as OE5) never fires
// over the as-built step on a two-slot worker+pod-death+crash+wedge
// alphabet with MAX_FAILOVERS = 0 and PG_FAULTS = 0 (no failover, no
// lost-PG-write histories). A "no violation" verdict is the
// machine-checked evidence for the NARROWED disposition recorded in the
// calibration table -- "the disconnect-triggered arm is unreachable
// as-built" -- not evidence that the code-level check is dead.
// =========================================================================
module retryCalibG4DisconnectThresholdProbe {
  import retryPolicyAsBuilt(
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
  ).* from "../retryPolicyAsBuilt"

  /// "The disconnect path's threshold re-check never produces the poison"
  /// -- checked as an invariant over the as-built step; HOLDS means the
  /// disconnect/force-drain-triggered arm never fires in today's
  /// structure over this probe's alphabet (no failover, no lost PG
  /// writes). It does NOT mean the code-level check is dead: the same
  /// block is the only threshold gate on the backstop path, whose
  /// instance is modeled inside backstopFires (OE8) and is outside this
  /// probe's scope.
  val noDisconnectThresholdPoison = not(last.kind == OE5 and last.terminal)
}

// =========================================================================
// CALIBRATION -- pre-b09c5b312 (the X13 half): the poison-TTL clear reset
// the in-memory state but PG only got the status update -- failed_workers
// (and retry_count) stayed behind in the durable row, so a crash after
// the clear recovered a stale exclusion set that immediately excluded the
// previously-failed workers from placement in the fresh cycle.
// PREDICTED: clearedPoisonScrubsExclusions falsifies in the state right
// after a TTL clear whose durable row kept failed_builders, worker
// alphabet with enough clock for the TTL to elapse.
// =========================================================================
module retryCalibG4TtlClearKeepsExclusions {
  import retryPolicyAsBuilt(
    EXECUTORS = Set("w1"),
    MAX_TIME = 3,
    MAX_ATTEMPTS = 2,
    ATTEMPT_BOUND = 2,
    MAX_FAILOVERS = 0,
    PG_FAULTS = 0,
    ENABLE_WORKER = true,
    ENABLE_PODDEATH = false,
    ENABLE_CRASH = false,
    ENABLE_WEDGE = false,
    ENABLE_RESETS = true
  ).* from "../retryPolicyAsBuilt"

  /// PRE-FIX poison-TTL expiry: the durable clear updates only the
  /// status/timestamp; the failed_builders exclusion set and the retry
  /// count stay behind in the row (the in-memory reset and the durable
  /// row disagree about the next cycle's starting state).
  action poisonTtlExpiresKeepsExclusions = all {
    ENABLE_RESETS,
    dStatus == DPoisoned,
    poisonTtlElapsed,
    val applied = specApply(spec, FPoisonCleared(now), eligibleFleet)
    all {
      dStatus' = DAbsent,
      poisonSrc' = NoPoison,
      live' = zeroCounters,
      spec' = applied.c,
      specVerdict' = applied.v,
      // PRE-FIX: only status/poisoned_at are cleared durably.
      pg' = { ...freshPg, failedBuilders: pg.failedBuilders, retryCount: pg.retryCount },
      last' = { ...noObs, kind: OTtlExpire },
    },
    depStatus' = depStatus,
    everFailureTerminal' = everFailureTerminal,
    attempt' = attempt, execs' = execs,
    now' = now, promotionsLeft' = promotionsLeft, physAttempts' = physAttempts,
    failoversLeft' = failoversLeft, pgFaultsLeft' = pgFaultsLeft,
    lastPoisonReason' = lastPoisonReason, justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
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
    poisonTtlExpiresKeepsExclusions,
  }
}
