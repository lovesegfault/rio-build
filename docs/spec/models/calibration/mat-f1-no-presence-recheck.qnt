// -*- mode: Bluespec; -*-
//
// C-prime calibration override, family F1 permissiveness (CE-1 -> C4).
// Transfer row: §9.3 "kept guard, data source re-pointed (§6);
// calibration override re-run against the relation-backed predicate".
//
// PRE-FIX behavior modeled: job creation with NO local-presence
// re-check — restricted to exactly the DELTA the re-check excludes
// (creation REQUIRES the live-wanted set covered), so the explored
// space is the pre-fix-only state space and any verdict is
// attributable to it.
//
// PREDICTED — **NO falsification** (the by-construction half of the
// F1-permissiveness transfer): in the job architecture a
// covered-creation cannot produce the CE-1 harm (a wrongful
// from-source build) — the job's claim/Success path completes the node
// for live interest without any build attempt; builderPull still
// requires a resolved job. The §9.1 conjunction must HOLD over this
// override's space; the noCoveredCreationJob witness must VIOLATE
// (the pre-fix state is reachable, so the no-falsify verdict is about
// a real space). A falsification here would be an architecture gap —
// stop-and-report.
//
// Run (expected [ok] over the conjunction; witness must violate):
//   quint verify --backend=tlc --main=matCalibF1NoPresenceRecheck \
//     --step=calibStep --invariant=<the §9.1 conjunction> \
//     docs/spec/models/calibration/mat-f1-no-presence-recheck.qnt
//   quint verify --backend=tlc --main=matCalibF1NoPresenceRecheck \
//     --step=calibStep --invariant=noCoveredCreationJob \
//     docs/spec/models/calibration/mat-f1-no-presence-recheck.qnt
module matCalibF1NoPresenceRecheck {
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

  /// The pre-fix-only delta: creation on a COVERED node (the state the
  /// as-built presence re-check excludes).
  action calibCreateJobOnCovered(d: str, origin: JobOrigin, tenant: str): bool = all {
    BUILDS.contains(tenant),
    buildLive.get(tenant),
    or {
      wanted.get((tenant, d)) != Set(),
      legacyInterest.get((tenant, d)),
    },
    memLiveWanted(d) != Set(),
    memLiveWanted(d).forall(o => present.get((d, o))),
    not(hasUnresolvedJob(d)),
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
      dupJob' = dupJob,
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
    requeueReadyJustifiedAll' = requeueReadyJustifiedAll,
  }

  /// Witness: a covered-creation job exists (every job under this
  /// calibStep is covered-created — violation = the pre-fix space is
  /// reachable, the no-falsify verdict is non-vacuous).
  val noCoveredCreationJob = DRVS.forall(d => not(hasUnresolvedJob(d)))

  /// Environment enrichment required by the reduced scope: the CE-1
  /// scenario starts from outputs ALREADY present locally (an earlier
  /// build's residue). At the one-drv Ex scope no other lifecycle can
  /// seed that presence before a job exists, so the override module
  /// carries the env action explicitly (monotone, unpinned).
  action calibPresenceSeed(d: str, o: str): bool = all {
    OUTPUTS.contains(o),
    not(present.get((d, o))),
    present' = present.put((d, o), true),
    pins' = pins, ingested' = ingested,
    jobCoreUnchanged, relUnchanged, ledgerUnchanged, envUnchanged,
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
      calibCreateJobOnCovered(d, origin, b),
      calibPresenceSeed(d, o),
      dedupRefeed(d),
      claimJob(d, replica),
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
