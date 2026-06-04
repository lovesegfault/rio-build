// -*- mode: Bluespec; -*-
//
// Bughunt-wave A3 calibration override, the merged_bug_020 shape —
// THE REMOVED AS-BUILT ENCODING, preserved per the re-derivation's
// polarity rule: the unobtainable one-shot read the DRV-LEVEL history
// (count_materialization_rows_in_history spanned jobs), so job 2's
// first evidence-backed unobtainable inherited job 1's consumed
// one-shot and routed from-source instead of re-arming. The
// production fix is rio_retry_kernel::materialization_counters over
// the per-job window realized by migration 085's job-creation reset
// row.
//
// PRE-FIX behavior modeled: calibUnscopedOneShot is
// consumeUnobtainable with oneShotSpent read from the drv-level
// ledger; the budget-soundness latch compares it against the job's
// OWN count and records the divergence.
//
// PREDICTED: budgetSoundness falsifies — a routing decision consumed
// another job's window.
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalibUnscopedCount \
//     --step=calibStep --invariant=budgetSoundness \
//     docs/spec/models/calibration/mat-unscoped-count.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalibUnscopedCount \
//     --invariant=budgetSoundness \
//     docs/spec/models/calibration/mat-unscoped-count.qnt
module matCalibUnscopedCount {
  import materializationJob(
    BUILDS = Set("b1", "b2"),
    DRVS = Set("d1"),
    OUTPUTS = Set("o1", "o2"),
    REPLICAS = Set("r1", "r2"),
    BUILDERS = Set("w1"),
    MAT_BUDGET = 2,
    MAX_EXECS = 6,
    MAX_GEN = 0,
    MAX_UPSTREAM_FLIPS = 4,
    MAX_LEGACY = 0,
    ENABLE_FAILOVER = false,
    ENABLE_GC = false,
    ENABLE_CRASH = false,
    ENABLE_STALE_TENURE = false,
    ENABLE_RESOLVE_FAULTS = false,
    ENABLE_WORKER_ABORTED = false
  ).* from "../materializationJob"

  /// PRE-FIX consumption: the drv-level one-shot read (everything
  /// else verbatim from consumeUnobtainable).
  action calibUnscopedOneShot(d: str, missing: Set[str]): bool = all {
    missing != Set(),
    missing.subseteq(OUTPUTS),
    match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) => a.kind == KMaterialization and not(channelStale.get(a.instance))
    },
    match jobs.get(d) { | NoJob => false | SomeJob(j) => j.state == JClaimed },
    // the report's missing paths really are missing-and-unavailable
    // (the executor verified them; B3's evidence-backed direction).
    missing.forall(o => not(present.get((d, o))) and not(upstreamAvail.get((d, o)))),
    matUnobtainableLedger.get(d) < MAT_BUDGET + 2,
    val w = liveWanted(d)
    val missingLive = missing.intersect(w)
    val execId = match attempts.get(d) { | NoAttempt => 0 | OpenAttempt(a) => a.execId }
    val marked = topdownPruned.get(d)
    // THE BUG: the drv-level one-shot (the as-built history read,
    // count_materialization_rows_in_history) — job 2's routing
    // consults job 1's spent shot.
    val oneShotSpent = matUnobtainableLedger.get(d) > 0
    val ownShotSpent = match jobs.get(d) { | NoJob => false | SomeJob(j) => j.matUnobtainableCount > 0 }
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
      // delta 1: the mark is consumed by success (arm0covered),
      // from-source (arm12; arm3fs is unmarked already) AND the
      // fail-fast itself (dispatch.rs:1126).
      topdownPruned' = if (arm0covered or arm12 or arm3fail)
        topdownPruned.put(d, false)
      else topdownPruned,
      matUnobtainableLedger' = matUnobtainableLedger.put(d, matUnobtainableLedger.get(d) + 1),
      // A3/020: the one-shot routing consulted the job's OWN window —
      // the mat-unscoped-count override records false on divergence.
      budgetSoundAll' = and { budgetSoundAll, oneShotSpent == ownShotSpent },
      abortChargeFree' = abortChargeFree,
      failFastCount' = if (arm3fail) failFastCount.put(d, failFastCount.get(d) + 1) else failFastCount,
      // delta 1: the four-conjunct justification, at decision time.
      // marked/Broken/missing-live are structural at the arm3fail
      // site; the latch records the fourth conjunct, with the spent
      // one-shot only counting when its charges were evidence-backed.
      ffJustifiedAll' = and {
        ffJustifiedAll,
        arm3fail implies or {
          not(reprobeAllObtainable),
          oneShotSpent and unobChargeBacked.get(d),
        },
      },
      // the 6-row from-source justification, at decision time.
      fsJustifiedAll' = and {
        fsJustifiedAll,
        arm12 implies (durableRel.get(d) == RVouched or durableRel.get(d) == RPending),
        arm3fs implies (and {
          durableRel.get(d) == RChildlessLeaf or durableRel.get(d) == RHoled,
          not(marked),
        }),
      },
      // C2: the from-source routing consumed evidence-backed missing
      // paths (verified absent-and-unavailable at this decision).
      fsReportSoundAll' = and {
        fsReportSoundAll,
        (arm12 or arm3fs) implies
          missingLive.forall(o => not(present.get((d, o))) and not(upstreamAvail.get((d, o)))),
      },
      // CE-17 latch: the moot-covered arm is a success resolution.
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
    requeueReadyJustifiedAll' = requeueReadyJustifiedAll,
    carried' = carried,
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
      // the override REPLACES consumeUnobtainable
      calibUnscopedOneShot(d, missing),
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
