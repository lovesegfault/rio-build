// Model N of the controller-formal campaign (Phase 0, Stage B): the L10
// NodeClaim-pool reconciler's mirror lifecycle — the tick body of
// rio-controller/src/reconcilers/nodeclaim_pool/mod.rs::reconcile_once /
// ::consolidate_only / ::run (the lease-edge and ⊥-streak handling),
// health.rs::{reap_unhealthy, detect_vanished},
// consolidate.rs::{observe_idle_to_busy, reap_idle},
// sketch.rs::observe_registered (the recency gate), and
// cover.rs::class_budget / the cover_deficit budget arithmetic.
//
// WHAT IS MODELED
//   One controller replica (the lease-gated L10 loop), two NodeClaim
//   slots over two cells, one tick per tick* action. The environment
//   owns Karpenter (registration, fast GC, termination edges), the
//   scheduler's reachability and ICE-mask peer state, the pod-requested
//   idle/busy truth per claim, the lease, and PG load failures. Demand
//   (unplaced intents per cell) and the FFD outcome (which registered
//   claims are reserved) are per-tick nondeterministic inputs — the
//   model consumes placed/unplaced/cell as opaque outcomes per the
//   campaign design.
//
// THE TICK FAMILY (mirrors the code; one named action per mode)
//   tickFull          scheduler reachable: lose-flag processing, reload
//                     latch, kube-only observations (idle->busy edges +
//                     recency-gated Registered boot samples), FFD
//                     outcome, placeable publish, stuck-claim reap,
//                     reap-name removal from inflight_created BEFORE
//                     detect_vanished, vanish detection (ICE), the
//                     ceilings/config-gated cover with global /
//                     per-class / per-tick budgets, the
//                     unfulfillable/registered ack, idle reap, persist.
//   tickBotEarly      a ⊥ tick below the streak threshold: the reload
//                     latch runs, then the kube-only observation block
//                     (idle→busy pruning, Registered-edge recording
//                     with the clear discarded) — NO reap, prune,
//                     create, publish, or ack (effects stay
//                     threshold-gated; the 2026-06-02 ⊥-arm fix).
//   tickConsolidateOnly  the streak at/over threshold: kube-only
//                     observations, idle/unhealthy reaps and the
//                     inflight prune run; no create, no publish, no ack.
//   tickStandby       not the leader: lose-flag processing only.
//
// TIME AND BUCKETS
//   Wall-clock is the tick counter. Idle spells and prev_idle entries
//   age young -> old once they have spanned a full tick (one bucket
//   stands in for "past the consolidation threshold"); the Registered
//   recency window (3 x TICK in the code) is one non-observing tick
//   here; the ⊥-streak threshold (5 in the code) is BOT_THRESHOLD = 2
//   (zero / one early-return tick / consolidate-only) — the structure,
//   not the constants, is what the invariants quantify over. A busy
//   period must span at least one tick before the claim can go idle
//   again (builds outlast the 10s tick by orders of magnitude); without
//   that assumption the idle-conflation hazard would be reachable even
//   with every observation running, which the real timing forbids.
//
// SIMPLIFICATIONS (documented, deliberate)
//   - classify()'s in-live ICE / boot-timeout arms are folded into one
//     "stuck" bit per in-flight claim; the dead_nodes arm is consumed
//     input only and out of model (its protection is the §4(a)1 gate
//     tests, not these invariants). Input-source note (executor 1c /
//     OA2): the dead_nodes-shaped input is now the union of the
//     scheduler-reported set (stream pools, until 1d) and the
//     controller-side per-node deadline-expiry clustering over the
//     open-attempt ledger view (nodeclaim_pool/wedge.rs). Still
//     consumed input only — none of this model's checked invariants
//     quantify over it; the clustering's protection is its own unit
//     battery plus the same dead-reap cap the modeled reaps respect.
//   - The Pool-coverage filter and its fail-open posture are out of
//     model (demand is post-coverage); the unknown-cell drop and the
//     ceilings gate of the config mirror ARE modeled.
//   - recorded_boot is folded into the per-claim boot phase; the
//     acquire edge re-classifies recorded claims as stale-other-tenure
//     immediately rather than at the reload-Ok arm — observationally
//     equivalent (neither path emits a clear or a sample for them).
//   - Sketch contents, FFD/cover sizing arithmetic and λ accounting are
//     out of model; PG is one abstract cell whose load may fail and
//     whose overwrite-while-stale is the reload-latch violation.
//   - The per-tick mark dedup (the BTreeSet in report_unfulfillable) is
//     by construction here (marks are a set); the soundness checks are
//     about WHICH cells are marked, not how many times.
//
// ASSUME-GUARANTEE CHECKLISTS
//   rio-lease (imported, never re-modeled): at most one believer; an
//   ex-leader keeps acting only until it observes the loss — the model
//   has one replica whose isLeader bit the environment toggles, and the
//   single-effective-provisioner invariant is about the replica taking
//   no effect while not leader (the cross-replica half is the lease
//   campaign's theorem).
//   Scheduler ICE ladder (peer): the model only arms/clears a masked
//   bit per cell; the backoff ladder, the |A'|=1 heartbeat clear and
//   TTL expiry are scheduler-side (sched.sla.hw-class.ice-mask).
//   Model J (spawnCoherence.qnt) consumes the placeable gate this model
//   publishes; the producer-side guarantee J assumes is exactly the
//   gateProducerGuarantee + singleEffectiveProvisioner invariants here.
//
// INVARIANTS (violation latches in `violations`)
//   idleReapSafety        an idle reap only deletes a claim whose
//                         current idle spell is genuinely past the
//                         threshold bucket (I9 + the amplify-polarity
//                         consequence of the lease-edge table)
//   iceMarkSoundness      a vanish ICE mark is never emitted for a
//                         claim the controller itself reaped (I7 / the
//                         reap-names-before-detect ordering)
//   bootSampleNotLost     a Registered edge that happened under the
//                         current tenure is recorded with its sample
//                         and clear (the no-loss half of
//                         BootRecordedOnce)
//   noMassClearAfterFailover  registered_cells clears are emitted only
//                         for recency-fresh registrations
//   reloadLatchRespected  no persist overwrites PG while the
//                         lease-acquire reload is pending
//   singleEffectiveProvisioner  no create / delete / ack / publish
//                         effect on a tick where the replica is not the
//                         leader
//   gateProducerGuarantee the placeable gate is unarmed by the end of
//                         any tick that observed a pending lease-loss
//                         flag and is never re-armed by a degraded tick
//   provisioningBudget    global, per-class and per-cell-per-tick
//                         budgets hold with terminating claims still
//                         counted; failed creates consume no budget
//   coverRespectsMask     no create lands in a cell ICE-marked this
//                         tick or already masked
//   degradedCoverPolarity no create without loaded ceilings; no create
//                         for an unconfigured cell
//   boundsOK              the ⊥-streak counter stays at or below its
//                         threshold ceiling
//
// PRE-REGISTERED AS-BUILT FALSIFICATIONS — RESOLVED 2026-06-02
//   The ⊥-tick early-return skip (entry 1, the list's ONLY entry) was
//   FIXED: pre-threshold ⊥ ticks now run the shared kube-only
//   observation block (see tickBotEarlyWith). idleReapSafety and
//   bootSampleNotLost moved from expect-violation checks into the
//   fault-rpc HOLD invariant list, and the reproducer traces were
//   rewritten as passing runs (idlePruneAcrossBotRun,
//   bootRecordedAcrossBotRun). The expected-as-built-falsifications
//   list is now EMPTY — the empty-remainder claim holds
//   unconditionally (controller-invariant-map.md records the executed
//   flip and the fix commit).
//
// WITNESSES (must be VIOLATED — wired as expect-violation checks)
//   canReachIdleReap, canReachVanishMark, canReachClearAfterMark,
//   canReachConsolidateOnlyReap, canReachClassBudgetBound,
//   canReachCreateFailure, canReachCeilingsFailClosed,
//   canReachUnknownCellDrop, canReachCreate (creates resume once the
//   ceilings are present), canReachAcquireWithInflight,
//   canReachDegradedReloadTick, canReachFreshClearAfterAcquire,
//   canReachStaleRecordOnly.
//
// WEAKENED-TEST PROCEDURE (results in the introducing commit message):
//   dropping the reap-name removal before detect_vanished (or the
//   consolidate-only prune) falsifies iceMarkSoundness in
//   fault-karpenter; dropping the unconditional prev_idle clear on the
//   reload edge falsifies idleReapSafety in fault-lease; dropping the
//   ceilings gate falsifies degradedCoverPolarity in fault-rpc;
//   dropping the persist gate falsifies reloadLatchRespected in
//   fault-lease.
module nodeclaimLifecycle {
  // ------------------------------------------------------------------
  // 1. Types
  // ------------------------------------------------------------------
  type ClaimId = str
  type Cell = str

