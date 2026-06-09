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

  /// C2/120: the intent's entry in the scheduler's `recently_closed`
  /// window (CCNone = no entry). Single-valued last-writer abstraction
  /// of the per-exec window.
  type CloseCause = CCNone | CCCompleted | CCCancelled

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
    | EMatJobFiltered
    | EEstablished
    | EPoisoned
    | ECancelReap
    | ESynthClose

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
    | VMatJobServed
    | VEstablishedEarly
    | VPoisonTransient
    | VCancelNonCancelled
    | VCancelPostCloseJob
    | VPoisonForeignCount
    | VForeignStreakWipe
    | VPoisonBurst
    | VYoungLeaderReap
    | VStaleClose

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
  /// Materialization jobs exist (substitution-replacement Phase B,
  /// PD-7 / bbb607447): an intent's node can carry an unresolved
  /// materialization job, and GetSpawnIntents FILTERS such intents
  /// from the served set (the spawn-intent gate's job conjunct —
  /// design §9.2: "spawnCoherence's intent-emission precondition
  /// gains the job filter"). false = the var stays constant-false:
  /// the JOB-FREE trace subspace, which post-D-prime is NOT a flag
  /// state (the coexistence flag was deleted, Wave D4) but the still-
  /// reachable runs where no intent's node ever carries an unresolved
  /// job — builds with no substitutable nodes (PD-D8: the six
  /// job-free regimes keep verifying that subspace).
  const ENABLE_MAT_JOBS: bool
  /// Bughunt wave A2 axis (bug_106; §4.F16 — axis-gated default-false,
  /// existing regimes state-space-identical): the spawn-rendered
  /// deadline is CARRIED into the executor binding (the
  /// jobs.rs annotation -> pods.rs winner tuple -> BoundIntent ->
  /// AuthBinding chain) and the establishment window anchors to it —
  /// never to a shrunk mint-time re-solve. C2's four axes follow this
  /// same gating (announced in the wave log).
  const ENABLE_RENDERED_DEADLINE: bool
  /// C2 area-A axis pair (bug_077 fleet-exhaust persistence): POISON
  /// turns on the exhaustion environment + the AD2 spawn-gate verdict;
  /// PERSIST selects the FIXED law (poison only at the third
  /// consecutive exhausted tick) vs the as-built immediate poison.
  const ENABLE_EXHAUST_POISON: bool
  const ENABLE_EXHAUST_PERSIST: bool
  /// C2 area-C axis pair (bug_120 close-cause): ASBUILT models the
  /// retired closed→active edge inference over tick-local seen-open
  /// evidence; CAUSE models the recently_closed cause-witness arm.
  /// Mutually exclusive by regime construction.
  const ENABLE_CANCEL_ASBUILT: bool
  const ENABLE_CANCEL_CAUSE: bool
  /// C2 area-C axis (merged_bug_221): true = the as-built reap with NO
  /// leader-age gate (the c473655f4 pin's behavior). All pre-C2
  /// regimes bind false and keep the gate — state-space identical.
  const ENABLE_YOUNG_LEADER_REAP: bool
  /// C2 area-D axis pair (merged_bug_135): SYNTH turns on the
  /// disruption-decide / close-land machinery; PINNED selects the
  /// exec-pinned law (a close lands only on the OBSERVED attempt;
  /// otherwise refused) vs the as-built newest-open-wins resolution.
  const ENABLE_SYNTH_CLOSE: bool
  const ENABLE_EXEC_PINNED: bool
  /// Bughunt wave A3 axis (bug_282; same §4.F16 gating —
  /// default-false, existing regimes state-space-identical): the
  /// transient-retry backoff is enforced at the SPAWN edge — a Ready
  /// node inside its backoff window is excluded from spawn intents
  /// (compute_spawn_intents' conjunct; the kernel's
  /// build_backoff_expired consumes the same fact at pull
  /// admission).
  const ENABLE_RETRY_BACKOFF: bool

  /// merged_bug_117 (bughunt-2 slot 5): a SECOND pool reconciling over
  /// the same scheduler. Gates the poolBTick action — false in every
  /// pre-existing regime, so their state graphs are bit-identical
  /// (the F16 Base identity).
  const ENABLE_MULTI_POOL: bool
  /// merged_bug_117 fix axis: exhaustion streaks keyed by (pool,
  /// intent). Off = the as-built pool-SHARED map (pool B's retain
  /// wipes pool A's streaks; an intent gated in both pools
  /// double-steps). Ghost-framed: meaningful only under
  /// ENABLE_MULTI_POOL ∧ ENABLE_EXHAUST_POISON.
  const ENABLE_STREAK_POOL_KEY: bool
  /// bug_113 (bughunt-2 slot 5): the cancel arm's generation conjunct
  /// — a Job that POSTDATES its intent's cancelled close is
  /// structurally unselectable (CancelTarget::bind conjunct 3). Off =
  /// the as-built cause-only law (the respawn-inside-the-window
  /// teardown, previously a contained residual). Ghost-framed:
  /// meaningful only under ENABLE_CANCEL_CAUSE.
  const ENABLE_CANCEL_GENERATION: bool
  /// m073 cadence axis (bughunt-4 S1a): wall-clock floor under the
  /// poison verdict. CADENCE turns on wall tracking, the fold-skip
  /// edge (a reconcile pass that retains the pool without stepping
  /// it) and the burst latch; ENFORCED adds the floor conjunct to the
  /// verdict itself — split so the falsify twin can track the clock
  /// without enforcing it (the as-built law).
  const ENABLE_STREAK_CADENCE: bool
  const STREAK_FLOOR_ENFORCED: bool
  /// Wall units a streak must span before the verdict may fire (2 =
  /// the 20s wall floor over the ~10s reconcile cadence, abstracted).
  const STREAK_FLOOR: int
  /// bug_028 axis (bughunt-5 S2; default false per the round-4 SS4.F16
  /// discipline, bughunt4-fix-specs.md --- every pre-existing regime
  /// binds false and stays state-space identical): the AS-BUILT
  /// window-coupled fold. True = the streak fold's evaluated set is
  /// the post-truncate spawn WINDOW (take(headroom) before the gate):
  /// the fold runs only when an in-window intent is exhausted, gated
  /// intents burn window slots, the report can only name in-window
  /// intents, and a live streak whose intent is exhausted-and-wanted
  /// but OUT of the window is RESET by the completing fold --- the
  /// reset-without-observation the VForeignStreakWipe latch records
  /// (the safety form of "persistent exhaustion eventually reports").
  /// False = the fixed law: the fold evaluates every wanted intent
  /// (the order this model always encoded --- the implementation
  /// diverged; the twin pins the divergence as a falsified law).
  const WINDOW_COUPLED_FOLD: bool

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

  /// NO_ELIGIBLE_SOURCE_PERSIST_TICKS clamp (the streak saturates at
  /// the poison threshold; higher counts are behaviorally identical).
  pure def clampStreak(n: int): int = if (n > 3) 3 else n
  /// m073: the wall-age clamp (saturating at the floor — beyond it
  /// the exact age is law-irrelevant; keeps the state space finite).
  pure def clampAge(n: int): int = if (n > STREAK_FLOOR) STREAK_FLOOR else n

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
  /// PD-7: the intent's node carries an unresolved materialization
  /// job (driven by the matJobOpens/matJobResolves environment;
  /// constant-false unless ENABLE_MAT_JOBS).
  var matJob: IntentId -> bool
  /// A2/106: the rendered (spawn-time) deadline rode the binding into
  /// the mint — set at execGetsWork when the axis is on; the
  /// establishment window below anchors to it (AgePastOrphan = the far
  /// rendered threshold; the pre-fix shrunk re-solve would have fired
  /// at AgePastPending).
  var renderedFarDl: IntentId -> bool
  /// C2/120: the recently_closed window entry per intent (env-driven;
  /// constant CCNone unless a cancel axis is on).
  var closedCause: IntentId -> CloseCause
  /// C2/120 as-built arm: the tick-local covered-by-open-attempt
  /// evidence map (constant-false unless ENABLE_CANCEL_ASBUILT).
  var seenOpen: IntentId -> bool
  /// C2/077: this tick the candidate set excludes every spawnable
  /// source for the intent (env-driven; constant-false off-axis).
  var exhausted: IntentId -> bool
  /// C2/077: consecutive exhausted-and-wanted ticks (clamped at 3;
  /// constant-zero off-axis).
  var streak: IntentId -> int
  /// merged_bug_117 ghost: THIS pool's true gated-tick count per
  /// intent (what the streak SHOULD equal under pool-keying). Written
  /// only by tick under the exact streak-increment condition;
  /// constant-zero off the exhaust axis.
  var ownGatedTicks: IntentId -> int
  /// m073 cadence ghost (frozen off-axis — the TRACK_EPISODE_GHOSTS
  /// precedent: all-constant unless ENABLE_STREAK_CADENCE, zero state
  /// cost). Wall units elapsed since the streak's FIRST gated
  /// observation, saturating at STREAK_FLOOR (beyond the floor the
  /// exact age is law-irrelevant — the clamp keeps TLC finite). Born
  /// 0 at the streak's birth tick, grows with the nondet wall
  /// advance, reset with the streak.
  var streakWallAge: IntentId -> int
  /// bug_113 ghost: the intent's current Job was created AFTER the
  /// window's close landed (birth-vs-close order of the live pair).
  /// Set by the spawn pass while a close is in the window; cleared
  /// when a NEWER close lands or the job leaves. Constant-false
  /// unless a cancel axis is on (closedCause is constant CCNone).
  var jobPostdatesClose: IntentId -> bool
  /// C2/135: a synthesized preempted close is in flight for the intent
  /// (decided, not yet landed). Constant-false unless ENABLE_SYNTH_CLOSE.
  var pendingClose: IntentId -> bool
  /// C2/135: the attempt the disruption watcher OBSERVED open is still
  /// the open one (false the moment that attempt ends, and never set by
  /// a successor attempt). Constant-false unless ENABLE_SYNTH_CLOSE.
  var obsLive: IntentId -> bool
  /// A3/282: the drv sits inside its transient-retry backoff window
  /// (env-driven; constant-false unless ENABLE_RETRY_BACKOFF). The
  /// backoff may only START on a non-queued intent (the failure that
  /// opens the window also leaves Ready), and the spawn edge refuses
  /// in-backoff intents — noSpawnIntentInsideBackoff.
  var inBackoff: IntentId -> bool

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
  /// A3/282: no spawn intent inside the transient-retry backoff
  /// window — the spawn edge's conjunct holds (trivially when the
  /// axis is off; the controller-282 calibration drops the conjunct
  /// and reaches the violation).
  val noSpawnIntentInsideBackoff = INTENTS.forall(i =>
    queued.get(i) implies not(inBackoff.get(i))
  )
  val freedSlotsSpendable = not(violations.contains(VFreedSlotUnused))
  /// PD-7: a fresh, successful GetSpawnIntents serve never contains a
  /// job-carrying intent (the spawn-intent filter; the stale-poll
  /// in-flight window is the documented as-built race and is excluded
  /// — the served-set claim is at serve time).
  val matJobFilteredFromIntents = not(violations.contains(VMatJobServed))

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
  /// PD-7: the job filter actually filters (a queued, job-carrying
  /// intent existed at a successful fresh poll).
  val canReachMatJobFiltered = not(effects.contains(EMatJobFiltered))
  /// A2/106: no establishment ever fired before the RENDERED (carried)
  /// deadline passed (the latch stays unset).
  val establishedOnlyPastRenderedDeadline = not(violations.contains(VEstablishedEarly))
  /// Violated when an establishment actually happens (the rendered-
  /// deadline regime's contended edge is reachable — non-vacuity).
  val canReachEstablished = not(effects.contains(EEstablished))

  /// C2/077: the AD2 fleet-exhaust verdict fires only after the full
  /// persistence window — a transient (sub-threshold) exhaustion never
  /// poisons a derivation that still has a placeable source within the
  /// window. Falsified as-built (immediate poison); holds with
  /// ENABLE_EXHAUST_PERSIST.
  val noPoisonWhilePlaceable = not(violations.contains(VPoisonTransient))
  /// C2/120: the AD5 cancel arm deletes only Jobs whose close cause is
  /// CANCELLED. Falsified as-built (the closed→active edge is
  /// cause-blind: a normal completion in the Job-status lag window is
  /// selected); holds with ENABLE_CANCEL_CAUSE.
  val cancelArmDeletesOnlyCancelled = not(violations.contains(VCancelNonCancelled))
  /// bug_113: the cancel arm never tears down a Job created AFTER its
  /// close (cancel + fast re-submit respawns the deterministic name).
  /// Falsified with ENABLE_CANCEL_GENERATION off (the previously
  /// contained 120-residual respawn trace, promoted to a violation);
  /// holds with the generation conjunct.
  val cancelNeverDeletesPostCloseJob = not(violations.contains(VCancelPostCloseJob))
  /// merged_bug_117: the poison verdict counts the OBSERVING pool's
  /// own consecutive observations — a foreign pool's tick on the
  /// shared map can neither advance the count (the double-step) nor
  /// reach the threshold for it. Falsified as-built under
  /// ENABLE_MULTI_POOL; holds with ENABLE_STREAK_POOL_KEY.
  val streakCountsOwnObservations = not(violations.contains(VPoisonForeignCount))
  /// merged_bug_117: a foreign pool's tick never resets a live streak
  /// (the as-built retain-wipe livelocked the report — the safety form
  /// of "persistent exhaustion eventually reports": given no foreign
  /// wipe, the 3-tick threshold delivers it).
  val persistentExhaustionEventuallyReports = not(violations.contains(VForeignStreakWipe))
  /// m073 (bughunt-4 S1a): the poison verdict spans at least
  /// STREAK_FLOOR wall units from the streak's first gated
  /// observation — a sub-second reconcile burst (ticks without wall
  /// advance) can never fire it. Falsified with CADENCE on and
  /// ENFORCED off (the as-built floor-blind law); holds when the
  /// verdict carries the floor conjunct.
  val poisonRespectsWallFloor = not(violations.contains(VPoisonBurst))
  /// C2/221: a never-pulled Running Job (no executor entry) is never
  /// orphan-reaped while the serving leader is younger than the grace.
  /// Falsified with ENABLE_YOUNG_LEADER_REAP (the as-built gate-less
  /// reap); holds in every gated regime including fault-lease.
  val noReapOfNeverPulledBeforeLeaderAged = not(violations.contains(VYoungLeaderReap))
  /// C2/135: a synthesized close consumes only the attempt the
  /// controller OBSERVED open at decision time — never a successor
  /// attempt minted in the decide→land window. Falsified as-built
  /// (newest-open-wins intent resolution); holds with
  /// ENABLE_EXEC_PINNED (the scheduler refuses instead).
  val closeTargetsIssuedAttempt = not(violations.contains(VStaleClose))

  // C2 reachability witnesses (each wired as an expected-violation
  // check in its hold regime — the invariants above must not hold
  // vacuously).
  val canReachPoison = not(effects.contains(EPoisoned))
  val canReachCancelReap = not(effects.contains(ECancelReap))
  val canReachSynthClose = not(effects.contains(ESynthClose))

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
    // m073 cadence: the wall may stand still across ticks (the burst)
    // and a reconcile pass may RETAIN the pool without folding it (no
    // step, no reset — the retain-without-step edge). Off-axis both
    // collapse to today's tick (wall frozen, fold always runs). The
    // fold-skip nondet is exercised only in the floor-BLIND world
    // (the twin): with the floor enforced it is monotone-safe — a
    // skipped fold freezes the streak while the age keeps growing,
    // strictly weakening every poison precondition — so the HOLD
    // regime pins it false to keep TLC inside budget (measured: the
    // 4-way tick branching put the exhaust-persist x cadence product
    // at 843M generated / 57M distinct / queue growing at the 1800s
    // kill).
    nondet wallAdv = (if (ENABLE_STREAK_CADENCE) Set(0, 1) else Set(0)).oneOf()
    nondet foldSkip = boolChoices(
      ENABLE_STREAK_CADENCE and not(STREAK_FLOOR_ENFORCED)).oneOf()

    // ---- 1. Intent poll (fail-open: ⊥ reads as empty) --------------
    // PD-7 (bbb607447): the served set excludes intents whose node
    // carries an unresolved materialization job — the filter applies
    // AT SERVE TIME, so the stale snapshot inherits the filter as of
    // its own tick (an in-flight response raced by a job creation is
    // the documented as-built window, not a violation).
    val matFiltered = INTENTS.filter(i => queued.get(i) and matJob.get(i))
    val currentQueue = INTENTS.filter(i => queued.get(i) and not(matJob.get(i)))
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
    val wantedCandidates = postGate.filter(i => not(existingNames.contains(i)))
    // C2/077: the AD2 fleet-exhaust gate — an exhausted intent's spawn
    // is WITHHELD (never created this tick) and its streak advances;
    // any non-exhausted (or unwanted) tick resets the streak to zero.
    // The poison verdict fires at the persistence threshold (PERSIST)
    // or immediately (the as-built law).
    val spawnCandidates = wantedCandidates.filter(i =>
      not(ENABLE_EXHAUST_POISON and exhausted.get(i)))
    // bug_028 axis: the as-built spawn WINDOW (take(headroom) BEFORE
    // the gate) and its fold-completion condition (the lazy LIST runs
    // only when an in-window intent carries exclusions). Pure derived
    // vals --- zero state cost off-axis.
    val windowSet = wantedCandidates.filter(i =>
      wantedCandidates.filter(o => idRank(o) < idRank(i)).size() < headroom)
    val windowFoldRuns = windowSet.filter(i =>
      ENABLE_EXHAUST_POISON and exhausted.get(i)).size() > 0
    // m073: a fold-skip tick retains streak state untouched (no step,
    // no reset) — sub-second re-reconciles must not advance the count.
    val streakAfter =
      if (foldSkip) streak
      else if (WINDOW_COUPLED_FOLD)
        // As-built: the fold's domain is the WINDOW; a no-exhausted-
        // in-window tick skips the fold entirely (retain), and a
        // completing fold resets everything outside its gated set ---
        // including out-of-window live streaks (the bug_028 wipe).
        (if (not(windowFoldRuns)) streak else INTENTS.mapBy(i =>
          if (ENABLE_EXHAUST_POISON and exhausted.get(i)
              and windowSet.contains(i) and jobsAtList.get(i).phase == JAbsent)
            clampStreak(streak.get(i) + 1)
          else 0))
      else INTENTS.mapBy(i =>
        if (ENABLE_EXHAUST_POISON and exhausted.get(i)
            and postGate.contains(i) and jobsAtList.get(i).phase == JAbsent)
          clampStreak(streak.get(i) + 1)
        else 0)
    // merged_bug_117 ghost: the same fold over the pool's OWN count —
    // identical condition, so under pool-keying streak == ownGatedTicks
    // by construction; a foreign double-step makes them diverge.
    val ownObsAfter =
      if (foldSkip) ownGatedTicks
      else if (WINDOW_COUPLED_FOLD)
        // Mirrors streakAfter's window law exactly (the ghost tracks
        // the fold actually performed; its foreign-double-step role
        // is meaningful only under ENABLE_MULTI_POOL).
        (if (not(windowFoldRuns)) ownGatedTicks else INTENTS.mapBy(i =>
          if (ENABLE_EXHAUST_POISON and exhausted.get(i)
              and windowSet.contains(i) and jobsAtList.get(i).phase == JAbsent)
            clampStreak(ownGatedTicks.get(i) + 1)
          else 0))
      else INTENTS.mapBy(i =>
        if (ENABLE_EXHAUST_POISON and exhausted.get(i)
            and postGate.contains(i) and jobsAtList.get(i).phase == JAbsent)
          clampStreak(ownGatedTicks.get(i) + 1)
        else 0)
    // m073: each live streak's wall age after this tick — born 0 at
    // the streak's birth tick, advanced by the wall (whether or not
    // the fold ran — a skipped fold does not stop the clock), reset
    // with the streak, saturating at the floor. Frozen all-0 off-axis
    // (streakAfter folds to 0 and wallAdv is pinned 0).
    val streakWallAgeAfter =
      if (foldSkip)
        INTENTS.mapBy(i =>
          if (streak.get(i) > 0) clampAge(streakWallAge.get(i) + wallAdv)
          else 0)
      else INTENTS.mapBy(i =>
        if (streakAfter.get(i) == 0) 0
        else if (streak.get(i) == 0) 0
        else clampAge(streakWallAge.get(i) + wallAdv))
    val poisonedBase = INTENTS.filter(i =>
      ENABLE_EXHAUST_POISON and exhausted.get(i)
      and postGate.contains(i) and jobsAtList.get(i).phase == JAbsent
      // bug_028 axis: as-built, the report is drawn from a COMPLETED
      // fold's gated set --- in-window intents only.
      and (not(WINDOW_COUPLED_FOLD) or (windowFoldRuns and windowSet.contains(i)))
      and (if (ENABLE_EXHAUST_PERSIST) streakAfter.get(i) >= 3
           else streakAfter.get(i) >= 1))
    // m073: the verdict carries the wall floor — only when ENFORCED
    // (the twin tracks the clock but keeps the as-built floor-blind
    // verdict, so the burst latch below can demonstrate it).
    val poisoned = poisonedBase.filter(i =>
      not(STREAK_FLOOR_ENFORCED)
      or streakWallAgeAfter.get(i) >= STREAK_FLOOR)
    val toSpawn =
      if (WINDOW_COUPLED_FOLD)
        // As-built: the window is cut FIRST, then gated intents are
        // withheld --- a gated intent burns its slot.
        windowSet.filter(i => not(ENABLE_EXHAUST_POISON and exhausted.get(i)))
      else spawnCandidates.filter(i =>
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

    // ---- 6b. AD5 cancel arm (C2/120) ----------------------------------
    // CAUSE law: select iff the window lists the intent CANCELLED and
    // no open attempt covers it (ExecBusy = open attempt). ASBUILT law:
    // the closed→active edge over the seen-open evidence map — covered
    // last successful read, uncovered now, still active (cause-blind).
    // Both fail closed on an unreadable view (the same listExec fault
    // models ListOpenAttempts).
    val cancelSelected =
      if (ENABLE_CANCEL_CAUSE)
        INTENTS.filter(i =>
          not(listExecFails)
          and censusActive(jobsAtList.get(i))
          and closedCause.get(i) == CCCancelled
          // bug_113 conjunct 3: the Job must PREDATE the close
          // (CancelTarget::bind's generation check) — a respawned Job
          // is structurally unselectable.
          and (not(ENABLE_CANCEL_GENERATION) or not(jobPostdatesClose.get(i)))
          and execr.get(i) != ExecBusy
          and not(reaped.contains(i)))
      else if (ENABLE_CANCEL_ASBUILT)
        INTENTS.filter(i =>
          not(listExecFails)
          and censusActive(jobsAtList.get(i))
          and seenOpen.get(i)
          and execr.get(i) != ExecBusy
          and not(reaped.contains(i)))
      else Set()

    // ---- 7. Ack (spawned ∪ already-Pending, minus reaped) -------------
    val pendingNotReaped = postGate.filter(i =>
      isPending(jobsAtList.get(i)) and not(reaped.contains(i))
      and not(cancelSelected.contains(i)))
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
      // C2/221 axis: the as-built reap had neither a leader-age gate
      // nor the stream-era executors-present arm — a successful read
      // of the durably-empty post-failover view was authoritative.
      and (leaderOld or ENABLE_YOUNG_LEADER_REAP)
      and (executorsPresent.size() > 0 or ENABLE_YOUNG_LEADER_REAP)
    val orphanReaped = if (orphanGatePasses)
        orphanCandidates.filter(i => execr.get(i) != ExecBusy)
      else Set()

    // ---- Post-state of the Job slots ----------------------------------
    val jobsAfter = INTENTS.mapBy(i =>
      if (spawnedSet.contains(i))
        { phase: JPending, age: AgeFresh, fp: intentFp.get(i), podRunning: false }
      else if (excessReaped.contains(i) or orphanReaped.contains(i)
               or cancelSelected.contains(i))
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
        .union(if (matFiltered.size() > 0 and not(pollFails) and not(useStalePoll))
          Set(EMatJobFiltered) else Set())
        .union(if (poisoned.size() > 0) Set(EPoisoned) else Set())
        .union(if (cancelSelected.size() > 0) Set(ECancelReap) else Set())

    // ---- Violation checks ----------------------------------------------
    // I4: spawns never push active-minus-freed past the ceiling.
    val vCeil = spawnedSet.size() > 0 and (activeAtList - freed + spawnedSet.size()) > CEILING
    // I3: an excess-reap of a live-Running pod, or an orphan-reap of a
    // busy executor / outside a passed gate.
    val vBusy =
      excessReaped.filter(i => jobsAtList.get(i).podRunning).size() > 0
      or orphanReaped.filter(i => execr.get(i) == ExecBusy).size() > 0
      or (orphanReaped.size() > 0 and not(orphanGatePasses))
    // C2/221: an orphan reap of a never-pulled Job (no executor entry)
    // under a young leader.
    val vYoungReap =
      orphanReaped.filter(i => execr.get(i) == ExecAbsent).size() > 0
      and not(leaderOld)
    // C2/077: a poison verdict before the persistence window closed.
    val vPoisonT = poisoned.filter(i => streakAfter.get(i) < 3).size() > 0
    // C2/120: a cancel of a Job whose close cause is not CANCELLED.
    val vCancelNonC =
      cancelSelected.filter(i => closedCause.get(i) != CCCancelled).size() > 0
    // bug_113: a cancel of a Job created AFTER its close landed.
    val vCancelPostClose =
      cancelSelected.filter(i => jobPostdatesClose.get(i)).size() > 0
    // merged_bug_117: a poison verdict whose OWN-pool observation
    // count is below the persistence threshold (a foreign pool's
    // double-step inflated the shared entry).
    val vPoisonForeign =
      poisoned.filter(i => ownObsAfter.get(i) < 3).size() > 0
    // bug_028: a completing window-coupled fold RESET a live streak
    // for an intent it never evaluated --- exhausted, wanted, job-
    // absent (it would have stepped under the all-wanted law) but
    // pushed out of the window. Reset-without-observation is the same
    // latch family as the m117 foreign wipe: persistent exhaustion
    // never reports.
    val vWindowWipe =
      WINDOW_COUPLED_FOLD and not(foldSkip) and windowFoldRuns and
      INTENTS.filter(i =>
        streak.get(i) > 0 and exhausted.get(i)
        and postGate.contains(i) and jobsAtList.get(i).phase == JAbsent
        and not(windowSet.contains(i))).size() > 0
    // m073: a poison verdict on a streak younger than the wall floor
    // (the reconcile-burst verdict). Under ENFORCED the floor conjunct
    // above empties this set by construction — the enforcement IS the
    // same predicate the latch tests (the machine witness).
    val vPoisonBurst =
      ENABLE_STREAK_CADENCE and
      poisoned.filter(i =>
        streakWallAgeAfter.get(i) < STREAK_FLOOR).size() > 0
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
      or (listExecFails and (orphanReaped.size() > 0 or cancelSelected.size() > 0))
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
        // C2/077: the freed-slot obligation binds only spawnABLE
        // intents — an exhausted intent's spawn is withheld by the
        // AD2 gate itself (that withholding is the law under test,
        // not a freed-slot regression).
        and not(ENABLE_EXHAUST_POISON and exhausted.get(i))
        and spawnCandidates.exclude(Set(i)).size() == 0
        and headroom >= 1
        and not(toSpawn.contains(i))
      ).size() > 0
    // PD-7: a fresh successful serve containing a job-carrying intent.
    val vMatServed =
      not(pollFails) and not(useStalePoll)
      and polled.filter(i => matJob.get(i)).size() > 0
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
        .union(if (vMatServed) Set(VMatJobServed) else Set())
        .union(if (vPoisonT) Set(VPoisonTransient) else Set())
        .union(if (vCancelNonC) Set(VCancelNonCancelled) else Set())
        .union(if (vCancelPostClose) Set(VCancelPostCloseJob) else Set())
        .union(if (vPoisonForeign) Set(VPoisonForeignCount) else Set())
        .union(if (vWindowWipe) Set(VForeignStreakWipe) else Set())
        .union(if (vPoisonBurst) Set(VPoisonBurst) else Set())
        .union(if (vYoungReap) Set(VYoungLeaderReap) else Set())

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
      matJob' = matJob,
      renderedFarDl' = renderedFarDl,
      // C2/120 as-built evidence rebuild — only on a SUCCESSFUL view
      // read (fail-closed leaves the recorded evidence untouched).
      seenOpen' = if (ENABLE_CANCEL_ASBUILT and not(listExecFails))
        INTENTS.mapBy(i =>
          censusActive(jobsAtList.get(i)) and execr.get(i) == ExecBusy)
      else seenOpen,
      streak' = streakAfter,
      ownGatedTicks' = ownObsAfter,
      streakWallAge' = streakWallAgeAfter,
      // bug_113 birth-order ghost: a spawned Job postdates the close
      // currently in the window; a surviving Job keeps its bit while
      // it exists; a departed Job clears it.
      jobPostdatesClose' = INTENTS.mapBy(i =>
        if (spawnedSet.contains(i)) closedCause.get(i) != CCNone
        else (jobPostdatesClose.get(i) and jobsAfter.get(i).phase != JAbsent)),
      closedCause' = closedCause,
      exhausted' = exhausted,
      pendingClose' = pendingClose,
      obsLive' = obsLive,
      inBackoff' = inBackoff,
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
    matJob' = matJob,
    renderedFarDl' = renderedFarDl,
    // C2 vars framed for every composing environment action. obsLive
    // is NOT here: the attempt-ending actions must clear it, so each
    // composing action frames (or clears) it explicitly.
    closedCause' = closedCause,
    seenOpen' = seenOpen,
    exhausted' = exhausted,
    streak' = streak,
    streakWallAge' = streakWallAge,
    ownGatedTicks' = ownGatedTicks,
    jobPostdatesClose' = jobPostdatesClose,
    pendingClose' = pendingClose,
    inBackoff' = inBackoff,
  }

  /// A drv becomes Ready: the intent (re-)appears in the queue.
  /// A3/282: the spawn edge refuses an intent inside its backoff
  /// window (compute_spawn_intents' conjunct — trivially true when
  /// the axis is off).
  action intentArrives(i: IntentId): bool = all {
    not(queued.get(i)),
    not(inBackoff.get(i)),
    queued' = queued.set(i, true),
    intentFp' = intentFp, job' = job, execr' = execr,
    leaderOld' = leaderOld, gate' = gate,
    obsLive' = obsLive,
    bookkeepingUnchanged,
  }

  /// The drv leaves Ready (assigned elsewhere, cancelled, completed).
  action intentLeaves(i: IntentId): bool = all {
    queued.get(i),
    queued' = queued.set(i, false),
    intentFp' = intentFp, job' = job, execr' = execr,
    leaderOld' = leaderOld, gate' = gate,
    obsLive' = obsLive,
    bookkeepingUnchanged,
  }

  /// The scheduler re-solves the intent with a different placement
  /// (ICE-backoff spot -> on-demand): the selector fingerprint flips.
  action intentResolves(i: IntentId): bool = all {
    queued.get(i),
    intentFp' = intentFp.set(i, if (intentFp.get(i) == FpA) FpB else FpA),
    queued' = queued, job' = job, execr' = execr,
    leaderOld' = leaderOld, gate' = gate,
    obsLive' = obsLive,
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
    obsLive' = obsLive,
    bookkeepingUnchanged,
  }

  /// The Job controller catches up: ready > 0.
  action jobControllerSyncs(i: IntentId): bool = all {
    job.get(i).phase == JPending,
    job.get(i).podRunning,
    job' = job.set(i, { ...job.get(i), phase: JRunning }),
    queued' = queued, intentFp' = intentFp, execr' = execr,
    leaderOld' = leaderOld, gate' = gate,
    obsLive' = obsLive,
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
    obsLive' = obsLive.set(i, false),
    bookkeepingUnchanged,
  }

  /// The pod fails before ever starting (image pull, admission):
  /// backoffLimit 0 => Failed.
  action jobFailsAtStart(i: IntentId): bool = all {
    job.get(i).phase == JPending,
    job' = job.set(i, { phase: JTerminal, age: AgeFresh, fp: FpA, podRunning: false }),
    queued' = queued, intentFp' = intentFp, execr' = execr,
    leaderOld' = leaderOld, gate' = gate,
    obsLive' = obsLive,
    bookkeepingUnchanged,
  }

  /// ttlSecondsAfterFinished reaps the terminal Job.
  action ttlDeletes(i: IntentId): bool = all {
    job.get(i).phase == JTerminal,
    job' = job.set(i, { phase: JAbsent, age: AgeFresh, fp: FpA, podRunning: false }),
    queued' = queued, intentFp' = intentFp, execr' = execr,
    leaderOld' = leaderOld, gate' = gate,
    obsLive' = obsLive,
    bookkeepingUnchanged,
  }

  /// A foreground delete completes (job-tracking finalizer cleared).
  action finalizerCompletes(i: IntentId): bool = all {
    job.get(i).phase == JTerminating,
    job' = job.set(i, { phase: JAbsent, age: AgeFresh, fp: FpA, podRunning: false }),
    execr' = execr.set(i, ExecAbsent),
    queued' = queued, intentFp' = intentFp,
    leaderOld' = leaderOld, gate' = gate,
    obsLive' = obsLive.set(i, false),
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
    obsLive' = obsLive,
    bookkeepingUnchanged,
  }

  /// The pod's worker registers with the scheduler (idle until assigned).
  action execRegisters(i: IntentId): bool = all {
    job.get(i).phase == JRunning,
    execr.get(i) == ExecAbsent,
    execr' = execr.set(i, ExecIdle),
    queued' = queued, intentFp' = intentFp, job' = job,
    leaderOld' = leaderOld, gate' = gate,
    obsLive' = obsLive,
    bookkeepingUnchanged,
  }

  /// The scheduler assigns work to the registered executor.
  action execGetsWork(i: IntentId): bool = all {
    execr.get(i) == ExecIdle,
    execr' = execr.set(i, ExecBusy),
    // A2/106: the rendered deadline rides the binding into the mint
    // (identity off-axis: the map stays all-false).
    renderedFarDl' = if (ENABLE_RENDERED_DEADLINE)
      renderedFarDl.set(i, true)
    else renderedFarDl,
    inBackoff' = inBackoff,
    queued' = queued, intentFp' = intentFp, job' = job,
    leaderOld' = leaderOld, gate' = gate,
    dispatched' = dispatched, effects' = effects,
    violations' = violations, prevQueuedSnap' = prevQueuedSnap,
    matJob' = matJob,
    closedCause' = closedCause,
    seenOpen' = seenOpen,
    exhausted' = exhausted,
    streak' = streak, ownGatedTicks' = ownGatedTicks, jobPostdatesClose' = jobPostdatesClose,
    streakWallAge' = streakWallAge,
    pendingClose' = pendingClose,
    obsLive' = obsLive,
  }

  /// The build finishes but the pod lingers (idle past its self-exit).
  action execGoesIdle(i: IntentId): bool = all {
    execr.get(i) == ExecBusy,
    execr' = execr.set(i, ExecIdle),
    queued' = queued, intentFp' = intentFp, job' = job,
    leaderOld' = leaderOld, gate' = gate,
    obsLive' = obsLive.set(i, false),
    bookkeepingUnchanged,
  }

  /// The executor disconnects without the Job leaving Running
  /// (D-state pod, network partition) — the I-165 orphan shape.
  action execVanishes(i: IntentId): bool = all {
    execr.get(i) != ExecAbsent,
    execr' = execr.set(i, ExecAbsent),
    queued' = queued, intentFp' = intentFp, job' = job,
    leaderOld' = leaderOld, gate' = gate,
    obsLive' = obsLive.set(i, false),
    bookkeepingUnchanged,
  }

  /// The scheduler leader has now held the lease past the orphan grace.
  action leaderAges = all {
    not(leaderOld),
    leaderOld' = true,
    queued' = queued, intentFp' = intentFp, job' = job, execr' = execr,
    gate' = gate,
    obsLive' = obsLive,
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
    matJob' = matJob,
    // A2/106: the carried deadline is durable (AuthBinding) — restart
    // does not lose it.
    renderedFarDl' = renderedFarDl,
    // C2: the recently_closed window is PG-durable; the seen-open /
    // exhaust / pending-close maps are CONTROLLER-side (a scheduler
    // restart does not clear them).
    closedCause' = closedCause,
    seenOpen' = seenOpen,
    exhausted' = exhausted,
    streak' = streak, ownGatedTicks' = ownGatedTicks, jobPostdatesClose' = jobPostdatesClose,
    streakWallAge' = streakWallAge,
    pendingClose' = pendingClose,
    obsLive' = obsLive,
    inBackoff' = inBackoff,
  }

  /// A dispatched_cells entry is consumed scheduler-side (heartbeat
  /// registration edge or DAG-state sweep).
  action dispatchedCleared(i: IntentId): bool = all {
    dispatched.contains(i),
    dispatched' = dispatched.exclude(Set(i)),
    queued' = queued, intentFp' = intentFp, job' = job, execr' = execr,
    leaderOld' = leaderOld, gate' = gate,
    effects' = effects, violations' = violations, prevQueuedSnap' = prevQueuedSnap,
    matJob' = matJob,
    renderedFarDl' = renderedFarDl,
    closedCause' = closedCause,
    seenOpen' = seenOpen,
    exhausted' = exhausted,
    streak' = streak, ownGatedTicks' = ownGatedTicks, jobPostdatesClose' = jobPostdatesClose,
    streakWallAge' = streakWallAge,
    pendingClose' = pendingClose,
    obsLive' = obsLive,
    inBackoff' = inBackoff,
  }

  /// The L10 producer publishes a placeable set (assume-guarantee: any
  /// subset, possibly stale relative to the current queue).
  action gatePublishes = all {
    CRD_PRESENT,
    nondet s = INTENTS.powerset().oneOf()
    gate' = GArmed(s),
    queued' = queued, intentFp' = intentFp, job' = job, execr' = execr,
    leaderOld' = leaderOld,
    obsLive' = obsLive,
    bookkeepingUnchanged,
  }

  /// PD-7: a materialization job is created for the intent's node
  /// (merge/probe classification) — the node stays Ready and queued
  /// scheduler-side; GetSpawnIntents stops serving it.
  action matJobOpens(i: IntentId): bool = all {
    ENABLE_MAT_JOBS,
    not(matJob.get(i)),
    matJob' = matJob.set(i, true),
    queued' = queued, intentFp' = intentFp, job' = job, execr' = execr,
    leaderOld' = leaderOld, gate' = gate,
    dispatched' = dispatched, effects' = effects, violations' = violations,
    prevQueuedSnap' = prevQueuedSnap,
    renderedFarDl' = renderedFarDl,
    closedCause' = closedCause,
    seenOpen' = seenOpen,
    exhausted' = exhausted,
    streak' = streak, ownGatedTicks' = ownGatedTicks, jobPostdatesClose' = jobPostdatesClose,
    streakWallAge' = streakWallAge,
    pendingClose' = pendingClose,
    obsLive' = obsLive,
    inBackoff' = inBackoff,
  }

  /// PD-7: the job resolves (success / from-source / cancelled) — the
  /// intent is servable again.
  action matJobResolves(i: IntentId): bool = all {
    ENABLE_MAT_JOBS,
    matJob.get(i),
    matJob' = matJob.set(i, false),
    queued' = queued, intentFp' = intentFp, job' = job, execr' = execr,
    leaderOld' = leaderOld, gate' = gate,
    dispatched' = dispatched, effects' = effects, violations' = violations,
    prevQueuedSnap' = prevQueuedSnap,
    renderedFarDl' = renderedFarDl,
    closedCause' = closedCause,
    seenOpen' = seenOpen,
    exhausted' = exhausted,
    streak' = streak, ownGatedTicks' = ownGatedTicks, jobPostdatesClose' = jobPostdatesClose,
    streakWallAge' = streakWallAge,
    pendingClose' = pendingClose,
    obsLive' = obsLive,
    inBackoff' = inBackoff,
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
    obsLive' = obsLive,
    bookkeepingUnchanged,
  }

  /// Bughunt wave A2 (bug_106): the establishment sweep closes a
  /// vanished executor's outstanding work — and its window anchors to
  /// the RENDERED deadline the binding carried (renderedFarDl; the far
  /// threshold = AgePastOrphan), never to a shrunk mint-time re-solve
  /// (which would have fired at AgePastPending). The VEstablishedEarly
  /// latch records an establishment that beat the rendered deadline;
  /// with the carry in place the guard makes it unreachable.
  action establishExpired(i: IntentId): bool = all {
    ENABLE_RENDERED_DEADLINE,
    execr.get(i) == ExecAbsent,
    censusActive(job.get(i)),
    // the fixed window: the carried (far) rendered deadline has passed.
    renderedFarDl.get(i) implies job.get(i).age == AgePastOrphan,
    // fire only past SOME deadline at all (the near threshold is the
    // earliest any establishment can believe itself due).
    job.get(i).age != AgeFresh,
    effects' = effects.union(Set(EEstablished)),
    violations' = if (renderedFarDl.get(i) and job.get(i).age != AgePastOrphan)
      violations.union(Set(VEstablishedEarly))
    else violations,
    queued' = queued, intentFp' = intentFp, job' = job, execr' = execr,
    leaderOld' = leaderOld, gate' = gate,
    dispatched' = dispatched, prevQueuedSnap' = prevQueuedSnap,
    matJob' = matJob,
    renderedFarDl' = renderedFarDl,
    closedCause' = closedCause,
    seenOpen' = seenOpen,
    exhausted' = exhausted,
    streak' = streak, ownGatedTicks' = ownGatedTicks, jobPostdatesClose' = jobPostdatesClose,
    streakWallAge' = streakWallAge,
    pendingClose' = pendingClose,
    obsLive' = obsLive,
    inBackoff' = inBackoff,
  }

  /// C2/077: the spawnable-source universe flips for the intent —
  /// excluded ⊇ spawnable starts or stops holding (node restart,
  /// informer lag, genuine fleet exhaustion).
  action exhaustFlips(i: IntentId): bool = all {
    ENABLE_EXHAUST_POISON,
    exhausted' = exhausted.set(i, not(exhausted.get(i))),
    queued' = queued, intentFp' = intentFp, job' = job, execr' = execr,
    leaderOld' = leaderOld, gate' = gate,
    dispatched' = dispatched, effects' = effects, violations' = violations,
    prevQueuedSnap' = prevQueuedSnap, matJob' = matJob,
    renderedFarDl' = renderedFarDl,
    closedCause' = closedCause, seenOpen' = seenOpen,
    streak' = streak, pendingClose' = pendingClose, obsLive' = obsLive,
    streakWallAge' = streakWallAge,
    ownGatedTicks' = ownGatedTicks, jobPostdatesClose' = jobPostdatesClose,
    inBackoff' = inBackoff,
  }

  /// C2/120: the scheduler cancels the intent's open attempt (user
  /// cancel): the attempt closes CANCELLED while the Job lives on —
  /// the teardown the cancel arm exists to perform.
  action attemptCancelled(i: IntentId): bool = all {
    or { ENABLE_CANCEL_ASBUILT, ENABLE_CANCEL_CAUSE },
    execr.get(i) != ExecAbsent,
    execr' = execr.set(i, ExecAbsent),
    closedCause' = closedCause.set(i, CCCancelled),
    // bug_113: this close is NEWER than any existing job — the live
    // pair's order resets (a later spawn re-sets the bit).
    jobPostdatesClose' = jobPostdatesClose.set(i, false),
    obsLive' = obsLive.set(i, false),
    queued' = queued, intentFp' = intentFp, job' = job,
    leaderOld' = leaderOld, gate' = gate,
    dispatched' = dispatched, effects' = effects, violations' = violations,
    prevQueuedSnap' = prevQueuedSnap, matJob' = matJob,
    renderedFarDl' = renderedFarDl,
    seenOpen' = seenOpen, exhausted' = exhausted,
    streak' = streak, ownGatedTicks' = ownGatedTicks,
    streakWallAge' = streakWallAge,
    pendingClose' = pendingClose,
    inBackoff' = inBackoff,
  }

  /// C2/120: the attempt completes NORMALLY but the Job's status has
  /// not propagated yet — the teardown-lag window. The close cause is
  /// COMPLETED; the Job stays Running until the controller's informer
  /// catches up (modeled by jobCompletes never having fired).
  action attemptCompletesLagged(i: IntentId): bool = all {
    or { ENABLE_CANCEL_ASBUILT, ENABLE_CANCEL_CAUSE },
    execr.get(i) == ExecBusy,
    job.get(i).phase == JRunning,
    execr' = execr.set(i, ExecAbsent),
    closedCause' = closedCause.set(i, CCCompleted),
    jobPostdatesClose' = jobPostdatesClose.set(i, false),
    obsLive' = obsLive.set(i, false),
    queued' = queued, intentFp' = intentFp, job' = job,
    leaderOld' = leaderOld, gate' = gate,
    dispatched' = dispatched, effects' = effects, violations' = violations,
    prevQueuedSnap' = prevQueuedSnap, matJob' = matJob,
    renderedFarDl' = renderedFarDl,
    seenOpen' = seenOpen, exhausted' = exhausted,
    streak' = streak, ownGatedTicks' = ownGatedTicks,
    streakWallAge' = streakWallAge,
    pendingClose' = pendingClose,
    inBackoff' = inBackoff,
  }

  /// C2/120: the close ages out of the recently_closed window (120 s).
  action windowExpires(i: IntentId): bool = all {
    or { ENABLE_CANCEL_ASBUILT, ENABLE_CANCEL_CAUSE },
    closedCause.get(i) != CCNone,
    closedCause' = closedCause.set(i, CCNone),
    jobPostdatesClose' = jobPostdatesClose.set(i, false),
    queued' = queued, intentFp' = intentFp, job' = job, execr' = execr,
    leaderOld' = leaderOld, gate' = gate,
    dispatched' = dispatched, effects' = effects, violations' = violations,
    prevQueuedSnap' = prevQueuedSnap, matJob' = matJob,
    renderedFarDl' = renderedFarDl,
    seenOpen' = seenOpen, exhausted' = exhausted,
    streak' = streak, ownGatedTicks' = ownGatedTicks,
    streakWallAge' = streakWallAge,
    pendingClose' = pendingClose, obsLive' = obsLive,
    inBackoff' = inBackoff,
  }

  /// C2/135: the disruption watcher decides to preempt — it OBSERVES
  /// the currently-open attempt and synthesizes the close it will send.
  action disruptionDecides(i: IntentId): bool = all {
    ENABLE_SYNTH_CLOSE,
    execr.get(i) == ExecBusy,
    not(pendingClose.get(i)),
    pendingClose' = pendingClose.set(i, true),
    obsLive' = obsLive.set(i, true),
    queued' = queued, intentFp' = intentFp, job' = job, execr' = execr,
    leaderOld' = leaderOld, gate' = gate,
    dispatched' = dispatched, effects' = effects, violations' = violations,
    prevQueuedSnap' = prevQueuedSnap, matJob' = matJob,
    renderedFarDl' = renderedFarDl,
    closedCause' = closedCause, seenOpen' = seenOpen,
    exhausted' = exhausted, streak' = streak, ownGatedTicks' = ownGatedTicks, jobPostdatesClose' = jobPostdatesClose,
    streakWallAge' = streakWallAge,
    inBackoff' = inBackoff,
  }

  /// C2/135: the synthesized close lands at the scheduler. As-built
  /// (intent-keyed, newest-open-wins): it consumes WHATEVER attempt is
  /// open — a successor minted in the decide→land window included
  /// (VStaleClose). Exec-pinned: it consumes the attempt iff it is
  /// still the OBSERVED one; otherwise the scheduler refuses and
  /// nothing is consumed.
  action closeLands(i: IntentId): bool = all {
    ENABLE_SYNTH_CLOSE,
    pendingClose.get(i),
    pendingClose' = pendingClose.set(i, false),
    obsLive' = obsLive.set(i, false),
    execr' =
      if (ENABLE_EXEC_PINNED)
        (if (obsLive.get(i) and execr.get(i) != ExecAbsent)
          execr.set(i, ExecAbsent) else execr)
      else
        (if (execr.get(i) != ExecAbsent) execr.set(i, ExecAbsent) else execr),
    effects' =
      if (ENABLE_EXEC_PINNED)
        (if (obsLive.get(i) and execr.get(i) != ExecAbsent)
          effects.union(Set(ESynthClose)) else effects)
      else
        (if (execr.get(i) != ExecAbsent)
          effects.union(Set(ESynthClose)) else effects),
    violations' =
      if (not(ENABLE_EXEC_PINNED) and execr.get(i) != ExecAbsent
          and not(obsLive.get(i)))
        violations.union(Set(VStaleClose))
      else violations,
    queued' = queued, intentFp' = intentFp, job' = job,
    leaderOld' = leaderOld, gate' = gate,
    dispatched' = dispatched,
    prevQueuedSnap' = prevQueuedSnap, matJob' = matJob,
    renderedFarDl' = renderedFarDl,
    closedCause' = closedCause, seenOpen' = seenOpen,
    exhausted' = exhausted, streak' = streak, ownGatedTicks' = ownGatedTicks, jobPostdatesClose' = jobPostdatesClose,
    streakWallAge' = streakWallAge,
    inBackoff' = inBackoff,
  }

  /// A3/282 environment: a transient failure opens the backoff
  /// window — only on a non-queued intent (the failing attempt's
  /// release left Ready; the spawn edge is what the conjunct guards).
  action backoffStarts(i: IntentId): bool = all {
    ENABLE_RETRY_BACKOFF,
    not(queued.get(i)),
    not(inBackoff.get(i)),
    inBackoff' = inBackoff.set(i, true),
    queued' = queued, intentFp' = intentFp, job' = job, execr' = execr,
    leaderOld' = leaderOld, gate' = gate,
    dispatched' = dispatched, effects' = effects, violations' = violations,
    prevQueuedSnap' = prevQueuedSnap,
    matJob' = matJob,
    renderedFarDl' = renderedFarDl,
    closedCause' = closedCause,
    seenOpen' = seenOpen,
    exhausted' = exhausted,
    streak' = streak, ownGatedTicks' = ownGatedTicks, jobPostdatesClose' = jobPostdatesClose,
    streakWallAge' = streakWallAge,
    pendingClose' = pendingClose,
    obsLive' = obsLive,
  }

  /// The backoff window expires — the intent is spawnable again.
  action backoffExpires(i: IntentId): bool = all {
    ENABLE_RETRY_BACKOFF,
    inBackoff.get(i),
    inBackoff' = inBackoff.set(i, false),
    queued' = queued, intentFp' = intentFp, job' = job, execr' = execr,
    leaderOld' = leaderOld, gate' = gate,
    dispatched' = dispatched, effects' = effects, violations' = violations,
    prevQueuedSnap' = prevQueuedSnap,
    matJob' = matJob,
    renderedFarDl' = renderedFarDl,
    closedCause' = closedCause,
    seenOpen' = seenOpen,
    exhausted' = exhausted,
    streak' = streak, ownGatedTicks' = ownGatedTicks, jobPostdatesClose' = jobPostdatesClose,
    streakWallAge' = streakWallAge,
    pendingClose' = pendingClose,
    obsLive' = obsLive,
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
    matJob' = INTENTS.mapBy(i => false),
    renderedFarDl' = INTENTS.mapBy(i => false),
    closedCause' = INTENTS.mapBy(i => CCNone),
    seenOpen' = INTENTS.mapBy(i => false),
    exhausted' = INTENTS.mapBy(i => false),
    streak' = INTENTS.mapBy(i => 0),
    ownGatedTicks' = INTENTS.mapBy(i => 0),
    streakWallAge' = INTENTS.mapBy(i => 0),
    jobPostdatesClose' = INTENTS.mapBy(i => false),
    pendingClose' = INTENTS.mapBy(i => false),
    obsLive' = INTENTS.mapBy(i => false),
    inBackoff' = INTENTS.mapBy(i => false),
  }

  /// merged_bug_117: a SECOND pool's reconcile tick, abstracted to its
  /// effect on the exhaustion-streak storage. As-built
  /// (ENABLE_STREAK_POOL_KEY off): the map is pool-SHARED — B's fold
  /// retains only ITS gated set (wiping A's streaks) and steps every
  /// intent gated in B (double-stepping overlap intents). Fixed: B's
  /// fold lands under B's own (pool, intent) keys — A's storage is
  /// untouched, a cross-pool wipe is inexpressible. ownGatedTicks (A's
  /// true observation count) is never B's to advance either way.
  action poolBTick = {
    nondet bGated = INTENTS.powerset().oneOf()
    all {
      ENABLE_MULTI_POOL,
      ENABLE_EXHAUST_POISON,
      streak' =
        if (ENABLE_STREAK_POOL_KEY) streak
        else INTENTS.mapBy(i =>
          if (bGated.contains(i)) clampStreak(streak.get(i) + 1) else 0),
      // The as-built retain-wipe of a LIVE foreign streak is the
      // violation (the safety form of "persistent exhaustion
      // eventually reports": no foreign reset, and the 3-tick
      // threshold delivers).
      violations' = violations.union(
        if (not(ENABLE_STREAK_POOL_KEY)
            and INTENTS.filter(i =>
              streak.get(i) > 0 and not(bGated.contains(i))).size() > 0)
          Set(VForeignStreakWipe)
        else Set()),
      ownGatedTicks' = ownGatedTicks,
      // m073 cadence ghost: B's tick is not A's wall — no age
      // advance. Under pool-keying B touches no streak, so the ages
      // stand; under the as-built shared map the ages follow B's
      // fold (a B-birth starts at 0, a B-reset clears).
      streakWallAge' =
        if (ENABLE_STREAK_POOL_KEY) streakWallAge
        else INTENTS.mapBy(i =>
          if (not(bGated.contains(i))) 0
          else if (streak.get(i) == 0) 0
          else streakWallAge.get(i)),
      jobPostdatesClose' = jobPostdatesClose,
      queued' = queued, intentFp' = intentFp, job' = job, execr' = execr,
      leaderOld' = leaderOld, gate' = gate, dispatched' = dispatched,
      effects' = effects, prevQueuedSnap' = prevQueuedSnap,
      matJob' = matJob, renderedFarDl' = renderedFarDl,
      closedCause' = closedCause, seenOpen' = seenOpen,
      exhausted' = exhausted, pendingClose' = pendingClose,
      obsLive' = obsLive, inBackoff' = inBackoff,
    }
  }

  action step = {
    nondet i = INTENTS.oneOf()
    any {
      tick,
      poolBTick,
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
      matJobOpens(i),
      matJobResolves(i),
      establishExpired(i),
      exhaustFlips(i),
      attemptCancelled(i),
      attemptCompletesLagged(i),
      windowExpires(i),
      disruptionDecides(i),
      closeLands(i),
      backoffStarts(i),
      backoffExpires(i),
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
    WINDOW_COUPLED_FOLD = false
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
    WINDOW_COUPLED_FOLD = false
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
    WINDOW_COUPLED_FOLD = false
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
    ENABLE_STALE_POLL = true,
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
    WINDOW_COUPLED_FOLD = false
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
    WINDOW_COUPLED_FOLD = false
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
    WINDOW_COUPLED_FOLD = false
  ).*
}

