// -*- mode: Bluespec; -*-
//
// C-prime calibration override, Phase-A bug B1 (7c9b9f949 — the
// admission gap; delta 6). The kinded wrapper's materialization
// Pending arm ran the as-built A11 must_substitute refusal, which also
// refused MATERIALIZATION claims of topdown-pruned roots — parking
// exactly the jobs that most need claiming, forever.
//
// PRE-FIX behavior modeled: claimJob carries the must_substitute
// conjunct (marked nodes refuse the claim).
//
// PREDICTED — **witness flip, not an invariant falsification** (the
// strand is a liveness shape): under calibStep the noMarkedClaim
// witness must NOT be violated (a marked node's job is never claimable
// — the B1 dead-end made visible as unreachability), while under the
// as-built step the same witness IS violated (the wired
// quint-materialization-witness-marked-claim). The §9.1 safety
// conjunction stays green under the override (a refusal strands, it
// does not corrupt) — recorded, not a falsification target.
//
// Run (expected [ok] — the witness must NOT violate under calibStep):
//   quint verify --backend=tlc --main=matCalibB1ClaimRefusesMarked \
//     --step=calibStep --invariant=noMarkedClaim \
//     docs/spec/models/calibration/mat-b1-claim-refuses-marked.qnt
// As-built direction (must VIOLATE — the wired witness check):
//   quint verify --backend=tlc --main=matCalibB1ClaimRefusesMarked \
//     --invariant=noMarkedClaim \
//     docs/spec/models/calibration/mat-b1-claim-refuses-marked.qnt
module matCalibB1ClaimRefusesMarked {
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

  /// PRE-FIX claim: the A11 refusal applies to the materialization
  /// kind too (marked nodes are not claimable).
  action calibClaimRefusesMarked(d: str, replica: str): bool = all {
    REPLICAS.contains(replica),
    execsMinted < MAX_EXECS,
    not(channelStale.get(replica)),
    not(topdownPruned.get(d)),
    view.get(d) == VPending(false),
    match jobs.get(d) {
      | NoJob => false
      | SomeJob(j) => j.state == JPending and not(j.parked)
    },
    attempts.get(d) == NoAttempt,
    nodeStatus.get(d) == NQueued or nodeStatus.get(d) == NReady,
    val newExec = execsMinted + 1
    all {
      attempts' = attempts.put(d, OpenAttempt({ kind: KMaterialization, instance: replica, execId: newExec })),
      jobs' = jobs.put(d, match jobs.get(d) {
        | NoJob => NoJob
        | SomeJob(j) => SomeJob({ ...j, state: JClaimed })
      }),
      view' = view.put(d, VClaimed(replica)),
      nodeStatus' = nodeStatus.put(d, NRunning),
      execsMinted' = newExec,
      claimAfterFailover' = or { claimAfterFailover, failedOver },
    },
    dupJob' = dupJob, topdownPruned' = topdownPruned,
    relUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
    leaseGen' = leaseGen, staleWriteDiscarded' = staleWriteDiscarded,
    staleJobWriteApplied' = staleJobWriteApplied,
    channelStale' = channelStale, failedOver' = failedOver,
    failoverDroppedJobs' = failoverDroppedJobs,
    ghostsUnchanged,
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
      calibClaimRefusesMarked(d, replica),
      executeStep(d, o),
      consumeSuccess(d),
      consumeUnobtainable(d, missing),
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
