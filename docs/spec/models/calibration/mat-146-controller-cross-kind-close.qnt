// -*- mode: Bluespec; -*-
//
// Bughunt-wave A2 calibration override, the merged_bug_146 shape (the
// 307 wedge mirror's controller half): the controller-facing
// ReportAttemptOutcome intake closed attempts KIND-BLIND — a
// controller verdict (preemption, node death) aimed at a derivation
// could close the open MATERIALIZATION attempt through the build-side
// path, leaving the job view Claimed with no attempt to drive it (the
// split-lifecycle wedge). The AttemptRef parse-once witness makes the
// cross-kind close uncompilable in production; this override is the
// model image of the pre-fix intake.
//
// PRE-FIX behavior modeled: calibControllerCloseKindblind closes the
// open materialization attempt (assignments-side) WITHOUT the
// consumption transaction — the job view keeps VClaimed(holder).
//
// PREDICTED: claimedImpliesOpenAttempt falsifies — a Claimed view with
// NoAttempt (the wedge state production could only leave by operator
// surgery).
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalib146ControllerCrossKindClose \
//     --step=calibStep --invariant=claimedImpliesOpenAttempt \
//     docs/spec/models/calibration/mat-146-controller-cross-kind-close.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalib146ControllerCrossKindClose \
//     --invariant=claimedImpliesOpenAttempt \
//     docs/spec/models/calibration/mat-146-controller-cross-kind-close.qnt
module matCalib146ControllerCrossKindClose {
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

  /// PRE-FIX intake: the kind-blind controller-verdict close — the
  /// assignments row dies, the job view does not hear about it.
  action calibControllerCloseKindblind(d: str): bool = all {
    match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) => a.kind == KMaterialization
    },
    attempts' = attempts.put(d, NoAttempt),
    // THE BUG: view untouched — VClaimed survives the close.
    view' = view,
    jobs' = jobs, dupJob' = dupJob,
    nodeStatus' = nodeStatus, topdownPruned' = topdownPruned,
    execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
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
        calibControllerCloseKindblind(d),
      },
      walkUnchanged,
    }
  }
}
