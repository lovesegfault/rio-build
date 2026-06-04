// -*- mode: Bluespec; -*-
//
// Bughunt-wave A2 calibration override, the bug_266 shape: the
// recovery job-view rebuild joined claimed_by KIND-BLIND — any open
// assignment on the derivation, including a BUILD attempt's, could
// become the holder of an unresolved job's view. The production fix is
// the kind-scoped join (load through db/open_attempts.rs with the
// EXISTS(attempt_kind='materialization') conjunct); projView in the
// main model is deliberately kind-blind in the same way the join was —
// the main model never reaches the coexistence (createJob refuses
// NRunning nodes), exactly as the as-built guard idealized it. This
// override opens the production window (job re-creation racing a
// from-source build) AND applies the kind-blind projection at
// failover.
//
// PRE-FIX behavior modeled: calibRaceCreate re-creates a job while a
// from-source KBuild attempt is running (the interest re-arrival the
// idealized guard hides); calibFailoverKindblind rebuilds the view
// projecting ANY open attempt — pending jobs included — as the holder.
//
// PREDICTED: claimedByOnlyMatHolders falsifies — a Claimed view held
// by a BUILDER identity (w1), which no kind-scoped rebuild can
// produce.
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalib266KindblindRecoveryHolder \
//     --step=calibStep --invariant=claimedByOnlyMatHolders \
//     docs/spec/models/calibration/mat-266-kindblind-recovery-holder.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalib266KindblindRecoveryHolder \
//     --invariant=claimedByOnlyMatHolders \
//     docs/spec/models/calibration/mat-266-kindblind-recovery-holder.qnt
module matCalib266KindblindRecoveryHolder {
  import materializationJob(
    BUILDS = Set("b1", "b2"),
    DRVS = Set("d1"),
    OUTPUTS = Set("o1", "o2"),
    REPLICAS = Set("r1", "r2"),
    BUILDERS = Set("w1"),
    MAT_BUDGET = 2,
    MAX_EXECS = 4,
    MAX_GEN = 1,
    MAX_UPSTREAM_FLIPS = 4,
    MAX_LEGACY = 0,
    ENABLE_FAILOVER = true,
    ENABLE_GC = true,
    ENABLE_CRASH = false,
    ENABLE_STALE_TENURE = false,
    ENABLE_RESOLVE_FAULTS = false,
    ENABLE_WORKER_ABORTED = false
  ).* from "../materializationJob"

  /// The kind-blind claimed_by projection: ANY open attempt's instance
  /// becomes the holder — pending jobs included (the join filled
  /// claimed_by before the view state was considered).
  pure def projViewKindblind(j: JobOpt, a: AttemptOpt): ViewState = match j {
    | NoJob => VNone
    | SomeJob(job) =>
      if (job.state == JClaimed or job.state == JPending)
        match a {
          | OpenAttempt(at) => VClaimed(at.instance)
          | NoAttempt =>
            if (job.state == JClaimed) VPending(job.parked)
            else VPending(job.parked)
        }
      else VNone
  }

  /// The production window the main createJob guard idealizes away:
  /// new interest re-creates the job while the prior resolution's
  /// from-source build is still running (KBuild attempt open).
  action calibRaceCreate(d: str, tenant: str): bool = all {
    BUILDS.contains(tenant),
    buildLive.get(tenant),
    wanted.get((tenant, d)) != Set(),
    not(hasUnresolvedJob(d)),
    match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) => a.kind == KBuild
    },
    jobs' = jobs.put(d, SomeJob({ ...freshJob, origin: OProbe, tenant: tenant })),
    view' = view.put(d, VPending(false)),
    dupJob' = dupJob,
    nodeStatus' = nodeStatus, topdownPruned' = topdownPruned,
    attempts' = attempts, execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
    tenureUnchanged, ghostsUnchanged,
    a4Unchanged,
  }

  /// PRE-FIX recovery: the failover view rebuild with the kind-blind
  /// claimed_by join.
  action calibFailoverKindblind = all {
    ENABLE_FAILOVER,
    leaseGen < MAX_GEN,
    leaseGen' = leaseGen + 1,
    view' = DRVS.mapBy(d => projViewKindblind(jobs.get(d), attempts.get(d))),
    channelStale' = REPLICAS.mapBy(r => true),
    failedOver' = true,
    failoverDroppedJobs' = failoverDroppedJobs,
    jobs' = jobs, dupJob' = dupJob, attempts' = attempts,
    nodeStatus' = nodeStatus, topdownPruned' = topdownPruned,
    execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
    staleWriteDiscarded' = staleWriteDiscarded,
    staleJobWriteApplied' = staleJobWriteApplied,
    claimAfterFailover' = claimAfterFailover,
    ghostsUnchanged,
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
      redialChannel(replica),
      staleTenureWriteDiscarded(d),
      releasePins(d),
      storeGc(d, o),
      upstreamChange(d, o),
      durableRelationChange(d, rel),
      calibRaceCreate(d, b),
      calibFailoverKindblind,
    }
  }
}
