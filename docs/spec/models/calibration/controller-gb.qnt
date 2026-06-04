// -*- mode: Bluespec; -*-
//
// Stage-C calibration overrides, family G-B: the ack/ICE protocol with
// the scheduler (false arming/clearing of scheduler-side peer state).
// Same extend-and-override pattern as controller-ga.qnt: the behavior
// vals are reverted to the pre-fix shape, the violation oracle stays
// as-built.
//
// Run (serially):
//   quint verify --backend=tlc --main=<module> --step=calibStep \
//     --invariant=<predicted> docs/spec/models/calibration/controller-gb.qnt

// =========================================================================
// CALIBRATION — pre-cdc78f839: AckSpawnedIntents{spawned} chained the
// ATTEMPTED spawn slice, not the successfully-created one, so a failed
// create (and a 409 collision against a terminating Job) was still acked
// — the scheduler armed dispatched_cells for an intent with no Job
// behind it (the false-ICE-arm half of the commit).
// PREDICTED: ackSoundness falsifies (an acked intent has no Pending Job
// at ack time) in the fault-rpc alphabet, where a Job create can fail.
// =========================================================================
module gbCalibAckAttempted {
  import spawnCoherence(
    IS_BUILDER = true,
    CRD_PRESENT = true,
    CEILING = 1,
    ENABLE_RPC_FAULT = true,
    ENABLE_LEASE_FAULT = false,
    ENABLE_STALE_POLL = false,
    ENABLE_MAT_JOBS = false,
    ENABLE_RENDERED_DEADLINE = false,
    ENABLE_EXHAUST_POISON = false,
    ENABLE_EXHAUST_PERSIST = false,
    ENABLE_CANCEL_ASBUILT = false,
    ENABLE_CANCEL_CAUSE = false,
    ENABLE_YOUNG_LEADER_REAP = false,
    ENABLE_SYNTH_CLOSE = false,
    ENABLE_EXEC_PINNED = false,
    ENABLE_MULTI_POOL = false,
    ENABLE_STREAK_POOL_KEY = false,
    ENABLE_CANCEL_GENERATION = false,
    ENABLE_RETRY_BACKOFF = false
  ).* from "../spawnCoherence"

