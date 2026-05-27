// Model J of the controller-formal campaign (Phase 0, Stage B): the L1
// Pool reconciler's same-tick coherence protocol — the tick body of
// rio-controller/src/reconcilers/pool/jobs.rs::reconcile plus the
// pool/job.rs helpers it calls (job_census, reap_stale_for_intents,
// reap_excess_pending, reap_orphan_running, orphan_reap_gate,
// spawn_for_each/try_spawn_job, the AckSpawnedIntents{spawned} ack).
//
// WHAT IS MODELED
//   One pool, two intents, one tick per `tick` action. The environment
//   owns everything between ticks: the scheduler's Ready-intent set and
//   per-intent selector fingerprint, the apiserver Job set (deterministic
//   name = f(intent), so the model keys Jobs by intent id), the live pod
//   phase vs the informer-lagged Job.status.ready the census reads, the
//   scheduler's executor map and leader age, the placeable gate published
//   by the L10 reconciler (Model N), and the scheduler-side
//   dispatched_cells entries the ack arms.
//
// THE TICK (mirrors the code's order — ctrl.pool.tick-ordering):
//   1. GetSpawnIntents poll (failure => intents=[], scheduler_err set)
//   2. placeable-gate retain (3-valued: crd-absent / present-unarmed /
//      armed) x pool kind                          [jobs.rs gate match]
//   3. Job LIST + census (after the poll — I-183)  [job_census]
//   4. reap_stale_for_intents: orphan-pending / terminal-collision /
//      selector-drift arms; no-op when the gated intent set is empty
//   5. headroom = ceiling - (active - freed), reaped names excluded
//      from the existing-name skip set             [JobCensus::headroom]
//   6. spawn_for_each / try_spawn_job (409 = dedupe, never acked;
//      a failed create is never acked)             [SpawnOutcome]
//   7. AckSpawnedIntents{spawned}: spawned-this-tick ∪ already-Pending
//      (minus reaped) — arms dispatched_cells      [ack chain]
//   8. reap_excess_pending: skipped when queued_known = None
//      (scheduler error or gate not armed); live pod-phase re-check
//      before every delete; foreground delete
//   9. reap_orphan_running behind the 3-arm orphan_reap_gate
//      (RPC error / leader younger than grace / empty list => no reap)
//   MintExecutorTokens, HwClassSampled, termination reports and the
//   status patch are out of model (no protocol state).
//
//   Busy-view note (executor Phase-1a, the §4.5 option-(b) bridge): as
//   built, step 9's busy input is now the OR of two carriers — the
//   stream executors map (ListExecutors.busy, as before) and the
//   durable open pull-mode attempt view (ListOpenAttempts), with the
//   same fail-closed posture on either read failing. The model's
//   single executor-map busy flag abstracts "busy in the scheduler's
//   view" and therefore stands for the OR of both carriers; no
//   transition changes. The busy-but-never-registered residual noted
//   under the weakened-test procedure below is, for pull-mode pods,
//   closed in the code by the durable view; it remains the documented
//   model bound until the 1c' re-target prices the pull path
//   explicitly.
//
// PER-READ SNAPSHOT ABSTRACTION (the load-bearing abstraction)
//   A tick is one transition; the I-183 hazard family is the environment
//   moving between two reads inside one tick. Instead of modeling every
//   await, the reads are snapshot-related:
//   - The Job LIST and the executor list read the tick-start state (the
//     environment only acts between ticks), so LIST is never older than
//     the poll — the monotonicity ctrl.pool.tick-ordering requires.
//   - The informer lag between the live pod phase and Job.status.ready
//     is modeled explicitly (podRunning vs JPending), so the census can
//     be stale relative to the pod while the live re-check is not.
//   - The intent poll may additionally read the PREVIOUS tick's queue
//     snapshot (fault-stale regime, ENABLE_STALE_POLL) — the one
//     staleness direction the channel allows (the poll is the earliest
//     read of the tick).
//   Fallback if Stage C cannot reproduce a G-A representative under this
//   abstraction: split the tick into read/effect phases (pre-registered
//   in the design, not done speculatively).
//
// ASSUME-GUARANTEE CHECKLISTS
//   Placeable gate (from Model N / nodeclaimLifecycle.qnt): the armed
//   set is from the producer's last successful FFD tick over Registered
//   claims; it is NOT republished on ⊥ or consolidate-only ticks; it is
//   unarmed (None) on producer lease loss before this consumer's next
//   tick, and on producer restart. Model J therefore lets the
//   environment arm the gate with an arbitrary (possibly stale) subset
//   of intents and unarm it (fault-lease regime), and never recomputes
//   it. The crd-absent configuration is an init-time constant: no
//   producer exists there (main.rs does not spawn the L10 reconciler).
//   rio-lease (imported, never re-modeled): at most one believer; an
//   ex-leader can keep acting only until it observes the loss. The L1
//   loop is not lease-gated; the lease only matters via the gate above.
//   Retry campaign (imported): the scheduler-side dedup of termination
//   reports (recently_disconnected) makes the controller's re-report
//   idempotent; termination reports are therefore out of this model.
//
// INVARIANTS (each with its violation latch in `violations`)
//   ceilingRespected     spawns never push active-minus-freed past the
//                        ceiling (I4)
//   reapSafety           no excess-reap of a live-Running pod; no
//                        orphan-reap of a busy executor or outside a
//                        passed 3-arm gate (I3, model-state form)
//   orphanRemoved        the I2 safety form: an orphan-pending /
//                        orphan-running Job whose enabling conditions
//                        held at tick start is deleted by tick end
//   ackSoundness         an intent is acked spawned only with a Job
//                        behind it (I5); 409s and failed creates are
//                        never acked
//   ackCoversPending     a surviving Pending Job for a gated intent is
//                        (re-)acked when the ack RPC succeeds — the
//                        scheduler-restart re-arm of dispatched_cells
//   degradedPolarity     per-consumer degradation matrix: a failed poll
//                        spawns nothing, reaps nothing (stale or
//                        excess), acks nothing; a failed executor list
//                        orphan-reaps nothing
//   gateFailClosed       the per-configuration GateFailClosed clauses
//                        (armed / present-unarmed / crd-absent x
//                        Builder / Fetcher)
//   freedSlotsSpendable  a selector-drift reap whose intent is still
//                        wanted gets its respawn attempt the same tick
//                        (the I-183 freed-slot / skip-set clause)
//   SingleJobPerIntent (I1) is enforced by construction (Jobs are keyed
//   by deterministic name = intent id and a create on an occupied slot
//   is the apiserver's 409); it is deliberately NOT claimed as a checked
//   invariant — the checked content is the no-ack-on-collision half
//   (ackSoundness) and the canReach409Dedupe witness.
//
// WITNESSES (must be VIOLATED — wired as expect-violation checks)
//   canReachExcessReap, canReachOrphanReap, canReach409Dedupe,
//   canReachGateBlockedSpawn, canReachDriftReap, and (crd-absent cfg)
//   canReachUngatedSpawn + canReachSuppressedExcess.
//
// WEAKENED-TEST PROCEDURE (recorded; depths/state counts live in the
// introducing commit message): deleting the live pod-phase re-check arm
// from the tick's excess-reap step falsifies reapSafety; deleting the
// queued_known guard falsifies degradedPolarity (and gateFailClosed in
// the present-unarmed configuration); deleting the leader-age arm of
// the orphan gate widens reapSafety's exposure in the fault-lease
// regime to the scheduler-restart window (the busy-but-never-registered
// residual stays a documented bound either way, because the executor
// map IS the scheduler's own view).
module spawnCoherence {
  // ------------------------------------------------------------------
  // 1. Types
  // ------------------------------------------------------------------
  type IntentId = str

