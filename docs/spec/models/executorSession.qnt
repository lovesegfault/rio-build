// -*- mode: Bluespec; -*-
//
// Model S of the executor-lifecycle campaign (#1, Phase 0 Stage B): the
// AS-BUILT scheduler⇄executor session state machine — registration,
// heartbeat, the bidi dispatch stream, completion intake, disconnect,
// reaping, the termination-report correlation/establishment lifecycle,
// draining flags, and failover convergence (P1 + P2 + P3's scheduler
// half + P4's flag semantics of executor-formal-design.md §2). The
// builder behind each slot is an honest-but-racy peer (it heartbeats
// what its slot holds, accepts at most one assignment, reports each
// accepted build at most once, never fabricates); its delivery
// choreography is Model D (executorDelivery.qnt), which discharges the
// "a started build's report is eventually delivered to some leader
// unless the pod dies first, and never twice as two different
// outcomes" assumption this model makes about its peer.
//
// Every action cites the production function it encodes (the 0a-pinned
// anchors are in docs/spec/models/executor-invariant-map.md). The
// invariant ↔ spec-rule linkage is the map's Stage-A section (F1–F6);
// check wiring and r[verify] markers live in nix/quint.nix, never here.
//
// WHAT IS MODELED
//   - Two executor slots and two derivations. Per-slot scheduler view
//     (ExecutorState): stream binding None/Open/Closed-but-bound (the
//     I-095 half-dead state), per-stream epoch, undelivered
//     reader-task disconnect events, auth_intent, systems-known (first
//     heartbeat), running_build, worker-time heartbeat bucket
//     (fresh/stale) plus the actor-stall credit, draining (admin),
//     draining_hb (worker-reported), store_degraded, last_completed,
//     phantom_suspect, warm, intent_id, entry presence. Per-slot
//     builder/pod truth: alive, claimed build, reported-yet, SIGTERM
//     flag, FUSE-degraded flag, observed lease generation (the B5
//     latch), the in-flight WorkAssignment.
//   - Per-derivation: status (Ready/Assigned/Running/Completed), its
//     PG mirror, assigned executor, attempt counter (the exec_id
//     carrier), the failed-on exclusion set (content imported from the
//     retry fold — see the checklist), starved-ticks counter, the
//     dispatched_cells ICE arming bit, the wedge flag, outputs-exist
//     ground truth.
//   - Actor maps: recently_disconnected (carrying the released
//     attempt, with a fresh/expired TTL bucket), dispatched_cells.
//   - The observation channels with environment-chosen interleaving:
//     heartbeat arrival, stream open / close / half-death, pod death,
//     controller termination report (with the race-ahead arm), builder
//     accept/reject of a push, and the housekeeping tick (stall credit
//     → reap → establishment sweep → backstop → cell sweep → dispatch
//     pass, atomic per the single-threaded actor).
//   - The leader environment as an abstract imported action (depose /
//     observe-loss / re-acquire / recovery / the 45 s reconcile
//     sweep), per the assume-guarantee checklist below. Bucketed time
//     only: heartbeat freshness, the 60 s correlation TTL and the
//     reconcile delay are buckets, never numeric clocks.
//
// WHAT IS NOT MODELED (deliberate scope boundaries, each priced)
//   - Budget/charge arithmetic and verdicts: every charge site appends
//     to the durable ledger and folds via decide() — that is
//     retryPolicy.qnt's subject (attemptsChargedOnce,
//     verdictMatchesFold, failoverPreservesHistory are imported, not
//     re-stated). This model tracks only the dedup-state lifecycle
//     those charges key on (F4) and adds the crashing slot to the
//     per-drv exclusion set at the classification/establishment points
//     the fold owns.
//   - Static-eligibility content (kind/system/features arithmetic),
//     warm-gate two-pass preference, prefetch internals, SLA solving,
//     resource-fit: pre-registered NOT-ENCODED (F5 carries the flags,
//     not the arithmetic). The warm bit is carried and flips at the
//     registration edge (the on_worker_registered empty-queue arm);
//     the PrefetchComplete ACK path is not a separate action.
//   - The log/data plane, BuildPhase, CancelBuild/admin-cancel paths,
//     the SLA estimator, hung-node detection (the node-regime
//     fallback decision is recorded in the invariant map), and the
//     controller's Job tick body (spawnCoherence.qnt owns it).
//   - try_send failure on a FULL (256-cap) channel: hard_filter
//     excludes closed streams (I-095), so the only as-built in-pass
//     send failure is channel-full; the fault-stream regime's
//     send-failure budget stands in for it and keeps
//     rollback_assignment reachable. Inline became-idle dispatch is a
//     latency carve-out, not a distinct obligation: the tick's
//     dispatch pass carries the bounded-offer obligation.
//   - Multi-replica state: exactly one scheduler replica's in-memory
//     state is modeled. The new leader after a failover is abstract
//     environment (workers observe its higher generation via heartbeat
//     fetch_max). Dual-belief writes by a deposed-but-unaware replica
//     are NOT claimed inert — that residual is bounded by the lease
//     model (boundedDualLeadership) and closed only by the
//     replacement's transaction-side fence (design §4.1); what this
//     model checks is that a replica which has OBSERVED deposition
//     (is_leader()=false) mutates nothing durable (the
//     standby-drops-writes gates).
//   - Pod respawn: a dead pod never comes back as the same identity
//     (replacement pods are fresh identities — the identity-freshness
//     fact owed to the retry model), so reconnect races involve only a
//     live builder's stream churn.
//
// ASSUME-GUARANTEE CHECKLIST (the foreign invariants this model relies
// on, named exactly as exported by their models — verified in-repo)
//   - leaderElection.qnt: atMostOneCASWinner (single-writer
//     acquisition), boundedDualLeadership (dual belief IS reachable in
//     degraded regimes, bounded with discovery armed),
//     staleLeaderHasStaleGeneration (concurrent believers are
//     generation-ordered — the bridge to the worker-side B5 latch /
//     sched.lease.generation-fence+2). neverDual is NOT imported: the
//     fault-leader regime here keeps the deposed-believer window
//     reachable (expect-violation witness noDeposedBeliever). The
//     acquire-epoch-vs-generation distinction is imported from the log
//     campaign's record + the recovery code, not from the lease model.
//   - retryPolicy.qnt: attemptsChargedOnce, verdictMatchesFold
//     (channel invariance), failoverPreservesHistory, and the atomic
//     appending transaction. This model owes the retry model the
//     environment facts it lists as not-modeled: the executor-map
//     lifecycle (F1), the dedup-state lifecycle (F4), identity
//     freshness (a crashed identity never re-registers), and the
//     static eligibility feeding placeable().
//   - spawnCoherence.qnt (Model J): ceilingRespected, reapSafety,
//     orphanRemoved, gateFailClosed, ackSoundness, ackCoversPending,
//     degradedPolarity. Single-Job-per-intent is an explicitly
//     ASSUMED-NOT-PROVEN environment constraint (spawnCoherence checks
//     only the no-ack-on-collision half plus the canReach409Dedupe
//     witness; ctrl.pool.spawn-once is untested there) — encoded here
//     as "each slot is one pod with one immutable auth_intent".
//   - nodeclaimLifecycle.qnt (Model N): nothing imported as a
//     precondition; listed only as the consumer of the dead_nodes
//     signal this subsystem produces (an owed obligation, F7/OA2).
//   - F7 obligations this model GUARANTEES to Model J/N rather than
//     checks: busy-accuracy of the executors map (running_build set at
//     record_assignment, cleared at completion/backstop/disconnect),
//     dispatched_cells ack-arming/clearing (armed by AckSpawnedIntents
//     — environment here — cleared at the registration edge,
//     disconnect, and the DAG-state sweep), termination-report
//     idempotency (the first-report-wins consumption modeled in F4).
//
// ENCODING DECISIONS
//   - One state record (`st`) threaded through pure defs, plus a latch
//     record (`viol`) for cross-action "never happens" invariants (the
//     spawnCoherence violations-set house pattern) and a reachability
//     record (`reach`) feeding the expect-violation witnesses. Latches
//     that the as-built encoding can never set (the gates exist at
//     every site) are kept so the Stage-C overrides have a stable
//     oracle; the Stage-B record in the invariant map lists which
//     invariants are by-construction vs checked.
//   - The housekeeping tick is ONE action (stall credit, reap,
//     establishment sweep, backstop, cell sweep, dispatch pass — in
//     handle_tick's order) because the actor is single-threaded: a
//     tick is one actor turn. Heartbeats, disconnect deliveries,
//     completions and controller reports interleave BETWEEN turns,
//     which is where the races live. A standby tick is a no-op
//     (sched.lease.standby-tick-noop) and is therefore not an action.
//   - The dispatch pass is a deterministic two-round greedy drain (drv
//     name order, then slot name order), which fully drains the
//     2-slot/2-drv bound. best_executor's scoring/warm preference is
//     placement preference, not safety: any eligible slot is a
//     faithful choice.
//   - Reader-task disconnect events are delivered oldest-epoch-first
//     (the actor mailbox is FIFO), which is what makes the I-056a
//     stale-disconnect-after-reconnect interleaving reachable.
//   - Ceilings are action preconditions (per-slot connect ceiling,
//     per-drv dispatch ceiling, fault budgets); the starved-ticks
//     counter is capped one past STARVE_BOUND so the violation state
//     stays representable without unbounded growth.
//
// The invariants, witnesses, regimes and their check wiring are
// documented at their definitions and in nix/quint.nix. Measured state
// counts, depths and wall-clocks live in the introducing commits'
// messages and the CI checks' transcripts, never in this file.
module executorSession {
  // --- 1. types ---------------------------------------------------------

  /// Scheduler-side view of a slot's stream binding.
  /// SNone     = no stream bound (stream_tx = None).
  /// SOpen     = stream bound and live.
  /// SClosedTx = stream bound but the channel is closed (the I-095
  ///             half-dead state: is_closed() = true, entry not yet
  ///             removed; also the post-close state while the reader's
  ///             ExecutorDisconnected is still queued).
  type StreamSt = SNone | SOpen | SClosedTx

