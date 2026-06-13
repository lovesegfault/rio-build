// -*- mode: Bluespec; -*-
//
// Calibration override / regression pin, bughunt-5 S8 merged_bug_002
// (rio-lease 409-deferral episode scoping): re-introduces the pre-fix
// resolving read that KEEPS the conflict-deferral latch — the
// loop-local-cleared-by-hand-enumerated-arms world in which the
// FetchedActFailed resolving legs never cleared the deferral, so a
// later believing 409 exhausted on a STALE latch and lost with the
// factually false "two consecutive renew 409s" warn.
//
// PRE-FIX behavior modeled (one swapped action):
// calibFetchObservesOwnCommitNoClear — identical to
// fetchObservesOwnCommit except `conflictDeferredV' =
// conflictDeferredV` (no clear). The deferralResolvedV ghost still
// records the resolution, so the conflict action's exhausted branch
// fires staleExhaustedLose when the un-cleared latch exhausts a 409
// whose question an intervening resolving read already answered.
//
// PREDICTED falsification: noStaleExhaustedLose in the
// holder-evidence constant family. This pin is the non-vacuity
// evidence for quint-leader-election-holder-evidence's
// noStaleExhaustedLose: if it ever stops falsifying, the model has
// drifted away from the stale-deferral class it documents.
//
// Run (the wired pin — pre-fix no-clear falsifies):
//   quint verify --backend=tlc --main=leaseCalib002StaleDeferral \
//     --step=calibStep --invariant=noStaleExhaustedLose \
//     docs/spec/models/calibration/lease-002-stale-deferral.qnt
module leaseCalib002StaleDeferral {
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
    MAX_DROPS = 1,
    MAX_FOREIGN_BUMPS = 2,
    OBS_KEY_RV = false,
    BLIND_HOLDER_BOUND = 8,
    HOLDER_EVIDENCE_LOSE = true,
    CONFLICT_IMMEDIATE_LOSE = false,
    MAX_STEP_DOWNS = 0,
    STEP_DOWN_COUNT_KEYED = false,
    CONFLICT_CLEARS_HOLD = false,
    MAX_VANISHES = 0,
    ABSENT_READ_KEEPS_BELIEF = false
  ).* from "../leaderElection"

  action calibFetchObservesOwnCommitNoClear(n: Node): bool = all {
    alive.get(n),
    pending.get(n) == NoPending,
    evidenceEnabled(n),
    fence' = fence.put(n, unconfAnchorOf(n)),
    // One action = one production loop pass (rio-lease lib.rs evidence
    // arm): the evidence stamp restores belief AND the fence still
    // arbitrates in the SAME pass — a ledger anchor already past
    // FENCE_AFTER re-fences before the pass ends, so belief never
    // outlives the discipline loopInterval states. Caught by TLC on
    // the foreign-rv regime: a LOST write's eldest-kept anchor consumed
    // via a LATER committing write's evidence re-believed 7 ticks stale.
    leading' = leading.put(n, clocks.get(n) - unconfAnchorOf(n) <= FENCE_AFTER),
    gen' = gen.put(n, if (gen.get(n) > lease.gen + 1) gen.get(n) else lease.gen + 1),
    unconfirmed' = unconfirmed.put(n, NoUnconf),
    lastFetchedKey' = lastFetchedKey.put(n, leaseKey(lease)),
    // merged_bug_180 ground truth, judged on CONTENT regardless of
    // what the evidence cursor keys on: consuming "evidence" while the
    // holder-authored content never moved past this node's last
    // completed fetch means the consumed signal was foreign rv churn.
    // P5 latch integrity: the fabrication/consumption oracles live in
    // the SHARED live module — calibration actions frame them
    // identity (this pin exercises the deferral plane only).
    evidenceFabricated' = evidenceFabricated,
    evidenceConsumed' = evidenceConsumed,
    lastFetchedContent' =
      if (MAX_FOREIGN_BUMPS == 0) lastFetchedContent
      else lastFetchedContent.put(n, lease.content),
    foreignBumps' = foreignBumps,
    // PRE-FIX (merged_bug_002): the resolving completed read does NOT
    // clear the deferral latch — the swapped line this pin exists for.
    conflictDeferredV' = conflictDeferredV,
    deferralResolvedV' =
      if (not(HOLDER_EVIDENCE_LOSE)) deferralResolvedV
      else deferralResolvedV.put(
        n, (deferralResolvedV.get(n) or conflictDeferredV.get(n))
      ),
    staleExhaustedLose' = staleExhaustedLose,
    blindConflictLose' = blindConflictLose,
    holdClearedByConflict' = holdClearedByConflict,
    leaseAbsent' = leaseAbsent, vanishes' = vanishes,
    keptBelievingPastAbsentRead' = keptBelievingPastAbsentRead,
    acquireInstV' = acquireInstV,
    stepDownReqV' = stepDownReqV,
    stepDownsV' = stepDownsV,
    staleStepDownServed' = staleStepDownServed,
    snap' = snap.put(n, Snap(lease)),
    held' = trackHeld(held, n, true),
    blindHolderFor' = 0,  // the holder re-believes; its entry is consumed
    clocks' = clocks, lease' = lease, alive' = alive, obs' = obs,
    genHW' = genHW, genHWHolder' = genHWHolder,
    acquiredAt' = acquiredAt, casRace' = casRace, deletes' = deletes,
    delVictims' = delVictims, claimFailures' = claimFailures,
    restores' = restores, stealRetained' = stealRetained,
    stealBumpedForeignTie' = stealBumpedForeignTie,
    renewBumpedAboveEntry' = renewBumpedAboveEntry,
    pending' = pending, exited' = exited, drops' = drops,
    suspends' = suspends, shutdowns' = shutdowns,
    releaseSkippedWhileHolder' = releaseSkippedWhileHolder,
    suspendStraddled' = suspendStraddled, fenceThenExit' = fenceThenExit,
  }

  action calibSelfFenceNoClear(n: Node): bool = all {
    alive.get(n),
    // A loop parked on an in-flight read runs no fence checks; the
    // first POST-READ tick is where the fence fires (vacuous at
    // MAX_SUSPENDS = 0). held is deliberately UNTOUCHED below — the
    // belief/hold split (bug_387): fencing is local.
    pending.get(n) == NoPending,
    leading.get(n),
    clocks.get(n) - fence.get(n) > FENCE_AFTER,
    leading' = leading.put(n, false),
    clocks' = clocks, lease' = lease, alive' = alive, snap' = snap,
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
    unconfirmed' = unconfirmed, lastFetchedKey' = lastFetchedKey,
    drops' = drops,
    foreignBumps' = foreignBumps,
    // PRE-FIX (merged_bug_002): the fence lose edge does NOT clear
    // the deferral latch — it leaks into the next belief episode.
    conflictDeferredV' = conflictDeferredV,
    // -- and the resolved-since ghost records the resolution.
    deferralResolvedV' =
      if (not(HOLDER_EVIDENCE_LOSE)) deferralResolvedV
      else deferralResolvedV.put(
        n, (deferralResolvedV.get(n) or conflictDeferredV.get(n))
      ),
    staleExhaustedLose' = staleExhaustedLose,
    blindConflictLose' = blindConflictLose,
    holdClearedByConflict' = holdClearedByConflict,
    leaseAbsent' = leaseAbsent, vanishes' = vanishes,
    keptBelievingPastAbsentRead' = keptBelievingPastAbsentRead,
    acquireInstV' = acquireInstV,
    stepDownReqV' = stepDownReqV,
    stepDownsV' = stepDownsV,
    staleStepDownServed' = staleStepDownServed,
    lastFetchedContent' = lastFetchedContent,
    evidenceFabricated' = evidenceFabricated,
    evidenceConsumed' = evidenceConsumed,
    blindHolderFor' = bhNext(lease, leading.put(n, false), alive, unconfirmed, lastFetchedKey, blindHolderFor, false),
  }

  /// One node's PRE-FIX no-clear self-fence.
  action calibSelfFenceNoClearAny = {
    nondet n = NODES.oneOf()
    calibSelfFenceNoClear(n)
  }

  /// One node's PRE-FIX no-clear resolving fetch.
  action calibFetchObservesOwnCommitNoClearAny = {
    nondet n = NODES.oneOf()
    calibFetchObservesOwnCommitNoClear(n)
  }

  /// holderEvidenceStep with the resolving read swapped for the
  /// pre-fix no-clear variant.
  action calibStep = any {
    tickAny,
    apiGetAny,
    stealAny,
    renewLeaseAny,
    conflictAny,
    loseObservedAny,
    calibSelfFenceNoClearAny,
    renewSendDroppedAny,
    renewSendLostAny,
    calibFetchObservesOwnCommitNoClearAny,
    bumpRv,
  }
}