  type ClaimPhase = CAbsent | CInFlight | CRegistered | CTerminating

  /// The current idle spell of a registered claim, as the world has it
  /// (requested.cores == 0 since when): not idle, idle but not yet
  /// spanning a tick, idle and seen by one tick (still young), idle
  /// past the threshold bucket.
  type Spell = SNotIdle | SYoungUnseen | SYoungSeen | SOld

  /// Boot-sample tracking per registered claim (recorded_boot folded
  /// in): the registration is recent (within the recency window) or
  /// stale, and happened under the current tenure or not; or the boot
  /// sample has been recorded (or deliberately skipped) already.
  type BootPhase = BFreshTenure | BFreshOther | BStaleTenure | BStaleOther | BRecorded

  /// The controller's prev_idle entry for a claim.
  type IdleTrack = INotTracked | ITrackedYoung | ITrackedOld

  type ClaimRec = {
    phase: ClaimPhase,
    cell: Cell,
    stuck: bool,
    spell: Spell,
    busyTickSeen: bool,
    boot: BootPhase,
    inflightTracked: bool,
    inflightCell: Cell,
    pIdle: IdleTrack,
    ctrlReaped: bool,
  }

  type Effect =
    | ECreateOk
    | ECreateFailed
    | EIceMarkEmitted
    | EClearEmitted
    | EClearOfMaskedCell
    | EVanishDetected
    | EStuckReap
    | EIdleReap
    | EConsolidateOnlyReap
    | EClassBudgetBound
    | EFailClosedNoCeilings
    | EUnknownCellDrop
    | EAcquireWithInflight
    | EDegradedReloadTick
    | EStaleRecordOnly
    | EPublished

  type Violation =
    | VIdleOverReap
    | VSpuriousMark
    | VBootSampleLost
    | VStaleClear
    | VStaleOverwrite
    | VStandbyEffect
    | VGateProducer
    | VBudgetGlobal
    | VBudgetClass
    | VBudgetPerTick
    | VCreateNoCeilings
    | VCreateUnconfigured
    | VCoverMasked

  // ------------------------------------------------------------------
  // 2. Constants
  // ------------------------------------------------------------------
  /// max_fleet_cores, in claim-sized units (every claim is one core).
  const MAX_FLEET_CORES: int
  /// Per-hwClass fleet cap for cellA (cellB has none — global only),
  /// mirroring HwClassConfig::fleet_cap_for == None for absent caps.
  const CLASS_CAP_A: int
  /// max_node_claims_per_cell_per_tick.
  const PER_CELL_TICK_CAP: int
  /// BOT_TICKS_BEFORE_CONSOLIDATE_ONLY (5 in the code; 2 here — one
  /// early-return tick, then consolidate-only).
  const BOT_THRESHOLD: int
  /// Per-tick ⊥ for GetSpawnIntents / the ack / the NodeClaim create,
  /// plus the ceilings-not-loaded and config-cell-drop actions.
  const ENABLE_RPC_FAULT: bool
  /// Lease lose / acquire edges, PG reload failure, controller restart.
  const ENABLE_LEASE_FAULT: bool
  /// Karpenter fast-GC of in-flight claims, launch-stuck claims and
  /// spot termination edges.
  const ENABLE_KARPENTER_FAULT: bool
  /// Whether the GetHwClassConfig global ceilings are loaded at init.
  const START_CEILINGS_KNOWN: bool

