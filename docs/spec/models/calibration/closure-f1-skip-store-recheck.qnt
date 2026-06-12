// -*- mode: Bluespec; -*-
//
// PHASE D-PRIME RETIREMENT NOTE (substitution-replacement T-D7.1):
// this evidence module was never wired; the closure-evidence wired
// family was pruned to the survivors core plus the two kept pins
// (closure-f1-stale-produced, closure-f4-demand-drop).
// The walk-era mechanism family this corpus calibrated was deleted in
// Waves D3-D6 (the walk, the evidence columns, the Substituting
// status; migration 080). This file stays as the predecessor-system
// evidence record until the A6 archive of closureEvidence.qnt; its
// re-run command remains valid against the retained model text.
//
// Stage-C calibration override, closure-evidence family F1,
// permissiveness direction (corpus rep CE-1, 5dc6cc478 — TOCTOU between
// the gateway probe and the merge: the merge trusted the stale probe
// verdict instead of re-checking the store, so a derivation whose
// outputs were already all present was queued and built from source).
// Crosswalk row: closure-evidence-formal-design.md §4 "F1 CE-1 → ENC →
// C4"; 0d verdict: the VERDICT block at the end of this header.
// Together with the F1 soundness override
// (closure-f1-stale-produced.qnt → B9) this discharges the §4
// both-directions clause for F1.
//
// PRE-FIX behavior modeled: the merge-time classification performs no
// local store re-check — cache-hit adoption is gone and the
// missing/available split is judged from the stale upstream/cache view
// only, so a locally-present node is routed to the from-source lane.
// The C4 latch keeps the as-built oracle (a node queued from source
// although every live-wanted output is present).
//
// PREDICTED: noBuildWhenWantedPresent (C4) falsifies — the
// counterexample is a present node classified missing by the
// re-check-free merge. The safety direction (B9 et al.) still holds
// under this override, which is what makes the falsification
// attributable to the permissiveness guard.
// Constants: the reduced exhaustive-base scope (closureEvidenceBaseEx);
// the as-built baseline for these constants is the Phase 0c closing run
// / property-identification run (C4 holds there), re-runnable as the
// same module without --step.
//
// Run (serially):
//   quint verify --backend=tlc --main=closureCalibF1SkipStoreRecheck \
//     --step=calibStep --invariant=noBuildWhenWantedPresent \
//     docs/spec/models/calibration/closure-f1-skip-store-recheck.qnt
// Baseline:
//   quint verify --backend=tlc --main=closureCalibF1SkipStoreRecheck \
//     --invariant=noBuildWhenWantedPresent \
//     docs/spec/models/calibration/closure-f1-skip-store-recheck.qnt
//
// VERDICT (Stage-C 0d calibration; relocated from the retired closure-
// evidence invariant map, full tables: docs/spec/models/closure-
// evidence-records.md): CE-1 → C4 VIOLATED — 5-state TLC trace (3.6 K
// distinct): a locally-present node is classified missing and queued
// from source. Baseline: HOLDS (0c BaseEx record + by construction).
// Wave-4 re-validation: still falsifies — TLC, 3.2 s, 171 distinct
// states (16 workers).
module closureCalibF1SkipStoreRecheck {
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
  /// merge-time classification has no local store re-check (no
  /// cache-hit adoption; missing judged from the stale upstream/cache
  /// view). Every other pass and every latch is the live model's.
  action calibMergeCommitNoStoreRecheck(b: str): bool = {
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
    val demandAvailable = demand != Set() and contribW != Set() and demand.forall(dd =>
      pEffWanted(drv, bldsProbe, BUILDS, dd).forall(i =>
        store.get((dd, i)).present or pSubAnswerable(store, (dd, i))))
    val pruneOk = tryPrune and demandAvailable
    val kept = if (pruneOk) demand else submitted
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
          nf: Set(), probed: false, probedKind: PKNone, walks: [],
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
    // pre-fix: no local store re-check — nothing is adopted as a cache
    // hit, and "missing" is judged from the stale upstream/cache view
    // alone (local presence is not consulted).
    val cacheHits = classifiable.filter(d => false)
    val confirmableMissing = classifiable.filter(d =>
      pEffWanted(drv, bldsNew, BUILDS, d).exists(i =>
        store.get((d, i)).cache != CHit and not(store.get((d, i)).upHas)))
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
      calibMergeCommitNoStoreRecheck(b),
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
