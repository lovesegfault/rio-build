// -*- mode: Bluespec; -*-
//
// Stage-C calibration overrides, family M2: the inflight_created mirror
// and ICE (vanish) detection — the four-mutator discipline of
// ctrl.nodeclaim.inflight-conservation. Same extend-and-override pattern
// as controller-m1.qnt.
//
// Run (serially):
//   quint verify --backend=tlc --main=<module> --step=calibStep \
//     --invariant=<predicted> docs/spec/models/calibration/controller-m2.qnt

// =========================================================================
// CALIBRATION — pre-08d49c52c, bug_012 half: consolidate_only ran the
// idle/unhealthy reaps but never pruned inflight_created against the
// tick's live set, so a claim the controller itself reaped on the
// consolidate-only path was misread by the next reachable tick's
// detect_vanished as a Karpenter GC — a spurious ICE mark for the
// controller's own delete.
// PREDICTED: iceMarkSoundness falsifies in the fault-rpc alphabet (the
// regime that can reach consolidate-only mode); the spurious mark is
// raised by the UNCHANGED as-built full tick after the pre-fix
// consolidate-only tick left the stale entry behind.
// =========================================================================
module m2CalibNoConsolidatePrune {
  import nodeclaimLifecycle(
    MAX_FLEET_CORES = 2,
    CLASS_CAP_A = 1,
    BOT_THRESHOLD = 2,
    ENABLE_RPC_FAULT = true,
    ENABLE_LEASE_FAULT = false,
    ENABLE_KARPENTER_FAULT = false,
    START_CEILINGS_KNOWN = false,
    ENABLE_EPOCH_MACHINERY = false,
    ENABLE_ACQUIRE_EPOCH = false,
    ENABLE_CLEARBUF_MACHINERY = false,
    ENABLE_EVIDENCE_BUFFER = false,
    ENABLE_ACK_LATCH = false,
    ENABLE_SNAPSHOT_MACHINERY = false,
    ENABLE_SNAPSHOT_PRESENCE = false,
    ENABLE_DEFER_MACHINERY = false,
    ENABLE_DEFER_DEMAND = false,
    ENABLE_TOMBSTONE_MACHINERY = false,
    ENABLE_FOLD_CLOCK = false,
    ENABLE_LAGGED_COMMIT_OBSERVATION = false,
    ENABLE_TYPED_TOMBSTONE_DISPOSITION = true
  ).* from "../nodeclaimLifecycle"

  /// The as-built consolidate-only tick with the inflight_created prune
  /// removed: reaped claims keep their entry, vanish detection does not
  /// run, registered/terminating entries are not dropped.
  action tickConsolidateOnlyNoPruneWith(pgLoadFails: bool): bool = all {
      isLeader,
      ENABLE_RPC_FAULT,
      botStreak + 1 >= BOT_THRESHOLD,
      val aged = CLAIMS.mapBy(k => ageWallclock(claims.get(k)))
      val loadOk = reloadPending and not(pgLoadFails)
      val afterLatch = CLAIMS.mapBy(k =>
        if (reloadPending)
          { ...aged.get(k), pIdle: INotTracked,
            inflightTracked: if (loadOk) false else aged.get(k).inflightTracked }
        else aged.get(k))
      val reloadStillPending = reloadPending and not(loadOk)
      val observed = CLAIMS.mapBy(k => observeIdleToBusy(afterLatch.get(k)))
      val obsReg = CLAIMS.mapBy(k => observeRegistered(observed.get(k)))
      val afterObs = CLAIMS.mapBy(k => obsReg.get(k)._1)
      val staleRecordOnly = CLAIMS.filter(k => obsReg.get(k)._3).size() > 0
      val lostSamples = CLAIMS.filter(k => obsReg.get(k)._4).size() > 0
      val stuckReaped = CLAIMS.filter(k =>
        afterObs.get(k).phase == CInFlight and afterObs.get(k).stuck)
      // PRE-FIX (08d49c52c): no detect_vanished pass and no prune in
      // consolidate-only mode.
      val idleReaped = CLAIMS.filter(k =>
        val c = afterObs.get(k)
        c.phase == CRegistered and c.spell != SNotIdle and c.pIdle == ITrackedOld)
      val overReap = idleReaped.filter(k => afterObs.get(k).spell != SOld).size() > 0
      val claimsAfter = CLAIMS.mapBy(k =>
        val c = afterObs.get(k)
        if (stuckReaped.contains(k) or idleReaped.contains(k))
          // PRE-FIX: the controller's own reap leaves the inflight entry
          // in place.
          { ...c, phase: CTerminating, stuck: false, spell: SNotIdle,
            busyTickSeen: false, boot: BRecorded, pIdle: INotTracked,
            ctrlReaped: true }
        else c)
      all {
        claims' = claimsAfter,
        placeableArmed' = if (losePending) false else placeableArmed,
        losePending' = false,
        isLeader' = isLeader,
        reloadPending' = reloadStillPending,
        botStreak' = if (botStreak + 1 > BOT_THRESHOLD) BOT_THRESHOLD else botStreak + 1,
        iceMasked' = iceMasked,
        configuredCells' = configuredCells,
        ceilingsKnown' = ceilingsKnown,
        hadAcquire' = hadAcquire,
        // C2 vars: constant under the calibration regimes (axes off).
        edgeClearDone' = edgeClearDone,
        pendingClear' = pendingClear,
        schedBinding' = schedBinding,
        deferredCells' = deferredCells,
        effects' = Set()
          .union(if (idleReaped.size() > 0) Set(EIdleReap, EConsolidateOnlyReap) else Set())
          .union(if (stuckReaped.size() > 0) Set(EStuckReap, EConsolidateOnlyReap) else Set())
          .union(if (staleRecordOnly) Set(EStaleRecordOnly) else Set())
          .union(if (reloadStillPending) Set(EDegradedReloadTick) else Set())
          .union(if (reloadPending and CLAIMS.filter(k => claims.get(k).inflightTracked).size() > 0)
                   Set(EAcquireWithInflight) else Set()),
        violations' = violations
          .union(if (overReap) Set(VIdleOverReap) else Set())
          .union(if (lostSamples) Set(VBootSampleLost) else Set()),
      }
  }