  /// Worker-time heartbeat freshness bucket (fresh = within the 30 s
  /// HEARTBEAT_TIMEOUT_SECS window, measured net of stall credit).
  type HbBucket = HFresh | HStale

  /// Derivation status (the session-relevant slice of DerivationStatus).
  type DStatus = DReady | DAssigned | DRunning | DCompleted

  /// How the current attempt of a drv was resolved (for
  /// claimedSlotResolvesAtMostOnce bookkeeping).
  type Resolution = RUnresolved | RCompleted | RReturned

  /// Scheduler-side per-slot record (state/executor.rs::ExecutorState
  /// plus the entry-presence bit of DagActor::executors).
  type SchedSlot = {
    present: bool,
    stream: StreamSt,
    streamEpoch: int,
    connects: int,
    pendingDisc: Set[int],
    authIntent: str,
    systemsKnown: bool,
    running: str,
    hb: HbBucket,
    drainingAdmin: bool,
    drainingHb: bool,
    storeDegraded: bool,
    lastCompleted: str,
    phantomSuspect: str,
    warm: bool,
    intentId: str
  }

  /// Builder/pod ground truth per slot (the honest-but-racy peer).
  type BldSlot = {
    alive: bool,
    claimed: str,
    reported: bool,
    sigterm: bool,
    fuseDegraded: bool,
    observedGen: int,
    pendingAssign: str,
    killedByEnv: bool
  }

  /// One recently_disconnected entry (actor/mod.rs::DisconnectedAttempt).
  type RdEntry = {
    present: bool,
    drv: str,
    attempt: int,
    expired: bool
  }

  /// Per-derivation record (DAG-node session slice + PG mirror).
  type DrvRec = {
    status: DStatus,
    pgStatus: DStatus,
    assignedExec: str,
    attempt: int,
    resolution: Resolution,
    excluded: Set[str],
    starved: int,
    cellArmed: bool,
    wedged: bool,
    outputsInStore: bool
  }

  /// The whole protocol state.
  type SState = {
    sched: str -> SchedSlot,
    bld: str -> BldSlot,
    drv: str -> DrvRec,
    rd: str -> RdEntry,
    believedLeader: bool,
    actualHolder: bool,
    generation: int,
    newLeaderGen: int,
    recoveryComplete: bool,
    reconcilePending: bool,
    podDeathsLeft: int,
    failoversLeft: int,
    persistFaultsLeft: int,
    msgLossLeft: int,
    sendFailLeft: int,
    halfDeathLeft: int,
    streamDropLeft: int,
    hbDropLeft: int,
    dupMsgLeft: int,
    wedgesLeft: int,
    stallsLeft: int,
    fuseFlipsLeft: int,
    adminDrainsLeft: int,
    sigtermsLeft: int
  }

  /// Latches for cross-action "this must never happen" invariants.
  /// Some are settable only by the Stage-C calibration overrides (the
  /// as-built guards make them unreachable); see the invariant map's
  /// Stage-B encoding notes for the by-construction list.
  type Viol = {
    secondLiveStream: bool,
    staleEpochMutation: bool,
    heartbeatCreatedEntry: bool,
    doubleResolution: bool,
    fabricatedCompletion: bool,
    reapedFreshSlot: bool,
    staleSlotSurvivedTick: bool,
    rdInsertNotMidBuild: bool,
    establishedBeforeTtl: bool,
    nonPromotingConsumed: bool,
    offeredUnrunnable: bool,
    offeredUnregistered: bool,
    starvedPastBound: bool,
    halfRecordedAssignment: bool,
    durableWriteWhileDeposed: bool,
    adoptAndReset: bool,
    unresolvedAfterSweep: bool
  }

  /// Reachability ghosts feeding the expect-violation witnesses. Only
  /// genuinely transient events are stored (one bit each); every other
  /// witness is a predicate over the current state, so the ghost record
  /// does not multiply the reachable state space.
  type Reach = {
    phantomDrained: bool,
    adoptHappened: bool,
    reapHappened: bool,
    deathByTwoChannels: bool
  }

  // --- 2. constants (const = overridable per regime) ---------------------

  const EXECS: Set[str]
  const DRVS: Set[str]

  /// Per-slot accepted-connect ceiling (the epoch/reconnect ceiling).
  const EPOCH_CEIL: int
  /// Per-drv dispatch ceiling (attempt-counter bound).
  const DISPATCH_CEIL: int
  /// Bounded-offer bound for F5 (consecutive starved leader ticks).
  const STARVE_BOUND: int

  /// Fault budgets (0 disables the fault class in a regime).
  const POD_DEATHS: int     // fault-process: pod death (with/without report)
  const WEDGES: int         // fault-process: wedge (no report, no death)
  const FAILOVERS: int      // fault-leader: depose/observe/reacquire cycle
  const PERSIST_FAULTS: int // fault-persist: a persist_status write fails
  const MSG_LOSS: int       // fault-stream: in-flight WorkAssignment lost
  const SEND_FAILS: int     // fault-stream: try_send fails (channel full) -> rollback
  const HALF_DEATHS: int    // fault-stream: bridge exit (half-dead stream)
  const STREAM_DROPS: int   // fault-stream: live builder's connection drops
  const HB_DROPS: int       // fault-stream: a live builder's heartbeats stop arriving
  const DUP_MSGS: int       // fault-stream: duplicate WorkAssignment delivery
  const STALLS: int         // actor stall (merge-time FMP) feeding the credit path
  const FUSE_FLIPS: int     // FUSE breaker open/close transitions on a builder
  const ADMIN_DRAINS: int   // operator/controller DrainExecutor calls
  const SIGTERMS: int       // SIGTERM drain transitions across the fleet

  // --- 3. pure defs -----------------------------------------------------

  pure def emptySched: SchedSlot = {
    present: false, stream: SNone, streamEpoch: 0, connects: 0,
    pendingDisc: Set(), authIntent: "", systemsKnown: false, running: "",
    hb: HFresh, drainingAdmin: false, drainingHb: false,
    storeDegraded: false, lastCompleted: "", phantomSuspect: "",
    warm: false, intentId: ""
  }

  /// Deterministic pick from a string set ("" when empty). Strings are
  /// opaque in Quint (no ordering), so the pick is "the first element
  /// the fold visits" — deterministic per evaluation, which is all the
  /// greedy dispatch pass needs: best_executor's scoring is placement
  /// preference, not safety, so any eligible slot is a faithful choice.
  pure def pickStr(s: Set[str]): str =
    s.fold("", (b, x) => if (b == "") x else b)

  /// Oldest element of a non-empty epoch set (epochs start at 1) —
  /// FIFO-ordered disconnect delivery.
  pure def leastInt(s: Set[int]): int =
    s.fold(0, (b, x) => if (b == 0 or x < b) x else b)

  /// is_registered(): stream bound AND a heartbeat landed
  /// (state/executor.rs::is_registered).
  pure def isRegistered(s: SchedSlot): bool =
    s.present and s.stream != SNone and s.systemsKnown

  /// is_draining(): scheduler-side OR worker-side flag
  /// (state/executor.rs::is_draining).
  pure def isDraining(s: SchedSlot): bool = s.drainingAdmin or s.drainingHb

  /// hard_filter / rejection_reason for one (slot, drv) pair, minus the
  /// NOT-ENCODED kind/system/feature/resource arithmetic (uniform
  /// fleet): registered, stream live (I-095), not draining, not
  /// degraded, slot free, intent reservation honoured, not in the
  /// drv's exclusion set (assignment.rs::rejection_reason).
  pure def eligibleFor(stx: SState, e: str, dn: str): bool = and {
    isRegistered(stx.sched.get(e)),
    stx.sched.get(e).stream == SOpen,
    not(isDraining(stx.sched.get(e))),
    not(stx.sched.get(e).storeDegraded),
    stx.sched.get(e).running == "",
    stx.sched.get(e).intentId == "" or stx.sched.get(e).intentId == dn,
    not(stx.drv.get(dn).excluded.contains(e))
  }

  /// Is dn dispatchable at all (Ready and under its attempt ceiling)?
  pure def dispatchable(stx: SState, dn: str): bool =
    stx.drv.get(dn).status == DReady and stx.drv.get(dn).attempt < DISPATCH_CEIL

  /// reset_to_ready + persist_status(Ready) for one drv — the repair
  /// requeue arm shared by reassign_derivations, drain_phantoms and the
  /// backstop. Records the repair resolution of the current attempt.
  pure def requeue(stx: SState, dn: str): SState =
    val d = stx.drv.get(dn)
    if (d.status == DAssigned or d.status == DRunning)
      { ...stx, drv: stx.drv.set(dn, { ...d,
          status: DReady, pgStatus: DReady, assignedExec: "",
          wedged: false,
          resolution: if (d.resolution == RUnresolved) RReturned else d.resolution }) }
    else stx

  /// DagActor::handle_executor_disconnected's effects for a
  /// CURRENT-epoch disconnect (also the reaper's synthesized
  /// disconnect): remove the entry, clear the pod's dispatched_cells
  /// cell, record the mid-build correlation entry carrying the
  /// released attempt, and (leader only) requeue the running build via
  /// reassign_derivations.
  pure def applyDisconnect(stx: SState, e: str): SState =
    val s = stx.sched.get(e)
    val midBuild = s.running != "" and s.lastCompleted != s.running
    val dn = s.running
    val st1 = {
      ...stx,
      sched: stx.sched.set(e, emptySched),
      drv: stx.drv.keys().mapBy(d =>
        val rec = stx.drv.get(d)
        if (s.authIntent == d) { ...rec, cellArmed: false } else rec),
      // the I-197 last_completed discriminator gates ONLY the
      // correlation-entry insert (an expected one-shot exit and a
      // race-ahead-classified death need no entry) ...
      rd: if (midBuild)
            stx.rd.set(e, {
              present: true, drv: dn,
              attempt: stx.drv.get(dn).attempt, expired: false })
          else stx.rd
    }
    // ... while reassign_derivations runs for whatever was in
    // running_build regardless (leader-gated; standby drops it and the
    // new leader's recovery owns the derivation).
    if (dn != "" and stx.believedLeader) requeue(st1, dn) else st1

