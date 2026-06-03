// -*- mode: Bluespec; -*-
//
// Calibration override / regression pin, bughunt-wave F1 bug_096
// (rio-lease suspend-straddle): re-introduces RESPONSE anchoring of
// the self-fence blind window — the pre-fix behavior the
// RenewAnchor/BlindClock newtypes made unrepresentable
// (rio-lease/src/lib.rs: the Ok arm stamped `last_successful_renew =
// fence_now()` at response arrival, erasing a suspend that straddled
// the in-flight round-trip).
//
// PRE-FIX behavior modeled (one swapped action):
// calibRenewReadOkResponseAnchored — identical to renewReadOk except
// `fence' = clocks.get(n)` (the read instant) instead of the captured
// attempt-start anchor.
//
// PREDICTED falsification: boundedDualLeadership in the suspend
// regime. The straddled believer resumes with a FRESH fence (disarming
// discovery disjunct (1)) and a spent snapshot (disarming (2)/(3));
// no deletion (4). The thief that stole mid-straddle and the resumed
// zombie are both believers with no discovery mechanism armed.
//
// This pin is the non-vacuity evidence for
// quint-leader-election-suspend: if it ever stops falsifying, the
// model has drifted away from the straddle class it documents.
//
// Run (the wired pin — pre-fix response anchoring falsifies):
//   quint verify --backend=tlc --main=leaseCalib096ResponseAnchor \
//     --step=calibStep --invariant=boundedDualLeadership \
//     docs/spec/models/calibration/lease-096-response-anchor.qnt
module leaseCalib096ResponseAnchor {
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
    MAX_SUSPENDS = 1,
    MAX_SHUTDOWNS = 0
  ).* from "../leaderElection"

  /// PRE-FIX read: stamps the fence at the READ instant — the
  /// response-anchored blind window that erases the straddled
  /// interval. Everything else is renewReadOk verbatim.
  action calibRenewReadOkResponseAnchored(n: str): bool = all {
    alive.get(n),
    pending.get(n) != NoPending,
    fence' = fence.put(n, clocks.get(n)),
    pending' = pending.put(n, NoPending),
    suspendStraddled' = (suspendStraddled or
      (clocks.get(n) - anchorOf(n) > FENCE_AFTER)),
    clocks' = clocks, lease' = lease, alive' = alive, leading' = leading,
    snap' = snap, obs' = obs, gen' = gen, genHW' = genHW,
    genHWHolder' = genHWHolder, acquiredAt' = acquiredAt,
    casRace' = casRace, deletes' = deletes, delVictims' = delVictims,
    claimFailures' = claimFailures, restores' = restores,
    stealRetained' = stealRetained,
    stealBumpedForeignTie' = stealBumpedForeignTie,
    renewBumpedAboveEntry' = renewBumpedAboveEntry,
    held' = held, exited' = exited,
    suspends' = suspends, shutdowns' = shutdowns,
    releaseSkippedWhileHolder' = releaseSkippedWhileHolder,
    fenceThenExit' = fenceThenExit,
  }

  action calibRenewReadOkAny = {
    nondet n = NODES.oneOf()
    calibRenewReadOkResponseAnchored(n)
  }

  /// The live step with ONLY the read action swapped for its pre-fix
  /// form (the calibration discipline: one frozen action, everything
  /// else the live model's).
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
    calibRenewReadOkAny,
    shutdownAny,
  }
}
