// -*- mode: Bluespec; -*-
//
// Closure-evidence lifecycle model (the `topdown_pruned` / `closure_hole`
// campaign, Phase 0b of closure-evidence-formal). Verification subject:
// the as-built evidence lifecycle on `formal-sprint` — the pruned-merge
// stamp (S1/S2 in the code inventory; `validate_and_ingest` +
// `batch_upsert_derivations`), the five clears (the merge post-
// reconciliation pass, the completion-time clear, the recovery gate, the
// lazy walk-failure clear, the fail-fast consume), the four hole stamps
// (terminal-build reap, admin ClearPoison, the poison-TTL sweep, the
// recovery stamp), the merge heal, the recovery gates over the durable
// relation (`load_parents_with_all_children_produced`,
// `load_parents_with_unproduced_terminal_children`,
// `load_parents_with_failed_deps`), the single classifier
// (`closure_evidence` -> Vouched/Pending/Broken), `must_substitute`,
// `fail_fast_topdown_pruned_root` and its three caller arms, the prune's
// demand-set computation (`check_roots_topdown`), and the pull-admission
// interlock (`admit_pull`'s NotYetReady refusal).
//
// Campaign design: closure-evidence-formal-design.md (A2-approved
// revision); invariant <-> spec-rule linkage and stage records:
// docs/spec/models/closure-evidence-invariant-map.md. Check wiring and
// r[verify] markers live in nix/quint.nix (Phase 0c), never here.
//
// WHAT IS MODELED
//   - One model action = one actor-handler invocation's in-memory
//     effect, atomic (the scheduler actor is single-threaded).
//     Environment answers consumed mid-handler (store probe partitions,
//     walk verdicts) are parameters of that action constrained by the
//     store component.
//   - The durable vs in-memory split. Every best-effort evidence
//     statement a handler issues (the both-bits clear, the mark-only
//     clear, the hole stamp, the heal) becomes a tenure-tagged write
//     intent applied to the PG state by a separate pgApply action —
//     possibly after a failover (the deposed-believer model), possibly
//     never (pgDrop, the best-effort loss). The merge transaction
//     (upsert + edges + links + activation) is ONE all-or-nothing
//     intent. Fenced statements (the pull mint) are not intents: the
//     fence makes a stale apply abort, so only current-tenure mints
//     exist.
//   - The durable declared relation pgEdges (append-only via committed
//     merges, deleted only with the row by the orphan-row GC) and the
//     durable row/status/wanted-union state recovery reads. The strict
//     PG criterion of the recovery clear gate and the recovery hole
//     stamp are evaluated over this durable relation, never over the
//     live in-memory DAG.
//   - The store as ground truth per output: present / pinned /
//     upstream-has, plus the probe cache (positive and negative entries
//     that may lag upstream until a fresh consult). FindMissingPaths
//     answers are constrained by these (Present => present;
//     Substitutable => cache-hit or upstream-has; MissingConfirmed =>
//     not present and (cache-miss or upstream-lacks); Indeterminate is
//     always available for a missing path).
//   - The substitution walk as an asynchronous verdict: spawned by the
//     handler that sets Substituting (snapshotting the forgivable set),
//     its verdict fixed mid-flight by the walk task's own serial check
//     loop (walkFinishes, the CHECK time; ok=true ingests the walked
//     REFS-closure as it goes), then sent and processed as one step
//     (consumeWalk, the SEND time). The store may change between the
//     check and the send — that window is the walk's own duration, and
//     it is the ONLY staleness source: the actor's FIFO mailbox excludes
//     reorderings where an event caused by the walk's outcome (a
//     resubmit after the failure it triggers) is processed before the
//     completion itself (C3-adjudication correction; see section 7g).
//     Completions carry no tenure/walk token; a later tenure of the
//     same process may consume one (stale-tenure regimes only).
//   - Leader tenure as a generation counter; LeaderLost / Recover;
//     recovery as one atomic action over the durable state (row load,
//     poisoned-row TTL split, edge truncation, failed-dep condemnation,
//     the clear gate, the hole stamp, the in-memory-only Substituting
//     reset, interest rebuild without per-build wanted contributions).
//   - Builds, interest, per-build wanted contributions (memory-only,
//     lost at failover -> stored-union widening), cancellation, the
//     terminal-build reap with hole stamps and survivor verdicts, the
//     orphan-terminal row GC and the external builds-row purge.
//   - Settlement as armed-state invariants (house style): no temporal
//     properties; "eventually" claims are stated as "a settling
//     transition is armed in every reachable state of that shape".
//
// WHAT IS NOT MODELED (deliberate scope boundaries; the design's §2e
// table carries the full list with named alternative coverage)
//   - Store-side walk implementation (ingest, singleflight,
//     placeholders, probe caps); the walk verdict abstraction is the
//     boundary.
//   - Retry fold internals, budgets and verdict arithmetic
//     (retryPolicy.qnt owns them); poison/requeue arrivals are
//     uninterpreted environment actions that never touch the bits.
//   - Executor/pull protocol internals (executorSession.qnt owns them);
//     the admit_pull reduction here is refuse-iff-Ready-and-
//     must-substitute, mint otherwise.
//   - Gateway wanted-output / explicitly-requested computation: the
//     wanted and explicit sets are submission inputs.
//   - The CA realisation lane, build-completion accounting, spawn-intent
//     churn, intra-recovery read skew (D9), tenant multiplicity.
//   - persist_status statements are modeled synchronously with the
//     in-memory transition (not as intents): no checked property reads
//     the status-lag window, and the evidence bits — the subject — keep
//     the full intent treatment. Recorded in the invariant map's stage
//     record as encoding decision ENC-0b-2.
//
// INVARIANTS AND WITNESSES: defined in section 6 below, named exactly
// as in the design's §3 property tables (A1..A22, B1..B10, C1..C5,
// L1..L3). Pre-registered expected-fail probes (A17, A18, B2-strong,
// C1-strict, L2) are encoded as named predicates but are NOT part of
// any exported conjunction — Phase 0c wires them as expect-violation
// witnesses. Non-vacuity witnesses are the canReach* family.
// Weakened-test procedure (Phase 0d): each calibration override in
// docs/spec/models/calibration/closure-*.qnt reverts one production
// guard via a calibStep and must falsify the property named in the
// design's §4 crosswalk while the unmodified baseline holds it at the
// same constants.
//
// PHASE D-PRIME STATUS (substitution-replacement, the deletion phase):
// the verification subject above is the PREDECESSOR system. Waves D3-D6
// deleted the walk machinery, the Substituting status, the coexistence
// flag and both evidence columns from production (migration 080); the
// store-owned materialization job (materializationJob.qnt) is the only
// substitution mechanism. This file's full-alphabet text is retained
// UNCHANGED as the predecessor record (archive is stage A6's move, after
// the survivors core has soaked one integration); the live wired oracle
// is the closureEvidenceSurvivors module at the end of this file — the
// six surviving invariants (A14/A15/A22/B9/B10/L3) over the restricted
// post-deletion action alphabet — plus the two kept calibration pins
// (closure-f1-stale-produced, closure-f4-demand-drop). The 27 retired
// walk-era checks' dispositions: the T-D7.1 commit body and the Phase
// D-prime stage record in substitution-replacement-invariant-map.md.
//
// Measured state counts, depths and wall-clocks live in the introducing
// commits' messages and the invariant map's stage records, never here.
module closureEvidence {
  // --- 1. types ---------------------------------------------------------

  /// In-memory derivation status (the scheduler actor's live DAG view).
  /// MAbsent   = not in the live DAG (never merged, reaped, or removed).
  /// MQueued   = waiting on dependencies (Created/Queued collapsed).
  /// MReady    = dispatchable now.
  /// MSubstituting = a substitution walk is (believed) in flight.
  /// MOpen     = a from-source attempt is open (Assigned/Running).
  /// MFailed   = failed, awaiting requeue (retry backoff).
  /// MPoisoned / MDepFailed / MCancelled = un-produced terminal.
  /// MProduced = terminal success (Completed/Skipped collapsed).
  type MSt =
    | MAbsent
    | MQueued
    | MReady
    | MSubstituting
    | MOpen
    | MFailed
    | MPoisoned
    | MDepFailed
    | MCancelled
    | MProduced

  /// The single classifier (dag `closure_evidence`).
  type Ev = EvVouched | EvPending | EvBroken

  /// Dominant verdict of the last consumed store probe for a node
  /// (feeds B3/B8: what kind of evidence routed the node).
  type PK = PKNone | PKPresent | PKSubst | PKIndet | PKMissing

  /// Probe-cache entry for one output (per-tenant collapsed to one).
  type CacheV = CNone | CHit | CMiss

  /// Result slot of a walk instance.
  type WalkRes = WPending | WDone({ ok: bool, forgiven: Set[int], genuine: bool })

  /// One physical walk instance: the spawn-time forgivable snapshot,
  /// the spawning tenure, and the (eventually) delivered result.
  type WalkRec = { forgivable: Set[int], tenure: int, res: WalkRes }

  /// Build phase. BpActive covers pending+active (the rollback split is
  /// absorbed by mergeRollback being a no-op).
  type BPhase = BpNone | BpActive | BpDoneOk | BpFailed | BpCancelled

  /// Per-derivation record: the in-memory view (discarded wholesale at
  /// failover) and the durable row.
  type DrvRec = {
    // in-memory
    mSt: MSt,
    mTp: bool,
    mCh: bool,
    tried: bool,
    triedGenuine: bool,
    nf: Set[int],              // never_forgive_paths (chain-scoped)
    probed: bool,
    probedKind: PK,
    walks: List[WalkRec],
    // durable row
    rowExists: bool,
    pgSt: MSt,
    pgTp: bool,
    pgCh: bool,
    pgTpGen: int,              // tenure that last set the durable mark
    pgChGen: int,              // tenure that last set the durable hole
    pgWanted: Set[int],        // stored wanted union (resolved, grows only)
    // attempts
    attemptOpen: bool,
    attemptCycles: int,
    // ceilings / permissiveness bookkeeping
    walkConsumed: int,
    wrongFF: int,              // wrongful fail-fasts since last arming (stamp OR recovery restore)
    wrongFFStrict: int         // wrongful fail-fasts since last stamp only (C1-strict probe)
  }

  /// Per-output store state (output identity = (drv, index)).
  type OutRec = {
    present: bool,
    everPresent: bool,
    pinned: bool,
    upHas: bool,
    upFlipped: bool,           // <=1 upstream change per output (ceiling)
    ingested: bool,            // <=1 non-walk ingest per output (ceiling)
    cache: CacheV
  }

  /// Per-build record. interest/contrib are in-memory (contrib lost at
  /// failover); pgRow/pgLinks are the durable builds row + build_derivations.
  type BuildRec = {
    phase: BPhase,
    interest: Set[str],
    contrib: str -> Set[int],
    pgRow: bool,
    pgLinks: Set[str],
    cleanup: bool              // terminal-cleanup pending (reap not yet run)
  }

  /// The all-or-nothing merge transaction payload.
  type MergeIntent = { bld: str, rows: Set[str], stamps: Set[str], pruned: bool, narrow: bool }

  /// One tenure-tagged PG write intent. Batched production statements
  /// are decomposed per-row (encoding decision ENC-0b-3).
  type IntentOp =
    | IClearBoth(str)          // W2 clear_topdown_pruned_by_hashes (both bits)
    | IClearMark(str)          // W3 clear_topdown_pruned_by_hash (mark only)
    | ISetHole(str)            // W4 set_closure_hole_by_hashes
    | IHeal(str)               // W5 clear_closure_hole_by_hashes
    | IMerge(MergeIntent)      // W1 + edges + links + activation (one tx)
    | INop                     // sentinel for total nondet picks
  type IntentRec = { op: IntentOp, tenure: int }

  /// Leader / tenure state.
  type LeadRec = { up: bool, gen: int, recovered: bool }

  /// Trace-level event budgets (ceilings as action preconditions).
  type BudgetRec = {
    subs: int, pgDrops: int, failovers: int,
    storeGc: int, rowGc: int, purges: int,
    poisons: int, cancels: int
  }

  /// Cross-action "never happens" latches. Each is set only by behavior
  /// the production guard forbids; calibration overrides revert one
  /// guard and expect the matching invariant to falsify.
  type Viol = {
    doomedDispatch: bool,          // A1: from-source attempt opened on a marked Broken node
    stampUnsound: bool,            // A2
    clearUnsound: bool,            // A3
    holeUnsound: bool,             // A4
    holeMissed: bool,              // A5
    healUnsound: bool,             // A6
    pgBitLowered: bool,            // A7
    brokenVouched: bool,           // A8
    ffBadEffect: bool,             // A9
    mintedWhileMustSub: bool,      // A11
    downgradeNoGrowth: bool,       // A12
    stampWithoutActivation: bool,  // A13
    terminalOverwritten: bool,     // A14
    readyAboveFailedDepAtRecovery: bool, // A15 (durable-relation half, evaluated at Recover)
    dispatchNoInterest: bool,      // A16
    staleClearOverride: bool,      // A17 (expected-fail probe, stale-tenure)
    staleEvidenceApply: bool,      // A18 (expected-fail probe, stale-tenure)
    recoveryClearMissed: bool,     // A19
    healMissed: bool,              // A20
    condemnUnscoped: bool,         // A22
    restoredBitDropped: bool,      // A10
    producedWithoutPresence: bool, // B1
    walkOkClosureNeverPresent: bool, // B2 (weak form)
    walkOkClosureGoneAtConsume: bool, // B2-strong (expected-fail probe, adversarial-store)
    indetDemoted: bool,            // B3
    vacuousWantedVerdict: bool,    // B4
    wantedShrunk: bool,            // B5
    rollbackMutated: bool,         // B6
    pruneWeakerThanClassify: bool, // B7
    dispatchUnprobed: bool,        // B8
    staleProducedUnlocked: bool,   // B9
    demandDropped: bool,           // B10
    wrongfulDemotion: bool,        // C2
    wrongfulTerminalFailure: bool, // C3
    queuedWhilePresent: bool,      // C4
    unionDrivenDecision: bool      // C5
  }

  /// Reachability flags feeding the non-vacuity witnesses.
  type Reach = {
    stampFired: bool,
    holeReap: bool,
    holeAdmin: bool,
    holeTtl: bool,
    holeRecovery: bool,
    failFast: bool,
    recoveryClear: bool,
    walkOkConsumed: bool,
    walkFailConsumed: bool,
    walkOkWithForgiven: bool,
    staleIntentApplied: bool,
    triedDemotionUpstreamHas: bool,
    pruneFired: bool,
    rollbackSeen: bool,
    crossTenureWalkConsumed: bool,
    downgradeRespawned: bool,
    // Phase 1 (the settlement): a node's SECOND walk completion was
    // consumed (walkConsumed >= 1 at consumption time). Feeds the
    // WALK_CONSUME_CEIL headroom witness — the settlement spawns one
    // verification walk per arming on top of the original, so its
    // completion needs a ceiling of at least 2 (review finding FC-5).
    secondWalkConsumed: bool,
    // Phase 1 (the fence): a stale (deposed-tenure) evidence intent
    // reached the apply site and was DISCARDED by the claims-floor
    // fence. Feeds the fence's non-vacuity witness — proof that the
    // stale-apply window still arises and the fence (not chance) is
    // what keeps A17/A18 holding.
    fencedDiscard: bool
  }

  // --- 2. constants (const = overridable per regime) ----------------------

  const DRVS: Set[str]
  const BUILDS: Set[str]
  /// The universe reference graph submissions and walk closures are
  /// drawn from (a submission DAG is the REFS-closure of its roots).
  const REFS: Set[(str, str)]
  const SUBMISSION_BUDGET: int   // merges + the directed resubmit
  const FAILOVERS: int           // LeaderLost events
  const PGDROP_BUDGET: int       // best-effort intent losses
  const STALE_APPLY: bool        // deposed-believer intents survive recovery
  const CROSS_TENURE_WALKS: bool // a later tenure may consume an older walk
  const STORE_GC_BUDGET: int     // unpinned-output GC events
  const ROW_GC_BUDGET: int       // orphan-terminal row GC events
  const PURGE_BUDGET: int        // external builds-row purges
  const POISON_BUDGET: int       // inbound poison verdicts
  const CANCEL_BUDGET: int       // build cancellations
  /// Walk completions consumed per node. Phase-1 sizing (review finding
  /// FC-5): the settlement spawns one verification walk per arming ON TOP
  /// of the original walk, so every regime used for a post-fix holds
  /// verdict needs a ceiling of at least 2 (original + verification); the
  /// reduced exhaustive / probe regimes use 3 to also admit one downgrade
  /// re-spawn. A ceiling of 1 makes the settlement's completion
  /// unreachable (holds checks go vacuous) -- the
  /// canReachVerificationWalkConsumed witness pins the headroom.
  const WALK_CONSUME_CEIL: int
  const ATTEMPT_CYCLE_CEIL: int  // attempt open/close cycles per node
  const INTENT_CEIL: int         // pending unapplied intents at any instant
  /// Reduction-ladder step (4) (design §2b.1; invariant-map Phase 0c stage
  /// record): outstanding walk instances per node. 2 in the regimes that
  /// exercise the dual-walk rows (stale-tenure / adversarial-store:
  /// an undelivered older-tenure walk next to a fresh spawn); 1 elsewhere.
  const WALKS_PER_NODE: int
  /// Reduction-ladder step (2) (design §2b.1): per-output vs per-derivation
  /// store/upstream granularity for the ENVIRONMENT letters. When false,
  /// storeIngestAt / upstreamChangeAt act on every output of the touched
  /// node together (per-derivation store presence); per-output divergence
  /// still arises where the families need it — walk-ok ingests skip the
  /// forgiven outputs and attempt success ingests only the built node.
  /// True in the regimes whose families read individual output paths
  /// (stale-tenure / adversarial-store, the GC-after-vouch direction).
  const STORE_PER_OUTPUT: bool

  // --- 3. pure defs -------------------------------------------------------

  /// Output indices per node: 1 = the always-wanted output, 2 = the
  /// possibly-unwanted (forgivable) one.
  pure val OUTIDX: Set[int] = Set(1, 2)

  pure def freshDrv: DrvRec = {
    mSt: MAbsent, mTp: false, mCh: false,
    tried: false, triedGenuine: false, nf: Set(),
    probed: false, probedKind: PKNone, walks: [],
    rowExists: false, pgSt: MQueued, pgTp: false, pgCh: false,
    pgTpGen: 0, pgChGen: 0, pgWanted: Set(),
    attemptOpen: false, attemptCycles: 0,
    walkConsumed: 0, wrongFF: 0, wrongFFStrict: 0
  }

  pure def freshBuild(ds: Set[str]): BuildRec = {
    phase: BpNone, interest: Set(), contrib: ds.mapBy(d => Set()),
    pgRow: false, pgLinks: Set(), cleanup: false
  }

  pure def noViol: Viol = {
    doomedDispatch: false, stampUnsound: false, clearUnsound: false,
    holeUnsound: false, holeMissed: false, healUnsound: false,
    pgBitLowered: false, brokenVouched: false, ffBadEffect: false,
    mintedWhileMustSub: false, downgradeNoGrowth: false,
    stampWithoutActivation: false, terminalOverwritten: false,
    readyAboveFailedDepAtRecovery: false,
    dispatchNoInterest: false, staleClearOverride: false,
    staleEvidenceApply: false, recoveryClearMissed: false,
    healMissed: false, condemnUnscoped: false, restoredBitDropped: false,
    producedWithoutPresence: false, walkOkClosureNeverPresent: false,
    walkOkClosureGoneAtConsume: false, indetDemoted: false,
    vacuousWantedVerdict: false, wantedShrunk: false,
    rollbackMutated: false, pruneWeakerThanClassify: false,
    dispatchUnprobed: false, staleProducedUnlocked: false,
    demandDropped: false, wrongfulDemotion: false,
    wrongfulTerminalFailure: false, queuedWhilePresent: false,
    unionDrivenDecision: false
  }

  pure def noReach: Reach = {
    stampFired: false, holeReap: false, holeAdmin: false, holeTtl: false,
    holeRecovery: false, failFast: false, recoveryClear: false,
    walkOkConsumed: false, walkFailConsumed: false,
    walkOkWithForgiven: false, staleIntentApplied: false,
    triedDemotionUpstreamHas: false, pruneFired: false,
    rollbackSeen: false, crossTenureWalkConsumed: false,
    downgradeRespawned: false, secondWalkConsumed: false,
    fencedDiscard: false
  }

  pure val noopIntent: IntentRec = { op: INop, tenure: 0 }


  /// Is a status un-produced terminal (the truncation producers)?
  pure def isUnprodTerminal(s: MSt): bool =
    s == MPoisoned or s == MDepFailed or s == MCancelled

  /// Is a status terminal at all?
  pure def isTerminal(s: MSt): bool =
    s == MProduced or isUnprodTerminal(s)

  /// REFS-closure of a root set (bounded iteration; the universe is tiny).
  pure def pClosure(refs: Set[(str, str)], roots: Set[str]): Set[str] =
    0.to(4).fold(roots, (acc, _i) =>
      acc.union(refs.filter(e => acc.contains(e._1)).map(e => e._2)))

  /// Children of d in an edge relation.
  pure def pKids(edges: Set[(str, str)], d: str): Set[str] =
    edges.filter(e => e._1 == d).map(e => e._2)

  /// Parents of d in an edge relation.
  pure def pParents(edges: Set[(str, str)], d: str): Set[str] =
    edges.filter(e => e._2 == d).map(e => e._1)

  /// The single classifier over the in-memory view (dag closure_evidence):
  /// Broken if absent, holed, or childless; Vouched if every live child
  /// is Produced; Pending otherwise.
  pure def pEvidence(dm: str -> DrvRec, memE: Set[(str, str)], d: str): Ev = {
    val r = dm.get(d)
    if (r.mSt == MAbsent) EvBroken
    else if (r.mCh) EvBroken
    else {
      val kids = pKids(memE, d)
      if (kids == Set()) EvBroken
      else if (kids.forall(c => dm.get(c).mSt == MProduced)) EvVouched
      else EvPending
    }
  }