  /// Selector fingerprint (rio.build/intent-selector) — two abstract
  /// values; drift is inequality.
  type Fp = FpA | FpB

  /// Job phase as the LIST/census sees it.
  ///   JPending     active, status.ready == 0, no deletionTimestamp
  ///   JRunning     active, status.ready  > 0, no deletionTimestamp
  ///   JTerminal    succeeded/failed > 0 (not active)
  ///   JTerminating deletionTimestamp set (foreground delete in flight)
  type JobPhase = JAbsent | JPending | JRunning | JTerminal | JTerminating

  /// Bucketed creation age. PastPending = older than REAP_PENDING_GRACE
  /// (10s); PastOrphan = older than ORPHAN_REAP_GRACE (300s).
  type Age = AgeFresh | AgePastPending | AgePastOrphan

  /// The scheduler's executor entry for a Job's pod ({job}-prefixed).
  type ExecState = ExecAbsent | ExecIdle | ExecBusy

  /// The placeable gate as the consumer sees it (CRD-present runs).
  type GateState = GUnarmed | GArmed(Set[IntentId])

  type JobRec = { phase: JobPhase, age: Age, fp: Fp, podRunning: bool }

  /// Per-tick observable effects (rewritten every tick) — the witness
  /// observables and the degradation-matrix bookkeeping.
  type Effect =
    | ESpawned
    | EDedupe409
    | ECreateFailed
    | EExcessReap
    | EOrphanReap
    | EDriftReap
    | EOrphanPendingReap
    | ETerminalReap
    | EGateBlockedSpawn
    | EAcked
    | EExcessSuppressed

