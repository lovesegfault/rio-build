// -*- mode: Bluespec; -*-
//
// Bughunt-wave A3 calibration override, the merged_bug_257 (and 055)
// shape: the stale-reset pass CLEARED the realized floating-CA output
// paths with a bare `output_paths.clear()` and the creation carried
// nothing — the carrier was lost on the !deps_ok arm and on
// failed/fenced job creation, and a later completion stamped [""].
// The production fix is #[must_use] RealizedPathCarrier
// (take_realized_paths fuses destruction with capture) +
// complete_materialization_for_live_interest owning the stamp.
//
// PRE-FIX behavior modeled: calibStaleResetCreateDropsCarrier is
// createJob with origin = OStaleReset and the carrier capture
// DROPPED.
//
// PREDICTED: carrierConservation falsifies (an unresolved stale-reset
// job without its carrier); completionRecordsRealizedPaths falsifies
// on the traces that reach success-resolution (the [""] stamp image)
// — the wired check runs both.
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalibChainResetDropsCarrier \
//     --step=calibStep --invariant=carrierConservation \
//     docs/spec/models/calibration/mat-chain-reset-drops-carrier.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalibChainResetDropsCarrier \
//     --invariant=carrierConservation \
//     docs/spec/models/calibration/mat-chain-reset-drops-carrier.qnt
module matCalibChainResetDropsCarrier {
  import materializationJob(
    BUILDS = Set("b1", "b2"),
    DRVS = Set("d1", "d2"),
    OUTPUTS = Set("o1", "o2"),
    REPLICAS = Set("r1", "r2"),
    BUILDERS = Set("w1"),
    MAT_BUDGET = 2,
    MAX_EXECS = 4,
    MAX_GEN = 0,
    MAX_UPSTREAM_FLIPS = 4,
    MAX_LEGACY = 0,
    ENABLE_FAILOVER = false,
    ENABLE_GC = true,
    ENABLE_CRASH = false,
    ENABLE_STALE_TENURE = false,
    ENABLE_RESOLVE_FAULTS = false
  ).* from "../materializationJob"