/// PD-7 regime (substitution-replacement Phase B): materialization
/// jobs open and resolve under the healthy Builder configuration —
/// the GetSpawnIntents job filter is live. matJobFilteredFromIntents
/// is the headline; canReachMatJobFiltered keeps it non-vacuous.
/// Deleting the serve-time filter conjunct re-finds VMatJobServed
/// (the weakened-test procedure; depth in the introducing commit).
module spawnCoherenceMatJobs {
  import spawnCoherence(
    IS_BUILDER = true,
    CRD_PRESENT = true,
    CEILING = 1,
    ENABLE_RPC_FAULT = false,
    ENABLE_LEASE_FAULT = false,
    ENABLE_STALE_POLL = false,
    ENABLE_MAT_JOBS = true,
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
    WINDOW_COUPLED_FOLD = false
  ).*
}

/// Bughunt-wave A2 regime (bug_106): the rendered-deadline carry under
/// the healthy Builder configuration — execGetsWork carries the
/// spawn-rendered deadline into the binding and the establishment
/// sweep anchors to it. establishedOnlyPastRenderedDeadline is the
/// headline; canReachEstablished keeps it non-vacuous. C2's four axes
/// extend this module family per §4.F16 (axis vars announced in the
/// wave log).
module spawnCoherenceRenderedDeadline {
  import spawnCoherence(
    IS_BUILDER = true,
    CRD_PRESENT = true,
    CEILING = 1,
    ENABLE_RPC_FAULT = false,
    ENABLE_LEASE_FAULT = false,
    ENABLE_STALE_POLL = false,
    ENABLE_MAT_JOBS = false,
    ENABLE_RENDERED_DEADLINE = true,
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
    WINDOW_COUPLED_FOLD = false
  ).*
}

