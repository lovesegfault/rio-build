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
//     OA2, updated post-1d): the dead_nodes-shaped input is SOLELY
//     the controller-side per-node deadline-expiry clustering over
//     the open-attempt ledger view (nodeclaim_pool/wedge.rs) — the
//     scheduler-reported set retired with the stream pools at the 1d
//     sweep (the proto reserves field 3/"dead_nodes"; history: the
//     input was the union of both until then). Still
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
//     MODEL-DIVERGENCE(law=ctrl.nodeclaim.mint-deficit-proportional; tree=rio-controller/src/reconcilers/nodeclaim_pool/cover.rs:sizing): the model certifies the two-term STRUCTURE (n = min(deficit, budget) in claim units) — NOT the sizing arithmetic (n_pack FFD packing, the budget/chunk integer granularity, the budget<chunk refusal arm) — retarget-by: encoding sizing arithmetic (a Stage-D item) or any change to the sizing law itself.
//   - The per-tick mark dedup (the BTreeSet in report_unfulfillable) is
//     by construction here (marks are a set); the soundness checks are
//     about WHICH cells are marked, not how many times.
//
// DIVERGENCE-HEADER GRAMMAR (minted round-9 WO-S7-2 — the R23 model
//   tier, machine-visible; the census-lint tier-2 drift grep consumes
//   the literal `MODEL-DIVERGENCE(` token): a law the model
//   deliberately does NOT certify carries, AT the model (and mirrored
//   at its invariant-map index row), one single-line header of the
//   exact form
//     MODEL-DIVERGENCE(law=<rule-or-law id>; tree=<file:anchor>): <what is not certified> — retarget-by: <trigger>
//   A change touching a header's `law=` id MUST either re-target the
//   model in the same wave or rewrite the header — a deferral minuted
//   anywhere else (the merged_bug_003 shape: a census-test comment in
//   a foreign crate) does not count as a divergence record.
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
//   provisioningBudget    the budget-brake mint law (live_049 L1):
//                         global and per-class budgets hold with
//                         terminating claims still counted; failed
//                         creates consume no budget; and a deficit
//                         within budget mints FULLY in one tick
//                         (deficitFullyMinted — the per-cell-per-tick
//                         cap is retired from relation AND invariant)
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
    | EEdgeIdleClear
    | EBufferedClearDelivered
    | EBindingCleared
    | EBurstFullMint
    | EDeferKept

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
    | VDeficitDeferred
    | VCreateNoCeilings
    | VCreateUnconfigured
    | VCoverMasked
    | VIdleSpellLost
    | VClearEvidenceDropped
    | VClearLostOnAckFailure
    | VStaleBinding
    | VDeferredDemandReaped

  // ------------------------------------------------------------------
  // 2. Constants
  // ------------------------------------------------------------------
  /// max_fleet_cores, in claim-sized units (every claim is one core).
  const MAX_FLEET_CORES: int
  /// Per-hwClass fleet cap for cellA (cellB has none — global only),
  /// mirroring HwClassConfig::fleet_cap_for == None for absent caps.
  const CLASS_CAP_A: int
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
  /// C2 area-G axis pair (bug_346): MACHINERY moves the edge-token
  /// state + latch; ACQUIRE_EPOCH selects the FIXED law (the
  /// amplify-class prev_idle clear fires once per acquisition, keyed
  /// on the epoch token) vs the as-built per-reload-pending-tick
  /// re-clear. All pre-C2 regimes bind both false — state-space
  /// identical.
  const ENABLE_EPOCH_MACHINERY: bool
  const ENABLE_ACQUIRE_EPOCH: bool
  /// C2 area-F axis pair (merged_bug_007): MACHINERY moves the
  /// pending-clear buffer + the drop latch; EVIDENCE_BUFFER selects
  /// the FIXED law (scheduler-bound ICE-clears produced on ⊥ /
  /// consolidate-only ticks are buffered and drained into the next
  /// full tick's ack) vs the as-built discard.
  const ENABLE_CLEARBUF_MACHINERY: bool
  const ENABLE_EVIDENCE_BUFFER: bool
  /// merged_bug_045 (bughunt-2 slot 5): commit-on-Ack. With the latch
  /// the buffered clears ship BY READ and the buffer empties ONLY when
  /// the delivering ack lands; off = the as-built mem::take before the
  /// RPC (an ack failure on exactly the delivering tick loses the
  /// batch — the retired 007 residual). Ghost-framed: only meaningful
  /// under ENABLE_CLEARBUF_MACHINERY ∧ ENABLE_EVIDENCE_BUFFER.
  const ENABLE_ACK_LATCH: bool
  /// C2 area-E axis pair (bug_285): MACHINERY moves the scheduler-side
  /// binding map + latch; SNAPSHOT_PRESENCE selects the FIXED law
  /// (every full-tick ack carries the snapshot — present-and-empty
  /// CLEARS) vs the as-built empty-suppressed legacy field.
  const ENABLE_SNAPSHOT_MACHINERY: bool
  const ENABLE_SNAPSHOT_PRESENCE: bool
  /// Round-10 area (merged_bug_012, WO-S4-2): MACHINERY moves the
  /// per-full-tick deferred-demand cell set (the FFD admission
  /// window's typed remainder, abstracted to the cells it routes to);
  /// DEFER_DEMAND selects the FIXED law (window-deferred demand
  /// counts for the NA keep-condition -- an idle claim in a cell with
  /// deferred demand is NOT idle-reaped this tick; the threshold rose
  /// with the demand) vs the as-built absence fold (the consolidator
  /// read remaindered cells as zero demand: the idle node was reaped
  /// at the floor and re-minted on the next window rotation -- the
  /// reap-then-re-mint churn). All pre-round-10 regimes bind both
  /// false -- deferredCells is constant-empty there and the reachable
  /// state space is unchanged.
  ///
  /// Divergence header (the bw9 grammar): this plane covers the
  /// CONSOLIDATE consumer of the deferred letter only -- the
  /// demand-visibility consumers (want-map membership, spawn-lane
  /// split, queued bound) live in the pool/jobs plane this model does
  /// not carry; their verification is the W10-AI/AK rust battery.
  /// Consolidate-only ticks mirror the code's bottom posture
  /// (deferred := empty -- no scheduler view, no deferral set; the
  /// stale set is never consulted).
  const ENABLE_DEFER_MACHINERY: bool
  const ENABLE_DEFER_DEMAND: bool

  pure val CLAIMS: Set[ClaimId] = Set("c1", "c2")
  pure val CELLS: Set[Cell] = Set("cellA", "cellB")

  pure def boolChoices(faultOn: bool): Set[bool] =
    if (faultOn) Set(false, true) else Set(false)

  /// Round-10/merged_bug_012: the per-full-tick deferred-cell choices.
  /// Off-machinery the only choice is the empty set, so pre-round-10
  /// regimes branch exactly as before (state space unchanged).
  pure val deferChoices: Set[Set[Cell]] =
    if (ENABLE_DEFER_MACHINERY) CELLS.powerset() else Set(Set())

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
  /// C2/346: the current acquisition's edge actions have run (the
  /// epoch-token cursor, collapsed to a bool: false = an acquisition
  /// edge is pending consumption). Constant true off-machinery.
  var edgeClearDone: bool
  /// C2/007: scheduler-bound ICE-clears consumed by non-delivering
  /// ticks, awaiting the next full tick. Constant empty off-machinery.
  var pendingClear: Set[Cell]
  /// C2/285: the SCHEDULER's authoritative-binding map, abstracted to
  /// the set of claims it believes bound. Constant empty off-machinery.
  var schedBinding: Set[ClaimId]
  /// Round-10/merged_bug_012: cells with window-deferred demand as of
  /// the last full tick. Constant empty off-machinery.
  var deferredCells: Set[Cell]
  var effects: Set[Effect]
  var violations: Set[Violation]

  // ------------------------------------------------------------------
  // 4. Invariants and witnesses
  // ------------------------------------------------------------------
  val boundsOK = botStreak >= 0 and botStreak <= BOT_THRESHOLD
  val idleReapSafety = not(violations.contains(VIdleOverReap))
  /// Round-10/merged_bug_012: window-deferred demand is never read as
  /// demand-gone by the idle reap (the consolidate consumer of the
  /// typed deferred letter).
  val deferredDemandKept = not(violations.contains(VDeferredDemandReaped))
  val iceMarkSoundness = not(violations.contains(VSpuriousMark))
  val bootSampleNotLost = not(violations.contains(VBootSampleLost))
  val noMassClearAfterFailover = not(violations.contains(VStaleClear))
  val reloadLatchRespected = not(violations.contains(VStaleOverwrite))
  val singleEffectiveProvisioner = not(violations.contains(VStandbyEffect))
  val gateProducerGuarantee = not(violations.contains(VGateProducer))
  /// The deficit-full-mint leaf, NAMED single-proposition (P7): a
  /// deficit within budget (and the slot universe) mints fully in one
  /// tick — the landed live_049 L1 law, satisfiable and asserted
  /// (round-9 WO-S7-2; pre-retarget the model asserted the RETIRED
  /// per-cell-per-tick cap here and refuted the tree).
  val deficitFullyMinted = not(violations.contains(VDeficitDeferred))
  val provisioningBudget =
    not(violations.contains(VBudgetGlobal))
    and not(violations.contains(VBudgetClass))
    and deficitFullyMinted
  val coverRespectsMask = not(violations.contains(VCoverMasked))
  val degradedCoverPolarity =
    not(violations.contains(VCreateNoCeilings))
    and not(violations.contains(VCreateUnconfigured))

  // Witnesses (expected to be VIOLATED).
  val canReachIdleReap = not(effects.contains(EIdleReap))
  /// Non-vacuity for the defer plane: a reap-eligible idle claim in a
  /// deferred-demand cell was actually KEPT under the fixed law.
  val canReachDeferKept = not(effects.contains(EDeferKept))
  val canReachVanishMark = not(effects.contains(EVanishDetected))
  val canReachClearAfterMark = not(effects.contains(EClearOfMaskedCell))
  val canReachConsolidateOnlyReap = not(effects.contains(EConsolidateOnlyReap))
  val canReachClassBudgetBound = not(effects.contains(EClassBudgetBound))
  val canReachCreateFailure = not(effects.contains(ECreateFailed))
  val canReachCeilingsFailClosed = not(effects.contains(EFailClosedNoCeilings))
  val canReachUnknownCellDrop = not(effects.contains(EUnknownCellDrop))
  val canReachCreate = not(effects.contains(ECreateOk))
  val canReachBurstFullMint = not(effects.contains(EBurstFullMint))
  val canReachAcquireWithInflight = not(effects.contains(EAcquireWithInflight))
  val canReachDegradedReloadTick = not(effects.contains(EDegradedReloadTick))
  val canReachFreshClearAfterAcquire =
    not(hadAcquire and effects.contains(EClearEmitted))
  val canReachStaleRecordOnly = not(effects.contains(EStaleRecordOnly))

  /// C2/346: an idle spell seeded under the current tenure survives a
  /// reload-Err loop — the amplify-class prev_idle clear is an
  /// acquisition-EDGE action, never a per-tick action. Falsified
  /// as-built (the boolean latch re-cleared every Err tick); holds
  /// with ENABLE_ACQUIRE_EPOCH.
  val idleSpellSurvivesReloadErr = not(violations.contains(VIdleSpellLost))
  /// C2/007: a fresh-tenure Registered edge consumed by a tick that
  /// cannot deliver its ICE-clear (⊥ pre-threshold / consolidate-only)
  /// never DROPS the clear — it is buffered for the next full tick.
  /// Falsified as-built (the discard); holds with
  /// ENABLE_EVIDENCE_BUFFER.
  val iceClearDelivered = not(violations.contains(VClearEvidenceDropped))
  /// merged_bug_045: the buffered clear batch survives the delivering
  /// tick's ack failure — commit-on-Ack means no moved-out value
  /// exists between the read and the commit. Falsified with the latch
  /// off (the mem::take-before-RPC shape); holds with ENABLE_ACK_LATCH.
  val clearSurvivesAckFailure = not(violations.contains(VClearLostOnAckFailure))
  /// C2/285: after every successful full-tick ack the scheduler's
  /// binding map equals the controller's bound set — present-and-empty
  /// clears (scale-to-zero says so). Falsified as-built (the all-empty
  /// suppression + empty-means-no-snapshot legacy wire); holds with
  /// ENABLE_SNAPSHOT_PRESENCE.
  val ackCarriesSnapshot = not(violations.contains(VStaleBinding))

  // C2 reachability witnesses (expected-violation checks in the hold
  // regimes — non-vacuity).
  val canReachEdgeIdleClear = not(effects.contains(EEdgeIdleClear))
  val canReachBufferedClearDelivered = not(effects.contains(EBufferedClearDelivered))
  val canReachBindingCleared = not(effects.contains(EBindingCleared))

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
    edgeClearDone' = edgeClearDone,
    pendingClear' = pendingClear,
    schedBinding' = schedBinding,
    deferredCells' = deferredCells,
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
      // C2/346: the amplify-class prev_idle clear. Fixed law: keyed on
      // the acquisition-edge token (once per acquisition, even across
      // a reload-Err loop). As-built law: keyed on the still-pending
      // reload latch (re-fires every Err tick — the bug).
      val clearIdleNow =
        if (ENABLE_EPOCH_MACHINERY and ENABLE_ACQUIRE_EPOCH) not(edgeClearDone)
        else reloadPending
      // merged_bug_004: inflight tracking is tenure state — fixed law
      // (epoch machinery on): cleared on the acquisition edge with the
      // other suppress-polarity buffers, exactly once per acquisition,
      // NEVER on the reload-Ok arm (the Ok-arm clear destroyed
      // tracking for claims created during the Err-retry window, so
      // their vanish never emitted an ICE mark). As-built (pre-epoch)
      // law retained for the machinery-off instances: cleared on
      // loadOk.
      val clearInflightNow =
        if (ENABLE_EPOCH_MACHINERY and ENABLE_ACQUIRE_EPOCH) not(edgeClearDone)
        else loadOk
      val idleTrackedNow =
        CLAIMS.filter(k => aged.get(k).pIdle != INotTracked).size() > 0
      // The latch: a re-clear — the edge actions already ran for this
      // acquisition, yet a tick clears tracked idle spells again.
      val vIdleLost =
        ENABLE_EPOCH_MACHINERY and clearIdleNow and edgeClearDone and idleTrackedNow
      // The witness: the EDGE clear acting on stale pre-tenure spells.
      val edgeClearActed =
        ENABLE_EPOCH_MACHINERY and clearIdleNow and not(edgeClearDone) and idleTrackedNow
      val afterLatch = CLAIMS.mapBy(k =>
        { ...aged.get(k),
          pIdle: if (clearIdleNow) INotTracked else aged.get(k).pIdle,
          inflightTracked: if (clearInflightNow) false else aged.get(k).inflightTracked })
      val reloadStillPending = reloadPending and not(loadOk)
      // Kube-only observations (shared with reconcile_once and
      // consolidate_only — the whole point of the fix).
      val observed = CLAIMS.mapBy(k => observeIdleToBusy(afterLatch.get(k)))
      val obsReg = CLAIMS.mapBy(k => observeRegistered(observed.get(k)))
      val afterObs = CLAIMS.mapBy(k => obsReg.get(k)._1)
      val staleRecordOnly = CLAIMS.filter(k => obsReg.get(k)._3).size() > 0
      val lostSamples = CLAIMS.filter(k => obsReg.get(k)._4).size() > 0
      // C2/007: scheduler-bound clears produced by THIS non-delivering
      // tick — buffered (fixed) or dropped-with-latch (as-built).
      val freshClears: Set[Cell] =
        CLAIMS.filter(k => obsReg.get(k)._2).map(k => afterObs.get(k).cell)
      val vClearDropped =
        ENABLE_CLEARBUF_MACHINERY and not(ENABLE_EVIDENCE_BUFFER)
        and freshClears.size() > 0
      all {
        claims' = afterObs,
        edgeClearDone' = if (ENABLE_EPOCH_MACHINERY) true else edgeClearDone,
        pendingClear' =
          if (ENABLE_CLEARBUF_MACHINERY and ENABLE_EVIDENCE_BUFFER)
            pendingClear.union(freshClears)
          else pendingClear,
        schedBinding' = schedBinding,
        deferredCells' = deferredCells,
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
          .union(if (edgeClearActed) Set(EEdgeIdleClear) else Set())
          .union(if (reloadPending and CLAIMS.filter(k => claims.get(k).inflightTracked).size() > 0)
                   Set(EAcquireWithInflight) else Set()),
        // lostSamples retained as the latch: with observations on every
        // leader tick it is structurally unreachable in fault-rpc (the
        // bootSampleNotLost HOLD invariant pins exactly that).
        violations' = violations
          .union(if (lostSamples) Set(VBootSampleLost) else Set())
          .union(if (vIdleLost) Set(VIdleSpellLost) else Set())
          .union(if (vClearDropped) Set(VClearEvidenceDropped) else Set()),
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
      // C2/346: the amplify-class prev_idle clear. Fixed law: keyed on
      // the acquisition-edge token (once per acquisition, even across
      // a reload-Err loop). As-built law: keyed on the still-pending
      // reload latch (re-fires every Err tick — the bug).
      val clearIdleNow =
        if (ENABLE_EPOCH_MACHINERY and ENABLE_ACQUIRE_EPOCH) not(edgeClearDone)
        else reloadPending
      // merged_bug_004: inflight tracking is tenure state — fixed law
      // (epoch machinery on): cleared on the acquisition edge with the
      // other suppress-polarity buffers, exactly once per acquisition,
      // NEVER on the reload-Ok arm (the Ok-arm clear destroyed
      // tracking for claims created during the Err-retry window, so
      // their vanish never emitted an ICE mark). As-built (pre-epoch)
      // law retained for the machinery-off instances: cleared on
      // loadOk.
      val clearInflightNow =
        if (ENABLE_EPOCH_MACHINERY and ENABLE_ACQUIRE_EPOCH) not(edgeClearDone)
        else loadOk
      val idleTrackedNow =
        CLAIMS.filter(k => aged.get(k).pIdle != INotTracked).size() > 0
      // The latch: a re-clear — the edge actions already ran for this
      // acquisition, yet a tick clears tracked idle spells again.
      val vIdleLost =
        ENABLE_EPOCH_MACHINERY and clearIdleNow and edgeClearDone and idleTrackedNow
      // The witness: the EDGE clear acting on stale pre-tenure spells.
      val edgeClearActed =
        ENABLE_EPOCH_MACHINERY and clearIdleNow and not(edgeClearDone) and idleTrackedNow
      val afterLatch = CLAIMS.mapBy(k =>
        { ...aged.get(k),
          pIdle: if (clearIdleNow) INotTracked else aged.get(k).pIdle,
          inflightTracked: if (clearInflightNow) false else aged.get(k).inflightTracked })
      val reloadStillPending = reloadPending and not(loadOk)
      // Kube-only observations (r43 bug_023: shared with reconcile_once).
      val observed = CLAIMS.mapBy(k => observeIdleToBusy(afterLatch.get(k)))
      val obsReg = CLAIMS.mapBy(k => observeRegistered(observed.get(k)))
      val afterObs = CLAIMS.mapBy(k => obsReg.get(k)._1)
      val staleRecordOnly = CLAIMS.filter(k => obsReg.get(k)._3).size() > 0
      val lostSamples = CLAIMS.filter(k => obsReg.get(k)._4).size() > 0
      // C2/007: same buffered-or-dropped split as the ⊥ tick.
      val freshClears: Set[Cell] =
        CLAIMS.filter(k => obsReg.get(k)._2).map(k => afterObs.get(k).cell)
      val vClearDropped =
        ENABLE_CLEARBUF_MACHINERY and not(ENABLE_EVIDENCE_BUFFER)
        and freshClears.size() > 0
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
        edgeClearDone' = if (ENABLE_EPOCH_MACHINERY) true else edgeClearDone,
        pendingClear' =
          if (ENABLE_CLEARBUF_MACHINERY and ENABLE_EVIDENCE_BUFFER)
            pendingClear.union(freshClears)
          else pendingClear,
        schedBinding' = schedBinding,
        // Round-10/merged_bug_012: the bottom posture — consolidate-only
        // has no scheduler view, hence no deferral set (the code passes
        // an empty deferred slice); the stale set is never consulted.
        deferredCells' = Set(),
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
          .union(if (edgeClearActed) Set(EEdgeIdleClear) else Set())
          .union(if (reloadPending and CLAIMS.filter(k => claims.get(k).inflightTracked).size() > 0)
                   Set(EAcquireWithInflight) else Set()),
        violations' = violations
          .union(if (overReap) Set(VIdleOverReap) else Set())
          .union(if (spurious) Set(VSpuriousMark) else Set())
          .union(if (lostSamples) Set(VBootSampleLost) else Set())
          .union(if (vIdleLost) Set(VIdleSpellLost) else Set())
          .union(if (vClearDropped) Set(VClearEvidenceDropped) else Set()),
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
    demandA: int, demandB: int, reservedChoice: Set[ClaimId],
    deferredChoice: Set[Cell]
  ): bool = all {
      isLeader,
      val aged = CLAIMS.mapBy(k => ageWallclock(claims.get(k)))
      // ---- run-loop prelude: lose flag, then the reload latch --------
      val gateAfterLose = if (losePending) false else placeableArmed
      val loadOk = reloadPending and not(pgLoadFails)
      // C2/346: the amplify-class prev_idle clear. Fixed law: keyed on
      // the acquisition-edge token (once per acquisition, even across
      // a reload-Err loop). As-built law: keyed on the still-pending
      // reload latch (re-fires every Err tick — the bug).
      val clearIdleNow =
        if (ENABLE_EPOCH_MACHINERY and ENABLE_ACQUIRE_EPOCH) not(edgeClearDone)
        else reloadPending
      // merged_bug_004: inflight tracking is tenure state — fixed law
      // (epoch machinery on): cleared on the acquisition edge with the
      // other suppress-polarity buffers, exactly once per acquisition,
      // NEVER on the reload-Ok arm (the Ok-arm clear destroyed
      // tracking for claims created during the Err-retry window, so
      // their vanish never emitted an ICE mark). As-built (pre-epoch)
      // law retained for the machinery-off instances: cleared on
      // loadOk.
      val clearInflightNow =
        if (ENABLE_EPOCH_MACHINERY and ENABLE_ACQUIRE_EPOCH) not(edgeClearDone)
        else loadOk
      val idleTrackedNow =
        CLAIMS.filter(k => aged.get(k).pIdle != INotTracked).size() > 0
      // The latch: a re-clear — the edge actions already ran for this
      // acquisition, yet a tick clears tracked idle spells again.
      val vIdleLost =
        ENABLE_EPOCH_MACHINERY and clearIdleNow and edgeClearDone and idleTrackedNow
      // The witness: the EDGE clear acting on stale pre-tenure spells.
      val edgeClearActed =
        ENABLE_EPOCH_MACHINERY and clearIdleNow and not(edgeClearDone) and idleTrackedNow
      val afterLatch = CLAIMS.mapBy(k =>
        { ...aged.get(k),
          pIdle: if (clearIdleNow) INotTracked else aged.get(k).pIdle,
          inflightTracked: if (clearInflightNow) false else aged.get(k).inflightTracked })
      val reloadStillPending = reloadPending and not(loadOk)
      val acquireWithInflight =
        reloadPending and CLAIMS.filter(k => claims.get(k).inflightTracked).size() > 0
      // ---- kube-only observations ------------------------------------
      val observed = CLAIMS.mapBy(k => observeIdleToBusy(afterLatch.get(k)))
      val obsReg = CLAIMS.mapBy(k => observeRegistered(observed.get(k)))
      val afterObs = CLAIMS.mapBy(k => obsReg.get(k)._1)
      val freshClearsFull: Set[Cell] =
        CLAIMS.filter(k => obsReg.get(k)._2).map(k => afterObs.get(k).cell)
      // C2/007: the buffered clears from ⊥/consolidate ticks drain into
      // THIS tick's ack alongside the fresh ones.
      val clears: Set[Cell] = freshClearsFull.union(
        if (ENABLE_CLEARBUF_MACHINERY and ENABLE_EVIDENCE_BUFFER) pendingClear
        else Set())
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
      // live_049 L1 / round-9 WO-S7-2 (merged_bug_003): the budget-brake
      // two-term mint law — demand × budget, nothing else. In claim
      // units (every claim one core, chunk = 1) the production
      // `n = n_pack.min(budget / chunk)` (cover.rs::sizing) is
      // n = min(deficit, remaining budget); the retired flat
      // per-cell-per-tick cap is GONE from the relation exactly as
      // 2e2ce1a32 removed it from the tree.
      val wantA = if (coverableA)
          min2(demandA, min2(classRemainingA, globalRemaining))
        else 0
      val wantB = if (coverableB)
          min2(demandB, globalRemaining - wantA)
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
      val bufferedDelivered =
        ENABLE_CLEARBUF_MACHINERY and ENABLE_EVIDENCE_BUFFER
        and ackOk and pendingClear.size() > 0
      // merged_bug_045: latch OFF + a failed delivering ack = the
      // buffered batch is lost (mem::take consumed it before the RPC).
      val vClearLostOnAck =
        ENABLE_CLEARBUF_MACHINERY and ENABLE_EVIDENCE_BUFFER
        and not(ENABLE_ACK_LATCH) and ackFails and pendingClear.size() > 0
      // C2/285: the binding snapshot. Fixed law: the full-tick ack is
      // ALWAYS sent (the snapshot is its payload — present-and-empty
      // clears) and the scheduler wholesale-rebuilds. As-built law:
      // the all-empty tick is suppressed and an empty bound set means
      // "no snapshot" — the scheduler keeps its stale map.
      val boundNow = CLAIMS.filter(k =>
        afterObs.get(k).phase == CRegistered and afterObs.get(k).spell == SNotIdle)
      val snapAckOk =
        if (ENABLE_SNAPSHOT_PRESENCE) not(ackFails)
        else (ackPayload or boundNow.size() > 0) and not(ackFails)
      val schedBindingAfter =
        if (not(ENABLE_SNAPSHOT_MACHINERY)) schedBinding
        else if (ENABLE_SNAPSHOT_PRESENCE and snapAckOk) boundNow
        else if (not(ENABLE_SNAPSHOT_PRESENCE) and snapAckOk and boundNow.size() > 0) boundNow
        else schedBinding
      val vStaleBinding =
        ENABLE_SNAPSHOT_MACHINERY and snapAckOk and schedBindingAfter != boundNow
      val bindingCleared =
        ENABLE_SNAPSHOT_MACHINERY and ENABLE_SNAPSHOT_PRESENCE and snapAckOk
        and boundNow.size() == 0 and schedBinding.size() > 0
      // ---- reap_idle ------------------------------------------------------
      // Round-10/merged_bug_012: the deferred-demand cells of THIS
      // tick's FFD window (machinery-gated; the fixed law keeps an
      // otherwise-reap-eligible idle claim whose cell still has
      // deferred demand -- e_fitting_cores counts the deferred
      // population, so the NA threshold rises with it).
      val deferredNow = if (ENABLE_DEFER_MACHINERY) deferredChoice else Set()
      val idleReaped = CLAIMS.filter(k =>
        val c = afterObs.get(k)
        c.phase == CRegistered and c.spell != SNotIdle and not(reserved.contains(k))
        and c.pIdle == ITrackedOld
        and not(ENABLE_DEFER_DEMAND and deferredNow.contains(c.cell)))
      val deferKept = ENABLE_DEFER_DEMAND and CLAIMS.filter(k =>
        val c = afterObs.get(k)
        c.phase == CRegistered and c.spell != SNotIdle and not(reserved.contains(k))
        and c.pIdle == ITrackedOld and deferredNow.contains(c.cell)).size() > 0
      val vDeferReaped = ENABLE_DEFER_MACHINERY and
        idleReaped.filter(k => deferredNow.contains(afterObs.get(k).cell)).size() > 0
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
      // The deficit-full-mint law (ctrl.nodeclaim.mint-deficit-
      // proportional, live_049 L1): on a successful covering tick the
      // demanded-and-affordable-and-representable deficit mints FULLY
      // in ONE tick — a smaller create is the retired-cap class (or
      // any reintroduced pacing term). Post-hoc against the rule, not
      // the relation's own vals: the bounds are restated from demand,
      // budget and the free-slot universe. B's budget bound uses
      // createdA (the actual debit, the controller's
      // created_this_tick semantics); in this 2-slot universe a
      // slot-bound A forces slots-for-B to 0, so created-vs-want
      // debit cannot diverge on the B bound.
      val vDeficitDeferred =
        (coverableA and not(createFails)
          and createdA < min2(min2(demandA, classRemainingA),
                              min2(globalRemaining, freeSlots.size())))
        or
        (coverableB and not(createFails)
          and createdB < min2(min2(demandB, globalRemaining - createdA),
                              freeSlots.size() - createdA))
      // The burst leaf (non-vacuity): one tick mints MORE than the
      // retired flat cap (1) ever allowed in a cell — the exact
      // population the pre-retirement model made unreachable
      // (vacuous burst invariants, merged_bug_003).
      val burstFullMint = createdA > 1 or createdB > 1
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
        edgeClearDone' = if (ENABLE_EPOCH_MACHINERY) true else edgeClearDone,
        // merged_bug_045: with the latch the buffer empties only when
        // the delivering ack LANDS (commit-on-Ack — retained on
        // ackFails for the next tick); latch off = the as-built
        // mem::take before the RPC (the batch is lost, recorded as
        // VClearLostOnAckFailure below).
        pendingClear' =
          if (ENABLE_CLEARBUF_MACHINERY and ENABLE_EVIDENCE_BUFFER)
            (if (ENABLE_ACK_LATCH and ackFails) pendingClear else Set())
          else pendingClear,
        schedBinding' = schedBindingAfter,
        deferredCells' = deferredNow,
        placeableArmed' = true,
        losePending' = false,
        isLeader' = isLeader,
        reloadPending' = reloadStillPending,
        botStreak' = 0,
        iceMasked' = iceMaskedAfter,
        configuredCells' = configuredCells,
        ceilingsKnown' = ceilingsKnown,
        hadAcquire' = hadAcquire,
        // Width-not-depth (the rust evaluator's serde recursion
        // limit): one flatten over the per-effect singletons.
        effects' = Set(EPublished).union(Set(
          if (createdA + createdB > 0) Set(ECreateOk) else Set(),
          if (burstFullMint) Set(EBurstFullMint) else Set(),
          if (deferKept) Set(EDeferKept) else Set(),
          if (createAttempted and createFails) Set(ECreateFailed) else Set(),
          if (ackOk and marksThisTick.size() > 0) Set(EIceMarkEmitted) else Set(),
          if (ackOk and clears.size() > 0) Set(EClearEmitted) else Set(),
          if (clearOfMasked) Set(EClearOfMaskedCell) else Set(),
          if (vanished.size() > 0) Set(EVanishDetected) else Set(),
          if (stuckReaped.size() > 0) Set(EStuckReap) else Set(),
          if (idleReaped.size() > 0) Set(EIdleReap) else Set(),
          if (classBound) Set(EClassBudgetBound) else Set(),
          if (ceilingsBlocked) Set(EFailClosedNoCeilings) else Set(),
          if (dropA or dropB) Set(EUnknownCellDrop) else Set(),
          if (acquireWithInflight) Set(EAcquireWithInflight) else Set(),
          if (reloadStillPending) Set(EDegradedReloadTick) else Set(),
          if (staleRecordOnly) Set(EStaleRecordOnly) else Set(),
          if (edgeClearActed) Set(EEdgeIdleClear) else Set(),
          if (bufferedDelivered) Set(EBufferedClearDelivered) else Set(),
          if (bindingCleared) Set(EBindingCleared) else Set()).flatten()),
        // Width-not-depth (the rust evaluator's serde recursion
        // limit bites on long .union chains): one flatten over the
        // per-violation singletons.
        violations' = violations.union(Set(
          if (overReap) Set(VIdleOverReap) else Set(),
          if (vDeferReaped) Set(VDeferredDemandReaped) else Set(),
          if (spurious) Set(VSpuriousMark) else Set(),
          if (lostSamples) Set(VBootSampleLost) else Set(),
          if (vBudgetGlobal) Set(VBudgetGlobal) else Set(),
          if (vBudgetClass) Set(VBudgetClass) else Set(),
          if (vDeficitDeferred) Set(VDeficitDeferred) else Set(),
          if (vNoCeil) Set(VCreateNoCeilings) else Set(),
          if (vUnconfigured) Set(VCreateUnconfigured) else Set(),
          if (vMasked) Set(VCoverMasked) else Set(),
          if (vGateProd) Set(VGateProducer) else Set(),
          if (vIdleLost) Set(VIdleSpellLost) else Set(),
          if (vClearLostOnAck) Set(VClearLostOnAckFailure) else Set(),
          if (vStaleBinding) Set(VStaleBinding) else Set()).flatten()),
      }
  }

  action tickFull = {
    nondet pgLoadFails = boolChoices(ENABLE_LEASE_FAULT).oneOf()
    nondet ackFails = boolChoices(ENABLE_RPC_FAULT).oneOf()
    nondet createFails = boolChoices(ENABLE_RPC_FAULT).oneOf()
    nondet demandA = Set(0, 1, 2).oneOf()
    nondet demandB = Set(0, 1, 2).oneOf()
    nondet reservedChoice = CLAIMS.powerset().oneOf()
    nondet deferredChoice = deferChoices.oneOf()
    tickFullWith(
      pgLoadFails, ackFails, createFails, demandA, demandB, reservedChoice,
      deferredChoice)
  }

  // ------------------------------------------------------------------
  // 7. Environment actions
  // ------------------------------------------------------------------
  action envFrame = all {
    isLeader' = isLeader,
    edgeClearDone' = edgeClearDone,
    pendingClear' = pendingClear,
    schedBinding' = schedBinding,
    deferredCells' = deferredCells,
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
    edgeClearDone' = edgeClearDone, pendingClear' = pendingClear,
    schedBinding' = schedBinding,
    deferredCells' = deferredCells,
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
    edgeClearDone' = edgeClearDone, pendingClear' = pendingClear,
    schedBinding' = schedBinding,
    deferredCells' = deferredCells,
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
    // C2/346: a NEW acquisition arms the edge token (the on_acquire
    // fetch_add); the first leader tick consumes it.
    edgeClearDone' = if (ENABLE_EPOCH_MACHINERY) false else edgeClearDone,
    // C2/007: suppress polarity — pre-tenure buffered evidence must
    // not ship a stale clear under the new tenure (mirrors the code's
    // edge reset; the Rust pin is pending_evidence_cleared_on_acquire_edge).
    pendingClear' =
      if (ENABLE_CLEARBUF_MACHINERY) Set() else pendingClear,
    schedBinding' = schedBinding,
    deferredCells' = deferredCells,
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
    // A restart loses the in-memory buffer (crash loss is outside the
    // discard latch — recorded residual) and has no pending edge until
    // the next acquisition; the scheduler's map is its own state.
    edgeClearDone' = if (ENABLE_EPOCH_MACHINERY) true else edgeClearDone,
    pendingClear' = if (ENABLE_CLEARBUF_MACHINERY) Set() else pendingClear,
    schedBinding' = schedBinding,
    deferredCells' = deferredCells,
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
    edgeClearDone' = edgeClearDone, pendingClear' = pendingClear,
    schedBinding' = schedBinding,
    deferredCells' = deferredCells,
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
    edgeClearDone' = edgeClearDone, pendingClear' = pendingClear,
    schedBinding' = schedBinding,
    deferredCells' = deferredCells,
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
    edgeClearDone' = edgeClearDone, pendingClear' = pendingClear,
    schedBinding' = schedBinding,
    deferredCells' = deferredCells,
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
    edgeClearDone' = true,
    pendingClear' = Set(),
    schedBinding' = Set(),
    deferredCells' = Set(),
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
    ENABLE_DEFER_DEMAND = false
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
    ENABLE_DEFER_DEMAND = false
  ).*

  /// The conflation trace, post-fix: the claim is observed idle, goes
  /// busy and idle again across ⊥ ticks — the busy edge IS observed on
  /// the ⊥ tick (prune + re-seed), so the threshold-crossing full tick
  /// never over-reaps the fresh spell. The documented trace HOLDS
  /// end-to-end (it falsified VIdleOverReap on the pre-fix encoding).
  run idlePruneAcrossBotRun =
    init
      .then(ceilingsLoad)
      .then(tickFullWith(false, false, false, 1, 0, Set(), Set()))
      .then(claimRegisters("c1"))
      .then(tickFullWith(false, false, false, 0, 0, Set(), Set()))
      .then(claimGoesBusy("c1"))
      .then(tickBotEarlyWith(false))
      .then(claimGoesIdle("c1"))
      .then(tickFullWith(false, false, false, 0, 0, Set(), Set()))
      .expect(not(violations.contains(VIdleOverReap)))
      .expect(claims.get("c1").pIdle != INotTracked)

  /// The boot half of the same trace, post-fix: a Registered edge
  /// inside the pre-threshold window is recorded ON the ⊥ tick (clear
  /// discarded), so the recency gate never expires it and
  /// BootRecordedOnce's no-loss clause HOLDS.
  run bootRecordedAcrossBotRun =
    init
      .then(ceilingsLoad)
      .then(tickFullWith(false, false, false, 1, 0, Set(), Set()))
      .then(claimRegisters("c1"))
      .then(tickBotEarlyWith(false))
      .then(tickFullWith(false, false, false, 0, 0, Set(), Set()))
      .expect(not(violations.contains(VBootSampleLost)))
      .expect(claims.get("c1").boot == BRecorded)
}