  /// PRE-FIX stale-reset creation: origin pinned OStaleReset, carrier
  /// capture dropped (everything else verbatim from createJob).
  action calibStaleResetCreateDropsCarrier(d: str, tenant: str): bool = all {
    BUILDS.contains(tenant),
    buildLive.get(tenant),
    or {
      wanted.get((tenant, d)) != Set(),
      legacyInterest.get((tenant, d)),
    },
    not(memLiveWanted(d).forall(o => present.get((d, o)))),
    not(hasUnresolvedJob(d)),
    nodeStatus.get(d) != NFailed,
    nodeStatus.get(d) != NDepFailed,
    nodeStatus.get(d) != NRunning,
    if (OStaleReset == OStaleReset)
      nodeStatus.get(d) == NCompleted
    else
      nodeStatus.get(d) == NQueued or nodeStatus.get(d) == NReady,
    // in-tx origins ride the merge tx whose batch wrote the tenant's
    // rows; the standalone sites accept legacy tenants and backfill.
    (false) implies
      wanted.get((tenant, d)) != Set(),
    val standalone = true
    val backfills = BUILDS.filter(b => and {
      standalone,
      buildLive.get(b),
      legacyInterest.get((b, d)),
    })
    val wantedNew = backfills.fold(wanted, (acc, b) => acc.put((b, d), OUTPUTS))
    all {
      jobs' = jobs.put(d, SomeJob({ ...freshJob, origin: OStaleReset, tenant: tenant })),
      view' = view.put(d, VPending(false)),
      dupJob' = dupJob,
      topdownPruned' = if (false) topdownPruned.put(d, true) else topdownPruned,
      nodeStatus' = if (true)
        nodeStatus.put(d, NQueued)
      else nodeStatus,
      wanted' = wantedNew,
      lastWritten' = backfills.fold(lastWritten, (acc, b) => acc.put((b, d), OUTPUTS)),
      legacyInterest' = backfills.fold(legacyInterest, (acc, b) => acc.put((b, d), false)),
      // delta 2b invariant latch: resolvable at creation commit —
      // some live build holds wanted rows AFTER the backfill.
      creationResolvableAll' = and {
        creationResolvableAll,
        BUILDS.exists(b => buildLive.get(b) and wantedNew.get((b, d)) != Set()),
      },
    },
    legacyBudget' = legacyBudget,
    attempts' = attempts, execsMinted' = execsMinted,
    storeUnchanged, ledgerUnchanged, envUnchanged, tenureUnchanged,
    failFastCount' = failFastCount,
    ffJustifiedAll' = ffJustifiedAll, fsJustifiedAll' = fsJustifiedAll,
    fsReportSoundAll' = fsReportSoundAll, covAtResolveAll' = covAtResolveAll,
    covLiveOnlyAll' = covLiveOnlyAll,
    chargedAfterCancel' = chargedAfterCancel,
    claimOrigin' = claimOrigin,
    requeueReadyJustifiedAll' = requeueReadyJustifiedAll,
    // THE BUG: the bare clear — the stale-reset pass destroyed the
    // realized paths and the creation carries NOTHING.
    carried' = carried,
    budgetSoundAll' = budgetSoundAll,
  }

  action calibStep = {
    nondet d = DRVS.oneOf()
    nondet b = BUILDS.oneOf()
    nondet o = OUTPUTS.oneOf()
    nondet replica = REPLICAS.oneOf()
    nondet builder = BUILDERS.oneOf()
    nondet origin = Set(OProbe, OMerge, OPruned, OReprobe).oneOf()
    nondet outs = OUTPUTS.powerset().oneOf()
    nondet missing = OUTPUTS.powerset().oneOf()
    nondet rel = Set(RVouched, RPending, RBroken).oneOf()
    any {
      recordWantedRelation(b, d, outs),
      legacyBuildArrives(b, d),
      // non-stale origins keep the as-built creation; the stale-reset
      // origin goes through the carrier-dropping override.
      createJob(d, origin, b),
      calibStaleResetCreateDropsCarrier(d, b),
      dedupRefeed(d),
      claimJob(d, replica),
      executeStep(d, o),
      consumeSuccess(d),
      consumeUnobtainable(d, missing),
      reportInfra(d),
      cancelOnZeroInterest(d),
      obsoleteOnProduced(d),
      parkReevaluate(d),
      parkBackoffExpires(d),
      buildTerminal(b),
      builderPull(d, builder),
      dispatchFromSource(d),
      completeReadyFromStore(d),
      releasePins(d),
      storeGc(d, o),
      upstreamChange(d, o),
      durableRelationChange(d, rel),
    }
  }
}

/// Reduced scope for the COMPLETION leg
/// (completionRecordsRealizedPaths): the deep trace — complete, GC,
/// carrier-dropped stale-reset, re-execute, success-resolve — needs
/// ~11 steps; the two-drv scope's TLC run is gate-hostile (~9 min to
/// first violation), so the leg pins at single-drv/single-output
/// scope per the §1.3/F13 sub-regime ladder. The conservation leg
/// stays at the full scope above (first violation at the creation
/// step).
module matCalibChainResetDropsCarrierEx {
  import materializationJob(
    BUILDS = Set("b1", "b2"),
    DRVS = Set("d1"),
    OUTPUTS = Set("o1"),
    REPLICAS = Set("r1", "r2"),
    BUILDERS = Set("w1"),
    MAT_BUDGET = 2,
    MAX_EXECS = 4,
    MAX_GEN = 0,
    MAX_UPSTREAM_FLIPS = 2,
    MAX_LEGACY = 0,
    ENABLE_FAILOVER = false,
    ENABLE_GC = true,
    ENABLE_CRASH = false,
    ENABLE_STALE_TENURE = false,
    ENABLE_RESOLVE_FAULTS = false
  ).* from "../materializationJob"