  pure def pVouched(dm: str -> DrvRec, memE: Set[(str, str)], d: str): bool =
    pEvidence(dm, memE, d) == EvVouched

  /// Total walk-list access (avoids out-of-bounds UB in disabled branches).
  pure def pWalkAt(ws: List[WalkRec], i: int): WalkRec =
    if (i >= 0 and i < ws.length()) ws[i]
    else { forgivable: Set(), tenure: 0, res: WPending }

  pure def pMustSub(dm: str -> DrvRec, memE: Set[(str, str)], d: str): bool =
    dm.get(d).mTp and pEvidence(dm, memE, d) == EvBroken

  /// Live interest: some active build holds d in its in-memory member set.
  pure def pLiveInterest(bm: str -> BuildRec, bs: Set[str], d: str): bool =
    bs.exists(b => bm.get(b).phase == BpActive and bm.get(b).interest.contains(d))

  /// The live effective wanted set: union of live builds' contributions
  /// when any exist, else the stored union, else all declared outputs.
  pure def pEffWanted(dm: str -> DrvRec, bm: str -> BuildRec, bs: Set[str], d: str): Set[int] = {
    val lc = bs.fold(Set(), (acc, b) =>
      if (bm.get(b).phase == BpActive and bm.get(b).interest.contains(d))
        acc.union(bm.get(b).contrib.get(d))
      else acc)
    if (lc != Set()) lc
    else if (dm.get(d).rowExists and dm.get(d).pgWanted != Set()) dm.get(d).pgWanted
    else OUTIDX
  }

  /// Every effective-wanted output of d is present in the store.
  pure def pAllWantedPresent(dm: str -> DrvRec, bm: str -> BuildRec, bs: Set[str],
                             stor: (str, int) -> OutRec, d: str): bool =
    pEffWanted(dm, bm, bs, d).forall(i => stor.get((d, i)).present)

  /// Every effective-wanted output of d is present or available upstream
  /// (the ground truth the wrongful-* definitions quantify over).
  pure def pAllWantedPresentOrUpstream(dm: str -> DrvRec, bm: str -> BuildRec, bs: Set[str],
                                       stor: (str, int) -> OutRec, d: str): bool =
    pEffWanted(dm, bm, bs, d).forall(i =>
      stor.get((d, i)).present or stor.get((d, i)).upHas)

  /// Substitutable answer available for a missing output (cache hit or
  /// live upstream); MissingConfirmed answer available (cache miss or
  /// upstream lacks).
  pure def pSubAnswerable(stor: (str, int) -> OutRec, o: (str, int)): bool =
    not(stor.get(o).present) and (stor.get(o).cache == CHit or stor.get(o).upHas)
  pure def pMissAnswerable(stor: (str, int) -> OutRec, o: (str, int)): bool =
    not(stor.get(o).present) and (stor.get(o).cache == CMiss or not(stor.get(o).upHas))

  /// Revert target after a failed/downgraded walk: DepFailed if a live
  /// child is un-produced terminal, Ready if every live child is
  /// produced (or there are none), else Queued.
  pure def pRevertTarget(dm: str -> DrvRec, memE: Set[(str, str)], d: str): MSt = {
    val kids = pKids(memE, d)
    if (kids.exists(c => isUnprodTerminal(dm.get(c).mSt))) MDepFailed
    else if (kids.forall(c => dm.get(c).mSt == MProduced)) MReady
    else MQueued
  }

  /// The strict PG criterion of the recovery clear gate
  /// (load_parents_with_all_children_produced): at least one durable
  /// child, every durable child Produced AND co-owned with d by a live
  /// build. Childless-in-PG nodes never satisfy it.
  pure def pStrictPgCriterion(dm: str -> DrvRec, bm: str -> BuildRec, bs: Set[str],
                              pgE: Set[(str, str)], d: str): bool = {
    val kids = pKids(pgE, d).filter(c => dm.get(c).rowExists)
    kids != Set() and kids.forall(c =>
      dm.get(c).pgSt == MProduced and
      bs.exists(b =>
        bm.get(b).phase == BpActive and bm.get(b).pgRow and
        bm.get(b).pgLinks.contains(d) and bm.get(b).pgLinks.contains(c)))
  }

  /// The recovery hole-stamp criterion
  /// (load_parents_with_unproduced_terminal_children): some durable
  /// child is un-produced terminal. Deliberately no live-build scoping.
  pure def pRecoveryHoleCriterion(dm: str -> DrvRec, pgE: Set[(str, str)], d: str): bool =
    pKids(pgE, d).exists(c => dm.get(c).rowExists and isUnprodTerminal(dm.get(c).pgSt))

  /// The recovery condemn criterion (load_parents_with_failed_deps):
  /// some durable un-produced-terminal child co-owned with d by a live
  /// build.
  pure def pCondemnCriterion(dm: str -> DrvRec, bm: str -> BuildRec, bs: Set[str],
                             pgE: Set[(str, str)], d: str): bool =
    pKids(pgE, d).exists(c =>
      dm.get(c).rowExists and isUnprodTerminal(dm.get(c).pgSt) and
      bs.exists(b =>
        bm.get(b).phase == BpActive and bm.get(b).pgRow and
        bm.get(b).pgLinks.contains(d) and bm.get(b).pgLinks.contains(c)))

  /// The in-DAG recompute's condemnation criterion (compute_initial_states
  /// → any_co_owned_dep_terminally_failed) as of the Phase-1 Wave-2b
  /// co-ownership fix: some LOADED un-produced-terminal child co-owned
  /// with d by a live build. Production scopes by intersection of the
  /// nodes' in-memory `interested_builds`; at recovery those sets ARE the
  /// durable links of live builds (the bd_rows rebuild), so the
  /// durable-link form here is the same predicate. Over the loaded
  /// relation this is subsumed by pCondemnCriterion (a loaded
  /// terminal-state child is a within-TTL poisoned row: rowExists, pgSt
  /// poisoned, its loaded edge ⊆ pgEdges) — kept as its own named
  /// criterion so the two production mechanisms stay separately visible
  /// and separately falsifiable by calibration overrides.
  pure def pInDagCondemnCriterion(dm: str -> DrvRec, bm: str -> BuildRec, bs: Set[str],
                                  memE: Set[(str, str)], d: str): bool =
    pKids(memE, d).exists(c =>
      isUnprodTerminal(dm.get(c).mSt) and
      bs.exists(b =>
        bm.get(b).phase == BpActive and bm.get(b).pgRow and
        bm.get(b).pgLinks.contains(d) and bm.get(b).pgLinks.contains(c)))

  /// Promote-and-clear cascade after a set of nodes became Produced:
  /// for every in-memory parent of a newly produced node, promote a
  /// vacuously-satisfied Queued parent to Ready and (the C2 clear) drop
  /// both bits off a marked parent that is now Vouched. Returns the new
  /// drv map and the set of parents whose bits were cleared (the caller
  /// emits the W2 intents).
  pure def pPromoteAndClear(dm: str -> DrvRec, memE: Set[(str, str)], ds: Set[str],
                            produced: Set[str]): { dm: str -> DrvRec, cleared: Set[str] } = {
    val parents = produced.fold(Set(), (acc, c) => acc.union(pParents(memE, c)))
    parents.fold({ dm: dm, cleared: Set() }, (st, p) => {
      val r = st.dm.get(p)
      if (r.mSt == MAbsent) st
      else {
        val kidsProduced = pKids(memE, p).forall(c => st.dm.get(c).mSt == MProduced)
        val promoted = if (r.mSt == MQueued and kidsProduced) { ...r, mSt: MReady } else r
        val doClear = promoted.mTp and kidsProduced and not(promoted.mCh) and pKids(memE, p) != Set()
        if (doClear)
          { dm: st.dm.set(p, { ...promoted, mTp: false, mCh: false, pgSt: promoted.pgSt }),
            cleared: st.cleared.union(Set(p)) }
        else
          { dm: st.dm.set(p, promoted), cleared: st.cleared }
      }
    })
  }

  /// The fail-fast effect (fail_fast_topdown_pruned_root) applied to one
  /// node over explicit maps (so the reap can run it on its intermediate
  /// state): park + cancel -> the node ends DepFailed, substitute_tried
  /// set, never_forgive cleared, the in-memory mark consumed, the hole
  /// retained, every then-interested live build terminally failed and its
  /// exclusively-owned un-produced members cancelled. Returns the new
  /// maps, the W3 emission, and whether the fire was wrongful (every
  /// effective-wanted output of every interested build present or
  /// available upstream at fire time).
  pure def pFailFast(dm: str -> DrvRec, bm: str -> BuildRec, bs: Set[str],
                     memE: Set[(str, str)], stor: (str, int) -> OutRec,
                     d: str): { dm: str -> DrvRec, bm: str -> BuildRec, wrongful: bool, fired: bool } = {
    val r = dm.get(d)
    // actionable guard: non-terminal, has live interest
    if (isTerminal(r.mSt) or r.mSt == MAbsent or not(pLiveInterest(bm, bs, d)))
      { dm: dm, bm: bm, wrongful: false, fired: false }
    else {
      val interested = bs.filter(b => bm.get(b).phase == BpActive and bm.get(b).interest.contains(d))
      // wrongful = every interested build's wanted outputs (over its members'
      // effective wanted sets) present-or-upstream at fire time
      val wrongful = interested != Set() and interested.forall(b =>
        bm.get(b).interest.forall(m => pAllWantedPresentOrUpstream(dm, bm, bs, stor, m)))
      // the node itself: tried, forgiveness cleared, mark consumed, hole kept,
      // parked then swept into DependencyFailed by the cancel pass
      val dm1 = dm.set(d, { ...r,
        mSt: MDepFailed, pgSt: if (r.rowExists) MDepFailed else r.pgSt,
        tried: true, triedGenuine: r.triedGenuine, nf: Set(),
        mTp: false,
        wrongFF: if (wrongful and r.wrongFF < 2) r.wrongFF + 1 else r.wrongFF,
        wrongFFStrict: if (wrongful and r.wrongFFStrict < 2) r.wrongFFStrict + 1 else r.wrongFFStrict })
      // cancel pass: exclusively-owned, un-produced members of the failed
      // builds go Cancelled; the builds terminally fail
      val dm2 = bs.fold(dm1, (acc, b) =>
        if (not(interested.contains(b))) acc
        else bm.get(b).interest.fold(acc, (acc2, m) =>
          if (m == d) acc2
          else {
            val mr = acc2.get(m)
            val exclusive = bs.forall(b2 => b2 == b or not(bm.get(b2).phase == BpActive and bm.get(b2).interest.contains(m)))
            if (exclusive and not(isTerminal(mr.mSt)) and mr.mSt != MAbsent)
              acc2.set(m, { ...mr, mSt: MCancelled, pgSt: if (mr.rowExists) MCancelled else mr.pgSt })
            else acc2
          }))
      val bm2 = bs.fold(bm, (acc, b) =>
        if (interested.contains(b)) acc.set(b, { ...acc.get(b), phase: BpFailed, cleanup: true }) else acc)
      { dm: dm2, bm: bm2, wrongful: wrongful, fired: true }
    }
  }

  // --- 4. state variables --------------------------------------------------

  var drv: str -> DrvRec
  var store: (str, int) -> OutRec
  var blds: str -> BuildRec
  var memEdges: Set[(str, str)]
  var pgEdges: Set[(str, str)]
  var intents: Set[IntentRec]
  var lead: LeadRec
  var budget: BudgetRec
  var viol: Viol
  var reach: Reach

  // --- 5. derived state ----------------------------------------------------

  pure val OUTIDXSET: Set[int] = Set(1, 2)
  def OUTS: Set[(str, int)] = tuples(DRVS, OUTIDX)

  def drvOf(d: str): DrvRec = drv.get(d)
  def evidence(d: str): Ev = pEvidence(drv, memEdges, d)
  def mustSub(d: str): bool = pMustSub(drv, memEdges, d)
  def vouchedNow(d: str): bool = pVouched(drv, memEdges, d)
  def liveInterest(d: str): bool = pLiveInterest(blds, BUILDS, d)
  def effWanted(d: str): Set[int] = pEffWanted(drv, blds, BUILDS, d)
  def allWantedPresent(d: str): bool = pAllWantedPresent(drv, blds, BUILDS, store, d)
  def allWantedPresentOrUpstream(d: str): bool = pAllWantedPresentOrUpstream(drv, blds, BUILDS, store, d)
  def memKids(d: str): Set[str] = pKids(memEdges, d)
  def memParentsOf(d: str): Set[str] = pParents(memEdges, d)
  def strictPgCriterion(d: str): bool = pStrictPgCriterion(drv, blds, BUILDS, pgEdges, d)
  def leaderActive: bool = lead.up and lead.recovered
  def anyStoreFault: bool = budget.storeGc < STORE_GC_BUDGET
  def anyUpstreamFlip: bool = DRVS.exists(d => OUTIDX.exists(i => store.get((d, i)).upFlipped))
  /// A store regression in the C3 sense: an upstream withdrawal (a
  /// publication is an improvement, not a regression).
  def anyUpstreamWithdrawal: bool = DRVS.exists(d => OUTIDX.exists(i =>
    store.get((d, i)).upFlipped and not(store.get((d, i)).upHas)))
  def anyFailover: bool = lead.gen > 1

  /// The D16 limbo cell: marked, Broken, tried, Ready, live interest,
  /// every effective-wanted output present, no walk in flight. As built
  /// the dispatch probe takes no action, the pull is refused, and no
  /// fail-fast arm is enabled — nothing settles it (L2's subject).
  def d16Cell(d: str): bool = and {
    drvOf(d).mTp,
    evidence(d) == EvBroken,
    drvOf(d).tried,
    drvOf(d).mSt == MReady,
    liveInterest(d),
    allWantedPresent(d),
    drvOf(d).walks == []
  }

  /// A wrongful-fail-fast trigger state: marked Broken Ready node with a
  /// fail-fast arm enabled while every effective-wanted output is
  /// present or available upstream (C-group non-vacuity).
  def wrongfulFfTrigger(d: str): bool = and {
    drvOf(d).mTp,
    evidence(d) == EvBroken,
    drvOf(d).mSt == MReady,
    liveInterest(d),
    allWantedPresentOrUpstream(d),
    or {
      effWanted(d).exists(i => pMissAnswerable(store, (d, i))),
      drvOf(d).tried and effWanted(d).exists(i => not(store.get((d, i)).present))
    }
  }

  /// Settlement-arm availability guards (Phase 1 — the C3+D16 settlement
  /// fix). Shared by the settling actions (probeSettleTried, the
  /// consumeWalk Broken-arm settlement, the reap-survivor settlement) AND
  /// the L2 armed form below, so the invariant and the action guards
  /// cannot silently diverge: an edit that re-narrows an action's
  /// availability arm re-narrows the partition the invariant checks, and
  /// the armed form goes red as soon as a checker reaches a cell inside
  /// the re-opened hole.
  ///
  /// settleArmAvailable: every live effective wanted output is obtainable
  /// — present locally or available upstream. The model form of the
  /// code's present-or-substitutable-or-indeterminate FMP partition (an
  /// indeterminate answer counts toward the settlement arm, never the
  /// fail-fast arm — B3; at this model's store abstraction a missing
  /// output with no upstream is always confirm-answerable, so the
  /// indeterminate letter collapses into these two arms). The settling
  /// action spawns the closure-re-verifying substitution walk.
  def settleArmAvailable(d: str): bool = allWantedPresentOrUpstream(d)

  /// failFastArmConfirmedMissing: some live effective wanted output is
  /// confirmed unobtainable (missing locally AND absent upstream) — the
  /// narrowed probeFailFastTried criterion, the genuine fail-fast arm.
  def failFastArmConfirmedMissing(d: str): bool = not(allWantedPresentOrUpstream(d))

  /// The settlement partition over a tried marked-Broken cell: at least
  /// one settling arm covers it. Post-fix the two arms partition
  /// availability totally; pre-fix the fail-fast arm alone existed and
  /// was keyed on LOCAL presence only (not(allWantedPresent)), which
  /// left the all-present cell covered by nothing — the D16 limbo. The
  /// pre-fix coverage is frozen as settlementEnabledPreFix in the
  /// closure-c3-no-reprobe calibration override.
  def settlementEnabled(d: str): bool = settleArmAvailable(d) or failFastArmConfirmedMissing(d)

  /// Progress armed for a member of a live build (L3's disjunction).
  /// The D16 cell is deliberately counted as armed here: that cell is
  /// L2's subject and is carved out of L3 so the two probes stay
  /// independent (encoding decision ENC-0b-6).
  def progressArmed(d: str): bool = or {
    drvOf(d).walks != [],
    drvOf(d).attemptOpen,
    drvOf(d).mSt == MFailed,
    isUnprodTerminal(drvOf(d).mSt),
    drvOf(d).mSt == MReady and not(mustSub(d)),
    drvOf(d).mSt == MReady and mustSub(d) and not(drvOf(d).tried),
    drvOf(d).mSt == MReady and mustSub(d) and drvOf(d).tried and not(allWantedPresent(d)),
    drvOf(d).mSt == MReady and drvOf(d).tried and allWantedPresent(d),
    drvOf(d).mSt == MQueued and memKids(d).exists(c => drvOf(c).mSt != MProduced)
  }

  // --- 6. invariants -------------------------------------------------------
  // Group A — safety (design §3a). Latch-backed properties hold because
  // the production guard exists at every site; the latch is the oracle a
  // calibration override falsifies.

  /// A1: a marked node with Broken evidence is never opened from source.
  val noDoomedFromSourceDispatch = not(viol.doomedDispatch)

  /// A2: the durable mark is set only by a committed pruned merge for a
  /// kept node whose submitted closure was dropped and that was not
  /// Vouched at stamp time.
  val stampSoundness = not(viol.stampUnsound)

  /// A3: the mark is cleared only on Vouched evidence, the strict PG
  /// criterion at recovery, or the fail-fast consume.
  val clearSoundness = not(viol.clearUnsound)

  /// A4: the hole is set only when an un-produced child was removed from
  /// a surviving parent.
  val holeSoundness = not(viol.holeUnsound)

  /// A5: every leader-side removal of an un-produced child stamps the
  /// surviving parents in the same step (modulo the named residuals).
  val holeCompleteness = not(viol.holeMissed)

  /// A6: the hole is cleared only by a full-merge heal, a Vouched-keyed
  /// both-bits clear, or removal of the node itself.
  val healSoundness = not(viol.healUnsound)

  /// A7: no merge upsert or status write ever lowers a durable bit.
  val pgMonotoneUpsert = not(viol.pgBitLowered)

  /// A8: a childless or holed child set never vouches.
  val brokenNeverVouches = not(viol.brokenVouched)

  /// A9: the fail-fast consumes the mark, keeps the hole, sets tried and
  /// terminally fails every then-interested build.
  val failFastConsumesMarkKeepsHole = not(viol.ffBadEffect)

  /// A10: durable bits true at recovery are carried into the recovered
  /// memory except via the strict-criterion clear or the poisoned-row
  /// carve-out.
  val failoverPreservation = not(viol.restoredBitDropped)

  /// A11: admit_pull never mints for a Ready must-substitute node.
  val pullRefusalNoMint = not(viol.mintedWhileMustSub)

  /// A12: every forgiven-now-wanted downgrade strictly grows the
  /// chain-scoped never-forgive set.
  val chainTermination = not(viol.downgradeNoGrowth)

  /// A13: merge stamps are visible iff the merge's activation landed
  /// (one all-or-nothing intent).
  val stampAtomicWithActivation = not(viol.stampWithoutActivation)

  /// A14: no terminal status is overwritten by a fail-fast or park.
  val terminalIsTerminal = not(viol.terminalOverwritten)

  /// A15: no node is seeded or promoted Ready above an un-produced
  /// declared dependency. The promotion-time half is by-construction
  /// (every promotion site — merge reconcile, completion cascade, reap
  /// promote arm — guards on the live children being produced); the
  /// checked half is the recovery-time gate over the DURABLE relation:
  /// recovery never seeds a node Ready above a durable un-produced-
  /// terminal child co-owned by a live build (the CE-47 direction;
  /// encoding decision ENC-0b-7 — a child later re-queued for another
  /// build's wider wanted set does not retroactively invalidate an
  /// already-computed readiness, because the dependent's inputs still
  /// exist).
  val readyImpliesDeclaredDepsProduced = not(viol.readyAboveFailedDepAtRecovery)

  /// A16: probe/dispatch/attempt actions fire only with live interest.
  val liveInterestRequiredForDispatch = not(viol.dispatchNoInterest)

  /// A17: a clear/heal intent from an older tenure never erases a bit a
  /// newer tenure set. Phase-1 flip (Wave 4): pre-fence this was an
  /// expected-fail probe (the stale-tenure regimes exhibited the
  /// unfenced-apply window); the claims-floor fence — pgApplyAny
  /// discards stale evidence intents instead of applying them, the
  /// model mirror of the Wave-3 production fence
  /// (sched.evidence.durability+2) — closes it, and the val now holds
  /// in every regime and sits in allInvariants/asBuiltHoldInvariants.
  /// Falsifiability is preserved by the closure-a17-unfenced
  /// calibration pin (frozen pre-fence apply).
  val noStaleTenureClearOverride = not(viol.staleClearOverride)

  /// A18: only the current tenure's evidence intents reach PG. Phase-1
  /// flip: same as A17 — the same fence closes both (the latches sit on
  /// the apply path, and fenced intents never reach it).
  val leaderClassEvidenceWrites = not(viol.staleEvidenceApply)

  /// A19: recovery clears every restored mark whose strict PG criterion
  /// holds.
  val recoveryClearCompleteness = not(viol.recoveryClearMissed)

  /// A20: a full merge heals every re-declared parent with a persisted
  /// hole, not keyed on the in-memory bit.
  val healCompleteness = not(viol.healMissed)

