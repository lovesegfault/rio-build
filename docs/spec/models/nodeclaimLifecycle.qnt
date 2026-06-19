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
//     MODEL-DIVERGENCE(law=ctrl.nodeclaim.mint-deficit-proportional+3; tree=rio-controller/src/reconcilers/nodeclaim_pool/cover.rs:sizing): the model certifies the three-term STRUCTURE (n = min(deficit, budget, inflightHeadroom) in claim units) — NOT the sizing arithmetic (n_pack FFD packing, the budget/chunk(n) integer granularity, the upward family search to the first affordable n, the truly-unaffordable warn+counter arm) — re-derived at the round-13 family-quantified affordability close (bug_062): in claim units the structure is INVARIANT under the family search (it only changes which chunk denominates a claim), so the certification stands; the family face is code-tier-verified (W13-AI) — retarget-by: encoding sizing arithmetic (a Stage-D item) or any change to the sizing law itself.
//     MODEL-DIVERGENCE(law=karpenter.cluster-synced; tree=kubernetes-sigs/karpenter@v1.11.1:pkg/controllers/state/cluster.go::Synced): Karpenter's cross-pool provisioner gate (Synced()=false while any NodeClaim lacks providerID) is environment-owned and out of model — certified instead by the controller-side inflight-headroom structural bound (sh-043; aws/karpenter-provider-aws#7428) — retarget-by: modeling rio-general as a second consumer pool.
//   - The per-tick mark dedup (the BTreeSet in report_unfulfillable) is
//     by construction here (marks are a set); the soundness checks are
//     about WHICH cells are marked, not how many times.
//
// DIVERGENCE-HEADER GRAMMAR (minted round-9 WO-S7-2 — the R23 model
//   tier, machine-visible; the census-lint tier-2 drift grep consumes
//   the literal `MODEL-DIVERGENCE(` token): a law the model
//   deliberately does NOT certify carries, AT the model (and mirrored
//   at its archived index row), one single-line header of the
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
//   provisioningBudget    the budget-brake mint law (live_049 L1 +
//                         sh-043): global and per-class budgets hold
//                         with terminating claims still counted;
//                         failed creates consume no budget; and a
//                         deficit within budget mints FULLY in one
//                         tick OR the inflight-unlaunched count is at
//                         cap (deficitFullyMinted — the
//                         per-cell-per-tick cap stays retired from
//                         relation AND invariant)
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
//   unconditionally (flip executed 2026-06-02; the fix commit is in
//   the introducing commit history).
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
    // sh-030: this controller OBSERVED the claim Registered on some
    // earlier tick — latched true by claimRegisters and never cleared
    // (carried through the tomb/absent state like inflightTracked).
    // The strengthened iceMarkSoundness law: a vanish ICE mark for a
    // claim with everRegistered=true is spurious (Karpenter empty-node
    // consolidation, not capacity failure).
    everRegistered: bool,
    pIdle: IdleTrack,
    ctrlReaped: bool,
    // Round-11 (WO-S4-1/-2): the controller's delete-provenance
    // tombstone for this claim is armed (an ambiguous non-404 delete
    // error whose commit is unconfirmed).
    tomb: bool,
    // The tombstone's age — denominated in FOLD EXECUTIONS under the
    // fixed law (ENABLE_FOLD_CLOCK) and in leader wall ticks under
    // the as-built law.
    tombAge: int,
    // GROUND TRUTH (the environment's, not the controller's): an
    // ambiguous delete of this claim COMMITTED server-side — its
    // later teardown is this controller's reap, whatever the
    // controller still knows.
    ourCommit: bool,
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
    | EInflightCapped
    | EDeferKept
    | EReapAmbiguous
    | ETombstoneConfirmed

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
    | VTombstoneDropped

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
  /// Round-11 area (bug_042/bug_043/bug_112, WO-S4-1/-2): MACHINERY
  /// moves the delete-outcome tombstone plane -- the ambiguous-delete
  /// nondet at BOTH reap edges (stuck + idle), the per-claim
  /// tombstone with its age, the ground-truth commit bit, and the
  /// consult (vanish-fold SelfReap + registered-tombstone sweep) at
  /// every fold-running tick (full + consolidate-only). FOLD_CLOCK
  /// selects the FIXED law (the tombstone TTL is denominated in
  /// CONSUMER FOLD EXECUTIONS and the prune runs AFTER the consult --
  /// one chokepoint, consult-then-prune by construction) vs the
  /// as-built wall-tick law (the TTL aged on every leader tick
  /// including foldless pre-threshold ⊥ ticks, and prune_expired ran
  /// BEFORE detect_vanished at both call sites -- a >=TTL foldless
  /// window dropped the tombstone before its first consult, and the
  /// next fold minted foreign-attributed vanish evidence for the
  /// controller's own committed delete: the false ICE mask +
  /// reaped_total{reason=vanished}). Same-tick stamps are not aged by
  /// their own tick's fold (the conservative direction -- grace can
  /// only lengthen). All pre-round-11 regimes bind both false -- the
  /// ambiguous-choice sets collapse to empty and the reachable state
  /// space is unchanged.
  ///
  /// MODEL-DIVERGENCE(law=ctrl.pool.delete-outcome; tree=rio-controller/src/reconcilers/nodeclaim_pool/health.rs:record_reap): the model certifies the outcome PARTITION (completed vs ambiguous; its single completed arm IS the Ok/404 parity law), the tombstone consume-or-disclosed-expiry protocol, and (round-12, merged_bug_050) the TYPED per-exit disposition — the registered-handoff exits hand the tombstone to the sweep (ENABLE_TYPED_TOMBSTONE_DISPOSITION; the as-built consume face is the disposition twins' red) — NOT the consequence payload (counters, the wedge-eviction feed, censored NA samples; the wedge plane is consumed-input, out of model) — retarget-by: modeling the wedge admission plane or any change to the consequence-packet law.
  const ENABLE_TOMBSTONE_MACHINERY: bool
  const ENABLE_FOLD_CLOCK: bool
  /// merged_bug_050 (round-12): the ambiguous-delete commit LAGS the
  /// LIST — commitChoice sets the ground truth (ourCommit) without
  /// moving the observed phase; the commit surfaces later via the
  /// ambiguousCommitSurfaces env action (a committed delete shows
  /// terminating only on a post-commit LIST). Off = the round-11
  /// regimes' immediate-surfacing shape (state spaces unchanged).
  const ENABLE_LAGGED_COMMIT_OBSERVATION: bool
  /// merged_bug_050 (round-12): tombstone disposition is TYPED per
  /// vanish exit — only the packet-firing exit (SelfReap/confirm)
  /// consumes; the registered-handoff exits HAND the tombstone to
  /// the registered-population sweep. Off = the as-built uniform
  /// consume-on-every-exit epilogue (the defect face).
  const ENABLE_TYPED_TOMBSTONE_DISPOSITION: bool
  /// sh-030 (round-13): the EmptyConsolidation gate — vanishMarks
  /// excludes a tracked claim the controller observed Registered
  /// (Karpenter empty-node consolidation across a controller-blind
  /// window is cleanup, not capacity failure). Off = the as-built
  /// unconditional GcVanish row (the defect face: a 295 s tick let
  /// 69 claims Register, sit empty past consolidateAfter, vanish, and
  /// ICE-mask the cell). The spurious LAW widens UNCONDITIONALLY (a
  /// mark for an everRegistered claim is spurious whatever the gate);
  /// this flag gates the CONTROLLER ACTION (vanishIce filter), not
  /// the predicate — the ENABLE_TYPED_TOMBSTONE_DISPOSITION precedent.
  const ENABLE_EMPTY_CONSOLIDATION_GATE: bool

  pure val CLAIMS: Set[ClaimId] = Set("c1", "c2")
  pure val CELLS: Set[Cell] = Set("cellA", "cellB")
  // sh-043: the GLOBAL concurrent-unlaunched ceiling. `pure val` (NOT
  // `const`): `const X: int = 2` is a Quint parse error (QNT000) and
  // a bare `const` would force binding at all 27
  // `import nodeclaimLifecycle(...)` sites; the slot universe
  // |CLAIMS|=2 is itself a `pure val` so no regime can usefully set
  // it differently. MUST be ≥ 2 — `canReachBurstFullMint`
  // (`createdB>1` in Base; CLASS_CAP_A=1 caps cellA) is unreachable
  // at 1.
  pure val MAX_INFLIGHT_UNLAUNCHED: int = 2

  pure def boolChoices(faultOn: bool): Set[bool] =
    if (faultOn) Set(false, true) else Set(false)

  /// Round-10/merged_bug_012: the per-full-tick deferred-cell choices.
  /// Off-machinery the only choice is the empty set, so pre-round-10
  /// regimes branch exactly as before (state space unchanged).
  pure val deferChoices: Set[Set[Cell]] =
    if (ENABLE_DEFER_MACHINERY) CELLS.powerset() else Set(Set())

  /// TOMBSTONE_TTL_FOLDS (3 in the code; 2 here -- one stale-LIST
  /// consult plus one disconfirmed consult before the disclosed
  /// expiry, scaled to the model's smaller orbit exactly like
  /// BOT_THRESHOLD).
  pure val TOMBSTONE_TTL: int = 2

  /// Round-11: the per-tick ambiguous-outcome choice set — empty or a
  /// SINGLETON (one ambiguous delete per tick; simultaneous ambiguity
  /// on both claims adds branching without adding law surface — the
  /// per-claim tombstone plane is independent). Off-machinery the
  /// only choice is the empty set (state space unchanged for every
  /// pre-round-11 regime).
  pure val tombChoices: Set[Set[ClaimId]] =
    if (ENABLE_TOMBSTONE_MACHINERY)
      Set(Set()).union(CLAIMS.map(k => Set(k)))
    else Set(Set())

  pure def claimRank(k: ClaimId): int = if (k == "c1") 1 else 2

  pure val absentClaim: ClaimRec = {
    phase: CAbsent, cell: "cellA", stuck: false, spell: SNotIdle,
    busyTickSeen: false, boot: BRecorded, inflightTracked: false,
    inflightCell: "cellA", everRegistered: false,
    pIdle: INotTracked, ctrlReaped: false,
    tomb: false, tombAge: 0, ourCommit: false,
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
  /// Round-11 (bug_042 + bug_043, ctrl.pool.delete-outcome /
  /// ctrl.pool.fold-clock): a COMMITTED ambiguous delete's tombstone
  /// is never dropped with its consequence unapplied -- every
  /// tombstone exit is a consult-consumption (the late consequence,
  /// ETombstoneConfirmed) or a disclosed expiry of a DISCONFIRMED
  /// attempt. Falsified as-built (wall-tick TTL aging through the
  /// foldless ⊥ window + prune-before-consult); holds with
  /// ENABLE_FOLD_CLOCK.
  val tombstoneConsequenceApplied = not(violations.contains(VTombstoneDropped))
  val bootSampleNotLost = not(violations.contains(VBootSampleLost))
  val noMassClearAfterFailover = not(violations.contains(VStaleClear))
  val reloadLatchRespected = not(violations.contains(VStaleOverwrite))
  val singleEffectiveProvisioner = not(violations.contains(VStandbyEffect))
  val gateProducerGuarantee = not(violations.contains(VGateProducer))
  /// The deficit-full-mint leaf, NAMED single-proposition (P7): a
  /// deficit within budget (and the slot universe) mints fully in one
  /// tick OR the inflight-unlaunched count is at cap (sh-043) — the
  /// landed live_049 L1 + sh-043 law, satisfiable and asserted
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
  /// Round-11 non-vacuity pair: the ambiguous arm actually taken, and
  /// the late-confirm consequence actually applied, in the hold
  /// regime (wired as expected-violation checks).
  val canReachReapAmbiguous = not(effects.contains(EReapAmbiguous))
  val canReachTombstoneConfirmed = not(effects.contains(ETombstoneConfirmed))
  val canReachClearAfterMark = not(effects.contains(EClearOfMaskedCell))
  val canReachConsolidateOnlyReap = not(effects.contains(EConsolidateOnlyReap))
  val canReachClassBudgetBound = not(effects.contains(EClassBudgetBound))
  val canReachCreateFailure = not(effects.contains(ECreateFailed))
  val canReachCeilingsFailClosed = not(effects.contains(EFailClosedNoCeilings))
  val canReachUnknownCellDrop = not(effects.contains(EUnknownCellDrop))
  val canReachCreate = not(effects.contains(ECreateOk))
  val canReachBurstFullMint = not(effects.contains(EBurstFullMint))
  /// sh-043 non-vacuity (shape-parity witness): the unlaunched count
  /// reached `MAX_INFLIGHT_UNLAUNCHED` with surplus demand. At
  /// |CLAIMS|=MAX=2 this coincides with the slot ceiling — it
  /// certifies the cap-hitting state is REACHABLE, NOT that headroom
  /// bound distinctly from slots (a falsifiable headroom check needs
  /// `MAX < |CLAIMS|`, deferred). The `not(...)` form is the project
  /// convention: "violated" means the effect IS reached;
  /// `effects.contains(...)` directly would trivially "violate" at
  /// init where `effects=Set()`.
  val canHitInflightCap = not(effects.contains(EInflightCapped))
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
      // Round-11 (bug_043): this FOLDLESS leader tick is exactly the
      // window the fold clock exists for — the as-built wall-tick TTL
      // ages here (tick_counter incremented, no consult ran); the
      // fixed law's fold clock does not move.
      val afterTomb = CLAIMS.mapBy(k =>
        val c = afterObs.get(k)
        if (ENABLE_TOMBSTONE_MACHINERY and not(ENABLE_FOLD_CLOCK) and c.tomb)
          { ...c, tombAge: c.tombAge + 1 }
        else c)
      all {
        claims' = afterTomb,
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
  action tickConsolidateOnlyWith(
    pgLoadFails: bool, ambChoice: Set[ClaimId], commitChoice: Set[ClaimId]
  ): bool = all {
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
      // ---- round-11 delete-outcome tombstone fold ----------------------
      // As-built: prune_expired runs BEFORE detect_vanished on the
      // WALL age (this tick increments it too); fixed
      // (ENABLE_FOLD_CLOCK): the consult runs first and ages are
      // denominated in fold executions — this tick is one.
      val preAged = CLAIMS.mapBy(k =>
        val c = afterObs.get(k)
        if (ENABLE_TOMBSTONE_MACHINERY and not(ENABLE_FOLD_CLOCK) and c.tomb)
          { ...c, tombAge: c.tombAge + 1 }
        else c)
      val prePruneDropped = CLAIMS.filter(k =>
        val c = preAged.get(k)
        ENABLE_TOMBSTONE_MACHINERY and not(ENABLE_FOLD_CLOCK)
        and c.tomb and c.tombAge > TOMBSTONE_TTL)
      val tombs = CLAIMS.mapBy(k =>
        val c = preAged.get(k)
        if (prePruneDropped.contains(k)) { ...c, tomb: false, tombAge: 0 }
        else c)
      // The consult: a tombstoned claim observed terminating or absent
      // is OUR delete confirmed (the vanish fold's SelfReap arm for
      // tracked claims; the registered-tombstone sweep for the rest) —
      // consume + the original consequence, never foreign-attributed
      // evidence.
      val tombConfirmed = CLAIMS.filter(k =>
        val c = tombs.get(k)
        c.tomb and (c.phase == CAbsent or c.phase == CTerminating))
      // Unhealthy reap (the stuck in-flight arm) — ICE cells dropped
      // (no scheduler to report to), names still pruned from inflight.
      val stuckReaped = CLAIMS.filter(k =>
        tombs.get(k).phase == CInFlight and tombs.get(k).stuck)
      // The r40 bug_012 prune: detect_vanished runs here too, with its
      // marks discarded.
      val vanished = CLAIMS.filter(k =>
        tombs.get(k).inflightTracked and not(stuckReaped.contains(k))
        and tombs.get(k).phase == CAbsent and not(tombConfirmed.contains(k)))
      // sh-030: the CONTROLLER ACTION gate — a vanish whose claim the
      // controller observed Registered is empty consolidation, not
      // capacity failure; the gated vanishIce is what would mark.
      val vanishIce = vanished.filter(k =>
        not(ENABLE_EMPTY_CONSOLIDATION_GATE and tombs.get(k).everRegistered))
      // bug_043 widening: a mark for a claim whose teardown is OURS by
      // ground truth (ctrlReaped — completed; ourCommit — committed
      // ambiguous) is spurious whatever the controller still knows.
      // sh-030 widening (UNCONDITIONAL — the strengthened soundness
      // law): so is a mark for a claim with everRegistered. Ranges
      // over vanishIce — the population that would mark.
      val spurious = vanishIce.filter(k =>
        tombs.get(k).ctrlReaped or tombs.get(k).ourCommit
        or tombs.get(k).everRegistered).size() > 0
      // Idle reap with an empty reserved set (no FFD this tick).
      val idleReaped = CLAIMS.filter(k =>
        val c = tombs.get(k)
        c.phase == CRegistered and c.spell != SNotIdle and c.pIdle == ITrackedOld)
      val overReap = idleReaped.filter(k => tombs.get(k).spell != SOld).size() > 0
      // Round-11: the delete-outcome split per reaped claim — the
      // completed arm (Ok and 404, one arm: the parity law) vs the
      // ambiguous arm (tombstone, no consequence yet).
      val ambReaped = stuckReaped.union(idleReaped).intersect(ambChoice)
      val completedReaped = stuckReaped.union(idleReaped).exclude(ambReaped)
      val claimsAfter = CLAIMS.mapBy(k =>
        val c = tombs.get(k)
        if (completedReaped.contains(k))
          // ourCommit machinery-gated: with the plane off the field
          // is constant false and the reachable state space is
          // byte-identical to the pre-round-11 regimes.
          { ...c, phase: CTerminating, stuck: false, spell: SNotIdle,
            busyTickSeen: false, boot: BRecorded, pIdle: INotTracked,
            inflightTracked: false, ctrlReaped: true,
            tomb: false, tombAge: 0,
            ourCommit: ENABLE_TOMBSTONE_MACHINERY }
        else if (ambReaped.contains(k))
          // The AmbiguousErr arm: NO consequence yet; the tombstone is
          // (re-)stamped; whether the delete committed is the
          // environment's truth — a committed delete surfaces as
          // terminating at the next LIST.
          { ...c,
            phase: if (commitChoice.contains(k) and not(ENABLE_LAGGED_COMMIT_OBSERVATION))
                     CTerminating else c.phase,
            stuck: if (commitChoice.contains(k) and not(ENABLE_LAGGED_COMMIT_OBSERVATION))
                     false else c.stuck,
            tomb: true, tombAge: 0,
            ourCommit: c.ourCommit or commitChoice.contains(k) }
        else if (tombConfirmed.contains(k))
          // The late consequence applied; provenance consumed; the
          // tracking story is over (the fold exit / the sweep).
          { ...c, tomb: false, tombAge: 0, inflightTracked: false,
            ctrlReaped: true }
        else if (vanished.contains(k)) { ...c, inflightTracked: false }
        else if (c.phase == CRegistered and c.spell == SNotIdle and c.inflightTracked)
          // detect_vanished drops registered/terminating entries too.
          // merged_bug_050: typed disposition — the handoff exits
          // keep the tombstone for the sweep; as-built consumed it.
          { ...c, inflightTracked: false,
            tomb: if (ENABLE_TYPED_TOMBSTONE_DISPOSITION) c.tomb else false,
            tombAge: if (ENABLE_TYPED_TOMBSTONE_DISPOSITION) c.tombAge else 0 }
        else if (c.phase == CRegistered and c.inflightTracked)
          { ...c, inflightTracked: false,
            tomb: if (ENABLE_TYPED_TOMBSTONE_DISPOSITION) c.tomb else false,
            tombAge: if (ENABLE_TYPED_TOMBSTONE_DISPOSITION) c.tombAge else 0 }
        else if (c.phase == CTerminating and c.inflightTracked)
          { ...c, inflightTracked: false,
            tomb: if (ENABLE_TYPED_TOMBSTONE_DISPOSITION) c.tomb else false,
            tombAge: if (ENABLE_TYPED_TOMBSTONE_DISPOSITION) c.tombAge else 0 }
        else c)
      // Fixed law: the post-consult fold-clock advance + prune — ages
      // move only on fold executions, AFTER the consult; a same-tick
      // stamp is not aged by its own fold. Expiry can only reach
      // DISCONFIRMED entries (committed ones confirmed above).
      val foldExpired = CLAIMS.filter(k =>
        val c = claimsAfter.get(k)
        ENABLE_TOMBSTONE_MACHINERY and ENABLE_FOLD_CLOCK and c.tomb
        and not(ambReaped.contains(k)) and c.tombAge + 1 > TOMBSTONE_TTL)
      val claimsAfterTomb = CLAIMS.mapBy(k =>
        val c = claimsAfter.get(k)
        if (foldExpired.contains(k)) { ...c, tomb: false, tombAge: 0 }
        else if (ENABLE_TOMBSTONE_MACHINERY and ENABLE_FOLD_CLOCK and c.tomb
                 and not(ambReaped.contains(k)))
          { ...c, tombAge: c.tombAge + 1 }
        else c)
      // The consumer law's violation: a tombstone dropped while its
      // delete had COMMITTED — the consequence can no longer fire
      // (bug_042's unconsumed expiry / bug_043's pruned-unconsulted).
      // merged_bug_050 (round-12): the as-built uniform epilogue
      // consumes the tombstone at the registered-handoff exit on
      // disconfirmed-only evidence — a committed delete's packet is
      // lost at that consume (the third drop path beside the two
      // prune paths below).
      val handoffDropped = CLAIMS.filter(k =>
        val c = tombs.get(k)
        ENABLE_TOMBSTONE_MACHINERY and not(ENABLE_TYPED_TOMBSTONE_DISPOSITION)
        and c.inflightTracked and c.tomb and c.ourCommit
        and c.phase == CRegistered)
      val vTombDropped = ENABLE_TOMBSTONE_MACHINERY and (
        prePruneDropped.filter(k => preAged.get(k).ourCommit).size() > 0
        or foldExpired.filter(k => claimsAfter.get(k).ourCommit).size() > 0
        or handoffDropped.size() > 0)
      all {
        claims' = claimsAfterTomb,
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
          .union(if (ambReaped.size() > 0) Set(EReapAmbiguous) else Set())
          .union(if (tombConfirmed.size() > 0) Set(ETombstoneConfirmed) else Set())
          .union(if (staleRecordOnly) Set(EStaleRecordOnly) else Set())
          .union(if (reloadStillPending) Set(EDegradedReloadTick) else Set())
          .union(if (edgeClearActed) Set(EEdgeIdleClear) else Set())
          .union(if (reloadPending and CLAIMS.filter(k => claims.get(k).inflightTracked).size() > 0)
                   Set(EAcquireWithInflight) else Set()),
        violations' = violations
          .union(if (overReap) Set(VIdleOverReap) else Set())
          .union(if (spurious) Set(VSpuriousMark) else Set())
          .union(if (vTombDropped) Set(VTombstoneDropped) else Set())
          .union(if (lostSamples) Set(VBootSampleLost) else Set())
          .union(if (vIdleLost) Set(VIdleSpellLost) else Set())
          .union(if (vClearDropped) Set(VClearEvidenceDropped) else Set()),
      }
  }

  action tickConsolidateOnly = {
    nondet pgLoadFails = boolChoices(ENABLE_LEASE_FAULT).oneOf()
    nondet ambChoice = tombChoices.oneOf()
    // Commit is all-or-none of the chosen attempt (the commit bit is
    // per-ATTEMPT; with singleton ambiguity this is exactly one bit).
    nondet commitAll = boolChoices(ENABLE_TOMBSTONE_MACHINERY).oneOf()
    tickConsolidateOnlyWith(
      pgLoadFails, ambChoice, if (commitAll) ambChoice else Set())
  }

  /// The full reconcile_once tick (scheduler reachable). Parameterized
  /// over the per-tick faults and the opaque scheduler/FFD inputs so
  /// deterministic runs can pin them; `tickFull` below is the
  /// nondeterministic wrapper used by `step`.
  action tickFullWith(
    pgLoadFails: bool, ackFails: bool, createFails: bool,
    demandA: int, demandB: int, reservedChoice: Set[ClaimId],
    deferredChoice: Set[Cell],
    ambChoice: Set[ClaimId], commitChoice: Set[ClaimId]
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
      // ---- round-11 delete-outcome tombstone fold ----------------------
      // As-built: prune_expired runs BEFORE detect_vanished on the
      // WALL age (this tick increments it too); fixed
      // (ENABLE_FOLD_CLOCK): the consult runs first and ages are
      // denominated in fold executions — this tick is one.
      val preAged = CLAIMS.mapBy(k =>
        val c = afterObs.get(k)
        if (ENABLE_TOMBSTONE_MACHINERY and not(ENABLE_FOLD_CLOCK) and c.tomb)
          { ...c, tombAge: c.tombAge + 1 }
        else c)
      val prePruneDropped = CLAIMS.filter(k =>
        val c = preAged.get(k)
        ENABLE_TOMBSTONE_MACHINERY and not(ENABLE_FOLD_CLOCK)
        and c.tomb and c.tombAge > TOMBSTONE_TTL)
      val tombs = CLAIMS.mapBy(k =>
        val c = preAged.get(k)
        if (prePruneDropped.contains(k)) { ...c, tomb: false, tombAge: 0 }
        else c)
      // The consult: tombstoned ∧ (terminating ∨ absent) = OUR delete
      // confirmed — consume + the original consequence, never
      // foreign-attributed evidence (SelfReap / the registered sweep).
      val tombConfirmed = CLAIMS.filter(k =>
        val c = tombs.get(k)
        c.tomb and (c.phase == CAbsent or c.phase == CTerminating))
      // ---- publish the placeable set (always on a successful tick) ----
      // ---- unhealthy reap (stuck in-flight) + vanish detection --------
      val stuckReaped = CLAIMS.filter(k =>
        tombs.get(k).phase == CInFlight and tombs.get(k).stuck)
      val stuckMarks: Set[Cell] = stuckReaped.exclude(ambChoice).map(k => tombs.get(k).cell)
      // Reap-name removal happens BEFORE detect_vanished, so the
      // controller's own deletes are not misread as Karpenter GC.
      val vanished = CLAIMS.filter(k =>
        tombs.get(k).inflightTracked and not(stuckReaped.contains(k))
        and tombs.get(k).phase == CAbsent and not(tombConfirmed.contains(k)))
      // sh-030: the CONTROLLER ACTION gate — a vanish whose claim the
      // controller observed Registered is empty consolidation
      // (counter only, never a mark). vanishMarks maps over the gated
      // population; the law's spurious filter ranges over the SAME
      // population (a mark that did not fire cannot be spurious).
      val vanishIce = vanished.filter(k =>
        not(ENABLE_EMPTY_CONSOLIDATION_GATE and tombs.get(k).everRegistered))
      val vanishMarks: Set[Cell] = vanishIce.map(k => tombs.get(k).inflightCell)
      // bug_043 widening: a mark for a claim whose teardown is OURS by
      // ground truth (ctrlReaped — completed; ourCommit — committed
      // ambiguous) is spurious whatever the controller still knows.
      // sh-030 widening (UNCONDITIONAL — the strengthened soundness
      // law): so is a mark for a claim with everRegistered.
      val spurious = vanishIce.filter(k =>
        tombs.get(k).ctrlReaped or tombs.get(k).ourCommit
        or tombs.get(k).everRegistered).size() > 0
      // A confirmed Ice-reason tombstone re-applies its mask late —
      // the model's reasons collapse to the stuck (ICE-shaped) reap,
      // so a confirmed formerly-stuck claim's cell marks now (the
      // SelfReap mask parity); idle-shaped confirms carry no mask.
      val confirmMarks: Set[Cell] = tombConfirmed.filter(k =>
        tombs.get(k).inflightTracked).map(k => tombs.get(k).inflightCell)
      val marksThisTick = stuckMarks.union(vanishMarks).union(confirmMarks)
      val masked = iceMasked.union(marksThisTick)
      // ---- cover_deficit ----------------------------------------------
      val freeSlots = CLAIMS.filter(k => afterObs.get(k).phase == CAbsent)
      // sh-043: the model has no Registered-with-Launched=Unknown
      // state and CInFlight already excludes CTerminating, so the
      // Rust-side `!n.registered && !n.terminating()` guards collapse
      // to the phase test.
      val unlaunchedBefore = CLAIMS.filter(k => afterObs.get(k).phase == CInFlight).size()
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
      // sh-043 / live_049 L1 / round-9 WO-S7-2 (merged_bug_003): the
      // three-term mint law — demand × budget × inflight-headroom. In
      // claim units (every claim one core, chunk = 1) the production
      // `n = n_aff.min(budget / chunk).min(cfg.inflight_headroom)`
      // (cover.rs::sizing) is n = min(deficit, remaining budget,
      // headroom); the retired flat per-cell-per-tick cap stays GONE
      // from the relation exactly as 2e2ce1a32 removed it from the
      // tree. wantB's headroom term debits `wantA` (NOT `createdA`:
      // `createdA` is bound below, after `wantB`; the `- wantA` debit
      // matches the existing `globalRemaining - wantA` shape on the
      // same line). At MAX=|CLAIMS|=2 the headroom term is
      // SHAPE-PARITY ONLY (phases partition CLAIMS ⇒ freeSlots ≤
      // MAX − unlaunchedBefore, so freeSlots already dominates).
      val wantA = if (coverableA)
          min2(min2(demandA, min2(classRemainingA, globalRemaining)),
               MAX_INFLIGHT_UNLAUNCHED - unlaunchedBefore)
        else 0
      val wantB = if (coverableB)
          min2(min2(demandB, globalRemaining - wantA),
               MAX_INFLIGHT_UNLAUNCHED - unlaunchedBefore - wantA)
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
      // Round-11: the delete-outcome split per reaped claim — the
      // completed arm (Ok and 404, one arm: the parity law) vs the
      // ambiguous arm (tombstone, no consequence yet).
      val ambReaped = stuckReaped.union(idleReaped).intersect(ambChoice)
      val completedReaped = stuckReaped.union(idleReaped).exclude(ambReaped)
      val claimsAfter = CLAIMS.mapBy(k =>
        val c = tombs.get(k)
        if (completedReaped.contains(k))
          // ourCommit machinery-gated: with the plane off the field
          // is constant false and the reachable state space is
          // byte-identical to the pre-round-11 regimes.
          { ...c, phase: CTerminating, stuck: false, spell: SNotIdle,
            busyTickSeen: false, boot: BRecorded, pIdle: INotTracked,
            inflightTracked: false, ctrlReaped: true,
            tomb: false, tombAge: 0,
            ourCommit: ENABLE_TOMBSTONE_MACHINERY }
        else if (ambReaped.contains(k))
          // The AmbiguousErr arm: NO consequence yet; the tombstone is
          // (re-)stamped; whether the delete committed is the
          // environment's truth — a committed delete surfaces as
          // terminating at the next LIST.
          { ...c,
            phase: if (commitChoice.contains(k) and not(ENABLE_LAGGED_COMMIT_OBSERVATION))
                     CTerminating else c.phase,
            stuck: if (commitChoice.contains(k) and not(ENABLE_LAGGED_COMMIT_OBSERVATION))
                     false else c.stuck,
            tomb: true, tombAge: 0,
            ourCommit: c.ourCommit or commitChoice.contains(k) }
        else if (tombConfirmed.contains(k))
          // The late consequence applied; provenance consumed; the
          // tracking story is over (the fold exit / the sweep).
          { ...c, tomb: false, tombAge: 0, inflightTracked: false,
            ctrlReaped: true }
        else if (vanished.contains(k)) { ...c, inflightTracked: false }
        else if (slotForA.contains(k))
          { phase: CInFlight, cell: "cellA", stuck: false, spell: SNotIdle,
            busyTickSeen: false, boot: BRecorded, inflightTracked: true,
            inflightCell: "cellA", everRegistered: false,
            pIdle: INotTracked, ctrlReaped: false,
            tomb: false, tombAge: 0, ourCommit: false }
        else if (slotForB.contains(k))
          { phase: CInFlight, cell: "cellB", stuck: false, spell: SNotIdle,
            busyTickSeen: false, boot: BRecorded, inflightTracked: true,
            inflightCell: "cellB", everRegistered: false,
            pIdle: INotTracked, ctrlReaped: false,
            tomb: false, tombAge: 0, ourCommit: false }
        else if (c.inflightTracked and (c.phase == CRegistered or c.phase == CTerminating))
          // detect_vanished drops entries for registered/terminating
          // claims. merged_bug_050: the as-built uniform epilogue
          // ALSO consumed the tombstone at these exits — on
          // disconfirmed-only evidence; typed disposition hands it
          // to the registered-population sweep instead.
          { ...c, inflightTracked: false,
            tomb: if (ENABLE_TYPED_TOMBSTONE_DISPOSITION) c.tomb else false,
            tombAge: if (ENABLE_TYPED_TOMBSTONE_DISPOSITION) c.tombAge else 0 }
        else c)
      // Fixed law: the post-consult fold-clock advance + prune (see
      // the consolidate-only twin for the derivation).
      val foldExpired = CLAIMS.filter(k =>
        val c = claimsAfter.get(k)
        ENABLE_TOMBSTONE_MACHINERY and ENABLE_FOLD_CLOCK and c.tomb
        and not(ambReaped.contains(k)) and c.tombAge + 1 > TOMBSTONE_TTL)
      val claimsAfterTomb = CLAIMS.mapBy(k =>
        val c = claimsAfter.get(k)
        if (foldExpired.contains(k)) { ...c, tomb: false, tombAge: 0 }
        else if (ENABLE_TOMBSTONE_MACHINERY and ENABLE_FOLD_CLOCK and c.tomb
                 and not(ambReaped.contains(k)))
          { ...c, tombAge: c.tombAge + 1 }
        else c)
      // merged_bug_050 (round-12): the as-built uniform epilogue
      // consumes the tombstone at the registered-handoff exit on
      // disconfirmed-only evidence — a committed delete's packet is
      // lost at that consume (the third drop path beside the two
      // prune paths below).
      val handoffDropped = CLAIMS.filter(k =>
        val c = tombs.get(k)
        ENABLE_TOMBSTONE_MACHINERY and not(ENABLE_TYPED_TOMBSTONE_DISPOSITION)
        and c.inflightTracked and c.tomb and c.ourCommit
        and c.phase == CRegistered)
      val vTombDropped = ENABLE_TOMBSTONE_MACHINERY and (
        prePruneDropped.filter(k => preAged.get(k).ourCommit).size() > 0
        or foldExpired.filter(k => claimsAfter.get(k).ourCommit).size() > 0
        or handoffDropped.size() > 0)
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
      // sh-043: the headroom arg is SHAPE-PARITY ONLY at
      // MAX=|CLAIMS|=2 (the existing freeSlots.size() arg already
      // dominates: freeSlots ≤ MAX − unlaunchedBefore by phase
      // partition), so this relaxation is a documented no-op exactly
      // like `vBudgetGlobal` — the model RECORDS the law's third term
      // without changing its reachable verdicts. Per-disjunct
      // inside-min, NOT an outer antecedent (which would mask the
      // disjunct at cap and gratuitously weaken the check).
      val vDeficitDeferred =
        (coverableA and not(createFails)
          and createdA < min2(min2(demandA, classRemainingA),
                              min2(min2(globalRemaining, freeSlots.size()),
                                   MAX_INFLIGHT_UNLAUNCHED - unlaunchedBefore)))
        or
        (coverableB and not(createFails)
          and createdB < min2(min2(demandB, globalRemaining - createdA),
                              min2(freeSlots.size() - createdA,
                                   MAX_INFLIGHT_UNLAUNCHED - unlaunchedBefore - createdA)))
      // sh-043: the unlaunched count reached MAX_INFLIGHT_UNLAUNCHED
      // with surplus demand (at |CLAIMS|=MAX=2 this coincides with
      // the slot ceiling — shape-parity witness).
      val eInflightCapped =
        (createdA + createdB > 0)
        and (unlaunchedBefore + createdA + createdB == MAX_INFLIGHT_UNLAUNCHED)
        and (demandA + demandB > createdA + createdB)
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
        claims' = claimsAfterTomb,
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
          if (eInflightCapped) Set(EInflightCapped) else Set(),
          if (deferKept) Set(EDeferKept) else Set(),
          if (createAttempted and createFails) Set(ECreateFailed) else Set(),
          if (ackOk and marksThisTick.size() > 0) Set(EIceMarkEmitted) else Set(),
          if (ackOk and clears.size() > 0) Set(EClearEmitted) else Set(),
          if (clearOfMasked) Set(EClearOfMaskedCell) else Set(),
          if (vanished.size() > 0) Set(EVanishDetected) else Set(),
          if (stuckReaped.size() > 0) Set(EStuckReap) else Set(),
          if (idleReaped.size() > 0) Set(EIdleReap) else Set(),
          if (ambReaped.size() > 0) Set(EReapAmbiguous) else Set(),
          if (tombConfirmed.size() > 0) Set(ETombstoneConfirmed) else Set(),
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
          if (vTombDropped) Set(VTombstoneDropped) else Set(),
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
    nondet ambChoice = tombChoices.oneOf()
    nondet commitAll = boolChoices(ENABLE_TOMBSTONE_MACHINERY).oneOf()
    tickFullWith(
      pgLoadFails, ackFails, createFails, demandA, demandB, reservedChoice,
      deferredChoice, ambChoice, if (commitAll) ambChoice else Set())
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
      everRegistered: true,
      boot: if (isLeader) BFreshTenure else BFreshOther }),
    envFrame,
  }

  /// merged_bug_050 env: an in-flight claim whose ambiguous delete
  /// is pending REGISTERS — the racing registration the as-built
  /// RegisteredHandoff exit consumes the tombstone on. Distinct from
  /// claimRegisters: the production in-flight reap (the controller's
  /// own ICE timeout) fires WITHOUT a terminal Karpenter condition,
  /// so its claim can register late; the `stuck` flag stands for the
  /// reap trigger, not a terminal fault, on this lane.
  action claimRegistersLate(k: ClaimId): bool = all {
    ENABLE_LAGGED_COMMIT_OBSERVATION,
    claims.get(k).phase == CInFlight,
    claims.get(k).tomb,
    claims' = claims.set(k, { ...claims.get(k),
      phase: CRegistered, spell: SYoungUnseen, busyTickSeen: false,
      everRegistered: true,
      boot: if (isLeader) BFreshTenure else BFreshOther }),
    envFrame,
  }

  /// merged_bug_050 env: the lagged ambiguous-delete commit surfaces
  /// on the next LIST (deletionTimestamp visible — the finalizer is
  /// draining).
  action ambiguousCommitSurfaces(k: ClaimId): bool = all {
    ENABLE_LAGGED_COMMIT_OBSERVATION,
    claims.get(k).ourCommit,
    claims.get(k).phase == CRegistered,
    claims' = claims.set(k,
      { ...claims.get(k), phase: CTerminating, stuck: false }),
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
      // sh-030: everRegistered is the controller's accumulated
      // observation, carried through the absent state exactly like
      // inflightTracked (the qnt-everregistered-absentclaim-reset
      // review finding: an absentClaim spread without this carry
      // resets the latch and the calibration trace passes vacuously).
      everRegistered: claims.get(k).everRegistered,
      ctrlReaped: claims.get(k).ctrlReaped,
      // The tombstone plane is CONTROLLER state keyed by name — the
      // claim's absence does not erase it (the consult adjudicates).
      tomb: claims.get(k).tomb,
      tombAge: claims.get(k).tombAge,
      ourCommit: claims.get(k).ourCommit }),
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
      everRegistered: claims.get(k).everRegistered,
      ctrlReaped: claims.get(k).ctrlReaped,
      tomb: claims.get(k).tomb,
      tombAge: claims.get(k).tombAge,
      ourCommit: claims.get(k).ourCommit }),
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
    // bug_094 suppress polarity: tombstones clear on the acquisition
    // edge beside inflight tracking (ourCommit is ground truth and
    // survives — but tracking clears too, so no mark can fire on it).
    claims' = CLAIMS.mapBy(k =>
      { ...acquireRekey(claims.get(k)), tomb: false, tombAge: 0 }),
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
        inflightTracked: false, pIdle: INotTracked,
        tomb: false, tombAge: 0 }),
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

  /// Round-11 L1 regime step — the tombstone HOLD check's dedicated
  /// transition relation: the law's OWN axes are explored
  /// exhaustively (the delete-outcome nondets, the full tick family
  /// with its skip paths, the claim lifecycle environment, the mask
  /// cycle) while the axes the law does not quantify over are PINNED
  /// (ackFails/createFails/pgLoadFails = false; demandA ∈ {0,1},
  /// demandB = 0; reserved = ∅; deferred = ∅; no lease/config
  /// faults). Each pinned axis keeps its own exhaustive green regime
  /// elsewhere (fault-rpc, fault-lease, defer, epoch, …) — P7 regime
  /// design: the exhaustive tier is PRESERVED for the tombstone law
  /// itself, with per-full-tick branching cut ~72× vs the default
  /// step under the same consts.
  action stepTombstoneHold = {
    nondet k = CLAIMS.oneOf()
    nondet cell = CELLS.oneOf()
    nondet ambChoice = tombChoices.oneOf()
    nondet commitAll = boolChoices(ENABLE_TOMBSTONE_MACHINERY).oneOf()
    nondet demandA = Set(0, 1).oneOf()
    any {
      tickFullWith(false, false, false, demandA, 0, Set(), Set(),
        ambChoice, if (commitAll) ambChoice else Set()),
      tickBotEarlyWith(false),
      tickConsolidateOnlyWith(false, ambChoice,
        if (commitAll) ambChoice else Set()),
      claimRegisters(k),
      claimGoesBusy(k),
      claimGoesIdle(k),
      claimLaunchStuck(k),
      karpenterGC(k),
      karpenterTerminationEdge(k),
      finalizerCompletes(k),
      iceClears(cell),
    }
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
    ENABLE_DEFER_DEMAND = false,
    ENABLE_TOMBSTONE_MACHINERY = false,
    ENABLE_FOLD_CLOCK = false,
    ENABLE_LAGGED_COMMIT_OBSERVATION = false,
    ENABLE_TYPED_TOMBSTONE_DISPOSITION = true,
    ENABLE_EMPTY_CONSOLIDATION_GATE = true
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
    ENABLE_DEFER_DEMAND = false,
    ENABLE_TOMBSTONE_MACHINERY = false,
    ENABLE_FOLD_CLOCK = false,
    ENABLE_LAGGED_COMMIT_OBSERVATION = false,
    ENABLE_TYPED_TOMBSTONE_DISPOSITION = true,
    ENABLE_EMPTY_CONSOLIDATION_GATE = true
  ).*

  /// The conflation trace, post-fix: the claim is observed idle, goes
  /// busy and idle again across ⊥ ticks — the busy edge IS observed on
  /// the ⊥ tick (prune + re-seed), so the threshold-crossing full tick
  /// never over-reaps the fresh spell. The documented trace HOLDS
  /// end-to-end (it falsified VIdleOverReap on the pre-fix encoding).
  run idlePruneAcrossBotRun =
    init
      .then(ceilingsLoad)
      .then(tickFullWith(false, false, false, 1, 0, Set(), Set(), Set(), Set()))
      .then(claimRegisters("c1"))
      .then(tickFullWith(false, false, false, 0, 0, Set(), Set(), Set(), Set()))
      .then(claimGoesBusy("c1"))
      .then(tickBotEarlyWith(false))
      .then(claimGoesIdle("c1"))
      .then(tickFullWith(false, false, false, 0, 0, Set(), Set(), Set(), Set()))
      .expect(not(violations.contains(VIdleOverReap)))
      .expect(claims.get("c1").pIdle != INotTracked)

  /// The boot half of the same trace, post-fix: a Registered edge
  /// inside the pre-threshold window is recorded ON the ⊥ tick (clear
  /// discarded), so the recency gate never expires it and
  /// BootRecordedOnce's no-loss clause HOLDS.
  run bootRecordedAcrossBotRun =
    init
      .then(ceilingsLoad)
      .then(tickFullWith(false, false, false, 1, 0, Set(), Set(), Set(), Set()))
      .then(claimRegisters("c1"))
      .then(tickBotEarlyWith(false))
      .then(tickFullWith(false, false, false, 0, 0, Set(), Set(), Set(), Set()))
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
    ENABLE_DEFER_DEMAND = false,
    ENABLE_TOMBSTONE_MACHINERY = false,
    ENABLE_FOLD_CLOCK = false,
    ENABLE_LAGGED_COMMIT_OBSERVATION = false,
    ENABLE_TYPED_TOMBSTONE_DISPOSITION = true,
    ENABLE_EMPTY_CONSOLIDATION_GATE = true
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
    ENABLE_DEFER_DEMAND = false,
    ENABLE_TOMBSTONE_MACHINERY = false,
    ENABLE_FOLD_CLOCK = false,
    ENABLE_LAGGED_COMMIT_OBSERVATION = false,
    ENABLE_TYPED_TOMBSTONE_DISPOSITION = true,
    ENABLE_EMPTY_CONSOLIDATION_GATE = true
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
    ENABLE_DEFER_DEMAND = false,
    ENABLE_TOMBSTONE_MACHINERY = false,
    ENABLE_FOLD_CLOCK = false,
    ENABLE_LAGGED_COMMIT_OBSERVATION = false,
    ENABLE_TYPED_TOMBSTONE_DISPOSITION = true,
    ENABLE_EMPTY_CONSOLIDATION_GATE = true
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
    ENABLE_DEFER_DEMAND = false,
    ENABLE_TOMBSTONE_MACHINERY = false,
    ENABLE_FOLD_CLOCK = false,
    ENABLE_LAGGED_COMMIT_OBSERVATION = false,
    ENABLE_TYPED_TOMBSTONE_DISPOSITION = true,
    ENABLE_EMPTY_CONSOLIDATION_GATE = true
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
    ENABLE_DEFER_DEMAND = false,
    ENABLE_TOMBSTONE_MACHINERY = false,
    ENABLE_FOLD_CLOCK = false,
    ENABLE_LAGGED_COMMIT_OBSERVATION = false,
    ENABLE_TYPED_TOMBSTONE_DISPOSITION = true,
    ENABLE_EMPTY_CONSOLIDATION_GATE = true
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
    ENABLE_DEFER_DEMAND = false,
    ENABLE_TOMBSTONE_MACHINERY = false,
    ENABLE_FOLD_CLOCK = false,
    ENABLE_LAGGED_COMMIT_OBSERVATION = false,
    ENABLE_TYPED_TOMBSTONE_DISPOSITION = true,
    ENABLE_EMPTY_CONSOLIDATION_GATE = true
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
    ENABLE_DEFER_DEMAND = false,
    ENABLE_TOMBSTONE_MACHINERY = false,
    ENABLE_FOLD_CLOCK = false,
    ENABLE_LAGGED_COMMIT_OBSERVATION = false,
    ENABLE_TYPED_TOMBSTONE_DISPOSITION = true,
    ENABLE_EMPTY_CONSOLIDATION_GATE = true
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
    ENABLE_DEFER_DEMAND = false,
    ENABLE_TOMBSTONE_MACHINERY = false,
    ENABLE_FOLD_CLOCK = false,
    ENABLE_LAGGED_COMMIT_OBSERVATION = false,
    ENABLE_TYPED_TOMBSTONE_DISPOSITION = true,
    ENABLE_EMPTY_CONSOLIDATION_GATE = true
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
    ENABLE_DEFER_DEMAND = false,
    ENABLE_TOMBSTONE_MACHINERY = false,
    ENABLE_FOLD_CLOCK = false,
    ENABLE_LAGGED_COMMIT_OBSERVATION = false,
    ENABLE_TYPED_TOMBSTONE_DISPOSITION = true,
    ENABLE_EMPTY_CONSOLIDATION_GATE = true
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
    ENABLE_DEFER_DEMAND = false,
    ENABLE_TOMBSTONE_MACHINERY = false,
    ENABLE_FOLD_CLOCK = false,
    ENABLE_LAGGED_COMMIT_OBSERVATION = false,
    ENABLE_TYPED_TOMBSTONE_DISPOSITION = true,
    ENABLE_EMPTY_CONSOLIDATION_GATE = true
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
    ENABLE_DEFER_DEMAND = true,
    ENABLE_TOMBSTONE_MACHINERY = false,
    ENABLE_FOLD_CLOCK = false,
    ENABLE_LAGGED_COMMIT_OBSERVATION = false,
    ENABLE_TYPED_TOMBSTONE_DISPOSITION = true,
    ENABLE_EMPTY_CONSOLIDATION_GATE = true
  ).*
}