/// C2/077 FALSIFY half: the as-built AD2 gate poisons on the FIRST
/// exhausted tick — noPoisonWhilePlaceable is expected to be violated
/// here (a transient single-tick exhaustion poisons).
module spawnCoherenceExhaustAsBuilt {
  import spawnCoherence(
    IS_BUILDER = true,
    CRD_PRESENT = true,
    CEILING = 1,
    ENABLE_RPC_FAULT = false,
    ENABLE_LEASE_FAULT = false,
    ENABLE_STALE_POLL = false,
    ENABLE_MAT_JOBS = false,
    ENABLE_RENDERED_DEADLINE = false,
    ENABLE_EXHAUST_POISON = true,
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
    WINDOW_COUPLED_FOLD = false
  ).*
}

/// C2/077 HOLD half: the persistence-window law — the verdict fires
/// only at the third consecutive exhausted-and-wanted tick; any
/// placeable tick resets the streak. canReachPoison keeps the
/// invariant non-vacuous.
module spawnCoherenceExhaustPersist {
  import spawnCoherence(
    IS_BUILDER = true,
    CRD_PRESENT = true,
    CEILING = 1,
    ENABLE_RPC_FAULT = false,
    ENABLE_LEASE_FAULT = false,
    ENABLE_STALE_POLL = false,
    ENABLE_MAT_JOBS = false,
    ENABLE_RENDERED_DEADLINE = false,
    ENABLE_EXHAUST_POISON = true,
    ENABLE_EXHAUST_PERSIST = true,
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
    WINDOW_COUPLED_FOLD = false
  ).*
}

