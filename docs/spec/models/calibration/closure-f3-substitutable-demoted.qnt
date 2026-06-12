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
// Stage-C calibration override, closure-evidence family F3,
// permissiveness direction (available treated as unavailable; corpus
// members CE-3 18df83e02 / CE-60 d6dd46edd — a probe that lost its
// substitutable component routed upstream-available work to from-source
// builds). The §4 both-directions clause for the permissiveness
// families (F1, F3, F6, the C-group) is discharged for F3 by this
// override falsifying C2 while the F3 soundness override
// (closure-f3-indet-failfast.qnt) falsifies B3. 0d verdict: the
// VERDICT block at the end of this header.
//
// PRE-FIX behavior modeled: the unmarked-Ready probe routing demotes a
// node to the from-source lane on any missing wanted output — no
// confirmed-miss answer required, so a node whose missing outputs are
// all available upstream is routed from source (the probe verdict lost
// its "substitutable" component). The C2 latch keeps the as-built
// oracle (every missing live-wanted output available upstream, no
// genuine-walk-failure one-shot).
//
// PREDICTED: noWrongfulFromSourceDemotion (C2) falsifies — the
// counterexample is a recovered, never-walked Ready node with
// upstream-available outputs routed from source by the pre-fix probe.
// Constants: the reduced exhaustive failover scope
// (closureEvidenceFailoverEx); the as-built baseline is the same module
// without --step at these constants (bounded run recorded in the
// invariant map's Phase 0d calibration table).
//
// Run (serially):
//   quint verify --backend=tlc --main=closureCalibF3SubstitutableDemoted \
//     --step=calibStep --invariant=noWrongfulFromSourceDemotion \
//     docs/spec/models/calibration/closure-f3-substitutable-demoted.qnt
// Baseline:
//   quint verify --backend=tlc --main=closureCalibF3SubstitutableDemoted \
//     --invariant=noWrongfulFromSourceDemotion \
//     docs/spec/models/calibration/closure-f3-substitutable-demoted.qnt
//
// VERDICT (Stage-C 0d calibration; relocated from the retired closure-
// evidence invariant map, full tables: docs/spec/models/closure-
// evidence-records.md): CE-60/CE-3 → C2 VIOLATED — 6-state TLC trace
// (4.5 K distinct): recovered Ready node with upstream-available outputs
// routed from source. Baseline: HOLDS-BOUNDED (FailoverEx depth 12) + by
// construction. Wave-4 re-validation: still falsifies — TLC, 38 s, 73 K
// distinct (16 workers).
module closureCalibF3SubstitutableDemoted {
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

  /// PRE-FIX unmarked-Ready probe routing: any missing wanted output is
  /// recorded as a confirmed miss (the substitutable/indeterminate
  /// component of the verdict is lost), so the node is left on the
  /// from-source lane although everything missing is available
  /// upstream. Latches keep the as-built oracle.
  action calibProbeMissingAnyAbsent(d: str): bool = all {
    leaderActive,
    drvOf(d).mSt == MReady,
    liveInterest(d),
    not(mustSub(d)),
    effWanted(d).exists(i => not(store.get((d, i)).present)),
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

  action calibStepDrv = {
    nondet d = DRVS.oneOf()
    any {
      probeInline(d),
      probeWalk(d),
      probeFailFastMissing(d),
      probeFailFastTried(d),
      probeSettleTried(d),
      calibProbeMissingAnyAbsent(d),
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