/// Lease faults: lose/acquire edges, PG reload failure, controller
/// restart — the lease-edge polarity table's regime.
module nodeclaimLifecycleFaultLease {
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
    ENABLE_SNAPSHOT_PRESENCE = false,
    ENABLE_DEFER_MACHINERY = false,
    ENABLE_DEFER_DEMAND = false
  ).*
}

/// Karpenter faults: launch failures, the 1s-GC-vs-10s-tick vanish race
/// and spot termination edges — the inflight-conservation / ICE-mark
/// regime.
module nodeclaimLifecycleFaultKarpenter {
  import nodeclaimLifecycle(
    MAX_FLEET_CORES = 2,
    CLASS_CAP_A = 1,
    BOT_THRESHOLD = 2,
    ENABLE_RPC_FAULT = false,
    ENABLE_LEASE_FAULT = false,
    ENABLE_KARPENTER_FAULT = true,
    START_CEILINGS_KNOWN = true,
    ENABLE_EPOCH_MACHINERY = false,
    ENABLE_ACQUIRE_EPOCH = false,
    ENABLE_CLEARBUF_MACHINERY = false,
    ENABLE_EVIDENCE_BUFFER = false,
    ENABLE_ACK_LATCH = false,
    ENABLE_SNAPSHOT_MACHINERY = false,
    ENABLE_SNAPSHOT_PRESENCE = false,
    ENABLE_DEFER_MACHINERY = false,
    ENABLE_DEFER_DEMAND = false
  ).*
}