/// C2/120 FALSIFY half: the retired closed→active edge inference —
/// cause-blind, so a normal completion in the Job-status lag window is
/// selected for cancellation (cancelArmDeletesOnlyCancelled violated).
module spawnCoherenceCancelAsBuilt {
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
    ENABLE_CANCEL_ASBUILT = true,
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
    WINDOW_COUPLED_FOLD = false
  ).*
}

/// C2/120 HOLD half: the close-cause witness arm — selection iff the
/// recently_closed window lists the intent CANCELLED and no open
/// attempt covers it. canReachCancelReap keeps it non-vacuous.
module spawnCoherenceCancelCause {
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
    ENABLE_CANCEL_CAUSE = true,
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
    WINDOW_COUPLED_FOLD = false
  ).*
}

/// C2/221 FALSIFY half: the as-built orphan reap with NO leader-age
/// gate, under the failover environment (scheduler restarts empty the
/// executor map and reset leader age) —
/// noReapOfNeverPulledBeforeLeaderAged is expected to be violated (the
/// post-failover empty view mass-reaps the never-pulled cohort). The
/// HOLD half is the existing fault-lease regime, whose gate forbids
/// exactly this trace.
module spawnCoherenceReapYoungLeader {
  import spawnCoherence(
    IS_BUILDER = true,
    CRD_PRESENT = true,
    CEILING = 1,
    ENABLE_RPC_FAULT = false,
    ENABLE_LEASE_FAULT = true,
    ENABLE_STALE_POLL = false,
    ENABLE_MAT_JOBS = false,
    ENABLE_RENDERED_DEADLINE = false,
    ENABLE_EXHAUST_POISON = false,
    ENABLE_EXHAUST_PERSIST = false,
    ENABLE_CANCEL_ASBUILT = false,
    ENABLE_CANCEL_CAUSE = false,
    ENABLE_YOUNG_LEADER_REAP = true,
    ENABLE_SYNTH_CLOSE = false,
    ENABLE_EXEC_PINNED = false,
    ENABLE_MULTI_POOL = false,
    ENABLE_STREAK_POOL_KEY = false,
    ENABLE_CANCEL_GENERATION = false,
    ENABLE_RETRY_BACKOFF = false,
    ENABLE_STREAK_CADENCE = false,
    STREAK_FLOOR_ENFORCED = false,
    STREAK_FLOOR = 2,
    WINDOW_COUPLED_FOLD = false
  ).*
}

