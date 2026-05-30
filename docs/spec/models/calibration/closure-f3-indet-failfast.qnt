// -*- mode: Bluespec; -*-
//
// Stage-C calibration override, closure-evidence family F3, soundness
// direction (false demotion to from-source / "unknown is not missing";
// corpus rep CE-61, 9bbe9ee29 — 429/5xx/deadline-cut probes were
// reported as confirmed-miss; CE-60 d6dd46edd is the same trichotomy
// with a probe-cap trigger). Crosswalk row:
// closure-evidence-formal-design.md §4 "F3 CE-60, CE-61 → ENC (rep:
// CE-61) → B3"; verdict table: closure-evidence-invariant-map.md
// (Phase 0d).
//
// PRE-FIX behavior modeled: the must-substitute fail-fast arm of the
// dispatch probe fires without a confirmed-missing answer being
// available for any wanted output — an indeterminate / substitutable
// answer is treated as a definitive miss (the pre-fix verdict mapping).
// The B3 latch keeps the as-built oracle (no miss-answerable output at
// fire time).
//
// PREDICTED: unknownNeverDemotes (B3) falsifies — the counterexample is
// a fail-fast taken on a recovered marked root whose missing outputs
// are available upstream (only a Substitutable/Indeterminate answer
// exists). The same trace also exhibits a wrongful terminal failure;
// B3 is the property this family pins.
// Constants: the reduced exhaustive failover scope
// (closureEvidenceFailoverEx); the as-built baseline is the same module
// without --step at these constants (bounded run recorded in the
// invariant map's Phase 0d calibration table).
//
// Run (serially):
//   quint verify --backend=tlc --main=closureCalibF3IndetFailFast \
//     --step=calibStep --invariant=unknownNeverDemotes \
//     docs/spec/models/calibration/closure-f3-indet-failfast.qnt
// Baseline:
//   quint verify --backend=tlc --main=closureCalibF3IndetFailFast \
//     --invariant=unknownNeverDemotes \
//     docs/spec/models/calibration/closure-f3-indet-failfast.qnt
module closureCalibF3IndetFailFast {
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

  /// PRE-FIX fail-fast arm of the dispatch probe: fires on any wanted
  /// output that is merely missing — no confirmed-miss answer required
  /// (indeterminate / rate-limited / substitutable answers are mapped
  /// to "missing", the pre-9bbe9ee29 verdict mapping). Latches keep the
  /// as-built oracle.
  action calibProbeFailFastIndet(d: str): bool = all {
    leaderActive,
    drvOf(d).mSt == MReady,
    liveInterest(d),
    mustSub(d),
    effWanted(d).exists(i => not(store.get((d, i)).present)),
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

  action calibStepDrv = {
    nondet d = DRVS.oneOf()
    any {
      probeInline(d),
      probeWalk(d),
      calibProbeFailFastIndet(d),
      probeFailFastTried(d),
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
