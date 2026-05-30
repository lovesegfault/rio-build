// -*- mode: Bluespec; -*-
//
// Stage-C calibration override, closure-evidence family F2 (closure
// incompleteness behind a "successful" substitution; corpus rep CE-9,
// 808d82579 — try_substitute never walked narinfo.References, so the
// seed landed and its transitive references were missing in the builder
// sandbox). Crosswalk row: closure-evidence-formal-design.md §4 "F2
// CE-8..CE-11 → ENC (rep: CE-9) → B2"; verdict table:
// docs/spec/models/closure-evidence-invariant-map.md (Phase 0d).
//
// PRE-FIX behavior modeled: the substitution walk checks and ingests
// ONLY the seed node's non-forgiven outputs — the REFS-descendant
// closure is neither required to be available nor ingested. The
// consumption-side latch (walkOkClosureNeverPresent) keeps the as-built
// oracle: a consumed ok walk whose walked closure was never present.
//
// PREDICTED: substituteOkImpliesClosureIngested (B2, weak form)
// falsifies — the counterexample is a parent completed by substitution
// while its child's outputs were never present anywhere.
// Constants: the reduced exhaustive-base scope (closureEvidenceBaseEx),
// the same constants the Phase 0c closing run used; the as-built
// baseline for these constants is that closing run plus the 0c
// property-identification run (B2 holds there), and the same module
// without --step re-checks it on demand.
//
// Run (serially — the bundled Apalache server port is shared):
//   quint verify --backend=tlc --main=closureCalibF2SeedOnlyWalk \
//     --step=calibStep --invariant=substituteOkImpliesClosureIngested \
//     docs/spec/models/calibration/closure-f2-seed-only-walk.qnt
// Baseline (must HOLD the same invariant):
//   quint verify --backend=tlc --main=closureCalibF2SeedOnlyWalk \
//     --invariant=substituteOkImpliesClosureIngested \
//     docs/spec/models/calibration/closure-f2-seed-only-walk.qnt
module closureCalibF2SeedOnlyWalk {
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

  /// PRE-FIX walk finish: ok walks check and ingest only the seed's
  /// non-forgiven outputs; the REFS-descendant closure is ignored
  /// (pre-808d82579: no References walk). Failed walks are unchanged.
  action calibWalkFinishesSeedOnly(d: str): bool = {
    nondet idx = Set(0, 1).oneOf()
    nondet okPick = Set(true, false).oneOf()
    val w = pWalkAt(drvOf(d).walks, idx)
    nondet forgivenPick = w.forgivable.powerset().oneOf()
    val forgiven = forgivenPick
    // pre-fix: seed outputs only — no descendant closure
    val ingestOuts = OUTIDX.exclude(forgiven).map(i => (d, i))
    val genuineFail = OUTIDX.exclude(w.forgivable).exists(i =>
      not(store.get((d, i)).present) and not(store.get((d, i)).upHas))
    all {
      idx < drvOf(d).walks.length(),
      w.res == WPending,
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

  /// The per-derivation letter group with the walk-finish letter swapped
  /// for the pre-fix variant; everything else is the live model's.
  action calibStepDrv = {
    nondet d = DRVS.oneOf()
    any {
      probeInline(d),
      probeWalk(d),
      probeFailFastMissing(d),
      probeFailFastTried(d),
      probeSettleTried(d),
      probeMissingUnmarked(d),
      calibWalkFinishesSeedOnly(d),
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