/// C2/135 FALSIFY half: as-built synthesized closes resolve by intent
/// (newest-open-wins) — a successor attempt minted in the decide→land
/// window is consumed by a verdict that never observed it
/// (closeTargetsIssuedAttempt violated).
module spawnCoherenceSynthCloseAsBuilt {
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
    ENABLE_SYNTH_CLOSE = true,
    ENABLE_EXEC_PINNED = false,
    ENABLE_MULTI_POOL = false,
    ENABLE_STREAK_POOL_KEY = false,
    ENABLE_CANCEL_GENERATION = false,
    ENABLE_RETRY_BACKOFF = false,
    ENABLE_STREAK_CADENCE = false,
    STREAK_FLOOR_ENFORCED = false,
    STREAK_FLOOR = 2,
    WINDOW_COUPLED_FOLD = false
  ).*
}

/// C2/135 HOLD half: exec-pinned synthesized closes — the close lands
/// iff the observed attempt is still the open one; otherwise the
/// scheduler refuses and consumes nothing. canReachSynthClose keeps it
/// non-vacuous.
module spawnCoherenceSynthClosePinned {
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
    ENABLE_SYNTH_CLOSE = true,
    ENABLE_EXEC_PINNED = true,
    ENABLE_MULTI_POOL = false,
    ENABLE_STREAK_POOL_KEY = false,
    ENABLE_CANCEL_GENERATION = false,
    ENABLE_RETRY_BACKOFF = false,
    ENABLE_STREAK_CADENCE = false,
    STREAK_FLOOR_ENFORCED = false,
    STREAK_FLOOR = 2,
    WINDOW_COUPLED_FOLD = false
  ).*
}