  /// tick_sweep_recently_disconnected's establishment for one expired
  /// entry: consume it, classify the released attempt as an unreported
  /// executor crash (charge imported), and add the crashed slot to the
  /// drv's fold-backed exclusion set (refresh_retry_view → the
  /// failed-on clause hard_filter consumes).
  pure def establish(stx: SState, e: str): SState =
    val entry = stx.rd.get(e)
    { ...stx,
      rd: stx.rd.set(e, { ...entry, present: false }),
      drv: stx.drv.set(entry.drv, { ...stx.drv.get(entry.drv),
        excluded: stx.drv.get(entry.drv).excluded.union(Set(e)) }) }

  /// One greedy round of the dispatch pass: pick the least dispatchable
  /// drv that has an eligible slot, assign it to its least eligible
  /// slot (assign_to_worker: Ready→Assigned, mint the next attempt,
  /// persist, set worker.running_build, push the WorkAssignment).
  pure def dispatchRound(stx: SState): SState =
    val cands = DRVS.filter(d => dispatchable(stx, d) and EXECS.exists(e => eligibleFor(stx, e, d)))
    if (cands.size() == 0) stx
    else
      val d = pickStr(cands)
      val e = pickStr(EXECS.filter(e2 => eligibleFor(stx, e2, d)))
      { ...stx,
        drv: stx.drv.set(d, { ...stx.drv.get(d),
          status: DAssigned, pgStatus: DAssigned, assignedExec: e,
          attempt: stx.drv.get(d).attempt + 1, resolution: RUnresolved,
          starved: 0 }),
        sched: stx.sched.set(e, { ...stx.sched.get(e), running: d }),
        bld: stx.bld.set(e, { ...stx.bld.get(e), pendingAssign: d }) }

  // --- 4. state variables -----------------------------------------------

  var st: SState
  var viol: Viol
  var reach: Reach

  // --- 5. derived state ---------------------------------------------------

  /// The deposed-believer window: this replica no longer holds the
  /// lease but has not yet observed the loss.
  def deposedBeliever = st.believedLeader and not(st.actualHolder)

  // --- 6. invariants ------------------------------------------------------
  // Names per design §3.3 / the invariant map's Stage-A section. The
  // latch-backed ones assert "the latch never fired"; the structural
  // ones are direct state predicates.

  // F1 — session identity.
  /// At most one live BuildExecution stream is bound to an executor id:
  /// an accept while the existing binding is still live never happens
  /// (handle_worker_connected's live-stream hijack guard).
  val atMostOneLiveStreamPerExecutor = not(viol.secondLiveStream)
  /// A disconnect from epoch N mutates nothing once the slot is bound
  /// to epoch M > N (handle_executor_disconnected's I-056a guard); the
  /// reaper synthesizes its disconnect at the CURRENT epoch.
  val staleStreamEventsAreInert = not(viol.staleEpochMutation)
  /// No dispatch to a slot lacking either registration half, and
  /// heartbeats never create session entries (I-048b).
  val registrationRequiresBothHalves = and {
    not(viol.offeredUnregistered),
    not(viol.heartbeatCreatedEntry)
  }

  // F2 — outcome delivery (scheduler half).
  /// An accepted assignment is never counted as both a leader-observed
  /// completion and a repair return-to-Ready, and no second resolution
  /// is recorded after either (completion.rs idempotency / staleness
  /// guards + the repair-precedence discipline).
  val claimedSlotResolvesAtMostOnce = not(viol.doubleResolution)
  /// In every leader-view state where an accepted assignment is
  /// neither completed nor returned to Ready, at least one repair
  /// mechanism is armed for it: the builder still owes the report
  /// (Model D's obligation), the in-flight push can still land, the
  /// slot bookkeeping still binds it (heartbeat reconcile / phantom /
  /// reaper family), a correlation entry is armed, the wedge backstop
  /// applies, or the post-failover reconcile is pending.
  val unresolvedClaimHasRepairArmed =
    DRVS.forall(d =>
      val rec = st.drv.get(d)
      val ae = rec.assignedExec
      or {
        not(st.believedLeader),
        rec.status != DAssigned and rec.status != DRunning,
        if (ae == "") false else and {
          st.bld.get(ae).alive,
          st.bld.get(ae).claimed == d
        },
        if (ae == "") false else st.bld.get(ae).pendingAssign == d,
        if (ae == "") false else and {
          st.sched.get(ae).present,
          st.sched.get(ae).running == d
        },
        EXECS.exists(e => st.rd.get(e).present and st.rd.get(e).drv == d),
        rec.wedged,
        st.reconcilePending
      })
  /// No repair path invents a completion the worker did not report
  /// (the store-probe adopt requires outputs; the phantom drain and
  /// orphan reset return work to Ready).
  val noFabricatedCompletion = not(viol.fabricatedCompletion)

  // F3 — liveness calibration.
  /// A slot whose worker heartbeated within the timeout bucket measured
  /// in worker time (net of stall credit) is never reaped.
  val noReapWhileFreshInWorkerTime = not(viol.reapedFreshSlot)
  /// Once a slot is past the timeout bucket in worker time, a leader
  /// reaper tick taken in that state reaps it (enabled-implies-fires);
  /// only a heartbeat refreshes the bucket.
  val silentSlotReapArmed = not(viol.staleSlotSurvivedTick)

  // F4 — death attribution (the dedup-state lifecycle; charges imported).
  /// A recently_disconnected entry is created only by a mid-build
  /// disconnect, consumed by exactly one of {classifying report,
  /// establishment} (structural: consumption removes it), and a
  /// non-promoting report neither consumes nor establishes.
  val correlationEntryLifecycle = and {
    not(viol.rdInsertNotMidBuild),
    not(viol.nonPromotingConsumed)
  }
  /// Establishment fires only when the correlation window has closed
  /// with no classifying report.
  val establishmentOnlyAfterWindowCloses = not(viol.establishedBeforeTtl)

  // F5 — eligibility coherence.
  /// No assignment is pushed to a slot that is draining, degraded, at
  /// capacity, closed-stream, intent-reserved elsewhere, unregistered,
  /// or in the drv's exclusion set.
  val neverOfferUnrunnableWork = and {
    not(viol.offeredUnrunnable),
    not(viol.offeredUnregistered)
  }
  /// A Ready drv with an eligible registered free slot is offered
  /// within STARVE_BOUND consecutive leader ticks.
  val eligibleWorkOfferedWithinBound = not(viol.starvedPastBound)
  /// A failed push leaves no half-recorded assignment
  /// (dispatch.rs::rollback_assignment): the rollback restores status,
  /// PG mirror, slot bookkeeping and the assignment binding together —
  /// a Ready drv never retains an assigned executor. (The worker-side
  /// at-capacity claim a heartbeat may re-assert for a cancelled build
  /// is the documented false-at-capacity window, not a half-recorded
  /// assignment, so it is deliberately not constrained here.)
  val rollbackRestoresExactly = and {
    not(viol.halfRecordedAssignment),
    DRVS.forall(d =>
      st.drv.get(d).status != DReady or st.drv.get(d).assignedExec == "")
  }

  // F6 — failover convergence.
  /// Once this replica has OBSERVED deposition (is_leader()=false), its
  /// session events mutate nothing durable (the standby-drops-writes
  /// gates). The unobserved dual-belief window is the lease model's
  /// bounded residual, kept reachable by the deposed-believer witness.
  val deposedLeaderSessionEventsAreInert = not(viol.durableWriteWhileDeposed)
  /// Every recovery/reconcile action on a PG-Assigned/Running drv
  /// either adopts it or resets it to Ready, never both, never
  /// fabricating a completion, and the sweep leaves no dead-worker drv
  /// unresolved.
  val convergenceToGroundTruth = and {
    not(viol.adoptAndReset),
    not(viol.unresolvedAfterSweep),
    not(viol.fabricatedCompletion)
  }

  /// Ceiling/budget sanity (the ceiling-as-precondition discipline).
  val boundsOK = and {
    st.podDeathsLeft >= 0, st.failoversLeft >= 0, st.persistFaultsLeft >= 0,
    st.msgLossLeft >= 0, st.sendFailLeft >= 0, st.halfDeathLeft >= 0,
    st.streamDropLeft >= 0, st.hbDropLeft >= 0, st.dupMsgLeft >= 0,
    st.wedgesLeft >= 0, st.stallsLeft >= 0,
    st.fuseFlipsLeft >= 0, st.adminDrainsLeft >= 0, st.sigtermsLeft >= 0,
    EXECS.forall(e => st.sched.get(e).connects <= EPOCH_CEIL),
    DRVS.forall(d => st.drv.get(d).attempt <= DISPATCH_CEIL),
    DRVS.forall(d => st.drv.get(d).starved <= STARVE_BOUND + 1)
  }

  /// All-invariants conjunction for the CI checks (re-run
  /// per-invariant to identify the culprit when red).
  val allInvariants = and {
    boundsOK,
    atMostOneLiveStreamPerExecutor,
    staleStreamEventsAreInert,
    registrationRequiresBothHalves,
    claimedSlotResolvesAtMostOnce,
    unresolvedClaimHasRepairArmed,
    noFabricatedCompletion,
    noReapWhileFreshInWorkerTime,
    silentSlotReapArmed,
    correlationEntryLifecycle,
    establishmentOnlyAfterWindowCloses,
    neverOfferUnrunnableWork,
    eligibleWorkOfferedWithinBound,
    rollbackRestoresExactly,
    deposedLeaderSessionEventsAreInert,
    convergenceToGroundTruth
  }