/// Round-11 as-built twin (bug_042 + bug_043, WO-S4-1/-2): the
/// delete-outcome tombstone machinery under the AS-BUILT clock law —
/// the TTL ages on every leader tick (including the foldless
/// pre-threshold ⊥ ticks) and prune_expired runs BEFORE the consult.
/// The W11-AI shape is reachable: an ambiguous-commit reap's
/// tombstone crosses a ≥TTL foldless window, the first fold prunes
/// it unconsulted, and the claim's absence mints foreign-attributed
/// vanish evidence for the controller's own delete — falsifying
/// iceMarkSoundness AND tombstoneConsequenceApplied (both wired as
/// expected-violation checks). BOT_THRESHOLD=3 widens the ⊥ window to
/// two foldless ticks (mirroring the real 5-tick threshold vs the
/// 3-tick TTL: the window can exceed the grace).
module nodeclaimLifecycleTombstoneAsBuilt {
  import nodeclaimLifecycle(
    MAX_FLEET_CORES = 2,
    CLASS_CAP_A = 1,
    BOT_THRESHOLD = 3,
    ENABLE_RPC_FAULT = true,
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
    ENABLE_DEFER_DEMAND = false,
    ENABLE_TOMBSTONE_MACHINERY = true,
    ENABLE_FOLD_CLOCK = false,
    ENABLE_LAGGED_COMMIT_OBSERVATION = false,
    ENABLE_TYPED_TOMBSTONE_DISPOSITION = true,
    ENABLE_EMPTY_CONSOLIDATION_GATE = true
  ).*

