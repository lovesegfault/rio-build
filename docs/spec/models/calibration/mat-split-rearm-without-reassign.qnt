// -*- mode: Bluespec; -*-
//
// Bughunt-wave A3 calibration override, the merged_bug_015/307 wedge
// shape — INTRODUCED CALIBRATION-ONLY (the main model's
// consumeUnobtainable is already atomic; the pre-fix split is not
// representable there): rearm_materialization_job cleared the claim
// holder WITHOUT requeueing the node, so the view said
// pending-claimable while the node sat Assigned/Running — every later
// claim answered NotYetReady forever (the second-replica wedge). The
// production fix is the atomic release_claim (view rearm +
// reassign_derivations in ONE fn; rearm_materialization_job deleted
// as a callable).
//
// PRE-FIX behavior modeled: calibSplitRearm is the InfraFailure
// re-arm with the node-requeue half DROPPED — nodeStatus stays
// NRunning while job/view go pending (rides BESIDE reportInfra, as
// the pre-fix code's bare-rearm sites coexisted with the fused
// paths).
//
// PREDICTED: pendingUnclaimedImpliesClaimableNode falsifies — a
// pending unparked unclaimed job strands its node Running.
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalibSplitRearm \
//     --step=calibStep --invariant=pendingUnclaimedImpliesClaimableNode \
//     docs/spec/models/calibration/mat-split-rearm-without-reassign.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalibSplitRearm \
//     --invariant=pendingUnclaimedImpliesClaimableNode \
//     docs/spec/models/calibration/mat-split-rearm-without-reassign.qnt
module matCalibSplitRearm {
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
    ENABLE_GC = false,
    ENABLE_CRASH = false,
    ENABLE_STALE_TENURE = false,
    ENABLE_RESOLVE_FAULTS = false
  ).* from "../materializationJob"

  /// PRE-FIX release: the InfraFailure re-arm splits — claim dropped,
  /// view re-armed pending, the budget charged, but the node is NEVER
  /// requeued (the reassign_derivations half lost — the
  /// materialize.rs:935 else-arm pre-fix shape, one of the five sites
  /// release_claim fused). Below the budget, so the job re-arms
  /// rather than parks — the wedge state.
  action calibSplitRearm(d: str): bool = all {
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
      // fire only on the RE-ARM side (below budget): the park side
      // is not the wedge.
      not(parks),
      jobs' = jobs.put(d, match jobs.get(d) {
        | NoJob => NoJob
        | SomeJob(j) => SomeJob({ ...j, state: JPending, parked: parks, matInfraCount: newCount })
      }),
      view' = view.put(d, VPending(parks)),
      budgetSoundAll' = and { budgetSoundAll, parks == (newCount >= MAT_BUDGET) },
    },
    attempts' = attempts.put(d, NoAttempt),
    // THE BUG: nodeStatus untouched — stays NRunning from the claim.
    nodeStatus' = nodeStatus,
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
    requeueReadyJustifiedAll' = requeueReadyJustifiedAll,
    carried' = carried,
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
    nondet rel = Set(RVouched, RPending, RBroken).oneOf()
    any {
      recordWantedRelation(b, d, outs),
      legacyBuildArrives(b, d),
      createJob(d, origin, b),
      dedupRefeed(d),
      claimJob(d, replica),
      executeStep(d, o),
      consumeSuccess(d),
      consumeUnobtainable(d, missing),
      // the split-release rides BESIDE the atomic consumption (the
      // wedge is one alternative arm, as in the pre-fix code where
      // rearm_materialization_job coexisted with the fused paths)
      calibSplitRearm(d),
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
      upstreamChange(d, o),
      durableRelationChange(d, rel),
    }
  }
}
