// -*- mode: Bluespec; -*-
//
// Stage-C calibration override, closure-evidence family F14
// (Substituting-status lifecycle / stuck states; wired family
// representative re-routed within the family to the CE-48(i) shape,
// 59f173225 — a recovered Substituting node re-entered the DAG with no
// walk anywhere and no legal transition, stuck forever; the design's
// named rep CE-52 d129eb820 is the Poisoned-at-limit re-probe lane,
// whose stuck cell the post-ladder model absorbs into the merge-time
// reset, so it cannot falsify a state invariant here — the re-route and
// that argument are recorded in the invariant map's Phase 0d
// calibration table; CE-52 keeps its named coverage in the merge
// resetNodes lane). Crosswalk rows: closure-evidence-formal-design.md
// §4 "F10 CE-48(i) → ENC → L1", "F14 CE-52..54 → ENC (rep: CE-52) →
// L1 / A15-adjacent".
//
// PRE-FIX behavior modeled: recovery trusts the persisted 'substituting'
// status — a row loaded as Substituting is re-entered as Substituting
// instead of being re-derived to Ready/Queued from the dependency walk,
// although no walk instance survives the failover (the spawning
// tenure's detached task died with it).
//
// PREDICTED: substitutingAlwaysArmed (L1) falsifies — the
// counterexample is a recovered Substituting node with an empty walk
// list and nothing armed to settle it.
// Constants: the reduced exhaustive failover scope
// (closureEvidenceFailoverEx); the as-built baseline is the same module
// without --step at these constants (bounded run recorded in the
// invariant map's Phase 0d calibration table).
//
// Run (serially):
//   quint verify --backend=tlc --main=closureCalibF14RecoveryKeepsSubstituting \
//     --step=calibStep --invariant=substitutingAlwaysArmed \
//     docs/spec/models/calibration/closure-f14-recovery-keeps-substituting.qnt
// Baseline:
//   quint verify --backend=tlc --main=closureCalibF14RecoveryKeepsSubstituting \
//     --invariant=substitutingAlwaysArmed \
//     docs/spec/models/calibration/closure-f14-recovery-keeps-substituting.qnt
module closureCalibF14RecoveryKeepsSubstituting {
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
    WALK_CONSUME_CEIL = 2,
    ATTEMPT_CYCLE_CEIL = 2,
    INTENT_CEIL = 3,
    WALKS_PER_NODE = 1,
    STORE_PER_OUTPUT = false
  ).* from "../closureEvidence"

  /// PRE-FIX leader recovery: identical to recoverAsLeader except that
  /// a row loaded with the persisted Substituting status keeps that
  /// status (no seed_ready_queue re-derivation for it) although no walk
  /// instance survives the failover. Every gate, stamp, condemnation
  /// and latch is the live model's.
  action calibRecoverTrustSubstituting: bool = {
    nondet expired = DRVS.powerset().oneOf()
    val newGen = lead.gen + 1
    val expiredPoisons = expired.intersect(DRVS.filter(d => drv.get(d).rowExists and drv.get(d).pgSt == MPoisoned))
    val loaded = DRVS.filter(d => drv.get(d).rowExists and or {
      not(isTerminal(drv.get(d).pgSt)),
      drv.get(d).pgSt == MPoisoned and not(expiredPoisons.contains(d))
    })
    val newMemEdges = pgEdges.filter(e => loaded.contains(e._1) and loaded.contains(e._2))
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
          wrongFF: if (r.pgTp) 0 else r.wrongFF, wrongFFStrict: r.wrongFFStrict }
    })
    val dm2 = DRVS.mapBy(d => {
      val r = dm1.get(d)
      if (r.mSt == MAbsent or r.mSt == MPoisoned) r
      else {
        val condemned = pCondemnCriterion(dm1, blds, BUILDS, pgEdges, d)
        val clearGate = r.mTp and not(r.mCh) and pStrictPgCriterion(dm1, blds, BUILDS, pgEdges, d)
        val holeStamp = pRecoveryHoleCriterion(dm1, pgEdges, d)
        val kidsLoaded = pKids(newMemEdges, d)
        val depsSatisfied = kidsLoaded.forall(c => dm1.get(c).mSt == MProduced)
        // pre-fix: a persisted Substituting status is trusted verbatim
        // (no re-derivation from the dependency walk for it).
        val baseSt =
          if (condemned) MDepFailed
          else if (r.mSt == MSubstituting) MSubstituting
          else if (r.mSt == MQueued or r.mSt == MReady)
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
    val condemnedSet = DRVS.filter(d => dm1.get(d).mSt != MAbsent and dm1.get(d).mSt != MPoisoned and
      pCondemnCriterion(dm1, blds, BUILDS, pgEdges, d))
    val newIntents =
      clearedSet.map(d => { op: IClearBoth(d), tenure: newGen })
        .union(holedSet.map(d => { op: ISetHole(d), tenure: newGen }))
    all {
      not(lead.up),
      (if (STALE_APPLY) intents.size() else 0) + newIntents.size() <= INTENT_CEIL,
      lead' = { up: true, gen: newGen, recovered: true },
      drv' = dm2,
      memEdges' = newMemEdges,
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
        recoveryClearMissed: viol.recoveryClearMissed,
        restoredBitDropped: viol.restoredBitDropped,
        condemnUnscoped: viol.condemnUnscoped or
          condemnedSet.exists(d => not(pCondemnCriterion(dm1, blds, BUILDS, pgEdges, d))),
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

  action calibStep = any {
    stepStore,
    stepBuild,
    stepDrv,
    pgApplyAny,
    pgDropAny,
    leaderLost,
    calibRecoverTrustSubstituting
  }
}