  /// Deterministic W11-AI reproducer (the foldless-window
  /// tombstone-drop trace, pinned so the falsification is a named
  /// run, not just an unbounded search): create → launch-stuck →
  /// ambiguous-commit reap → two foldless ⊥ ticks age the wall TTL
  /// past the grace → finalizer completes → the consolidate-only
  /// fold prunes the tombstone BEFORE consulting it and mints the
  /// false vanish mark for the controller's own committed delete.
  run w11aiFoldlessWindowDropRed = {
    init
      .then(tickFullWith(false, false, false, 1, 0, Set(), Set(), Set(), Set()))
      .then(claimLaunchStuck("c1"))
      .then(tickFullWith(false, false, false, 0, 0, Set(), Set(), Set("c1"), Set("c1")))
      .then(tickBotEarlyWith(false))
      .then(tickBotEarlyWith(false))
      .then(finalizerCompletes("c1"))
      .then(tickConsolidateOnlyWith(false, Set(), Set()))
      .expect(violations.contains(VSpuriousMark) and violations.contains(VTombstoneDropped))
  }
}

/// Round-11 hold regime (ctrl.pool.delete-outcome +
/// ctrl.pool.fold-clock): the tombstone machinery under the FIXED
/// law — the TTL is denominated in consumer fold executions and the
/// prune runs after the consult (one chokepoint). The same foldless
/// window now lengthens real grace instead of consuming it: the
/// tombstone survives to its first consult, the confirm applies the
/// original consequence (ETombstoneConfirmed), and no
/// foreign-attributed mark is minted. iceMarkSoundness and
/// tombstoneConsequenceApplied HOLD; canReachReapAmbiguous /
/// canReachTombstoneConfirmed are the non-vacuity witnesses.
module nodeclaimLifecycleTombstone {
  import nodeclaimLifecycle(
    MAX_FLEET_CORES = 2,
    CLASS_CAP_A = 1,
    // 2, not the falsifier's 3: under the FIXED law the grace clock
    // is skip-invariant by construction (foldless ticks do not move
    // it), so the hold needs skip paths to EXIST, not a window
    // longer than the grace — that face is the as-built falsifier's.
    // The smaller streak domain keeps this exhaustive check inside
    // the fault-karpenter-class budget.
    BOT_THRESHOLD = 2,
    ENABLE_RPC_FAULT = true,
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
    ENABLE_DEFER_DEMAND = false,
    ENABLE_TOMBSTONE_MACHINERY = true,
    ENABLE_FOLD_CLOCK = true,
    ENABLE_LAGGED_COMMIT_OBSERVATION = false,
    ENABLE_TYPED_TOMBSTONE_DISPOSITION = true,
    ENABLE_EMPTY_CONSOLIDATION_GATE = true
  ).*

