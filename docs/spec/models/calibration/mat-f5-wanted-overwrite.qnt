// -*- mode: Bluespec; -*-
//
// C-prime calibration override, family F5 (CE-16 -> B5, superseded
// with sign-off; PP-5 direction (i)). Transfer row: §9.3 "override
// 'another build's write replaces this build's row' must falsify
// interestUnionLiveOnly (cross-build isolation)" — re-pointed at this
// model to crossBuildWantedIsolation (the per-build write-history
// ghost is the direct cross-build-isolation oracle; the live-union
// latch reads decisions, not rows). The same-build narrowing direction
// (PP-5 ii) is the documented intended behavior — recorded by the
// noWantedRewrite witness in the parent model, not a violation.
//
// PRE-FIX behavior modeled: the wanted-relation write REPLACES the
// node's rows wholesale — writing build b's selection clears every
// other build's row for the derivation (the overwrite-vs-union class).
//
// PREDICTED: crossBuildWantedIsolation falsifies — a build's row
// diverges from its own last write.
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalibF5WantedOverwrite \
//     --step=calibStep --invariant=crossBuildWantedIsolation \
//     docs/spec/models/calibration/mat-f5-wanted-overwrite.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalibF5WantedOverwrite \
//     --invariant=crossBuildWantedIsolation \
//     docs/spec/models/calibration/mat-f5-wanted-overwrite.qnt
module matCalibF5WantedOverwrite {
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
    MAX_LEGACY = 1,
    ENABLE_FAILOVER = false,
    ENABLE_GC = false,
    ENABLE_CRASH = false,
    ENABLE_STALE_TENURE = false,
    ENABLE_RESOLVE_FAULTS = false,
    TENANTS = Set("tA", "tB"),
    ENABLE_WALK_FOLD = false,
    ENABLE_WORKER_ABORTED = false
  ).* from "../materializationJob"

  /// PRE-FIX relation write: replaces ALL builds' rows for d (the
  /// other builds' write-history ghosts are untouched — the oracle).
  action calibRecordWantedReplacesAll(b: str, d: str, outs: Set[str]): bool = all {
    buildLive.get(b),
    outs != Set(),
    outs.subseteq(OUTPUTS),
    wanted.get((b, d)) != outs,
    wanted' = BUILDS.fold(wanted.put((b, d), outs), (acc, other) =>
      if (other == b) acc else acc.put((other, d), Set())),
    lastWritten' = lastWritten.put((b, d), outs),
    legacyInterest' = legacyInterest.put((b, d), false),
    legacyBudget' = legacyBudget,
    jobCoreUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
    tenureUnchanged, ghostsUnchanged,
    a4Unchanged,
  }

  action calibStep = {
    nondet d = DRVS.oneOf()
    nondet b = BUILDS.oneOf()
    nondet o = OUTPUTS.oneOf()
    nondet replica = REPLICAS.oneOf()
    nondet builder = BUILDERS.oneOf()
    nondet origin = Set(OProbe, OMerge, OPruned, OReprobe, OStaleReset).oneOf()
    nondet outs = OUTPUTS.powerset().oneOf()
    nondet missing = OUTPUTS.powerset().oneOf()
    nondet rel = Set(RVouched, RPending, RChildlessLeaf, RHoled).oneOf()
    nondet refsMissing = Set(true, false).oneOf()
    all {
      any {
        calibRecordWantedReplacesAll(b, d, outs),
        legacyBuildArrives(b, d),
        createJob(d, origin, b),
        dedupRefeed(d),
        claimJob(d, replica),
        executeStep(d, o),
        consumeSuccess(d),
        consumeUnobtainable(d, missing, refsMissing),
        reportInfra(d),
        establishCrashedAttempt(d),
        cancelOnZeroInterest(d),
        obsoleteOnProduced(d),
        parkReevaluate(d),
        parkBackoffExpires(d),
        buildTerminal(b),
        builderPull(d, builder),
        dispatchFromSource(d),
        completeReadyFromStore(d),
        failover,
        redialChannel(replica),
        staleTenureWriteDiscarded(d),
        releasePins(d),
        storeGc(d, o),
        upstreamChange(d, o),
        durableRelationChange(d, rel),
      },
      walkUnchanged,
    }
  }
}
