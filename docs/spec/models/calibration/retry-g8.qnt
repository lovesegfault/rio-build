// -*- mode: Bluespec; -*-
//
// Stage-C calibration overrides, family G8: "failover loses or fabricates
// attempt history" (retry-invariant-map.md, calibration table; inventory
// section 5). Same extend-and-override pattern as retry-g1.qnt.

// =========================================================================
// CALIBRATION -- the 891a6520d poison-set shape, "the poison set fails to
// survive failover". The commit's actual mechanisms: (a) the old
// load_poisoned_derivations skipped status='poisoned' rows whose
// poisoned_at was NULL -- the crash window of the old non-atomic
// persist_status + set_poisoned_at sequence, closed by the atomic
// persist_poisoned -- and (b) the dag::remove_build reap deleted
// recovered-poisoned nodes (interested_builds empty from birth) on the
// first build completion after recovery. The pre-fix DAG DID reload
// timestamped poisoned rows; this override is an abstraction at model
// resolution, not a literal revert: the model's PG poison write is one
// atomic step (no NULL-timestamp window) and it has no build-completion
// reap, so both mechanisms are abstracted as "the poisoned-row arm of
// the failover behaves like the not-reloaded arm". The commit's
// id_to_hash/build-summary half is NOT covered here (build-level
// bookkeeping, NOT-ENC in the calibration table).
// PREDICTED: recoveryPreservesPoisonStatus falsifies in the post-failover
// state (a durable, unexpired Poisoned row behind a derivation that came
// back as anything but Poisoned), failover alphabet.
// =========================================================================
module retryCalibG8PoisonedRowNotReloaded {
  import retryPolicy(
    EXECUTORS = Set("w1"),
    MAX_TIME = 2,
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

  /// PRE-FIX leader failover: poisoned rows are simply not reloaded --
  /// the new leader's DAG has no trace of the poison.
  action leaderFailoverDropsPoisoned = all {
    failoversLeft > 0,
    val gone = dStatus == DAbsent or dStatus == DCompleted
    val projNT = recoveryProjectionNonTerminal(pg)
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
        // PRE-FIX: the poisoned row is dropped from the recovered DAG.
        dStatus' = DAbsent, live' = zeroCounters, spec' = zeroCounters,
        specVerdict' = VRequeue, pg' = pg, poisonSrc' = NoPoison,
        depStatus' = depStatus, everFailureTerminal' = everFailureTerminal,
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
    dispatchToAny,
    buildEndsPermanentAny,
    processPermanentAny,
    leaderFailoverDropsPoisoned,
  }
}

// =========================================================================
// CALIBRATION -- the family-level "wrong failed-count reconstruction"
// shape (the inventory's G8 description; the per-commit anchors are the
// recovery halves of 5b4543c3a / 891a6520d and the from_poisoned_row gap
// a4bcb5623's message records): recovery loads the persisted exclusion
// set but does not derive failure_count from it, so the recovered
// distinct-worker progress under-counts what the durable rows support.
// PREDICTED: recoveryIsTheDocumentedProjection falsifies in the
// post-failover state (failure_count is 0 where the documented projection
// derives failed_builders.len()), failover alphabet.
// =========================================================================
module retryCalibG8FailureCountNotDerived {
  import retryPolicy(
    EXECUTORS = Set("w1"),
    MAX_TIME = 2,
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

  /// PRE-FIX leader failover: the non-terminal recovery constructor loads
  /// count / resubmit_cycles / failed_builders but leaves failure_count at
  /// its default instead of deriving it from the exclusion set.
  action leaderFailoverNoDerivedCount = all {
    failoversLeft > 0,
    val gone = dStatus == DAbsent or dStatus == DCompleted
    val expiredInDowntime = and {
      pg.status == PgPoisoned,
      match pg.poisonedAt { | NoTime => false | At(t) => now - t > POISON_TTL },
    }
    val projNT = { ...recoveryProjectionNonTerminal(pg), failureCount: 0 }
    val projP = { ...recoveryProjectionPoisoned(pg), failureCount: 0 }
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
        // PRE-FIX: failure_count not derived (the spec ghost keeps the
        // documented projection -- it is the oracle, not the bug).
        dStatus' = DPoisoned, live' = projP, spec' = recoveryProjectionPoisoned(pg),
        specVerdict' = VPoison(RThreshold), pg' = pg,
        poisonSrc' = if (poisonSrc == NoPoison) EventPoison else poisonSrc,
        depStatus' = DepFailed,
        everFailureTerminal' = true,
        lastPoisonReason' = lastPoisonReason,
      } else if (pg.status == PgNonTerminal and orphanPoisons) all {
        dStatus' = DPoisoned,
        live' = { ...projNT, poisonedAt: At(now) },
        spec' = recoveryProjectionNonTerminal(pg),
        specVerdict' = VRequeue,
        pg' = { ...pg, status: PgPoisoned, poisonedAt: At(now) },
        poisonSrc' = RecoveryPoison,
        depStatus' = DepFailed,
        everFailureTerminal' = true,
        lastPoisonReason' = RThreshold,
      } else if (pg.status == PgNonTerminal) all {
        // PRE-FIX: failure_count not derived on the non-terminal path.
        dStatus' = DReady, live' = projNT, spec' = recoveryProjectionNonTerminal(pg),
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

  action buildEndsTransientAny = { nondet w = EXECUTORS.oneOf() buildEndsReported(w, RcTransient, false) }
  action processTransientAny = {
    nondet w = EXECUTORS.oneOf()
    processReport(w, RcTransient, false, false, false, false)
  }

  action calibStep = any {
    dispatchToAny,
    buildEndsTransientAny,
    processTransientAny,
    leaderFailoverNoDerivedCount,
  }
}
