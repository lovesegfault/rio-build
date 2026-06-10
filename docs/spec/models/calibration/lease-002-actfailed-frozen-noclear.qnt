// -*- mode: Bluespec; -*-
//
// Calibration override / regression pin, bughunt-6 S6 bug_002
// (rio-lease act-failed read-resolution totality): re-introduces the
// AS-BUILT routing in which the evidence-free believing holder=self
// completed read (frozen content, or moved content with an empty
// ledger — production route_act_failed_read's FunnelResolve cells)
// runs NO standing transition: the read happens, its facts re-baseline
// the cursor, but the 409-deferral latch survives — so a later
// believing 409 exhausts on a question an intervening completed read
// already answered, and the node loses with the factually false "two
// consecutive renew 409s" warn.
//
// PRE-FIX behavior modeled (one swapped action):
// calibActFailedHeldReadNoClear — identical to the live
// actFailedHeldReadObserved except `conflictDeferredV' =
// conflictDeferredV` (no clear). The deferralResolvedV ghost still
// records the resolution (ground truth: the read DID answer the
// question), so the conflict action's exhausted branch latches
// staleExhaustedLose.
//
// KILL ISOLATION (R16): fetchObservesOwnCommit and selfFence keep
// their FIXED clearing forms here (unlike the sibling
// lease-002-stale-deferral pin, which swaps those two) — a fixed
// resolver clears the latch in the same action that sets the
// resolved ghost, so exhausts-with-resolvedV is REACHABLE ONLY
// through this pin's inert act-failed read: any noStaleExhaustedLose
// violation routes through the swapped conjunct, with the sibling
// resolvers proven intact in the same lane. The twin is therefore
// also the reachability evidence for the live action: the inert
// variant shares its guard, regime constants, and step family, so
// the violating trace fires the action family the fix wires.
//
// PREDICTED falsification: noStaleExhaustedLose in the
// holder-evidence constant family.
//
// Run (the wired pin — as-built no-transition routing falsifies):
//   quint verify --backend=tlc --main=leaseCalib002ActFailedFrozenNoClear \
//     --step=calibStep --invariant=noStaleExhaustedLose \
//     docs/spec/models/calibration/lease-002-actfailed-frozen-noclear.qnt
module leaseCalib002ActFailedFrozenNoClear {
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
    STEP_DOWN_COUNT_KEYED = false
  ).* from "../leaderElection"

  /// PRE-FIX (bug_002): the evidence-free believing holder=self read
  /// fires — facts observed, cursor re-baselined — but the deferral
  /// latch is NOT cleared (the as-built arm ran no standing
  /// transition on this cell). The resolved ghost still records the
  /// ground truth.
  action calibActFailedHeldReadNoClear(n: Node): bool = all {
    HOLDER_EVIDENCE_LOSE,
    alive.get(n),
    pending.get(n) == NoPending,
    leading.get(n),
    lease.holder == Held(n),
    not(evidenceEnabled(n)),
    fence' = fence,
    leading' = leading,
    gen' = gen,
    unconfirmed' = unconfirmed,
    lastFetchedKey' =
      if (MAX_DROPS == 0) lastFetchedKey
      else lastFetchedKey.put(n, leaseKey(lease)),
    lastFetchedContent' =
      if (MAX_FOREIGN_BUMPS == 0) lastFetchedContent
      else lastFetchedContent.put(n, lease.content),
    evidenceFabricated' = evidenceFabricated,
    evidenceConsumed' = evidenceConsumed,
    foreignBumps' = foreignBumps,
    // PRE-FIX (bug_002): the resolving completed read does NOT clear
    // the deferral latch — the swapped line this pin exists for.
    conflictDeferredV' = conflictDeferredV,
    deferralResolvedV' =
      deferralResolvedV.put(
        n, (deferralResolvedV.get(n) or conflictDeferredV.get(n))
      ),
    staleExhaustedLose' = staleExhaustedLose,
    blindConflictLose' = blindConflictLose,
    acquireInstV' = acquireInstV,
    stepDownReqV' = stepDownReqV,
    stepDownsV' = stepDownsV,
    staleStepDownServed' = staleStepDownServed,
    snap' = snap.put(n, Snap(lease)),
    held' = trackHeld(held, n, true),
    blindHolderFor' = bhNext(
      lease, leading, alive, unconfirmed,
      (if (MAX_DROPS == 0) lastFetchedKey else lastFetchedKey.put(n, leaseKey(lease))),
      blindHolderFor, false
    ),
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

  /// One node's PRE-FIX no-clear act-failed read.
  action calibActFailedHeldReadNoClearAny = {
    nondet n = NODES.oneOf()
    calibActFailedHeldReadNoClear(n)
  }

  /// holderEvidenceStep with the act-failed read swapped for the
  /// pre-fix no-transition variant; every OTHER resolver stays FIXED
  /// (kill isolation — see the header).
  action calibStep = any {
    tickAny,
    apiGetAny,
    stealAny,
    renewLeaseAny,
    conflictAny,
    loseObservedAny,
    selfFenceAny,
    renewSendDroppedAny,
    renewSendLostAny,
    fetchObservesOwnCommitAny,
    calibActFailedHeldReadNoClearAny,
    bumpRv,
  }
}