  /// A21: the chain-scoped forgiveness latch never outlives its chain —
  /// a non-empty never-forgive set implies a walk chain is in flight (or
  /// the node is terminal/absent, where the dead latch is unreadable and
  /// the resubmit-reset re-creates fresh state).
  val chainEndClearsForgiveness = DRVS.forall(d =>
    (drvOf(d).nf != Set()) implies or {
      drvOf(d).mSt == MSubstituting,
      isTerminal(drvOf(d).mSt),
      drvOf(d).mSt == MAbsent
    })

  /// A22: the recovery failed-dep CASCADE PRE-PASS (the
  /// load_parents_with_failed_deps short-circuit) condemns only on a
  /// live co-owned persisted failure. The compute_initial_states
  /// in-DAG recompute that runs after it is a separate production
  /// mechanism, ALSO co-ownership-scoped as of the Phase-1 Wave-2b fix
  /// (any_co_owned_dep_terminally_failed; recoverAsLeader's condemnation
  /// arm (b) / pInDagCondemnCriterion). The latch stays keyed on the
  /// cascade arm — the mechanism the spec rule names — and the in-DAG
  /// arm's scoping is pinned operationally: the Wave-2b L3 re-hunt's red
  /// half (scoping without the poisonClear survivor promotion) re-opens
  /// the strand, and the wired battery holds with both halves in.
  val condemnRequiresLiveCoOwnedFailure = not(viol.condemnUnscoped)

  // Group B — the missing families (design §3b).

  /// B1: a non-build Produced entry requires the live-wanted outputs
  /// present at decision time.
  val completedWithoutBuildImpliesWantedPresent = not(viol.producedWithoutPresence)

  /// B2 (weak form): a consumed ok walk implies the non-forgiven walked
  /// closure was present at some instant before consumption.
  val substituteOkImpliesClosureIngested = not(viol.walkOkClosureNeverPresent)

  /// B2-strong (expected-fail probe, adversarial-store): still present at
  /// consumption time. NOT in any exported conjunction.
  val substituteOkClosureStillPresentAtConsume = not(viol.walkOkClosureGoneAtConsume)

  /// B3: an Indeterminate / failed probe verdict never routes from
  /// source, never counts as missing for the prune, never fail-fasts.
  val unknownNeverDemotes = not(viol.indetDemoted)

  /// B4: an empty/unresolvable wanted set never satisfies an
  /// availability or forgiveness predicate.
  val noVacuousWantedVerdict = not(viol.vacuousWantedVerdict)

  /// B5: the stored wanted union only grows.
  val storedWantedMonotone = not(viol.wantedShrunk)

  /// B6: a rolled-back merge changes nothing.
  val rollbackRestoresWantedAndEvidence = not(viol.rollbackMutated)

  /// B7: the prune's availability criterion is at least as strict as the
  /// dispatch-time classification criterion.
  val pruneNotMorePermissiveThanClassification = not(viol.pruneWeakerThanClassify)

  /// B8: no from-source attempt without a probe verdict this pass.
  val dispatchImpliesProbedThisPass = not(viol.dispatchUnprobed)

  /// B9: a dependent never advances past a Produced child whose
  /// live-wanted outputs are absent (merge adoption / attempt open).
  val staleProducedNeverUnlocksDependents = not(viol.staleProducedUnlocked)

  /// B10: the prune never drops a demand-set member.
  val demandSetSurvivesPrune = not(viol.demandDropped)

  // Group C — permissiveness (design §3c).

  /// C1: at most one wrongful fail-fast per (node, arming).
  val wrongfulFailFastBoundedPerArming = DRVS.forall(d => drvOf(d).wrongFF <= 1)

  /// C1-strict (expected-fail probe, failover/fault-persist): the
  /// per-(node, stamp) form with no recovery re-arming. NOT in any
  /// exported conjunction.
  val wrongfulFailFastBoundedPerStamp = DRVS.forall(d => drvOf(d).wrongFFStrict <= 1)

  /// C2: no wrongful from-source demotion outside the genuine-walk-
  /// failure one-shot.
  val noWrongfulFromSourceDemotion = not(viol.wrongfulDemotion)

  /// C3: with no failover, no store GC and no upstream withdrawal, no
  /// build is wrongfully terminally failed.
  val noWrongfulTerminalFailureSingleTenure =
    (not(anyFailover) and not(anyStoreFault) and not(anyUpstreamWithdrawal))
      implies not(viol.wrongfulTerminalFailure)

  /// C4: a node whose live-wanted outputs are all present at merge is
  /// not queued for a from-source build.
  val noBuildWhenWantedPresent = not(viol.queuedWhilePresent)

  /// C5: only live builds' wanted outputs drive resets, forgiveness
  /// refusal and re-pinning.
  val terminalBuildStopsPinning = not(viol.unionDrivenDecision)

  // Group L — settlement, armed-state form (design §3d).

  /// L1: a Substituting node always has a walk instance in flight.
  val substitutingAlwaysArmed = DRVS.forall(d =>
    (drvOf(d).mSt == MSubstituting) implies (drvOf(d).walks != []))

  /// L2 (ARMED/settlement form — the Phase-1 restatement; the FB-1
  /// armed-invariant precedent and ENC-0b-6's carve-out discipline):
  /// every D16 limbo cell is covered by the settlement partition — some
  /// settling arm's availability guard holds for it, so the next
  /// dispatch sweep settles the cell instead of skipping it. Pre-fix the
  /// partition had a hole at exactly the cell (the tried fail-fast arm
  /// was keyed on local presence only and nothing else fired for a
  /// present cell), so the pre-fix form of this val
  /// (markedBrokenSettlementArmedPreFix in the closure-c3-no-reprobe
  /// calibration override) is violated wherever the cell is reachable;
  /// post-fix probeSettleTried's arm covers it and this val holds.
  ///
  /// Why not the 0b state form (not(exists d16Cell))? That form can
  /// never hold post-fix: the fixes make the cell a designed TRANSIENT —
  /// the reap / poison-clear promotion arms turn a marked+tried Queued
  /// survivor into the cell, and the next sweep's settlement consumes it
  /// — so state-unreachability is the wrong claim (review findings
  /// RT-3/MCI-1/FC-3). The armed form is the wireable claim; the cell's
  /// reachability (non-vacuity of this implication) is the
  /// canReachD16Cell witness, kept wired.
  val markedBrokenSettlementArmed = DRVS.forall(d => d16Cell(d) implies settlementEnabled(d))

  /// L3: every live build has a progress step armed (or is all-produced /
  /// has a terminal member that arms its failure path).
  val liveBuildTerminalOrProgressArmed = BUILDS.forall(b =>
    (blds.get(b).phase == BpActive and blds.get(b).interest != Set()) implies or {
      blds.get(b).interest.forall(m => drvOf(m).mSt == MProduced),
      blds.get(b).interest.exists(m => progressArmed(m))
    })

  /// Bounds discipline: ceilings hold (every ceiling is an action
  /// precondition; this doubles as the forgotten-precondition check).
  val boundsOK = and {
    DRVS.forall(d => and {
      drvOf(d).walks.length() <= 2,
      drvOf(d).walkConsumed <= WALK_CONSUME_CEIL,
      drvOf(d).attemptCycles <= ATTEMPT_CYCLE_CEIL,
      drvOf(d).wrongFF <= 2, drvOf(d).wrongFFStrict <= 2
    }),
    intents.size() <= INTENT_CEIL,
    lead.gen <= 1 + FAILOVERS,
    budget.subs >= 0, budget.pgDrops >= 0, budget.failovers >= 0,
    budget.storeGc >= 0, budget.rowGc >= 0, budget.purges >= 0,
    budget.poisons >= 0, budget.cancels >= 0
  }

  /// The conjunction the exhaustive regime targets verify. HISTORY: at
  /// Phase 0c this was allInvariants minus C3
  /// (noWrongfulTerminalFailureSingleTenure) — the as-built consumption
  /// path genuinely violated C3 (a stale walk verdict fail-fasting a
  /// build whose wanted outputs are present), so the conjunction had to
  /// exclude it to be a usable HOLD target while the defect was open.
  /// **As of Phase 1 (Wave 4) the exclusion is closed**: the settlement
  /// re-probe (probeSettleTried / the consumeWalk Broken-arm settlement /
  /// the reap-survivor settlement, mirroring the Wave-1 production fix at
  /// the same three sites) removes the wrongful fail-fast class, C3 holds
  /// as-built, and this conjunction is again the FULL invariant set —
  /// identical to allInvariants (C3, the L2 armed form, and the
  /// fence-flipped A17/A18 included). The name is kept because the
  /// 0c/0d/Phase-1 records and the manual-target commands in
  /// nix/quint.nix reference it; the falsifiability records are
  /// preserved by the closure-c3-no-reprobe and closure-a17-unfenced
  /// calibration pins (pre-fix actions, expect-violation).
  val asBuiltHoldInvariants = and {
    noDoomedFromSourceDispatch, stampSoundness, clearSoundness,
    holeSoundness, holeCompleteness, healSoundness, pgMonotoneUpsert,
    brokenNeverVouches, failFastConsumesMarkKeepsHole,
    failoverPreservation, pullRefusalNoMint, chainTermination,
    stampAtomicWithActivation, terminalIsTerminal,
    readyImpliesDeclaredDepsProduced, liveInterestRequiredForDispatch,
    recoveryClearCompleteness, healCompleteness,
    chainEndClearsForgiveness, condemnRequiresLiveCoOwnedFailure,
    completedWithoutBuildImpliesWantedPresent,
    substituteOkImpliesClosureIngested, unknownNeverDemotes,
    noVacuousWantedVerdict, storedWantedMonotone,
    rollbackRestoresWantedAndEvidence,
    pruneNotMorePermissiveThanClassification,
    dispatchImpliesProbedThisPass, staleProducedNeverUnlocksDependents,
    demandSetSurvivesPrune, wrongfulFailFastBoundedPerArming,
    noWrongfulFromSourceDemotion, noWrongfulTerminalFailureSingleTenure,
    noBuildWhenWantedPresent, terminalBuildStopsPinning,
    substitutingAlwaysArmed, liveBuildTerminalOrProgressArmed,
    markedBrokenSettlementArmed,
    noStaleTenureClearOverride, leaderClassEvidenceWrites,
    boundsOK
  }

  /// The adversarial-store-scope hold conjunction: asBuiltHoldInvariants
  /// minus B9 (staleProducedNeverUnlocksDependents) — the documented
  /// pre-registered as-built trip under store GC (GC-after-produce, the
  /// design §5 accepted bound; the 0b/0c records and the per-property
  /// table's B9 row). Every other conjunct must hold under the
  /// adversarial-store alphabet; this is the AdversarialStoreEx
  /// exhaustive measurement / manual-target invariant.
  val asBuiltHoldInvariantsAdversarialStore = and {
    noDoomedFromSourceDispatch, stampSoundness, clearSoundness,
    holeSoundness, holeCompleteness, healSoundness, pgMonotoneUpsert,
    brokenNeverVouches, failFastConsumesMarkKeepsHole,
    failoverPreservation, pullRefusalNoMint, chainTermination,
    stampAtomicWithActivation, terminalIsTerminal,
    readyImpliesDeclaredDepsProduced, liveInterestRequiredForDispatch,
    recoveryClearCompleteness, healCompleteness,
    chainEndClearsForgiveness, condemnRequiresLiveCoOwnedFailure,
    completedWithoutBuildImpliesWantedPresent,
    substituteOkImpliesClosureIngested, unknownNeverDemotes,
    noVacuousWantedVerdict, storedWantedMonotone,
    rollbackRestoresWantedAndEvidence,
    pruneNotMorePermissiveThanClassification,
    dispatchImpliesProbedThisPass,
    demandSetSurvivesPrune, wrongfulFailFastBoundedPerArming,
    noWrongfulFromSourceDemotion, noWrongfulTerminalFailureSingleTenure,
    noBuildWhenWantedPresent, terminalBuildStopsPinning,
    substitutingAlwaysArmed, liveBuildTerminalOrProgressArmed,
    markedBrokenSettlementArmed,
    noStaleTenureClearOverride, leaderClassEvidenceWrites,
    boundsOK
  }

  /// The failover-scope variant of asBuiltHoldInvariants: additionally
  /// excludes L3 (liveBuildTerminalOrProgressArmed). HISTORICAL: the
  /// post-0d triage classified an L3 violation at FailoverEx as a real
  /// as-built finding (failover + recovery + ClearPoison strands a
  /// recovered parent Queued/childless/unarmed under a live build).
  /// **REFUTED by the Phase-1 Wave-2 recovery-condemnation correction**:
  /// the traced strand depended on recovery leaving the parent Queued
  /// above its still-poisoned child, which the model permitted only
  /// through the missing compute_initial_states condemnation arm
  /// (recoverAsLeader's arm (b), above). Wave 2b then fixed production's
  /// unscoped form of that arm red-first (the residual finding): the
  /// condemnation is now co-ownership-scoped (pInDagCondemnCriterion) AND
  /// the poison-clear removal promotes the surviving parents
  /// (poisonClear's promotion arm) — the spared parent waits Queued
  /// (progress-armed: its poisoned child is non-produced) and is promoted
  /// at the clear, so the strand stays closed by the
  /// scoping+promotion pair where the unscoped condemnation used to close
  /// it alone. The Wave-2b L3 re-hunt at FailoverEx and Duo (both
  /// backends) finds no violation with both halves in, and the red half
  /// (scoping without promotion) re-finds the strand — the calibration
  /// that the pairing, not luck, closes it. asBuiltHoldInvariants (WITH
  /// L3) remains the expected-hold conjunction at the failover scopes;
  /// this variant is retained for comparison against the 0d record.
  /// Refutation evidence, re-hunt figures and the Wave-2b fix record:
  /// invariant map, Phase-1 Wave-2/2b stage records. The single-tenure
  /// scopes' 0c BaseEx record is unaffected.
  val asBuiltHoldInvariantsFailoverEx = and {
    noDoomedFromSourceDispatch, stampSoundness, clearSoundness,
    holeSoundness, holeCompleteness, healSoundness, pgMonotoneUpsert,
    brokenNeverVouches, failFastConsumesMarkKeepsHole,
    failoverPreservation, pullRefusalNoMint, chainTermination,
    stampAtomicWithActivation, terminalIsTerminal,
    readyImpliesDeclaredDepsProduced, liveInterestRequiredForDispatch,
    recoveryClearCompleteness, healCompleteness,
    chainEndClearsForgiveness, condemnRequiresLiveCoOwnedFailure,
    completedWithoutBuildImpliesWantedPresent,
    substituteOkImpliesClosureIngested, unknownNeverDemotes,
    noVacuousWantedVerdict, storedWantedMonotone,
    rollbackRestoresWantedAndEvidence,
    pruneNotMorePermissiveThanClassification,
    dispatchImpliesProbedThisPass, staleProducedNeverUnlocksDependents,
    demandSetSurvivesPrune, wrongfulFailFastBoundedPerArming,
    noWrongfulFromSourceDemotion,
    noBuildWhenWantedPresent, terminalBuildStopsPinning,
    substitutingAlwaysArmed,
    boundsOK
  }

  /// The default safety conjunction (everything except the
  /// pre-registered expected-fail probes that remain expected-fail —
  /// B2-strong/C1-strict). Phase 0c selects per-regime lists from
  /// the individual names; this conjunction is the base-regime check's
  /// invariant.
  ///
  /// Phase-1 (Wave 4) membership flips, each preserved as falsifiable by
  /// an expect-violation calibration pin:
  ///  - L2 (markedBrokenSettlementArmed, restated in the armed form)
  ///    joined when the settlement was encoded — pre-fix it was the
  ///    expected-fail D16 probe. C3 was already a member (its violation
  ///    was the documented as-built defect that kept
  ///    asBuiltHoldInvariants separate); with the settlement encoded it
  ///    holds. Pin: closure-c3-no-reprobe.
  ///  - A17/A18 (noStaleTenureClearOverride / leaderClassEvidenceWrites)
  ///    joined when the claims-floor fence was encoded — pre-fence they
  ///    were the expected-fail stale-tenure probes. Pin:
  ///    closure-a17-unfenced.
  val allInvariants = and {
    noDoomedFromSourceDispatch, stampSoundness, clearSoundness,
    holeSoundness, holeCompleteness, healSoundness, pgMonotoneUpsert,
    brokenNeverVouches, failFastConsumesMarkKeepsHole,
    failoverPreservation, pullRefusalNoMint, chainTermination,
    stampAtomicWithActivation, terminalIsTerminal,
    readyImpliesDeclaredDepsProduced, liveInterestRequiredForDispatch,
    recoveryClearCompleteness, healCompleteness,
    chainEndClearsForgiveness, condemnRequiresLiveCoOwnedFailure,
    completedWithoutBuildImpliesWantedPresent,
    substituteOkImpliesClosureIngested, unknownNeverDemotes,
    noVacuousWantedVerdict, storedWantedMonotone,
    rollbackRestoresWantedAndEvidence,
    pruneNotMorePermissiveThanClassification,
    dispatchImpliesProbedThisPass, staleProducedNeverUnlocksDependents,
    demandSetSurvivesPrune, wrongfulFailFastBoundedPerArming,
    noWrongfulFromSourceDemotion, noWrongfulTerminalFailureSingleTenure,
    noBuildWhenWantedPresent, terminalBuildStopsPinning,
    substitutingAlwaysArmed, liveBuildTerminalOrProgressArmed,
    markedBrokenSettlementArmed,
    noStaleTenureClearOverride, leaderClassEvidenceWrites,
    boundsOK
  }

  // --- 6b. expect-violation witnesses (non-vacuity probes) -----------------
  // Each states "the interesting state never happens"; the wired check
  // (Phase 0c) passes only when the checker violates it.

  /// A pruned merge stamped a node.
  val canReachStamp = not(reach.stampFired)
  /// The terminal-build reap stamped a hole (H1).
  val canReachHoleFromReap = not(reach.holeReap)
  /// Admin ClearPoison stamped a hole (H2).
  val canReachHoleFromAdminClear = not(reach.holeAdmin)
  /// The poison-TTL sweep stamped a hole (H3).
  val canReachHoleFromTtlSweep = not(reach.holeTtl)
  /// Recovery stamped a hole (H4).
  val canReachHoleFromRecovery = not(reach.holeRecovery)
  /// The fail-fast fired.
  val canReachFailFast = not(reach.failFast)
  /// The recovery clear gate cleared a restored mark (C3).
  val canReachRecoveryClear = not(reach.recoveryClear)
  /// An ok walk was consumed into Produced.
  val canReachWalkOkConsumed = not(reach.walkOkConsumed)
  /// A failed walk was consumed.
  val canReachWalkFailConsumed = not(reach.walkFailConsumed)
  /// An ok walk with a non-empty forgiven set was consumed (the CE-23
  /// accepted forgiveness residual is reachable).
  val canReachForgivenResidual = not(reach.walkOkWithForgiven)
  /// A stale-tenure intent applied after a newer tenure recovered.
  /// UNREACHABLE in the production model as of Phase 1: the claims-floor
  /// fence discards stale evidence intents before the apply, so this
  /// flag can no longer be set here. Kept (not deleted) because the
  /// closure-a17-unfenced calibration override's frozen pre-fence apply
  /// still sets it — the predicate remains the override's reachability
  /// oracle. The production-model replacement is canReachFencedDiscard
  /// below (the same window, observed at the fence instead of at the
  /// apply).
  val canReachStaleIntentApply = not(reach.staleIntentApplied)
  /// A stale (deposed-tenure) evidence intent reached the apply site and
  /// was DISCARDED by the claims-floor fence (Phase 1). The fence's
  /// non-vacuity witness: proof that the stale-apply window the pre-fix
  /// model exhibited (the A18 trace) still arises — deposed intents
  /// still survive into a successor's tenure in the stale-apply regimes
  /// — and that the fence, not unreachability, is what keeps A17/A18
  /// holding. Replaces canReachStaleIntentApply on the wired surface.
  val canReachFencedDiscard = not(reach.fencedDiscard)
  /// The D16 limbo cell is reachable. Pre-fix (Phase 0c): never produced
  /// within any hunted budget — the >=16-step assembly (deferred-probe
  /// record). Post-fix (Phase 1): reachable at shallow depth via the
  /// reap / poison-clear promotion transient — a marked+tried Queued
  /// survivor whose last child is removed is promoted to Ready and IS
  /// the cell until the next sweep's probeSettleTried consumes it. This
  /// witness is the proof that probeSettleTried is live code and that
  /// the L2 armed form's implication is exercised non-vacuously.
  val canReachD16Cell = not(DRVS.exists(d => d16Cell(d)))
  /// A second walk completion was consumed on the same node. This is the
  /// WALK_CONSUME_CEIL headroom pin (review finding FC-5): the Phase-1
  /// settlement spawns one verification walk per arming ON TOP of the
  /// original walk, so a regime used for a post-fix holds verdict needs
  /// a consumption ceiling of at least 2 — if a future edit drops a
  /// ceiling back to 1, the settlement's completion becomes unreachable
  /// (the holds checks go vacuous) and this witness catches it as a
  /// wired-check failure.
  val canReachVerificationWalkConsumed = not(reach.secondWalkConsumed)
  /// A wrongful-fail-fast trigger state is reachable.
  val canReachWrongfulFfTrigger = not(DRVS.exists(d => wrongfulFfTrigger(d)))
  /// A substitute_tried one-shot demotion was taken while upstream had
  /// the missing output.
  val canReachTriedDemotionUpstreamHas = not(reach.triedDemotionUpstreamHas)
  /// The prune fired.
  val canReachPrune = not(reach.pruneFired)
  /// A merge rollback happened.
  val canReachRollback = not(reach.rollbackSeen)
  /// A walk spawned by an older tenure was consumed by a newer one.
  val canReachCrossTenureWalkConsume = not(reach.crossTenureWalkConsumed)
  /// A forgiven-now-wanted downgrade re-spawned the walk.
  val canReachDowngradeRespawn = not(reach.downgradeRespawned)

  // --- 7. actions ----------------------------------------------------------

  // --- 7a. store / upstream environment ------------------------------------

