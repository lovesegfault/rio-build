// -*- mode: Bluespec; -*-
//
// Bughunt-wave A4 calibration override, the merged_bug_194 store-leg
// shape — THE REMOVED AS-BUILT ENCODING: a first walk iteration that
// resolved no verifiable wanted path (the carrier-top-up edge: live
// wanted empty or all-unverifiable at the store) returned
// Success-with-NOTHING. The consumption's coverage check was vacuously
// satisfied (∀ over ∅) and the node completed with zero paths behind
// it.
//
// PRE-FIX behavior modeled: calibConsumeVacuousSuccess is
// consumeSuccess WITHOUT the report-evidence guard — the success
// consumption fires with empty live wanted and no store evidence; the
// vacuity latch records the zero-evidence completion.
//
// PREDICTED: noVacuousCoverage falsifies — a covered Success
// consumption carried neither width nor evidence.
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalibVacuousSuccess \
//     --step=calibStep --invariant=noVacuousCoverage \
//     docs/spec/models/calibration/mat-a4-vacuous-success.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalibVacuousSuccess \
//     --invariant=noVacuousCoverage \
//     docs/spec/models/calibration/mat-a4-vacuous-success.qnt
module matCalibVacuousSuccess {
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
    MAX_LEGACY = 0,
    ENABLE_FAILOVER = false,
    ENABLE_GC = false,
    ENABLE_CRASH = false,
    ENABLE_STALE_TENURE = false,
    ENABLE_RESOLVE_FAULTS = false,
    TENANTS = Set("tA", "tB"),
    ENABLE_WALK_FOLD = false,
    ENABLE_WORKER_ABORTED = false
  ).* from "../materializationJob"

  /// PRE-FIX success consumption: NO report-evidence guard — the
  /// vacuously-covered (empty live wanted, nothing ingested, nothing
  /// present) completion goes through.
  action calibConsumeVacuousSuccess(d: str): bool = all {
    match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) => a.kind == KMaterialization and not(channelStale.get(a.instance))
    },
    match jobs.get(d) { | NoJob => false | SomeJob(j) => j.state == JClaimed },
    val covered = liveWantedCovered(d)
    val execId = match attempts.get(d) { | NoAttempt => 0 | OpenAttempt(a) => a.execId }
    all {
      jobs' = jobs.put(d, match jobs.get(d) {
        | NoJob => NoJob
        | SomeJob(j) => SomeJob({ ...j,
            state: if (covered) JResolvedSuccess else JPending,
            resolutionExec: if (covered) execId else j.resolutionExec })
      }),
      view' = if (covered) view.put(d, VNone) else view.put(d, VPending(false)),
      nodeStatus' = if (covered) nodeStatus.put(d, NCompleted) else nodeStatus.put(d, NQueued),
      attempts' = attempts.put(d, NoAttempt),
      topdownPruned' = if (covered) topdownPruned.put(d, false) else topdownPruned,
      covAtResolveAll' = and { covAtResolveAll, covered implies covered },
      covLiveOnlyAll' = covLiveOnlyAll,
      // The latch records the vacuous completion (the as-built guard's
      // disjunction, evaluated honestly at this edge).
      noVacuousCoverageAll' = and {
        noVacuousCoverageAll,
        covered implies or {
          liveWanted(d) != Set(),
          OUTPUTS.exists(o => ingested.get((d, o)) or present.get((d, o))),
        },
      },
    },
    deferUntil' = deferUntil,
    transientChargeFreeAll' = transientChargeFreeAll,
    closureCompleteAll' = closureCompleteAll,
    everContrib' = everContrib,
    leafReevalReleased' = leafReevalReleased,
    dupJob' = dupJob, execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
    tenureUnchanged,
    failFastCount' = failFastCount,
    ffJustifiedAll' = ffJustifiedAll, fsJustifiedAll' = fsJustifiedAll,
    fsReportSoundAll' = fsReportSoundAll,
    creationResolvableAll' = creationResolvableAll,
    chargedAfterCancel' = chargedAfterCancel,
    claimOrigin' = claimOrigin,
    requeueReadyJustifiedAll' = requeueReadyJustifiedAll,
    carried' = carried,
    budgetSoundAll' = budgetSoundAll,
    abortChargeFree' = abortChargeFree,
  }

  action calibStep = {
    nondet d = DRVS.oneOf()
    nondet b = BUILDS.oneOf()
    nondet o = OUTPUTS.oneOf()
    nondet replica = REPLICAS.oneOf()
    nondet outs = OUTPUTS.powerset().oneOf()
    nondet origin = Set(OProbe, OMerge, OPruned, OReprobe, OStaleReset).oneOf()
    all {
      any {
        recordWantedRelation(b, d, outs),
        legacyBuildArrives(b, d),
        createJob(d, origin, b),
        claimJob(d, replica),
        executeStep(d, o),
        // the override REPLACES consumeSuccess
        calibConsumeVacuousSuccess(d),
        cancelOnZeroInterest(d),
        buildTerminal(b),
        upstreamChange(d, o),
      },
      walkUnchanged,
    }
  }
}