  /// The W11-AI shape under the fixed law (scaled to this regime's
  /// BOT_THRESHOLD=2: ONE foldless ⊥ tick before consolidate-only —
  /// the clock-invariance being certified is per-skip, so one skip
  /// witnesses it): the tombstone SURVIVES the foldless window (the
  /// fold clock never moved), the first fold consults and confirms
  /// it, and neither violation fires — the deterministic green twin
  /// of the as-built red run (which keeps THRESHOLD=3 and the full
  /// window-exceeds-grace trace).
  run w11aiFoldlessWindowSurvivesGreen = {
    init
      .then(tickFullWith(false, false, false, 1, 0, Set(), Set(), Set(), Set()))
      .then(claimLaunchStuck("c1"))
      .then(tickFullWith(false, false, false, 0, 0, Set(), Set(), Set("c1"), Set("c1")))
      .then(tickBotEarlyWith(false))
      .then(finalizerCompletes("c1"))
      .then(tickConsolidateOnlyWith(false, Set(), Set()))
      .expect(effects.contains(ETombstoneConfirmed)
        and not(violations.contains(VSpuriousMark))
        and not(violations.contains(VTombstoneDropped)))
  }
}


/// Round-12 merged_bug_050 AS-BUILT face: the uniform vanish-exit
/// epilogue consumes the delete tombstone on EVERY exit — including
/// the RegisteredHandoff exit's disconfirmed-only evidence. The
/// deterministic red: an in-flight claim is ambiguous-reaped (the
/// delete COMMITS but the LIST lags), its racing registration
/// surfaces, and the next fold's handoff exit eats the tombstone —
/// the committed delete's consequence packet is unrecoverably lost
/// (VTombstoneDropped), while the registered-population sweep would
/// have kept identical evidence armed one population over.
module nodeclaimLifecycleTombstoneDispositionAsBuilt {
  import nodeclaimLifecycle(
    MAX_FLEET_CORES = 2,
    CLASS_CAP_A = 1,
    BOT_THRESHOLD = 3,
    ENABLE_RPC_FAULT = true,
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
    ENABLE_DEFER_DEMAND = false,
    ENABLE_TOMBSTONE_MACHINERY = true,
    ENABLE_FOLD_CLOCK = true,
    ENABLE_LAGGED_COMMIT_OBSERVATION = true,
    ENABLE_TYPED_TOMBSTONE_DISPOSITION = false,
    ENABLE_EMPTY_CONSOLIDATION_GATE = true
  ).*