/// C2/346 FALSIFY half: the as-built boolean reload latch re-runs the
/// amplify-class prev_idle clear on EVERY Err tick of a PG outage —
/// idleSpellSurvivesReloadErr is expected to be violated (an idle
/// spell seeded under the current tenure is re-cleared).
module nodeclaimLifecycleEpochAsBuilt {
  import nodeclaimLifecycle(
    MAX_FLEET_CORES = 2,
    CLASS_CAP_A = 1,
    BOT_THRESHOLD = 2,
    ENABLE_RPC_FAULT = false,
    ENABLE_LEASE_FAULT = true,
    ENABLE_KARPENTER_FAULT = false,
    START_CEILINGS_KNOWN = true,
    ENABLE_EPOCH_MACHINERY = true,
    ENABLE_ACQUIRE_EPOCH = false,
    ENABLE_CLEARBUF_MACHINERY = false,
    ENABLE_EVIDENCE_BUFFER = false,
    ENABLE_ACK_LATCH = false,
    ENABLE_SNAPSHOT_MACHINERY = false,
    ENABLE_SNAPSHOT_PRESENCE = false,
    ENABLE_DEFER_MACHINERY = false,
    ENABLE_DEFER_DEMAND = false
  ).*
}

/// C2/346 HOLD half: the acquisition-epoch token — edge actions fire
/// exactly once per acquisition; a re-acquire mid-Err-loop is a new
/// epoch and re-fires once. canReachEdgeIdleClear keeps it
/// non-vacuous (the edge clear acts on stale pre-tenure spells).
module nodeclaimLifecycleEpoch {
  import nodeclaimLifecycle(
    MAX_FLEET_CORES = 2,
    CLASS_CAP_A = 1,
    BOT_THRESHOLD = 2,
    ENABLE_RPC_FAULT = false,
    ENABLE_LEASE_FAULT = true,
    ENABLE_KARPENTER_FAULT = false,
    START_CEILINGS_KNOWN = true,
    ENABLE_EPOCH_MACHINERY = true,
    ENABLE_ACQUIRE_EPOCH = true,
    ENABLE_CLEARBUF_MACHINERY = false,
    ENABLE_EVIDENCE_BUFFER = false,
    ENABLE_ACK_LATCH = false,
    ENABLE_SNAPSHOT_MACHINERY = false,
    ENABLE_SNAPSHOT_PRESENCE = false,
    ENABLE_DEFER_MACHINERY = false,
    ENABLE_DEFER_DEMAND = false
  ).*
}