/// A3/282 axis regime: the transient-retry backoff at the spawn edge
/// (backoffStarts/backoffExpires drive the window; intentArrives
/// refuses in-backoff intents — noSpawnIntentInsideBackoff).
module spawnCoherenceRetryBackoff {
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
    ENABLE_RETRY_BACKOFF = true,
    ENABLE_STREAK_CADENCE = false,
    STREAK_FLOOR_ENFORCED = false,
    STREAK_FLOOR = 2,
    WINDOW_COUPLED_FOLD = false
  ).*
}

/// merged_bug_117 FALSIFY half (bughunt-2 slot 5): the pool-SHARED
/// streak map under a second reconciling pool — B's retain wipes A's
/// live streaks (persistentExhaustionEventuallyReports violated: the
/// report is livelocked) and an overlap intent double-steps to the
/// threshold on 2 own observations (streakCountsOwnObservations
/// violated). Exhaust axes on, no faults, ceiling 1.
module spawnCoherenceMultiPoolAsBuilt {
  import spawnCoherence(
    IS_BUILDER = true,
    CRD_PRESENT = true,
    CEILING = 1,
    ENABLE_RPC_FAULT = false,
    ENABLE_LEASE_FAULT = false,
    ENABLE_STALE_POLL = false,
    ENABLE_MAT_JOBS = false,
    ENABLE_RENDERED_DEADLINE = false,
    ENABLE_EXHAUST_POISON = true,
    ENABLE_EXHAUST_PERSIST = true,
    ENABLE_CANCEL_ASBUILT = false,
    ENABLE_CANCEL_CAUSE = false,
    ENABLE_YOUNG_LEADER_REAP = false,
    ENABLE_SYNTH_CLOSE = false,
    ENABLE_EXEC_PINNED = false,
    ENABLE_MULTI_POOL = true,
    ENABLE_STREAK_POOL_KEY = false,
    ENABLE_CANCEL_GENERATION = false,
    ENABLE_RETRY_BACKOFF = false,
    ENABLE_STREAK_CADENCE = false,
    STREAK_FLOOR_ENFORCED = false,
    STREAK_FLOOR = 2,
    WINDOW_COUPLED_FOLD = false
  ).*
}