  /// The as-built tick with ONE change: the ack covers the attempted
  /// spawn slice (toSpawn) instead of the successfully-created set.
  action tickAckAttempted = {
    nondet pollFails = boolChoices(ENABLE_RPC_FAULT).oneOf()
    nondet listExecFails = boolChoices(ENABLE_RPC_FAULT).oneOf()
    nondet ackFails = boolChoices(ENABLE_RPC_FAULT).oneOf()
    nondet createFailsFor = (if (ENABLE_RPC_FAULT) INTENTS.powerset() else Set(Set())).oneOf()
    nondet useStalePoll = boolChoices(ENABLE_STALE_POLL).oneOf()

    val currentQueue = INTENTS.filter(i => queued.get(i))
    val polled = if (pollFails) Set()
                 else if (useStalePoll) prevQueuedSnap
                 else currentQueue
    val schedErr = pollFails

    val gr = gateRetain(CRD_PRESENT, IS_BUILDER, gate, polled)
    val gateArmed = gr._1
    val postGate = gr._2
    val q = postGate.size()

    val jobsAtList = job
    val activeAtList = INTENTS.filter(i => censusActive(jobsAtList.get(i))).size()

    val wantEmpty = postGate.size() == 0
    val orphanPendingReaps = if (wantEmpty) Set() else
      INTENTS.filter(i =>
        not(postGate.contains(i))
        and isPending(jobsAtList.get(i))
        and pastPendingGrace(jobsAtList.get(i)))
    val terminalReaps = if (wantEmpty) Set() else
      postGate.filter(i => jobsAtList.get(i).phase == JTerminal)
    val driftReaps = if (wantEmpty) Set() else
      postGate.filter(i =>
        isPending(jobsAtList.get(i)) and jobsAtList.get(i).fp != intentFp.get(i))
    val reaped = orphanPendingReaps.union(terminalReaps).union(driftReaps)

    val freed = reaped.filter(i => censusActive(jobsAtList.get(i))).size()
    val headroom0 = CEILING - (activeAtList - freed)
    val headroom = if (headroom0 < 0) 0 else headroom0
    val existingNames = INTENTS.filter(i => jobsAtList.get(i).phase != JAbsent).exclude(reaped)

    val spawnCandidates = postGate.filter(i => not(existingNames.contains(i)))
    val toSpawn = spawnCandidates.filter(i =>
      spawnCandidates.filter(o => idRank(o) < idRank(i)).size() < headroom)
    val jobsAfterReap = INTENTS.mapBy(i =>
      if (terminalReaps.contains(i)) { phase: JAbsent, age: AgeFresh, fp: FpA, podRunning: false }
      else if (reaped.contains(i)) { ...jobsAtList.get(i), phase: JTerminating }
      else jobsAtList.get(i))
    val spawnedSet = toSpawn.filter(i =>
      jobsAfterReap.get(i).phase == JAbsent and not(createFailsFor.contains(i)))
    val collided = toSpawn.filter(i => jobsAfterReap.get(i).phase != JAbsent)
    val createFailed = toSpawn.filter(i =>
      jobsAfterReap.get(i).phase == JAbsent and createFailsFor.contains(i))

    val pendingNotReaped = postGate.filter(i =>
      isPending(jobsAtList.get(i)) and not(reaped.contains(i)))
    // PRE-FIX (cdc78f839): the attempted slice is acked, success or not.
    val toAck = pendingNotReaped.union(toSpawn)
    val ackHappens = toAck.size() > 0 and not(ackFails)

    val queuedKnown = not(schedErr) and gateArmed
    val excessCandidates = INTENTS.filter(i =>
      isPending(jobsAtList.get(i))
      and pastPendingGrace(jobsAtList.get(i))
      and not(reaped.contains(i)))
    val excessOver = excessCandidates.size() - q
    val excessSelected = if (queuedKnown and excessOver > 0)
        oldestN(excessCandidates, jobsAtList, excessOver)
      else Set()
    val excessReaped = excessSelected.filter(i => not(jobsAtList.get(i).podRunning))
    val excessSuppressed = not(queuedKnown) and excessCandidates.size() > q

    val orphanCandidates = INTENTS.filter(i =>
      isRunning(jobsAtList.get(i))
      and pastOrphanGrace(jobsAtList.get(i))
      and not(reaped.contains(i)))
    val executorsPresent = INTENTS.filter(i => execr.get(i) != ExecAbsent)
    val orphanGatePasses =
      orphanCandidates.size() > 0
      and not(listExecFails)
      and leaderOld
      and executorsPresent.size() > 0
    val orphanReaped = if (orphanGatePasses)
        orphanCandidates.filter(i => execr.get(i) != ExecBusy)
      else Set()

    val jobsAfter = INTENTS.mapBy(i =>
      if (spawnedSet.contains(i))
        { phase: JPending, age: AgeFresh, fp: intentFp.get(i), podRunning: false }
      else if (excessReaped.contains(i) or orphanReaped.contains(i))
        { ...jobsAfterReap.get(i), phase: JTerminating }
      else jobsAfterReap.get(i))

    val gateBlocked =
      CRD_PRESENT and IS_BUILDER and gate == GUnarmed
      and polled.filter(i => jobsAtList.get(i).phase == JAbsent).size() > 0
      and activeAtList < CEILING
    val effectsNow =
      Set()
        .union(if (spawnedSet.size() > 0) Set(ESpawned) else Set())
        .union(if (collided.size() > 0) Set(EDedupe409) else Set())
        .union(if (createFailed.size() > 0) Set(ECreateFailed) else Set())
        .union(if (excessReaped.size() > 0) Set(EExcessReap) else Set())
        .union(if (orphanReaped.size() > 0) Set(EOrphanReap) else Set())
        .union(if (driftReaps.size() > 0) Set(EDriftReap) else Set())
        .union(if (orphanPendingReaps.size() > 0) Set(EOrphanPendingReap) else Set())
        .union(if (terminalReaps.size() > 0) Set(ETerminalReap) else Set())
        .union(if (gateBlocked) Set(EGateBlockedSpawn) else Set())
        .union(if (ackHappens) Set(EAcked) else Set())
        .union(if (excessSuppressed) Set(EExcessSuppressed) else Set())

    // ---- AS-BUILT oracle (unchanged) ---------------------------------
    val vCeil = spawnedSet.size() > 0 and (activeAtList - freed + spawnedSet.size()) > CEILING
    val vBusy =
      excessReaped.filter(i => jobsAtList.get(i).podRunning).size() > 0
      or orphanReaped.filter(i => execr.get(i) == ExecBusy).size() > 0
      or (orphanReaped.size() > 0 and not(orphanGatePasses))
    val orphanPendingDue = if (schedErr or postGate.size() == 0) Set() else
      INTENTS.filter(i =>
        not(postGate.contains(i))
        and isPending(jobsAtList.get(i))
        and pastPendingGrace(jobsAtList.get(i)))
    val orphanRunningDue = if (not(orphanGatePasses)) Set() else
      orphanCandidates.filter(i => execr.get(i) != ExecBusy)
    val vOrphan =
      orphanPendingDue.union(orphanRunningDue).filter(i =>
        jobsAfter.get(i).phase != JTerminating and jobsAfter.get(i).phase != JAbsent
      ).size() > 0
    val vAckNoJob = ackHappens and toAck.filter(i => not(isPending(jobsAfter.get(i)))).size() > 0
    val vAckMissed = ackHappens and pendingNotReaped.filter(i => not(toAck.contains(i))).size() > 0
    val vDegr =
      (schedErr and (
        spawnedSet.size() > 0 or reaped.size() > 0 or excessReaped.size() > 0
        or ackHappens))
      or (listExecFails and orphanReaped.size() > 0)
      or (ackFails and ackHappens)
    val builderGated = CRD_PRESENT and IS_BUILDER
    val vGateV =
      (builderGated and gate == GUnarmed
        and (spawnedSet.size() > 0 or excessReaped.size() > 0))
      or (builderGated and (match gate {
            | GUnarmed => false
            | GArmed(s) => spawnedSet.filter(i => not(s.contains(i))).size() > 0
          }))
      or (not(CRD_PRESENT) and excessReaped.size() > 0)
      or (not(CRD_PRESENT) and postGate != polled)
    val vFreed =
      driftReaps.filter(i =>
        postGate.contains(i)
        and spawnCandidates.exclude(Set(i)).size() == 0
        and headroom >= 1
        and not(toSpawn.contains(i))
      ).size() > 0
    val newViolations =
      Set()
        .union(if (vCeil) Set(VCeiling) else Set())
        .union(if (vBusy) Set(VReapedBusy) else Set())
        .union(if (vOrphan) Set(VOrphanMissed) else Set())
        .union(if (vAckNoJob) Set(VAckedWithoutJob) else Set())
        .union(if (vAckMissed) Set(VAckMissedPending) else Set())
        .union(if (vDegr) Set(VDegraded) else Set())
        .union(if (vGateV) Set(VGate) else Set())
        .union(if (vFreed) Set(VFreedSlotUnused) else Set())

    all {
      job' = jobsAfter,
      dispatched' = if (ackHappens) dispatched.union(toAck) else dispatched,
      effects' = effectsNow,
      violations' = violations.union(newViolations),
      prevQueuedSnap' = if (ENABLE_STALE_POLL) currentQueue else Set(),
      queued' = queued,
      intentFp' = intentFp,
      execr' = execr,
      leaderOld' = leaderOld,
      gate' = gate,
      matJob' = matJob,
      renderedFarDl' = renderedFarDl,
      // C2 vars: constant under the calibration regimes (axes off).
      closedCause' = closedCause,
      seenOpen' = seenOpen,
      exhausted' = exhausted,
      streak' = streak,
      ownGatedTicks' = ownGatedTicks,
      jobPostdatesClose' = jobPostdatesClose,
      pendingClose' = pendingClose,
      obsLive' = obsLive,
      inBackoff' = inBackoff,
    }
  }