/// C2/007 FALSIFY half: the as-built ⊥/consolidate ticks DISCARD the
/// scheduler-bound ICE-clear of a consumed (consume-once) Registered
/// edge — iceClearDelivered is expected to be violated.
module nodeclaimLifecycleClearBufferAsBuilt {
  import nodeclaimLifecycle(
    MAX_FLEET_CORES = 2,
    CLASS_CAP_A = 1,
    BOT_THRESHOLD = 2,
    ENABLE_RPC_FAULT = true,
    ENABLE_LEASE_FAULT = false,
    ENABLE_KARPENTER_FAULT = false,
    START_CEILINGS_KNOWN = true,
    ENABLE_EPOCH_MACHINERY = false,
    ENABLE_ACQUIRE_EPOCH = false,
    ENABLE_CLEARBUF_MACHINERY = true,
    ENABLE_EVIDENCE_BUFFER = false,
    ENABLE_ACK_LATCH = false,
    ENABLE_SNAPSHOT_MACHINERY = false,
    ENABLE_SNAPSHOT_PRESENCE = false,
    ENABLE_DEFER_MACHINERY = false,
    ENABLE_DEFER_DEMAND = false
  ).*
}

/// C2/007 HOLD half: the must-use evidence buffer — clears produced on
/// non-delivering ticks are buffered and drained into the next full
/// tick's ack. canReachBufferedClearDelivered keeps it non-vacuous.
module nodeclaimLifecycleClearBuffer {
  import nodeclaimLifecycle(
    MAX_FLEET_CORES = 2,
    CLASS_CAP_A = 1,
    BOT_THRESHOLD = 2,
    ENABLE_RPC_FAULT = true,
    ENABLE_LEASE_FAULT = false,
    ENABLE_KARPENTER_FAULT = false,
    START_CEILINGS_KNOWN = true,
    ENABLE_EPOCH_MACHINERY = false,
    ENABLE_ACQUIRE_EPOCH = false,
    ENABLE_CLEARBUF_MACHINERY = true,
    ENABLE_EVIDENCE_BUFFER = true,
    ENABLE_ACK_LATCH = true,
    ENABLE_SNAPSHOT_MACHINERY = false,
    ENABLE_SNAPSHOT_PRESENCE = false,
    ENABLE_DEFER_MACHINERY = false,
    ENABLE_DEFER_DEMAND = false
  ).*
}

