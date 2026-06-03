// -*- mode: Bluespec; -*-
//
// Bughunt-wave A3 calibration override, the bug_067 shape — THE
// REMOVED AS-BUILT ENCODING, preserved here per the re-derivation's
// polarity rule: the establishment sweep charged the materialization
// budget WITHOUT the park decision (materialize.rs:1243 pre-fix — the
// job stayed pending claimable however large the count), so an
// establishment-only crash-loop (a never-reporting store replica, a
// dead upstream) re-listed forever: charged at every expiry, never
// parked, never in the MD-D1 stalled population. The production fix
// is charge_materialization_infra fusing charge + kernel park verdict
// (party-blind parking — the owner-signed Q5 reversal of
// counter-signed residual (a), 2026-06-03).
//
// PRE-FIX behavior modeled: calibEstablishNeverParks is
// establishCrashedAttempt WITHOUT the park predicate — parked is
// carried through unchanged while the per-job count grows past
// MAT_BUDGET; the budget-soundness latch records the unapplied park.
//
// PREDICTED: budgetSoundness falsifies — a charge edge reached the
// budget without parking.
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalibEstablishNeverParks \
//     --step=calibStep --invariant=budgetSoundness \
//     docs/spec/models/calibration/mat-establish-never-parks.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalibEstablishNeverParks \
//     --invariant=budgetSoundness \
//     docs/spec/models/calibration/mat-establish-never-parks.qnt
module matCalibEstablishNeverParks {
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
    ENABLE_CRASH = true,
    ENABLE_STALE_TENURE = false,
    ENABLE_RESOLVE_FAULTS = false
  ).* from "../materializationJob"

  /// PRE-FIX establishment: the materialization charge with NO park
  /// decision (parked carried through unchanged; everything else
  /// verbatim from establishCrashedAttempt's mat arm).
  action calibEstablishNeverParks(d: str): bool = all {
    ENABLE_CRASH,
    attempts.get(d) != NoAttempt,
    match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) => a.kind == KMaterialization and matInfraLedger.get(d) < MAT_BUDGET + 2
    },
    val newCount = match jobs.get(d) { | NoJob => 0 | SomeJob(j) => j.matInfraCount + 1 }
    all {
      attempts' = attempts.put(d, NoAttempt),
      // THE BUG: no park predicate — parked unchanged at any count.
      jobs' = jobs.put(d, match jobs.get(d) {
        | NoJob => NoJob
        | SomeJob(j) => SomeJob({ ...j, state: JPending, matInfraCount: newCount })
      }),
      view' = view.put(d, match jobs.get(d) {
        | NoJob => VNone
        | SomeJob(j) => VPending(j.parked)
      }),
      nodeStatus' = nodeStatus.put(d, NQueued),
      matInfraLedger' = matInfraLedger.put(d, matInfraLedger.get(d) + 1),
      establishCount' = establishCount.put(d, establishCount.get(d) + 1),
      buildCrashLedger' = buildCrashLedger,
      buildCrashEstablished' = buildCrashEstablished,
      // The contract latch records the unapplied park: the applied
      // parked (unchanged) diverges from the per-job-window verdict
      // once newCount reaches the budget on an unparked job.
      budgetSoundAll' = and {
        budgetSoundAll,
        (match jobs.get(d) { | NoJob => false | SomeJob(j) => j.parked })
          == (newCount >= MAT_BUDGET),
      },
    },
    dupJob' = dupJob, topdownPruned' = topdownPruned, execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, envUnchanged,
    matUnobtainableLedger' = matUnobtainableLedger,
    unobChargeBacked' = unobChargeBacked,
    tenureUnchanged,
    failFastCount' = failFastCount,
    ffJustifiedAll' = ffJustifiedAll, fsJustifiedAll' = fsJustifiedAll,
    fsReportSoundAll' = fsReportSoundAll, covAtResolveAll' = covAtResolveAll,
    covLiveOnlyAll' = covLiveOnlyAll,
    creationResolvableAll' = creationResolvableAll,
    chargedAfterCancel' = chargedAfterCancel,
    claimOrigin' = claimOrigin,
    requeueReadyJustifiedAll' = requeueReadyJustifiedAll,
    carried' = carried,
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
      // the override REPLACES establishCrashedAttempt
      calibEstablishNeverParks(d),
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
