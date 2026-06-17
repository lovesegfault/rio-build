// -*- mode: Bluespec; -*-
//
// Stage-C calibration overrides, family G-G: reap delete-propagation and
// report-path mechanics in the pool/job.rs lineage. Most of the family is
// NOT-ENCODED (the foreground-delete / job-tracking-finalizer race sits
// below the model's Job-deletion atomicity); the one encodable half is the
// census-predicate filter below.
//
// Run (serially):
//   quint verify --backend=tlc --main=ggCalibTerminatingPending \
//     --step=calibStep --invariant=ackSoundness \
//     docs/spec/models/calibration/controller-gg.qnt

// =========================================================================
// CALIBRATION — pre-1779975f6 (the is_pending_job half): the pending
// predicate did not filter Jobs with a deletionTimestamp, so a Job being
// foreground-deleted still counted as Pending everywhere the census is
// consumed. On today's protocol the sharpest consequence is in the ack
// chain: a gated intent whose only Job is Terminating is "already
// Pending" to the pre-fix predicate and gets (re-)acked — arming
// dispatched_cells with no live Job behind it. (The commit's foreground-
// propagation half stays NOT-ENCODED — below the model's deletion
// atomicity.)
// PREDICTED: ackSoundness falsifies in the base alphabet (a drift-reaped
// Job left Terminating, its intent re-gated, the next tick re-acks it).
// =========================================================================
module ggCalibTerminatingPending {
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
    ENABLE_HELD_VISIBILITY = false,
    ENABLE_UNREPORTED_DEATH = false,
    ENABLE_TERMINAL_ABSENT_REAP = false
  ).* from "../spawnCoherence"

  /// PRE-FIX pending predicate: no deletionTimestamp filter.
  pure def isPendingPreFix(j: JobRec): bool =
    j.phase == JPending or j.phase == JTerminating

  /// The as-built tick with the pending-classification sites switched to
  /// the pre-fix predicate (stale-reap arms, the excess candidate set,
  /// the already-Pending ack set). The violation oracle keeps the
  /// as-built predicate.
  action tickTerminatingPending = {
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
    // PRE-FIX: the stale-reap arms classify Terminating as Pending.
    val orphanPendingReaps = if (wantEmpty) Set() else
      INTENTS.filter(i =>
        not(postGate.contains(i))
        and isPendingPreFix(jobsAtList.get(i))
        and pastPendingGrace(jobsAtList.get(i)))
    val terminalReaps = if (wantEmpty) Set() else
      postGate.filter(i => jobsAtList.get(i).phase == JTerminal)
    val driftReaps = if (wantEmpty) Set() else
      postGate.filter(i =>
        isPendingPreFix(jobsAtList.get(i)) and jobsAtList.get(i).fp != intentFp.get(i))
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

    // PRE-FIX: a Terminating Job still reads as "already Pending" to the
    // ack chain.
    val pendingNotReaped = postGate.filter(i =>
      isPendingPreFix(jobsAtList.get(i)) and not(reaped.contains(i)))
    val toAck = pendingNotReaped.union(spawnedSet)
    val ackHappens = toAck.size() > 0 and not(ackFails)

    val queuedKnown = not(schedErr) and gateArmed
    // PRE-FIX: the excess candidate set counts Terminating as Pending.
    val excessCandidates = INTENTS.filter(i =>
      isPendingPreFix(jobsAtList.get(i))
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

    // ---- AS-BUILT oracle (unchanged: as-built isPending) --------------
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
      tickTerminatingPending,
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