  /// A non-walk ingest makes an output present (another tenant's build,
  /// a PutPath, an out-of-band substitution). Under per-derivation store
  /// granularity (reduction-ladder step (2), STORE_PER_OUTPUT = false) the
  /// ingest covers every still-absent output of the touched node.
  action storeIngestAt(o: (str, int)): bool = {
    val outs = if (STORE_PER_OUTPUT) Set(o) else OUTIDX.map(i => (o._1, i))
    val targets = outs.filter(oo => not(store.get(oo).present) and not(store.get(oo).ingested))
    all {
      targets != Set(),
      store' = targets.fold(store, (acc, oo) =>
        acc.set(oo, { ...acc.get(oo), present: true, everPresent: true, ingested: true })),
      drv' = drv, blds' = blds, memEdges' = memEdges, pgEdges' = pgEdges,
      intents' = intents, lead' = lead, budget' = budget, viol' = viol, reach' = reach
    }
  }

  /// Upstream availability flips (publication or deletion). The probe
  /// cache is deliberately NOT updated — that is the staleness window.
  /// Under per-derivation granularity the flip covers the whole node
  /// (its outputs' upstream availability never diverges in those regimes).
  action upstreamChangeAt(o: (str, int)): bool = {
    val outs = if (STORE_PER_OUTPUT) Set(o) else OUTIDX.map(i => (o._1, i))
    all {
      outs.forall(oo => not(store.get(oo).upFlipped)),
      store' = outs.fold(store, (acc, oo) =>
        acc.set(oo, { ...acc.get(oo), upHas: not(acc.get(oo).upHas), upFlipped: true })),
      drv' = drv, blds' = blds, memEdges' = memEdges, pgEdges' = pgEdges,
      intents' = intents, lead' = lead, budget' = budget, viol' = viol, reach' = reach
    }
  }

  /// Store GC removes an unpinned present output (including after a
  /// vouch/clear — the GC-after-vouch fault). Adversarial-store regime.
  action storeGcAt(o: (str, int)): bool = all {
    budget.storeGc > 0,
    store.get(o).present,
    not(store.get(o).pinned),
    store' = store.set(o, { ...store.get(o), present: false }),
    budget' = { ...budget, storeGc: budget.storeGc - 1 },
    drv' = drv, blds' = blds, memEdges' = memEdges, pgEdges' = pgEdges,
    intents' = intents, lead' = lead, viol' = viol, reach' = reach
  }

  // --- 7b. PG intent plumbing ----------------------------------------------

  /// Apply one pending intent to the durable state. Best-effort
  /// statements may have been emitted by a now-deposed tenure; outside
  /// the stale-apply regimes those were discarded at recovery.
  ///
  /// Phase 1 — the claims-floor fence (the Wave-3 production fix,
  /// sched.evidence.durability+2, encoded Wave 4): a STALE evidence
  /// intent — one whose tenure sits below the current leader generation
  /// — is DISCARDED at the apply site instead of applied: it leaves the
  /// queue with no PG effect, the model form of "a below-floor write
  /// rolls back having written nothing". At this abstraction lead.gen
  /// IS the durable claims floor, because recoverAsLeader atomically
  /// claims the new generation together with the recovery's own writes;
  /// the code earns the same equivalence by claiming BEFORE its
  /// recovery writes run (the Wave-3 T-3.1 LeaderAcquired reorder — the
  /// model cannot represent the unfixed claim-after-recovery ordering,
  /// which is exactly why that reorder is a production prerequisite of
  /// the fence, review findings CF-2/FC-2). The A17/A18 violation
  /// latches sit on the apply path, so a fenced discard can never set
  /// them; their reachability evidence moves to the fencedDiscard flag
  /// (the canReachFencedDiscard witness) and to the frozen pre-fence
  /// apply in the closure-a17-unfenced calibration override. Zero new
  /// state variables (review finding MCI-4): the fence is a branch in
  /// this action, not a tracked floor.
  action pgApplyAny: bool = {
    nondet pick = intents.union(Set(noopIntent)).oneOf()
    val stale = pick.tenure < lead.gen
    val isEvidence = pick.op != INop
    // The fence: a stale evidence intent is discarded, never applied.
    val fenced = stale and isEvidence
    all {
      intents.contains(pick),
      intents' = intents.exclude(Set(pick)),
      reach' = if (fenced) { ...reach, fencedDiscard: true }
        else { ...reach, staleIntentApplied: reach.staleIntentApplied or stale },
      viol' = if (fenced) viol else {
        ...viol,
        staleEvidenceApply: viol.staleEvidenceApply or (stale and isEvidence),
        // A17: a stale clear/heal erasing a bit set by a newer tenure
        staleClearOverride: viol.staleClearOverride or (stale and
          match pick.op {
            | IClearBoth(d) => (drv.get(d).pgTp and pick.tenure < drv.get(d).pgTpGen)
                                or (drv.get(d).pgCh and pick.tenure < drv.get(d).pgChGen)
            | IClearMark(d) => drv.get(d).pgTp and pick.tenure < drv.get(d).pgTpGen
            | IHeal(d) => drv.get(d).pgCh and pick.tenure < drv.get(d).pgChGen
            | _ => false
          }),
        // A7/B5 hooks: the merge upsert never lowers a bit / shrinks the union
        pgBitLowered: viol.pgBitLowered,
        wantedShrunk: viol.wantedShrunk
      },
      drv' = if (fenced) drv else match pick.op {
        | IClearBoth(d) =>
            if (drv.get(d).rowExists)
              drv.set(d, { ...drv.get(d), pgTp: false, pgCh: false })
            else drv
        | IClearMark(d) =>
            if (drv.get(d).rowExists) drv.set(d, { ...drv.get(d), pgTp: false }) else drv
        | ISetHole(d) =>
            if (drv.get(d).rowExists and not(drv.get(d).pgCh))
              drv.set(d, { ...drv.get(d), pgCh: true, pgChGen: pick.tenure })
            else drv
        | IHeal(d) =>
            if (drv.get(d).rowExists) drv.set(d, { ...drv.get(d), pgCh: false }) else drv
        | IMerge(m) =>
            m.rows.fold(drv, (acc, r) => {
              val rec = acc.get(r)
              val newWanted = rec.pgWanted.union(if (m.narrow) Set(1) else OUTIDXSET)
              acc.set(r, { ...rec,
                rowExists: true,
                pgSt: if (rec.mSt != MAbsent) rec.mSt else if (rec.rowExists) rec.pgSt else MQueued,
                pgTp: rec.pgTp or m.stamps.contains(r),
                pgTpGen: if (not(rec.pgTp) and m.stamps.contains(r)) pick.tenure else rec.pgTpGen,
                pgWanted: newWanted })
            })
        | INop => drv
      },
      pgEdges' = if (fenced) pgEdges else match pick.op {
        | IMerge(m) =>
            if (m.pruned) pgEdges
            else pgEdges.union(REFS.filter(e => m.rows.contains(e._1) and m.rows.contains(e._2)))
        | _ => pgEdges
      },
      blds' = if (fenced) blds else match pick.op {
        // build_derivations links are per build id; the directed resubmit
        // re-uses the slot, so its links REPLACE the failed submission's
        // (a fresh build id in production)
        | IMerge(m) => blds.set(m.bld, { ...blds.get(m.bld), pgRow: true, pgLinks: m.rows })
        | _ => blds
      },
      store' = store, memEdges' = memEdges, lead' = lead, budget' = budget
    }
  }

  /// Lose one pending best-effort intent (the warn-and-continue
  /// statements). The merge transaction is droppable only once its
  /// tenure is deposed (the handler would otherwise have observed the
  /// failure and rolled back).
  action pgDropAny: bool = {
    nondet pick = intents.union(Set(noopIntent)).oneOf()
    all {
      budget.pgDrops > 0,
      intents.contains(pick),
      match pick.op { | IMerge(m) => pick.tenure < lead.gen | INop => false | _ => true },
      intents' = intents.exclude(Set(pick)),
      budget' = { ...budget, pgDrops: budget.pgDrops - 1 },
      drv' = drv, store' = store, blds' = blds, memEdges' = memEdges,
      pgEdges' = pgEdges, lead' = lead, viol' = viol, reach' = reach
    }
  }

  // --- 7c. lease transitions ------------------------------------------------

  /// The leader loses the lease. The in-memory state stays in place but
  /// no leader-gated handler can run until a recovery re-derives
  /// everything from PG; pending intents of this tenure may still land
  /// (stale-apply regimes) or are discarded at the next recovery.
  action leaderLost: bool = all {
    lead.up,
    budget.failovers > 0,
    lead' = { ...lead, up: false, recovered: false },
    budget' = { ...budget, failovers: budget.failovers - 1 },
    drv' = drv, store' = store, blds' = blds, memEdges' = memEdges,
    pgEdges' = pgEdges, intents' = intents, viol' = viol, reach' = reach
  }

  /// Leader recovery: one atomic action over the durable state at this
  /// instant — row load (terminal rows excluded), poisoned-row TTL
  /// split, edge truncation to loaded endpoints, failed-dep
  /// condemnation (the co-owned R2 cascade pre-pass AND the
  /// equally-co-owned compute_initial_states in-DAG dep check — see the
  /// dm2 comment; both scoped as of Phase-1 Wave 2b),
  /// the strict-criterion clear gate (C3), the recovery hole stamp
  /// (H4), the in-memory-only Substituting reset, interest rebuild
  /// from durable links without per-build contributions.
  action recoverAsLeader: bool = {
    nondet expired = DRVS.powerset().oneOf()
    val newGen = lead.gen + 1
    // poisoned rows chosen as TTL-expired: cleared in PG, not loaded (D10)
    val expiredPoisons = expired.intersect(DRVS.filter(d => drv.get(d).rowExists and drv.get(d).pgSt == MPoisoned))
    // rows loaded into the recovered DAG
    val loaded = DRVS.filter(d => drv.get(d).rowExists and or {
      not(isTerminal(drv.get(d).pgSt)),
      drv.get(d).pgSt == MPoisoned and not(expiredPoisons.contains(d))
    })
    val newMemEdges = pgEdges.filter(e => loaded.contains(e._1) and loaded.contains(e._2))
    // pass 1: restore rows verbatim (poisoned carve-out hard-codes bits false)
    val dm1 = DRVS.mapBy(d => {
      val r = drv.get(d)
      if (not(loaded.contains(d)))
        { ...r,
          mSt: MAbsent, mTp: false, mCh: false, tried: false, triedGenuine: false,
          nf: Set(), probed: false, probedKind: PKNone,
          walks: if (CROSS_TENURE_WALKS) r.walks else [],
          pgSt: if (expiredPoisons.contains(d)) MQueued else r.pgSt,
          wrongFF: r.wrongFF, wrongFFStrict: r.wrongFFStrict }
      else if (r.pgSt == MPoisoned)
        { ...r,
          mSt: MPoisoned, mTp: false, mCh: false, tried: false, triedGenuine: false,
          nf: Set(), probed: false, probedKind: PKNone,
          walks: if (CROSS_TENURE_WALKS) r.walks else [],
          wrongFF: 0, wrongFFStrict: r.wrongFFStrict }
      else
        { ...r,
          mSt: if (r.pgSt == MOpen and not(r.attemptOpen)) MReady else r.pgSt,
          mTp: r.pgTp, mCh: r.pgCh,
          tried: false, triedGenuine: false, nf: Set(), probed: false, probedKind: PKNone,
          walks: if (CROSS_TENURE_WALKS) r.walks else [],
          // a restored mark re-arms the per-arming wrongful-fail-fast budget
          wrongFF: if (r.pgTp) 0 else r.wrongFF, wrongFFStrict: r.wrongFFStrict }
    })
    // pass 2: failed-dep condemnation (R2 + the in-DAG recompute), clear gate
    // (C3), hole stamp (H4), Substituting reset, ready seeding — over the
    // durable relation
    val dm2 = DRVS.mapBy(d => {
      val r = dm1.get(d)
      if (r.mSt == MAbsent or r.mSt == MPoisoned) r
      else {
        val kidsLoaded = pKids(newMemEdges, d)
        // Failed-dep condemnation is the disjunction of production's TWO
        // recovery mechanisms — BOTH co-ownership-scoped as of the Phase-1
        // Wave-2b fix:
        //  (a) the R2 cascade pre-pass (load_parents_with_failed_deps →
        //      cascade_failed in seed_ready_queue) — co-ownership-scoped
        //      over the DURABLE relation (pCondemnCriterion; the bug_009
        //      evidence rule and the spec's condemnation rule);
        //  (b) the compute_initial_states recompute over the LOADED child
        //      set (any_co_owned_dep_terminally_failed): a node above an
        //      in-DAG un-produced-terminal child is condemned only when a
        //      live build co-owns the pair (pInDagCondemnCriterion). The
        //      pre-fix unscoped form — any loaded terminal child condemns,
        //      regardless of ownership — was the Wave-2 residual finding
        //      (spec-vs-code divergence from
        //      sched.recovery.failed-dep-cascade+2's MUST NOT clause),
        //      fixed red-first in Wave 2b.
        // A parent spared by the scoping recovers MQueued above the loaded
        // poisoned child; the poisonClear survivor promotion (the
        // production reevaluate_removal_survivors hook, mirrored in
        // poisonClear below) is its wake-up edge — the pairing that keeps
        // L3 (liveBuildTerminalOrProgressArmed) holding under the
        // narrowed condemnation. The A22 latch (condemnUnscoped) stays
        // keyed on arm (a) only — it guards the cascade mechanism the
        // spec rule names explicitly.
        val condemned = pCondemnCriterion(dm1, blds, BUILDS, pgEdges, d)
          or pInDagCondemnCriterion(dm1, blds, BUILDS, newMemEdges, d)
        val clearGate = r.mTp and not(r.mCh) and pStrictPgCriterion(dm1, blds, BUILDS, pgEdges, d)
        val holeStamp = pRecoveryHoleCriterion(dm1, pgEdges, d)
        val depsSatisfied = kidsLoaded.forall(c => dm1.get(c).mSt == MProduced)
        // seed_ready_queue re-derives dispatchability from the dependency
        // walk: a persisted 'ready'/'substituting' status is never trusted
        // over the recovered child set
        val baseSt =
          if (condemned) MDepFailed
          else if (r.mSt == MSubstituting or r.mSt == MQueued or r.mSt == MReady)
            (if (depsSatisfied) MReady else MQueued)
          else if (r.mSt == MOpen) MOpen
          else r.mSt
        { ...r,
          mSt: baseSt,
          mTp: if (clearGate) false else r.mTp,
          mCh: if (clearGate) false else (r.mCh or holeStamp),
          pgSt: if (condemned and r.rowExists) MDepFailed else r.pgSt }
      }
    })
    val clearedSet = DRVS.filter(d => dm1.get(d).mSt != MAbsent and dm1.get(d).mSt != MPoisoned and
      dm1.get(d).mTp and not(dm1.get(d).mCh) and pStrictPgCriterion(dm1, blds, BUILDS, pgEdges, d))
    val holedSet = DRVS.filter(d => dm2.get(d).mSt != MAbsent and dm2.get(d).mSt != MPoisoned and
      not(clearedSet.contains(d)) and pRecoveryHoleCriterion(dm1, pgEdges, d) and not(dm1.get(d).mCh))
    // The CASCADE-arm condemned set only (arm (a) above): feeds the A22
    // latch hook, which guards the co-ownership scoping of the R2 cascade
    // pre-pass. Nodes condemned solely by the in-DAG recompute arm (b) are
    // deliberately not in this set — see the dm2 comment.
    val condemnedSet = DRVS.filter(d => dm1.get(d).mSt != MAbsent and dm1.get(d).mSt != MPoisoned and
      pCondemnCriterion(dm1, blds, BUILDS, pgEdges, d))
    val newIntents =
      clearedSet.map(d => { op: IClearBoth(d), tenure: newGen })
        .union(holedSet.map(d => { op: ISetHole(d), tenure: newGen }))
    all {
      not(lead.up),
      // intent-ceiling headroom for the recovery's own emissions
      (if (STALE_APPLY) intents.size() else 0) + newIntents.size() <= INTENT_CEIL,
      lead' = { up: true, gen: newGen, recovered: true },
      drv' = dm2,
      memEdges' = newMemEdges,
      // interest rebuilt from durable links of live builds; contributions
      // lost (AW4); builds PG does not know (or that are terminal) carry
      // no recovered interest and no pending cleanup. The
      // finalize_recovered_builds tail fails a recovered build whose
      // member came back un-produced terminal (within-TTL poisoned load,
      // failed-dep condemnation) before the first dispatch.
      blds' = BUILDS.mapBy(b => {
        val br = blds.get(b)
        if (br.phase == BpActive and br.pgRow) {
          val newInterest = br.pgLinks.intersect(loaded)
          val memberFailed = newInterest.exists(m => isUnprodTerminal(dm2.get(m).mSt))
          { ...br,
            interest: newInterest,
            contrib: DRVS.mapBy(d => Set()),
            phase: if (memberFailed) BpFailed else br.phase,
            cleanup: br.cleanup or memberFailed }
        }
        else { ...br, interest: Set(), contrib: DRVS.mapBy(d => Set()), cleanup: false }
      }),
      intents' = (if (STALE_APPLY) intents else Set()).union(newIntents),
      pgEdges' = pgEdges, store' = store, budget' = budget,
      viol' = { ...viol,
        // A19/A10/A22 latch hooks: the as-built recovery implements the
        // gates exactly, so these stay false; calibration overrides
        // re-derive dm2 differently and set them.
        recoveryClearMissed: viol.recoveryClearMissed,
        restoredBitDropped: viol.restoredBitDropped,
        condemnUnscoped: viol.condemnUnscoped or
          condemnedSet.exists(d => not(pCondemnCriterion(dm1, blds, BUILDS, pgEdges, d))),
        // A15's durable-relation half (CE-47): no node is seeded Ready above
        // a durable un-produced-terminal child a live build co-owns with it
        // (the failed-dep condemnation owns exactly this set as built)
        readyAboveFailedDepAtRecovery: viol.readyAboveFailedDepAtRecovery or
          DRVS.exists(d => and {
            dm2.get(d).mSt == MReady,
            pCondemnCriterion(dm1, blds, BUILDS, pgEdges, d)
          })
      },
      reach' = { ...reach,
        recoveryClear: reach.recoveryClear or clearedSet != Set(),
        holeRecovery: reach.holeRecovery or holedSet != Set() }
    }
  }

  // --- 7d. merge (SubmitBuild -> MergeDag) ----------------------------------

  /// A submission is rolled back (activation failure): nothing changes.
  action mergeRollback(b: str): bool = all {
    leaderActive,
    budget.subs > 0,
    blds.get(b).phase == BpNone,
    budget' = { ...budget, subs: budget.subs - 1 },
    reach' = { ...reach, rollbackSeen: true },
    drv' = drv, store' = store, blds' = blds, memEdges' = memEdges,
    pgEdges' = pgEdges, intents' = intents, lead' = lead, viol' = viol
  }

  /// A committed merge: prune decision over the demand set, stamps for
  /// kept nodes whose submitted closure was dropped, cache-hit adoption,
  /// pending-substitute walk spawns, the stale-Completed verify, the
  /// heal, the post-reconciliation clear pass, and the all-or-nothing
  /// merge transaction emitted as one intent. Submissions are the
  /// REFS-closure of a single root (the DAG-shape freedom comes from the
  /// root choice, the REFS constant and overlapping builds).
  action mergeCommit(b: str): bool = {
    nondet rootPick = DRVS.oneOf()
    nondet narrow = Set(true, false).oneOf()
    nondet explicitOne = DRVS.oneOf()
    nondet useExplicit = Set(true, false).oneOf()
    nondet tryPrune = Set(true, false).oneOf()
    nondet pessimistic = Set(true, false).oneOf()
    val rootsPick = Set(rootPick)
    val submitted = pClosure(REFS, rootsPick)
    val explicitSet =
      if (useExplicit and submitted.contains(explicitOne) and not(rootsPick.contains(explicitOne)))
        Set(explicitOne)
      else Set()
    val demand = rootsPick.union(explicitSet)
    val contribW = if (narrow) Set(1) else OUTIDXSET
    // the submission-time wanted view (this build's contribution over its
    // whole submission) — every wanted-driven judgement below runs over the
    // LIVE effective wanted set, never this submission's contribution alone
    val bldsProbe = blds.set(b, { ...blds.get(b),
      phase: BpActive, cleanup: false,
      interest: submitted,
      contrib: DRVS.mapBy(d => if (submitted.contains(d)) contribW else Set()) })
    // prune availability over the demand set's live-wanted outputs (B4: the
    // empty-wanted guard; B7/B3: indeterminate never counts as available)
    val demandAvailable = demand != Set() and contribW != Set() and demand.forall(dd =>
      pEffWanted(drv, bldsProbe, BUILDS, dd).forall(i =>
        store.get((dd, i)).present or pSubAnswerable(store, (dd, i))))
    val pruneOk = tryPrune and demandAvailable
    val kept = if (pruneOk) demand else submitted
    // the post-merge build map: interest is the kept set
    val bldsNew = blds.set(b, { ...blds.get(b),
      phase: BpActive, cleanup: false,
      interest: kept,
      contrib: DRVS.mapBy(d => if (kept.contains(d)) contribW else Set()) })
    // per-node handling over the pre-merge state
    val resetNodes = kept.filter(d => isTerminal(drv.get(d).mSt) or drv.get(d).mSt == MFailed)
    val newEdges = if (pruneOk) Set() else REFS.filter(e => kept.contains(e._1) and kept.contains(e._2))
    val memEdges1 = memEdges.union(newEdges)
    // pass 1: bring kept nodes into the DAG / reset terminal ones.
    // The Cancelled/Failed resubmit-reset (dag/mod.rs:389-443
    // is_retriable_on_resubmit remove+reinsert; merge.rs existing_reprobe
    // for Poisoned/DependencyFailed): the node re-enters as FRESH state —
    // hole carried, mark re-derived by THIS merge's own prune/stamp
    // decision, and NO inherited probe or walk evidence: the reset node
    // flows through the classification below (cache-hit / pending-
    // substitute / confirmed-missing) against the store AS IT IS NOW,
    // never against a verdict formed before it went terminal
    // (C3-adjudication model correction). Stale walk records are
    // discarded here: production re-inserts the node without any link to
    // the old detached task, whose eventual completion meets the
    // not-Substituting drop guard.
    val dm1 = DRVS.mapBy(d => {
      val r = drv.get(d)
      if (not(kept.contains(d))) r
      else if (r.mSt == MAbsent)
        { ...r, mSt: MQueued, mTp: false, mCh: false, tried: false, triedGenuine: false,
          nf: Set(), probed: false, probedKind: PKNone }
      else if (resetNodes.contains(d) and r.mSt != MProduced)
        { ...r, mSt: MQueued, mTp: false, tried: false, triedGenuine: false,
          nf: Set(), probed: false, probedKind: PKNone, walks: [],
          pgSt: if (r.rowExists) MQueued else r.pgSt }
      else r
    })
    // stale-Completed verify: pre-existing Produced kept nodes whose
    // live-wanted outputs are absent are demoted back to Queued (B9 guard)
    val staleProduced = kept.filter(d => drv.get(d).mSt == MProduced and
      not(pEffWanted(drv, bldsNew, BUILDS, d).forall(i => store.get((d, i)).present)))
    val dm2 = staleProduced.fold(dm1, (acc, d) =>
      acc.set(d, { ...acc.get(d), mSt: MQueued, pgSt: if (acc.get(d).rowExists) MQueued else acc.get(d).pgSt }))
    // stamp decision: kept, pruned merge, submitted closure dropped, not Vouched post-merge
    val stamps = if (not(pruneOk)) Set() else kept.filter(d => and {
      pKids(REFS, d).intersect(submitted).exists(c => not(kept.contains(c))),
      not(pVouched(dm2, memEdges1, d))
    })
    val dm3 = stamps.fold(dm2, (acc, d) =>
      acc.set(d, { ...acc.get(d), mTp: true, wrongFF: 0, wrongFFStrict: 0 }))
    // merge-time classification for kept un-produced, un-substituting nodes,
    // judged over the live effective wanted set
    val classifiable = kept.filter(d => dm3.get(d).mSt == MQueued or dm3.get(d).mSt == MReady)
    val cacheHits = classifiable.filter(d =>
      pEffWanted(drv, bldsNew, BUILDS, d).forall(i => store.get((d, i)).present))
    val confirmableMissing = classifiable.filter(d =>
      pEffWanted(drv, bldsNew, BUILDS, d).exists(i => pMissAnswerable(store, (d, i))))
    // Pending-substitute lane (the I-094 / existing_reprobe analogue): a
    // classifiable node is never Substituting, so any walk records it
    // still carries are stale (their late results are dropped on
    // arrival); the re-probe spawns a fresh fetch regardless of them,
    // exactly as production's reprobe-substitutable lane does.
    val pendingSub = classifiable.filter(d => and {
      not(cacheHits.contains(d)),
      not(pessimistic and confirmableMissing.contains(d))
    })
    val normalQueued = classifiable.filter(d =>
      not(cacheHits.contains(d)) and pessimistic and confirmableMissing.contains(d))
    val dm4 = DRVS.mapBy(d => {
      val r = dm3.get(d)
      val effWd = pEffWanted(drv, bldsNew, BUILDS, d)
      if (cacheHits.contains(d))
        { ...r, mSt: MProduced, pgSt: if (r.rowExists) MProduced else r.pgSt,
          probed: true, probedKind: PKPresent }
      else if (pendingSub.contains(d))
        { ...r, mSt: MSubstituting, probed: true,
          probedKind: if (effWd.forall(i => store.get((d, i)).present or pSubAnswerable(store, (d, i)))) PKSubst else PKIndet,
          // fresh spawn replaces stale (never-Substituting) records — the
          // walk-instance ceiling stays respected without blocking the
          // re-probe arming the node
          walks: [{ forgivable: OUTIDXSET.exclude(effWd).exclude(r.nf), tenure: lead.gen, res: WPending }] }
      else if (normalQueued.contains(d))
        { ...r, probed: true, probedKind: PKMissing }
      else r
    })
    // heal (HC1): a full merge re-declaring a parent's edges heals its hole
    val healParents = if (pruneOk) Set() else kept.filter(d => pKids(REFS, d).intersect(kept) != Set())
    val dm5 = healParents.fold(dm4, (acc, d) => acc.set(d, { ...acc.get(d), mCh: false }))
    // C1 post-reconciliation clear pass: edge parents of this submission and
    // parents of this merge's cache hits, marked and Vouched now
    val clearCandidates = healParents.union(cacheHits.fold(Set(), (acc, c) => acc.union(pParents(memEdges1, c))))
    val clearedC1 = clearCandidates.filter(d => dm5.get(d).mTp and pVouched(dm5, memEdges1, d))
    val dm6c = clearedC1.fold(dm5, (acc, d) => acc.set(d, { ...acc.get(d), mTp: false, mCh: false }))
    // readiness reconcile over the merged edge set, both directions and for
    // every live node: a Queued node whose live children are all produced
    // promotes (the promote_newly_ready cascade for this merge's cache
    // hits); a Ready node that gained un-produced children — a full merge
    // adding unbuilt deps, or a stale-Produced child demoted by the verify
    // — is re-gated on them (the dependent-reset half of the stale reset)
    val dm7 = DRVS.mapBy(d => {
      val r = dm6c.get(d)
      val kidsProduced = pKids(memEdges1, d).forall(c => dm6c.get(c).mSt == MProduced)
      if (r.mSt == MQueued and kidsProduced) { ...r, mSt: MReady }
      else if (r.mSt == MReady and not(kidsProduced)) { ...r, mSt: MQueued }
      else r
    })
    val mergeIntent: IntentRec = { op: IMerge({ bld: b, rows: kept, stamps: stamps, pruned: pruneOk, narrow: narrow }), tenure: lead.gen }
    val healIntents = healParents.filter(d => drv.get(d).rowExists and drv.get(d).pgCh)
      .map(d => { op: IHeal(d), tenure: lead.gen })
    val clearIntents = clearedC1.map(d => { op: IClearBoth(d), tenure: lead.gen })
    val allNew = Set(mergeIntent).union(healIntents).union(clearIntents)
    all {
      leaderActive,
      budget.subs > 0,
      blds.get(b).phase == BpNone or blds.get(b).phase == BpFailed,
      intents.size() + allNew.size() <= INTENT_CEIL,
      drv' = dm7,
      memEdges' = memEdges1,
      blds' = bldsNew,
      intents' = intents.union(allNew),
      budget' = { ...budget, subs: budget.subs - 1 },
      store' = store, pgEdges' = pgEdges, lead' = lead,
      viol' = { ...viol,
        // A2 hook: a stamp outside the predicate (cannot happen as built)
        stampUnsound: viol.stampUnsound or stamps.exists(d =>
          not(pruneOk) or pVouched(dm2, memEdges1, d) or
          not(pKids(REFS, d).intersect(submitted).exists(c => not(kept.contains(c))))),
        // A20: a re-declared parent with a persisted hole missing its heal intent
        healMissed: viol.healMissed or
          (not(pruneOk) and healParents.exists(d => drv.get(d).rowExists and drv.get(d).pgCh and
            not(healIntents.exists(i => i.op == IHeal(d))))),
        // A8 hook: a C1 clear on a childless/holed parent
        brokenVouched: viol.brokenVouched or clearedC1.exists(d =>
          pKids(memEdges1, d) == Set() or dm5.get(d).mCh),
        // A3 hook: a C1 clear without Vouched evidence
        clearUnsound: viol.clearUnsound or clearedC1.exists(d => not(pVouched(dm5, memEdges1, d))),
        // B10: the prune dropped a demand-set member
        demandDropped: viol.demandDropped or (pruneOk and not(demand.subseteq(kept))),
        // B7: the prune counted an output available that the classification
        // criterion would not (neither present nor cache-hit/upstream-has)
        pruneWeakerThanClassify: viol.pruneWeakerThanClassify or (pruneOk and demand.exists(dd =>
          pEffWanted(drv, bldsNew, BUILDS, dd).exists(i =>
            not(store.get((dd, i)).present) and not(pSubAnswerable(store, (dd, i)))))),
        // B4: availability judged over an empty wanted set
        vacuousWantedVerdict: viol.vacuousWantedVerdict or (pruneOk and contribW == Set()),
        // C4: a node with all live-wanted outputs present queued for from-source
        queuedWhilePresent: viol.queuedWhilePresent or normalQueued.exists(d =>
          pEffWanted(drv, bldsNew, BUILDS, d).forall(i => store.get((d, i)).present)),
        // B1: cache-hit adoption without presence
        producedWithoutPresence: viol.producedWithoutPresence or cacheHits.exists(d =>
          not(pEffWanted(drv, bldsNew, BUILDS, d).forall(i => store.get((d, i)).present))),
        // B9: a kept parent promoted/left Ready above a stale Produced child
        staleProducedUnlocked: viol.staleProducedUnlocked or kept.exists(d =>
          dm7.get(d).mSt == MReady and pKids(memEdges1, d).exists(c =>
            dm7.get(c).mSt == MProduced and
            not(pEffWanted(dm7, bldsNew, BUILDS, c).forall(i => store.get((c, i)).present)))),
        // B3: indeterminate counted as available for the prune is impossible
        // by construction (pSubAnswerable excludes it); hook stays as is
        indetDemoted: viol.indetDemoted
      },
      reach' = { ...reach,
        stampFired: reach.stampFired or stamps != Set(),
        pruneFired: reach.pruneFired or pruneOk }
    }
  }

  // --- 7e. build lifecycle ---------------------------------------------------

  /// Client-side cancellation of an active build.
  action cancelBuild(b: str): bool = all {
    leaderActive,
    budget.cancels > 0,
    blds.get(b).phase == BpActive,
    val members = blds.get(b).interest
    val exclusive = members.filter(m => BUILDS.forall(b2 => b2 == b or
      not(blds.get(b2).phase == BpActive and blds.get(b2).interest.contains(m))))
    all {
      drv' = exclusive.fold(drv, (acc, m) => {
        val r = acc.get(m)
        if (not(isTerminal(r.mSt)) and r.mSt != MAbsent)
          acc.set(m, { ...r, mSt: MCancelled, pgSt: if (r.rowExists) MCancelled else r.pgSt })
        else acc
      }),
      blds' = blds.set(b, { ...blds.get(b), phase: BpCancelled, cleanup: true }),
      budget' = { ...budget, cancels: budget.cancels - 1 },
      store' = store, memEdges' = memEdges, pgEdges' = pgEdges,
      intents' = intents, lead' = lead, viol' = viol, reach' = reach
    }
  }

  /// A live build whose members are all produced completes.
  action buildCompletes(b: str): bool = all {
    leaderActive,
    blds.get(b).phase == BpActive,
    blds.get(b).interest != Set(),
    blds.get(b).interest.forall(m => drvOf(m).mSt == MProduced),
    blds' = blds.set(b, { ...blds.get(b), phase: BpDoneOk, cleanup: true }),
    drv' = drv, store' = store, memEdges' = memEdges, pgEdges' = pgEdges,
    intents' = intents, lead' = lead, budget' = budget, viol' = viol, reach' = reach
  }

  /// A live build with an un-produced terminal member fails.
  action buildFails(b: str): bool = all {
    leaderActive,
    blds.get(b).phase == BpActive,
    blds.get(b).interest.exists(m => isUnprodTerminal(drvOf(m).mSt)),
    blds' = blds.set(b, { ...blds.get(b), phase: BpFailed, cleanup: true }),
    drv' = drv, store' = store, memEdges' = memEdges, pgEdges' = pgEdges,
    intents' = intents, lead' = lead, budget' = budget, viol' = viol, reach' = reach
  }

  /// Terminal-build cleanup: reap exclusively-owned members, stamp holes
  /// on surviving parents that lost an un-produced child (H1), then run
  /// the survivor verdicts (Phase 1: the SETTLEMENT for marked-Broken
  /// non-Queued survivors — verification-respawn or genuine fail-fast —
  /// and the promotion arm for vacuously-ready Queued ones).
  action reapTerminalBuild(b: str): bool = all {
    leaderActive,
    blds.get(b).cleanup,
    blds.get(b).phase == BpDoneOk or blds.get(b).phase == BpFailed or blds.get(b).phase == BpCancelled,
    val members = blds.get(b).interest
    val removalSet = members.filter(m => BUILDS.forall(b2 => b2 == b or not(blds.get(b2).interest.contains(m))))
    val holedParents = DRVS.filter(p => and {
      not(removalSet.contains(p)),
      drvOf(p).mSt != MAbsent,
      removalSet.exists(c => memEdges.contains((p, c)) and drvOf(c).mSt != MProduced)
    })
    val memEdges1 = memEdges.filter(e => not(removalSet.contains(e._1)) and not(removalSet.contains(e._2)))
    val dmRemoved = DRVS.mapBy(d => {
      val r = drv.get(d)
      if (removalSet.contains(d)) { ...r, mSt: MAbsent, walks: [], nf: Set(), attemptOpen: false }
      else if (holedParents.contains(d)) { ...r, mCh: true }
      else r
    })
    val bm1 = blds.set(b, { ...blds.get(b), interest: Set(), cleanup: false })
    // Survivor verdicts on the post-removal state (Phase 1 — mirrors
    // production reevaluate_removal_survivors + settle_broken_marked_root,
    // Wave-1 T-1.6): every marked-Broken survivor that is not in flight
    // (Substituting/Open), not terminal and NOT Queued routes through the
    // settlement. The MQueued exclusion is the same one the code carries:
    // Queued survivors take the promotion arm below (and the next sweep's
    // settlement-aware partition picks the promoted node up); without the
    // exclusion the model would wrongfully fail-fast a Queued+tried
    // survivor that the code promotes-and-completes (review finding
    // MCI-2), and the dropped pre-fix `tried` requirement means untried
    // survivors are settled here too instead of being skipped.
    val survivors = DRVS.filter(p => not(removalSet.contains(p)) and dmRemoved.get(p).mSt != MAbsent and
      removalSet.exists(c => memEdges.contains((p, c))))
    val settleTargets = survivors.filter(p => and {
      pMustSub(dmRemoved, memEdges1, p),
      not(isTerminal(dmRemoved.get(p).mSt)),
      dmRemoved.get(p).mSt != MSubstituting, dmRemoved.get(p).mSt != MOpen,
      dmRemoved.get(p).mSt != MQueued,
      pLiveInterest(bm1, BUILDS, p)
    })
    // The settlement's two arms (settle_broken_marked_root): an UNTRIED
    // survivor whose live effective wanted outputs are all obtainable
    // re-spawns the verification walk (one-shot spent at the spawn); a
    // tried survivor (one-shot already spent) or one with a confirmed-
    // unobtainable output takes the genuine fail-fast.
    val respawnTargets = settleTargets.filter(p => and {
      not(dmRemoved.get(p).tried),
      pAllWantedPresentOrUpstream(dmRemoved, bm1, BUILDS, store, p)
    })
    val ffTargets = settleTargets.exclude(respawnTargets)
    val afterFf = ffTargets.fold({ dm: dmRemoved, bm: bm1, wrongful: false, fired: false }, (st, p) => {
      val one = pFailFast(st.dm, st.bm, BUILDS, memEdges1, store, p)
      { dm: one.dm, bm: one.bm, wrongful: st.wrongful or one.wrongful, fired: st.fired or one.fired }
    })
    val dmFinal = DRVS.mapBy(d => {
      val r = afterFf.dm.get(d)
      if (respawnTargets.contains(d))
        // verification-walk respawn: one-shot spent, fresh walk armed (the
        // survivor was not Substituting, so any walk records it carries are
        // stale never-Substituting leftovers — the fresh spawn replaces
        // them, the mergeCommit pendingSub re-probe precedent)
        { ...r, tried: true, mSt: MSubstituting, probed: true,
          probedKind: if (pAllWantedPresent(afterFf.dm, afterFf.bm, BUILDS, store, d)) PKPresent else PKSubst,
          walks: [{ forgivable: OUTIDXSET.exclude(pEffWanted(afterFf.dm, afterFf.bm, BUILDS, d)).exclude(r.nf), tenure: lead.gen, res: WPending }] }
      else if (survivors.contains(d) and not(ffTargets.contains(d)) and r.mSt == MQueued and
          pKids(memEdges1, d).forall(c => afterFf.dm.get(c).mSt == MProduced))
        { ...r, mSt: MReady }
      else r
    })
    val newIntents = holedParents.map(p => { op: ISetHole(p), tenure: lead.gen })
      .union(ffTargets.map(p => { op: IClearMark(p), tenure: lead.gen }))
    all {
      intents.size() + newIntents.size() <= INTENT_CEIL,
      drv' = dmFinal,
      blds' = afterFf.bm,
      memEdges' = memEdges1,
      intents' = intents.union(newIntents),
      store' = store, pgEdges' = pgEdges, lead' = lead, budget' = budget,
      viol' = { ...viol,
        // A4/A5 hooks: holes exactly for surviving parents of un-produced removals
        holeUnsound: viol.holeUnsound or holedParents.exists(p =>
          not(removalSet.exists(c => memEdges.contains((p, c)) and drv.get(c).mSt != MProduced))),
        holeMissed: viol.holeMissed or DRVS.exists(p => and {
          not(removalSet.contains(p)), drv.get(p).mSt != MAbsent,
          removalSet.exists(c => memEdges.contains((p, c)) and drv.get(c).mSt != MProduced),
          not(dmFinal.get(p).mCh or ffTargets.contains(p))
        }),
        // A14: the fail-fast never overwrites a terminal survivor (guard above)
        terminalOverwritten: viol.terminalOverwritten or ffTargets.exists(p => isTerminal(dmRemoved.get(p).mSt)),
        wrongfulTerminalFailure: viol.wrongfulTerminalFailure or afterFf.wrongful
      },
      reach' = { ...reach,
        holeReap: reach.holeReap or holedParents != Set(),
        failFast: reach.failFast or afterFf.fired }
    }
  }

  // --- 7f. dispatch probe (the Tick sweep partition) --------------------------

  /// Probe a Ready node and find every effective-wanted output present:
  /// inline-complete (gated on not substitute_tried), then the C2 clear
  /// and promote cascade for its parents.
  action probeInline(d: str): bool = all {
    leaderActive,
    drvOf(d).mSt == MReady,
    liveInterest(d),
    not(drvOf(d).tried),
    allWantedPresent(d),
    val dm1 = drv.set(d, { ...drvOf(d), mSt: MProduced, pgSt: if (drvOf(d).rowExists) MProduced else drvOf(d).pgSt,
      probed: true, probedKind: PKPresent, nf: Set() })
    val cascade = pPromoteAndClear(dm1, memEdges, DRVS, Set(d))
    val newIntents = cascade.cleared.map(p => { op: IClearBoth(p), tenure: lead.gen })
    all {
      intents.size() + newIntents.size() <= INTENT_CEIL,
      drv' = cascade.dm,
      intents' = intents.union(newIntents),
      store' = store, blds' = blds, memEdges' = memEdges, pgEdges' = pgEdges,
      lead' = lead, budget' = budget,
      viol' = { ...viol,
        producedWithoutPresence: viol.producedWithoutPresence or not(allWantedPresent(d)),
        dispatchNoInterest: viol.dispatchNoInterest or not(liveInterest(d)),
        brokenVouched: viol.brokenVouched or cascade.cleared.exists(p =>
          pKids(memEdges, p) == Set() or cascade.dm.get(p).mCh)
      },
      reach' = reach
    }
  }

  /// Probe a Ready node with missing wanted outputs answered
  /// substitutable-or-indeterminate: spawn a substitution walk (gated on
  /// not substitute_tried). The probe cache records a positive entry only
  /// when the answer derived from a live upstream consult.
  action probeWalk(d: str): bool = {
    nondet allSub = Set(true, false).oneOf()
    val missing = effWanted(d).filter(i => not(store.get((d, i)).present))
    val canAllSub = missing.forall(i => pSubAnswerable(store, (d, i)))
    val verdict = if (allSub and canAllSub) PKSubst else PKIndet
    all {
      leaderActive,
      drvOf(d).mSt == MReady,
      liveInterest(d),
      not(drvOf(d).tried),
      missing != Set(),
      drvOf(d).walks.length() < WALKS_PER_NODE,
      drv' = drv.set(d, { ...drvOf(d),
        mSt: MSubstituting, probed: true, probedKind: verdict,
        walks: drvOf(d).walks.append({ forgivable: OUTIDXSET.exclude(effWanted(d)).exclude(drvOf(d).nf), tenure: lead.gen, res: WPending }) }),
      store' = if (allSub and canAllSub)
        missing.fold(store, (acc, i) =>
          if (acc.get((d, i)).upHas) acc.set((d, i), { ...acc.get((d, i)), cache: CHit }) else acc)
        else store,
      viol' = { ...viol, dispatchNoInterest: viol.dispatchNoInterest or not(liveInterest(d)) },
      blds' = blds, memEdges' = memEdges, pgEdges' = pgEdges,
      intents' = intents, lead' = lead, budget' = budget, reach' = reach
    }
  }

  /// Probe a Ready marked-Broken node and get a confirmed-missing
  /// verdict for a wanted output: the fail-fast arm (the probe partition's
  /// must-substitute missing branch).
  action probeFailFastMissing(d: str): bool = all {
    leaderActive,
    drvOf(d).mSt == MReady,
    liveInterest(d),
    mustSub(d),
    effWanted(d).exists(i => pMissAnswerable(store, (d, i))),
    intents.size() + 1 <= INTENT_CEIL,
    val ff = pFailFast(drv.set(d, { ...drvOf(d), probed: true, probedKind: PKMissing }), blds, BUILDS, memEdges, store, d)
    all {
      ff.fired,
      drv' = ff.dm,
      blds' = ff.bm,
      intents' = intents.union(Set({ op: IClearMark(d), tenure: lead.gen })),
      store' = store,
      memEdges' = memEdges, pgEdges' = pgEdges, lead' = lead, budget' = budget,
      viol' = { ...viol,
        wrongfulTerminalFailure: viol.wrongfulTerminalFailure or ff.wrongful,
        ffBadEffect: viol.ffBadEffect or ff.dm.get(d).mTp or (drvOf(d).mCh and not(ff.dm.get(d).mCh)),
        terminalOverwritten: viol.terminalOverwritten or isTerminal(drvOf(d).mSt),
        dispatchNoInterest: viol.dispatchNoInterest or not(liveInterest(d)),
        indetDemoted: viol.indetDemoted or not(effWanted(d).exists(i => pMissAnswerable(store, (d, i))))
      },
      reach' = { ...reach, failFast: true }
    }
  }

  /// Probe a Ready marked-Broken node whose walk already failed
  /// (substitute_tried) and that has a CONFIRMED-UNOBTAINABLE wanted
  /// output (missing locally AND absent upstream): the tried-arm
  /// fail-fast, now genuine.
  ///
  /// Phase-1 narrowing (the D16/C3 settlement): pre-fix this arm fired
  /// on not(allWantedPresent(d)) — any locally-missing output, even one
  /// the upstream still offered — which was the wrongful half of the
  /// tried cell's routing; the obtainable cells (present, substitutable)
  /// now route to probeSettleTried instead. The two guards partition the
  /// tried cell via the shared settlement-arm defs (settlementEnabled),
  /// so this narrowing and the L2 armed form cannot drift apart.
  action probeFailFastTried(d: str): bool = all {
    leaderActive,
    drvOf(d).mSt == MReady,
    liveInterest(d),
    mustSub(d),
    drvOf(d).tried,
    failFastArmConfirmedMissing(d),
    intents.size() + 1 <= INTENT_CEIL,
    val ff = pFailFast(drv, blds, BUILDS, memEdges, store, d)
    all {
      ff.fired,
      drv' = ff.dm,
      blds' = ff.bm,
      intents' = intents.union(Set({ op: IClearMark(d), tenure: lead.gen })),
      store' = store, memEdges' = memEdges, pgEdges' = pgEdges, lead' = lead, budget' = budget,
      viol' = { ...viol,
        wrongfulTerminalFailure: viol.wrongfulTerminalFailure or ff.wrongful,
        ffBadEffect: viol.ffBadEffect or ff.dm.get(d).mTp or (drvOf(d).mCh and not(ff.dm.get(d).mCh)),
        terminalOverwritten: viol.terminalOverwritten or isTerminal(drvOf(d).mSt),
        dispatchNoInterest: viol.dispatchNoInterest or not(liveInterest(d))
      },
      reach' = { ...reach, failFast: true }
    }
  }

  /// Probe a Ready marked-Broken node whose walk already failed
  /// (substitute_tried) and whose live effective wanted outputs are ALL
  /// obtainable (present locally or available upstream): the Phase-1
  /// settlement arm — the model mirror of the production dispatch-probe
  /// partition's settlement cells (batch_probe_cached_ready, Wave 1
  /// T-1.2; the spec rule is sched.evidence.settlement). Pre-fix this cell
  /// either got NO action (all present — the D16 limbo: probeInline is
  /// gated on not-tried, the fail-fast arm on not-all-present, and
  /// admit_pull refuses must-substitute nodes) or the wrongful fail-fast
  /// (substitutable: the un-narrowed tried arm). Post-fix it re-enters
  /// the substitution walk — the closure-re-verifying "verification
  /// walk": its ok completion routes through consumeWalk's ok arm
  /// (complete-from-store), its genuine failure lands at the Broken arm
  /// with the one-shot spent and fail-fasts there. The model's
  /// single-step MReady -> MSubstituting respawn abstracts the code's
  /// spawn (Ready -> Substituting is a valid production transition at
  /// this call site — no revert needed, unlike the Broken-arm respawn).
  action probeSettleTried(d: str): bool = all {
    leaderActive,
    drvOf(d).mSt == MReady,
    liveInterest(d),
    mustSub(d),
    drvOf(d).tried,
    settleArmAvailable(d),
    drvOf(d).walks.length() < WALKS_PER_NODE,
    drv' = drv.set(d, { ...drvOf(d),
      mSt: MSubstituting, probed: true,
      probedKind: if (allWantedPresent(d)) PKPresent else PKSubst,
      walks: drvOf(d).walks.append({ forgivable: OUTIDXSET.exclude(effWanted(d)).exclude(drvOf(d).nf), tenure: lead.gen, res: WPending }) }),
    viol' = { ...viol, dispatchNoInterest: viol.dispatchNoInterest or not(liveInterest(d)) },
    store' = store, blds' = blds, memEdges' = memEdges, pgEdges' = pgEdges,
    intents' = intents, lead' = lead, budget' = budget, reach' = reach
  }

  /// Probe an unmarked Ready node and get a confirmed-missing verdict:
  /// it stays Ready for the normal from-source path, now probed. The C2
  /// latch lives at this routing decision: demoting a node ALL of whose
  /// missing wanted outputs are available upstream is wrongful unless the
  /// genuine-walk-failure one-shot applies (encoding decision ENC-0b-9 —
  /// post-decision upstream publication races are not demotions).
  action probeMissingUnmarked(d: str): bool = all {
    leaderActive,
    drvOf(d).mSt == MReady,
    liveInterest(d),
    not(mustSub(d)),
    effWanted(d).exists(i => pMissAnswerable(store, (d, i))),
    not(drvOf(d).probed and drvOf(d).probedKind == PKMissing),
    drv' = drv.set(d, { ...drvOf(d), probed: true, probedKind: PKMissing }),
    viol' = { ...viol,
      dispatchNoInterest: viol.dispatchNoInterest or not(liveInterest(d)),
      wrongfulDemotion: viol.wrongfulDemotion or and {
        effWanted(d).exists(i => not(store.get((d, i)).present)),
        effWanted(d).forall(i => store.get((d, i)).present or store.get((d, i)).upHas),
        not(drvOf(d).tried and drvOf(d).triedGenuine)
      }
    },
    store' = store, blds' = blds, memEdges' = memEdges, pgEdges' = pgEdges,
    intents' = intents, lead' = lead, budget' = budget, reach' = reach
  }

  // --- 7g. the substitution walk ---------------------------------------------
  //
  // Staleness grounding (C3-adjudication model correction): the walk is a
  // detached serial loop whose verdict is determined by what it observed
  // at CHECK time, while the actor learns it at SEND time —
  //
  //   walkFinishes  = the walk task's last presence/availability check:
  //                   the verdict (and, for ok walks, the ingest) is fixed
  //                   here, mid-flight. The task itself is still live.
  //   consumeWalk   = the task sends SubstituteComplete AND the actor
  //                   processes it, as one step.
  //   dropStaleWalkResult = the same send, processed by the actor's
  //                   not-Substituting drop guard.
  //
  // The window between walkFinishes and consumeWalk is therefore the TAIL
  // OF THE WALK'S OWN DURATION (after its last check, before its send) —
  // not an actor-mailbox delay. Anything that interleaves there happened
  // while the walk was still running, so a SubstituteComplete can be
  // stale only because the world changed DURING the walk. Modeling the
  // send and the processing as separate steps would instead allow
  // mailbox reorderings that the actor's FIFO mailbox excludes (a message
  // enqueued after the completion — e.g. a directed resubmit issued in
  // response to the build failure — can never be processed before it);
  // the refuted single-build C3 trace of the 0c record relied on exactly
  // such a reordering. See the invariant map's correction record.

  /// The walk task's serial check loop completes (environment): ok walks
  /// ingest the non-forgiven seed outputs and the REFS-descendant closure
  /// (every ingested output must be present already or available
  /// upstream); failed walks require a genuinely
  /// missing-and-unavailable non-forgivable output AT CHECK TIME
  /// (transient-only failures are below this model's resolution; encoding
  /// decision ENC-0b-5). The verdict recorded here is what the walk will
  /// deliver when it sends — the world may keep changing until then.
  action walkFinishes(d: str): bool = {
    nondet idx = Set(0, 1).oneOf()
    nondet okPick = Set(true, false).oneOf()
    val w = pWalkAt(drvOf(d).walks, idx)
    // the forgiven set ranges over subsets of the spawn-time forgivable
    // snapshot directly (the previous OUTIDX-powerset pick intersected
    // with it produced the same successor set with more enumeration).
    nondet forgivenPick = w.forgivable.powerset().oneOf()
    val forgiven = forgivenPick
    val descendants = pClosure(REFS, pKids(REFS, d))
    val ingestOuts = OUTIDX.exclude(forgiven).map(i => (d, i))
      .union(tuples(descendants, OUTIDX))
    val genuineFail = OUTIDX.exclude(w.forgivable).exists(i =>
      not(store.get((d, i)).present) and not(store.get((d, i)).upHas))
    all {
      idx < drvOf(d).walks.length(),
      w.res == WPending,
      // ok walks can only ingest what exists somewhere; failed walks need a
      // genuine missing non-forgivable output
      if (okPick) ingestOuts.forall(o => store.get(o).present or store.get(o).upHas) else genuineFail,
      drv' = drv.set(d, { ...drvOf(d),
        walks: drvOf(d).walks.replaceAt(idx, { ...w, res: WDone({ ok: okPick, forgiven: forgiven, genuine: not(okPick) }) }) }),
      store' = if (okPick)
        ingestOuts.fold(store, (acc, o) => acc.set(o, { ...acc.get(o), present: true, everPresent: true }))
        else store,
      blds' = blds, memEdges' = memEdges, pgEdges' = pgEdges,
      intents' = intents, lead' = lead, budget' = budget, viol' = viol, reach' = reach
    }
  }

  /// A walk task's send lands on a node that is no longer Substituting:
  /// the handler's drop guard discards the completion (and a still-pending
  /// task orphaned the same way will meet this guard when it eventually
  /// sends; orphaned tasks die with their tenure). This is the
  /// neutralization arm the cancel passes rely on — a cancelled node's
  /// in-flight walk ends here, never at consumeWalk.
  action dropStaleWalkResult(d: str): bool = all {
    drvOf(d).mSt != MSubstituting,
    drvOf(d).walks != [],
    drv' = drv.set(d, { ...drvOf(d), walks: [] }),
    store' = store, blds' = blds, memEdges' = memEdges, pgEdges' = pgEdges,
    intents' = intents, lead' = lead, budget' = budget, viol' = viol, reach' = reach
  }

  /// The walk task sends SubstituteComplete and the actor processes it
  /// (handle_substitute_complete), one step — the send-time end of the
  /// staleness window (see the section comment above). The routing
  /// re-reads the CURRENT evidence and wanted set: forgiven-now-wanted
  /// downgrade (re-spawn or revert, growing never-forgive), ok
  /// completion, Vouched lazy clear, Pending suppression, Broken
  /// SETTLEMENT (Phase 1: re-probe, then verification-respawn or genuine
  /// fail-fast). The verdict it routes on was fixed at check time
  /// (walkFinishes), so the routing can act on evidence that is stale by
  /// exactly the walk's own duration — the C3 mechanism whose terminal
  /// consequence the settlement re-probe now removes: a stale ok=false
  /// verdict for a node whose live-wanted outputs are obtainable at
  /// consumption time spends the one-shot and spawns the verification
  /// walk instead of terminally failing the interested builds.
  action consumeWalk(d: str): bool = {
    nondet idx = Set(0, 1).oneOf()
    val w = pWalkAt(drvOf(d).walks, idx)
    val res = match w.res { | WDone(r) => r | WPending => { ok: false, forgiven: OUTIDX.exclude(OUTIDX), genuine: false } }
    val crossTenure = w.tenure < lead.gen
    val baseRec = { ...drvOf(d), walks: drvOf(d).walks.select(ww => ww != w), walkConsumed: drvOf(d).walkConsumed + 1 }
    val forgivenNowWanted = res.forgiven.intersect(effWanted(d))
    val revertTo = pRevertTarget(drv, memEdges, d)
    val ev = evidence(d)
    // FC-5 ceiling-headroom tracking: is this the node's 2nd+ consumption?
    val secondConsume = drvOf(d).walkConsumed >= 1
    // downgrade ingredients
    val grownNf = baseRec.nf.union(forgivenNowWanted)
    val respawn = revertTo == MDepFailed or mustSub(d)
    // ok-completion ingredients
    val descendants = pClosure(REFS, pKids(REFS, d))
    val walkedOuts = OUTIDX.exclude(res.forgiven).map(i => (d, i)).union(tuples(descendants, OUTIDX))
    val dmProduced = drv.set(d, { ...baseRec, mSt: MProduced,
      pgSt: if (baseRec.rowExists) MProduced else baseRec.pgSt, nf: OUTIDX.exclude(OUTIDX) })
    val cascade = pPromoteAndClear(dmProduced, memEdges, DRVS, Set(d))
    val okIntents = cascade.cleared.map(p => { op: IClearBoth(p), tenure: lead.gen })
    // broken-arm ingredients: the settlement re-probe (Phase 1) and the
    // genuine fail-fast it narrows to
    val withTried = drv.set(d, { ...baseRec, tried: true, triedGenuine: res.genuine, mSt: MReady, nf: OUTIDX.exclude(OUTIDX) })
    val ff = pFailFast(withTried, blds, BUILDS, memEdges, store, d)
    all {
      leaderActive,
      drvOf(d).mSt == MSubstituting,
      drvOf(d).walkConsumed < WALK_CONSUME_CEIL,
      idx < drvOf(d).walks.length(),
      w.res != WPending,
      crossTenure implies CROSS_TENURE_WALKS,
      if (forgivenNowWanted != Set()) all {
        // downgrade: grow the chain-scoped never-forgive set; re-spawn when
        // the revert target is DepFailed or the node is must-substitute,
        // else revert and arm the one-shot (no genuine-failure exemption;
        // the stale probe verdict is dropped so a fresh probe must precede
        // any from-source attempt)
        drv' = drv.set(d, if (respawn)
          { ...baseRec, nf: grownNf,
            walks: baseRec.walks.append({ forgivable: OUTIDXSET.exclude(effWanted(d)).exclude(grownNf), tenure: lead.gen, res: WPending }) }
          else { ...baseRec, nf: OUTIDX.exclude(OUTIDX), mSt: revertTo, tried: true, triedGenuine: false,
            probed: false, probedKind: PKNone }),
        intents' = intents,
        blds' = blds,
        viol' = { ...viol,
          downgradeNoGrowth: viol.downgradeNoGrowth or forgivenNowWanted.subseteq(baseRec.nf) },
        reach' = { ...reach,
          walkFailConsumed: true,
          downgradeRespawned: reach.downgradeRespawned or respawn,
          secondWalkConsumed: reach.secondWalkConsumed or secondConsume,
          crossTenureWalkConsumed: reach.crossTenureWalkConsumed or crossTenure }
      } else if (res.ok) all {
        // ok: complete the node, end the chain, cascade C2 clears + promotes
        intents.size() + okIntents.size() <= INTENT_CEIL,
        drv' = cascade.dm,
        intents' = intents.union(okIntents),
        blds' = blds,
        viol' = { ...viol,
          walkOkClosureNeverPresent: viol.walkOkClosureNeverPresent or
            walkedOuts.exists(o => not(store.get(o).everPresent)),
          walkOkClosureGoneAtConsume: viol.walkOkClosureGoneAtConsume or
            walkedOuts.exists(o => not(store.get(o).present)),
          producedWithoutPresence: viol.producedWithoutPresence or
            effWanted(d).exists(i => not(store.get((d, i)).everPresent)),
          brokenVouched: viol.brokenVouched or cascade.cleared.exists(p =>
            pKids(memEdges, p) == Set() or cascade.dm.get(p).mCh)
        },
        reach' = { ...reach,
          walkOkConsumed: true,
          walkOkWithForgiven: reach.walkOkWithForgiven or res.forgiven != Set(),
          secondWalkConsumed: reach.secondWalkConsumed or secondConsume,
          crossTenureWalkConsumed: reach.crossTenureWalkConsumed or crossTenure }
      } else if (ev == EvVouched) all {
        // failed walk on Vouched evidence: the C4 lazy clear (mark only), revert
        intents.size() + 1 <= INTENT_CEIL,
        drv' = drv.set(d, { ...baseRec, mTp: false, mSt: revertTo, tried: true, triedGenuine: res.genuine,
          nf: OUTIDX.exclude(OUTIDX), probedKind: PKMissing }),
        intents' = intents.union(Set({ op: IClearMark(d), tenure: lead.gen })),
        blds' = blds,
        viol' = { ...viol, clearUnsound: viol.clearUnsound or not(vouchedNow(d)) },
        reach' = { ...reach, walkFailConsumed: true,
          secondWalkConsumed: reach.secondWalkConsumed or secondConsume,
          crossTenureWalkConsumed: reach.crossTenureWalkConsumed or crossTenure }
      } else if (ev == EvPending or not(drvOf(d).mTp)) all {
        // failed walk, Pending evidence (keep the mark) or unmarked node:
        // revert and arm the one-shot; the genuine failure is itself a
        // missing-confirmed observation
        drv' = drv.set(d, { ...baseRec, mSt: revertTo, tried: true, triedGenuine: res.genuine,
          nf: OUTIDX.exclude(OUTIDX), probedKind: PKMissing }),
        intents' = intents,
        blds' = blds,
        viol' = viol,
        reach' = { ...reach, walkFailConsumed: true,
          secondWalkConsumed: reach.secondWalkConsumed or secondConsume,
          crossTenureWalkConsumed: reach.crossTenureWalkConsumed or crossTenure }
      } else if (not(drvOf(d).tried) and settleArmAvailable(d)) all {
        // failed walk on a marked Broken node, verification one-shot
        // unspent, every live-wanted output obtainable at consumption
        // time: the Phase-1 SETTLEMENT re-probe (sched.evidence.
        // settlement / Wave-1 T-1.4's settle_broken_marked_root). The
        // verdict in hand was fixed at the walk's own check time and can
        // be stale by the walk's full duration (the C3 defect class);
        // instead of consuming it as a terminal verdict, spend the
        // one-shot and re-spawn the closure-re-verifying walk. The
        // model's single-step respawn (stay MSubstituting, fresh pending
        // walk) abstracts the code's revert-for-respawn — production has
        // no Substituting->Substituting edge, so it transitions to
        // revert_target_for(..) first and then spawns (review findings
        // CF-1/FC-1); the post-spawn state is the same either way.
        drv' = drv.set(d, { ...baseRec,
          tried: true, triedGenuine: res.genuine, mSt: MSubstituting,
          probed: true, probedKind: if (allWantedPresent(d)) PKPresent else PKSubst,
          walks: baseRec.walks.append({ forgivable: OUTIDXSET.exclude(effWanted(d)).exclude(baseRec.nf), tenure: lead.gen, res: WPending }) }),
        intents' = intents,
        blds' = blds,
        viol' = viol,
        reach' = { ...reach, walkFailConsumed: true,
          secondWalkConsumed: reach.secondWalkConsumed or secondConsume,
          crossTenureWalkConsumed: reach.crossTenureWalkConsumed or crossTenure }
      } else all {
        // failed walk on a marked Broken node, the GENUINE fail-fast arm:
        // the verification one-shot is already spent (this IS the
        // verification walk's own failure), or some live-wanted output
        // is confirmed unobtainable at consumption time.
        intents.size() + 1 <= INTENT_CEIL,
        drv' = ff.dm,
        blds' = ff.bm,
        intents' = intents.union(Set({ op: IClearMark(d), tenure: lead.gen })),
        viol' = { ...viol,
          wrongfulTerminalFailure: viol.wrongfulTerminalFailure or ff.wrongful,
          ffBadEffect: viol.ffBadEffect or (ff.fired and (ff.dm.get(d).mTp or (baseRec.mCh and not(ff.dm.get(d).mCh)))) },
        reach' = { ...reach, walkFailConsumed: true, failFast: reach.failFast or ff.fired,
          secondWalkConsumed: reach.secondWalkConsumed or secondConsume,
          crossTenureWalkConsumed: reach.crossTenureWalkConsumed or crossTenure }
      },
      store' = store, memEdges' = memEdges, pgEdges' = pgEdges, lead' = lead, budget' = budget
    }
  }

  // --- 7h. attempts (the pull path) ------------------------------------------

  /// admit_pull DeliverNew + the fenced mint: Ready, not must-substitute,
  /// probed this pass; opening pins the node's child outputs.
  action attemptOpen(d: str): bool = all {
    leaderActive,
    drvOf(d).mSt == MReady,
    liveInterest(d),
    not(mustSub(d)),
    drvOf(d).probed,
    not(drvOf(d).attemptOpen),
    drvOf(d).attemptCycles < ATTEMPT_CYCLE_CEIL,
    val pinOuts = tuples(memKids(d), OUTIDX)
    all {
      drv' = drv.set(d, { ...drvOf(d), mSt: MOpen, pgSt: if (drvOf(d).rowExists) MOpen else drvOf(d).pgSt,
        attemptOpen: true, attemptCycles: drvOf(d).attemptCycles + 1 }),
      store' = pinOuts.fold(store, (acc, o) => acc.set(o, { ...acc.get(o), pinned: true })),
      viol' = { ...viol,
        doomedDispatch: viol.doomedDispatch or (drvOf(d).mTp and evidence(d) == EvBroken),
        mintedWhileMustSub: viol.mintedWhileMustSub or mustSub(d),
        dispatchUnprobed: viol.dispatchUnprobed or not(drvOf(d).probed),
        dispatchNoInterest: viol.dispatchNoInterest or not(liveInterest(d)),
        indetDemoted: viol.indetDemoted or drvOf(d).probedKind == PKIndet,
        staleProducedUnlocked: viol.staleProducedUnlocked or memKids(d).exists(c =>
          drvOf(c).mSt == MProduced and effWanted(c).exists(i => not(store.get((c, i)).present)))
      },
      reach' = { ...reach, triedDemotionUpstreamHas: reach.triedDemotionUpstreamHas or
        (drvOf(d).tried and effWanted(d).exists(i => not(store.get((d, i)).present) and store.get((d, i)).upHas)) },
      blds' = blds, memEdges' = memEdges, pgEdges' = pgEdges,
      intents' = intents, lead' = lead, budget' = budget
    }
  }

  /// The open attempt reports success: the node is produced from source
  /// (all declared outputs uploaded), pins released, C2 clears + promotes
  /// cascade to the parents.
  action attemptCloseSuccess(d: str): bool = all {
    leaderActive,
    drvOf(d).mSt == MOpen,
    drvOf(d).attemptOpen,
    val dOuts = OUTIDX.map(i => (d, i))
    val pinOuts = tuples(memKids(d), OUTIDX)
    val dm1 = drv.set(d, { ...drvOf(d), mSt: MProduced, pgSt: if (drvOf(d).rowExists) MProduced else drvOf(d).pgSt,
      attemptOpen: false, nf: Set() })
    val cascade = pPromoteAndClear(dm1, memEdges, DRVS, Set(d))
    val newIntents = cascade.cleared.map(p => { op: IClearBoth(p), tenure: lead.gen })
    all {
      intents.size() + newIntents.size() <= INTENT_CEIL,
      drv' = cascade.dm,
      store' = dOuts.fold(pinOuts.fold(store, (acc, o) => acc.set(o, { ...acc.get(o), pinned: false })),
        (acc, o) => acc.set(o, { ...acc.get(o), present: true, everPresent: true })),
      intents' = intents.union(newIntents),
      blds' = blds, memEdges' = memEdges, pgEdges' = pgEdges, lead' = lead, budget' = budget,
      viol' = { ...viol, brokenVouched: viol.brokenVouched or cascade.cleared.exists(p =>
        pKids(memEdges, p) == Set() or cascade.dm.get(p).mCh) },
      reach' = reach
    }
  }

  /// The open attempt reports a retryable failure: requeue with backoff
  /// (the poison verdict is the separate poisonArrival action).
  action attemptCloseFail(d: str): bool = all {
    leaderActive,
    drvOf(d).mSt == MOpen,
    drvOf(d).attemptOpen,
    val pinOuts = tuples(memKids(d), OUTIDX)
    all {
      drv' = drv.set(d, { ...drvOf(d), mSt: MFailed, pgSt: if (drvOf(d).rowExists) MFailed else drvOf(d).pgSt,
        attemptOpen: false }),
      store' = pinOuts.fold(store, (acc, o) => acc.set(o, { ...acc.get(o), pinned: false })),
      blds' = blds, memEdges' = memEdges, pgEdges' = pgEdges,
      intents' = intents, lead' = lead, budget' = budget, viol' = viol, reach' = reach
    }
  }

  /// Executor disconnect / reassign: the open attempt closes uncharged
  /// and the node returns to Ready without consulting the evidence.
  action reassignOpen(d: str): bool = all {
    leaderActive,
    drvOf(d).mSt == MOpen,
    drvOf(d).attemptOpen,
    val pinOuts = tuples(memKids(d), OUTIDX)
    all {
      drv' = drv.set(d, { ...drvOf(d), mSt: MReady, pgSt: if (drvOf(d).rowExists) MReady else drvOf(d).pgSt,
        attemptOpen: false }),
      store' = pinOuts.fold(store, (acc, o) => acc.set(o, { ...acc.get(o), pinned: false })),
      blds' = blds, memEdges' = memEdges, pgEdges' = pgEdges,
      intents' = intents, lead' = lead, budget' = budget, viol' = viol, reach' = reach
    }
  }

  /// Retry backoff elapses: Failed -> Ready.
  action requeueFailed(d: str): bool = all {
    leaderActive,
    drvOf(d).mSt == MFailed,
    drv' = drv.set(d, { ...drvOf(d), mSt: MReady, pgSt: if (drvOf(d).rowExists) MReady else drvOf(d).pgSt }),
    store' = store, blds' = blds, memEdges' = memEdges, pgEdges' = pgEdges,
    intents' = intents, lead' = lead, budget' = budget, viol' = viol, reach' = reach
  }

  // --- 7i. inbound poison machinery -------------------------------------------

  /// A poison verdict lands on a node (retry threshold, admin, fleet
  /// exhaust — uninterpreted): the node goes Poisoned, the cascade flips
  /// its in-DAG ancestors to DependencyFailed, and — the
  /// terminal_failure_epilogue's completion check, run in the same
  /// handler — every live build holding a now-terminal member fails.
  /// Bits untouched. The cascade reaches only ancestors that have not
  /// started yet (Queued/Ready) — production cascade_dependency_failure
  /// skips Substituting/Assigned/Running/Failed parents, whose in-flight
  /// walk or attempt keeps its chance and settles them via its own
  /// completion (Phase 0c encoding fix; see the invariant map).
  ///
  /// Each failed build then runs handle_derivation_failure's
  /// keep_going=false arm, which is cancel_build_derivations: every
  /// member owned EXCLUSIVELY by the failing builds (no still-active
  /// build also holds interest) that is not itself in the hit set and
  /// not yet terminal goes Cancelled — including Substituting members,
  /// whose in-flight walk is thereby orphaned (its completion is later
  /// dropped by the not-Substituting guard). This pass is the
  /// C3-adjudication model correction: without it a sole-interest
  /// Substituting node survived its build's failure with a live,
  /// consumable stale walk, which is not as-built behavior (it mirrors
  /// cancelBuild / pFailFast's cancel pass; production
  /// build.rs:cancel_build_derivations).
  action poisonArrival(d: str): bool = all {
    leaderActive,
    budget.poisons > 0,
    drvOf(d).mSt == MReady or drvOf(d).mSt == MOpen or drvOf(d).mSt == MFailed or drvOf(d).mSt == MQueued,
    val ancestors = 0.to(4).fold(Set(d), (acc, _i) => acc.union(acc.fold(Set(), (a2, x) => a2.union(pParents(memEdges, x))))).exclude(Set(d))
    val cascaded = ancestors.filter(x => drv.get(x).mSt == MQueued or drv.get(x).mSt == MReady)
    val hitSet = Set(d).union(cascaded)
    val pinOuts = tuples(memKids(d), OUTIDX)
    // the builds this verdict terminally fails (terminal_failure_epilogue)
    val failedBuilds = BUILDS.filter(b =>
      blds.get(b).phase == BpActive and blds.get(b).interest.intersect(hitSet) != Set())
    // cancel_build_derivations pass over the failing builds' members:
    // exclusively-owned (every active build holding the member is itself
    // failing — production removes interest build-by-build, so a member
    // shared only among failing builds ends up sole-interest for the last
    // one), un-hit, non-terminal members go Cancelled.
    val cancelled = DRVS.filter(m => and {
      not(hitSet.contains(m)),
      not(isTerminal(drv.get(m).mSt)),
      drv.get(m).mSt != MAbsent,
      failedBuilds.exists(b => blds.get(b).interest.contains(m)),
      BUILDS.forall(b2 => or {
        failedBuilds.contains(b2),
        not(blds.get(b2).phase == BpActive and blds.get(b2).interest.contains(m))
      })
    })
    all {
      drv' = DRVS.mapBy(x => {
        val r = drv.get(x)
        if (x == d)
          { ...r, mSt: MPoisoned, pgSt: if (r.rowExists) MPoisoned else r.pgSt, attemptOpen: false }
        else if (cascaded.contains(x))
          { ...r, mSt: MDepFailed, pgSt: if (r.rowExists) MDepFailed else r.pgSt }
        else if (cancelled.contains(x))
          { ...r, mSt: MCancelled, pgSt: if (r.rowExists) MCancelled else r.pgSt }
        else r
      }),
      store' = if (drvOf(d).attemptOpen) pinOuts.fold(store, (acc, o) => acc.set(o, { ...acc.get(o), pinned: false })) else store,
      budget' = { ...budget, poisons: budget.poisons - 1 },
      blds' = BUILDS.mapBy(b => {
        val br = blds.get(b)
        if (br.phase == BpActive and br.interest.intersect(hitSet) != Set())
          { ...br, phase: BpFailed, cleanup: true }
        else br
      }),
      memEdges' = memEdges, pgEdges' = pgEdges,
      intents' = intents, lead' = lead, viol' = viol, reach' = reach
    }
  }

  /// Poison resolution (admin ClearPoison or the TTL sweep): the node is
  /// removed from the DAG, its row reset, every surviving parent gets
  /// the closure-hole breadcrumb (H2/H3), and the survivors are
  /// re-evaluated (the production reevaluate_removal_survivors hook —
  /// Phase-1 Wave 2b, sched.poison.clear-survivor-reevaluation): a
  /// MQueued survivor whose remaining loaded kids are all produced
  /// (vacuously, when the cleared child was its last one) is promoted to
  /// MReady — the same promotion arm reapTerminalBuild applies to its
  /// survivors. This is the wake-up edge for a parent the recovery
  /// condemnation spared on co-ownership grounds (it recovered MQueued
  /// above this non-co-owned poisoned child); without it that parent is
  /// stranded Queued/childless/un-armed and L3
  /// (liveBuildTerminalOrProgressArmed) is violated — the refuted 0d
  /// strand made real by the scoping. The settlement arm for
  /// marked-Broken survivors needs no separate encoding here: a promoted
  /// (or already-) MReady marked survivor is exactly what the
  /// dispatch-partition letters (probeWalk / probeFailFast*) fire on,
  /// and an MFailed survivor re-arms via requeueFailed.
  action poisonClear(d: str): bool = {
    nondet viaAdmin = Set(true, false).oneOf()
    all {
      leaderActive,
      drvOf(d).mSt == MPoisoned,
      val parents = memParentsOf(d).filter(p => drvOf(p).mSt != MAbsent)
      val newIntents = parents.map(p => { op: ISetHole(p), tenure: lead.gen })
      // The post-removal edge relation: the cleared child's edges are gone,
      // so the survivor promotion judges the remaining kids only.
      val memEdges1 = memEdges.filter(e => e._1 != d and e._2 != d)
      all {
        intents.size() + newIntents.size() <= INTENT_CEIL,
        drv' = DRVS.mapBy(x => {
          val r = drv.get(x)
          if (x == d) { ...r, mSt: MAbsent, pgSt: if (r.rowExists) MQueued else r.pgSt, walks: [], nf: Set() }
          else if (parents.contains(x)) {
            // Hole stamp + the survivor promotion arm (mirrors
            // reapTerminalBuild's dmFinal pass; mSt-only like that pass —
            // recovery re-derives dispatchability from the dep walk, so
            // the durable status is immaterial here).
            val promoted = r.mSt == MQueued and
              pKids(memEdges1, x).forall(c => drv.get(c).mSt == MProduced)
            { ...r, mCh: true, mSt: if (promoted) MReady else r.mSt }
          }
          else r
        }),
        memEdges' = memEdges1,
        intents' = intents.union(newIntents),
        store' = store, blds' = blds, pgEdges' = pgEdges, lead' = lead, budget' = budget,
        viol' = { ...viol, holeUnsound: viol.holeUnsound or parents.exists(p => drvOf(d).mSt == MProduced) },
        reach' = { ...reach,
          holeAdmin: reach.holeAdmin or (viaAdmin and parents != Set()),
          holeTtl: reach.holeTtl or (not(viaAdmin) and parents != Set()) }
      }
    }
  }

  // --- 7j. durable-state GC (adversarial-store regime) -------------------------

  /// The orphan-terminal row GC: a terminal, link-free, assignment-free
  /// row is deleted together with its durable edges — the only pgEdges
  /// delete in the alphabet.
  action orphanRowGc(d: str): bool = all {
    budget.rowGc > 0,
    drvOf(d).rowExists,
    isTerminal(drvOf(d).pgSt),
    not(drvOf(d).attemptOpen),
    BUILDS.forall(b => not(blds.get(b).pgLinks.contains(d))),
    drv' = drv.set(d, { ...drvOf(d), rowExists: false, pgTp: false, pgCh: false, pgTpGen: 0, pgChGen: 0, pgWanted: Set() }),
    pgEdges' = pgEdges.filter(e => e._1 != d and e._2 != d),
    budget' = { ...budget, rowGc: budget.rowGc - 1 },
    store' = store, blds' = blds, memEdges' = memEdges,
    intents' = intents, lead' = lead, viol' = viol, reach' = reach
  }

  /// The out-of-tree builds-row purge (the AW1 conjunction's second leg):
  /// deletes a terminal build's row and its build_derivations links.
  action buildsRowPurge(b: str): bool = all {
    budget.purges > 0,
    blds.get(b).pgRow,
    blds.get(b).phase == BpDoneOk or blds.get(b).phase == BpFailed or blds.get(b).phase == BpCancelled,
    blds' = blds.set(b, { ...blds.get(b), pgRow: false, pgLinks: Set() }),
    budget' = { ...budget, purges: budget.purges - 1 },
    drv' = drv, store' = store, memEdges' = memEdges, pgEdges' = pgEdges,
    intents' = intents, lead' = lead, viol' = viol, reach' = reach
  }

  // --- 8. init ------------------------------------------------------------
  // Reduction-ladder step CE-L1 (invariant-map Phase 0c stage record): the
  // initial store/upstream assignment is drawn PER DERIVATION (both outputs
  // of a node start with the same presence and the same upstream
  // availability) instead of per output. Every per-output-asymmetric
  // configuration stays reachable through the per-output environment
  // letters (storeIngestAt / upstreamChangeAt / storeGcAt / walk ingest /
  // attemptCloseSuccess), so no design-§4 falsification or witness depends
  // on an asymmetry only the initial assignment could provide; the cut is
  // 2^(2|D||O|) -> 2^(2|D|) initial states.

  action init = {
    nondet presentPick = DRVS.powerset().oneOf()
    nondet upstreamPick = DRVS.powerset().oneOf()
    all {
      drv' = DRVS.mapBy(d => freshDrv),
      store' = tuples(DRVS, OUTIDX).mapBy(o => {
        present: presentPick.contains(o._1), everPresent: presentPick.contains(o._1),
        pinned: false, upHas: upstreamPick.contains(o._1), upFlipped: false,
        ingested: false, cache: CNone }),
      blds' = BUILDS.mapBy(b => freshBuild(DRVS)),
      memEdges' = Set(),
      pgEdges' = Set(),
      intents' = Set(),
      lead' = { up: true, gen: 1, recovered: true },
      budget' = { subs: SUBMISSION_BUDGET, pgDrops: PGDROP_BUDGET, failovers: FAILOVERS,
        storeGc: STORE_GC_BUDGET, rowGc: ROW_GC_BUDGET, purges: PURGE_BUDGET,
        poisons: POISON_BUDGET, cancels: CANCEL_BUDGET },
      viol' = noViol,
      reach' = noReach
    }
  }

  // --- 9. step --------------------------------------------------------------
  // The transition relation is grouped by which nondeterministic argument
  // each action consumes, so the checker enumerates each argument domain
  // only for the actions that read it instead of taking the full
  // node x build x output cross product for every action. The successor
  // relation is identical to the flat form (each disjunct reads only its
  // own bound variable); only the successor-generation cost changes.
  // Recorded in the invariant map's Phase 0c stage record.

  /// Per-output environment letters.
  action stepStore = {
    nondet o = tuples(DRVS, OUTIDX).oneOf()
    any {
      storeIngestAt(o),
      upstreamChangeAt(o),
      storeGcAt(o)
    }
  }

  /// Per-build letters (merge, lifecycle, reap, purge).
  action stepBuild = {
    nondet b = BUILDS.oneOf()
    any {
      mergeCommit(b),
      mergeRollback(b),
      cancelBuild(b),
      buildCompletes(b),
      buildFails(b),
      reapTerminalBuild(b),
      buildsRowPurge(b)
    }
  }

  /// Per-derivation letters (dispatch probe partition, walks, attempts,
  /// poison machinery, row GC).
  action stepDrv = {
    nondet d = DRVS.oneOf()
    any {
      probeInline(d),
      probeWalk(d),
      probeFailFastMissing(d),
      probeFailFastTried(d),
      probeSettleTried(d),
      probeMissingUnmarked(d),
      walkFinishes(d),
      consumeWalk(d),
      dropStaleWalkResult(d),
      attemptOpen(d),
      attemptCloseSuccess(d),
      attemptCloseFail(d),
      reassignOpen(d),
      requeueFailed(d),
      poisonArrival(d),
      poisonClear(d),
      orphanRowGc(d)
    }
  }

  action step = any {
    stepStore,
    stepBuild,
    stepDrv,
    // PG intent plumbing
    pgApplyAny,
    pgDropAny,
    // lease
    leaderLost,
    recoverAsLeader
  }
}