/// merged_bug_117 HOLD half: PoolStreaks pool-keying — B's tick cannot
/// wipe or advance A's streaks; the poison verdict counts the
/// observing pool's own 3 observations. canReachPoison keeps the
/// threshold path non-vacuous under the multi-pool alphabet.
module spawnCoherenceMultiPool {
  import spawnCoherence(
    IS_BUILDER = true,
    CRD_PRESENT = true,
    CEILING = 1,
    ENABLE_RPC_FAULT = false,
    ENABLE_LEASE_FAULT = false,
    ENABLE_STALE_POLL = false,
    ENABLE_MAT_JOBS = false,
    ENABLE_RENDERED_DEADLINE = false,
    ENABLE_EXHAUST_POISON = true,
    ENABLE_EXHAUST_PERSIST = true,
    ENABLE_CANCEL_ASBUILT = false,
    ENABLE_CANCEL_CAUSE = false,
    ENABLE_YOUNG_LEADER_REAP = false,
    ENABLE_SYNTH_CLOSE = false,
    ENABLE_EXEC_PINNED = false,
    ENABLE_MULTI_POOL = true,
    ENABLE_STREAK_POOL_KEY = true,
    ENABLE_CANCEL_GENERATION = false,
    ENABLE_RETRY_BACKOFF = false,
    ENABLE_STREAK_CADENCE = false,
    STREAK_FLOOR_ENFORCED = false,
    STREAK_FLOOR = 2,
    WINDOW_COUPLED_FOLD = false
  ).*
}

