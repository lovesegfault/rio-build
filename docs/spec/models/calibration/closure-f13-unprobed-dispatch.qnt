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
// Stage-C calibration override, closure-evidence family F13
// (probe-coverage completeness; corpus rep CE-58, the
// 6058cafcd / 398d0ed9e / 75ca91224 / d6e4cf5c4 group — whole classes
// of nodes never received a substitutability verdict before being
// handed to a builder, so cached paths were compiled from source).
// Crosswalk row: closure-evidence-formal-design.md §4 "F13 CE-58 → ENC
// → B8"; 0d verdict: the VERDICT block at the end of this header.
//
// PRE-FIX behavior modeled: the from-source admission does not require
// a substitutability verdict consumed in the current probe pass — a
// Ready node that nothing probed since the last reset (here: since the
// recovery wiped the probe flags) is delivered anyway. The
// must_substitute interlock and every other guard stay as built; only
// the probed-this-pass requirement is dropped. The B8 latch keeps the
// as-built oracle.
//
// PREDICTED: dispatchImpliesProbedThisPass (B8) falsifies — the
// counterexample is a recovered, never-re-probed Ready node opened
// from source.
// Constants: the reduced exhaustive failover scope
// (closureEvidenceFailoverEx) — the unprobed-Ready trigger lives behind
// the recovery's probe-flag wipe; the as-built baseline is the same
// module without --step at these constants (bounded run recorded in the
// invariant map's Phase 0d calibration table).
//
// Run (serially):
//   quint verify --backend=tlc --main=closureCalibF13UnprobedDispatch \
//     --step=calibStep --invariant=dispatchImpliesProbedThisPass \
//     docs/spec/models/calibration/closure-f13-unprobed-dispatch.qnt
// Baseline:
//   quint verify --backend=tlc --main=closureCalibF13UnprobedDispatch \
//     --invariant=dispatchImpliesProbedThisPass \
//     docs/spec/models/calibration/closure-f13-unprobed-dispatch.qnt
//
// VERDICT (Stage-C 0d calibration; relocated from the retired closure-
// evidence invariant map, full tables: docs/spec/models/closure-
// evidence-records.md): CE-58 → B8 VIOLATED — 6-state TLC trace (6.8 K
// distinct): recovered, never-re-probed Ready node opened from source.
// Baseline: HOLDS-BOUNDED (FailoverEx depth 12) + by construction.
// Wave-4 re-validation: still falsifies — TLC, 39 s, 72 K distinct (16
// workers).
module closureCalibF13UnprobedDispatch {
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

  /// PRE-FIX from-source admission: the probed-this-pass requirement is
  /// dropped (pre-CE-58 probe coverage); the must_substitute interlock
  /// and the rest of the guard are as built. Latches unchanged.
  action calibAttemptOpenUnprobed(d: str): bool = all {
    leaderActive,
    drvOf(d).mSt == MReady,
    liveInterest(d),
    not(mustSub(d)),
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
      calibAttemptOpenUnprobed(d),
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