  // --- 6b. expect-violation witnesses (non-vacuity probes) ----------------
  // Each is "the interesting state is never reached"; the wired check
  // passes only when the checker violates it (mkQuintWitnessCheck).
  // Where the contended condition persists in the state, the witness is
  // a predicate over the current state (no stored ghost); the four
  // transient events use the reach ghosts.

  /// A confirmed phantom was drained (assignment accepted by the
  /// scheduler, never reported by the worker, two heartbeat strikes).
  val noPhantomDrain = not(reach.phantomDrained)
  /// The I-095 half-dead binding (stream_tx closed, entry retained) is
  /// reachable.
  val noHalfDeadStream =
    not(EXECS.exists(e => st.sched.get(e).present and st.sched.get(e).stream == SClosedTx))
  /// A stale-epoch disconnect is queued behind a re-bound entry (the
  /// I-056a late-half precondition).
  val noStaleEpochDisconnect =
    not(EXECS.exists(e => and {
      st.sched.get(e).present,
      st.sched.get(e).pendingDisc.exists(ep => ep != st.sched.get(e).streamEpoch)
    }))
  /// A heartbeat adopt re-learned an in-flight build the scheduler lost.
  val noAdopt = not(reach.adoptHappened)
  /// A reap happened in a behaviour where an actor stall also happened
  /// (the stall credit is what keeps the two compatible).
  val noReapAfterStall = not(st.stallsLeft < STALLS and reach.reapHappened)
  /// One pod death was observed by both the disconnect channel and the
  /// controller's classifying report.
  val noDeathByTwoChannels = not(reach.deathByTwoChannels)
  /// The lease moved away while an accepted assignment was in flight.
  val noFailoverWithInflight =
    not(not(st.actualHolder) and DRVS.exists(d =>
      st.drv.get(d).status == DAssigned or st.drv.get(d).status == DRunning))
  /// A draining slot coexists with a build its builder still owes.
  val noDrainWithPendingCompletion =
    not(EXECS.exists(e => and {
      isDraining(st.sched.get(e)),
      st.bld.get(e).claimed != "",
      not(st.bld.get(e).reported)
    }))
  /// The deposed-believer window (F6 non-vacuity).
  val noDeposedBeliever = not(deposedBeliever)
  /// An expired correlation entry is awaiting establishment.
  val noEstablishment =
    not(EXECS.exists(e => st.rd.get(e).present and st.rd.get(e).expired))
  /// A failed push was rolled back (the channel-full rollback path).
  val noRollback = not(st.sendFailLeft < SEND_FAILS)
  /// The race-ahead arm of the controller report is enabled (report
  /// before disconnect delivery).
  val noRaceAheadReport =
    not(EXECS.exists(e => and {
      not(st.bld.get(e).alive),
      st.bld.get(e).killedByEnv,
      not(st.rd.get(e).present),
      st.sched.get(e).present,
      st.sched.get(e).running != ""
    }))

  // --- 7. actions ----------------------------------------------------------

  // --- 7a. registration / stream lifecycle --------------------------------