  /// Violation latches. Each invariant below is `not(contains(...))`;
  /// in a correct model every latch stays unset forever, so these add
  /// no reachable states.
  type Violation =
    | VCeiling
    | VReapedBusy
    | VOrphanMissed
    | VAckedWithoutJob
    | VAckMissedPending
    | VDegraded
    | VGate
    | VFreedSlotUnused

  // ------------------------------------------------------------------
  // 2. Constants (regime modules instantiate these)
  // ------------------------------------------------------------------
  /// Pool kind: Builder pools are gate-retained; Fetcher pools are not.
  const IS_BUILDER: bool
  /// NodeClaim CRD present? (crd-absent: Ctx.placeable = None — no
  /// producer exists; gate_armed is false and intents pass unfiltered).
  const CRD_PRESENT: bool
  /// spec.maxConcurrent. The model always sets one (the uncapped case
  /// adds no protocol behavior).
  const CEILING: int
  /// Per-tick nondet ⊥ for GetSpawnIntents / ListExecutors / the ack /
  /// the Job create (fault-rpc regime).
  const ENABLE_RPC_FAULT: bool
  /// Producer lease-loss gate unarm + scheduler restart (fault-lease).
  const ENABLE_LEASE_FAULT: bool
  /// The intent poll may read the previous tick's queue snapshot
  /// (fault-stale regime).
  const ENABLE_STALE_POLL: bool

  pure val INTENTS: Set[IntentId] = Set("i1", "i2")

  pure def boolChoices(faultOn: bool): Set[bool] =
    if (faultOn) Set(false, true) else Set(false)

  // ------------------------------------------------------------------
  // 3. Pure predicates (mirror pool/job.rs)
  // ------------------------------------------------------------------
  /// is_active_job ∧ no deletionTimestamp — what JobCensus.active counts.
  pure def censusActive(j: JobRec): bool = j.phase == JPending or j.phase == JRunning

  /// is_pending_job — the excess-reap candidate set.
  pure def isPending(j: JobRec): bool = j.phase == JPending

  /// is_running_job — the orphan-reap candidate set.
  pure def isRunning(j: JobRec): bool = j.phase == JRunning

  /// job_older_than(REAP_PENDING_GRACE).
  pure def pastPendingGrace(j: JobRec): bool =
    j.age == AgePastPending or j.age == AgePastOrphan

  /// job_older_than(ORPHAN_REAP_GRACE).
  pure def pastOrphanGrace(j: JobRec): bool = j.age == AgePastOrphan

  /// Age ordering for the oldest-first excess sort (older = larger).
  pure def ageRank(a: Age): int =
    match a { AgeFresh => 0 | AgePastPending => 1 | AgePastOrphan => 2 }

  /// Deterministic intent order — stands in for the scheduler's
  /// priority sort and the oldest-first tie-break.
  pure def idRank(i: IntentId): int = if (i == "i1") 1 else 2

  // ------------------------------------------------------------------
  // 4. State
  // ------------------------------------------------------------------
  /// Scheduler Ready-intent set (the queue) and per-intent fingerprint
  /// of the current solve.
  var queued: IntentId -> bool
  var intentFp: IntentId -> Fp
  /// The apiserver Job slot per intent (deterministic name).
  var job: IntentId -> JobRec
  /// Scheduler executor map entry for each Job's pod.
  var execr: IntentId -> ExecState
  /// ListExecutors.leader_for_secs >= ORPHAN_REAP_GRACE.
  var leaderOld: bool
  /// The placeable gate (meaningful only when CRD_PRESENT).
  var gate: GateState
  /// Scheduler-side dispatched_cells entries (peer state armed by the
  /// ack; cleared by heartbeat / DAG sweep / scheduler restart).
  var dispatched: Set[IntentId]
  /// The queue as of the previous tick's poll — the stale snapshot the
  /// fault-stale regime lets the poll read. Kept empty when disabled.
  var prevQueuedSnap: Set[IntentId]
  /// Per-tick effect record (rewritten each tick).
  var effects: Set[Effect]
  /// Violation latches (must stay empty).
  var violations: Set[Violation]

  // ------------------------------------------------------------------
  // 5. Invariants
  // ------------------------------------------------------------------
  val ceilingRespected = not(violations.contains(VCeiling))
  val reapSafety = not(violations.contains(VReapedBusy))
  val orphanRemoved = not(violations.contains(VOrphanMissed))
  val ackSoundness = not(violations.contains(VAckedWithoutJob))
  val ackCoversPending = not(violations.contains(VAckMissedPending))
  val degradedPolarity = not(violations.contains(VDegraded))
  val gateFailClosed = not(violations.contains(VGate))
  val freedSlotsSpendable = not(violations.contains(VFreedSlotUnused))