/// merged_bug_045 FALSIFY half (bughunt-2 slot 5): the as-built
/// mem::take-before-RPC — the buffer drains into the delivering tick's
/// ack BEFORE the RPC outcome is known, so an ack failure on exactly
/// that tick loses the batch (clearSurvivesAckFailure is expected to
/// be violated). Same shape as the ClearBuffer hold regime, latch off.
module nodeclaimLifecycleAckLatchAsBuilt {
  import nodeclaimLifecycle(
    MAX_FLEET_CORES = 2,
    CLASS_CAP_A = 1,
    BOT_THRESHOLD = 2,
    ENABLE_RPC_FAULT = true,
    ENABLE_LEASE_FAULT = false,
    ENABLE_KARPENTER_FAULT = false,
    START_CEILINGS_KNOWN = true,
    ENABLE_EPOCH_MACHINERY = false,
    ENABLE_ACQUIRE_EPOCH = false,
    ENABLE_CLEARBUF_MACHINERY = true,
    ENABLE_EVIDENCE_BUFFER = true,
    ENABLE_ACK_LATCH = false,
    ENABLE_SNAPSHOT_MACHINERY = false,
    ENABLE_SNAPSHOT_PRESENCE = false,
    ENABLE_DEFER_MACHINERY = false,
    ENABLE_DEFER_DEMAND = false
  ).*
}

