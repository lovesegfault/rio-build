// -*- mode: Bluespec; -*-
//
// Stage-C calibration overrides, family FFD/cover <-> scheduler-config
// parity. Most of the family is sizing arithmetic and axis-parity work
// that the model abstracts to opaque placed/unplaced outcomes
// (NOT-ENCODED rows in the calibration table); the encodable half is the
// budget/polarity discipline of cover_deficit. The override below is a
// family-level reconstruction (the retry campaign's G8 precedent): it
// anchors on the per-class fleet clamp mechanism (cover.rs::class_budget,
// the ctrl.nodeclaim.budget.per-class rule) rather than on a single
// historical commit, because the clamp arrived as feature work and the
// family's fix commits adjust sizing inside it.
//
// Run (serially):
//   quint verify --backend=tlc --main=ffdCalibNoClassClamp \
//     --step=calibStep --invariant=provisioningBudget \
//     docs/spec/models/calibration/controller-ffd.qnt

// =========================================================================
// CALIBRATION — family-level: cover_deficit sized against the GLOBAL
// fleet budget only (no per-hwClass clamp). The per-class cap exists so
// one class cannot absorb the whole fleet allowance; without it a cell
// with standing demand exceeds its class ceiling as soon as the global
// budget still has room.
// PREDICTED: provisioningBudget falsifies (the per-class half) in the
// base alphabet within two cover ticks.
// =========================================================================
module ffdCalibNoClassClamp {
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
    ENABLE_SNAPSHOT_PRESENCE = false
  ).* from "../nodeclaimLifecycle"

  /// The as-built full tick with ONE change: the cellA cover allowance
  /// ignores the per-class remaining budget (global budget only —
  /// post-live_049 the relation has no per-tick cap to fall back on,
  /// which makes the class-clamp's absence bite even faster).
  action tickFullNoClassClampWith(
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
      // PRE-FIX (family-level): the per-class clamp is gone — cellA is
      // sized against the global budget alone.
      val wantA = if (coverableA)
          min2(demandA, globalRemaining)
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
            inflightCell: "cellA", pIdle: INotTracked, ctrlReaped: false }
        else if (slotForB.contains(k))
          { phase: CInFlight, cell: "cellB", stuck: false, spell: SNotIdle,
            busyTickSeen: false, boot: BRecorded, inflightTracked: true,
            inflightCell: "cellB", pIdle: INotTracked, ctrlReaped: false }
        else if (c.inflightTracked and (c.phase == CRegistered or c.phase == CTerminating))
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

  action tickFullNoClassClamp = {
    nondet pgLoadFails = boolChoices(ENABLE_LEASE_FAULT).oneOf()
    nondet ackFails = boolChoices(ENABLE_RPC_FAULT).oneOf()
    nondet createFails = boolChoices(ENABLE_RPC_FAULT).oneOf()
    nondet demandA = Set(0, 1, 2).oneOf()
    nondet demandB = Set(0, 1, 2).oneOf()
    nondet reservedChoice = CLAIMS.powerset().oneOf()
    tickFullNoClassClampWith(pgLoadFails, ackFails, createFails, demandA, demandB, reservedChoice)
  }

  action calibStep = {
    nondet k = CLAIMS.oneOf()
    nondet cell = CELLS.oneOf()
    any {
      tickFullNoClassClamp,
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
