// -*- mode: Bluespec; -*-
//
// Bughunt-wave A4 calibration override — THE REMOVED AS-BUILT
// ENCODING behind fork-11 commit 1's union upsert: the wanted-relation
// upsert OVERWROTE the row (`wanted = EXCLUDED.wanted`), so a build's
// re-merge with a narrower set silently un-wanted outputs its own
// earlier roots still needed. (PP-5 ii recorded the rewrite as
// "intended narrowing"; the bughunt re-adjudicated it: the durable row
// must only widen — live-build scoping is the narrowing mechanism, not
// row rewrites.)
//
// PRE-FIX behavior modeled: calibRecordWantedOverwrite writes `outs`
// verbatim (no union with the existing row) while the contribution
// ghost keeps accumulating — the durable row drops below the union of
// recorded contributions.
//
// PREDICTED: durableUnionWidensOrEqualsLive falsifies — a re-merge
// narrowed the durable row below an earlier contribution.
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalibUnionDropped \
//     --step=calibStep --invariant=durableUnionWidensOrEqualsLive \
//     docs/spec/models/calibration/mat-a4-union-dropped.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalibUnionDropped \
//     --invariant=durableUnionWidensOrEqualsLive \
//     docs/spec/models/calibration/mat-a4-union-dropped.qnt
module matCalibUnionDropped {
  import materializationJob(
    BUILDS = Set("b1", "b2"),
    DRVS = Set("d1"),
    OUTPUTS = Set("o1", "o2"),
    REPLICAS = Set("r1", "r2"),
    BUILDERS = Set("w1"),
    MAT_BUDGET = 2,
    MAX_EXECS = 4,
    MAX_GEN = 0,
    MAX_UPSTREAM_FLIPS = 2,
    MAX_LEGACY = 0,
    ENABLE_FAILOVER = false,
    ENABLE_GC = false,
    ENABLE_CRASH = false,
    ENABLE_STALE_TENURE = false,
    ENABLE_RESOLVE_FAULTS = false,
    ENABLE_WORKER_ABORTED = false
  ).* from "../materializationJob"

  /// PRE-FIX upsert: overwrite, not union. The contribution ghost
  /// still accumulates (it records what WAS contributed, not what
  /// survived the write).
  action calibRecordWantedOverwrite(b: str, d: str, outs: Set[str]): bool = all {
    buildLive.get(b),
    outs != Set(),
    outs.subseteq(OUTPUTS),
    wanted.get((b, d)) != outs,
    wanted' = wanted.put((b, d), outs),
    lastWritten' = lastWritten.put((b, d), outs),
    everContrib' = everContrib.put((b, d), everContrib.get((b, d)).union(outs)),
    legacyInterest' = legacyInterest.put((b, d), false),
    legacyBudget' = legacyBudget,
    deferUntil' = deferUntil,
    transientChargeFreeAll' = transientChargeFreeAll,
    closureCompleteAll' = closureCompleteAll,
    noVacuousCoverageAll' = noVacuousCoverageAll,
    leafReevalReleased' = leafReevalReleased,
    jobCoreUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
    tenureUnchanged, ghostsUnchanged,
  }

  action calibStep = {
    nondet d = DRVS.oneOf()
    nondet b = BUILDS.oneOf()
    nondet outs = OUTPUTS.powerset().oneOf()
    any {
      // the override REPLACES recordWantedRelation
      calibRecordWantedOverwrite(b, d, outs),
      buildTerminal(b),
    }
  }
}
