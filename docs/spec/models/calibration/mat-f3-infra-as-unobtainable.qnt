// -*- mode: Bluespec; -*-
//
// C-prime calibration override, family F3 soundness (CE-61 -> B3),
// direction (i). Transfer row: §9.3 "override 'InfraFailure treated as
// Unobtainable' must falsify noWrongfulFromSourceRouting" — re-pointed
// at this model to noWrongfulTerminalFailure: an infra blip charged to
// the unobtainable budget spends the re-probe one-shot WITHOUT
// evidence, so the next unobtainable consumption fail-fasts a marked
// node on a spent-but-unbacked one-shot (the wrongful terminal class;
// the unobChargeBacked ghost is the production oracle).
//
// PRE-FIX behavior modeled: the InfraFailure consumption charges
// materialization_unobtainable (not materialization_infra) and runs the
// four-arm routing as if the executor had verified every absent
// live-wanted path missing — without any verification.
//
// PREDICTED: noWrongfulTerminalFailure falsifies (a fail-fast whose
// spent one-shot was infra-corrupted). noWrongfulFromSourceRouting may
// additionally falsify through the arm-1/2 leg (recorded, secondary).
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalibF3InfraAsUnobtainable \
//     --step=calibStep --invariant=noWrongfulTerminalFailure \
//     docs/spec/models/calibration/mat-f3-infra-as-unobtainable.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalibF3InfraAsUnobtainable \
//     --invariant=noWrongfulTerminalFailure \
//     docs/spec/models/calibration/mat-f3-infra-as-unobtainable.qnt
module matCalibF3InfraAsUnobtainable {
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

  /// PRE-FIX InfraFailure consumption: charged as unobtainable, routed
  /// through the four arms with UNVERIFIED missing paths. The
  /// unobChargeBacked ghost records the corruption; the justification
  /// latches keep the production oracle.
  action calibInfraChargedAsUnobtainable(d: str): bool = all {
    match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) => a.kind == KMaterialization and not(channelStale.get(a.instance))
    },
    match jobs.get(d) { | NoJob => false | SomeJob(j) => j.state == JClaimed },
    matUnobtainableLedger.get(d) < MAT_BUDGET + 2,
    val w = liveWanted(d)
    val missingLive = w.filter(o => not(present.get((d, o))))
    val execId = match attempts.get(d) { | NoAttempt => 0 | OpenAttempt(a) => a.execId }
    val marked = topdownPruned.get(d)
    val oneShotSpent = matUnobtainableLedger.get(d) > 0
    val arm0 = missingLive == Set()
    val arm0covered = arm0 and liveWantedCovered(d)
    val arm12 = not(arm0) and (durableRel.get(d) == RVouched or durableRel.get(d) == RPending)
    val reprobeAllObtainable = w.forall(o => present.get((d, o)) or upstreamAvail.get((d, o)))
    val arm3rearm = not(arm0) and not(arm12) and reprobeAllObtainable and not(oneShotSpent)
    val arm3 = not(arm0) and not(arm12) and not(arm3rearm)
    val arm3fail = arm3 and marked
    val arm3fs = arm3 and not(marked)
    all {
      jobs' = jobs.put(d, match jobs.get(d) {
        | NoJob => NoJob
        | SomeJob(j) => SomeJob({ ...j,
            state: if (arm0covered) JResolvedSuccess
              else if (arm0) JPending
              else if (arm12 or arm3fs) JResolvedFromSource
              else if (arm3rearm) JPending
              else JResolvedUnobtainable,
            resolutionExec: if (arm0covered or arm12 or arm3fs or arm3fail) execId
              else j.resolutionExec,
            matUnobtainableCount: j.matUnobtainableCount + 1 })
      }),
      view' = if (arm0 and not(arm0covered)) view.put(d, VPending(false))
        else if (arm3rearm) view.put(d, VPending(false))
        else view.put(d, VNone),
      nodeStatus' = if (arm0covered)
        nodeStatus.put(d, NCompleted)
      else if (arm3fail)
        nodeStatus.put(d, NFailed)
      else if (arm12 or arm3fs)
        nodeStatus.put(d, NReady)
      else
        nodeStatus.put(d, NQueued),
      attempts' = attempts.put(d, NoAttempt),
      topdownPruned' = if (arm0covered or arm12 or arm3fail)
        topdownPruned.put(d, false)
      else topdownPruned,
      matUnobtainableLedger' = matUnobtainableLedger.put(d, matUnobtainableLedger.get(d) + 1),
      // the corrupted charge: NOT evidence-backed.
      unobChargeBacked' = unobChargeBacked.put(d, false),
      failFastCount' = if (arm3fail) failFastCount.put(d, failFastCount.get(d) + 1) else failFastCount,
      ffJustifiedAll' = and {
        ffJustifiedAll,
        arm3fail implies or {
          not(reprobeAllObtainable),
          oneShotSpent and unobChargeBacked.get(d),
        },
      },
      fsJustifiedAll' = and {
        fsJustifiedAll,
        arm12 implies (durableRel.get(d) == RVouched or durableRel.get(d) == RPending),
        arm3fs implies (and {
          durableRel.get(d) == RChildlessLeaf or durableRel.get(d) == RHoled,
          not(marked),
        }),
      },
      fsReportSoundAll' = and {
        fsReportSoundAll,
        (arm12 or arm3fs) implies
          missingLive.forall(o => not(present.get((d, o))) and not(upstreamAvail.get((d, o)))),
      },
      covAtResolveAll' = and { covAtResolveAll, arm0covered implies liveWantedCovered(d) },
      covLiveOnlyAll' = covLiveOnlyAll,
    },
    dupJob' = dupJob, execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, envUnchanged,
    matInfraLedger' = matInfraLedger, buildCrashLedger' = buildCrashLedger,
    establishCount' = establishCount, buildCrashEstablished' = buildCrashEstablished,
    tenureUnchanged,
    creationResolvableAll' = creationResolvableAll,
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
        createJob(d, origin, b),
        dedupRefeed(d),
        claimJob(d, replica),
        executeStep(d, o),
        consumeSuccess(d),
        consumeUnobtainable(d, missing, refsMissing),
        calibInfraChargedAsUnobtainable(d),
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