  /// The merged_bug_050 reproducer, pinned as a named deterministic
  /// run: mint → launch-stuck → ambiguous reap (commit lags the
  /// LIST) → the registration races in → the handoff fold consumes
  /// the tombstone on disconfirmed evidence → the committed packet
  /// is lost.
  run w12anRegisteredHandoffConsumeRed = {
    init
      .then(tickFullWith(false, false, false, 1, 0, Set(), Set(), Set(), Set()))
      .then(claimLaunchStuck("c1"))
      .then(tickFullWith(false, false, false, 0, 0, Set(), Set(), Set("c1"), Set("c1")))
      .then(claimRegistersLate("c1"))
      .then(tickFullWith(false, false, false, 0, 0, Set(), Set(), Set(), Set()))
      .expect(violations.contains(VTombstoneDropped))
  }
}

/// Round-12 merged_bug_050 FIXED face (typed per-exit disposition,
/// `sys.obligation.linear-discharge` instance): the handoff exits
/// HAND the tombstone to the registered-population sweep; when the
/// lagged commit surfaces, the sweep confirms on a post-stamp LIST
/// and the ORIGINAL consequence packet fires (ETombstoneConfirmed) —
/// no drop, no foreign attribution.
module nodeclaimLifecycleTombstoneDisposition {
  import nodeclaimLifecycle(
    MAX_FLEET_CORES = 2,
    CLASS_CAP_A = 1,
    BOT_THRESHOLD = 3,
    ENABLE_RPC_FAULT = true,
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
    ENABLE_DEFER_DEMAND = false,
    ENABLE_TOMBSTONE_MACHINERY = true,
    ENABLE_FOLD_CLOCK = true,
    ENABLE_LAGGED_COMMIT_OBSERVATION = true,
    ENABLE_TYPED_TOMBSTONE_DISPOSITION = true,
    ENABLE_EMPTY_CONSOLIDATION_GATE = true
  ).*