// ===========================================================================
// Regimes (design §2.6). One thin instantiation module per wired check;
// nix/quint.nix wires each in Phase 0c. The vertical-slice module is the
// §2b.1 measurement vehicle and stays for regression tracking.
// ===========================================================================

/// Vertical slice (§2b.1 measurement milestone): 2 derivations, 1 build,
/// base-regime fault alphabet.
module closureEvidenceSlice {
  import closureEvidence(
    DRVS = Set("d1", "d2"),
    BUILDS = Set("b1"),
    REFS = Set(("d1", "d2")),
    SUBMISSION_BUDGET = 2,
    FAILOVERS = 0,
    PGDROP_BUDGET = 0,
    STALE_APPLY = false,
    CROSS_TENURE_WALKS = false,
    STORE_GC_BUDGET = 0,
    ROW_GC_BUDGET = 0,
    PURGE_BUDGET = 0,
    POISON_BUDGET = 1,
    CANCEL_BUDGET = 1,
    WALK_CONSUME_CEIL = 2,
    ATTEMPT_CYCLE_CEIL = 2,
    INTENT_CEIL = 3,
    WALKS_PER_NODE = 1,
    STORE_PER_OUTPUT = false
  ).*
}

/// Base regime: single tenure, no faults, no GC. Every exported
/// invariant must hold; the expected-fail probes are wired separately.
module closureEvidenceBase {
  import closureEvidence(
    DRVS = Set("d1", "d2", "d3", "d4"),
    BUILDS = Set("b1", "b2"),
    REFS = Set(("d1", "d2"), ("d1", "d3"), ("d2", "d4"), ("d3", "d4")),
    SUBMISSION_BUDGET = 3,
    FAILOVERS = 0,
    PGDROP_BUDGET = 0,
    STALE_APPLY = false,
    CROSS_TENURE_WALKS = false,
    STORE_GC_BUDGET = 0,
    ROW_GC_BUDGET = 0,
    PURGE_BUDGET = 0,
    POISON_BUDGET = 2,
    CANCEL_BUDGET = 1,
    WALK_CONSUME_CEIL = 2,
    ATTEMPT_CYCLE_CEIL = 2,
    INTENT_CEIL = 3,
    WALKS_PER_NODE = 2,
    STORE_PER_OUTPUT = true
  ).*
}

