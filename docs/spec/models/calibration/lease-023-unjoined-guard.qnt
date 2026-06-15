// -*- mode: Bluespec; -*-
//
// Calibration override / regression pin, bughunt-r26 bug_023
// (rio-controller guard thread join): re-introduces the UNJOINED
// guard thread — the pre-fix behavior the `(GuardHandle, GuardJoin)`
// split makes unrepresentable (`guard.rs`: `std::thread::Builder
// ::spawn(...).expect(...);` discarded its `JoinHandle`, so `main()`
// returned and process exit killed the detached `rio-guard` thread
// mid-`step_down()` PATCH). The §Verifier-one-step-removed(b)
// recurrence of bug_118 one lifecycle level up: runtime→task drain
// was fixed; process→thread join was not.
//
// PRE-FIX behavior modeled (one swapped action):
// calibShutdownUnjoined — identical to shutdown except a
// nondeterministic `landed` ANDs into the release: the gate DECIDES
// to release exactly as the live action does, but whether the PATCH
// reaches the apiserver before the OS tears down the detached thread
// is the process-exit race the GuardJoin token closes.
//
// PREDICTED falsification: gracefulHandover in the shutdown regime,
// via the idle-SIGTERM trace (acquire → SIGTERM → reconcilers drain
// instantly → main() returns → guard thread killed mid-PATCH; the
// apiserver still names the dead pod).
//
// This pin is the non-vacuity evidence for the GuardJoin clause in
// quint-leader-election-shutdown's shutdown action: if it ever stops
// falsifying, the model has drifted away from the process↔thread
// lifecycle level it documents.
//
// Run (the wired pin — pre-fix unjoined guard falsifies):
//   quint verify --backend=tlc --main=leaseCalib023UnjoinedGuard \
//     --step=calibStep --invariant=gracefulHandover \
//     docs/spec/models/calibration/lease-023-unjoined-guard.qnt
module leaseCalib023UnjoinedGuard {
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
    MAX_SHUTDOWNS = 1,
    MAX_DROPS = 0,
    MAX_FOREIGN_BUMPS = 0,
    OBS_KEY_RV = false,
    BLIND_HOLDER_BOUND = 0,
    HOLDER_EVIDENCE_LOSE = false,
    CONFLICT_IMMEDIATE_LOSE = false,
    MAX_STEP_DOWNS = 0,
    STEP_DOWN_COUNT_KEYED = false,
    CONFLICT_CLEARS_HOLD = false,
    MAX_VANISHES = 0,
    ABSENT_READ_KEEPS_BELIEF = false
  ).* from "../leaderElection"

  /// PRE-FIX shutdown: the release gate is the LIVE hold-gated one,
  /// but `landed` ANDs in — the process-exit race. The latch is the
  /// live gate's spec (held ∧ holder == us ∧ no release), so a
  /// decided-but-aborted release latches.
  action calibShutdownUnjoined(n: str, landed: bool): bool = {
    val releases = held.get(n) and lease.holder == Held(n) and landed
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
                 { holder: NoHolder, rv: lease.rv + 1, content: lease.content + 1, gen: lease.gen }
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
      // New-var frames mirroring the live shutdown (the exiting
      // process takes its in-memory ledger and cursor with it;
      // MAX_DROPS = 0 here freezes all four).
      conflictDeferredV' = conflictDeferredV,
      deferralResolvedV' = deferralResolvedV,
      staleExhaustedLose' = staleExhaustedLose,
      blindConflictLose' = blindConflictLose,
      holdClearedByConflict' = holdClearedByConflict,
      leaseAbsent' = leaseAbsent, vanishes' = vanishes,
      keptBelievingPastAbsentRead' = keptBelievingPastAbsentRead,
      acquireInstV' = acquireInstV,
      stepDownReqV' = stepDownReqV,
      stepDownsV' = stepDownsV,
      staleStepDownServed' = staleStepDownServed,
      unconfirmed' = unconfirmed.put(n, NoUnconf),
      lastFetchedKey' = lastFetchedKey.put(n, -1),
      drops' = drops,
      foreignBumps' = foreignBumps,
      lastFetchedContent' = lastFetchedContent,
      evidenceFabricated' = evidenceFabricated,
      evidenceConsumed' = evidenceConsumed,
      blindHolderFor' = bhNext(
        if (releases) { holder: NoHolder, rv: lease.rv + 1, content: lease.content + 1, gen: lease.gen } else lease,
        leading.put(n, false), alive.put(n, false),
        unconfirmed.put(n, NoUnconf), lastFetchedKey,
        blindHolderFor, false),
    }
  }

  /// The live step with ONLY the shutdown swapped for its pre-fix
  /// unjoined-guard form (nondet `landed`).
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
    {
      nondet n = NODES.oneOf()
      nondet landed = Set(true, false).oneOf()
      calibShutdownUnjoined(n, landed)
    },
  }
}