  /// The same trace under the typed law, extended to the confirm:
  /// the handoff keeps the tombstone, the commit surfaces, and the
  /// first post-stamp fold fires the packet whole — the green twin
  /// of w12anRegisteredHandoffConsumeRed.
  run w12anHandoffHandsToSweepGreen = {
    init
      .then(tickFullWith(false, false, false, 1, 0, Set(), Set(), Set(), Set()))
      .then(claimLaunchStuck("c1"))
      .then(tickFullWith(false, false, false, 0, 0, Set(), Set(), Set("c1"), Set("c1")))
      .then(claimRegistersLate("c1"))
      .then(tickFullWith(false, false, false, 0, 0, Set(), Set(), Set(), Set()))
      .then(ambiguousCommitSurfaces("c1"))
      .then(tickFullWith(false, false, false, 0, 0, Set(), Set(), Set(), Set()))
      .expect(effects.contains(ETombstoneConfirmed)
        and not(violations.contains(VTombstoneDropped)))
  }
}

/// Round-13 sh-030 AS-BUILT face: the unconditional GcVanish row
/// ICE-masks a tracked claim that Registered, sat empty past
/// Karpenter consolidateAfter, and vanished across a controller-blind
/// window — Karpenter cleanup minted as capacity failure. The
/// deterministic red: mint → register (no controller tick observes
/// the handoff) → karpenterTerminationEdge → finalizerCompletes (the
/// everRegistered carry survives both absentClaim spreads) → tick:
/// vanishIce = vanished (gate off), vanishMarks fires, the
/// strengthened spurious law catches it → VSpuriousMark.
module nodeclaimLifecycleEmptyConsolidationAsBuilt {
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
    ENABLE_DEFER_DEMAND = false,
    ENABLE_TOMBSTONE_MACHINERY = false,
    ENABLE_FOLD_CLOCK = false,
    ENABLE_LAGGED_COMMIT_OBSERVATION = false,
    ENABLE_TYPED_TOMBSTONE_DISPOSITION = true,
    ENABLE_EMPTY_CONSOLIDATION_GATE = false
  ).*

  /// The sh-030 reproducer, pinned as a named deterministic run: the
  /// controller is blind across the Registered→Absent transition (a
  /// stalled tick — c1's 3×TICK bound is the Rust-side close). The
  /// .expect IS the calibration's non-vacuity proof: with the gate
  /// off, the unconditional vanish mark is spurious under the
  /// strengthened law.
  run sh030EmptyConsolidationRed = {
    init
      .then(tickFullWith(false, false, false, 1, 0, Set(), Set(), Set(), Set()))
      .then(claimRegisters("c1"))
      .then(karpenterTerminationEdge("c1"))
      .then(finalizerCompletes("c1"))
      .then(tickFullWith(false, false, false, 0, 0, Set(), Set(), Set(), Set()))
      .expect(violations.contains(VSpuriousMark))
  }
}