  pure val CLAIMS: Set[ClaimId] = Set("c1", "c2")
  pure val CELLS: Set[Cell] = Set("cellA", "cellB")

  pure def boolChoices(faultOn: bool): Set[bool] =
    if (faultOn) Set(false, true) else Set(false)

  pure def claimRank(k: ClaimId): int = if (k == "c1") 1 else 2

  pure val absentClaim: ClaimRec = {
    phase: CAbsent, cell: "cellA", stuck: false, spell: SNotIdle,
    busyTickSeen: false, boot: BRecorded, inflightTracked: false,
    inflightCell: "cellA", pIdle: INotTracked, ctrlReaped: false,
  }

  // ------------------------------------------------------------------
  // 3. State
  // ------------------------------------------------------------------
  var claims: ClaimId -> ClaimRec
  /// rio-lease grant as this replica observes it.
  var isLeader: bool
  /// ControllerLeaseHooks.lose — set by the loss edge, consumed at the
  /// top of the next tick.
  var losePending: bool
  /// ControllerLeaseHooks.reload — set by the acquire edge, cleared
  /// only by a successful PG load (latch-on-Ok-only).
  var reloadPending: bool
  /// consecutive_bot_ticks, capped at BOT_THRESHOLD.
  var botStreak: int
  /// The placeable watch channel: armed (Some) or unarmed (None).
  var placeableArmed: bool
  /// Scheduler-side IceBackoff peer state, abstracted to masked-or-not.
  var iceMasked: Set[Cell]
  /// The hw-class config mirror: which cells the controller's
  /// all_cells() currently contains.
  var configuredCells: Set[Cell]
  /// The global-ceilings-known bit of the config mirror.
  var ceilingsKnown: bool
  /// Latch: at least one lease acquire has happened (witness scoping).
  var hadAcquire: bool
  var effects: Set[Effect]
  var violations: Set[Violation]

  // ------------------------------------------------------------------
  // 4. Invariants and witnesses
  // ------------------------------------------------------------------
  val boundsOK = botStreak >= 0 and botStreak <= BOT_THRESHOLD
  val idleReapSafety = not(violations.contains(VIdleOverReap))
  val iceMarkSoundness = not(violations.contains(VSpuriousMark))
  val bootSampleNotLost = not(violations.contains(VBootSampleLost))
  val noMassClearAfterFailover = not(violations.contains(VStaleClear))
  val reloadLatchRespected = not(violations.contains(VStaleOverwrite))
  val singleEffectiveProvisioner = not(violations.contains(VStandbyEffect))
  val gateProducerGuarantee = not(violations.contains(VGateProducer))
  val provisioningBudget =
    not(violations.contains(VBudgetGlobal))
    and not(violations.contains(VBudgetClass))
    and not(violations.contains(VBudgetPerTick))
  val coverRespectsMask = not(violations.contains(VCoverMasked))
  val degradedCoverPolarity =
    not(violations.contains(VCreateNoCeilings))
    and not(violations.contains(VCreateUnconfigured))

  // Witnesses (expected to be VIOLATED).
  val canReachIdleReap = not(effects.contains(EIdleReap))
  val canReachVanishMark = not(effects.contains(EVanishDetected))
  val canReachClearAfterMark = not(effects.contains(EClearOfMaskedCell))
  val canReachConsolidateOnlyReap = not(effects.contains(EConsolidateOnlyReap))
  val canReachClassBudgetBound = not(effects.contains(EClassBudgetBound))
  val canReachCreateFailure = not(effects.contains(ECreateFailed))
  val canReachCeilingsFailClosed = not(effects.contains(EFailClosedNoCeilings))
  val canReachUnknownCellDrop = not(effects.contains(EUnknownCellDrop))
  val canReachCreate = not(effects.contains(ECreateOk))
  val canReachAcquireWithInflight = not(effects.contains(EAcquireWithInflight))
  val canReachDegradedReloadTick = not(effects.contains(EDegradedReloadTick))
  val canReachFreshClearAfterAcquire =
    not(hadAcquire and effects.contains(EClearEmitted))
  val canReachStaleRecordOnly = not(effects.contains(EStaleRecordOnly))

  // ------------------------------------------------------------------
  // 5. Pure helpers (the per-phase pieces of the tick)
  // ------------------------------------------------------------------
  /// Wall-clock aging applied at the start of every tick: idle spells
  /// and prev_idle entries that have spanned a full tick move to the
  /// old bucket; busy claims have now been exposed to a tick.
  pure def ageWallclock(c: ClaimRec): ClaimRec =
    if (c.phase != CRegistered) c
    else {
      ...c,
      spell: if (c.spell == SYoungSeen) SOld
             else if (c.spell == SYoungUnseen) SYoungSeen
             else c.spell,
      busyTickSeen: if (c.spell == SNotIdle) true else c.busyTickSeen,
      pIdle: if (c.pIdle == ITrackedYoung) ITrackedOld else c.pIdle,
    }

  /// Aging applied by ticks that do NOT run the kube-only observation
  /// block (standby ticks; controllerRestarts also applies it for the
  /// restart-gap recency expiry): an unrecorded Fresh registration's
  /// recency window expires before anyone records it.
  pure def ageBootNonObserving(c: ClaimRec): ClaimRec =
    if (c.phase != CRegistered) c
    else {
      ...c,
      boot: if (c.boot == BFreshTenure) BStaleTenure
            else if (c.boot == BFreshOther) BStaleOther
            else c.boot,
    }

  /// observe_idle_to_busy + the prev_idle prune (consolidate.rs):
  /// busy => entry removed; idle => seeded young if absent; entries for
  /// non-live or terminating claims dropped.
  pure def observeIdleToBusy(c: ClaimRec): ClaimRec =
    if (c.phase != CRegistered) { ...c, pIdle: INotTracked }
    else if (c.spell == SNotIdle) { ...c, pIdle: INotTracked }
    else if (c.pIdle == INotTracked) { ...c, pIdle: ITrackedYoung }
    else c