/// bug_113 FALSIFY half (bughunt-2 slot 5): the cause-only cancel law
/// — a derivation cancelled and fast re-submitted respawns the
/// deterministic Job name inside the recently_closed window; the
/// not-yet-pulled fresh Job is cancel-selected
/// (cancelNeverDeletesPostCloseJob violated — the previously contained
/// 120-residual trace, promoted).
module spawnCoherenceCancelGenAsBuilt {
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
    ENABLE_CANCEL_CAUSE = true,
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
    WINDOW_COUPLED_FOLD = false
  ).*
}

/// bug_113 HOLD half: CancelTarget::bind's generation conjunct — the
/// respawned Job postdates its close and is structurally unselectable;
/// cancelArmDeletesOnlyCancelled is retained alongside.
module spawnCoherenceCancelGeneration {
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
    ENABLE_CANCEL_CAUSE = true,
    ENABLE_YOUNG_LEADER_REAP = false,
    ENABLE_SYNTH_CLOSE = false,
    ENABLE_EXEC_PINNED = false,
    ENABLE_MULTI_POOL = false,
    ENABLE_STREAK_POOL_KEY = false,
    ENABLE_CANCEL_GENERATION = true,
    ENABLE_RETRY_BACKOFF = false,
    ENABLE_STREAK_CADENCE = false,
    STREAK_FLOOR_ENFORCED = false,
    STREAK_FLOOR = 2,
    WINDOW_COUPLED_FOLD = false
  ).*
}

/// m073 HOLD half (bughunt-4 S1a): the wall-clock cadence floor over
/// the exhaust-persist alphabet — fold-skip retains and a wall that
/// may stand still. The poison verdict carries the floor conjunct
/// (STREAK_FLOOR_ENFORCED): a burst of reconcile ticks inside one
/// wall instant can step the streak to the threshold but can never
/// fire the verdict — poisonRespectsWallFloor holds. The
/// merged_bug_117 own-count/wipe laws keep their OWN exhaustive
/// regime (spawnCoherenceMultiPool): the MULTI_POOL x CADENCE product
/// is TLC-infeasible (measured: 2,347,503,711 generated /
/// 108,632,544 distinct / 38.6M queue still growing at the 1800s
/// budget kill) and the floor law does not interact with pool keying
/// — the verdict predicate is per-intent over the OWN-pool streak
/// either way. The falsify twin (calibration/controller-073-burst.qnt)
/// binds ENFORCED=false: the clock is tracked but the verdict stays
/// floor-blind (the as-built law) — VPoisonBurst latches.
module spawnCoherenceStreakCadence {
  import spawnCoherence(
    IS_BUILDER = true,
    CRD_PRESENT = true,
    CEILING = 1,
    ENABLE_RPC_FAULT = false,
    ENABLE_LEASE_FAULT = false,
    ENABLE_STALE_POLL = false,
    ENABLE_MAT_JOBS = false,
    ENABLE_RENDERED_DEADLINE = false,
    ENABLE_EXHAUST_POISON = true,
    ENABLE_EXHAUST_PERSIST = true,
    ENABLE_CANCEL_ASBUILT = false,
    ENABLE_CANCEL_CAUSE = false,
    ENABLE_YOUNG_LEADER_REAP = false,
    ENABLE_SYNTH_CLOSE = false,
    ENABLE_EXEC_PINNED = false,
    ENABLE_MULTI_POOL = false,
    ENABLE_STREAK_POOL_KEY = false,
    ENABLE_CANCEL_GENERATION = false,
    ENABLE_RETRY_BACKOFF = false,
    ENABLE_STREAK_CADENCE = true,
    STREAK_FLOOR_ENFORCED = true,
    STREAK_FLOOR = 2,
    WINDOW_COUPLED_FOLD = false
  ).*
}