/// Round-13 sh-030 FIXED face (the EmptyConsolidation gate):
/// vanishIce excludes everRegistered claims, so vanishMarks never
/// fires for empty-node consolidation; the spurious filter ranges
/// over vanishIce → no VSpuriousMark. iceMarkSoundness HOLDS over
/// the full Karpenter-fault step relation (the same regime as
/// nodeclaimLifecycleFaultKarpenter, with the trace below as the
/// pinned green twin of sh030EmptyConsolidationRed).
module nodeclaimLifecycleEmptyConsolidation {
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
    ENABLE_DEFER_DEMAND = false,
    ENABLE_TOMBSTONE_MACHINERY = false,
    ENABLE_FOLD_CLOCK = false,
    ENABLE_LAGGED_COMMIT_OBSERVATION = false,
    ENABLE_TYPED_TOMBSTONE_DISPOSITION = true,
    ENABLE_EMPTY_CONSOLIDATION_GATE = true
  ).*

  /// The same trace under the gated law: the everRegistered claim is
  /// filtered out of vanishIce → no mark, no spurious — the green
  /// twin. EVanishDetected stays reachable for the never-Registered
  /// row (B11 preserved — pinned by canReachVanishMark in the
  /// FaultKarpenter regime, identical consts).
  run sh030EmptyConsolidationGreen = {
    init
      .then(tickFullWith(false, false, false, 1, 0, Set(), Set(), Set(), Set()))
      .then(claimRegisters("c1"))
      .then(karpenterTerminationEdge("c1"))
      .then(finalizerCompletes("c1"))
      .then(tickFullWith(false, false, false, 0, 0, Set(), Set(), Set(), Set()))
      .expect(not(violations.contains(VSpuriousMark)))
  }
}

// ─────────────────────────────────────────────────────────────────────
// WO-S7-1 (bug_151, R30 banner): the verdict-free-respawn GIVE-UP
// latch and its fresh-demand-epoch exit edge — a deliberately small
// companion machine beside Model N. The latch lives in the POOL
// reconciler (pool/candidate.rs::PoolStreaks), not the NodeClaim
// plane; the controller-formal corpus keeps its lifecycle models in
// this file (OQ-12: S4's DeleteOutcome letters extend the core
// module, this pair appends — disjoint regions).
//
// WHAT IS MODELED
//   One (pool, intent) respawn record against one drv's demand
//   stream. The environment owns the scheduler's demand epoch
//   (resubmit_cycle: bumped only when the drv re-enters Ready after
//   leaving) and the deadline that removes demand. One tick = the
//   production order: demand-seam pass (note_demand_epoch) → latch
//   filter (respawn_blocked) → spawn → optional verdict-free death
//   (the next reap's note_verdict_free_death, folded into the same
//   tick action — the ladder's TIMED windows are abstracted away:
//   only the latch face is modeled, so a mid-ladder tick MAY spawn).
//   demandCycle == -1 encodes the record's None (never observed an
//   epoch: first observation SETS, never decays — the fail-closed
//   arm).
//
// DIVERGENCE (recorded per the bw9 grammar): the model abstracts the
//   exponential backoff windows (time-free; mid-ladder spawns are
//   nondeterministic where production gates them on wall clock) and
//   the named-resolution alphabet (VerdictWitness lanes reset records
//   in production; omitting them only SHRINKS the reachable reset
//   set, so the absorbing-state falsification below is conservative —
//   a model WITH those lanes still cannot reset a gave-up record on
//   healthy-infra pod silence, which is exactly the bug_151 trap).
//
// THE TWIN PAIR (P7): respawnGiveUp (DECAY_ENABLED=true, the landed
//   code) holds freshEpochServed and reaches EGaveUpDecay (the R30
//   reachability face, wired as an expect-violation witness on
//   canReachGiveUpDecay); respawnGiveUpAsBuilt (DECAY_ENABLED=false,
//   the pre-fix tree) falsifies freshEpochServed — the absorbing
//   trace, machine-found.
module respawnGiveUpCore {
  /// The fix flag: the demand-seam decay arm exists (true = landed
  /// tree; false = the pre-fix absorbing-state encoding).
  const DECAY_ENABLED: bool
  /// Round-12 merged_bug_043: the decay guard keys on epoch CHANGE
  /// (true = the landed change-keyed law; false = the round-11
  /// order-keyed `>` encoding, whose exit edge is vacuous for the
  /// REWOUND orbit — produced by the poison-TTL lane's zero stamp
  /// and the failover floor-loss corner; the admin ClearPoison lane
  /// bumps `cycles + 1` as of bug_058 and no longer rewinds).
  const CHANGE_KEYED: bool
  /// RESPAWN_GIVE_UP_DEATHS, scaled for the explored space.
  const GIVE_UP: int
  /// Bound on the scheduler's resubmit cycle.
  const MAX_CYCLE: int

  type GiveUpEffect =
    | EGaveUpDecay
    | ESpawn
  // VFreshEpochStarved: a latched record observed a CHANGED demand
  // epoch (newer or rewound — merged_bug_043) and the intent was
  // still withheld that tick — the starved demand signal (the
  // bug_151 absorbing state / the round-12 rewind orbit, live).
  type GiveUpViolation =
    | VFreshEpochStarved

  /// 0 = no record (production: the map has no row); else the
  /// record's verdict-free death count, saturated at GIVE_UP + 1.
  var recDeaths: int
  /// The record's last-observed demand epoch; -1 = None (the record
  /// has never seen its intent at the seam).
  var recCycle: int
  /// The scheduler's current resubmit cycle for the drv.
  var envCycle: int
  /// The drv is in the wanted stream (demand is observable).
  var intentPresent: bool
  var giveUpViolations: Set[GiveUpViolation]
  var giveUpEffects: Set[GiveUpEffect]

  action init = all {
    recDeaths' = 0,
    recCycle' = -1,
    envCycle' = 0,
    intentPresent' = true,
    giveUpViolations' = Set(),
    giveUpEffects' = Set(),
  }

