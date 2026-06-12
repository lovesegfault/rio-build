// -*- mode: Bluespec; -*-
//
// Stage-C calibration overrides, family G-A: spawn<->reap<->queued
// coherence within one tick (controller-invariant-map.md, calibration
// table; controller-formal design §3.4). Each module instantiates the
// as-built spawnCoherence model (Model J), replaces the tick action with
// a local PRE-FIX variant (the behavior the named historical fix
// removed), and exposes it through a `calibStep`. The violation latches
// inside the pre-fix tick keep the AS-BUILT oracle: the behavior vals
// are reverted, the violation vals are not — a falsification therefore
// means the as-built invariant set re-finds the historical bug class.
//
// Run (serially — the bundled Apalache server port is shared):
//   quint verify --backend=tlc --main=<module> --step=calibStep \
//     --invariant=<predicted> docs/spec/models/calibration/controller-ga.qnt

// =========================================================================
// CALIBRATION — pre-fba9086dc: reap_excess_pending deleted on the
// informer-cached census alone (no live pod-phase re-check before the
// DELETE). The informer's Job.status.ready lags the pod by ~tens of
// seconds on cold start, so a Job whose pod had just started running was
// deleted as "excess Pending" when the queue dropped (observed: builder
// SIGTERM 2 s after assignment).
// PREDICTED: reapSafety falsifies (an excess reap deletes a job whose
// live pod phase is Running) in the base alphabet.
// =========================================================================
module gaCalibNoLiveRecheck {
  import spawnCoherence(
    IS_BUILDER = true,
    CRD_PRESENT = true,
    CEILING = 1,
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
    ENABLE_RETRY_BACKOFF = false,
    ENABLE_STREAK_CADENCE = false,
    STREAK_FLOOR_ENFORCED = false,
    STREAK_FLOOR = 2,
    WINDOW_COUPLED_FOLD = false,
    ENABLE_SKIP_SPAWN_FAILOPEN = false,
    SKIP_SPAWN_WITHHOLD = false,
    ENABLE_HELD_MACHINERY = false,
    ENABLE_HELD_VISIBILITY = false
  ).* from "../spawnCoherence"

