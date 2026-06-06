// -*- mode: Bluespec; -*-
//
// Bughunt-wave A2 calibration override, the merged_bug_318 shape: the
// release edge after a materialization attempt FORCED the node Ready —
// a Queued-origin (dep-racing) claim came back Ready with unbuilt
// deps, and the spawn machinery dispatched a build whose inputs did
// not exist. The production fix is validate_transition_for_release +
// reset_after_attempt(kind, deps_completed): a release never upgrades
// the claim-origin status. The main model's release edges all restore
// NQueued (the conservative arm); claimOrigin records the origin and
// the requeueReadyJustifiedAll latch pins that any Ready-setting
// release had a Ready origin.
//
// PRE-FIX behavior modeled: calibRequeueForcesReady is reportInfra's
// re-arm with the forced NReady — the latch records the upgrade of a
// Queued-origin claim.
//
// PREDICTED: readyImpliesDepsProducedOnRequeue falsifies — a re-armed
// release sets Ready on a claim whose origin status was NQueued.
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalib318RequeueForcesReady \
//     --step=calibStep --invariant=readyImpliesDepsProducedOnRequeue \
//     docs/spec/models/calibration/mat-318-requeue-forces-ready.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalib318RequeueForcesReady \
//     --invariant=readyImpliesDepsProducedOnRequeue \
//     docs/spec/models/calibration/mat-318-requeue-forces-ready.qnt
module matCalib318RequeueForcesReady {
  import materializationJob(
    BUILDS = Set("b1", "b2"),
    DRVS = Set("d1"),
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
    ENABLE_RESOLVE_FAULTS = false,
    TENANTS = Set("tA", "tB"),
    ENABLE_WALK_FOLD = false,
    ENABLE_WORKER_ABORTED = false
  ).* from "../materializationJob"

  /// PRE-FIX release: the InfraFailure re-arm that FORCES the node
  /// Ready regardless of the claim's origin status (reportInfra with
  /// the upgraded release; everything else verbatim).
  action calibRequeueForcesReady(d: str): bool = all {
    match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) => a.kind == KMaterialization and not(channelStale.get(a.instance))
    },
    match jobs.get(d) {
      | NoJob => false
      | SomeJob(j) => j.state == JClaimed and j.matInfraCount < MAT_BUDGET + 2
    },
    val newCount = match jobs.get(d) { | NoJob => 0 | SomeJob(j) => j.matInfraCount + 1 }
    val parks = newCount >= MAT_BUDGET
    all {
      jobs' = jobs.put(d, match jobs.get(d) {
        | NoJob => NoJob
        | SomeJob(j) => SomeJob({ ...j, state: JPending, parked: parks, matInfraCount: newCount })
      }),
      view' = view.put(d, VPending(parks)),
    },
    attempts' = attempts.put(d, NoAttempt),
    // THE BUG: the forced-Ready decision, routed through the LIVE
    // release seat (R10/P5: the seat computes the latch from the
    // claimOrigin witness; this override perturbs the decision only).
    releaseStatusApply(d, true),
    matInfraLedger' = matInfraLedger.put(d, matInfraLedger.get(d) + 1),
    dupJob' = dupJob, topdownPruned' = topdownPruned, execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, envUnchanged,
    matUnobtainableLedger' = matUnobtainableLedger,
    buildCrashLedger' = buildCrashLedger,
    establishCount' = establishCount, buildCrashEstablished' = buildCrashEstablished,
    unobChargeBacked' = unobChargeBacked,
    tenureUnchanged,
    failFastCount' = failFastCount,
    ffJustifiedAll' = ffJustifiedAll, fsJustifiedAll' = fsJustifiedAll,
    fsReportSoundAll' = fsReportSoundAll, covAtResolveAll' = covAtResolveAll,
    covLiveOnlyAll' = covLiveOnlyAll,
    creationResolvableAll' = creationResolvableAll,
    chargedAfterCancel' = chargedAfterCancel,
    claimOrigin' = claimOrigin,
    carried' = carried,
    budgetSoundAll' = budgetSoundAll,
    abortChargeFree' = abortChargeFree,
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
        calibRequeueForcesReady(d),
      },
      walkUnchanged,
    }
  }
}
