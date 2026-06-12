// -*- mode: Bluespec; -*-
//
// PHASE D-PRIME RETIREMENT NOTE (substitution-replacement T-D7.1):
// the wired check quint-closure-calib-f8-dispatch-no-evidence was
// pruned in the survivors-core landing; successor oracle:
// quint-materialization-calib-f8-pull-ignores-job
// (noFromSourceWhileJobUnresolved — the anchor).
// The walk-era mechanism family this corpus calibrated was deleted in
// Waves D3-D6 (the walk, the evidence columns, the Substituting
// status; migration 080). This file stays as the predecessor-system
// evidence record until the A6 archive of closureEvidence.qnt; its
// re-run command remains valid against the retained model text.
//
// Stage-C calibration override, closure-evidence family F8
// (must_substitute enforcement; corpus rep CE-33, 7e1092575 — the
// post-failover childless pruned root was probed, found definitively
// missing-and-unsubstitutable, left Ready and dispatched from source,
// ENOENT on never-merged inputDrvs). Crosswalk row:
// closure-evidence-formal-design.md §4 "F8 CE-33 → ENC (THE anchor) →
// A1"; 0d verdict: the VERDICT block at the end of this header.
//
// PRE-FIX behavior modeled: the from-source admission (admit_pull
// DeliverNew / the open-attempt mint) does not consult the closure
// evidence at all — neither the must_substitute interlock nor the
// probed-this-pass discipline existed on the pre-fix dispatch path, so
// a Ready node is delivered regardless of its mark/evidence. One
// production guard (evidence consulted before from-source admission),
// one model site (attemptOpen). The A1/A11/B8 latches keep the as-built
// oracle.
//
// PREDICTED: noDoomedFromSourceDispatch (A1) falsifies — the
// counterexample is a from-source attempt opened on a marked node with
// Broken evidence (the recovered childless pruned root).
// pullRefusalNoMint (A11) falsifies on the same trace.
// Constants: the reduced exhaustive failover scope
// (closureEvidenceFailoverEx) — the trigger needs the failover restore;
// the as-built baseline is the same module without --step at these
// constants (bounded run recorded in the invariant map's Phase 0d
// calibration table).
//
// Run (serially):
//   quint verify --backend=tlc --main=closureCalibF8DispatchNoEvidence \
//     --step=calibStep --invariant=noDoomedFromSourceDispatch \
//     docs/spec/models/calibration/closure-f8-dispatch-no-evidence.qnt
// Baseline:
//   quint verify --backend=tlc --main=closureCalibF8DispatchNoEvidence \
//     --invariant=noDoomedFromSourceDispatch \
//     docs/spec/models/calibration/closure-f8-dispatch-no-evidence.qnt
//
// VERDICT (Stage-C 0d calibration; relocated from the retired closure-
// evidence invariant map, full tables: docs/spec/models/closure-
// evidence-records.md): CE-33 → A1 VIOLATED — 6-state TLC trace (78.7 K
// distinct): recovered marked childless root delivered from source by
// the evidence-blind admission. Baseline: HOLDS-BOUNDED (FailoverEx
// depth 12) + by construction.
module closureCalibF8DispatchNoEvidence {
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

  /// PRE-FIX from-source admission: no must_substitute interlock, no
  /// probed-this-pass requirement — any Ready node with live interest
  /// and a free attempt slot is delivered (pre-7e1092575 / pre-interlock
  /// admit_pull). Latches unchanged (the as-built oracle).
  action calibAttemptOpenNoEvidenceGate(d: str): bool = all {
    leaderActive,
    drvOf(d).mSt == MReady,
    liveInterest(d),
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

  action calibStepDrv = {
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
      calibAttemptOpenNoEvidenceGate(d),
      attemptCloseSuccess(d),
      attemptCloseFail(d),
      reassignOpen(d),
      requeueFailed(d),
      poisonArrival(d),
      poisonClear(d),
      orphanRowGc(d)
    }
  }

  action calibStep = any {
    stepStore,
    stepBuild,
    calibStepDrv,
    pgApplyAny,
    pgDropAny,
    leaderLost,
    recoverAsLeader
  }
}