/// C2/285 FALSIFY half: the as-built ack suppresses the all-empty tick
/// and an empty bound set means "no snapshot" — after scale-to-zero
/// the scheduler keeps a stale binding map (ackCarriesSnapshot is
/// expected to be violated).
module nodeclaimLifecycleSnapshotAsBuilt {
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
    ENABLE_SNAPSHOT_MACHINERY = true,
    ENABLE_SNAPSHOT_PRESENCE = false,
    ENABLE_DEFER_MACHINERY = false,
    ENABLE_DEFER_DEMAND = false
  ).*
}

/// C2/285 HOLD half: presence-preserving snapshots — every full-tick
/// ack carries the bound set and present-and-empty CLEARS.
/// canReachBindingCleared keeps it non-vacuous (a scale-to-zero tick
/// actually clears a previously non-empty map).
module nodeclaimLifecycleSnapshot {
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
    ENABLE_SNAPSHOT_MACHINERY = true,
    ENABLE_SNAPSHOT_PRESENCE = true,
    ENABLE_DEFER_MACHINERY = false,
    ENABLE_DEFER_DEMAND = false
  ).*
}

/// Round-10/merged_bug_012 FALSIFY half: the as-built absence fold —
/// the consolidator reads window-remaindered cells as zero demand, so
/// a reap-eligible idle claim in a deferred-demand cell is reaped at
/// the floor (deferredDemandKept is expected to be violated; the next
/// rotation admits the deferred work onto a freshly-minted claim —
/// the reap-then-re-mint churn).
module nodeclaimLifecycleDeferAsBuilt {
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
    ENABLE_DEFER_MACHINERY = true,
    ENABLE_DEFER_DEMAND = false
  ).*
}

/// Round-10/merged_bug_012 HOLD half: window-deferred demand counts
/// for the NA keep-condition — an idle claim in a deferred-demand
/// cell is kept this tick (e_fitting_cores includes the deferred
/// population, raising the threshold). canReachDeferKept keeps it
/// non-vacuous.
module nodeclaimLifecycleDefer {
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
    ENABLE_DEFER_MACHINERY = true,
    ENABLE_DEFER_DEMAND = true
  ).*
}
