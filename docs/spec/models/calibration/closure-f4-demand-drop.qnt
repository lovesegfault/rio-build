// -*- mode: Bluespec; -*-
//
// Stage-C calibration override, closure-evidence family F4, prune
// demand-set completeness (corpus rep CE-66, 85213119d — an explicitly
// requested target inside another target's closure was not a structural
// root, so the prune dropped it entirely: never merged, classified,
// substituted or dispatched). Crosswalk row:
// closure-evidence-formal-design.md §4 "F4 CE-66 → ENC → B10"; verdict
// table: closure-evidence-invariant-map.md (Phase 0d).
//
// PRE-FIX behavior modeled: the prune's demand set is the structural
// roots only — the explicitly-requested marking is ignored both by the
// availability check and by the kept set, so an explicitly requested
// non-root is silently dropped when the prune fires. The B10 latch
// keeps the as-built oracle (demand = roots ∪ explicitly-requested,
// some member not kept).
//
// PREDICTED: demandSetSurvivesPrune (B10) falsifies — the
// counterexample is a pruned submission whose explicitly-requested
// non-root member is dropped.
// Constants: the reduced exhaustive-base scope (closureEvidenceBaseEx);
// the as-built baseline for these constants is the Phase 0c closing run
// / property-identification run (B10 holds there), re-runnable as the
// same module without --step.
//
// Run (serially):
//   quint verify --backend=tlc --main=closureCalibF4DemandDrop \
//     --step=calibStep --invariant=demandSetSurvivesPrune \
//     docs/spec/models/calibration/closure-f4-demand-drop.qnt
// Baseline:
//   quint verify --backend=tlc --main=closureCalibF4DemandDrop \
//     --invariant=demandSetSurvivesPrune \
//     docs/spec/models/calibration/closure-f4-demand-drop.qnt
module closureCalibF4DemandDrop {
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
  ).* from "../closureEvidence"

  /// PRE-FIX committed merge: identical to mergeCommit except the
  /// prune's demand set is the structural roots only (the
  /// explicitly-requested marking is ignored). Every other pass and
  /// every latch is the live model's.
  action calibMergeCommitRootsOnlyDemand(b: str): bool = {
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
    val bldsProbe = blds.set(b, { ...blds.get(b),
      phase: BpActive, cleanup: false,
      interest: submitted,
      contrib: DRVS.mapBy(d => if (submitted.contains(d)) contribW else Set()) })
    // pre-fix: the prune considers structural roots only — the
    // explicitly-requested marking is invisible to both the
    // availability check and the kept set. (`demand` above keeps the
    // as-built roots ∪ explicit definition as the oracle.)
    val demandAvailable = rootsPick != Set() and contribW != Set() and rootsPick.forall(dd =>
      pEffWanted(drv, bldsProbe, BUILDS, dd).forall(i =>
        store.get((dd, i)).present or pSubAnswerable(store, (dd, i))))
    val pruneOk = tryPrune and demandAvailable
    val kept = if (pruneOk) rootsPick else submitted
    val bldsNew = blds.set(b, { ...blds.get(b),
      phase: BpActive, cleanup: false,
      interest: kept,
      contrib: DRVS.mapBy(d => if (kept.contains(d)) contribW else Set()) })
    val resetNodes = kept.filter(d => isTerminal(drv.get(d).mSt) or drv.get(d).mSt == MFailed)
    val newEdges = if (pruneOk) Set() else REFS.filter(e => kept.contains(e._1) and kept.contains(e._2))
    val memEdges1 = memEdges.union(newEdges)
    val dm1 = DRVS.mapBy(d => {
      val r = drv.get(d)
      if (not(kept.contains(d))) r
      else if (r.mSt == MAbsent)
        { ...r, mSt: MQueued, mTp: false, mCh: false, tried: false, triedGenuine: false,
          nf: Set(), probed: false, probedKind: PKNone }
      else if (resetNodes.contains(d) and r.mSt != MProduced)
        { ...r, mSt: MQueued, mTp: false, tried: false, triedGenuine: false,
          nf: Set(), probed: false, probedKind: PKNone,
          pgSt: if (r.rowExists) MQueued else r.pgSt }
      else r
    })
    // stale-Completed verify (as built): pre-existing Produced kept nodes
    // whose live-wanted outputs are absent are demoted back to Queued.
    val staleProduced = kept.filter(d => drv.get(d).mSt == MProduced and
      not(pEffWanted(drv, bldsNew, BUILDS, d).forall(i => store.get((d, i)).present)))
    val dm2 = staleProduced.fold(dm1, (acc, d) =>
      acc.set(d, { ...acc.get(d), mSt: MQueued, pgSt: if (acc.get(d).rowExists) MQueued else acc.get(d).pgSt }))
    val stamps = if (not(pruneOk)) Set() else kept.filter(d => and {
      pKids(REFS, d).intersect(submitted).exists(c => not(kept.contains(c))),
      not(pVouched(dm2, memEdges1, d))
    })
    val dm3 = stamps.fold(dm2, (acc, d) =>
      acc.set(d, { ...acc.get(d), mTp: true, wrongFF: 0, wrongFFStrict: 0 }))
    val classifiable = kept.filter(d => dm3.get(d).mSt == MQueued or dm3.get(d).mSt == MReady)
    val cacheHits = classifiable.filter(d =>
      pEffWanted(drv, bldsNew, BUILDS, d).forall(i => store.get((d, i)).present))
    val confirmableMissing = classifiable.filter(d =>
      pEffWanted(drv, bldsNew, BUILDS, d).exists(i => pMissAnswerable(store, (d, i))))
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
          walks: [{ forgivable: OUTIDXSET.exclude(effWd).exclude(r.nf), tenure: lead.gen, res: WPending }] }
      else if (normalQueued.contains(d))
        { ...r, probed: true, probedKind: PKMissing }
      else r
    })
    val healParents = if (pruneOk) Set() else kept.filter(d => pKids(REFS, d).intersect(kept) != Set())
    val dm5 = healParents.fold(dm4, (acc, d) => acc.set(d, { ...acc.get(d), mCh: false }))
    val clearCandidates = healParents.union(cacheHits.fold(Set(), (acc, c) => acc.union(pParents(memEdges1, c))))
    val clearedC1 = clearCandidates.filter(d => dm5.get(d).mTp and pVouched(dm5, memEdges1, d))
    val dm6c = clearedC1.fold(dm5, (acc, d) => acc.set(d, { ...acc.get(d), mTp: false, mCh: false }))
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
        stampUnsound: viol.stampUnsound or stamps.exists(d =>
          not(pruneOk) or pVouched(dm2, memEdges1, d) or
          not(pKids(REFS, d).intersect(submitted).exists(c => not(kept.contains(c))))),
        healMissed: viol.healMissed or
          (not(pruneOk) and healParents.exists(d => drv.get(d).rowExists and drv.get(d).pgCh and
            not(healIntents.exists(i => i.op == IHeal(d))))),
        brokenVouched: viol.brokenVouched or clearedC1.exists(d =>
          pKids(memEdges1, d) == Set() or dm5.get(d).mCh),
        clearUnsound: viol.clearUnsound or clearedC1.exists(d => not(pVouched(dm5, memEdges1, d))),
        demandDropped: viol.demandDropped or (pruneOk and not(demand.subseteq(kept))),
        pruneWeakerThanClassify: viol.pruneWeakerThanClassify or (pruneOk and demand.exists(dd =>
          pEffWanted(drv, bldsNew, BUILDS, dd).exists(i =>
            not(store.get((dd, i)).present) and not(pSubAnswerable(store, (dd, i)))))),
        vacuousWantedVerdict: viol.vacuousWantedVerdict or (pruneOk and contribW == Set()),
        queuedWhilePresent: viol.queuedWhilePresent or normalQueued.exists(d =>
          pEffWanted(drv, bldsNew, BUILDS, d).forall(i => store.get((d, i)).present)),
        producedWithoutPresence: viol.producedWithoutPresence or cacheHits.exists(d =>
          not(pEffWanted(drv, bldsNew, BUILDS, d).forall(i => store.get((d, i)).present))),
        staleProducedUnlocked: viol.staleProducedUnlocked or kept.exists(d =>
          dm7.get(d).mSt == MReady and pKids(memEdges1, d).exists(c =>
            dm7.get(c).mSt == MProduced and
            not(pEffWanted(dm7, bldsNew, BUILDS, c).forall(i => store.get((c, i)).present)))),
        indetDemoted: viol.indetDemoted
      },
      reach' = { ...reach,
        stampFired: reach.stampFired or stamps != Set(),
        pruneFired: reach.pruneFired or pruneOk }
    }
  }

  action calibStepBuild = {
    nondet b = BUILDS.oneOf()
    any {
      calibMergeCommitRootsOnlyDemand(b),
      mergeRollback(b),
      cancelBuild(b),
      buildCompletes(b),
      buildFails(b),
      reapTerminalBuild(b),
      buildsRowPurge(b)
    }
  }

  action calibStep = any {
    stepStore,
    calibStepBuild,
    stepDrv,
    pgApplyAny,
    pgDropAny,
    leaderLost,
    recoverAsLeader
  }
}
