// -*- mode: Bluespec; -*-
//
// Stage-C calibration override, closure-evidence family F10
// (recovery / failover evidence scoping; corpus rep CE-45, f0d10a40b —
// the recovery's produced-children clear accepted children vouched only
// by long-terminal builds, clearing the mark although nothing live
// co-owned that evidence and the store may have GC'd those outputs;
// CE-46 650d5ac14 is the same scoping rule's condemn direction).
// Crosswalk row: closure-evidence-formal-design.md §4 "F10 CE-45,
// CE-46 → ENC (rep: CE-45) → A3(ii)/A19 vouch direction over the
// durable relation"; verdict table: closure-evidence-invariant-map.md
// (Phase 0d).
//
// PRE-FIX behavior modeled: the recovery clear gate accepts any durable
// all-children-Produced row set — the live-build co-ownership scoping
// of the strict PG criterion is dropped, so produced rows linked only
// to a dead/replaced submission still vouch the clear. The A3 oracle is
// added at this site (a recovery clear taken where the as-built strict
// criterion does not hold).
//
// PREDICTED: clearSoundness (A3) falsifies — the counterexample is a
// restored mark cleared at recovery on the strength of a child row
// whose only co-owning links belong to a superseded submission.
// Constants: the reduced exhaustive failover scope
// (closureEvidenceFailoverEx); the as-built baseline is the same module
// without --step at these constants (bounded run recorded in the
// invariant map's Phase 0d calibration table).
//
// Run (serially):
//   quint verify --backend=tlc --main=closureCalibF10RecoveryVouchUnscoped \
//     --step=calibStep --invariant=clearSoundness \
//     docs/spec/models/calibration/closure-f10-recovery-vouch-unscoped.qnt
// Baseline:
//   quint verify --backend=tlc --main=closureCalibF10RecoveryVouchUnscoped \
//     --invariant=clearSoundness \
//     docs/spec/models/calibration/closure-f10-recovery-vouch-unscoped.qnt
module closureCalibF10RecoveryVouchUnscoped {
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

  /// The pre-fix clear criterion: every durable child row Produced — no
  /// live-build co-ownership required (pre-f0d10a40b).
  def calibWeakClearCriterion(dm: str -> DrvRec, pgE: Set[(str, str)], d: str): bool = {
    val kids = pKids(pgE, d).filter(c => dm.get(c).rowExists)
    kids != Set() and kids.forall(c => dm.get(c).pgSt == MProduced)
  }

  /// PRE-FIX leader recovery: identical to recoverAsLeader except the
  /// clear gate uses the unscoped produced-children criterion. The A3
  /// oracle (the as-built strict criterion) is recorded for every clear
  /// the weak gate takes; everything else is the live model's.
  action calibRecoverVouchUnscoped: bool = {
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
        // pre-fix: the clear gate drops the live co-ownership scoping
        val clearGate = r.mTp and not(r.mCh) and calibWeakClearCriterion(dm1, pgEdges, d)
        val holeStamp = pRecoveryHoleCriterion(dm1, pgEdges, d)
        val kidsLoaded = pKids(newMemEdges, d)
        val depsSatisfied = kidsLoaded.forall(c => dm1.get(c).mSt == MProduced)
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
      dm1.get(d).mTp and not(dm1.get(d).mCh) and calibWeakClearCriterion(dm1, pgEdges, d))
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
        // A3 oracle: a recovery clear taken where the as-built strict
        // criterion (live co-owned produced children) does not hold.
        clearUnsound: viol.clearUnsound or
          clearedSet.exists(d => not(pStrictPgCriterion(dm1, blds, BUILDS, pgEdges, d))),
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
    calibRecoverVouchUnscoped
  }
}
