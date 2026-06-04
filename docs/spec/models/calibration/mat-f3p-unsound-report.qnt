// -*- mode: Bluespec; -*-
//
// C-prime calibration override, family F3 permissiveness (CE-60/CE-3
// -> C2). Transfer row: §9.3 "by-construction + successor: routing
// requires a completed execution; override ... falsifies C2-successor".
//
// PRE-FIX behavior modeled: the Unobtainable consumption accepts a
// report whose missing paths were NOT verified absent upstream (the
// probe-demotion-on-any-missing class) — from-source routing fires on
// paths the upstream still offers.
//
// PREDICTED: noWrongfulFromSourceRouting falsifies — a from-source
// routing consumed a report whose live-missing path was
// upstream-available at decision time.
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalibF3pUnsoundReport \
//     --step=calibStep --invariant=noWrongfulFromSourceRouting \
//     docs/spec/models/calibration/mat-f3p-unsound-report.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalibF3pUnsoundReport \
//     --invariant=noWrongfulFromSourceRouting \
//     docs/spec/models/calibration/mat-f3p-unsound-report.qnt
module matCalibF3pUnsoundReport {
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
    ENABLE_WORKER_ABORTED = false
  ).* from "../materializationJob"

  /// PRE-FIX Unobtainable consumption: the report's missing paths are
  /// trusted unverified (only local absence required). All routing
  /// arms and latches keep the production oracle.
  action calibConsumeUnobtainableUnverified(d: str, missing: Set[str]): bool = all {
    missing != Set(),
    missing.subseteq(OUTPUTS),
    match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) => a.kind == KMaterialization and not(channelStale.get(a.instance))
    },
    match jobs.get(d) { | NoJob => false | SomeJob(j) => j.state == JClaimed },
    // pre-fix: only local absence — upstream availability NOT checked.
    missing.forall(o => not(present.get((d, o)))),
    matUnobtainableLedger.get(d) < MAT_BUDGET + 2,
    val w = liveWanted(d)
    val missingLive = missing.intersect(w)
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
    unobChargeBacked' = unobChargeBacked,
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
    any {
      recordWantedRelation(b, d, outs),
      legacyBuildArrives(b, d),
      createJob(d, origin, b),
      dedupRefeed(d),
      claimJob(d, replica),
      executeStep(d, o),
      consumeSuccess(d),
      calibConsumeUnobtainableUnverified(d, missing),
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
    }
  }
}