  /// observe_registered (sketch.rs): record fresh registrations (sample
  /// + clear), record-only stale ones (no clear, no sample). Returns
  /// (claim', emitsClear, recordOnly, lostUnderTenure).
  pure def observeRegistered(c: ClaimRec): (ClaimRec, bool, bool, bool) =
    if (c.phase != CRegistered or c.boot == BRecorded)
      (c, false, false, false)
    else if (c.boot == BFreshTenure or c.boot == BFreshOther)
      ({ ...c, boot: BRecorded }, true, false, false)
    else
      // Stale: the recency gate marks it recorded without a sample or a
      // clear; if the edge happened under the current tenure the sample
      // is lost solely to scheduler unreachability.
      ({ ...c, boot: BRecorded }, false, true, c.boot == BStaleTenure)

  /// The lease-acquire edge re-keys per-claim tenure tracking: nothing
  /// registered before this acquire counts as the current tenure, and
  /// recorded_boot is (about to be) cleared, so recorded claims become
  /// stale-other (record-only on the next observation, no clear).
  pure def acquireRekey(c: ClaimRec): ClaimRec =
    if (c.phase != CRegistered) c
    else {
      ...c,
      boot: if (c.boot == BFreshTenure) BFreshOther
            else if (c.boot == BStaleTenure) BStaleOther
            else if (c.boot == BRecorded) BStaleOther
            else c.boot,
    }

  // ------------------------------------------------------------------
  // 6. The tick family
  // ------------------------------------------------------------------
  /// Standby tick: the run loop processes a pending lose flag and skips
  /// the body. Wall-clock still passes; nothing observes.
  action tickStandby = all {
    not(isLeader),
    claims' = CLAIMS.mapBy(k => ageBootNonObserving(ageWallclock(claims.get(k)))),
    placeableArmed' = if (losePending) false else placeableArmed,
    losePending' = false,
    isLeader' = isLeader,
    reloadPending' = reloadPending,
    botStreak' = botStreak,
    iceMasked' = iceMasked,
    configuredCells' = configuredCells,
    ceilingsKnown' = ceilingsKnown,
    hadAcquire' = hadAcquire,
    effects' = Set(),
    violations' = violations,
  }

  /// A ⊥ tick below the streak threshold (the 2026-06-02 fix): the
  /// reload latch runs ahead of the body on EVERY leader tick, then
  /// the SHARED kube-only observation block — idle→busy pruning and
  /// Registered-edge recording with the clear component discarded —
  /// and nothing else: no reap, no inflight prune, no vanish
  /// detection, no create, no publish, no ack. The pre-fix early
  /// return skipped the latch as a documented no-op simplification;
  /// with observations present the latch/observe ordering is real
  /// (acquire-during-outage), so the prelude mirrors
  /// tickConsolidateOnlyWith exactly. Parameterized so deterministic
  /// runs can pin the PG-load outcome.
  action tickBotEarlyWith(pgLoadFails: bool): bool = all {
      isLeader,
      ENABLE_RPC_FAULT,
      botStreak + 1 < BOT_THRESHOLD,
      val aged = CLAIMS.mapBy(k => ageWallclock(claims.get(k)))
      // Reload-latch processing (run loop, before the body): prev_idle
      // clears unconditionally; the load may fail and leave the latch.
      val loadOk = reloadPending and not(pgLoadFails)
      val afterLatch = CLAIMS.mapBy(k =>
        if (reloadPending)
          { ...aged.get(k), pIdle: INotTracked,
            inflightTracked: if (loadOk) false else aged.get(k).inflightTracked }
        else aged.get(k))
      val reloadStillPending = reloadPending and not(loadOk)
      // Kube-only observations (shared with reconcile_once and
      // consolidate_only — the whole point of the fix).
      val observed = CLAIMS.mapBy(k => observeIdleToBusy(afterLatch.get(k)))
      val obsReg = CLAIMS.mapBy(k => observeRegistered(observed.get(k)))
      val afterObs = CLAIMS.mapBy(k => obsReg.get(k)._1)
      val staleRecordOnly = CLAIMS.filter(k => obsReg.get(k)._3).size() > 0
      val lostSamples = CLAIMS.filter(k => obsReg.get(k)._4).size() > 0
      all {
        claims' = afterObs,
        placeableArmed' = if (losePending) false else placeableArmed,
        losePending' = false,
        isLeader' = isLeader,
        reloadPending' = reloadStillPending,
        botStreak' = botStreak + 1,
        iceMasked' = iceMasked,
        configuredCells' = configuredCells,
        ceilingsKnown' = ceilingsKnown,
        hadAcquire' = hadAcquire,
        effects' = Set()
          .union(if (staleRecordOnly) Set(EStaleRecordOnly) else Set())
          .union(if (reloadStillPending) Set(EDegradedReloadTick) else Set())
          .union(if (reloadPending and CLAIMS.filter(k => claims.get(k).inflightTracked).size() > 0)
                   Set(EAcquireWithInflight) else Set()),
        // lostSamples retained as the latch: with observations on every
        // leader tick it is structurally unreachable in fault-rpc (the
        // bootSampleNotLost HOLD invariant pins exactly that).
        violations' = violations
          .union(if (lostSamples) Set(VBootSampleLost) else Set()),
      }
  }

  action tickBotEarly = {
    nondet pgLoadFails = boolChoices(ENABLE_LEASE_FAULT).oneOf()
    tickBotEarlyWith(pgLoadFails)
  }