  action calibStep = {
    nondet i = INTENTS.oneOf()
    any {
      tickAckAttempted,
      intentArrives(i),
      intentLeaves(i),
      intentResolves(i),
      podStartsLagged(i),
      jobControllerSyncs(i),
      jobCompletes(i),
      jobFailsAtStart(i),
      ttlDeletes(i),
      finalizerCompletes(i),
      jobAges(i),
      execRegisters(i),
      execGetsWork(i),
      execGoesIdle(i),
      execVanishes(i),
      leaderAges,
      schedulerRestarts,
      dispatchedCleared(i),
      gatePublishes,
      gateUnarms,
    }
  }
}

// =========================================================================
// CALIBRATION — pre-5815a7544: the ack carried only the intents spawned
// THIS tick; an already-Pending Job (spawned on an earlier tick) was
// never re-acked, so a restarted scheduler (whose in-memory
// dispatched_cells emptied) was never re-armed for it — the heartbeat
// ICE-clear fallback never fired for pre-restart Pending Jobs.
// PREDICTED: ackCoversPending falsifies (a surviving gated Pending Job is
// left out of a successful ack) — needs CEILING = 2 so a new spawn and a
// surviving Pending Job can coexist in one tick. The as-built step over
// the same constants is the distinguishing baseline (run without --step)
// and must HOLD ackCoversPending.
// =========================================================================
module gbCalibAckOnlyNew {
  import spawnCoherence(
    IS_BUILDER = true,
    CRD_PRESENT = true,
    CEILING = 2,
    ENABLE_RPC_FAULT = false,
    ENABLE_LEASE_FAULT = false,
    ENABLE_STALE_POLL = false,
    ENABLE_MAT_JOBS = false,
    ENABLE_RENDERED_DEADLINE = false,
    ENABLE_EXHAUST_POISON = false,
    ENABLE_EXHAUST_PERSIST = false,
    ENABLE_CANCEL_ASBUILT = false,
    ENABLE_CANCEL_CAUSE = false,
    ENABLE_YOUNG_LEADER_REAP = false,
    ENABLE_SYNTH_CLOSE = false,
    ENABLE_EXEC_PINNED = false,
    ENABLE_MULTI_POOL = false,
    ENABLE_STREAK_POOL_KEY = false,
    ENABLE_CANCEL_GENERATION = false,
    ENABLE_RETRY_BACKOFF = false
  ).* from "../spawnCoherence"

