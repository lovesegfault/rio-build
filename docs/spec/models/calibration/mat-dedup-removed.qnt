// -*- mode: Bluespec; -*-
//
// C-prime calibration override, the partial-unique-index dedup row
// (atMostOneUnresolvedJobPerDrv — the §9.1 slot widened per the
// C-prime handoff: the dupJob ghost is the second slot).
//
// PRE-FIX behavior modeled: job creation without the
// one-unresolved-job-per-derivation arbitration — a second creation
// for a drv with an unresolved job lands a second row (the ghost
// latches; the slot carries the newest).
//
// PREDICTED: atMostOneUnresolvedJobPerDrv falsifies.
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalibDedupRemoved \
//     --step=calibStep --invariant=atMostOneUnresolvedJobPerDrv \
//     docs/spec/models/calibration/mat-dedup-removed.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalibDedupRemoved \
//     --invariant=atMostOneUnresolvedJobPerDrv \
//     docs/spec/models/calibration/mat-dedup-removed.qnt
module matCalibDedupRemoved {
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

  /// PRE-FIX creation: no dedup guard; a second unresolved row latches
  /// the widened-slot ghost.
  action calibCreateJobNoDedup(d: str, origin: JobOrigin, tenant: str): bool = all {
    BUILDS.contains(tenant),
    buildLive.get(tenant),
    or {
      wanted.get((tenant, d)) != Set(),
      legacyInterest.get((tenant, d)),
    },
    not(memLiveWanted(d).forall(o => present.get((d, o)))),
    nodeStatus.get(d) != NFailed,
    nodeStatus.get(d) != NDepFailed,
    nodeStatus.get(d) != NRunning,
    if (origin == OStaleReset)
      nodeStatus.get(d) == NCompleted
    else
      nodeStatus.get(d) == NQueued or nodeStatus.get(d) == NReady,
    (origin == OMerge or origin == OPruned or origin == OReprobe) implies
      wanted.get((tenant, d)) != Set(),
    val standalone = origin == OProbe or origin == OStaleReset
    val backfills = BUILDS.filter(b => and {
      standalone,
      buildLive.get(b),
      legacyInterest.get((b, d)),
    })
    val wantedNew = backfills.fold(wanted, (acc, b) => acc.put((b, d), OUTPUTS))
    all {
      jobs' = jobs.put(d, SomeJob({ ...freshJob, origin: origin, tenant: tenant })),
      view' = view.put(d, VPending(false)),
      // THE BUG: the dropped unique-index decision, routed through
      // the LIVE dedup seat (R10/P5: the seat computes the
      // widened-slot truth; this override perturbs the decision only
      // — and drops createJob's no-unresolved-job guard).
      dedupSlotApply(d, false),
      topdownPruned' = if (origin == OPruned) topdownPruned.put(d, true) else topdownPruned,
      nodeStatus' = if (origin == OStaleReset or origin == OReprobe)
        nodeStatus.put(d, NQueued)
      else nodeStatus,
      wanted' = wantedNew,
      lastWritten' = backfills.fold(lastWritten, (acc, b) => acc.put((b, d), OUTPUTS)),
      legacyInterest' = backfills.fold(legacyInterest, (acc, b) => acc.put((b, d), false)),
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
    carried' = carried,
    budgetSoundAll' = budgetSoundAll,
    abortChargeFree' = abortChargeFree,
    requeueReadyJustifiedAll' = requeueReadyJustifiedAll,
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
        recordWantedRelation(b, d, outs),
        legacyBuildArrives(b, d),
        calibCreateJobNoDedup(d, origin, b),
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
