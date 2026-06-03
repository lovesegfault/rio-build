// -*- mode: Bluespec; -*-
//
// Calibration override / regression pin, bughunt-wave F1 bug_387
// (rio-lease release gate): re-introduces the BELIEF-gated graceful
// release — the pre-fix behavior the LeaseStanding belief/hold split
// made unrepresentable (rio-lease/src/lib.rs: the shutdown release
// gated on `was_leading`, the same bool the local self-fence clears,
// so fence-then-SIGTERM skipped step_down while the apiserver still
// named us holder).
//
// PRE-FIX behavior modeled (one swapped action):
// calibShutdownBeliefGated — identical to shutdown except the release
// predicate is `leading.get(n)` (belief) instead of `held.get(n)`
// (the unsuperseded hold). The latch keeps the LIVE gate's spec
// (held ∧ holder == us ∧ no release), so the pin falsifies exactly
// when the belief gate skips a release the hold gate owes.
//
// PREDICTED falsification: gracefulHandover in the shutdown regime,
// via the fence-then-SIGTERM trace (acquire → blind past the fence
// deadline → selfFence clears belief, hold intact → shutdown skips
// the release while the lease still names the node).
//
// This pin is the non-vacuity evidence for
// quint-leader-election-shutdown: if it ever stops falsifying, the
// model has drifted away from the fence-then-exit class it documents.
//
// Run (the wired pin — pre-fix belief gating falsifies):
//   quint verify --backend=tlc --main=leaseCalib387BeliefGate \
//     --step=calibStep --invariant=gracefulHandover \
//     docs/spec/models/calibration/lease-387-belief-gate.qnt
module leaseCalib387BeliefGate {
  import leaderElection(
    NODES = Set("n1", "n2"),
    FENCE_AFTER = 2,
    STEAL_AFTER = 5,
    RENEW = 1,
    MAX_SKEW = 1,
    MAX_TIME = 8,
    MAX_GEN = 4,
    MAX_RV = 8,
    MAX_DELETES = 0,
    MAX_CLAIM_FAILURES = 0,
    MAX_RESTORES = 0,
    MAX_SUSPENDS = 0,
    MAX_SHUTDOWNS = 1
  ).* from "../leaderElection"

  /// PRE-FIX shutdown: releases on BELIEF. The latch is the live
  /// gate's spec (held-gated), so a skipped owed release latches.
  action calibShutdownBeliefGated(n: str): bool = {
    val releases = leading.get(n) and lease.holder == Held(n)
    all {
      shutdowns < MAX_SHUTDOWNS,
      alive.get(n),
      not(exited.get(n)),
      pending.get(n) == NoPending,
      lease.rv < MAX_RV,
      releaseSkippedWhileHolder' = (releaseSkippedWhileHolder or
        (held.get(n) and lease.holder == Held(n) and not(releases))),
      fenceThenExit' = (fenceThenExit or
        (held.get(n) and not(leading.get(n)) and lease.holder == Held(n))),
      lease' = if (releases)
                 { holder: NoHolder, rv: lease.rv + 1, gen: lease.gen }
               else lease,
      alive' = alive.put(n, false),
      exited' = exited.put(n, true),
      leading' = leading.put(n, false),
      held' = trackHeld(held, n, false),
      shutdowns' = shutdowns + 1,
      snap' = snap.put(n, NoSnap),
      obs' = obs.put(n, NoObs),
      clocks' = clocks, fence' = fence, gen' = gen, genHW' = genHW,
      genHWHolder' = genHWHolder, acquiredAt' = acquiredAt,
      casRace' = casRace, deletes' = deletes, delVictims' = delVictims,
      claimFailures' = claimFailures, restores' = restores,
      stealRetained' = stealRetained,
      stealBumpedForeignTie' = stealBumpedForeignTie,
      renewBumpedAboveEntry' = renewBumpedAboveEntry,
      pending' = pending, suspends' = suspends,
      suspendStraddled' = suspendStraddled,
    }
  }

  action calibShutdownAny = {
    nondet n = NODES.oneOf()
    calibShutdownBeliefGated(n)
  }

  /// The live step with ONLY the shutdown swapped for its pre-fix
  /// belief-gated form.
  action calibStep = any {
    tickAny,
    apiGetAny,
    stealAny,
    renewLeaseAny,
    conflictAny,
    selfFenceAny,
    crashAny,
    recoverAny,
    deleteLease,
    pgRestore,
    renewSendSteadyAny,
    renewReadOkAny,
    calibShutdownAny,
  }
}