  /// PRE-FIX stale-reset creation: origin pinned OStaleReset, carrier
  /// capture dropped (everything else verbatim from createJob).
  action calibStaleResetCreateDropsCarrier(d: str, tenant: str): bool = all {
    BUILDS.contains(tenant),
    buildLive.get(tenant),
    or {
      wanted.get((tenant, d)) != Set(),
      legacyInterest.get((tenant, d)),
    },
    not(memLiveWanted(d).forall(o => present.get((d, o)))),
    not(hasUnresolvedJob(d)),
    nodeStatus.get(d) != NFailed,
    nodeStatus.get(d) != NDepFailed,
    nodeStatus.get(d) != NRunning,
    if (OStaleReset == OStaleReset)
      nodeStatus.get(d) == NCompleted
    else
      nodeStatus.get(d) == NQueued or nodeStatus.get(d) == NReady,
    // in-tx origins ride the merge tx whose batch wrote the tenant's
    // rows; the standalone sites accept legacy tenants and backfill.
    (false) implies
      wanted.get((tenant, d)) != Set(),
    val standalone = true
    val backfills = BUILDS.filter(b => and {
      standalone,
      buildLive.get(b),
      legacyInterest.get((b, d)),
    })
    val wantedNew = backfills.fold(wanted, (acc, b) => acc.put((b, d), OUTPUTS))
    all {
      jobs' = jobs.put(d, SomeJob({ ...freshJob, origin: OStaleReset, tenant: tenant })),
      view' = view.put(d, VPending(false)),
      dupJob' = dupJob,
      topdownPruned' = if (false) topdownPruned.put(d, true) else topdownPruned,
      nodeStatus' = if (true)
        nodeStatus.put(d, NQueued)
      else nodeStatus,
      wanted' = wantedNew,
      lastWritten' = backfills.fold(lastWritten, (acc, b) => acc.put((b, d), OUTPUTS)),
      legacyInterest' = backfills.fold(legacyInterest, (acc, b) => acc.put((b, d), false)),
      // delta 2b invariant latch: resolvable at creation commit —
      // some live build holds wanted rows AFTER the backfill.
      creationResolvableAll' = and {
        creationResolvableAll,
        BUILDS.exists(b => buildLive.get(b) and wantedNew.get((b, d)) != Set()),
      },
    },
    legacyBudget' = legacyBudget,
    attempts' = attempts, execsMinted' = execsMinted,
    storeUnchanged, ledgerUnchanged, envUnchanged, tenureUnchanged,
    failFastCount' = failFastCount,
    ffJustifiedAll' = ffJustifiedAll, fsJustifiedAll' = fsJustifiedAll,
    fsReportSoundAll' = fsReportSoundAll, covAtResolveAll' = covAtResolveAll,
    covLiveOnlyAll' = covLiveOnlyAll,
    chargedAfterCancel' = chargedAfterCancel,
    claimOrigin' = claimOrigin,
    requeueReadyJustifiedAll' = requeueReadyJustifiedAll,
    // THE BUG: the bare clear — the stale-reset pass destroyed the
    // realized paths and the creation carries NOTHING.
    carried' = carried,
    budgetSoundAll' = budgetSoundAll,
  }

  action calibStep = {
    nondet d = DRVS.oneOf()
    nondet b = BUILDS.oneOf()
    nondet o = OUTPUTS.oneOf()
    nondet replica = REPLICAS.oneOf()
    nondet builder = BUILDERS.oneOf()
    nondet origin = Set(OProbe, OMerge, OPruned, OReprobe).oneOf()
    nondet outs = OUTPUTS.powerset().oneOf()
    nondet missing = OUTPUTS.powerset().oneOf()
    nondet rel = Set(RVouched, RPending, RBroken).oneOf()
    any {
      recordWantedRelation(b, d, outs),
      legacyBuildArrives(b, d),
      // non-stale origins keep the as-built creation; the stale-reset
      // origin goes through the carrier-dropping override.
      createJob(d, origin, b),
      calibStaleResetCreateDropsCarrier(d, b),
      dedupRefeed(d),
      claimJob(d, replica),
      executeStep(d, o),
      consumeSuccess(d),
      consumeUnobtainable(d, missing),
      reportInfra(d),
      cancelOnZeroInterest(d),
      obsoleteOnProduced(d),
      parkReevaluate(d),
      parkBackoffExpires(d),
      buildTerminal(b),
      builderPull(d, builder),
      dispatchFromSource(d),
      completeReadyFromStore(d),
      releasePins(d),
      storeGc(d, o),
      upstreamChange(d, o),
      durableRelationChange(d, rel),
    }
  }
}