  /// grpc/executor_service.rs::build_execution +
  /// DagActor::handle_worker_connected, accept path: allocate the
  /// per-stream epoch (STREAM_EPOCH_SEQ), bind the stream, stamp
  /// auth_intent on the first token-bearing connect, clear stale
  /// scheduler-side draining/store_degraded on reconnect (I-056a; NOT
  /// draining_hb), fire the registration edge (warm-gate empty-queue
  /// arm) if the heartbeat half already landed. The hijack guards
  /// (live-stream, intent-mismatch) are rejection no-ops, so only the
  /// accepted connect is an action; their content is the precondition.
  action connectAccept(e: str, presented: str): bool = {
    val s0 = st.sched.get(e)
    val isReconnect = s0.present
    // per-slot epoch numbering (the production STREAM_EPOCH_SEQ is
    // process-global, but the stale-disconnect guard only compares
    // epochs of the same slot, so per-slot numbering is equivalent and
    // keeps the state space free of cross-slot ordering artifacts)
    val newEpoch = s0.connects + 1
    all {
      st.bld.get(e).alive,
      s0.connects < EPOCH_CEIL,
      // ensure_leader at stream open
      st.believedLeader,
      // hijack guard 1: an existing LIVE binding rejects the new stream
      s0.stream != SOpen,
      // hijack guard 2: a presented intent must match the stored one
      or { s0.authIntent == "", presented == "", presented == s0.authIntent },
      st' = { ...st,
        sched: st.sched.set(e, { ...s0,
          present: true,
          stream: SOpen,
          streamEpoch: newEpoch,
          connects: s0.connects + 1,
          authIntent: if (s0.authIntent == "") presented else s0.authIntent,
          intentId: if (presented != "") presented else s0.intentId,
          drainingAdmin: if (isReconnect) false else s0.drainingAdmin,
          storeDegraded: if (isReconnect) false else s0.storeDegraded,
          warm: if (s0.systemsKnown) true else s0.warm }) },
      viol' = { ...viol,
        secondLiveStream: viol.secondLiveStream or (isReconnect and s0.stream == SOpen) },
      reach' = reach
    }
  }

  /// The bridge task exits while the reader stays up: stream_tx closes
  /// without an ExecutorDisconnected — the I-095 half-dead state the
  /// heartbeat WARN observes and rejection_reason("stream-closed")
  /// excludes from dispatch. fault-stream budget.
  action streamHalfDie(e: str): bool = all {
    st.halfDeathLeft > 0,
    st.sched.get(e).present,
    st.sched.get(e).stream == SOpen,
    st' = { ...st,
      sched: st.sched.set(e, { ...st.sched.get(e), stream: SClosedTx }),
      halfDeathLeft: st.halfDeathLeft - 1 },
    viol' = viol,
    reach' = reach
  }

  /// A live builder's connection drops (network/server churn): the
  /// reader queues ExecutorDisconnected{stream_epoch}
  /// (executor_service.rs worker-stream-reader tail); the builder keeps
  /// its build and will reconnect. fault-stream budget.
  action streamDrop(e: str): bool = {
    val s0 = st.sched.get(e)
    all {
      st.streamDropLeft > 0,
      st.bld.get(e).alive,
      s0.present,
      s0.stream == SOpen or s0.stream == SClosedTx,
      st' = { ...st,
        streamDropLeft: st.streamDropLeft - 1,
        sched: st.sched.set(e, { ...s0,
          stream: SClosedTx,
          pendingDisc: s0.pendingDisc.union(Set(s0.streamEpoch)) }),
        bld: st.bld.set(e, { ...st.bld.get(e), pendingAssign: "" }) },
      viol' = viol,
      reach' = reach
    }
  }

  /// The builder process exits voluntarily — the one-shot exit after
  /// its completion flushed, the SIGTERM drain exit, or the I-116 idle
  /// exit (rio-builder runtime/mod.rs::run teardown). The stream
  /// closes; the reader queues the disconnect.
  action builderExit(e: str): bool = {
    val s0 = st.sched.get(e)
    val b = st.bld.get(e)
    all {
      b.alive,
      b.claimed == "",
      or { b.reported, b.sigterm },
      st' = { ...st,
        bld: st.bld.set(e, { ...b, alive: false, pendingAssign: "" }),
        sched:
          if (s0.present and s0.stream != SNone)
            st.sched.set(e, { ...s0,
              stream: SClosedTx,
              pendingDisc: s0.pendingDisc.union(Set(s0.streamEpoch)) })
          else st.sched },
      viol' = viol,
      reach' = reach
    }
  }

  /// DagActor::handle_executor_disconnected — the oldest queued
  /// reader-task disconnect is delivered (the actor mailbox is FIFO).
  /// A stale-epoch event (the entry already re-bound to a newer
  /// stream) is inert — the I-056a late half; a current-epoch event
  /// removes the entry, records the mid-build correlation entry and
  /// (leader only) requeues the running build.
  action deliverDisconnect(e: str): bool = {
    val s0 = st.sched.get(e)
    val ep = leastInt(s0.pendingDisc)
    val stale = not(s0.present) or s0.streamEpoch != ep
    val cleared = { ...s0, pendingDisc: s0.pendingDisc.exclude(Set(ep)) }
    all {
      s0.pendingDisc.size() > 0,
      if (stale) all {
        st' = { ...st, sched: st.sched.set(e, cleared) },
        viol' = viol,
        reach' = reach
      } else all {
        st' = applyDisconnect({ ...st, sched: st.sched.set(e, cleared) }, e),
        viol' = viol,
        reach' = reach
      }
    }
  }

  // --- 7b. heartbeat -------------------------------------------------------

  /// DagActor::handle_heartbeat — one accepted heartbeat from slot e's
  /// live builder: the I-048b entry-existence drop and the auth_intent
  /// spoof guard are rejection no-ops (preconditions); the accepted
  /// path refreshes the worker-time bucket and the worker-reported
  /// flags, runs reconcile_running_build (keep / adopt / two-strike
  /// phantom), downgrades a stale intent reservation, fires the
  /// registration edge (on_worker_registered + the §13a ICE clear),
  /// and lets the mod.rs Heartbeat arm drain confirmed phantoms
  /// (leader-gated drain_phantoms). The heartbeat unary requires the
  /// serving replica to believe it leads (ensure_leader); the B5 latch
  /// observes the response generation (fetch_max).
  action heartbeat(e: str): bool = {
    val s0 = st.sched.get(e)
    val b = st.bld.get(e)
    val reported = b.claimed
    val wasRegistered = isRegistered(s0)
    // keep the scheduler-known build if still in flight ON THIS executor
    val prevKept =
      if (s0.running == "") ""
      else if (and {
        st.drv.get(s0.running).status == DAssigned or st.drv.get(s0.running).status == DRunning,
        st.drv.get(s0.running).assignedExec == e
      }) s0.running else ""
    val adopting = prevKept == "" and reported != "" and not(s0.drainingAdmin)
    val reconciled = if (prevKept != "") prevKept else reported
    val suspect = if (reconciled != "" and reported != reconciled) reconciled else ""
    val confirmedPhantom = suspect != "" and s0.phantomSuspect == suspect
    val newRunning = if (confirmedPhantom) "" else reconciled
    // adopt_heartbeat_build: DAG-side re-claim only when the node is
    // Ready (recovery reset it); Assigned/Running elsewhere is a
    // conflict (both run, do not steal); terminal claims are stale.
    val adoptDrv =
      if (not(adopting)) ""
      else if (st.drv.get(reported).status == DReady) reported else ""
    // drain_phantoms (leader-gated, ownership-checked)
    val drainPhantom = and {
      confirmedPhantom,
      st.believedLeader,
      st.drv.get(suspect).assignedExec == e,
      st.drv.get(suspect).status == DAssigned or st.drv.get(suspect).status == DRunning
    }
    all {
      st.bld.get(e).alive,
      st.believedLeader,
      // I-048b: a heartbeat for an executor with no entry is dropped and
      // creates nothing — the drop is a stutter, so only the accepted
      // heartbeat is an action.
      s0.present,
      st' = { ...st,
        sched: st.sched.set(e, { ...s0,
          systemsKnown: true,
          hb: HFresh,
          running: newRunning,
          drainingHb: b.sigterm,
          storeDegraded: b.fuseDegraded,
          intentId:
            if (s0.intentId == "") ""
            else if (st.drv.get(s0.intentId).status == DReady) s0.intentId else "",
          phantomSuspect: if (confirmedPhantom) "" else suspect,
          warm: if (not(wasRegistered) and s0.stream != SNone) true else s0.warm }),
        drv: {
          val afterAdopt =
            if (adoptDrv == "") st.drv
            else st.drv.set(adoptDrv, { ...st.drv.get(adoptDrv),
              status: DAssigned, assignedExec: e,
              // adoption re-establishes the claim: the resolution
              // bookkeeping starts over (a later completion is the
              // re-opened claim's first resolution, not a second one)
              resolution: RUnresolved })
          if (drainPhantom)
            afterAdopt.set(suspect, { ...afterAdopt.get(suspect),
              status: DReady, pgStatus: DReady, assignedExec: "",
              resolution:
                if (afterAdopt.get(suspect).resolution == RUnresolved) RReturned
                else afterAdopt.get(suspect).resolution })
          else afterAdopt
        },
        bld: st.bld.set(e, { ...b,
          observedGen: if (st.generation > b.observedGen) st.generation else b.observedGen })
      },
      viol' = { ...viol,
        doubleResolution: viol.doubleResolution or
          (drainPhantom and st.drv.get(suspect).resolution != RUnresolved) },
      reach' = { ...reach,
        adoptHappened: reach.adoptHappened or adoptDrv != "",
        phantomDrained: reach.phantomDrained or confirmedPhantom }
    }
  }

  /// Worker time passes without a heartbeat: the slot's freshness
  /// bucket ages to stale. A dead pod's slot ages for free (the
  /// silence IS the signal); a live builder's bucket ages only when
  /// its heartbeats stop arriving (fault-stream HB_DROPS budget).
  action hbAge(e: str): bool = all {
    st.sched.get(e).present,
    st.sched.get(e).hb == HFresh,
    or { not(st.bld.get(e).alive), st.hbDropLeft > 0 },
    st' = { ...st,
      sched: st.sched.set(e, { ...st.sched.get(e), hb: HStale }),
      hbDropLeft: if (st.bld.get(e).alive) st.hbDropLeft - 1 else st.hbDropLeft },
    viol' = viol,
    reach' = reach
  }

  /// An actor-side stall (merge-time FindMissingPaths, the I-178
  /// family): housekeeping credits every slot's last_heartbeat by the
  /// stall (housekeeping.rs::credit_heartbeats_for_stall) so the
  /// reaper measures only worker silence. The heartbeat bucket here IS
  /// worker time, so the credit is implicit: the stall itself never
  /// ages a bucket — this action only pins the interleaving for the
  /// reap-after-stall witness.
  action actorStall: bool = all {
    st.stallsLeft > 0,
    st' = { ...st, stallsLeft: st.stallsLeft - 1 },
    viol' = viol,
    reach' = reach
  }

  // --- 7c. dispatch delivery / builder lifecycle ---------------------------

  /// rio-builder runtime/mod.rs::handle_assignment — the builder
  /// receives the in-flight WorkAssignment: reject when the carried
  /// generation is stale (is_stale_assignment, the B5 latch), when
  /// draining (SIGTERM), or when the single BuildSlot is busy
  /// (try_claim); no ACK either way. Accept claims the slot.
  action builderRecvAssign(e: str): bool = {
    val b = st.bld.get(e)
    val dn = b.pendingAssign
    val accept = and {
      not(st.generation < b.observedGen),
      not(b.sigterm),
      b.claimed == ""
    }
    all {
      b.alive,
      dn != "",
      st' = { ...st,
        bld: st.bld.set(e, { ...b,
          pendingAssign: "",
          claimed: if (accept) dn else b.claimed,
          reported: if (accept) false else b.reported }) },
      viol' = viol,
      reach' = reach
    }
  }

  /// The in-flight WorkAssignment is lost in transit (fault-stream).
  action loseAssignMsg(e: str): bool = all {
    st.msgLossLeft > 0,
    st.bld.get(e).pendingAssign != "",
    st' = { ...st,
      bld: st.bld.set(e, { ...st.bld.get(e), pendingAssign: "" }),
      msgLossLeft: st.msgLossLeft - 1 },
    viol' = viol,
    reach' = reach
  }

  /// A duplicate WorkAssignment delivery for the build the slot already
  /// claimed (stream-message duplication): the builder's slot-busy
  /// reject absorbs it. fault-stream budget.
  action dupAssignDeliver(e: str): bool = all {
    st.dupMsgLeft > 0,
    st.bld.get(e).alive,
    st.bld.get(e).claimed != "",
    st.bld.get(e).pendingAssign == "",
    st' = { ...st,
      bld: st.bld.set(e, { ...st.bld.get(e), pendingAssign: st.bld.get(e).claimed }),
      dupMsgLeft: st.dupMsgLeft - 1 },
    viol' = viol,
    reach' = reach
  }

  /// dispatch.rs::rollback_assignment — the try_send into the worker
  /// stream failed (channel full; fault-stream budget): clear
  /// worker.running_build, reset the drv to Ready, persist Ready,
  /// delete the assignment/execution rows. Modeled as the immediate
  /// undo of a push whose message never reached the builder.
  action rollbackAssign(e: str): bool = {
    val s0 = st.sched.get(e)
    val dn = st.bld.get(e).pendingAssign
    all {
      st.sendFailLeft > 0,
      dn != "",
      s0.running == dn,
      st' = {
        val st1 = { ...st,
          sendFailLeft: st.sendFailLeft - 1,
          sched: st.sched.set(e, { ...s0, running: "" }),
          bld: st.bld.set(e, { ...st.bld.get(e), pendingAssign: "" }) }
        { ...st1, drv: st1.drv.set(dn, { ...st1.drv.get(dn),
            status: DReady, pgStatus: DReady, assignedExec: "",
            resolution: RUnresolved }) }
      },
      viol' = viol,
      reach' = reach
    }
  }

  /// The builder's accepted build reaches its terminal outcome and the
  /// CompletionReport ARRIVES at the serving replica (the delivery
  /// choreography is Model D's subject; arrival is one step here).
  /// DagActor::handle_completion: free the slot capacity and stamp
  /// last_completed + the I-188 one-shot drain BEFORE any early
  /// return, then the idempotency / staleness guards, then the
  /// terminal transition (success) or the requeue verdict (failure —
  /// verdict arithmetic imported from the retry fold, the slot joins
  /// the exclusion set).
  action builderComplete(e: str, ok: bool): bool = {
    val b = st.bld.get(e)
    val dn = b.claimed
    val s0 = st.sched.get(e)
    all {
      b.alive,
      dn != "",
      not(b.reported),
      not(st.drv.get(dn).wedged),
      st.believedLeader,
      s0.present,
      s0.stream == SOpen,
      val rec = st.drv.get(dn)
      val accepted = and {
        rec.status == DAssigned or rec.status == DRunning,
        rec.assignedExec == e
      }
      all {
        st' = { ...st,
          sched: st.sched.set(e, { ...s0,
            running: if (s0.running == dn) "" else s0.running,
            lastCompleted: if (s0.running == dn) dn else s0.lastCompleted,
            drainingAdmin: if (s0.running == dn) true else s0.drainingAdmin }),
          bld: st.bld.set(e, { ...b, claimed: "", reported: true }),
          drv:
            if (accepted and ok)
              st.drv.set(dn, { ...rec,
                status: DCompleted, pgStatus: DCompleted, assignedExec: "",
                resolution: RCompleted, outputsInStore: true })
            else if (accepted)
              st.drv.set(dn, { ...rec,
                status: DReady, pgStatus: DReady, assignedExec: "",
                resolution: RReturned,
                excluded: rec.excluded.union(Set(e)) })
            else st.drv },
        viol' = { ...viol,
          doubleResolution: viol.doubleResolution or (accepted and rec.resolution != RUnresolved) },
        reach' = reach
      }
    }
  }

  /// The builder wedges: it will neither report nor exit
  /// (fault-process; the backstop's subject — tick_scan_dag /
  /// tick_process_backstop_timeouts).
  action builderWedge(e: str): bool = all {
    st.wedgesLeft > 0,
    st.bld.get(e).claimed != "",
    not(st.bld.get(e).reported),
    st' = { ...st,
      drv: st.drv.set(st.bld.get(e).claimed,
        { ...st.drv.get(st.bld.get(e).claimed), wedged: true }),
      wedgesLeft: st.wedgesLeft - 1 },
    viol' = viol,
    reach' = reach
  }

  /// SIGTERM lands on the builder (the drain transition): subsequent
  /// heartbeats advertise draining=true (I-063) and new assignments
  /// are rejected; the in-flight build continues.
  action builderSigterm(e: str): bool = all {
    st.bld.get(e).alive,
    not(st.bld.get(e).sigterm),
    st.sigtermsLeft > 0,
    st' = { ...st, sigtermsLeft: st.sigtermsLeft - 1,
      bld: st.bld.set(e, { ...st.bld.get(e), sigterm: true }) },
    viol' = viol,
    reach' = reach
  }

  /// The FUSE circuit breaker opens or closes on the builder
  /// (store_degraded heartbeat content; two-way, unlike draining).
  /// Budgeted so the toggle does not multiply interleavings unboundedly.
  action builderFuseFlip(e: str): bool = all {
    st.bld.get(e).alive,
    st.fuseFlipsLeft > 0,
    st' = { ...st,
      fuseFlipsLeft: st.fuseFlipsLeft - 1,
      bld: st.bld.set(e, { ...st.bld.get(e), fuseDegraded: not(st.bld.get(e).fuseDegraded) }) },
    viol' = viol,
    reach' = reach
  }

  // --- 7d. pod death and the controller channel ----------------------------

  /// The pod dies (fault-process), with or without having reported:
  /// the builder is gone for good (replacement pods are fresh
  /// identities), its stream closes, heartbeats stop, the kubelet/Job
  /// controller will surface it to the controller channel.
  action podDie(e: str): bool = {
    val s0 = st.sched.get(e)
    all {
      st.podDeathsLeft > 0,
      st.bld.get(e).alive,
      st' = { ...st,
        podDeathsLeft: st.podDeathsLeft - 1,
        bld: st.bld.set(e, { ...st.bld.get(e),
          alive: false, claimed: "", pendingAssign: "", killedByEnv: true }),
        sched:
          if (s0.present and s0.stream != SNone)
            st.sched.set(e, { ...s0,
              stream: SClosedTx,
              pendingDisc: s0.pendingDisc.union(Set(s0.streamEpoch)) })
          else st.sched },
      viol' = viol,
      reach' = reach
    }
  }

  /// DagActor::handle_executor_termination — the controller's
  /// classifying ReportExecutorTermination (OOMKilled /
  /// EvictedDiskPressure / DeadlineExceeded) for a dead pod.
  /// Correlated arm: consume the recently_disconnected entry
  /// (first-report-wins; the second-installment fill and the
  /// floor/charge content are the retry fold's). Race-ahead arm: the
  /// report lands before the disconnect — fall back to the still-live
  /// entry's running build and stamp last_completed so the imminent
  /// disconnect skips the correlation insert. The non-promoting arm
  /// returns before touching the entry (a pure no-op — not an
  /// action). Leader-gated at the command arm.
  action controllerReport(e: str): bool = {
    val entry = st.rd.get(e)
    val s0 = st.sched.get(e)
    val raceAhead = not(entry.present)
    all {
      st.believedLeader,
      not(st.bld.get(e).alive),
      // only a resource-kill (OOMKilled / EvictedDiskPressure /
      // DeadlineExceeded) produces a promoting report; a clean exit is
      // reported as Completed, the non-promoting no-op arm
      st.bld.get(e).killedByEnv,
      or {
        entry.present,
        and { s0.present, s0.running != "" }
      },
      st' =
        if (raceAhead)
          { ...st, sched: st.sched.set(e, { ...s0, lastCompleted: s0.running }) }
        else
          { ...st, rd: st.rd.set(e, { ...entry, present: false }) },
      viol' = viol,
      reach' = { ...reach,
        deathByTwoChannels: reach.deathByTwoChannels or entry.present }
    }
  }

  /// The correlation entry's TTL window closes (bucketed time): the
  /// entry becomes sweepable by the establishment sweep.
  action rdExpire(e: str): bool = all {
    st.rd.get(e).present,
    not(st.rd.get(e).expired),
    st' = { ...st, rd: st.rd.set(e, { ...st.rd.get(e), expired: true }) },
    viol' = viol,
    reach' = reach
  }

  // --- 7e. draining (admin) -------------------------------------------------

  /// DagActor::handle_drain_executor: force=false marks the slot
  /// draining (in-flight builds continue); force=true additionally
  /// takes the running build, sends a best-effort CancelSignal (the
  /// builder aborts), appends the force_drain row in one installment
  /// (charge imported; deliberately NO correlation entry) and requeues
  /// via reassign_derivations (leader-gated).
  action adminDrain(e: str, force: bool): bool = {
    val s0 = st.sched.get(e)
    val dn = s0.running
    all {
      // admin/mod.rs::drain_executor: ensure_leader() before the actor
      // command, so a standby never processes a drain
      st.believedLeader,
      st.adminDrainsLeft > 0,
      s0.present,
      st' =
        if (not(force) or dn == "")
          { ...st, adminDrainsLeft: st.adminDrainsLeft - 1,
            sched: st.sched.set(e, { ...s0, drainingAdmin: true }) }
        else {
          val st1 = { ...st, adminDrainsLeft: st.adminDrainsLeft - 1,
            sched: st.sched.set(e, { ...s0, drainingAdmin: true, running: "" }),
            bld: st.bld.set(e, { ...st.bld.get(e), claimed: "" }) }
          if (st.believedLeader) requeue(st1, dn) else st1
        },
      viol' = viol,
      reach' = reach
    }
  }

  // --- 7f. the housekeeping tick -------------------------------------------

  /// DagActor::handle_tick — one leader actor turn:
  /// credit_heartbeats_for_stall (implicit in the worker-time bucket),
  /// tick_check_heartbeats (reap slots stale in worker time, each reap
  /// = handle_executor_disconnected at the CURRENT epoch),
  /// tick_sweep_recently_disconnected (establish expired entries),
  /// tick_process_backstop_timeouts (CancelSignal best-effort,
  /// quarantine the wedged worker, requeue), tick_sweep_dispatched_cells,
  /// then the dispatch pass (dispatch_ready → assign_to_worker, gated
  /// on recovery). A standby tick is a no-op and is not an action.
  action tick: bool = {
    val staleSlots = EXECS.filter(e => st.sched.get(e).present and st.sched.get(e).hb == HStale)
    val freshSlots = EXECS.filter(e => st.sched.get(e).present and st.sched.get(e).hb == HFresh)
    // 1. reap every worker-time-stale slot
    val afterReap = staleSlots.fold(st, (acc, e) => applyDisconnect(acc, e))
    // 2. establishment sweep over expired correlation entries
    val expired = EXECS.filter(e => afterReap.rd.get(e).present and afterReap.rd.get(e).expired)
    val afterSweep = expired.fold(afterReap, (acc, e) => establish(acc, e))
    // 3. backstop: every Running+wedged drv — clear the slot's running
    //    build, quarantine (draining), requeue (charge imported)
    val wedgedDrvs = DRVS.filter(d =>
      afterSweep.drv.get(d).status == DRunning and afterSweep.drv.get(d).wedged)
    val afterBackstop = wedgedDrvs.fold(afterSweep, (acc, d) => {
      val ex = acc.drv.get(d).assignedExec
      val acc1 =
        if (ex == "") acc
        else if (acc.sched.get(ex).present)
          { ...acc, sched: acc.sched.set(ex, { ...acc.sched.get(ex),
              running: if (acc.sched.get(ex).running == d) "" else acc.sched.get(ex).running,
              drainingAdmin: true }) }
        else acc
      requeue(acc1, d)
    })
    // 4. dispatched_cells DAG-state sweep: retain only pre-terminal drvs
    val afterCells = { ...afterBackstop,
      drv: afterBackstop.drv.keys().mapBy(d => {
        val rec = afterBackstop.drv.get(d)
        if (rec.cellArmed and rec.status == DCompleted) { ...rec, cellArmed: false } else rec
      }) }
    // 5. dispatch pass (leader + recovery gates of dispatch_ready):
    //    a two-round greedy drain covering the 2-slot/2-drv bound
    val afterDispatch =
      if (not(afterCells.recoveryComplete)) afterCells
      else dispatchRound(dispatchRound(afterCells))
    // 6. starved-ticks accounting: a still-Ready drv that had an
    //    eligible free slot this turn and was not offered ages one
    //    starved tick (capped one past the bound)
    val final = { ...afterDispatch,
      drv: afterDispatch.drv.keys().mapBy(d => {
        val rec = afterDispatch.drv.get(d)
        if (and {
          rec.status == DReady,
          rec.attempt < DISPATCH_CEIL,
          afterCells.recoveryComplete,
          EXECS.exists(e => eligibleFor(afterDispatch, e, d))
        })
          { ...rec, starved: if (rec.starved <= STARVE_BOUND) rec.starved + 1 else rec.starved }
        else { ...rec, starved: 0 }
      }) }
    all {
      st.believedLeader,
      st' = final,
      viol' = { ...viol,
        reapedFreshSlot: viol.reapedFreshSlot or
          freshSlots.exists(e => not(final.sched.get(e).present)),
        staleSlotSurvivedTick: viol.staleSlotSurvivedTick or
          staleSlots.exists(e => final.sched.get(e).present),
        offeredUnrunnable: viol.offeredUnrunnable or
          DRVS.exists(d => and {
            final.drv.get(d).status == DAssigned,
            st.drv.get(d).status == DReady,
            val ex = final.drv.get(d).assignedExec
            if (ex == "") false else not(eligibleFor(afterCells, ex, d))
          }),
        offeredUnregistered: viol.offeredUnregistered or
          DRVS.exists(d => and {
            final.drv.get(d).status == DAssigned,
            st.drv.get(d).status == DReady,
            val ex = final.drv.get(d).assignedExec
            if (ex == "") false else not(isRegistered(afterCells.sched.get(ex)))
          }),
        starvedPastBound: viol.starvedPastBound or
          DRVS.exists(d => final.drv.get(d).starved > STARVE_BOUND),
        doubleResolution: viol.doubleResolution or
          wedgedDrvs.exists(d => afterSweep.drv.get(d).resolution != RUnresolved) or
          staleSlots.exists(e => {
            val dn = st.sched.get(e).running
            if (dn == "" or st.sched.get(e).lastCompleted == dn) false
            else and {
              st.drv.get(dn).resolution != RUnresolved,
              st.drv.get(dn).status == DAssigned or st.drv.get(dn).status == DRunning
            }
          }) },
      reach' = { ...reach,
        reapHappened: reach.reapHappened or staleSlots.size() > 0 }
    }
  }

  /// The accepted assignment actually starts running on the worker
  /// (the scheduler-side Running transition observed via BuildPhase /
  /// the heartbeat; collapsed to one in-place transition so the
  /// backstop's Running precondition is reachable).
  action drvStartsRunning(d: str): bool = {
    val rec = st.drv.get(d)
    all {
      rec.status == DAssigned,
      rec.assignedExec != "",
      if (rec.assignedExec == "") false else st.bld.get(rec.assignedExec).claimed == d,
      st' = { ...st, drv: st.drv.set(d, { ...rec, status: DRunning, pgStatus: DRunning }) },
      viol' = viol,
      reach' = reach
    }
  }

  // --- 7g. the persist fault ------------------------------------------------

  /// fault-persist (the retry model's TX_FAULTS shape): one
  /// persist_status write silently failed, so the PG mirror still holds
  /// the PREVIOUS value while the in-memory view moved on — observable
  /// only through the next failover's recovery, which rebuilds from PG.
  /// Both reachable divergence directions are modeled: the assignment
  /// persist was lost (in-memory Assigned, PG still Ready) and the
  /// requeue persist was lost after a real dispatch (in-memory Ready,
  /// PG still Assigned — requires that an assignment was persisted at
  /// some point, i.e. attempt >= 1). PG never gains a value no persist
  /// call could have written.
  action persistFault(d: str): bool = {
    val rec = st.drv.get(d)
    all {
      st.persistFaultsLeft > 0,
      rec.status == rec.pgStatus,
      or {
        rec.status == DAssigned,
        and { rec.status == DReady, rec.attempt >= 1 }
      },
      st' = { ...st,
        persistFaultsLeft: st.persistFaultsLeft - 1,
        drv: st.drv.set(d, { ...rec,
          pgStatus: if (rec.status == DReady) DAssigned else DReady }) },
      viol' = viol,
      reach' = reach
    }
  }

  // --- 7h. leader failover ----------------------------------------------------

  /// The lease moves to another replica (fault-leader): this replica
  /// is deposed but has not observed it yet — the deposed-believer
  /// window (boundedDualLeadership's subject; the witness pins it
  /// reachable).
  action depose: bool = all {
    st.failoversLeft > 0,
    st.actualHolder,
    st' = { ...st,
      actualHolder: false,
      newLeaderGen: st.generation + 1,
      failoversLeft: st.failoversLeft - 1 },
    viol' = viol,
    reach' = reach
  }

  /// A worker's heartbeat lands on the NEW leader (abstract): its B5
  /// latch observes the higher generation (fetch_max is monotone), so
  /// this replica's stale assignments are rejected from now on
  /// (sched.lease.generation-fence).
  action workerObservesNewLeader(e: str): bool = all {
    not(st.actualHolder),
    st.bld.get(e).alive,
    st.bld.get(e).observedGen < st.newLeaderGen,
    st' = { ...st, bld: st.bld.set(e, { ...st.bld.get(e), observedGen: st.newLeaderGen }) },
    viol' = viol,
    reach' = reach
  }

  /// This replica observes the loss (LeaderState::on_lose →
  /// ActorCommand::LeaderLost → handle_leader_lost +
  /// clear_persisted_state): is_leader() goes false, the DAG and the
  /// actor maps (recently_disconnected, dispatched_cells, hung_nodes,
  /// authoritative_binding) are wiped, the executors map is retained
  /// (live connections), and the per-stream generation fence closes
  /// the worker streams (the reader's stream_is_stale check).
  action observeLoss: bool = all {
    st.believedLeader,
    not(st.actualHolder),
    st' = { ...st,
      believedLeader: false,
      recoveryComplete: false,
      reconcilePending: false,
      rd: EXECS.mapBy(e => { present: false, drv: "", attempt: 0, expired: false }),
      drv: st.drv.keys().mapBy(d => { ...st.drv.get(d), cellArmed: false }),
      sched: EXECS.mapBy(e => {
        val s = st.sched.get(e)
        if (s.present and s.stream == SOpen)
          { ...s, stream: SClosedTx, pendingDisc: s.pendingDisc.union(Set(s.streamEpoch)) }
        else s
      }) },
    viol' = viol,
    reach' = reach
  }

  /// This replica re-acquires the lease (handle_leader_acquired):
  /// recovery rebuilds the DAG from PG (status := the persisted
  /// mirror), the generation moves past the interim holder's, recovery
  /// completes and the 45 s reconcile timer is armed
  /// (schedule_reconcile_timer). The in-memory resolution bookkeeping
  /// of recovered in-flight attempts starts over (the durable history
  /// is the retry model's subject).
  action reacquire: bool = all {
    not(st.believedLeader),
    not(st.actualHolder),
    st' = { ...st,
      believedLeader: true,
      actualHolder: true,
      generation: st.newLeaderGen + 1,
      recoveryComplete: true,
      reconcilePending: true,
      // recover_from_pg's first statement is clear_persisted_state, so
      // standby-era correlation entries and cells never leak into the
      // new tenure
      rd: EXECS.mapBy(e => { present: false, drv: "", attempt: 0, expired: false }),
      drv: st.drv.keys().mapBy(d => {
        val rec = st.drv.get(d)
        val pgInflight = rec.pgStatus == DAssigned or rec.pgStatus == DRunning
        { ...rec,
          status: rec.pgStatus,
          cellArmed: false,
          // the assignment binding is recovered from PG: no in-flight
          // persisted row means no assigned executor
          assignedExec: if (pgInflight) rec.assignedExec else "",
          resolution: if (pgInflight) RUnresolved else rec.resolution }
      }) },
    viol' = viol,
    reach' = reach
  }

  /// recovery.rs::handle_reconcile_assignments — the 45 s post-failover
  /// reconcile sweep (leader-gated): every PG-Assigned/Running drv
  /// whose worker did not come back is resolved — adopted as completed
  /// when its outputs are in the store (the store-probe arm), else
  /// reset to Ready (no charge); a worker that reconnected and reports
  /// the build keeps it; a stream-connected-but-not-yet-heartbeated
  /// worker is deferred. A sweep that deferred at least one claim
  /// re-arms itself (handle_reconcile_assignments calls
  /// schedule_reconcile_timer when its collection pass deferred
  /// anything), so `reconcilePending` stays set until a sweep completes
  /// with no deferral — the deferral is a delay, never a terminal
  /// state.
  action reconcileSweep: bool = {
    val resolved = DRVS.fold(st, (acc, d) => {
      val rec = acc.drv.get(d)
      if (rec.status != DAssigned and rec.status != DRunning) acc
      else {
        val e = rec.assignedExec
        val workerRegistered = if (e == "") false else isRegistered(acc.sched.get(e))
        val workerHasIt = if (e == "") false else workerRegistered and acc.sched.get(e).running == d
        val deferred = if (e == "") false else acc.sched.get(e).present and not(workerRegistered)
        if (workerHasIt or deferred) acc
        else if (rec.outputsInStore)
          { ...acc, drv: acc.drv.set(d, { ...rec,
              status: DCompleted, pgStatus: DCompleted, assignedExec: "",
              resolution: RCompleted }) }
        else
          { ...acc, drv: acc.drv.set(d, { ...rec,
              status: DReady, pgStatus: DReady, assignedExec: "",
              resolution: RReturned }) }
      }
    })
    // The defer condition of the collection pass, re-stated over the
    // pre-sweep state (the fold mutates only drv records, never the
    // slots): an in-flight claim whose executor entry exists but is
    // not yet registered. Any such claim re-arms the sweep
    // (recovery.rs::handle_reconcile_assignments, deferred > 0 →
    // schedule_reconcile_timer).
    val anyDeferred = DRVS.exists(d => {
      val rec = st.drv.get(d)
      and {
        rec.status == DAssigned or rec.status == DRunning,
        rec.assignedExec != "",
        st.sched.get(rec.assignedExec).present,
        not(isRegistered(st.sched.get(rec.assignedExec)))
      }
    })
    all {
      st.believedLeader,
      st.recoveryComplete,
      st.reconcilePending,
      st' = { ...resolved, reconcilePending: anyDeferred },
      viol' = { ...viol,
        fabricatedCompletion: viol.fabricatedCompletion or
          DRVS.exists(d => and {
            st.drv.get(d).status != DCompleted,
            resolved.drv.get(d).status == DCompleted,
            not(st.drv.get(d).outputsInStore)
          }),
        doubleResolution: viol.doubleResolution or
          DRVS.exists(d => {
            val rec = st.drv.get(d)
            and {
              rec.status == DAssigned or rec.status == DRunning,
              rec.resolution != RUnresolved,
              resolved.drv.get(d).resolution != rec.resolution
            }
          }),
        unresolvedAfterSweep: viol.unresolvedAfterSweep or
          DRVS.exists(d => {
            val rec = resolved.drv.get(d)
            and {
              rec.status == DAssigned or rec.status == DRunning,
              if (rec.assignedExec == "") true
              else not(resolved.sched.get(rec.assignedExec).present)
            }
          }) },
      reach' = reach
    }
  }

  // --- 8. init ------------------------------------------------------------

  action init = all {
    st' = {
      sched: EXECS.mapBy(e => emptySched),
      bld: EXECS.mapBy(e => {
        alive: true, claimed: "", reported: false, sigterm: false,
        fuseDegraded: false, observedGen: 1, pendingAssign: "",
        killedByEnv: false }),
      drv: DRVS.mapBy(d => {
        status: DReady, pgStatus: DReady, assignedExec: "", attempt: 0,
        resolution: RUnresolved, excluded: Set(), starved: 0,
        // the controller acked the spawned intents before these pods came
        // up (handle_ack_spawned_intents armed the cells); the model
        // starts there and only the clear sites run
        cellArmed: true, wedged: false, outputsInStore: false }),
      rd: EXECS.mapBy(e => { present: false, drv: "", attempt: 0, expired: false }),
      believedLeader: true,
      actualHolder: true,
      generation: 1,
      newLeaderGen: 1,
      recoveryComplete: true,
      reconcilePending: false,
      podDeathsLeft: POD_DEATHS,
      failoversLeft: FAILOVERS,
      persistFaultsLeft: PERSIST_FAULTS,
      msgLossLeft: MSG_LOSS,
      sendFailLeft: SEND_FAILS,
      halfDeathLeft: HALF_DEATHS,
      streamDropLeft: STREAM_DROPS,
      hbDropLeft: HB_DROPS,
      dupMsgLeft: DUP_MSGS,
      wedgesLeft: WEDGES,
      stallsLeft: STALLS,
      fuseFlipsLeft: FUSE_FLIPS,
      adminDrainsLeft: ADMIN_DRAINS,
      sigtermsLeft: SIGTERMS
    },
    viol' = {
      secondLiveStream: false, staleEpochMutation: false,
      heartbeatCreatedEntry: false, doubleResolution: false,
      fabricatedCompletion: false, reapedFreshSlot: false,
      staleSlotSurvivedTick: false, rdInsertNotMidBuild: false,
      establishedBeforeTtl: false, nonPromotingConsumed: false,
      offeredUnrunnable: false, offeredUnregistered: false,
      starvedPastBound: false, halfRecordedAssignment: false,
      durableWriteWhileDeposed: false, adoptAndReset: false,
      unresolvedAfterSweep: false
    },
    reach' = {
      phantomDrained: false, adoptHappened: false,
      reapHappened: false, deathByTwoChannels: false
    }
  }

  // --- 9. step --------------------------------------------------------------

  action step = {
    nondet e = EXECS.oneOf()
    nondet d = DRVS.oneOf()
    nondet ok = Set(true, false).oneOf()
    any {
      connectAccept(e, d),
      streamHalfDie(e),
      streamDrop(e),
      builderExit(e),
      deliverDisconnect(e),
      heartbeat(e),
      hbAge(e),
      actorStall,
      builderRecvAssign(e),
      loseAssignMsg(e),
      dupAssignDeliver(e),
      rollbackAssign(e),
      builderComplete(e, ok),
      builderWedge(e),
      builderSigterm(e),
      builderFuseFlip(e),
      podDie(e),
      controllerReport(e),
      rdExpire(e),
      adminDrain(e, false),
      adminDrain(e, true),
      tick,
      drvStartsRunning(d),
      persistFault(d),
      depose,
      workerObservesNewLeader(e),
      observeLoss,
      reacquire,
      reconcileSweep
    }
  }
}

