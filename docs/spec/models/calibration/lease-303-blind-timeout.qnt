// -*- mode: Bluespec; -*-
//
// Calibration override / regression pin, bughunt2-wave slot 8
// merged_bug_303 (rio-lease cancelled writes): re-introduces the
// BLIND-TIMEOUT world — the pre-fix renew loop in which a transmitted
// PUT whose response never arrived stamped nothing and NOTHING could
// ever re-anchor the holder's belief from its own committed writes
// (rio-lease/src/lib.rs pre-fix: a single deadline over the whole
// round, no RenewOutcome phases, no UnconfirmedPut ledger, no
// own-commit evidence).
//
// PRE-FIX behavior modeled (the evidence rule removed, three pieces):
//   - calibStep drops fetchObservesOwnCommit entirely (no consumption,
//     no anchor stamp, no belief re-entry);
//   - calibTick replaces tick WITHOUT the evidence-forcing cap (no
//     obligation exists where no evidence rule exists; the leading
//     cap and the skew bound are kept);
//   - calibGet replaces apiGet WITHOUT the evidence-fetch guard (the
//     pre-fix GET reads and learns nothing about own commits; the
//     ledger/cursor bookkeeping is kept — it feeds the ghost, not the
//     behavior).
//
// PREDICTED falsification: blindHolderBounded. The mid-band schedule
// (fence, then the candidate's re-acquire PUT transmits and drops,
// committing and bumping the rv) leaves the lease's holder fenced
// with own-commit evidence OBSERVABLE and nothing to observe it: the
// window stretches to the trace horizon — one cycle of the unbounded
// leaderless livelock, in which committed-but-cancelled PUTs
// re-anchor every standby's staleness clock while the blind ex-holder
// cannot re-anchor its own belief (production paid this as a
// fence/re-acquire/fence loop with the cluster leaderless throughout;
// the paused-clock mid-band red of the slot-8 work order).
//
// This pin is the non-vacuity evidence for
// quint-leader-election-dropped-write: if it ever stops falsifying,
// the model has drifted away from the livelock the evidence rule
// exists to break.
//
// Run (the wired pin — the blind-timeout world falsifies):
//   quint verify --backend=tlc --main=leaseCalib303BlindTimeout \
//     --step=calibStep --invariant=blindHolderBounded \
//     docs/spec/models/calibration/lease-303-blind-timeout.qnt
module leaseCalib303BlindTimeout {
  import leaderElection(
    NODES = Set("n1", "n2"),
    FENCE_AFTER = 2,
    STEAL_AFTER = 5,
    RENEW = 1,
    MAX_SKEW = 1,
    MAX_TIME = 10,
    MAX_GEN = 4,
    MAX_RV = 8,
    MAX_DELETES = 0,
    MAX_CLAIM_FAILURES = 0,
    MAX_RESTORES = 0,
    MAX_SUSPENDS = 0,
    MAX_SHUTDOWNS = 0,
    MAX_DROPS = 2,
    MAX_FOREIGN_BUMPS = 0,
    OBS_KEY_RV = false,
    BLIND_HOLDER_BOUND = 8,
    HOLDER_EVIDENCE_LOSE = false,
    CONFLICT_IMMEDIATE_LOSE = false,
    MAX_STEP_DOWNS = 0,
    STEP_DOWN_COUNT_KEYED = false,
    CONFLICT_CLEARS_HOLD = false
  ).* from "../leaderElection"

  /// PRE-FIX tick: no evidence-forcing cap (the loop owes no fetch it
  /// has no rule for). Leading cap and skew kept — those predate the
  /// fix.
  action calibTick(n: Node): bool = all {
    clocks.get(n) < MAX_TIME,
    (alive.get(n) and leading.get(n) and pending.get(n) == NoPending) implies
      ((clocks.get(n) + 1) - fence.get(n) <= FENCE_AFTER + RENEW),
    NODES.forall(m => (clocks.get(n) + 1) - clocks.get(m) <= MAX_SKEW),
    clocks' = clocks.put(n, clocks.get(n) + 1),
    lease' = lease, alive' = alive, leading' = leading, snap' = snap,
    obs' = obs, fence' = fence, gen' = gen, genHW' = genHW,
    genHWHolder' = genHWHolder, acquiredAt' = acquiredAt,
    casRace' = casRace, deletes' = deletes,
    delVictims' = delVictims, claimFailures' = claimFailures,
    restores' = restores, stealRetained' = stealRetained,
    stealBumpedForeignTie' = stealBumpedForeignTie,
    renewBumpedAboveEntry' = renewBumpedAboveEntry,
    pending' = pending, held' = held, exited' = exited,
    suspends' = suspends, shutdowns' = shutdowns,
    releaseSkippedWhileHolder' = releaseSkippedWhileHolder,
    suspendStraddled' = suspendStraddled, fenceThenExit' = fenceThenExit,
    // bughunt-4 S2 planes, frozen here (HOLDER_EVIDENCE_LOSE = false,
    // MAX_STEP_DOWNS = 0 in this pin's import):
    conflictDeferredV' = conflictDeferredV,
      deferralResolvedV' = deferralResolvedV,
      staleExhaustedLose' = staleExhaustedLose,
    blindConflictLose' = blindConflictLose,
    holdClearedByConflict' = holdClearedByConflict,
    acquireInstV' = acquireInstV,
    stepDownReqV' = stepDownReqV,
    stepDownsV' = stepDownsV,
    staleStepDownServed' = staleStepDownServed,
    unconfirmed' = unconfirmed, lastFetchedKey' = lastFetchedKey,
    drops' = drops,
    foreignBumps' = foreignBumps,
    lastFetchedContent' = lastFetchedContent,
    evidenceFabricated' = evidenceFabricated,
    evidenceConsumed' = evidenceConsumed,
    blindHolderFor' = bhNext(lease, leading, alive, unconfirmed, lastFetchedKey, blindHolderFor, true),
  }
  action calibTickAny = {
    nondet n = NODES.oneOf()
    calibTick(n)
  }