  /// The as-built tick with ONE change: the live pod-phase re-check is
  /// dropped from the excess-reap arm (excessReaped = excessSelected).
  action tickNoLiveRecheck = {
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

    val gr = gateRetain(CRD_PRESENT, IS_BUILDER, gate, polled, Set())
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
    val toAck = pendingNotReaped.union(spawnedSet)
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
    // PRE-FIX (fba9086dc): no live pod-phase re-check — the selected
    // excess set is deleted as-is, informer lag and all.
    val excessReaped = excessSelected
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
            | GArmed(sf) => spawnedSet.filter(i => not(sf._1.contains(i))).size() > 0
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
      streakWallAge' = streakWallAge,
      ownGatedTicks' = ownGatedTicks,
      jobPostdatesClose' = jobPostdatesClose,
      pendingClose' = pendingClose,
      obsLive' = obsLive,
      inBackoff' = inBackoff,
      skipSpawned' = skipSpawned,
    }
  }

  action calibStep = {
    nondet i = INTENTS.oneOf()
    any {
      tickNoLiveRecheck,
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
// CALIBRATION — pre-6c4f4983d (its C3 half): orphan_reap_gate treated a
// successful-but-EMPTY ListExecutors response as an authoritative "no
// executors exist", so a new scheduler leader (workers not yet
// reconnected) licensed the orphan reap and Running Jobs whose workers
// were merely unregistered were deleted (bug_397). The as-built gate is
// 3-arm fail-closed: error / young leader / EMPTY list => no reap.
// PREDICTED: reapSafety falsifies (an orphan reap fires while the
// as-built gate would not have passed) in the base alphabet — a Running
// Job past the orphan grace whose executor never registered is enough.
// =========================================================================
module gaCalibOrphanGateEmptyOk {
  import spawnCoherence(
    IS_BUILDER = true,
    CRD_PRESENT = true,
    CEILING = 1,
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
    ENABLE_RETRY_BACKOFF = false,
    ENABLE_STREAK_CADENCE = false,
    STREAK_FLOOR_ENFORCED = false,
    STREAK_FLOOR = 2,
    WINDOW_COUPLED_FOLD = false,
    ENABLE_SKIP_SPAWN_FAILOPEN = false,
    SKIP_SPAWN_WITHHOLD = false,
    ENABLE_HELD_MACHINERY = false,
    ENABLE_HELD_VISIBILITY = false
  ).* from "../spawnCoherence"

  /// The as-built tick with ONE change: the orphan-reap decision uses a
  /// PRE-FIX gate without the empty-list arm. The violation oracle keeps
  /// the as-built 3-arm gate.
  action tickOrphanGateEmptyOk = {
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

    val gr = gateRetain(CRD_PRESENT, IS_BUILDER, gate, polled, Set())
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
    val toAck = pendingNotReaped.union(spawnedSet)
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
    // AS-BUILT gate (the violation oracle).
    val orphanGatePasses =
      orphanCandidates.size() > 0
      and not(listExecFails)
      and leaderOld
      and executorsPresent.size() > 0
    // PRE-FIX gate (the reap decision): Ok([]) is authoritative — the
    // empty-list arm is missing.
    val orphanGatePassesPreFix =
      orphanCandidates.size() > 0
      and not(listExecFails)
      and leaderOld
    val orphanReaped = if (orphanGatePassesPreFix)
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

    // ---- AS-BUILT oracle (unchanged; uses the as-built gate) ----------
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
            | GArmed(sf) => spawnedSet.filter(i => not(sf._1.contains(i))).size() > 0
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
      streakWallAge' = streakWallAge,
      ownGatedTicks' = ownGatedTicks,
      jobPostdatesClose' = jobPostdatesClose,
      pendingClose' = pendingClose,
      obsLive' = obsLive,
      inBackoff' = inBackoff,
      skipSpawned' = skipSpawned,
    }
  }

  action calibStep = {
    nondet i = INTENTS.oneOf()
    any {
      tickOrphanGateEmptyOk,
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
// CALIBRATION — pre-8b0128f5a (the 94f3a91c state): when the NodeClaim
// CRD is absent the gate match arm returned `true`, so spawn passed
// through ungated AND `gate_armed=true` made `queued` count as known —
// reap_excess_pending ran against an ungated queue and deleted the
// just-completed Job racing the job-tracking finalizer. The as-built
// 3-arm match keeps spawn pass-through but forces gate_armed=false
// (queued_known=None) so the excess reap is fail-closed.
// PREDICTED: gateFailClosed falsifies (an excess reap fires in the
// crd-absent configuration) in the crd-absent alphabet.
// =========================================================================
module gaCalibCrdAbsentArmed {
  import spawnCoherence(
    IS_BUILDER = true,
    CRD_PRESENT = false,
    CEILING = 1,
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
    ENABLE_RETRY_BACKOFF = false,
    ENABLE_STREAK_CADENCE = false,
    STREAK_FLOOR_ENFORCED = false,
    STREAK_FLOOR = 2,
    WINDOW_COUPLED_FOLD = false,
    ENABLE_SKIP_SPAWN_FAILOPEN = false,
    SKIP_SPAWN_WITHHOLD = false,
    ENABLE_HELD_MACHINERY = false,
    ENABLE_HELD_VISIBILITY = false
  ).* from "../spawnCoherence"

  /// PRE-FIX gate retain: the crd-absent arm arms the gate (pass-through
  /// for spawn AND for the reap's queued_known).
  pure def gateRetainPreFix(
    crdPresent: bool, isBuilder: bool, g: GateState, polled: Set[IntentId]
  ): (bool, Set[IntentId]) =
    if (not(crdPresent))
      (true, polled)
    else if (not(isBuilder))
      (true, polled)
    else
      match g {
        | GUnarmed => (false, Set())
        | GArmed(sf) => (true, polled.intersect(sf._1))
      }

  /// The as-built tick with ONE change: the gate retain is the pre-fix
  /// one, so queued_known is treated as known in the crd-absent
  /// configuration. The gateFailClosed oracle keeps the as-built
  /// crd-absent posture (excess reap must stay fail-closed there).
  action tickCrdAbsentArmed = {
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

    // PRE-FIX (8b0128f5a): crd-absent arms the gate.
    val gr = gateRetainPreFix(CRD_PRESENT, IS_BUILDER, gate, polled)
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
    val toAck = pendingNotReaped.union(spawnedSet)
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

    // ---- AS-BUILT oracle (unchanged; the crd-absent clauses keep the
    // as-built posture: no excess reap, spawn demand unfiltered) --------
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
            | GArmed(sf) => spawnedSet.filter(i => not(sf._1.contains(i))).size() > 0
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
      streakWallAge' = streakWallAge,
      ownGatedTicks' = ownGatedTicks,
      jobPostdatesClose' = jobPostdatesClose,
      pendingClose' = pendingClose,
      obsLive' = obsLive,
      inBackoff' = inBackoff,
      skipSpawned' = skipSpawned,
    }
  }

  action calibStep = {
    nondet i = INTENTS.oneOf()
    any {
      tickCrdAbsentArmed,
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