// ===========================================================================
// Regimes (one thin instantiation module per check; nix/quint.nix wires
// each as its own exhaustive TLC run, plus the witness checks).
// ===========================================================================

/// Base regime: no faults. Streams stay healthy, pods stay alive, the
/// leader never changes. The core protocol — registration, dispatch,
/// builder accept/reject, completion, the one-shot drain, the SIGTERM
/// and admin drain flags, the voluntary exits and their disconnects —
/// must hold every invariant here.
module executorSessionBase {
  import executorSession(
    EXECS = Set("e1", "e2"),
    DRVS = Set("d1"),
    EPOCH_CEIL = 2,
    DISPATCH_CEIL = 2,
    STARVE_BOUND = 2,
    POD_DEATHS = 0,
    WEDGES = 0,
    FAILOVERS = 0,
    PERSIST_FAULTS = 0,
    MSG_LOSS = 0,
    SEND_FAILS = 0,
    HALF_DEATHS = 0,
    STREAM_DROPS = 0,
    HB_DROPS = 0,
    DUP_MSGS = 0,
    STALLS = 1,
    FUSE_FLIPS = 2,
    ADMIN_DRAINS = 1,
    SIGTERMS = 1
  ).*
}

/// fault-stream is split into two cfgs (the campaign plan's
/// pre-registered demote-or-split fallback, taken for per-check cost):
/// message faults and connection faults. Both stay in checks.*; the
/// design's per-class budgets are unchanged (1 each), only their
/// grouping per check changed.
///
/// fault-stream-msg: faults on messages in transit — assignment-message
/// loss, duplicate assignment delivery, try_send failure (the rollback
/// path).
module executorSessionFaultStreamMsg {
  import executorSession(
    EXECS = Set("e1", "e2"),
    DRVS = Set("d1"),
    EPOCH_CEIL = 2,
    DISPATCH_CEIL = 2,
    STARVE_BOUND = 2,
    POD_DEATHS = 0,
    WEDGES = 0,
    FAILOVERS = 0,
    PERSIST_FAULTS = 0,
    MSG_LOSS = 1,
    SEND_FAILS = 1,
    HALF_DEATHS = 0,
    STREAM_DROPS = 0,
    HB_DROPS = 0,
    DUP_MSGS = 1,
    STALLS = 1,
    FUSE_FLIPS = 2,
    ADMIN_DRAINS = 1,
    SIGTERMS = 1
  ).*
}