  // Witnesses (expected to be VIOLATED — see the module header).
  val canReachExcessReap = not(effects.contains(EExcessReap))
  val canReachOrphanReap = not(effects.contains(EOrphanReap))
  val canReach409Dedupe = not(effects.contains(EDedupe409))
  val canReachGateBlockedSpawn = not(effects.contains(EGateBlockedSpawn))
  val canReachDriftReap = not(effects.contains(EDriftReap))
  /// crd-absent configuration, two halves of the C1 adjudication: a
  /// Builder spawn proceeds ungated (fail-open), and an excess-pending
  /// surplus is left in place because queued is never authoritative
  /// there (fail-closed). Two witnesses because at the 2-intent scale a
  /// spawn and a surplus cannot coexist in one tick (a spawnable intent
  /// has no Job yet, so it never contributes to the surplus).
  val canReachUngatedSpawn = not(effects.contains(ESpawned))
  val canReachSuppressedExcess = not(effects.contains(EExcessSuppressed))

  // ------------------------------------------------------------------
  // 6. The tick (one action; helper pure defs first)
  // ------------------------------------------------------------------
  /// Placeable-gate retain (jobs.rs gate match + PlaceableGate::retain):
  /// returns (gate_armed, post-gate intent set).
  pure def gateRetain(
    crdPresent: bool, isBuilder: bool, g: GateState, polled: Set[IntentId]
  ): (bool, Set[IntentId]) =
    if (not(crdPresent))
      // CRD absent: gate_armed = false, intents pass through unfiltered.
      (false, polled)
    else if (not(isBuilder))
      // Fetcher pool with the CRD present: armed for reap purposes,
      // never spawn-filtered.
      (true, polled)
    else
      match g {
        | GUnarmed => (false, Set())            // retain() cleared the vec
        | GArmed(s) => (true, polled.intersect(s))
      }

  /// The oldest `n` of the pending candidates (oldest-first, id as the
  /// deterministic tie-break) — select_excess_pending's sort+truncate.
  pure def oldestN(cands: Set[IntentId], jobsAtList: IntentId -> JobRec, n: int): Set[IntentId] =
    cands.filter(i =>
      cands.filter(o =>
        ageRank(jobsAtList.get(o).age) > ageRank(jobsAtList.get(i).age)
        or (ageRank(jobsAtList.get(o).age) == ageRank(jobsAtList.get(i).age) and idRank(o) < idRank(i))
      ).size() < n
    )

