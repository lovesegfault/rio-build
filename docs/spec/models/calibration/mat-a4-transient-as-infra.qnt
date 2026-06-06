// -*- mode: Bluespec; -*-
//
// Bughunt-wave A4 calibration override, the merged_bug_178/195 shape —
// THE REMOVED AS-BUILT ENCODING: there was no transient outcome class.
// A raced placeholder slot or an upstream 429 came back through the
// blanket `Err(e) => infra_failure(...)` arm and CHARGED the
// materialization budget like genuine infrastructure trouble — three
// 429 waves parked a healthy job (park → PD-20 → from-source walk for
// content the upstream still had).
//
// PRE-FIX behavior modeled: calibTransientAsInfra is reportTransient
// routed through reportInfra's body — the ledger row lands, the
// per-job count grows, the budget parks — while the charge-free latch
// records the divergence (a transient close moved a ledger / parked).
//
// PREDICTED: transientOutcomesNeverCharge falsifies — a transient
// close charged the budget (and at the budget, parked the job).
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalibTransientAsInfra \
//     --step=calibStep --invariant=transientOutcomesNeverCharge \
//     docs/spec/models/calibration/mat-a4-transient-as-infra.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalibTransientAsInfra \
//     --invariant=transientOutcomesNeverCharge \
//     docs/spec/models/calibration/mat-a4-transient-as-infra.qnt
module matCalibTransientAsInfra {
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

  /// PRE-FIX transient consumption: the 429/raced report charges the
  /// materialization budget (reportInfra's body verbatim) — no
  /// deferral, a real ledger row, the park at the budget. The
  /// charge-free latch records the moved ledger.
  action calibTransientAsInfra(d: str): bool = all {
    match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) => a.kind == KMaterialization and not(channelStale.get(a.instance))
    },
    match jobs.get(d) {
      | NoJob => false
      | SomeJob(j) => j.state == JClaimed and j.matInfraCount < MAT_BUDGET + 2
    },
    val newCount = match jobs.get(d) { | NoJob => 0 | SomeJob(j) => j.matInfraCount + 1 }
    val parks = newCount >= MAT_BUDGET
    all {
      jobs' = jobs.put(d, match jobs.get(d) {
        | NoJob => NoJob
        | SomeJob(j) => SomeJob({ ...j, state: JPending, parked: parks, matInfraCount: newCount })
      }),
      view' = view.put(d, VPending(parks)),
      nodeStatus' = nodeStatus.put(d, NQueued),
      attempts' = attempts.put(d, NoAttempt),
      matInfraLedger' = matInfraLedger.put(d, matInfraLedger.get(d) + 1),
      // THE BUG, recorded: the transient close moved a ledger (and may
      // have parked) — never charge-free.
      transientChargeFreeAll' = and { transientChargeFreeAll, false },
    },
    deferUntil' = deferUntil,
    closureCompleteAll' = closureCompleteAll,
    noVacuousCoverageAll' = noVacuousCoverageAll,
    everContrib' = everContrib,
    leafReevalReleased' = leafReevalReleased,
    dupJob' = dupJob, topdownPruned' = topdownPruned, execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, envUnchanged, tenureUnchanged,
    matUnobtainableLedger' = matUnobtainableLedger,
    buildCrashLedger' = buildCrashLedger,
    establishCount' = establishCount,
    buildCrashEstablished' = buildCrashEstablished,
    unobChargeBacked' = unobChargeBacked,
    ghostsUnchanged,
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
        createJob(d, origin, b),
        claimJob(d, replica),
        executeStep(d, o),
        consumeSuccess(d),
        // the override REPLACES reportTransient
        calibTransientAsInfra(d),
        cancelOnZeroInterest(d),
        parkReevaluate(d),
        parkBackoffExpires(d),
        buildTerminal(b),
        upstreamChange(d, o),
      },
      walkUnchanged,
    }
  }
}