  action tickConsolidateOnlyNoPrune = {
    nondet pgLoadFails = boolChoices(ENABLE_LEASE_FAULT).oneOf()
    tickConsolidateOnlyNoPruneWith(pgLoadFails)
  }

  action calibStep = {
    nondet k = CLAIMS.oneOf()
    nondet cell = CELLS.oneOf()
    any {
      tickFull,
      tickBotEarly,
      tickConsolidateOnlyNoPrune,
      tickStandby,
      claimRegisters(k),
      claimGoesBusy(k),
      claimGoesIdle(k),
      claimLaunchStuck(k),
      karpenterGC(k),
      karpenterTerminationEdge(k),
      finalizerCompletes(k),
      iceClears(cell),
      leaseLose,
      leaseAcquire,
      controllerRestarts,
      ceilingsLoad,
      configDropsCell(cell),
      configRestoresCell(cell),
    }
  }

  /// Deterministic reproducer: the bug_012 incident shape — an idle claim
  /// reaped by consolidate-only mode keeps its inflight entry, and the
  /// first reachable full tick marks its cell as a Karpenter vanish.
  run m2NoConsolidatePruneRun =
    init
      .then(ceilingsLoad)
      .then(tickFullWith(false, false, false, 1, 0, Set(), Set(), Set(), Set()))
      .then(claimRegisters("c1"))
      .then(tickBotEarly)
      .then(tickConsolidateOnlyNoPruneWith(false))
      .then(tickConsolidateOnlyNoPruneWith(false))
      .then(finalizerCompletes("c1"))
      .then(tickFullWith(false, false, false, 0, 0, Set(), Set(), Set(), Set()))
      .expect(violations.contains(VSpuriousMark))
}

// =========================================================================
// CALIBRATION — pre-08d49c52c, bug_020 half (the r40 KEEP arm):
// detect_vanished pruned every inflight_created entry on its FIRST
// appearance in the live LIST, regardless of registered/terminating
// state. A claim observed in-flight at ~10 s and Karpenter-GC'd at
// ~13-16 s escaped every detection path — the cell churned unmasked
// NodeClaims indefinitely. The as-built rule keeps the entry while the
// claim is still in-flight and prunes only on registration/termination.
// The harm is a MISSED mark (a completeness property), so the calibration
// pins the structural conservation half as a module-local invariant:
//   inflightKeptWhileInFlight — a claim that is present and still
//   in-flight keeps its inflight_created entry across a tick.
// PREDICTED: inflightKeptWhileInFlight falsifies under calibStep; the
// as-built step over the same base alphabet HOLDS it (the baseline run,
// no --step flag).
// =========================================================================
module m2CalibInflightDropOnSight {
  import nodeclaimLifecycle(
    MAX_FLEET_CORES = 2,
    CLASS_CAP_A = 1,
    BOT_THRESHOLD = 2,
    ENABLE_RPC_FAULT = false,
    ENABLE_LEASE_FAULT = false,
    ENABLE_KARPENTER_FAULT = false,
    START_CEILINGS_KNOWN = true,
    ENABLE_EPOCH_MACHINERY = false,
    ENABLE_ACQUIRE_EPOCH = false,
    ENABLE_CLEARBUF_MACHINERY = false,
    ENABLE_EVIDENCE_BUFFER = false,
    ENABLE_ACK_LATCH = false,
    ENABLE_SNAPSHOT_MACHINERY = false,
    ENABLE_SNAPSHOT_PRESENCE = false,
    ENABLE_DEFER_MACHINERY = false,
    ENABLE_DEFER_DEMAND = false,
    ENABLE_TOMBSTONE_MACHINERY = false,
    ENABLE_FOLD_CLOCK = false,
    ENABLE_LAGGED_COMMIT_OBSERVATION = false,
    ENABLE_TYPED_TOMBSTONE_DISPOSITION = true
  ).* from "../nodeclaimLifecycle"

