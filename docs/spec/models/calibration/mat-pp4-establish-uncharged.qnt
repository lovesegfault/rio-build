// -*- mode: Bluespec; -*-
//
// C-prime calibration override, the PP-4 crash-loop family, direction
// (ii). Transfer row: §9.3 "override 'crash rows count toward no
// budget' must falsify ..." — re-pointed at this model to
// materializationCrashChargedOnce (the unbounded-re-claim strand the
// design predicted is a liveness shape; its safety-checkable core is
// the charge-once discipline: every establishment must charge the
// materialization budget, else the crash loop never converges on the
// park).
//
// PRE-FIX behavior modeled: the establishment closes the crashed
// materialization attempt WITHOUT writing the materialization_infra
// charge (the establishment-observed ghost still counts the event).
//
// PREDICTED: materializationCrashChargedOnce falsifies — the
// establishment count outruns the infra ledger.
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalibPp4EstablishUncharged \
//     --step=calibStep --invariant=materializationCrashChargedOnce \
//     docs/spec/models/calibration/mat-pp4-establish-uncharged.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalibPp4EstablishUncharged \
//     --invariant=materializationCrashChargedOnce \
//     docs/spec/models/calibration/mat-pp4-establish-uncharged.qnt
module matCalibPp4EstablishUncharged {
  import materializationJob(
    BUILDS = Set("b1", "b2"),
    DRVS = Set("d1"),
    OUTPUTS = Set("o1", "o2"),
    REPLICAS = Set("r1", "r2"),
    BUILDERS = Set("w1"),
    MAT_BUDGET = 2,
    MAX_EXECS = 6,
    MAX_GEN = 0,
    MAX_UPSTREAM_FLIPS = 2,
    MAX_LEGACY = 0,
    ENABLE_FAILOVER = false,
    ENABLE_GC = false,
    ENABLE_CRASH = true,
    ENABLE_STALE_TENURE = false,
    ENABLE_RESOLVE_FAULTS = false
  ).* from "../materializationJob"

  /// PRE-FIX establishment: charge-free for the materialization kind.
  action calibEstablishUncharged(d: str): bool = all {
    ENABLE_CRASH,
    attempts.get(d) != NoAttempt,
    match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) =>
        if (a.kind == KMaterialization)
          establishCount.get(d) < MAT_BUDGET + 2
        else
          buildCrashLedger.get(d) < MAT_BUDGET
    },
    val isMat = match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) => a.kind == KMaterialization
    }
    all {
      attempts' = attempts.put(d, NoAttempt),
      jobs' = if (isMat) jobs.put(d, match jobs.get(d) {
        | NoJob => NoJob
        | SomeJob(j) => SomeJob({ ...j, state: JPending })
      }) else jobs,
      view' = if (isMat) view.put(d, match jobs.get(d) {
        | NoJob => VNone
        | SomeJob(j) => VPending(j.parked)
      }) else view,
      nodeStatus' = nodeStatus.put(d, NQueued),
      // pre-fix: NO infra charge; the event ghost still counts.
      matInfraLedger' = matInfraLedger,
      establishCount' = if (isMat) establishCount.put(d, establishCount.get(d) + 1) else establishCount,
      buildCrashLedger' = if (isMat) buildCrashLedger else buildCrashLedger.put(d, buildCrashLedger.get(d) + 1),
      buildCrashEstablished' = if (isMat) buildCrashEstablished else buildCrashEstablished.put(d, true),
    },
    dupJob' = dupJob, topdownPruned' = topdownPruned, execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, envUnchanged,
    matUnobtainableLedger' = matUnobtainableLedger,
    unobChargeBacked' = unobChargeBacked,
    tenureUnchanged, ghostsUnchanged,
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
      reportInfra(d),
      calibEstablishUncharged(d),
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
    }
  }
}