/// Fault-persist regime: best-effort PG writes may be lost.
module closureEvidenceFaultPersist {
  import closureEvidence(
    DRVS = Set("d1", "d2", "d3", "d4"),
    BUILDS = Set("b1", "b2"),
    REFS = Set(("d1", "d2"), ("d1", "d3"), ("d2", "d4"), ("d3", "d4")),
    SUBMISSION_BUDGET = 3,
    FAILOVERS = 0,
    PGDROP_BUDGET = 2,
    STALE_APPLY = false,
    CROSS_TENURE_WALKS = false,
    STORE_GC_BUDGET = 0,
    ROW_GC_BUDGET = 0,
    PURGE_BUDGET = 0,
    POISON_BUDGET = 2,
    CANCEL_BUDGET = 1,
    WALK_CONSUME_CEIL = 2,
    ATTEMPT_CYCLE_CEIL = 2,
    INTENT_CEIL = 3,
    WALKS_PER_NODE = 2,
    STORE_PER_OUTPUT = true
  ).*
}

/// Failover regime: lost writes plus up to two failovers and recoveries.
module closureEvidenceFailover {
  import closureEvidence(
    DRVS = Set("d1", "d2", "d3", "d4"),
    BUILDS = Set("b1", "b2"),
    REFS = Set(("d1", "d2"), ("d1", "d3"), ("d2", "d4"), ("d3", "d4")),
    SUBMISSION_BUDGET = 3,
    FAILOVERS = 2,
    PGDROP_BUDGET = 2,
    STALE_APPLY = false,
    CROSS_TENURE_WALKS = false,
    STORE_GC_BUDGET = 0,
    ROW_GC_BUDGET = 0,
    PURGE_BUDGET = 0,
    POISON_BUDGET = 2,
    CANCEL_BUDGET = 1,
    WALK_CONSUME_CEIL = 2,
    ATTEMPT_CYCLE_CEIL = 2,
    INTENT_CEIL = 3,
    WALKS_PER_NODE = 2,
    STORE_PER_OUTPUT = true
  ).*
}