  /// The structural conservation property the KEEP arm protects. Scoped
  /// to the base alphabet: the lease-acquire Ok-arm clear and the
  /// controller restart legitimately empty the map in the fault-lease
  /// regime (the documented one-provisioning-round detection loss).
  val inflightKeptWhileInFlight =
    CLAIMS.forall(k =>
      claims.get(k).phase == CInFlight implies claims.get(k).inflightTracked)

  /// The as-built full tick with ONE change: the inflight prune fires for
  /// every present claim (drop on first sight in live), not only for
  /// registered/terminating ones.
  action tickFullDropOnSightWith(
    pgLoadFails: bool, ackFails: bool, createFails: bool,
    demandA: int, demandB: int, reservedChoice: Set[ClaimId]
  ): bool = all {
      isLeader,
      val aged = CLAIMS.mapBy(k => ageWallclock(claims.get(k)))
      val gateAfterLose = if (losePending) false else placeableArmed
      val loadOk = reloadPending and not(pgLoadFails)
      val afterLatch = CLAIMS.mapBy(k =>
        if (reloadPending)
          { ...aged.get(k), pIdle: INotTracked,
            inflightTracked: if (loadOk) false else aged.get(k).inflightTracked }
        else aged.get(k))
      val reloadStillPending = reloadPending and not(loadOk)
      val acquireWithInflight =
        reloadPending and CLAIMS.filter(k => claims.get(k).inflightTracked).size() > 0
      val observed = CLAIMS.mapBy(k => observeIdleToBusy(afterLatch.get(k)))
      val obsReg = CLAIMS.mapBy(k => observeRegistered(observed.get(k)))
      val afterObs = CLAIMS.mapBy(k => obsReg.get(k)._1)
      val clears: Set[Cell] = CLAIMS.filter(k => obsReg.get(k)._2).map(k => afterObs.get(k).cell)
      val staleRecordOnly = CLAIMS.filter(k => obsReg.get(k)._3).size() > 0
      val lostSamples = CLAIMS.filter(k => obsReg.get(k)._4).size() > 0
      val reserved = reservedChoice.filter(k =>
        afterObs.get(k).phase == CRegistered and afterObs.get(k).spell != SNotIdle)
      val stuckReaped = CLAIMS.filter(k =>
        afterObs.get(k).phase == CInFlight and afterObs.get(k).stuck)
      val stuckMarks: Set[Cell] = stuckReaped.map(k => afterObs.get(k).cell)
      val vanished = CLAIMS.filter(k =>
        afterObs.get(k).inflightTracked and not(stuckReaped.contains(k))
        and afterObs.get(k).phase == CAbsent)
      val vanishMarks: Set[Cell] = vanished.map(k => afterObs.get(k).inflightCell)
      val spurious = vanished.filter(k => afterObs.get(k).ctrlReaped).size() > 0
      val marksThisTick = stuckMarks.union(vanishMarks)
      val masked = iceMasked.union(marksThisTick)
      val freeSlots = CLAIMS.filter(k => afterObs.get(k).phase == CAbsent)
      val liveCores = CLAIMS.filter(k => afterObs.get(k).phase != CAbsent and not(stuckReaped.contains(k))).size()
        + stuckReaped.size()
      val globalRemaining = if (MAX_FLEET_CORES > liveCores) MAX_FLEET_CORES - liveCores else 0
      val cellALive = CLAIMS.filter(k =>
        afterObs.get(k).phase != CAbsent and afterObs.get(k).cell == "cellA").size()
      val classRemainingA = if (CLASS_CAP_A > cellALive) CLASS_CAP_A - cellALive else 0
      val dropA = demandA > 0 and not(configuredCells.contains("cellA"))
      val dropB = demandB > 0 and not(configuredCells.contains("cellB"))
      val coverableA = demandA > 0 and ceilingsKnown and configuredCells.contains("cellA")
        and not(masked.contains("cellA"))
      val coverableB = demandB > 0 and ceilingsKnown and configuredCells.contains("cellB")
        and not(masked.contains("cellB"))
      pure def min2(a: int, b: int): int = if (a < b) a else b
      val wantA = if (coverableA)
          min2(demandA, min2(classRemainingA, globalRemaining))
        else 0
      val wantB = if (coverableB)
          min2(demandB, globalRemaining - wantA)
        else 0
      val createNA = min2(wantA, freeSlots.size())
      val createNB = min2(wantB, freeSlots.size() - createNA)
      val createdA = if (createFails) 0 else createNA
      val createdB = if (createFails) 0 else createNB
      val createAttempted = createNA + createNB > 0
      val slotForA = freeSlots.filter(k => freeSlots.filter(o => claimRank(o) < claimRank(k)).size() < createdA)
      val slotForB = freeSlots.exclude(slotForA).filter(k =>
        freeSlots.exclude(slotForA).filter(o => claimRank(o) < claimRank(k)).size() < createdB)
      val classBound = demandA > 0 and configuredCells.contains("cellA") and ceilingsKnown
        and not(masked.contains("cellA")) and classRemainingA == 0 and globalRemaining > 0
      val ceilingsBlocked = (demandA > 0 or demandB > 0) and not(ceilingsKnown)
      val ackPayload = marksThisTick.size() > 0 or clears.size() > 0
      val ackOk = ackPayload and not(ackFails)
      val iceMaskedAfter = if (ackOk) iceMasked.union(marksThisTick).exclude(clears) else iceMasked
      val clearOfMasked = ackOk and clears.intersect(iceMasked).size() > 0
      val idleReaped = CLAIMS.filter(k =>
        val c = afterObs.get(k)
        c.phase == CRegistered and c.spell != SNotIdle and not(reserved.contains(k))
        and c.pIdle == ITrackedOld)
      val overReap = idleReaped.filter(k => afterObs.get(k).spell != SOld).size() > 0
      val claimsAfter = CLAIMS.mapBy(k =>
        val c = afterObs.get(k)
        if (stuckReaped.contains(k) or idleReaped.contains(k))
          { ...c, phase: CTerminating, stuck: false, spell: SNotIdle,
            busyTickSeen: false, boot: BRecorded, pIdle: INotTracked,
            inflightTracked: false, ctrlReaped: true }
        else if (vanished.contains(k)) { ...c, inflightTracked: false }
        else if (slotForA.contains(k))
          { phase: CInFlight, cell: "cellA", stuck: false, spell: SNotIdle,
            busyTickSeen: false, boot: BRecorded, inflightTracked: true,
            inflightCell: "cellA", pIdle: INotTracked, ctrlReaped: false,
            tomb: false, tombAge: 0, ourCommit: false }
        else if (slotForB.contains(k))
          { phase: CInFlight, cell: "cellB", stuck: false, spell: SNotIdle,
            busyTickSeen: false, boot: BRecorded, inflightTracked: true,
            inflightCell: "cellB", pIdle: INotTracked, ctrlReaped: false,
            tomb: false, tombAge: 0, ourCommit: false }
        // PRE-FIX (08d49c52c bug_020): every entry whose claim appears in
        // the live LIST is pruned on first sight — in-flight ones too.
        else if (c.inflightTracked and c.phase != CAbsent)
          { ...c, inflightTracked: false }
        else c)
      val totalAfter = CLAIMS.filter(k => claimsAfter.get(k).phase != CAbsent).size()
      val cellAAfter = CLAIMS.filter(k =>
        claimsAfter.get(k).phase != CAbsent and claimsAfter.get(k).cell == "cellA").size()
      val vBudgetGlobal = (createdA + createdB > 0) and totalAfter > MAX_FLEET_CORES
      val vBudgetClass = createdA > 0 and cellAAfter > CLASS_CAP_A
      // Round-9 WO-S7-2 mirror: the budget-brake deficit-full-mint law
      // (the retired per-tick cap is gone from relation AND check;
      // post-hoc against the rule, mirroring the base model).
      val vDeficitDeferred =
        (coverableA and not(createFails)
          and createdA < min2(min2(demandA, classRemainingA),
                              min2(globalRemaining, freeSlots.size())))
        or
        (coverableB and not(createFails)
          and createdB < min2(min2(demandB, globalRemaining - createdA),
                              freeSlots.size() - createdA))
      val vNoCeil = (createdA + createdB > 0) and not(ceilingsKnown)
      val vUnconfigured =
        (createdA > 0 and not(configuredCells.contains("cellA")))
        or (createdB > 0 and not(configuredCells.contains("cellB")))
      val vMasked =
        (createdA > 0 and masked.contains("cellA"))
        or (createdB > 0 and masked.contains("cellB"))
      val vGateProd = losePending and not(isLeader)
      all {
        claims' = claimsAfter,
        placeableArmed' = true,
        losePending' = false,
        isLeader' = isLeader,
        reloadPending' = reloadStillPending,
        botStreak' = 0,
        iceMasked' = iceMaskedAfter,
        configuredCells' = configuredCells,
        ceilingsKnown' = ceilingsKnown,
        hadAcquire' = hadAcquire,
        // C2 vars: constant under the calibration regimes (axes off).
        edgeClearDone' = edgeClearDone,
        pendingClear' = pendingClear,
        schedBinding' = schedBinding,
        deferredCells' = deferredCells,
        effects' = Set(EPublished)
          .union(if (createdA + createdB > 0) Set(ECreateOk) else Set())
          .union(if (createAttempted and createFails) Set(ECreateFailed) else Set())
          .union(if (ackOk and marksThisTick.size() > 0) Set(EIceMarkEmitted) else Set())
          .union(if (ackOk and clears.size() > 0) Set(EClearEmitted) else Set())
          .union(if (clearOfMasked) Set(EClearOfMaskedCell) else Set())
          .union(if (vanished.size() > 0) Set(EVanishDetected) else Set())
          .union(if (stuckReaped.size() > 0) Set(EStuckReap) else Set())
          .union(if (idleReaped.size() > 0) Set(EIdleReap) else Set())
          .union(if (classBound) Set(EClassBudgetBound) else Set())
          .union(if (ceilingsBlocked) Set(EFailClosedNoCeilings) else Set())
          .union(if (dropA or dropB) Set(EUnknownCellDrop) else Set())
          .union(if (acquireWithInflight) Set(EAcquireWithInflight) else Set())
          .union(if (reloadStillPending) Set(EDegradedReloadTick) else Set())
          .union(if (staleRecordOnly) Set(EStaleRecordOnly) else Set()),
        violations' = violations
          .union(if (overReap) Set(VIdleOverReap) else Set())
          .union(if (spurious) Set(VSpuriousMark) else Set())
          .union(if (lostSamples) Set(VBootSampleLost) else Set())
          .union(if (vBudgetGlobal) Set(VBudgetGlobal) else Set())
          .union(if (vBudgetClass) Set(VBudgetClass) else Set())
          .union(if (vDeficitDeferred) Set(VDeficitDeferred) else Set())
          .union(if (vNoCeil) Set(VCreateNoCeilings) else Set())
          .union(if (vUnconfigured) Set(VCreateUnconfigured) else Set())
          .union(if (vMasked) Set(VCoverMasked) else Set())
          .union(if (vGateProd) Set(VGateProducer) else Set()),
      }
  }