  def latched(d: int): bool = d >= GIVE_UP

  /// One reconcile tick while demand is present, in production order
  /// (seam → filter → spawn → optional verdict-free death).
  action tickWith(dies: bool): bool = all {
    intentPresent,
    val hasRecord = recDeaths > 0
    // The demand-signal TRUTH: any changed epoch (newer or rewound)
    // is the operator's intent signal (merged_bug_043).
    val epochChangedSignal = hasRecord and recCycle >= 0 and envCycle != recCycle
    // The encoding under test: change-keyed (landed) or order-keyed
    // (the round-11 guard, vacuous on the rewound orbit).
    val epochGuardFires = hasRecord and recCycle >= 0 and
      (if (CHANGE_KEYED) envCycle != recCycle else envCycle > recCycle)
    val decays = DECAY_ENABLED and latched(recDeaths) and epochGuardFires
    val starved = latched(recDeaths) and epochChangedSignal and not(decays)
    val midDeaths = if (decays) 0 else recDeaths
    val midCycle =
      if (decays) -1
      else if (hasRecord and (if (CHANGE_KEYED) envCycle != recCycle else envCycle > recCycle))
        envCycle
      else recCycle
    val spawns = not(latched(midDeaths))
    all {
      recDeaths' =
        if (spawns and dies)
          (if (midDeaths <= GIVE_UP) midDeaths + 1 else midDeaths)
        else midDeaths,
      // A death on a record-less intent MINTS the record with
      // demandCycle=None (production: row_or_insert_with at the
      // reap); an existing record keeps its seam-updated epoch.
      recCycle' = if (spawns and dies and midDeaths == 0) -1 else midCycle,
      envCycle' = envCycle,
      intentPresent' = intentPresent,
      giveUpViolations' =
        giveUpViolations.union(if (starved) Set(VFreshEpochStarved) else Set()),
      giveUpEffects' =
        giveUpEffects
          .union(if (decays) Set(EGaveUpDecay) else Set())
          .union(if (spawns) Set(ESpawn) else Set()),
    }
  }

  /// A tick with no wanted intent: the seam observes nothing, the
  /// latch holds (gave-up records are expiry-immune in production —
  /// the orphan expiry is out of scope here).
  action tickAbsent = all {
    not(intentPresent),
    recDeaths' = recDeaths,
    recCycle' = recCycle,
    envCycle' = envCycle,
    intentPresent' = intentPresent,
    giveUpViolations' = giveUpViolations,
    giveUpEffects' = giveUpEffects,
  }

  /// The drv's scheduler deadline lapses: demand leaves the stream.
  action deadlineExpires = all {
    intentPresent,
    intentPresent' = false,
    recDeaths' = recDeaths,
    recCycle' = recCycle,
    envCycle' = envCycle,
    giveUpViolations' = giveUpViolations,
    giveUpEffects' = giveUpEffects,
  }

  /// The operator resubmits the drv: it re-enters Ready with a
  /// strictly newer resubmit cycle (the demand epoch — the one
  /// signal observable from inside the gave-up state).
  action resubmit = all {
    not(intentPresent),
    envCycle < MAX_CYCLE,
    envCycle' = envCycle + 1,
    intentPresent' = true,
    recDeaths' = recDeaths,
    recCycle' = recCycle,
    giveUpViolations' = giveUpViolations,
    giveUpEffects' = giveUpEffects,
  }

  /// Round-12 merged_bug_043: demand returns with a REWOUND epoch.
  /// The epoch is a demand signal, not a monotone counter —
  /// scheduler lanes lawfully rewind it. Producer re-derivation at
  /// bug_058 (the action name is historical): the admin ClearPoison
  /// lane now BUMPS `cycles + 1` (completion.rs) and floors the
  /// post-clear re-insert, so it no longer produces this orbit; the
  /// remaining rewind producers are the poison-TTL expiry lane
  /// (still stamps 0) and a scheduler failover inside the
  /// clear→resubmit window (the in-memory floor is lost; the
  /// re-insert starts at 0). The orbit stays in the environment
  /// alphabet and the change-keyed decay stays load-bearing.
  action clearPoisonRewind = all {
    not(intentPresent),
    envCycle > 0,
    envCycle' = 0,
    intentPresent' = true,
    recDeaths' = recDeaths,
    recCycle' = recCycle,
    giveUpViolations' = giveUpViolations,
    giveUpEffects' = giveUpEffects,
  }

  action step = any {
    tickWith(true),
    tickWith(false),
    tickAbsent,
    deadlineExpires,
    resubmit,
    clearPoisonRewind,
  }

  /// HOLDS (fixed tree): no tick ever leaves a CHANGED demand epoch
  /// (newer or REWOUND — merged_bug_043) withheld by a latched
  /// record — the demand signal is served (decayed and spawned) the
  /// same tick it is observed.
  val freshEpochServed = not(giveUpViolations.contains(VFreshEpochStarved))

  /// Bounds (vacuity-exempt class boundsOK).
  val giveUpBoundsOK = and {
    recDeaths >= 0,
    recDeaths <= GIVE_UP + 1,
    recCycle >= -1,
    recCycle <= MAX_CYCLE,
    envCycle >= 0,
    envCycle <= MAX_CYCLE,
  }

  /// Reachability witness (expect-violation wiring): the decay cell
  /// IS reachable in the fixed encoding — R30's exit-edge face.
  val canReachGiveUpDecay = not(giveUpEffects.contains(EGaveUpDecay))
}

/// The landed encoding: the demand-seam decay arm exists. The
/// deterministic re-latch trace doubles as the budget pin (W11-BE's
/// quint half): give-up → resubmission decays and spawns the same
/// tick → fresh deaths re-latch at the FULL budget → the re-latched
/// epoch holds (no self-decay).
module respawnGiveUp {
  import respawnGiveUpCore(
    DECAY_ENABLED = true,
    CHANGE_KEYED = true,
    GIVE_UP = 2,
    MAX_CYCLE = 2
  ).*

  run relatchTrace =
    init
      .then(tickWith(true))      // spawn, verdict-free death 1 (record minted, cycle None)
      .then(tickWith(true))      // seam sets cycle=0; death 2 == GIVE_UP: latched
      .then(tickWith(false))     // latched at its own epoch: withheld, no decay
      .then(deadlineExpires)
      .then(resubmit)            // envCycle=1, demand returns
      .then(tickWith(true))      // DECAY + same-tick spawn; the new job dies again
      .expect(giveUpEffects.contains(EGaveUpDecay))
      .expect(recDeaths == 1)    // the budget restarted — not carried over
      .then(tickWith(true))      // seam sets cycle=1; death 2: re-latched at FULL budget
      .expect(recDeaths == 2)
      .then(tickWith(false))     // re-latched epoch holds: no decay at its own epoch
      .expect(freshEpochServed)

  /// Round-12 merged_bug_043 green twin: the same ClearPoison-rewind
  /// schedule under the change-keyed guard — the REWOUND epoch
  /// decays the latch and the spawn goes out the same tick; the
  /// re-presented equal epoch latches (anti-replay, W12-AP2).
  run w12apRewindDecaysGreen =
    init
      .then(deadlineExpires)
      .then(resubmit)            // envCycle=1
      .then(tickWith(true))      // death 1 (record minted, cycle None)
      .then(tickWith(true))      // seam sets cycle=1; death 2: latched
      .then(deadlineExpires)
      .then(clearPoisonRewind)   // demand returns at cycle 0 (REWOUND)
      .then(tickWith(false))     // change-keyed: 0 != 1 — DECAY + spawn
      .expect(giveUpEffects.contains(EGaveUpDecay))
      .expect(freshEpochServed)
      .then(tickWith(false))     // equal epoch (0 == 0... record gone; no-op spawn lane)
      .expect(freshEpochServed)
}

/// The pre-fix encoding (live-import falsify twin): no decay arm —
/// the latched record starves the resubmission forever
/// (freshEpochServed is violated; the absorbing trace machine-found).
module respawnGiveUpAsBuilt {
  import respawnGiveUpCore(
    DECAY_ENABLED = false,
    CHANGE_KEYED = false,
    GIVE_UP = 2,
    MAX_CYCLE = 2
  ).*
}

/// Round-12 merged_bug_043 falsify twin: the round-11 ORDER-KEYED
/// guard (`envCycle > recCycle`) with the decay arm present — the
/// exit edge is vacuous for exactly the post-ClearPoison REWOUND
/// orbit: the latched record observes cycle 0 < latched 1, the
/// `Some(_)` fallthrough absorbs it, and the documented operator
/// recovery never unblocks the spawn (VFreshEpochStarved — the
/// deterministic w12apRewindStarvedRed trace).
module respawnGiveUpOrderKeyed {
  import respawnGiveUpCore(
    DECAY_ENABLED = true,
    CHANGE_KEYED = false,
    GIVE_UP = 2,
    MAX_CYCLE = 2
  ).*

  run w12apRewindStarvedRed =
    init
      .then(deadlineExpires)
      .then(resubmit)            // envCycle=1: the drv was once resubmitted
      .then(tickWith(true))      // death 1 (record minted, cycle None)
      .then(tickWith(true))      // seam sets cycle=1; death 2 == GIVE_UP: latched
      .then(deadlineExpires)
      .then(clearPoisonRewind)   // ClearPoison: demand returns at cycle 0 (REWOUND)
      .then(tickWith(false))     // order-keyed guard: 0 > 1 false — ABSORBED
      .expect(giveUpViolations.contains(VFreshEpochStarved))
      .expect(not(giveUpEffects.contains(EGaveUpDecay)))
}
