// -*- mode: Bluespec; -*-
//
// C-prime calibration override, family F9 (CE-41 -> A5, the laundering
// class re-pointed). Transfer row: §9.3 "the laundering hazard
// (truncated set vouches) is unconstructible because routing reads
// pg.edges. Calibration: override 'routing reads in-memory children'
// falsifies routingRequiresDurableVouchOrFailFast".
//
// PRE-FIX behavior modeled: the Unobtainable routing trusts a divergent
// in-memory child view that reads as Vouched — every non-moot report
// routes from-source regardless of the DURABLE relation (the truncated
// in-memory set vouching for a Broken closure).
//
// PREDICTED: routingRequiresDurableVouchOrFailFast falsifies — a
// from-source disposition while the durable relation is Broken on a
// marked node (the latch keeps the durable oracle).
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalibF9DivergentEvidence \
//     --step=calibStep --invariant=routingRequiresDurableVouchOrFailFast \
//     docs/spec/models/calibration/mat-f9-divergent-evidence.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalibF9DivergentEvidence \
//     --invariant=routingRequiresDurableVouchOrFailFast \
//     docs/spec/models/calibration/mat-f9-divergent-evidence.qnt
module matCalibF9DivergentEvidence {
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
    ENABLE_RESOLVE_FAULTS = false
  ).* from "../materializationJob"

  /// PRE-FIX Unobtainable consumption: arm 1/2 fires on EVERY non-moot
  /// report (the divergent in-memory view always says Vouched); the
  /// justification latch keeps the durable oracle.
  action calibRoutingTrustsMemEvidence(d: str, missing: Set[str]): bool = all {
    missing != Set(),
    missing.subseteq(OUTPUTS),
    match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) => a.kind == KMaterialization and not(channelStale.get(a.instance))
    },
    match jobs.get(d) { | NoJob => false | SomeJob(j) => j.state == JClaimed },
    missing.forall(o => not(present.get((d, o))) and not(upstreamAvail.get((d, o)))),
    matUnobtainableLedger.get(d) < MAT_BUDGET + 2,
    val w = liveWanted(d)
    val missingLive = missing.intersect(w)
    val execId = match attempts.get(d) { | NoAttempt => 0 | OpenAttempt(a) => a.execId }
    val arm0 = missingLive == Set()
    val arm0covered = arm0 and liveWantedCovered(d)
    // pre-fix: the in-memory view vouches unconditionally.
    val arm12 = not(arm0)
    all {
      jobs' = jobs.put(d, match jobs.get(d) {
        | NoJob => NoJob
        | SomeJob(j) => SomeJob({ ...j,
            state: if (arm0covered) JResolvedSuccess
              else if (arm0) JPending
              else JResolvedFromSource,
            resolutionExec: if (arm0covered or arm12) execId else j.resolutionExec,
            matUnobtainableCount: j.matUnobtainableCount + 1 })
      }),
      view' = if (arm0 and not(arm0covered)) view.put(d, VPending(false))
        else view.put(d, VNone),
      nodeStatus' = if (arm0covered) nodeStatus.put(d, NCompleted)
        else if (arm12) nodeStatus.put(d, NReady)
        else nodeStatus.put(d, NQueued),
      attempts' = attempts.put(d, NoAttempt),
      topdownPruned' = if (arm0covered or arm12) topdownPruned.put(d, false) else topdownPruned,
      matUnobtainableLedger' = matUnobtainableLedger.put(d, matUnobtainableLedger.get(d) + 1),
      // production oracle: arm-1/2 dispositions are justified only by
      // the DURABLE relation.
      fsJustifiedAll' = and {
        fsJustifiedAll,
        arm12 implies (durableRel.get(d) == RVouched or durableRel.get(d) == RPending),
      },
      fsReportSoundAll' = and {
        fsReportSoundAll,
        arm12 implies
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
    failFastCount' = failFastCount, ffJustifiedAll' = ffJustifiedAll,
    creationResolvableAll' = creationResolvableAll,
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
      calibRoutingTrustsMemEvidence(d, missing),
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