/// Stale-tenure regime: deposed-believer intents survive the successor's
/// recovery and an older tenure's walk completion may be consumed by a
/// newer one. The A17/A18 expected-fail probes live here.
module closureEvidenceStaleTenure {
  import closureEvidence(
    DRVS = Set("d1", "d2", "d3", "d4"),
    BUILDS = Set("b1", "b2"),
    REFS = Set(("d1", "d2"), ("d1", "d3"), ("d2", "d4"), ("d3", "d4")),
    SUBMISSION_BUDGET = 3,
    FAILOVERS = 2,
    PGDROP_BUDGET = 2,
    STALE_APPLY = true,
    CROSS_TENURE_WALKS = true,
    STORE_GC_BUDGET = 0,
    ROW_GC_BUDGET = 0,
    PURGE_BUDGET = 0,
    POISON_BUDGET = 2,
    CANCEL_BUDGET = 1,
    WALK_CONSUME_CEIL = 2,
    ATTEMPT_CYCLE_CEIL = 2,
    INTENT_CEIL = 5,
    WALKS_PER_NODE = 2,
    STORE_PER_OUTPUT = true
  ).*
}

/// Adversarial-store regime: store GC (including after a vouch),
/// upstream changes, the orphan-terminal row GC and the external
/// builds-row purge. The B2-strong / GC-after-vouch questions are
/// answered here.
module closureEvidenceAdversarialStore {
  import closureEvidence(
    DRVS = Set("d1", "d2", "d3", "d4"),
    BUILDS = Set("b1", "b2"),
    REFS = Set(("d1", "d2"), ("d1", "d3"), ("d2", "d4"), ("d3", "d4")),
    SUBMISSION_BUDGET = 3,
    FAILOVERS = 2,
    PGDROP_BUDGET = 2,
    STALE_APPLY = true,
    CROSS_TENURE_WALKS = true,
    STORE_GC_BUDGET = 2,
    ROW_GC_BUDGET = 1,
    PURGE_BUDGET = 1,
    POISON_BUDGET = 2,
    CANCEL_BUDGET = 1,
    WALK_CONSUME_CEIL = 2,
    ATTEMPT_CYCLE_CEIL = 2,
    INTENT_CEIL = 5,
    WALKS_PER_NODE = 2,
    STORE_PER_OUTPUT = true
  ).*
}