  /// PRE-FIX GET: no evidence-fetch guard — the read proceeds and
  /// learns nothing about own commits. Bookkeeping (cursor, wholesale
  /// clear on foreign observation) kept: it feeds the ghost; the
  /// pre-fix BEHAVIOR difference is that nothing consumes the entry
  /// or stamps the anchor.
  action calibGet(n: Node): bool = all {
    alive.get(n),
    pending.get(n) == NoPending,
    snap' = snap.put(n, Snap(lease)),
    obs' = obs.put(n,
      if (lease.holder == NoHolder) NoObs
      else if (lease.holder == Held(n)) obs.get(n)
      else if (hasObs(n) and obsKeyOf(n) == leaseKey(lease)) obs.get(n)
      else Observed({ key: leaseKey(lease), since: clocks.get(n) })),
    clocks' = clocks, lease' = lease, alive' = alive, leading' = leading,
    fence' = fence, gen' = gen, genHW' = genHW, genHWHolder' = genHWHolder,
    acquiredAt' = acquiredAt,
    casRace' = casRace, deletes' = deletes, delVictims' = delVictims,
    claimFailures' = claimFailures, restores' = restores,
    stealRetained' = stealRetained,
    stealBumpedForeignTie' = stealBumpedForeignTie,
    renewBumpedAboveEntry' = renewBumpedAboveEntry,
    pending' = pending,
    held' = trackHeld(held, n, if (lease.holder == Held(n)) held.get(n) else false),
    exited' = exited,
    suspends' = suspends, shutdowns' = shutdowns,
    releaseSkippedWhileHolder' = releaseSkippedWhileHolder,
    suspendStraddled' = suspendStraddled, fenceThenExit' = fenceThenExit,
    // bughunt-4 S2 planes, frozen here (HOLDER_EVIDENCE_LOSE = false,
    // MAX_STEP_DOWNS = 0 in this pin's import):
    conflictDeferredV' = conflictDeferredV,
      deferralResolvedV' = deferralResolvedV,
      staleExhaustedLose' = staleExhaustedLose,
    blindConflictLose' = blindConflictLose,
    holdClearedByConflict' = holdClearedByConflict,
    acquireInstV' = acquireInstV,
    stepDownReqV' = stepDownReqV,
    stepDownsV' = stepDownsV,
    staleStepDownServed' = staleStepDownServed,
    unconfirmed' =
      if (lease.holder == Held(n)) unconfirmed
      else unconfirmed.put(n, NoUnconf),
    lastFetchedKey' =
      if (lease.holder == Held(n)) lastFetchedKey
      else if (lease.holder == NoHolder) lastFetchedKey.put(n, -1)
      else lastFetchedKey.put(n, leaseKey(lease)),
    drops' = drops,
    foreignBumps' = foreignBumps,
    lastFetchedContent' = lastFetchedContent,
    evidenceFabricated' = evidenceFabricated,
    evidenceConsumed' = evidenceConsumed,
    blindHolderFor' = bhNext(lease, leading, alive, unconfirmed, lastFetchedKey, blindHolderFor, false),
  }
  action calibGetAny = {
    nondet n = NODES.oneOf()
    calibGet(n)
  }

  /// The mid-band step with the evidence rule REMOVED (one mechanism
  /// deleted, in its three pieces; everything else the live regime's).
  action calibStep = any {
    calibTickAny,
    calibGetAny,
    stealAny,
    conflictAny,
    selfFenceAny,
    renewSendDroppedAny,
  }
}