/// fault-stream-conn: faults on the connection — bridge half-death
/// (I-095), full connection drop (the I-056a reconnect/late-disconnect
/// race), heartbeat loss/delay. Carries the F1 session-identity
/// content: stale-epoch disconnects stay inert, the half-dead binding
/// never accepts a second live stream, registration still requires
/// both halves.
module executorSessionFaultStreamConn {
  import executorSession(
    EXECS = Set("e1", "e2"),
    DRVS = Set("d1"),
    EPOCH_CEIL = 2,
    DISPATCH_CEIL = 2,
    STARVE_BOUND = 2,
    POD_DEATHS = 0,
    WEDGES = 0,
    FAILOVERS = 0,
    PERSIST_FAULTS = 0,
    MSG_LOSS = 0,
    SEND_FAILS = 0,
    HALF_DEATHS = 1,
    STREAM_DROPS = 1,
    HB_DROPS = 1,
    DUP_MSGS = 0,
    STALLS = 1,
    FUSE_FLIPS = 2,
    ADMIN_DRAINS = 1,
    SIGTERMS = 1
  ).*
}

/// fault-process: pod death (with or without a prior report) and the
/// wedge (no report, no death — the
/// backstop's subject), plus heartbeat loss so a live-but-silent slot
/// is distinguishable from a dead one.
module executorSessionFaultProcess {
  import executorSession(
    EXECS = Set("e1", "e2"),
    DRVS = Set("d1"),
    EPOCH_CEIL = 2,
    DISPATCH_CEIL = 2,
    STARVE_BOUND = 2,
    POD_DEATHS = 1,
    WEDGES = 1,
    FAILOVERS = 0,
    PERSIST_FAULTS = 0,
    MSG_LOSS = 0,
    SEND_FAILS = 0,
    HALF_DEATHS = 0,
    STREAM_DROPS = 0,
    HB_DROPS = 1,
    DUP_MSGS = 0,
    STALLS = 1,
    FUSE_FLIPS = 1,
    ADMIN_DRAINS = 1,
    SIGTERMS = 1
  ).*
}

