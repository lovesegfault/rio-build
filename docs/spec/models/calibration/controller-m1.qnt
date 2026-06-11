// -*- mode: Bluespec; -*-
//
// Stage-C calibration overrides, family M1: the prev_idle mirror and the
// idle model (the AMPLIFY-polarity field of the lease-edge table). Each
// module instantiates the as-built nodeclaimLifecycle model (Model N) and
// swaps one tick action for a local PRE-FIX variant; the violation
// latches keep the as-built oracle.
//
// Run (serially):
//   quint verify --backend=tlc --main=<module> --step=calibStep \
//     --invariant=<predicted> docs/spec/models/calibration/controller-m1.qnt

// =========================================================================
// CALIBRATION — pre-79f86b888: prev_idle.clear() lived inside the
// lease-acquire reload's Ok arm only. When the PG load FAILED, the loop
// fell through to reconcile_once with the previous tenure's prev_idle
// intact, and observe_idle_to_busy's or_insert is a no-op on a present
// key — so reap_idle computed an idle age inflated by the entire standby
// gap and deleted a node that had only just gone idle (the under-reap
// polarity inverted to over-reap).
// PREDICTED: idleReapSafety falsifies in the fault-lease alphabet (an
// acquire whose reload fails, a claim whose old entry survives, a fresh
// idle spell reaped as if it were old).
// =========================================================================
module m1CalibAcquireClearOkOnly {
  import nodeclaimLifecycle(
    MAX_FLEET_CORES = 2,
    CLASS_CAP_A = 1,
    BOT_THRESHOLD = 2,
    ENABLE_RPC_FAULT = false,
    ENABLE_LEASE_FAULT = true,
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

  /// The as-built full tick with ONE change: the reload-latch processing
  /// clears prev_idle only when the PG load succeeded (the pre-fix
  /// Ok-arm-only clear), instead of unconditionally before the attempt.
  action tickFullPreFixWith(
    pgLoadFails: bool, ackFails: bool, createFails: bool,
    demandA: int, demandB: int, reservedChoice: Set[ClaimId]
  ): bool = all {
      isLeader,
      val aged = CLAIMS.mapBy(k => ageWallclock(claims.get(k)))
      val gateAfterLose = if (losePending) false else placeableArmed
      val loadOk = reloadPending and not(pgLoadFails)
      // PRE-FIX (79f86b888): prev_idle survives a failed reload.
      val afterLatch = CLAIMS.mapBy(k =>
        if (reloadPending)
          { ...aged.get(k),
            pIdle: if (loadOk) INotTracked else aged.get(k).pIdle,
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

  action tickFullPreFix = {
    nondet pgLoadFails = boolChoices(ENABLE_LEASE_FAULT).oneOf()
    nondet ackFails = boolChoices(ENABLE_RPC_FAULT).oneOf()
    nondet createFails = boolChoices(ENABLE_RPC_FAULT).oneOf()
    nondet demandA = Set(0, 1, 2).oneOf()
    nondet demandB = Set(0, 1, 2).oneOf()
    nondet reservedChoice = CLAIMS.powerset().oneOf()
    tickFullPreFixWith(pgLoadFails, ackFails, createFails, demandA, demandB, reservedChoice)
  }

  action calibStep = {
    nondet k = CLAIMS.oneOf()
    nondet cell = CELLS.oneOf()
    any {
      tickFullPreFix,
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

  /// Deterministic reproducer: the documented incident shape — a tracked
  /// idle entry survives a failed-reload acquire and reaps the fresh idle
  /// spell that follows a standby-window busy period.
  run m1AcquireClearOkOnlyRun =
    init
      .then(tickFullPreFixWith(false, false, false, 1, 0, Set()))
      .then(claimRegisters("c1"))
      .then(tickFullPreFixWith(false, false, false, 0, 0, Set()))
      .then(leaseLose)
      .then(claimGoesBusy("c1"))
      .then(tickStandby)
      .then(claimGoesIdle("c1"))
      .then(leaseAcquire)
      .then(tickFullPreFixWith(true, false, false, 0, 0, Set()))
      .expect(violations.contains(VIdleOverReap))
}

// =========================================================================
// CALIBRATION PROBE — 13806e99a (the reap_idle busy-guard half). The fix
// added a requested>0 guard directly to reap_idle, defending against a
// prev_idle entry that survives into a tick where the node is busy. At
// the model's resolution that state is unreachable: observe_idle_to_busy
// runs earlier in the SAME tick and prunes the entry of every busy claim,
// so a reap candidate with a live entry is never busy. This probe drops
// the busy-guard from the reap predicate and exhaustively re-checks
// idleReapSafety over the base alphabet.
// PREDICTED: HOLDS — machine-checked evidence that the guard is
// redundant at the model's tick-internal ordering resolution (NOT a
// deletion license: the real loop is not atomic between the observation
// and the reap, and the guard is the defense for exactly that window).
// =========================================================================
module m1CalibReapBusyGuardProbe {
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

  /// The as-built full tick with ONE change: reap_idle no longer requires
  /// the claim to be idle NOW (the busy-guard) — it reaps on the tracked
  /// entry alone.
  action tickFullNoBusyGuardWith(
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
      // PRE-FIX (13806e99a): the reap predicate drops the idle-now
      // busy-guard.
      val idleReaped = CLAIMS.filter(k =>
        val c = afterObs.get(k)
        c.phase == CRegistered and not(reserved.contains(k))
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

  action tickFullNoBusyGuard = {
    nondet pgLoadFails = boolChoices(ENABLE_LEASE_FAULT).oneOf()
    nondet ackFails = boolChoices(ENABLE_RPC_FAULT).oneOf()
    nondet createFails = boolChoices(ENABLE_RPC_FAULT).oneOf()
    nondet demandA = Set(0, 1, 2).oneOf()
    nondet demandB = Set(0, 1, 2).oneOf()
    nondet reservedChoice = CLAIMS.powerset().oneOf()
    tickFullNoBusyGuardWith(pgLoadFails, ackFails, createFails, demandA, demandB, reservedChoice)
  }

  action calibStep = {
    nondet k = CLAIMS.oneOf()
    nondet cell = CELLS.oneOf()
    any {
      tickFullNoBusyGuard,
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