  /// The consolidate-only tick: kube-only observations, idle/unhealthy
  /// reaps and the inflight prune; no create, no publish, no ack.
  /// Parameterized so deterministic runs can pin the PG-load outcome;
  /// `tickConsolidateOnly` below is the nondeterministic wrapper used
  /// by `step`.
  action tickConsolidateOnlyWith(pgLoadFails: bool): bool = all {
      isLeader,
      ENABLE_RPC_FAULT,
      botStreak + 1 >= BOT_THRESHOLD,
      val aged = CLAIMS.mapBy(k => ageWallclock(claims.get(k)))
      // Reload-latch processing (run loop, before the body): prev_idle
      // clears unconditionally; the load may fail and leave the latch.
      val loadOk = reloadPending and not(pgLoadFails)
      val afterLatch = CLAIMS.mapBy(k =>
        if (reloadPending)
          { ...aged.get(k), pIdle: INotTracked,
            inflightTracked: if (loadOk) false else aged.get(k).inflightTracked }
        else aged.get(k))
      val reloadStillPending = reloadPending and not(loadOk)
      // Kube-only observations (r43 bug_023: shared with reconcile_once).
      val observed = CLAIMS.mapBy(k => observeIdleToBusy(afterLatch.get(k)))
      val obsReg = CLAIMS.mapBy(k => observeRegistered(observed.get(k)))
      val afterObs = CLAIMS.mapBy(k => obsReg.get(k)._1)
      val staleRecordOnly = CLAIMS.filter(k => obsReg.get(k)._3).size() > 0
      val lostSamples = CLAIMS.filter(k => obsReg.get(k)._4).size() > 0
      // Unhealthy reap (the stuck in-flight arm) — ICE cells dropped
      // (no scheduler to report to), names still pruned from inflight.
      val stuckReaped = CLAIMS.filter(k =>
        afterObs.get(k).phase == CInFlight and afterObs.get(k).stuck)
      // The r40 bug_012 prune: detect_vanished runs here too, with its
      // marks discarded.
      val vanished = CLAIMS.filter(k =>
        afterObs.get(k).inflightTracked and not(stuckReaped.contains(k))
        and afterObs.get(k).phase == CAbsent)
      val spurious = vanished.filter(k => afterObs.get(k).ctrlReaped).size() > 0
      // Idle reap with an empty reserved set (no FFD this tick).
      val idleReaped = CLAIMS.filter(k =>
        val c = afterObs.get(k)
        c.phase == CRegistered and c.spell != SNotIdle and c.pIdle == ITrackedOld)
      val overReap = idleReaped.filter(k => afterObs.get(k).spell != SOld).size() > 0
      val claimsAfter = CLAIMS.mapBy(k =>
        val c = afterObs.get(k)
        if (stuckReaped.contains(k) or idleReaped.contains(k))
          { ...c, phase: CTerminating, stuck: false, spell: SNotIdle,
            busyTickSeen: false, boot: BRecorded, pIdle: INotTracked,
            inflightTracked: false, ctrlReaped: true }
        else if (vanished.contains(k)) { ...c, inflightTracked: false }
        else if (c.phase == CRegistered and c.spell == SNotIdle and c.inflightTracked)
          // detect_vanished drops registered/terminating entries too.
          { ...c, inflightTracked: false }
        else if (c.phase == CRegistered and c.inflightTracked)
          { ...c, inflightTracked: false }
        else if (c.phase == CTerminating and c.inflightTracked)
          { ...c, inflightTracked: false }
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
        effects' = Set()
          .union(if (idleReaped.size() > 0) Set(EIdleReap, EConsolidateOnlyReap) else Set())
          .union(if (stuckReaped.size() > 0) Set(EStuckReap, EConsolidateOnlyReap) else Set())
          .union(if (vanished.size() > 0) Set(EVanishDetected) else Set())
          .union(if (staleRecordOnly) Set(EStaleRecordOnly) else Set())
          .union(if (reloadStillPending) Set(EDegradedReloadTick) else Set())
          .union(if (reloadPending and CLAIMS.filter(k => claims.get(k).inflightTracked).size() > 0)
                   Set(EAcquireWithInflight) else Set()),
        violations' = violations
          .union(if (overReap) Set(VIdleOverReap) else Set())
          .union(if (spurious) Set(VSpuriousMark) else Set())
          .union(if (lostSamples) Set(VBootSampleLost) else Set()),
      }
  }

  action tickConsolidateOnly = {
    nondet pgLoadFails = boolChoices(ENABLE_LEASE_FAULT).oneOf()
    tickConsolidateOnlyWith(pgLoadFails)
  }