  action tickFullDropOnSight = {
    nondet pgLoadFails = boolChoices(ENABLE_LEASE_FAULT).oneOf()
    nondet ackFails = boolChoices(ENABLE_RPC_FAULT).oneOf()
    nondet createFails = boolChoices(ENABLE_RPC_FAULT).oneOf()
    nondet demandA = Set(0, 1, 2).oneOf()
    nondet demandB = Set(0, 1, 2).oneOf()
    nondet reservedChoice = CLAIMS.powerset().oneOf()
    tickFullDropOnSightWith(pgLoadFails, ackFails, createFails, demandA, demandB, reservedChoice)
  }

  action calibStep = {
    nondet k = CLAIMS.oneOf()
    nondet cell = CELLS.oneOf()
    any {
      tickFullDropOnSight,
      tickBotEarly,
      tickConsolidateOnly,
      tickStandby,
      claimRegisters(k),
      claimGoesBusy(k),
      claimGoesIdle(k),
      claimLaunchStuck(k),
      karpenterGC(k),
      karpenterTerminationEdge(k),
      finalizerCompletes(k),
      iceClears(cell),
      leaseLose,
      leaseAcquire,
      controllerRestarts,
      ceilingsLoad,
      configDropsCell(cell),
      configRestoresCell(cell),
    }
  }
}