  /// The as-built tick with ONE change: only the spawned-this-tick set is
  /// acked (the already-Pending re-ack is missing).
  action tickAckOnlyNew = {
    nondet pollFails = boolChoices(ENABLE_RPC_FAULT).oneOf()
    nondet listExecFails = boolChoices(ENABLE_RPC_FAULT).oneOf()
    nondet ackFails = boolChoices(ENABLE_RPC_FAULT).oneOf()
    nondet createFailsFor = (if (ENABLE_RPC_FAULT) INTENTS.powerset() else Set(Set())).oneOf()
    nondet useStalePoll = boolChoices(ENABLE_STALE_POLL).oneOf()

    val currentQueue = INTENTS.filter(i => queued.get(i))
    val polled = if (pollFails) Set()
                 else if (useStalePoll) prevQueuedSnap
                 else currentQueue
    val schedErr = pollFails

    val gr = gateRetain(CRD_PRESENT, IS_BUILDER, gate, polled)
    val gateArmed = gr._1
    val postGate = gr._2
    val q = postGate.size()

    val jobsAtList = job
    val activeAtList = INTENTS.filter(i => censusActive(jobsAtList.get(i))).size()

    val wantEmpty = postGate.size() == 0
    val orphanPendingReaps = if (wantEmpty) Set() else
      INTENTS.filter(i =>
        not(postGate.contains(i))
        and isPending(jobsAtList.get(i))
        and pastPendingGrace(jobsAtList.get(i)))
    val terminalReaps = if (wantEmpty) Set() else
      postGate.filter(i => jobsAtList.get(i).phase == JTerminal)
    val driftReaps = if (wantEmpty) Set() else
      postGate.filter(i =>
        isPending(jobsAtList.get(i)) and jobsAtList.get(i).fp != intentFp.get(i))
    val reaped = orphanPendingReaps.union(terminalReaps).union(driftReaps)

    val freed = reaped.filter(i => censusActive(jobsAtList.get(i))).size()
    val headroom0 = CEILING - (activeAtList - freed)
    val headroom = if (headroom0 < 0) 0 else headroom0
    val existingNames = INTENTS.filter(i => jobsAtList.get(i).phase != JAbsent).exclude(reaped)

    val spawnCandidates = postGate.filter(i => not(existingNames.contains(i)))
    val toSpawn = spawnCandidates.filter(i =>
      spawnCandidates.filter(o => idRank(o) < idRank(i)).size() < headroom)
    val jobsAfterReap = INTENTS.mapBy(i =>
      if (terminalReaps.contains(i)) { phase: JAbsent, age: AgeFresh, fp: FpA, podRunning: false }
      else if (reaped.contains(i)) { ...jobsAtList.get(i), phase: JTerminating }
      else jobsAtList.get(i))
    val spawnedSet = toSpawn.filter(i =>
      jobsAfterReap.get(i).phase == JAbsent and not(createFailsFor.contains(i)))
    val collided = toSpawn.filter(i => jobsAfterReap.get(i).phase != JAbsent)
    val createFailed = toSpawn.filter(i =>
      jobsAfterReap.get(i).phase == JAbsent and createFailsFor.contains(i))

    val pendingNotReaped = postGate.filter(i =>
      isPending(jobsAtList.get(i)) and not(reaped.contains(i)))
    // PRE-FIX (5815a7544): only the newly-spawned intents are acked.
    val toAck = spawnedSet
    val ackHappens = toAck.size() > 0 and not(ackFails)

    val queuedKnown = not(schedErr) and gateArmed
    val excessCandidates = INTENTS.filter(i =>
      isPending(jobsAtList.get(i))
      and pastPendingGrace(jobsAtList.get(i))
      and not(reaped.contains(i)))
    val excessOver = excessCandidates.size() - q
    val excessSelected = if (queuedKnown and excessOver > 0)
        oldestN(excessCandidates, jobsAtList, excessOver)
      else Set()
    val excessReaped = excessSelected.filter(i => not(jobsAtList.get(i).podRunning))
    val excessSuppressed = not(queuedKnown) and excessCandidates.size() > q

    val orphanCandidates = INTENTS.filter(i =>
      isRunning(jobsAtList.get(i))
      and pastOrphanGrace(jobsAtList.get(i))
      and not(reaped.contains(i)))
    val executorsPresent = INTENTS.filter(i => execr.get(i) != ExecAbsent)
    val orphanGatePasses =
      orphanCandidates.size() > 0
      and not(listExecFails)
      and leaderOld
      and executorsPresent.size() > 0
    val orphanReaped = if (orphanGatePasses)
        orphanCandidates.filter(i => execr.get(i) != ExecBusy)
      else Set()

    val jobsAfter = INTENTS.mapBy(i =>
      if (spawnedSet.contains(i))
        { phase: JPending, age: AgeFresh, fp: intentFp.get(i), podRunning: false }
      else if (excessReaped.contains(i) or orphanReaped.contains(i))
        { ...jobsAfterReap.get(i), phase: JTerminating }
      else jobsAfterReap.get(i))

    val gateBlocked =
      CRD_PRESENT and IS_BUILDER and gate == GUnarmed
      and polled.filter(i => jobsAtList.get(i).phase == JAbsent).size() > 0
      and activeAtList < CEILING
    val effectsNow =
      Set()
        .union(if (spawnedSet.size() > 0) Set(ESpawned) else Set())
        .union(if (collided.size() > 0) Set(EDedupe409) else Set())
        .union(if (createFailed.size() > 0) Set(ECreateFailed) else Set())
        .union(if (excessReaped.size() > 0) Set(EExcessReap) else Set())
        .union(if (orphanReaped.size() > 0) Set(EOrphanReap) else Set())
        .union(if (driftReaps.size() > 0) Set(EDriftReap) else Set())
        .union(if (orphanPendingReaps.size() > 0) Set(EOrphanPendingReap) else Set())
        .union(if (terminalReaps.size() > 0) Set(ETerminalReap) else Set())
        .union(if (gateBlocked) Set(EGateBlockedSpawn) else Set())
        .union(if (ackHappens) Set(EAcked) else Set())
        .union(if (excessSuppressed) Set(EExcessSuppressed) else Set())

    // ---- AS-BUILT oracle (unchanged) ---------------------------------
    val vCeil = spawnedSet.size() > 0 and (activeAtList - freed + spawnedSet.size()) > CEILING
    val vBusy =
      excessReaped.filter(i => jobsAtList.get(i).podRunning).size() > 0
      or orphanReaped.filter(i => execr.get(i) == ExecBusy).size() > 0
      or (orphanReaped.size() > 0 and not(orphanGatePasses))
    val orphanPendingDue = if (schedErr or postGate.size() == 0) Set() else
      INTENTS.filter(i =>
        not(postGate.contains(i))
        and isPending(jobsAtList.get(i))
        and pastPendingGrace(jobsAtList.get(i)))
    val orphanRunningDue = if (not(orphanGatePasses)) Set() else
      orphanCandidates.filter(i => execr.get(i) != ExecBusy)
    val vOrphan =
      orphanPendingDue.union(orphanRunningDue).filter(i =>
        jobsAfter.get(i).phase != JTerminating and jobsAfter.get(i).phase != JAbsent
      ).size() > 0
    val vAckNoJob = ackHappens and toAck.filter(i => not(isPending(jobsAfter.get(i)))).size() > 0
    val vAckMissed = ackHappens and pendingNotReaped.filter(i => not(toAck.contains(i))).size() > 0
    val vDegr =
      (schedErr and (
        spawnedSet.size() > 0 or reaped.size() > 0 or excessReaped.size() > 0
        or ackHappens))
      or (listExecFails and orphanReaped.size() > 0)
      or (ackFails and ackHappens)
    val builderGated = CRD_PRESENT and IS_BUILDER
    val vGateV =
      (builderGated and gate == GUnarmed
        and (spawnedSet.size() > 0 or excessReaped.size() > 0))
      or (builderGated and (match gate {
            | GUnarmed => false
            | GArmed(s) => spawnedSet.filter(i => not(s.contains(i))).size() > 0
          }))
      or (not(CRD_PRESENT) and excessReaped.size() > 0)
      or (not(CRD_PRESENT) and postGate != polled)
    val vFreed =
      driftReaps.filter(i =>
        postGate.contains(i)
        and spawnCandidates.exclude(Set(i)).size() == 0
        and headroom >= 1
        and not(toSpawn.contains(i))
      ).size() > 0
    val newViolations =
      Set()
        .union(if (vCeil) Set(VCeiling) else Set())
        .union(if (vBusy) Set(VReapedBusy) else Set())
        .union(if (vOrphan) Set(VOrphanMissed) else Set())
        .union(if (vAckNoJob) Set(VAckedWithoutJob) else Set())
        .union(if (vAckMissed) Set(VAckMissedPending) else Set())
        .union(if (vDegr) Set(VDegraded) else Set())
        .union(if (vGateV) Set(VGate) else Set())
        .union(if (vFreed) Set(VFreedSlotUnused) else Set())

    all {
      job' = jobsAfter,
      dispatched' = if (ackHappens) dispatched.union(toAck) else dispatched,
      effects' = effectsNow,
      violations' = violations.union(newViolations),
      prevQueuedSnap' = if (ENABLE_STALE_POLL) currentQueue else Set(),
      queued' = queued,
      intentFp' = intentFp,
      execr' = execr,
      leaderOld' = leaderOld,
      gate' = gate,
      matJob' = matJob,
      renderedFarDl' = renderedFarDl,
      // C2 vars: constant under the calibration regimes (axes off).
      closedCause' = closedCause,
      seenOpen' = seenOpen,
      exhausted' = exhausted,
      streak' = streak,
      ownGatedTicks' = ownGatedTicks,
      jobPostdatesClose' = jobPostdatesClose,
      pendingClose' = pendingClose,
      obsLive' = obsLive,
      inBackoff' = inBackoff,
    }
  }

  action calibStep = {
    nondet i = INTENTS.oneOf()
    any {
      tickAckOnlyNew,
      intentArrives(i),
      intentLeaves(i),
      intentResolves(i),
      podStartsLagged(i),
      jobControllerSyncs(i),
      jobCompletes(i),
      jobFailsAtStart(i),
      ttlDeletes(i),
      finalizerCompletes(i),
      jobAges(i),
      execRegisters(i),
      execGetsWork(i),
      execGoesIdle(i),
      execVanishes(i),
      leaderAges,
      schedulerRestarts,
      dispatchedCleared(i),
      gatePublishes,
      gateUnarms,
    }
  }
}