  /// The full reconcile_once tick (scheduler reachable). Parameterized
  /// over the per-tick faults and the opaque scheduler/FFD inputs so
  /// deterministic runs can pin them; `tickFull` below is the
  /// nondeterministic wrapper used by `step`.
  action tickFullWith(
    pgLoadFails: bool, ackFails: bool, createFails: bool,
    demandA: int, demandB: int, reservedChoice: Set[ClaimId]
  ): bool = all {
      isLeader,
      val aged = CLAIMS.mapBy(k => ageWallclock(claims.get(k)))
      // ---- run-loop prelude: lose flag, then the reload latch --------
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
      // ---- kube-only observations ------------------------------------
      val observed = CLAIMS.mapBy(k => observeIdleToBusy(afterLatch.get(k)))
      val obsReg = CLAIMS.mapBy(k => observeRegistered(observed.get(k)))
      val afterObs = CLAIMS.mapBy(k => obsReg.get(k)._1)
      val clears: Set[Cell] = CLAIMS.filter(k => obsReg.get(k)._2).map(k => afterObs.get(k).cell)
      val staleRecordOnly = CLAIMS.filter(k => obsReg.get(k)._3).size() > 0
      val lostSamples = CLAIMS.filter(k => obsReg.get(k)._4).size() > 0
      // ---- FFD outcome (opaque): reserved registered idle claims ------
      val reserved = reservedChoice.filter(k =>
        afterObs.get(k).phase == CRegistered and afterObs.get(k).spell != SNotIdle)
      // ---- publish the placeable set (always on a successful tick) ----
      // ---- unhealthy reap (stuck in-flight) + vanish detection --------
      val stuckReaped = CLAIMS.filter(k =>
        afterObs.get(k).phase == CInFlight and afterObs.get(k).stuck)
      val stuckMarks: Set[Cell] = stuckReaped.map(k => afterObs.get(k).cell)
      // Reap-name removal happens BEFORE detect_vanished, so the
      // controller's own deletes are not misread as Karpenter GC.
      val vanished = CLAIMS.filter(k =>
        afterObs.get(k).inflightTracked and not(stuckReaped.contains(k))
        and afterObs.get(k).phase == CAbsent)
      val vanishMarks: Set[Cell] = vanished.map(k => afterObs.get(k).inflightCell)
      val spurious = vanished.filter(k => afterObs.get(k).ctrlReaped).size() > 0
      val marksThisTick = stuckMarks.union(vanishMarks)
      val masked = iceMasked.union(marksThisTick)
      // ---- cover_deficit ----------------------------------------------
      val freeSlots = CLAIMS.filter(k => afterObs.get(k).phase == CAbsent)
      val liveCores = CLAIMS.filter(k => afterObs.get(k).phase != CAbsent and not(stuckReaped.contains(k))).size()
        + stuckReaped.size()  // a just-deleted claim is terminating and still bills
      val globalRemaining = if (MAX_FLEET_CORES > liveCores) MAX_FLEET_CORES - liveCores else 0
      val cellALive = CLAIMS.filter(k =>
        afterObs.get(k).phase != CAbsent and afterObs.get(k).cell == "cellA").size()
      val classRemainingA = if (CLASS_CAP_A > cellALive) CLASS_CAP_A - cellALive else 0
      // Demand for an unconfigured cell is dropped, never covered.
      val dropA = demandA > 0 and not(configuredCells.contains("cellA"))
      val dropB = demandB > 0 and not(configuredCells.contains("cellB"))
      val coverableA = demandA > 0 and ceilingsKnown and configuredCells.contains("cellA")
        and not(masked.contains("cellA"))
      val coverableB = demandB > 0 and ceilingsKnown and configuredCells.contains("cellB")
        and not(masked.contains("cellB"))
      // cellA first, then cellB (the rotation is fairness only).
      pure def min2(a: int, b: int): int = if (a < b) a else b
      val wantA = if (coverableA)
          min2(min2(demandA, classRemainingA), min2(globalRemaining, PER_CELL_TICK_CAP))
        else 0
      val wantB = if (coverableB)
          min2(min2(demandB, globalRemaining - wantA), PER_CELL_TICK_CAP)
        else 0
      val createNA = min2(wantA, freeSlots.size())
      val createNB = min2(wantB, freeSlots.size() - createNA)
      // Failed creates consume no budget and materialize no claim.
      val createdA = if (createFails) 0 else createNA
      val createdB = if (createFails) 0 else createNB
      val createAttempted = createNA + createNB > 0
      // Assign created claims to the lowest-ranked free slots, cellA first.
      val slotForA = freeSlots.filter(k => freeSlots.filter(o => claimRank(o) < claimRank(k)).size() < createdA)
      val slotForB = freeSlots.exclude(slotForA).filter(k =>
        freeSlots.exclude(slotForA).filter(o => claimRank(o) < claimRank(k)).size() < createdB)
      val classBound = demandA > 0 and configuredCells.contains("cellA") and ceilingsKnown
        and not(masked.contains("cellA")) and classRemainingA == 0 and globalRemaining > 0
      val ceilingsBlocked = (demandA > 0 or demandB > 0) and not(ceilingsKnown)
      // ---- ack (report_unfulfillable) -----------------------------------
      val ackPayload = marksThisTick.size() > 0 or clears.size() > 0
      val ackOk = ackPayload and not(ackFails)
      val iceMaskedAfter = if (ackOk) iceMasked.union(marksThisTick).exclude(clears) else iceMasked
      val clearOfMasked = ackOk and clears.intersect(iceMasked).size() > 0
      // ---- reap_idle ------------------------------------------------------
      val idleReaped = CLAIMS.filter(k =>
        val c = afterObs.get(k)
        c.phase == CRegistered and c.spell != SNotIdle and not(reserved.contains(k))
        and c.pIdle == ITrackedOld)
      val overReap = idleReaped.filter(k => afterObs.get(k).spell != SOld).size() > 0
      // ---- assemble the post-state ----------------------------------------
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
          // detect_vanished drops entries for registered/terminating claims.
          { ...c, inflightTracked: false }
        else c)
      // ---- budget violation checks (post-hoc, against the rule) -----------
      val totalAfter = CLAIMS.filter(k => claimsAfter.get(k).phase != CAbsent).size()
      val cellAAfter = CLAIMS.filter(k =>
        claimsAfter.get(k).phase != CAbsent and claimsAfter.get(k).cell == "cellA").size()
      val vBudgetGlobal = (createdA + createdB > 0) and totalAfter > MAX_FLEET_CORES
      val vBudgetClass = createdA > 0 and cellAAfter > CLASS_CAP_A
      val vBudgetPerTick = createdA > PER_CELL_TICK_CAP or createdB > PER_CELL_TICK_CAP
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
          .union(if (vBudgetPerTick) Set(VBudgetPerTick) else Set())
          .union(if (vNoCeil) Set(VCreateNoCeilings) else Set())
          .union(if (vUnconfigured) Set(VCreateUnconfigured) else Set())
          .union(if (vMasked) Set(VCoverMasked) else Set())
          .union(if (vGateProd) Set(VGateProducer) else Set()),
      }
  }

  action tickFull = {
    nondet pgLoadFails = boolChoices(ENABLE_LEASE_FAULT).oneOf()
    nondet ackFails = boolChoices(ENABLE_RPC_FAULT).oneOf()
    nondet createFails = boolChoices(ENABLE_RPC_FAULT).oneOf()
    nondet demandA = Set(0, 1, 2).oneOf()
    nondet demandB = Set(0, 1, 2).oneOf()
    nondet reservedChoice = CLAIMS.powerset().oneOf()
    tickFullWith(pgLoadFails, ackFails, createFails, demandA, demandB, reservedChoice)
  }

  // ------------------------------------------------------------------
  // 7. Environment actions
  // ------------------------------------------------------------------
  action envFrame = all {
    isLeader' = isLeader,
    losePending' = losePending,
    reloadPending' = reloadPending,
    botStreak' = botStreak,
    placeableArmed' = placeableArmed,
    iceMasked' = iceMasked,
    configuredCells' = configuredCells,
    ceilingsKnown' = ceilingsKnown,
    hadAcquire' = hadAcquire,
    effects' = effects,
    violations' = violations,
  }

  /// Karpenter launches the instance and the kubelet registers: the
  /// in-flight claim becomes Registered (idle — no pods yet). The
  /// registration is recency-fresh; whether it is under the current
  /// tenure depends on whether this replica leads right now.
  action claimRegisters(k: ClaimId): bool = all {
    claims.get(k).phase == CInFlight,
    not(claims.get(k).stuck),
    claims' = claims.set(k, { ...claims.get(k),
      phase: CRegistered, spell: SYoungUnseen, busyTickSeen: false,
      boot: if (isLeader) BFreshTenure else BFreshOther }),
    envFrame,
  }

  /// A builder pod binds to the node: the claim is busy.
  action claimGoesBusy(k: ClaimId): bool = all {
    claims.get(k).phase == CRegistered,
    claims.get(k).spell != SNotIdle,
    claims' = claims.set(k, { ...claims.get(k),
      spell: SNotIdle, busyTickSeen: false }),
    envFrame,
  }

  /// The last pod leaves the node. A busy period spans at least one
  /// tick (builds outlast the 10s tick), hence the busyTickSeen guard.
  action claimGoesIdle(k: ClaimId): bool = all {
    claims.get(k).phase == CRegistered,
    claims.get(k).spell == SNotIdle,
    claims.get(k).busyTickSeen,
    claims' = claims.set(k, { ...claims.get(k), spell: SYoungUnseen }),
    envFrame,
  }

  /// Karpenter posts a terminal launch failure (LaunchFailed /
  /// InsufficientCapacity) on an in-flight claim.
  action claimLaunchStuck(k: ClaimId): bool = all {
    ENABLE_KARPENTER_FAULT,
    claims.get(k).phase == CInFlight,
    not(claims.get(k).stuck),
    claims' = claims.set(k, { ...claims.get(k), stuck: true }),
    envFrame,
  }

  /// Karpenter GCs an in-flight claim (~1s after a launch failure —
  /// faster than the tick): it vanishes without ever registering.
  action karpenterGC(k: ClaimId): bool = all {
    ENABLE_KARPENTER_FAULT,
    claims.get(k).phase == CInFlight,
    claims' = claims.set(k, { ...absentClaim,
      inflightTracked: claims.get(k).inflightTracked,
      inflightCell: claims.get(k).inflightCell,
      ctrlReaped: claims.get(k).ctrlReaped }),
    envFrame,
  }

  /// Spot interruption / Karpenter disruption begins draining a
  /// registered claim.
  action karpenterTerminationEdge(k: ClaimId): bool = all {
    ENABLE_KARPENTER_FAULT,
    claims.get(k).phase == CRegistered,
    claims' = claims.set(k, { ...claims.get(k),
      phase: CTerminating, spell: SNotIdle, busyTickSeen: false,
      boot: BRecorded, pIdle: INotTracked }),
    envFrame,
  }

  /// The Karpenter finalizer finishes: the terminating claim is gone.
  action finalizerCompletes(k: ClaimId): bool = all {
    claims.get(k).phase == CTerminating,
    claims' = claims.set(k, { ...absentClaim,
      inflightTracked: claims.get(k).inflightTracked,
      inflightCell: claims.get(k).inflightCell,
      ctrlReaped: claims.get(k).ctrlReaped }),
    envFrame,
  }

  /// The scheduler clears an ICE mask on its own (heartbeat edge, TTL).
  action iceClears(cell: Cell): bool = all {
    iceMasked.contains(cell),
    iceMasked' = iceMasked.exclude(Set(cell)),
    claims' = claims,
    isLeader' = isLeader, losePending' = losePending,
    reloadPending' = reloadPending, botStreak' = botStreak,
    placeableArmed' = placeableArmed,
    configuredCells' = configuredCells, ceilingsKnown' = ceilingsKnown,
    hadAcquire' = hadAcquire, effects' = effects, violations' = violations,
  }

  /// The lease is lost: the on_lose hook arms the unarm-gate flag.
  action leaseLose = all {
    ENABLE_LEASE_FAULT,
    isLeader,
    isLeader' = false,
    losePending' = true,
    claims' = claims,
    reloadPending' = reloadPending, botStreak' = botStreak,
    placeableArmed' = placeableArmed, iceMasked' = iceMasked,
    configuredCells' = configuredCells, ceilingsKnown' = ceilingsKnown,
    hadAcquire' = hadAcquire, effects' = effects, violations' = violations,
  }

  /// The lease is (re-)acquired: the on_acquire hook arms the reload
  /// latch; per-claim tenure tracking is re-keyed.
  action leaseAcquire = all {
    ENABLE_LEASE_FAULT,
    not(isLeader),
    isLeader' = true,
    reloadPending' = true,
    hadAcquire' = true,
    claims' = CLAIMS.mapBy(k => acquireRekey(claims.get(k))),
    losePending' = losePending, botStreak' = botStreak,
    placeableArmed' = placeableArmed, iceMasked' = iceMasked,
    configuredCells' = configuredCells, ceilingsKnown' = ceilingsKnown,
    effects' = effects, violations' = violations,
  }

  /// The controller process restarts: every in-memory mirror resets,
  /// the gate channel starts unarmed, and leadership must be
  /// re-acquired. Anything registered before the restart is no longer
  /// under the current tenure, and the restart gap expires recency.
  action controllerRestarts = all {
    ENABLE_LEASE_FAULT,
    isLeader' = false,
    losePending' = false,
    reloadPending' = false,
    botStreak' = 0,
    placeableArmed' = false,
    claims' = CLAIMS.mapBy(k =>
      { ...ageBootNonObserving(acquireRekey(claims.get(k))),
        inflightTracked: false, pIdle: INotTracked }),
    iceMasked' = iceMasked,
    configuredCells' = configuredCells, ceilingsKnown' = ceilingsKnown,
    hadAcquire' = hadAcquire, effects' = effects, violations' = violations,
  }

  /// GetHwClassConfig finally delivers the global ceilings.
  action ceilingsLoad = all {
    not(ceilingsKnown),
    ceilingsKnown' = true,
    claims' = claims,
    isLeader' = isLeader, losePending' = losePending,
    reloadPending' = reloadPending, botStreak' = botStreak,
    placeableArmed' = placeableArmed, iceMasked' = iceMasked,
    configuredCells' = configuredCells,
    hadAcquire' = hadAcquire, effects' = effects, violations' = violations,
  }

  /// The config mirror loses a cell (hwClass removed mid-rollout).
  action configDropsCell(cell: Cell): bool = all {
    ENABLE_RPC_FAULT,
    configuredCells.contains(cell),
    configuredCells.size() > 1,
    configuredCells' = configuredCells.exclude(Set(cell)),
    claims' = claims,
    isLeader' = isLeader, losePending' = losePending,
    reloadPending' = reloadPending, botStreak' = botStreak,
    placeableArmed' = placeableArmed, iceMasked' = iceMasked,
    ceilingsKnown' = ceilingsKnown,
    hadAcquire' = hadAcquire, effects' = effects, violations' = violations,
  }

  /// The next refresh restores the cell.
  action configRestoresCell(cell: Cell): bool = all {
    ENABLE_RPC_FAULT,
    not(configuredCells.contains(cell)),
    configuredCells' = configuredCells.union(Set(cell)),
    claims' = claims,
    isLeader' = isLeader, losePending' = losePending,
    reloadPending' = reloadPending, botStreak' = botStreak,
    placeableArmed' = placeableArmed, iceMasked' = iceMasked,
    ceilingsKnown' = ceilingsKnown,
    hadAcquire' = hadAcquire, effects' = effects, violations' = violations,
  }

  // ------------------------------------------------------------------
  // 8. init / step
  // ------------------------------------------------------------------
  action init = all {
    claims' = CLAIMS.mapBy(k => absentClaim),
    isLeader' = true,
    losePending' = false,
    reloadPending' = false,
    botStreak' = 0,
    placeableArmed' = false,
    iceMasked' = Set(),
    configuredCells' = CELLS,
    ceilingsKnown' = START_CEILINGS_KNOWN,
    hadAcquire' = false,
    effects' = Set(),
    violations' = Set(),
  }

  action step = {
    nondet k = CLAIMS.oneOf()
    nondet cell = CELLS.oneOf()
    any {
      tickFull,
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

// =====================================================================
// Regime modules — one per check in nix/quint.nix.
// =====================================================================

/// Healthy lifecycle, no faults: create -> register -> busy/idle ->
/// idle-reap, the FFD reservation, the budgets and the gate publish.
module nodeclaimLifecycleBase {
  import nodeclaimLifecycle(
    MAX_FLEET_CORES = 2,
    CLASS_CAP_A = 1,
    PER_CELL_TICK_CAP = 1,
    BOT_THRESHOLD = 2,
    ENABLE_RPC_FAULT = false,
    ENABLE_LEASE_FAULT = false,
    ENABLE_KARPENTER_FAULT = false,
    START_CEILINGS_KNOWN = true
  ).*
}

/// Per-RPC ⊥ faults: the ⊥-streak with its early-return window and
/// consolidate-only mode, ack and create failures, the ceilings/config
/// not-yet-loaded states. The two pre-registered as-built
/// falsifications live here, with their deterministic reproducer runs.
module nodeclaimLifecycleFaultRpc {
  import nodeclaimLifecycle(
    MAX_FLEET_CORES = 2,
    CLASS_CAP_A = 1,
    PER_CELL_TICK_CAP = 1,
    BOT_THRESHOLD = 2,
    ENABLE_RPC_FAULT = true,
    ENABLE_LEASE_FAULT = false,
    ENABLE_KARPENTER_FAULT = false,
    START_CEILINGS_KNOWN = false
  ).*

  /// The conflation trace, post-fix: the claim is observed idle, goes
  /// busy and idle again across ⊥ ticks — the busy edge IS observed on
  /// the ⊥ tick (prune + re-seed), so the threshold-crossing full tick
  /// never over-reaps the fresh spell. The documented trace HOLDS
  /// end-to-end (it falsified VIdleOverReap on the pre-fix encoding).
  run idlePruneAcrossBotRun =
    init
      .then(ceilingsLoad)
      .then(tickFullWith(false, false, false, 1, 0, Set()))
      .then(claimRegisters("c1"))
      .then(tickFullWith(false, false, false, 0, 0, Set()))
      .then(claimGoesBusy("c1"))
      .then(tickBotEarlyWith(false))
      .then(claimGoesIdle("c1"))
      .then(tickFullWith(false, false, false, 0, 0, Set()))
      .expect(not(violations.contains(VIdleOverReap)))
      .expect(claims.get("c1").pIdle != INotTracked)

  /// The boot half of the same trace, post-fix: a Registered edge
  /// inside the pre-threshold window is recorded ON the ⊥ tick (clear
  /// discarded), so the recency gate never expires it and
  /// BootRecordedOnce's no-loss clause HOLDS.
  run bootRecordedAcrossBotRun =
    init
      .then(ceilingsLoad)
      .then(tickFullWith(false, false, false, 1, 0, Set()))
      .then(claimRegisters("c1"))
      .then(tickBotEarlyWith(false))
      .then(tickFullWith(false, false, false, 0, 0, Set()))
      .expect(not(violations.contains(VBootSampleLost)))
      .expect(claims.get("c1").boot == BRecorded)
}

/// Lease faults: lose/acquire edges, PG reload failure, controller
/// restart — the lease-edge polarity table's regime.
module nodeclaimLifecycleFaultLease {
  import nodeclaimLifecycle(
    MAX_FLEET_CORES = 2,
    CLASS_CAP_A = 1,
    PER_CELL_TICK_CAP = 1,
    BOT_THRESHOLD = 2,
    ENABLE_RPC_FAULT = false,
    ENABLE_LEASE_FAULT = true,
    ENABLE_KARPENTER_FAULT = false,
    START_CEILINGS_KNOWN = true
  ).*
}

/// Karpenter faults: launch failures, the 1s-GC-vs-10s-tick vanish race
/// and spot termination edges — the inflight-conservation / ICE-mark
/// regime.
module nodeclaimLifecycleFaultKarpenter {
  import nodeclaimLifecycle(
    MAX_FLEET_CORES = 2,
    CLASS_CAP_A = 1,
    PER_CELL_TICK_CAP = 1,
    BOT_THRESHOLD = 2,
    ENABLE_RPC_FAULT = false,
    ENABLE_LEASE_FAULT = false,
    ENABLE_KARPENTER_FAULT = true,
    START_CEILINGS_KNOWN = true
  ).*
}