  action tick = {
    // Per-tick faults (per-RPC ⊥ subsets) and the stale-poll choice.
    nondet pollFails = boolChoices(ENABLE_RPC_FAULT).oneOf()
    nondet listExecFails = boolChoices(ENABLE_RPC_FAULT).oneOf()
    nondet ackFails = boolChoices(ENABLE_RPC_FAULT).oneOf()
    nondet createFailsFor = (if (ENABLE_RPC_FAULT) INTENTS.powerset() else Set(Set())).oneOf()
    nondet useStalePoll = boolChoices(ENABLE_STALE_POLL).oneOf()

    // ---- 1. Intent poll (fail-open: ⊥ reads as empty) --------------
    val currentQueue = INTENTS.filter(i => queued.get(i))
    val polled = if (pollFails) Set()
                 else if (useStalePoll) prevQueuedSnap
                 else currentQueue
    val schedErr = pollFails

    // ---- 2. Placeable-gate retain -----------------------------------
    val gr = gateRetain(CRD_PRESENT, IS_BUILDER, gate, polled)
    val gateArmed = gr._1
    val postGate = gr._2
    val q = postGate.size()

    // ---- 3. Job LIST + census (tick-start snapshot) ------------------
    val jobsAtList = job
    val activeAtList = INTENTS.filter(i => censusActive(jobsAtList.get(i))).size()

    // ---- 4. reap_stale_for_intents ----------------------------------
    // want = post-gate intents; empty want => no reaps (fail-closed).
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

    // ---- 5. Headroom (freed slots subtracted before the clamp) -------
    val freed = reaped.filter(i => censusActive(jobsAtList.get(i))).size()
    val headroom0 = CEILING - (activeAtList - freed)
    val headroom = if (headroom0 < 0) 0 else headroom0
    val existingNames = INTENTS.filter(i => jobsAtList.get(i).phase != JAbsent).exclude(reaped)

    // ---- 6. Spawn pass ------------------------------------------------
    // Existing-name filter BEFORE the headroom truncate; deterministic
    // priority order (i1 before i2) stands in for the scheduler's
    // priority sort.
    val spawnCandidates = postGate.filter(i => not(existingNames.contains(i)))
    val toSpawn = spawnCandidates.filter(i =>
      spawnCandidates.filter(o => idRank(o) < idRank(i)).size() < headroom)
    // The create's outcome depends on the slot's truth at create time:
    // terminal-collision reaps were background deletes (object gone),
    // foreground deletes leave a Terminating object => 409.
    val jobsAfterReap = INTENTS.mapBy(i =>
      if (terminalReaps.contains(i)) { phase: JAbsent, age: AgeFresh, fp: FpA, podRunning: false }
      else if (reaped.contains(i)) { ...jobsAtList.get(i), phase: JTerminating }
      else jobsAtList.get(i))
    val spawnedSet = toSpawn.filter(i =>
      jobsAfterReap.get(i).phase == JAbsent and not(createFailsFor.contains(i)))
    val collided = toSpawn.filter(i => jobsAfterReap.get(i).phase != JAbsent)
    val createFailed = toSpawn.filter(i =>
      jobsAfterReap.get(i).phase == JAbsent and createFailsFor.contains(i))

    // ---- 7. Ack (spawned ∪ already-Pending, minus reaped) -------------
    val pendingNotReaped = postGate.filter(i =>
      isPending(jobsAtList.get(i)) and not(reaped.contains(i)))
    val toAck = pendingNotReaped.union(spawnedSet)
    val ackHappens = toAck.size() > 0 and not(ackFails)

    // ---- 8. reap_excess_pending ---------------------------------------
    val queuedKnown = not(schedErr) and gateArmed
    val excessCandidates = INTENTS.filter(i =>
      isPending(jobsAtList.get(i))
      and pastPendingGrace(jobsAtList.get(i))
      and not(reaped.contains(i)))
    val excessOver = excessCandidates.size() - q
    val excessSelected = if (queuedKnown and excessOver > 0)
        oldestN(excessCandidates, jobsAtList, excessOver)
      else Set()
    // Live (non-informer) pod-phase re-check: a live Running pod is
    // skipped (fail-closed on the lookup is not modeled separately —
    // a skipped delete is a skipped delete).
    val excessReaped = excessSelected.filter(i => not(jobsAtList.get(i).podRunning))
    val excessSuppressed = not(queuedKnown) and excessCandidates.size() > q

    // ---- 9. reap_orphan_running ---------------------------------------
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

    // ---- Post-state of the Job slots ----------------------------------
    val jobsAfter = INTENTS.mapBy(i =>
      if (spawnedSet.contains(i))
        { phase: JPending, age: AgeFresh, fp: intentFp.get(i), podRunning: false }
      else if (excessReaped.contains(i) or orphanReaped.contains(i))
        { ...jobsAfterReap.get(i), phase: JTerminating }
      else jobsAfterReap.get(i))

    // ---- Effects --------------------------------------------------------
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

    // ---- Violation checks ----------------------------------------------
    // I4: spawns never push active-minus-freed past the ceiling.
    val vCeil = spawnedSet.size() > 0 and (activeAtList - freed + spawnedSet.size()) > CEILING
    // I3: an excess-reap of a live-Running pod, or an orphan-reap of a
    // busy executor / outside a passed gate.
    val vBusy =
      excessReaped.filter(i => jobsAtList.get(i).podRunning).size() > 0
      or orphanReaped.filter(i => execr.get(i) == ExecBusy).size() > 0
      or (orphanReaped.size() > 0 and not(orphanGatePasses))
    // I2 safety form: enabling conditions at tick start imply the delete
    // by tick end (orphan-pending and orphan-running arms).
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
    // I5: every acked intent has a Job behind it at ack time.
    val vAckNoJob = ackHappens and toAck.filter(i => not(isPending(jobsAfter.get(i)))).size() > 0
    // The re-ack half: a surviving gated Pending Job is acked when the
    // ack RPC went through.
    val vAckMissed = ackHappens and pendingNotReaped.filter(i => not(toAck.contains(i))).size() > 0
    // Degradation matrix (per-consumer, not per-RPC).
    val vDegr =
      (schedErr and (
        spawnedSet.size() > 0 or reaped.size() > 0 or excessReaped.size() > 0
        or ackHappens))
      or (listExecFails and orphanReaped.size() > 0)
      or (ackFails and ackHappens)
    // GateFailClosed, split per configuration (C1/C2 adjudications).
    val builderGated = CRD_PRESENT and IS_BUILDER
    val vGateV =
      // (a) present ∧ unarmed: zero Builder spawns, zero excess-reaps.
      (builderGated and gate == GUnarmed
        and (spawnedSet.size() > 0 or excessReaped.size() > 0))
      // (c) armed: Builder spawns only inside the published set.
      or (builderGated and (match gate {
            | GUnarmed => false
            | GArmed(s) => spawnedSet.filter(i => not(s.contains(i))).size() > 0
          }))
      // (b)+(d) crd-absent (any kind): excess-reap stays fail-closed.
      or (not(CRD_PRESENT) and excessReaped.size() > 0)
      // (b) crd-absent never filters spawn demand (fail-open): the
      // post-gate set is the polled set untouched. The headroom /
      // existing-name truncation downstream is the same as for any
      // other configuration and is not a gate effect.
      or (not(CRD_PRESENT) and postGate != polled)
    // I-183 freed-slot clause, single-candidate form: a drift-reaped
    // intent that is still wanted, has a freed slot available and no
    // competing spawn candidate gets its respawn attempt the same tick
    // (worst case it 409s against the terminating old Job). Catches the
    // freed-not-credited and reaped-name-still-skipped regressions
    // without over-constraining the priority order between intents.
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
      // The tick reads but does not write the environment.
      queued' = queued,
      intentFp' = intentFp,
      execr' = execr,
      leaderOld' = leaderOld,
      gate' = gate,
    }
  }

  // ------------------------------------------------------------------
  // 7. Environment actions (between ticks)
  // ------------------------------------------------------------------
  /// The controller's per-tick bookkeeping is untouched by environment
  /// actions.
  action bookkeepingUnchanged = all {
    dispatched' = dispatched,
    effects' = effects,
    violations' = violations,
    prevQueuedSnap' = prevQueuedSnap,
  }

  /// A drv becomes Ready: the intent (re-)appears in the queue.
  action intentArrives(i: IntentId): bool = all {
    not(queued.get(i)),
    queued' = queued.set(i, true),
    intentFp' = intentFp, job' = job, execr' = execr,
    leaderOld' = leaderOld, gate' = gate,
    bookkeepingUnchanged,
  }

  /// The drv leaves Ready (assigned elsewhere, cancelled, completed).
  action intentLeaves(i: IntentId): bool = all {
    queued.get(i),
    queued' = queued.set(i, false),
    intentFp' = intentFp, job' = job, execr' = execr,
    leaderOld' = leaderOld, gate' = gate,
    bookkeepingUnchanged,
  }

  /// The scheduler re-solves the intent with a different placement
  /// (ICE-backoff spot -> on-demand): the selector fingerprint flips.
  action intentResolves(i: IntentId): bool = all {
    queued.get(i),
    intentFp' = intentFp.set(i, if (intentFp.get(i) == FpA) FpB else FpA),
    queued' = queued, job' = job, execr' = execr,
    leaderOld' = leaderOld, gate' = gate,
    bookkeepingUnchanged,
  }

  /// The Pending Job's pod starts running; Job.status.ready lags
  /// (the informer-lag window the live re-check exists for).
  action podStartsLagged(i: IntentId): bool = all {
    job.get(i).phase == JPending,
    not(job.get(i).podRunning),
    job' = job.set(i, { ...job.get(i), podRunning: true }),
    queued' = queued, intentFp' = intentFp, execr' = execr,
    leaderOld' = leaderOld, gate' = gate,
    bookkeepingUnchanged,
  }

  /// The Job controller catches up: ready > 0.
  action jobControllerSyncs(i: IntentId): bool = all {
    job.get(i).phase == JPending,
    job.get(i).podRunning,
    job' = job.set(i, { ...job.get(i), phase: JRunning }),
    queued' = queued, intentFp' = intentFp, execr' = execr,
    leaderOld' = leaderOld, gate' = gate,
    bookkeepingUnchanged,
  }

  /// The pod exits (build done or worker idle-exit): Job goes terminal,
  /// the executor entry disappears.
  action jobCompletes(i: IntentId): bool = all {
    job.get(i).phase == JRunning,
    job' = job.set(i, { phase: JTerminal, age: AgeFresh, fp: FpA, podRunning: false }),
    execr' = execr.set(i, ExecAbsent),
    queued' = queued, intentFp' = intentFp,
    leaderOld' = leaderOld, gate' = gate,
    bookkeepingUnchanged,
  }

  /// The pod fails before ever starting (image pull, admission):
  /// backoffLimit 0 => Failed.
  action jobFailsAtStart(i: IntentId): bool = all {
    job.get(i).phase == JPending,
    job' = job.set(i, { phase: JTerminal, age: AgeFresh, fp: FpA, podRunning: false }),
    queued' = queued, intentFp' = intentFp, execr' = execr,
    leaderOld' = leaderOld, gate' = gate,
    bookkeepingUnchanged,
  }

  /// ttlSecondsAfterFinished reaps the terminal Job.
  action ttlDeletes(i: IntentId): bool = all {
    job.get(i).phase == JTerminal,
    job' = job.set(i, { phase: JAbsent, age: AgeFresh, fp: FpA, podRunning: false }),
    queued' = queued, intentFp' = intentFp, execr' = execr,
    leaderOld' = leaderOld, gate' = gate,
    bookkeepingUnchanged,
  }

  /// A foreground delete completes (job-tracking finalizer cleared).
  action finalizerCompletes(i: IntentId): bool = all {
    job.get(i).phase == JTerminating,
    job' = job.set(i, { phase: JAbsent, age: AgeFresh, fp: FpA, podRunning: false }),
    execr' = execr.set(i, ExecAbsent),
    queued' = queued, intentFp' = intentFp,
    leaderOld' = leaderOld, gate' = gate,
    bookkeepingUnchanged,
  }

  /// Wall-clock passes for a live Job: Fresh -> PastPending -> PastOrphan.
  action jobAges(i: IntentId): bool = all {
    censusActive(job.get(i)),
    job.get(i).age != AgePastOrphan,
    job' = job.set(i, { ...job.get(i),
      age: if (job.get(i).age == AgeFresh) AgePastPending else AgePastOrphan }),
    queued' = queued, intentFp' = intentFp, execr' = execr,
    leaderOld' = leaderOld, gate' = gate,
    bookkeepingUnchanged,
  }

  /// The pod's worker registers with the scheduler (idle until assigned).
  action execRegisters(i: IntentId): bool = all {
    job.get(i).phase == JRunning,
    execr.get(i) == ExecAbsent,
    execr' = execr.set(i, ExecIdle),
    queued' = queued, intentFp' = intentFp, job' = job,
    leaderOld' = leaderOld, gate' = gate,
    bookkeepingUnchanged,
  }

  /// The scheduler assigns work to the registered executor.
  action execGetsWork(i: IntentId): bool = all {
    execr.get(i) == ExecIdle,
    execr' = execr.set(i, ExecBusy),
    queued' = queued, intentFp' = intentFp, job' = job,
    leaderOld' = leaderOld, gate' = gate,
    bookkeepingUnchanged,
  }

  /// The build finishes but the pod lingers (idle past its self-exit).
  action execGoesIdle(i: IntentId): bool = all {
    execr.get(i) == ExecBusy,
    execr' = execr.set(i, ExecIdle),
    queued' = queued, intentFp' = intentFp, job' = job,
    leaderOld' = leaderOld, gate' = gate,
    bookkeepingUnchanged,
  }

  /// The executor disconnects without the Job leaving Running
  /// (D-state pod, network partition) — the I-165 orphan shape.
  action execVanishes(i: IntentId): bool = all {
    execr.get(i) != ExecAbsent,
    execr' = execr.set(i, ExecAbsent),
    queued' = queued, intentFp' = intentFp, job' = job,
    leaderOld' = leaderOld, gate' = gate,
    bookkeepingUnchanged,
  }

  /// The scheduler leader has now held the lease past the orphan grace.
  action leaderAges = all {
    not(leaderOld),
    leaderOld' = true,
    queued' = queued, intentFp' = intentFp, job' = job, execr' = execr,
    gate' = gate,
    bookkeepingUnchanged,
  }

  /// Scheduler restart / failover: the executor map and dispatched_cells
  /// are in-memory and empty out; the new leader is young.
  action schedulerRestarts = all {
    ENABLE_LEASE_FAULT,
    execr' = INTENTS.mapBy(i => ExecAbsent),
    leaderOld' = false,
    dispatched' = Set(),
    queued' = queued, intentFp' = intentFp, job' = job, gate' = gate,
    effects' = effects, violations' = violations, prevQueuedSnap' = prevQueuedSnap,
  }

  /// A dispatched_cells entry is consumed scheduler-side (heartbeat
  /// registration edge or DAG-state sweep).
  action dispatchedCleared(i: IntentId): bool = all {
    dispatched.contains(i),
    dispatched' = dispatched.exclude(Set(i)),
    queued' = queued, intentFp' = intentFp, job' = job, execr' = execr,
    leaderOld' = leaderOld, gate' = gate,
    effects' = effects, violations' = violations, prevQueuedSnap' = prevQueuedSnap,
  }

  /// The L10 producer publishes a placeable set (assume-guarantee: any
  /// subset, possibly stale relative to the current queue).
  action gatePublishes = all {
    CRD_PRESENT,
    nondet s = INTENTS.powerset().oneOf()
    gate' = GArmed(s),
    queued' = queued, intentFp' = intentFp, job' = job, execr' = execr,
    leaderOld' = leaderOld,
    bookkeepingUnchanged,
  }

  /// The producer loses its lease (or restarts): the gate is unarmed
  /// before this consumer's next tick (the +5 producer guarantee).
  action gateUnarms = all {
    ENABLE_LEASE_FAULT,
    CRD_PRESENT,
    gate != GUnarmed,
    gate' = GUnarmed,
    queued' = queued, intentFp' = intentFp, job' = job, execr' = execr,
    leaderOld' = leaderOld,
    bookkeepingUnchanged,
  }

  // ------------------------------------------------------------------
  // 8. init / step
  // ------------------------------------------------------------------
  action init = all {
    queued' = INTENTS.mapBy(i => false),
    intentFp' = INTENTS.mapBy(i => FpA),
    job' = INTENTS.mapBy(i => { phase: JAbsent, age: AgeFresh, fp: FpA, podRunning: false }),
    execr' = INTENTS.mapBy(i => ExecAbsent),
    leaderOld' = true,
    gate' = GUnarmed,
    dispatched' = Set(),
    prevQueuedSnap' = Set(),
    effects' = Set(),
    violations' = Set(),
  }

  action step = {
    nondet i = INTENTS.oneOf()
    any {
      tick,
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

// =====================================================================
// Regime modules — one per check in nix/quint.nix.
// =====================================================================

/// Production shape, no faults: Builder pool, NodeClaim CRD present,
/// ceiling 1 (binds with two intents). The healthy lifecycle plus the
/// gate arming/publish staleness.
module spawnCoherenceBase {
  import spawnCoherence(
    IS_BUILDER = true,
    CRD_PRESENT = true,
    CEILING = 1,
    ENABLE_RPC_FAULT = false,
    ENABLE_LEASE_FAULT = false,
    ENABLE_STALE_POLL = false
  ).*
}

/// Per-RPC ⊥ faults: the GetSpawnIntents poll, the ListExecutors call,
/// the ack and the Job create can each fail per tick.
module spawnCoherenceFaultRpc {
  import spawnCoherence(
    IS_BUILDER = true,
    CRD_PRESENT = true,
    CEILING = 1,
    ENABLE_RPC_FAULT = true,
    ENABLE_LEASE_FAULT = false,
    ENABLE_STALE_POLL = false
  ).*
}

/// Failover faults: the L10 producer loses its lease (gate unarm) and
/// the scheduler restarts (executor map + dispatched_cells empty out,
/// leader young again).
module spawnCoherenceFaultLease {
  import spawnCoherence(
    IS_BUILDER = true,
    CRD_PRESENT = true,
    CEILING = 1,
    ENABLE_RPC_FAULT = false,
    ENABLE_LEASE_FAULT = true,
    ENABLE_STALE_POLL = false
  ).*
}

/// Per-read snapshot staleness: the intent poll may read the previous
/// tick's queue while the Job LIST reads the current state — the I-183
/// incoherence direction the channel allows.
module spawnCoherenceFaultStale {
  import spawnCoherence(
    IS_BUILDER = true,
    CRD_PRESENT = true,
    CEILING = 1,
    ENABLE_RPC_FAULT = false,
    ENABLE_LEASE_FAULT = false,
    ENABLE_STALE_POLL = true
  ).*
}

/// CRD-absent configuration (static-node / k3s without Karpenter):
/// gate_armed is false every tick — spawn fail-open, excess-reap
/// fail-closed (the C1 adjudication).
module spawnCoherenceCrdAbsent {
  import spawnCoherence(
    IS_BUILDER = true,
    CRD_PRESENT = false,
    CEILING = 1,
    ENABLE_RPC_FAULT = false,
    ENABLE_LEASE_FAULT = false,
    ENABLE_STALE_POLL = false
  ).*
}

/// Fetcher pool with the CRD present: never spawn-filtered, excess-reap
/// keyed only on scheduler reachability (the C2 adjudication).
module spawnCoherenceFetcher {
  import spawnCoherence(
    IS_BUILDER = false,
    CRD_PRESENT = true,
    CEILING = 1,
    ENABLE_RPC_FAULT = false,
    ENABLE_LEASE_FAULT = false,
    ENABLE_STALE_POLL = false
  ).*
}