/// fault-leader: one failover under in-flight builds — depose,
/// deposed-believer window, observed loss (state wipe + stream fence),
/// re-acquire, PG recovery, the 45 s reconcile sweep. Imports
/// boundedDualLeadership, not neverDual: the deposed-believer window
/// stays reachable (noDeposedBeliever witness).
module executorSessionFaultLeader {
  import executorSession(
    EXECS = Set("e1", "e2"),
    DRVS = Set("d1"),
    EPOCH_CEIL = 2,
    DISPATCH_CEIL = 2,
    STARVE_BOUND = 2,
    POD_DEATHS = 0,
    WEDGES = 0,
    FAILOVERS = 1,
    PERSIST_FAULTS = 0,
    MSG_LOSS = 0,
    SEND_FAILS = 0,
    HALF_DEATHS = 0,
    STREAM_DROPS = 0,
    HB_DROPS = 0,
    DUP_MSGS = 0,
    STALLS = 0,
    FUSE_FLIPS = 0,
    ADMIN_DRAINS = 1,
    SIGTERMS = 1
  ).*
}

/// fault-persist: one status-persist write may silently fail (the
/// retry model's TX_FAULTS shape). The divergence is only observable
/// through a failover's PG recovery, so the regime also carries one
/// failover and one pod death — the lost-Ready / lost-Assigned mirrors
/// must still converge through the reconcile sweep without fabricating
/// a completion or losing the claim.
module executorSessionFaultPersist {
  import executorSession(
    EXECS = Set("e1", "e2"),
    DRVS = Set("d1"),
    EPOCH_CEIL = 2,
    DISPATCH_CEIL = 2,
    STARVE_BOUND = 2,
    POD_DEATHS = 1,
    WEDGES = 0,
    FAILOVERS = 1,
    PERSIST_FAULTS = 1,
    MSG_LOSS = 0,
    SEND_FAILS = 0,
    HALF_DEATHS = 0,
    STREAM_DROPS = 0,
    HB_DROPS = 0,
    DUP_MSGS = 0,
    STALLS = 0,
    FUSE_FLIPS = 0,
    ADMIN_DRAINS = 0,
    SIGTERMS = 1
  ).*
}