// ===========================================================================
// Exhaustive-scope instantiations (Phase 0c reduction ladder; invariant-map
// 0c stage record). The design-scale modules above keep the §2b granularity
// and carry the witness / expected-fail-probe checks (reach tracking on,
// dual walks, per-output store letters); these reduced instantiations are
// the regimes the exhaustive invariant-conjunction checks run over —
// reductions: 2-node universe, one build slot, two submissions,
// per-derivation store letters, one outstanding walk, no reach tracking.
// Witness reachability at this scope is NOT claimed; the canReach* checks
// run against the design-scale modules.
// ===========================================================================

/// Exhaustive base regime: single tenure, no faults, no GC.
module closureEvidenceBaseEx {
  import closureEvidence(
    DRVS = Set("d1", "d2"),
    BUILDS = Set("b1"),
    REFS = Set(("d1", "d2")),
    SUBMISSION_BUDGET = 2,
    FAILOVERS = 0,
    PGDROP_BUDGET = 0,
    STALE_APPLY = false,
    CROSS_TENURE_WALKS = false,
    STORE_GC_BUDGET = 0,
    ROW_GC_BUDGET = 0,
    PURGE_BUDGET = 0,
    POISON_BUDGET = 1,
    CANCEL_BUDGET = 1,
    // Phase 1 (FC-5): admits original walk + the settlement's verification
    // walk + one downgrade re-spawn; canReachVerificationWalkConsumed pins it.
    WALK_CONSUME_CEIL = 3,
    ATTEMPT_CYCLE_CEIL = 2,
    INTENT_CEIL = 3,
    WALKS_PER_NODE = 1,
    STORE_PER_OUTPUT = false
  ).*
}

/// Exhaustive fault-persist regime: best-effort PG writes may be lost.
module closureEvidenceFaultPersistEx {
  import closureEvidence(
    DRVS = Set("d1", "d2"),
    BUILDS = Set("b1"),
    REFS = Set(("d1", "d2")),
    SUBMISSION_BUDGET = 2,
    FAILOVERS = 0,
    PGDROP_BUDGET = 2,
    STALE_APPLY = false,
    CROSS_TENURE_WALKS = false,
    STORE_GC_BUDGET = 0,
    ROW_GC_BUDGET = 0,
    PURGE_BUDGET = 0,
    POISON_BUDGET = 1,
    CANCEL_BUDGET = 1,
    // Phase 1 (FC-5): admits original walk + the settlement's verification
    // walk + one downgrade re-spawn; canReachVerificationWalkConsumed pins it.
    WALK_CONSUME_CEIL = 3,
    ATTEMPT_CYCLE_CEIL = 2,
    INTENT_CEIL = 3,
    WALKS_PER_NODE = 1,
    STORE_PER_OUTPUT = false
  ).*
}

/// Exhaustive failover regime: lost writes plus one failover/recovery.
module closureEvidenceFailoverEx {
  import closureEvidence(
    DRVS = Set("d1", "d2"),
    BUILDS = Set("b1"),
    REFS = Set(("d1", "d2")),
    SUBMISSION_BUDGET = 2,
    FAILOVERS = 1,
    PGDROP_BUDGET = 1,
    STALE_APPLY = false,
    CROSS_TENURE_WALKS = false,
    STORE_GC_BUDGET = 0,
    ROW_GC_BUDGET = 0,
    PURGE_BUDGET = 0,
    POISON_BUDGET = 1,
    CANCEL_BUDGET = 1,
    // Phase 1 (FC-5): admits original walk + the settlement's verification
    // walk + one downgrade re-spawn; canReachVerificationWalkConsumed pins it.
    WALK_CONSUME_CEIL = 3,
    ATTEMPT_CYCLE_CEIL = 2,
    INTENT_CEIL = 3,
    WALKS_PER_NODE = 1,
    STORE_PER_OUTPUT = false
  ).*
}

/// Two-build investigation/probe regime ("duo"): the smallest universe in
/// which the cross-build evidence shapes exist (a wider-wanted co-build
/// next to a narrow one, reap-survivor verdicts, the present-but-tried
/// limbo cell). No faults. Used by expect-violation probes and the Phase
/// 0c pre-registered-observation investigations. HISTORY: through Phase
/// 0c-0d this scope was excluded from exhaustive invariant checks because
/// the as-built fail-fast genuinely violated C3 here (the confirmed
/// two-build wrongful-terminal-failure trace); as of Phase 1 (Wave 4) the
/// settlement encoding closes that violation and the full conjunction is
/// expected to HOLD at this scope — the pre-fix falsification is
/// preserved by the closure-c3-no-reprobe calibration pin.
module closureEvidenceDuo {
  import closureEvidence(
    DRVS = Set("d1", "d2"),
    BUILDS = Set("b1", "b2"),
    REFS = Set(("d1", "d2")),
    SUBMISSION_BUDGET = 3,
    FAILOVERS = 0,
    PGDROP_BUDGET = 0,
    STALE_APPLY = false,
    CROSS_TENURE_WALKS = false,
    STORE_GC_BUDGET = 0,
    ROW_GC_BUDGET = 0,
    PURGE_BUDGET = 0,
    POISON_BUDGET = 1,
    CANCEL_BUDGET = 1,
    // Phase 1 (FC-5): admits original walk + the settlement's verification
    // walk + one downgrade re-spawn; canReachVerificationWalkConsumed pins it.
    WALK_CONSUME_CEIL = 3,
    ATTEMPT_CYCLE_CEIL = 1,
    INTENT_CEIL = 3,
    WALKS_PER_NODE = 1,
    STORE_PER_OUTPUT = true
  ).*
}

/// Stale-tenure probe regime at duo scope: the deposed-believer late-apply
/// alphabet (STALE_APPLY / cross-tenure walk consumption) over the
/// two-node, two-build universe. Phase-1 roles: the constants the
/// closure-a17-unfenced calibration override imports (the frozen
/// pre-fence apply falsifies A18 here in five states — the wired
/// regression pin), the scope of the post-fence holds check
/// (quint-closure-evidence-stale-fence-holds: A17+A18 hold under the
/// claims-floor fence), and the scope of the fenced-discard and
/// cross-tenure-walk witnesses. Through 0c-0d this was the A17/A18
/// expected-fail probe scope; the A17 dangerous-direction interleaving
/// (clear-then-restamp-then-stale-apply) was never produced within any
/// hunted budget and is closed by the same fence that closes A18.
module closureEvidenceStaleDuo {
  import closureEvidence(
    DRVS = Set("d1", "d2"),
    BUILDS = Set("b1", "b2"),
    REFS = Set(("d1", "d2")),
    SUBMISSION_BUDGET = 3,
    FAILOVERS = 2,
    PGDROP_BUDGET = 1,
    STALE_APPLY = true,
    CROSS_TENURE_WALKS = true,
    STORE_GC_BUDGET = 0,
    ROW_GC_BUDGET = 0,
    PURGE_BUDGET = 0,
    POISON_BUDGET = 1,
    CANCEL_BUDGET = 1,
    // Phase 1 (FC-5): admits original walk + the settlement's verification
    // walk + one downgrade re-spawn; canReachVerificationWalkConsumed pins it.
    WALK_CONSUME_CEIL = 3,
    ATTEMPT_CYCLE_CEIL = 1,
    INTENT_CEIL = 5,
    WALKS_PER_NODE = 2,
    STORE_PER_OUTPUT = true
  ).*
}

/// Failover probe regime at duo scope: lost best-effort writes plus two
/// failovers over the two-node, two-build universe, sized so the bounded
/// checker can reach the C1-strict re-arming loop (lost mark-clear ->
/// failover restore -> second wrongful fail-fast) within a wired check's
/// budget.
module closureEvidenceFailoverDuo {
  import closureEvidence(
    DRVS = Set("d1", "d2"),
    BUILDS = Set("b1", "b2"),
    REFS = Set(("d1", "d2")),
    SUBMISSION_BUDGET = 3,
    FAILOVERS = 2,
    PGDROP_BUDGET = 2,
    STALE_APPLY = false,
    CROSS_TENURE_WALKS = false,
    STORE_GC_BUDGET = 0,
    ROW_GC_BUDGET = 0,
    PURGE_BUDGET = 0,
    POISON_BUDGET = 1,
    CANCEL_BUDGET = 1,
    WALK_CONSUME_CEIL = 2,
    ATTEMPT_CYCLE_CEIL = 1,
    INTENT_CEIL = 3,
    WALKS_PER_NODE = 1,
    STORE_PER_OUTPUT = true
  ).*
}

/// C3/settlement probe regime (post-0d triage; re-purposed by Phase 1):
/// the two-build universe restricted to exactly the alphabet the
/// confirmed C3 trace family needs — duplicate submissions, the client
/// cancel + terminal-build reap, per-output store letters and the
/// substitution walk — with the poison, attempt, fault and GC alphabets
/// off and one extra intent slot so the fail-fast's mark-clear emission
/// fits without a pgApply step.
///
/// Roles after the Phase-1 settlement encoding (Wave 4):
///  - the constants the closure-c3-no-reprobe calibration override
///    imports (the pre-fix actions falsify C3 here via the second C3
///    family: the reap of a marked+tried Queued survivor whose outputs
///    are obtainable — review finding MCI-2's seven-action trace);
///  - the scope of the post-fix HOLDS checks for C3 + the L2 armed form
///    (quint-closure-evidence-settlement-holds) — the same alphabet that
///    produced the violation pre-fix is what makes the holds verdict
///    non-vacuous post-fix;
///  - the scope of the canReachD16Cell witness (the cell assembles here
///    through the reap-promotion transient: merge wide / walk fails /
///    consume reverts Queued+tried / ingest / prune-merge stamps / cancel
///    / reap holes+promotes — the cell, settled by the next sweep) and of
///    the canReachVerificationWalkConsumed ceiling-headroom witness;
///  - the wired downgrade-respawn witness scope (unchanged from post-0d).
/// Following the closureEvidenceAdversarialStoreEx precedent: a probe
/// scope is sized so its violation sits inside a wired check's budget,
/// never so small that the violation disappears.
module closureEvidenceC3Duo {
  import closureEvidence(
    DRVS = Set("d1", "d2"),
    BUILDS = Set("b1", "b2"),
    REFS = Set(("d1", "d2")),
    SUBMISSION_BUDGET = 2,
    FAILOVERS = 0,
    PGDROP_BUDGET = 0,
    STALE_APPLY = false,
    CROSS_TENURE_WALKS = false,
    STORE_GC_BUDGET = 0,
    ROW_GC_BUDGET = 0,
    PURGE_BUDGET = 0,
    POISON_BUDGET = 0,
    CANCEL_BUDGET = 1,
    // Phase 1 (FC-5): 2 = original walk + the settlement's verification walk
    // (the C3 trace consumes one stale verdict, the settlement re-spawn's
    // completion is the second); canReachVerificationWalkConsumed pins it.
    WALK_CONSUME_CEIL = 2,
    ATTEMPT_CYCLE_CEIL = 0,
    INTENT_CEIL = 4,
    WALKS_PER_NODE = 1,
    STORE_PER_OUTPUT = true
  ).*
}

/// Adversarial-store probe regime at the reduced exhaustive scope
/// (Phase 0d): single tenure, no failover, store GC / row GC / purge on,
/// over the two-node, one-build universe — sized so the bounded checker
/// reaches the GC-after-vouch window (the B2-strong probe: an ok walk's
/// ingested closure is GC'd between the walk's finish and its
/// consumption) within a wired check's budget. The design-scale
/// closureEvidenceAdversarialStore module keeps the full §2b alphabet
/// for the held-back exhaustive target and the deferred hunts.
module closureEvidenceAdversarialStoreEx {
  import closureEvidence(
    DRVS = Set("d1", "d2"),
    BUILDS = Set("b1"),
    REFS = Set(("d1", "d2")),
    SUBMISSION_BUDGET = 2,
    FAILOVERS = 0,
    PGDROP_BUDGET = 0,
    STALE_APPLY = false,
    CROSS_TENURE_WALKS = false,
    STORE_GC_BUDGET = 2,
    ROW_GC_BUDGET = 1,
    PURGE_BUDGET = 1,
    POISON_BUDGET = 1,
    CANCEL_BUDGET = 1,
    // Phase 1 (FC-5): admits original walk + the settlement's verification
    // walk + one downgrade re-spawn; canReachVerificationWalkConsumed pins it.
    WALK_CONSUME_CEIL = 3,
    ATTEMPT_CYCLE_CEIL = 2,
    INTENT_CEIL = 3,
    WALKS_PER_NODE = 1,
    STORE_PER_OUTPUT = false
  ).*
}

// ===========================================================================
// Phase D-prime survivors core (substitution-replacement T-D7.1): the
// post-deletion scheduler's image of this model — the action alphabet
// restricted to the machinery that SURVIVES the D-prime deletion
// (status transitions, dep gating, attempts, the prune, poison
// co-ownership, reap/purge, recovery, the PG intent plumbing) with the
// walk lifecycle excluded from the step: no walk spawn (probeWalk), no
// finish/consume/drop (walkFinishes / consumeWalk /
// dropStaleWalkResult), no substitute_tried demotion
// (probeFailFastTried), no settlement re-probe (probeSettleTried) —
// those mechanisms were deleted in Waves D3-D5, so MSubstituting is
// unreachable here (the deleted status's model image) and the
// walks/tried/nf state stays frozen at its initial values. The mark
// letter SURVIVES re-read: the mergeCommit stamp and the
// probeFailFastMissing consumption are the predecessor carriers of the
// surviving pruned classification (post-D-prime production:
// `materialization_jobs.origin = 'pruned'` and its routing fail-fast —
// materializationJob.qnt delta 1 + the PD-D1 dedup upgrade; this
// model's durable mark is the closest in-alphabet image).
//
// The wired oracle: the six surviving invariants — A14
// (terminalIsTerminal), A15 (readyImpliesDeclaredDepsProduced), A22
// (condemnRequiresLiveCoOwnedFailure), B9
// (staleProducedNeverUnlocksDependents), B10 (demandSetSurvivesPrune)
// and L3 (liveBuildTerminalOrProgressArmed) — over TWO instantiations
// whose union covers every survivor subject's contended scope:
//
//   closureEvidenceSurvivors          Duo constants (two builds, single
//     tenure: the prune, reap-survivor, cross-build wanted and
//     stale-Produced shapes — B9/B10/A14/L3 contended);
//   closureEvidenceSurvivorsFailover  FailoverEx constants (failover +
//     lost best-effort writes: the recovery gates — A15/A22 contended).
//
// SCOPE NOTE (recorded during T-D7.1 wiring): the first wiring attempt
// instantiated the restricted alphabet at the closureEvidenceFailoverDuo
// constants (two builds AND failover) and B9 VIOLATED in 2 M samples —
// and, by trace inclusion (survivorsStep's disjuncts are a strict
// subset of step's, same init), the same trace violates B9 under the
// FULL alphabet at that scope. The trace is the documented
// predecessor-system degradation, not a restriction artifact: a narrow
// co-build drives a child Produced, failover drops the memory-only
// per-build wanted contributions, the effective wanted set widens to
// the stored union (the 062-column fallback this model encodes — see
// "interest rebuild without per-build wanted contributions" in the
// header), and a dependent opens from source above the now-stale
// Produced child. The shape needs two builds of DIFFERENT widths plus
// a failover, which no recorded-green regime combined (the B9 holds
// records are single-tenure duo and single-build failover; FailoverDuo
// was a C1-strict probe scope, never a holds scope). Production
// post-D-prime REPLACED the lossy fallback with the durable
// per-(build, derivation) relation rebuilt at recovery (Wave D2.3 /
// PD-D5), so the post-deletion system does not have this corner for
// relation-era builds; the pre-relation legacy residue saturates to
// all-declared (DQ-2, conservative-absent — the WIDER-only direction)
// and is recorded as the D-prime wanted-width transition residual.
// The survivors core therefore wires the six properties at the two
// recorded-green scopes above; the FailoverDuo B9 corner is recorded
// in the Phase D-prime stage record as a follow-up candidate against
// the PREDECESSOR encoding (history — the mechanism it models was
// deleted), with the trace seed in the stage record.
//
// Falsifiability pairing (the sim-holds vacuity rule): the two kept
// calibration pins — closure-f1-stale-produced (B9) and
// closure-f4-demand-drop (B10) — keep the invariant set demonstrably
// falsifiable; A14/A15/A22/L3 ride the same alphabet (their walk-era
// pre-fix overrides retired with the checks — dispositions in the
// Phase D-prime stage record).
// ===========================================================================

/// Survivors core, single-tenure two-build scope (Duo constants).
module closureEvidenceSurvivors {
  import closureEvidence(
    DRVS = Set("d1", "d2"),
    BUILDS = Set("b1", "b2"),
    REFS = Set(("d1", "d2")),
    SUBMISSION_BUDGET = 3,
    FAILOVERS = 0,
    PGDROP_BUDGET = 0,
    STALE_APPLY = false,
    CROSS_TENURE_WALKS = false,
    STORE_GC_BUDGET = 0,
    ROW_GC_BUDGET = 0,
    PURGE_BUDGET = 0,
    POISON_BUDGET = 1,
    CANCEL_BUDGET = 1,
    WALK_CONSUME_CEIL = 3,
    ATTEMPT_CYCLE_CEIL = 1,
    INTENT_CEIL = 3,
    WALKS_PER_NODE = 1,
    STORE_PER_OUTPUT = true
  ).*

  /// The surviving per-derivation letters: stepDrv minus the walk
  /// lifecycle (probeWalk, walkFinishes, consumeWalk,
  /// dropStaleWalkResult, probeFailFastTried, probeSettleTried).
  /// orphanRowGc stays in the alphabet (a surviving mechanism) though
  /// ROW_GC_BUDGET = 0 keeps it disabled at these constants.
  action survivorsStepDrv = {
    nondet d = DRVS.oneOf()
    any {
      probeInline(d),
      probeFailFastMissing(d),
      probeMissingUnmarked(d),
      attemptOpen(d),
      attemptCloseSuccess(d),
      attemptCloseFail(d),
      reassignOpen(d),
      requeueFailed(d),
      poisonArrival(d),
      poisonClear(d),
      orphanRowGc(d)
    }
  }

  /// The post-deletion transition relation.
  action survivorsStep = any {
    stepStore,
    stepBuild,
    survivorsStepDrv,
    pgApplyAny,
    pgDropAny,
    leaderLost,
    recoverAsLeader
  }

  /// The six surviving invariants, conjoined (the wired check's oracle;
  /// nix/quint.nix lists the same six). Manual form:
  ///
  ///   quint run --backend=rust --main=closureEvidenceSurvivors \
  ///     --step=survivorsStep --invariant=survivorsInvariants \
  ///     docs/spec/models/closureEvidence.qnt
  val survivorsInvariants = and {
    terminalIsTerminal,
    readyImpliesDeclaredDepsProduced,
    condemnRequiresLiveCoOwnedFailure,
    staleProducedNeverUnlocksDependents,
    demandSetSurvivesPrune,
    liveBuildTerminalOrProgressArmed
  }
}

/// Survivors core, failover scope (FailoverEx constants — single
/// build, one failover, one lost best-effort write: the recovery-gate
/// subjects A15/A22 contended; same restricted alphabet).
module closureEvidenceSurvivorsFailover {
  import closureEvidence(
    DRVS = Set("d1", "d2"),
    BUILDS = Set("b1"),
    REFS = Set(("d1", "d2")),
    SUBMISSION_BUDGET = 2,
    FAILOVERS = 1,
    PGDROP_BUDGET = 1,
    STALE_APPLY = false,
    CROSS_TENURE_WALKS = false,
    STORE_GC_BUDGET = 0,
    ROW_GC_BUDGET = 0,
    PURGE_BUDGET = 0,
    POISON_BUDGET = 1,
    CANCEL_BUDGET = 1,
    WALK_CONSUME_CEIL = 3,
    ATTEMPT_CYCLE_CEIL = 2,
    INTENT_CEIL = 3,
    WALKS_PER_NODE = 1,
    STORE_PER_OUTPUT = false
  ).*

  /// Same restricted per-derivation alphabet as closureEvidenceSurvivors.
  action survivorsStepDrv = {
    nondet d = DRVS.oneOf()
    any {
      probeInline(d),
      probeFailFastMissing(d),
      probeMissingUnmarked(d),
      attemptOpen(d),
      attemptCloseSuccess(d),
      attemptCloseFail(d),
      reassignOpen(d),
      requeueFailed(d),
      poisonArrival(d),
      poisonClear(d),
      orphanRowGc(d)
    }
  }

  /// The post-deletion transition relation.
  action survivorsStep = any {
    stepStore,
    stepBuild,
    survivorsStepDrv,
    pgApplyAny,
    pgDropAny,
    leaderLost,
    recoverAsLeader
  }

  /// The six surviving invariants (same conjunction as
  /// closureEvidenceSurvivors). Manual form:
  ///
  ///   quint run --backend=rust --main=closureEvidenceSurvivorsFailover \
  ///     --step=survivorsStep --invariant=survivorsInvariants \
  ///     docs/spec/models/closureEvidence.qnt
  val survivorsInvariants = and {
    terminalIsTerminal,
    readyImpliesDeclaredDepsProduced,
    condemnRequiresLiveCoOwnedFailure,
    staleProducedNeverUnlocksDependents,
    demandSetSurvivesPrune,
    liveBuildTerminalOrProgressArmed
  }
}
